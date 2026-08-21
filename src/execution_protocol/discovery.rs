//! Typed, deterministic discovery contracts for Execution Protocol v1.
//!
//! The types in this module deliberately contain identities, hashes, bounded
//! metadata, and content-addressed references only. Repository file contents
//! and unconstrained provider payloads are not part of authoritative state.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Deserializer, Serialize};

use super::{
    ActionId, ContextManifestId, EvidenceId, NodeId, RepositoryProfileId, RepositoryRevisionId,
    ReservationId, SearchId, stable_sha256,
};

pub(crate) const DISCOVERY_SCHEMA_VERSION: u16 = 1;
pub(crate) const MAX_SEARCH_QUERY_BYTES: usize = 512;
pub(crate) const MAX_SEARCH_ROOTS: usize = 32;
pub(crate) const MAX_SEARCH_EXTENSIONS: usize = 32;
pub(crate) const MAX_SEARCH_MATCHES: usize = 256;
pub(crate) const MAX_DISCOVERY_SEARCH_TERMS: usize = 32;
pub(crate) const MAX_DISCOVERY_CRITERIA: usize = 64;
pub(crate) const MAX_DISCOVERY_CANDIDATES: usize = 64;
pub(crate) const MAX_COMPLETED_SEARCHES: usize = 128;
pub(crate) const MAX_FILE_EVIDENCE: usize = 128;
pub(crate) const MAX_RELATIONSHIP_EVIDENCE: usize = 128;
pub(crate) const MAX_UNRESOLVED_QUESTIONS: usize = 32;
pub(crate) const MAX_CONTEXT_EVIDENCE: usize = 128;
pub(crate) const MAX_CONTEXT_SECTIONS: usize = 224;
pub(crate) const MAX_ACTION_PATHS: usize = 64;
pub(crate) const MAX_GROUNDING_PATHS_PER_ACTION: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiscoveryContractError {
    EmptyField { field: &'static str },
    InvalidPath,
    InvalidExtension,
    InvalidHash { field: &'static str },
    LimitExceeded { field: &'static str, limit: usize },
    InvalidRange,
    InvalidAction { code: &'static str },
    InvalidContext { code: &'static str },
    RepositoryRevisionMismatch,
    Serialization,
}

impl DiscoveryContractError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::EmptyField { .. } => "discovery_empty_field",
            Self::InvalidPath => "discovery_invalid_repository_path",
            Self::InvalidExtension => "discovery_invalid_extension",
            Self::InvalidHash { .. } => "discovery_invalid_hash",
            Self::LimitExceeded { .. } => "discovery_limit_exceeded",
            Self::InvalidRange => "discovery_invalid_file_range",
            Self::InvalidAction { code } | Self::InvalidContext { code } => code,
            Self::RepositoryRevisionMismatch => "discovery_repository_revision_mismatch",
            Self::Serialization => "discovery_serialization_failed",
        }
    }
}

impl fmt::Display for DiscoveryContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField { field } => write!(formatter, "discovery field `{field}` is empty"),
            Self::InvalidPath => formatter.write_str("discovery repository path is invalid"),
            Self::InvalidExtension => formatter.write_str("discovery file extension is invalid"),
            Self::InvalidHash { field } => write!(formatter, "discovery hash `{field}` is invalid"),
            Self::LimitExceeded { field, limit } => {
                write!(formatter, "discovery field `{field}` exceeds limit {limit}")
            }
            Self::InvalidRange => formatter.write_str("discovery file range is invalid"),
            Self::InvalidAction { code } => {
                write!(formatter, "discovery action is invalid: {code}")
            }
            Self::InvalidContext { code } => {
                write!(formatter, "discovery context is invalid: {code}")
            }
            Self::RepositoryRevisionMismatch => {
                formatter.write_str("discovery evidence observes a different repository revision")
            }
            Self::Serialization => formatter.write_str("discovery identity serialization failed"),
        }
    }
}

impl std::error::Error for DiscoveryContractError {}

#[derive(Clone, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct DiscoveryPath(String);

impl DiscoveryPath {
    pub(crate) fn new(value: impl AsRef<str>) -> Result<Self, DiscoveryContractError> {
        normalize_repository_path(value.as_ref()).map(Self)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DiscoveryPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DiscoveryPath")
            .field(&self.0)
            .finish()
    }
}

impl<'de> Deserialize<'de> for DiscoveryPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

fn normalize_repository_path(value: &str) -> Result<String, DiscoveryContractError> {
    if value.is_empty()
        || value.starts_with('/')
        || value.starts_with('\\')
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(DiscoveryContractError::InvalidPath);
    }
    let portable = value.replace('\\', "/");
    if portable.len() >= 2 && portable.as_bytes()[1] == b':' {
        return Err(DiscoveryContractError::InvalidPath);
    }
    let mut components = Vec::new();
    for component in portable.split('/') {
        match component {
            "" | "." => {}
            ".." => return Err(DiscoveryContractError::InvalidPath),
            component => components.push(component),
        }
    }
    if components.is_empty() {
        return Err(DiscoveryContractError::InvalidPath);
    }
    Ok(components.join("/"))
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SearchMode {
    LiteralCaseSensitive,
    LiteralCaseInsensitive,
    RegexCaseSensitive,
    RegexCaseInsensitive,
    FileName,
}

