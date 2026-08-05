// Extracted from the hosted execution composition root.
use super::*;

#[derive(Debug)]
pub(super) struct HostedLeaseLost {
    pub(super) operation: &'static str,
    pub(super) detail: String,
}

impl std::fmt::Display for HostedLeaseLost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "hosted execution lease was lost during {}; stale terminal writes are suppressed: {}",
            self.operation, self.detail
        )
    }
}

impl std::error::Error for HostedLeaseLost {}

#[derive(Debug)]
pub(super) struct HostedInvariantFailure {
    pub(super) code: &'static str,
    pub(super) message: String,
}

impl HostedInvariantFailure {
    pub(super) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HostedInvariantFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for HostedInvariantFailure {}

pub(super) fn classify_hosted_execution_failure(
    error: &anyhow::Error,
) -> Option<crate::error::ExecutionFailure> {
    use crate::error::{
        AccessError, CancellationError, ControlPlaneError, ExecutionFailure, ExecutionFailureKind,
        InfrastructureError, ManifestError, ProviderError,
    };

    if let Some(http) = error.downcast_ref::<HostedHttpError>() {
        let context = http.to_string();
        let kind = match http.code.as_str() {
            "execution_cancelled" | "execution_completion_preempted_by_cancellation" => {
                ExecutionFailureKind::Cancellation(CancellationError::Requested)
            }
            "execution_lost" | "execution_lease_lost" => ExecutionFailureKind::LeaseLost {
                operation: http.path.clone(),
            },
            "execution_token_invalid" => {
                ExecutionFailureKind::Access(AccessError::AuthenticationRejected)
            }
            "execution_token_scope_invalid" | "execution_ai_access_revoked" => {
                ExecutionFailureKind::Access(AccessError::AuthorizationRejected)
            }
            _ => match http.failure_class() {
                AiFailureClass::RequestValidation | AiFailureClass::ProviderValidation => {
                    ExecutionFailureKind::Provider(ProviderError::Protocol)
                }
                AiFailureClass::ProviderAuthentication => {
                    ExecutionFailureKind::Access(AccessError::AuthorizationRejected)
                }
                AiFailureClass::ProviderRateLimit
                | AiFailureClass::ProviderServer
                | AiFailureClass::ProviderTimeout
                | AiFailureClass::ProviderDispatchUncertain => {
                    ExecutionFailureKind::ControlPlane(ControlPlaneError::Retryable {
                        operation: http.path.clone(),
                        status: Some(http.status.as_u16()),
                        request_id: http.request_id.clone(),
                    })
                }
                AiFailureClass::RegistrationConflict | AiFailureClass::Gateway
                    if http.retryable == Some(true)
                        || http.retryable_gateway_transport_failure() =>
                {
                    ExecutionFailureKind::ControlPlane(ControlPlaneError::Retryable {
                        operation: http.path.clone(),
                        status: Some(http.status.as_u16()),
                        request_id: http.request_id.clone(),
                    })
                }
                AiFailureClass::RegistrationConflict | AiFailureClass::Gateway => {
                    ExecutionFailureKind::ControlPlane(ControlPlaneError::Rejected {
                        operation: http.path.clone(),
                        status: Some(http.status.as_u16()),
                        request_id: http.request_id.clone(),
                    })
                }
            },
        };
        return Some(ExecutionFailure::with_safe_source(
            kind,
            context,
            "the hosted control-plane operation returned a structured rejection",
        ));
    }
    if let Some(lease) = error.downcast_ref::<HostedLeaseLost>() {
        return Some(ExecutionFailure::with_safe_source(
            ExecutionFailureKind::LeaseLost {
                operation: lease.operation.into(),
            },
            lease.to_string(),
            "hosted lease authority was invalidated",
        ));
    }
    if error.downcast_ref::<HostedInvariantFailure>().is_some()
        || error
            .downcast_ref::<crate::hosted_orchestrator::OrchestrationInvariantError>()
            .is_some()
        || error
            .downcast_ref::<crate::execution_graph::GraphInvariantError>()
            .is_some()
    {
        return Some(ExecutionFailure::with_safe_source(
            ExecutionFailureKind::Invariant,
            error.to_string(),
            "the hosted execution graph or lifecycle invariant was rejected",
        ));
    }
    if error
        .downcast_ref::<HostedProviderContractFailure>()
        .is_some()
    {
        return Some(ExecutionFailure::with_safe_source(
            ExecutionFailureKind::Provider(ProviderError::Protocol),
            error.to_string(),
            "the provider payload did not satisfy the hosted protocol contract",
        ));
    }
    if error.downcast_ref::<ExecutionBudgetMismatch>().is_some() {
        return Some(ExecutionFailure::with_safe_source(
            ExecutionFailureKind::Manifest(ManifestError::InvalidPolicy),
            error.to_string(),
            "the signed execution budget was inconsistent across manifest fields",
        ));
    }
    if let Some(failure) = error.downcast_ref::<HostedStartupFailure>() {
        return Some(ExecutionFailure::with_safe_source(
            ExecutionFailureKind::Infrastructure(InfrastructureError {
                component: "hosted startup".into(),
                retryable: true,
            }),
            failure.message.clone(),
            failure.underlying.to_string(),
        ));
    }
    if let Some(failure) = error.downcast_ref::<HostedAgentExecutionFailure>() {
        let kind = match failure.code.as_str() {
            "execution_ai_budget_exceeded" | "phase_model_call_budget_exhausted" => {
                ExecutionFailureKind::Provider(ProviderError::BudgetExhausted)
            }
            "invalid_model_artifact" | "impact_map_invalid" => {
                ExecutionFailureKind::Provider(ProviderError::InvalidArtifact)
            }
            _ => ExecutionFailureKind::Infrastructure(InfrastructureError {
                component: "hosted orchestration".into(),
                retryable: failure.recoverable,
            }),
        };
        return Some(ExecutionFailure::with_safe_source(
            kind,
            failure.message.clone(),
            failure.underlying_error.message.clone(),
        ));
    }
    None
}

pub(super) fn classify_mutation_application_exhausted(
    mut failure: HostedAgentExecutionFailure,
) -> HostedAgentExecutionFailure {
    failure.status = "blocked";
    failure.category = "MutationFailure";
    failure.process_health = "healthy";
    failure.mission_outcome = "blocked";
    failure.blocker = Some("mutation_application_exhausted".into());
    failure.phase = ExecutionPhase::Implementation;
    failure.resume_phase = ExecutionPhase::Implementation.as_str().into();
    failure
}

impl<'a> GatewayAgent<'a> {
    pub(super) fn has_unresolved_mutation_application_failure(&self) -> bool {
        self.notebook
            .orchestration
            .failures
            .unresolved()
            .any(|failure| {
                failure.category == crate::execution_graph::FailureCategory::MutationConflict
                    && failure.message.starts_with("mutation_application_failure:")
            })
    }

