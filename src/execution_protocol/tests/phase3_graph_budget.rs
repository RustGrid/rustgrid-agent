use std::collections::BTreeSet;

use super::*;

fn mission_capacity(model_calls: u32) -> PlanMissionCapacity {
    PlanMissionCapacity {
        remaining_model_calls: model_calls,
        remaining_cost_micros: u64::from(model_calls),
        remaining_duration_ms: u64::from(model_calls),
    }
}

fn validation_expectations(
    seed: &super::phase2_discovery::Phase3PlanningSeed,
) -> BTreeSet<ValidationExpectation> {
    let candidates = seed
        .profile
        .validation_candidates
        .iter()
        .take(2)
        .collect::<Vec<_>>();
    assert_eq!(
        candidates.len(),
        2,
        "the checked-in profile must expose two independently budgeted validation gates"
    );
    candidates
        .into_iter()
        .map(|candidate| {
            ValidationExpectation::new(
                candidate.candidate_id.clone(),
                BTreeSet::from([seed.criterion_id.clone()]),
            )
            .expect("profile-bound validation expectation")
        })
        .collect()
}

fn candidate_with_two_targets(seed: &super::phase2_discovery::Phase3PlanningSeed) -> PlanCandidate {
    let expected_validation = validation_expectations(seed);
    let source_target = PlannedTargetV1 {
        target_id: TargetId::new("target:budget-source"),
        change_id: ChangeId::new("change:budget-source"),
        path: ProfilePath::new("src/slug.rs").expect("exact source path"),
        operation: TargetOperation::ModifyExisting {
            expected_content_hash: seed.source_file.content_hash.clone(),
        },
        role: TargetRole::Source,
        rationale: "Update the evidence-grounded source behavior".to_owned(),
        acceptance_criteria: BTreeSet::from([seed.criterion_id.clone()]),
        required_evidence: BTreeSet::from([seed.source_file.evidence_id.clone()]),
        expected_validation: expected_validation.clone(),
        dependencies: BTreeSet::new(),
        estimated_change: ChangeEstimate {
            size: ChangeSize::Small,
            risk: ChangeRisk::Low,
            estimated_changed_lines: 8,
        },
    };
    let generated_target = PlannedTargetV1 {
        target_id: TargetId::new("target:budget-companion"),
        change_id: ChangeId::new("change:budget-companion"),
        path: ProfilePath::new("src/slug_companion.rs").expect("exact companion path"),
        operation: TargetOperation::CreateFile {
            specification: CreationSpecification {
                kind: CreatedFileKind::Source,
                purpose: "Add the source companion required by the accepted criterion".to_owned(),
            },
        },
        role: TargetRole::Source,
        rationale: "Add an evidence-grounded source companion".to_owned(),
        acceptance_criteria: BTreeSet::from([seed.criterion_id.clone()]),
        required_evidence: BTreeSet::from([seed.source_file.evidence_id.clone()]),
        expected_validation,
        dependencies: BTreeSet::from([source_target.target_id.clone()]),
        estimated_change: ChangeEstimate {
            size: ChangeSize::Small,
            risk: ChangeRisk::Low,
            estimated_changed_lines: 12,
        },
    };
    let discovery = seed.state.discovery.as_ref().expect("accepted discovery");
    PlanCandidate::new(
        1,
        discovery.repository_revision.clone(),
        discovery
            .impact_map
            .as_ref()
            .expect("accepted impact map")
            .evidence_id
            .clone(),
        PlanDecisionCandidate::Changes {
            targets: vec![source_target, generated_target],
        },
    )
    .expect("bounded typed plan candidate")
}

fn accepted_plan(
    seed: &super::phase2_discovery::Phase3PlanningSeed,
    graph_budget: &PlanGraphBudgetContract,
) -> AcceptedPlan {
    let candidate = candidate_with_two_targets(seed);
    let discovery = seed.state.discovery.as_ref().expect("accepted discovery");
    let PlanValidationResult::Accepted { plan } = validate_plan_candidate(
        &candidate,
        &seed.profile,
        discovery,
        graph_budget,
        PlanMissionCapacity {
            remaining_model_calls: 100,
            remaining_cost_micros: 100_000,
            remaining_duration_ms: 100_000,
        },
    ) else {
        panic!("semantically valid plan must fit the trusted graph contract");
    };
    plan
}

