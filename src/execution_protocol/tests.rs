use super::*;

mod phase2_discovery;
mod phase2_edges;
mod phase2_profile;
mod phase2_relationship;
mod phase3_graph_budget;
mod phase3_planning;
mod phase3_planning_hardening;
mod phase4_implementation_context;
mod phase5_mutation;
mod phase6_validation;
mod phase7_review_publication;
mod phase8_contract_freeze;

const EXECUTION_ID: &str = "execution-protocol-v1:test-execution";
const REPOSITORY_REVISION: &str = "repository-revision:test-0";

#[derive(Clone, Debug)]
struct PlanNodes {
    implementation: Vec<NodeId>,
    validation: NodeId,
    review: NodeId,
    completion: NodeId,
    publication: NodeId,
}

fn model_budget(max_model_calls: u32) -> NodeBudgetContract {
    NodeBudgetContract {
        max_model_calls,
        max_cost_micros: 10_000,
        max_duration_ms: 10_000,
        max_mutation_attempts: 3,
        max_context_rebuilds: 2,
        max_input_tokens_per_call: 4_096,
        max_output_tokens_per_call: 2_048,
    }
}

fn mission_budget(max_model_calls: u32) -> MissionBudgetContract {
    MissionBudgetContract {
        max_model_calls,
        max_cost_micros: 100_000,
        max_duration_ms: 100_000,
    }
}

fn plan_graph_budget() -> PlanGraphBudgetContract {
    PlanGraphBudgetContract {
        max_implementation_nodes: 32,
        max_validation_nodes: 16,
        max_total_nodes: 51,
        implementation: model_budget(3),
        validation: NodeBudgetContract::deterministic(),
        review: model_budget(1),
        completion_evaluation: model_budget(1),
        publication: NodeBudgetContract::deterministic(),
    }
}

fn bootstrap(discovery_model_calls: u32, mission_model_calls: u32) -> ExecutionState {
    ExecutionState::bootstrap(
        ExecutionId::new(EXECUTION_ID),
        1,
        RepositoryRevisionId::new(REPOSITORY_REVISION),
        mission_budget(mission_model_calls),
        model_budget(discovery_model_calls),
        model_budget(3),
        plan_graph_budget(),
        None,
    )
}

fn envelope(
    state: &ExecutionState,
    semantic_key: &str,
    payload: impl Into<DomainEvent>,
) -> ProtocolEventEnvelope {
    ProtocolEventEnvelope::new_legacy_test_compatible(
        state,
        semantic_key,
        state.next_sequence().saturating_mul(10),
        payload,
    )
    .expect("valid semantic event identity")
}

fn append(
    state: &mut ExecutionState,
    semantic_key: &str,
    payload: impl Into<DomainEvent>,
) -> ProtocolEventEnvelope {
    let event = envelope(state, semantic_key, payload);
    assert_eq!(
        state
            .append_event(event.clone())
            .expect("valid protocol event"),
        AppendOutcome::Applied {
            revision: state.aggregate_revision,
        }
    );
    event
}

fn proof(
    state: &mut ExecutionState,
    name: &str,
    kind: ProofKind,
    node_ids: Vec<NodeId>,
    related_proof_ids: Vec<ProofId>,
) -> ProofId {
    let proof_id = ProofId::new(format!("proof:{name}"));
    let proof = ProofRecord {
        id: proof_id.clone(),
        kind,
        repository_revision: state.repository_revision.clone(),
        node_ids,
        related_proof_ids,
        related_evidence_ids: Vec::new(),
        detail_hash: stable_sha256(&["execution-protocol-v1:test-proof", name]),
    };
    append(
        state,
        &format!("proof:{name}"),
        EvidenceEvent::ProofRecorded { proof },
    );
    proof_id
}

fn advance(state: &mut ExecutionState, from: ProtocolStage, to: ProtocolStage, proof_id: &ProofId) {
    append(
        state,
        &format!("advance:{from:?}:{to:?}:{}", proof_id.as_str()),
        LifecycleEvent::PositionAdvanced {
            from,
            to,
            proof_id: proof_id.clone(),
        },
    );
}

fn start(state: &mut ExecutionState, node_id: &NodeId, attempt: u32, key: &str) {
    let event: DomainEvent = GraphEvent::NodeStarted {
        node_id: node_id.clone(),
        attempt,
    }
    .into();
    assert_eq!(
        decide(state).expect("node-start decision"),
        ProtocolDecision::Emit {
            event: event.clone(),
        }
    );
    append(state, key, event);
}

fn succeed(state: &mut ExecutionState, node_id: &NodeId, proof_id: &ProofId, key: &str) {
    append(
        state,
        key,
        GraphEvent::NodeSucceeded {
            node_id: node_id.clone(),
            proof_id: proof_id.clone(),
        },
    );
}

fn enter_discovery(state: &mut ExecutionState) -> ProofId {
    let profile = proof(
        state,
        "repository-profile",
        ProofKind::RepositoryProfile,
        Vec::new(),
        Vec::new(),
    );
    advance(
        state,
        ProtocolStage::Profiling,
        ProtocolStage::Discovery,
        &profile,
    );
    profile
}

fn complete_discovery(state: &mut ExecutionState) -> ProofId {
    let discovery = NodeId::new("protocol-v1:discovery");
    start(state, &discovery, 1, "discovery:start");
    let impact_map = proof(
        state,
        "discovery-impact-map",
        ProofKind::DiscoveryImpactMap,
        vec![discovery.clone()],
        Vec::new(),
    );
    succeed(state, &discovery, &impact_map, "discovery:succeeded");
    advance(
        state,
        ProtocolStage::Discovery,
        ProtocolStage::Planning,
        &impact_map,
    );
    impact_map
}

