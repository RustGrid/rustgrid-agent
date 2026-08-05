//! Ephemeral GitHub Actions execution.
//!
//! This module is intentionally separate from the persistent worker client. It
//! never loads an [`AppContext`](crate::config::AppContext), a keyring entry, or
//! Codex/ChatGPT authentication. GitHub OIDC is exchanged once for a
//! short-lived, mission-scoped execution token; that token remains in this
//! process and is stripped from every repository subprocess.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    command,
    config::{DEFAULT_INSTANCE_URL, RepoConfig},
    execution_graph::{
        MutationApplicationFailure, MutationDiagnostics, MutationFallbackPolicy,
        MutationStrategyFingerprint, MutationToolPolicyViolation, RejectedMutation,
        RepairRequestPreflight, TargetAttemptAccounting,
    },
    git::{RemoteBranchMoved, Repo, read_repo_instructions},
    github::{GitHubClient, PullRequest},
    hosted_orchestrator::{
        ExecutionDecision, MissionOutcome as OrchestratedMissionOutcome, classify_mutation_request,
        reconcile_execution,
    },
    shutdown,
    telemetry::{
        ExecutionSnapshot as TelemetryExecutionSnapshot, ExecutionStatus,
        HostedOrchestrationTelemetry, PhaseSnapshot, TELEMETRY_VERSION, TelemetryBatch,
        TelemetryEvent, TelemetryPayload, now_rfc3339,
    },
    token::parse_rfc3339_utc,
};

mod graph_bridge;
mod impact_map;
mod lifecycle;
mod orchestration;

mod authentication;
mod contracts;
mod control_plane;
mod environment;
mod errors;
mod execution;
mod lifecycle_state;
mod model_session;
mod provider;
mod provider_protocol;
mod publication;
mod recovery;
mod telemetry;
mod terminal;
mod tools;

use authentication::*;
use contracts::*;
use control_plane::*;
use environment::*;
use errors::*;
use execution::*;
use lifecycle_state::*;
use model_session::*;
use provider::*;
use provider_protocol::*;
use publication::*;
use recovery::*;
use telemetry::*;
use terminal::*;
use tools::*;

use graph_bridge::{
    HostedOrchestrationCheckpoint, HostedReconciliationFacts, HostedResumeReason,
    mission_outcome_from_completion,
};
use impact_map::{
    ArtifactSource, IMPACT_MAP_SCHEMA_VERSION, ImpactArea, ImpactMap, InvalidPayloadShape,
    ValidationError,
};

use lifecycle::{
    ImplementationCompletionStatus, ImplementationProgressAction, ImplementationSubstate,
    RemainingWorkItem, RequiredGate, ToolProgressClass, ValidationEntryDecision,
    ValidationEvidence, ValidationGateType, ValidationSource, ValidationStatus,
    canonical_running_state, derive_remaining_work, implementation_completion_status,
    implementation_progress_action, legacy_remaining_work, new_running_evidence, passed_evidence,
    supersede_stale_validation, validate_lifecycle_invariants, validation_entry_decision,
    validation_fingerprint,
};
#[cfg(test)]
use orchestration::phase_budget_allocation;
use orchestration::{
    DEFAULT_HOSTED_MODEL_CALLS, ExecutionPhase, MINIMUM_HOSTED_MODEL_CALLS, PhaseLedger,
    SearchGuard, SearchSignature,
};

const EXECUTION_LEASE_SECONDS: i64 = 900;
const EXECUTION_TOKEN_TTL_SECONDS: i64 = 900;
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(180);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const MAX_HTTP_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_HTTP_ERROR_BYTES: usize = 128 * 1024;
const MAX_PROVIDER_ERROR_MESSAGE_BYTES: usize = 8 * 1024;
const MAX_PROVIDER_ERROR_PARAMETER_BYTES: usize = 512;
const MAX_PROVIDER_RESPONSE_BODY_BYTES: usize = 48 * 1024;
const MAX_TOOL_OUTPUT_BYTES: usize = 48 * 1024;
const MAX_DISCOVERY_REQUEST_BYTES: usize = 48 * 1024;
const MAX_MODEL_FILE_BYTES: usize = 512 * 1024;
// The backend remains authoritative: the worker only accepts and enforces the
// signed mission budget. This ceiling must accommodate repository-wide hosted
// work instead of silently imposing the old 40/64-call product policy.
const MAX_MODEL_CALLS_HARD_LIMIT: usize = 100;
const MAX_HOSTED_TURN_WINDOWS: usize = 3;
const MAX_REPAIR_ATTEMPTS: usize = 2;
const MAX_HOSTED_EXECUTION_DURATION: Duration = Duration::from_secs(75 * 60);
const MAX_AI_REGISTRATION_ATTEMPTS: usize = 3;
const MAX_SMALL_FILE_REWRITE_BYTES: usize = 64 * 1024;
const MAX_AMBIGUOUS_REPLACEMENT_FAILURES: usize = 2;
const MAX_TARGET_REPAIR_FAILURES: usize = 4;
const HOSTED_NAMESPACE: Uuid = Uuid::from_u128(0xc4e820c0_9ee5_4d13_87d0_05582a548e76);
const EXECUTION_PERMISSIONS: [&str; 7] = [
    "ai:invoke",
    "artifacts:write",
    "events:append",
    "execution:complete",
    "mission:claim",
    "mission:heartbeat",
    "mission:read",
];

pub fn execute_github_actions(execution_id: Uuid) -> crate::error::ExecutionResult<()> {
    execute_github_actions_impl(execution_id).map_err(|error| {
        classify_hosted_execution_failure(&error)
            .unwrap_or_else(|| crate::error::ExecutionFailure::from_anyhow(error))
    })
}

