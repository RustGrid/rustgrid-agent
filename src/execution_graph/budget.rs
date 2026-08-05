#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct NodeBudgetUsage {
    #[serde(default)]
    pub model_calls_reserved: u32,
    #[serde(default, alias = "model_calls")]
    pub model_calls_consumed: u32,
    pub cost_micros: u64,
    #[serde(default)]
    pub cost_micros_reserved: u64,
    #[serde(with = "duration_millis")]
    pub duration: Duration,
    pub repair_attempts: u32,
    #[serde(default)]
    pub validation_repair_attempts: u32,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ModelCallBreakdown {
    pub initial_target_mutation_calls: u32,
    pub target_mutation_repair_calls: u32,
    pub validation_diagnosis_calls: u32,
    pub validation_repair_mutation_calls: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelCallPurpose {
    InitialTargetMutation,
    TargetMutationRepair,
    ValidationDiagnosis,
    ValidationRepairMutation,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ProgressWindow {
    pub max_model_calls_without_progress: u32,
    #[serde(with = "duration_millis")]
    pub max_duration_without_progress: Duration,
}

impl Default for ProgressWindow {
    fn default() -> Self {
        Self {
            max_model_calls_without_progress: 3,
            max_duration_without_progress: Duration::from_secs(3 * 60),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEventKind {
    NewRelevantEvidenceRecorded,
    PlanAccepted,
    NodeMadeReady,
    SourceMutationApplied,
    TestMutationApplied,
    FailureSuperseded,
    FailureRepaired,
    ValidationPassed,
    DiffReviewed,
    CriterionEvidenced,
    CommitCreated,
    PullRequestCreated,
}

impl ProgressEventKind {
    pub const fn score(self) -> u32 {
        match self {
            Self::NewRelevantEvidenceRecorded | Self::NodeMadeReady => 1,
            Self::FailureSuperseded | Self::CriterionEvidenced => 3,
            Self::PlanAccepted | Self::DiffReviewed => 4,
            Self::FailureRepaired | Self::CommitCreated => 5,
            Self::ValidationPassed => 6,
            Self::TestMutationApplied => 7,
            Self::SourceMutationApplied => 8,
            Self::PullRequestCreated => 10,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ProgressEvent {
    pub sequence: u64,
    pub kind: ProgressEventKind,
    pub node_id: Option<ExecutionNodeId>,
    pub model_calls_at_event: u32,
    pub cost_micros_at_event: u64,
    #[serde(with = "duration_millis")]
    pub elapsed_at_event: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCallAdmission {
    pub node_id: ExecutionNodeId,
    pub max_model_calls: u32,
    pub consumed_calls: u32,
    pub reserved_calls: u32,
    pub requested_calls: u32,
    pub admitted: bool,
    pub rejection_reason: Option<&'static str>,
    pub node_cost_used: u64,
    pub node_cost_reserved: u64,
    pub node_cost_limit: u64,
    pub estimated_request_cost: u64,
    pub projected_node_cost: u64,
    pub mission_cost_used: u64,
    pub mission_calls_used: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCallReservation {
    pub node_id: ExecutionNodeId,
    pub estimated_cost_micros: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationRepairReallocation {
    pub session_id: RepairSessionId,
    pub model_calls: u32,
    pub cost_micros: u64,
    pub budget: ValidationRepairBudget,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct BudgetState {
    pub mission: MissionBudget,
    #[serde(default)]
    pub total_model_calls_reserved: u32,
    pub total_model_calls: u32,
    #[serde(default)]
    pub total_cost_micros_reserved: u64,
    pub total_cost_micros: u64,
    #[serde(with = "duration_millis")]
    pub elapsed: Duration,
    #[serde(default)]
    pub node_usage: BTreeMap<ExecutionNodeId, NodeBudgetUsage>,
    /// Deterministic validation work is accounted independently from model
    /// call admission so a command rerun can never consume repair capacity.
    #[serde(default)]
    pub validation_gate_usage: BTreeMap<ExecutionNodeId, ValidationGateBudget>,
    #[serde(default)]
    pub progress_events: Vec<ProgressEvent>,
    #[serde(default)]
    pub model_call_breakdown: ModelCallBreakdown,
    pub progress_score: u64,
    pub progress_window: ProgressWindow,
    /// Model-backed validation repair uses a synthetic, persisted budget
    /// owner rather than borrowing the failed gate's deterministic budget.
    #[serde(default)]
    pub validation_repair_sessions: BTreeMap<RepairSessionId, ValidationRepairSession>,
    #[serde(default)]
    pub validation_failure_revisions:
        BTreeMap<ValidationId, Vec<ValidationFailureRevision>>,
}

impl Default for BudgetState {
    fn default() -> Self {
        Self::new(MissionBudget::default())
    }
}

impl BudgetState {
    pub fn new(mission: MissionBudget) -> Self {
        Self {
            mission,
            total_model_calls_reserved: 0,
            total_model_calls: 0,
            total_cost_micros_reserved: 0,
            total_cost_micros: 0,
            elapsed: Duration::ZERO,
            node_usage: BTreeMap::new(),
            validation_gate_usage: BTreeMap::new(),
            progress_events: Vec::new(),
            model_call_breakdown: ModelCallBreakdown::default(),
            progress_score: 0,
            progress_window: ProgressWindow::default(),
            validation_repair_sessions: BTreeMap::new(),
            validation_failure_revisions: BTreeMap::new(),
        }
    }

    pub fn record_validation_command_run(&mut self, node_id: ExecutionNodeId) {
        let usage = self.validation_gate_usage.entry(node_id).or_default();
        usage.command_runs = usage.command_runs.saturating_add(1);
    }

    pub fn record_validation_parsing_call(&mut self, node_id: ExecutionNodeId) {
        let usage = self.validation_gate_usage.entry(node_id).or_default();
        usage.parsing_calls = usage.parsing_calls.saturating_add(1);
    }

    pub fn record_validation_diagnosis_call(&mut self, node_id: ExecutionNodeId) {
        let usage = self.validation_gate_usage.entry(node_id).or_default();
        usage.diagnosis_calls = usage.diagnosis_calls.saturating_add(1);
    }

    pub fn repair_session_id(failure_id: &FailureId) -> RepairSessionId {
        format!("validation-repair-session:{failure_id}")
    }

    pub fn repair_session_for_failure(
        &self,
        failure_id: &FailureId,
    ) -> Option<&ValidationRepairSession> {
        self.validation_repair_sessions
            .get(&Self::repair_session_id(failure_id))
            .or_else(|| {
                self.validation_repair_sessions
                    .values()
                    .find(|session| session.failed_validation_id == failure_id.to_string())
            })
    }

    pub fn repair_session_for_failure_mut(
        &mut self,
        failure_id: &FailureId,
    ) -> Option<&mut ValidationRepairSession> {
        let direct = Self::repair_session_id(failure_id);
        let key = if self.validation_repair_sessions.contains_key(&direct) {
            direct
        } else {
            self.validation_repair_sessions
                .iter()
                .find(|(_, session)| session.failed_validation_id == failure_id.to_string())
                .map(|(key, _)| key.clone())?
        };
        self.validation_repair_sessions.get_mut(&key)
    }

    pub fn repair_budget_owner(
        &self,
        failure_id: &FailureId,
    ) -> Option<(ExecutionNodeId, NodeBudget)> {
        let session = self.repair_session_for_failure(failure_id)?;
        Some((
            ExecutionNodeId::new(session.session_id.clone()),
            session.budget.as_node_budget(),
        ))
    }

    pub fn create_validation_failure_revision(
        &mut self,
        failure: &FailureRecord,
        sequence: u64,
    ) -> ValidationFailureRevision {
        let revisions = self
            .validation_failure_revisions
            .entry(failure.node_id.to_string())
            .or_default();
        let revision = revisions.last().map_or(1, |prior| prior.revision.saturating_add(1));
        let assertion_ids = failure
            .assertion_failures
            .iter()
            .enumerate()
            .map(|(index, assertion)| {
                format!(
                    "{}:{}:{}:{}",
                    assertion.test_file,
                    assertion.source_line.unwrap_or_default(),
                    assertion.test_name,
                    index
                )
            })
            .collect();
        let created = ValidationFailureRevision {
            validation_id: failure.node_id.to_string(),
            revision,
            repository_fingerprint: RepositoryFingerprint::new(
                failure.repository_fingerprint.clone(),
            ),
            assertion_ids,
            created_at: format!("event-sequence:{sequence}"),
        };
        revisions.push(created.clone());
        created
    }

    pub fn current_validation_failure_revision(
        &self,
        validation_id: &str,
        repository_fingerprint: &str,
    ) -> Option<&ValidationFailureRevision> {
        self.validation_failure_revisions
            .get(validation_id)?
            .iter()
            .rev()
            .find(|revision| {
                revision.repository_fingerprint.as_str() == repository_fingerprint
            })
    }

    pub fn ensure_validation_repair_session(
        &mut self,
        failure: &FailureRecord,
        inputs: ValidationRepairBudgetInputs,
    ) -> Result<&ValidationRepairSession, GraphInvariantError> {
        let session_id = Self::repair_session_id(&failure.id);
        let existing_session_id = self
            .validation_repair_sessions
            .iter()
            .find(|(_, session)| {
                session.originating_gate_id == failure.node_id
                    && session.failed_validation_id == failure.id.to_string()
            })
            .map(|(key, _)| key.clone());
        let session_id = existing_session_id.unwrap_or(session_id);
        if !self.validation_repair_sessions.contains_key(&session_id) {
            let mission_calls_remaining = self
                .mission
                .max_model_calls
                .saturating_sub(self.total_model_calls.saturating_add(self.total_model_calls_reserved));
            let mission_cost_remaining = self
                .mission
                .max_cost_micros
                .saturating_sub(self.total_cost_micros.saturating_add(self.total_cost_micros_reserved));
            let assertion_bound = inputs.failed_assertion_count.max(1);
            let implicated_bound = inputs.implicated_target_count.max(1);
            let policy_bound = self.mission.max_target_repair_rounds.max(1);
            let target_attempts = implicated_bound
                .min(policy_bound)
                .min(if inputs.originating_gate_required {
                    policy_bound
                } else {
                    1
                });
            let diagnosis_calls = 1_u32.saturating_add(assertion_bound.saturating_sub(1) / 4);
            let desired_calls = diagnosis_calls.saturating_add(target_attempts);
            let size_context_rebuilds = u32::try_from(
                inputs
                    .implicated_target_bytes
                    .saturating_add(256 * 1024 - 1)
                    / (256 * 1024),
            )
            .unwrap_or(u32::MAX)
            .max(1)
            .min(target_attempts);
            let budget = ValidationRepairBudget {
                max_model_calls: desired_calls.min(mission_calls_remaining),
                max_target_attempts: target_attempts,
                max_repository_writes: target_attempts,
                max_context_rebuilds: target_attempts.saturating_add(size_context_rebuilds),
                max_cost_micros: mission_cost_remaining,
            };
            budget.validate(target_attempts > 1).map_err(|_| {
                GraphInvariantError::new(
                    "validation repair admission policy is misconfigured or lacks the minimum mission capacity",
                )
            })?;
            let current_revision = self
                .current_validation_failure_revision(
                    failure.node_id.as_str(),
                    &failure.repository_fingerprint,
                )
                .map_or(0, |revision| revision.revision);
            self.validation_repair_sessions.insert(
                session_id.clone(),
                ValidationRepairSession {
                    session_id: session_id.clone(),
                    failed_validation_id: failure.id.to_string(),
                    originating_gate_id: failure.node_id.clone(),
                    budget,
                    status: ValidationRepairSessionStatus::Active,
                    attempted_targets: Vec::new(),
                    current_assertion_set_revision: current_revision,
                    stop_reason: None,
                    reallocated_model_calls: 0,
                    reallocated_cost_micros: 0,
                    repository_writes_consumed: 0,
                    context_rebuilds_consumed: 0,
                    budget_inputs: inputs,
                },
            );
        }
        if let Some(session) = self.validation_repair_sessions.get_mut(&session_id) {
            session.status = ValidationRepairSessionStatus::Active;
            session.stop_reason = None;
        }
        self.validation_repair_sessions
            .get(&session_id)
            .ok_or_else(|| GraphInvariantError::new("validation repair session was not materialized"))
    }

    pub fn record_validation_repair_attempt_for_failure(
        &mut self,
        failure_id: &FailureId,
        mut attempt: ValidationRepairAttempt,
    ) -> Result<(), GraphInvariantError> {
        let session = self.repair_session_for_failure_mut(failure_id).ok_or_else(|| {
            GraphInvariantError::new("validation repair attempt has no materialized session")
        })?;
        if attempt.attempt_number == 0 {
            attempt.attempt_number = u32::try_from(session.attempted_targets.len())
                .unwrap_or(u32::MAX)
                .saturating_add(1);
        }
        if attempt.failure_revision == 0 {
            attempt.failure_revision = session.current_assertion_set_revision;
        }
        session.attempted_targets.push(attempt);
        Ok(())
    }

    pub fn record_validation_repair_context_rebuild(
        &mut self,
        failure_id: &FailureId,
    ) -> Result<(), GraphInvariantError> {
        let session = self.repair_session_for_failure_mut(failure_id).ok_or_else(|| {
            GraphInvariantError::new("validation repair context rebuild has no session")
        })?;
        if session.context_rebuilds_consumed >= session.budget.max_context_rebuilds {
            return Err(GraphInvariantError::new(
                "validation repair context rebuild budget is exhausted",
            ));
        }
        session.context_rebuilds_consumed = session.context_rebuilds_consumed.saturating_add(1);
        Ok(())
    }

    pub fn record_validation_repair_repository_write(
        &mut self,
        failure_id: &FailureId,
    ) -> Result<(), GraphInvariantError> {
        let session = self.repair_session_for_failure_mut(failure_id).ok_or_else(|| {
            GraphInvariantError::new("validation repair repository write has no session")
        })?;
        if session.repository_writes_consumed >= session.budget.max_repository_writes {
            return Err(GraphInvariantError::new(
                "validation repair repository-write budget is exhausted",
            ));
        }
        session.repository_writes_consumed = session.repository_writes_consumed.saturating_add(1);
        Ok(())
    }

    pub fn reallocate_validation_repair_capacity(
        &mut self,
        failure_id: &FailureId,
        requested_model_calls: u32,
        requested_cost_micros: u64,
    ) -> Result<ValidationRepairReallocation, GraphInvariantError> {
        let progress_allows_reallocation = self.progress_events.last().is_none_or(|progress| {
            self.total_model_calls
                .saturating_sub(progress.model_calls_at_event)
                < self.progress_window.max_model_calls_without_progress
        });
        if !progress_allows_reallocation {
            return Err(GraphInvariantError::new(
                "validation repair reallocation is blocked by the progress window",
            ));
        }
        let session = self.repair_session_for_failure(failure_id).cloned().ok_or_else(|| {
            GraphInvariantError::new("validation repair reallocation has no session")
        })?;
        if session.attempted_targets.len()
            >= usize::try_from(session.budget.max_target_attempts).unwrap_or(usize::MAX)
        {
            return Err(GraphInvariantError::new(
                "validation repair reallocation cannot exceed the per-target attempt policy",
            ));
        }
        let owner = ExecutionNodeId::new(session.session_id.clone());
        let usage = self.usage_for(&owner);
        let mission_calls_remaining = self.mission.max_model_calls.saturating_sub(
            self.total_model_calls
                .saturating_add(self.total_model_calls_reserved),
        );
        let mission_cost_remaining = self.mission.max_cost_micros.saturating_sub(
            self.total_cost_micros
                .saturating_add(self.total_cost_micros_reserved),
        );
        let local_calls_remaining = session.budget.max_model_calls.saturating_sub(
            usage
                .model_calls_consumed
                .saturating_add(usage.model_calls_reserved),
        );
        let local_cost_remaining = session.budget.max_cost_micros.saturating_sub(
            usage
                .cost_micros
                .saturating_add(usage.cost_micros_reserved),
        );
        let model_calls = requested_model_calls.min(
            mission_calls_remaining.saturating_sub(local_calls_remaining),
        );
        let cost_micros = requested_cost_micros.min(
            mission_cost_remaining.saturating_sub(local_cost_remaining),
        );
        if model_calls == 0 && cost_micros == 0 {
            return Err(GraphInvariantError::new(
                "validation repair reallocation has no mission capacity available",
            ));
        }
        let session = self
            .repair_session_for_failure_mut(failure_id)
            .expect("validated repair session remains present");
        session.budget.max_model_calls =
            session.budget.max_model_calls.saturating_add(model_calls);
        session.budget.max_cost_micros =
            session.budget.max_cost_micros.saturating_add(cost_micros);
        session.reallocated_model_calls =
            session.reallocated_model_calls.saturating_add(model_calls);
        session.reallocated_cost_micros =
            session.reallocated_cost_micros.saturating_add(cost_micros);
        Ok(ValidationRepairReallocation {
            session_id: session.session_id.clone(),
            model_calls,
            cost_micros,
            budget: session.budget.clone(),
        })
    }

    pub fn continue_validation_repair_session(
        &mut self,
        failure: &FailureRecord,
        failure_revision: u64,
    ) {
        let direct_key = Self::repair_session_id(&failure.id);
        if self.validation_repair_sessions.contains_key(&direct_key) {
            if let Some(session) = self.validation_repair_sessions.get_mut(&direct_key) {
                session.current_assertion_set_revision = failure_revision;
                session.status = ValidationRepairSessionStatus::Active;
                session.stop_reason = None;
            }
            return;
        }
        let prior_key = self
            .validation_repair_sessions
            .iter()
            .find(|(_, session)| {
                session.originating_gate_id == failure.node_id
                    && session.status == ValidationRepairSessionStatus::ReadyForRerun
            })
            .map(|(key, _)| key.clone());
        let Some(prior_key) = prior_key else {
            return;
        };
        if let Some(session) = self.validation_repair_sessions.get_mut(&prior_key) {
            session.failed_validation_id = failure.id.to_string();
            session.current_assertion_set_revision = failure_revision;
            session.status = ValidationRepairSessionStatus::Active;
            session.stop_reason = None;
        }
    }

    pub fn usage_for(&self, node_id: &ExecutionNodeId) -> NodeBudgetUsage {
        self.node_usage.get(node_id).cloned().unwrap_or_default()
    }

    pub fn validate_invariants(&self, graph: &ExecutionGraph) -> Result<(), GraphInvariantError> {
        if self
            .total_model_calls
            .saturating_add(self.total_model_calls_reserved)
            > self.mission.max_model_calls
        {
            return Err(GraphInvariantError::new(
                "model-call reservations exceed the signed mission call budget",
            ));
        }
        if self
            .total_cost_micros
            .saturating_add(self.total_cost_micros_reserved)
            > self.mission.max_cost_micros
        {
            return Err(GraphInvariantError::new(
                "model-call reservations exceed the signed mission cost budget",
            ));
        }

        let node_reserved_calls = self
            .node_usage
            .values()
            .map(|usage| usage.model_calls_reserved)
            .fold(0_u32, u32::saturating_add);
        let node_reserved_cost = self
            .node_usage
            .values()
            .map(|usage| usage.cost_micros_reserved)
            .fold(0_u64, u64::saturating_add);
        if node_reserved_calls != self.total_model_calls_reserved
            || node_reserved_cost != self.total_cost_micros_reserved
        {
            return Err(GraphInvariantError::new(
                "mission reservation totals do not match per-node reservations",
            ));
        }
        for session in self.validation_repair_sessions.values() {
            let gate = graph.node(&session.originating_gate_id).ok_or_else(|| {
                GraphInvariantError::new(format!(
                    "validation repair session `{}` refers to unknown gate `{}`",
                    session.session_id, session.originating_gate_id
                ))
            })?;
            if !gate.kind.is_validation() {
                return Err(GraphInvariantError::new(format!(
                    "validation repair session `{}` does not originate from a validation gate",
                    session.session_id
                )));
            }
            session
                .budget
                .validate(session.budget.max_target_attempts > 1)?;
            if session.attempted_targets.len()
                > usize::try_from(session.budget.max_target_attempts).unwrap_or(usize::MAX)
            {
                return Err(GraphInvariantError::new(format!(
                    "validation repair session `{}` persisted more attempts than its target budget",
                    session.session_id
                )));
            }
            if session.repository_writes_consumed > session.budget.max_repository_writes
                || session.context_rebuilds_consumed > session.budget.max_context_rebuilds
            {
                return Err(GraphInvariantError::new(format!(
                    "validation repair session `{}` exceeded its repository-write or context-rebuild budget",
                    session.session_id
                )));
            }
        }
        for (node_id, usage) in &self.node_usage {
            let owned_budget;
            let budget = if let Some(node) = graph.node(node_id) {
                &node.budget
            } else if let Some(session) = self.validation_repair_sessions.get(node_id.as_str()) {
                owned_budget = session.budget.as_node_budget();
                &owned_budget
            } else {
                return Err(GraphInvariantError::new(format!(
                    "budget usage refers to unknown execution node or repair session `{node_id}`"
                )));
            };
            if usage
                .model_calls_consumed
                .saturating_add(usage.model_calls_reserved)
                > budget.max_model_calls
                || usage
                    .cost_micros
                    .saturating_add(usage.cost_micros_reserved)
                    > budget.max_cost_micros
            {
                return Err(GraphInvariantError::new(format!(
                    "node `{node_id}` reservations exceed its signed budget"
                )));
            }
        }
        Ok(())
    }

    pub fn remaining_for(
        &self,
        node_id: &ExecutionNodeId,
        node_budget: &NodeBudget,
    ) -> NodeBudgetRemaining {
        node_budget.remaining(&self.usage_for(node_id))
    }

    pub fn can_start_model_call(
        &self,
        node_id: &ExecutionNodeId,
        node_budget: &NodeBudget,
    ) -> bool {
        self.can_spend_model_call(node_id, node_budget, 0, Duration::ZERO)
    }

    pub fn can_start_model_call_for(&self, node: &ExecutionNode) -> bool {
        self.can_start_model_call(&node.id, &node.budget)
    }

    pub fn can_spend_model_call(
        &self,
        node_id: &ExecutionNodeId,
        node_budget: &NodeBudget,
        estimated_cost_micros: u64,
        estimated_duration: Duration,
    ) -> bool {
        self.evaluate_model_call_admission(
            node_id,
            node_budget,
            1,
            estimated_cost_micros,
            estimated_duration,
        )
        .admitted
    }

    pub fn evaluate_model_call_admission(
        &self,
        node_id: &ExecutionNodeId,
        node_budget: &NodeBudget,
        requested_calls: u32,
        estimated_cost_micros: u64,
        estimated_duration: Duration,
    ) -> ModelCallAdmission {
        let usage = self.usage_for(node_id);
        let node_calls_after = usage
            .model_calls_consumed
            .saturating_add(usage.model_calls_reserved)
            .saturating_add(requested_calls);
        let mission_calls_after = self
            .total_model_calls
            .saturating_add(self.total_model_calls_reserved)
            .saturating_add(requested_calls);
        let node_cost_after = usage
            .cost_micros
            .saturating_add(usage.cost_micros_reserved)
            .saturating_add(estimated_cost_micros);
        let mission_cost_after = self
            .total_cost_micros
            .saturating_add(self.total_cost_micros_reserved)
            .saturating_add(estimated_cost_micros);

        let rejection_reason = if requested_calls == 0 {
            Some("invalid_requested_calls")
        } else if node_calls_after > node_budget.max_model_calls {
            Some("node_model_call_budget_exhausted")
        } else if mission_calls_after > self.mission.max_model_calls {
            Some("mission_model_call_budget_exhausted")
        } else if node_cost_after > node_budget.max_cost_micros {
            Some("node_cost_budget_exhausted")
        } else if mission_cost_after > self.mission.max_cost_micros {
            Some("mission_cost_budget_exhausted")
        } else if usage.duration.saturating_add(estimated_duration) > node_budget.max_duration {
            Some("node_duration_budget_exhausted")
        } else if self.elapsed.saturating_add(estimated_duration) > self.mission.max_duration {
            Some("mission_duration_budget_exhausted")
        } else {
            None
        };

        debug_assert!(
            !(node_budget.max_model_calls > 0
                && usage.model_calls_consumed == 0
                && usage.model_calls_reserved == 0
                && requested_calls == 1
                && rejection_reason == Some("node_model_call_budget_exhausted")),
            "a fresh node with a positive call budget must admit its first single-call request"
        );

        ModelCallAdmission {
            node_id: node_id.clone(),
            max_model_calls: node_budget.max_model_calls,
            consumed_calls: usage.model_calls_consumed,
            reserved_calls: usage.model_calls_reserved,
            requested_calls,
            admitted: rejection_reason.is_none(),
            rejection_reason,
            node_cost_used: usage.cost_micros,
            node_cost_reserved: usage.cost_micros_reserved,
            node_cost_limit: node_budget.max_cost_micros,
            estimated_request_cost: estimated_cost_micros,
            projected_node_cost: node_cost_after,
            mission_cost_used: self.total_cost_micros,
            mission_calls_used: self.total_model_calls,
        }
    }

    pub fn reserve_model_call(
        &mut self,
        node_id: &ExecutionNodeId,
        node_budget: &NodeBudget,
        estimated_cost_micros: u64,
        estimated_duration: Duration,
    ) -> Result<ModelCallReservation, ModelCallAdmission> {
        let admission = self.evaluate_model_call_admission(
            node_id,
            node_budget,
            1,
            estimated_cost_micros,
            estimated_duration,
        );
        if !admission.admitted {
            return Err(admission);
        }
        self.total_model_calls_reserved = self.total_model_calls_reserved.saturating_add(1);
        self.total_cost_micros_reserved = self
            .total_cost_micros_reserved
            .saturating_add(estimated_cost_micros);
        let usage = self.node_usage.entry(node_id.clone()).or_default();
        usage.model_calls_reserved = usage.model_calls_reserved.saturating_add(1);
        usage.cost_micros_reserved = usage
            .cost_micros_reserved
            .saturating_add(estimated_cost_micros);
        Ok(ModelCallReservation {
            node_id: node_id.clone(),
            estimated_cost_micros,
        })
    }

    pub fn release_model_call_reservation(&mut self, reservation: &ModelCallReservation) {
        self.total_model_calls_reserved = self.total_model_calls_reserved.saturating_sub(1);
        self.total_cost_micros_reserved = self
            .total_cost_micros_reserved
            .saturating_sub(reservation.estimated_cost_micros);
        let usage = self
            .node_usage
            .entry(reservation.node_id.clone())
            .or_default();
        usage.model_calls_reserved = usage.model_calls_reserved.saturating_sub(1);
        usage.cost_micros_reserved = usage
            .cost_micros_reserved
            .saturating_sub(reservation.estimated_cost_micros);
    }

    pub fn consume_model_call_reservation(
        &mut self,
        reservation: &ModelCallReservation,
        actual_cost_micros: u64,
        duration: Duration,
    ) {
        self.release_model_call_reservation(reservation);
        self.record_model_call(reservation.node_id.clone(), actual_cost_micros, duration);
    }

    pub fn record_model_call(
        &mut self,
        node_id: ExecutionNodeId,
        cost_micros: u64,
        duration: Duration,
    ) {
        self.total_model_calls = self.total_model_calls.saturating_add(1);
        self.total_cost_micros = self.total_cost_micros.saturating_add(cost_micros);
        self.elapsed = self.elapsed.saturating_add(duration);
        let usage = self.node_usage.entry(node_id).or_default();
        usage.model_calls_consumed = usage.model_calls_consumed.saturating_add(1);
        usage.cost_micros = usage.cost_micros.saturating_add(cost_micros);
        usage.duration = usage.duration.saturating_add(duration);
    }

    /// Accounts for bounded node work that does not invoke a model, such as a
    /// worker-owned validation command. Validation therefore consumes the same
    /// node and mission duration envelopes as model-backed work.
    pub fn record_node_duration(&mut self, node_id: ExecutionNodeId, duration: Duration) {
        self.elapsed = self.elapsed.saturating_add(duration);
        let usage = self.node_usage.entry(node_id).or_default();
        usage.duration = usage.duration.saturating_add(duration);
    }

    pub fn record_repair_attempt(&mut self, node_id: ExecutionNodeId) {
        let usage = self.node_usage.entry(node_id).or_default();
        usage.repair_attempts = usage.repair_attempts.saturating_add(1);
    }

    pub fn restore_repair_attempt(&mut self, node_id: &ExecutionNodeId) {
        let usage = self.node_usage.entry(node_id.clone()).or_default();
        usage.repair_attempts = usage.repair_attempts.saturating_sub(1);
    }

    pub fn record_validation_repair_attempt(&mut self, node_id: ExecutionNodeId) {
        let usage = self.node_usage.entry(node_id).or_default();
        usage.validation_repair_attempts = usage.validation_repair_attempts.saturating_add(1);
    }

    pub fn record_model_call_purpose(&mut self, purpose: ModelCallPurpose) {
        let counter = match purpose {
            ModelCallPurpose::InitialTargetMutation => {
                &mut self.model_call_breakdown.initial_target_mutation_calls
            }
            ModelCallPurpose::TargetMutationRepair => {
                &mut self.model_call_breakdown.target_mutation_repair_calls
            }
            ModelCallPurpose::ValidationDiagnosis => {
                &mut self.model_call_breakdown.validation_diagnosis_calls
            }
            ModelCallPurpose::ValidationRepairMutation => {
                &mut self.model_call_breakdown.validation_repair_mutation_calls
            }
        };
        *counter = counter.saturating_add(1);
    }

    pub fn restore_model_call_purpose(&mut self, purpose: ModelCallPurpose) {
        let counter = match purpose {
            ModelCallPurpose::InitialTargetMutation => {
                &mut self.model_call_breakdown.initial_target_mutation_calls
            }
            ModelCallPurpose::TargetMutationRepair => {
                &mut self.model_call_breakdown.target_mutation_repair_calls
            }
            ModelCallPurpose::ValidationDiagnosis => {
                &mut self.model_call_breakdown.validation_diagnosis_calls
            }
            ModelCallPurpose::ValidationRepairMutation => {
                &mut self.model_call_breakdown.validation_repair_mutation_calls
            }
        };
        *counter = counter.saturating_sub(1);
    }

    pub fn record_progress(&mut self, event: ProgressEvent) {
        // Event-sequence deduplication makes resume/replay idempotent.
        if self
            .progress_events
            .iter()
            .any(|existing| existing.sequence == event.sequence)
        {
            return;
        }
        self.progress_score = self
            .progress_score
            .saturating_add(u64::from(event.kind.score()));
        self.progress_events.push(event);
        self.progress_events.sort_by_key(|event| event.sequence);
    }

    pub fn record_progress_kind(
        &mut self,
        sequence: u64,
        kind: ProgressEventKind,
        node_id: Option<ExecutionNodeId>,
    ) {
        self.record_progress(ProgressEvent {
            sequence,
            kind,
            node_id,
            model_calls_at_event: self.total_model_calls,
            cost_micros_at_event: self.total_cost_micros,
            elapsed_at_event: self.elapsed,
        });
    }

    /// Stops immediately at a hard bound. At the deterministic 80% soft bound,
    /// it stops only after the configured call/time window produced no progress.
    pub fn should_stop_node(&self, node_id: &ExecutionNodeId, node_budget: &NodeBudget) -> bool {
        let usage = self.usage_for(node_id);
        let hard_exceeded = (node_budget.max_model_calls > 0
            && usage
                .model_calls_consumed
                .saturating_add(usage.model_calls_reserved)
                >= node_budget.max_model_calls)
            || (node_budget.max_cost_micros > 0
                && usage.cost_micros >= node_budget.max_cost_micros)
            || (!node_budget.max_duration.is_zero() && usage.duration >= node_budget.max_duration)
            || usage.repair_attempts > node_budget.max_repair_attempts
            || self.total_model_calls >= self.mission.max_model_calls
            || self.total_cost_micros >= self.mission.max_cost_micros
            || self.elapsed >= self.mission.max_duration;
        if hard_exceeded {
            return true;
        }

        let soft_exceeded = ratio_at_least(
            usage
                .model_calls_consumed
                .saturating_add(usage.model_calls_reserved),
            node_budget.max_model_calls,
            80,
        ) || ratio_at_least(usage.cost_micros, node_budget.max_cost_micros, 80)
            || duration_ratio_at_least(usage.duration, node_budget.max_duration, 80);
        if !soft_exceeded {
            return false;
        }

        let last_progress = self
            .progress_events
            .iter()
            .rev()
            .find(|event| event.node_id.as_ref() == Some(node_id));
        let calls_at_progress = last_progress.map_or(0, |event| event.model_calls_at_event);
        let elapsed_at_progress =
            last_progress.map_or(Duration::ZERO, |event| event.elapsed_at_event);
        self.total_model_calls.saturating_sub(calls_at_progress)
            >= self.progress_window.max_model_calls_without_progress
            || self.elapsed.saturating_sub(elapsed_at_progress)
                >= self.progress_window.max_duration_without_progress
    }
}

fn ratio_at_least(value: impl Into<u64>, maximum: impl Into<u64>, percent: u64) -> bool {
    let value = value.into();
    let maximum = maximum.into();
    maximum > 0 && value.saturating_mul(100) >= maximum.saturating_mul(percent)
}

fn duration_ratio_at_least(value: Duration, maximum: Duration, percent: u64) -> bool {
    let value = u64::try_from(value.as_millis()).unwrap_or(u64::MAX);
    let maximum = u64::try_from(maximum.as_millis()).unwrap_or(u64::MAX);
    ratio_at_least(value, maximum, percent)
}