fn standard_plan(implementation_count: usize) -> (PlanNodes, Vec<NodeSpec>) {
    let implementation = (0..implementation_count)
        .map(|index| NodeId::new(format!("implementation:{index}")))
        .collect::<Vec<_>>();
    let validation = NodeId::new("validation:focused");
    let review = NodeId::new("review:diff");
    let completion = NodeId::new("review:completion");
    let publication = NodeId::new("publication:pull-request");

    let mut specs = implementation
        .iter()
        .cloned()
        .map(|id| NodeSpec {
            id,
            kind: NodeKind::Implementation,
            required: true,
            dependencies: Vec::new(),
            budget: model_budget(3),
        })
        .collect::<Vec<_>>();
    specs.extend([
        NodeSpec {
            id: validation.clone(),
            kind: NodeKind::Validation,
            required: true,
            dependencies: implementation.clone(),
            budget: NodeBudgetContract::deterministic(),
        },
        NodeSpec {
            id: review.clone(),
            kind: NodeKind::Review,
            required: true,
            dependencies: vec![validation.clone()],
            budget: model_budget(2),
        },
        NodeSpec {
            id: completion.clone(),
            kind: NodeKind::CompletionEvaluation,
            required: true,
            dependencies: vec![review.clone()],
            budget: model_budget(2),
        },
        NodeSpec {
            id: publication.clone(),
            kind: NodeKind::Publication,
            required: true,
            dependencies: vec![completion.clone()],
            budget: NodeBudgetContract::deterministic(),
        },
    ]);

    (
        PlanNodes {
            implementation,
            validation,
            review,
            completion,
            publication,
        },
        specs,
    )
}

fn active_planning_with_plan(state: &mut ExecutionState) -> ProofId {
    enter_discovery(state);
    complete_discovery(state);

    let planning = NodeId::new("protocol-v1:planning");
    start(state, &planning, 1, "planning:start");
    proof(
        state,
        "accepted-plan",
        ProofKind::PlanAccepted,
        vec![planning],
        Vec::new(),
    )
}

fn enter_implementation(
    state: &mut ExecutionState,
    implementation_count: usize,
) -> (PlanNodes, ProofId) {
    let plan = active_planning_with_plan(state);
    let planning = NodeId::new("protocol-v1:planning");
    let (nodes, specs) = standard_plan(implementation_count);
    append(
        state,
        "planning:materialize-graph",
        GraphEvent::NodesAdded {
            plan_proof_id: plan.clone(),
            nodes: specs,
        },
    );
    succeed(state, &planning, &plan, "planning:succeeded");
    advance(
        state,
        ProtocolStage::Planning,
        ProtocolStage::Implementation,
        &plan,
    );
    (nodes, plan)
}

fn complete_implementation_node(
    state: &mut ExecutionState,
    node_id: &NodeId,
    index: usize,
) -> ProofId {
    start(state, node_id, 1, &format!("implementation:{index}:start"));
    let verified = proof(
        state,
        &format!("implementation-{index}-verified"),
        ProofKind::MutationVerified,
        vec![node_id.clone()],
        Vec::new(),
    );
    succeed(
        state,
        node_id,
        &verified,
        &format!("implementation:{index}:succeeded"),
    );
    verified
}

fn complete_implementation_and_enter_validation(
    state: &mut ExecutionState,
    nodes: &PlanNodes,
) -> ProofId {
    let implementation_proofs = nodes
        .implementation
        .iter()
        .enumerate()
        .map(|(index, node_id)| complete_implementation_node(state, node_id, index))
        .collect::<Vec<_>>();
    let barrier = proof(
        state,
        "implementation-barrier",
        ProofKind::ImplementationBarrier,
        nodes.implementation.clone(),
        implementation_proofs,
    );
    advance(
        state,
        ProtocolStage::Implementation,
        ProtocolStage::Validation,
        &barrier,
    );
    barrier
}

fn complete_validation_and_enter_review(state: &mut ExecutionState, nodes: &PlanNodes) -> ProofId {
    start(state, &nodes.validation, 1, "validation:start");
    let passed = proof(
        state,
        "validation-passed",
        ProofKind::ValidationPassed,
        vec![nodes.validation.clone()],
        Vec::new(),
    );
    succeed(state, &nodes.validation, &passed, "validation:succeeded");
    let required = proof(
        state,
        "required-validation-passed",
        ProofKind::RequiredValidationPassed,
        vec![nodes.validation.clone()],
        vec![passed],
    );
    advance(
        state,
        ProtocolStage::Validation,
        ProtocolStage::Review,
        &required,
    );
    required
}

fn complete_review_and_enter_publication(state: &mut ExecutionState, nodes: &PlanNodes) -> ProofId {
    start(state, &nodes.review, 1, "review:start");
    let review = proof(
        state,
        "review-completed",
        ProofKind::ReviewCompleted,
        vec![nodes.review.clone()],
        Vec::new(),
    );
    succeed(state, &nodes.review, &review, "review:succeeded");

    start(state, &nodes.completion, 1, "completion:start");
    let completion = proof(
        state,
        "completion-evaluated",
        ProofKind::CompletionEvaluated,
        vec![nodes.completion.clone()],
        vec![review],
    );
    succeed(
        state,
        &nodes.completion,
        &completion,
        "completion:succeeded",
    );

    let eligibility = proof(
        state,
        "publication-eligibility",
        ProofKind::PublicationEligibility,
        Vec::new(),
        vec![completion],
    );
    advance(
        state,
        ProtocolStage::Review,
        ProtocolStage::Publication,
        &eligibility,
    );
    eligibility
}

fn successful_execution_with_implementation_count(implementation_count: usize) -> ExecutionState {
    let mut state = bootstrap(3, 20);
    let (nodes, _) = enter_implementation(&mut state, implementation_count);
    complete_implementation_and_enter_validation(&mut state, &nodes);
    complete_validation_and_enter_review(&mut state, &nodes);
    complete_review_and_enter_publication(&mut state, &nodes);

    start(&mut state, &nodes.publication, 1, "publication:start");
    let publication = proof(
        &mut state,
        "publication-completed",
        ProofKind::PublicationCompleted,
        vec![nodes.publication.clone()],
        Vec::new(),
    );
    succeed(
        &mut state,
        &nodes.publication,
        &publication,
        "publication:succeeded",
    );
    let result = CanonicalResult {
        mission: MissionResult::Succeeded {
            publication_proof_id: publication,
        },
        process_health: ProcessHealth::Healthy,
        reason_code: "pull_request_created".into(),
        repository_revision: state.repository_revision.clone(),
        remaining_work: Vec::new(),
    };
    append(
        &mut state,
        "terminal:succeeded",
        TerminalEvent::CanonicalResultRecorded { result },
    );
    state
}

