use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use super::{
    ActionId, ContextManifestId, EffectId, EvidenceId, ExecutionId, ExecutionNode,
    FailureRevisionId, ModelCallAdmission, ModelCallId, NodeId, NodeKind, NodeState,
    PlannedTargetV1, ProfilePath, RepositoryRevisionId, ReservationId, TargetContentSelection,
    TargetContextManifest, TargetExecutionPurpose, TargetId, TargetOperation, TextEncoding,
    stable_sha256,
};

pub(crate) const MUTATION_SCHEMA_VERSION: u16 = 1;
pub(crate) const MUTATION_UNCONTACTED_RELEASE_RETRY_LIMIT: u32 = 1;

const TOOL_SCHEMA_FIXED_TOKENS: u32 = 96;
const TOOL_CALL_FIXED_TOKENS: u32 = 128;
const PATCH_FIXED_TOKENS: u32 = 192;
const CONTENT_BYTES_PER_ESTIMATED_LINE: u64 = 96;
const MIN_CONTENT_CANDIDATE_BYTES: u64 = 256;
const JSON_WORST_CASE_EXPANSION: u64 = 6;
const SERIALIZED_BYTES_PER_TOKEN: u64 = 3;
const MAX_MUTATION_CANDIDATE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MutationContractError {
    Invalid {
        code: &'static str,
    },
    NoFeasibleStrategy,
    AttemptBudgetExhausted {
        attempted: u32,
        maximum: u32,
    },
    CandidateTooLarge {
        actual_bytes: u64,
        maximum_bytes: u64,
    },
    Serialization,
}

impl MutationContractError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::Invalid { code } => code,
            Self::NoFeasibleStrategy => "mutation_no_feasible_strategy",
            Self::AttemptBudgetExhausted { .. } => "mutation_attempt_budget_exhausted",
            Self::CandidateTooLarge { .. } => "mutation_candidate_too_large",
            Self::Serialization => "mutation_contract_serialization_failed",
        }
    }
}

impl fmt::Display for MutationContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { code } => write!(formatter, "mutation contract violates `{code}`"),
            Self::NoFeasibleStrategy => {
                formatter.write_str("no mutation strategy is feasible for the active target")
            }
            Self::AttemptBudgetExhausted { attempted, maximum } => write!(
                formatter,
                "mutation attempt {attempted} exceeds the signed maximum of {maximum}"
            ),
            Self::CandidateTooLarge {
                actual_bytes,
                maximum_bytes,
            } => write!(
                formatter,
                "mutation candidate has {actual_bytes} bytes; the bounded maximum is {maximum_bytes}"
            ),
            Self::Serialization => formatter.write_str("mutation identity serialization failed"),
        }
    }
}

impl std::error::Error for MutationContractError {}