impl SearchMode {
    const fn is_case_insensitive(self) -> bool {
        matches!(
            self,
            Self::LiteralCaseInsensitive | Self::RegexCaseInsensitive
        )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchScope {
    pub(crate) roots: BTreeSet<DiscoveryPath>,
    pub(crate) excluded_roots: BTreeSet<DiscoveryPath>,
}

impl SearchScope {
    pub(crate) fn repository() -> Self {
        Self {
            roots: BTreeSet::new(),
            excluded_roots: BTreeSet::new(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), DiscoveryContractError> {
        if self.roots.len() > MAX_SEARCH_ROOTS {
            return Err(DiscoveryContractError::LimitExceeded {
                field: "search_scope.roots",
                limit: MAX_SEARCH_ROOTS,
            });
        }
        if self.excluded_roots.len() > MAX_SEARCH_ROOTS {
            return Err(DiscoveryContractError::LimitExceeded {
                field: "search_scope.excluded_roots",
                limit: MAX_SEARCH_ROOTS,
            });
        }
        if self
            .roots
            .iter()
            .any(|root| self.excluded_roots.contains(root))
        {
            return Err(DiscoveryContractError::InvalidAction {
                code: "search_scope_root_excluded",
            });
        }
        Ok(())
    }
}

fn path_is_at_or_below(path: &DiscoveryPath, root: &DiscoveryPath) -> bool {
    path == root
        || path
            .as_str()
            .strip_prefix(root.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchRequest {
    pub(crate) schema_version: u16,
    pub(crate) search_id: SearchId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_profile_id: RepositoryProfileId,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
    pub(crate) normalized_query: String,
    pub(crate) scope: SearchScope,
    pub(crate) extensions: BTreeSet<String>,
    pub(crate) mode: SearchMode,
    pub(crate) context_evidence_ids: BTreeSet<EvidenceId>,
}

impl SearchRequest {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        repository_revision: RepositoryRevisionId,
        repository_profile_id: RepositoryProfileId,
        criterion_ids: BTreeSet<DiscoveryCriterionId>,
        query: &str,
        scope: SearchScope,
        extensions: impl IntoIterator<Item = String>,
        mode: SearchMode,
        context_evidence_ids: BTreeSet<EvidenceId>,
    ) -> Result<Self, DiscoveryContractError> {
        scope.validate()?;
        if criterion_ids.is_empty() {
            return Err(DiscoveryContractError::EmptyField {
                field: "search.criterion_ids",
            });
        }
        if criterion_ids.len() > MAX_DISCOVERY_CRITERIA {
            return Err(DiscoveryContractError::LimitExceeded {
                field: "search.criterion_ids",
                limit: MAX_DISCOVERY_CRITERIA,
            });
        }
        let normalized_query = normalize_search_query(query, mode)?;
        let extensions = extensions
            .into_iter()
            .map(|extension| normalize_extension(&extension))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if extensions.len() > MAX_SEARCH_EXTENSIONS {
            return Err(DiscoveryContractError::LimitExceeded {
                field: "search.extensions",
                limit: MAX_SEARCH_EXTENSIONS,
            });
        }
        if context_evidence_ids.len() > MAX_CONTEXT_EVIDENCE {
            return Err(DiscoveryContractError::LimitExceeded {
                field: "search.context_evidence_ids",
                limit: MAX_CONTEXT_EVIDENCE,
            });
        }
        let identity = SearchIdentity {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            repository_revision: &repository_revision,
            repository_profile_id: &repository_profile_id,
            criterion_ids: &criterion_ids,
            normalized_query: &normalized_query,
            scope: &scope,
            extensions: &extensions,
            mode,
            context_evidence_ids: &context_evidence_ids,
        };
        let canonical =
            serde_json::to_string(&identity).map_err(|_| DiscoveryContractError::Serialization)?;
        let search_id = SearchId::new(format!(
            "epv1:{}",
            stable_sha256(&["execution-protocol-v1:search", &canonical])
        ));
        Ok(Self {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            search_id,
            repository_revision,
            repository_profile_id,
            criterion_ids,
            normalized_query,
            scope,
            extensions,
            mode,
            context_evidence_ids,
        })
    }

    pub(crate) fn id(&self) -> &SearchId {
        &self.search_id
    }

    pub(crate) fn validate(&self) -> Result<(), DiscoveryContractError> {
        let rebuilt = Self::new(
            self.repository_revision.clone(),
            self.repository_profile_id.clone(),
            self.criterion_ids.clone(),
            &self.normalized_query,
            self.scope.clone(),
            self.extensions.iter().cloned(),
            self.mode,
            self.context_evidence_ids.clone(),
        )?;
        if rebuilt != *self {
            return Err(DiscoveryContractError::InvalidAction {
                code: "search_request_not_canonical",
            });
        }
        Ok(())
    }

    pub(crate) fn permits_path(&self, path: &DiscoveryPath) -> bool {
        let within_roots = self.scope.roots.is_empty()
            || self
                .scope
                .roots
                .iter()
                .any(|root| path_is_at_or_below(path, root));
        let excluded = self
            .scope
            .excluded_roots
            .iter()
            .any(|root| path_is_at_or_below(path, root));
        let extension_allowed = self.extensions.is_empty()
            || path
                .as_str()
                .rsplit_once('.')
                .is_some_and(|(_, extension)| self.extensions.contains(extension));
        within_roots && !excluded && extension_allowed
    }
}

#[derive(Serialize)]
struct SearchIdentity<'a> {
    schema_version: u16,
    repository_revision: &'a RepositoryRevisionId,
    repository_profile_id: &'a RepositoryProfileId,
    criterion_ids: &'a BTreeSet<DiscoveryCriterionId>,
    normalized_query: &'a str,
    scope: &'a SearchScope,
    extensions: &'a BTreeSet<String>,
    mode: SearchMode,
    context_evidence_ids: &'a BTreeSet<EvidenceId>,
}

pub(crate) fn normalize_search_query(
    query: &str,
    mode: SearchMode,
) -> Result<String, DiscoveryContractError> {
    if query
        .bytes()
        .any(|byte| byte == 0 || (byte.is_ascii_control() && !byte.is_ascii_whitespace()))
    {
        return Err(DiscoveryContractError::EmptyField {
            field: "search.query",
        });
    }
    let mut normalized = query.split_whitespace().collect::<Vec<_>>().join(" ");
    if mode.is_case_insensitive() {
        normalized = normalized.to_lowercase();
    }
    if normalized.is_empty() {
        return Err(DiscoveryContractError::EmptyField {
            field: "search.query",
        });
    }
    if normalized.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(DiscoveryContractError::LimitExceeded {
            field: "search.query",
            limit: MAX_SEARCH_QUERY_BYTES,
        });
    }
    Ok(normalized)
}

fn normalize_extension(extension: &str) -> Result<String, DiscoveryContractError> {
    let extension = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if extension.is_empty()
        || extension.len() > 32
        || !extension
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'_'))
    {
        return Err(DiscoveryContractError::InvalidExtension);
    }
    Ok(extension)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LineRange {
    pub(crate) start: u32,
    pub(crate) end_inclusive: u32,
}

impl LineRange {
    pub(crate) fn new(start: u32, end_inclusive: u32) -> Result<Self, DiscoveryContractError> {
        if start == 0 || end_inclusive < start {
            return Err(DiscoveryContractError::InvalidRange);
        }
        Ok(Self {
            start,
            end_inclusive,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CandidateReason {
    SearchMatch,
    ProfileSourceRoot,
    ProfileTestRoot,
    MetadataReference,
    Relationship,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchEvidence {
    pub(crate) schema_version: u16,
    pub(crate) evidence_id: EvidenceId,
    pub(crate) producer_node_id: NodeId,
    pub(crate) request: SearchRequest,
    pub(crate) matched_paths: BTreeSet<DiscoveryPath>,
    pub(crate) result_set_hash: String,
    pub(crate) truncated: bool,
}

impl SearchEvidence {
    pub(crate) fn new(
        producer_node_id: NodeId,
        request: SearchRequest,
        matched_paths: BTreeSet<DiscoveryPath>,
        truncated: bool,
    ) -> Result<Self, DiscoveryContractError> {
        request.validate()?;
        if matched_paths.len() > MAX_SEARCH_MATCHES
            || !matched_paths.iter().all(|path| request.permits_path(path))
        {
            return Err(DiscoveryContractError::InvalidAction {
                code: "search_result_outside_authorized_scope",
            });
        }
        let result_set_hash = search_result_set_hash(&matched_paths)?;
        let evidence_id = derived_evidence_id(
            "search",
            &(
                DISCOVERY_SCHEMA_VERSION,
                &producer_node_id,
                &request,
                &matched_paths,
                &result_set_hash,
                truncated,
            ),
        )?;
        Ok(Self {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            evidence_id,
            producer_node_id,
            request,
            matched_paths,
            result_set_hash,
            truncated,
        })
    }

    fn expected_evidence_id(&self) -> Result<EvidenceId, DiscoveryContractError> {
        derived_evidence_id(
            "search",
            &(
                self.schema_version,
                &self.producer_node_id,
                &self.request,
                &self.matched_paths,
                &self.result_set_hash,
                self.truncated,
            ),
        )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CandidatePathEvidence {
    pub(crate) schema_version: u16,
    pub(crate) evidence_id: EvidenceId,
    pub(crate) producer_node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) path: DiscoveryPath,
    pub(crate) rank: u32,
    pub(crate) reasons: BTreeSet<CandidateReason>,
    pub(crate) source_search_ids: BTreeSet<SearchId>,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
}

impl CandidatePathEvidence {
    fn expected_evidence_id(&self) -> Result<EvidenceId, DiscoveryContractError> {
        derived_evidence_id(
            "candidate",
            &(
                self.schema_version,
                &self.producer_node_id,
                &self.repository_revision,
                &self.path,
                self.rank,
                &self.reasons,
                &self.source_search_ids,
                &self.criterion_ids,
            ),
        )
    }

    pub(crate) fn canonicalize_id(mut self) -> Result<Self, DiscoveryContractError> {
        self.evidence_id = self.expected_evidence_id()?;
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TextEncoding {
    Utf8,
    Utf8WithBom,
    UnknownText,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileEvidence {
    pub(crate) schema_version: u16,
    pub(crate) evidence_id: EvidenceId,
    pub(crate) producer_node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) path: DiscoveryPath,
    pub(crate) line_range: LineRange,
    pub(crate) content_hash: String,
    pub(crate) artifact_reference_hash: String,
    pub(crate) encoding: TextEncoding,
    pub(crate) truncated: bool,
}

impl FileEvidence {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        producer_node_id: NodeId,
        repository_revision: RepositoryRevisionId,
        path: DiscoveryPath,
        line_range: LineRange,
        content_hash: String,
        artifact_reference_hash: String,
        encoding: TextEncoding,
        truncated: bool,
    ) -> Result<Self, DiscoveryContractError> {
        LineRange::new(line_range.start, line_range.end_inclusive)?;
        validate_hash("file.content_hash", &content_hash)?;
        validate_hash("file.artifact_reference_hash", &artifact_reference_hash)?;
        let mut evidence = Self {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            evidence_id: EvidenceId::new("pending:file-evidence"),
            producer_node_id,
            repository_revision,
            path,
            line_range,
            content_hash,
            artifact_reference_hash,
            encoding,
            truncated,
        };
        evidence.evidence_id = evidence.expected_evidence_id()?;
        Ok(evidence)
    }

    fn expected_evidence_id(&self) -> Result<EvidenceId, DiscoveryContractError> {
        derived_evidence_id(
            "file",
            &(
                self.schema_version,
                &self.producer_node_id,
                &self.repository_revision,
                &self.path,
                &self.line_range,
                &self.content_hash,
                &self.artifact_reference_hash,
                self.encoding,
                self.truncated,
            ),
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RelationshipKind {
    Imports,
    ImportedBy,
    Defines,
    Tests,
    TestedBy,
    Configures,
    GeneratedFrom,
    Related,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RelationshipEvidence {
    pub(crate) schema_version: u16,
    pub(crate) evidence_id: EvidenceId,
    pub(crate) producer_node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) from: DiscoveryPath,
    pub(crate) to: DiscoveryPath,
    pub(crate) kind: RelationshipKind,
    pub(crate) supporting_evidence_ids: BTreeSet<EvidenceId>,
}

impl RelationshipEvidence {
    pub(crate) fn new(
        producer_node_id: NodeId,
        repository_revision: RepositoryRevisionId,
        from: DiscoveryPath,
        to: DiscoveryPath,
        kind: RelationshipKind,
        supporting_evidence_ids: BTreeSet<EvidenceId>,
    ) -> Result<Self, DiscoveryContractError> {
        if from == to || supporting_evidence_ids.is_empty() {
            return Err(DiscoveryContractError::InvalidAction {
                code: "relationship_evidence_binding_invalid",
            });
        }
        let mut evidence = Self {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            evidence_id: EvidenceId::new("pending:relationship-evidence"),
            producer_node_id,
            repository_revision,
            from,
            to,
            kind,
            supporting_evidence_ids,
        };
        evidence.evidence_id = evidence.expected_evidence_id()?;
        Ok(evidence)
    }

    fn expected_evidence_id(&self) -> Result<EvidenceId, DiscoveryContractError> {
        derived_evidence_id(
            "relationship",
            &(
                self.schema_version,
                &self.producer_node_id,
                &self.repository_revision,
                &self.from,
                &self.to,
                self.kind,
                &self.supporting_evidence_ids,
            ),
        )
    }
}

#[derive(Clone, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct DiscoveryCriterionId(String);

impl DiscoveryCriterionId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, DiscoveryContractError> {
        let value = value.into();
        if value.trim().is_empty()
            || value.trim() != value
            || value.len() > 128
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(DiscoveryContractError::EmptyField {
                field: "criterion_id",
            });
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for DiscoveryCriterionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DiscoveryCriterionId")
            .field(&self.0)
            .finish()
    }
}

impl<'de> Deserialize<'de> for DiscoveryCriterionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceConfidence {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImpactArea {
    pub(crate) criterion_id: DiscoveryCriterionId,
    pub(crate) paths: BTreeSet<DiscoveryPath>,
    pub(crate) evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) confidence: EvidenceConfidence,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImpactMapEvidence {
    pub(crate) schema_version: u16,
    pub(crate) evidence_id: EvidenceId,
    pub(crate) producer_node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) areas: Vec<ImpactArea>,
    pub(crate) unresolved_question_ids: BTreeSet<DiscoveryQuestionId>,
}

impl ImpactMapEvidence {
    pub(crate) fn new(
        producer_node_id: NodeId,
        repository_revision: RepositoryRevisionId,
        areas: Vec<ImpactArea>,
        unresolved_question_ids: BTreeSet<DiscoveryQuestionId>,
    ) -> Result<Self, DiscoveryContractError> {
        let mut evidence = Self {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            evidence_id: EvidenceId::new("pending:impact-map-evidence"),
            producer_node_id,
            repository_revision,
            areas,
            unresolved_question_ids,
        };
        evidence.evidence_id = evidence.expected_evidence_id()?;
        Ok(evidence)
    }

    fn expected_evidence_id(&self) -> Result<EvidenceId, DiscoveryContractError> {
        derived_evidence_id(
            "impact-map",
            &(
                self.schema_version,
                &self.producer_node_id,
                &self.repository_revision,
                &self.areas,
                &self.unresolved_question_ids,
            ),
        )
    }
}

fn derived_evidence_id<T: Serialize>(
    kind: &'static str,
    value: &T,
) -> Result<EvidenceId, DiscoveryContractError> {
    let canonical =
        serde_json::to_string(value).map_err(|_| DiscoveryContractError::Serialization)?;
    Ok(EvidenceId::new(format!(
        "epv1:{}",
        stable_sha256(&["execution-protocol-v1:discovery-evidence", kind, &canonical,])
    )))
}

#[derive(Clone, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct DiscoveryQuestionId(String);

impl DiscoveryQuestionId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, DiscoveryContractError> {
        let value = value.into();
        if value.trim().is_empty()
            || value.trim() != value
            || value.len() > 128
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(DiscoveryContractError::EmptyField {
                field: "question_id",
            });
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for DiscoveryQuestionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DiscoveryQuestionId")
            .field(&self.0)
            .finish()
    }
}

impl<'de> Deserialize<'de> for DiscoveryQuestionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UnresolvedQuestion {
    pub(crate) id: DiscoveryQuestionId,
    pub(crate) kind: RelationshipKind,
    pub(crate) subject_path: DiscoveryPath,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
}

impl UnresolvedQuestion {
    pub(crate) fn new(
        kind: RelationshipKind,
        subject_path: DiscoveryPath,
        criterion_ids: BTreeSet<DiscoveryCriterionId>,
    ) -> Result<Self, DiscoveryContractError> {
        if criterion_ids.is_empty() {
            return Err(DiscoveryContractError::EmptyField {
                field: "question.criterion_ids",
            });
        }
        let canonical = serde_json::to_string(&(
            DISCOVERY_SCHEMA_VERSION,
            kind,
            &subject_path,
            &criterion_ids,
        ))
        .map_err(|_| DiscoveryContractError::Serialization)?;
        Ok(Self {
            id: DiscoveryQuestionId::new(format!(
                "epv1:{}",
                stable_sha256(&["execution-protocol-v1:discovery-question", &canonical,])
            ))?,
            kind,
            subject_path,
            criterion_ids,
        })
    }

    fn expected_id(&self) -> Result<DiscoveryQuestionId, DiscoveryContractError> {
        Ok(Self::new(
            self.kind,
            self.subject_path.clone(),
            self.criterion_ids.clone(),
        )?
        .id)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiscoveryGoal {
    pub(crate) goal_hash: String,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
    pub(crate) normalized_search_terms: BTreeSet<String>,
}

impl DiscoveryGoal {
    pub(crate) fn new(
        goal_hash: String,
        criterion_ids: BTreeSet<DiscoveryCriterionId>,
        search_terms: impl IntoIterator<Item = String>,
    ) -> Result<Self, DiscoveryContractError> {
        validate_hash("goal_hash", &goal_hash)?;
        if criterion_ids.is_empty() {
            return Err(DiscoveryContractError::EmptyField {
                field: "criterion_ids",
            });
        }
        if criterion_ids.len() > MAX_DISCOVERY_CRITERIA {
            return Err(DiscoveryContractError::LimitExceeded {
                field: "criterion_ids",
                limit: MAX_DISCOVERY_CRITERIA,
            });
        }
        let normalized_search_terms = search_terms
            .into_iter()
            .map(|term| normalize_search_query(&term, SearchMode::LiteralCaseInsensitive))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if normalized_search_terms.is_empty() {
            return Err(DiscoveryContractError::EmptyField {
                field: "search_terms",
            });
        }
        if normalized_search_terms.len() > MAX_DISCOVERY_SEARCH_TERMS {
            return Err(DiscoveryContractError::LimitExceeded {
                field: "search_terms",
                limit: MAX_DISCOVERY_SEARCH_TERMS,
            });
        }
        Ok(Self {
            goal_hash,
            criterion_ids,
            normalized_search_terms,
        })
    }

    pub(crate) fn validate(&self) -> Result<(), DiscoveryContractError> {
        let rebuilt = Self::new(
            self.goal_hash.clone(),
            self.criterion_ids.clone(),
            self.normalized_search_terms.iter().cloned(),
        )?;
        if rebuilt != *self {
            return Err(DiscoveryContractError::InvalidAction {
                code: "discovery_goal_not_normalized",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiscoverySubstate {
    NeedCandidates,
    NeedGroundedReads,
    NeedRelations,
    ReadyToSynthesize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DiscoveryConvergence {
    ImpactMapAccepted {
        evidence_id: EvidenceId,
    },
    EvidenceSufficientForDeterministicImpactMap {
        criterion_paths: BTreeMap<DiscoveryCriterionId, BTreeSet<DiscoveryPath>>,
        evidence_ids: BTreeSet<EvidenceId>,
    },
    InsufficientEvidence {
        reason: InsufficientEvidenceReason,
    },
    BudgetBlocked {
        reason: DiscoveryBudgetBlockReason,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InsufficientEvidenceReason {
    NoUsefulCandidates,
    NoCriterionCoverage,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiscoveryBudgetBlockReason {
    GroundedEvidenceMissing,
    RelationshipEvidenceMissing,
    ImpactMapIncomplete,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiscoveryActionRejectionReason {
    ProviderProtocolViolation,
    InvalidSearchObservation,
    InvalidFileObservation,
    InvalidRelationshipObservation,
    InvalidImpactMapObservation,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiscoveryState {
    pub(crate) schema_version: u16,
    pub(crate) node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_profile_id: RepositoryProfileId,
    pub(crate) goal: DiscoveryGoal,
    pub(crate) completed_searches: BTreeMap<SearchId, SearchEvidence>,
    pub(crate) candidates: BTreeMap<DiscoveryPath, CandidatePathEvidence>,
    pub(crate) file_evidence: BTreeMap<EvidenceId, FileEvidence>,
    pub(crate) relationships: BTreeMap<EvidenceId, RelationshipEvidence>,
    pub(crate) unresolved_questions: BTreeMap<DiscoveryQuestionId, UnresolvedQuestion>,
    pub(crate) impact_map: Option<ImpactMapEvidence>,
    pub(crate) convergence: Option<DiscoveryConvergence>,
}

impl DiscoveryState {
    pub(crate) fn new(
        node_id: NodeId,
        repository_revision: RepositoryRevisionId,
        repository_profile_id: RepositoryProfileId,
        goal: DiscoveryGoal,
    ) -> Self {
        Self {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            node_id,
            repository_revision,
            repository_profile_id,
            goal,
            completed_searches: BTreeMap::new(),
            candidates: BTreeMap::new(),
            file_evidence: BTreeMap::new(),
            relationships: BTreeMap::new(),
            unresolved_questions: BTreeMap::new(),
            impact_map: None,
            convergence: None,
        }
    }

    pub(crate) fn substate(&self) -> DiscoverySubstate {
        let candidate_criteria = self
            .candidates
            .values()
            .flat_map(|candidate| candidate.criterion_ids.iter().cloned())
            .collect::<BTreeSet<_>>();
        if self.candidates.is_empty() {
            DiscoverySubstate::NeedCandidates
        } else if !self.ranked_candidate_paths().is_empty() {
            DiscoverySubstate::NeedGroundedReads
        } else if candidate_criteria != self.goal.criterion_ids {
            DiscoverySubstate::NeedCandidates
        } else if !self.unresolved_questions.is_empty() {
            DiscoverySubstate::NeedRelations
        } else {
            DiscoverySubstate::ReadyToSynthesize
        }
    }

    pub(crate) fn grounded_candidate_paths(&self) -> BTreeSet<DiscoveryPath> {
        self.file_evidence
            .values()
            .filter(|evidence| {
                evidence.repository_revision == self.repository_revision
                    && self.candidates.contains_key(&evidence.path)
            })
            .map(|evidence| evidence.path.clone())
            .collect()
    }

    pub(crate) fn grounded_criterion_ids(&self) -> BTreeSet<DiscoveryCriterionId> {
        let grounded_paths = self.grounded_candidate_paths();
        self.candidates
            .values()
            .filter(|candidate| grounded_paths.contains(&candidate.path))
            .flat_map(|candidate| candidate.criterion_ids.iter().cloned())
            .collect()
    }

    pub(crate) fn non_relationship_evidence_touches_path(
        &self,
        evidence_id: &EvidenceId,
        path: &DiscoveryPath,
    ) -> bool {
        self.completed_searches.values().any(|evidence| {
            &evidence.evidence_id == evidence_id && evidence.matched_paths.contains(path)
        }) || self
            .candidates
            .values()
            .any(|evidence| &evidence.evidence_id == evidence_id && &evidence.path == path)
            || self
                .file_evidence
                .values()
                .any(|evidence| &evidence.evidence_id == evidence_id && &evidence.path == path)
    }

    pub(crate) fn ranked_candidate_paths(&self) -> Vec<DiscoveryPath> {
        let grounded_paths = self.grounded_candidate_paths();
        self.all_ranked_candidate_paths()
            .into_iter()
            .filter(|path| !grounded_paths.contains(path))
            .take(MAX_GROUNDING_PATHS_PER_ACTION)
            .collect()
    }

    pub(crate) fn all_ranked_candidate_paths(&self) -> Vec<DiscoveryPath> {
        let mut candidates = self.candidates.values().collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            left.rank
                .cmp(&right.rank)
                .then_with(|| left.path.cmp(&right.path))
        });
        candidates
            .into_iter()
            .take(MAX_ACTION_PATHS)
            .map(|candidate| candidate.path.clone())
            .collect()
    }

    pub(crate) fn impact_map_evidence_ids(&self) -> BTreeSet<EvidenceId> {
        let grounded_paths = self.grounded_candidate_paths();
        let mut selected = BTreeSet::new();
        for criterion_id in &self.goal.criterion_ids {
            let mut candidates = self
                .candidates
                .values()
                .filter(|candidate| {
                    candidate.criterion_ids.contains(criterion_id)
                        && grounded_paths.contains(&candidate.path)
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|left, right| {
                left.rank
                    .cmp(&right.rank)
                    .then_with(|| left.path.cmp(&right.path))
            });
            if let Some(candidate) = candidates.into_iter().find(|candidate| {
                self.file_evidence
                    .values()
                    .any(|evidence| evidence.path == candidate.path)
            }) {
                if let Some(evidence_id) = self
                    .file_evidence
                    .values()
                    .filter(|evidence| evidence.path == candidate.path)
                    .map(|evidence| evidence.evidence_id.clone())
                    .min()
                {
                    selected.insert(evidence_id);
                }
                if let Some(evidence_id) = self
                    .relationships
                    .values()
                    .filter(|evidence| {
                        evidence.from == candidate.path || evidence.to == candidate.path
                    })
                    .map(|evidence| evidence.evidence_id.clone())
                    .min()
                {
                    selected.insert(evidence_id);
                }
            }
        }
        selected
    }

    fn impact_area_is_semantically_grounded(&self, area: &ImpactArea) -> bool {
        let grounded_paths = self.grounded_candidate_paths();
        self.goal.criterion_ids.contains(&area.criterion_id)
            && !area.paths.is_empty()
            && !area.evidence_ids.is_empty()
            && area.paths.iter().all(|path| {
                grounded_paths.contains(path)
                    && self.candidates.get(path).is_some_and(|candidate| {
                        candidate.criterion_ids.contains(&area.criterion_id)
                    })
            })
            && area.evidence_ids.iter().all(|evidence_id| {
                self.file_evidence
                    .get(evidence_id)
                    .is_some_and(|evidence| area.paths.contains(&evidence.path))
                    || self.relationships.get(evidence_id).is_some_and(|evidence| {
                        area.paths.contains(&evidence.from) || area.paths.contains(&evidence.to)
                    })
            })
    }

    pub(crate) fn classify_search(&self, request: &SearchRequest) -> SearchAdmission {
        if self.completed_searches.contains_key(request.id()) {
            SearchAdmission::DuplicateCompleted {
                search_id: request.search_id.clone(),
            }
        } else {
            SearchAdmission::New {
                search_id: request.search_id.clone(),
            }
        }
    }

    pub(crate) fn validate_revision(&self) -> Result<(), DiscoveryContractError> {
        let same_revision =
            self.completed_searches
                .values()
                .all(|evidence| evidence.request.repository_revision == self.repository_revision)
                && self
                    .candidates
                    .values()
                    .all(|evidence| evidence.repository_revision == self.repository_revision)
                && self
                    .file_evidence
                    .values()
                    .all(|evidence| evidence.repository_revision == self.repository_revision)
                && self
                    .relationships
                    .values()
                    .all(|evidence| evidence.repository_revision == self.repository_revision)
                && self.impact_map.as_ref().is_none_or(|evidence| {
                    evidence.repository_revision == self.repository_revision
                });
        if same_revision {
            Ok(())
        } else {
            Err(DiscoveryContractError::RepositoryRevisionMismatch)
        }
    }

    pub(crate) fn validate(&self) -> Result<(), DiscoveryContractError> {
        if self.schema_version != DISCOVERY_SCHEMA_VERSION {
            return Err(DiscoveryContractError::InvalidAction {
                code: "discovery_schema_version_invalid",
            });
        }
        self.goal.validate()?;
        enforce_state_limits(self)?;

        let mut all_evidence_ids = BTreeSet::new();
        for (search_id, evidence) in &self.completed_searches {
            if evidence.schema_version != DISCOVERY_SCHEMA_VERSION
                || &evidence.request.search_id != search_id
                || evidence.producer_node_id != self.node_id
                || evidence.request.repository_revision != self.repository_revision
                || evidence.request.repository_profile_id != self.repository_profile_id
                || !evidence
                    .request
                    .criterion_ids
                    .is_subset(&self.goal.criterion_ids)
                || evidence.evidence_id != evidence.expected_evidence_id()?
            {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "search_evidence_binding_invalid",
                });
            }
            evidence.request.validate()?;
            if evidence.matched_paths.len() > MAX_SEARCH_MATCHES {
                return Err(DiscoveryContractError::LimitExceeded {
                    field: "search.matched_paths",
                    limit: MAX_SEARCH_MATCHES,
                });
            }
            if !evidence
                .matched_paths
                .iter()
                .all(|path| evidence.request.permits_path(path))
            {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "search_result_outside_authorized_scope",
                });
            }
            validate_hash("search.result_set_hash", &evidence.result_set_hash)?;
            if evidence.result_set_hash != search_result_set_hash(&evidence.matched_paths)? {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "search_result_set_hash_mismatch",
                });
            }
            if !all_evidence_ids.insert(evidence.evidence_id.clone()) {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "discovery_evidence_id_duplicate",
                });
            }
        }

        for (path, evidence) in &self.candidates {
            let source_criteria = evidence
                .source_search_ids
                .iter()
                .filter_map(|search_id| self.completed_searches.get(search_id))
                .flat_map(|search| search.request.criterion_ids.iter().cloned())
                .collect::<BTreeSet<_>>();
            if evidence.schema_version != DISCOVERY_SCHEMA_VERSION
                || &evidence.path != path
                || evidence.producer_node_id != self.node_id
                || evidence.repository_revision != self.repository_revision
                || evidence.rank == 0
                || evidence.reasons.is_empty()
                || evidence.criterion_ids.is_empty()
                || evidence.evidence_id != evidence.expected_evidence_id()?
                || !evidence
                    .criterion_ids
                    .iter()
                    .all(|criterion| self.goal.criterion_ids.contains(criterion))
                || !evidence
                    .source_search_ids
                    .iter()
                    .all(|search_id| self.completed_searches.contains_key(search_id))
                || evidence.reasons.contains(&CandidateReason::SearchMatch)
                    && evidence.source_search_ids.is_empty()
                || evidence.reasons == BTreeSet::from([CandidateReason::SearchMatch])
                    && evidence.criterion_ids != source_criteria
            {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "candidate_evidence_binding_invalid",
                });
            }
            if !all_evidence_ids.insert(evidence.evidence_id.clone()) {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "discovery_evidence_id_duplicate",
                });
            }
        }

        for (evidence_id, evidence) in &self.file_evidence {
            if evidence.schema_version != DISCOVERY_SCHEMA_VERSION
                || &evidence.evidence_id != evidence_id
                || evidence.producer_node_id != self.node_id
                || evidence.repository_revision != self.repository_revision
                || evidence.evidence_id != evidence.expected_evidence_id()?
            {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "file_evidence_binding_invalid",
                });
            }
            LineRange::new(evidence.line_range.start, evidence.line_range.end_inclusive)?;
            validate_hash("file.content_hash", &evidence.content_hash)?;
            validate_hash(
                "file.artifact_reference_hash",
                &evidence.artifact_reference_hash,
            )?;
            if !all_evidence_ids.insert(evidence.evidence_id.clone()) {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "discovery_evidence_id_duplicate",
                });
            }
        }

        let non_relationship_evidence_ids = all_evidence_ids.clone();
        for (evidence_id, evidence) in &self.relationships {
            if evidence.schema_version != DISCOVERY_SCHEMA_VERSION
                || &evidence.evidence_id != evidence_id
                || evidence.producer_node_id != self.node_id
                || evidence.repository_revision != self.repository_revision
                || evidence.from == evidence.to
                || evidence.evidence_id != evidence.expected_evidence_id()?
                || evidence.supporting_evidence_ids.is_empty()
                || !evidence
                    .supporting_evidence_ids
                    .iter()
                    .all(|support| non_relationship_evidence_ids.contains(support))
                || !evidence.supporting_evidence_ids.iter().all(|support| {
                    self.non_relationship_evidence_touches_path(support, &evidence.from)
                        || self.non_relationship_evidence_touches_path(support, &evidence.to)
                })
            {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "relationship_evidence_binding_invalid",
                });
            }
            if !all_evidence_ids.insert(evidence.evidence_id.clone()) {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "discovery_evidence_id_duplicate",
                });
            }
        }

        for (question_id, question) in &self.unresolved_questions {
            let subject_candidate = self.candidates.get(&question.subject_path);
            if &question.id != question_id
                || question.id != question.expected_id()?
                || subject_candidate.is_none()
                || question.criterion_ids.is_empty()
                || !question.criterion_ids.is_subset(
                    &subject_candidate
                        .expect("question subject candidate was checked")
                        .criterion_ids,
                )
            {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "unresolved_question_binding_invalid",
                });
            }
        }

        if let Some(impact_map) = &self.impact_map {
            let mapped_criteria = impact_map
                .areas
                .iter()
                .map(|area| &area.criterion_id)
                .collect::<BTreeSet<_>>();
            if impact_map.schema_version != DISCOVERY_SCHEMA_VERSION
                || impact_map.producer_node_id != self.node_id
                || impact_map.repository_revision != self.repository_revision
                || impact_map.areas.is_empty()
                || impact_map.areas.len() > self.goal.criterion_ids.len()
                || mapped_criteria.len() != impact_map.areas.len()
                || impact_map.evidence_id != impact_map.expected_evidence_id()?
                || !impact_map
                    .areas
                    .windows(2)
                    .all(|pair| pair[0].criterion_id < pair[1].criterion_id)
                || !impact_map
                    .unresolved_question_ids
                    .iter()
                    .all(|question| self.unresolved_questions.contains_key(question))
            {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "impact_map_binding_invalid",
                });
            }
            for area in &impact_map.areas {
                if !self.impact_area_is_semantically_grounded(area) {
                    return Err(DiscoveryContractError::InvalidAction {
                        code: "impact_area_grounding_invalid",
                    });
                }
            }
            if !all_evidence_ids.insert(impact_map.evidence_id.clone()) {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "discovery_evidence_id_duplicate",
                });
            }
        }

        if self
            .convergence
            .as_ref()
            .is_some_and(|convergence| convergence != &evaluate_discovery_convergence(self))
        {
            return Err(DiscoveryContractError::InvalidAction {
                code: "discovery_convergence_not_authoritative",
            });
        }
        self.validate_revision()
    }
}

