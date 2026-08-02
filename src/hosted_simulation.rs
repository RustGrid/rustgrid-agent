//! Deterministic, provider-free replay harness for the hosted orchestrator.
//!
//! The harness intentionally executes the production [`reconcile_execution`]
//! function and records state through the same domain-event reducer used by the
//! hosted adapter.  Its effects are scripted fakes: no repository, command,
//! model, network, git, or publication I/O is performed.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use crate::execution_graph::*;
use crate::hosted_orchestrator::{
    ExecutionDecision, OrchestrationInvariantError, classify_mutation_request, reconcile_execution,
};

const DEFAULT_MAX_STEPS: usize = 256;
const DEFAULT_MODEL_CALL_COST_MICROS: u64 = 50_000;
const DEFAULT_MODEL_CALL_DURATION: Duration = Duration::from_secs(1);

/// A complete, deterministic mission fixture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptedMission {
    pub name: String,
    pub complexity: MissionComplexity,
    pub targets: Vec<PlannedTarget>,
    pub validation_gates: Vec<ValidationGateSpec>,
    /// Canonical acceptance criteria that the complete scripted plan must cover.
    /// When empty, the union referenced by the targets is used.
    pub required_acceptance_criteria_ids: Vec<String>,
    pub actions: Vec<ScriptedAction>,
    pub initial_repository_fingerprint: String,
    pub completion_outcome: MissionOutcome,
    pub max_steps: usize,
    pub model_call_cost_micros: u64,
    pub model_call_duration: Duration,
}

impl ScriptedMission {
    pub fn new(name: impl Into<String>, complexity: MissionComplexity) -> Self {
        Self {
            name: name.into(),
            complexity,
            targets: Vec::new(),
            validation_gates: Vec::new(),
            required_acceptance_criteria_ids: Vec::new(),
            actions: Vec::new(),
            initial_repository_fingerprint: "sim-tree-0000".to_owned(),
            completion_outcome: MissionOutcome::Complete,
            max_steps: DEFAULT_MAX_STEPS,
            model_call_cost_micros: DEFAULT_MODEL_CALL_COST_MICROS,
            model_call_duration: DEFAULT_MODEL_CALL_DURATION,
        }
    }

    pub fn with_target(mut self, target: PlannedTarget) -> Self {
        self.targets.push(target);
        self
    }

    pub fn with_validation_gate(mut self, gate: ValidationGateSpec) -> Self {
        self.validation_gates.push(gate);
        self
    }

