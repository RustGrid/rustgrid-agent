use std::{collections::BTreeSet, fs, path::PathBuf};

use super::*;
use crate::execution_protocol::reducer::repository_profile_proof_hash;

const DISCOVERY_EXECUTION_ID: &str = "execution-protocol-v1:phase2-discovery";
const DISCOVERY_REVISION: &str = "repository-revision:phase2-discovery";
const SECRET_SENTINEL: &str = "rg-discovery-secret-sentinel-41d7c5f9";

#[derive(Clone)]
struct ActiveDiscovery {
    trusted_initial: ExecutionState,
    state: ExecutionState,
    profile: RepositoryProfile,
    node_id: NodeId,
    criterion_id: DiscoveryCriterionId,
}

pub(super) struct Phase3PlanningSeed {
    pub(super) trusted_initial: ExecutionState,
    pub(super) state: ExecutionState,
    pub(super) profile: RepositoryProfile,
    pub(super) criterion_id: DiscoveryCriterionId,
    pub(super) source_file: FileEvidence,
}

pub(super) fn phase3_planning_seed() -> Phase3PlanningSeed {
    let mut active = active_discovery_with_contracts(2, 2, 6, false);
    let request = search_request(&active.state, &active.profile, "normalize slug");
    let search = prepared_action(
        &active,
        "phase3-seed:search",
        DiscoveryActionConstraints::Search { request },
    );
    prepare(&mut active, "phase3-seed:search", &search);
    consume_prepared_action(&mut active, "phase3-seed:search", &search);
    observe_search(
        &mut active,
        "phase3-seed:search",
        &search,
        BTreeSet::from([DiscoveryPath::new("src/slug.rs").expect("valid candidate path")]),
    );
    project_candidates(&mut active, "phase3-seed:search");
    let grounding = ground_ranked_candidates(&mut active, "phase3-seed:ground");
    prepare(&mut active, "phase3-seed:ground", &grounding);
    consume_prepared_action(&mut active, "phase3-seed:ground", &grounding);
    observe_grounded_files(&mut active, "phase3-seed:ground", &grounding);
    for key in [
        "phase3-seed:convergence",
        "phase3-seed:impact-map",
        "phase3-seed:impact-map-accepted",
        "phase3-seed:impact-proof",
        "phase3-seed:discovery-succeeded",
        "phase3-seed:planning-transition",
    ] {
        append_decision_event(&mut active.state, key);
    }
    let source_file = active
        .state
        .discovery
        .as_ref()
        .expect("typed discovery")
        .file_evidence
        .values()
        .find(|evidence| evidence.path.as_str() == "src/slug.rs")
        .expect("grounded source evidence")
        .clone();
    Phase3PlanningSeed {
        trusted_initial: active.trusted_initial,
        state: active.state,
        profile: active.profile,
        criterion_id: active.criterion_id,
        source_file,
    }
}

fn discovery_bootstrap(max_model_calls: u32) -> ExecutionState {
    discovery_bootstrap_with_contracts(max_model_calls, 1, max_model_calls)
}

fn discovery_bootstrap_with_contracts(
    discovery_model_calls: u32,
    planning_model_calls: u32,
    mission_model_calls: u32,
) -> ExecutionState {
    ExecutionState::bootstrap(
        ExecutionId::new(DISCOVERY_EXECUTION_ID),
        1,
        RepositoryRevisionId::new(DISCOVERY_REVISION),
        mission_budget(mission_model_calls),
        model_budget(discovery_model_calls),
        model_budget(planning_model_calls),
        super::plan_graph_budget(),
        None,
    )
}

