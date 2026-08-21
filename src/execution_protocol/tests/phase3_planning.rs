use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use super::*;

const PLAN_REVISION: &str = "repository-revision:phase3-planning";

#[derive(Clone)]
struct PlanningFixture {
    profile: RepositoryProfile,
    discovery: DiscoveryState,
    source_criterion: DiscoveryCriterionId,
    test_criterion: DiscoveryCriterionId,
    source_file: FileEvidence,
    test_file: FileEvidence,
    validation: ValidationExpectation,
}

fn fixture_root(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/execution_protocol_v1/profile_discovery")
        .join(name)
        .join("repository")
}

fn collect_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(directory)
        .expect("read fixture directory")
        .map(|entry| entry.expect("read fixture entry").path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_files(root, &path, files);
        } else {
            assert!(path.starts_with(root));
            files.push(path);
        }
    }
}

fn fixture_profile(name: &str) -> RepositoryProfile {
    let root = fixture_root(name);
    let mut files = Vec::new();
    collect_files(&root, &root, &mut files);
    let observations = files
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(&root)
                .expect("fixture file below root")
                .iter()
                .map(|component| component.to_str().expect("UTF-8 fixture path"))
                .collect::<Vec<_>>()
                .join("/");
            RepositoryFileObservation::from_bytes(
                relative,
                fs::read(path).expect("read fixture file"),
            )
            .expect("bounded fixture observation")
        })
        .collect();
    build_repository_profile(
        &RepositoryInventory::new(RepositoryRevisionId::new(PLAN_REVISION), observations)
            .expect("valid fixture inventory"),
    )
    .expect("deterministic fixture profile")
}

fn file_evidence(node_id: &NodeId, path: &str) -> FileEvidence {
    file_evidence_from_fixture("candidate_grounding", node_id, path)
}

fn file_evidence_from_fixture(fixture: &str, node_id: &NodeId, path: &str) -> FileEvidence {
    let bytes = fs::read(fixture_root(fixture).join(path)).expect("read checked-in fixture file");
    let line_count = u32::try_from(bytes.split(|byte| *byte == b'\n').count()).unwrap_or(u32::MAX);
    FileEvidence::new(
        node_id.clone(),
        RepositoryRevisionId::new(PLAN_REVISION),
        DiscoveryPath::new(path).expect("valid discovery path"),
        LineRange::new(1, line_count.max(1)).expect("valid line range"),
        hex::encode(Sha256::digest(&bytes)),
        stable_sha256(&["phase3:file-artifact", path]),
        TextEncoding::Utf8,
        false,
    )
    .expect("valid file evidence")
}

fn single_path_discovery(
    fixture: &str,
    profile: &RepositoryProfile,
    path: &str,
) -> (DiscoveryState, DiscoveryCriterionId, FileEvidence) {
    let node_id = NodeId::new("protocol-v1:discovery");
    let criterion = DiscoveryCriterionId::new("criterion:single-target").unwrap();
    let goal = DiscoveryGoal::new(
        stable_sha256(&["phase3-single-path-goal", fixture, path]),
        BTreeSet::from([criterion.clone()]),
        ["single target".to_owned()],
    )
    .unwrap();
    let mut discovery = DiscoveryState::new(
        node_id.clone(),
        RepositoryRevisionId::new(PLAN_REVISION),
        profile.profile_id.clone(),
        goal,
    );
    let request = SearchRequest::new(
        discovery.repository_revision.clone(),
        profile.profile_id.clone(),
        BTreeSet::from([criterion.clone()]),
        "single target",
        SearchScope::repository(),
        Vec::<String>::new(),
        SearchMode::LiteralCaseInsensitive,
        BTreeSet::new(),
    )
    .unwrap();
    let discovery_path = DiscoveryPath::new(path).unwrap();
    let search = SearchEvidence::new(
        node_id.clone(),
        request,
        BTreeSet::from([discovery_path.clone()]),
        false,
    )
    .unwrap();
    let candidate = candidate_for_search(
        &node_id,
        path,
        1,
        &criterion,
        search.request.search_id.clone(),
    );
    discovery
        .completed_searches
        .insert(search.request.search_id.clone(), search);
    discovery
        .candidates
        .insert(discovery_path.clone(), candidate);
    let file = file_evidence_from_fixture(fixture, &node_id, path);
    discovery
        .file_evidence
        .insert(file.evidence_id.clone(), file.clone());
    let impact = ImpactMapEvidence::new(
        node_id,
        RepositoryRevisionId::new(PLAN_REVISION),
        vec![ImpactArea {
            criterion_id: criterion.clone(),
            paths: BTreeSet::from([discovery_path]),
            evidence_ids: BTreeSet::from([file.evidence_id.clone()]),
            confidence: EvidenceConfidence::High,
        }],
        BTreeSet::new(),
    )
    .unwrap();
    discovery.impact_map = Some(impact);
    discovery.convergence = Some(evaluate_discovery_convergence(&discovery));
    discovery.validate().unwrap();
    (discovery, criterion, file)
}

