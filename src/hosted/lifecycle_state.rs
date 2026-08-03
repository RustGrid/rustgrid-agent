// Extracted from the hosted execution composition root.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PartialRunContext {
    pub(super) pull_request_number: u64,
    pub(super) changed_paths: Vec<String>,
    pub(super) remaining_work: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // Names are the explicit startup-path contract.
pub(super) enum StartupMode {
    FreshRun,
    ResumeRun,
    RecoveryPublicationRun,
}

impl StartupMode {
    pub(super) const fn next_decision(self) -> &'static str {
        match self {
            Self::FreshRun => "initialize_execution_snapshot",
            Self::ResumeRun => "resume_next_graph_node",
            Self::RecoveryPublicationRun => "evaluate_recovery_publication",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct StartupModeResolution {
    pub(super) mode: StartupMode,
    pub(super) persisted_graph_present: bool,
    pub(super) persisted_notebook_revision: Option<u64>,
    pub(super) recovery_marker_present: bool,
}

pub(super) fn compatible_worker_notebook(manifest: &HostedManifest) -> Option<WorkerNotebook> {
    manifest
        .run
        .metadata
        .get("worker_notebook")
        .cloned()
        .and_then(|value| serde_json::from_value::<WorkerNotebook>(value).ok())
        .filter(|notebook| {
            notebook.schema_version == 1
                && notebook.repository_base_sha == manifest.github.base_sha
                && notebook.branch == manifest.github.branch
        })
}

pub(super) fn resolve_startup_mode(
    manifest: &HostedManifest,
    resumed_branch: bool,
    changed_paths: &[String],
) -> StartupModeResolution {
    use crate::execution_graph::{FailureCategory, PublicationStatus};

    let persisted = compatible_worker_notebook(manifest);
    let persisted_graph_present = persisted
        .as_ref()
        .is_some_and(|notebook| notebook.orchestration.graph.is_some());
    let persisted_notebook_revision = persisted.as_ref().map(|notebook| notebook.revision);
    let recoverable_orchestration_failure = persisted.as_ref().is_some_and(|notebook| {
        notebook
            .orchestration
            .failures
            .unresolved()
            .any(|failure| failure.category == FailureCategory::OrchestrationInvariantViolation)
    });
    let interrupted_publication = persisted.as_ref().is_some_and(|notebook| {
        notebook.orchestration.publication.recovery_requested
            || matches!(
                notebook.orchestration.publication.status,
                PublicationStatus::InProgress
                    | PublicationStatus::CommitCreated
                    | PublicationStatus::BranchPushed
                    | PublicationStatus::Failed
            )
    });
    // A branch is recovery evidence only when checkout confirms a persisted
    // remote branch and its base-to-head diff is non-empty. Branch existence
    // by itself is normal for a freshly checked-out mission.
    let persisted_branch_with_changes = resumed_branch && !changed_paths.is_empty();
    let recovery_marker_present = recoverable_orchestration_failure
        || interrupted_publication
        || persisted_branch_with_changes;
    let mode = if recovery_marker_present {
        StartupMode::RecoveryPublicationRun
    } else if persisted.is_some() {
        StartupMode::ResumeRun
    } else {
        StartupMode::FreshRun
    };
    StartupModeResolution {
        mode,
        persisted_graph_present,
        persisted_notebook_revision,
        recovery_marker_present,
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct ToolUsage {
    pub(super) reads: u32,
    pub(super) failed_reads: u32,
    pub(super) searches: u32,
    pub(super) writes: u32,
    pub(super) successful_writes: u32,
    pub(super) failed_writes: u32,
    pub(super) write_preflight_rejections: u32,
    pub(super) write_execution_failures: u32,
    pub(super) validation_commands: u32,
    pub(super) focused_validations: u32,
    pub(super) required_validations: u32,
    pub(super) deduplicated_validations: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ToolProgressRecord {
    #[serde(default)]
    pub(super) execution_attempt: i32,
    pub(super) model_call: usize,
    pub(super) phase: ExecutionPhase,
    pub(super) tool: String,
    #[serde(default)]
    pub(super) target: Option<String>,
    pub(super) class: ToolProgressClass,
    pub(super) outcome_signature: String,
    pub(super) detail: String,
    pub(super) repository_progress: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ImplementationReadProgress {
    pub(super) consecutive_preparation_reads: usize,
    pub(super) recoverable_read_failures: usize,
    pub(super) repeated_identical_read_failures: usize,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn new_tool_progress_record(
    execution_attempt: i32,
    model_call: usize,
    phase: ExecutionPhase,
    tool: &str,
    target: Option<String>,
    class: ToolProgressClass,
    detail: impl Into<String>,
    repository_progress: bool,
) -> ToolProgressRecord {
    let detail = truncate_text(&detail.into(), 1_000);
    let outcome_signature = sha256_text(&format!(
        "{tool}\0{}\0{detail}",
        target.as_deref().unwrap_or_default()
    ));
    ToolProgressRecord {
        execution_attempt,
        model_call,
        phase,
        tool: tool.to_owned(),
        target,
        class,
        outcome_signature,
        detail,
        repository_progress,
    }
}

pub(super) fn implementation_read_progress(
    records: &[ToolProgressRecord],
    execution_attempt: i32,
) -> ImplementationReadProgress {
    let records = records
        .iter()
        .filter(|record| {
            record.execution_attempt == execution_attempt
                && matches!(
                    record.phase,
                    ExecutionPhase::Implementation | ExecutionPhase::Repair
                )
                && matches!(
                    record.tool.as_str(),
                    "read_file" | "search_text" | "related_tests"
                )
        })
        .collect::<Vec<_>>();
    let consecutive_preparation_reads = records
        .iter()
        .rev()
        .take_while(|record| {
            matches!(
                record.class,
                ToolProgressClass::Productive
                    | ToolProgressClass::Neutral
                    | ToolProgressClass::ActionRedirected
                    | ToolProgressClass::Duplicate
            )
        })
        .count();
    let recoverable_read_failures = records
        .iter()
        .filter(|record| record.class == ToolProgressClass::RecoverableFailure)
        .count();
    let repeated_identical_read_failures = records
        .iter()
        .filter(|record| record.class == ToolProgressClass::RecoverableFailure)
        .fold(BTreeMap::<&str, usize>::new(), |mut counts, record| {
            *counts.entry(record.outcome_signature.as_str()).or_default() += 1;
            counts
        })
        .into_values()
        .max()
        .unwrap_or_default();
    ImplementationReadProgress {
        consecutive_preparation_reads,
        recoverable_read_failures,
        repeated_identical_read_failures,
    }
}

pub(super) fn unresolved_preparation_blockers(
    records: &[ToolProgressRecord],
    execution_attempt: i32,
    implementation_calls: usize,
    successful_writes: u32,
) -> Vec<String> {
    let mut unresolved = BTreeMap::<(String, Option<String>), (usize, String, String)>::new();
    for (index, record) in records
        .iter()
        .enumerate()
        .filter(|(_, record)| record.execution_attempt == execution_attempt)
    {
        let key = (record.tool.clone(), record.target.clone());
        match record.class {
            ToolProgressClass::RecoverableFailure | ToolProgressClass::BlockingFailure => {
                unresolved.insert(
                    key,
                    (
                        index,
                        record.outcome_signature.clone(),
                        format!(
                            "{}{}: {}",
                            record.tool,
                            record
                                .target
                                .as_deref()
                                .map(|target| format!(" `{target}`"))
                                .unwrap_or_default(),
                            truncate_text(&record.detail, 500),
                        ),
                    ),
                );
            }
            ToolProgressClass::Productive => {
                unresolved.remove(&key);
            }
            ToolProgressClass::Neutral
            | ToolProgressClass::ActionRedirected
            | ToolProgressClass::Duplicate => {}
        }
    }
    let mut unresolved = unresolved.into_values().collect::<Vec<_>>();
    unresolved.sort_by_key(|(index, _, _)| *index);
    let mut seen = BTreeSet::new();
    let mut blockers = unresolved
        .into_iter()
        .rev()
        .filter(|(_, signature, _)| seen.insert(signature.clone()))
        .take(6)
        .map(|(_, _, summary)| summary)
        .collect::<Vec<_>>();
    blockers.reverse();
    if blockers.is_empty()
        && successful_writes == 0
        && implementation_calls >= lifecycle::FIRST_WRITE_DELAY_CALL
    {
        blockers.push(format!(
            "{implementation_calls} implementation turns produced no repository operation or verified mutation; the guided recovery turn must act on the current target"
        ));
    }
    blockers
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct PlanningRepairState {
    #[serde(default)]
    pub(super) valid_planned_changes: Vec<PlannedChange>,
    #[serde(default)]
    pub(super) valid_planned_change_positions: Vec<usize>,
    #[serde(default)]
    pub(super) original_change_ids: Vec<Option<String>>,
    #[serde(default)]
    pub(super) original_change_count: usize,
    #[serde(default)]
    pub(super) invalid_fields: Vec<String>,
    pub(super) model_call: usize,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ImplementationStartContext {
    pub(super) goal: String,
    #[serde(skip_serializing)]
    pub(super) target_order: Vec<ImplementationTarget>,
    pub(super) acceptance_criteria_ids: Vec<String>,
    pub(super) assigned_acceptance_criteria: Vec<impact_map::AcceptanceCriterion>,
    pub(super) exact_files_already_read: Vec<String>,
    pub(super) missing_file_contents: Vec<String>,
    pub(super) source_tree_hash: String,
    pub(super) remaining_call_budget: usize,
    pub(super) current_target: Option<ImplementationTarget>,
    pub(super) cached_current_file_content: Option<String>,
    pub(super) target_content_hash: Option<String>,
    pub(super) repository_fingerprint: String,
    pub(super) mutation_repair: Option<MutationDiagnosticArtifact>,
    pub(super) cached_nearby_context: Vec<crate::execution_graph::FileExcerpt>,
    pub(super) graph_node_id: Option<crate::execution_graph::ExecutionNodeId>,
    pub(super) dependency_evidence: Vec<crate::execution_graph::EvidenceSummary>,
    pub(super) relevant_impact_areas: Vec<ImpactArea>,
    pub(super) related_test_evidence: Vec<crate::execution_graph::FileExcerpt>,
    pub(super) constraints: Vec<String>,
    pub(super) allowed_tools: Vec<crate::execution_graph::ToolKind>,
    pub(super) remaining_node_budget: Option<crate::execution_graph::NodeBudgetRemaining>,
    pub(super) guided_recovery: bool,
    pub(super) unresolved_preparation_blockers: Vec<String>,
    pub(super) instruction: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ImplementationTarget {
    pub(super) change_id: String,
    pub(super) path: String,
    pub(super) role: String,
    pub(super) new_file: bool,
    pub(super) intent: String,
    pub(super) acceptance_criteria: Vec<String>,
    pub(super) status: IntendedChangeStatus,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct FinalizationRevalidation {
    pub(super) repository_fingerprint: String,
    pub(super) invalidated_after_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct PersistedCompletionArtifact {
    pub(super) event_sequence: u64,
    pub(super) repository_fingerprint: String,
    #[serde(default)]
    pub(super) validation_evidence_ids: Vec<String>,
    #[serde(default)]
    pub(super) reviewed_paths: Vec<String>,
    #[serde(default)]
    pub(super) declaration: Option<ImplementationDeclaration>,
    pub(super) evaluation: CompletionEvaluation,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DependencyBootstrapStatus {
    Passed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct DependencyBootstrapEvidence {
    pub(super) command: String,
    pub(super) lock_hash: String,
    pub(super) repository_fingerprint: String,
    pub(super) completed_at: String,
    pub(super) status: DependencyBootstrapStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct WorkerNotebook {
    pub(super) schema_version: u32,
    pub(super) revision: u64,
    pub(super) goal: String,
    #[serde(default)]
    pub(super) acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub(super) acceptance_criteria_v2: Vec<impact_map::AcceptanceCriterion>,
    pub(super) phase: ExecutionPhase,
    #[serde(default)]
    pub(super) implementation_substate: ImplementationSubstate,
    pub(super) repository_base_sha: String,
    pub(super) branch: String,
    pub(super) repository_fingerprint: String,
    pub(super) execution_attempt: i32,
    #[serde(default)]
    pub(super) architecture_findings: Vec<String>,
    #[serde(default)]
    pub(super) impact_map: Vec<ImpactArea>,
    #[serde(default)]
    pub(super) impact_map_v2: Option<ImpactMap>,
    #[serde(default)]
    pub(super) impact_map_artifact: ArtifactCheckpoint,
    #[serde(default)]
    pub(super) impact_map_invalid_payload: Option<Value>,
    #[serde(default)]
    pub(super) impact_evidence: Vec<impact_map::EvidenceReference>,
    #[serde(default)]
    pub(super) files_inspected: Vec<String>,
    #[serde(default)]
    pub(super) read_ranges_inspected: Vec<String>,
    #[serde(default)]
    pub(super) searches_completed: Vec<String>,
    #[serde(default)]
    pub(super) discovery_paths_sampled: Vec<String>,
    #[serde(default)]
    pub(super) planned_changes: Vec<PlannedChange>,
    #[serde(default)]
    pub(super) planning_repair: Option<PlanningRepairState>,
    #[serde(default)]
    pub(super) intended_changes: Vec<IntendedChangeRecord>,
    #[serde(default)]
    pub(super) write_attempts: Vec<WriteAttemptRecord>,
    #[serde(default)]
    pub(super) mutation_diagnostics: Vec<MutationDiagnosticArtifact>,
    #[serde(default)]
    pub(super) write_preflight_rejections: Vec<MutationPreflightRecord>,
    #[serde(default)]
    pub(super) completed_changes: Vec<String>,
    #[serde(default)]
    pub(super) failed_changes: Vec<ToolFailureRecord>,
    #[serde(default)]
    pub(super) tool_progress: Vec<ToolProgressRecord>,
    #[serde(default)]
    pub(super) remaining_work: Vec<String>,
    #[serde(default)]
    pub(super) remaining_work_v2: Vec<RemainingWorkItem>,
    #[serde(default)]
    pub(super) blocking_unknowns: Vec<String>,
    #[serde(default)]
    pub(super) validation_failures: Vec<String>,
    #[serde(default)]
    pub(super) validation_evidence: Vec<ValidationEvidence>,
    #[serde(default)]
    pub(super) required_gates: Vec<RequiredGate>,
    #[serde(default)]
    pub(super) dependency_bootstrap_evidence: Option<DependencyBootstrapEvidence>,
    #[serde(default)]
    pub(super) phase_budget: Value,
    #[serde(default)]
    pub(super) last_successful_action: Value,
    #[serde(default)]
    pub(super) last_orchestration_decision_key: Option<String>,
    #[serde(default)]
    pub(super) finalization_revalidation: Option<FinalizationRevalidation>,
    #[serde(default)]
    pub(super) completion_artifact: Option<PersistedCompletionArtifact>,
    #[serde(default)]
    pub(super) orchestration: HostedOrchestrationCheckpoint,
}

pub(super) fn canonical_finalization_state(
    checkpoint: &HostedOrchestrationCheckpoint,
) -> (bool, Option<OrchestratedMissionOutcome>) {
    let diff_complete = checkpoint
        .graph
        .as_ref()
        .and_then(|graph| {
            graph
                .nodes
                .iter()
                .find(|node| node.kind == crate::execution_graph::ExecutionNodeKind::DiffReview)
        })
        .is_some_and(|node| node.status.is_success());
    let completion_complete = checkpoint
        .graph
        .as_ref()
        .and_then(|graph| {
            graph.nodes.iter().find(|node| {
                node.kind == crate::execution_graph::ExecutionNodeKind::CompletionEvaluation
            })
        })
        .is_some_and(|node| node.status.is_success());
    let diff_reviewed = diff_complete
        && checkpoint.domain_events.iter().any(|event| {
            matches!(
                event,
                crate::execution_graph::ExecutionDomainEvent::DiffReviewed { .. }
            )
        });
    let completion_outcome = completion_complete.then(|| {
        checkpoint
            .domain_events
            .iter()
            .rev()
            .find_map(|event| match event {
                crate::execution_graph::ExecutionDomainEvent::CompletionEvaluated {
                    outcome,
                    ..
                } => Some(*outcome),
                _ => None,
            })
    });
    (diff_reviewed, completion_outcome.flatten())
}

pub(super) fn valid_completion_artifact<'a>(
    notebook: &'a WorkerNotebook,
    repository_fingerprint: &str,
    changed_paths: &[String],
) -> Option<&'a PersistedCompletionArtifact> {
    let artifact = notebook.completion_artifact.as_ref()?;
    let (event_sequence, event_outcome) = notebook
        .orchestration
        .domain_events
        .iter()
        .rev()
        .find_map(|event| match event {
            crate::execution_graph::ExecutionDomainEvent::CompletionEvaluated {
                sequence,
                outcome,
                ..
            } => Some((*sequence, *outcome)),
            _ => None,
        })?;
    let completion_complete = notebook
        .orchestration
        .graph
        .as_ref()?
        .nodes
        .iter()
        .find(|node| node.kind == crate::execution_graph::ExecutionNodeKind::CompletionEvaluation)
        .is_some_and(|node| node.status.is_success());
    let evidence_is_current = artifact.validation_evidence_ids.iter().all(|evidence_id| {
        notebook
            .orchestration
            .evidence
            .validations
            .get(evidence_id)
            .is_some_and(|evidence| {
                evidence.status == crate::execution_graph::ValidationEvidenceStatus::Passed
                    && evidence.repository_fingerprint == repository_fingerprint
            })
    });
    (completion_complete
        && artifact.event_sequence == event_sequence
        && artifact.repository_fingerprint == repository_fingerprint
        && artifact.reviewed_paths == changed_paths
        && mission_outcome_from_completion(artifact.evaluation.status) == event_outcome
        && evidence_is_current)
        .then_some(artifact)
}

pub(super) fn notebook_finalization_requires_revalidation(
    notebook: &WorkerNotebook,
    repository_fingerprint: &str,
    changed_paths: &[String],
) -> bool {
    let Some(graph) = notebook.orchestration.graph.as_ref() else {
        return false;
    };
    let finalization_started = graph.nodes.iter().any(|node| {
        (node.kind.is_validation()
            || matches!(
                node.kind,
                crate::execution_graph::ExecutionNodeKind::DiffReview
                    | crate::execution_graph::ExecutionNodeKind::CompletionEvaluation
            ))
            && node.status.is_success()
    }) || notebook.orchestration.publication.status
        != crate::execution_graph::PublicationStatus::NotStarted;
    if !finalization_started {
        return false;
    }

    let validation_is_stale = graph
        .nodes
        .iter()
        .filter(|node| node.required && node.kind.is_validation() && node.status.is_success())
        .any(|node| {
            let Some(gate) = node.validation.as_ref() else {
                return true;
            };
            let expected_fingerprint = gate.fingerprint(repository_fingerprint);
            !node.evidence_ids.iter().any(|evidence_id| {
                notebook
                    .orchestration
                    .evidence
                    .validations
                    .get(evidence_id)
                    .is_some_and(|evidence| {
                        evidence.status == crate::execution_graph::ValidationEvidenceStatus::Passed
                            && evidence.repository_fingerprint == repository_fingerprint
                            && evidence.fingerprint == expected_fingerprint
                    })
            })
        });
    if validation_is_stale {
        return true;
    }

    let completion_finished = graph.nodes.iter().any(|node| {
        node.kind == crate::execution_graph::ExecutionNodeKind::CompletionEvaluation
            && node.status.is_success()
    });
    let publication_started = notebook.orchestration.publication.status
        != crate::execution_graph::PublicationStatus::NotStarted;
    (publication_started && !completion_finished)
        || (completion_finished
            && valid_completion_artifact(notebook, repository_fingerprint, changed_paths).is_none())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct LocalizedDiscoveryCoverage {
    pub(super) provider: bool,
    pub(super) selector: bool,
    pub(super) token_source: bool,
    pub(super) focused_tests: bool,
    pub(super) validation_commands: bool,
    pub(super) centralized_abstraction: bool,
    pub(super) representative_consumers: usize,
}

pub(super) fn localized_visual_goal(goal: &str) -> bool {
    let goal = goal.to_ascii_lowercase();
    [
        "theme",
        "color scheme",
        "colour scheme",
        "design token",
        "css variable",
        "visual system",
    ]
    .iter()
    .any(|needle| goal.contains(needle))
}

pub(super) fn localized_discovery_core_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    [
        "themeprovider",
        "theme-provider",
        "themetoggle",
        "theme-toggle",
        "token",
        "variable",
        "global.css",
        "globals.css",
        "test",
        "package.json",
        "cargo.toml",
    ]
    .iter()
    .any(|needle| path.contains(needle))
}

pub(super) fn localized_discovery_coverage(
    notebook: &WorkerNotebook,
) -> LocalizedDiscoveryCoverage {
    let evidence = notebook
        .files_inspected
        .iter()
        .chain(notebook.searches_completed.iter())
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let contains = |needles: &[&str]| {
        evidence
            .iter()
            .any(|value| needles.iter().any(|needle| value.contains(needle)))
    };
    let centralized_abstraction = notebook.architecture_findings.iter().any(|finding| {
        let finding = finding.to_ascii_lowercase();
        (finding.contains("central") || finding.contains("semantic"))
            && (finding.contains("token")
                || finding.contains("variable")
                || finding.contains("theme"))
    });
    LocalizedDiscoveryCoverage {
        provider: contains(&["themeprovider", "theme-provider", "theme provider"]),
        selector: contains(&["themetoggle", "theme-toggle", "theme selector"]),
        token_source: contains(&["globals.css", "global.css", "design token", "css variable"]),
        focused_tests: contains(&["theme-provider.test", "theme-tokens.test", "theme test"]),
        validation_commands: contains(&["package.json", "cargo.toml", "npm test", "npm run"]),
        centralized_abstraction,
        representative_consumers: notebook
            .files_inspected
            .iter()
            .chain(notebook.discovery_paths_sampled.iter())
            .filter(|path| !localized_discovery_core_path(path))
            .collect::<BTreeSet<_>>()
            .len(),
    }
}

pub(super) fn localized_discovery_should_stop(coverage: LocalizedDiscoveryCoverage) -> bool {
    coverage.centralized_abstraction
        && coverage.provider
        && coverage.selector
        && coverage.token_source
        && coverage.focused_tests
        && coverage.validation_commands
}

pub(super) fn validate_localized_discovery_scope(
    notebook: &WorkerNotebook,
    requested_paths: &[&str],
) -> Result<()> {
    if !localized_visual_goal(&notebook.goal) {
        return Ok(());
    }
    let coverage = localized_discovery_coverage(notebook);
    if localized_discovery_should_stop(coverage) {
        bail!(
            "localized_discovery_complete: record the compact impact map instead of inspecting more repository files"
        );
    }
    if coverage.centralized_abstraction {
        let new_consumers = requested_paths
            .iter()
            .filter(|path| !localized_discovery_core_path(path))
            .filter(|path| {
                !notebook.files_inspected.iter().any(|seen| seen == **path)
                    && !notebook
                        .discovery_paths_sampled
                        .iter()
                        .any(|seen| seen == **path)
            })
            .copied()
            .collect::<BTreeSet<_>>()
            .len();
        if coverage
            .representative_consumers
            .saturating_add(new_consumers)
            > 3
        {
            bail!(
                "localized_discovery_consumer_limit: centralized theme architecture permits at most three representative consumers"
            );
        }
    }
    Ok(())
}

pub(super) fn discovery_requested_paths<'a>(
    name: &str,
    arguments: &'a serde_json::Map<String, Value>,
) -> Vec<&'a str> {
    match name {
        "read_file" => arguments
            .get("path")
            .and_then(Value::as_str)
            .into_iter()
            .collect(),
        "search_text" => arguments
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| Path::new(path).extension().is_some())
            .into_iter()
            .collect(),
        "read_files" | "related_tests" => arguments
            .get("paths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect(),
        _ => Vec::new(),
    }
}

pub(super) fn record_centralized_discovery_finding(notebook: &mut WorkerNotebook, reason: &str) {
    if notebook.phase != ExecutionPhase::Discovery || !localized_visual_goal(&notebook.goal) {
        return;
    }
    let reason = reason.to_ascii_lowercase();
    if (reason.contains("central") || reason.contains("semantic"))
        && (reason.contains("token") || reason.contains("variable") || reason.contains("theme"))
    {
        push_unique(
            &mut notebook.architecture_findings,
            "Centralized semantic theme abstraction confirmed by targeted discovery.".into(),
        );
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct UnderlyingFailure {
    pub(super) r#type: String,
    pub(super) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) stack_reference: Option<String>,
}

#[derive(Debug)]
pub(super) struct HostedStartupFailure {
    pub(super) category: &'static str,
    pub(super) code: &'static str,
    pub(super) message: String,
    pub(super) underlying: anyhow::Error,
}

impl std::fmt::Display for HostedStartupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HostedStartupFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.underlying.as_ref())
    }
}

#[derive(Debug, Serialize)]
pub(super) struct HostedAgentExecutionFailure {
    pub(super) status: &'static str,
    pub(super) category: &'static str,
    pub(super) process_health: &'static str,
    pub(super) mission_outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) blocker: Option<String>,
    pub(super) resumable: bool,
    pub(super) code: String,
    pub(super) phase: ExecutionPhase,
    pub(super) message: String,
    pub(super) underlying_error: UnderlyingFailure,
    pub(super) model_calls_used: usize,
    pub(super) model_calls_limit: usize,
    pub(super) model_calls_remaining: usize,
    pub(super) phase_calls_used: usize,
    pub(super) phase_calls_limit: usize,
    pub(super) last_successful_action: Value,
    pub(super) usage: ToolUsage,
    pub(super) estimated_cost_micros: u64,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) changed_paths: Vec<String>,
    pub(super) remaining_work: Vec<RemainingWorkItem>,
    pub(super) failed_tool_operations: Vec<ToolProgressRecord>,
    pub(super) current_plan: Vec<PlannedChange>,
    pub(super) validation_evidence: Vec<ValidationEvidence>,
    pub(super) notebook_revision: u64,
    pub(super) recoverable: bool,
    pub(super) resume_phase: String,
    pub(super) recommended_action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) artifact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) semantic_status: Option<ArtifactSemanticStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) persistence_status: Option<ArtifactPersistenceStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) rustgrid_gateway_status: Option<Option<u16>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) upstream_provider_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) failure_stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) provider_contacted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) call_budget_consumed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reservation_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reservation_reconciliation_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) rustgrid_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) transport_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) provider_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) provider_error: Option<ProviderErrorDiagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) provider_response_body: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) model_alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) resolved_provider_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) adapter_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) payload_schema_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) provider_attempts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) actual_cost_micros: Option<u64>,
}

