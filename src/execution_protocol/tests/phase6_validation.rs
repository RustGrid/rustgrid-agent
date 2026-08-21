//! Phase 6 validation and evidence-driven repair contract regressions.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::phase4_implementation_context::materialized_context;
use super::phase5_mutation::{
    CompletedImplementationBarrierSeed, completed_implementation_barrier_seed,
    completed_implementation_barrier_seed_with_phase7_review_budget,
    completed_implementation_barrier_seed_with_validation_commands,
    dispatch_and_consume_aggregate_mutation, durable_artifact, file_state,
    phase6_implementation_target_bytes, release_uncontacted_aggregate_action,
};
use super::*;

const VALIDATION_SECRET_SENTINEL: &str = "phase6-expanded-secret-5c9d2be7";

struct ValidationContractSeed {
    barrier: CompletedImplementationBarrierSeed,
    profile: RepositoryProfile,
    graph: PlanGraphMaterialization,
    policy: ValidationPolicyV1,
    gates: Vec<ValidationGateV1>,
}

#[derive(Clone, Debug)]
pub(super) struct Phase7RepairAncestryIds {
    pub(super) failure_revision_id: FailureRevisionId,
    pub(super) failed_validation_evidence_id: ValidationEvidenceId,
    pub(super) validation_failure_proof_id: ProofId,
    pub(super) repair_intent_id: RepairIntentId,
    pub(super) repair_eligibility_proof_id: ProofId,
    pub(super) repair_mutation_evidence_id: EvidenceId,
    pub(super) repair_mutation_proof_id: ProofId,
    pub(super) repair_verification_proof_id: ProofId,
    pub(super) invalidated_validation_evidence_ids: BTreeSet<ValidationEvidenceId>,
    pub(super) validation_rerun_id: EvidenceId,
    pub(super) validation_rerun_proof_id: ProofId,
}

#[derive(Clone, Debug)]
pub(super) struct Phase7ReviewEntrySeed {
    pub(super) trusted_initial: ExecutionState,
    pub(super) state: ExecutionState,
    pub(super) implementation_barrier_proof_id: ProofId,
    pub(super) required_validation_proof_id: ProofId,
    pub(super) current_validation_pass_proof_ids: Vec<ProofId>,
    pub(super) current_validation_evidence_ids: Vec<EvidenceId>,
    pub(super) repair_ancestry: Option<Phase7RepairAncestryIds>,
    pub(super) review_node_id: NodeId,
    pub(super) completion_node_id: NodeId,
    pub(super) publication_node_id: NodeId,
}

fn required_phase7_node_id(state: &ExecutionState, kind: NodeKind) -> NodeId {
    let nodes = state.required_nodes(kind);
    let [node] = nodes.as_slice() else {
        panic!("Phase 7 review-entry seed requires exactly one {kind:?} node");
    };
    node.id.clone()
}

fn authorization_for_candidate(
    candidate: &ValidationCommandCandidate,
) -> ValidationCommandAuthorization {
    let (gate_class, parser) = match candidate.command {
        ValidationCommandKind::CargoTest => {
            (ValidationGateClass::Focused, ValidationParserKind::Cargo)
        }
        ValidationCommandKind::CargoBuild => {
            (ValidationGateClass::Build, ValidationParserKind::Cargo)
        }
        ValidationCommandKind::NpmTest => {
            (ValidationGateClass::TestSuite, ValidationParserKind::Node)
        }
        ValidationCommandKind::NpmBuild => (ValidationGateClass::Build, ValidationParserKind::Node),
        ValidationCommandKind::NpmTypecheck => {
            (ValidationGateClass::Typecheck, ValidationParserKind::Node)
        }
        ValidationCommandKind::NpmLint => (ValidationGateClass::Lint, ValidationParserKind::Node),
        ValidationCommandKind::PythonPytest => {
            (ValidationGateClass::TestSuite, ValidationParserKind::Pytest)
        }
        ValidationCommandKind::PythonBuild => {
            (ValidationGateClass::Build, ValidationParserKind::Pytest)
        }
        ValidationCommandKind::GoTestAll => {
            (ValidationGateClass::TestSuite, ValidationParserKind::Go)
        }
        ValidationCommandKind::GoBuildAll => (ValidationGateClass::Build, ValidationParserKind::Go),
    };
    ValidationCommandAuthorization {
        candidate_id: candidate.candidate_id.clone(),
        gate_class,
        parser,
        timeout_ms: 30_000,
        output_limit_bytes: 4_096,
        max_runs: 2,
        environment_fingerprint: stable_sha256(&["execution-protocol-v1:phase6-safe-environment"]),
        dependency_fingerprint: stable_sha256(&[
            "execution-protocol-v1:phase6-dependencies",
            candidate.candidate_id.as_str(),
        ]),
    }
}

fn validation_policy(
    barrier: &CompletedImplementationBarrierSeed,
    profile: &RepositoryProfile,
    test_repair_authorizations: Vec<TestRepairAuthorization>,
) -> ValidationPolicyV1 {
    let expected_candidate_ids = barrier
        .phase4
        .accepted_plan
        .targets
        .iter()
        .flat_map(|target| target.expected_validation.iter())
        .map(|expectation| &expectation.command_candidate_id)
        .collect::<BTreeSet<_>>();
    let authorizations = profile
        .validation_candidates
        .iter()
        .filter(|candidate| expected_candidate_ids.contains(&candidate.candidate_id))
        .map(authorization_for_candidate)
        .collect::<Vec<_>>();
    ValidationPolicyV1::new(
        EvidenceId::new("policy-evidence:phase6-validation"),
        profile,
        authorizations,
        BTreeSet::new(),
        barrier
            .phase4
            .state
            .plan_graph_budget
            .implementation
            .clone(),
        1,
        test_repair_authorizations,
    )
    .expect("signed Phase 6 validation policy")
}

fn validation_contract_seed() -> ValidationContractSeed {
    validation_contract_seed_from_barrier(completed_implementation_barrier_seed())
}

fn validation_contract_seed_from_barrier(
    barrier: CompletedImplementationBarrierSeed,
) -> ValidationContractSeed {
    let profile = barrier
        .phase4
        .state
        .repository_profile
        .clone()
        .expect("Phase 6 seed repository profile");
    let graph = materialize_accepted_plan(
        &barrier.phase4.accepted_plan,
        &barrier.phase4.state.plan_graph_budget,
    )
    .expect("Phase 6 seed plan graph");
    let policy = validation_policy(&barrier, &profile, Vec::new());
    let gates = build_validation_gates(
        &barrier.phase4.accepted_plan,
        &graph,
        &profile,
        &policy,
        &barrier.phase4.state.repository_revision,
    )
    .expect("canonical Phase 6 gates");
    ValidationContractSeed {
        barrier,
        profile,
        graph,
        policy,
        gates,
    }
}

fn validation_contract_seed_with_finalization_policy(
    policy_for_plan: impl FnOnce(&AcceptedPlan) -> FinalizationPolicyV1,
) -> ValidationContractSeed {
    let mut seed = validation_contract_seed_from_barrier(
        completed_implementation_barrier_seed_with_phase7_review_budget(),
    );
    let policy = policy_for_plan(&seed.barrier.phase4.accepted_plan);
    assert_eq!(
        policy.publication.base_repository_revision,
        seed.barrier.phase4.accepted_plan.repository_revision,
        "Phase 7 policy must bind the accepted plan's trusted base revision"
    );
    seed.barrier.phase4.trusted_initial.finalization_policy = Some(policy.clone());
    seed.barrier.phase4.state.finalization_policy = Some(policy);
    seed
}

fn validation_contract_seed_with_max_runs(max_runs: u32) -> ValidationContractSeed {
    let mut seed = validation_contract_seed();
    let mut authorizations = seed.policy.authorizations.clone();
    for authorization in &mut authorizations {
        authorization.max_runs = max_runs;
    }
    seed.policy = ValidationPolicyV1::new(
        EvidenceId::new(format!("policy-evidence:phase6-max-runs-{max_runs}")),
        &seed.profile,
        authorizations,
        BTreeSet::new(),
        seed.policy.repair_node_budget.clone(),
        1,
        Vec::new(),
    )
    .unwrap();
    seed.gates = build_validation_gates(
        &seed.barrier.phase4.accepted_plan,
        &seed.graph,
        &seed.profile,
        &seed.policy,
        &seed.barrier.phase4.state.repository_revision,
    )
    .unwrap();
    seed
}

fn validation_contract_seed_with_repair_budget(
    label: &str,
    configure: impl FnOnce(&mut NodeBudgetContract),
) -> ValidationContractSeed {
    let mut seed = validation_contract_seed();
    let mut repair_budget = seed.policy.repair_node_budget.clone();
    configure(&mut repair_budget);
    seed.policy = ValidationPolicyV1::new(
        EvidenceId::new(format!("policy-evidence:phase6-repair-budget-{label}")),
        &seed.profile,
        seed.policy.authorizations.clone(),
        seed.policy.required_broad_candidates.clone(),
        repair_budget,
        seed.policy.max_repair_targets_per_failure,
        seed.policy.test_repair_authorizations.clone(),
    )
    .expect("custom repair budget remains a valid signed validation policy");
    seed.gates = build_validation_gates(
        &seed.barrier.phase4.accepted_plan,
        &seed.graph,
        &seed.profile,
        &seed.policy,
        &seed.barrier.phase4.state.repository_revision,
    )
    .expect("custom repair budget preserves the canonical validation gates");
    seed
}

fn validation_contract_seed_with_two_focused_and_broad_gates(
    first_focused_max_runs: u32,
    second_focused_max_runs: u32,
    broad_max_runs: u32,
) -> ValidationContractSeed {
    let barrier = completed_implementation_barrier_seed_with_validation_commands(BTreeSet::from([
        ValidationCommandKind::CargoTest,
        ValidationCommandKind::CargoBuild,
    ]));
    let profile = barrier
        .phase4
        .state
        .repository_profile
        .clone()
        .expect("multi-gate seed repository profile");
    let graph = materialize_accepted_plan(
        &barrier.phase4.accepted_plan,
        &barrier.phase4.state.plan_graph_budget,
    )
    .expect("multi-gate accepted plan graph");
    let mut broad_candidate_id = None;
    let authorizations = profile
        .validation_candidates
        .iter()
        .filter_map(|candidate| {
            let (class, max_runs, include) = match candidate.command {
                ValidationCommandKind::CargoTest => {
                    (ValidationGateClass::Focused, first_focused_max_runs, true)
                }
                ValidationCommandKind::CargoBuild => {
                    (ValidationGateClass::Focused, second_focused_max_runs, true)
                }
                ValidationCommandKind::NpmTest => {
                    broad_candidate_id = Some(candidate.candidate_id.clone());
                    (ValidationGateClass::TestSuite, broad_max_runs, true)
                }
                ValidationCommandKind::NpmBuild
                | ValidationCommandKind::NpmTypecheck
                | ValidationCommandKind::NpmLint
                | ValidationCommandKind::PythonPytest
                | ValidationCommandKind::PythonBuild
                | ValidationCommandKind::GoTestAll
                | ValidationCommandKind::GoBuildAll => {
                    (ValidationGateClass::TestSuite, broad_max_runs, false)
                }
            };
            include.then(|| {
                let mut authorization = authorization_for_candidate(candidate);
                authorization.gate_class = class;
                authorization.max_runs = max_runs;
                authorization
            })
        })
        .collect::<Vec<_>>();
    let broad_candidate_id = broad_candidate_id.expect("fixture profile provides npm test");
    let policy = ValidationPolicyV1::new(
        EvidenceId::new("policy-evidence:phase6-two-focused-late-broad"),
        &profile,
        authorizations,
        BTreeSet::from([broad_candidate_id]),
        barrier
            .phase4
            .state
            .plan_graph_budget
            .implementation
            .clone(),
        1,
        Vec::new(),
    )
    .expect("multi-gate validation policy");
    let gates = build_validation_gates(
        &barrier.phase4.accepted_plan,
        &graph,
        &profile,
        &policy,
        &barrier.phase4.state.repository_revision,
    )
    .expect("two focused gates and one required broad gate");
    assert_eq!(gates.len(), 3);
    assert_eq!(gates[0].class, ValidationGateClass::Focused);
    assert_eq!(gates[1].class, ValidationGateClass::Focused);
    assert_eq!(gates[2].class, ValidationGateClass::TestSuite);
    assert_eq!(gates[2].node_id, gates[1].node_id);
    ValidationContractSeed {
        barrier,
        profile,
        graph,
        policy,
        gates,
    }
}

fn current_repair_mutation_baselines(seed: &ValidationContractSeed) -> RepairMutationBaselines {
    let state = &seed.barrier.phase4.state;
    let plan = &seed.barrier.phase4.accepted_plan;
    let implementation = state
        .implementation
        .as_ref()
        .expect("Phase 6 seed implementation projection");
    let mut baselines = implementation
        .node_targets
        .iter()
        .filter_map(|(node_id, target_id)| {
            let node = state.node(node_id)?;
            let NodeState::Succeeded { proof_id } = &node.state else {
                return None;
            };
            let proof = state.proofs.get(proof_id)?;
            let evidence = state.mutation.current_target(node_id)?.verified.as_ref()?;
            (proof.kind == ProofKind::MutationVerified
                && proof.related_evidence_ids.contains(&evidence.evidence_id)
                && evidence.node_id == *node_id
                && evidence.target_id == *target_id
                && evidence.validate().is_ok())
            .then(|| {
                let target = plan
                    .targets
                    .iter()
                    .find(|target| &target.target_id == target_id)
                    .expect("implementation baseline target");
                (
                    target_id.clone(),
                    RepairMutationBaseline::from_implementation(plan, target, evidence.clone())
                        .expect("verified implementation-owned repair baseline"),
                )
            })
        })
        .collect::<RepairMutationBaselines>();
    let Some(validation) = &state.validation else {
        return baselines;
    };
    let mut pending = state
        .event_log
        .iter()
        .filter_map(|stored| match &stored.envelope.payload {
            DomainEvent::Mutation(MutationEvent::MutationVerified { evidence })
                if state
                    .node(&evidence.node_id)
                    .is_some_and(|node| node.kind == NodeKind::ValidationRepair) =>
            {
                Some(evidence.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    loop {
        let mut unresolved = Vec::new();
        let mut progressed = false;
        for evidence in pending {
            let Some((failure, selection)) = validation.failures.values().find_map(|failure| {
                validation
                    .selections
                    .get(&failure.failure_revision_id)
                    .filter(|selection| selection.repair_node.id == evidence.node_id)
                    .map(|selection| (failure, selection))
            }) else {
                unresolved.push(evidence);
                continue;
            };
            let Some(prior) = baselines
                .get(&selection.intent.target_id)
                .filter(|baseline| {
                    baseline.evidence().evidence_id
                        == selection.intent.baseline_mutation_evidence_id
                })
                .cloned()
            else {
                unresolved.push(evidence);
                continue;
            };
            let repair_proof = state
                .node(&selection.repair_node.id)
                .and_then(|node| match &node.state {
                    NodeState::Succeeded { proof_id } => state.proofs.get(proof_id),
                    _ => None,
                })
                .filter(|proof| {
                    proof.kind == ProofKind::RepairVerified
                        && proof.node_ids == [selection.repair_node.id.clone()]
                        && proof.related_evidence_ids == [evidence.evidence_id.clone()]
                        && proof.related_proof_ids.iter().any(|proof_id| {
                            state.proofs.get(proof_id).is_some_and(|proof| {
                                proof.kind == ProofKind::MutationVerified
                                    && proof.node_ids == [selection.repair_node.id.clone()]
                                    && proof.related_evidence_ids == [evidence.evidence_id.clone()]
                            })
                        })
                });
            if repair_proof.is_none() {
                unresolved.push(evidence);
                continue;
            }
            let next = RepairMutationBaseline::from_verified_repair(
                plan, failure, selection, &prior, evidence,
            )
            .expect("verified repair-owned target baseline");
            baselines.insert(selection.intent.target_id.clone(), next);
            progressed = true;
        }
        if !progressed {
            break;
        }
        pending = unresolved;
    }
    baselines.retain(|_, baseline| {
        baseline.evidence().repository_revision_after == state.repository_revision
    });
    baselines
}

fn mixed_validation_profile(repository_revision: RepositoryRevisionId) -> RepositoryProfile {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/execution_protocol_v1/profile_discovery/mixed_rust_node/repository");
    let observations = ["Cargo.toml", "package.json", "src/lib.rs", "ui/index.js"]
        .into_iter()
        .map(|path| {
            RepositoryFileObservation::from_bytes(
                path,
                fs::read(root.join(path)).expect("read checked-in mixed repository fixture"),
            )
            .expect("mixed fixture file is a bounded repository observation")
        })
        .collect();
    build_repository_profile(
        &RepositoryInventory::new(repository_revision, observations)
            .expect("mixed fixture inventory"),
    )
    .expect("mixed fixture profile")
}

fn initial_process_request(seed: &ValidationContractSeed) -> ValidationProcessRequest {
    let gate = seed.gates.first().expect("first required validation gate");
    let schedule = ValidationRunSchedule::new(
        seed.barrier.phase4.state.execution_id.clone(),
        seed.barrier.phase4.state.execution_attempt,
        gate,
        1,
        seed.barrier.phase4.state.repository_revision.clone(),
        1,
        ValidationRunKind::Initial,
    )
    .expect("initial validation schedule");
    ValidationProcessRequest::new(schedule, gate, &seed.policy)
        .expect("exact validation process request")
}

fn artifact_receipt(label: &str, bytes: &[u8]) -> ValidationArtifactReceipt {
    let content_hash = hex::encode(Sha256::digest(bytes));
    ValidationArtifactReceipt {
        content_hash: content_hash.clone(),
        artifact_locator_hash: stable_sha256(&[
            "execution-protocol-v1:phase6-output-locator",
            label,
            &content_hash,
        ]),
        persistence_receipt_hash: stable_sha256(&[
            "execution-protocol-v1:phase6-output-receipt",
            label,
            &content_hash,
        ]),
        byte_len: u64::try_from(bytes.len()).expect("bounded test output length"),
    }
}

fn empty_output_stream() -> BoundedOutputStream {
    BoundedOutputStream {
        original_bytes: 0,
        captured_bytes: 0,
        dropped_bytes: 0,
        truncated: false,
        head: None,
        tail: None,
    }
}

fn complete_output(stdout: &[u8]) -> BoundedProcessOutput {
    BoundedProcessOutput {
        stdout: BoundedOutputStream {
            original_bytes: u64::try_from(stdout.len()).unwrap(),
            captured_bytes: u64::try_from(stdout.len()).unwrap(),
            dropped_bytes: 0,
            truncated: false,
            head: (!stdout.is_empty()).then(|| artifact_receipt("complete-stdout", stdout)),
            tail: None,
        },
        stderr: empty_output_stream(),
    }
}

fn failure_diagnostic(
    source_path: ProfilePath,
    implicated_paths: BTreeSet<ProfilePath>,
    relationship_evidence_ids: BTreeSet<EvidenceId>,
) -> ValidationDiagnostic {
    ValidationDiagnostic::new(
        ValidationDiagnosticKind::TestAssertion,
        Some(stable_sha256(&["phase6:test:generic_failure"])),
        Some(ValidationSourceLocation {
            path: source_path,
            line: Some(42),
            column: Some(19),
        }),
        Some(stable_sha256(&["phase6:expected:completed"])),
        Some(stable_sha256(&["phase6:actual:running"])),
        implicated_paths,
        relationship_evidence_ids,
        "test_assertion_failed".into(),
        stable_sha256(&["phase6:safe-summary:test-assertion-failed"]),
        ParserConfidence::Structured,
    )
    .expect("structured generic validation diagnostic")
}

struct FailedValidation {
    request: ValidationProcessRequest,
    started: ValidationProcessStarted,
    completed: ValidationProcessCompleted,
    evidence: ValidationEvidenceV1,
    failure: ValidationFailureRevisionV1,
}

struct ActiveAggregateValidationRun {
    request: ValidationProcessRequest,
    started: ValidationProcessStarted,
    scheduled_event: ProtocolEventEnvelope,
}

struct ActiveAggregateRepair {
    seed: ValidationContractSeed,
    selection: RepairTargetSelection,
}

fn failed_validation(seed: &ValidationContractSeed) -> FailedValidation {
    let request = initial_process_request(seed);
    let started =
        ValidationProcessStarted::new(&request, stable_sha256(&["phase6:process-handle:failed"]))
            .expect("failed validation process start");
    let completed = ValidationProcessCompleted::new(
        &request,
        Some(&started),
        250,
        ValidationProcessResult::Exited { exit_code: 1 },
        complete_output(b"generic test failure; see structured parser artifacts"),
    )
    .expect("non-zero validation completion");
    let source_path = seed.barrier.phase4.accepted_plan.targets[0].path.clone();
    let diagnostic = failure_diagnostic(
        ProfilePath::new("tests/generic_validation.rs").unwrap(),
        BTreeSet::from([
            source_path,
            ProfilePath::new("tests/generic_validation.rs").unwrap(),
        ]),
        BTreeSet::from([EvidenceId::new("evidence:phase6-source-test-relation")]),
    );
    let evidence = ValidationEvidenceV1::from_completed(
        &request,
        &started,
        &completed,
        ParserConfidence::Structured,
        GateSemanticsObservation::ExpectedSemanticsObserved,
        vec![diagnostic],
    )
    .expect("non-zero exit becomes validation-domain evidence");
    let failure = ValidationFailureRevisionV1::from_evidence(&evidence)
        .expect("failure revision from failed evidence");
    FailedValidation {
        request,
        started,
        completed,
        evidence,
        failure,
    }
}

fn append_next_authoritative(state: &mut ExecutionState, semantic_key: &str) -> DomainEvent {
    let ProtocolDecision::Emit { event } = decide(state).expect("authoritative Phase 6 decision")
    else {
        panic!("expected authoritative emitted event for {semantic_key}");
    };
    append(state, semantic_key, event.clone());
    event
}

fn enter_aggregate_validation(mut seed: ValidationContractSeed) -> ValidationContractSeed {
    seed.barrier.phase4.trusted_initial.validation_policy = Some(seed.policy.clone());
    seed.barrier.phase4.state.validation_policy = Some(seed.policy.clone());
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:aggregate:enter-validation",
        ),
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Implementation,
            to: ProtocolStage::Validation,
            proof_id: seed.barrier.barrier_proof_id.clone(),
        }
        .into()
    );
    assert_eq!(
        seed.barrier.phase4.state.position,
        ProtocolPosition::Validation(ValidationStep::ScheduleGate)
    );
    let validation = seed
        .barrier
        .phase4
        .state
        .validation
        .as_ref()
        .expect("implementation barrier initializes validation state");
    assert_eq!(validation.policy_id, seed.policy.policy_id);
    assert_eq!(
        validation.gate_order,
        seed.gates
            .iter()
            .map(|gate| gate.gate_id.clone())
            .collect::<Vec<_>>()
    );
    seed
}

fn start_aggregate_validation(
    seed: &mut ValidationContractSeed,
    label: &str,
) -> ActiveAggregateValidationRun {
    let validation_node_id = seed
        .barrier
        .phase4
        .state
        .validation
        .as_ref()
        .and_then(ValidationState::next_gate)
        .expect("next canonical validation gate")
        .node_id
        .clone();
    let node_attempt = seed
        .barrier
        .phase4
        .state
        .node(&validation_node_id)
        .expect("canonical validation node")
        .attempts_started
        .saturating_add(1);
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            &format!("phase6:{label}:node-started"),
        ),
        GraphEvent::NodeStarted {
            node_id: validation_node_id,
            attempt: node_attempt,
        }
        .into()
    );
    start_gate_on_active_validation_node(seed, label)
}

