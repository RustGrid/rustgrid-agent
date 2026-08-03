use super::*;
use crate::execution_graph::{
    EvidenceRecord, MissionBudget, MissionComplexity, NodeAttempt, ProgressEventKind,
    RepositorySnapshot,
};
use crate::hosted_orchestrator::{ExecutionDecision, reconcile_execution};

fn target(change_id: &str, path: &str, role: &str) -> GraphPlannedTarget {
    GraphPlannedTarget {
        change_id: change_id.to_owned(),
        path: path.to_owned(),
        role: role.to_owned(),
        intent: format!("Implement {change_id}"),
        acceptance_criteria_ids: vec!["ac-1".to_owned()],
        operation: Default::default(),
        new_file: false,
    }
}

fn gate() -> GraphValidationGateSpec {
    GraphValidationGateSpec {
        gate_id: "tests".to_owned(),
        gate_type: GraphValidationGateType::TestSuite,
        command: "cargo test".to_owned(),
        working_directory: ".".to_owned(),
        required: true,
        dependency_lock_hash: "lock-1".to_owned(),
        relevant_environment_fingerprint: "env-1".to_owned(),
    }
}

fn graph(targets: &[GraphPlannedTarget]) -> ExecutionGraph {
    ExecutionGraph::from_targets(
        "graph-1",
        MissionComplexity::Small,
        "tree-1",
        targets,
        &[gate()],
        &MissionBudget::for_complexity(MissionComplexity::Small),
    )
}

fn notebook(phase: ExecutionPhase) -> WorkerNotebook {
    serde_json::from_value(json!({
        "schema_version": 1,
        "revision": 1,
        "goal": "Exercise graph compatibility",
        "phase": phase,
        "repository_base_sha": "base-1",
        "branch": "rustgrid/test",
        "repository_fingerprint": "tree-1",
        "execution_attempt": 2
    }))
    .expect("minimal worker notebook")
}

fn hosted_manifest() -> HostedManifest {
    serde_json::from_value(json!({
        "manifest_version": 4,
        "budget_source": "user_selected",
        "execution": {
            "execution_id": "00000000-0000-4000-8000-000000000001",
            "status": "running",
            "attempt_number": 1
        },
        "run": {
            "id": "00000000-0000-4000-8000-000000000002",
            "ticket_id": "00000000-0000-4000-8000-000000000003",
            "input_prompt": "Implement the accepted plan.",
            "attempt": 1
        },
        "project_id": "00000000-0000-4000-8000-000000000004",
        "project_key": "RG",
        "project_name": "RustGrid",
        "ticket_id": "00000000-0000-4000-8000-000000000003",
        "ticket_key": "RG-1",
        "ticket_title": "Preserve graph budget",
        "github": {
            "repository_id": 1,
            "repository": "RustGrid/example",
            "clone_url": "https://github.com/RustGrid/example.git",
            "web_base_url": "https://github.com",
            "installation_id": 1,
            "base_ref": "main",
            "base_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "branch": "rustgrid/rg-1",
            "github_token_url": "https://app.rustgrid.test/github-token"
        },
        "ai_gateway": {
            "responses_url": "https://app.rustgrid.test/ai/responses",
            "model": "gpt-5.6-sol",
            "maximum_input_tokens": 100000,
            "maximum_output_tokens": 8000,
            "maximum_model_calls": 14,
            "maximum_cost_usd": "2.00"
        },
        "execution_policy": {
            "policy_version": 1,
            "codex": {
                "command": ["codex", "exec", "--json"],
                "environment_allowlist": ["PATH"]
            },
            "quality_gates": [{
                "id": "test",
                "command": "cargo test",
                "timeout_seconds": 60,
                "required": true
            }],
            "timeout_seconds": 480,
            "sandbox": {
                "mode": "workspace_write",
                "network_access": false,
                "writable_roots": ["."],
                "approval_policy": "never"
            }
        },
        "execution_policy_sha256": "policy-hash",
        "heartbeat_url": "https://app.rustgrid.test/heartbeat",
        "token_refresh_url": "https://app.rustgrid.test/token/refresh",
        "events_url": "https://app.rustgrid.test/events",
        "telemetry_url": "https://app.rustgrid.test/telemetry",
        "state_url": "https://app.rustgrid.test/state",
        "complete_url": "https://app.rustgrid.test/complete"
    }))
    .expect("hosted manifest")
}

#[test]
fn manifest_validation_gates_are_canonicalized_before_graph_construction() {
    let mut manifest = hosted_manifest();
    let prototype = manifest.execution_policy.quality_gates[0].clone();
    let gate = |id: &str, command: &str| {
        let mut gate = prototype.clone();
        gate.id = id.to_owned();
        gate.command = command.to_owned();
        gate
    };
    manifest.execution_policy.quality_gates = vec![
        gate("lint-z", "cargo clippy"),
        gate("build", "cargo build"),
        gate("suite", "cargo test"),
        gate("focused", "cargo test focused_theme"),
        gate("custom", "npm run docs-check"),
        gate("typecheck-a", "npm run typecheck"),
    ];

    let canonical = canonical_validation_gates(&manifest);
    assert_eq!(
        canonical
            .iter()
            .map(|gate| gate.gate_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "focused",
            "lint-z",
            "typecheck-a",
            "suite",
            "build",
            "custom",
        ]
    );
    assert_eq!(
        canonical
            .iter()
            .map(|gate| gate.gate_type)
            .collect::<Vec<_>>(),
        vec![
            GraphValidationGateType::FocusedTest,
            GraphValidationGateType::Lint,
            GraphValidationGateType::Typecheck,
            GraphValidationGateType::TestSuite,
            GraphValidationGateType::Build,
            GraphValidationGateType::Custom,
        ]
    );
}

#[test]
fn existing_vitest_target_inserts_focused_gate_before_broad_validation() {
    let mut manifest = hosted_manifest();
    manifest.execution_policy.quality_gates[0].id = "test".into();
    manifest.execution_policy.quality_gates[0].command = "npm test".into();
    let targets = vec![GraphPlannedTarget {
        change_id: "theme-test".into(),
        path: "tests/theme-provider.test.tsx".into(),
        role: "test".into(),
        intent: "cover theme selection".into(),
        acceptance_criteria_ids: vec!["ac-1".into()],
        operation: Default::default(),
        new_file: false,
    }];
    let gates = canonical_validation_gates_for_targets(&manifest, &targets, true);
    assert_eq!(gates.len(), 2);
    assert_eq!(gates[0].gate_type, GraphValidationGateType::FocusedTest);
    assert_eq!(
        gates[0].command,
        "npx vitest run tests/theme-provider.test.tsx"
    );
    assert_eq!(gates[1].gate_type, GraphValidationGateType::TestSuite);
}

#[test]
fn startup_bootstrap_suppresses_redundant_install_unless_dependencies_change() {
    let mut manifest = hosted_manifest();
    let mut install = manifest.execution_policy.quality_gates[0].clone();
    install.id = "install".into();
    install.command = "npm ci --maxsockets=1".into();
    manifest.execution_policy.quality_gates.push(install);
    let source = GraphPlannedTarget {
        change_id: "theme".into(),
        path: "src/theme.tsx".into(),
        role: "source".into(),
        intent: "update theme".into(),
        acceptance_criteria_ids: vec![],
        operation: Default::default(),
        new_file: false,
    };
    let gates =
        canonical_validation_gates_for_targets(&manifest, std::slice::from_ref(&source), true);
    assert!(!gates.iter().any(|gate| gate.gate_id == "install"));

    let gates =
        canonical_validation_gates_for_targets(&manifest, std::slice::from_ref(&source), false);
    assert!(gates.iter().any(|gate| gate.gate_id == "install"));

    let mut dependency = source;
    dependency.path = "package-lock.json".into();
    let gates = canonical_validation_gates_for_targets(&manifest, &[dependency], true);
    assert_eq!(gates[0].gate_id, "install");
}

