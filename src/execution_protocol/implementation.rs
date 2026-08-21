use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    AcceptedPlan, ChangeId, DiscoveryCriterionId, DiscoveryState, EffectId, EvidenceId,
    ExecutionId, ExecutionNode, FailureRevisionId, FileEvidence, LineRange, NodeId, NodeState,
    PlanId, PlanRevisionId, PlannedTargetV1, ProfilePath, RepairIntentId, RepairMutationBaseline,
    RepairTargetSelection, RepositoryRevisionId, TargetId, TargetOperation, ValidationEvidenceId,
    ValidationExpectationId, ValidationFailureRevisionV1, ValidationGateId,
    repair_target_for_selection, stable_sha256,
};

pub(crate) const IMPLEMENTATION_CONTEXT_SCHEMA_VERSION: u16 = 1;
const MAX_CONTEXT_EVIDENCE: usize = 64;
const MAX_CONTEXT_SECTIONS: usize = 96;
const FIXED_CONTEXT_TOKENS: u32 = 128;
const SECTION_OVERHEAD_TOKENS: u32 = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TargetContextContractError {
    Invalid {
        code: &'static str,
    },
    MandatoryContextTooLarge {
        required_tokens: u32,
        input_token_ceiling: u32,
    },
    Serialization,
}

impl TargetContextContractError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::Invalid { code } => code,
            Self::MandatoryContextTooLarge { .. } => "implementation_context_too_large",
            Self::Serialization => "implementation_context_serialization_failed",
        }
    }
}

impl fmt::Display for TargetContextContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { code } => write!(formatter, "target context violates `{code}`"),
            Self::MandatoryContextTooLarge {
                required_tokens,
                input_token_ceiling,
            } => write!(
                formatter,
                "mandatory target context requires {required_tokens} tokens but the input ceiling is {input_token_ceiling}"
            ),
            Self::Serialization => {
                formatter.write_str("target context identity serialization failed")
            }
        }
    }
}

impl std::error::Error for TargetContextContractError {}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "expectation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TargetPathExpectation {
    Existing {
        path: ProfilePath,
        expected_content_hash: String,
    },
    Absent {
        path: ProfilePath,
    },
}

