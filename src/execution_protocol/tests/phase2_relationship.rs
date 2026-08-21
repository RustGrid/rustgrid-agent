use std::collections::BTreeSet;

use super::*;
use crate::execution_protocol::reducer::repository_profile_proof_hash;

const EXECUTION: &str = "execution-protocol-v1:phase2-relationship";
const REVISION: &str = "repository-revision:phase2-relationship";

fn append_emitted(state: &mut ExecutionState, semantic_key: &str) -> DomainEvent {
    let ProtocolDecision::Emit { event } = decide(state).expect("typed event decision") else {
        panic!("expected an emitted protocol event");
    };
    append(state, semantic_key, event.clone());
    event
}

fn consume_action(state: &mut ExecutionState, label: &str, prepared: &PreparedDiscoveryAction) {
    assert_eq!(
        append_emitted(state, &format!("phase2:relationship:{label}:prepared"),),
        DiscoveryEvent::ActionPrepared {
            prepared: Box::new(prepared.clone()),
        }
        .into()
    );
    assert_eq!(
        append_emitted(state, &format!("phase2:relationship:{label}:admitted")),
        BudgetEvent::ModelCallAdmitted {
            admission: prepared.admission.clone(),
        }
        .into()
    );
    assert_eq!(
        append_emitted(state, &format!("phase2:relationship:{label}:reserved")),
        BudgetEvent::ModelCallReserved {
            call_id: prepared.admission.call_id.clone(),
        }
        .into()
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Discovery(DiscoveryEffectRequest::DispatchProvider { envelope }),
    } = decide(state).expect("provider dispatch decision")
    else {
        panic!("reserved relationship action must dispatch");
    };
    assert_eq!(*envelope, prepared.envelope);
    append(
        state,
        &format!("phase2:relationship:{label}:dispatched"),
        BudgetEvent::ProviderDispatchStarted {
            call_id: prepared.admission.call_id.clone(),
            payload_hash: prepared.envelope.payload_identity.clone(),
        },
    );
    append(
        state,
        &format!("phase2:relationship:{label}:reconciled"),
        BudgetEvent::ModelCallReconciled {
            call_id: prepared.admission.call_id.clone(),
            result: ModelCallReconciliation::Consumed {
                actual_cost_micros: 10,
                duration_ms: 10,
            },
        },
    );
}

fn active_discovery() -> (ExecutionState, NodeId) {
    let mut state = ExecutionState::bootstrap(
        ExecutionId::new(EXECUTION),
        1,
        RepositoryRevisionId::new(REVISION),
        mission_budget(8),
        model_budget(8),
        model_budget(1),
        super::plan_graph_budget(),
        None,
    );
    let profile = build_repository_profile(
        &RepositoryInventory::new(
            RepositoryRevisionId::new(REVISION),
            vec![
                RepositoryFileObservation::from_bytes(
                    "Cargo.toml",
                    b"[package]\nname = \"relationship-fixture\"\nversion = \"0.1.0\"\n",
                )
                .expect("bounded manifest"),
                RepositoryFileObservation::from_bytes(
                    "src/lib.rs",
                    b"pub fn normalize(value: &str) -> String { value.trim().to_owned() }\n",
                )
                .expect("bounded source"),
            ],
        )
        .expect("bounded repository inventory"),
    )
    .expect("deterministic repository profile");
    append(
        &mut state,
        "phase2:relationship:profile",
        ProfileEvent::RepositoryProfileRecorded {
            profile: profile.clone(),
        },
    );
    let criterion_a = DiscoveryCriterionId::new("criterion:behavior").expect("criterion A");
    let criterion_b = DiscoveryCriterionId::new("criterion:tests").expect("criterion B");
    append(
        &mut state,
        "phase2:relationship:goal",
        DiscoveryEvent::GoalRecorded {
            goal: DiscoveryGoal::new(
                stable_sha256(&["phase2-relationship-goal"]),
                BTreeSet::from([criterion_a, criterion_b]),
                ["normalize".to_owned()],
            )
            .expect("bounded discovery goal"),
        },
    );
    let profile_proof_id = ProofId::new("proof:phase2:relationship:profile");
    let repository_revision = state.repository_revision.clone();
    append(
        &mut state,
        "phase2:relationship:profile-proof",
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
        "phase2:relationship:enter-discovery",
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Profiling,
            to: ProtocolStage::Discovery,
            proof_id: profile_proof_id,
        },
    );
    let node_id = NodeId::new("protocol-v1:discovery");
    assert_eq!(
        append_emitted(&mut state, "phase2:relationship:start"),
        GraphEvent::NodeStarted {
            node_id: node_id.clone(),
            attempt: 1,
        }
        .into()
    );
    (state, node_id)
}

