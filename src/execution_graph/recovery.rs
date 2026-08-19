#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    ReadFile,
    SearchRepository,
    ApplyPatch,
    CreateFile,
    DeleteFile,
    RenameFile,
    MoveFile,
    RunFocusedCommand,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetStateProbe {
    pub operation: TargetOperation,
    pub target_path: RepositoryPath,
    pub target_exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_exists: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_content_hash: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_content_hash: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_result_content_hash: Option<ContentHash>,
    pub repository_fingerprint: RepositoryFingerprint,
}

impl TargetStateProbe {
    pub fn inspection_outcome(&self) -> TargetInspectionOutcome {
        let conflict = |code: &str, message: &str| TargetInspectionOutcome::OperationConflict {
            conflict: TargetOperationConflict {
                code: code.to_owned(),
                operation: self.operation.clone(),
                target_path: self.target_path.clone(),
                source_path: self.operation.source_path().map(str::to_owned),
                message: message.to_owned(),
                recoverable: true,
            },
        };
        match &self.operation {
            TargetOperation::ModifyExisting
                if self.target_exists
                    && self.expected_result_content_hash.is_some()
                    && self.expected_result_content_hash == self.target_content_hash =>
            {
                TargetInspectionOutcome::AlreadyApplied
            }
            TargetOperation::ModifyExisting if self.target_exists => {
                TargetInspectionOutcome::ExistingTargetLoaded
            }
            TargetOperation::ModifyExisting => conflict(
                "expected_existing_target_missing",
                "the accepted modify target is absent",
            ),
            TargetOperation::CreateNew if !self.target_exists => {
                TargetInspectionOutcome::NewTargetConfirmedAbsent
            }
            TargetOperation::CreateNew
                if self.expected_result_content_hash.is_some()
                    && self.expected_result_content_hash == self.target_content_hash =>
            {
                TargetInspectionOutcome::AlreadyApplied
            }
            TargetOperation::CreateNew => conflict(
                "create_target_already_exists",
                "the accepted create destination exists without matching mutation intent",
            ),
            TargetOperation::DeleteExisting if self.target_exists => {
                TargetInspectionOutcome::ExistingTargetLoaded
            }
            TargetOperation::DeleteExisting => TargetInspectionOutcome::AlreadyApplied,
            TargetOperation::Rename { .. } | TargetOperation::Move { .. }
                if self.source_exists == Some(true) && !self.target_exists =>
            {
                TargetInspectionOutcome::ExistingTargetLoaded
            }
            TargetOperation::Rename { .. } | TargetOperation::Move { .. }
                if self.source_exists == Some(false)
                    && self.target_exists
                    && self.expected_result_content_hash.is_some()
                    && self.expected_result_content_hash == self.target_content_hash =>
            {
                TargetInspectionOutcome::AlreadyApplied
            }
            TargetOperation::Rename { .. } | TargetOperation::Move { .. }
                if self.source_exists == Some(false) && self.target_exists =>
            {
                conflict(
                    "destination_content_mismatch",
                    "the destination does not match the accepted source evidence",
                )
            }
            TargetOperation::Rename { .. } | TargetOperation::Move { .. }
                if self.source_exists == Some(true) =>
            {
                conflict(
                    "destination_already_exists",
                    "the accepted destination already exists",
                )
            }
            TargetOperation::Rename { .. } | TargetOperation::Move { .. } => conflict(
                "expected_source_target_missing",
                "the accepted source and destination are both absent",
            ),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetOperationConflict {
    pub code: String,
    pub operation: TargetOperation,
    pub target_path: RepositoryPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<RepositoryPath>,
    pub message: String,
    #[serde(default)]
    pub recoverable: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TargetInspectionOutcome {
    ExistingTargetLoaded,
    NewTargetConfirmedAbsent,
    AlreadyApplied,
    OperationConflict { conflict: TargetOperationConflict },
    UnsafePath,
    #[default]
    InspectionInfrastructureFailure,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryOperationOutcome {
    Applied,
    AlreadyApplied,
    Repaired,
    Rejected,
    Conflict,
    #[default]
    Failed,
}

/// Durable lifecycle of one repository mutation. Producing or applying a
/// payload is deliberately not sufficient to satisfy execution dependencies;
/// only deterministic post-write verification may produce `Verified`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryMutationLifecycle {
    #[default]
    Proposed,
    Validated,
    AppliedUnverified,
    Verified,
    Rejected,
    Conflict,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct OperationIntent {
    pub operation: TargetOperation,
    pub target_path: RepositoryPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_result_hash: Option<ContentHash>,
    #[serde(default)]
    pub satisfied_intent: SatisfiedIntent,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationReadinessProof {
    pub graph_revision: u64,
    #[serde(default)]
    pub satisfied_implementation_nodes: Vec<ExecutionNodeId>,
    #[serde(default)]
    pub required_implementation_nodes: usize,
    #[serde(default)]
    pub completed_implementation_nodes: usize,
    #[serde(default)]
    pub unresolved_nodes: Vec<ExecutionNodeId>,
    #[serde(default)]
    pub repository_fingerprint: RepositoryFingerprint,
}

impl ValidationReadinessProof {
    pub fn is_satisfied(&self) -> bool {
        self.unresolved_nodes.is_empty()
            && self.required_implementation_nodes == self.completed_implementation_nodes
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "evidence", rename_all = "snake_case")]
pub enum SuccessfulOperationEvidence {
    Applied {
        before: RepositoryFingerprint,
        after: RepositoryFingerprint,
    },
    AlreadyApplied {
        observed: RepositoryFingerprint,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RepositoryOperationResult {
    Verified {
        outcome: RepositoryOperationOutcome,
        evidence: SuccessfulOperationEvidence,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_result_hash: Option<ContentHash>,
        semantic_id: String,
        attempt: u32,
        completed_at: String,
    },
    Rejected,
    Conflict,
}

impl RepositoryOperationResult {
    pub const fn lifecycle(&self) -> RepositoryMutationLifecycle {
        match self {
            Self::Verified { .. } => RepositoryMutationLifecycle::Verified,
            Self::Rejected => RepositoryMutationLifecycle::Rejected,
            Self::Conflict => RepositoryMutationLifecycle::Conflict,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SatisfiedIntent {
    #[default]
    OriginalImplementation,
    ValidationRepair,
    MutationFallback,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationIntentKind {
    #[default]
    InitialImplementation,
    MutationFallback,
    ValidationRepair,
    DiffReviewRepair,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct MutationEventContext {
    pub node_id: ExecutionNodeId,
    pub intent_kind: MutationIntentKind,
    pub target_id: TargetId,
    pub target_path: RepositoryPath,
    pub repository_fingerprint: RepositoryFingerprint,
}

impl SatisfiedIntent {
    pub const fn repair_intent_kind(self) -> Option<RepairIntentKind> {
        match self {
            Self::OriginalImplementation => None,
            Self::ValidationRepair => Some(RepairIntentKind::ValidationRepair),
            Self::MutationFallback => Some(RepairIntentKind::MutationApplicationFallback),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairIntentKind {
    #[default]
    MutationApplicationFallback,
    ValidationRepair,
    DiffReviewRepair,
    PlanningRepair,
    ArtifactRepair,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct RepairBudget {
    pub max_attempts: u32,
    pub attempts_consumed: u32,
}

impl RepairBudget {
    pub const fn exhausted(&self) -> bool {
        self.attempts_consumed >= self.max_attempts
    }

    pub const fn remaining(&self) -> u32 {
        self.max_attempts.saturating_sub(self.attempts_consumed)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct RepairBudgets {
    pub mutation_application: RepairBudget,
    pub validation: RepairBudget,
    pub review: RepairBudget,
    pub planning: RepairBudget,
    pub artifact: RepairBudget,
}

impl RepairBudgets {
    pub const fn for_kind(&self, kind: RepairIntentKind) -> &RepairBudget {
        match kind {
            RepairIntentKind::MutationApplicationFallback => &self.mutation_application,
            RepairIntentKind::ValidationRepair => &self.validation,
            RepairIntentKind::DiffReviewRepair => &self.review,
            RepairIntentKind::PlanningRepair => &self.planning,
            RepairIntentKind::ArtifactRepair => &self.artifact,
        }
    }

    pub const fn for_kind_mut(&mut self, kind: RepairIntentKind) -> &mut RepairBudget {
        match kind {
            RepairIntentKind::MutationApplicationFallback => &mut self.mutation_application,
            RepairIntentKind::ValidationRepair => &mut self.validation,
            RepairIntentKind::DiffReviewRepair => &mut self.review,
            RepairIntentKind::PlanningRepair => &mut self.planning,
            RepairIntentKind::ArtifactRepair => &mut self.artifact,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct AlreadyAppliedTransition {
    pub node_id: ExecutionNodeId,
    pub operation: TargetOperation,
    pub target_path: RepositoryPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_result_hash: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_result_hash: Option<ContentHash>,
    pub repository_fingerprint: RepositoryFingerprint,
    pub completed_at: String,
}

impl AlreadyAppliedTransition {
    pub fn semantic_id(&self, execution_id: &str, attempt: u32) -> String {
        stable_hash(&format!(
            "{}\0{}\0{}\0{}\0{}\0{}\0{}\0already_applied",
            execution_id,
            self.node_id,
            attempt,
            self.operation.as_str(),
            self.target_path,
            self.expected_result_hash.as_deref().unwrap_or_default(),
            self.repository_fingerprint,
        ))
    }

    pub fn evidence(&self, semantic_id: String, attempt: u32) -> OperationEvidence {
        OperationEvidence {
            semantic_id,
            outcome: RepositoryOperationOutcome::AlreadyApplied,
            operation: self.operation.clone(),
            target_path: self.target_path.clone(),
            expected_result_hash: self.expected_result_hash.clone(),
            observed_result_hash: self.observed_result_hash.clone(),
            repository_fingerprint: self.repository_fingerprint.clone(),
            attempt,
            completed_at: self.completed_at.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct OperationEvidence {
    pub semantic_id: String,
    pub outcome: RepositoryOperationOutcome,
    pub operation: TargetOperation,
    pub target_path: RepositoryPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_result_hash: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_result_hash: Option<ContentHash>,
    pub repository_fingerprint: RepositoryFingerprint,
    pub attempt: u32,
    pub completed_at: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExpectedRepositoryTargetState {
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<ContentHash>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ObservedTargetState {
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<ContentHash>,
}

/// Proof for one repository operation only. It intentionally says nothing
/// about automated validation gates for the repository as a whole.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct RepositoryOperationVerification {
    pub node_id: ExecutionNodeId,
    pub target_path: RepositoryPath,
    pub operation: TargetOperation,
    pub expected_state: ExpectedRepositoryTargetState,
    pub observed_state: ObservedTargetState,
    pub repository_fingerprint_before: RepositoryFingerprint,
    pub repository_fingerprint_after: RepositoryFingerprint,
    pub verified_at: String,
}

impl RepositoryOperationVerification {
    pub fn from_completed_node(node: &ExecutionNode) -> Option<Self> {
        node.target.as_ref()?;
        let evidence = node.operation_evidence.last()?;
        let attempt = node.attempts.last()?;
        let expected_exists = evidence.operation != TargetOperation::DeleteExisting;
        Some(Self {
            node_id: node.id.clone(),
            target_path: evidence.target_path.clone(),
            operation: evidence.operation.clone(),
            expected_state: ExpectedRepositoryTargetState {
                exists: expected_exists,
                content_hash: evidence.expected_result_hash.clone(),
            },
            observed_state: ObservedTargetState {
                exists: expected_exists,
                content_hash: evidence.observed_result_hash.clone(),
            },
            repository_fingerprint_before: attempt.repository_fingerprint_before.clone().into(),
            repository_fingerprint_after: evidence.repository_fingerprint.clone(),
            verified_at: evidence.completed_at.clone(),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ImplementationBarrierProof {
    pub repository_fingerprint: RepositoryFingerprint,
    #[serde(default)]
    pub satisfied_at: String,
    #[serde(default)]
    pub implementation_revision: u64,
    #[serde(default)]
    pub required_nodes: Vec<ExecutionNodeId>,
    #[serde(default)]
    pub completed_nodes: Vec<ExecutionNodeId>,
    #[serde(default)]
    pub unresolved_nodes: Vec<ExecutionNodeId>,
    pub satisfied: bool,
}

pub const fn can_transition_implementation_status(
    from: ExecutionNodeStatus,
    to: ExecutionNodeStatus,
) -> bool {
    match from {
        ExecutionNodeStatus::Completed => matches!(to, ExecutionNodeStatus::Completed),
        ExecutionNodeStatus::Applied => matches!(to, ExecutionNodeStatus::Applied),
        ExecutionNodeStatus::Skipped => matches!(to, ExecutionNodeStatus::Skipped),
        _ => true,
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairNodeStatus {
    Pending,
    Ready,
    #[default]
    Running,
    Completed,
    Failed,
    Blocked,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationRepairNodeMetadata {
    pub repair_node_id: RepairNodeId,
    pub target: RepositoryTargetRef,
    pub originating_implementation_node_id: ExecutionNodeId,
    pub validation_session_id: ValidationRepairSessionId,
    pub failure_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_repair_eligibility: Option<TestRepairEligibilityDecision>,
    pub status: RepairNodeStatus,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ActivateValidationRepair {
    pub repair_session_id: ValidationRepairSessionId,
    pub repair_node_id: RepairNodeId,
    pub target_id: TargetId,
    pub originating_implementation_node_id: ExecutionNodeId,
    pub failure_revision: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationRepairOperationEvidence {
    pub repair_node_id: RepairNodeId,
    pub validation_session_id: ValidationRepairSessionId,
    pub failure_revision: u64,
    pub target_id: TargetId,
    pub repository_fingerprint_before: RepositoryFingerprint,
    pub repository_fingerprint_after: RepositoryFingerprint,
    pub verification_evidence_id: EvidenceId,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "producer", content = "node_id", rename_all = "snake_case")]
pub enum TargetRevisionProducer {
    InitialImplementation(ExecutionNodeId),
    MutationFallback(ExecutionNodeId),
    ValidationRepair(RepairNodeId),
    DiffReviewRepair(RepairNodeId),
}

impl Default for TargetRevisionProducer {
    fn default() -> Self {
        Self::InitialImplementation(ExecutionNodeId::default())
    }
}

impl TargetRevisionProducer {
    pub fn for_mutation_owner(node: &ExecutionNode, intent: SatisfiedIntent) -> Self {
        match (node.kind, intent) {
            (ExecutionNodeKind::ValidationRepair, _) => Self::ValidationRepair(node.id.clone()),
            (ExecutionNodeKind::DiffReviewRepair, _) => {
                Self::DiffReviewRepair(node.id.clone())
            }
            (_, SatisfiedIntent::MutationFallback) => Self::MutationFallback(node.id.clone()),
            _ => Self::InitialImplementation(node.id.clone()),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetRevision {
    pub target_id: TargetId,
    pub revision: u64,
    pub producer: TargetRevisionProducer,
    pub repository_fingerprint: RepositoryFingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<ContentHash>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeTransition {
    Completed(OperationEvidence),
    NoOp(OperationEvidence),
    StateConflict,
    InvalidTransition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionError {
    pub code: &'static str,
    pub message: String,
}

pub type OperationReductionError = TransitionError;

/// The sole reducer from a verified repository result into authoritative graph
/// state. It uses clone-then-commit so attempt finalization, node completion,
/// evidence attachment, and dependency refresh are atomic.
pub fn reduce_repository_operation(
    graph: &mut ExecutionGraph,
    node_id: ExecutionNodeId,
    intent: OperationIntent,
    result: RepositoryOperationResult,
) -> Result<GraphMutationResult, OperationReductionError> {
    let (
        outcome,
        successful_evidence,
        observed_result_hash,
        semantic_id,
        attempt,
        completed_at,
    ) = match result {
        RepositoryOperationResult::Verified {
            outcome,
            evidence,
            observed_result_hash,
            semantic_id,
            attempt,
            completed_at,
        } => (
            outcome,
            evidence,
            observed_result_hash,
            semantic_id,
            attempt,
            completed_at,
        ),
        RepositoryOperationResult::Rejected => {
            return Err(TransitionError::new(
                "repository_operation_rejected",
                format!("repository operation for node `{node_id}` was rejected before verification"),
            ));
        }
        RepositoryOperationResult::Conflict => {
            return Err(TransitionError::new(
                "repository_operation_conflict",
                format!("repository operation for node `{node_id}` conflicts with repository state"),
            ));
        }
    };
    if !matches!(
        outcome,
        RepositoryOperationOutcome::Applied | RepositoryOperationOutcome::AlreadyApplied
    ) {
        return Err(TransitionError::new(
            "repository_operation_not_successful",
            format!("verified repository operation for node `{node_id}` has non-success outcome {outcome:?}"),
        ));
    }
    if semantic_id.trim().is_empty() || completed_at.trim().is_empty() {
        return Err(TransitionError::new(
            "repository_operation_evidence_incomplete",
            format!("verified repository operation for node `{node_id}` lacks durable completion evidence"),
        ));
    }

    let repository_fingerprint = match &successful_evidence {
        SuccessfulOperationEvidence::Applied { after, .. } => after.clone(),
        SuccessfulOperationEvidence::AlreadyApplied { observed } => observed.clone(),
    };
    let evidence = OperationEvidence {
        semantic_id: semantic_id.clone(),
        outcome,
        operation: intent.operation.clone(),
        target_path: intent.target_path.clone(),
        expected_result_hash: intent.expected_result_hash.clone(),
        observed_result_hash,
        repository_fingerprint,
        attempt,
        completed_at: completed_at.clone(),
    };
    let node = graph.node(&node_id).ok_or_else(|| {
        TransitionError::new(
            "repository_operation_node_unknown",
            format!("unknown execution node `{node_id}`"),
        )
    })?;
    if !node.has_capability(NodeCapability::RepositoryMutation)
        || node.target.as_ref().is_none_or(|target| {
            target.effective_operation() != intent.operation
                || target.effective_operation().destination_path(&target.path)
                    != intent.target_path
        })
    {
        return Err(TransitionError::new(
            "repository_operation_intent_conflict",
            format!("repository operation intent does not match repository-mutation producer `{node_id}`"),
        ));
    }
    if node.kind.is_mutation()
        && node.status == ExecutionNodeStatus::Completed
        && intent.satisfied_intent == SatisfiedIntent::ValidationRepair
    {
        return Err(TransitionError::new(
            "completed_implementation_node_reopened",
            format!(
                "validation repair cannot append operation evidence or attempts to completed implementation node `{node_id}`"
            ),
        ));
    }
    match reduce_operation_outcome(node, outcome, evidence.clone())? {
        NodeTransition::NoOp(_) => Ok(GraphMutationResult::NoChange {
            current_revision: graph.revision,
        }),
        NodeTransition::StateConflict => Err(TransitionError::new(
            "repository_operation_evidence_conflict",
            format!("completed node `{node_id}` has conflicting operation evidence"),
        )),
        NodeTransition::InvalidTransition => Err(TransitionError::new(
            "repository_operation_before_node_activation",
            format!("node `{node_id}` cannot complete before activation"),
        )),
        NodeTransition::Completed(_) => {
            let mut next = graph.clone();
            let previous_revision = next.revision;
            let node = next.node_mut(&node_id).ok_or_else(|| {
                TransitionError::new(
                    "repository_operation_node_unknown",
                    format!("unknown execution node `{node_id}`"),
                )
            })?;
            let active_attempt = node.attempts.last_mut().ok_or_else(|| {
                TransitionError::new(
                    "repository_operation_attempt_missing",
                    format!("active node `{node_id}` has no persisted attempt"),
                )
            })?;
            if active_attempt.attempt != attempt {
                return Err(TransitionError::new(
                    "repository_operation_attempt_conflict",
                    format!(
                        "active node `{node_id}` attempt {} does not match operation attempt {attempt}",
                        active_attempt.attempt
                    ),
                ));
            }
            active_attempt.completed_at = Some(completed_at);
            active_attempt.repository_fingerprint_after =
                Some(evidence.repository_fingerprint.to_string());
            active_attempt.outcome = Some(ExecutionNodeStatus::Completed);
            node.status = ExecutionNodeStatus::Completed;
            node.repository_mutation_lifecycle = Some(RepositoryMutationLifecycle::Verified);
            if !node.evidence_ids.contains(&semantic_id) {
                node.evidence_ids.push(semantic_id);
            }
            node.operation_evidence.push(evidence);
            next.dependency_satisfaction_overrides.remove(&node_id);
            next.refresh_readiness_without_revision();
            next.revision = previous_revision.saturating_add(1);
            next.validate_invariants().map_err(|error| {
                TransitionError::new("repository_operation_invariant_failed", error.to_string())
            })?;
            let new_revision = next.revision;
            *graph = next;
            Ok(GraphMutationResult::Changed { new_revision })
        }
    }
}

impl TransitionError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl fmt::Display for TransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for TransitionError {}

pub fn reduce_operation_outcome(
    node: &ExecutionNode,
    outcome: RepositoryOperationOutcome,
    evidence: OperationEvidence,
) -> Result<NodeTransition, TransitionError> {
    let successful = matches!(outcome, RepositoryOperationOutcome::Applied | RepositoryOperationOutcome::AlreadyApplied);
    match node.status {
        ExecutionNodeStatus::Running if successful => Ok(NodeTransition::Completed(evidence)),
        ExecutionNodeStatus::Completed if successful => {
            if node.operation_evidence.iter().any(|existing| existing == &evidence) {
                Ok(NodeTransition::NoOp(evidence))
            } else {
                Ok(NodeTransition::StateConflict)
            }
        }
        ExecutionNodeStatus::Pending | ExecutionNodeStatus::Ready => Ok(NodeTransition::InvalidTransition),
        _ => Err(TransitionError::new(
            "operation_outcome_invalid",
            format!("operation outcome {outcome:?} cannot reduce node `{}` in status {:?}", node.id, node.status),
        )),
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct OrchestrationCycleResult {
    pub graph_changed: bool,
    pub repository_changed: bool,
    pub validation_changed: bool,
    #[serde(default)]
    pub phase_changed: bool,
    pub external_wait_scheduled: bool,
    pub terminal_selected: bool,
}

impl OrchestrationCycleResult {
    pub const fn made_semantic_progress(&self) -> bool {
        self.graph_changed
            || self.repository_changed
            || self.validation_changed
            || self.phase_changed
            || self.external_wait_scheduled
            || self.terminal_selected
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationGuardrailOutcome {
    ReconcileSuccessfulMutation,
    ReconcileNodeState,
    AdvanceToNextNode,
    ReviewIncompleteDiff,
    FinishBlocked,
    #[default]
    FailOrchestrator,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationGuardrailAction {
    ReconcileSuccessfulMutation,
    AttemptBoundedRecovery,
    FinishBlocked,
    #[default]
    FailOrchestrator,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CycleCause {
    SuccessfulMutationNotReduced,
    AlreadyAppliedNotReduced,
    StaleActivePointer,
    DecisionSelectorNoProgress,
    PersistenceMismatch,
    #[default]
    #[serde(
        alias = "repository_state_unchanged",
        alias = "repair_budget_exhausted",
        alias = "orchestration_state_diverged"
    )]
    Unknown,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct SemanticCycleObservation {
    pub semantic_state_hash: String,
    pub semantic_decision_hash: String,
    pub outcome: String,
    pub repeated_count: u8,
    pub observed_at: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct WorkerLiveness {
    pub lease_renewed_at: Option<String>,
    pub last_semantic_progress_at: Option<String>,
}

pub const MAX_IDENTICAL_DETERMINISTIC_CYCLES: u8 = 2;

pub fn observe_semantic_cycle(
    history: &mut Vec<SemanticCycleObservation>,
    state_hash: &str,
    decision_hash: &str,
    outcome: &str,
    observed_at: &str,
) -> u8 {
    const MAX_HISTORY: usize = 8;
    let repeated_count = history.last().map_or(1, |prior| {
        if prior.semantic_state_hash == state_hash && prior.semantic_decision_hash == decision_hash && prior.outcome == outcome {
            prior.repeated_count.saturating_add(1)
        } else { 1 }
    });
    history.push(SemanticCycleObservation {
        semantic_state_hash: state_hash.to_owned(),
        semantic_decision_hash: decision_hash.to_owned(),
        outcome: outcome.to_owned(),
        repeated_count,
        observed_at: observed_at.to_owned(),
    });
    if history.len() > MAX_HISTORY { history.remove(0); }
    repeated_count
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct CreateTargetSpecification {
    pub path: RepositoryPath,
    pub role: String,
    pub intent: String,
    #[serde(default)]
    pub acceptance_criteria_ids: Vec<String>,
    #[serde(default)]
    pub related_evidence_ids: Vec<EvidenceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_artifact_kind: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct CreatedTargetEvidence {
    pub path: RepositoryPath,
    pub content_hash: ContentHash,
    pub repository_fingerprint_before: RepositoryFingerprint,
    pub repository_fingerprint_after: RepositoryFingerprint,
    pub creation_tool: String,
    #[serde(default)]
    pub validation_gate_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetExecutionContext {
    pub node_id: ExecutionNodeId,
    pub change_id: String,
    pub target: PlannedTarget,
    pub intent: String,
    #[serde(default)]
    pub acceptance_criteria_ids: Vec<String>,
    #[serde(default)]
    pub dependency_evidence: Vec<EvidenceSummary>,
    pub current_file_content: Option<String>,
    #[serde(default)]
    pub target_content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_state_probe: Option<TargetStateProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspection_outcome: Option<TargetInspectionOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_specification: Option<CreateTargetSpecification>,
    #[serde(default)]
    pub repository_fingerprint: String,
    #[serde(default)]
    pub accepted_intent_hash: String,
    #[serde(default)]
    pub nearby_context: Vec<FileExcerpt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_repair: Option<ValidationRepairContext>,
    #[serde(default)]
    pub allowed_tools: Vec<ToolKind>,
    pub remaining_node_budget: NodeBudgetRemaining,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationRepairContext {
    pub repair_intent: ValidationRepairIntent,
    pub focused_validation_command: String,
    #[serde(default)]
    pub assertion_failures: Vec<ValidationAssertionFailure>,
    #[serde(default)]
    pub implicated_targets: Vec<FileExcerpt>,
    pub selected_target: String,
    #[serde(default)]
    pub target_ref: RepositoryTargetRef,
    #[serde(default)]
    pub originating_implementation_node_id: ExecutionNodeId,
    #[serde(default)]
    pub repair_node_id: RepairNodeId,
    #[serde(default)]
    pub failure_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_repair_eligibility: Option<TestRepairEligibilityDecision>,
    pub repository_fingerprint: String,
    pub accepted_implementation_intent: String,
    #[serde(default)]
    pub existing_diff_paths: Vec<String>,
    #[serde(default)]
    pub correction_contracts: Vec<AssertionRepairContract>,
    #[serde(default)]
    pub attempted_targets: Vec<RepositoryPath>,
    #[serde(default)]
    pub remaining_eligible_targets: Vec<RepositoryPath>,
}

pub type IntentId = String;
pub type ChangeId = String;
pub type ValidationId = String;
pub type ValidationAssertionId = String;
pub type RepairSessionId = String;
pub type ModelCallId = String;
pub type MutationToolPolicy = MutationFallbackPolicy;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TestRepairEligibilityDecision {
    pub target_path: RepositoryPath,
    pub eligible: bool,
    pub reason_code: String,
    #[serde(default)]
    pub supporting_specification_evidence_ids: Vec<EvidenceId>,
    pub failure_revision: u64,
    pub repair_intent_id: IntentId,
    pub repair_session_id: RepairSessionId,
}

impl TestRepairEligibilityDecision {
    pub fn authorizes(
        &self,
        target_path: &str,
        failure_revision: u64,
        repair_intent_id: &str,
        repair_session_id: &str,
    ) -> bool {
        self.eligible
            && self.target_path == target_path
            && self.failure_revision == failure_revision
            && self.repair_intent_id == repair_intent_id
            && self.repair_session_id == repair_session_id
            && !self.supporting_specification_evidence_ids.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRepairSelectionStatus {
    #[default]
    Unassessed,
    CandidateSelected,
    NoValidRepair,
}

pub fn validation_repair_node_id(
    failure_id: &FailureId,
    failure_revision: u64,
    target: &PlannedTarget,
) -> RepairNodeId {
    ExecutionNodeId::new(format!(
        "validation-repair:{}:{}:{}",
        failure_id,
        failure_revision,
        target.mutation_target_id()
    ))
}

/// Deterministic work owned by a validation gate. None of these counters may
/// be used to admit a model-backed repository mutation.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationGateBudget {
    pub command_runs: u32,
    pub parsing_calls: u32,
    pub diagnosis_calls: u32,
}

/// The independently admitted envelope for one bounded validation-repair
/// session. Verification and command reruns are deterministic and therefore
/// do not consume `max_model_calls`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationRepairBudget {
    pub max_model_calls: u32,
    pub max_target_attempts: u32,
    pub max_repository_writes: u32,
    pub max_context_rebuilds: u32,
    pub max_cost_micros: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationRepairBudgetInputs {
    pub failed_assertion_count: u32,
    pub implicated_target_count: u32,
    pub originating_gate_required: bool,
    pub implicated_target_bytes: u64,
}

impl ValidationRepairBudget {
    pub const MINIMUM_MODEL_CALLS: u32 = 2;

    pub fn validate(&self, multi_target: bool) -> Result<(), GraphInvariantError> {
        let required_targets = if multi_target {
            self.max_target_attempts
        } else {
            1
        };
        let minimum_calls = Self::MINIMUM_MODEL_CALLS.max(1_u32.saturating_add(required_targets));
        if self.max_model_calls < minimum_calls
            || self.max_target_attempts < required_targets
            || self.max_repository_writes < required_targets
            || self.max_context_rebuilds == 0
            || self.max_cost_micros == 0
        {
            return Err(GraphInvariantError::new(
                "validation repair budget cannot execute diagnosis, mutation, and deterministic verification",
            ));
        }
        Ok(())
    }

    pub fn as_node_budget(&self) -> NodeBudget {
        NodeBudget {
            max_model_calls: self.max_model_calls,
            max_cost_micros: self.max_cost_micros,
            max_duration: Duration::from_secs(10 * 60),
            max_mutation_fallback_attempts: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRepairSessionStatus {
    #[default]
    Active,
    ReadyForRerun,
    ValidationPassed,
    Stopped,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRepairStopReason {
    NoEligibleTargets,
    RepairBudgetExhausted,
    MissionBudgetExhausted,
    AdmissionPolicyMisconfigured,
    NoSafeRepair,
    ValidationPassed,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationFailureRevision {
    pub validation_id: ValidationId,
    pub revision: u64,
    pub repository_fingerprint: RepositoryFingerprint,
    #[serde(default)]
    pub assertion_ids: Vec<ValidationAssertionId>,
    /// RFC 3339 when supplied by an adapter; replayed legacy events use a
    /// stable sequence-derived value instead of consulting wall-clock time.
    pub created_at: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationRepairSession {
    pub session_id: RepairSessionId,
    pub failed_validation_id: ValidationId,
    pub originating_gate_id: ExecutionNodeId,
    pub budget: ValidationRepairBudget,
    pub status: ValidationRepairSessionStatus,
    #[serde(default)]
    pub attempted_targets: Vec<ValidationRepairAttempt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempt_reservations: Vec<RepairAttemptReservation>,
    #[serde(default)]
    pub repair_nodes: Vec<RepairNodeId>,
    pub current_assertion_set_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<ValidationRepairStopReason>,
    #[serde(default)]
    pub reallocated_model_calls: u32,
    #[serde(default)]
    pub reallocated_cost_micros: u64,
    #[serde(default)]
    pub repository_writes_consumed: u32,
    #[serde(default)]
    pub context_rebuilds_consumed: u32,
    #[serde(default)]
    pub budget_inputs: ValidationRepairBudgetInputs,
}

pub type RepairAttemptId = String;

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairAttemptReservationState {
    #[default]
    Reserved,
    Consumed,
    Released,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct RepairAttemptReservation {
    pub repair_session_id: ValidationRepairSessionId,
    pub attempt_id: RepairAttemptId,
    pub target_id: TargetId,
    pub state: RepairAttemptReservationState,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairTargetState {
    #[default]
    Candidate,
    Selected,
    AttemptReserved,
    MutationExecuted,
    Exhausted,
    Resolved,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExpectedTargetState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<ContentHash>,
    #[serde(default)]
    pub required_assertion_ids: Vec<ValidationAssertionId>,
    #[serde(default)]
    pub required_observable_change: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ImplementationIntent {
    pub intent_id: IntentId,
    pub change_id: ChangeId,
    pub target: RepositoryPath,
    pub expected_state: ExpectedTargetState,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationRepairIntent {
    pub repair_intent_id: IntentId,
    pub failed_validation_id: ValidationId,
    pub target: RepositoryPath,
    pub diagnosis: ValidationRepairDiagnosis,
    pub expected_correction: ExpectedTargetState,
    #[serde(default)]
    pub evidence_ids: Vec<EvidenceId>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct SourceLocation {
    pub path: RepositoryPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct AssertionRepairContract {
    pub assertion_id: ValidationAssertionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_location: Option<SourceLocation>,
    #[serde(default)]
    pub implicated_paths: Vec<RepositoryPath>,
    pub required_observable_change: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct AlreadyAppliedRepairEvidence {
    pub repair_intent_id: IntentId,
    pub target_path: RepositoryPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_state_hash: Option<ContentHash>,
    pub current_state_hash: ContentHash,
    #[serde(default)]
    pub satisfied_assertions: Vec<ValidationAssertionId>,
    #[serde(default)]
    pub supporting_evidence_ids: Vec<EvidenceId>,
}

impl AlreadyAppliedRepairEvidence {
    pub fn proves(&self, intent: &ValidationRepairIntent) -> bool {
        self.repair_intent_id == intent.repair_intent_id
            && self.target_path == intent.target
            && !self.current_state_hash.is_empty()
            && !intent.expected_correction.required_assertion_ids.is_empty()
            && !self.satisfied_assertions.is_empty()
            && !self.supporting_evidence_ids.is_empty()
            && intent
                .expected_correction
                .required_assertion_ids
                .iter()
                .all(|assertion| self.satisfied_assertions.contains(assertion))
            && self
                .supporting_evidence_ids
                .iter()
                .all(|evidence| intent.evidence_ids.contains(evidence))
            && intent
                .expected_correction
                .content_hash
                .as_ref()
                .is_none_or(|expected| expected == &self.current_state_hash)
            && self.expected_state_hash.as_ref().is_none_or(|expected| {
                expected == &self.current_state_hash
            })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum MutationResult {
    Applied {
        node_id: ExecutionNodeId,
        target: PlannedTarget,
        repository_fingerprint: String,
        evidence_id: String,
    },
    AlreadyApplied {
        node_id: ExecutionNodeId,
        target: PlannedTarget,
        repository_fingerprint: String,
    },
    RecoverableFailure {
        failure: FailureRecord,
    },
    BlockingFailure {
        failure: FailureRecord,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRepairDiagnosis {
    SourceDefect,
    TestExpectationDefect,
    Both,
    #[default]
    Inconclusive,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRepairMutationOutcome {
    MutationApplied,
    AlreadySatisfiesRepairIntent,
    NoChangeAgainstCurrentTarget,
    MutationRejected,
    AdmissionRejected,
    #[default]
    NoValidRepair,
    WrongRepairTarget,
}

impl ValidationRepairMutationOutcome {
    pub const fn consumes_repository_write_allowance(self) -> bool {
        matches!(
            self,
            Self::MutationApplied
                | Self::NoChangeAgainstCurrentTarget
                | Self::MutationRejected
                | Self::WrongRepairTarget
        )
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationRepairAttempt {
    #[serde(default)]
    pub attempt_number: u32,
    pub repair_intent_id: IntentId,
    pub target_path: RepositoryPath,
    #[serde(default)]
    pub failure_revision: u64,
    pub diagnosis: ValidationRepairDiagnosis,
    pub requested_tool_policy: MutationToolPolicy,
    pub outcome: ValidationRepairMutationOutcome,
    pub repository_fingerprint_before: RepositoryFingerprint,
    pub repository_fingerprint_after: RepositoryFingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_call_id: Option<ModelCallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_rejection_reason: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct UnresolvedValidationRepair {
    pub validation_id: ValidationId,
    pub repair_intent_id: IntentId,
    pub selected_target: RepositoryPath,
    pub diagnosis: ValidationRepairDiagnosis,
    pub outcome: ValidationRepairMutationOutcome,
    pub reason: String,
    #[serde(default)]
    pub attempted_targets: Vec<RepositoryPath>,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRepairAction {
    BuildRepairEvidence,
    DiagnoseFailure,
    SelectRepairTarget,
    MutateRepairTarget,
    VerifyRepair,
    RerunFailedGate,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationAssertionFailure {
    pub test_file: String,
    #[serde(default)]
    pub suite_path: Vec<String>,
    pub test_name: String,
    pub source_location: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_column: Option<u32>,
    pub assertion_kind: String,
    pub expected: String,
    pub received: String,
    #[serde(default)]
    pub implicated_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<ValidationRepairDiagnosis>,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub context: String,
    #[serde(default)]
    pub proposed_repair: String,
    #[serde(default)]
    pub expected_validation_effect: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RepairResult {
    MutationProduced {
        selected_target: String,
        #[serde(default)]
        repair_intent_id: IntentId,
    },
    AlreadySatisfiesRepairIntent {
        evidence: AlreadyAppliedRepairEvidence,
    },
    NoMutation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnosis: Option<ValidationRepairDiagnosis>,
        reason: String,
        #[serde(default)]
        outcome: ValidationRepairMutationOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unresolved: Option<UnresolvedValidationRepair>,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationStatus {
    #[default]
    Planned,
    Running,
    Applied,
    FailedRecoverable,
    FailedBlocking,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    #[default]
    Pending,
    Passed,
    FailedCode,
    FailedInfrastructure,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetState {
    pub mutation_status: MutationStatus,
    pub validation_status: ValidationStatus,
}

pub type ImplementationStatus = MutationStatus;

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStatus {
    #[default]
    NotRequired,
    Pending,
    Running,
    CandidateApplied,
    AlreadySatisfied,
    Unresolved,
    Exhausted,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetExecutionState {
    pub implementation_status: ImplementationStatus,
    pub repair_status: RepairStatus,
    pub validation_status: ValidationStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedReason {
    ValidationRepairUnresolvedWithoutDiff,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessFailureReason {
    HostedLifecycleContractFailure,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum RepairTerminalDecision {
    ContinueRepair,
    RerunValidation,
    ReviewIncompleteDiff { reason: IncompleteReason },
    FinishBlockedWithoutDiff { reason: BlockedReason },
    FailProcess { reason: ProcessFailureReason },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    ModelArtifactRecoverable,
    #[default]
    ToolRecoverable,
    MutationConflict,
    PlanRepositoryConflict,
    TargetBlocked,
    ValidationFailure,
    InfrastructureFailure,
    OrchestrationInvariantViolation,
    UserCancellation,
}

impl FailureCategory {
    pub const fn creates_repair_work(self) -> bool {
        matches!(
            self,
            Self::ModelArtifactRecoverable
                | Self::ToolRecoverable
                | Self::MutationConflict
                | Self::PlanRepositoryConflict
                | Self::TargetBlocked
                | Self::ValidationFailure
        )
    }

    pub const fn is_infrastructure(self) -> bool {
        matches!(self, Self::InfrastructureFailure)
    }

    /// Only failures caused by a repository mutation/tool conflict may be
    /// inferred obsolete from a later successful write. Validation,
    /// infrastructure, invariant, cancellation, and semantic blocker failures
    /// require their own explicit recovery event.
    pub const fn is_supersedable_by_applied_target(self) -> bool {
        matches!(
            self,
            Self::ToolRecoverable | Self::MutationConflict | Self::PlanRepositoryConflict
        )
    }

    const fn node_status(self) -> ExecutionNodeStatus {
        match self {
            Self::ModelArtifactRecoverable
            | Self::ToolRecoverable
            | Self::MutationConflict
            | Self::PlanRepositoryConflict
            | Self::ValidationFailure => ExecutionNodeStatus::FailedRecoverable,
            Self::TargetBlocked
            | Self::InfrastructureFailure
            | Self::OrchestrationInvariantViolation
            | Self::UserCancellation => ExecutionNodeStatus::FailedBlocking,
        }
    }

    const fn is_valid_for_node_kind(self, kind: ExecutionNodeKind) -> bool {
        match self {
            Self::MutationConflict | Self::PlanRepositoryConflict | Self::TargetBlocked => {
                kind.has_capability(NodeCapability::RepositoryMutation)
            }
            Self::ValidationFailure => kind.is_validation(),
            Self::ModelArtifactRecoverable => kind.requires_model(),
            Self::ToolRecoverable => matches!(
                kind,
                ExecutionNodeKind::Discovery
                    | ExecutionNodeKind::Planning
                    | ExecutionNodeKind::SourceMutation
                    | ExecutionNodeKind::TestMutation
                    | ExecutionNodeKind::ValidationRepair
                    | ExecutionNodeKind::DiffReviewRepair
                    | ExecutionNodeKind::ValidationRepairSession
                    | ExecutionNodeKind::ValidationFocused
                    | ExecutionNodeKind::ValidationSuite
                    | ExecutionNodeKind::ValidationBuild
                    | ExecutionNodeKind::ValidationLint
            ),
            Self::InfrastructureFailure
            | Self::OrchestrationInvariantViolation
            | Self::UserCancellation => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStatus {
    #[default]
    Active,
    Recovered,
    Superseded,
}

/// A repository mutation failure classified before orchestration chooses the
/// next bounded action. This value is persisted as data; callers must never
/// recover it by parsing a human-readable diagnostic.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationApplicationFailure {
    InvalidPatchTarget,
    InvalidPatchSyntax,
    PatchContextMismatch,
    PatchWouldModifyUnexpectedPath,
    ReplacementContentInvalid,
    RepositoryChangedSinceContext,
    MutationProducedNoChange,
    CreateTargetAlreadyExists,
    DeleteTargetMissing,
    RenameDestinationConflict,
}

impl MutationApplicationFailure {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidPatchTarget => "invalid_patch_target",
            Self::InvalidPatchSyntax => "invalid_patch_syntax",
            Self::PatchContextMismatch => "patch_context_mismatch",
            Self::PatchWouldModifyUnexpectedPath => "patch_would_modify_unexpected_path",
            Self::ReplacementContentInvalid => "replacement_content_invalid",
            Self::RepositoryChangedSinceContext => "repository_changed_since_context",
            Self::MutationProducedNoChange => "mutation_produced_no_change",
            Self::CreateTargetAlreadyExists => "create_target_already_exists",
            Self::DeleteTargetMissing => "delete_target_missing",
            Self::RenameDestinationConflict => "rename_destination_conflict",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "invalid_patch_target" => Self::InvalidPatchTarget,
            "invalid_patch_syntax" => Self::InvalidPatchSyntax,
            "patch_context_mismatch" => Self::PatchContextMismatch,
            "patch_would_modify_unexpected_path" => Self::PatchWouldModifyUnexpectedPath,
            "replacement_content_invalid" => Self::ReplacementContentInvalid,
            "repository_changed_since_context" => Self::RepositoryChangedSinceContext,
            "mutation_produced_no_change" => Self::MutationProducedNoChange,
            "create_target_already_exists" => Self::CreateTargetAlreadyExists,
            "delete_target_missing" => Self::DeleteTargetMissing,
            "rename_destination_conflict" | "destination_already_exists" => {
                Self::RenameDestinationConflict
            }
            _ => return None,
        })
    }

    pub const fn uses_replacement_threshold(self) -> bool {
        matches!(
            self,
            Self::InvalidPatchTarget
                | Self::InvalidPatchSyntax
                | Self::PatchContextMismatch
                | Self::PatchWouldModifyUnexpectedPath
                | Self::ReplacementContentInvalid
                | Self::MutationProducedNoChange
        )
    }
}

/// Executable policy for the next target-repair request.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationFallbackPolicy {
    ForceReplaceFile,
    ForceCreateFile,
    ForceDeleteFile,
    ForceRename,
    RebuildTargetContext,
    RetryPatchWithNormalizedPayload,
    #[default]
    NoSafeFallback,
}

impl MutationFallbackPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ForceReplaceFile => "force_replace_file",
            Self::ForceCreateFile => "force_create_file",
            Self::ForceDeleteFile => "force_delete_file",
            Self::ForceRename => "force_rename",
            Self::RebuildTargetContext => "rebuild_target_context",
            Self::RetryPatchWithNormalizedPayload => "retry_patch_with_normalized_payload",
            Self::NoSafeFallback => "no_safe_fallback",
        }
    }

    pub const fn permitted_tools(self) -> &'static [&'static str] {
        match self {
            Self::ForceReplaceFile => &["replace_file"],
            Self::ForceCreateFile => &["create_file"],
            Self::ForceDeleteFile => &["delete_file"],
            Self::ForceRename => &["rename_file"],
            Self::RetryPatchWithNormalizedPayload => &["apply_patch"],
            Self::RebuildTargetContext | Self::NoSafeFallback => &[],
        }
    }

    pub const fn forced_tool(self) -> Option<&'static str> {
        match self.permitted_tools() {
            [tool] => Some(tool),
            _ => None,
        }
    }

    pub const fn requires_provider_mutation(self) -> bool {
        self.forced_tool().is_some()
    }

    pub const fn compatible_with(self, operation: &TargetOperation) -> bool {
        matches!(
            (self, operation),
            (
                Self::ForceReplaceFile | Self::RetryPatchWithNormalizedPayload,
                TargetOperation::ModifyExisting
            ) | (Self::ForceCreateFile, TargetOperation::CreateNew)
                | (Self::ForceDeleteFile, TargetOperation::DeleteExisting)
                | (
                    Self::ForceRename,
                    TargetOperation::Rename { .. } | TargetOperation::Move { .. }
                )
                | (Self::RebuildTargetContext | Self::NoSafeFallback, _)
        )
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetAttemptAccounting {
    pub primary_mutation_calls: u32,
    pub mutation_repair_calls: u32,
    pub context_rebuilds: u32,
    pub repository_write_attempts: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct MutationStrategyFingerprint {
    pub operation: TargetOperation,
    pub tool: String,
    pub fallback_policy: MutationFallbackPolicy,
    pub payload_type: String,
    pub failure_category: MutationApplicationFailure,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct MutationDiagnostics {
    pub message: String,
    #[serde(default)]
    pub normalized_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_check: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RejectedMutation {
    pub tool: String,
    pub payload_hash: String,
    pub failure_category: MutationApplicationFailure,
    pub failure_diagnostics: MutationDiagnostics,
    pub repository_fingerprint: RepositoryFingerprint,
    pub applied: bool,
    #[serde(default)]
    pub status: FailureStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_repository_fingerprint: Option<RepositoryFingerprint>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct RepairRequestPreflight {
    pub policy_present: bool,
    pub policy_compatible_with_operation: bool,
    pub exact_target_bound: bool,
    pub required_content_present: bool,
    pub target_hash_present: bool,
    pub repository_fingerprint_present: bool,
    pub tool_surface_matches_policy: bool,
    pub forced_tool_choice_matches_policy: bool,
}

impl RepairRequestPreflight {
    pub const fn passed(&self) -> bool {
        self.policy_present
            && self.policy_compatible_with_operation
            && self.exact_target_bound
            && self.required_content_present
            && self.target_hash_present
            && self.repository_fingerprint_present
            && self.tool_surface_matches_policy
            && self.forced_tool_choice_matches_policy
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct MutationToolPolicyViolation {
    pub node_id: ExecutionNodeId,
    pub target_path: String,
    pub active_policy: MutationFallbackPolicy,
    pub expected_tools: Vec<String>,
    pub received_tool: String,
}

impl fmt::Display for MutationToolPolicyViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "mutation_tool_policy_violation: policy {:?} permits {:?}, received `{}` for `{}`",
            self.active_policy, self.expected_tools, self.received_tool, self.target_path
        )
    }
}

impl std::error::Error for MutationToolPolicyViolation {}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct FailureRecord {
    pub id: FailureId,
    pub node_id: ExecutionNodeId,
    pub target_path: Option<String>,
    pub category: FailureCategory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub status: FailureStatus,
    /// Compatibility flags are serialized explicitly while `status` remains
    /// canonical. Constructors and store methods keep all three in sync.
    #[serde(default)]
    pub recovered: bool,
    #[serde(default)]
    pub superseded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by_attempt: Option<u32>,
    pub attempt: u32,
    pub repository_fingerprint: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_command: Option<String>,
    #[serde(default)]
    pub assertion_failures: Vec<ValidationAssertionFailure>,
    #[serde(default)]
    pub test_repair_eligibility: Vec<TestRepairEligibilityDecision>,
    #[serde(default)]
    pub validation_repair_selection_status: ValidationRepairSelectionStatus,
    #[serde(default)]
    pub resolved_repository_fingerprint: Option<String>,
}

impl FailureRecord {
    pub fn new(
        id: impl Into<FailureId>,
        node_id: impl Into<ExecutionNodeId>,
        category: FailureCategory,
        attempt: u32,
        repository_fingerprint: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            node_id: node_id.into(),
            category,
            attempt,
            repository_fingerprint: repository_fingerprint.into(),
            message: message.into(),
            ..Self::default()
        }
    }

    pub fn is_unresolved(&self) -> bool {
        self.status == FailureStatus::Active && !self.recovered && !self.superseded
    }

    pub fn mark_recovered(&mut self, repository_fingerprint: impl Into<String>) {
        self.status = FailureStatus::Recovered;
        self.recovered = true;
        self.superseded = false;
        self.resolved_repository_fingerprint = Some(repository_fingerprint.into());
    }

    pub fn mark_superseded(&mut self, repository_fingerprint: impl Into<String>) {
        self.mark_superseded_by(repository_fingerprint, None);
    }

    pub fn mark_superseded_by(
        &mut self,
        repository_fingerprint: impl Into<String>,
        attempt: Option<u32>,
    ) {
        self.status = FailureStatus::Superseded;
        self.recovered = false;
        self.superseded = true;
        self.superseded_by_attempt = attempt;
        self.resolved_repository_fingerprint = Some(repository_fingerprint.into());
    }

    pub fn normalize_compatibility_flags(&mut self) {
        match self.status {
            FailureStatus::Active => {
                if self.superseded {
                    self.status = FailureStatus::Superseded;
                    self.recovered = false;
                } else if self.recovered {
                    self.status = FailureStatus::Recovered;
                }
            }
            FailureStatus::Recovered => {
                self.recovered = true;
                self.superseded = false;
            }
            FailureStatus::Superseded => {
                self.recovered = false;
                self.superseded = true;
            }
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct FailureStore {
    #[serde(default)]
    pub records: Vec<FailureRecord>,
}

impl FailureStore {
    pub fn record(&mut self, mut failure: FailureRecord) -> FailureId {
        failure.normalize_compatibility_flags();
        let id = failure.id.clone();
        if let Some(existing) = self.records.iter_mut().find(|record| record.id == id) {
            *existing = failure;
        } else {
            self.records.push(failure);
        }
        id
    }

    pub fn get(&self, id: &FailureId) -> Option<&FailureRecord> {
        self.records.iter().find(|failure| &failure.id == id)
    }

    pub fn get_mut(&mut self, id: &FailureId) -> Option<&mut FailureRecord> {
        self.records.iter_mut().find(|failure| &failure.id == id)
    }

    pub fn unresolved(&self) -> impl Iterator<Item = &FailureRecord> {
        self.records
            .iter()
            .filter(|failure| failure.is_unresolved())
    }

    pub fn unresolved_for_node(
        &self,
        node_id: &ExecutionNodeId,
    ) -> impl Iterator<Item = &FailureRecord> {
        self.unresolved()
            .filter(move |failure| &failure.node_id == node_id)
    }

    pub fn has_unresolved(&self) -> bool {
        self.unresolved().next().is_some()
    }

    pub fn has_unresolved_for_node(&self, node_id: &ExecutionNodeId) -> bool {
        self.unresolved_for_node(node_id).next().is_some()
    }

    pub fn mark_recovered(
        &mut self,
        id: &FailureId,
        repository_fingerprint: impl Into<String>,
    ) -> bool {
        let Some(failure) = self.get_mut(id) else {
            return false;
        };
        failure.mark_recovered(repository_fingerprint);
        true
    }

    pub fn mark_superseded(
        &mut self,
        id: &FailureId,
        repository_fingerprint: impl Into<String>,
    ) -> bool {
        let Some(failure) = self.get_mut(id) else {
            return false;
        };
        failure.mark_superseded(repository_fingerprint);
        true
    }

    pub fn mark_superseded_by(
        &mut self,
        id: &FailureId,
        repository_fingerprint: impl Into<String>,
        attempt: Option<u32>,
    ) -> bool {
        let Some(failure) = self.get_mut(id) else {
            return false;
        };
        failure.mark_superseded_by(repository_fingerprint, attempt);
        true
    }

    /// Supersedes every unresolved failure for the applied node or target. This
    /// covers duplicate requests and later successful mutations of the same path.
    pub fn supersede_for_applied_target(
        &mut self,
        node_id: &ExecutionNodeId,
        target_path: &str,
        repository_fingerprint: &str,
        superseded_by_attempt: Option<u32>,
    ) -> Vec<FailureId> {
        let mut superseded = Vec::new();
        for failure in self.records.iter_mut().filter(|failure| {
            failure.is_unresolved()
                && failure.category.is_supersedable_by_applied_target()
                && (&failure.node_id == node_id
                    || failure.target_path.as_deref() == Some(target_path))
        }) {
            failure.mark_superseded_by(
                repository_fingerprint.to_owned(),
                superseded_by_attempt,
            );
            superseded.push(failure.id.clone());
        }
        superseded
    }

    /// Reconciles failures against any authoritative predicate, such as final
    /// diff inspection proving that an intended target change is present.
    pub fn supersede_where<F>(
        &mut self,
        repository_fingerprint: &str,
        mut intended_change_is_present: F,
    ) -> Vec<FailureId>
    where
        F: FnMut(&FailureRecord) -> bool,
    {
        let mut superseded = Vec::new();
        for failure in self.records.iter_mut().filter(|failure| {
            failure.is_unresolved() && failure.category.is_supersedable_by_applied_target()
        }) {
            if intended_change_is_present(failure) {
                failure.mark_superseded(repository_fingerprint.to_owned());
                superseded.push(failure.id.clone());
            }
        }
        superseded
    }
}