    pub(super) fn emit_guardrail(&self, code: &str, action: &str, message: &str) -> Result<()> {
        self.api.append_event(
            "progress",
            json!({
                "event_type": "worker.guardrail",
                "code": code,
                "phase": self.phases.active(),
                "action": action,
                "message": message,
                "budget": self.budget_telemetry(),
                "tool_usage": self.tool_usage,
            }),
        )
    }

    pub(super) fn emit_phase_budget_warning(&self) -> Result<()> {
        let phase = self.phases.active();
        self.api.append_event(
            "progress",
            json!({
                "event_type": "worker.phase_budget_warning",
                "phase": phase,
                "calls_used": self.phases.phase_calls(phase),
                "calls_limit": self.effective_phase_model_call_limit(),
                "total_calls_used": self.phases.total_calls(),
                "total_calls_limit": self.phases.total_limit(),
            }),
        )
    }

    pub(super) fn execution_failure(
        &self,
        code: &str,
        message: impl Into<String>,
        underlying: Option<&anyhow::Error>,
        recoverable: bool,
        recommended_action: &str,
    ) -> anyhow::Error {
        let category = underlying.map_or("orchestration_execution_failed", hosted_failure_category);
        self.categorized_execution_failure(
            category,
            code,
            message,
            underlying,
            recoverable,
            recommended_action,
        )
    }

