// Extracted from the hosted execution composition root.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hosted) enum TargetContextPreparationResult {
    Prepared,
    TargetContextAlreadyPrepared,
}

fn target_context_already_prepared(
    events: &[crate::execution_graph::ExecutionDomainEvent],
    node_id: &crate::execution_graph::ExecutionNodeId,
    target_path: &str,
    target_content_hash: &Option<String>,
    repository_fingerprint: &str,
    accepted_intent_hash: &str,
) -> bool {
    events.iter().rev().any(|event| {
        matches!(
            event,
            crate::execution_graph::ExecutionDomainEvent::TargetContextPrepared {
                node_id: prepared_node_id,
                target_path: prepared_target_path,
                repository_fingerprint: prepared_repository_fingerprint,
                target_content_hash: prepared_target_content_hash,
                accepted_intent_hash: prepared_intent_hash,
                ..
            } if prepared_node_id == node_id
                && prepared_target_path == target_path
                && prepared_repository_fingerprint.as_str() == repository_fingerprint
                && prepared_target_content_hash == target_content_hash
                && prepared_intent_hash == accepted_intent_hash
        )
    })
}

impl<'a> GatewayAgent<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::hosted) fn new(
        api: HostedApiClient,
        manifest: &'a HostedManifest,
        repo: &'a Repo,
        trusted_git_config: &[u8],
        running: &'a Arc<AtomicBool>,
        stop_reason: &'a Arc<Mutex<Option<HostedStopReason>>>,
        containment: &'a command::HostedProcessContainment,
        partial_run: Option<PartialRunContext>,
    ) -> Result<Self> {
        let budget = manifest
            .budget_audit()
            .expect("hosted manifest budget was validated before agent construction");
        let total_calls = usize::try_from(budget.worker_received_model_call_budget)
            .unwrap_or_default()
            .min(MAX_MODEL_CALLS_HARD_LIMIT);
        let repository_fingerprint =
            repository_state_fingerprint(repo, &manifest.github.base_sha).unwrap_or_default();
        let restored = compatible_worker_notebook(manifest);
        let mut notebook = restored.unwrap_or_else(|| {
            new_worker_notebook(
                manifest,
                repository_fingerprint.clone(),
                partial_run.as_ref(),
            )
        });
        let restored_execution_attempt = notebook.execution_attempt;
        notebook.repository_fingerprint = repository_fingerprint;
        if notebook.acceptance_criteria_v2.is_empty() {
            notebook.acceptance_criteria_v2 =
                impact_map::acceptance_criteria(&notebook.acceptance_criteria);
        }
        if notebook.impact_evidence.is_empty() {
            notebook.impact_evidence = impact_map::evidence_catalog(
                &notebook.files_inspected,
                &notebook.searches_completed,
            );
        }
        if notebook.orchestration.legacy_import_pending() {
            normalize_notebook_intended_changes(&mut notebook, &repo.root)?;
        }
        if notebook.orchestration.legacy_import_pending()
            && let Some(partial_run) = &partial_run
        {
            if notebook.impact_map.is_empty() {
                notebook = new_worker_notebook(
                    manifest,
                    notebook.repository_fingerprint.clone(),
                    Some(partial_run),
                );
            } else {
                for work in &partial_run.remaining_work {
                    push_unique(&mut notebook.remaining_work, work.clone());
                }
            }
        }
        if notebook.phase == ExecutionPhase::ArtifactRepair && notebook.impact_map.is_empty() {
            let recovered = notebook
                .impact_map_invalid_payload
                .as_ref()
                .and_then(|payload| {
                    impact_map::normalize(
                        payload,
                        &notebook.files_inspected,
                        &notebook.searches_completed,
                        &notebook.acceptance_criteria,
                    )
                    .ok()
                    .map(|(map, source)| (map, source, 1.0))
                })
                .or_else(|| {
                    impact_map::fallback(
                        &notebook.files_inspected,
                        &notebook.searches_completed,
                        &notebook.acceptance_criteria,
                        &notebook.blocking_unknowns,
                    )
                    .map(|(map, confidence)| {
                        (map, ArtifactSource::OrchestratorFallback, confidence)
                    })
                });
            if let Some((map, source, confidence)) = recovered
                .filter(|(_, _, confidence)| *confidence >= impact_map_fallback_threshold(manifest))
            {
                notebook.impact_map = map.areas.clone();
                notebook.impact_map_v2 = Some(map.clone());
                notebook.files_inspected = map.inspected_files.clone();
                notebook.searches_completed = map
                    .searches
                    .iter()
                    .map(|search| search.query.clone())
                    .collect();
                notebook.blocking_unknowns = map.unresolved_questions.clone();
                notebook.impact_map_invalid_payload = None;
                notebook.impact_map_artifact = ArtifactCheckpoint {
                    artifact: "impact_map".into(),
                    semantic_status: ArtifactSemanticStatus::Sufficient,
                    serialization_status: ArtifactSerializationStatus::Valid,
                    persistence_status: ArtifactPersistenceStatus::PendingRetry,
                    artifact_sha256: impact_map_sha256(&map),
                    model_call_index: None,
                    phase: ExecutionPhase::ArtifactRepair,
                    safe_error: None,
                    normalization_metadata: None,
                    artifact_source: Some(source),
                    confidence: Some(confidence),
                    failure_layer: None,
                    validation_errors: Vec::new(),
                    invalid_payload_shape: None,
                };
            }
        }
        if !notebook.impact_map.is_empty()
            && notebook.impact_map_artifact.semantic_status == ArtifactSemanticStatus::Missing
        {
            let restored_map = ImpactMap {
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
            };
            notebook.impact_map_v2 = Some(restored_map.clone());
            notebook.impact_map_artifact = ArtifactCheckpoint {
                artifact: "impact_map".into(),
                semantic_status: ArtifactSemanticStatus::Sufficient,
                serialization_status: ArtifactSerializationStatus::Valid,
                persistence_status: ArtifactPersistenceStatus::Persisted,
                artifact_sha256: impact_map_sha256(&restored_map),
                model_call_index: None,
                phase: ExecutionPhase::Discovery,
                safe_error: None,
                normalization_metadata: None,
                artifact_source: Some(ArtifactSource::NormalizedModel),
                confidence: Some(1.0),
                failure_layer: None,
                validation_errors: Vec::new(),
                invalid_payload_shape: None,
            };
        }
        let (impact_map, legacy_implementation_plan, legacy_initial_phase) =
            notebook_orchestration_state(&notebook);
        let restored_changed_paths =
            completion_changed_paths(repo, &manifest.github.base_sha).unwrap_or_default();
        reconcile_notebook_orchestration(
            &mut notebook,
            manifest,
            legacy_implementation_plan.as_ref(),
            &restored_changed_paths,
            &HostedReconciliationFacts::default(),
        );
        let resume_reason = if manifest.execution.attempt_number > restored_execution_attempt {
            let previous_attempt =
                u32::try_from(restored_execution_attempt).with_context(|| {
                    format!("persisted execution attempt `{restored_execution_attempt}` is invalid")
                })?;
            let execution_attempt = u32::try_from(manifest.execution.attempt_number)
                .context("hosted execution attempt is invalid")?;
            notebook
                .orchestration
                .resume_for_new_attempt(
                    manifest.execution.execution_id.to_string(),
                    crate::execution_graph::RepositorySnapshot {
                        fingerprint: notebook.repository_fingerprint.clone(),
                        source_tree_hash: notebook.repository_fingerprint.clone(),
                        changed_paths: restored_changed_paths.iter().cloned().collect(),
                        ..crate::execution_graph::RepositorySnapshot::default()
                    },
                    previous_attempt,
                    execution_attempt,
                )
                .map_err(|error| anyhow!("could not resume hosted execution: {error}"))?
        } else {
            None
        };
        if resume_reason == Some(HostedResumeReason::PartialReviewable) {
            notebook.completion_artifact = None;
            notebook.finalization_revalidation = None;
        }
        if resume_reason.is_some() {
            let orchestration = std::mem::take(&mut notebook.orchestration);
            orchestration.materialize_legacy_notebook(&mut notebook);
            notebook.orchestration = orchestration;
        }
        let implementation_plan = implementation_plan_from_notebook(&notebook);
        let (diff_reviewed, completion_outcome) =
            canonical_finalization_state(&notebook.orchestration);
        let restored_declaration = valid_completion_artifact(
            &notebook,
            &notebook.repository_fingerprint,
            &restored_changed_paths,
        )
        .and_then(|artifact| artifact.declaration.clone());
        let restored_cost_guard = CostGuard {
            estimated_cost_micros: notebook.orchestration.budget.total_cost_micros,
            call_count: notebook.orchestration.budget.total_model_calls,
            hard_limit_micros: notebook.orchestration.budget.mission.max_cost_micros,
            max_duration_seconds: notebook.orchestration.budget.mission.max_duration.as_secs(),
            ..CostGuard::default()
        };
        let initial_phase = notebook.orchestration.execution_phase(legacy_initial_phase);
        let impact_map_failure =
            notebook
                .impact_map_invalid_payload
                .as_ref()
                .map(|payload| ImpactMapFailure {
                    code: "impact_map_schema_mismatch",
                    safe_error: notebook
                        .impact_map_artifact
                        .safe_error
                        .clone()
                        .unwrap_or_else(|| "impact_map_schema_mismatch".into()),
                    errors: notebook.impact_map_artifact.validation_errors.clone(),
                    invalid_payload: payload.clone(),
                    invalid_payload_shape: notebook
                        .impact_map_artifact
                        .invalid_payload_shape
                        .clone()
                        .unwrap_or_else(|| impact_map::safe_shape(payload)),
                    failure_layer: notebook
                        .impact_map_artifact
                        .failure_layer
                        .unwrap_or(ArtifactFailureLayer::WorkerToolSchemaValidation),
                });
        let mut phases = PhaseLedger::new(total_calls, initial_phase);
        phases.ensure_finalization_minimum(notebook.acceptance_criteria.len());
        let restored_last_successful_action = notebook.last_successful_action.clone();
        let mut agent = Self {
            api,
            manifest,
            repo,
            trusted_git_config: trusted_git_config.to_vec(),
            running,
            stop_reason,
            containment,
            budget,
            phases,
            impact_map,
            implementation_plan,
            declaration: restored_declaration,
            tool_failures: notebook.failed_changes.clone(),
            tool_usage: ToolUsage::default(),
            notebook: WorkerNotebook {
                phase: initial_phase,
                execution_attempt: manifest.execution.attempt_number,
                ..notebook
            },
            search_guard: SearchGuard::default(),
            repair_read_targets: BTreeSet::new(),
            diff_reviewed,
            diff_review_cursor: 0,
            diff_review_digest: None,
            write_blocker: None,
            blocked_plan_recorded_at: None,
            impact_map_failure,
            last_successful_action: restored_last_successful_action,
            partial_run,
            budget_advisory_percent: 0,
            last_cache_prefix_sha256: None,
            last_tool_order_sha256: None,
            guided_first_write_recovery_issued: false,
            last_repository_progress_call: 0,
            cost_guard: restored_cost_guard,
            execution_started_at: Instant::now(),
            phase_started_at: Instant::now(),
            last_source_progress_call: 0,
            current_decision: None,
            completion_outcome,
        };
        if resume_reason.is_some() {
            agent.persist_orchestration_checkpoint("execution_resumed", false)?;
        }
        Ok(agent)
    }

    pub(in crate::hosted) fn implement(&mut self) -> Result<ImplementationOutcome> {
        let prompt = build_hosted_prompt(self.manifest, self.repo, self.partial_run.as_ref())?;
        if self.reconcile_authoritative_target_state()?
            == ImplementationCompletionStatus::ReadyForValidation
        {
            let decision = self.reconcile_execution_and_apply()?;
            if !matches!(decision.decision, ExecutionDecision::RunValidation { .. })
                && self.phases.active() != ExecutionPhase::Validation
            {
                bail!("restored execution graph did not authorize validation");
            }
            return Ok(ImplementationOutcome {
                summary: "All planned targets were already applied; resumed at validation.".into(),
                budget_exhausted: false,
                explicit_declaration: self.declaration.clone(),
            });
        }
        self.checkpoint_notebook(false)?;
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.notebook_checkpoint",
                "phase": self.phases.active(),
                "notebook": self.notebook,
                "checkpoint": self.notebook_checkpoint_metadata(None),
                "budget": self.budget_telemetry(),
                "resumed": self.manifest.execution.attempt_number > 1
                    && self.impact_map.is_some(),
            }),
            "initial notebook checkpoint",
        );
        self.run_session(&prompt, true)
    }

    pub(in crate::hosted) fn budget_telemetry(&self) -> Value {
        let mut telemetry = self.phases.telemetry();
        if let Some(object) = telemetry.as_object_mut() {
            object.insert(
                "requested_model_call_budget".into(),
                json!(self.budget.requested_model_call_budget),
            );
            object.insert(
                "resolved_model_call_budget".into(),
                json!(self.budget.resolved_model_call_budget),
            );
            object.insert(
                "model_call_budget".into(),
                json!(self.budget.resolved_model_call_budget),
            );
            object.insert(
                "worker_received_model_call_budget".into(),
                json!(self.budget.worker_received_model_call_budget),
            );
            object.insert("budget_source".into(), json!(self.budget.budget_source));
            object.insert("clamped".into(), json!(self.budget.clamped));
            object.insert("clamp_reason".into(), json!(self.budget.clamp_reason));
            object.insert("budget_contract".into(), json!(self.budget.contract));
            if self.notebook.orchestration.graph.is_some() {
                let snapshot = self.notebook.orchestration.snapshot(
                    self.manifest.execution.execution_id.to_string(),
                    crate::execution_graph::RepositorySnapshot {
                        fingerprint: self.notebook.repository_fingerprint.clone(),
                        source_tree_hash: self.notebook.repository_fingerprint.clone(),
                        ..crate::execution_graph::RepositorySnapshot::default()
                    },
                );
                let mut graph_telemetry = HostedOrchestrationTelemetry::from_snapshot(&snapshot);
                graph_telemetry.complexity_score = self
                    .notebook
                    .orchestration
                    .complexity
                    .as_ref()
                    .map(|assessment| assessment.score);
                object.insert(
                    "hosted_execution_stage".into(),
                    json!(self.notebook.orchestration.hosted_stage()),
                );
                object.insert(
                    "hosted_orchestration".into(),
                    serde_json::to_value(graph_telemetry).unwrap_or_else(|_| json!({})),
                );
            }
            object.insert(
                "context_policy".into(),
                json!({
                    "authoritative_notebook": true,
                    "raw_turn_windows_retained": MAX_HOSTED_TURN_WINDOWS,
                    "older_tool_output_compacted": true,
                }),
            );
        }
        telemetry
    }

    pub(in crate::hosted) fn append_event_recoverable(
        &self,
        event_type: &str,
        data: Value,
        operation: &str,
    ) -> bool {
        match self.api.append_event(event_type, data) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("[warning] {operation} could not be persisted: {error:#}");
                false
            }
        }
    }

    pub(in crate::hosted) fn ensure_active_or_checkpoint_cancellation(&mut self) -> Result<()> {
        if self.running.load(Ordering::SeqCst) && !shutdown::requested() {
            return Ok(());
        }
        let stop_reason = self
            .stop_reason
            .lock()
            .expect("hosted stop reason lock poisoned")
            .clone();
        if let Some(HostedStopReason::Infrastructure(detail)) = stop_reason {
            let _ = self.containment.drain();
            self.reconcile_authoritative_target_state()?;
            let node_id = self
                .current_decision
                .as_ref()
                .and_then(ExecutionDecision::node_id)
                .cloned()
                .or_else(|| {
                    self.notebook
                        .orchestration
                        .graph
                        .as_ref()
                        .and_then(|graph| graph.next_runnable_node())
                        .map(|node| node.id.clone())
                })
                .context("infrastructure stop occurred before a graph node was available")?;
            let fingerprint =
                repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
            let failure_id = crate::execution_graph::FailureId::new(format!(
                "infrastructure-{}",
                sha256_text(&format!("{node_id}\0{fingerprint}\0{detail}"))
            ));
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::FailureRecorded {
                    sequence: self.next_domain_event_sequence(),
                    failure: crate::execution_graph::FailureRecord::new(
                        failure_id,
                        node_id.clone(),
                        crate::execution_graph::FailureCategory::InfrastructureFailure,
                        1,
                        fingerprint,
                        detail.clone(),
                    ),
                },
            )?;
            self.finalize_guardrail_outcome(OrchestratedMissionOutcome::FailedInfrastructure)?;
            self.persist_orchestration_checkpoint("infrastructure_failure", true)?;
            return Err(self.infrastructure_stop_failure(&detail));
        }
        let active_validation_terminated = self.containment.drain().is_ok();
        self.reconcile_authoritative_target_state()?;
        let repo_config = self.manifest.repo_config()?;
        let preservation_result = (|| {
            ensure_cancellation_repository_integrity(
                self.repo,
                &repo_config,
                self.manifest,
                &self.trusted_git_config,
            )?;
            preserve_cancellation_branch_with(
                self.repo,
                &self.manifest.github.base_sha,
                &self.manifest.github.branch,
                &format!(
                    "{}: {} (cancellation checkpoint)",
                    self.manifest.ticket_key, self.manifest.ticket_title
                ),
                |branch, commit_sha| {
                    ensure_cancellation_repository_integrity(
                        self.repo,
                        &repo_config,
                        self.manifest,
                        &self.trusted_git_config,
                    )?;
                    let token = self.api.github_token(&self.manifest.github.repository)?;
                    let pushed = self.repo.push(
                        branch,
                        commit_sha,
                        token.expose(),
                        &self.manifest.github.web_base_url,
                    )?;
                    drop(token);
                    Ok(pushed)
                },
            )
        })();
        let mut preservation_failure = None;
        let preservation = match preservation_result {
            Ok(preservation) => preservation,
            Err(error) => {
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.cancellation_branch_preservation_failed",
                        "status": "failed",
                        "branch": self.manifest.github.branch,
                        "safe_error": truncate_text(&format!("{error:#}"), 2_000),
                        "canonical_checkpoint_will_be_persisted": true,
                        "publication_started": false,
                        "resumable": true,
                    }),
                    "cancellation branch preservation failure",
                );
                preservation_failure = Some(error);
                None
            }
        };
        if let Some(preservation) = &preservation {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.cancellation_branch_preserved",
                    "status": "completed",
                    "branch": self.manifest.github.branch,
                    "head_sha": preservation.commit_sha,
                    "changed_paths": preservation.changed_paths,
                    "committed_paths": preservation.committed_paths,
                    "commit_created": preservation.commit_created,
                    "push_performed": preservation.push_performed,
                    "remote_already_current": preservation.remote_already_current,
                    "remote_preserved": true,
                    "publication_started": false,
                    "resumable": true,
                }),
                "cancellation branch preservation progress",
            );
        }
        let cancellation = crate::execution_graph::CancellationState {
            requested_at: now_rfc3339(),
            reason: "user_or_lease_owner requested hosted execution cancellation".into(),
            requested_by: Some("user_or_lease_owner".into()),
            active_validation_terminated,
            checkpointed: true,
        };
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::CancellationRequested {
                sequence: self.next_domain_event_sequence(),
                state: cancellation,
            },
        )?;
        // Cancellation is a resumable stop, not a terminal domain result. A
        // strictly newer attempt clears it through `ExecutionResumed` and
        // continues from the next graph node.
        self.persist_orchestration_checkpoint("cancellation_checkpointed", true)?;
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
        let result = CancellationResult {
            requested_by: "user_or_lease_owner",
            requested_at: now_rfc3339(),
            phase: self.phases.active(),
            changed_paths,
            completed_changes: self.notebook.completed_changes.clone(),
            remaining_work: self.notebook.remaining_work_v2.clone(),
            source_tree_hash: self.notebook.repository_fingerprint.clone(),
            resumable: true,
            resume_phase: self.phases.active(),
        };
        self.append_event_recoverable(
            "result",
            json!({
                "event_type": "worker.terminal_result_persisted",
                "status": "cancelled",
                "process_health": "healthy",
                "mission_outcome": OrchestratedMissionOutcome::Cancelled,
                "active_validation_terminated": active_validation_terminated,
                "cancellation": result,
                "notebook": self.notebook,
            }),
            "cancellation checkpoint",
        );
        if let Some(error) = preservation_failure {
            return Err(error).context(
                "hosted execution was cancelled and checkpointed, but branch preservation failed",
            );
        }
        bail!("hosted execution was cancelled after preserving a resumable checkpoint")
    }

    pub(in crate::hosted) fn record_tool_progress(
        &mut self,
        tool: &str,
        target: Option<String>,
        class: ToolProgressClass,
        detail: impl Into<String>,
        repository_progress: bool,
    ) {
        let record = new_tool_progress_record(
            self.manifest.execution.attempt_number,
            self.phases.total_calls(),
            self.phases.active(),
            tool,
            target,
            class,
            detail,
            repository_progress,
        );
        if repository_progress {
            self.last_repository_progress_call = self.phases.implementation_repair_calls();
        }
        self.notebook.tool_progress.push(record);
        if self.notebook.tool_progress.len() > 64 {
            self.notebook
                .tool_progress
                .drain(..self.notebook.tool_progress.len().saturating_sub(64));
        }
    }

    pub(in crate::hosted) fn observe_implementation_progress(&mut self) -> Result<()> {
        if !matches!(
            self.phases.active(),
            ExecutionPhase::Implementation | ExecutionPhase::Repair
        ) {
            return Ok(());
        }
        let calls = self.phases.implementation_repair_calls();
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let read_progress = implementation_read_progress(
            &self.notebook.tool_progress,
            self.manifest.execution.attempt_number,
        );
        let calls_since_repository_progress =
            calls.saturating_sub(self.last_repository_progress_call);
        let action = implementation_progress_action(
            calls,
            self.tool_usage.successful_writes,
            read_progress.consecutive_preparation_reads,
            read_progress.recoverable_read_failures,
            read_progress.repeated_identical_read_failures,
            self.guided_first_write_recovery_issued,
            calls_since_repository_progress,
        );
        if action == ImplementationProgressAction::FirstWriteDelayed {
            self.guided_first_write_recovery_issued = true;
        }
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": match action {
                    ImplementationProgressAction::FirstWriteDelayed => "worker.first_write_delayed",
                    ImplementationProgressAction::Continue => "worker.implementation_progress_window",
                },
                "implementation_calls": calls,
                "implementation_substate": self.notebook.implementation_substate,
                "successful_writes": self.tool_usage.successful_writes,
                "changed_paths": changed_paths,
                "consecutive_preparation_reads": read_progress.consecutive_preparation_reads,
                "recoverable_read_failures": read_progress.recoverable_read_failures,
                "repeated_identical_read_failures": read_progress.repeated_identical_read_failures,
                "calls_since_repository_progress": calls_since_repository_progress,
                "guided_recovery_issued": self.guided_first_write_recovery_issued,
                "unresolved_preparation_blockers": unresolved_preparation_blockers(
                    &self.notebook.tool_progress,
                    self.manifest.execution.attempt_number,
                    calls,
                    self.tool_usage.successful_writes,
                ),
                "last_six_tool_outcomes": self.notebook.tool_progress.iter().rev().take(6).collect::<Vec<_>>(),
                "orchestration_action": if action == ImplementationProgressAction::FirstWriteDelayed {
                    "guided_single_target_recovery"
                } else {
                    "continue"
                },
            }),
            "implementation progress window",
        );
        Ok(())
    }

    /// Reconciles the signed wall-clock envelope with the canonical graph.
    /// Expiry may stop model work, but it must not discard reviewable changes
    /// before validation or interrupt an already-authorized publication route.
    pub(in crate::hosted) fn reconcile_wall_clock_boundary(
        &mut self,
        boundary: HostedWallClockBoundary,
    ) -> Result<()> {
        let limit = Duration::from_secs(self.cost_guard.max_duration_seconds)
            .min(MAX_HOSTED_EXECUTION_DURATION);
        let elapsed = self.execution_started_at.elapsed();
        let expired = elapsed >= limit;
        if !expired {
            return Ok(());
        }

        let decision = self.peek_execution_decision()?;
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
        let action =
            hosted_wall_clock_action(expired, boundary, !changed_paths.is_empty(), &decision);
        match action {
            HostedWallClockAction::Continue => Ok(()),
            HostedWallClockAction::EnterPartialValidation => {
                self.record_partial_reviewable_handoff(
                    crate::execution_graph::GuardrailReason::MissionBudgetExhausted,
                    "the mission wall-clock budget ended with a non-empty reviewable diff",
                )?;
                let next = self.peek_execution_decision()?;
                if !matches!(
                    next,
                    ExecutionDecision::RunValidation { .. }
                        | ExecutionDecision::ReviewDiff { .. }
                        | ExecutionDecision::EvaluateCompletion { .. }
                        | ExecutionDecision::Publish { .. }
                        | ExecutionDecision::Finish { .. }
                ) {
                    bail!(
                        "hosted orchestration invariant: wall-clock partial handoff returned `{}` instead of validation or finalization",
                        execution_decision_name(&next)
                    );
                }
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.wall_clock_partial_validation_authorized",
                        "boundary": boundary.as_str(),
                        "elapsed_ms": elapsed.as_millis(),
                        "limit_seconds": limit.as_secs(),
                        "changed_paths": changed_paths,
                        "next_decision": execution_decision_name(&next),
                        "publication_mode": "draft_if_incomplete",
                    }),
                    "wall-clock partial validation handoff",
                );
                Ok(())
            }
            HostedWallClockAction::CompleteBlockedNoDiff => {
                self.append_execution_domain_event(
                    crate::execution_graph::ExecutionDomainEvent::GuardrailTriggered {
                        sequence: self.next_domain_event_sequence(),
                        reason: crate::execution_graph::GuardrailReason::MissionBudgetExhausted,
                        outcome: OrchestratedMissionOutcome::BlockedNoDiff,
                        detail: "the mission wall-clock budget ended before any reviewable repository change"
                            .into(),
                    },
                )?;
                self.finalize_guardrail_outcome(OrchestratedMissionOutcome::BlockedNoDiff)?;
                self.persist_orchestration_checkpoint("wall_clock_blocked_no_diff", false)?;
                Err(self.blocked_no_diff_failure())
            }
            HostedWallClockAction::ContinueFinalization => {
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.wall_clock_finalization_continued",
                        "boundary": boundary.as_str(),
                        "elapsed_ms": elapsed.as_millis(),
                        "limit_seconds": limit.as_secs(),
                        "changed_paths": changed_paths,
                        "graph_decision": execution_decision_name(&decision),
                        "reason": "validation or publication is already graph-authorized; finish safe pull-request finalization",
                    }),
                    "wall-clock graph-authorized finalization",
                );
                Ok(())
            }
            HostedWallClockAction::InvalidFinalizationRoute => bail!(
                "hosted orchestration invariant: wall-clock boundary `{}` observed non-finalizable graph decision `{}`",
                boundary.as_str(),
                execution_decision_name(&decision)
            ),
        }
    }

    pub(in crate::hosted) fn record_partial_reviewable_handoff(
        &mut self,
        reason: crate::execution_graph::GuardrailReason,
        detail: &str,
    ) -> Result<()> {
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
        if changed_paths.is_empty() {
            bail!("partial-reviewable handoff requires a non-empty repository diff");
        }
        let already_recorded = crate::execution_graph::current_execution_epoch(
            &self.notebook.orchestration.domain_events,
        )
        .iter()
        .any(|event| {
            matches!(
                event,
                crate::execution_graph::ExecutionDomainEvent::GuardrailTriggered {
                    outcome: OrchestratedMissionOutcome::PartialReviewable,
                    ..
                }
            )
        });
        if !already_recorded {
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::GuardrailTriggered {
                    sequence: self.next_domain_event_sequence(),
                    reason,
                    outcome: OrchestratedMissionOutcome::PartialReviewable,
                    detail: detail.into(),
                },
            )?;
            self.persist_orchestration_checkpoint("partial_reviewable_handoff", true)?;
        }
        Ok(())
    }

    pub(in crate::hosted) fn record_cache_observability(
        &mut self,
        request: &Value,
        response: &Value,
    ) {
        let (payload, prefix_sha256, tool_order_sha256) = cache_observability_payload(
            request,
            response,
            self.last_cache_prefix_sha256.as_deref(),
            self.last_tool_order_sha256.as_deref(),
        );
        self.append_event_recoverable("progress", payload, "AI cache observability");
        self.last_cache_prefix_sha256 = Some(prefix_sha256);
        self.last_tool_order_sha256 = Some(tool_order_sha256);
    }

    pub(in crate::hosted) fn reserve_graph_model_call(
        &mut self,
        request: &Value,
    ) -> Option<crate::execution_graph::ModelCallReservation> {
        let node_id = self
            .current_decision
            .as_ref()
            .and_then(ExecutionDecision::node_id)
            .cloned()
            .or_else(|| {
                self.notebook
                    .orchestration
                    .graph
                    .as_ref()
                    .and_then(|graph| {
                        graph
                            .nodes
                            .iter()
                            .find(|node| {
                                node.status == crate::execution_graph::ExecutionNodeStatus::Running
                            })
                            .or_else(|| graph.next_runnable_node())
                    })
                    .map(|node| node.id.clone())
            })?;
        let node_budget = self
            .notebook
            .orchestration
            .graph
            .as_ref()
            .and_then(|graph| graph.node(&node_id))
            .map(|node| node.budget.clone())?;
        let estimate = estimate_model_call_request_cost(request);
        let estimated_cost_micros = estimate.estimated_request_cost;
        let admission = self
            .notebook
            .orchestration
            .budget
            .evaluate_model_call_admission(
                &node_id,
                &node_budget,
                1,
                estimated_cost_micros,
                Duration::ZERO,
            );
        let reservation = if admission.admitted {
            self.notebook
                .orchestration
                .budget
                .reserve_model_call(
                    &node_id,
                    &node_budget,
                    estimated_cost_micros,
                    Duration::ZERO,
                )
                .ok()
        } else {
            None
        };
        self.append_event_recoverable(
            "progress",
            model_call_admission_telemetry(&admission, &estimate),
            "model-call admission telemetry",
        );
        if !admission.admitted {
            return None;
        }
        reservation
    }

    pub(in crate::hosted) fn observe_model_cost(
        &mut self,
        reservation: &crate::execution_graph::ModelCallReservation,
        request: &Value,
        response: &Value,
        duration: Duration,
    ) -> Result<()> {
        let (input_tokens, output_tokens, estimated) =
            match model_usage_for_accounting(request, response) {
                Ok(usage) => usage,
                Err(error) => {
                    self.observe_failed_model_cost(reservation, request, None, duration);
                    return Err(error);
                }
            };
        self.cost_guard.call_count = self.cost_guard.call_count.saturating_add(1);
        self.cost_guard.input_tokens = self.cost_guard.input_tokens.saturating_add(input_tokens);
        self.cost_guard.output_tokens = self.cost_guard.output_tokens.saturating_add(output_tokens);
        if estimated {
            self.cost_guard.usage_estimate_fallbacks =
                self.cost_guard.usage_estimate_fallbacks.saturating_add(1);
        }
        // Conservative provider-independent estimate: EUR 5/M input and EUR 15/M output.
        let call_cost_micros = input_tokens
            .saturating_mul(5)
            .saturating_add(output_tokens.saturating_mul(15));
        self.cost_guard.estimated_cost_micros = self
            .cost_guard
            .estimated_cost_micros
            .saturating_add(call_cost_micros);
        self.notebook
            .orchestration
            .budget
            .consume_model_call_reservation(reservation, call_cost_micros, duration);
        if estimated {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.model_usage_estimated",
                    "reason_code": "provider_usage_missing_or_invalid",
                    "input_tokens_accounted": input_tokens,
                    "output_tokens_accounted": output_tokens,
                    "usage_estimate_fallbacks": self.cost_guard.usage_estimate_fallbacks,
                    "estimated_cost_micros": self.cost_guard.estimated_cost_micros,
                }),
                "conservative model usage accounting",
            );
        }
        Ok(())
    }

    /// A dispatched call is canonical budget usage even when no successful
    /// provider response is available. Only an explicitly restored reservation
    /// may be omitted. Unknown dispositions are charged conservatively so a
    /// transport failure cannot authorize paid retries beyond the graph budget.
    pub(in crate::hosted) fn observe_failed_model_cost(
        &mut self,
        reservation: &crate::execution_graph::ModelCallReservation,
        request: &Value,
        actual_cost_micros: Option<u64>,
        duration: Duration,
    ) {
        let (input_tokens, output_tokens, call_cost_micros, estimated) =
            failed_model_usage_for_accounting(request, actual_cost_micros);

        self.cost_guard.call_count = self.cost_guard.call_count.saturating_add(1);
        self.cost_guard.estimated_cost_micros = self
            .cost_guard
            .estimated_cost_micros
            .saturating_add(call_cost_micros);
        if estimated {
            self.cost_guard.input_tokens =
                self.cost_guard.input_tokens.saturating_add(input_tokens);
            self.cost_guard.output_tokens =
                self.cost_guard.output_tokens.saturating_add(output_tokens);
            self.cost_guard.usage_estimate_fallbacks =
                self.cost_guard.usage_estimate_fallbacks.saturating_add(1);
        }

        self.notebook
            .orchestration
            .budget
            .consume_model_call_reservation(reservation, call_cost_micros, duration);
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.failed_model_call_accounted",
                "actual_cost_micros": actual_cost_micros,
                "charged_cost_micros": call_cost_micros,
                "usage_estimated": estimated,
                "duration_ms": duration.as_millis(),
            }),
            "failed model call budget accounting",
        );
    }

    pub(in crate::hosted) fn notebook_checkpoint_metadata(
        &self,
        artifact_sha256: Option<&str>,
    ) -> Value {
        json!({
            "execution_id": self.manifest.execution.execution_id,
            "notebook_revision": self.notebook.revision,
            "expected_previous_revision": self.notebook.revision.saturating_sub(1),
            "artifact_hash": artifact_sha256,
            "model_call_index": self.phases.total_calls(),
            "phase": self.phases.active(),
            "tool_schema_version": IMPACT_MAP_SCHEMA_VERSION,
            "tool_schema_sha256": impact_map::schema_sha256(),
            "validator_schema_version": IMPACT_MAP_SCHEMA_VERSION,
            "validator_schema_sha256": impact_map::schema_sha256(),
        })
    }

    pub(in crate::hosted) fn apply_execution_decision(
        &mut self,
        decision: ExecutionDecision,
    ) -> Result<DecisionExecutionResult> {
        let phase = match &decision {
            ExecutionDecision::ContinueDiscovery {
                action: crate::hosted_orchestrator::DiscoveryAction::RepairImpactMap { .. },
            } => Some(ExecutionPhase::ArtifactRepair),
            ExecutionDecision::ContinueDiscovery { .. } => Some(ExecutionPhase::Discovery),
            ExecutionDecision::ContinuePlanning { .. } => Some(ExecutionPhase::Planning),
            ExecutionDecision::ExecuteTarget {
                action: crate::hosted_orchestrator::MutationAction::RepairTarget { .. },
                ..
            } => Some(ExecutionPhase::Repair),
            ExecutionDecision::ExecuteTarget { .. } => Some(ExecutionPhase::Implementation),
            ExecutionDecision::RepairTarget { .. } => Some(ExecutionPhase::Repair),
            ExecutionDecision::RunValidation { .. } => Some(ExecutionPhase::Validation),
            ExecutionDecision::ReviewDiff { .. } => Some(ExecutionPhase::DiffReview),
            ExecutionDecision::EvaluateCompletion { .. } => {
                Some(ExecutionPhase::CompletionEvaluation)
            }
            ExecutionDecision::Publish { .. } => Some(ExecutionPhase::Publication),
            ExecutionDecision::Finish { .. } | ExecutionDecision::StopForGuardrail { .. } => None,
        };
        let previous = self.phases.active();
        if let Some(next) = phase
            && next != previous
            && (!previous.stage().can_transition_to(next.stage())
                || !legal_phase_transition(previous, next))
        {
            bail!(
                "illegal hosted lifecycle transition from `{}` to `{}`",
                previous.as_str(),
                next.as_str()
            );
        }
        let event_count_before = self.notebook.orchestration.domain_events.len();
        self.record_decision_domain_event(&decision)?;
        if matches!(decision, ExecutionDecision::Publish { .. })
            && self.notebook.finalization_revalidation.is_some()
        {
            self.complete_finalization_revalidation()?;
        }
        let decision_event_added =
            self.notebook.orchestration.domain_events.len() > event_count_before;
        if decision_event_added && (phase.is_none() || phase == Some(previous)) {
            self.persist_orchestration_checkpoint("decision_domain_event_applied", false)?;
        }
        let (phase_decision, persistence_error) = if let Some(phase) = phase {
            if phase == previous {
                (PhaseDecision::Stay, None)
            } else {
                // This is the sole hosted lifecycle mutation. Every caller must
                // arrive here with a decision from `reconcile_execution`.
                self.phases.transition(phase);
                if self.tool_usage.successful_writes > 0
                    && matches!(
                        (previous, phase),
                        (ExecutionPhase::Implementation, ExecutionPhase::Repair)
                            | (ExecutionPhase::Repair, ExecutionPhase::Implementation)
                    )
                {
                    self.last_repository_progress_call = self.phases.implementation_repair_calls();
                }
                self.phase_started_at = Instant::now();
                self.notebook.phase = phase;
                self.checkpoint_notebook(false)?;
                let reason = format!(
                    "authoritative decision `{}`",
                    execution_decision_name(&decision)
                );
                let event = json!({
                    "event_type": "worker.phase_transition",
                    "from_phase": previous,
                    "phase": phase,
                    "from_state": canonical_running_state(previous),
                    "to_state": canonical_running_state(phase),
                    "reason_code": "phase_reconciled",
                    "reason": reason,
                    "source": match phase {
                        ExecutionPhase::Validation => "quality_gate",
                        ExecutionPhase::Publication => "publication",
                        _ => "orchestrator",
                    },
                    "notebook_revision": self.notebook.revision,
                    "source_tree_hash": self.notebook.repository_fingerprint,
                    "occurred_at": now_rfc3339(),
                    "budget": self.budget_telemetry(),
                    "notebook": self.notebook,
                    "checkpoint": self.notebook_checkpoint_metadata(
                        self.notebook.impact_map_artifact.artifact_sha256.as_deref()
                    ),
                });
                let persistence_error = self
                    .api
                    .append_event("progress", event)
                    .err()
                    .map(|error| truncate_text(&format!("{error:#}"), 2_000));
                if let Some(error) = persistence_error.as_deref() {
                    eprintln!("[warning] phase transition could not be persisted: {error}");
                    self.append_event_recoverable(
                        "progress",
                        json!({
                            "event_type": "worker.phase_persistence_failed",
                            "from_phase": previous,
                            "phase": phase,
                            "recoverable": true,
                            "action": "retry_or_continue",
                            "safe_error": error,
                            "checkpoint": self.notebook_checkpoint_metadata(
                                self.notebook.impact_map_artifact.artifact_sha256.as_deref()
                            ),
                        }),
                        "phase persistence failure warning",
                    );
                }
                (PhaseDecision::Transition(phase), persistence_error)
            }
        } else {
            (PhaseDecision::Stay, None)
        };
        self.current_decision = Some(decision.clone());
        Ok(DecisionExecutionResult {
            decision,
            phase_decision,
            persistence_error,
        })
    }

    pub(in crate::hosted) fn record_decision_domain_event(
        &mut self,
        decision: &ExecutionDecision,
    ) -> Result<()> {
        use crate::execution_graph::{ExecutionDomainEvent, ExecutionNodeKind};

        let sequence = self.next_domain_event_sequence();
        let repository_fingerprint =
            repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let node_started = |node_id: &crate::execution_graph::ExecutionNodeId,
                            this: &Self|
         -> Option<ExecutionDomainEvent> {
            let node = this.notebook.orchestration.graph.as_ref()?.node(node_id)?;
            if node.status == crate::execution_graph::ExecutionNodeStatus::Running {
                return None;
            }
            Some(ExecutionDomainEvent::NodeStarted {
                sequence,
                node_id: node_id.clone(),
                attempt: u32::try_from(node.attempts.len())
                    .unwrap_or(u32::MAX)
                    .saturating_add(1),
                started_at: now_rfc3339(),
                repository_fingerprint: repository_fingerprint.clone(),
            })
        };
        let event = match decision {
            ExecutionDecision::ContinueDiscovery { .. } => {
                let already_started = self
                    .notebook
                    .orchestration
                    .graph
                    .as_ref()
                    .and_then(|graph| {
                        graph
                            .nodes
                            .iter()
                            .find(|node| node.kind == ExecutionNodeKind::Discovery)
                    })
                    .is_some_and(|node| {
                        node.status == crate::execution_graph::ExecutionNodeStatus::Running
                    });
                (!already_started).then_some(ExecutionDomainEvent::DiscoveryStarted { sequence })
            }
            ExecutionDecision::ContinuePlanning { .. } => self
                .graph_node_id(ExecutionNodeKind::Planning)
                .ok()
                .and_then(|node_id| node_started(&node_id, self)),
            ExecutionDecision::ExecuteTarget {
                action: crate::hosted_orchestrator::MutationAction::PrepareTargetContext { .. },
                ..
            } => None,
            ExecutionDecision::ExecuteTarget { node_id, .. }
            | ExecutionDecision::RepairTarget { node_id, .. }
            | ExecutionDecision::ReviewDiff { node_id }
            | ExecutionDecision::EvaluateCompletion { node_id } => node_started(node_id, self),
            ExecutionDecision::RunValidation { node_id, gate } => {
                let running = self
                    .notebook
                    .orchestration
                    .graph
                    .as_ref()
                    .and_then(|graph| graph.node(node_id))
                    .is_some_and(|node| {
                        node.status == crate::execution_graph::ExecutionNodeStatus::Running
                    });
                (!running).then_some(ExecutionDomainEvent::ValidationStarted {
                    sequence,
                    node_id: node_id.clone(),
                    fingerprint: gate.fingerprint(&repository_fingerprint),
                })
            }
            ExecutionDecision::Publish { mode } => {
                let node_id = self.graph_node_id(ExecutionNodeKind::Publication)?;
                (self.notebook.orchestration.publication.status
                    == crate::execution_graph::PublicationStatus::NotStarted)
                    .then_some(ExecutionDomainEvent::PublicationStarted {
                        sequence,
                        node_id,
                        mode: *mode,
                    })
            }
            ExecutionDecision::StopForGuardrail { outcome, reason } => {
                let already_recorded = crate::execution_graph::current_execution_epoch(
                    &self.notebook.orchestration.domain_events,
                )
                .iter()
                .rev()
                .any(|event| {
                    matches!(
                        event,
                        ExecutionDomainEvent::GuardrailTriggered {
                            reason: existing_reason,
                            outcome: existing_outcome,
                            ..
                        } if existing_reason == reason && existing_outcome == outcome
                    )
                });
                (!already_recorded).then_some(ExecutionDomainEvent::GuardrailTriggered {
                    sequence,
                    reason: *reason,
                    outcome: *outcome,
                    detail: format!("authoritative guardrail decision: {reason:?}"),
                })
            }
            ExecutionDecision::Finish { outcome } => {
                crate::execution_graph::current_epoch_terminal_outcome(
                    &self.notebook.orchestration.domain_events,
                )
                .is_none()
                .then_some(ExecutionDomainEvent::RunFinished {
                    sequence,
                    outcome: *outcome,
                })
            }
        };
        if let Some(event) = event {
            self.append_execution_domain_event(event)?;
        }
        Ok(())
    }

    pub(in crate::hosted) fn next_domain_event_sequence(&self) -> u64 {
        self.notebook
            .orchestration
            .domain_events
            .last()
            .map_or(1, |event| event.sequence().saturating_add(1))
    }

    pub(in crate::hosted) fn initialize_fresh_execution_snapshot(
        &mut self,
        startup: &StartupModeResolution,
        resumed_branch: bool,
    ) -> Result<()> {
        let graph = self
            .notebook
            .orchestration
            .graph
            .clone()
            .context("fresh execution did not initialize an execution graph")?;
        if self
            .notebook
            .orchestration
            .domain_events
            .iter()
            .any(|event| {
                matches!(
                    event,
                    crate::execution_graph::ExecutionDomainEvent::GraphCreated { .. }
                )
            })
        {
            bail!("fresh execution unexpectedly contained a persisted GraphCreated event");
        }
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::GraphCreated {
                sequence: self.next_domain_event_sequence(),
                graph_id: graph.graph_id.clone(),
                revision: graph.revision,
                graph: Some(graph.clone()),
                preserved_node_ids: Vec::new(),
            },
        )?;
        self.persist_orchestration_checkpoint("fresh_execution_initialized", false)?;
        let repository_diff_status =
            if completion_changed_paths(self.repo, &self.manifest.github.base_sha)?.is_empty() {
                "clean"
            } else {
                "changed"
            };
        self.api.append_event(
            "progress",
            json!({
                "event_type": "worker.execution_snapshot_initialized",
                "startup_mode": StartupMode::FreshRun,
                "persisted_graph_presence": startup.persisted_graph_present,
                "persisted_notebook_revision": startup.persisted_notebook_revision,
                "repository_diff_status": repository_diff_status,
                "branch_state": if resumed_branch { "existing" } else { "created" },
                "graph_id": graph.graph_id,
                "graph_revision": graph.revision,
                "notebook_revision": self.notebook.revision,
                "selected_next_decision": "begin_discovery",
            }),
        )?;
        self.api.append_event(
            "progress",
            json!({
                "event_type": "worker.execution_graph_created",
                "startup_mode": StartupMode::FreshRun,
                "persisted_graph_presence": startup.persisted_graph_present,
                "persisted_notebook_revision": startup.persisted_notebook_revision,
                "repository_diff_status": repository_diff_status,
                "branch_state": if resumed_branch { "existing" } else { "created" },
                "graph_id": graph.graph_id,
                "graph_revision": graph.revision,
                "selected_next_decision": "dispatch_discovery_model_call",
            }),
        )
    }

    pub(in crate::hosted) fn checkpoint_notebook(
        &mut self,
        repository_changed: bool,
    ) -> Result<()> {
        self.notebook.revision = self.notebook.revision.saturating_add(1);
        self.notebook.phase = if self.notebook.finalization_revalidation.is_some() {
            ExecutionPhase::Validation
        } else {
            self.phases.active()
        };
        self.notebook.phase_budget = self.budget_telemetry();
        self.notebook.last_successful_action = self.last_successful_action.clone();
        self.notebook.acceptance_criteria_v2 =
            impact_map::acceptance_criteria(&self.notebook.acceptance_criteria);
        self.notebook.impact_evidence = impact_map::evidence_catalog(
            &self.notebook.files_inspected,
            &self.notebook.searches_completed,
        );
        if repository_changed {
            self.notebook.repository_fingerprint =
                repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        }
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
        let plan = self.implementation_plan.clone();
        let facts = HostedReconciliationFacts {
            diff_reviewed: self.diff_reviewed,
            completion_outcome: self.completion_outcome,
            publication: None,
        };
        reconcile_notebook_orchestration(
            &mut self.notebook,
            self.manifest,
            plan.as_ref(),
            &changed_paths,
            &facts,
        );
        self.implementation_plan = implementation_plan_from_notebook(&self.notebook);
        Ok(())
    }

    pub(in crate::hosted) fn persist_orchestration_checkpoint(
        &mut self,
        reason: &str,
        repository_changed: bool,
    ) -> Result<()> {
        self.checkpoint_notebook(repository_changed)?;
        self.api.append_event(
            "progress",
            json!({
                "event_type": "worker.execution_graph_checkpoint",
                "reason": reason,
                "graph_revision": self.notebook.orchestration.graph_revision,
                "domain_event_sequence": self
                    .notebook
                    .orchestration
                    .domain_events
                    .last()
                    .map(crate::execution_graph::ExecutionDomainEvent::sequence),
                "repository_fingerprint": self.notebook.repository_fingerprint,
                "publication": self.notebook.orchestration.publication,
                "notebook": self.notebook,
                "checkpoint": self.notebook_checkpoint_metadata(None),
            }),
        )
    }

    pub(in crate::hosted) fn ordered_implementation_targets(&self) -> Vec<ImplementationTarget> {
        ordered_implementation_targets_from_notebook(&self.notebook)
    }

    pub(in crate::hosted) fn current_implementation_target(&self) -> Option<ImplementationTarget> {
        let graph_target = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget { target, .. }) => Some(&target.target),
            Some(ExecutionDecision::RepairTarget { context, .. }) => Some(&context.target.target),
            _ => None,
        };
        if let Some(target) = graph_target {
            let status = self
                .notebook
                .intended_changes
                .iter()
                .find_map(|change| {
                    (change.change_id == target.change_id)
                        .then(|| {
                            change
                                .targets
                                .iter()
                                .find(|candidate| candidate.path == target.path)
                        })
                        .flatten()
                })
                .map_or(IntendedChangeStatus::Planned, |candidate| candidate.status);
            return Some(ImplementationTarget {
                change_id: target.change_id.clone(),
                path: target.path.clone(),
                role: target.role.clone(),
                new_file: target.new_file,
                intent: target.intent.clone(),
                acceptance_criteria: target.acceptance_criteria_ids.clone(),
                status,
            });
        }
        if self.phases.active() == ExecutionPhase::Repair
            && has_unresolved_validation_failure(&self.notebook)
        {
            return None;
        }
        self.ordered_implementation_targets()
            .into_iter()
            .find(|target| {
                !matches!(
                    target.status,
                    IntendedChangeStatus::Applied | IntendedChangeStatus::Verified
                )
            })
    }

    pub(in crate::hosted) fn implementation_start_context(
        &self,
    ) -> Result<ImplementationStartContext> {
        let mut context = implementation_start_context_from_notebook(
            &self.notebook,
            repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?,
            self.phases
                .phase_limit(self.phases.active())
                .saturating_sub(self.phases.phase_calls(self.phases.active())),
            self.guided_first_write_recovery_issued,
            self.phases.implementation_repair_calls(),
            self.tool_usage.successful_writes,
        );
        let target_context = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget { target, .. }) => Some(target),
            Some(ExecutionDecision::RepairTarget { context, .. }) => Some(&context.target),
            _ => None,
        };
        if let Some(target) = target_context {
            let status = context
                .target_order
                .iter()
                .find(|candidate| {
                    candidate.change_id == target.change_id && candidate.path == target.target.path
                })
                .map_or(IntendedChangeStatus::Planned, |candidate| candidate.status);
            context.current_target = Some(ImplementationTarget {
                change_id: target.change_id.clone(),
                path: target.target.path.clone(),
                role: target.target.role.clone(),
                new_file: target.target.new_file,
                intent: target.intent.clone(),
                acceptance_criteria: target.acceptance_criteria_ids.clone(),
                status,
            });
            context.cached_current_file_content = target.current_file_content.clone();
            context.target_content_hash = target.target_content_hash.clone();
            context.repository_fingerprint = target.repository_fingerprint.clone();
            context.mutation_repair = matches!(
                self.current_decision.as_ref(),
                Some(ExecutionDecision::ExecuteTarget {
                    action: crate::hosted_orchestrator::MutationAction::RepairTarget { .. },
                    ..
                }) | Some(ExecutionDecision::RepairTarget { .. })
            )
            .then(|| {
                self.notebook
                    .mutation_diagnostics
                    .iter()
                    .rev()
                    .find(|diagnostic| diagnostic.target_path == target.target.path)
                    .cloned()
            })
            .flatten();
            context.cached_nearby_context = target.nearby_context.clone();
            context.graph_node_id = Some(target.node_id.clone());
            context.dependency_evidence = target.dependency_evidence.clone();
            context.relevant_impact_areas = self
                .notebook
                .impact_map
                .iter()
                .filter(|area| area.candidate_paths.contains(&target.target.path))
                .cloned()
                .collect();
            context.related_test_evidence = self
                .notebook
                .orchestration
                .evidence
                .files
                .values()
                .filter(|evidence| {
                    evidence.repository_fingerprint == self.notebook.repository_fingerprint && {
                        let path = evidence.path.to_ascii_lowercase();
                        path.contains("test") || path.contains("spec")
                    }
                })
                .map(crate::execution_graph::FileExcerpt::from)
                .take(4)
                .collect();
            context.allowed_tools = target.allowed_tools.clone();
            context.remaining_node_budget = Some(target.remaining_node_budget.clone());
            context.acceptance_criteria_ids = target.acceptance_criteria_ids.clone();
            context.assigned_acceptance_criteria = self
                .notebook
                .acceptance_criteria_v2
                .iter()
                .filter(|criterion| target.acceptance_criteria_ids.contains(&criterion.id))
                .cloned()
                .collect();
            if target.current_file_content.is_some() {
                context
                    .missing_file_contents
                    .retain(|path| path != &target.target.path);
            }
            context.instruction = if context.mutation_repair.is_some() {
                "Repair only current_target from its exact current content. The rejected mutation was not applied. Follow mutation_repair.repair_strategy and do not repeat the rejected patch strategy.".into()
            } else {
                "Mutate only current_target using the persisted evidence bundle. Do not rediscover the repository. Return exactly one target-bound mutation, or a concrete typed blocker; verification is deterministic and model-free.".into()
            };
        }
        Ok(context)
    }

    pub(in crate::hosted) fn reconcile_authoritative_target_state(
        &mut self,
    ) -> Result<ImplementationCompletionStatus> {
        self.reconcile_repository_failure_supersession()?;
        let snapshot = self.build_execution_snapshot()?;
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        // Compatibility copies are projections of the graph/event snapshot.
        // Never infer target completion merely from a changed path: two plan
        // nodes may intentionally address the same file.
        self.tool_failures = self.notebook.failed_changes.clone();
        self.implementation_plan = implementation_plan_from_notebook(&self.notebook);
        validate_lifecycle_invariants(
            &self.notebook.intended_changes,
            &self.notebook.validation_evidence,
            &snapshot.current_repository.fingerprint,
        )
        .map_err(|error| anyhow!("lifecycle invariant violated: {error}"))?;
        let unresolved = snapshot.failures.has_unresolved();
        let status = implementation_completion_status(
            &self.notebook.intended_changes,
            &changed_paths,
            unresolved,
            self.write_blocker.is_some() || !self.notebook.blocking_unknowns.is_empty(),
        );
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.implementation_completion_reconciled",
                "status": status,
                "implementation_substate": self.notebook.implementation_substate,
                "changed_paths": changed_paths,
                "remaining_work": self.notebook.remaining_work_v2,
                "notebook_revision": self.notebook.revision,
            }),
            "implementation completion reconciliation",
        );
        Ok(status)
    }

    pub(in crate::hosted) fn reconcile_repository_failure_supersession(&mut self) -> Result<()> {
        let fingerprint = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let path_counts = self
            .notebook
            .orchestration
            .graph
            .as_ref()
            .map(|graph| {
                graph
                    .nodes
                    .iter()
                    .filter_map(|node| node.target.as_ref().map(|target| target.path.clone()))
                    .fold(BTreeMap::<String, usize>::new(), |mut counts, path| {
                        *counts.entry(path).or_default() += 1;
                        counts
                    })
            })
            .unwrap_or_default();
        let superseded = self
            .notebook
            .orchestration
            .failures
            .unresolved()
            .filter(|failure| failure.category.is_supersedable_by_applied_target())
            .filter_map(|failure| {
                let target_path = failure.target_path.as_ref()?;
                if !changed_paths.contains(target_path) {
                    return None;
                }
                let node = self
                    .notebook
                    .orchestration
                    .graph
                    .as_ref()?
                    .node(&failure.node_id)?;
                let path_uniquely_identifies_node = path_counts.get(target_path) == Some(&1);
                let node_has_mutation_evidence = node.evidence_ids.iter().any(|evidence_id| {
                    self.notebook
                        .orchestration
                        .evidence
                        .records
                        .get(evidence_id)
                        .is_some_and(|evidence| {
                            evidence.kind == crate::execution_graph::EvidenceKind::Mutation
                                && evidence.node_id.as_ref() == Some(&failure.node_id)
                        })
                });
                (node.kind.is_mutation()
                    && (path_uniquely_identifies_node
                        || node.status.is_success()
                        || node_has_mutation_evidence))
                    .then(|| (failure.id.clone(), failure.node_id.clone()))
            })
            .collect::<Vec<_>>();
        for (failure_id, node_id) in superseded {
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::FailureSuperseded {
                    sequence: self.next_domain_event_sequence(),
                    node_id,
                    failure_id,
                    repository_fingerprint: fingerprint.clone(),
                },
            )?;
        }
        Ok(())
    }

    pub(in crate::hosted) fn build_execution_snapshot(
        &mut self,
    ) -> Result<crate::execution_graph::ExecutionSnapshot> {
        let fingerprint = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        self.notebook.repository_fingerprint = fingerprint.clone();
        // Wall-clock admission is part of the immutable snapshot consumed by
        // the orchestrator. A legacy timer must never independently choose a
        // terminal or partial outcome before graph reconciliation runs.
        self.notebook.orchestration.budget.elapsed = self
            .notebook
            .orchestration
            .budget
            .elapsed
            .max(self.execution_started_at.elapsed());
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
        let changed_path_set = changed_paths.iter().cloned().collect::<BTreeSet<_>>();
        let plan = self.implementation_plan.clone();
        let facts = HostedReconciliationFacts {
            diff_reviewed: self.diff_reviewed,
            completion_outcome: self.completion_outcome,
            publication: None,
        };
        reconcile_notebook_orchestration(
            &mut self.notebook,
            self.manifest,
            plan.as_ref(),
            &changed_paths,
            &facts,
        );
        self.implementation_plan = implementation_plan_from_notebook(&self.notebook);
        let dependency_lock_hash = dependency_lock_fingerprint(&self.repo.root)?;
        let environment_fingerprint =
            relevant_environment_fingerprint(&self.manifest.execution_policy)?;
        let working_directory = self.repo.root.to_string_lossy().into_owned();
        if let Some(graph) = self.notebook.orchestration.graph.as_mut() {
            let mut graph_changed = false;
            for gate in graph
                .nodes
                .iter_mut()
                .filter_map(|node| node.validation.as_mut())
            {
                if gate.working_directory != working_directory
                    || gate.dependency_lock_hash != dependency_lock_hash
                    || gate.relevant_environment_fingerprint != environment_fingerprint
                {
                    gate.working_directory.clone_from(&working_directory);
                    gate.dependency_lock_hash.clone_from(&dependency_lock_hash);
                    gate.relevant_environment_fingerprint
                        .clone_from(&environment_fingerprint);
                    graph_changed = true;
                }
            }
            if graph_changed {
                graph.revision = graph.revision.saturating_add(1);
                self.notebook.orchestration.graph_revision = graph.revision;
            }
        }
        let repository = crate::execution_graph::RepositorySnapshot {
            fingerprint: fingerprint.clone(),
            source_tree_hash: fingerprint,
            dependency_lock_hash,
            relevant_environment_fingerprint: environment_fingerprint,
            changed_paths: changed_path_set,
        };
        let snapshot = self
            .notebook
            .orchestration
            .snapshot(self.manifest.execution.execution_id.to_string(), repository);
        snapshot
            .validate_invariants()
            .map_err(|error| anyhow!("hosted orchestration snapshot is invalid: {error}"))?;
        Ok(snapshot)
    }

    pub(in crate::hosted) fn reconcile_execution_and_apply(
        &mut self,
    ) -> Result<DecisionExecutionResult> {
        self.reconcile_repository_failure_supersession()?;
        let snapshot = self.build_execution_snapshot()?;
        let decision = reconcile_execution(&snapshot)
            .map_err(|error| anyhow!("hosted orchestration invariant failed: {error}"))?;
        let decision_key = execution_decision_idempotency_key(&snapshot, &decision);
        if !orchestration_decision_is_new(
            self.notebook.last_orchestration_decision_key.as_deref(),
            &decision_key,
        ) {
            self.current_decision = Some(decision.clone());
            return Ok(DecisionExecutionResult {
                decision,
                phase_decision: PhaseDecision::Stay,
                persistence_error: None,
            });
        }
        let previous_key = self
            .notebook
            .last_orchestration_decision_key
            .replace(decision_key.clone());
        let result = match self.apply_execution_decision(decision) {
            Ok(result) => result,
            Err(error) => {
                self.notebook.last_orchestration_decision_key = previous_key;
                return Err(error);
            }
        };
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.execution_decision_applied",
                "decision": execution_decision_name(&result.decision),
                "stage": result.decision.stage(),
                "phase": self.phases.active(),
                "graph_revision": self.notebook.orchestration.graph_revision,
                "decision_idempotency_key": decision_key,
                "remaining_required_nodes": snapshot
                    .remaining_required_nodes()
                    .iter()
                    .map(|node| node.id.as_str())
                    .collect::<Vec<_>>(),
            }),
            "execution decision",
        );
        Ok(result)
    }

    pub(in crate::hosted) fn finalize_guardrail_outcome(
        &mut self,
        expected: OrchestratedMissionOutcome,
    ) -> Result<()> {
        for _ in 0..2 {
            let result = self.reconcile_execution_and_apply()?;
            match result.decision {
                ExecutionDecision::StopForGuardrail { outcome, .. } if outcome == expected => {
                    continue;
                }
                ExecutionDecision::Finish { outcome } if outcome == expected => {
                    self.completion_outcome = Some(outcome);
                    return Ok(());
                }
                decision => {
                    bail!(
                        "hosted orchestrator returned `{}` while finalizing guardrail outcome `{expected:?}`",
                        execution_decision_name(&decision)
                    );
                }
            }
        }
        bail!("hosted orchestrator did not finalize guardrail outcome `{expected:?}`")
    }

    pub(in crate::hosted) fn peek_execution_decision(&mut self) -> Result<ExecutionDecision> {
        self.reconcile_repository_failure_supersession()?;
        let snapshot = self.build_execution_snapshot()?;
        reconcile_execution(&snapshot)
            .map_err(|error| anyhow!("hosted orchestration invariant failed: {error}"))
    }

    pub(in crate::hosted) fn restored_validation_results(
        &mut self,
    ) -> Result<Vec<ValidationResult>> {
        let snapshot = self.build_execution_snapshot()?;
        restored_validation_results_from_snapshot(&snapshot)
    }

    pub(in crate::hosted) fn reconstruct_implementation_outcome(
        &mut self,
    ) -> Result<ImplementationOutcome> {
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
        let all_required_targets_applied =
            self.notebook
                .orchestration
                .graph
                .as_ref()
                .is_some_and(|graph| {
                    graph
                        .nodes
                        .iter()
                        .filter(|node| node.required && node.kind.is_mutation())
                        .all(|node| node.status.is_success())
                });
        let declaration = if all_required_targets_applied {
            deterministic_complete_declaration(
                &self.notebook.planned_changes,
                &self.notebook.acceptance_criteria,
                &changed_paths,
                &self.notebook.remaining_work_v2,
                &self.tool_failures,
            )
        } else {
            deterministic_partial_declaration(
                &self.notebook.planned_changes,
                &changed_paths,
                &self.notebook.remaining_work_v2,
            )
        }
        .or_else(|| self.declaration.clone());
        Ok(ImplementationOutcome {
            summary: if all_required_targets_applied {
                "Reconstructed completed implementation from the authoritative execution graph."
                    .into()
            } else {
                "Reconstructed reviewable partial implementation from the authoritative execution graph."
                    .into()
            },
            budget_exhausted: !all_required_targets_applied,
            explicit_declaration: declaration,
        })
    }

    pub(in crate::hosted) fn restored_completion_evaluation(
        &mut self,
        implementation: &ImplementationOutcome,
        validation: &[ValidationResult],
        reviewed_paths: &[String],
    ) -> Result<CompletionEvaluation> {
        let repository_fingerprint =
            repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        if let Some(artifact) =
            valid_completion_artifact(&self.notebook, &repository_fingerprint, reviewed_paths)
        {
            return Ok(artifact.evaluation.clone());
        }
        let unrecovered =
            self.reconcile_write_failures(implementation, validation, reviewed_paths)?;
        let fallback = completion_fallback(
            implementation,
            self.impact_map.as_ref(),
            self.implementation_plan.as_ref(),
            &unrecovered,
            reviewed_paths,
            &self.notebook.acceptance_criteria,
            validation,
            project_verification_policy(self.manifest),
        );
        if self
            .completion_outcome
            .is_some_and(|outcome| mission_outcome_from_completion(fallback.status) != outcome)
        {
            bail!(
                "persisted completion outcome cannot be reconstructed from current graph evidence"
            );
        }
        Ok(fallback)
    }

    pub(in crate::hosted) fn finalization_requires_revalidation(
        &self,
        repository_fingerprint: &str,
        changed_paths: &[String],
    ) -> bool {
        notebook_finalization_requires_revalidation(
            &self.notebook,
            repository_fingerprint,
            changed_paths,
        )
    }

    pub(in crate::hosted) fn append_execution_domain_event(
        &mut self,
        event: crate::execution_graph::ExecutionDomainEvent,
    ) -> Result<()> {
        let mut snapshot = self.build_execution_snapshot()?;
        snapshot
            .append_event(event)
            .map_err(|error| anyhow!("could not apply hosted execution event: {error}"))?;
        let mut orchestration = std::mem::take(&mut self.notebook.orchestration);
        orchestration.replace_from_snapshot(&snapshot);
        orchestration.materialize_legacy_notebook(&mut self.notebook);
        self.notebook.orchestration = orchestration;
        Ok(())
    }

    pub(in crate::hosted) fn graph_node_id(
        &self,
        kind: crate::execution_graph::ExecutionNodeKind,
    ) -> Result<crate::execution_graph::ExecutionNodeId> {
        self.notebook
            .orchestration
            .graph
            .as_ref()
            .and_then(|graph| graph.nodes.iter().find(|node| node.kind == kind))
            .map(|node| node.id.clone())
            .ok_or_else(|| anyhow!("hosted execution graph has no {kind:?} node"))
    }

    pub(in crate::hosted) fn record_discovery_completed(&mut self) -> Result<()> {
        let node_id = self.graph_node_id(crate::execution_graph::ExecutionNodeKind::Discovery)?;
        let fingerprint = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let recovered = self
            .notebook
            .orchestration
            .failures
            .unresolved_for_node(&node_id)
            .map(|failure| failure.id.clone())
            .collect::<Vec<_>>();
        for failure_id in recovered {
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::FailureRecovered {
                    sequence: self.next_domain_event_sequence(),
                    node_id: node_id.clone(),
                    failure_id,
                    repository_fingerprint: fingerprint.clone(),
                },
            )?;
        }
        if self
            .notebook
            .orchestration
            .graph
            .as_ref()
            .and_then(|graph| {
                graph
                    .nodes
                    .iter()
                    .find(|node| node.kind == crate::execution_graph::ExecutionNodeKind::Discovery)
            })
            .is_some_and(|node| node.status.is_success())
        {
            return Ok(());
        }
        let sequence = self
            .notebook
            .orchestration
            .domain_events
            .last()
            .map_or(1, |event| event.sequence().saturating_add(1));
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::DiscoveryCompleted {
                sequence,
                repository_fingerprint: fingerprint,
            },
        )
    }

    pub(in crate::hosted) fn record_discovery_failure(&mut self, detail: &str) -> Result<()> {
        let node_id = self.graph_node_id(crate::execution_graph::ExecutionNodeKind::Discovery)?;
        let fingerprint = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let failure_id = crate::execution_graph::FailureId::new(format!(
            "discovery-{}",
            sha256_text(&format!("{fingerprint}\0{detail}"))
        ));
        let failure = crate::execution_graph::FailureRecord::new(
            failure_id,
            node_id.clone(),
            crate::execution_graph::FailureCategory::ModelArtifactRecoverable,
            1,
            fingerprint,
            detail,
        );
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::FailureRecorded {
                sequence: self.next_domain_event_sequence(),
                failure,
            },
        )
    }

    pub(in crate::hosted) fn record_planning_failure(&mut self, raw_arguments: &str) -> Result<()> {
        let node_id = self.graph_node_id(crate::execution_graph::ExecutionNodeKind::Planning)?;
        let fingerprint = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let validation_errors = self
            .notebook
            .planning_repair
            .as_ref()
            .map(|repair| repair.invalid_fields.clone())
            .unwrap_or_else(|| vec!["$: implementation plan validation failed".into()]);
        let previous_plan = json_object_from_text(raw_arguments).unwrap_or(Value::Null);
        let detail = json!({
            "validation_errors": validation_errors,
            "previous_plan": previous_plan,
        })
        .to_string();
        let failure_id = crate::execution_graph::FailureId::new(format!(
            "planning-{}",
            sha256_text(&format!("{fingerprint}\0{detail}"))
        ));
        let failure = crate::execution_graph::FailureRecord::new(
            failure_id,
            node_id,
            crate::execution_graph::FailureCategory::ModelArtifactRecoverable,
            1,
            fingerprint,
            detail,
        );
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::FailureRecorded {
                sequence: self.next_domain_event_sequence(),
                failure,
            },
        )
    }

    pub(in crate::hosted) fn record_planning_failures_recovered(
        &mut self,
        repository_fingerprint: &str,
    ) -> Result<()> {
        let node_id = self.graph_node_id(crate::execution_graph::ExecutionNodeKind::Planning)?;
        let failure_ids = self
            .notebook
            .orchestration
            .failures
            .unresolved_for_node(&node_id)
            .map(|failure| failure.id.clone())
            .collect::<Vec<_>>();
        for failure_id in failure_ids {
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::FailureRecovered {
                    sequence: self.next_domain_event_sequence(),
                    node_id: node_id.clone(),
                    failure_id,
                    repository_fingerprint: repository_fingerprint.to_owned(),
                },
            )?;
        }
        Ok(())
    }

    pub(in crate::hosted) fn record_active_target_failure(
        &mut self,
        category: crate::execution_graph::FailureCategory,
        detail: &str,
    ) -> Result<()> {
        let (node_id, target_path) = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                node_id, target, ..
            }) => (node_id.clone(), Some(target.target.path.clone())),
            Some(ExecutionDecision::RepairTarget {
                node_id, context, ..
            }) => (node_id.clone(), Some(context.target.target.path.clone())),
            _ => return Ok(()),
        };
        let fingerprint = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let failure_id = crate::execution_graph::FailureId::new(format!(
            "target-{}",
            sha256_text(&format!("{node_id}\0{fingerprint}\0{detail}"))
        ));
        let mut failure = crate::execution_graph::FailureRecord::new(
            failure_id,
            node_id.clone(),
            category,
            self.notebook
                .orchestration
                .budget
                .usage_for(&node_id)
                .repair_attempts
                .saturating_add(1),
            fingerprint,
            detail,
        );
        failure.target_path = target_path;
        let older_failures = self
            .notebook
            .orchestration
            .failures
            .unresolved_for_node(&node_id)
            .filter(|older| older.category == category && older.id != failure.id)
            .map(|older| older.id.clone())
            .collect::<Vec<_>>();
        for failure_id in older_failures {
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::FailureSuperseded {
                    sequence: self.next_domain_event_sequence(),
                    node_id: node_id.clone(),
                    failure_id,
                    repository_fingerprint: failure.repository_fingerprint.clone(),
                },
            )?;
        }
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::MutationRejected {
                sequence: self.next_domain_event_sequence(),
                node_id,
                failure,
            },
        )
    }

    pub(in crate::hosted) fn record_validation_failures(
        &mut self,
        failures: &[ValidationResult],
        repair_attempt: usize,
    ) -> Result<()> {
        let fingerprint = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let graph = self
            .notebook
            .orchestration
            .graph
            .as_ref()
            .context("validation failed before an execution graph was created")?;
        let mutation_target_paths = graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_mutation())
            .filter_map(|node| node.target.as_ref().map(|target| target.path.clone()))
            .collect::<Vec<_>>();
        let mapped = failures
            .iter()
            .filter_map(|failure| {
                graph
                    .nodes
                    .iter()
                    .find(|node| {
                        node.validation
                            .as_ref()
                            .is_some_and(|gate| gate.gate_id == failure.id)
                    })
                    .map(|node| (failure, node.id.clone()))
            })
            .collect::<Vec<_>>();
        for (failure, node_id) in mapped {
            let Some(category) = validation_failure_category(&failure.status) else {
                // Cancellation is checkpointed by the active-process guard and
                // must not be converted into target repair work.
                continue;
            };
            let failure_id = crate::execution_graph::FailureId::new(format!(
                "validation-{}",
                sha256_text(&format!(
                    "{}\0{}\0{}\0{}",
                    failure.id, fingerprint, repair_attempt, failure.output
                ))
            ));
            let mut record = crate::execution_graph::FailureRecord::new(
                failure_id.clone(),
                node_id.clone(),
                category,
                u32::try_from(repair_attempt).unwrap_or(u32::MAX),
                fingerprint.clone(),
                format!("{}: {}", failure.id, truncate_text(&failure.output, 2_000)),
            );
            let diagnostics = format!("{}\n{}", failure.command, failure.output);
            if category == crate::execution_graph::FailureCategory::ValidationFailure {
                record.target_path =
                    validation_failure_target_hint(&mutation_target_paths, &diagnostics);
            }
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::FailureRecorded {
                    sequence: self.next_domain_event_sequence(),
                    failure: record,
                },
            )?;
            let validation_fingerprint = self
                .notebook
                .validation_evidence
                .iter()
                .rev()
                .find(|evidence| evidence.gate_id == failure.id)
                .map_or_else(
                    || sha256_text(&format!("{}\0{fingerprint}", failure.command)),
                    |evidence| evidence.command_fingerprint.clone(),
                );
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::ValidationFailed {
                    sequence: self.next_domain_event_sequence(),
                    node_id,
                    failure_id,
                    fingerprint: validation_fingerprint,
                },
            )?;
        }
        Ok(())
    }

    pub(in crate::hosted) fn record_active_target_applied(
        &mut self,
        target_path: &str,
    ) -> Result<()> {
        let validation_recovery = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                action: crate::hosted_orchestrator::MutationAction::RepairTarget { failure, .. },
                ..
            }) if failure.category
                == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                Some((failure.id.clone(), failure.node_id.clone()))
            }
            Some(ExecutionDecision::RepairTarget {
                failure_id,
                context,
                ..
            }) if context.failure.category
                == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                Some((failure_id.clone(), context.failure.node_id.clone()))
            }
            _ => None,
        }
        .or_else(|| {
            self.notebook
                .orchestration
                .failures
                .unresolved()
                .find(|failure| {
                    failure.category == crate::execution_graph::FailureCategory::ValidationFailure
                        && failure.target_path.as_deref() == Some(target_path)
                })
                .map(|failure| (failure.id.clone(), failure.node_id.clone()))
        });
        let node_id = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget { node_id, .. })
            | Some(ExecutionDecision::RepairTarget { node_id, .. }) => node_id.clone(),
            _ => return Ok(()),
        };
        let fingerprint = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let evidence_id = format!(
            "mutation-{}",
            sha256_text(&format!("{node_id}\0{target_path}\0{fingerprint}"))
        );
        let superseded_mutation_failures = self
            .notebook
            .orchestration
            .failures
            .unresolved_for_node(&node_id)
            .filter(|failure| {
                failure.category == crate::execution_graph::FailureCategory::MutationConflict
                    && failure.target_path.as_deref() == Some(target_path)
            })
            .map(|failure| failure.id.clone())
            .collect::<Vec<_>>();
        for failure_id in superseded_mutation_failures {
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::FailureRecovered {
                    sequence: self.next_domain_event_sequence(),
                    node_id: node_id.clone(),
                    failure_id: failure_id.clone(),
                    repository_fingerprint: fingerprint.clone(),
                },
            )?;
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::FailureSuperseded {
                    sequence: self.next_domain_event_sequence(),
                    node_id: node_id.clone(),
                    failure_id,
                    repository_fingerprint: fingerprint.clone(),
                },
            )?;
        }
        let sequence = self
            .notebook
            .orchestration
            .domain_events
            .last()
            .map_or(1, |event| event.sequence().saturating_add(1));
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::MutationApplied {
                sequence,
                node_id,
                target_path: target_path.to_owned(),
                repository_fingerprint: fingerprint.clone(),
                evidence_id,
            },
        )?;
        if let Some((failure_id, validation_node_id)) = validation_recovery {
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::FailureRecovered {
                    sequence: self.next_domain_event_sequence(),
                    node_id: validation_node_id,
                    failure_id,
                    repository_fingerprint: fingerprint,
                },
            )?;
        }
        Ok(())
    }

    pub(in crate::hosted) fn prepare_active_target_context(
        &mut self,
    ) -> Result<TargetContextPreparationResult> {
        let (node_id, target_path, accepted_intent_hash) = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                node_id,
                target: context,
                ..
            }) => (
                node_id.clone(),
                context.target.path.clone(),
                context.accepted_intent_hash.clone(),
            ),
            _ => return Ok(TargetContextPreparationResult::Prepared),
        };
        let fingerprint = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let mut evidence = self
            .notebook
            .orchestration
            .evidence
            .reusable_file(&target_path, &fingerprint, None)
            .cloned();
        if evidence.is_none() {
            let target = safe_repo_path(&self.repo.root, &target_path, false)?;
            let content = fs::read_to_string(&target).with_context(|| {
                format!("could not prepare exact UTF-8 target context for {target_path}")
            })?;
            let captured = crate::execution_graph::FileEvidence::capture(
                &target_path,
                &fingerprint,
                None,
                content,
                false,
            );
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::RepositoryEvidenceRecorded {
                    sequence: self.next_domain_event_sequence(),
                    evidence_id: captured.evidence_id.clone(),
                    repository_fingerprint: fingerprint.clone(),
                    evidence: Some(captured.clone()),
                },
            )?;
            evidence = Some(captured);
        }
        let target_content_hash = evidence
            .as_ref()
            .map(|evidence| evidence.content_hash.clone());
        let already_prepared = target_context_already_prepared(
            &self.notebook.orchestration.domain_events,
            &node_id,
            &target_path,
            &target_content_hash,
            &fingerprint,
            &accepted_intent_hash,
        );
        if already_prepared {
            return Ok(TargetContextPreparationResult::TargetContextAlreadyPrepared);
        }
        let evidence_ids = evidence
            .as_ref()
            .map(|evidence| vec![evidence.evidence_id.clone()])
            .unwrap_or_default();
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::TargetContextPrepared {
                sequence: self.next_domain_event_sequence(),
                node_id,
                target_path: target_path.clone(),
                repository_fingerprint: crate::execution_graph::RepositoryFingerprint::new(
                    fingerprint.clone(),
                ),
                target_content_hash: target_content_hash.clone(),
                accepted_intent_hash,
                evidence_ids,
            },
        )?;
        if let Some(evidence) = evidence {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.evidence_cache_hit",
                    "target": target_path,
                    "content_hash": evidence.content_hash,
                    "evidence_id": evidence.evidence_id,
                    "repository_fingerprint": fingerprint,
                    "model_call_consumed": false,
                    "tool_operation_consumed": false,
                    "progress_window_consumed": false,
                }),
                "target evidence cache hit",
            );
        }
        self.persist_orchestration_checkpoint("target_context_prepared", false)?;
        Ok(TargetContextPreparationResult::Prepared)
    }

    pub(in crate::hosted) fn record_active_target_mutation_produced(
        &mut self,
        target_path: &str,
        before_content_hash: Option<String>,
        after_content_hash: Option<String>,
    ) -> Result<()> {
        let (node_id, expected) = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                node_id,
                action:
                    crate::hosted_orchestrator::MutationAction::MutateTarget {
                        expected_repository_fingerprint,
                        ..
                    },
                ..
            }) => (node_id.clone(), expected_repository_fingerprint.clone()),
            Some(ExecutionDecision::ExecuteTarget { node_id, .. }) => (
                node_id.clone(),
                crate::execution_graph::RepositoryFingerprint::new(
                    self.notebook.repository_fingerprint.clone(),
                ),
            ),
            _ => return Ok(()),
        };
        let fingerprint = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::TargetMutationProduced {
                sequence: self.next_domain_event_sequence(),
                node_id,
                target_path: target_path.to_owned(),
                expected_repository_fingerprint: expected,
                repository_fingerprint: crate::execution_graph::RepositoryFingerprint::new(
                    fingerprint,
                ),
                before_content_hash,
                after_content_hash,
            },
        )
    }

    pub(in crate::hosted) fn verify_active_target_state(&mut self) -> Result<()> {
        let (node_id, target_path) = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                node_id,
                action: crate::hosted_orchestrator::MutationAction::VerifyTargetState { target, .. },
                ..
            }) => (node_id.clone(), target.path.clone()),
            _ => return Ok(()),
        };
        let produced = self
            .notebook
            .orchestration
            .domain_events
            .iter()
            .rev()
            .find_map(|event| match event {
                crate::execution_graph::ExecutionDomainEvent::TargetMutationProduced {
                    node_id: produced_node,
                    target_path,
                    before_content_hash,
                    after_content_hash,
                    ..
                } if produced_node == &node_id => Some((
                    target_path.clone(),
                    before_content_hash.clone(),
                    after_content_hash.clone(),
                )),
                _ => None,
            })
            .context("target verification requires a produced mutation event")?;
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
        let repository_fingerprint =
            repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let mutation_tool = self
            .notebook
            .write_attempts
            .iter()
            .rev()
            .find(|attempt| attempt.target == target_path)
            .map(|attempt| attempt.tool.clone());
        let usage = self.notebook.orchestration.budget.usage_for(&node_id);
        let mutation_attempt = usage.model_calls_consumed;
        let repair_attempt = usage.repair_attempts;
        if produced.0 != target_path
            || produced.1 == produced.2
            || !changed_paths.contains(&target_path)
        {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.target_verification_completed",
                    "node_id": node_id,
                    "target": target_path,
                    "mutation_tool": mutation_tool.clone(),
                    "verified": false,
                    "before_content_hash": produced.1,
                    "after_content_hash": produced.2,
                    "repository_fingerprint": repository_fingerprint,
                    "failure_category": MutationApplicationFailure::MutationProducedNoChange,
                    "mutation_attempt": mutation_attempt,
                    "repair_attempt": repair_attempt,
                }),
                "target mutation verification",
            );
            self.record_active_target_failure(
                crate::execution_graph::FailureCategory::MutationConflict,
                "MutationNotProduced: deterministic verification found no attributable target change",
            )?;
            return Ok(());
        }
        self.record_active_target_applied(&target_path)?;
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.target_verification_completed",
                "node_id": node_id,
                "target": target_path,
                "mutation_tool": mutation_tool,
                "verified": true,
                "before_content_hash": produced.1,
                "after_content_hash": produced.2,
                "repository_fingerprint": repository_fingerprint,
                "failure_category": Value::Null,
                "mutation_attempt": mutation_attempt,
                "repair_attempt": repair_attempt,
            }),
            "target mutation verification",
        );
        self.persist_orchestration_checkpoint("target_state_verified", true)
    }

    pub(in crate::hosted) fn reconcile_active_phase(
        &mut self,
        _reason: &str,
    ) -> Result<PhaseDecision> {
        let result = self.reconcile_execution_and_apply()?;
        match result.decision {
            ExecutionDecision::StopForGuardrail {
                outcome: OrchestratedMissionOutcome::PartialReviewable,
                ..
            }
            | ExecutionDecision::Finish { .. } => Ok(result.phase_decision),
            ExecutionDecision::StopForGuardrail { outcome, reason } => {
                if reason == crate::execution_graph::GuardrailReason::NodeBudgetExhausted
                    && self.has_unresolved_mutation_application_failure()
                {
                    return Err(self.mutation_application_exhausted_failure());
                }
                Err(self.execution_failure(
                    "execution_graph_guardrail",
                    format!(
                        "The authoritative execution graph stopped at {reason:?} with outcome {outcome:?}."
                    ),
                    None,
                    true,
                    "Resume from the persisted graph after resolving the reported guardrail.",
                ))
            }
            _ => Ok(result.phase_decision),
        }
    }

    pub(in crate::hosted) fn invalidate_finalization_after_remote_reconciliation(
        &mut self,
        repository_fingerprint: &str,
    ) -> Result<()> {
        use crate::execution_graph::ExecutionDomainEvent;

        let invalidated_after_sequence = self
            .notebook
            .orchestration
            .domain_events
            .last()
            .map_or(0, ExecutionDomainEvent::sequence);
        self.notebook.finalization_revalidation = Some(FinalizationRevalidation {
            repository_fingerprint: repository_fingerprint.to_owned(),
            invalidated_after_sequence,
        });

        self.diff_reviewed = false;
        self.diff_review_cursor = 0;
        self.diff_review_digest = None;
        self.declaration = None;
        self.completion_outcome = None;
        self.notebook.completion_artifact = None;
        self.current_decision = None;
        let invalidation = finalization_invalidation_event(
            &self.notebook.orchestration,
            self.next_domain_event_sequence(),
            repository_fingerprint,
        )?;
        let superseded_graph_validation_count = match &invalidation {
            ExecutionDomainEvent::FinalizationInvalidated {
                stale_validation_evidence_ids,
                ..
            } => stale_validation_evidence_ids.len(),
            _ => unreachable!("finalization invalidation builder returns one event type"),
        };
        self.append_execution_domain_event(invalidation)?;
        self.persist_orchestration_checkpoint(
            "remote_reconciliation_invalidated_finalization",
            true,
        )?;
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.remote_reconciliation_invalidated_finalization",
                "source_tree_hash": repository_fingerprint,
                "invalidated_after_sequence": invalidated_after_sequence,
                "superseded_validation_count": superseded_graph_validation_count,
                "next_phase": ExecutionPhase::Validation,
            }),
            "remote reconciliation finalization invalidation",
        );
        Ok(())
    }

    pub(in crate::hosted) fn complete_finalization_revalidation(&mut self) -> Result<()> {
        let Some(revalidation) = self.notebook.finalization_revalidation.clone() else {
            return Ok(());
        };
        let repository_fingerprint =
            repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        validate_reconciled_finalization_route(
            &self.notebook.orchestration,
            &revalidation,
            &repository_fingerprint,
        )?;
        self.notebook.finalization_revalidation = None;
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.remote_reconciliation_finalization_reestablished",
                "source_tree_hash": repository_fingerprint,
                "invalidated_after_sequence": revalidation.invalidated_after_sequence,
                "route": ["validation", "diff_review", "completion_evaluation", "publication"],
            }),
            "remote reconciliation finalization proof",
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_context_identity_is_idempotent_and_fingerprint_bound() {
        let node_id = crate::execution_graph::ExecutionNodeId::new("source-000");
        let content_hash = Some("content-1".to_owned());
        let event = crate::execution_graph::ExecutionDomainEvent::TargetContextPrepared {
            sequence: 1,
            node_id: node_id.clone(),
            target_path: "src/theme.ts".into(),
            repository_fingerprint: crate::execution_graph::RepositoryFingerprint::new("tree-1"),
            target_content_hash: content_hash.clone(),
            accepted_intent_hash: "intent-1".into(),
            evidence_ids: vec!["file-1".into()],
        };
        assert!(target_context_already_prepared(
            std::slice::from_ref(&event),
            &node_id,
            "src/theme.ts",
            &content_hash,
            "tree-1",
            "intent-1",
        ));
        assert!(!target_context_already_prepared(
            std::slice::from_ref(&event),
            &node_id,
            "src/theme.ts",
            &content_hash,
            "tree-2",
            "intent-1",
        ));
    }
}