fn start_gate_on_active_validation_node(
    seed: &mut ValidationContractSeed,
    label: &str,
) -> ActiveAggregateValidationRun {
    let ProtocolDecision::Emit {
        event: DomainEvent::Validation(ValidationEvent::ValidationScheduled { request }),
    } = decide(&seed.barrier.phase4.state)
        .expect("active validation node schedules its exact gate")
    else {
        panic!("expected canonical validation schedule");
    };
    let scheduled_event = append(
        &mut seed.barrier.phase4.state,
        &format!("phase6:{label}:scheduled"),
        ValidationEvent::ValidationScheduled {
            request: request.clone(),
        },
    );
    assert_eq!(
        decide(&seed.barrier.phase4.state).expect("scheduled validation dispatches"),
        ProtocolDecision::Perform {
            effect: EffectRequest::Validation(ValidationEffectRequest::RunProcess {
                request: Box::new(request.clone()),
            }),
        }
    );
    let started = ValidationProcessStarted::new(
        &request,
        stable_sha256(&["execution-protocol-v1:phase6-aggregate-process", label]),
    )
    .expect("typed validation process start");
    append(
        &mut seed.barrier.phase4.state,
        &format!("phase6:{label}:process-started"),
        ValidationEvent::ValidationProcessStarted {
            started: started.clone(),
        },
    );
    assert_eq!(
        seed.barrier.phase4.state.position,
        ProtocolPosition::Validation(ValidationStep::Running)
    );
    assert_eq!(
        decide(&seed.barrier.phase4.state).unwrap(),
        ProtocolDecision::Wait {
            reason: WaitReason::ValidationProcessObservation {
                run_id: request.schedule.run_id.clone(),
                process_id: Some(started.process_id.clone()),
            },
        }
    );
    ActiveAggregateValidationRun {
        request,
        started,
        scheduled_event,
    }
}

fn complete_aggregate_validation_run(
    seed: &mut ValidationContractSeed,
    run: &ActiveAggregateValidationRun,
    label: &str,
    exit_code: i32,
    diagnostics: Vec<ValidationDiagnostic>,
) -> ValidationEvidenceV1 {
    let completed = ValidationProcessCompleted::new(
        &run.request,
        Some(&run.started),
        125,
        ValidationProcessResult::Exited { exit_code },
        complete_output(if exit_code == 0 {
            b"validation passed"
        } else {
            b"generic validation assertion failed"
        }),
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        &format!("phase6:{label}:completed"),
        ValidationEvent::ValidationProcessCompleted {
            completed: completed.clone(),
        },
    );
    let evidence = ValidationEvidenceV1::from_completed(
        &run.request,
        &run.started,
        &completed,
        if exit_code == 0 {
            ParserConfidence::Exact
        } else {
            ParserConfidence::Structured
        },
        GateSemanticsObservation::ExpectedSemanticsObserved,
        diagnostics,
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        &format!("phase6:{label}:evidence"),
        ValidationEvent::ValidationEvidenceRecorded {
            evidence: evidence.clone(),
        },
    );
    evidence
}

fn record_passing_gate(
    state: &mut ValidationState,
    gate: &ValidationGateV1,
    policy: &ValidationPolicyV1,
    run_label: &str,
) {
    let schedule = ValidationRunSchedule::new(
        ExecutionId::new("execution-protocol-v1:phase6-gate-order"),
        1,
        gate,
        1,
        state.repository_revision.clone(),
        1,
        ValidationRunKind::Initial,
    )
    .unwrap();
    let request = ValidationProcessRequest::new(schedule, gate, policy).unwrap();
    let started = ValidationProcessStarted::new(
        &request,
        stable_sha256(&["execution-protocol-v1:phase6-gate-order", run_label]),
    )
    .unwrap();
    let completed = ValidationProcessCompleted::new(
        &request,
        Some(&started),
        100,
        ValidationProcessResult::Exited { exit_code: 0 },
        complete_output(b"validation passed"),
    )
    .unwrap();
    let evidence = ValidationEvidenceV1::from_completed(
        &request,
        &started,
        &completed,
        ParserConfidence::Exact,
        GateSemanticsObservation::ExpectedSemanticsObserved,
        Vec::new(),
    )
    .unwrap();
    for event in [
        ValidationEvent::ValidationScheduled { request },
        ValidationEvent::ValidationProcessStarted { started },
        ValidationEvent::ValidationProcessCompleted { completed },
        ValidationEvent::ValidationEvidenceRecorded { evidence },
    ] {
        state.apply(&event, policy).unwrap();
    }
}

fn active_aggregate_repair_from_seed(
    seed: ValidationContractSeed,
    label: &str,
) -> ActiveAggregateRepair {
    let mut seed = enter_aggregate_validation(seed);
    let run = start_aggregate_validation(&mut seed, label);
    let completed = ValidationProcessCompleted::new(
        &run.request,
        Some(&run.started),
        200,
        ValidationProcessResult::Exited { exit_code: 1 },
        complete_output(b"generic repairable source assertion"),
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        &format!("phase6:{label}:completed"),
        ValidationEvent::ValidationProcessCompleted {
            completed: completed.clone(),
        },
    );
    let evidence = ValidationEvidenceV1::from_completed(
        &run.request,
        &run.started,
        &completed,
        ParserConfidence::Structured,
        GateSemanticsObservation::ExpectedSemanticsObserved,
        vec![failure_diagnostic(
            seed.barrier.phase4.accepted_plan.targets[0].path.clone(),
            BTreeSet::from([seed.barrier.phase4.accepted_plan.targets[0].path.clone()]),
            BTreeSet::new(),
        )],
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        &format!("phase6:{label}:evidence"),
        ValidationEvent::ValidationEvidenceRecorded { evidence },
    );
    activate_current_validation_failure(seed, label)
}

fn activate_current_validation_failure(
    mut seed: ValidationContractSeed,
    label: &str,
) -> ActiveAggregateRepair {
    for suffix in ["failure", "node-failed", "proof", "repair-transition"] {
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            &format!("phase6:{label}:{suffix}"),
        );
    }
    for suffix in ["ranking", "eligibility"] {
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            &format!("phase6:{label}:{suffix}"),
        );
    }
    let DomainEvent::Validation(ValidationEvent::RepairTargetSelected { selection }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            &format!("phase6:{label}:selection"),
        )
    else {
        panic!("repairable validation failure selects one target");
    };
    for suffix in ["eligibility-proof", "node-added", "node-started"] {
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            &format!("phase6:{label}:{suffix}"),
        );
    }
    assert_eq!(
        seed.barrier.phase4.state.active_node().map(|node| &node.id),
        Some(&selection.repair_node.id)
    );
    ActiveAggregateRepair { seed, selection }
}

fn active_aggregate_repair(label: &str) -> ActiveAggregateRepair {
    active_aggregate_repair_from_seed(validation_contract_seed(), label)
}

fn rebind_mutation_verification_identity(evidence: &mut MutationVerificationEvidence) {
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
    .unwrap();
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

fn rebind_mutation_baseline_to_target(
    baseline: &MutationVerificationEvidence,
    plan: &AcceptedPlan,
    target: &PlannedTargetV1,
) -> MutationVerificationEvidence {
    let mut rebound = baseline.clone();
    let transition = rebound
        .path_transitions
        .values()
        .next()
        .expect("single-path Phase 6 baseline transition")
        .clone();
    rebound.node_id = implementation_node_id(plan, target);
    rebound.target_id = target.target_id.clone();
    rebound.changed_paths = BTreeSet::from([target.path.clone()]);
    rebound.path_transitions.clear();
    rebound
        .path_transitions
        .insert(target.path.clone(), transition);
    rebind_mutation_verification_identity(&mut rebound);
    rebound
        .validate()
        .expect("rebound test baseline remains valid typed evidence");
    rebound
}

fn repair_eligibility_decision<'a>(
    evaluation: &'a RepairEligibilityEvaluation,
    target_id: &TargetId,
) -> &'a RepairEligibilityDecision {
    evaluation
        .decisions
        .iter()
        .find(|decision| &decision.target_id == target_id)
        .expect("ranked target has one eligibility decision")
}

fn materialized_repair_context(
    seed: &ValidationContractSeed,
    request: &TargetContextLoadRequest,
    baseline: &RepairMutationBaseline,
) -> (MaterializedTargetContext, Vec<u8>) {
    let target_path = match request
        .path_expectations
        .iter()
        .next()
        .expect("one repair target path")
    {
        TargetPathExpectation::Existing { path, .. } | TargetPathExpectation::Absent { path } => {
            path
        }
    };
    let mut current_bytes = phase6_implementation_target_bytes(target_path);
    if matches!(
        baseline.owner(),
        RepairMutationBaselineOwner::ValidationRepair { .. }
    ) {
        current_bytes.extend_from_slice(b"\n// phase6 verified repair candidate\n");
    }
    let current_hash = hex::encode(Sha256::digest(&current_bytes));
    let mut materialized = materialized_context(&seed.barrier.phase4, request);
    for state in &mut materialized.path_states {
        if let LoadedPathState::Existing { path, content } = state {
            let expected = request
                .path_expectations
                .iter()
                .find_map(|expectation| match expectation {
                    TargetPathExpectation::Existing {
                        path: expected_path,
                        expected_content_hash,
                    } if expected_path == path => Some(expected_content_hash),
                    _ => None,
                })
                .expect("repair context existing-path expectation");
            assert_eq!(expected, &current_hash);
            *content = LoadedContextArtifact::new(
                current_hash.clone(),
                ArtifactScope::FullFile,
                current_bytes.clone(),
            )
            .expect("current repaired target artifact");
        }
    }
    materialized.repository_fingerprint = baseline.evidence().repository_fingerprint_after.clone();
    (materialized, current_bytes)
}

struct PreparedAggregateRepairContext {
    failure: ValidationFailureRevisionV1,
    target: PlannedTargetV1,
    context: PreparedTargetContext,
    current_bytes: Vec<u8>,
    feasibility: MutationFeasibilitySet,
}

fn prepare_aggregate_repair_context(
    active: &mut ActiveAggregateRepair,
    label: &str,
) -> PreparedAggregateRepairContext {
    let failure = active
        .seed
        .barrier
        .phase4
        .state
        .validation
        .as_ref()
        .and_then(ValidationState::current_failure)
        .expect("active aggregate repair failure")
        .clone();
    let baseline = current_repair_mutation_baselines(&active.seed)
        .get(&active.selection.intent.target_id)
        .expect("active aggregate repair baseline")
        .clone();
    let target = repair_target_for_selection(
        &active.selection,
        &failure,
        &active.seed.barrier.phase4.accepted_plan,
        &baseline,
    )
    .expect("active aggregate repair target");
    let ProtocolDecision::Perform {
        effect:
            EffectRequest::Validation(ValidationEffectRequest::LoadRepairTargetContext { request }),
    } = decide(&active.seed.barrier.phase4.state).unwrap()
    else {
        panic!("active repair must load its exact target context");
    };
    let (materialized, current_bytes) =
        materialized_repair_context(&active.seed, &request, &baseline);
    let context = prepare_target_context(&request, &materialized).unwrap();
    append(
        &mut active.seed.barrier.phase4.state,
        &format!("phase6:{label}:repair-context-prepared"),
        ValidationEvent::RepairTargetContextPrepared {
            prepared: Box::new(context.clone()),
        },
    );
    let DomainEvent::Mutation(MutationEvent::FeasibilityEvaluated { feasibility }) =
        append_next_authoritative(
            &mut active.seed.barrier.phase4.state,
            &format!("phase6:{label}:repair-feasibility"),
        )
    else {
        panic!("prepared repair context must evaluate mutation feasibility");
    };
    PreparedAggregateRepairContext {
        failure,
        target,
        context,
        current_bytes,
        feasibility,
    }
}

fn prepare_aggregate_repair_action(
    active: &mut ActiveAggregateRepair,
    label: &str,
) -> PreparedMutationAction {
    let DomainEvent::Mutation(MutationEvent::AttemptPolicySelected { .. }) =
        append_next_authoritative(
            &mut active.seed.barrier.phase4.state,
            &format!("phase6:{label}:repair-policy"),
        )
    else {
        panic!("feasible repair must select a mutation policy");
    };
    let DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared }) =
        append_next_authoritative(
            &mut active.seed.barrier.phase4.state,
            &format!("phase6:{label}:repair-action"),
        )
    else {
        panic!("repair mutation policy must prepare a provider action");
    };
    *prepared
}

fn accepted_repair_candidate(
    prepared: &PreparedMutationAction,
    context: &PreparedAggregateRepairContext,
    label: &str,
) -> MutationCandidateRecord {
    let mut expected_after = context.current_bytes.clone();
    expected_after.extend_from_slice(b"\n// phase6 verified repair candidate\n");
    let invocation = MaterializedMutationInvocation {
        action_id: prepared.provider_request.action_id.clone(),
        call_id: prepared.provider_request.call_id.clone(),
        tool_call_count: 1,
        completeness: ProviderOutputCompleteness::Complete,
        arguments: MaterializedMutationArguments::ApplyPatch {
            path: context.target.path.clone(),
            expected_content_hash: hex::encode(Sha256::digest(&context.current_bytes)),
            patch: durable_artifact(
                &format!("phase6-{label}-patch"),
                b"--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+repaired\n".to_vec(),
            ),
            expected_after_content: durable_artifact(
                &format!("phase6-{label}-expected-after"),
                expected_after,
            ),
        },
    };
    let MutationCandidateObservation::Accepted { candidate } =
        record_mutation_candidate(prepared, &context.target, &invocation).unwrap()
    else {
        panic!("canonical repair candidate must be accepted");
    };
    candidate
}

