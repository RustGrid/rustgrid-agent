use std::collections::BTreeSet;

use super::*;
use crate::execution_protocol::reducer::{
    build_prepared_discovery_action, repository_profile_proof_hash,
};

const EDGE_EXECUTION_ID: &str = "execution-protocol-v1:phase2-edges";
const EDGE_REVISION: &str = "repository-revision:phase2-edges";

struct ActiveEdgeDiscovery {
    state: ExecutionState,
    node_id: NodeId,
}

fn edge_profile() -> RepositoryProfile {
    let inventory = RepositoryInventory::new(
        RepositoryRevisionId::new(EDGE_REVISION),
        vec![
            RepositoryFileObservation::from_bytes(
                "Cargo.toml",
                b"[package]\nname = \"edge-fixture\"\nversion = \"0.1.0\"\n",
            )
            .expect("bounded Cargo manifest"),
            RepositoryFileObservation::from_bytes(
                "src/lib.rs",
                b"pub fn normalize(value: &str) -> String { value.trim().to_owned() }\n",
            )
            .expect("bounded Rust source"),
        ],
    )
    .expect("valid generic repository inventory");
    build_repository_profile(&inventory).expect("deterministic repository profile")
}

fn active_edge_discovery(discovery_budget: NodeBudgetContract) -> ActiveEdgeDiscovery {
    active_edge_discovery_with_goal(
        discovery_budget,
        &["criterion:normalize"],
        &["normalize value", "related behavior"],
    )
}

fn active_edge_discovery_with_criteria(
    discovery_budget: NodeBudgetContract,
    criteria: &[&str],
) -> ActiveEdgeDiscovery {
    active_edge_discovery_with_goal(
        discovery_budget,
        criteria,
        &["normalize value", "related behavior"],
    )
}

fn active_edge_discovery_with_goal(
    discovery_budget: NodeBudgetContract,
    criteria: &[&str],
    search_terms: &[&str],
) -> ActiveEdgeDiscovery {
    active_edge_discovery_with_limits(
        discovery_budget,
        MissionBudgetContract {
            max_model_calls: 10,
            max_cost_micros: 10_000,
            max_duration_ms: 10_000,
        },
        criteria,
        search_terms,
    )
}

fn active_edge_discovery_with_limits(
    discovery_budget: NodeBudgetContract,
    mission_budget: MissionBudgetContract,
    criteria: &[&str],
    search_terms: &[&str],
) -> ActiveEdgeDiscovery {
    let mut state = ExecutionState::bootstrap(
        ExecutionId::new(EDGE_EXECUTION_ID),
        1,
        RepositoryRevisionId::new(EDGE_REVISION),
        mission_budget,
        discovery_budget,
        model_budget(1),
        super::plan_graph_budget(),
        None,
    );
    let profile = edge_profile();
    append(
        &mut state,
        "phase2:edges:profile",
        ProfileEvent::RepositoryProfileRecorded {
            profile: profile.clone(),
        },
    );

    let criterion_ids = criteria
        .iter()
        .map(|value| DiscoveryCriterionId::new(*value).expect("valid criterion identity"))
        .collect::<BTreeSet<_>>();
    let goal = DiscoveryGoal::new(
        stable_sha256(&["phase2-edge-goal"]),
        criterion_ids,
        search_terms.iter().map(|term| (*term).to_owned()),
    )
    .expect("valid discovery goal");
    append(
        &mut state,
        "phase2:edges:goal",
        DiscoveryEvent::GoalRecorded { goal },
    );

    let profile_proof_id = ProofId::new("proof:phase2:edges:profile");
    let repository_revision = state.repository_revision.clone();
    append(
        &mut state,
        "phase2:edges:profile-proof",
        EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                id: profile_proof_id.clone(),
                kind: ProofKind::RepositoryProfile,
                repository_revision,
                node_ids: Vec::new(),
                related_proof_ids: Vec::new(),
                related_evidence_ids: Vec::new(),
                detail_hash: repository_profile_proof_hash(&profile.profile_id),
            },
        },
    );
    append(
        &mut state,
        "phase2:edges:enter-discovery",
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Profiling,
            to: ProtocolStage::Discovery,
            proof_id: profile_proof_id,
        },
    );

    let node_id = NodeId::new("protocol-v1:discovery");
    assert_eq!(
        append_emitted(&mut state, "phase2:edges:start-discovery"),
        GraphEvent::NodeStarted {
            node_id: node_id.clone(),
            attempt: 1,
        }
        .into()
    );
    ActiveEdgeDiscovery { state, node_id }
}

fn append_emitted(state: &mut ExecutionState, semantic_key: &str) -> DomainEvent {
    let ProtocolDecision::Emit { event } = decide(state).expect("typed event decision") else {
        panic!("expected emitted event");
    };
    append(state, semantic_key, event.clone());
    event
}

