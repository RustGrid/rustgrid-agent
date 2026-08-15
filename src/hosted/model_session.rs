// Extracted from the hosted execution composition root.
use super::*;

/// Model invocation capability consumed by hosted model sessions.
pub(crate) trait ModelProvider {
    type Error: Into<anyhow::Error>;

    fn invoke(
        &self,
        request: Value,
        registration: &AiCallRegistration,
        execution_deadline: Option<Instant>,
    ) -> std::result::Result<Value, Self::Error>;
}

impl ModelProvider for HostedApiClient {
    type Error = anyhow::Error;

    fn invoke(
        &self,
        request: Value,
        registration: &AiCallRegistration,
        execution_deadline: Option<Instant>,
    ) -> Result<Value> {
        self.ai_response_until(request, registration, execution_deadline)
    }
}

pub(super) fn invoke_model<P: ModelProvider>(
    provider: &P,
    request: Value,
    registration: &AiCallRegistration,
    execution_deadline: Option<Instant>,
) -> Result<Value> {
    provider
        .invoke(request, registration, execution_deadline)
        .map_err(Into::into)
}

pub(super) struct GatewayAgent<'a> {
    pub(super) api: HostedApiClient,
    pub(super) manifest: &'a HostedManifest,
    pub(super) repo: &'a Repo,
    pub(super) trusted_git_config: Vec<u8>,
    pub(super) running: &'a Arc<AtomicBool>,
    pub(super) stop_reason: &'a Arc<Mutex<Option<HostedStopReason>>>,
    pub(super) lease_renewed_at: &'a Arc<Mutex<Option<String>>>,
    pub(super) containment: &'a command::HostedProcessContainment,
    pub(super) budget: BudgetAudit,
    pub(super) phases: PhaseLedger,
    pub(super) impact_map: Option<ImpactMap>,
    pub(super) implementation_plan: Option<ImplementationPlan>,
    pub(super) declaration: Option<ImplementationDeclaration>,
    pub(super) tool_failures: Vec<ToolFailureRecord>,
    pub(super) tool_usage: ToolUsage,
    pub(super) notebook: WorkerNotebook,
    pub(super) search_guard: SearchGuard,
    pub(super) repair_read_targets: BTreeSet<String>,
    pub(super) diff_reviewed: bool,
    pub(super) diff_review_cursor: usize,
    pub(super) diff_review_digest: Option<String>,
    pub(super) write_blocker: Option<String>,
    pub(super) blocked_plan_recorded_at: Option<usize>,
    pub(super) impact_map_failure: Option<ImpactMapFailure>,
    pub(super) last_successful_action: Value,
    pub(super) partial_run: Option<PartialRunContext>,
    pub(super) budget_advisory_percent: u8,
    pub(super) last_cache_prefix_sha256: Option<String>,
    pub(super) last_tool_order_sha256: Option<String>,
    pub(super) guided_first_write_recovery_issued: bool,
    pub(super) last_repository_progress_call: usize,
    pub(super) cost_guard: CostGuard,
    pub(super) execution_started_at: Instant,
    pub(super) phase_started_at: Instant,
    pub(super) last_source_progress_call: usize,
    /// Last pure-orchestrator decision applied by the sole lifecycle adapter.
    pub(super) current_decision: Option<ExecutionDecision>,
    pub(super) completion_outcome: Option<OrchestratedMissionOutcome>,
    /// Semantic identity of the model call whose tool response is currently
    /// being applied. Transport retries retain this identity.
    pub(super) active_model_call_id: Option<String>,
    pub(super) phase_persistence_failure: Option<PhasePersistenceFailure>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct CostGuard {
    pub(super) estimated_cost_micros: u64,
    pub(super) call_count: u32,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) usage_estimate_fallbacks: u32,
    pub(super) repository_progress_score: f32,
    pub(super) hard_limit_micros: u64,
    pub(super) max_duration_seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RequestCostEstimate {
    pub(super) input_tokens_estimated: u64,
    pub(super) max_output_tokens: u64,
    pub(super) reasoning_effort: String,
    pub(super) estimated_request_cost: u64,
    pub(super) cost_estimation_method: &'static str,
}

pub(super) fn estimate_model_call_request_cost(request: &Value) -> RequestCostEstimate {
    let serialized_bytes = serde_json::to_vec(request).map_or(0, |bytes| bytes.len());
    let input_tokens_estimated =
        u64::try_from(serialized_bytes.saturating_add(3) / 4).unwrap_or(u64::MAX);
    let max_output_tokens = request
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let reasoning_effort = request
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_owned();
    // Provider-independent upper bound. The output cap includes reasoning
    // tokens, so effort is reported and bounded by the same exact cap rather
    // than charged a second time.
    let estimated_request_cost = input_tokens_estimated
        .saturating_mul(5)
        .saturating_add(max_output_tokens.saturating_mul(15));
    RequestCostEstimate {
        input_tokens_estimated,
        max_output_tokens,
        reasoning_effort,
        estimated_request_cost,
        cost_estimation_method: "serialized_request_bytes_div_4_plus_action_output_cap_v1",
    }
}

