use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::execution_protocol::reducer::repository_profile_proof_hash;

fn strict_profile() -> RepositoryProfile {
    let inventory = RepositoryInventory::new(
        RepositoryRevisionId::new(REPOSITORY_REVISION),
        vec![
            RepositoryFileObservation::from_bytes(
                "Cargo.toml",
                b"[package]\nname = \"strict-bootstrap-fixture\"\nversion = \"0.1.0\"\n",
            )
            .expect("bounded Cargo manifest"),
            RepositoryFileObservation::from_bytes("src/lib.rs", b"pub fn fixture() {}\n")
                .expect("bounded Rust source"),
        ],
    )
    .expect("canonical strict-bootstrap inventory");
    build_repository_profile(&inventory).expect("strict-bootstrap profile")
}

fn validation_policy(profile: &RepositoryProfile) -> ValidationPolicyV1 {
    let candidate = profile
        .validation_candidates
        .first()
        .expect("Rust fixture has a validation candidate");
    let (gate_class, parser) = match candidate.command {
        ValidationCommandKind::CargoTest => {
            (ValidationGateClass::TestSuite, ValidationParserKind::Cargo)
        }
        ValidationCommandKind::CargoBuild => {
            (ValidationGateClass::Build, ValidationParserKind::Cargo)
        }
        command => panic!("unexpected strict-bootstrap validation candidate: {command:?}"),
    };
    ValidationPolicyV1::new(
        EvidenceId::new("policy-evidence:strict-bootstrap-validation"),
        profile,
        vec![ValidationCommandAuthorization {
            candidate_id: candidate.candidate_id.clone(),
            gate_class,
            parser,
            timeout_ms: 30_000,
            output_limit_bytes: 4_096,
            max_runs: 1,
            environment_fingerprint: stable_sha256(&[
                "execution-protocol-v1:strict-bootstrap-environment",
            ]),
            dependency_fingerprint: stable_sha256(&[
                "execution-protocol-v1:strict-bootstrap-dependencies",
            ]),
        }],
        BTreeSet::new(),
        model_budget(1),
        1,
        Vec::new(),
    )
    .expect("valid strict validation policy")
}

fn strict_goal() -> DiscoveryGoal {
    DiscoveryGoal::new(
        stable_sha256(&["execution-protocol-v1:strict-bootstrap-goal"]),
        BTreeSet::from([DiscoveryCriterionId::new("criterion:strict-bootstrap")
            .expect("valid strict criterion")]),
        ["strict bootstrap fixture".to_owned()],
    )
    .expect("valid strict discovery goal")
}

fn finalization_policy_for(base_repository_revision: RepositoryRevisionId) -> FinalizationPolicyV1 {
    let publication = PublicationContractV1::new(
        PublicationModeV1::Normal,
        stable_sha256(&["execution-protocol-v1:strict-bootstrap-repository-binding"]),
        stable_sha256(&["execution-protocol-v1:strict-bootstrap-installation-binding"]),
        base_repository_revision,
        "refs/heads/main".into(),
        "refs/heads/rustgrid/strict-bootstrap".into(),
        None,
        stable_sha256(&["execution-protocol-v1:strict-bootstrap-commit-identity"]),
        1,
        1,
        1,
    )
    .expect("valid strict publication contract");
    FinalizationPolicyV1::new(
        EvidenceId::new("policy-evidence:strict-bootstrap-finalization"),
        8,
        4,
        8 * 1024,
        32 * 1024,
        1,
        BTreeMap::new(),
        publication,
    )
    .expect("valid strict finalization policy")
}

fn finalization_policy() -> FinalizationPolicyV1 {
    finalization_policy_for(RepositoryRevisionId::new(REPOSITORY_REVISION))
}

fn strict_bootstrap() -> ExecutionState {
    let profile = strict_profile();
    ExecutionState::bootstrap_strict_v1(
        ExecutionId::new(EXECUTION_ID),
        1,
        RepositoryRevisionId::new(REPOSITORY_REVISION),
        mission_budget(10),
        model_budget(2),
        model_budget(2),
        plan_graph_budget(),
        strict_goal(),
        validation_policy(&profile),
        finalization_policy(),
    )
    .expect("fully policy-bound strict bootstrap")
}