fn candidate_for_search(
    node_id: &NodeId,
    path: &str,
    rank: u32,
    criterion_id: &DiscoveryCriterionId,
    search_id: SearchId,
) -> CandidatePathEvidence {
    CandidatePathEvidence {
        schema_version: DISCOVERY_SCHEMA_VERSION,
        evidence_id: EvidenceId::new("pending:phase3-candidate"),
        producer_node_id: node_id.clone(),
        repository_revision: RepositoryRevisionId::new(PLAN_REVISION),
        path: DiscoveryPath::new(path).expect("valid candidate path"),
        rank,
        reasons: BTreeSet::from([CandidateReason::SearchMatch]),
        source_search_ids: BTreeSet::from([search_id]),
        criterion_ids: BTreeSet::from([criterion_id.clone()]),
    }
    .canonicalize_id()
    .expect("canonical candidate identity")
}

fn planning_fixture() -> PlanningFixture {
    let profile = fixture_profile("candidate_grounding");
    let node_id = NodeId::new("protocol-v1:discovery");
    let source_criterion = DiscoveryCriterionId::new("criterion:slug-behavior").expect("criterion");
    let test_criterion = DiscoveryCriterionId::new("criterion:slug-regression").expect("criterion");
    let goal = DiscoveryGoal::new(
        stable_sha256(&["phase3-planning-goal"]),
        BTreeSet::from([source_criterion.clone(), test_criterion.clone()]),
        ["slug behavior".to_owned(), "slug regression".to_owned()],
    )
    .expect("valid discovery goal");
    let mut discovery = DiscoveryState::new(
        node_id.clone(),
        RepositoryRevisionId::new(PLAN_REVISION),
        profile.profile_id.clone(),
        goal,
    );

    let source_request = SearchRequest::new(
        discovery.repository_revision.clone(),
        profile.profile_id.clone(),
        BTreeSet::from([source_criterion.clone()]),
        "slug behavior",
        SearchScope::repository(),
        Vec::<String>::new(),
        SearchMode::LiteralCaseInsensitive,
        BTreeSet::new(),
    )
    .expect("source search");
    let source_search = SearchEvidence::new(
        node_id.clone(),
        source_request,
        BTreeSet::from([DiscoveryPath::new("src/slug.rs").unwrap()]),
        false,
    )
    .expect("source search evidence");
    let test_request = SearchRequest::new(
        discovery.repository_revision.clone(),
        profile.profile_id.clone(),
        BTreeSet::from([test_criterion.clone()]),
        "slug regression",
        SearchScope::repository(),
        Vec::<String>::new(),
        SearchMode::LiteralCaseInsensitive,
        BTreeSet::new(),
    )
    .expect("test search");
    let test_search = SearchEvidence::new(
        node_id.clone(),
        test_request,
        BTreeSet::from([DiscoveryPath::new("tests/slug.rs").unwrap()]),
        false,
    )
    .expect("test search evidence");
    let source_candidate = candidate_for_search(
        &node_id,
        "src/slug.rs",
        1,
        &source_criterion,
        source_search.request.search_id.clone(),
    );
    let test_candidate = candidate_for_search(
        &node_id,
        "tests/slug.rs",
        2,
        &test_criterion,
        test_search.request.search_id.clone(),
    );
    discovery
        .completed_searches
        .insert(source_search.request.search_id.clone(), source_search);
    discovery
        .completed_searches
        .insert(test_search.request.search_id.clone(), test_search);
    discovery
        .candidates
        .insert(source_candidate.path.clone(), source_candidate);
    discovery
        .candidates
        .insert(test_candidate.path.clone(), test_candidate);

    let source_file = file_evidence(&node_id, "src/slug.rs");
    let test_file = file_evidence(&node_id, "tests/slug.rs");
    discovery
        .file_evidence
        .insert(source_file.evidence_id.clone(), source_file.clone());
    discovery
        .file_evidence
        .insert(test_file.evidence_id.clone(), test_file.clone());
    let impact_map = ImpactMapEvidence::new(
        node_id,
        RepositoryRevisionId::new(PLAN_REVISION),
        vec![
            ImpactArea {
                criterion_id: source_criterion.clone(),
                paths: BTreeSet::from([DiscoveryPath::new("src/slug.rs").unwrap()]),
                evidence_ids: BTreeSet::from([source_file.evidence_id.clone()]),
                confidence: EvidenceConfidence::High,
            },
            ImpactArea {
                criterion_id: test_criterion.clone(),
                paths: BTreeSet::from([DiscoveryPath::new("tests/slug.rs").unwrap()]),
                evidence_ids: BTreeSet::from([test_file.evidence_id.clone()]),
                confidence: EvidenceConfidence::High,
            },
        ],
        BTreeSet::new(),
    )
    .expect("valid impact map");
    discovery.impact_map = Some(impact_map);
    discovery.convergence = Some(evaluate_discovery_convergence(&discovery));
    discovery.validate().expect("canonical planning discovery");

    let cargo_test = profile
        .validation_candidates
        .iter()
        .find(|candidate| candidate.command == ValidationCommandKind::CargoTest)
        .expect("Cargo test candidate");
    let validation = ValidationExpectation::new(
        cargo_test.candidate_id.clone(),
        BTreeSet::from([source_criterion.clone(), test_criterion.clone()]),
    )
    .expect("profile-bound validation expectation");
    PlanningFixture {
        profile,
        discovery,
        source_criterion,
        test_criterion,
        source_file,
        test_file,
        validation,
    }
}

fn graph_model_budget() -> NodeBudgetContract {
    NodeBudgetContract {
        max_model_calls: 3,
        max_cost_micros: 10_000,
        max_duration_ms: 10_000,
        max_mutation_attempts: 2,
        max_context_rebuilds: 1,
        max_input_tokens_per_call: 4_096,
        max_output_tokens_per_call: 2_048,
    }
}

