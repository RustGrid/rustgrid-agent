//! Compatibility bridge between the legacy hosted notebook and the canonical
//! execution graph.
//!
//! This module is intentionally pure. It translates persisted facts in both
//! directions, but never reads the repository, invokes a model, persists a
//! checkpoint, or publishes anything.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::execution_graph::{
    BudgetState, CancellationState, ComplexityAssessment, ComplexityClassificationStage,
    ComplexityInput, EvidenceKind, EvidenceStore, ExecutionDomainEvent, ExecutionGraph,
    ExecutionNodeId, ExecutionNodeKind, ExecutionNodeStatus, ExecutionSnapshot, FailureCategory,
    FailureId, FailureRecord, FailureStatus, FailureStore, GraphInvariantError, MissionBudget,
    MissionBudgetOverride, MissionComplexity, MissionOutcome, PlannedTarget as GraphPlannedTarget,
    PublicationState, PublicationStatus, RepositorySnapshot, ValidationEvidenceRecord,
    ValidationEvidenceStatus, ValidationGateSpec as GraphValidationGateSpec,
    ValidationGateType as GraphValidationGateType, normalize_validation_gate_order,
};
use crate::lifecycle::HostedExecutionStage;

use super::lifecycle::{
    RemainingWorkItem, RequiredGate, ValidationEvidence, ValidationGateType, ValidationSource,
    ValidationStatus,
};
use super::{
    CompletionStatus, ExecutionPhase, FailureReconciliation, HostedManifest, ImplementationPlan,
    ImplementationSubstate, IntendedChangeRecord, IntendedChangeStatus, PlannedChange,
    PlannedTarget, ToolFailureRecord, WorkerNotebook,
};

/// Durable graph state embedded in the legacy notebook during staged rollout.
///
/// `graph_revision` is duplicated deliberately: it permits a checkpoint reader
/// to detect a partially written or stale graph payload before trusting it.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct HostedOrchestrationCheckpoint {
    pub(super) graph: Option<ExecutionGraph>,
    pub(super) graph_revision: u64,
    /// True after compatibility state from a pre-graph notebook has been
    /// imported. Missing values in old serialized checkpoints default to
    /// false, allowing exactly one migration pass on resume.
    #[serde(default)]
    pub(super) legacy_import_completed: bool,
    /// True only after the locked startup dependency installation completed
    /// for the current repository/lock state.
    #[serde(default)]
    pub(super) dependency_bootstrap_completed: bool,
    pub(super) domain_events: Vec<ExecutionDomainEvent>,
    pub(super) evidence: EvidenceStore,
    pub(super) failures: FailureStore,
    pub(super) budget: BudgetState,
    pub(super) cancellation: Option<CancellationState>,
    pub(super) publication: PublicationState,
    pub(super) complexity: Option<ComplexityAssessment>,
    /// Ephemeral handoff from pure topology construction to the immediately
    /// following GraphCreated event. The event persists the exact set.
    #[serde(skip)]
    pub(super) pending_topology_preserved_node_ids: Vec<ExecutionNodeId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostedResumeReason {
    Cancellation,
    PartialReviewable,
}

fn completed_change_ids(graph: &ExecutionGraph) -> Vec<String> {
    let mut statuses = BTreeMap::<String, Vec<bool>>::new();
    for node in graph.nodes.iter().filter(|node| node.kind.is_mutation()) {
        let Some(target) = node.target.as_ref() else {
            continue;
        };
        statuses
            .entry(target.change_id.clone())
            .or_default()
            .push(node.status.is_success());
    }
    statuses
        .into_iter()
        .filter_map(|(change_id, statuses)| {
            (!statuses.is_empty() && statuses.into_iter().all(|status| status)).then_some(change_id)
        })
        .collect()
}

struct MaterializedLegacyChange {
    change_id: String,
    intent: String,
    acceptance_criteria: BTreeSet<String>,
    targets: Vec<PlannedTarget>,
}

fn materialize_legacy_changes(
    graph: &ExecutionGraph,
) -> (Vec<PlannedChange>, Vec<IntendedChangeRecord>) {
    let mut groups = Vec::<MaterializedLegacyChange>::new();
    let mut group_positions = BTreeMap::<String, usize>::new();

    for node in graph.nodes.iter().filter(|node| node.kind.is_mutation()) {
        let Some(target) = node.target.as_ref() else {
            continue;
        };
        let group_index = if let Some(index) = group_positions.get(&target.change_id) {
            *index
        } else {
            let index = groups.len();
            group_positions.insert(target.change_id.clone(), index);
            groups.push(MaterializedLegacyChange {
                change_id: target.change_id.clone(),
                intent: target.intent.clone(),
                acceptance_criteria: BTreeSet::new(),
                targets: Vec::new(),
            });
            index
        };
        let group = &mut groups[group_index];
        group
            .acceptance_criteria
            .extend(canonical_criterion_ids(&target.acceptance_criteria_ids));
        group.targets.push(PlannedTarget {
            path: target.path.clone(),
            role: target.role.clone(),
            new_file: target.new_file,
            status: legacy_status_from_graph(node.status),
        });
    }

    let planned_changes = groups
        .into_iter()
        .map(|group| {
            let status = aggregate_legacy_status(group.targets.iter().map(|target| target.status));
            let reason = group.intent.clone();
            PlannedChange {
                change_id: group.change_id,
                parent_change_id: None,
                path: String::new(),
                targets: group.targets,
                change: group.intent,
                // The graph does not retain the provider's separate rationale.
                // Reuse the canonical intent rather than preserving stale
                // compatibility metadata or mislabeling a target role.
                reason,
                status,
                acceptance_criteria: group.acceptance_criteria.into_iter().collect(),
                test_coverage: Vec::new(),
            }
        })
        .collect::<Vec<_>>();
    let intended_changes = planned_changes
        .iter()
        .map(|change| IntendedChangeRecord {
            change_id: change.change_id.clone(),
            intent: change.change.clone(),
            status: change.status,
            target: String::new(),
            targets: change.targets.clone(),
            attempts: Vec::new(),
            recovery: None,
        })
        .collect();
    (planned_changes, intended_changes)
}

fn remaining_work_item(node: &crate::execution_graph::ExecutionNode) -> RemainingWorkItem {
    if let Some(target) = node.target.as_ref() {
        return RemainingWorkItem {
            change_id: target.change_id.clone(),
            path: target.path.clone(),
            role: target.role.clone(),
            status: legacy_status_from_graph(node.status),
            reason: if target.intent.trim().is_empty() {
                format!("Complete required graph node `{}`.", node.id)
            } else {
                target.intent.clone()
            },
        };
    }
    let (role, reason) = match node.kind {
        ExecutionNodeKind::Discovery => (
            "repository discovery",
            "Complete repository discovery and persist its evidence.".to_owned(),
        ),
        ExecutionNodeKind::Planning => (
            "implementation planning",
            "Accept a complete implementation plan and materialize its graph.".to_owned(),
        ),
        ExecutionNodeKind::ValidationFocused
        | ExecutionNodeKind::ValidationSuite
        | ExecutionNodeKind::ValidationBuild
        | ExecutionNodeKind::ValidationLint => {
            let gate = node.validation.as_ref();
            (
                "required validation",
                gate.map_or_else(
                    || "Run the required validation gate.".to_owned(),
                    |gate| {
                        format!(
                            "Run required validation gate `{}` using `{}`.",
                            gate.gate_id, gate.command
                        )
                    },
                ),
            )
        }
        ExecutionNodeKind::DiffReview => (
            "diff review",
            "Review the final repository diff after required validation.".to_owned(),
        ),
        ExecutionNodeKind::CompletionEvaluation => (
            "completion evaluation",
            "Evaluate completion from the reviewed final diff.".to_owned(),
        ),
        ExecutionNodeKind::Publication => (
            "publication",
            "Publish the preserved result using the evaluated mission outcome.".to_owned(),
        ),
        ExecutionNodeKind::SourceMutation | ExecutionNodeKind::TestMutation => (
            "repository mutation",
            "Complete the required repository mutation.".to_owned(),
        ),
    };
    RemainingWorkItem {
        change_id: node.id.as_str().to_owned(),
        path: node.id.as_str().to_owned(),
        role: role.to_owned(),
        status: legacy_status_from_graph(node.status),
        reason,
    }
}

fn materialize_failed_changes(
    graph: &ExecutionGraph,
    failures: &FailureStore,
    existing: &[ToolFailureRecord],
) -> Vec<ToolFailureRecord> {
    failures
        .records
        .iter()
        .map(|failure| {
            let target = graph
                .node(&failure.node_id)
                .and_then(|node| node.target.as_ref());
            let existing = existing.iter().find(|legacy| {
                legacy.target == failure.target_path
                    && legacy.attempt_index
                        == usize::try_from(failure.attempt).unwrap_or(usize::MAX)
                    && legacy.error == failure.message
            });
            ToolFailureRecord {
                attempt_index: usize::try_from(failure.attempt).unwrap_or(usize::MAX),
                change_id: target.map(|target| target.change_id.clone()),
                tool: existing
                    .map(|failure| failure.tool.clone())
                    .filter(|tool| !tool.trim().is_empty())
                    .unwrap_or_default(),
                target: failure.target_path.clone(),
                error_code: existing
                    .map(|failure| failure.error_code.clone())
                    .filter(|code| !code.trim().is_empty())
                    .unwrap_or_else(|| failure_category_code(failure.category).to_owned()),
                match_count: existing.and_then(|failure| failure.match_count),
                error: failure.message.clone(),
                recovered: failure.status == FailureStatus::Recovered,
                reconciliation: match failure.status {
                    FailureStatus::Active => FailureReconciliation::StillUnresolved,
                    FailureStatus::Recovered => FailureReconciliation::Recovered,
                    FailureStatus::Superseded => FailureReconciliation::Superseded,
                },
                recovery: existing.and_then(|failure| failure.recovery.clone()),
                intended_change_sha256: existing
                    .and_then(|failure| failure.intended_change_sha256.clone()),
            }
        })
        .collect()
}

fn materialize_validation_evidence(
    graph: &ExecutionGraph,
    evidence: &EvidenceStore,
) -> Vec<ValidationEvidence> {
    evidence
        .validations
        .values()
        .filter_map(|record| {
            let node = graph.node(&record.node_id)?;
            let gate = node.validation.as_ref()?;
            Some(ValidationEvidence {
                evidence_id: record.evidence_id.clone(),
                gate_id: record.gate_id.clone(),
                gate_type: legacy_validation_gate_type(gate.gate_type),
                command: record.command.clone(),
                normalized_command: super::lifecycle::normalize_command(&record.command),
                command_fingerprint: record.fingerprint.clone(),
                source_tree_hash: record.repository_fingerprint.clone(),
                dependency_lock_hash: gate.dependency_lock_hash.clone(),
                started_at: String::new(),
                completed_at: None,
                duration_ms: u64::try_from(record.duration.as_millis()).unwrap_or(u64::MAX),
                exit_code: record.exit_code,
                status: legacy_validation_status(record.status),
                stdout_summary: record.output_summary.clone(),
                stderr_summary: String::new(),
                source: ValidationSource::ResumeReused,
            })
        })
        .collect()
}

fn materialize_validation_failures(failures: &FailureStore) -> Vec<String> {
    failures
        .unresolved()
        .filter(|failure| failure.category == FailureCategory::ValidationFailure)
        .map(|failure| format!("{}: {}", failure.id, failure.message))
        .collect()
}

fn materialize_required_gates(
    graph: &ExecutionGraph,
    evidence: &EvidenceStore,
) -> Vec<RequiredGate> {
    graph
        .nodes
        .iter()
        .filter(|node| node.required && node.kind.is_validation())
        .filter_map(|node| {
            let gate = node.validation.as_ref()?;
            let evidence_id = node
                .evidence_ids
                .iter()
                .rev()
                .find(|evidence_id| evidence.validations.contains_key(*evidence_id))
                .cloned();
            let status = evidence_id
                .as_ref()
                .and_then(|evidence_id| evidence.validations.get(evidence_id))
                .map_or_else(
                    || legacy_validation_status_from_node(node.status),
                    |evidence| legacy_validation_status(evidence.status),
                );
            Some(RequiredGate {
                gate_id: gate.gate_id.clone(),
                gate_type: legacy_validation_gate_type(gate.gate_type),
                required: node.required,
                command: gate.command.clone(),
                status,
                evidence_id,
            })
        })
        .collect()
}

fn last_successful_domain_action(events: &[ExecutionDomainEvent]) -> Option<Value> {
    events.iter().rev().find_map(|event| {
        let successful = matches!(
            event,
            ExecutionDomainEvent::RepositoryEvidenceRecorded { .. }
                | ExecutionDomainEvent::DiscoveryCompleted { .. }
                | ExecutionDomainEvent::PlanAccepted { .. }
                | ExecutionDomainEvent::PlanRepaired { .. }
                | ExecutionDomainEvent::GraphCreated { .. }
                | ExecutionDomainEvent::MutationApplied { .. }
                | ExecutionDomainEvent::MutationSuperseded { .. }
                | ExecutionDomainEvent::ValidationPassed { .. }
                | ExecutionDomainEvent::DiffReviewed { .. }
                | ExecutionDomainEvent::CompletionEvaluated { .. }
                | ExecutionDomainEvent::CommitCreated { .. }
                | ExecutionDomainEvent::BranchPushed { .. }
                | ExecutionDomainEvent::PullRequestCreated { .. }
        ) || matches!(
            event,
            ExecutionDomainEvent::NodeCompleted { status, .. } if status.is_success()
        );
        successful.then(|| {
            json!({
                "event_type": event.event_type(),
                "sequence": event.sequence(),
                "node_id": event.node_id().map(ExecutionNodeId::as_str),
            })
        })
    })
}

const fn failure_category_code(category: FailureCategory) -> &'static str {
    match category {
        FailureCategory::ModelArtifactRecoverable => "model_artifact_recoverable",
        FailureCategory::ToolRecoverable => "tool_recoverable",
        FailureCategory::MutationConflict => "mutation_conflict",
        FailureCategory::TargetBlocked => "target_blocked",
        FailureCategory::ValidationFailure => "validation_failure",
        FailureCategory::InfrastructureFailure => "infrastructure_failure",
        FailureCategory::OrchestrationInvariantViolation => "orchestration_invariant_violation",
        FailureCategory::UserCancellation => "user_cancellation",
    }
}

/// Facts that exist outside the legacy notebook but are needed to materialize
/// the terminal side of the graph. All fields are optional so old checkpoints
/// can be reconciled without inventing publication state.
#[derive(Clone, Debug, Default)]
pub(super) struct HostedReconciliationFacts {
    pub(super) diff_reviewed: bool,
    pub(super) completion_outcome: Option<MissionOutcome>,
    pub(super) publication: Option<PublicationState>,
}

impl HostedOrchestrationCheckpoint {
    /// Creates the discovery/planning graph with a bounded bootstrap envelope.
    /// Tighter signed limits are respected; complexity is reassessed only after
    /// a plan is accepted.
    pub(super) fn bootstrap(manifest: &HostedManifest, repository_fingerprint: &str) -> Self {
        let assessment = provisional_complexity_assessment(manifest);
        let graph_id = graph_id(manifest);
        let graph = ExecutionGraph::bootstrap(
            graph_id,
            repository_fingerprint,
            assessment.class,
            &assessment.budget,
        );
        let graph_revision = graph.revision;
        Self {
            graph: Some(graph),
            graph_revision,
            budget: BudgetState::new(assessment.budget.clone()),
            complexity: Some(assessment),
            ..Self::default()
        }
    }