    pub(super) fn categorized_execution_failure(
        &self,
        category: &'static str,
        code: &str,
        message: impl Into<String>,
        underlying: Option<&anyhow::Error>,
        recoverable: bool,
        recommended_action: &str,
    ) -> anyhow::Error {
        let phase = self.phases.active();
        let http = underlying.and_then(|error| error.downcast_ref::<HostedHttpError>());
        let (underlying_type, underlying_message, stack_reference) = if let Some(http) = http {
            (
                "rustgrid_http_error".to_owned(),
                http.to_string(),
                http.request_id.clone(),
            )
        } else if let Some(error) = underlying {
            (
                "worker_error".to_owned(),
                truncate_text(&error.to_string(), 2_000),
                None,
            )
        } else {
            ("orchestration_guardrail".to_owned(), code.to_owned(), None)
        };
        anyhow!(HostedAgentExecutionFailure {
            status: "failed",
            category,
            process_health: "failed",
            mission_outcome: "failed",
            blocker: None,
            resumable: recoverable,
            code: code.to_owned(),
            phase,
            message: message.into(),
            underlying_error: UnderlyingFailure {
                r#type: underlying_type,
                message: underlying_message,
                stack_reference,
            },
            model_calls_used: self.phases.total_calls(),
            model_calls_limit: self.phases.total_limit(),
            model_calls_remaining: self
                .phases
                .total_limit()
                .saturating_sub(self.phases.total_calls()),
            phase_calls_used: self.phases.phase_calls(phase),
            phase_calls_limit: self.effective_phase_model_call_limit(),
            last_successful_action: self.last_successful_action.clone(),
            usage: self.tool_usage.clone(),
            estimated_cost_micros: self.cost_guard.estimated_cost_micros,
            input_tokens: self.cost_guard.input_tokens,
            output_tokens: self.cost_guard.output_tokens,
            changed_paths: completion_changed_paths(self.repo, &self.manifest.github.base_sha)
                .unwrap_or_default(),
            remaining_work: self.notebook.remaining_work_v2.clone(),
            failed_tool_operations: self
                .notebook
                .tool_progress
                .iter()
                .filter(|record| record.class.is_failure())
                .cloned()
                .collect(),
            current_plan: self.notebook.planned_changes.clone(),
            validation_evidence: self.notebook.validation_evidence.clone(),
            notebook_revision: self.notebook.revision,
            recoverable,
            resume_phase: phase.as_str().into(),
            recommended_action: recommended_action.to_owned(),
            artifact: None,
            semantic_status: None,
            persistence_status: None,
            rustgrid_gateway_status: http.and_then(HostedHttpError::rustgrid_gateway_status),
            upstream_provider_status: http.and_then(|failure| failure.upstream_provider_status),
            failure_stage: http
                .and_then(HostedHttpError::failure_stage)
                .map(str::to_owned),
            provider_contacted: http.and_then(HostedHttpError::provider_contacted),
            call_budget_consumed: http.and_then(HostedHttpError::call_budget_consumed),
            reservation_state: http
                .and_then(HostedHttpError::reservation_state)
                .map(str::to_owned),
            reservation_reconciliation_state: http
                .and_then(HostedHttpError::reservation_reconciliation_state)
                .map(str::to_owned),
            rustgrid_request_id: http.and_then(|failure| failure.rustgrid_request_id.clone()),
            transport_request_id: http.and_then(|failure| failure.transport_request_id.clone()),
            provider_request_id: http.and_then(|failure| failure.provider_request_id.clone()),
            provider_error: http.and_then(|failure| failure.provider_error.clone()),
            provider_response_body: http.and_then(|failure| failure.provider_response_body.clone()),
            model_alias: http.and_then(|failure| failure.model_alias.clone()),
            resolved_provider_model: http
                .and_then(|failure| failure.resolved_provider_model.clone()),
            adapter_version: http.and_then(|failure| failure.adapter_version.clone()),
            payload_schema_version: http.and_then(|failure| failure.payload_schema_version.clone()),
            provider_attempts: http.and_then(|failure| failure.provider_attempts),
            actual_cost_micros: http.and_then(|failure| failure.actual_cost_micros),
        })
    }