fn repository_profile(include_secret: bool) -> RepositoryProfile {
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/execution_protocol_v1/profile_discovery")
        .join("candidate_grounding/repository");
    let mut observations = [
        "Cargo.toml",
        "docs/slug-rules.md",
        "src/lib.rs",
        "src/slug.rs",
        "tests/slug.rs",
    ]
    .into_iter()
    .map(|path| {
        RepositoryFileObservation::from_bytes(
            path,
            fs::read(fixture_root.join(path)).expect("read checked-in discovery fixture"),
        )
        .expect("bounded checked-in repository observation")
    })
    .collect::<Vec<_>>();
    if include_secret {
        observations.push(
            RepositoryFileObservation::from_bytes("config/private.txt", SECRET_SENTINEL.as_bytes())
                .expect("bounded private observation"),
        );
    }
    let inventory =
        RepositoryInventory::new(RepositoryRevisionId::new(DISCOVERY_REVISION), observations)
            .expect("valid bounded inventory");
    build_repository_profile(&inventory).expect("deterministic repository profile")
}

fn active_discovery(max_model_calls: u32, include_secret: bool) -> ActiveDiscovery {
    active_discovery_with_contracts(max_model_calls, 1, max_model_calls, include_secret)
}

fn active_discovery_with_contracts(
    discovery_model_calls: u32,
    planning_model_calls: u32,
    mission_model_calls: u32,
    include_secret: bool,
) -> ActiveDiscovery {
    let mut state = discovery_bootstrap_with_contracts(
        discovery_model_calls,
        planning_model_calls,
        mission_model_calls,
    );
    let trusted_initial = state.clone();
    let profile = repository_profile(include_secret);
    append(
        &mut state,
        "phase2:profile:recorded",
        ProfileEvent::RepositoryProfileRecorded {
            profile: profile.clone(),
        },
    );

    let criterion_id =
        DiscoveryCriterionId::new("criterion:normalize-slug").expect("valid discovery criterion");
    let goal = DiscoveryGoal::new(
        stable_sha256(&["phase2-discovery-goal"]),
        BTreeSet::from([criterion_id.clone()]),
        ["Normalize   Slug".to_owned()],
    )
    .expect("valid normalized discovery goal");
    append(
        &mut state,
        "phase2:discovery:goal",
        DiscoveryEvent::GoalRecorded { goal },
    );

    let profile_proof_id = ProofId::new("proof:phase2:repository-profile");
    let repository_revision = state.repository_revision.clone();
    append(
        &mut state,
        "phase2:profile:proof",
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
        "phase2:profile:advance-to-discovery",
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Profiling,
            to: ProtocolStage::Discovery,
            proof_id: profile_proof_id,
        },
    );

    let node_id = NodeId::new("protocol-v1:discovery");
    assert_eq!(
        append_decision_event(&mut state, "phase2:discovery:start"),
        GraphEvent::NodeStarted {
            node_id: node_id.clone(),
            attempt: 1,
        }
        .into()
    );

    ActiveDiscovery {
        trusted_initial,
        state,
        profile,
        node_id,
        criterion_id,
    }
}

fn search_request(
    state: &ExecutionState,
    profile: &RepositoryProfile,
    query: &str,
) -> SearchRequest {
    SearchRequest::new(
        state.repository_revision.clone(),
        profile.profile_id.clone(),
        state
            .discovery
            .as_ref()
            .expect("typed discovery state")
            .goal
            .criterion_ids
            .clone(),
        query,
        SearchScope::repository(),
        Vec::<String>::new(),
        SearchMode::LiteralCaseInsensitive,
        BTreeSet::new(),
    )
    .expect("canonical bounded search request")
}

fn prepared_action(
    active: &ActiveDiscovery,
    _label: &str,
    constraints: DiscoveryActionConstraints,
) -> PreparedDiscoveryAction {
    let prepared = build_prepared_discovery_action(&active.state)
        .expect("authoritative discovery admission policy");
    assert_eq!(prepared.envelope.constraints, constraints);
    prepared
}

fn prepare(active: &mut ActiveDiscovery, label: &str, prepared: &PreparedDiscoveryAction) {
    assert_eq!(
        append_decision_event(
            &mut active.state,
            &format!("phase2:discovery:{label}:prepared"),
        ),
        DiscoveryEvent::ActionPrepared {
            prepared: Box::new(prepared.clone()),
        }
        .into()
    );
}

