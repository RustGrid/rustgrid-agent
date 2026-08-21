use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::*;
use crate::execution_protocol::reducer::repository_profile_proof_hash;

const CONTEXT_EXECUTION_ID: &str = "execution-protocol-v1:phase4-context";
const CONTEXT_REVISION: &str = "repository-revision:phase4-context";
const SECRET_SENTINEL: &str = "rg-phase4-secret-sentinel-9a71e4c2";

#[derive(Clone, Copy)]
pub(super) enum FixtureOperation {
    ModifySmall,
    ModifyLarge,
    Create,
    Delete,
    Move,
}

pub(super) struct ImplementationSeed {
    pub(super) trusted_initial: ExecutionState,
    pub(super) state: ExecutionState,
    pub(super) target_node_id: NodeId,
    pub(super) accepted_plan: AcceptedPlan,
    pub(super) artifacts: BTreeMap<EvidenceId, Vec<u8>>,
}

pub(super) fn target_context_request(state: &ExecutionState) -> TargetContextLoadRequest {
    let ProtocolDecision::Perform {
        effect:
            EffectRequest::Implementation(ImplementationEffectRequest::LoadTargetContext { request }),
    } = decide(state).expect("read-only target-context load decision")
    else {
        panic!("active implementation target must request its bounded context");
    };
    *request
}

pub(super) fn fixture_bytes(path: &ProfilePath) -> Vec<u8> {
    fs::read(fixture_root().join(path.as_str())).expect("read operation-owned fixture path")
}

fn artifact_reference_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(super) fn materialized_context(
    seed: &ImplementationSeed,
    request: &TargetContextLoadRequest,
) -> MaterializedTargetContext {
    let path_states = request
        .path_expectations
        .iter()
        .map(|expectation| match expectation {
            TargetPathExpectation::Existing { path, .. } => {
                let bytes = fixture_bytes(path);
                LoadedPathState::Existing {
                    path: path.clone(),
                    content: LoadedContextArtifact::new(
                        artifact_reference_hash(&bytes),
                        ArtifactScope::FullFile,
                        bytes,
                    )
                    .expect("full target probe artifact"),
                }
            }
            TargetPathExpectation::Absent { path } => {
                LoadedPathState::Absent { path: path.clone() }
            }
        })
        .collect::<Vec<_>>();
    let evidence_artifacts = request
        .artifact_requirements
        .iter()
        .map(|requirement| {
            let bytes = seed
                .artifacts
                .get(&requirement.evidence_id)
                .expect("fixture artifact required by authoritative request")
                .clone();
            let scope = if hex::encode(Sha256::digest(&bytes)) == requirement.source_content_hash {
                ArtifactScope::FullFile
            } else {
                ArtifactScope::ExactRange {
                    line_range: requirement.line_range.clone(),
                    source_content_hash: requirement.source_content_hash.clone(),
                }
            };
            (
                requirement.evidence_id.clone(),
                LoadedContextArtifact::new(
                    requirement.artifact_reference_hash.clone(),
                    scope,
                    bytes,
                )
                .expect("content-addressed fixture artifact"),
            )
        })
        .collect::<BTreeMap<_, _>>();
    MaterializedTargetContext {
        request_id: request.request_id.clone(),
        repository_revision: request.repository_revision.clone(),
        repository_fingerprint: stable_sha256(&[
            "execution-protocol-v1:phase4-repository-fingerprint",
            request.repository_revision.as_str(),
        ]),
        path_states,
        evidence_artifacts,
    }
}

fn recompute_request_id(request: &TargetContextLoadRequest) -> EffectId {
    #[derive(Serialize)]
    struct RequestIdentity<'a> {
        schema_version: u16,
        execution_id: &'a ExecutionId,
        execution_attempt: u32,
        node_id: &'a NodeId,
        node_attempt: u32,
        target_id: &'a TargetId,
        purpose: &'a TargetExecutionPurpose,
        plan_id: &'a PlanId,
        plan_revision_id: &'a PlanRevisionId,
        repository_revision: &'a RepositoryRevisionId,
        goal_hash: &'a str,
        criterion_ids: &'a BTreeSet<DiscoveryCriterionId>,
        path_expectations: &'a BTreeSet<TargetPathExpectation>,
        required_evidence_ids: &'a BTreeSet<EvidenceId>,
        optional_evidence_ids: &'a BTreeSet<EvidenceId>,
        artifact_requirements: &'a [EvidenceArtifactRequirement],
        validation_expectation_ids: &'a BTreeSet<ValidationExpectationId>,
        input_token_ceiling: u32,
    }
    let canonical = serde_json::to_string(&RequestIdentity {
        schema_version: request.schema_version,
        execution_id: &request.execution_id,
        execution_attempt: request.execution_attempt,
        node_id: &request.node_id,
        node_attempt: request.node_attempt,
        target_id: &request.target_id,
        purpose: &request.purpose,
        plan_id: &request.plan_id,
        plan_revision_id: &request.plan_revision_id,
        repository_revision: &request.repository_revision,
        goal_hash: &request.goal_hash,
        criterion_ids: &request.criterion_ids,
        path_expectations: &request.path_expectations,
        required_evidence_ids: &request.required_evidence_ids,
        optional_evidence_ids: &request.optional_evidence_ids,
        artifact_requirements: &request.artifact_requirements,
        validation_expectation_ids: &request.validation_expectation_ids,
        input_token_ceiling: request.input_token_ceiling,
    })
    .expect("serialize canonical test request identity");
    EffectId::new(format!(
        "epv1:{}",
        stable_sha256(&["execution-protocol-v1:target-context-load", &canonical])
    ))
}

