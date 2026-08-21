//! Phase 5 target-local mutation protocol regressions.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::phase4_implementation_context::{
    FixtureOperation, ImplementationSeed, fixture_bytes, implementation_seed,
    implementation_seed_with_validation_commands,
    implementation_seed_with_validation_commands_and_graph_budget, materialized_context,
    target_context_request,
};
use super::*;

const MUTATION_SECRET_SENTINEL: &str = "phase5-raw-candidate-secret-47f4f251";

struct MutationSeed {
    phase4: ImplementationSeed,
    node: ExecutionNode,
    target: PlannedTargetV1,
    context: TargetContextManifest,
    feasibility: MutationFeasibilitySet,
    policy: MutationAttemptPolicy,
    prepared: PreparedMutationAction,
}

pub(super) struct CompletedImplementationBarrierSeed {
    pub(super) phase4: ImplementationSeed,
    pub(super) barrier_proof_id: ProofId,
    pub(super) validation_node_id: NodeId,
}

fn mutation_seed(operation: FixtureOperation, input_token_ceiling: u32) -> MutationSeed {
    mutation_seed_from_phase4(implementation_seed(operation, input_token_ceiling))
}

fn mutation_seed_with_budget(
    operation: FixtureOperation,
    input_token_ceiling: u32,
    configure: impl FnOnce(&mut NodeBudgetContract),
) -> MutationSeed {
    mutation_seed_from_phase4(implementation_seed_with_budget(
        operation,
        input_token_ceiling,
        configure,
    ))
}

fn implementation_seed_with_budget(
    operation: FixtureOperation,
    input_token_ceiling: u32,
    configure: impl FnOnce(&mut NodeBudgetContract),
) -> ImplementationSeed {
    let mut phase4 = implementation_seed(operation, input_token_ceiling);
    let mut implementation_budget = phase4
        .state
        .node(&phase4.target_node_id)
        .expect("materialized implementation budget")
        .budget
        .clone();
    configure(&mut implementation_budget);

    let mut trusted_initial = phase4.trusted_initial.clone();
    trusted_initial.plan_graph_budget.implementation = implementation_budget;
    trusted_initial
        .plan_graph_budget
        .validate()
        .expect("custom Phase 5 implementation budget");
    let mut rebuilt = trusted_initial.clone();
    for stored in &phase4.state.event_log {
        let payload = if matches!(
            stored.envelope.payload,
            DomainEvent::Graph(GraphEvent::NodesAdded { .. })
        ) {
            let ProtocolDecision::Emit { event } =
                decide(&rebuilt).expect("custom-budget plan graph decision")
            else {
                panic!("custom budget must still materialize the accepted plan");
            };
            assert!(matches!(
                event,
                DomainEvent::Graph(GraphEvent::NodesAdded { .. })
            ));
            event
        } else {
            stored.envelope.payload.clone()
        };
        append(&mut rebuilt, &stored.envelope.semantic_key, payload);
    }
    phase4.trusted_initial = trusted_initial;
    phase4.state = rebuilt;
    phase4
}

fn implementation_seed_with_two_ready_targets(
    input_token_ceiling: u32,
    configure: impl FnOnce(&mut NodeBudgetContract),
) -> (ImplementationSeed, NodeId) {
    let original = implementation_seed_with_budget(
        FixtureOperation::ModifySmall,
        input_token_ceiling,
        configure,
    );
    let mut primary_target = original.accepted_plan.targets[0].clone();
    primary_target.target_id = TargetId::new("a");
    primary_target.change_id = ChangeId::new("a");
    primary_target.path = ProfilePath::new("x/a").expect("valid generated fixture path");
    primary_target.operation = TargetOperation::CreateFile {
        specification: CreationSpecification::new(CreatedFileKind::Source, "a")
            .expect("bounded creation specification"),
    };
    primary_target.rationale = "a".into();
    let secondary_target = PlannedTargetV1 {
        target_id: TargetId::new("b"),
        change_id: ChangeId::new("b"),
        path: ProfilePath::new("x/b").expect("valid generated fixture path"),
        operation: TargetOperation::CreateFile {
            specification: CreationSpecification::new(CreatedFileKind::Source, "b")
                .expect("bounded creation specification"),
        },
        role: TargetRole::Source,
        rationale: "b".into(),
        acceptance_criteria: primary_target.acceptance_criteria.clone(),
        required_evidence: primary_target.required_evidence.clone(),
        expected_validation: primary_target.expected_validation.clone(),
        dependencies: BTreeSet::new(),
        estimated_change: ChangeEstimate {
            size: ChangeSize::Small,
            risk: ChangeRisk::Low,
            estimated_changed_lines: 8,
        },
    };

    let trusted_initial = original.trusted_initial.clone();
    let mut rebuilt = trusted_initial.clone();
    let mut candidate_replaced = false;
    for stored in &original.state.event_log {
        let payload = match &stored.envelope.payload {
            DomainEvent::Planning(PlanningEvent::CandidateRecorded {
                action_id,
                call_id,
                candidate,
            }) => {
                candidate_replaced = true;
                let replacement_candidate = PlanCandidate::new(
                    candidate.revision_index,
                    candidate.repository_revision.clone(),
                    candidate.discovery_impact_map_id.clone(),
                    PlanDecisionCandidate::Changes {
                        targets: vec![primary_target.clone(), secondary_target.clone()],
                    },
                )
                .expect("bounded two-target planning candidate");
                PlanningEvent::CandidateRecorded {
                    action_id: action_id.clone(),
                    call_id: call_id.clone(),
                    candidate: replacement_candidate,
                }
                .into()
            }
            _ if candidate_replaced => {
                let ProtocolDecision::Emit { event } =
                    decide(&rebuilt).expect("authoritative rebuilt two-target decision")
                else {
                    panic!("two-target fixture must remain event-driven after plan acceptance");
                };
                event
            }
            _ => stored.envelope.payload.clone(),
        };
        append(&mut rebuilt, &stored.envelope.semantic_key, payload);
    }
    assert!(candidate_replaced, "planning candidate must be replaced");

    let accepted_plan = rebuilt
        .planning
        .as_ref()
        .and_then(|planning| planning.accepted_plan.clone())
        .expect("accepted two-target plan");
    let materialized = materialize_accepted_plan(&accepted_plan, &rebuilt.plan_graph_budget)
        .expect("materialized two-target plan");
    assert_eq!(materialized.target_nodes.len(), 2);
    let primary_node_id = materialized
        .target_nodes
        .values()
        .find(|node_id| {
            matches!(
                rebuilt.node(node_id).map(|node| &node.state),
                Some(NodeState::Active { attempt: 1 })
            )
        })
        .expect("one active implementation target")
        .clone();
    let secondary_node_id = materialized
        .target_nodes
        .values()
        .find(|node_id| {
            matches!(
                rebuilt.node(node_id).map(|node| &node.state),
                Some(NodeState::Ready)
            )
        })
        .expect("one independently ready implementation target")
        .clone();
    assert!(matches!(
        rebuilt.node(&primary_node_id).map(|node| &node.state),
        Some(NodeState::Active { attempt: 1 })
    ));
    assert!(matches!(
        rebuilt.node(&secondary_node_id).map(|node| &node.state),
        Some(NodeState::Ready)
    ));

    (
        ImplementationSeed {
            trusted_initial,
            state: rebuilt,
            target_node_id: primary_node_id,
            accepted_plan,
            artifacts: original.artifacts,
        },
        secondary_node_id,
    )
}

fn mutation_seed_from_phase4(mut phase4: ImplementationSeed) -> MutationSeed {
    let request = target_context_request(&phase4.state);
    let materialized = materialized_context(&phase4, &request);
    let prepared_context =
        prepare_target_context(&request, &materialized).expect("bounded Phase 4 target context");
    append(
        &mut phase4.state,
        "phase5:target-context-prepared",
        ImplementationEvent::TargetContextPrepared {
            prepared: Box::new(prepared_context.clone()),
        },
    );
    let node = phase4
        .state
        .node(&phase4.target_node_id)
        .expect("active implementation node")
        .clone();
    let target = phase4
        .accepted_plan
        .targets
        .first()
        .expect("one accepted target")
        .clone();
    let context = prepared_context.manifest;
    let feasibility = evaluate_mutation_feasibility(&node, &target, &context)
        .expect("deterministic target-local feasibility");
    let policy = select_initial_mutation_policy(
        &phase4.state.execution_id,
        phase4.state.execution_attempt,
        &node,
        &target,
        &context,
        &feasibility,
    )
    .expect("initial mutation policy");
    let prepared = build_prepared_mutation_action(
        &node,
        &target,
        &context,
        &feasibility,
        policy.clone(),
        100,
        100,
    )
    .expect("exact mutation provider action");
    MutationSeed {
        phase4,
        node,
        target,
        context,
        feasibility,
        policy,
        prepared,
    }
}

fn provider_json(prepared: &PreparedMutationAction) -> Value {
    serde_json::from_slice(
        &prepared
            .provider_request
            .canonical_bytes()
            .expect("canonical provider bytes"),
    )
    .expect("canonical provider JSON")
}

fn serialized_tool_names(provider: &Value) -> Vec<&str> {
    provider["tools"]
        .as_array()
        .expect("provider tools array")
        .iter()
        .map(|tool| {
            tool.pointer("/function/name")
                .and_then(Value::as_str)
                .expect("function tool name")
        })
        .collect()
}

fn expected_target_hash(target: &PlannedTargetV1) -> String {
    target
        .operation
        .expected_content_hash()
        .expect("existing target operation hash")
        .to_owned()
}

pub(super) fn durable_artifact(label: &str, bytes: Vec<u8>) -> DurableMutationArtifact {
    let content = std::str::from_utf8(&bytes).expect("UTF-8 mutation test artifact");
    let receipt = stable_sha256(&["phase5:durable-artifact-receipt", label, content]);
    DurableMutationArtifact::new(bytes, receipt).expect("durable mutation test artifact")
}

pub(super) fn phase6_implementation_target_bytes(path: &ProfilePath) -> Vec<u8> {
    let mut bytes = fixture_bytes(path);
    bytes.extend_from_slice(b"\n// phase5 verified mutation\n");
    bytes
}

fn expected_after_patch_bytes(seed: &MutationSeed) -> Vec<u8> {
    phase6_implementation_target_bytes(&seed.target.path)
}

fn patch_invocation(
    seed: &MutationSeed,
    bytes: Vec<u8>,
    completeness: ProviderOutputCompleteness,
) -> MaterializedMutationInvocation {
    MaterializedMutationInvocation {
        action_id: seed.prepared.provider_request.action_id.clone(),
        call_id: seed.prepared.provider_request.call_id.clone(),
        tool_call_count: 1,
        completeness,
        arguments: MaterializedMutationArguments::ApplyPatch {
            path: seed.target.path.clone(),
            expected_content_hash: expected_target_hash(&seed.target),
            patch: durable_artifact("patch", bytes),
            expected_after_content: durable_artifact(
                "patch-expected-after",
                expected_after_patch_bytes(seed),
            ),
        },
    }
}

fn canonical_invocation(seed: &MutationSeed) -> MaterializedMutationInvocation {
    let arguments = match &seed.target.operation {
        TargetOperation::ModifyExisting { .. } => MaterializedMutationArguments::ApplyPatch {
            path: seed.target.path.clone(),
            expected_content_hash: expected_target_hash(&seed.target),
            patch: durable_artifact(
                "canonical-patch",
                b"--- a/target\n+++ b/target\n@@ -1 +1 @@\n-old\n+new\n".to_vec(),
            ),
            expected_after_content: durable_artifact(
                "canonical-patch-expected-after",
                expected_after_patch_bytes(seed),
            ),
        },
        TargetOperation::CreateFile { .. } => MaterializedMutationArguments::CreateFile {
            path: seed.target.path.clone(),
            content: durable_artifact(
                "canonical-create-content",
                b"pub fn created() -> bool { true }\n".to_vec(),
            ),
        },
        TargetOperation::DeleteFile { .. } => MaterializedMutationArguments::DeleteFile {
            path: seed.target.path.clone(),
            expected_content_hash: expected_target_hash(&seed.target),
        },
        TargetOperation::MoveFile { destination, .. } => MaterializedMutationArguments::MoveFile {
            source_path: seed.target.path.clone(),
            destination_path: destination.clone(),
            expected_content_hash: expected_target_hash(&seed.target),
        },
    };
    MaterializedMutationInvocation {
        action_id: seed.prepared.provider_request.action_id.clone(),
        call_id: seed.prepared.provider_request.call_id.clone(),
        tool_call_count: 1,
        completeness: ProviderOutputCompleteness::Complete,
        arguments,
    }
}

fn accepted_candidate(seed: &MutationSeed) -> MutationCandidateRecord {
    let MutationCandidateObservation::Accepted { candidate } =
        record_mutation_candidate(&seed.prepared, &seed.target, &canonical_invocation(seed))
            .expect("operation-owned mutation candidate")
    else {
        panic!("canonical operation invocation must be accepted");
    };
    candidate
}

pub(super) fn file_state(content_hash: String, byte_len: u64) -> MutationPathState {
    MutationPathState::File {
        content_hash,
        byte_len,
        encoding: TextEncoding::Utf8,
    }
}