fn graph_budget() -> PlanGraphBudgetContract {
    PlanGraphBudgetContract {
        max_implementation_nodes: 32,
        max_validation_nodes: 16,
        max_total_nodes: 51,
        implementation: graph_model_budget(),
        validation: NodeBudgetContract::deterministic(),
        review: graph_model_budget(),
        completion_evaluation: graph_model_budget(),
        publication: NodeBudgetContract::deterministic(),
    }
}

fn ample_mission_capacity() -> PlanMissionCapacity {
    PlanMissionCapacity {
        remaining_model_calls: 100,
        remaining_cost_micros: 100_000,
        remaining_duration_ms: 100_000,
    }
}

fn target(
    fixture: &PlanningFixture,
    id: &str,
    path: &str,
    criterion: DiscoveryCriterionId,
    file: &FileEvidence,
    dependencies: BTreeSet<TargetId>,
) -> PlannedTargetV1 {
    PlannedTargetV1 {
        target_id: TargetId::new(id),
        change_id: ChangeId::new(format!("change:{id}")),
        path: ProfilePath::new(path).expect("exact target path"),
        operation: TargetOperation::ModifyExisting {
            expected_content_hash: file.content_hash.clone(),
        },
        role: if path.starts_with("tests/") {
            TargetRole::Test
        } else {
            TargetRole::Source
        },
        rationale: format!("Implement the behavior bound to {id}"),
        acceptance_criteria: BTreeSet::from([criterion.clone()]),
        required_evidence: BTreeSet::from([file.evidence_id.clone()]),
        expected_validation: BTreeSet::from([ValidationExpectation::new(
            fixture.validation.command_candidate_id.clone(),
            BTreeSet::from([criterion]),
        )
        .expect("target validation")]),
        dependencies,
        estimated_change: ChangeEstimate {
            size: ChangeSize::Small,
            risk: ChangeRisk::Low,
            estimated_changed_lines: 12,
        },
    }
}

fn valid_targets(fixture: &PlanningFixture) -> Vec<PlannedTargetV1> {
    let source = target(
        fixture,
        "target:source",
        "src/slug.rs",
        fixture.source_criterion.clone(),
        &fixture.source_file,
        BTreeSet::new(),
    );
    let test = target(
        fixture,
        "target:test",
        "tests/slug.rs",
        fixture.test_criterion.clone(),
        &fixture.test_file,
        BTreeSet::from([source.target_id.clone()]),
    );
    vec![source, test]
}

fn candidate(fixture: &PlanningFixture, decision: PlanDecisionCandidate) -> PlanCandidate {
    PlanCandidate::new(
        1,
        fixture.discovery.repository_revision.clone(),
        fixture
            .discovery
            .impact_map
            .as_ref()
            .expect("impact map")
            .evidence_id
            .clone(),
        decision,
    )
    .expect("typed plan candidate")
}

fn append_next_decision(state: &mut ExecutionState, semantic_key: &str) -> DomainEvent {
    let ProtocolDecision::Emit { event } = decide(state).expect("authoritative protocol decision")
    else {
        panic!("expected reducer-owned emitted event");
    };
    append(state, semantic_key, event.clone());
    event
}

fn consume_planning_action(state: &mut ExecutionState, label: &str) -> PreparedPlanningAction {
    let event = append_next_decision(state, &format!("phase3:{label}:prepared"));
    let DomainEvent::Planning(PlanningEvent::ActionPrepared { prepared }) = event else {
        panic!("planning action preparation must be authoritative");
    };
    let prepared = *prepared;
    assert_eq!(
        append_next_decision(state, &format!("phase3:{label}:admitted")),
        BudgetEvent::ModelCallAdmitted {
            admission: prepared.admission.clone(),
        }
        .into()
    );
    assert_eq!(
        append_next_decision(state, &format!("phase3:{label}:reserved")),
        BudgetEvent::ModelCallReserved {
            call_id: prepared.admission.call_id.clone(),
        }
        .into()
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Planning(PlanningEffectRequest::DispatchProvider { envelope }),
    } = decide(state).expect("planning provider dispatch")
    else {
        panic!("reserved planning action must perform its exact provider effect");
    };
    assert_eq!(*envelope, prepared.envelope);
    append(
        state,
        &format!("phase3:{label}:dispatch-started"),
        BudgetEvent::ProviderDispatchStarted {
            call_id: prepared.admission.call_id.clone(),
            payload_hash: prepared.envelope.payload_identity.clone(),
        },
    );
    append(
        state,
        &format!("phase3:{label}:reconciled"),
        BudgetEvent::ModelCallReconciled {
            call_id: prepared.admission.call_id.clone(),
            result: ModelCallReconciliation::Consumed {
                actual_cost_micros: 100,
                duration_ms: 50,
            },
        },
    );
    prepared
}