    /// Older checkpoints predate the explicit classification-stage field. If
    /// their topology still contains only bootstrap work, restore the semantic
    /// stage rather than accepting serde's authoritative compatibility default.
    pub(super) fn normalize_pre_plan_classification(&mut self, manifest: &HostedManifest) {
        let is_bootstrap_topology = self.graph.as_ref().is_some_and(|graph| {
            !graph.nodes.is_empty()
                && graph.nodes.iter().all(|node| {
                    matches!(
                        node.kind,
                        ExecutionNodeKind::Discovery | ExecutionNodeKind::Planning
                    )
                })
        });
        if !is_bootstrap_topology {
            return;
        }
        if let Some(graph) = self.graph.as_mut() {
            graph.complexity_classification_stage = ComplexityClassificationStage::Provisional;
        }
        if let Some(assessment) = self.complexity.as_mut() {
            assessment.stage = ComplexityClassificationStage::Provisional;
        } else {
            let mut assessment = provisional_complexity_assessment(manifest);
            assessment.budget = self.budget.mission.clone();
            self.complexity = Some(assessment);
        }
    }

    /// Reconstructs the canonical graph from the accepted plan. Target order is
    /// stable: plan order, then target order within each planned change. The
    /// graph builder keeps production targets ahead of test targets while
    /// retaining their relative order within each partition.
    pub(super) fn rebuild_from_plan(
        &mut self,
        manifest: &HostedManifest,
        plan: &ImplementationPlan,
        repository_fingerprint: &str,
    ) -> &ComplexityAssessment {
        let targets = canonical_plan_targets(plan);
        let validation_gates = canonical_validation_gates_for_targets(
            manifest,
            &targets,
            self.dependency_bootstrap_completed,
        );
        let input = complexity_input(&targets, &validation_gates);
        let assessment = complexity_assessment(manifest, &input);
        let mut graph = ExecutionGraph::from_targets(
            graph_id(manifest),
            assessment.class,
            repository_fingerprint,
            &targets,
            &validation_gates,
            &assessment.budget,
        );

        if let Some(previous) = self.graph.clone() {
            let preserved = preserve_pre_plan_graph_progress(&previous, &mut graph);
            graph.revision = previous.revision.saturating_add(1);
            retain_checkpoint_progress_for_nodes(self, &preserved, &graph);
            self.pending_topology_preserved_node_ids = preserved.into_iter().collect();
        } else {
            let preserved = graph
                .nodes
                .iter()
                .filter(|node| {
                    matches!(
                        node.kind,
                        ExecutionNodeKind::Discovery | ExecutionNodeKind::Planning
                    )
                })
                .map(|node| node.id.clone())
                .collect::<BTreeSet<_>>();
            retain_checkpoint_progress_for_nodes(self, &preserved, &graph);
            self.pending_topology_preserved_node_ids = preserved.into_iter().collect();
        }
        graph.refresh_readiness();
        self.graph_revision = graph.revision;
        self.graph = Some(graph);
        // Plan acceptance changes the mission envelope and graph topology, not
        // the mission's accounting epoch. Discovery and planning have already
        // spent calls, cost, and wall-clock time, so retain their usage and all
        // mission totals while applying the plan-derived envelope.
        self.budget.mission = assessment.budget.clone();
        self.complexity = Some(assessment);
        self.complexity
            .as_ref()
            .expect("complexity was stored immediately above")
    }

    /// Rebuilds when an old checkpoint has no graph; otherwise preserves the
    /// persisted graph and its attempts/events.
    pub(super) fn ensure_graph_from_plan(
        &mut self,
        manifest: &HostedManifest,
        plan: &ImplementationPlan,
        repository_fingerprint: &str,
    ) {
        let stale_revision = self
            .graph
            .as_ref()
            .is_some_and(|graph| graph.revision != self.graph_revision);
        if self.graph.is_none() {
            self.rebuild_from_plan(manifest, plan, repository_fingerprint);
            return;
        }

        let topology_changed = self.graph.as_ref().is_some_and(|graph| {
            !graph_matches_plan_topology(
                graph,
                manifest,
                plan,
                repository_fingerprint,
                self.dependency_bootstrap_completed,
            )
        });
        if stale_revision || topology_changed {
            self.reconcile_plan_topology(manifest, plan, repository_fingerprint);
        }
    }

    /// Rebuilds repaired plan topology while retaining progress for every node
    /// whose semantic identity is unchanged. Stable node ids keep persisted
    /// events, evidence, failures, attempts, and budget usage referentially
    /// valid even when inserting or removing another target changes graph
    /// ordering.
    pub(super) fn reconcile_plan_topology(
        &mut self,
        manifest: &HostedManifest,
        plan: &ImplementationPlan,
        repository_fingerprint: &str,
    ) -> &ComplexityAssessment {
        let targets = canonical_plan_targets(plan);
        let validation_gates = canonical_validation_gates_for_targets(
            manifest,
            &targets,
            self.dependency_bootstrap_completed,
        );
        let input = complexity_input(&targets, &validation_gates);
        let assessment = complexity_assessment(manifest, &input);
        let mut replacement = ExecutionGraph::from_targets(
            graph_id(manifest),
            assessment.class,
            repository_fingerprint,
            &targets,
            &validation_gates,
            &assessment.budget,
        );

        if let Some(previous) = self.graph.clone() {
            let preserved = preserve_unchanged_graph_progress(&previous, &mut replacement);
            retain_checkpoint_progress_for_nodes(self, &preserved, &replacement);
            self.pending_topology_preserved_node_ids = preserved.into_iter().collect();
            replacement.revision = previous.revision.saturating_add(1);
        } else {
            self.pending_topology_preserved_node_ids.clear();
        }
        replacement.refresh_readiness();
        self.graph_revision = replacement.revision;
        self.graph = Some(replacement);
        self.budget.mission = assessment.budget.clone();
        self.complexity = Some(assessment);
        self.complexity
            .as_ref()
            .expect("complexity was stored immediately above")
    }

    /// Imports a pre-graph notebook exactly once. After this migration boundary
    /// the graph and domain events are authoritative and callers may only
    /// materialize compatibility state from them.
    pub(super) fn import_legacy_state_once(
        &mut self,
        notebook: &WorkerNotebook,
        authoritative_changed_paths: &[String],
        facts: &HostedReconciliationFacts,
    ) -> bool {
        if !self.legacy_import_pending() || self.graph.is_none() {
            return false;
        }
        self.import_legacy_state(notebook, authoritative_changed_paths, facts);
        self.legacy_import_completed = true;
        true
    }

    pub(super) const fn legacy_import_pending(&self) -> bool {
        !self.legacy_import_completed
    }

    /// Starts a new event epoch for a resumable cancellation or published
    /// partial result, but only when a strictly newer hosted attempt begins.
    /// The reducer clears cancellation or reopens remaining graph work so
    /// serialized checkpoints and deterministic replay remain identical.
    pub(super) fn resume_for_new_attempt(
        &mut self,
        run_id: impl Into<String>,
        repository: RepositorySnapshot,
        previous_attempt: u32,
        execution_attempt: u32,
    ) -> Result<Option<HostedResumeReason>, GraphInvariantError> {
        if execution_attempt <= previous_attempt {
            return Ok(None);
        }
        let mut snapshot = self.snapshot(run_id, repository);
        let (reason, previous_outcome) =
            if snapshot.cancellation.is_some() && snapshot.terminal_outcome().is_none() {
                (HostedResumeReason::Cancellation, None)
            } else if snapshot.terminal_outcome() == Some(MissionOutcome::PartialReviewable) {
                (
                    HostedResumeReason::PartialReviewable,
                    Some(MissionOutcome::PartialReviewable),
                )
            } else {
                return Ok(None);
            };
        snapshot.append_event(ExecutionDomainEvent::ExecutionResumed {
            sequence: next_event_sequence(&snapshot.events),
            execution_attempt,
            previous_outcome,
        })?;
        self.replace_from_snapshot(&snapshot);
        Ok(Some(reason))
    }

    fn import_legacy_state(
        &mut self,
        notebook: &WorkerNotebook,
        authoritative_changed_paths: &[String],
        facts: &HostedReconciliationFacts,
    ) {
        let changed_paths = authoritative_changed_paths
            .iter()
            .map(|path| path.trim().to_owned())
            .filter(|path| !path.is_empty())
            .collect::<BTreeSet<_>>();
        let legacy_statuses = legacy_target_statuses(notebook);
        let mutation_path_counts = self
            .graph
            .as_ref()
            .map(mutation_path_counts)
            .unwrap_or_default();
        let evidenced_mutations = authoritative_mutation_node_ids(self);

        if let Some(graph) = self.graph.as_mut() {
            let mut changed = false;
            let discovery_complete = !matches!(
                notebook.phase,
                ExecutionPhase::Discovery | ExecutionPhase::ArtifactRepair
            );
            let planning_complete = !matches!(
                notebook.phase,
                ExecutionPhase::Discovery
                    | ExecutionPhase::ArtifactRepair
                    | ExecutionPhase::Planning
            );
            for node in &mut graph.nodes {
                let compatibility_status = match node.kind {
                    ExecutionNodeKind::Discovery if discovery_complete => {
                        Some(ExecutionNodeStatus::Completed)
                    }
                    ExecutionNodeKind::Planning if planning_complete => {
                        Some(ExecutionNodeStatus::Completed)
                    }
                    _ => None,
                };
                if let Some(status) = compatibility_status
                    && node.status != status
                {
                    node.status = status;
                    changed = true;
                }
            }
            for node in graph
                .nodes
                .iter_mut()
                .filter(|node| node.kind.is_mutation())
            {
                let Some(target) = node.target.as_ref() else {
                    continue;
                };
                let legacy_status = legacy_statuses
                    .get(&(target.change_id.clone(), target.path.clone()))
                    .copied();
                let path_can_identify_node = mutation_path_counts
                    .get(&target.path)
                    .copied()
                    .unwrap_or_default()
                    == 1;
                let matching_status_is_applied = legacy_status.is_some_and(|status| {
                    matches!(
                        status,
                        IntendedChangeStatus::Applied | IntendedChangeStatus::Verified
                    )
                });
                let status = if changed_paths.contains(&target.path)
                    && (path_can_identify_node
                        || node.status.is_success()
                        || evidenced_mutations.contains(&node.id)
                        || matching_status_is_applied)
                {
                    ExecutionNodeStatus::Applied
                } else {
                    legacy_status
                        .map(graph_status_from_legacy)
                        .unwrap_or(node.status)
                };
                if node.status != status {
                    node.status = status;
                    changed = true;
                }
            }
            if changed {
                graph.revision = graph.revision.saturating_add(1);
            }
            graph.refresh_readiness();
        }

        self.synchronize_failures(notebook);
        self.synchronize_validation(notebook);

        let inferred_diff_reviewed = facts.diff_reviewed
            || matches!(
                notebook.phase,
                ExecutionPhase::CompletionEvaluation | ExecutionPhase::Publication
            );
        let persisted_completion = self
            .domain_events
            .iter()
            .rev()
            .find_map(|event| match event {
                ExecutionDomainEvent::CompletionEvaluated { outcome, .. } => Some(*outcome),
                _ => None,
            });
        let inferred_completion = facts
            .completion_outcome
            .or(persisted_completion)
            .or_else(|| {
                (notebook.phase == ExecutionPhase::Publication).then_some(MissionOutcome::Complete)
            });
        self.synchronize_review_and_publication(
            inferred_diff_reviewed,
            inferred_completion,
            facts.publication.as_ref(),
        );

        if let Some(graph) = self.graph.as_mut() {
            graph.refresh_readiness();
            self.graph_revision = graph.revision;
        }
    }

    /// Returns the immutable value consumed by the pure orchestrator.
    pub(super) fn snapshot(
        &self,
        run_id: impl Into<String>,
        current_repository: RepositorySnapshot,
    ) -> ExecutionSnapshot {
        ExecutionSnapshot {
            run_id: run_id.into(),
            current_repository,
            graph: self.graph.clone().unwrap_or_default(),
            events: self.domain_events.clone(),
            evidence: self.evidence.clone(),
            failures: self.failures.clone(),
            budget: self.budget.clone(),
            cancellation: self.cancellation.clone(),
            publication: self.publication.clone(),
        }
    }

    /// Stores a snapshot after the sole decision adapter has applied a domain
    /// event. This is an in-memory state replacement, not persistence I/O.
    pub(super) fn replace_from_snapshot(&mut self, snapshot: &ExecutionSnapshot) {
        self.graph_revision = snapshot.graph.revision;
        self.graph = Some(snapshot.graph.clone());
        self.domain_events = snapshot.events.clone();
        self.evidence = snapshot.evidence.clone();
        self.failures = snapshot.failures.clone();
        self.budget = snapshot.budget.clone();
        self.cancellation = snapshot.cancellation.clone();
        self.publication = snapshot.publication.clone();
    }

    pub(super) fn hosted_stage(&self) -> HostedExecutionStage {
        self.graph
            .as_ref()
            .map_or(HostedExecutionStage::Discovery, ExecutionGraph::stage)
    }

    pub(super) fn execution_phase(&self, current: ExecutionPhase) -> ExecutionPhase {
        let Some(graph) = self.graph.as_ref() else {
            return current;
        };
        let active_kind = graph
            .next_runnable_node()
            .map(|node| node.kind)
            .or_else(|| {
                graph
                    .nodes
                    .iter()
                    .find(|node| node.required && !node.status.is_success())
                    .map(|node| node.kind)
            });
        match active_kind {
            Some(ExecutionNodeKind::Discovery) => {
                if current == ExecutionPhase::ArtifactRepair {
                    current
                } else {
                    ExecutionPhase::Discovery
                }
            }
            Some(ExecutionNodeKind::Planning) => ExecutionPhase::Planning,
            Some(ExecutionNodeKind::SourceMutation | ExecutionNodeKind::TestMutation) => {
                if self.failures.has_unresolved() {
                    ExecutionPhase::Repair
                } else {
                    ExecutionPhase::Implementation
                }
            }
            Some(
                ExecutionNodeKind::ValidationFocused
                | ExecutionNodeKind::ValidationSuite
                | ExecutionNodeKind::ValidationBuild
                | ExecutionNodeKind::ValidationLint,
            ) => ExecutionPhase::Validation,
            Some(ExecutionNodeKind::DiffReview) => ExecutionPhase::DiffReview,
            Some(ExecutionNodeKind::CompletionEvaluation) => ExecutionPhase::CompletionEvaluation,
            Some(ExecutionNodeKind::Publication) => ExecutionPhase::Publication,
            None => current,
        }
    }

    /// Materializes only compatibility fields. The graph remains authoritative;
    /// callers must persist the checkpoint together with the notebook.
    pub(super) fn materialize_legacy_notebook(&self, notebook: &mut WorkerNotebook) {
        notebook.validation_failures = materialize_validation_failures(&self.failures);
        let Some(graph) = self.graph.as_ref() else {
            return;
        };
        let existing_failures = notebook.failed_changes.clone();
        notebook.phase = self.execution_phase(notebook.phase);
        notebook.implementation_substate = match notebook.phase {
            ExecutionPhase::Repair => ImplementationSubstate::Repairing,
            ExecutionPhase::Implementation => ImplementationSubstate::Mutating,
            ExecutionPhase::Validation
            | ExecutionPhase::DiffReview
            | ExecutionPhase::CompletionEvaluation
            | ExecutionPhase::Publication => ImplementationSubstate::ReadyForValidation,
            ExecutionPhase::Discovery
            | ExecutionPhase::ArtifactRepair
            | ExecutionPhase::Planning => ImplementationSubstate::Preparing,
        };

        let (planned_changes, intended_changes) = materialize_legacy_changes(graph);
        notebook.planned_changes = planned_changes;
        notebook.intended_changes = intended_changes;

        notebook.completed_changes = completed_change_ids(graph);
        notebook.remaining_work_v2 = graph
            .nodes
            .iter()
            .filter(|node| node.required && !node.status.is_success())
            .map(remaining_work_item)
            .collect();
        notebook.remaining_work = notebook
            .remaining_work_v2
            .iter()
            .map(|item| format!("{}: {}", item.path, item.reason))
            .collect();
        notebook.failed_changes =
            materialize_failed_changes(graph, &self.failures, &existing_failures);
        notebook.validation_evidence = materialize_validation_evidence(graph, &self.evidence);
        notebook.required_gates = materialize_required_gates(graph, &self.evidence);
        notebook.last_successful_action =
            last_successful_domain_action(&self.domain_events).unwrap_or_else(|| json!({}));
    }

