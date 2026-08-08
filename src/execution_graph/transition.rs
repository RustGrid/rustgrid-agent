#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum ExecutionDomainEvent {
    DiscoveryStarted {
        sequence: u64,
    },
    RepositoryEvidenceRecorded {
        sequence: u64,
        evidence_id: String,
        repository_fingerprint: String,
        /// New checkpoints carry the complete immutable observation so event
        /// replay reconstructs the EvidenceStore. Older checkpoints omit it
        /// and retain their already-materialized store for compatibility.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<FileEvidence>,
    },
    DiscoveryCompleted {
        sequence: u64,
        repository_fingerprint: String,
    },
    ComplexityClassified {
        sequence: u64,
        assessment: ComplexityAssessment,
    },
    PlanAccepted {
        sequence: u64,
        target_count: u32,
    },
    PlanRepaired {
        sequence: u64,
        repaired_criterion_ids: Vec<String>,
    },
    GraphCreated {
        sequence: u64,
        graph_id: String,
        revision: u64,
        /// Carries the authoritative topology for append-only replay. Legacy
        /// checkpoints may omit it because they already persist a materialized
        /// graph alongside the event stream.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph: Option<ExecutionGraph>,
        /// Exact semantic identities retained from the previous topology.
        /// Stores and budget usage are reduced against this set during replay.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        preserved_node_ids: Vec<ExecutionNodeId>,
    },
    NodeStarted {
        sequence: u64,
        node_id: ExecutionNodeId,
        attempt: u32,
        started_at: String,
        repository_fingerprint: String,
    },
    MutationRepairAllowanceRestored {
        sequence: u64,
        node_id: ExecutionNodeId,
    },
    MutationRepairAllowanceConsumed {
        sequence: u64,
        node_id: ExecutionNodeId,
    },
    TargetContextPrepared {
        sequence: u64,
        node_id: ExecutionNodeId,
        target_path: String,
        #[serde(default)]
        operation: TargetOperation,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_path: Option<RepositoryPath>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_exists: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_exists: Option<bool>,
        repository_fingerprint: RepositoryFingerprint,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_content_hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_content_hash: Option<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        accepted_intent_hash: String,
        #[serde(default)]
        evidence_ids: Vec<String>,
    },
    TargetMutationIntentRecorded {
        sequence: u64,
        node_id: ExecutionNodeId,
        target_path: RepositoryPath,
        operation: TargetOperation,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_path: Option<RepositoryPath>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_result_content_hash: Option<ContentHash>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_source_content_hash: Option<ContentHash>,
        repository_fingerprint: RepositoryFingerprint,
        accepted_intent_hash: String,
    },
    TargetMutationProduced {
        sequence: u64,
        node_id: ExecutionNodeId,
        target_path: String,
        expected_repository_fingerprint: RepositoryFingerprint,
        repository_fingerprint: RepositoryFingerprint,
        before_content_hash: Option<String>,
        after_content_hash: Option<String>,
    },
    MutationApplied {
        sequence: u64,
        node_id: ExecutionNodeId,
        target_path: String,
        repository_fingerprint: String,
        evidence_id: String,
        /// Completion time captured after deterministic target verification.
        /// Legacy journals replay using the active attempt's stable start time.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        completed_at: String,
        #[serde(default)]
        satisfied_intent: SatisfiedIntent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repair_failure_id: Option<FailureId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_target_evidence: Option<CreatedTargetEvidence>,
    },
    TargetOperationAlreadyApplied {
        sequence: u64,
        execution_id: String,
        attempt: u32,
        transition: AlreadyAppliedTransition,
        semantic_id: String,
        #[serde(default)]
        satisfied_intent: SatisfiedIntent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repair_failure_id: Option<FailureId>,
    },
    MutationRejected {
        sequence: u64,
        node_id: ExecutionNodeId,
        failure: FailureRecord,
    },
    MutationSuperseded {
        sequence: u64,
        node_id: ExecutionNodeId,
        failure_id: FailureId,
        repository_fingerprint: String,
    },
    /// Records a failure for any execution node. Mutation-specific callers may
    /// continue to use `MutationRejected`; this variant exists for discovery,
    /// planning, validation, review, publication, and infrastructure failures
    /// whose full state must be reconstructible from the event stream.
    FailureRecorded {
        sequence: u64,
        failure: FailureRecord,
    },
    FailureRecovered {
        sequence: u64,
        node_id: ExecutionNodeId,
        failure_id: FailureId,
        repository_fingerprint: String,
    },
    FailureSuperseded {
        sequence: u64,
        node_id: ExecutionNodeId,
        failure_id: FailureId,
        repository_fingerprint: String,
    },
    NodeCompleted {
        sequence: u64,
        node_id: ExecutionNodeId,
        status: ExecutionNodeStatus,
    },
    ValidationStarted {
        sequence: u64,
        node_id: ExecutionNodeId,
        fingerprint: String,
    },
    ValidationEvidenceRecorded {
        sequence: u64,
        node_id: ExecutionNodeId,
        evidence: ValidationEvidenceRecord,
    },
    ValidationPassed {
        sequence: u64,
        node_id: ExecutionNodeId,
        evidence_id: String,
        fingerprint: String,
    },
    ValidationFailed {
        sequence: u64,
        node_id: ExecutionNodeId,
        failure_id: FailureId,
        fingerprint: String,
    },
    ValidationRepairStarted {
        sequence: u64,
        validation_node_id: ExecutionNodeId,
        failure_id: FailureId,
        #[serde(default)]
        repair_intent: ValidationRepairIntent,
        selected_target: String,
        #[serde(default)]
        implicated_paths: Vec<String>,
        #[serde(default)]
        correction_contracts: Vec<AssertionRepairContract>,
        #[serde(default)]
        requested_tool_policy: MutationToolPolicy,
        #[serde(default)]
        repository_fingerprint_before: RepositoryFingerprint,
    },
    ValidationRepairCompleted {
        sequence: u64,
        validation_node_id: ExecutionNodeId,
        failure_id: FailureId,
        result: RepairResult,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<ValidationRepairAttempt>,
    },
    ValidationSuperseded {
        sequence: u64,
        node_id: ExecutionNodeId,
        evidence_id: String,
        repository_fingerprint: String,
    },
    /// Invalidates all finalization derived from an earlier repository state.
    /// The evidence ids are the canonical, complete set of validation passes
    /// invalidated by this repository observation.
    FinalizationInvalidated {
        sequence: u64,
        repository_fingerprint: String,
        stale_validation_evidence_ids: Vec<String>,
    },
    DiffReviewed {
        sequence: u64,
        node_id: ExecutionNodeId,
        evidence_ids: Vec<String>,
    },
    IncompleteDiffReviewRequested {
        sequence: u64,
        node_id: ExecutionNodeId,
        reason: IncompleteReason,
        #[serde(default)]
        dependency_overrides: Vec<DependencyOverride>,
    },
    CompletionEvaluated {
        sequence: u64,
        node_id: ExecutionNodeId,
        outcome: MissionOutcome,
    },
    /// Authorizes a draft recovery publication from current validation proof
    /// without claiming that diff review or completion evaluation succeeded.
    RecoveryPublicationRequested {
        sequence: u64,
        node_id: ExecutionNodeId,
        repository_fingerprint: String,
        validation_evidence_ids: Vec<String>,
    },
    PublicationStarted {
        sequence: u64,
        node_id: ExecutionNodeId,
        mode: PublicationMode,
    },
    CommitCreated {
        sequence: u64,
        node_id: ExecutionNodeId,
        commit_sha: String,
    },
    BranchPushed {
        sequence: u64,
        node_id: ExecutionNodeId,
        branch: String,
    },
    PullRequestCreated {
        sequence: u64,
        node_id: ExecutionNodeId,
        url: String,
        number: Option<u64>,
        draft: bool,
    },
    GuardrailTriggered {
        sequence: u64,
        reason: GuardrailReason,
        outcome: MissionOutcome,
        detail: String,
    },
    CancellationRequested {
        sequence: u64,
        state: CancellationState,
    },
    /// Starts a newer execution attempt from a resumable cancellation
    /// checkpoint. The reducer, rather than startup compatibility code,
    /// clears the canonical cancellation state.
    ExecutionResumed {
        sequence: u64,
        execution_attempt: u32,
        /// A prior partial terminal outcome starts a new continuation epoch;
        /// cancellation-only resumes leave this empty.
        previous_outcome: Option<MissionOutcome>,
    },
    RunFinished {
        sequence: u64,
        outcome: MissionOutcome,
    },
}

