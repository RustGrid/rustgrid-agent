use std::collections::BTreeSet;

use super::*;

fn planning_budget() -> NodeBudgetContract {
    NodeBudgetContract {
        max_model_calls: 2,
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
        implementation: planning_budget(),
        validation: NodeBudgetContract::deterministic(),
        review: planning_budget(),
        completion_evaluation: planning_budget(),
        publication: NodeBudgetContract::deterministic(),
    }
}

fn mission_capacity() -> PlanMissionCapacity {
    PlanMissionCapacity {
        remaining_model_calls: 100,
        remaining_cost_micros: 100_000,
        remaining_duration_ms: 100_000,
    }
}

fn validation_expectation(
    profile: &RepositoryProfile,
    criterion_ids: BTreeSet<DiscoveryCriterionId>,
) -> ValidationExpectation {
    let candidate = profile
        .validation_candidates
        .iter()
        .find(|candidate| candidate.command == ValidationCommandKind::CargoTest)
        .expect("fixture profile exposes cargo test validation");
    ValidationExpectation::new(candidate.candidate_id.clone(), criterion_ids)
        .expect("bounded validation expectation")
}

fn source_target(
    evidence: &FileEvidence,
    criterion_ids: BTreeSet<DiscoveryCriterionId>,
    expected_validation: BTreeSet<ValidationExpectation>,
) -> PlannedTargetV1 {
    PlannedTargetV1 {
        target_id: TargetId::new("target:source"),
        change_id: ChangeId::new("change:source"),
        path: ProfilePath::new("src/slug.rs").expect("exact source path"),
        operation: TargetOperation::ModifyExisting {
            expected_content_hash: evidence.content_hash.clone(),
        },
        role: TargetRole::Source,
        rationale: "Change the grounded slug implementation".into(),
        acceptance_criteria: criterion_ids,
        required_evidence: BTreeSet::from([evidence.evidence_id.clone()]),
        expected_validation,
        dependencies: BTreeSet::new(),
        estimated_change: ChangeEstimate {
            size: ChangeSize::Small,
            risk: ChangeRisk::Low,
            estimated_changed_lines: 8,
        },
    }
}

fn candidate_for(
    discovery: &DiscoveryState,
    targets: Vec<PlannedTargetV1>,
) -> Result<PlanCandidate, PlanningContractError> {
    PlanCandidate::new(
        1,
        discovery.repository_revision.clone(),
        discovery
            .impact_map
            .as_ref()
            .expect("accepted impact map")
            .evidence_id
            .clone(),
        PlanDecisionCandidate::Changes { targets },
    )
}