macro_rules! mutation_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn new(value: impl Into<String>) -> Self {
                let value = value.into();
                assert!(
                    !value.trim().is_empty(),
                    concat!(stringify!($name), " must not be empty")
                );
                Self(value)
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

mutation_id!(MutationAttemptId);
mutation_id!(MutationCandidateId);
mutation_id!(MutationApplicationId);

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PatchMode {
    Initial,
    NormalizedRetry,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "strategy", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MutationStrategy {
    ApplyPatch { mode: PatchMode },
    ReplaceFile,
    CreateFile,
    DeleteFile,
    MoveFile,
}

impl MutationStrategy {
    pub(crate) const fn tool(self) -> MutationToolName {
        match self {
            Self::ApplyPatch { .. } => MutationToolName::ApplyPatch,
            Self::ReplaceFile => MutationToolName::ReplaceFile,
            Self::CreateFile => MutationToolName::CreateFile,
            Self::DeleteFile => MutationToolName::DeleteFile,
            Self::MoveFile => MutationToolName::MoveFile,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationToolName {
    ApplyPatch,
    ReplaceFile,
    CreateFile,
    DeleteFile,
    MoveFile,
}

impl MutationToolName {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ApplyPatch => "apply_patch",
            Self::ReplaceFile => "replace_file",
            Self::CreateFile => "create_file",
            Self::DeleteFile => "delete_file",
            Self::MoveFile => "move_file",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationFeasibilityReason {
    Feasible,
    IllegalForOperation,
    TargetContentUnavailable,
    InputContextTooLarge,
    OutputAllowanceInsufficient,
    AttemptBudgetExhausted,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationFeasibility {
    pub(crate) strategy: MutationStrategy,
    pub(crate) legal_for_operation: bool,
    pub(crate) target_size_bytes: u64,
    pub(crate) required_context_tokens: u32,
    pub(crate) worst_case_output_tokens: u32,
    pub(crate) serialized_tool_overhead_tokens: u32,
    pub(crate) output_allowance: u32,
    pub(crate) maximum_candidate_bytes: u64,
    pub(crate) context_fits: bool,
    pub(crate) output_fits: bool,
    pub(crate) reason: MutationFeasibilityReason,
}

impl MutationFeasibility {
    pub(crate) const fn is_feasible(&self) -> bool {
        self.legal_for_operation && self.context_fits && self.output_fits
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationFeasibilitySet {
    pub(crate) schema_version: u16,
    pub(crate) node_id: NodeId,
    pub(crate) node_attempt: u32,
    pub(crate) target_id: TargetId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) output_allowance: u32,
    pub(crate) evaluations: Vec<MutationFeasibility>,
    pub(crate) feasibility_hash: String,
}

impl MutationFeasibilitySet {
    pub(crate) fn feasible_strategies(&self) -> Vec<MutationStrategy> {
        self.evaluations
            .iter()
            .filter(|evaluation| evaluation.is_feasible())
            .map(|evaluation| evaluation.strategy)
            .collect()
    }

    pub(crate) fn evaluation(&self, strategy: MutationStrategy) -> Option<&MutationFeasibility> {
        self.evaluations
            .iter()
            .find(|evaluation| evaluation.strategy == strategy)
            .or_else(|| {
                matches!(
                    strategy,
                    MutationStrategy::ApplyPatch {
                        mode: PatchMode::NormalizedRetry
                    }
                )
                .then(|| {
                    self.evaluations.iter().find(|evaluation| {
                        evaluation.strategy
                            == MutationStrategy::ApplyPatch {
                                mode: PatchMode::Initial,
                            }
                    })
                })
                .flatten()
            })
    }

    pub(crate) fn validate(&self) -> Result<(), MutationContractError> {
        if self.schema_version != MUTATION_SCHEMA_VERSION
            || self.node_id.is_empty()
            || self.node_attempt == 0
            || self.target_id.is_empty()
            || self.context_manifest_id.is_empty()
            || self.repository_revision.is_empty()
            || self.output_allowance == 0
            || self.evaluations.is_empty()
            || self.evaluations.len() > 2
            || self.evaluations.windows(2).any(|pair| {
                canonical_strategy_rank(pair[0].strategy)
                    >= canonical_strategy_rank(pair[1].strategy)
            })
            || self.evaluations.iter().any(|evaluation| {
                evaluation.output_allowance != self.output_allowance
                    || (evaluation.reason == MutationFeasibilityReason::Feasible)
                        != evaluation.is_feasible()
            })
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_feasibility_set_invalid",
            });
        }
        if self.feasibility_hash != feasibility_hash(self)? {
            return Err(MutationContractError::Invalid {
                code: "mutation_feasibility_identity_mismatch",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationRecoveryKind {
    ModelRetry,
    StrategyFallback,
    RepositoryRebuild,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationRecoveryContext {
    pub(crate) kind: MutationRecoveryKind,
    pub(crate) prior_attempt_id: MutationAttemptId,
    pub(crate) failure_revision_id: FailureRevisionId,
    pub(crate) failure_class: MutationFailureClass,
    pub(crate) failure_detail_code: MutationFailureDetailCode,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationAttemptPolicy {
    pub(crate) schema_version: u16,
    pub(crate) attempt_id: MutationAttemptId,
    pub(crate) attempt_index: u32,
    pub(crate) execution_id: ExecutionId,
    pub(crate) execution_attempt: u32,
    pub(crate) node_id: NodeId,
    pub(crate) node_attempt: u32,
    pub(crate) target_id: TargetId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) permitted_strategies: Vec<MutationStrategy>,
    pub(crate) forced_strategy: Option<MutationStrategy>,
    pub(crate) prior_attempt_id: Option<MutationAttemptId>,
    pub(crate) recovery: Option<MutationRecoveryContext>,
    pub(crate) feasibility_hash: String,
}

impl MutationAttemptPolicy {
    pub(crate) fn permitted_tools(&self) -> Vec<MutationToolName> {
        self.permitted_strategies
            .iter()
            .map(|strategy| strategy.tool())
            .collect()
    }

    pub(crate) fn validate_against(
        &self,
        node: &ExecutionNode,
        target: &PlannedTargetV1,
        context: &TargetContextManifest,
        feasibility: &MutationFeasibilitySet,
    ) -> Result<(), MutationContractError> {
        validate_feasibility_active_binding(node, target, context, feasibility)?;
        validate_policy_feasibility_binding(self, feasibility)?;
        if self.schema_version != MUTATION_SCHEMA_VERSION
            || self.execution_id.is_empty()
            || self.execution_attempt == 0
            || self.attempt_index == 0
            || self.attempt_index > node.budget.max_mutation_attempts
            || self.node_id != node.id
            || self.node_attempt != context.node_attempt
            || self.target_id != target.target_id
            || self.context_manifest_id != context.context_manifest_id
            || self.repository_revision != context.repository_revision
            || self.feasibility_hash != feasibility.feasibility_hash
            || self.permitted_strategies.is_empty()
            || self.permitted_strategies.len() > 2
            || self
                .permitted_strategies
                .windows(2)
                .any(|pair| canonical_strategy_rank(pair[0]) >= canonical_strategy_rank(pair[1]))
            || self.permitted_strategies.iter().any(|strategy| {
                !feasibility
                    .evaluation(*strategy)
                    .is_some_and(MutationFeasibility::is_feasible)
            })
            || self
                .forced_strategy
                .is_some_and(|forced| self.permitted_strategies != [forced])
            || (self.attempt_index == 1 && self.prior_attempt_id.is_some())
            || (self.attempt_index > 1 && self.prior_attempt_id.is_none())
            || (self.attempt_index == 1 && self.recovery.is_some())
            || (self.attempt_index > 1
                && self.recovery.as_ref().is_none_or(|recovery| {
                    Some(&recovery.prior_attempt_id) != self.prior_attempt_id.as_ref()
                        || recovery.failure_revision_id.is_empty()
                }))
            || self.attempt_id != expected_attempt_id(self)?
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_attempt_policy_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryDriftRecovery {
    pub(crate) expected_revision: RepositoryRevisionId,
    pub(crate) observed_revision: RepositoryRevisionId,
    pub(crate) expected_fingerprint: String,
    pub(crate) observed_fingerprint: String,
    pub(crate) context_rebuild_required: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationFailureClass {
    ProviderProtocol,
    OutputTruncated,
    ToolMismatch,
    PathMismatch,
    CandidateSchemaInvalid,
    CandidateTooLarge,
    PatchMalformed,
    ApplyRejected,
    RepositoryDrift,
    OutsideOwnership,
    VerificationMismatch,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationRetryability {
    NoRetry,
    ModelRetry,
    SameTargetFallback,
    RebuildContext,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationFailureDetailCode {
    ProviderProtocolViolation,
    OutputTruncated,
    ToolNotPermitted,
    PathBindingMismatch,
    ExpectedHashMismatch,
    CandidateTooLarge,
    CandidateEncodingInvalid,
    ArtifactNotDurable,
    PatchMalformed,
    ApplyRejected,
    RepositoryDrift,
    OutsideOwnership,
    VerificationMismatch,
}

const fn expected_retryability(
    class: MutationFailureClass,
    detail: MutationFailureDetailCode,
) -> MutationRetryability {
    match class {
        MutationFailureClass::RepositoryDrift => MutationRetryability::RebuildContext,
        MutationFailureClass::OutputTruncated
        | MutationFailureClass::ProviderProtocol
        | MutationFailureClass::ToolMismatch
        | MutationFailureClass::PathMismatch
        | MutationFailureClass::CandidateTooLarge
        | MutationFailureClass::OutsideOwnership
        | MutationFailureClass::VerificationMismatch => MutationRetryability::NoRetry,
        MutationFailureClass::CandidateSchemaInvalid
            if matches!(detail, MutationFailureDetailCode::ArtifactNotDurable) =>
        {
            MutationRetryability::NoRetry
        }
        MutationFailureClass::CandidateSchemaInvalid => MutationRetryability::ModelRetry,
        MutationFailureClass::PatchMalformed | MutationFailureClass::ApplyRejected => {
            MutationRetryability::SameTargetFallback
        }
    }
}

const fn failure_detail_matches_class(
    class: MutationFailureClass,
    detail: MutationFailureDetailCode,
) -> bool {
    matches!(
        (class, detail),
        (
            MutationFailureClass::ProviderProtocol,
            MutationFailureDetailCode::ProviderProtocolViolation
        ) | (
            MutationFailureClass::OutputTruncated,
            MutationFailureDetailCode::OutputTruncated
        ) | (
            MutationFailureClass::ToolMismatch,
            MutationFailureDetailCode::ToolNotPermitted
        ) | (
            MutationFailureClass::PathMismatch,
            MutationFailureDetailCode::PathBindingMismatch
        ) | (
            MutationFailureClass::CandidateSchemaInvalid,
            MutationFailureDetailCode::ExpectedHashMismatch
                | MutationFailureDetailCode::CandidateEncodingInvalid
                | MutationFailureDetailCode::ArtifactNotDurable
        ) | (
            MutationFailureClass::CandidateTooLarge,
            MutationFailureDetailCode::CandidateTooLarge
        ) | (
            MutationFailureClass::PatchMalformed,
            MutationFailureDetailCode::PatchMalformed
        ) | (
            MutationFailureClass::ApplyRejected,
            MutationFailureDetailCode::ApplyRejected
        ) | (
            MutationFailureClass::RepositoryDrift,
            MutationFailureDetailCode::RepositoryDrift
        ) | (
            MutationFailureClass::OutsideOwnership,
            MutationFailureDetailCode::OutsideOwnership
        ) | (
            MutationFailureClass::VerificationMismatch,
            MutationFailureDetailCode::VerificationMismatch
        )
    )
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationFailure {
    pub(crate) schema_version: u16,
    pub(crate) failure_revision_id: FailureRevisionId,
    pub(crate) execution_id: ExecutionId,
    pub(crate) node_id: NodeId,
    pub(crate) target_id: TargetId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) attempt_id: MutationAttemptId,
    pub(crate) attempt_index: u32,
    pub(crate) strategy: Option<MutationStrategy>,
    pub(crate) candidate_id: Option<MutationCandidateId>,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) class: MutationFailureClass,
    pub(crate) retryability: MutationRetryability,
    pub(crate) detail_code: MutationFailureDetailCode,
    pub(crate) repository_drift: Option<RepositoryDriftRecovery>,
}

impl MutationFailure {
    pub(crate) fn new(
        policy: &MutationAttemptPolicy,
        strategy: Option<MutationStrategy>,
        candidate_id: Option<MutationCandidateId>,
        class: MutationFailureClass,
        detail_code: MutationFailureDetailCode,
        repository_drift: Option<RepositoryDriftRecovery>,
    ) -> Result<Self, MutationContractError> {
        let retryability = expected_retryability(class, detail_code);
        if (class == MutationFailureClass::RepositoryDrift) != repository_drift.is_some()
            || !failure_detail_matches_class(class, detail_code)
            || strategy.is_some_and(|strategy| !policy.permitted_strategies.contains(&strategy))
            || (candidate_id.is_some() && strategy.is_none())
            || (retryability == MutationRetryability::ModelRetry && strategy.is_none())
            || (class == MutationFailureClass::PatchMalformed
                && !matches!(strategy, Some(MutationStrategy::ApplyPatch { .. })))
            || matches!(
                class,
                MutationFailureClass::ApplyRejected
                    | MutationFailureClass::OutsideOwnership
                    | MutationFailureClass::VerificationMismatch
            ) && candidate_id.is_none()
            || repository_drift.as_ref().is_some_and(|drift| {
                !is_sha256(&drift.expected_fingerprint)
                    || !is_sha256(&drift.observed_fingerprint)
                    || !drift.context_rebuild_required
                    || drift.expected_revision != policy.repository_revision
                    || drift.expected_revision == drift.observed_revision
                    || drift.expected_fingerprint == drift.observed_fingerprint
            })
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_failure_invalid",
            });
        }
        let canonical = canonical_json(&(
            MUTATION_SCHEMA_VERSION,
            &policy.execution_id,
            &policy.node_id,
            &policy.target_id,
            &policy.context_manifest_id,
            &policy.attempt_id,
            policy.attempt_index,
            strategy,
            &candidate_id,
            &policy.repository_revision,
            class,
            retryability,
            detail_code,
            &repository_drift,
        ))?;
        Ok(Self {
            schema_version: MUTATION_SCHEMA_VERSION,
            failure_revision_id: FailureRevisionId::new(format!(
                "epv1:{}",
                stable_sha256(&["execution-protocol-v1:mutation-failure", &canonical])
            )),
            execution_id: policy.execution_id.clone(),
            node_id: policy.node_id.clone(),
            target_id: policy.target_id.clone(),
            context_manifest_id: policy.context_manifest_id.clone(),
            attempt_id: policy.attempt_id.clone(),
            attempt_index: policy.attempt_index,
            strategy,
            candidate_id,
            repository_revision: policy.repository_revision.clone(),
            class,
            retryability,
            detail_code,
            repository_drift,
        })
    }

    pub(crate) fn validate_against(
        &self,
        policy: &MutationAttemptPolicy,
        context: &TargetContextManifest,
    ) -> Result<(), MutationContractError> {
        self.validate_identity_against(policy)?;
        if self.context_manifest_id != context.context_manifest_id
            || self.repository_revision != context.repository_revision
            || self.repository_drift.as_ref().is_some_and(|drift| {
                drift.expected_revision != context.repository_revision
                    || drift.expected_fingerprint != context.repository_fingerprint
                    || drift.observed_revision == drift.expected_revision
                    || drift.observed_fingerprint == drift.expected_fingerprint
            })
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_failure_binding_invalid",
            });
        }
        Ok(())
    }

    fn validate_identity_against(
        &self,
        policy: &MutationAttemptPolicy,
    ) -> Result<(), MutationContractError> {
        let expected = Self::new(
            policy,
            self.strategy,
            self.candidate_id.clone(),
            self.class,
            self.detail_code,
            self.repository_drift.clone(),
        )?;
        if self != &expected
            || self.retryability != expected_retryability(self.class, self.detail_code)
            || !failure_detail_matches_class(self.class, self.detail_code)
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_failure_binding_invalid",
            });
        }
        Ok(())
    }
}

impl MutationRecoveryContext {
    fn from_failure(
        failure: &MutationFailure,
        kind: MutationRecoveryKind,
    ) -> Result<Self, MutationContractError> {
        let expected_retryability = match kind {
            MutationRecoveryKind::ModelRetry => MutationRetryability::ModelRetry,
            MutationRecoveryKind::StrategyFallback => MutationRetryability::SameTargetFallback,
            MutationRecoveryKind::RepositoryRebuild => MutationRetryability::RebuildContext,
        };
        if failure.retryability != expected_retryability {
            return Err(MutationContractError::Invalid {
                code: "mutation_recovery_context_retryability_mismatch",
            });
        }
        Ok(Self {
            kind,
            prior_attempt_id: failure.attempt_id.clone(),
            failure_revision_id: failure.failure_revision_id.clone(),
            failure_class: failure.class,
            failure_detail_code: failure.detail_code,
        })
    }
}

pub(crate) fn mutation_failure_matches_stage(
    failure: &MutationFailure,
    candidate: Option<&MutationCandidateRecord>,
    application: Option<&MutationApplicationObservation>,
) -> bool {
    match (&failure.candidate_id, candidate) {
        (None, None) => {
            application.is_none()
                && matches!(
                    failure.class,
                    MutationFailureClass::ProviderProtocol
                        | MutationFailureClass::OutputTruncated
                        | MutationFailureClass::ToolMismatch
                        | MutationFailureClass::PathMismatch
                        | MutationFailureClass::CandidateSchemaInvalid
                        | MutationFailureClass::CandidateTooLarge
                        | MutationFailureClass::PatchMalformed
                )
        }
        (Some(failure_candidate_id), Some(candidate))
            if failure_candidate_id == &candidate.candidate_id
                && failure.strategy == Some(candidate.strategy) =>
        {
            match failure.class {
                MutationFailureClass::PatchMalformed
                | MutationFailureClass::ApplyRejected
                | MutationFailureClass::RepositoryDrift => application.is_none(),
                MutationFailureClass::OutsideOwnership
                | MutationFailureClass::VerificationMismatch => application
                    .is_some_and(|application| application.candidate_id == candidate.candidate_id),
                MutationFailureClass::ProviderProtocol
                | MutationFailureClass::OutputTruncated
                | MutationFailureClass::ToolMismatch
                | MutationFailureClass::PathMismatch
                | MutationFailureClass::CandidateSchemaInvalid
                | MutationFailureClass::CandidateTooLarge => false,
            }
        }
        _ => false,
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MutationRecoveryDecision {
    ModelRetry {
        policy: MutationAttemptPolicy,
    },
    SelectFallback {
        policy: MutationAttemptPolicy,
    },
    RebuildContext {
        drift: RepositoryDriftRecovery,
    },
    NoSafeFallback {
        failure_revision_id: FailureRevisionId,
        reason: MutationConvergenceReason,
    },
}

pub(crate) fn evaluate_mutation_feasibility(
    node: &ExecutionNode,
    target: &PlannedTargetV1,
    context: &TargetContextManifest,
) -> Result<MutationFeasibilitySet, MutationContractError> {
    validate_active_binding(node, target, context)?;
    let strategies = legal_initial_strategies(&target.operation);
    let target_size_bytes = context
        .full_target_artifact
        .as_ref()
        .map_or(0, |artifact| artifact.byte_len);
    let output_allowance = node.budget.max_output_tokens_per_call;
    if output_allowance == 0 {
        return Err(MutationContractError::Invalid {
            code: "mutation_output_allowance_zero",
        });
    }
    let mut evaluations = Vec::with_capacity(strategies.len());
    for strategy in strategies {
        let required_context_tokens = context.estimated_input_tokens;
        let serialized_tool_overhead_tokens = estimate_tool_schema_tokens(strategy, target);
        let candidate_bytes = estimated_candidate_bytes(strategy, target, target_size_bytes);
        let candidate_tokens = estimate_serialized_candidate_tokens(candidate_bytes);
        let strategy_overhead = match strategy {
            MutationStrategy::ApplyPatch { .. } => PATCH_FIXED_TOKENS,
            _ => 0,
        };
        let worst_case_output_tokens = serialized_tool_overhead_tokens
            .saturating_add(TOOL_CALL_FIXED_TOKENS)
            .saturating_add(strategy_overhead)
            .saturating_add(candidate_tokens);
        let context_fits = required_context_tokens <= node.budget.max_input_tokens_per_call;
        let complete_target_available = !strategy_requires_complete_target(strategy)
            || matches!(
                &context.target_content,
                TargetContentSelection::FullFile { .. }
            );
        let maximum_candidate_bytes = max_candidate_bytes(
            output_allowance,
            serialized_tool_overhead_tokens
                .saturating_add(TOOL_CALL_FIXED_TOKENS)
                .saturating_add(strategy_overhead),
        );
        let output_fits = complete_target_available
            && candidate_bytes <= maximum_candidate_bytes
            && worst_case_output_tokens <= output_allowance;
        let reason = if !complete_target_available {
            MutationFeasibilityReason::TargetContentUnavailable
        } else if !context_fits {
            MutationFeasibilityReason::InputContextTooLarge
        } else if !output_fits {
            MutationFeasibilityReason::OutputAllowanceInsufficient
        } else {
            MutationFeasibilityReason::Feasible
        };
        evaluations.push(MutationFeasibility {
            strategy,
            legal_for_operation: true,
            target_size_bytes,
            required_context_tokens,
            worst_case_output_tokens,
            serialized_tool_overhead_tokens,
            output_allowance,
            maximum_candidate_bytes,
            context_fits,
            output_fits,
            reason,
        });
    }
    evaluations.sort_by_key(|evaluation| canonical_strategy_rank(evaluation.strategy));
    let mut set = MutationFeasibilitySet {
        schema_version: MUTATION_SCHEMA_VERSION,
        node_id: node.id.clone(),
        node_attempt: context.node_attempt,
        target_id: target.target_id.clone(),
        context_manifest_id: context.context_manifest_id.clone(),
        repository_revision: context.repository_revision.clone(),
        output_allowance,
        evaluations,
        feasibility_hash: String::new(),
    };
    set.feasibility_hash = feasibility_hash(&set)?;
    set.validate()?;
    Ok(set)
}

pub(crate) fn select_initial_mutation_policy(
    execution_id: &ExecutionId,
    execution_attempt: u32,
    node: &ExecutionNode,
    target: &PlannedTargetV1,
    context: &TargetContextManifest,
    feasibility: &MutationFeasibilitySet,
) -> Result<MutationAttemptPolicy, MutationContractError> {
    validate_feasibility_active_binding(node, target, context, feasibility)?;
    if node.usage.mutation_attempts != 0 {
        return Err(MutationContractError::Invalid {
            code: "initial_mutation_policy_after_prior_attempt",
        });
    }
    let permitted_strategies = feasibility.feasible_strategies();
    if permitted_strategies.is_empty() {
        return Err(MutationContractError::NoFeasibleStrategy);
    }
    let forced_strategy = (permitted_strategies.len() == 1).then_some(permitted_strategies[0]);
    build_attempt_policy(
        execution_id,
        execution_attempt,
        node,
        target,
        context,
        feasibility,
        1,
        permitted_strategies,
        forced_strategy,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn select_rebuilt_mutation_policy(
    execution_id: &ExecutionId,
    execution_attempt: u32,
    node: &ExecutionNode,
    target: &PlannedTargetV1,
    context: &TargetContextManifest,
    feasibility: &MutationFeasibilitySet,
    previous_context: &TargetContextManifest,
    previous: &MutationAttemptPolicy,
    failure: &MutationFailure,
) -> Result<MutationAttemptPolicy, MutationContractError> {
    validate_feasibility_active_binding(node, target, context, feasibility)?;
    failure.validate_against(previous, previous_context)?;
    let drift = failure
        .repository_drift
        .as_ref()
        .ok_or(MutationContractError::Invalid {
            code: "rebuilt_mutation_policy_without_repository_drift",
        })?;
    if previous.node_id != node.id
        || previous.target_id != target.target_id
        || previous.context_manifest_id != previous_context.context_manifest_id
        || previous.repository_revision != previous_context.repository_revision
        || previous_context.purpose != context.purpose
        || previous.repository_revision == context.repository_revision
        || previous.context_manifest_id == context.context_manifest_id
        || failure.retryability != MutationRetryability::RebuildContext
        || drift.expected_revision != previous_context.repository_revision
        || drift.expected_fingerprint != previous_context.repository_fingerprint
        || drift.observed_revision != context.repository_revision
        || drift.observed_fingerprint != context.repository_fingerprint
    {
        return Err(MutationContractError::Invalid {
            code: "rebuilt_mutation_policy_binding_invalid",
        });
    }
    let attempt_index = previous.attempt_index.saturating_add(1);
    let permitted_strategies = feasibility.feasible_strategies();
    if permitted_strategies.is_empty() {
        return Err(MutationContractError::NoFeasibleStrategy);
    }
    let forced_strategy = (permitted_strategies.len() == 1).then_some(permitted_strategies[0]);
    build_attempt_policy(
        execution_id,
        execution_attempt,
        node,
        target,
        context,
        feasibility,
        attempt_index,
        permitted_strategies,
        forced_strategy,
        Some(MutationRecoveryContext::from_failure(
            failure,
            MutationRecoveryKind::RepositoryRebuild,
        )?),
    )
}

pub(crate) fn select_mutation_recovery(
    node: &ExecutionNode,
    target: &PlannedTargetV1,
    context: &TargetContextManifest,
    feasibility: &MutationFeasibilitySet,
    previous: &MutationAttemptPolicy,
    failure: &MutationFailure,
) -> Result<MutationRecoveryDecision, MutationContractError> {
    previous.validate_against(node, target, context, feasibility)?;
    failure.validate_against(previous, context)?;
    if failure.attempt_id != previous.attempt_id
        || failure.node_id != node.id
        || failure.target_id != target.target_id
        || failure.context_manifest_id != context.context_manifest_id
    {
        return Err(MutationContractError::Invalid {
            code: "mutation_recovery_failure_binding_mismatch",
        });
    }
    let next_index = previous.attempt_index.saturating_add(1);
    if let Some(drift) = &failure.repository_drift {
        if next_index > node.budget.max_mutation_attempts {
            return Ok(MutationRecoveryDecision::NoSafeFallback {
                failure_revision_id: failure.failure_revision_id.clone(),
                reason: MutationConvergenceReason::MutationAttemptBudgetExhausted,
            });
        }
        if node.usage.context_rebuilds >= node.budget.max_context_rebuilds {
            return Ok(MutationRecoveryDecision::NoSafeFallback {
                failure_revision_id: failure.failure_revision_id.clone(),
                reason: MutationConvergenceReason::ContextRebuildBudgetExhausted,
            });
        }
        return Ok(MutationRecoveryDecision::RebuildContext {
            drift: drift.clone(),
        });
    }
    if failure.retryability == MutationRetryability::NoRetry {
        return Ok(MutationRecoveryDecision::NoSafeFallback {
            failure_revision_id: failure.failure_revision_id.clone(),
            reason: MutationConvergenceReason::NoSafeFallback,
        });
    }
    if next_index > node.budget.max_mutation_attempts {
        return Ok(MutationRecoveryDecision::NoSafeFallback {
            failure_revision_id: failure.failure_revision_id.clone(),
            reason: MutationConvergenceReason::MutationAttemptBudgetExhausted,
        });
    }
    if failure.retryability == MutationRetryability::ModelRetry {
        let Some(strategy) = failure.strategy else {
            return Ok(MutationRecoveryDecision::NoSafeFallback {
                failure_revision_id: failure.failure_revision_id.clone(),
                reason: MutationConvergenceReason::NoSafeFallback,
            });
        };
        let policy = build_attempt_policy(
            &previous.execution_id,
            previous.execution_attempt,
            node,
            target,
            context,
            feasibility,
            next_index,
            vec![strategy],
            Some(strategy),
            Some(MutationRecoveryContext::from_failure(
                failure,
                MutationRecoveryKind::ModelRetry,
            )?),
        )?;
        return Ok(MutationRecoveryDecision::ModelRetry { policy });
    }
    if failure.retryability != MutationRetryability::SameTargetFallback {
        return Err(MutationContractError::Invalid {
            code: "mutation_recovery_retryability_invalid",
        });
    }
    let prior_strategy = failure
        .strategy
        .or(previous.forced_strategy)
        .or_else(|| previous.permitted_strategies.first().copied());
    let fallback = match prior_strategy {
        Some(MutationStrategy::ApplyPatch { .. }) => feasibility
            .evaluation(MutationStrategy::ReplaceFile)
            .filter(|evaluation| evaluation.is_feasible())
            .map(|_| MutationStrategy::ReplaceFile)
            .or_else(|| {
                feasibility
                    .evaluation(MutationStrategy::ApplyPatch {
                        mode: PatchMode::Initial,
                    })
                    .filter(|evaluation| evaluation.is_feasible())
                    .map(|_| MutationStrategy::ApplyPatch {
                        mode: PatchMode::NormalizedRetry,
                    })
            }),
        _ => None,
    };
    let Some(fallback) = fallback else {
        return Ok(MutationRecoveryDecision::NoSafeFallback {
            failure_revision_id: failure.failure_revision_id.clone(),
            reason: MutationConvergenceReason::NoSafeFallback,
        });
    };
    let policy = build_attempt_policy(
        &previous.execution_id,
        previous.execution_attempt,
        node,
        target,
        context,
        feasibility,
        next_index,
        vec![fallback],
        Some(fallback),
        Some(MutationRecoveryContext::from_failure(
            failure,
            MutationRecoveryKind::StrategyFallback,
        )?),
    )?;
    Ok(MutationRecoveryDecision::SelectFallback { policy })
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderToolKind {
    Function,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JsonSchemaType {
    Object,
    String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationStringSchema {
    #[serde(rename = "type")]
    pub(crate) schema_type: JsonSchemaType,
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub(crate) enum_values: Option<Vec<String>>,
    #[serde(rename = "minLength", skip_serializing_if = "Option::is_none")]
    pub(crate) min_length: Option<u64>,
    #[serde(rename = "maxLength", skip_serializing_if = "Option::is_none")]
    pub(crate) max_length: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationObjectSchema {
    #[serde(rename = "type")]
    pub(crate) schema_type: JsonSchemaType,
    pub(crate) properties: BTreeMap<String, MutationStringSchema>,
    pub(crate) required: Vec<String>,
    #[serde(rename = "additionalProperties")]
    pub(crate) additional_properties: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationFunctionDefinition {
    pub(crate) name: MutationToolName,
    pub(crate) description: String,
    pub(crate) strict: bool,
    pub(crate) parameters: MutationObjectSchema,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationFunctionTool {
    #[serde(rename = "type")]
    pub(crate) tool_type: ProviderToolKind,
    pub(crate) function: MutationFunctionDefinition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MutationProviderToolChoice {
    Required,
    Named { tool: MutationToolName },
}

impl Serialize for MutationProviderToolChoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct NamedFunction {
            name: MutationToolName,
        }
        #[derive(Serialize)]
        struct NamedChoice {
            #[serde(rename = "type")]
            tool_type: ProviderToolKind,
            function: NamedFunction,
        }
        match self {
            Self::Required => serializer.serialize_str("required"),
            Self::Named { tool } => NamedChoice {
                tool_type: ProviderToolKind::Function,
                function: NamedFunction { name: *tool },
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for MutationProviderToolChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Choice {
            Keyword(String),
            Named(NamedChoice),
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NamedChoice {
            #[serde(rename = "type")]
            tool_type: ProviderToolKind,
            function: NamedFunction,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NamedFunction {
            name: MutationToolName,
        }
        match Choice::deserialize(deserializer)? {
            Choice::Keyword(keyword) if keyword == "required" => Ok(Self::Required),
            Choice::Keyword(_) => Err(serde::de::Error::custom(
                "unsupported mutation provider tool choice",
            )),
            Choice::Named(NamedChoice {
                tool_type: ProviderToolKind::Function,
                function,
            }) => Ok(Self::Named {
                tool: function.name,
            }),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationProviderRequestContract {
    pub(crate) schema_version: u16,
    pub(crate) action_id: ActionId,
    pub(crate) action_index: u32,
    pub(crate) prior_released_action_id: Option<ActionId>,
    pub(crate) call_id: ModelCallId,
    pub(crate) reservation_id: ReservationId,
    pub(crate) node_id: NodeId,
    pub(crate) node_attempt: u32,
    pub(crate) target_id: TargetId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) materialized_context_hash: String,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_fingerprint: String,
    pub(crate) attempt_id: MutationAttemptId,
    pub(crate) attempt_index: u32,
    pub(crate) permitted_strategies: Vec<MutationStrategy>,
    pub(crate) recovery: Option<MutationRecoveryContext>,
    pub(crate) tools: Vec<MutationFunctionTool>,
    pub(crate) tool_choice: MutationProviderToolChoice,
    pub(crate) parallel_tool_calls: bool,
    pub(crate) input_token_ceiling: u32,
    pub(crate) output_token_allowance: u32,
    pub(crate) budget_owner_node_id: NodeId,
}

impl MutationProviderRequestContract {
    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, MutationContractError> {
        serde_json::to_vec(self).map_err(|_| MutationContractError::Serialization)
    }

    pub(crate) fn payload_hash(&self) -> Result<String, MutationContractError> {
        let bytes = self.canonical_bytes()?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    pub(crate) fn tool_names(&self) -> Vec<MutationToolName> {
        self.tools.iter().map(|tool| tool.function.name).collect()
    }

    pub(crate) fn validate_against(
        &self,
        node: &ExecutionNode,
        target: &PlannedTargetV1,
        context: &TargetContextManifest,
        feasibility: &MutationFeasibilitySet,
        policy: &MutationAttemptPolicy,
    ) -> Result<(), MutationContractError> {
        policy.validate_against(node, target, context, feasibility)?;
        let expected_tools = provider_tools(target, feasibility, policy)?;
        let expected_choice = if policy.forced_strategy.is_some() {
            MutationProviderToolChoice::Named {
                tool: policy.permitted_strategies[0].tool(),
            }
        } else {
            MutationProviderToolChoice::Required
        };
        validate_action_chain_binding(self.action_index, self.prior_released_action_id.as_ref())?;
        let expected_action_id = mutation_action_id(
            policy,
            &context.context_manifest_id,
            self.action_index,
            self.prior_released_action_id.as_ref(),
        );
        let expected_call_id = mutation_call_id(&expected_action_id);
        let expected_reservation_id = mutation_reservation_id(&expected_call_id, &node.id);
        if self.schema_version != MUTATION_SCHEMA_VERSION
            || self.action_id != expected_action_id
            || self.call_id != expected_call_id
            || self.reservation_id != expected_reservation_id
            || self.node_id != node.id
            || self.node_attempt != context.node_attempt
            || self.target_id != target.target_id
            || self.context_manifest_id != context.context_manifest_id
            || self.materialized_context_hash != context.materialized_context_hash
            || self.repository_revision != context.repository_revision
            || self.repository_fingerprint != context.repository_fingerprint
            || self.attempt_id != policy.attempt_id
            || self.attempt_index != policy.attempt_index
            || self.permitted_strategies != policy.permitted_strategies
            || self.recovery != policy.recovery
            || self.tools != expected_tools
            || self.tool_choice != expected_choice
            || self.parallel_tool_calls
            || self.input_token_ceiling != node.budget.max_input_tokens_per_call
            || self.output_token_allowance != node.budget.max_output_tokens_per_call
            || self.budget_owner_node_id != node.id
            || !is_sha256(&self.materialized_context_hash)
            || !is_sha256(&self.repository_fingerprint)
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_provider_request_binding_mismatch",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedMutationAction {
    pub(crate) policy: MutationAttemptPolicy,
    pub(crate) action_index: u32,
    pub(crate) prior_released_action_id: Option<ActionId>,
    pub(crate) provider_request: MutationProviderRequestContract,
    pub(crate) admission: ModelCallAdmission,
}

impl PreparedMutationAction {
    pub(crate) fn validate_against(
        &self,
        node: &ExecutionNode,
        target: &PlannedTargetV1,
        context: &TargetContextManifest,
        feasibility: &MutationFeasibilitySet,
    ) -> Result<(), MutationContractError> {
        self.policy
            .validate_against(node, target, context, feasibility)?;
        self.provider_request
            .validate_against(node, target, context, feasibility, &self.policy)?;
        if self.action_index != self.provider_request.action_index
            || self.prior_released_action_id != self.provider_request.prior_released_action_id
            || self.admission.call_id != self.provider_request.call_id
            || self.admission.node_id != node.id
            || self.admission.action_id != self.provider_request.action_id
            || self.admission.payload_hash != self.provider_request.payload_hash()?
            || self.admission.input_tokens != context.estimated_input_tokens
            || self.admission.output_tokens != node.budget.max_output_tokens_per_call
        {
            return Err(MutationContractError::Invalid {
                code: "prepared_mutation_admission_mismatch",
            });
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_prepared_mutation_action(
    node: &ExecutionNode,
    target: &PlannedTargetV1,
    context: &TargetContextManifest,
    feasibility: &MutationFeasibilitySet,
    policy: MutationAttemptPolicy,
    reserved_cost_micros: u64,
    duration_allowance_ms: u64,
) -> Result<PreparedMutationAction, MutationContractError> {
    build_prepared_mutation_action_retry(
        node,
        target,
        context,
        feasibility,
        policy,
        1,
        None,
        reserved_cost_micros,
        duration_allowance_ms,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_prepared_mutation_action_retry(
    node: &ExecutionNode,
    target: &PlannedTargetV1,
    context: &TargetContextManifest,
    feasibility: &MutationFeasibilitySet,
    policy: MutationAttemptPolicy,
    action_index: u32,
    prior_released_action_id: Option<ActionId>,
    reserved_cost_micros: u64,
    duration_allowance_ms: u64,
) -> Result<PreparedMutationAction, MutationContractError> {
    policy.validate_against(node, target, context, feasibility)?;
    validate_action_chain_binding(action_index, prior_released_action_id.as_ref())?;
    if reserved_cost_micros == 0 || duration_allowance_ms == 0 {
        return Err(MutationContractError::Invalid {
            code: "mutation_reservation_allowance_invalid",
        });
    }
    let action_id = mutation_action_id(
        &policy,
        &context.context_manifest_id,
        action_index,
        prior_released_action_id.as_ref(),
    );
    let call_id = mutation_call_id(&action_id);
    let reservation_id = mutation_reservation_id(&call_id, &node.id);
    let provider_request = MutationProviderRequestContract {
        schema_version: MUTATION_SCHEMA_VERSION,
        action_id: action_id.clone(),
        action_index,
        prior_released_action_id: prior_released_action_id.clone(),
        call_id: call_id.clone(),
        reservation_id,
        node_id: node.id.clone(),
        node_attempt: context.node_attempt,
        target_id: target.target_id.clone(),
        context_manifest_id: context.context_manifest_id.clone(),
        materialized_context_hash: context.materialized_context_hash.clone(),
        repository_revision: context.repository_revision.clone(),
        repository_fingerprint: context.repository_fingerprint.clone(),
        attempt_id: policy.attempt_id.clone(),
        attempt_index: policy.attempt_index,
        permitted_strategies: policy.permitted_strategies.clone(),
        recovery: policy.recovery.clone(),
        tools: provider_tools(target, feasibility, &policy)?,
        tool_choice: if let Some(forced) = policy.forced_strategy {
            MutationProviderToolChoice::Named {
                tool: forced.tool(),
            }
        } else {
            MutationProviderToolChoice::Required
        },
        parallel_tool_calls: false,
        input_token_ceiling: node.budget.max_input_tokens_per_call,
        output_token_allowance: node.budget.max_output_tokens_per_call,
        budget_owner_node_id: node.id.clone(),
    };
    provider_request.validate_against(node, target, context, feasibility, &policy)?;
    let admission = ModelCallAdmission {
        call_id,
        node_id: node.id.clone(),
        action_id,
        payload_hash: provider_request.payload_hash()?,
        input_tokens: context.estimated_input_tokens,
        output_tokens: node.budget.max_output_tokens_per_call,
        reserved_cost_micros,
        duration_allowance_ms,
    };
    let prepared = PreparedMutationAction {
        policy,
        action_index,
        prior_released_action_id,
        provider_request,
        admission,
    };
    prepared.validate_against(node, target, context, feasibility)?;
    Ok(prepared)
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderOutputCompleteness {
    Complete,
    Truncated,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationArtifactHandle {
    pub(crate) content_address: String,
    pub(crate) store_locator_hash: String,
    pub(crate) persistence_receipt_hash: String,
}

impl MutationArtifactHandle {
    fn validate_for_artifact(
        &self,
        content_hash: &str,
        byte_len: u64,
        encoding: TextEncoding,
    ) -> bool {
        self.content_address == format!("sha256:{content_hash}")
            && is_sha256(&self.store_locator_hash)
            && is_sha256(&self.persistence_receipt_hash)
            && self.persistence_receipt_hash
                == expected_persistence_receipt_hash(
                    &self.content_address,
                    &self.store_locator_hash,
                    content_hash,
                    byte_len,
                    encoding,
                )
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DurableMutationArtifact {
    pub(crate) handle: MutationArtifactHandle,
    bytes: Vec<u8>,
}

impl DurableMutationArtifact {
    pub(crate) fn new(
        bytes: Vec<u8>,
        store_locator_hash: String,
    ) -> Result<Self, MutationContractError> {
        if !is_sha256(&store_locator_hash) || std::str::from_utf8(&bytes).is_err() {
            return Err(MutationContractError::Invalid {
                code: "durable_mutation_artifact_invalid",
            });
        }
        let content_hash = hex::encode(Sha256::digest(&bytes));
        let content_address = format!("sha256:{content_hash}");
        let byte_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let encoding = if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            TextEncoding::Utf8WithBom
        } else {
            TextEncoding::Utf8
        };
        Ok(Self {
            handle: MutationArtifactHandle {
                persistence_receipt_hash: expected_persistence_receipt_hash(
                    &content_address,
                    &store_locator_hash,
                    &content_hash,
                    byte_len,
                    encoding,
                ),
                content_address,
                store_locator_hash,
            },
            bytes,
        })
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn receipt(&self) -> MutationArtifactReceipt {
        let content_hash = hex::encode(Sha256::digest(&self.bytes));
        let encoding = if self.bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            TextEncoding::Utf8WithBom
        } else {
            TextEncoding::Utf8
        };
        MutationArtifactReceipt {
            handle: self.handle.clone(),
            content_hash,
            byte_len: u64::try_from(self.bytes.len()).unwrap_or(u64::MAX),
            encoding,
        }
    }

    fn validate(&self) -> bool {
        let content_hash = hex::encode(Sha256::digest(&self.bytes));
        let receipt = self.receipt();
        self.handle
            .validate_for_artifact(&content_hash, receipt.byte_len, receipt.encoding)
            && std::str::from_utf8(&self.bytes).is_ok()
    }
}

impl fmt::Debug for DurableMutationArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableMutationArtifact")
            .field("handle", &self.handle)
            .field("byte_len", &self.bytes.len())
            .field("content", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum MaterializedMutationArguments {
    ApplyPatch {
        path: ProfilePath,
        expected_content_hash: String,
        patch: DurableMutationArtifact,
        expected_after_content: DurableMutationArtifact,
    },
    ReplaceFile {
        path: ProfilePath,
        expected_content_hash: String,
        content: DurableMutationArtifact,
    },
    CreateFile {
        path: ProfilePath,
        content: DurableMutationArtifact,
    },
    DeleteFile {
        path: ProfilePath,
        expected_content_hash: String,
    },
    MoveFile {
        source_path: ProfilePath,
        destination_path: ProfilePath,
        expected_content_hash: String,
    },
}

impl MaterializedMutationArguments {
    pub(crate) const fn tool(&self) -> MutationToolName {
        match self {
            Self::ApplyPatch { .. } => MutationToolName::ApplyPatch,
            Self::ReplaceFile { .. } => MutationToolName::ReplaceFile,
            Self::CreateFile { .. } => MutationToolName::CreateFile,
            Self::DeleteFile { .. } => MutationToolName::DeleteFile,
            Self::MoveFile { .. } => MutationToolName::MoveFile,
        }
    }

    fn artifact_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::ApplyPatch { patch, .. } => Some(patch.bytes()),
            Self::ReplaceFile { content, .. } | Self::CreateFile { content, .. } => {
                Some(content.bytes())
            }
            Self::DeleteFile { .. } | Self::MoveFile { .. } => None,
        }
    }

    fn artifacts_are_durable(&self) -> bool {
        match self {
            Self::ApplyPatch {
                patch,
                expected_after_content,
                ..
            } => patch.validate() && expected_after_content.validate(),
            Self::ReplaceFile { content, .. } | Self::CreateFile { content, .. } => {
                content.validate()
            }
            Self::DeleteFile { .. } | Self::MoveFile { .. } => true,
        }
    }
}

impl fmt::Debug for MaterializedMutationArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (name, byte_len) = match self {
            Self::ApplyPatch { patch, .. } => ("ApplyPatch", Some(patch.bytes().len())),
            Self::ReplaceFile { content, .. } => ("ReplaceFile", Some(content.bytes().len())),
            Self::CreateFile { content, .. } => ("CreateFile", Some(content.bytes().len())),
            Self::DeleteFile { .. } => ("DeleteFile", None),
            Self::MoveFile { .. } => ("MoveFile", None),
        };
        formatter
            .debug_struct(name)
            .field("byte_len", &byte_len)
            .field("content", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MaterializedMutationInvocation {
    pub(crate) action_id: ActionId,
    pub(crate) call_id: ModelCallId,
    pub(crate) tool_call_count: u32,
    pub(crate) completeness: ProviderOutputCompleteness,
    pub(crate) arguments: MaterializedMutationArguments,
}

impl fmt::Debug for MaterializedMutationInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaterializedMutationInvocation")
            .field("action_id", &self.action_id)
            .field("call_id", &self.call_id)
            .field("tool_call_count", &self.tool_call_count)
            .field("completeness", &self.completeness)
            .field("tool", &self.arguments.tool())
            .field("arguments", &self.arguments)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationArtifactReceipt {
    pub(crate) handle: MutationArtifactHandle,
    pub(crate) content_hash: String,
    pub(crate) byte_len: u64,
    pub(crate) encoding: TextEncoding,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MutationCandidateOperation {
    ApplyPatch {
        path: ProfilePath,
        expected_content_hash: String,
        patch: MutationArtifactReceipt,
        expected_after_content: MutationArtifactReceipt,
    },
    ReplaceFile {
        path: ProfilePath,
        expected_content_hash: String,
        content: MutationArtifactReceipt,
    },
    CreateFile {
        path: ProfilePath,
        content: MutationArtifactReceipt,
    },
    DeleteFile {
        path: ProfilePath,
        expected_content_hash: String,
    },
    MoveFile {
        source_path: ProfilePath,
        destination_path: ProfilePath,
        expected_content_hash: String,
    },
}

impl MutationCandidateOperation {
    pub(crate) const fn tool(&self) -> MutationToolName {
        match self {
            Self::ApplyPatch { .. } => MutationToolName::ApplyPatch,
            Self::ReplaceFile { .. } => MutationToolName::ReplaceFile,
            Self::CreateFile { .. } => MutationToolName::CreateFile,
            Self::DeleteFile { .. } => MutationToolName::DeleteFile,
            Self::MoveFile { .. } => MutationToolName::MoveFile,
        }
    }

    pub(crate) fn owned_paths(&self) -> BTreeSet<ProfilePath> {
        match self {
            Self::ApplyPatch { path, .. }
            | Self::ReplaceFile { path, .. }
            | Self::CreateFile { path, .. }
            | Self::DeleteFile { path, .. } => BTreeSet::from([path.clone()]),
            Self::MoveFile {
                source_path,
                destination_path,
                ..
            } => BTreeSet::from([source_path.clone(), destination_path.clone()]),
        }
    }

    fn artifact(&self) -> Option<&MutationArtifactReceipt> {
        match self {
            Self::ApplyPatch { patch, .. } => Some(patch),
            Self::ReplaceFile { content, .. } | Self::CreateFile { content, .. } => Some(content),
            Self::DeleteFile { .. } | Self::MoveFile { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationCandidateRecord {
    pub(crate) schema_version: u16,
    pub(crate) candidate_id: MutationCandidateId,
    pub(crate) action_id: ActionId,
    pub(crate) call_id: ModelCallId,
    pub(crate) node_id: NodeId,
    pub(crate) target_id: TargetId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) attempt_id: MutationAttemptId,
    pub(crate) attempt_index: u32,
    pub(crate) strategy: MutationStrategy,
    pub(crate) complete: bool,
    pub(crate) truncated: bool,
    pub(crate) operation: MutationCandidateOperation,
    pub(crate) candidate_hash: String,
}

impl MutationCandidateRecord {
    pub(crate) fn validate_against(
        &self,
        prepared: &PreparedMutationAction,
        target: &PlannedTargetV1,
    ) -> Result<(), MutationContractError> {
        if self.schema_version != MUTATION_SCHEMA_VERSION
            || !prepared_request_matches_policy(prepared)
            || self.action_id != prepared.provider_request.action_id
            || self.call_id != prepared.provider_request.call_id
            || self.node_id != prepared.policy.node_id
            || self.target_id != prepared.policy.target_id
            || self.context_manifest_id != prepared.policy.context_manifest_id
            || self.repository_revision != prepared.policy.repository_revision
            || self.attempt_id != prepared.policy.attempt_id
            || self.attempt_index != prepared.policy.attempt_index
            || !prepared
                .policy
                .permitted_strategies
                .contains(&self.strategy)
            || self.operation.tool() != self.strategy.tool()
            || self.target_id != target.target_id
            || !candidate_operation_matches_target(&self.operation, target)
            || !self.complete
            || self.truncated
            || !candidate_artifacts_are_valid(&self.operation)
            || self.operation.artifact().is_some_and(|artifact| {
                prepared
                    .provider_request
                    .tools
                    .iter()
                    .find(|tool| tool.function.name == self.operation.tool())
                    .and_then(tool_candidate_max_length)
                    .is_some_and(|maximum| artifact.byte_len > maximum)
            })
            || self.candidate_hash != expected_candidate_hash(self)?
            || self.candidate_id != expected_candidate_id(self)?
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_candidate_record_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationCandidateRejectionReason {
    ProviderProtocolViolation,
    OutputTruncated,
    ToolNotPermitted,
    PathBindingMismatch,
    ExpectedHashMismatch,
    CandidateTooLarge,
    CandidateEncodingInvalid,
    ArtifactNotDurable,
}

impl MutationCandidateRejectionReason {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::ProviderProtocolViolation => "mutation_provider_protocol_violation",
            Self::OutputTruncated => "mutation_output_truncated",
            Self::ToolNotPermitted => "mutation_tool_not_permitted",
            Self::PathBindingMismatch => "mutation_path_binding_mismatch",
            Self::ExpectedHashMismatch => "mutation_expected_hash_mismatch",
            Self::CandidateTooLarge => "mutation_candidate_too_large",
            Self::CandidateEncodingInvalid => "mutation_candidate_encoding_invalid",
            Self::ArtifactNotDurable => "mutation_candidate_artifact_not_durable",
        }
    }

    pub(crate) const fn detail_code(self) -> MutationFailureDetailCode {
        match self {
            Self::ProviderProtocolViolation => MutationFailureDetailCode::ProviderProtocolViolation,
            Self::OutputTruncated => MutationFailureDetailCode::OutputTruncated,
            Self::ToolNotPermitted => MutationFailureDetailCode::ToolNotPermitted,
            Self::PathBindingMismatch => MutationFailureDetailCode::PathBindingMismatch,
            Self::ExpectedHashMismatch => MutationFailureDetailCode::ExpectedHashMismatch,
            Self::CandidateTooLarge => MutationFailureDetailCode::CandidateTooLarge,
            Self::CandidateEncodingInvalid => MutationFailureDetailCode::CandidateEncodingInvalid,
            Self::ArtifactNotDurable => MutationFailureDetailCode::ArtifactNotDurable,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MutationCandidateObservation {
    Accepted {
        candidate: MutationCandidateRecord,
    },
    Rejected {
        reason: MutationCandidateRejectionReason,
        failure: MutationFailure,
    },
}

pub(crate) fn record_mutation_candidate(
    prepared: &PreparedMutationAction,
    target: &PlannedTargetV1,
    invocation: &MaterializedMutationInvocation,
) -> Result<MutationCandidateObservation, MutationContractError> {
    let tool = invocation.arguments.tool();
    let strategy = prepared
        .policy
        .permitted_strategies
        .iter()
        .copied()
        .find(|strategy| strategy.tool() == tool);
    let rejection = if invocation.action_id != prepared.provider_request.action_id
        || invocation.call_id != prepared.provider_request.call_id
        || invocation.tool_call_count != 1
    {
        Some(MutationCandidateRejectionReason::ProviderProtocolViolation)
    } else if invocation.completeness == ProviderOutputCompleteness::Truncated {
        Some(MutationCandidateRejectionReason::OutputTruncated)
    } else if strategy.is_none() {
        Some(MutationCandidateRejectionReason::ToolNotPermitted)
    } else if !invocation_matches_target(&invocation.arguments, target) {
        Some(MutationCandidateRejectionReason::PathBindingMismatch)
    } else if !invocation_expected_hash_matches(&invocation.arguments, &target.operation) {
        Some(MutationCandidateRejectionReason::ExpectedHashMismatch)
    } else if !invocation.arguments.artifacts_are_durable() {
        Some(MutationCandidateRejectionReason::ArtifactNotDurable)
    } else if invocation
        .arguments
        .artifact_bytes()
        .is_some_and(|bytes| std::str::from_utf8(bytes).is_err())
    {
        Some(MutationCandidateRejectionReason::CandidateEncodingInvalid)
    } else if let (Some(strategy), Some(bytes)) = (strategy, invocation.arguments.artifact_bytes())
    {
        let maximum = prepared
            .provider_request
            .tools
            .iter()
            .find(|provider_tool| provider_tool.function.name == strategy.tool())
            .and_then(tool_candidate_max_length)
            .unwrap_or(0);
        (u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum)
            .then_some(MutationCandidateRejectionReason::CandidateTooLarge)
    } else {
        None
    };
    if let Some(reason) = rejection {
        let class = match reason {
            MutationCandidateRejectionReason::ProviderProtocolViolation => {
                MutationFailureClass::ProviderProtocol
            }
            MutationCandidateRejectionReason::OutputTruncated => {
                MutationFailureClass::OutputTruncated
            }
            MutationCandidateRejectionReason::ToolNotPermitted => {
                MutationFailureClass::ToolMismatch
            }
            MutationCandidateRejectionReason::PathBindingMismatch => {
                MutationFailureClass::PathMismatch
            }
            MutationCandidateRejectionReason::CandidateTooLarge => {
                MutationFailureClass::CandidateTooLarge
            }
            MutationCandidateRejectionReason::ExpectedHashMismatch
            | MutationCandidateRejectionReason::CandidateEncodingInvalid
            | MutationCandidateRejectionReason::ArtifactNotDurable => {
                MutationFailureClass::CandidateSchemaInvalid
            }
        };
        return Ok(MutationCandidateObservation::Rejected {
            reason,
            failure: MutationFailure::new(
                &prepared.policy,
                strategy,
                None,
                class,
                reason.detail_code(),
                None,
            )?,
        });
    }
    let strategy = strategy.expect("candidate admission established a permitted strategy");
    let operation = candidate_operation(&invocation.arguments);
    let mut candidate = MutationCandidateRecord {
        schema_version: MUTATION_SCHEMA_VERSION,
        candidate_id: MutationCandidateId::new("pending:mutation-candidate"),
        action_id: invocation.action_id.clone(),
        call_id: invocation.call_id.clone(),
        node_id: prepared.policy.node_id.clone(),
        target_id: prepared.policy.target_id.clone(),
        context_manifest_id: prepared.policy.context_manifest_id.clone(),
        repository_revision: prepared.policy.repository_revision.clone(),
        attempt_id: prepared.policy.attempt_id.clone(),
        attempt_index: prepared.policy.attempt_index,
        strategy,
        complete: true,
        truncated: false,
        operation,
        candidate_hash: String::new(),
    };
    candidate.candidate_hash = expected_candidate_hash(&candidate)?;
    candidate.candidate_id = expected_candidate_id(&candidate)?;
    candidate.validate_against(prepared, target)?;
    Ok(MutationCandidateObservation::Accepted { candidate })
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationApplyRequest {
    pub(crate) schema_version: u16,
    pub(crate) request_id: EffectId,
    pub(crate) application_id: MutationApplicationId,
    pub(crate) action_id: ActionId,
    pub(crate) call_id: ModelCallId,
    pub(crate) node_id: NodeId,
    pub(crate) target_id: TargetId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_fingerprint: String,
    pub(crate) attempt_id: MutationAttemptId,
    pub(crate) candidate_id: MutationCandidateId,
    pub(crate) candidate_hash: String,
    pub(crate) operation: MutationCandidateOperation,
    pub(crate) owned_paths: BTreeSet<ProfilePath>,
}

impl MutationApplyRequest {
    pub(crate) fn new(
        prepared: &PreparedMutationAction,
        candidate: &MutationCandidateRecord,
        target: &PlannedTargetV1,
        context: &TargetContextManifest,
    ) -> Result<Self, MutationContractError> {
        candidate.validate_against(prepared, target)?;
        if !prepared_request_matches_context(prepared, context) {
            return Err(MutationContractError::Invalid {
                code: "mutation_apply_context_binding_mismatch",
            });
        }
        let application_id = MutationApplicationId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:mutation-application",
                candidate.candidate_id.as_str(),
                &candidate.candidate_hash,
                context.repository_revision.as_str(),
            ])
        ));
        let request_id = EffectId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:mutation-apply-request",
                application_id.as_str(),
            ])
        ));
        let request = Self {
            schema_version: MUTATION_SCHEMA_VERSION,
            request_id,
            application_id,
            action_id: candidate.action_id.clone(),
            call_id: candidate.call_id.clone(),
            node_id: candidate.node_id.clone(),
            target_id: candidate.target_id.clone(),
            context_manifest_id: candidate.context_manifest_id.clone(),
            repository_revision: candidate.repository_revision.clone(),
            repository_fingerprint: context.repository_fingerprint.clone(),
            attempt_id: candidate.attempt_id.clone(),
            candidate_id: candidate.candidate_id.clone(),
            candidate_hash: candidate.candidate_hash.clone(),
            operation: candidate.operation.clone(),
            owned_paths: candidate.operation.owned_paths(),
        };
        request.validate_against(prepared, candidate, target, context)?;
        Ok(request)
    }

    pub(crate) fn validate_against(
        &self,
        prepared: &PreparedMutationAction,
        candidate: &MutationCandidateRecord,
        target: &PlannedTargetV1,
        context: &TargetContextManifest,
    ) -> Result<(), MutationContractError> {
        candidate.validate_against(prepared, target)?;
        let expected = Self::new_unvalidated(candidate, context);
        if !prepared_request_matches_context(prepared, context)
            || self != &expected
            || self.schema_version != MUTATION_SCHEMA_VERSION
            || self.owned_paths != candidate.operation.owned_paths()
            || !candidate_operation_matches_target(&self.operation, target)
            || !is_sha256(&self.repository_fingerprint)
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_apply_request_invalid",
            });
        }
        Ok(())
    }

    fn new_unvalidated(
        candidate: &MutationCandidateRecord,
        context: &TargetContextManifest,
    ) -> Self {
        let application_id = MutationApplicationId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:mutation-application",
                candidate.candidate_id.as_str(),
                &candidate.candidate_hash,
                context.repository_revision.as_str(),
            ])
        ));
        Self {
            schema_version: MUTATION_SCHEMA_VERSION,
            request_id: EffectId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:mutation-apply-request",
                    application_id.as_str(),
                ])
            )),
            application_id,
            action_id: candidate.action_id.clone(),
            call_id: candidate.call_id.clone(),
            node_id: candidate.node_id.clone(),
            target_id: candidate.target_id.clone(),
            context_manifest_id: candidate.context_manifest_id.clone(),
            repository_revision: candidate.repository_revision.clone(),
            repository_fingerprint: context.repository_fingerprint.clone(),
            attempt_id: candidate.attempt_id.clone(),
            candidate_id: candidate.candidate_id.clone(),
            candidate_hash: candidate.candidate_hash.clone(),
            operation: candidate.operation.clone(),
            owned_paths: candidate.operation.owned_paths(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationApplicationStatus {
    Applied,
    AlreadyApplied,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationApplicationObservation {
    pub(crate) schema_version: u16,
    pub(crate) request_id: EffectId,
    pub(crate) application_id: MutationApplicationId,
    pub(crate) candidate_id: MutationCandidateId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) status: MutationApplicationStatus,
}

impl MutationApplicationObservation {
    pub(crate) fn new(request: &MutationApplyRequest, status: MutationApplicationStatus) -> Self {
        Self {
            schema_version: MUTATION_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            application_id: request.application_id.clone(),
            candidate_id: request.candidate_id.clone(),
            repository_revision: request.repository_revision.clone(),
            status,
        }
    }

    pub(crate) fn validate_against(
        &self,
        request: &MutationApplyRequest,
    ) -> Result<(), MutationContractError> {
        if self.schema_version != MUTATION_SCHEMA_VERSION
            || self.request_id != request.request_id
            || self.application_id != request.application_id
            || self.candidate_id != request.candidate_id
            || self.repository_revision != request.repository_revision
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_application_observation_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationVerifyRequest {
    pub(crate) schema_version: u16,
    pub(crate) request_id: EffectId,
    pub(crate) application_id: MutationApplicationId,
    pub(crate) node_id: NodeId,
    pub(crate) target_id: TargetId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_fingerprint: String,
    pub(crate) candidate_id: MutationCandidateId,
    pub(crate) candidate_hash: String,
    pub(crate) operation: MutationCandidateOperation,
    pub(crate) owned_paths: BTreeSet<ProfilePath>,
}

impl MutationVerifyRequest {
    pub(crate) fn new(
        apply: &MutationApplyRequest,
        observation: &MutationApplicationObservation,
    ) -> Result<Self, MutationContractError> {
        observation.validate_against(apply)?;
        let request_id = EffectId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:mutation-verify-request",
                apply.application_id.as_str(),
                apply.candidate_id.as_str(),
            ])
        ));
        let request = Self {
            schema_version: MUTATION_SCHEMA_VERSION,
            request_id,
            application_id: apply.application_id.clone(),
            node_id: apply.node_id.clone(),
            target_id: apply.target_id.clone(),
            context_manifest_id: apply.context_manifest_id.clone(),
            repository_revision: apply.repository_revision.clone(),
            repository_fingerprint: apply.repository_fingerprint.clone(),
            candidate_id: apply.candidate_id.clone(),
            candidate_hash: apply.candidate_hash.clone(),
            operation: apply.operation.clone(),
            owned_paths: apply.owned_paths.clone(),
        };
        request.validate_against(apply, observation)?;
        Ok(request)
    }

    pub(crate) fn validate_against(
        &self,
        apply: &MutationApplyRequest,
        observation: &MutationApplicationObservation,
    ) -> Result<(), MutationContractError> {
        observation.validate_against(apply)?;
        let expected_id = EffectId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:mutation-verify-request",
                apply.application_id.as_str(),
                apply.candidate_id.as_str(),
            ])
        ));
        if self.schema_version != MUTATION_SCHEMA_VERSION
            || self.request_id != expected_id
            || self.application_id != apply.application_id
            || self.node_id != apply.node_id
            || self.target_id != apply.target_id
            || self.context_manifest_id != apply.context_manifest_id
            || self.repository_revision != apply.repository_revision
            || self.repository_fingerprint != apply.repository_fingerprint
            || self.candidate_id != apply.candidate_id
            || self.candidate_hash != apply.candidate_hash
            || self.operation != apply.operation
            || self.owned_paths != apply.owned_paths
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_verify_request_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MutationPathState {
    Absent,
    File {
        content_hash: String,
        byte_len: u64,
        encoding: TextEncoding,
    },
}

impl MutationPathState {
    fn content_hash(&self) -> Option<&str> {
        match self {
            Self::Absent => None,
            Self::File { content_hash, .. } => Some(content_hash),
        }
    }

    fn validate(&self) -> bool {
        match self {
            Self::Absent => true,
            Self::File { content_hash, .. } => is_sha256(content_hash),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationPathTransition {
    pub(crate) before: MutationPathState,
    pub(crate) after: MutationPathState,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MaterializedMutationVerification {
    pub(crate) request_id: EffectId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_fingerprint_before: String,
    pub(crate) repository_fingerprint_after: String,
    pub(crate) changed_paths: BTreeSet<ProfilePath>,
    pub(crate) path_transitions: BTreeMap<ProfilePath, MutationPathTransition>,
}

impl fmt::Debug for MaterializedMutationVerification {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaterializedMutationVerification")
            .field("request_id", &self.request_id)
            .field("repository_revision", &self.repository_revision)
            .field(
                "repository_fingerprint_before",
                &self.repository_fingerprint_before,
            )
            .field(
                "repository_fingerprint_after",
                &self.repository_fingerprint_after,
            )
            .field("changed_paths", &self.changed_paths)
            .field("path_transitions", &self.path_transitions)
            .field("raw_repository_content", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationVerificationEvidence {
    pub(crate) schema_version: u16,
    pub(crate) evidence_id: EvidenceId,
    pub(crate) verification_request_id: EffectId,
    pub(crate) application_id: MutationApplicationId,
    pub(crate) node_id: NodeId,
    pub(crate) target_id: TargetId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) attempt_id: MutationAttemptId,
    pub(crate) candidate_id: MutationCandidateId,
    pub(crate) repository_revision_before: RepositoryRevisionId,
    pub(crate) repository_revision_after: RepositoryRevisionId,
    pub(crate) repository_fingerprint_before: String,
    pub(crate) repository_fingerprint_after: String,
    pub(crate) changed_paths: BTreeSet<ProfilePath>,
    pub(crate) path_transitions: BTreeMap<ProfilePath, MutationPathTransition>,
    pub(crate) detail_hash: String,
}

impl MutationVerificationEvidence {
    pub(crate) fn validate(&self) -> Result<(), MutationContractError> {
        if self.schema_version != MUTATION_SCHEMA_VERSION
            || self.node_id.is_empty()
            || self.target_id.is_empty()
            || self.context_manifest_id.is_empty()
            || self.repository_revision_before == self.repository_revision_after
            || !is_sha256(&self.repository_fingerprint_before)
            || !is_sha256(&self.repository_fingerprint_after)
            || self.repository_fingerprint_before == self.repository_fingerprint_after
            || self.changed_paths.is_empty()
            || self.changed_paths
                != self
                    .path_transitions
                    .iter()
                    .filter(|(_, transition)| transition.before != transition.after)
                    .map(|(path, _)| path.clone())
                    .collect()
            || self
                .path_transitions
                .values()
                .any(|transition| !transition.before.validate() || !transition.after.validate())
            || self.repository_revision_after
                != derive_repository_revision(
                    &self.repository_revision_before,
                    &self.repository_fingerprint_after,
                    &self.candidate_id,
                )
            || self.detail_hash != expected_verification_detail_hash(self)?
            || self.evidence_id != expected_verification_evidence_id(self)?
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_verification_evidence_invalid",
            });
        }
        Ok(())
    }
}

pub(crate) fn verify_mutation_application(
    request: &MutationVerifyRequest,
    apply: &MutationApplyRequest,
    application: &MutationApplicationObservation,
    candidate: &MutationCandidateRecord,
    target: &PlannedTargetV1,
    materialized: &MaterializedMutationVerification,
) -> Result<MutationVerificationEvidence, MutationContractError> {
    request.validate_against(apply, application)?;
    if candidate.candidate_id != request.candidate_id
        || candidate.candidate_hash != request.candidate_hash
        || candidate.operation != request.operation
        || !candidate_operation_matches_target(&candidate.operation, target)
        || materialized.request_id != request.request_id
        || materialized.repository_revision != request.repository_revision
        || materialized.repository_fingerprint_before != request.repository_fingerprint
        || !is_sha256(&materialized.repository_fingerprint_after)
        || materialized.repository_fingerprint_after == request.repository_fingerprint
        || materialized.changed_paths != request.owned_paths
        || materialized
            .path_transitions
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            != request.owned_paths
        || materialized.path_transitions.values().any(|transition| {
            !transition.before.validate()
                || !transition.after.validate()
                || transition.before == transition.after
        })
        || !operation_transition_is_verified(&candidate.operation, &materialized.path_transitions)
    {
        return Err(MutationContractError::Invalid {
            code: "mutation_verification_observation_invalid",
        });
    }
    let repository_revision_after = derive_repository_revision(
        &request.repository_revision,
        &materialized.repository_fingerprint_after,
        &candidate.candidate_id,
    );
    let mut evidence = MutationVerificationEvidence {
        schema_version: MUTATION_SCHEMA_VERSION,
        evidence_id: EvidenceId::new("pending:mutation-verification"),
        verification_request_id: request.request_id.clone(),
        application_id: request.application_id.clone(),
        node_id: request.node_id.clone(),
        target_id: request.target_id.clone(),
        context_manifest_id: request.context_manifest_id.clone(),
        attempt_id: candidate.attempt_id.clone(),
        candidate_id: candidate.candidate_id.clone(),
        repository_revision_before: request.repository_revision.clone(),
        repository_revision_after,
        repository_fingerprint_before: request.repository_fingerprint.clone(),
        repository_fingerprint_after: materialized.repository_fingerprint_after.clone(),
        changed_paths: materialized.changed_paths.clone(),
        path_transitions: materialized.path_transitions.clone(),
        detail_hash: String::new(),
    };
    evidence.detail_hash = expected_verification_detail_hash(&evidence)?;
    evidence.evidence_id = expected_verification_evidence_id(&evidence)?;
    evidence.validate()?;
    Ok(evidence)
}

pub(crate) fn derive_repository_revision(
    before: &RepositoryRevisionId,
    after_fingerprint: &str,
    candidate_id: &MutationCandidateId,
) -> RepositoryRevisionId {
    RepositoryRevisionId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:repository-revision-after-mutation",
            before.as_str(),
            after_fingerprint,
            candidate_id.as_str(),
        ])
    ))
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationConvergenceReason {
    NoSafeFallback,
    ContextRebuildUnavailable,
    MutationAttemptBudgetExhausted,
    ContextRebuildBudgetExhausted,
}

impl MutationConvergenceReason {
    #[allow(non_upper_case_globals)]
    pub(crate) const AttemptBudgetExhausted: Self = Self::MutationAttemptBudgetExhausted;
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationConvergence {
    pub(crate) schema_version: u16,
    pub(crate) convergence_id: EvidenceId,
    pub(crate) node_id: NodeId,
    pub(crate) target_id: TargetId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_revision_after: RepositoryRevisionId,
    pub(crate) repository_drift: Option<RepositoryDriftRecovery>,
    pub(crate) final_attempt_id: MutationAttemptId,
    pub(crate) final_attempt_index: u32,
    pub(crate) last_failure_revision_id: FailureRevisionId,
    pub(crate) reason: MutationConvergenceReason,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationAdmissionBudgetDimension {
    ModelCalls,
    CostMicros,
    DurationMs,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationAdmissionBudgetRemaining {
    pub(crate) model_calls: u32,
    pub(crate) cost_micros: u64,
    pub(crate) duration_ms: u64,
}

impl MutationAdmissionBudgetRemaining {
    pub(crate) const fn new(model_calls: u32, cost_micros: u64, duration_ms: u64) -> Self {
        Self {
            model_calls,
            cost_micros,
            duration_ms,
        }
    }

    pub(crate) fn exhausted_dimensions(&self) -> BTreeSet<MutationAdmissionBudgetDimension> {
        let mut dimensions = BTreeSet::new();
        if self.model_calls == 0 {
            dimensions.insert(MutationAdmissionBudgetDimension::ModelCalls);
        }
        if self.cost_micros == 0 {
            dimensions.insert(MutationAdmissionBudgetDimension::CostMicros);
        }
        if self.duration_ms == 0 {
            dimensions.insert(MutationAdmissionBudgetDimension::DurationMs);
        }
        dimensions
    }

    pub(crate) fn is_exhausted(&self) -> bool {
        !self.exhausted_dimensions().is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MutationReadinessConvergenceReason {
    NoFeasibleStrategy,
    AdmissionBudgetExhausted {
        remaining: MutationAdmissionBudgetRemaining,
        exhausted_dimensions: BTreeSet<MutationAdmissionBudgetDimension>,
    },
    UncontactedActionRetryExhausted {
        released_actions: u32,
        maximum_actions: u32,
        last_released_action_id: ActionId,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationReadinessConvergence {
    pub(crate) schema_version: u16,
    pub(crate) convergence_id: EvidenceId,
    pub(crate) failure_revision_id: FailureRevisionId,
    pub(crate) execution_id: ExecutionId,
    pub(crate) execution_attempt: u32,
    pub(crate) node_id: NodeId,
    pub(crate) node_attempt: u32,
    pub(crate) target_id: TargetId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) feasibility_hash: String,
    pub(crate) attempt_id: Option<MutationAttemptId>,
    pub(crate) attempt_index: Option<u32>,
    pub(crate) reason: MutationReadinessConvergenceReason,
}

impl MutationReadinessConvergence {
    pub(crate) fn no_feasible_strategy(
        execution_id: &ExecutionId,
        execution_attempt: u32,
        feasibility: &MutationFeasibilitySet,
    ) -> Result<Self, MutationContractError> {
        feasibility.validate()?;
        if !feasibility.feasible_strategies().is_empty() {
            return Err(MutationContractError::Invalid {
                code: "mutation_readiness_feasible_strategy_exists",
            });
        }
        Self::new(
            execution_id,
            execution_attempt,
            feasibility,
            None,
            MutationReadinessConvergenceReason::NoFeasibleStrategy,
        )
    }

    pub(crate) fn admission_budget_exhausted(
        policy: &MutationAttemptPolicy,
        feasibility: &MutationFeasibilitySet,
        remaining: MutationAdmissionBudgetRemaining,
    ) -> Result<Self, MutationContractError> {
        let exhausted_dimensions = remaining.exhausted_dimensions();
        if exhausted_dimensions.is_empty() {
            return Err(MutationContractError::Invalid {
                code: "mutation_readiness_admission_budget_not_exhausted",
            });
        }
        Self::new(
            &policy.execution_id,
            policy.execution_attempt,
            feasibility,
            Some(policy),
            MutationReadinessConvergenceReason::AdmissionBudgetExhausted {
                remaining,
                exhausted_dimensions,
            },
        )
    }

    pub(crate) fn uncontacted_action_retry_exhausted(
        policy: &MutationAttemptPolicy,
        feasibility: &MutationFeasibilitySet,
        released_actions: u32,
        last_released_action_id: ActionId,
    ) -> Result<Self, MutationContractError> {
        let maximum_actions = MUTATION_UNCONTACTED_RELEASE_RETRY_LIMIT.saturating_add(1);
        if released_actions != maximum_actions {
            return Err(MutationContractError::Invalid {
                code: "mutation_readiness_release_limit_not_exhausted",
            });
        }
        Self::new(
            &policy.execution_id,
            policy.execution_attempt,
            feasibility,
            Some(policy),
            MutationReadinessConvergenceReason::UncontactedActionRetryExhausted {
                released_actions,
                maximum_actions,
                last_released_action_id,
            },
        )
    }

    fn new(
        execution_id: &ExecutionId,
        execution_attempt: u32,
        feasibility: &MutationFeasibilitySet,
        policy: Option<&MutationAttemptPolicy>,
        reason: MutationReadinessConvergenceReason,
    ) -> Result<Self, MutationContractError> {
        feasibility.validate()?;
        if execution_id.is_empty()
            || policy.is_some_and(|policy| {
                validate_policy_feasibility_binding(policy, feasibility).is_err()
                    || expected_attempt_id(policy).ok().as_ref() != Some(&policy.attempt_id)
                    || policy.execution_id != *execution_id
                    || policy.execution_attempt != execution_attempt
                    || policy.node_id != feasibility.node_id
                    || policy.node_attempt != feasibility.node_attempt
                    || policy.target_id != feasibility.target_id
                    || policy.context_manifest_id != feasibility.context_manifest_id
                    || policy.repository_revision != feasibility.repository_revision
                    || policy.feasibility_hash != feasibility.feasibility_hash
            })
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_readiness_binding_invalid",
            });
        }
        let mut convergence = Self {
            schema_version: MUTATION_SCHEMA_VERSION,
            convergence_id: EvidenceId::new("pending:mutation-readiness-convergence"),
            failure_revision_id: FailureRevisionId::new("pending:mutation-readiness-failure"),
            execution_id: execution_id.clone(),
            execution_attempt,
            node_id: feasibility.node_id.clone(),
            node_attempt: feasibility.node_attempt,
            target_id: feasibility.target_id.clone(),
            context_manifest_id: feasibility.context_manifest_id.clone(),
            repository_revision: feasibility.repository_revision.clone(),
            feasibility_hash: feasibility.feasibility_hash.clone(),
            attempt_id: policy.map(|policy| policy.attempt_id.clone()),
            attempt_index: policy.map(|policy| policy.attempt_index),
            reason,
        };
        convergence.convergence_id = expected_readiness_convergence_id(&convergence)?;
        convergence.failure_revision_id = expected_readiness_failure_revision_id(&convergence);
        convergence.validate()?;
        Ok(convergence)
    }

    pub(crate) fn validate(&self) -> Result<(), MutationContractError> {
        let reason_valid = match &self.reason {
            MutationReadinessConvergenceReason::NoFeasibleStrategy => {
                self.attempt_id.is_none() && self.attempt_index.is_none()
            }
            MutationReadinessConvergenceReason::AdmissionBudgetExhausted {
                remaining,
                exhausted_dimensions,
            } => {
                self.attempt_id.is_some()
                    && self.attempt_index.is_some_and(|index| index > 0)
                    && remaining.is_exhausted()
                    && exhausted_dimensions == &remaining.exhausted_dimensions()
            }
            MutationReadinessConvergenceReason::UncontactedActionRetryExhausted {
                released_actions,
                maximum_actions,
                last_released_action_id,
            } => {
                self.attempt_id.is_some()
                    && self.attempt_index.is_some_and(|index| index > 0)
                    && *maximum_actions
                        == MUTATION_UNCONTACTED_RELEASE_RETRY_LIMIT.saturating_add(1)
                    && released_actions == maximum_actions
                    && !last_released_action_id.is_empty()
            }
        };
        if self.schema_version != MUTATION_SCHEMA_VERSION
            || self.execution_id.is_empty()
            || self.execution_attempt == 0
            || self.node_id.is_empty()
            || self.node_attempt == 0
            || self.target_id.is_empty()
            || self.context_manifest_id.is_empty()
            || !is_sha256(&self.feasibility_hash)
            || !reason_valid
            || self.convergence_id != expected_readiness_convergence_id(self)?
            || self.failure_revision_id != expected_readiness_failure_revision_id(self)
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_readiness_convergence_invalid",
            });
        }
        Ok(())
    }
}

impl MutationConvergence {
    pub(crate) fn new(
        policy: &MutationAttemptPolicy,
        failure: &MutationFailure,
        reason: MutationConvergenceReason,
    ) -> Result<Self, MutationContractError> {
        if failure.attempt_id != policy.attempt_id
            || failure.node_id != policy.node_id
            || failure.target_id != policy.target_id
            || failure.context_manifest_id != policy.context_manifest_id
            || failure.repository_revision != policy.repository_revision
            || matches!(
                reason,
                MutationConvergenceReason::ContextRebuildBudgetExhausted
                    | MutationConvergenceReason::ContextRebuildUnavailable
            ) && failure.retryability != MutationRetryability::RebuildContext
            || matches!(
                reason,
                MutationConvergenceReason::MutationAttemptBudgetExhausted
            ) && !matches!(
                failure.retryability,
                MutationRetryability::ModelRetry
                    | MutationRetryability::SameTargetFallback
                    | MutationRetryability::RebuildContext
            )
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_convergence_failure_binding_mismatch",
            });
        }
        let mut convergence = Self {
            schema_version: MUTATION_SCHEMA_VERSION,
            convergence_id: EvidenceId::new("pending:mutation-convergence"),
            node_id: policy.node_id.clone(),
            target_id: policy.target_id.clone(),
            context_manifest_id: policy.context_manifest_id.clone(),
            repository_revision: policy.repository_revision.clone(),
            repository_revision_after: failure.repository_drift.as_ref().map_or_else(
                || policy.repository_revision.clone(),
                |drift| drift.observed_revision.clone(),
            ),
            repository_drift: failure.repository_drift.clone(),
            final_attempt_id: policy.attempt_id.clone(),
            final_attempt_index: policy.attempt_index,
            last_failure_revision_id: failure.failure_revision_id.clone(),
            reason,
        };
        convergence.convergence_id = expected_convergence_id(&convergence)?;
        convergence.validate()?;
        Ok(convergence)
    }

    pub(crate) fn validate(&self) -> Result<(), MutationContractError> {
        let repository_drift_valid = self.repository_drift.as_ref().map_or_else(
            || self.repository_revision_after == self.repository_revision,
            |drift| {
                matches!(
                    self.reason,
                    MutationConvergenceReason::MutationAttemptBudgetExhausted
                        | MutationConvergenceReason::ContextRebuildBudgetExhausted
                        | MutationConvergenceReason::ContextRebuildUnavailable
                ) && drift.expected_revision == self.repository_revision
                    && drift.observed_revision == self.repository_revision_after
                    && drift.expected_revision != drift.observed_revision
                    && is_sha256(&drift.expected_fingerprint)
                    && is_sha256(&drift.observed_fingerprint)
                    && drift.expected_fingerprint != drift.observed_fingerprint
                    && drift.context_rebuild_required
            },
        );
        if self.schema_version != MUTATION_SCHEMA_VERSION
            || self.node_id.is_empty()
            || self.target_id.is_empty()
            || self.context_manifest_id.is_empty()
            || self.final_attempt_index == 0
            || !repository_drift_valid
            || self.convergence_id != expected_convergence_id(self)?
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_convergence_invalid",
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
pub(crate) enum MutationEvent {
    FeasibilityEvaluated {
        feasibility: MutationFeasibilitySet,
    },
    AttemptPolicySelected {
        policy: MutationAttemptPolicy,
    },
    ActionPrepared {
        prepared: Box<PreparedMutationAction>,
    },
    ActionReleased {
        action_id: ActionId,
    },
    ActionRejected {
        failure: MutationFailure,
    },
    CandidateRecorded {
        candidate: MutationCandidateRecord,
    },
    AttemptFailed {
        failure: MutationFailure,
    },
    ApplicationObserved {
        request: MutationApplyRequest,
        observation: MutationApplicationObservation,
    },
    MutationVerified {
        evidence: MutationVerificationEvidence,
    },
    ConvergenceEvaluated {
        convergence: MutationConvergence,
    },
    ReadinessConvergenceEvaluated {
        convergence: MutationReadinessConvergence,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MutationEffectRequest {
    DispatchProvider {
        request: Box<MutationProviderRequestContract>,
    },
    ApplyMutation {
        request: Box<MutationApplyRequest>,
    },
    VerifyMutation {
        request: Box<MutationVerifyRequest>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationActionProjection {
    pub(crate) prepared: PreparedMutationAction,
    pub(crate) released_uncontacted: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationAttemptProjection {
    pub(crate) policy: MutationAttemptPolicy,
    pub(crate) actions: BTreeMap<u32, MutationActionProjection>,
    /// Cached latest action retained for v1 projection readers.
    pub(crate) prepared_action: Option<PreparedMutationAction>,
    /// Cached release state for `prepared_action`.
    pub(crate) action_released: bool,
    pub(crate) candidate: Option<MutationCandidateRecord>,
    pub(crate) failure: Option<MutationFailure>,
    pub(crate) apply_request: Option<MutationApplyRequest>,
    pub(crate) application: Option<MutationApplicationObservation>,
    pub(crate) verification: Option<MutationVerificationEvidence>,
}

impl MutationAttemptProjection {
    pub(crate) fn active_action(&self) -> Option<&PreparedMutationAction> {
        self.actions
            .values()
            .next_back()
            .filter(|action| !action.released_uncontacted)
            .map(|action| &action.prepared)
    }

    pub(crate) fn released_action_count(&self) -> u32 {
        u32::try_from(
            self.actions
                .values()
                .filter(|action| action.released_uncontacted)
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    pub(crate) fn last_released_action_id(&self) -> Option<&ActionId> {
        self.actions.values().next_back().and_then(|action| {
            action
                .released_uncontacted
                .then_some(&action.prepared.provider_request.action_id)
        })
    }

    pub(crate) fn uncontacted_release_limit_exhausted(&self) -> bool {
        self.released_action_count() == MUTATION_UNCONTACTED_RELEASE_RETRY_LIMIT.saturating_add(1)
            && self
                .actions
                .values()
                .all(|action| action.released_uncontacted)
            && self.candidate.is_none()
            && self.failure.is_none()
    }

    pub(crate) fn next_action_binding(
        &self,
    ) -> Result<Option<(u32, Option<ActionId>)>, MutationContractError> {
        if self.candidate.is_some()
            || self.failure.is_some()
            || self.application.is_some()
            || self.verification.is_some()
        {
            return Ok(None);
        }
        let Some((last_index, last)) = self.actions.last_key_value() else {
            return Ok(Some((1, None)));
        };
        if !last.released_uncontacted {
            return Ok(None);
        }
        let next_index = last_index.saturating_add(1);
        if next_index > MUTATION_UNCONTACTED_RELEASE_RETRY_LIMIT.saturating_add(1) {
            return Ok(None);
        }
        validate_action_chain_binding(next_index, Some(&last.prepared.provider_request.action_id))?;
        Ok(Some((
            next_index,
            Some(last.prepared.provider_request.action_id.clone()),
        )))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetMutationState {
    pub(crate) node_id: NodeId,
    pub(crate) target_id: TargetId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) feasibility: MutationFeasibilitySet,
    pub(crate) attempts: BTreeMap<u32, MutationAttemptProjection>,
    pub(crate) verified: Option<MutationVerificationEvidence>,
    pub(crate) convergence: Option<MutationConvergence>,
    pub(crate) readiness_convergence: Option<MutationReadinessConvergence>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationLedger {
    pub(crate) contexts: BTreeMap<ContextManifestId, TargetMutationState>,
    pub(crate) current_by_node: BTreeMap<NodeId, ContextManifestId>,
    pub(crate) last_attempt_index_by_node: BTreeMap<NodeId, u32>,
}

impl MutationLedger {
    pub(crate) fn current_target(&self, node_id: &NodeId) -> Option<&TargetMutationState> {
        self.current_by_node
            .get(node_id)
            .and_then(|context_id| self.contexts.get(context_id))
    }

    pub(crate) fn attempt(
        &self,
        attempt_id: &MutationAttemptId,
    ) -> Option<&MutationAttemptProjection> {
        self.contexts
            .values()
            .flat_map(|target| target.attempts.values())
            .find(|attempt| attempt.policy.attempt_id == *attempt_id)
    }

    pub(crate) fn attempt_id_for_call(&self, call_id: &ModelCallId) -> Option<MutationAttemptId> {
        self.contexts
            .values()
            .flat_map(|target| target.attempts.values())
            .find(|attempt| {
                attempt
                    .actions
                    .values()
                    .any(|action| action.prepared.provider_request.call_id == *call_id)
            })
            .map(|attempt| attempt.policy.attempt_id.clone())
    }

    pub(crate) fn consumed_attempt_ids_for_calls(
        &self,
        consumed_call_ids: &BTreeSet<ModelCallId>,
    ) -> BTreeSet<MutationAttemptId> {
        consumed_call_ids
            .iter()
            .filter_map(|call_id| self.attempt_id_for_call(call_id))
            .collect()
    }

    pub(crate) fn apply(&mut self, event: &MutationEvent) -> Result<(), MutationContractError> {
        let mut next = self.clone();
        next.apply_in_place(event)?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<(), MutationContractError> {
        let mut attempts_by_node =
            BTreeMap::<NodeId, BTreeMap<u32, (MutationAttemptId, Option<MutationAttemptId>)>>::new(
            );
        for target in self.contexts.values() {
            for (index, attempt) in &target.attempts {
                if attempts_by_node
                    .entry(target.node_id.clone())
                    .or_default()
                    .insert(
                        *index,
                        (
                            attempt.policy.attempt_id.clone(),
                            attempt.policy.prior_attempt_id.clone(),
                        ),
                    )
                    .is_some()
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_global_attempt_index_duplicate",
                    });
                }
            }
        }
        if attempts_by_node.values().any(|attempts| {
            attempts.iter().any(|(index, (_, prior_attempt_id))| {
                let expected = index
                    .checked_sub(1)
                    .filter(|prior| *prior > 0)
                    .and_then(|prior| attempts.get(&prior))
                    .map(|(attempt_id, _)| attempt_id);
                prior_attempt_id.as_ref() != expected
            })
        }) {
            return Err(MutationContractError::Invalid {
                code: "mutation_global_attempt_sequence_invalid",
            });
        }
        let mut context_nodes = BTreeSet::<NodeId>::new();
        for (context_id, target) in &self.contexts {
            context_nodes.insert(target.node_id.clone());
            target.feasibility.validate()?;
            if context_id != &target.context_manifest_id
                || target.node_id != target.feasibility.node_id
                || target.target_id != target.feasibility.target_id
                || target.context_manifest_id != target.feasibility.context_manifest_id
                || target.repository_revision != target.feasibility.repository_revision
            {
                return Err(MutationContractError::Invalid {
                    code: "mutation_ledger_target_binding_mismatch",
                });
            }
            for (index, attempt) in &target.attempts {
                if *index != attempt.policy.attempt_index
                    || attempt.policy.node_id != target.node_id
                    || attempt.policy.target_id != target.target_id
                    || attempt.policy.context_manifest_id != target.context_manifest_id
                    || attempt.policy.repository_revision != target.repository_revision
                    || attempt.policy.feasibility_hash != target.feasibility.feasibility_hash
                    || validate_policy_feasibility_binding(&attempt.policy, &target.feasibility)
                        .is_err()
                    || validate_attempt_action_history(attempt).is_err()
                    || attempt
                        .prepared_action
                        .as_ref()
                        .is_some_and(|prepared| prepared.policy != attempt.policy)
                    || attempt.candidate.as_ref().is_some_and(|candidate| {
                        candidate.attempt_id != attempt.policy.attempt_id
                            || candidate.node_id != target.node_id
                            || candidate.target_id != target.target_id
                            || candidate.context_manifest_id != target.context_manifest_id
                            || candidate.repository_revision != target.repository_revision
                            || expected_candidate_hash(candidate).ok().as_ref()
                                != Some(&candidate.candidate_hash)
                            || expected_candidate_id(candidate).ok().as_ref()
                                != Some(&candidate.candidate_id)
                    })
                    || attempt.failure.as_ref().is_some_and(|failure| {
                        failure.validate_identity_against(&attempt.policy).is_err()
                            || failure.node_id != target.node_id
                            || failure.target_id != target.target_id
                            || failure.context_manifest_id != target.context_manifest_id
                            || !mutation_failure_matches_stage(
                                failure,
                                attempt.candidate.as_ref(),
                                attempt.application.as_ref(),
                            )
                    })
                    || attempt.apply_request.as_ref().is_some_and(|request| {
                        attempt.candidate.as_ref().is_none_or(|candidate| {
                            request.candidate_id != candidate.candidate_id
                                || request.candidate_hash != candidate.candidate_hash
                                || request.operation != candidate.operation
                                || request.owned_paths != candidate.operation.owned_paths()
                                || request.context_manifest_id != target.context_manifest_id
                                || request.repository_revision != target.repository_revision
                        })
                    })
                    || attempt.application.as_ref().is_some_and(|observation| {
                        attempt
                            .apply_request
                            .as_ref()
                            .is_none_or(|request| observation.validate_against(request).is_err())
                    })
                    || attempt.verification.as_ref().is_some_and(|evidence| {
                        evidence.validate().is_err()
                            || evidence.attempt_id != attempt.policy.attempt_id
                            || evidence.node_id != target.node_id
                            || evidence.target_id != target.target_id
                            || evidence.context_manifest_id != target.context_manifest_id
                            || attempt.apply_request.as_ref().is_none_or(|request| {
                                evidence.application_id != request.application_id
                                    || evidence.candidate_id != request.candidate_id
                                    || evidence.repository_revision_before
                                        != request.repository_revision
                                    || evidence.repository_fingerprint_before
                                        != request.repository_fingerprint
                                    || evidence.changed_paths != request.owned_paths
                                    || evidence
                                        .path_transitions
                                        .keys()
                                        .cloned()
                                        .collect::<BTreeSet<_>>()
                                        != request.owned_paths
                            })
                            || attempt.candidate.as_ref().is_none_or(|candidate| {
                                !operation_transition_is_verified(
                                    &candidate.operation,
                                    &evidence.path_transitions,
                                )
                            })
                    })
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_ledger_attempt_invalid",
                    });
                }
            }
            if target
                .attempts
                .keys()
                .copied()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|pair| pair[0].checked_add(1) != Some(pair[1]))
            {
                return Err(MutationContractError::Invalid {
                    code: "mutation_ledger_attempt_sequence_invalid",
                });
            }
            let projected_verified = target
                .attempts
                .values()
                .find_map(|attempt| attempt.verification.as_ref());
            if target.verified.as_ref() != projected_verified
                || target
                    .attempts
                    .values()
                    .filter(|attempt| attempt.verification.is_some())
                    .count()
                    > 1
            {
                return Err(MutationContractError::Invalid {
                    code: "mutation_ledger_verification_projection_mismatch",
                });
            }
            if usize::from(target.verified.is_some())
                + usize::from(target.convergence.is_some())
                + usize::from(target.readiness_convergence.is_some())
                > 1
            {
                return Err(MutationContractError::Invalid {
                    code: "mutation_ledger_terminal_projection_conflict",
                });
            }
            if let Some(convergence) = &target.convergence {
                convergence.validate()?;
                let Some(final_attempt) = target.attempts.get(&convergence.final_attempt_index)
                else {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_convergence_attempt_missing",
                    });
                };
                if convergence.node_id != target.node_id
                    || convergence.target_id != target.target_id
                    || convergence.context_manifest_id != target.context_manifest_id
                    || convergence.repository_revision != target.repository_revision
                    || convergence.final_attempt_id != final_attempt.policy.attempt_id
                    || final_attempt
                        .failure
                        .as_ref()
                        .map(|failure| &failure.failure_revision_id)
                        != Some(&convergence.last_failure_revision_id)
                    || final_attempt
                        .failure
                        .as_ref()
                        .map(|failure| &failure.repository_drift)
                        != Some(&convergence.repository_drift)
                    || final_attempt.failure.as_ref().is_some_and(|failure| {
                        matches!(
                            convergence.reason,
                            MutationConvergenceReason::ContextRebuildBudgetExhausted
                                | MutationConvergenceReason::ContextRebuildUnavailable
                        ) && failure.retryability != MutationRetryability::RebuildContext
                            || matches!(
                                convergence.reason,
                                MutationConvergenceReason::MutationAttemptBudgetExhausted
                            ) && !matches!(
                                failure.retryability,
                                MutationRetryability::ModelRetry
                                    | MutationRetryability::SameTargetFallback
                                    | MutationRetryability::RebuildContext
                            )
                    })
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_convergence_projection_mismatch",
                    });
                }
            }
            if let Some(convergence) = &target.readiness_convergence {
                convergence.validate()?;
                if convergence.node_id != target.node_id
                    || convergence.target_id != target.target_id
                    || convergence.context_manifest_id != target.context_manifest_id
                    || convergence.repository_revision != target.repository_revision
                    || convergence.feasibility_hash != target.feasibility.feasibility_hash
                    || !readiness_convergence_matches_target(convergence, target)
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_readiness_projection_mismatch",
                    });
                }
            }
        }
        if self.current_by_node.len() != context_nodes.len()
            || self.current_by_node.iter().any(|(node_id, context_id)| {
                self.contexts
                    .get(context_id)
                    .is_none_or(|target| &target.node_id != node_id)
            })
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_current_context_projection_invalid",
            });
        }
        for node_id in context_nodes {
            let attempts = attempts_by_node.remove(&node_id).unwrap_or_default();
            let last = self
                .last_attempt_index_by_node
                .get(&node_id)
                .copied()
                .unwrap_or_default();
            if (last == 0 && !attempts.is_empty())
                || (last > 0 && attempts.keys().copied().ne(1..=last))
                || attempts.iter().any(|(index, (_, prior_attempt_id))| {
                    let expected = index
                        .checked_sub(1)
                        .filter(|prior| *prior > 0)
                        .and_then(|prior| attempts.get(&prior))
                        .map(|(attempt_id, _)| attempt_id);
                    prior_attempt_id.as_ref() != expected
                })
            {
                return Err(MutationContractError::Invalid {
                    code: "mutation_global_attempt_sequence_invalid",
                });
            }
        }
        if !attempts_by_node.is_empty()
            || self
                .last_attempt_index_by_node
                .keys()
                .any(|node_id| !self.current_by_node.contains_key(node_id))
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_attempt_frontier_invalid",
            });
        }
        if self.contexts.values().any(|target| {
            target.attempts.values().any(|attempt| {
                expected_attempt_id(&attempt.policy).ok().as_ref()
                    != Some(&attempt.policy.attempt_id)
            })
        }) {
            return Err(MutationContractError::Invalid {
                code: "mutation_attempt_identity_invalid",
            });
        }
        Ok(())
    }

    fn apply_in_place(&mut self, event: &MutationEvent) -> Result<(), MutationContractError> {
        match event {
            MutationEvent::FeasibilityEvaluated { feasibility } => {
                feasibility.validate()?;
                if self.contexts.contains_key(&feasibility.context_manifest_id) {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_feasibility_already_recorded",
                    });
                }
                if let Some(previous_context_id) = self.current_by_node.get(&feasibility.node_id) {
                    let previous = self.contexts.get(previous_context_id).ok_or(
                        MutationContractError::Invalid {
                            code: "mutation_current_context_missing",
                        },
                    )?;
                    let rebuild_allowed = previous.verified.is_none()
                        && previous.convergence.is_none()
                        && previous.readiness_convergence.is_none()
                        && previous
                            .attempts
                            .values()
                            .next_back()
                            .is_some_and(|attempt| {
                                attempt.failure.as_ref().is_some_and(|failure| {
                                    failure.retryability == MutationRetryability::RebuildContext
                                })
                            });
                    if previous.target_id != feasibility.target_id || !rebuild_allowed {
                        return Err(MutationContractError::Invalid {
                            code: "mutation_context_replacement_not_authorized",
                        });
                    }
                }
                self.contexts.insert(
                    feasibility.context_manifest_id.clone(),
                    TargetMutationState {
                        node_id: feasibility.node_id.clone(),
                        target_id: feasibility.target_id.clone(),
                        context_manifest_id: feasibility.context_manifest_id.clone(),
                        repository_revision: feasibility.repository_revision.clone(),
                        feasibility: feasibility.clone(),
                        attempts: BTreeMap::new(),
                        verified: None,
                        convergence: None,
                        readiness_convergence: None,
                    },
                );
                self.current_by_node.insert(
                    feasibility.node_id.clone(),
                    feasibility.context_manifest_id.clone(),
                );
            }
            MutationEvent::AttemptPolicySelected { policy } => {
                let expected_index = self
                    .last_attempt_index_by_node
                    .get(&policy.node_id)
                    .copied()
                    .unwrap_or_default()
                    .saturating_add(1);
                let expected_prior_attempt_id = (expected_index > 1)
                    .then(|| self.attempt_id_at(&policy.node_id, expected_index - 1))
                    .transpose()?;
                let target = self.current_target_mut(&policy.node_id)?;
                if target.verified.is_some()
                    || target.convergence.is_some()
                    || target.readiness_convergence.is_some()
                    || policy.attempt_index != expected_index
                    || policy.target_id != target.target_id
                    || policy.context_manifest_id != target.context_manifest_id
                    || policy.repository_revision != target.repository_revision
                    || policy.feasibility_hash != target.feasibility.feasibility_hash
                    || policy.prior_attempt_id != expected_prior_attempt_id
                    || policy.attempt_id != expected_attempt_id(policy)?
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_policy_projection_rejected",
                    });
                }
                target.attempts.insert(
                    policy.attempt_index,
                    MutationAttemptProjection {
                        policy: policy.clone(),
                        actions: BTreeMap::new(),
                        prepared_action: None,
                        action_released: false,
                        candidate: None,
                        failure: None,
                        apply_request: None,
                        application: None,
                        verification: None,
                    },
                );
                self.last_attempt_index_by_node
                    .insert(policy.node_id.clone(), policy.attempt_index);
            }
            MutationEvent::ActionPrepared { prepared } => {
                let attempt = self.attempt_mut(&prepared.policy)?;
                let Some((expected_index, expected_prior)) = attempt.next_action_binding()? else {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_action_not_ready",
                    });
                };
                if prepared.action_index != expected_index
                    || prepared.prior_released_action_id != expected_prior
                    || prepared.provider_request.action_index != expected_index
                    || prepared.provider_request.prior_released_action_id
                        != prepared.prior_released_action_id
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_action_chain_not_authoritative",
                    });
                }
                attempt.actions.insert(
                    prepared.action_index,
                    MutationActionProjection {
                        prepared: (**prepared).clone(),
                        released_uncontacted: false,
                    },
                );
                attempt.prepared_action = Some((**prepared).clone());
                attempt.action_released = false;
            }
            MutationEvent::ActionReleased { action_id } => {
                let attempt =
                    self.contexts
                        .values_mut()
                        .filter(|target| {
                            target.verified.is_none()
                                && target.convergence.is_none()
                                && target.readiness_convergence.is_none()
                        })
                        .flat_map(|target| target.attempts.values_mut())
                        .find(|attempt| {
                            attempt.actions.values().any(|action| {
                                action.prepared.provider_request.action_id == *action_id
                            })
                        })
                        .ok_or(MutationContractError::Invalid {
                            code: "mutation_action_unknown",
                        })?;
                let Some((last_index, last)) = attempt.actions.last_key_value() else {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_action_unknown",
                    });
                };
                let last_index = *last_index;
                let last_action_id = last.prepared.provider_request.action_id.clone();
                let last_released = last.released_uncontacted;
                if last_action_id != *action_id
                    || last_released
                    || attempt.candidate.is_some()
                    || attempt.failure.is_some()
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_action_release_invalid",
                    });
                }
                attempt
                    .actions
                    .get_mut(&last_index)
                    .expect("last action key remains present")
                    .released_uncontacted = true;
                attempt.action_released = true;
            }
            MutationEvent::ActionRejected { failure }
            | MutationEvent::AttemptFailed { failure } => {
                let attempt = self.attempt_for_failure_mut(failure)?;
                if attempt
                    .failure
                    .as_ref()
                    .is_some_and(|existing| existing != failure)
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_attempt_failure_conflict",
                    });
                }
                attempt.failure = Some(failure.clone());
            }
            MutationEvent::CandidateRecorded { candidate } => {
                let attempt = self.attempt_for_candidate_mut(candidate)?;
                if attempt.candidate.is_some()
                    || attempt.action_released
                    || attempt.active_action().is_none()
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_candidate_projection_rejected",
                    });
                }
                attempt.candidate = Some(candidate.clone());
            }
            MutationEvent::ApplicationObserved {
                request,
                observation,
            } => {
                observation.validate_against(request)?;
                let attempt = self.attempt_for_id_mut(&request.attempt_id)?;
                if attempt.application.is_some()
                    || attempt.candidate.as_ref().is_none_or(|candidate| {
                        request.candidate_id != candidate.candidate_id
                            || request.candidate_hash != candidate.candidate_hash
                    })
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_application_projection_rejected",
                    });
                }
                attempt.apply_request = Some(request.clone());
                attempt.application = Some(observation.clone());
            }
            MutationEvent::MutationVerified { evidence } => {
                evidence.validate()?;
                if self.current_by_node.get(&evidence.node_id)
                    != Some(&evidence.context_manifest_id)
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_verification_context_not_current",
                    });
                }
                let target = self.target_by_context_mut(&evidence.context_manifest_id)?;
                if target.node_id != evidence.node_id {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_verification_context_not_current",
                    });
                }
                if target.verified.is_some()
                    || target.convergence.is_some()
                    || target.readiness_convergence.is_some()
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_verification_already_recorded",
                    });
                }
                let attempt = target
                    .attempts
                    .values_mut()
                    .find(|attempt| attempt.policy.attempt_id == evidence.attempt_id)
                    .ok_or(MutationContractError::Invalid {
                        code: "mutation_verification_attempt_unknown",
                    })?;
                if attempt.application.is_none()
                    || attempt
                        .candidate
                        .as_ref()
                        .is_none_or(|candidate| candidate.candidate_id != evidence.candidate_id)
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_verification_projection_rejected",
                    });
                }
                attempt.verification = Some(evidence.clone());
                target.verified = Some(evidence.clone());
            }
            MutationEvent::ConvergenceEvaluated { convergence } => {
                convergence.validate()?;
                if self.current_by_node.get(&convergence.node_id)
                    != Some(&convergence.context_manifest_id)
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_convergence_context_not_current",
                    });
                }
                let target = self.target_by_context_mut(&convergence.context_manifest_id)?;
                if target.node_id != convergence.node_id {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_convergence_context_not_current",
                    });
                }
                if target.verified.is_some()
                    || target.convergence.is_some()
                    || target.readiness_convergence.is_some()
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_convergence_already_terminal",
                    });
                }
                let attempt = target
                    .attempts
                    .get(&convergence.final_attempt_index)
                    .ok_or(MutationContractError::Invalid {
                        code: "mutation_convergence_attempt_missing",
                    })?;
                if convergence.target_id != target.target_id
                    || convergence.context_manifest_id != target.context_manifest_id
                    || convergence.repository_revision != target.repository_revision
                    || convergence.final_attempt_id != attempt.policy.attempt_id
                    || attempt
                        .failure
                        .as_ref()
                        .map(|failure| &failure.failure_revision_id)
                        != Some(&convergence.last_failure_revision_id)
                    || attempt
                        .failure
                        .as_ref()
                        .map(|failure| &failure.repository_drift)
                        != Some(&convergence.repository_drift)
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_convergence_projection_rejected",
                    });
                }
                target.convergence = Some(convergence.clone());
            }
            MutationEvent::ReadinessConvergenceEvaluated { convergence } => {
                convergence.validate()?;
                if self.current_by_node.get(&convergence.node_id)
                    != Some(&convergence.context_manifest_id)
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_readiness_context_not_current",
                    });
                }
                let target = self.target_by_context_mut(&convergence.context_manifest_id)?;
                if target.node_id != convergence.node_id
                    || target.verified.is_some()
                    || target.convergence.is_some()
                    || target.readiness_convergence.is_some()
                    || !readiness_convergence_matches_target(convergence, target)
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_readiness_projection_rejected",
                    });
                }
                target.readiness_convergence = Some(convergence.clone());
            }
        }
        Ok(())
    }

    fn target_by_context_mut(
        &mut self,
        context_id: &ContextManifestId,
    ) -> Result<&mut TargetMutationState, MutationContractError> {
        self.contexts
            .get_mut(context_id)
            .ok_or(MutationContractError::Invalid {
                code: "mutation_target_projection_missing",
            })
    }

    fn current_target_mut(
        &mut self,
        node_id: &NodeId,
    ) -> Result<&mut TargetMutationState, MutationContractError> {
        let context_id =
            self.current_by_node
                .get(node_id)
                .cloned()
                .ok_or(MutationContractError::Invalid {
                    code: "mutation_current_context_missing",
                })?;
        self.target_by_context_mut(&context_id)
    }

    fn attempt_id_at(
        &self,
        node_id: &NodeId,
        attempt_index: u32,
    ) -> Result<MutationAttemptId, MutationContractError> {
        self.contexts
            .values()
            .filter(|target| &target.node_id == node_id)
            .find_map(|target| target.attempts.get(&attempt_index))
            .map(|attempt| attempt.policy.attempt_id.clone())
            .ok_or(MutationContractError::Invalid {
                code: "mutation_prior_attempt_missing",
            })
    }

    fn attempt_mut(
        &mut self,
        policy: &MutationAttemptPolicy,
    ) -> Result<&mut MutationAttemptProjection, MutationContractError> {
        if self.current_by_node.get(&policy.node_id) != Some(&policy.context_manifest_id) {
            return Err(MutationContractError::Invalid {
                code: "mutation_attempt_context_not_current",
            });
        }
        let target = self.target_by_context_mut(&policy.context_manifest_id)?;
        if target.verified.is_some()
            || target.convergence.is_some()
            || target.readiness_convergence.is_some()
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_target_already_terminal",
            });
        }
        target
            .attempts
            .get_mut(&policy.attempt_index)
            .filter(|attempt| attempt.policy == *policy)
            .ok_or(MutationContractError::Invalid {
                code: "mutation_attempt_projection_missing",
            })
    }

    fn attempt_for_id_mut(
        &mut self,
        attempt_id: &MutationAttemptId,
    ) -> Result<&mut MutationAttemptProjection, MutationContractError> {
        for target in self.contexts.values_mut() {
            if target
                .attempts
                .values()
                .any(|attempt| attempt.policy.attempt_id == *attempt_id)
            {
                if target.verified.is_some()
                    || target.convergence.is_some()
                    || target.readiness_convergence.is_some()
                {
                    return Err(MutationContractError::Invalid {
                        code: "mutation_target_already_terminal",
                    });
                }
                return target
                    .attempts
                    .values_mut()
                    .find(|attempt| attempt.policy.attempt_id == *attempt_id)
                    .ok_or(MutationContractError::Invalid {
                        code: "mutation_attempt_projection_missing",
                    });
            }
        }
        Err(MutationContractError::Invalid {
            code: "mutation_attempt_projection_missing",
        })
    }

    fn attempt_for_failure_mut(
        &mut self,
        failure: &MutationFailure,
    ) -> Result<&mut MutationAttemptProjection, MutationContractError> {
        self.attempt_for_id_mut(&failure.attempt_id)
            .and_then(|attempt| {
                (attempt.policy.node_id == failure.node_id
                    && attempt.policy.target_id == failure.target_id)
                    .then_some(attempt)
                    .ok_or(MutationContractError::Invalid {
                        code: "mutation_failure_projection_binding_mismatch",
                    })
            })
    }

    fn attempt_for_candidate_mut(
        &mut self,
        candidate: &MutationCandidateRecord,
    ) -> Result<&mut MutationAttemptProjection, MutationContractError> {
        self.attempt_for_id_mut(&candidate.attempt_id)
            .and_then(|attempt| {
                (attempt.policy.node_id == candidate.node_id
                    && attempt.policy.target_id == candidate.target_id)
                    .then_some(attempt)
                    .ok_or(MutationContractError::Invalid {
                        code: "mutation_candidate_projection_binding_mismatch",
                    })
            })
    }
}

