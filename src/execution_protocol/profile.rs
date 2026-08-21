use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use super::{EvidenceId, RepositoryProfileId, RepositoryRevisionId, stable_sha256};

pub(crate) const REPOSITORY_PROFILE_SCHEMA_VERSION: u16 = 1;
const MAX_INVENTORY_FILES: usize = 20_000;
const MAX_CAPTURED_FILE_BYTES: usize = 128 * 1024;
const MAX_CAPTURED_TOTAL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RepositoryProfileError {
    InvalidPath(InvalidProfilePathReason),
    InvalidContentHash,
    InventoryFileLimitExceeded { observed: usize, maximum: usize },
    CapturedFileLimitExceeded { observed: usize, maximum: usize },
    CapturedTotalLimitExceeded { observed: usize, maximum: usize },
    ConflictingObservation { path: ProfilePath },
    IdentityEncoding,
    UnsupportedSchema { observed: u16, expected: u16 },
    NonCanonicalField { field: &'static str },
    InconsistentProfile { code: &'static str },
    ProfileIdentityMismatch,
}

impl fmt::Display for RepositoryProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(reason) => {
                write!(formatter, "invalid repository profile path: {reason}")
            }
            Self::InvalidContentHash => formatter.write_str("invalid repository content hash"),
            Self::InventoryFileLimitExceeded { observed, maximum } => write!(
                formatter,
                "repository inventory has {observed} files; the bounded maximum is {maximum}"
            ),
            Self::CapturedFileLimitExceeded { observed, maximum } => write!(
                formatter,
                "captured repository file has {observed} bytes; the bounded maximum is {maximum}"
            ),
            Self::CapturedTotalLimitExceeded { observed, maximum } => write!(
                formatter,
                "captured repository content has {observed} bytes; the bounded maximum is {maximum}"
            ),
            Self::ConflictingObservation { path } => {
                write!(
                    formatter,
                    "repository inventory contains conflicting observations for {path}"
                )
            }
            Self::IdentityEncoding => {
                formatter.write_str("repository profile identity could not be encoded")
            }
            Self::UnsupportedSchema { observed, expected } => write!(
                formatter,
                "repository profile schema {observed} is unsupported; expected {expected}"
            ),
            Self::NonCanonicalField { field } => {
                write!(
                    formatter,
                    "repository profile field `{field}` is not canonical"
                )
            }
            Self::InconsistentProfile { code } => {
                write!(formatter, "repository profile violates `{code}`")
            }
            Self::ProfileIdentityMismatch => {
                formatter.write_str("repository profile identity does not match its fields")
            }
        }
    }
}

impl std::error::Error for RepositoryProfileError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InvalidProfilePathReason {
    Empty,
    Absolute,
    NotNormalized,
    ParentTraversal,
    ControlCharacter,
}

impl fmt::Display for InvalidProfilePathReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "path is empty",
            Self::Absolute => "path is absolute",
            Self::NotNormalized => "path is not normalized",
            Self::ParentTraversal => "path traverses a parent directory",
            Self::ControlCharacter => "path contains a control character",
        })
    }
}

#[derive(Clone, Debug, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct ProfilePath(String);

impl ProfilePath {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, RepositoryProfileError> {
        let value = value.into();
        validate_profile_path(&value)?;
        Ok(Self(value))
    }