fn materialized_verification(
    seed: &MutationSeed,
    candidate: &MutationCandidateRecord,
    request: &MutationVerifyRequest,
) -> MaterializedMutationVerification {
    let transitions = match &candidate.operation {
        MutationCandidateOperation::ApplyPatch {
            path,
            expected_content_hash,
            expected_after_content,
            ..
        } => BTreeMap::from([(
            path.clone(),
            MutationPathTransition {
                before: file_state(
                    expected_content_hash.clone(),
                    u64::try_from(fixture_bytes(path).len()).unwrap(),
                ),
                after: file_state(
                    expected_after_content.content_hash.clone(),
                    expected_after_content.byte_len,
                ),
            },
        )]),
        MutationCandidateOperation::ReplaceFile {
            path,
            expected_content_hash,
            content,
        } => BTreeMap::from([(
            path.clone(),
            MutationPathTransition {
                before: file_state(
                    expected_content_hash.clone(),
                    u64::try_from(fixture_bytes(path).len()).unwrap(),
                ),
                after: file_state(content.content_hash.clone(), content.byte_len),
            },
        )]),
        MutationCandidateOperation::CreateFile { path, content } => BTreeMap::from([(
            path.clone(),
            MutationPathTransition {
                before: MutationPathState::Absent,
                after: file_state(content.content_hash.clone(), content.byte_len),
            },
        )]),
        MutationCandidateOperation::DeleteFile {
            path,
            expected_content_hash,
        } => BTreeMap::from([(
            path.clone(),
            MutationPathTransition {
                before: file_state(
                    expected_content_hash.clone(),
                    u64::try_from(fixture_bytes(path).len()).unwrap(),
                ),
                after: MutationPathState::Absent,
            },
        )]),
        MutationCandidateOperation::MoveFile {
            source_path,
            destination_path,
            expected_content_hash,
        } => BTreeMap::from([
            (
                source_path.clone(),
                MutationPathTransition {
                    before: file_state(
                        expected_content_hash.clone(),
                        u64::try_from(fixture_bytes(source_path).len()).unwrap(),
                    ),
                    after: MutationPathState::Absent,
                },
            ),
            (
                destination_path.clone(),
                MutationPathTransition {
                    before: MutationPathState::Absent,
                    after: file_state(
                        expected_content_hash.clone(),
                        u64::try_from(fixture_bytes(source_path).len()).unwrap(),
                    ),
                },
            ),
        ]),
    };
    MaterializedMutationVerification {
        request_id: request.request_id.clone(),
        repository_revision: seed.context.repository_revision.clone(),
        repository_fingerprint_before: seed.context.repository_fingerprint.clone(),
        repository_fingerprint_after: stable_sha256(&[
            "phase5:repository-fingerprint-after",
            candidate.candidate_id.as_str(),
        ]),
        changed_paths: transitions.keys().cloned().collect(),
        path_transitions: transitions,
    }
}

fn rebind_verification_identity(evidence: &mut MutationVerificationEvidence) {
    let canonical = serde_json::to_string(&(
        evidence.schema_version,
        &evidence.verification_request_id,
        &evidence.application_id,
        &evidence.node_id,
        &evidence.target_id,
        &evidence.context_manifest_id,
        &evidence.attempt_id,
        &evidence.candidate_id,
        &evidence.repository_revision_before,
        &evidence.repository_revision_after,
        &evidence.repository_fingerprint_before,
        &evidence.repository_fingerprint_after,
        &evidence.changed_paths,
        &evidence.path_transitions,
    ))
    .expect("verification identity preimage serializes");
    evidence.detail_hash = stable_sha256(&[
        "execution-protocol-v1:mutation-verification-detail",
        &canonical,
    ]);
    evidence.evidence_id = EvidenceId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:mutation-verification-evidence",
            evidence.verification_request_id.as_str(),
            evidence.candidate_id.as_str(),
            &evidence.detail_hash,
        ])
    ));
}

fn append_next_emitted(state: &mut ExecutionState, semantic_key: &str) -> DomainEvent {
    let ProtocolDecision::Emit { event } = decide(state).expect("authoritative mutation decision")
    else {
        panic!("expected emitted mutation event for {semantic_key}");
    };
    append(state, semantic_key, event.clone());
    event
}

fn prepare_aggregate_mutation(seed: &mut MutationSeed, label: &str) -> PreparedMutationAction {
    assert_eq!(
        append_next_emitted(
            &mut seed.phase4.state,
            &format!("phase5:{label}:feasibility"),
        ),
        MutationEvent::FeasibilityEvaluated {
            feasibility: seed.feasibility.clone(),
        }
        .into()
    );
    assert_eq!(
        append_next_emitted(&mut seed.phase4.state, &format!("phase5:{label}:policy"),),
        MutationEvent::AttemptPolicySelected {
            policy: seed.policy.clone(),
        }
        .into()
    );
    let DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared }) = append_next_emitted(
        &mut seed.phase4.state,
        &format!("phase5:{label}:action-prepared"),
    ) else {
        panic!("mutation action must be prepared before budget admission");
    };
    assert_eq!(prepared.provider_request, seed.prepared.provider_request);
    *prepared
}

pub(super) fn dispatch_and_consume_aggregate_mutation(
    state: &mut ExecutionState,
    prepared: &PreparedMutationAction,
    label: &str,
    actual_cost_micros: u64,
    duration_ms: u64,
) {
    assert_eq!(
        append_next_emitted(state, &format!("phase5:{label}:admitted")),
        BudgetEvent::ModelCallAdmitted {
            admission: prepared.admission.clone(),
        }
        .into()
    );
    assert_eq!(
        append_next_emitted(state, &format!("phase5:{label}:reserved")),
        BudgetEvent::ModelCallReserved {
            call_id: prepared.admission.call_id.clone(),
        }
        .into()
    );
    assert_eq!(
        decide(state).expect("reserved mutation call dispatches"),
        ProtocolDecision::Perform {
            effect: EffectRequest::Mutation(MutationEffectRequest::DispatchProvider {
                request: Box::new(prepared.provider_request.clone()),
            }),
        }
    );
    append(
        state,
        &format!("phase5:{label}:dispatch-started"),
        BudgetEvent::ProviderDispatchStarted {
            call_id: prepared.admission.call_id.clone(),
            payload_hash: prepared.provider_request.payload_hash().unwrap(),
        },
    );
    assert_eq!(
        decide(state).expect("provider reconciliation wait"),
        ProtocolDecision::Wait {
            reason: WaitReason::ProviderReconciliation {
                call_id: prepared.admission.call_id.clone(),
            },
        }
    );
    append(
        state,
        &format!("phase5:{label}:reconciled"),
        BudgetEvent::ModelCallReconciled {
            call_id: prepared.admission.call_id.clone(),
            result: ModelCallReconciliation::Consumed {
                actual_cost_micros,
                duration_ms,
            },
        },
    );
    assert_eq!(
        decide(state).expect("mutation observation wait"),
        ProtocolDecision::Wait {
            reason: WaitReason::MutationObservation {
                action_id: prepared.provider_request.action_id.clone(),
            },
        }
    );
}

pub(super) fn release_uncontacted_aggregate_action(
    state: &mut ExecutionState,
    prepared: &PreparedMutationAction,
    label: &str,
) {
    assert_eq!(
        append_next_emitted(state, &format!("phase5:{label}:admitted")),
        BudgetEvent::ModelCallAdmitted {
            admission: prepared.admission.clone(),
        }
        .into()
    );
    assert_eq!(
        append_next_emitted(state, &format!("phase5:{label}:reserved")),
        BudgetEvent::ModelCallReserved {
            call_id: prepared.admission.call_id.clone(),
        }
        .into()
    );
    assert!(matches!(
        decide(state).expect("reserved mutation action is dispatchable"),
        ProtocolDecision::Perform {
            effect: EffectRequest::Mutation(MutationEffectRequest::DispatchProvider { .. })
        }
    ));
    append(
        state,
        &format!("phase5:{label}:released-uncontacted"),
        BudgetEvent::ModelCallReconciled {
            call_id: prepared.admission.call_id.clone(),
            result: ModelCallReconciliation::ReleasedUncontacted,
        },
    );
    assert_eq!(
        append_next_emitted(state, &format!("phase5:{label}:action-released")),
        MutationEvent::ActionReleased {
            action_id: prepared.provider_request.action_id.clone(),
        }
        .into()
    );
}

fn reject_consumed_action_for_model_retry(
    state: &mut ExecutionState,
    prepared: &PreparedMutationAction,
    label: &str,
) -> MutationAttemptPolicy {
    let failure = MutationFailure::new(
        &prepared.policy,
        prepared.policy.permitted_strategies.first().copied(),
        None,
        MutationFailureClass::CandidateSchemaInvalid,
        MutationFailureDetailCode::ExpectedHashMismatch,
        None,
    )
    .expect("typed retryable provider-candidate rejection");
    append(
        state,
        &format!("phase5:{label}:action-rejected"),
        MutationEvent::ActionRejected { failure },
    );
    let DomainEvent::Mutation(MutationEvent::AttemptPolicySelected { policy }) =
        append_next_emitted(state, &format!("phase5:{label}:retry-policy"))
    else {
        panic!("retryable provider rejection must select the next mutation policy");
    };
    policy
}

pub(super) fn completed_implementation_barrier_seed() -> CompletedImplementationBarrierSeed {
    completed_implementation_barrier_seed_from_mutation(mutation_seed(
        FixtureOperation::ModifySmall,
        4_096,
    ))
}

pub(super) fn completed_implementation_barrier_seed_with_validation_commands(
    validation_commands: BTreeSet<ValidationCommandKind>,
) -> CompletedImplementationBarrierSeed {
    completed_implementation_barrier_seed_from_mutation(mutation_seed_from_phase4(
        implementation_seed_with_validation_commands(
            FixtureOperation::ModifySmall,
            4_096,
            validation_commands,
        ),
    ))
}

pub(super) fn completed_implementation_barrier_seed_with_phase7_review_budget()
-> CompletedImplementationBarrierSeed {
    completed_implementation_barrier_seed_from_mutation(mutation_seed_from_phase4(
        implementation_seed_with_validation_commands_and_graph_budget(
            FixtureOperation::ModifySmall,
            4_096,
            BTreeSet::from([ValidationCommandKind::CargoTest]),
            |budget| {
                budget.review.max_input_tokens_per_call = 32 * 1_024;
                budget.completion_evaluation.max_input_tokens_per_call = 32 * 1_024;
            },
        ),
    ))
}

fn completed_implementation_barrier_seed_from_mutation(
    mut seed: MutationSeed,
) -> CompletedImplementationBarrierSeed {
    let prepared = prepare_aggregate_mutation(&mut seed, "phase6-seed");
    dispatch_and_consume_aggregate_mutation(
        &mut seed.phase4.state,
        &prepared,
        "phase6-seed",
        80,
        50,
    );
    let MutationCandidateObservation::Accepted { candidate } =
        record_mutation_candidate(&prepared, &seed.target, &canonical_invocation(&seed))
            .expect("Phase 6 seed mutation candidate")
    else {
        panic!("Phase 6 seed candidate must be accepted");
    };
    append(
        &mut seed.phase4.state,
        "phase5:phase6-seed:candidate",
        MutationEvent::CandidateRecorded {
            candidate: candidate.clone(),
        },
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Mutation(MutationEffectRequest::ApplyMutation { request: apply }),
    } = decide(&seed.phase4.state).expect("Phase 6 seed apply decision")
    else {
        panic!("Phase 6 seed candidate must request application");
    };
    let application =
        MutationApplicationObservation::new(&apply, MutationApplicationStatus::Applied);
    append(
        &mut seed.phase4.state,
        "phase5:phase6-seed:application",
        MutationEvent::ApplicationObserved {
            request: (*apply).clone(),
            observation: application.clone(),
        },
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Mutation(MutationEffectRequest::VerifyMutation { request: verify }),
    } = decide(&seed.phase4.state).expect("Phase 6 seed verification decision")
    else {
        panic!("Phase 6 seed application must request verification");
    };
    let materialized = materialized_verification(&seed, &candidate, &verify);
    let evidence = verify_mutation_application(
        &verify,
        &apply,
        &application,
        &candidate,
        &seed.target,
        &materialized,
    )
    .expect("Phase 6 seed verified mutation");
    append(
        &mut seed.phase4.state,
        "phase5:phase6-seed:verified",
        MutationEvent::MutationVerified { evidence },
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof }) = append_next_emitted(
        &mut seed.phase4.state,
        "phase5:phase6-seed:verification-proof",
    ) else {
        panic!("Phase 6 seed requires mutation verification proof");
    };
    append_next_emitted(&mut seed.phase4.state, "phase5:phase6-seed:node-succeeded");
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof: barrier }) =
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:phase6-seed:implementation-barrier",
        )
    else {
        panic!("Phase 6 seed requires current implementation barrier");
    };
    assert_eq!(barrier.kind, ProofKind::ImplementationBarrier);
    assert_eq!(barrier.related_proof_ids, [proof.id]);
    let materialized = materialize_accepted_plan(
        &seed.phase4.accepted_plan,
        &seed.phase4.state.plan_graph_budget,
    )
    .expect("Phase 6 seed graph materialization");
    let validation_node_id = materialized
        .validation_nodes
        .values()
        .next()
        .expect("Phase 6 seed validation node")
        .clone();
    CompletedImplementationBarrierSeed {
        phase4: seed.phase4,
        barrier_proof_id: barrier.id,
        validation_node_id,
    }
}

#[test]
fn typed_implementation_rejects_forged_completion_proofs_before_mutation() {
    let mut seed = implementation_seed(FixtureOperation::ModifySmall, 4_096);
    assert!(matches!(
        seed.state.event_log.last().map(|stored| &stored.envelope.payload),
        Some(DomainEvent::Graph(GraphEvent::NodeStarted {
            node_id,
            attempt: 1,
        })) if node_id == &seed.target_node_id
    ));
    assert!(
        seed.state
            .event_log
            .iter()
            .all(|stored| !matches!(stored.envelope.payload, DomainEvent::Mutation(_)))
    );

    for (suffix, kind, expected_code) in [
        (
            "mutation-verified",
            ProofKind::MutationVerified,
            "mutation_verification_evidence_missing",
        ),
        (
            "already-satisfied",
            ProofKind::AlreadySatisfied,
            "already_satisfied_proof_unavailable",
        ),
    ] {
        let proof_id = ProofId::new(format!("proof:phase5:forged:{suffix}"));
        let proof = ProofRecord {
            id: proof_id.clone(),
            kind,
            repository_revision: seed.state.repository_revision.clone(),
            node_ids: vec![seed.target_node_id.clone()],
            related_proof_ids: Vec::new(),
            related_evidence_ids: (kind == ProofKind::MutationVerified)
                .then(|| EvidenceId::new("evidence:phase5:forged:mutation"))
                .into_iter()
                .collect(),
            detail_hash: stable_sha256(&[
                "execution-protocol-v1:phase5:forged-completion-proof",
                suffix,
            ]),
        };
        let forged_proof = envelope(
            &seed.state,
            &format!("phase5:forged-proof:{suffix}"),
            EvidenceEvent::ProofRecorded { proof },
        );
        let before_proof = seed.state.clone();
        assert!(matches!(
            seed.state.append_event(forged_proof),
            Err(ProtocolViolation::InvalidProof {
                proof_id: rejected_proof_id,
                code,
            }) if rejected_proof_id == proof_id && code == expected_code
        ));
        assert_eq!(seed.state, before_proof);

        let forged_success = envelope(
            &seed.state,
            &format!("phase5:forged-success:{suffix}"),
            GraphEvent::NodeSucceeded {
                node_id: seed.target_node_id.clone(),
                proof_id: proof_id.clone(),
            },
        );
        let before_success = seed.state.clone();
        assert!(matches!(
            seed.state.append_event(forged_success),
            Err(ProtocolViolation::UnknownProof {
                proof_id: missing_proof_id,
            }) if missing_proof_id == proof_id
        ));
        assert_eq!(seed.state, before_success);
    }

    assert!(matches!(
        seed.state
            .node(&seed.target_node_id)
            .map(|node| &node.state),
        Some(NodeState::Active { attempt: 1 })
    ));
}