fn edge_budget(max_model_calls: u32) -> NodeBudgetContract {
    NodeBudgetContract {
        max_model_calls,
        max_cost_micros: 1_000,
        max_duration_ms: 1_000,
        max_mutation_attempts: 0,
        max_context_rebuilds: 0,
        max_input_tokens_per_call: 4_096,
        max_output_tokens_per_call: 2_048,
    }
}

fn consume_provider_action(
    active: &mut ActiveEdgeDiscovery,
    label: &str,
    prepared: &PreparedDiscoveryAction,
    actual_cost_micros: u64,
    actual_duration_ms: u64,
) -> ActionEnvelope {
    assert_eq!(
        append_emitted(&mut active.state, &format!("phase2:edges:{label}:prepared"),),
        DiscoveryEvent::ActionPrepared {
            prepared: Box::new(prepared.clone()),
        }
        .into()
    );
    assert_eq!(
        append_emitted(&mut active.state, &format!("phase2:edges:{label}:admitted")),
        BudgetEvent::ModelCallAdmitted {
            admission: prepared.admission.clone(),
        }
        .into()
    );
    assert_eq!(
        append_emitted(&mut active.state, &format!("phase2:edges:{label}:reserved")),
        BudgetEvent::ModelCallReserved {
            call_id: prepared.admission.call_id.clone(),
        }
        .into()
    );

    let ProtocolDecision::Perform {
        effect: EffectRequest::Discovery(DiscoveryEffectRequest::DispatchProvider { envelope }),
    } = decide(&active.state).expect("provider dispatch decision")
    else {
        panic!("reserved discovery action must dispatch");
    };
    assert_eq!(*envelope, prepared.envelope);
    append(
        &mut active.state,
        &format!("phase2:edges:{label}:dispatched"),
        BudgetEvent::ProviderDispatchStarted {
            call_id: prepared.admission.call_id.clone(),
            payload_hash: prepared.envelope.payload_identity.clone(),
        },
    );
    append(
        &mut active.state,
        &format!("phase2:edges:{label}:reconciled"),
        BudgetEvent::ModelCallReconciled {
            call_id: prepared.admission.call_id.clone(),
            result: ModelCallReconciliation::Consumed {
                actual_cost_micros,
                duration_ms: actual_duration_ms,
            },
        },
    );
    assert_eq!(
        decide(&active.state).expect("provider observation wait"),
        ProtocolDecision::Wait {
            reason: WaitReason::DiscoveryObservation {
                action_id: prepared.envelope.action_id.clone(),
            },
        }
    );
    *envelope
}

fn complete_empty_search(
    active: &mut ActiveEdgeDiscovery,
    actual_cost_micros: u64,
    actual_duration_ms: u64,
) {
    let prepared = build_prepared_discovery_action(&active.state)
        .expect("authoritative prepared discovery action");
    consume_provider_action(
        active,
        "empty-search",
        &prepared,
        actual_cost_micros,
        actual_duration_ms,
    );

    let DiscoveryActionConstraints::Search { request } = &prepared.envelope.constraints else {
        panic!("prepared edge action must remain a search");
    };
    append(
        &mut active.state,
        "phase2:edges:empty-search:observed",
        DiscoveryEvent::SearchCompleted {
            action_id: prepared.envelope.action_id,
            evidence: SearchEvidence::new(
                active.node_id.clone(),
                request.clone(),
                BTreeSet::new(),
                false,
            )
            .expect("canonical empty search evidence"),
        },
    );
}

fn record_candidate_search(active: &mut ActiveEdgeDiscovery, label: &str) -> DiscoveryPath {
    record_candidate_search_at_path(
        active,
        label,
        DiscoveryPath::new("src/normalize.rs").expect("valid candidate path"),
        10,
        10,
    )
}

fn record_candidate_search_with_usage(
    active: &mut ActiveEdgeDiscovery,
    label: &str,
    actual_cost_micros: u64,
    actual_duration_ms: u64,
) -> DiscoveryPath {
    record_candidate_search_at_path(
        active,
        label,
        DiscoveryPath::new("src/normalize.rs").expect("valid candidate path"),
        actual_cost_micros,
        actual_duration_ms,
    )
}