fn fixture_tree_bytes() -> BTreeMap<String, Vec<u8>> {
    let root = fixture_root();
    let mut paths = Vec::new();
    collect_fixture_files(&root, &root, &mut paths);
    paths
        .into_iter()
        .map(|path| {
            (
                relative_fixture_path(&root, &path),
                fs::read(path).expect("read immutable fixture tree"),
            )
        })
        .collect()
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/execution_protocol_v1/implementation_context/repository")
}

fn collect_fixture_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(directory)
        .expect("read implementation-context fixture directory")
        .map(|entry| entry.expect("read fixture entry").path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_fixture_files(root, &path, files);
        } else {
            assert!(path.starts_with(root));
            files.push(path);
        }
    }
}

fn relative_fixture_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .expect("fixture path remains below fixture root")
        .iter()
        .map(|component| component.to_str().expect("UTF-8 fixture path"))
        .collect::<Vec<_>>()
        .join("/")
}

fn fixture_profile() -> RepositoryProfile {
    let root = fixture_root();
    let mut paths = Vec::new();
    collect_fixture_files(&root, &root, &mut paths);
    let observations = paths
        .into_iter()
        .map(|path| {
            RepositoryFileObservation::from_bytes(
                relative_fixture_path(&root, &path),
                fs::read(path).expect("read implementation-context fixture file"),
            )
            .expect("bounded implementation-context observation")
        })
        .collect();
    build_repository_profile(
        &RepositoryInventory::new(RepositoryRevisionId::new(CONTEXT_REVISION), observations)
            .expect("valid implementation-context inventory"),
    )
    .expect("deterministic implementation-context profile")
}

fn context_graph_budget(input_token_ceiling: u32) -> PlanGraphBudgetContract {
    let mut budget = super::plan_graph_budget();
    budget.implementation.max_input_tokens_per_call = input_token_ceiling;
    budget
}

fn append_decision_event(state: &mut ExecutionState, semantic_key: &str) -> DomainEvent {
    let decision = decide(state).expect("authoritative protocol decision");
    let ProtocolDecision::Emit { event } = decision else {
        panic!("expected an authoritative event decision for {semantic_key}, got {decision:?}");
    };
    append(state, semantic_key, event.clone());
    event
}

fn consume_discovery_action(state: &mut ExecutionState, label: &str) -> PreparedDiscoveryAction {
    let DomainEvent::Discovery(DiscoveryEvent::ActionPrepared { prepared }) =
        append_decision_event(state, &format!("phase4:{label}:prepared"))
    else {
        panic!("discovery must prepare an action");
    };
    let prepared = *prepared;
    assert_eq!(
        append_decision_event(state, &format!("phase4:{label}:admitted")),
        BudgetEvent::ModelCallAdmitted {
            admission: prepared.admission.clone(),
        }
        .into()
    );
    assert_eq!(
        append_decision_event(state, &format!("phase4:{label}:reserved")),
        BudgetEvent::ModelCallReserved {
            call_id: prepared.admission.call_id.clone(),
        }
        .into()
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Discovery(DiscoveryEffectRequest::DispatchProvider { envelope }),
    } = decide(state).expect("discovery provider dispatch")
    else {
        panic!("discovery reservation must dispatch its exact provider request");
    };
    assert_eq!(*envelope, prepared.envelope);
    append(
        state,
        &format!("phase4:{label}:dispatch-started"),
        BudgetEvent::ProviderDispatchStarted {
            call_id: prepared.admission.call_id.clone(),
            payload_hash: prepared.envelope.payload_identity.clone(),
        },
    );
    append(
        state,
        &format!("phase4:{label}:reconciled"),
        BudgetEvent::ModelCallReconciled {
            call_id: prepared.admission.call_id.clone(),
            result: ModelCallReconciliation::Consumed {
                actual_cost_micros: 80,
                duration_ms: 50,
            },
        },
    );
    prepared
}

fn consume_planning_action(state: &mut ExecutionState) -> PreparedPlanningAction {
    let DomainEvent::Planning(PlanningEvent::ActionPrepared { prepared }) =
        append_decision_event(state, "phase4:planning:prepared")
    else {
        panic!("planning must prepare an action");
    };
    let prepared = *prepared;
    assert_eq!(
        append_decision_event(state, "phase4:planning:admitted"),
        BudgetEvent::ModelCallAdmitted {
            admission: prepared.admission.clone(),
        }
        .into()
    );
    assert_eq!(
        append_decision_event(state, "phase4:planning:reserved"),
        BudgetEvent::ModelCallReserved {
            call_id: prepared.admission.call_id.clone(),
        }
        .into()
    );
    let ProtocolDecision::Perform {
        effect: EffectRequest::Planning(PlanningEffectRequest::DispatchProvider { envelope }),
    } = decide(state).expect("planning provider dispatch")
    else {
        panic!("planning reservation must dispatch its exact provider request");
    };
    assert_eq!(*envelope, prepared.envelope);
    append(
        state,
        "phase4:planning:dispatch-started",
        BudgetEvent::ProviderDispatchStarted {
            call_id: prepared.admission.call_id.clone(),
            payload_hash: prepared.envelope.payload_identity.clone(),
        },
    );
    append(
        state,
        "phase4:planning:reconciled",
        BudgetEvent::ModelCallReconciled {
            call_id: prepared.admission.call_id.clone(),
            result: ModelCallReconciliation::Consumed {
                actual_cost_micros: 80,
                duration_ms: 50,
            },
        },
    );
    prepared
}