fn successful_execution() -> ExecutionState {
    successful_execution_with_implementation_count(2)
}

#[test]
fn legal_protocol_path_reaches_a_proof_carrying_terminal_result() {
    let state = successful_execution();

    assert_eq!(state.stage(), ProtocolStage::Terminal);
    let result = state.terminal.as_ref().expect("canonical result");
    assert_eq!(result.mission.outcome(), MissionOutcomeV1::Succeeded);
    assert_eq!(result.process_health, ProcessHealth::Healthy);
    assert!(result.remaining_work.is_empty());
    assert!(matches!(
        decide(&state).expect("terminal decision"),
        ProtocolDecision::Finish { result: decided } if decided == *result
    ));
    validate_state(&state).expect("terminal state satisfies protocol invariants");
}

#[test]
fn rejected_transition_is_atomic_and_pure_reduce_does_not_mutate_input() {
    let state = bootstrap(2, 10);
    let original = state.clone();
    let bogus_proof = ProofId::new("proof:not-recorded");
    let event = envelope(
        &state,
        "illegal:profiling-to-planning",
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Profiling,
            to: ProtocolStage::Planning,
            proof_id: bogus_proof,
        },
    );

    assert_eq!(
        reduce(&state, event),
        Err(ProtocolViolation::IllegalTransition {
            from: ProtocolStage::Profiling,
            to: ProtocolStage::Planning,
        })
    );
    assert_eq!(state, original);

    let mut state = state;
    let missing_proof_event = envelope(
        &state,
        "illegal:missing-profile-proof",
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Profiling,
            to: ProtocolStage::Discovery,
            proof_id: ProofId::new("proof:missing-profile"),
        },
    );
    let snapshot = state.clone();
    assert_eq!(
        state.append_event(missing_proof_event),
        Err(ProtocolViolation::MissingTransitionProof {
            required: ProofKind::RepositoryProfile,
        })
    );
    assert_eq!(state, snapshot);
}

#[test]
fn illegal_stage_transition_table_is_rejected_atomically() {
    let profiling = bootstrap(3, 20);

    let mut discovery = bootstrap(3, 20);
    enter_discovery(&mut discovery);

    let mut planning = bootstrap(3, 20);
    enter_discovery(&mut planning);
    complete_discovery(&mut planning);

    let mut implementation = bootstrap(3, 20);
    enter_implementation(&mut implementation, 1);

    let mut validation = bootstrap(3, 20);
    let (validation_nodes, _) = enter_implementation(&mut validation, 1);
    complete_implementation_and_enter_validation(&mut validation, &validation_nodes);

    let mut review = bootstrap(3, 20);
    let (review_nodes, _) = enter_implementation(&mut review, 1);
    complete_implementation_and_enter_validation(&mut review, &review_nodes);
    complete_validation_and_enter_review(&mut review, &review_nodes);

    let mut publication = bootstrap(3, 20);
    let (publication_nodes, _) = enter_implementation(&mut publication, 1);
    complete_implementation_and_enter_validation(&mut publication, &publication_nodes);
    complete_validation_and_enter_review(&mut publication, &publication_nodes);
    complete_review_and_enter_publication(&mut publication, &publication_nodes);

    let cases = [
        (profiling, ProtocolStage::Planning),
        (discovery, ProtocolStage::Implementation),
        (planning, ProtocolStage::Validation),
        (implementation, ProtocolStage::Review),
        (validation, ProtocolStage::Publication),
        (review, ProtocolStage::Implementation),
        (publication, ProtocolStage::Validation),
    ];

    for (mut state, to) in cases {
        let from = state.stage();
        let snapshot = state.clone();
        let event = envelope(
            &state,
            &format!("illegal:{from:?}:{to:?}"),
            LifecycleEvent::PositionAdvanced {
                from,
                to,
                proof_id: ProofId::new("proof:not-recorded"),
            },
        );
        assert_eq!(
            state.append_event(event),
            Err(ProtocolViolation::IllegalTransition { from, to }),
            "unexpected result for {from:?} -> {to:?}"
        );
        assert_eq!(state, snapshot, "rejection mutated {from:?} state");
    }
}

#[test]
fn event_replay_is_exact_and_conflicting_identity_or_revision_is_rejected() {
    let mut state = bootstrap(2, 10);
    let recorded = envelope(
        &state,
        "proof:repository-profile",
        EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                id: ProofId::new("proof:repository-profile"),
                kind: ProofKind::RepositoryProfile,
                repository_revision: state.repository_revision.clone(),
                node_ids: Vec::new(),
                related_proof_ids: Vec::new(),
                related_evidence_ids: Vec::new(),
                detail_hash: "profile-detail".into(),
            },
        },
    );
    let stale = envelope(
        &state,
        "event:built-at-stale-revision",
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Profiling,
            to: ProtocolStage::Discovery,
            proof_id: ProofId::new("proof:repository-profile"),
        },
    );
    assert_eq!(
        state
            .append_event(recorded.clone())
            .expect("initial append"),
        AppendOutcome::Applied { revision: 1 }
    );

    let after_first_append = state.clone();
    assert_eq!(
        state.append_event(recorded.clone()).expect("exact replay"),
        AppendOutcome::IdempotentReplay { revision: 1 }
    );
    assert_eq!(state, after_first_append);

    let mut conflicting = recorded;
    conflicting.occurred_at_ms = conflicting.occurred_at_ms.saturating_add(1);
    assert!(matches!(
        state.append_event(conflicting),
        Err(ProtocolViolation::EventIdentityConflict { .. })
    ));
    assert_eq!(state, after_first_append);

    assert_eq!(
        state.append_event(stale),
        Err(ProtocolViolation::RevisionConflict {
            expected: 0,
            actual: 1,
        })
    );
    assert_eq!(state, after_first_append);
}

