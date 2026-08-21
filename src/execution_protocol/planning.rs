use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use super::{
    ActionId, ChangeId, DiscoveryConvergence, DiscoveryCriterionId, DiscoveryPath, DiscoveryState,
    EvidenceId, ModelCallAdmission, NodeBudgetContract, NodeId, NodeKind, NodeSpec, PlanId,
    PlanRevisionId, ProfilePath, ProofId, RepositoryProfile, RepositoryProfileId,
    RepositoryRevisionId, ReservationId, TargetId, ValidationExpectationId, stable_sha256,
};

pub(crate) const PLANNING_SCHEMA_VERSION: u16 = 1;
const MAX_PLAN_TARGETS: usize = 32;
const MAX_TARGET_DEPENDENCIES: usize = 16;
const MAX_TARGET_EVIDENCE: usize = 32;
const MAX_TARGET_CRITERIA: usize = 32;
const MAX_VALIDATION_EXPECTATIONS: usize = 16;
const MAX_CRITERION_SATISFACTION_OBSERVATIONS: usize = 64;
const MAX_SATISFACTION_SUPPORTING_EVIDENCE: usize = 32;
const MAX_TEXT_BYTES: usize = 2_048;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_SEMANTIC_ID_BYTES: usize = 256;
const MAX_PLAN_CANDIDATE_BYTES: usize = 256 * 1_024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanGraphBudgetContract {
    pub(crate) max_implementation_nodes: u32,
    pub(crate) max_validation_nodes: u32,
    pub(crate) max_total_nodes: u32,
    pub(crate) implementation: NodeBudgetContract,
    pub(crate) validation: NodeBudgetContract,
    pub(crate) review: NodeBudgetContract,
    pub(crate) completion_evaluation: NodeBudgetContract,
    pub(crate) publication: NodeBudgetContract,
}