fn fixture_evidence(node_id: &NodeId, path: &DiscoveryPath) -> Vec<(FileEvidence, Vec<u8>)> {
    let bytes = fs::read(fixture_root().join(path.as_str())).expect("read target fixture bytes");
    let line_count = u32::try_from(bytes.split(|byte| *byte == b'\n').count())
        .unwrap_or(u32::MAX)
        .max(1);
    let content_hash = hex::encode(Sha256::digest(&bytes));
    let use_exact_range = path.as_str() == "src/large_target.rs";
    let line_range = if use_exact_range {
        LineRange::new(2, 2).expect("one-line exact range")
    } else {
        LineRange::new(1, line_count).expect("bounded fixture line range")
    };
    let artifact_bytes = if use_exact_range {
        bytes
            .split_inclusive(|byte| *byte == b'\n')
            .nth(1)
            .expect("large fixture target has a second line")
            .to_vec()
    } else {
        bytes
    };
    let artifact_reference_hash = artifact_reference_hash(&artifact_bytes);
    let evidence = FileEvidence::new(
        node_id.clone(),
        RepositoryRevisionId::new(CONTEXT_REVISION),
        path.clone(),
        line_range,
        content_hash,
        artifact_reference_hash,
        TextEncoding::Utf8,
        false,
    )
    .expect("canonical fixture evidence");
    vec![(evidence, artifact_bytes)]
}

pub(super) fn implementation_seed(
    fixture_operation: FixtureOperation,
    input_token_ceiling: u32,
) -> ImplementationSeed {
    implementation_seed_with_validation_commands(
        fixture_operation,
        input_token_ceiling,
        BTreeSet::from([ValidationCommandKind::CargoTest]),
    )
}

pub(super) fn implementation_seed_with_validation_commands(
    fixture_operation: FixtureOperation,
    input_token_ceiling: u32,
    validation_commands: BTreeSet<ValidationCommandKind>,
) -> ImplementationSeed {
    implementation_seed_with_validation_commands_and_graph_budget(
        fixture_operation,
        input_token_ceiling,
        validation_commands,
        |_| {},
    )
}

