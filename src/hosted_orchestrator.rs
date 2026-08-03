//! Pure, deterministic lifecycle authority for hosted executions.
//!
//! This module deliberately performs no I/O.  Callers checkpoint facts into an
//! [`ExecutionSnapshot`], ask [`reconcile_execution`] for exactly one decision,
//! and execute that decision through the hosted adapter.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

pub use crate::execution_graph::*;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DiscoveryAction {
    InspectRepository {
        inspection_scope: InspectionScope,
    },
    FinalizeImpactMap {
        evidence_ids: Vec<EvidenceId>,
    },
    RepairImpactMap {
        validation_errors: Vec<ArtifactValidationError>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct InspectionScope {
    #[serde(default)]
    pub missing_evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reopened_for: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ArtifactValidationError {
    pub path: String,
    pub keyword: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PlanningAction {
    BuildPlan {
        impact_map_id: ArtifactId,
        evidence_ids: Vec<EvidenceId>,
    },
    RepairPlan {
        validation_errors: Vec<PlanValidationError>,
        previous_plan: PlanArtifact,
    },
    ResolveEvidenceGap {
        missing_evidence: Vec<MissingEvidenceRequirement>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct PlanValidationError {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct PlanArtifact {
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct MissingEvidenceRequirement {
    pub path: Option<String>,
    pub symbol: Option<String>,
    pub test_relationship: Option<String>,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetRepairContext {
    pub failure: FailureRecord,
    pub target: TargetExecutionContext,
    pub next_repair_attempt: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MutationAction {
    PrepareTargetContext {
        node_id: ExecutionNodeId,
        target: MutationTarget,
    },
    MutateTarget {
        node_id: ExecutionNodeId,
        target: MutationTarget,
        expected_repository_fingerprint: RepositoryFingerprint,
    },
    VerifyTargetState {
        node_id: ExecutionNodeId,
        target: MutationTarget,
    },
    RepairTarget {
        node_id: ExecutionNodeId,
        target: MutationTarget,
        failure: FailureRecord,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ExecutionDecision {
    ContinueDiscovery {
        action: DiscoveryAction,
    },
    ContinuePlanning {
        action: PlanningAction,
    },
    ExecuteTarget {
        node_id: ExecutionNodeId,
        action: MutationAction,
        target: TargetExecutionContext,
    },
    /// Legacy checkpoint shape. New reconciliation emits
    /// `ExecuteTarget { action: MutationAction::RepairTarget, .. }`.
    RepairTarget {
        node_id: ExecutionNodeId,
        failure_id: FailureId,
        context: TargetRepairContext,
    },
    RunValidation {
        node_id: ExecutionNodeId,
        gate: ValidationGateSpec,
    },
    ReviewDiff {
        node_id: ExecutionNodeId,
    },
    ReviewIncompleteDiff {
        node_id: ExecutionNodeId,
        reason: IncompleteReason,
    },
    EvaluateCompletion {
        node_id: ExecutionNodeId,
    },
    Publish {
        mode: PublicationMode,
    },
    Finish {
        outcome: MissionOutcome,
    },
    StopForGuardrail {
        outcome: MissionOutcome,
        reason: GuardrailReason,
    },
}

impl ExecutionDecision {
    pub const fn stage(&self) -> HostedExecutionStage {
        match self {
            Self::ContinueDiscovery { .. } => HostedExecutionStage::Discovery,
            Self::ContinuePlanning { .. } => HostedExecutionStage::Planning,
            Self::ExecuteTarget { .. } | Self::RepairTarget { .. } => {
                HostedExecutionStage::Implementation
            }
            Self::RunValidation { .. } => HostedExecutionStage::Validation,
            Self::ReviewDiff { .. }
            | Self::ReviewIncompleteDiff { .. }
            | Self::EvaluateCompletion { .. } => HostedExecutionStage::Review,
            Self::Publish { .. } => HostedExecutionStage::Publication,
            Self::Finish { .. } | Self::StopForGuardrail { .. } => HostedExecutionStage::Terminal,
        }
    }

    pub fn node_id(&self) -> Option<&ExecutionNodeId> {
        match self {
            Self::ExecuteTarget { node_id, .. }
            | Self::RepairTarget { node_id, .. }
            | Self::RunValidation { node_id, .. }
            | Self::ReviewDiff { node_id }
            | Self::ReviewIncompleteDiff { node_id, .. }
            | Self::EvaluateCompletion { node_id } => Some(node_id),
            _ => None,
        }
    }

    pub fn budget_node_id(&self) -> Option<&ExecutionNodeId> {
        match self {
            Self::ExecuteTarget {
                action: MutationAction::RepairTarget { failure, .. },
                ..
            }
            | Self::RepairTarget {
                context: TargetRepairContext { failure, .. },
                ..
            } if failure.category == FailureCategory::ValidationFailure => Some(&failure.node_id),
            _ => self.node_id(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct OrchestrationInvariantError {
    pub code: String,
    pub message: String,
    pub node_id: Option<ExecutionNodeId>,
}

impl OrchestrationInvariantError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            node_id: None,
        }
    }

    fn for_node(
        code: impl Into<String>,
        node_id: ExecutionNodeId,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            node_id: Some(node_id),
        }
    }
}

impl fmt::Display for OrchestrationInvariantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(node_id) = &self.node_id {
            write!(
                formatter,
                "{}: {} (node {node_id})",
                self.code, self.message
            )
        } else {
            write!(formatter, "{}: {}", self.code, self.message)
        }
    }
}

impl std::error::Error for OrchestrationInvariantError {}

impl From<GraphInvariantError> for OrchestrationInvariantError {
    fn from(error: GraphInvariantError) -> Self {
        Self::new("orchestration_invariant_violation", error.to_string())
    }
}

/// Returns the sole authoritative next action for a hosted execution.
///
/// The function is referentially transparent: equal snapshots always produce
/// equal decisions and it never mutates the supplied graph or performs I/O.
pub fn reconcile_execution(
    snapshot: &ExecutionSnapshot,
) -> Result<ExecutionDecision, OrchestrationInvariantError> {
    snapshot.validate_invariants()?;

    if let Some(outcome) = snapshot.terminal_outcome() {
        return Ok(ExecutionDecision::Finish { outcome });
    }

    if snapshot.cancellation.is_some() {
        return Ok(ExecutionDecision::StopForGuardrail {
            outcome: MissionOutcome::Cancelled,
            reason: GuardrailReason::Cancellation,
        });
    }

    if let Some(outcome) = current_execution_epoch(&snapshot.events)
        .iter()
        .rev()
        .find_map(|event| match event {
            ExecutionDomainEvent::GuardrailTriggered { outcome, .. }
                if *outcome != MissionOutcome::PartialReviewable =>
            {
                Some(*outcome)
            }
            _ => None,
        })
    {
        return Ok(ExecutionDecision::Finish { outcome });
    }

    if snapshot.publication.recovery_requested && snapshot.publication.is_published() {
        if snapshot.has_partial_reviewable_guardrail() {
            return Ok(ExecutionDecision::Finish {
                outcome: MissionOutcome::PartialReviewable,
            });
        }
        return Ok(ExecutionDecision::StopForGuardrail {
            outcome: MissionOutcome::PartialReviewable,
            reason: GuardrailReason::OrchestrationInvariantViolation,
        });
    }

    let running = snapshot
        .graph
        .nodes
        .iter()
        .filter(|node| node.status == ExecutionNodeStatus::Running)
        .collect::<Vec<_>>();
    if running.len() > 1 {
        return Err(OrchestrationInvariantError::new(
            "multiple_running_nodes",
            "an execution graph may have only one running node",
        ));
    }

    if let Some(failure) = snapshot
        .failures
        .unresolved()
        .find(|failure| failure.category.is_infrastructure())
    {
        if snapshot.current_repository.has_changes() && all_required_mutations_applied(snapshot) {
            return incomplete_diff_decision(
                snapshot,
                IncompleteReason::ValidationInfrastructureFailure,
            );
        }
        return Ok(ExecutionDecision::StopForGuardrail {
            outcome: MissionOutcome::FailedInfrastructure,
            reason: if failure.category == FailureCategory::InfrastructureFailure {
                GuardrailReason::InfrastructureFailure
            } else {
                GuardrailReason::BlockingFailure
            },
        });
    }

    if snapshot.failures.unresolved().any(|failure| {
        failure.category == FailureCategory::PlanRepositoryConflict
            || failure.category == FailureCategory::MutationConflict
                && matches!(
                    failure.code.as_deref(),
                    Some("create_target_already_exists" | "destination_already_exists")
                )
    }) {
        if snapshot.current_repository.has_changes() {
            return incomplete_diff_decision(snapshot, IncompleteReason::TargetOperationConflict);
        }
        return Ok(ExecutionDecision::StopForGuardrail {
            outcome: MissionOutcome::BlockedNoDiff,
            reason: GuardrailReason::BlockingFailure,
        });
    }

    if let Some(failure) = snapshot
        .failures
        .unresolved()
        .find(|failure| failure.category == FailureCategory::ValidationFailure)
    {
        if matches!(
            latest_validation_repair_result(snapshot, &failure.id),
            Some(RepairResult::NoMutation { .. })
        ) {
            if !snapshot.current_repository.has_changes() {
                return Ok(ExecutionDecision::StopForGuardrail {
                    outcome: MissionOutcome::BlockedNoDiff,
                    reason: GuardrailReason::BlockingFailure,
                });
            }
            return incomplete_diff_decision(
                snapshot,
                IncompleteReason::ValidationRepairProducedNoMutation,
            );
        }
        if let Some(path) = failure.target_path.as_deref() {
            let matching_nodes = snapshot
                .graph
                .nodes
                .iter()
                .filter(|node| {
                    node.kind.is_mutation()
                        && node
                            .target
                            .as_ref()
                            .is_some_and(|target| target.path == path)
                })
                .count();
            if matching_nodes > 1 {
                return Err(OrchestrationInvariantError::new(
                    "ambiguous_validation_failure_target",
                    format!(
                        "validation failure `{}` matches {matching_nodes} mutation nodes for `{path}`; canonical node identity is required",
                        failure.id
                    ),
                ));
            }
        }
        let Some(target_node) = affected_mutation_node(snapshot, failure) else {
            return Err(OrchestrationInvariantError::new(
                "validation_failure_target_missing",
                format!(
                    "validation failure `{}` cannot be assigned to a canonical mutation node",
                    failure.id
                ),
            ));
        };
        if repair_budget_exhausted(snapshot, target_node) {
            return Ok(ExecutionDecision::StopForGuardrail {
                outcome: guardrail_outcome(snapshot),
                reason: GuardrailReason::NodeBudgetExhausted,
            });
        }
        return repair_target_decision(snapshot, target_node, failure);
    }

    let effective_success = effective_success_ids(snapshot);
    let next = select_next_node(snapshot, &effective_success);

    if let Some(node) = next {
        if (node.status == ExecutionNodeStatus::FailedRecoverable
            && repair_budget_exhausted(snapshot, node))
            || snapshot.budget.should_stop_node(&node.id, &node.budget)
        {
            return Ok(ExecutionDecision::StopForGuardrail {
                outcome: guardrail_outcome(snapshot),
                reason: if hard_budget_exhausted(snapshot, node)
                    || repair_budget_exhausted(snapshot, node)
                {
                    GuardrailReason::NodeBudgetExhausted
                } else {
                    GuardrailReason::NoProgress
                },
            });
        }

        return decision_for_node(snapshot, node);
    }

    if let Some(node) = snapshot.graph.nodes.iter().find(|node| {
        node.required
            && node.status == ExecutionNodeStatus::FailedBlocking
            && !effective_success.contains(&node.id)
    }) {
        let reason = snapshot
            .failures
            .unresolved_for_node(&node.id)
            .next()
            .map_or(GuardrailReason::BlockingFailure, |failure| {
                if failure.category.is_infrastructure() {
                    GuardrailReason::InfrastructureFailure
                } else {
                    GuardrailReason::BlockingFailure
                }
            });
        let outcome = if reason == GuardrailReason::InfrastructureFailure {
            MissionOutcome::FailedInfrastructure
        } else {
            guardrail_outcome(snapshot)
        };
        return Ok(ExecutionDecision::StopForGuardrail { outcome, reason });
    }

    if all_required_effectively_succeeded(snapshot, &effective_success) {
        let outcome = completion_outcome(snapshot).unwrap_or_else(|| {
            if snapshot.current_repository.has_changes() {
                MissionOutcome::Complete
            } else {
                MissionOutcome::BlockedNoDiff
            }
        });
        if outcome.publication_mode().is_some() && !snapshot.publication.is_published() {
            return Err(OrchestrationInvariantError::new(
                "publication_incomplete",
                "all required graph nodes completed but no pull request was recorded",
            ));
        }
        return Ok(ExecutionDecision::Finish { outcome });
    }

    Err(OrchestrationInvariantError::new(
        "graph_stalled",
        "required graph work remains but no node is runnable",
    ))
}

fn all_required_mutations_applied(snapshot: &ExecutionSnapshot) -> bool {
    snapshot
        .graph
        .nodes
        .iter()
        .filter(|node| node.required && node.kind.is_mutation())
        .all(|node| node.status == ExecutionNodeStatus::Applied)
}

fn latest_validation_repair_result<'a>(
    snapshot: &'a ExecutionSnapshot,
    failure_id: &FailureId,
) -> Option<&'a RepairResult> {
    current_execution_epoch(&snapshot.events)
        .iter()
        .rev()
        .find_map(|event| match event {
            ExecutionDomainEvent::ValidationRepairCompleted {
                failure_id: repaired_failure_id,
                result,
                ..
            } if repaired_failure_id == failure_id => Some(result),
            _ => None,
        })
}

fn incomplete_diff_decision(
    snapshot: &ExecutionSnapshot,
    reason: IncompleteReason,
) -> Result<ExecutionDecision, OrchestrationInvariantError> {
    let diff_review = snapshot
        .graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::DiffReview)
        .ok_or_else(|| {
            OrchestrationInvariantError::new(
                "incomplete_diff_review_node_missing",
                "partial-reviewable execution requires a diff-review node",
            )
        })?;
    if diff_review.status != ExecutionNodeStatus::Completed {
        return Ok(ExecutionDecision::ReviewIncompleteDiff {
            node_id: diff_review.id.clone(),
            reason,
        });
    }

    let completion = snapshot
        .graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
        .ok_or_else(|| {
            OrchestrationInvariantError::new(
                "incomplete_completion_node_missing",
                "partial-reviewable execution requires a completion-evaluation node",
            )
        })?;
    if completion.status != ExecutionNodeStatus::Completed {
        return Ok(ExecutionDecision::EvaluateCompletion {
            node_id: completion.id.clone(),
        });
    }

    if snapshot.publication.is_published() {
        return Ok(ExecutionDecision::Finish {
            outcome: MissionOutcome::PartialReviewable,
        });
    }
    Ok(ExecutionDecision::Publish {
        mode: PublicationMode::Draft,
    })
}

fn decision_for_node(
    snapshot: &ExecutionSnapshot,
    node: &ExecutionNode,
) -> Result<ExecutionDecision, OrchestrationInvariantError> {
    match node.kind {
        ExecutionNodeKind::Discovery => match node.status {
            ExecutionNodeStatus::Applied => Ok(ExecutionDecision::ContinueDiscovery {
                action: DiscoveryAction::FinalizeImpactMap {
                    evidence_ids: current_repository_evidence_ids(snapshot),
                },
            }),
            ExecutionNodeStatus::FailedRecoverable => {
                let failure = unresolved_failure_for_node(snapshot, &node.id)?;
                Ok(ExecutionDecision::ContinueDiscovery {
                    action: DiscoveryAction::RepairImpactMap {
                        validation_errors: artifact_validation_errors(&failure.message),
                    },
                })
            }
            _ => {
                let evidence_ids = current_repository_evidence_ids(snapshot);
                Ok(ExecutionDecision::ContinueDiscovery {
                    action: if discovery_evidence_is_sufficient(snapshot) {
                        DiscoveryAction::FinalizeImpactMap { evidence_ids }
                    } else {
                        DiscoveryAction::InspectRepository {
                            inspection_scope: InspectionScope {
                                missing_evidence: missing_discovery_evidence(snapshot),
                                reopened_for: None,
                            },
                        }
                    },
                })
            }
        },
        ExecutionNodeKind::Planning => {
            if node.status == ExecutionNodeStatus::FailedRecoverable {
                let failure = unresolved_failure_for_node(snapshot, &node.id)?;
                let cached_paths = current_repository_evidence_paths(snapshot);
                let missing_evidence = missing_plan_evidence_requirements(&failure.message)
                    .into_iter()
                    .filter(|requirement| {
                        requirement
                            .path
                            .as_ref()
                            .is_none_or(|path| !cached_paths.contains(&path.to_ascii_lowercase()))
                    })
                    .collect::<Vec<_>>();
                Ok(ExecutionDecision::ContinuePlanning {
                    action: if missing_evidence.is_empty() {
                        PlanningAction::RepairPlan {
                            validation_errors: plan_validation_errors(&failure.message),
                            previous_plan: previous_plan_artifact(&failure.message),
                        }
                    } else {
                        PlanningAction::ResolveEvidenceGap { missing_evidence }
                    },
                })
            } else {
                Ok(ExecutionDecision::ContinuePlanning {
                    action: PlanningAction::BuildPlan {
                        impact_map_id: ArtifactId::new(format!(
                            "impact-map:{}",
                            snapshot.current_repository.fingerprint
                        )),
                        evidence_ids: current_repository_evidence_ids(snapshot),
                    },
                })
            }
        }
        ExecutionNodeKind::SourceMutation | ExecutionNodeKind::TestMutation => {
            if node.status == ExecutionNodeStatus::FailedRecoverable {
                let failure = unresolved_failure_for_node(snapshot, &node.id)?;
                repair_target_decision(snapshot, node, failure)
            } else {
                let mut target = snapshot.target_execution_context(
                    &node.id,
                    tools_for_target_operation(
                        node.target.as_ref().expect("mutation node target"),
                    )?,
                )?;
                let prepared = snapshot.events.iter().rev().any(|event| {
                    matches!(
                        event,
                        ExecutionDomainEvent::TargetContextPrepared {
                            node_id,
                            target_path,
                            operation,
                            source_path,
                            repository_fingerprint,
                            target_content_hash,
                            accepted_intent_hash,
                            ..
                        } if node_id == &node.id
                            && target_path == &target.target.path
                            && operation == &target.target.effective_operation()
                            && source_path.as_deref() == target.target.effective_operation().source_path()
                            && repository_fingerprint.as_str()
                                == snapshot.current_repository.fingerprint
                            && target_content_hash == &target.target_content_hash
                            && accepted_intent_hash == &target.accepted_intent_hash
                    )
                });
                let mutation_produced =
                    snapshot.events.iter().rev().find_map(|event| match event {
                        ExecutionDomainEvent::TargetMutationProduced { node_id, .. }
                            if node_id == &node.id =>
                        {
                            Some(true)
                        }
                        ExecutionDomainEvent::TargetContextPrepared { node_id, .. }
                        | ExecutionDomainEvent::MutationApplied { node_id, .. }
                        | ExecutionDomainEvent::MutationRejected { node_id, .. }
                        | ExecutionDomainEvent::MutationSuperseded { node_id, .. }
                            if node_id == &node.id =>
                        {
                            Some(false)
                        }
                        _ => None,
                    }) == Some(true);
                let planned_target = target.target.clone();
                let action = if mutation_produced {
                    MutationAction::VerifyTargetState {
                        node_id: node.id.clone(),
                        target: planned_target,
                    }
                } else if prepared {
                    MutationAction::MutateTarget {
                        node_id: node.id.clone(),
                        target: planned_target,
                        expected_repository_fingerprint: RepositoryFingerprint::new(
                            snapshot.current_repository.fingerprint.clone(),
                        ),
                    }
                } else {
                    target.allowed_tools.clear();
                    MutationAction::PrepareTargetContext {
                        node_id: node.id.clone(),
                        target: planned_target,
                    }
                };
                Ok(ExecutionDecision::ExecuteTarget {
                    node_id: node.id.clone(),
                    action,
                    target,
                })
            }
        }
        ExecutionNodeKind::ValidationFocused
        | ExecutionNodeKind::ValidationSuite
        | ExecutionNodeKind::ValidationBuild
        | ExecutionNodeKind::ValidationLint => {
            let gate = node.validation.clone().ok_or_else(|| {
                OrchestrationInvariantError::for_node(
                    "validation_gate_missing",
                    node.id.clone(),
                    "a validation node has no gate specification",
                )
            })?;
            let fingerprint = gate.fingerprint(&snapshot.current_repository.fingerprint);
            if snapshot.evidence.has_passed_validation(&fingerprint) {
                return Err(OrchestrationInvariantError::for_node(
                    "stale_validation_node",
                    node.id.clone(),
                    "reusable passed validation evidence was not materialized into graph state",
                ));
            }
            Ok(ExecutionDecision::RunValidation {
                node_id: node.id.clone(),
                gate,
            })
        }
        ExecutionNodeKind::DiffReview => Ok(ExecutionDecision::ReviewDiff {
            node_id: node.id.clone(),
        }),
        ExecutionNodeKind::CompletionEvaluation => Ok(ExecutionDecision::EvaluateCompletion {
            node_id: node.id.clone(),
        }),
        ExecutionNodeKind::Publication => {
            let outcome = completion_outcome(snapshot).ok_or_else(|| {
                OrchestrationInvariantError::for_node(
                    "completion_outcome_missing",
                    node.id.clone(),
                    "publication became runnable before a completion outcome was recorded",
                )
            })?;
            match outcome.publication_mode() {
                Some(mode) => Ok(ExecutionDecision::Publish { mode }),
                None => Ok(ExecutionDecision::Finish { outcome }),
            }
        }
    }
}

fn current_repository_evidence_ids(snapshot: &ExecutionSnapshot) -> Vec<EvidenceId> {
    snapshot
        .evidence
        .files
        .values()
        .filter(|evidence| {
            evidence.repository_fingerprint == snapshot.current_repository.fingerprint
        })
        .map(|evidence| EvidenceId::new(evidence.evidence_id.clone()))
        .collect()
}

fn current_repository_evidence_paths(snapshot: &ExecutionSnapshot) -> BTreeSet<String> {
    snapshot
        .evidence
        .files
        .values()
        .filter(|evidence| {
            evidence.repository_fingerprint == snapshot.current_repository.fingerprint
        })
        .map(|evidence| evidence.path.to_ascii_lowercase())
        .collect()
}

fn discovery_evidence_is_sufficient(snapshot: &ExecutionSnapshot) -> bool {
    let paths = current_repository_evidence_paths(snapshot);
    let has_source = paths.iter().any(|path| {
        path.starts_with("src/") || path.starts_with("app/") || path.starts_with("crates/")
    });
    let has_test = paths
        .iter()
        .any(|path| path.contains("test") || path.contains("spec"));
    let has_validation_contract = paths.iter().any(|path| {
        matches!(
            path.as_str(),
            "package.json" | "cargo.toml" | "pyproject.toml" | "go.mod" | "makefile"
        )
    });
    paths.len() >= 4 && has_source && has_test && has_validation_contract
}

fn missing_discovery_evidence(snapshot: &ExecutionSnapshot) -> Vec<String> {
    let paths = current_repository_evidence_paths(snapshot);
    let mut missing = Vec::new();
    if !paths.iter().any(|path| {
        path.starts_with("src/") || path.starts_with("app/") || path.starts_with("crates/")
    }) {
        missing.push("implementation targets".into());
    }
    if !paths
        .iter()
        .any(|path| path.contains("test") || path.contains("spec"))
    {
        missing.push("related tests".into());
    }
    if !paths.iter().any(|path| {
        matches!(
            path.as_str(),
            "package.json" | "cargo.toml" | "pyproject.toml" | "go.mod" | "makefile"
        )
    }) {
        missing.push("validation commands".into());
    }
    missing
}

fn artifact_validation_errors(message: &str) -> Vec<ArtifactValidationError> {
    serde_json::from_str::<serde_json::Value>(message)
        .ok()
        .and_then(|value| {
            value
                .get("errors")
                .and_then(serde_json::Value::as_array)
                .cloned()
        })
        .into_iter()
        .flatten()
        .filter_map(|error| {
            Some(ArtifactValidationError {
                path: error.get("path")?.as_str()?.to_owned(),
                keyword: error.get("keyword")?.as_str()?.to_owned(),
                message: error.get("message")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

fn plan_validation_errors(message: &str) -> Vec<PlanValidationError> {
    let value = serde_json::from_str::<serde_json::Value>(message).unwrap_or_default();
    value
        .get("validation_errors")
        .or_else(|| value.get("invalid_fields"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|error| {
            if let Some(message) = error.as_str() {
                let path = message.split_once(':').map_or("$", |(path, _)| path);
                return Some(PlanValidationError {
                    path: path.trim().to_owned(),
                    message: message.to_owned(),
                });
            }
            Some(PlanValidationError {
                path: error.get("path")?.as_str()?.to_owned(),
                message: error.get("message")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

fn previous_plan_artifact(message: &str) -> PlanArtifact {
    let value = serde_json::from_str::<serde_json::Value>(message).unwrap_or_default();
    PlanArtifact {
        value: value
            .get("previous_plan")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    }
}

fn missing_plan_evidence_requirements(message: &str) -> Vec<MissingEvidenceRequirement> {
    let value = serde_json::from_str::<serde_json::Value>(message).unwrap_or_default();
    value
        .get("missing_evidence")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|requirement| {
            let reason = requirement.get("reason")?.as_str()?.trim().to_owned();
            let candidate = MissingEvidenceRequirement {
                path: requirement
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                symbol: requirement
                    .get("symbol")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                test_relationship: requirement
                    .get("test_relationship")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                reason,
            };
            (candidate.path.is_some()
                || candidate.symbol.is_some()
                || candidate.test_relationship.is_some())
            .then_some(candidate)
        })
        .collect()
}

fn repair_target_decision(
    snapshot: &ExecutionSnapshot,
    node: &ExecutionNode,
    failure: &FailureRecord,
) -> Result<ExecutionDecision, OrchestrationInvariantError> {
    if repair_budget_exhausted(snapshot, node) {
        return Err(OrchestrationInvariantError::for_node(
            "target_repair_budget_exhausted",
            node.id.clone(),
            "target repair was requested after its node repair budget was exhausted",
        ));
    }
    let mut target = snapshot.target_execution_context(
        &node.id,
        tools_for_target_operation(node.target.as_ref().expect("mutation node target"))?,
    )?;
    if failure.category == FailureCategory::ValidationFailure {
        let implicated_paths = failure
            .assertion_failures
            .iter()
            .flat_map(|assertion| assertion.implicated_paths.iter().cloned())
            .collect::<BTreeSet<_>>();
        let implicated_targets = implicated_paths
            .iter()
            .filter_map(|path| {
                snapshot.evidence.reusable_file(
                    path,
                    &snapshot.current_repository.fingerprint,
                    None,
                )
            })
            .map(FileExcerpt::from)
            .collect();
        target.validation_repair = Some(ValidationRepairContext {
            focused_validation_command: failure.validation_command.clone().unwrap_or_default(),
            assertion_failures: failure.assertion_failures.clone(),
            implicated_targets,
            selected_target: target.target.path.clone(),
            repository_fingerprint: snapshot.current_repository.fingerprint.clone(),
            accepted_implementation_intent: target.intent.clone(),
            existing_diff_paths: snapshot
                .current_repository
                .changed_paths
                .iter()
                .cloned()
                .collect(),
        });
    }
    let context_prepared = snapshot.events.iter().rev().any(|event| {
        matches!(
            event,
            ExecutionDomainEvent::TargetContextPrepared {
                node_id,
                target_path,
                operation,
                source_path,
                repository_fingerprint,
                target_content_hash,
                accepted_intent_hash,
                ..
            } if node_id == &node.id
                && target_path == &target.target.path
                && operation == &target.target.effective_operation()
                && source_path.as_deref() == target.target.effective_operation().source_path()
                && repository_fingerprint.as_str() == snapshot.current_repository.fingerprint
                && target_content_hash == &target.target_content_hash
                && accepted_intent_hash == &target.accepted_intent_hash
        )
    });
    if failure.category == FailureCategory::ValidationFailure && !context_prepared {
        target.allowed_tools.clear();
        return Ok(ExecutionDecision::ExecuteTarget {
            node_id: node.id.clone(),
            action: MutationAction::PrepareTargetContext {
                node_id: node.id.clone(),
                target: target.target.clone(),
            },
            target,
        });
    }
    if failure.category == FailureCategory::ValidationFailure
        && !target.target.new_file
        && (target.current_file_content.is_none() || target.target_content_hash.is_none())
    {
        return Err(OrchestrationInvariantError::for_node(
            "repair_context_incomplete",
            node.id.clone(),
            "validation repair target exists but its current content or content hash is missing",
        ));
    }
    Ok(ExecutionDecision::ExecuteTarget {
        node_id: node.id.clone(),
        action: MutationAction::RepairTarget {
            node_id: node.id.clone(),
            target: target.target.clone(),
            failure: failure.clone(),
        },
        target,
    })
}

fn tools_for_target_operation(
    target: &crate::execution_graph::PlannedTarget,
) -> Result<Vec<ToolKind>, OrchestrationInvariantError> {
    let operation = target.effective_operation();
    if let crate::execution_graph::TargetOperation::Rename {
        source,
        destination,
    }
    | crate::execution_graph::TargetOperation::Move {
        source,
        destination,
    } = &operation
        && (source.trim().is_empty()
            || destination.trim().is_empty()
            || source == destination
            || destination != &target.path)
    {
        return Err(OrchestrationInvariantError::new(
            "unsupported_operation_contract",
            format!(
                "{} requires distinct non-empty source and destination paths with destination equal to the target path",
                operation.as_str()
            ),
        ));
    }
    Ok(match operation {
        crate::execution_graph::TargetOperation::ModifyExisting => {
            vec![ToolKind::ApplyPatch]
        }
        crate::execution_graph::TargetOperation::CreateNew => vec![ToolKind::CreateFile],
        crate::execution_graph::TargetOperation::DeleteExisting => vec![ToolKind::DeleteFile],
        crate::execution_graph::TargetOperation::Rename { .. } => {
            vec![ToolKind::RenameFile, ToolKind::MoveFile]
        }
        crate::execution_graph::TargetOperation::Move { .. } => vec![ToolKind::MoveFile],
    })
}

fn unresolved_failure_for_node<'a>(
    snapshot: &'a ExecutionSnapshot,
    node_id: &ExecutionNodeId,
) -> Result<&'a FailureRecord, OrchestrationInvariantError> {
    snapshot
        .failures
        .unresolved_for_node(node_id)
        .max_by_key(|failure| failure.attempt)
        .ok_or_else(|| {
            OrchestrationInvariantError::for_node(
                "recoverable_node_without_failure",
                node_id.clone(),
                "a recoverable node has no unresolved failure context",
            )
        })
}

fn affected_mutation_node<'a>(
    snapshot: &'a ExecutionSnapshot,
    failure: &FailureRecord,
) -> Option<&'a ExecutionNode> {
    let path = failure.target_path.as_deref()?;
    snapshot.graph.nodes.iter().find(|node| {
        node.kind.is_mutation()
            && node
                .target
                .as_ref()
                .is_some_and(|target| target.path == path)
    })
}

fn effective_success_ids(snapshot: &ExecutionSnapshot) -> BTreeSet<ExecutionNodeId> {
    let mut successful = snapshot.dependency_satisfaction_ids();

    for node in &snapshot.graph.nodes {
        if node.kind.is_validation()
            && node.validation.as_ref().is_some_and(|gate| {
                snapshot.evidence.has_passed_validation(
                    &gate.fingerprint(&snapshot.current_repository.fingerprint),
                )
            })
        {
            successful.insert(node.id.clone());
        }
        if node.kind.is_mutation()
            && node.status == ExecutionNodeStatus::FailedRecoverable
            && !snapshot.failures.has_unresolved_for_node(&node.id)
            && node.target.as_ref().is_some_and(|target| {
                snapshot
                    .current_repository
                    .contains_changed_path(&target.path)
            })
        {
            successful.insert(node.id.clone());
        }
    }
    successful
}

fn select_next_node<'a>(
    snapshot: &'a ExecutionSnapshot,
    successful: &BTreeSet<ExecutionNodeId>,
) -> Option<&'a ExecutionNode> {
    let dependency_ready = |node: &&ExecutionNode| {
        node.dependencies
            .iter()
            .all(|dependency| successful.contains(dependency))
    };

    snapshot
        .graph
        .nodes
        .iter()
        .find(|node| node.status == ExecutionNodeStatus::Running && !successful.contains(&node.id))
        .or_else(|| {
            snapshot.graph.nodes.iter().find(|node| {
                node.status == ExecutionNodeStatus::FailedRecoverable
                    && !successful.contains(&node.id)
                    && snapshot.failures.has_unresolved_for_node(&node.id)
                    && dependency_ready(node)
            })
        })
        .or_else(|| {
            snapshot.graph.nodes.iter().find(|node| {
                matches!(
                    node.status,
                    ExecutionNodeStatus::Ready | ExecutionNodeStatus::Pending
                ) && !successful.contains(&node.id)
                    && dependency_ready(node)
            })
        })
}

fn completion_outcome(snapshot: &ExecutionSnapshot) -> Option<MissionOutcome> {
    snapshot.events.iter().rev().find_map(|event| match event {
        ExecutionDomainEvent::CompletionEvaluated { outcome, .. } => Some(*outcome),
        _ => None,
    })
}

fn all_required_effectively_succeeded(
    snapshot: &ExecutionSnapshot,
    successful: &BTreeSet<ExecutionNodeId>,
) -> bool {
    snapshot
        .graph
        .nodes
        .iter()
        .filter(|node| node.required)
        .all(|node| successful.contains(&node.id))
}

fn guardrail_outcome(snapshot: &ExecutionSnapshot) -> MissionOutcome {
    if snapshot.current_repository.has_changes() {
        MissionOutcome::PartialReviewable
    } else {
        MissionOutcome::BlockedNoDiff
    }
}

fn hard_budget_exhausted(snapshot: &ExecutionSnapshot, node: &ExecutionNode) -> bool {
    let usage = snapshot.budget.usage_for(&node.id);
    (node.budget.max_model_calls > 0
        && usage
            .model_calls_consumed
            .saturating_add(usage.model_calls_reserved)
            >= node.budget.max_model_calls)
        || (node.budget.max_cost_micros > 0 && usage.cost_micros >= node.budget.max_cost_micros)
        || (!node.budget.max_duration.is_zero() && usage.duration >= node.budget.max_duration)
        || usage.repair_attempts > node.budget.max_repair_attempts
        || snapshot.budget.total_model_calls >= snapshot.budget.mission.max_model_calls
        || snapshot.budget.total_cost_micros >= snapshot.budget.mission.max_cost_micros
        || snapshot.budget.elapsed >= snapshot.budget.mission.max_duration
}

fn repair_budget_exhausted(snapshot: &ExecutionSnapshot, node: &ExecutionNode) -> bool {
    if let Some(validation_failure) = snapshot.failures.unresolved().find(|failure| {
        failure.category == FailureCategory::ValidationFailure
            && (failure.target_path.as_deref()
                == node.target.as_ref().map(|target| target.path.as_str())
                || failure.assertion_failures.iter().any(|assertion| {
                    node.target
                        .as_ref()
                        .is_some_and(|target| assertion.implicated_paths.contains(&target.path))
                }))
    }) && let Some(validation_node) = snapshot.graph.node(&validation_failure.node_id)
    {
        return snapshot
            .budget
            .usage_for(&validation_node.id)
            .validation_repair_attempts
            >= validation_node.budget.max_repair_attempts.max(1);
    }
    let repair_pending = node.status == ExecutionNodeStatus::FailedRecoverable
        || snapshot.failures.unresolved().any(|failure| {
            failure.category.creates_repair_work()
                && (failure.node_id == node.id
                    || node.target.as_ref().is_some_and(|target| {
                        failure.target_path.as_deref() == Some(target.path.as_str())
                    }))
        });
    repair_pending
        && snapshot.budget.usage_for(&node.id).repair_attempts >= node.budget.max_repair_attempts
}

/// Classifies a duplicate mutation deterministically before invoking a tool.
/// Already-applied work creates no failure and consumes no repair attempt.
pub fn classify_mutation_request(
    snapshot: &ExecutionSnapshot,
    node_id: &ExecutionNodeId,
) -> Result<Option<MutationResult>, OrchestrationInvariantError> {
    let node = snapshot.graph.node(node_id).ok_or_else(|| {
        OrchestrationInvariantError::for_node(
            "unknown_mutation_node",
            node_id.clone(),
            "mutation request refers to an unknown graph node",
        )
    })?;
    let target = node.target.clone().ok_or_else(|| {
        OrchestrationInvariantError::for_node(
            "mutation_target_missing",
            node_id.clone(),
            "mutation request refers to a non-mutation graph node",
        )
    })?;
    let node_has_mutation_evidence = node.evidence_ids.iter().any(|evidence_id| {
        snapshot
            .evidence
            .records
            .get(evidence_id)
            .is_some_and(|evidence| {
                evidence.kind == crate::execution_graph::EvidenceKind::Mutation
                    && evidence.node_id.as_ref() == Some(&node.id)
            })
    });
    let node_has_applied_event = snapshot.events.iter().any(|event| {
        matches!(
            event,
            crate::execution_graph::ExecutionDomainEvent::MutationApplied {
                node_id: applied_node,
                ..
            } | crate::execution_graph::ExecutionDomainEvent::MutationSuperseded {
                node_id: applied_node,
                ..
            } if applied_node == &node.id
        )
    });
    Ok(
        (node.status.is_success() || node_has_mutation_evidence || node_has_applied_event).then(
            || MutationResult::AlreadyApplied {
                node_id: node.id.clone(),
                target,
                repository_fingerprint: snapshot.current_repository.fingerprint.clone(),
            },
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn target(change_id: &str, path: &str) -> PlannedTarget {
        PlannedTarget {
            change_id: change_id.into(),
            path: path.into(),
            role: if path.contains("test") {
                "tests".into()
            } else {
                "production".into()
            },
            intent: format!("change {path}"),
            acceptance_criteria_ids: vec!["ac-1".into()],
            operation: Default::default(),
            new_file: false,
        }
    }

    fn gate() -> ValidationGateSpec {
        ValidationGateSpec {
            gate_id: "test".into(),
            gate_type: ValidationGateType::TestSuite,
            command: "cargo test".into(),
            working_directory: ".".into(),
            required: true,
            ..ValidationGateSpec::default()
        }
    }

    fn snapshot(targets: &[PlannedTarget]) -> ExecutionSnapshot {
        let budget = MissionBudget::for_complexity(MissionComplexity::Small);
        ExecutionSnapshot {
            run_id: "run-1".into(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".into(),
                source_tree_hash: "tree-1".into(),
                ..RepositorySnapshot::default()
            },
            graph: ExecutionGraph::from_targets(
                "graph-1",
                MissionComplexity::Small,
                "tree-1",
                targets,
                &[gate()],
                &budget,
            ),
            budget: BudgetState::new(budget),
            ..ExecutionSnapshot::default()
        }
    }

    fn discovery_snapshot() -> ExecutionSnapshot {
        let budget = MissionBudget::for_complexity(MissionComplexity::Small);
        ExecutionSnapshot {
            run_id: "discovery-run".into(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".into(),
                source_tree_hash: "tree-1".into(),
                ..RepositorySnapshot::default()
            },
            graph: ExecutionGraph::bootstrap(
                "discovery-graph",
                "tree-1",
                MissionComplexity::Small,
                &budget,
            ),
            budget: BudgetState::new(budget),
            ..ExecutionSnapshot::default()
        }
    }

    #[test]
    fn mutation_actions_advance_prepare_mutate_verify_without_repository_exploration() {
        let mut state = snapshot(&[target("theme", "src/theme.ts")]);
        let node = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .expect("mutation node")
            .clone();

        let prepared = reconcile_execution(&state).expect("prepare decision");
        assert!(matches!(
            prepared,
            ExecutionDecision::ExecuteTarget {
                action: MutationAction::PrepareTargetContext { .. },
                ref target,
                ..
            } if target.allowed_tools.is_empty()
        ));

        state
            .events
            .push(ExecutionDomainEvent::TargetContextPrepared {
                sequence: 1,
                node_id: node.id.clone(),
                target_path: "src/theme.ts".into(),
                operation: TargetOperation::ModifyExisting,
                source_path: None,
                target_exists: Some(true),
                source_exists: None,
                repository_fingerprint: RepositoryFingerprint::new("tree-1"),
                evidence_ids: vec!["file-current".into()],
                target_content_hash: None,
                source_content_hash: None,
                accepted_intent_hash: hex::encode(Sha256::digest(b"change src/theme.ts")),
            });
        assert!(matches!(
            reconcile_execution(&state).expect("mutate decision"),
            ExecutionDecision::ExecuteTarget {
                action: MutationAction::MutateTarget {
                    expected_repository_fingerprint,
                    ..
                },
                ref target,
                ..
            } if expected_repository_fingerprint.as_str() == "tree-1"
                && target.allowed_tools == vec![ToolKind::ApplyPatch]
        ));

        state
            .events
            .push(ExecutionDomainEvent::TargetMutationProduced {
                sequence: 2,
                node_id: node.id,
                target_path: "src/theme.ts".into(),
                expected_repository_fingerprint: RepositoryFingerprint::new("tree-1"),
                repository_fingerprint: RepositoryFingerprint::new("tree-2"),
                before_content_hash: Some("before".into()),
                after_content_hash: Some("after".into()),
            });
        assert!(matches!(
            reconcile_execution(&state).expect("verify decision"),
            ExecutionDecision::ExecuteTarget {
                action: MutationAction::VerifyTargetState { .. },
                ..
            }
        ));
    }

    #[test]
    fn localized_discovery_inspects_once_then_finalizes_and_advances_to_planning() {
        let mut state = discovery_snapshot();
        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::ContinueDiscovery {
                action: DiscoveryAction::InspectRepository { .. }
            }
        ));

        for path in [
            "src/components/theme/ThemeProvider.tsx",
            "src/components/theme/ThemeToggle.tsx",
            "tests/theme-provider.test.tsx",
            "package.json",
        ] {
            state.evidence.capture_file(
                path,
                "tree-1",
                None,
                format!("evidence for {path}"),
                false,
            );
        }

        let evidence_ids = match reconcile_execution(&state).unwrap() {
            ExecutionDecision::ContinueDiscovery {
                action: DiscoveryAction::FinalizeImpactMap { evidence_ids },
            } => evidence_ids,
            decision => panic!("expected impact-map finalization, got {decision:?}"),
        };
        assert_eq!(evidence_ids.len(), 4);

        state
            .append_event(ExecutionDomainEvent::DiscoveryCompleted {
                sequence: 1,
                repository_fingerprint: "tree-1".into(),
            })
            .unwrap();
        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::ContinuePlanning {
                action: PlanningAction::BuildPlan { .. }
            }
        ));
        assert_eq!(
            state.graph.next_runnable_node().map(|node| node.kind),
            Some(ExecutionNodeKind::Planning)
        );
    }

    #[test]
    fn planning_build_references_the_accepted_impact_map_and_discovery_evidence() {
        let mut state = discovery_snapshot();
        for path in [
            "src/components/theme/ThemeProvider.tsx",
            "tests/theme-provider.test.tsx",
            "package.json",
        ] {
            state.evidence.capture_file(
                path,
                "tree-1",
                None,
                format!("evidence for {path}"),
                false,
            );
        }
        state
            .append_event(ExecutionDomainEvent::DiscoveryCompleted {
                sequence: 1,
                repository_fingerprint: "tree-1".into(),
            })
            .unwrap();
        match reconcile_execution(&state).unwrap() {
            ExecutionDecision::ContinuePlanning {
                action:
                    PlanningAction::BuildPlan {
                        impact_map_id,
                        evidence_ids,
                    },
            } => {
                assert_eq!(impact_map_id.as_str(), "impact-map:tree-1");
                assert_eq!(evidence_ids.len(), 3);
            }
            decision => panic!("expected typed plan build, got {decision:?}"),
        }
    }

    #[test]
    fn cached_planning_evidence_is_reused_instead_of_scheduling_a_duplicate_read() {
        let mut state = discovery_snapshot();
        for path in [
            "src/components/theme/ThemeProvider.tsx",
            "tests/theme-provider.test.tsx",
            "package.json",
        ] {
            state.evidence.capture_file(
                path,
                "tree-1",
                None,
                format!("evidence for {path}"),
                false,
            );
        }
        state
            .append_event(ExecutionDomainEvent::DiscoveryCompleted {
                sequence: 1,
                repository_fingerprint: "tree-1".into(),
            })
            .unwrap();
        let planning_id = ExecutionNodeId::new("planning");
        state
            .graph
            .set_node_status(&planning_id, ExecutionNodeStatus::FailedRecoverable)
            .unwrap();
        state.failures.record(FailureRecord::new(
            "plan-invalid",
            planning_id,
            FailureCategory::ModelArtifactRecoverable,
            1,
            "tree-1",
            serde_json::json!({
                "validation_errors": ["$.planned_changes[0].intent: required"],
                "previous_plan": {"implementation_status": "ready"},
                "missing_evidence": [{
                    "path": "src/components/theme/ThemeProvider.tsx",
                    "reason": "provider behavior must be confirmed"
                }]
            })
            .to_string(),
        ));
        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::ContinuePlanning {
                action: PlanningAction::RepairPlan { .. }
            }
        ));
    }

    #[test]
    fn planning_schedules_reads_only_for_a_concrete_uncached_evidence_gap() {
        let mut state = discovery_snapshot();
        for path in ["src/theme.ts", "tests/theme.test.ts", "package.json"] {
            state.evidence.capture_file(
                path,
                "tree-1",
                None,
                format!("evidence for {path}"),
                false,
            );
        }
        state
            .append_event(ExecutionDomainEvent::DiscoveryCompleted {
                sequence: 1,
                repository_fingerprint: "tree-1".into(),
            })
            .unwrap();
        let planning_id = ExecutionNodeId::new("planning");
        state
            .graph
            .set_node_status(&planning_id, ExecutionNodeStatus::FailedRecoverable)
            .unwrap();
        state.failures.record(FailureRecord::new(
            "plan-evidence-gap",
            planning_id,
            FailureCategory::ModelArtifactRecoverable,
            1,
            "tree-1",
            serde_json::json!({
                "missing_evidence": [{
                    "path": "src/new-theme-registry.ts",
                    "reason": "registry relationship is not present in discovery evidence"
                }]
            })
            .to_string(),
        ));
        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::ContinuePlanning {
                action: PlanningAction::ResolveEvidenceGap { missing_evidence }
            } if missing_evidence[0].path.as_deref() == Some("src/new-theme-registry.ts")
        ));
    }

    fn complete_node(snapshot: &mut ExecutionSnapshot, kind: ExecutionNodeKind) {
        let id = snapshot
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == kind)
            .unwrap()
            .id
            .clone();
        let status = if kind.is_mutation() {
            ExecutionNodeStatus::Applied
        } else if kind.is_validation() {
            ExecutionNodeStatus::Passed
        } else {
            ExecutionNodeStatus::Completed
        };
        snapshot.graph.set_node_status(&id, status).unwrap();
    }

    #[test]
    fn deterministic_target_order_advances_without_an_active_target_copy() {
        let mut state = snapshot(&[
            target("one", "src/one.rs"),
            target("two", "tests/two_test.rs"),
        ]);
        let first = reconcile_execution(&state).unwrap();
        let first_id = match first {
            ExecutionDecision::ExecuteTarget { node_id, .. } => node_id,
            decision => panic!("unexpected {decision:?}"),
        };
        state
            .graph
            .set_node_status(&first_id, ExecutionNodeStatus::Applied)
            .unwrap();
        state
            .current_repository
            .changed_paths
            .insert("src/one.rs".into());
        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::ExecuteTarget { target, .. }
                if target.target.path == "tests/two_test.rs"
        ));
    }

    #[test]
    fn completion_and_publication_cannot_bypass_diff_review() {
        let mut state = snapshot(&[target("one", "src/one.rs")]);
        complete_node(&mut state, ExecutionNodeKind::SourceMutation);
        complete_node(&mut state, ExecutionNodeKind::ValidationSuite);
        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::ReviewDiff { .. }
        ));
        complete_node(&mut state, ExecutionNodeKind::DiffReview);
        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::EvaluateCompletion { .. }
        ));
    }

    #[test]
    fn failed_validation_no_mutation_routes_applied_diff_to_draft_review() {
        let targets = [
            target("provider", "src/components/theme/ThemeProvider.tsx"),
            target("toggle", "src/components/theme/ThemeToggle.tsx"),
            target("styles", "src/styles/globals.css"),
            target("tests", "tests/theme-provider.test.tsx"),
        ];
        let gates = [
            ValidationGateSpec {
                gate_id: "focused-theme-provider".into(),
                gate_type: ValidationGateType::FocusedTest,
                command: "npx vitest run tests/theme-provider.test.tsx".into(),
                required: true,
                ..ValidationGateSpec::default()
            },
            ValidationGateSpec {
                gate_id: "suite".into(),
                gate_type: ValidationGateType::TestSuite,
                command: "npm test".into(),
                required: true,
                ..ValidationGateSpec::default()
            },
            ValidationGateSpec {
                gate_id: "build".into(),
                gate_type: ValidationGateType::Build,
                command: "npm run build".into(),
                required: true,
                ..ValidationGateSpec::default()
            },
        ];
        let budget = MissionBudget::for_complexity(MissionComplexity::Small);
        let mut state = ExecutionSnapshot {
            run_id: "attempt-30".into(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-after-four-mutations".into(),
                source_tree_hash: "tree-after-four-mutations".into(),
                changed_paths: targets.iter().map(|target| target.path.clone()).collect(),
                ..RepositorySnapshot::default()
            },
            graph: ExecutionGraph::from_targets(
                "attempt-30-graph",
                MissionComplexity::Small,
                "tree-before",
                &targets,
                &gates,
                &budget,
            ),
            budget: BudgetState::new(budget),
            ..ExecutionSnapshot::default()
        };
        for node in state
            .graph
            .nodes
            .iter_mut()
            .filter(|node| node.kind.is_mutation())
        {
            node.status = ExecutionNodeStatus::Applied;
        }
        state.graph.refresh_readiness();
        let validation_node = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::ValidationFocused)
            .expect("focused validation")
            .id
            .clone();
        let validation_gate = state
            .graph
            .node(&validation_node)
            .and_then(|node| node.validation.as_ref())
            .expect("focused validation gate")
            .clone();
        let validation_fingerprint = validation_gate.fingerprint("tree-after-four-mutations");
        state
            .append_event(ExecutionDomainEvent::ValidationEvidenceRecorded {
                sequence: 1,
                node_id: validation_node.clone(),
                evidence: ValidationEvidenceRecord {
                    evidence_id: "attempt-30-focused-evidence".into(),
                    node_id: validation_node.clone(),
                    gate_id: validation_gate.gate_id,
                    fingerprint: validation_fingerprint.clone(),
                    repository_fingerprint: "tree-after-four-mutations".into(),
                    command: validation_gate.command,
                    working_directory: validation_gate.working_directory,
                    status: ValidationEvidenceStatus::Failed,
                    exit_code: Some(1),
                    output_summary: "3 focused assertions failed".into(),
                    duration: std::time::Duration::from_millis(100),
                },
            })
            .unwrap();
        let mut failure = FailureRecord::new(
            "attempt-30-focused-failure",
            validation_node.clone(),
            FailureCategory::ValidationFailure,
            1,
            "tree-after-four-mutations",
            "3 focused assertions failed",
        );
        failure.target_path = Some("src/components/theme/ThemeProvider.tsx".into());
        failure.validation_command = Some("npx vitest run tests/theme-provider.test.tsx".into());
        failure.assertion_failures = vec![ValidationAssertionFailure {
            test_file: "tests/theme-provider.test.tsx".into(),
            test_name: "restores light-blue".into(),
            expected: "light-blue".into(),
            received: String::new(),
            implicated_paths: vec![
                "src/components/theme/ThemeProvider.tsx".into(),
                "tests/theme-provider.test.tsx".into(),
            ],
            ..ValidationAssertionFailure::default()
        }];
        state
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 2,
                failure: failure.clone(),
            })
            .unwrap();
        state
            .append_event(ExecutionDomainEvent::ValidationFailed {
                sequence: 3,
                node_id: validation_node.clone(),
                failure_id: failure.id.clone(),
                fingerprint: validation_fingerprint,
            })
            .unwrap();

        assert!(
            state
                .graph
                .nodes
                .iter()
                .filter(|node| node.kind.is_mutation())
                .all(|node| node.status == ExecutionNodeStatus::Applied)
        );
        assert_eq!(
            state
                .graph
                .nodes
                .iter()
                .find(|node| node.kind == ExecutionNodeKind::ValidationSuite)
                .unwrap()
                .status,
            ExecutionNodeStatus::Pending
        );
        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::ExecuteTarget {
                action: MutationAction::PrepareTargetContext { ref target, .. },
                ..
            } if target.path == "src/components/theme/ThemeProvider.tsx"
        ));

        state
            .append_event(ExecutionDomainEvent::ValidationRepairCompleted {
                sequence: 4,
                validation_node_id: validation_node,
                failure_id: failure.id,
                result: RepairResult::NoMutation {
                    diagnosis: Some(ValidationRepairDiagnosis::Inconclusive),
                    reason: "no safe mutation".into(),
                },
            })
            .unwrap();
        let (diff_node, reason) = match reconcile_execution(&state).unwrap() {
            ExecutionDecision::ReviewIncompleteDiff { node_id, reason } => (node_id, reason),
            decision => panic!("expected incomplete review, got {decision:?}"),
        };
        let overrides = state.incomplete_diff_dependency_overrides(&diff_node, reason);
        assert_eq!(overrides.len(), 3);
        assert!(
            overrides.iter().all(|override_| {
                override_.allowed_outcome == MissionOutcome::PartialReviewable
            })
        );
        state
            .append_event(ExecutionDomainEvent::IncompleteDiffReviewRequested {
                sequence: 5,
                node_id: diff_node.clone(),
                reason,
                dependency_overrides: overrides,
            })
            .unwrap();
        state
            .append_event(ExecutionDomainEvent::DiffReviewed {
                sequence: 6,
                node_id: diff_node,
                evidence_ids: Vec::new(),
            })
            .unwrap();
        let completion_node = match reconcile_execution(&state).unwrap() {
            ExecutionDecision::EvaluateCompletion { node_id } => node_id,
            decision => panic!("expected partial completion evaluation, got {decision:?}"),
        };
        state
            .append_event(ExecutionDomainEvent::CompletionEvaluated {
                sequence: 7,
                node_id: completion_node,
                outcome: MissionOutcome::PartialReviewable,
            })
            .unwrap();
        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::Publish {
                mode: PublicationMode::Draft
            }
        ));
        assert!(
            state
                .graph
                .nodes
                .iter()
                .filter(|node| node.kind.is_mutation())
                .all(|node| node.status == ExecutionNodeStatus::Applied)
        );
        assert_eq!(
            state.target_state(
                &state
                    .graph
                    .nodes
                    .iter()
                    .find(|node| {
                        node.target
                            .as_ref()
                            .is_some_and(|target| target.path == "tests/theme-provider.test.tsx")
                    })
                    .unwrap()
                    .id
            ),
            Some(TargetState {
                mutation_status: MutationStatus::Applied,
                validation_status: ValidationStatus::FailedCode,
            })
        );

        let publication_node = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Publication)
            .expect("publication node")
            .id
            .clone();
        for event in [
            ExecutionDomainEvent::PublicationStarted {
                sequence: 8,
                node_id: publication_node.clone(),
                mode: PublicationMode::Draft,
            },
            ExecutionDomainEvent::CommitCreated {
                sequence: 9,
                node_id: publication_node.clone(),
                commit_sha: "attempt-30-commit".into(),
            },
            ExecutionDomainEvent::BranchPushed {
                sequence: 10,
                node_id: publication_node.clone(),
                branch: "rustgrid/attempt-30".into(),
            },
            ExecutionDomainEvent::PullRequestCreated {
                sequence: 11,
                node_id: publication_node,
                url: "https://example.test/pull/30".into(),
                number: Some(30),
                draft: true,
            },
            ExecutionDomainEvent::RunFinished {
                sequence: 12,
                outcome: MissionOutcome::PartialReviewable,
            },
            ExecutionDomainEvent::ExecutionResumed {
                sequence: 13,
                execution_attempt: 31,
                previous_outcome: Some(MissionOutcome::PartialReviewable),
            },
        ] {
            state.append_event(event).unwrap();
        }
        assert!(
            state
                .graph
                .nodes
                .iter()
                .filter(|node| node.kind.is_mutation())
                .all(|node| node.status == ExecutionNodeStatus::Applied)
        );
        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::ExecuteTarget {
                action: MutationAction::PrepareTargetContext { ref target, .. },
                ..
            } if target.path == "src/components/theme/ThemeProvider.tsx"
        ));
    }

    #[test]
    fn already_applied_mutation_is_a_no_op_not_a_failure() {
        let mut state = snapshot(&[target("one", "src/one.rs")]);
        let node = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .unwrap()
            .id
            .clone();
        state
            .current_repository
            .changed_paths
            .insert("src/one.rs".into());
        state
            .graph
            .set_node_status(&node, ExecutionNodeStatus::Applied)
            .unwrap();
        let before_repairs = state.budget.usage_for(&node).repair_attempts;
        assert!(matches!(
            classify_mutation_request(&state, &node).unwrap(),
            Some(MutationResult::AlreadyApplied { .. })
        ));
        assert!(!state.failures.has_unresolved());
        assert_eq!(
            state.budget.usage_for(&node).repair_attempts,
            before_repairs
        );
    }

    #[test]
    fn changed_path_does_not_conflate_two_nodes_targeting_the_same_file() {
        let mut state = snapshot(&[
            target("first", "src/shared.rs"),
            target("second", "src/shared.rs"),
        ]);
        let mutation_nodes = state
            .graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_mutation())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(mutation_nodes.len(), 2);
        state
            .current_repository
            .changed_paths
            .insert("src/shared.rs".into());
        state
            .graph
            .set_node_status(&mutation_nodes[0], ExecutionNodeStatus::Applied)
            .unwrap();

        assert!(matches!(
            classify_mutation_request(&state, &mutation_nodes[0]).unwrap(),
            Some(MutationResult::AlreadyApplied { .. })
        ));
        assert_eq!(
            classify_mutation_request(&state, &mutation_nodes[1]).unwrap(),
            None
        );
    }

    #[test]
    fn validation_repair_rejects_ambiguous_same_path_target_identity() {
        let mut state = snapshot(&[
            target("first", "src/shared.rs"),
            target("second", "src/shared.rs"),
        ]);
        for node in state
            .graph
            .nodes
            .iter_mut()
            .filter(|node| node.kind.is_mutation())
        {
            node.status = ExecutionNodeStatus::Applied;
        }
        state.graph.refresh_readiness();
        let validation_id = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_validation())
            .expect("validation node")
            .id
            .clone();
        state
            .graph
            .set_node_status(&validation_id, ExecutionNodeStatus::FailedRecoverable)
            .unwrap();
        let mut failure = FailureRecord::new(
            "validation-shared",
            validation_id,
            FailureCategory::ValidationFailure,
            1,
            "tree-1",
            "shared path failed validation",
        );
        failure.target_path = Some("src/shared.rs".into());
        state.failures.record(failure);

        let error = reconcile_execution(&state).expect_err("ambiguous path must be rejected");
        assert_eq!(error.code, "ambiguous_validation_failure_target");
    }

    #[test]
    fn validation_repair_rejects_unmapped_failure_instead_of_using_last_successful_target() {
        let mut state = snapshot(&[
            target("first", "src/first.rs"),
            target("last", "src/last.rs"),
        ]);
        for node in state
            .graph
            .nodes
            .iter_mut()
            .filter(|node| node.kind.is_mutation())
        {
            node.status = ExecutionNodeStatus::Applied;
        }
        state.graph.refresh_readiness();
        let validation_id = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_validation())
            .expect("validation node")
            .id
            .clone();
        state
            .graph
            .set_node_status(&validation_id, ExecutionNodeStatus::FailedRecoverable)
            .unwrap();
        state.failures.record(FailureRecord::new(
            "validation-unmapped",
            validation_id,
            FailureCategory::ValidationFailure,
            1,
            "tree-1",
            "validation failed without identifying a planned target",
        ));

        let error = reconcile_execution(&state)
            .expect_err("unmapped validation must not repair the last successful target");
        assert_eq!(error.code, "validation_failure_target_missing");
        assert!(error.message.contains("cannot be assigned"));
    }

    #[test]
    fn superseded_failure_does_not_keep_the_graph_in_repair() {
        let mut state = snapshot(&[target("one", "src/one.rs"), target("two", "src/two.rs")]);
        let ids = state
            .graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_mutation())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        state
            .graph
            .set_node_status(&ids[0], ExecutionNodeStatus::FailedRecoverable)
            .unwrap();
        let mut failure = FailureRecord::new(
            "failure-1",
            ids[0].clone(),
            FailureCategory::MutationConflict,
            1,
            "tree-1",
            "stale replacement",
        );
        failure.target_path = Some("src/one.rs".into());
        failure.mark_superseded("tree-2");
        state.failures.record(failure);
        state.current_repository.fingerprint = "tree-2".into();
        state
            .current_repository
            .changed_paths
            .insert("src/one.rs".into());
        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::ExecuteTarget { node_id, .. } if node_id == ids[1]
        ));
    }

    #[test]
    fn passed_validation_fingerprint_executes_only_once() {
        let mut state = snapshot(&[target("one", "src/one.rs")]);
        complete_node(&mut state, ExecutionNodeKind::SourceMutation);
        let validation = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_validation())
            .unwrap()
            .clone();
        let gate = validation.validation.as_ref().unwrap();
        let fingerprint = gate.fingerprint("tree-1");
        state.evidence.record_validation(ValidationEvidenceRecord {
            evidence_id: "validation-1".into(),
            node_id: validation.id.clone(),
            gate_id: gate.gate_id.clone(),
            fingerprint,
            repository_fingerprint: "tree-1".into(),
            command: gate.command.clone(),
            working_directory: gate.working_directory.clone(),
            status: ValidationEvidenceStatus::Passed,
            ..ValidationEvidenceRecord::default()
        });
        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::ReviewDiff { .. }
        ));
    }

    #[test]
    fn infrastructure_failure_is_not_partial_completion() {
        let mut state = snapshot(&[target("one", "src/one.rs")]);
        let node = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .unwrap()
            .id
            .clone();
        state.failures.record(FailureRecord::new(
            "infra-1",
            node,
            FailureCategory::InfrastructureFailure,
            1,
            "tree-1",
            "github authentication failed",
        ));
        assert_eq!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::StopForGuardrail {
                outcome: MissionOutcome::FailedInfrastructure,
                reason: GuardrailReason::InfrastructureFailure,
            }
        );
        state
            .append_event(ExecutionDomainEvent::GuardrailTriggered {
                sequence: state.next_event_sequence(),
                reason: GuardrailReason::InfrastructureFailure,
                outcome: MissionOutcome::FailedInfrastructure,
                detail: "authoritative infrastructure stop".into(),
            })
            .unwrap();
        assert_eq!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::Finish {
                outcome: MissionOutcome::FailedInfrastructure,
            }
        );
    }

    #[test]
    fn published_recovery_finishes_the_authorized_partial_outcome() {
        let mut state = snapshot(&[target("one", "src/one.rs")]);
        state
            .current_repository
            .changed_paths
            .insert("src/one.rs".into());
        complete_node(&mut state, ExecutionNodeKind::Discovery);
        complete_node(&mut state, ExecutionNodeKind::Planning);
        complete_node(&mut state, ExecutionNodeKind::SourceMutation);
        complete_node(&mut state, ExecutionNodeKind::ValidationSuite);
        let validation = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_validation())
            .unwrap()
            .clone();
        let gate = validation.validation.as_ref().unwrap();
        let evidence_id = "recovery-validation".to_owned();
        state.evidence.record_validation(ValidationEvidenceRecord {
            evidence_id: evidence_id.clone(),
            node_id: validation.id,
            gate_id: gate.gate_id.clone(),
            fingerprint: gate.fingerprint("tree-1"),
            repository_fingerprint: "tree-1".into(),
            command: gate.command.clone(),
            working_directory: gate.working_directory.clone(),
            status: ValidationEvidenceStatus::Passed,
            ..ValidationEvidenceRecord::default()
        });
        let publication = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Publication)
            .unwrap()
            .id
            .clone();
        for event in [
            ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: 1,
                node_id: publication.clone(),
                repository_fingerprint: "tree-1".into(),
                validation_evidence_ids: vec![evidence_id],
            },
            ExecutionDomainEvent::CommitCreated {
                sequence: 2,
                node_id: publication.clone(),
                commit_sha: "recovery-commit".into(),
            },
            ExecutionDomainEvent::BranchPushed {
                sequence: 3,
                node_id: publication.clone(),
                branch: "rustgrid/recovery".into(),
            },
            ExecutionDomainEvent::PullRequestCreated {
                sequence: 4,
                node_id: publication,
                url: "https://example.test/pull/1".into(),
                number: Some(1),
                draft: true,
            },
        ] {
            state.append_event(event).unwrap();
        }

        let stop = ExecutionDecision::StopForGuardrail {
            outcome: MissionOutcome::PartialReviewable,
            reason: GuardrailReason::OrchestrationInvariantViolation,
        };
        assert_eq!(reconcile_execution(&state).unwrap(), stop);
        state
            .append_event(ExecutionDomainEvent::GuardrailTriggered {
                sequence: 5,
                reason: GuardrailReason::OrchestrationInvariantViolation,
                outcome: MissionOutcome::PartialReviewable,
                detail: "authorized recovery publication".into(),
            })
            .unwrap();
        assert_eq!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::Finish {
                outcome: MissionOutcome::PartialReviewable,
            }
        );
    }

    #[test]
    fn failed_validation_events_preserve_blocking_and_recoverable_decisions() {
        for (category, expected_status) in [
            (
                FailureCategory::ValidationFailure,
                ExecutionNodeStatus::FailedRecoverable,
            ),
            (
                FailureCategory::InfrastructureFailure,
                ExecutionNodeStatus::FailedBlocking,
            ),
        ] {
            let mut state = snapshot(&[target("one", "src/one.rs")]);
            complete_node(&mut state, ExecutionNodeKind::SourceMutation);
            state
                .current_repository
                .changed_paths
                .insert("src/one.rs".into());
            let validation = state
                .graph
                .nodes
                .iter()
                .find(|node| node.kind.is_validation())
                .expect("validation node")
                .clone();
            let gate = validation.validation.as_ref().expect("validation gate");
            let fingerprint = gate.fingerprint("tree-1");
            state
                .append_event(ExecutionDomainEvent::ValidationEvidenceRecorded {
                    sequence: 1,
                    node_id: validation.id.clone(),
                    evidence: ValidationEvidenceRecord {
                        evidence_id: format!("evidence-{category:?}"),
                        node_id: validation.id.clone(),
                        gate_id: gate.gate_id.clone(),
                        fingerprint: fingerprint.clone(),
                        repository_fingerprint: "tree-1".into(),
                        command: gate.command.clone(),
                        working_directory: gate.working_directory.clone(),
                        status: ValidationEvidenceStatus::Failed,
                        exit_code: Some(1),
                        output_summary: "validation failed".into(),
                        duration: std::time::Duration::from_millis(1),
                    },
                })
                .expect("record validation evidence");
            let failure_id = FailureId::new(format!("failure-{category:?}"));
            let mut failure = FailureRecord::new(
                failure_id.clone(),
                validation.id.clone(),
                category,
                1,
                "tree-1",
                "validation failed",
            );
            failure.target_path = Some("src/one.rs".into());
            state
                .append_event(ExecutionDomainEvent::FailureRecorded {
                    sequence: 2,
                    failure,
                })
                .expect("record validation failure");
            state
                .append_event(ExecutionDomainEvent::ValidationFailed {
                    sequence: 3,
                    node_id: validation.id.clone(),
                    failure_id: failure_id.clone(),
                    fingerprint,
                })
                .expect("materialize validation failure");

            assert_eq!(
                state.graph.node(&validation.id).map(|node| node.status),
                Some(expected_status)
            );
            match category {
                FailureCategory::InfrastructureFailure => assert_eq!(
                    reconcile_execution(&state).expect("infrastructure decision"),
                    ExecutionDecision::ReviewIncompleteDiff {
                        node_id: ExecutionNodeId::new("diff-review"),
                        reason: IncompleteReason::ValidationInfrastructureFailure,
                    }
                ),
                FailureCategory::ValidationFailure => assert!(matches!(
                    reconcile_execution(&state).expect("repair decision"),
                    ExecutionDecision::ExecuteTarget {
                        action: MutationAction::PrepareTargetContext { .. },
                        ..
                    }
                )),
                _ => unreachable!("fixture only covers validation failure categories"),
            }
        }
    }

    #[test]
    fn cancellation_is_resumable_and_deterministic() {
        let mut state = snapshot(&[target("one", "src/one.rs")]);
        state.cancellation = Some(CancellationState {
            requested_at: "t1".into(),
            reason: "cancelled by user".into(),
            checkpointed: true,
            ..CancellationState::default()
        });
        assert_eq!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::StopForGuardrail {
                outcome: MissionOutcome::Cancelled,
                reason: GuardrailReason::Cancellation,
            }
        );
    }

    #[test]
    fn no_progress_at_the_soft_bound_returns_a_guardrail_decision() {
        let mut state = snapshot(&[target("one", "src/one.rs")]);
        let node_id = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .expect("mutation node")
            .id
            .clone();
        let node = state.graph.node_mut(&node_id).expect("mutation node");
        node.budget = NodeBudget {
            max_model_calls: 10,
            max_cost_micros: 10_000,
            max_duration: std::time::Duration::from_secs(100),
            max_repair_attempts: 1,
        };
        state.budget.mission = MissionBudget {
            max_model_calls: 20,
            max_cost_micros: 20_000,
            max_duration: std::time::Duration::from_secs(200),
            max_target_repair_rounds: 1,
        };
        for _ in 0..8 {
            state
                .budget
                .record_model_call(node_id.clone(), 100, std::time::Duration::from_secs(1));
        }

        assert_eq!(
            reconcile_execution(&state).expect("deterministic no-progress decision"),
            ExecutionDecision::StopForGuardrail {
                outcome: MissionOutcome::BlockedNoDiff,
                reason: GuardrailReason::NoProgress,
            }
        );
    }

    #[test]
    fn exhausted_repair_budget_stops_before_another_repair_call() {
        let mut state = snapshot(&[target("one", "src/one.rs")]);
        let node_id = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .expect("mutation node")
            .id
            .clone();
        let node = state.graph.node_mut(&node_id).expect("mutation node");
        node.status = ExecutionNodeStatus::FailedRecoverable;
        node.budget.max_repair_attempts = 1;
        let mut failure = FailureRecord::new(
            "failure-1",
            node_id.clone(),
            FailureCategory::MutationConflict,
            1,
            "tree-1",
            "replacement did not match",
        );
        failure.target_path = Some("src/one.rs".into());
        state.failures.record(failure);
        state.budget.record_repair_attempt(node_id);

        assert_eq!(
            reconcile_execution(&state).expect("bounded repair decision"),
            ExecutionDecision::StopForGuardrail {
                outcome: MissionOutcome::BlockedNoDiff,
                reason: GuardrailReason::NodeBudgetExhausted,
            }
        );
    }

    #[test]
    fn explicit_partial_guardrail_advances_to_validation_and_preserves_remaining_work() {
        let mut state = snapshot(&[
            target("source", "src/provider.rs"),
            target("tests", "tests/provider_test.rs"),
        ]);
        let source_id = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source node")
            .id
            .clone();
        let remaining_id = state
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::TestMutation)
            .expect("test node")
            .id
            .clone();
        state
            .graph
            .set_node_status(&source_id, ExecutionNodeStatus::Applied)
            .expect("apply useful source work");
        state.current_repository.fingerprint = "tree-2".into();
        state
            .current_repository
            .changed_paths
            .insert("src/provider.rs".into());
        state
            .append_event(ExecutionDomainEvent::GuardrailTriggered {
                sequence: 1,
                reason: GuardrailReason::NodeBudgetExhausted,
                outcome: MissionOutcome::PartialReviewable,
                detail: "implementation budget ended with useful work".into(),
            })
            .expect("partial validation handoff");

        assert!(matches!(
            reconcile_execution(&state).expect("validation follows partial handoff"),
            ExecutionDecision::RunValidation { .. }
        ));
        assert!(
            state
                .remaining_required_nodes()
                .iter()
                .any(|node| node.id == remaining_id),
            "dependency override must not claim the remaining target was applied"
        );
    }

    #[test]
    fn malformed_relocation_contract_is_an_explicit_orchestration_failure() {
        let mut malformed = target("move", "src/new.rs");
        malformed.operation = TargetOperation::Move {
            source: String::new(),
            destination: "src/new.rs".into(),
        };
        let state = snapshot(&[malformed]);
        let error = reconcile_execution(&state).unwrap_err();
        assert_eq!(error.code, "unsupported_operation_contract");
    }

    #[test]
    fn late_operation_conflict_preserves_applied_work_and_routes_incomplete_review() {
        let mut state = snapshot(&[
            target("first", "src/first.rs"),
            target("second", "src/second.rs"),
        ]);
        let mutation_ids = state
            .graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_mutation())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        state
            .graph
            .set_node_status(&mutation_ids[0], ExecutionNodeStatus::Applied)
            .unwrap();
        state
            .graph
            .set_node_status(&mutation_ids[1], ExecutionNodeStatus::FailedRecoverable)
            .unwrap();
        state.current_repository.fingerprint = "tree-2".into();
        state
            .current_repository
            .changed_paths
            .insert("src/first.rs".into());
        let mut failure = FailureRecord::new(
            "target-conflict",
            mutation_ids[1].clone(),
            FailureCategory::PlanRepositoryConflict,
            1,
            "tree-2",
            "accepted modify target is absent",
        );
        failure.code = Some("expected_existing_target_missing".into());
        failure.target_path = Some("src/second.rs".into());
        state.failures.record(failure);

        assert!(matches!(
            reconcile_execution(&state).unwrap(),
            ExecutionDecision::ReviewIncompleteDiff {
                reason: IncompleteReason::TargetOperationConflict,
                ..
            }
        ));
        assert_eq!(
            state.graph.node(&mutation_ids[0]).unwrap().status,
            ExecutionNodeStatus::Applied
        );
        assert_eq!(
            state.graph.node(&mutation_ids[1]).unwrap().status,
            ExecutionNodeStatus::FailedRecoverable
        );
    }
}