#[test]
fn active_and_waiting_nodes_are_the_single_execution_owner() {
    let mut state = bootstrap(2, 10);
    enter_discovery(&mut state);
    let discovery = NodeId::new("protocol-v1:discovery");
    let planning = NodeId::new("protocol-v1:planning");
    start(&mut state, &discovery, 1, "discovery:start");

    let active_snapshot = state.clone();
    let competing_start = envelope(
        &state,
        "planning:competing-start",
        GraphEvent::NodeStarted {
            node_id: planning.clone(),
            attempt: 1,
        },
    );
    assert_eq!(
        state.append_event(competing_start),
        Err(ProtocolViolation::ActiveOwnerConflict {
            active_node_id: discovery.clone(),
            requested_node_id: planning.clone(),
        })
    );
    assert_eq!(state, active_snapshot);

    let effect_id = EffectId::new("effect:discovery-provider");
    append(
        &mut state,
        "discovery:waiting",
        GraphEvent::NodeWaiting {
            node_id: discovery.clone(),
            effect_id: effect_id.clone(),
        },
    );
    assert!(matches!(
        state.node(&discovery).map(|node| &node.state),
        Some(NodeState::Waiting { effect_id: actual, .. }) if actual == &effect_id
    ));

    let waiting_snapshot = state.clone();
    let competing_start = envelope(
        &state,
        "planning:competing-with-waiting",
        GraphEvent::NodeStarted {
            node_id: planning.clone(),
            attempt: 1,
        },
    );
    assert_eq!(
        state.append_event(competing_start),
        Err(ProtocolViolation::ActiveOwnerConflict {
            active_node_id: discovery.clone(),
            requested_node_id: planning,
        })
    );
    assert_eq!(state, waiting_snapshot);

    append(
        &mut state,
        "discovery:resumed",
        GraphEvent::NodeResumed {
            node_id: discovery.clone(),
            effect_id,
        },
    );
    assert!(matches!(
        state.node(&discovery).map(|node| &node.state),
        Some(NodeState::Active { attempt: 1 })
    ));
    validate_state(&state).expect("single owner remains valid");
}

#[test]
fn waiting_node_requires_closed_calls_and_matching_resume_before_new_work() {
    let discovery = NodeId::new("protocol-v1:discovery");
    let effect_id = EffectId::new("effect:discovery-wait");

    let mut open_call_state = bootstrap(2, 10);
    enter_discovery(&mut open_call_state);
    start(
        &mut open_call_state,
        &discovery,
        1,
        "open-call-discovery:start",
    );
    let open_admission = ModelCallAdmission {
        call_id: ModelCallId::new("model-call:open-before-wait"),
        node_id: discovery.clone(),
        action_id: ActionId::new("action:open-before-wait"),
        payload_hash: "payload:open-before-wait".into(),
        input_tokens: 10,
        output_tokens: 10,
        reserved_cost_micros: 10,
        duration_allowance_ms: 10,
    };
    append(
        &mut open_call_state,
        "model-call:open-before-wait:admitted",
        BudgetEvent::ModelCallAdmitted {
            admission: open_admission,
        },
    );
    let open_call_snapshot = open_call_state.clone();
    let wait_with_open_call = envelope(
        &open_call_state,
        "discovery:wait-with-open-call",
        GraphEvent::NodeWaiting {
            node_id: discovery.clone(),
            effect_id: effect_id.clone(),
        },
    );
    assert_eq!(
        open_call_state.append_event(wait_with_open_call),
        Err(ProtocolViolation::InvalidNodeState {
            node_id: discovery.clone(),
            code: "node_waiting_with_open_model_call",
        })
    );
    assert_eq!(open_call_state, open_call_snapshot);

    let mut waiting = bootstrap(2, 10);
    enter_discovery(&mut waiting);
    start(&mut waiting, &discovery, 1, "waiting-discovery:start");
    append(
        &mut waiting,
        "waiting-discovery:waiting",
        GraphEvent::NodeWaiting {
            node_id: discovery.clone(),
            effect_id: effect_id.clone(),
        },
    );
    let waiting_snapshot = waiting.clone();

    let failure_while_waiting = envelope(
        &waiting,
        "waiting-discovery:failure-before-resume",
        GraphEvent::NodeFailed {
            node_id: discovery.clone(),
            failure_revision_id: FailureRevisionId::new("failure:before-resume"),
            terminal: false,
        },
    );
    assert_eq!(
        waiting.append_event(failure_while_waiting),
        Err(ProtocolViolation::InvalidNodeState {
            node_id: discovery.clone(),
            code: "node_failed",
        })
    );
    assert_eq!(waiting, waiting_snapshot);

    let admission_while_waiting = ModelCallAdmission {
        call_id: ModelCallId::new("model-call:before-resume"),
        node_id: discovery.clone(),
        action_id: ActionId::new("action:before-resume"),
        payload_hash: "payload:before-resume".into(),
        input_tokens: 10,
        output_tokens: 10,
        reserved_cost_micros: 10,
        duration_allowance_ms: 10,
    };
    let admission_event = envelope(
        &waiting,
        "model-call:before-resume:admitted",
        BudgetEvent::ModelCallAdmitted {
            admission: admission_while_waiting,
        },
    );
    assert_eq!(
        waiting.append_event(admission_event),
        Err(ProtocolViolation::InvalidNodeState {
            node_id: discovery.clone(),
            code: "model_call_admission",
        })
    );
    assert_eq!(waiting, waiting_snapshot);

    let wrong_resume = envelope(
        &waiting,
        "waiting-discovery:wrong-resume",
        GraphEvent::NodeResumed {
            node_id: discovery.clone(),
            effect_id: EffectId::new("effect:not-the-owner"),
        },
    );
    assert_eq!(
        waiting.append_event(wrong_resume),
        Err(ProtocolViolation::InvalidNodeState {
            node_id: discovery.clone(),
            code: "effect_identity_mismatch",
        })
    );
    assert_eq!(waiting, waiting_snapshot);

    append(
        &mut waiting,
        "waiting-discovery:matching-resume",
        GraphEvent::NodeResumed {
            node_id: discovery.clone(),
            effect_id,
        },
    );
    let mut resumed_for_failure = waiting.clone();
    append(
        &mut resumed_for_failure,
        "resumed-discovery:failed",
        GraphEvent::NodeFailed {
            node_id: discovery.clone(),
            failure_revision_id: FailureRevisionId::new("failure:after-resume"),
            terminal: false,
        },
    );
    assert!(matches!(
        resumed_for_failure.node(&discovery).map(|node| &node.state),
        Some(NodeState::FailedRecoverable { .. })
    ));

    append(
        &mut waiting,
        "model-call:after-resume:admitted",
        BudgetEvent::ModelCallAdmitted {
            admission: ModelCallAdmission {
                call_id: ModelCallId::new("model-call:after-resume"),
                node_id: discovery.clone(),
                action_id: ActionId::new("action:after-resume"),
                payload_hash: "payload:after-resume".into(),
                input_tokens: 10,
                output_tokens: 10,
                reserved_cost_micros: 10,
                duration_allowance_ms: 10,
            },
        },
    );
    assert!(matches!(
        waiting
            .budgets
            .model_calls
            .get(&ModelCallId::new("model-call:after-resume"))
            .map(|record| &record.state),
        Some(ModelCallState::Admitted)
    ));
}