pub(super) fn implementation_seed_with_validation_commands_and_graph_budget(
    fixture_operation: FixtureOperation,
    input_token_ceiling: u32,
    validation_commands: BTreeSet<ValidationCommandKind>,
    configure_graph_budget: impl FnOnce(&mut PlanGraphBudgetContract),
) -> ImplementationSeed {
    let mut graph_budget = context_graph_budget(input_token_ceiling);
    configure_graph_budget(&mut graph_budget);
    graph_budget
        .validate()
        .expect("configured implementation fixture graph budget");
    let mut state = ExecutionState::bootstrap(
        ExecutionId::new(CONTEXT_EXECUTION_ID),
        1,
        RepositoryRevisionId::new(CONTEXT_REVISION),
        mission_budget(10),
        model_budget(2),
        model_budget(2),
        graph_budget.clone(),
        None,
    );
    let trusted_initial = state.clone();
    let profile = fixture_profile();
    append(
        &mut state,
        "phase4:profile:recorded",
        ProfileEvent::RepositoryProfileRecorded {
            profile: profile.clone(),
        },
    );

    let criterion_id =
        DiscoveryCriterionId::new("criterion:bounded-target-context").expect("criterion identity");
    let goal = DiscoveryGoal::new(
        stable_sha256(&["execution-protocol-v1:phase4-context-goal"]),
        BTreeSet::from([criterion_id.clone()]),
        ["bounded target context".to_owned()],
    )
    .expect("bounded implementation-context goal");
    append(
        &mut state,
        "phase4:discovery:goal",
        DiscoveryEvent::GoalRecorded { goal },
    );
    let profile_proof_id = ProofId::new("proof:phase4:repository-profile");
    let repository_revision = state.repository_revision.clone();
    append(
        &mut state,
        "phase4:profile:proof",
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
        "phase4:profile:advance",
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Profiling,
            to: ProtocolStage::Discovery,
            proof_id: profile_proof_id,
        },
    );
    assert!(matches!(
        append_decision_event(&mut state, "phase4:discovery:start"),
        DomainEvent::Graph(GraphEvent::NodeStarted { .. })
    ));

    let search = consume_discovery_action(&mut state, "discovery:search");
    let DiscoveryActionConstraints::Search { request } = &search.envelope.constraints else {
        panic!("initial discovery action must search");
    };
    let discovery_path = match fixture_operation {
        FixtureOperation::ModifyLarge => "src/large_target.rs",
        FixtureOperation::Delete | FixtureOperation::Move => "src/move_source.rs",
        FixtureOperation::ModifySmall | FixtureOperation::Create => "src/small_target.rs",
    };
    let searched_paths = [discovery_path]
        .into_iter()
        .map(|path| DiscoveryPath::new(path).expect("valid fixture discovery path"))
        .collect::<BTreeSet<_>>();
    append(
        &mut state,
        "phase4:discovery:search-observed",
        DiscoveryEvent::SearchCompleted {
            action_id: search.envelope.action_id.clone(),
            evidence: SearchEvidence::new(
                NodeId::new("protocol-v1:discovery"),
                request.clone(),
                searched_paths,
                false,
            )
            .expect("canonical fixture search evidence"),
        },
    );
    assert!(matches!(
        append_decision_event(&mut state, "phase4:discovery:candidates"),
        DomainEvent::Discovery(DiscoveryEvent::CandidatesRecorded { .. })
    ));

    let grounding = consume_discovery_action(&mut state, "discovery:ground");
    let DiscoveryActionConstraints::ExactPaths { paths } = &grounding.envelope.constraints else {
        panic!("final discovery action must ground exact paths");
    };
    let mut artifacts = BTreeMap::new();
    let mut evidence = Vec::new();
    for path in paths {
        for (file_evidence, bytes) in fixture_evidence(&NodeId::new("protocol-v1:discovery"), path)
        {
            artifacts.insert(file_evidence.evidence_id.clone(), bytes);
            evidence.push(file_evidence);
        }
    }
    append(
        &mut state,
        "phase4:discovery:files-observed",
        DiscoveryEvent::FileEvidenceRecorded {
            action_id: grounding.envelope.action_id.clone(),
            evidence,
            unresolved_questions: Vec::new(),
        },
    );
    for key in [
        "phase4:discovery:convergence",
        "phase4:discovery:impact-map",
        "phase4:discovery:impact-map-accepted",
        "phase4:discovery:impact-proof",
        "phase4:discovery:succeeded",
        "phase4:discovery:planning-transition",
    ] {
        append_decision_event(&mut state, key);
    }
    assert_eq!(state.stage(), ProtocolStage::Planning);
    assert!(matches!(
        append_decision_event(&mut state, "phase4:planning:start"),
        DomainEvent::Graph(GraphEvent::NodeStarted { .. })
    ));
    let planning_action = consume_planning_action(&mut state);

    let discovery = state.discovery.as_ref().expect("authoritative discovery");
    let (path, explicit_operation, required_path) = match fixture_operation {
        FixtureOperation::ModifySmall => ("src/small_target.rs", None, "src/small_target.rs"),
        FixtureOperation::ModifyLarge => ("src/large_target.rs", None, "src/large_target.rs"),
        FixtureOperation::Create => (
            "src/created_target.rs",
            Some(TargetOperation::CreateFile {
                specification: CreationSpecification::new(
                    CreatedFileKind::Source,
                    "Create the bounded target-context fixture output",
                )
                .expect("creation specification"),
            }),
            "src/small_target.rs",
        ),
        FixtureOperation::Delete => ("src/move_source.rs", None, "src/move_source.rs"),
        FixtureOperation::Move => ("src/move_source.rs", None, "src/move_source.rs"),
    };
    let mut matching_evidence = discovery
        .file_evidence
        .values()
        .filter(|evidence| evidence.path.as_str() == required_path)
        .cloned()
        .collect::<Vec<_>>();
    matching_evidence.sort_by_key(|evidence| {
        evidence
            .line_range
            .end_inclusive
            .saturating_sub(evidence.line_range.start)
    });
    let required_file = if matches!(fixture_operation, FixtureOperation::ModifyLarge) {
        matching_evidence.first()
    } else {
        matching_evidence.last()
    }
    .expect("required fixture evidence")
    .clone();
    let operation = explicit_operation.unwrap_or_else(|| match fixture_operation {
        FixtureOperation::Delete => TargetOperation::DeleteFile {
            expected_content_hash: required_file.content_hash.clone(),
        },
        FixtureOperation::Move => TargetOperation::MoveFile {
            destination: ProfilePath::new("src/moved_target.rs").expect("move destination"),
            expected_content_hash: required_file.content_hash.clone(),
        },
        FixtureOperation::ModifySmall | FixtureOperation::ModifyLarge => {
            TargetOperation::ModifyExisting {
                expected_content_hash: required_file.content_hash.clone(),
            }
        }
        FixtureOperation::Create => unreachable!("create operation is explicit"),
    });
    assert!(!validation_commands.is_empty());
    let validation = validation_commands
        .into_iter()
        .map(|command| {
            let candidate = profile
                .validation_candidates
                .iter()
                .find(|candidate| candidate.command == command)
                .expect("fixture profile provides requested validation command");
            ValidationExpectation::new(
                candidate.candidate_id.clone(),
                BTreeSet::from([criterion_id.clone()]),
            )
            .expect("profile-bound validation expectation")
        })
        .collect();
    let target = PlannedTargetV1 {
        target_id: TargetId::new("target:phase4-context"),
        change_id: ChangeId::new("change:phase4-context"),
        path: ProfilePath::new(path).expect("exact fixture target"),
        operation,
        role: TargetRole::Source,
        rationale: "Exercise one bounded target-local implementation context".into(),
        acceptance_criteria: BTreeSet::from([criterion_id]),
        required_evidence: BTreeSet::from([required_file.evidence_id.clone()]),
        expected_validation: validation,
        dependencies: BTreeSet::new(),
        estimated_change: ChangeEstimate {
            size: ChangeSize::Small,
            risk: ChangeRisk::Low,
            estimated_changed_lines: 8,
        },
    };
    let planning = state.planning.as_ref().expect("planning projection");
    let candidate = PlanCandidate::new(
        planning.next_revision_index(),
        state.repository_revision.clone(),
        planning.discovery_impact_map_id.clone(),
        PlanDecisionCandidate::Changes {
            targets: vec![target],
        },
    )
    .expect("typed implementation-context plan candidate");
    append(
        &mut state,
        "phase4:planning:candidate",
        PlanningEvent::CandidateRecorded {
            action_id: planning_action.envelope.action_id,
            call_id: planning_action.admission.call_id,
            candidate,
        },
    );
    for key in [
        "phase4:planning:proof",
        "phase4:planning:graph",
        "phase4:planning:succeeded",
        "phase4:planning:implementation-transition",
    ] {
        append_decision_event(&mut state, key);
    }
    assert_eq!(state.stage(), ProtocolStage::Implementation);
    let accepted_plan = state
        .planning
        .as_ref()
        .and_then(|planning| planning.accepted_plan.clone())
        .expect("accepted implementation-context plan");
    let materialized =
        materialize_accepted_plan(&accepted_plan, &graph_budget).expect("materialized plan");
    let target_node_id = materialized
        .target_nodes
        .values()
        .next()
        .expect("one implementation node")
        .clone();
    assert_eq!(
        append_decision_event(&mut state, "phase4:implementation:start"),
        GraphEvent::NodeStarted {
            node_id: target_node_id.clone(),
            attempt: 1,
        }
        .into()
    );

    ImplementationSeed {
        trusted_initial,
        state,
        target_node_id,
        accepted_plan,
        artifacts,
    }
}

