use super::*;

const TERMINAL_REVISION: u64 = 1;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CanonicalMissionOutcome {
    Complete,
    CompletePendingExternalReview,
    PartialReviewable,
    Blocked,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProcessHealth {
    Healthy,
    Degraded,
    Failed,
}

impl ProcessHealth {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DomainExecutionStatus {
    Completed,
    AwaitingExternalReview,
    NeedsContinuation,
    Blocked,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TerminalAuthority {
    WorkerDomain,
    InfrastructureFallback,
    AdministrativeOverride,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TerminalEvidenceSource {
    CanonicalWorkerResult,
    FinalCallback,
    WorkflowConclusion,
    LeaseExpiration,
    AdministrativeOverride,
}

impl TerminalEvidenceSource {
    pub(super) const fn precedence(self) -> Option<u8> {
        match self {
            Self::CanonicalWorkerResult => Some(4),
            Self::FinalCallback => Some(3),
            Self::WorkflowConclusion => Some(2),
            Self::LeaseExpiration => Some(1),
            // An administrative override is an explicit operation, not an
            // automatically competing observation in the evidence ordering.
            Self::AdministrativeOverride => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CallbackStatus {
    Pending,
    Acknowledged,
    Missing,
    FailedTransport,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct HostedTerminalTransportState {
    pub(super) callback_status: CallbackStatus,
    pub(super) callback_attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_callback_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) workflow_conclusion: Option<String>,
    pub(super) observed_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct FinalExecutionCallback {
    pub(super) execution_id: Uuid,
    pub(super) canonical_terminal_result_id: Uuid,
    pub(super) terminal_revision: u64,
    pub(super) final_notebook_revision: u64,
    pub(super) process_exit_code: i32,
    pub(super) workflow_run_id: String,
    pub(super) sent_at: String,
    pub(super) idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct TerminalCallbackOutboxEntry {
    pub(super) execution_id: Uuid,
    pub(super) canonical_terminal_result_id: Uuid,
    pub(super) payload_hash: String,
    pub(super) attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) acknowledged_at: Option<String>,
    pub(super) created_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalCallbackDelivery {
    Acknowledged { attempts: u32 },
    Pending { attempts: u32 },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct TerminalFinality {
    pub(super) terminal_result_id: Uuid,
    pub(super) terminal_revision: u64,
    pub(super) authority: TerminalAuthority,
    pub(super) finalized_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum Resumability {
    NotResumable,
    AwaitingExternalReview,
    Resumable {
        #[serde(skip_serializing_if = "Option::is_none")]
        resume_phase: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct ResumabilityDecision {
    pub(super) status: Resumability,
    pub(super) reason_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) resume_from_node: Option<String>,
    pub(super) repository_fingerprint: String,
}

impl Resumability {
    pub(super) const fn is_resumable(&self) -> bool {
        matches!(self, Self::Resumable { .. })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RemainingEvidenceType {
    ExternalApproval,
    ManualInspection,
    MissingAutomatedValidation,
    UnresolvedImplementation,
    InfrastructureIncomplete,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RemainingWorkKind {
    ExternalApproval,
    ManualVisualInspection,
    MissingImplementation,
    FailedValidation,
    MissingAutomatedValidation,
    InfrastructureFollowUp,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct CanonicalPublicationResult {
    pub(super) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pull_request_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pull_request_url: Option<String>,
    pub(super) draft: bool,
    pub(super) mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) completed_at: Option<String>,
}

impl CanonicalPublicationResult {
    fn published(result: &HostedResult, completed_at: &str, draft: bool) -> Self {
        Self {
            status: "pull_request_created".into(),
            branch: Some(result.branch.clone()),
            commit_sha: Some(result.commit.clone()),
            pull_request_number: Some(result.pull_request.number),
            pull_request_url: Some(result.pull_request.url.clone()),
            draft,
            mode: "pull_request".into(),
            completed_at: Some(completed_at.into()),
        }
    }

    fn not_published() -> Self {
        Self {
            status: "not_published".into(),
            branch: None,
            commit_sha: None,
            pull_request_number: None,
            pull_request_url: None,
            draft: false,
            mode: "none".into(),
            completed_at: None,
        }
    }

    pub(super) fn is_published(&self) -> bool {
        self.status == "pull_request_created"
            && self.branch.is_some()
            && self.commit_sha.is_some()
            && self.pull_request_number.is_some()
            && self.pull_request_url.is_some()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct CanonicalTerminalResult {
    pub(super) terminal_result_id: Uuid,
    pub(super) mission_outcome: CanonicalMissionOutcome,
    pub(super) process_health: ProcessHealth,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) failure_category: Option<String>,
    pub(super) reason_code: String,
    pub(super) execution_status: DomainExecutionStatus,
    pub(super) publication: CanonicalPublicationResult,
    pub(super) completion: CompletionEvaluation,
    pub(super) remaining_work: Vec<RemainingWorkItem>,
    pub(super) remaining_evidence_types: Vec<RemainingEvidenceType>,
    pub(super) remaining_work_kinds: Vec<RemainingWorkKind>,
    pub(super) resumability: Resumability,
    pub(super) resumability_decision: ResumabilityDecision,
    pub(super) completed_at: String,
    pub(super) finality: TerminalFinality,
}

impl CanonicalTerminalResult {
    pub(super) const fn process_exit_code(&self) -> i32 {
        match self.process_health {
            ProcessHealth::Healthy | ProcessHealth::Degraded => 0,
            ProcessHealth::Failed => 1,
        }
    }

    pub(super) const fn completion_request_status(&self) -> &'static str {
        match self.execution_status {
            DomainExecutionStatus::Completed => "completed",
            DomainExecutionStatus::AwaitingExternalReview => "awaiting_external_review",
            DomainExecutionStatus::NeedsContinuation => "partial_result",
            DomainExecutionStatus::Blocked => "blocked",
            DomainExecutionStatus::Cancelled => "cancelled",
            DomainExecutionStatus::Failed => "failed",
        }
    }

    pub(super) const fn compatibility_completion_status(&self) -> CompletionStatus {
        match self.mission_outcome {
            CanonicalMissionOutcome::Complete => CompletionStatus::Complete,
            CanonicalMissionOutcome::CompletePendingExternalReview => {
                CompletionStatus::CompletePendingExternalReview
            }
            CanonicalMissionOutcome::PartialReviewable => CompletionStatus::Partial,
            CanonicalMissionOutcome::Blocked => CompletionStatus::Blocked,
            CanonicalMissionOutcome::Cancelled | CanonicalMissionOutcome::Failed => {
                CompletionStatus::Incomplete
            }
        }
    }

    pub(super) fn ui_status(&self) -> &'static str {
        match self.mission_outcome {
            CanonicalMissionOutcome::Complete => "completed",
            CanonicalMissionOutcome::CompletePendingExternalReview => "awaiting_review",
            CanonicalMissionOutcome::PartialReviewable if self.publication.is_published() => {
                "draft_pr_ready"
            }
            CanonicalMissionOutcome::PartialReviewable => "needs_continuation",
            CanonicalMissionOutcome::Blocked => "blocked",
            CanonicalMissionOutcome::Cancelled => "cancelled",
            CanonicalMissionOutcome::Failed => "failed",
        }
    }
}

fn terminal_result_id(execution_id: Uuid) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("rustgrid-terminal:{execution_id}:{TERMINAL_REVISION}").as_bytes(),
    )
}

pub(super) fn canonical_terminal_result_id(execution_id: Uuid) -> Uuid {
    terminal_result_id(execution_id)
}

pub(super) fn terminal_callback_idempotency_key(
    execution_id: Uuid,
    terminal_result_id: Uuid,
    terminal_revision: u64,
) -> Uuid {
    Uuid::new_v5(
        &HOSTED_NAMESPACE,
        &[
            b"terminal-callback:".as_slice(),
            execution_id.as_bytes().as_slice(),
            terminal_result_id.as_bytes().as_slice(),
            terminal_revision.to_be_bytes().as_slice(),
        ]
        .concat(),
    )
}

fn final_execution_callback(
    api: &HostedApiClient,
    canonical: &CanonicalTerminalResult,
    final_notebook_revision: u64,
    sent_at: &str,
) -> FinalExecutionCallback {
    let idempotency_key = terminal_callback_idempotency_key(
        api.execution_id,
        canonical.terminal_result_id,
        canonical.finality.terminal_revision,
    );
    FinalExecutionCallback {
        execution_id: api.execution_id,
        canonical_terminal_result_id: canonical.terminal_result_id,
        terminal_revision: canonical.finality.terminal_revision,
        final_notebook_revision,
        process_exit_code: canonical.process_exit_code(),
        workflow_run_id: api.github_workflow_run_id.to_string(),
        sent_at: sent_at.into(),
        idempotency_key: idempotency_key.to_string(),
    }
}

fn validate_callback_acknowledgement(
    canonical: &CanonicalTerminalResult,
    completion: &CompletionRequest,
    callback: &FinalExecutionCallback,
) -> Result<()> {
    let expected_key = terminal_callback_idempotency_key(
        callback.execution_id,
        canonical.terminal_result_id,
        canonical.finality.terminal_revision,
    );
    if terminal_result_id(callback.execution_id) != canonical.terminal_result_id
        || callback.canonical_terminal_result_id != canonical.terminal_result_id
        || callback.terminal_revision != canonical.finality.terminal_revision
        || callback.process_exit_code != canonical.process_exit_code()
        || callback.idempotency_key != expected_key.to_string()
        || completion.canonical_terminal_result_id != Some(canonical.terminal_result_id)
        || completion.terminal_revision != Some(canonical.finality.terminal_revision)
        || completion.mission_outcome != Some(canonical.compatibility_completion_status())
        || completion.status != canonical.completion_request_status()
    {
        bail!("terminal callback does not acknowledge the finalized canonical result");
    }
    Ok(())
}

fn callback_failure_code(error: &anyhow::Error) -> String {
    error
        .downcast_ref::<HostedHttpError>()
        .map(|failure| failure.effective_code().to_string())
        .unwrap_or_else(|| "terminal_callback_transport_failed".into())
}

fn callback_failure_is_retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<HostedHttpError>()
        .is_none_or(|failure| !failure.invalidates_execution() && retryable_status(failure.status))
}

pub(super) fn deliver_terminal_callback(
    api: &HostedApiClient,
    canonical: &CanonicalTerminalResult,
    completion: &CompletionRequest,
    final_notebook_revision: u64,
    sent_at: &str,
) -> Result<TerminalCallbackDelivery> {
    const MAX_ATTEMPTS: u32 = 3;
    let callback = final_execution_callback(api, canonical, final_notebook_revision, sent_at);
    validate_callback_acknowledgement(canonical, completion, &callback)?;
    let mut envelope = completion.clone();
    envelope.final_callback = Some(callback.clone());
    let payload_hash = sha256_text(&serde_json::to_string(&envelope)?);
    let outbox = TerminalCallbackOutboxEntry {
        execution_id: api.execution_id,
        canonical_terminal_result_id: canonical.terminal_result_id,
        payload_hash: payload_hash.clone(),
        attempts: 0,
        last_error: None,
        acknowledged_at: None,
        created_at: sent_at.into(),
    };
    if let Err(error) = api.append_event(
        "progress",
        json!({
            "event_type": "worker.terminal_callback_outbox_persisted",
            "canonical_terminal_result_id": canonical.terminal_result_id,
            "callback_idempotency_key": callback.idempotency_key,
            "outbox": outbox,
            "callback": callback,
            "completion": envelope,
            "transport": HostedTerminalTransportState {
                callback_status: CallbackStatus::Pending,
                callback_attempts: 0,
                last_callback_error: None,
                workflow_conclusion: None,
                observed_at: sent_at.into(),
            },
        }),
    ) {
        let error_code = callback_failure_code(&error);
        let _ = api.append_event(
            "progress",
            json!({
                "event_type": "worker.terminal_callback_transport_failed",
                "canonical_terminal_result_id": canonical.terminal_result_id,
                "callback_idempotency_key": callback.idempotency_key,
                "attempt": 0,
                "transport_result": error_code,
                "outbox_persisted": false,
                "transport": HostedTerminalTransportState {
                    callback_status: CallbackStatus::FailedTransport,
                    callback_attempts: 0,
                    last_callback_error: Some(error_code),
                    workflow_conclusion: None,
                    observed_at: now_rfc3339(),
                },
                "alert": {
                    "severity": "warning",
                    "category": "hosted_terminal_transport",
                    "code": "final_callback_missing",
                },
            }),
        );
        return Ok(TerminalCallbackDelivery::Pending { attempts: 0 });
    }

    let idempotency_key = terminal_callback_idempotency_key(
        api.execution_id,
        canonical.terminal_result_id,
        canonical.finality.terminal_revision,
    );
    for attempt in 1..=MAX_ATTEMPTS {
        let _ = api.append_event(
            "progress",
            json!({
                "event_type": "worker.terminal_callback_attempted",
                "canonical_terminal_result_id": canonical.terminal_result_id,
                "callback_idempotency_key": callback.idempotency_key,
                "attempt": attempt,
                "payload_hash": payload_hash,
            }),
        );
        match api.complete_once(&envelope, idempotency_key) {
            Ok(_) => {
                let acknowledged_at = now_rfc3339();
                let _ = api.append_event(
                    "progress",
                    json!({
                        "event_type": "worker.terminal_callback_acknowledged",
                        "canonical_terminal_result_id": canonical.terminal_result_id,
                        "callback_idempotency_key": callback.idempotency_key,
                        "attempt": attempt,
                        "acknowledged_at": acknowledged_at,
                        "outbox": TerminalCallbackOutboxEntry {
                            execution_id: api.execution_id,
                            canonical_terminal_result_id: canonical.terminal_result_id,
                            payload_hash: payload_hash.clone(),
                            attempts: attempt,
                            last_error: None,
                            acknowledged_at: Some(acknowledged_at.clone()),
                            created_at: sent_at.into(),
                        },
                        "transport": HostedTerminalTransportState {
                            callback_status: CallbackStatus::Acknowledged,
                            callback_attempts: attempt,
                            last_callback_error: None,
                            workflow_conclusion: None,
                            observed_at: acknowledged_at,
                        },
                    }),
                );
                return Ok(TerminalCallbackDelivery::Acknowledged { attempts: attempt });
            }
            Err(error) => {
                let error_code = callback_failure_code(&error);
                if attempt < MAX_ATTEMPTS && callback_failure_is_retryable(&error) {
                    let delay = retry_delay((attempt - 1) as usize);
                    let _ = api.append_event(
                        "progress",
                        json!({
                            "event_type": "worker.terminal_callback_retry_scheduled",
                            "canonical_terminal_result_id": canonical.terminal_result_id,
                            "callback_idempotency_key": callback.idempotency_key,
                            "attempt": attempt,
                            "next_attempt": attempt + 1,
                            "delay_milliseconds": delay.as_millis(),
                            "transport_result": error_code,
                            "outbox": TerminalCallbackOutboxEntry {
                                execution_id: api.execution_id,
                                canonical_terminal_result_id: canonical.terminal_result_id,
                                payload_hash: payload_hash.clone(),
                                attempts: attempt,
                                last_error: Some(error_code.clone()),
                                acknowledged_at: None,
                                created_at: sent_at.into(),
                            },
                        }),
                    );
                    api.clock.sleep(delay);
                    continue;
                }
                let observed_at = now_rfc3339();
                let transport = HostedTerminalTransportState {
                    callback_status: CallbackStatus::FailedTransport,
                    callback_attempts: attempt,
                    last_callback_error: Some(error_code.clone()),
                    workflow_conclusion: None,
                    observed_at: observed_at.clone(),
                };
                let _ = api.append_event(
                    "progress",
                    json!({
                        "event_type": "worker.terminal_callback_transport_failed",
                        "canonical_terminal_result_id": canonical.terminal_result_id,
                        "callback_idempotency_key": callback.idempotency_key,
                        "attempt": attempt,
                        "transport_result": error_code,
                        "outbox": TerminalCallbackOutboxEntry {
                            execution_id: api.execution_id,
                            canonical_terminal_result_id: canonical.terminal_result_id,
                            payload_hash: payload_hash.clone(),
                            attempts: attempt,
                            last_error: Some(error_code.clone()),
                            acknowledged_at: None,
                            created_at: sent_at.into(),
                        },
                        "transport": transport,
                        "alert": {
                            "severity": "warning",
                            "category": "hosted_terminal_transport",
                            "code": "final_callback_missing",
                        },
                    }),
                );
                let _ = api.append_event(
                    "progress",
                    json!({
                        "event_type": "execution.callback_missing_but_domain_preserved",
                        "execution_id": api.execution_id,
                        "canonical_terminal_result_id": canonical.terminal_result_id,
                        "callback_idempotency_key": callback.idempotency_key,
                        "reconciliation_authority": TerminalEvidenceSource::CanonicalWorkerResult,
                        "mission_outcome": canonical.mission_outcome,
                        "domain_execution_status": canonical.execution_status,
                        "infrastructure_health": "degraded",
                        "anomaly_code": "final_callback_missing",
                    }),
                );
                return Ok(TerminalCallbackDelivery::Pending { attempts: attempt });
            }
        }
    }
    unreachable!("bounded terminal callback loop always returns")
}

pub(super) fn recover_persisted_terminal_callback(
    api: &HostedApiClient,
    manifest: &HostedManifest,
) -> Result<bool> {
    let execution = &manifest.execution;
    let Some(canonical_value) = execution.canonical_terminal_result.as_ref() else {
        return Ok(false);
    };
    let canonical: CanonicalTerminalResult = serde_json::from_value(canonical_value.clone())
        .context("persisted canonical terminal result is invalid")?;
    if execution.canonical_terminal_result_id != Some(canonical.terminal_result_id)
        || execution.terminal_revision != i64::try_from(canonical.finality.terminal_revision).ok()
        || execution.terminal_authority.as_deref() != Some("worker_domain")
    {
        bail!("persisted canonical terminal identity is inconsistent");
    }
    let github = execution.github_actions.as_ref();
    if github.and_then(|state| state.callback_status.as_deref()) == Some("acknowledged") {
        println!(
            "[complete] Execution {} already has an acknowledged canonical terminal result",
            api.execution_id
        );
        return Ok(true);
    }
    let Some(outbox) = github.and_then(|state| state.callback_outbox.as_ref()) else {
        eprintln!(
            "[warning] execution {} has a canonical terminal result but no recoverable callback outbox",
            api.execution_id
        );
        return Ok(true);
    };
    let mut completion: CompletionRequest = outbox
        .get("completion")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .context("persisted terminal callback completion envelope is invalid")?
        .ok_or_else(|| anyhow!("persisted terminal callback has no completion envelope"))?;
    let callback = completion
        .final_callback
        .clone()
        .or_else(|| {
            outbox
                .get("callback")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
        })
        .ok_or_else(|| anyhow!("persisted terminal callback metadata is missing"))?;
    completion.final_callback = Some(callback.clone());
    validate_callback_acknowledgement(&canonical, &completion, &callback)?;
    let idempotency_key = terminal_callback_idempotency_key(
        api.execution_id,
        canonical.terminal_result_id,
        canonical.finality.terminal_revision,
    );
    for attempt in 1..=3_u32 {
        let _ = api.append_event(
            "progress",
            json!({
                "event_type": "worker.terminal_callback_attempted",
                "canonical_terminal_result_id": canonical.terminal_result_id,
                "callback_idempotency_key": callback.idempotency_key,
                "attempt": attempt,
                "recovered_from_durable_outbox": true,
            }),
        );
        match api.complete_once(&completion, idempotency_key) {
            Ok(_) => {
                println!(
                    "[complete] Execution {} recovered its persisted terminal callback",
                    api.execution_id
                );
                return Ok(true);
            }
            Err(error) if attempt < 3 && callback_failure_is_retryable(&error) => {
                api.clock.sleep(retry_delay((attempt - 1) as usize));
            }
            Err(error) => {
                eprintln!(
                    "[warning] execution {} preserved its canonical result, but recovered callback delivery remains pending: {}",
                    api.execution_id,
                    callback_failure_code(&error)
                );
                return Ok(true);
            }
        }
    }
    unreachable!("bounded recovered callback loop always returns")
}

fn remaining_evidence_types(completion: &CompletionEvaluation) -> Vec<RemainingEvidenceType> {
    let mut types = Vec::new();
    let mut push = |value| {
        if !types.contains(&value) {
            types.push(value);
        }
    };
    if !completion.pending_external_review.is_empty()
        || completion
            .criteria
            .iter()
            .any(|criterion| criterion.status == CriterionStatus::ExternalReviewRequired)
    {
        push(RemainingEvidenceType::ExternalApproval);
    }
    if completion.review_checklist.iter().any(|item| {
        item.status != "completed"
            && matches!(
                item.r#type,
                VerificationType::ManualQa
                    | VerificationType::AccessibilityReview
                    | VerificationType::VisualReview
                    | VerificationType::DeploymentEnvironment
            )
    }) {
        push(RemainingEvidenceType::ManualInspection);
    }
    if !completion.remaining_automated_verification.is_empty() {
        push(RemainingEvidenceType::MissingAutomatedValidation);
    }
    if completion.implementation_completeness != ImplementationCompleteness::Complete
        || !completion.remaining_implementation_work.is_empty()
    {
        push(RemainingEvidenceType::UnresolvedImplementation);
    }
    if !completion.unrecovered_tool_failures.is_empty() {
        push(RemainingEvidenceType::InfrastructureIncomplete);
    }
    types
}

fn remaining_work_kinds(
    completion: &CompletionEvaluation,
    validation: &[ValidationResult],
) -> Vec<RemainingWorkKind> {
    let mut kinds = Vec::new();
    let mut push = |value| {
        if !kinds.contains(&value) {
            kinds.push(value);
        }
    };
    if !completion.pending_external_review.is_empty()
        || completion
            .criteria
            .iter()
            .any(|criterion| criterion.status == CriterionStatus::ExternalReviewRequired)
    {
        push(RemainingWorkKind::ExternalApproval);
    }
    if completion.review_checklist.iter().any(|item| {
        item.status != "completed"
            && matches!(
                item.r#type,
                VerificationType::ManualQa
                    | VerificationType::AccessibilityReview
                    | VerificationType::VisualReview
                    | VerificationType::DeploymentEnvironment
            )
    }) {
        push(RemainingWorkKind::ManualVisualInspection);
    }
    if completion.implementation_completeness != ImplementationCompleteness::Complete
        || !completion.remaining_implementation_work.is_empty()
    {
        push(RemainingWorkKind::MissingImplementation);
    }
    if validation
        .iter()
        .any(|result| matches!(result.status.as_str(), "failed" | "failed_code"))
    {
        push(RemainingWorkKind::FailedValidation);
    }
    if !completion.remaining_automated_verification.is_empty()
        || validation.iter().any(|result| result.status != "passed")
    {
        push(RemainingWorkKind::MissingAutomatedValidation);
    }
    if !completion.unrecovered_tool_failures.is_empty() {
        push(RemainingWorkKind::InfrastructureFollowUp);
    }
    kinds
}

fn normalized_completion(
    mut completion: CompletionEvaluation,
    outcome: CanonicalMissionOutcome,
) -> CompletionEvaluation {
    completion.status = match outcome {
        CanonicalMissionOutcome::Complete => CompletionStatus::Complete,
        CanonicalMissionOutcome::CompletePendingExternalReview => {
            CompletionStatus::CompletePendingExternalReview
        }
        CanonicalMissionOutcome::PartialReviewable => CompletionStatus::Partial,
        CanonicalMissionOutcome::Blocked => CompletionStatus::Blocked,
        CanonicalMissionOutcome::Cancelled | CanonicalMissionOutcome::Failed => {
            CompletionStatus::Incomplete
        }
    };
    completion
}

pub(super) fn resolve_published_terminal_result(
    execution_id: Uuid,
    result: &HostedResult,
    completed_at: &str,
) -> CanonicalTerminalResult {
    let remaining_evidence_types = remaining_evidence_types(&result.completeness);
    let remaining_work_kinds = remaining_work_kinds(&result.completeness, &result.validation);
    let automated_gates_passed = result
        .validation
        .iter()
        .all(|validation| validation.status == "passed");
    let engineering_work_remains = remaining_work_kinds.iter().any(|kind| {
        matches!(
            kind,
            RemainingWorkKind::MissingImplementation
                | RemainingWorkKind::FailedValidation
                | RemainingWorkKind::MissingAutomatedValidation
                | RemainingWorkKind::InfrastructureFollowUp
        )
    }) || !automated_gates_passed;
    let external_only = !remaining_work_kinds.is_empty()
        && remaining_work_kinds.iter().all(|kind| {
            matches!(
                kind,
                RemainingWorkKind::ExternalApproval | RemainingWorkKind::ManualVisualInspection
            )
        });
    let mission_outcome = if !engineering_work_remains
        && (external_only
            || result.completeness.status == CompletionStatus::CompletePendingExternalReview)
    {
        CanonicalMissionOutcome::CompletePendingExternalReview
    } else if !engineering_work_remains
        && result.completeness.implementation_completeness == ImplementationCompleteness::Complete
        && result.completeness.status == CompletionStatus::Complete
    {
        CanonicalMissionOutcome::Complete
    } else {
        // A HostedResult contains a successfully created or recovered pull
        // request. Strong publication evidence therefore resolves ambiguous,
        // incomplete, blocked, and uncertain evaluations as reviewable partial
        // work instead of a process failure.
        CanonicalMissionOutcome::PartialReviewable
    };
    let draft = mission_outcome != CanonicalMissionOutcome::Complete;
    let publication = CanonicalPublicationResult::published(result, completed_at, draft);
    let (default_reason_code, execution_status, resumability) = match mission_outcome {
        CanonicalMissionOutcome::Complete => (
            "completed",
            DomainExecutionStatus::Completed,
            Resumability::NotResumable,
        ),
        CanonicalMissionOutcome::CompletePendingExternalReview => (
            "external_review_required",
            DomainExecutionStatus::AwaitingExternalReview,
            Resumability::AwaitingExternalReview,
        ),
        CanonicalMissionOutcome::PartialReviewable => (
            "partial_reviewable",
            DomainExecutionStatus::NeedsContinuation,
            Resumability::Resumable {
                resume_phase: Some("implementation".into()),
            },
        ),
        _ => unreachable!("published terminal resolution has three legal outcomes"),
    };
    let reason_code = result
        .terminal_telemetry
        .phase_persistence_failure_code
        .as_deref()
        .unwrap_or(default_reason_code);
    let resumability_decision = ResumabilityDecision {
        status: resumability.clone(),
        reason_code: reason_code.into(),
        resume_from_node: None,
        repository_fingerprint: String::new(),
    };
    let terminal_result_id = terminal_result_id(execution_id);
    CanonicalTerminalResult {
        terminal_result_id,
        mission_outcome,
        process_health: if result
            .terminal_telemetry
            .phase_persistence_failure_code
            .is_some()
        {
            ProcessHealth::Degraded
        } else {
            ProcessHealth::Healthy
        },
        failure_category: None,
        reason_code: reason_code.into(),
        execution_status,
        publication,
        completion: normalized_completion(result.completeness.clone(), mission_outcome),
        remaining_work: result.terminal_telemetry.remaining_work.clone(),
        remaining_evidence_types,
        remaining_work_kinds,
        resumability,
        resumability_decision,
        completed_at: completed_at.into(),
        finality: TerminalFinality {
            terminal_result_id,
            terminal_revision: TERMINAL_REVISION,
            authority: TerminalAuthority::WorkerDomain,
            finalized_at: completed_at.into(),
        },
    }
}

pub(super) fn resolve_blocked_terminal_result(
    execution_id: Uuid,
    reason_code: &str,
    remaining_work: Vec<RemainingWorkItem>,
    resume_phase: &str,
    completion: CompletionEvaluation,
    completed_at: &str,
) -> CanonicalTerminalResult {
    let terminal_result_id = terminal_result_id(execution_id);
    let resumability = Resumability::Resumable {
        resume_phase: Some(resume_phase.into()),
    };
    let resumability_decision = ResumabilityDecision {
        status: resumability.clone(),
        reason_code: reason_code.into(),
        resume_from_node: None,
        repository_fingerprint: String::new(),
    };
    CanonicalTerminalResult {
        terminal_result_id,
        mission_outcome: CanonicalMissionOutcome::Blocked,
        process_health: ProcessHealth::Healthy,
        failure_category: None,
        reason_code: reason_code.into(),
        execution_status: DomainExecutionStatus::Blocked,
        publication: CanonicalPublicationResult::not_published(),
        completion: normalized_completion(completion, CanonicalMissionOutcome::Blocked),
        remaining_work,
        remaining_evidence_types: vec![RemainingEvidenceType::UnresolvedImplementation],
        remaining_work_kinds: vec![RemainingWorkKind::MissingImplementation],
        resumability,
        resumability_decision,
        completed_at: completed_at.into(),
        finality: TerminalFinality {
            terminal_result_id,
            terminal_revision: TERMINAL_REVISION,
            authority: TerminalAuthority::WorkerDomain,
            finalized_at: completed_at.into(),
        },
    }
}

pub(super) fn resolve_unsuccessful_terminal_result(
    execution_id: Uuid,
    cancelled: bool,
    reason_code: &str,
    failure_category: &str,
    safe_summary: &str,
    completed_at: &str,
    resumability_decision: ResumabilityDecision,
) -> CanonicalTerminalResult {
    let terminal_result_id = terminal_result_id(execution_id);
    let mission_outcome = if cancelled {
        CanonicalMissionOutcome::Cancelled
    } else {
        CanonicalMissionOutcome::Failed
    };
    let process_health = if cancelled {
        ProcessHealth::Healthy
    } else {
        ProcessHealth::Failed
    };
    let execution_status = if cancelled {
        DomainExecutionStatus::Cancelled
    } else {
        DomainExecutionStatus::Failed
    };
    let resumability = resumability_decision.status.clone();
    CanonicalTerminalResult {
        terminal_result_id,
        mission_outcome,
        process_health,
        failure_category: Some(failure_category.into()),
        reason_code: reason_code.into(),
        execution_status,
        publication: CanonicalPublicationResult::not_published(),
        completion: CompletionEvaluation {
            status: CompletionStatus::Incomplete,
            implementation_completeness: ImplementationCompleteness::Incomplete,
            verification_readiness: VerificationReadiness::Blocked,
            evaluation_source: EvaluationSource::OrchestratorFallback,
            confidence: 1.0,
            criteria: Vec::new(),
            remaining_implementation_work: Vec::new(),
            remaining_automated_verification: Vec::new(),
            pending_external_review: Vec::new(),
            optional_follow_up: Vec::new(),
            review_checklist: Vec::new(),
            unrecovered_tool_failures: (!cancelled)
                .then(|| safe_summary.into())
                .into_iter()
                .collect(),
            summary: safe_summary.into(),
        },
        remaining_work: Vec::new(),
        remaining_evidence_types: vec![if cancelled {
            RemainingEvidenceType::UnresolvedImplementation
        } else {
            RemainingEvidenceType::InfrastructureIncomplete
        }],
        remaining_work_kinds: vec![if cancelled {
            RemainingWorkKind::MissingImplementation
        } else {
            RemainingWorkKind::InfrastructureFollowUp
        }],
        resumability,
        resumability_decision,
        completed_at: completed_at.into(),
        finality: TerminalFinality {
            terminal_result_id,
            terminal_revision: TERMINAL_REVISION,
            authority: TerminalAuthority::WorkerDomain,
            finalized_at: completed_at.into(),
        },
    }
}

pub(super) fn resolve_failure_resumability(
    error: &anyhow::Error,
    cancelled: bool,
    reason_code: &str,
) -> ResumabilityDecision {
    if cancelled {
        return ResumabilityDecision {
            status: Resumability::Resumable {
                resume_phase: Some("implementation".into()),
            },
            reason_code: reason_code.into(),
            resume_from_node: None,
            repository_fingerprint: String::new(),
        };
    }
    if let Some(failure) = error.downcast_ref::<HostedAgentExecutionFailure>() {
        return ResumabilityDecision {
            status: if failure.resumable {
                Resumability::Resumable {
                    resume_phase: Some(failure.resume_phase.clone()),
                }
            } else {
                Resumability::NotResumable
            },
            reason_code: failure.code.clone(),
            resume_from_node: failure.resume_from_node.clone(),
            repository_fingerprint: failure.repository_fingerprint.clone(),
        };
    }
    if let Some(failure) = error.downcast_ref::<HostedInvariantFailure>() {
        return ResumabilityDecision {
            status: Resumability::Resumable {
                resume_phase: Some(failure.phase.into()),
            },
            reason_code: failure.code.into(),
            resume_from_node: failure.resume_from_node.clone(),
            repository_fingerprint: String::new(),
        };
    }
    if let Some(failure) =
        error.downcast_ref::<crate::hosted_orchestrator::OrchestrationInvariantError>()
    {
        return ResumabilityDecision {
            status: Resumability::Resumable {
                resume_phase: Some("implementation".into()),
            },
            reason_code: failure.code.clone(),
            resume_from_node: failure.node_id.as_ref().map(ToString::to_string),
            repository_fingerprint: String::new(),
        };
    }
    ResumabilityDecision {
        status: Resumability::NotResumable,
        reason_code: reason_code.into(),
        resume_from_node: None,
        repository_fingerprint: String::new(),
    }
}

pub(super) fn resolve_infrastructure_fallback_terminal_result(
    execution_id: Uuid,
    reason_code: &str,
    safe_summary: &str,
    completed_at: &str,
) -> CanonicalTerminalResult {
    let mut terminal = resolve_unsuccessful_terminal_result(
        execution_id,
        false,
        reason_code,
        "infrastructure_failure",
        safe_summary,
        completed_at,
        ResumabilityDecision {
            status: Resumability::NotResumable,
            reason_code: reason_code.into(),
            resume_from_node: None,
            repository_fingerprint: String::new(),
        },
    );
    terminal.finality.authority = TerminalAuthority::InfrastructureFallback;
    terminal
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct InfrastructureTerminalMetadata {
    pub(super) provider: String,
    pub(super) workflow_run_id: Option<String>,
    pub(super) workflow_job_id: Option<String>,
    pub(super) workflow_status: String,
    pub(super) workflow_conclusion: Option<String>,
    pub(super) runner_name: Option<String>,
    pub(super) observed_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum InfrastructureReconciliationDecision {
    DomainResultPreserved,
    DomainFailureConfirmed,
    InfrastructureFallbackRequired,
    AwaitingGracePeriod,
    LostWithoutTerminalProof,
    TerminalStateCorruption,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct InfrastructureReconciliationResult {
    pub(super) decision: InfrastructureReconciliationDecision,
    pub(super) infrastructure: InfrastructureTerminalMetadata,
    pub(super) anomaly_code: Option<&'static str>,
    pub(super) terminal_result_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct TerminalReconciliation {
    pub(super) decision: InfrastructureReconciliationDecision,
    pub(super) authority: TerminalEvidenceSource,
    pub(super) anomaly_code: Option<&'static str>,
    pub(super) terminal_result_id: Option<Uuid>,
    pub(super) domain_status_preserved: bool,
    pub(super) infrastructure_health: ProcessHealth,
}

fn strongest_terminal_evidence(
    sources: impl IntoIterator<Item = TerminalEvidenceSource>,
) -> TerminalEvidenceSource {
    sources
        .into_iter()
        .filter_map(|source| source.precedence().map(|precedence| (source, precedence)))
        .max_by_key(|(_, precedence)| *precedence)
        .map(|(source, _)| source)
        .unwrap_or(TerminalEvidenceSource::LeaseExpiration)
}

pub(super) fn reconcile_terminal_execution(
    canonical: Option<&CanonicalTerminalResult>,
    canonical_corrupt: bool,
    callback_status: CallbackStatus,
    infrastructure: &InfrastructureTerminalMetadata,
    lease_expired: bool,
    grace_period_elapsed: bool,
    resumable_worker_active: bool,
) -> TerminalReconciliation {
    if let Some(canonical) = canonical {
        let callback_missing = matches!(
            callback_status,
            CallbackStatus::Missing | CallbackStatus::FailedTransport
        );
        return TerminalReconciliation {
            decision: InfrastructureReconciliationDecision::DomainResultPreserved,
            authority: strongest_terminal_evidence([
                TerminalEvidenceSource::CanonicalWorkerResult,
                TerminalEvidenceSource::WorkflowConclusion,
            ]),
            anomaly_code: callback_missing.then_some("final_callback_missing"),
            terminal_result_id: Some(canonical.terminal_result_id),
            domain_status_preserved: true,
            infrastructure_health: if callback_missing {
                ProcessHealth::Degraded
            } else {
                canonical.process_health
            },
        };
    }
    if canonical_corrupt || callback_status == CallbackStatus::Acknowledged {
        return TerminalReconciliation {
            decision: InfrastructureReconciliationDecision::TerminalStateCorruption,
            authority: if callback_status == CallbackStatus::Acknowledged {
                TerminalEvidenceSource::FinalCallback
            } else {
                TerminalEvidenceSource::WorkflowConclusion
            },
            anomaly_code: Some("terminal_state_corruption"),
            terminal_result_id: None,
            domain_status_preserved: false,
            infrastructure_health: ProcessHealth::Failed,
        };
    }
    let workflow_terminal = infrastructure.workflow_status == "completed"
        || infrastructure.workflow_conclusion.is_some();
    if workflow_terminal && !grace_period_elapsed {
        return TerminalReconciliation {
            decision: InfrastructureReconciliationDecision::AwaitingGracePeriod,
            authority: TerminalEvidenceSource::WorkflowConclusion,
            anomaly_code: None,
            terminal_result_id: None,
            domain_status_preserved: false,
            infrastructure_health: ProcessHealth::Degraded,
        };
    }
    if infrastructure.workflow_conclusion.as_deref() == Some("failure") {
        return TerminalReconciliation {
            decision: InfrastructureReconciliationDecision::InfrastructureFallbackRequired,
            authority: TerminalEvidenceSource::WorkflowConclusion,
            anomaly_code: Some("workflow_failed_without_terminal_result"),
            terminal_result_id: None,
            domain_status_preserved: false,
            infrastructure_health: ProcessHealth::Failed,
        };
    }
    if (workflow_terminal || lease_expired) && grace_period_elapsed && !resumable_worker_active {
        return TerminalReconciliation {
            decision: InfrastructureReconciliationDecision::LostWithoutTerminalProof,
            authority: strongest_terminal_evidence(
                workflow_terminal
                    .then_some(TerminalEvidenceSource::WorkflowConclusion)
                    .into_iter()
                    .chain(lease_expired.then_some(TerminalEvidenceSource::LeaseExpiration)),
            ),
            anomaly_code: Some("terminal_proof_missing"),
            terminal_result_id: None,
            domain_status_preserved: false,
            infrastructure_health: ProcessHealth::Failed,
        };
    }
    TerminalReconciliation {
        decision: InfrastructureReconciliationDecision::AwaitingGracePeriod,
        authority: if workflow_terminal {
            TerminalEvidenceSource::WorkflowConclusion
        } else {
            TerminalEvidenceSource::LeaseExpiration
        },
        anomaly_code: None,
        terminal_result_id: None,
        domain_status_preserved: false,
        infrastructure_health: ProcessHealth::Degraded,
    }
}

pub(super) fn reconcile_infrastructure_terminal(
    canonical: Option<&CanonicalTerminalResult>,
    infrastructure: InfrastructureTerminalMetadata,
) -> InfrastructureReconciliationResult {
    if let Some(canonical) = canonical {
        let policy = reconcile_terminal_execution(
            Some(canonical),
            false,
            CallbackStatus::Acknowledged,
            &infrastructure,
            false,
            true,
            false,
        );
        debug_assert_eq!(
            policy.decision,
            InfrastructureReconciliationDecision::DomainResultPreserved
        );
    }
    let workflow_failed = infrastructure.workflow_conclusion.as_deref() == Some("failure");
    match canonical {
        Some(canonical) if workflow_failed && canonical.process_health != ProcessHealth::Failed => {
            debug_assert_eq!(
                reconcile_terminal_replacement(
                    canonical,
                    TerminalAuthority::InfrastructureFallback
                ),
                TerminalReplacementDecision::RejectedFinalWorkerDomain
            );
            InfrastructureReconciliationResult {
                decision: InfrastructureReconciliationDecision::DomainResultPreserved,
                infrastructure,
                anomaly_code: Some("workflow_conclusion_conflicts_with_domain_result"),
                terminal_result_id: Some(canonical.terminal_result_id),
            }
        }
        Some(canonical) if workflow_failed => InfrastructureReconciliationResult {
            decision: InfrastructureReconciliationDecision::DomainFailureConfirmed,
            infrastructure,
            anomaly_code: None,
            terminal_result_id: Some(canonical.terminal_result_id),
        },
        Some(canonical) => InfrastructureReconciliationResult {
            decision: InfrastructureReconciliationDecision::DomainResultPreserved,
            infrastructure,
            anomaly_code: None,
            terminal_result_id: Some(canonical.terminal_result_id),
        },
        None => InfrastructureReconciliationResult {
            decision: InfrastructureReconciliationDecision::InfrastructureFallbackRequired,
            infrastructure,
            anomaly_code: None,
            terminal_result_id: None,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalReplacementDecision {
    RejectedFinalWorkerDomain,
    AdministrativeOverrideAccepted,
}

pub(super) fn reconcile_terminal_replacement(
    existing: &CanonicalTerminalResult,
    replacement_authority: TerminalAuthority,
) -> TerminalReplacementDecision {
    if existing.finality.authority == TerminalAuthority::WorkerDomain
        && replacement_authority != TerminalAuthority::AdministrativeOverride
    {
        TerminalReplacementDecision::RejectedFinalWorkerDomain
    } else {
        TerminalReplacementDecision::AdministrativeOverrideAccepted
    }
}

pub(super) fn record_noncritical_post_publication_failure(
    canonical: &mut CanonicalTerminalResult,
    _failure_layer: &str,
) {
    if canonical.publication.is_published() && canonical.process_health == ProcessHealth::Healthy {
        canonical.process_health = ProcessHealth::Degraded;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(status: CompletionStatus) -> CompletionEvaluation {
        let implementation_completeness = match status {
            CompletionStatus::Complete | CompletionStatus::CompletePendingExternalReview => {
                ImplementationCompleteness::Complete
            }
            CompletionStatus::Partial => ImplementationCompleteness::Partial,
            CompletionStatus::Incomplete
            | CompletionStatus::Blocked
            | CompletionStatus::Uncertain => ImplementationCompleteness::Incomplete,
        };
        CompletionEvaluation {
            status,
            implementation_completeness,
            verification_readiness: VerificationReadiness::Blocked,
            evaluation_source: EvaluationSource::OrchestratorFallback,
            confidence: 1.0,
            criteria: Vec::new(),
            remaining_implementation_work: Vec::new(),
            remaining_automated_verification: Vec::new(),
            pending_external_review: Vec::new(),
            optional_follow_up: Vec::new(),
            review_checklist: Vec::new(),
            unrecovered_tool_failures: Vec::new(),
            summary: "Deterministic terminal fixture".into(),
        }
    }

    fn published(completion: CompletionEvaluation) -> HostedResult {
        HostedResult {
            summary: "Review the published changes.".into(),
            branch: "rustgrid/generic-terminal-fixture".into(),
            commit: "a".repeat(40),
            pull_request: PullRequestResult {
                number: 41,
                url: "https://github.example/repository/pull/41".into(),
            },
            validation: vec![ValidationResult {
                id: "required".into(),
                command: "project-check".into(),
                status: "passed".into(),
                output: String::new(),
            }],
            completeness: completion,
            terminal_telemetry: TerminalTelemetry::default(),
        }
    }

    fn infrastructure(conclusion: &str) -> InfrastructureTerminalMetadata {
        InfrastructureTerminalMetadata {
            provider: "github_actions".into(),
            workflow_run_id: Some("100".into()),
            workflow_job_id: Some("200".into()),
            workflow_status: "completed".into(),
            workflow_conclusion: Some(conclusion.into()),
            runner_name: Some("runner".into()),
            observed_at: "2026-08-04T00:00:00Z".into(),
        }
    }

    #[test]
    fn healthy_non_complete_domain_results_exit_zero() {
        let partial = resolve_published_terminal_result(
            Uuid::nil(),
            &published(completion(CompletionStatus::Partial)),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            partial.mission_outcome,
            CanonicalMissionOutcome::PartialReviewable
        );
        assert_eq!(partial.process_health, ProcessHealth::Healthy);
        assert_eq!(partial.process_exit_code(), 0);
        assert_eq!(partial.ui_status(), "draft_pr_ready");

        let mut external = completion(CompletionStatus::Uncertain);
        external.implementation_completeness = ImplementationCompleteness::Complete;
        external.pending_external_review = vec!["A reviewer approves the result.".into()];
        let pending = resolve_published_terminal_result(
            Uuid::nil(),
            &published(external),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            pending.mission_outcome,
            CanonicalMissionOutcome::CompletePendingExternalReview
        );
        assert_eq!(pending.process_exit_code(), 0);
        assert_eq!(
            pending.completion.status,
            CompletionStatus::CompletePendingExternalReview
        );

        let blocked = resolve_blocked_terminal_result(
            Uuid::nil(),
            "safe_blocker_recorded",
            vec![RemainingWorkItem {
                change_id: "change".into(),
                path: "src/module.ext".into(),
                role: "source".into(),
                status: IntendedChangeStatus::Unresolved,
                reason: "Safe blocker".into(),
            }],
            "implementation",
            completion(CompletionStatus::Blocked),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(blocked.mission_outcome, CanonicalMissionOutcome::Blocked);
        assert!(blocked.resumability.is_resumable());
        assert_eq!(blocked.process_exit_code(), 0);
    }

    #[test]
    fn genuine_process_failure_is_the_only_nonzero_terminal_health() {
        let failed = resolve_unsuccessful_terminal_result(
            Uuid::nil(),
            false,
            "runner_failed",
            "orchestration_execution_failed",
            "The runner could not execute reliably.",
            "2026-08-04T00:00:00Z",
            ResumabilityDecision {
                status: Resumability::NotResumable,
                reason_code: "runner_failed".into(),
                resume_from_node: None,
                repository_fingerprint: String::new(),
            },
        );
        assert_eq!(failed.mission_outcome, CanonicalMissionOutcome::Failed);
        assert_eq!(failed.process_health, ProcessHealth::Failed);
        assert_eq!(failed.process_exit_code(), 1);

        let cancelled = resolve_unsuccessful_terminal_result(
            Uuid::nil(),
            true,
            "cancelled",
            "cancelled",
            "Cancellation was persisted.",
            "2026-08-04T00:00:00Z",
            ResumabilityDecision {
                status: Resumability::Resumable {
                    resume_phase: Some("implementation".into()),
                },
                reason_code: "cancelled".into(),
                resume_from_node: None,
                repository_fingerprint: String::new(),
            },
        );
        assert_eq!(
            cancelled.mission_outcome,
            CanonicalMissionOutcome::Cancelled
        );
        assert_eq!(cancelled.process_health, ProcessHealth::Healthy);
        assert_eq!(cancelled.process_exit_code(), 0);
    }

    #[test]
    fn deterministic_evidence_replaces_uncertain_with_stronger_outcomes() {
        let mut external = completion(CompletionStatus::Uncertain);
        external.implementation_completeness = ImplementationCompleteness::Complete;
        external.pending_external_review = vec!["Manual approval".into()];
        let external = resolve_published_terminal_result(
            Uuid::nil(),
            &published(external),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            external.remaining_evidence_types,
            vec![RemainingEvidenceType::ExternalApproval]
        );
        assert_eq!(
            external.mission_outcome,
            CanonicalMissionOutcome::CompletePendingExternalReview
        );

        let mut automated = completion(CompletionStatus::Uncertain);
        automated.implementation_completeness = ImplementationCompleteness::Complete;
        automated.remaining_automated_verification = vec!["Run required check".into()];
        let automated = resolve_published_terminal_result(
            Uuid::nil(),
            &published(automated),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            automated.mission_outcome,
            CanonicalMissionOutcome::PartialReviewable
        );
        assert_eq!(automated.completion.status, CompletionStatus::Partial);
        assert_ne!(automated.completion.status, CompletionStatus::Uncertain);
    }

    #[test]
    fn workflow_failure_annotates_but_does_not_replace_worker_domain_result() {
        let canonical = resolve_published_terminal_result(
            Uuid::nil(),
            &published(completion(CompletionStatus::Partial)),
            "2026-08-04T00:00:00Z",
        );
        let publication = canonical.publication.clone();
        let reconciled =
            reconcile_infrastructure_terminal(Some(&canonical), infrastructure("failure"));
        assert_eq!(
            reconciled.decision,
            InfrastructureReconciliationDecision::DomainResultPreserved
        );
        assert_eq!(
            reconciled.anomaly_code,
            Some("workflow_conclusion_conflicts_with_domain_result")
        );
        assert_eq!(
            reconciled.infrastructure.workflow_run_id.as_deref(),
            Some("100")
        );
        assert_eq!(canonical.publication, publication);
        assert_eq!(canonical.ui_status(), "draft_pr_ready");
    }

    #[test]
    fn missing_worker_result_allows_infrastructure_fallback_failure() {
        let reconciled = reconcile_infrastructure_terminal(None, infrastructure("failure"));
        assert_eq!(
            reconciled.decision,
            InfrastructureReconciliationDecision::InfrastructureFallbackRequired
        );
        assert_eq!(reconciled.terminal_result_id, None);
        let fallback = resolve_infrastructure_fallback_terminal_result(
            Uuid::nil(),
            "workflow_failed",
            "No worker-domain result exists.",
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            fallback.finality.authority,
            TerminalAuthority::InfrastructureFallback
        );
        assert_eq!(fallback.process_exit_code(), 1);
    }

    #[test]
    fn finalized_worker_result_requires_explicit_administrative_override() {
        let canonical = resolve_published_terminal_result(
            Uuid::nil(),
            &published(completion(CompletionStatus::Complete)),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            reconcile_terminal_replacement(&canonical, TerminalAuthority::InfrastructureFallback),
            TerminalReplacementDecision::RejectedFinalWorkerDomain
        );
        assert_eq!(
            reconcile_terminal_replacement(&canonical, TerminalAuthority::AdministrativeOverride),
            TerminalReplacementDecision::AdministrativeOverrideAccepted
        );
    }

    #[test]
    fn noncritical_post_publication_failure_degrades_health_only() {
        let mut canonical = resolve_published_terminal_result(
            Uuid::nil(),
            &published(completion(CompletionStatus::Partial)),
            "2026-08-04T00:00:00Z",
        );
        let outcome = canonical.mission_outcome;
        let publication = canonical.publication.clone();
        record_noncritical_post_publication_failure(&mut canonical, "cleanup_failed");
        assert_eq!(canonical.process_health, ProcessHealth::Degraded);
        assert_eq!(canonical.mission_outcome, outcome);
        assert_eq!(canonical.publication, publication);
        assert_eq!(canonical.process_exit_code(), 0);
    }

    #[test]
    fn phase_persistence_degradation_is_preserved_in_the_canonical_terminal_result() {
        let mut result = published(completion(CompletionStatus::Partial));
        result.terminal_telemetry.phase_persistence_failure_code =
            Some("phase_transition_persistence_failed".into());
        let canonical =
            resolve_published_terminal_result(Uuid::nil(), &result, "2026-08-04T00:00:00Z");
        assert_eq!(canonical.process_health, ProcessHealth::Degraded);
        assert_eq!(
            canonical.mission_outcome,
            CanonicalMissionOutcome::PartialReviewable
        );
        assert_eq!(canonical.reason_code, "phase_transition_persistence_failed");
        assert_eq!(canonical.process_exit_code(), 0);
    }

    #[test]
    fn finalized_canonical_result_is_terminal_proof_when_callback_is_missing() {
        let canonical = resolve_published_terminal_result(
            Uuid::nil(),
            &published(completion(CompletionStatus::Complete)),
            "2026-08-04T00:00:00Z",
        );
        let publication = canonical.publication.clone();
        let reconciled = reconcile_terminal_execution(
            Some(&canonical),
            false,
            CallbackStatus::Missing,
            &infrastructure("success"),
            true,
            true,
            false,
        );
        assert_eq!(
            reconciled.decision,
            InfrastructureReconciliationDecision::DomainResultPreserved
        );
        assert_eq!(
            reconciled.authority,
            TerminalEvidenceSource::CanonicalWorkerResult
        );
        assert_eq!(reconciled.anomaly_code, Some("final_callback_missing"));
        assert!(reconciled.domain_status_preserved);
        assert_eq!(reconciled.infrastructure_health, ProcessHealth::Degraded);
        assert_eq!(canonical.publication, publication);
        assert_eq!(canonical.execution_status, DomainExecutionStatus::Completed);
    }

    #[test]
    fn workflow_completion_without_terminal_proof_observes_grace_before_lost() {
        let workflow = infrastructure("success");
        let racing = reconcile_terminal_execution(
            None,
            false,
            CallbackStatus::Missing,
            &workflow,
            true,
            false,
            false,
        );
        assert_eq!(
            racing.decision,
            InfrastructureReconciliationDecision::AwaitingGracePeriod
        );

        let lost = reconcile_terminal_execution(
            None,
            false,
            CallbackStatus::Missing,
            &workflow,
            true,
            true,
            false,
        );
        assert_eq!(
            lost.decision,
            InfrastructureReconciliationDecision::LostWithoutTerminalProof
        );
        assert_eq!(lost.anomaly_code, Some("terminal_proof_missing"));

        let active = reconcile_terminal_execution(
            None,
            false,
            CallbackStatus::Missing,
            &workflow,
            true,
            true,
            true,
        );
        assert_eq!(
            active.decision,
            InfrastructureReconciliationDecision::AwaitingGracePeriod
        );
    }

    #[test]
    fn callback_without_its_canonical_result_is_terminal_corruption() {
        let reconciled = reconcile_terminal_execution(
            None,
            false,
            CallbackStatus::Acknowledged,
            &infrastructure("success"),
            false,
            true,
            false,
        );
        assert_eq!(
            reconciled.decision,
            InfrastructureReconciliationDecision::TerminalStateCorruption
        );
        assert_eq!(reconciled.authority, TerminalEvidenceSource::FinalCallback);
    }

    #[test]
    fn external_review_only_is_typed_and_stronger_than_partial() {
        let mut evaluation = completion(CompletionStatus::Uncertain);
        evaluation.implementation_completeness = ImplementationCompleteness::Complete;
        evaluation.pending_external_review = vec!["Approve the visual result.".into()];
        let canonical = resolve_published_terminal_result(
            Uuid::nil(),
            &published(evaluation),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            canonical.remaining_work_kinds,
            vec![RemainingWorkKind::ExternalApproval]
        );
        assert_eq!(
            canonical.mission_outcome,
            CanonicalMissionOutcome::CompletePendingExternalReview
        );
        assert_eq!(canonical.reason_code, "external_review_required");
    }

    #[test]
    fn terminal_callback_identity_uses_only_canonical_coordinates() {
        let execution_id = Uuid::from_u128(1);
        let terminal_id = Uuid::from_u128(2);
        assert_eq!(
            terminal_callback_idempotency_key(execution_id, terminal_id, 7),
            terminal_callback_idempotency_key(execution_id, terminal_id, 7)
        );
        assert_ne!(
            terminal_callback_idempotency_key(execution_id, terminal_id, 7),
            terminal_callback_idempotency_key(execution_id, terminal_id, 8)
        );
    }

    #[test]
    fn administrative_override_is_not_an_implicit_evidence_precedence() {
        assert_eq!(
            strongest_terminal_evidence([
                TerminalEvidenceSource::AdministrativeOverride,
                TerminalEvidenceSource::WorkflowConclusion,
                TerminalEvidenceSource::CanonicalWorkerResult,
                TerminalEvidenceSource::FinalCallback,
                TerminalEvidenceSource::LeaseExpiration,
            ]),
            TerminalEvidenceSource::CanonicalWorkerResult
        );
        assert_eq!(
            strongest_terminal_evidence([
                TerminalEvidenceSource::AdministrativeOverride,
                TerminalEvidenceSource::WorkflowConclusion,
            ]),
            TerminalEvidenceSource::WorkflowConclusion
        );
    }

    #[test]
    fn final_callback_acknowledges_and_cannot_redefine_canonical_outcome() {
        let execution_id = Uuid::from_u128(1);
        let canonical = resolve_published_terminal_result(
            execution_id,
            &published(completion(CompletionStatus::Complete)),
            "2026-08-04T00:00:00Z",
        );
        let mut request = CompletionRequest {
            status: canonical.completion_request_status().into(),
            canonical_terminal_result_id: Some(canonical.terminal_result_id),
            terminal_revision: Some(canonical.finality.terminal_revision),
            terminal_authority: Some("worker_domain".into()),
            canonical_terminal_result: Some(serde_json::to_value(&canonical).unwrap()),
            mission_outcome: Some(canonical.compatibility_completion_status()),
            process_health: Some(canonical.process_health.as_str().into()),
            completion_evaluation: Some(canonical.completion.clone()),
            output_summary: None,
            failure_code: None,
            failure_message: None,
            head_branch: canonical.publication.branch.clone(),
            head_sha: canonical.publication.commit_sha.clone(),
            pull_request_number: canonical
                .publication
                .pull_request_number
                .map(|number| number as i64),
            pull_request_url: canonical.publication.pull_request_url.clone(),
            final_callback: None,
        };
        let callback = FinalExecutionCallback {
            execution_id,
            canonical_terminal_result_id: canonical.terminal_result_id,
            terminal_revision: canonical.finality.terminal_revision,
            final_notebook_revision: 9,
            process_exit_code: canonical.process_exit_code(),
            workflow_run_id: "100".into(),
            sent_at: "2026-08-04T00:00:01Z".into(),
            idempotency_key: terminal_callback_idempotency_key(
                execution_id,
                canonical.terminal_result_id,
                canonical.finality.terminal_revision,
            )
            .to_string(),
        };
        validate_callback_acknowledgement(&canonical, &request, &callback).unwrap();

        request.mission_outcome = Some(CompletionStatus::Partial);
        assert!(validate_callback_acknowledgement(&canonical, &request, &callback).is_err());
    }

    #[test]
    fn terminal_callback_outbox_round_trips_for_restart_recovery() {
        let entry = TerminalCallbackOutboxEntry {
            execution_id: Uuid::from_u128(1),
            canonical_terminal_result_id: Uuid::from_u128(2),
            payload_hash: "a".repeat(64),
            attempts: 2,
            last_error: Some("terminal_callback_transport_failed".into()),
            acknowledged_at: None,
            created_at: "2026-08-04T00:00:00Z".into(),
        };
        let persisted = serde_json::to_vec(&entry).unwrap();
        let restored: TerminalCallbackOutboxEntry = serde_json::from_slice(&persisted).unwrap();
        assert_eq!(restored, entry);
        assert_eq!(restored.attempts, 2);
        assert!(restored.acknowledged_at.is_none());
    }
}