#[test]
fn valid_multi_target_plan_has_order_independent_identity_and_canonical_graph() {
    let fixture = planning_fixture();
    let targets = valid_targets(&fixture);
    let forward = candidate(
        &fixture,
        PlanDecisionCandidate::Changes {
            targets: targets.clone(),
        },
    );
    let reverse = candidate(
        &fixture,
        PlanDecisionCandidate::Changes {
            targets: targets.into_iter().rev().collect(),
        },
    );
    assert_eq!(forward, reverse);

    let PlanValidationResult::Accepted { plan } = validate_plan_candidate(
        &forward,
        &fixture.profile,
        &fixture.discovery,
        &graph_budget(),
        ample_mission_capacity(),
    ) else {
        panic!("valid typed plan must be accepted");
    };
    assert_eq!(plan.targets[0].path.as_str(), "src/slug.rs");
    assert_eq!(plan.targets[1].path.as_str(), "tests/slug.rs");
    let graph = materialize_accepted_plan(&plan, &graph_budget()).expect("canonical graph");
    assert_eq!(graph.target_nodes.len(), 2);
    assert!(!graph.validation_nodes.is_empty());
    let source_node = graph.target_nodes[&plan.targets[0].target_id].clone();
    let test_node = graph.target_nodes[&plan.targets[1].target_id].clone();
    let test_spec = graph
        .nodes
        .iter()
        .find(|node| node.id == test_node)
        .unwrap();
    assert_eq!(test_spec.dependencies, vec![source_node]);
    let implementation = graph
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::Implementation)
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    assert!(
        graph
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Validation)
            .all(|node| {
                node.dependencies.iter().cloned().collect::<BTreeSet<_>>() == implementation
            })
    );
}

#[test]
fn plan_validation_rejects_vague_unknown_uncovered_and_cyclic_targets() {
    let fixture = planning_fixture();
    let mut vague = valid_targets(&fixture);
    vague[0].path = ProfilePath::root();
    let PlanValidationResult::Rejected { violations } = validate_plan_candidate(
        &candidate(&fixture, PlanDecisionCandidate::Changes { targets: vague }),
        &fixture.profile,
        &fixture.discovery,
        &graph_budget(),
        ample_mission_capacity(),
    ) else {
        panic!("repository-scoped target must be rejected");
    };
    assert!(
        violations
            .iter()
            .any(|violation| matches!(violation, PlanViolation::RepositoryScopedTarget { .. }))
    );

    let mut invalid = valid_targets(&fixture);
    invalid[0].required_evidence = BTreeSet::from([EvidenceId::new("evidence:unknown")]);
    invalid[0].acceptance_criteria.clear();
    invalid[0].dependencies = BTreeSet::from([invalid[1].target_id.clone()]);
    let PlanValidationResult::Rejected { violations } = validate_plan_candidate(
        &candidate(
            &fixture,
            PlanDecisionCandidate::Changes { targets: invalid },
        ),
        &fixture.profile,
        &fixture.discovery,
        &graph_budget(),
        ample_mission_capacity(),
    ) else {
        panic!("invalid plan must be rejected");
    };
    assert!(
        violations
            .iter()
            .any(|violation| matches!(violation, PlanViolation::UnknownEvidence { .. }))
    );
    assert!(violations.contains(&PlanViolation::DependencyCycle));
    assert!(
        violations
            .iter()
            .any(|violation| matches!(violation, PlanViolation::CriterionUncovered { .. }))
    );
}

#[test]
fn typed_create_delete_and_move_validate_repository_state() {
    let fixture = planning_fixture();
    let mut create = target(
        &fixture,
        "target:create",
        "tests/slug_edge.rs",
        fixture.test_criterion.clone(),
        &fixture.test_file,
        BTreeSet::new(),
    );
    create.operation = TargetOperation::CreateFile {
        specification: CreationSpecification::new(
            CreatedFileKind::Test,
            "Add focused slug edge-case coverage",
        )
        .unwrap(),
    };
    let source = target(
        &fixture,
        "target:source",
        "src/slug.rs",
        fixture.source_criterion.clone(),
        &fixture.source_file,
        BTreeSet::new(),
    );
    let test_companion = target(
        &fixture,
        "target:test",
        "tests/slug.rs",
        fixture.test_criterion.clone(),
        &fixture.test_file,
        BTreeSet::from([source.target_id.clone()]),
    );
    assert!(matches!(
        validate_plan_candidate(
            &candidate(
                &fixture,
                PlanDecisionCandidate::Changes {
                    targets: vec![source.clone(), create],
                },
            ),
            &fixture.profile,
            &fixture.discovery,
            &graph_budget(),
            ample_mission_capacity(),
        ),
        PlanValidationResult::Accepted { .. }
    ));

    let mut delete = source.clone();
    delete.operation = TargetOperation::DeleteFile {
        expected_content_hash: fixture.source_file.content_hash.clone(),
    };
    assert!(matches!(
        validate_plan_candidate(
            &candidate(
                &fixture,
                PlanDecisionCandidate::Changes {
                    targets: vec![delete, test_companion.clone()]
                }
            ),
            &fixture.profile,
            &fixture.discovery,
            &graph_budget(),
            ample_mission_capacity(),
        ),
        PlanValidationResult::Accepted { .. }
    ));

    let mut moved = source;
    moved.operation = TargetOperation::MoveFile {
        destination: ProfilePath::new("src/slug_format.rs").unwrap(),
        expected_content_hash: fixture.source_file.content_hash.clone(),
    };
    assert!(matches!(
        validate_plan_candidate(
            &candidate(
                &fixture,
                PlanDecisionCandidate::Changes {
                    targets: vec![moved, test_companion]
                }
            ),
            &fixture.profile,
            &fixture.discovery,
            &graph_budget(),
            ample_mission_capacity(),
        ),
        PlanValidationResult::Accepted { .. }
    ));
}