#[test]
fn unstarted_required_gates_materialize_as_ready_or_pending_never_running() {
    let target = target("theme", "src/theme.tsx", "production");
    let mut suite = gate();
    suite.gate_id = "suite".into();
    let mut build = gate();
    build.gate_id = "build".into();
    build.gate_type = GraphValidationGateType::Build;
    build.command = "cargo build".into();
    let mut graph = ExecutionGraph::from_targets(
        "gate-statuses",
        MissionComplexity::Small,
        "tree-1",
        std::slice::from_ref(&target),
        &[suite, build],
        &MissionBudget::for_complexity(MissionComplexity::Small),
    );
    let mutation_id = graph
        .nodes
        .iter()
        .find(|node| node.kind.is_mutation())
        .unwrap()
        .id
        .clone();
    graph
        .set_node_status(&mutation_id, ExecutionNodeStatus::Applied)
        .unwrap();
    graph.refresh_readiness();
    let statuses = materialize_required_gates(&graph, &EvidenceStore::default())
        .into_iter()
        .map(|gate| gate.status)
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        [ValidationStatus::Ready, ValidationStatus::Pending]
    );
}

fn accepted_plan() -> ImplementationPlan {
    ImplementationPlan {
        implementation_status: "ready".to_owned(),
        planned_changes: vec![PlannedChange {
            change_id: "change-source".to_owned(),
            parent_change_id: None,
            path: String::new(),
            targets: vec![PlannedTarget {
                path: "src/lib.rs".to_owned(),
                role: "production".to_owned(),
                operation: Default::default(),
                new_file: false,
                status: IntendedChangeStatus::Planned,
            }],
            change: "Implement the source change".to_owned(),
            reason: "Satisfy ac-1".to_owned(),
            status: IntendedChangeStatus::Planned,
            acceptance_criteria: vec!["ac-1".to_owned()],
            test_coverage: Vec::new(),
        }],
        planned_new_files: Vec::new(),
        planned_test_changes: Vec::new(),
        remaining_unknowns: Vec::new(),
        blocking_unknowns: Vec::new(),
    }
}

fn complex_accepted_plan() -> ImplementationPlan {
    let mut plan = accepted_plan();
    plan.planned_changes = [
        ("dependency", "Cargo.toml", "production", false),
        ("schema", "migrations/0099_scope.sql", "production", true),
        (
            "integration",
            "src/github/auth_client.rs",
            "production",
            true,
        ),
        ("tests", "tests/integration_test.rs", "tests", true),
    ]
    .into_iter()
    .map(|(change_id, path, role, new_file)| PlannedChange {
        change_id: change_id.to_owned(),
        parent_change_id: None,
        path: String::new(),
        targets: vec![PlannedTarget {
            path: path.to_owned(),
            role: role.to_owned(),
            operation: new_file.then_some(crate::execution_graph::TargetOperation::CreateNew),
            new_file,
            status: IntendedChangeStatus::Planned,
        }],
        change: format!("Implement {change_id}"),
        reason: "Exercise authoritative complexity inputs".to_owned(),
        status: IntendedChangeStatus::Planned,
        acceptance_criteria: vec!["ac-1".to_owned()],
        test_coverage: Vec::new(),
    })
    .collect();
    plan
}

#[test]
fn complexity_is_provisional_until_an_accepted_plan_reclassifies_it() {
    let manifest = hosted_manifest();
    let mut checkpoint = HostedOrchestrationCheckpoint::bootstrap(&manifest, "tree-clean");
    let provisional = checkpoint
        .complexity
        .as_ref()
        .expect("bootstrap assessment");
    assert_eq!(
        provisional.stage,
        ComplexityClassificationStage::Provisional
    );
    assert_eq!(provisional.class, MissionComplexity::Tiny);
    assert_eq!(
        checkpoint
            .graph
            .as_ref()
            .expect("bootstrap graph")
            .complexity_classification_stage,
        ComplexityClassificationStage::Provisional
    );
    assert!(
        checkpoint
            .graph
            .as_ref()
            .expect("bootstrap graph")
            .nodes
            .iter()
            .all(|node| matches!(
                node.kind,
                ExecutionNodeKind::Discovery | ExecutionNodeKind::Planning
            ))
    );

    let graph = checkpoint.graph.as_ref().expect("bootstrap graph");
    let discovery = graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::Discovery)
        .expect("discovery node");
    let planning = graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::Planning)
        .expect("planning node");
    assert_eq!(discovery.budget.max_model_calls, 3);
    assert_eq!(planning.budget.max_model_calls, 2);

    for _ in 0..3 {
        let reservation = checkpoint
            .budget
            .reserve_model_call(&discovery.id, &discovery.budget, 1, Duration::ZERO)
            .expect("discovery bootstrap allowance");
        checkpoint
            .budget
            .consume_model_call_reservation(&reservation, 1, Duration::ZERO);
    }
    assert!(
        checkpoint
            .budget
            .reserve_model_call(&discovery.id, &discovery.budget, 1, Duration::ZERO,)
            .is_err(),
        "discovery must stop after its three-call bootstrap allowance"
    );
    for _ in 0..2 {
        let reservation = checkpoint
            .budget
            .reserve_model_call(&planning.id, &planning.budget, 1, Duration::ZERO)
            .expect("planning bootstrap allowance");
        checkpoint
            .budget
            .consume_model_call_reservation(&reservation, 1, Duration::ZERO);
    }

    let authoritative = checkpoint
        .rebuild_from_plan(&manifest, &complex_accepted_plan(), "tree-clean")
        .clone();
    assert_eq!(
        authoritative.stage,
        ComplexityClassificationStage::Authoritative
    );
    assert_ne!(authoritative.class, MissionComplexity::Tiny);
    for factor in [
        crate::execution_graph::ComplexityFactorKind::PlannedTargetCount,
        crate::execution_graph::ComplexityFactorKind::NewFileCount,
        crate::execution_graph::ComplexityFactorKind::DependencyChanges,
        crate::execution_graph::ComplexityFactorKind::DatabaseSchemaChanges,
        crate::execution_graph::ComplexityFactorKind::ExternalIntegrations,
        crate::execution_graph::ComplexityFactorKind::TestSurface,
        crate::execution_graph::ComplexityFactorKind::ExpectedValidationDuration,
        crate::execution_graph::ComplexityFactorKind::CrossModuleImpact,
    ] {
        assert!(
            authoritative
                .factors
                .iter()
                .any(|entry| entry.kind == factor && entry.value > 0),
            "authoritative assessment omitted {factor:?}"
        );
    }
    assert_eq!(
        checkpoint
            .graph
            .as_ref()
            .expect("accepted-plan graph")
            .complexity_classification_stage,
        ComplexityClassificationStage::Authoritative
    );
}