#[test]
fn implementation_context_fixture_builds_a_real_accepted_plan() {
    let seed = implementation_seed(FixtureOperation::ModifySmall, 4_096);
    assert_eq!(seed.state.stage(), ProtocolStage::Implementation);
    assert!(matches!(
        seed.state
            .node(&seed.target_node_id)
            .map(|node| &node.state),
        Some(NodeState::Active { attempt: 1 })
    ));
    assert_eq!(seed.accepted_plan.targets.len(), 1);
    assert!(!seed.artifacts.is_empty());
    InMemoryEventStore::restore(seed.trusted_initial, seed.state)
        .expect("real accepted-plan seed replays from its trusted bootstrap");
}

#[test]
fn active_target_loads_exact_read_only_context_and_stops_before_provider_mutation() {
    let mut seed = implementation_seed(FixtureOperation::ModifySmall, 4_096);
    let tree_before = fixture_tree_bytes();
    let repository_revision_before = seed.state.repository_revision.clone();
    let budget_before = seed.state.budgets.clone();
    let request = target_context_request(&seed.state);
    assert_eq!(request.node_id, seed.target_node_id);
    assert_eq!(request.node_attempt, 1);
    assert_eq!(request.plan_id, seed.accepted_plan.plan_id);
    assert_eq!(
        request.plan_revision_id,
        seed.accepted_plan.plan_revision_id
    );
    assert_eq!(request.repository_revision, repository_revision_before);
    assert_eq!(request.input_token_ceiling, 4_096);
    assert_eq!(request.required_evidence_ids.len(), 1);
    assert!(request.optional_evidence_ids.is_empty());
    assert_eq!(
        request.path_expectations,
        BTreeSet::from([TargetPathExpectation::Existing {
            path: ProfilePath::new("src/small_target.rs").unwrap(),
            expected_content_hash: hex::encode(Sha256::digest(fixture_bytes(
                &ProfilePath::new("src/small_target.rs").unwrap(),
            ))),
        }])
    );

    let materialized = materialized_context(&seed, &request);
    let prepared =
        prepare_target_context(&request, &materialized).expect("bounded full-file target context");
    assert_eq!(prepared.node_id, seed.target_node_id);
    assert_eq!(prepared.manifest.input_token_ceiling, 4_096);
    assert!(prepared.manifest.estimated_input_tokens <= 4_096);
    assert_eq!(prepared.manifest.materialized_context_hash.len(), 64);
    assert!(matches!(
        prepared.manifest.target_content,
        TargetContentSelection::FullFile { .. }
    ));
    let serialized_prepared = serde_json::to_string(&prepared).expect("serialize safe manifest");
    for forbidden_key in [
        "allowed_tools",
        "tool_choice",
        "output_token_allowance",
        "reservation_id",
        "provider_payload",
    ] {
        assert!(!serialized_prepared.contains(forbidden_key));
    }
    assert!(!serialized_prepared.contains("value + 1"));

    let prepared_event = append(
        &mut seed.state,
        "phase4:implementation:context-prepared",
        ImplementationEvent::TargetContextPrepared {
            prepared: Box::new(prepared.clone()),
        },
    );
    let node = seed
        .state
        .node(&seed.target_node_id)
        .expect("active implementation node");
    let target = seed.accepted_plan.targets.first().expect("one plan target");
    let feasibility = evaluate_mutation_feasibility(node, target, &prepared.manifest)
        .expect("first reducer-owned Phase 5 projection");
    assert_eq!(
        decide(&seed.state).expect("context-ready boundary decision"),
        ProtocolDecision::Emit {
            event: MutationEvent::FeasibilityEvaluated { feasibility }.into(),
        }
    );
    assert_eq!(seed.state.repository_revision, repository_revision_before);
    assert_eq!(seed.state.budgets, budget_before);
    assert!(matches!(
        seed.state
            .node(&seed.target_node_id)
            .map(|node| &node.state),
        Some(NodeState::Active { attempt: 1 })
    ));
    assert!(
        seed.state
            .proofs
            .values()
            .all(|proof| proof.kind != ProofKind::MutationVerified)
    );
    assert_eq!(fixture_tree_bytes(), tree_before);

    let after_event = seed.state.clone();
    assert!(matches!(
        seed.state
            .append_event(prepared_event)
            .expect("exact context event replay"),
        AppendOutcome::IdempotentReplay { .. }
    ));
    assert_eq!(seed.state, after_event);
    let restored = InMemoryEventStore::restore(seed.trusted_initial, seed.state.clone())
        .expect("prepared target context replays from trusted events")
        .into_state();
    assert_eq!(restored, seed.state);
}