fn observe_search(
    state: &mut ExecutionState,
    node_id: &NodeId,
    label: &str,
    matched_paths: BTreeSet<DiscoveryPath>,
) {
    let prepared = build_prepared_discovery_action(state).expect("authoritative search action");
    consume_action(state, label, &prepared);
    let DiscoveryActionConstraints::Search { request } = &prepared.envelope.constraints else {
        panic!("candidate discovery must be a search");
    };
    append(
        state,
        &format!("phase2:relationship:{label}:observed"),
        DiscoveryEvent::SearchCompleted {
            action_id: prepared.envelope.action_id,
            evidence: SearchEvidence::new(node_id.clone(), request.clone(), matched_paths, false)
                .expect("canonical search evidence"),
        },
    );
    assert!(matches!(
        append_emitted(state, &format!("phase2:relationship:{label}:candidates")),
        DomainEvent::Discovery(DiscoveryEvent::CandidatesRecorded { .. })
    ));
}

#[test]
fn relationship_result_is_bound_to_subject_context_and_may_name_safe_related_test() {
    let (mut state, node_id) = active_discovery();
    let subject = DiscoveryPath::new("src/normalize.rs").expect("normalized subject path");
    let unrelated = DiscoveryPath::new("src/unrelated.rs").expect("normalized unrelated path");
    observe_search(
        &mut state,
        &node_id,
        "criterion-a-search",
        BTreeSet::from([subject.clone(), unrelated.clone()]),
    );

    let ground = build_prepared_discovery_action(&state).expect("authoritative grounding action");
    let DiscoveryActionConstraints::ExactPaths { paths } = &ground.envelope.constraints else {
        panic!("grounding must authorize exact candidate paths");
    };
    assert_eq!(paths, &BTreeSet::from([subject.clone(), unrelated.clone()]));
    consume_action(&mut state, "ground-candidates", &ground);
    let evidence = paths
        .iter()
        .map(|path| {
            FileEvidence::new(
                node_id.clone(),
                state.repository_revision.clone(),
                path.clone(),
                LineRange::new(1, 8).expect("bounded source range"),
                stable_sha256(&["relationship-file", path.as_str()]),
                stable_sha256(&["relationship-artifact", path.as_str()]),
                TextEncoding::Utf8,
                false,
            )
            .expect("canonical grounded evidence")
        })
        .collect::<Vec<_>>();
    let criterion_a = state
        .discovery
        .as_ref()
        .expect("discovery state")
        .candidates
        .get(&subject)
        .expect("subject candidate")
        .criterion_ids
        .iter()
        .next()
        .expect("criterion A")
        .clone();
    let criterion_b = state
        .discovery
        .as_ref()
        .expect("discovery state")
        .goal
        .criterion_ids
        .iter()
        .find(|criterion| *criterion != &criterion_a)
        .expect("criterion B")
        .clone();
    let invalid_question = UnresolvedQuestion::new(
        RelationshipKind::TestedBy,
        subject.clone(),
        BTreeSet::from([criterion_b]),
    )
    .expect("well-formed but ungrounded question criterion");
    let before_invalid_question = state.clone();
    let invalid_question_event = envelope(
        &state,
        "phase2:relationship:invalid-question-criterion",
        DiscoveryEvent::FileEvidenceRecorded {
            action_id: ground.envelope.action_id.clone(),
            evidence: evidence.clone(),
            unresolved_questions: vec![invalid_question],
        },
    );
    assert_eq!(
        state.append_event(invalid_question_event),
        Err(ProtocolViolation::DiscoveryContract {
            code: "unresolved_question_observation_invalid",
        })
    );
    assert_eq!(state, before_invalid_question);

    let question = UnresolvedQuestion::new(
        RelationshipKind::TestedBy,
        subject.clone(),
        BTreeSet::from([criterion_a]),
    )
    .expect("subject-bound unresolved question");
    append(
        &mut state,
        "phase2:relationship:grounded-with-question",
        DiscoveryEvent::FileEvidenceRecorded {
            action_id: ground.envelope.action_id,
            evidence: evidence.clone(),
            unresolved_questions: vec![question.clone()],
        },
    );

    observe_search(
        &mut state,
        &node_id,
        "criterion-b-search",
        BTreeSet::from([subject.clone()]),
    );
    assert_eq!(
        state
            .discovery
            .as_ref()
            .expect("discovery state")
            .substate(),
        DiscoverySubstate::NeedRelations
    );

    let relation =
        build_prepared_discovery_action(&state).expect("authoritative relationship action");
    let DiscoveryActionConstraints::NamedRelationship {
        question: prepared_question,
        paths,
        targeted_search,
    } = &relation.envelope.constraints
    else {
        panic!("relationship action must retain its typed question");
    };
    assert_eq!(prepared_question, &question);
    assert_eq!(paths, &BTreeSet::from([subject.clone()]));
    assert!(targeted_search.is_none());
    let related_tests = relation
        .envelope
        .allowed_tools
        .iter()
        .find(|authorization| authorization.tool == DiscoveryTool::RelatedTests)
        .expect("related-tests authorization");
    assert_eq!(related_tests.permitted_paths, *paths);

    let subject_file = evidence
        .iter()
        .find(|item| item.path == subject)
        .expect("subject file evidence")
        .evidence_id
        .clone();
    let unrelated_file = evidence
        .iter()
        .find(|item| item.path == unrelated)
        .expect("unrelated file evidence")
        .evidence_id
        .clone();
    assert!(relation.context.evidence_ids.contains(&subject_file));
    assert!(!relation.context.evidence_ids.contains(&unrelated_file));
    consume_action(&mut state, "resolve-related-test", &relation);

    let related_test =
        DiscoveryPath::new("tests/normalize.rs").expect("safe normalized related-test path");
    let unrelated_support = RelationshipEvidence::new(
        node_id.clone(),
        state.repository_revision.clone(),
        subject.clone(),
        related_test.clone(),
        RelationshipKind::TestedBy,
        BTreeSet::from([unrelated_file]),
    )
    .expect("well-formed relationship evidence");
    let mut invalid_discovery = state.discovery.clone().expect("discovery state");
    invalid_discovery.relationships.insert(
        unrelated_support.evidence_id.clone(),
        unrelated_support.clone(),
    );
    assert_eq!(
        invalid_discovery.validate(),
        Err(DiscoveryContractError::InvalidAction {
            code: "relationship_evidence_binding_invalid",
        })
    );
    let before_unrelated_support = state.clone();
    let unrelated_support_event = envelope(
        &state,
        "phase2:relationship:unrelated-support",
        DiscoveryEvent::RelationshipEvidenceRecorded {
            action_id: relation.envelope.action_id.clone(),
            evidence: vec![unrelated_support],
        },
    );
    assert_eq!(
        state.append_event(unrelated_support_event),
        Err(ProtocolViolation::DiscoveryContract {
            code: "relationship_support_outside_prepared_context",
        })
    );
    assert_eq!(state, before_unrelated_support);

    let relationship = RelationshipEvidence::new(
        node_id,
        state.repository_revision.clone(),
        subject,
        related_test.clone(),
        RelationshipKind::TestedBy,
        BTreeSet::from([subject_file]),
    )
    .expect("subject-grounded relationship evidence");
    append(
        &mut state,
        "phase2:relationship:recorded",
        DiscoveryEvent::RelationshipEvidenceRecorded {
            action_id: relation.envelope.action_id,
            evidence: vec![relationship.clone()],
        },
    );
    let discovery = state.discovery.as_ref().expect("discovery state");
    assert_eq!(
        discovery.relationships.get(&relationship.evidence_id),
        Some(&relationship)
    );
    assert!(discovery.unresolved_questions.is_empty());
    assert!(!discovery.candidates.contains_key(&related_test));
    validate_state(&state).expect("relationship event preserves aggregate invariants");
}
