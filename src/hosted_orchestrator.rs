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
    InspectRepository { missing_evidence: Vec<String> },
    ReuseEvidence { evidence_ids: Vec<String> },
    RepairArtifact { failure_id: FailureId },
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct PlanRepair {
    pub failure_id: Option<FailureId>,
    #[serde(default)]
    pub missing_criterion_ids: Vec<String>,
    #[serde(default)]
    pub invalid_fields: Vec<String>,
    pub instruction: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetRepairContext {
    pub failure: FailureRecord,
    pub target: TargetExecutionContext,
    pub next_repair_attempt: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ExecutionDecision {
    ContinueDiscovery {
        action: DiscoveryAction,
    },
    FinalizeDiscovery,
    BuildPlan,
    RepairPlan {
        repair: PlanRepair,
    },
    ExecuteTarget {
        node_id: ExecutionNodeId,
        target: TargetExecutionContext,
    },
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
            Self::ContinueDiscovery { .. } | Self::FinalizeDiscovery => {
                HostedExecutionStage::Discovery
            }
            Self::BuildPlan | Self::RepairPlan { .. } => HostedExecutionStage::Planning,
            Self::ExecuteTarget { .. } | Self::RepairTarget { .. } => {
                HostedExecutionStage::Implementation
            }
            Self::RunValidation { .. } => HostedExecutionStage::Validation,
            Self::ReviewDiff { .. } | Self::EvaluateCompletion { .. } => {
                HostedExecutionStage::Review
            }
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
            | Self::EvaluateCompletion { node_id } => Some(node_id),
            _ => None,
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
        return Ok(ExecutionDecision::StopForGuardrail {
            outcome: MissionOutcome::FailedInfrastructure,
            reason: if failure.category == FailureCategory::InfrastructureFailure {
                GuardrailReason::InfrastructureFailure
            } else {
                GuardrailReason::BlockingFailure
            },
        });
    }

    if let Some(failure) = snapshot
        .failures
        .unresolved()
        .find(|failure| failure.category == FailureCategory::ValidationFailure)
    {
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

fn decision_for_node(
    snapshot: &ExecutionSnapshot,
    node: &ExecutionNode,
) -> Result<ExecutionDecision, OrchestrationInvariantError> {
    match node.kind {
        ExecutionNodeKind::Discovery => match node.status {
            ExecutionNodeStatus::Applied => Ok(ExecutionDecision::FinalizeDiscovery),
            ExecutionNodeStatus::FailedRecoverable => {
                let failure = unresolved_failure_for_node(snapshot, &node.id)?;
                Ok(ExecutionDecision::ContinueDiscovery {
                    action: DiscoveryAction::RepairArtifact {
                        failure_id: failure.id.clone(),
                    },
                })
            }
            _ => {
                let evidence_ids = snapshot
                    .evidence
                    .files
                    .values()
                    .filter(|evidence| {
                        evidence.repository_fingerprint == snapshot.current_repository.fingerprint
                    })
                    .map(|evidence| evidence.evidence_id.clone())
                    .collect::<Vec<_>>();
                Ok(ExecutionDecision::ContinueDiscovery {
                    action: if evidence_ids.is_empty() {
                        DiscoveryAction::InspectRepository {
                            missing_evidence: vec![
                                "implementation targets".into(),
                                "related tests".into(),
                                "validation commands".into(),
                            ],
                        }
                    } else {
                        DiscoveryAction::ReuseEvidence { evidence_ids }
                    },
                })
            }
        },
        ExecutionNodeKind::Planning => {
            if node.status == ExecutionNodeStatus::FailedRecoverable {
                let failure = unresolved_failure_for_node(snapshot, &node.id)?;
                Ok(ExecutionDecision::RepairPlan {
                    repair: PlanRepair {
                        failure_id: Some(failure.id.clone()),
                        instruction: failure.message.clone(),
                        ..PlanRepair::default()
                    },
                })
            } else {
                Ok(ExecutionDecision::BuildPlan)
            }
        }
        ExecutionNodeKind::SourceMutation | ExecutionNodeKind::TestMutation => {
            if node.status == ExecutionNodeStatus::FailedRecoverable {
                let failure = unresolved_failure_for_node(snapshot, &node.id)?;
                repair_target_decision(snapshot, node, failure)
            } else {
                let target = snapshot.target_execution_context(
                    &node.id,
                    vec![
                        ToolKind::ReadFile,
                        ToolKind::SearchRepository,
                        ToolKind::ApplyPatch,
                        ToolKind::CreateFile,
                        ToolKind::DeleteFile,
                        ToolKind::RunFocusedCommand,
                    ],
                )?;
                Ok(ExecutionDecision::ExecuteTarget {
                    node_id: node.id.clone(),
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
    let target = snapshot.target_execution_context(
        &node.id,
        vec![
            ToolKind::ReadFile,
            ToolKind::SearchRepository,
            ToolKind::ApplyPatch,
            ToolKind::CreateFile,
            ToolKind::RunFocusedCommand,
        ],
    )?;
    let usage = snapshot.budget.usage_for(&node.id);
    Ok(ExecutionDecision::RepairTarget {
        node_id: node.id.clone(),
        failure_id: failure.id.clone(),
        context: TargetRepairContext {
            failure: failure.clone(),
            target,
            next_repair_attempt: usage.repair_attempts.saturating_add(1),
        },
    })
}

fn unresolved_failure_for_node<'a>(
    snapshot: &'a ExecutionSnapshot,
    node_id: &ExecutionNodeId,
) -> Result<&'a FailureRecord, OrchestrationInvariantError> {
    snapshot
        .failures
        .unresolved_for_node(node_id)
        .next()
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
    (node.budget.max_model_calls > 0 && usage.model_calls >= node.budget.max_model_calls)
        || (node.budget.max_cost_micros > 0 && usage.cost_micros >= node.budget.max_cost_micros)
        || (!node.budget.max_duration.is_zero() && usage.duration >= node.budget.max_duration)
        || usage.repair_attempts > node.budget.max_repair_attempts
        || snapshot.budget.total_model_calls >= snapshot.budget.mission.max_model_calls
        || snapshot.budget.total_cost_micros >= snapshot.budget.mission.max_cost_micros
        || snapshot.budget.elapsed >= snapshot.budget.mission.max_duration
}

fn repair_budget_exhausted(snapshot: &ExecutionSnapshot, node: &ExecutionNode) -> bool {
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
                    ExecutionDecision::StopForGuardrail {
                        outcome: MissionOutcome::FailedInfrastructure,
                        reason: GuardrailReason::InfrastructureFailure,
                    }
                ),
                FailureCategory::ValidationFailure => assert!(matches!(
                    reconcile_execution(&state).expect("repair decision"),
                    ExecutionDecision::RepairTarget {
                        failure_id: decision_failure_id,
                        ..
                    } if decision_failure_id == failure_id
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
}
