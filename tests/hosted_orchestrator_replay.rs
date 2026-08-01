use std::collections::BTreeSet;

use rustgrid_agent::execution_graph::{
    ExecutionDomainEvent, FailureCategory, FailureStatus, MissionBudget, MissionComplexity,
    MissionOutcome, PlannedTarget, PublicationMode, PublicationStatus, ValidationGateSpec,
    ValidationGateType,
};
use rustgrid_agent::hosted_orchestrator::ExecutionDecision;
use rustgrid_agent::hosted_simulation::{
    ScriptedAction, ScriptedDiscoveryResult, ScriptedMission, ScriptedPlanningResult,
    ScriptedTargetResult, ScriptedValidationResult, SimulationHarness, SimulationReport,
};

fn target(path: &str, role: &str, criteria: &[&str]) -> PlannedTarget {
    PlannedTarget {
        change_id: format!("change-{}", path.replace(['/', '.'], "-")),
        path: path.to_owned(),
        role: role.to_owned(),
        intent: format!("update {path}"),
        acceptance_criteria_ids: criteria
            .iter()
            .map(|criterion| (*criterion).to_owned())
            .collect(),
        new_file: role.contains("new"),
    }
}

fn gate(id: &str, gate_type: ValidationGateType, command: &str) -> ValidationGateSpec {
    ValidationGateSpec {
        gate_id: id.to_owned(),
        gate_type,
        command: command.to_owned(),
        working_directory: ".".to_owned(),
        required: true,
        dependency_lock_hash: "sim-lock".to_owned(),
        relevant_environment_fingerprint: "sim-env".to_owned(),
    }
}

fn basic_mission(
    name: &str,
    complexity: MissionComplexity,
    targets: Vec<PlannedTarget>,
) -> ScriptedMission {
    let mut mission = ScriptedMission::new(name, complexity).with_validation_gate(gate(
        "tests",
        ValidationGateType::TestSuite,
        "npm test",
    ));
    mission.targets = targets;
    mission
}

fn normal_route_positions(report: &SimulationReport) -> (usize, usize, usize, usize, usize) {
    let last_planned_target = report
        .decisions
        .iter()
        .rposition(|decision| matches!(decision, ExecutionDecision::ExecuteTarget { .. }))
        .expect("mutation decision");
    let first_validation = report
        .decisions
        .iter()
        .position(|decision| matches!(decision, ExecutionDecision::RunValidation { .. }))
        .expect("validation decision");
    let review = report
        .decisions
        .iter()
        .position(|decision| matches!(decision, ExecutionDecision::ReviewDiff { .. }))
        .expect("diff review decision");
    let completion = report
        .decisions
        .iter()
        .position(|decision| matches!(decision, ExecutionDecision::EvaluateCompletion { .. }))
        .expect("completion decision");
    let publication = report
        .decisions
        .iter()
        .position(|decision| matches!(decision, ExecutionDecision::Publish { .. }))
        .expect("publication decision");
    let finish = report
        .decisions
        .iter()
        .position(|decision| matches!(decision, ExecutionDecision::Finish { .. }))
        .expect("finish decision");
    assert!(last_planned_target < first_validation);
    assert!(first_validation < review);
    assert!(review < completion);
    assert!(completion < publication);
    assert!(publication < finish);
    (first_validation, review, completion, publication, finish)
}

fn assert_normal_success(report: &SimulationReport) {
    assert_eq!(report.outcome, MissionOutcome::Complete);
    assert!(report.snapshot.is_terminal());
    assert_eq!(
        report.snapshot.publication.status,
        PublicationStatus::PullRequestCreated
    );
    assert_eq!(
        report.snapshot.publication.mode,
        Some(PublicationMode::Normal)
    );
    assert!(!report.snapshot.publication.draft);
    assert!(report.all_target_diffs_preserved());
    assert!(report.is_within_complexity_ceiling());
    assert_eq!(report.unresolved_failure_count(), 0);
    assert!(report.has_only_legal_adjacent_transitions());
    normal_route_positions(report);
}