    fn synchronize_failures(&mut self, notebook: &WorkerNotebook) {
        let Some(graph) = self.graph.as_mut() else {
            return;
        };
        let mut graph_changed = false;
        for legacy in &notebook.failed_changes {
            let Some(node_id) = mutation_node_for_failure(graph, legacy) else {
                continue;
            };
            let target_path = legacy.target.clone();
            let id = stable_failure_id(legacy, &node_id);
            let node_is_applied = graph
                .node(&node_id)
                .is_some_and(|node| node.status.is_success());
            let status = match legacy.reconciliation {
                FailureReconciliation::Recovered => FailureStatus::Recovered,
                FailureReconciliation::Superseded => FailureStatus::Superseded,
                FailureReconciliation::Unrelated => continue,
                FailureReconciliation::StillUnresolved => {
                    if legacy.recovered {
                        FailureStatus::Recovered
                    } else if node_is_applied {
                        FailureStatus::Superseded
                    } else {
                        FailureStatus::Active
                    }
                }
            };
            let mut failure = FailureRecord::new(
                id,
                node_id.clone(),
                FailureCategory::ToolRecoverable,
                u32::try_from(legacy.attempt_index).unwrap_or(u32::MAX),
                notebook.repository_fingerprint.clone(),
                legacy.error.clone(),
            );
            failure.target_path = target_path.clone();
            match status {
                FailureStatus::Recovered => {
                    failure.mark_recovered(notebook.repository_fingerprint.clone())
                }
                FailureStatus::Superseded => {
                    failure.mark_superseded(notebook.repository_fingerprint.clone())
                }
                FailureStatus::Active => {}
            }
            self.failures.record(failure);

            if matches!(status, FailureStatus::Recovered | FailureStatus::Superseded) {
                if let Some(node) = graph.node_mut(&node_id) {
                    let status = if node_is_applied {
                        ExecutionNodeStatus::Applied
                    } else {
                        ExecutionNodeStatus::Pending
                    };
                    if node.status != status {
                        node.status = status;
                        graph_changed = true;
                    }
                }
            } else if status == FailureStatus::Active
                && let Some(node) = graph.node_mut(&node_id)
                && !node.status.is_success()
                && node.status != ExecutionNodeStatus::FailedRecoverable
            {
                node.status = ExecutionNodeStatus::FailedRecoverable;
                graph_changed = true;
            }
        }
        if graph_changed {
            graph.revision = graph.revision.saturating_add(1);
            graph.refresh_readiness();
        }
    }

    fn synchronize_validation(&mut self, notebook: &WorkerNotebook) {
        let Some(graph) = self.graph.as_mut() else {
            return;
        };
        let mut graph_changed = false;
        let required_statuses = notebook
            .required_gates
            .iter()
            .map(|gate| (gate.gate_id.as_str(), gate.status))
            .collect::<BTreeMap<_, _>>();
        for legacy in &notebook.validation_evidence {
            let Some(node_id) = graph
                .nodes
                .iter()
                .find(|node| {
                    node.validation
                        .as_ref()
                        .is_some_and(|gate| gate.gate_id == legacy.gate_id)
                })
                .map(|node| node.id.clone())
            else {
                continue;
            };
            let status = graph_validation_status(legacy.status);
            self.evidence.record_validation(ValidationEvidenceRecord {
                evidence_id: legacy.evidence_id.clone(),
                node_id: node_id.clone(),
                gate_id: legacy.gate_id.clone(),
                fingerprint: legacy.command_fingerprint.clone(),
                repository_fingerprint: legacy.source_tree_hash.clone(),
                command: legacy.command.clone(),
                working_directory: String::new(),
                status,
                exit_code: legacy.exit_code,
                output_summary: validation_output_summary(legacy),
                duration: Duration::from_millis(legacy.duration_ms),
            });
            if let Some(node) = graph.node_mut(&node_id) {
                if !node.evidence_ids.contains(&legacy.evidence_id) {
                    node.evidence_ids.push(legacy.evidence_id.clone());
                    graph_changed = true;
                }
                let node_status = graph_node_status_from_validation(status);
                if node.status != node_status {
                    node.status = node_status;
                    graph_changed = true;
                }
            }
        }
        for node in graph
            .nodes
            .iter_mut()
            .filter(|node| node.kind.is_validation())
        {
            let Some(gate) = node.validation.as_ref() else {
                continue;
            };
            if let Some(status) = required_statuses.get(gate.gate_id.as_str()) {
                let current_evidence = notebook.validation_evidence.iter().any(|evidence| {
                    evidence.gate_id == gate.gate_id
                        && evidence.source_tree_hash == notebook.repository_fingerprint
                        && evidence.status != ValidationStatus::Superseded
                });
                let status = if current_evidence {
                    graph_node_status_from_validation(graph_validation_status(*status))
                } else if matches!(
                    node.status,
                    ExecutionNodeStatus::FailedRecoverable | ExecutionNodeStatus::Running
                ) {
                    ExecutionNodeStatus::Pending
                } else {
                    node.status
                };
                if node.status != status {
                    node.status = status;
                    graph_changed = true;
                }
            }
        }
        if graph_changed {
            graph.revision = graph.revision.saturating_add(1);
            graph.refresh_readiness();
        }
    }

    fn synchronize_review_and_publication(
        &mut self,
        diff_reviewed: bool,
        completion_outcome: Option<MissionOutcome>,
        publication: Option<&PublicationState>,
    ) {
        if let Some(publication) = publication {
            self.publication = publication.clone();
        }
        let Some(graph) = self.graph.as_mut() else {
            return;
        };
        let mut graph_changed = false;
        if diff_reviewed
            && let Some(node_id) = graph
                .nodes
                .iter()
                .find(|node| node.kind == ExecutionNodeKind::DiffReview)
                .map(|node| node.id.clone())
        {
            if let Some(node) = graph.node_mut(&node_id)
                && node.status != ExecutionNodeStatus::Completed
            {
                node.status = ExecutionNodeStatus::Completed;
                graph_changed = true;
            }
            if !self
                .domain_events
                .iter()
                .any(|event| matches!(event, ExecutionDomainEvent::DiffReviewed { .. }))
            {
                self.domain_events.push(ExecutionDomainEvent::DiffReviewed {
                    sequence: next_event_sequence(&self.domain_events),
                    node_id,
                    evidence_ids: Vec::new(),
                });
            }
        }
        if let Some(outcome) = completion_outcome
            && let Some(node_id) = graph
                .nodes
                .iter()
                .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
                .map(|node| node.id.clone())
        {
            if let Some(node) = graph.node_mut(&node_id)
                && node.status != ExecutionNodeStatus::Completed
            {
                node.status = ExecutionNodeStatus::Completed;
                graph_changed = true;
            }
            if !self.domain_events.iter().any(|event| {
                matches!(
                    event,
                    ExecutionDomainEvent::CompletionEvaluated {
                        outcome: existing,
                        ..
                    } if *existing == outcome
                )
            }) {
                self.domain_events
                    .push(ExecutionDomainEvent::CompletionEvaluated {
                        sequence: next_event_sequence(&self.domain_events),
                        node_id,
                        outcome,
                    });
            }
        }
        if self.publication.status == PublicationStatus::PullRequestCreated
            && let Some(node_id) = graph
                .nodes
                .iter()
                .find(|node| node.kind == ExecutionNodeKind::Publication)
                .map(|node| node.id.clone())
        {
            if let Some(node) = graph.node_mut(&node_id)
                && node.status != ExecutionNodeStatus::Completed
            {
                node.status = ExecutionNodeStatus::Completed;
                graph_changed = true;
            }
            if let Some(url) = self.publication.pull_request_url.clone()
                && !self.domain_events.iter().any(|event| {
                    matches!(
                        event,
                        ExecutionDomainEvent::PullRequestCreated {
                            url: existing,
                            ..
                        } if existing == &url
                    )
                })
            {
                self.domain_events
                    .push(ExecutionDomainEvent::PullRequestCreated {
                        sequence: next_event_sequence(&self.domain_events),
                        node_id,
                        url,
                        number: self.publication.pull_request_number,
                        draft: self.publication.draft,
                    });
            }
        }
        if graph_changed {
            graph.revision = graph.revision.saturating_add(1);
            graph.refresh_readiness();
        }
    }
}

fn graph_matches_plan_topology(
    graph: &ExecutionGraph,
    manifest: &HostedManifest,
    plan: &ImplementationPlan,
    repository_fingerprint: &str,
    dependency_bootstrap_completed: bool,
) -> bool {
    let targets = canonical_plan_targets(plan);
    let validation_gates =
        canonical_validation_gates_for_targets(manifest, &targets, dependency_bootstrap_completed);
    let assessment =
        complexity_assessment(manifest, &complexity_input(&targets, &validation_gates));
    let expected = ExecutionGraph::from_targets(
        graph_id(manifest),
        assessment.class,
        repository_fingerprint,
        &targets,
        &validation_gates,
        &assessment.budget,
    );
    graph_topology_signature(graph) == graph_topology_signature(&expected)
}

fn graph_topology_signature(graph: &ExecutionGraph) -> Vec<String> {
    graph
        .nodes
        .iter()
        .map(|node| {
            let dependencies = node
                .dependencies
                .iter()
                .filter_map(|dependency| graph.node(dependency))
                .map(node_semantic_identity)
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{}|required={}|target={}|validation={}|dependencies={dependencies}",
                node_semantic_identity(node),
                node.required,
                node.target
                    .as_ref()
                    .and_then(|target| serde_json::to_string(target).ok())
                    .unwrap_or_default(),
                node.validation
                    .as_ref()
                    .map(validation_gate_topology_signature)
                    .unwrap_or_default(),
            )
        })
        .collect()
}

/// Carries the completed pre-plan work into the accepted-plan graph without
/// copying its transient status. `from_targets` authoritatively materializes
/// discovery and planning as completed; only their attempts and evidence are
/// durable work products that must cross this topology boundary.
fn preserve_pre_plan_graph_progress(
    previous: &ExecutionGraph,
    replacement: &mut ExecutionGraph,
) -> BTreeSet<ExecutionNodeId> {
    let mut preserved = BTreeSet::new();
    for kind in [ExecutionNodeKind::Discovery, ExecutionNodeKind::Planning] {
        let Some(previous_node) = previous.nodes.iter().find(|node| node.kind == kind) else {
            continue;
        };
        let Some(replacement_node) = replacement.nodes.iter_mut().find(|node| node.kind == kind)
        else {
            continue;
        };
        replacement_node.attempts = previous_node.attempts.clone();
        replacement_node.evidence_ids = previous_node.evidence_ids.clone();
        preserved.insert(replacement_node.id.clone());
    }
    preserved
}

fn preserve_unchanged_graph_progress(
    previous: &ExecutionGraph,
    replacement: &mut ExecutionGraph,
) -> BTreeSet<ExecutionNodeId> {
    let mut used_previous = BTreeSet::<ExecutionNodeId>::new();
    let mut id_remap = BTreeMap::<ExecutionNodeId, ExecutionNodeId>::new();
    let mut previous_nodes = BTreeMap::new();

    for node in &replacement.nodes {
        let identity = node_semantic_identity(node);
        let Some(previous_node) = previous.nodes.iter().find(|candidate| {
            !used_previous.contains(&candidate.id) && node_semantic_identity(candidate) == identity
        }) else {
            continue;
        };
        used_previous.insert(previous_node.id.clone());
        id_remap.insert(node.id.clone(), previous_node.id.clone());
        previous_nodes.insert(node.id.clone(), previous_node.clone());
    }

    for node in &mut replacement.nodes {
        if let Some(stable_id) = id_remap.get(&node.id) {
            node.id = stable_id.clone();
        }
        for dependency in &mut node.dependencies {
            if let Some(stable_id) = id_remap.get(dependency) {
                *dependency = stable_id.clone();
            }
        }
    }

    let previous_by_stable_id = previous_nodes
        .into_values()
        .map(|node| (node.id.clone(), node))
        .collect::<BTreeMap<_, _>>();
    let mut preserved = BTreeSet::new();
    let mut stable_topology = BTreeSet::new();
    for node in &mut replacement.nodes {
        let Some(previous_node) = previous_by_stable_id.get(&node.id) else {
            continue;
        };
        let payload_unchanged = previous_node.kind == node.kind
            && previous_node.target == node.target
            && validation_gate_topology_matches(
                previous_node.validation.as_ref(),
                node.validation.as_ref(),
            );
        let dependencies_unchanged = previous_node.dependencies == node.dependencies;
        let dependency_lineage_unchanged = dependencies_unchanged
            && node
                .dependencies
                .iter()
                .all(|dependency| stable_topology.contains(dependency));
        if payload_unchanged && dependency_lineage_unchanged {
            stable_topology.insert(node.id.clone());
        }
        let mutation_progress_is_repository_scoped = node.kind.is_mutation() && payload_unchanged;
        if payload_unchanged
            && (mutation_progress_is_repository_scoped || dependency_lineage_unchanged)
        {
            if previous_node.validation.is_some() {
                node.validation.clone_from(&previous_node.validation);
            }
            node.status = previous_node.status;
            node.attempts = previous_node.attempts.clone();
            node.evidence_ids = previous_node.evidence_ids.clone();
            preserved.insert(node.id.clone());
        }
    }
    replacement.dependency_satisfaction_overrides = previous
        .dependency_satisfaction_overrides
        .intersection(&preserved)
        .cloned()
        .collect();
    preserved
}

fn validation_gate_topology_signature(gate: &GraphValidationGateSpec) -> String {
    format!(
        "{}|{}|{}|{}",
        gate.gate_id,
        graph_validation_gate_type_label(gate.gate_type),
        gate.required,
        gate.command
    )
}

fn validation_gate_topology_matches(
    previous: Option<&GraphValidationGateSpec>,
    replacement: Option<&GraphValidationGateSpec>,
) -> bool {
    match (previous, replacement) {
        (Some(previous), Some(replacement)) => {
            validation_gate_topology_signature(previous)
                == validation_gate_topology_signature(replacement)
        }
        (None, None) => true,
        _ => false,
    }
}

const fn graph_validation_gate_type_label(gate_type: GraphValidationGateType) -> &'static str {
    match gate_type {
        GraphValidationGateType::FocusedTest => "focused_test",
        GraphValidationGateType::TestSuite => "test_suite",
        GraphValidationGateType::Build => "build",
        GraphValidationGateType::Lint => "lint",
        GraphValidationGateType::Typecheck => "typecheck",
        GraphValidationGateType::Custom => "custom",
    }
}

fn retain_checkpoint_progress_for_nodes(
    checkpoint: &mut HostedOrchestrationCheckpoint,
    preserved: &BTreeSet<ExecutionNodeId>,
    replacement: &ExecutionGraph,
) {
    // Domain history is append-only. A GraphCreated event carries each new
    // topology, so events for removed nodes remain replayable against the
    // graph generation in which they originally occurred.
    checkpoint
        .failures
        .records
        .retain(|failure| preserved.contains(&failure.node_id));
    checkpoint
        .evidence
        .validations
        .retain(|_, evidence| preserved.contains(&evidence.node_id));
    checkpoint.evidence.records.retain(|_, evidence| {
        evidence
            .node_id
            .as_ref()
            .is_none_or(|node_id| preserved.contains(node_id))
    });
    checkpoint
        .budget
        .node_usage
        .retain(|node_id, _| preserved.contains(node_id));
    checkpoint.budget.progress_events.retain(|event| {
        event
            .node_id
            .as_ref()
            .is_none_or(|node_id| preserved.contains(node_id))
    });
    checkpoint.budget.progress_score = checkpoint
        .budget
        .progress_events
        .iter()
        .map(|event| u64::from(event.kind.score()))
        .sum();
    let publication_preserved = replacement
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::Publication)
        .is_some_and(|node| preserved.contains(&node.id));
    if !publication_preserved {
        checkpoint.publication = PublicationState::default();
    }
}

fn node_semantic_identity(node: &crate::execution_graph::ExecutionNode) -> String {
    if let Some(target) = node.target.as_ref() {
        return format!(
            "{}:target:{}:{}",
            node_kind_label(node.kind),
            target.change_id,
            target.path
        );
    }
    if let Some(gate) = node.validation.as_ref() {
        return format!("{}:validation:{}", node_kind_label(node.kind), gate.gate_id);
    }
    node_kind_label(node.kind).to_owned()
}