#[test]
fn mutation_failure_identity_binds_the_exact_typed_detail() {
    let seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let strategy = seed.policy.permitted_strategies.first().copied();
    let hash_mismatch = MutationFailure::new(
        &seed.policy,
        strategy,
        None,
        MutationFailureClass::CandidateSchemaInvalid,
        MutationFailureDetailCode::ExpectedHashMismatch,
        None,
    )
    .expect("typed expected-hash failure");
    let encoding_invalid = MutationFailure::new(
        &seed.policy,
        strategy,
        None,
        MutationFailureClass::CandidateSchemaInvalid,
        MutationFailureDetailCode::CandidateEncodingInvalid,
        None,
    )
    .expect("typed candidate-encoding failure");

    assert_ne!(
        hash_mismatch.failure_revision_id, encoding_invalid.failure_revision_id,
        "the failure revision must bind the machine-semantic detail code"
    );

    let MutationRecoveryDecision::ModelRetry { policy: hash_retry } = select_mutation_recovery(
        &seed.node,
        &seed.target,
        &seed.context,
        &seed.feasibility,
        &seed.policy,
        &hash_mismatch,
    )
    .expect("hash mismatch has a bounded model retry") else {
        panic!("hash mismatch must select a model retry");
    };
    let MutationRecoveryDecision::ModelRetry {
        policy: encoding_retry,
    } = select_mutation_recovery(
        &seed.node,
        &seed.target,
        &seed.context,
        &seed.feasibility,
        &seed.policy,
        &encoding_invalid,
    )
    .expect("encoding failure has a bounded model retry")
    else {
        panic!("encoding failure must select a model retry");
    };
    assert_ne!(hash_retry.attempt_id, encoding_retry.attempt_id);
    assert_eq!(
        hash_retry.recovery.as_ref().unwrap().failure_revision_id,
        hash_mismatch.failure_revision_id
    );
    assert_eq!(
        encoding_retry
            .recovery
            .as_ref()
            .unwrap()
            .failure_revision_id,
        encoding_invalid.failure_revision_id
    );
    let hash_action = build_prepared_mutation_action(
        &seed.node,
        &seed.target,
        &seed.context,
        &seed.feasibility,
        hash_retry,
        100,
        100,
    )
    .expect("hash-mismatch retry action");
    let encoding_action = build_prepared_mutation_action(
        &seed.node,
        &seed.target,
        &seed.context,
        &seed.feasibility,
        encoding_retry,
        100,
        100,
    )
    .expect("encoding retry action");
    assert_ne!(
        hash_action.provider_request.payload_hash().unwrap(),
        encoding_action.provider_request.payload_hash().unwrap()
    );
    assert_eq!(
        hash_action
            .provider_request
            .recovery
            .as_ref()
            .unwrap()
            .failure_detail_code,
        MutationFailureDetailCode::ExpectedHashMismatch
    );
    assert_eq!(
        encoding_action
            .provider_request
            .recovery
            .as_ref()
            .unwrap()
            .failure_detail_code,
        MutationFailureDetailCode::CandidateEncodingInvalid
    );

    let mut tampered = hash_mismatch;
    tampered.detail_code = MutationFailureDetailCode::CandidateEncodingInvalid;
    assert_eq!(
        tampered
            .validate_against(&seed.policy, &seed.context)
            .expect_err("changing the detail without changing the identity must fail")
            .code(),
        "mutation_failure_binding_invalid"
    );
}

fn two_context_mutation_ledger() -> (MutationLedger, ContextManifestId) {
    let seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let replacement_revision = RepositoryRevisionId::new("repository-revision:ledger-rebuild");
    let replacement_request = build_target_context_load_request(
        &seed.phase4.state.execution_id,
        seed.phase4.state.execution_attempt,
        &replacement_revision,
        &seed.node,
        &seed.phase4.accepted_plan,
        seed.phase4.state.discovery.as_ref().unwrap(),
    )
    .expect("replacement context request");
    let replacement_context = prepare_target_context(
        &replacement_request,
        &materialized_context(&seed.phase4, &replacement_request),
    )
    .expect("replacement context")
    .manifest;
    let replacement_feasibility =
        evaluate_mutation_feasibility(&seed.node, &seed.target, &replacement_context).unwrap();
    let candidate = accepted_candidate(&seed);
    let drift_failure = MutationFailure::new(
        &seed.policy,
        Some(candidate.strategy),
        Some(candidate.candidate_id.clone()),
        MutationFailureClass::RepositoryDrift,
        MutationFailureDetailCode::RepositoryDrift,
        Some(RepositoryDriftRecovery {
            expected_revision: seed.context.repository_revision.clone(),
            observed_revision: replacement_revision,
            expected_fingerprint: seed.context.repository_fingerprint.clone(),
            observed_fingerprint: replacement_context.repository_fingerprint.clone(),
            context_rebuild_required: true,
        }),
    )
    .expect("typed rebuild failure");
    let replacement_policy = select_rebuilt_mutation_policy(
        &seed.phase4.state.execution_id,
        seed.phase4.state.execution_attempt,
        &seed.node,
        &seed.target,
        &replacement_context,
        &replacement_feasibility,
        &seed.context,
        &seed.policy,
        &drift_failure,
    )
    .expect("second global mutation attempt after context rebuild");

    let mut ledger = MutationLedger::default();
    ledger
        .apply(&MutationEvent::FeasibilityEvaluated {
            feasibility: seed.feasibility.clone(),
        })
        .unwrap();
    ledger
        .apply(&MutationEvent::AttemptPolicySelected {
            policy: seed.policy.clone(),
        })
        .unwrap();
    ledger
        .apply(&MutationEvent::ActionPrepared {
            prepared: Box::new(seed.prepared),
        })
        .unwrap();
    ledger
        .apply(&MutationEvent::CandidateRecorded {
            candidate: candidate.clone(),
        })
        .unwrap();
    ledger
        .apply(&MutationEvent::AttemptFailed {
            failure: drift_failure,
        })
        .unwrap();
    ledger
        .apply(&MutationEvent::FeasibilityEvaluated {
            feasibility: replacement_feasibility,
        })
        .unwrap();
    ledger
        .apply(&MutationEvent::AttemptPolicySelected {
            policy: replacement_policy,
        })
        .unwrap();
    ledger.validate().expect("two-context mutation history");
    (ledger, replacement_context.context_manifest_id)
}