    pub(super) fn implementation_preparation_failure(&self) -> anyhow::Error {
        let error = self.execution_failure(
            "implementation_preparation_failed",
            "Implementation could not begin after the bounded preparation allowance and guided single-target recovery.",
            None,
            true,
            "Resume in implementation at the current planned target using the persisted read failures and recovery data.",
        );
        let mut failure = error
            .downcast::<HostedAgentExecutionFailure>()
            .expect("execution_failure always returns HostedAgentExecutionFailure");
        failure =
            classify_implementation_preparation_failure(failure, &self.notebook.remaining_work_v2);
        anyhow!(failure)
    }

    pub(super) fn blocked_no_diff_failure(&self) -> anyhow::Error {
        let error = self.execution_failure(
            "blocked_no_diff",
            "The hosted mission produced no reviewable repository diff; graph state and remaining work were preserved.",
            None,
            true,
            "Resume from the next pending required graph node after resolving the recorded blocker.",
        );
        let mut failure = error
            .downcast::<HostedAgentExecutionFailure>()
            .expect("execution_failure always returns HostedAgentExecutionFailure");
        failure.status = "blocked";
        failure.category = "hosted_execution_blocked";
        failure.process_health = "healthy";
        failure.mission_outcome = "blocked";
        failure.blocker = Some("no_reviewable_diff".into());
        anyhow!(failure)
    }

    pub(super) fn mutation_application_exhausted_failure(&self) -> anyhow::Error {
        let error = self.execution_failure(
            "mutation_application_exhausted",
            "The target-bound mutation and its single bounded repair were rejected without changing repository content.",
            None,
            true,
            "Resume from the persisted target context after correcting the recorded mutation blocker.",
        );
        let failure = error
            .downcast::<HostedAgentExecutionFailure>()
            .expect("execution_failure always returns HostedAgentExecutionFailure");
        anyhow!(classify_mutation_application_exhausted(failure))
    }

    pub(super) fn infrastructure_stop_failure(&self, detail: &str) -> anyhow::Error {
        let error = self.execution_failure(
            "hosted_supervisor_infrastructure_failure",
            format!("Hosted execution supervision failed: {}", truncate_text(detail, 2_000)),
            None,
            true,
            "Resume from the persisted graph after worker connectivity and lease supervision recover.",
        );
        let mut failure = error
            .downcast::<HostedAgentExecutionFailure>()
            .expect("execution_failure always returns HostedAgentExecutionFailure");
        failure.category = "hosted_infrastructure_failure";
        failure.mission_outcome = "failed_infrastructure";
        failure.blocker = Some("worker_supervision".into());
        anyhow!(failure)
    }