fn second_criterion_discovery(
    original: &DiscoveryState,
    share_source_path: bool,
) -> (DiscoveryState, DiscoveryCriterionId, FileEvidence) {
    let mut discovery = original.clone();
    let original_criterion = discovery
        .goal
        .criterion_ids
        .iter()
        .next()
        .expect("original criterion")
        .clone();
    let second_criterion =
        DiscoveryCriterionId::new("criterion:slug-regression").expect("second criterion");
    discovery
        .goal
        .criterion_ids
        .insert(second_criterion.clone());

    let path = if share_source_path {
        DiscoveryPath::new("src/slug.rs").unwrap()
    } else {
        DiscoveryPath::new("tests/slug.rs").unwrap()
    };
    let query = discovery
        .goal
        .normalized_search_terms
        .iter()
        .next()
        .expect("normalized discovery query")
        .clone();
    let request = SearchRequest::new(
        discovery.repository_revision.clone(),
        discovery.repository_profile_id.clone(),
        BTreeSet::from([second_criterion.clone()]),
        &query,
        SearchScope::repository(),
        Vec::<String>::new(),
        SearchMode::LiteralCaseInsensitive,
        BTreeSet::new(),
    )
    .expect("criterion-specific search");
    let search = SearchEvidence::new(
        discovery.node_id.clone(),
        request,
        BTreeSet::from([path.clone()]),
        false,
    )
    .expect("bounded search evidence");
    let search_id = search.request.search_id.clone();
    discovery
        .completed_searches
        .insert(search_id.clone(), search);

    let source_file = discovery
        .file_evidence
        .values()
        .find(|evidence| evidence.path.as_str() == "src/slug.rs")
        .expect("source file evidence")
        .clone();
    let second_file = if share_source_path {
        let source_candidate = discovery
            .candidates
            .get_mut(&path)
            .expect("source candidate");
        source_candidate
            .criterion_ids
            .insert(second_criterion.clone());
        source_candidate.source_search_ids.insert(search_id);
        *source_candidate = source_candidate
            .clone()
            .canonicalize_id()
            .expect("canonical shared candidate");
        source_file.clone()
    } else {
        let candidate = CandidatePathEvidence {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            evidence_id: EvidenceId::new("pending:hardening-candidate"),
            producer_node_id: discovery.node_id.clone(),
            repository_revision: discovery.repository_revision.clone(),
            path: path.clone(),
            rank: 2,
            reasons: BTreeSet::from([CandidateReason::SearchMatch]),
            source_search_ids: BTreeSet::from([search_id]),
            criterion_ids: BTreeSet::from([second_criterion.clone()]),
        }
        .canonicalize_id()
        .expect("canonical split candidate");
        discovery.candidates.insert(path.clone(), candidate);
        let file = FileEvidence::new(
            discovery.node_id.clone(),
            discovery.repository_revision.clone(),
            path.clone(),
            LineRange::new(1, 20).unwrap(),
            stable_sha256(&["phase3-hardening-file", path.as_str()]),
            stable_sha256(&["phase3-hardening-artifact", path.as_str()]),
            TextEncoding::Utf8,
            false,
        )
        .expect("bounded split file evidence");
        discovery
            .file_evidence
            .insert(file.evidence_id.clone(), file.clone());
        file
    };

    let mut areas = vec![
        ImpactArea {
            criterion_id: original_criterion,
            paths: BTreeSet::from([DiscoveryPath::new("src/slug.rs").unwrap()]),
            evidence_ids: BTreeSet::from([source_file.evidence_id.clone()]),
            confidence: EvidenceConfidence::High,
        },
        ImpactArea {
            criterion_id: second_criterion.clone(),
            paths: BTreeSet::from([path]),
            evidence_ids: BTreeSet::from([second_file.evidence_id.clone()]),
            confidence: EvidenceConfidence::High,
        },
    ];
    areas.sort_by(|left, right| left.criterion_id.cmp(&right.criterion_id));
    let impact_map = ImpactMapEvidence::new(
        discovery.node_id.clone(),
        discovery.repository_revision.clone(),
        areas,
        BTreeSet::new(),
    )
    .expect("canonical two-criterion impact map");
    discovery.impact_map = Some(impact_map);
    discovery.convergence = Some(evaluate_discovery_convergence(&discovery));
    discovery
        .validate()
        .expect("two-criterion discovery remains authoritative");
    (discovery, second_criterion, source_file)
}

fn discovery_with_truncated_source(original: &DiscoveryState) -> (DiscoveryState, FileEvidence) {
    let mut discovery = original.clone();
    let source = discovery
        .file_evidence
        .values()
        .find(|evidence| evidence.path.as_str() == "src/slug.rs")
        .expect("source evidence")
        .clone();
    let truncated = FileEvidence::new(
        source.producer_node_id,
        source.repository_revision,
        source.path.clone(),
        source.line_range,
        source.content_hash,
        source.artifact_reference_hash,
        source.encoding,
        true,
    )
    .expect("canonical truncated evidence");
    discovery.file_evidence.clear();
    discovery
        .file_evidence
        .insert(truncated.evidence_id.clone(), truncated.clone());
    let criterion = discovery
        .goal
        .criterion_ids
        .iter()
        .next()
        .expect("criterion")
        .clone();
    let impact_map = ImpactMapEvidence::new(
        discovery.node_id.clone(),
        discovery.repository_revision.clone(),
        vec![ImpactArea {
            criterion_id: criterion,
            paths: BTreeSet::from([truncated.path.clone()]),
            evidence_ids: BTreeSet::from([truncated.evidence_id.clone()]),
            confidence: EvidenceConfidence::High,
        }],
        BTreeSet::new(),
    )
    .expect("truncated impact map remains a typed discovery observation");
    discovery.impact_map = Some(impact_map);
    discovery.convergence = Some(evaluate_discovery_convergence(&discovery));
    discovery
        .validate()
        .expect("truncated discovery evidence is structurally valid");
    (discovery, truncated)
}

fn append_next_decision(state: &mut ExecutionState, semantic_key: &str) -> DomainEvent {
    let ProtocolDecision::Emit { event } = decide(state).expect("authoritative decision") else {
        panic!("expected an emitted event");
    };
    super::append(state, semantic_key, event.clone());
    event
}

