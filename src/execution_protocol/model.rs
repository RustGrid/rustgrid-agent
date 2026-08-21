use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    ActionEnvelope, ActionId, ContextManifest, DiscoveryGoal, DiscoveryState, EffectId, EventId,
    EvidenceId, ExecutionId, FailureRevisionId, FinalizationPolicyV1, ImplementationState,
    ModelCallId, MutationLedger, NodeId, PlanGraphBudgetContract, PlanningState,
    PreparedPlanningAction, ProofId, ProtocolViolation, PublicationStateV1, RepositoryProfile,
    RepositoryRevisionId, ReviewStateV1, ValidationPolicyV1, ValidationState,
};

pub(crate) const EXECUTION_PROTOCOL_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProtocolStage {
    Profiling,
    Discovery,
    Planning,
    Implementation,
    Validation,
    Repair,
    Review,
    Publication,
    Terminal,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProfileStep {
    InspectingMetadata,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiscoveryStep {
    NeedCandidates,
    NeedGroundedReads,
    NeedRelations,
    ReadyToSynthesize,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanningStep {
    ReadyToSynthesize,
    AwaitingPlan,
    EvidenceGap,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImplementationStep {
    SelectTarget,
    PrepareContext,
    GenerateCandidate,
    ApplyCandidate,
    VerifyRepository,
    Barrier,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationStep {
    ScheduleGate,
    Running,
    Completed,
    DiagnoseFailure,
    AllRequiredPassed,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepairStep {
    RankCandidates,
    CheckEligibility,
    TargetSelected,
    ExecuteTarget,
    ScheduleRerun,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewStep {
    DiffReview,
    CompletionEvaluation,
    PublicationEligibility,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublicationStep {
    Commit,
    Push,
    PullRequest,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(
    tag = "stage",
    content = "step",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ProtocolPosition {
    Profiling(ProfileStep),
    Discovery(DiscoveryStep),
    Planning(PlanningStep),
    Implementation(ImplementationStep),
    Validation(ValidationStep),
    Repair(RepairStep),
    Review(ReviewStep),
    Publication(PublicationStep),
    Terminal,
}

impl ProtocolPosition {
    pub(crate) const fn stage(self) -> ProtocolStage {
        match self {
            Self::Profiling(_) => ProtocolStage::Profiling,
            Self::Discovery(_) => ProtocolStage::Discovery,
            Self::Planning(_) => ProtocolStage::Planning,
            Self::Implementation(_) => ProtocolStage::Implementation,
            Self::Validation(_) => ProtocolStage::Validation,
            Self::Repair(_) => ProtocolStage::Repair,
            Self::Review(_) => ProtocolStage::Review,
            Self::Publication(_) => ProtocolStage::Publication,
            Self::Terminal => ProtocolStage::Terminal,
        }
    }

    pub(crate) const fn initial(stage: ProtocolStage) -> Self {
        match stage {
            ProtocolStage::Profiling => Self::Profiling(ProfileStep::InspectingMetadata),
            ProtocolStage::Discovery => Self::Discovery(DiscoveryStep::NeedCandidates),
            ProtocolStage::Planning => Self::Planning(PlanningStep::ReadyToSynthesize),
            ProtocolStage::Implementation => Self::Implementation(ImplementationStep::SelectTarget),
            ProtocolStage::Validation => Self::Validation(ValidationStep::ScheduleGate),
            ProtocolStage::Repair => Self::Repair(RepairStep::RankCandidates),
            ProtocolStage::Review => Self::Review(ReviewStep::DiffReview),
            ProtocolStage::Publication => Self::Publication(PublicationStep::Commit),
            ProtocolStage::Terminal => Self::Terminal,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NodeKind {
    Discovery,
    Planning,
    Implementation,
    Validation,
    ValidationRepair,
    Review,
    CompletionEvaluation,
    Publication,
}

impl NodeKind {
    pub(crate) const fn stage(self) -> ProtocolStage {
        match self {
            Self::Discovery => ProtocolStage::Discovery,
            Self::Planning => ProtocolStage::Planning,
            Self::Implementation => ProtocolStage::Implementation,
            Self::Validation => ProtocolStage::Validation,
            Self::ValidationRepair => ProtocolStage::Repair,
            Self::Review | Self::CompletionEvaluation => ProtocolStage::Review,
            Self::Publication => ProtocolStage::Publication,
        }
    }

    pub(crate) const fn requires_model(self) -> bool {
        matches!(
            self,
            Self::Discovery
                | Self::Planning
                | Self::Implementation
                | Self::ValidationRepair
                | Self::Review
                | Self::CompletionEvaluation
        )
    }

    pub(crate) const fn is_implementation(self) -> bool {
        matches!(self, Self::Implementation)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum NodeState {
    Pending,
    Ready,
    Active {
        attempt: u32,
    },
    Waiting {
        attempt: u32,
        effect_id: EffectId,
    },
    Succeeded {
        proof_id: ProofId,
    },
    FailedRecoverable {
        failure_revision_id: FailureRevisionId,
    },
    FailedTerminal {
        failure_revision_id: FailureRevisionId,
    },
    Superseded {
        repository_revision: RepositoryRevisionId,
    },
    Skipped {
        proof_id: ProofId,
    },
}

impl NodeState {
    pub(crate) const fn owns_execution(&self) -> bool {
        matches!(self, Self::Active { .. } | Self::Waiting { .. })
    }

    pub(crate) const fn satisfies_dependency(&self) -> bool {
        matches!(self, Self::Succeeded { .. } | Self::Skipped { .. })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MissionBudgetContract {
    pub(crate) max_model_calls: u32,
    pub(crate) max_cost_micros: u64,
    pub(crate) max_duration_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NodeBudgetContract {
    pub(crate) max_model_calls: u32,
    pub(crate) max_cost_micros: u64,
    pub(crate) max_duration_ms: u64,
    pub(crate) max_mutation_attempts: u32,
    pub(crate) max_context_rebuilds: u32,
    pub(crate) max_input_tokens_per_call: u32,
    pub(crate) max_output_tokens_per_call: u32,
}

impl NodeBudgetContract {
    pub(crate) const fn deterministic() -> Self {
        Self {
            max_model_calls: 0,
            max_cost_micros: 0,
            max_duration_ms: 0,
            max_mutation_attempts: 0,
            max_context_rebuilds: 0,
            max_input_tokens_per_call: 0,
            max_output_tokens_per_call: 0,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetUsage {
    pub(crate) model_calls_reserved: u32,
    pub(crate) model_calls_consumed: u32,
    pub(crate) cost_micros_reserved: u64,
    pub(crate) cost_micros_consumed: u64,
    pub(crate) duration_ms_reserved: u64,
    pub(crate) duration_ms_consumed: u64,
    pub(crate) mutation_attempts: u32,
    pub(crate) context_rebuilds: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NodeSpec {
    pub(crate) id: NodeId,
    pub(crate) kind: NodeKind,
    pub(crate) required: bool,
    pub(crate) dependencies: Vec<NodeId>,
    pub(crate) budget: NodeBudgetContract,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionNode {
    pub(crate) id: NodeId,
    pub(crate) kind: NodeKind,
    pub(crate) required: bool,
    pub(crate) dependencies: Vec<NodeId>,
    pub(crate) budget: NodeBudgetContract,
    pub(crate) usage: BudgetUsage,
    pub(crate) state: NodeState,
    pub(crate) attempts_started: u32,
}

impl From<NodeSpec> for ExecutionNode {
    fn from(spec: NodeSpec) -> Self {
        Self {
            id: spec.id,
            kind: spec.kind,
            required: spec.required,
            dependencies: spec.dependencies,
            budget: spec.budget,
            usage: BudgetUsage::default(),
            state: NodeState::Pending,
            attempts_started: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProofKind {
    RepositoryProfile,
    DiscoveryImpactMap,
    PlanAccepted,
    MutationVerified,
    AlreadySatisfied,
    ImplementationBarrier,
    ValidationPassed,
    ValidationFailure,
    RepairEligibility,
    RepairVerified,
    ValidationRerunScheduled,
    RequiredValidationPassed,
    ReviewCompleted,
    CompletionEvaluated,
    PublicationEligibility,
    PublicationCompleted,
    NoOpSatisfied,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProofRecord {
    pub(crate) id: ProofId,
    pub(crate) kind: ProofKind,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) node_ids: Vec<NodeId>,
    pub(crate) related_proof_ids: Vec<ProofId>,
    pub(crate) related_evidence_ids: Vec<EvidenceId>,
    pub(crate) detail_hash: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelCallAdmission {
    pub(crate) call_id: ModelCallId,
    pub(crate) node_id: NodeId,
    pub(crate) action_id: ActionId,
    pub(crate) payload_hash: String,
    pub(crate) input_tokens: u32,
    pub(crate) output_tokens: u32,
    pub(crate) reserved_cost_micros: u64,
    pub(crate) duration_allowance_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedDiscoveryAction {
    pub(crate) context: ContextManifest,
    pub(crate) envelope: ActionEnvelope,
    pub(crate) admission: ModelCallAdmission,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ModelCallState {
    Admitted,
    Reserved,
    Dispatched,
    ReconciledConsumed {
        actual_cost_micros: u64,
        duration_ms: u64,
    },
    ReconciledReleased,
}

impl ModelCallState {
    pub(crate) const fn owns_reservation(&self) -> bool {
        matches!(self, Self::Reserved | Self::Dispatched)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelCallRecord {
    pub(crate) admission: ModelCallAdmission,
    pub(crate) state: ModelCallState,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ModelCallReconciliation {
    Consumed {
        actual_cost_micros: u64,
        duration_ms: u64,
    },
    ReleasedUncontacted,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MissionOutcomeV1 {
    Succeeded,
    SucceededNoOp,
    PartialReviewable,
    BlockedNoDiff,
    NoValidRepair,
    InsufficientEvidence,
    ValidationFailed,
    BudgetBlocked,
    InfrastructureFailed,
    PublicationFailed,
    Canceled,
}

impl MissionOutcomeV1 {
    pub(crate) const fn is_success(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::SucceededNoOp | Self::PartialReviewable
        )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ProcessHealth {
    Healthy,
    Degraded { code: String },
    Failed { code: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FirstFatalBlocker {
    pub(crate) category: String,
    pub(crate) code: String,
    pub(crate) node_id: Option<NodeId>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MissionResult {
    Succeeded {
        publication_proof_id: ProofId,
    },
    SucceededNoOp {
        no_op_proof_id: ProofId,
    },
    PartialReviewable {
        publication_proof_id: ProofId,
        external_review_reason_code: String,
    },
    BlockedNoDiff {
        failure: FirstFatalBlocker,
    },
    NoValidRepair {
        failure: FirstFatalBlocker,
    },
    InsufficientEvidence {
        failure: FirstFatalBlocker,
    },
    ValidationFailed {
        failure: FirstFatalBlocker,
    },
    BudgetBlocked {
        node_id: NodeId,
        failure: FirstFatalBlocker,
    },
    InfrastructureFailed {
        failure: FirstFatalBlocker,
    },
    PublicationFailed {
        failure: FirstFatalBlocker,
    },
    Canceled {
        cancellation_reason_code: String,
    },
}

impl MissionResult {
    pub(crate) const fn outcome(&self) -> MissionOutcomeV1 {
        match self {
            Self::Succeeded { .. } => MissionOutcomeV1::Succeeded,
            Self::SucceededNoOp { .. } => MissionOutcomeV1::SucceededNoOp,
            Self::PartialReviewable { .. } => MissionOutcomeV1::PartialReviewable,
            Self::BlockedNoDiff { .. } => MissionOutcomeV1::BlockedNoDiff,
            Self::NoValidRepair { .. } => MissionOutcomeV1::NoValidRepair,
            Self::InsufficientEvidence { .. } => MissionOutcomeV1::InsufficientEvidence,
            Self::ValidationFailed { .. } => MissionOutcomeV1::ValidationFailed,
            Self::BudgetBlocked { .. } => MissionOutcomeV1::BudgetBlocked,
            Self::InfrastructureFailed { .. } => MissionOutcomeV1::InfrastructureFailed,
            Self::PublicationFailed { .. } => MissionOutcomeV1::PublicationFailed,
            Self::Canceled { .. } => MissionOutcomeV1::Canceled,
        }
    }

    pub(crate) const fn proof_id(&self) -> Option<&ProofId> {
        match self {
            Self::Succeeded {
                publication_proof_id,
            }
            | Self::PartialReviewable {
                publication_proof_id,
                ..
            } => Some(publication_proof_id),
            Self::SucceededNoOp { no_op_proof_id } => Some(no_op_proof_id),
            _ => None,
        }
    }

    pub(crate) const fn first_fatal_blocker(&self) -> Option<&FirstFatalBlocker> {
        match self {
            Self::BlockedNoDiff { failure }
            | Self::NoValidRepair { failure }
            | Self::InsufficientEvidence { failure }
            | Self::ValidationFailed { failure }
            | Self::BudgetBlocked { failure, .. }
            | Self::InfrastructureFailed { failure }
            | Self::PublicationFailed { failure } => Some(failure),
            Self::Succeeded { .. }
            | Self::SucceededNoOp { .. }
            | Self::PartialReviewable { .. }
            | Self::Canceled { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalResult {
    pub(crate) mission: MissionResult,
    pub(crate) process_health: ProcessHealth,
    pub(crate) reason_code: String,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) remaining_work: Vec<NodeId>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetLedger {
    pub(crate) mission_usage: BudgetUsage,
    pub(crate) model_calls: BTreeMap<ModelCallId, ModelCallRecord>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredProtocolEvent {
    pub(crate) envelope: super::ProtocolEventEnvelope,
    pub(crate) payload_hash: String,
}

/// Selects whether the aggregate is a compatibility scaffold used by the
/// side-by-side conformance work or a fully policy-bound Protocol v1 attempt.
/// Production runners must accept only `StrictV1`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionProtocolModeV1 {
    CompatibilityScaffold,
    StrictV1,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionState {
    /// In-memory provenance marker. It is intentionally absent from the wire:
    /// deserialized snapshots must be replayed from a separately trusted
    /// bootstrap before they can drive decisions or reductions.
    #[serde(skip)]
    pub(super) trusted_bootstrap: bool,
    pub(crate) protocol_version: u16,
    pub(crate) protocol_mode: ExecutionProtocolModeV1,
    pub(crate) aggregate_revision: u64,
    pub(crate) execution_id: ExecutionId,
    pub(crate) execution_attempt: u32,
    pub(crate) initial_repository_revision: RepositoryRevisionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) position: ProtocolPosition,
    pub(crate) latest_transition_proof: Option<ProofId>,
    pub(crate) mission_budget: MissionBudgetContract,
    pub(crate) plan_graph_budget: PlanGraphBudgetContract,
    /// Exact trusted mission/search authority. Compatibility scaffolds retain
    /// `None`; strict execution requires and revalidates `Some`.
    pub(crate) requested_discovery_goal: Option<DiscoveryGoal>,
    pub(crate) validation_policy: Option<ValidationPolicyV1>,
    pub(crate) finalization_policy: Option<FinalizationPolicyV1>,
    pub(crate) nodes: BTreeMap<NodeId, ExecutionNode>,
    pub(crate) node_order: Vec<NodeId>,
    pub(crate) proofs: BTreeMap<ProofId, ProofRecord>,
    pub(crate) repository_profile: Option<RepositoryProfile>,
    pub(crate) discovery: Option<DiscoveryState>,
    pub(crate) current_discovery_action: Option<PreparedDiscoveryAction>,
    pub(crate) planning: Option<PlanningState>,
    pub(crate) current_planning_action: Option<PreparedPlanningAction>,
    pub(crate) implementation: Option<ImplementationState>,
    pub(crate) mutation: MutationLedger,
    pub(crate) validation: Option<ValidationState>,
    pub(crate) review: Option<ReviewStateV1>,
    pub(crate) publication: Option<PublicationStateV1>,
    pub(crate) budgets: BudgetLedger,
    pub(crate) terminal: Option<CanonicalResult>,
    pub(crate) event_log: Vec<StoredProtocolEvent>,
    pub(crate) event_payload_hashes: BTreeMap<EventId, String>,
}

impl ExecutionState {
    // Bootstrap lists every separately trusted protocol contract explicitly;
    // grouping them would create a second, less precise authority container.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bootstrap(
        execution_id: ExecutionId,
        execution_attempt: u32,
        repository_revision: RepositoryRevisionId,
        mission_budget: MissionBudgetContract,
        discovery_budget: NodeBudgetContract,
        planning_budget: NodeBudgetContract,
        plan_graph_budget: PlanGraphBudgetContract,
        validation_policy: Option<ValidationPolicyV1>,
    ) -> Self {
        Self::bootstrap_with_finalization_policy(
            execution_id,
            execution_attempt,
            repository_revision,
            mission_budget,
            discovery_budget,
            planning_budget,
            plan_graph_budget,
            validation_policy,
            None,
        )
    }

    // Finalization authority is a separately trusted bootstrap contract. The
    // legacy constructor deliberately supplies `None` so pre-Phase-7 fixtures
    // retain their original serialized/replay behavior.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bootstrap_with_finalization_policy(
        execution_id: ExecutionId,
        execution_attempt: u32,
        repository_revision: RepositoryRevisionId,
        mission_budget: MissionBudgetContract,
        discovery_budget: NodeBudgetContract,
        planning_budget: NodeBudgetContract,
        plan_graph_budget: PlanGraphBudgetContract,
        validation_policy: Option<ValidationPolicyV1>,
        finalization_policy: Option<FinalizationPolicyV1>,
    ) -> Self {
        Self::bootstrap_with_mode(
            execution_id,
            execution_attempt,
            repository_revision,
            mission_budget,
            discovery_budget,
            planning_budget,
            plan_graph_budget,
            None,
            validation_policy,
            finalization_policy,
            ExecutionProtocolModeV1::CompatibilityScaffold,
        )
    }

    /// Creates a production-eligible Protocol v1 aggregate.
    ///
    /// Unlike the compatibility constructors, this rejects missing or
    /// structurally invalid validation/finalization authority at revision zero.
    /// Repository-specific validation-policy membership is revalidated after
    /// the canonical repository profile is recorded.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bootstrap_strict_v1(
        execution_id: ExecutionId,
        execution_attempt: u32,
        repository_revision: RepositoryRevisionId,
        mission_budget: MissionBudgetContract,
        discovery_budget: NodeBudgetContract,
        planning_budget: NodeBudgetContract,
        plan_graph_budget: PlanGraphBudgetContract,
        requested_discovery_goal: DiscoveryGoal,
        validation_policy: ValidationPolicyV1,
        finalization_policy: FinalizationPolicyV1,
    ) -> Result<Self, ProtocolViolation> {
        requested_discovery_goal.validate()?;
        validation_policy.validate_structure()?;
        finalization_policy.validate()?;
        if finalization_policy.publication.base_repository_revision != repository_revision {
            return Err(ProtocolViolation::ReviewContract {
                code: "strict_v1_publication_base_revision_mismatch",
            });
        }
        let state = Self::bootstrap_with_mode(
            execution_id,
            execution_attempt,
            repository_revision,
            mission_budget,
            discovery_budget,
            planning_budget,
            plan_graph_budget,
            Some(requested_discovery_goal),
            Some(validation_policy),
            Some(finalization_policy),
            ExecutionProtocolModeV1::StrictV1,
        );
        state.validate_strict_bootstrap_contract()?;
        state.validate_invariants()?;
        Ok(state)
    }

    #[allow(clippy::too_many_arguments)]
    fn bootstrap_with_mode(
        execution_id: ExecutionId,
        execution_attempt: u32,
        repository_revision: RepositoryRevisionId,
        mission_budget: MissionBudgetContract,
        discovery_budget: NodeBudgetContract,
        planning_budget: NodeBudgetContract,
        plan_graph_budget: PlanGraphBudgetContract,
        requested_discovery_goal: Option<DiscoveryGoal>,
        validation_policy: Option<ValidationPolicyV1>,
        finalization_policy: Option<FinalizationPolicyV1>,
        protocol_mode: ExecutionProtocolModeV1,
    ) -> Self {
        let discovery_id = NodeId::new("protocol-v1:discovery");
        let planning_id = NodeId::new("protocol-v1:planning");
        let bootstrap_nodes = [
            NodeSpec {
                id: discovery_id.clone(),
                kind: NodeKind::Discovery,
                required: true,
                dependencies: Vec::new(),
                budget: discovery_budget,
            },
            NodeSpec {
                id: planning_id,
                kind: NodeKind::Planning,
                required: true,
                dependencies: vec![discovery_id],
                budget: planning_budget,
            },
        ];
        let nodes = bootstrap_nodes
            .iter()
            .cloned()
            .map(|spec| (spec.id.clone(), ExecutionNode::from(spec)))
            .collect();
        Self {
            trusted_bootstrap: true,
            protocol_version: EXECUTION_PROTOCOL_VERSION,
            protocol_mode,
            aggregate_revision: 0,
            execution_id,
            execution_attempt,
            initial_repository_revision: repository_revision.clone(),
            repository_revision,
            position: ProtocolPosition::Profiling(ProfileStep::InspectingMetadata),
            latest_transition_proof: None,
            mission_budget,
            plan_graph_budget,
            requested_discovery_goal,
            validation_policy,
            finalization_policy,
            nodes,
            node_order: bootstrap_nodes.into_iter().map(|spec| spec.id).collect(),
            proofs: BTreeMap::new(),
            repository_profile: None,
            discovery: None,
            current_discovery_action: None,
            planning: None,
            current_planning_action: None,
            implementation: None,
            mutation: MutationLedger::default(),
            validation: None,
            review: None,
            publication: None,
            budgets: BudgetLedger::default(),
            terminal: None,
            event_log: Vec::new(),
            event_payload_hashes: BTreeMap::new(),
        }
    }

    pub(crate) fn validate_strict_bootstrap_contract(&self) -> Result<(), ProtocolViolation> {
        if self.protocol_mode != ExecutionProtocolModeV1::StrictV1 {
            return Err(ProtocolViolation::Invariant {
                code: "strict_v1_bootstrap_required",
                detail: "production execution requires strict Protocol v1 bootstrap authority"
                    .into(),
            });
        }
        let requested_goal =
            self.requested_discovery_goal
                .as_ref()
                .ok_or(ProtocolViolation::DiscoveryContract {
                    code: "strict_v1_discovery_goal_missing",
                })?;
        requested_goal.validate()?;
        if self
            .discovery
            .as_ref()
            .is_some_and(|discovery| &discovery.goal != requested_goal)
        {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "strict_v1_discovery_goal_mismatch",
            });
        }
        let validation_policy =
            self.validation_policy
                .as_ref()
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "strict_v1_validation_policy_missing",
                })?;
        validation_policy.validate_structure()?;
        if let Some(profile) = self.repository_profile.as_ref() {
            validation_policy.validate(profile)?;
        }
        let finalization_policy =
            self.finalization_policy
                .as_ref()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "strict_v1_finalization_policy_missing",
                })?;
        finalization_policy.validate()?;
        if finalization_policy.publication.base_repository_revision
            != self.initial_repository_revision
        {
            return Err(ProtocolViolation::ReviewContract {
                code: "strict_v1_publication_base_revision_mismatch",
            });
        }
        Ok(())
    }

    pub(crate) fn stage(&self) -> ProtocolStage {
        self.position.stage()
    }

    pub(crate) fn next_sequence(&self) -> u64 {
        self.event_log
            .last()
            .map_or(1, |event| event.envelope.sequence.saturating_add(1))
    }

    pub(crate) fn node(&self, node_id: &NodeId) -> Option<&ExecutionNode> {
        self.nodes.get(node_id)
    }

    pub(crate) fn active_node(&self) -> Option<&ExecutionNode> {
        self.node_order
            .iter()
            .filter_map(|node_id| self.nodes.get(node_id))
            .find(|node| node.state.owns_execution())
    }

    pub(crate) fn active_reservation_count(&self) -> usize {
        self.budgets
            .model_calls
            .values()
            .filter(|record| record.state.owns_reservation())
            .count()
    }

    pub(crate) fn required_nodes(&self, kind: NodeKind) -> Vec<&ExecutionNode> {
        self.node_order
            .iter()
            .filter_map(|node_id| self.nodes.get(node_id))
            .filter(|node| node.required && node.kind == kind)
            .collect()
    }

    pub(crate) fn proof_kind(&self, proof_id: &ProofId) -> Option<ProofKind> {
        self.proofs.get(proof_id).map(|proof| proof.kind)
    }

    pub(crate) fn succeeded_proof_kind(&self, node: &ExecutionNode) -> Option<ProofKind> {
        match &node.state {
            NodeState::Succeeded { proof_id } | NodeState::Skipped { proof_id } => {
                self.proof_kind(proof_id)
            }
            _ => None,
        }
    }

    pub(crate) fn unresolved_required_nodes(&self) -> BTreeSet<NodeId> {
        self.nodes
            .values()
            .filter(|node| node.required && !node.state.satisfies_dependency())
            .map(|node| node.id.clone())
            .collect()
    }
}