impl std::fmt::Display for HostedAgentExecutionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HostedAgentExecutionFailure {}

pub(super) fn classify_implementation_preparation_failure(
    mut failure: HostedAgentExecutionFailure,
    remaining_work: &[RemainingWorkItem],
) -> HostedAgentExecutionFailure {
    failure.status = "blocked";
    failure.category = "implementation_blocked";
    failure.process_health = "healthy";
    failure.mission_outcome = "blocked";
    failure.blocker = Some("implementation_preparation_failed".into());
    failure.resumable = true;
    failure.code = "implementation_preparation_failed".into();
    failure.resume_phase = ExecutionPhase::Implementation.as_str().into();
    failure.recommended_action = "Resume in implementation at the current planned target using the persisted read failures and recovery data.".into();
    failure.remaining_work = remaining_work.to_vec();
    failure
}

pub(super) fn blocked_result_event_payload(
    failure: &HostedAgentExecutionFailure,
    diagnostics: Value,
) -> Value {
    json!({
        "status": "blocked",
        "mission_outcome": "blocked",
        "process_health": "healthy",
        "reason_code": failure.code,
        "resumable": failure.resumable,
        "resume_phase": failure.resume_phase,
        "changed_paths": failure.changed_paths,
        "remaining_work": failure.remaining_work,
        "terminal_telemetry": {
            "model_calls_used": failure.model_calls_used,
            "input_tokens": failure.input_tokens,
            "output_tokens": failure.output_tokens,
            "estimated_cost_micros": failure.estimated_cost_micros,
            "usage": failure.usage,
            "changed_paths": failure.changed_paths,
            "last_successful_action": failure.last_successful_action,
            "phase_reached": failure.phase,
            "plan": failure.current_plan,
            "remaining_work": failure.remaining_work,
            "validation_evidence": failure.validation_evidence,
            "notebook_revision": failure.notebook_revision,
        },
        "failure": diagnostics,
    })
}