fn consume_planning_action(state: &mut ExecutionState) -> PreparedPlanningAction {
    append_next_decision(state, "phase3-hardening:planning-start");
    let DomainEvent::Planning(PlanningEvent::ActionPrepared { prepared }) =
        append_next_decision(state, "phase3-hardening:action-prepared")
    else {
        panic!("planning action must be prepared");
    };
    let prepared = *prepared;
    append_next_decision(state, "phase3-hardening:action-admitted");
    append_next_decision(state, "phase3-hardening:action-reserved");
    let ProtocolDecision::Perform {
        effect: EffectRequest::Planning(PlanningEffectRequest::DispatchProvider { envelope }),
    } = decide(state).expect("planning dispatch")
    else {
        panic!("planning action must dispatch");
    };
    assert_eq!(*envelope, prepared.envelope);
    super::append(
        state,
        "phase3-hardening:dispatch-started",
        BudgetEvent::ProviderDispatchStarted {
            call_id: prepared.admission.call_id.clone(),
            payload_hash: prepared.envelope.payload_identity.clone(),
        },
    );
    super::append(
        state,
        "phase3-hardening:call-reconciled",
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
fn target_criterion_claims_require_that_criterions_impact_area() {
    let seed = super::phase2_discovery::phase3_planning_seed();
    let original = seed.state.discovery.as_ref().expect("discovery state");
    let (discovery, second_criterion, source_file) = second_criterion_discovery(original, false);
    let criteria = BTreeSet::from([seed.criterion_id.clone(), second_criterion]);
    let target = source_target(
        &source_file,
        criteria.clone(),
        BTreeSet::from([validation_expectation(&seed.profile, criteria)]),
    );

    assert!(matches!(
        validate_plan_candidate(
            &candidate_for(&discovery, vec![target]).unwrap(),
            &seed.profile,
            &discovery,
            &graph_budget(),
            mission_capacity(),
        ),
        PlanValidationResult::Rejected { .. }
    ));
}

#[test]
fn validation_expectations_collectively_cover_every_target_criterion() {
    let seed = super::phase2_discovery::phase3_planning_seed();
    let original = seed.state.discovery.as_ref().expect("discovery state");
    let (discovery, second_criterion, source_file) = second_criterion_discovery(original, true);
    let criteria = BTreeSet::from([seed.criterion_id.clone(), second_criterion]);
    let target = source_target(
        &source_file,
        criteria,
        BTreeSet::from([validation_expectation(
            &seed.profile,
            BTreeSet::from([seed.criterion_id]),
        )]),
    );

    assert!(matches!(
        validate_plan_candidate(
            &candidate_for(&discovery, vec![target]).unwrap(),
            &seed.profile,
            &discovery,
            &graph_budget(),
            mission_capacity(),
        ),
        PlanValidationResult::Rejected { .. }
    ));
}

#[test]
fn truncated_file_evidence_cannot_authorize_modify_delete_or_move() {
    let seed = super::phase2_discovery::phase3_planning_seed();
    let original = seed.state.discovery.as_ref().expect("discovery state");
    let (discovery, truncated) = discovery_with_truncated_source(original);
    let criteria = BTreeSet::from([seed.criterion_id.clone()]);
    let base = source_target(
        &truncated,
        criteria.clone(),
        BTreeSet::from([validation_expectation(&seed.profile, criteria)]),
    );
    let operations = [
        TargetOperation::ModifyExisting {
            expected_content_hash: truncated.content_hash.clone(),
        },
        TargetOperation::DeleteFile {
            expected_content_hash: truncated.content_hash.clone(),
        },
        TargetOperation::MoveFile {
            destination: ProfilePath::new("src/slug_moved.rs").unwrap(),
            expected_content_hash: truncated.content_hash.clone(),
        },
    ];

    for operation in operations {
        let mut target = base.clone();
        target.operation = operation;
        assert!(matches!(
            validate_plan_candidate(
                &candidate_for(&discovery, vec![target]).unwrap(),
                &seed.profile,
                &discovery,
                &graph_budget(),
                mission_capacity(),
            ),
            PlanValidationResult::Rejected { .. }
        ));
    }
}

#[test]
fn oversized_provider_candidate_is_rejected_atomically_before_recording() {
    let seed = super::phase2_discovery::phase3_planning_seed();
    let mut state = seed.state;
    let prepared = consume_planning_action(&mut state);
    let discovery = state.discovery.as_ref().expect("discovery state");
    let criteria = BTreeSet::from([seed.criterion_id.clone()]);
    let target = source_target(
        &seed.source_file,
        criteria.clone(),
        BTreeSet::from([validation_expectation(&seed.profile, criteria)]),
    );
    let mut candidate = candidate_for(discovery, vec![target]).expect("valid bounded candidate");
    let PlanDecisionCandidate::Changes { targets } = &mut candidate.decision else {
        panic!("change candidate expected");
    };
    targets[0].rationale = "x".repeat(64 * 1024);
    let candidate: PlanCandidate = serde_json::from_slice(
        &serde_json::to_vec(&candidate).expect("serialize hostile provider response"),
    )
    .expect("deserialize provider candidate at the wire boundary");
    let before = state.clone();
    let event = super::envelope(
        &state,
        "phase3-hardening:oversized-candidate",
        PlanningEvent::CandidateRecorded {
            action_id: prepared.envelope.action_id.clone(),
            call_id: prepared.admission.call_id.clone(),
            candidate,
        },
    );

    assert!(state.append_event(event).is_err());
    assert_eq!(state, before);
    assert!(
        state
            .planning
            .as_ref()
            .expect("planning state")
            .candidate_records
            .is_empty()
    );
    super::append(
        &mut state,
        "phase3-hardening:oversized-candidate-rejected",
        PlanningEvent::ActionRejected {
            action_id: prepared.envelope.action_id,
            reason: PlanningActionRejectionReason::InvalidPlanObservation,
        },
    );
    assert!(state.current_planning_action.is_none());
    assert!(
        state
            .planning
            .as_ref()
            .expect("planning state")
            .candidate_records
            .is_empty()
    );
}

#[test]
fn candidate_must_fit_the_signed_provider_output_allowance() {
    let seed = super::phase2_discovery::phase3_planning_seed();
    let mut state = seed.state;
    let prepared = consume_planning_action(&mut state);
    let discovery = state.discovery.as_ref().expect("discovery state");
    let criteria = BTreeSet::from([seed.criterion_id.clone()]);
    let mut target = source_target(
        &seed.source_file,
        criteria.clone(),
        BTreeSet::from([validation_expectation(&seed.profile, criteria)]),
    );
    target.rationale = "x".repeat(2_048);
    let candidate = candidate_for(discovery, vec![target]).expect("structurally bounded candidate");
    assert!(
        serde_json::to_vec(&candidate).unwrap().len()
            > usize::try_from(prepared.envelope.output_token_allowance).unwrap()
    );
    let before = state.clone();
    let event = super::envelope(
        &state,
        "phase3-hardening:output-allowance-candidate",
        PlanningEvent::CandidateRecorded {
            action_id: prepared.envelope.action_id.clone(),
            call_id: prepared.admission.call_id,
            candidate,
        },
    );
    assert!(state.append_event(event).is_err());
    assert_eq!(state, before);
    super::append(
        &mut state,
        "phase3-hardening:output-allowance-rejected",
        PlanningEvent::ActionRejected {
            action_id: prepared.envelope.action_id,
            reason: PlanningActionRejectionReason::InvalidPlanObservation,
        },
    );
    assert!(state.current_planning_action.is_none());
    assert!(
        state
            .planning
            .as_ref()
            .expect("planning state")
            .candidate_records
            .is_empty()
    );
}

#[test]
fn semantic_plan_identity_ignores_provider_chosen_target_and_change_labels() {
    let seed = super::phase2_discovery::phase3_planning_seed();
    let discovery = seed.state.discovery.as_ref().expect("discovery state");
    let criteria = BTreeSet::from([seed.criterion_id.clone()]);
    let target = source_target(
        &seed.source_file,
        criteria.clone(),
        BTreeSet::from([validation_expectation(&seed.profile, criteria)]),
    );
    let original = candidate_for(discovery, vec![target.clone()]).unwrap();
    let mut renamed = target;
    renamed.target_id = TargetId::new("target:arbitrary-provider-alias");
    renamed.change_id = ChangeId::new("change:arbitrary-provider-alias");
    let renamed = candidate_for(discovery, vec![renamed]).unwrap();

    assert_eq!(original.plan_id, renamed.plan_id);
    assert_eq!(original.plan_revision_id, renamed.plan_revision_id);
}