#[test]
fn model_call_reservations_reconcile_and_exact_exhaustion_denies_the_next_call() {
    let mut state = bootstrap(1, 1);
    enter_discovery(&mut state);
    let discovery = NodeId::new("protocol-v1:discovery");
    start(&mut state, &discovery, 1, "discovery:start");

    let released = ModelCallAdmission {
        call_id: ModelCallId::new("model-call:released"),
        node_id: discovery.clone(),
        action_id: ActionId::new("action:released"),
        payload_hash: "payload:released".into(),
        input_tokens: 100,
        output_tokens: 100,
        reserved_cost_micros: 500,
        duration_allowance_ms: 500,
    };
    append(
        &mut state,
        "model-call:released:admitted",
        BudgetEvent::ModelCallAdmitted {
            admission: released.clone(),
        },
    );
    append(
        &mut state,
        "model-call:released:reserved",
        BudgetEvent::ModelCallReserved {
            call_id: released.call_id.clone(),
        },
    );
    append(
        &mut state,
        "model-call:released:dispatch-started",
        BudgetEvent::ProviderDispatchStarted {
            call_id: released.call_id.clone(),
            payload_hash: released.payload_hash.clone(),
        },
    );
    assert_eq!(state.active_reservation_count(), 1);
    assert_eq!(state.budgets.mission_usage.model_calls_reserved, 1);
    append(
        &mut state,
        "model-call:released:reconciled",
        BudgetEvent::ModelCallReconciled {
            call_id: released.call_id,
            result: ModelCallReconciliation::ReleasedUncontacted,
        },
    );
    assert_eq!(state.active_reservation_count(), 0);
    assert_eq!(state.budgets.mission_usage, BudgetUsage::default());

    let consumed = ModelCallAdmission {
        call_id: ModelCallId::new("model-call:consumed"),
        node_id: discovery.clone(),
        action_id: ActionId::new("action:consumed"),
        payload_hash: "payload:consumed".into(),
        input_tokens: 100,
        output_tokens: 100,
        reserved_cost_micros: 500,
        duration_allowance_ms: 500,
    };
    append(
        &mut state,
        "model-call:consumed:admitted",
        BudgetEvent::ModelCallAdmitted {
            admission: consumed.clone(),
        },
    );
    append(
        &mut state,
        "model-call:consumed:reserved",
        BudgetEvent::ModelCallReserved {
            call_id: consumed.call_id.clone(),
        },
    );
    append(
        &mut state,
        "model-call:consumed:dispatched",
        BudgetEvent::ProviderDispatchStarted {
            call_id: consumed.call_id.clone(),
            payload_hash: consumed.payload_hash.clone(),
        },
    );
    assert_eq!(
        decide(&state).expect("dispatched model call waits"),
        ProtocolDecision::Wait {
            reason: WaitReason::ProviderReconciliation {
                call_id: consumed.call_id.clone(),
            },
        }
    );
    append(
        &mut state,
        "model-call:consumed:reconciled",
        BudgetEvent::ModelCallReconciled {
            call_id: consumed.call_id,
            result: ModelCallReconciliation::Consumed {
                actual_cost_micros: 400,
                duration_ms: 300,
            },
        },
    );
    assert_eq!(state.active_reservation_count(), 0);
    assert_eq!(state.budgets.mission_usage.model_calls_consumed, 1);
    assert_eq!(state.budgets.mission_usage.cost_micros_consumed, 400);
    assert_eq!(state.budgets.mission_usage.duration_ms_consumed, 300);

    let exhausted_snapshot = state.clone();
    let next = ModelCallAdmission {
        call_id: ModelCallId::new("model-call:over-budget"),
        node_id: discovery.clone(),
        action_id: ActionId::new("action:over-budget"),
        payload_hash: "payload:over-budget".into(),
        input_tokens: 1,
        output_tokens: 1,
        reserved_cost_micros: 1,
        duration_allowance_ms: 1,
    };
    let event = envelope(
        &state,
        "model-call:over-budget:admitted",
        BudgetEvent::ModelCallAdmitted { admission: next },
    );
    assert_eq!(
        state.append_event(event),
        Err(ProtocolViolation::BudgetExceeded {
            node_id: Some(discovery),
            dimension: "model_calls",
        })
    );
    assert_eq!(state, exhausted_snapshot);
}

#[test]
fn plan_graph_admission_enforces_review_topology_and_single_materialization() {
    let mut state = bootstrap(3, 20);
    let plan = active_planning_with_plan(&mut state);
    let (_, mut malformed_specs) = standard_plan(1);
    let completion_index = malformed_specs
        .iter()
        .position(|node| node.kind == NodeKind::CompletionEvaluation)
        .expect("completion node");
    let mut completion = malformed_specs.remove(completion_index);
    completion.dependencies.clear();
    let review_index = malformed_specs
        .iter()
        .position(|node| node.kind == NodeKind::Review)
        .expect("review node");
    malformed_specs.insert(review_index, completion.clone());

    let malformed = envelope(
        &state,
        "planning:completion-before-review",
        GraphEvent::NodesAdded {
            plan_proof_id: plan.clone(),
            nodes: malformed_specs,
        },
    );
    let before_malformed = state.clone();
    assert_eq!(
        state.append_event(malformed),
        Err(ProtocolViolation::InvalidGraph {
            code: "completion_does_not_depend_on_required_review",
            node_id: Some(completion.id),
        })
    );
    assert_eq!(state, before_malformed);

    let (_, valid_specs) = standard_plan(1);
    append(
        &mut state,
        "planning:materialize-graph",
        GraphEvent::NodesAdded {
            plan_proof_id: plan.clone(),
            nodes: valid_specs.clone(),
        },
    );
    let materialized = state.clone();
    let second_materialization = envelope(
        &state,
        "planning:materialize-second-graph",
        GraphEvent::NodesAdded {
            plan_proof_id: plan,
            nodes: valid_specs,
        },
    );
    assert_eq!(
        state.append_event(second_materialization),
        Err(ProtocolViolation::InvalidGraph {
            code: "plan_graph_already_materialized",
            node_id: None,
        })
    );
    assert_eq!(state, materialized);
}