fn append_decision_event(state: &mut ExecutionState, semantic_key: &str) -> DomainEvent {
    let ProtocolDecision::Emit { event } = decide(state).expect("typed event decision") else {
        panic!("expected an emitted protocol event");
    };
    append(state, semantic_key, event.clone());
    event
}

fn consume_prepared_action(
    active: &mut ActiveDiscovery,
    label: &str,
    prepared: &PreparedDiscoveryAction,
) -> ActionEnvelope {
    assert_eq!(
        append_decision_event(
            &mut active.state,
            &format!("phase2:discovery:{label}:admitted")
        ),
        BudgetEvent::ModelCallAdmitted {
            admission: prepared.admission.clone(),
        }
        .into()
    );
    assert_eq!(
        append_decision_event(
            &mut active.state,
            &format!("phase2:discovery:{label}:reserved")
        ),
        BudgetEvent::ModelCallReserved {
            call_id: prepared.admission.call_id.clone(),
        }
        .into()
    );

    let ProtocolDecision::Perform {
        effect: EffectRequest::Discovery(DiscoveryEffectRequest::DispatchProvider { envelope }),
    } = decide(&active.state).expect("provider dispatch decision")
    else {
        panic!("reserved discovery call must expose the typed provider payload");
    };
    assert_eq!(*envelope, prepared.envelope);
    assert_eq!(
        serde_json::to_vec(&*envelope).expect("serialize decided provider envelope"),
        serde_json::to_vec(&prepared.envelope).expect("serialize prepared provider envelope")
    );

    append(
        &mut active.state,
        &format!("phase2:discovery:{label}:dispatched"),
        BudgetEvent::ProviderDispatchStarted {
            call_id: prepared.admission.call_id.clone(),
            payload_hash: envelope.payload_identity.clone(),
        },
    );
    append(
        &mut active.state,
        &format!("phase2:discovery:{label}:reconciled"),
        BudgetEvent::ModelCallReconciled {
            call_id: prepared.admission.call_id.clone(),
            result: ModelCallReconciliation::Consumed {
                actual_cost_micros: 80,
                duration_ms: 80,
            },
        },
    );
    assert_eq!(
        decide(&active.state).expect("observation wait decision"),
        ProtocolDecision::Wait {
            reason: WaitReason::DiscoveryObservation {
                action_id: prepared.envelope.action_id.clone(),
            },
        }
    );
    *envelope
}

fn observe_search(
    active: &mut ActiveDiscovery,
    label: &str,
    prepared: &PreparedDiscoveryAction,
    matched_paths: BTreeSet<DiscoveryPath>,
) -> ProtocolEventEnvelope {
    let DiscoveryActionConstraints::Search { request } = &prepared.envelope.constraints else {
        panic!("search observation requires a search action");
    };
    let evidence = SearchEvidence::new(
        active.node_id.clone(),
        request.clone(),
        matched_paths,
        false,
    )
    .expect("canonical search evidence");
    append(
        &mut active.state,
        &format!("phase2:discovery:{label}:search-observed"),
        DiscoveryEvent::SearchCompleted {
            action_id: prepared.envelope.action_id.clone(),
            evidence,
        },
    )
}

fn project_candidates(active: &mut ActiveDiscovery, label: &str) {
    let event = append_decision_event(
        &mut active.state,
        &format!("phase2:discovery:{label}:candidates"),
    );
    assert!(matches!(
        event,
        DomainEvent::Discovery(DiscoveryEvent::CandidatesRecorded { .. })
    ));
}

fn ground_ranked_candidates(active: &mut ActiveDiscovery, label: &str) -> PreparedDiscoveryAction {
    let discovery = active.state.discovery.as_ref().expect("discovery state");
    let paths = discovery
        .ranked_candidate_paths()
        .into_iter()
        .collect::<BTreeSet<_>>();
    prepared_action(
        active,
        label,
        DiscoveryActionConstraints::ExactPaths { paths },
    )
}