#[test]
fn initial_provider_payload_serializes_only_operation_owned_feasible_tools() {
    let cases = [
        (
            FixtureOperation::ModifySmall,
            4_096,
            vec![MutationToolName::ApplyPatch, MutationToolName::ReplaceFile],
        ),
        (
            FixtureOperation::ModifyLarge,
            1_500,
            vec![MutationToolName::ApplyPatch],
        ),
        (
            FixtureOperation::Create,
            4_096,
            vec![MutationToolName::CreateFile],
        ),
        (
            FixtureOperation::Delete,
            4_096,
            vec![MutationToolName::DeleteFile],
        ),
        (
            FixtureOperation::Move,
            4_096,
            vec![MutationToolName::MoveFile],
        ),
    ];

    for (operation, input_ceiling, expected_tools) in cases {
        let seed = mutation_seed(operation, input_ceiling);
        let serialized = provider_json(&seed.prepared);
        let canonical = String::from_utf8(
            seed.prepared
                .provider_request
                .canonical_bytes()
                .expect("canonical provider bytes"),
        )
        .expect("provider request is UTF-8 JSON");
        assert_eq!(
            seed.prepared.provider_request.tool_names(),
            expected_tools,
            "internal contract and serialized boundary must share one authority"
        );
        assert_eq!(
            serialized_tool_names(&serialized),
            expected_tools
                .iter()
                .map(|tool| tool.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            seed.prepared.provider_request.context_manifest_id,
            seed.context.context_manifest_id
        );
        assert_eq!(
            seed.prepared.provider_request.repository_revision,
            seed.phase4.state.repository_revision
        );
        assert_eq!(
            seed.prepared.admission.payload_hash,
            seed.prepared.provider_request.payload_hash().unwrap()
        );
        assert_eq!(
            seed.prepared.admission.input_tokens,
            seed.context.estimated_input_tokens
        );
        assert_eq!(
            seed.prepared.admission.output_tokens,
            seed.node.budget.max_output_tokens_per_call
        );
        assert_eq!(serialized["parallel_tool_calls"], false);
        assert!(canonical.contains("\"additionalProperties\":false"));
        assert!(!canonical.contains("additional_properties"));
        assert!(!canonical.contains("min_length"));
        assert!(!canonical.contains("max_length"));
        for tool in serialized["tools"].as_array().unwrap() {
            let parameters = &tool["function"]["parameters"];
            assert_eq!(parameters["additionalProperties"], false);
            assert!(parameters.get("additional_properties").is_none());
            for property_name in ["patch", "content"] {
                if let Some(property) = parameters["properties"].get(property_name) {
                    assert_eq!(property["minLength"], 0);
                    assert!(property["maxLength"].as_u64().is_some());
                    assert!(property.get("min_length").is_none());
                    assert!(property.get("max_length").is_none());
                }
            }
        }

        match &seed.target.operation {
            TargetOperation::MoveFile { destination, .. } => {
                let tool = &serialized["tools"][0]["function"]["parameters"]["properties"];
                assert_eq!(
                    tool["source_path"]["enum"],
                    serde_json::json!([seed.target.path])
                );
                assert_eq!(
                    tool["destination_path"]["enum"],
                    serde_json::json!([destination])
                );
            }
            _ => {
                for tool in serialized["tools"].as_array().unwrap() {
                    assert_eq!(
                        tool["function"]["parameters"]["properties"]["path"]["enum"],
                        serde_json::json!([seed.target.path])
                    );
                }
            }
        }

        if expected_tools.len() == 1 {
            assert_eq!(
                serialized.pointer("/tool_choice/function/name"),
                Some(&Value::String(expected_tools[0].as_str().to_owned()))
            );
        } else {
            assert_eq!(serialized["tool_choice"], "required");
        }
    }
}

#[test]
fn replacement_feasibility_includes_output_overhead_and_excludes_large_target() {
    let small = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let small_replacement = small
        .feasibility
        .evaluation(MutationStrategy::ReplaceFile)
        .expect("small replacement evaluation");
    assert!(small_replacement.is_feasible());
    assert!(small_replacement.worst_case_output_tokens <= small_replacement.output_allowance);

    let ranged = mutation_seed(FixtureOperation::ModifyLarge, 1_500);
    assert_eq!(
        ranged
            .feasibility
            .evaluation(MutationStrategy::ReplaceFile)
            .expect("ranged replacement evaluation")
            .reason,
        MutationFeasibilityReason::TargetContentUnavailable
    );

    let large = mutation_seed(FixtureOperation::ModifyLarge, 4_096);
    let large_patch = large
        .feasibility
        .evaluation(MutationStrategy::ApplyPatch {
            mode: PatchMode::Initial,
        })
        .expect("large patch evaluation");
    let large_replacement = large
        .feasibility
        .evaluation(MutationStrategy::ReplaceFile)
        .expect("large replacement evaluation");
    assert!(large_patch.is_feasible());
    assert!(!large_replacement.is_feasible());
    assert_eq!(
        large_replacement.reason,
        MutationFeasibilityReason::OutputAllowanceInsufficient
    );
    assert!(large_replacement.worst_case_output_tokens > large_replacement.output_allowance);
    let serialized = provider_json(&large.prepared);
    assert_eq!(serialized_tool_names(&serialized), ["apply_patch"]);
    assert!(
        !String::from_utf8(large.prepared.provider_request.canonical_bytes().unwrap())
            .unwrap()
            .contains("replace_file")
    );
}

#[test]
fn malformed_patch_fallback_has_distinct_identity_and_one_forced_serialized_tool() {
    let seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    for invalid_strategy in [None, Some(MutationStrategy::ReplaceFile)] {
        assert_eq!(
            MutationFailure::new(
                &seed.policy,
                invalid_strategy,
                None,
                MutationFailureClass::PatchMalformed,
                MutationFailureDetailCode::PatchMalformed,
                None,
            )
            .expect_err("malformed-patch failures must name the exact patch strategy")
            .code(),
            "mutation_failure_invalid"
        );
    }
    let failure = MutationFailure::new(
        &seed.policy,
        Some(MutationStrategy::ApplyPatch {
            mode: PatchMode::Initial,
        }),
        None,
        MutationFailureClass::PatchMalformed,
        MutationFailureDetailCode::PatchMalformed,
        None,
    )
    .expect("typed malformed-patch failure");
    let MutationRecoveryDecision::SelectFallback { policy } = select_mutation_recovery(
        &seed.node,
        &seed.target,
        &seed.context,
        &seed.feasibility,
        &seed.policy,
        &failure,
    )
    .expect("small target has replacement fallback") else {
        panic!("small malformed patch must select a feasible fallback");
    };
    assert_eq!(policy.attempt_index, 2);
    assert_eq!(
        policy.prior_attempt_id,
        Some(seed.policy.attempt_id.clone())
    );
    assert_ne!(policy.attempt_id, seed.policy.attempt_id);
    assert_eq!(policy.permitted_strategies, [MutationStrategy::ReplaceFile]);
    assert_eq!(policy.forced_strategy, Some(MutationStrategy::ReplaceFile));

    let fallback = build_prepared_mutation_action(
        &seed.node,
        &seed.target,
        &seed.context,
        &seed.feasibility,
        policy.clone(),
        100,
        100,
    )
    .expect("forced replacement action");
    let replay = build_prepared_mutation_action(
        &seed.node,
        &seed.target,
        &seed.context,
        &seed.feasibility,
        policy,
        100,
        100,
    )
    .expect("deterministic fallback replay");
    assert_eq!(fallback, replay);
    assert_ne!(
        fallback.provider_request.action_id,
        seed.prepared.provider_request.action_id
    );
    assert_ne!(
        fallback.provider_request.call_id,
        seed.prepared.provider_request.call_id
    );
    let serialized = provider_json(&fallback);
    assert_eq!(serialized_tool_names(&serialized), ["replace_file"]);
    assert_eq!(
        serialized.pointer("/tool_choice/function/name"),
        Some(&Value::String("replace_file".into()))
    );
    assert!(
        !fallback
            .provider_request
            .canonical_bytes()
            .unwrap()
            .windows("apply_patch".len())
            .any(|window| window == b"apply_patch")
    );
}

#[test]
fn provider_candidate_rejections_are_typed_deterministic_and_side_effect_free() {
    let seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let state_before = seed.phase4.state.clone();
    let base = patch_invocation(
        &seed,
        b"--- a/src/small_target.rs\n+++ b/src/small_target.rs\n".to_vec(),
        ProviderOutputCompleteness::Complete,
    );

    let mut truncated = base.clone();
    truncated.completeness = ProviderOutputCompleteness::Truncated;
    let truncated_result = record_mutation_candidate(&seed.prepared, &seed.target, &truncated)
        .expect("typed truncated observation");
    let MutationCandidateObservation::Rejected { reason, failure } = &truncated_result else {
        panic!("truncated output cannot become a mutation candidate");
    };
    assert_eq!(*reason, MutationCandidateRejectionReason::OutputTruncated);
    assert_eq!(failure.class, MutationFailureClass::OutputTruncated);
    assert_eq!(failure.retryability, MutationRetryability::NoRetry);
    assert_eq!(
        record_mutation_candidate(&seed.prepared, &seed.target, &truncated).unwrap(),
        truncated_result
    );

    let mut wrong_call = base.clone();
    wrong_call.call_id = ModelCallId::new("model-call:wrong");
    assert!(matches!(
        record_mutation_candidate(&seed.prepared, &seed.target, &wrong_call).unwrap(),
        MutationCandidateObservation::Rejected {
            reason: MutationCandidateRejectionReason::ProviderProtocolViolation,
            ..
        }
    ));

    let mut wrong_path = base.clone();
    let MaterializedMutationArguments::ApplyPatch { path, .. } = &mut wrong_path.arguments else {
        unreachable!();
    };
    *path = ProfilePath::new("src/neighbor.rs").unwrap();
    assert!(matches!(
        record_mutation_candidate(&seed.prepared, &seed.target, &wrong_path).unwrap(),
        MutationCandidateObservation::Rejected {
            reason: MutationCandidateRejectionReason::PathBindingMismatch,
            ..
        }
    ));

    let mut wrong_hash = base.clone();
    let MaterializedMutationArguments::ApplyPatch {
        expected_content_hash,
        ..
    } = &mut wrong_hash.arguments
    else {
        unreachable!();
    };
    *expected_content_hash = stable_sha256(&["wrong-before-hash"]);
    assert!(matches!(
        record_mutation_candidate(&seed.prepared, &seed.target, &wrong_hash).unwrap(),
        MutationCandidateObservation::Rejected {
            reason: MutationCandidateRejectionReason::ExpectedHashMismatch,
            ..
        }
    ));

    let mut forged_receipt = base.clone();
    let MaterializedMutationArguments::ApplyPatch { patch, .. } = &mut forged_receipt.arguments
    else {
        unreachable!();
    };
    patch.handle.persistence_receipt_hash = stable_sha256(&["forged-persistence-receipt"]);
    assert!(matches!(
        record_mutation_candidate(&seed.prepared, &seed.target, &forged_receipt).unwrap(),
        MutationCandidateObservation::Rejected {
            reason: MutationCandidateRejectionReason::ArtifactNotDurable,
            ..
        }
    ));

    let mut forged_locator = base.clone();
    let MaterializedMutationArguments::ApplyPatch { patch, .. } = &mut forged_locator.arguments
    else {
        unreachable!();
    };
    patch.handle.store_locator_hash = stable_sha256(&["forged-store-locator"]);
    assert!(matches!(
        record_mutation_candidate(&seed.prepared, &seed.target, &forged_locator).unwrap(),
        MutationCandidateObservation::Rejected {
            reason: MutationCandidateRejectionReason::ArtifactNotDurable,
            ..
        }
    ));

    let unpermitted = MaterializedMutationInvocation {
        action_id: base.action_id.clone(),
        call_id: base.call_id.clone(),
        tool_call_count: 1,
        completeness: ProviderOutputCompleteness::Complete,
        arguments: MaterializedMutationArguments::DeleteFile {
            path: seed.target.path.clone(),
            expected_content_hash: expected_target_hash(&seed.target),
        },
    };
    assert!(matches!(
        record_mutation_candidate(&seed.prepared, &seed.target, &unpermitted).unwrap(),
        MutationCandidateObservation::Rejected {
            reason: MutationCandidateRejectionReason::ToolNotPermitted,
            ..
        }
    ));

    let maximum_patch_bytes = seed
        .prepared
        .provider_request
        .tools
        .iter()
        .find(|tool| tool.function.name == MutationToolName::ApplyPatch)
        .and_then(|tool| tool.function.parameters.properties.get("patch"))
        .and_then(|schema| schema.max_length)
        .expect("bounded patch schema");
    let oversized = patch_invocation(
        &seed,
        vec![b'x'; usize::try_from(maximum_patch_bytes + 1).unwrap()],
        ProviderOutputCompleteness::Complete,
    );
    assert!(matches!(
        record_mutation_candidate(&seed.prepared, &seed.target, &oversized).unwrap(),
        MutationCandidateObservation::Rejected {
            reason: MutationCandidateRejectionReason::CandidateTooLarge,
            ..
        }
    ));
    assert_eq!(seed.phase4.state, state_before);
}

#[test]
fn mutation_contract_serde_and_candidate_identity_are_strict_stable_and_redacted() {
    let seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let feasibility_again =
        evaluate_mutation_feasibility(&seed.node, &seed.target, &seed.context).unwrap();
    let policy_again = select_initial_mutation_policy(
        &seed.phase4.state.execution_id,
        seed.phase4.state.execution_attempt,
        &seed.node,
        &seed.target,
        &seed.context,
        &feasibility_again,
    )
    .unwrap();
    let prepared_again = build_prepared_mutation_action(
        &seed.node,
        &seed.target,
        &seed.context,
        &feasibility_again,
        policy_again,
        100,
        100,
    )
    .unwrap();
    assert_eq!(seed.feasibility, feasibility_again);
    assert_eq!(seed.prepared, prepared_again);

    let request_bytes = seed.prepared.provider_request.canonical_bytes().unwrap();
    let request_roundtrip: MutationProviderRequestContract =
        serde_json::from_slice(&request_bytes).expect("strict provider request roundtrip");
    assert_eq!(request_roundtrip, seed.prepared.provider_request);
    let mut request_unknown = serde_json::to_value(&request_roundtrip).unwrap();
    request_unknown
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), Value::Bool(true));
    assert!(serde_json::from_value::<MutationProviderRequestContract>(request_unknown).is_err());

    let named_request = mutation_seed(FixtureOperation::Create, 4_096)
        .prepared
        .provider_request;
    let mut named_choice_unknown = serde_json::to_value(named_request).unwrap();
    named_choice_unknown
        .pointer_mut("/tool_choice")
        .and_then(Value::as_object_mut)
        .unwrap()
        .insert("unexpected".into(), Value::Bool(true));
    assert!(
        serde_json::from_value::<MutationProviderRequestContract>(named_choice_unknown).is_err(),
        "the actual named tool-choice object must reject unknown fields"
    );

    let invocation = patch_invocation(
        &seed,
        MUTATION_SECRET_SENTINEL.as_bytes().to_vec(),
        ProviderOutputCompleteness::Complete,
    );
    assert!(!format!("{invocation:?}").contains(MUTATION_SECRET_SENTINEL));
    let accepted = record_mutation_candidate(&seed.prepared, &seed.target, &invocation).unwrap();
    let MutationCandidateObservation::Accepted { candidate } = accepted else {
        panic!("bounded complete invocation must produce a candidate receipt");
    };
    let repeated = record_mutation_candidate(&seed.prepared, &seed.target, &invocation).unwrap();
    assert_eq!(
        repeated,
        MutationCandidateObservation::Accepted {
            candidate: candidate.clone(),
        }
    );
    let candidate_json = serde_json::to_string(&candidate).unwrap();
    assert!(!candidate_json.contains(MUTATION_SECRET_SENTINEL));
    assert!(
        !String::from_utf8(request_bytes)
            .unwrap()
            .contains("DEFAULT_INCREMENT")
    );
    let candidate_roundtrip: MutationCandidateRecord =
        serde_json::from_str(&candidate_json).expect("strict candidate roundtrip");
    assert_eq!(candidate_roundtrip, candidate);
    let mut candidate_unknown = serde_json::to_value(&candidate).unwrap();
    candidate_unknown.as_object_mut().unwrap().insert(
        "raw_content".into(),
        Value::String(MUTATION_SECRET_SENTINEL.into()),
    );
    assert!(serde_json::from_value::<MutationCandidateRecord>(candidate_unknown).is_err());
}

#[test]
fn operation_owned_apply_and_verification_produce_exact_revision_evidence() {
    let cases = [
        (FixtureOperation::ModifySmall, 4_096),
        (FixtureOperation::Create, 4_096),
        (FixtureOperation::Delete, 4_096),
        (FixtureOperation::Move, 4_096),
    ];
    for (operation, input_ceiling) in cases {
        let seed = mutation_seed(operation, input_ceiling);
        let candidate = accepted_candidate(&seed);
        let apply =
            MutationApplyRequest::new(&seed.prepared, &candidate, &seed.target, &seed.context)
                .expect("operation-owned apply request");
        assert_eq!(apply.owned_paths, candidate.operation.owned_paths());
        let application =
            MutationApplicationObservation::new(&apply, MutationApplicationStatus::Applied);
        let verify =
            MutationVerifyRequest::new(&apply, &application).expect("verification request");
        let materialized = materialized_verification(&seed, &candidate, &verify);
        assert_eq!(materialized.changed_paths, apply.owned_paths);
        let evidence = verify_mutation_application(
            &verify,
            &apply,
            &application,
            &candidate,
            &seed.target,
            &materialized,
        )
        .expect("verified repository operation");
        evidence
            .validate()
            .expect("canonical verification evidence");
        assert_eq!(
            evidence.repository_revision_before,
            seed.context.repository_revision
        );
        assert_eq!(
            evidence.repository_revision_after,
            derive_repository_revision(
                &seed.context.repository_revision,
                &materialized.repository_fingerprint_after,
                &candidate.candidate_id,
            )
        );
        assert_ne!(
            evidence.repository_revision_before,
            evidence.repository_revision_after
        );
        assert_eq!(evidence.changed_paths, candidate.operation.owned_paths());
        assert_eq!(
            verify_mutation_application(
                &verify,
                &apply,
                &application,
                &candidate,
                &seed.target,
                &materialized,
            )
            .unwrap(),
            evidence
        );

        let apply_json = serde_json::to_value(&apply).expect("serialize apply contract");
        let apply_roundtrip: MutationApplyRequest =
            serde_json::from_value(apply_json.clone()).expect("strict apply roundtrip");
        assert_eq!(apply_roundtrip, apply);
        let evidence_json = serde_json::to_string(&evidence).expect("serialize safe evidence");
        assert!(!evidence_json.contains(MUTATION_SECRET_SENTINEL));
        assert!(!format!("{materialized:?}").contains(MUTATION_SECRET_SENTINEL));
    }
}

#[test]
fn stale_or_extra_path_verification_is_rejected_without_producing_evidence() {
    let seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let candidate = accepted_candidate(&seed);
    let apply =
        MutationApplyRequest::new(&seed.prepared, &candidate, &seed.target, &seed.context).unwrap();
    let application =
        MutationApplicationObservation::new(&apply, MutationApplicationStatus::Applied);
    let verify = MutationVerifyRequest::new(&apply, &application).unwrap();
    let valid = materialized_verification(&seed, &candidate, &verify);

    let mut stale = valid.clone();
    stale.repository_revision = RepositoryRevisionId::new("repository-revision:stale");
    assert_eq!(
        verify_mutation_application(
            &verify,
            &apply,
            &application,
            &candidate,
            &seed.target,
            &stale,
        )
        .expect_err("stale repository observation")
        .code(),
        "mutation_verification_observation_invalid"
    );

    let mut extra = valid.clone();
    let extra_path = ProfilePath::new("src/neighbor.rs").unwrap();
    extra.changed_paths.insert(extra_path.clone());
    extra.path_transitions.insert(
        extra_path,
        MutationPathTransition {
            before: file_state(stable_sha256(&["neighbor-before"]), 8),
            after: file_state(stable_sha256(&["neighbor-after"]), 9),
        },
    );
    assert_eq!(
        verify_mutation_application(
            &verify,
            &apply,
            &application,
            &candidate,
            &seed.target,
            &extra,
        )
        .expect_err("extra changed path is outside target ownership")
        .code(),
        "mutation_verification_observation_invalid"
    );
    let accepted = verify_mutation_application(
        &verify,
        &apply,
        &application,
        &candidate,
        &seed.target,
        &valid,
    )
    .expect("valid verification remains independently acceptable");
    assert_eq!(accepted.changed_paths, BTreeSet::from([seed.target.path]));
}