fn record_candidate_search_at_path(
    active: &mut ActiveEdgeDiscovery,
    label: &str,
    path: DiscoveryPath,
    actual_cost_micros: u64,
    actual_duration_ms: u64,
) -> DiscoveryPath {
    let prepared =
        build_prepared_discovery_action(&active.state).expect("authoritative candidate search");
    consume_provider_action(
        active,
        label,
        &prepared,
        actual_cost_micros,
        actual_duration_ms,
    );
    let DiscoveryActionConstraints::Search { request } = &prepared.envelope.constraints else {
        panic!("candidate action must be a search");
    };
    let matched_paths = BTreeSet::from([path.clone()]);
    append(
        &mut active.state,
        &format!("phase2:edges:{label}:observed"),
        DiscoveryEvent::SearchCompleted {
            action_id: prepared.envelope.action_id.clone(),
            evidence: SearchEvidence::new(
                active.node_id.clone(),
                request.clone(),
                matched_paths,
                false,
            )
            .expect("canonical candidate search evidence"),
        },
    );
    assert!(matches!(
        append_emitted(
            &mut active.state,
            &format!("phase2:edges:{label}:candidates")
        ),
        DomainEvent::Discovery(DiscoveryEvent::CandidatesRecorded { .. })
    ));
    path
}

fn record_grounded_criterion_coverage(
    active: &mut ActiveEdgeDiscovery,
    label: &str,
) -> (DiscoveryPath, Vec<FileEvidence>) {
    let mut path = None;
    let mut file_evidence = Vec::new();
    for index in 0..MAX_DISCOVERY_SEARCH_TERMS.saturating_add(MAX_DISCOVERY_CANDIDATES) {
        match active
            .state
            .discovery
            .as_ref()
            .expect("aggregate discovery state")
            .substate()
        {
            DiscoverySubstate::NeedCandidates => {
                path = Some(record_candidate_search(
                    active,
                    &format!("{label}-search-{index}"),
                ));
            }
            DiscoverySubstate::NeedGroundedReads => file_evidence.extend(record_grounded_read(
                active,
                &format!("{label}-ground-{index}"),
                Vec::new(),
            )),
            DiscoverySubstate::NeedRelations | DiscoverySubstate::ReadyToSynthesize => break,
        }
    }
    assert_eq!(
        active
            .state
            .discovery
            .as_ref()
            .expect("aggregate discovery state")
            .substate(),
        DiscoverySubstate::ReadyToSynthesize
    );
    (path.expect("at least one candidate search"), file_evidence)
}

fn record_grounded_read(
    active: &mut ActiveEdgeDiscovery,
    label: &str,
    unresolved_questions: Vec<UnresolvedQuestion>,
) -> Vec<FileEvidence> {
    let prepared =
        build_prepared_discovery_action(&active.state).expect("authoritative grounded read");
    let DiscoveryActionConstraints::ExactPaths { paths } = &prepared.envelope.constraints else {
        panic!("grounded action must use exact paths");
    };
    let evidence = paths
        .iter()
        .map(|path| {
            FileEvidence::new(
                active.node_id.clone(),
                active.state.repository_revision.clone(),
                path.clone(),
                LineRange::new(1, 20).expect("valid grounded range"),
                stable_sha256(&["phase2-edge-file-content", path.as_str()]),
                stable_sha256(&["phase2-edge-file-artifact", path.as_str()]),
                TextEncoding::Utf8,
                false,
            )
            .expect("canonical grounded file evidence")
        })
        .collect::<Vec<_>>();
    consume_provider_action(active, label, &prepared, 10, 10);
    append(
        &mut active.state,
        &format!("phase2:edges:{label}:observed"),
        DiscoveryEvent::FileEvidenceRecorded {
            action_id: prepared.envelope.action_id,
            evidence: evidence.clone(),
            unresolved_questions,
        },
    );
    evidence
}

#[test]
fn exact_cost_or_duration_exhaustion_converges_before_another_admission() {
    let cases = [
        (
            NodeBudgetContract {
                max_model_calls: 3,
                max_cost_micros: 100,
                max_duration_ms: 1_000,
                max_mutation_attempts: 0,
                max_context_rebuilds: 0,
                max_input_tokens_per_call: 4_096,
                max_output_tokens_per_call: 2_048,
            },
            100,
            10,
        ),
        (
            NodeBudgetContract {
                max_model_calls: 3,
                max_cost_micros: 1_000,
                max_duration_ms: 100,
                max_mutation_attempts: 0,
                max_context_rebuilds: 0,
                max_input_tokens_per_call: 4_096,
                max_output_tokens_per_call: 2_048,
            },
            10,
            100,
        ),
    ];

    for (index, (budget, cost, duration)) in cases.into_iter().enumerate() {
        let mut active = active_edge_discovery(budget);
        complete_empty_search(&mut active, cost, duration);
        let decision = decide(&active.state).expect("exhausted discovery decision");
        let ProtocolDecision::Emit {
            event: DomainEvent::Discovery(DiscoveryEvent::ConvergenceEvaluated { convergence }),
        } = decision
        else {
            panic!("exact budget exhaustion must converge before another admission");
        };
        assert_eq!(
            convergence,
            DiscoveryConvergence::InsufficientEvidence {
                reason: InsufficientEvidenceReason::NoUsefulCandidates,
            }
        );
        append(
            &mut active.state,
            &format!("phase2:edges:budget-{index}:convergence"),
            DiscoveryEvent::ConvergenceEvaluated { convergence },
        );
        assert!(active.state.current_discovery_action.is_none());
        assert_eq!(active.state.budgets.model_calls.len(), 1);
    }
}