const fn node_kind_label(kind: ExecutionNodeKind) -> &'static str {
    match kind {
        ExecutionNodeKind::Discovery => "discovery",
        ExecutionNodeKind::Planning => "planning",
        ExecutionNodeKind::SourceMutation => "source_mutation",
        ExecutionNodeKind::TestMutation => "test_mutation",
        ExecutionNodeKind::ValidationFocused => "validation_focused",
        ExecutionNodeKind::ValidationSuite => "validation_suite",
        ExecutionNodeKind::ValidationBuild => "validation_build",
        ExecutionNodeKind::ValidationLint => "validation_lint",
        ExecutionNodeKind::DiffReview => "diff_review",
        ExecutionNodeKind::CompletionEvaluation => "completion_evaluation",
        ExecutionNodeKind::Publication => "publication",
    }
}

pub(super) fn canonical_plan_targets(plan: &ImplementationPlan) -> Vec<GraphPlannedTarget> {
    let mut targets = Vec::new();
    for (change_index, change) in plan.planned_changes.iter().enumerate() {
        let change_id = canonical_change_id(change, change_index);
        if change.targets.is_empty() && !change.path.trim().is_empty() {
            targets.push(GraphPlannedTarget {
                change_id,
                path: change.path.trim().to_owned(),
                role: String::new(),
                intent: change.change.clone(),
                acceptance_criteria_ids: canonical_criterion_ids(&change.acceptance_criteria),
                new_file: plan
                    .planned_new_files
                    .iter()
                    .any(|path| path == &change.path),
            });
            continue;
        }
        for target in &change.targets {
            if target.path.trim().is_empty() {
                continue;
            }
            targets.push(GraphPlannedTarget {
                change_id: change_id.clone(),
                path: target.path.trim().to_owned(),
                role: target.role.trim().to_owned(),
                intent: change.change.clone(),
                acceptance_criteria_ids: canonical_criterion_ids(&change.acceptance_criteria),
                new_file: target.new_file
                    || plan
                        .planned_new_files
                        .iter()
                        .any(|path| path == &target.path),
            });
        }
    }
    targets
}

#[cfg(test)]
pub(super) fn canonical_validation_gates(
    manifest: &HostedManifest,
) -> Vec<GraphValidationGateSpec> {
    canonical_validation_gates_for_targets(manifest, &[], true)
}

fn canonical_validation_gates_for_targets(
    manifest: &HostedManifest,
    targets: &[GraphPlannedTarget],
    dependency_bootstrap_completed: bool,
) -> Vec<GraphValidationGateSpec> {
    let dependency_changed = targets.iter().any(|target| dependency_path(&target.path));
    let mut gates = manifest
        .execution_policy
        .quality_gates
        .iter()
        .filter(|gate| {
            dependency_changed
                || !dependency_bootstrap_completed
                || !is_dependency_install_command(&gate.command)
        })
        .map(|gate| GraphValidationGateSpec {
            gate_id: gate.id.clone(),
            gate_type: infer_validation_gate_type(&gate.id, &gate.command),
            command: gate.command.clone(),
            working_directory: String::new(),
            required: gate.required,
            dependency_lock_hash: String::new(),
            relevant_environment_fingerprint: String::new(),
        })
        .collect::<Vec<_>>();
    let has_vitest_suite = gates.iter().any(|gate| {
        gate.gate_type == GraphValidationGateType::TestSuite
            && (gate.command.to_ascii_lowercase().contains("vitest")
                || gate.command.to_ascii_lowercase().starts_with("npm test"))
    });
    if has_vitest_suite {
        for target in targets.iter().filter(|target| {
            !target.new_file && target.is_test_target() && is_vitest_test_path(&target.path)
        }) {
            let gate_id = format!("focused-{}", focused_gate_label(&target.path));
            if gates.iter().any(|gate| gate.gate_id == gate_id) {
                continue;
            }
            gates.push(GraphValidationGateSpec {
                gate_id,
                gate_type: GraphValidationGateType::FocusedTest,
                command: format!("npx vitest run {}", target.path),
                working_directory: String::new(),
                required: true,
                dependency_lock_hash: String::new(),
                relevant_environment_fingerprint: String::new(),
            });
        }
    }
    normalize_validation_gate_order(&mut gates);
    gates
}

fn is_dependency_install_command(command: &str) -> bool {
    let normalized = command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    [
        "npm ci",
        "npm install",
        "pnpm install",
        "yarn install",
        "bun install",
    ]
    .iter()
    .any(|prefix| normalized.starts_with(prefix))
}

fn is_vitest_test_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    [
        ".test.ts",
        ".test.tsx",
        ".spec.ts",
        ".spec.tsx",
        ".test.js",
        ".test.jsx",
        ".spec.js",
        ".spec.jsx",
    ]
    .iter()
    .any(|suffix| path.ends_with(suffix))
}

fn focused_gate_label(path: &str) -> String {
    path.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_ascii_lowercase()
}

pub(super) fn mission_outcome_from_completion(status: CompletionStatus) -> MissionOutcome {
    match status {
        CompletionStatus::Complete => MissionOutcome::Complete,
        CompletionStatus::CompletePendingExternalReview => {
            MissionOutcome::CompletePendingExternalReview
        }
        CompletionStatus::Partial | CompletionStatus::Incomplete | CompletionStatus::Uncertain => {
            MissionOutcome::PartialReviewable
        }
        CompletionStatus::Blocked => MissionOutcome::BlockedNoDiff,
    }
}

fn graph_id(manifest: &HostedManifest) -> String {
    format!("execution-{}", manifest.execution.execution_id)
}

fn next_event_sequence(events: &[ExecutionDomainEvent]) -> u64 {
    events
        .last()
        .map_or(1, |event| event.sequence().saturating_add(1))
}

fn complexity_assessment(
    manifest: &HostedManifest,
    input: &ComplexityInput,
) -> ComplexityAssessment {
    let default = ComplexityAssessment::classify(input);
    let policy = manifest_budget_override(manifest, &default);
    ComplexityAssessment::classify_with_policy(input, &policy)
}

fn provisional_complexity_assessment(manifest: &HostedManifest) -> ComplexityAssessment {
    let input = ComplexityInput {
        repository_count: 1,
        ..ComplexityInput::default()
    };
    let default = ComplexityAssessment::classify(&input);
    let policy = manifest_budget_override(manifest, &default);
    let baseline = MissionBudget::for_complexity(MissionComplexity::Tiny);
    let requested = baseline.applying_override(&policy);
    ComplexityAssessment {
        stage: ComplexityClassificationStage::Provisional,
        class: MissionComplexity::Tiny,
        score: default.score,
        factors: default.factors,
        budget: MissionBudget {
            max_model_calls: requested.max_model_calls.min(baseline.max_model_calls),
            max_cost_micros: requested.max_cost_micros.min(baseline.max_cost_micros),
            max_duration: requested.max_duration.min(baseline.max_duration),
            max_target_repair_rounds: requested
                .max_target_repair_rounds
                .min(baseline.max_target_repair_rounds),
        },
    }
}

fn manifest_budget_override(
    manifest: &HostedManifest,
    default: &ComplexityAssessment,
) -> MissionBudgetOverride {
    // A user/project selection (and legacy signed manifests, which had no
    // source discriminator) is an explicit override. A system-default envelope
    // is only a ceiling; it must not erase the complexity defaults.
    let explicit_model_budget = manifest.manifest_version < 4
        || matches!(
            manifest.budget_source,
            Some(super::BudgetSource::UserSelected | super::BudgetSource::ProjectDefault)
        );
    let policy_duration = u64::try_from(manifest.execution_policy.timeout_seconds)
        .ok()
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .filter(|duration| *duration < default.budget.max_duration);
    MissionBudgetOverride {
        max_model_calls: explicit_model_budget
            .then(|| u32::try_from(manifest.ai_gateway.maximum_model_calls).ok())
            .flatten(),
        max_cost_micros: explicit_model_budget
            .then(|| parse_usd_micros(&manifest.ai_gateway.maximum_cost_usd))
            .flatten(),
        max_duration: policy_duration,
        max_target_repair_rounds: None,
    }
}