#[test]
fn empty_file_candidate_and_verified_path_state_are_valid() {
    let seed = mutation_seed(FixtureOperation::Create, 4_096);
    let invocation = MaterializedMutationInvocation {
        action_id: seed.prepared.provider_request.action_id.clone(),
        call_id: seed.prepared.provider_request.call_id.clone(),
        tool_call_count: 1,
        completeness: ProviderOutputCompleteness::Complete,
        arguments: MaterializedMutationArguments::CreateFile {
            path: seed.target.path.clone(),
            content: durable_artifact("empty-created-file", Vec::new()),
        },
    };
    let MutationCandidateObservation::Accepted { candidate } =
        record_mutation_candidate(&seed.prepared, &seed.target, &invocation).unwrap()
    else {
        panic!("a complete empty UTF-8 file is a valid candidate");
    };
    let MutationCandidateOperation::CreateFile { content, .. } = &candidate.operation else {
        unreachable!();
    };
    assert_eq!(content.byte_len, 0);
    assert_eq!(
        content.content_hash,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );

    let apply =
        MutationApplyRequest::new(&seed.prepared, &candidate, &seed.target, &seed.context).unwrap();
    let application =
        MutationApplicationObservation::new(&apply, MutationApplicationStatus::Applied);
    let verify = MutationVerifyRequest::new(&apply, &application).unwrap();
    let materialized = materialized_verification(&seed, &candidate, &verify);
    let transition = materialized
        .path_transitions
        .get(&seed.target.path)
        .expect("created file transition");
    assert!(matches!(
        transition.after,
        MutationPathState::File { byte_len: 0, .. }
    ));
    verify_mutation_application(
        &verify,
        &apply,
        &application,
        &candidate,
        &seed.target,
        &materialized,
    )
    .expect("empty file state remains valid verification evidence");
}

#[test]
fn aggregate_mutation_lifecycle_is_budget_exact_replayable_and_barrier_eligible() {
    let mut seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let initial_revision = seed.phase4.state.repository_revision.clone();
    let node_usage_before = seed
        .phase4
        .state
        .node(&seed.node.id)
        .expect("active mutation owner")
        .usage
        .clone();
    let mission_usage_before = seed.phase4.state.budgets.mission_usage.clone();
    let prepared = prepare_aggregate_mutation(&mut seed, "aggregate-success");
    dispatch_and_consume_aggregate_mutation(
        &mut seed.phase4.state,
        &prepared,
        "aggregate-success",
        80,
        50,
    );

    let node_usage = &seed.phase4.state.node(&seed.node.id).unwrap().usage;
    assert_eq!(
        node_usage.model_calls_consumed,
        node_usage_before.model_calls_consumed + 1
    );
    assert_eq!(node_usage.model_calls_reserved, 0);
    assert_eq!(
        node_usage.cost_micros_consumed,
        node_usage_before.cost_micros_consumed + 80
    );
    assert_eq!(node_usage.cost_micros_reserved, 0);
    assert_eq!(
        node_usage.duration_ms_consumed,
        node_usage_before.duration_ms_consumed + 50
    );
    assert_eq!(node_usage.duration_ms_reserved, 0);
    assert_eq!(
        node_usage.mutation_attempts,
        node_usage_before.mutation_attempts + 1
    );
    assert_eq!(
        seed.phase4.state.budgets.mission_usage.model_calls_consumed,
        mission_usage_before.model_calls_consumed + 1
    );
    assert_eq!(
        seed.phase4.state.budgets.mission_usage.mutation_attempts,
        mission_usage_before.mutation_attempts + 1
    );

    let MutationCandidateObservation::Accepted { candidate } =
        record_mutation_candidate(&prepared, &seed.target, &canonical_invocation(&seed))
            .expect("provider candidate observation")
    else {
        panic!("canonical provider result must be admitted");
    };
    let candidate_event = append(
        &mut seed.phase4.state,
        "phase5:aggregate-success:candidate",
        MutationEvent::CandidateRecorded {
            candidate: candidate.clone(),
        },
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Mutation(MutationEffectRequest::ApplyMutation { request: apply }),
    } = decide(&seed.phase4.state).expect("accepted candidate requests target-local apply")
    else {
        panic!("accepted candidate must reach the apply effect");
    };
    assert_eq!(
        apply.owned_paths,
        BTreeSet::from([seed.target.path.clone()])
    );
    let application =
        MutationApplicationObservation::new(&apply, MutationApplicationStatus::Applied);
    append(
        &mut seed.phase4.state,
        "phase5:aggregate-success:application",
        MutationEvent::ApplicationObserved {
            request: (*apply).clone(),
            observation: application.clone(),
        },
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Mutation(MutationEffectRequest::VerifyMutation { request: verify }),
    } = decide(&seed.phase4.state).expect("applied candidate requests exact verification")
    else {
        panic!("application observation must reach verification");
    };
    let materialized = materialized_verification(&seed, &candidate, &verify);
    let evidence = verify_mutation_application(
        &verify,
        &apply,
        &application,
        &candidate,
        &seed.target,
        &materialized,
    )
    .expect("exact repository mutation verification");
    let mut forged_unowned_observation = evidence.clone();
    let unowned_path = ProfilePath::new("src/neighbor.rs").unwrap();
    let unchanged = file_state(stable_sha256(&["phase5:unowned-unchanged"]), 9);
    forged_unowned_observation.path_transitions.insert(
        unowned_path,
        MutationPathTransition {
            before: unchanged.clone(),
            after: unchanged,
        },
    );
    rebind_verification_identity(&mut forged_unowned_observation);
    forged_unowned_observation
        .validate()
        .expect("generic evidence shape permits unchanged observations");
    let forged_event = envelope(
        &seed.phase4.state,
        "phase5:aggregate-success:forged-unowned-verification",
        MutationEvent::MutationVerified {
            evidence: forged_unowned_observation,
        },
    );
    let before_forged = seed.phase4.state.clone();
    assert!(matches!(
        seed.phase4.state.append_event(forged_event),
        Err(ProtocolViolation::MutationContract {
            code: "mutation_verification_chain_mismatch"
        })
    ));
    assert_eq!(seed.phase4.state, before_forged);
    let verified_event = append(
        &mut seed.phase4.state,
        "phase5:aggregate-success:verified",
        MutationEvent::MutationVerified {
            evidence: evidence.clone(),
        },
    );
    assert_eq!(
        seed.phase4.state.repository_revision,
        evidence.repository_revision_after
    );
    assert_ne!(seed.phase4.state.repository_revision, initial_revision);

    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof }) = append_next_emitted(
        &mut seed.phase4.state,
        "phase5:aggregate-success:verification-proof",
    ) else {
        panic!("verified mutation must produce its canonical proof");
    };
    assert_eq!(proof.kind, ProofKind::MutationVerified);
    assert_eq!(
        proof.node_ids.as_slice(),
        std::slice::from_ref(&seed.node.id)
    );
    assert_eq!(
        proof.related_evidence_ids.as_slice(),
        std::slice::from_ref(&evidence.evidence_id)
    );
    assert_eq!(
        proof.repository_revision,
        evidence.repository_revision_after
    );
    assert_eq!(
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:aggregate-success:node-succeeded",
        ),
        GraphEvent::NodeSucceeded {
            node_id: seed.node.id.clone(),
            proof_id: proof.id.clone(),
        }
        .into()
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof: barrier }) =
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:aggregate-success:implementation-barrier",
        )
    else {
        panic!("all required implementation nodes must make the barrier eligible");
    };
    assert_eq!(barrier.kind, ProofKind::ImplementationBarrier);
    assert_eq!(barrier.related_proof_ids, [proof.id]);
    assert_eq!(
        barrier.repository_revision,
        evidence.repository_revision_after
    );
    assert!(matches!(
        decide(&seed.phase4.state).unwrap(),
        ProtocolDecision::Emit {
            event: DomainEvent::Lifecycle(LifecycleEvent::PositionAdvanced {
                from: ProtocolStage::Implementation,
                to: ProtocolStage::Validation,
                ..
            })
        }
    ));

    let after_lifecycle = seed.phase4.state.clone();
    assert!(matches!(
        seed.phase4
            .state
            .append_event(candidate_event)
            .expect("exact candidate event replay"),
        AppendOutcome::IdempotentReplay { .. }
    ));
    assert!(matches!(
        seed.phase4
            .state
            .append_event(verified_event)
            .expect("exact verification event replay"),
        AppendOutcome::IdempotentReplay { .. }
    ));
    assert_eq!(seed.phase4.state, after_lifecycle);
    let restored =
        InMemoryEventStore::restore(seed.phase4.trusted_initial, seed.phase4.state.clone())
            .expect("full mutation lifecycle restores from trusted events")
            .into_state();
    assert_eq!(restored, seed.phase4.state);
}

#[test]
fn verified_mutation_rejects_context_supersession_before_proof_atomically() {
    let mut seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let prepared = prepare_aggregate_mutation(&mut seed, "verified-supersession");
    dispatch_and_consume_aggregate_mutation(
        &mut seed.phase4.state,
        &prepared,
        "verified-supersession",
        80,
        50,
    );
    let MutationCandidateObservation::Accepted { candidate } =
        record_mutation_candidate(&prepared, &seed.target, &canonical_invocation(&seed)).unwrap()
    else {
        panic!("canonical provider result must be admitted");
    };
    append(
        &mut seed.phase4.state,
        "phase5:verified-supersession:candidate",
        MutationEvent::CandidateRecorded {
            candidate: candidate.clone(),
        },
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Mutation(MutationEffectRequest::ApplyMutation { request: apply }),
    } = decide(&seed.phase4.state).expect("accepted candidate requests target-local apply")
    else {
        panic!("accepted candidate must reach the apply effect");
    };
    let application =
        MutationApplicationObservation::new(&apply, MutationApplicationStatus::Applied);
    append(
        &mut seed.phase4.state,
        "phase5:verified-supersession:application",
        MutationEvent::ApplicationObserved {
            request: (*apply).clone(),
            observation: application.clone(),
        },
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Mutation(MutationEffectRequest::VerifyMutation { request: verify }),
    } = decide(&seed.phase4.state).expect("applied candidate requests exact verification")
    else {
        panic!("application observation must reach verification");
    };
    let evidence = verify_mutation_application(
        &verify,
        &apply,
        &application,
        &candidate,
        &seed.target,
        &materialized_verification(&seed, &candidate, &verify),
    )
    .expect("exact repository mutation verification");
    append(
        &mut seed.phase4.state,
        "phase5:verified-supersession:verified",
        MutationEvent::MutationVerified { evidence },
    );

    let prepared_context = seed
        .phase4
        .state
        .implementation
        .as_ref()
        .unwrap()
        .prepared_context_for_node(&seed.node.id)
        .unwrap()
        .clone();
    let supersession = TargetContextSupersession::new(
        &prepared_context,
        seed.phase4.state.repository_revision.clone(),
    )
    .expect("well-formed supersession against the now-stale prepared context");
    let event = envelope(
        &seed.phase4.state,
        "phase5:verified-supersession:forged",
        ImplementationEvent::TargetContextSuperseded {
            supersession: Box::new(supersession),
        },
    );
    let before = seed.phase4.state.clone();
    assert!(matches!(
        seed.phase4.state.append_event(event),
        Err(ProtocolViolation::MutationContract {
            code: "implementation_context_change_after_mutation_terminal"
        })
    ));
    assert_eq!(seed.phase4.state, before);
    assert!(matches!(
        decide(&seed.phase4.state).unwrap(),
        ProtocolDecision::Emit {
            event: DomainEvent::Evidence(EvidenceEvent::ProofRecorded { .. })
        }
    ));
}

#[test]
fn aggregate_fallback_is_attempt_scoped_and_dispatches_only_the_forced_tool() {
    let mut seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let initial = prepare_aggregate_mutation(&mut seed, "aggregate-fallback-initial");
    assert_eq!(
        initial.provider_request.tool_names(),
        [MutationToolName::ApplyPatch, MutationToolName::ReplaceFile]
    );
    dispatch_and_consume_aggregate_mutation(
        &mut seed.phase4.state,
        &initial,
        "aggregate-fallback-initial",
        75,
        45,
    );
    let failure = MutationFailure::new(
        &initial.policy,
        Some(MutationStrategy::ApplyPatch {
            mode: PatchMode::Initial,
        }),
        None,
        MutationFailureClass::PatchMalformed,
        MutationFailureDetailCode::PatchMalformed,
        None,
    )
    .expect("typed malformed-patch failure");
    let failure_event = append(
        &mut seed.phase4.state,
        "phase5:aggregate-fallback-initial:rejected",
        MutationEvent::ActionRejected {
            failure: failure.clone(),
        },
    );

    let DomainEvent::Mutation(MutationEvent::AttemptPolicySelected {
        policy: fallback_policy,
    }) = append_next_emitted(&mut seed.phase4.state, "phase5:aggregate-fallback:policy")
    else {
        panic!("malformed initial patch must select the bounded same-target fallback");
    };
    assert_eq!(fallback_policy.attempt_index, 2);
    assert_eq!(
        fallback_policy.prior_attempt_id,
        Some(initial.policy.attempt_id.clone())
    );
    assert_eq!(
        fallback_policy.permitted_strategies,
        [MutationStrategy::ReplaceFile]
    );
    assert_eq!(
        fallback_policy.forced_strategy,
        Some(MutationStrategy::ReplaceFile)
    );
    assert_ne!(fallback_policy.attempt_id, initial.policy.attempt_id);

    let DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared: fallback }) =
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:aggregate-fallback:action-prepared",
        )
    else {
        panic!("fallback policy must prepare its own provider request");
    };
    let fallback = *fallback;
    let serialized = provider_json(&fallback);
    let recovery = fallback
        .provider_request
        .recovery
        .as_ref()
        .expect("fallback request must carry its exact recovery cause");
    assert_eq!(recovery.kind, MutationRecoveryKind::StrategyFallback);
    assert_eq!(recovery.failure_revision_id, failure.failure_revision_id);
    assert_eq!(
        recovery.failure_detail_code,
        MutationFailureDetailCode::PatchMalformed
    );
    assert_eq!(
        fallback.provider_request.permitted_strategies,
        [MutationStrategy::ReplaceFile]
    );
    assert_eq!(serialized_tool_names(&serialized), ["replace_file"]);
    assert_eq!(
        serialized.pointer("/tool_choice/function/name"),
        Some(&Value::String("replace_file".into()))
    );
    assert!(
        !fallback
            .provider_request
            .canonical_bytes()
            .unwrap()
            .windows("apply_patch".len())
            .any(|window| window == b"apply_patch")
    );
    assert_ne!(
        fallback.provider_request.call_id,
        initial.provider_request.call_id
    );
    dispatch_and_consume_aggregate_mutation(
        &mut seed.phase4.state,
        &fallback,
        "aggregate-fallback",
        70,
        40,
    );

    let target_state = seed
        .phase4
        .state
        .mutation
        .contexts
        .get(&seed.context.context_manifest_id)
        .expect("current target mutation projection");
    assert_eq!(target_state.attempts.len(), 2);
    assert_eq!(
        target_state.attempts[&1]
            .prepared_action
            .as_ref()
            .unwrap()
            .provider_request
            .tool_names(),
        [MutationToolName::ApplyPatch, MutationToolName::ReplaceFile]
    );
    assert_eq!(
        target_state.attempts[&2]
            .prepared_action
            .as_ref()
            .unwrap()
            .provider_request
            .tool_names(),
        [MutationToolName::ReplaceFile]
    );
    assert_eq!(
        seed.phase4
            .state
            .node(&seed.node.id)
            .unwrap()
            .usage
            .mutation_attempts,
        2
    );
    assert!(matches!(
        seed.phase4
            .state
            .append_event(failure_event)
            .expect("exact failure replay remains idempotent"),
        AppendOutcome::IdempotentReplay { .. }
    ));
}