pub(super) fn model_call_admission_telemetry(
    admission: &crate::execution_graph::ModelCallAdmission,
    estimate: &RequestCostEstimate,
) -> Value {
    json!({
        "event_type": "worker.model_call_admission_evaluated",
        "node_id": admission.node_id,
        "max_model_calls": admission.max_model_calls,
        "consumed_calls": admission.consumed_calls,
        "reserved_calls": admission.reserved_calls,
        "requested_calls": admission.requested_calls,
        "admitted": admission.admitted,
        "rejection_reason": admission.rejection_reason,
        "node_cost_used": admission.node_cost_used,
        "node_cost_limit": admission.node_cost_limit,
        "node_cost_consumed": admission.node_cost_used,
        "node_cost_reserved": admission.node_cost_reserved,
        "estimated_request_cost": admission.estimated_request_cost,
        "projected_node_cost": admission.projected_node_cost,
        "input_tokens_estimated": estimate.input_tokens_estimated,
        "max_output_tokens": estimate.max_output_tokens,
        "reasoning_effort": estimate.reasoning_effort,
        "cost_estimation_method": estimate.cost_estimation_method,
        "mission_cost_used": admission.mission_cost_used,
        "mission_calls_used": admission.mission_calls_used,
        "model_calls_remaining": admission.max_model_calls.saturating_sub(
            admission
                .consumed_calls
                .saturating_add(admission.reserved_calls)
                .saturating_add(admission.requested_calls)
        ),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostedWallClockBoundary {
    BeforeValidation,
    PublicationReconciliation,
    PullRequestCreation,
}

impl HostedWallClockBoundary {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::BeforeValidation => "before_validation",
            Self::PublicationReconciliation => "publication_reconciliation",
            Self::PullRequestCreation => "pull_request_creation",
        }
    }

    pub(super) const fn is_publication(self) -> bool {
        matches!(
            self,
            Self::PublicationReconciliation | Self::PullRequestCreation
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostedWallClockAction {
    Continue,
    EnterPartialValidation,
    CompleteBlockedNoDiff,
    ContinueFinalization,
    InvalidFinalizationRoute,
}

pub(super) fn hosted_wall_clock_action(
    expired: bool,
    boundary: HostedWallClockBoundary,
    has_reviewable_diff: bool,
    decision: &ExecutionDecision,
) -> HostedWallClockAction {
    if !expired {
        return HostedWallClockAction::Continue;
    }
    let publication_route = matches!(decision, ExecutionDecision::Publish { .. })
        || matches!(
            decision,
            ExecutionDecision::Finish { outcome } if outcome.publication_mode().is_some()
        );
    if boundary.is_publication() {
        return if publication_route {
            HostedWallClockAction::ContinueFinalization
        } else {
            HostedWallClockAction::InvalidFinalizationRoute
        };
    }
    let terminal_without_publication = matches!(
        decision,
        ExecutionDecision::StopForGuardrail { outcome, .. }
            | ExecutionDecision::Finish { outcome }
                if outcome.publication_mode().is_none()
    );
    if terminal_without_publication {
        return HostedWallClockAction::InvalidFinalizationRoute;
    }
    if matches!(
        decision,
        ExecutionDecision::ReviewDiff { .. }
            | ExecutionDecision::EvaluateCompletion { .. }
            | ExecutionDecision::Publish { .. }
            | ExecutionDecision::Finish { .. }
    ) {
        return HostedWallClockAction::ContinueFinalization;
    }
    if has_reviewable_diff {
        HostedWallClockAction::EnterPartialValidation
    } else {
        HostedWallClockAction::CompleteBlockedNoDiff
    }
}

#[cfg(test)]
pub(super) const fn model_cost_limit_for_target_count(target_count: usize) -> u64 {
    match target_count {
        0 | 1 => 2_000_000,
        2..=8 => 5_000_000,
        9..=12 => 10_000_000,
        _ => 20_000_000,
    }
}

pub(super) fn constrain_request_to_cost_limit(
    request: &mut Value,
    guard: &CostGuard,
) -> Result<bool> {
    let configured_output = request
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .context("provider request is missing max_output_tokens")?;
    let request_bytes = u64::try_from(serde_json::to_vec(request)?.len()).unwrap_or(u64::MAX);
    // A tokenizer cannot emit more tokens than the number of encoded input bytes. Reserve that
    // conservative input cost first, then cap the provider's output allowance to the remaining
    // estimated-cost envelope before dispatch.
    let input_cost_upper_bound = request_bytes.saturating_mul(5);
    let remaining = guard
        .hard_limit_micros
        .saturating_sub(guard.estimated_cost_micros);
    if remaining <= input_cost_upper_bound.saturating_add(15) {
        return Ok(false);
    }
    let affordable_output = remaining
        .saturating_sub(input_cost_upper_bound)
        .checked_div(15)
        .unwrap_or_default()
        .min(configured_output);
    if affordable_output == 0 {
        return Ok(false);
    }
    request["max_output_tokens"] = json!(affordable_output);
    Ok(true)
}

pub(super) fn model_usage_for_accounting(
    request: &Value,
    response: &Value,
) -> Result<(u64, u64, bool)> {
    let reported_input = response
        .pointer("/usage/input_tokens")
        .and_then(Value::as_u64);
    let reported_output = response
        .pointer("/usage/output_tokens")
        .and_then(Value::as_u64);
    let conservative_input = u64::try_from(serde_json::to_vec(request)?.len()).unwrap_or(u64::MAX);
    let conservative_output = request
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .context("provider request is missing max_output_tokens")?;
    Ok((
        reported_input.unwrap_or(conservative_input),
        reported_output.unwrap_or(conservative_output),
        reported_input.is_none() || reported_output.is_none(),
    ))
}

pub(super) fn failed_model_usage_for_accounting(
    request: &Value,
    actual_cost_micros: Option<u64>,
) -> (u64, u64, u64, bool) {
    let serialized_bytes = serde_json::to_vec(request).map_or(0, |bytes| bytes.len());
    let estimated_input_tokens =
        u64::try_from(serialized_bytes.saturating_add(3) / 4).unwrap_or(u64::MAX);
    let estimated_output_tokens = request
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let conservative_cost_micros = estimated_input_tokens
        .saturating_mul(5)
        .saturating_add(estimated_output_tokens.saturating_mul(15));
    match actual_cost_micros {
        Some(cost) => (0, 0, cost, false),
        None => (
            estimated_input_tokens,
            estimated_output_tokens,
            conservative_cost_micros,
            true,
        ),
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CancellationResult {
    pub(super) requested_by: &'static str,
    pub(super) requested_at: String,
    pub(super) phase: ExecutionPhase,
    pub(super) changed_paths: Vec<String>,
    pub(super) completed_changes: Vec<String>,
    pub(super) remaining_work: Vec<RemainingWorkItem>,
    pub(super) source_tree_hash: String,
    pub(super) resumable: bool,
    pub(super) resume_phase: ExecutionPhase,
}

pub(super) fn ordered_implementation_targets_from_notebook(
    notebook: &WorkerNotebook,
) -> Vec<ImplementationTarget> {
    notebook
        .intended_changes
        .iter()
        .flat_map(|change| {
            change
                .targets
                .iter()
                .map(move |target| ImplementationTarget {
                    change_id: change.change_id.clone(),
                    path: target.path.clone(),
                    role: target.role.clone(),
                    new_file: target.new_file,
                    intent: change.intent.clone(),
                    acceptance_criteria: notebook
                        .planned_changes
                        .iter()
                        .find(|planned| planned.change_id == change.change_id)
                        .map(|planned| planned.acceptance_criteria.clone())
                        .unwrap_or_default(),
                    status: target.status,
                })
        })
        .collect()
}

pub(super) fn implementation_start_context_from_notebook(
    notebook: &WorkerNotebook,
    source_tree_hash: String,
    remaining_call_budget: usize,
    guided_recovery: bool,
    implementation_calls: usize,
    successful_writes: u32,
) -> ImplementationStartContext {
    let validation_repair =
        notebook.phase == ExecutionPhase::Repair && has_unresolved_validation_failure(notebook);
    let target_order = ordered_implementation_targets_from_notebook(notebook)
        .into_iter()
        .filter(|target| {
            validation_repair
                || !matches!(
                    target.status,
                    IntendedChangeStatus::Applied | IntendedChangeStatus::Verified
                )
        })
        .collect::<Vec<_>>();
    let target_paths = target_order
        .iter()
        .map(|target| target.path.as_str())
        .collect::<BTreeSet<_>>();
    let exact_files_already_read = notebook
        .files_inspected
        .iter()
        .filter(|path| target_paths.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let inspected = exact_files_already_read
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let missing_file_contents = target_order
        .iter()
        .filter(|target| !target.new_file && !inspected.contains(target.path.as_str()))
        .map(|target| target.path.clone())
        .collect();
    let current_target = target_order
        .iter()
        .find(|target| {
            !matches!(
                target.status,
                IntendedChangeStatus::Applied | IntendedChangeStatus::Verified
            )
        })
        .cloned()
        .or_else(|| target_order.first().cloned());
    let unresolved_preparation_blockers = if guided_recovery {
        unresolved_preparation_blockers(
            &notebook.tool_progress,
            notebook.execution_attempt,
            implementation_calls,
            successful_writes,
        )
    } else {
        Vec::new()
    };
    ImplementationStartContext {
        goal: notebook.goal.clone(),
        target_order,
        acceptance_criteria_ids: notebook
            .acceptance_criteria_v2
            .iter()
            .map(|criterion| criterion.id.clone())
            .collect(),
        assigned_acceptance_criteria: Vec::new(),
        exact_files_already_read,
        missing_file_contents,
        source_tree_hash,
        remaining_call_budget,
        current_target,
        cached_current_file_content: None,
        target_content_hash: None,
        repository_fingerprint: notebook.repository_fingerprint.clone(),
        mutation_repair: None,
        cached_nearby_context: Vec::new(),
        graph_node_id: None,
        dependency_evidence: Vec::new(),
        relevant_impact_areas: Vec::new(),
        related_test_evidence: Vec::new(),
        constraints: vec![
            "Preserve existing defaults and fallback behavior unless the accepted change intent explicitly replaces them.".into(),
            "Preserve public APIs, persisted keys, and behavior outside the assigned acceptance criteria.".into(),
            "Do not inspect or mutate another planned target during this node action.".into(),
        ],
        allowed_tools: Vec::new(),
        remaining_node_budget: None,
        guided_recovery,
        unresolved_preparation_blockers,
        instruction: if guided_recovery {
            "FIRST-WRITE RECOVERY: work only on current_target. Use its existing read evidence or perform one bounded read of that exact path, then attempt its authorized mutation. Do not inspect another target, re-run discovery, or perform validation.".into()
        } else {
            "Read only the exact files still needed. Begin source mutations as soon as sufficient context exists. Do not re-run discovery. Do not perform final repository validation; the worker runs authoritative validation after all targets are applied.".into()
        },
    }
}

pub(super) fn has_unresolved_validation_failure(notebook: &WorkerNotebook) -> bool {
    notebook.orchestration.failures.unresolved().any(|failure| {
        failure.category == crate::execution_graph::FailureCategory::ValidationFailure
    })
}

pub(super) fn validate_current_target_scope(
    current_target: Option<&ImplementationTarget>,
    guided_recovery: bool,
    successful_writes: u32,
    paths: &[&str],
    source_mutation: bool,
) -> Result<()> {
    let Some(current_target) = current_target else {
        return Ok(());
    };
    if source_mutation
        && paths
            .first()
            .is_some_and(|path| *path != current_target.path)
    {
        bail!("active_target_mismatch: mutate the current planned target before later targets");
    }
    if guided_recovery
        && successful_writes == 0
        && paths.iter().any(|path| *path != current_target.path)
    {
        bail!(
            "first_write_recovery_target_mismatch: guided recovery is constrained to the current planned target"
        );
    }
    Ok(())
}

pub(super) fn classify_hosted_mutation_preflight(
    snapshot: &crate::execution_graph::ExecutionSnapshot,
    current_node_id: Option<&crate::execution_graph::ExecutionNodeId>,
    attempted_path: &str,
    active_validation_repair: bool,
) -> std::result::Result<
    Option<MutationPreflightError>,
    crate::hosted_orchestrator::OrchestrationInvariantError,
> {
    // Implementation idempotency is not proof that a failed assertion's
    // correction intent is already satisfied. Repair uses its own evidence
    // contract and must be allowed to inspect or mutate an implementation-
    // complete target.
    if active_validation_repair {
        return Ok(None);
    }
    let current_path_matches = current_node_id.is_some_and(|node_id| {
        snapshot
            .graph
            .node(node_id)
            .and_then(|node| node.target.as_ref())
            .is_some_and(|target| target.path == attempted_path)
    });
    let node_id = if current_path_matches {
        current_node_id.cloned()
    } else {
        let mut matching_nodes = snapshot.graph.nodes.iter().filter(|node| {
            node.kind.is_mutation()
                && node
                    .target
                    .as_ref()
                    .is_some_and(|target| target.path == attempted_path)
        });
        let matching = matching_nodes.next();
        if matching_nodes.next().is_some() {
            return Ok(None);
        }
        matching.map(|node| node.id.clone())
    };
    let Some(node_id) = node_id else {
        return Ok(None);
    };
    let Some(crate::execution_graph::MutationResult::AlreadyApplied { target, .. }) =
        classify_mutation_request(snapshot, &node_id)?
    else {
        return Ok(None);
    };
    let next_target = snapshot
        .graph
        .nodes
        .iter()
        .find(|node| node.kind.is_mutation() && !node.status.is_success())
        .and_then(|node| node.target.as_ref())
        .map_or("worker-owned validation", |target| target.path.as_str());
    Ok(Some(MutationPreflightError {
        code: "target_already_applied",
        change_id: target.change_id,
        target: target.path,
        message: format!(
            "target_already_applied: this target is already present in the authoritative repository state; continue with `{next_target}`"
        ),
        repair_strategy: "continue_next_target",
    }))
}

pub(super) fn mark_mutation_preflight_blocker(
    write_blocker: &mut Option<String>,
    target: &str,
) -> bool {
    write_blocker.get_or_insert_with(|| {
        format!(
            "mutation_preflight_rejected: target `{target}` requires persisted plan repair before implementation can continue"
        )
    });
    true
}

impl<'a> GatewayAgent<'a> {
    pub(super) fn prepare_next_model_call(
        &mut self,
        allow_budget_handoff: bool,
    ) -> Result<Option<ImplementationOutcome>> {
        self.active_model_call_id = None;
        loop {
            if active_mutation_fallback(self.current_decision.as_ref()).is_some() {
                break;
            }
            let graph_decision = self.reconcile_execution_and_apply()?;
            match graph_decision.decision {
                ExecutionDecision::ExecuteTarget {
                    action: crate::hosted_orchestrator::MutationAction::PrepareTargetContext { .. },
                    ..
                } => {
                    let _ = self.prepare_active_target_context()?;
                    continue;
                }
                ExecutionDecision::ExecuteTarget {
                    action: crate::hosted_orchestrator::MutationAction::VerifyTargetState { .. },
                    ..
                } => {
                    self.verify_active_target_state()?;
                    continue;
                }
                ExecutionDecision::StopForGuardrail { outcome, reason } => {
                    if reason == crate::execution_graph::GuardrailReason::NoProgress {
                        self.emit_mutation_no_progress_diagnostics()?;
                    }
                    let changed_paths =
                        completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
                    if outcome == OrchestratedMissionOutcome::PartialReviewable
                        && allow_budget_handoff
                        && !changed_paths.is_empty()
                    {
                        return Ok(Some(ImplementationOutcome {
                            summary: format!(
                                "The execution graph stopped the active node at {reason:?}; preserved {} changed path(s) for deterministic validation and draft review.",
                                changed_paths.len()
                            ),
                            budget_exhausted: true,
                            explicit_declaration: self.declaration.clone(),
                        }));
                    }
                    if reason == crate::execution_graph::GuardrailReason::NodeBudgetExhausted
                        && self.has_unresolved_mutation_application_failure()
                    {
                        return Err(self.mutation_application_exhausted_failure());
                    }
                    return Err(self.execution_failure(
                        "execution_graph_guardrail",
                        format!(
                            "The execution graph stopped hosted work at {reason:?} with outcome {outcome:?}."
                        ),
                        None,
                        true,
                        "Resume from the persisted graph after resolving the reported guardrail.",
                    ));
                }
                ExecutionDecision::RunValidation { .. }
                    if matches!(
                        self.phases.active(),
                        ExecutionPhase::Validation
                            | ExecutionPhase::Implementation
                            | ExecutionPhase::Repair
                    ) =>
                {
                    return Ok(Some(ImplementationOutcome {
                        summary:
                            "The execution graph completed model-owned work and selected validation."
                                .into(),
                        budget_exhausted: false,
                        explicit_declaration: self.declaration.clone(),
                    }));
                }
                ExecutionDecision::ReviewDiff { .. }
                | ExecutionDecision::EvaluateCompletion { .. }
                | ExecutionDecision::Publish { .. }
                | ExecutionDecision::Finish { .. } => {
                    return Ok(Some(ImplementationOutcome {
                        summary:
                            "The execution graph advanced beyond model-owned implementation work."
                                .into(),
                        budget_exhausted: false,
                        explicit_declaration: self.declaration.clone(),
                    }));
                }
                _ => {}
            }
            if let Some((threshold, code, message)) =
                hosted_budget_advisory(self.phases.total_calls(), self.phases.total_limit())
                    .filter(|(threshold, _, _)| *threshold > self.budget_advisory_percent)
            {
                self.budget_advisory_percent = threshold;
                self.emit_guardrail(code, "continue_toward_completion", message)?;
            }
            let phase = self.phases.active();
            if phase == ExecutionPhase::Planning
                && self.blocked_plan_recorded_at.is_some_and(|recorded_at| {
                    self.phases.phase_calls(ExecutionPhase::Planning) > recorded_at
                })
            {
                self.emit_guardrail(
                    "blocked_insufficient_context",
                    "terminate",
                    "The one targeted inspection cycle after a blocked plan did not resolve its blocker.",
                )?;
                return Err(self.execution_failure(
                    "blocked_insufficient_context",
                    "Planning remained blocked after one targeted inspection cycle.",
                    None,
                    true,
                    "Resolve the listed blocking unknown or continue from the preserved notebook.",
                ));
            }
            let used = self.phases.phase_calls(phase);
            let limit = self.phases.phase_limit(phase);
            if self.notebook.orchestration.graph.is_some() || used < limit {
                break;
            }
            match phase {
                ExecutionPhase::Discovery if self.impact_map.is_some() => {
                    self.record_discovery_completed()?;
                    self.reconcile_execution_and_apply()?;
                }
                ExecutionPhase::Discovery if self.impact_map_failure.is_some() => {
                    let detail = self
                        .impact_map_failure
                        .as_ref()
                        .map_or("impact map requires repair", |failure| {
                            failure.safe_error.as_str()
                        })
                        .to_owned();
                    self.record_discovery_failure(&detail)?;
                    self.reconcile_execution_and_apply()?;
                }
                ExecutionPhase::Discovery => {
                    if self.accept_deterministic_impact_map_if_available(
                        "discovery_model_call_budget_exhausted_after_evidence_collection",
                    )? {
                        continue;
                    }
                    self.emit_guardrail(
                        "discovery_budget_exhausted",
                        "terminate",
                        "Discovery reached its hard limit without an implementation impact map.",
                    )?;
                    return Err(self.impact_map_execution_failure(
                        "impact_map_not_produced",
                        format!(
                            "Discovery reached call {limit} without a valid implementation impact map."
                        ),
                        ArtifactSemanticStatus::Missing,
                        ArtifactPersistenceStatus::PendingRetry,
                        "Continue with a narrower discovery scope and record the impact map.",
                    ));
                }
                ExecutionPhase::ArtifactRepair if self.impact_map.is_some() => {
                    self.record_discovery_completed()?;
                    self.reconcile_execution_and_apply()?;
                }
                ExecutionPhase::ArtifactRepair => {
                    let failure = self.impact_map_failure.as_ref();
                    let code = failure
                        .map(|failure| failure.code)
                        .unwrap_or("impact_map_invalid");
                    let detail = failure
                        .map(|failure| failure.safe_error.as_str())
                        .unwrap_or("The targeted artifact repair did not produce a valid map.");
                    self.emit_guardrail(
                        code,
                        "resume_artifact_repair",
                        "The targeted impact-map repair call did not produce a valid artifact.",
                    )?;
                    return Err(self.impact_map_execution_failure(
                        code,
                        format!("Impact-map repair failed: {detail}"),
                        self.notebook.impact_map_artifact.semantic_status,
                        ArtifactPersistenceStatus::PendingRetry,
                        "Resume from artifact repair with the preserved discovery notebook.",
                    ));
                }
                ExecutionPhase::Planning
                    if self
                        .implementation_plan
                        .as_ref()
                        .is_some_and(|plan| plan.implementation_status == "ready") =>
                {
                    self.reconcile_execution_and_apply()?;
                }
                ExecutionPhase::Planning => {
                    if self.accept_deterministic_implementation_plan_if_available(
                        "planning_model_call_budget_exhausted_after_impact_map",
                    )? {
                        continue;
                    }
                    self.emit_guardrail(
                        "planning_budget_exhausted",
                        "terminate",
                        "Planning reached its hard limit without a machine-readable implementation plan.",
                    )?;
                    return Err(self.execution_failure(
                        "implementation_plan_missing",
                        format!(
                            "Planning reached its {limit}-call limit without a valid implementation plan."
                        ),
                        None,
                        true,
                        "Continue from the impact map and record a machine-readable plan.",
                    ));
                }
                ExecutionPhase::Implementation | ExecutionPhase::Repair => {
                    let status = self.reconcile_authoritative_target_state()?;
                    if status == ImplementationCompletionStatus::ReadyForValidation {
                        self.reconcile_execution_and_apply()?;
                        return Ok(Some(ImplementationOutcome {
                            summary: "All planned targets are applied; continuing with validation."
                                .into(),
                            budget_exhausted: false,
                            explicit_declaration: self.declaration.clone(),
                        }));
                    }
                    let changed_paths =
                        completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
                    if allow_budget_handoff && !changed_paths.is_empty() {
                        return Ok(Some(ImplementationOutcome {
                            summary: format!(
                                "Implementation call guardrail preserved {} changed path(s) as a resumable partial result.",
                                changed_paths.len()
                            ),
                            budget_exhausted: true,
                            explicit_declaration: self.declaration.clone(),
                        }));
                    }
                    if self.tool_usage.successful_writes == 0 && changed_paths.is_empty() {
                        self.write_blocker.get_or_insert_with(|| {
                            "implementation preparation exhausted its bounded call allowance without a verified repository mutation".into()
                        });
                        self.checkpoint_notebook(false)?;
                        return Err(self.implementation_preparation_failure());
                    }
                    return Err(self.execution_failure(
                        "implementation_progress_missing",
                        "Implementation reached its bounded call limit without an applied target.",
                        None,
                        true,
                        "Resume from the persisted implementation plan after resolving the blocker.",
                    ));
                }
                ExecutionPhase::DiffReview => {
                    let changed_paths = self.repo.new_agent_paths(&BTreeSet::new())?;
                    if let Some(summary) =
                        model_budget_handoff_summary(allow_budget_handoff, &changed_paths)
                    {
                        self.emit_guardrail(
                            "diff_review_budget_exhausted",
                            "preserve_partial_result",
                            "Diff review ended without a complete implementation declaration.",
                        )?;
                        return Ok(Some(ImplementationOutcome {
                            summary,
                            budget_exhausted: true,
                            explicit_declaration: self.declaration.clone(),
                        }));
                    }
                    return Err(self.execution_failure(
                        "diff_review_incomplete",
                        "The diff-review allocation ended without a complete implementation declaration.",
                        None,
                        true,
                        "Continue from the preserved diff and complete review and declaration.",
                    ));
                }
                ExecutionPhase::CompletionEvaluation => {
                    bail!("completion evaluation exhausted its reserved model-call allocation");
                }
                ExecutionPhase::Validation | ExecutionPhase::Publication => {
                    bail!(
                        "phase `{}` cannot run the implementation model",
                        phase.as_str()
                    );
                }
            }
        }

        if self
            .phases
            .phase_calls(self.phases.active())
            .saturating_add(1)
            >= self.effective_phase_model_call_limit()
        {
            self.emit_phase_budget_warning()?;
        }
        Ok(None)
    }

    pub(super) fn repair(
        &mut self,
        failures: &[ValidationResult],
        attempt: usize,
    ) -> Result<ImplementationOutcome> {
        self.record_validation_failures(failures, attempt)?;
        if failures.iter().any(|failure| {
            matches!(
                failure.status.as_str(),
                "infrastructure_failed" | "timed_out"
            )
        }) {
            let changed_paths =
                completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
            let all_mutations_applied =
                self.notebook
                    .orchestration
                    .graph
                    .as_ref()
                    .is_some_and(|graph| {
                        graph
                            .nodes
                            .iter()
                            .filter(|node| node.required && node.kind.is_mutation())
                            .all(|node| node.status.satisfies_dependency())
                    });
            if !changed_paths.is_empty() && all_mutations_applied {
                self.record_partial_reviewable_handoff(
                    crate::execution_graph::GuardrailReason::InfrastructureFailure,
                    "required validation remained incomplete after one model-free infrastructure retry; preserve the applied diff for draft review",
                )?;
                self.persist_orchestration_checkpoint(
                    "validation_infrastructure_partial_reviewable",
                    true,
                )?;
                let timed_out = failures.iter().any(|failure| failure.status == "timed_out");
                return Err(self.execution_failure(
                    if timed_out {
                        "validation_process_timeout"
                    } else {
                        "validation_infrastructure_failure"
                    },
                    "Worker-owned validation did not produce a code assertion result after its infrastructure retry.",
                    None,
                    true,
                    "Resume at the incomplete validation node; the applied repository diff is preserved in a draft pull request.",
                ));
            }
            self.finalize_guardrail_outcome(OrchestratedMissionOutcome::FailedInfrastructure)?;
            self.persist_orchestration_checkpoint("validation_infrastructure_failure", true)?;
            return Err(self.execution_failure(
                if failures.iter().any(|failure| failure.status == "timed_out") {
                    "validation_process_timeout"
                } else {
                    "validation_infrastructure_failure"
                },
                "Worker-owned validation could not complete because its process or duration infrastructure failed.",
                None,
                true,
                "Resume validation from the persisted graph after restoring execution capacity; do not mutate source solely to repair an infrastructure failure.",
            ));
        }
        let decision = self.reconcile_active_phase("required validation failed")?;
        if !matches!(decision, PhaseDecision::Transition(ExecutionPhase::Repair))
            && self.phases.active() != ExecutionPhase::Repair
        {
            return Err(anyhow!(HostedInvariantFailure::new(
                "validation_repair_transition_invalid",
                "failed validation must enter repair",
            )));
        }
        let diagnostics = failures
            .iter()
            .map(|failure| {
                format!(
                    "Gate {} (`{}`) failed:\n{}",
                    failure.id,
                    failure.command,
                    truncate_text(&failure.output, 12_000)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        self.run_session(
            &format!(
                "Repair validation attempt {attempt} for RustGrid ticket {}. Inspect the current diff and make the smallest correct changes needed for these failures. Do not commit, push, create branches, or open pull requests.\n\n{diagnostics}",
                self.manifest.ticket_key
            ),
            true,
        )
    }

    pub(super) fn run_session(
        &mut self,
        prompt: &str,
        allow_budget_handoff: bool,
    ) -> Result<ImplementationOutcome> {
        let mut initial = json!({"role": "user", "content": prompt});
        let mut turns = VecDeque::<Vec<Value>>::new();
        let mut registration_attempt = 0;
        let mut previous_context_phase = None;
        loop {
            self.ensure_active_or_checkpoint_cancellation()?;
            if let Some(outcome) = self.prepare_next_model_call(allow_budget_handoff)? {
                return Ok(outcome);
            }
            let artifact_repair = self.phases.active() == ExecutionPhase::ArtifactRepair;
            let impact_map_finalization = matches!(
                self.current_decision.as_ref(),
                Some(ExecutionDecision::ContinueDiscovery {
                    action: crate::hosted_orchestrator::DiscoveryAction::FinalizeImpactMap { .. },
                })
            );
            let planning_artifact_action = matches!(
                self.current_decision.as_ref(),
                Some(ExecutionDecision::ContinuePlanning {
                    action: crate::hosted_orchestrator::PlanningAction::BuildPlan { .. }
                        | crate::hosted_orchestrator::PlanningAction::RepairPlan { .. },
                })
            );
            let active_context_phase = self.phases.active();
            if previous_context_phase.is_some_and(|phase| phase != active_context_phase) {
                turns.clear();
            }
            previous_context_phase = Some(active_context_phase);
            initial["content"] = Value::String(if artifact_repair {
                compact_impact_map_repair_context(self.impact_map_failure.as_ref(), &self.notebook)
            } else if impact_map_finalization {
                compact_impact_map_finalization_context(&self.notebook)
            } else if planning_artifact_action {
                compact_implementation_plan_context(&self.notebook, self.current_decision.as_ref())
            } else if matches!(
                active_context_phase,
                ExecutionPhase::Implementation | ExecutionPhase::Repair
            ) {
                let context = serde_json::to_string(&self.implementation_start_context()?)?;
                let repair_context = if active_context_phase == ExecutionPhase::Repair {
                    truncate_text(prompt, 12_000)
                } else {
                    String::new()
                };
                format!(
                    "{repair_context}\n\nRustGrid implementation start context (authoritative):\n{context}"
                )
            } else {
                format!(
                    "{prompt}\n\nRustGrid worker notebook (authoritative compact continuation state):\n{}",
                    compact_notebook_for_phase(&self.notebook, self.phases.active())
                )
            });
            let mut input = vec![initial.clone()];
            if !artifact_repair {
                for turn in &turns {
                    input.extend(turn.iter().cloned());
                }
            }
            let active_phase = self.phases.active();
            let action_profile = ModelActionProfile::for_decision(
                active_phase,
                self.current_decision.as_ref(),
                u64::try_from(self.manifest.ai_gateway.maximum_output_tokens).unwrap_or_default(),
            );
            let mut request = json!({
                "model": self.manifest.ai_gateway.model,
                "input": input,
                "instructions": hosted_agent_instructions_for_decision(
                    active_phase,
                    self.current_decision.as_ref(),
                ),
                "max_output_tokens": action_profile.max_output_tokens,
                "reasoning": {"effort": action_profile.reasoning_effort},
                "tools": hosted_tools_for_action(active_phase, self.current_decision.as_ref()),
                "tool_choice": action_profile.tool_choice(),
                "parallel_tool_calls": false,
                "metadata": provider_request_metadata(
                    self.manifest.execution.execution_id,
                    self.manifest.ticket_key.as_str(),
                    "rustgrid-agent-hosted",
                    active_phase,
                    self.budget.resolved_model_call_budget,
                ),
                "store": false,
                "stream": false
            });
            fit_request_to_input_ceiling(
                &mut request,
                &initial,
                &mut turns,
                phase_request_input_ceiling(
                    active_phase,
                    usize::try_from(self.manifest.ai_gateway.maximum_input_tokens)
                        .unwrap_or_default(),
                ),
            )?;
            if let Some((node_id, target, policy, failure_category)) = active_mutation_fallback(
                self.current_decision.as_ref(),
            )
            .map(|(node_id, target, policy, failure)| {
                (node_id.clone(), target.clone(), policy, failure)
            }) {
                let rejected = self
                    .notebook
                    .mutation_diagnostics
                    .iter()
                    .rev()
                    .find(|diagnostic| diagnostic.target_path == target.target.path);
                let original_tool = rejected.map(|diagnostic| diagnostic.tool.clone());
                let rejected_payload_hash = rejected
                    .and_then(|diagnostic| diagnostic.rejected_mutation.as_ref())
                    .map(|rejected| rejected.payload_hash.clone());
                let repair_call_number = self
                    .notebook
                    .orchestration
                    .budget
                    .usage_for(&node_id)
                    .mutation_fallback_attempts;
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.mutation_repair_request_built",
                        "node_id": node_id,
                        "target_path": target.target.path,
                        "target_operation": target.target.effective_operation(),
                        "original_tool": original_tool,
                        "original_failure_category": failure_category,
                        "selected_fallback_policy": policy,
                        "permitted_tools": policy.permitted_tools(),
                        "forced_tool_choice": policy.forced_tool(),
                        "repair_call_number": repair_call_number,
                        "target_content_hash": target.target_content_hash,
                        "repository_fingerprint": target.repository_fingerprint,
                        "rejected_mutation_payload_hash": rejected_payload_hash,
                    }),
                    "mutation repair request built",
                );
                let preflight =
                    mutation_repair_request_preflight(self.current_decision.as_ref(), &request)
                        .expect("active mutation fallback has a repair preflight");
                if !preflight.passed() {
                    self.restore_mutation_repair_allowance(&node_id)?;
                    self.checkpoint_notebook(false)?;
                    bail!(
                        "mutation_repair_request_preflight_failed: {}",
                        serde_json::to_string(&preflight)?
                    );
                }
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.mutation_repair_request_preflight_passed",
                        "node_id": node_id,
                        "target_path": target.target.path,
                        "target_operation": target.target.effective_operation(),
                        "original_tool": original_tool,
                        "original_failure_category": failure_category,
                        "selected_fallback_policy": policy,
                        "permitted_tools": policy.permitted_tools(),
                        "forced_tool_choice": policy.forced_tool(),
                        "preflight": preflight,
                        "repair_call_number": repair_call_number,
                        "target_content_hash": target.target_content_hash,
                        "repository_fingerprint": target.repository_fingerprint,
                        "rejected_mutation_payload_hash": rejected_payload_hash,
                    }),
                    "mutation repair request preflight",
                );
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.mutation_tool_policy_enforced",
                        "node_id": node_id,
                        "target_path": target.target.path,
                        "target_operation": target.target.effective_operation(),
                        "original_tool": original_tool,
                        "original_failure_category": failure_category,
                        "selected_fallback_policy": policy,
                        "permitted_tools": policy.permitted_tools(),
                        "forced_tool_choice": policy.forced_tool(),
                        "repair_call_number": repair_call_number,
                        "target_content_hash": target.target_content_hash,
                        "repository_fingerprint": target.repository_fingerprint,
                        "rejected_mutation_payload_hash": rejected_payload_hash,
                    }),
                    "mutation tool policy enforcement",
                );
            }
            validate_provider_request_envelope(&request)?;
            let cost_admitted = constrain_request_to_cost_limit(&mut request, &self.cost_guard)?;
            if cost_admitted {
                validate_provider_request_envelope(&request)?;
            }
            let reservation = cost_admitted
                .then(|| self.reserve_graph_model_call(&request))
                .flatten();
            let Some(reservation) = reservation else {
                let admission_reason = if cost_admitted {
                    if self
                        .notebook
                        .orchestration
                        .budget
                        .total_model_calls
                        .saturating_add(
                            self.notebook
                                .orchestration
                                .budget
                                .total_model_calls_reserved,
                        )
                        >= self.notebook.orchestration.budget.mission.max_model_calls
                    {
                        "mission_model_call_budget_exhausted"
                    } else {
                        "repair_session_model_call_budget_exhausted"
                    }
                } else {
                    "repair_cost_preflight_rejected"
                };
                self.record_validation_repair_admission_rejection(admission_reason)?;
                let compact_discovery_finalization = matches!(
                    self.current_decision.as_ref(),
                    Some(ExecutionDecision::ContinueDiscovery {
                        action: crate::hosted_orchestrator::DiscoveryAction::FinalizeImpactMap { .. }
                            | crate::hosted_orchestrator::DiscoveryAction::RepairImpactMap { .. },
                    })
                );
                if compact_discovery_finalization
                    && self.accept_deterministic_impact_map_if_available(
                        "compact_finalization_cost_budget_exhausted",
                    )?
                {
                    turns.clear();
                    continue;
                }
                let compact_plan_finalization = matches!(
                    self.current_decision.as_ref(),
                    Some(ExecutionDecision::ContinuePlanning {
                        action: crate::hosted_orchestrator::PlanningAction::BuildPlan { .. }
                            | crate::hosted_orchestrator::PlanningAction::RepairPlan { .. },
                    })
                );
                if compact_plan_finalization
                    && self.accept_deterministic_implementation_plan_if_available(
                        "compact_planning_cost_budget_exhausted",
                    )?
                {
                    turns.clear();
                    continue;
                }
                let changed_paths =
                    completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
                if allow_budget_handoff && !changed_paths.is_empty() {
                    self.record_partial_reviewable_handoff(
                        crate::execution_graph::GuardrailReason::NodeBudgetExhausted,
                        "model-call preflight admission ended with a non-empty reviewable diff",
                    )?;
                } else if allow_budget_handoff && changed_paths.is_empty() {
                    self.append_execution_domain_event(
                        crate::execution_graph::ExecutionDomainEvent::GuardrailTriggered {
                            sequence: self.next_domain_event_sequence(),
                            reason: crate::execution_graph::GuardrailReason::NodeBudgetExhausted,
                            outcome: OrchestratedMissionOutcome::BlockedNoDiff,
                            detail: "model-call preflight admission ended before any reviewable repository change"
                                .into(),
                        },
                    )?;
                    self.finalize_guardrail_outcome(OrchestratedMissionOutcome::BlockedNoDiff)?;
                    self.persist_orchestration_checkpoint(
                        "model_preflight_blocked_no_diff",
                        false,
                    )?;
                    return Err(self.blocked_no_diff_failure());
                }
                self.checkpoint_notebook(false)?;
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.model_cost_preflight_stopped",
                        "phase": active_phase,
                        "estimated_cost_micros": self.cost_guard.estimated_cost_micros,
                        "hard_limit_micros": self.cost_guard.hard_limit_micros,
                        "model_calls_used": self.phases.total_calls(),
                        "source_mutation_observed": self.tool_usage.successful_writes > 0,
                        "changed_paths": changed_paths,
                        "resumable": true,
                    }),
                    "model cost preflight",
                );
                return Ok(ImplementationOutcome {
                    summary: "The next model call could not fit inside the hard estimated-cost envelope; preserved work is resumable.".into(),
                    budget_exhausted: true,
                    explicit_declaration: self.declaration.clone(),
                });
            };
            let call_phase = self.phases.active();
            let model_call = match self.phases.begin_graph_model_call() {
                Ok(model_call) => model_call,
                Err(error) => {
                    self.release_graph_model_call_reservation(&reservation);
                    return Err(error);
                }
            };
            let registration = ai_call_registration(
                self.manifest.execution.execution_id,
                self.api.execution_attempt,
                self.api.session_id()?,
                model_call.saturating_sub(1),
                call_phase,
                registration_attempt,
            );
            self.active_model_call_id = Some(registration.semantic_call_id.to_string());
            if let Err(error) = self.api.append_event(
                "progress",
                json!({
                    "step": "ai_gateway",
                    "status": "running",
                    "model_call": model_call,
                    "model": self.manifest.ai_gateway.model,
                    "phase": call_phase,
                    "phase_call": self.phases.phase_calls(call_phase),
                    "budget": self.budget_telemetry(),
                }),
            ) {
                self.phases.rollback_model_call(call_phase)?;
                self.release_graph_model_call_reservation(&reservation);
                return Err(error);
            }
            let execution_deadline = match hosted_execution_deadline(
                self.execution_started_at,
                Duration::from_secs(self.cost_guard.max_duration_seconds),
            ) {
                Ok(deadline) => deadline,
                Err(error) => {
                    self.phases.rollback_model_call(call_phase)?;
                    self.release_graph_model_call_reservation(&reservation);
                    return Err(error);
                }
            };
            let model_call_started = Instant::now();
            let response = match invoke_model(
                &self.api,
                request.clone(),
                &registration,
                Some(execution_deadline),
            ) {
                Ok(response) => {
                    registration_attempt = 0;
                    response
                }
                Err(error) => {
                    let http = error.downcast_ref::<HostedHttpError>();
                    let budget_disposition = http
                        .map(HostedHttpError::budget_disposition)
                        .unwrap_or(AiBudgetDisposition::Unknown);
                    if budget_disposition == AiBudgetDisposition::Restore {
                        self.phases.rollback_model_call(call_phase)?;
                        self.release_graph_model_call_reservation(&reservation);
                        if http.is_some_and(|failure| {
                            failure.failure_class() == AiFailureClass::RegistrationConflict
                        }) {
                            let registration_can_retry =
                                http.is_some_and(HostedHttpError::retryable_registration_failure);
                            let retryable = registration_can_retry
                                && registration_attempt + 1 < MAX_AI_REGISTRATION_ATTEMPTS;
                            let retries_exhausted = registration_can_retry && !retryable;
                            self.append_event_recoverable(
                                "progress",
                                json!({
                                    "event_type": if retryable {
                                        "execution.ai.registration_retry"
                                    } else {
                                        "execution.ai.registration_failure"
                                    },
                                    "semantic_call_id": registration.semantic_call_id,
                                    "call_index": model_call.saturating_sub(1),
                                    "execution_attempt": self.api.execution_attempt,
                                    "worker_session_id": self.api.session_id()?,
                                    "failure_stage": "request_registration",
                                    "rustgrid_gateway_status": http
                                        .and_then(HostedHttpError::rustgrid_gateway_status),
                                    "upstream_provider_status": Value::Null,
                                    "provider_contacted": false,
                                    "call_budget_consumed": false,
                                    "reservation_state": http
                                        .and_then(HostedHttpError::reservation_state),
                                    "reservation_reconciliation_state": http
                                        .and_then(
                                            HostedHttpError::reservation_reconciliation_state
                                        ),
                                    "reason": http
                                        .and_then(
                                            HostedHttpError::reservation_reconciliation_state
                                        )
                                        .unwrap_or("failed_before_dispatch"),
                                    "retryable": retryable,
                                    "registration_attempt": if retryable {
                                        registration_attempt.saturating_add(1)
                                    } else {
                                        registration_attempt
                                    },
                                    "registration_attempts_exhausted": retries_exhausted,
                                    "message": retries_exhausted.then_some(
                                        "The AI request could not be registered after 3 attempts. No provider call, model budget, or actual cost was consumed."
                                    ),
                                    "budget": self.budget_telemetry(),
                                    "notebook": self.notebook,
                                }),
                                "AI request registration failure telemetry",
                            );
                            if retryable {
                                sleep_before_execution_retry(
                                    self.api.clock.as_ref(),
                                    Some(execution_deadline),
                                    registration_retry_delay(
                                        registration_attempt,
                                        registration.semantic_call_id,
                                    ),
                                    "AI request registration retry",
                                )?;
                                registration_attempt = registration_attempt.saturating_add(1);
                                continue;
                            }
                        } else if let Some(failure) = http.filter(|failure| {
                            failure.failure_class() == AiFailureClass::ProviderValidation
                        }) {
                            self.append_event_recoverable(
                                "progress",
                                provider_rejected_event(
                                    failure,
                                    &registration,
                                    self.api.execution_attempt,
                                    model_call,
                                    self.manifest.ai_gateway.model.as_str(),
                                    self.phases.total_calls(),
                                    self.budget_telemetry(),
                                    json!(&self.notebook),
                                ),
                                "AI provider rejection telemetry",
                            );
                        }
                    } else {
                        let actual_cost_micros =
                            http.and_then(|failure| failure.actual_cost_micros);
                        self.observe_failed_model_cost(
                            &reservation,
                            &request,
                            actual_cost_micros,
                            model_call_started.elapsed(),
                        );
                    }
                    let exhaustion_reason = ai_budget_exhaustion_reason(&error);
                    let changed_paths =
                        completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
                    if allow_budget_handoff
                        && !changed_paths.is_empty()
                        && exhaustion_reason.is_some()
                    {
                        let exhaustion_reason = exhaustion_reason.unwrap_or_default();
                        self.record_partial_reviewable_handoff(
                            crate::execution_graph::GuardrailReason::NodeBudgetExhausted,
                            "the provider budget ended with a non-empty reviewable diff",
                        )?;
                        let summary = format!(
                            "The implementation model stopped after RustGrid reported `{exhaustion_reason}` with {} changed path(s). The work remains resumable and requires independent completion evaluation.",
                            changed_paths.len()
                        );
                        self.api.append_event(
                            "message",
                            json!({
                                "step": "ai_gateway",
                                "status": "budget_handoff",
                                "exhaustion_reason": exhaustion_reason,
                                "model_calls_used": self.phases.total_calls(),
                                "phase": self.phases.active(),
                                "changed_paths": changed_paths,
                                "summary": summary
                            }),
                        )?;
                        return Ok(ImplementationOutcome {
                            summary,
                            budget_exhausted: true,
                            explicit_declaration: self.declaration.clone(),
                        });
                    }
                    let code = http
                        .map(HostedHttpError::effective_code)
                        .unwrap_or("ai_gateway_request_failed");
                    return Err(self.execution_failure(
                        code,
                        http.map(HostedHttpError::terminal_message)
                            .unwrap_or("The hosted model call failed."),
                        Some(&error),
                        true,
                        http.map(HostedHttpError::recommended_action).unwrap_or(
                            "Retry from the persisted phase and notebook after resolving the reported cause.",
                        ),
                    ));
                }
            };
            self.record_cache_observability(&request, &response);
            self.observe_model_cost(
                &reservation,
                &request,
                &response,
                model_call_started.elapsed(),
            )?;
            let output = response
                .get("output")
                .and_then(Value::as_array)
                .context("AI gateway response has no output array")?;
            let mut turn = Vec::new();
            let mut function_calls = Vec::new();
            let mut summary = String::new();
            for item in output {
                match item.get("type").and_then(Value::as_str) {
                    Some("function_call") => {
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .context("AI function call has no call_id")?;
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .context("AI function call has no name")?;
                        let arguments = item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .context("AI function call has no arguments")?;
                        if call_id.len() > 200 || name.len() > 64 || arguments.len() > 512 * 1024 {
                            bail!("AI function call exceeds the hosted tool contract");
                        }
                        turn.push(json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments,
                            "status": "completed"
                        }));
                        function_calls.push((
                            call_id.to_owned(),
                            name.to_owned(),
                            arguments.to_owned(),
                        ));
                    }
                    Some("message") => {
                        let content = sanitized_message_content(item);
                        for value in &content {
                            if let Some(text) = value.get("text").and_then(Value::as_str) {
                                if !summary.is_empty() {
                                    summary.push('\n');
                                }
                                summary.push_str(text);
                            }
                        }
                        if !content.is_empty() {
                            turn.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": content
                            }));
                        }
                    }
                    _ => {}
                }
            }
            let policy_violation = if function_calls.is_empty() {
                mutation_tool_policy_violation(self.current_decision.as_ref(), "<none>")
            } else {
                function_calls.iter().find_map(|(_, name, _)| {
                    mutation_tool_policy_violation(self.current_decision.as_ref(), name)
                })
            };
            if let Some(violation) = policy_violation {
                let active_context = active_mutation_fallback(self.current_decision.as_ref()).map(
                    |(_, target, _, failure)| {
                        (
                            target.target.effective_operation(),
                            target.target_content_hash.clone(),
                            target.repository_fingerprint.clone(),
                            failure,
                        )
                    },
                );
                let diagnostic = self
                    .notebook
                    .mutation_diagnostics
                    .iter()
                    .rev()
                    .find(|diagnostic| diagnostic.target_path == violation.target_path)
                    .cloned();
                let repair_call_number = self
                    .notebook
                    .orchestration
                    .budget
                    .usage_for(&violation.node_id)
                    .mutation_fallback_attempts;
                self.restore_mutation_repair_allowance(&violation.node_id)?;
                let repeated_strategy = self
                    .notebook
                    .mutation_diagnostics
                    .iter()
                    .rev()
                    .find(|diagnostic| diagnostic.target_path == violation.target_path)
                    .is_some_and(|diagnostic| {
                        diagnostic
                            .strategy_fingerprint
                            .as_ref()
                            .is_some_and(|strategy| {
                                strategy.tool == violation.received_tool
                                    && strategy.fallback_policy == violation.active_policy
                                    && strategy.failure_category == diagnostic.failure_category
                            })
                            && diagnostic.repository_fingerprint
                                == self.notebook.repository_fingerprint
                    });
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.mutation_tool_policy_violation",
                        "node_id": violation.node_id,
                        "target_path": violation.target_path,
                        "target_operation": active_context.as_ref().map(|context| &context.0),
                        "original_tool": diagnostic.as_ref().map(|diagnostic| &diagnostic.tool),
                        "original_failure_category": active_context.as_ref().map(|context| context.3),
                        "active_policy": violation.active_policy,
                        "expected_tools": violation.expected_tools,
                        "forced_tool_choice": violation.active_policy.forced_tool(),
                        "received_tool": violation.received_tool,
                        "repair_call_number": repair_call_number,
                        "target_content_hash": active_context.as_ref().and_then(|context| context.1.as_deref()),
                        "repository_fingerprint": active_context.as_ref().map(|context| context.2.as_str()),
                        "repository_touched": false,
                        "repository_write_attempt_consumed": false,
                        "mutation_repair_allowance_consumed": false,
                        "provider_contract_violation": true,
                    }),
                    "mutation tool policy violation",
                );
                if repeated_strategy {
                    self.append_event_recoverable(
                        "progress",
                        json!({
                            "event_type": "worker.repeated_mutation_strategy_rejected",
                            "node_id": violation.node_id,
                            "target_path": violation.target_path,
                            "target_operation": active_context.as_ref().map(|context| &context.0),
                            "original_tool": diagnostic.as_ref().map(|diagnostic| &diagnostic.tool),
                            "original_failure_category": active_context.as_ref().map(|context| context.3),
                            "received_tool": violation.received_tool,
                            "active_policy": violation.active_policy,
                            "permitted_tools": violation.active_policy.permitted_tools(),
                            "forced_tool_choice": violation.active_policy.forced_tool(),
                            "repair_call_number": repair_call_number,
                            "target_content_hash": active_context.as_ref().and_then(|context| context.1.as_deref()),
                            "repository_fingerprint": self.notebook.repository_fingerprint,
                            "material_context_change": false,
                        }),
                        "repeated mutation strategy rejection",
                    );
                }
                if function_calls.is_empty() {
                    turn.push(json!({
                        "role": "user",
                        "content": format!(
                            "RustGrid provider-contract guardrail: invoke exactly `{}` for `{}`; the rejected mutation was not applied.",
                            violation.active_policy.forced_tool().unwrap_or("the forced repair tool"),
                            violation.target_path
                        )
                    }));
                } else {
                    for (call_id, _, _) in &function_calls {
                        turn.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": serde_json::to_string(&json!({
                                "ok": false,
                                "error_code": "mutation_tool_policy_violation",
                                "error": violation.to_string(),
                                "repository_touched": false,
                                "repair_allowance_consumed": false,
                            }))?,
                        }));
                    }
                }
                turns.push_back(turn);
                compact_hosted_turns(&mut turns);
                self.checkpoint_notebook(false)?;
                continue;
            }
            if function_calls.is_empty() {
                if summary.trim().is_empty() {
                    bail!("AI gateway returned neither tool calls nor a final message");
                }
                if matches!(
                    self.phases.active(),
                    ExecutionPhase::Discovery | ExecutionPhase::ArtifactRepair
                ) && self.impact_map.is_none()
                    && let Ok((map, source)) =
                        recover_impact_map(None, Some(&summary), &self.notebook)
                {
                    self.accept_impact_map(
                        map,
                        source,
                        1.0,
                        Some(&anyhow!("record_impact_map was not invoked")),
                    )?;
                    turns.push_back(turn);
                    compact_hosted_turns(&mut turns);
                    continue;
                }
                let missing_artifact = match self.phases.active() {
                    ExecutionPhase::Discovery if self.impact_map.is_none() => {
                        Some("record the required implementation impact map")
                    }
                    ExecutionPhase::ArtifactRepair if self.impact_map.is_none() => {
                        Some("repair the impact map using only record_impact_map")
                    }
                    ExecutionPhase::Planning if self.implementation_plan.is_none() => {
                        Some("record the required machine-readable implementation plan")
                    }
                    ExecutionPhase::Implementation | ExecutionPhase::Repair => {
                        Some("produce the required target-bound mutation")
                    }
                    ExecutionPhase::DiffReview if self.declaration.is_none() => {
                        Some("record the required implementation declaration")
                    }
                    _ => None,
                };
                if let Some(required_action) = missing_artifact {
                    self.emit_guardrail(
                        "premature_final_response",
                        "continue_required_phase",
                        &format!(
                            "A final response cannot bypass orchestration; {required_action}."
                        ),
                    )?;
                    turn.push(json!({
                        "role": "user",
                        "content": format!(
                            "RustGrid guardrail: do not finish yet; {required_action} using the required structured tool."
                        )
                    }));
                    if matches!(
                        self.phases.active(),
                        ExecutionPhase::Implementation | ExecutionPhase::Repair
                    ) {
                        self.record_active_target_failure(
                            crate::execution_graph::FailureCategory::ToolRecoverable,
                            "MutationNotProduced: the target-bound model response contained no mutation tool call",
                        )?;
                        self.reconcile_authoritative_target_state()?;
                        self.reconcile_execution_and_apply()?;
                        self.observe_implementation_progress()?;
                    }
                    turns.push_back(turn);
                    compact_hosted_turns(&mut turns);
                    continue;
                }
                self.api.append_event(
                    "message",
                    json!({
                        "step": "ai_gateway",
                        "status": "completed",
                        "model_calls_used": self.phases.total_calls(),
                        "phase": self.phases.active(),
                        "summary": truncate_text(&summary, 4_000)
                    }),
                )?;
                return Ok(ImplementationOutcome {
                    summary: truncate_text(&summary, 16_000),
                    budget_exhausted: false,
                    explicit_declaration: self.declaration.clone(),
                });
            }
            if let Some((node_id, _, _, _)) = active_mutation_fallback(
                self.current_decision.as_ref(),
            )
            .map(|(node_id, target, policy, failure)| {
                (node_id.clone(), target.clone(), policy, failure)
            }) {
                self.consume_pending_mutation_repair_allowance(&node_id)?;
            }
            let mut mutation_preflight_halt = None;
            for (call_id, name, arguments) in function_calls {
                self.ensure_active_or_checkpoint_cancellation()?;
                if name == "record_impact_map" {
                    self.api.append_event(
                        "progress",
                        impact_map_artifact_attempt_payload(self.phases.active()),
                    )?;
                }
                let target = tool_target(&arguments);
                let change_id = tool_change_id(&arguments);
                let before_sha256 = target
                    .as_deref()
                    .and_then(|path| repo_file_sha256(&self.repo.root, path));
                let intended_change_sha256 =
                    is_source_mutation_tool(&name).then(|| tool_intent_sha256(&name, &arguments));
                let inspected_before = self.notebook.files_inspected.len();
                let read_ranges_before = self.notebook.read_ranges_inspected.len();
                let searches_before = self.notebook.searches_completed.len();
                let file_evidence_before = self.notebook.orchestration.evidence.files.len();
                let failed_reads_before = self.tool_usage.failed_reads;
                let mut progress_class = ToolProgressClass::Neutral;
                let mut progress_detail = String::new();
                let mut verified_repository_progress = false;
                let result = match self.execute_tool(&name, &arguments) {
                    Ok(output) => {
                        if successful_tool_updates_last_action(
                            &name,
                            file_evidence_before,
                            self.notebook.orchestration.evidence.files.len(),
                        ) {
                            self.last_successful_action = json!({
                                "model_call": self.phases.total_calls(),
                                "phase": self.phases.active(),
                                "tool": name,
                                "target": target,
                            });
                        }
                        if is_source_mutation_tool(&name) {
                            let mut attempt = WriteAttemptRecord {
                                attempt_index: self.notebook.write_attempts.len(),
                                change_id: change_id.clone().unwrap_or_default(),
                                target: target.clone().unwrap_or_default(),
                                tool: name.clone(),
                                status: WriteAttemptStatus::Applied,
                                error_code: None,
                                match_count: None,
                                intended_change_sha256: intended_change_sha256.clone(),
                                before_sha256,
                                after_sha256: target
                                    .as_deref()
                                    .and_then(|path| repo_file_sha256(&self.repo.root, path)),
                            };
                            let changed_paths = completion_changed_paths(
                                self.repo,
                                &self.manifest.github.base_sha,
                            )?;
                            let target_was_modified = attempt_modified_target(&attempt)
                                && changed_paths.contains(&attempt.target);
                            if !target_was_modified {
                                attempt.status = WriteAttemptStatus::NoChange;
                            }
                            let mutation_before_hash = attempt.before_sha256.clone();
                            let mutation_after_hash = attempt.after_sha256.clone();
                            self.notebook.write_attempts.push(attempt);
                            if target_was_modified {
                                self.tool_usage.successful_writes =
                                    self.tool_usage.successful_writes.saturating_add(1);
                                self.last_source_progress_call = self.phases.total_calls();
                                self.cost_guard.repository_progress_score += 1.0;
                                verified_repository_progress = true;
                                progress_class = ToolProgressClass::Productive;
                                progress_detail = "verified repository mutation".into();
                            } else {
                                progress_class = ToolProgressClass::Duplicate;
                                progress_detail =
                                    "mutation tool returned successfully but repository content did not change"
                                        .into();
                                self.record_active_target_failure(
                                    crate::execution_graph::FailureCategory::ToolRecoverable,
                                    "MutationNotProduced: mutation tool completed without an attributable target change",
                                )?;
                            }
                            self.diff_reviewed = false;
                            self.diff_review_cursor = 0;
                            self.diff_review_digest = None;
                            self.declaration = None;
                            for failure in &mut self.tool_failures {
                                if target_was_modified
                                    && !failure.recovered
                                    && failure.target.is_some()
                                    && failure.target == target
                                    && (failure.change_id == change_id
                                        || failure.intended_change_sha256 == intended_change_sha256
                                        || matches!(
                                            name.as_str(),
                                            "write_file" | "rewrite_small_file"
                                        ))
                                {
                                    failure.recovered = true;
                                    failure.reconciliation = FailureReconciliation::Superseded;
                                    failure.recovery = Some(IntendedChangeRecovery {
                                        recovered: true,
                                        method: "later_successful_target_write".into(),
                                        evidence: vec![format!(
                                            "A later successful {} modified {}.",
                                            name,
                                            target.as_deref().unwrap_or("the same target")
                                        )],
                                    });
                                }
                            }
                            if target_was_modified && let Some(target_path) = target.as_deref() {
                                self.record_active_target_mutation_produced(
                                    target_path,
                                    mutation_before_hash,
                                    mutation_after_hash,
                                )?;
                                self.verify_active_target_state()?;
                            }
                            self.reconcile_authoritative_target_state()?;
                            self.reconcile_active_phase(
                                "successful mutation reconciled against remaining planned targets",
                            )?;
                        } else if matches!(
                            name.as_str(),
                            "read_file" | "read_files" | "related_tests"
                        ) {
                            let (class, detail) = successful_read_progress(
                                &name,
                                self.notebook.files_inspected.len() > inspected_before,
                                self.notebook.read_ranges_inspected.len() > read_ranges_before,
                                self.notebook.searches_completed.len() > searches_before,
                                self.tool_usage.failed_reads > failed_reads_before,
                            );
                            progress_class = class;
                            progress_detail = detail.into();
                        } else if name == "search_text" {
                            if self.notebook.searches_completed.len() > searches_before {
                                progress_class = ToolProgressClass::Productive;
                                progress_detail = "new targeted repository search completed".into();
                            } else {
                                progress_class = ToolProgressClass::Duplicate;
                                progress_detail =
                                    "repository search did not add new evidence".into();
                            }
                        } else if name == "report_write_progress" {
                            let semantics = informational_write_progress_semantics();
                            progress_class = semantics.0;
                            verified_repository_progress = semantics.1;
                            progress_detail =
                                "informational progress report; repository state was not changed"
                                    .into();
                        } else {
                            progress_class = ToolProgressClass::Productive;
                            progress_detail =
                                "orchestration artifact or focused action completed".into();
                        }
                        json!({"ok": true, "output": truncate_text(&output, MAX_TOOL_OUTPUT_BYTES)})
                    }
                    Err(error) => {
                        if name == "record_impact_map"
                            && matches!(
                                self.phases.active(),
                                ExecutionPhase::Discovery | ExecutionPhase::ArtifactRepair
                            )
                        {
                            match recover_impact_map(
                                Some(&arguments),
                                Some(&summary),
                                &self.notebook,
                            ) {
                                Ok((map, source)) => {
                                    let output =
                                        self.accept_impact_map(map, source, 1.0, Some(&error))?;
                                    json!({
                                        "ok": true,
                                        "output": output,
                                        "recovered": true,
                                        "semantic_status": ArtifactSemanticStatus::Sufficient,
                                        "serialization_status": self.notebook.impact_map_artifact.serialization_status,
                                        "persistence_status": self.notebook
                                            .impact_map_artifact
                                            .persistence_status,
                                    })
                                }
                                Err(recovery_error) => {
                                    if let Some((fallback, confidence)) = impact_map::fallback(
                                        &self.notebook.files_inspected,
                                        &self.notebook.searches_completed,
                                        &self.notebook.acceptance_criteria,
                                        &self.notebook.blocking_unknowns,
                                    )
                                    .filter(|(_, confidence)| {
                                        *confidence >= impact_map_fallback_threshold(self.manifest)
                                    }) {
                                        let output = self.accept_impact_map(
                                            fallback,
                                            ArtifactSource::OrchestratorFallback,
                                            confidence,
                                            Some(&error),
                                        )?;
                                        self.append_event_recoverable("progress", json!({
                                            "event_type":"worker.impact_map_fallback_accepted",
                                            "artifact_source":"orchestrator_fallback",
                                            "confidence":confidence,
                                            "process_health":"healthy",
                                            "mission_outcome":"continuing",
                                            "tool_schema_version":IMPACT_MAP_SCHEMA_VERSION,
                                            "tool_schema_sha256":impact_map::schema_sha256(),
                                            "validator_schema_version":IMPACT_MAP_SCHEMA_VERSION,
                                            "validator_schema_sha256":impact_map::schema_sha256(),
                                        }), "impact-map deterministic fallback");
                                        json!({"ok":true,"output":output,"recovered":true,"artifact_source":"orchestrator_fallback","confidence":confidence})
                                    } else {
                                        let invalid_payload = json_object_from_text(&arguments)
                                            .unwrap_or(Value::Null);
                                        let validation_errors = impact_map::normalize(
                                            &invalid_payload,
                                            &self.notebook.files_inspected,
                                            &self.notebook.searches_completed,
                                            &self.notebook.acceptance_criteria,
                                        )
                                        .err()
                                        .unwrap_or_default();
                                        let invalid_payload_shape =
                                            impact_map::safe_shape(&invalid_payload);
                                        let semantic_status =
                                            invalid_impact_map_semantic_status(&invalid_payload);
                                        let failure_layer = if invalid_payload.is_null() {
                                            ArtifactFailureLayer::GatewayToolArgumentParsing
                                        } else if semantic_status == ArtifactSemanticStatus::Partial
                                        {
                                            ArtifactFailureLayer::ArtifactSemanticValidation
                                        } else {
                                            ArtifactFailureLayer::WorkerToolSchemaValidation
                                        };
                                        let mut failure = classify_impact_map_failure(&error);
                                        failure.code = "impact_map_schema_mismatch";
                                        failure.safe_error = serde_json::to_string(&json!({
                                            "code":"impact_map_schema_mismatch",
                                            "errors":validation_errors,
                                        }))
                                        .unwrap_or_else(|_| "impact_map_schema_mismatch".into());
                                        failure.errors = validation_errors.clone();
                                        failure.invalid_payload = invalid_payload.clone();
                                        failure.invalid_payload_shape =
                                            invalid_payload_shape.clone();
                                        failure.failure_layer = failure_layer;
                                        let safe_error = failure.safe_error.clone();
                                        self.impact_map_failure = Some(failure);
                                        self.notebook.impact_map_invalid_payload =
                                            Some(invalid_payload.clone());
                                        self.notebook.impact_map_artifact = ArtifactCheckpoint {
                                            artifact: "impact_map".into(),
                                            semantic_status,
                                            serialization_status:
                                                ArtifactSerializationStatus::Invalid,
                                            persistence_status:
                                                ArtifactPersistenceStatus::PendingRetry,
                                            artifact_sha256: None,
                                            model_call_index: Some(self.phases.total_calls()),
                                            phase: self.phases.active(),
                                            safe_error: Some(safe_error.clone()),
                                            normalization_metadata: None,
                                            artifact_source: None,
                                            confidence: None,
                                            failure_layer: Some(failure_layer),
                                            validation_errors: validation_errors.clone(),
                                            invalid_payload_shape: Some(
                                                invalid_payload_shape.clone(),
                                            ),
                                        };
                                        self.append_event_recoverable(
                                        "progress",
                                        json!({
                                            "event_type": "worker.artifact_repair_required",
                                            "artifact": "impact_map",
                                            "code": self.impact_map_failure.as_ref().map(
                                                |failure| failure.code
                                            ),
                                            "semantic_status": semantic_status,
                                            "serialization_status": ArtifactSerializationStatus::Invalid,
                                            "failure_layer": failure_layer,
                                            "validation_errors": validation_errors,
                                            "invalid_payload_shape": invalid_payload_shape,
                                            "tool_schema_version": IMPACT_MAP_SCHEMA_VERSION,
                                            "tool_schema_sha256": impact_map::schema_sha256(),
                                            "validator_schema_version": IMPACT_MAP_SCHEMA_VERSION,
                                            "validator_schema_sha256": impact_map::schema_sha256(),
                                            "process_health":"healthy",
                                            "mission_outcome":"blocked",
                                            "persistence_status":
                                                ArtifactPersistenceStatus::PendingRetry,
                                            "recoverable": true,
                                            "action": "repair_artifact",
                                            "safe_error": safe_error,
                                            "recovery_error": truncate_text(
                                                &recovery_error.to_string(),
                                                2_000
                                            ),
                                            "resume_phase": "artifact_repair",
                                            "notebook": self.notebook,
                                            "checkpoint": self.notebook_checkpoint_metadata(None),
                                        }),
                                        "impact-map repair checkpoint",
                                    );
                                        if self.phases.active() == ExecutionPhase::Discovery {
                                            self.record_discovery_failure(&safe_error)?;
                                            self.reconcile_execution_and_apply()?;
                                        }
                                        json!({
                                            "ok": false,
                                            "error": safe_error,
                                            "recoverable": true,
                                            "resume_phase": "artifact_repair",
                                        })
                                    }
                                }
                            }
                        } else if name == "record_implementation_plan"
                            && self.phases.active() == ExecutionPhase::Planning
                        {
                            self.record_planning_failure(&arguments)?;
                            let exhausted = self.phases.phase_calls(ExecutionPhase::Planning)
                                >= self.phases.phase_limit(ExecutionPhase::Planning);
                            if exhausted
                                && self.accept_deterministic_implementation_plan_if_available(
                                    "planning_repair_call_did_not_produce_a_valid_plan",
                                )?
                            {
                                json!({
                                    "ok": true,
                                    "output": "accepted deterministic implementation plan after bounded repair",
                                    "recovered": true,
                                    "artifact_source": "orchestrator_fallback",
                                })
                            } else {
                                json!({
                                    "ok": false,
                                    "error": truncate_text(&format!("{error:#}"), 4_000),
                                    "recoverable": true,
                                    "next_action": "repair_plan",
                                })
                            }
                        } else if let Some(preflight) =
                            error.downcast_ref::<MutationPreflightError>()
                        {
                            if preflight.code == "target_already_applied" {
                                let next_target = self
                                    .current_implementation_target()
                                    .map(|target| target.path);
                                self.append_event_recoverable(
                                    "progress",
                                    json!({
                                        "event_type": "worker.target_already_applied",
                                        "change_id": preflight.change_id,
                                        "target": preflight.target,
                                        "next_target": next_target.clone(),
                                        "mutation_attempted": false,
                                        "unresolved_failure_recorded": false,
                                    }),
                                    "already-applied target reconciliation",
                                );
                                json!({
                                    "ok": false,
                                    "error": preflight.message,
                                    "error_code": preflight.code,
                                    "repair_strategy": preflight.repair_strategy,
                                    "mutation_attempted": false,
                                    "unresolved_failure_recorded": false,
                                    "next_target": next_target,
                                })
                            } else {
                                let decision = record_mutation_preflight_rejection(
                                    &mut self.notebook,
                                    &mut self.tool_usage,
                                    preflight,
                                );
                                self.append_event_recoverable(
                                    "progress",
                                    json!({
                                        "event_type": "worker.mutation_preflight_rejected",
                                        "change_id": preflight.change_id,
                                        "target": preflight.target,
                                        "failure_code": preflight.code,
                                        "plan_revision": self.notebook.revision,
                                        "retryable_with_same_plan": false,
                                        "repair_strategy": preflight.repair_strategy,
                                        "mutation_attempted": false,
                                        "mutation_preflight_failed": true,
                                        "circuit_breaker_open": decision.repeated,
                                        "orchestration_halted": decision.halt_orchestration,
                                    }),
                                    "mutation preflight rejection",
                                );
                                mark_mutation_preflight_blocker(
                                    &mut self.write_blocker,
                                    &preflight.target,
                                );
                                mutation_preflight_halt = Some(format!(
                                    "Implementation paused after non-retryable mutation preflight rejection `{}` for `{}`. Repair the persisted plan metadata and resume without repeating discovery or planning.",
                                    preflight.code, preflight.target
                                ));
                                json!({
                                    "ok": false,
                                    "error": preflight.message,
                                    "error_code": preflight.code,
                                    "retryable_with_same_plan": false,
                                    "repair_strategy": preflight.repair_strategy,
                                    "mutation_attempted": false,
                                    "mutation_preflight_failed": true,
                                    "circuit_breaker_open": decision.repeated,
                                })
                            }
                        } else {
                            let mutation_application =
                                error.downcast_ref::<MutationApplicationError>().cloned();
                            let error = truncate_text(&format!("{error:#}"), 4_000);
                            if is_source_mutation_tool(&name) {
                                let (error_code, match_count) =
                                    mutation_application.as_ref().map_or_else(
                                        || classify_write_failure(&error),
                                        |application| {
                                            (application.failure.as_str().to_owned(), None)
                                        },
                                    );
                                let proposed_content_hash =
                                    serde_json::from_str::<Value>(&arguments).ok().and_then(
                                        |value| {
                                            value
                                                .get("content")
                                                .and_then(Value::as_str)
                                                .map(sha256_text)
                                        },
                                    );
                                let validation_repair_no_change =
                                    mutation_application.as_ref().is_some_and(|application| {
                                        application.failure
                                            == MutationApplicationFailure::MutationProducedNoChange
                                    }) && self.record_active_validation_repair_no_change(
                                        &error,
                                        before_sha256.as_deref(),
                                        proposed_content_hash.as_deref(),
                                    )?;
                                self.tool_usage.failed_writes =
                                    self.tool_usage.failed_writes.saturating_add(1);
                                self.tool_usage.write_execution_failures =
                                    self.tool_usage.write_execution_failures.saturating_add(1);
                                let attempt_index = self.notebook.write_attempts.len();
                                let attempt = WriteAttemptRecord {
                                    attempt_index,
                                    change_id: change_id.clone().unwrap_or_default(),
                                    target: target.clone().unwrap_or_default(),
                                    tool: name.clone(),
                                    status: if validation_repair_no_change {
                                        WriteAttemptStatus::NoChange
                                    } else {
                                        WriteAttemptStatus::Failed
                                    },
                                    error_code: Some(error_code.clone()),
                                    match_count,
                                    intended_change_sha256: intended_change_sha256.clone(),
                                    before_sha256,
                                    after_sha256: None,
                                };
                                self.notebook.write_attempts.push(attempt);
                                if let Some(application) = mutation_application.as_ref() {
                                    self.record_mutation_application_diagnostic(
                                        &name,
                                        &arguments,
                                        target.as_deref().unwrap_or_default(),
                                        application,
                                    )?;
                                    if application.failure
                                        == MutationApplicationFailure::RepositoryChangedSinceContext
                                    {
                                        let _ = self.prepare_active_target_context()?;
                                    }
                                }
                                self.tool_failures.push(ToolFailureRecord {
                                    attempt_index,
                                    change_id,
                                    tool: name.clone(),
                                    target: target.clone(),
                                    error_code,
                                    match_count,
                                    error: error.clone(),
                                    recovered: false,
                                    reconciliation: FailureReconciliation::StillUnresolved,
                                    recovery: None,
                                    intended_change_sha256: intended_change_sha256.clone(),
                                });
                                if !validation_repair_no_change {
                                    self.record_active_target_failure_with_code(
                                        crate::execution_graph::FailureCategory::MutationConflict,
                                        mutation_application
                                            .as_ref()
                                            .map(|application| application.failure.as_str()),
                                        &error,
                                    )?;
                                }
                                self.reconcile_authoritative_target_state()?;
                                self.reconcile_active_phase(
                                    "source-changing tool failure reconciled against repository state",
                                )?;
                            }
                            json!({"ok": false, "error": error})
                        }
                    }
                };
                if result["ok"] != true {
                    let error = result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("tool operation failed");
                    let error_code = result
                        .get("error_code")
                        .and_then(Value::as_str)
                        .unwrap_or("tool_operation_failed");
                    progress_detail = truncate_text(error, 1_000);
                    if error_code == "localized_discovery_complete" {
                        progress_class = ToolProgressClass::ActionRedirected;
                        progress_detail =
                            "localized discovery evidence is complete; finalize the impact map"
                                .into();
                        self.append_event_recoverable(
                            "progress",
                            json!({
                                "event_type": "worker.discovery_action_redirected",
                                "from_action": "inspect_repository",
                                "to_action": "finalize_impact_map",
                                "reason_code": "localized_discovery_complete",
                                "repository_failure": false,
                                "tool_failure": false,
                            }),
                            "discovery action redirection",
                        );
                    } else if matches!(name.as_str(), "read_file" | "read_files" | "related_tests")
                    {
                        if name != "read_files" {
                            self.tool_usage.failed_reads =
                                self.tool_usage.failed_reads.saturating_add(1);
                        }
                        progress_class = read_error_progress_class(error);
                    } else if name == "search_text" {
                        progress_class = if error_code == "duplicate_search" {
                            ToolProgressClass::Duplicate
                        } else {
                            ToolProgressClass::RecoverableFailure
                        };
                    } else if name == "record_implementation_plan" {
                        progress_class = ToolProgressClass::RecoverableFailure;
                    } else if is_source_mutation_tool(&name) {
                        if result["error_code"] == "target_already_applied" {
                            progress_class = ToolProgressClass::Duplicate;
                            progress_detail =
                                "target already applied; continue with next remaining target"
                                    .into();
                        } else {
                            progress_class = ToolProgressClass::RecoverableFailure;
                        }
                    } else {
                        progress_class = ToolProgressClass::BlockingFailure;
                    }
                }
                self.record_tool_progress(
                    &name,
                    target.clone(),
                    progress_class,
                    progress_detail,
                    verified_repository_progress,
                );
                if let Err(error) = self.checkpoint_notebook(verified_repository_progress) {
                    self.append_event_recoverable(
                        "progress",
                        json!({
                            "event_type": "worker.notebook_persistence_failed",
                            "phase": self.phases.active(),
                            "recoverable": true,
                            "action": "retry_or_continue",
                            "safe_error": truncate_text(&error.to_string(), 2_000),
                            "checkpoint": self.notebook_checkpoint_metadata(
                                self.notebook.impact_map_artifact.artifact_sha256.as_deref()
                            ),
                        }),
                        "notebook persistence warning",
                    );
                }
                let retrying_impact_map_persistence = name == "record_impact_map"
                    && self.notebook.impact_map_artifact.semantic_status
                        == ArtifactSemanticStatus::Sufficient
                    && self.notebook.impact_map_artifact.persistence_status
                        != ArtifactPersistenceStatus::Persisted;
                let mut event_notebook = self.notebook.clone();
                if retrying_impact_map_persistence {
                    event_notebook.impact_map_artifact.persistence_status =
                        ArtifactPersistenceStatus::Persisted;
                }
                self.api
                    .append_event(
                        "tool",
                        json!({
                            "event_type": "worker.authoritative_tool_checkpoint",
                            "tool": name,
                            "target": target,
                            "status": if result["ok"] == true { "completed" } else { "failed" },
                            "phase": self.phases.active(),
                            "model_call": self.phases.total_calls(),
                            "progress_class": progress_class,
                            "repository_progress": verified_repository_progress,
                            "usage": self.tool_usage,
                            "budget": self.budget_telemetry(),
                            "notebook": event_notebook,
                            "checkpoint": self.notebook_checkpoint_metadata(
                                self.notebook.impact_map_artifact.artifact_sha256.as_deref()
                            ),
                        }),
                    )
                    .context("could not persist authoritative hosted tool checkpoint")?;
                if retrying_impact_map_persistence {
                    self.notebook.impact_map_artifact.persistence_status =
                        ArtifactPersistenceStatus::Persisted;
                }
                turn.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": serde_json::to_string(&result)?
                }));
            }
            self.observe_implementation_progress()?;
            self.reconcile_authoritative_target_state()?;
            let phase_decision =
                if active_mutation_fallback(self.current_decision.as_ref()).is_some() {
                    PhaseDecision::Stay
                } else {
                    self.reconcile_active_phase(
                        "implementation turn reconciled against authoritative target state",
                    )?
                };
            if matches!(
                phase_decision,
                PhaseDecision::Transition(ExecutionPhase::Validation)
            ) || self.phases.active() == ExecutionPhase::Validation
            {
                self.phases.release_unused_implementation_capacity();
                return Ok(ImplementationOutcome {
                    summary: format!(
                        "Applied all {} planned target(s); continuing with worker-owned validation.",
                        self.notebook
                            .intended_changes
                            .iter()
                            .map(|change| change.targets.len())
                            .sum::<usize>()
                    ),
                    budget_exhausted: false,
                    explicit_declaration: self.declaration.clone(),
                });
            }
            if self.phases.active() == ExecutionPhase::DiffReview {
                return Ok(ImplementationOutcome {
                    summary: "Bounded validation repair produced a typed no-valid-repair result; preserving the applied diff for incomplete review.".into(),
                    budget_exhausted: true,
                    explicit_declaration: self.declaration.clone(),
                });
            }
            if let Some(summary) = mutation_preflight_halt {
                return Ok(ImplementationOutcome {
                    summary,
                    budget_exhausted: false,
                    explicit_declaration: self.declaration.clone(),
                });
            }
            turns.push_back(turn);
            compact_hosted_turns(&mut turns);
        }
    }
}