fn finish_repair_mutation_convergence(
    active: &mut ActiveAggregateRepair,
    label: &str,
    failure_revision_id: FailureRevisionId,
) -> CanonicalResult {
    assert_eq!(
        append_next_authoritative(
            &mut active.seed.barrier.phase4.state,
            &format!("phase6:{label}:repair-node-failed"),
        ),
        GraphEvent::NodeFailed {
            node_id: active.selection.repair_node.id.clone(),
            failure_revision_id,
            terminal: true,
        }
        .into()
    );
    let ProtocolDecision::Finish { result } =
        decide(&active.seed.barrier.phase4.state).expect("repair convergence terminal decision")
    else {
        panic!("terminal repair mutation convergence must finish");
    };
    result
}

struct CompletedAggregateRepair {
    seed: ValidationContractSeed,
    failure: ValidationFailureRevisionV1,
    selection: RepairTargetSelection,
    verification: MutationVerificationEvidence,
    invalidation: ValidationInvalidation,
    rerun: ValidationRerunSchedule,
}

fn complete_verified_aggregate_repair(
    mut active: ActiveAggregateRepair,
    label: &str,
) -> CompletedAggregateRepair {
    let repair = prepare_aggregate_repair_context(&mut active, label);
    let action = prepare_aggregate_repair_action(&mut active, label);
    dispatch_and_consume_aggregate_mutation(
        &mut active.seed.barrier.phase4.state,
        &action,
        &format!("phase6-{label}"),
        75,
        45,
    );
    let candidate = accepted_repair_candidate(&action, &repair, label);
    append(
        &mut active.seed.barrier.phase4.state,
        &format!("phase6:{label}:repair-candidate"),
        MutationEvent::CandidateRecorded {
            candidate: candidate.clone(),
        },
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Mutation(MutationEffectRequest::ApplyMutation { request: apply }),
    } = decide(&active.seed.barrier.phase4.state).unwrap()
    else {
        panic!("accepted repair candidate must request repository application");
    };
    let application =
        MutationApplicationObservation::new(&apply, MutationApplicationStatus::Applied);
    append(
        &mut active.seed.barrier.phase4.state,
        &format!("phase6:{label}:repair-applied"),
        MutationEvent::ApplicationObserved {
            request: (*apply).clone(),
            observation: application.clone(),
        },
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Mutation(MutationEffectRequest::VerifyMutation { request: verify }),
    } = decide(&active.seed.barrier.phase4.state).unwrap()
    else {
        panic!("applied repair candidate must request repository verification");
    };
    let mut repaired_bytes = repair.current_bytes.clone();
    repaired_bytes.extend_from_slice(b"\n// phase6 verified repair candidate\n");
    let transitions = BTreeMap::from([(
        repair.target.path.clone(),
        MutationPathTransition {
            before: file_state(
                hex::encode(Sha256::digest(&repair.current_bytes)),
                u64::try_from(repair.current_bytes.len()).unwrap(),
            ),
            after: file_state(
                hex::encode(Sha256::digest(&repaired_bytes)),
                u64::try_from(repaired_bytes.len()).unwrap(),
            ),
        },
    )]);
    let materialized = MaterializedMutationVerification {
        request_id: verify.request_id.clone(),
        repository_revision: repair.context.manifest.repository_revision.clone(),
        repository_fingerprint_before: repair.context.manifest.repository_fingerprint.clone(),
        repository_fingerprint_after: stable_sha256(&[
            "execution-protocol-v1:phase6-completed-repair-repository-after",
            label,
            candidate.candidate_id.as_str(),
        ]),
        changed_paths: transitions.keys().cloned().collect(),
        path_transitions: transitions,
    };
    let verification = verify_mutation_application(
        &verify,
        &apply,
        &application,
        &candidate,
        &repair.target,
        &materialized,
    )
    .unwrap();
    append(
        &mut active.seed.barrier.phase4.state,
        &format!("phase6:{label}:repair-verified"),
        MutationEvent::MutationVerified {
            evidence: verification.clone(),
        },
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: mutation_proof,
    }) = append_next_authoritative(
        &mut active.seed.barrier.phase4.state,
        &format!("phase6:{label}:repair-mutation-proof"),
    )
    else {
        panic!("verified repair mutation must produce its mutation proof");
    };
    assert_eq!(mutation_proof.kind, ProofKind::MutationVerified);
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: repair_proof,
    }) = append_next_authoritative(
        &mut active.seed.barrier.phase4.state,
        &format!("phase6:{label}:repair-proof"),
    )
    else {
        panic!("verified repair mutation must produce its repair proof");
    };
    assert_eq!(repair_proof.kind, ProofKind::RepairVerified);
    assert_eq!(
        append_next_authoritative(
            &mut active.seed.barrier.phase4.state,
            &format!("phase6:{label}:repair-node-succeeded"),
        ),
        GraphEvent::NodeSucceeded {
            node_id: active.selection.repair_node.id.clone(),
            proof_id: repair_proof.id,
        }
        .into()
    );
    let DomainEvent::Validation(ValidationEvent::PriorValidationInvalidated { invalidation }) =
        append_next_authoritative(
            &mut active.seed.barrier.phase4.state,
            &format!("phase6:{label}:prior-validation-invalidated"),
        )
    else {
        panic!("verified repair must invalidate prior validation evidence");
    };
    let DomainEvent::Validation(ValidationEvent::ValidationRerunScheduled { rerun }) =
        append_next_authoritative(
            &mut active.seed.barrier.phase4.state,
            &format!("phase6:{label}:rerun-scheduled"),
        )
    else {
        panic!("verified repair must schedule its exact originating gate");
    };
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof: rerun_proof }) =
        append_next_authoritative(
            &mut active.seed.barrier.phase4.state,
            &format!("phase6:{label}:rerun-proof"),
        )
    else {
        panic!("scheduled repair rerun must produce its handoff proof");
    };
    assert_eq!(rerun_proof.kind, ProofKind::ValidationRerunScheduled);
    assert_eq!(
        append_next_authoritative(
            &mut active.seed.barrier.phase4.state,
            &format!("phase6:{label}:return-to-validation"),
        ),
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Repair,
            to: ProtocolStage::Validation,
            proof_id: rerun_proof.id,
        }
        .into()
    );
    CompletedAggregateRepair {
        seed: active.seed,
        failure: repair.failure,
        selection: active.selection,
        verification,
        invalidation,
        rerun,
    }
}

fn assert_event_rejected_atomically(
    state: &mut ExecutionState,
    semantic_key: &str,
    event: DomainEvent,
    expected_code: &str,
) {
    let event = envelope(state, semantic_key, event);
    let before = state.clone();
    let error = state
        .append_event(event)
        .expect_err("unauthorized Phase 6 event must be rejected");
    assert_eq!(error.code(), expected_code);
    assert_eq!(*state, before);
}

fn assert_execution_replays_exactly(seed: &ValidationContractSeed) {
    let restored = InMemoryEventStore::restore(
        seed.barrier.phase4.trusted_initial.clone(),
        seed.barrier.phase4.state.clone(),
    )
    .expect("Phase 6 aggregate checkpoint restores exactly")
    .into_state();
    assert_eq!(restored, seed.barrier.phase4.state);
}

#[test]
fn barrier_constructs_exact_authorized_gate_and_serialized_process_request() {
    let seed = validation_contract_seed();
    assert_eq!(seed.gates.len(), 1);
    let gate = &seed.gates[0];
    assert_eq!(gate.node_id, seed.barrier.validation_node_id);
    assert_eq!(gate.class, ValidationGateClass::Focused);
    assert_eq!(
        gate.repository_revision,
        seed.barrier.phase4.state.repository_revision
    );
    assert_ne!(
        gate.repository_revision,
        seed.barrier.phase4.accepted_plan.repository_revision
    );
    assert_eq!(gate.command.executable, "cargo");
    assert_eq!(gate.command.args, ["test"]);
    assert_eq!(gate.command.working_directory.as_str(), ".");
    assert!(gate.required);
    assert!(gate.dependencies.is_empty());
    assert_eq!(
        gate.provenance.plan_id,
        seed.barrier.phase4.accepted_plan.plan_id
    );
    assert_eq!(
        seed.graph.validation_nodes[gate
            .provenance
            .expectation_id
            .as_ref()
            .expect("plan-derived gate expectation")],
        gate.node_id
    );

    let request = initial_process_request(&seed);
    let duplicate = initial_process_request(&seed);
    assert_eq!(request, duplicate);
    assert_eq!(
        request.canonical_bytes().unwrap(),
        duplicate.canonical_bytes().unwrap()
    );
    request.validate_against(gate, &seed.policy).unwrap();
    let serialized: Value = serde_json::from_slice(&request.canonical_bytes().unwrap()).unwrap();
    assert_eq!(
        serialized.pointer("/command/executable"),
        Some(&Value::String("cargo".into()))
    );
    assert_eq!(
        serialized.pointer("/command/args/0"),
        Some(&Value::String("test".into()))
    );
    assert_eq!(
        serialized.pointer("/schedule/repository_revision"),
        Some(&Value::String(
            seed.barrier.phase4.state.repository_revision.to_string()
        ))
    );
    assert_eq!(
        serialized.pointer("/parser"),
        Some(&Value::String("cargo".into()))
    );
    assert_eq!(
        serialized.pointer("/timeout_ms"),
        Some(&Value::from(30_000))
    );
    assert_eq!(
        serialized.pointer("/output_limit_bytes"),
        Some(&Value::from(4_096))
    );
    assert!(serialized.pointer("/tools").is_none());
    assert!(serialized.pointer("/shell").is_none());
    assert!(serialized.pointer("/environment").is_none());
    assert!(
        !String::from_utf8(request.canonical_bytes().unwrap())
            .unwrap()
            .contains(VALIDATION_SECRET_SENTINEL)
    );
    assert!(matches!(
        EffectRequest::Validation(ValidationEffectRequest::RunProcess {
            request: Box::new(request.clone())
        }),
        EffectRequest::Validation(ValidationEffectRequest::RunProcess { .. })
    ));

    let mut unknown = serialized;
    unknown
        .as_object_mut()
        .unwrap()
        .insert("hosted_route".into(), Value::Bool(true));
    assert!(serde_json::from_value::<ValidationProcessRequest>(unknown).is_err());
}

#[test]
fn canonical_focused_order_owns_the_required_broad_gate_and_remains_executable() {
    let seed = validation_contract_seed();
    let profile = mixed_validation_profile(seed.barrier.phase4.state.repository_revision.clone());
    let candidate = |kind| {
        profile
            .validation_candidates
            .iter()
            .find(|candidate| candidate.command == kind)
            .expect("mixed fixture validation candidate")
    };
    let cargo_test = candidate(ValidationCommandKind::CargoTest);
    let cargo_build = candidate(ValidationCommandKind::CargoBuild);
    let npm_test = candidate(ValidationCommandKind::NpmTest);
    let build_criterion = DiscoveryCriterionId::new("criterion:phase6-order-build").unwrap();
    let build_expectation = ValidationExpectation::new(
        cargo_build.candidate_id.clone(),
        BTreeSet::from([build_criterion.clone()]),
    )
    .unwrap();
    let test_criterion = DiscoveryCriterionId::new("criterion:phase6-order-test").unwrap();
    let test_expectation = ValidationExpectation::new(
        cargo_test.candidate_id.clone(),
        BTreeSet::from([test_criterion.clone()]),
    )
    .unwrap();
    let mut target = seed.barrier.phase4.accepted_plan.targets[0].clone();
    target.acceptance_criteria = BTreeSet::from([test_criterion, build_criterion]);
    target.expected_validation =
        BTreeSet::from([test_expectation.clone(), build_expectation.clone()]);
    let mut plan = seed.barrier.phase4.accepted_plan.clone();
    plan.targets = vec![target];
    let graph = materialize_accepted_plan(&plan, &seed.barrier.phase4.state.plan_graph_budget)
        .expect("two focused validation nodes");
    let authorizations = profile
        .validation_candidates
        .iter()
        .map(authorization_for_candidate)
        .collect::<Vec<_>>();
    let broad = BTreeSet::from([npm_test.candidate_id.clone()]);
    let mut invalid_authorizations = authorizations.clone();
    invalid_authorizations
        .iter_mut()
        .find(|authorization| authorization.candidate_id == npm_test.candidate_id)
        .unwrap()
        .gate_class = ValidationGateClass::Focused;
    assert_eq!(
        ValidationPolicyV1::new(
            EvidenceId::new("policy-evidence:phase6-invalid-focused-broad"),
            &profile,
            invalid_authorizations,
            broad.clone(),
            seed.policy.repair_node_budget.clone(),
            1,
            Vec::new(),
        )
        .expect_err("a required broad candidate cannot be classified as focused")
        .code(),
        "validation_policy_invalid"
    );
    let policy = ValidationPolicyV1::new(
        EvidenceId::new("policy-evidence:phase6-gate-order"),
        &profile,
        authorizations,
        broad,
        seed.policy.repair_node_budget.clone(),
        1,
        Vec::new(),
    )
    .unwrap();
    let gates = build_validation_gates(
        &plan,
        &graph,
        &profile,
        &policy,
        &seed.barrier.phase4.state.repository_revision,
    )
    .unwrap();
    assert_eq!(gates.len(), 3);
    let focused = gates
        .iter()
        .filter(|gate| gate.class == ValidationGateClass::Focused)
        .collect::<Vec<_>>();
    assert_eq!(focused.len(), 2);
    assert_eq!(
        focused.last().unwrap().provenance.expectation_id.as_ref(),
        Some(&build_expectation.expectation_id),
        "the canonically last focused command owns the broad-gate node"
    );
    let broad_gate = gates.last().unwrap();
    assert_eq!(broad_gate.class, ValidationGateClass::TestSuite);
    assert_eq!(broad_gate.provenance.expectation_id, None);
    assert_eq!(broad_gate.node_id, focused.last().unwrap().node_id);
    assert_eq!(
        broad_gate.dependencies,
        [focused.last().unwrap().gate_id.clone()]
    );

    let mut state = ValidationState::new(gates.clone(), &policy, &plan).unwrap();
    for (index, gate) in focused.into_iter().enumerate() {
        assert_eq!(state.next_gate(), Some(gate));
        record_passing_gate(&mut state, gate, &policy, &format!("focused-{index}"));
    }
    assert_eq!(state.next_gate(), Some(broad_gate));
    record_passing_gate(&mut state, broad_gate, &policy, "required-broad");
    assert_eq!(state.next_gate(), None);
}

#[test]
fn schedule_and_output_contracts_reject_tampering_revision_drift_and_missing_tail() {
    let seed = validation_contract_seed();
    let mut state = ValidationState::new(
        seed.gates.clone(),
        &seed.policy,
        &seed.barrier.phase4.accepted_plan,
    )
    .unwrap();
    let request = initial_process_request(&seed);

    for (label, tampered) in [
        ("command", {
            let mut value = request.clone();
            value
                .command
                .args
                .push("--ignored-forbidden-argument".into());
            value
        }),
        ("parser", {
            let mut value = request.clone();
            value.parser = ValidationParserKind::Node;
            value
        }),
        ("policy", {
            let mut value = request.clone();
            value.policy_id = ValidationPolicyId::new("policy:forged");
            value
        }),
    ] {
        let before = state.clone();
        assert_eq!(
            state
                .apply(
                    &ValidationEvent::ValidationScheduled { request: tampered },
                    &seed.policy,
                )
                .expect_err(label)
                .code(),
            "validation_process_request_invalid"
        );
        assert_eq!(state, before, "{label} rejection must be atomic");
    }

    let gate = &seed.gates[0];
    let drift_schedule = ValidationRunSchedule::new(
        seed.barrier.phase4.state.execution_id.clone(),
        seed.barrier.phase4.state.execution_attempt,
        gate,
        1,
        RepositoryRevisionId::new("repository-revision:phase6-stale-schedule"),
        1,
        ValidationRunKind::Initial,
    )
    .expect("schedule shape can be constructed for aggregate revision validation");
    let drift_request = ValidationProcessRequest::new(drift_schedule, gate, &seed.policy).unwrap();
    let before = state.clone();
    assert_eq!(
        state
            .apply(
                &ValidationEvent::ValidationScheduled {
                    request: drift_request,
                },
                &seed.policy,
            )
            .expect_err("stale revision cannot become the current gate run")
            .code(),
        "validation_schedule_not_next"
    );
    assert_eq!(state, before);

    let head = b"PASS generic_case\n";
    let missing_tail = BoundedOutputStream {
        original_bytes: 10_000,
        captured_bytes: u64::try_from(head.len()).unwrap(),
        dropped_bytes: 10_000 - u64::try_from(head.len()).unwrap(),
        truncated: true,
        head: Some(artifact_receipt("missing-tail-head", head)),
        tail: None,
    };
    assert_eq!(
        missing_tail
            .validate(request.output_limit_bytes)
            .expect_err("truncated output requires a failure-relevant tail receipt")
            .code(),
        "validation_bounded_output_invalid"
    );
}