fn validate_attempt_action_history(
    attempt: &MutationAttemptProjection,
) -> Result<(), MutationContractError> {
    let maximum_actions = MUTATION_UNCONTACTED_RELEASE_RETRY_LIMIT.saturating_add(1);
    if u32::try_from(attempt.actions.len()).unwrap_or(u32::MAX) > maximum_actions
        || attempt
            .actions
            .keys()
            .copied()
            .ne(1..=u32::try_from(attempt.actions.len()).unwrap_or(u32::MAX))
    {
        return Err(MutationContractError::Invalid {
            code: "mutation_action_history_sequence_invalid",
        });
    }
    let mut prior_action_id: Option<&ActionId> = None;
    for (index, action) in &attempt.actions {
        let prepared = &action.prepared;
        validate_action_chain_binding(*index, prepared.prior_released_action_id.as_ref())?;
        let expected_action_id = mutation_action_id(
            &attempt.policy,
            &attempt.policy.context_manifest_id,
            *index,
            prior_action_id,
        );
        if prepared.policy != attempt.policy
            || prepared.action_index != *index
            || prepared.provider_request.action_index != *index
            || prepared.prior_released_action_id.as_ref() != prior_action_id
            || prepared.provider_request.prior_released_action_id.as_ref() != prior_action_id
            || prepared.provider_request.action_id != expected_action_id
            || prepared.admission.action_id != expected_action_id
            || prepared.provider_request.call_id != mutation_call_id(&expected_action_id)
            || prepared.admission.call_id != prepared.provider_request.call_id
            || prepared.provider_request.reservation_id
                != mutation_reservation_id(
                    &prepared.provider_request.call_id,
                    &attempt.policy.node_id,
                )
            || prior_action_id.is_some()
                && attempt
                    .actions
                    .get(&index.saturating_sub(1))
                    .is_none_or(|prior| !prior.released_uncontacted)
        {
            return Err(MutationContractError::Invalid {
                code: "mutation_action_history_binding_invalid",
            });
        }
        prior_action_id = Some(&prepared.provider_request.action_id);
    }
    let latest = attempt.actions.values().next_back();
    if attempt.prepared_action.as_ref() != latest.map(|action| &action.prepared)
        || attempt.action_released != latest.is_some_and(|action| action.released_uncontacted)
        || attempt.candidate.as_ref().is_some_and(|candidate| {
            latest.is_none_or(|action| {
                action.released_uncontacted
                    || candidate.action_id != action.prepared.provider_request.action_id
                    || candidate.call_id != action.prepared.provider_request.call_id
            })
        })
    {
        return Err(MutationContractError::Invalid {
            code: "mutation_action_history_projection_invalid",
        });
    }
    Ok(())
}