fn observe_grounded_files(
    active: &mut ActiveDiscovery,
    label: &str,
    prepared: &PreparedDiscoveryAction,
) {
    let DiscoveryActionConstraints::ExactPaths { paths } = &prepared.envelope.constraints else {
        panic!("grounded observation requires exact paths");
    };
    let evidence = paths
        .iter()
        .map(|path| {
            FileEvidence::new(
                active.node_id.clone(),
                active.state.repository_revision.clone(),
                path.clone(),
                LineRange::new(1, 20).expect("valid bounded file range"),
                stable_sha256(&["phase2-discovery-file", path.as_str()]),
                stable_sha256(&["phase2-discovery-artifact-reference", path.as_str()]),
                TextEncoding::Utf8,
                false,
            )
            .expect("canonical file evidence")
        })
        .collect::<Vec<_>>();
    append(
        &mut active.state,
        &format!("phase2:discovery:{label}:files-observed"),
        DiscoveryEvent::FileEvidenceRecorded {
            action_id: prepared.envelope.action_id.clone(),
            evidence,
            unresolved_questions: Vec::new(),
        },
    );
}

fn completed_model_call_count(state: &ExecutionState) -> usize {
    state
        .budgets
        .model_calls
        .values()
        .filter(|record| matches!(record.state, ModelCallState::ReconciledConsumed { .. }))
        .count()
}

#[test]
fn search_identity_normalizes_equivalent_requests_and_binds_repository_revision() {
    let profile = repository_profile(false);
    let revision = RepositoryRevisionId::new(DISCOVERY_REVISION);
    let criterion_ids =
        BTreeSet::from([DiscoveryCriterionId::new("criterion:search-identity")
            .expect("valid criterion identity")]);
    let canonical = SearchRequest::new(
        revision.clone(),
        profile.profile_id.clone(),
        criterion_ids.clone(),
        "  Normalize    SLUG  ",
        SearchScope::repository(),
        [".RS".to_owned()],
        SearchMode::LiteralCaseInsensitive,
        BTreeSet::new(),
    )
    .expect("canonical search request");
    let equivalent = SearchRequest::new(
        revision,
        profile.profile_id.clone(),
        criterion_ids.clone(),
        "normalize slug",
        SearchScope::repository(),
        ["rs".to_owned()],
        SearchMode::LiteralCaseInsensitive,
        BTreeSet::new(),
    )
    .expect("equivalent search request");
    let changed_revision = SearchRequest::new(
        RepositoryRevisionId::new("repository-revision:phase2-discovery-next"),
        profile.profile_id,
        criterion_ids,
        "normalize slug",
        SearchScope::repository(),
        ["rs".to_owned()],
        SearchMode::LiteralCaseInsensitive,
        BTreeSet::new(),
    )
    .expect("revision-bound search request");

    assert_eq!(canonical, equivalent);
    assert_eq!(canonical.id(), equivalent.id());
    assert_ne!(canonical.id(), changed_revision.id());
}

#[test]
fn completed_search_rejects_semantic_duplicate_and_exact_event_replay_is_idempotent() {
    let mut active = active_discovery(2, false);
    let request = search_request(&active.state, &active.profile, "normalize slug");
    let first = prepared_action(
        &active,
        "duplicate:first",
        DiscoveryActionConstraints::Search {
            request: request.clone(),
        },
    );
    prepare(&mut active, "duplicate:first", &first);
    consume_prepared_action(&mut active, "duplicate:first", &first);
    let observed = observe_search(&mut active, "duplicate:first", &first, BTreeSet::new());

    let before_replay = active.state.clone();
    assert!(matches!(
        active
            .state
            .append_event(observed.clone())
            .expect("exact search observation replay"),
        AppendOutcome::IdempotentReplay { .. }
    ));
    assert_eq!(active.state, before_replay);

    let mut duplicate = first.clone();
    let DiscoveryActionConstraints::Search {
        request: duplicate_request,
    } = &mut duplicate.envelope.constraints
    else {
        panic!("first action was a search");
    };
    *duplicate_request = request;
    let duplicate_event = envelope(
        &active.state,
        "phase2:discovery:duplicate:second:prepared",
        DiscoveryEvent::ActionPrepared {
            prepared: Box::new(duplicate),
        },
    );
    let error = active
        .state
        .append_event(duplicate_event)
        .expect_err("equivalent completed search must be rejected");
    assert!(matches!(error, ProtocolViolation::DuplicateSearch { .. }));
    assert_eq!(error.code(), "duplicate_discovery_search");
}