fn enforce_state_limits(state: &DiscoveryState) -> Result<(), DiscoveryContractError> {
    for (field, actual, limit) in [
        (
            "discovery.completed_searches",
            state.completed_searches.len(),
            MAX_COMPLETED_SEARCHES,
        ),
        (
            "discovery.candidates",
            state.candidates.len(),
            MAX_DISCOVERY_CANDIDATES,
        ),
        (
            "discovery.file_evidence",
            state.file_evidence.len(),
            MAX_FILE_EVIDENCE,
        ),
        (
            "discovery.relationships",
            state.relationships.len(),
            MAX_RELATIONSHIP_EVIDENCE,
        ),
        (
            "discovery.unresolved_questions",
            state.unresolved_questions.len(),
            MAX_UNRESOLVED_QUESTIONS,
        ),
    ] {
        if actual > limit {
            return Err(DiscoveryContractError::LimitExceeded { field, limit });
        }
    }
    Ok(())
}

pub(crate) fn search_result_set_hash(
    paths: &BTreeSet<DiscoveryPath>,
) -> Result<String, DiscoveryContractError> {
    let canonical =
        serde_json::to_string(paths).map_err(|_| DiscoveryContractError::Serialization)?;
    Ok(stable_sha256(&[
        "execution-protocol-v1:search-result-set",
        &canonical,
    ]))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SearchAdmission {
    New { search_id: SearchId },
    DuplicateCompleted { search_id: SearchId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiscoveryNextStep {
    Action(DiscoveryActionClass),
    Converge(DiscoveryConvergence),
}

pub(crate) fn select_next_discovery_step(
    state: &DiscoveryState,
    admissible_model_calls_remaining: u32,
) -> DiscoveryNextStep {
    if state
        .impact_map
        .as_ref()
        .is_some_and(|impact_map| impact_map_is_complete(state, impact_map))
        || admissible_model_calls_remaining == 0
    {
        return DiscoveryNextStep::Converge(evaluate_discovery_convergence(state));
    }
    match state.substate() {
        DiscoverySubstate::NeedCandidates if all_goal_searches_completed(state) => {
            DiscoveryNextStep::Converge(evaluate_discovery_convergence(state))
        }
        DiscoverySubstate::NeedCandidates => {
            DiscoveryNextStep::Action(DiscoveryActionClass::DiscoverCandidates)
        }
        // Once any useful candidate exists, the next action is a bounded read.
        // This makes the mandatory final-call rule structural: search cannot be
        // selected when one call remains (or at any later grounding step).
        DiscoverySubstate::NeedGroundedReads => {
            DiscoveryNextStep::Action(DiscoveryActionClass::GroundCandidateEvidence)
        }
        DiscoverySubstate::NeedRelations => {
            DiscoveryNextStep::Action(DiscoveryActionClass::ResolveNamedRelationship)
        }
        DiscoverySubstate::ReadyToSynthesize => {
            DiscoveryNextStep::Action(DiscoveryActionClass::RecordImpactMap)
        }
    }
}

pub(crate) fn all_goal_searches_completed(state: &DiscoveryState) -> bool {
    state.goal.criterion_ids.iter().all(|criterion_id| {
        state.goal.normalized_search_terms.iter().all(|term| {
            state.completed_searches.values().any(|evidence| {
                evidence.request.criterion_ids == BTreeSet::from([criterion_id.clone()])
                    && evidence.request.normalized_query == *term
            })
        })
    })
}

pub(crate) fn evaluate_discovery_convergence(state: &DiscoveryState) -> DiscoveryConvergence {
    if let Some(impact_map) = &state.impact_map
        && impact_map_is_complete(state, impact_map)
    {
        return DiscoveryConvergence::ImpactMapAccepted {
            evidence_id: impact_map.evidence_id.clone(),
        };
    }
    if state.candidates.is_empty() {
        return DiscoveryConvergence::InsufficientEvidence {
            reason: InsufficientEvidenceReason::NoUsefulCandidates,
        };
    }
    let grounded = state.grounded_candidate_paths();
    if grounded.is_empty() {
        return DiscoveryConvergence::BudgetBlocked {
            reason: DiscoveryBudgetBlockReason::GroundedEvidenceMissing,
        };
    }
    if !state.unresolved_questions.is_empty() {
        return DiscoveryConvergence::BudgetBlocked {
            reason: DiscoveryBudgetBlockReason::RelationshipEvidenceMissing,
        };
    }

    let mut criterion_paths = state
        .goal
        .criterion_ids
        .iter()
        .cloned()
        .map(|criterion| (criterion, BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for candidate in state
        .candidates
        .values()
        .filter(|candidate| grounded.contains(&candidate.path))
    {
        for criterion in &candidate.criterion_ids {
            if let Some(paths) = criterion_paths.get_mut(criterion) {
                paths.insert(candidate.path.clone());
            }
        }
    }
    if criterion_paths.values().any(BTreeSet::is_empty) {
        return DiscoveryConvergence::InsufficientEvidence {
            reason: InsufficientEvidenceReason::NoCriterionCoverage,
        };
    }
    let evidence_ids = state.impact_map_evidence_ids();
    DiscoveryConvergence::EvidenceSufficientForDeterministicImpactMap {
        criterion_paths,
        evidence_ids,
    }
}

pub(crate) fn impact_map_is_complete(
    state: &DiscoveryState,
    impact_map: &ImpactMapEvidence,
) -> bool {
    if impact_map.repository_revision != state.repository_revision
        || !impact_map.unresolved_question_ids.is_empty()
    {
        return false;
    }
    let covered = impact_map
        .areas
        .iter()
        .map(|area| &area.criterion_id)
        .collect::<BTreeSet<_>>();
    state
        .goal
        .criterion_ids
        .iter()
        .all(|criterion| covered.contains(criterion))
        && impact_map
            .areas
            .iter()
            .all(|area| state.impact_area_is_semantically_grounded(area))
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiscoveryModelPurpose {
    DiscoverCandidates,
    GroundCandidateEvidence,
    ResolveNamedRelationship,
    RecordImpactMap,
}

#[derive(Clone, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(
    tag = "section",
    content = "reference",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ContextSection {
    ProtocolInstructions { schema_hash: String },
    Goal { goal_hash: String },
    AcceptanceCriterion { criterion_id: DiscoveryCriterionId },
    RepositoryProfile { profile_id: RepositoryProfileId },
    Evidence { evidence_id: EvidenceId },
    UnresolvedRelationship { question_id: DiscoveryQuestionId },
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompactionKind {
    OmittedOptional,
    BoundedRange,
    DeterministicSummary,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompactionDecision {
    pub(crate) section: ContextSection,
    pub(crate) kind: CompactionKind,
    pub(crate) original_estimated_tokens: u32,
    pub(crate) retained_estimated_tokens: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContextManifest {
    pub(crate) schema_version: u16,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) action_id: ActionId,
    pub(crate) node_id: NodeId,
    pub(crate) purpose: DiscoveryModelPurpose,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) mandatory_sections: Vec<ContextSection>,
    pub(crate) optional_sections: Vec<ContextSection>,
    pub(crate) input_token_ceiling: u32,
    pub(crate) estimated_input_tokens: u32,
    pub(crate) compaction: Vec<CompactionDecision>,
    pub(crate) materialized_context_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ContextBuildError {
    Contract(DiscoveryContractError),
    MandatoryTooLarge {
        required_tokens: u32,
        input_token_ceiling: u32,
    },
}

impl fmt::Display for ContextBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contract(error) => write!(formatter, "discovery context build failed: {error}"),
            Self::MandatoryTooLarge {
                required_tokens,
                input_token_ceiling,
            } => write!(
                formatter,
                "mandatory discovery context requires {required_tokens} tokens but the input ceiling is {input_token_ceiling}"
            ),
        }
    }
}

impl std::error::Error for ContextBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Contract(error) => Some(error),
            Self::MandatoryTooLarge { .. } => None,
        }
    }
}

impl From<DiscoveryContractError> for ContextBuildError {
    fn from(error: DiscoveryContractError) -> Self {
        Self::Contract(error)
    }
}

/// Builds the persisted, identity-only context for one discovery action.
///
/// Raw repository content is materialized later from the content-addressed
/// evidence references. This function only selects stable references, applies
/// the signed input ceiling, and records every optional section omitted by
/// that ceiling.
pub(crate) fn build_discovery_context(
    state: &DiscoveryState,
    action_id: ActionId,
    constraints: &DiscoveryActionConstraints,
    input_token_ceiling: u32,
) -> Result<ContextManifest, ContextBuildError> {
    state.validate()?;

    let action_class = constraints.action_class();
    let ordered_evidence = ordered_discovery_evidence(state);
    let mandatory_evidence = mandatory_evidence_for_action(state, constraints)?;
    let mandatory_question = mandatory_question_for_action(state, constraints)?;
    let relevant_criteria = relevant_criteria_for_action(state, constraints)?;

    let mut mandatory_sections = vec![
        ContextSection::ProtocolInstructions {
            schema_hash: stable_sha256(&["execution-protocol-v1:discovery-context-schema", "1"]),
        },
        ContextSection::Goal {
            goal_hash: state.goal.goal_hash.clone(),
        },
    ];
    mandatory_sections.extend(
        relevant_criteria
            .into_iter()
            .map(|criterion_id| ContextSection::AcceptanceCriterion { criterion_id }),
    );
    mandatory_sections.push(ContextSection::RepositoryProfile {
        profile_id: state.repository_profile_id.clone(),
    });
    if let Some(question_id) = mandatory_question {
        mandatory_sections.push(ContextSection::UnresolvedRelationship { question_id });
    }
    mandatory_sections.extend(
        ordered_evidence
            .iter()
            .filter(|evidence_id| mandatory_evidence.contains(*evidence_id))
            .cloned()
            .map(|evidence_id| ContextSection::Evidence { evidence_id }),
    );

    if mandatory_sections.len() > MAX_CONTEXT_SECTIONS {
        return Err(DiscoveryContractError::LimitExceeded {
            field: "context.mandatory_sections",
            limit: MAX_CONTEXT_SECTIONS,
        }
        .into());
    }

    let required_tokens = estimate_context_tokens(state, &mandatory_sections)?;
    if required_tokens > input_token_ceiling {
        return Err(ContextBuildError::MandatoryTooLarge {
            required_tokens,
            input_token_ceiling,
        });
    }

    let mut optional_candidates = Vec::new();
    optional_candidates.dedup();
    optional_candidates.truncate(MAX_CONTEXT_SECTIONS.saturating_sub(mandatory_sections.len()));

    let mut estimated_input_tokens = required_tokens;
    let mut optional_sections = Vec::new();
    let mut compaction = Vec::new();
    for section in optional_candidates {
        let section_tokens = estimate_context_section_tokens(state, &section)?;
        if estimated_input_tokens.saturating_add(section_tokens) <= input_token_ceiling {
            estimated_input_tokens = estimated_input_tokens.saturating_add(section_tokens);
            optional_sections.push(section);
        } else {
            compaction.push(CompactionDecision {
                section,
                kind: CompactionKind::OmittedOptional,
                original_estimated_tokens: section_tokens,
                retained_estimated_tokens: 0,
            });
        }
    }

    let evidence_ids = mandatory_sections
        .iter()
        .chain(&optional_sections)
        .filter_map(|section| match section {
            ContextSection::Evidence { evidence_id } => Some(evidence_id.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let materialized_identity = serde_json::to_string(&(
        DISCOVERY_SCHEMA_VERSION,
        &action_id,
        &state.node_id,
        constraints,
        &state.repository_revision,
        &mandatory_sections,
        &optional_sections,
        input_token_ceiling,
        estimated_input_tokens,
        &compaction,
    ))
    .map_err(|_| DiscoveryContractError::Serialization)?;
    let materialized_context_hash = stable_sha256(&[
        "execution-protocol-v1:discovery-materialized-context",
        &materialized_identity,
    ]);
    let manifest = ContextManifest::new(
        action_id,
        state.node_id.clone(),
        action_class.purpose(),
        state.repository_revision.clone(),
        evidence_ids,
        mandatory_sections,
        optional_sections,
        input_token_ceiling,
        estimated_input_tokens,
        compaction,
        materialized_context_hash,
    )?;
    manifest.validate()?;
    Ok(manifest)
}

fn ordered_discovery_evidence(state: &DiscoveryState) -> Vec<EvidenceId> {
    let mut ordered = state
        .completed_searches
        .values()
        .map(|evidence| evidence.evidence_id.clone())
        .collect::<Vec<_>>();

    let mut candidates = state.candidates.values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.evidence_id.cmp(&right.evidence_id))
    });
    ordered.extend(
        candidates
            .into_iter()
            .map(|evidence| evidence.evidence_id.clone()),
    );

    let mut file_evidence = state.file_evidence.values().collect::<Vec<_>>();
    file_evidence.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.line_range.start.cmp(&right.line_range.start))
            .then_with(|| {
                left.line_range
                    .end_inclusive
                    .cmp(&right.line_range.end_inclusive)
            })
            .then_with(|| left.evidence_id.cmp(&right.evidence_id))
    });
    ordered.extend(
        file_evidence
            .into_iter()
            .map(|evidence| evidence.evidence_id.clone()),
    );

    let mut relationships = state.relationships.values().collect::<Vec<_>>();
    relationships.sort_by(|left, right| {
        left.from
            .cmp(&right.from)
            .then_with(|| left.to.cmp(&right.to))
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.evidence_id.cmp(&right.evidence_id))
    });
    ordered.extend(
        relationships
            .into_iter()
            .map(|evidence| evidence.evidence_id.clone()),
    );
    ordered
}