fn readiness_convergence_matches_target(
    convergence: &MutationReadinessConvergence,
    target: &TargetMutationState,
) -> bool {
    match &convergence.reason {
        MutationReadinessConvergenceReason::NoFeasibleStrategy => {
            target.attempts.is_empty() && target.feasibility.feasible_strategies().is_empty()
        }
        MutationReadinessConvergenceReason::AdmissionBudgetExhausted { .. } => {
            let (Some(attempt_id), Some(attempt_index)) =
                (&convergence.attempt_id, convergence.attempt_index)
            else {
                return false;
            };
            target.attempts.get(&attempt_index).is_some_and(|attempt| {
                attempt.policy.attempt_id == *attempt_id
                    && attempt.policy.execution_id == convergence.execution_id
                    && attempt.policy.execution_attempt == convergence.execution_attempt
                    && attempt.next_action_binding().ok().flatten().is_some()
            })
        }
        MutationReadinessConvergenceReason::UncontactedActionRetryExhausted {
            released_actions,
            maximum_actions,
            last_released_action_id,
        } => {
            let (Some(attempt_id), Some(attempt_index)) =
                (&convergence.attempt_id, convergence.attempt_index)
            else {
                return false;
            };
            target.attempts.get(&attempt_index).is_some_and(|attempt| {
                attempt.policy.attempt_id == *attempt_id
                    && attempt.policy.execution_id == convergence.execution_id
                    && attempt.policy.execution_attempt == convergence.execution_attempt
                    && attempt.released_action_count() == *released_actions
                    && *maximum_actions
                        == MUTATION_UNCONTACTED_RELEASE_RETRY_LIMIT.saturating_add(1)
                    && attempt.last_released_action_id() == Some(last_released_action_id)
                    && attempt.uncontacted_release_limit_exhausted()
            })
        }
    }
}