pub(super) fn blocked_completion_evaluation(
    failure: &HostedAgentExecutionFailure,
) -> CompletionEvaluation {
    CompletionEvaluation {
        status: CompletionStatus::Blocked,
        implementation_completeness: ImplementationCompleteness::Incomplete,
        verification_readiness: VerificationReadiness::Blocked,
        evaluation_source: EvaluationSource::OrchestratorFallback,
        confidence: 1.0,
        criteria: Vec::new(),
        remaining_implementation_work: failure
            .remaining_work
            .iter()
            .map(|item| {
                format!(
                    "{}: {} ({})",
                    item.path,
                    item.reason,
                    format!("{:?}", item.status).to_ascii_lowercase()
                )
            })
            .collect(),
        remaining_automated_verification: Vec::new(),
        pending_external_review: Vec::new(),
        optional_follow_up: Vec::new(),
        review_checklist: Vec::new(),
        unrecovered_tool_failures: failure
            .failed_tool_operations
            .iter()
            .map(|operation| operation.detail.clone())
            .collect(),
        summary: failure.message.clone(),
    }
}

pub(super) fn acceptance_criteria_from_ticket(ticket: &str) -> Vec<String> {
    let mut criteria = Vec::new();
    let mut in_acceptance_criteria = false;
    for line in ticket.lines() {
        let trimmed = line.trim();
        let normalized = trimmed.trim_start_matches('#').trim().to_ascii_lowercase();
        if normalized == "acceptance criteria" {
            in_acceptance_criteria = true;
            continue;
        }
        if in_acceptance_criteria && trimmed.starts_with('#') {
            break;
        }
        if !in_acceptance_criteria {
            continue;
        }
        let item = trimmed
            .strip_prefix("- [ ] ")
            .or_else(|| trimmed.strip_prefix("- [x] "))
            .or_else(|| trimmed.strip_prefix("- [X] "))
            .or_else(|| trimmed.strip_prefix("- "))
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| {
                let (number, item) = trimmed.split_once(". ")?;
                number
                    .chars()
                    .all(|character| character.is_ascii_digit())
                    .then_some(item)
            })
            .map(str::trim)
            .filter(|item| !item.is_empty());
        if let Some(item) = item {
            push_unique(&mut criteria, item.to_owned());
        }
    }
    if criteria.is_empty() && !ticket.trim().is_empty() {
        criteria.push(ticket.trim().to_owned());
    }
    criteria
}