#[test]
fn generated_output_policy_blocks_only_evidence_marked_generated_files() {
    let profile = fixture_profile("generated_openapi");
    let generated_path = "generated/openapi-client/src/apis/DefaultApi.ts";
    let (generated_discovery, criterion, generated_file) =
        single_path_discovery("generated_openapi", &profile, generated_path);
    let generated_target = PlannedTargetV1 {
        target_id: TargetId::new("target:generated-client"),
        change_id: ChangeId::new("change:generated-client"),
        path: ProfilePath::new(generated_path).unwrap(),
        operation: TargetOperation::ModifyExisting {
            expected_content_hash: generated_file.content_hash.clone(),
        },
        role: TargetRole::Source,
        rationale: "Directly modify the generated client".into(),
        acceptance_criteria: BTreeSet::from([criterion.clone()]),
        required_evidence: BTreeSet::from([generated_file.evidence_id.clone()]),
        expected_validation: BTreeSet::new(),
        dependencies: BTreeSet::new(),
        estimated_change: ChangeEstimate {
            size: ChangeSize::Small,
            risk: ChangeRisk::High,
            estimated_changed_lines: 5,
        },
    };
    let generated_candidate = PlanCandidate::new(
        1,
        generated_discovery.repository_revision.clone(),
        generated_discovery
            .impact_map
            .as_ref()
            .unwrap()
            .evidence_id
            .clone(),
        PlanDecisionCandidate::Changes {
            targets: vec![generated_target],
        },
    )
    .unwrap();
    let PlanValidationResult::Rejected { violations } = validate_plan_candidate(
        &generated_candidate,
        &profile,
        &generated_discovery,
        &graph_budget(),
        ample_mission_capacity(),
    ) else {
        panic!("direct generated output mutation must be rejected");
    };
    assert!(
        violations
            .iter()
            .any(|violation| matches!(violation, PlanViolation::GeneratedTargetForbidden { .. }))
    );

    let ordinary_path = "src/generated_summary.rs";
    let (ordinary_discovery, ordinary_criterion, ordinary_file) =
        single_path_discovery("generated_openapi", &profile, ordinary_path);
    let ordinary_target = PlannedTargetV1 {
        target_id: TargetId::new("target:ordinary-summary"),
        change_id: ChangeId::new("change:ordinary-summary"),
        path: ProfilePath::new(ordinary_path).unwrap(),
        operation: TargetOperation::ModifyExisting {
            expected_content_hash: ordinary_file.content_hash.clone(),
        },
        role: TargetRole::Source,
        rationale: "Modify an ordinary source file whose name happens to contain generated".into(),
        acceptance_criteria: BTreeSet::from([ordinary_criterion]),
        required_evidence: BTreeSet::from([ordinary_file.evidence_id]),
        expected_validation: BTreeSet::new(),
        dependencies: BTreeSet::new(),
        estimated_change: ChangeEstimate {
            size: ChangeSize::Tiny,
            risk: ChangeRisk::Low,
            estimated_changed_lines: 2,
        },
    };
    let ordinary_candidate = PlanCandidate::new(
        1,
        ordinary_discovery.repository_revision.clone(),
        ordinary_discovery
            .impact_map
            .as_ref()
            .unwrap()
            .evidence_id
            .clone(),
        PlanDecisionCandidate::Changes {
            targets: vec![ordinary_target],
        },
    )
    .unwrap();
    let PlanValidationResult::Rejected { violations } = validate_plan_candidate(
        &ordinary_candidate,
        &profile,
        &ordinary_discovery,
        &graph_budget(),
        ample_mission_capacity(),
    ) else {
        panic!("fixture has no validation candidate, so plan remains incomplete");
    };
    assert!(
        !violations
            .iter()
            .any(|violation| matches!(violation, PlanViolation::GeneratedTargetForbidden { .. }))
    );
}

fn satisfaction_observation(
    fixture: &PlanningFixture,
    criterion_id: DiscoveryCriterionId,
    evidence_id: EvidenceId,
) -> CriterionSatisfactionObservation {
    CriterionSatisfactionObservation::new(
        fixture.discovery.repository_revision.clone(),
        criterion_id,
        CriterionSatisfactionAuthority::RequiredValidationPassed {
            proof_id: ProofId::new("unavailable:required-validation-proof"),
        },
        BTreeSet::from([evidence_id]),
    )
    .expect("bounded typed criterion satisfaction observation")
}