#[test]
fn succeeded_no_op_rejects_an_already_materialized_plan_graph() {
    let mut state = bootstrap(3, 20);
    let plan = active_planning_with_plan(&mut state);
    let (_, specs) = standard_plan(1);
    append(
        &mut state,
        "planning:materialize-graph",
        GraphEvent::NodesAdded {
            plan_proof_id: plan,
            nodes: specs,
        },
    );

    let planning = NodeId::new("protocol-v1:planning");
    let no_op = proof(
        &mut state,
        "no-op-satisfied-after-materialization",
        ProofKind::NoOpSatisfied,
        vec![planning.clone()],
        Vec::new(),
    );
    succeed(&mut state, &planning, &no_op, "planning:no-op-succeeded");
    let remaining_work = state
        .unresolved_required_nodes()
        .into_iter()
        .collect::<Vec<_>>();
    let terminal = envelope(
        &state,
        "terminal:no-op-after-materialized-plan",
        TerminalEvent::CanonicalResultRecorded {
            result: CanonicalResult {
                mission: MissionResult::SucceededNoOp {
                    no_op_proof_id: no_op,
                },
                process_health: ProcessHealth::Healthy,
                reason_code: "already_satisfied".into(),
                repository_revision: state.repository_revision.clone(),
                remaining_work,
            },
        },
    );
    let snapshot = state.clone();
    assert_eq!(
        state.append_event(terminal),
        Err(ProtocolViolation::TerminalPredicate {
            code: "no_op_conflicts_with_materialized_plan",
        })
    );
    assert_eq!(state, snapshot);
}

#[test]
fn implementation_barrier_blocks_validation_until_every_target_is_verified() {
    let mut state = bootstrap(3, 20);
    let (nodes, _) = enter_implementation(&mut state, 2);
    let first_proof = complete_implementation_node(&mut state, &nodes.implementation[0], 0);

    assert_eq!(
        state.node(&nodes.validation).map(|node| &node.state),
        Some(&NodeState::Pending)
    );
    let snapshot = state.clone();
    let incomplete_barrier = envelope(
        &state,
        "proof:premature-implementation-barrier",
        EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                id: ProofId::new("proof:premature-implementation-barrier"),
                kind: ProofKind::ImplementationBarrier,
                repository_revision: state.repository_revision.clone(),
                node_ids: nodes.implementation.clone(),
                related_proof_ids: vec![first_proof],
                related_evidence_ids: Vec::new(),
                detail_hash: "premature-barrier".into(),
            },
        },
    );
    assert!(matches!(
        state.append_event(incomplete_barrier),
        Err(ProtocolViolation::InvalidProof {
            code: "implementation_node_not_verified",
            ..
        })
    ));
    assert_eq!(state, snapshot);

    let second_proof = complete_implementation_node(&mut state, &nodes.implementation[1], 1);
    let barrier = proof(
        &mut state,
        "implementation-barrier",
        ProofKind::ImplementationBarrier,
        nodes.implementation.clone(),
        vec![second_proof],
    );
    assert_eq!(
        state.node(&nodes.validation).map(|node| &node.state),
        Some(&NodeState::Pending)
    );
    advance(
        &mut state,
        ProtocolStage::Implementation,
        ProtocolStage::Validation,
        &barrier,
    );
    assert_eq!(
        state.node(&nodes.validation).map(|node| &node.state),
        Some(&NodeState::Ready)
    );
}