#[test]
fn only_candidate_bound_repository_drift_can_advance_revision_and_rebuild_context() {
    let mut seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let prepared = prepare_aggregate_mutation(&mut seed, "aggregate-drift");
    dispatch_and_consume_aggregate_mutation(
        &mut seed.phase4.state,
        &prepared,
        "aggregate-drift",
        75,
        45,
    );
    let MutationCandidateObservation::Accepted { candidate } =
        record_mutation_candidate(&prepared, &seed.target, &canonical_invocation(&seed)).unwrap()
    else {
        panic!("canonical candidate must be accepted before apply-time drift");
    };
    append(
        &mut seed.phase4.state,
        "phase5:aggregate-drift:candidate",
        MutationEvent::CandidateRecorded {
            candidate: candidate.clone(),
        },
    );
    assert!(matches!(
        decide(&seed.phase4.state).unwrap(),
        ProtocolDecision::Perform {
            effect: EffectRequest::Mutation(MutationEffectRequest::ApplyMutation { .. })
        }
    ));

    let prepared_context = seed
        .phase4
        .state
        .implementation
        .as_ref()
        .unwrap()
        .prepared_context_for_node(&seed.node.id)
        .unwrap()
        .clone();
    let arbitrary_revision = RepositoryRevisionId::new("repository-revision:unproven");
    let arbitrary_supersession =
        TargetContextSupersession::new(&prepared_context, arbitrary_revision).unwrap();
    let arbitrary_event = envelope(
        &seed.phase4.state,
        "phase5:aggregate-drift:unproven-supersession",
        ImplementationEvent::TargetContextSuperseded {
            supersession: Box::new(arbitrary_supersession),
        },
    );
    let before_unproven = seed.phase4.state.clone();
    assert!(matches!(
        seed.phase4.state.append_event(arbitrary_event),
        Err(ProtocolViolation::MutationContract {
            code: "target_context_drift_adoption_not_authoritative"
        })
    ));
    assert_eq!(seed.phase4.state, before_unproven);

    let observed_revision = RepositoryRevisionId::new("repository-revision:observed-drift");
    let drift = RepositoryDriftRecovery {
        expected_revision: seed.context.repository_revision.clone(),
        observed_revision: observed_revision.clone(),
        expected_fingerprint: seed.context.repository_fingerprint.clone(),
        observed_fingerprint: stable_sha256(&["phase5:observed-drift-fingerprint"]),
        context_rebuild_required: true,
    };
    let failure = MutationFailure::new(
        &prepared.policy,
        Some(candidate.strategy),
        Some(candidate.candidate_id.clone()),
        MutationFailureClass::RepositoryDrift,
        MutationFailureDetailCode::RepositoryDrift,
        Some(drift),
    )
    .expect("candidate-bound repository drift");
    append(
        &mut seed.phase4.state,
        "phase5:aggregate-drift:attempt-failed",
        MutationEvent::AttemptFailed { failure },
    );
    let model_call_count = seed.phase4.state.budgets.model_calls.len();
    let mutation_attempts = seed
        .phase4
        .state
        .node(&seed.node.id)
        .unwrap()
        .usage
        .mutation_attempts;
    let rebuilds_before = seed
        .phase4
        .state
        .node(&seed.node.id)
        .unwrap()
        .usage
        .context_rebuilds;
    let DomainEvent::Implementation(ImplementationEvent::TargetContextSuperseded { supersession }) =
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:aggregate-drift:authoritative-supersession",
        )
    else {
        panic!("typed repository drift must authorize exact context supersession");
    };
    assert_eq!(
        supersession.replacement_repository_revision,
        observed_revision
    );
    assert_eq!(seed.phase4.state.repository_revision, observed_revision);
    assert_eq!(
        seed.phase4.state.budgets.model_calls.len(),
        model_call_count
    );
    assert_eq!(
        seed.phase4
            .state
            .node(&seed.node.id)
            .unwrap()
            .usage
            .mutation_attempts,
        mutation_attempts
    );
    assert_eq!(
        seed.phase4
            .state
            .node(&seed.node.id)
            .unwrap()
            .usage
            .context_rebuilds,
        rebuilds_before + 1
    );
    let ProtocolDecision::Perform {
        effect:
            EffectRequest::Implementation(ImplementationEffectRequest::LoadTargetContext { request }),
    } = decide(&seed.phase4.state).expect("superseded context is reloaded read-only")
    else {
        panic!("context drift must reload evidence before another provider mutation call");
    };
    assert_eq!(request.repository_revision, observed_revision);
    assert_eq!(request.node_id, seed.node.id);
    assert_eq!(
        seed.phase4.state.budgets.model_calls.len(),
        model_call_count
    );
}

#[test]
fn exhausted_context_rebuild_adopts_the_observed_revision_before_terminal() {
    let mut seed = mutation_seed_with_budget(FixtureOperation::ModifySmall, 4_096, |budget| {
        budget.max_context_rebuilds = 0
    });
    let prepared = prepare_aggregate_mutation(&mut seed, "aggregate-drift-exhausted");
    dispatch_and_consume_aggregate_mutation(
        &mut seed.phase4.state,
        &prepared,
        "aggregate-drift-exhausted",
        75,
        45,
    );
    let MutationCandidateObservation::Accepted { candidate } =
        record_mutation_candidate(&prepared, &seed.target, &canonical_invocation(&seed)).unwrap()
    else {
        panic!("canonical candidate must be persisted before apply-time drift");
    };
    append(
        &mut seed.phase4.state,
        "phase5:aggregate-drift-exhausted:candidate",
        MutationEvent::CandidateRecorded {
            candidate: candidate.clone(),
        },
    );

    let observed_revision =
        RepositoryRevisionId::new("repository-revision:observed-drift-at-rebuild-limit");
    let failure = MutationFailure::new(
        &prepared.policy,
        Some(candidate.strategy),
        Some(candidate.candidate_id.clone()),
        MutationFailureClass::RepositoryDrift,
        MutationFailureDetailCode::RepositoryDrift,
        Some(RepositoryDriftRecovery {
            expected_revision: seed.context.repository_revision.clone(),
            observed_revision: observed_revision.clone(),
            expected_fingerprint: seed.context.repository_fingerprint.clone(),
            observed_fingerprint: stable_sha256(&[
                "phase5:observed-drift-at-rebuild-limit-fingerprint",
            ]),
            context_rebuild_required: true,
        }),
    )
    .expect("candidate-bound repository drift at the rebuild limit");
    append(
        &mut seed.phase4.state,
        "phase5:aggregate-drift-exhausted:attempt-failed",
        MutationEvent::AttemptFailed { failure },
    );

    let DomainEvent::Mutation(MutationEvent::ConvergenceEvaluated { convergence }) =
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:aggregate-drift-exhausted:convergence",
        )
    else {
        panic!("rebuild exhaustion must converge instead of attempting supersession");
    };
    assert_eq!(
        convergence.reason,
        MutationConvergenceReason::ContextRebuildBudgetExhausted
    );
    assert_eq!(
        convergence.repository_revision,
        seed.context.repository_revision
    );
    assert_eq!(convergence.repository_revision_after, observed_revision);
    assert_eq!(seed.phase4.state.repository_revision, observed_revision);
    assert_eq!(
        seed.phase4
            .state
            .node(&seed.node.id)
            .unwrap()
            .usage
            .context_rebuilds,
        0
    );

    assert_eq!(
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:aggregate-drift-exhausted:node-failed",
        ),
        GraphEvent::NodeFailed {
            node_id: seed.node.id.clone(),
            failure_revision_id: convergence.last_failure_revision_id,
            terminal: true,
        }
        .into()
    );
    let ProtocolDecision::Finish { result } = decide(&seed.phase4.state).unwrap() else {
        panic!("context-rebuild exhaustion must have a canonical terminal result");
    };
    assert_eq!(result.mission.outcome(), MissionOutcomeV1::BudgetBlocked);
    assert_eq!(result.repository_revision, observed_revision);
    assert_eq!(
        result.reason_code,
        "mutation_context_rebuild_budget_exhausted"
    );
    append(
        &mut seed.phase4.state,
        "phase5:aggregate-drift-exhausted:terminal",
        TerminalEvent::CanonicalResultRecorded { result },
    );
    InMemoryEventStore::restore(seed.phase4.trusted_initial, seed.phase4.state)
        .expect("drift exhaustion revision adoption must replay exactly");
}

#[test]
fn final_mutation_attempt_drift_adopts_observed_revision_and_converges_without_rebuild() {
    let mut seed = mutation_seed_with_budget(FixtureOperation::ModifySmall, 4_096, |budget| {
        budget.max_mutation_attempts = 1
    });
    let prepared = prepare_aggregate_mutation(&mut seed, "final-attempt-drift");
    dispatch_and_consume_aggregate_mutation(
        &mut seed.phase4.state,
        &prepared,
        "final-attempt-drift",
        75,
        45,
    );
    let MutationCandidateObservation::Accepted { candidate } =
        record_mutation_candidate(&prepared, &seed.target, &canonical_invocation(&seed)).unwrap()
    else {
        panic!("canonical candidate must be persisted before final-attempt drift");
    };
    append(
        &mut seed.phase4.state,
        "phase5:final-attempt-drift:candidate",
        MutationEvent::CandidateRecorded {
            candidate: candidate.clone(),
        },
    );
    assert!(matches!(
        decide(&seed.phase4.state).unwrap(),
        ProtocolDecision::Perform {
            effect: EffectRequest::Mutation(MutationEffectRequest::ApplyMutation { .. })
        }
    ));

    let observed_revision =
        RepositoryRevisionId::new("repository-revision:observed-drift-at-attempt-limit");
    let drift = RepositoryDriftRecovery {
        expected_revision: seed.context.repository_revision.clone(),
        observed_revision: observed_revision.clone(),
        expected_fingerprint: seed.context.repository_fingerprint.clone(),
        observed_fingerprint: stable_sha256(&[
            "phase5:observed-drift-at-attempt-limit-fingerprint",
        ]),
        context_rebuild_required: true,
    };
    let failure = MutationFailure::new(
        &prepared.policy,
        Some(candidate.strategy),
        Some(candidate.candidate_id.clone()),
        MutationFailureClass::RepositoryDrift,
        MutationFailureDetailCode::RepositoryDrift,
        Some(drift.clone()),
    )
    .expect("candidate-bound repository drift on the final mutation attempt");
    append(
        &mut seed.phase4.state,
        "phase5:final-attempt-drift:attempt-failed",
        MutationEvent::AttemptFailed { failure },
    );

    let current_prepared = seed
        .phase4
        .state
        .implementation
        .as_ref()
        .unwrap()
        .prepared_context_for_node(&seed.node.id)
        .unwrap()
        .clone();
    let forged_supersession =
        TargetContextSupersession::new(&current_prepared, observed_revision.clone())
            .expect("the forged event is structurally well formed");
    let forged_event = envelope(
        &seed.phase4.state,
        "phase5:final-attempt-drift:forged-supersession",
        ImplementationEvent::TargetContextSuperseded {
            supersession: Box::new(forged_supersession),
        },
    );
    let before_forged_supersession = seed.phase4.state.clone();
    assert!(matches!(
        seed.phase4.state.append_event(forged_event),
        Err(ProtocolViolation::MutationContract {
            code: "target_context_drift_adoption_not_authoritative"
        })
    ));
    assert_eq!(seed.phase4.state, before_forged_supersession);

    let DomainEvent::Mutation(MutationEvent::ConvergenceEvaluated { convergence }) =
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:final-attempt-drift:convergence",
        )
    else {
        panic!("final-attempt drift must converge before any context rebuild or load");
    };
    assert_eq!(
        convergence.reason,
        MutationConvergenceReason::MutationAttemptBudgetExhausted
    );
    assert_eq!(convergence.repository_drift, Some(drift));
    assert_eq!(
        convergence.repository_revision,
        seed.context.repository_revision
    );
    assert_eq!(convergence.repository_revision_after, observed_revision);
    assert_eq!(seed.phase4.state.repository_revision, observed_revision);
    assert_eq!(
        seed.phase4
            .state
            .node(&seed.node.id)
            .unwrap()
            .usage
            .context_rebuilds,
        0
    );

    assert_eq!(
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:final-attempt-drift:node-failed",
        ),
        GraphEvent::NodeFailed {
            node_id: seed.node.id.clone(),
            failure_revision_id: convergence.last_failure_revision_id,
            terminal: true,
        }
        .into()
    );
    let ProtocolDecision::Finish { result } = decide(&seed.phase4.state).unwrap() else {
        panic!("final-attempt drift must have a canonical terminal result");
    };
    assert_eq!(result.mission.outcome(), MissionOutcomeV1::BudgetBlocked);
    assert_eq!(result.repository_revision, observed_revision);
    assert_eq!(result.reason_code, "mutation_attempt_budget_exhausted");
    append(
        &mut seed.phase4.state,
        "phase5:final-attempt-drift:terminal",
        TerminalEvent::CanonicalResultRecorded { result },
    );
    InMemoryEventStore::restore(seed.phase4.trusted_initial, seed.phase4.state)
        .expect("final-attempt drift convergence must replay exactly");
}