    pub fn with_required_acceptance_criteria(
        mut self,
        criteria: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.required_acceptance_criteria_ids = criteria.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_action(mut self, action: ScriptedAction) -> Self {
        self.actions.push(action);
        self
    }

    pub fn with_completion_outcome(mut self, outcome: MissionOutcome) -> Self {
        self.completion_outcome = outcome;
        self
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }

    pub fn covered_acceptance_criteria(&self) -> Vec<String> {
        let mut criteria = self
            .targets
            .iter()
            .flat_map(|target| target.acceptance_criteria_ids.iter().cloned())
            .collect::<Vec<_>>();
        criteria.sort();
        criteria.dedup();
        criteria
    }
}

/// One fake effect or injected condition in a replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptedAction {
    /// Supplies an explicit model result for repository discovery.
    DiscoveryOutput { result: ScriptedDiscoveryResult },
    /// Supplies an explicit model result for implementation planning.
    PlanningOutput { result: ScriptedPlanningResult },
    /// A preparation read failed before target execution. It is diagnostic and
    /// must not manufacture an unchanged-tree validation path.
    PreparationReadFailure { message: String },
    /// Supplies the next result for a mutation or repair of `path`.
    TargetResult {
        path: String,
        result: ScriptedTargetResult,
    },
    /// Attempts a duplicate mutation after `path` is already represented in the
    /// authoritative repository state.
    DuplicateMutation { path: String },
    /// Supplies the next result for a validation gate.
    ValidationResult {
        gate_id: String,
        result: ScriptedValidationResult,
    },
    /// Requests an identical validation again. The evidence cache must suppress
    /// it without spending or appending a second validation event.
    RepeatValidation { gate_id: String },
    /// Records prior passing validation for the current repository fingerprint.
    /// This is useful for exercising safe partial publication.
    SeedPassedValidation { gate_id: String },
    /// Deterministically consumes the selected target's model-call allowance.
    ExhaustTargetBudget { path: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptedDiscoveryResult {
    Completed,
    RecoverableFailure { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptedPlanningResult {
    Accepted,
    RecoverableFailure { message: String },
}

impl ScriptedAction {
    pub fn applied(path: impl Into<String>) -> Self {
        Self::TargetResult {
            path: path.into(),
            result: ScriptedTargetResult::Applied,
        }
    }

    pub fn duplicate(path: impl Into<String>) -> Self {
        Self::DuplicateMutation { path: path.into() }
    }

    pub fn validation_passes(gate_id: impl Into<String>) -> Self {
        Self::ValidationResult {
            gate_id: gate_id.into(),
            result: ScriptedValidationResult::Passed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptedTargetResult {
    Applied,
    AlreadyApplied,
    RecoverableFailure { message: String },
    BlockingFailure { message: String },
    InfrastructureFailure { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptedValidationResult {
    Passed,
    RecoverableFailure { message: String },
    InfrastructureFailure { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetResultRecord {
    pub node_id: ExecutionNodeId,
    pub path: String,
    pub result: ScriptedTargetResult,
    pub repository_fingerprint_before: String,
    pub repository_fingerprint_after: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationRunRecord {
    pub node_id: ExecutionNodeId,
    pub gate_id: String,
    pub fingerprint: String,
    pub result: ScriptedValidationResult,
}

/// Observable output from one complete replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulationReport {
    pub mission_name: String,
    pub outcome: MissionOutcome,
    pub snapshot: ExecutionSnapshot,
    pub decisions: Vec<ExecutionDecision>,
    pub target_results: Vec<TargetResultRecord>,
    pub validation_runs: Vec<ValidationRunRecord>,
    pub suppressed_validation_runs: u32,
    pub preparation_read_failures: u32,
    pub scripted_model_outputs_consumed: u32,
    pub phase_trace: Vec<SimulationPhase>,
    pub steps: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimulationPhase {
    Discovery,
    Planning,
    Implementation,
    Repair,
    Validation,
    DiffReview,
    CompletionEvaluation,
    Publication,
    Terminal,
}

impl SimulationPhase {
    pub fn permits_transition_to(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (Self::Discovery, Self::Planning)
                    | (Self::Planning, Self::Implementation)
                    | (Self::Implementation, Self::Repair | Self::Validation)
                    | (Self::Repair, Self::Implementation | Self::Validation)
                    | (Self::Validation, Self::Repair | Self::DiffReview)
                    | (Self::DiffReview, Self::CompletionEvaluation)
                    | (
                        Self::CompletionEvaluation,
                        Self::Publication | Self::Terminal
                    )
                    | (Self::Publication, Self::Terminal)
                    | (_, Self::Terminal)
            )
    }

    pub const fn for_decision(decision: &ExecutionDecision) -> Self {
        match decision {
            ExecutionDecision::ContinueDiscovery { .. } => Self::Discovery,
            ExecutionDecision::BuildPlan | ExecutionDecision::RepairPlan { .. } => Self::Planning,
            ExecutionDecision::ExecuteTarget { .. } => Self::Implementation,
            ExecutionDecision::RepairTarget { .. } => Self::Repair,
            ExecutionDecision::RunValidation { .. } => Self::Validation,
            ExecutionDecision::ReviewDiff { .. } => Self::DiffReview,
            ExecutionDecision::EvaluateCompletion { .. } => Self::CompletionEvaluation,
            ExecutionDecision::Publish { .. } => Self::Publication,
            ExecutionDecision::StopForGuardrail {
                outcome: MissionOutcome::PartialReviewable,
                ..
            } => Self::Validation,
            ExecutionDecision::Finish { .. } | ExecutionDecision::StopForGuardrail { .. } => {
                Self::Terminal
            }
        }
    }
}

impl SimulationReport {
    pub fn validation_run_count(&self, gate_id: &str) -> usize {
        self.validation_runs
            .iter()
            .filter(|run| run.gate_id == gate_id)
            .count()
    }

    pub fn unresolved_failure_count(&self) -> usize {
        self.snapshot.failures.unresolved().count()
    }

    pub fn is_within_complexity_ceiling(&self) -> bool {
        let budget = &self.snapshot.budget;
        budget.total_cost_micros <= budget.mission.max_cost_micros
            && budget.total_model_calls <= budget.mission.max_model_calls
            && budget.elapsed <= budget.mission.max_duration
    }

    pub fn all_target_diffs_preserved(&self) -> bool {
        self.snapshot
            .graph
            .nodes
            .iter()
            .filter(|node| node.required && node.kind.is_mutation())
            .filter_map(|node| node.target.as_ref())
            .all(|target| {
                self.snapshot
                    .current_repository
                    .contains_changed_path(&target.path)
            })
    }

    pub fn decision_names(&self) -> Vec<&'static str> {
        self.decisions
            .iter()
            .map(|decision| match decision {
                ExecutionDecision::ContinueDiscovery { .. } => "continue_discovery",
                ExecutionDecision::BuildPlan => "build_plan",
                ExecutionDecision::RepairPlan { .. } => "repair_plan",
                ExecutionDecision::ExecuteTarget { .. } => "execute_target",
                ExecutionDecision::RepairTarget { .. } => "repair_target",
                ExecutionDecision::RunValidation { .. } => "run_validation",
                ExecutionDecision::ReviewDiff { .. } => "review_diff",
                ExecutionDecision::EvaluateCompletion { .. } => "evaluate_completion",
                ExecutionDecision::Publish { .. } => "publish",
                ExecutionDecision::Finish { .. } => "finish",
                ExecutionDecision::StopForGuardrail { .. } => "stop_for_guardrail",
            })
            .collect()
    }

    pub fn has_only_legal_adjacent_transitions(&self) -> bool {
        self.phase_trace
            .windows(2)
            .all(|pair| pair[0].permits_transition_to(pair[1]))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulationError {
    pub code: &'static str,
    pub message: String,
}

impl SimulationError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for SimulationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SimulationError {}

impl From<GraphInvariantError> for SimulationError {
    fn from(error: GraphInvariantError) -> Self {
        Self::new("graph_invariant", error.to_string())
    }
}

impl From<OrchestrationInvariantError> for SimulationError {
    fn from(error: OrchestrationInvariantError) -> Self {
        Self::new("orchestration_invariant", error.to_string())
    }
}

/// Executes scripted fake effects around the production pure reconciler.
pub struct SimulationHarness {
    mission: ScriptedMission,
    snapshot: ExecutionSnapshot,
    actions: VecDeque<ScriptedAction>,
    decisions: Vec<ExecutionDecision>,
    target_results: Vec<TargetResultRecord>,
    validation_runs: Vec<ValidationRunRecord>,
    suppressed_validation_runs: u32,
    pending_validation_suppressions: Vec<ExecutionNodeId>,
    preparation_read_failures: u32,
    scripted_model_outputs_consumed: u32,
    phase_trace: Vec<SimulationPhase>,
    mutation_revision: u64,
    steps: usize,
}

impl SimulationHarness {
    pub fn new(mission: ScriptedMission) -> Self {
        let budget = MissionBudget::for_complexity(mission.complexity);
        let graph_id = format!("simulation-{}", stable_label(&mission.name));
        let graph = ExecutionGraph::bootstrap(
            graph_id,
            mission.initial_repository_fingerprint.clone(),
            mission.complexity,
            &budget,
        );
        let snapshot = ExecutionSnapshot {
            run_id: format!("simulation-run-{}", stable_label(&mission.name)),
            current_repository: RepositorySnapshot {
                fingerprint: mission.initial_repository_fingerprint.clone(),
                source_tree_hash: mission.initial_repository_fingerprint.clone(),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(budget),
            ..ExecutionSnapshot::default()
        };
        Self {
            actions: mission.actions.clone().into(),
            mission,
            snapshot,
            decisions: Vec::new(),
            target_results: Vec::new(),
            validation_runs: Vec::new(),
            suppressed_validation_runs: 0,
            pending_validation_suppressions: Vec::new(),
            preparation_read_failures: 0,
            scripted_model_outputs_consumed: 0,
            phase_trace: Vec::new(),
            mutation_revision: 0,
            steps: 0,
        }
    }

    pub fn snapshot(&self) -> &ExecutionSnapshot {
        &self.snapshot
    }

    pub fn run(mut self) -> Result<SimulationReport, SimulationError> {
        loop {
            self.process_ready_control_actions()?;
            if self.steps >= self.mission.max_steps {
                return Err(SimulationError::new(
                    "maximum_steps_exceeded",
                    format!(
                        "mission `{}` did not terminate within {} decisions",
                        self.mission.name, self.mission.max_steps
                    ),
                ));
            }

            let decision = reconcile_execution(&self.snapshot)?;
            self.verify_validation_suppressions(&decision)?;
            self.record_phase(&decision)?;
            self.steps = self.steps.saturating_add(1);
            self.decisions.push(decision.clone());
            if let Some(outcome) = self.apply_decision(decision)? {
                self.snapshot.validate_invariants()?;
                if !self.actions.is_empty() {
                    return Err(SimulationError::new(
                        "unconsumed_script_actions",
                        format!(
                            "{} scripted action(s) were not exercised",
                            self.actions.len()
                        ),
                    ));
                }
                return Ok(SimulationReport {
                    mission_name: self.mission.name,
                    outcome,
                    snapshot: self.snapshot,
                    decisions: self.decisions,
                    target_results: self.target_results,
                    validation_runs: self.validation_runs,
                    suppressed_validation_runs: self.suppressed_validation_runs,
                    preparation_read_failures: self.preparation_read_failures,
                    scripted_model_outputs_consumed: self.scripted_model_outputs_consumed,
                    phase_trace: self.phase_trace,
                    steps: self.steps,
                });
            }
        }
    }

    fn apply_decision(
        &mut self,
        decision: ExecutionDecision,
    ) -> Result<Option<MissionOutcome>, SimulationError> {
        match decision {
            ExecutionDecision::ContinueDiscovery { .. } => {
                self.simulate_discovery()?;
                Ok(None)
            }
            ExecutionDecision::BuildPlan => {
                self.simulate_planning()?;
                Ok(None)
            }
            ExecutionDecision::RepairPlan { .. } => {
                self.simulate_planning()?;
                Ok(None)
            }
            ExecutionDecision::ExecuteTarget {
                node_id, target, ..
            } => {
                self.simulate_target(node_id, target.target, false)?;
                Ok(None)
            }
            ExecutionDecision::RepairTarget {
                node_id, context, ..
            } => {
                self.simulate_target(node_id, context.target.target, true)?;
                Ok(None)
            }
            ExecutionDecision::RunValidation { node_id, gate } => {
                self.simulate_validation(node_id, gate)?;
                Ok(None)
            }
            ExecutionDecision::ReviewDiff { node_id } => {
                self.simulate_diff_review(node_id)?;
                Ok(None)
            }
            ExecutionDecision::EvaluateCompletion { node_id } => {
                self.simulate_completion(node_id)?;
                Ok(None)
            }
            ExecutionDecision::Publish { mode } => {
                self.simulate_publication(mode)?;
                Ok(None)
            }
            ExecutionDecision::Finish { outcome } => {
                self.finish(outcome)?;
                Ok(Some(outcome))
            }
            ExecutionDecision::StopForGuardrail { outcome, reason } => {
                self.append(ExecutionDomainEvent::GuardrailTriggered {
                    sequence: self.sequence(),
                    reason,
                    outcome,
                    detail: "scripted deterministic guardrail".to_owned(),
                })?;
                if outcome == MissionOutcome::PartialReviewable {
                    return Ok(None);
                }
                self.finish(outcome)?;
                Ok(Some(outcome))
            }
        }
    }

    fn simulate_discovery(&mut self) -> Result<(), SimulationError> {
        let node_id = self.node_id(ExecutionNodeKind::Discovery)?;
        self.charge_model_call(&node_id)?;
        self.append(ExecutionDomainEvent::DiscoveryStarted {
            sequence: self.sequence(),
        })?;
        let result = self
            .take_discovery_result()
            .unwrap_or(ScriptedDiscoveryResult::Completed);
        if let ScriptedDiscoveryResult::RecoverableFailure { message } = result {
            self.record_recoverable_model_failure(
                node_id,
                FailureCategory::ModelArtifactRecoverable,
                message,
            )?;
            return Ok(());
        }
        self.recover_failures_for_node(&node_id)?;
        let fingerprint = self.snapshot.current_repository.fingerprint.clone();
        let evidence = crate::execution_graph::FileEvidence::capture(
            "SIMULATED_REPOSITORY",
            fingerprint.clone(),
            None,
            "deterministic repository evidence",
            false,
        );
        let evidence_id = evidence.evidence_id.clone();
        self.append(ExecutionDomainEvent::RepositoryEvidenceRecorded {
            sequence: self.sequence(),
            evidence_id,
            repository_fingerprint: fingerprint.clone(),
            evidence: Some(evidence),
        })?;
        self.append(ExecutionDomainEvent::DiscoveryCompleted {
            sequence: self.sequence(),
            repository_fingerprint: fingerprint,
        })?;
        Ok(())
    }

    fn simulate_planning(&mut self) -> Result<(), SimulationError> {
        let node_id = self.node_id(ExecutionNodeKind::Planning)?;
        self.charge_model_call(&node_id)?;
        self.start_node(&node_id)?;
        let result = self
            .take_planning_result()
            .unwrap_or(ScriptedPlanningResult::Accepted);
        if let ScriptedPlanningResult::RecoverableFailure { message } = result {
            self.record_recoverable_model_failure(
                node_id,
                FailureCategory::ModelArtifactRecoverable,
                message,
            )?;
            return Ok(());
        }

        self.recover_failures_for_node(&node_id)?;
        self.validate_complete_plan_coverage()?;
        self.append(ExecutionDomainEvent::PlanAccepted {
            sequence: self.sequence(),
            target_count: u32::try_from(self.mission.targets.len()).unwrap_or(u32::MAX),
        })?;
        self.install_accepted_plan_graph()?;
        Ok(())
    }

    fn install_accepted_plan_graph(&mut self) -> Result<(), SimulationError> {
        let graph_id = self.snapshot.graph.graph_id.clone();
        let old_graph = self.snapshot.graph.clone();
        let mut graph = ExecutionGraph::from_targets(
            graph_id.clone(),
            self.mission.complexity,
            self.snapshot.current_repository.fingerprint.clone(),
            &self.mission.targets,
            &self.mission.validation_gates,
            &self.snapshot.budget.mission,
        );
        for kind in [ExecutionNodeKind::Discovery, ExecutionNodeKind::Planning] {
            let Some(previous) = old_graph.nodes.iter().find(|node| node.kind == kind) else {
                continue;
            };
            if let Some(materialized) = graph.nodes.iter_mut().find(|node| node.kind == kind) {
                materialized.attempts = previous.attempts.clone();
                materialized.evidence_ids = previous.evidence_ids.clone();
            }
        }
        graph.revision = old_graph.revision.saturating_add(1);
        let preserved_node_ids = graph
            .nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.kind,
                    ExecutionNodeKind::Discovery | ExecutionNodeKind::Planning
                )
            })
            .map(|node| node.id.clone())
            .collect();
        self.snapshot.graph = graph;
        self.append(ExecutionDomainEvent::GraphCreated {
            sequence: self.sequence(),
            graph_id,
            revision: self.snapshot.graph.revision,
            graph: Some(self.snapshot.graph.clone()),
            preserved_node_ids,
        })?;
        Ok(())
    }

    fn validate_complete_plan_coverage(&self) -> Result<(), SimulationError> {
        let required = if self.mission.required_acceptance_criteria_ids.is_empty() {
            self.mission.covered_acceptance_criteria()
        } else {
            canonical_ids(&self.mission.required_acceptance_criteria_ids)
        };
        let required_set = required
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut covered = std::collections::BTreeSet::new();
        for target in &self.mission.targets {
            let referenced = canonical_ids(&target.acceptance_criteria_ids);
            if referenced.is_empty() {
                return Err(SimulationError::new(
                    "plan_criterion_coverage_invalid",
                    format!("planned target `{}` references no criterion", target.path),
                ));
            }
            for criterion in referenced {
                if !required_set.contains(&criterion) {
                    return Err(SimulationError::new(
                        "plan_criterion_reference_unknown",
                        format!("planned target `{}` references `{criterion}`", target.path),
                    ));
                }
                covered.insert(criterion);
            }
        }
        let missing = required_set
            .difference(&covered)
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(SimulationError::new(
                "plan_criterion_coverage_missing",
                format!("complete plan is missing {}", missing.join(", ")),
            ));
        }
        Ok(())
    }

    fn record_recoverable_model_failure(
        &mut self,
        node_id: ExecutionNodeId,
        category: FailureCategory,
        message: String,
    ) -> Result<(), SimulationError> {
        let failure = FailureRecord::new(
            format!("failure-{:04}", self.sequence()),
            node_id.clone(),
            category,
            1,
            self.snapshot.current_repository.fingerprint.clone(),
            message,
        );
        self.append(ExecutionDomainEvent::FailureRecorded {
            sequence: self.sequence(),
            failure,
        })?;
        Ok(())
    }

    fn recover_failures_for_node(
        &mut self,
        node_id: &ExecutionNodeId,
    ) -> Result<(), SimulationError> {
        let fingerprint = self.snapshot.current_repository.fingerprint.clone();
        let ids = self
            .snapshot
            .failures
            .unresolved_for_node(node_id)
            .map(|failure| failure.id.clone())
            .collect::<Vec<_>>();
        for failure_id in ids {
            self.append(ExecutionDomainEvent::FailureRecovered {
                sequence: self.sequence(),
                node_id: node_id.clone(),
                failure_id,
                repository_fingerprint: fingerprint.clone(),
            })?;
        }
        Ok(())
    }

    fn simulate_target(
        &mut self,
        node_id: ExecutionNodeId,
        target: PlannedTarget,
        repairing: bool,
    ) -> Result<(), SimulationError> {
        if !repairing
            && let Some(MutationResult::AlreadyApplied { .. }) =
                classify_mutation_request(&self.snapshot, &node_id)?
        {
            let fingerprint = self.snapshot.current_repository.fingerprint.clone();
            self.append(ExecutionDomainEvent::NodeCompleted {
                sequence: self.sequence(),
                node_id: node_id.clone(),
                status: ExecutionNodeStatus::Applied,
            })?;
            self.target_results.push(TargetResultRecord {
                node_id,
                path: target.path,
                result: ScriptedTargetResult::AlreadyApplied,
                repository_fingerprint_before: fingerprint.clone(),
                repository_fingerprint_after: fingerprint,
            });
            return Ok(());
        }

        self.charge_model_call(&node_id)?;
        self.start_node(&node_id)?;
        let result = self
            .take_target_result(&target.path)
            .unwrap_or(ScriptedTargetResult::Applied);
        let before = self.snapshot.current_repository.fingerprint.clone();
        match &result {
            ScriptedTargetResult::Applied => {
                self.mutation_revision = self.mutation_revision.saturating_add(1);
                let after = format!("sim-tree-{:04}", self.mutation_revision);
                let evidence_id = format!("mutation-{}-{:04}", node_id, self.mutation_revision);
                self.append(ExecutionDomainEvent::MutationApplied {
                    sequence: self.sequence(),
                    node_id: node_id.clone(),
                    target_path: target.path.clone(),
                    repository_fingerprint: after.clone(),
                    evidence_id,
                })?;
                if repairing {
                    self.reset_failed_validation_nodes()?;
                }
                self.target_results.push(TargetResultRecord {
                    node_id,
                    path: target.path,
                    result,
                    repository_fingerprint_before: before,
                    repository_fingerprint_after: after,
                });
            }
            ScriptedTargetResult::AlreadyApplied => {
                if classify_mutation_request(&self.snapshot, &node_id)?.is_none() {
                    return Err(SimulationError::new(
                        "invalid_already_applied",
                        format!(
                            "target `{}` is not present in repository state",
                            target.path
                        ),
                    ));
                }
                self.append(ExecutionDomainEvent::NodeCompleted {
                    sequence: self.sequence(),
                    node_id: node_id.clone(),
                    status: ExecutionNodeStatus::Applied,
                })?;
                self.target_results.push(TargetResultRecord {
                    node_id,
                    path: target.path,
                    result,
                    repository_fingerprint_before: before.clone(),
                    repository_fingerprint_after: before,
                });
            }
            ScriptedTargetResult::RecoverableFailure { message } => {
                self.reject_mutation(
                    node_id.clone(),
                    &target.path,
                    FailureCategory::ToolRecoverable,
                    message,
                )?;
                self.target_results.push(TargetResultRecord {
                    node_id,
                    path: target.path,
                    result,
                    repository_fingerprint_before: before.clone(),
                    repository_fingerprint_after: before,
                });
            }
            ScriptedTargetResult::BlockingFailure { message } => {
                self.reject_mutation(
                    node_id.clone(),
                    &target.path,
                    FailureCategory::TargetBlocked,
                    message,
                )?;
                self.target_results.push(TargetResultRecord {
                    node_id,
                    path: target.path,
                    result,
                    repository_fingerprint_before: before.clone(),
                    repository_fingerprint_after: before,
                });
            }
            ScriptedTargetResult::InfrastructureFailure { message } => {
                self.reject_mutation(
                    node_id.clone(),
                    &target.path,
                    FailureCategory::InfrastructureFailure,
                    message,
                )?;
                self.target_results.push(TargetResultRecord {
                    node_id,
                    path: target.path,
                    result,
                    repository_fingerprint_before: before.clone(),
                    repository_fingerprint_after: before,
                });
            }
        }
        Ok(())
    }

    fn reject_mutation(
        &mut self,
        node_id: ExecutionNodeId,
        path: &str,
        category: FailureCategory,
        message: &str,
    ) -> Result<(), SimulationError> {
        let attempt = self.snapshot.graph.node(&node_id).map_or(1, |node| {
            u32::try_from(node.attempts.len()).unwrap_or(u32::MAX)
        });
        let failure_id = FailureId::new(format!("failure-{:04}", self.sequence()));
        let mut failure = FailureRecord::new(
            failure_id,
            node_id.clone(),
            category,
            attempt,
            self.snapshot.current_repository.fingerprint.clone(),
            message,
        );
        failure.target_path = Some(path.to_owned());
        self.append(ExecutionDomainEvent::MutationRejected {
            sequence: self.sequence(),
            node_id,
            failure,
        })?;
        Ok(())
    }

    fn simulate_validation(
        &mut self,
        node_id: ExecutionNodeId,
        gate: ValidationGateSpec,
    ) -> Result<(), SimulationError> {
        let fingerprint = gate.fingerprint(&self.snapshot.current_repository.fingerprint);
        if self.snapshot.evidence.has_passed_validation(&fingerprint) {
            self.suppressed_validation_runs = self.suppressed_validation_runs.saturating_add(1);
            self.append(ExecutionDomainEvent::NodeCompleted {
                sequence: self.sequence(),
                node_id,
                status: ExecutionNodeStatus::Passed,
            })?;
            return Ok(());
        }

        self.append(ExecutionDomainEvent::ValidationStarted {
            sequence: self.sequence(),
            node_id: node_id.clone(),
            fingerprint: fingerprint.clone(),
        })?;
        let result = self
            .take_validation_result(&gate.gate_id)
            .unwrap_or(ScriptedValidationResult::Passed);
        self.validation_runs.push(ValidationRunRecord {
            node_id: node_id.clone(),
            gate_id: gate.gate_id.clone(),
            fingerprint: fingerprint.clone(),
            result: result.clone(),
        });
        let evidence_id = format!("validation-{}-{:04}", node_id, self.validation_runs.len());
        let status = match result {
            ScriptedValidationResult::Passed => ValidationEvidenceStatus::Passed,
            ScriptedValidationResult::RecoverableFailure { .. } => ValidationEvidenceStatus::Failed,
            ScriptedValidationResult::InfrastructureFailure { .. } => {
                ValidationEvidenceStatus::TimedOut
            }
        };
        let evidence = ValidationEvidenceRecord {
            evidence_id: evidence_id.clone(),
            node_id: node_id.clone(),
            gate_id: gate.gate_id.clone(),
            fingerprint: fingerprint.clone(),
            repository_fingerprint: self.snapshot.current_repository.fingerprint.clone(),
            command: gate.command,
            working_directory: gate.working_directory,
            status,
            exit_code: (status == ValidationEvidenceStatus::Passed).then_some(0),
            output_summary: if status == ValidationEvidenceStatus::Passed {
                "scripted validation passed".to_owned()
            } else {
                "scripted validation failed".to_owned()
            },
            duration: Duration::from_secs(1),
        };
        self.append(ExecutionDomainEvent::ValidationEvidenceRecorded {
            sequence: self.sequence(),
            node_id: node_id.clone(),
            evidence,
        })?;

        match result {
            ScriptedValidationResult::Passed => {
                self.append(ExecutionDomainEvent::ValidationPassed {
                    sequence: self.sequence(),
                    node_id,
                    evidence_id,
                    fingerprint,
                })?;
            }
            ScriptedValidationResult::RecoverableFailure { message } => {
                self.fail_validation(
                    node_id,
                    evidence_id,
                    fingerprint,
                    FailureCategory::ValidationFailure,
                    message,
                )?;
            }
            ScriptedValidationResult::InfrastructureFailure { message } => {
                self.fail_validation(
                    node_id,
                    evidence_id,
                    fingerprint,
                    FailureCategory::InfrastructureFailure,
                    message,
                )?;
            }
        }
        Ok(())
    }

    fn fail_validation(
        &mut self,
        node_id: ExecutionNodeId,
        _evidence_id: String,
        fingerprint: String,
        category: FailureCategory,
        message: String,
    ) -> Result<(), SimulationError> {
        let failure_id = FailureId::new(format!("failure-{:04}", self.sequence()));
        let mut failure = FailureRecord::new(
            failure_id.clone(),
            node_id.clone(),
            category,
            1,
            self.snapshot.current_repository.fingerprint.clone(),
            message,
        );
        failure.target_path = self
            .snapshot
            .graph
            .nodes
            .iter()
            .rev()
            .find(|node| node.kind.is_mutation() && node.status.is_success())
            .and_then(|node| node.target.as_ref())
            .map(|target| target.path.clone());
        self.append(ExecutionDomainEvent::FailureRecorded {
            sequence: self.sequence(),
            failure,
        })?;
        self.append(ExecutionDomainEvent::ValidationFailed {
            sequence: self.sequence(),
            node_id,
            failure_id,
            fingerprint,
        })?;
        Ok(())
    }

    fn reset_failed_validation_nodes(&mut self) -> Result<(), SimulationError> {
        let failed = self
            .snapshot
            .graph
            .nodes
            .iter()
            .filter(|node| {
                node.kind.is_validation() && node.status == ExecutionNodeStatus::FailedRecoverable
            })
            .filter_map(|node| {
                self.snapshot
                    .evidence
                    .validations
                    .values()
                    .find(|evidence| {
                        evidence.node_id == node.id
                            && evidence.status == ValidationEvidenceStatus::Failed
                    })
                    .map(|evidence| (node.id.clone(), evidence.evidence_id.clone()))
            })
            .collect::<Vec<_>>();
        for (node_id, evidence_id) in failed {
            self.append(ExecutionDomainEvent::ValidationSuperseded {
                sequence: self.sequence(),
                node_id: node_id.clone(),
                evidence_id,
                repository_fingerprint: self.snapshot.current_repository.fingerprint.clone(),
            })?;
            let fingerprint = self.snapshot.current_repository.fingerprint.clone();
            let failure_ids = self
                .snapshot
                .failures
                .unresolved_for_node(&node_id)
                .filter(|failure| failure.category == FailureCategory::ValidationFailure)
                .map(|failure| failure.id.clone())
                .collect::<Vec<_>>();
            for failure_id in failure_ids {
                self.append(ExecutionDomainEvent::FailureRecovered {
                    sequence: self.sequence(),
                    node_id: node_id.clone(),
                    failure_id,
                    repository_fingerprint: fingerprint.clone(),
                })?;
            }
        }
        Ok(())
    }

    fn simulate_diff_review(&mut self, node_id: ExecutionNodeId) -> Result<(), SimulationError> {
        self.charge_model_call(&node_id)?;
        self.start_node(&node_id)?;
        let evidence_id = format!("diff-review-{:04}", self.sequence());
        self.snapshot.evidence.record(EvidenceRecord {
            evidence_id: evidence_id.clone(),
            kind: EvidenceKind::DiffReview,
            node_id: Some(node_id.clone()),
            repository_fingerprint: self.snapshot.current_repository.fingerprint.clone(),
            summary: format!(
                "reviewed {} changed paths",
                self.snapshot.current_repository.changed_paths.len()
            ),
        });
        self.append(ExecutionDomainEvent::DiffReviewed {
            sequence: self.sequence(),
            node_id,
            evidence_ids: vec![evidence_id],
        })?;
        Ok(())
    }

    fn simulate_completion(&mut self, node_id: ExecutionNodeId) -> Result<(), SimulationError> {
        self.charge_model_call(&node_id)?;
        self.start_node(&node_id)?;
        let outcome = if self.snapshot.has_partial_reviewable_guardrail() {
            MissionOutcome::PartialReviewable
        } else if self.mission.completion_outcome == MissionOutcome::Complete
            && !self.snapshot.current_repository.has_changes()
        {
            MissionOutcome::BlockedNoDiff
        } else {
            self.mission.completion_outcome
        };
        self.append(ExecutionDomainEvent::CompletionEvaluated {
            sequence: self.sequence(),
            node_id,
            outcome,
        })?;
        Ok(())
    }

    fn simulate_publication(&mut self, mode: PublicationMode) -> Result<(), SimulationError> {
        let node_id = self.node_id(ExecutionNodeKind::Publication)?;
        self.append(ExecutionDomainEvent::PublicationStarted {
            sequence: self.sequence(),
            node_id: node_id.clone(),
            mode,
        })?;
        let suffix = stable_label(&self.mission.name);
        self.append(ExecutionDomainEvent::CommitCreated {
            sequence: self.sequence(),
            node_id: node_id.clone(),
            commit_sha: format!("simulated-commit-{suffix}"),
        })?;
        self.append(ExecutionDomainEvent::BranchPushed {
            sequence: self.sequence(),
            node_id: node_id.clone(),
            branch: format!("simulation/{suffix}"),
        })?;
        self.append(ExecutionDomainEvent::PullRequestCreated {
            sequence: self.sequence(),
            node_id,
            url: format!("https://example.test/{suffix}/pull/1"),
            number: Some(1),
            draft: matches!(
                mode,
                PublicationMode::Draft | PublicationMode::DraftRecovery
            ),
        })?;
        Ok(())
    }

    fn finish(&mut self, outcome: MissionOutcome) -> Result<(), SimulationError> {
        if !self.snapshot.is_terminal() {
            self.append(ExecutionDomainEvent::RunFinished {
                sequence: self.sequence(),
                outcome,
            })?;
        }
        Ok(())
    }

    fn process_ready_control_actions(&mut self) -> Result<(), SimulationError> {
        loop {
            let Some(action) = self.actions.front().cloned() else {
                return Ok(());
            };
            match action {
                ScriptedAction::PreparationReadFailure { .. }
                | ScriptedAction::DiscoveryOutput { .. }
                | ScriptedAction::PlanningOutput { .. } => return Ok(()),
                ScriptedAction::DuplicateMutation { path } => {
                    let Some(node_id) = self.target_node_id(&path) else {
                        return Err(SimulationError::new(
                            "unknown_script_target",
                            format!("duplicate action refers to unknown target `{path}`"),
                        ));
                    };
                    if !self
                        .snapshot
                        .current_repository
                        .contains_changed_path(&path)
                    {
                        return Ok(());
                    }
                    let Some(MutationResult::AlreadyApplied { .. }) =
                        classify_mutation_request(&self.snapshot, &node_id)?
                    else {
                        return Err(SimulationError::new(
                            "duplicate_not_classified",
                            format!("duplicate target `{path}` was not already applied"),
                        ));
                    };
                    self.actions.pop_front();
                    let fingerprint = self.snapshot.current_repository.fingerprint.clone();
                    self.target_results.push(TargetResultRecord {
                        node_id,
                        path,
                        result: ScriptedTargetResult::AlreadyApplied,
                        repository_fingerprint_before: fingerprint.clone(),
                        repository_fingerprint_after: fingerprint,
                    });
                }
                ScriptedAction::RepeatValidation { gate_id } => {
                    if !self.snapshot.graph.nodes.iter().any(|node| {
                        node.validation
                            .as_ref()
                            .is_some_and(|gate| gate.gate_id == gate_id)
                    }) {
                        return Ok(());
                    }
                    let (node_id, gate) = self.validation_node(&gate_id)?;
                    let fingerprint =
                        gate.fingerprint(&self.snapshot.current_repository.fingerprint);
                    if !self.snapshot.evidence.has_passed_validation(&fingerprint) {
                        return Ok(());
                    }
                    self.actions.pop_front();
                    self.append(ExecutionDomainEvent::NodeCompleted {
                        sequence: self.sequence(),
                        node_id: node_id.clone(),
                        status: ExecutionNodeStatus::Ready,
                    })?;
                    self.pending_validation_suppressions.push(node_id);
                }
                ScriptedAction::SeedPassedValidation { gate_id } => {
                    self.actions.pop_front();
                    self.seed_passed_validation(&gate_id)?;
                }
                ScriptedAction::ExhaustTargetBudget { path } => {
                    let Some(node_id) = self.target_node_id(&path) else {
                        return Err(SimulationError::new(
                            "unknown_script_target",
                            format!("budget action refers to unknown target `{path}`"),
                        ));
                    };
                    self.actions.pop_front();
                    let max_calls = self
                        .snapshot
                        .graph
                        .node(&node_id)
                        .map_or(1, |node| node.budget.max_model_calls.max(1));
                    let used = self
                        .snapshot
                        .budget
                        .usage_for(&node_id)
                        .model_calls_consumed;
                    for _ in used..max_calls {
                        self.snapshot
                            .budget
                            .record_model_call(node_id.clone(), 0, Duration::ZERO);
                    }
                }
                ScriptedAction::TargetResult { .. } | ScriptedAction::ValidationResult { .. } => {
                    return Ok(());
                }
            }
        }
    }

    fn seed_passed_validation(&mut self, gate_id: &str) -> Result<(), SimulationError> {
        let (node_id, gate) = self.validation_node(gate_id)?;
        let fingerprint = gate.fingerprint(&self.snapshot.current_repository.fingerprint);
        if self.snapshot.evidence.has_passed_validation(&fingerprint) {
            self.suppressed_validation_runs = self.suppressed_validation_runs.saturating_add(1);
            return Ok(());
        }
        let evidence_id = format!("seeded-validation-{node_id}");
        let evidence = ValidationEvidenceRecord {
            evidence_id: evidence_id.clone(),
            node_id: node_id.clone(),
            gate_id: gate.gate_id.clone(),
            fingerprint: fingerprint.clone(),
            repository_fingerprint: self.snapshot.current_repository.fingerprint.clone(),
            command: gate.command.clone(),
            working_directory: gate.working_directory.clone(),
            status: ValidationEvidenceStatus::Passed,
            exit_code: Some(0),
            output_summary: "seeded deterministic passing validation".to_owned(),
            duration: Duration::from_secs(1),
        };
        self.append(ExecutionDomainEvent::ValidationEvidenceRecorded {
            sequence: self.sequence(),
            node_id: node_id.clone(),
            evidence,
        })?;
        self.validation_runs.push(ValidationRunRecord {
            node_id: node_id.clone(),
            gate_id: gate.gate_id,
            fingerprint: fingerprint.clone(),
            result: ScriptedValidationResult::Passed,
        });
        Ok(())
    }

    fn take_target_result(&mut self, path: &str) -> Option<ScriptedTargetResult> {
        match self.actions.front() {
            Some(ScriptedAction::TargetResult {
                path: scripted_path,
                ..
            }) if scripted_path == path => match self.actions.pop_front() {
                Some(ScriptedAction::TargetResult { result, .. }) => Some(result),
                _ => unreachable!("front action was a target result"),
            },
            Some(ScriptedAction::PreparationReadFailure { .. }) => match self.actions.pop_front() {
                Some(ScriptedAction::PreparationReadFailure { message }) => {
                    self.preparation_read_failures =
                        self.preparation_read_failures.saturating_add(1);
                    Some(ScriptedTargetResult::RecoverableFailure { message })
                }
                _ => unreachable!("front action was a preparation read failure"),
            },
            _ => None,
        }
    }

    fn take_discovery_result(&mut self) -> Option<ScriptedDiscoveryResult> {
        match self.actions.front() {
            Some(ScriptedAction::DiscoveryOutput { .. }) => match self.actions.pop_front() {
                Some(ScriptedAction::DiscoveryOutput { result }) => {
                    self.scripted_model_outputs_consumed =
                        self.scripted_model_outputs_consumed.saturating_add(1);
                    Some(result)
                }
                _ => unreachable!("front action was a discovery output"),
            },
            _ => None,
        }
    }

    fn take_planning_result(&mut self) -> Option<ScriptedPlanningResult> {
        match self.actions.front() {
            Some(ScriptedAction::PlanningOutput { .. }) => match self.actions.pop_front() {
                Some(ScriptedAction::PlanningOutput { result }) => {
                    self.scripted_model_outputs_consumed =
                        self.scripted_model_outputs_consumed.saturating_add(1);
                    Some(result)
                }
                _ => unreachable!("front action was a planning output"),
            },
            _ => None,
        }
    }

    fn take_validation_result(&mut self, gate_id: &str) -> Option<ScriptedValidationResult> {
        match self.actions.front() {
            Some(ScriptedAction::ValidationResult {
                gate_id: scripted_gate,
                ..
            }) if scripted_gate == gate_id => match self.actions.pop_front() {
                Some(ScriptedAction::ValidationResult { result, .. }) => Some(result),
                _ => unreachable!("front action was a validation result"),
            },
            _ => None,
        }
    }

    fn verify_validation_suppressions(
        &mut self,
        decision: &ExecutionDecision,
    ) -> Result<(), SimulationError> {
        if self.pending_validation_suppressions.is_empty() {
            return Ok(());
        }
        if let ExecutionDecision::RunValidation { node_id, .. } = decision
            && self.pending_validation_suppressions.contains(node_id)
        {
            return Err(SimulationError::new(
                "duplicate_validation_not_suppressed",
                format!("reconciler reran validation node `{node_id}` for the same fingerprint"),
            ));
        }
        self.suppressed_validation_runs = self.suppressed_validation_runs.saturating_add(
            u32::try_from(self.pending_validation_suppressions.len()).unwrap_or(u32::MAX),
        );
        self.pending_validation_suppressions.clear();
        Ok(())
    }

    fn record_phase(&mut self, decision: &ExecutionDecision) -> Result<(), SimulationError> {
        let phase = SimulationPhase::for_decision(decision);
        if let Some(previous) = self.phase_trace.last().copied()
            && !previous.permits_transition_to(phase)
        {
            return Err(SimulationError::new(
                "illegal_simulated_lifecycle_transition",
                format!("illegal adjacent transition from `{previous:?}` to `{phase:?}`"),
            ));
        }
        self.phase_trace.push(phase);
        Ok(())
    }

    fn start_node(&mut self, node_id: &ExecutionNodeId) -> Result<(), SimulationError> {
        let attempt = self.snapshot.graph.node(node_id).map_or(1, |node| {
            u32::try_from(node.attempts.len())
                .unwrap_or(u32::MAX)
                .saturating_add(1)
        });
        self.append(ExecutionDomainEvent::NodeStarted {
            sequence: self.sequence(),
            node_id: node_id.clone(),
            attempt,
            started_at: format!("simulation-step-{:04}", self.steps),
            repository_fingerprint: self.snapshot.current_repository.fingerprint.clone(),
        })?;
        Ok(())
    }

    fn charge_model_call(&mut self, node_id: &ExecutionNodeId) -> Result<(), SimulationError> {
        let node = self.snapshot.graph.node(node_id).ok_or_else(|| {
            SimulationError::new(
                "unknown_model_call_node",
                format!("model call refers to unknown node `{node_id}`"),
            )
        })?;
        if !self.snapshot.budget.can_spend_model_call(
            node_id,
            &node.budget,
            self.mission.model_call_cost_micros,
            self.mission.model_call_duration,
        ) {
            return Err(SimulationError::new(
                "model_call_budget_denied",
                format!("pre-dispatch budget denied model call for node `{node_id}`"),
            ));
        }
        self.snapshot.budget.record_model_call(
            node_id.clone(),
            self.mission.model_call_cost_micros,
            self.mission.model_call_duration,
        );
        Ok(())
    }

    fn append(&mut self, event: ExecutionDomainEvent) -> Result<(), SimulationError> {
        self.snapshot.append_event(event)?;
        Ok(())
    }

    fn sequence(&self) -> u64 {
        self.snapshot.next_event_sequence()
    }

    fn node_id(&self, kind: ExecutionNodeKind) -> Result<ExecutionNodeId, SimulationError> {
        self.snapshot
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == kind)
            .map(|node| node.id.clone())
            .ok_or_else(|| {
                SimulationError::new(
                    "missing_graph_node",
                    format!("simulation graph has no {kind:?} node"),
                )
            })
    }

    fn target_node_id(&self, path: &str) -> Option<ExecutionNodeId> {
        self.snapshot
            .graph
            .nodes
            .iter()
            .find(|node| {
                node.kind.is_mutation()
                    && node
                        .target
                        .as_ref()
                        .is_some_and(|target| target.path == path)
            })
            .map(|node| node.id.clone())
    }

    fn validation_node(
        &self,
        gate_id: &str,
    ) -> Result<(ExecutionNodeId, ValidationGateSpec), SimulationError> {
        self.snapshot
            .graph
            .nodes
            .iter()
            .find_map(|node| {
                node.validation
                    .as_ref()
                    .filter(|gate| gate.gate_id == gate_id)
                    .map(|gate| (node.id.clone(), gate.clone()))
            })
            .ok_or_else(|| {
                SimulationError::new(
                    "unknown_validation_gate",
                    format!("simulation graph has no validation gate `{gate_id}`"),
                )
            })
    }
}

fn stable_label(value: &str) -> String {
    let label = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let label = label.trim_matches('-');
    if label.is_empty() {
        "mission".to_owned()
    } else {
        label.to_owned()
    }
}

fn canonical_ids(values: &[String]) -> Vec<String> {
    let mut values = values
        .iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_next_decision(harness: &mut SimulationHarness) -> ExecutionDecision {
        harness
            .process_ready_control_actions()
            .expect("process scripted control actions");
        let decision = reconcile_execution(&harness.snapshot).expect("reconcile simulation");
        harness.steps = harness.steps.saturating_add(1);
        harness.decisions.push(decision.clone());
        assert!(
            harness
                .apply_decision(decision.clone())
                .expect("apply simulation decision")
                .is_none(),
            "checkpoint fixture must not terminate early"
        );
        decision
    }

    #[test]
    fn validation_repair_checkpoint_replays_graph_and_failures_from_domain_events() {
        let mission =
            ScriptedMission::new("validation repair event replay", MissionComplexity::Small)
                .with_target(PlannedTarget {
                    change_id: "parser-fix".to_owned(),
                    path: "src/parser.rs".to_owned(),
                    role: "production".to_owned(),
                    intent: "repair parser behavior".to_owned(),
                    acceptance_criteria_ids: vec!["ac-1".to_owned()],
                    new_file: false,
                })
                .with_validation_gate(ValidationGateSpec {
                    gate_id: "parser-tests".to_owned(),
                    gate_type: ValidationGateType::TestSuite,
                    command: "cargo test parser".to_owned(),
                    working_directory: String::new(),
                    required: true,
                    dependency_lock_hash: "lock-v1".to_owned(),
                    relevant_environment_fingerprint: "rust-stable".to_owned(),
                })
                .with_action(ScriptedAction::ValidationResult {
                    gate_id: "parser-tests".to_owned(),
                    result: ScriptedValidationResult::RecoverableFailure {
                        message: "focused parser assertion failed".to_owned(),
                    },
                });
        let mut harness = SimulationHarness::new(mission);

        let checkpoint = loop {
            harness
                .process_ready_control_actions()
                .expect("process scripted control actions");
            let decision =
                reconcile_execution(&harness.snapshot).expect("reconcile before validation");
            if matches!(decision, ExecutionDecision::RunValidation { .. }) {
                break harness.snapshot.clone();
            }
            harness.steps = harness.steps.saturating_add(1);
            harness.decisions.push(decision.clone());
            assert!(
                harness
                    .apply_decision(decision)
                    .expect("advance to validation")
                    .is_none()
            );
        };

        assert!(matches!(
            apply_next_decision(&mut harness),
            ExecutionDecision::RunValidation { .. }
        ));
        assert!(matches!(
            apply_next_decision(&mut harness),
            ExecutionDecision::RepairTarget { .. }
        ));
        let persisted = harness.snapshot.clone();
        let suffix = persisted.events[checkpoint.events.len()..].to_vec();

        assert!(suffix.iter().any(|event| matches!(
            event,
            ExecutionDomainEvent::FailureRecorded { failure, .. }
                if failure.category == FailureCategory::ValidationFailure
        )));
        assert!(
            suffix
                .iter()
                .any(|event| matches!(event, ExecutionDomainEvent::FailureRecovered { .. }))
        );
        assert_eq!(persisted.failures.unresolved().count(), 0);

        let encoded = serde_json::to_string(&suffix).expect("serialize domain event suffix");
        let replay_events: Vec<ExecutionDomainEvent> =
            serde_json::from_str(&encoded).expect("deserialize domain event suffix");
        let mut replayed = checkpoint;
        for event in replay_events {
            replayed.append_event(event).expect("replay domain event");
        }

        assert_eq!(replayed.graph, persisted.graph);
        assert_eq!(replayed.failures, persisted.failures);
        assert_eq!(replayed.events, persisted.events);
    }
}