#[test]
fn pass_failure_and_infrastructure_observations_remain_distinct() {
    let seed = validation_contract_seed();
    let gate = &seed.gates[0];
    let request = initial_process_request(&seed);
    let started =
        ValidationProcessStarted::new(&request, stable_sha256(&["phase6:process-handle:passing"]))
            .unwrap();
    let completed = ValidationProcessCompleted::new(
        &request,
        Some(&started),
        200,
        ValidationProcessResult::Exited { exit_code: 0 },
        complete_output(b"test result: ok"),
    )
    .unwrap();
    let passed = ValidationEvidenceV1::from_completed(
        &request,
        &started,
        &completed,
        ParserConfidence::Exact,
        GateSemanticsObservation::ExpectedSemanticsObserved,
        Vec::new(),
    )
    .expect("zero exit plus observed semantics passes");
    assert_eq!(passed.outcome, ValidationEvidenceOutcome::Passed);

    let mut state = ValidationState::new(
        seed.gates.clone(),
        &seed.policy,
        &seed.barrier.phase4.accepted_plan,
    )
    .unwrap();
    for event in [
        ValidationEvent::ValidationScheduled {
            request: request.clone(),
        },
        ValidationEvent::ValidationProcessStarted {
            started: started.clone(),
        },
        ValidationEvent::ValidationProcessCompleted {
            completed: completed.clone(),
        },
        ValidationEvent::ValidationEvidenceRecorded {
            evidence: passed.clone(),
        },
    ] {
        state.apply(&event, &seed.policy).unwrap();
    }
    assert_eq!(state.next_gate_id(), None);
    assert_eq!(
        state.current_evidence_by_gate[&gate.gate_id],
        passed.evidence_id
    );

    let failed = failed_validation(&seed);
    assert!(matches!(
        failed.evidence.outcome,
        ValidationEvidenceOutcome::DomainFailed { .. }
    ));
    let mut failed_state = ValidationState::new(
        seed.gates.clone(),
        &seed.policy,
        &seed.barrier.phase4.accepted_plan,
    )
    .unwrap();
    for event in [
        ValidationEvent::ValidationScheduled {
            request: failed.request.clone(),
        },
        ValidationEvent::ValidationProcessStarted {
            started: failed.started.clone(),
        },
        ValidationEvent::ValidationProcessCompleted {
            completed: failed.completed.clone(),
        },
        ValidationEvent::ValidationEvidenceRecorded {
            evidence: failed.evidence.clone(),
        },
        ValidationEvent::ValidationFailureRevisionRecorded {
            failure: failed.failure.clone(),
        },
    ] {
        failed_state.apply(&event, &seed.policy).unwrap();
    }
    assert_eq!(failed_state.current_failure(), Some(&failed.failure));
    assert_eq!(failed_state.next_gate_id(), None);

    let spawn = ValidationProcessCompleted::new(
        &request,
        None,
        0,
        ValidationProcessResult::InfrastructureFailure {
            kind: ValidationInfrastructureFailureKind::Spawn,
            safe_code: "validation_spawn_failed".into(),
        },
        BoundedProcessOutput {
            stdout: empty_output_stream(),
            stderr: empty_output_stream(),
        },
    )
    .expect("spawn failure is a typed process result without a process identity");
    assert_eq!(spawn.process_id, None);
    let timeout = ValidationProcessCompleted::new(
        &request,
        Some(&started),
        request.timeout_ms,
        ValidationProcessResult::InfrastructureFailure {
            kind: ValidationInfrastructureFailureKind::Timeout,
            safe_code: "validation_timeout".into(),
        },
        BoundedProcessOutput {
            stdout: empty_output_stream(),
            stderr: empty_output_stream(),
        },
    )
    .expect("post-start timeout is a typed infrastructure result");
    assert_eq!(
        ValidationEvidenceV1::from_completed(
            &request,
            &started,
            &timeout,
            ParserConfidence::Fallback,
            GateSemanticsObservation::ExpectedSemanticsMissing,
            Vec::new(),
        )
        .expect_err("infrastructure failure cannot become domain evidence")
        .code(),
        "infrastructure_result_cannot_create_validation_evidence"
    );
}

#[test]
fn bounded_failed_output_preserves_failure_tail_receipt_without_raw_output() {
    let seed = validation_contract_seed();
    let request = initial_process_request(&seed);
    let started = ValidationProcessStarted::new(
        &request,
        stable_sha256(&["phase6:process-handle:large-failure"]),
    )
    .unwrap();
    let head = b"PASS generic_case_0001\nPASS generic_case_0002\n";
    let tail = format!(
        "FAIL generic_case\nAssertionError: expected completed\n{}\ntests/generic_validation.rs:42:19\n",
        VALIDATION_SECRET_SENTINEL
    );
    let original_bytes = 128_000;
    let captured_bytes = u64::try_from(head.len() + tail.len()).unwrap();
    let output = BoundedProcessOutput {
        stdout: BoundedOutputStream {
            original_bytes,
            captured_bytes,
            dropped_bytes: original_bytes - captured_bytes,
            truncated: true,
            head: Some(artifact_receipt("large-failure-head", head)),
            tail: Some(artifact_receipt("large-failure-tail", tail.as_bytes())),
        },
        stderr: empty_output_stream(),
    };
    output.validate(request.output_limit_bytes).unwrap();
    let completed = ValidationProcessCompleted::new(
        &request,
        Some(&started),
        500,
        ValidationProcessResult::Exited { exit_code: 1 },
        output,
    )
    .unwrap();
    assert!(completed.output.stdout.truncated);
    assert_eq!(
        completed.output.stdout.tail.as_ref().unwrap().content_hash,
        hex::encode(Sha256::digest(tail.as_bytes()))
    );
    assert!(completed.output.stdout.dropped_bytes > 0);
    let serialized = serde_json::to_string(&completed).unwrap();
    assert!(!serialized.contains(VALIDATION_SECRET_SENTINEL));
    assert!(!format!("{completed:?}").contains(VALIDATION_SECRET_SENTINEL));
}

#[test]
fn repair_ranking_rejects_unproven_test_and_selects_evidence_bound_source() {
    let seed = validation_contract_seed();
    let failed = failed_validation(&seed);
    let source = seed.barrier.phase4.accepted_plan.targets[0].clone();
    let mut test = source.clone();
    test.target_id = TargetId::new("target:phase6-generic-stale-test");
    test.change_id = ChangeId::new("change:phase6-generic-stale-test");
    test.path = ProfilePath::new("tests/generic_validation.rs").unwrap();
    test.role = TargetRole::Test;
    let mut unrelated = source.clone();
    unrelated.target_id = TargetId::new("target:phase6-unrelated-source");
    unrelated.change_id = ChangeId::new("change:phase6-unrelated-source");
    unrelated.path = ProfilePath::new("src/unrelated.rs").unwrap();
    let mut repair_plan = seed.barrier.phase4.accepted_plan.clone();
    repair_plan.targets = vec![test.clone(), source.clone(), unrelated.clone()];
    let relationships = &seed
        .barrier
        .phase4
        .state
        .discovery
        .as_ref()
        .expect("Phase 6 discovery evidence")
        .relationships;

    let ranking = rank_repair_candidates(
        &failed.failure,
        &failed.evidence,
        &repair_plan,
        relationships,
    )
    .unwrap();
    let mut reversed = repair_plan.clone();
    reversed.targets.reverse();
    assert_eq!(
        rank_repair_candidates(&failed.failure, &failed.evidence, &reversed, relationships,)
            .unwrap(),
        ranking
    );
    assert_eq!(ranking.candidates.len(), 2);
    assert_eq!(ranking.candidates[0].target_id, test.target_id);
    assert!(
        ranking
            .candidates
            .iter()
            .all(|candidate| candidate.target_id != unrelated.target_id),
        "an untrusted relationship identifier must not score an unrelated target"
    );
    let baselines = current_repair_mutation_baselines(&seed);

    let evaluation = evaluate_repair_eligibility(
        &ranking,
        &failed.failure,
        &failed.evidence,
        &repair_plan,
        &seed.profile,
        &seed.policy,
        &baselines,
    )
    .unwrap();
    let test_decision = evaluation
        .decisions
        .iter()
        .find(|decision| decision.target_id == test.target_id)
        .unwrap();
    assert!(!test_decision.eligible);
    assert_eq!(
        test_decision.reason,
        RepairEligibilityReason::IneligibleTestRequiresSpecification
    );
    let source_decision = evaluation
        .decisions
        .iter()
        .find(|decision| decision.target_id == source.target_id)
        .unwrap();
    assert!(source_decision.eligible);
    assert_eq!(
        source_decision.reason,
        RepairEligibilityReason::EligibleDirectSourceEvidence
    );
    let selection = select_repair_target(
        &ranking,
        &evaluation,
        &failed.failure,
        &seed.gates[0],
        &repair_plan,
        &seed.policy,
        &baselines,
    )
    .unwrap()
    .expect("eligible source repair target");
    assert_eq!(selection.intent.target_id, source.target_id);
    assert_eq!(selection.repair_node.kind, NodeKind::ValidationRepair);
    assert_eq!(selection.repair_node.budget, seed.policy.repair_node_budget);

    let stale_expected_hash = failed.evidence.diagnostics[0]
        .expected_value_hash
        .clone()
        .unwrap();
    let accepted_actual_hash = failed.evidence.diagnostics[0]
        .actual_value_hash
        .clone()
        .unwrap();
    let authorized_policy = validation_policy(
        &seed.barrier,
        &seed.profile,
        vec![TestRepairAuthorization {
            target_id: test.target_id.clone(),
            criterion_ids: test.acceptance_criteria.clone(),
            specification_evidence_ids: test.required_evidence.clone(),
            stale_expected_hash,
            accepted_actual_hash,
        }],
    );
    let authorized = evaluate_repair_eligibility(
        &ranking,
        &failed.failure,
        &failed.evidence,
        &repair_plan,
        &seed.profile,
        &authorized_policy,
        &baselines,
    )
    .unwrap();
    let unverified_test = authorized
        .decisions
        .iter()
        .find(|decision| decision.target_id == test.target_id)
        .unwrap();
    assert!(!unverified_test.eligible);
    assert_eq!(
        unverified_test.reason,
        RepairEligibilityReason::IneligibleMutationBaselineMissing
    );
    let mut authorized_baselines = baselines.clone();
    let source_baseline = baselines
        .get(&source.target_id)
        .expect("verified source implementation baseline");
    let rebound =
        rebind_mutation_baseline_to_target(source_baseline.evidence(), &repair_plan, &test);
    authorized_baselines.insert(
        test.target_id.clone(),
        RepairMutationBaseline::from_implementation(&repair_plan, &test, rebound).unwrap(),
    );
    let authorized = evaluate_repair_eligibility(
        &ranking,
        &failed.failure,
        &failed.evidence,
        &repair_plan,
        &seed.profile,
        &authorized_policy,
        &authorized_baselines,
    )
    .unwrap();
    let verified_test = authorized
        .decisions
        .iter()
        .find(|decision| decision.target_id == test.target_id)
        .unwrap();
    assert!(verified_test.eligible);
    assert_eq!(
        verified_test.reason,
        RepairEligibilityReason::EligibleStaleTestSpecification
    );
    assert_eq!(
        select_repair_target(
            &ranking,
            &authorized,
            &failed.failure,
            &seed.gates[0],
            &repair_plan,
            &authorized_policy,
            &authorized_baselines,
        )
        .unwrap()
        .unwrap()
        .intent
        .target_id,
        test.target_id
    );
}

#[test]
fn repair_eligibility_rejects_missing_stale_absent_and_multi_path_baselines() {
    let seed = validation_contract_seed();
    let failed = failed_validation(&seed);
    let plan = &seed.barrier.phase4.accepted_plan;
    let target = &plan.targets[0];
    let ranking = rank_repair_candidates(
        &failed.failure,
        &failed.evidence,
        plan,
        &seed
            .barrier
            .phase4
            .state
            .discovery
            .as_ref()
            .expect("Phase 6 discovery evidence")
            .relationships,
    )
    .unwrap();
    let current = current_repair_mutation_baselines(&seed);
    let evaluate = |baselines: &RepairMutationBaselines| {
        evaluate_repair_eligibility(
            &ranking,
            &failed.failure,
            &failed.evidence,
            plan,
            &seed.profile,
            &seed.policy,
            baselines,
        )
        .unwrap()
    };
    assert!(repair_eligibility_decision(&evaluate(&current), &target.target_id).eligible);

    let missing = RepairMutationBaselines::new();
    let missing_evaluation = evaluate(&missing);
    assert_eq!(
        repair_eligibility_decision(&missing_evaluation, &target.target_id).reason,
        RepairEligibilityReason::IneligibleMutationBaselineMissing
    );

    let baseline = current
        .get(&target.target_id)
        .expect("verified current implementation baseline");
    let mut stale_baseline = baseline.evidence().clone();
    stale_baseline.repository_fingerprint_after =
        stable_sha256(&["execution-protocol-v1:phase6-stale-baseline"]);
    stale_baseline.repository_revision_after = derive_repository_revision(
        &stale_baseline.repository_revision_before,
        &stale_baseline.repository_fingerprint_after,
        &stale_baseline.candidate_id,
    );
    rebind_mutation_verification_identity(&mut stale_baseline);
    stale_baseline.validate().unwrap();
    let stale = RepairMutationBaselines::from([(
        target.target_id.clone(),
        RepairMutationBaseline::from_implementation(plan, target, stale_baseline).unwrap(),
    )]);
    let stale_evaluation = evaluate(&stale);
    assert_eq!(
        repair_eligibility_decision(&stale_evaluation, &target.target_id).reason,
        RepairEligibilityReason::IneligibleMutationBaselineNotCurrent
    );

    let mut absent_baseline = baseline.evidence().clone();
    absent_baseline
        .path_transitions
        .get_mut(&target.path)
        .expect("target baseline transition")
        .after = MutationPathState::Absent;
    rebind_mutation_verification_identity(&mut absent_baseline);
    absent_baseline.validate().unwrap();
    let absent = RepairMutationBaselines::from([(
        target.target_id.clone(),
        RepairMutationBaseline::from_implementation(plan, target, absent_baseline).unwrap(),
    )]);
    let absent_evaluation = evaluate(&absent);
    assert_eq!(
        repair_eligibility_decision(&absent_evaluation, &target.target_id).reason,
        RepairEligibilityReason::IneligibleUnsupportedMutationBaseline
    );

    let mut multi_path_baseline = baseline.evidence().clone();
    let second_path = ProfilePath::new("src/phase6_unrelated.rs").unwrap();
    let second_transition = multi_path_baseline
        .path_transitions
        .get(&target.path)
        .expect("target baseline transition")
        .clone();
    multi_path_baseline
        .changed_paths
        .insert(second_path.clone());
    multi_path_baseline
        .path_transitions
        .insert(second_path, second_transition);
    rebind_mutation_verification_identity(&mut multi_path_baseline);
    multi_path_baseline.validate().unwrap();
    let multi_path = RepairMutationBaselines::from([(
        target.target_id.clone(),
        RepairMutationBaseline::from_implementation(plan, target, multi_path_baseline).unwrap(),
    )]);
    let multi_path_evaluation = evaluate(&multi_path);
    assert_eq!(
        repair_eligibility_decision(&multi_path_evaluation, &target.target_id).reason,
        RepairEligibilityReason::IneligibleUnsupportedMutationBaseline
    );

    for evaluation in [
        missing_evaluation,
        stale_evaluation,
        absent_evaluation,
        multi_path_evaluation,
    ] {
        assert!(
            select_repair_target(
                &ranking,
                &evaluation,
                &failed.failure,
                &seed.gates[0],
                plan,
                &seed.policy,
                &RepairMutationBaselines::new(),
            )
            .unwrap()
            .is_none()
        );
    }
}

#[test]
fn repair_selection_requires_complete_eligibility_then_schedules_exact_rerun() {
    let seed = validation_contract_seed();
    let failed = failed_validation(&seed);
    let ranking = rank_repair_candidates(
        &failed.failure,
        &failed.evidence,
        &seed.barrier.phase4.accepted_plan,
        &seed
            .barrier
            .phase4
            .state
            .discovery
            .as_ref()
            .expect("Phase 6 discovery evidence")
            .relationships,
    )
    .unwrap();
    let baselines = current_repair_mutation_baselines(&seed);
    let evaluation = evaluate_repair_eligibility(
        &ranking,
        &failed.failure,
        &failed.evidence,
        &seed.barrier.phase4.accepted_plan,
        &seed.profile,
        &seed.policy,
        &baselines,
    )
    .unwrap();
    let selection = select_repair_target(
        &ranking,
        &evaluation,
        &failed.failure,
        &seed.gates[0],
        &seed.barrier.phase4.accepted_plan,
        &seed.policy,
        &baselines,
    )
    .unwrap()
    .expect("current source failure is repairable");
    let mut state = ValidationState::new(
        seed.gates.clone(),
        &seed.policy,
        &seed.barrier.phase4.accepted_plan,
    )
    .unwrap();
    for event in [
        ValidationEvent::ValidationScheduled {
            request: failed.request.clone(),
        },
        ValidationEvent::ValidationProcessStarted {
            started: failed.started.clone(),
        },
        ValidationEvent::ValidationProcessCompleted {
            completed: failed.completed.clone(),
        },
        ValidationEvent::ValidationEvidenceRecorded {
            evidence: failed.evidence.clone(),
        },
        ValidationEvent::ValidationFailureRevisionRecorded {
            failure: failed.failure.clone(),
        },
        ValidationEvent::RepairCandidatesRanked {
            ranking: ranking.clone(),
        },
    ] {
        state.apply(&event, &seed.policy).unwrap();
    }
    assert_eq!(
        state
            .apply(
                &ValidationEvent::RepairTargetSelected {
                    selection: selection.clone(),
                },
                &seed.policy,
            )
            .expect_err("selection cannot precede complete eligibility")
            .code(),
        "repair_selection_not_authorized"
    );
    state
        .apply(
            &ValidationEvent::RepairEligibilityEvaluated {
                evaluation: evaluation.clone(),
            },
            &seed.policy,
        )
        .unwrap();
    state
        .apply(
            &ValidationEvent::RepairTargetSelected {
                selection: selection.clone(),
            },
            &seed.policy,
        )
        .unwrap();

    let next_revision = RepositoryRevisionId::new("repository-revision:phase6-repaired");
    let invalidation = ValidationInvalidation {
        failure_revision_id: failed.failure.failure_revision_id.clone(),
        repair_intent_id: selection.intent.repair_intent_id.clone(),
        repository_revision_before: failed.failure.repository_revision.clone(),
        repository_revision_after: next_revision.clone(),
        invalidated_evidence_ids: BTreeSet::from([failed.evidence.evidence_id.clone()]),
        verified_repair_evidence_id: EvidenceId::new("evidence:phase6-verified-repair"),
    };
    let rerun = ValidationRerunSchedule::new(&invalidation, &selection, &seed.gates[0]).unwrap();
    let before_unbound_rerun = state.clone();
    assert_eq!(
        state
            .apply(
                &ValidationEvent::ValidationRerunScheduled {
                    rerun: rerun.clone(),
                },
                &seed.policy,
            )
            .expect_err("rerun cannot precede its persisted invalidation")
            .code(),
        "validation_rerun_not_authorized"
    );
    assert_eq!(state, before_unbound_rerun);
    state
        .apply(
            &ValidationEvent::PriorValidationInvalidated {
                invalidation: invalidation.clone(),
            },
            &seed.policy,
        )
        .unwrap();
    state
        .apply(
            &ValidationEvent::ValidationRerunScheduled {
                rerun: rerun.clone(),
            },
            &seed.policy,
        )
        .unwrap();
    assert_eq!(state.active_failure, None);
    assert!(
        state
            .invalidated_evidence
            .contains(&failed.evidence.evidence_id)
    );
    assert_eq!(state.next_gate_id(), Some(&seed.gates[0].gate_id));

    let rerun_schedule = ValidationRunSchedule::new(
        seed.barrier.phase4.state.execution_id.clone(),
        seed.barrier.phase4.state.execution_attempt,
        &seed.gates[0],
        2,
        next_revision,
        2,
        ValidationRunKind::ExactRepairRerun {
            failure_revision_id: failed.failure.failure_revision_id,
            repair_intent_id: selection.intent.repair_intent_id,
            verified_repair_evidence_id: invalidation.verified_repair_evidence_id,
        },
    )
    .unwrap();
    let rerun_request =
        ValidationProcessRequest::new(rerun_schedule, &seed.gates[0], &seed.policy).unwrap();
    assert_eq!(rerun_request.command, failed.request.command);
    assert_ne!(
        rerun_request.schedule.run_id,
        failed.request.schedule.run_id
    );
    assert_eq!(rerun_request.schedule.run_attempt, 2);
    assert_eq!(
        ValidationRunSchedule::new(
            seed.barrier.phase4.state.execution_id.clone(),
            seed.barrier.phase4.state.execution_attempt,
            &seed.gates[0],
            3,
            rerun_request.schedule.repository_revision.clone(),
            3,
            rerun_request.schedule.kind.clone(),
        )
        .expect_err("gate run budget forbids a third attempt")
        .code(),
        "validation_run_schedule_invalid"
    );
    state
        .apply(
            &ValidationEvent::ValidationScheduled {
                request: rerun_request.clone(),
            },
            &seed.policy,
        )
        .unwrap();
    let before_duplicate = state.clone();
    assert_eq!(
        state
            .apply(
                &ValidationEvent::ValidationScheduled {
                    request: rerun_request,
                },
                &seed.policy,
            )
            .expect_err("a current rerun cannot be overwritten")
            .code(),
        "validation_schedule_not_next"
    );
    assert_eq!(state, before_duplicate);
    for (event, expected_code) in [
        (
            ValidationEvent::ValidationProcessStarted {
                started: failed.started,
            },
            "validation_process_start_not_current",
        ),
        (
            ValidationEvent::ValidationProcessCompleted {
                completed: failed.completed,
            },
            "validation_process_completion_not_current",
        ),
        (
            ValidationEvent::ValidationEvidenceRecorded {
                evidence: failed.evidence,
            },
            "validation_evidence_run_not_current",
        ),
    ] {
        let before = state.clone();
        assert_eq!(
            state
                .apply(&event, &seed.policy)
                .expect_err("historical run observation is not current")
                .code(),
            expected_code
        );
        assert_eq!(state, before);
    }
}