#[test]
fn rolling_revision_context_rebuild_preserves_history_and_base_plan_binding() {
    let seed = implementation_seed(FixtureOperation::ModifySmall, 4_096);
    let initial_revision = seed.state.initial_repository_revision.clone();
    let initial_request = target_context_request(&seed.state);
    let initial_prepared = prepare_target_context(
        &initial_request,
        &materialized_context(&seed, &initial_request),
    )
    .expect("initial target context");

    assert!(matches!(
        TargetContextSupersession::new(&initial_prepared, initial_revision.clone()),
        Err(TargetContextContractError::Invalid {
            code: "target_context_supersession_invalid"
        })
    ));

    let replacement_revision = RepositoryRevisionId::new("repository-revision:phase5-next");
    let node = seed
        .state
        .node(&seed.target_node_id)
        .expect("active implementation node");
    let discovery = seed.state.discovery.as_ref().expect("discovery projection");
    let replacement_request = build_target_context_load_request(
        &seed.state.execution_id,
        seed.state.execution_attempt,
        &replacement_revision,
        node,
        &seed.accepted_plan,
        discovery,
    )
    .expect("current revision may differ from the base-bound plan");
    let replacement_prepared = prepare_target_context(
        &replacement_request,
        &materialized_context(&seed, &replacement_request),
    )
    .expect("replacement target context");
    assert_ne!(
        initial_prepared.context_manifest_id,
        replacement_prepared.context_manifest_id
    );

    let mut implementation = ImplementationState::new(&seed.accepted_plan).unwrap();
    implementation
        .record_prepared_context(initial_prepared.clone())
        .expect("record initial context");
    let supersession =
        TargetContextSupersession::new(&initial_prepared, replacement_revision.clone())
            .expect("deterministic supersession");
    assert_eq!(
        supersession,
        TargetContextSupersession::new(&initial_prepared, replacement_revision.clone()).unwrap()
    );
    implementation
        .supersede_context(supersession.clone())
        .expect("supersede current context without deleting it");
    implementation
        .record_prepared_context(replacement_prepared.clone())
        .expect("record replacement context");

    assert_eq!(implementation.repository_revision, initial_revision);
    assert_eq!(seed.accepted_plan.repository_revision, initial_revision);
    assert_eq!(discovery.repository_revision, initial_revision);
    assert_eq!(implementation.prepared_contexts.len(), 2);
    assert_eq!(
        implementation.prepared_context(&initial_prepared.context_manifest_id),
        Some(&initial_prepared)
    );
    assert_eq!(
        implementation
            .superseded_contexts
            .get(&initial_prepared.context_manifest_id),
        Some(&supersession)
    );
    assert_eq!(
        implementation.prepared_context_for_node(&seed.target_node_id),
        Some(&replacement_prepared)
    );
    implementation
        .validate(&seed.accepted_plan)
        .expect("history and latest-context pointer remain canonical");
}

#[test]
fn target_probe_paths_are_owned_exactly_by_the_typed_operation() {
    let cases = [
        (
            FixtureOperation::ModifySmall,
            BTreeSet::from([TargetPathExpectation::Existing {
                path: ProfilePath::new("src/small_target.rs").unwrap(),
                expected_content_hash: hex::encode(Sha256::digest(fixture_bytes(
                    &ProfilePath::new("src/small_target.rs").unwrap(),
                ))),
            }]),
        ),
        (
            FixtureOperation::Create,
            BTreeSet::from([TargetPathExpectation::Absent {
                path: ProfilePath::new("src/created_target.rs").unwrap(),
            }]),
        ),
        (
            FixtureOperation::Delete,
            BTreeSet::from([TargetPathExpectation::Existing {
                path: ProfilePath::new("src/move_source.rs").unwrap(),
                expected_content_hash: hex::encode(Sha256::digest(fixture_bytes(
                    &ProfilePath::new("src/move_source.rs").unwrap(),
                ))),
            }]),
        ),
        (
            FixtureOperation::Move,
            BTreeSet::from([
                TargetPathExpectation::Existing {
                    path: ProfilePath::new("src/move_source.rs").unwrap(),
                    expected_content_hash: hex::encode(Sha256::digest(fixture_bytes(
                        &ProfilePath::new("src/move_source.rs").unwrap(),
                    ))),
                },
                TargetPathExpectation::Absent {
                    path: ProfilePath::new("src/moved_target.rs").unwrap(),
                },
            ]),
        ),
    ];
    for (operation, expected_paths) in cases {
        let seed = implementation_seed(operation, 4_096);
        let request = target_context_request(&seed.state);
        assert_eq!(request.path_expectations, expected_paths);
        let serialized = serde_json::to_value(&request).expect("serialize read-only request");
        let object = serialized.as_object().expect("request JSON object");
        for forbidden_field in [
            "allowed_tools",
            "tool_choice",
            "output_token_allowance",
            "reservation_id",
        ] {
            assert!(!object.contains_key(forbidden_field));
        }
    }
}