fn validate_active_binding(
    node: &ExecutionNode,
    target: &PlannedTargetV1,
    context: &TargetContextManifest,
) -> Result<(), MutationContractError> {
    let NodeState::Active { attempt } = node.state else {
        return Err(MutationContractError::Invalid {
            code: "mutation_node_not_active",
        });
    };
    let expected_hash = target.operation.expected_content_hash();
    let receipt_matches = match (&context.full_target_artifact, expected_hash) {
        (None, None) => true,
        (Some(receipt), Some(hash)) => {
            receipt.line_range.is_none()
                && receipt.content_hash == hash
                && receipt.source_content_hash == hash
                && receipt.artifact_reference_hash == hash
        }
        _ => false,
    };
    if !context_purpose_matches_node(node, target, context)
        || attempt == 0
        || attempt != context.node_attempt
        || node.id != context.node_id
        || target.target_id != context.target_id
        || context.schema_version != super::IMPLEMENTATION_CONTEXT_SCHEMA_VERSION
        || context.input_token_ceiling != node.budget.max_input_tokens_per_call
        || context.estimated_input_tokens > context.input_token_ceiling
        || !is_sha256(&context.repository_fingerprint)
        || !is_sha256(&context.materialized_context_hash)
        || !receipt_matches
    {
        return Err(MutationContractError::Invalid {
            code: "mutation_active_target_binding_mismatch",
        });
    }
    Ok(())
}