#[test]
fn current_max_validation_run_cannot_be_overwritten_or_preempted() {
    let mut seed = enter_aggregate_validation(validation_contract_seed_with_max_runs(1));
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:current-run-authority:node-started",
        ),
        GraphEvent::NodeStarted {
            node_id: seed.barrier.validation_node_id.clone(),
            attempt: 1,
        }
        .into()
    );
    let ProtocolDecision::Emit {
        event: DomainEvent::Validation(ValidationEvent::ValidationScheduled { request }),
    } = decide(&seed.barrier.phase4.state).unwrap()
    else {
        panic!("active validation node must schedule its current gate");
    };
    append(
        &mut seed.barrier.phase4.state,
        "phase6:current-run-authority:scheduled",
        ValidationEvent::ValidationScheduled {
            request: request.clone(),
        },
    );

    assert_event_rejected_atomically(
        &mut seed.barrier.phase4.state,
        "phase6:current-run-authority:duplicate-pending-schedule",
        ValidationEvent::ValidationScheduled {
            request: request.clone(),
        }
        .into(),
        "validation_schedule_has_current_run",
    );
    let premature_convergence = ValidationConvergence::new(
        FailureRevisionId::new("failure:phase6-premature-current-run-convergence"),
        seed.barrier.phase4.state.repository_revision.clone(),
        ValidationConvergenceReason::GateRunBudgetExhausted {
            gate_id: seed.gates[0].gate_id.clone(),
        },
    )
    .unwrap();
    assert_event_rejected_atomically(
        &mut seed.barrier.phase4.state,
        "phase6:current-run-authority:premature-pending-convergence",
        ValidationEvent::ConvergenceEvaluated {
            convergence: premature_convergence.clone(),
        }
        .into(),
        "validation_convergence_with_current_gate_run",
    );

    let started = ValidationProcessStarted::new(
        &request,
        stable_sha256(&["execution-protocol-v1:phase6-current-run-authority"]),
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:current-run-authority:started",
        ValidationEvent::ValidationProcessStarted {
            started: started.clone(),
        },
    );
    let completed = ValidationProcessCompleted::new(
        &request,
        Some(&started),
        100,
        ValidationProcessResult::Exited { exit_code: 1 },
        complete_output(b"validation failed but evidence is not recorded yet"),
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:current-run-authority:completed",
        ValidationEvent::ValidationProcessCompleted { completed },
    );
    assert_event_rejected_atomically(
        &mut seed.barrier.phase4.state,
        "phase6:current-run-authority:duplicate-completed-schedule",
        ValidationEvent::ValidationScheduled { request }.into(),
        "validation_schedule_has_current_run",
    );
    assert_event_rejected_atomically(
        &mut seed.barrier.phase4.state,
        "phase6:current-run-authority:premature-completed-convergence",
        ValidationEvent::ConvergenceEvaluated {
            convergence: premature_convergence,
        }
        .into(),
        "validation_convergence_with_current_gate_run",
    );
}

#[test]
fn failed_maximum_gate_run_converges_before_repair_work_without_usage_or_revision_change() {
    let mut seed = enter_aggregate_validation(validation_contract_seed_with_max_runs(1));
    let repository_revision = seed.barrier.phase4.state.repository_revision.clone();
    let budget_ledger = seed.barrier.phase4.state.budgets.clone();
    let run = start_aggregate_validation(&mut seed, "max-gate-run");
    let completed = ValidationProcessCompleted::new(
        &run.request,
        Some(&run.started),
        125,
        ValidationProcessResult::Exited { exit_code: 1 },
        complete_output(b"generic validation assertion failed at the gate run limit"),
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:max-gate-run:completed",
        ValidationEvent::ValidationProcessCompleted {
            completed: completed.clone(),
        },
    );
    let evidence = ValidationEvidenceV1::from_completed(
        &run.request,
        &run.started,
        &completed,
        ParserConfidence::Structured,
        GateSemanticsObservation::ExpectedSemanticsObserved,
        vec![failure_diagnostic(
            seed.barrier.phase4.accepted_plan.targets[0].path.clone(),
            BTreeSet::from([seed.barrier.phase4.accepted_plan.targets[0].path.clone()]),
            BTreeSet::new(),
        )],
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:max-gate-run:evidence",
        ValidationEvent::ValidationEvidenceRecorded { evidence },
    );
    let DomainEvent::Validation(ValidationEvent::ValidationFailureRevisionRecorded { failure }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:max-gate-run:failure-revision",
        )
    else {
        panic!("failed maximum gate run must record its domain failure");
    };
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:max-gate-run:node-failed",
        ),
        GraphEvent::NodeFailed {
            node_id: seed.barrier.validation_node_id.clone(),
            failure_revision_id: failure.failure_revision_id.clone(),
            terminal: false,
        }
        .into()
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:max-gate-run:failure-proof",
    ) else {
        panic!("failed maximum gate run must record its validation proof");
    };
    assert_eq!(proof.kind, ProofKind::ValidationFailure);
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:max-gate-run:repair-transition",
        ),
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Validation,
            to: ProtocolStage::Repair,
            proof_id: proof.id,
        }
        .into()
    );

    let validation = seed.barrier.phase4.state.validation.as_ref().unwrap();
    assert!(validation.rankings.is_empty());
    assert!(validation.eligibility.is_empty());
    assert!(validation.selections.is_empty());
    assert!(
        seed.barrier
            .phase4
            .state
            .nodes
            .values()
            .all(|node| node.kind != NodeKind::ValidationRepair)
    );
    let ProtocolDecision::Emit {
        event: DomainEvent::Validation(ValidationEvent::ConvergenceEvaluated { convergence }),
    } = decide(&seed.barrier.phase4.state).unwrap()
    else {
        panic!("gate run budget must converge before repair ranking or activation");
    };
    assert_eq!(
        convergence.reason,
        ValidationConvergenceReason::GateRunBudgetExhausted {
            gate_id: seed.gates[0].gate_id.clone(),
        }
    );
    append(
        &mut seed.barrier.phase4.state,
        "phase6:max-gate-run:convergence",
        ValidationEvent::ConvergenceEvaluated { convergence },
    );

    let validation = seed.barrier.phase4.state.validation.as_ref().unwrap();
    assert!(validation.rankings.is_empty());
    assert!(validation.eligibility.is_empty());
    assert!(validation.selections.is_empty());
    assert_eq!(seed.barrier.phase4.state.budgets, budget_ledger);
    assert_eq!(
        seed.barrier.phase4.state.repository_revision,
        repository_revision
    );
    assert!(
        seed.barrier
            .phase4
            .state
            .nodes
            .values()
            .all(|node| node.kind != NodeKind::ValidationRepair)
    );
    let ProtocolDecision::Finish { result } = decide(&seed.barrier.phase4.state).unwrap() else {
        panic!("gate run budget convergence must have one canonical result");
    };
    assert!(matches!(
        result.mission,
        MissionResult::BudgetBlocked { .. }
    ));
    assert_eq!(result.process_health, ProcessHealth::Healthy);
    assert_eq!(result.reason_code, "validation_gate_run_budget_exhausted");
}

#[test]
fn earlier_exhausted_gate_preflights_before_later_failure_repair_work() {
    let mut seed = enter_aggregate_validation(
        validation_contract_seed_with_two_focused_and_broad_gates(1, 2, 2),
    );
    let first_gate = seed.gates[0].clone();
    let second_gate = seed.gates[1].clone();
    let repository_revision = seed.barrier.phase4.state.repository_revision.clone();
    let budget_ledger = seed.barrier.phase4.state.budgets.clone();

    let first = start_aggregate_validation(&mut seed, "cross-gate-budget:first");
    assert_eq!(first.request.schedule.gate_id, first_gate.gate_id);
    complete_aggregate_validation_run(&mut seed, &first, "cross-gate-budget:first", 0, Vec::new());
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof: first_proof }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:cross-gate-budget:first-proof",
        )
    else {
        panic!("the first validation node must retain its passing proof");
    };
    assert_eq!(first_proof.kind, ProofKind::ValidationPassed);
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:cross-gate-budget:first-node-succeeded",
        ),
        GraphEvent::NodeSucceeded {
            node_id: first_gate.node_id.clone(),
            proof_id: first_proof.id,
        }
        .into()
    );

    let second = start_aggregate_validation(&mut seed, "cross-gate-budget:second");
    assert_eq!(second.request.schedule.gate_id, second_gate.gate_id);
    let repair_path = seed.barrier.phase4.accepted_plan.targets[0].path.clone();
    complete_aggregate_validation_run(
        &mut seed,
        &second,
        "cross-gate-budget:second",
        1,
        vec![failure_diagnostic(
            repair_path.clone(),
            BTreeSet::from([repair_path]),
            BTreeSet::new(),
        )],
    );
    let DomainEvent::Validation(ValidationEvent::ValidationFailureRevisionRecorded { failure }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:cross-gate-budget:failure",
        )
    else {
        panic!("the later gate failure must be recorded before repair preflight");
    };
    assert_eq!(failure.gate_id, second_gate.gate_id);
    let DomainEvent::Graph(GraphEvent::NodeFailed { .. }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:cross-gate-budget:node-failed",
    ) else {
        panic!("the later validation owner must fail recoverably");
    };
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: failure_proof,
    }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:cross-gate-budget:failure-proof",
    )
    else {
        panic!("the later failure must produce its transition proof");
    };
    assert_eq!(failure_proof.kind, ProofKind::ValidationFailure);
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:cross-gate-budget:repair-transition",
        ),
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Validation,
            to: ProtocolStage::Repair,
            proof_id: failure_proof.id,
        }
        .into()
    );

    let ProtocolDecision::Emit {
        event: DomainEvent::Validation(ValidationEvent::ConvergenceEvaluated { convergence }),
    } = decide(&seed.barrier.phase4.state).unwrap()
    else {
        panic!("all-gate run-budget preflight must precede repair ranking");
    };
    assert_eq!(
        convergence.reason,
        ValidationConvergenceReason::GateRunBudgetExhausted {
            gate_id: first_gate.gate_id,
        }
    );
    append(
        &mut seed.barrier.phase4.state,
        "phase6:cross-gate-budget:convergence",
        ValidationEvent::ConvergenceEvaluated { convergence },
    );
    let validation = seed.barrier.phase4.state.validation.as_ref().unwrap();
    assert!(validation.rankings.is_empty());
    assert!(validation.eligibility.is_empty());
    assert!(validation.selections.is_empty());
    assert!(
        seed.barrier
            .phase4
            .state
            .nodes
            .values()
            .all(|node| node.kind != NodeKind::ValidationRepair)
    );
    assert_eq!(seed.barrier.phase4.state.budgets, budget_ledger);
    assert_eq!(
        seed.barrier.phase4.state.repository_revision,
        repository_revision
    );
    let ProtocolDecision::Finish { result } = decide(&seed.barrier.phase4.state).unwrap() else {
        panic!("cross-gate run-budget preflight must terminate canonically");
    };
    assert_eq!(result.reason_code, "validation_gate_run_budget_exhausted");
    assert_eq!(result.mission.outcome(), MissionOutcomeV1::BudgetBlocked);
    assert_eq!(result.process_health, ProcessHealth::Healthy);
}

#[test]
fn active_validation_repair_loads_context_and_rejects_prebinding_events_atomically() {
    let ActiveAggregateRepair {
        mut seed,
        selection,
    } = active_aggregate_repair("repair-inertness");
    let repair_node_id = selection.repair_node.id;
    let repository_revision = seed.barrier.phase4.state.repository_revision.clone();
    let budget_ledger = seed.barrier.phase4.state.budgets.clone();
    let ProtocolDecision::Perform {
        effect:
            EffectRequest::Validation(ValidationEffectRequest::LoadRepairTargetContext { request }),
    } = decide(&seed.barrier.phase4.state).unwrap()
    else {
        panic!("active repair must request its authoritative context before execution");
    };
    assert_eq!(request.node_id, repair_node_id);
    assert_eq!(request.target_id, selection.intent.target_id);
    assert_eq!(
        request.purpose,
        TargetExecutionPurpose::ValidationRepair {
            repair_intent_id: selection.intent.repair_intent_id.clone(),
            failure_revision_id: selection.intent.failure_revision_id.clone(),
            originating_gate_id: selection.intent.originating_gate_id.clone(),
            validation_evidence_id: seed
                .barrier
                .phase4
                .state
                .validation
                .as_ref()
                .unwrap()
                .current_failure()
                .unwrap()
                .validation_evidence_id
                .clone(),
            baseline_mutation_evidence_id: selection.intent.baseline_mutation_evidence_id.clone(),
        }
    );

    let failure = seed
        .barrier
        .phase4
        .state
        .validation
        .as_ref()
        .and_then(ValidationState::current_failure)
        .expect("active repair failure")
        .clone();
    let baseline = current_repair_mutation_baselines(&seed)
        .get(&selection.intent.target_id)
        .expect("active repair current mutation baseline")
        .clone();
    let (materialized, _) = materialized_repair_context(&seed, &request, &baseline);
    let mut mismatched_context = prepare_target_context(&request, &materialized).unwrap();
    mismatched_context.manifest.repository_fingerprint = stable_sha256(&[
        "execution-protocol-v1:phase6-forged-repair-repository-fingerprint",
        failure.failure_revision_id.as_str(),
    ]);
    assert_ne!(
        mismatched_context.manifest.repository_fingerprint,
        baseline.evidence().repository_fingerprint_after
    );
    assert_event_rejected_atomically(
        &mut seed.barrier.phase4.state,
        "phase6:repair-inertness:repository-fingerprint-mismatch",
        ValidationEvent::RepairTargetContextPrepared {
            prepared: Box::new(mismatched_context),
        }
        .into(),
        "repair_target_context_repository_fingerprint_mismatch",
    );

    assert_event_rejected_atomically(
        &mut seed.barrier.phase4.state,
        "phase6:repair-inertness:model-call",
        BudgetEvent::ModelCallAdmitted {
            admission: ModelCallAdmission {
                call_id: ModelCallId::new("call:phase6-forged-repair"),
                node_id: repair_node_id.clone(),
                action_id: ActionId::new("action:phase6-forged-repair"),
                payload_hash: stable_sha256(&["phase6:forged-repair-payload"]),
                input_tokens: 1,
                output_tokens: 1,
                reserved_cost_micros: 1,
                duration_allowance_ms: 1,
            },
        }
        .into(),
        "repair_model_call_without_authoritative_action",
    );
    let forged_effect_id = EffectId::new("effect:phase6-forged-repair");
    assert_event_rejected_atomically(
        &mut seed.barrier.phase4.state,
        "phase6:repair-inertness:waiting",
        GraphEvent::NodeWaiting {
            node_id: repair_node_id.clone(),
            effect_id: forged_effect_id.clone(),
        }
        .into(),
        "repair_execution_effect_unavailable",
    );
    assert_event_rejected_atomically(
        &mut seed.barrier.phase4.state,
        "phase6:repair-inertness:resumed",
        GraphEvent::NodeResumed {
            node_id: repair_node_id.clone(),
            effect_id: forged_effect_id,
        }
        .into(),
        "repair_execution_effect_unavailable",
    );
    assert_event_rejected_atomically(
        &mut seed.barrier.phase4.state,
        "phase6:repair-inertness:failed",
        GraphEvent::NodeFailed {
            node_id: repair_node_id.clone(),
            failure_revision_id: FailureRevisionId::new("failure:phase6-forged-repair"),
            terminal: false,
        }
        .into(),
        "repair_failure_without_exact_mutation_convergence",
    );

    let mut verification = seed
        .barrier
        .phase4
        .state
        .event_log
        .iter()
        .rev()
        .find_map(|stored| match &stored.envelope.payload {
            DomainEvent::Mutation(MutationEvent::MutationVerified { evidence }) => {
                Some(evidence.clone())
            }
            _ => None,
        })
        .expect("the completed implementation barrier contains verified mutation evidence");
    verification.node_id = repair_node_id.clone();
    rebind_mutation_verification_identity(&mut verification);
    verification.validate().unwrap();
    assert_event_rejected_atomically(
        &mut seed.barrier.phase4.state,
        "phase6:repair-inertness:mutation-verified",
        MutationEvent::MutationVerified {
            evidence: verification,
        }
        .into(),
        "repair_mutation_current_context_missing",
    );

    assert_event_rejected_atomically(
        &mut seed.barrier.phase4.state,
        "phase6:repair-inertness:already-satisfied",
        EvidenceEvent::ProofRecorded {
            proof: ProofRecord {
                id: ProofId::new("proof:phase6-forged-repair-already-satisfied"),
                kind: ProofKind::AlreadySatisfied,
                repository_revision: repository_revision.clone(),
                node_ids: vec![repair_node_id.clone()],
                related_proof_ids: Vec::new(),
                related_evidence_ids: Vec::new(),
                detail_hash: stable_sha256(&[
                    "execution-protocol-v1:phase6-forged-repair-already-satisfied",
                ]),
            },
        }
        .into(),
        "repair_already_satisfied_proof_unavailable",
    );

    assert!(matches!(
        seed.barrier
            .phase4
            .state
            .node(&repair_node_id)
            .map(|node| &node.state),
        Some(NodeState::Active { attempt: 1 })
    ));
    assert_eq!(seed.barrier.phase4.state.budgets, budget_ledger);
    assert_eq!(
        seed.barrier.phase4.state.repository_revision,
        repository_revision
    );
}