fn parse_usd_micros(value: &str) -> Option<u64> {
    let value = value.trim().strip_prefix('$').unwrap_or(value.trim());
    if value.starts_with('-') || value.is_empty() {
        return None;
    }
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if !whole.chars().all(|character| character.is_ascii_digit())
        || !fraction.chars().all(|character| character.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.parse::<u64>().ok()?;
    let mut fractional = fraction.chars().take(6).collect::<String>();
    while fractional.len() < 6 {
        fractional.push('0');
    }
    let fractional = if fractional.is_empty() {
        0
    } else {
        fractional.parse::<u64>().ok()?
    };
    whole.checked_mul(1_000_000)?.checked_add(fractional)
}

fn complexity_input(
    targets: &[GraphPlannedTarget],
    gates: &[GraphValidationGateSpec],
) -> ComplexityInput {
    let dependency_change_count = count_paths(targets, dependency_path);
    let database_schema_change_count = count_paths(targets, schema_path);
    let security_sensitive_change_count = count_paths(targets, security_path);
    let external_integration_count = count_paths(targets, integration_path);
    let modules = targets
        .iter()
        .filter_map(|target| target.path.split('/').next())
        .filter(|segment| !segment.is_empty())
        .collect::<BTreeSet<_>>()
        .len();
    ComplexityInput {
        planned_target_count: u32::try_from(targets.len()).unwrap_or(u32::MAX),
        new_file_count: u32::try_from(targets.iter().filter(|target| target.new_file).count())
            .unwrap_or(u32::MAX),
        repository_count: 1,
        dependency_change_count,
        database_schema_change_count,
        external_integration_count,
        security_sensitive_change_count,
        architectural_uncertainty: 0,
        test_surface: u32::try_from(
            targets
                .iter()
                .filter(|target| target.is_test_target())
                .count(),
        )
        .unwrap_or(u32::MAX),
        expected_validation_duration: Duration::from_secs(
            u64::try_from(gates.len())
                .unwrap_or(u64::MAX)
                .saturating_mul(120),
        ),
        cross_module_impact: u32::try_from(modules.saturating_sub(1)).unwrap_or(u32::MAX),
    }
}

fn count_paths(targets: &[GraphPlannedTarget], predicate: impl Fn(&str) -> bool) -> u32 {
    u32::try_from(
        targets
            .iter()
            .filter(|target| predicate(&target.path.to_ascii_lowercase()))
            .count(),
    )
    .unwrap_or(u32::MAX)
}

fn dependency_path(path: &str) -> bool {
    path.ends_with("cargo.toml")
        || path.ends_with("cargo.lock")
        || path.ends_with("package.json")
        || path.ends_with("package-lock.json")
        || path.ends_with("pnpm-lock.yaml")
        || path.ends_with("yarn.lock")
}

fn schema_path(path: &str) -> bool {
    path.contains("migration") || path.contains("schema") || path.ends_with(".sql")
}

fn security_path(path: &str) -> bool {
    ["auth", "credential", "permission", "secret", "token"]
        .iter()
        .any(|needle| path.contains(needle))
}

fn integration_path(path: &str) -> bool {
    ["github", "stripe", "webhook", "integration", "client"]
        .iter()
        .any(|needle| path.contains(needle))
}

fn canonical_change_id(change: &PlannedChange, index: usize) -> String {
    let value = change.change_id.trim();
    if value.is_empty() {
        format!("change-{:03}", index + 1)
    } else {
        value.to_owned()
    }
}

fn canonical_criterion_ids(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn infer_validation_gate_type(id: &str, command: &str) -> GraphValidationGateType {
    let value = format!("{id} {command}").to_ascii_lowercase();
    if value.contains("focused") {
        GraphValidationGateType::FocusedTest
    } else if value.contains("typecheck") || value.contains("type-check") || value.contains("tsc") {
        GraphValidationGateType::Typecheck
    } else if value.contains("lint") || value.contains("clippy") || value.contains("fmt --check") {
        GraphValidationGateType::Lint
    } else if value.contains("build") || value.contains("cargo check") {
        GraphValidationGateType::Build
    } else if value.contains("test") {
        GraphValidationGateType::TestSuite
    } else {
        GraphValidationGateType::Custom
    }
}

fn legacy_target_statuses(
    notebook: &WorkerNotebook,
) -> BTreeMap<(String, String), IntendedChangeStatus> {
    let mut result = BTreeMap::new();
    for change in &notebook.planned_changes {
        for target in &change.targets {
            result.insert(
                (change.change_id.clone(), target.path.clone()),
                target.status,
            );
        }
    }
    for change in &notebook.intended_changes {
        for target in &change.targets {
            result.insert(
                (change.change_id.clone(), target.path.clone()),
                target.status,
            );
        }
        if change.targets.is_empty() && !change.target.is_empty() {
            result.insert(
                (change.change_id.clone(), change.target.clone()),
                change.status,
            );
        }
    }
    result
}

fn mutation_path_counts(graph: &ExecutionGraph) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for path in graph
        .nodes
        .iter()
        .filter(|node| node.kind.is_mutation())
        .filter_map(|node| node.target.as_ref().map(|target| target.path.clone()))
    {
        *counts.entry(path).or_insert(0) += 1;
    }
    counts
}

fn authoritative_mutation_node_ids(
    checkpoint: &HostedOrchestrationCheckpoint,
) -> BTreeSet<ExecutionNodeId> {
    let mut node_ids = checkpoint
        .domain_events
        .iter()
        .filter_map(|event| match event {
            ExecutionDomainEvent::MutationApplied { node_id, .. }
            | ExecutionDomainEvent::MutationSuperseded { node_id, .. } => Some(node_id.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    node_ids.extend(checkpoint.evidence.records.values().filter_map(|evidence| {
        (evidence.kind == EvidenceKind::Mutation)
            .then(|| evidence.node_id.clone())
            .flatten()
    }));
    node_ids
}

const fn graph_status_from_legacy(status: IntendedChangeStatus) -> ExecutionNodeStatus {
    match status {
        IntendedChangeStatus::Planned => ExecutionNodeStatus::Pending,
        // A checkpoint is a model/tool-call boundary, so no legacy in-flight
        // declaration may retain execution ownership. Readiness is re-derived
        // from dependencies immediately after synchronization.
        IntendedChangeStatus::InProgress => ExecutionNodeStatus::Pending,
        IntendedChangeStatus::Applied | IntendedChangeStatus::Verified => {
            ExecutionNodeStatus::Applied
        }
        IntendedChangeStatus::Partial | IntendedChangeStatus::Unresolved => {
            ExecutionNodeStatus::FailedRecoverable
        }
    }
}

const fn legacy_status_from_graph(status: ExecutionNodeStatus) -> IntendedChangeStatus {
    match status {
        ExecutionNodeStatus::Pending | ExecutionNodeStatus::Ready => IntendedChangeStatus::Planned,
        ExecutionNodeStatus::Running => IntendedChangeStatus::InProgress,
        ExecutionNodeStatus::Applied
        | ExecutionNodeStatus::Superseded
        | ExecutionNodeStatus::Skipped => IntendedChangeStatus::Applied,
        ExecutionNodeStatus::Passed | ExecutionNodeStatus::Completed => {
            IntendedChangeStatus::Verified
        }
        ExecutionNodeStatus::FailedRecoverable => IntendedChangeStatus::Partial,
        ExecutionNodeStatus::FailedBlocking => IntendedChangeStatus::Unresolved,
    }
}

fn aggregate_legacy_status(
    statuses: impl Iterator<Item = IntendedChangeStatus>,
) -> IntendedChangeStatus {
    let statuses = statuses.collect::<Vec<_>>();
    if statuses.is_empty() {
        return IntendedChangeStatus::Planned;
    }
    if statuses.iter().all(|status| {
        matches!(
            status,
            IntendedChangeStatus::Applied | IntendedChangeStatus::Verified
        )
    }) {
        return IntendedChangeStatus::Applied;
    }
    if statuses.contains(&IntendedChangeStatus::Unresolved) {
        return IntendedChangeStatus::Unresolved;
    }
    if statuses.iter().any(|status| {
        matches!(
            status,
            IntendedChangeStatus::Applied
                | IntendedChangeStatus::Verified
                | IntendedChangeStatus::Partial
        )
    }) {
        return IntendedChangeStatus::Partial;
    }
    if statuses.contains(&IntendedChangeStatus::InProgress) {
        IntendedChangeStatus::InProgress
    } else {
        IntendedChangeStatus::Planned
    }
}

fn mutation_node_for_failure(
    graph: &ExecutionGraph,
    failure: &ToolFailureRecord,
) -> Option<ExecutionNodeId> {
    let mutation_nodes = graph
        .nodes
        .iter()
        .filter(|node| node.kind.is_mutation())
        .collect::<Vec<_>>();
    if let (Some(change_id), Some(path)) = (failure.change_id.as_deref(), failure.target.as_deref())
    {
        return mutation_nodes
            .iter()
            .find(|node| {
                node.target
                    .as_ref()
                    .is_some_and(|target| target.change_id == change_id && target.path == path)
            })
            .map(|node| node.id.clone());
    }
    if let Some(change_id) = failure.change_id.as_deref() {
        let matches = mutation_nodes
            .iter()
            .filter(|node| {
                node.target
                    .as_ref()
                    .is_some_and(|target| target.change_id == change_id)
            })
            .collect::<Vec<_>>();
        if matches.len() == 1 {
            return Some(matches[0].id.clone());
        }
    }
    if let Some(path) = failure.target.as_deref() {
        let matches = mutation_nodes
            .iter()
            .filter(|node| {
                node.target
                    .as_ref()
                    .is_some_and(|target| target.path == path)
            })
            .collect::<Vec<_>>();
        if matches.len() == 1 {
            return Some(matches[0].id.clone());
        }
    }
    None
}

fn stable_failure_id(failure: &ToolFailureRecord, node_id: &ExecutionNodeId) -> FailureId {
    let material = format!(
        "{}\0{}\0{}\0{}\0{}",
        node_id,
        failure.attempt_index,
        failure.tool,
        failure.target.as_deref().unwrap_or_default(),
        failure.error_code
    );
    let digest = hex::encode(Sha256::digest(material.as_bytes()));
    FailureId::new(format!("legacy-failure-{}", &digest[..20]))
}

const fn graph_validation_status(status: ValidationStatus) -> ValidationEvidenceStatus {
    match status {
        ValidationStatus::Passed => ValidationEvidenceStatus::Passed,
        ValidationStatus::FailedCode => ValidationEvidenceStatus::Failed,
        ValidationStatus::FailedInfrastructure | ValidationStatus::TimedOut => {
            ValidationEvidenceStatus::TimedOut
        }
        ValidationStatus::Cancelled => ValidationEvidenceStatus::Cancelled,
        ValidationStatus::Skipped | ValidationStatus::Superseded => {
            ValidationEvidenceStatus::Superseded
        }
        ValidationStatus::Pending | ValidationStatus::Ready | ValidationStatus::Running => {
            ValidationEvidenceStatus::Running
        }
    }
}

const fn legacy_validation_status(status: ValidationEvidenceStatus) -> ValidationStatus {
    match status {
        ValidationEvidenceStatus::Running => ValidationStatus::Running,
        ValidationEvidenceStatus::Passed => ValidationStatus::Passed,
        ValidationEvidenceStatus::Failed => ValidationStatus::FailedCode,
        ValidationEvidenceStatus::TimedOut => ValidationStatus::TimedOut,
        ValidationEvidenceStatus::Cancelled => ValidationStatus::Cancelled,
        ValidationEvidenceStatus::Superseded => ValidationStatus::Superseded,
    }
}

const fn legacy_validation_status_from_node(status: ExecutionNodeStatus) -> ValidationStatus {
    match status {
        ExecutionNodeStatus::Passed | ExecutionNodeStatus::Completed => ValidationStatus::Passed,
        ExecutionNodeStatus::FailedRecoverable => ValidationStatus::FailedCode,
        ExecutionNodeStatus::FailedBlocking => ValidationStatus::FailedInfrastructure,
        ExecutionNodeStatus::Superseded => ValidationStatus::Superseded,
        ExecutionNodeStatus::Skipped => ValidationStatus::Skipped,
        ExecutionNodeStatus::Pending => ValidationStatus::Pending,
        ExecutionNodeStatus::Ready => ValidationStatus::Ready,
        ExecutionNodeStatus::Running => ValidationStatus::Running,
        ExecutionNodeStatus::Applied => ValidationStatus::Passed,
    }
}

const fn legacy_validation_gate_type(gate_type: GraphValidationGateType) -> ValidationGateType {
    match gate_type {
        GraphValidationGateType::FocusedTest => ValidationGateType::FocusedTest,
        GraphValidationGateType::TestSuite => ValidationGateType::TestSuite,
        GraphValidationGateType::Build => ValidationGateType::Build,
        GraphValidationGateType::Lint => ValidationGateType::Lint,
        GraphValidationGateType::Typecheck => ValidationGateType::Typecheck,
        GraphValidationGateType::Custom => ValidationGateType::Custom,
    }
}

const fn graph_node_status_from_validation(
    status: ValidationEvidenceStatus,
) -> ExecutionNodeStatus {
    match status {
        ValidationEvidenceStatus::Running => ExecutionNodeStatus::Running,
        ValidationEvidenceStatus::Passed => ExecutionNodeStatus::Passed,
        ValidationEvidenceStatus::Failed | ValidationEvidenceStatus::TimedOut => {
            ExecutionNodeStatus::FailedRecoverable
        }
        ValidationEvidenceStatus::Cancelled => ExecutionNodeStatus::FailedBlocking,
        ValidationEvidenceStatus::Superseded => ExecutionNodeStatus::Pending,
    }
}

fn validation_output_summary(evidence: &ValidationEvidence) -> String {
    match (
        evidence.stdout_summary.trim().is_empty(),
        evidence.stderr_summary.trim().is_empty(),
    ) {
        (false, false) => format!(
            "stdout: {}\nstderr: {}",
            evidence.stdout_summary, evidence.stderr_summary
        ),
        (false, true) => evidence.stdout_summary.clone(),
        (true, false) => evidence.stderr_summary.clone(),
        (true, true) => String::new(),
    }
}

#[allow(dead_code)]
const fn legacy_gate_type(gate_type: ValidationGateType) -> GraphValidationGateType {
    match gate_type {
        ValidationGateType::FocusedTest => GraphValidationGateType::FocusedTest,
        ValidationGateType::TestSuite => GraphValidationGateType::TestSuite,
        ValidationGateType::Build => GraphValidationGateType::Build,
        ValidationGateType::Lint => GraphValidationGateType::Lint,
        ValidationGateType::Typecheck => GraphValidationGateType::Typecheck,
        ValidationGateType::Custom => GraphValidationGateType::Custom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_graph::{
        EvidenceRecord, MissionBudget, MissionComplexity, NodeAttempt, ProgressEventKind,
        RepositorySnapshot,
    };
    use crate::hosted_orchestrator::{ExecutionDecision, reconcile_execution};

    fn target(change_id: &str, path: &str, role: &str) -> GraphPlannedTarget {
        GraphPlannedTarget {
            change_id: change_id.to_owned(),
            path: path.to_owned(),
            role: role.to_owned(),
            intent: format!("Implement {change_id}"),
            acceptance_criteria_ids: vec!["ac-1".to_owned()],
            new_file: false,
        }
    }

    fn gate() -> GraphValidationGateSpec {
        GraphValidationGateSpec {
            gate_id: "tests".to_owned(),
            gate_type: GraphValidationGateType::TestSuite,
            command: "cargo test".to_owned(),
            working_directory: ".".to_owned(),
            required: true,
            dependency_lock_hash: "lock-1".to_owned(),
            relevant_environment_fingerprint: "env-1".to_owned(),
        }
    }

    fn graph(targets: &[GraphPlannedTarget]) -> ExecutionGraph {
        ExecutionGraph::from_targets(
            "graph-1",
            MissionComplexity::Small,
            "tree-1",
            targets,
            &[gate()],
            &MissionBudget::for_complexity(MissionComplexity::Small),
        )
    }

    fn notebook(phase: ExecutionPhase) -> WorkerNotebook {
        serde_json::from_value(json!({
            "schema_version": 1,
            "revision": 1,
            "goal": "Exercise graph compatibility",
            "phase": phase,
            "repository_base_sha": "base-1",
            "branch": "rustgrid/test",
            "repository_fingerprint": "tree-1",
            "execution_attempt": 2
        }))
        .expect("minimal worker notebook")
    }

    fn hosted_manifest() -> HostedManifest {
        serde_json::from_value(json!({
            "manifest_version": 4,
            "budget_source": "user_selected",
            "execution": {
                "execution_id": "00000000-0000-4000-8000-000000000001",
                "status": "running",
                "attempt_number": 1
            },
            "run": {
                "id": "00000000-0000-4000-8000-000000000002",
                "ticket_id": "00000000-0000-4000-8000-000000000003",
                "input_prompt": "Implement the accepted plan.",
                "attempt": 1
            },
            "project_id": "00000000-0000-4000-8000-000000000004",
            "project_key": "RG",
            "project_name": "RustGrid",
            "ticket_id": "00000000-0000-4000-8000-000000000003",
            "ticket_key": "RG-1",
            "ticket_title": "Preserve graph budget",
            "github": {
                "repository_id": 1,
                "repository": "RustGrid/example",
                "clone_url": "https://github.com/RustGrid/example.git",
                "web_base_url": "https://github.com",
                "installation_id": 1,
                "base_ref": "main",
                "base_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "branch": "rustgrid/rg-1",
                "github_token_url": "https://app.rustgrid.test/github-token"
            },
            "ai_gateway": {
                "responses_url": "https://app.rustgrid.test/ai/responses",
                "model": "gpt-5.6-sol",
                "maximum_input_tokens": 100000,
                "maximum_output_tokens": 8000,
                "maximum_model_calls": 14,
                "maximum_cost_usd": "2.00"
            },
            "execution_policy": {
                "policy_version": 1,
                "codex": {
                    "command": ["codex", "exec", "--json"],
                    "environment_allowlist": ["PATH"]
                },
                "quality_gates": [{
                    "id": "test",
                    "command": "cargo test",
                    "timeout_seconds": 60,
                    "required": true
                }],
                "timeout_seconds": 480,
                "sandbox": {
                    "mode": "workspace_write",
                    "network_access": false,
                    "writable_roots": ["."],
                    "approval_policy": "never"
                }
            },
            "execution_policy_sha256": "policy-hash",
            "heartbeat_url": "https://app.rustgrid.test/heartbeat",
            "token_refresh_url": "https://app.rustgrid.test/token/refresh",
            "events_url": "https://app.rustgrid.test/events",
            "telemetry_url": "https://app.rustgrid.test/telemetry",
            "state_url": "https://app.rustgrid.test/state",
            "complete_url": "https://app.rustgrid.test/complete"
        }))
        .expect("hosted manifest")
    }

    #[test]
    fn manifest_validation_gates_are_canonicalized_before_graph_construction() {
        let mut manifest = hosted_manifest();
        let prototype = manifest.execution_policy.quality_gates[0].clone();
        let gate = |id: &str, command: &str| {
            let mut gate = prototype.clone();
            gate.id = id.to_owned();
            gate.command = command.to_owned();
            gate
        };
        manifest.execution_policy.quality_gates = vec![
            gate("lint-z", "cargo clippy"),
            gate("build", "cargo build"),
            gate("suite", "cargo test"),
            gate("focused", "cargo test focused_theme"),
            gate("custom", "npm run docs-check"),
            gate("typecheck-a", "npm run typecheck"),
        ];

        let canonical = canonical_validation_gates(&manifest);
        assert_eq!(
            canonical
                .iter()
                .map(|gate| gate.gate_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "focused",
                "lint-z",
                "typecheck-a",
                "suite",
                "build",
                "custom",
            ]
        );
        assert_eq!(
            canonical
                .iter()
                .map(|gate| gate.gate_type)
                .collect::<Vec<_>>(),
            vec![
                GraphValidationGateType::FocusedTest,
                GraphValidationGateType::Lint,
                GraphValidationGateType::Typecheck,
                GraphValidationGateType::TestSuite,
                GraphValidationGateType::Build,
                GraphValidationGateType::Custom,
            ]
        );
    }

    #[test]
    fn existing_vitest_target_inserts_focused_gate_before_broad_validation() {
        let mut manifest = hosted_manifest();
        manifest.execution_policy.quality_gates[0].id = "test".into();
        manifest.execution_policy.quality_gates[0].command = "npm test".into();
        let targets = vec![GraphPlannedTarget {
            change_id: "theme-test".into(),
            path: "tests/theme-provider.test.tsx".into(),
            role: "test".into(),
            intent: "cover theme selection".into(),
            acceptance_criteria_ids: vec!["ac-1".into()],
            new_file: false,
        }];
        let gates = canonical_validation_gates_for_targets(&manifest, &targets, true);
        assert_eq!(gates.len(), 2);
        assert_eq!(gates[0].gate_type, GraphValidationGateType::FocusedTest);
        assert_eq!(
            gates[0].command,
            "npx vitest run tests/theme-provider.test.tsx"
        );
        assert_eq!(gates[1].gate_type, GraphValidationGateType::TestSuite);
    }

    #[test]
    fn startup_bootstrap_suppresses_redundant_install_unless_dependencies_change() {
        let mut manifest = hosted_manifest();
        let mut install = manifest.execution_policy.quality_gates[0].clone();
        install.id = "install".into();
        install.command = "npm ci --maxsockets=1".into();
        manifest.execution_policy.quality_gates.push(install);
        let source = GraphPlannedTarget {
            change_id: "theme".into(),
            path: "src/theme.tsx".into(),
            role: "source".into(),
            intent: "update theme".into(),
            acceptance_criteria_ids: vec![],
            new_file: false,
        };
        let gates =
            canonical_validation_gates_for_targets(&manifest, std::slice::from_ref(&source), true);
        assert!(!gates.iter().any(|gate| gate.gate_id == "install"));

        let gates =
            canonical_validation_gates_for_targets(&manifest, std::slice::from_ref(&source), false);
        assert!(gates.iter().any(|gate| gate.gate_id == "install"));

        let mut dependency = source;
        dependency.path = "package-lock.json".into();
        let gates = canonical_validation_gates_for_targets(&manifest, &[dependency], true);
        assert_eq!(gates[0].gate_id, "install");
    }

    #[test]
    fn unstarted_required_gates_materialize_as_ready_or_pending_never_running() {
        let target = target("theme", "src/theme.tsx", "production");
        let mut suite = gate();
        suite.gate_id = "suite".into();
        let mut build = gate();
        build.gate_id = "build".into();
        build.gate_type = GraphValidationGateType::Build;
        build.command = "cargo build".into();
        let mut graph = ExecutionGraph::from_targets(
            "gate-statuses",
            MissionComplexity::Small,
            "tree-1",
            std::slice::from_ref(&target),
            &[suite, build],
            &MissionBudget::for_complexity(MissionComplexity::Small),
        );
        let mutation_id = graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .unwrap()
            .id
            .clone();
        graph
            .set_node_status(&mutation_id, ExecutionNodeStatus::Applied)
            .unwrap();
        graph.refresh_readiness();
        let statuses = materialize_required_gates(&graph, &EvidenceStore::default())
            .into_iter()
            .map(|gate| gate.status)
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            [ValidationStatus::Ready, ValidationStatus::Pending]
        );
    }

    fn accepted_plan() -> ImplementationPlan {
        ImplementationPlan {
            implementation_status: "ready".to_owned(),
            planned_changes: vec![PlannedChange {
                change_id: "change-source".to_owned(),
                parent_change_id: None,
                path: String::new(),
                targets: vec![PlannedTarget {
                    path: "src/lib.rs".to_owned(),
                    role: "production".to_owned(),
                    new_file: false,
                    status: IntendedChangeStatus::Planned,
                }],
                change: "Implement the source change".to_owned(),
                reason: "Satisfy ac-1".to_owned(),
                status: IntendedChangeStatus::Planned,
                acceptance_criteria: vec!["ac-1".to_owned()],
                test_coverage: Vec::new(),
            }],
            planned_new_files: Vec::new(),
            planned_test_changes: Vec::new(),
            remaining_unknowns: Vec::new(),
            blocking_unknowns: Vec::new(),
        }
    }

    fn complex_accepted_plan() -> ImplementationPlan {
        let mut plan = accepted_plan();
        plan.planned_changes = [
            ("dependency", "Cargo.toml", "production", false),
            ("schema", "migrations/0099_scope.sql", "production", true),
            (
                "integration",
                "src/github/auth_client.rs",
                "production",
                true,
            ),
            ("tests", "tests/integration_test.rs", "tests", true),
        ]
        .into_iter()
        .map(|(change_id, path, role, new_file)| PlannedChange {
            change_id: change_id.to_owned(),
            parent_change_id: None,
            path: String::new(),
            targets: vec![PlannedTarget {
                path: path.to_owned(),
                role: role.to_owned(),
                new_file,
                status: IntendedChangeStatus::Planned,
            }],
            change: format!("Implement {change_id}"),
            reason: "Exercise authoritative complexity inputs".to_owned(),
            status: IntendedChangeStatus::Planned,
            acceptance_criteria: vec!["ac-1".to_owned()],
            test_coverage: Vec::new(),
        })
        .collect();
        plan
    }

    #[test]
    fn complexity_is_provisional_until_an_accepted_plan_reclassifies_it() {
        let manifest = hosted_manifest();
        let mut checkpoint = HostedOrchestrationCheckpoint::bootstrap(&manifest, "tree-clean");
        let provisional = checkpoint
            .complexity
            .as_ref()
            .expect("bootstrap assessment");
        assert_eq!(
            provisional.stage,
            ComplexityClassificationStage::Provisional
        );
        assert_eq!(provisional.class, MissionComplexity::Tiny);
        assert_eq!(
            checkpoint
                .graph
                .as_ref()
                .expect("bootstrap graph")
                .complexity_classification_stage,
            ComplexityClassificationStage::Provisional
        );
        assert!(
            checkpoint
                .graph
                .as_ref()
                .expect("bootstrap graph")
                .nodes
                .iter()
                .all(|node| matches!(
                    node.kind,
                    ExecutionNodeKind::Discovery | ExecutionNodeKind::Planning
                ))
        );

        let graph = checkpoint.graph.as_ref().expect("bootstrap graph");
        let discovery = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Discovery)
            .expect("discovery node");
        let planning = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Planning)
            .expect("planning node");
        assert_eq!(discovery.budget.max_model_calls, 3);
        assert_eq!(planning.budget.max_model_calls, 2);

        for _ in 0..3 {
            let reservation = checkpoint
                .budget
                .reserve_model_call(&discovery.id, &discovery.budget, 1, Duration::ZERO)
                .expect("discovery bootstrap allowance");
            checkpoint
                .budget
                .consume_model_call_reservation(&reservation, 1, Duration::ZERO);
        }
        assert!(
            checkpoint
                .budget
                .reserve_model_call(&discovery.id, &discovery.budget, 1, Duration::ZERO,)
                .is_err(),
            "discovery must stop after its three-call bootstrap allowance"
        );
        for _ in 0..2 {
            let reservation = checkpoint
                .budget
                .reserve_model_call(&planning.id, &planning.budget, 1, Duration::ZERO)
                .expect("planning bootstrap allowance");
            checkpoint
                .budget
                .consume_model_call_reservation(&reservation, 1, Duration::ZERO);
        }

        let authoritative = checkpoint
            .rebuild_from_plan(&manifest, &complex_accepted_plan(), "tree-clean")
            .clone();
        assert_eq!(
            authoritative.stage,
            ComplexityClassificationStage::Authoritative
        );
        assert_ne!(authoritative.class, MissionComplexity::Tiny);
        for factor in [
            crate::execution_graph::ComplexityFactorKind::PlannedTargetCount,
            crate::execution_graph::ComplexityFactorKind::NewFileCount,
            crate::execution_graph::ComplexityFactorKind::DependencyChanges,
            crate::execution_graph::ComplexityFactorKind::DatabaseSchemaChanges,
            crate::execution_graph::ComplexityFactorKind::ExternalIntegrations,
            crate::execution_graph::ComplexityFactorKind::TestSurface,
            crate::execution_graph::ComplexityFactorKind::ExpectedValidationDuration,
            crate::execution_graph::ComplexityFactorKind::CrossModuleImpact,
        ] {
            assert!(
                authoritative
                    .factors
                    .iter()
                    .any(|entry| entry.kind == factor && entry.value > 0),
                "authoritative assessment omitted {factor:?}"
            );
        }
        assert_eq!(
            checkpoint
                .graph
                .as_ref()
                .expect("accepted-plan graph")
                .complexity_classification_stage,
            ComplexityClassificationStage::Authoritative
        );
    }

    #[test]
    fn legacy_bootstrap_checkpoint_without_stage_is_normalized_to_provisional() {
        let manifest = hosted_manifest();
        let checkpoint = HostedOrchestrationCheckpoint::bootstrap(&manifest, "tree-clean");
        let mut serialized = serde_json::to_value(checkpoint).expect("serialize checkpoint");
        serialized["graph"]
            .as_object_mut()
            .expect("serialized graph")
            .remove("complexity_classification_stage");
        serialized["complexity"]
            .as_object_mut()
            .expect("serialized assessment")
            .remove("stage");

        let mut restored: HostedOrchestrationCheckpoint =
            serde_json::from_value(serialized).expect("restore legacy checkpoint");
        assert_eq!(
            restored
                .graph
                .as_ref()
                .expect("legacy graph")
                .complexity_classification_stage,
            ComplexityClassificationStage::Authoritative,
            "the serde default alone cannot identify a pre-plan checkpoint"
        );

        restored.normalize_pre_plan_classification(&manifest);
        assert_eq!(
            restored
                .graph
                .as_ref()
                .expect("normalized graph")
                .complexity_classification_stage,
            ComplexityClassificationStage::Provisional
        );
        assert_eq!(
            restored.complexity.as_ref().map(|value| value.stage),
            Some(ComplexityClassificationStage::Provisional)
        );
    }

    #[test]
    fn parses_signed_decimal_cost_without_floating_point_drift() {
        assert_eq!(parse_usd_micros("5"), Some(5_000_000));
        assert_eq!(parse_usd_micros("$10.250001"), Some(10_250_001));
        assert_eq!(parse_usd_micros("0.0000099"), Some(9));
        assert_eq!(parse_usd_micros("-1"), None);
        assert_eq!(parse_usd_micros("unbounded"), None);
    }

    #[test]
    fn criterion_references_are_canonical_and_deduplicated() {
        assert_eq!(
            canonical_criterion_ids(&[
                " AC-2 ".to_owned(),
                "ac-1".to_owned(),
                "ac-2".to_owned(),
                String::new(),
            ]),
            vec!["ac-1", "ac-2"]
        );
    }

    #[test]
    fn checkpoint_boundary_does_not_restore_stale_in_flight_ownership() {
        assert_eq!(
            graph_status_from_legacy(IntendedChangeStatus::InProgress),
            ExecutionNodeStatus::Pending
        );
    }

    #[test]
    fn accepted_plan_rebuild_preserves_pre_plan_usage_attempts_and_evidence() {
        let manifest = hosted_manifest();
        let mut checkpoint =
            HostedOrchestrationCheckpoint::bootstrap(&manifest, "tree-before-plan");
        let discovery_id = ExecutionNodeId::new("discovery");
        let planning_id = ExecutionNodeId::new("planning");
        let discovery_attempt = NodeAttempt {
            attempt: 1,
            repository_fingerprint_before: "tree-before-plan".to_owned(),
            repository_fingerprint_after: Some("tree-before-plan".to_owned()),
            model_calls: 1,
            cost_micros: 100,
            duration: Duration::from_secs(2),
            outcome: Some(ExecutionNodeStatus::Completed),
            ..NodeAttempt::default()
        };
        let planning_attempt = NodeAttempt {
            attempt: 1,
            repository_fingerprint_before: "tree-before-plan".to_owned(),
            repository_fingerprint_after: Some("tree-before-plan".to_owned()),
            model_calls: 1,
            cost_micros: 200,
            duration: Duration::from_secs(3),
            outcome: Some(ExecutionNodeStatus::Completed),
            ..NodeAttempt::default()
        };
        let graph = checkpoint.graph.as_mut().expect("bootstrap graph");
        let discovery = graph.node_mut(&discovery_id).expect("discovery node");
        discovery.status = ExecutionNodeStatus::Completed;
        discovery.attempts.push(discovery_attempt.clone());
        discovery.evidence_ids.push("evidence-discovery".to_owned());
        let planning = graph.node_mut(&planning_id).expect("planning node");
        planning.status = ExecutionNodeStatus::Completed;
        planning.attempts.push(planning_attempt.clone());
        planning.evidence_ids.push("evidence-planning".to_owned());
        graph.refresh_readiness();

        checkpoint.evidence.record(EvidenceRecord {
            evidence_id: "evidence-discovery".to_owned(),
            kind: EvidenceKind::RepositoryObservation,
            node_id: Some(discovery_id.clone()),
            repository_fingerprint: "tree-before-plan".to_owned(),
            summary: "repository topology".to_owned(),
        });
        checkpoint.evidence.record(EvidenceRecord {
            evidence_id: "evidence-planning".to_owned(),
            kind: EvidenceKind::AcceptanceCriterion,
            node_id: Some(planning_id.clone()),
            repository_fingerprint: "tree-before-plan".to_owned(),
            summary: "accepted criterion mapping".to_owned(),
        });
        checkpoint
            .budget
            .record_model_call(discovery_id.clone(), 100, Duration::from_secs(2));
        checkpoint.budget.record_progress_kind(
            1,
            ProgressEventKind::NewRelevantEvidenceRecorded,
            Some(discovery_id.clone()),
        );
        checkpoint
            .budget
            .record_model_call(planning_id.clone(), 200, Duration::from_secs(3));
        checkpoint.budget.record_progress_kind(
            2,
            ProgressEventKind::NodeMadeReady,
            Some(planning_id.clone()),
        );
        let previous_budget = checkpoint.budget.clone();
        let provisional_discovery_budget = checkpoint
            .graph
            .as_ref()
            .and_then(|graph| graph.node(&discovery_id))
            .map(|node| node.budget.clone())
            .expect("provisional discovery budget");
        assert_eq!(
            checkpoint.complexity.as_ref().map(|value| value.stage),
            Some(ComplexityClassificationStage::Provisional)
        );

        let assessment = checkpoint
            .rebuild_from_plan(&manifest, &accepted_plan(), "tree-before-plan")
            .clone();

        let graph = checkpoint.graph.as_ref().expect("accepted-plan graph");
        let discovery = graph.node(&discovery_id).expect("preserved discovery");
        let planning = graph.node(&planning_id).expect("preserved planning");
        assert_eq!(discovery.status, ExecutionNodeStatus::Completed);
        assert_eq!(planning.status, ExecutionNodeStatus::Completed);
        assert_eq!(discovery.attempts, vec![discovery_attempt]);
        assert_eq!(planning.attempts, vec![planning_attempt]);
        assert_eq!(discovery.evidence_ids, vec!["evidence-discovery"]);
        assert_eq!(planning.evidence_ids, vec!["evidence-planning"]);
        assert!(
            checkpoint
                .evidence
                .records
                .contains_key("evidence-discovery")
        );
        assert!(
            checkpoint
                .evidence
                .records
                .contains_key("evidence-planning")
        );

        assert_eq!(checkpoint.budget.mission, assessment.budget);
        assert_eq!(
            assessment.stage,
            ComplexityClassificationStage::Authoritative
        );
        assert_eq!(checkpoint.budget.total_model_calls, 2);
        assert_eq!(checkpoint.budget.total_cost_micros, 300);
        assert_eq!(checkpoint.budget.elapsed, Duration::from_secs(5));
        assert_eq!(
            checkpoint.budget.progress_events,
            previous_budget.progress_events
        );
        assert_eq!(
            checkpoint.budget.progress_score,
            previous_budget.progress_score
        );
        assert_eq!(
            checkpoint
                .budget
                .usage_for(&discovery_id)
                .model_calls_consumed,
            1
        );
        assert_eq!(
            checkpoint
                .budget
                .usage_for(&planning_id)
                .model_calls_consumed,
            1
        );
        assert_ne!(
            discovery.budget, provisional_discovery_budget,
            "accepted-plan classification must replace provisional node budgets"
        );
    }

    #[test]
    fn notebook_is_materialized_from_graph_failures_validation_and_events() {
        let source = target("change-source", "src/lib.rs", "production");
        let test = target("change-test", "tests/lib.rs", "tests");
        let mut graph = graph(&[source.clone(), test.clone()]);
        let source_id = graph
            .nodes
            .iter()
            .find(|node| {
                node.target
                    .as_ref()
                    .is_some_and(|target| target.path == source.path)
            })
            .expect("source node")
            .id
            .clone();
        let test_id = graph
            .nodes
            .iter()
            .find(|node| {
                node.target
                    .as_ref()
                    .is_some_and(|target| target.path == test.path)
            })
            .expect("test node")
            .id
            .clone();
        let validation_id = graph
            .nodes
            .iter()
            .find(|node| node.kind.is_validation())
            .expect("validation node")
            .id
            .clone();
        graph
            .set_node_status(&source_id, ExecutionNodeStatus::Applied)
            .expect("apply source");
        graph
            .set_node_status(&test_id, ExecutionNodeStatus::FailedRecoverable)
            .expect("fail test target");
        graph
            .set_node_status(&validation_id, ExecutionNodeStatus::FailedRecoverable)
            .expect("fail validation");
        graph
            .node_mut(&validation_id)
            .expect("validation node")
            .evidence_ids
            .push("validation-tests".to_owned());

        let mut failures = FailureStore::default();
        let mut failure = FailureRecord::new(
            "failure-test",
            test_id,
            FailureCategory::MutationConflict,
            1,
            "tree-1",
            "replacement did not match",
        );
        failure.target_path = Some(test.path.clone());
        failures.record(failure);
        let mut evidence = EvidenceStore::default();
        evidence.record_validation(ValidationEvidenceRecord {
            evidence_id: "validation-tests".to_owned(),
            node_id: validation_id.clone(),
            gate_id: "tests".to_owned(),
            fingerprint: "validation-fingerprint".to_owned(),
            repository_fingerprint: "tree-1".to_owned(),
            command: "cargo test".to_owned(),
            working_directory: ".".to_owned(),
            status: ValidationEvidenceStatus::Failed,
            exit_code: Some(1),
            output_summary: "one test failed".to_owned(),
            duration: Duration::from_millis(42),
        });
        let checkpoint = HostedOrchestrationCheckpoint {
            graph_revision: graph.revision,
            graph: Some(graph),
            domain_events: vec![ExecutionDomainEvent::MutationApplied {
                sequence: 1,
                node_id: source_id.clone(),
                target_path: source.path.clone(),
                repository_fingerprint: "tree-1".to_owned(),
                evidence_id: "mutation-source".to_owned(),
            }],
            failures,
            evidence,
            ..HostedOrchestrationCheckpoint::default()
        };
        let mut notebook = notebook(ExecutionPhase::Implementation);
        checkpoint.materialize_legacy_notebook(&mut notebook);

        assert_eq!(notebook.completed_changes, vec!["change-source"]);
        assert!(
            notebook
                .remaining_work_v2
                .iter()
                .any(|item| { item.change_id == "change-test" && item.path == "tests/lib.rs" })
        );
        assert_eq!(
            notebook
                .remaining_work_v2
                .iter()
                .map(|item| item.change_id.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "change-test",
                validation_id.as_str(),
                "diff-review",
                "completion-evaluation",
                "publication",
            ])
        );
        assert_eq!(notebook.failed_changes.len(), 1);
        let materialized_failure = &notebook.failed_changes[0];
        assert_eq!(materialized_failure.attempt_index, 1);
        assert_eq!(
            materialized_failure.change_id.as_deref(),
            Some("change-test")
        );
        assert!(materialized_failure.tool.is_empty());
        assert_eq!(materialized_failure.target.as_deref(), Some("tests/lib.rs"));
        assert_eq!(materialized_failure.error_code, "mutation_conflict");
        assert_eq!(materialized_failure.match_count, None);
        assert_eq!(materialized_failure.error, "replacement did not match");
        assert!(!materialized_failure.recovered);
        assert_eq!(
            materialized_failure.reconciliation,
            FailureReconciliation::StillUnresolved
        );
        assert!(materialized_failure.recovery.is_none());
        assert!(materialized_failure.intended_change_sha256.is_none());
        assert_eq!(
            notebook.validation_evidence,
            vec![ValidationEvidence {
                evidence_id: "validation-tests".to_owned(),
                gate_id: "tests".to_owned(),
                gate_type: ValidationGateType::TestSuite,
                command: "cargo test".to_owned(),
                normalized_command: "cargo test".to_owned(),
                command_fingerprint: "validation-fingerprint".to_owned(),
                source_tree_hash: "tree-1".to_owned(),
                dependency_lock_hash: "lock-1".to_owned(),
                started_at: String::new(),
                completed_at: None,
                duration_ms: 42,
                exit_code: Some(1),
                status: ValidationStatus::FailedCode,
                stdout_summary: "one test failed".to_owned(),
                stderr_summary: String::new(),
                source: ValidationSource::ResumeReused,
            }]
        );
        assert_eq!(
            notebook.required_gates,
            vec![RequiredGate {
                gate_id: "tests".to_owned(),
                gate_type: ValidationGateType::TestSuite,
                required: true,
                command: "cargo test".to_owned(),
                status: ValidationStatus::FailedCode,
                evidence_id: Some("validation-tests".to_owned()),
            }]
        );
        assert_eq!(
            notebook.last_successful_action,
            json!({
                "event_type": "mutation_applied",
                "sequence": 1,
                "node_id": source_id.as_str(),
            })
        );
    }

    #[test]
    fn replayed_validation_projection_ignores_blank_or_poisoned_legacy_arrays() {
        let mut validation_graph = graph(&[]);
        for node in &mut validation_graph.nodes {
            if matches!(
                node.kind,
                ExecutionNodeKind::Discovery | ExecutionNodeKind::Planning
            ) {
                node.status = ExecutionNodeStatus::Completed;
            }
        }
        validation_graph.refresh_readiness();
        let validation_node = validation_graph
            .nodes
            .iter()
            .find(|node| node.kind.is_validation())
            .expect("validation node")
            .clone();
        let gate = validation_node
            .validation
            .as_ref()
            .expect("validation gate")
            .clone();
        let validation_fingerprint = gate.fingerprint("tree-1");
        let evidence = ValidationEvidenceRecord {
            evidence_id: "canonical-validation-failure".to_owned(),
            node_id: validation_node.id.clone(),
            gate_id: gate.gate_id,
            fingerprint: validation_fingerprint.clone(),
            repository_fingerprint: "tree-1".to_owned(),
            command: gate.command,
            working_directory: gate.working_directory,
            status: ValidationEvidenceStatus::Failed,
            exit_code: Some(1),
            output_summary: "canonical failure output".to_owned(),
            duration: Duration::from_millis(17),
        };
        let failure_id = FailureId::new("canonical-validation-failure");
        let events = vec![
            ExecutionDomainEvent::ValidationStarted {
                sequence: 1,
                node_id: validation_node.id.clone(),
                fingerprint: validation_fingerprint.clone(),
            },
            ExecutionDomainEvent::ValidationEvidenceRecorded {
                sequence: 2,
                node_id: validation_node.id.clone(),
                evidence,
            },
            ExecutionDomainEvent::FailureRecorded {
                sequence: 3,
                failure: FailureRecord::new(
                    failure_id.clone(),
                    validation_node.id.clone(),
                    FailureCategory::ValidationFailure,
                    1,
                    "tree-1",
                    "canonical failure output",
                ),
            },
            ExecutionDomainEvent::ValidationFailed {
                sequence: 4,
                node_id: validation_node.id.clone(),
                failure_id: failure_id.clone(),
                fingerprint: validation_fingerprint,
            },
        ];
        let encoded = serde_json::to_string(&events).expect("serialize validation event stream");
        let replay_events: Vec<ExecutionDomainEvent> =
            serde_json::from_str(&encoded).expect("deserialize validation event stream");
        let mut replayed = ExecutionSnapshot {
            run_id: "validation-projection-replay".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                source_tree_hash: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: validation_graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        for event in replay_events {
            replayed
                .append_event(event)
                .expect("replay canonical validation event");
        }
        let mut checkpoint = HostedOrchestrationCheckpoint::default();
        checkpoint.replace_from_snapshot(&replayed);

        let mut blank = notebook(ExecutionPhase::Validation);
        blank.validation_evidence.clear();
        blank.required_gates.clear();
        blank.validation_failures.clear();
        checkpoint.materialize_legacy_notebook(&mut blank);
        let canonical_evidence = blank.validation_evidence.clone();
        let canonical_gates = blank.required_gates.clone();
        let canonical_failures = blank.validation_failures.clone();
        assert_eq!(
            canonical_failures,
            ["canonical-validation-failure: canonical failure output"]
        );
        assert_eq!(canonical_evidence.len(), 1);
        assert_eq!(canonical_evidence[0].status, ValidationStatus::FailedCode);
        assert_eq!(
            canonical_evidence[0].stdout_summary,
            "canonical failure output"
        );
        assert!(canonical_evidence[0].started_at.is_empty());
        assert_eq!(canonical_evidence[0].completed_at, None);
        assert_eq!(canonical_evidence[0].source, ValidationSource::ResumeReused);

        let mut poisoned = notebook(ExecutionPhase::Validation);
        let mut poisoned_evidence = canonical_evidence[0].clone();
        poisoned_evidence.evidence_id = "poisoned-evidence".to_owned();
        poisoned_evidence.status = ValidationStatus::Passed;
        poisoned_evidence.stdout_summary = "poisoned output".to_owned();
        poisoned_evidence.started_at = "2099-01-01T00:00:00Z".to_owned();
        poisoned_evidence.source = ValidationSource::ModelRequested;
        let mut poisoned_gate = canonical_gates[0].clone();
        poisoned_gate.status = ValidationStatus::Passed;
        poisoned_gate.evidence_id = Some("poisoned-evidence".to_owned());
        poisoned.validation_evidence = vec![poisoned_evidence];
        poisoned.required_gates = vec![poisoned_gate];
        poisoned.validation_failures = vec!["poisoned legacy failure".to_owned()];

        checkpoint.materialize_legacy_notebook(&mut poisoned);
        assert_eq!(poisoned.validation_evidence, canonical_evidence);
        assert_eq!(poisoned.required_gates, canonical_gates);
        assert_eq!(poisoned.validation_failures, canonical_failures);

        replayed
            .append_event(ExecutionDomainEvent::FailureRecovered {
                sequence: 5,
                node_id: validation_node.id,
                failure_id,
                repository_fingerprint: "tree-1".to_owned(),
            })
            .expect("recover canonical validation failure");
        checkpoint.replace_from_snapshot(&replayed);
        poisoned.validation_failures = canonical_failures;
        checkpoint.materialize_legacy_notebook(&mut poisoned);
        assert!(
            poisoned.validation_failures.is_empty(),
            "recovered graph failure must clear the legacy projection"
        );
    }

    #[test]
    fn empty_notebook_change_arrays_are_rebuilt_from_graph_targets() {
        let mut source = target("change-palette", "src/palette.rs", "production source");
        source.intent = "Implement the palette".to_owned();
        source.acceptance_criteria_ids = vec![" AC-2 ".to_owned(), "ac-1".to_owned()];
        source.new_file = true;
        let mut other_tests = target("change-other", "tests/other.rs", "test coverage");
        other_tests.intent = "Cover the other behavior".to_owned();
        other_tests.acceptance_criteria_ids = vec!["AC-9".to_owned()];
        let mut palette_tests = target(
            "change-palette",
            "tests/palette.rs",
            "palette test coverage",
        );
        palette_tests.intent = "Implement the palette".to_owned();
        palette_tests.acceptance_criteria_ids = vec!["ac-3".to_owned(), "AC-2".to_owned()];
        let mut graph = graph(&[source.clone(), other_tests, palette_tests]);
        let source_id = graph
            .nodes
            .iter()
            .find(|node| node.target.as_ref() == Some(&source))
            .expect("palette source node")
            .id
            .clone();
        graph
            .set_node_status(&source_id, ExecutionNodeStatus::Applied)
            .expect("apply palette source");
        let checkpoint = HostedOrchestrationCheckpoint {
            graph_revision: graph.revision,
            graph: Some(graph),
            ..HostedOrchestrationCheckpoint::default()
        };
        let mut notebook = notebook(ExecutionPhase::Implementation);
        assert!(notebook.planned_changes.is_empty());
        assert!(notebook.intended_changes.is_empty());

        checkpoint.materialize_legacy_notebook(&mut notebook);

        assert_eq!(notebook.planned_changes.len(), 2);
        let palette = &notebook.planned_changes[0];
        assert_eq!(palette.change_id, "change-palette");
        assert_eq!(palette.parent_change_id, None);
        assert!(palette.path.is_empty());
        assert_eq!(palette.change, "Implement the palette");
        assert_eq!(palette.reason, "Implement the palette");
        assert_eq!(palette.status, IntendedChangeStatus::Partial);
        assert_eq!(palette.acceptance_criteria, ["ac-1", "ac-2", "ac-3"]);
        assert!(palette.test_coverage.is_empty());
        assert_eq!(palette.targets.len(), 2);
        assert_eq!(palette.targets[0].path, "src/palette.rs");
        assert_eq!(palette.targets[0].role, "production source");
        assert!(palette.targets[0].new_file);
        assert_eq!(palette.targets[0].status, IntendedChangeStatus::Applied);
        assert_eq!(palette.targets[1].path, "tests/palette.rs");
        assert_eq!(palette.targets[1].role, "palette test coverage");
        assert!(!palette.targets[1].new_file);
        assert_eq!(palette.targets[1].status, IntendedChangeStatus::Planned);

        let other = &notebook.planned_changes[1];
        assert_eq!(other.change_id, "change-other");
        assert_eq!(other.acceptance_criteria, ["ac-9"]);
        assert_eq!(other.targets.len(), 1);
        assert_eq!(other.targets[0].path, "tests/other.rs");

        assert_eq!(notebook.intended_changes.len(), 2);
        for (planned, intended) in notebook
            .planned_changes
            .iter()
            .zip(&notebook.intended_changes)
        {
            assert_eq!(intended.change_id, planned.change_id);
            assert_eq!(intended.intent, planned.change);
            assert_eq!(intended.status, planned.status);
            assert!(intended.target.is_empty());
            assert_eq!(
                intended
                    .targets
                    .iter()
                    .map(|target| (
                        target.path.as_str(),
                        target.role.as_str(),
                        target.new_file,
                        target.status,
                    ))
                    .collect::<Vec<_>>(),
                planned
                    .targets
                    .iter()
                    .map(|target| (
                        target.path.as_str(),
                        target.role.as_str(),
                        target.new_file,
                        target.status,
                    ))
                    .collect::<Vec<_>>()
            );
            assert!(intended.attempts.is_empty());
            assert!(intended.recovery.is_none());
        }
    }

    #[test]
    fn old_checkpoint_imports_legacy_state_once_and_ignores_stale_mutation_state() {
        let planned_target = target("change-one", "src/one.rs", "production");
        let graph = graph(std::slice::from_ref(&planned_target));
        let checkpoint = HostedOrchestrationCheckpoint {
            graph_revision: graph.revision,
            graph: Some(graph),
            ..HostedOrchestrationCheckpoint::default()
        };
        let mut old_payload = serde_json::to_value(checkpoint).expect("serialize checkpoint");
        old_payload
            .as_object_mut()
            .expect("checkpoint object")
            .remove("legacy_import_completed");
        let mut restored: HostedOrchestrationCheckpoint =
            serde_json::from_value(old_payload).expect("deserialize old checkpoint");
        assert!(restored.legacy_import_pending());

        let mut notebook = notebook(ExecutionPhase::Implementation);
        let (planned_changes, intended_changes) =
            materialize_legacy_changes(restored.graph.as_ref().expect("graph"));
        notebook.planned_changes = planned_changes;
        notebook.intended_changes = intended_changes;
        notebook.planned_changes[0].status = IntendedChangeStatus::Applied;
        notebook.planned_changes[0].targets[0].status = IntendedChangeStatus::Applied;
        notebook.intended_changes[0].status = IntendedChangeStatus::Applied;
        notebook.intended_changes[0].targets[0].status = IntendedChangeStatus::Applied;
        assert!(restored.import_legacy_state_once(
            &notebook,
            std::slice::from_ref(&planned_target.path),
            &HostedReconciliationFacts::default(),
        ));
        assert!(restored.legacy_import_completed);
        let node_id = restored
            .graph
            .as_ref()
            .expect("graph")
            .nodes
            .iter()
            .find(|node| node.target.as_ref() == Some(&planned_target))
            .expect("planned target node")
            .id
            .clone();
        let authoritative_status = restored
            .graph
            .as_ref()
            .expect("graph")
            .node(&node_id)
            .expect("planned target node")
            .status;
        assert_eq!(authoritative_status, ExecutionNodeStatus::Applied);

        let migrated_payload =
            serde_json::to_value(restored).expect("serialize migrated checkpoint");
        assert_eq!(migrated_payload["legacy_import_completed"], true);
        let mut resumed: HostedOrchestrationCheckpoint =
            serde_json::from_value(migrated_payload).expect("resume migrated checkpoint");
        notebook.planned_changes[0].status = IntendedChangeStatus::Planned;
        notebook.planned_changes[0].targets[0].status = IntendedChangeStatus::Planned;
        notebook.intended_changes[0].status = IntendedChangeStatus::Planned;
        notebook.intended_changes[0].targets[0].status = IntendedChangeStatus::Planned;
        assert!(!resumed.import_legacy_state_once(
            &notebook,
            &[],
            &HostedReconciliationFacts::default(),
        ));
        assert_eq!(
            resumed
                .graph
                .as_ref()
                .expect("graph")
                .node(&node_id)
                .expect("planned target node")
                .status,
            authoritative_status
        );
    }

    #[test]
    fn duplicate_path_is_not_applied_without_node_specific_evidence() {
        let first = target("change-one", "src/shared.rs", "first responsibility");
        let second = target("change-two", "src/shared.rs", "second responsibility");
        let graph = graph(&[first, second]);
        let first_id = graph
            .nodes
            .iter()
            .find(|node| {
                node.target
                    .as_ref()
                    .is_some_and(|target| target.change_id == "change-one")
            })
            .expect("first node")
            .id
            .clone();
        let mut checkpoint = HostedOrchestrationCheckpoint {
            graph_revision: graph.revision,
            graph: Some(graph.clone()),
            ..HostedOrchestrationCheckpoint::default()
        };
        let notebook = notebook(ExecutionPhase::Implementation);
        assert!(checkpoint.import_legacy_state_once(
            &notebook,
            &["src/shared.rs".to_owned()],
            &HostedReconciliationFacts::default(),
        ));
        let observed_graph = checkpoint.graph.as_ref().expect("graph");
        assert_eq!(
            observed_graph
                .nodes
                .iter()
                .filter(|node| node.kind.is_mutation() && node.status.is_success())
                .count(),
            0
        );

        let mut checkpoint = HostedOrchestrationCheckpoint {
            graph_revision: graph.revision,
            graph: Some(graph),
            domain_events: vec![ExecutionDomainEvent::MutationApplied {
                sequence: 1,
                node_id: first_id.clone(),
                target_path: "src/shared.rs".to_owned(),
                repository_fingerprint: "tree-1".to_owned(),
                evidence_id: "mutation-one".to_owned(),
            }],
            ..HostedOrchestrationCheckpoint::default()
        };
        assert!(checkpoint.import_legacy_state_once(
            &notebook,
            &["src/shared.rs".to_owned()],
            &HostedReconciliationFacts::default(),
        ));
        let graph = checkpoint.graph.as_ref().expect("graph");
        assert!(
            graph
                .node(&first_id)
                .is_some_and(|node| node.status.is_success())
        );
        assert_eq!(
            graph
                .nodes
                .iter()
                .filter(|node| node.kind.is_mutation() && node.status.is_success())
                .count(),
            1
        );
    }

    #[test]
    fn repaired_topology_preserves_unchanged_node_progress_and_budget() {
        let first = target("change-one", "src/one.rs", "production");
        let second = target("change-two", "src/two.rs", "production");
        let inserted = target("change-new", "src/new.rs", "production");
        let mut previous = graph(&[first.clone(), second.clone()]);
        let first_id = previous
            .nodes
            .iter()
            .find(|node| node.target.as_ref() == Some(&first))
            .expect("first node")
            .id
            .clone();
        let second_id = previous
            .nodes
            .iter()
            .find(|node| node.target.as_ref() == Some(&second))
            .expect("second node")
            .id
            .clone();
        let validation_id = previous
            .nodes
            .iter()
            .find(|node| node.kind.is_validation())
            .expect("validation node")
            .id
            .clone();
        let diff_review_id = ExecutionNodeId::new("diff-review");
        let completion_id = ExecutionNodeId::new("completion-evaluation");
        let publication_id = ExecutionNodeId::new("publication");
        previous
            .set_node_status(&first_id, ExecutionNodeStatus::Applied)
            .expect("apply first target");
        previous
            .set_node_status(&second_id, ExecutionNodeStatus::Applied)
            .expect("apply second target");
        previous
            .set_node_status(&validation_id, ExecutionNodeStatus::Passed)
            .expect("pass validation");
        previous
            .set_node_status(&diff_review_id, ExecutionNodeStatus::Completed)
            .expect("complete diff review");
        previous
            .set_node_status(&completion_id, ExecutionNodeStatus::Completed)
            .expect("complete completion evaluation");
        previous
            .set_node_status(&publication_id, ExecutionNodeStatus::Completed)
            .expect("complete publication");
        previous
            .node_mut(&first_id)
            .expect("first target")
            .attempts
            .push(NodeAttempt {
                attempt: 1,
                repository_fingerprint_before: "tree-0".to_owned(),
                repository_fingerprint_after: Some("tree-1".to_owned()),
                outcome: Some(ExecutionNodeStatus::Applied),
                ..NodeAttempt::default()
            });
        let mut replacement = graph(&[first, inserted, second]);
        let preserved = preserve_unchanged_graph_progress(&previous, &mut replacement);

        assert!(preserved.contains(&first_id));
        assert!(preserved.contains(&second_id));
        for invalidated in [
            &validation_id,
            &diff_review_id,
            &completion_id,
            &publication_id,
        ] {
            assert!(
                !preserved.contains(invalidated),
                "downstream node {invalidated} must be invalidated"
            );
            assert!(
                replacement
                    .node(invalidated)
                    .is_some_and(|node| !node.status.is_success()),
                "downstream node {invalidated} retained stale success"
            );
        }
        assert_eq!(
            replacement
                .node(&first_id)
                .expect("stable first node")
                .attempts
                .len(),
            1
        );
        assert!(
            replacement
                .node(&first_id)
                .is_some_and(|node| node.status == ExecutionNodeStatus::Applied)
        );

        let mut checkpoint = HostedOrchestrationCheckpoint {
            graph: Some(previous),
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..HostedOrchestrationCheckpoint::default()
        };
        checkpoint
            .budget
            .record_model_call(first_id.clone(), 125, Duration::from_millis(10));
        retain_checkpoint_progress_for_nodes(&mut checkpoint, &preserved, &replacement);
        assert_eq!(
            checkpoint.budget.usage_for(&first_id).model_calls_consumed,
            1
        );
    }

    #[test]
    fn materialization_clears_legacy_last_action_without_a_canonical_event() {
        let graph = graph(&[target("change-one", "src/one.rs", "production")]);
        let checkpoint = HostedOrchestrationCheckpoint {
            graph_revision: graph.revision,
            graph: Some(graph),
            ..HostedOrchestrationCheckpoint::default()
        };
        let mut notebook = notebook(ExecutionPhase::Implementation);
        notebook.last_successful_action = json!({"tool": "stale_legacy_action"});

        checkpoint.materialize_legacy_notebook(&mut notebook);

        assert_eq!(notebook.last_successful_action, json!({}));
        assert!(
            checkpoint
                .graph
                .as_ref()
                .expect("graph")
                .derived_collections()
                .applied_mutation_targets
                .is_empty(),
            "a legacy last_successful_action is not repository mutation evidence"
        );
    }

    #[test]
    fn serialized_checkpoint_resume_selects_the_next_ready_node() {
        let first = target("change-one", "src/one.rs", "production");
        let second = target("change-two", "src/two.rs", "production");
        let mut graph = graph(&[first.clone(), second.clone()]);
        let first_id = graph
            .nodes
            .iter()
            .find(|node| node.target.as_ref() == Some(&first))
            .expect("first node")
            .id
            .clone();
        graph
            .set_node_status(&first_id, ExecutionNodeStatus::Applied)
            .expect("apply first target");
        let checkpoint = HostedOrchestrationCheckpoint {
            graph_revision: graph.revision,
            graph: Some(graph),
            ..HostedOrchestrationCheckpoint::default()
        };
        let encoded = serde_json::to_vec(&checkpoint).expect("serialize checkpoint");
        let restored: HostedOrchestrationCheckpoint =
            serde_json::from_slice(&encoded).expect("deserialize checkpoint");
        let snapshot = restored.snapshot(
            "run-resumed",
            RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                changed_paths: BTreeSet::from([first.path]),
                ..RepositorySnapshot::default()
            },
        );
        assert!(matches!(
            reconcile_execution(&snapshot).expect("resume decision"),
            ExecutionDecision::ExecuteTarget { target, .. }
                if target.target.path == second.path
        ));
    }

    #[test]
    fn newer_attempt_clears_cancellation_through_the_event_reducer() {
        let target = target("change-one", "src/one.rs", "production");
        let graph = graph(std::slice::from_ref(&target));
        let mut checkpoint = HostedOrchestrationCheckpoint {
            graph_revision: graph.revision,
            graph: Some(graph),
            legacy_import_completed: true,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            cancellation: Some(CancellationState {
                requested_at: "attempt-1".to_owned(),
                reason: "user requested cancellation".to_owned(),
                checkpointed: true,
                ..CancellationState::default()
            }),
            ..HostedOrchestrationCheckpoint::default()
        };
        let repository = RepositorySnapshot {
            fingerprint: "tree-1".to_owned(),
            ..RepositorySnapshot::default()
        };

        assert!(
            checkpoint
                .resume_for_new_attempt("run-1", repository.clone(), 1, 1)
                .expect("same attempt remains cancelled")
                .is_none()
        );
        assert!(checkpoint.cancellation.is_some());
        assert_eq!(
            checkpoint
                .resume_for_new_attempt("run-1", repository.clone(), 1, 2)
                .expect("newer attempt resumes"),
            Some(HostedResumeReason::Cancellation)
        );
        assert!(checkpoint.cancellation.is_none());
        assert!(matches!(
            checkpoint.domain_events.last(),
            Some(ExecutionDomainEvent::ExecutionResumed {
                execution_attempt: 2,
                previous_outcome: None,
                ..
            })
        ));
        assert!(matches!(
            reconcile_execution(&checkpoint.snapshot("run-1", repository))
                .expect("resumed graph decision"),
            ExecutionDecision::ExecuteTarget { target: context, .. }
                if context.target == target
        ));
    }

    #[test]
    fn newer_attempt_reopens_remaining_work_after_a_published_partial_result() {
        let first = target("change-one", "src/one.rs", "production");
        let second = target("change-two", "src/two.rs", "production");
        let mut graph = graph(&[first.clone(), second.clone()]);
        let first_id = graph
            .nodes
            .iter()
            .find(|node| node.target.as_ref() == Some(&first))
            .expect("first target")
            .id
            .clone();
        let second_id = graph
            .nodes
            .iter()
            .find(|node| node.target.as_ref() == Some(&second))
            .expect("second target")
            .id
            .clone();
        graph
            .set_node_status(&first_id, ExecutionNodeStatus::Applied)
            .expect("apply first target");
        graph
            .dependency_satisfaction_overrides
            .insert(second_id.clone());
        graph.refresh_readiness();
        let validation_ids = graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_validation())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        for node_id in validation_ids {
            graph
                .set_node_status(&node_id, ExecutionNodeStatus::Passed)
                .expect("pass partial validation");
        }
        let diff_id = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::DiffReview)
            .expect("diff node")
            .id
            .clone();
        graph
            .set_node_status(&diff_id, ExecutionNodeStatus::Completed)
            .expect("complete partial diff review");
        let completion_id = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .expect("completion node")
            .id
            .clone();
        graph
            .set_node_status(&completion_id, ExecutionNodeStatus::Completed)
            .expect("complete partial evaluation");
        let publication_id = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Publication)
            .expect("publication node")
            .id
            .clone();
        graph
            .set_node_status(&publication_id, ExecutionNodeStatus::Completed)
            .expect("publish partial result");
        let events = vec![
            ExecutionDomainEvent::GuardrailTriggered {
                sequence: 1,
                reason: crate::execution_graph::GuardrailReason::NodeBudgetExhausted,
                outcome: MissionOutcome::PartialReviewable,
                detail: "useful partial work".to_owned(),
            },
            ExecutionDomainEvent::CompletionEvaluated {
                sequence: 2,
                node_id: completion_id.clone(),
                outcome: MissionOutcome::PartialReviewable,
            },
            ExecutionDomainEvent::RunFinished {
                sequence: 3,
                outcome: MissionOutcome::PartialReviewable,
            },
        ];
        let mut checkpoint = HostedOrchestrationCheckpoint {
            graph_revision: graph.revision,
            graph: Some(graph),
            legacy_import_completed: true,
            domain_events: events,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            publication: PublicationState {
                status: PublicationStatus::PullRequestCreated,
                mode: Some(crate::execution_graph::PublicationMode::Draft),
                commit_sha: Some("partial-commit".to_owned()),
                branch: Some("rustgrid/partial".to_owned()),
                pull_request_url: Some("https://example.test/pull/7".to_owned()),
                pull_request_number: Some(7),
                draft: true,
                recovery_requested: false,
            },
            ..HostedOrchestrationCheckpoint::default()
        };
        let repository = RepositorySnapshot {
            fingerprint: "tree-1".to_owned(),
            changed_paths: BTreeSet::from([first.path.clone()]),
            ..RepositorySnapshot::default()
        };

        assert_eq!(
            checkpoint
                .resume_for_new_attempt("run-partial", repository.clone(), 1, 2)
                .expect("resume published partial"),
            Some(HostedResumeReason::PartialReviewable)
        );
        let mut resumed = checkpoint.snapshot("run-partial", repository);
        assert_eq!(resumed.terminal_outcome(), None);
        assert!(resumed.graph.dependency_satisfaction_overrides.is_empty());
        assert_eq!(
            resumed.graph.node(&first_id).map(|node| node.status),
            Some(ExecutionNodeStatus::Applied)
        );
        assert_eq!(
            resumed.graph.node(&second_id).map(|node| node.status),
            Some(ExecutionNodeStatus::Ready)
        );
        assert_eq!(resumed.publication.status, PublicationStatus::NotStarted);
        assert_eq!(
            resumed.publication.pull_request_number,
            Some(7),
            "existing draft PR identity remains available for update"
        );
        assert!(matches!(
            reconcile_execution(&resumed).expect("continue remaining partial work"),
            ExecutionDecision::ExecuteTarget { target, .. }
                if target.target == second
        ));

        assert!(crate::execution_graph::current_execution_epoch(&resumed.events).is_empty());
        resumed
            .append_event(ExecutionDomainEvent::GuardrailTriggered {
                sequence: 5,
                reason: crate::execution_graph::GuardrailReason::NodeBudgetExhausted,
                outcome: MissionOutcome::PartialReviewable,
                detail: "second useful partial epoch".to_owned(),
            })
            .expect("the resumed epoch records its own partial handoff");
        assert_eq!(
            crate::execution_graph::current_execution_epoch(&resumed.events)
                .iter()
                .filter(|event| matches!(event, ExecutionDomainEvent::GuardrailTriggered { .. }))
                .count(),
            1
        );
        let validation_id = resumed
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_validation())
            .expect("validation node")
            .id
            .clone();
        let validation_gate = resumed
            .graph
            .node(&validation_id)
            .and_then(|node| node.validation.clone())
            .expect("validation gate");
        let validation_fingerprint =
            validation_gate.fingerprint(&resumed.current_repository.fingerprint);
        resumed
            .append_event(ExecutionDomainEvent::ValidationStarted {
                sequence: 6,
                node_id: validation_id.clone(),
                fingerprint: validation_fingerprint.clone(),
            })
            .expect("restart validation in resumed epoch");
        resumed
            .append_event(ExecutionDomainEvent::ValidationEvidenceRecorded {
                sequence: 7,
                node_id: validation_id.clone(),
                evidence: ValidationEvidenceRecord {
                    evidence_id: "second-validation".to_owned(),
                    node_id: validation_id.clone(),
                    gate_id: validation_gate.gate_id,
                    fingerprint: validation_fingerprint.clone(),
                    repository_fingerprint: resumed.current_repository.fingerprint.clone(),
                    command: validation_gate.command,
                    working_directory: validation_gate.working_directory,
                    status: ValidationEvidenceStatus::Passed,
                    exit_code: Some(0),
                    output_summary: "resumed validation passed".to_owned(),
                    duration: Duration::from_millis(1),
                },
            })
            .expect("record resumed validation evidence");
        resumed
            .append_event(ExecutionDomainEvent::ValidationPassed {
                sequence: 8,
                node_id: validation_id,
                evidence_id: "second-validation".to_owned(),
                fingerprint: validation_fingerprint,
            })
            .expect("pass validation in resumed epoch");
        resumed
            .append_event(ExecutionDomainEvent::DiffReviewed {
                sequence: 9,
                node_id: diff_id,
                evidence_ids: vec!["second-validation".to_owned()],
            })
            .expect("review resumed partial diff");
        resumed
            .append_event(ExecutionDomainEvent::CompletionEvaluated {
                sequence: 10,
                node_id: completion_id,
                outcome: MissionOutcome::PartialReviewable,
            })
            .expect("evaluate resumed partial completion");
        resumed
            .append_event(ExecutionDomainEvent::PublicationStarted {
                sequence: 11,
                node_id: publication_id.clone(),
                mode: crate::execution_graph::PublicationMode::Draft,
            })
            .expect("restart draft publication");
        resumed
            .append_event(ExecutionDomainEvent::CommitCreated {
                sequence: 12,
                node_id: publication_id.clone(),
                commit_sha: "second-partial-commit".to_owned(),
            })
            .expect("record resumed commit");
        resumed
            .append_event(ExecutionDomainEvent::BranchPushed {
                sequence: 13,
                node_id: publication_id.clone(),
                branch: "rustgrid/partial".to_owned(),
            })
            .expect("push resumed partial");
        resumed
            .append_event(ExecutionDomainEvent::PullRequestCreated {
                sequence: 14,
                node_id: publication_id,
                url: "https://example.test/pull/7".to_owned(),
                number: Some(7),
                draft: true,
            })
            .expect("update resumed draft pull request");
        resumed
            .append_event(ExecutionDomainEvent::RunFinished {
                sequence: 15,
                outcome: MissionOutcome::PartialReviewable,
            })
            .expect("the resumed epoch can finish independently");
        assert_eq!(
            resumed.terminal_outcome(),
            Some(MissionOutcome::PartialReviewable)
        );
        assert_eq!(
            resumed
                .events
                .iter()
                .filter(|event| matches!(event, ExecutionDomainEvent::RunFinished { .. }))
                .count(),
            2
        );
    }
}