#[test]
fn legacy_bootstrap_checkpoint_without_stage_is_normalized_to_provisional() {
    let manifest = hosted_manifest();
    let checkpoint = HostedOrchestrationCheckpoint::bootstrap(&manifest, "tree-clean");
    let mut serialized = serde_json::to_value(checkpoint).expect("serialize checkpoint");
    serialized["graph"]
        .as_object_mut()
        .expect("serialized graph")
        .remove("complexity_classification_stage");
    serialized["complexity"]
        .as_object_mut()
        .expect("serialized assessment")
        .remove("stage");

    let mut restored: HostedOrchestrationCheckpoint =
        serde_json::from_value(serialized).expect("restore legacy checkpoint");
    assert_eq!(
        restored
            .graph
            .as_ref()
            .expect("legacy graph")
            .complexity_classification_stage,
        ComplexityClassificationStage::Authoritative,
        "the serde default alone cannot identify a pre-plan checkpoint"
    );

    restored.normalize_pre_plan_classification(&manifest);
    assert_eq!(
        restored
            .graph
            .as_ref()
            .expect("normalized graph")
            .complexity_classification_stage,
        ComplexityClassificationStage::Provisional
    );
    assert_eq!(
        restored.complexity.as_ref().map(|value| value.stage),
        Some(ComplexityClassificationStage::Provisional)
    );
}

#[test]
fn parses_signed_decimal_cost_without_floating_point_drift() {
    assert_eq!(parse_usd_micros("5"), Some(5_000_000));
    assert_eq!(parse_usd_micros("$10.250001"), Some(10_250_001));
    assert_eq!(parse_usd_micros("0.0000099"), Some(9));
    assert_eq!(parse_usd_micros("-1"), None);
    assert_eq!(parse_usd_micros("unbounded"), None);
}

#[test]
fn criterion_references_are_canonical_and_deduplicated() {
    assert_eq!(
        canonical_criterion_ids(&[
            " AC-2 ".to_owned(),
            "ac-1".to_owned(),
            "ac-2".to_owned(),
            String::new(),
        ]),
        vec!["ac-1", "ac-2"]
    );
}

#[test]
fn checkpoint_boundary_does_not_restore_stale_in_flight_ownership() {
    assert_eq!(
        graph_status_from_legacy(IntendedChangeStatus::InProgress),
        ExecutionNodeStatus::Pending
    );
}

#[test]
fn accepted_plan_rebuild_preserves_pre_plan_usage_attempts_and_evidence() {
    let manifest = hosted_manifest();
    let mut checkpoint = HostedOrchestrationCheckpoint::bootstrap(&manifest, "tree-before-plan");
    let discovery_id = ExecutionNodeId::new("discovery");
    let planning_id = ExecutionNodeId::new("planning");
    let discovery_attempt = NodeAttempt {
        attempt: 1,
        repository_fingerprint_before: "tree-before-plan".to_owned(),
        repository_fingerprint_after: Some("tree-before-plan".to_owned()),
        model_calls: 1,
        cost_micros: 100,
        duration: Duration::from_secs(2),
        outcome: Some(ExecutionNodeStatus::Completed),
        ..NodeAttempt::default()
    };
    let planning_attempt = NodeAttempt {
        attempt: 1,
        repository_fingerprint_before: "tree-before-plan".to_owned(),
        repository_fingerprint_after: Some("tree-before-plan".to_owned()),
        model_calls: 1,
        cost_micros: 200,
        duration: Duration::from_secs(3),
        outcome: Some(ExecutionNodeStatus::Completed),
        ..NodeAttempt::default()
    };
    let graph = checkpoint.graph.as_mut().expect("bootstrap graph");
    let discovery = graph.node_mut(&discovery_id).expect("discovery node");
    discovery.status = ExecutionNodeStatus::Completed;
    discovery.attempts.push(discovery_attempt.clone());
    discovery.evidence_ids.push("evidence-discovery".to_owned());
    let planning = graph.node_mut(&planning_id).expect("planning node");
    planning.status = ExecutionNodeStatus::Completed;
    planning.attempts.push(planning_attempt.clone());
    planning.evidence_ids.push("evidence-planning".to_owned());
    graph.refresh_readiness();

    checkpoint.evidence.record(EvidenceRecord {
        evidence_id: "evidence-discovery".to_owned(),
        kind: EvidenceKind::RepositoryObservation,
        node_id: Some(discovery_id.clone()),
        repository_fingerprint: "tree-before-plan".to_owned(),
        summary: "repository topology".to_owned(),
    });
    checkpoint.evidence.record(EvidenceRecord {
        evidence_id: "evidence-planning".to_owned(),
        kind: EvidenceKind::AcceptanceCriterion,
        node_id: Some(planning_id.clone()),
        repository_fingerprint: "tree-before-plan".to_owned(),
        summary: "accepted criterion mapping".to_owned(),
    });
    checkpoint
        .budget
        .record_model_call(discovery_id.clone(), 100, Duration::from_secs(2));
    checkpoint.budget.record_progress_kind(
        1,
        ProgressEventKind::NewRelevantEvidenceRecorded,
        Some(discovery_id.clone()),
    );
    checkpoint
        .budget
        .record_model_call(planning_id.clone(), 200, Duration::from_secs(3));
    checkpoint.budget.record_progress_kind(
        2,
        ProgressEventKind::NodeMadeReady,
        Some(planning_id.clone()),
    );
    let previous_budget = checkpoint.budget.clone();
    let provisional_discovery_budget = checkpoint
        .graph
        .as_ref()
        .and_then(|graph| graph.node(&discovery_id))
        .map(|node| node.budget.clone())
        .expect("provisional discovery budget");
    assert_eq!(
        checkpoint.complexity.as_ref().map(|value| value.stage),
        Some(ComplexityClassificationStage::Provisional)
    );

    let assessment = checkpoint
        .rebuild_from_plan(&manifest, &accepted_plan(), "tree-before-plan")
        .clone();

    let graph = checkpoint.graph.as_ref().expect("accepted-plan graph");
    let discovery = graph.node(&discovery_id).expect("preserved discovery");
    let planning = graph.node(&planning_id).expect("preserved planning");
    assert_eq!(discovery.status, ExecutionNodeStatus::Completed);
    assert_eq!(planning.status, ExecutionNodeStatus::Completed);
    assert_eq!(discovery.attempts, vec![discovery_attempt]);
    assert_eq!(planning.attempts, vec![planning_attempt]);
    assert_eq!(discovery.evidence_ids, vec!["evidence-discovery"]);
    assert_eq!(planning.evidence_ids, vec!["evidence-planning"]);
    assert!(
        checkpoint
            .evidence
            .records
            .contains_key("evidence-discovery")
    );
    assert!(
        checkpoint
            .evidence
            .records
            .contains_key("evidence-planning")
    );

    assert_eq!(checkpoint.budget.mission, assessment.budget);
    assert_eq!(
        assessment.stage,
        ComplexityClassificationStage::Authoritative
    );
    assert_eq!(checkpoint.budget.total_model_calls, 2);
    assert_eq!(checkpoint.budget.total_cost_micros, 300);
    assert_eq!(checkpoint.budget.elapsed, Duration::from_secs(5));
    assert_eq!(
        checkpoint.budget.progress_events,
        previous_budget.progress_events
    );
    assert_eq!(
        checkpoint.budget.progress_score,
        previous_budget.progress_score
    );
    assert_eq!(
        checkpoint
            .budget
            .usage_for(&discovery_id)
            .model_calls_consumed,
        1
    );
    assert_eq!(
        checkpoint
            .budget
            .usage_for(&planning_id)
            .model_calls_consumed,
        1
    );
    assert_ne!(
        discovery.budget, provisional_discovery_budget,
        "accepted-plan classification must replace provisional node budgets"
    );
}