fn context(
    causation_id: Option<EventId>,
    correlation_id: &str,
    node_id: Option<NodeId>,
) -> ProtocolEventContext {
    ProtocolEventContext::new(
        causation_id,
        CorrelationId::new(correlation_id).expect("valid correlation ID"),
        node_id,
    )
    .expect("valid event context")
}

fn rehash_stored_event(snapshot: &mut ExecutionState, index: usize) {
    let (old_event_id, event_id, payload_hash) = {
        let stored = snapshot
            .event_log
            .get_mut(index)
            .expect("stored event at test index");
        let old_event_id = stored.envelope.event_id.clone();
        stored.envelope.semantic_identity = stored
            .envelope
            .expected_semantic_identity()
            .expect("tampered context remains intrinsically serializable");
        stored.envelope.event_id = stored
            .envelope
            .expected_event_id()
            .expect("rederive tampered event ID");
        let payload_hash = stored
            .envelope
            .canonical_hash()
            .expect("rehash tampered envelope");
        stored.payload_hash = payload_hash.clone();
        (old_event_id, stored.envelope.event_id.clone(), payload_hash)
    };
    snapshot.event_payload_hashes.remove(&old_event_id);
    snapshot.event_payload_hashes.insert(event_id, payload_hash);
}

fn profile_payload() -> DomainEvent {
    ProfileEvent::RepositoryProfileRecorded {
        profile: strict_profile(),
    }
    .into()
}

#[test]
fn strict_entry_points_reject_compatibility_scaffolds() {
    let state = bootstrap(2, 10);
    let discovery_id = NodeId::new("protocol-v1:discovery");
    let event = ProtocolEventEnvelope::new_legacy_test_compatible(
        &state,
        "phase8:compatibility-event",
        10,
        GraphEvent::NodeStarted {
            node_id: discovery_id,
            attempt: 1,
        },
    )
    .expect("well-formed compatibility event");

    assert_eq!(
        decide_strict_v1(&state).expect_err("compatibility mode cannot drive strict decisions"),
        ProtocolViolation::Invariant {
            code: "strict_v1_bootstrap_required",
            detail: "production execution requires strict Protocol v1 bootstrap authority".into(),
        }
    );
    assert_eq!(
        reduce_strict_v1(&state, event)
            .expect_err("compatibility mode cannot drive strict reductions")
            .code(),
        "strict_v1_bootstrap_required"
    );
}

#[test]
fn strict_bootstrap_roundtrips_and_restores_from_its_trusted_root() {
    let trusted = strict_bootstrap();
    assert_eq!(trusted.protocol_mode, ExecutionProtocolModeV1::StrictV1);
    assert!(decide_strict_v1(&trusted).is_ok());
    assert_eq!(
        ProtocolEventEnvelope::new_legacy_test_compatible(
            &trusted,
            "phase8:strict-legacy-context",
            1,
            profile_payload(),
        )
        .expect_err("strict events cannot infer causal context")
        .code(),
        "strict_v1_explicit_event_context_required"
    );

    let encoded = serde_json::to_value(&trusted).expect("serialize strict bootstrap");
    let decoded: ExecutionState =
        serde_json::from_value(encoded).expect("deserialize strict bootstrap");
    assert_eq!(decoded.protocol_mode, ExecutionProtocolModeV1::StrictV1);
    assert_eq!(decoded.validation_policy, trusted.validation_policy);
    assert_eq!(decoded.finalization_policy, trusted.finalization_policy);

    let restored = InMemoryEventStore::restore(trusted.clone(), decoded)
        .expect("strict snapshot restores only from its trusted bootstrap")
        .into_state();
    assert_eq!(restored, trusted);
    assert!(decide_strict_v1(&restored).is_ok());
}