fn context_purpose_matches_node(
    node: &ExecutionNode,
    target: &PlannedTargetV1,
    context: &TargetContextManifest,
) -> bool {
    match (&node.kind, &context.purpose) {
        (NodeKind::Implementation, TargetExecutionPurpose::Implementation { change_id }) => {
            change_id == &target.change_id
        }
        (
            NodeKind::ValidationRepair,
            TargetExecutionPurpose::ValidationRepair {
                repair_intent_id,
                failure_revision_id,
                originating_gate_id,
                validation_evidence_id,
                baseline_mutation_evidence_id,
            },
        ) => {
            !repair_intent_id.is_empty()
                && !failure_revision_id.is_empty()
                && !originating_gate_id.is_empty()
                && !validation_evidence_id.is_empty()
                && !baseline_mutation_evidence_id.is_empty()
        }
        _ => false,
    }
}

fn validate_feasibility_active_binding(
    node: &ExecutionNode,
    target: &PlannedTargetV1,
    context: &TargetContextManifest,
    feasibility: &MutationFeasibilitySet,
) -> Result<(), MutationContractError> {
    validate_active_binding(node, target, context)?;
    feasibility.validate()?;
    if feasibility.node_id != node.id
        || feasibility.node_attempt != context.node_attempt
        || feasibility.target_id != target.target_id
        || feasibility.context_manifest_id != context.context_manifest_id
        || feasibility.repository_revision != context.repository_revision
        || feasibility.output_allowance != node.budget.max_output_tokens_per_call
    {
        return Err(MutationContractError::Invalid {
            code: "mutation_feasibility_active_binding_mismatch",
        });
    }
    Ok(())
}