#[test]
fn notebook_is_materialized_from_graph_failures_validation_and_events() {
    let source = target("change-source", "src/lib.rs", "production");
    let test = target("change-test", "tests/lib.rs", "tests");
    let mut graph = graph(&[source.clone(), test.clone()]);
    let source_id = graph
        .nodes
        .iter()
        .find(|node| {
            node.target
                .as_ref()
                .is_some_and(|target| target.path == source.path)
        })
        .expect("source node")
        .id
        .clone();
    let test_id = graph
        .nodes
        .iter()
        .find(|node| {
            node.target
                .as_ref()
                .is_some_and(|target| target.path == test.path)
        })
        .expect("test node")
        .id
        .clone();
    let validation_id = graph
        .nodes
        .iter()
        .find(|node| node.kind.is_validation())
        .expect("validation node")
        .id
        .clone();
    graph
        .set_node_status(&source_id, ExecutionNodeStatus::Applied)
        .expect("apply source");
    graph
        .set_node_status(&test_id, ExecutionNodeStatus::FailedRecoverable)
        .expect("fail test target");
    graph
        .set_node_status(&validation_id, ExecutionNodeStatus::FailedRecoverable)
        .expect("fail validation");
    graph
        .node_mut(&validation_id)
        .expect("validation node")
        .evidence_ids
        .push("validation-tests".to_owned());

    let mut failures = FailureStore::default();
    let mut failure = FailureRecord::new(
        "failure-test",
        test_id,
        FailureCategory::MutationConflict,
        1,
        "tree-1",
        "replacement did not match",
    );
    failure.target_path = Some(test.path.clone());
    failures.record(failure);
    let mut evidence = EvidenceStore::default();
    evidence.record_validation(ValidationEvidenceRecord {
        evidence_id: "validation-tests".to_owned(),
        node_id: validation_id.clone(),
        gate_id: "tests".to_owned(),
        fingerprint: "validation-fingerprint".to_owned(),
        repository_fingerprint: "tree-1".to_owned(),
        command: "cargo test".to_owned(),
        working_directory: ".".to_owned(),
        status: ValidationEvidenceStatus::Failed,
        exit_code: Some(1),
        output_summary: "one test failed".to_owned(),
        duration: Duration::from_millis(42),
    });
    let checkpoint = HostedOrchestrationCheckpoint {
        graph_revision: graph.revision,
        graph: Some(graph),
        domain_events: vec![ExecutionDomainEvent::MutationApplied {
            sequence: 1,
            node_id: source_id.clone(),
            target_path: source.path.clone(),
            repository_fingerprint: "tree-1".to_owned(),
            evidence_id: "mutation-source".to_owned(),
            created_target_evidence: None,
        }],
        failures,
        evidence,
        ..HostedOrchestrationCheckpoint::default()
    };
    let mut notebook = notebook(ExecutionPhase::Implementation);
    checkpoint.materialize_legacy_notebook(&mut notebook);

    assert_eq!(notebook.completed_changes, vec!["change-source"]);
    assert!(
        notebook
            .remaining_work_v2
            .iter()
            .any(|item| { item.change_id == "change-test" && item.path == "tests/lib.rs" })
    );
    assert_eq!(
        notebook
            .remaining_work_v2
            .iter()
            .map(|item| item.change_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "change-test",
            validation_id.as_str(),
            "diff-review",
            "completion-evaluation",
            "publication",
        ])
    );
    assert_eq!(notebook.failed_changes.len(), 1);
    let materialized_failure = &notebook.failed_changes[0];
    assert_eq!(materialized_failure.attempt_index, 1);
    assert_eq!(
        materialized_failure.change_id.as_deref(),
        Some("change-test")
    );
    assert!(materialized_failure.tool.is_empty());
    assert_eq!(materialized_failure.target.as_deref(), Some("tests/lib.rs"));
    assert_eq!(materialized_failure.error_code, "mutation_conflict");
    assert_eq!(materialized_failure.match_count, None);
    assert_eq!(materialized_failure.error, "replacement did not match");
    assert!(!materialized_failure.recovered);
    assert_eq!(
        materialized_failure.reconciliation,
        FailureReconciliation::StillUnresolved
    );
    assert!(materialized_failure.recovery.is_none());
    assert!(materialized_failure.intended_change_sha256.is_none());
    assert_eq!(
        notebook.validation_evidence,
        vec![ValidationEvidence {
            evidence_id: "validation-tests".to_owned(),
            gate_id: "tests".to_owned(),
            gate_type: ValidationGateType::TestSuite,
            command: "cargo test".to_owned(),
            normalized_command: "cargo test".to_owned(),
            command_fingerprint: "validation-fingerprint".to_owned(),
            source_tree_hash: "tree-1".to_owned(),
            dependency_lock_hash: "lock-1".to_owned(),
            started_at: String::new(),
            completed_at: None,
            duration_ms: 42,
            exit_code: Some(1),
            status: ValidationStatus::FailedCode,
            stdout_summary: "one test failed".to_owned(),
            stderr_summary: String::new(),
            source: ValidationSource::ResumeReused,
        }]
    );
    assert_eq!(
        notebook.required_gates,
        vec![RequiredGate {
            gate_id: "tests".to_owned(),
            gate_type: ValidationGateType::TestSuite,
            required: true,
            command: "cargo test".to_owned(),
            status: ValidationStatus::FailedCode,
            evidence_id: Some("validation-tests".to_owned()),
        }]
    );
    assert_eq!(
        notebook.last_successful_action,
        json!({
            "event_type": "mutation_applied",
            "sequence": 1,
            "node_id": source_id.as_str(),
        })
    );
}