#[test]
fn validation_repair_transition_returns_the_originating_gate_to_ready() {
    let mut state = bootstrap(3, 20);
    let (nodes, _) = enter_implementation(&mut state, 1);
    complete_implementation_and_enter_validation(&mut state, &nodes);

    start(&mut state, &nodes.validation, 1, "validation:start");
    append(
        &mut state,
        "validation:failed",
        GraphEvent::NodeFailed {
            node_id: nodes.validation.clone(),
            failure_revision_id: FailureRevisionId::new("failure:validation:1"),
            terminal: false,
        },
    );
    let failure = proof(
        &mut state,
        "validation-failure",
        ProofKind::ValidationFailure,
        vec![nodes.validation.clone()],
        Vec::new(),
    );
    advance(
        &mut state,
        ProtocolStage::Validation,
        ProtocolStage::Repair,
        &failure,
    );

    let eligibility = proof(
        &mut state,
        "repair-eligibility",
        ProofKind::RepairEligibility,
        Vec::new(),
        vec![failure],
    );
    let repair = NodeId::new("validation-repair:1");
    append(
        &mut state,
        "repair:node-added",
        GraphEvent::ValidationRepairNodeAdded {
            eligibility_proof_id: eligibility,
            node: NodeSpec {
                id: repair.clone(),
                kind: NodeKind::ValidationRepair,
                required: true,
                dependencies: Vec::new(),
                budget: model_budget(2),
            },
        },
    );
    start(&mut state, &repair, 1, "repair:start");
    let nonselected_verified = proof(
        &mut state,
        "repair-verified-but-not-selected-for-success",
        ProofKind::RepairVerified,
        vec![repair.clone()],
        Vec::new(),
    );
    let verified = proof(
        &mut state,
        "repair-verified",
        ProofKind::RepairVerified,
        vec![repair.clone()],
        Vec::new(),
    );
    succeed(&mut state, &repair, &verified, "repair:succeeded");

    let wrong_gate = envelope(
        &state,
        "proof:validation-rerun-wrong-gate",
        EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                id: ProofId::new("proof:validation-rerun-wrong-gate"),
                kind: ProofKind::ValidationRerunScheduled,
                repository_revision: state.repository_revision.clone(),
                node_ids: vec![nodes.review.clone()],
                related_proof_ids: vec![verified.clone()],
                related_evidence_ids: Vec::new(),
                detail_hash: "validation-rerun-wrong-gate".into(),
            },
        },
    );
    let repaired_snapshot = state.clone();
    assert!(matches!(
        state.append_event(wrong_gate),
        Err(ProtocolViolation::InvalidProof {
            code: "originating_validation_gate_mismatch",
            ..
        })
    ));
    assert_eq!(state, repaired_snapshot);

    let wrong_repair_success = envelope(
        &state,
        "proof:validation-rerun-wrong-repair-success",
        EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                id: ProofId::new("proof:validation-rerun-wrong-repair-success"),
                kind: ProofKind::ValidationRerunScheduled,
                repository_revision: state.repository_revision.clone(),
                node_ids: vec![nodes.validation.clone()],
                related_proof_ids: vec![nonselected_verified],
                related_evidence_ids: Vec::new(),
                detail_hash: "validation-rerun-wrong-repair-success".into(),
            },
        },
    );
    assert!(matches!(
        state.append_event(wrong_repair_success),
        Err(ProtocolViolation::InvalidProof {
            code: "verified_repair_proof_mismatch",
            ..
        })
    ));
    assert_eq!(state, repaired_snapshot);

    let rerun = proof(
        &mut state,
        "validation-rerun-scheduled",
        ProofKind::ValidationRerunScheduled,
        vec![nodes.validation.clone()],
        vec![verified],
    );
    advance(
        &mut state,
        ProtocolStage::Repair,
        ProtocolStage::Validation,
        &rerun,
    );

    assert!(matches!(
        state.node(&repair).map(|node| &node.state),
        Some(NodeState::Succeeded { .. })
    ));
    assert_eq!(
        state.node(&nodes.validation).map(|node| &node.state),
        Some(&NodeState::Ready)
    );
    start(&mut state, &nodes.validation, 2, "validation:rerun:start");
}

#[test]
fn publication_eligibility_requires_validation_review_and_completion_in_order() {
    let mut state = bootstrap(3, 20);
    let (nodes, _) = enter_implementation(&mut state, 1);
    complete_implementation_and_enter_validation(&mut state, &nodes);
    complete_validation_and_enter_review(&mut state, &nodes);

    let premature = envelope(
        &state,
        "proof:premature-publication-eligibility",
        EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                id: ProofId::new("proof:premature-publication-eligibility"),
                kind: ProofKind::PublicationEligibility,
                repository_revision: state.repository_revision.clone(),
                node_ids: Vec::new(),
                related_proof_ids: Vec::new(),
                related_evidence_ids: Vec::new(),
                detail_hash: "premature-eligibility".into(),
            },
        },
    );
    let snapshot = state.clone();
    assert!(matches!(
        state.append_event(premature),
        Err(ProtocolViolation::InvalidProof {
            code: "review_or_completion_incomplete",
            ..
        })
    ));
    assert_eq!(state, snapshot);

    start(&mut state, &nodes.review, 1, "review:start");
    let review = proof(
        &mut state,
        "review-completed",
        ProofKind::ReviewCompleted,
        vec![nodes.review.clone()],
        Vec::new(),
    );
    succeed(&mut state, &nodes.review, &review, "review:succeeded");
    let still_premature = envelope(
        &state,
        "proof:eligibility-before-completion",
        EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                id: ProofId::new("proof:eligibility-before-completion"),
                kind: ProofKind::PublicationEligibility,
                repository_revision: state.repository_revision.clone(),
                node_ids: Vec::new(),
                related_proof_ids: vec![review.clone()],
                related_evidence_ids: Vec::new(),
                detail_hash: "eligibility-before-completion".into(),
            },
        },
    );
    assert!(matches!(
        state.append_event(still_premature),
        Err(ProtocolViolation::InvalidProof {
            code: "review_or_completion_incomplete",
            ..
        })
    ));

    start(&mut state, &nodes.completion, 1, "completion:start");
    let completion = proof(
        &mut state,
        "completion-evaluated",
        ProofKind::CompletionEvaluated,
        vec![nodes.completion.clone()],
        vec![review],
    );
    succeed(
        &mut state,
        &nodes.completion,
        &completion,
        "completion:succeeded",
    );
    let eligibility = proof(
        &mut state,
        "publication-eligibility",
        ProofKind::PublicationEligibility,
        Vec::new(),
        vec![completion],
    );
    advance(
        &mut state,
        ProtocolStage::Review,
        ProtocolStage::Publication,
        &eligibility,
    );
    start(
        &mut state,
        &nodes.publication,
        1,
        "publication:eligibility:start",
    );
}

#[test]
fn canonical_terminal_result_is_immutable_but_its_exact_event_replays() {
    let mut state = successful_execution();
    let terminal_event = state
        .event_log
        .last()
        .expect("terminal event")
        .envelope
        .clone();
    let terminal_revision = state.aggregate_revision;
    assert_eq!(
        state
            .append_event(terminal_event)
            .expect("exact terminal event replay"),
        AppendOutcome::IdempotentReplay {
            revision: terminal_revision,
        }
    );
    assert_eq!(state.aggregate_revision, terminal_revision);

    let forbidden = envelope(
        &state,
        "terminal:attempted-rewrite",
        TerminalEvent::CanonicalResultRecorded {
            result: CanonicalResult {
                mission: MissionResult::Canceled {
                    cancellation_reason_code: "operator_canceled".into(),
                },
                process_health: ProcessHealth::Healthy,
                reason_code: "canceled".into(),
                repository_revision: state.repository_revision.clone(),
                remaining_work: Vec::new(),
            },
        },
    );
    let snapshot = state.clone();
    assert_eq!(
        state.append_event(forbidden),
        Err(ProtocolViolation::TerminalImmutable)
    );
    assert_eq!(state, snapshot);
}