#[test]
fn ordinary_discovery_evidence_cannot_authorize_a_no_op() {
    let fixture = planning_fixture();
    let observations = vec![
        satisfaction_observation(
            &fixture,
            fixture.source_criterion.clone(),
            fixture.source_file.evidence_id.clone(),
        ),
        satisfaction_observation(
            &fixture,
            fixture.test_criterion.clone(),
            fixture.test_file.evidence_id.clone(),
        ),
    ];
    let no_op = candidate(
        &fixture,
        PlanDecisionCandidate::NoOp {
            criterion_satisfaction: observations.clone(),
        },
    );
    let PlanValidationResult::Rejected { violations } = validate_plan_candidate(
        &no_op,
        &fixture.profile,
        &fixture.discovery,
        &graph_budget(),
        ample_mission_capacity(),
    ) else {
        panic!("provider-authored satisfaction observations must fail closed in Phase 3");
    };
    assert_eq!(
        violations
            .iter()
            .filter(|violation| matches!(
                violation,
                PlanViolation::NoOpSatisfactionProofUnavailable { .. }
            ))
            .count(),
        2
    );
    assert_eq!(
        observations[0],
        satisfaction_observation(
            &fixture,
            fixture.source_criterion.clone(),
            fixture.source_file.evidence_id.clone(),
        ),
        "criterion-satisfaction identity must be deterministic"
    );
    let mut non_strict = serde_json::to_value(&observations[0]).expect("serialize observation");
    non_strict
        .as_object_mut()
        .expect("observation is an object")
        .insert("unexpected".into(), serde_json::Value::Bool(true));
    assert!(
        serde_json::from_value::<CriterionSatisfactionObservation>(non_strict).is_err(),
        "criterion-satisfaction observations must reject unknown fields"
    );
    assert!(matches!(
        CriterionSatisfactionObservation::new(
            fixture.discovery.repository_revision.clone(),
            fixture.source_criterion.clone(),
            CriterionSatisfactionAuthority::RequiredValidationPassed {
                proof_id: ProofId::new("unavailable:required-validation-proof"),
            },
            (0..33)
                .map(|index| EvidenceId::new(format!("evidence:{index}")))
                .collect(),
        ),
        Err(PlanningContractError::InvalidCandidate {
            code: "criterion_satisfaction_support_invalid"
        })
    ));

    let invalid = candidate(
        &fixture,
        PlanDecisionCandidate::NoOp {
            criterion_satisfaction: vec![satisfaction_observation(
                &fixture,
                fixture.source_criterion.clone(),
                fixture.source_file.evidence_id.clone(),
            )],
        },
    );
    let PlanValidationResult::Rejected { violations } = validate_plan_candidate(
        &invalid,
        &fixture.profile,
        &fixture.discovery,
        &graph_budget(),
        ample_mission_capacity(),
    ) else {
        panic!("missing criterion satisfaction must be rejected");
    };
    assert!(violations.contains(&PlanViolation::NoOpCriterionMissing {
        criterion_id: fixture.test_criterion,
    }));
}