fn mandatory_evidence_for_action(
    state: &DiscoveryState,
    constraints: &DiscoveryActionConstraints,
) -> Result<BTreeSet<EvidenceId>, DiscoveryContractError> {
    let evidence_ids = match constraints {
        DiscoveryActionConstraints::Search { request } => request.context_evidence_ids.clone(),
        DiscoveryActionConstraints::ExactPaths { paths } => {
            if paths.is_empty() {
                return Err(DiscoveryContractError::InvalidContext {
                    code: "grounding_context_candidates_missing",
                });
            }
            paths
                .iter()
                .map(|path| {
                    state
                        .candidates
                        .get(path)
                        .map(|candidate| candidate.evidence_id.clone())
                        .ok_or(DiscoveryContractError::InvalidContext {
                            code: "grounding_context_candidate_missing",
                        })
                })
                .collect::<Result<_, _>>()?
        }
        DiscoveryActionConstraints::NamedRelationship { question, .. } => {
            if state.unresolved_questions.get(&question.id) != Some(question) {
                return Err(DiscoveryContractError::InvalidContext {
                    code: "relationship_context_question_missing",
                });
            }
            let mut evidence_ids = state
                .candidates
                .get(&question.subject_path)
                .map(|candidate| candidate.evidence_id.clone())
                .into_iter()
                .collect::<BTreeSet<_>>();
            if let Some(file_evidence_id) = state
                .file_evidence
                .values()
                .filter(|evidence| evidence.path == question.subject_path)
                .map(|evidence| evidence.evidence_id.clone())
                .min()
            {
                evidence_ids.insert(file_evidence_id);
            }
            evidence_ids
        }
        DiscoveryActionConstraints::ImpactMap { evidence_ids, .. } => {
            if evidence_ids.is_empty() {
                return Err(DiscoveryContractError::InvalidContext {
                    code: "impact_map_context_evidence_missing",
                });
            }
            evidence_ids.clone()
        }
    };
    Ok(evidence_ids)
}