pub(super) fn project_verification_policy(manifest: &HostedManifest) -> ProjectVerificationPolicy {
    manifest
        .run
        .metadata
        .get("project_verification_policy")
        .or_else(|| manifest.run.metadata.get("browser_test_policy"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .or_else(|| serde_json::from_value(manifest.run.metadata.clone()).ok())
        .unwrap_or_default()
}

pub(super) fn impact_map_fallback_threshold(manifest: &HostedManifest) -> f64 {
    manifest
        .run
        .metadata
        .get("impact_map_fallback_confidence_threshold")
        .and_then(Value::as_f64)
        .filter(|value| (0.0..=1.0).contains(value))
        .unwrap_or(0.8)
}

pub(super) fn partial_pr_remaining_work(body: Option<&str>) -> Vec<String> {
    let Some(body) = body else {
        return Vec::new();
    };
    let Some((_, remainder)) = body.split_once("Remaining work:\n") else {
        return Vec::new();
    };
    let section = remainder
        .split_once("\n\nTechnical validation:")
        .map(|(section, _)| section)
        .unwrap_or(remainder);
    section
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- "))
        .map(str::trim)
        .filter(|work| !work.is_empty() && *work != "None reported.")
        .map(str::to_owned)
        .collect()
}

pub(super) fn detect_partial_run(
    pull_request: Option<&PullRequest>,
    resumed_branch: bool,
    execution_attempt: i32,
    changed_paths: Vec<String>,
) -> Option<PartialRunContext> {
    let pull_request = pull_request?;
    let explicitly_incomplete = pull_request.body.as_deref().is_some_and(|body| {
        body.contains("INCOMPLETE")
            && body.contains("continue implementation before review or merge")
            && body.contains("Remaining work:")
    });
    if execution_attempt <= 1
        || !resumed_branch
        || !pull_request.draft
        || !explicitly_incomplete
        || changed_paths.is_empty()
    {
        return None;
    }
    Some(PartialRunContext {
        pull_request_number: pull_request.number,
        changed_paths,
        remaining_work: partial_pr_remaining_work(pull_request.body.as_deref()),
    })
}

pub(super) fn new_worker_notebook(
    manifest: &HostedManifest,
    repository_fingerprint: String,
    partial_run: Option<&PartialRunContext>,
) -> WorkerNotebook {
    let acceptance_criteria = acceptance_criteria_from_ticket(&manifest.run.input_prompt);
    let mut notebook = WorkerNotebook {
        schema_version: 1,
        revision: 0,
        goal: manifest.ticket_title.clone(),
        acceptance_criteria: acceptance_criteria.clone(),
        acceptance_criteria_v2: impact_map::acceptance_criteria(&acceptance_criteria),
        phase: ExecutionPhase::Discovery,
        implementation_substate: ImplementationSubstate::Preparing,
        repository_base_sha: manifest.github.base_sha.clone(),
        branch: manifest.github.branch.clone(),
        repository_fingerprint: repository_fingerprint.clone(),
        execution_attempt: manifest.execution.attempt_number,
        architecture_findings: Vec::new(),
        impact_map: Vec::new(),
        impact_map_v2: None,
        impact_map_artifact: ArtifactCheckpoint::default(),
        impact_map_invalid_payload: None,
        impact_evidence: Vec::new(),
        files_inspected: Vec::new(),
        read_ranges_inspected: Vec::new(),
        searches_completed: Vec::new(),
        discovery_paths_sampled: Vec::new(),
        planned_changes: Vec::new(),
        planning_repair: None,
        intended_changes: Vec::new(),
        write_attempts: Vec::new(),
        mutation_diagnostics: Vec::new(),
        write_preflight_rejections: Vec::new(),
        completed_changes: Vec::new(),
        failed_changes: Vec::new(),
        tool_progress: Vec::new(),
        remaining_work: Vec::new(),
        remaining_work_v2: Vec::new(),
        blocking_unknowns: Vec::new(),
        validation_failures: Vec::new(),
        validation_evidence: Vec::new(),
        required_gates: Vec::new(),
        dependency_bootstrap_evidence: None,
        phase_budget: Value::Null,
        last_successful_action: json!({}),
        last_orchestration_decision_key: None,
        finalization_revalidation: None,
        completion_artifact: None,
        orchestration: HostedOrchestrationCheckpoint::bootstrap(manifest, &repository_fingerprint),
    };
    if let Some(partial_run) = partial_run {
        notebook.phase = ExecutionPhase::Planning;
        notebook.architecture_findings.push(format!(
            "Recovered draft pull request #{} with {} changed path(s); preserve valid prior work.",
            partial_run.pull_request_number,
            partial_run.changed_paths.len()
        ));
        let criteria_ids = (0..acceptance_criteria.len())
            .map(impact_map::criterion_id)
            .collect();
        notebook.impact_map.push(ImpactArea {
            area_id: "area-existing-partial-implementation".into(),
            name: "Existing partial implementation".into(),
            candidate_paths: partial_run.changed_paths.clone(),
            evidence: partial_run.changed_paths.iter().map(|path| impact_map::ImpactEvidence {
                evidence_type: impact_map::EvidenceType::Inference,
                path: Some(path.clone()), query: None,
                description: "Path was preserved from the resumed draft pull request.".into(),
            }).collect(),
            reason: "A later execution attempt resumed a draft pull request and must reconcile its existing diff before changing more code.".into(),
            acceptance_criteria_ids: criteria_ids,
        });
        notebook.remaining_work = if partial_run.remaining_work.is_empty() {
            vec!["Reconcile the preserved diff against every acceptance criterion.".into()]
        } else {
            partial_run.remaining_work.clone()
        };
        let restored_map = ImpactMap {
            schema_version: IMPACT_MAP_SCHEMA_VERSION.into(),
            areas: notebook.impact_map.clone(),
            inspected_files: partial_run.changed_paths.clone(),
            searches: Vec::new(),
            unresolved_questions: Vec::new(),
        };
        notebook.impact_map_v2 = Some(restored_map.clone());
        notebook.impact_map_artifact = ArtifactCheckpoint {
            artifact: "impact_map".into(),
            semantic_status: ArtifactSemanticStatus::Sufficient,
            serialization_status: ArtifactSerializationStatus::Valid,
            persistence_status: ArtifactPersistenceStatus::PendingRetry,
            artifact_sha256: impact_map_sha256(&restored_map),
            model_call_index: None,
            phase: ExecutionPhase::Planning,
            safe_error: None,
            normalization_metadata: None,
            artifact_source: Some(ArtifactSource::OrchestratorFallback),
            confidence: Some(1.0),
            failure_layer: None,
            validation_errors: Vec::new(),
            invalid_payload_shape: None,
        };
    }
    notebook
}

pub(super) fn notebook_orchestration_state(
    notebook: &WorkerNotebook,
) -> (
    Option<ImpactMap>,
    Option<ImplementationPlan>,
    ExecutionPhase,
) {
    let impact_map = notebook.impact_map_v2.clone().or_else(|| {
        (!notebook.impact_map.is_empty()).then(|| ImpactMap {
            schema_version: IMPACT_MAP_SCHEMA_VERSION.into(),
            areas: notebook.impact_map.clone(),
            inspected_files: notebook.files_inspected.clone(),
            searches: notebook
                .searches_completed
                .iter()
                .map(|query| impact_map::ImpactSearch {
                    query: query.clone(),
                    scope: None,
                })
                .collect(),
            unresolved_questions: notebook.blocking_unknowns.clone(),
        })
    });
    let implementation_plan = implementation_plan_from_notebook(notebook);
    let phase = if implementation_plan.is_some() {
        ExecutionPhase::Implementation
    } else if impact_map.is_some() {
        ExecutionPhase::Planning
    } else if notebook.phase == ExecutionPhase::ArtifactRepair {
        ExecutionPhase::ArtifactRepair
    } else {
        ExecutionPhase::Discovery
    };
    (impact_map, implementation_plan, phase)
}

pub(super) fn implementation_plan_from_notebook(
    notebook: &WorkerNotebook,
) -> Option<ImplementationPlan> {
    (!notebook.planned_changes.is_empty()).then(|| {
        let planned_new_files = notebook
            .planned_changes
            .iter()
            .flat_map(|change| &change.targets)
            .filter(|target| target.new_file)
            .map(|target| target.path.clone())
            .collect();
        let planned_test_changes = notebook
            .planned_changes
            .iter()
            .flat_map(|change| &change.targets)
            .filter(|target| {
                let role = target.role.to_ascii_lowercase();
                let path = target.path.to_ascii_lowercase();
                role.contains("test")
                    || path.contains("/tests/")
                    || path.starts_with("tests/")
                    || path.contains(".test.")
                    || path.contains(".spec.")
            })
            .map(|target| target.path.clone())
            .collect();
        ImplementationPlan {
            implementation_status: "ready".into(),
            planned_changes: notebook.planned_changes.clone(),
            planned_new_files,
            planned_test_changes,
            remaining_unknowns: Vec::new(),
            blocking_unknowns: notebook.blocking_unknowns.clone(),
        }
    })
}
