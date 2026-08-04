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
    #[serde(default)]
    pub progress_events: Vec<ProgressEvent>,
    #[serde(default)]
    pub model_call_breakdown: ModelCallBreakdown,
    pub progress_score: u64,
    pub progress_window: ProgressWindow,
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
            progress_events: Vec::new(),
            model_call_breakdown: ModelCallBreakdown::default(),
            progress_score: 0,
            progress_window: ProgressWindow::default(),
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
        for (node_id, usage) in &self.node_usage {
            let node = graph.node(node_id).ok_or_else(|| {
                GraphInvariantError::new(format!(
                    "budget usage refers to unknown execution node `{node_id}`"
                ))
            })?;
            if usage
                .model_calls_consumed
                .saturating_add(usage.model_calls_reserved)
                > node.budget.max_model_calls
                || usage
                    .cost_micros
                    .saturating_add(usage.cost_micros_reserved)
                    > node.budget.max_cost_micros
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