#[test]
fn replayed_validation_projection_ignores_blank_or_poisoned_legacy_arrays() {
    let mut validation_graph = graph(&[]);
    for node in &mut validation_graph.nodes {
        if matches!(
            node.kind,
            ExecutionNodeKind::Discovery | ExecutionNodeKind::Planning
        ) {
            node.status = ExecutionNodeStatus::Completed;
        }
    }
    validation_graph.refresh_readiness();
    let validation_node = validation_graph
        .nodes
        .iter()
        .find(|node| node.kind.is_validation())
        .expect("validation node")
        .clone();
    let gate = validation_node
        .validation
        .as_ref()
        .expect("validation gate")
        .clone();
    let validation_fingerprint = gate.fingerprint("tree-1");
    let evidence = ValidationEvidenceRecord {
        evidence_id: "canonical-validation-failure".to_owned(),
        node_id: validation_node.id.clone(),
        gate_id: gate.gate_id,
        fingerprint: validation_fingerprint.clone(),
        repository_fingerprint: "tree-1".to_owned(),
        command: gate.command,
        working_directory: gate.working_directory,
        status: ValidationEvidenceStatus::Failed,
        exit_code: Some(1),
        output_summary: "canonical failure output".to_owned(),
        duration: Duration::from_millis(17),
    };
    let failure_id = FailureId::new("canonical-validation-failure");
    let events = vec![
        ExecutionDomainEvent::ValidationStarted {
            sequence: 1,
            node_id: validation_node.id.clone(),
            fingerprint: validation_fingerprint.clone(),
        },
        ExecutionDomainEvent::ValidationEvidenceRecorded {
            sequence: 2,
            node_id: validation_node.id.clone(),
            evidence,
        },
        ExecutionDomainEvent::FailureRecorded {
            sequence: 3,
            failure: FailureRecord::new(
                failure_id.clone(),
                validation_node.id.clone(),
                FailureCategory::ValidationFailure,
                1,
                "tree-1",
                "canonical failure output",
            ),
        },
        ExecutionDomainEvent::ValidationFailed {
            sequence: 4,
            node_id: validation_node.id.clone(),
            failure_id: failure_id.clone(),
            fingerprint: validation_fingerprint,
        },
    ];
    let encoded = serde_json::to_string(&events).expect("serialize validation event stream");
    let replay_events: Vec<ExecutionDomainEvent> =
        serde_json::from_str(&encoded).expect("deserialize validation event stream");
    let mut replayed = ExecutionSnapshot {
        run_id: "validation-projection-replay".to_owned(),
        current_repository: RepositorySnapshot {
            fingerprint: "tree-1".to_owned(),
            source_tree_hash: "tree-1".to_owned(),
            ..RepositorySnapshot::default()
        },
        graph: validation_graph,
        budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
        ..ExecutionSnapshot::default()
    };
    for event in replay_events {
        replayed
            .append_event(event)
            .expect("replay canonical validation event");
    }
    let mut checkpoint = HostedOrchestrationCheckpoint::default();
    checkpoint.replace_from_snapshot(&replayed);

    let mut blank = notebook(ExecutionPhase::Validation);
    blank.validation_evidence.clear();
    blank.required_gates.clear();
    blank.validation_failures.clear();
    checkpoint.materialize_legacy_notebook(&mut blank);
    let canonical_evidence = blank.validation_evidence.clone();
    let canonical_gates = blank.required_gates.clone();
    let canonical_failures = blank.validation_failures.clone();
    assert_eq!(
        canonical_failures,
        ["canonical-validation-failure: canonical failure output"]
    );
    assert_eq!(canonical_evidence.len(), 1);
    assert_eq!(canonical_evidence[0].status, ValidationStatus::FailedCode);
    assert_eq!(
        canonical_evidence[0].stdout_summary,
        "canonical failure output"
    );
    assert!(canonical_evidence[0].started_at.is_empty());
    assert_eq!(canonical_evidence[0].completed_at, None);
    assert_eq!(canonical_evidence[0].source, ValidationSource::ResumeReused);

    let mut poisoned = notebook(ExecutionPhase::Validation);
    let mut poisoned_evidence = canonical_evidence[0].clone();
    poisoned_evidence.evidence_id = "poisoned-evidence".to_owned();
    poisoned_evidence.status = ValidationStatus::Passed;
    poisoned_evidence.stdout_summary = "poisoned output".to_owned();
    poisoned_evidence.started_at = "2099-01-01T00:00:00Z".to_owned();
    poisoned_evidence.source = ValidationSource::ModelRequested;
    let mut poisoned_gate = canonical_gates[0].clone();
    poisoned_gate.status = ValidationStatus::Passed;
    poisoned_gate.evidence_id = Some("poisoned-evidence".to_owned());
    poisoned.validation_evidence = vec![poisoned_evidence];
    poisoned.required_gates = vec![poisoned_gate];
    poisoned.validation_failures = vec!["poisoned legacy failure".to_owned()];

    checkpoint.materialize_legacy_notebook(&mut poisoned);
    assert_eq!(poisoned.validation_evidence, canonical_evidence);
    assert_eq!(poisoned.required_gates, canonical_gates);
    assert_eq!(poisoned.validation_failures, canonical_failures);

    replayed
        .append_event(ExecutionDomainEvent::FailureRecovered {
            sequence: 5,
            node_id: validation_node.id,
            failure_id,
            repository_fingerprint: "tree-1".to_owned(),
        })
        .expect("recover canonical validation failure");
    checkpoint.replace_from_snapshot(&replayed);
    poisoned.validation_failures = canonical_failures;
    checkpoint.materialize_legacy_notebook(&mut poisoned);
    assert!(
        poisoned.validation_failures.is_empty(),
        "recovered graph failure must clear the legacy projection"
    );
}

#[test]
fn empty_notebook_change_arrays_are_rebuilt_from_graph_targets() {
    let mut source = target("change-palette", "src/palette.rs", "production source");
    source.intent = "Implement the palette".to_owned();
    source.acceptance_criteria_ids = vec![" AC-2 ".to_owned(), "ac-1".to_owned()];
    source.new_file = true;
    let mut other_tests = target("change-other", "tests/other.rs", "test coverage");
    other_tests.intent = "Cover the other behavior".to_owned();
    other_tests.acceptance_criteria_ids = vec!["AC-9".to_owned()];
    let mut palette_tests = target(
        "change-palette",
        "tests/palette.rs",
        "palette test coverage",
    );
    palette_tests.intent = "Implement the palette".to_owned();
    palette_tests.acceptance_criteria_ids = vec!["ac-3".to_owned(), "AC-2".to_owned()];
    let mut graph = graph(&[source.clone(), other_tests, palette_tests]);
    let source_id = graph
        .nodes
        .iter()
        .find(|node| node.target.as_ref() == Some(&source))
        .expect("palette source node")
        .id
        .clone();
    graph
        .set_node_status(&source_id, ExecutionNodeStatus::Applied)
        .expect("apply palette source");
    let checkpoint = HostedOrchestrationCheckpoint {
        graph_revision: graph.revision,
        graph: Some(graph),
        ..HostedOrchestrationCheckpoint::default()
    };
    let mut notebook = notebook(ExecutionPhase::Implementation);
    assert!(notebook.planned_changes.is_empty());
    assert!(notebook.intended_changes.is_empty());

    checkpoint.materialize_legacy_notebook(&mut notebook);

    assert_eq!(notebook.planned_changes.len(), 2);
    let palette = &notebook.planned_changes[0];
    assert_eq!(palette.change_id, "change-palette");
    assert_eq!(palette.parent_change_id, None);
    assert!(palette.path.is_empty());
    assert_eq!(palette.change, "Implement the palette");
    assert_eq!(palette.reason, "Implement the palette");
    assert_eq!(palette.status, IntendedChangeStatus::Partial);
    assert_eq!(palette.acceptance_criteria, ["ac-1", "ac-2", "ac-3"]);
    assert!(palette.test_coverage.is_empty());
    assert_eq!(palette.targets.len(), 2);
    assert_eq!(palette.targets[0].path, "src/palette.rs");
    assert_eq!(palette.targets[0].role, "production source");
    assert!(palette.targets[0].new_file);
    assert_eq!(palette.targets[0].status, IntendedChangeStatus::Applied);
    assert_eq!(palette.targets[1].path, "tests/palette.rs");
    assert_eq!(palette.targets[1].role, "palette test coverage");
    assert!(!palette.targets[1].new_file);
    assert_eq!(palette.targets[1].status, IntendedChangeStatus::Planned);

    let other = &notebook.planned_changes[1];
    assert_eq!(other.change_id, "change-other");
    assert_eq!(other.acceptance_criteria, ["ac-9"]);
    assert_eq!(other.targets.len(), 1);
    assert_eq!(other.targets[0].path, "tests/other.rs");

    assert_eq!(notebook.intended_changes.len(), 2);
    for (planned, intended) in notebook
        .planned_changes
        .iter()
        .zip(&notebook.intended_changes)
    {
        assert_eq!(intended.change_id, planned.change_id);
        assert_eq!(intended.intent, planned.change);
        assert_eq!(intended.status, planned.status);
        assert!(intended.target.is_empty());
        assert_eq!(
            intended
                .targets
                .iter()
                .map(|target| (
                    target.path.as_str(),
                    target.role.as_str(),
                    target.new_file,
                    target.status,
                ))
                .collect::<Vec<_>>(),
            planned
                .targets
                .iter()
                .map(|target| (
                    target.path.as_str(),
                    target.role.as_str(),
                    target.new_file,
                    target.status,
                ))
                .collect::<Vec<_>>()
        );
        assert!(intended.attempts.is_empty());
        assert!(intended.recovery.is_none());
    }
}