#[test]
fn repair_mutation_convergence_maps_readiness_and_drift_to_exact_terminal_results() {
    struct Observation {
        label: &'static str,
        result: CanonicalResult,
        expected_reason: &'static str,
        expected_outcome: MissionOutcomeV1,
        expected_health: ProcessHealth,
        expected_category: &'static str,
    }

    let mut observations = Vec::new();

    let no_feasible_seed = validation_contract_seed_with_repair_budget("no-feasible", |budget| {
        budget.max_output_tokens_per_call = 1;
    });
    let mut no_feasible = active_aggregate_repair_from_seed(no_feasible_seed, "repair-no-feasible");
    let no_feasible_context =
        prepare_aggregate_repair_context(&mut no_feasible, "repair-no-feasible");
    assert!(
        no_feasible_context
            .feasibility
            .feasible_strategies()
            .is_empty()
    );
    let DomainEvent::Mutation(MutationEvent::ReadinessConvergenceEvaluated {
        convergence: no_feasible_convergence,
    }) = append_next_authoritative(
        &mut no_feasible.seed.barrier.phase4.state,
        "phase6:repair-no-feasible:convergence",
    )
    else {
        panic!("an empty repair feasibility set must converge before policy creation");
    };
    assert_eq!(
        no_feasible_convergence.reason,
        MutationReadinessConvergenceReason::NoFeasibleStrategy
    );
    let no_feasible_result = finish_repair_mutation_convergence(
        &mut no_feasible,
        "repair-no-feasible",
        no_feasible_convergence.failure_revision_id,
    );
    observations.push(Observation {
        label: "readiness/no-feasible",
        result: no_feasible_result,
        expected_reason: "repair_no_feasible_strategy",
        expected_outcome: MissionOutcomeV1::NoValidRepair,
        expected_health: ProcessHealth::Healthy,
        expected_category: "validation",
    });

    let admission_seed = validation_contract_seed_with_repair_budget("admission", |budget| {
        budget.max_model_calls = 1;
    });
    let mut admission = active_aggregate_repair_from_seed(admission_seed, "repair-admission");
    let _ = prepare_aggregate_repair_context(&mut admission, "repair-admission");
    let admission_action = prepare_aggregate_repair_action(&mut admission, "repair-admission");
    dispatch_and_consume_aggregate_mutation(
        &mut admission.seed.barrier.phase4.state,
        &admission_action,
        "phase6-repair-admission",
        25,
        20,
    );
    let rejected = MutationFailure::new(
        &admission_action.policy,
        admission_action
            .policy
            .permitted_strategies
            .first()
            .copied(),
        None,
        MutationFailureClass::CandidateSchemaInvalid,
        MutationFailureDetailCode::ExpectedHashMismatch,
        None,
    )
    .unwrap();
    append(
        &mut admission.seed.barrier.phase4.state,
        "phase6:repair-admission:action-rejected",
        MutationEvent::ActionRejected { failure: rejected },
    );
    let DomainEvent::Mutation(MutationEvent::AttemptPolicySelected {
        policy: retry_policy,
    }) = append_next_authoritative(
        &mut admission.seed.barrier.phase4.state,
        "phase6:repair-admission:retry-policy",
    )
    else {
        panic!("retryable candidate rejection must select its bounded retry policy");
    };
    let DomainEvent::Mutation(MutationEvent::ReadinessConvergenceEvaluated {
        convergence: admission_convergence,
    }) = append_next_authoritative(
        &mut admission.seed.barrier.phase4.state,
        "phase6:repair-admission:convergence",
    )
    else {
        panic!("exhausted repair admission budget must converge before another action");
    };
    let MutationReadinessConvergenceReason::AdmissionBudgetExhausted {
        remaining,
        exhausted_dimensions,
    } = &admission_convergence.reason
    else {
        panic!("repair admission convergence must retain typed dimensions");
    };
    assert_eq!(
        admission_convergence.attempt_id,
        Some(retry_policy.attempt_id)
    );
    assert_eq!(remaining.model_calls, 0);
    assert!(exhausted_dimensions.contains(&MutationAdmissionBudgetDimension::ModelCalls));
    let admission_result = finish_repair_mutation_convergence(
        &mut admission,
        "repair-admission",
        admission_convergence.failure_revision_id,
    );
    observations.push(Observation {
        label: "readiness/admission-budget",
        result: admission_result,
        expected_reason: "repair_admission_budget_exhausted",
        expected_outcome: MissionOutcomeV1::BudgetBlocked,
        expected_health: ProcessHealth::Healthy,
        expected_category: "budget",
    });

    let mut uncontacted = active_aggregate_repair("repair-uncontacted");
    let _ = prepare_aggregate_repair_context(&mut uncontacted, "repair-uncontacted");
    let first_action = prepare_aggregate_repair_action(&mut uncontacted, "repair-uncontacted");
    release_uncontacted_aggregate_action(
        &mut uncontacted.seed.barrier.phase4.state,
        &first_action,
        "phase6-repair-uncontacted-first",
    );
    let DomainEvent::Mutation(MutationEvent::ActionPrepared {
        prepared: second_action,
    }) = append_next_authoritative(
        &mut uncontacted.seed.barrier.phase4.state,
        "phase6:repair-uncontacted:second-action",
    )
    else {
        panic!("one uncontacted release must prepare the bounded retry action");
    };
    let second_action = *second_action;
    release_uncontacted_aggregate_action(
        &mut uncontacted.seed.barrier.phase4.state,
        &second_action,
        "phase6-repair-uncontacted-second",
    );
    let DomainEvent::Mutation(MutationEvent::ReadinessConvergenceEvaluated {
        convergence: uncontacted_convergence,
    }) = append_next_authoritative(
        &mut uncontacted.seed.barrier.phase4.state,
        "phase6:repair-uncontacted:convergence",
    )
    else {
        panic!("bounded repair action releases must converge");
    };
    assert!(matches!(
        uncontacted_convergence.reason,
        MutationReadinessConvergenceReason::UncontactedActionRetryExhausted { .. }
    ));
    let uncontacted_result = finish_repair_mutation_convergence(
        &mut uncontacted,
        "repair-uncontacted",
        uncontacted_convergence.failure_revision_id,
    );
    observations.push(Observation {
        label: "readiness/uncontacted-release",
        result: uncontacted_result,
        expected_reason: "repair_uncontacted_action_retry_exhausted",
        expected_outcome: MissionOutcomeV1::InfrastructureFailed,
        expected_health: ProcessHealth::Failed {
            code: "repair_uncontacted_action_retry_exhausted".into(),
        },
        expected_category: "infrastructure",
    });

    let mut drift = active_aggregate_repair("repair-drift");
    let drift_context = prepare_aggregate_repair_context(&mut drift, "repair-drift");
    let drift_action = prepare_aggregate_repair_action(&mut drift, "repair-drift");
    dispatch_and_consume_aggregate_mutation(
        &mut drift.seed.barrier.phase4.state,
        &drift_action,
        "phase6-repair-drift",
        25,
        20,
    );
    let candidate = accepted_repair_candidate(&drift_action, &drift_context, "repair-drift");
    append(
        &mut drift.seed.barrier.phase4.state,
        "phase6:repair-drift:candidate",
        MutationEvent::CandidateRecorded {
            candidate: candidate.clone(),
        },
    );
    let observed_revision =
        RepositoryRevisionId::new("repository-revision:phase6-repair-drift-observed");
    let repository_drift = RepositoryDriftRecovery {
        expected_revision: drift_context.context.manifest.repository_revision.clone(),
        observed_revision: observed_revision.clone(),
        expected_fingerprint: drift_context
            .context
            .manifest
            .repository_fingerprint
            .clone(),
        observed_fingerprint: stable_sha256(&[
            "execution-protocol-v1:phase6-repair-drift-observed-fingerprint",
        ]),
        context_rebuild_required: true,
    };
    let drift_failure = MutationFailure::new(
        &drift_action.policy,
        Some(candidate.strategy),
        Some(candidate.candidate_id),
        MutationFailureClass::RepositoryDrift,
        MutationFailureDetailCode::RepositoryDrift,
        Some(repository_drift.clone()),
    )
    .unwrap();
    append(
        &mut drift.seed.barrier.phase4.state,
        "phase6:repair-drift:attempt-failed",
        MutationEvent::AttemptFailed {
            failure: drift_failure,
        },
    );
    let context_rebuilds_before = drift
        .seed
        .barrier
        .phase4
        .state
        .node(&drift.selection.repair_node.id)
        .unwrap()
        .usage
        .context_rebuilds;
    let DomainEvent::Mutation(MutationEvent::ConvergenceEvaluated {
        convergence: drift_convergence,
    }) = append_next_authoritative(
        &mut drift.seed.barrier.phase4.state,
        "phase6:repair-drift:convergence",
    )
    else {
        panic!("candidate-bound repair drift must converge without rebuilding context");
    };
    assert_eq!(
        drift_convergence.reason,
        MutationConvergenceReason::ContextRebuildUnavailable
    );
    assert_eq!(drift_convergence.repository_drift, Some(repository_drift));
    assert_eq!(
        drift_convergence.repository_revision_after,
        observed_revision
    );
    assert_eq!(
        drift.seed.barrier.phase4.state.repository_revision,
        observed_revision
    );
    assert_eq!(
        drift
            .seed
            .barrier
            .phase4
            .state
            .node(&drift.selection.repair_node.id)
            .unwrap()
            .usage
            .context_rebuilds,
        context_rebuilds_before
    );
    let drift_result = finish_repair_mutation_convergence(
        &mut drift,
        "repair-drift",
        drift_convergence.last_failure_revision_id,
    );
    assert_eq!(drift_result.repository_revision, observed_revision);
    append(
        &mut drift.seed.barrier.phase4.state,
        "phase6:repair-drift:terminal",
        TerminalEvent::CanonicalResultRecorded {
            result: drift_result.clone(),
        },
    );
    assert_execution_replays_exactly(&drift.seed);
    observations.push(Observation {
        label: "normal/repository-drift",
        result: drift_result,
        expected_reason: "repair_context_rebuild_unavailable",
        expected_outcome: MissionOutcomeV1::NoValidRepair,
        expected_health: ProcessHealth::Healthy,
        expected_category: "validation",
    });

    for observation in observations {
        assert_eq!(
            observation.result.reason_code, observation.expected_reason,
            "{} reason mapping",
            observation.label
        );
        assert_eq!(
            observation.result.mission.outcome(),
            observation.expected_outcome,
            "{} mission mapping",
            observation.label
        );
        assert_eq!(
            observation.result.process_health, observation.expected_health,
            "{} health mapping",
            observation.label
        );
        let blocker = observation
            .result
            .mission
            .first_fatal_blocker()
            .expect("repair convergence retains its exact blocker");
        assert_eq!(
            blocker.category, observation.expected_category,
            "{} blocker category",
            observation.label
        );
        assert_eq!(blocker.code, observation.expected_reason);
    }
}

fn phase7_golden_b_review_entry_seed_from_contract(
    contract_seed: ValidationContractSeed,
    verify_handoff_checkpoints: bool,
) -> Phase7ReviewEntrySeed {
    let ActiveAggregateRepair {
        mut seed,
        selection,
    } = active_aggregate_repair_from_seed(contract_seed, "golden-b");
    let implementation_barrier_proof_id = seed.barrier.barrier_proof_id.clone();
    let validation_failure_proof_id = seed
        .barrier
        .phase4
        .state
        .latest_transition_proof
        .as_ref()
        .filter(|proof_id| {
            seed.barrier.phase4.state.proof_kind(proof_id) == Some(ProofKind::ValidationFailure)
        })
        .expect("Golden B repair entry retains its exact validation-failure proof")
        .clone();
    let failure = seed
        .barrier
        .phase4
        .state
        .validation
        .as_ref()
        .and_then(ValidationState::current_failure)
        .expect("Golden B active validation failure")
        .clone();
    let baselines = current_repair_mutation_baselines(&seed);
    let baseline = baselines
        .get(&selection.intent.target_id)
        .expect("Golden B current verified mutation baseline")
        .clone();
    let repair_target = repair_target_for_selection(
        &selection,
        &failure,
        &seed.barrier.phase4.accepted_plan,
        &baseline,
    )
    .unwrap();
    let ProtocolDecision::Perform {
        effect:
            EffectRequest::Validation(ValidationEffectRequest::LoadRepairTargetContext { request }),
    } = decide(&seed.barrier.phase4.state).unwrap()
    else {
        panic!("Golden B must load the selected repair target context");
    };
    let (materialized, current_bytes) = materialized_repair_context(&seed, &request, &baseline);
    let prepared_context = prepare_target_context(&request, &materialized).unwrap();
    assert_eq!(
        prepared_context.manifest.purpose,
        selection.execution_purpose(&failure).unwrap()
    );
    let repository_revision_before = seed.barrier.phase4.state.repository_revision.clone();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:golden-b:repair-context-prepared",
        ValidationEvent::RepairTargetContextPrepared {
            prepared: Box::new(prepared_context.clone()),
        },
    );
    assert_eq!(
        seed.barrier
            .phase4
            .state
            .validation
            .as_ref()
            .unwrap()
            .repair_contexts
            .prepared_context_for_node(&selection.repair_node.id),
        Some(&prepared_context)
    );
    assert_eq!(
        seed.barrier.phase4.state.repository_revision,
        repository_revision_before
    );

    let DomainEvent::Mutation(MutationEvent::FeasibilityEvaluated { feasibility }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:golden-b:repair-feasibility",
        )
    else {
        panic!("prepared repair context must evaluate mutation feasibility");
    };
    assert_eq!(feasibility.target_id, repair_target.target_id);
    assert_eq!(
        feasibility.context_manifest_id,
        prepared_context.context_manifest_id
    );
    let DomainEvent::Mutation(MutationEvent::AttemptPolicySelected { policy }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:golden-b:repair-policy",
        )
    else {
        panic!("feasible repair target must select one mutation attempt policy");
    };
    assert_eq!(policy.node_id, selection.repair_node.id);
    let DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:golden-b:repair-action",
        )
    else {
        panic!("repair mutation policy must prepare one provider action");
    };
    let prepared = *prepared;
    assert_eq!(
        prepared.provider_request.tool_names(),
        vec![MutationToolName::ApplyPatch, MutationToolName::ReplaceFile]
    );
    assert_eq!(prepared.provider_request.node_id, selection.repair_node.id);
    assert_eq!(
        prepared.provider_request.context_manifest_id,
        prepared_context.context_manifest_id
    );
    dispatch_and_consume_aggregate_mutation(
        &mut seed.barrier.phase4.state,
        &prepared,
        "golden-b-repair",
        75,
        45,
    );

    let mut repaired_bytes = current_bytes.clone();
    repaired_bytes.extend_from_slice(b"\n// phase6 verified validation repair\n");
    let invocation = MaterializedMutationInvocation {
        action_id: prepared.provider_request.action_id.clone(),
        call_id: prepared.provider_request.call_id.clone(),
        tool_call_count: 1,
        completeness: ProviderOutputCompleteness::Complete,
        arguments: MaterializedMutationArguments::ApplyPatch {
            path: repair_target.path.clone(),
            expected_content_hash: hex::encode(Sha256::digest(&current_bytes)),
            patch: durable_artifact(
                "phase6-golden-b-patch",
                b"--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+repaired\n".to_vec(),
            ),
            expected_after_content: durable_artifact(
                "phase6-golden-b-expected-after",
                repaired_bytes.clone(),
            ),
        },
    };
    let MutationCandidateObservation::Accepted { candidate } =
        record_mutation_candidate(&prepared, &repair_target, &invocation).unwrap()
    else {
        panic!("Golden B repair mutation candidate must be accepted");
    };
    append(
        &mut seed.barrier.phase4.state,
        "phase6:golden-b:repair-candidate",
        MutationEvent::CandidateRecorded {
            candidate: candidate.clone(),
        },
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Mutation(MutationEffectRequest::ApplyMutation { request: apply }),
    } = decide(&seed.barrier.phase4.state).unwrap()
    else {
        panic!("accepted repair candidate must request repository application");
    };
    let application =
        MutationApplicationObservation::new(&apply, MutationApplicationStatus::Applied);
    append(
        &mut seed.barrier.phase4.state,
        "phase6:golden-b:repair-applied",
        MutationEvent::ApplicationObserved {
            request: (*apply).clone(),
            observation: application.clone(),
        },
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Mutation(MutationEffectRequest::VerifyMutation { request: verify }),
    } = decide(&seed.barrier.phase4.state).unwrap()
    else {
        panic!("applied repair mutation must request repository verification");
    };
    let repaired_hash = hex::encode(Sha256::digest(&repaired_bytes));
    let transitions = BTreeMap::from([(
        repair_target.path.clone(),
        MutationPathTransition {
            before: file_state(
                hex::encode(Sha256::digest(&current_bytes)),
                u64::try_from(current_bytes.len()).unwrap(),
            ),
            after: file_state(repaired_hash, u64::try_from(repaired_bytes.len()).unwrap()),
        },
    )]);
    let materialized_verification = MaterializedMutationVerification {
        request_id: verify.request_id.clone(),
        repository_revision: repository_revision_before.clone(),
        repository_fingerprint_before: prepared_context.manifest.repository_fingerprint.clone(),
        repository_fingerprint_after: stable_sha256(&[
            "execution-protocol-v1:phase6-golden-b-repository-after",
            candidate.candidate_id.as_str(),
        ]),
        changed_paths: transitions.keys().cloned().collect(),
        path_transitions: transitions,
    };
    let verification = verify_mutation_application(
        &verify,
        &apply,
        &application,
        &candidate,
        &repair_target,
        &materialized_verification,
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:golden-b:repair-verified",
        MutationEvent::MutationVerified {
            evidence: verification.clone(),
        },
    );
    assert_eq!(
        seed.barrier.phase4.state.repository_revision,
        verification.repository_revision_after
    );
    if verify_handoff_checkpoints {
        assert_execution_replays_exactly(&seed);
    }

    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: mutation_proof,
    }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:golden-b:mutation-proof",
    )
    else {
        panic!("verified repair mutation must produce its mutation proof");
    };
    assert_eq!(mutation_proof.kind, ProofKind::MutationVerified);
    if verify_handoff_checkpoints {
        assert_execution_replays_exactly(&seed);
    }
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: repair_proof,
    }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:golden-b:repair-proof",
    )
    else {
        panic!("verified repair mutation must produce its repair proof");
    };
    assert_eq!(repair_proof.kind, ProofKind::RepairVerified);
    let repair_eligibility_proof_id = repair_proof
        .related_proof_ids
        .iter()
        .find(|proof_id| {
            seed.barrier.phase4.state.proof_kind(proof_id) == Some(ProofKind::RepairEligibility)
        })
        .expect("Golden B repair proof retains its eligibility ancestor")
        .clone();
    if verify_handoff_checkpoints {
        assert_execution_replays_exactly(&seed);
    }
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:golden-b:repair-node-succeeded",
        ),
        GraphEvent::NodeSucceeded {
            node_id: selection.repair_node.id.clone(),
            proof_id: repair_proof.id.clone(),
        }
        .into()
    );
    if verify_handoff_checkpoints {
        assert_execution_replays_exactly(&seed);
    }
    let DomainEvent::Validation(ValidationEvent::PriorValidationInvalidated { invalidation }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:golden-b:prior-validation-invalidated",
        )
    else {
        panic!("verified repair must invalidate the exact stale validation evidence");
    };
    assert_eq!(
        invalidation.verified_repair_evidence_id,
        verification.evidence_id
    );
    if verify_handoff_checkpoints {
        assert_execution_replays_exactly(&seed);
    }
    let DomainEvent::Validation(ValidationEvent::ValidationRerunScheduled { rerun }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:golden-b:rerun-scheduled",
        )
    else {
        panic!("verified repair must schedule its originating validation gate");
    };
    assert_eq!(rerun.originating_gate_id, failure.gate_id);
    if verify_handoff_checkpoints {
        assert_execution_replays_exactly(&seed);
    }
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof: rerun_proof }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:golden-b:rerun-proof",
        )
    else {
        panic!("scheduled rerun must produce its handoff proof");
    };
    assert_eq!(rerun_proof.kind, ProofKind::ValidationRerunScheduled);
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:golden-b:return-to-validation",
        ),
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Repair,
            to: ProtocolStage::Validation,
            proof_id: rerun_proof.id.clone(),
        }
        .into()
    );
    if verify_handoff_checkpoints {
        assert_execution_replays_exactly(&seed);
    }

    let rerun_process = start_aggregate_validation(&mut seed, "golden-b-rerun");
    assert_eq!(rerun_process.request.schedule.gate_id, failure.gate_id);
    assert_eq!(rerun_process.request.schedule.run_attempt, 2);
    assert!(matches!(
        rerun_process.request.schedule.kind,
        ValidationRunKind::ExactRepairRerun { .. }
    ));
    let completed = ValidationProcessCompleted::new(
        &rerun_process.request,
        Some(&rerun_process.started),
        140,
        ValidationProcessResult::Exited { exit_code: 0 },
        complete_output(b"repaired validation passed"),
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:golden-b:rerun-completed",
        ValidationEvent::ValidationProcessCompleted {
            completed: completed.clone(),
        },
    );
    let passed = ValidationEvidenceV1::from_completed(
        &rerun_process.request,
        &rerun_process.started,
        &completed,
        ParserConfidence::Exact,
        GateSemanticsObservation::ExpectedSemanticsObserved,
        Vec::new(),
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:golden-b:rerun-evidence",
        ValidationEvent::ValidationEvidenceRecorded { evidence: passed },
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof: pass_proof }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:golden-b:rerun-pass-proof",
        )
    else {
        panic!("passing rerun must produce a validation proof");
    };
    assert_eq!(pass_proof.kind, ProofKind::ValidationPassed);
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:golden-b:validation-node-succeeded",
        ),
        GraphEvent::NodeSucceeded {
            node_id: seed.barrier.validation_node_id.clone(),
            proof_id: pass_proof.id.clone(),
        }
        .into()
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: required_proof,
    }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:golden-b:required-validation-proof",
    )
    else {
        panic!("passing exact rerun must satisfy required validation");
    };
    assert_eq!(required_proof.kind, ProofKind::RequiredValidationPassed);
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:golden-b:review-transition",
        ),
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Validation,
            to: ProtocolStage::Review,
            proof_id: required_proof.id.clone(),
        }
        .into()
    );
    assert_eq!(
        seed.barrier.phase4.state.position,
        ProtocolPosition::Review(ReviewStep::DiffReview)
    );
    if verify_handoff_checkpoints {
        let restored = InMemoryEventStore::restore(
            seed.barrier.phase4.trusted_initial.clone(),
            seed.barrier.phase4.state.clone(),
        )
        .expect("Golden B repair and exact rerun restore identically")
        .into_state();
        assert_eq!(restored, seed.barrier.phase4.state);
    }

    let review_node_id = required_phase7_node_id(&seed.barrier.phase4.state, NodeKind::Review);
    let completion_node_id =
        required_phase7_node_id(&seed.barrier.phase4.state, NodeKind::CompletionEvaluation);
    let publication_node_id =
        required_phase7_node_id(&seed.barrier.phase4.state, NodeKind::Publication);
    let review_entry = Phase7ReviewEntrySeed {
        trusted_initial: seed.barrier.phase4.trusted_initial,
        state: seed.barrier.phase4.state,
        implementation_barrier_proof_id,
        required_validation_proof_id: required_proof.id,
        current_validation_pass_proof_ids: required_proof.related_proof_ids,
        current_validation_evidence_ids: required_proof.related_evidence_ids,
        repair_ancestry: Some(Phase7RepairAncestryIds {
            failure_revision_id: failure.failure_revision_id,
            failed_validation_evidence_id: failure.validation_evidence_id,
            validation_failure_proof_id,
            repair_intent_id: selection.intent.repair_intent_id,
            repair_eligibility_proof_id,
            repair_mutation_evidence_id: verification.evidence_id,
            repair_mutation_proof_id: mutation_proof.id,
            repair_verification_proof_id: repair_proof.id,
            invalidated_validation_evidence_ids: invalidation.invalidated_evidence_ids,
            validation_rerun_id: rerun.rerun_id,
            validation_rerun_proof_id: rerun_proof.id,
        }),
        review_node_id,
        completion_node_id,
        publication_node_id,
    };
    assert_eq!(review_entry.state.stage(), ProtocolStage::Review);
    review_entry
}