#[test]
fn maximum_candidate_result_is_grounded_in_bounded_deterministic_batches() {
    let mut active = active_edge_discovery(edge_budget(3));
    let search = build_prepared_discovery_action(&active.state)
        .expect("authoritative maximum-candidate search");
    consume_provider_action(&mut active, "maximum-candidates-search", &search, 10, 10);
    let DiscoveryActionConstraints::Search { request } = &search.envelope.constraints else {
        panic!("initial action must be a search");
    };
    let matched_paths = (0..MAX_DISCOVERY_CANDIDATES)
        .map(|index| {
            DiscoveryPath::new(format!("src/candidate_{index:02}.rs"))
                .expect("canonical candidate path")
        })
        .collect::<BTreeSet<_>>();
    append(
        &mut active.state,
        "phase2:edges:maximum-candidates:observed",
        DiscoveryEvent::SearchCompleted {
            action_id: search.envelope.action_id,
            evidence: SearchEvidence::new(
                active.node_id.clone(),
                request.clone(),
                matched_paths,
                false,
            )
            .expect("canonical maximum search evidence"),
        },
    );
    append_emitted(
        &mut active.state,
        "phase2:edges:maximum-candidates:projected",
    );

    let first_batch = build_prepared_discovery_action(&active.state)
        .expect("maximum legal candidates remain actionable");
    let DiscoveryActionConstraints::ExactPaths { paths } = &first_batch.envelope.constraints else {
        panic!("candidate grounding must use exact paths");
    };
    assert_eq!(paths.len(), MAX_GROUNDING_PATHS_PER_ACTION);
    assert!(first_batch.context.mandatory_sections.len() <= MAX_CONTEXT_SECTIONS);
    assert_eq!(
        first_batch
            .context
            .mandatory_sections
            .iter()
            .filter(|section| matches!(section, ContextSection::Evidence { .. }))
            .count(),
        MAX_GROUNDING_PATHS_PER_ACTION
    );
}

#[test]
fn mission_only_exact_exhaustion_converges_and_maps_to_budget_blocked() {
    let cases = [
        (
            "calls",
            MissionBudgetContract {
                max_model_calls: 1,
                max_cost_micros: 10_000,
                max_duration_ms: 10_000,
            },
            10,
            10,
        ),
        (
            "cost",
            MissionBudgetContract {
                max_model_calls: 10,
                max_cost_micros: 100,
                max_duration_ms: 10_000,
            },
            100,
            10,
        ),
        (
            "duration",
            MissionBudgetContract {
                max_model_calls: 10,
                max_cost_micros: 10_000,
                max_duration_ms: 100,
            },
            10,
            100,
        ),
    ];

    for (dimension, mission_budget, actual_cost, actual_duration) in cases {
        let discovery_budget = edge_budget(3);
        let mut active = active_edge_discovery_with_limits(
            discovery_budget.clone(),
            mission_budget,
            &["criterion:normalize"],
            &["normalize value"],
        );
        record_candidate_search_with_usage(
            &mut active,
            &format!("mission-{dimension}"),
            actual_cost,
            actual_duration,
        );

        let node_usage = &active
            .state
            .node(&active.node_id)
            .expect("active discovery node")
            .usage;
        assert!(node_usage.model_calls_consumed < discovery_budget.max_model_calls);
        assert!(node_usage.cost_micros_consumed < discovery_budget.max_cost_micros);
        assert!(node_usage.duration_ms_consumed < discovery_budget.max_duration_ms);

        let convergence = append_emitted(
            &mut active.state,
            &format!("phase2:edges:mission-{dimension}:convergence"),
        );
        assert_eq!(
            convergence,
            DiscoveryEvent::ConvergenceEvaluated {
                convergence: DiscoveryConvergence::BudgetBlocked {
                    reason: DiscoveryBudgetBlockReason::GroundedEvidenceMissing,
                },
            }
            .into()
        );
        assert!(matches!(
            append_emitted(
                &mut active.state,
                &format!("phase2:edges:mission-{dimension}:node-failed"),
            ),
            DomainEvent::Graph(GraphEvent::NodeFailed { terminal: true, .. })
        ));

        let ProtocolDecision::Finish { result } =
            decide(&active.state).expect("mission-only exhaustion terminal decision")
        else {
            panic!("mission-only exhaustion must resolve to a terminal result");
        };
        assert!(matches!(
            &result.mission,
            MissionResult::BudgetBlocked { node_id, .. } if node_id == &active.node_id
        ));
        append(
            &mut active.state,
            &format!("phase2:edges:mission-{dimension}:terminal"),
            TerminalEvent::CanonicalResultRecorded {
                result: result.clone(),
            },
        );
        assert_eq!(
            decide(&active.state).expect("canonical terminal decision"),
            ProtocolDecision::Finish { result }
        );
    }
}

