// Extracted from the hosted execution composition root.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hosted) enum TargetContextPreparationResult {
    Prepared,
    TargetContextAlreadyPrepared,
}

struct TargetContextIdentity<'a> {
    node_id: &'a crate::execution_graph::ExecutionNodeId,
    target_path: &'a str,
    operation: &'a crate::execution_graph::TargetOperation,
    source_path: Option<&'a str>,
    target_content_hash: &'a Option<String>,
    repository_fingerprint: &'a str,
    accepted_intent_hash: &'a str,
}

fn validate_critical_worker_event_fields(data: &Value) -> std::result::Result<(), String> {
    let Some(object) = data.as_object() else {
        return Ok(());
    };
    let Some(event_type) = object.get("event_type").and_then(Value::as_str) else {
        return Ok(());
    };
    let required: &[&str] = match event_type {
        "worker.repository_operation_reduction_started"
        | "worker.successful_mutation_reconciliation_started" => &[
            "node_id",
            "operation",
            "attempt_id",
            "repair_intent_kind",
            "repair_budget_owner",
            "repository_fingerprint_before",
            "repository_fingerprint_after",
            "verification_evidence_id",
            "node_status_before",
        ],
        "worker.repository_operation_reduced"
        | "worker.node_completed_from_verified_write"
        | "worker.successful_mutation_reconciliation_completed" => &[
            "node_id",
            "operation",
            "attempt_id",
            "repair_intent_kind",
            "repair_budget_owner",
            "repository_fingerprint_before",
            "repository_fingerprint_after",
            "verification_evidence_id",
            "node_status_before",
            "node_status_after",
        ],
        "worker.implementation_barrier_checked"
        | "worker.implementation_barrier_rejected_validation" => &[
            "validation_node_id",
            "implementation_barrier_result",
            "required_implementation_nodes",
            "completed_implementation_nodes",
            "unresolved_nodes",
            "repository_fingerprint",
        ],
        "worker.repair_intent_session_created"
        | "worker.repair_budget_scope_resolved"
        | "worker.validation_repair_target_bound" => &[
            "repair_intent_kind",
            "repair_budget_owner",
            "repair_budget",
            "target_node_id",
        ],
        "worker.validation_repair_node_created"
        | "worker.validation_repair_node_started"
        | "worker.validation_repair_target_linked"
        | "worker.validation_repair_operation_verified"
        | "worker.validation_repair_node_completed"
        | "worker.originating_implementation_node_preserved" => &[
            "repair_node_id",
            "originating_implementation_node_id",
            "target_id",
            "target_path",
            "validation_node_id",
            "failure_id",
            "failure_revision",
            "implementation_status_before",
            "implementation_status_after",
            "repair_status_before",
            "repair_status_after",
        ],
        "worker.graph_invariant_violation" => {
            &["category", "code", "phase", "resumable", "node_id"]
        }
        "worker.lifecycle_invariant_check_started"
        | "worker.lifecycle_invariant_check_passed"
        | "worker.lifecycle_invariant_check_failed"
        | "worker.lifecycle_invariant_not_applicable" => &[
            "invariant_id",
            "scope",
            "phase",
            "required_evidence_kinds",
            "available_evidence_kinds",
            "current_node",
            "graph_revision",
            "repository_fingerprint",
        ],
        "worker.implementation_barrier_created"
        | "worker.implementation_barrier_satisfied"
        | "worker.implementation_barrier_unsatisfied" => &[
            "required_nodes",
            "completed_nodes",
            "unresolved_nodes",
            "repository_fingerprint",
            "graph_revision",
        ],
        "worker.next_implementation_node_selected" => &[
            "completed_node",
            "next_node",
            "next_node_kind",
            "graph_revision",
            "repository_fingerprint",
        ],
        _ => return Ok(()),
    };
    let missing = required
        .iter()
        .filter(|field| !object.contains_key(**field))
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("{event_type} lacks fields: {}", missing.join(", ")))
    }
}

fn target_context_already_prepared(
    events: &[crate::execution_graph::ExecutionDomainEvent],
    identity: &TargetContextIdentity<'_>,
) -> bool {
    events.iter().rev().any(|event| {
        matches!(
            event,
            crate::execution_graph::ExecutionDomainEvent::TargetContextPrepared {
                node_id: prepared_node_id,
                target_path: prepared_target_path,
                operation: prepared_operation,
                source_path: prepared_source_path,
                repository_fingerprint: prepared_repository_fingerprint,
                target_content_hash: prepared_target_content_hash,
                accepted_intent_hash: prepared_intent_hash,
                ..
            } if prepared_node_id == identity.node_id
                && prepared_target_path == identity.target_path
                && prepared_operation == identity.operation
                && prepared_source_path.as_deref() == identity.source_path
                && prepared_repository_fingerprint.as_str() == identity.repository_fingerprint
                && prepared_target_content_hash == identity.target_content_hash
                && prepared_intent_hash == identity.accepted_intent_hash
        )
    })
}