fn prepared_request_matches_policy(prepared: &PreparedMutationAction) -> bool {
    let policy = &prepared.policy;
    let request = &prepared.provider_request;
    request.node_id == policy.node_id
        && request.node_attempt == policy.node_attempt
        && request.target_id == policy.target_id
        && request.context_manifest_id == policy.context_manifest_id
        && request.repository_revision == policy.repository_revision
        && request.attempt_id == policy.attempt_id
        && request.attempt_index == policy.attempt_index
        && request.permitted_strategies == policy.permitted_strategies
        && request.recovery == policy.recovery
}

fn prepared_request_matches_context(
    prepared: &PreparedMutationAction,
    context: &TargetContextManifest,
) -> bool {
    prepared_request_matches_policy(prepared)
        && prepared.policy.node_id == context.node_id
        && prepared.policy.node_attempt == context.node_attempt
        && prepared.policy.target_id == context.target_id
        && prepared.policy.context_manifest_id == context.context_manifest_id
        && prepared.policy.repository_revision == context.repository_revision
        && prepared.provider_request.materialized_context_hash == context.materialized_context_hash
        && prepared.provider_request.repository_fingerprint == context.repository_fingerprint
}

fn legal_initial_strategies(operation: &TargetOperation) -> Vec<MutationStrategy> {
    match operation {
        TargetOperation::ModifyExisting { .. } => vec![
            MutationStrategy::ApplyPatch {
                mode: PatchMode::Initial,
            },
            MutationStrategy::ReplaceFile,
        ],
        TargetOperation::CreateFile { .. } => vec![MutationStrategy::CreateFile],
        TargetOperation::DeleteFile { .. } => vec![MutationStrategy::DeleteFile],
        TargetOperation::MoveFile { .. } => vec![MutationStrategy::MoveFile],
    }
}

const fn canonical_strategy_rank(strategy: MutationStrategy) -> u8 {
    match strategy {
        MutationStrategy::ApplyPatch { .. } => 0,
        MutationStrategy::ReplaceFile => 1,
        MutationStrategy::CreateFile => 2,
        MutationStrategy::DeleteFile => 3,
        MutationStrategy::MoveFile => 4,
    }
}

const fn strategy_requires_complete_target(strategy: MutationStrategy) -> bool {
    matches!(strategy, MutationStrategy::ReplaceFile)
}

fn estimated_candidate_bytes(
    strategy: MutationStrategy,
    target: &PlannedTargetV1,
    target_size_bytes: u64,
) -> u64 {
    let estimated_change_bytes = u64::from(target.estimated_change.estimated_changed_lines)
        .saturating_mul(CONTENT_BYTES_PER_ESTIMATED_LINE)
        .max(MIN_CONTENT_CANDIDATE_BYTES);
    match strategy {
        MutationStrategy::ApplyPatch { .. } => estimated_change_bytes,
        MutationStrategy::ReplaceFile => target_size_bytes.saturating_add(estimated_change_bytes),
        MutationStrategy::CreateFile => estimated_change_bytes,
        MutationStrategy::DeleteFile | MutationStrategy::MoveFile => 0,
    }
}

fn estimate_tool_schema_tokens(_strategy: MutationStrategy, target: &PlannedTargetV1) -> u32 {
    let path_bytes = match &target.operation {
        TargetOperation::MoveFile { destination, .. } => target
            .path
            .as_str()
            .len()
            .saturating_add(destination.as_str().len()),
        _ => target.path.as_str().len(),
    };
    TOOL_SCHEMA_FIXED_TOKENS
        .saturating_add(u32::try_from(path_bytes.div_ceil(3)).unwrap_or(u32::MAX))
}

fn estimate_serialized_candidate_tokens(candidate_bytes: u64) -> u32 {
    let escaped = candidate_bytes.saturating_mul(JSON_WORST_CASE_EXPANSION);
    u32::try_from(escaped.div_ceil(SERIALIZED_BYTES_PER_TOKEN)).unwrap_or(u32::MAX)
}

fn max_candidate_bytes(output_allowance: u32, fixed_tokens: u32) -> u64 {
    let available_tokens = output_allowance.saturating_sub(fixed_tokens);
    u64::from(available_tokens)
        .saturating_mul(SERIALIZED_BYTES_PER_TOKEN)
        .checked_div(JSON_WORST_CASE_EXPANSION)
        .unwrap_or(0)
        .min(MAX_MUTATION_CANDIDATE_BYTES)
}

fn feasibility_hash(feasibility: &MutationFeasibilitySet) -> Result<String, MutationContractError> {
    let canonical = canonical_json(&(
        feasibility.schema_version,
        &feasibility.node_id,
        feasibility.node_attempt,
        &feasibility.target_id,
        &feasibility.context_manifest_id,
        &feasibility.repository_revision,
        feasibility.output_allowance,
        &feasibility.evaluations,
    ))?;
    Ok(stable_sha256(&[
        "execution-protocol-v1:mutation-feasibility",
        &canonical,
    ]))
}

fn validate_policy_feasibility_binding(
    policy: &MutationAttemptPolicy,
    feasibility: &MutationFeasibilitySet,
) -> Result<(), MutationContractError> {
    feasibility.validate()?;
    if policy.schema_version != MUTATION_SCHEMA_VERSION
        || policy.execution_id.is_empty()
        || policy.execution_attempt == 0
        || policy.attempt_index == 0
        || policy.node_id != feasibility.node_id
        || policy.node_attempt != feasibility.node_attempt
        || policy.target_id != feasibility.target_id
        || policy.context_manifest_id != feasibility.context_manifest_id
        || policy.repository_revision != feasibility.repository_revision
        || policy.feasibility_hash != feasibility.feasibility_hash
        || policy.permitted_strategies.is_empty()
        || policy.permitted_strategies.len() > 2
        || policy
            .permitted_strategies
            .windows(2)
            .any(|pair| canonical_strategy_rank(pair[0]) >= canonical_strategy_rank(pair[1]))
        || policy.permitted_strategies.iter().any(|strategy| {
            !feasibility
                .evaluation(*strategy)
                .is_some_and(MutationFeasibility::is_feasible)
        })
        || policy
            .forced_strategy
            .is_some_and(|forced| policy.permitted_strategies != [forced])
        || (policy.attempt_index == 1 && policy.prior_attempt_id.is_some())
        || (policy.attempt_index > 1 && policy.prior_attempt_id.is_none())
        || (policy.attempt_index == 1 && policy.recovery.is_some())
        || (policy.attempt_index > 1
            && policy.recovery.as_ref().is_none_or(|recovery| {
                Some(&recovery.prior_attempt_id) != policy.prior_attempt_id.as_ref()
                    || recovery.failure_revision_id.is_empty()
            }))
    {
        return Err(MutationContractError::Invalid {
            code: "mutation_policy_feasibility_binding_invalid",
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_attempt_policy(
    execution_id: &ExecutionId,
    execution_attempt: u32,
    node: &ExecutionNode,
    target: &PlannedTargetV1,
    context: &TargetContextManifest,
    feasibility: &MutationFeasibilitySet,
    attempt_index: u32,
    permitted_strategies: Vec<MutationStrategy>,
    forced_strategy: Option<MutationStrategy>,
    recovery: Option<MutationRecoveryContext>,
) -> Result<MutationAttemptPolicy, MutationContractError> {
    if attempt_index == 0 || attempt_index > node.budget.max_mutation_attempts {
        return Err(MutationContractError::AttemptBudgetExhausted {
            attempted: attempt_index,
            maximum: node.budget.max_mutation_attempts,
        });
    }
    let prior_attempt_id = recovery
        .as_ref()
        .map(|recovery| recovery.prior_attempt_id.clone());
    let mut policy = MutationAttemptPolicy {
        schema_version: MUTATION_SCHEMA_VERSION,
        attempt_id: MutationAttemptId::new("pending:mutation-attempt"),
        attempt_index,
        execution_id: execution_id.clone(),
        execution_attempt,
        node_id: node.id.clone(),
        node_attempt: context.node_attempt,
        target_id: target.target_id.clone(),
        context_manifest_id: context.context_manifest_id.clone(),
        repository_revision: context.repository_revision.clone(),
        permitted_strategies,
        forced_strategy,
        prior_attempt_id,
        recovery,
        feasibility_hash: feasibility.feasibility_hash.clone(),
    };
    policy.attempt_id = expected_attempt_id(&policy)?;
    policy.validate_against(node, target, context, feasibility)?;
    Ok(policy)
}

fn expected_attempt_id(
    policy: &MutationAttemptPolicy,
) -> Result<MutationAttemptId, MutationContractError> {
    let canonical = canonical_json(&(
        policy.schema_version,
        &policy.execution_id,
        policy.execution_attempt,
        &policy.node_id,
        policy.node_attempt,
        &policy.target_id,
        &policy.context_manifest_id,
        &policy.repository_revision,
        policy.attempt_index,
        &policy.permitted_strategies,
        policy.forced_strategy,
        &policy.prior_attempt_id,
        &policy.recovery,
        &policy.feasibility_hash,
    ))?;
    Ok(MutationAttemptId::new(format!(
        "epv1:{}",
        stable_sha256(&["execution-protocol-v1:mutation-attempt", &canonical])
    )))
}

fn validate_action_chain_binding(
    action_index: u32,
    prior_released_action_id: Option<&ActionId>,
) -> Result<(), MutationContractError> {
    let maximum_actions = MUTATION_UNCONTACTED_RELEASE_RETRY_LIMIT.saturating_add(1);
    if action_index == 0
        || action_index > maximum_actions
        || (action_index == 1) != prior_released_action_id.is_none()
    {
        return Err(MutationContractError::Invalid {
            code: "mutation_action_chain_binding_invalid",
        });
    }
    Ok(())
}

fn mutation_action_id(
    policy: &MutationAttemptPolicy,
    context_manifest_id: &ContextManifestId,
    action_index: u32,
    prior_released_action_id: Option<&ActionId>,
) -> ActionId {
    let action_index = action_index.to_string();
    let prior_released_action_id = prior_released_action_id.map_or("", ActionId::as_str);
    ActionId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:mutation-action",
            policy.attempt_id.as_str(),
            context_manifest_id.as_str(),
            &action_index,
            prior_released_action_id,
        ])
    ))
}