    pub(super) fn impact_map_execution_failure(
        &self,
        code: &str,
        message: impl Into<String>,
        semantic_status: ArtifactSemanticStatus,
        persistence_status: ArtifactPersistenceStatus,
        recommended_action: &str,
    ) -> anyhow::Error {
        let phase = self.phases.active();
        anyhow!(HostedAgentExecutionFailure {
            status: "blocked",
            category: "artifact_blocked",
            process_health: "healthy",
            mission_outcome: "blocked",
            blocker: Some("impact_map_artifact_invalid".into()),
            resumable: true,
            code: code.to_owned(),
            phase,
            message: message.into(),
            underlying_error: UnderlyingFailure {
                r#type: "orchestration_guardrail".into(),
                message: code.to_owned(),
                stack_reference: None,
            },
            model_calls_used: self.phases.total_calls(),
            model_calls_limit: self.phases.total_limit(),
            model_calls_remaining: self
                .phases
                .total_limit()
                .saturating_sub(self.phases.total_calls()),
            phase_calls_used: self.phases.phase_calls(phase),
            phase_calls_limit: self.effective_phase_model_call_limit(),
            last_successful_action: self.last_successful_action.clone(),
            usage: self.tool_usage.clone(),
            estimated_cost_micros: self.cost_guard.estimated_cost_micros,
            input_tokens: self.cost_guard.input_tokens,
            output_tokens: self.cost_guard.output_tokens,
            changed_paths: completion_changed_paths(self.repo, &self.manifest.github.base_sha)
                .unwrap_or_default(),
            remaining_work: self.notebook.remaining_work_v2.clone(),
            failed_tool_operations: self
                .notebook
                .tool_progress
                .iter()
                .filter(|record| record.class.is_failure())
                .cloned()
                .collect(),
            current_plan: self.notebook.planned_changes.clone(),
            validation_evidence: self.notebook.validation_evidence.clone(),
            notebook_revision: self.notebook.revision,
            recoverable: true,
            resume_phase: "artifact_repair".into(),
            recommended_action: recommended_action.to_owned(),
            artifact: Some("impact_map".into()),
            semantic_status: Some(semantic_status),
            persistence_status: Some(persistence_status),
            rustgrid_gateway_status: None,
            upstream_provider_status: None,
            failure_stage: None,
            provider_contacted: None,
            call_budget_consumed: None,
            reservation_state: None,
            reservation_reconciliation_state: None,
            rustgrid_request_id: None,
            transport_request_id: None,
            provider_request_id: None,
            provider_error: None,
            provider_response_body: None,
            model_alias: None,
            resolved_provider_model: None,
            adapter_version: None,
            payload_schema_version: None,
            provider_attempts: None,
            actual_cost_micros: None,
        })
    }

    pub(super) fn emit_mutation_no_progress_diagnostics(&mut self) -> Result<()> {
        let active = self
            .notebook
            .orchestration
            .graph
            .as_ref()
            .and_then(crate::execution_graph::ExecutionGraph::active_node)
            .or_else(|| {
                self.notebook
                    .orchestration
                    .graph
                    .as_ref()
                    .and_then(crate::execution_graph::ExecutionGraph::next_runnable_node)
            });
        let node_id = active.map(|node| node.id.clone());
        let target = active.and_then(|node| node.target.as_ref().map(|target| target.path.clone()));
        let calls = node_id.as_ref().map_or(0, |node_id| {
            self.notebook
                .orchestration
                .budget
                .usage_for(node_id)
                .model_calls_consumed
        });
        let read_paths = self
            .notebook
            .tool_progress
            .iter()
            .filter(|progress| {
                matches!(
                    progress.phase,
                    ExecutionPhase::Implementation | ExecutionPhase::Repair
                ) && matches!(
                    progress.tool.as_str(),
                    "read_file" | "read_files" | "search_text" | "related_tests"
                )
            })
            .filter_map(|progress| progress.target.clone())
            .collect::<Vec<_>>();
        let duplicate_target_reads = target.as_ref().map_or(0, |target| {
            read_paths
                .iter()
                .filter(|path| *path == target)
                .count()
                .saturating_sub(1)
        });
        let cross_target_reads = target.as_ref().map_or(0, |target| {
            read_paths.iter().filter(|path| *path != target).count()
        });
        self.api.append_event(
            "progress",
            json!({
                "event_type": "worker.mutation_no_progress_diagnostics",
                "active_node_id": node_id,
                "active_target": target,
                "expected_action": "MutateTarget",
                "calls_consumed_by_action_kind": {"mutate_or_repair": calls},
                "read_paths_requested": read_paths,
                "repository_reads_requested": read_paths.len(),
                "cross_target_reads_requested": cross_target_reads,
                "duplicate_cache_eligible_reads": duplicate_target_reads,
                "mutation_tools_offered": ["apply_patch", "replace_file"],
                "mutation_tools_invoked": self.notebook.write_attempts.len(),
                "reason_no_mutation_was_produced": "no attributable target mutation or typed blocker was recorded before the no-progress boundary",
            }),
        )
    }
}