#[test]
fn restored_ledger_rejects_duplicate_global_attempts_and_wrong_prior_chain() {
    let (ledger, replacement_context_id) = two_context_mutation_ledger();
    let restored: MutationLedger = serde_json::from_value(serde_json::to_value(&ledger).unwrap())
        .expect("strict ledger roundtrip");
    assert_eq!(restored, ledger);

    let mut duplicate_index = restored.clone();
    let replacement = duplicate_index
        .contexts
        .get_mut(&replacement_context_id)
        .expect("replacement context history");
    let mut second_attempt = replacement.attempts.remove(&2).expect("second attempt");
    second_attempt.policy.attempt_index = 1;
    second_attempt.policy.prior_attempt_id = None;
    replacement.attempts.insert(1, second_attempt);
    assert_eq!(
        duplicate_index
            .validate()
            .expect_err("global attempt indexes cannot repeat across context histories")
            .code(),
        "mutation_global_attempt_index_duplicate"
    );

    let mut wrong_prior = restored;
    wrong_prior
        .contexts
        .get_mut(&replacement_context_id)
        .unwrap()
        .attempts
        .get_mut(&2)
        .unwrap()
        .policy
        .prior_attempt_id = Some(MutationAttemptId::new("mutation-attempt:wrong-prior"));
    assert_eq!(
        wrong_prior
            .validate()
            .expect_err("attempt N must reference the exact attempt N-1 identity")
            .code(),
        "mutation_global_attempt_sequence_invalid"
    );
}

#[test]
fn aggregate_rejects_post_candidate_failure_without_the_recorded_candidate_identity() {
    let mut pre_candidate = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let DomainEvent::Mutation(MutationEvent::FeasibilityEvaluated { .. }) = append_next_emitted(
        &mut pre_candidate.phase4.state,
        "phase5:aggregate-failure-binding:pre-candidate-feasibility",
    ) else {
        panic!("mutation feasibility must be recorded before policy selection");
    };
    let DomainEvent::Mutation(MutationEvent::AttemptPolicySelected { policy }) =
        append_next_emitted(
            &mut pre_candidate.phase4.state,
            "phase5:aggregate-failure-binding:pre-candidate-policy",
        )
    else {
        panic!("mutation policy must follow feasibility");
    };
    let pre_candidate_failure = MutationFailure::new(
        &policy,
        None,
        None,
        MutationFailureClass::ProviderProtocol,
        MutationFailureDetailCode::ProviderProtocolViolation,
        None,
    )
    .expect("typed provider-protocol failure");
    let pre_candidate_event = envelope(
        &pre_candidate.phase4.state,
        "phase5:aggregate-failure-binding:pre-candidate-attempt-failed",
        MutationEvent::AttemptFailed {
            failure: pre_candidate_failure,
        },
    );
    let before_pre_candidate = pre_candidate.phase4.state.clone();
    assert!(matches!(
        pre_candidate.phase4.state.append_event(pre_candidate_event),
        Err(ProtocolViolation::MutationContract {
            code: "mutation_attempt_failure_without_candidate"
        })
    ));
    assert_eq!(pre_candidate.phase4.state, before_pre_candidate);

    let mut seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let prepared = prepare_aggregate_mutation(&mut seed, "aggregate-failure-binding");
    dispatch_and_consume_aggregate_mutation(
        &mut seed.phase4.state,
        &prepared,
        "aggregate-failure-binding",
        75,
        45,
    );
    let MutationCandidateObservation::Accepted { candidate } =
        record_mutation_candidate(&prepared, &seed.target, &canonical_invocation(&seed)).unwrap()
    else {
        panic!("canonical mutation candidate");
    };
    append(
        &mut seed.phase4.state,
        "phase5:aggregate-failure-binding:candidate",
        MutationEvent::CandidateRecorded {
            candidate: candidate.clone(),
        },
    );

    let verification_too_early = MutationFailure::new(
        &prepared.policy,
        Some(candidate.strategy),
        Some(candidate.candidate_id.clone()),
        MutationFailureClass::VerificationMismatch,
        MutationFailureDetailCode::VerificationMismatch,
        None,
    )
    .expect("candidate-bound verification failure shape");
    let verification_too_early_event = envelope(
        &seed.phase4.state,
        "phase5:aggregate-failure-binding:verification-before-application",
        MutationEvent::AttemptFailed {
            failure: verification_too_early,
        },
    );
    let before_verification_too_early = seed.phase4.state.clone();
    assert!(matches!(
        seed.phase4.state.append_event(verification_too_early_event),
        Err(ProtocolViolation::MutationContract {
            code: "mutation_attempt_failure_stage_mismatch"
        })
    ));
    assert_eq!(seed.phase4.state, before_verification_too_early);

    let mut after_application = seed.phase4.state.clone();
    let ProtocolDecision::Perform {
        effect: EffectRequest::Mutation(MutationEffectRequest::ApplyMutation { request: apply }),
    } = decide(&after_application).expect("candidate requests apply")
    else {
        panic!("candidate must produce an apply request");
    };
    append(
        &mut after_application,
        "phase5:aggregate-failure-binding:application",
        MutationEvent::ApplicationObserved {
            request: (*apply).clone(),
            observation: MutationApplicationObservation::new(
                &apply,
                MutationApplicationStatus::Applied,
            ),
        },
    );
    let apply_failure_too_late = MutationFailure::new(
        &prepared.policy,
        Some(candidate.strategy),
        Some(candidate.candidate_id.clone()),
        MutationFailureClass::ApplyRejected,
        MutationFailureDetailCode::ApplyRejected,
        None,
    )
    .expect("candidate-bound apply failure shape");
    let apply_failure_too_late_event = envelope(
        &after_application,
        "phase5:aggregate-failure-binding:apply-failure-after-application",
        MutationEvent::AttemptFailed {
            failure: apply_failure_too_late,
        },
    );
    let before_apply_failure_too_late = after_application.clone();
    assert!(matches!(
        after_application.append_event(apply_failure_too_late_event),
        Err(ProtocolViolation::MutationContract {
            code: "mutation_attempt_failure_stage_mismatch"
        })
    ));
    assert_eq!(after_application, before_apply_failure_too_late);

    let valid = MutationFailure::new(
        &prepared.policy,
        Some(candidate.strategy),
        Some(candidate.candidate_id.clone()),
        MutationFailureClass::ApplyRejected,
        MutationFailureDetailCode::ApplyRejected,
        None,
    )
    .expect("candidate-bound apply failure");
    let mut missing_candidate = valid;
    missing_candidate.candidate_id = None;
    let missing_event = envelope(
        &seed.phase4.state,
        "phase5:aggregate-failure-binding:missing-candidate",
        MutationEvent::AttemptFailed {
            failure: missing_candidate,
        },
    );
    let before_missing = seed.phase4.state.clone();
    assert!(matches!(
        seed.phase4.state.append_event(missing_event),
        Err(ProtocolViolation::MutationContract { .. })
    ));
    assert_eq!(seed.phase4.state, before_missing);

    let wrong_candidate = MutationFailure::new(
        &prepared.policy,
        Some(candidate.strategy),
        Some(MutationCandidateId::new("mutation-candidate:wrong")),
        MutationFailureClass::ApplyRejected,
        MutationFailureDetailCode::ApplyRejected,
        None,
    )
    .expect("well-formed but unrecorded candidate identity");
    let wrong_event = envelope(
        &seed.phase4.state,
        "phase5:aggregate-failure-binding:wrong-candidate",
        MutationEvent::AttemptFailed {
            failure: wrong_candidate,
        },
    );
    let before_wrong = seed.phase4.state.clone();
    assert!(matches!(
        seed.phase4.state.append_event(wrong_event),
        Err(ProtocolViolation::MutationContract {
            code: "mutation_failure_candidate_missing"
        })
    ));
    assert_eq!(seed.phase4.state, before_wrong);

    let wrong_strategy = MutationFailure::new(
        &prepared.policy,
        Some(MutationStrategy::ReplaceFile),
        Some(candidate.candidate_id.clone()),
        MutationFailureClass::ApplyRejected,
        MutationFailureDetailCode::ApplyRejected,
        None,
    )
    .expect("well-formed strategy permitted by the attempt policy");
    let wrong_strategy_event = envelope(
        &seed.phase4.state,
        "phase5:aggregate-failure-binding:wrong-strategy",
        MutationEvent::AttemptFailed {
            failure: wrong_strategy,
        },
    );
    let before_wrong_strategy = seed.phase4.state.clone();
    assert!(matches!(
        seed.phase4.state.append_event(wrong_strategy_event),
        Err(ProtocolViolation::MutationContract {
            code: "mutation_failure_candidate_binding_mismatch"
        })
    ));
    assert_eq!(seed.phase4.state, before_wrong_strategy);
}

#[test]
fn no_feasible_initial_strategy_persists_convergence_and_fails_without_usage() {
    let mut phase4 =
        implementation_seed_with_budget(FixtureOperation::ModifySmall, 4_096, |budget| {
            budget.max_output_tokens_per_call = 1
        });
    let request = target_context_request(&phase4.state);
    let materialized = materialized_context(&phase4, &request);
    let prepared_context =
        prepare_target_context(&request, &materialized).expect("bounded target context");
    append(
        &mut phase4.state,
        "phase5:no-feasible:target-context-prepared",
        ImplementationEvent::TargetContextPrepared {
            prepared: Box::new(prepared_context.clone()),
        },
    );
    let node = phase4
        .state
        .node(&phase4.target_node_id)
        .expect("active implementation node")
        .clone();
    let target = phase4.accepted_plan.targets[0].clone();
    let feasibility = evaluate_mutation_feasibility(&node, &target, &prepared_context.manifest)
        .expect("output-limited feasibility remains structured");
    assert!(feasibility.feasible_strategies().is_empty());
    assert!(
        feasibility
            .evaluations
            .iter()
            .all(|evaluation| !evaluation.is_feasible())
    );
    let usage_before = node.usage.clone();
    let calls_before = phase4.state.budgets.model_calls.len();

    assert_eq!(
        append_next_emitted(&mut phase4.state, "phase5:no-feasible:feasibility"),
        MutationEvent::FeasibilityEvaluated {
            feasibility: feasibility.clone(),
        }
        .into()
    );
    let DomainEvent::Mutation(MutationEvent::ReadinessConvergenceEvaluated { convergence }) =
        append_next_emitted(&mut phase4.state, "phase5:no-feasible:convergence")
    else {
        panic!("an empty feasible set must converge before policy or action creation");
    };
    assert_eq!(
        convergence.reason,
        MutationReadinessConvergenceReason::NoFeasibleStrategy
    );
    assert_eq!(convergence.attempt_id, None);
    assert_eq!(convergence.attempt_index, None);
    let target_state = phase4
        .state
        .mutation
        .current_target(&node.id)
        .expect("persisted no-feasible convergence");
    assert_eq!(
        target_state.readiness_convergence.as_ref(),
        Some(&convergence)
    );
    assert!(target_state.attempts.is_empty());
    assert_eq!(phase4.state.budgets.model_calls.len(), calls_before);
    assert_eq!(phase4.state.node(&node.id).unwrap().usage, usage_before);

    assert_eq!(
        append_next_emitted(&mut phase4.state, "phase5:no-feasible:node-failed"),
        GraphEvent::NodeFailed {
            node_id: node.id.clone(),
            failure_revision_id: convergence.failure_revision_id.clone(),
            terminal: true,
        }
        .into()
    );
    let ProtocolDecision::Finish { result } =
        decide(&phase4.state).expect("no-feasible convergence has a canonical result")
    else {
        panic!("failed implementation node must terminate instead of waiting");
    };
    assert_eq!(result.reason_code, "mutation_no_feasible_strategy");
    assert_eq!(result.process_health, ProcessHealth::Healthy);
    assert_eq!(result.mission.outcome(), MissionOutcomeV1::BlockedNoDiff);
    let blocker = result
        .mission
        .first_fatal_blocker()
        .expect("no-feasible result retains its blocker");
    assert_eq!(blocker.category, "mutation");
    assert_eq!(blocker.code, result.reason_code);
    assert_eq!(blocker.node_id.as_ref(), Some(&node.id));
    append(
        &mut phase4.state,
        "phase5:no-feasible:terminal",
        TerminalEvent::CanonicalResultRecorded {
            result: result.clone(),
        },
    );
    assert_eq!(
        decide(&phase4.state).unwrap(),
        ProtocolDecision::Finish { result }
    );
}