impl ExecutionDomainEvent {
    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::DiscoveryStarted { .. } => "discovery_started",
            Self::RepositoryEvidenceRecorded { .. } => "repository_evidence_recorded",
            Self::DiscoveryCompleted { .. } => "discovery_completed",
            Self::ComplexityClassified { .. } => "complexity_classified",
            Self::PlanAccepted { .. } => "plan_accepted",
            Self::PlanRepaired { .. } => "plan_repaired",
            Self::GraphCreated { .. } => "graph_created",
            Self::NodeStarted { .. } => "node_started",
            Self::MutationRepairAllowanceRestored { .. } => {
                "mutation_repair_allowance_restored"
            }
            Self::MutationRepairAllowanceConsumed { .. } => {
                "mutation_repair_allowance_consumed"
            }
            Self::TargetContextPrepared { .. } => "target_context_prepared",
            Self::TargetMutationIntentRecorded { .. } => "target_mutation_intent_recorded",
            Self::TargetMutationProduced { .. } => "target_mutation_produced",
            Self::MutationApplied { .. } => "mutation_applied",
            Self::TargetOperationAlreadyApplied { .. } => "target_operation_already_applied",
            Self::MutationRejected { .. } => "mutation_rejected",
            Self::MutationSuperseded { .. } => "mutation_superseded",
            Self::FailureRecorded { .. } => "failure_recorded",
            Self::FailureRecovered { .. } => "failure_recovered",
            Self::FailureSuperseded { .. } => "failure_superseded",
            Self::NodeCompleted { .. } => "node_completed",
            Self::ValidationStarted { .. } => "validation_started",
            Self::ValidationEvidenceRecorded { .. } => "validation_evidence_recorded",
            Self::ValidationPassed { .. } => "validation_passed",
            Self::ValidationFailed { .. } => "validation_failed",
            Self::ValidationRepairStarted { .. } => "validation_repair_started",
            Self::ValidationRepairCompleted { .. } => "validation_repair_completed",
            Self::ValidationSuperseded { .. } => "validation_superseded",
            Self::FinalizationInvalidated { .. } => "finalization_invalidated",
            Self::DiffReviewed { .. } => "diff_reviewed",
            Self::IncompleteDiffReviewRequested { .. } => "incomplete_diff_review_requested",
            Self::CompletionEvaluated { .. } => "completion_evaluated",
            Self::RecoveryPublicationRequested { .. } => "recovery_publication_requested",
            Self::PublicationStarted { .. } => "publication_started",
            Self::CommitCreated { .. } => "commit_created",
            Self::BranchPushed { .. } => "branch_pushed",
            Self::PullRequestCreated { .. } => "pull_request_created",
            Self::GuardrailTriggered { .. } => "guardrail_triggered",
            Self::CancellationRequested { .. } => "cancellation_requested",
            Self::ExecutionResumed { .. } => "execution_resumed",
            Self::RunFinished { .. } => "run_finished",
        }
    }

    pub const fn sequence(&self) -> u64 {
        match self {
            Self::DiscoveryStarted { sequence }
            | Self::RepositoryEvidenceRecorded { sequence, .. }
            | Self::DiscoveryCompleted { sequence, .. }
            | Self::ComplexityClassified { sequence, .. }
            | Self::PlanAccepted { sequence, .. }
            | Self::PlanRepaired { sequence, .. }
            | Self::GraphCreated { sequence, .. }
            | Self::NodeStarted { sequence, .. }
            | Self::MutationRepairAllowanceRestored { sequence, .. }
            | Self::MutationRepairAllowanceConsumed { sequence, .. }
            | Self::TargetContextPrepared { sequence, .. }
            | Self::TargetMutationIntentRecorded { sequence, .. }
            | Self::TargetMutationProduced { sequence, .. }
            | Self::MutationApplied { sequence, .. }
            | Self::TargetOperationAlreadyApplied { sequence, .. }
            | Self::MutationRejected { sequence, .. }
            | Self::MutationSuperseded { sequence, .. }
            | Self::FailureRecorded { sequence, .. }
            | Self::FailureRecovered { sequence, .. }
            | Self::FailureSuperseded { sequence, .. }
            | Self::NodeCompleted { sequence, .. }
            | Self::ValidationStarted { sequence, .. }
            | Self::ValidationEvidenceRecorded { sequence, .. }
            | Self::ValidationPassed { sequence, .. }
            | Self::ValidationFailed { sequence, .. }
            | Self::ValidationRepairStarted { sequence, .. }
            | Self::ValidationRepairCompleted { sequence, .. }
            | Self::ValidationSuperseded { sequence, .. }
            | Self::FinalizationInvalidated { sequence, .. }
            | Self::DiffReviewed { sequence, .. }
            | Self::IncompleteDiffReviewRequested { sequence, .. }
            | Self::CompletionEvaluated { sequence, .. }
            | Self::RecoveryPublicationRequested { sequence, .. }
            | Self::PublicationStarted { sequence, .. }
            | Self::CommitCreated { sequence, .. }
            | Self::BranchPushed { sequence, .. }
            | Self::PullRequestCreated { sequence, .. }
            | Self::GuardrailTriggered { sequence, .. }
            | Self::CancellationRequested { sequence, .. }
            | Self::ExecutionResumed { sequence, .. }
            | Self::RunFinished { sequence, .. } => *sequence,
        }
    }

    pub fn node_id(&self) -> Option<&ExecutionNodeId> {
        match self {
            Self::NodeStarted { node_id, .. }
            | Self::MutationRepairAllowanceRestored { node_id, .. }
            | Self::MutationRepairAllowanceConsumed { node_id, .. }
            | Self::TargetContextPrepared { node_id, .. }
            | Self::TargetMutationIntentRecorded { node_id, .. }
            | Self::TargetMutationProduced { node_id, .. }
            | Self::MutationApplied { node_id, .. }
            | Self::MutationRejected { node_id, .. }
            | Self::MutationSuperseded { node_id, .. }
            | Self::FailureRecovered { node_id, .. }
            | Self::FailureSuperseded { node_id, .. }
            | Self::NodeCompleted { node_id, .. }
            | Self::ValidationStarted { node_id, .. }
            | Self::ValidationEvidenceRecorded { node_id, .. }
            | Self::ValidationPassed { node_id, .. }
            | Self::ValidationFailed { node_id, .. }
            | Self::ValidationSuperseded { node_id, .. }
            | Self::DiffReviewed { node_id, .. }
            | Self::IncompleteDiffReviewRequested { node_id, .. }
            | Self::CompletionEvaluated { node_id, .. }
            | Self::RecoveryPublicationRequested { node_id, .. }
            | Self::PublicationStarted { node_id, .. }
            | Self::CommitCreated { node_id, .. }
            | Self::BranchPushed { node_id, .. }
            | Self::PullRequestCreated { node_id, .. } => Some(node_id),
            Self::TargetOperationAlreadyApplied { transition, .. } => Some(&transition.node_id),
            Self::FailureRecorded { failure, .. } => Some(&failure.node_id),
            Self::ValidationRepairCompleted {
                validation_node_id,
                ..
            }
            | Self::ValidationRepairStarted {
                validation_node_id,
                ..
            } => Some(validation_node_id),
            _ => None,
        }
    }

    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::RunFinished { .. })
    }
}