fn assert_each_validation_fingerprint_ran_once(report: &SimulationReport) {
    let fingerprints = report
        .validation_runs
        .iter()
        .map(|run| (run.gate_id.clone(), run.fingerprint.clone()))
        .collect::<Vec<_>>();
    let unique = fingerprints.iter().cloned().collect::<BTreeSet<_>>();
    assert_eq!(
        fingerprints.len(),
        unique.len(),
        "an identical validation fingerprint ran more than once"
    );
}

#[test]
fn attempt_17_repeated_validation_is_suppressed_by_fingerprint() {
    let mission = basic_mission(
        "attempt-17",
        MissionComplexity::Small,
        vec![target("src/theme.rs", "production", &["ac-1"])],
    )
    .with_action(ScriptedAction::RepeatValidation {
        gate_id: "tests".to_owned(),
    });

    let report = SimulationHarness::new(mission)
        .run()
        .expect("attempt 17 replay");

    assert_normal_success(&report);
    assert_eq!(report.validation_run_count("tests"), 1);
    assert_eq!(report.suppressed_validation_runs, 1);
    assert_eq!(
        report
            .snapshot
            .events
            .iter()
            .filter(|event| matches!(event, ExecutionDomainEvent::ValidationStarted { .. }))
            .count(),
        1
    );
    assert_each_validation_fingerprint_ran_once(&report);
}

#[test]
fn attempt_18_read_failures_do_not_validate_an_unchanged_tree() {
    let mission = basic_mission(
        "attempt-18",
        MissionComplexity::Small,
        vec![target("src/palette.rs", "production", &["ac-1"])],
    )
    .with_action(ScriptedAction::PreparationReadFailure {
        message: "first bounded read failed".to_owned(),
    })
    .with_action(ScriptedAction::PreparationReadFailure {
        message: "fallback bounded read failed".to_owned(),
    });
    let initial_fingerprint = mission.initial_repository_fingerprint.clone();

    let report = SimulationHarness::new(mission)
        .run()
        .expect("attempt 18 replay");

    assert_normal_success(&report);
    assert_eq!(report.preparation_read_failures, 2);
    assert_eq!(
        report
            .decisions
            .iter()
            .filter(|decision| matches!(decision, ExecutionDecision::RepairTarget { .. }))
            .count(),
        2,
        "both recoverable preparation reads must route through guided target repair"
    );
    assert_eq!(report.snapshot.failures.records.len(), 2);
    assert!(
        report
            .snapshot
            .failures
            .records
            .iter()
            .all(|failure| failure.status == FailureStatus::Superseded)
    );
    assert_ne!(
        report.validation_runs[0].fingerprint,
        gate("tests", ValidationGateType::TestSuite, "npm test").fingerprint(&initial_fingerprint),
        "validation must not reuse the unchanged repository fingerprint"
    );
    let first_mutation_event = report
        .snapshot
        .events
        .iter()
        .position(|event| matches!(event, ExecutionDomainEvent::MutationApplied { .. }))
        .expect("mutation event");
    let first_validation_event = report
        .snapshot
        .events
        .iter()
        .position(|event| matches!(event, ExecutionDomainEvent::ValidationStarted { .. }))
        .expect("validation event");
    assert!(first_mutation_event < first_validation_event);
}

