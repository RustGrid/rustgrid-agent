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

    #[test]
    fn validation_repair_budget_and_call_accounting_are_separate_from_mutation_work() {
        let graph = graph();
        let validation = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::ValidationFocused)
            .expect("focused validation node");
        let mutation = graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .expect("mutation node");
        assert!(validation.budget.max_model_calls >= 1);

        let mut budget = BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small));
        budget.record_validation_repair_attempt(validation.id.clone());
        budget.record_validation_command_run(validation.id.clone());
        budget.record_validation_parsing_call(validation.id.clone());
        budget.record_validation_diagnosis_call(validation.id.clone());
        budget.record_model_call_purpose(ModelCallPurpose::ValidationDiagnosis);
        budget.record_model_call_purpose(ModelCallPurpose::ValidationRepairMutation);

        assert_eq!(budget.usage_for(&validation.id).validation_repair_attempts, 1);
        assert_eq!(budget.usage_for(&validation.id).mutation_fallback_attempts, 0);
        assert_eq!(budget.usage_for(&mutation.id).mutation_fallback_attempts, 0);
        assert_eq!(
            budget.validation_gate_usage.get(&validation.id),
            Some(&ValidationGateBudget {
                command_runs: 1,
                parsing_calls: 1,
                diagnosis_calls: 1,
            })
        );
        assert_eq!(budget.model_call_breakdown.validation_diagnosis_calls, 1);
        assert_eq!(
            budget
                .model_call_breakdown
                .validation_repair_mutation_calls,
            1
        );
        assert_eq!(budget.model_call_breakdown.target_mutation_repair_calls, 0);
    }

    #[test]
    fn validation_repair_budget_rejects_an_impossible_legal_sequence() {
        let impossible = ValidationRepairBudget {
            max_model_calls: 1,
            max_target_attempts: 1,
            max_repository_writes: 1,
            max_context_rebuilds: 1,
            max_cost_micros: 100,
        };
        assert!(impossible.validate(false).is_err());

        let multi_target = ValidationRepairBudget {
            max_model_calls: 3,
            max_target_attempts: 2,
            max_repository_writes: 2,
            max_context_rebuilds: 2,
            max_cost_micros: 100,
        };
        assert!(multi_target.validate(true).is_ok());
    }

    #[test]
    fn one_call_validation_gate_materializes_an_independent_multi_call_repair_session() {
        let gate_id = ExecutionNodeId::new("validation-focused");
        let mut failure = FailureRecord::new(
            "failure-1",
            gate_id.clone(),
            FailureCategory::ValidationFailure,
            1,
            "tree-1",
            "two assertions failed",
        );
        failure.assertion_failures = vec![ValidationAssertionFailure {
            test_file: "tests/a.rs".into(),
            test_name: "a".into(),
            implicated_paths: vec!["src/a.rs".into(), "src/b.rs".into()],
            ..ValidationAssertionFailure::default()
        }];
        let mut budget = BudgetState::new(MissionBudget {
            max_model_calls: 8,
            max_cost_micros: 10_000,
            max_duration: Duration::from_secs(600),
            max_target_repair_rounds: 2,
        });
        budget.create_validation_failure_revision(&failure, 3);
        let session = budget
            .ensure_validation_repair_session(
                &failure,
                ValidationRepairBudgetInputs {
                    failed_assertion_count: 2,
                    implicated_target_count: 2,
                    originating_gate_required: true,
                    implicated_target_bytes: 600 * 1024,
                },
            )
            .expect("viable repair session")
            .clone();
        assert_eq!(session.budget.max_model_calls, 3);
        assert_eq!(session.budget.max_target_attempts, 2);
        assert_eq!(session.budget.max_context_rebuilds, 4);
        assert_eq!(budget.usage_for(&gate_id).model_calls_consumed, 0);

        let (owner, repair_budget) = budget
            .repair_budget_owner(&failure.id)
            .expect("repair budget owner");
        let first = budget
            .reserve_model_call(&owner, &repair_budget, 100, Duration::ZERO)
            .expect("first repair call");
        budget.consume_model_call_reservation(&first, 80, Duration::from_millis(1));
        let second = budget
            .reserve_model_call(&owner, &repair_budget, 100, Duration::ZERO)
            .expect("second repair call");
        budget.consume_model_call_reservation(&second, 80, Duration::from_millis(1));
        assert_eq!(budget.usage_for(&owner).model_calls_consumed, 2);
        assert_eq!(budget.usage_for(&gate_id).model_calls_consumed, 0);
    }

    #[test]
    fn validation_failure_revisions_are_fingerprint_bound_and_recomputed() {
        let gate_id = ExecutionNodeId::new("validation-focused");
        let mut first = FailureRecord::new(
            "failure-1",
            gate_id.clone(),
            FailureCategory::ValidationFailure,
            1,
            "tree-1",
            "a and b failed",
        );
        first.assertion_failures = vec![
            ValidationAssertionFailure {
                test_file: "tests/a.rs".into(),
                test_name: "a".into(),
                ..ValidationAssertionFailure::default()
            },
            ValidationAssertionFailure {
                test_file: "tests/b.rs".into(),
                test_name: "b".into(),
                ..ValidationAssertionFailure::default()
            },
        ];
        let mut budget = BudgetState::new(MissionBudget::default());
        let revision_one = budget.create_validation_failure_revision(&first, 3);
        assert_eq!(revision_one.assertion_ids.len(), 2);
        assert!(
            budget
                .current_validation_failure_revision(gate_id.as_str(), "tree-2")
                .is_none(),
            "a repository mutation makes the old assertion revision stale"
        );

        let mut second = FailureRecord::new(
            "failure-2",
            gate_id.clone(),
            FailureCategory::ValidationFailure,
            2,
            "tree-2",
            "only b still fails",
        );
        second.assertion_failures = vec![ValidationAssertionFailure {
            test_file: "tests/b.rs".into(),
            test_name: "b".into(),
            ..ValidationAssertionFailure::default()
        }];
        let revision_two = budget.create_validation_failure_revision(&second, 8);
        assert_eq!(revision_two.revision, 2);
        assert_eq!(revision_two.assertion_ids.len(), 1);
        assert_eq!(
            budget
                .current_validation_failure_revision(gate_id.as_str(), "tree-2")
                .map(|revision| revision.assertion_ids.len()),
            Some(1)
        );
    }

    #[test]
    fn repair_reallocation_and_attempt_history_remain_bounded_and_auditable() {
        let gate_id = ExecutionNodeId::new("validation-focused");
        let mut failure = FailureRecord::new(
            "failure-reallocation",
            gate_id,
            FailureCategory::ValidationFailure,
            1,
            "tree-1",
            "repairable assertion",
        );
        failure.assertion_failures = vec![ValidationAssertionFailure {
            test_file: "tests/a.rs".into(),
            test_name: "a".into(),
            implicated_paths: vec!["src/a.rs".into(), "src/b.rs".into()],
            ..ValidationAssertionFailure::default()
        }];
        let mut budget = BudgetState::new(MissionBudget {
            max_model_calls: 8,
            max_cost_micros: 10_000,
            max_duration: Duration::from_secs(600),
            max_target_repair_rounds: 2,
        });
        budget.create_validation_failure_revision(&failure, 1);
        let session = budget
            .ensure_validation_repair_session(
                &failure,
                ValidationRepairBudgetInputs {
                    failed_assertion_count: 1,
                    implicated_target_count: 2,
                    originating_gate_required: true,
                    implicated_target_bytes: 1,
                },
            )
            .unwrap()
            .clone();
        let owner = ExecutionNodeId::new(session.session_id.clone());
        for _ in 0..session.budget.max_model_calls {
            let reservation = budget
                .reserve_model_call(&owner, &session.budget.as_node_budget(), 100, Duration::ZERO)
                .unwrap();
            budget.consume_model_call_reservation(&reservation, 100, Duration::ZERO);
        }
        let reallocated = budget
            .reallocate_validation_repair_capacity(&failure.id, 1, 0)
            .expect("remaining mission capacity is available");
        assert_eq!(reallocated.model_calls, 1);
        assert_eq!(reallocated.cost_micros, 0);
        for _ in 0..session.budget.max_context_rebuilds {
            budget
                .record_validation_repair_context_rebuild(&failure.id)
                .unwrap();
        }
        assert!(
            budget
                .record_validation_repair_context_rebuild(&failure.id)
                .is_err()
        );
        for _ in 0..session.budget.max_repository_writes {
            budget
                .record_validation_repair_repository_write(&failure.id)
                .unwrap();
        }
        assert!(
            budget
                .record_validation_repair_repository_write(&failure.id)
                .is_err()
        );

        for (outcome, target, model_call_id) in [
            (
                ValidationRepairMutationOutcome::MutationApplied,
                "src/a.rs",
                Some("semantic-call-1".into()),
            ),
            (
                ValidationRepairMutationOutcome::AdmissionRejected,
                "src/b.rs",
                None,
            ),
        ] {
            budget
                .record_validation_repair_attempt_for_failure(
                    &failure.id,
                    ValidationRepairAttempt {
                        target_path: target.into(),
                        outcome,
                        model_call_id,
                        admission_rejection_reason: (outcome
                            == ValidationRepairMutationOutcome::AdmissionRejected)
                            .then(|| "node_model_call_budget_exhausted".into()),
                        ..ValidationRepairAttempt::default()
                    },
                )
                .unwrap();
        }
        let persisted = budget.repair_session_for_failure(&failure.id).unwrap();
        assert_eq!(persisted.attempted_targets.len(), 2);
        assert_eq!(persisted.attempted_targets[0].attempt_number, 1);
        assert_eq!(
            persisted.attempted_targets[0].model_call_id.as_deref(),
            Some("semantic-call-1")
        );
        assert_eq!(
            persisted.attempted_targets[1]
                .admission_rejection_reason
                .as_deref(),
            Some("node_model_call_budget_exhausted")
        );
    }

    #[test]
    fn partial_recovery_accepts_explicit_stale_after_applied_repair_observation() {
        let (mut snapshot, _, evidence_ids) = recovery_publication_snapshot();
        let validation_node = snapshot
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_validation())
            .expect("validation node")
            .id
            .clone();
        for evidence in snapshot.evidence.validations.values_mut() {
            evidence.repository_fingerprint = "tree-before-repair".into();
            evidence.status = ValidationEvidenceStatus::Failed;
        }
        for node in snapshot
            .graph
            .nodes
            .iter_mut()
            .filter(|node| node.kind.is_validation())
        {
            node.status = ExecutionNodeStatus::FailedRecoverable;
        }
        snapshot.current_repository.fingerprint = "tree-after-repair".into();
        snapshot.current_repository.source_tree_hash = "tree-after-repair".into();
        let failure = FailureRecord::new(
            "failure-before-repair",
            validation_node.clone(),
            FailureCategory::ValidationFailure,
            1,
            "tree-before-repair",
            "focused validation failed",
        );
        snapshot.failures.record(failure.clone());
        snapshot.events.push(ExecutionDomainEvent::IncompleteDiffReviewRequested {
            sequence: 1,
            node_id: ExecutionNodeId::new("diff-review"),
            reason: IncompleteReason::ValidationRepairProducedNoMeaningfulMutation,
            dependency_overrides: Vec::new(),
        });
        let session_id = BudgetState::repair_session_id(&failure.id);
        snapshot.budget.validation_repair_sessions.insert(
            session_id.clone(),
            ValidationRepairSession {
                session_id,
                failed_validation_id: failure.id.to_string(),
                originating_gate_id: validation_node,
                budget: ValidationRepairBudget {
                    max_model_calls: 2,
                    max_target_attempts: 1,
                    max_repository_writes: 1,
                    max_context_rebuilds: 1,
                    max_cost_micros: 1_000,
                },
                status: ValidationRepairSessionStatus::ReadyForRerun,
                attempted_targets: vec![ValidationRepairAttempt {
                    target_path: "src/theme.ts".into(),
                    outcome: ValidationRepairMutationOutcome::MutationApplied,
                    repository_fingerprint_before: RepositoryFingerprint::new(
                        "tree-before-repair",
                    ),
                    repository_fingerprint_after: RepositoryFingerprint::new(
                        "tree-after-repair",
                    ),
                    ..ValidationRepairAttempt::default()
                }],
                current_assertion_set_revision: 1,
                ..ValidationRepairSession::default()
            },
        );

        let authorized = snapshot
            .recovery_publication_validation_evidence_ids()
            .expect("stale-after-repair proof remains explicit and publishable as partial");
        assert_eq!(authorized.len(), 1);
        assert!(evidence_ids.contains(&authorized[0]));
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
                max_mutation_fallback_attempts: 0,
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
    fn completion_node_rejects_a_budget_that_cannot_fund_its_compact_profile() {
        let mut graph = graph();
        let completion = graph
            .nodes
            .iter_mut()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .expect("completion node");
        completion.budget.max_model_calls = 1;
        completion.budget.max_cost_micros = 99_999;
        let error = graph.validate_invariants().unwrap_err();
        assert!(error.message.contains("budget_configuration_invalid"));
        assert!(error.message.contains("minimum_viable_node_cost=100000"));
    }

    #[test]
    fn mutation_node_cannot_advertise_repair_without_a_distinct_model_call() {
        let mut graph = graph();
        let mutation = graph
            .nodes
            .iter_mut()
            .find(|node| node.kind.is_mutation())
            .expect("mutation node");
        mutation.budget.max_model_calls = 1;
        mutation.budget.max_mutation_fallback_attempts = 1;
        let error = graph.validate_invariants().unwrap_err();
        assert!(error.message.contains("budget_configuration_invalid"));
        assert!(error.message.contains("cannot fund a primary attempt and a distinct repair"));
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
            failures.supersede_for_applied_target(&node_id, "src/theme.ts", "tree-2", Some(2)),
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
                .supersede_for_applied_target(&node_id, "src/theme.ts", "tree-2", Some(2))
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
            Some(ExecutionNodeStatus::Ready)
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
            .append_event(ExecutionDomainEvent::NodeStarted {
                sequence: 2,
                node_id: source.id.clone(),
                attempt: 1,
                started_at: "2026-08-08T00:00:01Z".to_owned(),
                repository_fingerprint: "tree-1".to_owned(),
            })
            .expect("start mutation attempt");
        persisted
            .append_event(ExecutionDomainEvent::MutationApplied {
                sequence: 3,
                node_id: source.id.clone(),
                target_path: "src/theme.ts".to_owned(),
                repository_fingerprint: "tree-2".to_owned(),
                evidence_id: "mutation-theme-tree-2".to_owned(),
                completed_at: "2026-08-08T00:00:02Z".to_owned(),
                satisfied_intent: SatisfiedIntent::OriginalImplementation,
                repair_failure_id: None,
                created_target_evidence: None,
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
                sequence: 4,
                graph_id: replacement.graph_id.clone(),
                revision: replacement.revision,
                graph: Some(replacement.clone()),
                preserved_node_ids: Vec::new(),
            })
            .expect("append replacement topology");

        assert_eq!(persisted.events.len(), 4);
        assert!(matches!(
            &persisted.events[2],
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
                .set_node_status(&mutation_id, ExecutionNodeStatus::Completed)
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
    #[test]
    fn target_operations_have_stable_structured_wire_forms() {
        let cases = [
            (TargetOperation::ModifyExisting, "modify_existing"),
            (TargetOperation::CreateNew, "create_new"),
            (TargetOperation::DeleteExisting, "delete_existing"),
            (TargetOperation::Rename { source: "a".into(), destination: "b".into() }, "rename"),
            (TargetOperation::Move { source: "a".into(), destination: "dir/b".into() }, "move"),
        ];
        for (operation, kind) in cases {
            let encoded = serde_json::to_value(&operation).unwrap();
            assert_eq!(encoded["kind"], kind);
            assert_eq!(serde_json::from_value::<TargetOperation>(encoded).unwrap(), operation);
        }
    }

    #[test]
    fn legacy_new_file_metadata_maps_only_to_create_operation() {
        let mut planned = target("src/new.rs", "production");
        planned.new_file = true;
        assert_eq!(planned.effective_operation(), TargetOperation::CreateNew);
        planned.new_file = false;
        assert_eq!(planned.effective_operation(), TargetOperation::ModifyExisting);
    }

    #[test]
    fn operation_source_and_destination_are_typed_not_inferred() {
        let rename = TargetOperation::Rename { source: "old.rs".into(), destination: "new.rs".into() };
        assert_eq!(rename.source_path(), Some("old.rs"));
        assert_eq!(rename.destination_path("ignored"), "new.rs");
        assert_eq!(TargetOperation::DeleteExisting.source_path(), None);
        assert_eq!(TargetOperation::DeleteExisting.destination_path("old.rs"), "old.rs");
    }

    #[test]
    fn target_state_probe_preserves_operation_hashes_and_existence() {
        let probe = TargetStateProbe {
            operation: TargetOperation::Move { source: "a".into(), destination: "b".into() },
            target_path: "b".into(),
            target_exists: false,
            source_exists: Some(true),
            target_content_hash: None,
            source_content_hash: Some("hash-a".into()),
            expected_result_content_hash: Some("hash-a".into()),
            repository_fingerprint: RepositoryFingerprint::new("tree-1"),
        };
        let replayed: TargetStateProbe = serde_json::from_value(serde_json::to_value(&probe).unwrap()).unwrap();
        assert_eq!(replayed, probe);
    }

    #[test]
    fn target_state_probe_classifies_every_operation_state_without_strings() {
        let classify = |operation, target_exists, source_exists| {
            TargetStateProbe {
                operation,
                target_path: "destination".into(),
                target_exists,
                source_exists,
                repository_fingerprint: RepositoryFingerprint::new("tree-1"),
                ..TargetStateProbe::default()
            }
            .inspection_outcome()
        };
        assert_eq!(classify(TargetOperation::ModifyExisting, true, None), TargetInspectionOutcome::ExistingTargetLoaded);
        assert!(matches!(classify(TargetOperation::ModifyExisting, false, None), TargetInspectionOutcome::OperationConflict { conflict } if conflict.code == "expected_existing_target_missing"));
        assert_eq!(classify(TargetOperation::CreateNew, false, None), TargetInspectionOutcome::NewTargetConfirmedAbsent);
        assert!(matches!(classify(TargetOperation::CreateNew, true, None), TargetInspectionOutcome::OperationConflict { conflict } if conflict.code == "create_target_already_exists"));
        let matching = |operation, source_exists| TargetStateProbe {
            operation,
            target_path: "destination".into(),
            target_exists: true,
            source_exists,
            target_content_hash: Some("expected".into()),
            expected_result_content_hash: Some("expected".into()),
            repository_fingerprint: RepositoryFingerprint::new("tree-2"),
            ..TargetStateProbe::default()
        }.inspection_outcome();
        assert_eq!(matching(TargetOperation::CreateNew, None), TargetInspectionOutcome::AlreadyApplied);
        assert_eq!(classify(TargetOperation::DeleteExisting, false, None), TargetInspectionOutcome::AlreadyApplied);
        let rename = TargetOperation::Rename { source: "source".into(), destination: "destination".into() };
        assert_eq!(matching(rename.clone(), Some(false)), TargetInspectionOutcome::AlreadyApplied);
        assert!(matches!(classify(rename.clone(), true, Some(true)), TargetInspectionOutcome::OperationConflict { conflict } if conflict.code == "destination_already_exists"));
        assert!(matches!(classify(rename, false, Some(false)), TargetInspectionOutcome::OperationConflict { conflict } if conflict.code == "expected_source_target_missing"));
    }

    #[test]
    fn inspection_conflict_keeps_machine_code_separate_from_message() {
        let conflict = TargetOperationConflict {
            code: "create_target_already_exists".into(),
            operation: TargetOperation::CreateNew,
            target_path: "src/new.rs".into(),
            source_path: None,
            message: "destination is occupied".into(),
            recoverable: true,
        };
        let outcome = TargetInspectionOutcome::OperationConflict { conflict: conflict.clone() };
        let replayed: TargetInspectionOutcome = serde_json::from_value(serde_json::to_value(&outcome).unwrap()).unwrap();
        assert_eq!(replayed, outcome);
        assert_eq!(conflict.code, "create_target_already_exists");
    }

    #[test]
    fn create_specification_is_language_and_framework_neutral() {
        let specification = CreateTargetSpecification {
            path: "docs/architecture.note".into(),
            role: "repository documentation".into(),
            intent: "record the lifecycle contract".into(),
            acceptance_criteria_ids: vec!["ac-1".into()],
            related_evidence_ids: vec![EvidenceId::new("evidence-1")],
            expected_artifact_kind: Some("documentation".into()),
        };
        assert_eq!(specification.path, "docs/architecture.note");
        assert!(!serde_json::to_string(&specification).unwrap().contains("rust"));
    }

    #[test]
    fn failure_code_survives_event_replay_without_message_parsing() {
        let mut failure = FailureRecord::new("failure-1", "source-000", FailureCategory::MutationConflict, 1, "tree-1", "human context");
        failure.code = Some("destination_already_exists".into());
        let replayed: FailureRecord = serde_json::from_value(serde_json::to_value(&failure).unwrap()).unwrap();
        assert_eq!(replayed.code.as_deref(), Some("destination_already_exists"));
        assert_eq!(replayed.message, "human context");
    }

    #[test]
    fn created_target_evidence_binds_before_and_after_fingerprints() {
        let evidence = CreatedTargetEvidence {
            path: "src/new.rs".into(),
            content_hash: "content".into(),
            repository_fingerprint_before: RepositoryFingerprint::new("before"),
            repository_fingerprint_after: RepositoryFingerprint::new("after"),
            creation_tool: "create_file".into(),
            validation_gate_ids: vec!["build".into()],
        };
        assert_ne!(evidence.repository_fingerprint_before, evidence.repository_fingerprint_after);
        assert_eq!(evidence.creation_tool, "create_file");
        let event = ExecutionDomainEvent::MutationApplied {
            sequence: 1,
            node_id: ExecutionNodeId::new("source-create"),
            target_path: evidence.path.clone(),
            repository_fingerprint: evidence.repository_fingerprint_after.to_string(),
            evidence_id: "mutation-create".into(),
            completed_at: "2026-08-08T00:00:01Z".into(),
            satisfied_intent: SatisfiedIntent::OriginalImplementation,
            repair_failure_id: None,
            created_target_evidence: Some(evidence.clone()),
        };
        let replayed: ExecutionDomainEvent =
            serde_json::from_value(serde_json::to_value(event).unwrap()).unwrap();
        assert!(matches!(
            replayed,
            ExecutionDomainEvent::MutationApplied {
                created_target_evidence: Some(replayed),
                ..
            } if replayed == evidence
        ));
    }

    #[test]
    fn already_applied_repair_evidence_is_scoped_to_the_exact_repair_intent() {
        let intent = ValidationRepairIntent {
            repair_intent_id: "repair-validation-1".into(),
            failed_validation_id: "validation-1".into(),
            target: "src/lib.rs".into(),
            diagnosis: ValidationRepairDiagnosis::SourceDefect,
            expected_correction: ExpectedTargetState {
                content_hash: Some("corrected-hash".into()),
                required_assertion_ids: vec!["assertion-1".into()],
                required_observable_change: "returns corrected value".into(),
            },
            evidence_ids: vec![EvidenceId::new("validation-evidence-1")],
        };
        let evidence = AlreadyAppliedRepairEvidence {
            repair_intent_id: intent.repair_intent_id.clone(),
            target_path: intent.target.clone(),
            expected_state_hash: Some("corrected-hash".into()),
            current_state_hash: "corrected-hash".into(),
            satisfied_assertions: vec!["assertion-1".into()],
            supporting_evidence_ids: vec![EvidenceId::new("validation-evidence-1")],
        };
        assert!(evidence.proves(&intent));
        let mut wrong_intent = intent.clone();
        wrong_intent.repair_intent_id = "repair-validation-2".into();
        assert!(!evidence.proves(&wrong_intent));
        let mut incomplete_evidence = evidence;
        incomplete_evidence.satisfied_assertions.clear();
        assert!(!incomplete_evidence.proves(&intent));
    }

    #[test]
    fn already_applied_atomically_completes_the_node_and_is_revision_idempotent() {
        let mut create = target("src/generated.txt", "production");
        create.operation = TargetOperation::CreateNew;
        let graph = ExecutionGraph::from_targets(
            "graph-already-applied",
            MissionComplexity::Small,
            "tree-before",
            &[create],
            &[gate("focused", ValidationGateType::FocusedTest)],
            &MissionBudget::for_complexity(MissionComplexity::Small),
        );
        let node_id = graph.nodes().find(|node| node.kind.is_mutation()).unwrap().id.clone();
        let mut snapshot = ExecutionSnapshot {
            run_id: "execution-1".into(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-before".into(),
                source_tree_hash: "tree-before".into(),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        snapshot.append_event(ExecutionDomainEvent::NodeStarted {
            sequence: 1,
            node_id: node_id.clone(),
            attempt: 1,
            started_at: "2026-08-05T10:00:00Z".into(),
            repository_fingerprint: "tree-before".into(),
        }).unwrap();
        let transition = AlreadyAppliedTransition {
            node_id: node_id.clone(),
            operation: TargetOperation::CreateNew,
            target_path: "src/generated.txt".into(),
            expected_result_hash: Some("expected".into()),
            observed_result_hash: Some("expected".into()),
            repository_fingerprint: RepositoryFingerprint::new("tree-before"),
            completed_at: "2026-08-05T10:00:01Z".into(),
        };
        let semantic_id = transition.semantic_id("execution-1", 1);
        let event = ExecutionDomainEvent::TargetOperationAlreadyApplied {
            sequence: 2,
            execution_id: "execution-1".into(),
            attempt: 1,
            transition: transition.clone(),
            semantic_id: semantic_id.clone(),
            satisfied_intent: SatisfiedIntent::OriginalImplementation,
            repair_failure_id: None,
        };
        let revision_before = snapshot.graph.revision();
        snapshot.append_event(event).unwrap();
        let node = snapshot.graph.node(&node_id).unwrap();
        assert_eq!(node.status, ExecutionNodeStatus::Completed);
        assert_eq!(node.attempts[0].outcome, Some(ExecutionNodeStatus::Completed));
        assert_eq!(node.attempts[0].completed_at.as_deref(), Some("2026-08-05T10:00:01Z"));
        assert_eq!(node.operation_evidence.len(), 1);
        assert_eq!(node.operation_evidence[0].semantic_id, semantic_id);
        assert_eq!(snapshot.graph.revision(), revision_before + 1);
        assert!(snapshot.graph.active_node().is_none());
        let validation = snapshot.graph.nodes().find(|node| node.kind.is_validation()).unwrap();
        assert_eq!(validation.status, ExecutionNodeStatus::Ready);
        let revision_after = snapshot.graph.revision();
        let event_count = snapshot.events.len();
        snapshot.append_event(ExecutionDomainEvent::TargetOperationAlreadyApplied {
            sequence: 3,
            execution_id: "execution-1".into(),
            attempt: 1,
            transition,
            semantic_id,
            satisfied_intent: SatisfiedIntent::OriginalImplementation,
            repair_failure_id: None,
        }).unwrap();
        assert_eq!(snapshot.graph.revision(), revision_after);
        assert_eq!(snapshot.events.len(), event_count);
    }

    #[test]
    fn operation_reducer_rejects_early_or_conflicting_success_and_accepts_terminal_replay() {
        let mut node = ExecutionNode {
            id: ExecutionNodeId::new("source-1"),
            status: ExecutionNodeStatus::Pending,
            target: Some(target("src/lib.rs", "production")),
            ..ExecutionNode::default()
        };
        let evidence = OperationEvidence {
            semantic_id: "semantic-1".into(),
            outcome: RepositoryOperationOutcome::AlreadyApplied,
            operation: TargetOperation::ModifyExisting,
            target_path: "src/lib.rs".into(),
            repository_fingerprint: RepositoryFingerprint::new("tree"),
            attempt: 1,
            completed_at: "now".into(),
            ..OperationEvidence::default()
        };
        assert_eq!(reduce_operation_outcome(&node, RepositoryOperationOutcome::AlreadyApplied, evidence.clone()).unwrap(), NodeTransition::InvalidTransition);
        node.status = ExecutionNodeStatus::Running;
        assert!(matches!(reduce_operation_outcome(&node, RepositoryOperationOutcome::Applied, evidence.clone()).unwrap(), NodeTransition::Completed(_)));
        node.status = ExecutionNodeStatus::Completed;
        node.operation_evidence.push(evidence.clone());
        assert!(matches!(reduce_operation_outcome(&node, RepositoryOperationOutcome::AlreadyApplied, evidence.clone()).unwrap(), NodeTransition::NoOp(_)));
        let mut conflicting = evidence;
        conflicting.observed_result_hash = Some("different".into());
        assert_eq!(reduce_operation_outcome(&node, RepositoryOperationOutcome::AlreadyApplied, conflicting).unwrap(), NodeTransition::StateConflict);
    }

    #[test]
    fn deterministic_cycle_detection_is_bounded_and_lease_renewal_is_not_progress() {
        let mut history = Vec::new();
        let mut liveness = WorkerLiveness {
            lease_renewed_at: Some("later".into()),
            last_semantic_progress_at: Some("earlier".into()),
        };
        assert_eq!(observe_semantic_cycle(&mut history, "state", "decision", "probe", "t1"), 1);
        liveness.lease_renewed_at = Some("latest".into());
        assert_eq!(observe_semantic_cycle(&mut history, "state", "decision", "probe", "t2"), MAX_IDENTICAL_DETERMINISTIC_CYCLES);
        assert_eq!(liveness.last_semantic_progress_at.as_deref(), Some("earlier"));
        let cancellation = CancellationRequest {
            initiator: CancellationInitiator::CycleGuardrail,
            reason_code: "deterministic_orchestration_cycle".into(),
            requested_at: "t2".into(),
        };
        assert_eq!(cancellation.initiator, CancellationInitiator::CycleGuardrail);
        assert!(!OrchestrationCycleResult::default().made_semantic_progress());
        for index in 3..=100 {
            observe_semantic_cycle(&mut history, "state", "decision", "probe", &format!("t{index}"));
        }
        assert!(history.len() <= 8);
        assert_eq!(history.last().unwrap().repeated_count, 100);
    }

    #[test]
    fn setting_an_identical_node_status_is_a_pure_graph_no_op() {
        let graph = ExecutionGraph::from_targets(
            "graph-no-op",
            MissionComplexity::Small,
            "tree-before",
            &[target("src/lib.rs", "production")],
            &[],
            &MissionBudget::for_complexity(MissionComplexity::Small),
        );
        let node_id = graph.nodes().find(|node| node.kind.is_mutation()).unwrap().id.clone();
        let status = graph.node(&node_id).unwrap().status;
        let revision = graph.revision();
        let mut graph = graph;
        assert_eq!(graph.set_node_status_if_changed(&node_id, status).unwrap(), GraphMutationResult::NoChange { current_revision: revision });
        assert_eq!(graph.revision(), revision);
    }