#[test]
fn provider_boundary_serializes_initial_search_then_final_call_as_grounded_read_only() {
    let mut active = active_discovery(2, false);
    let request = search_request(&active.state, &active.profile, "normalize slug");
    let search = prepared_action(
        &active,
        "provider:search",
        DiscoveryActionConstraints::Search { request },
    );
    assert_eq!(
        search.envelope.tool_names(),
        BTreeSet::from([DiscoveryTool::ListFiles, DiscoveryTool::SearchText])
    );
    assert_eq!(search.envelope.tool_choice, ToolChoice::Required);
    let mut non_authoritative = search.clone();
    non_authoritative.admission.reserved_cost_micros = non_authoritative
        .admission
        .reserved_cost_micros
        .saturating_sub(1);
    let before_non_authoritative = active.state.clone();
    let non_authoritative_event = envelope(
        &active.state,
        "phase2:discovery:provider:non-authoritative-admission",
        DiscoveryEvent::ActionPrepared {
            prepared: Box::new(non_authoritative),
        },
    );
    assert_eq!(
        active.state.append_event(non_authoritative_event),
        Err(ProtocolViolation::DiscoveryContract {
            code: "discovery_action_not_authoritative",
        })
    );
    assert_eq!(active.state, before_non_authoritative);
    prepare(&mut active, "provider:search", &search);
    let dispatched_search = consume_prepared_action(&mut active, "provider:search", &search);
    assert_eq!(dispatched_search, search.envelope);
    let DiscoveryActionConstraints::Search { request } = &search.envelope.constraints else {
        panic!("provider search action must bind its request");
    };
    let matched_paths =
        BTreeSet::from([DiscoveryPath::new("src/slug.rs").expect("valid candidate path")]);
    let mut tampered_evidence = SearchEvidence::new(
        active.node_id.clone(),
        request.clone(),
        matched_paths.clone(),
        false,
    )
    .expect("canonical search evidence");
    tampered_evidence.evidence_id = EvidenceId::new("evidence:tampered-search-identity");
    let before_tamper = active.state.clone();
    let tampered_event = envelope(
        &active.state,
        "phase2:discovery:provider:tampered-search",
        DiscoveryEvent::SearchCompleted {
            action_id: search.envelope.action_id.clone(),
            evidence: tampered_evidence,
        },
    );
    assert!(matches!(
        active.state.append_event(tampered_event),
        Err(ProtocolViolation::DiscoveryContract {
            code: "search_evidence_binding_invalid"
        })
    ));
    assert_eq!(active.state, before_tamper);
    observe_search(&mut active, "provider:search", &search, matched_paths);
    project_candidates(&mut active, "provider:search");

    assert_eq!(completed_model_call_count(&active.state), 1);
    let grounding = ground_ranked_candidates(&mut active, "provider:ground");
    assert_eq!(
        grounding.envelope.tool_names(),
        BTreeSet::from([DiscoveryTool::ReadFile])
    );
    assert_eq!(
        grounding.envelope.tool_choice,
        ToolChoice::Named(DiscoveryTool::ReadFile)
    );
    assert!(
        !grounding
            .envelope
            .tool_names()
            .contains(&DiscoveryTool::SearchText)
    );
    prepare(&mut active, "provider:ground", &grounding);
    let dispatched_grounding = consume_prepared_action(&mut active, "provider:ground", &grounding);
    assert_eq!(
        dispatched_grounding.tool_names(),
        BTreeSet::from([DiscoveryTool::ReadFile])
    );
    observe_grounded_files(&mut active, "provider:ground", &grounding);
    assert_eq!(completed_model_call_count(&active.state), 2);
}

