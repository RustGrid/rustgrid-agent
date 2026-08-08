#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvariantScope {
    Always,
    RepositoryOperationReduction,
    Implementation,
    ImplementationBarrier,
    Validation,
    DiffReview,
    Completion,
    Publication,
    Terminal,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    RepositoryOperationReduction,
    #[default]
    Implementation,
    ImplementationBarrier,
    Validation,
    DiffReview,
    Completion,
    Publication,
    Terminal,
}

impl LifecycleState {
    pub const fn scope(self) -> InvariantScope {
        match self {
            Self::RepositoryOperationReduction => InvariantScope::RepositoryOperationReduction,
            Self::Implementation => InvariantScope::Implementation,
            Self::ImplementationBarrier => InvariantScope::ImplementationBarrier,
            Self::Validation => InvariantScope::Validation,
            Self::DiffReview => InvariantScope::DiffReview,
            Self::Completion => InvariantScope::Completion,
            Self::Publication => InvariantScope::Publication,
            Self::Terminal => InvariantScope::Terminal,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvariantTrigger {
    Startup,
    RepositoryOperationReduced,
    #[default]
    PhaseTransition,
    CompletionDecision,
    PublicationDecision,
    TerminalProjection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleInvariantDefinition {
    pub id: &'static str,
    pub scope: InvariantScope,
    pub required_evidence: Vec<EvidenceKind>,
    pub required_node_kinds: Vec<ExecutionNodeKind>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvariantViolation {
    pub code: &'static str,
    pub scope: InvariantScope,
    pub lifecycle: LifecycleState,
    pub trigger: InvariantTrigger,
    pub node_id: Option<ExecutionNodeId>,
    pub message: String,
}

impl fmt::Display for InvariantViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invariant={} scope={:?} lifecycle={:?} trigger={:?}: {}",
            self.code, self.scope, self.lifecycle, self.trigger, self.message
        )
    }
}

impl std::error::Error for InvariantViolation {}

pub fn lifecycle_invariant_definitions() -> Vec<LifecycleInvariantDefinition> {
    vec![
        LifecycleInvariantDefinition {
            id: "verified_operation_missing_operation_evidence",
            scope: InvariantScope::RepositoryOperationReduction,
            required_evidence: vec![EvidenceKind::RepositoryOperationVerification],
            required_node_kinds: vec![
                ExecutionNodeKind::SourceMutation,
                ExecutionNodeKind::TestMutation,
            ],
        },
        LifecycleInvariantDefinition {
            id: "validation_started_before_implementation_barrier",
            scope: InvariantScope::Validation,
            required_evidence: vec![EvidenceKind::ImplementationBarrierProof],
            required_node_kinds: vec![
                ExecutionNodeKind::SourceMutation,
                ExecutionNodeKind::TestMutation,
            ],
        },
        LifecycleInvariantDefinition {
            id: "current_validation_missing_at_diff_review",
            scope: InvariantScope::DiffReview,
            required_evidence: vec![EvidenceKind::ValidationGateResult],
            required_node_kinds: vec![
                ExecutionNodeKind::ValidationFocused,
                ExecutionNodeKind::ValidationSuite,
                ExecutionNodeKind::ValidationBuild,
                ExecutionNodeKind::ValidationLint,
            ],
        },
        LifecycleInvariantDefinition {
            id: "current_validation_missing_at_completion",
            scope: InvariantScope::Completion,
            required_evidence: vec![EvidenceKind::ValidationGateResult],
            required_node_kinds: vec![
                ExecutionNodeKind::ValidationFocused,
                ExecutionNodeKind::ValidationSuite,
                ExecutionNodeKind::ValidationBuild,
                ExecutionNodeKind::ValidationLint,
            ],
        },
        LifecycleInvariantDefinition {
            id: "current_validation_missing_at_publication",
            scope: InvariantScope::Publication,
            required_evidence: vec![EvidenceKind::ValidationGateResult],
            required_node_kinds: vec![
                ExecutionNodeKind::ValidationFocused,
                ExecutionNodeKind::ValidationSuite,
                ExecutionNodeKind::ValidationBuild,
                ExecutionNodeKind::ValidationLint,
            ],
        },
    ]
}

const fn lifecycle_rank(lifecycle: LifecycleState) -> u8 {
    match lifecycle {
        LifecycleState::RepositoryOperationReduction => 1,
        LifecycleState::Implementation => 2,
        LifecycleState::ImplementationBarrier => 3,
        LifecycleState::Validation => 4,
        LifecycleState::DiffReview => 5,
        LifecycleState::Completion => 6,
        LifecycleState::Publication => 7,
        LifecycleState::Terminal => 8,
    }
}

const fn evidence_producer_rank(kind: EvidenceKind) -> u8 {
    match kind {
        EvidenceKind::RepositoryOperationVerification | EvidenceKind::Mutation => 1,
        EvidenceKind::ImplementationBarrierProof => 3,
        EvidenceKind::ValidationGateResult => 4,
        EvidenceKind::DiffReviewResult | EvidenceKind::DiffReview => 5,
        EvidenceKind::CompletionEvaluation | EvidenceKind::Completion => 6,
        EvidenceKind::PublicationEvidence | EvidenceKind::Publication => 7,
        EvidenceKind::RepositoryObservation | EvidenceKind::AcceptanceCriterion => 0,
    }
}

pub const fn evidence_producer_precedes_requirement(
    evidence_kind: EvidenceKind,
    lifecycle: LifecycleState,
) -> bool {
    evidence_producer_rank(evidence_kind) <= lifecycle_rank(lifecycle)
}

pub fn validate_lifecycle_invariant_definitions(
    definitions: &[LifecycleInvariantDefinition],
) -> Result<(), InvariantViolation> {
    for definition in definitions {
        let lifecycle = lifecycle_for_scope(definition.scope);
        if let Some(evidence) = definition
            .required_evidence
            .iter()
            .copied()
            .find(|kind| !evidence_producer_precedes_requirement(*kind, lifecycle))
        {
            return Err(InvariantViolation {
                code: "invariant_requires_future_evidence",
                scope: definition.scope,
                lifecycle,
                trigger: InvariantTrigger::Startup,
                node_id: None,
                message: format!(
                    "invariant `{}` requires {:?}, whose producer runs after {:?}",
                    definition.id, evidence, definition.scope
                ),
            });
        }
    }
    Ok(())
}

const fn lifecycle_for_scope(scope: InvariantScope) -> LifecycleState {
    match scope {
        InvariantScope::Always | InvariantScope::RepositoryOperationReduction => {
            LifecycleState::RepositoryOperationReduction
        }
        InvariantScope::Implementation => LifecycleState::Implementation,
        InvariantScope::ImplementationBarrier => LifecycleState::ImplementationBarrier,
        InvariantScope::Validation => LifecycleState::Validation,
        InvariantScope::DiffReview => LifecycleState::DiffReview,
        InvariantScope::Completion => LifecycleState::Completion,
        InvariantScope::Publication => LifecycleState::Publication,
        InvariantScope::Terminal => LifecycleState::Terminal,
    }
}

fn violation(
    code: &'static str,
    lifecycle: LifecycleState,
    trigger: InvariantTrigger,
    node_id: Option<ExecutionNodeId>,
    message: impl Into<String>,
) -> InvariantViolation {
    InvariantViolation {
        code,
        scope: lifecycle.scope(),
        lifecycle,
        trigger,
        node_id,
        message: message.into(),
    }
}

/// Checks only evidence that can legally exist in the current lifecycle.
/// Repository-operation reduction deliberately never inspects validation
/// evidence, which is produced only after the implementation barrier.
pub fn check_invariants(
    graph: &ExecutionGraph,
    lifecycle: LifecycleState,
    trigger: InvariantTrigger,
) -> Result<(), InvariantViolation> {
    validate_lifecycle_invariant_definitions(&lifecycle_invariant_definitions())?;

    if let Some(node) = graph.nodes().find(|node| {
        node.kind.is_mutation()
            && !node.operation_evidence.is_empty()
            && node.status != ExecutionNodeStatus::Completed
    }) {
        return Err(violation(
            "completed_implementation_node_reopened",
            lifecycle,
            trigger,
            Some(node.id.clone()),
            "implementation operation evidence is immutable after node completion",
        ));
    }

    for node in graph
        .nodes()
        .filter(|node| {
            node.kind.is_mutation()
                && node.status == ExecutionNodeStatus::Completed
                && (node.repository_mutation_lifecycle
                    == Some(RepositoryMutationLifecycle::Verified)
                    || !node.operation_evidence.is_empty())
        })
    {
        node.operation_evidence.last().ok_or_else(|| {
            violation(
                "verified_operation_missing_operation_evidence",
                lifecycle,
                trigger,
                Some(node.id.clone()),
                "completed implementation node has no repository-operation verification evidence",
            )
        })?;
        let attempt = node.attempts.last().ok_or_else(|| {
            violation(
                "verified_operation_missing_completed_attempt",
                lifecycle,
                trigger,
                Some(node.id.clone()),
                "completed implementation node has no attempt record",
            )
        })?;
        let attempt_matches_operation_evidence = attempt
            .repository_fingerprint_after
            .as_deref()
            .is_some_and(|fingerprint| {
                node.operation_evidence.iter().any(|evidence| {
                    evidence.repository_fingerprint.as_str() == fingerprint
                        && attempt.completed_at.as_deref() == Some(evidence.completed_at.as_str())
                })
            });
        if attempt.completed_at.is_none()
            || attempt.outcome != Some(ExecutionNodeStatus::Completed)
            || !attempt_matches_operation_evidence
        {
            return Err(violation(
                "verified_operation_attempt_not_finalized",
                lifecycle,
                trigger,
                Some(node.id.clone()),
                "operation evidence and the finalized attempt disagree",
            ));
        }
        if graph.active_node().is_some_and(|active| active.id == node.id) {
            return Err(violation(
                "completed_node_still_active",
                lifecycle,
                trigger,
                Some(node.id.clone()),
                "completed implementation node still owns execution",
            ));
        }
        for dependent in graph.nodes().filter(|candidate| {
            candidate.dependencies.contains(&node.id)
                && candidate.dependencies.iter().all(|dependency| {
                    graph
                        .node(dependency)
                        .is_some_and(|dependency| dependency.status.satisfies_dependency())
                })
        }) {
            if dependent.status == ExecutionNodeStatus::Pending {
                return Err(violation(
                    "downstream_readiness_not_recomputed",
                    lifecycle,
                    trigger,
                    Some(dependent.id.clone()),
                    "a dependency-satisfied downstream node remained pending",
                ));
            }
        }
    }

    if lifecycle_rank(lifecycle) >= lifecycle_rank(LifecycleState::Validation)
        && !graph.implementation_barrier_satisfied()
    {
        return Err(violation(
            "validation_started_before_implementation_barrier",
            lifecycle,
            trigger,
            graph.next_runnable_node().map(|node| node.id.clone()),
            "required implementation nodes remain unresolved",
        ));
    }
    Ok(())
}

pub fn check_snapshot_invariants(
    snapshot: &ExecutionSnapshot,
    lifecycle: LifecycleState,
    trigger: InvariantTrigger,
) -> Result<(), InvariantViolation> {
    let explicit_partial_or_recovery = snapshot.has_partial_reviewable_guardrail()
        || snapshot.has_incomplete_diff_review_request()
        || snapshot.graph.recovery_publication_dependency_override;
    if lifecycle_rank(lifecycle) >= lifecycle_rank(LifecycleState::DiffReview)
        && explicit_partial_or_recovery
    {
        return Ok(());
    }
    check_invariants(&snapshot.graph, lifecycle, trigger)?;
    if lifecycle_rank(lifecycle) < lifecycle_rank(LifecycleState::DiffReview) {
        return Ok(());
    }
    for node in snapshot
        .graph
        .nodes()
        .filter(|node| node.required && node.kind.is_validation())
    {
        let gate = node.validation.as_ref().ok_or_else(|| {
            violation(
                "required_validation_gate_missing_specification",
                lifecycle,
                trigger,
                Some(node.id.clone()),
                "required validation node has no gate specification",
            )
        })?;
        // Compatibility for pre-evidence checkpoints that persisted a passed
        // validation node before the EvidenceStore/event contract existed.
        // New transitions cannot create this shape: they must record evidence
        // before applying ValidationPassed.
        if node.status == ExecutionNodeStatus::Passed
            && node.evidence_ids.is_empty()
            && snapshot.evidence.validations.is_empty()
            && !snapshot.events.iter().any(|event| {
                matches!(
                    event,
                    ExecutionDomainEvent::ValidationEvidenceRecorded { .. }
                )
            })
        {
            continue;
        }
        let fingerprint = gate.fingerprint(snapshot.current_repository.validation_source_tree_hash());
        if !snapshot.evidence.has_passed_validation(&fingerprint) {
            let code = match lifecycle {
                LifecycleState::DiffReview => "current_validation_missing_at_diff_review",
                LifecycleState::Completion => "current_validation_missing_at_completion",
                LifecycleState::Publication | LifecycleState::Terminal => {
                    "current_validation_missing_at_publication"
                }
                _ => "current_validation_missing",
            };
            return Err(violation(
                code,
                lifecycle,
                trigger,
                Some(node.id.clone()),
                "required validation evidence is absent or stale for the current repository fingerprint",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    #[default]
    Implementation,
    ImplementationBarrier,
    Validation,
    DiffReview,
    Completion,
    Publication,
    Terminal,
}

pub fn resolve_next_phase(graph: &ExecutionGraph) -> LifecyclePhase {
    if graph.nodes().any(|node| {
        node.required
            && node.kind.is_mutation()
            && !matches!(
                node.status,
                ExecutionNodeStatus::Completed | ExecutionNodeStatus::Skipped
            )
            && !(node.status == ExecutionNodeStatus::Applied
                && node.repository_mutation_lifecycle.is_none())
    }) {
        return LifecyclePhase::Implementation;
    }
    if !graph.implementation_barrier_satisfied() {
        return LifecyclePhase::ImplementationBarrier;
    }
    if graph
        .nodes()
        .any(|node| node.required && node.kind.is_validation() && !node.status.is_success())
    {
        return LifecyclePhase::Validation;
    }
    if graph.nodes().any(|node| {
        node.required && node.kind == ExecutionNodeKind::DiffReview && !node.status.is_success()
    }) {
        return LifecyclePhase::DiffReview;
    }
    if graph.nodes().any(|node| {
        node.required
            && node.kind == ExecutionNodeKind::CompletionEvaluation
            && !node.status.is_success()
    }) {
        return LifecyclePhase::Completion;
    }
    if graph.nodes().any(|node| {
        node.required && node.kind == ExecutionNodeKind::Publication && !node.status.is_success()
    }) {
        return LifecyclePhase::Publication;
    }
    LifecyclePhase::Terminal
}

#[cfg(test)]
mod lifecycle_invariant_tests {
    use super::*;

    fn target(path: &str, role: &str) -> PlannedTarget {
        PlannedTarget {
            change_id: format!("change-{path}"),
            path: path.into(),
            role: role.into(),
            intent: format!("update {path}"),
            ..PlannedTarget::default()
        }
    }

    fn gate() -> ValidationGateSpec {
        ValidationGateSpec {
            gate_id: "focused".into(),
            gate_type: ValidationGateType::FocusedTest,
            command: "cargo test focused".into(),
            working_directory: ".".into(),
            required: true,
            ..ValidationGateSpec::default()
        }
    }

    fn snapshot_with_targets(targets: Vec<PlannedTarget>) -> ExecutionSnapshot {
        ExecutionSnapshot {
            run_id: "lifecycle-run".into(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-0".into(),
                source_tree_hash: "tree-0".into(),
                ..RepositorySnapshot::default()
            },
            graph: ExecutionGraph::from_targets(
                "lifecycle-graph",
                MissionComplexity::Small,
                "tree-0",
                &targets,
                &[gate()],
                &MissionBudget::for_complexity(MissionComplexity::Small),
            ),
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        }
    }

    fn complete_next_mutation(
        snapshot: &mut ExecutionSnapshot,
        fingerprint: &str,
        satisfied_intent: SatisfiedIntent,
    ) -> ExecutionNodeId {
        let node = snapshot
            .graph
            .next_runnable_node()
            .filter(|node| node.kind.is_mutation())
            .expect("next mutation")
            .clone();
        let sequence = snapshot.next_event_sequence();
        snapshot
            .append_event(ExecutionDomainEvent::NodeStarted {
                sequence,
                node_id: node.id.clone(),
                attempt: 1,
                started_at: format!("started-{}", node.id),
                repository_fingerprint: snapshot.current_repository.fingerprint.clone(),
            })
            .unwrap();
        snapshot
            .append_event(ExecutionDomainEvent::MutationApplied {
                sequence: sequence + 1,
                node_id: node.id.clone(),
                target_path: node.target.as_ref().unwrap().path.clone(),
                repository_fingerprint: fingerprint.into(),
                evidence_id: format!("operation-evidence-{}", node.id),
                completed_at: format!("completed-{}", node.id),
                satisfied_intent,
                repair_failure_id: None,
                created_target_evidence: None,
            })
            .unwrap();
        snapshot.current_repository.fingerprint = fingerprint.into();
        snapshot.current_repository.source_tree_hash = fingerprint.into();
        node.id
    }

    #[test]
    fn invariant_registry_rejects_future_evidence_dependencies() {
        let invalid = LifecycleInvariantDefinition {
            id: "invalid-future-validation",
            scope: InvariantScope::RepositoryOperationReduction,
            required_evidence: vec![EvidenceKind::ValidationGateResult],
            required_node_kinds: vec![ExecutionNodeKind::SourceMutation],
        };
        let error = validate_lifecycle_invariant_definitions(&[invalid]).unwrap_err();
        assert_eq!(error.code, "invariant_requires_future_evidence");
        validate_lifecycle_invariant_definitions(&lifecycle_invariant_definitions()).unwrap();
    }

    #[test]
    fn multiple_mutations_complete_in_order_without_validation_evidence() {
        let mut snapshot = snapshot_with_targets(vec![
            target("src/a.rs", "source"),
            target("src/b.rs", "source"),
            target("src/c.rs", "source"),
            target("tests/a.rs", "test"),
        ]);
        for (index, expected_path) in
            ["src/a.rs", "src/b.rs", "src/c.rs", "tests/a.rs"]
                .into_iter()
                .enumerate()
        {
            assert!(snapshot.evidence.validations.is_empty());
            let completed = complete_next_mutation(
                &mut snapshot,
                &format!("tree-{}", index + 1),
                SatisfiedIntent::OriginalImplementation,
            );
            assert_eq!(
                snapshot.graph.node(&completed).unwrap().target.as_ref().unwrap().path,
                expected_path
            );
            check_invariants(
                &snapshot.graph,
                LifecycleState::RepositoryOperationReduction,
                InvariantTrigger::RepositoryOperationReduced,
            )
            .unwrap();
            assert_eq!(
                resolve_next_phase(&snapshot.graph),
                if index == 3 {
                    LifecyclePhase::Validation
                } else {
                    LifecyclePhase::Implementation
                }
            );
        }
        let barrier = snapshot.graph.implementation_barrier_proof(
            snapshot.current_repository.fingerprint.clone().into(),
        );
        assert!(barrier.satisfied);
        assert_eq!(barrier.completed_nodes, barrier.required_nodes);
        assert!(barrier.unresolved_nodes.is_empty());
    }

    #[test]
    fn fallback_and_already_applied_completion_continue_to_the_next_target() {
        let mut snapshot = snapshot_with_targets(vec![
            target("src/a.rs", "source"),
            target("src/b.rs", "source"),
        ]);
        let first = complete_next_mutation(
            &mut snapshot,
            "tree-fallback",
            SatisfiedIntent::MutationFallback,
        );
        assert_eq!(snapshot.graph.node(&first).unwrap().status, ExecutionNodeStatus::Completed);
        assert!(snapshot.evidence.validations.is_empty());
        assert_eq!(resolve_next_phase(&snapshot.graph), LifecyclePhase::Implementation);

        let second = snapshot.graph.next_runnable_node().unwrap().clone();
        let sequence = snapshot.next_event_sequence();
        snapshot
            .append_event(ExecutionDomainEvent::NodeStarted {
                sequence,
                node_id: second.id.clone(),
                attempt: 1,
                started_at: "already-started".into(),
                repository_fingerprint: "tree-fallback".into(),
            })
            .unwrap();
        let transition = AlreadyAppliedTransition {
            node_id: second.id.clone(),
            operation: TargetOperation::ModifyExisting,
            target_path: second.target.as_ref().unwrap().path.clone(),
            repository_fingerprint: RepositoryFingerprint::new("tree-fallback"),
            completed_at: "already-completed".into(),
            ..AlreadyAppliedTransition::default()
        };
        let semantic_id = transition.semantic_id(&snapshot.run_id, 1);
        snapshot
            .append_event(ExecutionDomainEvent::TargetOperationAlreadyApplied {
                sequence: sequence + 1,
                execution_id: snapshot.run_id.clone(),
                attempt: 1,
                transition,
                semantic_id,
                satisfied_intent: SatisfiedIntent::OriginalImplementation,
                repair_failure_id: None,
            })
            .unwrap();
        check_invariants(
            &snapshot.graph,
            LifecycleState::RepositoryOperationReduction,
            InvariantTrigger::RepositoryOperationReduced,
        )
        .unwrap();
        assert_eq!(snapshot.graph.node(&second.id).unwrap().status, ExecutionNodeStatus::Completed);
        assert_eq!(resolve_next_phase(&snapshot.graph), LifecyclePhase::Validation);
    }

    #[test]
    fn resume_preserves_completed_targets_and_selects_the_next_ready_node() {
        let mut snapshot = snapshot_with_targets(vec![
            target("src/a.rs", "source"),
            target("src/b.rs", "source"),
            target("src/c.rs", "source"),
        ]);
        let completed = complete_next_mutation(
            &mut snapshot,
            "tree-after-a",
            SatisfiedIntent::OriginalImplementation,
        );
        let next = snapshot.graph.next_runnable_node().unwrap();
        assert_eq!(
            snapshot.graph.node(&completed).unwrap().status,
            ExecutionNodeStatus::Completed
        );
        assert_eq!(next.target.as_ref().unwrap().path, "src/b.rs");
        assert_eq!(
            snapshot
                .graph
                .nodes()
                .find(|node| node.target.as_ref().is_some_and(|target| target.path == "src/c.rs"))
                .unwrap()
                .status,
            ExecutionNodeStatus::Pending
        );
    }

    #[test]
    fn broken_verified_reduction_uses_the_specific_operation_evidence_code() {
        let mut snapshot = snapshot_with_targets(vec![target("src/a.rs", "source")]);
        let completed = complete_next_mutation(
            &mut snapshot,
            "tree-after-a",
            SatisfiedIntent::OriginalImplementation,
        );
        snapshot
            .graph
            .node_mut(&completed)
            .unwrap()
            .operation_evidence
            .clear();
        let error = check_invariants(
            &snapshot.graph,
            LifecycleState::RepositoryOperationReduction,
            InvariantTrigger::RepositoryOperationReduced,
        )
        .unwrap_err();
        assert_eq!(error.code, "verified_operation_missing_operation_evidence");
    }

    #[test]
    fn current_validation_is_required_only_after_the_barrier_and_becomes_stale() {
        let mut snapshot = snapshot_with_targets(vec![target("src/a.rs", "source")]);
        complete_next_mutation(
            &mut snapshot,
            "tree-complete",
            SatisfiedIntent::OriginalImplementation,
        );
        check_snapshot_invariants(
            &snapshot,
            LifecycleState::Validation,
            InvariantTrigger::PhaseTransition,
        )
        .unwrap();
        let missing = check_snapshot_invariants(
            &snapshot,
            LifecycleState::Completion,
            InvariantTrigger::CompletionDecision,
        )
        .unwrap_err();
        assert_eq!(missing.code, "current_validation_missing_at_completion");

        let validation_id = snapshot
            .graph
            .nodes()
            .find(|node| node.kind.is_validation())
            .unwrap()
            .id
            .clone();
        let gate = snapshot.graph.node(&validation_id).unwrap().validation.clone().unwrap();
        let fingerprint = gate.fingerprint("tree-complete");
        snapshot.evidence.record_validation(ValidationEvidenceRecord {
            evidence_id: "validation-current".into(),
            node_id: validation_id.clone(),
            gate_id: gate.gate_id,
            fingerprint,
            repository_fingerprint: "tree-complete".into(),
            command: gate.command,
            working_directory: gate.working_directory,
            status: ValidationEvidenceStatus::Passed,
            ..ValidationEvidenceRecord::default()
        });
        check_snapshot_invariants(
            &snapshot,
            LifecycleState::Completion,
            InvariantTrigger::CompletionDecision,
        )
        .unwrap();

        snapshot.current_repository.fingerprint = "tree-after-repair".into();
        snapshot.current_repository.source_tree_hash = "tree-after-repair".into();
        assert_eq!(snapshot.evidence.supersede_stale_validation("tree-after-repair"), 1);
        let stale = check_snapshot_invariants(
            &snapshot,
            LifecycleState::Completion,
            InvariantTrigger::CompletionDecision,
        )
        .unwrap_err();
        assert_eq!(stale.code, "current_validation_missing_at_completion");
    }

    fn snapshot_with_active_validation_repair() -> (
        ExecutionSnapshot,
        ExecutionNodeId,
        ExecutionNodeId,
        FailureId,
    ) {
        snapshot_with_active_validation_repair_after(SatisfiedIntent::OriginalImplementation)
    }

    fn snapshot_with_active_validation_repair_after(
        implementation_intent: SatisfiedIntent,
    ) -> (
        ExecutionSnapshot,
        ExecutionNodeId,
        ExecutionNodeId,
        FailureId,
    ) {
        let mut snapshot = snapshot_with_targets(vec![target("tests/behavior.rs", "test")]);
        let implementation_node = complete_next_mutation(
            &mut snapshot,
            "tree-implemented",
            implementation_intent,
        );
        let validation_node = snapshot
            .graph
            .nodes()
            .find(|node| node.kind.is_validation())
            .unwrap()
            .id
            .clone();
        let validation_fingerprint = snapshot
            .graph
            .node(&validation_node)
            .unwrap()
            .validation
            .as_ref()
            .unwrap()
            .fingerprint("tree-implemented");
        let failure_id = FailureId::new("focused-validation-failure");
        let failure = FailureRecord {
            id: failure_id.clone(),
            node_id: validation_node.clone(),
            category: FailureCategory::ValidationFailure,
            attempt: 1,
            target_path: Some("tests/behavior.rs".into()),
            repository_fingerprint: "tree-implemented".into(),
            validation_command: Some("cargo test focused".into()),
            message: "focused validation failed".into(),
            ..FailureRecord::default()
        };
        let sequence = snapshot.next_event_sequence();
        snapshot
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence,
                failure: failure.clone(),
            })
            .unwrap();
        snapshot
            .append_event(ExecutionDomainEvent::ValidationEvidenceRecorded {
                sequence: sequence + 1,
                node_id: validation_node.clone(),
                evidence: ValidationEvidenceRecord {
                    evidence_id: "focused-validation-failed-evidence".into(),
                    node_id: validation_node.clone(),
                    gate_id: "focused".into(),
                    fingerprint: validation_fingerprint.clone(),
                    repository_fingerprint: "tree-implemented".into(),
                    command: "cargo test focused".into(),
                    working_directory: ".".into(),
                    status: ValidationEvidenceStatus::Failed,
                    ..ValidationEvidenceRecord::default()
                },
            })
            .unwrap();
        snapshot
            .append_event(ExecutionDomainEvent::ValidationFailed {
                sequence: sequence + 2,
                node_id: validation_node.clone(),
                failure_id: failure_id.clone(),
                fingerprint: validation_fingerprint,
            })
            .unwrap();
        let revision = snapshot
            .budget
            .current_validation_failure_revision(validation_node.as_ref(), "tree-implemented")
            .unwrap()
            .revision;
        let implementation = snapshot.graph.node(&implementation_node).unwrap();
        let planned_target = implementation.target.as_ref().unwrap();
        let repair_node_id = validation_repair_node_id(&failure_id, revision, planned_target);
        snapshot
            .append_event(ExecutionDomainEvent::ValidationRepairStarted {
                sequence: sequence + 3,
                validation_node_id: validation_node.clone(),
                failure_id: failure_id.clone(),
                repair_node_id: repair_node_id.clone(),
                originating_implementation_node_id: implementation_node.clone(),
                target_ref: RepositoryTargetRef {
                    target_id: planned_target.mutation_target_id().to_string(),
                    path: planned_target.path.clone(),
                },
                failure_revision: revision,
                repair_intent: ValidationRepairIntent {
                    repair_intent_id: "repair-focused-r1".into(),
                    failed_validation_id: failure_id.to_string(),
                    target: planned_target.path.clone(),
                    ..ValidationRepairIntent::default()
                },
                selected_target: planned_target.path.clone(),
                implicated_paths: vec![planned_target.path.clone()],
                correction_contracts: Vec::new(),
                requested_tool_policy: MutationFallbackPolicy::ForceReplaceFile,
                repository_fingerprint_before: RepositoryFingerprint::new("tree-implemented"),
            })
            .unwrap();
        (snapshot, implementation_node, repair_node_id, failure_id)
    }

    #[test]
    fn completed_implementation_cannot_reopen_and_repair_uses_a_separate_attempt_ledger() {
        let (mut snapshot, implementation_node, repair_node_id, _) =
            snapshot_with_active_validation_repair();
        let implementation_before = snapshot.graph.node(&implementation_node).unwrap().clone();
        let repair = snapshot.graph.node(&repair_node_id).unwrap();
        assert_eq!(implementation_before.status, ExecutionNodeStatus::Completed);
        assert_eq!(implementation_before.attempts.len(), 1);
        assert_eq!(repair.kind, ExecutionNodeKind::ValidationRepair);
        assert_eq!(repair.status, ExecutionNodeStatus::Running);
        assert_eq!(repair.attempts.len(), 1);
        assert_eq!(repair.dependencies, vec![implementation_node.clone()]);
        assert_eq!(
            snapshot
                .target_execution_state(&implementation_node)
                .unwrap()
                .repair_status,
            RepairStatus::Running
        );
        assert!(snapshot
            .graph
            .implementation_barrier_proof("tree-implemented".into())
            .satisfied);

        let before_rejected_transition = snapshot.clone();
        let error = snapshot
            .append_event(ExecutionDomainEvent::NodeStarted {
                sequence: snapshot.next_event_sequence(),
                node_id: implementation_node.clone(),
                attempt: 2,
                started_at: "invalid-repair-start".into(),
                repository_fingerprint: "tree-implemented".into(),
            })
            .unwrap_err();
        assert_eq!(error.code, "completed_implementation_node_reopened");
        assert_eq!(snapshot, before_rejected_transition);
        assert_eq!(
            snapshot.graph.node(&implementation_node).unwrap(),
            &implementation_before
        );
    }

    #[test]
    fn completed_source_and_test_implementation_statuses_are_monotonic() {
        for (path, role) in [("src/behavior.rs", "source"), ("tests/behavior.rs", "test")] {
            let mut snapshot = snapshot_with_targets(vec![target(path, role)]);
            let implementation_node = complete_next_mutation(
                &mut snapshot,
                "tree-implemented",
                SatisfiedIntent::OriginalImplementation,
            );
            let before = snapshot.clone();
            let error = snapshot
                .append_event(ExecutionDomainEvent::NodeStarted {
                    sequence: snapshot.next_event_sequence(),
                    node_id: implementation_node.clone(),
                    attempt: 2,
                    started_at: "invalid-later-repair".into(),
                    repository_fingerprint: "tree-implemented".into(),
                })
                .unwrap_err();
            assert_eq!(error.code, "completed_implementation_node_reopened");
            assert_eq!(snapshot, before);
            assert_eq!(
                snapshot.graph.node(&implementation_node).unwrap().status,
                ExecutionNodeStatus::Completed
            );
        }
    }

    #[test]
    fn failed_validation_repair_blocks_only_the_repair_node() {
        let (mut snapshot, implementation_node, repair_node_id, failure_id) =
            snapshot_with_active_validation_repair();
        let implementation_before = snapshot.graph.node(&implementation_node).unwrap().clone();
        snapshot
            .append_event(ExecutionDomainEvent::ValidationRepairCompleted {
                sequence: snapshot.next_event_sequence(),
                validation_node_id: snapshot
                    .failures
                    .get(&failure_id)
                    .unwrap()
                    .node_id
                    .clone(),
                failure_id,
                result: RepairResult::NoMutation {
                    diagnosis: None,
                    reason: "no safe target-bound correction".into(),
                    outcome: ValidationRepairMutationOutcome::NoValidRepair,
                    unresolved: None,
                },
                attempt: Some(ValidationRepairAttempt {
                    repair_intent_id: "repair-focused-r1".into(),
                    target_path: "tests/behavior.rs".into(),
                    outcome: ValidationRepairMutationOutcome::NoValidRepair,
                    repository_fingerprint_before: "tree-implemented".into(),
                    repository_fingerprint_after: "tree-implemented".into(),
                    ..ValidationRepairAttempt::default()
                }),
            })
            .unwrap();

        assert_eq!(
            snapshot.graph.node(&implementation_node).unwrap(),
            &implementation_before
        );
        let repair = snapshot.graph.node(&repair_node_id).unwrap();
        assert_eq!(repair.status, ExecutionNodeStatus::FailedBlocking);
        assert_eq!(
            repair.validation_repair.as_ref().unwrap().status,
            RepairNodeStatus::Blocked
        );
    }

    #[test]
    fn implementation_fallback_completion_admits_an_independent_validation_repair() {
        let (snapshot, implementation_node, repair_node_id, _) =
            snapshot_with_active_validation_repair_after(SatisfiedIntent::MutationFallback);
        let implementation = snapshot.graph.node(&implementation_node).unwrap();
        let repair = snapshot.graph.node(&repair_node_id).unwrap();

        assert_eq!(implementation.status, ExecutionNodeStatus::Completed);
        assert_eq!(implementation.attempts.len(), 1);
        assert_eq!(implementation.operation_evidence.len(), 1);
        assert_eq!(repair.kind, ExecutionNodeKind::ValidationRepair);
        assert_eq!(repair.status, ExecutionNodeStatus::Running);
        assert_eq!(repair.attempts.len(), 1);
        assert!(repair.operation_evidence.is_empty());
    }

    #[test]
    fn repair_operation_preserves_implementation_evidence_and_stales_validation_only() {
        let (mut snapshot, implementation_node, repair_node_id, failure_id) =
            snapshot_with_active_validation_repair();
        let implementation_before = snapshot.graph.node(&implementation_node).unwrap().clone();
        snapshot.evidence.record_validation(ValidationEvidenceRecord {
            evidence_id: "old-validation".into(),
            node_id: snapshot
                .graph
                .nodes()
                .find(|node| node.kind.is_validation())
                .unwrap()
                .id
                .clone(),
            repository_fingerprint: "tree-implemented".into(),
            fingerprint: "focused:tree-implemented".into(),
            status: ValidationEvidenceStatus::Passed,
            ..ValidationEvidenceRecord::default()
        });
        let sequence = snapshot.next_event_sequence();
        snapshot
            .append_event(ExecutionDomainEvent::MutationApplied {
                sequence,
                node_id: repair_node_id.clone(),
                target_path: "tests/behavior.rs".into(),
                repository_fingerprint: "tree-repaired".into(),
                evidence_id: "repair-operation-evidence".into(),
                completed_at: "repair-completed".into(),
                satisfied_intent: SatisfiedIntent::ValidationRepair,
                repair_failure_id: Some(failure_id.clone()),
                created_target_evidence: None,
            })
            .unwrap();
        snapshot
            .append_event(ExecutionDomainEvent::ValidationRepairCompleted {
                sequence: sequence + 1,
                validation_node_id: snapshot
                    .failures
                    .get(&failure_id)
                    .unwrap()
                    .node_id
                    .clone(),
                failure_id,
                result: RepairResult::MutationProduced {
                    selected_target: "tests/behavior.rs".into(),
                    repair_intent_id: "repair-focused-r1".into(),
                },
                attempt: Some(ValidationRepairAttempt {
                    repair_intent_id: "repair-focused-r1".into(),
                    target_path: "tests/behavior.rs".into(),
                    failure_revision: 1,
                    outcome: ValidationRepairMutationOutcome::MutationApplied,
                    repository_fingerprint_before: "tree-implemented".into(),
                    repository_fingerprint_after: "tree-repaired".into(),
                    ..ValidationRepairAttempt::default()
                }),
            })
            .unwrap();

        assert_eq!(
            snapshot.graph.node(&implementation_node).unwrap(),
            &implementation_before
        );
        let repair = snapshot.graph.node(&repair_node_id).unwrap();
        assert_eq!(repair.status, ExecutionNodeStatus::Completed);
        assert!(repair.operation_evidence.is_empty());
        assert_eq!(repair.validation_repair_operation_evidence.len(), 1);
        assert!(snapshot
            .graph
            .implementation_barrier_proof("tree-repaired".into())
            .satisfied);
        assert_eq!(
            snapshot.evidence.validations["old-validation"].status,
            ValidationEvidenceStatus::Superseded
        );
        assert!(matches!(
            snapshot.target_revisions.last().map(|revision| &revision.producer),
            Some(TargetRevisionProducer::ValidationRepair(node_id)) if node_id == &repair_node_id
        ));
    }
}