#[test]
fn nonempty_strict_stream_restores_and_rejects_context_hash_and_v1_envelope_tampering() {
    let trusted = strict_bootstrap();
    let correlation =
        CorrelationId::for_execution(&trusted.execution_id, trusted.execution_attempt);
    let mut store = InMemoryEventStore::new(trusted.clone()).expect("trusted strict store");
    let profile_event = ProtocolEventEnvelope::new_with_context(
        store.state(),
        "phase8:strict-profile",
        10,
        ProtocolEventContext::new(None, correlation.clone(), None).expect("root context"),
        profile_payload(),
    )
    .expect("strict profile event");
    store.append(profile_event.clone()).expect("append profile");

    let after_profile = store.state().clone();
    let ProtocolDecision::Emit {
        event: goal_payload,
    } = decide_strict_v1(&after_profile).expect("strict profiling decision")
    else {
        panic!("strict reducer must emit its trusted discovery goal after profiling");
    };
    assert!(matches!(
        &goal_payload,
        DomainEvent::Discovery(DiscoveryEvent::GoalRecorded { goal })
            if goal == &strict_goal()
    ));
    let goal_event = ProtocolEventEnvelope::new_with_context(
        &after_profile,
        "phase8:strict-goal",
        20,
        ProtocolEventContext::new(Some(profile_event.event_id.clone()), correlation, None)
            .expect("goal context"),
        goal_payload,
    )
    .expect("strict goal envelope");

    let mut v1_event = goal_event.clone();
    v1_event.event_schema_version = 1;
    assert!(matches!(
        reduce_strict_v1(&after_profile, v1_event),
        Err(ProtocolViolation::EnvelopeMismatch {
            field: "event_schema_version"
        })
    ));

    store.append(goal_event).expect("append trusted goal");
    let snapshot = store.state().clone();

    let profile = snapshot
        .repository_profile
        .as_ref()
        .expect("profile is materialized");
    let forged_proof = ProofRecord {
        id: ProofId::new("proof:caller-shaped-profile"),
        kind: ProofKind::RepositoryProfile,
        repository_revision: snapshot.repository_revision.clone(),
        node_ids: Vec::new(),
        related_proof_ids: Vec::new(),
        related_evidence_ids: Vec::new(),
        detail_hash: repository_profile_proof_hash(&profile.profile_id),
    };
    let forged_proof_event = ProtocolEventEnvelope::new_with_context(
        &snapshot,
        "phase8:caller-shaped-profile-proof",
        30,
        ProtocolEventContext::new(
            snapshot
                .event_log
                .last()
                .map(|stored| stored.envelope.event_id.clone()),
            snapshot.event_log[0].envelope.correlation_id.clone(),
            None,
        )
        .expect("proof context"),
        EvidenceEvent::ProofRecorded {
            proof: forged_proof,
        },
    )
    .expect("caller-shaped proof envelope");
    let mut forged_state = snapshot.clone();
    assert_eq!(
        forged_state
            .append_event(forged_proof_event)
            .expect_err("strict profile proof identity is reducer-owned")
            .code(),
        "strict_v1_repository_profile_proof_mismatch"
    );
    assert_eq!(forged_state, snapshot);

    let decoded: ExecutionState = serde_json::from_slice(
        &serde_json::to_vec(&snapshot).expect("serialize nonempty strict snapshot"),
    )
    .expect("deserialize nonempty strict snapshot");
    let restored = InMemoryEventStore::restore(trusted.clone(), decoded)
        .expect("restore strict nonempty stream")
        .into_state();
    assert_eq!(restored, snapshot);

    let mut changed_correlation = snapshot.clone();
    changed_correlation.event_log[1].envelope.correlation_id =
        CorrelationId::new("correlation:tampered").expect("valid alternate correlation");
    rehash_stored_event(&mut changed_correlation, 1);
    assert!(InMemoryEventStore::restore(trusted.clone(), changed_correlation).is_err());

    let mut unknown_cause = snapshot.clone();
    unknown_cause.event_log[1].envelope.causation_id = Some(EventId::new("event:unknown-cause"));
    rehash_stored_event(&mut unknown_cause, 1);
    assert!(InMemoryEventStore::restore(trusted.clone(), unknown_cause).is_err());

    let mut wrong_owner = snapshot.clone();
    wrong_owner.event_log[1].envelope.node_id = Some(NodeId::new("protocol-v1:discovery"));
    rehash_stored_event(&mut wrong_owner, 1);
    assert!(InMemoryEventStore::restore(trusted.clone(), wrong_owner).is_err());

    let mut broken_hash = snapshot;
    broken_hash.event_log[1].payload_hash = "0".repeat(64);
    assert!(InMemoryEventStore::restore(trusted, broken_hash).is_err());
}