#[test]
fn old_checkpoint_imports_legacy_state_once_and_ignores_stale_mutation_state() {
    let planned_target = target("change-one", "src/one.rs", "production");
    let graph = graph(std::slice::from_ref(&planned_target));
    let checkpoint = HostedOrchestrationCheckpoint {
        graph_revision: graph.revision,
        graph: Some(graph),
        ..HostedOrchestrationCheckpoint::default()
    };
    let mut old_payload = serde_json::to_value(checkpoint).expect("serialize checkpoint");
    old_payload
        .as_object_mut()
        .expect("checkpoint object")
        .remove("legacy_import_completed");
    let mut restored: HostedOrchestrationCheckpoint =
        serde_json::from_value(old_payload).expect("deserialize old checkpoint");
    assert!(restored.legacy_import_pending());

    let mut notebook = notebook(ExecutionPhase::Implementation);
    let (planned_changes, intended_changes) =
        materialize_legacy_changes(restored.graph.as_ref().expect("graph"));
    notebook.planned_changes = planned_changes;
    notebook.intended_changes = intended_changes;
    notebook.planned_changes[0].status = IntendedChangeStatus::Applied;
    notebook.planned_changes[0].targets[0].status = IntendedChangeStatus::Applied;
    notebook.intended_changes[0].status = IntendedChangeStatus::Applied;
    notebook.intended_changes[0].targets[0].status = IntendedChangeStatus::Applied;
    assert!(restored.import_legacy_state_once(
        &notebook,
        std::slice::from_ref(&planned_target.path),
        &HostedReconciliationFacts::default(),
    ));
    assert!(restored.legacy_import_completed);
    let node_id = restored
        .graph
        .as_ref()
        .expect("graph")
        .nodes
        .iter()
        .find(|node| node.target.as_ref() == Some(&planned_target))
        .expect("planned target node")
        .id
        .clone();
    let authoritative_status = restored
        .graph
        .as_ref()
        .expect("graph")
        .node(&node_id)
        .expect("planned target node")
        .status;
    assert_eq!(authoritative_status, ExecutionNodeStatus::Applied);

    let migrated_payload = serde_json::to_value(restored).expect("serialize migrated checkpoint");
    assert_eq!(migrated_payload["legacy_import_completed"], true);
    let mut resumed: HostedOrchestrationCheckpoint =
        serde_json::from_value(migrated_payload).expect("resume migrated checkpoint");
    notebook.planned_changes[0].status = IntendedChangeStatus::Planned;
    notebook.planned_changes[0].targets[0].status = IntendedChangeStatus::Planned;
    notebook.intended_changes[0].status = IntendedChangeStatus::Planned;
    notebook.intended_changes[0].targets[0].status = IntendedChangeStatus::Planned;
    assert!(!resumed.import_legacy_state_once(
        &notebook,
        &[],
        &HostedReconciliationFacts::default(),
    ));
    assert_eq!(
        resumed
            .graph
            .as_ref()
            .expect("graph")
            .node(&node_id)
            .expect("planned target node")
            .status,
        authoritative_status
    );
}

#[test]
fn duplicate_path_is_not_applied_without_node_specific_evidence() {
    let first = target("change-one", "src/shared.rs", "first responsibility");
    let second = target("change-two", "src/shared.rs", "second responsibility");
    let graph = graph(&[first, second]);
    let first_id = graph
        .nodes
        .iter()
        .find(|node| {
            node.target
                .as_ref()
                .is_some_and(|target| target.change_id == "change-one")
        })
        .expect("first node")
        .id
        .clone();
    let mut checkpoint = HostedOrchestrationCheckpoint {
        graph_revision: graph.revision,
        graph: Some(graph.clone()),
        ..HostedOrchestrationCheckpoint::default()
    };
    let notebook = notebook(ExecutionPhase::Implementation);
    assert!(checkpoint.import_legacy_state_once(
        &notebook,
        &["src/shared.rs".to_owned()],
        &HostedReconciliationFacts::default(),
    ));
    let observed_graph = checkpoint.graph.as_ref().expect("graph");
    assert_eq!(
        observed_graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_mutation() && node.status.is_success())
            .count(),
        0
    );

    let mut checkpoint = HostedOrchestrationCheckpoint {
        graph_revision: graph.revision,
        graph: Some(graph),
        domain_events: vec![ExecutionDomainEvent::MutationApplied {
            sequence: 1,
            node_id: first_id.clone(),
            target_path: "src/shared.rs".to_owned(),
            repository_fingerprint: "tree-1".to_owned(),
            evidence_id: "mutation-one".to_owned(),
            created_target_evidence: None,
        }],
        ..HostedOrchestrationCheckpoint::default()
    };
    assert!(checkpoint.import_legacy_state_once(
        &notebook,
        &["src/shared.rs".to_owned()],
        &HostedReconciliationFacts::default(),
    ));
    let graph = checkpoint.graph.as_ref().expect("graph");
    assert!(
        graph
            .node(&first_id)
            .is_some_and(|node| node.status.is_success())
    );
    assert_eq!(
        graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_mutation() && node.status.is_success())
            .count(),
        1
    );
}

