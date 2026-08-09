// Extracted from the hosted execution composition root.
use super::*;

pub(super) fn send_execution_telemetry(
    api: &HostedApiClient,
    execution_id: Uuid,
    started_at: &str,
    completed_at: Option<&str>,
    status: ExecutionStatus,
    revision: u32,
) {
    let occurred_at = completed_at.unwrap_or(started_at).to_owned();
    let event = TelemetryEvent {
        event_id: Uuid::new_v5(
            &HOSTED_NAMESPACE,
            format!("execution:{execution_id}:{revision}").as_bytes(),
        ),
        entity_revision: revision,
        occurred_at,
        event_type: if completed_at.is_some() {
            "execution.completed"
        } else {
            "execution.started"
        }
        .into(),
        payload: TelemetryPayload::Execution {
            execution: TelemetryExecutionSnapshot {
                id: execution_id,
                agent_id: None,
                agent_name: Some("rustgrid-agent-hosted".into()),
                role: Some("implementation".into()),
                started_at: started_at.to_owned(),
                completed_at: completed_at.map(str::to_owned),
                status,
            },
        },
    };
    if let Err(error) = api.telemetry(&TelemetryBatch {
        telemetry_version: TELEMETRY_VERSION.into(),
        events: vec![event],
    }) {
        eprintln!("[warning] hosted execution telemetry delivery failed: {error:#}");
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn send_quality_gate_phase_telemetry(
    api: &HostedApiClient,
    execution_id: Uuid,
    gate: &HostedQualityGate,
    workflow_run_attempt: i32,
    validation_round: u32,
    started_at: &str,
    completed_at: Option<&str>,
    status: ExecutionStatus,
    revision: u32,
) -> Result<()> {
    api.telemetry(&TelemetryBatch::new(vec![quality_gate_phase_event(
        execution_id,
        gate,
        workflow_run_attempt,
        validation_round,
        started_at,
        completed_at,
        status,
        revision,
    )]))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn quality_gate_phase_event(
    execution_id: Uuid,
    gate: &HostedQualityGate,
    workflow_run_attempt: i32,
    validation_round: u32,
    started_at: &str,
    completed_at: Option<&str>,
    status: ExecutionStatus,
    revision: u32,
) -> TelemetryEvent {
    let phase_id = Uuid::new_v5(
        &HOSTED_NAMESPACE,
        format!(
            "execution:{execution_id}:workflow-attempt:{workflow_run_attempt}:quality-gate:{validation_round}:{}",
            gate.id,
        )
        .as_bytes(),
    );
    let event_type = if completed_at.is_some() {
        "phase.completed"
    } else {
        "phase.started"
    };
    TelemetryEvent {
        event_id: Uuid::new_v5(
            &HOSTED_NAMESPACE,
            format!("phase:{phase_id}:revision:{revision}").as_bytes(),
        ),
        entity_revision: revision,
        occurred_at: completed_at.unwrap_or(started_at).to_owned(),
        event_type: event_type.into(),
        payload: TelemetryPayload::Phase {
            phase: PhaseSnapshot {
                id: phase_id,
                execution_id,
                name: format!("quality_gate:{}", gate.id),
                started_at: started_at.to_owned(),
                completed_at: completed_at.map(str::to_owned),
                status,
            },
        },
    }
}

pub(super) fn safe_failure(error: &anyhow::Error, cancelled: bool) -> (String, String) {
    if cancelled {
        return (
            "execution_cancelled".into(),
            "The hosted execution was cancelled or its mission lease was revoked.".into(),
        );
    }
    if let Some(failure) = error.downcast_ref::<HostedHttpError>() {
        let code = failure.effective_code().to_owned();
        if failure.failure_class() != AiFailureClass::Gateway {
            return (code, failure.terminal_message().to_owned());
        }
        return (
            code.clone(),
            format!(
                "RustGrid rejected a hosted execution operation with {}.",
                code
            ),
        );
    }
    if let Some(failure) = error.downcast_ref::<HostedAgentExecutionFailure>() {
        return (failure.code.clone(), failure.message.clone());
    }
    if let Some(failure) = error.downcast_ref::<HostedInvariantFailure>() {
        return (failure.code.into(), failure.message.clone());
    }
    if let Some(failure) =
        error.downcast_ref::<crate::hosted_orchestrator::OrchestrationInvariantError>()
    {
        return (failure.code.clone(), failure.message.clone());
    }
    if let Some(failure) = error.downcast_ref::<HostedStartupFailure>() {
        return (failure.code.into(), failure.message.clone());
    }
    if let Some(failure) = error.downcast_ref::<HostedProviderContractFailure>() {
        return (failure.code.clone(), failure.message.clone());
    }
    if error.downcast_ref::<ExecutionBudgetMismatch>().is_some() {
        return (
            "execution_budget_mismatch".into(),
            "The requested, resolved, and worker-received model-call budgets did not match.".into(),
        );
    }
    (
        "orchestration_execution_failed".into(),
        format!(
            "Hosted orchestration failed: {}",
            truncate_text(&error.to_string(), 2_000)
        ),
    )
}

pub(super) fn failure_diagnostics(error: &anyhow::Error, cancelled: bool) -> Value {
    if let Some(failure) = error.downcast_ref::<HostedAgentExecutionFailure>() {
        return serde_json::to_value(failure).unwrap_or_else(|_| {
            json!({
                "status": "failed",
                "category": failure.category,
                "code": failure.code,
                "phase": failure.phase,
                "message": failure.message,
            })
        });
    }
    if let Some(failure) = error.downcast_ref::<HostedInvariantFailure>() {
        return json!({
            "status": "failed",
            "category": failure.category(),
            "process_health": "failed",
            "mission_outcome": "failed",
            "code": failure.code,
            "phase": failure.phase,
            "message": failure.message,
            "resumable": failure.resumable,
            "recoverable": failure.resumable,
            "resume_phase": failure.phase,
            "resume_from_node": failure.resume_from_node,
            "recommended_action": "Resume from the next unresolved implementation node in the persisted execution graph.",
        });
    }
    if let Some(failure) =
        error.downcast_ref::<crate::hosted_orchestrator::OrchestrationInvariantError>()
    {
        return json!({
            "status": "failed",
            "category": "OrchestrationStateInvariantFailure",
            "process_health": "failed",
            "mission_outcome": "failed",
            "code": failure.code,
            "phase": "orchestration",
            "message": failure.message,
            "node_id": failure.node_id,
            "resumable": true,
            "recoverable": true,
            "resume_phase": "implementation",
            "recommended_action": "Resume from the persisted graph at the next unresolved node after correcting the rejected invariant.",
        });
    }
    if let Some(failure) = error.downcast_ref::<HostedStartupFailure>() {
        return json!({
            "status": "failed",
            "category": failure.category,
            "code": failure.code,
            "phase": "startup",
            "message": failure.message,
            "underlying_error": {
                "type": "worker_error",
                "message": truncate_text(&format!("{:#}", failure.underlying), 2_000),
                "stack_reference": null,
            },
            "provider_contacted": false,
            "model_calls_used": 0,
            "recoverable": true,
            "resume_phase": "startup",
            "recommended_action": "Retry from the persisted startup state after resolving the exact initialization error.",
        });
    }
    if let Some(failure) = error.downcast_ref::<HostedProviderContractFailure>() {
        return json!({
            "status": "failed",
            "category": "provider_protocol_failure",
            "code": failure.code,
            "phase": "request_validation",
            "message": failure.message,
            "underlying_error": {
                "type": "provider_contract_validation",
                "message": failure.message,
                "stack_reference": null,
            },
            "failure_stage": "request_validation",
            "provider_contacted": false,
            "reservation_state": "not_created",
            "call_budget_consumed": false,
            "actual_cost_micros": 0,
            "model_calls_used": 0,
            "model_calls_limit": 0,
            "model_calls_remaining": 0,
            "phase_calls_used": 0,
            "phase_calls_limit": 0,
            "last_successful_action": {},
            "usage": ToolUsage::default(),
            "recoverable": true,
            "resume_phase": "request_validation",
            "recommended_action":
                "Correct the exact reported provider tool, schema, or request path before dispatch.",
        });
    }
    if let Some(mismatch) = error.downcast_ref::<ExecutionBudgetMismatch>() {
        return json!({
            "status": "failed",
            "category": "manifest_policy_invalid",
            "code": "execution_budget_mismatch",
            "phase": "manifest_validation",
            "message":
                "The requested, resolved, and worker-received model-call budgets did not match.",
            "requested_model_call_budget": mismatch.requested,
            "resolved_model_call_budget": mismatch.resolved,
            "model_call_budget": mismatch.canonical,
            "persisted_execution_model_call_budget": mismatch.execution,
            "worker_received_model_call_budget": mismatch.worker_received,
            "model_calls_used": 0,
            "recoverable": true,
            "resume_phase": "manifest_validation",
            "recommended_action":
                "Correct budget propagation and dispatch a manifest with one unchanged canonical value.",
        });
    }
    let (code, message) = safe_failure(error, cancelled);
    let (underlying_type, underlying_message, stack_reference) =
        if let Some(http) = error.downcast_ref::<HostedHttpError>() {
            (
                "rustgrid_http_error",
                http.to_string(),
                http.request_id.clone(),
            )
        } else {
            (
                "worker_error",
                truncate_text(&format!("{error:#}"), 2_000),
                None,
            )
        };
    json!({
        "status": if cancelled { "cancelled" } else { "failed" },
        "category": hosted_failure_category(error),
        "code": code,
        "phase": ExecutionPhase::Implementation,
        "message": message,
        "underlying_error": {
            "type": underlying_type,
            "message": underlying_message,
            "stack_reference": stack_reference,
        },
        "model_calls_used": 0,
        "model_calls_limit": 0,
        "model_calls_remaining": 0,
        "phase_calls_used": 0,
        "phase_calls_limit": 0,
        "last_successful_action": {},
        "usage": ToolUsage::default(),
        "recoverable": !cancelled,
        "resume_phase": ExecutionPhase::Implementation,
        "recommended_action": if cancelled {
            "Start a new authorized execution if the ticket still requires work."
        } else {
            "Inspect the specific failure code and retry from the preserved execution state."
        },
    })
}

pub(super) fn unsuccessful_completion(
    cancelled: bool,
    failure_code: String,
    failure_message: String,
) -> CompletionRequest {
    CompletionRequest {
        status: if cancelled {
            "cancelled".into()
        } else {
            "failed".into()
        },
        canonical_terminal_result_id: None,
        terminal_revision: None,
        terminal_authority: None,
        canonical_terminal_result: None,
        mission_outcome: None,
        process_health: Some(if cancelled { "healthy" } else { "failed" }.into()),
        completion_evaluation: None,
        output_summary: None,
        failure_code: (!cancelled).then_some(failure_code),
        failure_message: (!cancelled).then_some(failure_message),
        head_branch: None,
        head_sha: None,
        pull_request_number: None,
        pull_request_url: None,
        final_callback: None,
    }
}

pub(super) fn hosted_pull_request_body(
    manifest: &HostedManifest,
    validation: &[ValidationResult],
    completeness: &CompletionEvaluation,
) -> String {
    let checks = validation
        .iter()
        .map(|result| {
            let icon = match result.status.as_str() {
                "passed" => "✅",
                "failed" | "failed_code" => "❌",
                _ => "⏳",
            };
            format!("- {} `{}`", icon, result.command)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let infrastructure_incomplete = validation
        .iter()
        .filter(|result| {
            matches!(
                result.status.as_str(),
                "timed_out" | "infrastructure_failed" | "pending" | "ready"
            )
        })
        .map(|result| {
            if result.status == "timed_out" {
                format!("- {} timed out: {}", result.command, result.output)
            } else if result.status == "infrastructure_failed" {
                format!("- {} could not complete: {}", result.command, result.output)
            } else {
                format!("- {} not yet run", result.command)
            }
        })
        .collect::<Vec<_>>();
    let infrastructure_notice = if infrastructure_incomplete.is_empty() {
        String::new()
    } else {
        format!(
            "Validation incomplete due to worker infrastructure:\n{}\n\nNo test failure was observed because the incomplete command did not produce an assertion result.\n\n",
            infrastructure_incomplete.join("\n")
        )
    };
    let code_failures = validation
        .iter()
        .filter(|result| matches!(result.status.as_str(), "failed" | "failed_code"))
        .map(|result| {
            let assertion_lines = result
                .output
                .lines()
                .filter(|line| {
                    let trimmed = line.trim();
                    trimmed.contains("AssertionError:")
                        || trimmed.starts_with("Expected:")
                        || trimmed.starts_with("Received:")
                        || trimmed.starts_with('❯')
                })
                .take(16)
                .map(str::trim)
                .collect::<Vec<_>>();
            format!(
                "- `{}`\n  {}",
                result.command,
                if assertion_lines.is_empty() {
                    truncate_text(&result.output, 1_000).replace('\n', "\n  ")
                } else {
                    assertion_lines.join("\n  ")
                }
            )
        })
        .collect::<Vec<_>>();
    let code_failure_notice = if code_failures.is_empty() {
        String::new()
    } else {
        format!(
            "Known validation failures:\n{}\n\n",
            code_failures.join("\n")
        )
    };
    let completeness_heading = match completeness.status {
        CompletionStatus::Complete => "Implementation completeness: **complete**",
        CompletionStatus::CompletePendingExternalReview => {
            "✅ **IMPLEMENTATION COMPLETE — external review remains**"
        }
        CompletionStatus::Blocked => "⛔ **BLOCKED — external technical input is required**",
        CompletionStatus::Partial | CompletionStatus::Incomplete | CompletionStatus::Uncertain => {
            "⚠️ **INCOMPLETE — continue implementation before review or merge**"
        }
    };
    let external_review_notice =
        if completeness.status == CompletionStatus::CompletePendingExternalReview {
            "Implementation complete.\nManual visual/product review remains.\n\n"
        } else {
            ""
        };
    let render_items = |items: &[String]| {
        if items.is_empty() {
            "- None.".into()
        } else {
            items
                .iter()
                .map(|work| format!("- {work}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    let criteria = completeness
        .criteria
        .iter()
        .map(|criterion| {
            let evidence = if criterion.evidence.is_empty() {
                "no repository evidence".into()
            } else {
                criterion
                    .evidence
                    .iter()
                    .map(|evidence| format!("`{}` — {}", evidence.path, evidence.description))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            format!(
                "- **{}** · `{}` · `{}` — {}",
                criterion.criterion_id,
                criterion.verification_type.as_str(),
                criterion.status.as_str(),
                evidence
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut review_items = completeness
        .review_checklist
        .iter()
        .map(|item| format!("- [ ] {}", item.description))
        .collect::<Vec<_>>();
    for pending in &completeness.pending_external_review {
        let item = format!("- [ ] {pending}");
        if !review_items.contains(&item) {
            review_items.push(item);
        }
    }
    let review_checklist = if review_items.is_empty() {
        "- None.".into()
    } else {
        review_items.join("\n")
    };
    let partial_summary = if requires_implementation_continuation(completeness.status) {
        let completed = completeness
            .criteria
            .iter()
            .flat_map(|criterion| &criterion.evidence)
            .map(|evidence| evidence.path.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|path| format!("- `{path}`"))
            .collect::<Vec<_>>();
        let root_cause = completeness
            .unrecovered_tool_failures
            .first()
            .cloned()
            .unwrap_or_else(|| "The planned-versus-changed path evidence is incomplete.".into());
        format!(
            "### Completed\n{}\n\n### Not completed\n{}\n\n### Root cause\n{}\n\n### Resume action\nNormalize the planned target set and resume implementation from the persisted notebook without repeating discovery, planning, or completed work.\n\n",
            if completed.is_empty() {
                "- No planned target has complete diff evidence yet.".into()
            } else {
                completed.join("\n")
            },
            render_items(&completeness.remaining_implementation_work),
            root_cause,
        )
    } else {
        String::new()
    };
    format!(
        "{}\n\n{}RustGrid ticket **{}** through the ephemeral GitHub Actions provider.\n\n\
Execution: `{}` (attempt {})\nModel: `{}`\nMaximum cost: `${}`\n\n\
Completion evaluator: `{}` at {:.0}% confidence\n\
Implementation: `{}` · verification: `{}` · source: `{}`\n\n{}\n\n\
Criterion evidence:\n{}\n\n\
Remaining implementation work:\n{}\n\n\
Remaining automated verification:\n{}\n\n\
External review checklist:\n{}\n\n\
Optional follow-up:\n{}\n\n{}{}{}Technical validation:\n{}\n\n\
_The OpenAI credential remained encrypted in RustGrid and was never sent to this runner._",
        completeness_heading,
        external_review_notice,
        manifest.ticket_key,
        manifest.execution.execution_id,
        manifest.execution.attempt_number,
        manifest.ai_gateway.model,
        manifest.ai_gateway.maximum_cost_usd,
        completeness.status.as_str(),
        completeness.confidence * 100.0,
        completeness.implementation_completeness.as_str(),
        completeness.verification_readiness.as_str(),
        completeness.evaluation_source.as_str(),
        completeness.summary,
        if criteria.is_empty() {
            "- No acceptance criteria were supplied.".into()
        } else {
            criteria
        },
        render_items(&completeness.remaining_implementation_work),
        render_items(&completeness.remaining_automated_verification),
        review_checklist,
        render_items(&completeness.optional_follow_up),
        partial_summary,
        code_failure_notice,
        infrastructure_notice,
        if checks.is_empty() {
            "- No required validation commands configured.".into()
        } else {
            checks
        }
    )
}

pub(super) fn sanitized_message_content(item: &Value) -> Vec<Value> {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(
            |content| match content.get("type").and_then(Value::as_str) {
                Some("output_text") => content.get("text").and_then(Value::as_str).map(
                    |text| json!({"type": "output_text", "text": truncate_text(text, 64 * 1024)}),
                ),
                Some("refusal") => content.get("refusal").and_then(Value::as_str).map(
                    |text| json!({"type": "refusal", "refusal": truncate_text(text, 64 * 1024)}),
                ),
                _ => None,
            },
        )
        .collect()
}

pub(super) fn cache_observability_payload(
    request: &Value,
    response: &Value,
    previous_prefix_sha256: Option<&str>,
    previous_tool_order_sha256: Option<&str>,
) -> (Value, String, String) {
    let tools = request.get("tools").cloned().unwrap_or_else(|| json!([]));
    let stable_prefix = json!({
        "model": request.get("model"),
        "instructions": request.get("instructions"),
        "tools": tools,
    });
    let encoded_prefix = serde_json::to_vec(&stable_prefix).unwrap_or_default();
    let prefix_sha256 = hex::encode(Sha256::digest(&encoded_prefix));
    let encoded_tools = serde_json::to_vec(&tools).unwrap_or_default();
    let tool_order_sha256 = hex::encode(Sha256::digest(&encoded_tools));
    let cached_tokens = response
        .pointer("/usage/input_tokens_details/cached_tokens")
        .or_else(|| response.pointer("/usage/cached_input_tokens"))
        .and_then(Value::as_u64);
    let invalidation_reason = if previous_prefix_sha256.is_none() {
        "cold_start"
    } else if previous_tool_order_sha256 != Some(tool_order_sha256.as_str()) {
        "tool_order_changed"
    } else if previous_prefix_sha256 != Some(prefix_sha256.as_str()) {
        "stable_prefix_changed"
    } else if cached_tokens == Some(0) {
        "provider_reported_zero_cache_read"
    } else {
        "none"
    };
    (
        json!({
            "event_type": "execution.ai.cache_observability",
            "stable_prefix_sha256": prefix_sha256,
            "cache_eligible_prefix_bytes": encoded_prefix.len(),
            "cache_read_tokens": cached_tokens,
            "cache_read": cached_tokens.is_some_and(|value| value > 0),
            "cache_invalidation_reason": invalidation_reason,
            "model_cache_support_reported": cached_tokens.is_some(),
            "gateway_forwarded_cache_fields":
                request.get("prompt_cache_key").is_some()
                    || request.get("cache_control").is_some(),
            "metadata_excluded_from_stable_prefix": true,
            "tool_order_sha256": tool_order_sha256,
        }),
        prefix_sha256,
        tool_order_sha256,
    )
}