fn execute_github_actions_impl(execution_id: Uuid) -> Result<()> {
    let environment = GithubActionsEnvironment::load(execution_id)?;
    environment.require_execute_context()?;
    let git_author = environment.git_author()?;
    harden_hosted_process()?;
    let http = hosted_http_client()?;
    let oidc_token = request_github_oidc(&http, &environment)?;
    let exchange = exchange_github_oidc(&http, &environment, execution_id, &oidc_token)?;
    let api = HostedApiClient::from_exchange(
        http,
        environment.api_root.clone(),
        execution_id,
        exchange,
        Arc::new(SystemHostedClock),
    )?;
    println!("[starting] Authenticated ephemeral GitHub Actions execution {execution_id}");

    let preparation = (|| {
        api.claim()
            .context("could not claim the hosted execution")?;
        let manifest = api
            .manifest()
            .context("could not retrieve the hosted execution manifest")?;
        manifest.validate(execution_id, &environment, &api)?;
        api.append_event(
            "progress",
            json!({
                "step": "authenticated",
                "status": "completed",
                "provider": "github_actions",
                "execution_id": execution_id
            }),
        )?;
        Ok::<HostedManifest, anyhow::Error>(manifest)
    })();
    let manifest = match preparation {
        Ok(manifest) => manifest,
        Err(error) => {
            let (code, message) = safe_failure(&error, false);
            let diagnostics = failure_diagnostics(&error, false);
            let _ = api.append_event(
                "result",
                json!({
                    "status": "failed",
                    "code": code,
                    "failure": diagnostics,
                }),
            );
            let _ = api.complete(&CompletionRequest {
                status: "failed".into(),
                canonical_terminal_result_id: None,
                terminal_revision: None,
                terminal_authority: None,
                canonical_terminal_result: None,
                mission_outcome: None,
                process_health: Some("failed".into()),
                completion_evaluation: None,
                output_summary: None,
                failure_code: Some(code),
                failure_message: Some(message),
                head_branch: None,
                head_sha: None,
                pull_request_number: None,
                pull_request_url: None,
                final_callback: None,
            });
            return Err(error);
        }
    };

    if recover_persisted_terminal_callback(&api, &manifest)? {
        return Ok(());
    }

    let running = Arc::new(AtomicBool::new(true));
    let stop_reason = Arc::new(Mutex::new(None));
    let supervisor =
        HostedSupervisor::start(api.clone(), Arc::clone(&running), Arc::clone(&stop_reason));
    let started_at = now_rfc3339();
    send_execution_telemetry(
        &api,
        execution_id,
        &started_at,
        None,
        ExecutionStatus::Running,
        1,
    );

    let result = run_hosted_execution(&api, &manifest, &git_author, &running, &stop_reason);
    supervisor.stop();
    let terminal_at = now_rfc3339();
    match result {
        Ok(result) => report_hosted_result(&api, execution_id, &started_at, &terminal_at, &result),
        Err(error) if !may_publish_hosted_terminal_state(&error) => {
            eprintln!("[warning] {error}; skipped stale hosted terminal updates");
            Err(error)
        }
        Err(error)
            if error
                .downcast_ref::<HostedAgentExecutionFailure>()
                .is_some_and(|failure| failure.mission_outcome == "blocked") =>
        {
            let failure = error
                .downcast_ref::<HostedAgentExecutionFailure>()
                .expect("guard checked structured blocked outcome");
            send_execution_telemetry(
                &api,
                execution_id,
                &started_at,
                Some(&terminal_at),
                ExecutionStatus::NeedsContinuation,
                2,
            );
            let diagnostics = failure_diagnostics(&error, false);
            let completion_evaluation = blocked_completion_evaluation(failure);
            let terminal = resolve_blocked_terminal_result(
                execution_id,
                &failure.code,
                failure.remaining_work.clone(),
                &failure.resume_phase,
                completion_evaluation.clone(),
                &terminal_at,
            );
            api.append_event(
                "progress",
                json!({
                    "event_type": "worker.canonical_terminal_result_resolved",
                    "canonical_terminal_result_id": terminal.terminal_result_id,
                    "mission_outcome": terminal.mission_outcome,
                    "process_health": terminal.process_health,
                    "domain_execution_status": terminal.execution_status,
                    "publication_status": terminal.publication.status,
                    "authority": terminal.finality.authority,
                }),
            )?;
            let mut blocked_payload = blocked_result_event_payload(failure, diagnostics);
            if let Some(payload) = blocked_payload.as_object_mut() {
                payload.insert(
                    "event_type".into(),
                    json!("worker.canonical_terminal_result_persisted"),
                );
                payload.insert(
                    "canonical_terminal_result_id".into(),
                    json!(terminal.terminal_result_id),
                );
                payload.insert("canonical_terminal_result".into(), json!(terminal));
                payload.insert("mission_outcome".into(), json!(terminal.mission_outcome));
                payload.insert("process_health".into(), json!(terminal.process_health));
                payload.insert("terminal_finality".into(), json!(terminal.finality));
                payload.insert(
                    "resumable".into(),
                    json!(terminal.resumability.is_resumable()),
                );
                payload.insert("ui_status".into(), json!(terminal.ui_status()));
            }
            api.append_event("result", blocked_payload)
                .context("could not persist canonical blocked terminal result")?;
            let completion = CompletionRequest {
                status: terminal.completion_request_status().into(),
                canonical_terminal_result_id: Some(terminal.terminal_result_id),
                terminal_revision: Some(terminal.finality.terminal_revision),
                terminal_authority: Some("worker_domain".into()),
                canonical_terminal_result: Some(serde_json::to_value(&terminal)?),
                mission_outcome: Some(terminal.compatibility_completion_status()),
                process_health: Some(terminal.process_health.as_str().into()),
                completion_evaluation: Some(terminal.completion.clone()),
                output_summary: Some(failure.message.clone()),
                failure_code: Some(failure.code.clone()),
                failure_message: Some(failure.message.clone()),
                head_branch: Some(manifest.github.branch.clone()),
                head_sha: None,
                pull_request_number: None,
                pull_request_url: None,
                final_callback: None,
            };
            if matches!(
                deliver_terminal_callback(
                    &api,
                    &terminal,
                    &completion,
                    failure.notebook_revision,
                    &terminal_at,
                )?,
                TerminalCallbackDelivery::Pending { .. }
            ) {
                eprintln!(
                    "[warning] blocked execution {execution_id} was persisted, but its terminal callback remains pending"
                );
            }
            let _ = api.append_event(
                "progress",
                json!({
                    "event_type": "worker.process_exit_code_resolved",
                    "canonical_terminal_result_id": terminal.terminal_result_id,
                    "exit_code": terminal.process_exit_code(),
                    "mission_outcome": terminal.mission_outcome,
                    "process_health": terminal.process_health,
                    "reason_code": terminal.reason_code,
                    "authority": terminal.finality.authority,
                }),
            );
            println!(
                "[blocked] Execution {execution_id} preserved implementation state without running validation or creating a pull request"
            );
            Ok(())
        }
        Err(error) => {
            let infrastructure_failure = error
                .downcast_ref::<HostedAgentExecutionFailure>()
                .is_some_and(|failure| failure.mission_outcome == "failed_infrastructure");
            let cancelled = (!running.load(Ordering::SeqCst) || shutdown::requested())
                && !infrastructure_failure;
            send_execution_telemetry(
                &api,
                execution_id,
                &started_at,
                Some(&terminal_at),
                if cancelled {
                    ExecutionStatus::Cancelled
                } else {
                    ExecutionStatus::Failed
                },
                2,
            );
            let (code, message) = safe_failure(&error, cancelled);
            let diagnostics = failure_diagnostics(&error, cancelled);
            let terminal = resolve_unsuccessful_terminal_result(
                execution_id,
                cancelled,
                &code,
                &message,
                &terminal_at,
            );
            api.append_event(
                "progress",
                json!({
                    "event_type": "worker.canonical_terminal_result_resolved",
                    "canonical_terminal_result_id": terminal.terminal_result_id,
                    "mission_outcome": terminal.mission_outcome,
                    "process_health": terminal.process_health,
                    "domain_execution_status": terminal.execution_status,
                    "publication_status": terminal.publication.status,
                    "authority": terminal.finality.authority,
                }),
            )?;
            api.append_event(
                "result",
                json!({
                    "event_type": "worker.canonical_terminal_result_persisted",
                    "canonical_terminal_result_id": terminal.terminal_result_id,
                    "canonical_terminal_result": terminal,
                    "status": terminal.completion_request_status(),
                    "mission_outcome": terminal.mission_outcome,
                    "process_health": terminal.process_health,
                    "code": code,
                    "failure": diagnostics,
                    "terminal_finality": terminal.finality,
                }),
            )
            .context("could not persist unsuccessful canonical hosted terminal result")?;
            let mut completion = unsuccessful_completion(cancelled, code, message);
            completion.canonical_terminal_result_id = Some(terminal.terminal_result_id);
            completion.terminal_revision = Some(terminal.finality.terminal_revision);
            completion.terminal_authority = Some("worker_domain".into());
            completion.canonical_terminal_result = Some(serde_json::to_value(&terminal)?);
            completion.mission_outcome = Some(terminal.compatibility_completion_status());
            let final_notebook_revision = error
                .downcast_ref::<HostedAgentExecutionFailure>()
                .map_or(0, |failure| failure.notebook_revision);
            if matches!(
                deliver_terminal_callback(
                    &api,
                    &terminal,
                    &completion,
                    final_notebook_revision,
                    &terminal_at,
                )?,
                TerminalCallbackDelivery::Pending { .. }
            ) {
                eprintln!(
                    "[warning] execution {execution_id} was persisted, but its terminal callback remains pending"
                );
            }
            let _ = api.append_event(
                "progress",
                json!({
                    "event_type": "worker.process_exit_code_resolved",
                    "canonical_terminal_result_id": terminal.terminal_result_id,
                    "exit_code": terminal.process_exit_code(),
                    "mission_outcome": terminal.mission_outcome,
                    "process_health": terminal.process_health,
                    "reason_code": terminal.reason_code,
                    "authority": terminal.finality.authority,
                }),
            );
            if cancelled {
                println!(
                    "[cancelled] Execution {execution_id} preserved its checkpoint and ended normally"
                );
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

fn report_hosted_result(
    api: &HostedApiClient,
    execution_id: Uuid,
    started_at: &str,
    terminal_at: &str,
    result: &HostedResult,
) -> Result<()> {
    let terminal = resolve_published_terminal_result(execution_id, result, terminal_at);
    let telemetry_status = match terminal.execution_status {
        DomainExecutionStatus::Completed | DomainExecutionStatus::AwaitingExternalReview => {
            ExecutionStatus::Succeeded
        }
        DomainExecutionStatus::NeedsContinuation | DomainExecutionStatus::Blocked => {
            ExecutionStatus::NeedsContinuation
        }
        DomainExecutionStatus::Cancelled => ExecutionStatus::Cancelled,
        DomainExecutionStatus::Failed => ExecutionStatus::Failed,
    };
    send_execution_telemetry(
        api,
        execution_id,
        started_at,
        Some(terminal_at),
        telemetry_status,
        2,
    );
    api.append_event(
        "progress",
        json!({
            "event_type": "worker.canonical_terminal_result_resolved",
            "canonical_terminal_result_id": terminal.terminal_result_id,
            "mission_outcome": terminal.mission_outcome,
            "process_health": terminal.process_health,
            "domain_execution_status": terminal.execution_status,
            "publication_status": terminal.publication.status,
            "authority": terminal.finality.authority,
        }),
    )?;
    if result.completeness.status != terminal.completion.status {
        api.append_event(
            "progress",
            json!({
                "event_type": "worker.terminal_outcome_conflict_detected",
                "canonical_terminal_result_id": terminal.terminal_result_id,
                "completion_model_outcome": result.completeness.status,
                "canonical_completion_outcome": terminal.completion.status,
                "mission_outcome": terminal.mission_outcome,
                "publication_status": terminal.publication.status,
                "resolution": "canonical_deterministic_evidence_preserved",
                "authority": terminal.finality.authority,
            }),
        )?;
    }
    api.append_event(
        "result",
        json!({
            "event_type": "worker.canonical_terminal_result_persisted",
            "canonical_terminal_result_id": terminal.terminal_result_id,
            "canonical_terminal_result": terminal,
            // Compatibility fields are projections of the same canonical
            // value rather than an independently classified outcome.
            "status": terminal.completion_request_status(),
            "mission_outcome": terminal.mission_outcome,
            "process_health": terminal.process_health,
            "branch": terminal.publication.branch,
            "head_sha": terminal.publication.commit_sha,
            "pull_request_number": terminal.publication.pull_request_number,
            "pull_request_url": terminal.publication.pull_request_url,
            "implementation_completeness": terminal.completion,
            "remaining_work": terminal.remaining_work,
            "resumability": terminal.resumability,
            "resumable": terminal.resumability.is_resumable(),
            "terminal_finality": terminal.finality,
            "ui_status": terminal.ui_status(),
            "technical_validation": result.validation,
            "terminal_telemetry": result.terminal_telemetry
        }),
    )
    .context("could not persist canonical hosted terminal result")?;
    let completion = CompletionRequest {
        status: terminal.completion_request_status().into(),
        canonical_terminal_result_id: Some(terminal.terminal_result_id),
        terminal_revision: Some(terminal.finality.terminal_revision),
        terminal_authority: Some("worker_domain".into()),
        canonical_terminal_result: Some(serde_json::to_value(&terminal)?),
        mission_outcome: Some(terminal.compatibility_completion_status()),
        process_health: Some(terminal.process_health.as_str().into()),
        completion_evaluation: Some(terminal.completion.clone()),
        output_summary: Some(truncate_text(&result.summary, 16_000)),
        failure_code: None,
        failure_message: None,
        head_branch: terminal.publication.branch.clone(),
        head_sha: terminal.publication.commit_sha.clone(),
        pull_request_number: terminal
            .publication
            .pull_request_number
            .map(i64::try_from)
            .transpose()
            .context("pull request number is too large")?,
        pull_request_url: terminal.publication.pull_request_url.clone(),
        final_callback: None,
    };
    if matches!(
        deliver_terminal_callback(
            api,
            &terminal,
            &completion,
            result.terminal_telemetry.notebook_revision,
            terminal_at,
        )?,
        TerminalCallbackDelivery::Pending { .. }
    ) {
        // RunFinished is the canonical terminal result. A best-effort API
        // callback cannot reverse that result or trigger the emergency failure
        // path merely because delivery was temporarily unavailable.
        eprintln!(
            "[warning] hosted execution {execution_id} finished successfully, but the terminal callback remains pending"
        );
        let mut observed = terminal.clone();
        record_noncritical_post_publication_failure(
            &mut observed,
            "terminal_callback_transport_failed",
        );
        let _ = api.append_event(
            "progress",
            json!({
                "event_type": "worker.infrastructure_terminal_did_not_override_domain",
                "canonical_terminal_result_id": terminal.terminal_result_id,
                "mission_outcome": terminal.mission_outcome,
                "canonical_process_health": terminal.process_health,
                "observed_process_health": observed.process_health,
                "reconciliation_decision": InfrastructureReconciliationDecision::DomainResultPreserved,
                "anomaly_code": "terminal_callback_transport_failed",
                "authority": terminal.finality.authority,
            }),
        );
    } else {
        println!(
            "[complete] Execution {execution_id} opened or reused pull request #{} at {}",
            result.pull_request.number, result.pull_request.url
        );
    }
    if let Err(error) = api.append_event(
        "progress",
        json!({
            "event_type": "worker.process_exit_code_resolved",
            "canonical_terminal_result_id": terminal.terminal_result_id,
            "exit_code": terminal.process_exit_code(),
            "mission_outcome": terminal.mission_outcome,
            "process_health": terminal.process_health,
            "reason_code": terminal.reason_code,
            "authority": terminal.finality.authority,
        }),
    ) {
        eprintln!("[warning] process-exit telemetry delivery failed: {error:#}");
    }
    Ok(())
}

const fn terminal_reason_code(status: CompletionStatus) -> &'static str {
    match status {
        CompletionStatus::Complete => "completed",
        CompletionStatus::CompletePendingExternalReview => "external_review_required",
        CompletionStatus::Partial => "partial_reviewable",
        CompletionStatus::Blocked => "blocked_resumable",
        CompletionStatus::Incomplete => "incomplete",
        CompletionStatus::Uncertain => "uncertain",
    }
}

const fn requires_implementation_continuation(status: CompletionStatus) -> bool {
    matches!(
        status,
        CompletionStatus::Partial
            | CompletionStatus::Incomplete
            | CompletionStatus::Uncertain
            | CompletionStatus::Blocked
    )
}

pub fn report_emergency_failure(execution_id: Uuid) -> Result<()> {
    let environment = GithubActionsEnvironment::load(execution_id)?;
    harden_hosted_process()?;
    let http = hosted_http_client()?;
    let oidc_token = request_github_oidc(&http, &environment)?;
    let exchange = exchange_github_oidc(&http, &environment, execution_id, &oidc_token)?;
    let api = HostedApiClient::from_exchange(
        http,
        environment.api_root,
        execution_id,
        exchange,
        Arc::new(SystemHostedClock),
    )?;
    report_emergency_failure_with_api(&api, execution_id)
}

fn report_emergency_failure_with_api(api: &HostedApiClient, execution_id: Uuid) -> Result<()> {
    if let Err(error) = api.claim() {
        if error
            .downcast_ref::<HostedHttpError>()
            .is_some_and(HostedHttpError::invalidates_execution)
        {
            println!(
                "[ignored] Emergency failure callback found execution {execution_id} already terminal or invalidated"
            );
            return Ok(());
        }
        return Err(error).context(
            "could not confirm ownership before reporting an emergency hosted execution failure",
        );
    }
    let observed_at = now_rfc3339();
    let infrastructure = InfrastructureTerminalMetadata {
        provider: "github_actions".into(),
        workflow_run_id: env::var("GITHUB_RUN_ID").ok(),
        workflow_job_id: env::var("GITHUB_JOB").ok(),
        workflow_status: "completed".into(),
        workflow_conclusion: Some("failure".into()),
        runner_name: env::var("RUNNER_NAME").ok(),
        observed_at: observed_at.clone(),
    };
    let reconciliation = reconcile_infrastructure_terminal(None, infrastructure);
    api.append_event(
        "progress",
        json!({
            "event_type": "worker.infrastructure_terminal_observed",
            "workflow_conclusion": reconciliation.infrastructure.workflow_conclusion,
            "workflow_run_id": reconciliation.infrastructure.workflow_run_id,
            "workflow_job_id": reconciliation.infrastructure.workflow_job_id,
            "observed_at": reconciliation.infrastructure.observed_at,
        }),
    )?;
    api.append_event(
        "progress",
        json!({
            "event_type": "worker.infrastructure_terminal_reconciled",
            "reconciliation_decision": reconciliation.decision,
            "anomaly_code": reconciliation.anomaly_code,
            "authority": TerminalAuthority::InfrastructureFallback,
        }),
    )?;
    let terminal = resolve_infrastructure_fallback_terminal_result(
        execution_id,
        "github_actions_step_failed",
        "The GitHub Actions job failed before the normal execution callback completed.",
        &observed_at,
    );
    api.append_event(
        "result",
        json!({
            "event_type": "worker.canonical_terminal_result_persisted",
            "canonical_terminal_result_id": terminal.terminal_result_id,
            "canonical_terminal_result": terminal,
            "status": "failed",
            "code": "github_actions_step_failed",
            "emergency_callback": true,
            "authority": TerminalAuthority::InfrastructureFallback,
        }),
    )?;
    let completion = CompletionRequest {
        status: "failed".into(),
        canonical_terminal_result_id: Some(terminal.terminal_result_id),
        terminal_revision: Some(terminal.finality.terminal_revision),
        terminal_authority: Some("infrastructure_fallback".into()),
        canonical_terminal_result: Some(serde_json::to_value(&terminal)?),
        mission_outcome: None,
        process_health: Some("failed".into()),
        completion_evaluation: None,
        output_summary: None,
        failure_code: Some("github_actions_step_failed".into()),
        failure_message: Some(
            "The GitHub Actions job failed before the normal execution callback completed.".into(),
        ),
        head_branch: None,
        head_sha: None,
        pull_request_number: None,
        pull_request_url: None,
        final_callback: None,
    };
    if let Err(error) = api.complete(&completion) {
        if error
            .downcast_ref::<HostedHttpError>()
            .is_some_and(HostedHttpError::invalidates_execution)
        {
            println!(
                "[ignored] Emergency failure callback lost terminal ownership for execution {execution_id}"
            );
            return Ok(());
        }
        return Err(error).context("could not report emergency hosted execution failure");
    }
    println!("[complete] Reported emergency failure for execution {execution_id}");
    Ok(())
}

struct HostedSupervisor {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

#[derive(Clone, Debug)]
enum HostedLeaseFailure {
    Temporary(String),
    Invalidated(String),
    Cancelled,
}

trait HostedLeaseControlPlane: Clone + Send + 'static {
    fn renew_execution_lease(&self) -> std::result::Result<(), HostedLeaseFailure>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HostedHeartbeatAction {
    Continue,
    Stop(HostedStopReason),
}

fn reconcile_hosted_heartbeat(
    failures: &mut u8,
    result: std::result::Result<(), HostedLeaseFailure>,
) -> HostedHeartbeatAction {
    match result {
        Ok(()) => {
            *failures = 0;
            HostedHeartbeatAction::Continue
        }
        Err(failure) => {
            *failures = failures.saturating_add(1);
            let stop_immediately = matches!(
                failure,
                HostedLeaseFailure::Invalidated(_) | HostedLeaseFailure::Cancelled
            );
            if !stop_immediately && *failures < 3 {
                return HostedHeartbeatAction::Continue;
            }
            HostedHeartbeatAction::Stop(match failure {
                HostedLeaseFailure::Cancelled => HostedStopReason::Cancellation,
                HostedLeaseFailure::Invalidated(message) => HostedStopReason::LeaseLost(message),
                HostedLeaseFailure::Temporary(message) => HostedStopReason::Infrastructure(
                    truncate_text(&format!("heartbeat failed: {message}"), 2_000),
                ),
            })
        }
    }
}

impl HostedLeaseControlPlane for HostedApiClient {
    fn renew_execution_lease(&self) -> std::result::Result<(), HostedLeaseFailure> {
        self.heartbeat().map_err(|error| {
            let hosted = error.downcast_ref::<HostedHttpError>();
            if hosted.is_some_and(|failure| {
                matches!(
                    failure.effective_code(),
                    "execution_cancelled" | "execution_completion_preempted_by_cancellation"
                )
            }) {
                HostedLeaseFailure::Cancelled
            } else if hosted.is_some_and(HostedHttpError::invalidates_execution) {
                HostedLeaseFailure::Invalidated(truncate_text(&format!("{error:#}"), 2_000))
            } else {
                HostedLeaseFailure::Temporary(truncate_text(&format!("{error:#}"), 2_000))
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HostedStopReason {
    Cancellation,
    LeaseLost(String),
    Infrastructure(String),
}

impl HostedSupervisor {
    fn start(
        api: HostedApiClient,
        running: Arc<AtomicBool>,
        stop_reason: Arc<Mutex<Option<HostedStopReason>>>,
    ) -> Self {
        Self::start_with(api, running, stop_reason)
    }

    fn start_with<C: HostedLeaseControlPlane>(
        api: C,
        running: Arc<AtomicBool>,
        stop_reason: Arc<Mutex<Option<HostedStopReason>>>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut next = Instant::now() + HEARTBEAT_INTERVAL;
            let mut failures = 0u8;
            while !thread_stop.load(Ordering::SeqCst)
                && running.load(Ordering::SeqCst)
                && !shutdown::requested()
            {
                if Instant::now() < next {
                    thread::sleep(Duration::from_millis(250));
                    continue;
                }
                match reconcile_hosted_heartbeat(&mut failures, api.renew_execution_lease()) {
                    HostedHeartbeatAction::Continue => {}
                    HostedHeartbeatAction::Stop(reason) => {
                        *stop_reason
                            .lock()
                            .expect("hosted stop reason lock poisoned") = Some(reason);
                        running.store(false, Ordering::SeqCst);
                        break;
                    }
                }
                next = Instant::now() + HEARTBEAT_INTERVAL;
            }
            if shutdown::requested() {
                *stop_reason
                    .lock()
                    .expect("hosted stop reason lock poisoned") =
                    Some(HostedStopReason::Cancellation);
                running.store(false, Ordering::SeqCst);
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_hosted_execution(
    api: &HostedApiClient,
    manifest: &HostedManifest,
    git_author: &GithubActionsAuthor,
    running: &Arc<AtomicBool>,
    stop_reason: &Arc<Mutex<Option<HostedStopReason>>>,
) -> Result<HostedResult> {
    ensure_running(running)?;
    if let Err(error) = validate_hosted_provider_startup_contract(manifest) {
        return Err(HostedProviderContractFailure::from_validation(error).into());
    }
    let containment = command::HostedProcessContainment::new()
        .context("hosted repository process containment is unavailable")?;
    let repo = Repo::discover()?;
    let repo_config = manifest.repo_config()?;
    repo.verify_origin(&repo_config.owner, &repo_config.name)?;
    repo.verify_hosted_origin(
        &repo_config.owner,
        &repo_config.name,
        &manifest.github.web_base_url,
    )?;
    let initial_dirty = repo.ensure_safe(false)?;
    if !initial_dirty.is_empty() {
        bail!("hosted checkout must start with a clean working tree");
    }

    api.append_event(
        "progress",
        json!({
            "step": "repository",
            "status": "running",
            "repository": manifest.github.repository,
            "branch": manifest.github.branch
        }),
    )?;
    containment.drain()?;
    let branch_token = api.github_token(&manifest.github.repository)?;
    let resumed = repo.checkout_or_create_hosted_branch(
        &manifest.github.branch,
        &manifest.github.base_sha,
        branch_token.expose(),
        &manifest.github.web_base_url,
    )?;
    drop(branch_token);
    repo.configure_hosted_author(&git_author.name, &git_author.email)?;
    let trusted_git_config = repo.hosted_local_config()?;
    let trusted_head = command::checked("git", ["rev-parse", "HEAD"], &repo.root)?;
    let baseline = BTreeSet::new();
    api.append_event(
        "progress",
        json!({
            "step": "branch",
            "status": "completed",
            "branch": manifest.github.branch,
            "resumed": resumed
        }),
    )?;

    containment.drain()?;
    let recovery_token = api.github_token(&manifest.github.repository)?;
    let recovery_github =
        GitHubClient::new(recovery_token.expose(), &manifest.github.web_base_url)?;
    let existing_pr =
        recovery_github.find_open_pull_request(&repo_config, &manifest.github.branch)?;
    drop(recovery_github);
    drop(recovery_token);

    let startup_changed_paths = completion_changed_paths(&repo, &manifest.github.base_sha)?;
    let startup = resolve_startup_mode(manifest, resumed, &startup_changed_paths);
    api.append_event(
        "progress",
        json!({
            "event_type": "worker.startup_mode_resolved",
            "startup_mode": startup.mode,
            "persisted_graph_presence": startup.persisted_graph_present,
            "persisted_notebook_revision": startup.persisted_notebook_revision,
            "repository_diff_status": if startup_changed_paths.is_empty() { "clean" } else { "changed" },
            "branch_state": if resumed { "existing" } else { "created" },
            "recovery_marker_present": startup.recovery_marker_present,
            "selected_next_decision": startup.mode.next_decision(),
        }),
    )?;
    let partial_run = detect_partial_run(
        existing_pr.as_ref(),
        resumed,
        manifest.execution.attempt_number,
        startup_changed_paths,
    );
    let mut agent = GatewayAgent::new(
        api.clone(),
        manifest,
        &repo,
        &trusted_git_config,
        running,
        stop_reason,
        &containment,
        partial_run,
    )
    .map_err(|underlying| {
        anyhow!(HostedStartupFailure {
            category: "execution_graph_initialization_failed",
            code: "execution_graph_initialization_failed",
            message: format!(
                "The hosted execution graph could not be initialized: {}",
                truncate_text(&underlying.to_string(), 2_000)
            ),
            underlying,
        })
    })?;
    let execution_result = (|| -> Result<HostedResult> {
        if let Some(partial_run) = &agent.partial_run {
            api.append_event(
                "progress",
                json!({
                    "event_type": "worker.partial_run_detected",
                    "step": "implementation",
                    "status": "continuing",
                    "branch": manifest.github.branch,
                    "execution_attempt": manifest.execution.attempt_number,
                    "pull_request_number": partial_run.pull_request_number,
                    "changed_paths": partial_run.changed_paths,
                    "remaining_work": agent.notebook.remaining_work,
                    "resume_phase": agent.phases.active(),
                    "resumable": true
                }),
            )?;
        }
        agent.ensure_active_or_checkpoint_cancellation()?;
        let bootstrap_result = bootstrap_hosted_dependencies(
            api,
            manifest,
            &repo,
            running,
            &containment,
            agent.notebook.dependency_bootstrap_evidence.as_ref(),
        );
        agent.ensure_active_or_checkpoint_cancellation()?;
        let bootstrap_evidence = bootstrap_result?;
        if let Some(evidence) = bootstrap_evidence {
            agent.notebook.dependency_bootstrap_evidence = Some(evidence);
            agent.notebook.orchestration.dependency_bootstrap_completed = true;
            agent.persist_orchestration_checkpoint("dependency_bootstrap_completed", false)?;
        }
        if startup.mode == StartupMode::FreshRun {
            agent
                .initialize_fresh_execution_snapshot(&startup, resumed)
                .map_err(|underlying| {
                    anyhow!(HostedStartupFailure {
                        category: "execution_graph_initialization_failed",
                        code: "execution_graph_initialization_failed",
                        message: format!(
                            "The fresh execution snapshot could not be persisted: {}",
                            truncate_text(&underlying.to_string(), 2_000)
                        ),
                        underlying,
                    })
                })?;
        }
        if startup.mode == StartupMode::RecoveryPublicationRun {
            let persisted_reason = agent
                .notebook
                .orchestration
                .failures
                .unresolved()
                .find(|failure| {
                    failure.category
                        == crate::execution_graph::FailureCategory::OrchestrationInvariantViolation
                })
                .map(|failure| failure.message.clone())
                .unwrap_or_else(|| {
                    "persisted execution state requires recovery publication evaluation".into()
                });
            let recovery_cause = anyhow!(persisted_reason);
            let recovery = attempt_safe_recovery_publication(
                &mut agent,
                RecoveryPublicationContext {
                    api,
                    manifest,
                    repo: &repo,
                    repo_config: &repo_config,
                    trusted_git_config: &trusted_git_config,
                    trusted_head: &trusted_head,
                    baseline: &baseline,
                    containment: &containment,
                    running,
                    startup_mode: startup.mode,
                },
                &recovery_cause,
            );
            match recovery.result {
                RecoveryPublicationResult::PublishedDraft => {
                    return Ok(recovery
                        .published
                        .expect("published recovery result includes hosted output"));
                }
                RecoveryPublicationResult::NotApplicable
                | RecoveryPublicationResult::SkippedNoDiff => {}
                RecoveryPublicationResult::FailedInfrastructure => {
                    let recovery_error = recovery
                        .error
                        .expect("failed recovery result includes its infrastructure error");
                    return Err(agent.categorized_execution_failure(
                        "recovery_publication_failed",
                        "recovery_publication_failed",
                        format!(
                            "Recovery publication failed: {}",
                            truncate_text(&recovery_error.to_string(), 2_000)
                        ),
                        Some(&recovery_error),
                        true,
                        "Resume the interrupted recovery publication from its persisted checkpoint.",
                    ));
                }
            }
        }
        // Refresh validation gate fingerprints against the live dependency and
        // environment state before deciding whether persisted finalization can
        // be reused.
        let _ = agent.build_execution_snapshot()?;
        let live_repository_fingerprint =
            repository_state_fingerprint(&repo, &manifest.github.base_sha)?;
        let live_changed_paths = completion_changed_paths(&repo, &manifest.github.base_sha)?;
        if agent
            .finalization_requires_revalidation(&live_repository_fingerprint, &live_changed_paths)
        {
            if crate::execution_graph::current_epoch_terminal_outcome(
                &agent.notebook.orchestration.domain_events,
            )
            .is_some()
            {
                bail!(
                    "persisted terminal finalization does not match the live repository fingerprint"
                );
            }
            agent.invalidate_finalization_after_remote_reconciliation(
                &live_repository_fingerprint,
            )?;
        }
        let initial_decision = agent.peek_execution_decision()?;
        match &initial_decision {
            ExecutionDecision::StopForGuardrail { outcome, .. }
                if outcome.publication_mode().is_none() =>
            {
                agent.reconcile_active_phase("restored terminal guardrail forbids publication")?;
                bail!("restored terminal guardrail cannot enter publication");
            }
            ExecutionDecision::Finish { outcome } if outcome.publication_mode().is_none() => {
                bail!(
                    "restored hosted execution already terminated with non-publication outcome `{outcome:?}`"
                );
            }
            _ => {}
        }
        let mut implementation = if execution_decision_requires_model_work(&initial_decision) {
            if existing_pr.is_some() && resumed {
                api.append_event(
                    "progress",
                    json!({
                        "step": "implementation",
                        "status": "continuing",
                        "branch": manifest.github.branch,
                        "execution_attempt": manifest.execution.attempt_number,
                        "resumable": true
                    }),
                )?;
            }
            agent.implement()?
        } else {
            agent.reconstruct_implementation_outcome()?
        };
        agent.ensure_active_or_checkpoint_cancellation()?;
        agent.reconcile_wall_clock_boundary(HostedWallClockBoundary::BeforeValidation)?;

        let mut validation_round = 1_u32;
        let decision_after_model_work = agent.peek_execution_decision()?;
        let mut validation = if matches!(
            decision_after_model_work,
            ExecutionDecision::RunValidation { .. }
        ) {
            let implementation_status = agent.reconcile_authoritative_target_state()?;
            let implementation_changed_paths =
                completion_changed_paths(&repo, &manifest.github.base_sha)?;
            let validation_entry = validation_entry_decision(
                implementation_status,
                implementation_changed_paths.len(),
                agent.partial_run.is_some(),
                implementation.budget_exhausted
                    || agent.write_blocker.is_some()
                    || !agent.notebook.blocking_unknowns.is_empty(),
            );
            if !validation_entry_allows_gates(validation_entry) {
                let reason_code = if implementation_changed_paths.is_empty() {
                    "no_implementation_changes"
                } else {
                    "implementation_not_ready_for_validation"
                };
                agent.append_event_recoverable(
                    "validation",
                    json!({
                        "event_type": "worker.validation_skipped",
                        "reason_code": reason_code,
                        "implementation_status": implementation_status,
                        "implementation_substate": agent.notebook.implementation_substate,
                        "changed_paths": implementation_changed_paths,
                        "remaining_work": agent.notebook.remaining_work_v2,
                        "model_calls_used": agent.phases.total_calls(),
                        "cost": agent.cost_guard,
                        "tool_usage": agent.tool_usage,
                    }),
                    "validation skip invariant",
                );
                if implementation_changed_paths.is_empty() {
                    agent.append_execution_domain_event(
                        crate::execution_graph::ExecutionDomainEvent::GuardrailTriggered {
                            sequence: agent.next_domain_event_sequence(),
                            reason: crate::execution_graph::GuardrailReason::BlockingFailure,
                            outcome: OrchestratedMissionOutcome::BlockedNoDiff,
                            detail: "implementation ended without a reviewable repository diff"
                                .into(),
                        },
                    )?;
                    agent.finalize_guardrail_outcome(OrchestratedMissionOutcome::BlockedNoDiff)?;
                    agent.persist_orchestration_checkpoint("blocked_no_diff", false)?;
                    return Err(agent.blocked_no_diff_failure());
                }
                agent.write_blocker.get_or_insert_with(|| {
                    "repository validation is forbidden until implementation produces relevant changes"
                        .into()
                });
                agent.checkpoint_notebook(false)?;
                return Err(agent.implementation_preparation_failure());
            }

            api.update_state("validating")?;
            let mut validation = dispatch_validation_gates(validation_entry, || {
                run_graph_validation_sequence(&mut agent, api, manifest, &repo, validation_round)
            })?
            .context("validation entry policy rejected worker-owned quality gates")?;
            let maximum_repair_attempts = usize::try_from(
                agent
                    .notebook
                    .orchestration
                    .budget
                    .mission
                    .max_target_repair_rounds,
            )
            .unwrap_or(MAX_REPAIR_ATTEMPTS)
            .max(1);
            for repair_attempt in 0..maximum_repair_attempts {
                let failures = validation
                    .iter()
                    .filter(|result| result.status != "passed")
                    .cloned()
                    .collect::<Vec<_>>();
                if failures.is_empty() {
                    break;
                }
                let repair_tree_before =
                    repository_state_fingerprint(&repo, &manifest.github.base_sha)?;
                implementation = agent.repair(&failures, repair_attempt + 1)?;
                let repair_tree_after =
                    repository_state_fingerprint(&repo, &manifest.github.base_sha)?;
                if repair_tree_after == repair_tree_before {
                    agent.record_validation_repair_no_mutation(
                        &failures,
                        "bounded validation repair produced no repository mutation",
                    )?;
                    agent.append_event_recoverable(
                        "validation",
                        json!({
                            "event_type": "worker.validation_repair_stopped",
                            "reason_code": "repair_produced_no_source_mutation",
                            "source_tree_hash": repair_tree_after,
                            "failed_gates": failures.iter().map(|failure| &failure.id).collect::<Vec<_>>(),
                            "rerun_skipped": true,
                        }),
                        "validation repair no-mutation guard",
                    );
                    implementation.budget_exhausted = true;
                    break;
                }
                agent.reconcile_authoritative_target_state()?;
                let repair_decision = agent.reconcile_active_phase(
                    "validation repair ended; rerunning required quality gates",
                )?;
                if !matches!(
                    repair_decision,
                    PhaseDecision::Transition(ExecutionPhase::Validation)
                ) && agent.phases.active() != ExecutionPhase::Validation
                {
                    return Err(anyhow!(HostedInvariantFailure::new(
                        "required_implementation_targets_unresolved",
                        "validation repair left required implementation targets unresolved",
                    )));
                }
                validation_round = validation_round.saturating_add(1);
                validation = run_graph_validation_sequence(
                    &mut agent,
                    api,
                    manifest,
                    &repo,
                    validation_round,
                )?;
            }
            validation
        } else if execution_decision_has_completed_validation(&decision_after_model_work) {
            agent.restored_validation_results()?
        } else {
            bail!(
                "hosted orchestrator returned `{}` where validation or a later finalization stage was required",
                execution_decision_name(&decision_after_model_work)
            );
        };
        let validation_passed = validation.iter().all(|result| result.status == "passed");
        let diff_decision = agent.peek_execution_decision()?;
        let incomplete_review_reason = match &diff_decision {
            ExecutionDecision::ReviewIncompleteDiff { reason, .. } => Some(*reason),
            _ => None,
        };
        let review_paths = match diff_decision {
            ExecutionDecision::ReviewDiff { .. } if validation_passed => {
                agent.deterministic_diff_review()?
            }
            ExecutionDecision::ReviewIncompleteDiff { reason, .. } => {
                agent.deterministic_incomplete_diff_review(reason).map_err(|error| {
                    agent.categorized_execution_failure(
                        "HostedLifecycleContractFailure",
                        "repair_to_incomplete_review_not_supported",
                        "The execution graph selected a legal repair-to-incomplete-review transition, but the hosted lifecycle wrapper rejected it.",
                        Some(&error),
                        false,
                        "Fix the hosted lifecycle adapter; do not restart discovery or relabel this as initialization failure.",
                    )
                })?
            }
            ExecutionDecision::EvaluateCompletion { .. }
            | ExecutionDecision::Publish { .. }
            | ExecutionDecision::Finish { .. } => {
                completion_changed_paths(&repo, &manifest.github.base_sha)?
            }
            ref decision => {
                bail!(
                    "hosted orchestrator returned `{}` where diff review or a later finalization stage was required",
                    execution_decision_name(decision)
                )
            }
        };
        let completion_decision = agent.peek_execution_decision()?;
        let evaluating_completion = matches!(
            completion_decision,
            ExecutionDecision::EvaluateCompletion { .. }
        );
        if evaluating_completion
            && validation_passed
            && implementation.explicit_declaration.is_none()
            && let Some(declaration) = deterministic_complete_declaration(
                &agent.notebook.planned_changes,
                &agent.notebook.acceptance_criteria,
                &review_paths,
                &agent.notebook.remaining_work_v2,
                &agent.tool_failures,
            )
        {
            agent.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.implementation_declaration_reconciled",
                    "source": "authoritative_target_and_diff_state",
                    "implementation_status": declaration.implementation_status,
                    "changed_paths": declaration.changed_paths,
                    "criteria_evidence": declaration.criteria_evidence,
                }),
                "deterministic implementation declaration",
            );
            agent.declaration = Some(declaration.clone());
            implementation.explicit_declaration = Some(declaration);
        }
        if evaluating_completion
            && validation_passed
            && implementation.budget_exhausted
            && implementation.explicit_declaration.is_none()
            && let Some(declaration) = deterministic_partial_declaration(
                &agent.notebook.planned_changes,
                &review_paths,
                &agent.notebook.remaining_work_v2,
            )
        {
            agent.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.partial_implementation_reconciled",
                    "implementation_status": "partial",
                    "changed_paths": declaration.changed_paths,
                    "remaining_work": declaration.remaining_work,
                    "publication_mode": "draft_pull_request",
                }),
                "deterministic partial implementation declaration",
            );
            agent.declaration = Some(declaration.clone());
            implementation.explicit_declaration = Some(declaration);
        }
        let mut completeness = match completion_decision {
            ExecutionDecision::EvaluateCompletion { .. } => {
                let mut completeness =
                    agent.evaluate_completion(&implementation, &validation, &review_paths)?;
                if let Some(reason) = incomplete_review_reason {
                    let (failure_category, reason_code, failure_phase) = match reason {
                        crate::execution_graph::IncompleteReason::ValidationRepairProducedNoMutation =>
                            ("validation_failure", "validation_failed_repair_incomplete", "repair"),
                        crate::execution_graph::IncompleteReason::ValidationRepairProducedNoMeaningfulMutation =>
                            ("validation_repair_failure", "validation_repair_unresolved", "repair"),
                        crate::execution_graph::IncompleteReason::ValidationInfrastructureFailure =>
                            ("infrastructure_failure", "validation_infrastructure_incomplete", "validation"),
                        crate::execution_graph::IncompleteReason::TargetOperationConflict =>
                            ("mutation_conflict", "target_operation_conflict", "implementation"),
                    };
                    completeness.status = CompletionStatus::Partial;
                    completeness.implementation_completeness = ImplementationCompleteness::Partial;
                    completeness.verification_readiness =
                        VerificationReadiness::PendingManualReview;
                    completeness.evaluation_source = EvaluationSource::OrchestratorFallback;
                    completeness.confidence = 1.0;
                    for failure in validation.iter().filter(|result| result.status != "passed") {
                        push_unique(
                            &mut completeness.remaining_automated_verification,
                            format!(
                                "Reconcile `{}` and rerun `{}`.",
                                failure.id, failure.command
                            ),
                        );
                    }
                    for gate in agent
                        .notebook
                        .required_gates
                        .iter()
                        .filter(|gate| gate.required && gate.status != ValidationStatus::Passed)
                    {
                        push_unique(
                            &mut completeness.remaining_automated_verification,
                            format!("Run required gate `{}`: `{}`.", gate.gate_id, gate.command),
                        );
                    }
                    let repair_attempts = crate::execution_graph::current_execution_epoch(
                        &agent.notebook.orchestration.domain_events,
                    )
                    .iter()
                    .filter_map(|event| {
                        match event {
                        crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
                            attempt: Some(attempt),
                            ..
                        } => Some(format!(
                            "{} via {} ({:?})",
                            attempt.target_path,
                            attempt.requested_tool_policy.as_str(),
                            attempt.outcome
                        )),
                        _ => None,
                    }
                    })
                    .collect::<Vec<_>>();
                    let failing_assertions = agent
                        .notebook
                        .orchestration
                        .failures
                        .unresolved()
                        .filter(|failure| {
                            failure.category
                                == crate::execution_graph::FailureCategory::ValidationFailure
                        })
                        .flat_map(|failure| failure.assertion_failures.iter())
                        .map(|assertion| assertion.test_name.clone())
                        .filter(|name| !name.is_empty())
                        .collect::<BTreeSet<_>>();
                    push_unique(
                        &mut completeness.pending_external_review,
                        format!(
                            "Review unresolved validation repair after attempts [{}]; failing assertions [{}].",
                            repair_attempts.join(", "),
                            failing_assertions
                                .iter()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    );
                    completeness.summary = format!(
                        "RustGrid preserved the applied repository diff for draft review after {reason:?}; required validation remains incomplete. Attempted repairs: [{}]. Failing assertions: [{}].",
                        repair_attempts.join(", "),
                        failing_assertions
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    agent.append_event_recoverable(
                        "result",
                        json!({
                            "event_type": "worker.partial_reviewable_evaluated",
                            "category": failure_category,
                            "reason_code": reason_code,
                            "phase": failure_phase,
                            "recoverable": true,
                            "reason": reason,
                            "mission_outcome": "partial_reviewable",
                            "process_health": "healthy",
                            "changed_paths": review_paths,
                            "failed_gates": validation.iter()
                                .filter(|result| result.status != "passed")
                                .collect::<Vec<_>>(),
                            "attempted_repairs": repair_attempts,
                            "failing_assertions": failing_assertions,
                            "external_review_required": true,
                        }),
                        "partial-reviewable evaluation",
                    );
                }
                agent.record_completion_evaluated(
                    &completeness,
                    review_paths.clone(),
                    implementation.explicit_declaration.clone(),
                    "completion_evaluated",
                    false,
                )?;
                api.append_event(
                    "result",
                    json!({
                        "status": "implementation_evaluated",
                        "canonical_terminal_result_id": canonical_terminal_result_id(
                            manifest.execution.execution_id
                        ),
                        "mission_outcome": agent.completion_outcome,
                        "graph_outcome": agent.completion_outcome,
                        "implementation_completeness": completeness,
                        "technical_validation": {
                            "status": if validation_passed { "passed" } else { "failed" },
                            "gates": validation
                        },
                        "budget": agent.budget_telemetry(),
                        "tool_usage": agent.tool_usage,
                        "changed_path_count": review_paths.len(),
                        "resumable": requires_implementation_continuation(completeness.status)
                    }),
                )?;
                completeness
            }
            ExecutionDecision::Publish { .. } | ExecutionDecision::Finish { .. } => {
                agent.restored_completion_evaluation(&implementation, &validation, &review_paths)?
            }
            ref decision => bail!(
                "hosted orchestrator returned `{}` where completion evaluation or a later finalization stage was required",
                execution_decision_name(decision)
            ),
        };

        let publication_decision = agent.peek_execution_decision()?;
        match publication_decision {
            ExecutionDecision::Publish { .. } => {
                let applied = agent.reconcile_execution_and_apply()?;
                if !matches!(applied.decision, ExecutionDecision::Publish { .. })
                    || agent.phases.active() != ExecutionPhase::Publication
                {
                    bail!(
                        "lifecycle invariant violated: publication requires completion evaluation"
                    );
                }
            }
            ExecutionDecision::Finish { .. } => {}
            ref decision => bail!(
                "hosted orchestrator returned `{}` where publication was required",
                execution_decision_name(decision)
            ),
        }
        if repo.hosted_local_config()? != trusted_git_config {
            bail!("repository-controlled execution modified the protected local Git configuration");
        }
        repo.verify_hosted_origin(
            &repo_config.owner,
            &repo_config.name,
            &manifest.github.web_base_url,
        )?;
        let restored_publication = agent.notebook.orchestration.publication.clone();
        let local_head = command::checked("git", ["rev-parse", "HEAD"], &repo.root)?;
        if let Some(persisted_commit) = restored_publication.commit_sha.as_deref() {
            if local_head != persisted_commit {
                bail!(
                    "persisted publication commit `{persisted_commit}` does not match local HEAD `{local_head}`"
                );
            }
        } else if local_head != trusted_head {
            bail!("repository-controlled execution modified Git history before publication");
        }
        let publication_node =
            agent.graph_node_id(crate::execution_graph::ExecutionNodeKind::Publication)?;
        let mut commit = if let Some(commit) = restored_publication.commit_sha.clone() {
            commit
        } else {
            let dirty = repo.new_agent_paths(&baseline)?;
            let commit = if dirty.is_empty() {
                let (commit, committed_paths) =
                    committed_head_for_publication(&repo, &manifest.github.base_sha)?
                        .context("the hosted execution produced no committable changes")?;
                if existing_pr.is_none() {
                    api.append_event(
                        "progress",
                        json!({
                            "event_type": "worker.publication_recovered_committed_head",
                            "head_sha": commit,
                            "changed_paths": committed_paths,
                            "reason": "resumed branch contains base-to-head changes but no pull request",
                        }),
                    )?;
                }
                commit
            } else {
                let commit = repo.commit_paths(
                    &dirty,
                    &format!("{}: {}", manifest.ticket_key, manifest.ticket_title),
                )?;
                api.append_event(
                    "progress",
                    json!({
                        "step": "commit",
                        "status": "completed",
                        "head_sha": commit,
                        "changed_paths": dirty
                    }),
                )?;
                commit
            };
            agent.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::CommitCreated {
                    sequence: agent.next_domain_event_sequence(),
                    node_id: publication_node.clone(),
                    commit_sha: commit.clone(),
                },
            )?;
            agent.persist_orchestration_checkpoint("commit_created", true)?;
            commit
        };

        let publication_claims_pushed = matches!(
            restored_publication.status,
            crate::execution_graph::PublicationStatus::BranchPushed
                | crate::execution_graph::PublicationStatus::PullRequestCreated
        );
        let branch_already_pushed = if publication_claims_pushed {
            let verification_token = api.github_token(&manifest.github.repository)?;
            let remote_head = repo.remote_branch_head(
                &manifest.github.branch,
                verification_token.expose(),
                &manifest.github.web_base_url,
            )?;
            drop(verification_token);
            let current = remote_head.as_deref() == Some(commit.as_str());
            if !current
                && restored_publication.status
                    == crate::execution_graph::PublicationStatus::PullRequestCreated
            {
                bail!(
                    "published pull request branch no longer points to persisted commit `{commit}`"
                );
            }
            current
        } else {
            false
        };
        if !branch_already_pushed {
            agent.ensure_active_or_checkpoint_cancellation()?;
            let publication_context = HostedPublicationContext {
                api,
                manifest,
                repo: &repo,
                repo_config: &repo_config,
                trusted_git_config: &trusted_git_config,
                containment: &containment,
                validation_round: &mut validation_round,
            };
            publish_hosted_branch(
                &mut agent,
                publication_context,
                &mut commit,
                &mut validation,
                &mut implementation,
                &mut completeness,
            )?;
            agent.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::BranchPushed {
                    sequence: agent.next_domain_event_sequence(),
                    node_id: publication_node.clone(),
                    branch: manifest.github.branch.clone(),
                },
            )?;
            agent.persist_orchestration_checkpoint("branch_pushed", true)?;
        }
        agent.reconcile_wall_clock_boundary(HostedWallClockBoundary::PullRequestCreation)?;
        let pull = if restored_publication.status
            == crate::execution_graph::PublicationStatus::PullRequestCreated
        {
            PullRequestResult {
                number: restored_publication
                    .pull_request_number
                    .or_else(|| existing_pr.as_ref().map(|pull| pull.number))
                    .context("persisted publication has no pull request number")?,
                url: restored_publication
                    .pull_request_url
                    .clone()
                    .or_else(|| existing_pr.as_ref().map(|pull| pull.html_url.clone()))
                    .context("persisted publication has no pull request URL")?,
            }
        } else {
            api.update_state("creating_pull_request")?;
            containment.drain()?;
            let publication_token = api.github_token(&manifest.github.repository)?;
            let github =
                GitHubClient::new(publication_token.expose(), &manifest.github.web_base_url)?;
            let partial = requires_implementation_continuation(completeness.status);
            let draft =
                partial || completeness.status == CompletionStatus::CompletePendingExternalReview;
            let created = find_or_create_hosted_pull_request(
                &github,
                &repo_config,
                manifest,
                &validation,
                &completeness,
                draft,
                partial,
            )?;
            drop(github);
            drop(publication_token);
            agent.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::PullRequestCreated {
                    sequence: agent.next_domain_event_sequence(),
                    node_id: publication_node,
                    url: created.html_url.clone(),
                    number: Some(created.number),
                    draft,
                },
            )?;
            agent.persist_orchestration_checkpoint("pull_request_created", true)?;
            PullRequestResult {
                number: created.number,
                url: created.html_url,
            }
        };
        let terminal = agent.reconcile_execution_and_apply()?;
        if !matches!(
            terminal.decision,
            ExecutionDecision::Finish {
                outcome
            } if Some(outcome) == agent.completion_outcome
        ) {
            bail!(
                "hosted orchestrator did not produce the expected terminal outcome after publication"
            );
        }
        agent.persist_orchestration_checkpoint("run_finished", true)?;
        agent.append_event_recoverable(
            "result",
            json!({
                "event_type": "worker.domain_run_finished",
                "canonical_terminal_result_id": canonical_terminal_result_id(
                    manifest.execution.execution_id
                ),
                "mission_outcome": agent.completion_outcome,
                "graph_outcome": agent.completion_outcome,
                "process_health": "healthy",
                "reason_code": terminal_reason_code(completeness.status),
                "publication": agent.notebook.orchestration.publication,
                "remaining_work": agent.notebook.remaining_work_v2,
                "notebook": agent.notebook,
            }),
            "terminal domain result",
        );
        let terminal_telemetry = TerminalTelemetry {
            model_calls_used: agent.phases.total_calls(),
            input_tokens: agent.cost_guard.input_tokens,
            output_tokens: agent.cost_guard.output_tokens,
            estimated_cost_micros: agent.cost_guard.estimated_cost_micros,
            usage: agent.tool_usage.clone(),
            changed_paths: completion_changed_paths(&repo, &manifest.github.base_sha)?,
            last_successful_action: agent.last_successful_action.clone(),
            phase_reached: Some(agent.phases.active()),
            plan: agent.notebook.planned_changes.clone(),
            remaining_work: agent.notebook.remaining_work_v2.clone(),
            validation_evidence: agent.notebook.validation_evidence.clone(),
            notebook_revision: agent.notebook.revision,
            discovery_calls: agent.phases.phase_calls(ExecutionPhase::Discovery),
            planning_calls: agent.phases.phase_calls(ExecutionPhase::Planning),
            initial_target_mutation_calls: agent
                .notebook
                .orchestration
                .budget
                .model_call_breakdown
                .initial_target_mutation_calls,
            target_mutation_repair_calls: agent
                .notebook
                .orchestration
                .budget
                .model_call_breakdown
                .target_mutation_repair_calls,
            validation_diagnosis_calls: agent
                .notebook
                .orchestration
                .budget
                .model_call_breakdown
                .validation_diagnosis_calls,
            validation_repair_mutation_calls: agent
                .notebook
                .orchestration
                .budget
                .model_call_breakdown
                .validation_repair_mutation_calls,
            diff_review_calls: agent.phases.phase_calls(ExecutionPhase::DiffReview),
            completion_evaluation_calls: agent
                .phases
                .phase_calls(ExecutionPhase::CompletionEvaluation),
        };
        Ok(HostedResult {
            summary: implementation.summary,
            branch: manifest.github.branch.clone(),
            commit,
            pull_request: pull,
            validation,
            completeness,
            terminal_telemetry,
        })
    })();
    match execution_result {
        Ok(result) => Ok(result),
        Err(error)
            if (is_hosted_orchestration_invariant_error(&error)
                && startup.mode == StartupMode::RecoveryPublicationRun)
                || agent
                    .notebook
                    .orchestration
                    .snapshot(
                        manifest.execution.execution_id.to_string(),
                        crate::execution_graph::RepositorySnapshot {
                            fingerprint: agent.notebook.repository_fingerprint.clone(),
                            source_tree_hash: agent.notebook.repository_fingerprint.clone(),
                            changed_paths: completion_changed_paths(
                                &repo,
                                &manifest.github.base_sha,
                            )
                            .unwrap_or_default()
                            .into_iter()
                            .collect(),
                            ..crate::execution_graph::RepositorySnapshot::default()
                        },
                    )
                    .has_partial_reviewable_guardrail()
                || agent
                    .notebook
                    .orchestration
                    .snapshot(
                        manifest.execution.execution_id.to_string(),
                        crate::execution_graph::RepositorySnapshot {
                            fingerprint: agent.notebook.repository_fingerprint.clone(),
                            source_tree_hash: agent.notebook.repository_fingerprint.clone(),
                            changed_paths: completion_changed_paths(
                                &repo,
                                &manifest.github.base_sha,
                            )
                            .unwrap_or_default()
                            .into_iter()
                            .collect(),
                            ..crate::execution_graph::RepositorySnapshot::default()
                        },
                    )
                    .has_incomplete_diff_review_request() =>
        {
            let recovery = attempt_safe_recovery_publication(
                &mut agent,
                RecoveryPublicationContext {
                    api,
                    manifest,
                    repo: &repo,
                    repo_config: &repo_config,
                    trusted_git_config: &trusted_git_config,
                    trusted_head: &trusted_head,
                    baseline: &baseline,
                    containment: &containment,
                    running,
                    startup_mode: startup.mode,
                },
                &error,
            );
            match recovery.result {
                RecoveryPublicationResult::PublishedDraft => Ok(recovery
                    .published
                    .expect("published recovery result includes hosted output")),
                RecoveryPublicationResult::NotApplicable
                | RecoveryPublicationResult::SkippedNoDiff => {
                    if error
                        .downcast_ref::<HostedAgentExecutionFailure>()
                        .is_some()
                    {
                        Err(error)
                    } else {
                        let (code, message) = safe_failure(&error, false);
                        Err(agent.categorized_execution_failure(
                            "execution_graph_initialization_failed",
                            &code,
                            message,
                            Some(&error),
                            true,
                            "Resume from the persisted notebook after resolving the exact orchestration invariant.",
                        ))
                    }
                }
                RecoveryPublicationResult::FailedInfrastructure => {
                    let recovery_error = recovery
                        .error
                        .expect("failed recovery result includes its infrastructure error");
                    agent.append_event_recoverable(
                        "progress",
                        json!({
                            "event_type": "worker.recovery_publication_failed",
                            "original_failure": truncate_text(&error.to_string(), 2_000),
                            "recovery_failure": truncate_text(&recovery_error.to_string(), 2_000),
                        }),
                        "recovery publication failure",
                    );
                    if error
                        .downcast_ref::<HostedAgentExecutionFailure>()
                        .is_some()
                    {
                        Err(error)
                    } else {
                        Err(agent.categorized_execution_failure(
                            "recovery_publication_failed",
                            "recovery_publication_failed",
                            format!(
                                "Recovery publication failed: {}",
                                truncate_text(&recovery_error.to_string(), 2_000)
                            ),
                            Some(&recovery_error),
                            true,
                            "Resume from the persisted notebook; the validated draft-recovery publication attempt was preserved for an idempotent retry.",
                        ))
                    }
                }
            }
        }
        Err(error)
            if error
                .downcast_ref::<HostedAgentExecutionFailure>()
                .is_some() =>
        {
            Err(error)
        }
        Err(error) if error.downcast_ref::<HostedStartupFailure>().is_some() => Err(error),
        Err(error) => {
            let (code, message) = safe_failure(&error, false);
            let category = hosted_failure_category(&error);
            Err(agent.categorized_execution_failure(
                category,
                &code,
                message,
                Some(&error),
                true,
                "Resume from the persisted notebook after resolving the exact validation or publication failure.",
            ))
        }
    }
}

fn may_publish_hosted_terminal_state(error: &anyhow::Error) -> bool {
    error.downcast_ref::<HostedLeaseLost>().is_none()
}

#[cfg(test)]
mod tests;