#[test]
fn attempt_19_acceptance_criteria_are_covered_collectively() {
    let mission = basic_mission(
        "attempt-19",
        MissionComplexity::Small,
        vec![
            target("src/provider.tsx", "production", &["ac-1", "ac-2"]),
            target("src/toggle.tsx", "production", &["ac-3", "ac-4"]),
            target("src/palette.css", "production", &["ac-5", "ac-6", "ac-9"]),
            target("tests/theme.test.ts", "tests", &["ac-7", "ac-8"]),
        ],
    )
    .with_required_acceptance_criteria((1..=9).map(|index| format!("ac-{index}")))
    .with_action(ScriptedAction::DiscoveryOutput {
        result: ScriptedDiscoveryResult::Completed,
    })
    .with_action(ScriptedAction::PlanningOutput {
        result: ScriptedPlanningResult::Accepted,
    });
    let expected = (1..=9)
        .map(|index| format!("ac-{index}"))
        .collect::<Vec<_>>();

    assert_eq!(mission.covered_acceptance_criteria(), expected);
    assert!(
        mission
            .targets
            .iter()
            .all(|planned_target| planned_target.acceptance_criteria_ids.len() < 9)
    );

    let report = SimulationHarness::new(mission)
        .run()
        .expect("attempt 19 replay");
    assert_normal_success(&report);
    assert_eq!(report.scripted_model_outputs_consumed, 2);
    assert!(report.snapshot.events.iter().any(|event| matches!(
        event,
        ExecutionDomainEvent::PlanAccepted {
            target_count: 4,
            ..
        }
    )));
    assert_eq!(
        report
            .snapshot
            .graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_mutation())
            .count(),
        4
    );
}