/// Returns only events in the active execution epoch. `ExecutionResumed`
/// starts a new epoch, so terminal and guardrail decisions from a published
/// partial attempt cannot suppress decisions in its continuation.
pub fn current_execution_epoch(events: &[ExecutionDomainEvent]) -> &[ExecutionDomainEvent] {
    let start = events
        .iter()
        .rposition(|event| matches!(event, ExecutionDomainEvent::ExecutionResumed { .. }))
        .map_or(0, |position| position.saturating_add(1));
    &events[start..]
}

pub fn mutation_repair_allowance_is_restored(
    events: &[ExecutionDomainEvent],
    node_id: &ExecutionNodeId,
) -> bool {
    events.iter().rev().find_map(|event| match event {
        ExecutionDomainEvent::MutationRepairAllowanceRestored {
            node_id: event_node_id,
            ..
        } if event_node_id == node_id => Some(true),
        ExecutionDomainEvent::MutationRepairAllowanceConsumed {
            node_id: event_node_id,
            ..
        } if event_node_id == node_id => Some(false),
        _ => None,
    }) == Some(true)
}

pub fn current_epoch_terminal_outcome(events: &[ExecutionDomainEvent]) -> Option<MissionOutcome> {
    current_execution_epoch(events)
        .iter()
        .rev()
        .find_map(|event| match event {
            ExecutionDomainEvent::RunFinished { outcome, .. } => Some(*outcome),
            _ => None,
        })
}