#[test]
fn trusted_graph_budget_rejects_node_and_mission_overcommit_before_acceptance() {
    let seed = super::phase2_discovery::phase3_planning_seed();
    let candidate = candidate_with_two_targets(&seed);
    let discovery = seed.state.discovery.as_ref().expect("accepted discovery");
    assert!(matches!(
        validate_plan_candidate(
            &candidate,
            &seed.profile,
            discovery,
            &super::plan_graph_budget(),
            PlanMissionCapacity {
                remaining_model_calls: 100,
                remaining_cost_micros: 100_000,
                remaining_duration_ms: 100_000,
            },
        ),
        PlanValidationResult::Accepted { .. }
    ));

    let mut constrained = super::plan_graph_budget();
    constrained.max_implementation_nodes = 1;
    constrained.max_validation_nodes = 1;
    constrained.max_total_nodes = 5;
    constrained
        .validate()
        .expect("small trusted graph contract remains structurally valid");
    let PlanValidationResult::Rejected { violations } = validate_plan_candidate(
        &candidate,
        &seed.profile,
        discovery,
        &constrained,
        mission_capacity(3),
    ) else {
        panic!("node and mission overcommit must be rejected before plan acceptance");
    };
    assert!(violations.contains(&PlanViolation::ImplementationNodeLimitExceeded));
    assert!(violations.contains(&PlanViolation::ValidationNodeLimitExceeded));
    assert!(violations.contains(&PlanViolation::TotalNodeLimitExceeded));
    assert!(violations.contains(&PlanViolation::MissionBudgetInfeasible));
}

#[test]
fn accepted_plan_materializes_exact_trusted_per_kind_budgets() {
    let seed = super::phase2_discovery::phase3_planning_seed();
    let mut graph_budget = super::plan_graph_budget();
    graph_budget.implementation = super::model_budget(7);
    graph_budget.review = super::model_budget(5);
    graph_budget.completion_evaluation = super::model_budget(4);
    let plan = accepted_plan(&seed, &graph_budget);
    let materialization =
        materialize_accepted_plan(&plan, &graph_budget).expect("trusted graph materialization");

    assert_eq!(
        seed.state
            .node(&NodeId::new("protocol-v1:planning"))
            .expect("planning node")
            .budget,
        super::model_budget(2)
    );
    assert_ne!(
        seed.state
            .node(&NodeId::new("protocol-v1:planning"))
            .expect("planning node")
            .budget,
        graph_budget.implementation,
        "downstream implementation budgets must not reuse the planning-node budget"
    );

    let mut implementation_count = 0;
    let mut validation_count = 0;
    for node in &materialization.nodes {
        let expected = match node.kind {
            NodeKind::Implementation => {
                implementation_count += 1;
                &graph_budget.implementation
            }
            NodeKind::Validation => {
                validation_count += 1;
                &graph_budget.validation
            }
            NodeKind::Review => &graph_budget.review,
            NodeKind::CompletionEvaluation => &graph_budget.completion_evaluation,
            NodeKind::Publication => &graph_budget.publication,
            NodeKind::Discovery | NodeKind::Planning | NodeKind::ValidationRepair => {
                panic!("accepted-plan materialization emitted an unrelated node kind")
            }
        };
        assert_eq!(&node.budget, expected);
    }
    assert_eq!(implementation_count, 2);
    assert_eq!(validation_count, 2);
}

#[test]
fn trusted_graph_budget_is_preserved_by_replay_and_rejects_snapshot_tampering() {
    let seed = super::phase2_discovery::phase3_planning_seed();
    let trusted_graph_budget = seed.trusted_initial.plan_graph_budget.clone();
    assert_eq!(seed.state.plan_graph_budget, trusted_graph_budget);
    let restored = InMemoryEventStore::restore(seed.trusted_initial.clone(), seed.state.clone())
        .expect("trusted graph budget survives exact replay");
    assert_eq!(restored.state().plan_graph_budget, trusted_graph_budget);

    let mut tampered = seed.state;
    tampered.plan_graph_budget.implementation.max_model_calls = tampered
        .plan_graph_budget
        .implementation
        .max_model_calls
        .saturating_add(1);
    assert!(
        InMemoryEventStore::restore(seed.trusted_initial, tampered).is_err(),
        "snapshot-local graph budget changes must not override trusted bootstrap authority"
    );
}