#[test]
fn repaired_topology_preserves_unchanged_node_progress_and_budget() {
    let first = target("change-one", "src/one.rs", "production");
    let second = target("change-two", "src/two.rs", "production");
    let inserted = target("change-new", "src/new.rs", "production");
    let mut previous = graph(&[first.clone(), second.clone()]);
    let first_id = previous
        .nodes
        .iter()
        .find(|node| node.target.as_ref() == Some(&first))
        .expect("first node")
        .id
        .clone();
    let second_id = previous
        .nodes
        .iter()
        .find(|node| node.target.as_ref() == Some(&second))
        .expect("second node")
        .id
        .clone();
    let validation_id = previous
        .nodes
        .iter()
        .find(|node| node.kind.is_validation())
        .expect("validation node")
        .id
        .clone();
    let diff_review_id = ExecutionNodeId::new("diff-review");
    let completion_id = ExecutionNodeId::new("completion-evaluation");
    let publication_id = ExecutionNodeId::new("publication");
    previous
        .set_node_status(&first_id, ExecutionNodeStatus::Applied)
        .expect("apply first target");
    previous
        .set_node_status(&second_id, ExecutionNodeStatus::Applied)
        .expect("apply second target");
    previous
        .set_node_status(&validation_id, ExecutionNodeStatus::Passed)
        .expect("pass validation");
    previous
        .set_node_status(&diff_review_id, ExecutionNodeStatus::Completed)
        .expect("complete diff review");
    previous
        .set_node_status(&completion_id, ExecutionNodeStatus::Completed)
        .expect("complete completion evaluation");
    previous
        .set_node_status(&publication_id, ExecutionNodeStatus::Completed)
        .expect("complete publication");
    previous
        .node_mut(&first_id)
        .expect("first target")
        .attempts
        .push(NodeAttempt {
            attempt: 1,
            repository_fingerprint_before: "tree-0".to_owned(),
            repository_fingerprint_after: Some("tree-1".to_owned()),
            outcome: Some(ExecutionNodeStatus::Applied),
            ..NodeAttempt::default()
        });
    let mut replacement = graph(&[first, inserted, second]);
    let preserved = preserve_unchanged_graph_progress(&previous, &mut replacement);

    assert!(preserved.contains(&first_id));
    assert!(preserved.contains(&second_id));
    for invalidated in [
        &validation_id,
        &diff_review_id,
        &completion_id,
        &publication_id,
    ] {
        assert!(
            !preserved.contains(invalidated),
            "downstream node {invalidated} must be invalidated"
        );
        assert!(
            replacement
                .node(invalidated)
                .is_some_and(|node| !node.status.is_success()),
            "downstream node {invalidated} retained stale success"
        );
    }
    assert_eq!(
        replacement
            .node(&first_id)
            .expect("stable first node")
            .attempts
            .len(),
        1
    );
    assert!(
        replacement
            .node(&first_id)
            .is_some_and(|node| node.status == ExecutionNodeStatus::Applied)
    );

    let mut checkpoint = HostedOrchestrationCheckpoint {
        graph: Some(previous),
        budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
        ..HostedOrchestrationCheckpoint::default()
    };
    checkpoint
        .budget
        .record_model_call(first_id.clone(), 125, Duration::from_millis(10));
    retain_checkpoint_progress_for_nodes(&mut checkpoint, &preserved, &replacement);
    assert_eq!(
        checkpoint.budget.usage_for(&first_id).model_calls_consumed,
        1
    );
}

#[test]
fn materialization_clears_legacy_last_action_without_a_canonical_event() {
    let graph = graph(&[target("change-one", "src/one.rs", "production")]);
    let checkpoint = HostedOrchestrationCheckpoint {
        graph_revision: graph.revision,
        graph: Some(graph),
        ..HostedOrchestrationCheckpoint::default()
    };
    let mut notebook = notebook(ExecutionPhase::Implementation);
    notebook.last_successful_action = json!({"tool": "stale_legacy_action"});

    checkpoint.materialize_legacy_notebook(&mut notebook);

    assert_eq!(notebook.last_successful_action, json!({}));
    assert!(
        checkpoint
            .graph
            .as_ref()
            .expect("graph")
            .derived_collections()
            .applied_mutation_targets
            .is_empty(),
        "a legacy last_successful_action is not repository mutation evidence"
    );
}

#[test]
fn serialized_checkpoint_resume_selects_the_next_ready_node() {
    let first = target("change-one", "src/one.rs", "production");
    let second = target("change-two", "src/two.rs", "production");
    let mut graph = graph(&[first.clone(), second.clone()]);
    let first_id = graph
        .nodes
        .iter()
        .find(|node| node.target.as_ref() == Some(&first))
        .expect("first node")
        .id
        .clone();
    graph
        .set_node_status(&first_id, ExecutionNodeStatus::Applied)
        .expect("apply first target");
    let checkpoint = HostedOrchestrationCheckpoint {
        graph_revision: graph.revision,
        graph: Some(graph),
        ..HostedOrchestrationCheckpoint::default()
    };
    let encoded = serde_json::to_vec(&checkpoint).expect("serialize checkpoint");
    let restored: HostedOrchestrationCheckpoint =
        serde_json::from_slice(&encoded).expect("deserialize checkpoint");
    let snapshot = restored.snapshot(
        "run-resumed",
        RepositorySnapshot {
            fingerprint: "tree-1".to_owned(),
            changed_paths: BTreeSet::from([first.path]),
            ..RepositorySnapshot::default()
        },
    );
    assert!(matches!(
        reconcile_execution(&snapshot).expect("resume decision"),
        ExecutionDecision::ExecuteTarget { target, .. }
            if target.target.path == second.path
    ));
}

#[test]
fn newer_attempt_clears_cancellation_through_the_event_reducer() {
    let target = target("change-one", "src/one.rs", "production");
    let graph = graph(std::slice::from_ref(&target));
    let mut checkpoint = HostedOrchestrationCheckpoint {
        graph_revision: graph.revision,
        graph: Some(graph),
        legacy_import_completed: true,
        budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
        cancellation: Some(CancellationState {
            requested_at: "attempt-1".to_owned(),
            reason: "user requested cancellation".to_owned(),
            checkpointed: true,
            ..CancellationState::default()
        }),
        ..HostedOrchestrationCheckpoint::default()
    };
    let repository = RepositorySnapshot {
        fingerprint: "tree-1".to_owned(),
        ..RepositorySnapshot::default()
    };

    assert!(
        checkpoint
            .resume_for_new_attempt("run-1", repository.clone(), 1, 1)
            .expect("same attempt remains cancelled")
            .is_none()
    );
    assert!(checkpoint.cancellation.is_some());
    assert_eq!(
        checkpoint
            .resume_for_new_attempt("run-1", repository.clone(), 1, 2)
            .expect("newer attempt resumes"),
        Some(HostedResumeReason::Cancellation)
    );
    assert!(checkpoint.cancellation.is_none());
    assert!(matches!(
        checkpoint.domain_events.last(),
        Some(ExecutionDomainEvent::ExecutionResumed {
            execution_attempt: 2,
            previous_outcome: None,
            ..
        })
    ));
    assert!(matches!(
        reconcile_execution(&checkpoint.snapshot("run-1", repository))
            .expect("resumed graph decision"),
        ExecutionDecision::ExecuteTarget { target: context, .. }
            if context.target == target
    ));
}