impl PlanGraphBudgetContract {
    pub(crate) fn validate(&self) -> Result<(), PlanningContractError> {
        if self.max_implementation_nodes == 0
            || self.max_validation_nodes == 0
            || self.max_total_nodes < 5
            || !model_node_budget_is_feasible(&self.implementation, true)
            || !model_node_budget_is_feasible(&self.review, false)
            || !model_node_budget_is_feasible(&self.completion_evaluation, false)
            || self.validation != NodeBudgetContract::deterministic()
            || self.publication != NodeBudgetContract::deterministic()
        {
            return Err(PlanningContractError::InvalidCandidate {
                code: "plan_graph_budget_contract_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanMissionCapacity {
    pub(crate) remaining_model_calls: u32,
    pub(crate) remaining_cost_micros: u64,
    pub(crate) remaining_duration_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlanningContractError {
    InvalidCandidate { code: &'static str },
    InvalidContext { code: &'static str },
    LimitExceeded { field: &'static str, limit: usize },
    Serialization,
}

impl PlanningContractError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::InvalidCandidate { code } | Self::InvalidContext { code } => code,
            Self::LimitExceeded { .. } => "planning_limit_exceeded",
            Self::Serialization => "planning_identity_serialization_failed",
        }
    }
}

impl fmt::Display for PlanningContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCandidate { code } => {
                write!(formatter, "planning candidate violates `{code}`")
            }
            Self::InvalidContext { code } => {
                write!(formatter, "planning context violates `{code}`")
            }
            Self::LimitExceeded { field, limit } => {
                write!(formatter, "planning field `{field}` exceeds limit {limit}")
            }
            Self::Serialization => formatter.write_str("planning identity serialization failed"),
        }
    }
}

impl std::error::Error for PlanningContractError {}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TargetRole {
    Source,
    Test,
    Documentation,
    Configuration,
    Metadata,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CreatedFileKind {
    Source,
    Test,
    Documentation,
    Configuration,
    Metadata,
    Other,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreationSpecification {
    pub(crate) kind: CreatedFileKind,
    pub(crate) purpose: String,
}

impl CreationSpecification {
    pub(crate) fn new(
        kind: CreatedFileKind,
        purpose: impl Into<String>,
    ) -> Result<Self, PlanningContractError> {
        let purpose = purpose.into();
        validate_bounded_text("creation_purpose", &purpose)?;
        Ok(Self { kind, purpose })
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TargetOperation {
    ModifyExisting {
        expected_content_hash: String,
    },
    CreateFile {
        specification: CreationSpecification,
    },
    DeleteFile {
        expected_content_hash: String,
    },
    MoveFile {
        destination: ProfilePath,
        expected_content_hash: String,
    },
}

impl TargetOperation {
    pub(crate) const fn requires_existing(&self) -> bool {
        !matches!(self, Self::CreateFile { .. })
    }

    pub(crate) fn destination(&self) -> Option<&ProfilePath> {
        match self {
            Self::MoveFile { destination, .. } => Some(destination),
            _ => None,
        }
    }

    pub(crate) fn expected_content_hash(&self) -> Option<&str> {
        match self {
            Self::ModifyExisting {
                expected_content_hash,
            }
            | Self::DeleteFile {
                expected_content_hash,
            }
            | Self::MoveFile {
                expected_content_hash,
                ..
            } => Some(expected_content_hash),
            Self::CreateFile { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChangeSize {
    Tiny,
    Small,
    Medium,
    Large,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChangeRisk {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChangeEstimate {
    pub(crate) size: ChangeSize,
    pub(crate) risk: ChangeRisk,
    pub(crate) estimated_changed_lines: u32,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationExpectation {
    pub(crate) expectation_id: ValidationExpectationId,
    pub(crate) command_candidate_id: EvidenceId,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
}

impl ValidationExpectation {
    pub(crate) fn new(
        command_candidate_id: EvidenceId,
        criterion_ids: BTreeSet<DiscoveryCriterionId>,
    ) -> Result<Self, PlanningContractError> {
        if criterion_ids.is_empty() || criterion_ids.len() > MAX_TARGET_CRITERIA {
            return Err(PlanningContractError::InvalidCandidate {
                code: "validation_expectation_criteria_invalid",
            });
        }
        let identity = canonical_json(&(&command_candidate_id, &criterion_ids))?;
        Ok(Self {
            expectation_id: ValidationExpectationId::new(format!(
                "epv1:{}",
                stable_sha256(&["execution-protocol-v1:validation-expectation", &identity,])
            )),
            command_candidate_id,
            criterion_ids,
        })
    }

    fn validate_identity(&self) -> Result<(), PlanningContractError> {
        if Self::new(
            self.command_candidate_id.clone(),
            self.criterion_ids.clone(),
        )? != *self
        {
            return Err(PlanningContractError::InvalidCandidate {
                code: "validation_expectation_identity_mismatch",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlannedTargetV1 {
    pub(crate) target_id: TargetId,
    pub(crate) change_id: ChangeId,
    pub(crate) path: ProfilePath,
    pub(crate) operation: TargetOperation,
    pub(crate) role: TargetRole,
    pub(crate) rationale: String,
    pub(crate) acceptance_criteria: BTreeSet<DiscoveryCriterionId>,
    pub(crate) required_evidence: BTreeSet<EvidenceId>,
    pub(crate) expected_validation: BTreeSet<ValidationExpectation>,
    pub(crate) dependencies: BTreeSet<TargetId>,
    pub(crate) estimated_change: ChangeEstimate,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "authority", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CriterionSatisfactionAuthority {
    RequiredValidationPassed { proof_id: ProofId },
    CompletionEvaluated { proof_id: ProofId },
}

impl CriterionSatisfactionAuthority {
    pub(crate) fn proof_id(&self) -> &ProofId {
        match self {
            Self::RequiredValidationPassed { proof_id }
            | Self::CompletionEvaluated { proof_id } => proof_id,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CriterionSatisfactionObservation {
    pub(crate) schema_version: u16,
    pub(crate) observation_id: EvidenceId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) criterion_id: DiscoveryCriterionId,
    pub(crate) authority: CriterionSatisfactionAuthority,
    pub(crate) supporting_evidence_ids: BTreeSet<EvidenceId>,
}

impl CriterionSatisfactionObservation {
    pub(crate) fn new(
        repository_revision: RepositoryRevisionId,
        criterion_id: DiscoveryCriterionId,
        authority: CriterionSatisfactionAuthority,
        supporting_evidence_ids: BTreeSet<EvidenceId>,
    ) -> Result<Self, PlanningContractError> {
        if supporting_evidence_ids.is_empty()
            || supporting_evidence_ids.len() > MAX_SATISFACTION_SUPPORTING_EVIDENCE
        {
            return Err(PlanningContractError::InvalidCandidate {
                code: "criterion_satisfaction_support_invalid",
            });
        }
        let identity = canonical_json(&(
            PLANNING_SCHEMA_VERSION,
            &repository_revision,
            &criterion_id,
            &authority,
            &supporting_evidence_ids,
        ))?;
        Ok(Self {
            schema_version: PLANNING_SCHEMA_VERSION,
            observation_id: EvidenceId::new(format!(
                "epv1:{}",
                stable_sha256(&["execution-protocol-v1:criterion-satisfaction", &identity])
            )),
            repository_revision,
            criterion_id,
            authority,
            supporting_evidence_ids,
        })
    }

    fn validate_identity(&self) -> Result<(), PlanningContractError> {
        if Self::new(
            self.repository_revision.clone(),
            self.criterion_id.clone(),
            self.authority.clone(),
            self.supporting_evidence_ids.clone(),
        )? != *self
        {
            return Err(PlanningContractError::InvalidCandidate {
                code: "criterion_satisfaction_identity_mismatch",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PlanDecisionCandidate {
    Changes {
        targets: Vec<PlannedTargetV1>,
    },
    NoOp {
        criterion_satisfaction: Vec<CriterionSatisfactionObservation>,
    },
    EvidenceGap {
        criterion_ids: BTreeSet<DiscoveryCriterionId>,
        reason_code: PlanningEvidenceGapReason,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanningEvidenceGapReason {
    TargetEvidenceMissing,
    ValidationProvenanceMissing,
    UnsafeGeneratedTarget,
    DependencyUnresolved,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanCandidate {
    pub(crate) schema_version: u16,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) revision_index: u32,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) discovery_impact_map_id: EvidenceId,
    pub(crate) decision: PlanDecisionCandidate,
}

impl PlanCandidate {
    pub(crate) fn new(
        revision_index: u32,
        repository_revision: RepositoryRevisionId,
        discovery_impact_map_id: EvidenceId,
        decision: PlanDecisionCandidate,
    ) -> Result<Self, PlanningContractError> {
        if revision_index == 0 {
            return Err(PlanningContractError::InvalidCandidate {
                code: "plan_revision_index_invalid",
            });
        }
        validate_plan_decision_hard_bounds(&decision)?;
        let decision = canonicalize_plan_decision(decision)?;
        let plan_identity = canonical_json(&(
            PLANNING_SCHEMA_VERSION,
            &repository_revision,
            &discovery_impact_map_id,
            &decision,
        ))?;
        let plan_id = PlanId::new(format!(
            "epv1:{}",
            stable_sha256(&["execution-protocol-v1:plan", &plan_identity])
        ));
        let plan_revision_id = PlanRevisionId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:plan-revision",
                plan_id.as_str(),
                &revision_index.to_string(),
            ])
        ));
        Ok(Self {
            schema_version: PLANNING_SCHEMA_VERSION,
            plan_id,
            plan_revision_id,
            revision_index,
            repository_revision,
            discovery_impact_map_id,
            decision,
        })
    }

    pub(crate) fn validate_identity(&self) -> Result<(), PlanningContractError> {
        validate_plan_decision_hard_bounds(&self.decision)?;
        if self.schema_version != PLANNING_SCHEMA_VERSION
            || Self::new(
                self.revision_index,
                self.repository_revision.clone(),
                self.discovery_impact_map_id.clone(),
                self.decision.clone(),
            )? != *self
        {
            return Err(PlanningContractError::InvalidCandidate {
                code: "plan_candidate_identity_mismatch",
            });
        }
        Ok(())
    }

    pub(crate) fn validate_output_allowance(
        &self,
        output_token_allowance: u32,
    ) -> Result<(), PlanningContractError> {
        let serialized_bytes = canonical_json(self)?.len();
        // One UTF-8 byte per token is a conservative tokenizer-independent
        // ceiling. Provider adapters may prove a tighter model-specific bound,
        // but the protocol never persists a candidate that cannot fit this
        // signed allowance.
        if serialized_bytes > usize::try_from(output_token_allowance).unwrap_or(usize::MAX) {
            return Err(PlanningContractError::InvalidCandidate {
                code: "plan_candidate_output_allowance_exceeded",
            });
        }
        Ok(())
    }
}

fn canonicalize_plan_decision(
    decision: PlanDecisionCandidate,
) -> Result<PlanDecisionCandidate, PlanningContractError> {
    Ok(match decision {
        PlanDecisionCandidate::Changes { mut targets } => {
            let provider_ids = targets
                .iter()
                .map(|target| target.target_id.clone())
                .collect::<BTreeSet<_>>();
            if provider_ids.len() != targets.len() {
                return Err(PlanningContractError::InvalidCandidate {
                    code: "plan_provider_target_identity_ambiguous",
                });
            }
            let semantic_ids = targets
                .iter()
                .map(|target| Ok((target.target_id.clone(), derive_target_id(target)?)))
                .collect::<Result<BTreeMap<_, _>, PlanningContractError>>()?;
            for target in &mut targets {
                target.dependencies = target
                    .dependencies
                    .iter()
                    .map(|dependency| {
                        semantic_ids
                            .get(dependency)
                            .cloned()
                            .unwrap_or_else(|| dependency.clone())
                    })
                    .collect();
                target.target_id = semantic_ids
                    .get(&target.target_id)
                    .expect("provider target identity was indexed")
                    .clone();
                target.change_id = derive_change_id(target)?;
            }
            targets = canonical_target_order(&targets).unwrap_or_else(|| {
                targets.sort_by(|left, right| left.target_id.cmp(&right.target_id));
                targets
            });
            PlanDecisionCandidate::Changes { targets }
        }
        PlanDecisionCandidate::NoOp {
            mut criterion_satisfaction,
        } => {
            criterion_satisfaction
                .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
            PlanDecisionCandidate::NoOp {
                criterion_satisfaction,
            }
        }
        other => other,
    })
}

fn derive_target_id(target: &PlannedTargetV1) -> Result<TargetId, PlanningContractError> {
    let identity = canonical_json(&(
        PLANNING_SCHEMA_VERSION,
        &target.path,
        &target.operation,
        target.role,
        &target.acceptance_criteria,
        &target.required_evidence,
        &target.expected_validation,
    ))?;
    Ok(TargetId::new(format!(
        "epv1:{}",
        stable_sha256(&["execution-protocol-v1:plan-target", &identity])
    )))
}

fn derive_change_id(target: &PlannedTargetV1) -> Result<ChangeId, PlanningContractError> {
    let identity = canonical_json(&(
        PLANNING_SCHEMA_VERSION,
        &target.target_id,
        &target.rationale,
        &target.dependencies,
        &target.estimated_change,
    ))?;
    Ok(ChangeId::new(format!(
        "epv1:{}",
        stable_sha256(&["execution-protocol-v1:plan-change", &identity])
    )))
}

fn validate_plan_decision_hard_bounds(
    decision: &PlanDecisionCandidate,
) -> Result<(), PlanningContractError> {
    match decision {
        PlanDecisionCandidate::Changes { targets } => {
            validate_collection_limit("plan.targets", targets.len(), MAX_PLAN_TARGETS)?;
            for target in targets {
                validate_identifier("plan.target_id", target.target_id.as_str())?;
                validate_identifier("plan.change_id", target.change_id.as_str())?;
                validate_path_length("plan.target_path", target.path.as_str())?;
                validate_bounded_text("plan.rationale", &target.rationale)?;
                validate_collection_limit(
                    "plan.target.criteria",
                    target.acceptance_criteria.len(),
                    MAX_TARGET_CRITERIA,
                )?;
                validate_collection_limit(
                    "plan.target.evidence",
                    target.required_evidence.len(),
                    MAX_TARGET_EVIDENCE,
                )?;
                validate_collection_limit(
                    "plan.target.validation",
                    target.expected_validation.len(),
                    MAX_VALIDATION_EXPECTATIONS,
                )?;
                validate_collection_limit(
                    "plan.target.dependencies",
                    target.dependencies.len(),
                    MAX_TARGET_DEPENDENCIES,
                )?;
                for evidence_id in &target.required_evidence {
                    validate_identifier("plan.evidence_id", evidence_id.as_str())?;
                }
                for dependency_id in &target.dependencies {
                    validate_identifier("plan.dependency_id", dependency_id.as_str())?;
                }
                for expectation in &target.expected_validation {
                    validate_identifier(
                        "plan.validation_expectation_id",
                        expectation.expectation_id.as_str(),
                    )?;
                    validate_identifier(
                        "plan.validation_candidate_id",
                        expectation.command_candidate_id.as_str(),
                    )?;
                }
                match &target.operation {
                    TargetOperation::ModifyExisting {
                        expected_content_hash,
                    }
                    | TargetOperation::DeleteFile {
                        expected_content_hash,
                    } => validate_content_hash(expected_content_hash)?,
                    TargetOperation::MoveFile {
                        destination,
                        expected_content_hash,
                    } => {
                        validate_path_length("plan.move_destination", destination.as_str())?;
                        validate_content_hash(expected_content_hash)?;
                    }
                    TargetOperation::CreateFile { specification } => {
                        validate_bounded_text("plan.creation_purpose", &specification.purpose)?;
                    }
                }
            }
        }
        PlanDecisionCandidate::NoOp {
            criterion_satisfaction,
        } => {
            validate_collection_limit(
                "plan.criterion_satisfaction",
                criterion_satisfaction.len(),
                MAX_CRITERION_SATISFACTION_OBSERVATIONS,
            )?;
            for observation in criterion_satisfaction {
                validate_identifier(
                    "plan.satisfaction_observation_id",
                    observation.observation_id.as_str(),
                )?;
                validate_identifier(
                    "plan.satisfaction_authority_proof_id",
                    observation.authority.proof_id().as_str(),
                )?;
                validate_collection_limit(
                    "plan.satisfaction_evidence",
                    observation.supporting_evidence_ids.len(),
                    MAX_SATISFACTION_SUPPORTING_EVIDENCE,
                )?;
                for evidence_id in &observation.supporting_evidence_ids {
                    validate_identifier("plan.evidence_id", evidence_id.as_str())?;
                }
            }
        }
        PlanDecisionCandidate::EvidenceGap { criterion_ids, .. } => {
            if criterion_ids.is_empty() {
                return Err(PlanningContractError::InvalidCandidate {
                    code: "plan_evidence_gap_criteria_empty",
                });
            }
            validate_collection_limit(
                "plan.evidence_gap_criteria",
                criterion_ids.len(),
                MAX_CRITERION_SATISFACTION_OBSERVATIONS,
            )?;
        }
    }
    let serialized = canonical_json(decision)?;
    validate_collection_limit(
        "plan.candidate_bytes",
        serialized.len(),
        MAX_PLAN_CANDIDATE_BYTES,
    )
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "violation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PlanViolation {
    EmptyChangePlan,
    TooManyTargets,
    DuplicateTarget {
        target_id: TargetId,
    },
    DuplicateChange {
        change_id: ChangeId,
    },
    OverlappingTargetPath {
        path: ProfilePath,
    },
    RepositoryScopedTarget {
        target_id: TargetId,
        path: ProfilePath,
    },
    ExistingTargetEvidenceMissing {
        target_id: TargetId,
    },
    ExistingTargetHashMismatch {
        target_id: TargetId,
    },
    ExistingTargetEvidenceIncomplete {
        target_id: TargetId,
    },
    CreateTargetAlreadyExists {
        target_id: TargetId,
    },
    MoveDestinationExists {
        target_id: TargetId,
    },
    MoveSourceEqualsDestination {
        target_id: TargetId,
    },
    GeneratedTargetForbidden {
        target_id: TargetId,
        path: ProfilePath,
    },
    InvalidRationale {
        target_id: TargetId,
    },
    InvalidChangeEstimate {
        target_id: TargetId,
    },
    AcceptanceCriteriaMissing {
        target_id: TargetId,
    },
    UnknownCriterion {
        target_id: TargetId,
        criterion_id: DiscoveryCriterionId,
    },
    CriterionUncovered {
        criterion_id: DiscoveryCriterionId,
    },
    CriterionDoesNotGroundTarget {
        target_id: TargetId,
        criterion_id: DiscoveryCriterionId,
    },
    RequiredEvidenceMissing {
        target_id: TargetId,
    },
    UnknownEvidence {
        target_id: TargetId,
        evidence_id: EvidenceId,
    },
    EvidenceDoesNotGroundTarget {
        target_id: TargetId,
        evidence_id: EvidenceId,
    },
    ValidationExpectationMissing {
        target_id: TargetId,
    },
    UnknownValidationCandidate {
        target_id: TargetId,
        expectation_id: ValidationExpectationId,
    },
    InvalidValidationExpectation {
        target_id: TargetId,
        expectation_id: ValidationExpectationId,
    },
    ValidationCriterionUncovered {
        target_id: TargetId,
        criterion_id: DiscoveryCriterionId,
    },
    UnknownDependency {
        target_id: TargetId,
        dependency_id: TargetId,
    },
    SelfDependency {
        target_id: TargetId,
    },
    DependencyCycle,
    NoOpCriterionMissing {
        criterion_id: DiscoveryCriterionId,
    },
    NoOpSatisfactionObservationLimitExceeded,
    NoOpSatisfactionObservationDuplicate {
        criterion_id: DiscoveryCriterionId,
    },
    NoOpSatisfactionObservationInvalid {
        observation_id: EvidenceId,
    },
    NoOpSatisfactionRevisionMismatch {
        criterion_id: DiscoveryCriterionId,
    },
    NoOpEvidenceUnknown {
        criterion_id: DiscoveryCriterionId,
        evidence_id: EvidenceId,
    },
    NoOpEvidenceNotGrounded {
        criterion_id: DiscoveryCriterionId,
        evidence_id: EvidenceId,
    },
    NoOpSatisfactionProofUnavailable {
        criterion_id: DiscoveryCriterionId,
        proof_id: ProofId,
    },
    EvidenceGapReported {
        reason_code: PlanningEvidenceGapReason,
    },
    ImplementationNodeLimitExceeded,
    ValidationNodeLimitExceeded,
    TotalNodeLimitExceeded,
    NodeBudgetInfeasible,
    MissionBudgetInfeasible,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedPlan {
    pub(crate) schema_version: u16,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) discovery_impact_map_id: EvidenceId,
    pub(crate) targets: Vec<PlannedTargetV1>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedNoOp {
    pub(crate) schema_version: u16,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) discovery_impact_map_id: EvidenceId,
    pub(crate) criterion_satisfaction: Vec<CriterionSatisfactionObservation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PlanValidationResult {
    Accepted { plan: AcceptedPlan },
    AcceptedNoOp { no_op: AcceptedNoOp },
    Rejected { violations: BTreeSet<PlanViolation> },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanCandidateRecord {
    pub(crate) candidate: PlanCandidate,
    pub(crate) mission_capacity: PlanMissionCapacity,
    pub(crate) validation: PlanValidationResult,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "convergence", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PlanningConvergence {
    InsufficientEvidence { violations: BTreeSet<PlanViolation> },
    BudgetBlocked { violations: BTreeSet<PlanViolation> },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanningState {
    pub(crate) schema_version: u16,
    pub(crate) node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_profile_id: RepositoryProfileId,
    pub(crate) discovery_impact_map_id: EvidenceId,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
    pub(crate) candidate_records: Vec<PlanCandidateRecord>,
    pub(crate) accepted_plan: Option<AcceptedPlan>,
    pub(crate) accepted_no_op: Option<AcceptedNoOp>,
    pub(crate) convergence: Option<PlanningConvergence>,
}

impl PlanningState {
    pub(crate) fn new(
        node_id: NodeId,
        profile: &RepositoryProfile,
        discovery: &DiscoveryState,
    ) -> Result<Self, PlanningContractError> {
        let impact_map = accepted_impact_map(discovery)?;
        Ok(Self {
            schema_version: PLANNING_SCHEMA_VERSION,
            node_id,
            repository_revision: discovery.repository_revision.clone(),
            repository_profile_id: profile.profile_id.clone(),
            discovery_impact_map_id: impact_map.evidence_id.clone(),
            criterion_ids: discovery.goal.criterion_ids.clone(),
            candidate_records: Vec::new(),
            accepted_plan: None,
            accepted_no_op: None,
            convergence: None,
        })
    }

    pub(crate) fn next_revision_index(&self) -> u32 {
        u32::try_from(self.candidate_records.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1)
    }

    pub(crate) fn latest_violations(&self) -> BTreeSet<PlanViolation> {
        self.candidate_records
            .last()
            .and_then(|record| match &record.validation {
                PlanValidationResult::Rejected { violations } => Some(violations.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub(crate) fn validate(
        &self,
        profile: &RepositoryProfile,
        discovery: &DiscoveryState,
        graph_budget: &PlanGraphBudgetContract,
    ) -> Result<(), PlanningContractError> {
        graph_budget.validate()?;
        let expected = Self::new(self.node_id.clone(), profile, discovery)?;
        if self.schema_version != PLANNING_SCHEMA_VERSION
            || self.repository_revision != expected.repository_revision
            || self.repository_profile_id != expected.repository_profile_id
            || self.discovery_impact_map_id != expected.discovery_impact_map_id
            || self.criterion_ids != expected.criterion_ids
            || (self.accepted_plan.is_some() && self.accepted_no_op.is_some())
            || ((self.accepted_plan.is_some() || self.accepted_no_op.is_some())
                && self.convergence.is_some())
        {
            return Err(PlanningContractError::InvalidCandidate {
                code: "planning_state_binding_mismatch",
            });
        }
        for (index, record) in self.candidate_records.iter().enumerate() {
            record.candidate.validate_identity()?;
            if record.candidate.revision_index
                != u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1)
                || record.candidate.repository_revision != self.repository_revision
                || record.candidate.discovery_impact_map_id != self.discovery_impact_map_id
                || validate_plan_candidate(
                    &record.candidate,
                    profile,
                    discovery,
                    graph_budget,
                    record.mission_capacity,
                ) != record.validation
            {
                return Err(PlanningContractError::InvalidCandidate {
                    code: "planning_candidate_record_mismatch",
                });
            }
        }
        let latest = self.candidate_records.last();
        if self.accepted_plan.as_ref()
            != latest.and_then(|record| match &record.validation {
                PlanValidationResult::Accepted { plan } => Some(plan),
                _ => None,
            })
            || self.accepted_no_op.as_ref()
                != latest.and_then(|record| match &record.validation {
                    PlanValidationResult::AcceptedNoOp { no_op } => Some(no_op),
                    _ => None,
                })
        {
            return Err(PlanningContractError::InvalidCandidate {
                code: "planning_acceptance_projection_mismatch",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanningTool {
    RecordPlan,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "choice", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PlanningToolChoice {
    Named { tool: PlanningTool },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanningContextManifest {
    pub(crate) schema_version: u16,
    pub(crate) context_manifest_id: super::ContextManifestId,
    pub(crate) action_id: ActionId,
    pub(crate) node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_profile_id: RepositoryProfileId,
    pub(crate) discovery_impact_map_id: EvidenceId,
    pub(crate) plan_revision_index: u32,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
    pub(crate) evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) prior_candidate: Option<Box<PlanCandidate>>,
    pub(crate) prior_violations: BTreeSet<PlanViolation>,
    pub(crate) input_token_ceiling: u32,
    pub(crate) estimated_input_tokens: u32,
    pub(crate) materialized_context_hash: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanningActionEnvelope {
    pub(crate) schema_version: u16,
    pub(crate) action_id: ActionId,
    pub(crate) node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) context_manifest_id: super::ContextManifestId,
    pub(crate) repository_profile_id: RepositoryProfileId,
    pub(crate) discovery_impact_map_id: EvidenceId,
    pub(crate) plan_revision_index: u32,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
    pub(crate) evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) prior_candidate: Option<Box<PlanCandidate>>,
    pub(crate) prior_violations: BTreeSet<PlanViolation>,
    pub(crate) tools: BTreeSet<PlanningTool>,
    pub(crate) tool_choice: PlanningToolChoice,
    pub(crate) input_token_ceiling: u32,
    pub(crate) output_token_allowance: u32,
    pub(crate) budget_owner_node_id: NodeId,
    pub(crate) reservation_id: ReservationId,
    pub(crate) payload_identity: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedPlanningAction {
    pub(crate) context: PlanningContextManifest,
    pub(crate) envelope: PlanningActionEnvelope,
    pub(crate) admission: ModelCallAdmission,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlanningEffectRequest {
    DispatchProvider {
        envelope: Box<PlanningActionEnvelope>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanningActionRejectionReason {
    ProviderProtocolViolation,
    InvalidPlanObservation,
}

pub(crate) fn build_planning_context(
    state: &PlanningState,
    discovery: &DiscoveryState,
    action_id: ActionId,
    input_token_ceiling: u32,
) -> Result<PlanningContextManifest, PlanningContractError> {
    let impact_map = accepted_impact_map(discovery)?;
    if impact_map.evidence_id != state.discovery_impact_map_id {
        return Err(PlanningContractError::InvalidContext {
            code: "planning_impact_map_binding_mismatch",
        });
    }
    let evidence_ids = impact_map
        .areas
        .iter()
        .flat_map(|area| area.evidence_ids.iter().cloned())
        .collect::<BTreeSet<_>>();
    let prior_candidate = state
        .candidate_records
        .last()
        .map(|record| Box::new(record.candidate.clone()));
    let prior_violations = state.latest_violations();
    let prior_materialized = canonical_json(&(&prior_candidate, &prior_violations))?;
    let estimated_input_tokens = 320_u32
        .saturating_add(
            u32::try_from(state.criterion_ids.len())
                .unwrap_or(u32::MAX)
                .saturating_mul(48),
        )
        .saturating_add(
            u32::try_from(evidence_ids.len())
                .unwrap_or(u32::MAX)
                .saturating_mul(96),
        )
        .saturating_add(u32::try_from(prior_materialized.len().div_ceil(4)).unwrap_or(u32::MAX));
    if input_token_ceiling == 0 || estimated_input_tokens > input_token_ceiling {
        return Err(PlanningContractError::InvalidContext {
            code: "planning_context_token_ceiling_exceeded",
        });
    }
    let materialized = canonical_json(&(
        PLANNING_SCHEMA_VERSION,
        &action_id,
        &state.node_id,
        &state.repository_revision,
        &state.repository_profile_id,
        &state.discovery_impact_map_id,
        state.next_revision_index(),
        &state.criterion_ids,
        &evidence_ids,
        &prior_candidate,
        &prior_violations,
        input_token_ceiling,
        estimated_input_tokens,
    ))?;
    let materialized_context_hash = stable_sha256(&[
        "execution-protocol-v1:planning-materialized-context",
        &materialized,
    ]);
    let context_manifest_id = super::ContextManifestId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:planning-context-manifest",
            &materialized,
            &materialized_context_hash,
        ])
    ));
    Ok(PlanningContextManifest {
        schema_version: PLANNING_SCHEMA_VERSION,
        context_manifest_id,
        action_id,
        node_id: state.node_id.clone(),
        repository_revision: state.repository_revision.clone(),
        repository_profile_id: state.repository_profile_id.clone(),
        discovery_impact_map_id: state.discovery_impact_map_id.clone(),
        plan_revision_index: state.next_revision_index(),
        criterion_ids: state.criterion_ids.clone(),
        evidence_ids,
        prior_candidate,
        prior_violations,
        input_token_ceiling,
        estimated_input_tokens,
        materialized_context_hash,
    })
}

impl PlanningActionEnvelope {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        action_id: ActionId,
        node_id: NodeId,
        repository_revision: RepositoryRevisionId,
        context: &PlanningContextManifest,
        input_token_ceiling: u32,
        output_token_allowance: u32,
        budget_owner_node_id: NodeId,
        reservation_id: ReservationId,
    ) -> Result<Self, PlanningContractError> {
        let tools = BTreeSet::from([PlanningTool::RecordPlan]);
        let tool_choice = PlanningToolChoice::Named {
            tool: PlanningTool::RecordPlan,
        };
        let identity = canonical_json(&(
            PLANNING_SCHEMA_VERSION,
            &action_id,
            &node_id,
            &repository_revision,
            &context.context_manifest_id,
            &(
                &context.repository_profile_id,
                &context.discovery_impact_map_id,
                context.plan_revision_index,
                &context.criterion_ids,
                &context.evidence_ids,
                &context.prior_candidate,
                &context.prior_violations,
            ),
            &tools,
            &tool_choice,
            input_token_ceiling,
            output_token_allowance,
            &budget_owner_node_id,
            &reservation_id,
        ))?;
        let envelope = Self {
            schema_version: PLANNING_SCHEMA_VERSION,
            action_id,
            node_id,
            repository_revision,
            context_manifest_id: context.context_manifest_id.clone(),
            repository_profile_id: context.repository_profile_id.clone(),
            discovery_impact_map_id: context.discovery_impact_map_id.clone(),
            plan_revision_index: context.plan_revision_index,
            criterion_ids: context.criterion_ids.clone(),
            evidence_ids: context.evidence_ids.clone(),
            prior_candidate: context.prior_candidate.clone(),
            prior_violations: context.prior_violations.clone(),
            tools,
            tool_choice,
            input_token_ceiling,
            output_token_allowance,
            budget_owner_node_id,
            reservation_id,
            payload_identity: stable_sha256(&[
                "execution-protocol-v1:planning-provider-payload",
                &identity,
            ]),
        };
        envelope.validate_against_context(context)?;
        Ok(envelope)
    }

    pub(crate) fn validate_against_context(
        &self,
        context: &PlanningContextManifest,
    ) -> Result<(), PlanningContractError> {
        if self.schema_version != PLANNING_SCHEMA_VERSION
            || context.schema_version != PLANNING_SCHEMA_VERSION
            || self.action_id != context.action_id
            || self.node_id != context.node_id
            || self.repository_revision != context.repository_revision
            || self.context_manifest_id != context.context_manifest_id
            || self.repository_profile_id != context.repository_profile_id
            || self.discovery_impact_map_id != context.discovery_impact_map_id
            || self.plan_revision_index != context.plan_revision_index
            || self.criterion_ids != context.criterion_ids
            || self.evidence_ids != context.evidence_ids
            || self.prior_candidate != context.prior_candidate
            || self.prior_violations != context.prior_violations
            || self.tools != BTreeSet::from([PlanningTool::RecordPlan])
            || self.tool_choice
                != (PlanningToolChoice::Named {
                    tool: PlanningTool::RecordPlan,
                })
            || self.input_token_ceiling != context.input_token_ceiling
            || context.estimated_input_tokens > self.input_token_ceiling
            || self.output_token_allowance == 0
            || self.budget_owner_node_id != self.node_id
        {
            return Err(PlanningContractError::InvalidContext {
                code: "planning_provider_envelope_binding_mismatch",
            });
        }
        let expected = Self::new_unvalidated(
            self.action_id.clone(),
            self.node_id.clone(),
            self.repository_revision.clone(),
            context,
            self.input_token_ceiling,
            self.output_token_allowance,
            self.budget_owner_node_id.clone(),
            self.reservation_id.clone(),
        )?;
        if self.payload_identity != expected.payload_identity {
            return Err(PlanningContractError::InvalidContext {
                code: "planning_provider_payload_identity_mismatch",
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn new_unvalidated(
        action_id: ActionId,
        node_id: NodeId,
        repository_revision: RepositoryRevisionId,
        context: &PlanningContextManifest,
        input_token_ceiling: u32,
        output_token_allowance: u32,
        budget_owner_node_id: NodeId,
        reservation_id: ReservationId,
    ) -> Result<Self, PlanningContractError> {
        let tools = BTreeSet::from([PlanningTool::RecordPlan]);
        let tool_choice = PlanningToolChoice::Named {
            tool: PlanningTool::RecordPlan,
        };
        let identity = canonical_json(&(
            PLANNING_SCHEMA_VERSION,
            &action_id,
            &node_id,
            &repository_revision,
            &context.context_manifest_id,
            &(
                &context.repository_profile_id,
                &context.discovery_impact_map_id,
                context.plan_revision_index,
                &context.criterion_ids,
                &context.evidence_ids,
                &context.prior_candidate,
                &context.prior_violations,
            ),
            &tools,
            &tool_choice,
            input_token_ceiling,
            output_token_allowance,
            &budget_owner_node_id,
            &reservation_id,
        ))?;
        Ok(Self {
            schema_version: PLANNING_SCHEMA_VERSION,
            action_id,
            node_id,
            repository_revision,
            context_manifest_id: context.context_manifest_id.clone(),
            repository_profile_id: context.repository_profile_id.clone(),
            discovery_impact_map_id: context.discovery_impact_map_id.clone(),
            plan_revision_index: context.plan_revision_index,
            criterion_ids: context.criterion_ids.clone(),
            evidence_ids: context.evidence_ids.clone(),
            prior_candidate: context.prior_candidate.clone(),
            prior_violations: context.prior_violations.clone(),
            tools,
            tool_choice,
            input_token_ceiling,
            output_token_allowance,
            budget_owner_node_id,
            reservation_id,
            payload_identity: stable_sha256(&[
                "execution-protocol-v1:planning-provider-payload",
                &identity,
            ]),
        })
    }
}

impl PlanViolation {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::EmptyChangePlan => "plan_targets_empty",
            Self::TooManyTargets => "plan_target_limit_exceeded",
            Self::DuplicateTarget { .. } => "plan_target_duplicate",
            Self::DuplicateChange { .. } => "plan_change_duplicate",
            Self::OverlappingTargetPath { .. } => "plan_target_path_overlap",
            Self::RepositoryScopedTarget { .. } => "plan_target_repository_scoped",
            Self::ExistingTargetEvidenceMissing { .. } => "plan_existing_target_evidence_missing",
            Self::ExistingTargetHashMismatch { .. } => "plan_existing_target_hash_mismatch",
            Self::ExistingTargetEvidenceIncomplete { .. } => {
                "plan_existing_target_evidence_incomplete"
            }
            Self::CreateTargetAlreadyExists { .. } => "plan_create_target_exists",
            Self::MoveDestinationExists { .. } => "plan_move_destination_exists",
            Self::MoveSourceEqualsDestination { .. } => "plan_move_same_path",
            Self::GeneratedTargetForbidden { .. } => "plan_generated_target_forbidden",
            Self::InvalidRationale { .. } => "plan_rationale_invalid",
            Self::InvalidChangeEstimate { .. } => "plan_change_estimate_invalid",
            Self::AcceptanceCriteriaMissing { .. } => "plan_target_criteria_missing",
            Self::UnknownCriterion { .. } => "plan_target_criterion_unknown",
            Self::CriterionUncovered { .. } => "plan_criterion_uncovered",
            Self::CriterionDoesNotGroundTarget { .. } => "plan_target_criterion_not_grounded",
            Self::RequiredEvidenceMissing { .. } => "plan_target_evidence_missing",
            Self::UnknownEvidence { .. } => "plan_target_evidence_unknown",
            Self::EvidenceDoesNotGroundTarget { .. } => "plan_target_evidence_not_grounded",
            Self::ValidationExpectationMissing { .. } => "plan_validation_missing",
            Self::UnknownValidationCandidate { .. } => "plan_validation_candidate_unknown",
            Self::InvalidValidationExpectation { .. } => "plan_validation_expectation_invalid",
            Self::ValidationCriterionUncovered { .. } => "plan_validation_criterion_uncovered",
            Self::UnknownDependency { .. } => "plan_dependency_unknown",
            Self::SelfDependency { .. } => "plan_dependency_self",
            Self::DependencyCycle => "plan_dependency_cycle",
            Self::NoOpCriterionMissing { .. } => "plan_no_op_criterion_missing",
            Self::NoOpSatisfactionObservationLimitExceeded => {
                "plan_no_op_satisfaction_observation_limit_exceeded"
            }
            Self::NoOpSatisfactionObservationDuplicate { .. } => {
                "plan_no_op_satisfaction_observation_duplicate"
            }
            Self::NoOpSatisfactionObservationInvalid { .. } => {
                "plan_no_op_satisfaction_observation_invalid"
            }
            Self::NoOpSatisfactionRevisionMismatch { .. } => {
                "plan_no_op_satisfaction_revision_mismatch"
            }
            Self::NoOpEvidenceUnknown { .. } => "plan_no_op_evidence_unknown",
            Self::NoOpEvidenceNotGrounded { .. } => "plan_no_op_evidence_not_grounded",
            Self::NoOpSatisfactionProofUnavailable { .. } => {
                "plan_no_op_satisfaction_proof_unavailable"
            }
            Self::EvidenceGapReported { .. } => "plan_evidence_gap_reported",
            Self::ImplementationNodeLimitExceeded => "plan_implementation_node_limit_exceeded",
            Self::ValidationNodeLimitExceeded => "plan_validation_node_limit_exceeded",
            Self::TotalNodeLimitExceeded => "plan_total_node_limit_exceeded",
            Self::NodeBudgetInfeasible => "plan_node_budget_infeasible",
            Self::MissionBudgetInfeasible => "plan_mission_budget_infeasible",
        }
    }
}

pub(crate) fn validate_plan_candidate(
    candidate: &PlanCandidate,
    profile: &RepositoryProfile,
    discovery: &DiscoveryState,
    graph_budget: &PlanGraphBudgetContract,
    mission_capacity: PlanMissionCapacity,
) -> PlanValidationResult {
    let mut violations = BTreeSet::new();
    if candidate.validate_identity().is_err()
        || candidate.repository_revision != discovery.repository_revision
        || candidate.repository_revision != profile.repository_revision
        || discovery.repository_profile_id != profile.profile_id
        || candidate.discovery_impact_map_id
            != discovery
                .impact_map
                .as_ref()
                .map(|impact_map| impact_map.evidence_id.clone())
                .unwrap_or_else(|| EvidenceId::new("missing:impact-map"))
        || !matches!(
            discovery.convergence,
            Some(DiscoveryConvergence::ImpactMapAccepted { ref evidence_id })
                if evidence_id == &candidate.discovery_impact_map_id
        )
    {
        violations.insert(PlanViolation::EvidenceGapReported {
            reason_code: PlanningEvidenceGapReason::TargetEvidenceMissing,
        });
        return PlanValidationResult::Rejected { violations };
    }
    match &candidate.decision {
        PlanDecisionCandidate::Changes { targets } => {
            validate_change_targets(targets, profile, discovery, &mut violations);
            validate_graph_feasibility(targets, graph_budget, mission_capacity, &mut violations);
            if violations.is_empty() {
                let targets = canonical_target_order(targets).unwrap_or_else(|| targets.clone());
                PlanValidationResult::Accepted {
                    plan: AcceptedPlan {
                        schema_version: PLANNING_SCHEMA_VERSION,
                        plan_id: candidate.plan_id.clone(),
                        plan_revision_id: candidate.plan_revision_id.clone(),
                        repository_revision: candidate.repository_revision.clone(),
                        discovery_impact_map_id: candidate.discovery_impact_map_id.clone(),
                        targets,
                    },
                }
            } else {
                PlanValidationResult::Rejected { violations }
            }
        }
        PlanDecisionCandidate::NoOp {
            criterion_satisfaction,
        } => {
            validate_no_op(criterion_satisfaction, discovery, &mut violations);
            if violations.is_empty() {
                PlanValidationResult::AcceptedNoOp {
                    no_op: AcceptedNoOp {
                        schema_version: PLANNING_SCHEMA_VERSION,
                        plan_id: candidate.plan_id.clone(),
                        plan_revision_id: candidate.plan_revision_id.clone(),
                        repository_revision: candidate.repository_revision.clone(),
                        discovery_impact_map_id: candidate.discovery_impact_map_id.clone(),
                        criterion_satisfaction: criterion_satisfaction.clone(),
                    },
                }
            } else {
                PlanValidationResult::Rejected { violations }
            }
        }
        PlanDecisionCandidate::EvidenceGap { reason_code, .. } => {
            violations.insert(PlanViolation::EvidenceGapReported {
                reason_code: *reason_code,
            });
            PlanValidationResult::Rejected { violations }
        }
    }
}

fn validate_change_targets(
    targets: &[PlannedTargetV1],
    profile: &RepositoryProfile,
    discovery: &DiscoveryState,
    violations: &mut BTreeSet<PlanViolation>,
) {
    if targets.is_empty() {
        violations.insert(PlanViolation::EmptyChangePlan);
        return;
    }
    if targets.len() > MAX_PLAN_TARGETS {
        violations.insert(PlanViolation::TooManyTargets);
    }
    let target_ids = targets
        .iter()
        .map(|target| target.target_id.clone())
        .collect::<BTreeSet<_>>();
    let change_ids = targets
        .iter()
        .map(|target| target.change_id.clone())
        .collect::<BTreeSet<_>>();
    if target_ids.len() != targets.len() {
        for target in targets {
            if targets
                .iter()
                .filter(|other| other.target_id == target.target_id)
                .count()
                > 1
            {
                violations.insert(PlanViolation::DuplicateTarget {
                    target_id: target.target_id.clone(),
                });
            }
        }
    }
    if change_ids.len() != targets.len() {
        for target in targets {
            if targets
                .iter()
                .filter(|other| other.change_id == target.change_id)
                .count()
                > 1
            {
                violations.insert(PlanViolation::DuplicateChange {
                    change_id: target.change_id.clone(),
                });
            }
        }
    }
    let mut owned_paths = BTreeSet::new();
    let mut covered = BTreeSet::new();
    for target in targets {
        if !owned_paths.insert(target.path.clone()) {
            violations.insert(PlanViolation::OverlappingTargetPath {
                path: target.path.clone(),
            });
        }
        if let Some(destination) = target.operation.destination()
            && !owned_paths.insert(destination.clone())
        {
            violations.insert(PlanViolation::OverlappingTargetPath {
                path: destination.clone(),
            });
        }
        validate_target(target, &target_ids, profile, discovery, violations);
        covered.extend(target.acceptance_criteria.iter().cloned());
    }
    for criterion_id in discovery.goal.criterion_ids.difference(&covered) {
        violations.insert(PlanViolation::CriterionUncovered {
            criterion_id: criterion_id.clone(),
        });
    }
    if canonical_target_order(targets).is_none() {
        violations.insert(PlanViolation::DependencyCycle);
    }
}

fn validate_target(
    target: &PlannedTargetV1,
    target_ids: &BTreeSet<TargetId>,
    profile: &RepositoryProfile,
    discovery: &DiscoveryState,
    violations: &mut BTreeSet<PlanViolation>,
) {
    if target.path.is_root()
        || profile.source_roots.contains(&target.path)
        || profile.test_roots.contains(&target.path)
        || target
            .path
            .as_str()
            .bytes()
            .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'{' | b'}'))
    {
        violations.insert(PlanViolation::RepositoryScopedTarget {
            target_id: target.target_id.clone(),
            path: target.path.clone(),
        });
    }
    if target.rationale.trim() != target.rationale
        || target.rationale.is_empty()
        || target.rationale.len() > MAX_TEXT_BYTES
    {
        violations.insert(PlanViolation::InvalidRationale {
            target_id: target.target_id.clone(),
        });
    }
    if target.estimated_change.estimated_changed_lines == 0 {
        violations.insert(PlanViolation::InvalidChangeEstimate {
            target_id: target.target_id.clone(),
        });
    }
    if target.acceptance_criteria.is_empty()
        || target.acceptance_criteria.len() > MAX_TARGET_CRITERIA
    {
        violations.insert(PlanViolation::AcceptanceCriteriaMissing {
            target_id: target.target_id.clone(),
        });
    }
    for criterion_id in &target.acceptance_criteria {
        if !discovery.goal.criterion_ids.contains(criterion_id) {
            violations.insert(PlanViolation::UnknownCriterion {
                target_id: target.target_id.clone(),
                criterion_id: criterion_id.clone(),
            });
        } else if !target_criterion_is_grounded(target, criterion_id, discovery) {
            violations.insert(PlanViolation::CriterionDoesNotGroundTarget {
                target_id: target.target_id.clone(),
                criterion_id: criterion_id.clone(),
            });
        }
    }
    if target.required_evidence.is_empty() || target.required_evidence.len() > MAX_TARGET_EVIDENCE {
        violations.insert(PlanViolation::RequiredEvidenceMissing {
            target_id: target.target_id.clone(),
        });
    }
    let discovery_path = DiscoveryPath::new(target.path.as_str()).ok();
    for evidence_id in &target.required_evidence {
        if !evidence_is_known(discovery, evidence_id) {
            violations.insert(PlanViolation::UnknownEvidence {
                target_id: target.target_id.clone(),
                evidence_id: evidence_id.clone(),
            });
        } else if !evidence_grounds_target(
            discovery,
            evidence_id,
            discovery_path.as_ref(),
            &target.acceptance_criteria,
            matches!(target.operation, TargetOperation::CreateFile { .. }),
        ) {
            violations.insert(PlanViolation::EvidenceDoesNotGroundTarget {
                target_id: target.target_id.clone(),
                evidence_id: evidence_id.clone(),
            });
        }
    }
    if target.expected_validation.is_empty()
        || target.expected_validation.len() > MAX_VALIDATION_EXPECTATIONS
    {
        violations.insert(PlanViolation::ValidationExpectationMissing {
            target_id: target.target_id.clone(),
        });
    }
    let mut validation_criteria = BTreeSet::new();
    for expectation in &target.expected_validation {
        validation_criteria.extend(expectation.criterion_ids.iter().cloned());
        if expectation.validate_identity().is_err()
            || !expectation
                .criterion_ids
                .is_subset(&target.acceptance_criteria)
        {
            violations.insert(PlanViolation::InvalidValidationExpectation {
                target_id: target.target_id.clone(),
                expectation_id: expectation.expectation_id.clone(),
            });
        }
        if !profile
            .validation_candidates
            .iter()
            .any(|candidate| candidate.candidate_id == expectation.command_candidate_id)
        {
            violations.insert(PlanViolation::UnknownValidationCandidate {
                target_id: target.target_id.clone(),
                expectation_id: expectation.expectation_id.clone(),
            });
        }
    }
    for criterion_id in target.acceptance_criteria.difference(&validation_criteria) {
        violations.insert(PlanViolation::ValidationCriterionUncovered {
            target_id: target.target_id.clone(),
            criterion_id: criterion_id.clone(),
        });
    }
    if target.dependencies.len() > MAX_TARGET_DEPENDENCIES {
        violations.insert(PlanViolation::TooManyTargets);
    }
    for dependency_id in &target.dependencies {
        if dependency_id == &target.target_id {
            violations.insert(PlanViolation::SelfDependency {
                target_id: target.target_id.clone(),
            });
        } else if !target_ids.contains(dependency_id) {
            violations.insert(PlanViolation::UnknownDependency {
                target_id: target.target_id.clone(),
                dependency_id: dependency_id.clone(),
            });
        }
    }
    let existing_files = discovery_path.as_ref().map_or_else(Vec::new, |path| {
        discovery
            .file_evidence
            .values()
            .filter(|evidence| {
                &evidence.path == path
                    && evidence.repository_revision == discovery.repository_revision
            })
            .collect::<Vec<_>>()
    });
    match &target.operation {
        TargetOperation::CreateFile { specification } => {
            if validate_bounded_text("creation_purpose", &specification.purpose).is_err() {
                violations.insert(PlanViolation::InvalidRationale {
                    target_id: target.target_id.clone(),
                });
            }
            if !existing_files.is_empty() {
                violations.insert(PlanViolation::CreateTargetAlreadyExists {
                    target_id: target.target_id.clone(),
                });
            }
            let file_name = target.path.as_str().rsplit('/').next().unwrap_or_default();
            if !target.path.as_str().contains('/')
                && !file_name.contains('.')
                && specification.kind != CreatedFileKind::Metadata
            {
                violations.insert(PlanViolation::RepositoryScopedTarget {
                    target_id: target.target_id.clone(),
                    path: target.path.clone(),
                });
            }
        }
        TargetOperation::ModifyExisting {
            expected_content_hash,
        }
        | TargetOperation::DeleteFile {
            expected_content_hash,
        } => {
            validate_existing_target(target, expected_content_hash, &existing_files, violations);
        }
        TargetOperation::MoveFile {
            destination,
            expected_content_hash,
        } => {
            validate_existing_target(target, expected_content_hash, &existing_files, violations);
            if destination == &target.path {
                violations.insert(PlanViolation::MoveSourceEqualsDestination {
                    target_id: target.target_id.clone(),
                });
            }
            if destination.is_root()
                || profile.source_roots.contains(destination)
                || profile.test_roots.contains(destination)
            {
                violations.insert(PlanViolation::RepositoryScopedTarget {
                    target_id: target.target_id.clone(),
                    path: destination.clone(),
                });
            }
            if DiscoveryPath::new(destination.as_str())
                .ok()
                .is_some_and(|path| {
                    discovery
                        .file_evidence
                        .values()
                        .any(|evidence| evidence.path == path)
                })
            {
                violations.insert(PlanViolation::MoveDestinationExists {
                    target_id: target.target_id.clone(),
                });
            }
        }
    }
    for path in std::iter::once(&target.path).chain(target.operation.destination()) {
        if profile.generated_disposition(path) != super::GeneratedPathDisposition::OrdinarySource {
            violations.insert(PlanViolation::GeneratedTargetForbidden {
                target_id: target.target_id.clone(),
                path: path.clone(),
            });
        }
    }
}

fn validate_existing_target(
    target: &PlannedTargetV1,
    expected_content_hash: &str,
    existing_files: &[&super::FileEvidence],
    violations: &mut BTreeSet<PlanViolation>,
) {
    if existing_files.is_empty() {
        violations.insert(PlanViolation::ExistingTargetEvidenceMissing {
            target_id: target.target_id.clone(),
        });
        return;
    }
    let complete_files = existing_files
        .iter()
        .copied()
        .filter(|file| !file.truncated)
        .collect::<Vec<_>>();
    if complete_files.is_empty() {
        violations.insert(PlanViolation::ExistingTargetEvidenceIncomplete {
            target_id: target.target_id.clone(),
        });
        return;
    }
    let hash_matches = complete_files
        .iter()
        .copied()
        .filter(|file| file.content_hash == expected_content_hash)
        .collect::<Vec<_>>();
    if hash_matches.is_empty() {
        violations.insert(PlanViolation::ExistingTargetHashMismatch {
            target_id: target.target_id.clone(),
        });
        return;
    }
    if !hash_matches
        .iter()
        .any(|file| target.required_evidence.contains(&file.evidence_id))
    {
        violations.insert(PlanViolation::ExistingTargetEvidenceMissing {
            target_id: target.target_id.clone(),
        });
    }
}

fn validate_no_op(
    criterion_satisfaction: &[CriterionSatisfactionObservation],
    discovery: &DiscoveryState,
    violations: &mut BTreeSet<PlanViolation>,
) {
    if criterion_satisfaction.len() > MAX_CRITERION_SATISFACTION_OBSERVATIONS {
        violations.insert(PlanViolation::NoOpSatisfactionObservationLimitExceeded);
    }
    let mut observed_criteria = BTreeSet::new();
    for observation in criterion_satisfaction {
        if !observed_criteria.insert(observation.criterion_id.clone()) {
            violations.insert(PlanViolation::NoOpSatisfactionObservationDuplicate {
                criterion_id: observation.criterion_id.clone(),
            });
        }
        if !discovery
            .goal
            .criterion_ids
            .contains(&observation.criterion_id)
        {
            violations.insert(PlanViolation::NoOpCriterionMissing {
                criterion_id: observation.criterion_id.clone(),
            });
        }
        if observation.validate_identity().is_err() {
            violations.insert(PlanViolation::NoOpSatisfactionObservationInvalid {
                observation_id: observation.observation_id.clone(),
            });
        }
        if observation.repository_revision != discovery.repository_revision {
            violations.insert(PlanViolation::NoOpSatisfactionRevisionMismatch {
                criterion_id: observation.criterion_id.clone(),
            });
        }
        let authority_proof_id = observation.authority.proof_id();
        // Phase 3 has no authoritative criterion-satisfaction event or state.
        // A provider-authored reference must therefore fail closed even when
        // its ordinary discovery evidence is current and well grounded.
        violations.insert(PlanViolation::NoOpSatisfactionProofUnavailable {
            criterion_id: observation.criterion_id.clone(),
            proof_id: authority_proof_id.clone(),
        });
        for evidence_id in &observation.supporting_evidence_ids {
            if !evidence_is_known(discovery, evidence_id) {
                violations.insert(PlanViolation::NoOpEvidenceUnknown {
                    criterion_id: observation.criterion_id.clone(),
                    evidence_id: evidence_id.clone(),
                });
            } else if !impact_area_for_criterion(discovery, &observation.criterion_id)
                .is_some_and(|area| area.evidence_ids.contains(evidence_id))
            {
                violations.insert(PlanViolation::NoOpEvidenceNotGrounded {
                    criterion_id: observation.criterion_id.clone(),
                    evidence_id: evidence_id.clone(),
                });
            }
        }
    }
    for criterion_id in &discovery.goal.criterion_ids {
        if !observed_criteria.contains(criterion_id) {
            violations.insert(PlanViolation::NoOpCriterionMissing {
                criterion_id: criterion_id.clone(),
            });
        }
    }
}

fn evidence_is_known(discovery: &DiscoveryState, evidence_id: &EvidenceId) -> bool {
    discovery
        .completed_searches
        .values()
        .any(|item| &item.evidence_id == evidence_id)
        || discovery
            .candidates
            .values()
            .any(|item| &item.evidence_id == evidence_id)
        || discovery.file_evidence.contains_key(evidence_id)
        || discovery.relationships.contains_key(evidence_id)
        || discovery
            .impact_map
            .as_ref()
            .is_some_and(|item| &item.evidence_id == evidence_id)
}

fn evidence_grounds_target(
    discovery: &DiscoveryState,
    evidence_id: &EvidenceId,
    target_path: Option<&DiscoveryPath>,
    criteria: &BTreeSet<DiscoveryCriterionId>,
    is_creation: bool,
) -> bool {
    let admitted_for_claimed_criterion = criteria.iter().any(|criterion_id| {
        impact_area_for_criterion(discovery, criterion_id)
            .is_some_and(|area| area.evidence_ids.contains(evidence_id))
    });
    admitted_for_claimed_criterion
        && (is_creation
            || target_path.is_some_and(|path| {
                discovery.non_relationship_evidence_touches_path(evidence_id, path)
            })
            || discovery
                .relationships
                .get(evidence_id)
                .is_some_and(|relationship| {
                    target_path
                        .is_some_and(|path| relationship.from == *path || relationship.to == *path)
                }))
}

fn target_criterion_is_grounded(
    target: &PlannedTargetV1,
    criterion_id: &DiscoveryCriterionId,
    discovery: &DiscoveryState,
) -> bool {
    let Some(area) = impact_area_for_criterion(discovery, criterion_id) else {
        return false;
    };
    let evidence_intersection = target
        .required_evidence
        .iter()
        .any(|evidence_id| area.evidence_ids.contains(evidence_id));
    if !evidence_intersection {
        return false;
    }
    if matches!(target.operation, TargetOperation::CreateFile { .. }) {
        return true;
    }
    DiscoveryPath::new(target.path.as_str())
        .ok()
        .is_some_and(|path| area.paths.contains(&path))
}

fn impact_area_for_criterion<'a>(
    discovery: &'a DiscoveryState,
    criterion_id: &DiscoveryCriterionId,
) -> Option<&'a super::ImpactArea> {
    discovery
        .impact_map
        .as_ref()?
        .areas
        .iter()
        .find(|area| &area.criterion_id == criterion_id)
}

fn accepted_impact_map(
    discovery: &DiscoveryState,
) -> Result<&super::ImpactMapEvidence, PlanningContractError> {
    let impact_map =
        discovery
            .impact_map
            .as_ref()
            .ok_or(PlanningContractError::InvalidContext {
                code: "planning_impact_map_missing",
            })?;
    if !matches!(
        discovery.convergence,
        Some(DiscoveryConvergence::ImpactMapAccepted { ref evidence_id })
            if evidence_id == &impact_map.evidence_id
    ) {
        return Err(PlanningContractError::InvalidContext {
            code: "planning_impact_map_not_accepted",
        });
    }
    Ok(impact_map)
}

fn canonical_target_order(targets: &[PlannedTargetV1]) -> Option<Vec<PlannedTargetV1>> {
    let by_id = targets
        .iter()
        .cloned()
        .map(|target| (target.target_id.clone(), target))
        .collect::<BTreeMap<_, _>>();
    if by_id.len() != targets.len() {
        return None;
    }
    let mut remaining = by_id.keys().cloned().collect::<BTreeSet<_>>();
    let mut completed = BTreeSet::new();
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let next = remaining
            .iter()
            .find(|target_id| {
                by_id
                    .get(*target_id)
                    .is_some_and(|target| target.dependencies.is_subset(&completed))
            })
            .cloned()?;
        remaining.remove(&next);
        completed.insert(next.clone());
        ordered.push(by_id.get(&next).expect("canonical target exists").clone());
    }
    Some(ordered)
}

fn validate_graph_feasibility(
    targets: &[PlannedTargetV1],
    graph_budget: &PlanGraphBudgetContract,
    mission_capacity: PlanMissionCapacity,
    violations: &mut BTreeSet<PlanViolation>,
) {
    if graph_budget.validate().is_err() {
        violations.insert(PlanViolation::NodeBudgetInfeasible);
        return;
    }
    let implementation_count = u32::try_from(targets.len()).unwrap_or(u32::MAX);
    let validation_count = u32::try_from(
        targets
            .iter()
            .flat_map(|target| target.expected_validation.iter())
            .map(|expectation| &expectation.expectation_id)
            .collect::<BTreeSet<_>>()
            .len(),
    )
    .unwrap_or(u32::MAX);
    let total_count = implementation_count
        .saturating_add(validation_count)
        .saturating_add(3);
    if implementation_count > graph_budget.max_implementation_nodes {
        violations.insert(PlanViolation::ImplementationNodeLimitExceeded);
    }
    if validation_count > graph_budget.max_validation_nodes {
        violations.insert(PlanViolation::ValidationNodeLimitExceeded);
    }
    if total_count > graph_budget.max_total_nodes {
        violations.insert(PlanViolation::TotalNodeLimitExceeded);
    }
    let minimum_model_calls = implementation_count.saturating_add(2);
    let minimum_cost_micros = u64::from(minimum_model_calls);
    let minimum_duration_ms = u64::from(minimum_model_calls);
    if minimum_model_calls > mission_capacity.remaining_model_calls
        || minimum_cost_micros > mission_capacity.remaining_cost_micros
        || minimum_duration_ms > mission_capacity.remaining_duration_ms
    {
        violations.insert(PlanViolation::MissionBudgetInfeasible);
    }
}

fn model_node_budget_is_feasible(budget: &NodeBudgetContract, mutation: bool) -> bool {
    budget.max_model_calls > 0
        && budget.max_cost_micros > 0
        && budget.max_duration_ms > 0
        && budget.max_input_tokens_per_call > 0
        && budget.max_output_tokens_per_call > 0
        && (!mutation || budget.max_mutation_attempts > 0)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanGraphMaterialization {
    pub(crate) plan_id: PlanId,
    pub(crate) target_nodes: BTreeMap<TargetId, NodeId>,
    pub(crate) validation_nodes: BTreeMap<ValidationExpectationId, NodeId>,
    pub(crate) nodes: Vec<NodeSpec>,
}

pub(crate) fn materialize_accepted_plan(
    plan: &AcceptedPlan,
    graph_budget: &PlanGraphBudgetContract,
) -> Result<PlanGraphMaterialization, PlanningContractError> {
    graph_budget.validate()?;
    if u32::try_from(plan.targets.len()).unwrap_or(u32::MAX) > graph_budget.max_implementation_nodes
    {
        return Err(PlanningContractError::InvalidCandidate {
            code: "plan_implementation_node_limit_exceeded",
        });
    }
    let mut target_nodes = BTreeMap::new();
    for target in &plan.targets {
        target_nodes.insert(
            target.target_id.clone(),
            NodeId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:implementation-node",
                    plan.plan_id.as_str(),
                    target.target_id.as_str(),
                ])
            )),
        );
    }
    let mut nodes = Vec::new();
    for target in &plan.targets {
        let dependencies = target
            .dependencies
            .iter()
            .filter_map(|target_id| target_nodes.get(target_id).cloned())
            .collect::<Vec<_>>();
        nodes.push(NodeSpec {
            id: target_nodes
                .get(&target.target_id)
                .expect("target node identity exists")
                .clone(),
            kind: NodeKind::Implementation,
            required: true,
            dependencies,
            budget: graph_budget.implementation.clone(),
        });
    }
    let expectations = plan
        .targets
        .iter()
        .flat_map(|target| target.expected_validation.iter().cloned())
        .map(|expectation| (expectation.expectation_id.clone(), expectation))
        .collect::<BTreeMap<_, _>>();
    if expectations.is_empty() {
        return Err(PlanningContractError::InvalidCandidate {
            code: "plan_validation_missing",
        });
    }
    if u32::try_from(expectations.len()).unwrap_or(u32::MAX) > graph_budget.max_validation_nodes {
        return Err(PlanningContractError::InvalidCandidate {
            code: "plan_validation_node_limit_exceeded",
        });
    }
    let total_nodes = plan
        .targets
        .len()
        .saturating_add(expectations.len())
        .saturating_add(3);
    if u32::try_from(total_nodes).unwrap_or(u32::MAX) > graph_budget.max_total_nodes {
        return Err(PlanningContractError::InvalidCandidate {
            code: "plan_total_node_limit_exceeded",
        });
    }
    let implementation_ids = nodes.iter().map(|node| node.id.clone()).collect::<Vec<_>>();
    let mut validation_nodes = BTreeMap::new();
    for expectation_id in expectations.keys() {
        let node_id = NodeId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:validation-node",
                plan.plan_id.as_str(),
                expectation_id.as_str(),
            ])
        ));
        validation_nodes.insert(expectation_id.clone(), node_id.clone());
        nodes.push(NodeSpec {
            id: node_id,
            kind: NodeKind::Validation,
            required: true,
            dependencies: implementation_ids.clone(),
            budget: graph_budget.validation.clone(),
        });
    }
    let validation_ids = validation_nodes.values().cloned().collect::<Vec<_>>();
    let review = NodeId::new(format!(
        "epv1:{}",
        stable_sha256(&["execution-protocol-v1:review-node", plan.plan_id.as_str()])
    ));
    let completion = NodeId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:completion-node",
            plan.plan_id.as_str(),
        ])
    ));
    let publication = NodeId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:publication-node",
            plan.plan_id.as_str(),
        ])
    ));
    nodes.extend([
        NodeSpec {
            id: review.clone(),
            kind: NodeKind::Review,
            required: true,
            dependencies: validation_ids,
            budget: graph_budget.review.clone(),
        },
        NodeSpec {
            id: completion.clone(),
            kind: NodeKind::CompletionEvaluation,
            required: true,
            dependencies: vec![review],
            budget: graph_budget.completion_evaluation.clone(),
        },
        NodeSpec {
            id: publication,
            kind: NodeKind::Publication,
            required: true,
            dependencies: vec![completion],
            budget: graph_budget.publication.clone(),
        },
    ]);
    Ok(PlanGraphMaterialization {
        plan_id: plan.plan_id.clone(),
        target_nodes,
        validation_nodes,
        nodes,
    })
}

pub(crate) fn plan_accepted_proof_hash(plan: &AcceptedPlan) -> String {
    stable_sha256(&[
        "execution-protocol-v1:plan-accepted-proof",
        plan.plan_id.as_str(),
        plan.plan_revision_id.as_str(),
        plan.discovery_impact_map_id.as_str(),
    ])
}

pub(crate) fn no_op_satisfied_proof_hash(no_op: &AcceptedNoOp) -> String {
    let satisfaction_identity = no_op
        .criterion_satisfaction
        .iter()
        .map(|observation| observation.observation_id.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    stable_sha256(&[
        "execution-protocol-v1:no-op-satisfied-proof",
        no_op.plan_id.as_str(),
        no_op.plan_revision_id.as_str(),
        no_op.discovery_impact_map_id.as_str(),
        &satisfaction_identity,
    ])
}

fn validate_bounded_text(field: &'static str, value: &str) -> Result<(), PlanningContractError> {
    if value.is_empty()
        || value.trim() != value
        || value.len() > MAX_TEXT_BYTES
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(PlanningContractError::InvalidCandidate { code: field });
    }
    Ok(())
}

fn validate_collection_limit(
    field: &'static str,
    actual: usize,
    limit: usize,
) -> Result<(), PlanningContractError> {
    if actual > limit {
        return Err(PlanningContractError::LimitExceeded { field, limit });
    }
    Ok(())
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), PlanningContractError> {
    if value.is_empty()
        || value.trim() != value
        || value.len() > MAX_SEMANTIC_ID_BYTES
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(PlanningContractError::InvalidCandidate { code: field });
    }
    Ok(())
}

fn validate_path_length(field: &'static str, value: &str) -> Result<(), PlanningContractError> {
    if value.len() > MAX_PATH_BYTES {
        return Err(PlanningContractError::LimitExceeded {
            field,
            limit: MAX_PATH_BYTES,
        });
    }
    Ok(())
}

fn validate_content_hash(value: &str) -> Result<(), PlanningContractError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(PlanningContractError::InvalidCandidate {
            code: "plan_expected_content_hash_invalid",
        });
    }
    Ok(())
}

fn canonical_json<T: Serialize>(value: &T) -> Result<String, PlanningContractError> {
    serde_json::to_string(value).map_err(|_| PlanningContractError::Serialization)
}