pub(super) fn phase7_golden_b_review_entry_seed() -> Phase7ReviewEntrySeed {
    phase7_golden_b_review_entry_seed_from_contract(validation_contract_seed(), true)
}

pub(super) fn phase7_golden_b_review_entry_seed_with_policy(
    policy_for_plan: impl FnOnce(&AcceptedPlan) -> FinalizationPolicyV1,
) -> Phase7ReviewEntrySeed {
    phase7_golden_b_review_entry_seed_from_contract(
        validation_contract_seed_with_finalization_policy(policy_for_plan),
        false,
    )
}

#[test]
fn aggregate_golden_b_verified_repair_reruns_the_exact_gate_and_reaches_review() {
    let seed = phase7_golden_b_review_entry_seed();
    assert!(seed.repair_ancestry.is_some());
    assert_eq!(seed.state.stage(), ProtocolStage::Review);
}

#[test]
fn late_broad_failure_repair_reruns_origin_then_all_invalidated_focused_gates() {
    let mut seed = enter_aggregate_validation(
        validation_contract_seed_with_two_focused_and_broad_gates(3, 3, 3),
    );
    let first_focused = seed.gates[0].clone();
    let second_focused = seed.gates[1].clone();
    let broad = seed.gates[2].clone();
    let repository_revision_before = seed.barrier.phase4.state.repository_revision.clone();

    let first_run = start_aggregate_validation(&mut seed, "late-broad:first-focused-r1");
    assert_eq!(first_run.request.schedule.gate_id, first_focused.gate_id);
    let first_evidence = complete_aggregate_validation_run(
        &mut seed,
        &first_run,
        "late-broad:first-focused-r1",
        0,
        Vec::new(),
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: retained_first_proof,
    }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:late-broad:first-focused-r1-proof",
    )
    else {
        panic!("the earlier focused node must retain its R1 validation proof");
    };
    assert_eq!(retained_first_proof.kind, ProofKind::ValidationPassed);
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:late-broad:first-focused-r1-succeeded",
        ),
        GraphEvent::NodeSucceeded {
            node_id: first_focused.node_id.clone(),
            proof_id: retained_first_proof.id.clone(),
        }
        .into()
    );

    let second_run = start_aggregate_validation(&mut seed, "late-broad:second-focused-r1");
    assert_eq!(second_run.request.schedule.gate_id, second_focused.gate_id);
    let second_evidence = complete_aggregate_validation_run(
        &mut seed,
        &second_run,
        "late-broad:second-focused-r1",
        0,
        Vec::new(),
    );
    let broad_run = start_gate_on_active_validation_node(&mut seed, "late-broad:broad-r1");
    assert_eq!(broad_run.request.schedule.gate_id, broad.gate_id);
    assert_eq!(
        broad_run.request.schedule.repository_revision,
        repository_revision_before
    );
    let repair_path = seed.barrier.phase4.accepted_plan.targets[0].path.clone();
    let broad_evidence = complete_aggregate_validation_run(
        &mut seed,
        &broad_run,
        "late-broad:broad-r1",
        1,
        vec![failure_diagnostic(
            repair_path.clone(),
            BTreeSet::from([repair_path]),
            BTreeSet::new(),
        )],
    );

    let active = activate_current_validation_failure(seed, "late-broad");
    assert_eq!(active.selection.intent.originating_gate_id, broad.gate_id);
    let CompletedAggregateRepair {
        mut seed,
        failure,
        selection,
        verification,
        invalidation,
        rerun,
    } = complete_verified_aggregate_repair(active, "late-broad");
    let repository_revision_after = verification.repository_revision_after.clone();
    assert_ne!(repository_revision_after, repository_revision_before);
    assert_eq!(failure.gate_id, broad.gate_id);
    assert_eq!(selection.intent.originating_gate_id, broad.gate_id);
    assert_eq!(rerun.originating_gate_id, broad.gate_id);
    assert_eq!(rerun.repository_revision, repository_revision_after);
    assert_eq!(
        invalidation.invalidated_evidence_ids,
        BTreeSet::from([
            first_evidence.evidence_id,
            second_evidence.evidence_id,
            broad_evidence.evidence_id,
        ])
    );

    let broad_rerun = start_aggregate_validation(&mut seed, "late-broad:broad-r2");
    assert_eq!(broad_rerun.request.schedule.gate_id, broad.gate_id);
    assert_eq!(
        broad_rerun.request.schedule.repository_revision,
        repository_revision_after
    );
    assert_eq!(broad_rerun.request.schedule.run_attempt, 2);
    assert!(matches!(
        broad_rerun.request.schedule.kind,
        ValidationRunKind::ExactRepairRerun { .. }
    ));
    complete_aggregate_validation_run(
        &mut seed,
        &broad_rerun,
        "late-broad:broad-r2",
        0,
        Vec::new(),
    );

    let second_rerun =
        start_gate_on_active_validation_node(&mut seed, "late-broad:second-focused-r2");
    assert_eq!(
        second_rerun.request.schedule.gate_id,
        second_focused.gate_id
    );
    assert_eq!(
        second_rerun.request.schedule.repository_revision,
        repository_revision_after
    );
    assert_eq!(second_rerun.request.schedule.run_attempt, 2);
    assert_eq!(
        second_rerun.request.schedule.kind,
        ValidationRunKind::Initial
    );
    complete_aggregate_validation_run(
        &mut seed,
        &second_rerun,
        "late-broad:second-focused-r2",
        0,
        Vec::new(),
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: second_r2_proof,
    }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:late-broad:second-focused-r2-proof",
    )
    else {
        panic!("the broad owner must receive a current R2 validation proof");
    };
    assert_eq!(second_r2_proof.kind, ProofKind::ValidationPassed);
    assert_eq!(
        second_r2_proof.repository_revision,
        repository_revision_after
    );
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:late-broad:second-focused-r2-succeeded",
        ),
        GraphEvent::NodeSucceeded {
            node_id: second_focused.node_id.clone(),
            proof_id: second_r2_proof.id,
        }
        .into()
    );

    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:late-broad:first-focused-r2-started",
        ),
        GraphEvent::NodeStarted {
            node_id: first_focused.node_id.clone(),
            attempt: 2,
        }
        .into()
    );
    assert_event_rejected_atomically(
        &mut seed.barrier.phase4.state,
        "phase6:late-broad:first-focused-stale-r1-proof",
        GraphEvent::NodeSucceeded {
            node_id: first_focused.node_id.clone(),
            proof_id: retained_first_proof.id,
        }
        .into(),
        "validation_node_success_proof_not_current",
    );

    let first_rerun =
        start_gate_on_active_validation_node(&mut seed, "late-broad:first-focused-r2");
    assert_eq!(first_rerun.request.schedule.gate_id, first_focused.gate_id);
    assert_eq!(
        first_rerun.request.schedule.repository_revision,
        repository_revision_after
    );
    assert_eq!(first_rerun.request.schedule.run_attempt, 2);
    assert_eq!(
        first_rerun.request.schedule.kind,
        ValidationRunKind::Initial
    );
    complete_aggregate_validation_run(
        &mut seed,
        &first_rerun,
        "late-broad:first-focused-r2",
        0,
        Vec::new(),
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: first_r2_proof,
    }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:late-broad:first-focused-r2-proof",
    )
    else {
        panic!("the invalidated earlier node must receive a current R2 proof");
    };
    assert_eq!(first_r2_proof.kind, ProofKind::ValidationPassed);
    assert_eq!(
        first_r2_proof.repository_revision,
        repository_revision_after
    );
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:late-broad:first-focused-r2-succeeded",
        ),
        GraphEvent::NodeSucceeded {
            node_id: first_focused.node_id,
            proof_id: first_r2_proof.id,
        }
        .into()
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: required_proof,
    }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:late-broad:required-validation-proof",
    )
    else {
        panic!("all current R2 gates must satisfy required validation");
    };
    assert_eq!(required_proof.kind, ProofKind::RequiredValidationPassed);
    assert_eq!(
        required_proof.repository_revision,
        repository_revision_after
    );
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:late-broad:review-transition",
        ),
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Validation,
            to: ProtocolStage::Review,
            proof_id: required_proof.id,
        }
        .into()
    );
    assert_eq!(
        seed.barrier.phase4.state.position,
        ProtocolPosition::Review(ReviewStep::DiffReview)
    );
    assert_execution_replays_exactly(&seed);
}

#[test]
fn same_target_second_repair_uses_verified_repair_baseline_and_reaches_review() {
    let first_active = active_aggregate_repair_from_seed(
        validation_contract_seed_with_max_runs(3),
        "second-cycle:first",
    );
    let CompletedAggregateRepair {
        mut seed,
        failure: first_failure,
        selection: first_selection,
        verification: first_verification,
        ..
    } = complete_verified_aggregate_repair(first_active, "second-cycle:first");
    let repository_revision_two = first_verification.repository_revision_after.clone();
    assert_execution_replays_exactly(&seed);

    let first_exact_rerun = start_aggregate_validation(&mut seed, "second-cycle:r2-failure");
    assert_eq!(
        first_exact_rerun.request.schedule.gate_id,
        first_failure.gate_id
    );
    assert_eq!(first_exact_rerun.request.schedule.run_attempt, 2);
    assert_eq!(
        first_exact_rerun.request.schedule.repository_revision,
        repository_revision_two
    );
    assert!(matches!(
        first_exact_rerun.request.schedule.kind,
        ValidationRunKind::ExactRepairRerun {
            ref failure_revision_id,
            ref repair_intent_id,
            ref verified_repair_evidence_id,
        } if failure_revision_id == &first_failure.failure_revision_id
            && repair_intent_id == &first_selection.intent.repair_intent_id
            && verified_repair_evidence_id == &first_verification.evidence_id
    ));
    let repair_path = seed.barrier.phase4.accepted_plan.targets[0].path.clone();
    complete_aggregate_validation_run(
        &mut seed,
        &first_exact_rerun,
        "second-cycle:r2-failure",
        1,
        vec![failure_diagnostic(
            repair_path.clone(),
            BTreeSet::from([repair_path.clone()]),
            BTreeSet::new(),
        )],
    );
    let second_active = activate_current_validation_failure(seed, "second-cycle:second");
    let second_failure = second_active
        .seed
        .barrier
        .phase4
        .state
        .validation
        .as_ref()
        .and_then(ValidationState::current_failure)
        .expect("second repair-cycle failure")
        .clone();
    assert_ne!(
        second_failure.failure_revision_id,
        first_failure.failure_revision_id
    );
    assert_eq!(
        second_active.selection.intent.baseline_mutation_evidence_id,
        first_verification.evidence_id
    );
    let current_baselines = current_repair_mutation_baselines(&second_active.seed);
    let second_baseline = current_baselines
        .get(&second_active.selection.intent.target_id)
        .expect("second repair uses the first verified repair as its current baseline");
    assert_eq!(second_baseline.evidence(), &first_verification);
    assert_eq!(
        second_baseline.owner(),
        &RepairMutationBaselineOwner::ValidationRepair {
            node_id: first_selection.repair_node.id.clone(),
            repair_intent_id: first_selection.intent.repair_intent_id.clone(),
            failure_revision_id: first_failure.failure_revision_id.clone(),
            baseline_mutation_evidence_id: first_selection
                .intent
                .baseline_mutation_evidence_id
                .clone(),
        }
    );

    let ProtocolDecision::Perform {
        effect:
            EffectRequest::Validation(ValidationEffectRequest::LoadRepairTargetContext { request }),
    } = decide(&second_active.seed.barrier.phase4.state).unwrap()
    else {
        panic!("second repair must load context from its verified R2 baseline");
    };
    let expected_hash_two = match &first_verification.path_transitions[&repair_path].after {
        MutationPathState::File { content_hash, .. } => content_hash,
        MutationPathState::Absent => panic!("same-target repair baseline remains a file"),
    };
    assert!(
        request
            .path_expectations
            .contains(&TargetPathExpectation::Existing {
                path: repair_path.clone(),
                expected_content_hash: expected_hash_two.clone(),
            })
    );
    assert_eq!(request.repository_revision, repository_revision_two);

    let CompletedAggregateRepair {
        mut seed,
        failure: recorded_second_failure,
        selection: second_selection,
        verification: second_verification,
        rerun: second_rerun,
        ..
    } = complete_verified_aggregate_repair(second_active, "second-cycle:second");
    assert_eq!(recorded_second_failure, second_failure);
    assert_eq!(
        second_selection.intent.baseline_mutation_evidence_id,
        first_verification.evidence_id
    );
    assert_eq!(
        second_verification.repository_revision_before,
        repository_revision_two
    );
    let repository_revision_three = second_verification.repository_revision_after.clone();
    assert_ne!(repository_revision_three, repository_revision_two);
    assert_eq!(
        second_rerun.failure_revision_id,
        second_failure.failure_revision_id
    );
    assert_eq!(second_rerun.repository_revision, repository_revision_three);
    assert_execution_replays_exactly(&seed);

    let final_rerun = start_aggregate_validation(&mut seed, "second-cycle:r3-pass");
    assert_eq!(final_rerun.request.schedule.gate_id, second_failure.gate_id);
    assert_eq!(final_rerun.request.schedule.run_attempt, 3);
    assert_eq!(
        final_rerun.request.schedule.repository_revision,
        repository_revision_three
    );
    assert!(matches!(
        final_rerun.request.schedule.kind,
        ValidationRunKind::ExactRepairRerun {
            ref failure_revision_id,
            ref repair_intent_id,
            ref verified_repair_evidence_id,
        } if failure_revision_id == &second_failure.failure_revision_id
            && repair_intent_id == &second_selection.intent.repair_intent_id
            && verified_repair_evidence_id == &second_verification.evidence_id
    ));
    complete_aggregate_validation_run(
        &mut seed,
        &final_rerun,
        "second-cycle:r3-pass",
        0,
        Vec::new(),
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof: pass_proof }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:second-cycle:r3-pass-proof",
        )
    else {
        panic!("the final exact rerun must produce current validation proof");
    };
    assert_eq!(pass_proof.kind, ProofKind::ValidationPassed);
    assert_eq!(pass_proof.repository_revision, repository_revision_three);
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:second-cycle:validation-node-succeeded",
        ),
        GraphEvent::NodeSucceeded {
            node_id: seed.barrier.validation_node_id.clone(),
            proof_id: pass_proof.id,
        }
        .into()
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: required_proof,
    }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:second-cycle:required-proof",
    )
    else {
        panic!("the second repair's exact pass must satisfy required validation");
    };
    assert_eq!(required_proof.kind, ProofKind::RequiredValidationPassed);
    assert_eq!(
        required_proof.repository_revision,
        repository_revision_three
    );
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:second-cycle:review-transition",
        ),
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Validation,
            to: ProtocolStage::Review,
            proof_id: required_proof.id,
        }
        .into()
    );
    assert_eq!(
        seed.barrier.phase4.state.position,
        ProtocolPosition::Review(ReviewStep::DiffReview)
    );
    assert_execution_replays_exactly(&seed);
}

