    #[test]
    fn default_complexity_envelopes_are_exact() {
        let cases = [
            (MissionComplexity::Tiny, 2_000_000, 14, 8, 1),
            (MissionComplexity::Small, 5_000_000, 25, 15, 2),
            (MissionComplexity::Medium, 10_000_000, 45, 35, 3),
            (MissionComplexity::Large, 20_000_000, 80, 75, 4),
        ];
        for (complexity, cost, calls, minutes, repairs) in cases {
            let budget = MissionBudget::for_complexity(complexity);
            assert_eq!(budget.max_cost_micros, cost);
            assert_eq!(budget.max_model_calls, calls);
            assert_eq!(budget.max_duration, Duration::from_secs(minutes * 60));
            assert_eq!(budget.max_target_repair_rounds, repairs);
        }
    }

    fn one_call_budget() -> (BudgetState, ExecutionNodeId, NodeBudget) {
        (
            BudgetState::new(MissionBudget {
                max_model_calls: 1,
                max_cost_micros: 1_000,
                max_duration: Duration::from_secs(10),
                max_target_repair_rounds: 0,
            }),
            ExecutionNodeId::new("discovery"),
            NodeBudget {
                max_model_calls: 1,
                max_cost_micros: 1_000,
                max_duration: Duration::from_secs(10),
                max_repair_attempts: 0,
            },
        )
    }

    #[test]
    fn first_call_is_admitted_when_maximum_is_one_and_usage_is_zero() {
        let (state, node_id, node_budget) = one_call_budget();
        let admission =
            state.evaluate_model_call_admission(&node_id, &node_budget, 1, 100, Duration::ZERO);
        assert!(admission.admitted);
        assert_eq!(admission.consumed_calls, 0);
        assert_eq!(admission.reserved_calls, 0);
    }

    #[test]
    fn second_call_is_rejected_after_the_only_call_is_consumed() {
        let (mut state, node_id, node_budget) = one_call_budget();
        let reservation = state
            .reserve_model_call(&node_id, &node_budget, 100, Duration::ZERO)
            .expect("first call reservation");
        state.consume_model_call_reservation(&reservation, 75, Duration::from_millis(5));
        let rejected = state
            .reserve_model_call(&node_id, &node_budget, 100, Duration::ZERO)
            .expect_err("second call must exceed the maximum");
        assert_eq!(
            rejected.rejection_reason,
            Some("node_model_call_budget_exhausted")
        );
        assert_eq!(rejected.consumed_calls, 1);
        assert_eq!(rejected.reserved_calls, 0);
    }

    #[test]
    fn active_reservation_prevents_a_duplicate_concurrent_call() {
        let (mut state, node_id, node_budget) = one_call_budget();
        let _reservation = state
            .reserve_model_call(&node_id, &node_budget, 100, Duration::ZERO)
            .expect("first active reservation");
        let rejected = state
            .reserve_model_call(&node_id, &node_budget, 100, Duration::ZERO)
            .expect_err("active reservation must occupy the remaining call slot");
        assert_eq!(rejected.consumed_calls, 0);
        assert_eq!(rejected.reserved_calls, 1);
    }

    #[test]
    fn failed_provider_contact_releases_without_consuming_the_reservation() {
        let (mut state, node_id, node_budget) = one_call_budget();
        let reservation = state
            .reserve_model_call(&node_id, &node_budget, 100, Duration::ZERO)
            .expect("provider call reservation");
        state.release_model_call_reservation(&reservation);
        let usage = state.usage_for(&node_id);
        assert_eq!(usage.model_calls_reserved, 0);
        assert_eq!(usage.model_calls_consumed, 0);
        assert_eq!(state.total_model_calls_reserved, 0);
        assert_eq!(state.total_model_calls, 0);
        assert!(state.can_spend_model_call(&node_id, &node_budget, 100, Duration::ZERO));
    }

    #[test]
    fn reservation_reconciliation_does_not_double_count_the_call_budget() {
        let (mut state, node_id, node_budget) = one_call_budget();
        let reservation = state
            .reserve_model_call(&node_id, &node_budget, 100, Duration::ZERO)
            .expect("provider call reservation");
        assert_eq!(state.total_model_calls, 0);
        assert_eq!(state.total_model_calls_reserved, 1);
        state.consume_model_call_reservation(&reservation, 80, Duration::from_millis(5));
        assert_eq!(state.total_model_calls, 1);
        assert_eq!(state.total_model_calls_reserved, 0);
        let usage = state.usage_for(&node_id);
        assert_eq!(usage.model_calls_consumed, 1);
        assert_eq!(usage.model_calls_reserved, 0);
    }

    #[test]
    fn legacy_model_call_usage_restores_as_consumed_without_an_active_reservation() {
        let usage: NodeBudgetUsage = serde_json::from_value(serde_json::json!({
            "model_calls": 2,
            "cost_micros": 125,
            "duration": 5,
            "repair_attempts": 0
        }))
        .expect("legacy node usage");
        assert_eq!(usage.model_calls_consumed, 2);
        assert_eq!(usage.model_calls_reserved, 0);
        assert_eq!(usage.cost_micros_reserved, 0);
    }

    #[test]
    fn bootstrap_graph_assigns_only_the_bounded_discovery_and_planning_budgets() {
        let mission = MissionBudget::for_complexity(MissionComplexity::Tiny);
        let graph =
            ExecutionGraph::bootstrap("bootstrap", "tree", MissionComplexity::Tiny, &mission);
        let discovery = graph.node(&ExecutionNodeId::new("discovery")).unwrap();
        let planning = graph.node(&ExecutionNodeId::new("planning")).unwrap();
        assert_eq!(discovery.budget.max_model_calls, 3);
        assert_eq!(discovery.budget.max_cost_micros, 350_000);
        assert_eq!(
            discovery.budget.max_duration,
            Duration::from_millis(120_000)
        );
        assert_eq!(planning.budget.max_model_calls, 2);
        assert_eq!(planning.budget.max_cost_micros, 300_000);
        assert_eq!(planning.budget.max_duration, Duration::from_millis(90_000));
        assert!(graph.nodes.iter().all(|node| matches!(
            node.kind,
            ExecutionNodeKind::Discovery | ExecutionNodeKind::Planning
        )));
    }

    #[test]
    fn incoherent_bootstrap_call_and_cost_budget_fails_graph_validation() {
        let mission = MissionBudget::for_complexity(MissionComplexity::Tiny);
        let mut graph =
            ExecutionGraph::bootstrap("bootstrap", "tree", MissionComplexity::Tiny, &mission);
        graph
            .node_mut(&ExecutionNodeId::new("discovery"))
            .unwrap()
            .budget
            .max_cost_micros = 219_999;
        let error = graph.validate_invariants().unwrap_err();
        assert!(error.message.contains("budget_configuration_invalid"));
        assert!(error.message.contains("minimum_viable_node_cost=220000"));

        let mut graph =
            ExecutionGraph::bootstrap("bootstrap", "tree", MissionComplexity::Tiny, &mission);
        graph
            .node_mut(&ExecutionNodeId::new("planning"))
            .unwrap()
            .budget
            .max_cost_micros = 259_999;
        let error = graph.validate_invariants().unwrap_err();
        assert!(error.message.contains("budget_configuration_invalid"));
        assert!(error.message.contains("minimum_viable_node_cost=260000"));
    }

    #[test]
    fn policy_overrides_do_not_change_the_classification() {
        let input = ComplexityInput {
            planned_target_count: 5,
            ..ComplexityInput::default()
        };
        let assessment = ComplexityAssessment::classify_with_policy(
            &input,
            &MissionBudgetOverride {
                max_model_calls: Some(30),
                max_cost_micros: Some(4_500_000),
                ..MissionBudgetOverride::default()
            },
        );
        assert_eq!(assessment.class, MissionComplexity::Small);
        assert_eq!(assessment.budget.max_model_calls, 30);
        assert_eq!(assessment.budget.max_cost_micros, 4_500_000);
    }

    #[test]
    fn accepted_plan_builds_the_mandatory_dependency_chain() {
        let graph = graph();
        graph.validate_invariants().expect("valid graph");

        let source = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source node");
        let test = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::TestMutation)
            .expect("test node");
        let focused = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::ValidationFocused)
            .expect("focused node");
        let suite = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::ValidationSuite)
            .expect("suite node");
        let build = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::ValidationBuild)
            .expect("build node");
        let review = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::DiffReview)
            .expect("review node");
        let completion = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .expect("completion node");
        let publication = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Publication)
            .expect("publication node");

        assert_eq!(test.dependencies, vec![source.id.clone()]);
        assert_eq!(focused.dependencies, vec![test.id.clone()]);
        assert_eq!(suite.dependencies, vec![focused.id.clone()]);
        assert_eq!(build.dependencies, vec![suite.id.clone()]);
        assert_eq!(review.dependencies, vec![build.id.clone()]);
        assert_eq!(completion.dependencies, vec![review.id.clone()]);
        assert_eq!(publication.dependencies, vec![completion.id.clone()]);
        assert_eq!(
            graph.next_runnable_node().map(|node| &node.id),
            Some(&source.id)
        );
    }

    #[test]
    fn scrambled_validation_input_builds_one_canonical_dependency_chain() {
        let scrambled = vec![
            gate("lint-z", ValidationGateType::Lint),
            gate("build", ValidationGateType::Build),
            gate("suite-z", ValidationGateType::TestSuite),
            gate("focused-z", ValidationGateType::FocusedTest),
            gate("custom", ValidationGateType::Custom),
            gate("typecheck-a", ValidationGateType::Typecheck),
            gate("focused-a", ValidationGateType::FocusedTest),
            gate("suite-a", ValidationGateType::TestSuite),
        ];
        let build = |gates: &[ValidationGateSpec]| {
            ExecutionGraph::from_targets(
                "graph-scrambled-validation",
                MissionComplexity::Small,
                "tree-1",
                &[target("src/theme.ts", "production")],
                gates,
                &MissionBudget::for_complexity(MissionComplexity::Small),
            )
        };
        let graph = build(&scrambled);
        graph.validate_invariants().expect("canonical graph");
        let validation_nodes = graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_validation())
            .collect::<Vec<_>>();
        assert_eq!(
            validation_nodes
                .iter()
                .map(|node| {
                    node.validation
                        .as_ref()
                        .expect("validation gate")
                        .gate_id
                        .as_str()
                })
                .collect::<Vec<_>>(),
            vec![
                "focused-a",
                "focused-z",
                "lint-z",
                "typecheck-a",
                "suite-a",
                "suite-z",
                "build",
                "custom",
            ]
        );
        for pair in validation_nodes.windows(2) {
            assert_eq!(pair[1].dependencies, vec![pair[0].id.clone()]);
        }

        let mut reversed = scrambled;
        reversed.reverse();
        let reversed_graph = build(&reversed);
        let canonical_projection = |graph: &ExecutionGraph| {
            graph
                .nodes
                .iter()
                .filter(|node| node.kind.is_validation())
                .map(|node| {
                    (
                        node.id.clone(),
                        node.dependencies.clone(),
                        node.validation.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            canonical_projection(&graph),
            canonical_projection(&reversed_graph),
            "equivalent gate sets must not produce manifest-order-dependent topology"
        );
    }

    #[test]
    fn validation_process_budgets_are_independent_from_node_scheduling_budgets() {
        let suite =
            ValidationNodeBudget::for_gate(ValidationGateType::TestSuite, Duration::from_secs(74));
        assert_eq!(suite.scheduling_deadline, Duration::from_secs(74));
        assert_eq!(
            suite.process_timeout.execution_timeout,
            Duration::from_secs(240)
        );
        assert_eq!(
            suite.process_timeout.absolute_timeout,
            Duration::from_secs(300)
        );
        assert_eq!(
            suite.retry_policy,
            ValidationRetryPolicy::TransientInfrastructureOnce
        );

        let focused = ValidationTimeoutPolicy::for_gate(ValidationGateType::FocusedTest);
        assert_eq!(focused.execution_timeout, Duration::from_secs(90));
        assert_eq!(focused.absolute_timeout, Duration::from_secs(120));
        let install = ValidationTimeoutPolicy::dependency_install();
        assert_eq!(install.execution_timeout, Duration::from_secs(300));
        assert_eq!(install.absolute_timeout, Duration::from_secs(360));
    }

    #[test]
    fn remaining_work_is_exactly_the_pending_required_graph_nodes() {
        let mut graph = graph();
        let expected = graph
            .nodes
            .iter()
            .filter(|node| node.required && !node.status.is_success())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            graph
                .remaining_required_nodes()
                .iter()
                .map(|node| node.id.clone())
                .collect::<Vec<_>>(),
            expected
        );

        let first_target = graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .expect("first mutation")
            .id
            .clone();
        graph
            .set_node_status(&first_target, ExecutionNodeStatus::Applied)
            .expect("apply first target");
        let remaining = graph.remaining_required_nodes();
        assert!(!remaining.iter().any(|node| node.id == first_target));
        assert!(
            remaining
                .iter()
                .all(|node| node.required && !node.status.is_success())
        );
    }

    #[test]
    fn fresh_bootstrap_graph_has_only_orchestration_remaining_work() {
        let graph = ExecutionGraph::bootstrap(
            "graph-bootstrap",
            "tree-clean",
            MissionComplexity::Tiny,
            &MissionBudget::for_complexity(MissionComplexity::Tiny),
        );

        graph.validate_invariants().expect("valid fresh graph");
        let collections = graph.derived_collections();
        assert_eq!(
            collections.remaining_graph_nodes,
            BTreeSet::from([
                ExecutionNodeId::new("discovery"),
                ExecutionNodeId::new("planning"),
            ])
        );
        assert!(collections.remaining_mutation_targets.is_empty());
        assert!(collections.applied_mutation_targets.is_empty());
        assert!(collections.completed_validation_nodes.is_empty());
        assert_eq!(
            graph.node_by_str("discovery").map(|node| node.status),
            Some(ExecutionNodeStatus::Ready)
        );
        assert_eq!(
            graph.node_by_str("planning").map(|node| node.status),
            Some(ExecutionNodeStatus::Pending)
        );
        assert_eq!(
            graph
                .node_by_str("planning")
                .map(|node| node.dependencies.clone()),
            Some(vec![ExecutionNodeId::new("discovery")])
        );
    }

    #[test]
    fn applied_mutation_targets_are_excluded_from_remaining_mutation_targets() {
        let mut graph = graph();
        let mutation_id = graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .expect("mutation node")
            .id
            .clone();
        let target_id = graph
            .node(&mutation_id)
            .and_then(|node| node.target.as_ref())
            .expect("mutation target")
            .mutation_target_id();

        graph
            .set_node_status(&mutation_id, ExecutionNodeStatus::Applied)
            .expect("apply mutation");
        let collections = graph.derived_collections();
        assert!(collections.applied_mutation_targets.contains(&target_id));
        assert!(!collections.remaining_mutation_targets.contains(&target_id));
    }

    #[test]
    fn passed_validation_has_a_typed_completed_validation_identity() {
        let mut graph = graph();
        let validation_id = graph
            .nodes
            .iter()
            .find(|node| node.kind.is_validation())
            .expect("validation node")
            .id
            .clone();
        graph
            .node_mut(&validation_id)
            .expect("validation node")
            .status = ExecutionNodeStatus::Passed;

        assert_eq!(
            graph.derived_collections().completed_validation_nodes,
            BTreeSet::from([ValidationNodeId::new(validation_id.as_str())])
        );
    }

    #[test]
    fn mutation_overlap_diagnostic_names_invariant_and_typed_state() {
        let mut graph = graph();
        let mutation_index = graph
            .nodes
            .iter()
            .position(|node| node.kind.is_mutation())
            .expect("mutation node");
        let mut duplicate = graph.nodes[mutation_index].clone();
        duplicate.id = ExecutionNodeId::new("duplicate-mutation-target");
        duplicate.status = ExecutionNodeStatus::Pending;
        graph.nodes[mutation_index].status = ExecutionNodeStatus::Applied;
        graph.nodes.push(duplicate);

        let error = graph.validate_invariants().expect_err("overlap must fail");
        assert!(
            error
                .message
                .contains("invariant=applied_mutation_target_excluded_from_remaining")
        );
        assert!(error.message.contains("kind=SourceMutation"));
        assert!(error.message.contains("status=Applied"));
        assert!(error.message.contains("status=Pending"));
        assert!(error.message.contains("remaining_mutation_target_ids="));
        assert!(error.message.contains("applied_mutation_target_ids="));
    }

    #[test]
    fn graph_created_event_does_not_create_an_applied_mutation_target() {
        let graph = ExecutionGraph::bootstrap(
            "graph-created-fixture",
            "tree-clean",
            MissionComplexity::Tiny,
            &MissionBudget::for_complexity(MissionComplexity::Tiny),
        );
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-created-fixture".to_owned(),
            graph: graph.clone(),
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Tiny)),
            ..ExecutionSnapshot::default()
        };
        snapshot
            .append_event(ExecutionDomainEvent::GraphCreated {
                sequence: 1,
                graph_id: graph.graph_id.clone(),
                revision: graph.revision,
                graph: Some(graph),
                preserved_node_ids: Vec::new(),
            })
            .expect("graph-created event");

        assert!(
            snapshot
                .graph
                .derived_collections()
                .applied_mutation_targets
                .is_empty()
        );
    }

    #[test]
    fn graph_ids_and_serialization_are_deterministic() {
        let first = graph();
        let second = graph();
        assert_eq!(first, second);
        let encoded = serde_json::to_string(&first).expect("serialize graph");
        let decoded: ExecutionGraph = serde_json::from_str(&encoded).expect("deserialize graph");
        assert_eq!(decoded, first);
    }

    #[test]
    fn evidence_cache_reuses_only_compatible_content() {
        let mut evidence = EvidenceStore::default();
        let range = LineRange::new(10, 30).expect("range");
        let id = evidence.capture_file("src/lib.rs", "tree-1", Some(range), "bounded", true);
        assert_eq!(
            evidence
                .reusable_file("src/lib.rs", "tree-1", LineRange::new(15, 20),)
                .map(|entry| entry.evidence_id.as_str()),
            Some(id.as_str())
        );
        assert!(
            evidence
                .reusable_file("src/lib.rs", "tree-1", None)
                .is_none(),
            "a truncated excerpt cannot satisfy a full-file read"
        );
        assert!(
            evidence
                .reusable_file("src/lib.rs", "tree-2", LineRange::new(15, 20),)
                .is_none(),
            "repository changes invalidate cached reads"
        );
        assert_eq!(evidence.record_file(evidence.files[&id].clone()), id);
        assert_eq!(evidence.files.len(), 1, "duplicate reads are deduplicated");
    }

    #[test]
    fn applied_target_supersedes_failures_and_unblocks_the_store() {
        let node_id = ExecutionNodeId::new("target-1");
        let failure = FailureRecord {
            id: FailureId::new("failure-1"),
            node_id: node_id.clone(),
            target_path: Some("src/theme.ts".to_owned()),
            category: FailureCategory::MutationConflict,
            status: FailureStatus::Active,
            attempt: 1,
            repository_fingerprint: "tree-1".to_owned(),
            message: "replace text did not match".to_owned(),
            ..FailureRecord::default()
        };
        let mut failures = FailureStore::default();
        failures.record(failure);
        assert!(failures.has_unresolved_for_node(&node_id));
        assert_eq!(
            failures.supersede_for_applied_target(&node_id, "src/theme.ts", "tree-2"),
            vec![FailureId::new("failure-1")]
        );
        assert!(!failures.has_unresolved());
        assert_eq!(
            failures
                .get(&FailureId::new("failure-1"))
                .map(|failure| failure.status),
            Some(FailureStatus::Superseded)
        );
    }

    #[test]
    fn applied_target_does_not_supersede_non_mutation_failures() {
        let node_id = ExecutionNodeId::new("target-1");
        let preserved = [
            FailureCategory::ModelArtifactRecoverable,
            FailureCategory::TargetBlocked,
            FailureCategory::ValidationFailure,
            FailureCategory::InfrastructureFailure,
            FailureCategory::OrchestrationInvariantViolation,
            FailureCategory::UserCancellation,
        ];
        let mut failures = FailureStore::default();
        for (index, category) in preserved.into_iter().enumerate() {
            let mut failure = FailureRecord::new(
                format!("failure-{index}"),
                node_id.clone(),
                category,
                1,
                "tree-1",
                "must remain explicit",
            );
            failure.target_path = Some("src/theme.ts".to_owned());
            failures.record(failure);
        }

        assert!(
            failures
                .supersede_for_applied_target(&node_id, "src/theme.ts", "tree-2")
                .is_empty()
        );
        assert_eq!(failures.unresolved().count(), preserved.len());
    }

    #[test]
    fn failure_event_stream_replays_graph_store_and_progress_exactly() {
        let initial_graph = graph();
        let source = initial_graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source mutation")
            .clone();
        let test = initial_graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::TestMutation)
            .expect("test mutation")
            .clone();
        let initial = ExecutionSnapshot {
            run_id: "run-event-replay".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                source_tree_hash: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: initial_graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        let mut persisted = initial.clone();
        persisted
            .append_event(ExecutionDomainEvent::NodeStarted {
                sequence: 1,
                node_id: source.id.clone(),
                attempt: 1,
                started_at: "attempt-1".to_owned(),
                repository_fingerprint: "tree-1".to_owned(),
            })
            .expect("start mutation");
        let mut mutation_failure = FailureRecord::new(
            "mutation-failure",
            source.id.clone(),
            FailureCategory::MutationConflict,
            1,
            "tree-1",
            "replacement no longer matched",
        );
        mutation_failure.target_path = source.target.as_ref().map(|target| target.path.clone());
        persisted
            .append_event(ExecutionDomainEvent::MutationRejected {
                sequence: 2,
                node_id: source.id.clone(),
                failure: mutation_failure,
            })
            .expect("record mutation rejection");
        persisted
            .append_event(ExecutionDomainEvent::FailureSuperseded {
                sequence: 3,
                node_id: source.id.clone(),
                failure_id: FailureId::new("mutation-failure"),
                repository_fingerprint: "tree-2".to_owned(),
            })
            .expect("supersede mutation failure from final state");

        let mut infrastructure_failure = FailureRecord::new(
            "infrastructure-failure",
            test.id.clone(),
            FailureCategory::InfrastructureFailure,
            1,
            "tree-2",
            "repository transport unavailable",
        );
        infrastructure_failure.target_path = test.target.as_ref().map(|target| target.path.clone());
        persisted
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 4,
                failure: infrastructure_failure,
            })
            .expect("record infrastructure failure");

        let encoded = serde_json::to_string(&persisted.events).expect("serialize event stream");
        let replay_events: Vec<ExecutionDomainEvent> =
            serde_json::from_str(&encoded).expect("deserialize event stream");
        let mut replayed = initial;
        for event in replay_events {
            replayed.append_event(event).expect("replay event");
        }

        assert_eq!(replayed.events, persisted.events);
        assert_eq!(replayed.graph, persisted.graph);
        assert_eq!(replayed.failures, persisted.failures);
        assert_eq!(
            replayed.budget.progress_events,
            persisted.budget.progress_events
        );
        assert_eq!(
            replayed
                .failures
                .get(&FailureId::new("mutation-failure"))
                .map(|failure| failure.status),
            Some(FailureStatus::Superseded)
        );
        assert_eq!(
            replayed.graph.node(&source.id).map(|node| node.status),
            Some(ExecutionNodeStatus::Superseded)
        );
        assert_eq!(
            replayed.graph.node(&test.id).map(|node| node.status),
            Some(ExecutionNodeStatus::FailedBlocking)
        );
        assert_eq!(
            replayed
                .failures
                .get(&FailureId::new("infrastructure-failure"))
                .map(|failure| failure.category),
            Some(FailureCategory::InfrastructureFailure)
        );
        assert!(replayed.budget.progress_events.iter().any(|progress| {
            progress.sequence == 3
                && progress.kind == ProgressEventKind::FailureSuperseded
                && progress.node_id.as_ref() == Some(&source.id)
        }));
    }

    #[test]
    fn evidence_and_topology_events_replay_without_deleting_history() {
        let initial_graph = graph();
        let source = initial_graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source mutation")
            .clone();
        let validation_node = initial_graph
            .nodes
            .iter()
            .find(|node| {
                node.validation
                    .as_ref()
                    .is_some_and(|gate| gate.gate_id == "suite")
            })
            .expect("stable suite validation")
            .clone();
        let validation_fingerprint = validation_node
            .validation
            .as_ref()
            .expect("suite gate")
            .fingerprint("tree-1");
        let mut initial = ExecutionSnapshot {
            run_id: "run-topology-replay".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                source_tree_hash: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: initial_graph.clone(),
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            publication: PublicationState {
                status: PublicationStatus::BranchPushed,
                branch: Some("rustgrid/replay".to_owned()),
                ..PublicationState::default()
            },
            ..ExecutionSnapshot::default()
        };
        initial
            .evidence
            .record_validation(ValidationEvidenceRecord {
                evidence_id: "stale-suite-evidence".to_owned(),
                node_id: validation_node.id.clone(),
                gate_id: "suite".to_owned(),
                fingerprint: validation_fingerprint,
                repository_fingerprint: "tree-1".to_owned(),
                command: "run suite".to_owned(),
                working_directory: ".".to_owned(),
                status: ValidationEvidenceStatus::Failed,
                output_summary: "old topology failure".to_owned(),
                ..ValidationEvidenceRecord::default()
            });
        initial
            .budget
            .record_model_call(validation_node.id.clone(), 75, Duration::from_millis(5));
        initial.budget.record_progress_kind(
            0,
            ProgressEventKind::NodeMadeReady,
            Some(validation_node.id.clone()),
        );
        let repository_evidence =
            FileEvidence::capture("src/theme.ts", "tree-1", None, "export {};\n", false);
        let repository_evidence_id = repository_evidence.evidence_id.clone();
        let mut persisted = initial.clone();
        persisted
            .append_event(ExecutionDomainEvent::RepositoryEvidenceRecorded {
                sequence: 1,
                evidence_id: repository_evidence_id.clone(),
                repository_fingerprint: "tree-1".to_owned(),
                evidence: Some(repository_evidence),
            })
            .expect("record repository evidence");
        persisted
            .append_event(ExecutionDomainEvent::MutationApplied {
                sequence: 2,
                node_id: source.id.clone(),
                target_path: "src/theme.ts".to_owned(),
                repository_fingerprint: "tree-2".to_owned(),
                evidence_id: "mutation-theme-tree-2".to_owned(),
            })
            .expect("record mutation evidence");
        assert!(
            persisted
                .evidence
                .records
                .contains_key("mutation-theme-tree-2")
        );

        let mut replacement = ExecutionGraph::from_targets(
            "graph-1",
            MissionComplexity::Small,
            "tree-2",
            &[target("src/replacement.ts", "production")],
            &[gate("suite", ValidationGateType::TestSuite)],
            &MissionBudget::for_complexity(MissionComplexity::Small),
        );
        replacement.revision = initial_graph.revision.saturating_add(1);
        persisted
            .append_event(ExecutionDomainEvent::GraphCreated {
                sequence: 3,
                graph_id: replacement.graph_id.clone(),
                revision: replacement.revision,
                graph: Some(replacement.clone()),
                preserved_node_ids: Vec::new(),
            })
            .expect("append replacement topology");

        assert_eq!(persisted.events.len(), 3);
        assert!(matches!(
            &persisted.events[1],
            ExecutionDomainEvent::MutationApplied { node_id, .. } if node_id == &source.id
        ));
        assert_eq!(persisted.graph, replacement);
        assert!(
            persisted
                .evidence
                .files
                .contains_key(&repository_evidence_id)
        );
        assert!(
            !persisted
                .evidence
                .validations
                .contains_key("stale-suite-evidence"),
            "a stable validation node invalidated by changed dependencies must lose stale evidence"
        );
        assert_eq!(
            persisted
                .budget
                .usage_for(&validation_node.id)
                .model_calls_consumed,
            0
        );
        assert!(
            persisted
                .budget
                .progress_events
                .iter()
                .all(|progress| progress.node_id.as_ref() != Some(&validation_node.id))
        );
        assert_eq!(persisted.publication, PublicationState::default());
        persisted
            .validate_invariants()
            .expect("persisted invariants");

        let events = persisted.events.clone();
        let mut replayed = initial;
        for event in events {
            replayed.append_event(event).expect("replay event");
        }
        assert_eq!(replayed.graph, persisted.graph);
        assert_eq!(replayed.events, persisted.events);
        assert_eq!(replayed.evidence, persisted.evidence);
        assert_eq!(replayed.failures, persisted.failures);
        assert_eq!(replayed.budget, persisted.budget);
    }

    #[test]
    fn generic_failure_events_recover_discovery_and_validation_nodes() {
        let budget = MissionBudget::for_complexity(MissionComplexity::Small);
        let discovery_graph =
            ExecutionGraph::bootstrap("bootstrap", "tree-1", MissionComplexity::Small, &budget);
        let discovery = discovery_graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Discovery)
            .expect("discovery node")
            .id
            .clone();
        let mut discovery_snapshot = ExecutionSnapshot {
            run_id: "run-discovery-recovery".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: discovery_graph,
            budget: BudgetState::new(budget),
            ..ExecutionSnapshot::default()
        };
        discovery_snapshot
            .append_event(ExecutionDomainEvent::DiscoveryStarted { sequence: 1 })
            .expect("start discovery");
        discovery_snapshot
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 2,
                failure: FailureRecord::new(
                    "discovery-artifact",
                    discovery.clone(),
                    FailureCategory::ModelArtifactRecoverable,
                    1,
                    "tree-1",
                    "discovery artifact was malformed",
                ),
            })
            .expect("record discovery failure");
        assert_eq!(
            discovery_snapshot
                .graph
                .node(&discovery)
                .map(|node| node.status),
            Some(ExecutionNodeStatus::FailedRecoverable)
        );
        discovery_snapshot
            .append_event(ExecutionDomainEvent::FailureRecovered {
                sequence: 3,
                node_id: discovery.clone(),
                failure_id: FailureId::new("discovery-artifact"),
                repository_fingerprint: "tree-1".to_owned(),
            })
            .expect("recover discovery failure");
        assert_eq!(
            discovery_snapshot
                .graph
                .node(&discovery)
                .map(|node| node.status),
            Some(ExecutionNodeStatus::Ready)
        );
        assert_eq!(
            discovery_snapshot
                .failures
                .get(&FailureId::new("discovery-artifact"))
                .map(|failure| failure.status),
            Some(FailureStatus::Recovered)
        );
        assert!(
            discovery_snapshot
                .budget
                .progress_events
                .iter()
                .any(|progress| {
                    progress.kind == ProgressEventKind::FailureRepaired
                        && progress.node_id.as_ref() == Some(&discovery)
                })
        );

        let mut validation_graph = graph();
        let mutation_ids = validation_graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_mutation())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        for mutation_id in mutation_ids {
            validation_graph
                .set_node_status(&mutation_id, ExecutionNodeStatus::Applied)
                .expect("apply prerequisite mutation");
        }
        let validation = validation_graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::ValidationFocused)
            .expect("focused validation")
            .id
            .clone();
        let mut validation_snapshot = ExecutionSnapshot {
            run_id: "run-validation-recovery".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-2".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: validation_graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        validation_snapshot
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 1,
                failure: FailureRecord::new(
                    "validation-failure",
                    validation.clone(),
                    FailureCategory::ValidationFailure,
                    1,
                    "tree-2",
                    "focused validation failed",
                ),
            })
            .expect("record validation failure");
        validation_snapshot
            .append_event(ExecutionDomainEvent::FailureRecovered {
                sequence: 2,
                node_id: validation.clone(),
                failure_id: FailureId::new("validation-failure"),
                repository_fingerprint: "tree-2".to_owned(),
            })
            .expect("recover validation failure");
        assert_eq!(
            validation_snapshot
                .graph
                .node(&validation)
                .map(|node| node.status),
            Some(ExecutionNodeStatus::Ready)
        );
        assert_eq!(validation_snapshot.failures.unresolved().count(), 0);
    }

    #[test]
    fn validation_failed_preserves_blocking_infrastructure_state_on_replay() {
        for (category, expected) in [
            (
                FailureCategory::ValidationFailure,
                ExecutionNodeStatus::FailedRecoverable,
            ),
            (
                FailureCategory::InfrastructureFailure,
                ExecutionNodeStatus::FailedBlocking,
            ),
        ] {
            let mut initial_graph = graph();
            let mutation_ids = initial_graph
                .nodes
                .iter()
                .filter(|node| node.kind.is_mutation())
                .map(|node| node.id.clone())
                .collect::<Vec<_>>();
            for node_id in mutation_ids {
                initial_graph
                    .set_node_status(&node_id, ExecutionNodeStatus::Applied)
                    .unwrap();
            }
            let validation_id = initial_graph
                .nodes
                .iter()
                .find(|node| node.kind.is_validation())
                .expect("validation node")
                .id
                .clone();
            let validation_gate = initial_graph
                .node(&validation_id)
                .and_then(|node| node.validation.clone())
                .expect("validation gate");
            let initial = ExecutionSnapshot {
                run_id: format!("validation-{category:?}"),
                current_repository: RepositorySnapshot {
                    fingerprint: "tree-1".into(),
                    changed_paths: BTreeSet::from(["src/theme.ts".into()]),
                    ..RepositorySnapshot::default()
                },
                graph: initial_graph,
                ..ExecutionSnapshot::default()
            };
            let failure_id = FailureId::new(format!("validation-{category:?}"));
            let validation_fingerprint = validation_gate.fingerprint("tree-1");
            let evidence_id = format!("evidence-{category:?}");
            let evidence = ValidationEvidenceRecord {
                evidence_id: evidence_id.clone(),
                node_id: validation_id.clone(),
                gate_id: validation_gate.gate_id,
                fingerprint: validation_fingerprint.clone(),
                repository_fingerprint: "tree-1".into(),
                command: validation_gate.command,
                working_directory: validation_gate.working_directory,
                status: if category == FailureCategory::InfrastructureFailure {
                    ValidationEvidenceStatus::TimedOut
                } else {
                    ValidationEvidenceStatus::Failed
                },
                exit_code: Some(1),
                output_summary: "validation did not complete successfully".into(),
                duration: Duration::from_millis(5),
            };
            let events = vec![
                ExecutionDomainEvent::ValidationEvidenceRecorded {
                    sequence: 1,
                    node_id: validation_id.clone(),
                    evidence: evidence.clone(),
                },
                ExecutionDomainEvent::FailureRecorded {
                    sequence: 2,
                    failure: FailureRecord::new(
                        failure_id.clone(),
                        validation_id.clone(),
                        category,
                        1,
                        "tree-1",
                        "validation did not complete successfully",
                    ),
                },
                ExecutionDomainEvent::ValidationFailed {
                    sequence: 3,
                    node_id: validation_id.clone(),
                    failure_id,
                    fingerprint: validation_fingerprint,
                },
            ];
            let mut persisted = initial.clone();
            let status_before_evidence =
                persisted.graph.node(&validation_id).map(|node| node.status);
            persisted.append_event(events[0].clone()).unwrap();
            assert_eq!(
                persisted.graph.node(&validation_id).map(|node| node.status),
                status_before_evidence,
                "recording evidence must not change validation lifecycle status"
            );
            assert_eq!(
                persisted
                    .graph
                    .node(&validation_id)
                    .map(|node| node.evidence_ids.as_slice()),
                Some(std::slice::from_ref(&evidence_id))
            );
            for event in events.iter().skip(1) {
                persisted.append_event(event.clone()).unwrap();
            }
            assert_eq!(
                persisted.graph.node(&validation_id).map(|node| node.status),
                Some(expected)
            );
            assert_eq!(
                persisted.evidence.validations.get(&evidence_id),
                Some(&evidence)
            );

            let encoded = serde_json::to_string(&events).unwrap();
            let replay_events: Vec<ExecutionDomainEvent> = serde_json::from_str(&encoded).unwrap();
            let mut replayed = initial;
            for event in replay_events {
                replayed.append_event(event).unwrap();
            }
            assert_eq!(replayed.graph, persisted.graph);
            assert_eq!(replayed.failures, persisted.failures);
            assert_eq!(replayed.evidence, persisted.evidence);
        }
    }