impl GatewayAgent<'_> {
    fn record_mutation_application_diagnostic(
        &mut self,
        tool: &str,
        rejected_payload: &str,
        target_path: &str,
        application: &MutationApplicationError,
    ) -> Result<()> {
        let repository_fingerprint =
            repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let (mutation_attempt, repair_attempt) = self
            .current_decision
            .as_ref()
            .and_then(ExecutionDecision::node_id)
            .map_or((0, 0), |node_id| {
                let mutation_attempt = self
                    .notebook
                    .orchestration
                    .graph
                    .as_ref()
                    .and_then(|graph| graph.node(node_id))
                    .map_or(0, |node| {
                        u32::try_from(node.attempts.len()).unwrap_or(u32::MAX)
                    });
                let repair_attempt = self
                    .notebook
                    .orchestration
                    .budget
                    .usage_for(node_id)
                    .mutation_fallback_attempts;
                (mutation_attempt, repair_attempt)
            });
        let node_id = self
            .current_decision
            .as_ref()
            .and_then(ExecutionDecision::node_id)
            .cloned();
        let normalized_paths = application
            .patch_validation
            .as_ref()
            .map_or_else(Vec::new, |validation| validation.normalized_paths.clone());
        let raw_patch_hash = application
            .raw_patch_sha256
            .clone()
            .or_else(|| Some(sha256_text(rejected_payload)));
        let target_operation = self
            .current_decision
            .as_ref()
            .and_then(|decision| match decision {
                ExecutionDecision::ExecuteTarget { target, .. } => {
                    Some(target.target.effective_operation())
                }
                ExecutionDecision::RepairTarget { context, .. } => {
                    Some(context.target.target.effective_operation())
                }
                _ => None,
            })
            .unwrap_or(crate::execution_graph::TargetOperation::ModifyExisting);
        let fallback_policy = self
            .current_decision
            .as_ref()
            .and_then(|decision| match decision {
                ExecutionDecision::ExecuteTarget { target, .. } => Some(target),
                ExecutionDecision::RepairTarget { context, .. } => Some(&context.target),
                _ => None,
            })
            .map_or(MutationFallbackPolicy::NoSafeFallback, |target| {
                crate::hosted_orchestrator::select_fallback_with_threshold(
                    &target_operation,
                    application.failure,
                    target,
                    self.manifest
                        .execution_policy
                        .mutation_replacement_max_bytes
                        .unwrap_or(
                            crate::hosted_orchestrator::DEFAULT_MUTATION_REPLACEMENT_THRESHOLD_BYTES,
                        )
                        .min(MAX_MODEL_FILE_BYTES),
                )
            });
        let repair_strategy = fallback_policy.as_str().to_owned();
        let payload_hash = raw_patch_hash
            .clone()
            .unwrap_or_else(|| sha256_text(rejected_payload));
        self.notebook
            .mutation_diagnostics
            .push(MutationDiagnosticArtifact {
                tool: tool.to_owned(),
                rejected_mutation_payload: truncate_text(rejected_payload, 64 * 1024),
                raw_patch_hash: raw_patch_hash.clone(),
                target_path: target_path.to_owned(),
                normalized_paths: normalized_paths.clone(),
                target_content_hash: application.target_content_hash.clone(),
                repository_fingerprint: repository_fingerprint.clone(),
                git_apply_check_result: application.git_apply_check.clone(),
                failure_category: application.failure,
                repair_strategy: repair_strategy.clone(),
                fallback_policy,
                rejected_mutation: Some(RejectedMutation {
                    tool: tool.to_owned(),
                    payload_hash: payload_hash.clone(),
                    failure_category: application.failure,
                    failure_diagnostics: MutationDiagnostics {
                        message: truncate_text(&application.message, 4_000),
                        normalized_paths: normalized_paths.clone(),
                        application_check: application.git_apply_check.clone(),
                    },
                    repository_fingerprint: repository_fingerprint.clone().into(),
                    applied: false,
                    status: crate::execution_graph::FailureStatus::Active,
                    superseded_by: None,
                    resolved_repository_fingerprint: None,
                }),
                attempt_accounting: target_attempt_accounting(
                    mutation_attempt,
                    repair_attempt,
                    fallback_policy,
                    self.notebook
                        .write_attempts
                        .iter()
                        .filter(|attempt| attempt.target == target_path)
                        .count(),
                ),
                strategy_fingerprint: Some(MutationStrategyFingerprint {
                    operation: target_operation,
                    tool: tool.to_owned(),
                    fallback_policy,
                    payload_type: if matches!(tool, "apply_patch" | "apply_unified_diff") {
                        "unified_diff"
                    } else {
                        "complete_content"
                    }
                    .into(),
                    failure_category: application.failure,
                }),
                mutation_attempt,
                repair_attempt,
            });
        if self.notebook.mutation_diagnostics.len() > 8 {
            let excess = self.notebook.mutation_diagnostics.len() - 8;
            self.notebook.mutation_diagnostics.drain(..excess);
        }
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.mutation_application_failed",
                "node_id": node_id.clone(),
                "tool": tool,
                "target": target_path,
                "raw_patch_hash": raw_patch_hash,
                "normalized_paths": normalized_paths,
                "target_content_hash": application.target_content_hash,
                "repository_fingerprint": repository_fingerprint,
                "failure_category": application.failure,
                "repair_strategy": repair_strategy,
                "mutation_attempt": mutation_attempt,
                "repair_attempt": repair_attempt,
            }),
            "mutation application failure",
        );
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.mutation_fallback_policy_selected",
                "node_id": node_id,
                "target": target_path,
                "target_operation": self.current_decision.as_ref().and_then(|decision| match decision {
                    ExecutionDecision::ExecuteTarget { target, .. } => Some(target.target.effective_operation()),
                    ExecutionDecision::RepairTarget { context, .. } => Some(context.target.target.effective_operation()),
                    _ => None,
                }),
                "mutation_tool": tool,
                "target_content_hash": application.target_content_hash,
                "repository_fingerprint": repository_fingerprint,
                "normalized_patch_paths": normalized_paths,
                "selected_fallback_policy": fallback_policy,
                "permitted_tools": fallback_policy.permitted_tools(),
                "forced_tool_choice": fallback_policy.forced_tool(),
                "failure_category": application.failure,
                "raw_patch_hash": raw_patch_hash,
                "mutation_attempt": mutation_attempt,
                "repair_attempt": repair_attempt,
                "verification_result": "not_applied",
            }),
            "mutation fallback selection",
        );
        Ok(())
    }
}

pub(super) fn target_attempt_accounting(
    total_node_attempts: u32,
    mutation_repair_calls: u32,
    fallback_policy: MutationFallbackPolicy,
    repository_write_attempts: usize,
) -> TargetAttemptAccounting {
    TargetAttemptAccounting {
        primary_mutation_calls: total_node_attempts.saturating_sub(mutation_repair_calls),
        mutation_repair_calls,
        context_rebuilds: u32::from(
            fallback_policy == MutationFallbackPolicy::RebuildTargetContext,
        ),
        repository_write_attempts: u32::try_from(repository_write_attempts).unwrap_or(u32::MAX),
    }
}