#[test]
fn attempt_20_duplicate_first_target_advances_through_all_five_targets() {
    let paths = [
        "src/ThemeProvider.tsx",
        "src/ThemeToggle.tsx",
        "src/globals.css",
        "tests/theme-provider.test.tsx",
        "tests/theme-palette.test.ts",
    ];
    let mut mission = ScriptedMission::new("attempt-20", MissionComplexity::Small)
        .with_validation_gate(gate("tests", ValidationGateType::TestSuite, "npm test"))
        .with_validation_gate(gate("build", ValidationGateType::Build, "npm run build"));
    mission.targets = paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            target(
                path,
                if path.starts_with("tests/") {
                    "tests"
                } else {
                    "production"
                },
                &[if index == 4 { "ac-9" } else { "ac-1" }],
            )
        })
        .collect();
    mission.actions = vec![
        ScriptedAction::applied(paths[0]),
        ScriptedAction::duplicate(paths[0]),
    ];

    let report = SimulationHarness::new(mission)
        .run()
        .expect("attempt 20 replay");

    assert_normal_success(&report);
    let execution_order = report
        .decisions
        .iter()
        .filter_map(|decision| match decision {
            ExecutionDecision::ExecuteTarget { target, .. } => Some(target.target.path.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(execution_order, paths);
    assert_eq!(
        report
            .target_results
            .iter()
            .filter(|record| record.result == ScriptedTargetResult::AlreadyApplied)
            .count(),
        1
    );
    assert_eq!(report.snapshot.failures.records.len(), 0);
    assert!(
        !report
            .decisions
            .iter()
            .any(|decision| matches!(decision, ExecutionDecision::RepairTarget { .. }))
    );
    assert!(
        report
            .snapshot
            .graph
            .nodes
            .iter()
            .filter(|node| node.required)
            .all(|node| node.status.is_success())
    );
    assert_eq!(report.validation_run_count("tests"), 1);
    assert_eq!(report.validation_run_count("build"), 1);
    assert!(report.snapshot.publication.commit_sha.is_some());
    assert!(report.snapshot.publication.branch.is_some());
    assert!(report.snapshot.publication.pull_request_url.is_some());
    assert_each_validation_fingerprint_ran_once(&report);
}

#[derive(Clone)]
struct BenchmarkCase {
    name: &'static str,
    complexity: MissionComplexity,
    targets: Vec<PlannedTarget>,
    gates: Vec<ValidationGateSpec>,
    actions: Vec<ScriptedAction>,
}

fn benchmark_cases() -> Vec<BenchmarkCase> {
    vec![
        BenchmarkCase {
            name: "single-label rename",
            complexity: MissionComplexity::Tiny,
            targets: vec![target("src/labels.rs", "label rename", &["ac-1"])],
            gates: vec![gate(
                "focused",
                ValidationGateType::FocusedTest,
                "cargo test rename",
            )],
            actions: vec![],
        },
        BenchmarkCase {
            name: "small UI theme option",
            complexity: MissionComplexity::Small,
            targets: vec![
                target("src/theme/provider.tsx", "production", &["ac-1"]),
                target("src/theme/palette.css", "production", &["ac-2"]),
                target("tests/theme.test.tsx", "tests", &["ac-3"]),
            ],
            gates: vec![
                gate("tests", ValidationGateType::TestSuite, "npm test"),
                gate("build", ValidationGateType::Build, "npm run build"),
            ],
            actions: vec![],
        },
        BenchmarkCase {
            name: "unit-test bug fix",
            complexity: MissionComplexity::Small,
            targets: vec![target("tests/recovery_test.rs", "tests", &["ac-1"])],
            gates: vec![gate(
                "focused",
                ValidationGateType::FocusedTest,
                "cargo test recovery_test",
            )],
            actions: vec![],
        },
        BenchmarkCase {
            name: "API field addition",
            complexity: MissionComplexity::Medium,
            targets: vec![
                target("src/api/model.rs", "API field addition", &["ac-1"]),
                target("src/api/route.rs", "production", &["ac-2"]),
                target("tests/api_contract.rs", "tests", &["ac-3"]),
            ],
            gates: vec![
                gate("tests", ValidationGateType::TestSuite, "cargo test"),
                gate("lint", ValidationGateType::Lint, "cargo clippy"),
            ],
            actions: vec![],
        },
        BenchmarkCase {
            name: "new frontend component",
            complexity: MissionComplexity::Small,
            targets: vec![
                target(
                    "src/components/card.tsx",
                    "new frontend component",
                    &["ac-1"],
                ),
                target("src/styles/card.css", "production", &["ac-2"]),
                target("tests/card.test.tsx", "tests", &["ac-3"]),
            ],
            gates: vec![gate("build", ValidationGateType::Build, "npm run build")],
            actions: vec![],
        },
        BenchmarkCase {
            name: "database migration",
            complexity: MissionComplexity::Medium,
            targets: vec![
                target("migrations/0080_state.sql", "new migration", &["ac-1"]),
                target("src/state.rs", "production", &["ac-2"]),
                target("tests/migrations.rs", "tests", &["ac-3"]),
            ],
            gates: vec![gate(
                "tests",
                ValidationGateType::TestSuite,
                "cargo test migrations",
            )],
            actions: vec![],
        },
        BenchmarkCase {
            name: "dependency upgrade",
            complexity: MissionComplexity::Medium,
            targets: vec![
                target("Cargo.toml", "dependency manifest", &["ac-1"]),
                target("Cargo.lock", "dependency lock", &["ac-1"]),
                target("src/integration.rs", "production", &["ac-2"]),
            ],
            gates: vec![
                gate("tests", ValidationGateType::TestSuite, "cargo test"),
                gate("build", ValidationGateType::Build, "cargo build"),
            ],
            actions: vec![],
        },
        BenchmarkCase {
            name: "cross-module refactor",
            complexity: MissionComplexity::Large,
            targets: vec![
                target("src/auth/model.rs", "production", &["ac-1"]),
                target("src/auth/service.rs", "production", &["ac-2"]),
                target("src/api/auth.rs", "production", &["ac-3"]),
                target("src/ui/session.rs", "production", &["ac-4"]),
                target("tests/auth.rs", "tests", &["ac-5"]),
                target("tests/session.rs", "tests", &["ac-6"]),
            ],
            gates: vec![
                gate("tests", ValidationGateType::TestSuite, "cargo test"),
                gate("lint", ValidationGateType::Lint, "cargo clippy"),
                gate("build", ValidationGateType::Build, "cargo build"),
            ],
            actions: vec![],
        },
        BenchmarkCase {
            name: "validation failure requiring repair",
            complexity: MissionComplexity::Small,
            targets: vec![target("src/parser.rs", "production", &["ac-1"])],
            gates: vec![gate(
                "tests",
                ValidationGateType::TestSuite,
                "cargo test parser",
            )],
            actions: vec![ScriptedAction::ValidationResult {
                gate_id: "tests".to_owned(),
                result: ScriptedValidationResult::RecoverableFailure {
                    message: "focused assertion failed".to_owned(),
                },
            }],
        },
        BenchmarkCase {
            name: "documentation-only task",
            complexity: MissionComplexity::Tiny,
            targets: vec![target("docs/worker.md", "documentation", &["ac-1"])],
            gates: vec![gate(
                "docs",
                ValidationGateType::Custom,
                "npm run docs-check",
            )],
            actions: vec![],
        },
    ]
}

fn cost_ceiling(complexity: MissionComplexity) -> u64 {
    match complexity {
        MissionComplexity::Tiny => MissionBudget::TINY_MAX_COST_MICROS,
        MissionComplexity::Small => MissionBudget::SMALL_MAX_COST_MICROS,
        MissionComplexity::Medium => MissionBudget::MEDIUM_MAX_COST_MICROS,
        MissionComplexity::Large => MissionBudget::LARGE_MAX_COST_MICROS,
    }
}

#[test]
fn ten_mission_benchmarks_are_bounded_deterministic_and_lossless() {
    let cases = benchmark_cases();
    assert_eq!(cases.len(), 10);
    assert_eq!(
        cases.iter().map(|case| case.name).collect::<Vec<_>>(),
        vec![
            "single-label rename",
            "small UI theme option",
            "unit-test bug fix",
            "API field addition",
            "new frontend component",
            "database migration",
            "dependency upgrade",
            "cross-module refactor",
            "validation failure requiring repair",
            "documentation-only task",
        ]
    );
    let frontend = cases
        .iter()
        .find(|case| case.name == "new frontend component")
        .expect("frontend benchmark");
    assert!(
        frontend
            .targets
            .iter()
            .any(|target| target.path == "src/components/card.tsx" && target.new_file)
    );

    for case in cases {
        let mut mission = ScriptedMission::new(case.name, case.complexity);
        mission.targets = case.targets;
        mission.validation_gates = case.gates;
        mission.actions = case.actions;

        let first = SimulationHarness::new(mission.clone())
            .run()
            .unwrap_or_else(|error| panic!("{} failed: {error}", case.name));
        let replay = SimulationHarness::new(mission)
            .run()
            .unwrap_or_else(|error| panic!("{} replay failed: {error}", case.name));

        assert_eq!(first, replay, "{} is not replay deterministic", case.name);
        assert_normal_success(&first);
        assert_each_validation_fingerprint_ran_once(&first);
        assert!(first.steps < 64, "{} entered an execution loop", case.name);
        assert_eq!(
            first.snapshot.budget.mission.max_cost_micros,
            cost_ceiling(case.complexity),
            "{} used the wrong complexity ceiling",
            case.name
        );
        assert!(
            first.snapshot.budget.total_cost_micros <= cost_ceiling(case.complexity),
            "{} exceeded its complexity cost ceiling",
            case.name
        );
        assert_eq!(
            first
                .snapshot
                .events
                .iter()
                .filter(|event| matches!(event, ExecutionDomainEvent::PullRequestCreated { .. }))
                .count(),
            1,
            "{} published more than once",
            case.name
        );
    }
}

#[test]
fn model_dispatch_is_denied_before_recording_an_over_budget_call() {
    let mut mission = basic_mission(
        "pre-dispatch-budget",
        MissionComplexity::Tiny,
        vec![target("src/label.rs", "label rename", &["ac-1"])],
    );
    mission.model_call_cost_micros = MissionBudget::TINY_MAX_COST_MICROS + 1;

    let first = SimulationHarness::new(mission.clone())
        .run()
        .expect_err("oversized call must be rejected before dispatch");
    let replay = SimulationHarness::new(mission)
        .run()
        .expect_err("budget rejection must be deterministic");

    assert_eq!(first, replay);
    assert_eq!(first.code, "model_call_budget_denied");
}

#[test]
fn useful_partial_work_publishes_a_draft_pr_at_budget_exhaustion() {
    let first = "src/provider.tsx";
    let remaining = "tests/provider.test.tsx";
    let mission = basic_mission(
        "partial-reviewable",
        MissionComplexity::Small,
        vec![
            target(first, "production", &["ac-1"]),
            target(remaining, "tests", &["ac-2"]),
        ],
    )
    .with_action(ScriptedAction::applied(first))
    .with_action(ScriptedAction::SeedPassedValidation {
        gate_id: "tests".to_owned(),
    })
    .with_action(ScriptedAction::ExhaustTargetBudget {
        path: remaining.to_owned(),
    });

    let report = SimulationHarness::new(mission)
        .run()
        .expect("partial replay");

    assert_eq!(report.outcome, MissionOutcome::PartialReviewable);
    assert_eq!(
        report.snapshot.publication.mode,
        Some(PublicationMode::Draft)
    );
    assert!(report.snapshot.publication.draft);
    assert!(report.snapshot.publication.is_published());
    assert!(
        report
            .snapshot
            .current_repository
            .contains_changed_path(first)
    );
    assert!(
        !report
            .snapshot
            .current_repository
            .contains_changed_path(remaining)
    );
    let partial_guardrail = report
        .decisions
        .iter()
        .position(|decision| {
            matches!(
                decision,
                ExecutionDecision::StopForGuardrail {
                    outcome: MissionOutcome::PartialReviewable,
                    ..
                }
            )
        })
        .expect("partial guardrail decision");
    let review = report
        .decisions
        .iter()
        .position(|decision| matches!(decision, ExecutionDecision::ReviewDiff { .. }))
        .expect("partial diff review");
    let completion = report
        .decisions
        .iter()
        .position(|decision| matches!(decision, ExecutionDecision::EvaluateCompletion { .. }))
        .expect("partial completion evaluation");
    let publication = report
        .decisions
        .iter()
        .position(|decision| {
            matches!(
                decision,
                ExecutionDecision::Publish {
                    mode: PublicationMode::Draft
                }
            )
        })
        .expect("draft publication decision");
    let finish = report
        .decisions
        .iter()
        .position(|decision| {
            matches!(
                decision,
                ExecutionDecision::Finish {
                    outcome: MissionOutcome::PartialReviewable
                }
            )
        })
        .expect("partial terminal decision");
    assert!(partial_guardrail < review);
    assert!(review < completion);
    assert!(completion < publication);
    assert!(publication < finish);
    assert!(report.has_only_legal_adjacent_transitions());
    assert!(report.snapshot.events.iter().any(|event| matches!(
        event,
        ExecutionDomainEvent::CompletionEvaluated {
            outcome: MissionOutcome::PartialReviewable,
            ..
        }
    )));
}

#[test]
fn infrastructure_failure_is_distinct_and_never_publishes() {
    let path = "src/remote.rs";
    let mission = basic_mission(
        "infrastructure-failure",
        MissionComplexity::Small,
        vec![target(path, "production", &["ac-1"])],
    )
    .with_action(ScriptedAction::TargetResult {
        path: path.to_owned(),
        result: ScriptedTargetResult::InfrastructureFailure {
            message: "repository transport unavailable".to_owned(),
        },
    });

    let report = SimulationHarness::new(mission)
        .run()
        .expect("infrastructure replay");

    assert_eq!(report.outcome, MissionOutcome::FailedInfrastructure);
    assert_eq!(
        report.snapshot.publication.status,
        PublicationStatus::NotStarted
    );
    assert!(!report.snapshot.current_repository.has_changes());
    assert_eq!(report.unresolved_failure_count(), 1);
    assert_eq!(
        report.snapshot.failures.records[0].category,
        FailureCategory::InfrastructureFailure
    );
    assert!(!report.snapshot.events.iter().any(|event| matches!(
        event,
        ExecutionDomainEvent::CommitCreated { .. }
            | ExecutionDomainEvent::BranchPushed { .. }
            | ExecutionDomainEvent::PullRequestCreated { .. }
    )));
}