#[test]
fn strict_wire_rejects_pre_freeze_snapshots_and_invalid_pristine_roots() {
    let trusted = strict_bootstrap();
    let mut old_snapshot = serde_json::to_value(&trusted).expect("serialize strict bootstrap");
    old_snapshot
        .as_object_mut()
        .expect("state object")
        .remove("protocol_mode");
    assert!(
        serde_json::from_value::<ExecutionState>(old_snapshot).is_err(),
        "pre-freeze snapshots require an explicit migration rather than a serde default"
    );

    let profile = strict_profile();
    let invalid_attempt = ExecutionState::bootstrap_strict_v1(
        ExecutionId::new(EXECUTION_ID),
        0,
        RepositoryRevisionId::new(REPOSITORY_REVISION),
        mission_budget(10),
        model_budget(2),
        model_budget(2),
        plan_graph_budget(),
        strict_goal(),
        validation_policy(&profile),
        finalization_policy(),
    )
    .expect_err("attempt zero cannot become a strict aggregate");
    assert_eq!(invalid_attempt.code(), "invalid_identity");

    let base_mismatch = ExecutionState::bootstrap_strict_v1(
        ExecutionId::new(EXECUTION_ID),
        1,
        RepositoryRevisionId::new(REPOSITORY_REVISION),
        mission_budget(10),
        model_budget(2),
        model_budget(2),
        plan_graph_budget(),
        strict_goal(),
        validation_policy(&profile),
        finalization_policy_for(RepositoryRevisionId::new(
            "repository-revision:different-base",
        )),
    )
    .expect_err("signed publication base must equal the strict initial revision");
    assert_eq!(
        base_mismatch.code(),
        "strict_v1_publication_base_revision_mismatch"
    );
}

#[test]
fn strict_bootstrap_rejects_missing_and_tampered_policy_authority() {
    let trusted = strict_bootstrap();

    let mut missing_goal = trusted.clone();
    missing_goal.requested_discovery_goal = None;
    assert_eq!(
        decide_strict_v1(&missing_goal)
            .expect_err("missing mission authority must fail")
            .code(),
        "strict_v1_discovery_goal_missing"
    );
    assert!(InMemoryEventStore::restore(trusted.clone(), missing_goal).is_err());

    let mut missing_validation = trusted.clone();
    missing_validation.validation_policy = None;
    assert_eq!(
        decide_strict_v1(&missing_validation)
            .expect_err("missing validation authority must fail")
            .code(),
        "strict_v1_validation_policy_missing"
    );
    assert!(InMemoryEventStore::restore(trusted.clone(), missing_validation).is_err());

    let mut missing_finalization = trusted.clone();
    missing_finalization.finalization_policy = None;
    assert_eq!(
        decide_strict_v1(&missing_finalization)
            .expect_err("missing finalization authority must fail")
            .code(),
        "strict_v1_finalization_policy_missing"
    );
    assert!(InMemoryEventStore::restore(trusted.clone(), missing_finalization).is_err());

    let mut tampered_validation = trusted.clone();
    tampered_validation
        .validation_policy
        .as_mut()
        .expect("validation policy")
        .max_repair_targets_per_failure += 1;
    assert_eq!(
        decide_strict_v1(&tampered_validation)
            .expect_err("validation policy identity must bind its fields")
            .code(),
        "validation_policy_invalid"
    );

    let mut tampered_finalization = trusted;
    tampered_finalization
        .finalization_policy
        .as_mut()
        .expect("finalization policy")
        .max_changed_paths += 1;
    assert_eq!(
        decide_strict_v1(&tampered_finalization)
            .expect_err("finalization policy identity must bind its fields")
            .code(),
        "finalization_policy_invalid"
    );
}

#[test]
fn strict_goal_event_must_equal_trusted_bootstrap_authority_atomically() {
    let trusted = strict_bootstrap();
    let mut state = trusted.clone();
    let profile = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:goal-authority-profile",
        10,
        context(None, "correlation:goal-authority", None),
        profile_payload(),
    )
    .expect("profile envelope");
    state.append_event(profile.clone()).expect("profile append");
    let baseline = state.clone();
    let forged_goal = DiscoveryGoal::new(
        stable_sha256(&["execution-protocol-v1:forged-goal"]),
        BTreeSet::from([
            DiscoveryCriterionId::new("criterion:forged").expect("valid forged criterion")
        ]),
        ["different requested work".to_owned()],
    )
    .expect("intrinsically valid forged goal");
    let event = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:forged-goal",
        20,
        context(Some(profile.event_id), "correlation:goal-authority", None),
        DiscoveryEvent::GoalRecorded { goal: forged_goal },
    )
    .expect("forged goal has a well-formed envelope");

    assert_eq!(
        state
            .append_event(event)
            .expect_err("event stream cannot choose a different strict mission")
            .code(),
        "strict_v1_discovery_goal_mismatch"
    );
    assert_eq!(state, baseline);
}

