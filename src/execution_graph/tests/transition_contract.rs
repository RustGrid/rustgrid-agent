fn transition_graph(status: ExecutionNodeStatus) -> ExecutionGraph {
    ExecutionGraph {
        graph_id: "transition-contract".to_owned(),
        nodes: vec![ExecutionNode {
            id: ExecutionNodeId::new("node"),
            kind: ExecutionNodeKind::DiffReview,
            status,
            required: true,
            ..ExecutionNode::default()
        }],
        ..ExecutionGraph::default()
    }
}

macro_rules! legal_transition_test {
    ($name:ident, $from:ident => $to:ident) => {
        #[test]
        fn $name() {
            let mut graph = transition_graph(ExecutionNodeStatus::$from);
            let revision = graph.revision();
            assert_eq!(
                graph
                    .transition_node(&ExecutionNodeId::new("node"), ExecutionNodeStatus::$to)
                    .expect("legal transition"),
                TransitionOutcome::Applied
            );
            assert_eq!(graph.node_by_str("node").unwrap().status, ExecutionNodeStatus::$to);
            assert!(graph.revision() > revision);
        }
    };
}

legal_transition_test!(pending_becomes_ready, Pending => Ready);
legal_transition_test!(ready_becomes_running, Ready => Running);
legal_transition_test!(recoverable_becomes_running, FailedRecoverable => Running);
legal_transition_test!(running_becomes_applied, Running => Applied);
legal_transition_test!(running_becomes_passed, Running => Passed);
legal_transition_test!(running_becomes_recoverable, Running => FailedRecoverable);
legal_transition_test!(running_becomes_blocking, Running => FailedBlocking);
legal_transition_test!(running_becomes_superseded, Running => Superseded);
legal_transition_test!(running_becomes_skipped, Running => Skipped);
legal_transition_test!(running_becomes_completed, Running => Completed);

#[test]
fn transition_replay_is_idempotent_and_round_trips() {
    let mut graph = transition_graph(ExecutionNodeStatus::Ready);
    let node_id = ExecutionNodeId::new("node");
    graph
        .transition_node(&node_id, ExecutionNodeStatus::Running)
        .expect("start node");
    let revision = graph.revision();
    assert_eq!(
        graph
            .transition_node(&node_id, ExecutionNodeStatus::Running)
            .expect("idempotent replay"),
        TransitionOutcome::IdempotentReplay
    );
    assert_eq!(graph.revision(), revision);

    let encoded = serde_json::to_string(&graph).expect("serialize graph");
    let decoded: ExecutionGraph = serde_json::from_str(&encoded).expect("deserialize graph");
    assert_eq!(decoded, graph);
}

#[test]
fn completed_node_cannot_return_to_active_state() {
    let mut graph = transition_graph(ExecutionNodeStatus::Completed);
    let before = graph.clone();
    let error = graph
        .transition_node(
            &ExecutionNodeId::new("node"),
            ExecutionNodeStatus::Running,
        )
        .expect_err("completed work cannot restart through forward execution");
    assert!(matches!(
        error,
        GraphTransitionError::IllegalNodeTransition {
            from: ExecutionNodeStatus::Completed,
            to: ExecutionNodeStatus::Running,
            ..
        }
    ));
    assert_eq!(graph, before, "failed transitions are atomic");
}

#[test]
fn graph_rejects_multiple_active_execution_owners() {
    let mut graph = transition_graph(ExecutionNodeStatus::Running);
    graph.nodes.push(ExecutionNode {
        id: ExecutionNodeId::new("other"),
        kind: ExecutionNodeKind::DiffReview,
        status: ExecutionNodeStatus::Running,
        ..ExecutionNode::default()
    });
    assert!(graph
        .validate_invariants()
        .expect_err("two owners must be rejected")
        .message
        .contains("multiple active owners"));
}

#[test]
fn persisted_reservations_cannot_exceed_signed_budget() {
    let graph = transition_graph(ExecutionNodeStatus::Ready);
    let mut snapshot = ExecutionSnapshot {
        run_id: "budget-invariant".to_owned(),
        graph,
        budget: BudgetState::new(MissionBudget {
            max_model_calls: 1,
            max_cost_micros: 100,
            max_duration: Duration::from_secs(1),
            max_target_repair_rounds: 0,
        }),
        ..ExecutionSnapshot::default()
    };
    snapshot.budget.total_model_calls_reserved = 2;
    assert!(snapshot
        .validate_invariants()
        .expect_err("over-reservation must be rejected")
        .message
        .contains("signed mission call budget"));
}

#[test]
fn validation_evidence_is_bound_to_the_source_tree_hash() {
    let mut snapshot = ExecutionSnapshot {
        run_id: "source-tree-evidence".to_owned(),
        current_repository: RepositorySnapshot {
            fingerprint: "repository-envelope".to_owned(),
            source_tree_hash: "tree-current".to_owned(),
            ..RepositorySnapshot::default()
        },
        graph: graph(),
        budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
        ..ExecutionSnapshot::default()
    };
    let node = snapshot
        .graph
        .nodes()
        .find(|node| node.kind.is_validation())
        .expect("validation node")
        .clone();
    let gate = node.validation.expect("validation gate");
    let wrong_fingerprint = gate.fingerprint("repository-envelope");
    let event = ExecutionDomainEvent::ValidationEvidenceRecorded {
        sequence: 1,
        node_id: node.id.clone(),
        evidence: ValidationEvidenceRecord {
            evidence_id: "wrong-tree".to_owned(),
            node_id: node.id,
            gate_id: gate.gate_id,
            fingerprint: wrong_fingerprint,
            repository_fingerprint: "repository-envelope".to_owned(),
            command: gate.command,
            working_directory: gate.working_directory,
            status: ValidationEvidenceStatus::Passed,
            exit_code: Some(0),
            output_summary: "passed elsewhere".to_owned(),
            duration: Duration::from_millis(1),
        },
    };
    let before = snapshot.clone();
    snapshot
        .append_event(event)
        .expect_err("evidence from another source tree must be rejected");
    assert_eq!(snapshot, before, "event rejection must be atomic");
}

#[test]
fn persisted_graph_revision_cannot_regress() {
    let mut snapshot = ExecutionSnapshot {
        run_id: "revision-invariant".to_owned(),
        graph: transition_graph(ExecutionNodeStatus::Ready),
        budget: BudgetState::new(MissionBudget::default()),
        ..ExecutionSnapshot::default()
    };
    let mut revision_five = transition_graph(ExecutionNodeStatus::Ready);
    revision_five.revision = 5;
    snapshot
        .append_event(ExecutionDomainEvent::GraphCreated {
            sequence: 1,
            graph_id: revision_five.graph_id.clone(),
            revision: 5,
            graph: Some(revision_five),
            preserved_node_ids: Vec::new(),
        })
        .expect("first persisted revision");

    let before = snapshot.clone();
    let mut revision_four = transition_graph(ExecutionNodeStatus::Ready);
    revision_four.revision = 4;
    snapshot
        .append_event(ExecutionDomainEvent::GraphCreated {
            sequence: 2,
            graph_id: revision_four.graph_id.clone(),
            revision: 4,
            graph: Some(revision_four),
            preserved_node_ids: Vec::new(),
        })
        .expect_err("persisted revision regression must fail");
    assert_eq!(snapshot, before, "revision rejection must be atomic");
}