impl ExecutionGraph {
    pub fn apply_domain_event(
        &mut self,
        event: &ExecutionDomainEvent,
    ) -> Result<(), GraphInvariantError> {
        self.apply_domain_event_with_dependency_satisfaction(event, &BTreeSet::new())
    }

    fn apply_domain_event_with_dependency_satisfaction(
        &mut self,
        event: &ExecutionDomainEvent,
        additionally_satisfied: &BTreeSet<ExecutionNodeId>,
    ) -> Result<(), GraphInvariantError> {
        let graph_before = self.clone();
        let revision_before = self.revision;
        let satisfied = self.dependency_satisfaction_ids(additionally_satisfied);
        let guarded_node = match event {
            ExecutionDomainEvent::NodeStarted { node_id, .. }
            | ExecutionDomainEvent::TargetContextPrepared { node_id, .. }
            | ExecutionDomainEvent::TargetMutationIntentRecorded { node_id, .. }
            | ExecutionDomainEvent::TargetMutationProduced { node_id, .. }
            | ExecutionDomainEvent::MutationApplied { node_id, .. }
            | ExecutionDomainEvent::MutationSuperseded { node_id, .. }
            | ExecutionDomainEvent::FailureSuperseded { node_id, .. }
            | ExecutionDomainEvent::ValidationStarted { node_id, .. }
            | ExecutionDomainEvent::ValidationPassed { node_id, .. }
            | ExecutionDomainEvent::DiffReviewed { node_id, .. }
            | ExecutionDomainEvent::CompletionEvaluated { node_id, .. }
            | ExecutionDomainEvent::PublicationStarted { node_id, .. }
            | ExecutionDomainEvent::CommitCreated { node_id, .. }
            | ExecutionDomainEvent::BranchPushed { node_id, .. }
            | ExecutionDomainEvent::PullRequestCreated { node_id, .. } => Some(node_id),
            ExecutionDomainEvent::TargetOperationAlreadyApplied { transition, .. } => Some(&transition.node_id),
            ExecutionDomainEvent::NodeCompleted {
                node_id, status, ..
            } if status.satisfies_dependency() && *status != ExecutionNodeStatus::Skipped => {
                Some(node_id)
            }
            _ => None,
        };
        if let Some(node_id) = guarded_node {
            self.ensure_node_dependencies_satisfied(node_id, &satisfied)?;
        }
        self.validate_event_node_kind(event)?;

        match event {
            ExecutionDomainEvent::NodeStarted {
                node_id,
                attempt,
                started_at,
                repository_fingerprint,
                ..
            } => {
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.status = ExecutionNodeStatus::Running;
                if node.kind.is_mutation() {
                    node.repository_mutation_lifecycle =
                        Some(RepositoryMutationLifecycle::Proposed);
                }
                if !node
                    .attempts
                    .iter()
                    .any(|existing| existing.attempt == *attempt)
                {
                    node.attempts.push(NodeAttempt {
                        attempt: *attempt,
                        started_at: started_at.clone(),
                        repository_fingerprint_before: repository_fingerprint.clone(),
                        ..NodeAttempt::default()
                    });
                }
                self.revision = self.revision.saturating_add(1);
            }
            ExecutionDomainEvent::TargetContextPrepared { .. }
            => {
                // These events advance the action state while the same node
                // attempt remains Running. The orchestrator derives the next
                // action from the append-only event stream.
                // Action-stream facts do not mutate durable graph state.
            }
            ExecutionDomainEvent::TargetMutationIntentRecorded { node_id, .. } => {
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.repository_mutation_lifecycle = Some(RepositoryMutationLifecycle::Validated);
                self.revision = self.revision.saturating_add(1);
            }
            ExecutionDomainEvent::TargetMutationProduced { node_id, .. } => {
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.repository_mutation_lifecycle =
                    Some(RepositoryMutationLifecycle::AppliedUnverified);
                self.revision = self.revision.saturating_add(1);
            }
            ExecutionDomainEvent::MutationApplied {
                node_id,
                repository_fingerprint,
                evidence_id,
                completed_at,
                satisfied_intent,
                ..
            } => {
                let node = self.node(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                let target = node.target.as_ref().ok_or_else(|| {
                    GraphInvariantError::new(format!("mutation node `{node_id}` has no target"))
                })?;
                let attempt = node.attempts.last().ok_or_else(|| {
                    GraphInvariantError::new(format!("active mutation node `{node_id}` has no attempt"))
                })?;
                let operation = target.effective_operation();
                let target_path = operation.destination_path(&target.path).to_owned();
                let attempt_number = attempt.attempt;
                let fingerprint_before = attempt.repository_fingerprint_before.clone();
                let completion_time = if completed_at.is_empty() {
                    attempt.started_at.clone()
                } else {
                    completed_at.clone()
                };
                reduce_repository_operation(
                    self,
                    node_id.clone(),
                    OperationIntent {
                        operation,
                        target_path,
                        expected_result_hash: None,
                        satisfied_intent: *satisfied_intent,
                    },
                    RepositoryOperationResult::Verified {
                        outcome: RepositoryOperationOutcome::Applied,
                        evidence: SuccessfulOperationEvidence::Applied {
                            before: RepositoryFingerprint::new(fingerprint_before),
                            after: RepositoryFingerprint::new(repository_fingerprint.clone()),
                        },
                        observed_result_hash: None,
                        semantic_id: evidence_id.clone(),
                        attempt: attempt_number,
                        completed_at: completion_time,
                    },
                )
                .map_err(|error| GraphInvariantError::new(error.to_string()))?;
            }
            ExecutionDomainEvent::TargetOperationAlreadyApplied {
                execution_id,
                attempt,
                transition,
                semantic_id,
                ..
            } => {
                if semantic_id != &transition.semantic_id(execution_id, *attempt) {
                    return Err(GraphInvariantError::new("already-applied transition semantic identity does not match its payload"));
                }
                self.apply_already_applied_transition(execution_id, *attempt, transition)
                    .map_err(|error| GraphInvariantError::new(error.to_string()))?;
            }
            ExecutionDomainEvent::MutationRejected {
                node_id, failure, ..
            } => {
                let status = failure.category.node_status();
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.status = status;
                node.repository_mutation_lifecycle = Some(if matches!(
                    failure.category,
                    FailureCategory::MutationConflict | FailureCategory::PlanRepositoryConflict
                ) {
                    RepositoryMutationLifecycle::Conflict
                } else {
                    RepositoryMutationLifecycle::Rejected
                });
                if let Some(attempt) = node.attempts.last_mut() {
                    attempt.outcome = Some(status);
                    attempt.failure_id = Some(failure.id.clone());
                }
                self.revision = self.revision.saturating_add(1);
            }
            ExecutionDomainEvent::MutationSuperseded { .. } => {
                // Failure reconciliation is independent from node lifecycle.
                // A later verified repository result owns node completion.
            }
            ExecutionDomainEvent::FailureRecorded { failure, .. } => {
                self.set_node_status(&failure.node_id, failure.category.node_status())?;
                // Validation correctness is owned by the validation node. A
                // failed assertion may schedule repair for an already-applied
                // target, but it must never erase that target's verified
                // mutation result.
            }
            ExecutionDomainEvent::FailureRecovered { node_id, .. } => {
                let reset_validation_evidence = self
                    .node(node_id)
                    .is_some_and(|node| node.kind.is_validation());
                if let Some(node) = self.node_mut(node_id)
                    && matches!(
                        node.status,
                        ExecutionNodeStatus::FailedRecoverable
                            | ExecutionNodeStatus::FailedBlocking
                    )
                {
                    node.status = ExecutionNodeStatus::Pending;
                    if reset_validation_evidence {
                        node.evidence_ids.clear();
                    }
                    self.revision = self.revision.saturating_add(1);
                    self.refresh_readiness();
                }
            }
            ExecutionDomainEvent::FailureSuperseded { .. } => {
                // The failure store applies the status change. Never use
                // failure vocabulary as an execution-node transition.
            }
            ExecutionDomainEvent::NodeCompleted {
                node_id, status, ..
            } => {
                self.set_node_status(node_id, *status)?;
            }
            ExecutionDomainEvent::ValidationStarted { node_id, .. } => {
                self.set_node_status(node_id, ExecutionNodeStatus::Running)?;
            }
            ExecutionDomainEvent::ValidationEvidenceRecorded {
                node_id, evidence, ..
            } => {
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                if !node.evidence_ids.contains(&evidence.evidence_id) {
                    node.evidence_ids.push(evidence.evidence_id.clone());
                    self.revision = self.revision.saturating_add(1);
                }
            }
            ExecutionDomainEvent::ValidationPassed {
                node_id,
                evidence_id,
                ..
            } => {
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.status = ExecutionNodeStatus::Passed;
                if !node.evidence_ids.contains(evidence_id) {
                    node.evidence_ids.push(evidence_id.clone());
                }
                self.revision = self.revision.saturating_add(1);
                self.refresh_readiness();
            }
            ExecutionDomainEvent::ValidationFailed { node_id, .. } => {
                let status = if self
                    .node(node_id)
                    .is_some_and(|node| node.status == ExecutionNodeStatus::FailedBlocking)
                {
                    ExecutionNodeStatus::FailedBlocking
                } else {
                    ExecutionNodeStatus::FailedRecoverable
                };
                self.set_node_status(node_id, status)?;
            }
            ExecutionDomainEvent::ValidationRepairCompleted { .. } => {
                // The result remains an append-only reconciliation fact. A
                // no-mutation result intentionally leaves the failed
                // validation node and every applied mutation node unchanged.
                self.revision = self.revision.saturating_add(1);
            }
            ExecutionDomainEvent::ValidationRepairStarted { .. } => {
                // Validation repair is separate work. The selected mutation
                // target remains Applied until a new verified mutation event
                // replaces its repository evidence.
                self.revision = self.revision.saturating_add(1);
            }
            ExecutionDomainEvent::ValidationSuperseded {
                node_id,
                evidence_id,
                ..
            } => {
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.evidence_ids.retain(|id| id != evidence_id);
                node.status = ExecutionNodeStatus::Pending;
                self.revision = self.revision.saturating_add(1);
                self.refresh_readiness();
            }
            ExecutionDomainEvent::FinalizationInvalidated { .. } => {
                self.recovery_publication_dependency_override = false;
                self.dependency_overrides.clear();
                for node in &mut self.nodes {
                    if node.kind.is_validation()
                        || matches!(
                            node.kind,
                            ExecutionNodeKind::DiffReview
                                | ExecutionNodeKind::CompletionEvaluation
                                | ExecutionNodeKind::Publication
                        )
                    {
                        node.status = ExecutionNodeStatus::Pending;
                        node.evidence_ids.clear();
                    }
                }
                self.revision = self.revision.saturating_add(1);
                self.refresh_readiness();
            }
            ExecutionDomainEvent::DiffReviewed {
                node_id,
                evidence_ids,
                ..
            } => {
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.status = ExecutionNodeStatus::Completed;
                for evidence_id in evidence_ids {
                    if !node.evidence_ids.contains(evidence_id) {
                        node.evidence_ids.push(evidence_id.clone());
                    }
                }
                self.revision = self.revision.saturating_add(1);
                self.refresh_readiness();
            }
            ExecutionDomainEvent::IncompleteDiffReviewRequested {
                node_id,
                dependency_overrides,
                ..
            } => {
                for override_ in dependency_overrides {
                    if override_.dependent_node == *node_id
                        && override_.allowed_outcome == MissionOutcome::PartialReviewable
                        && !self.dependency_overrides.contains(override_)
                    {
                        self.dependency_overrides.push(override_.clone());
                    }
                }
                self.revision = self.revision.saturating_add(1);
                self.refresh_readiness();
            }
            ExecutionDomainEvent::CompletionEvaluated { node_id, .. } => {
                self.set_node_status(node_id, ExecutionNodeStatus::Completed)?;
            }
            ExecutionDomainEvent::RecoveryPublicationRequested { node_id, .. } => {
                self.recovery_publication_dependency_override = true;
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.status = ExecutionNodeStatus::Running;
                self.revision = self.revision.saturating_add(1);
                self.refresh_readiness();
            }
            ExecutionDomainEvent::PublicationStarted { node_id, .. } => {
                self.recovery_publication_dependency_override = false;
                self.set_node_status(node_id, ExecutionNodeStatus::Running)?;
            }
            ExecutionDomainEvent::PullRequestCreated { node_id, .. } => {
                self.set_node_status(node_id, ExecutionNodeStatus::Completed)?;
            }
            ExecutionDomainEvent::DiscoveryStarted { .. } => {
                if let Some(id) = self
                    .nodes
                    .iter()
                    .find(|node| node.kind == ExecutionNodeKind::Discovery)
                    .map(|node| node.id.clone())
                {
                    self.set_node_status(&id, ExecutionNodeStatus::Running)?;
                }
            }
            ExecutionDomainEvent::DiscoveryCompleted { .. } => {
                if let Some(id) = self
                    .nodes
                    .iter()
                    .find(|node| node.kind == ExecutionNodeKind::Discovery)
                    .map(|node| node.id.clone())
                {
                    self.set_node_status(&id, ExecutionNodeStatus::Completed)?;
                }
            }
            ExecutionDomainEvent::GuardrailTriggered {
                outcome: MissionOutcome::PartialReviewable,
                ..
            } => {
                let mut changed = false;
                for node in self
                    .nodes
                    .iter_mut()
                    .filter(|node| node.kind.is_mutation() && !node.status.satisfies_dependency())
                {
                    changed |= self
                        .dependency_satisfaction_overrides
                        .insert(node.id.clone());
                    if node.status == ExecutionNodeStatus::Running {
                        node.status = ExecutionNodeStatus::Pending;
                        changed = true;
                    }
                }
                if changed {
                    self.revision = self.revision.saturating_add(1);
                    self.refresh_readiness();
                }
            }
            ExecutionDomainEvent::ExecutionResumed {
                previous_outcome: Some(MissionOutcome::PartialReviewable),
                ..
            } => {
                let mut changed = !self.dependency_satisfaction_overrides.is_empty()
                    || !self.dependency_overrides.is_empty()
                    || self.recovery_publication_dependency_override;
                self.dependency_satisfaction_overrides.clear();
                self.dependency_overrides.clear();
                self.recovery_publication_dependency_override = false;
                for node in &mut self.nodes {
                    if node.kind.is_mutation() {
                        if node.status == ExecutionNodeStatus::Running {
                            node.status = ExecutionNodeStatus::Pending;
                            changed = true;
                        } else if node.status == ExecutionNodeStatus::Applied {
                            // `Applied` is the legacy terminal representation
                            // for a verified write. Normalize it at the explicit
                            // new-attempt boundary so validation uses the same
                            // Completed barrier as current reducers.
                            node.status = ExecutionNodeStatus::Completed;
                            if let Some(attempt) = node.attempts.last_mut() {
                                attempt.completed_at.get_or_insert_with(|| {
                                    attempt.started_at.clone()
                                });
                                attempt.outcome = Some(ExecutionNodeStatus::Completed);
                            }
                            changed = true;
                        }
                        continue;
                    }
                    if node.kind.is_validation()
                        || matches!(
                            node.kind,
                            ExecutionNodeKind::DiffReview
                                | ExecutionNodeKind::CompletionEvaluation
                                | ExecutionNodeKind::Publication
                        )
                    {
                        changed |= node.status != ExecutionNodeStatus::Pending
                            || !node.evidence_ids.is_empty();
                        node.status = ExecutionNodeStatus::Pending;
                        node.evidence_ids.clear();
                    }
                }
                if changed {
                    self.revision = self.revision.saturating_add(1);
                }
                self.refresh_readiness();
            }
            ExecutionDomainEvent::CommitCreated { .. }
            | ExecutionDomainEvent::BranchPushed { .. }
            | ExecutionDomainEvent::MutationRepairAllowanceRestored { .. }
            | ExecutionDomainEvent::MutationRepairAllowanceConsumed { .. }
            | ExecutionDomainEvent::RepositoryEvidenceRecorded { .. }
            | ExecutionDomainEvent::ComplexityClassified { .. }
            | ExecutionDomainEvent::PlanAccepted { .. }
            | ExecutionDomainEvent::PlanRepaired { .. }
            | ExecutionDomainEvent::GraphCreated { .. }
            | ExecutionDomainEvent::GuardrailTriggered { .. }
            | ExecutionDomainEvent::CancellationRequested { .. }
            | ExecutionDomainEvent::ExecutionResumed { .. }
            | ExecutionDomainEvent::RunFinished { .. } => {}
        }
        let mut before_without_revision = graph_before;
        before_without_revision.revision = self.revision;
        if *self == before_without_revision {
            self.revision = revision_before;
        } else {
            self.revision = revision_before.saturating_add(1);
        }
        Ok(())
    }

    fn validate_event_node_kind(
        &self,
        event: &ExecutionDomainEvent,
    ) -> Result<(), GraphInvariantError> {
        let Some(node_id) = event.node_id() else {
            return Ok(());
        };
        let node = self.node(node_id).ok_or_else(|| {
            GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
        })?;
        let kind_matches = match event {
            ExecutionDomainEvent::MutationApplied { .. }
            | ExecutionDomainEvent::TargetOperationAlreadyApplied { .. }
            | ExecutionDomainEvent::MutationRepairAllowanceRestored { .. }
            | ExecutionDomainEvent::MutationRepairAllowanceConsumed { .. }
            | ExecutionDomainEvent::TargetMutationIntentRecorded { .. }
            | ExecutionDomainEvent::MutationRejected { .. }
            | ExecutionDomainEvent::MutationSuperseded { .. } => node.kind.is_mutation(),
            ExecutionDomainEvent::FailureRecorded { failure, .. } => {
                failure.category.is_valid_for_node_kind(node.kind)
            }
            ExecutionDomainEvent::FailureSuperseded { .. } => node.kind.is_mutation(),
            ExecutionDomainEvent::ValidationStarted { .. }
            | ExecutionDomainEvent::ValidationEvidenceRecorded { .. }
            | ExecutionDomainEvent::ValidationPassed { .. }
            | ExecutionDomainEvent::ValidationFailed { .. }
            | ExecutionDomainEvent::ValidationSuperseded { .. } => node.kind.is_validation(),
            ExecutionDomainEvent::ValidationRepairCompleted { .. } => node.kind.is_validation(),
            ExecutionDomainEvent::ValidationRepairStarted { .. } => node.kind.is_validation(),
            ExecutionDomainEvent::DiffReviewed { .. } => node.kind == ExecutionNodeKind::DiffReview,
            ExecutionDomainEvent::IncompleteDiffReviewRequested { .. } => {
                node.kind == ExecutionNodeKind::DiffReview
            }
            ExecutionDomainEvent::CompletionEvaluated { .. } => {
                node.kind == ExecutionNodeKind::CompletionEvaluation
            }
            ExecutionDomainEvent::PublicationStarted { .. }
            | ExecutionDomainEvent::RecoveryPublicationRequested { .. }
            | ExecutionDomainEvent::CommitCreated { .. }
            | ExecutionDomainEvent::BranchPushed { .. }
            | ExecutionDomainEvent::PullRequestCreated { .. } => {
                node.kind == ExecutionNodeKind::Publication
            }
            ExecutionDomainEvent::FailureRecovered { .. } => true,
            _ => true,
        };
        if !kind_matches {
            return Err(GraphInvariantError::new(format!(
                "event `{}` is incompatible with node `{node_id}` of kind `{:?}`",
                event.event_type(),
                node.kind
            )));
        }
        Ok(())
    }
}
