use super::*;

fn target(path: &str, role: &str) -> PlannedTarget {
    PlannedTarget {
        change_id: format!("change-{path}"),
        path: path.to_owned(),
        role: role.to_owned(),
        intent: format!("update {path}"),
        acceptance_criteria_ids: vec!["ac-1".to_owned()],
        operation: Default::default(),
        new_file: false,
    }
}

fn gate(id: &str, gate_type: ValidationGateType) -> ValidationGateSpec {
    ValidationGateSpec {
        gate_id: id.to_owned(),
        gate_type,
        command: format!("run {id}"),
        working_directory: ".".to_owned(),
        required: true,
        dependency_lock_hash: "lock".to_owned(),
        relevant_environment_fingerprint: "env".to_owned(),
    }
}

fn graph() -> ExecutionGraph {
    ExecutionGraph::from_targets(
        "graph-1",
        MissionComplexity::Small,
        "tree-1",
        &[
            // Input ordering must not permit a test mutation before source work.
            target("tests/theme.test.ts", "test"),
            target("src/theme.ts", "production"),
        ],
        &[
            gate("focused", ValidationGateType::FocusedTest),
            gate("suite", ValidationGateType::TestSuite),
            gate("build", ValidationGateType::Build),
        ],
        &MissionBudget::for_complexity(MissionComplexity::Small),
    )
}

fn recovery_publication_snapshot() -> (ExecutionSnapshot, ExecutionNodeId, Vec<String>) {
    let mut graph = graph();
    let mutation_ids = graph
        .nodes
        .iter()
        .filter(|node| node.kind.is_mutation())
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    for node_id in mutation_ids {
        graph
            .set_node_status(&node_id, ExecutionNodeStatus::Completed)
            .expect("apply recovery fixture target");
    }

    let repository_fingerprint = "tree-recovery".to_owned();
    let mut evidence = EvidenceStore::default();
    let validation_ids = graph
        .nodes
        .iter()
        .filter(|node| node.required && node.kind.is_validation())
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    let mut evidence_ids = Vec::new();
    for (index, node_id) in validation_ids.into_iter().enumerate() {
        let gate = graph
            .node(&node_id)
            .and_then(|node| node.validation.clone())
            .expect("validation gate");
        let evidence_id = format!("recovery-validation-{index}");
        let validation_fingerprint = gate.fingerprint(&repository_fingerprint);
        evidence.record_validation(ValidationEvidenceRecord {
            evidence_id: evidence_id.clone(),
            node_id: node_id.clone(),
            gate_id: gate.gate_id,
            fingerprint: validation_fingerprint,
            repository_fingerprint: repository_fingerprint.clone(),
            command: gate.command,
            working_directory: gate.working_directory,
            status: ValidationEvidenceStatus::Passed,
            exit_code: Some(0),
            output_summary: "passed".to_owned(),
            duration: Duration::from_millis(1),
        });
        let node = graph.node_mut(&node_id).expect("validation node");
        node.status = ExecutionNodeStatus::Passed;
        node.evidence_ids.push(evidence_id.clone());
        graph.refresh_readiness();
        evidence_ids.push(evidence_id);
    }
    evidence_ids.sort();
    let publication = graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::Publication)
        .expect("publication node")
        .id
        .clone();
    let snapshot = ExecutionSnapshot {
        run_id: "run-recovery-publication".to_owned(),
        current_repository: RepositorySnapshot {
            fingerprint: repository_fingerprint.clone(),
            source_tree_hash: repository_fingerprint,
            changed_paths: BTreeSet::from(["src/theme.ts".to_owned()]),
            ..RepositorySnapshot::default()
        },
        graph,
        evidence,
        budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
        ..ExecutionSnapshot::default()
    };
    assert_eq!(
        snapshot
            .current_required_validation_evidence_ids()
            .expect("current validation evidence"),
        evidence_ids
    );
    (snapshot, publication, evidence_ids)
}

include!("core.rs");
include!("transition_contract.rs");
include!("recovery.rs");