#[test]
fn real_planning_provider_boundary_accepts_plan_and_materializes_graph() {
    let seed = super::phase2_discovery::phase3_planning_seed();
    let mut state = seed.state;
    let planning_node = NodeId::new("protocol-v1:planning");
    assert_eq!(state.stage(), ProtocolStage::Planning);
    assert!(state.planning.is_some());
    assert_eq!(
        append_next_decision(&mut state, "phase3:planning:start"),
        GraphEvent::NodeStarted {
            node_id: planning_node.clone(),
            attempt: 1,
        }
        .into()
    );

    let ProtocolDecision::Emit {
        event:
            DomainEvent::Planning(PlanningEvent::ActionPrepared {
                prepared: prepared_box,
            }),
    } = decide(&state).expect("planning preparation decision")
    else {
        panic!("planning must emit an authoritative prepared action");
    };
    let prepared = *prepared_box;
    assert_eq!(
        prepared.envelope.tools,
        BTreeSet::from([PlanningTool::RecordPlan])
    );
    assert_eq!(
        prepared.envelope.tool_choice,
        PlanningToolChoice::Named {
            tool: PlanningTool::RecordPlan,
        }
    );
    assert_eq!(prepared.envelope.budget_owner_node_id, planning_node);
    let serialized = serde_json::to_string(&prepared.envelope).expect("serialize provider payload");
    assert!(serialized.contains("record_plan"));
    for forbidden in [
        "apply_patch",
        "replace_file",
        "create_file",
        "search_text",
        "read_file",
        "list_files",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "forbidden tool {forbidden}"
        );
    }

    let mut tampered = prepared.clone();
    tampered.envelope.budget_owner_node_id = NodeId::new("protocol-v1:discovery");
    let before_tamper = state.clone();
    let event = envelope(
        &state,
        "phase3:planning:tampered-preparation",
        PlanningEvent::ActionPrepared {
            prepared: Box::new(tampered),
        },
    );
    let error = state
        .append_event(event)
        .expect_err("tampered planning admission must fail");
    assert_eq!(error.code(), "planning_provider_envelope_binding_mismatch");
    assert_eq!(state, before_tamper);

    append(
        &mut state,
        "phase3:planning:prepared",
        PlanningEvent::ActionPrepared {
            prepared: Box::new(prepared.clone()),
        },
    );
    assert_eq!(
        append_next_decision(&mut state, "phase3:planning:admitted"),
        BudgetEvent::ModelCallAdmitted {
            admission: prepared.admission.clone(),
        }
        .into()
    );
    assert_eq!(
        append_next_decision(&mut state, "phase3:planning:reserved"),
        BudgetEvent::ModelCallReserved {
            call_id: prepared.admission.call_id.clone(),
        }
        .into()
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Planning(PlanningEffectRequest::DispatchProvider { envelope }),
    } = decide(&state).expect("planning provider decision")
    else {
        panic!("planning reservation must dispatch its exact provider payload");
    };
    assert_eq!(*envelope, prepared.envelope);
    assert_eq!(
        serde_json::to_vec(&*envelope).unwrap(),
        serde_json::to_vec(&prepared.envelope).unwrap()
    );
    append(
        &mut state,
        "phase3:planning:dispatch-started",
        BudgetEvent::ProviderDispatchStarted {
            call_id: prepared.admission.call_id.clone(),
            payload_hash: prepared.envelope.payload_identity.clone(),
        },
    );
    append(
        &mut state,
        "phase3:planning:reconciled",
        BudgetEvent::ModelCallReconciled {
            call_id: prepared.admission.call_id.clone(),
            result: ModelCallReconciliation::Consumed {
                actual_cost_micros: 100,
                duration_ms: 50,
            },
        },
    );

    let validation_candidate = seed
        .profile
        .validation_candidates
        .iter()
        .find(|candidate| candidate.command == ValidationCommandKind::CargoTest)
        .expect("Cargo test profile candidate");
    let expectation = ValidationExpectation::new(
        validation_candidate.candidate_id.clone(),
        BTreeSet::from([seed.criterion_id.clone()]),
    )
    .expect("profile-bound validation expectation");
    let target = PlannedTargetV1 {
        target_id: TargetId::new("target:slug-source"),
        change_id: ChangeId::new("change:slug-source"),
        path: ProfilePath::new("src/slug.rs").unwrap(),
        operation: TargetOperation::ModifyExisting {
            expected_content_hash: seed.source_file.content_hash.clone(),
        },
        role: TargetRole::Source,
        rationale: "Normalize slug behavior at the grounded source target".into(),
        acceptance_criteria: BTreeSet::from([seed.criterion_id.clone()]),
        required_evidence: BTreeSet::from([seed.source_file.evidence_id.clone()]),
        expected_validation: BTreeSet::from([expectation]),
        dependencies: BTreeSet::new(),
        estimated_change: ChangeEstimate {
            size: ChangeSize::Small,
            risk: ChangeRisk::Low,
            estimated_changed_lines: 10,
        },
    };
    let planning = state.planning.as_ref().expect("planning state");
    let plan_candidate = PlanCandidate::new(
        planning.next_revision_index(),
        state.repository_revision.clone(),
        planning.discovery_impact_map_id.clone(),
        PlanDecisionCandidate::Changes {
            targets: vec![target],
        },
    )
    .expect("typed provider plan candidate");
    let candidate_event = append(
        &mut state,
        "phase3:planning:candidate-recorded",
        PlanningEvent::CandidateRecorded {
            action_id: prepared.envelope.action_id.clone(),
            call_id: prepared.admission.call_id.clone(),
            candidate: plan_candidate,
        },
    );
    assert_eq!(
        state
            .node(&NodeId::new("protocol-v1:discovery"))
            .unwrap()
            .usage
            .model_calls_consumed,
        2
    );
    assert_eq!(
        state
            .node(&NodeId::new("protocol-v1:planning"))
            .unwrap()
            .usage
            .model_calls_consumed,
        1
    );

    let proof = append_next_decision(&mut state, "phase3:planning:accepted-proof");
    assert!(matches!(
        proof,
        DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                kind: ProofKind::PlanAccepted,
                ..
            }
        })
    ));
    let graph = append_next_decision(&mut state, "phase3:planning:graph-materialized");
    assert!(matches!(
        graph,
        DomainEvent::Graph(GraphEvent::NodesAdded { .. })
    ));
    assert_eq!(state.required_nodes(NodeKind::Implementation).len(), 1);
    assert!(!state.required_nodes(NodeKind::Validation).is_empty());
    assert_eq!(state.required_nodes(NodeKind::Review).len(), 1);
    assert_eq!(
        state.required_nodes(NodeKind::CompletionEvaluation).len(),
        1
    );
    assert_eq!(state.required_nodes(NodeKind::Publication).len(), 1);
    append_next_decision(&mut state, "phase3:planning:succeeded");
    let transition = append_next_decision(&mut state, "phase3:planning:implementation-transition");
    assert!(matches!(
        transition,
        DomainEvent::Lifecycle(LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Planning,
            to: ProtocolStage::Implementation,
            ..
        })
    ));
    assert_eq!(state.stage(), ProtocolStage::Implementation);

    let after_progress = state.clone();
    assert!(matches!(
        state.append_event(candidate_event),
        Ok(AppendOutcome::IdempotentReplay { .. })
    ));
    assert_eq!(state, after_progress);
    let restored = InMemoryEventStore::restore(seed.trusted_initial, state.clone())
        .expect("accepted plan and graph restore from event authority")
        .into_state();
    assert_eq!(restored, state);
}