#[test]
fn newer_attempt_reopens_remaining_work_after_a_published_partial_result() {
    let first = target("change-one", "src/one.rs", "production");
    let second = target("change-two", "src/two.rs", "production");
    let mut graph = graph(&[first.clone(), second.clone()]);
    let first_id = graph
        .nodes
        .iter()
        .find(|node| node.target.as_ref() == Some(&first))
        .expect("first target")
        .id
        .clone();
    let second_id = graph
        .nodes
        .iter()
        .find(|node| node.target.as_ref() == Some(&second))
        .expect("second target")
        .id
        .clone();
    graph
        .set_node_status(&first_id, ExecutionNodeStatus::Applied)
        .expect("apply first target");
    graph
        .dependency_satisfaction_overrides
        .insert(second_id.clone());
    graph.refresh_readiness();
    let validation_ids = graph
        .nodes
        .iter()
        .filter(|node| node.kind.is_validation())
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    for node_id in validation_ids {
        graph
            .set_node_status(&node_id, ExecutionNodeStatus::Passed)
            .expect("pass partial validation");
    }
    let diff_id = graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::DiffReview)
        .expect("diff node")
        .id
        .clone();
    graph
        .set_node_status(&diff_id, ExecutionNodeStatus::Completed)
        .expect("complete partial diff review");
    let completion_id = graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
        .expect("completion node")
        .id
        .clone();
    graph
        .set_node_status(&completion_id, ExecutionNodeStatus::Completed)
        .expect("complete partial evaluation");
    let publication_id = graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::Publication)
        .expect("publication node")
        .id
        .clone();
    graph
        .set_node_status(&publication_id, ExecutionNodeStatus::Completed)
        .expect("publish partial result");
    let events = vec![
        ExecutionDomainEvent::GuardrailTriggered {
            sequence: 1,
            reason: crate::execution_graph::GuardrailReason::NodeBudgetExhausted,
            outcome: MissionOutcome::PartialReviewable,
            detail: "useful partial work".to_owned(),
        },
        ExecutionDomainEvent::CompletionEvaluated {
            sequence: 2,
            node_id: completion_id.clone(),
            outcome: MissionOutcome::PartialReviewable,
        },
        ExecutionDomainEvent::RunFinished {
            sequence: 3,
            outcome: MissionOutcome::PartialReviewable,
        },
    ];
    let mut checkpoint = HostedOrchestrationCheckpoint {
        graph_revision: graph.revision,
        graph: Some(graph),
        legacy_import_completed: true,
        domain_events: events,
        budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
        publication: PublicationState {
            status: PublicationStatus::PullRequestCreated,
            mode: Some(crate::execution_graph::PublicationMode::Draft),
            commit_sha: Some("partial-commit".to_owned()),
            branch: Some("rustgrid/partial".to_owned()),
            pull_request_url: Some("https://example.test/pull/7".to_owned()),
            pull_request_number: Some(7),
            draft: true,
            recovery_requested: false,
        },
        ..HostedOrchestrationCheckpoint::default()
    };
    let repository = RepositorySnapshot {
        fingerprint: "tree-1".to_owned(),
        changed_paths: BTreeSet::from([first.path.clone()]),
        ..RepositorySnapshot::default()
    };

    assert_eq!(
        checkpoint
            .resume_for_new_attempt("run-partial", repository.clone(), 1, 2)
            .expect("resume published partial"),
        Some(HostedResumeReason::PartialReviewable)
    );
    let mut resumed = checkpoint.snapshot("run-partial", repository);
    assert_eq!(resumed.terminal_outcome(), None);
    assert!(resumed.graph.dependency_satisfaction_overrides.is_empty());
    assert_eq!(
        resumed.graph.node(&first_id).map(|node| node.status),
        Some(ExecutionNodeStatus::Applied)
    );
    assert_eq!(
        resumed.graph.node(&second_id).map(|node| node.status),
        Some(ExecutionNodeStatus::Ready)
    );
    assert_eq!(resumed.publication.status, PublicationStatus::NotStarted);
    assert_eq!(
        resumed.publication.pull_request_number,
        Some(7),
        "existing draft PR identity remains available for update"
    );
    assert!(matches!(
        reconcile_execution(&resumed).expect("continue remaining partial work"),
        ExecutionDecision::ExecuteTarget { target, .. }
            if target.target == second
    ));

    assert!(crate::execution_graph::current_execution_epoch(&resumed.events).is_empty());
    resumed
        .append_event(ExecutionDomainEvent::GuardrailTriggered {
            sequence: 5,
            reason: crate::execution_graph::GuardrailReason::NodeBudgetExhausted,
            outcome: MissionOutcome::PartialReviewable,
            detail: "second useful partial epoch".to_owned(),
        })
        .expect("the resumed epoch records its own partial handoff");
    assert_eq!(
        crate::execution_graph::current_execution_epoch(&resumed.events)
            .iter()
            .filter(|event| matches!(event, ExecutionDomainEvent::GuardrailTriggered { .. }))
            .count(),
        1
    );
    let validation_id = resumed
        .graph
        .nodes
        .iter()
        .find(|node| node.kind.is_validation())
        .expect("validation node")
        .id
        .clone();
    let validation_gate = resumed
        .graph
        .node(&validation_id)
        .and_then(|node| node.validation.clone())
        .expect("validation gate");
    let validation_fingerprint =
        validation_gate.fingerprint(&resumed.current_repository.fingerprint);
    resumed
        .append_event(ExecutionDomainEvent::ValidationStarted {
            sequence: 6,
            node_id: validation_id.clone(),
            fingerprint: validation_fingerprint.clone(),
        })
        .expect("restart validation in resumed epoch");
    resumed
        .append_event(ExecutionDomainEvent::ValidationEvidenceRecorded {
            sequence: 7,
            node_id: validation_id.clone(),
            evidence: ValidationEvidenceRecord {
                evidence_id: "second-validation".to_owned(),
                node_id: validation_id.clone(),
                gate_id: validation_gate.gate_id,
                fingerprint: validation_fingerprint.clone(),
                repository_fingerprint: resumed.current_repository.fingerprint.clone(),
                command: validation_gate.command,
                working_directory: validation_gate.working_directory,
                status: ValidationEvidenceStatus::Passed,
                exit_code: Some(0),
                output_summary: "resumed validation passed".to_owned(),
                duration: Duration::from_millis(1),
            },
        })
        .expect("record resumed validation evidence");
    resumed
        .append_event(ExecutionDomainEvent::ValidationPassed {
            sequence: 8,
            node_id: validation_id,
            evidence_id: "second-validation".to_owned(),
            fingerprint: validation_fingerprint,
        })
        .expect("pass validation in resumed epoch");
    resumed
        .append_event(ExecutionDomainEvent::DiffReviewed {
            sequence: 9,
            node_id: diff_id,
            evidence_ids: vec!["second-validation".to_owned()],
        })
        .expect("review resumed partial diff");
    resumed
        .append_event(ExecutionDomainEvent::CompletionEvaluated {
            sequence: 10,
            node_id: completion_id,
            outcome: MissionOutcome::PartialReviewable,
        })
        .expect("evaluate resumed partial completion");
    resumed
        .append_event(ExecutionDomainEvent::PublicationStarted {
            sequence: 11,
            node_id: publication_id.clone(),
            mode: crate::execution_graph::PublicationMode::Draft,
        })
        .expect("restart draft publication");
    resumed
        .append_event(ExecutionDomainEvent::CommitCreated {
            sequence: 12,
            node_id: publication_id.clone(),
            commit_sha: "second-partial-commit".to_owned(),
        })
        .expect("record resumed commit");
    resumed
        .append_event(ExecutionDomainEvent::BranchPushed {
            sequence: 13,
            node_id: publication_id.clone(),
            branch: "rustgrid/partial".to_owned(),
        })
        .expect("push resumed partial");
    resumed
        .append_event(ExecutionDomainEvent::PullRequestCreated {
            sequence: 14,
            node_id: publication_id,
            url: "https://example.test/pull/7".to_owned(),
            number: Some(7),
            draft: true,
        })
        .expect("update resumed draft pull request");
    resumed
        .append_event(ExecutionDomainEvent::RunFinished {
            sequence: 15,
            outcome: MissionOutcome::PartialReviewable,
        })
        .expect("the resumed epoch can finish independently");
    assert_eq!(
        resumed.terminal_outcome(),
        Some(MissionOutcome::PartialReviewable)
    );
    assert_eq!(
        resumed
            .events
            .iter()
            .filter(|event| matches!(event, ExecutionDomainEvent::RunFinished { .. }))
            .count(),
        2
    );
}