#[test]
fn full_file_and_exact_range_contexts_have_stable_distinct_identities() {
    let small_a = implementation_seed(FixtureOperation::ModifySmall, 4_096);
    let small_request_a = target_context_request(&small_a.state);
    let small_prepared_a = prepare_target_context(
        &small_request_a,
        &materialized_context(&small_a, &small_request_a),
    )
    .expect("small target full context");
    let small_b = implementation_seed(FixtureOperation::ModifySmall, 4_096);
    let small_request_b = target_context_request(&small_b.state);
    let small_prepared_b = prepare_target_context(
        &small_request_b,
        &materialized_context(&small_b, &small_request_b),
    )
    .expect("repeated small target context");
    assert_eq!(small_request_a, small_request_b);
    assert_eq!(small_prepared_a, small_prepared_b);
    assert!(matches!(
        small_prepared_a.manifest.target_content,
        TargetContentSelection::FullFile { .. }
    ));

    let large = implementation_seed(FixtureOperation::ModifyLarge, 1_500);
    let large_request = target_context_request(&large.state);
    let large_prepared = prepare_target_context(
        &large_request,
        &materialized_context(&large, &large_request),
    )
    .expect("large target uses its exact grounded range");
    let TargetContentSelection::ExactRanges { artifacts } = &large_prepared.manifest.target_content
    else {
        panic!("large target must retain exact ranges instead of truncating a full file");
    };
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].line_range, Some(LineRange::new(2, 2).unwrap()));
    assert!(large_prepared.manifest.compaction.iter().any(|decision| {
        decision.kind == TargetContextCompactionKind::BoundedRange
            && decision.original_estimated_tokens > decision.retained_estimated_tokens
    }));
    assert_ne!(
        small_prepared_a.manifest.materialized_context_hash,
        large_prepared.manifest.materialized_context_hash
    );
}

#[test]
fn optional_evidence_is_omitted_deterministically_at_the_signed_ceiling() {
    let mut seed = implementation_seed(FixtureOperation::ModifySmall, 4_096);
    let base_request = target_context_request(&seed.state);
    let base_prepared =
        prepare_target_context(&base_request, &materialized_context(&seed, &base_request))
            .expect("base mandatory context");

    let neighbor_path = DiscoveryPath::new("src/neighbor.rs").unwrap();
    let (neighbor_evidence, neighbor_bytes) =
        fixture_evidence(&NodeId::new("protocol-v1:discovery"), &neighbor_path)
            .pop()
            .expect("neighbor fixture evidence");
    seed.artifacts
        .insert(neighbor_evidence.evidence_id.clone(), neighbor_bytes);
    let mut request = base_request;
    request.input_token_ceiling = base_prepared.manifest.estimated_input_tokens;
    request
        .optional_evidence_ids
        .insert(neighbor_evidence.evidence_id.clone());
    request
        .artifact_requirements
        .push(EvidenceArtifactRequirement {
            evidence_id: neighbor_evidence.evidence_id.clone(),
            path: ProfilePath::new(neighbor_evidence.path.as_str()).unwrap(),
            line_range: neighbor_evidence.line_range,
            source_content_hash: neighbor_evidence.content_hash,
            artifact_reference_hash: neighbor_evidence.artifact_reference_hash,
            encoding: neighbor_evidence.encoding,
            truncated: neighbor_evidence.truncated,
            mandatory: false,
        });
    request.artifact_requirements.sort_by(|left, right| {
        left.evidence_id
            .cmp(&right.evidence_id)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.line_range.start.cmp(&right.line_range.start))
            .then_with(|| {
                left.line_range
                    .end_inclusive
                    .cmp(&right.line_range.end_inclusive)
            })
    });
    request.request_id = recompute_request_id(&request);
    request
        .validate()
        .expect("canonical optional-evidence request");

    let materialized = materialized_context(&seed, &request);
    let first = prepare_target_context(&request, &materialized)
        .expect("mandatory content fits while optional evidence is compacted");
    let second = prepare_target_context(&request, &materialized)
        .expect("repeated compaction is deterministic");
    assert_eq!(first, second);
    assert!(first.manifest.selected_optional_evidence_ids.is_empty());
    assert!(first.manifest.optional_sections.is_empty());
    assert!(first.manifest.compaction.iter().any(|decision| {
        decision.kind == TargetContextCompactionKind::OmittedOptional
            && decision.section
                == TargetContextSection::Evidence {
                    evidence_id: neighbor_evidence.evidence_id.clone(),
                }
    }));
}

#[test]
fn mandatory_context_overflow_is_typed_and_does_not_change_state() {
    let seed = implementation_seed(FixtureOperation::ModifyLarge, 1);
    let state_before = seed.state.clone();
    let request = target_context_request(&seed.state);
    let materialized = materialized_context(&seed, &request);
    let error = prepare_target_context(&request, &materialized)
        .expect_err("mandatory target context cannot exceed its signed ceiling");
    assert!(matches!(
        &error,
        TargetContextContractError::MandatoryContextTooLarge {
            required_tokens,
            input_token_ceiling: 1,
        } if *required_tokens > 1
    ));
    assert_eq!(error.code(), "implementation_context_too_large");
    assert_eq!(seed.state, state_before);
    assert!(
        seed.state
            .implementation
            .as_ref()
            .expect("implementation projection")
            .prepared_contexts
            .is_empty()
    );
}