#[test]
fn each_admission_budget_dimension_converges_before_retry_action_or_reservation() {
    struct Case {
        label: &'static str,
        max_model_calls: u32,
        max_cost_micros: u64,
        max_duration_ms: u64,
        actual_cost_micros: u64,
        actual_duration_ms: u64,
        expected_remaining: MutationAdmissionBudgetRemaining,
        exhausted_dimension: MutationAdmissionBudgetDimension,
    }

    let cases = [
        Case {
            label: "model-calls",
            max_model_calls: 1,
            max_cost_micros: 10_000,
            max_duration_ms: 10_000,
            actual_cost_micros: 80,
            actual_duration_ms: 50,
            expected_remaining: MutationAdmissionBudgetRemaining::new(0, 9_920, 9_950),
            exhausted_dimension: MutationAdmissionBudgetDimension::ModelCalls,
        },
        Case {
            label: "cost",
            max_model_calls: 3,
            max_cost_micros: 80,
            max_duration_ms: 10_000,
            actual_cost_micros: 80,
            actual_duration_ms: 50,
            expected_remaining: MutationAdmissionBudgetRemaining::new(2, 0, 9_950),
            exhausted_dimension: MutationAdmissionBudgetDimension::CostMicros,
        },
        Case {
            label: "duration",
            max_model_calls: 3,
            max_cost_micros: 10_000,
            max_duration_ms: 50,
            actual_cost_micros: 80,
            actual_duration_ms: 50,
            expected_remaining: MutationAdmissionBudgetRemaining::new(2, 9_920, 0),
            exhausted_dimension: MutationAdmissionBudgetDimension::DurationMs,
        },
    ];

    for case in cases {
        let mut seed = mutation_seed_with_budget(FixtureOperation::ModifySmall, 4_096, |budget| {
            budget.max_model_calls = case.max_model_calls;
            budget.max_cost_micros = case.max_cost_micros;
            budget.max_duration_ms = case.max_duration_ms;
        });
        let calls_before = seed.phase4.state.budgets.model_calls.len();
        let initial =
            prepare_aggregate_mutation(&mut seed, &format!("budget-{}-initial", case.label));
        dispatch_and_consume_aggregate_mutation(
            &mut seed.phase4.state,
            &initial,
            &format!("budget-{}-initial", case.label),
            case.actual_cost_micros,
            case.actual_duration_ms,
        );
        let retry_policy = reject_consumed_action_for_model_retry(
            &mut seed.phase4.state,
            &initial,
            &format!("budget-{}", case.label),
        );
        assert_eq!(retry_policy.attempt_index, 2);

        if case.exhausted_dimension == MutationAdmissionBudgetDimension::ModelCalls {
            let current_node = seed
                .phase4
                .state
                .node(&seed.node.id)
                .expect("current exhausted implementation node")
                .clone();
            let forged = build_prepared_mutation_action_retry(
                &current_node,
                &seed.target,
                &seed.context,
                &seed.feasibility,
                retry_policy.clone(),
                1,
                None,
                case.expected_remaining.cost_micros,
                case.expected_remaining.duration_ms,
            )
            .expect("cost and duration alone still permit a structurally valid action");
            let forged_event = envelope(
                &seed.phase4.state,
                "phase5:budget-model-calls:forged-action",
                MutationEvent::ActionPrepared {
                    prepared: Box::new(forged),
                },
            );
            let before_forged = seed.phase4.state.clone();
            assert!(matches!(
                seed.phase4.state.append_event(forged_event),
                Err(ProtocolViolation::MutationContract {
                    code: "mutation_action_after_admission_budget_exhaustion"
                })
            ));
            assert_eq!(seed.phase4.state, before_forged);
        }

        let DomainEvent::Mutation(MutationEvent::ReadinessConvergenceEvaluated { convergence }) =
            append_next_emitted(
                &mut seed.phase4.state,
                &format!("phase5:budget-{}:convergence", case.label),
            )
        else {
            panic!("{} exhaustion must converge before an action", case.label);
        };
        let MutationReadinessConvergenceReason::AdmissionBudgetExhausted {
            remaining,
            exhausted_dimensions,
        } = &convergence.reason
        else {
            panic!("{} must retain typed budget exhaustion", case.label);
        };
        assert_eq!(*remaining, case.expected_remaining);
        assert_eq!(
            exhausted_dimensions,
            &BTreeSet::from([case.exhausted_dimension])
        );
        assert_eq!(
            convergence.attempt_id.as_ref(),
            Some(&retry_policy.attempt_id)
        );
        let target_state = seed
            .phase4
            .state
            .mutation
            .current_target(&seed.node.id)
            .unwrap();
        let retry_attempt = &target_state.attempts[&retry_policy.attempt_index];
        assert!(retry_attempt.actions.is_empty());
        assert!(retry_attempt.prepared_action.is_none());
        assert_eq!(
            seed.phase4.state.budgets.model_calls.len(),
            calls_before + 1
        );
        let usage = &seed.phase4.state.node(&seed.node.id).unwrap().usage;
        assert_eq!(usage.model_calls_reserved, 0);
        assert_eq!(usage.cost_micros_reserved, 0);
        assert_eq!(usage.duration_ms_reserved, 0);
        assert!(matches!(
            decide(&seed.phase4.state).unwrap(),
            ProtocolDecision::Emit {
                event: DomainEvent::Graph(GraphEvent::NodeFailed {
                    node_id,
                    failure_revision_id,
                    terminal: true,
                })
            } if node_id == seed.node.id
                && failure_revision_id == convergence.failure_revision_id
        ));
    }
}

#[test]
fn released_uncontacted_action_retries_with_distinct_identity_under_one_policy() {
    let mut seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let usage_before = seed.phase4.state.node(&seed.node.id).unwrap().usage.clone();
    let initial = prepare_aggregate_mutation(&mut seed, "released-chain-initial");
    assert_eq!(initial.action_index, 1);
    assert_eq!(initial.prior_released_action_id, None);
    release_uncontacted_aggregate_action(
        &mut seed.phase4.state,
        &initial,
        "released-chain-initial",
    );

    let DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared: retry }) =
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:released-chain:retry-prepared",
        )
    else {
        panic!("one uncontacted release must prepare the bounded retry action");
    };
    let retry = *retry;
    assert_eq!(retry.policy, initial.policy);
    assert_eq!(retry.action_index, 2);
    assert_eq!(
        retry.prior_released_action_id.as_ref(),
        Some(&initial.provider_request.action_id)
    );
    assert_ne!(
        retry.provider_request.action_id,
        initial.provider_request.action_id
    );
    assert_ne!(
        retry.provider_request.call_id,
        initial.provider_request.call_id
    );
    assert_ne!(
        retry.provider_request.reservation_id,
        initial.provider_request.reservation_id
    );
    assert_eq!(
        retry.provider_request.tool_names(),
        initial.provider_request.tool_names()
    );
    dispatch_and_consume_aggregate_mutation(
        &mut seed.phase4.state,
        &retry,
        "released-chain-retry",
        80,
        50,
    );

    let target = seed
        .phase4
        .state
        .mutation
        .current_target(&seed.node.id)
        .unwrap();
    let attempt = &target.attempts[&initial.policy.attempt_index];
    assert_eq!(attempt.actions.len(), 2);
    assert!(attempt.actions[&1].released_uncontacted);
    assert!(!attempt.actions[&2].released_uncontacted);
    assert_eq!(attempt.prepared_action.as_ref(), Some(&retry));
    let usage = &seed.phase4.state.node(&seed.node.id).unwrap().usage;
    assert_eq!(
        usage.model_calls_consumed,
        usage_before.model_calls_consumed + 1
    );
    assert_eq!(usage.mutation_attempts, usage_before.mutation_attempts + 1);
    assert_eq!(usage.model_calls_reserved, 0);
    assert_eq!(usage.cost_micros_reserved, 0);
    assert_eq!(usage.duration_ms_reserved, 0);
}

#[test]
fn repeated_uncontacted_release_converges_and_terminal_reason_matches() {
    let mut seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let usage_before = seed.phase4.state.node(&seed.node.id).unwrap().usage.clone();
    let calls_before = seed.phase4.state.budgets.model_calls.len();
    let initial = prepare_aggregate_mutation(&mut seed, "release-exhaustion-initial");
    release_uncontacted_aggregate_action(
        &mut seed.phase4.state,
        &initial,
        "release-exhaustion-initial",
    );
    let DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared: retry }) =
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:release-exhaustion:retry-prepared",
        )
    else {
        panic!("the one permitted retry must be prepared");
    };
    let retry = *retry;
    release_uncontacted_aggregate_action(
        &mut seed.phase4.state,
        &retry,
        "release-exhaustion-retry",
    );

    let DomainEvent::Mutation(MutationEvent::ReadinessConvergenceEvaluated { convergence }) =
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:release-exhaustion:convergence",
        )
    else {
        panic!("bounded releases must converge rather than wait forever");
    };
    assert_eq!(
        convergence.reason,
        MutationReadinessConvergenceReason::UncontactedActionRetryExhausted {
            released_actions: MUTATION_UNCONTACTED_RELEASE_RETRY_LIMIT + 1,
            maximum_actions: MUTATION_UNCONTACTED_RELEASE_RETRY_LIMIT + 1,
            last_released_action_id: retry.provider_request.action_id.clone(),
        }
    );
    assert_eq!(
        seed.phase4.state.budgets.model_calls.len(),
        calls_before + 2
    );
    assert!(
        seed.phase4
            .state
            .budgets
            .model_calls
            .values()
            .filter(|record| record.admission.node_id == seed.node.id)
            .all(|record| record.state == ModelCallState::ReconciledReleased)
    );
    assert_eq!(
        seed.phase4.state.node(&seed.node.id).unwrap().usage,
        usage_before
    );

    let wrong_failure = envelope(
        &seed.phase4.state,
        "phase5:release-exhaustion:wrong-node-failure",
        GraphEvent::NodeFailed {
            node_id: seed.node.id.clone(),
            failure_revision_id: FailureRevisionId::new("failure:wrong-release-convergence"),
            terminal: true,
        },
    );
    let before_wrong = seed.phase4.state.clone();
    assert!(matches!(
        seed.phase4.state.append_event(wrong_failure),
        Err(ProtocolViolation::MutationContract {
            code: "implementation_failure_without_exact_mutation_convergence"
        })
    ));
    assert_eq!(seed.phase4.state, before_wrong);

    assert_eq!(
        append_next_emitted(
            &mut seed.phase4.state,
            "phase5:release-exhaustion:node-failed",
        ),
        GraphEvent::NodeFailed {
            node_id: seed.node.id.clone(),
            failure_revision_id: convergence.failure_revision_id.clone(),
            terminal: true,
        }
        .into()
    );
    let ProtocolDecision::Finish { result } =
        decide(&seed.phase4.state).expect("release exhaustion terminal decision")
    else {
        panic!("release exhaustion must not return another wait");
    };
    assert_eq!(
        result.reason_code,
        "mutation_uncontacted_action_retry_exhausted"
    );
    assert_eq!(
        result.process_health,
        ProcessHealth::Failed {
            code: result.reason_code.clone(),
        }
    );
    assert_eq!(
        result.mission.outcome(),
        MissionOutcomeV1::InfrastructureFailed
    );
    let blocker = result.mission.first_fatal_blocker().unwrap();
    assert_eq!(blocker.category, "infrastructure");
    assert_eq!(blocker.code, result.reason_code);
    assert_eq!(blocker.node_id.as_ref(), Some(&seed.node.id));
}

#[test]
fn implementation_node_failure_without_mutation_convergence_is_rejected_atomically() {
    let seed = mutation_seed(FixtureOperation::ModifySmall, 4_096);
    let mut state = seed.phase4.state;
    let before = state.clone();
    let event = envelope(
        &state,
        "phase5:unbound-implementation-failure",
        GraphEvent::NodeFailed {
            node_id: seed.node.id.clone(),
            failure_revision_id: FailureRevisionId::new("failure:unbound-implementation"),
            terminal: true,
        },
    );
    assert!(matches!(
        state.append_event(event),
        Err(ProtocolViolation::MutationContract {
            code: "implementation_failure_without_exact_mutation_convergence"
        })
    ));
    assert_eq!(state, before);
    assert!(matches!(
        state.node(&seed.node.id).map(|node| &node.state),
        Some(NodeState::Active { attempt: 1 })
    ));
}

#[test]
fn terminal_mutation_failure_freezes_other_ready_target_until_exact_result_is_recorded() {
    let (mut phase4, second_node_id) =
        implementation_seed_with_two_ready_targets(4_096, |budget| {
            budget.max_output_tokens_per_call = 1
        });
    let request = target_context_request(&phase4.state);
    let materialized = materialized_context(&phase4, &request);
    let prepared_context =
        prepare_target_context(&request, &materialized).expect("bounded first-target context");
    append(
        &mut phase4.state,
        "phase5:terminal-freeze:target-context-prepared",
        ImplementationEvent::TargetContextPrepared {
            prepared: Box::new(prepared_context.clone()),
        },
    );

    let first_node = phase4
        .state
        .node(&phase4.target_node_id)
        .expect("active first implementation target")
        .clone();
    let first_target = phase4
        .accepted_plan
        .targets
        .iter()
        .find(|target| target.target_id == request.target_id)
        .expect("first accepted target")
        .clone();
    let feasibility =
        evaluate_mutation_feasibility(&first_node, &first_target, &prepared_context.manifest)
            .expect("typed infeasible mutation strategy set");
    assert!(feasibility.feasible_strategies().is_empty());
    assert_eq!(
        append_next_emitted(&mut phase4.state, "phase5:terminal-freeze:feasibility"),
        MutationEvent::FeasibilityEvaluated {
            feasibility: feasibility.clone(),
        }
        .into()
    );
    let DomainEvent::Mutation(MutationEvent::ReadinessConvergenceEvaluated { convergence }) =
        append_next_emitted(&mut phase4.state, "phase5:terminal-freeze:convergence")
    else {
        panic!("the first target must persist exact mutation convergence");
    };
    assert_eq!(
        convergence.reason,
        MutationReadinessConvergenceReason::NoFeasibleStrategy
    );
    assert_eq!(
        append_next_emitted(&mut phase4.state, "phase5:terminal-freeze:node-failed"),
        GraphEvent::NodeFailed {
            node_id: first_node.id.clone(),
            failure_revision_id: convergence.failure_revision_id,
            terminal: true,
        }
        .into()
    );
    assert!(matches!(
        phase4.state.node(&second_node_id).map(|node| &node.state),
        Some(NodeState::Ready)
    ));

    let ProtocolDecision::Finish { result } =
        decide(&phase4.state).expect("terminal mutation failure has one canonical result")
    else {
        panic!("terminal mutation failure must finish before another ready target starts");
    };
    let start_second = envelope(
        &phase4.state,
        "phase5:terminal-freeze:start-second-target",
        GraphEvent::NodeStarted {
            node_id: second_node_id.clone(),
            attempt: 1,
        },
    );
    let before_start = phase4.state.clone();
    assert!(matches!(
        phase4.state.append_event(start_second),
        Err(ProtocolViolation::MutationContract {
            code: "mutation_terminal_progress_frozen"
        })
    ));
    assert_eq!(phase4.state, before_start);

    let mut wrong_result = result.clone();
    wrong_result.reason_code = "mutation_wrong_terminal_result".into();
    let wrong_terminal = envelope(
        &phase4.state,
        "phase5:terminal-freeze:wrong-terminal",
        TerminalEvent::CanonicalResultRecorded {
            result: wrong_result,
        },
    );
    let before_wrong_terminal = phase4.state.clone();
    assert!(matches!(
        phase4.state.append_event(wrong_terminal),
        Err(ProtocolViolation::MutationContract {
            code: "mutation_terminal_progress_frozen"
        })
    ));
    assert_eq!(phase4.state, before_wrong_terminal);

    append(
        &mut phase4.state,
        "phase5:terminal-freeze:canonical-terminal",
        TerminalEvent::CanonicalResultRecorded {
            result: result.clone(),
        },
    );
    assert_eq!(phase4.state.terminal.as_ref(), Some(&result));
    InMemoryEventStore::restore(phase4.trusted_initial, phase4.state)
        .expect("the exact terminal result must replay from the two-target bootstrap");
}