fn mutation_call_id(action_id: &ActionId) -> ModelCallId {
    ModelCallId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:mutation-model-call",
            action_id.as_str(),
        ])
    ))
}

fn mutation_reservation_id(call_id: &ModelCallId, node_id: &NodeId) -> ReservationId {
    ReservationId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:mutation-reservation",
            call_id.as_str(),
            node_id.as_str(),
        ])
    ))
}

fn provider_tools(
    target: &PlannedTargetV1,
    feasibility: &MutationFeasibilitySet,
    policy: &MutationAttemptPolicy,
) -> Result<Vec<MutationFunctionTool>, MutationContractError> {
    policy
        .permitted_strategies
        .iter()
        .map(|strategy| {
            let evaluation =
                feasibility
                    .evaluation(*strategy)
                    .ok_or(MutationContractError::Invalid {
                        code: "mutation_strategy_feasibility_missing",
                    })?;
            if !evaluation.is_feasible() {
                return Err(MutationContractError::Invalid {
                    code: "mutation_strategy_not_feasible",
                });
            }
            provider_tool(*strategy, target, evaluation.maximum_candidate_bytes)
        })
        .collect()
}

fn provider_tool(
    strategy: MutationStrategy,
    target: &PlannedTargetV1,
    maximum_candidate_bytes: u64,
) -> Result<MutationFunctionTool, MutationContractError> {
    let path = target.path.as_str().to_owned();
    let expected_hash = target.operation.expected_content_hash().map(str::to_owned);
    let mut properties = BTreeMap::new();
    let required = match strategy {
        MutationStrategy::ApplyPatch { .. } => {
            properties.insert("path".into(), exact_string(path));
            properties.insert(
                "expected_content_hash".into(),
                exact_string(expected_hash.ok_or(MutationContractError::Invalid {
                    code: "mutation_expected_hash_missing",
                })?),
            );
            properties.insert("patch".into(), bounded_string(maximum_candidate_bytes));
            vec![
                "path".into(),
                "expected_content_hash".into(),
                "patch".into(),
            ]
        }
        MutationStrategy::ReplaceFile => {
            properties.insert("path".into(), exact_string(path));
            properties.insert(
                "expected_content_hash".into(),
                exact_string(expected_hash.ok_or(MutationContractError::Invalid {
                    code: "mutation_expected_hash_missing",
                })?),
            );
            properties.insert("content".into(), bounded_string(maximum_candidate_bytes));
            vec![
                "path".into(),
                "expected_content_hash".into(),
                "content".into(),
            ]
        }
        MutationStrategy::CreateFile => {
            properties.insert("path".into(), exact_string(path));
            properties.insert("content".into(), bounded_string(maximum_candidate_bytes));
            vec!["path".into(), "content".into()]
        }
        MutationStrategy::DeleteFile => {
            properties.insert("path".into(), exact_string(path));
            properties.insert(
                "expected_content_hash".into(),
                exact_string(expected_hash.ok_or(MutationContractError::Invalid {
                    code: "mutation_expected_hash_missing",
                })?),
            );
            vec!["path".into(), "expected_content_hash".into()]
        }
        MutationStrategy::MoveFile => {
            let destination =
                target
                    .operation
                    .destination()
                    .ok_or(MutationContractError::Invalid {
                        code: "mutation_move_destination_missing",
                    })?;
            properties.insert("source_path".into(), exact_string(path));
            properties.insert(
                "destination_path".into(),
                exact_string(destination.as_str().to_owned()),
            );
            properties.insert(
                "expected_content_hash".into(),
                exact_string(expected_hash.ok_or(MutationContractError::Invalid {
                    code: "mutation_expected_hash_missing",
                })?),
            );
            vec![
                "source_path".into(),
                "destination_path".into(),
                "expected_content_hash".into(),
            ]
        }
    };
    Ok(MutationFunctionTool {
        tool_type: ProviderToolKind::Function,
        function: MutationFunctionDefinition {
            name: strategy.tool(),
            description: format!(
                "Produce one bounded {} mutation for the authorized target",
                strategy.tool().as_str()
            ),
            strict: true,
            parameters: MutationObjectSchema {
                schema_type: JsonSchemaType::Object,
                properties,
                required,
                additional_properties: false,
            },
        },
    })
}

fn exact_string(value: String) -> MutationStringSchema {
    MutationStringSchema {
        schema_type: JsonSchemaType::String,
        enum_values: Some(vec![value]),
        min_length: None,
        max_length: None,
    }
}

fn bounded_string(maximum_bytes: u64) -> MutationStringSchema {
    MutationStringSchema {
        schema_type: JsonSchemaType::String,
        enum_values: None,
        min_length: Some(0),
        max_length: Some(maximum_bytes),
    }
}

fn tool_candidate_max_length(tool: &MutationFunctionTool) -> Option<u64> {
    let property = match tool.function.name {
        MutationToolName::ApplyPatch => "patch",
        MutationToolName::ReplaceFile | MutationToolName::CreateFile => "content",
        MutationToolName::DeleteFile | MutationToolName::MoveFile => return None,
    };
    tool.function
        .parameters
        .properties
        .get(property)
        .and_then(|schema| schema.max_length)
}

fn invocation_matches_target(
    arguments: &MaterializedMutationArguments,
    target: &PlannedTargetV1,
) -> bool {
    match (arguments, &target.operation) {
        (
            MaterializedMutationArguments::ApplyPatch { path, .. }
            | MaterializedMutationArguments::ReplaceFile { path, .. },
            TargetOperation::ModifyExisting { .. },
        )
        | (
            MaterializedMutationArguments::CreateFile { path, .. },
            TargetOperation::CreateFile { .. },
        )
        | (
            MaterializedMutationArguments::DeleteFile { path, .. },
            TargetOperation::DeleteFile { .. },
        ) => path == &target.path,
        (
            MaterializedMutationArguments::MoveFile {
                source_path,
                destination_path,
                ..
            },
            TargetOperation::MoveFile { destination, .. },
        ) => source_path == &target.path && destination_path == destination,
        _ => false,
    }
}

fn invocation_expected_hash_matches(
    arguments: &MaterializedMutationArguments,
    operation: &TargetOperation,
) -> bool {
    let observed = match arguments {
        MaterializedMutationArguments::ApplyPatch {
            expected_content_hash,
            ..
        }
        | MaterializedMutationArguments::ReplaceFile {
            expected_content_hash,
            ..
        }
        | MaterializedMutationArguments::DeleteFile {
            expected_content_hash,
            ..
        }
        | MaterializedMutationArguments::MoveFile {
            expected_content_hash,
            ..
        } => Some(expected_content_hash.as_str()),
        MaterializedMutationArguments::CreateFile { .. } => None,
    };
    observed == operation.expected_content_hash()
}

fn candidate_operation_matches_target(
    operation: &MutationCandidateOperation,
    target: &PlannedTargetV1,
) -> bool {
    match (operation, &target.operation) {
        (
            MutationCandidateOperation::ApplyPatch {
                path,
                expected_content_hash,
                ..
            }
            | MutationCandidateOperation::ReplaceFile {
                path,
                expected_content_hash,
                ..
            },
            TargetOperation::ModifyExisting {
                expected_content_hash: planned_hash,
            },
        )
        | (
            MutationCandidateOperation::DeleteFile {
                path,
                expected_content_hash,
            },
            TargetOperation::DeleteFile {
                expected_content_hash: planned_hash,
            },
        ) => path == &target.path && expected_content_hash == planned_hash,
        (
            MutationCandidateOperation::CreateFile { path, .. },
            TargetOperation::CreateFile { .. },
        ) => path == &target.path,
        (
            MutationCandidateOperation::MoveFile {
                source_path,
                destination_path,
                expected_content_hash,
            },
            TargetOperation::MoveFile {
                destination,
                expected_content_hash: planned_hash,
            },
        ) => {
            source_path == &target.path
                && destination_path == destination
                && expected_content_hash == planned_hash
        }
        _ => false,
    }
}

pub(crate) fn operation_transition_is_verified(
    operation: &MutationCandidateOperation,
    transitions: &BTreeMap<ProfilePath, MutationPathTransition>,
) -> bool {
    match operation {
        MutationCandidateOperation::ApplyPatch {
            path,
            expected_content_hash,
            expected_after_content,
            ..
        } => transitions.get(path).is_some_and(|transition| {
            transition.before.content_hash() == Some(expected_content_hash)
                && state_matches_artifact(&transition.after, expected_after_content)
        }),
        MutationCandidateOperation::ReplaceFile {
            path,
            expected_content_hash,
            content,
        } => transitions.get(path).is_some_and(|transition| {
            transition.before.content_hash() == Some(expected_content_hash)
                && state_matches_artifact(&transition.after, content)
                && content.content_hash != *expected_content_hash
        }),
        MutationCandidateOperation::CreateFile { path, content } => {
            transitions.get(path).is_some_and(|transition| {
                transition.before == MutationPathState::Absent
                    && state_matches_artifact(&transition.after, content)
            })
        }
        MutationCandidateOperation::DeleteFile {
            path,
            expected_content_hash,
        } => transitions.get(path).is_some_and(|transition| {
            transition.before.content_hash() == Some(expected_content_hash)
                && transition.after == MutationPathState::Absent
        }),
        MutationCandidateOperation::MoveFile {
            source_path,
            destination_path,
            expected_content_hash,
        } => {
            let source = transitions.get(source_path);
            let destination = transitions.get(destination_path);
            source.is_some_and(|transition| {
                transition.before.content_hash() == Some(expected_content_hash)
                    && transition.after == MutationPathState::Absent
            }) && destination.is_some_and(|transition| {
                transition.before == MutationPathState::Absent
                    && transition.after.content_hash() == Some(expected_content_hash)
            }) && source
                .zip(destination)
                .is_some_and(|(source, destination)| source.before == destination.after)
        }
    }
}

fn state_matches_artifact(state: &MutationPathState, artifact: &MutationArtifactReceipt) -> bool {
    matches!(
        state,
        MutationPathState::File {
            content_hash,
            byte_len,
            encoding,
        } if content_hash == &artifact.content_hash
            && byte_len == &artifact.byte_len
            && encoding == &artifact.encoding
    )
}

fn expected_verification_detail_hash(
    evidence: &MutationVerificationEvidence,
) -> Result<String, MutationContractError> {
    let canonical = canonical_json(&(
        evidence.schema_version,
        &evidence.verification_request_id,
        &evidence.application_id,
        &evidence.node_id,
        &evidence.target_id,
        &evidence.context_manifest_id,
        &evidence.attempt_id,
        &evidence.candidate_id,
        &evidence.repository_revision_before,
        &evidence.repository_revision_after,
        &evidence.repository_fingerprint_before,
        &evidence.repository_fingerprint_after,
        &evidence.changed_paths,
        &evidence.path_transitions,
    ))?;
    Ok(stable_sha256(&[
        "execution-protocol-v1:mutation-verification-detail",
        &canonical,
    ]))
}

fn expected_verification_evidence_id(
    evidence: &MutationVerificationEvidence,
) -> Result<EvidenceId, MutationContractError> {
    Ok(EvidenceId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:mutation-verification-evidence",
            evidence.verification_request_id.as_str(),
            evidence.candidate_id.as_str(),
            &evidence.detail_hash,
        ])
    )))
}

fn expected_convergence_id(
    convergence: &MutationConvergence,
) -> Result<EvidenceId, MutationContractError> {
    let canonical = canonical_json(&(
        convergence.schema_version,
        &convergence.node_id,
        &convergence.target_id,
        &convergence.context_manifest_id,
        &convergence.repository_revision,
        &convergence.repository_revision_after,
        &convergence.repository_drift,
        &convergence.final_attempt_id,
        convergence.final_attempt_index,
        &convergence.last_failure_revision_id,
        convergence.reason,
    ))?;
    Ok(EvidenceId::new(format!(
        "epv1:{}",
        stable_sha256(&["execution-protocol-v1:mutation-convergence", &canonical])
    )))
}

fn expected_readiness_convergence_id(
    convergence: &MutationReadinessConvergence,
) -> Result<EvidenceId, MutationContractError> {
    let canonical = canonical_json(&(
        convergence.schema_version,
        &convergence.execution_id,
        convergence.execution_attempt,
        &convergence.node_id,
        convergence.node_attempt,
        &convergence.target_id,
        &convergence.context_manifest_id,
        &convergence.repository_revision,
        &convergence.feasibility_hash,
        &convergence.attempt_id,
        convergence.attempt_index,
        &convergence.reason,
    ))?;
    Ok(EvidenceId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:mutation-readiness-convergence",
            &canonical,
        ])
    )))
}

fn expected_readiness_failure_revision_id(
    convergence: &MutationReadinessConvergence,
) -> FailureRevisionId {
    FailureRevisionId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:mutation-readiness-failure",
            convergence.convergence_id.as_str(),
        ])
    ))
}

fn candidate_operation(arguments: &MaterializedMutationArguments) -> MutationCandidateOperation {
    match arguments {
        MaterializedMutationArguments::ApplyPatch {
            path,
            expected_content_hash,
            patch,
            expected_after_content,
        } => MutationCandidateOperation::ApplyPatch {
            path: path.clone(),
            expected_content_hash: expected_content_hash.clone(),
            patch: patch.receipt(),
            expected_after_content: expected_after_content.receipt(),
        },
        MaterializedMutationArguments::ReplaceFile {
            path,
            expected_content_hash,
            content,
        } => MutationCandidateOperation::ReplaceFile {
            path: path.clone(),
            expected_content_hash: expected_content_hash.clone(),
            content: content.receipt(),
        },
        MaterializedMutationArguments::CreateFile { path, content } => {
            MutationCandidateOperation::CreateFile {
                path: path.clone(),
                content: content.receipt(),
            }
        }
        MaterializedMutationArguments::DeleteFile {
            path,
            expected_content_hash,
        } => MutationCandidateOperation::DeleteFile {
            path: path.clone(),
            expected_content_hash: expected_content_hash.clone(),
        },
        MaterializedMutationArguments::MoveFile {
            source_path,
            destination_path,
            expected_content_hash,
        } => MutationCandidateOperation::MoveFile {
            source_path: source_path.clone(),
            destination_path: destination_path.clone(),
            expected_content_hash: expected_content_hash.clone(),
        },
    }
}

fn candidate_artifacts_are_valid(operation: &MutationCandidateOperation) -> bool {
    let valid = |artifact: &MutationArtifactReceipt| {
        is_sha256(&artifact.content_hash)
            && artifact.handle.validate_for_artifact(
                &artifact.content_hash,
                artifact.byte_len,
                artifact.encoding,
            )
            && matches!(
                artifact.encoding,
                TextEncoding::Utf8 | TextEncoding::Utf8WithBom
            )
    };
    match operation {
        MutationCandidateOperation::ApplyPatch {
            patch,
            expected_after_content,
            expected_content_hash,
            ..
        } => {
            valid(patch)
                && valid(expected_after_content)
                && expected_after_content.content_hash != *expected_content_hash
        }
        MutationCandidateOperation::ReplaceFile { content, .. }
        | MutationCandidateOperation::CreateFile { content, .. } => valid(content),
        MutationCandidateOperation::DeleteFile { .. }
        | MutationCandidateOperation::MoveFile { .. } => true,
    }
}

fn expected_candidate_hash(
    candidate: &MutationCandidateRecord,
) -> Result<String, MutationContractError> {
    let canonical = canonical_json(&(
        candidate.schema_version,
        &candidate.action_id,
        &candidate.call_id,
        &candidate.node_id,
        &candidate.target_id,
        &candidate.context_manifest_id,
        &candidate.repository_revision,
        &candidate.attempt_id,
        candidate.attempt_index,
        candidate.strategy,
        candidate.complete,
        candidate.truncated,
        &candidate.operation,
    ))?;
    Ok(stable_sha256(&[
        "execution-protocol-v1:mutation-candidate-payload",
        &canonical,
    ]))
}

fn expected_candidate_id(
    candidate: &MutationCandidateRecord,
) -> Result<MutationCandidateId, MutationContractError> {
    Ok(MutationCandidateId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:mutation-candidate",
            candidate.attempt_id.as_str(),
            &candidate.candidate_hash,
        ])
    )))
}

fn canonical_json<T: Serialize>(value: &T) -> Result<String, MutationContractError> {
    serde_json::to_string(value).map_err(|_| MutationContractError::Serialization)
}

fn expected_persistence_receipt_hash(
    content_address: &str,
    store_locator_hash: &str,
    content_hash: &str,
    byte_len: u64,
    encoding: TextEncoding,
) -> String {
    let encoding = match encoding {
        TextEncoding::Utf8 => "utf8",
        TextEncoding::Utf8WithBom => "utf8_with_bom",
        TextEncoding::UnknownText => "unknown_text",
    };
    stable_sha256(&[
        "execution-protocol-v1:mutation-artifact-persisted",
        content_address,
        store_locator_hash,
        content_hash,
        &byte_len.to_string(),
        encoding,
    ])
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