impl TargetPathExpectation {
    fn path(&self) -> &ProfilePath {
        match self {
            Self::Existing { path, .. } | Self::Absent { path } => path,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "purpose", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TargetExecutionPurpose {
    Implementation {
        change_id: ChangeId,
    },
    ValidationRepair {
        repair_intent_id: RepairIntentId,
        failure_revision_id: FailureRevisionId,
        originating_gate_id: ValidationGateId,
        validation_evidence_id: ValidationEvidenceId,
        baseline_mutation_evidence_id: EvidenceId,
    },
}

impl TargetExecutionPurpose {
    fn validate(&self) -> Result<(), TargetContextContractError> {
        let valid = match self {
            Self::Implementation { change_id } => !change_id.is_empty(),
            Self::ValidationRepair {
                repair_intent_id,
                failure_revision_id,
                originating_gate_id,
                validation_evidence_id,
                baseline_mutation_evidence_id,
            } => {
                !repair_intent_id.is_empty()
                    && !failure_revision_id.is_empty()
                    && !originating_gate_id.is_empty()
                    && !validation_evidence_id.is_empty()
                    && !baseline_mutation_evidence_id.is_empty()
            }
        };
        if !valid {
            return Err(TargetContextContractError::Invalid {
                code: "target_execution_purpose_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceArtifactRequirement {
    pub(crate) evidence_id: EvidenceId,
    pub(crate) path: ProfilePath,
    pub(crate) line_range: LineRange,
    pub(crate) source_content_hash: String,
    pub(crate) artifact_reference_hash: String,
    pub(crate) encoding: super::TextEncoding,
    pub(crate) truncated: bool,
    pub(crate) mandatory: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetContextLoadRequest {
    pub(crate) schema_version: u16,
    pub(crate) request_id: EffectId,
    pub(crate) execution_id: ExecutionId,
    pub(crate) execution_attempt: u32,
    pub(crate) node_id: NodeId,
    pub(crate) node_attempt: u32,
    pub(crate) target_id: TargetId,
    pub(crate) purpose: TargetExecutionPurpose,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) goal_hash: String,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
    pub(crate) path_expectations: BTreeSet<TargetPathExpectation>,
    pub(crate) required_evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) optional_evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) artifact_requirements: Vec<EvidenceArtifactRequirement>,
    pub(crate) validation_expectation_ids: BTreeSet<ValidationExpectationId>,
    pub(crate) input_token_ceiling: u32,
}

impl TargetContextLoadRequest {
    pub(crate) fn validate(&self) -> Result<(), TargetContextContractError> {
        if self.schema_version != IMPLEMENTATION_CONTEXT_SCHEMA_VERSION
            || self.execution_id.is_empty()
            || self.node_id.is_empty()
            || self.target_id.is_empty()
            || self.purpose.validate().is_err()
            || self.plan_id.is_empty()
            || self.plan_revision_id.is_empty()
            || self.repository_revision.is_empty()
            || self.node_attempt == 0
            || !is_sha256(&self.goal_hash)
            || self.criterion_ids.is_empty()
            || self.path_expectations.is_empty()
            || self.path_expectations.len() > 2
            || self
                .path_expectations
                .iter()
                .any(|expectation| matches!(expectation, TargetPathExpectation::Existing { expected_content_hash, .. } if !is_sha256(expected_content_hash)))
            || self
                .path_expectations
                .iter()
                .map(TargetPathExpectation::path)
                .collect::<BTreeSet<_>>()
                .len()
                != self.path_expectations.len()
            || self.required_evidence_ids.is_empty()
            || self.required_evidence_ids.len() > MAX_CONTEXT_EVIDENCE
            || self.optional_evidence_ids.len() > MAX_CONTEXT_EVIDENCE
            || self
                .required_evidence_ids
                .intersection(&self.optional_evidence_ids)
                .next()
                .is_some()
            || self.validation_expectation_ids.is_empty()
            || self.input_token_ceiling == 0
        {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_load_request_invalid",
            });
        }
        let mut requirements = self.artifact_requirements.clone();
        requirements.sort_by(|left, right| {
            left.evidence_id
                .cmp(&right.evidence_id)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.line_range.start.cmp(&right.line_range.start))
                .then_with(|| {
                    left.line_range
                        .end_inclusive
                        .cmp(&right.line_range.end_inclusive)
                })
        });
        if requirements != self.artifact_requirements
            || requirements.len() > MAX_CONTEXT_EVIDENCE
            || requirements
                .iter()
                .map(|requirement| &requirement.evidence_id)
                .collect::<BTreeSet<_>>()
                .len()
                != requirements.len()
            || requirements.iter().any(|requirement| {
                !is_sha256(&requirement.source_content_hash)
                    || !is_sha256(&requirement.artifact_reference_hash)
                    || LineRange::new(
                        requirement.line_range.start,
                        requirement.line_range.end_inclusive,
                    )
                    .is_err()
                    || (requirement.mandatory
                        && !self
                            .required_evidence_ids
                            .contains(&requirement.evidence_id))
                    || (!requirement.mandatory
                        && !self
                            .optional_evidence_ids
                            .contains(&requirement.evidence_id))
            })
        {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_artifact_requirements_invalid",
            });
        }
        if self.request_id != self.expected_request_id()? {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_load_request_identity_mismatch",
            });
        }
        Ok(())
    }

    fn expected_request_id(&self) -> Result<EffectId, TargetContextContractError> {
        #[derive(Serialize)]
        struct RequestIdentity<'a> {
            schema_version: u16,
            execution_id: &'a ExecutionId,
            execution_attempt: u32,
            node_id: &'a NodeId,
            node_attempt: u32,
            target_id: &'a TargetId,
            purpose: &'a TargetExecutionPurpose,
            plan_id: &'a PlanId,
            plan_revision_id: &'a PlanRevisionId,
            repository_revision: &'a RepositoryRevisionId,
            goal_hash: &'a str,
            criterion_ids: &'a BTreeSet<DiscoveryCriterionId>,
            path_expectations: &'a BTreeSet<TargetPathExpectation>,
            required_evidence_ids: &'a BTreeSet<EvidenceId>,
            optional_evidence_ids: &'a BTreeSet<EvidenceId>,
            artifact_requirements: &'a [EvidenceArtifactRequirement],
            validation_expectation_ids: &'a BTreeSet<ValidationExpectationId>,
            input_token_ceiling: u32,
        }
        let canonical = serde_json::to_string(&RequestIdentity {
            schema_version: self.schema_version,
            execution_id: &self.execution_id,
            execution_attempt: self.execution_attempt,
            node_id: &self.node_id,
            node_attempt: self.node_attempt,
            target_id: &self.target_id,
            purpose: &self.purpose,
            plan_id: &self.plan_id,
            plan_revision_id: &self.plan_revision_id,
            repository_revision: &self.repository_revision,
            goal_hash: &self.goal_hash,
            criterion_ids: &self.criterion_ids,
            path_expectations: &self.path_expectations,
            required_evidence_ids: &self.required_evidence_ids,
            optional_evidence_ids: &self.optional_evidence_ids,
            artifact_requirements: &self.artifact_requirements,
            validation_expectation_ids: &self.validation_expectation_ids,
            input_token_ceiling: self.input_token_ceiling,
        })
        .map_err(|_| TargetContextContractError::Serialization)?;
        Ok(EffectId::new(format!(
            "epv1:{}",
            stable_sha256(&["execution-protocol-v1:target-context-load", &canonical])
        )))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ArtifactScope {
    FullFile,
    ExactRange {
        line_range: LineRange,
        source_content_hash: String,
    },
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LoadedContextArtifact {
    pub(crate) artifact_reference_hash: String,
    pub(crate) scope: ArtifactScope,
    bytes: Vec<u8>,
}

impl LoadedContextArtifact {
    pub(crate) fn new(
        artifact_reference_hash: String,
        scope: ArtifactScope,
        bytes: Vec<u8>,
    ) -> Result<Self, TargetContextContractError> {
        if !is_sha256(&artifact_reference_hash)
            || artifact_reference_hash != hex::encode(Sha256::digest(&bytes))
        {
            return Err(TargetContextContractError::Invalid {
                code: "loaded_context_artifact_invalid",
            });
        }
        if let ArtifactScope::ExactRange {
            line_range,
            source_content_hash,
        } = &scope
            && (LineRange::new(line_range.start, line_range.end_inclusive).is_err()
                || !is_sha256(source_content_hash))
        {
            return Err(TargetContextContractError::Invalid {
                code: "loaded_context_range_invalid",
            });
        }
        Ok(Self {
            artifact_reference_hash,
            scope,
            bytes,
        })
    }

    pub(crate) fn content_hash(&self) -> String {
        hex::encode(Sha256::digest(&self.bytes))
    }

    pub(crate) fn byte_len(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }

    pub(crate) fn encoding(&self) -> super::TextEncoding {
        if self.bytes.starts_with(&[0xef, 0xbb, 0xbf])
            && std::str::from_utf8(&self.bytes[3..]).is_ok()
        {
            super::TextEncoding::Utf8WithBom
        } else if std::str::from_utf8(&self.bytes).is_ok() {
            super::TextEncoding::Utf8
        } else {
            super::TextEncoding::UnknownText
        }
    }
}

impl fmt::Debug for LoadedContextArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadedContextArtifact")
            .field("artifact_reference_hash", &self.artifact_reference_hash)
            .field("scope", &self.scope)
            .field("byte_len", &self.bytes.len())
            .field("content", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoadedPathState {
    Existing {
        path: ProfilePath,
        content: LoadedContextArtifact,
    },
    Absent {
        path: ProfilePath,
    },
}

impl LoadedPathState {
    fn path(&self) -> &ProfilePath {
        match self {
            Self::Existing { path, .. } | Self::Absent { path } => path,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MaterializedTargetContext {
    pub(crate) request_id: EffectId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_fingerprint: String,
    pub(crate) path_states: Vec<LoadedPathState>,
    pub(crate) evidence_artifacts: BTreeMap<EvidenceId, LoadedContextArtifact>,
}

impl fmt::Debug for MaterializedTargetContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaterializedTargetContext")
            .field("request_id", &self.request_id)
            .field("repository_revision", &self.repository_revision)
            .field("repository_fingerprint", &self.repository_fingerprint)
            .field("path_states", &self.path_states)
            .field("evidence_artifact_count", &self.evidence_artifacts.len())
            .field("content", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "section", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TargetContextSection {
    ProtocolInstructions {
        schema_hash: String,
    },
    Goal {
        goal_hash: String,
    },
    AcceptanceCriterion {
        criterion_id: DiscoveryCriterionId,
    },
    AcceptedTarget {
        target_id: TargetId,
    },
    RepositoryPath {
        path: ProfilePath,
    },
    Evidence {
        evidence_id: EvidenceId,
    },
    ValidationExpectation {
        expectation_id: ValidationExpectationId,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TargetContextCompactionKind {
    OmittedOptional,
    BoundedRange,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetContextCompactionDecision {
    pub(crate) section: TargetContextSection,
    pub(crate) kind: TargetContextCompactionKind,
    pub(crate) original_estimated_tokens: u32,
    pub(crate) retained_estimated_tokens: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactReceipt {
    pub(crate) artifact_reference_hash: String,
    pub(crate) content_hash: String,
    pub(crate) source_content_hash: String,
    pub(crate) byte_len: u64,
    pub(crate) line_range: Option<LineRange>,
    pub(crate) encoding: super::TextEncoding,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "selection", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TargetContentSelection {
    NotRequired,
    FullFile { artifact: ArtifactReceipt },
    ExactRanges { artifacts: Vec<ArtifactReceipt> },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetContextManifest {
    pub(crate) schema_version: u16,
    pub(crate) context_manifest_id: super::ContextManifestId,
    pub(crate) request_id: EffectId,
    pub(crate) node_id: NodeId,
    pub(crate) node_attempt: u32,
    pub(crate) target_id: TargetId,
    pub(crate) purpose: TargetExecutionPurpose,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_fingerprint: String,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
    pub(crate) required_evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) selected_optional_evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) full_target_artifact: Option<ArtifactReceipt>,
    pub(crate) evidence_artifact_receipts: BTreeMap<EvidenceId, ArtifactReceipt>,
    pub(crate) target_content: TargetContentSelection,
    pub(crate) mandatory_sections: Vec<TargetContextSection>,
    pub(crate) optional_sections: Vec<TargetContextSection>,
    pub(crate) input_token_ceiling: u32,
    pub(crate) estimated_input_tokens: u32,
    pub(crate) compaction: Vec<TargetContextCompactionDecision>,
    pub(crate) materialized_context_hash: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedTargetContext {
    pub(crate) node_id: NodeId,
    pub(crate) node_attempt: u32,
    pub(crate) target_id: TargetId,
    pub(crate) request_id: EffectId,
    pub(crate) context_manifest_id: super::ContextManifestId,
    pub(crate) manifest: TargetContextManifest,
}

impl PreparedTargetContext {
    pub(crate) fn validate_against_request(
        &self,
        request: &TargetContextLoadRequest,
    ) -> Result<(), TargetContextContractError> {
        request.validate()?;
        if self.manifest.schema_version != IMPLEMENTATION_CONTEXT_SCHEMA_VERSION
            || self.node_id != request.node_id
            || self.node_attempt != request.node_attempt
            || self.target_id != request.target_id
            || self.request_id != request.request_id
            || self.context_manifest_id != self.manifest.context_manifest_id
            || self.manifest.request_id != request.request_id
            || self.manifest.node_id != request.node_id
            || self.manifest.node_attempt != request.node_attempt
            || self.manifest.target_id != request.target_id
            || self.manifest.purpose != request.purpose
            || self.manifest.plan_id != request.plan_id
            || self.manifest.plan_revision_id != request.plan_revision_id
            || self.manifest.repository_revision != request.repository_revision
            || self.manifest.criterion_ids != request.criterion_ids
            || self.manifest.required_evidence_ids != request.required_evidence_ids
            || self.manifest.input_token_ceiling != request.input_token_ceiling
            || !is_sha256(&self.manifest.repository_fingerprint)
            || !is_sha256(&self.manifest.materialized_context_hash)
        {
            return Err(TargetContextContractError::Invalid {
                code: "prepared_target_context_binding_mismatch",
            });
        }
        validate_manifest_receipts(request, &self.manifest)?;
        let projection = derive_context_projection(
            request,
            self.manifest.full_target_artifact.as_ref(),
            &self.manifest.evidence_artifact_receipts,
        )?;
        if self.manifest.target_content != projection.target_content
            || self.manifest.mandatory_sections != projection.mandatory_sections
            || self.manifest.selected_optional_evidence_ids
                != projection.selected_optional_evidence_ids
            || self.manifest.optional_sections != projection.optional_sections
            || self.manifest.estimated_input_tokens != projection.estimated_input_tokens
            || self.manifest.compaction != projection.compaction
        {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_projection_mismatch",
            });
        }
        if expected_materialized_context_hash(&self.manifest)?
            != self.manifest.materialized_context_hash
        {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_materialized_hash_mismatch",
            });
        }
        let expected_id = target_context_manifest_id(&self.manifest)?;
        if expected_id != self.manifest.context_manifest_id {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_manifest_identity_mismatch",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ImplementationEvent {
    TargetContextPrepared {
        prepared: Box<PreparedTargetContext>,
    },
    TargetContextSuperseded {
        supersession: Box<TargetContextSupersession>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImplementationEffectRequest {
    LoadTargetContext {
        request: Box<TargetContextLoadRequest>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetContextSupersession {
    pub(crate) schema_version: u16,
    pub(crate) supersession_id: EffectId,
    pub(crate) node_id: NodeId,
    pub(crate) node_attempt: u32,
    pub(crate) context_manifest_id: super::ContextManifestId,
    pub(crate) prepared_repository_revision: RepositoryRevisionId,
    pub(crate) replacement_repository_revision: RepositoryRevisionId,
}

impl TargetContextSupersession {
    pub(crate) fn new(
        prepared: &PreparedTargetContext,
        replacement_repository_revision: RepositoryRevisionId,
    ) -> Result<Self, TargetContextContractError> {
        let mut supersession = Self {
            schema_version: IMPLEMENTATION_CONTEXT_SCHEMA_VERSION,
            supersession_id: EffectId::new("pending:target-context-supersession"),
            node_id: prepared.node_id.clone(),
            node_attempt: prepared.node_attempt,
            context_manifest_id: prepared.context_manifest_id.clone(),
            prepared_repository_revision: prepared.manifest.repository_revision.clone(),
            replacement_repository_revision,
        };
        supersession.supersession_id = supersession.expected_supersession_id()?;
        supersession.validate_against(prepared)?;
        Ok(supersession)
    }

    pub(crate) fn validate_against(
        &self,
        prepared: &PreparedTargetContext,
    ) -> Result<(), TargetContextContractError> {
        if self.schema_version != IMPLEMENTATION_CONTEXT_SCHEMA_VERSION
            || self.node_id != prepared.node_id
            || self.node_attempt != prepared.node_attempt
            || self.context_manifest_id != prepared.context_manifest_id
            || self.prepared_repository_revision != prepared.manifest.repository_revision
            || self.replacement_repository_revision.is_empty()
            || self.replacement_repository_revision == self.prepared_repository_revision
            || self.supersession_id != self.expected_supersession_id()?
        {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_supersession_invalid",
            });
        }
        Ok(())
    }

    fn expected_supersession_id(&self) -> Result<EffectId, TargetContextContractError> {
        let canonical = serde_json::to_string(&(
            IMPLEMENTATION_CONTEXT_SCHEMA_VERSION,
            &self.node_id,
            self.node_attempt,
            &self.context_manifest_id,
            &self.prepared_repository_revision,
            &self.replacement_repository_revision,
        ))
        .map_err(|_| TargetContextContractError::Serialization)?;
        Ok(EffectId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:target-context-supersession",
                &canonical,
            ])
        )))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepairTargetContextLedger {
    pub(crate) schema_version: u16,
    pub(crate) prepared_contexts: BTreeMap<super::ContextManifestId, PreparedTargetContext>,
    pub(crate) current_contexts: BTreeMap<NodeId, super::ContextManifestId>,
}

impl RepairTargetContextLedger {
    pub(crate) fn new() -> Self {
        Self {
            schema_version: IMPLEMENTATION_CONTEXT_SCHEMA_VERSION,
            prepared_contexts: BTreeMap::new(),
            current_contexts: BTreeMap::new(),
        }
    }

    pub(crate) fn prepared_context_for_node(
        &self,
        node_id: &NodeId,
    ) -> Option<&PreparedTargetContext> {
        self.current_contexts
            .get(node_id)
            .and_then(|context_id| self.prepared_contexts.get(context_id))
    }

    pub(crate) fn context_for_node(&self, node_id: &NodeId) -> Option<&TargetContextManifest> {
        self.prepared_context_for_node(node_id)
            .map(|prepared| &prepared.manifest)
    }

    pub(crate) fn prepared_context(
        &self,
        context_id: &super::ContextManifestId,
    ) -> Option<&PreparedTargetContext> {
        self.prepared_contexts.get(context_id)
    }

    pub(crate) fn record_prepared_context(
        &mut self,
        prepared: PreparedTargetContext,
    ) -> Result<(), TargetContextContractError> {
        if self.schema_version != IMPLEMENTATION_CONTEXT_SCHEMA_VERSION
            || self.current_contexts.contains_key(&prepared.node_id)
            || self
                .prepared_contexts
                .contains_key(&prepared.context_manifest_id)
            || prepared.context_manifest_id != prepared.manifest.context_manifest_id
            || prepared.node_id != prepared.manifest.node_id
            || prepared.node_attempt != prepared.manifest.node_attempt
            || prepared.target_id != prepared.manifest.target_id
            || !matches!(
                &prepared.manifest.purpose,
                TargetExecutionPurpose::ValidationRepair { .. }
            )
        {
            return Err(TargetContextContractError::Invalid {
                code: "repair_target_context_already_prepared_or_invalid",
            });
        }
        let node_id = prepared.node_id.clone();
        let context_id = prepared.context_manifest_id.clone();
        self.prepared_contexts.insert(context_id.clone(), prepared);
        self.current_contexts.insert(node_id, context_id);
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<(), TargetContextContractError> {
        if self.schema_version != IMPLEMENTATION_CONTEXT_SCHEMA_VERSION
            || self.prepared_contexts.iter().any(|(context_id, prepared)| {
                context_id != &prepared.context_manifest_id
                    || prepared.context_manifest_id != prepared.manifest.context_manifest_id
                    || prepared.node_id != prepared.manifest.node_id
                    || prepared.node_attempt != prepared.manifest.node_attempt
                    || prepared.target_id != prepared.manifest.target_id
                    || !matches!(
                        &prepared.manifest.purpose,
                        TargetExecutionPurpose::ValidationRepair { .. }
                    )
            })
            || self.current_contexts.iter().any(|(node_id, context_id)| {
                self.prepared_contexts
                    .get(context_id)
                    .is_none_or(|prepared| &prepared.node_id != node_id)
            })
            || self.prepared_contexts.keys().any(|context_id| {
                !self
                    .current_contexts
                    .values()
                    .any(|current_id| current_id == context_id)
            })
        {
            return Err(TargetContextContractError::Invalid {
                code: "repair_target_context_ledger_invalid",
            });
        }
        Ok(())
    }
}

impl Default for RepairTargetContextLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImplementationState {
    pub(crate) schema_version: u16,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) node_targets: BTreeMap<NodeId, TargetId>,
    pub(crate) prepared_contexts: BTreeMap<super::ContextManifestId, PreparedTargetContext>,
    pub(crate) current_contexts: BTreeMap<NodeId, super::ContextManifestId>,
    pub(crate) superseded_contexts: BTreeMap<super::ContextManifestId, TargetContextSupersession>,
}

impl ImplementationState {
    pub(crate) fn new(plan: &AcceptedPlan) -> Result<Self, TargetContextContractError> {
        let node_targets = plan
            .targets
            .iter()
            .map(|target| {
                (
                    implementation_node_id(plan, target),
                    target.target_id.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if node_targets.len() != plan.targets.len() {
            return Err(TargetContextContractError::Invalid {
                code: "implementation_target_node_identity_collision",
            });
        }
        Ok(Self {
            schema_version: IMPLEMENTATION_CONTEXT_SCHEMA_VERSION,
            plan_id: plan.plan_id.clone(),
            plan_revision_id: plan.plan_revision_id.clone(),
            repository_revision: plan.repository_revision.clone(),
            node_targets,
            prepared_contexts: BTreeMap::new(),
            current_contexts: BTreeMap::new(),
            superseded_contexts: BTreeMap::new(),
        })
    }

    pub(crate) fn validate(&self, plan: &AcceptedPlan) -> Result<(), TargetContextContractError> {
        let expected = Self::new(plan)?;
        if self.schema_version != expected.schema_version
            || self.plan_id != expected.plan_id
            || self.plan_revision_id != expected.plan_revision_id
            || self.repository_revision != expected.repository_revision
            || self.node_targets != expected.node_targets
            || self.prepared_contexts.iter().any(|(context_id, context)| {
                let expected_purpose = plan
                    .targets
                    .iter()
                    .find(|target| target.target_id == context.target_id)
                    .map(|target| TargetExecutionPurpose::Implementation {
                        change_id: target.change_id.clone(),
                    });
                context_id != &context.context_manifest_id
                    || self.node_targets.get(&context.node_id) != Some(&context.target_id)
                    || expected_purpose.as_ref() != Some(&context.manifest.purpose)
                    || context.manifest.plan_id != self.plan_id
                    || context.manifest.plan_revision_id != self.plan_revision_id
            })
            || self.current_contexts.iter().any(|(node_id, context_id)| {
                self.prepared_contexts
                    .get(context_id)
                    .is_none_or(|context| {
                        &context.node_id != node_id
                            || self.superseded_contexts.contains_key(context_id)
                    })
            })
            || self
                .superseded_contexts
                .iter()
                .any(|(context_id, supersession)| {
                    self.prepared_contexts
                        .get(context_id)
                        .is_none_or(|context| {
                            context_id != &supersession.context_manifest_id
                                || supersession.validate_against(context).is_err()
                        })
                        || self
                            .current_contexts
                            .values()
                            .any(|current| current == context_id)
                })
            || self.prepared_contexts.keys().any(|context_id| {
                !self
                    .current_contexts
                    .values()
                    .any(|current| current == context_id)
                    && !self.superseded_contexts.contains_key(context_id)
            })
            || self
                .superseded_contexts
                .values()
                .map(|supersession| &supersession.supersession_id)
                .collect::<BTreeSet<_>>()
                .len()
                != self.superseded_contexts.len()
        {
            return Err(TargetContextContractError::Invalid {
                code: "implementation_state_binding_mismatch",
            });
        }
        Ok(())
    }

    pub(crate) fn context_for_node(&self, node_id: &NodeId) -> Option<&TargetContextManifest> {
        self.current_contexts
            .get(node_id)
            .and_then(|context_id| self.prepared_contexts.get(context_id))
            .map(|prepared| &prepared.manifest)
    }

    pub(crate) fn prepared_context_for_node(
        &self,
        node_id: &NodeId,
    ) -> Option<&PreparedTargetContext> {
        self.current_contexts
            .get(node_id)
            .and_then(|context_id| self.prepared_contexts.get(context_id))
    }

    pub(crate) fn prepared_context(
        &self,
        context_id: &super::ContextManifestId,
    ) -> Option<&PreparedTargetContext> {
        self.prepared_contexts.get(context_id)
    }

    pub(crate) fn record_prepared_context(
        &mut self,
        prepared: PreparedTargetContext,
    ) -> Result<(), TargetContextContractError> {
        if self.current_contexts.contains_key(&prepared.node_id)
            || self
                .prepared_contexts
                .contains_key(&prepared.context_manifest_id)
            || self.node_targets.get(&prepared.node_id) != Some(&prepared.target_id)
            || prepared.manifest.plan_id != self.plan_id
            || prepared.manifest.plan_revision_id != self.plan_revision_id
            || !matches!(
                &prepared.manifest.purpose,
                TargetExecutionPurpose::Implementation { .. }
            )
        {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_already_prepared",
            });
        }
        let node_id = prepared.node_id.clone();
        let context_id = prepared.context_manifest_id.clone();
        self.prepared_contexts.insert(context_id.clone(), prepared);
        self.current_contexts.insert(node_id, context_id);
        Ok(())
    }

    pub(crate) fn supersede_context(
        &mut self,
        supersession: TargetContextSupersession,
    ) -> Result<(), TargetContextContractError> {
        let prepared = self
            .prepared_contexts
            .get(&supersession.context_manifest_id)
            .ok_or(TargetContextContractError::Invalid {
                code: "target_context_supersession_unknown_context",
            })?;
        supersession.validate_against(prepared)?;
        if self.current_contexts.get(&supersession.node_id)
            != Some(&supersession.context_manifest_id)
            || self
                .superseded_contexts
                .contains_key(&supersession.context_manifest_id)
        {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_supersession_not_current",
            });
        }
        self.current_contexts.remove(&supersession.node_id);
        self.superseded_contexts
            .insert(supersession.context_manifest_id.clone(), supersession);
        Ok(())
    }
}

pub(crate) fn build_target_context_load_request(
    execution_id: &ExecutionId,
    execution_attempt: u32,
    repository_revision: &RepositoryRevisionId,
    node: &ExecutionNode,
    plan: &AcceptedPlan,
    discovery: &DiscoveryState,
) -> Result<TargetContextLoadRequest, TargetContextContractError> {
    let NodeState::Active { attempt } = node.state else {
        return Err(TargetContextContractError::Invalid {
            code: "target_context_node_not_active",
        });
    };
    build_target_context_load_request_for_attempt(
        execution_id,
        execution_attempt,
        repository_revision,
        node,
        attempt,
        plan,
        discovery,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_target_context_load_request_for_attempt(
    execution_id: &ExecutionId,
    execution_attempt: u32,
    repository_revision: &RepositoryRevisionId,
    node: &ExecutionNode,
    node_attempt: u32,
    plan: &AcceptedPlan,
    discovery: &DiscoveryState,
) -> Result<TargetContextLoadRequest, TargetContextContractError> {
    if node.kind != super::NodeKind::Implementation
        || node.budget.max_input_tokens_per_call == 0
        || node_attempt == 0
        || node_attempt > node.attempts_started
        || repository_revision.is_empty()
        || plan.repository_revision != discovery.repository_revision
    {
        return Err(TargetContextContractError::Invalid {
            code: "target_context_repository_or_node_binding_mismatch",
        });
    }
    let target = plan
        .targets
        .iter()
        .find(|target| implementation_node_id(plan, target) == node.id)
        .ok_or(TargetContextContractError::Invalid {
            code: "implementation_node_has_no_plan_target",
        })?;
    let path_expectations = path_expectations(target);
    let optional_evidence_ids = relevant_optional_evidence(target, discovery);
    let artifact_requirements =
        artifact_requirements(&target.required_evidence, &optional_evidence_ids, discovery)?;
    let validation_expectation_ids = target
        .expected_validation
        .iter()
        .map(|expectation| expectation.expectation_id.clone())
        .collect();
    let mut request = TargetContextLoadRequest {
        schema_version: IMPLEMENTATION_CONTEXT_SCHEMA_VERSION,
        request_id: EffectId::new("pending:target-context-load"),
        execution_id: execution_id.clone(),
        execution_attempt,
        node_id: node.id.clone(),
        node_attempt,
        target_id: target.target_id.clone(),
        purpose: TargetExecutionPurpose::Implementation {
            change_id: target.change_id.clone(),
        },
        plan_id: plan.plan_id.clone(),
        plan_revision_id: plan.plan_revision_id.clone(),
        repository_revision: repository_revision.clone(),
        goal_hash: discovery.goal.goal_hash.clone(),
        criterion_ids: target.acceptance_criteria.clone(),
        path_expectations,
        required_evidence_ids: target.required_evidence.clone(),
        optional_evidence_ids,
        artifact_requirements,
        validation_expectation_ids,
        input_token_ceiling: node.budget.max_input_tokens_per_call,
    };
    request.request_id = request.expected_request_id()?;
    request.validate()?;
    Ok(request)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_validation_repair_target_context_load_request(
    execution_id: &ExecutionId,
    execution_attempt: u32,
    repository_revision: &RepositoryRevisionId,
    node: &ExecutionNode,
    selection: &RepairTargetSelection,
    failure: &ValidationFailureRevisionV1,
    baseline: &RepairMutationBaseline,
    plan: &AcceptedPlan,
    discovery: &DiscoveryState,
) -> Result<TargetContextLoadRequest, TargetContextContractError> {
    let NodeState::Active { attempt } = node.state else {
        return Err(TargetContextContractError::Invalid {
            code: "repair_target_context_node_not_active",
        });
    };
    build_validation_repair_target_context_load_request_for_attempt(
        execution_id,
        execution_attempt,
        repository_revision,
        node,
        attempt,
        selection,
        failure,
        baseline,
        plan,
        discovery,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_validation_repair_target_context_load_request_for_attempt(
    execution_id: &ExecutionId,
    execution_attempt: u32,
    repository_revision: &RepositoryRevisionId,
    node: &ExecutionNode,
    node_attempt: u32,
    selection: &RepairTargetSelection,
    failure: &ValidationFailureRevisionV1,
    baseline: &RepairMutationBaseline,
    plan: &AcceptedPlan,
    discovery: &DiscoveryState,
) -> Result<TargetContextLoadRequest, TargetContextContractError> {
    if node.kind != super::NodeKind::ValidationRepair
        || node.id != selection.repair_node.id
        || node.budget != selection.repair_node.budget
        || node.budget.max_input_tokens_per_call == 0
        || node_attempt == 0
        || node_attempt > node.attempts_started
        || repository_revision != &failure.repository_revision
        || plan.repository_revision != discovery.repository_revision
    {
        return Err(TargetContextContractError::Invalid {
            code: "repair_target_context_repository_or_node_binding_mismatch",
        });
    }
    let target = repair_target_for_selection(selection, failure, plan, baseline).map_err(|_| {
        TargetContextContractError::Invalid {
            code: "repair_target_context_selection_binding_mismatch",
        }
    })?;
    let purpose =
        selection
            .execution_purpose(failure)
            .map_err(|_| TargetContextContractError::Invalid {
                code: "repair_target_context_purpose_binding_mismatch",
            })?;
    let path_expectations = path_expectations(&target);
    let mut required_evidence_ids = target.required_evidence.clone();
    required_evidence_ids.extend(selection.intent.supporting_evidence_ids.iter().cloned());
    required_evidence_ids.extend(failure.diagnostic_ids.iter().cloned());
    required_evidence_ids.insert(baseline.evidence().evidence_id.clone());
    let mut optional_evidence_ids = relevant_optional_evidence(&target, discovery);
    optional_evidence_ids.retain(|evidence_id| !required_evidence_ids.contains(evidence_id));
    let artifact_requirements =
        artifact_requirements(&required_evidence_ids, &optional_evidence_ids, discovery)?;
    let validation_expectation_ids = target
        .expected_validation
        .iter()
        .map(|expectation| expectation.expectation_id.clone())
        .collect();
    let mut request = TargetContextLoadRequest {
        schema_version: IMPLEMENTATION_CONTEXT_SCHEMA_VERSION,
        request_id: EffectId::new("pending:repair-target-context-load"),
        execution_id: execution_id.clone(),
        execution_attempt,
        node_id: node.id.clone(),
        node_attempt,
        target_id: target.target_id.clone(),
        purpose,
        plan_id: plan.plan_id.clone(),
        plan_revision_id: plan.plan_revision_id.clone(),
        repository_revision: repository_revision.clone(),
        goal_hash: discovery.goal.goal_hash.clone(),
        criterion_ids: selection.intent.criterion_ids.clone(),
        path_expectations,
        required_evidence_ids,
        optional_evidence_ids,
        artifact_requirements,
        validation_expectation_ids,
        input_token_ceiling: node.budget.max_input_tokens_per_call,
    };
    request.request_id = request.expected_request_id()?;
    request.validate()?;
    Ok(request)
}

pub(crate) fn prepare_target_context(
    request: &TargetContextLoadRequest,
    materialized: &MaterializedTargetContext,
) -> Result<PreparedTargetContext, TargetContextContractError> {
    request.validate()?;
    validate_materialized_context(request, materialized)?;

    let full_target_artifact = materialized
        .path_states
        .iter()
        .find_map(|state| match state {
            LoadedPathState::Existing { content, .. } => Some(artifact_receipt(content)),
            LoadedPathState::Absent { .. } => None,
        });
    let evidence_artifact_receipts = request
        .artifact_requirements
        .iter()
        .map(|requirement| {
            let artifact = materialized
                .evidence_artifacts
                .get(&requirement.evidence_id)
                .expect("materialized input validation established artifact presence");
            (requirement.evidence_id.clone(), artifact_receipt(artifact))
        })
        .collect::<BTreeMap<_, _>>();
    let projection = derive_context_projection(
        request,
        full_target_artifact.as_ref(),
        &evidence_artifact_receipts,
    )?;
    let mut manifest = TargetContextManifest {
        schema_version: IMPLEMENTATION_CONTEXT_SCHEMA_VERSION,
        context_manifest_id: super::ContextManifestId::new("pending:target-context"),
        request_id: request.request_id.clone(),
        node_id: request.node_id.clone(),
        node_attempt: request.node_attempt,
        target_id: request.target_id.clone(),
        purpose: request.purpose.clone(),
        plan_id: request.plan_id.clone(),
        plan_revision_id: request.plan_revision_id.clone(),
        repository_revision: request.repository_revision.clone(),
        repository_fingerprint: materialized.repository_fingerprint.clone(),
        criterion_ids: request.criterion_ids.clone(),
        required_evidence_ids: request.required_evidence_ids.clone(),
        selected_optional_evidence_ids: projection.selected_optional_evidence_ids,
        full_target_artifact,
        evidence_artifact_receipts,
        target_content: projection.target_content,
        mandatory_sections: projection.mandatory_sections,
        optional_sections: projection.optional_sections,
        input_token_ceiling: request.input_token_ceiling,
        estimated_input_tokens: projection.estimated_input_tokens,
        compaction: projection.compaction,
        materialized_context_hash: String::new(),
    };
    manifest.materialized_context_hash = expected_materialized_context_hash(&manifest)?;
    manifest.context_manifest_id = target_context_manifest_id(&manifest)?;
    let prepared = PreparedTargetContext {
        node_id: request.node_id.clone(),
        node_attempt: request.node_attempt,
        target_id: request.target_id.clone(),
        request_id: request.request_id.clone(),
        context_manifest_id: manifest.context_manifest_id.clone(),
        manifest,
    };
    prepared.validate_against_request(request)?;
    Ok(prepared)
}

pub(crate) fn implementation_node_id(plan: &AcceptedPlan, target: &PlannedTargetV1) -> NodeId {
    NodeId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:implementation-node",
            plan.plan_id.as_str(),
            target.target_id.as_str(),
        ])
    ))
}

fn path_expectations(target: &PlannedTargetV1) -> BTreeSet<TargetPathExpectation> {
    match &target.operation {
        TargetOperation::CreateFile { .. } => BTreeSet::from([TargetPathExpectation::Absent {
            path: target.path.clone(),
        }]),
        TargetOperation::ModifyExisting {
            expected_content_hash,
        }
        | TargetOperation::DeleteFile {
            expected_content_hash,
        } => BTreeSet::from([TargetPathExpectation::Existing {
            path: target.path.clone(),
            expected_content_hash: expected_content_hash.clone(),
        }]),
        TargetOperation::MoveFile {
            destination,
            expected_content_hash,
        } => BTreeSet::from([
            TargetPathExpectation::Existing {
                path: target.path.clone(),
                expected_content_hash: expected_content_hash.clone(),
            },
            TargetPathExpectation::Absent {
                path: destination.clone(),
            },
        ]),
    }
}

fn relevant_optional_evidence(
    target: &PlannedTargetV1,
    discovery: &DiscoveryState,
) -> BTreeSet<EvidenceId> {
    let target_path = target.path.as_str();
    let mut optional = BTreeSet::new();
    if let Some(impact_map) = &discovery.impact_map {
        for area in &impact_map.areas {
            if !target.acceptance_criteria.contains(&area.criterion_id)
                || !area.paths.iter().any(|path| path.as_str() == target_path)
            {
                continue;
            }
            for evidence_id in &area.evidence_ids {
                let touches_target = discovery
                    .file_evidence
                    .get(evidence_id)
                    .is_some_and(|evidence| evidence.path.as_str() == target_path)
                    || discovery
                        .relationships
                        .get(evidence_id)
                        .is_some_and(|evidence| {
                            evidence.from.as_str() == target_path
                                || evidence.to.as_str() == target_path
                        });
                if touches_target && !target.required_evidence.contains(evidence_id) {
                    optional.insert(evidence_id.clone());
                }
            }
        }
    }
    optional
}

fn artifact_requirements(
    required: &BTreeSet<EvidenceId>,
    optional: &BTreeSet<EvidenceId>,
    discovery: &DiscoveryState,
) -> Result<Vec<EvidenceArtifactRequirement>, TargetContextContractError> {
    let mut requirements = Vec::new();
    for (evidence_id, mandatory) in required
        .iter()
        .map(|id| (id, true))
        .chain(optional.iter().map(|id| (id, false)))
    {
        if let Some(file) = discovery.file_evidence.get(evidence_id) {
            requirements.push(file_artifact_requirement(file, mandatory)?);
        }
    }
    requirements.sort_by(|left, right| {
        left.evidence_id
            .cmp(&right.evidence_id)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.line_range.start.cmp(&right.line_range.start))
            .then_with(|| {
                left.line_range
                    .end_inclusive
                    .cmp(&right.line_range.end_inclusive)
            })
    });
    Ok(requirements)
}

fn file_artifact_requirement(
    file: &FileEvidence,
    mandatory: bool,
) -> Result<EvidenceArtifactRequirement, TargetContextContractError> {
    Ok(EvidenceArtifactRequirement {
        evidence_id: file.evidence_id.clone(),
        path: ProfilePath::new(file.path.as_str()).map_err(|_| {
            TargetContextContractError::Invalid {
                code: "target_context_evidence_path_invalid",
            }
        })?,
        line_range: file.line_range.clone(),
        source_content_hash: file.content_hash.clone(),
        artifact_reference_hash: file.artifact_reference_hash.clone(),
        encoding: file.encoding,
        truncated: file.truncated,
        mandatory,
    })
}

fn validate_materialized_context(
    request: &TargetContextLoadRequest,
    materialized: &MaterializedTargetContext,
) -> Result<(), TargetContextContractError> {
    let expected_artifact_ids = request
        .artifact_requirements
        .iter()
        .map(|requirement| requirement.evidence_id.clone())
        .collect::<BTreeSet<_>>();
    if materialized.request_id != request.request_id
        || materialized.repository_revision != request.repository_revision
        || !is_sha256(&materialized.repository_fingerprint)
        || materialized.evidence_artifacts.len() > MAX_CONTEXT_EVIDENCE
    {
        return Err(TargetContextContractError::Invalid {
            code: "materialized_target_context_binding_mismatch",
        });
    }
    if materialized
        .evidence_artifacts
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>()
        != expected_artifact_ids
    {
        return Err(TargetContextContractError::Invalid {
            code: "target_context_artifact_set_mismatch",
        });
    }
    let observed_paths = materialized
        .path_states
        .iter()
        .map(|state| state.path().clone())
        .collect::<BTreeSet<_>>();
    let expected_paths = request
        .path_expectations
        .iter()
        .map(|expectation| expectation.path().clone())
        .collect::<BTreeSet<_>>();
    if observed_paths != expected_paths || observed_paths.len() != materialized.path_states.len() {
        return Err(TargetContextContractError::Invalid {
            code: "materialized_target_paths_mismatch",
        });
    }
    for expectation in &request.path_expectations {
        let observed = materialized
            .path_states
            .iter()
            .find(|state| state.path() == expectation.path())
            .expect("path-set equality established observation presence");
        match (expectation, observed) {
            (
                TargetPathExpectation::Existing {
                    expected_content_hash,
                    ..
                },
                LoadedPathState::Existing { content, .. },
            ) if matches!(content.scope, ArtifactScope::FullFile)
                && content.content_hash() == *expected_content_hash => {}
            (TargetPathExpectation::Absent { .. }, LoadedPathState::Absent { .. }) => {}
            _ => {
                return Err(TargetContextContractError::Invalid {
                    code: "materialized_target_path_state_conflict",
                });
            }
        }
    }
    for requirement in &request.artifact_requirements {
        let Some(artifact) = materialized
            .evidence_artifacts
            .get(&requirement.evidence_id)
        else {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_artifact_missing",
            });
        };
        let scope_matches = match &artifact.scope {
            ArtifactScope::FullFile => {
                !requirement.truncated && artifact.content_hash() == requirement.source_content_hash
            }
            ArtifactScope::ExactRange {
                line_range,
                source_content_hash,
            } => {
                line_range == &requirement.line_range
                    && source_content_hash == &requirement.source_content_hash
            }
        };
        if artifact.artifact_reference_hash != requirement.artifact_reference_hash
            || artifact.encoding() != requirement.encoding
            || !scope_matches
        {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_artifact_binding_mismatch",
            });
        }
    }
    Ok(())
}

fn artifact_receipt(artifact: &LoadedContextArtifact) -> ArtifactReceipt {
    let (source_content_hash, line_range) = match &artifact.scope {
        ArtifactScope::FullFile => (artifact.content_hash(), None),
        ArtifactScope::ExactRange {
            line_range,
            source_content_hash,
        } => (source_content_hash.clone(), Some(line_range.clone())),
    };
    ArtifactReceipt {
        artifact_reference_hash: artifact.artifact_reference_hash.clone(),
        content_hash: artifact.content_hash(),
        source_content_hash,
        byte_len: artifact.byte_len(),
        line_range,
        encoding: artifact.encoding(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ContextProjection {
    target_content: TargetContentSelection,
    mandatory_sections: Vec<TargetContextSection>,
    selected_optional_evidence_ids: BTreeSet<EvidenceId>,
    optional_sections: Vec<TargetContextSection>,
    estimated_input_tokens: u32,
    compaction: Vec<TargetContextCompactionDecision>,
}

fn derive_context_projection(
    request: &TargetContextLoadRequest,
    full_target_artifact: Option<&ArtifactReceipt>,
    evidence_receipts: &BTreeMap<EvidenceId, ArtifactReceipt>,
) -> Result<ContextProjection, TargetContextContractError> {
    let mandatory_sections = canonical_mandatory_sections(request);
    let required_receipts = request
        .required_evidence_ids
        .iter()
        .filter_map(|evidence_id| evidence_receipts.get(evidence_id))
        .collect::<Vec<_>>();
    let no_existing_target = request
        .path_expectations
        .iter()
        .all(|expectation| matches!(expectation, TargetPathExpectation::Absent { .. }));
    let mut compaction = Vec::new();
    let target_content = if no_existing_target {
        TargetContentSelection::NotRequired
    } else {
        let full = full_target_artifact.ok_or(TargetContextContractError::Invalid {
            code: "target_context_full_target_receipt_missing",
        })?;
        let full_selection = TargetContentSelection::FullFile {
            artifact: full.clone(),
        };
        let full_tokens = estimate_context_tokens(
            &mandatory_sections,
            &[],
            &full_selection,
            &required_receipts,
            &[],
        )?;
        if full_tokens <= request.input_token_ceiling {
            full_selection
        } else {
            let target_path = request
                .path_expectations
                .iter()
                .find_map(|expectation| match expectation {
                    TargetPathExpectation::Existing { path, .. } => Some(path),
                    TargetPathExpectation::Absent { .. } => None,
                })
                .expect("existing target expectation was established");
            let mut ranges = request
                .artifact_requirements
                .iter()
                .filter(|requirement| requirement.mandatory && &requirement.path == target_path)
                .filter_map(|requirement| evidence_receipts.get(&requirement.evidence_id))
                .filter(|receipt| receipt.line_range.is_some())
                .cloned()
                .collect::<Vec<_>>();
            ranges.sort_by(|left, right| {
                left.line_range
                    .as_ref()
                    .map(|range| (range.start, range.end_inclusive))
                    .cmp(
                        &right
                            .line_range
                            .as_ref()
                            .map(|range| (range.start, range.end_inclusive)),
                    )
                    .then_with(|| {
                        left.artifact_reference_hash
                            .cmp(&right.artifact_reference_hash)
                    })
            });
            ranges.dedup();
            let ranged = TargetContentSelection::ExactRanges { artifacts: ranges };
            let ranged_tokens = estimate_context_tokens(
                &mandatory_sections,
                &[],
                &ranged,
                &required_receipts,
                &[],
            )?;
            let has_ranges = matches!(&ranged, TargetContentSelection::ExactRanges { artifacts } if !artifacts.is_empty());
            if !has_ranges || ranged_tokens > request.input_token_ceiling {
                return Err(TargetContextContractError::MandatoryContextTooLarge {
                    required_tokens: if has_ranges {
                        ranged_tokens
                    } else {
                        full_tokens
                    },
                    input_token_ceiling: request.input_token_ceiling,
                });
            }
            compaction.push(TargetContextCompactionDecision {
                section: TargetContextSection::AcceptedTarget {
                    target_id: request.target_id.clone(),
                },
                kind: TargetContextCompactionKind::BoundedRange,
                original_estimated_tokens: receipt_token_cost(full)?,
                retained_estimated_tokens: match &ranged {
                    TargetContentSelection::ExactRanges { artifacts } => {
                        artifacts.iter().try_fold(0_u32, |sum, receipt| {
                            Ok::<_, TargetContextContractError>(
                                sum.saturating_add(receipt_token_cost(receipt)?),
                            )
                        })?
                    }
                    _ => 0,
                },
            });
            ranged
        }
    };

    let required_tokens = estimate_context_tokens(
        &mandatory_sections,
        &[],
        &target_content,
        &required_receipts,
        &[],
    )?;
    if required_tokens > request.input_token_ceiling {
        return Err(TargetContextContractError::MandatoryContextTooLarge {
            required_tokens,
            input_token_ceiling: request.input_token_ceiling,
        });
    }
    let mut selected_optional_evidence_ids = BTreeSet::new();
    let mut optional_sections = Vec::new();
    let mut estimated_input_tokens = required_tokens;
    for evidence_id in &request.optional_evidence_ids {
        let section = TargetContextSection::Evidence {
            evidence_id: evidence_id.clone(),
        };
        let mut proposed_ids = selected_optional_evidence_ids.clone();
        proposed_ids.insert(evidence_id.clone());
        let mut proposed_sections = optional_sections.clone();
        proposed_sections.push(section.clone());
        let optional_receipts = proposed_ids
            .iter()
            .filter_map(|id| evidence_receipts.get(id))
            .collect::<Vec<_>>();
        let proposed_tokens = estimate_context_tokens(
            &mandatory_sections,
            &proposed_sections,
            &target_content,
            &required_receipts,
            &optional_receipts,
        )?;
        if proposed_tokens <= request.input_token_ceiling {
            selected_optional_evidence_ids = proposed_ids;
            optional_sections = proposed_sections;
            estimated_input_tokens = proposed_tokens;
        } else {
            compaction.push(TargetContextCompactionDecision {
                section,
                kind: TargetContextCompactionKind::OmittedOptional,
                original_estimated_tokens: proposed_tokens.saturating_sub(estimated_input_tokens),
                retained_estimated_tokens: 0,
            });
        }
    }
    if mandatory_sections.len() + optional_sections.len() > MAX_CONTEXT_SECTIONS {
        return Err(TargetContextContractError::Invalid {
            code: "target_context_section_limit_exceeded",
        });
    }
    Ok(ContextProjection {
        target_content,
        mandatory_sections,
        selected_optional_evidence_ids,
        optional_sections,
        estimated_input_tokens,
        compaction,
    })
}

fn estimate_context_tokens(
    mandatory_sections: &[TargetContextSection],
    optional_sections: &[TargetContextSection],
    target_content: &TargetContentSelection,
    required_receipts: &[&ArtifactReceipt],
    optional_receipts: &[&ArtifactReceipt],
) -> Result<u32, TargetContextContractError> {
    let serialized_sections = serde_json::to_vec(&(mandatory_sections, optional_sections))
        .map_err(|_| TargetContextContractError::Serialization)?;
    let mut tokens = FIXED_CONTEXT_TOKENS
        .saturating_add(u32::try_from(serialized_sections.len()).unwrap_or(u32::MAX));
    let target_receipts = match target_content {
        TargetContentSelection::NotRequired => Vec::new(),
        TargetContentSelection::FullFile { artifact } => vec![artifact],
        TargetContentSelection::ExactRanges { artifacts } => artifacts.iter().collect(),
    };
    let mut seen_references = BTreeSet::new();
    for receipt in target_receipts
        .into_iter()
        .chain(required_receipts.iter().copied())
        .chain(optional_receipts.iter().copied())
    {
        if seen_references.insert(receipt.artifact_reference_hash.clone()) {
            tokens = tokens.saturating_add(receipt_token_cost(receipt)?);
        }
    }
    Ok(tokens)
}

fn receipt_token_cost(receipt: &ArtifactReceipt) -> Result<u32, TargetContextContractError> {
    let serialized =
        serde_json::to_vec(receipt).map_err(|_| TargetContextContractError::Serialization)?;
    Ok(u32::try_from(serialized.len())
        .unwrap_or(u32::MAX)
        .saturating_add(u32::try_from(receipt.byte_len).unwrap_or(u32::MAX))
        .saturating_add(SECTION_OVERHEAD_TOKENS))
}

fn canonical_mandatory_sections(request: &TargetContextLoadRequest) -> Vec<TargetContextSection> {
    let mut sections = vec![
        TargetContextSection::ProtocolInstructions {
            schema_hash: stable_sha256(&["execution-protocol-v1:target-context-schema", "1"]),
        },
        TargetContextSection::Goal {
            goal_hash: request.goal_hash.clone(),
        },
        TargetContextSection::AcceptedTarget {
            target_id: request.target_id.clone(),
        },
    ];
    sections.extend(
        request
            .criterion_ids
            .iter()
            .cloned()
            .map(|criterion_id| TargetContextSection::AcceptanceCriterion { criterion_id }),
    );
    sections.extend(request.path_expectations.iter().map(|expectation| {
        TargetContextSection::RepositoryPath {
            path: expectation.path().clone(),
        }
    }));
    sections.extend(
        request
            .required_evidence_ids
            .iter()
            .cloned()
            .map(|evidence_id| TargetContextSection::Evidence { evidence_id }),
    );
    sections.extend(
        request
            .validation_expectation_ids
            .iter()
            .cloned()
            .map(|expectation_id| TargetContextSection::ValidationExpectation { expectation_id }),
    );
    sections
}

fn validate_manifest_receipts(
    request: &TargetContextLoadRequest,
    manifest: &TargetContextManifest,
) -> Result<(), TargetContextContractError> {
    let expected_receipt_ids = request
        .artifact_requirements
        .iter()
        .map(|requirement| requirement.evidence_id.clone())
        .collect::<BTreeSet<_>>();
    if manifest
        .evidence_artifact_receipts
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>()
        != expected_receipt_ids
    {
        return Err(TargetContextContractError::Invalid {
            code: "target_context_evidence_receipt_set_mismatch",
        });
    }
    for requirement in &request.artifact_requirements {
        let Some(receipt) = manifest
            .evidence_artifact_receipts
            .get(&requirement.evidence_id)
        else {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_evidence_receipt_missing",
            });
        };
        let full_file = receipt.line_range.is_none()
            && receipt.content_hash == requirement.source_content_hash
            && receipt.source_content_hash == requirement.source_content_hash
            && receipt.encoding == requirement.encoding;
        let exact_range = receipt.line_range.as_ref() == Some(&requirement.line_range)
            && receipt.source_content_hash == requirement.source_content_hash
            && receipt.encoding == requirement.encoding
            && is_sha256(&receipt.content_hash);
        if receipt.artifact_reference_hash != requirement.artifact_reference_hash
            || receipt.content_hash != receipt.artifact_reference_hash
            || (!full_file && !exact_range)
        {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_evidence_receipt_invalid",
            });
        }
    }

    let expected_existing_hash =
        request
            .path_expectations
            .iter()
            .find_map(|expectation| match expectation {
                TargetPathExpectation::Existing {
                    expected_content_hash,
                    ..
                } => Some(expected_content_hash),
                TargetPathExpectation::Absent { .. } => None,
            });
    match (&manifest.full_target_artifact, expected_existing_hash) {
        (None, None) => {}
        (Some(artifact), Some(expected_hash))
            if artifact.line_range.is_none()
                && artifact.source_content_hash == *expected_hash
                && artifact.content_hash == *expected_hash
                && artifact.artifact_reference_hash == *expected_hash => {}
        _ => {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_full_target_receipt_invalid",
            });
        }
    }
    match (&manifest.target_content, expected_existing_hash) {
        (TargetContentSelection::NotRequired, None) => {}
        (TargetContentSelection::FullFile { artifact }, Some(expected_hash))
            if artifact.line_range.is_none()
                && artifact.content_hash == *expected_hash
                && artifact.source_content_hash == *expected_hash
                && artifact.content_hash == artifact.artifact_reference_hash
                && is_sha256(&artifact.artifact_reference_hash)
                && Some(artifact) == manifest.full_target_artifact.as_ref() => {}
        (TargetContentSelection::ExactRanges { artifacts }, Some(expected_hash))
            if !artifacts.is_empty()
                && artifacts.iter().all(|artifact| {
                    artifact.line_range.is_some()
                        && artifact.source_content_hash == *expected_hash
                        && artifact.content_hash == artifact.artifact_reference_hash
                        && is_sha256(&artifact.artifact_reference_hash)
                        && request.artifact_requirements.iter().any(|requirement| {
                            requirement.mandatory
                                && requirement.source_content_hash == *expected_hash
                                && Some(&requirement.line_range) == artifact.line_range.as_ref()
                                && requirement.artifact_reference_hash
                                    == artifact.artifact_reference_hash
                        })
                }) => {}
        _ => {
            return Err(TargetContextContractError::Invalid {
                code: "target_context_content_selection_invalid",
            });
        }
    }
    Ok(())
}

fn expected_materialized_context_hash(
    manifest: &TargetContextManifest,
) -> Result<String, TargetContextContractError> {
    #[derive(Serialize)]
    struct MaterializedIdentity<'a> {
        schema_version: u16,
        request_id: &'a EffectId,
        node_id: &'a NodeId,
        node_attempt: u32,
        target_id: &'a TargetId,
        purpose: &'a TargetExecutionPurpose,
        plan_id: &'a PlanId,
        plan_revision_id: &'a PlanRevisionId,
        repository_revision: &'a RepositoryRevisionId,
        repository_fingerprint: &'a str,
        criterion_ids: &'a BTreeSet<DiscoveryCriterionId>,
        required_evidence_ids: &'a BTreeSet<EvidenceId>,
        selected_optional_evidence_ids: &'a BTreeSet<EvidenceId>,
        full_target_artifact: &'a Option<ArtifactReceipt>,
        evidence_artifact_receipts: &'a BTreeMap<EvidenceId, ArtifactReceipt>,
        target_content: &'a TargetContentSelection,
        mandatory_sections: &'a [TargetContextSection],
        optional_sections: &'a [TargetContextSection],
        input_token_ceiling: u32,
        estimated_input_tokens: u32,
        compaction: &'a [TargetContextCompactionDecision],
    }
    let canonical = serde_json::to_string(&MaterializedIdentity {
        schema_version: manifest.schema_version,
        request_id: &manifest.request_id,
        node_id: &manifest.node_id,
        node_attempt: manifest.node_attempt,
        target_id: &manifest.target_id,
        purpose: &manifest.purpose,
        plan_id: &manifest.plan_id,
        plan_revision_id: &manifest.plan_revision_id,
        repository_revision: &manifest.repository_revision,
        repository_fingerprint: &manifest.repository_fingerprint,
        criterion_ids: &manifest.criterion_ids,
        required_evidence_ids: &manifest.required_evidence_ids,
        selected_optional_evidence_ids: &manifest.selected_optional_evidence_ids,
        full_target_artifact: &manifest.full_target_artifact,
        evidence_artifact_receipts: &manifest.evidence_artifact_receipts,
        target_content: &manifest.target_content,
        mandatory_sections: &manifest.mandatory_sections,
        optional_sections: &manifest.optional_sections,
        input_token_ceiling: manifest.input_token_ceiling,
        estimated_input_tokens: manifest.estimated_input_tokens,
        compaction: &manifest.compaction,
    })
    .map_err(|_| TargetContextContractError::Serialization)?;
    Ok(stable_sha256(&[
        "execution-protocol-v1:materialized-target-context",
        &canonical,
    ]))
}