#[test]
fn current_revision_discovery_evidence_cannot_emit_no_op_satisfied() {
    let seed = super::phase2_discovery::phase3_planning_seed();
    let mut state = seed.state;
    append_next_decision(&mut state, "phase3:no-op:start");
    let prepared = consume_planning_action(&mut state, "no-op");
    let planning = state.planning.as_ref().expect("planning state");
    let criterion_satisfaction = CriterionSatisfactionObservation::new(
        state.repository_revision.clone(),
        seed.criterion_id,
        CriterionSatisfactionAuthority::RequiredValidationPassed {
            proof_id: ProofId::new("unavailable:required-validation-proof"),
        },
        BTreeSet::from([seed.source_file.evidence_id]),
    )
    .expect("bounded typed criterion satisfaction observation");
    let candidate = PlanCandidate::new(
        planning.next_revision_index(),
        state.repository_revision.clone(),
        planning.discovery_impact_map_id.clone(),
        PlanDecisionCandidate::NoOp {
            criterion_satisfaction: vec![criterion_satisfaction],
        },
    )
    .expect("explicit no-op candidate");
    append(
        &mut state,
        "phase3:no-op:candidate",
        PlanningEvent::CandidateRecorded {
            action_id: prepared.envelope.action_id,
            call_id: prepared.admission.call_id,
            candidate,
        },
    );
    let planning = state.planning.as_ref().expect("planning state");
    assert!(planning.accepted_no_op.is_none());
    assert!(
        planning
            .latest_violations()
            .iter()
            .any(|violation| matches!(
                violation,
                PlanViolation::NoOpSatisfactionProofUnavailable { .. }
            ))
    );
    assert!(!matches!(
        decide(&state).expect("planning retries or converges after rejected no-op"),
        ProtocolDecision::Finish {
            result: CanonicalResult {
                mission: MissionResult::SucceededNoOp { .. },
                ..
            }
        }
    ));
    assert!(!state.event_log.iter().any(|event| matches!(
        event.envelope.payload,
        DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                kind: ProofKind::NoOpSatisfied,
                ..
            }
        })
    )));
    assert!(!state.event_log.iter().any(|event| matches!(
        event.envelope.payload,
        DomainEvent::Graph(GraphEvent::NodesAdded { .. })
    )));
    assert!(state.required_nodes(NodeKind::Implementation).is_empty());
    assert!(state.required_nodes(NodeKind::Validation).is_empty());
    InMemoryEventStore::restore(seed.trusted_initial, state)
        .expect("rejected no-op candidate restores from event authority");
}

#[test]
fn rejected_plan_retry_uses_planning_budget_and_exhausts_before_a_third_call() {
    let seed = super::phase2_discovery::phase3_planning_seed();
    let mut state = seed.state;
    append_next_decision(&mut state, "phase3:retry:start");
    let discovery_usage_before = state
        .node(&NodeId::new("protocol-v1:discovery"))
        .unwrap()
        .usage
        .clone();

    let first = consume_planning_action(&mut state, "retry:first");
    let first_candidate = {
        let planning = state.planning.as_ref().unwrap();
        PlanCandidate::new(
            planning.next_revision_index(),
            state.repository_revision.clone(),
            planning.discovery_impact_map_id.clone(),
            PlanDecisionCandidate::EvidenceGap {
                criterion_ids: BTreeSet::from([seed.criterion_id.clone()]),
                reason_code: PlanningEvidenceGapReason::TargetEvidenceMissing,
            },
        )
        .unwrap()
    };
    append(
        &mut state,
        "phase3:retry:first-candidate",
        PlanningEvent::CandidateRecorded {
            action_id: first.envelope.action_id,
            call_id: first.admission.call_id,
            candidate: first_candidate.clone(),
        },
    );

    let ProtocolDecision::Emit {
        event:
            DomainEvent::Planning(PlanningEvent::ActionPrepared {
                prepared: second_preview,
            }),
    } = decide(&state).expect("retry preparation")
    else {
        panic!("rejected candidate with remaining budget must prepare one retry");
    };
    assert_eq!(second_preview.context.plan_revision_index, 2);
    assert_eq!(
        second_preview.context.prior_candidate.as_deref(),
        Some(&first_candidate)
    );
    assert!(
        second_preview
            .context
            .prior_violations
            .iter()
            .any(|violation| { matches!(violation, PlanViolation::EvidenceGapReported { .. }) })
    );
    assert_eq!(
        second_preview.envelope.prior_candidate,
        second_preview.context.prior_candidate
    );
    assert_eq!(
        second_preview.envelope.prior_violations,
        second_preview.context.prior_violations
    );

    let second = consume_planning_action(&mut state, "retry:second");
    let second_candidate = {
        let planning = state.planning.as_ref().unwrap();
        PlanCandidate::new(
            planning.next_revision_index(),
            state.repository_revision.clone(),
            planning.discovery_impact_map_id.clone(),
            PlanDecisionCandidate::EvidenceGap {
                criterion_ids: BTreeSet::from([seed.criterion_id]),
                reason_code: PlanningEvidenceGapReason::TargetEvidenceMissing,
            },
        )
        .unwrap()
    };
    append(
        &mut state,
        "phase3:retry:second-candidate",
        PlanningEvent::CandidateRecorded {
            action_id: second.envelope.action_id,
            call_id: second.admission.call_id,
            candidate: second_candidate,
        },
    );
    assert_eq!(
        &state
            .node(&NodeId::new("protocol-v1:discovery"))
            .unwrap()
            .usage,
        &discovery_usage_before
    );
    assert_eq!(
        state
            .node(&NodeId::new("protocol-v1:planning"))
            .unwrap()
            .usage
            .model_calls_consumed,
        2
    );
    let convergence = append_next_decision(&mut state, "phase3:retry:convergence");
    assert!(matches!(
        convergence,
        DomainEvent::Planning(PlanningEvent::ConvergenceEvaluated {
            convergence: PlanningConvergence::InsufficientEvidence { .. }
        })
    ));
    append_next_decision(&mut state, "phase3:retry:planning-failed");
    let ProtocolDecision::Finish { result } = decide(&state).expect("bounded planning failure")
    else {
        panic!("planning exhaustion must finish without a third admission");
    };
    assert_eq!(
        result.mission.outcome(),
        MissionOutcomeV1::InsufficientEvidence
    );
    assert_eq!(result.process_health, ProcessHealth::Healthy);
    assert_eq!(
        state
            .budgets
            .model_calls
            .values()
            .filter(|call| call.admission.node_id == NodeId::new("protocol-v1:planning"))
            .count(),
        2
    );
}