#[test]
fn json_event_replay_reconstructs_the_identical_terminal_aggregate() {
    let completed = successful_execution();
    let encoded = completed
        .event_log
        .iter()
        .map(|stored| serde_json::to_string(&stored.envelope).expect("serialize event"))
        .collect::<Vec<_>>();

    let mut store = InMemoryEventStore::new(bootstrap(3, 20)).expect("valid bootstrap aggregate");
    for event_json in encoded {
        let event: ProtocolEventEnvelope =
            serde_json::from_str(&event_json).expect("deserialize event");
        store.append(event).expect("replay event");
    }
    assert_eq!(store.events().len(), completed.event_log.len());
    validate_state(store.state()).expect("stored state satisfies invariants");
    let replayed = store.into_state();

    assert_eq!(replayed, completed);
    validate_state(&replayed).expect("replayed state satisfies invariants");
    let state_json = serde_json::to_string(&replayed).expect("serialize state");
    let snapshot: ExecutionState = serde_json::from_str(&state_json).expect("restore state");
    let restored = InMemoryEventStore::restore(bootstrap(3, 20), snapshot)
        .expect("trusted event replay restores the snapshot")
        .into_state();
    assert_eq!(restored, replayed);
    validate_state(&restored).expect("restored state satisfies invariants");
}

#[test]
fn generated_legal_traces_replay_deterministically_and_exact_replays_are_no_ops() {
    let mut seed = 0x9e37_79b9_u32;
    for case in 0..6_u32 {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let implementation_count = usize::try_from(case % 3 + 1).expect("small count");
        let replay_stride = seed % 4 + 2;
        let completed = successful_execution_with_implementation_count(implementation_count);
        let expected_events = completed
            .event_log
            .iter()
            .map(|stored| stored.envelope.clone())
            .collect::<Vec<_>>();
        let mut store =
            InMemoryEventStore::new(bootstrap(3, 20)).expect("valid bootstrap aggregate");

        for (index, event) in expected_events.into_iter().enumerate() {
            let outcome = store
                .append(event.clone())
                .expect("generated event applies");
            assert!(matches!(outcome, AppendOutcome::Applied { .. }));
            store
                .state()
                .validate_invariants()
                .expect("generated prefix satisfies invariants");

            if (u32::try_from(index).expect("test index") + case) % replay_stride == 0 {
                let before = store.state().clone();
                let original_revision = event.aggregate_revision_before.saturating_add(1);
                assert_eq!(
                    store.append(event).expect("exact replay is accepted"),
                    AppendOutcome::IdempotentReplay {
                        revision: original_revision,
                    }
                );
                assert_eq!(store.state(), &before);
            }
        }

        assert_eq!(store.state(), &completed);
        validate_state(store.state()).expect("generated trace replays from genesis");
        assert_eq!(
            completed,
            successful_execution_with_implementation_count(implementation_count),
            "case {case} was not deterministic"
        );
    }
}

#[test]
fn restored_snapshot_must_equal_its_committed_event_replay() {
    let trusted_initial = bootstrap(3, 20);
    let completed = successful_execution();
    assert_eq!(
        InMemoryEventStore::restore(trusted_initial.clone(), completed.clone())
            .expect("valid snapshot restores")
            .into_state(),
        completed
    );

    let mut corrupted = completed.clone();
    let implementation = NodeId::new("implementation:0");
    corrupted
        .nodes
        .get_mut(&implementation)
        .expect("implementation node")
        .attempts_started = 99;

    assert!(matches!(
        validate_state(&corrupted),
        Err(ProtocolViolation::Invariant {
            code: "snapshot_does_not_match_event_replay",
            ..
        })
    ));
    assert!(matches!(
        decide(&corrupted),
        Err(ProtocolViolation::Invariant {
            code: "snapshot_does_not_match_event_replay",
            ..
        })
    ));
    assert!(matches!(
        InMemoryEventStore::restore(trusted_initial.clone(), corrupted),
        Err(ProtocolViolation::Invariant {
            code: "snapshot_does_not_match_event_replay",
            ..
        })
    ));

    let mut serialized_snapshot = serde_json::to_value(&completed).expect("serialize snapshot");
    let mission_budget = serialized_snapshot
        .get_mut("mission_budget")
        .and_then(serde_json::Value::as_object_mut)
        .expect("mission budget object");
    let max_model_calls = mission_budget
        .get("max_model_calls")
        .and_then(serde_json::Value::as_u64)
        .expect("numeric model-call ceiling");
    mission_budget.insert(
        "max_model_calls".into(),
        serde_json::json!(max_model_calls.saturating_add(1)),
    );
    let inflated_budget: ExecutionState =
        serde_json::from_value(serialized_snapshot).expect("deserialize tampered snapshot");
    assert!(matches!(
        decide(&inflated_budget),
        Err(ProtocolViolation::Invariant {
            code: "untrusted_execution_snapshot",
            ..
        })
    ));
    assert!(matches!(
        InMemoryEventStore::restore(trusted_initial, inflated_budget),
        Err(ProtocolViolation::Invariant {
            code: "snapshot_does_not_match_event_replay",
            ..
        })
    ));
}

#[test]
fn persisted_state_and_event_envelope_reject_unknown_json_fields() {
    let state = successful_execution();
    let mut state_value = serde_json::to_value(&state).expect("serialize state value");
    state_value
        .as_object_mut()
        .expect("state is a JSON object")
        .insert("unknown_state_field".into(), serde_json::json!(true));
    let state_error =
        serde_json::from_value::<ExecutionState>(state_value).expect_err("unknown state field");
    assert!(state_error.to_string().contains("unknown_state_field"));

    let event = state
        .event_log
        .first()
        .expect("protocol event")
        .envelope
        .clone();
    let mut event_value = serde_json::to_value(event).expect("serialize event value");
    event_value
        .as_object_mut()
        .expect("event is a JSON object")
        .insert("unknown_event_field".into(), serde_json::json!(true));
    let event_error = serde_json::from_value::<ProtocolEventEnvelope>(event_value)
        .expect_err("unknown event field");
    assert!(event_error.to_string().contains("unknown_event_field"));
}