#[test]
fn criterion_bound_search_does_not_fabricate_other_criterion_coverage() {
    let criterion_a =
        DiscoveryCriterionId::new("criterion:a").expect("valid first criterion identity");
    let criterion_b =
        DiscoveryCriterionId::new("criterion:b").expect("valid second criterion identity");
    let mut active = active_edge_discovery_with_goal(
        edge_budget(3),
        &["criterion:a", "criterion:b"],
        &["normalize value"],
    );
    let path = record_candidate_search(&mut active, "criterion-a-search");

    let discovery = active
        .state
        .discovery
        .as_ref()
        .expect("aggregate discovery state");
    let candidate = discovery
        .candidates
        .get(&path)
        .expect("projected criterion-A candidate");
    assert_eq!(
        candidate.criterion_ids,
        BTreeSet::from([criterion_a.clone()])
    );
    assert!(!candidate.criterion_ids.contains(&criterion_b));
    assert_eq!(discovery.substate(), DiscoverySubstate::NeedGroundedReads);
    assert_ne!(discovery.substate(), DiscoverySubstate::ReadyToSynthesize);
    let grounded = record_grounded_read(&mut active, "criterion-a-ground", Vec::new());
    assert_eq!(grounded.len(), 1);

    let discovery = active
        .state
        .discovery
        .as_ref()
        .expect("aggregate discovery state");
    assert_eq!(
        discovery.grounded_criterion_ids(),
        BTreeSet::from([criterion_a])
    );
    assert!(!discovery.grounded_criterion_ids().contains(&criterion_b));
    assert_eq!(discovery.substate(), DiscoverySubstate::NeedCandidates);
    assert_ne!(discovery.substate(), DiscoverySubstate::ReadyToSynthesize);

    let prepared = build_prepared_discovery_action(&active.state)
        .expect("authoritative missing-criterion search");
    let DiscoveryActionConstraints::Search { request } = &prepared.envelope.constraints else {
        panic!("missing criterion must be pursued by a search");
    };
    assert_eq!(request.criterion_ids, BTreeSet::from([criterion_b]));
    let context_criteria = prepared
        .context
        .mandatory_sections
        .iter()
        .filter_map(|section| match section {
            ContextSection::AcceptanceCriterion { criterion_id } => Some(criterion_id.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(context_criteria, request.criterion_ids);
    assert_eq!(prepared.context.evidence_ids, request.context_evidence_ids);
    assert!(prepared.context.optional_sections.is_empty());
    assert_eq!(
        append_emitted(
            &mut active.state,
            "phase2:edges:criterion-b-search:prepared",
        ),
        DiscoveryEvent::ActionPrepared {
            prepared: Box::new(prepared.clone()),
        }
        .into()
    );
    append_emitted(
        &mut active.state,
        "phase2:edges:criterion-b-search:admitted",
    );
    append_emitted(
        &mut active.state,
        "phase2:edges:criterion-b-search:reserved",
    );
    let ProtocolDecision::Perform {
        effect:
            EffectRequest::Discovery(DiscoveryEffectRequest::DispatchProvider {
                envelope: serialized,
            }),
    } = decide(&active.state).expect("criterion-B provider dispatch")
    else {
        panic!("criterion-B search must reach the provider boundary");
    };
    assert_eq!(*serialized, prepared.envelope);
}

#[test]
fn impact_area_rejects_evidence_from_an_unrelated_grounded_path() {
    let criterion_a =
        DiscoveryCriterionId::new("criterion:a").expect("valid first criterion identity");
    let mut active = active_edge_discovery_with_goal(
        edge_budget(6),
        &["criterion:a", "criterion:b"],
        &["normalize value"],
    );
    let path_a = DiscoveryPath::new("src/criterion_a.rs").expect("valid first path");
    record_candidate_search_at_path(&mut active, "impact-binding-a", path_a.clone(), 10, 10);
    let evidence_a = record_grounded_read(&mut active, "impact-binding-a-ground", Vec::new());

    let path_b = DiscoveryPath::new("tests/criterion_b.rs").expect("valid second path");
    record_candidate_search_at_path(&mut active, "impact-binding-b", path_b, 10, 10);
    let evidence_b = record_grounded_read(&mut active, "impact-binding-b-ground", Vec::new());
    assert_eq!(
        active
            .state
            .discovery
            .as_ref()
            .expect("aggregate discovery state")
            .substate(),
        DiscoverySubstate::ReadyToSynthesize
    );

    let map_action =
        build_prepared_discovery_action(&active.state).expect("authoritative impact action");
    consume_provider_action(&mut active, "impact-binding-map", &map_action, 10, 10);
    let discovery = active
        .state
        .discovery
        .as_ref()
        .expect("aggregate discovery state");
    let unrelated = ImpactMapEvidence::new(
        discovery.node_id.clone(),
        discovery.repository_revision.clone(),
        vec![ImpactArea {
            criterion_id: criterion_a,
            paths: BTreeSet::from([path_a]),
            evidence_ids: BTreeSet::from([evidence_b
                .first()
                .expect("second grounded evidence")
                .evidence_id
                .clone()]),
            confidence: EvidenceConfidence::Medium,
        }],
        BTreeSet::new(),
    )
    .expect("structurally canonical but semantically unrelated impact map");
    assert_ne!(
        evidence_a
            .first()
            .expect("first grounded evidence")
            .evidence_id
            .as_str(),
        unrelated.areas[0]
            .evidence_ids
            .iter()
            .next()
            .expect("unrelated evidence identity")
            .as_str()
    );

    let before = active.state.clone();
    let event = envelope(
        &active.state,
        "phase2:edges:impact-binding:unrelated",
        DiscoveryEvent::ImpactMapRecorded {
            action_id: Some(map_action.envelope.action_id),
            evidence: unrelated,
        },
    );
    assert!(matches!(
        active.state.append_event(event),
        Err(ProtocolViolation::DiscoveryContract {
            code: "impact_area_grounding_invalid"
        })
    ));
    assert_eq!(active.state, before);
}

#[test]
fn incomplete_impact_map_with_budget_remaining_requests_another_map() {
    let mut active = active_edge_discovery_with_criteria(
        edge_budget(6),
        &["criterion:behavior", "criterion:tests"],
    );
    let (path, file_evidence) = record_grounded_criterion_coverage(&mut active, "impact-coverage");
    let discovery = active
        .state
        .discovery
        .as_ref()
        .expect("aggregate discovery state");
    assert_eq!(
        discovery.grounded_criterion_ids(),
        discovery.goal.criterion_ids
    );
    assert_eq!(discovery.substate(), DiscoverySubstate::ReadyToSynthesize);
    let first_map_action = build_prepared_discovery_action(&active.state)
        .expect("authoritative initial impact-map action");
    consume_provider_action(&mut active, "impact-first-map", &first_map_action, 10, 10);

    let discovery = active
        .state
        .discovery
        .as_ref()
        .expect("aggregate discovery state");
    let first_criterion = discovery
        .goal
        .criterion_ids
        .iter()
        .next()
        .expect("at least one criterion")
        .clone();
    let file_evidence_id = file_evidence
        .first()
        .expect("grounded file evidence")
        .evidence_id
        .clone();
    let incomplete_map = ImpactMapEvidence::new(
        discovery.node_id.clone(),
        discovery.repository_revision.clone(),
        vec![ImpactArea {
            criterion_id: first_criterion,
            paths: BTreeSet::from([path]),
            evidence_ids: BTreeSet::from([file_evidence_id.clone()]),
            confidence: EvidenceConfidence::Medium,
        }],
        BTreeSet::new(),
    )
    .expect("canonical incomplete impact-map evidence");
    append(
        &mut active.state,
        "phase2:edges:impact-first-map:observed",
        DiscoveryEvent::ImpactMapRecorded {
            action_id: Some(first_map_action.envelope.action_id.clone()),
            evidence: incomplete_map.clone(),
        },
    );
    assert_eq!(
        active
            .state
            .discovery
            .as_ref()
            .and_then(|state| state.impact_map.as_ref()),
        Some(&incomplete_map)
    );

    let retry =
        build_prepared_discovery_action(&active.state).expect("authoritative impact-map retry");
    assert_ne!(
        retry.envelope.action_id,
        first_map_action.envelope.action_id
    );
    assert_eq!(
        retry.envelope.tool_names(),
        BTreeSet::from([DiscoveryTool::RecordImpactMap])
    );
    assert_eq!(
        retry.envelope.tool_choice,
        ToolChoice::Named(DiscoveryTool::RecordImpactMap)
    );

    assert_eq!(
        append_emitted(&mut active.state, "phase2:edges:impact-retry:prepared"),
        DiscoveryEvent::ActionPrepared {
            prepared: Box::new(retry.clone()),
        }
        .into()
    );
    append_emitted(&mut active.state, "phase2:edges:impact-retry:admitted");
    append_emitted(&mut active.state, "phase2:edges:impact-retry:reserved");
    let ProtocolDecision::Perform {
        effect:
            EffectRequest::Discovery(DiscoveryEffectRequest::DispatchProvider {
                envelope: serialized_retry,
            }),
    } = decide(&active.state).expect("retry provider dispatch")
    else {
        panic!("incomplete map must dispatch another map action");
    };
    assert_eq!(*serialized_retry, retry.envelope);
}

#[test]
fn exhausted_final_incomplete_map_is_replaced_by_deterministic_complete_map() {
    let mut active = active_edge_discovery_with_goal(
        edge_budget(4),
        &["criterion:behavior", "criterion:tests"],
        &["normalize value"],
    );
    let (path, file_evidence) =
        record_grounded_criterion_coverage(&mut active, "final-map-coverage");
    assert_eq!(
        active
            .state
            .node(&active.node_id)
            .expect("discovery node")
            .usage
            .model_calls_consumed,
        3
    );

    let final_map_action = build_prepared_discovery_action(&active.state)
        .expect("authoritative final impact-map action");
    consume_provider_action(&mut active, "final-map", &final_map_action, 10, 10);
    assert_eq!(
        active
            .state
            .node(&active.node_id)
            .expect("discovery node")
            .usage
            .model_calls_consumed,
        4
    );

    let discovery = active
        .state
        .discovery
        .as_ref()
        .expect("aggregate discovery state");
    let first_criterion = discovery
        .goal
        .criterion_ids
        .iter()
        .next()
        .expect("at least one criterion")
        .clone();
    let file_evidence_id = file_evidence
        .first()
        .expect("grounded file evidence")
        .evidence_id
        .clone();
    let incomplete_map = ImpactMapEvidence::new(
        discovery.node_id.clone(),
        discovery.repository_revision.clone(),
        vec![ImpactArea {
            criterion_id: first_criterion,
            paths: BTreeSet::from([path]),
            evidence_ids: BTreeSet::from([file_evidence_id]),
            confidence: EvidenceConfidence::Medium,
        }],
        BTreeSet::new(),
    )
    .expect("canonical incomplete final impact map");
    append(
        &mut active.state,
        "phase2:edges:final-map:provider-observed",
        DiscoveryEvent::ImpactMapRecorded {
            action_id: Some(final_map_action.envelope.action_id),
            evidence: incomplete_map.clone(),
        },
    );

    let convergence = append_emitted(
        &mut active.state,
        "phase2:edges:final-map:deterministic-convergence",
    );
    assert!(matches!(
        convergence,
        DomainEvent::Discovery(DiscoveryEvent::ConvergenceEvaluated {
            convergence: DiscoveryConvergence::EvidenceSufficientForDeterministicImpactMap { .. }
        })
    ));

    let ProtocolDecision::Emit {
        event:
            DomainEvent::Discovery(DiscoveryEvent::ImpactMapRecorded {
                action_id: None,
                evidence: deterministic_map,
            }),
    } = decide(&active.state).expect("deterministic replacement decision")
    else {
        panic!("exact exhaustion must replace the incomplete provider map deterministically");
    };
    assert_ne!(deterministic_map, incomplete_map);
    append(
        &mut active.state,
        "phase2:edges:final-map:deterministic-map",
        DiscoveryEvent::ImpactMapRecorded {
            action_id: None,
            evidence: deterministic_map.clone(),
        },
    );
    assert!(impact_map_is_complete(
        active
            .state
            .discovery
            .as_ref()
            .expect("aggregate discovery state"),
        &deterministic_map,
    ));

    let accepted = append_emitted(
        &mut active.state,
        "phase2:edges:final-map:accepted-convergence",
    );
    assert_eq!(
        accepted,
        DiscoveryEvent::ConvergenceEvaluated {
            convergence: DiscoveryConvergence::ImpactMapAccepted {
                evidence_id: deterministic_map.evidence_id.clone(),
            },
        }
        .into()
    );

    let mut protected = active.state.clone();
    let duplicate = envelope(
        &protected,
        "phase2:edges:final-map:replace-accepted",
        DiscoveryEvent::ImpactMapRecorded {
            action_id: None,
            evidence: deterministic_map,
        },
    );
    assert!(matches!(
        protected.append_event(duplicate),
        Err(ProtocolViolation::DiscoveryContract {
            code: "complete_impact_map_already_recorded"
        })
    ));
}

#[test]
fn grounded_unresolved_relationship_authorizes_only_its_question_and_paths() {
    let mut active = active_edge_discovery(edge_budget(4));
    let path = record_candidate_search(&mut active, "relationship-search");
    let criterion_ids = active
        .state
        .discovery
        .as_ref()
        .expect("aggregate discovery state")
        .goal
        .criterion_ids
        .clone();
    let question = UnresolvedQuestion::new(RelationshipKind::Tests, path.clone(), criterion_ids)
        .expect("canonical relationship question");
    record_grounded_read(&mut active, "relationship-ground", vec![question.clone()]);

    assert_eq!(
        active
            .state
            .discovery
            .as_ref()
            .expect("aggregate discovery state")
            .substate(),
        DiscoverySubstate::NeedRelations
    );
    let prepared =
        build_prepared_discovery_action(&active.state).expect("authoritative relationship action");
    assert!(prepared.context.mandatory_sections.contains(
        &ContextSection::UnresolvedRelationship {
            question_id: question.id.clone(),
        }
    ));
    let permitted_paths = BTreeSet::from([path]);
    assert_eq!(
        prepared.envelope.constraints,
        DiscoveryActionConstraints::NamedRelationship {
            question: question.clone(),
            paths: permitted_paths.clone(),
            targeted_search: None,
        }
    );

    assert_eq!(
        prepared.envelope.tool_names(),
        BTreeSet::from([DiscoveryTool::ReadFile, DiscoveryTool::RelatedTests])
    );
    assert_eq!(prepared.envelope.tool_choice, ToolChoice::Required);
    assert!(prepared.envelope.allowed_tools.iter().all(|authorization| {
        authorization.permitted_paths == permitted_paths
            && authorization.relationship_question_id == Some(question.id.clone())
            && authorization.search_id.is_none()
    }));
    assert!(
        !prepared
            .envelope
            .tool_names()
            .contains(&DiscoveryTool::SearchText)
    );

    assert_eq!(
        append_emitted(&mut active.state, "phase2:edges:relationship:prepared"),
        DiscoveryEvent::ActionPrepared {
            prepared: Box::new(prepared.clone()),
        }
        .into()
    );
    append_emitted(&mut active.state, "phase2:edges:relationship:admitted");
    append_emitted(&mut active.state, "phase2:edges:relationship:reserved");
    let ProtocolDecision::Perform {
        effect:
            EffectRequest::Discovery(DiscoveryEffectRequest::DispatchProvider {
                envelope: serialized_envelope,
            }),
    } = decide(&active.state).expect("relationship provider dispatch")
    else {
        panic!("relationship action must reach the provider boundary");
    };
    let serialized =
        serde_json::to_value(&*serialized_envelope).expect("serialize provider envelope");
    assert_eq!(
        serialized["constraints"]["data"]["question"],
        serde_json::to_value(&question).expect("serialize question")
    );
}

#[test]
fn rejected_consumed_action_preserves_usage_and_retries_only_with_capacity() {
    for max_model_calls in [2, 1] {
        let mut active = active_edge_discovery(edge_budget(max_model_calls));
        let prepared =
            build_prepared_discovery_action(&active.state).expect("authoritative discovery action");
        consume_provider_action(&mut active, "rejected-search", &prepared, 10, 10);
        let node_usage = active
            .state
            .node(&active.node_id)
            .expect("discovery node")
            .usage
            .clone();
        let mission_usage = active.state.budgets.mission_usage.clone();
        let call_record = active
            .state
            .budgets
            .model_calls
            .get(&prepared.admission.call_id)
            .expect("consumed call record")
            .clone();

        append(
            &mut active.state,
            &format!("phase2:edges:rejected-search:{max_model_calls}"),
            DiscoveryEvent::ActionRejected {
                action_id: prepared.envelope.action_id.clone(),
                reason: DiscoveryActionRejectionReason::InvalidSearchObservation,
            },
        );
        assert!(active.state.current_discovery_action.is_none());
        assert_eq!(
            active
                .state
                .node(&active.node_id)
                .expect("discovery node")
                .usage,
            node_usage
        );
        assert_eq!(active.state.budgets.mission_usage, mission_usage);
        assert_eq!(
            active
                .state
                .budgets
                .model_calls
                .get(&prepared.admission.call_id),
            Some(&call_record)
        );

        if max_model_calls == 2 {
            let retry =
                build_prepared_discovery_action(&active.state).expect("authoritative retry action");
            assert_ne!(retry.envelope.action_id, prepared.envelope.action_id);
            assert_eq!(retry.envelope.constraints, prepared.envelope.constraints);
            assert_eq!(
                append_emitted(&mut active.state, "phase2:edges:retry:prepared"),
                DiscoveryEvent::ActionPrepared {
                    prepared: Box::new(retry),
                }
                .into()
            );
        } else {
            assert!(matches!(
                decide(&active.state).expect("exhausted rejection decision"),
                ProtocolDecision::Emit {
                    event: DomainEvent::Discovery(DiscoveryEvent::ConvergenceEvaluated { .. })
                }
            ));
        }
    }
}