#[test]
fn exact_exhaustion_with_grounded_evidence_converges_and_replays_without_an_extra_call() {
    let mut active = active_discovery(2, true);
    let request = search_request(&active.state, &active.profile, "normalize slug");
    let search = prepared_action(
        &active,
        "success:search",
        DiscoveryActionConstraints::Search { request },
    );
    prepare(&mut active, "success:search", &search);
    consume_prepared_action(&mut active, "success:search", &search);
    observe_search(
        &mut active,
        "success:search",
        &search,
        BTreeSet::from([DiscoveryPath::new("src/slug.rs").expect("valid candidate path")]),
    );
    project_candidates(&mut active, "success:search");

    let grounding = ground_ranked_candidates(&mut active, "success:ground");
    prepare(&mut active, "success:ground", &grounding);
    consume_prepared_action(&mut active, "success:ground", &grounding);
    observe_grounded_files(&mut active, "success:ground", &grounding);
    assert_eq!(completed_model_call_count(&active.state), 2);

    let convergence =
        append_decision_event(&mut active.state, "phase2:discovery:success:convergence");
    assert!(matches!(
        convergence,
        DomainEvent::Discovery(DiscoveryEvent::ConvergenceEvaluated {
            convergence: DiscoveryConvergence::EvidenceSufficientForDeterministicImpactMap { .. }
        })
    ));
    let impact_map =
        append_decision_event(&mut active.state, "phase2:discovery:success:impact-map");
    assert!(matches!(
        impact_map,
        DomainEvent::Discovery(DiscoveryEvent::ImpactMapRecorded {
            action_id: None,
            ..
        })
    ));
    let accepted = append_decision_event(
        &mut active.state,
        "phase2:discovery:success:impact-map-accepted",
    );
    assert!(matches!(
        accepted,
        DomainEvent::Discovery(DiscoveryEvent::ConvergenceEvaluated {
            convergence: DiscoveryConvergence::ImpactMapAccepted { .. }
        })
    ));
    let proof = append_decision_event(&mut active.state, "phase2:discovery:success:impact-proof");
    assert!(matches!(
        proof,
        DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                kind: ProofKind::DiscoveryImpactMap,
                ..
            }
        })
    ));
    let node_success =
        append_decision_event(&mut active.state, "phase2:discovery:success:node-succeeded");
    assert!(matches!(
        node_success,
        DomainEvent::Graph(GraphEvent::NodeSucceeded { .. })
    ));
    let transition = append_decision_event(
        &mut active.state,
        "phase2:discovery:success:planning-transition",
    );
    assert!(matches!(
        transition,
        DomainEvent::Lifecycle(LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Discovery,
            to: ProtocolStage::Planning,
            ..
        })
    ));
    assert_eq!(active.state.stage(), ProtocolStage::Planning);
    assert!(matches!(
        active.state.node(&active.node_id).map(|node| &node.state),
        Some(NodeState::Succeeded { .. })
    ));
    assert_eq!(completed_model_call_count(&active.state), 2);
    assert_eq!(active.state.budgets.model_calls.len(), 2);

    let restored =
        InMemoryEventStore::restore(active.trusted_initial.clone(), active.state.clone())
            .expect("Phase 2 discovery state restores from committed events")
            .into_state();
    assert_eq!(restored, active.state);

    let mut tampered = active.state.clone();
    tampered.event_log[0]
        .envelope
        .semantic_key
        .push_str(":tampered");
    assert!(InMemoryEventStore::restore(active.trusted_initial, tampered).is_err());

    let state_json = serde_json::to_string(&active.state).expect("serialize discovery state");
    let events_json = serde_json::to_string(&active.state.event_log).expect("serialize events");
    let state_debug = format!("{:?}", active.state);
    for formatted in [state_json, events_json, state_debug] {
        assert!(!formatted.contains(SECRET_SENTINEL));
    }
}