#[test]
fn strict_profile_recording_revalidates_signed_repository_authority_atomically() {
    let mut state = strict_bootstrap();
    let baseline = state.clone();
    let inventory = RepositoryInventory::new(
        RepositoryRevisionId::new(REPOSITORY_REVISION),
        vec![
            RepositoryFileObservation::from_bytes(
                "Cargo.toml",
                b"[package]\nname = \"different-profile\"\nversion = \"0.1.0\"\n",
            )
            .expect("bounded manifest"),
            RepositoryFileObservation::from_bytes("src/main.rs", b"fn main() {}\n")
                .expect("bounded source"),
        ],
    )
    .expect("canonical mismatching inventory");
    let mismatching_profile =
        build_repository_profile(&inventory).expect("valid mismatching profile");
    assert_ne!(
        mismatching_profile.profile_id,
        strict_profile().profile_id,
        "the negative requires a different but intrinsically valid profile"
    );
    let event = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:mismatching-profile",
        10,
        context(None, "correlation:mismatching-profile", None),
        ProfileEvent::RepositoryProfileRecorded {
            profile: mismatching_profile,
        },
    )
    .expect("the envelope is well formed before aggregate policy validation");

    assert_eq!(
        state
            .append_event(event)
            .expect_err("profile membership must match signed bootstrap authority")
            .code(),
        "validation_policy_invalid"
    );
    assert_eq!(state, baseline, "rejected profile append must be atomic");
}

#[test]
fn event_context_enforces_prior_causation_stable_correlation_and_exact_node_owner() {
    let mut state = bootstrap(2, 10);
    let correlation = "correlation:phase8-context";
    let first = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:profile",
        10,
        context(None, correlation, None),
        profile_payload(),
    )
    .expect("first event establishes correlation");
    state
        .append_event(first.clone())
        .expect("profile event is committed");

    let unknown_cause = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:unknown-cause",
        20,
        context(Some(EventId::new("event:not-committed")), correlation, None),
        EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                id: ProofId::new("proof:phase8-context"),
                kind: ProofKind::RepositoryProfile,
                repository_revision: state.repository_revision.clone(),
                node_ids: Vec::new(),
                related_proof_ids: Vec::new(),
                related_evidence_ids: Vec::new(),
                detail_hash: stable_sha256(&["execution-protocol-v1:phase8-context-proof"]),
            },
        },
    )
    .expect_err("causation must reference an already committed event");
    assert!(matches!(
        unknown_cause,
        ProtocolViolation::EnvelopeMismatch {
            field: "causation_id"
        }
    ));

    let changed_correlation = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:changed-correlation",
        20,
        context(
            Some(first.event_id.clone()),
            "correlation:different-attempt",
            None,
        ),
        EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                id: ProofId::new("proof:phase8-context"),
                kind: ProofKind::RepositoryProfile,
                repository_revision: state.repository_revision.clone(),
                node_ids: Vec::new(),
                related_proof_ids: Vec::new(),
                related_evidence_ids: Vec::new(),
                detail_hash: stable_sha256(&["execution-protocol-v1:phase8-context-proof"]),
            },
        },
    )
    .expect_err("correlation is immutable for an execution attempt");
    assert!(matches!(
        changed_correlation,
        ProtocolViolation::EnvelopeMismatch {
            field: "correlation_id"
        }
    ));

    let discovery_id = NodeId::new("protocol-v1:discovery");
    let node_payload = GraphEvent::NodeStarted {
        node_id: discovery_id.clone(),
        attempt: 1,
    };
    ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:owned-node",
        20,
        context(
            Some(first.event_id.clone()),
            correlation,
            Some(discovery_id),
        ),
        node_payload.clone(),
    )
    .expect("exact payload node owner is accepted");
    let wrong_owner = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:wrong-owner",
        20,
        context(
            Some(first.event_id),
            correlation,
            Some(NodeId::new("protocol-v1:planning")),
        ),
        node_payload,
    )
    .expect_err("a different known node cannot claim the event");
    assert!(matches!(
        wrong_owner,
        ProtocolViolation::EnvelopeMismatch { field: "node_id" }
    ));
}