pub(in crate::hosted) fn bind_validation_repair_model_call(
    event: &mut crate::execution_graph::ExecutionDomainEvent,
    active_model_call_id: Option<&str>,
) {
    if let crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
        attempt: Some(attempt),
        ..
    } = event
        && attempt.model_call_id.is_none()
    {
        attempt.model_call_id = active_model_call_id.map(str::to_owned);
    }
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
        lease_renewed_at: &'a Arc<Mutex<Option<String>>>,
        containment: &'a command::HostedProcessContainment,
        partial_run: Option<PartialRunContext>,
    ) -> Result<Self> {
        crate::execution_graph::validate_lifecycle_invariant_definitions(
            &crate::execution_graph::lifecycle_invariant_definitions(),
        )
        .map_err(|violation| {
            anyhow!(HostedInvariantFailure::in_phase(
                violation.code,
                "startup",
                violation.to_string(),
            ))
        })?;
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
            lease_renewed_at,
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
            active_model_call_id: None,
            phase_persistence_failure: None,
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
                let identical_decisions = self
                    .notebook
                    .orchestration
                    .semantic_cycle_history
                    .last()
                    .map_or(0, |observation| observation.repeated_count);
                let cycle_guardrail_activations = self
                    .notebook
                    .orchestration
                    .semantic_cycle_history
                    .iter()
                    .filter(|observation| {
                        observation.repeated_count
                            >= crate::execution_graph::MAX_IDENTICAL_DETERMINISTIC_CYCLES
                    })
                    .count();
                object.insert("orchestration_convergence".into(), json!({
                    "identical_decisions_per_execution": identical_decisions,
                    "identical_target_probes_per_node": if self.notebook.orchestration.semantic_cycle_history.last().is_some_and(|observation| observation.outcome == "prepare_target_context") { identical_decisions } else { 0 },
                    "graph_revisions_without_semantic_progress": identical_decisions.saturating_sub(1),
                    "last_semantic_progress_at": self.notebook.orchestration.worker_liveness.last_semantic_progress_at,
                    "lease_renewed_at": self.notebook.orchestration.worker_liveness.lease_renewed_at,
                    "cycle_guardrail_activations": cycle_guardrail_activations,
                    "alert_active": identical_decisions > 3,
                }));
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
        if let Err(error) = validate_critical_worker_event_fields(&data) {
            eprintln!(
                "[warning] {operation} was not emitted because its telemetry contract is incomplete: {error}"
            );
            return false;
        }
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
        if let Some(HostedStopReason::LeaseLost(detail)) = stop_reason.as_ref() {
            let _ = self.containment.drain();
            return Err(anyhow!(HostedLeaseLost {
                operation: "heartbeat",
                detail: detail.clone(),
            }));
        }
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
            initiator: crate::execution_graph::CancellationInitiator::User,
            reason_code: "user_or_lease_owner_requested".into(),
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
        let validation_repair_failure_id = self.current_decision.as_ref().and_then(|decision| {
            match decision {
                ExecutionDecision::ExecuteTarget {
                    action:
                        crate::hosted_orchestrator::MutationAction::RepairTarget {
                            failure,
                            ..
                        },
                    ..
                } if failure.category
                    == crate::execution_graph::FailureCategory::ValidationFailure =>
                {
                    Some(failure.id.clone())
                }
                ExecutionDecision::RepairTarget { context, .. }
                    if context.failure.category
                        == crate::execution_graph::FailureCategory::ValidationFailure =>
                {
                    Some(context.failure.id.clone())
                }
                _ => None,
            }
        });
        let graph_node_id = self
            .current_decision
            .as_ref()
            .and_then(ExecutionDecision::budget_node_id)
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
        let (node_id, mut node_budget) =
            if let Some(failure_id) = validation_repair_failure_id.as_ref() {
                self.notebook
                    .orchestration
                    .budget
                    .repair_budget_owner(failure_id)?
            } else {
                let node_budget = self
                    .notebook
                    .orchestration
                    .graph
                    .as_ref()
                    .and_then(|graph| graph.node(&graph_node_id))
                    .map(|node| node.budget.clone())?;
                (graph_node_id, node_budget)
            };
        let estimate = estimate_model_call_request_cost(request);
        let estimated_cost_micros = estimate.estimated_request_cost;
        let mut admission = self
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
        if !admission.admitted
            && admission.rejection_reason == Some("node_model_call_budget_exhausted")
            && let Some(failure_id) = validation_repair_failure_id.as_ref()
        {
            let mission_calls_remaining = self
                .notebook
                .orchestration
                .budget
                .mission
                .max_model_calls
                .saturating_sub(
                    self.notebook
                        .orchestration
                        .budget
                        .total_model_calls
                        .saturating_add(
                            self.notebook
                                .orchestration
                                .budget
                                .total_model_calls_reserved,
                        ),
                );
            if mission_calls_remaining > 0 {
                let reallocated = self
                    .notebook
                    .orchestration
                    .budget
                    .reallocate_validation_repair_capacity(failure_id, 1, 0)
                    .ok();
                if let Some(reallocated) = reallocated {
                    let repaired_budget = reallocated.budget;
                    if let Some(graph) = self.notebook.orchestration.graph.as_mut()
                        && let Some(node) =
                            graph.node_mut(&crate::execution_graph::ExecutionNodeId::new(
                                reallocated.session_id.clone(),
                            ))
                    {
                        node.budget = repaired_budget.as_node_budget();
                        graph.revision = graph.revision.saturating_add(1);
                        self.notebook.orchestration.graph_revision = graph.revision;
                    }
                    node_budget = repaired_budget.as_node_budget();
                    admission = self
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
                    self.append_event_recoverable(
                        "validation",
                        json!({
                            "event_type": "worker.validation_repair_budget_reallocated",
                            "repair_session_id": reallocated.session_id,
                            "originating_validation_gate": self.notebook.orchestration.budget
                                .repair_session_for_failure(failure_id)
                                .map(|session| session.originating_gate_id.as_str()),
                            "failure_revision": self.notebook.orchestration.budget
                                .repair_session_for_failure(failure_id)
                                .map(|session| session.current_assertion_set_revision),
                            "source": "remaining_mission_model_calls",
                            "reallocated_model_calls": reallocated.model_calls,
                            "reallocated_cost_micros": reallocated.cost_micros,
                            "mission_model_calls_remaining": mission_calls_remaining.saturating_sub(reallocated.model_calls),
                            "local_model_calls_remaining": repaired_budget.max_model_calls.saturating_sub(
                                admission.consumed_calls.saturating_add(admission.reserved_calls)
                            ),
                        }),
                        "validation repair budget reallocation",
                    );
                }
            }
        }
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
        if reservation.is_some()
            && self.current_model_call_purpose()
                == Some(crate::execution_graph::ModelCallPurpose::ValidationRepairMutation)
            && let Some(failure_id) = validation_repair_failure_id.as_ref()
        {
            let target_id = self
                .notebook
                .orchestration
                .budget
                .repair_session_for_failure(failure_id)
                .and_then(|session| session.repair_nodes.last())
                .and_then(|repair_node_id| {
                    self.notebook
                        .orchestration
                        .graph
                        .as_ref()
                        .and_then(|graph| graph.node(repair_node_id))
                })
                .and_then(|node| node.target.as_ref())
                .map(|target| target.mutation_target_id().to_string())
                .unwrap_or_default();
            let repair_reservation = self
                .notebook
                .orchestration
                .budget
                .reserve_validation_repair_attempt(failure_id, target_id);
            match repair_reservation {
                Ok(repair_reservation) => {
                    let attempts_consumed_before = self
                        .notebook
                        .orchestration
                        .budget
                        .usage_for(&node_id)
                        .validation_repair_attempts;
                    self.append_event_recoverable(
                        "repair",
                        json!({
                            "event_type": "worker.repair_attempt_reserved",
                            "repair_session_id": repair_reservation.repair_session_id,
                            "repair_attempt_id": repair_reservation.attempt_id,
                            "target_id": repair_reservation.target_id,
                            "attempts_consumed_before": attempts_consumed_before,
                            "attempts_consumed_after": attempts_consumed_before,
                        }),
                        "validation repair provider attempt reserved",
                    );
                }
                Err(_) => {
                    if let Some(reservation) = reservation.as_ref() {
                        self.release_graph_model_call_reservation(reservation);
                    }
                    return None;
                }
            }
        }
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

    pub(in crate::hosted) fn release_graph_model_call_reservation(
        &mut self,
        reservation: &crate::execution_graph::ModelCallReservation,
    ) {
        let repair_reservation = self
            .notebook
            .orchestration
            .budget
            .validation_repair_sessions
            .get(reservation.node_id.as_str())
            .and_then(|session| {
                session.attempt_reservations.iter().find(|attempt| {
                    attempt.state == crate::execution_graph::RepairAttemptReservationState::Reserved
                })
            })
            .cloned();
        let attempts_consumed = self
            .notebook
            .orchestration
            .budget
            .usage_for(&reservation.node_id)
            .validation_repair_attempts;
        self.notebook
            .orchestration
            .budget
            .release_model_call_reservation(reservation);
        if let Some(repair_reservation) = repair_reservation {
            self.append_event_recoverable(
                "repair",
                json!({
                    "event_type": "worker.repair_attempt_released",
                    "repair_session_id": repair_reservation.repair_session_id,
                    "repair_attempt_id": repair_reservation.attempt_id,
                    "target_id": repair_reservation.target_id,
                    "attempts_consumed_before": attempts_consumed,
                    "attempts_consumed_after": attempts_consumed,
                }),
                "validation repair provider attempt released",
            );
        }
    }

    pub(in crate::hosted) fn effective_phase_model_call_limit(&self) -> usize {
        let phase = self.phases.active();
        let legacy_limit = self.phases.phase_limit(phase);
        if phase != ExecutionPhase::Repair {
            return legacy_limit;
        }
        let failure_id = self.current_decision.as_ref().and_then(|decision| match decision {
            ExecutionDecision::ExecuteTarget {
                action:
                    crate::hosted_orchestrator::MutationAction::RepairTarget { failure, .. },
                ..
            } if failure.category
                == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                Some(&failure.id)
            }
            ExecutionDecision::RepairTarget { context, .. }
                if context.failure.category
                    == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                Some(&context.failure.id)
            }
            _ => None,
        });
        let Some(failure_id) = failure_id else {
            return legacy_limit;
        };
        let Some((owner, budget)) = self
            .notebook
            .orchestration
            .budget
            .repair_budget_owner(failure_id)
        else {
            return legacy_limit;
        };
        let remaining = self
            .notebook
            .orchestration
            .budget
            .remaining_for(&owner, &budget)
            .model_calls_remaining;
        legacy_limit.max(
            self.phases
                .phase_calls(phase)
                .saturating_add(usize::try_from(remaining).unwrap_or(usize::MAX)),
        )
    }

    pub(in crate::hosted) fn record_validation_repair_admission_rejection(
        &mut self,
        reason: &str,
    ) -> Result<bool> {
        let repair = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                action: crate::hosted_orchestrator::MutationAction::RepairTarget { failure, .. },
                target,
                ..
            }) if failure.category
                == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                target.validation_repair.as_ref().map(|context| {
                    (
                        failure.id.clone(),
                        failure.node_id.clone(),
                        context.repair_intent.clone(),
                        context.selected_target.clone(),
                        context.repository_fingerprint.clone(),
                    )
                })
            }
            Some(ExecutionDecision::RepairTarget { context, .. })
                if context.failure.category
                    == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                context.target.validation_repair.as_ref().map(|repair| {
                    (
                        context.failure.id.clone(),
                        context.failure.node_id.clone(),
                        repair.repair_intent.clone(),
                        repair.selected_target.clone(),
                        repair.repository_fingerprint.clone(),
                    )
                })
            }
            _ => None,
        };
        let Some((failure_id, validation_node_id, repair_intent, selected_target, fingerprint)) =
            repair
        else {
            return Ok(false);
        };
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
                sequence: self.next_domain_event_sequence(),
                validation_node_id: validation_node_id.clone(),
                failure_id: failure_id.clone(),
                result: crate::execution_graph::RepairResult::NoMutation {
                    diagnosis: Some(repair_intent.diagnosis),
                    reason: reason.to_owned(),
                    outcome:
                        crate::execution_graph::ValidationRepairMutationOutcome::AdmissionRejected,
                    unresolved: Some(crate::execution_graph::UnresolvedValidationRepair {
                        validation_id: validation_node_id.to_string(),
                        repair_intent_id: repair_intent.repair_intent_id.clone(),
                        selected_target: selected_target.clone(),
                        diagnosis: repair_intent.diagnosis,
                        outcome: crate::execution_graph::ValidationRepairMutationOutcome::AdmissionRejected,
                        reason: reason.to_owned(),
                        attempted_targets: vec![selected_target.clone()],
                    }),
                },
                attempt: Some(crate::execution_graph::ValidationRepairAttempt {
                    repair_intent_id: repair_intent.repair_intent_id,
                    target_path: selected_target.clone(),
                    diagnosis: repair_intent.diagnosis,
                    requested_tool_policy:
                        crate::execution_graph::MutationFallbackPolicy::NoSafeFallback,
                    outcome:
                        crate::execution_graph::ValidationRepairMutationOutcome::AdmissionRejected,
                    repository_fingerprint_before: fingerprint.clone().into(),
                    repository_fingerprint_after: fingerprint.into(),
                    admission_rejection_reason: Some(reason.to_owned()),
                    ..Default::default()
                }),
            },
        )?;
        self.append_event_recoverable(
            "validation",
            json!({
                "event_type": "worker.validation_repair_attempt_recorded",
                "repair_session_id": crate::execution_graph::BudgetState::repair_session_id(&failure_id),
                "originating_validation_gate": validation_node_id,
                "target": selected_target,
                "attempt_outcome": "admission_rejected",
                "reason_code": reason,
            }),
            "validation repair admission rejection",
        );
        Ok(true)
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
        let purpose = self.current_model_call_purpose();
        self.notebook
            .orchestration
            .budget
            .consume_model_call_reservation(reservation, call_cost_micros, duration);
        if let Some(purpose) = purpose {
            self.notebook
                .orchestration
                .budget
                .record_model_call_purpose(purpose);
            if purpose == crate::execution_graph::ModelCallPurpose::ValidationRepairMutation {
                let attempts_after = self
                    .notebook
                    .orchestration
                    .budget
                    .usage_for(&reservation.node_id)
                    .validation_repair_attempts;
                self.append_event_recoverable(
                    "repair",
                    json!({
                        "event_type": "worker.repair_attempt_consumed",
                        "repair_session_id": reservation.node_id.as_str(),
                        "attempts_consumed_before": attempts_after.saturating_sub(1),
                        "attempts_consumed_after": attempts_after,
                    }),
                    "validation repair provider attempt consumed",
                );
            }
        }
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

    fn current_model_call_purpose(&self) -> Option<crate::execution_graph::ModelCallPurpose> {
        if self
            .current_decision
            .as_ref()
            .and_then(ExecutionDecision::node_id)
            .is_some_and(|node_id| {
                crate::execution_graph::mutation_repair_allowance_is_restored(
                    &self.notebook.orchestration.domain_events,
                    node_id,
                )
            })
        {
            return None;
        }
        match self.current_decision.as_ref()? {
            ExecutionDecision::ExecuteTarget {
                action: crate::hosted_orchestrator::MutationAction::MutateTarget { .. },
                ..
            } => Some(crate::execution_graph::ModelCallPurpose::InitialTargetMutation),
            ExecutionDecision::ExecuteTarget {
                action: crate::hosted_orchestrator::MutationAction::RepairTarget { failure, .. },
                ..
            } if failure.category == crate::execution_graph::FailureCategory::ValidationFailure => {
                let diagnosed_without_mutation = self
                    .notebook
                    .orchestration
                    .domain_events
                    .iter()
                    .rev()
                    .any(|event| {
                        matches!(
                            event,
                            crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
                                failure_id,
                                result: crate::execution_graph::RepairResult::NoMutation { .. },
                                ..
                            } if failure_id == &failure.id
                        )
                    });
                Some(if diagnosed_without_mutation {
                    crate::execution_graph::ModelCallPurpose::ValidationDiagnosis
                } else {
                    crate::execution_graph::ModelCallPurpose::ValidationRepairMutation
                })
            }
            ExecutionDecision::RepairTarget {
                context: crate::hosted_orchestrator::TargetRepairContext { failure, .. },
                ..
            } if failure.category == crate::execution_graph::FailureCategory::ValidationFailure => {
                let diagnosed_without_mutation = self
                    .notebook
                    .orchestration
                    .domain_events
                    .iter()
                    .rev()
                    .any(|event| {
                        matches!(
                            event,
                            crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
                                failure_id,
                                result: crate::execution_graph::RepairResult::NoMutation { .. },
                                ..
                            } if failure_id == &failure.id
                        )
                    });
                Some(if diagnosed_without_mutation {
                    crate::execution_graph::ModelCallPurpose::ValidationDiagnosis
                } else {
                    crate::execution_graph::ModelCallPurpose::ValidationRepairMutation
                })
            }
            ExecutionDecision::ExecuteTarget {
                action: crate::hosted_orchestrator::MutationAction::RepairTarget { .. },
                ..
            }
            | ExecutionDecision::RepairTarget { .. } => {
                Some(crate::execution_graph::ModelCallPurpose::TargetMutationRepair)
            }
            _ => None,
        }
    }

    pub(in crate::hosted) fn restore_mutation_repair_allowance(
        &mut self,
        node_id: &crate::execution_graph::ExecutionNodeId,
    ) -> Result<()> {
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::MutationRepairAllowanceRestored {
                sequence: self.next_domain_event_sequence(),
                node_id: node_id.clone(),
            },
        )?;
        Ok(())
    }

    pub(in crate::hosted) fn consume_pending_mutation_repair_allowance(
        &mut self,
        node_id: &crate::execution_graph::ExecutionNodeId,
    ) -> Result<()> {
        if crate::execution_graph::mutation_repair_allowance_is_restored(
            &self.notebook.orchestration.domain_events,
            node_id,
        ) {
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::MutationRepairAllowanceConsumed {
                    sequence: self.next_domain_event_sequence(),
                    node_id: node_id.clone(),
                },
            )?;
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

        let purpose = self.current_model_call_purpose();
        self.notebook
            .orchestration
            .budget
            .consume_model_call_reservation(reservation, call_cost_micros, duration);
        if let Some(purpose) = purpose {
            self.notebook
                .orchestration
                .budget
                .record_model_call_purpose(purpose);
            if purpose == crate::execution_graph::ModelCallPurpose::ValidationRepairMutation {
                let attempts_after = self
                    .notebook
                    .orchestration
                    .budget
                    .usage_for(&reservation.node_id)
                    .validation_repair_attempts;
                self.append_event_recoverable(
                    "repair",
                    json!({
                        "event_type": "worker.repair_attempt_consumed",
                        "repair_session_id": reservation.node_id.as_str(),
                        "attempts_consumed_before": attempts_after.saturating_sub(1),
                        "attempts_consumed_after": attempts_after,
                        "provider_call_failed": true,
                    }),
                    "failed validation repair provider attempt consumed",
                );
            }
        }
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
            ExecutionDecision::ExecuteTarget { target, .. }
                if target.validation_repair.is_some() =>
            {
                Some(ExecutionPhase::Repair)
            }
            ExecutionDecision::ExecuteTarget { .. } => Some(ExecutionPhase::Implementation),
            ExecutionDecision::RepairTarget { .. } => Some(ExecutionPhase::Repair),
            ExecutionDecision::RunValidation { .. } => Some(ExecutionPhase::Validation),
            ExecutionDecision::ReviewDiff { .. }
            | ExecutionDecision::ReviewIncompleteDiff { .. } => Some(ExecutionPhase::DiffReview),
            ExecutionDecision::EvaluateCompletion { .. } => {
                Some(ExecutionPhase::CompletionEvaluation)
            }
            ExecutionDecision::Publish { .. } => Some(ExecutionPhase::Publication),
            ExecutionDecision::Finish { .. } | ExecutionDecision::StopForGuardrail { .. } => None,
        };
        let previous = self.phases.active();
        if let Some(next) = phase
            && next != previous
            && (!(previous.stage().can_transition_to(next.stage())
                || matches!(
                    (previous, next),
                    (ExecutionPhase::Repair, ExecutionPhase::DiffReview)
                ))
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
        if decision_event_added {
            match &decision {
                ExecutionDecision::ExecuteTarget {
                    action:
                        crate::hosted_orchestrator::MutationAction::RepairTarget {
                            target: repair_target,
                            failure,
                            ..
                        },
                    target,
                    ..
                } if failure.category
                    == crate::execution_graph::FailureCategory::ValidationFailure =>
                {
                    let repair = target.validation_repair.as_ref();
                    if let Some(session) = self
                        .notebook
                        .orchestration
                        .budget
                        .repair_session_for_failure(&failure.id)
                        .cloned()
                    {
                        let owner = crate::execution_graph::ExecutionNodeId::new(
                            session.session_id.clone(),
                        );
                        let usage = self.notebook.orchestration.budget.usage_for(&owner);
                        self.append_event_recoverable(
                            "validation",
                            json!({
                                "event_type": "worker.validation_repair_session_created",
                                "repair_session_id": session.session_id,
                                "originating_validation_gate": session.originating_gate_id,
                                "failed_validation_id": session.failed_validation_id,
                                "failure_revision": session.current_assertion_set_revision,
                                "repository_fingerprint": target.repository_fingerprint,
                            }),
                            "validation repair session",
                        );
                        self.append_event_recoverable(
                            "validation",
                            json!({
                                "event_type": "worker.validation_repair_budget_resolved",
                                "repair_session_id": session.session_id,
                                "budget": session.budget,
                                "local_model_calls_remaining": session.budget.max_model_calls.saturating_sub(
                                    usage.model_calls_consumed.saturating_add(usage.model_calls_reserved)
                                ),
                                "mission_model_calls_remaining": self.notebook.orchestration.budget.mission.max_model_calls.saturating_sub(
                                    self.notebook.orchestration.budget.total_model_calls.saturating_add(
                                        self.notebook.orchestration.budget.total_model_calls_reserved
                                    )
                                ),
                            }),
                            "validation repair budget",
                        );
                    }
                    self.append_event_recoverable(
                        "validation",
                        json!({
                            "event_type": "worker.repair_evidence_built",
                            "validation_gate": failure.node_id,
                            "selected_repair_target": target.target.path,
                            "target_content_hash": target.target_content_hash,
                            "repository_fingerprint": target.repository_fingerprint,
                            "implicated_paths": repair.map(|context| context.implicated_targets.iter().map(|excerpt| excerpt.path.as_str()).collect::<Vec<_>>()).unwrap_or_default(),
                            "existing_diff_paths": repair.map(|context| context.existing_diff_paths.as_slice()).unwrap_or_default(),
                        }),
                        "validation repair evidence construction",
                    );
                    self.append_event_recoverable(
                        "validation",
                        json!({
                            "event_type": "worker.repair_target_ranked",
                            "validation_gate": failure.node_id,
                            "selected_repair_target": target.target.path,
                            "target_role": target.target.role,
                            "selection_basis": "structured_assertion_and_source_contract",
                            "diff_fingerprint": target.repository_fingerprint,
                        }),
                        "validation repair target ranking",
                    );
                    self.append_event_recoverable(
                        "validation",
                        json!({
                            "event_type": "worker.repair_context_validated",
                            "validation_gate": failure.node_id,
                            "selected_repair_target": target.target.path,
                            "has_current_file_content": target.current_file_content.is_some(),
                            "has_target_content_hash": target.target_content_hash.is_some(),
                            "repository_fingerprint": target.repository_fingerprint,
                        }),
                        "validation repair context validation",
                    );
                    self.append_event_recoverable(
                        "validation",
                        json!({
                            "event_type": "worker.validation_repair_target_selected",
                            "validation_gate": failure.node_id,
                            "failing_tests": failure.assertion_failures.iter()
                                .map(|assertion| assertion.test_name.as_str())
                                .collect::<Vec<_>>(),
                            "implicated_paths": failure.assertion_failures.iter()
                                .flat_map(|assertion| assertion.implicated_paths.iter())
                                .collect::<BTreeSet<_>>(),
                            "selected_repair_target": repair_target.path,
                            "diff_fingerprint": self.notebook.repository_fingerprint,
                        }),
                        "validation repair target selection",
                    );
                }
                ExecutionDecision::RunValidation { node_id, gate } => {
                    let sessions = self
                        .notebook
                        .orchestration
                        .budget
                        .validation_repair_sessions
                        .values()
                        .filter(|session| {
                            &session.originating_gate_id == node_id
                                && session.status
                                    == crate::execution_graph::ValidationRepairSessionStatus::ReadyForRerun
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    for session in sessions {
                        self.append_event_recoverable(
                            "validation",
                            json!({
                                "event_type": "worker.validation_rerun_scheduled",
                                "repair_session_id": session.session_id,
                                "originating_validation_gate": node_id,
                                "failure_revision": session.current_assertion_set_revision,
                                "command": gate.command,
                                "repository_fingerprint": self.notebook.repository_fingerprint,
                                "model_calls_consumed": 0,
                            }),
                            "validation rerun scheduling",
                        );
                    }
                }
                ExecutionDecision::Publish {
                    mode:
                        crate::execution_graph::PublicationMode::Draft
                        | crate::execution_graph::PublicationMode::DraftRecovery,
                } => {
                    self.append_event_recoverable(
                        "publication",
                        json!({
                            "event_type": "worker.draft_publication_started",
                            "publication_mode": decision,
                            "publication_outcome": "partial_reviewable",
                            "override_reason": self.notebook.orchestration.graph.as_ref()
                                .and_then(|graph| graph.dependency_overrides.first())
                                .map(|override_| override_.reason.as_str()),
                            "diff_fingerprint": self.notebook.repository_fingerprint,
                        }),
                        "draft publication start",
                    );
                }
                _ => {}
            }
        }
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
                    "transition_payload_version": 1,
                    "from_phase": previous,
                    "phase": phase,
                    "decision": execution_decision_name(&decision),
                    "from_state": canonical_running_state(previous),
                    "to_state": canonical_running_state(phase),
                    "reason_code": "phase_reconciled",
                    "reason": reason,
                    "source": match phase {
                        ExecutionPhase::Validation => "quality_gate",
                        ExecutionPhase::Publication => "publication",
                        _ => "orchestrator",
                    },
                    "graph_revision": self.notebook.orchestration.graph_revision,
                    "notebook_revision": self.notebook.revision,
                    "source_tree_hash": self.notebook.repository_fingerprint,
                    "occurred_at": now_rfc3339(),
                    "budget": self.budget_telemetry(),
                    "notebook": self.notebook,
                    "checkpoint": self.notebook_checkpoint_metadata(
                        self.notebook.impact_map_artifact.artifact_sha256.as_deref()
                    ),
                });
                let preflight = preflight_phase_transition(
                    &event,
                    previous,
                    phase,
                    self.notebook.orchestration.graph_revision,
                    self.notebook.revision,
                );
                if !preflight.passed() {
                    self.append_event_recoverable(
                        "progress",
                        json!({
                            "event_type": "worker.phase_transition_contract_rejected",
                            "category": "OrchestrationContractFailure",
                            "code": "phase_transition_event_invalid",
                            "from_phase": previous,
                            "phase": phase,
                            "preflight": preflight,
                        }),
                        "phase-transition contract rejection",
                    );
                    bail!(
                        "phase_transition_event_invalid: local phase-transition preflight rejected `{}` to `{}`",
                        previous.as_str(),
                        phase.as_str()
                    );
                }
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.phase_transition_preflight_passed",
                        "from_phase": previous,
                        "phase": phase,
                        "decision": execution_decision_name(&decision),
                        "graph_revision": self.notebook.orchestration.graph_revision,
                        "notebook_revision": self.notebook.revision,
                        "transition_payload_version": 1,
                    }),
                    "phase-transition preflight",
                );
                let persistence_result = self.api.append_event("progress", event);
                let contract_rejected = persistence_result.as_ref().err().is_some_and(|error| {
                    error
                        .downcast_ref::<HostedHttpError>()
                        .is_some_and(|http| http.status == reqwest::StatusCode::BAD_REQUEST)
                });
                let persistence_error =
                    persistence_result
                        .err()
                        .map(|error| PhasePersistenceFailure {
                            kind: if contract_rejected {
                                PhasePersistenceFailureKind::Contract
                            } else {
                                PhasePersistenceFailureKind::Persistence
                            },
                            from_phase: previous,
                            phase,
                            safe_error: truncate_text(&format!("{error:#}"), 2_000),
                        });
                if let Some(error) = persistence_error.as_ref() {
                    self.phase_persistence_failure = Some(error.clone());
                    self.notebook.phase_persistence_failure_code = Some(error.code().to_owned());
                    eprintln!("[warning] phase transition could not be persisted: {error}");
                    self.append_event_recoverable(
                        "progress",
                        json!({
                            "event_type": "worker.phase_persistence_failed",
                            "category": error.category(),
                            "code": error.code(),
                            "from_phase": previous,
                            "phase": phase,
                            "process_health": error.process_health(),
                            "recoverable": !contract_rejected,
                            "action": if contract_rejected { "safe_terminal_result" } else { "retry_or_continue" },
                            "safe_error": error.safe_error,
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
                node_id,
                action: crate::hosted_orchestrator::MutationAction::PrepareTargetContext { .. },
                target,
            } if target.validation_repair.is_some() => {
                let repair = target.validation_repair.clone().unwrap_or_default();
                let failure_id = crate::execution_graph::FailureId::new(
                    repair.repair_intent.failed_validation_id.clone(),
                );
                let failure = self
                    .notebook
                    .orchestration
                    .failures
                    .get(&failure_id)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow!(HostedInvariantFailure::in_phase(
                            "validation_repair_failure_missing",
                            "validation_repair",
                            format!(
                                "repair node `{node_id}` has no originating validation failure"
                            ),
                        ))
                    })?;
                Some(ExecutionDomainEvent::ValidationRepairStarted {
                    sequence,
                    validation_node_id: failure.node_id,
                    failure_id,
                    repair_node_id: repair.repair_node_id,
                    originating_implementation_node_id: repair.originating_implementation_node_id,
                    target_ref: repair.target_ref,
                    failure_revision: repair.failure_revision,
                    repair_intent: repair.repair_intent,
                    selected_target: target.target.path.clone(),
                    implicated_paths: failure
                        .assertion_failures
                        .iter()
                        .flat_map(|assertion| assertion.implicated_paths.iter().cloned())
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                    correction_contracts: repair.correction_contracts,
                    requested_tool_policy:
                        crate::execution_graph::MutationFallbackPolicy::NoSafeFallback,
                    repository_fingerprint_before:
                        crate::execution_graph::RepositoryFingerprint::new(
                            repair.repository_fingerprint,
                        ),
                })
            }
            ExecutionDecision::ExecuteTarget {
                node_id,
                action: crate::hosted_orchestrator::MutationAction::PrepareTargetContext { .. },
                ..
            } => node_started(node_id, self),
            ExecutionDecision::ExecuteTarget {
                action:
                    crate::hosted_orchestrator::MutationAction::RepairTarget {
                        target,
                        failure,
                        fallback_policy,
                        ..
                    },
                target: context,
                ..
            } if failure.category == crate::execution_graph::FailureCategory::ValidationFailure => {
                let repair = context.validation_repair.clone().unwrap_or_default();
                let already_started = self
                    .notebook
                    .orchestration
                    .graph
                    .as_ref()
                    .and_then(|graph| graph.node(&repair.repair_node_id))
                    .is_some_and(|node| {
                        node.kind == ExecutionNodeKind::ValidationRepair
                            && node.status == crate::execution_graph::ExecutionNodeStatus::Running
                    });
                (!already_started).then_some(ExecutionDomainEvent::ValidationRepairStarted {
                    sequence,
                    validation_node_id: failure.node_id.clone(),
                    failure_id: failure.id.clone(),
                    repair_node_id: repair.repair_node_id,
                    originating_implementation_node_id: repair.originating_implementation_node_id,
                    target_ref: repair.target_ref,
                    failure_revision: repair.failure_revision,
                    repair_intent: repair.repair_intent,
                    selected_target: target.path.clone(),
                    implicated_paths: failure
                        .assertion_failures
                        .iter()
                        .flat_map(|assertion| assertion.implicated_paths.iter().cloned())
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                    correction_contracts: repair.correction_contracts,
                    requested_tool_policy: *fallback_policy,
                    repository_fingerprint_before:
                        crate::execution_graph::RepositoryFingerprint::new(
                            repair.repository_fingerprint,
                        ),
                })
            }
            ExecutionDecision::ExecuteTarget { node_id, .. }
            | ExecutionDecision::RepairTarget { node_id, .. }
            | ExecutionDecision::ReviewDiff { node_id }
            | ExecutionDecision::EvaluateCompletion { node_id } => node_started(node_id, self),
            ExecutionDecision::ReviewIncompleteDiff { node_id, reason } => {
                let snapshot = self.build_execution_snapshot()?;
                Some(ExecutionDomainEvent::IncompleteDiffReviewRequested {
                    sequence,
                    node_id: node_id.clone(),
                    reason: *reason,
                    dependency_overrides: snapshot
                        .incomplete_diff_dependency_overrides(node_id, *reason),
                })
            }
            ExecutionDecision::RunValidation { node_id, gate } => {
                let barrier = self
                    .notebook
                    .orchestration
                    .graph
                    .as_ref()
                    .ok_or_else(|| anyhow!("validation requires an authoritative execution graph"))?
                    .validation_readiness(crate::execution_graph::RepositoryFingerprint::new(
                        repository_fingerprint.clone(),
                    ));
                self.append_event_recoverable(
                    "validation",
                    json!({
                        "event_type": "worker.implementation_barrier_checked",
                        "validation_node_id": node_id,
                        "implementation_barrier_result": barrier.is_satisfied(),
                        "required_implementation_nodes": barrier.required_implementation_nodes,
                        "completed_implementation_nodes": barrier.completed_implementation_nodes,
                        "unresolved_nodes": barrier.unresolved_nodes,
                        "repository_fingerprint": barrier.repository_fingerprint,
                        "graph_revision": barrier.graph_revision,
                    }),
                    "implementation barrier check",
                );
                if !barrier.is_satisfied() {
                    self.append_event_recoverable(
                        "validation",
                        json!({
                            "event_type": "worker.implementation_barrier_rejected_validation",
                            "validation_node_id": node_id,
                            "category": "OrchestrationStateInvariantFailure",
                            "code": "implementation_barrier_unsatisfied",
                            "phase": "validation",
                            "resumable": true,
                            "implementation_barrier_result": false,
                            "required_implementation_nodes": barrier.required_implementation_nodes,
                            "completed_implementation_nodes": barrier.completed_implementation_nodes,
                            "unresolved_nodes": barrier.unresolved_nodes,
                            "repository_fingerprint": barrier.repository_fingerprint,
                            "graph_revision": barrier.graph_revision,
                        }),
                        "implementation barrier rejected validation",
                    );
                    return Err(anyhow!(HostedInvariantFailure::new(
                        "implementation_barrier_unsatisfied",
                        format!(
                            "required implementation nodes remain unresolved: {:?}",
                            barrier.unresolved_nodes
                        ),
                    )));
                }
                let repaired_failures = self
                    .notebook
                    .orchestration
                    .budget
                    .validation_repair_sessions
                    .values()
                    .filter(|session| {
                        &session.originating_gate_id == node_id
                            && session.status
                                == crate::execution_graph::ValidationRepairSessionStatus::ReadyForRerun
                    })
                    .map(|session| crate::execution_graph::FailureId::new(
                        session.failed_validation_id.clone(),
                    ))
                    .filter(|failure_id| {
                        self.notebook
                            .orchestration
                            .failures
                            .get(failure_id)
                            .is_some_and(crate::execution_graph::FailureRecord::is_unresolved)
                    })
                    .collect::<Vec<_>>();
                for failure_id in repaired_failures {
                    self.append_execution_domain_event(ExecutionDomainEvent::FailureRecovered {
                        sequence: self.next_domain_event_sequence(),
                        node_id: node_id.clone(),
                        failure_id,
                        repository_fingerprint: repository_fingerprint.clone(),
                    })?;
                }
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
                    sequence: self.next_domain_event_sequence(),
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
        let started_validation_repair = event.as_ref().and_then(|event| match event {
            ExecutionDomainEvent::ValidationRepairStarted {
                validation_node_id,
                failure_id,
                repair_node_id,
                originating_implementation_node_id,
                target_ref,
                failure_revision,
                ..
            } if !repair_node_id.as_str().is_empty() => Some((
                validation_node_id.clone(),
                failure_id.clone(),
                repair_node_id.clone(),
                originating_implementation_node_id.clone(),
                target_ref.clone(),
                *failure_revision,
            )),
            _ => None,
        });
        let repair_activation_was_present =
            started_validation_repair
                .as_ref()
                .is_some_and(|(_, _, repair_node_id, _, _, _)| {
                    self.notebook
                        .orchestration
                        .graph
                        .as_ref()
                        .is_some_and(|graph| graph.node(repair_node_id).is_some())
                });
        if let Some(event) = event {
            self.append_execution_domain_event(event)?;
        }
        if let Some((
            validation_node_id,
            failure_id,
            repair_node_id,
            originating_implementation_node_id,
            target_ref,
            failure_revision,
        )) = started_validation_repair
        {
            let common = json!({
                "repair_node_id": repair_node_id,
                "originating_implementation_node_id": originating_implementation_node_id,
                "target_id": target_ref.target_id,
                "target_path": target_ref.path,
                "validation_node_id": validation_node_id,
                "failure_id": failure_id,
                "failure_revision": failure_revision,
                "implementation_status_before": "completed",
                "implementation_status_after": "completed",
                "repair_status_before": "ready",
                "repair_status_after": "running",
            });
            let event_type = if repair_activation_was_present {
                "worker.repair_node_activation_idempotent"
            } else {
                "worker.validation_repair_node_activated"
            };
            let mut data = common;
            data["event_type"] = Value::String(event_type.into());
            data["node_kind"] = Value::String("validation_repair".into());
            data["capabilities"] = json!(["repository_read", "repository_mutation", "repair"]);
            self.append_event_recoverable(
                "validation",
                data,
                if repair_activation_was_present {
                    "validation repair node activation replayed idempotently"
                } else {
                    "validation repair node activated atomically"
                },
            );
            self.append_event_recoverable(
                "validation",
                json!({
                    "event_type": "worker.node_capabilities_resolved",
                    "node_id": repair_node_id,
                    "node_kind": "validation_repair",
                    "capabilities": ["repository_read", "repository_mutation", "repair"],
                    "repair_session_id": crate::execution_graph::BudgetState::repair_session_id(&failure_id),
                    "target_id": target_ref.target_id,
                }),
                "validation repair node capabilities resolved",
            );
        }
        if let ExecutionDecision::ExecuteTarget {
            action: crate::hosted_orchestrator::MutationAction::RepairTarget { failure, .. },
            target,
            ..
        } = decision
            && failure.category == crate::execution_graph::FailureCategory::ValidationFailure
            && let Some(repair) = target.validation_repair.as_ref()
        {
            let session = self
                .notebook
                .orchestration
                .budget
                .repair_session_for_failure(&failure.id)
                .cloned();
            let repair_budget_owner = session.as_ref().map(|session| {
                crate::execution_graph::ExecutionNodeId::new(session.session_id.clone())
            });
            let repair_budget = session.as_ref().and_then(|session| {
                repair_budget_owner.as_ref().map(|owner| {
                    self.notebook.orchestration.budget.repair_budget_for(
                        crate::execution_graph::RepairIntentKind::ValidationRepair,
                        owner,
                        session.budget.max_target_attempts,
                    )
                })
            });
            self.append_event_recoverable(
                "validation",
                json!({
                    "event_type": "worker.repair_intent_session_created",
                    "repair_intent_kind": crate::execution_graph::RepairIntentKind::ValidationRepair,
                    "repair_session_id": session.as_ref().map(|session| session.session_id.as_str()),
                    "repair_budget_owner": repair_budget_owner,
                    "repair_budget": repair_budget,
                    "target_node_id": target.node_id,
                    "failed_validation_id": repair.repair_intent.failed_validation_id,
                    "failure_revision": session.as_ref().map(|session| session.current_assertion_set_revision),
                    "repository_fingerprint": repair.repository_fingerprint,
                }),
                "validation repair session created",
            );
            self.append_event_recoverable(
                "validation",
                json!({
                    "event_type": "worker.repair_budget_scope_resolved",
                    "repair_intent_kind": crate::execution_graph::RepairIntentKind::ValidationRepair,
                    "repair_session_id": session.as_ref().map(|session| session.session_id.as_str()),
                    "repair_budget_owner": repair_budget_owner,
                    "repair_budget": repair_budget,
                    "target_node_id": target.node_id,
                    "max_target_attempts": session.as_ref().map(|session| session.budget.max_target_attempts),
                    "mutation_fallback_attempts_consumed": self.notebook.orchestration.budget.usage_for(&target.node_id).mutation_fallback_attempts,
                    "validation_repair_attempts_consumed": session.as_ref().map(|session| {
                        let owner = crate::execution_graph::ExecutionNodeId::new(session.session_id.clone());
                        self.notebook.orchestration.budget.usage_for(&owner).validation_repair_attempts
                    }),
                }),
                "repair budget scope resolved",
            );
            self.append_event_recoverable(
                "validation",
                json!({
                    "event_type": "worker.validation_repair_target_bound",
                    "repair_session_id": session.as_ref().map(|session| session.session_id.as_str()),
                    "repair_intent_kind": crate::execution_graph::RepairIntentKind::ValidationRepair,
                    "repair_budget_owner": repair_budget_owner,
                    "repair_budget": repair_budget,
                    "repair_intent_id": repair.repair_intent.repair_intent_id,
                    "target_node_id": target.node_id,
                    "selected_target": repair.selected_target,
                    "failure_revision": session.as_ref().map(|session| session.current_assertion_set_revision),
                    "repository_fingerprint": repair.repository_fingerprint,
                }),
                "validation repair target bound",
            );
            self.append_event_recoverable(
                "validation",
                json!({
                    "event_type": "worker.validation_repair_intent_created",
                    "failed_validation_id": repair.repair_intent.failed_validation_id,
                    "repair_intent_id": repair.repair_intent.repair_intent_id,
                    "selected_target": repair.repair_intent.target,
                    "assertion_ids": repair.repair_intent.expected_correction.required_assertion_ids,
                    "remaining_eligible_targets": repair.remaining_eligible_targets,
                }),
                "validation repair intent created",
            );
            self.append_event_recoverable(
                "validation",
                json!({
                    "event_type": "worker.validation_repair_contract_built",
                    "failed_validation_id": repair.repair_intent.failed_validation_id,
                    "repair_intent_id": repair.repair_intent.repair_intent_id,
                    "assertion_ids": repair.correction_contracts.iter()
                        .map(|contract| contract.assertion_id.as_str())
                        .collect::<Vec<_>>(),
                    "contracts": repair.correction_contracts,
                }),
                "validation repair contract built",
            );
            if !repair.attempted_targets.is_empty() {
                let selects_next = repair
                    .remaining_eligible_targets
                    .contains(&repair.selected_target);
                self.append_event_recoverable(
                    "validation",
                    json!({
                        "event_type": if selects_next {
                            "worker.validation_repair_next_target_selected"
                        } else {
                            "worker.validation_repair_active_target_confirmed"
                        },
                        "failed_validation_id": repair.repair_intent.failed_validation_id,
                        "repair_intent_id": repair.repair_intent.repair_intent_id,
                        "selected_target": repair.selected_target,
                        "previous_targets": repair.attempted_targets,
                        "remaining_eligible_targets": repair.remaining_eligible_targets,
                    }),
                    if selects_next {
                        "validation repair next target selected"
                    } else {
                        "validation repair active target confirmed"
                    },
                );
            }
        }
        if matches!(
            decision,
            ExecutionDecision::ReviewIncompleteDiff {
                reason: crate::execution_graph::IncompleteReason::ValidationRepairProducedNoMeaningfulMutation,
                ..
            }
        ) {
            let snapshot = self.build_execution_snapshot()?;
            let unresolved = snapshot
                .failures
                .unresolved()
                .find(|failure| {
                    failure.category
                        == crate::execution_graph::FailureCategory::ValidationFailure
                })
                .cloned();
            let attempts = snapshot
                .events
                .iter()
                .filter_map(|event| match event {
                    crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
                        attempt: Some(attempt),
                        ..
                    } => Some(attempt.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            self.append_event_recoverable(
                "validation",
                json!({
                    "event_type": "worker.validation_repair_target_exhausted",
                    "failed_validation_id": unresolved.as_ref().map(|failure| failure.id.to_string()),
                    "assertion_ids": unresolved.as_ref().into_iter()
                        .flat_map(|failure| failure.assertion_failures.iter())
                        .enumerate()
                        .map(|(index, assertion)| format!("{}:{}:{}:{}", assertion.test_file, assertion.source_line.unwrap_or_default(), assertion.test_name, index))
                        .collect::<Vec<_>>(),
                    "attempted_targets": attempts.iter().map(|attempt| attempt.target_path.as_str()).collect::<Vec<_>>(),
                    "repair_intent_id": attempts.last().map(|attempt| attempt.repair_intent_id.as_str()),
                    "remaining_eligible_targets": [],
                    "final_repair_decision": "review_incomplete_diff",
                    "diff_fingerprint": self.notebook.repository_fingerprint,
                }),
                "validation repair targets exhausted",
            );
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
            self.effective_phase_model_call_limit()
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
                "Repair only current_target from its exact current content. The rejected mutation was not applied. Follow mutation_repair.fallback_policy as an executable tool constraint and do not repeat the rejected strategy.".into()
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
            &self.notebook.validation_evidence,
            &snapshot.current_repository.fingerprint,
            crate::execution_graph::InvariantScope::RepositoryOperationReduction,
        )
        .map_err(|(code, error)| {
            anyhow!(HostedInvariantFailure::in_phase(
                code,
                self.phases.active().as_str(),
                error,
            ))
        })?;
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
        self.notebook.orchestration.worker_liveness.lease_renewed_at = self
            .lease_renewed_at
            .lock()
            .expect("hosted lease renewal timestamp lock poisoned")
            .clone();
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
        if let Some(stale_node_id) = self
            .current_decision
            .as_ref()
            .and_then(ExecutionDecision::node_id)
            .filter(|node_id| {
                snapshot
                    .graph
                    .node(node_id)
                    .is_some_and(|node| node.status.is_terminal())
            })
            .cloned()
        {
            self.current_decision = None;
            self.append_event_recoverable("progress", json!({
                "event_type": "worker.active_node_pointer_reconciled",
                "node_id": stale_node_id,
                "graph_revision_before": snapshot.graph.revision,
                "graph_revision_after": snapshot.graph.revision,
                "selected_next_node": snapshot.graph.next_runnable_node().map(|node| node.id.as_str()),
            }), "stale active node pointer reconciled");
        }
        let mut decision = reconcile_execution(&snapshot).map_err(anyhow::Error::new)?;
        if let ExecutionDecision::ExecuteTarget {
            action:
                crate::hosted_orchestrator::MutationAction::RepairTarget {
                    failure,
                    fallback_policy,
                    ..
                },
            target,
            ..
        } = &mut decision
            && failure.category == crate::execution_graph::FailureCategory::MutationConflict
            && let Some(failure_category) = failure
                .code
                .as_deref()
                .and_then(MutationApplicationFailure::from_code)
        {
            *fallback_policy = crate::hosted_orchestrator::refine_fallback_for_replacement_threshold(
                *fallback_policy,
                &target.target.effective_operation(),
                failure_category,
                target,
                self.manifest
                    .execution_policy
                    .mutation_replacement_max_bytes
                    .unwrap_or(
                        crate::hosted_orchestrator::DEFAULT_MUTATION_REPLACEMENT_THRESHOLD_BYTES,
                    )
                    .min(MAX_MODEL_FILE_BYTES),
            );
        }
        let decision_key = execution_decision_idempotency_key(&snapshot, &decision);
        if !orchestration_decision_is_new(
            self.notebook.last_orchestration_decision_key.as_deref(),
            &decision_key,
        ) {
            let observed_at = now_rfc3339();
            let semantic_state_hash = decision_key
                .rsplit(':')
                .next()
                .unwrap_or_default()
                .to_owned();
            let semantic_decision_hash = sha256_text(&decision_key);
            let repeated_count = crate::execution_graph::observe_semantic_cycle(
                &mut self.notebook.orchestration.semantic_cycle_history,
                &semantic_state_hash,
                &semantic_decision_hash,
                execution_decision_name(&decision),
                &observed_at,
            );
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.semantic_decision_deduplicated",
                    "node_id": decision.node_id(),
                    "semantic_state_hash": semantic_state_hash,
                    "semantic_decision_hash": semantic_decision_hash,
                    "graph_revision_before": snapshot.graph.revision,
                    "graph_revision_after": snapshot.graph.revision,
                    "repeated_cycle_count": repeated_count,
                }),
                "semantic orchestration decision deduplicated",
            );
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.graph_revision_no_change",
                    "node_id": decision.node_id(),
                    "semantic_state_hash": semantic_state_hash,
                    "semantic_decision_hash": semantic_decision_hash,
                    "graph_revision": snapshot.graph.revision,
                    "repeated_cycle_count": repeated_count,
                }),
                "graph revision unchanged",
            );
            let cycle_node_id = decision.node_id().cloned();
            let cycle_on_repository_node = cycle_node_id.as_ref().is_some_and(|node_id| {
                snapshot
                    .graph
                    .node(node_id)
                    .is_some_and(|node| node.kind.is_mutation())
            });
            let cycle_cause = cycle_node_id.as_ref().map_or(
                crate::execution_graph::CycleCause::Unknown,
                |node_id| {
                    let last_repository_event = crate::execution_graph::current_execution_epoch(
                        &snapshot.events,
                    )
                    .iter()
                    .rev()
                    .find(|event| event.node_id() == Some(node_id));
                    match last_repository_event {
                        Some(
                            crate::execution_graph::ExecutionDomainEvent::TargetOperationAlreadyApplied { .. },
                        ) => crate::execution_graph::CycleCause::AlreadyAppliedNotReduced,
                        Some(
                            crate::execution_graph::ExecutionDomainEvent::TargetMutationProduced { .. },
                        ) => crate::execution_graph::CycleCause::SuccessfulMutationNotReduced,
                        _ if snapshot.graph.node(node_id).is_some_and(|node| {
                            node.status != crate::execution_graph::ExecutionNodeStatus::Running
                        }) => crate::execution_graph::CycleCause::StaleActivePointer,
                        _ => crate::execution_graph::CycleCause::DecisionSelectorNoProgress,
                    }
                },
            );
            if repeated_count == 1
                && cycle_on_repository_node
                && matches!(
                    cycle_cause,
                    crate::execution_graph::CycleCause::SuccessfulMutationNotReduced
                        | crate::execution_graph::CycleCause::AlreadyAppliedNotReduced
                )
            {
                let cycle_node = cycle_node_id
                    .as_ref()
                    .and_then(|node_id| snapshot.graph.node(node_id));
                let cycle_operation = cycle_node
                    .and_then(|node| node.target.as_ref())
                    .map(|target| target.effective_operation());
                let cycle_attempt = cycle_node
                    .and_then(|node| node.attempts.last())
                    .map(|attempt| attempt.attempt);
                let cycle_repository_fingerprint_before = cycle_node
                    .and_then(|node| node.attempts.last())
                    .map(|attempt| attempt.repository_fingerprint_before.clone());
                let cycle_node_status_before = cycle_node.map(|node| node.status);
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.successful_mutation_reconciliation_started",
                        "node_id": cycle_node_id,
                        "operation": cycle_operation,
                        "attempt_id": cycle_attempt,
                        "repository_fingerprint_before": cycle_repository_fingerprint_before,
                        "repository_fingerprint_after": snapshot.current_repository.fingerprint,
                        "verification_evidence_id": null,
                        "node_status_before": cycle_node_status_before,
                        "repair_intent_kind": null,
                        "repair_budget_owner": null,
                        "guardrail_action": crate::execution_graph::OrchestrationGuardrailAction::ReconcileSuccessfulMutation,
                        "cycle_cause": cycle_cause,
                        "graph_revision_before": snapshot.graph.revision,
                    }),
                    "successful mutation cycle reconciliation",
                );
                self.verify_active_target_state()?;
                let reconciled = self.build_execution_snapshot()?;
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.successful_mutation_reconciliation_completed",
                        "node_id": cycle_node_id,
                        "operation": cycle_operation,
                        "attempt_id": cycle_attempt,
                        "repository_fingerprint_before": cycle_repository_fingerprint_before,
                        "repository_fingerprint_after": reconciled.current_repository.fingerprint,
                        "verification_evidence_id": cycle_node_id.as_ref().and_then(|node_id| {
                            reconciled.graph.node(node_id).and_then(|node| {
                                node.operation_evidence.last().map(|evidence| evidence.semantic_id.as_str())
                            })
                        }),
                        "node_status_before": cycle_node_status_before,
                        "node_status_after": cycle_node_id.as_ref().and_then(|node_id| reconciled.graph.node(node_id)).map(|node| node.status),
                        "repair_intent_kind": null,
                        "repair_budget_owner": null,
                        "graph_revision_before": snapshot.graph.revision,
                        "graph_revision_after": reconciled.graph.revision,
                        "node_status": cycle_node_id.as_ref().and_then(|node_id| reconciled.graph.node(node_id)).map(|node| node.status),
                    }),
                    "successful mutation cycle reconciliation",
                );
                return self.reconcile_execution_and_apply();
            }
            if repeated_count >= crate::execution_graph::MAX_IDENTICAL_DETERMINISTIC_CYCLES {
                let incomplete_implementation = snapshot.graph.nodes().any(|node| {
                    node.required
                        && node.kind.is_mutation()
                        && !matches!(
                            node.status,
                            crate::execution_graph::ExecutionNodeStatus::Completed
                                | crate::execution_graph::ExecutionNodeStatus::Skipped
                        )
                });
                if cycle_on_repository_node && incomplete_implementation {
                    self.append_event_recoverable(
                        "progress",
                        json!({
                            "event_type": "worker.graph_invariant_violation",
                            "node_id": cycle_node_id,
                            "category": "OrchestrationStateInvariantFailure",
                            "code": "successful_mutation_not_reduced",
                            "phase": "implementation",
                            "resumable": true,
                            "invariant": "cycle_recovery_cannot_skip_required_implementation",
                            "cycle_cause": crate::execution_graph::CycleCause::PersistenceMismatch,
                            "guardrail_action": crate::execution_graph::OrchestrationGuardrailAction::FailOrchestrator,
                            "graph_revision": snapshot.graph.revision,
                        }),
                        "unreconciled repository-operation cycle",
                    );
                    return Err(anyhow!(HostedInvariantFailure::new(
                        "successful_mutation_not_reduced",
                        "a deterministic repository-operation cycle remained after local reconciliation while required implementation nodes were unfinished",
                    )));
                }
                let outcome = if snapshot.current_repository.has_changes() {
                    OrchestratedMissionOutcome::PartialReviewable
                } else {
                    OrchestratedMissionOutcome::BlockedNoDiff
                };
                self.notebook.orchestration.cycle_cancellation_request =
                    Some(crate::execution_graph::CancellationRequest {
                        initiator: crate::execution_graph::CancellationInitiator::CycleGuardrail,
                        reason_code: "deterministic_orchestration_cycle".into(),
                        requested_at: observed_at,
                    });
                self.append_event_recoverable("progress", json!({
                    "event_type": "worker.orchestration_cycle_detected",
                    "node_id": decision.node_id(),
                    "semantic_state_hash": semantic_state_hash,
                    "semantic_decision_hash": semantic_decision_hash,
                    "graph_revision_before": snapshot.graph.revision,
                    "graph_revision_after": snapshot.graph.revision,
                    "repeated_cycle_count": repeated_count,
                    "guardrail_outcome": if snapshot.current_repository.has_changes() {
                        crate::execution_graph::OrchestrationGuardrailOutcome::ReviewIncompleteDiff
                    } else { crate::execution_graph::OrchestrationGuardrailOutcome::FinishBlocked },
                    "cancellation_initiator": "cycle_guardrail",
                    "reason_code": "deterministic_orchestration_cycle",
                }), "deterministic orchestration cycle detected");
                return self.apply_execution_decision(ExecutionDecision::StopForGuardrail {
                    outcome,
                    reason: crate::execution_graph::GuardrailReason::NoProgress,
                });
            }
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
        let validation_changed = self.notebook.orchestration.domain_events
            [snapshot.events.len().min(self.notebook.orchestration.domain_events.len())..]
            .iter()
            .any(|event| {
                matches!(
                    event,
                    crate::execution_graph::ExecutionDomainEvent::ValidationStarted { .. }
                        | crate::execution_graph::ExecutionDomainEvent::ValidationPassed { .. }
                        | crate::execution_graph::ExecutionDomainEvent::ValidationFailed { .. }
                        | crate::execution_graph::ExecutionDomainEvent::ValidationRepairStarted { .. }
                        | crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted { .. }
                        | crate::execution_graph::ExecutionDomainEvent::ValidationSuperseded { .. }
                )
            });
        let cycle_result = crate::execution_graph::OrchestrationCycleResult {
            graph_changed: self.notebook.orchestration.graph_revision != snapshot.graph.revision,
            repository_changed: self.notebook.repository_fingerprint
                != snapshot.current_repository.fingerprint,
            validation_changed,
            phase_changed: matches!(result.phase_decision, PhaseDecision::Transition(_)),
            external_wait_scheduled: false,
            terminal_selected: matches!(
                result.decision,
                ExecutionDecision::Finish { .. } | ExecutionDecision::StopForGuardrail { .. }
            ),
        };
        if cycle_result.made_semantic_progress() {
            self.notebook
                .orchestration
                .worker_liveness
                .last_semantic_progress_at = Some(now_rfc3339());
        }
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
        mut event: crate::execution_graph::ExecutionDomainEvent,
    ) -> Result<()> {
        let validation_repair_admission = matches!(
            event,
            crate::execution_graph::ExecutionDomainEvent::ValidationRepairStarted { .. }
        );
        bind_validation_repair_model_call(&mut event, self.active_model_call_id.as_deref());
        let mut snapshot = self.build_execution_snapshot()?;
        let mutation_owner = event.node_id().cloned();
        let mutation_context = event.mutation_context(&snapshot.graph).map_err(|error| {
            anyhow!(mutation_owner.as_ref().map_or_else(
                || HostedInvariantFailure::in_phase(
                    "mutation_capability_contract_mismatch",
                    "repair",
                    error.to_string(),
                ),
                |node_id| HostedInvariantFailure::for_node_in_phase(
                    "mutation_capability_contract_mismatch",
                    "repair",
                    node_id.to_string(),
                    error.to_string(),
                )
            ))
        })?;
        let mutation_event_family = event.event_type();
        snapshot.append_event(event).map_err(|error| {
            if error.code == "completed_implementation_node_reopened" {
                anyhow!(HostedInvariantFailure::in_phase(
                    "completed_implementation_node_reopened",
                    "validation_repair",
                    error.to_string(),
                ))
            } else if error.code == "mutation_capability_contract_mismatch" {
                anyhow!(mutation_owner.as_ref().map_or_else(
                    || HostedInvariantFailure::in_phase(
                        "mutation_capability_contract_mismatch",
                        "repair",
                        error.to_string(),
                    ),
                    |node_id| HostedInvariantFailure::for_node_in_phase(
                        "mutation_capability_contract_mismatch",
                        "repair",
                        node_id.to_string(),
                        error.to_string(),
                    )
                ))
            } else if validation_repair_admission {
                anyhow!(HostedRepairAccountingFailure::incompatible_scope(
                    error.to_string()
                ))
            } else {
                anyhow!("could not apply hosted execution event: {error}")
            }
        })?;
        let mut orchestration = std::mem::take(&mut self.notebook.orchestration);
        orchestration.replace_from_snapshot(&snapshot);
        orchestration.materialize_legacy_notebook(&mut self.notebook);
        self.notebook.orchestration = orchestration;
        if let Some(context) = mutation_context {
            let producer = self
                .notebook
                .orchestration
                .graph
                .as_ref()
                .and_then(|graph| graph.node(&context.node_id));
            let node_kind = producer.map(|node| node.kind);
            let capabilities = producer.map(|node| node.kind.capabilities());
            let common = json!({
                "node_id": context.node_id,
                "node_kind": node_kind,
                "capabilities": capabilities,
                "intent_kind": context.intent_kind,
                "target_id": context.target_id,
                "target_path": context.target_path,
                "repository_fingerprint": context.repository_fingerprint,
                "event_family": mutation_event_family,
            });
            for event_type in [
                "worker.repository_mutation_capability_checked",
                "worker.mutation_event_owner_resolved",
            ] {
                let mut data = common.clone();
                data["event_type"] = Value::String(event_type.into());
                self.append_event_recoverable(
                    "repair",
                    data,
                    "repository mutation producer capability resolved",
                );
            }
        }
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
        self.record_active_target_failure_with_code(category, None, detail)
    }

    pub(in crate::hosted) fn record_active_target_failure_with_code(
        &mut self,
        category: crate::execution_graph::FailureCategory,
        code: Option<&str>,
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
                .mutation_fallback_attempts
                .saturating_add(1),
            fingerprint,
            detail,
        );
        failure.target_path = target_path;
        failure.code = code.map(str::to_owned);
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
                node_id: node_id.clone(),
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
        let evidence_paths = mutation_target_paths
            .iter()
            .cloned()
            .chain(
                failures
                    .iter()
                    .flat_map(|failure| structured_validation_paths(&failure.output)),
            )
            .collect::<BTreeSet<_>>();
        let target_contents = evidence_paths
            .iter()
            .map(|path| {
                let content = safe_repo_path(&self.repo.root, path, false)
                    .ok()
                    .and_then(|absolute| fs::read_to_string(absolute).ok())
                    .or_else(|| {
                        self.notebook
                            .orchestration
                            .evidence
                            .reusable_file(path, &fingerprint, None)
                            .map(|evidence| evidence.captured_content.clone())
                    })
                    .unwrap_or_default();
                (path.clone(), content)
            })
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
        for (path, content) in &target_contents {
            if content.is_empty() {
                continue;
            }
            let evidence = crate::execution_graph::FileEvidence::capture(
                path,
                &fingerprint,
                None,
                content.clone(),
                false,
            );
            if !self
                .notebook
                .orchestration
                .evidence
                .files
                .contains_key(&evidence.evidence_id)
            {
                self.append_execution_domain_event(
                    crate::execution_graph::ExecutionDomainEvent::RepositoryEvidenceRecorded {
                        sequence: self.next_domain_event_sequence(),
                        evidence_id: evidence.evidence_id.clone(),
                        repository_fingerprint: fingerprint.clone(),
                        evidence: Some(evidence),
                    },
                )?;
            }
        }
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
                self.notebook
                    .orchestration
                    .budget
                    .record_validation_diagnosis_call(node_id.clone());
                record.validation_command = Some(failure.command.clone());
                record.assertion_failures = parse_validation_assertion_failures(
                    &failure.command,
                    &failure.output,
                    &target_contents,
                );
                if record.assertion_failures.is_empty()
                    && looks_like_structured_test_failure(&failure.output)
                {
                    self.append_event_recoverable(
                        "validation",
                        json!({
                            "event_type": "worker.validation_output_parse_incomplete",
                            "validation_gate": failure.id,
                            "parser": "vitest",
                            "raw_excerpt": truncate_text(&failure.output, 2_000),
                            "fallback_attempted": true,
                            "diff_fingerprint": fingerprint,
                        }),
                        "incomplete structured validation parsing",
                    );
                    if let Some(fallback) = fallback_validation_assertion_failure(
                        &failure.command,
                        &failure.output,
                        &target_contents,
                    ) {
                        record.assertion_failures.push(fallback);
                    }
                }
                record.target_path = validation_repair_target_hint(
                    &record.assertion_failures,
                    &mutation_target_paths,
                    &target_contents,
                )
                .or_else(|| validation_failure_target_hint(&mutation_target_paths, &diagnostics));
                if let Some(assertion) = record
                    .assertion_failures
                    .iter()
                    .max_by_key(|assertion| assertion_specificity(assertion))
                {
                    self.append_event_recoverable(
                        "validation",
                        json!({
                            "event_type": "worker.transition_contract_compared",
                            "validation_gate": failure.id,
                            "test_file": assertion.test_file,
                            "suite_path": assertion.suite_path,
                            "test_name": assertion.test_name,
                            "expected_transition": assertion.expected,
                            "observed_transition": assertion.received,
                            "selected_repair_target": record.target_path,
                            "implicated_paths": assertion.implicated_paths,
                            "diff_fingerprint": fingerprint,
                        }),
                        "validation transition contract comparison",
                    );
                }
                self.append_event_recoverable(
                    "validation",
                    json!({
                        "event_type": "worker.validation_output_parsed",
                        "validation_gate": failure.id,
                        "parser": "vitest",
                        "assertion_count": record.assertion_failures.len(),
                        "failing_tests": record.assertion_failures,
                        "implicated_paths": record.assertion_failures.iter()
                            .flat_map(|assertion| assertion.implicated_paths.iter())
                            .collect::<BTreeSet<_>>(),
                        "selected_repair_target": record.target_path,
                        "diff_fingerprint": fingerprint,
                    }),
                    "structured validation failure parsing",
                );
            }
            let implicated_targets = record
                .assertion_failures
                .iter()
                .flat_map(|assertion| assertion.implicated_paths.iter().cloned())
                .collect::<BTreeSet<_>>();
            let selected_target = record.target_path.clone();
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
                    node_id: node_id.clone(),
                    failure_id: failure_id.clone(),
                    fingerprint: validation_fingerprint,
                },
            )?;
            if let Some(revision) = self
                .notebook
                .orchestration
                .budget
                .current_validation_failure_revision(node_id.as_str(), &fingerprint)
                .cloned()
            {
                let prior_assertions = self
                    .notebook
                    .orchestration
                    .budget
                    .validation_failure_revisions
                    .get(node_id.as_str())
                    .and_then(|revisions| revisions.iter().rev().nth(1))
                    .map(|revision| {
                        revision
                            .assertion_ids
                            .iter()
                            .cloned()
                            .collect::<BTreeSet<_>>()
                    })
                    .unwrap_or_default();
                let current_assertions = revision
                    .assertion_ids
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if revision.revision > 1 {
                    let session = self
                        .notebook
                        .orchestration
                        .budget
                        .validation_repair_sessions
                        .values()
                        .find(|session| session.originating_gate_id == node_id)
                        .cloned();
                    self.append_event_recoverable(
                        "validation",
                        json!({
                            "event_type": "worker.validation_assertion_set_recomputed",
                            "repair_session_id": session.as_ref().map(|session| session.session_id.as_str()),
                            "originating_validation_gate": node_id,
                            "failure_revision": revision.revision,
                            "repository_fingerprint": revision.repository_fingerprint,
                            "assertion_ids": revision.assertion_ids,
                            "added_assertion_ids": current_assertions.difference(&prior_assertions).collect::<Vec<_>>(),
                            "removed_assertion_ids": prior_assertions.difference(&current_assertions).collect::<Vec<_>>(),
                            "implicated_targets": implicated_targets,
                            "target": selected_target,
                            "local_model_calls_remaining": session.as_ref().map(|session| {
                                let owner = crate::execution_graph::ExecutionNodeId::new(session.session_id.clone());
                                let usage = self.notebook.orchestration.budget.usage_for(&owner);
                                session.budget.max_model_calls.saturating_sub(
                                    usage.model_calls_consumed.saturating_add(usage.model_calls_reserved)
                                )
                            }),
                            "mission_model_calls_remaining": self.notebook.orchestration.budget.mission.max_model_calls.saturating_sub(
                                self.notebook.orchestration.budget.total_model_calls.saturating_add(
                                    self.notebook.orchestration.budget.total_model_calls_reserved
                                )
                            ),
                            "model_calls_consumed": 0,
                        }),
                        "validation assertion-set recomputation",
                    );
                }
                self.append_event_recoverable(
                    "validation",
                    json!({
                        "event_type": "worker.validation_failure_revision_created",
                        "validation_id": revision.validation_id,
                        "failure_id": failure_id,
                        "failure_revision": revision.revision,
                        "repository_fingerprint": revision.repository_fingerprint,
                        "assertion_ids": revision.assertion_ids,
                    }),
                    "validation failure revision",
                );
            }
        }
        Ok(())
    }

    pub(in crate::hosted) fn record_validation_repair_no_mutation(
        &mut self,
        failures: &[ValidationResult],
        reason: &str,
    ) -> Result<()> {
        let gate_ids = failures
            .iter()
            .map(|failure| failure.id.as_str())
            .collect::<BTreeSet<_>>();
        let snapshot = self.build_execution_snapshot()?;
        let unresolved = snapshot
            .failures
            .unresolved()
            .filter(|failure| {
                failure.category == crate::execution_graph::FailureCategory::ValidationFailure
                    && snapshot.graph.node(&failure.node_id).is_some_and(|node| {
                        node.validation
                            .as_ref()
                            .is_some_and(|gate| gate_ids.contains(gate.gate_id.as_str()))
                    })
            })
            .map(|failure| (failure.node_id.clone(), failure.id.clone()))
            .collect::<Vec<_>>();
        for (validation_node_id, failure_id) in unresolved {
            let latest_started = self
                .notebook
                .orchestration
                .domain_events
                .iter()
                .rev()
                .find_map(|event| match event {
                    crate::execution_graph::ExecutionDomainEvent::ValidationRepairStarted {
                        sequence,
                        failure_id: existing_failure_id,
                        repair_intent,
                        selected_target,
                        requested_tool_policy,
                        repository_fingerprint_before,
                        ..
                    } if existing_failure_id == &failure_id => Some((
                        *sequence,
                        repair_intent.clone(),
                        selected_target.clone(),
                        *requested_tool_policy,
                        repository_fingerprint_before.clone(),
                    )),
                    _ => None,
                });
            let Some((
                started_sequence,
                repair_intent,
                selected_target,
                requested_tool_policy,
                repository_fingerprint_before,
            )) = latest_started
            else {
                continue;
            };
            let completed_after_start = self.notebook.orchestration.domain_events.iter().rev().any(
                |event| {
                    matches!(
                        event,
                        crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
                            sequence,
                            failure_id: existing_failure_id,
                            ..
                        } if existing_failure_id == &failure_id && *sequence > started_sequence
                    )
                },
            );
            if completed_after_start {
                continue;
            }
            let mut attempted_targets =
                crate::hosted_orchestrator::attempted_validation_repair_targets(
                    &snapshot,
                    &failure_id,
                )
                .into_iter()
                .collect::<Vec<_>>();
            if !attempted_targets.contains(&selected_target) {
                attempted_targets.push(selected_target.clone());
            }
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
                    sequence: self.next_domain_event_sequence(),
                    validation_node_id: validation_node_id.clone(),
                    failure_id: failure_id.clone(),
                    result: crate::execution_graph::RepairResult::NoMutation {
                        diagnosis: Some(repair_intent.diagnosis),
                        reason: reason.to_owned(),
                        outcome: crate::execution_graph::ValidationRepairMutationOutcome::NoChangeAgainstCurrentTarget,
                        unresolved: Some(crate::execution_graph::UnresolvedValidationRepair {
                            validation_id: failure_id.to_string(),
                            repair_intent_id: repair_intent.repair_intent_id.clone(),
                            selected_target: selected_target.clone(),
                            diagnosis: repair_intent.diagnosis,
                            outcome: crate::execution_graph::ValidationRepairMutationOutcome::NoChangeAgainstCurrentTarget,
                            reason: reason.to_owned(),
                            attempted_targets: attempted_targets.clone(),
                        }),
                    },
                    attempt: Some(crate::execution_graph::ValidationRepairAttempt {
                        repair_intent_id: repair_intent.repair_intent_id.clone(),
                        target_path: selected_target.clone(),
                        diagnosis: repair_intent.diagnosis,
                        requested_tool_policy,
                        outcome: crate::execution_graph::ValidationRepairMutationOutcome::NoChangeAgainstCurrentTarget,
                        repository_fingerprint_before,
                        repository_fingerprint_after: snapshot.current_repository.fingerprint.clone().into(),
                        ..Default::default()
                    }),
                },
            )?;
            self.append_event_recoverable(
                "validation",
                json!({
                    "event_type": "worker.validation_repair_no_change_detected",
                    "validation_gate": validation_node_id,
                    "failure_id": failure_id,
                    "repair_intent_id": repair_intent.repair_intent_id,
                    "selected_repair_target": selected_target,
                    "attempted_targets": attempted_targets,
                    "unresolved": true,
                    "reason": reason,
                    "diff_fingerprint": self.notebook.repository_fingerprint,
                }),
                "validation repair no change",
            );
            self.append_event_recoverable(
                "validation",
                json!({
                    "event_type": "worker.validation_repair_unresolved",
                    "validation_gate": validation_node_id,
                    "failure_id": failure_id,
                    "repair_intent_id": repair_intent.repair_intent_id,
                    "selected_repair_target": selected_target,
                    "outcome": "no_change_against_current_target",
                }),
                "validation repair unresolved",
            );
        }
        Ok(())
    }

    pub(in crate::hosted) fn record_validation_no_valid_repair(
        &mut self,
        diagnosis: crate::execution_graph::ValidationRepairDiagnosis,
        reason: &str,
    ) -> Result<()> {
        let (
            validation_node_id,
            failure_id,
            repair_intent,
            selected_target,
            implicated_paths,
            requested_tool_policy,
            repository_fingerprint_before,
        ) = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                action:
                    crate::hosted_orchestrator::MutationAction::RepairTarget {
                        target,
                        failure,
                        fallback_policy,
                        ..
                    },
                target: context,
                ..
            }) if failure.category
                == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                (
                    failure.node_id.clone(),
                    failure.id.clone(),
                    context
                        .validation_repair
                        .as_ref()
                        .map(|repair| repair.repair_intent.clone())
                        .unwrap_or_default(),
                    target.path.clone(),
                    failure
                        .assertion_failures
                        .iter()
                        .flat_map(|assertion| assertion.implicated_paths.iter().cloned())
                        .collect::<BTreeSet<_>>(),
                    *fallback_policy,
                    context
                        .validation_repair
                        .as_ref()
                        .map(|repair| {
                            crate::execution_graph::RepositoryFingerprint::new(
                                repair.repository_fingerprint.clone(),
                            )
                        })
                        .unwrap_or_default(),
                )
            }
            _ => bail!("no-valid-repair requires an active validation repair decision"),
        };
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
                sequence: self.next_domain_event_sequence(),
                validation_node_id: validation_node_id.clone(),
                failure_id: failure_id.clone(),
                result: crate::execution_graph::RepairResult::NoMutation {
                    diagnosis: Some(diagnosis),
                    reason: reason.to_owned(),
                    outcome: crate::execution_graph::ValidationRepairMutationOutcome::NoValidRepair,
                    unresolved: Some(crate::execution_graph::UnresolvedValidationRepair {
                        validation_id: failure_id.to_string(),
                        repair_intent_id: repair_intent.repair_intent_id.clone(),
                        selected_target: selected_target.clone(),
                        diagnosis,
                        outcome:
                            crate::execution_graph::ValidationRepairMutationOutcome::NoValidRepair,
                        reason: reason.to_owned(),
                        attempted_targets: vec![selected_target.clone()],
                    }),
                },
                attempt: Some(crate::execution_graph::ValidationRepairAttempt {
                    repair_intent_id: repair_intent.repair_intent_id.clone(),
                    target_path: selected_target.clone(),
                    diagnosis,
                    requested_tool_policy,
                    outcome: crate::execution_graph::ValidationRepairMutationOutcome::NoValidRepair,
                    repository_fingerprint_before,
                    repository_fingerprint_after: self
                        .notebook
                        .repository_fingerprint
                        .clone()
                        .into(),
                    ..Default::default()
                }),
            },
        )?;
        self.append_event_recoverable(
            "validation",
            json!({
                "event_type": "worker.validation_repair_diagnosed",
                "validation_gate": validation_node_id,
                "failure_id": failure_id,
                "selected_repair_target": selected_target,
                "implicated_paths": implicated_paths,
                "repair_diagnosis": diagnosis,
                "result": "no_mutation",
                "reason": reason,
                "diff_fingerprint": self.notebook.repository_fingerprint,
            }),
            "validation repair diagnosis",
        );
        self.append_event_recoverable(
            "validation",
            json!({
                "event_type": "worker.validation_repair_unresolved",
                "failed_validation_id": failure_id,
                "repair_intent_id": repair_intent.repair_intent_id,
                "assertion_ids": repair_intent.expected_correction.required_assertion_ids,
                "selected_target": selected_target,
                "attempted_targets": [selected_target],
                "current_content_hash": repo_file_sha256(&self.repo.root, &selected_target),
                "proposed_content_hash": Value::Null,
                "no_op_reason": reason,
                "remaining_eligible_targets": [],
                "final_repair_decision": "select_next_target_or_review_incomplete_diff",
            }),
            "validation repair unresolved",
        );
        Ok(())
    }

    pub(in crate::hosted) fn record_validation_repair_intent_satisfied(
        &mut self,
        repair_intent_id: &str,
        target_path: &str,
        expected_state_hash: Option<&str>,
        current_state_hash: &str,
        satisfied_assertions: Vec<String>,
        supporting_evidence_ids: Vec<String>,
    ) -> Result<()> {
        let (
            validation_node_id,
            failure_id,
            repair_intent,
            requested_tool_policy,
            repository_fingerprint_before,
        ) = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                action:
                    crate::hosted_orchestrator::MutationAction::RepairTarget {
                        failure,
                        fallback_policy,
                        ..
                    },
                target,
                ..
            }) if failure.category
                == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                let repair = target
                    .validation_repair
                    .as_ref()
                    .context("active validation repair lacks its repair intent")?;
                (
                    failure.node_id.clone(),
                    failure.id.clone(),
                    repair.repair_intent.clone(),
                    *fallback_policy,
                    repair.repository_fingerprint.clone().into(),
                )
            }
            _ => bail!("repair-intent satisfaction requires an active validation repair"),
        };
        let actual_hash = repo_file_sha256(&self.repo.root, target_path)
            .context("repair-intent satisfaction target is not a readable repository file")?;
        if actual_hash != current_state_hash {
            bail!("repair-intent satisfaction current hash does not match repository state");
        }
        let evidence = crate::execution_graph::AlreadyAppliedRepairEvidence {
            repair_intent_id: repair_intent_id.to_owned(),
            target_path: target_path.to_owned(),
            expected_state_hash: expected_state_hash.map(str::to_owned),
            current_state_hash: current_state_hash.to_owned(),
            satisfied_assertions,
            supporting_evidence_ids: supporting_evidence_ids
                .into_iter()
                .map(crate::execution_graph::EvidenceId::new)
                .collect(),
        };
        if !evidence.proves(&repair_intent) {
            bail!(
                "repair-intent satisfaction evidence does not prove the active assertion contract"
            );
        }
        let repository_fingerprint_after =
            repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
                sequence: self.next_domain_event_sequence(),
                validation_node_id: validation_node_id.clone(),
                failure_id: failure_id.clone(),
                result: crate::execution_graph::RepairResult::AlreadySatisfiesRepairIntent {
                    evidence: evidence.clone(),
                },
                attempt: Some(crate::execution_graph::ValidationRepairAttempt {
                    repair_intent_id: repair_intent.repair_intent_id.clone(),
                    target_path: target_path.to_owned(),
                    diagnosis: repair_intent.diagnosis,
                    requested_tool_policy,
                    outcome: crate::execution_graph::ValidationRepairMutationOutcome::AlreadySatisfiesRepairIntent,
                    repository_fingerprint_before,
                    repository_fingerprint_after: repository_fingerprint_after.clone().into(),
                    ..Default::default()
                }),
            },
        )?;
        self.append_event_recoverable(
            "validation",
            json!({
                "event_type": "worker.validation_repair_intent_satisfied",
                "failed_validation_id": failure_id,
                "repair_intent_id": repair_intent.repair_intent_id,
                "selected_target": target_path,
                "current_content_hash": current_state_hash,
                "expected_content_hash": expected_state_hash,
                "satisfied_assertions": evidence.satisfied_assertions,
                "supporting_evidence_ids": evidence.supporting_evidence_ids,
                "final_repair_decision": "rerun_validation",
                "repository_fingerprint": repository_fingerprint_after,
            }),
            "validation repair intent satisfied",
        );
        Ok(())
    }

    pub(in crate::hosted) fn record_active_validation_repair_no_change(
        &mut self,
        reason: &str,
        current_content_hash: Option<&str>,
        proposed_content_hash: Option<&str>,
    ) -> Result<bool> {
        let (
            validation_node_id,
            failure_id,
            repair_intent,
            selected_target,
            requested_tool_policy,
            repository_fingerprint_before,
            assertion_ids,
            remaining_eligible_targets,
        ) = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                action:
                    crate::hosted_orchestrator::MutationAction::RepairTarget {
                        target,
                        failure,
                        fallback_policy,
                        ..
                    },
                target: context,
                ..
            }) if failure.category
                == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                let repair = context.validation_repair.clone().unwrap_or_default();
                (
                    failure.node_id.clone(),
                    failure.id.clone(),
                    repair.repair_intent,
                    target.path.clone(),
                    *fallback_policy,
                    repair.repository_fingerprint.into(),
                    repair
                        .correction_contracts
                        .iter()
                        .map(|contract| contract.assertion_id.clone())
                        .collect::<Vec<_>>(),
                    repair.remaining_eligible_targets,
                )
            }
            _ => return Ok(false),
        };
        let snapshot = self.build_execution_snapshot()?;
        let mut attempted_targets =
            crate::hosted_orchestrator::attempted_validation_repair_targets(&snapshot, &failure_id)
                .into_iter()
                .collect::<Vec<_>>();
        if !attempted_targets.contains(&selected_target) {
            attempted_targets.push(selected_target.clone());
        }
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
                sequence: self.next_domain_event_sequence(),
                validation_node_id: validation_node_id.clone(),
                failure_id: failure_id.clone(),
                result: crate::execution_graph::RepairResult::NoMutation {
                    diagnosis: Some(repair_intent.diagnosis),
                    reason: reason.to_owned(),
                    outcome: crate::execution_graph::ValidationRepairMutationOutcome::NoChangeAgainstCurrentTarget,
                    unresolved: Some(crate::execution_graph::UnresolvedValidationRepair {
                        validation_id: failure_id.to_string(),
                        repair_intent_id: repair_intent.repair_intent_id.clone(),
                        selected_target: selected_target.clone(),
                        diagnosis: repair_intent.diagnosis,
                        outcome: crate::execution_graph::ValidationRepairMutationOutcome::NoChangeAgainstCurrentTarget,
                        reason: reason.to_owned(),
                        attempted_targets: attempted_targets.clone(),
                    }),
                },
                attempt: Some(crate::execution_graph::ValidationRepairAttempt {
                    repair_intent_id: repair_intent.repair_intent_id.clone(),
                    target_path: selected_target.clone(),
                    diagnosis: repair_intent.diagnosis,
                    requested_tool_policy,
                    outcome: crate::execution_graph::ValidationRepairMutationOutcome::NoChangeAgainstCurrentTarget,
                    repository_fingerprint_before,
                    repository_fingerprint_after: snapshot.current_repository.fingerprint.clone().into(),
                    ..Default::default()
                }),
            },
        )?;
        self.append_event_recoverable(
            "validation",
            json!({
                "event_type": "worker.validation_repair_no_change_detected",
                "validation_gate": validation_node_id,
                "failure_id": failure_id,
                "repair_intent_id": repair_intent.repair_intent_id,
                "selected_repair_target": selected_target,
                "attempted_targets": attempted_targets,
                "assertion_ids": assertion_ids,
                "current_content_hash": current_content_hash,
                "proposed_content_hash": proposed_content_hash,
                "remaining_eligible_targets": remaining_eligible_targets,
                "unresolved": true,
                "reason": reason,
                "final_repair_decision": "select_next_target_or_review_incomplete_diff",
                "diff_fingerprint": self.notebook.repository_fingerprint,
            }),
            "validation repair no change",
        );
        Ok(true)
    }

    pub(in crate::hosted) fn record_active_target_applied(
        &mut self,
        target_path: &str,
    ) -> Result<String> {
        let validation_repair = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                action:
                    crate::hosted_orchestrator::MutationAction::RepairTarget {
                        failure,
                        fallback_policy,
                        ..
                    },
                target,
                ..
            }) if failure.category
                == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                Some((
                    failure.id.clone(),
                    failure.node_id.clone(),
                    target
                        .validation_repair
                        .as_ref()
                        .map(|repair| repair.repair_intent.repair_intent_id.clone())
                        .unwrap_or_default(),
                    *fallback_policy,
                    target
                        .validation_repair
                        .as_ref()
                        .map(|repair| {
                            crate::execution_graph::RepositoryFingerprint::new(
                                repair.repository_fingerprint.clone(),
                            )
                        })
                        .unwrap_or_default(),
                    target
                        .validation_repair
                        .as_ref()
                        .map(|repair| repair.repair_intent.diagnosis)
                        .unwrap_or_default(),
                    target
                        .validation_repair
                        .as_ref()
                        .map(|repair| repair.repair_node_id.clone())
                        .unwrap_or_default(),
                    target
                        .validation_repair
                        .as_ref()
                        .map(|repair| repair.originating_implementation_node_id.clone())
                        .unwrap_or_default(),
                    target
                        .validation_repair
                        .as_ref()
                        .map(|repair| repair.target_ref.clone())
                        .unwrap_or_default(),
                    target
                        .validation_repair
                        .as_ref()
                        .map_or(0, |repair| repair.failure_revision),
                ))
            }
            Some(ExecutionDecision::RepairTarget {
                failure_id,
                context,
                ..
            }) if context.failure.category
                == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                Some((
                    failure_id.clone(),
                    context.failure.node_id.clone(),
                    context
                        .target
                        .validation_repair
                        .as_ref()
                        .map(|repair| repair.repair_intent.repair_intent_id.clone())
                        .unwrap_or_default(),
                    crate::execution_graph::MutationFallbackPolicy::NoSafeFallback,
                    context
                        .target
                        .validation_repair
                        .as_ref()
                        .map(|repair| repair.repository_fingerprint.clone().into())
                        .unwrap_or_default(),
                    context
                        .target
                        .validation_repair
                        .as_ref()
                        .map(|repair| repair.repair_intent.diagnosis)
                        .unwrap_or_default(),
                    context
                        .target
                        .validation_repair
                        .as_ref()
                        .map(|repair| repair.repair_node_id.clone())
                        .unwrap_or_default(),
                    context
                        .target
                        .validation_repair
                        .as_ref()
                        .map(|repair| repair.originating_implementation_node_id.clone())
                        .unwrap_or_default(),
                    context
                        .target
                        .validation_repair
                        .as_ref()
                        .map(|repair| repair.target_ref.clone())
                        .unwrap_or_default(),
                    context
                        .target
                        .validation_repair
                        .as_ref()
                        .map_or(0, |repair| repair.failure_revision),
                ))
            }
            _ => None,
        };
        let node_id = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget { node_id, .. })
            | Some(ExecutionDecision::RepairTarget { node_id, .. }) => node_id.clone(),
            _ => return Ok(String::new()),
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
        let created_target_evidence = self.current_decision.as_ref().and_then(|decision| {
            let target = match decision {
                ExecutionDecision::ExecuteTarget { target, .. } => &target.target,
                ExecutionDecision::RepairTarget { context, .. } => &context.target.target,
                _ => return None,
            };
            if !matches!(
                target.effective_operation(),
                crate::execution_graph::TargetOperation::CreateNew
            ) {
                return None;
            }
            let content = fs::read_to_string(self.repo.root.join(target_path)).ok()?;
            let before = self
                .notebook
                .orchestration
                .domain_events
                .iter()
                .rev()
                .find_map(|event| {
                    match event {
                    crate::execution_graph::ExecutionDomainEvent::TargetMutationProduced {
                        node_id: produced_node_id,
                        expected_repository_fingerprint,
                        ..
                    } if produced_node_id == &node_id => {
                        Some(expected_repository_fingerprint.clone())
                    }
                    crate::execution_graph::ExecutionDomainEvent::TargetMutationIntentRecorded {
                        node_id: intent_node_id,
                        repository_fingerprint,
                        ..
                    } if intent_node_id == &node_id => Some(repository_fingerprint.clone()),
                    _ => None,
                }
                })
                .unwrap_or_else(|| {
                    crate::execution_graph::RepositoryFingerprint::new(
                        self.notebook.repository_fingerprint.clone(),
                    )
                });
            let validation_gate_ids = self
                .notebook
                .orchestration
                .graph
                .as_ref()
                .into_iter()
                .flat_map(|graph| graph.nodes.iter())
                .filter(|node| node.required && node.kind.is_validation())
                .map(|node| node.id.to_string())
                .collect();
            Some(crate::execution_graph::CreatedTargetEvidence {
                path: target_path.to_owned(),
                content_hash: sha256_text(&content),
                repository_fingerprint_before: before,
                repository_fingerprint_after: crate::execution_graph::RepositoryFingerprint::new(
                    fingerprint.clone(),
                ),
                creation_tool: "create_file".into(),
                validation_gate_ids,
            })
        });
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::MutationApplied {
                sequence,
                node_id: node_id.clone(),
                target_path: target_path.to_owned(),
                repository_fingerprint: fingerprint.clone(),
                evidence_id: evidence_id.clone(),
                completed_at: now_rfc3339(),
                satisfied_intent: if validation_repair.is_some() {
                    crate::execution_graph::SatisfiedIntent::ValidationRepair
                } else if self.current_decision.as_ref().is_some_and(|decision| {
                    matches!(
                        decision,
                        ExecutionDecision::ExecuteTarget {
                            action: crate::hosted_orchestrator::MutationAction::RepairTarget { .. },
                            ..
                        }
                    )
                }) {
                    crate::execution_graph::SatisfiedIntent::MutationFallback
                } else {
                    crate::execution_graph::SatisfiedIntent::OriginalImplementation
                },
                repair_failure_id: validation_repair
                    .as_ref()
                    .map(|(failure_id, ..)| failure_id.clone()),
                created_target_evidence,
            },
        )
        .map_err(|error| {
            anyhow!(HostedInvariantFailure::new(
                "successful_mutation_not_reduced",
                error.to_string(),
            ))
        })?;
        if let Some((
            failure_id,
            validation_node_id,
            repair_intent_id,
            requested_tool_policy,
            repository_fingerprint_before,
            diagnosis,
            repair_node_id,
            originating_implementation_node_id,
            target_ref,
            failure_revision,
        )) = validation_repair
        {
            let session = self
                .notebook
                .orchestration
                .budget
                .repair_session_for_failure(&failure_id)
                .cloned();
            if let Some(session) = session.as_ref() {
                self.append_event_recoverable(
                    "validation",
                    json!({
                        "event_type": "worker.validation_failure_revision_staled",
                        "repair_session_id": session.session_id,
                        "originating_validation_gate": validation_node_id,
                        "failure_revision": session.current_assertion_set_revision,
                        "repository_fingerprint_before": repository_fingerprint_before,
                        "repository_fingerprint_after": fingerprint,
                    }),
                    "validation failure revision invalidation",
                );
            }
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
                    sequence: self.next_domain_event_sequence(),
                    validation_node_id: validation_node_id.clone(),
                    failure_id: failure_id.clone(),
                    result: crate::execution_graph::RepairResult::MutationProduced {
                        selected_target: target_path.to_owned(),
                        repair_intent_id: repair_intent_id.clone(),
                    },
                    attempt: Some(crate::execution_graph::ValidationRepairAttempt {
                        repair_intent_id: repair_intent_id.clone(),
                        target_path: target_path.to_owned(),
                        diagnosis,
                        requested_tool_policy,
                        outcome: crate::execution_graph::ValidationRepairMutationOutcome::MutationApplied,
                        repository_fingerprint_before: repository_fingerprint_before.clone(),
                        repository_fingerprint_after: fingerprint.clone().into(),
                        ..Default::default()
                    }),
                },
            )?;
            let common = json!({
                "repair_node_id": repair_node_id,
                "originating_implementation_node_id": originating_implementation_node_id,
                "target_id": target_ref.target_id,
                "target_path": target_ref.path,
                "validation_node_id": validation_node_id,
                "failure_id": failure_id,
                "failure_revision": failure_revision,
                "implementation_status_before": "completed",
                "implementation_status_after": "completed",
                "repair_status_before": "running",
                "repair_status_after": "completed",
            });
            for (event_type, message) in [
                (
                    "worker.validation_repair_operation_verified",
                    "validation repair operation verified",
                ),
                (
                    "worker.validation_repair_node_completed",
                    "validation repair node completed",
                ),
                (
                    "worker.originating_implementation_node_preserved",
                    "originating implementation node preserved",
                ),
            ] {
                let mut data = common.clone();
                data["event_type"] = Value::String(event_type.into());
                self.append_event_recoverable("validation", data, message);
            }
            self.append_event_recoverable(
                "validation",
                json!({
                    "event_type": "worker.validation_repair_attempt_recorded",
                    "repair_session_id": session.as_ref().map(|session| session.session_id.as_str()),
                    "originating_validation_gate": validation_node_id,
                    "failure_revision": session.as_ref().map(|session| session.current_assertion_set_revision),
                    "target": target_path,
                    "attempt_outcome": "mutation_applied",
                    "repository_fingerprint_before": repository_fingerprint_before,
                    "repository_fingerprint_after": fingerprint,
                }),
                "validation repair attempt",
            );
        }
        Ok(evidence_id)
    }

    pub(in crate::hosted) fn record_active_target_already_applied(
        &mut self,
        probe: &crate::execution_graph::TargetStateProbe,
    ) -> Result<()> {
        if probe.inspection_outcome()
            != crate::execution_graph::TargetInspectionOutcome::AlreadyApplied
        {
            bail!("already-applied transition requires operation-aware matching evidence");
        }
        let (node_id, satisfied_intent, repair_failure_id) = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                node_id,
                target,
                action,
            }) => {
                let repair_failure_id = target
                    .validation_repair
                    .as_ref()
                    .map(|repair| {
                        crate::execution_graph::FailureId::new(
                            repair.repair_intent.failed_validation_id.clone(),
                        )
                    })
                    .or_else(|| match action {
                        crate::hosted_orchestrator::MutationAction::RepairTarget {
                            failure,
                            ..
                        } if failure.category
                            == crate::execution_graph::FailureCategory::ValidationFailure =>
                        {
                            Some(failure.id.clone())
                        }
                        _ => None,
                    });
                let intent = if target.validation_repair.is_some() {
                    crate::execution_graph::SatisfiedIntent::ValidationRepair
                } else if matches!(
                    action,
                    crate::hosted_orchestrator::MutationAction::RepairTarget { .. }
                ) {
                    crate::execution_graph::SatisfiedIntent::MutationFallback
                } else {
                    crate::execution_graph::SatisfiedIntent::OriginalImplementation
                };
                (node_id.clone(), intent, repair_failure_id)
            }
            Some(ExecutionDecision::RepairTarget {
                node_id,
                failure_id,
                ..
            }) => (
                node_id.clone(),
                crate::execution_graph::SatisfiedIntent::ValidationRepair,
                Some(failure_id.clone()),
            ),
            _ => bail!("already-applied transition has no active mutation decision"),
        };
        let snapshot = self.build_execution_snapshot()?;
        let node = snapshot.graph.node(&node_id).ok_or_else(|| {
            crate::hosted_orchestrator::OrchestrationInvariantError::for_node(
                "already_applied_node_did_not_converge",
                node_id.clone(),
                "already-applied transition refers to an unknown active node",
            )
        })?;
        if node.status != crate::execution_graph::ExecutionNodeStatus::Running {
            return Err(
                crate::hosted_orchestrator::OrchestrationInvariantError::for_node(
                    "already_applied_node_did_not_converge",
                    node_id.clone(),
                    format!("node is {:?}, expected Running", node.status),
                )
                .into(),
            );
        }
        let attempt = node
            .attempts
            .last()
            .map(|attempt| attempt.attempt)
            .ok_or_else(|| {
                crate::hosted_orchestrator::OrchestrationInvariantError::for_node(
                    "already_applied_node_did_not_converge",
                    node_id.clone(),
                    "active node has no persisted attempt",
                )
            })?;
        let node_status_before = node.status;
        let lifecycle_before = node.repository_mutation_lifecycle;
        let repository_fingerprint_before = node
            .attempts
            .last()
            .map(|attempt| attempt.repository_fingerprint_before.clone());
        let repair_intent_kind = satisfied_intent.repair_intent_kind();
        let repair_budget_owner = match repair_intent_kind {
            Some(crate::execution_graph::RepairIntentKind::ValidationRepair) => {
                repair_failure_id.as_ref().map(|failure_id| {
                    crate::execution_graph::ExecutionNodeId::new(
                        crate::execution_graph::BudgetState::repair_session_id(failure_id),
                    )
                })
            }
            Some(crate::execution_graph::RepairIntentKind::MutationApplicationFallback) => {
                Some(node_id.clone())
            }
            _ => None,
        };
        let repair_budget = repair_intent_kind.and_then(|kind| {
            let owner = repair_budget_owner.as_ref()?;
            let maximum = match kind {
                crate::execution_graph::RepairIntentKind::ValidationRepair => repair_failure_id
                    .as_ref()
                    .and_then(|failure_id| {
                        snapshot
                            .budget
                            .repair_session_for_failure(failure_id)
                            .map(|session| session.budget.max_target_attempts)
                    })
                    .unwrap_or_default(),
                crate::execution_graph::RepairIntentKind::MutationApplicationFallback => {
                    node.budget.max_mutation_fallback_attempts
                }
                _ => 0,
            };
            Some(snapshot.budget.repair_budget_for(kind, owner, maximum))
        });
        let transition = crate::execution_graph::AlreadyAppliedTransition {
            node_id: node_id.clone(),
            operation: probe.operation.clone(),
            target_path: probe.target_path.clone(),
            expected_result_hash: probe.expected_result_content_hash.clone(),
            observed_result_hash: probe.target_content_hash.clone(),
            repository_fingerprint: probe.repository_fingerprint.clone(),
            completed_at: now_rfc3339(),
        };
        let semantic_id =
            transition.semantic_id(&self.manifest.execution.execution_id.to_string(), attempt);
        let validation_repair_failure_id = repair_failure_id.clone();
        let revision_before = snapshot.graph.revision;
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.repository_operation_reduction_started",
                "node_id": node_id,
                "attempt_id": attempt,
                "operation": probe.operation.as_str(),
                "target_path": probe.target_path,
                "result": "already_applied",
                "repair_intent_kind": repair_intent_kind,
                "repair_budget_owner": repair_budget_owner,
                "repair_budget": repair_budget,
                "repository_fingerprint_before": repository_fingerprint_before,
                "repository_fingerprint_after": probe.repository_fingerprint,
                "verification_evidence_id": semantic_id,
                "node_status_before": node_status_before,
                "mutation_lifecycle_before": lifecycle_before,
                "semantic_decision_hash": semantic_id,
                "graph_revision_before": revision_before,
            }),
            "repository operation reduction",
        );
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::TargetOperationAlreadyApplied {
                sequence: self.next_domain_event_sequence(),
                execution_id: self.manifest.execution.execution_id.to_string(),
                attempt,
                transition: transition.clone(),
                semantic_id: semantic_id.clone(),
                satisfied_intent,
                repair_failure_id,
            },
        )
        .map_err(|error| {
            anyhow!(HostedInvariantFailure::new(
                "successful_mutation_not_reduced",
                error.to_string(),
            ))
        })?;
        self.current_decision = None;
        self.persist_orchestration_checkpoint("already_applied_node_completed", false)?;
        let graph = self
            .notebook
            .orchestration
            .graph
            .as_ref()
            .context("already-applied transition lost the execution graph")?;
        if !graph.node(&node_id).is_some_and(|node| {
            node.status == crate::execution_graph::ExecutionNodeStatus::Completed
        }) {
            return Err(
                crate::hosted_orchestrator::OrchestrationInvariantError::for_node(
                    "already_applied_node_did_not_converge",
                    node_id.clone(),
                    "node remained active after its durable successful transition",
                )
                .into(),
            );
        }
        let revision_after = graph.revision;
        let node_after = graph.node(&node_id);
        let node_status_after = node_after.map(|node| node.status);
        let lifecycle_after = node_after.and_then(|node| node.repository_mutation_lifecycle);
        let selected_next_node = graph.next_runnable_node().map(|node| node.id.to_string());
        for (event_type, description) in [
            (
                "worker.repository_operation_reduced",
                "repository operation reduced",
            ),
            (
                "worker.already_applied_transition_persisted",
                "already-applied transition persisted",
            ),
            (
                "worker.node_completed_from_already_applied",
                "node completed from already-applied evidence",
            ),
            (
                "worker.next_ready_node_selected",
                "next ready node selection",
            ),
        ] {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": event_type,
                    "node_id": node_id,
                    "attempt_id": attempt,
                    "operation": transition.operation.as_str(),
                    "target_path": transition.target_path,
                    "result": "already_applied",
                    "repair_intent_kind": repair_intent_kind,
                    "repair_budget_owner": repair_budget_owner,
                    "repair_budget": repair_budget,
                    "repository_fingerprint_before": repository_fingerprint_before,
                    "repository_fingerprint_after": transition.repository_fingerprint,
                    "verification_evidence_id": semantic_id,
                    "node_status_before": node_status_before,
                    "node_status_after": node_status_after,
                    "mutation_lifecycle_before": lifecycle_before,
                    "mutation_lifecycle_after": lifecycle_after,
                    "semantic_decision_hash": semantic_id,
                    "graph_revision_before": revision_before,
                    "graph_revision_after": revision_after,
                    "selected_next_node": selected_next_node,
                }),
                description,
            );
        }
        if satisfied_intent == crate::execution_graph::SatisfiedIntent::ValidationRepair
            && let Some(metadata) = node_after.and_then(|node| node.validation_repair.as_ref())
            && let Some(failure_id) = validation_repair_failure_id
        {
            let validation_node_id = self
                .notebook
                .orchestration
                .failures
                .get(&failure_id)
                .map(|failure| failure.node_id.clone());
            let common = json!({
                "repair_node_id": metadata.repair_node_id,
                "originating_implementation_node_id": metadata.originating_implementation_node_id,
                "target_id": metadata.target.target_id,
                "target_path": metadata.target.path,
                "validation_node_id": validation_node_id,
                "failure_id": failure_id,
                "failure_revision": metadata.failure_revision,
                "implementation_status_before": "completed",
                "implementation_status_after": "completed",
                "repair_status_before": "running",
                "repair_status_after": "completed",
            });
            for (event_type, message) in [
                (
                    "worker.validation_repair_operation_verified",
                    "validation repair operation verified",
                ),
                (
                    "worker.validation_repair_node_completed",
                    "validation repair node completed",
                ),
                (
                    "worker.originating_implementation_node_preserved",
                    "originating implementation node preserved",
                ),
            ] {
                let mut data = common.clone();
                data["event_type"] = Value::String(event_type.into());
                self.append_event_recoverable("validation", data, message);
            }
        }
        Ok(())
    }

    pub(in crate::hosted) fn prepare_active_target_context(
        &mut self,
    ) -> Result<TargetContextPreparationResult> {
        let (node_id, target, accepted_intent_hash) = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                node_id,
                target: context,
                ..
            }) => (
                node_id.clone(),
                context.target.clone(),
                context.accepted_intent_hash.clone(),
            ),
            _ => return Ok(TargetContextPreparationResult::Prepared),
        };
        let target_path = target.path.clone();
        let operation = target.effective_operation();
        let source_path = operation.source_path().map(str::to_owned);
        let fingerprint = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let inspected_path = match safe_repo_path(&self.repo.root, &target_path, true) {
            Ok(path) => path,
            Err(error) => {
                let (category, code) = match error.kind {
                    RepoPathErrorKind::NotAllowed => (
                        crate::execution_graph::FailureCategory::PlanRepositoryConflict,
                        "unsafe_target_path",
                    ),
                    RepoPathErrorKind::NotFound | RepoPathErrorKind::Infrastructure => (
                        crate::execution_graph::FailureCategory::InfrastructureFailure,
                        "target_inspection_failed",
                    ),
                };
                self.record_active_target_failure_with_code(
                    category,
                    Some(code),
                    &json!({
                        "code": code,
                        "operation": operation.as_str(),
                        "target_path": target_path,
                        "message": error.to_string(),
                    })
                    .to_string(),
                )?;
                return Ok(TargetContextPreparationResult::Prepared);
            }
        };
        let target_exists = inspected_path.is_file();
        let mut evidence = self
            .notebook
            .orchestration
            .evidence
            .reusable_file(&target_path, &fingerprint, None)
            .cloned()
            .filter(|_| target_exists);
        if target_exists && evidence.is_none() {
            let content = match fs::read_to_string(&inspected_path) {
                Ok(content) => content,
                Err(error) => {
                    self.record_active_target_failure_with_code(
                        crate::execution_graph::FailureCategory::InfrastructureFailure,
                        Some("target_inspection_failed"),
                        &json!({
                            "code": "target_inspection_failed",
                            "operation": operation.as_str(),
                            "target_path": target_path,
                            "error_kind": format!("{:?}", error.kind()),
                        })
                        .to_string(),
                    )?;
                    return Ok(TargetContextPreparationResult::Prepared);
                }
            };
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
        let mut source_evidence = None;
        let source_exists = if let Some(source_path) = source_path.as_deref() {
            let source = match safe_repo_path(&self.repo.root, source_path, true) {
                Ok(path) => path,
                Err(error) => {
                    let category = if error.kind == RepoPathErrorKind::NotAllowed {
                        crate::execution_graph::FailureCategory::PlanRepositoryConflict
                    } else {
                        crate::execution_graph::FailureCategory::InfrastructureFailure
                    };
                    let code = if category
                        == crate::execution_graph::FailureCategory::InfrastructureFailure
                    {
                        "target_inspection_failed"
                    } else {
                        "unsafe_source_path"
                    };
                    self.record_active_target_failure_with_code(
                        category,
                        Some(code),
                        &json!({
                            "code": code,
                            "operation": operation.as_str(),
                            "source_path": source_path,
                            "target_path": target_path,
                            "message": error.to_string(),
                        })
                        .to_string(),
                    )?;
                    return Ok(TargetContextPreparationResult::Prepared);
                }
            };
            let exists = source.is_file();
            if exists {
                let content = match fs::read_to_string(&source) {
                    Ok(content) => content,
                    Err(error) => {
                        self.record_active_target_failure_with_code(
                            crate::execution_graph::FailureCategory::InfrastructureFailure,
                            Some("target_inspection_failed"),
                            &json!({
                                "code": "target_inspection_failed",
                                "operation": operation.as_str(),
                                "source_path": source_path,
                                "error_kind": format!("{:?}", error.kind()),
                            })
                            .to_string(),
                        )?;
                        return Ok(TargetContextPreparationResult::Prepared);
                    }
                };
                let captured = crate::execution_graph::FileEvidence::capture(
                    source_path,
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
                source_evidence = Some(captured);
            }
            Some(exists)
        } else {
            None
        };

        let target_content_hash = evidence.as_ref().map(|value| value.content_hash.clone());
        let expected_result_content_hash = self
            .notebook
            .orchestration
            .domain_events
            .iter()
            .rev()
            .find_map(|event| match event {
                crate::execution_graph::ExecutionDomainEvent::TargetMutationIntentRecorded {
                    node_id: recorded_node_id,
                    target_path: recorded_target_path,
                    operation: recorded_operation,
                    expected_result_content_hash,
                    accepted_intent_hash: recorded_intent_hash,
                    ..
                } if recorded_node_id == &node_id
                    && recorded_target_path == &target_path
                    && recorded_operation == &operation
                    && recorded_intent_hash == &accepted_intent_hash =>
                {
                    expected_result_content_hash.clone()
                }
                _ => None,
            });
        let probe = crate::execution_graph::TargetStateProbe {
            operation: operation.clone(),
            target_path: target_path.clone(),
            target_exists,
            source_exists,
            target_content_hash: target_content_hash.clone(),
            source_content_hash: source_evidence
                .as_ref()
                .map(|value| value.content_hash.clone()),
            expected_result_content_hash,
            repository_fingerprint: crate::execution_graph::RepositoryFingerprint::new(
                fingerprint.clone(),
            ),
        };
        let inspection_outcome = probe.inspection_outcome();
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.target_state_probed",
                "node_id": node_id,
                "operation": operation.as_str(),
                "target_path": target_path,
                "source_path": source_path,
                "target_exists": target_exists,
                "source_exists": source_exists,
                "target_content_hash": target_content_hash,
                "source_content_hash": probe.source_content_hash,
                "expected_result_content_hash": probe.expected_result_content_hash,
                "repository_fingerprint": fingerprint,
                "inspection_outcome": inspection_outcome,
                "process_health": "healthy",
                "mission_outcome": "continuing",
            }),
            "target state probed",
        );
        if inspection_outcome
            == crate::execution_graph::TargetInspectionOutcome::NewTargetConfirmedAbsent
        {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.new_target_absence_confirmed",
                    "node_id": node_id,
                    "operation": operation.as_str(),
                    "target_path": target_path,
                    "target_exists": false,
                    "repository_fingerprint": fingerprint,
                    "verification_result": "confirmed_absent",
                    "process_health": "healthy",
                    "mission_outcome": "continuing",
                }),
                "new target absence confirmed",
            );
        }
        if let crate::execution_graph::TargetInspectionOutcome::OperationConflict { conflict } =
            &inspection_outcome
        {
            let category = if matches!(
                conflict.code.as_str(),
                "expected_existing_target_missing" | "expected_source_target_missing"
            ) {
                crate::execution_graph::FailureCategory::PlanRepositoryConflict
            } else {
                crate::execution_graph::FailureCategory::MutationConflict
            };
            let code = conflict.code.as_str();
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.target_operation_conflict",
                    "node_id": node_id,
                    "operation": operation.as_str(),
                    "target_path": target_path,
                    "source_path": source_path,
                    "failure_code": code,
                    "repository_fingerprint": fingerprint,
                    "process_health": "healthy",
                    "mission_outcome": "incomplete",
                }),
                "target operation conflict",
            );
            self.record_active_target_failure_with_code(
                category,
                Some(code),
                &json!({
                    "code": code,
                    "operation": operation.as_str(),
                    "target_path": target_path,
                    "source_path": source_path,
                })
                .to_string(),
            )?;
            return Ok(TargetContextPreparationResult::Prepared);
        }
        if inspection_outcome == crate::execution_graph::TargetInspectionOutcome::AlreadyApplied {
            self.record_active_target_already_applied(&probe)?;
            return Ok(TargetContextPreparationResult::Prepared);
        }
        let already_prepared = target_context_already_prepared(
            &self.notebook.orchestration.domain_events,
            &TargetContextIdentity {
                node_id: &node_id,
                target_path: &target_path,
                operation: &operation,
                source_path: source_path.as_deref(),
                target_content_hash: &target_content_hash,
                repository_fingerprint: &fingerprint,
                accepted_intent_hash: &accepted_intent_hash,
            },
        );
        if already_prepared {
            return Ok(TargetContextPreparationResult::TargetContextAlreadyPrepared);
        }
        let evidence_ids = evidence
            .as_ref()
            .map(|evidence| vec![evidence.evidence_id.clone()])
            .unwrap_or_default();
        let mut evidence_ids = evidence_ids;
        if let Some(source) = source_evidence.as_ref() {
            evidence_ids.push(source.evidence_id.clone());
        }
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::TargetContextPrepared {
                sequence: self.next_domain_event_sequence(),
                node_id: node_id.clone(),
                target_path: target_path.clone(),
                operation: operation.clone(),
                source_path: source_path.clone(),
                target_exists: Some(target_exists),
                source_exists,
                repository_fingerprint: crate::execution_graph::RepositoryFingerprint::new(
                    fingerprint.clone(),
                ),
                target_content_hash: target_content_hash.clone(),
                source_content_hash: source_evidence
                    .as_ref()
                    .map(|value| value.content_hash.clone()),
                accepted_intent_hash,
                evidence_ids,
            },
        )?;
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.target_operation_context_prepared",
                "node_id": node_id,
                "operation": operation.as_str(),
                "target_path": target_path,
                "source_path": source_path,
                "target_exists": target_exists,
                "source_exists": source_exists,
                "repository_fingerprint": fingerprint,
                "process_health": "healthy",
                "mission_outcome": "continuing",
            }),
            "target operation context prepared",
        );
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
        let repair = self
            .current_decision
            .as_ref()
            .and_then(|decision| match decision {
                ExecutionDecision::ExecuteTarget {
                    action:
                        crate::hosted_orchestrator::MutationAction::RepairTarget {
                            failure,
                            fallback_policy,
                            ..
                        },
                    ..
                } => Some((failure.clone(), *fallback_policy)),
                _ => None,
            });
        let repair_diagnostic = repair.as_ref().and_then(|_| {
            self.notebook
                .mutation_diagnostics
                .iter()
                .rev()
                .find(|diagnostic| diagnostic.target_path == target_path)
                .cloned()
        });
        let fingerprint = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let event_fingerprint = fingerprint.clone();
        let event_before_hash = before_content_hash.clone();
        let event_after_hash = after_content_hash.clone();
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::TargetMutationProduced {
                sequence: self.next_domain_event_sequence(),
                node_id: node_id.clone(),
                target_path: target_path.to_owned(),
                expected_repository_fingerprint: expected,
                repository_fingerprint: crate::execution_graph::RepositoryFingerprint::new(
                    fingerprint,
                ),
                before_content_hash,
                after_content_hash,
            },
        )?;
        if let Some((failure, fallback_policy)) = repair {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.mutation_repair_applied",
                    "node_id": node_id,
                    "target_path": target_path,
                    "target_operation": self.current_decision.as_ref().and_then(|decision| match decision {
                        ExecutionDecision::ExecuteTarget { target, .. } => Some(target.target.effective_operation()),
                        _ => None,
                    }),
                    "original_tool": repair_diagnostic.as_ref().map(|diagnostic| &diagnostic.tool),
                    "original_failure_category": failure.code,
                    "selected_fallback_policy": fallback_policy,
                    "permitted_tools": fallback_policy.permitted_tools(),
                    "forced_tool_choice": fallback_policy.forced_tool(),
                    "repair_call_number": repair_diagnostic.as_ref().map(|diagnostic| diagnostic.repair_attempt),
                    "before_content_hash": event_before_hash,
                    "after_content_hash": event_after_hash,
                    "repository_fingerprint": event_fingerprint,
                    "verification_result": "pending",
                }),
                "mutation repair application",
            );
        }
        Ok(())
    }

    pub(in crate::hosted) fn record_active_target_mutation_intent(
        &mut self,
        expected_result_content_hash: Option<String>,
    ) -> Result<()> {
        let (node_id, context) = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                node_id, target, ..
            }) => (node_id.clone(), target.clone()),
            _ => return Ok(()),
        };
        let operation = context.target.effective_operation();
        let fingerprint = crate::execution_graph::RepositoryFingerprint::new(
            context.repository_fingerprint.clone(),
        );
        let duplicate = self
            .notebook
            .orchestration
            .domain_events
            .iter()
            .rev()
            .any(|event| {
                matches!(
                    event,
                    crate::execution_graph::ExecutionDomainEvent::TargetMutationIntentRecorded {
                        node_id: recorded_node_id,
                        target_path,
                        operation: recorded_operation,
                        expected_result_content_hash: recorded_result_hash,
                        repository_fingerprint,
                        accepted_intent_hash,
                        ..
                    } if recorded_node_id == &node_id
                        && target_path == &context.target.path
                        && recorded_operation == &operation
                        && recorded_result_hash == &expected_result_content_hash
                        && repository_fingerprint == &fingerprint
                        && accepted_intent_hash == &context.accepted_intent_hash
                )
            });
        if duplicate {
            return Ok(());
        }
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::TargetMutationIntentRecorded {
                sequence: self.next_domain_event_sequence(),
                node_id,
                target_path: context.target.path.clone(),
                operation: operation.clone(),
                source_path: operation.source_path().map(str::to_owned),
                expected_result_content_hash,
                expected_source_content_hash: context.source_content_hash,
                repository_fingerprint: fingerprint,
                accepted_intent_hash: context.accepted_intent_hash,
            },
        )?;
        self.persist_orchestration_checkpoint("target_mutation_intent_recorded", false)
    }

    pub(in crate::hosted) fn verify_active_target_state(&mut self) -> Result<()> {
        let (node_id, target_path, operation) = match self.current_decision.as_ref() {
            Some(ExecutionDecision::ExecuteTarget {
                node_id, target, ..
            }) => (
                node_id.clone(),
                target
                    .target
                    .effective_operation()
                    .destination_path(&target.target.path)
                    .to_owned(),
                target.target.effective_operation(),
            ),
            _ => return Ok(()),
        };
        let source_path = operation.source_path().map(str::to_owned);
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
        let repair_attempt = usage.mutation_fallback_attempts;
        let active_node = self
            .notebook
            .orchestration
            .graph
            .as_ref()
            .and_then(|graph| graph.node(&node_id));
        let attempt_id = active_node
            .and_then(|node| node.attempts.last())
            .map(|attempt| attempt.attempt);
        let repository_fingerprint_before = active_node
            .and_then(|node| node.attempts.last())
            .map(|attempt| attempt.repository_fingerprint_before.clone());
        let node_status_before = active_node.map(|node| node.status);
        let lifecycle_before = active_node.and_then(|node| node.repository_mutation_lifecycle);
        let (repair_intent_kind, repair_budget_owner, repair_budget) = match self
            .current_decision
            .as_ref()
        {
            Some(ExecutionDecision::ExecuteTarget {
                action: crate::hosted_orchestrator::MutationAction::RepairTarget { failure, .. },
                ..
            }) if failure.category
                == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                let session = self
                    .notebook
                    .orchestration
                    .budget
                    .repair_session_for_failure(&failure.id);
                let owner = crate::execution_graph::ExecutionNodeId::new(
                    session
                        .map(|session| session.session_id.clone())
                        .unwrap_or_else(|| {
                            crate::execution_graph::BudgetState::repair_session_id(&failure.id)
                        }),
                );
                let maximum = session.map_or(0, |session| session.budget.max_target_attempts);
                (
                    Some(crate::execution_graph::RepairIntentKind::ValidationRepair),
                    Some(owner.clone()),
                    Some(self.notebook.orchestration.budget.repair_budget_for(
                        crate::execution_graph::RepairIntentKind::ValidationRepair,
                        &owner,
                        maximum,
                    )),
                )
            }
            Some(ExecutionDecision::RepairTarget { context, .. })
                if context.failure.category
                    == crate::execution_graph::FailureCategory::ValidationFailure =>
            {
                let session = self
                    .notebook
                    .orchestration
                    .budget
                    .repair_session_for_failure(&context.failure.id);
                let owner = crate::execution_graph::ExecutionNodeId::new(
                    session
                        .map(|session| session.session_id.clone())
                        .unwrap_or_else(|| {
                            crate::execution_graph::BudgetState::repair_session_id(
                                &context.failure.id,
                            )
                        }),
                );
                let maximum = session.map_or(0, |session| session.budget.max_target_attempts);
                (
                    Some(crate::execution_graph::RepairIntentKind::ValidationRepair),
                    Some(owner.clone()),
                    Some(self.notebook.orchestration.budget.repair_budget_for(
                        crate::execution_graph::RepairIntentKind::ValidationRepair,
                        &owner,
                        maximum,
                    )),
                )
            }
            Some(ExecutionDecision::ExecuteTarget {
                action: crate::hosted_orchestrator::MutationAction::RepairTarget { .. },
                ..
            })
            | Some(ExecutionDecision::RepairTarget { .. }) => {
                let maximum =
                    active_node.map_or(0, |node| node.budget.max_mutation_fallback_attempts);
                (
                    Some(crate::execution_graph::RepairIntentKind::MutationApplicationFallback),
                    Some(node_id.clone()),
                    Some(self.notebook.orchestration.budget.repair_budget_for(
                        crate::execution_graph::RepairIntentKind::MutationApplicationFallback,
                        &node_id,
                        maximum,
                    )),
                )
            }
            _ => (None, None, None),
        };
        let observed_target_hash = repo_file_sha256(&self.repo.root, &target_path);
        let source_absent = operation
            .source_path()
            .is_none_or(|path| !self.repo.root.join(path).exists());
        let target_state_verified = match operation {
            crate::execution_graph::TargetOperation::ModifyExisting
            | crate::execution_graph::TargetOperation::CreateNew => {
                produced.2.is_some() && observed_target_hash == produced.2
            }
            crate::execution_graph::TargetOperation::DeleteExisting => {
                produced.2.is_none() && observed_target_hash.is_none()
            }
            crate::execution_graph::TargetOperation::Rename { .. }
            | crate::execution_graph::TargetOperation::Move { .. } => {
                source_absent
                    && produced.2.is_some()
                    && observed_target_hash == produced.2
                    && operation
                        .source_path()
                        .is_some_and(|path| changed_paths.iter().any(|changed| changed == path))
            }
        };
        if produced.0 != target_path
            || produced.1 == produced.2
            || !changed_paths.contains(&target_path)
            || !target_state_verified
        {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.target_operation_verified",
                    "node_id": node_id,
                    "operation": operation.as_str(),
                    "target_path": target_path,
                    "source_path": source_path,
                    "selected_mutation_tool": mutation_tool.clone(),
                    "verification_result": "failed",
                    "before_content_hash": produced.1,
                    "after_content_hash": produced.2,
                    "repository_fingerprint": repository_fingerprint,
                    "failure_category": MutationApplicationFailure::MutationProducedNoChange,
                    "mutation_attempt": mutation_attempt,
                    "repair_attempt": repair_attempt,
                }),
                "target mutation verification",
            );
            self.record_active_target_failure_with_code(
                crate::execution_graph::FailureCategory::MutationConflict,
                Some(MutationApplicationFailure::MutationProducedNoChange.as_str()),
                "MutationNotProduced: deterministic verification found no attributable target change",
            )?;
            return Ok(());
        }
        let graph_revision_before = self.notebook.orchestration.graph_revision;
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.repository_operation_reduction_started",
                "node_id": node_id,
                "attempt_id": attempt_id,
                "operation": operation.as_str(),
                "target_path": target_path,
                "result": "verified",
                "repair_intent_kind": repair_intent_kind,
                "repair_budget_owner": repair_budget_owner,
                "repair_budget": repair_budget,
                "repository_fingerprint_before": repository_fingerprint_before,
                "repository_fingerprint_after": repository_fingerprint,
                "verification_evidence_id": null,
                "node_status_before": node_status_before,
                "mutation_lifecycle_before": lifecycle_before,
                "graph_revision_before": graph_revision_before,
            }),
            "repository operation reduction",
        );
        let verification_evidence_id = self.record_active_target_applied(&target_path)?;
        let reduced_snapshot = self.build_execution_snapshot()?;
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.lifecycle_invariant_check_started",
                "invariant_id": "verified_operation_missing_operation_evidence",
                "scope": crate::execution_graph::InvariantScope::RepositoryOperationReduction,
                "phase": self.phases.active(),
                "required_evidence_kinds": [crate::execution_graph::EvidenceKind::RepositoryOperationVerification],
                "available_evidence_kinds": [crate::execution_graph::EvidenceKind::RepositoryOperationVerification],
                "current_node": node_id,
                "graph_revision": reduced_snapshot.graph.revision(),
                "repository_fingerprint": repository_fingerprint,
            }),
            "repository operation lifecycle invariant check",
        );
        if let Err(violation) = crate::execution_graph::check_invariants(
            &reduced_snapshot.graph,
            crate::execution_graph::LifecycleState::RepositoryOperationReduction,
            crate::execution_graph::InvariantTrigger::RepositoryOperationReduced,
        ) {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.lifecycle_invariant_check_failed",
                    "invariant_id": violation.code,
                    "scope": violation.scope,
                    "phase": self.phases.active(),
                    "required_evidence_kinds": [crate::execution_graph::EvidenceKind::RepositoryOperationVerification],
                    "available_evidence_kinds": [],
                    "current_node": violation.node_id,
                    "graph_revision": reduced_snapshot.graph.revision(),
                    "repository_fingerprint": repository_fingerprint,
                }),
                "repository operation lifecycle invariant failure",
            );
            return Err(anyhow!(HostedInvariantFailure::in_phase(
                violation.code,
                self.phases.active().as_str(),
                violation.to_string(),
            )));
        }
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.lifecycle_invariant_check_passed",
                "invariant_id": "verified_operation_missing_operation_evidence",
                "scope": crate::execution_graph::InvariantScope::RepositoryOperationReduction,
                "phase": self.phases.active(),
                "required_evidence_kinds": [crate::execution_graph::EvidenceKind::RepositoryOperationVerification],
                "available_evidence_kinds": [crate::execution_graph::EvidenceKind::RepositoryOperationVerification],
                "current_node": node_id,
                "graph_revision": reduced_snapshot.graph.revision(),
                "repository_fingerprint": repository_fingerprint,
            }),
            "repository operation lifecycle invariant check",
        );
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.lifecycle_invariant_not_applicable",
                "invariant_id": "current_validation_missing_at_completion",
                "scope": crate::execution_graph::InvariantScope::RepositoryOperationReduction,
                "phase": self.phases.active(),
                "required_evidence_kinds": [crate::execution_graph::EvidenceKind::ValidationGateResult],
                "available_evidence_kinds": [],
                "current_validation_evidence_required": false,
                "reason": "implementation barrier not yet reached; repository-operation verification is the only required evidence",
                "current_node": node_id,
                "graph_revision": reduced_snapshot.graph.revision(),
                "repository_fingerprint": repository_fingerprint,
            }),
            "future validation evidence suppression",
        );
        let barrier = reduced_snapshot.graph.implementation_barrier_proof(
            reduced_snapshot
                .current_repository
                .fingerprint
                .clone()
                .into(),
        );
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.implementation_barrier_created",
                "repository_fingerprint": barrier.repository_fingerprint,
                "required_nodes": barrier.required_nodes,
                "completed_nodes": barrier.completed_nodes,
                "unresolved_nodes": barrier.unresolved_nodes,
                "satisfied": barrier.satisfied,
                "graph_revision": reduced_snapshot.graph.revision(),
            }),
            "implementation barrier proof",
        );
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": if barrier.satisfied {
                    "worker.implementation_barrier_satisfied"
                } else {
                    "worker.implementation_barrier_unsatisfied"
                },
                "repository_fingerprint": barrier.repository_fingerprint,
                "required_nodes": barrier.required_nodes,
                "completed_nodes": barrier.completed_nodes,
                "unresolved_nodes": barrier.unresolved_nodes,
                "graph_revision": reduced_snapshot.graph.revision(),
            }),
            "implementation barrier state",
        );
        if crate::execution_graph::resolve_next_phase(&reduced_snapshot.graph)
            == crate::execution_graph::LifecyclePhase::Implementation
            && let Some(next) = reduced_snapshot
                .graph
                .next_runnable_node()
                .filter(|node| node.kind.is_mutation())
        {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.next_implementation_node_selected",
                    "completed_node": node_id,
                    "next_node": next.id,
                    "next_node_kind": next.kind,
                    "graph_revision": reduced_snapshot.graph.revision(),
                    "repository_fingerprint": repository_fingerprint,
                }),
                "next implementation node selection",
            );
        }
        let reduced_status = reduced_snapshot
            .graph
            .node(&node_id)
            .map(|node| node.status);
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.repository_operation_reduced",
                "node_id": node_id,
                "attempt_id": attempt_id,
                "operation": operation.as_str(),
                "target_path": target_path,
                "result": "verified",
                "repair_intent_kind": repair_intent_kind,
                "repair_budget_owner": repair_budget_owner,
                "repair_budget": repair_budget,
                "repository_fingerprint_before": repository_fingerprint_before,
                "repository_fingerprint_after": repository_fingerprint,
                "verification_evidence_id": verification_evidence_id,
                "node_status_before": node_status_before,
                "node_status_after": reduced_status,
                "mutation_lifecycle_before": lifecycle_before,
                "mutation_lifecycle_after": reduced_snapshot.graph.node(&node_id)
                    .and_then(|node| node.repository_mutation_lifecycle),
                "graph_revision_before": graph_revision_before,
                "graph_revision_after": reduced_snapshot.graph.revision,
                "node_status": reduced_status,
            }),
            "repository operation reduction",
        );
        if reduced_status == Some(crate::execution_graph::ExecutionNodeStatus::Completed) {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.node_completed_from_verified_write",
                    "node_id": node_id,
                    "attempt_id": attempt_id,
                    "operation": operation.as_str(),
                    "target_path": target_path,
                    "repair_intent_kind": repair_intent_kind,
                    "repair_budget_owner": repair_budget_owner,
                    "repository_fingerprint_before": repository_fingerprint_before,
                    "repository_fingerprint_after": repository_fingerprint,
                    "verification_evidence_id": verification_evidence_id,
                    "node_status_before": node_status_before,
                    "node_status_after": reduced_status,
                    "graph_revision": reduced_snapshot.graph.revision,
                }),
                "node completion from verified write",
            );
        }
        let repair_mutation_id = format!(
            "repair-{}",
            sha256_text(&format!(
                "{node_id}\0{target_path}\0{}",
                produced.2.as_deref().unwrap_or_default()
            ))
        );
        let repaired_diagnostic = self
            .notebook
            .mutation_diagnostics
            .iter_mut()
            .rev()
            .find(|diagnostic| diagnostic.target_path == target_path);
        let repaired_policy = repaired_diagnostic
            .as_ref()
            .map(|diagnostic| diagnostic.fallback_policy);
        let repaired_failure = repaired_diagnostic
            .as_ref()
            .map(|diagnostic| diagnostic.failure_category);
        let repaired_original_tool = repaired_diagnostic
            .as_ref()
            .map(|diagnostic| diagnostic.tool.clone());
        let repaired_call_number = repaired_diagnostic
            .as_ref()
            .map(|diagnostic| diagnostic.repair_attempt);
        if let Some(diagnostic) = repaired_diagnostic
            && let Some(rejected) = diagnostic.rejected_mutation.as_mut()
        {
            rejected.status = crate::execution_graph::FailureStatus::Recovered;
            rejected.superseded_by = Some(repair_mutation_id.clone());
            rejected.resolved_repository_fingerprint = Some(
                crate::execution_graph::RepositoryFingerprint::new(repository_fingerprint.clone()),
            );
        }
        if let Some(policy) = repaired_policy {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.mutation_repair_verified",
                    "node_id": node_id,
                    "target_path": target_path,
                    "target_operation": operation,
                    "original_tool": repaired_original_tool,
                    "original_failure_category": repaired_failure,
                    "selected_fallback_policy": policy,
                    "repair_mutation_id": repair_mutation_id,
                    "repair_call_number": repaired_call_number,
                    "before_content_hash": produced.1,
                    "after_content_hash": produced.2,
                    "repository_fingerprint": repository_fingerprint,
                    "verification_result": "verified",
                    "original_failure_status": "recovered",
                }),
                "mutation repair verification",
            );
        }
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.target_operation_verified",
                "node_id": node_id,
                "operation": operation.as_str(),
                "target_path": target_path,
                "source_path": source_path,
                "selected_mutation_tool": mutation_tool,
                "verification_result": "verified",
                "before_content_hash": produced.1,
                "after_content_hash": produced.2,
                "repository_fingerprint": repository_fingerprint,
                "failure_category": Value::Null,
                "mutation_attempt": mutation_attempt,
                "repair_attempt": repair_attempt,
            }),
            "target mutation verification",
        );
        self.current_decision = None;
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
            operation: crate::execution_graph::TargetOperation::ModifyExisting,
            source_path: None,
            target_exists: Some(true),
            source_exists: None,
            repository_fingerprint: crate::execution_graph::RepositoryFingerprint::new("tree-1"),
            target_content_hash: content_hash.clone(),
            source_content_hash: None,
            accepted_intent_hash: "intent-1".into(),
            evidence_ids: vec!["file-1".into()],
        };
        assert!(target_context_already_prepared(
            std::slice::from_ref(&event),
            &TargetContextIdentity {
                node_id: &node_id,
                target_path: "src/theme.ts",
                operation: &crate::execution_graph::TargetOperation::ModifyExisting,
                source_path: None,
                target_content_hash: &content_hash,
                repository_fingerprint: "tree-1",
                accepted_intent_hash: "intent-1",
            },
        ));
        assert!(!target_context_already_prepared(
            std::slice::from_ref(&event),
            &TargetContextIdentity {
                node_id: &node_id,
                target_path: "src/theme.ts",
                operation: &crate::execution_graph::TargetOperation::ModifyExisting,
                source_path: None,
                target_content_hash: &content_hash,
                repository_fingerprint: "tree-2",
                accepted_intent_hash: "intent-1",
            },
        ));
    }

    #[test]
    fn critical_repository_telemetry_requires_machine_decision_fields() {
        let complete = json!({
            "event_type": "worker.repository_operation_reduced",
            "node_id": "source-000",
            "operation": "modify_existing",
            "attempt_id": 2,
            "repair_intent_kind": "mutation_application_fallback",
            "repair_budget_owner": "source-000",
            "repository_fingerprint_before": "tree-1",
            "repository_fingerprint_after": "tree-2",
            "verification_evidence_id": "mutation-1",
            "node_status_before": "running",
            "node_status_after": "completed",
        });
        validate_critical_worker_event_fields(&complete).expect("complete telemetry contract");

        let incomplete = json!({
            "event_type": "worker.repository_operation_reduced",
            "node_id": "source-000",
        });
        let error = validate_critical_worker_event_fields(&incomplete)
            .expect_err("missing decision fields must be rejected");
        assert!(error.contains("attempt_id"));
        assert!(error.contains("verification_evidence_id"));
        assert!(error.contains("node_status_after"));
    }
}