fn mandatory_question_for_action(
    state: &DiscoveryState,
    constraints: &DiscoveryActionConstraints,
) -> Result<Option<DiscoveryQuestionId>, DiscoveryContractError> {
    if let DiscoveryActionConstraints::NamedRelationship { question, .. } = constraints {
        if state.unresolved_questions.get(&question.id) != Some(question) {
            return Err(DiscoveryContractError::InvalidContext {
                code: "relationship_context_question_missing",
            });
        }
        return Ok(Some(question.id.clone()));
    }
    Ok(None)
}

fn relevant_criteria_for_action(
    state: &DiscoveryState,
    constraints: &DiscoveryActionConstraints,
) -> Result<BTreeSet<DiscoveryCriterionId>, DiscoveryContractError> {
    let selected = match constraints {
        DiscoveryActionConstraints::Search { request } => request.criterion_ids.clone(),
        DiscoveryActionConstraints::ExactPaths { paths } => paths
            .iter()
            .map(|path| {
                state
                    .candidates
                    .get(path)
                    .ok_or(DiscoveryContractError::InvalidContext {
                        code: "grounding_context_candidate_missing",
                    })
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flat_map(|candidate| candidate.criterion_ids.iter().cloned())
            .collect(),
        DiscoveryActionConstraints::NamedRelationship { question, .. } => {
            question.criterion_ids.clone()
        }
        DiscoveryActionConstraints::ImpactMap { criterion_ids, .. } => criterion_ids.clone(),
    };
    if selected.is_empty() {
        return Err(DiscoveryContractError::InvalidContext {
            code: "action_context_criteria_missing",
        });
    }
    Ok(selected)
}

fn estimate_context_tokens(
    state: &DiscoveryState,
    sections: &[ContextSection],
) -> Result<u32, DiscoveryContractError> {
    sections.iter().try_fold(0_u32, |total, section| {
        estimate_context_section_tokens(state, section).map(|tokens| total.saturating_add(tokens))
    })
}

fn estimate_context_section_tokens(
    state: &DiscoveryState,
    section: &ContextSection,
) -> Result<u32, DiscoveryContractError> {
    let canonical_bytes = serde_json::to_vec(section)
        .map_err(|_| DiscoveryContractError::Serialization)?
        .len();
    let identity_tokens = u32::try_from(canonical_bytes.div_ceil(4))
        .unwrap_or(u32::MAX)
        .saturating_add(8);
    let materialization_tokens = match section {
        ContextSection::ProtocolInstructions { .. } => 192,
        ContextSection::Goal { .. } => 64,
        ContextSection::AcceptanceCriterion { .. } => 48,
        ContextSection::RepositoryProfile { .. } => 96,
        ContextSection::UnresolvedRelationship { .. } => 64,
        ContextSection::Evidence { evidence_id } => {
            estimate_evidence_materialization_tokens(state, evidence_id)
        }
    };
    Ok(identity_tokens.saturating_add(materialization_tokens))
}

fn estimate_evidence_materialization_tokens(
    state: &DiscoveryState,
    evidence_id: &EvidenceId,
) -> u32 {
    if let Some(search) = state
        .completed_searches
        .values()
        .find(|evidence| &evidence.evidence_id == evidence_id)
    {
        return 48_u32.saturating_add(
            u32::try_from(search.matched_paths.len())
                .unwrap_or(u32::MAX)
                .saturating_mul(12),
        );
    }
    if let Some(candidate) = state
        .candidates
        .values()
        .find(|evidence| &evidence.evidence_id == evidence_id)
    {
        return 48_u32
            .saturating_add(
                u32::try_from(candidate.criterion_ids.len())
                    .unwrap_or(u32::MAX)
                    .saturating_mul(12),
            )
            .saturating_add(
                u32::try_from(candidate.source_search_ids.len())
                    .unwrap_or(u32::MAX)
                    .saturating_mul(8),
            );
    }
    if let Some(file) = state.file_evidence.get(evidence_id) {
        let line_count = file
            .line_range
            .end_inclusive
            .saturating_sub(file.line_range.start)
            .saturating_add(1);
        return 32_u32.saturating_add(line_count.saturating_mul(16));
    }
    if let Some(relationship) = state.relationships.get(evidence_id) {
        return 48_u32.saturating_add(
            u32::try_from(relationship.supporting_evidence_ids.len())
                .unwrap_or(u32::MAX)
                .saturating_mul(12),
        );
    }
    32
}

impl ContextManifest {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        action_id: ActionId,
        node_id: NodeId,
        purpose: DiscoveryModelPurpose,
        repository_revision: RepositoryRevisionId,
        evidence_ids: BTreeSet<EvidenceId>,
        mandatory_sections: Vec<ContextSection>,
        optional_sections: Vec<ContextSection>,
        input_token_ceiling: u32,
        estimated_input_tokens: u32,
        compaction: Vec<CompactionDecision>,
        materialized_context_hash: String,
    ) -> Result<Self, DiscoveryContractError> {
        validate_hash("materialized_context_hash", &materialized_context_hash)?;
        if evidence_ids.len() > MAX_CONTEXT_EVIDENCE {
            return Err(DiscoveryContractError::LimitExceeded {
                field: "context.evidence_ids",
                limit: MAX_CONTEXT_EVIDENCE,
            });
        }
        if mandatory_sections
            .len()
            .saturating_add(optional_sections.len())
            > MAX_CONTEXT_SECTIONS
        {
            return Err(DiscoveryContractError::LimitExceeded {
                field: "context.sections",
                limit: MAX_CONTEXT_SECTIONS,
            });
        }
        if input_token_ceiling == 0 || estimated_input_tokens > input_token_ceiling {
            return Err(DiscoveryContractError::InvalidContext {
                code: "context_token_ceiling_exceeded",
            });
        }
        let identity = serde_json::to_string(&(
            DISCOVERY_SCHEMA_VERSION,
            &action_id,
            &node_id,
            purpose,
            &repository_revision,
            &evidence_ids,
            &mandatory_sections,
            &optional_sections,
            input_token_ceiling,
            estimated_input_tokens,
            &compaction,
            &materialized_context_hash,
        ))
        .map_err(|_| DiscoveryContractError::Serialization)?;
        let context_manifest_id = ContextManifestId::new(format!(
            "epv1:{}",
            stable_sha256(&["execution-protocol-v1:context-manifest", &identity])
        ));
        Ok(Self {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            context_manifest_id,
            action_id,
            node_id,
            purpose,
            repository_revision,
            evidence_ids,
            mandatory_sections,
            optional_sections,
            input_token_ceiling,
            estimated_input_tokens,
            compaction,
            materialized_context_hash,
        })
    }

    pub(crate) fn validate(&self) -> Result<(), DiscoveryContractError> {
        if self.schema_version != DISCOVERY_SCHEMA_VERSION {
            return Err(DiscoveryContractError::InvalidContext {
                code: "context_schema_version_invalid",
            });
        }
        let sections = self
            .mandatory_sections
            .iter()
            .chain(&self.optional_sections)
            .collect::<BTreeSet<_>>();
        if sections.len()
            != self
                .mandatory_sections
                .len()
                .saturating_add(self.optional_sections.len())
        {
            return Err(DiscoveryContractError::InvalidContext {
                code: "context_section_duplicate",
            });
        }
        if sections.iter().any(|section| {
            matches!(section, ContextSection::Evidence { evidence_id } if !self.evidence_ids.contains(evidence_id))
        }) {
            return Err(DiscoveryContractError::InvalidContext {
                code: "context_evidence_reference_invalid",
            });
        }
        if self.evidence_ids.iter().any(|evidence_id| {
            !sections.iter().any(|section| {
                matches!(section, ContextSection::Evidence { evidence_id: section_id } if section_id == evidence_id)
            })
        }) {
            return Err(DiscoveryContractError::InvalidContext {
                code: "context_evidence_section_missing",
            });
        }
        if self.compaction.len() > MAX_CONTEXT_SECTIONS {
            return Err(DiscoveryContractError::LimitExceeded {
                field: "context.compaction",
                limit: MAX_CONTEXT_SECTIONS,
            });
        }
        let compacted_sections = self
            .compaction
            .iter()
            .map(|decision| &decision.section)
            .collect::<BTreeSet<_>>();
        if compacted_sections.len() != self.compaction.len()
            || self.compaction.iter().any(|decision| {
                decision.retained_estimated_tokens > decision.original_estimated_tokens
                    || match decision.kind {
                        CompactionKind::OmittedOptional => {
                            decision.retained_estimated_tokens != 0
                                || sections.contains(&decision.section)
                        }
                        CompactionKind::BoundedRange | CompactionKind::DeterministicSummary => {
                            !sections.contains(&decision.section)
                        }
                    }
            })
        {
            return Err(DiscoveryContractError::InvalidContext {
                code: "context_compaction_invalid",
            });
        }
        let rebuilt = Self::new(
            self.action_id.clone(),
            self.node_id.clone(),
            self.purpose,
            self.repository_revision.clone(),
            self.evidence_ids.clone(),
            self.mandatory_sections.clone(),
            self.optional_sections.clone(),
            self.input_token_ceiling,
            self.estimated_input_tokens,
            self.compaction.clone(),
            self.materialized_context_hash.clone(),
        )?;
        if rebuilt != *self {
            return Err(DiscoveryContractError::InvalidContext {
                code: "context_identity_mismatch",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiscoveryActionClass {
    DiscoverCandidates,
    GroundCandidateEvidence,
    ResolveNamedRelationship,
    RecordImpactMap,
}

impl DiscoveryActionClass {
    pub(crate) const fn purpose(self) -> DiscoveryModelPurpose {
        match self {
            Self::DiscoverCandidates => DiscoveryModelPurpose::DiscoverCandidates,
            Self::GroundCandidateEvidence => DiscoveryModelPurpose::GroundCandidateEvidence,
            Self::ResolveNamedRelationship => DiscoveryModelPurpose::ResolveNamedRelationship,
            Self::RecordImpactMap => DiscoveryModelPurpose::RecordImpactMap,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiscoveryTool {
    ListFiles,
    SearchText,
    ReadFile,
    ReadFiles,
    RelatedTests,
    RecordImpactMap,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "constraint",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum DiscoveryActionConstraints {
    Search {
        request: SearchRequest,
    },
    ExactPaths {
        paths: BTreeSet<DiscoveryPath>,
    },
    NamedRelationship {
        question: UnresolvedQuestion,
        paths: BTreeSet<DiscoveryPath>,
        targeted_search: Option<SearchRequest>,
    },
    ImpactMap {
        criterion_ids: BTreeSet<DiscoveryCriterionId>,
        evidence_ids: BTreeSet<EvidenceId>,
    },
}

impl DiscoveryActionConstraints {
    pub(crate) const fn action_class(&self) -> DiscoveryActionClass {
        match self {
            Self::Search { .. } => DiscoveryActionClass::DiscoverCandidates,
            Self::ExactPaths { .. } => DiscoveryActionClass::GroundCandidateEvidence,
            Self::NamedRelationship { .. } => DiscoveryActionClass::ResolveNamedRelationship,
            Self::ImpactMap { .. } => DiscoveryActionClass::RecordImpactMap,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolAuthorization {
    pub(crate) tool: DiscoveryTool,
    pub(crate) permitted_paths: BTreeSet<DiscoveryPath>,
    pub(crate) search_id: Option<SearchId>,
    pub(crate) relationship_question_id: Option<DiscoveryQuestionId>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    content = "tool",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ToolChoice {
    Required,
    Named(DiscoveryTool),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionEnvelope {
    pub(crate) schema_version: u16,
    pub(crate) action_id: ActionId,
    pub(crate) node_id: NodeId,
    pub(crate) action_class: DiscoveryActionClass,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) constraints: DiscoveryActionConstraints,
    pub(crate) allowed_tools: Vec<ToolAuthorization>,
    pub(crate) tool_choice: ToolChoice,
    pub(crate) input_token_ceiling: u32,
    pub(crate) output_token_allowance: u32,
    pub(crate) budget_owner: NodeId,
    pub(crate) reservation_id: ReservationId,
    pub(crate) payload_identity: String,
}

impl ActionEnvelope {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        action_id: ActionId,
        node_id: NodeId,
        repository_revision: RepositoryRevisionId,
        context: &ContextManifest,
        constraints: DiscoveryActionConstraints,
        input_token_ceiling: u32,
        output_token_allowance: u32,
        budget_owner: NodeId,
        reservation_id: ReservationId,
    ) -> Result<Self, DiscoveryContractError> {
        context.validate()?;
        if context.action_id != action_id
            || context.node_id != node_id
            || context.repository_revision != repository_revision
        {
            return Err(DiscoveryContractError::InvalidContext {
                code: "action_context_binding_mismatch",
            });
        }
        if budget_owner != node_id {
            return Err(DiscoveryContractError::InvalidAction {
                code: "discovery_budget_owner_mismatch",
            });
        }
        if input_token_ceiling == 0
            || input_token_ceiling > context.input_token_ceiling
            || context.estimated_input_tokens > input_token_ceiling
            || output_token_allowance == 0
        {
            return Err(DiscoveryContractError::InvalidAction {
                code: "action_token_allowance_invalid",
            });
        }
        let (action_class, allowed_tools, tool_choice) = authorize_tools(&constraints)?;
        match &constraints {
            DiscoveryActionConstraints::Search { request } => {
                let context_criteria = context
                    .mandatory_sections
                    .iter()
                    .filter_map(|section| match section {
                        ContextSection::AcceptanceCriterion { criterion_id } => {
                            Some(criterion_id.clone())
                        }
                        _ => None,
                    })
                    .collect::<BTreeSet<_>>();
                if request.repository_revision != repository_revision {
                    return Err(DiscoveryContractError::RepositoryRevisionMismatch);
                }
                if request.criterion_ids != context_criteria
                    || request.context_evidence_ids != context.evidence_ids
                {
                    return Err(DiscoveryContractError::InvalidAction {
                        code: "search_context_binding_mismatch",
                    });
                }
            }
            DiscoveryActionConstraints::NamedRelationship {
                question,
                paths,
                targeted_search,
            } => {
                let context_question_matches = context.mandatory_sections.iter().any(|section| {
                    matches!(
                        section,
                        ContextSection::UnresolvedRelationship { question_id }
                            if question_id == &question.id
                    )
                });
                if !context_question_matches
                    || !paths.contains(&question.subject_path)
                    || targeted_search.as_ref().is_some_and(|search| {
                        search.repository_revision != repository_revision
                            || !search.context_evidence_ids.is_subset(&context.evidence_ids)
                    })
                {
                    return Err(DiscoveryContractError::InvalidAction {
                        code: "relationship_action_binding_invalid",
                    });
                }
            }
            DiscoveryActionConstraints::ImpactMap { evidence_ids, .. } => {
                if evidence_ids.len() > MAX_CONTEXT_EVIDENCE
                    || !evidence_ids.is_subset(&context.evidence_ids)
                {
                    return Err(DiscoveryContractError::InvalidAction {
                        code: "impact_map_context_evidence_mismatch",
                    });
                }
            }
            DiscoveryActionConstraints::ExactPaths { .. } => {}
        }
        if context.purpose != action_class.purpose() {
            return Err(DiscoveryContractError::InvalidContext {
                code: "action_context_purpose_mismatch",
            });
        }
        let identity = serde_json::to_string(&(
            DISCOVERY_SCHEMA_VERSION,
            &action_id,
            &node_id,
            action_class,
            &repository_revision,
            &context.context_manifest_id,
            &constraints,
            &allowed_tools,
            &tool_choice,
            input_token_ceiling,
            output_token_allowance,
            &budget_owner,
            &reservation_id,
        ))
        .map_err(|_| DiscoveryContractError::Serialization)?;
        let payload_identity = stable_sha256(&[
            "execution-protocol-v1:discovery-provider-payload",
            &identity,
        ]);
        Ok(Self {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            action_id,
            node_id,
            action_class,
            repository_revision,
            context_manifest_id: context.context_manifest_id.clone(),
            constraints,
            allowed_tools,
            tool_choice,
            input_token_ceiling,
            output_token_allowance,
            budget_owner,
            reservation_id,
            payload_identity,
        })
    }

    pub(crate) fn validate_against_context(
        &self,
        context: &ContextManifest,
    ) -> Result<(), DiscoveryContractError> {
        if self.schema_version != DISCOVERY_SCHEMA_VERSION {
            return Err(DiscoveryContractError::InvalidAction {
                code: "action_schema_version_invalid",
            });
        }
        let rebuilt = Self::new(
            self.action_id.clone(),
            self.node_id.clone(),
            self.repository_revision.clone(),
            context,
            self.constraints.clone(),
            self.input_token_ceiling,
            self.output_token_allowance,
            self.budget_owner.clone(),
            self.reservation_id.clone(),
        )?;
        if rebuilt != *self {
            return Err(DiscoveryContractError::InvalidAction {
                code: "action_envelope_not_canonical",
            });
        }
        Ok(())
    }

    pub(crate) fn tool_names(&self) -> BTreeSet<DiscoveryTool> {
        self.allowed_tools
            .iter()
            .map(|authorization| authorization.tool)
            .collect()
    }
}

fn authorize_tools(
    constraints: &DiscoveryActionConstraints,
) -> Result<(DiscoveryActionClass, Vec<ToolAuthorization>, ToolChoice), DiscoveryContractError> {
    let authorized = match constraints {
        DiscoveryActionConstraints::Search { request } => {
            request.validate()?;
            let paths = request.scope.roots.clone();
            let search_id = Some(request.search_id.clone());
            (
                DiscoveryActionClass::DiscoverCandidates,
                vec![
                    ToolAuthorization {
                        tool: DiscoveryTool::ListFiles,
                        permitted_paths: paths.clone(),
                        search_id: search_id.clone(),
                        relationship_question_id: None,
                    },
                    ToolAuthorization {
                        tool: DiscoveryTool::SearchText,
                        permitted_paths: paths,
                        search_id,
                        relationship_question_id: None,
                    },
                ],
                ToolChoice::Required,
            )
        }
        DiscoveryActionConstraints::ExactPaths { paths } => {
            validate_action_paths(paths)?;
            let tool = if paths.len() == 1 {
                DiscoveryTool::ReadFile
            } else {
                DiscoveryTool::ReadFiles
            };
            (
                DiscoveryActionClass::GroundCandidateEvidence,
                vec![ToolAuthorization {
                    tool,
                    permitted_paths: paths.clone(),
                    search_id: None,
                    relationship_question_id: None,
                }],
                ToolChoice::Named(tool),
            )
        }
        DiscoveryActionConstraints::NamedRelationship {
            question,
            paths,
            targeted_search,
        } => {
            validate_action_paths(paths)?;
            if let Some(search) = targeted_search {
                search.validate()?;
            }
            let question_id = Some(question.id.clone());
            let mut tools = vec![ToolAuthorization {
                tool: DiscoveryTool::RelatedTests,
                permitted_paths: paths.clone(),
                search_id: None,
                relationship_question_id: question_id.clone(),
            }];
            if let Some(search) = targeted_search {
                tools.push(ToolAuthorization {
                    tool: DiscoveryTool::SearchText,
                    permitted_paths: search.scope.roots.clone(),
                    search_id: Some(search.search_id.clone()),
                    relationship_question_id: question_id.clone(),
                });
            }
            let read_tool = if paths.len() == 1 {
                DiscoveryTool::ReadFile
            } else {
                DiscoveryTool::ReadFiles
            };
            tools.push(ToolAuthorization {
                tool: read_tool,
                permitted_paths: paths.clone(),
                search_id: None,
                relationship_question_id: question_id,
            });
            (
                DiscoveryActionClass::ResolveNamedRelationship,
                tools,
                ToolChoice::Required,
            )
        }
        DiscoveryActionConstraints::ImpactMap {
            criterion_ids,
            evidence_ids,
        } => {
            if criterion_ids.is_empty() || evidence_ids.is_empty() {
                return Err(DiscoveryContractError::InvalidAction {
                    code: "impact_map_inputs_empty",
                });
            }
            let tool = DiscoveryTool::RecordImpactMap;
            (
                DiscoveryActionClass::RecordImpactMap,
                vec![ToolAuthorization {
                    tool,
                    permitted_paths: BTreeSet::new(),
                    search_id: None,
                    relationship_question_id: None,
                }],
                ToolChoice::Named(tool),
            )
        }
    };
    Ok(authorized)
}

fn validate_action_paths(paths: &BTreeSet<DiscoveryPath>) -> Result<(), DiscoveryContractError> {
    if paths.is_empty() {
        return Err(DiscoveryContractError::InvalidAction {
            code: "action_paths_empty",
        });
    }
    if paths.len() > MAX_ACTION_PATHS {
        return Err(DiscoveryContractError::LimitExceeded {
            field: "action.paths",
            limit: MAX_ACTION_PATHS,
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "effect",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum DiscoveryEffectRequest {
    DispatchProvider { envelope: Box<ActionEnvelope> },
    RecordConvergence { convergence: DiscoveryConvergence },
}

fn validate_hash(field: &'static str, value: &str) -> Result<(), DiscoveryContractError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DiscoveryContractError::InvalidHash { field });
    }
    Ok(())
}

#[cfg(test)]
mod context_builder_tests {
    use super::*;

    fn discovery_state() -> DiscoveryState {
        let goal = DiscoveryGoal::new(
            "a".repeat(64),
            BTreeSet::from([DiscoveryCriterionId::new("criterion-1").expect("criterion")]),
            ["repository context".to_owned()],
        )
        .expect("goal");
        DiscoveryState::new(
            NodeId::new("discovery-node"),
            RepositoryRevisionId::new("revision-1"),
            RepositoryProfileId::new("profile-1"),
            goal,
        )
    }

    fn search_constraints(state: &DiscoveryState) -> DiscoveryActionConstraints {
        DiscoveryActionConstraints::Search {
            request: SearchRequest::new(
                state.repository_revision.clone(),
                state.repository_profile_id.clone(),
                state.goal.criterion_ids.clone(),
                "repository context",
                SearchScope::repository(),
                Vec::<String>::new(),
                SearchMode::LiteralCaseInsensitive,
                BTreeSet::new(),
            )
            .expect("search constraints"),
        }
    }

    #[test]
    fn context_build_is_deterministic_and_identity_only() {
        let state = discovery_state();
        let constraints = search_constraints(&state);
        let first = build_discovery_context(&state, ActionId::new("action-1"), &constraints, 4_096)
            .expect("context");
        let second =
            build_discovery_context(&state, ActionId::new("action-1"), &constraints, 4_096)
                .expect("context");

        assert_eq!(first, second);
        assert!(first.estimated_input_tokens > 0);
        assert!(first.estimated_input_tokens <= first.input_token_ceiling);
        assert!(
            !serde_json::to_string(&first)
                .expect("serialize context")
                .contains("repository context")
        );
    }

    #[test]
    fn mandatory_context_too_large_is_reported_before_manifest_creation() {
        let state = discovery_state();
        let constraints = search_constraints(&state);
        let error = build_discovery_context(&state, ActionId::new("action-1"), &constraints, 1)
            .expect_err("mandatory context exceeds ceiling");

        assert!(matches!(
            error,
            ContextBuildError::MandatoryTooLarge {
                input_token_ceiling: 1,
                ..
            }
        ));
    }

    #[test]
    fn contract_error_source_is_preserved() {
        let mut state = discovery_state();
        let constraints = search_constraints(&state);
        state.schema_version = 99;
        let error = build_discovery_context(&state, ActionId::new("action-1"), &constraints, 4_096)
            .expect_err("invalid state");

        assert!(std::error::Error::source(&error).is_some());
    }
}