#[test]
fn stale_hash_missing_and_unselected_artifacts_are_rejected_atomically() {
    let mut seed = implementation_seed(FixtureOperation::ModifySmall, 4_096);
    let request = target_context_request(&seed.state);
    let valid = materialized_context(&seed, &request);

    let mut stale = valid.clone();
    stale.repository_revision = RepositoryRevisionId::new("repository-revision:stale");
    assert_eq!(
        prepare_target_context(&request, &stale)
            .expect_err("stale repository context")
            .code(),
        "materialized_target_context_binding_mismatch"
    );

    let mut wrong_target_hash = valid.clone();
    let LoadedPathState::Existing { content, .. } = &mut wrong_target_hash.path_states[0] else {
        panic!("modify probe must load an existing target");
    };
    let wrong_target_bytes = b"wrong target bytes".to_vec();
    *content = LoadedContextArtifact::new(
        artifact_reference_hash(&wrong_target_bytes),
        ArtifactScope::FullFile,
        wrong_target_bytes,
    )
    .unwrap();
    assert_eq!(
        prepare_target_context(&request, &wrong_target_hash)
            .expect_err("target hash mismatch")
            .code(),
        "materialized_target_path_state_conflict"
    );

    let mut missing = valid.clone();
    missing.evidence_artifacts.clear();
    assert_eq!(
        prepare_target_context(&request, &missing)
            .expect_err("mandatory artifact missing")
            .code(),
        "target_context_artifact_set_mismatch"
    );

    let mut wrong_reference = valid.clone();
    let artifact = wrong_reference
        .evidence_artifacts
        .values_mut()
        .next()
        .expect("required artifact");
    artifact.artifact_reference_hash = stable_sha256(&["wrong-artifact-reference"]);
    assert_eq!(
        prepare_target_context(&request, &wrong_reference)
            .expect_err("content-address mismatch")
            .code(),
        "target_context_artifact_binding_mismatch"
    );

    let mut unselected = valid.clone();
    unselected.evidence_artifacts.insert(
        EvidenceId::new("evidence:unselected-secret"),
        LoadedContextArtifact::new(
            artifact_reference_hash(SECRET_SENTINEL.as_bytes()),
            ArtifactScope::FullFile,
            SECRET_SENTINEL.as_bytes().to_vec(),
        )
        .unwrap(),
    );
    assert_eq!(
        prepare_target_context(&request, &unselected)
            .expect_err("unselected evidence must not enter target-local materialization")
            .code(),
        "target_context_artifact_set_mismatch"
    );

    let prepared = prepare_target_context(&request, &valid).expect("valid target context");
    let mut forged_projection = prepared.clone();
    forged_projection.manifest.estimated_input_tokens = forged_projection
        .manifest
        .estimated_input_tokens
        .saturating_add(1);
    assert_eq!(
        forged_projection
            .validate_against_request(&request)
            .expect_err("a self-reported token estimate is not authoritative")
            .code(),
        "target_context_projection_mismatch"
    );

    let mut tampered = prepared;
    tampered.manifest.repository_revision = RepositoryRevisionId::new("repository-revision:other");
    let before_event = seed.state.clone();
    let tampered_event = envelope(
        &seed.state,
        "phase4:implementation:tampered-context",
        ImplementationEvent::TargetContextPrepared {
            prepared: Box::new(tampered),
        },
    );
    assert!(seed.state.append_event(tampered_event).is_err());
    assert_eq!(seed.state, before_event);
}

#[test]
fn context_serde_is_strict_and_loaded_content_is_redacted() {
    let seed = implementation_seed(FixtureOperation::ModifySmall, 4_096);
    let request = target_context_request(&seed.state);
    let materialized = materialized_context(&seed, &request);
    let prepared = prepare_target_context(&request, &materialized).expect("prepared context");

    let manifest_json = serde_json::to_string(&prepared.manifest).expect("serialize manifest");
    let replayed: TargetContextManifest =
        serde_json::from_str(&manifest_json).expect("strict manifest roundtrip");
    assert_eq!(replayed, prepared.manifest);
    let mut unknown = serde_json::to_value(&prepared.manifest).unwrap();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), serde_json::json!(true));
    assert!(serde_json::from_value::<TargetContextManifest>(unknown).is_err());

    let secret = LoadedContextArtifact::new(
        artifact_reference_hash(SECRET_SENTINEL.as_bytes()),
        ArtifactScope::FullFile,
        SECRET_SENTINEL.as_bytes().to_vec(),
    )
    .unwrap();
    assert!(!format!("{secret:?}").contains(SECRET_SENTINEL));
    let secret_materialized = MaterializedTargetContext {
        request_id: request.request_id,
        repository_revision: request.repository_revision,
        repository_fingerprint: stable_sha256(&["phase4-secret-fingerprint"]),
        path_states: vec![LoadedPathState::Existing {
            path: ProfilePath::new("src/small_target.rs").unwrap(),
            content: secret.clone(),
        }],
        evidence_artifacts: BTreeMap::from([(EvidenceId::new("evidence:secret"), secret)]),
    };
    assert!(!format!("{secret_materialized:?}").contains(SECRET_SENTINEL));
    assert!(!manifest_json.contains(SECRET_SENTINEL));
    assert!(!format!("{:?}", seed.state).contains(SECRET_SENTINEL));
}