    pub(crate) fn root() -> Self {
        Self(".".into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn is_root(&self) -> bool {
        self.0 == "."
    }

    pub(crate) fn parent(&self) -> Self {
        self.0
            .rsplit_once('/')
            .map_or_else(Self::root, |(parent, _)| Self(parent.into()))
    }
}

impl fmt::Display for ProfilePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProfilePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

fn validate_profile_path(value: &str) -> Result<(), RepositoryProfileError> {
    if value.is_empty() {
        return Err(RepositoryProfileError::InvalidPath(
            InvalidProfilePathReason::Empty,
        ));
    }
    if value.starts_with('/')
        || value.starts_with('\\')
        || value
            .as_bytes()
            .get(1)
            .is_some_and(|character| *character == b':')
    {
        return Err(RepositoryProfileError::InvalidPath(
            InvalidProfilePathReason::Absolute,
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(RepositoryProfileError::InvalidPath(
            InvalidProfilePathReason::ControlCharacter,
        ));
    }
    if value.contains('\\') || value.ends_with('/') || value.contains("//") {
        return Err(RepositoryProfileError::InvalidPath(
            InvalidProfilePathReason::NotNormalized,
        ));
    }
    if value == "." {
        return Ok(());
    }
    for component in value.split('/') {
        match component {
            ".." => {
                return Err(RepositoryProfileError::InvalidPath(
                    InvalidProfilePathReason::ParentTraversal,
                ));
            }
            "" | "." => {
                return Err(RepositoryProfileError::InvalidPath(
                    InvalidProfilePathReason::NotNormalized,
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct ObservedContentHash(String);

impl ObservedContentHash {
    fn from_content(content: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(content)))
    }

    pub(crate) fn new(value: impl Into<String>) -> Result<Self, RepositoryProfileError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|character| character.is_ascii_digit() || (b'a'..=b'f').contains(&character))
        {
            return Err(RepositoryProfileError::InvalidContentHash);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ObservedContentHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone)]
struct CapturedContent(Vec<u8>);

impl fmt::Debug for CapturedContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedContent")
            .field("byte_len", &self.0.len())
            .field("content", &"<redacted>")
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct RepositoryFileObservation {
    path: ProfilePath,
    byte_len: u64,
    content_hash: ObservedContentHash,
    captured_content: Option<CapturedContent>,
}

impl fmt::Debug for RepositoryFileObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RepositoryFileObservation")
            .field("path", &self.path)
            .field("byte_len", &self.byte_len)
            .field("content_hash", &self.content_hash)
            .field(
                "content",
                &self.captured_content.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl RepositoryFileObservation {
    pub(crate) fn from_bytes(
        path: impl Into<String>,
        content: impl AsRef<[u8]>,
    ) -> Result<Self, RepositoryProfileError> {
        let path = ProfilePath::new(path)?;
        if path.is_root() {
            return Err(RepositoryProfileError::InvalidPath(
                InvalidProfilePathReason::NotNormalized,
            ));
        }
        let content = content.as_ref();
        if content.len() > MAX_CAPTURED_FILE_BYTES {
            return Err(RepositoryProfileError::CapturedFileLimitExceeded {
                observed: content.len(),
                maximum: MAX_CAPTURED_FILE_BYTES,
            });
        }
        Ok(Self {
            path,
            byte_len: u64::try_from(content.len()).expect("bounded content length fits u64"),
            content_hash: ObservedContentHash::from_content(content),
            captured_content: Some(CapturedContent(content.to_vec())),
        })
    }

    pub(crate) fn opaque(
        path: impl Into<String>,
        byte_len: u64,
        content_hash: ObservedContentHash,
    ) -> Result<Self, RepositoryProfileError> {
        let path = ProfilePath::new(path)?;
        if path.is_root() {
            return Err(RepositoryProfileError::InvalidPath(
                InvalidProfilePathReason::NotNormalized,
            ));
        }
        Ok(Self {
            path,
            byte_len,
            content_hash,
            captured_content: None,
        })
    }

    pub(crate) fn path(&self) -> &ProfilePath {
        &self.path
    }
}

#[derive(Clone)]
pub(crate) struct RepositoryInventory {
    repository_revision: RepositoryRevisionId,
    files: Vec<RepositoryFileObservation>,
    total_bytes: u64,
    captured_bytes: usize,
}

impl fmt::Debug for RepositoryInventory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RepositoryInventory")
            .field("repository_revision", &self.repository_revision)
            .field("file_count", &self.files.len())
            .field("total_bytes", &self.total_bytes)
            .field("captured_bytes", &self.captured_bytes)
            .finish()
    }
}

impl RepositoryInventory {
    pub(crate) fn new(
        repository_revision: RepositoryRevisionId,
        files: Vec<RepositoryFileObservation>,
    ) -> Result<Self, RepositoryProfileError> {
        if files.len() > MAX_INVENTORY_FILES {
            return Err(RepositoryProfileError::InventoryFileLimitExceeded {
                observed: files.len(),
                maximum: MAX_INVENTORY_FILES,
            });
        }

        let mut canonical = BTreeMap::<ProfilePath, RepositoryFileObservation>::new();
        for observation in files {
            match canonical.get_mut(&observation.path) {
                Some(existing)
                    if existing.content_hash != observation.content_hash
                        || existing.byte_len != observation.byte_len =>
                {
                    return Err(RepositoryProfileError::ConflictingObservation {
                        path: observation.path,
                    });
                }
                Some(existing)
                    if existing.captured_content.is_none()
                        && observation.captured_content.is_some() =>
                {
                    *existing = observation;
                }
                Some(_) => {}
                None => {
                    canonical.insert(observation.path.clone(), observation);
                }
            }
        }

        let files = canonical.into_values().collect::<Vec<_>>();
        let captured_bytes = files
            .iter()
            .filter_map(|file| file.captured_content.as_ref())
            .try_fold(0_usize, |total, content| total.checked_add(content.0.len()))
            .ok_or(RepositoryProfileError::CapturedTotalLimitExceeded {
                observed: usize::MAX,
                maximum: MAX_CAPTURED_TOTAL_BYTES,
            })?;
        if captured_bytes > MAX_CAPTURED_TOTAL_BYTES {
            return Err(RepositoryProfileError::CapturedTotalLimitExceeded {
                observed: captured_bytes,
                maximum: MAX_CAPTURED_TOTAL_BYTES,
            });
        }
        let total_bytes = files
            .iter()
            .fold(0_u64, |total, file| total.saturating_add(file.byte_len));

        Ok(Self {
            repository_revision,
            files,
            total_bytes,
            captured_bytes,
        })
    }

    pub(crate) fn repository_revision(&self) -> &RepositoryRevisionId {
        &self.repository_revision
    }

    pub(crate) fn files(&self) -> &[RepositoryFileObservation] {
        &self.files
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EcosystemKind {
    Rust,
    Node,
    Python,
    Go,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapabilityKind {
    CargoProject,
    PackageScripts,
    PythonProject,
    GoModule,
    GenericTextStructure,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EcosystemCapability {
    pub(crate) ecosystem: EcosystemKind,
    pub(crate) capability: CapabilityKind,
    pub(crate) evidence_id: EvidenceId,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MetadataKind {
    CargoManifest,
    CargoLock,
    PackageManifest,
    NpmLock,
    PnpmLock,
    YarnLock,
    PythonProject,
    PoetryLock,
    UvLock,
    GoModule,
    GoChecksum,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MetadataStatus {
    Parsed,
    Observed,
    Malformed,
    ContentUnavailable,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MetadataObservation {
    pub(crate) path: ProfilePath,
    pub(crate) kind: MetadataKind,
    pub(crate) status: MetadataStatus,
    pub(crate) evidence_id: EvidenceId,
    pub(crate) content_hash: ObservedContentHash,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GeneratedPathDisposition {
    OrdinarySource,
    ReadOnlyGeneratedOutput,
    RegenerateThroughAuthorizedCommand,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GeneratedMarkerKind {
    AtGenerated,
    CodeGeneratedDoNotEdit,
    GeneratedFileDoNotEdit,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum GeneratedRuleProvenance {
    FileMarker {
        evidence_id: EvidenceId,
        marker: GeneratedMarkerKind,
        content_hash: ObservedContentHash,
    },
    GeneratorConfiguration {
        evidence_id: EvidenceId,
    },
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GeneratedPathRule {
    pub(crate) path: ProfilePath,
    pub(crate) disposition: GeneratedPathDisposition,
    pub(crate) provenance: GeneratedRuleProvenance,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationClass {
    TestSuite,
    Build,
    Typecheck,
    Lint,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationCommandKind {
    CargoTest,
    CargoBuild,
    NpmTest,
    NpmBuild,
    NpmTypecheck,
    NpmLint,
    PythonPytest,
    PythonBuild,
    GoTestAll,
    GoBuildAll,
}

impl ValidationCommandKind {
    pub(crate) fn argv(self) -> &'static [&'static str] {
        match self {
            Self::CargoTest => &["cargo", "test"],
            Self::CargoBuild => &["cargo", "build"],
            Self::NpmTest => &["npm", "run", "test"],
            Self::NpmBuild => &["npm", "run", "build"],
            Self::NpmTypecheck => &["npm", "run", "typecheck"],
            Self::NpmLint => &["npm", "run", "lint"],
            Self::PythonPytest => &["python", "-m", "pytest"],
            Self::PythonBuild => &["python", "-m", "build"],
            Self::GoTestAll => &["go", "test", "./..."],
            Self::GoBuildAll => &["go", "build", "./..."],
        }
    }

    const fn identity_key(self) -> &'static str {
        match self {
            Self::CargoTest => "cargo_test",
            Self::CargoBuild => "cargo_build",
            Self::NpmTest => "npm_test",
            Self::NpmBuild => "npm_build",
            Self::NpmTypecheck => "npm_typecheck",
            Self::NpmLint => "npm_lint",
            Self::PythonPytest => "python_pytest",
            Self::PythonBuild => "python_build",
            Self::GoTestAll => "go_test_all",
            Self::GoBuildAll => "go_build_all",
        }
    }

    const fn validation_class(self) -> ValidationClass {
        match self {
            Self::CargoTest | Self::NpmTest | Self::PythonPytest | Self::GoTestAll => {
                ValidationClass::TestSuite
            }
            Self::CargoBuild | Self::NpmBuild | Self::PythonBuild | Self::GoBuildAll => {
                ValidationClass::Build
            }
            Self::NpmTypecheck => ValidationClass::Typecheck,
            Self::NpmLint => ValidationClass::Lint,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CommandProvenance {
    SignedExecutionPolicy { evidence_id: EvidenceId },
    ParsedProjectMetadata { evidence_id: EvidenceId },
    ParsedCiConfiguration { evidence_id: EvidenceId },
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommandAuthority {
    CandidateOnly,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationCommandCandidate {
    pub(crate) candidate_id: EvidenceId,
    pub(crate) command: ValidationCommandKind,
    pub(crate) class: ValidationClass,
    pub(crate) working_directory: ProfilePath,
    pub(crate) provenance: CommandProvenance,
    pub(crate) authority: CommandAuthority,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepositorySizeClass {
    Tiny,
    Small,
    Medium,
    Large,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileSizePolicy {
    pub(crate) profile_capture_limit_bytes: u64,
    pub(crate) discovery_full_read_limit_bytes: u64,
    pub(crate) discovery_range_limit_bytes: u64,
}

impl Default for FileSizePolicy {
    fn default() -> Self {
        Self {
            profile_capture_limit_bytes: MAX_CAPTURED_FILE_BYTES as u64,
            discovery_full_read_limit_bytes: 64 * 1024,
            discovery_range_limit_bytes: 16 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProfileUncertaintyKind {
    NoKnownEcosystem,
    MalformedMetadata,
    MetadataContentUnavailable,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileUncertainty {
    pub(crate) kind: ProfileUncertaintyKind,
    pub(crate) path: Option<ProfilePath>,
    pub(crate) evidence_id: Option<EvidenceId>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryProfile {
    pub(crate) schema_version: u16,
    pub(crate) profile_id: RepositoryProfileId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) inventory_fingerprint: ObservedContentHash,
    pub(crate) ecosystems: Vec<EcosystemCapability>,
    pub(crate) source_roots: Vec<ProfilePath>,
    pub(crate) test_roots: Vec<ProfilePath>,
    pub(crate) metadata_files: Vec<MetadataObservation>,
    pub(crate) dependency_files: Vec<ProfilePath>,
    pub(crate) generated_rules: Vec<GeneratedPathRule>,
    pub(crate) validation_candidates: Vec<ValidationCommandCandidate>,
    pub(crate) repository_size: RepositorySizeClass,
    pub(crate) text_file_limits: FileSizePolicy,
    pub(crate) uncertainties: Vec<ProfileUncertainty>,
}

impl RepositoryProfile {
    pub(crate) fn generated_disposition(&self, path: &ProfilePath) -> GeneratedPathDisposition {
        self.generated_rules
            .iter()
            .find(|rule| &rule.path == path)
            .map_or(GeneratedPathDisposition::OrdinarySource, |rule| {
                rule.disposition
            })
    }

    pub(crate) fn has_executable_command_authority(&self) -> bool {
        self.validation_candidates
            .iter()
            .any(|candidate| candidate.authority != CommandAuthority::CandidateOnly)
    }

    pub(crate) fn validate(&self) -> Result<(), RepositoryProfileError> {
        if self.schema_version != REPOSITORY_PROFILE_SCHEMA_VERSION {
            return Err(RepositoryProfileError::UnsupportedSchema {
                observed: self.schema_version,
                expected: REPOSITORY_PROFILE_SCHEMA_VERSION,
            });
        }
        for (name, canonical) in [
            ("ecosystems", is_strictly_sorted(&self.ecosystems)),
            ("source_roots", is_strictly_sorted(&self.source_roots)),
            ("test_roots", is_strictly_sorted(&self.test_roots)),
            ("metadata_files", is_strictly_sorted(&self.metadata_files)),
            (
                "dependency_files",
                is_strictly_sorted(&self.dependency_files),
            ),
            ("generated_rules", is_strictly_sorted(&self.generated_rules)),
            (
                "validation_candidates",
                is_strictly_sorted(&self.validation_candidates),
            ),
            ("uncertainties", is_strictly_sorted(&self.uncertainties)),
        ] {
            if !canonical {
                return Err(RepositoryProfileError::NonCanonicalField { field: name });
            }
        }

        for path in self
            .source_roots
            .iter()
            .chain(&self.test_roots)
            .chain(&self.dependency_files)
        {
            validate_profile_path(path.as_str())?;
        }

        for metadata in &self.metadata_files {
            validate_profile_path(metadata.path.as_str())?;
            if metadata.path.is_root() || metadata_kind(&metadata.path) != Some(metadata.kind) {
                return Err(RepositoryProfileError::InconsistentProfile {
                    code: "metadata_kind_path_mismatch",
                });
            }
            if metadata.evidence_id
                != evidence_id(
                    &self.repository_revision,
                    "metadata",
                    &metadata.path,
                    &metadata.content_hash,
                )
            {
                return Err(RepositoryProfileError::InconsistentProfile {
                    code: "metadata_evidence_identity_mismatch",
                });
            }
            let status_is_valid = match metadata.kind {
                MetadataKind::CargoManifest
                | MetadataKind::PackageManifest
                | MetadataKind::PythonProject
                | MetadataKind::GoModule => matches!(
                    metadata.status,
                    MetadataStatus::Parsed
                        | MetadataStatus::Malformed
                        | MetadataStatus::ContentUnavailable
                ),
                MetadataKind::CargoLock
                | MetadataKind::NpmLock
                | MetadataKind::PnpmLock
                | MetadataKind::YarnLock
                | MetadataKind::PoetryLock
                | MetadataKind::UvLock
                | MetadataKind::GoChecksum => matches!(
                    metadata.status,
                    MetadataStatus::Observed | MetadataStatus::ContentUnavailable
                ),
            };
            if !status_is_valid {
                return Err(RepositoryProfileError::InconsistentProfile {
                    code: "metadata_status_kind_mismatch",
                });
            }
        }

        let expected_dependencies = self
            .metadata_files
            .iter()
            .map(|metadata| metadata.path.clone())
            .collect::<Vec<_>>();
        if self.dependency_files != expected_dependencies {
            return Err(RepositoryProfileError::InconsistentProfile {
                code: "dependency_file_evidence_mismatch",
            });
        }

        self.validate_ecosystems()?;
        self.validate_generated_rules()?;
        self.validate_command_candidates()?;
        self.validate_uncertainties()?;

        if self.text_file_limits != FileSizePolicy::default() {
            return Err(RepositoryProfileError::InconsistentProfile {
                code: "file_size_policy_mismatch",
            });
        }
        let expected_id = derive_profile_id(&ProfileIdentityMaterial::from_profile(self))?;
        if self.profile_id != expected_id {
            return Err(RepositoryProfileError::ProfileIdentityMismatch);
        }
        Ok(())
    }

    fn validate_ecosystems(&self) -> Result<(), RepositoryProfileError> {
        let mut expected = self
            .metadata_files
            .iter()
            .filter_map(|metadata| {
                metadata_capability(metadata.kind).map(|(ecosystem, capability)| {
                    EcosystemCapability {
                        ecosystem,
                        capability,
                        evidence_id: metadata.evidence_id.clone(),
                    }
                })
            })
            .collect::<Vec<_>>();
        if expected.is_empty() {
            let evidence_id = EvidenceId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "repository-profile:generic-structure",
                    self.repository_revision.as_str(),
                    self.inventory_fingerprint.as_str(),
                ])
            ));
            expected.push(EcosystemCapability {
                ecosystem: EcosystemKind::Unknown,
                capability: CapabilityKind::GenericTextStructure,
                evidence_id,
            });
        }
        expected.sort();
        expected.dedup();
        if self.ecosystems != expected {
            return Err(RepositoryProfileError::InconsistentProfile {
                code: "ecosystem_metadata_mismatch",
            });
        }
        Ok(())
    }

    fn validate_generated_rules(&self) -> Result<(), RepositoryProfileError> {
        for rule in &self.generated_rules {
            validate_profile_path(rule.path.as_str())?;
            if rule.path.is_root() {
                return Err(RepositoryProfileError::InconsistentProfile {
                    code: "generated_rule_targets_root",
                });
            }
            match (&rule.provenance, rule.disposition) {
                (
                    GeneratedRuleProvenance::FileMarker {
                        evidence_id: recorded_evidence_id,
                        content_hash,
                        ..
                    },
                    GeneratedPathDisposition::ReadOnlyGeneratedOutput,
                ) if *recorded_evidence_id
                    == evidence_id(
                        &self.repository_revision,
                        "generated-marker",
                        &rule.path,
                        content_hash,
                    ) => {}
                (GeneratedRuleProvenance::GeneratorConfiguration { .. }, _) => {
                    return Err(RepositoryProfileError::InconsistentProfile {
                        code: "generator_configuration_provenance_not_implemented",
                    });
                }
                _ => {
                    return Err(RepositoryProfileError::InconsistentProfile {
                        code: "generated_rule_provenance_mismatch",
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_command_candidates(&self) -> Result<(), RepositoryProfileError> {
        for candidate in &self.validation_candidates {
            validate_profile_path(candidate.working_directory.as_str())?;
            if candidate.authority != CommandAuthority::CandidateOnly
                || candidate.class != candidate.command.validation_class()
            {
                return Err(RepositoryProfileError::InconsistentProfile {
                    code: "validation_candidate_authority_or_class_mismatch",
                });
            }
            let CommandProvenance::ParsedProjectMetadata { evidence_id } = &candidate.provenance
            else {
                return Err(RepositoryProfileError::InconsistentProfile {
                    code: "validation_candidate_provenance_mismatch",
                });
            };
            let Some(metadata) = self.metadata_files.iter().find(|metadata| {
                metadata.evidence_id == *evidence_id && metadata.status == MetadataStatus::Parsed
            }) else {
                return Err(RepositoryProfileError::InconsistentProfile {
                    code: "validation_candidate_metadata_missing",
                });
            };
            if candidate.working_directory != metadata.path.parent()
                || !command_matches_metadata(candidate.command, metadata.kind)
            {
                return Err(RepositoryProfileError::InconsistentProfile {
                    code: "validation_candidate_metadata_mismatch",
                });
            }
            if candidate.candidate_id
                != command_candidate_id(
                    evidence_id,
                    candidate.command,
                    &candidate.working_directory,
                )
            {
                return Err(RepositoryProfileError::InconsistentProfile {
                    code: "validation_candidate_identity_mismatch",
                });
            }
        }
        Ok(())
    }

    fn validate_uncertainties(&self) -> Result<(), RepositoryProfileError> {
        let mut expected = self
            .metadata_files
            .iter()
            .filter_map(|metadata| match metadata.status {
                MetadataStatus::Malformed => Some(ProfileUncertainty {
                    kind: ProfileUncertaintyKind::MalformedMetadata,
                    path: Some(metadata.path.clone()),
                    evidence_id: Some(metadata.evidence_id.clone()),
                }),
                MetadataStatus::ContentUnavailable => Some(ProfileUncertainty {
                    kind: ProfileUncertaintyKind::MetadataContentUnavailable,
                    path: Some(metadata.path.clone()),
                    evidence_id: Some(metadata.evidence_id.clone()),
                }),
                MetadataStatus::Parsed | MetadataStatus::Observed => None,
            })
            .collect::<Vec<_>>();
        if self
            .ecosystems
            .iter()
            .any(|capability| capability.ecosystem == EcosystemKind::Unknown)
        {
            expected.push(ProfileUncertainty {
                kind: ProfileUncertaintyKind::NoKnownEcosystem,
                path: None,
                evidence_id: None,
            });
        }
        expected.sort();
        expected.dedup();
        if self.uncertainties != expected {
            return Err(RepositoryProfileError::InconsistentProfile {
                code: "profile_uncertainty_mismatch",
            });
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ProfileIdentityMaterial<'a> {
    schema_version: u16,
    repository_revision: &'a RepositoryRevisionId,
    inventory_fingerprint: &'a ObservedContentHash,
    ecosystems: &'a [EcosystemCapability],
    source_roots: &'a [ProfilePath],
    test_roots: &'a [ProfilePath],
    metadata_files: &'a [MetadataObservation],
    dependency_files: &'a [ProfilePath],
    generated_rules: &'a [GeneratedPathRule],
    validation_candidates: &'a [ValidationCommandCandidate],
    repository_size: RepositorySizeClass,
    text_file_limits: &'a FileSizePolicy,
    uncertainties: &'a [ProfileUncertainty],
}

impl<'a> ProfileIdentityMaterial<'a> {
    fn from_profile(profile: &'a RepositoryProfile) -> Self {
        Self {
            schema_version: profile.schema_version,
            repository_revision: &profile.repository_revision,
            inventory_fingerprint: &profile.inventory_fingerprint,
            ecosystems: &profile.ecosystems,
            source_roots: &profile.source_roots,
            test_roots: &profile.test_roots,
            metadata_files: &profile.metadata_files,
            dependency_files: &profile.dependency_files,
            generated_rules: &profile.generated_rules,
            validation_candidates: &profile.validation_candidates,
            repository_size: profile.repository_size,
            text_file_limits: &profile.text_file_limits,
            uncertainties: &profile.uncertainties,
        }
    }
}

fn derive_profile_id(
    material: &ProfileIdentityMaterial<'_>,
) -> Result<RepositoryProfileId, RepositoryProfileError> {
    let canonical =
        serde_json::to_string(material).map_err(|_| RepositoryProfileError::IdentityEncoding)?;
    Ok(RepositoryProfileId::new(format!(
        "epv1:{}",
        stable_sha256(&["execution-protocol-v1:repository-profile", &canonical])
    )))
}

fn is_strictly_sorted<T: Ord>(items: &[T]) -> bool {
    items.windows(2).all(|pair| pair[0] < pair[1])
}

pub(crate) fn build_repository_profile(
    inventory: &RepositoryInventory,
) -> Result<RepositoryProfile, RepositoryProfileError> {
    let inventory_fingerprint = inventory_fingerprint(inventory);
    let mut metadata_files = Vec::new();
    let mut ecosystems = Vec::new();
    let mut dependency_files = Vec::new();
    let mut generated_rules = Vec::new();
    let mut validation_candidates = Vec::new();
    let mut uncertainties = Vec::new();

    for file in &inventory.files {
        if let Some(kind) = metadata_kind(&file.path) {
            let evidence_id = evidence_id(
                &inventory.repository_revision,
                "metadata",
                &file.path,
                &file.content_hash,
            );
            let status = metadata_status(kind, file.captured_content.as_ref());
            metadata_files.push(MetadataObservation {
                path: file.path.clone(),
                kind,
                status,
                evidence_id: evidence_id.clone(),
                content_hash: file.content_hash.clone(),
            });
            dependency_files.push(file.path.clone());
            if let Some((ecosystem, capability)) = metadata_capability(kind) {
                ecosystems.push(EcosystemCapability {
                    ecosystem,
                    capability,
                    evidence_id: evidence_id.clone(),
                });
            }
            match status {
                MetadataStatus::Malformed => uncertainties.push(ProfileUncertainty {
                    kind: ProfileUncertaintyKind::MalformedMetadata,
                    path: Some(file.path.clone()),
                    evidence_id: Some(evidence_id.clone()),
                }),
                MetadataStatus::ContentUnavailable => uncertainties.push(ProfileUncertainty {
                    kind: ProfileUncertaintyKind::MetadataContentUnavailable,
                    path: Some(file.path.clone()),
                    evidence_id: Some(evidence_id.clone()),
                }),
                MetadataStatus::Parsed | MetadataStatus::Observed => {}
            }
            if status == MetadataStatus::Parsed {
                validation_candidates.extend(command_candidates(kind, file, &evidence_id));
            }
        }

        if let Some(content) = file.captured_content.as_ref()
            && let Some(marker) = generated_marker(&content.0)
        {
            generated_rules.push(GeneratedPathRule {
                path: file.path.clone(),
                disposition: GeneratedPathDisposition::ReadOnlyGeneratedOutput,
                provenance: GeneratedRuleProvenance::FileMarker {
                    evidence_id: evidence_id(
                        &inventory.repository_revision,
                        "generated-marker",
                        &file.path,
                        &file.content_hash,
                    ),
                    marker,
                    content_hash: file.content_hash.clone(),
                },
            });
        }
    }

    if ecosystems.is_empty() {
        ecosystems.push(EcosystemCapability {
            ecosystem: EcosystemKind::Unknown,
            capability: CapabilityKind::GenericTextStructure,
            evidence_id: EvidenceId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "repository-profile:generic-structure",
                    inventory.repository_revision.as_str(),
                    inventory_fingerprint.as_str(),
                ])
            )),
        });
        uncertainties.push(ProfileUncertainty {
            kind: ProfileUncertaintyKind::NoKnownEcosystem,
            path: None,
            evidence_id: None,
        });
    }

    ecosystems.sort();
    ecosystems.dedup();
    metadata_files.sort();
    metadata_files.dedup();
    dependency_files.sort();
    dependency_files.dedup();
    generated_rules.sort();
    generated_rules.dedup();
    validation_candidates.sort();
    validation_candidates.dedup();
    uncertainties.sort();
    uncertainties.dedup();

    let ecosystem_kinds = ecosystems
        .iter()
        .map(|capability| capability.ecosystem)
        .collect::<Vec<_>>();
    let (source_roots, test_roots) = classify_roots(&inventory.files, &ecosystem_kinds);
    let repository_size = match inventory.files.len() {
        0..=20 => RepositorySizeClass::Tiny,
        21..=200 => RepositorySizeClass::Small,
        201..=2_000 => RepositorySizeClass::Medium,
        _ => RepositorySizeClass::Large,
    };
    let text_file_limits = FileSizePolicy::default();
    let material = ProfileIdentityMaterial {
        schema_version: REPOSITORY_PROFILE_SCHEMA_VERSION,
        repository_revision: &inventory.repository_revision,
        inventory_fingerprint: &inventory_fingerprint,
        ecosystems: &ecosystems,
        source_roots: &source_roots,
        test_roots: &test_roots,
        metadata_files: &metadata_files,
        dependency_files: &dependency_files,
        generated_rules: &generated_rules,
        validation_candidates: &validation_candidates,
        repository_size,
        text_file_limits: &text_file_limits,
        uncertainties: &uncertainties,
    };
    let profile_id = derive_profile_id(&material)?;

    Ok(RepositoryProfile {
        schema_version: REPOSITORY_PROFILE_SCHEMA_VERSION,
        profile_id,
        repository_revision: inventory.repository_revision.clone(),
        inventory_fingerprint,
        ecosystems,
        source_roots,
        test_roots,
        metadata_files,
        dependency_files,
        generated_rules,
        validation_candidates,
        repository_size,
        text_file_limits,
        uncertainties,
    })
}

fn inventory_fingerprint(inventory: &RepositoryInventory) -> ObservedContentHash {
    let canonical = inventory
        .files
        .iter()
        .map(|file| {
            format!(
                "{}\u{0}{}\u{0}{}",
                file.path,
                file.byte_len,
                file.content_hash.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join("\u{1f}");
    ObservedContentHash(stable_sha256(&[
        "execution-protocol-v1:repository-inventory",
        inventory.repository_revision.as_str(),
        &canonical,
    ]))
}

fn evidence_id(
    revision: &RepositoryRevisionId,
    kind: &str,
    path: &ProfilePath,
    content_hash: &ObservedContentHash,
) -> EvidenceId {
    EvidenceId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:profile-evidence",
            revision.as_str(),
            kind,
            path.as_str(),
            content_hash.as_str(),
        ])
    ))
}

fn metadata_kind(path: &ProfilePath) -> Option<MetadataKind> {
    let name = path.as_str().rsplit('/').next()?;
    match name {
        "Cargo.toml" => Some(MetadataKind::CargoManifest),
        "Cargo.lock" => Some(MetadataKind::CargoLock),
        "package.json" => Some(MetadataKind::PackageManifest),
        "package-lock.json" => Some(MetadataKind::NpmLock),
        "pnpm-lock.yaml" => Some(MetadataKind::PnpmLock),
        "yarn.lock" => Some(MetadataKind::YarnLock),
        "pyproject.toml" => Some(MetadataKind::PythonProject),
        "poetry.lock" => Some(MetadataKind::PoetryLock),
        "uv.lock" => Some(MetadataKind::UvLock),
        "go.mod" => Some(MetadataKind::GoModule),
        "go.sum" => Some(MetadataKind::GoChecksum),
        _ => None,
    }
}

fn metadata_capability(kind: MetadataKind) -> Option<(EcosystemKind, CapabilityKind)> {
    match kind {
        MetadataKind::CargoManifest => Some((EcosystemKind::Rust, CapabilityKind::CargoProject)),
        MetadataKind::PackageManifest => {
            Some((EcosystemKind::Node, CapabilityKind::PackageScripts))
        }
        MetadataKind::PythonProject => Some((EcosystemKind::Python, CapabilityKind::PythonProject)),
        MetadataKind::GoModule => Some((EcosystemKind::Go, CapabilityKind::GoModule)),
        MetadataKind::CargoLock
        | MetadataKind::NpmLock
        | MetadataKind::PnpmLock
        | MetadataKind::YarnLock
        | MetadataKind::PoetryLock
        | MetadataKind::UvLock
        | MetadataKind::GoChecksum => None,
    }
}

fn metadata_status(kind: MetadataKind, content: Option<&CapturedContent>) -> MetadataStatus {
    let Some(content) = content else {
        return MetadataStatus::ContentUnavailable;
    };
    let Ok(text) = std::str::from_utf8(&content.0) else {
        return MetadataStatus::Malformed;
    };
    match kind {
        MetadataKind::CargoManifest => bool_status(
            text.lines()
                .map(str::trim)
                .any(|line| matches!(line, "[package]" | "[workspace]")),
        ),
        MetadataKind::PackageManifest => {
            bool_status(serde_json::from_str::<PackageManifest>(text).is_ok())
        }
        MetadataKind::PythonProject => bool_status(text.lines().map(str::trim).any(|line| {
            line == "[project]" || line == "[build-system]" || line.starts_with("[tool.")
        })),
        MetadataKind::GoModule => bool_status(
            text.lines()
                .map(str::trim)
                .any(|line| line.starts_with("module ") && line.len() > "module ".len()),
        ),
        MetadataKind::CargoLock
        | MetadataKind::NpmLock
        | MetadataKind::PnpmLock
        | MetadataKind::YarnLock
        | MetadataKind::PoetryLock
        | MetadataKind::UvLock
        | MetadataKind::GoChecksum => MetadataStatus::Observed,
    }
}

fn bool_status(parsed: bool) -> MetadataStatus {
    if parsed {
        MetadataStatus::Parsed
    } else {
        MetadataStatus::Malformed
    }
}

#[derive(Deserialize)]
struct PackageManifest {
    #[serde(default)]
    scripts: BTreeMap<String, String>,
}

fn command_candidates(
    kind: MetadataKind,
    file: &RepositoryFileObservation,
    evidence_id: &EvidenceId,
) -> Vec<ValidationCommandCandidate> {
    let working_directory = file.path.parent();
    let mut commands = Vec::new();
    match kind {
        MetadataKind::CargoManifest => {
            commands.push((ValidationCommandKind::CargoTest, ValidationClass::TestSuite));
            commands.push((ValidationCommandKind::CargoBuild, ValidationClass::Build));
        }
        MetadataKind::PackageManifest => {
            let scripts = file
                .captured_content
                .as_ref()
                .and_then(|content| std::str::from_utf8(&content.0).ok())
                .and_then(|text| serde_json::from_str::<PackageManifest>(text).ok())
                .map_or_else(BTreeMap::new, |manifest| manifest.scripts);
            for (script, command, class) in [
                (
                    "test",
                    ValidationCommandKind::NpmTest,
                    ValidationClass::TestSuite,
                ),
                (
                    "build",
                    ValidationCommandKind::NpmBuild,
                    ValidationClass::Build,
                ),
                (
                    "typecheck",
                    ValidationCommandKind::NpmTypecheck,
                    ValidationClass::Typecheck,
                ),
                (
                    "lint",
                    ValidationCommandKind::NpmLint,
                    ValidationClass::Lint,
                ),
            ] {
                if scripts.contains_key(script) {
                    commands.push((command, class));
                }
            }
        }
        MetadataKind::PythonProject => {
            let text = file
                .captured_content
                .as_ref()
                .and_then(|content| std::str::from_utf8(&content.0).ok())
                .unwrap_or_default();
            if text
                .lines()
                .map(str::trim)
                .any(|line| line == "[tool.pytest]" || line.starts_with("[tool.pytest."))
            {
                commands.push((
                    ValidationCommandKind::PythonPytest,
                    ValidationClass::TestSuite,
                ));
            }
            if text
                .lines()
                .map(str::trim)
                .any(|line| line == "[build-system]")
            {
                commands.push((ValidationCommandKind::PythonBuild, ValidationClass::Build));
            }
        }
        MetadataKind::GoModule => {
            commands.push((ValidationCommandKind::GoTestAll, ValidationClass::TestSuite));
            commands.push((ValidationCommandKind::GoBuildAll, ValidationClass::Build));
        }
        MetadataKind::CargoLock
        | MetadataKind::NpmLock
        | MetadataKind::PnpmLock
        | MetadataKind::YarnLock
        | MetadataKind::PoetryLock
        | MetadataKind::UvLock
        | MetadataKind::GoChecksum => {}
    }

    commands
        .into_iter()
        .map(|(command, class)| {
            let candidate_id = command_candidate_id(evidence_id, command, &working_directory);
            ValidationCommandCandidate {
                candidate_id,
                command,
                class,
                working_directory: working_directory.clone(),
                provenance: CommandProvenance::ParsedProjectMetadata {
                    evidence_id: evidence_id.clone(),
                },
                authority: CommandAuthority::CandidateOnly,
            }
        })
        .collect()
}

fn command_candidate_id(
    evidence_id: &EvidenceId,
    command: ValidationCommandKind,
    working_directory: &ProfilePath,
) -> EvidenceId {
    EvidenceId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:validation-command-candidate",
            evidence_id.as_str(),
            command.identity_key(),
            working_directory.as_str(),
        ])
    ))
}

fn command_matches_metadata(command: ValidationCommandKind, kind: MetadataKind) -> bool {
    matches!(
        (command, kind),
        (
            ValidationCommandKind::CargoTest | ValidationCommandKind::CargoBuild,
            MetadataKind::CargoManifest
        ) | (
            ValidationCommandKind::NpmTest
                | ValidationCommandKind::NpmBuild
                | ValidationCommandKind::NpmTypecheck
                | ValidationCommandKind::NpmLint,
            MetadataKind::PackageManifest
        ) | (
            ValidationCommandKind::PythonPytest | ValidationCommandKind::PythonBuild,
            MetadataKind::PythonProject
        ) | (
            ValidationCommandKind::GoTestAll | ValidationCommandKind::GoBuildAll,
            MetadataKind::GoModule
        )
    )
}

fn generated_marker(content: &[u8]) -> Option<GeneratedMarkerKind> {
    let text = std::str::from_utf8(content).ok()?;
    let prefix = text
        .chars()
        .take(4_096)
        .collect::<String>()
        .to_ascii_lowercase();
    if prefix.contains("@generated") {
        Some(GeneratedMarkerKind::AtGenerated)
    } else if prefix.contains("code generated") && prefix.contains("do not edit") {
        Some(GeneratedMarkerKind::CodeGeneratedDoNotEdit)
    } else if (prefix.contains("generated file")
        || prefix.contains("file was generated")
        || prefix.contains("generated by"))
        && prefix.contains("do not edit")
    {
        Some(GeneratedMarkerKind::GeneratedFileDoNotEdit)
    } else {
        None
    }
}

fn classify_roots(
    files: &[RepositoryFileObservation],
    ecosystems: &[EcosystemKind],
) -> (Vec<ProfilePath>, Vec<ProfilePath>) {
    let mut sources = BTreeMap::<ProfilePath, ()>::new();
    let mut tests = BTreeMap::<ProfilePath, ()>::new();
    for file in files {
        if !is_source_for_ecosystem(&file.path, ecosystems) {
            continue;
        }
        let components = file.path.as_str().split('/').collect::<Vec<_>>();
        if let Some(index) = components
            .iter()
            .position(|component| matches!(*component, "test" | "tests" | "spec" | "__tests__"))
        {
            let path = components[..=index].join("/");
            tests.insert(ProfilePath(path), ());
        } else if let Some(index) = components.iter().position(|component| {
            matches!(
                *component,
                "src" | "lib" | "app" | "cmd" | "internal" | "pkg"
            )
        }) {
            let path = components[..=index].join("/");
            sources.insert(ProfilePath(path), ());
        } else if is_test_file(components.last().copied().unwrap_or_default()) {
            tests.insert(ProfilePath::root(), ());
        } else {
            sources.insert(ProfilePath::root(), ());
        }
    }
    (sources.into_keys().collect(), tests.into_keys().collect())
}

fn is_source_for_ecosystem(path: &ProfilePath, ecosystems: &[EcosystemKind]) -> bool {
    let extension = path
        .as_str()
        .rsplit_once('.')
        .map(|(_, extension)| extension);
    ecosystems.iter().any(|ecosystem| match ecosystem {
        EcosystemKind::Rust => extension == Some("rs"),
        EcosystemKind::Node => {
            matches!(extension, Some("js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs"))
        }
        EcosystemKind::Python => extension == Some("py"),
        EcosystemKind::Go => extension == Some("go"),
        EcosystemKind::Unknown => false,
    })
}

fn is_test_file(name: &str) -> bool {
    name.starts_with("test_")
        || name.ends_with("_test.go")
        || name.contains(".test.")
        || name.contains(".spec.")
}