fn target_context_manifest_id(
    manifest: &TargetContextManifest,
) -> Result<super::ContextManifestId, TargetContextContractError> {
    #[derive(Serialize)]
    struct ManifestIdentity<'a> {
        schema_version: u16,
        request_id: &'a EffectId,
        node_id: &'a NodeId,
        node_attempt: u32,
        target_id: &'a TargetId,
        purpose: &'a TargetExecutionPurpose,
        plan_id: &'a PlanId,
        plan_revision_id: &'a PlanRevisionId,
        repository_revision: &'a RepositoryRevisionId,
        repository_fingerprint: &'a str,
        criterion_ids: &'a BTreeSet<DiscoveryCriterionId>,
        required_evidence_ids: &'a BTreeSet<EvidenceId>,
        selected_optional_evidence_ids: &'a BTreeSet<EvidenceId>,
        full_target_artifact: &'a Option<ArtifactReceipt>,
        evidence_artifact_receipts: &'a BTreeMap<EvidenceId, ArtifactReceipt>,
        target_content: &'a TargetContentSelection,
        mandatory_sections: &'a [TargetContextSection],
        optional_sections: &'a [TargetContextSection],
        input_token_ceiling: u32,
        estimated_input_tokens: u32,
        compaction: &'a [TargetContextCompactionDecision],
        materialized_context_hash: &'a str,
    }
    let canonical = serde_json::to_string(&ManifestIdentity {
        schema_version: manifest.schema_version,
        request_id: &manifest.request_id,
        node_id: &manifest.node_id,
        node_attempt: manifest.node_attempt,
        target_id: &manifest.target_id,
        purpose: &manifest.purpose,
        plan_id: &manifest.plan_id,
        plan_revision_id: &manifest.plan_revision_id,
        repository_revision: &manifest.repository_revision,
        repository_fingerprint: &manifest.repository_fingerprint,
        criterion_ids: &manifest.criterion_ids,
        required_evidence_ids: &manifest.required_evidence_ids,
        selected_optional_evidence_ids: &manifest.selected_optional_evidence_ids,
        full_target_artifact: &manifest.full_target_artifact,
        evidence_artifact_receipts: &manifest.evidence_artifact_receipts,
        target_content: &manifest.target_content,
        mandatory_sections: &manifest.mandatory_sections,
        optional_sections: &manifest.optional_sections,
        input_token_ceiling: manifest.input_token_ceiling,
        estimated_input_tokens: manifest.estimated_input_tokens,
        compaction: &manifest.compaction,
        materialized_context_hash: &manifest.materialized_context_hash,
    })
    .map_err(|_| TargetContextContractError::Serialization)?;
    Ok(super::ContextManifestId::new(format!(
        "epv1:{}",
        stable_sha256(&["execution-protocol-v1:target-context-manifest", &canonical])
    )))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