#[test]
fn every_event_context_field_participates_in_semantic_identity() {
    let mut state = bootstrap(2, 10);
    let profile_a = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:identity-profile",
        10,
        context(None, "correlation:identity-a", None),
        profile_payload(),
    )
    .expect("profile envelope A");
    let profile_b = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:identity-profile",
        10,
        context(None, "correlation:identity-b", None),
        profile_payload(),
    )
    .expect("profile envelope B");
    assert_ne!(profile_a.semantic_identity, profile_b.semantic_identity);
    assert_ne!(profile_a.event_id, profile_b.event_id);

    state
        .append_event(profile_a.clone())
        .expect("commit identity root");
    let proof = ProofRecord {
        id: ProofId::new("proof:phase8-identity"),
        kind: ProofKind::RepositoryProfile,
        repository_revision: state.repository_revision.clone(),
        node_ids: Vec::new(),
        related_proof_ids: Vec::new(),
        related_evidence_ids: Vec::new(),
        detail_hash: stable_sha256(&["execution-protocol-v1:phase8-identity-proof"]),
    };
    let uncaused = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:identity-proof",
        20,
        context(None, "correlation:identity-a", None),
        EvidenceEvent::ProofRecorded {
            proof: proof.clone(),
        },
    )
    .expect_err("only the first event may omit causation");
    assert!(matches!(
        uncaused,
        ProtocolViolation::EnvelopeMismatch {
            field: "causation_id"
        }
    ));
    let caused = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:identity-proof",
        20,
        context(Some(profile_a.event_id), "correlation:identity-a", None),
        EvidenceEvent::ProofRecorded { proof },
    )
    .expect("known causation");
    let caused_identity = caused.semantic_identity.clone();
    let caused_event_id = caused.event_id.clone();
    let mut context_tampered = caused.clone();
    context_tampered.causation_id = None;
    assert_ne!(
        caused_identity,
        context_tampered.expected_semantic_identity().unwrap()
    );
    assert_ne!(
        caused_event_id,
        context_tampered.expected_event_id().unwrap()
    );

    let mut node_owned = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:identity-node",
        20,
        context(
            Some(caused.causation_id.expect("known cause")),
            "correlation:identity-a",
            Some(NodeId::new("protocol-v1:discovery")),
        ),
        GraphEvent::NodeStarted {
            node_id: NodeId::new("protocol-v1:discovery"),
            attempt: 1,
        },
    )
    .expect("owned graph event");
    let owned_identity = node_owned.expected_semantic_identity().unwrap();
    node_owned.node_id = Some(NodeId::new("protocol-v1:planning"));
    assert_ne!(
        owned_identity,
        node_owned.expected_semantic_identity().unwrap()
    );
}

#[test]
fn event_envelope_serde_rejects_missing_or_unknown_context_without_state_change() {
    let state = bootstrap(2, 10);
    let baseline = state.clone();
    let event = ProtocolEventEnvelope::new_with_context(
        &state,
        "phase8:serde-context",
        10,
        context(None, "correlation:serde-context", None),
        profile_payload(),
    )
    .expect("valid context envelope");
    let canonical = serde_json::to_value(event).expect("serialize envelope");

    for missing in [
        "causation_id",
        "correlation_id",
        "node_id",
        "effect_observation",
    ] {
        let mut value = canonical.clone();
        value
            .as_object_mut()
            .expect("envelope object")
            .remove(missing);
        assert!(
            serde_json::from_value::<ProtocolEventEnvelope>(value).is_err(),
            "missing {missing} must be rejected"
        );
        assert_eq!(state, baseline);
    }

    let mut unknown = canonical;
    unknown.as_object_mut().expect("envelope object").insert(
        "unexpected_context_authority".into(),
        serde_json::json!(true),
    );
    assert!(serde_json::from_value::<ProtocolEventEnvelope>(unknown).is_err());
    assert_eq!(state, baseline);
}