fn phase7_golden_a_review_entry_seed_from_contract(
    contract_seed: ValidationContractSeed,
) -> Phase7ReviewEntrySeed {
    let mut seed = enter_aggregate_validation(contract_seed);
    let implementation_barrier_proof_id = seed.barrier.barrier_proof_id.clone();
    let run = start_aggregate_validation(&mut seed, "aggregate-pass");
    assert_eq!(run.request, initial_process_request(&seed));
    let completed = ValidationProcessCompleted::new(
        &run.request,
        Some(&run.started),
        175,
        ValidationProcessResult::Exited { exit_code: 0 },
        complete_output(b"test result: ok"),
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:aggregate-pass:completed",
        ValidationEvent::ValidationProcessCompleted {
            completed: completed.clone(),
        },
    );
    assert_eq!(
        seed.barrier.phase4.state.position,
        ProtocolPosition::Validation(ValidationStep::Completed)
    );
    let evidence = ValidationEvidenceV1::from_completed(
        &run.request,
        &run.started,
        &completed,
        ParserConfidence::Exact,
        GateSemanticsObservation::ExpectedSemanticsObserved,
        Vec::new(),
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:aggregate-pass:evidence",
        ValidationEvent::ValidationEvidenceRecorded {
            evidence: evidence.clone(),
        },
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:aggregate-pass:gate-proof",
    ) else {
        panic!("passing gate must produce its canonical proof");
    };
    assert_eq!(proof.kind, ProofKind::ValidationPassed);
    assert_eq!(proof.node_ids, [seed.barrier.validation_node_id.clone()]);
    assert_eq!(
        proof.related_evidence_ids,
        [EvidenceId::new(evidence.evidence_id.as_str())]
    );
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:aggregate-pass:node-succeeded",
        ),
        GraphEvent::NodeSucceeded {
            node_id: seed.barrier.validation_node_id.clone(),
            proof_id: proof.id.clone(),
        }
        .into()
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: required_proof,
    }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:aggregate-pass:required-proof",
    )
    else {
        panic!("all passing gates must produce the required-validation barrier");
    };
    assert_eq!(required_proof.kind, ProofKind::RequiredValidationPassed);
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:aggregate-pass:review-transition",
        ),
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Validation,
            to: ProtocolStage::Review,
            proof_id: required_proof.id.clone(),
        }
        .into()
    );
    assert_eq!(seed.barrier.phase4.state.stage(), ProtocolStage::Review);
    let after_success = seed.barrier.phase4.state.clone();
    assert!(matches!(
        seed.barrier
            .phase4
            .state
            .append_event(run.scheduled_event)
            .expect("exact schedule replay remains idempotent"),
        AppendOutcome::IdempotentReplay { .. }
    ));
    assert_eq!(seed.barrier.phase4.state, after_success);
    let restored = InMemoryEventStore::restore(
        seed.barrier.phase4.trusted_initial.clone(),
        seed.barrier.phase4.state.clone(),
    )
    .expect("passing validation lifecycle restores from the trusted bootstrap")
    .into_state();
    assert_eq!(restored, seed.barrier.phase4.state);

    let review_node_id = required_phase7_node_id(&seed.barrier.phase4.state, NodeKind::Review);
    let completion_node_id =
        required_phase7_node_id(&seed.barrier.phase4.state, NodeKind::CompletionEvaluation);
    let publication_node_id =
        required_phase7_node_id(&seed.barrier.phase4.state, NodeKind::Publication);
    Phase7ReviewEntrySeed {
        trusted_initial: seed.barrier.phase4.trusted_initial,
        state: seed.barrier.phase4.state,
        implementation_barrier_proof_id,
        required_validation_proof_id: required_proof.id,
        current_validation_pass_proof_ids: required_proof.related_proof_ids,
        current_validation_evidence_ids: required_proof.related_evidence_ids,
        repair_ancestry: None,
        review_node_id,
        completion_node_id,
        publication_node_id,
    }
}

pub(super) fn phase7_golden_a_review_entry_seed() -> Phase7ReviewEntrySeed {
    phase7_golden_a_review_entry_seed_from_contract(validation_contract_seed())
}

pub(super) fn phase7_golden_a_review_entry_seed_with_policy(
    policy_for_plan: impl FnOnce(&AcceptedPlan) -> FinalizationPolicyV1,
) -> Phase7ReviewEntrySeed {
    phase7_golden_a_review_entry_seed_from_contract(
        validation_contract_seed_with_finalization_policy(policy_for_plan),
    )
}

#[test]
fn aggregate_passing_gate_reaches_review_with_exact_ownership_and_replay() {
    let seed = phase7_golden_a_review_entry_seed();
    assert!(seed.repair_ancestry.is_none());
    assert_eq!(seed.state.stage(), ProtocolStage::Review);
}

#[test]
fn aggregate_domain_failure_enters_evidence_driven_repair_but_cannot_forge_rerun() {
    let mut seed = enter_aggregate_validation(validation_contract_seed());
    let run = start_aggregate_validation(&mut seed, "aggregate-domain-failure");
    let completed = ValidationProcessCompleted::new(
        &run.request,
        Some(&run.started),
        225,
        ValidationProcessResult::Exited { exit_code: 1 },
        complete_output(b"generic validation assertion failed"),
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:aggregate-domain-failure:completed",
        ValidationEvent::ValidationProcessCompleted {
            completed: completed.clone(),
        },
    );
    let diagnostic = failure_diagnostic(
        seed.barrier.phase4.accepted_plan.targets[0].path.clone(),
        BTreeSet::from([seed.barrier.phase4.accepted_plan.targets[0].path.clone()]),
        BTreeSet::new(),
    );
    let evidence = ValidationEvidenceV1::from_completed(
        &run.request,
        &run.started,
        &completed,
        ParserConfidence::Structured,
        GateSemanticsObservation::ExpectedSemanticsObserved,
        vec![diagnostic],
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:aggregate-domain-failure:evidence",
        ValidationEvent::ValidationEvidenceRecorded {
            evidence: evidence.clone(),
        },
    );
    let DomainEvent::Validation(ValidationEvent::ValidationFailureRevisionRecorded { failure }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:aggregate-domain-failure:failure-revision",
        )
    else {
        panic!("domain failure evidence must create a failure revision");
    };
    assert_eq!(
        seed.barrier.phase4.state.position,
        ProtocolPosition::Validation(ValidationStep::DiagnoseFailure)
    );
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:aggregate-domain-failure:node-failed",
        ),
        GraphEvent::NodeFailed {
            node_id: seed.barrier.validation_node_id.clone(),
            failure_revision_id: failure.failure_revision_id.clone(),
            terminal: false,
        }
        .into()
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:aggregate-domain-failure:proof",
    ) else {
        panic!("failed validation must produce a typed failure proof");
    };
    assert_eq!(proof.kind, ProofKind::ValidationFailure);
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:aggregate-domain-failure:repair-transition",
        ),
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Validation,
            to: ProtocolStage::Repair,
            proof_id: proof.id,
        }
        .into()
    );
    assert_eq!(
        seed.barrier.phase4.state.position,
        ProtocolPosition::Repair(RepairStep::RankCandidates)
    );
    let DomainEvent::Validation(ValidationEvent::RepairCandidatesRanked { ranking }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:aggregate-domain-failure:ranking",
        )
    else {
        panic!("repair must rank evidence-bound candidates");
    };
    assert_eq!(ranking.failure_revision_id, failure.failure_revision_id);
    assert_eq!(
        seed.barrier.phase4.state.position,
        ProtocolPosition::Repair(RepairStep::CheckEligibility)
    );
    let DomainEvent::Validation(ValidationEvent::RepairEligibilityEvaluated { evaluation }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:aggregate-domain-failure:eligibility",
        )
    else {
        panic!("every ranked candidate must receive a persisted eligibility decision");
    };
    assert_eq!(evaluation.decisions.len(), ranking.candidates.len());
    assert_eq!(
        seed.barrier.phase4.state.position,
        ProtocolPosition::Repair(RepairStep::TargetSelected)
    );
    let ProtocolDecision::Emit {
        event: DomainEvent::Validation(ValidationEvent::RepairTargetSelected { selection }),
    } = decide(&seed.barrier.phase4.state).unwrap()
    else {
        panic!("eligible source evidence must select the canonical repair target");
    };
    let mut forged_selection = selection.clone();
    forged_selection.repair_node.id = NodeId::new("node:forged-validation-repair");
    let forged = envelope(
        &seed.barrier.phase4.state,
        "phase6:aggregate-domain-failure:forged-selection",
        ValidationEvent::RepairTargetSelected {
            selection: forged_selection,
        },
    );
    let before_forged = seed.barrier.phase4.state.clone();
    assert!(matches!(
        seed.barrier.phase4.state.append_event(forged),
        Err(ProtocolViolation::ValidationContract {
            code: "repair_selection_not_authoritative"
        })
    ));
    assert_eq!(seed.barrier.phase4.state, before_forged);
    append(
        &mut seed.barrier.phase4.state,
        "phase6:aggregate-domain-failure:selection",
        ValidationEvent::RepairTargetSelected {
            selection: selection.clone(),
        },
    );

    let forged_invalidation = ValidationInvalidation {
        failure_revision_id: failure.failure_revision_id.clone(),
        repair_intent_id: selection.intent.repair_intent_id.clone(),
        repository_revision_before: seed.barrier.phase4.state.repository_revision.clone(),
        repository_revision_after: RepositoryRevisionId::new(
            "repository-revision:phase6-unverified-repair",
        ),
        invalidated_evidence_ids: BTreeSet::from([evidence.evidence_id.clone()]),
        verified_repair_evidence_id: EvidenceId::new("evidence:phase6-forged-verification"),
    };
    let forged = envelope(
        &seed.barrier.phase4.state,
        "phase6:aggregate-domain-failure:forged-invalidation",
        ValidationEvent::PriorValidationInvalidated {
            invalidation: forged_invalidation,
        },
    );
    let before_forged = seed.barrier.phase4.state.clone();
    assert!(matches!(
        seed.barrier.phase4.state.append_event(forged),
        Err(ProtocolViolation::ValidationContract {
            code: "validation_invalidation_outside_verified_repair"
        })
    ));
    assert_eq!(seed.barrier.phase4.state, before_forged);

    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: eligibility_proof,
    }) = append_next_authoritative(
        &mut seed.barrier.phase4.state,
        "phase6:aggregate-domain-failure:eligibility-proof",
    )
    else {
        panic!("selection must produce repair eligibility proof");
    };
    assert_eq!(eligibility_proof.kind, ProofKind::RepairEligibility);
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:aggregate-domain-failure:repair-node-added",
        ),
        GraphEvent::ValidationRepairNodeAdded {
            eligibility_proof_id: eligibility_proof.id,
            node: selection.repair_node.clone(),
        }
        .into()
    );
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:aggregate-domain-failure:repair-node-started",
        ),
        GraphEvent::NodeStarted {
            node_id: selection.repair_node.id.clone(),
            attempt: 1,
        }
        .into()
    );
    assert_eq!(
        seed.barrier.phase4.state.position,
        ProtocolPosition::Repair(RepairStep::ExecuteTarget)
    );
    let ProtocolDecision::Perform {
        effect:
            EffectRequest::Validation(ValidationEffectRequest::LoadRepairTargetContext { request }),
    } = decide(&seed.barrier.phase4.state).unwrap()
    else {
        panic!("active repair must load its exact read-only target context first");
    };
    assert_eq!(request.node_id, selection.repair_node.id);
    assert_eq!(request.node_attempt, 1);
    assert_eq!(request.target_id, selection.intent.target_id);
    assert_eq!(
        request.purpose,
        TargetExecutionPurpose::ValidationRepair {
            repair_intent_id: selection.intent.repair_intent_id.clone(),
            failure_revision_id: failure.failure_revision_id.clone(),
            originating_gate_id: failure.gate_id.clone(),
            validation_evidence_id: failure.validation_evidence_id.clone(),
            baseline_mutation_evidence_id: selection.intent.baseline_mutation_evidence_id.clone(),
        }
    );
    assert_eq!(
        request.repository_revision,
        seed.barrier.phase4.state.repository_revision
    );
    assert!(
        request
            .required_evidence_ids
            .contains(&selection.intent.baseline_mutation_evidence_id)
    );
    let validation = seed.barrier.phase4.state.validation.as_ref().unwrap();
    assert!(validation.invalidations.is_empty());
    assert!(validation.reruns.is_empty());
    assert!(validation.pending_rerun.is_none());
    let restored = InMemoryEventStore::restore(
        seed.barrier.phase4.trusted_initial,
        seed.barrier.phase4.state.clone(),
    )
    .expect("domain failure and repair selection restore exactly")
    .into_state();
    assert_eq!(restored, seed.barrier.phase4.state);
}

#[test]
fn aggregate_validation_infrastructure_failure_converges_to_exact_terminal_result() {
    let mut seed = enter_aggregate_validation(validation_contract_seed());
    let run = start_aggregate_validation(&mut seed, "aggregate-infrastructure");
    let completed = ValidationProcessCompleted::new(
        &run.request,
        Some(&run.started),
        run.request.timeout_ms,
        ValidationProcessResult::InfrastructureFailure {
            kind: ValidationInfrastructureFailureKind::Timeout,
            safe_code: "validation_timeout".into(),
        },
        BoundedProcessOutput {
            stdout: empty_output_stream(),
            stderr: empty_output_stream(),
        },
    )
    .unwrap();
    append(
        &mut seed.barrier.phase4.state,
        "phase6:aggregate-infrastructure:completed",
        ValidationEvent::ValidationProcessCompleted { completed },
    );
    let DomainEvent::Validation(ValidationEvent::ConvergenceEvaluated { convergence }) =
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:aggregate-infrastructure:convergence",
        )
    else {
        panic!("typed process failure must converge before terminal graph failure");
    };
    assert!(matches!(
        convergence.reason,
        ValidationConvergenceReason::InfrastructureFailure {
            kind: ValidationInfrastructureFailureKind::Timeout,
            ..
        }
    ));
    assert_eq!(
        append_next_authoritative(
            &mut seed.barrier.phase4.state,
            "phase6:aggregate-infrastructure:node-failed",
        ),
        GraphEvent::NodeFailed {
            node_id: seed.barrier.validation_node_id.clone(),
            failure_revision_id: convergence.failure_revision_id,
            terminal: true,
        }
        .into()
    );
    let ProtocolDecision::Finish { result } = decide(&seed.barrier.phase4.state).unwrap() else {
        panic!("infrastructure convergence must have one authoritative terminal result");
    };
    assert!(matches!(
        result.mission,
        MissionResult::InfrastructureFailed { .. }
    ));
    assert_eq!(
        result.process_health,
        ProcessHealth::Failed {
            code: "validation_process_timeout".into(),
        }
    );
    assert_eq!(result.reason_code, "validation_process_timeout");
    let restored = InMemoryEventStore::restore(
        seed.barrier.phase4.trusted_initial,
        seed.barrier.phase4.state.clone(),
    )
    .expect("validation infrastructure convergence restores exactly")
    .into_state();
    assert_eq!(restored, seed.barrier.phase4.state);
}

#[test]
fn validation_state_serde_is_strict_stable_and_secret_free() {
    let seed = validation_contract_seed();
    let failed = failed_validation(&seed);
    let mut state = ValidationState::new(
        seed.gates.clone(),
        &seed.policy,
        &seed.barrier.phase4.accepted_plan,
    )
    .unwrap();
    for event in [
        ValidationEvent::ValidationScheduled {
            request: failed.request,
        },
        ValidationEvent::ValidationProcessStarted {
            started: failed.started,
        },
        ValidationEvent::ValidationProcessCompleted {
            completed: failed.completed,
        },
        ValidationEvent::ValidationEvidenceRecorded {
            evidence: failed.evidence,
        },
        ValidationEvent::ValidationFailureRevisionRecorded {
            failure: failed.failure,
        },
    ] {
        state.apply(&event, &seed.policy).unwrap();
    }
    let serialized = serde_json::to_value(&state).unwrap();
    let restored: ValidationState = serde_json::from_value(serialized.clone()).unwrap();
    assert_eq!(restored, state);
    let encoded = serde_json::to_string(&serialized).unwrap();
    assert!(!encoded.contains(VALIDATION_SECRET_SENTINEL));
    assert!(!format!("{state:?}").contains(VALIDATION_SECRET_SENTINEL));

    let mut unknown = serialized;
    unknown.as_object_mut().unwrap().insert(
        "raw_process_output".into(),
        Value::String(VALIDATION_SECRET_SENTINEL.into()),
    );
    assert!(serde_json::from_value::<ValidationState>(unknown).is_err());
}