#[test]
fn exact_exhaustion_distinguishes_no_evidence_from_ungrounded_candidates() {
    let mut empty = active_discovery(1, false);
    let empty_request = search_request(&empty.state, &empty.profile, "normalize slug");
    let empty_search = prepared_action(
        &empty,
        "empty:search",
        DiscoveryActionConstraints::Search {
            request: empty_request,
        },
    );
    prepare(&mut empty, "empty:search", &empty_search);
    consume_prepared_action(&mut empty, "empty:search", &empty_search);
    observe_search(&mut empty, "empty:search", &empty_search, BTreeSet::new());
    let empty_convergence =
        append_decision_event(&mut empty.state, "phase2:discovery:empty:convergence");
    assert_eq!(
        empty_convergence,
        DiscoveryEvent::ConvergenceEvaluated {
            convergence: DiscoveryConvergence::InsufficientEvidence {
                reason: InsufficientEvidenceReason::NoUsefulCandidates,
            },
        }
        .into()
    );
    assert!(matches!(
        append_decision_event(&mut empty.state, "phase2:discovery:empty:node-failed"),
        DomainEvent::Graph(GraphEvent::NodeFailed { terminal: true, .. })
    ));
    let ProtocolDecision::Finish { result } =
        decide(&empty.state).expect("insufficient evidence terminal decision")
    else {
        panic!("failed discovery must resolve a terminal result");
    };
    assert!(matches!(
        result.mission,
        MissionResult::InsufficientEvidence { .. }
    ));
    append(
        &mut empty.state,
        "phase2:discovery:empty:terminal",
        TerminalEvent::CanonicalResultRecorded {
            result: result.clone(),
        },
    );
    assert_eq!(
        decide(&empty.state).expect("canonical result is authoritative"),
        ProtocolDecision::Finish { result }
    );
    assert_eq!(completed_model_call_count(&empty.state), 1);
    assert_eq!(empty.state.budgets.model_calls.len(), 1);

    let mut ungrounded = active_discovery(1, false);
    let candidate_request =
        search_request(&ungrounded.state, &ungrounded.profile, "normalize slug");
    let candidate_search = prepared_action(
        &ungrounded,
        "ungrounded:search",
        DiscoveryActionConstraints::Search {
            request: candidate_request,
        },
    );
    prepare(&mut ungrounded, "ungrounded:search", &candidate_search);
    consume_prepared_action(&mut ungrounded, "ungrounded:search", &candidate_search);
    observe_search(
        &mut ungrounded,
        "ungrounded:search",
        &candidate_search,
        BTreeSet::from([DiscoveryPath::new("src/slug.rs").expect("valid candidate path")]),
    );
    project_candidates(&mut ungrounded, "ungrounded:search");
    let budget_convergence = append_decision_event(
        &mut ungrounded.state,
        "phase2:discovery:ungrounded:convergence",
    );
    assert_eq!(
        budget_convergence,
        DiscoveryEvent::ConvergenceEvaluated {
            convergence: DiscoveryConvergence::BudgetBlocked {
                reason: DiscoveryBudgetBlockReason::GroundedEvidenceMissing,
            },
        }
        .into()
    );
    assert!(matches!(
        append_decision_event(
            &mut ungrounded.state,
            "phase2:discovery:ungrounded:node-failed"
        ),
        DomainEvent::Graph(GraphEvent::NodeFailed { terminal: true, .. })
    ));
    let ProtocolDecision::Finish { result } =
        decide(&ungrounded.state).expect("budget block terminal decision")
    else {
        panic!("budget-blocked discovery must resolve a terminal result");
    };
    assert!(matches!(
        result.mission,
        MissionResult::BudgetBlocked { .. }
    ));
    assert_eq!(completed_model_call_count(&ungrounded.state), 1);
    assert_eq!(ungrounded.state.budgets.model_calls.len(), 1);
}
