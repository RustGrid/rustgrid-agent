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
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{
    Method, StatusCode, Url,
    blocking::{Client, Response},
    header,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    command,
    config::{DEFAULT_INSTANCE_URL, RepoConfig},
    git::{RemoteBranchMoved, Repo, read_repo_instructions},
    github::{GitHubClient, PullRequest},
    shutdown,
    telemetry::{
        ExecutionSnapshot, ExecutionStatus, PhaseSnapshot, TELEMETRY_VERSION, TelemetryBatch,
        TelemetryEvent, TelemetryPayload, now_rfc3339,
    },
    token::parse_rfc3339_utc,
};

mod impact_map;
mod lifecycle;
mod orchestration;

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
const MAX_HOSTED_EXECUTION_DURATION: Duration = Duration::from_secs(10 * 60);
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

#[derive(Clone)]
struct SecretString(String);

impl SecretString {
    fn new(value: String, name: &str) -> Result<Self> {
        if value.trim().is_empty() || value.len() > 32 * 1024 || !value.is_ascii() {
            bail!("{name} is missing or malformed");
        }
        Ok(Self(value))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

struct GithubActionsEnvironment {
    api_root: Url,
    audience: String,
    oidc_request_url: Url,
    oidc_request_token: SecretString,
    dispatch_nonce: SecretString,
    repository: Option<String>,
    repository_id: Option<u64>,
    sha: Option<String>,
    workflow_run_id: Option<i64>,
    workflow_run_attempt: Option<i32>,
    actor: Option<String>,
    actor_id: Option<u64>,
}

struct GithubActionsAuthor {
    name: String,
    email: String,
}

impl GithubActionsEnvironment {
    fn load(execution_id: Uuid) -> Result<Self> {
        reject_inherited_provider_credentials()?;
        let configured_execution_id = required_env("RUSTGRID_EXECUTION_ID")?;
        let configured_execution_id = Uuid::parse_str(&configured_execution_id)
            .context("RUSTGRID_EXECUTION_ID must be a UUID")?;
        if configured_execution_id != execution_id {
            bail!("CLI execution ID does not match RUSTGRID_EXECUTION_ID");
        }

        let api_root = normalize_api_root(
            &env::var("RUSTGRID_API_URL").unwrap_or_else(|_| DEFAULT_INSTANCE_URL.to_owned()),
        )?;
        let audience = api_origin(&api_root)?;
        let oidc_request_url = secure_github_oidc_url(
            "RUSTGRID_OIDC_REQUEST_URL",
            &required_env("RUSTGRID_OIDC_REQUEST_URL")?,
        )?;
        let oidc_request_token = SecretString::new(
            required_env("RUSTGRID_OIDC_REQUEST_TOKEN")?,
            "RUSTGRID_OIDC_REQUEST_TOKEN",
        )?;
        let dispatch_nonce = SecretString::new(
            required_env("RUSTGRID_DISPATCH_NONCE")?,
            "RUSTGRID_DISPATCH_NONCE",
        )?;
        validate_dispatch_nonce(dispatch_nonce.expose())?;
        let repository = optional_env("GITHUB_REPOSITORY");
        let repository_id = optional_env("GITHUB_REPOSITORY_ID")
            .map(|value| value.parse::<u64>())
            .transpose()
            .context("GITHUB_REPOSITORY_ID must be an integer")?;
        let sha = optional_env("GITHUB_SHA");
        let workflow_run_id = optional_env("GITHUB_RUN_ID")
            .map(|value| value.parse::<i64>())
            .transpose()
            .context("GITHUB_RUN_ID must be an integer")?;
        let workflow_run_attempt = optional_env("GITHUB_RUN_ATTEMPT")
            .map(|value| value.parse::<i32>())
            .transpose()
            .context("GITHUB_RUN_ATTEMPT must be an integer")?;
        let actor = optional_env("GITHUB_ACTOR");
        let actor_id = optional_env("GITHUB_ACTOR_ID")
            .map(|value| value.parse::<u64>())
            .transpose()
            .context("GITHUB_ACTOR_ID must be an integer")?;
        Ok(Self {
            api_root,
            audience,
            oidc_request_url,
            oidc_request_token,
            dispatch_nonce,
            repository,
            repository_id,
            sha,
            workflow_run_id,
            workflow_run_attempt,
            actor,
            actor_id,
        })
    }

    fn require_execute_context(&self) -> Result<()> {
        if self.repository.as_deref().is_none_or(str::is_empty)
            || self.repository_id.is_none_or(|value| value == 0)
            || self.sha.as_deref().is_none_or(|value| !commit_sha(value))
            || self.workflow_run_id.is_none_or(|value| value < 1)
            || self.workflow_run_attempt.is_none_or(|value| value < 1)
        {
            bail!(
                "GitHub Actions execution requires repository, repository ID, run ID, and run-attempt context"
            );
        }
        self.git_author()?;
        Ok(())
    }

    fn git_author(&self) -> Result<GithubActionsAuthor> {
        let name = self
            .actor
            .as_deref()
            .filter(|value| valid_github_actor(value))
            .context("GITHUB_ACTOR must identify a valid GitHub account")?;
        let actor_id = self
            .actor_id
            .filter(|value| *value > 0)
            .context("GITHUB_ACTOR_ID must identify a valid GitHub account")?;
        Ok(GithubActionsAuthor {
            name: name.to_owned(),
            email: format!("{actor_id}+{name}@users.noreply.github.com"),
        })
    }
}

#[derive(Deserialize)]
struct GithubOidcResponse {
    value: String,
}

#[derive(Deserialize)]
struct ExchangeResponse {
    access_token: String,
    token_type: String,
    expires_in: i64,
    expires_at: String,
    token_id: Uuid,
    tenant_id: Uuid,
    project_id: Uuid,
    execution_id: Uuid,
    execution_attempt: i32,
    session_id: Uuid,
    worker_id: Uuid,
    repository_id: i64,
    github_workflow_run_id: i64,
    permissions: Vec<String>,
}

#[derive(Deserialize)]
struct RefreshedTokenResponse {
    access_token: String,
    token_type: String,
    expires_at: String,
    token_id: Uuid,
    session_id: Uuid,
}

struct TokenState {
    value: SecretString,
    expires_at: SystemTime,
    refresh_after: SystemTime,
    token_id: Uuid,
    session_id: Uuid,
}

#[derive(Clone)]
struct HostedApiClient {
    http: Client,
    api_root: Url,
    execution_id: Uuid,
    project_id: Uuid,
    repository_id: i64,
    execution_attempt: i32,
    github_workflow_run_id: i64,
    auth: Arc<Mutex<TokenState>>,
    refresh_lock: Arc<Mutex<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ProviderErrorDiagnostic {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    error_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameter: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AiFailureClass {
    RegistrationConflict,
    RequestValidation,
    Gateway,
    ProviderValidation,
    ProviderRateLimit,
    ProviderAuthentication,
    ProviderServer,
    ProviderTimeout,
    ProviderDispatchUncertain,
}

impl AiFailureClass {
    const fn is_provider_failure(self) -> bool {
        matches!(
            self,
            Self::ProviderValidation
                | Self::ProviderRateLimit
                | Self::ProviderAuthentication
                | Self::ProviderServer
                | Self::ProviderTimeout
                | Self::ProviderDispatchUncertain
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AiBudgetDisposition {
    Restore,
    Consumed,
    Unknown,
}

#[derive(Debug)]
struct HostedHttpError {
    status: StatusCode,
    path: String,
    code: String,
    request_id: Option<String>,
    rustgrid_gateway_status: Option<Option<u16>>,
    upstream_provider_status: Option<u16>,
    failure_stage: Option<String>,
    provider_contacted: Option<bool>,
    call_budget_consumed: Option<bool>,
    reservation_state: Option<String>,
    reservation_reconciliation_state: Option<String>,
    retryable: Option<bool>,
    rustgrid_request_id: Option<String>,
    transport_request_id: Option<String>,
    provider_request_id: Option<String>,
    provider_error: Option<ProviderErrorDiagnostic>,
    provider_response_body: Option<Value>,
    model_alias: Option<String>,
    resolved_provider_model: Option<String>,
    adapter_version: Option<String>,
    payload_schema_version: Option<String>,
    provider_attempts: Option<u64>,
    actual_cost_micros: Option<u64>,
}

impl HostedHttpError {
    fn invalidates_execution(&self) -> bool {
        self.status == StatusCode::UNAUTHORIZED
            || matches!(
                self.code.as_str(),
                "execution_token_invalid"
                    | "execution_token_scope_invalid"
                    | "execution_ai_access_revoked"
                    | "execution_cancelled"
                    | "execution_timed_out"
                    | "execution_lost"
            )
    }

    fn effective_code(&self) -> &str {
        match self.failure_class() {
            AiFailureClass::ProviderValidation => "ai_provider_invalid_request",
            AiFailureClass::ProviderRateLimit if self.code == "ai_provider_request_failed" => {
                "ai_provider_rate_limited"
            }
            AiFailureClass::ProviderAuthentication if self.code == "ai_provider_request_failed" => {
                "ai_provider_authentication_failed"
            }
            AiFailureClass::ProviderServer if self.code == "ai_provider_request_failed" => {
                "ai_provider_unavailable"
            }
            AiFailureClass::ProviderTimeout if self.code == "ai_provider_request_failed" => {
                "ai_provider_timeout"
            }
            AiFailureClass::ProviderDispatchUncertain => "ai_request_dispatch_uncertain",
            _ => &self.code,
        }
    }

    fn failure_stage(&self) -> Option<&str> {
        if self.failure_class().is_provider_failure() {
            Some("provider_dispatch")
        } else {
            self.failure_stage.as_deref()
        }
    }

    fn provider_contacted(&self) -> Option<bool> {
        self.provider_contacted
    }

    fn call_budget_consumed(&self) -> Option<bool> {
        self.call_budget_consumed
    }

    fn reservation_state(&self) -> Option<&str> {
        self.reservation_state
            .as_deref()
            .or(self.reservation_reconciliation_state.as_deref())
    }

    fn reservation_reconciliation_state(&self) -> Option<&str> {
        self.reservation_reconciliation_state.as_deref()
    }

    fn has_definite_provider_response(&self) -> bool {
        self.provider_contacted == Some(true) && self.upstream_provider_status.is_some()
    }

    fn failure_class(&self) -> AiFailureClass {
        if self.code == "ai_request_dispatch_uncertain" && !self.has_definite_provider_response() {
            return AiFailureClass::ProviderDispatchUncertain;
        }
        if self.has_definite_provider_response()
            && (self.code == "ai_provider_invalid_request"
                || matches!(self.upstream_provider_status, Some(400 | 404 | 409 | 422)))
        {
            return AiFailureClass::ProviderValidation;
        }
        if self.code == "ai_provider_rate_limited"
            || (self.has_definite_provider_response() && self.upstream_provider_status == Some(429))
        {
            return AiFailureClass::ProviderRateLimit;
        }
        if matches!(
            self.code.as_str(),
            "ai_provider_authentication_failed" | "ai_provider_authentication_error"
        ) || (self.has_definite_provider_response()
            && matches!(self.upstream_provider_status, Some(401 | 403)))
        {
            return AiFailureClass::ProviderAuthentication;
        }
        if self.code == "ai_provider_timeout"
            || (self.has_definite_provider_response() && self.upstream_provider_status == Some(408))
        {
            return AiFailureClass::ProviderTimeout;
        }
        if matches!(
            self.code.as_str(),
            "ai_provider_server_error" | "ai_provider_unavailable"
        ) || (self.has_definite_provider_response()
            && self
                .upstream_provider_status
                .is_some_and(|status| status >= 500))
        {
            return AiFailureClass::ProviderServer;
        }
        if self.has_definite_provider_response() {
            return AiFailureClass::ProviderServer;
        }
        if self.failure_stage.as_deref() == Some("request_validation")
            && self.provider_contacted == Some(false)
        {
            return AiFailureClass::RequestValidation;
        }
        if self.failure_stage.as_deref() == Some("request_registration") {
            return AiFailureClass::RegistrationConflict;
        }
        AiFailureClass::Gateway
    }

    fn rustgrid_gateway_status(&self) -> Option<Option<u16>> {
        if self.failure_class().is_provider_failure() {
            self.rustgrid_gateway_status
        } else {
            self.rustgrid_gateway_status
                .or(Some(Some(self.status.as_u16())))
        }
    }

    fn terminal_message(&self) -> &'static str {
        match self.failure_class() {
            AiFailureClass::RegistrationConflict => {
                "AI request registration conflicted before provider dispatch."
            }
            AiFailureClass::RequestValidation => {
                "RustGrid rejected the AI request during adapter validation."
            }
            AiFailureClass::Gateway => "The RustGrid AI gateway rejected the model call.",
            AiFailureClass::ProviderValidation => {
                "The upstream model provider rejected the request as invalid."
            }
            AiFailureClass::ProviderRateLimit => {
                "The upstream model provider rate-limited the request."
            }
            AiFailureClass::ProviderAuthentication => {
                "The upstream model provider rejected RustGrid's credentials or access."
            }
            AiFailureClass::ProviderServer => {
                "The upstream model provider failed while processing the request."
            }
            AiFailureClass::ProviderTimeout => {
                "The upstream model provider did not respond before the request deadline."
            }
            AiFailureClass::ProviderDispatchUncertain => {
                "RustGrid could not determine whether the upstream provider accepted the request."
            }
        }
    }

    fn recommended_action(&self) -> &'static str {
        match self.failure_class() {
            AiFailureClass::RegistrationConflict => {
                "Retry from the persisted phase and notebook; do not repeat repository bootstrap or discovery."
            }
            AiFailureClass::RequestValidation => {
                "Correct the reported model, parameter, or schema before retrying; do not resend the unchanged invalid payload."
            }
            AiFailureClass::ProviderValidation => {
                "Correct the reported provider parameter or schema before retrying; do not resend the unchanged invalid payload."
            }
            AiFailureClass::ProviderDispatchUncertain => {
                "Reconcile the provider attempt before retrying to avoid duplicate model work."
            }
            _ => "Retry from the persisted phase and notebook after resolving the reported cause.",
        }
    }

    fn budget_disposition(&self) -> AiBudgetDisposition {
        if self.call_budget_consumed == Some(true) {
            return AiBudgetDisposition::Consumed;
        }
        let safe_registration_release = self.failure_class()
            == AiFailureClass::RegistrationConflict
            && self.provider_contacted == Some(false)
            && self.call_budget_consumed == Some(false);
        let safe_preflight_rejection = self.failure_class() == AiFailureClass::RequestValidation
            && self.provider_contacted == Some(false)
            && self.call_budget_consumed == Some(false)
            && self.actual_cost_micros == Some(0)
            && self.reservation_state() == Some("not_created");
        let confirmed_pre_dispatch_release = self.provider_contacted == Some(false)
            && self.upstream_provider_status.is_none()
            && self.call_budget_consumed == Some(false)
            && self.actual_cost_micros == Some(0)
            && matches!(
                self.reservation_state(),
                Some(
                    "not_created"
                        | "released"
                        | "reconciled"
                        | "failed_before_dispatch"
                        | "previous_request_settled"
                )
            );
        let confirmed_non_billable_validation = self.failure_class()
            == AiFailureClass::ProviderValidation
            && self.has_definite_provider_response()
            && self.call_budget_consumed == Some(false)
            && self.actual_cost_micros == Some(0);
        if safe_registration_release
            || safe_preflight_rejection
            || confirmed_pre_dispatch_release
            || confirmed_non_billable_validation
        {
            AiBudgetDisposition::Restore
        } else {
            AiBudgetDisposition::Unknown
        }
    }

    fn retryable_gateway_transport_failure(&self) -> bool {
        !self.has_definite_provider_response()
            && self.failure_class() == AiFailureClass::Gateway
            && retryable_status(self.status)
    }

    fn retryable_registration_failure(&self) -> bool {
        let safe_pre_dispatch_failure = self.failure_stage() == Some("request_registration")
            && self.provider_contacted() == Some(false)
            && self.call_budget_consumed() == Some(false);
        let retryable_reconciliation = matches!(
            self.reservation_reconciliation_state(),
            Some("failed_before_dispatch" | "previous_request_settled" | "released" | "reconciled")
        );
        let permanent_conflict = matches!(
            self.effective_code(),
            "ai_request_payload_conflict"
                | "execution_ai_access_revoked"
                | "execution_ai_request_not_allowed"
                | "execution_token_invalid"
                | "execution_token_scope_invalid"
        );
        safe_pre_dispatch_failure
            && !permanent_conflict
            && (self.retryable == Some(true)
                || (self.retryable == Some(false) && retryable_reconciliation))
    }
}

impl std::fmt::Display for HostedHttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "RustGrid {} returned {} ({}){}",
            self.path,
            self.status,
            self.code,
            self.request_id
                .as_ref()
                .map(|id| format!("; request {id}"))
                .unwrap_or_default()
        )
    }
}

impl std::error::Error for HostedHttpError {}

impl HostedApiClient {
    fn from_exchange(
        http: Client,
        api_root: Url,
        execution_id: Uuid,
        exchange: ExchangeResponse,
    ) -> Result<Self> {
        if exchange.execution_id != execution_id
            || exchange.token_type != "Bearer"
            || exchange.expires_in < 30
            || exchange.token_id.is_nil()
            || exchange.session_id.is_nil()
            || exchange.execution_attempt < 1
            || exchange.tenant_id.is_nil()
            || exchange.project_id.is_nil()
            || exchange.worker_id.is_nil()
            || exchange.repository_id < 1
            || exchange.github_workflow_run_id < 1
            || exchange.permissions.len() != EXECUTION_PERMISSIONS.len()
            || EXECUTION_PERMISSIONS
                .iter()
                .any(|permission| !exchange.permissions.iter().any(|value| value == permission))
        {
            bail!("RustGrid returned an invalid execution credential");
        }
        validate_execution_token(&exchange.access_token)?;
        let expires_at = parse_rfc3339_utc(&exchange.expires_at)
            .context("RustGrid returned an invalid execution-token expiry")?;
        if expires_at
            .duration_since(SystemTime::now())
            .unwrap_or_default()
            < Duration::from_secs(30)
        {
            bail!("RustGrid returned an already-expired execution credential");
        }
        let state = TokenState {
            value: SecretString::new(exchange.access_token, "execution token")?,
            expires_at,
            refresh_after: token_refresh_after(expires_at),
            token_id: exchange.token_id,
            session_id: exchange.session_id,
        };
        Ok(Self {
            http,
            api_root,
            execution_id,
            project_id: exchange.project_id,
            repository_id: exchange.repository_id,
            execution_attempt: exchange.execution_attempt,
            github_workflow_run_id: exchange.github_workflow_run_id,
            auth: Arc::new(Mutex::new(state)),
            refresh_lock: Arc::new(Mutex::new(())),
        })
    }

    fn claim(&self) -> Result<Value> {
        self.send_json(
            Method::POST,
            &format!("executions/{}/claim", self.execution_id),
            Some(json!({"lease_seconds": EXECUTION_LEASE_SECONDS})),
            None,
            2,
        )
    }

    fn manifest(&self) -> Result<HostedManifest> {
        self.send_json(
            Method::GET,
            &format!("executions/{}/manifest", self.execution_id),
            None,
            None,
            2,
        )
    }

    fn heartbeat(&self) -> Result<()> {
        let _: Value = self.send_json(
            Method::POST,
            &format!("executions/{}/heartbeat", self.execution_id),
            Some(json!({"lease_seconds": EXECUTION_LEASE_SECONDS})),
            None,
            2,
        )?;
        Ok(())
    }

    fn append_event(&self, event_type: &str, data: Value) -> Result<()> {
        if !matches!(
            event_type,
            "progress" | "message" | "log" | "tool" | "validation" | "result"
        ) || !data.is_object()
        {
            bail!("invalid hosted execution event");
        }
        let body = json!({"event_type": event_type, "data": data});
        let encoded = serde_json::to_vec(&body)?;
        let idempotency_key = Uuid::new_v5(
            &HOSTED_NAMESPACE,
            &[
                b"worker-event:".as_slice(),
                self.execution_id.as_bytes().as_slice(),
                encoded.as_slice(),
            ]
            .concat(),
        );
        let _: Value = self.send_json(
            Method::POST,
            &format!("executions/{}/worker-events", self.execution_id),
            Some(body),
            Some(idempotency_key),
            2,
        )?;
        Ok(())
    }

    fn update_state(&self, state: &str) -> Result<()> {
        if !matches!(state, "validating" | "creating_pull_request") {
            bail!("invalid worker-owned execution state");
        }
        let _: Value = self.send_json(
            Method::POST,
            &format!("executions/{}/state", self.execution_id),
            Some(json!({"state": state})),
            None,
            2,
        )?;
        Ok(())
    }

    fn github_token(&self, expected_repository: &str) -> Result<SecretString> {
        let issued: GithubTokenResponse = self.send_json(
            Method::POST,
            &format!("executions/{}/github-token", self.execution_id),
            None,
            None,
            1,
        )?;
        if !issued.repository.eq_ignore_ascii_case(expected_repository)
            || issued.permissions.get("contents").and_then(Value::as_str) != Some("write")
            || issued
                .permissions
                .get("pull_requests")
                .and_then(Value::as_str)
                != Some("write")
        {
            bail!(
                "RustGrid returned a GitHub token outside the manifest repository or permissions"
            );
        }
        let expires_at = parse_rfc3339_utc(&issued.expires_at)
            .context("RustGrid returned an invalid GitHub token expiry")?;
        if expires_at
            .duration_since(SystemTime::now())
            .unwrap_or_default()
            < Duration::from_secs(30)
        {
            bail!("RustGrid returned an already-expired GitHub repository token");
        }
        SecretString::new(issued.token, "GitHub repository token")
    }

    #[cfg(test)]
    fn ai_response(&self, body: Value, registration: &AiCallRegistration) -> Result<Value> {
        self.ai_response_until(body, registration, None)
    }

    fn ai_response_until(
        &self,
        body: Value,
        registration: &AiCallRegistration,
        execution_deadline: Option<Instant>,
    ) -> Result<Value> {
        ai_request_timeout(execution_deadline)?;
        self.ensure_fresh()?;
        let token = self.current_token()?;
        let path = format!("executions/{}/ai/responses", self.execution_id);
        let url = self
            .api_root
            .join(&path)
            .with_context(|| format!("invalid RustGrid API path {path}"))?;
        for attempt in 0..3 {
            let request_timeout = ai_request_timeout(execution_deadline)?;
            let response = self
                .http
                .post(url.clone())
                .bearer_auth(token.expose())
                .header(header::ACCEPT, "application/json")
                .header("Idempotency-Key", registration.request_id.to_string())
                .header(
                    "X-RustGrid-Semantic-Call-Id",
                    registration.semantic_call_id.to_string(),
                )
                .header("X-RustGrid-Call-Index", registration.call_index.to_string())
                .header("X-RustGrid-Call-Phase", registration.phase.as_str())
                .header(
                    "X-RustGrid-Registration-Attempt",
                    registration.registration_attempt.to_string(),
                )
                .json(&body)
                .timeout(request_timeout)
                .send();
            match response {
                Ok(response) if response.status().is_success() => {
                    return decode_response(response, &path);
                }
                Ok(response) => {
                    let error = decode_response::<Value>(response, &path)
                        .expect_err("non-success AI gateway responses must decode as failures");
                    let can_retry_transport = error
                        .downcast_ref::<HostedHttpError>()
                        .is_some_and(HostedHttpError::retryable_gateway_transport_failure);
                    if can_retry_transport && attempt < 2 {
                        sleep_before_ai_retry(execution_deadline, attempt)?;
                    } else {
                        return Err(error);
                    }
                }
                Err(_) if attempt < 2 => {
                    sleep_before_ai_retry(execution_deadline, attempt)?;
                }
                Err(_) => bail!("RustGrid {path} transport failed"),
            }
        }
        unreachable!("bounded AI gateway transport loop always returns")
    }

    fn telemetry(&self, batch: &TelemetryBatch) -> Result<()> {
        let body = serde_json::to_value(batch)?;
        let _: Value = self.send_json(
            Method::POST,
            &format!("executions/{}/telemetry/batch", self.execution_id),
            Some(body),
            None,
            1,
        )?;
        Ok(())
    }

    fn complete(&self, completion: &CompletionRequest) -> Result<Value> {
        let body = serde_json::to_value(completion)?;
        let key = completion_idempotency_key(self.execution_id, completion)?;
        self.send_json(
            Method::POST,
            &format!("executions/{}/complete", self.execution_id),
            Some(body),
            Some(key),
            3,
        )
    }

    fn ensure_fresh(&self) -> Result<()> {
        let refresh_required = {
            let state = self
                .auth
                .lock()
                .map_err(|_| anyhow!("execution-token lock is poisoned"))?;
            SystemTime::now() >= state.refresh_after
        };
        if refresh_required {
            self.refresh_token()?;
        }
        Ok(())
    }

    fn refresh_token(&self) -> Result<()> {
        let _refresh = self
            .refresh_lock
            .lock()
            .map_err(|_| anyhow!("execution-token refresh lock is poisoned"))?;
        let refresh_required = {
            let state = self
                .auth
                .lock()
                .map_err(|_| anyhow!("execution-token lock is poisoned"))?;
            SystemTime::now() >= state.refresh_after
        };
        if !refresh_required {
            return Ok(());
        }
        let token = self.current_token()?;
        let path = format!("executions/{}/token/refresh", self.execution_id);
        let response: RefreshedTokenResponse = self.send_with_token(
            Method::POST,
            &path,
            Some(json!({"ttl_seconds": EXECUTION_TOKEN_TTL_SECONDS})),
            None,
            1,
            token.expose(),
        )?;
        if response.token_type != "Bearer" {
            bail!("RustGrid returned an invalid refreshed execution credential");
        }
        validate_execution_token(&response.access_token)?;
        let expires_at = parse_rfc3339_utc(&response.expires_at)
            .context("RustGrid returned an invalid refreshed token expiry")?;
        if response.token_id.is_nil()
            || expires_at
                .duration_since(SystemTime::now())
                .unwrap_or_default()
                < Duration::from_secs(30)
        {
            bail!("RustGrid returned an already-expired refreshed execution credential");
        }
        let mut state = self
            .auth
            .lock()
            .map_err(|_| anyhow!("execution-token lock is poisoned"))?;
        if response.session_id != state.session_id || response.token_id == state.token_id {
            bail!("RustGrid refreshed execution credential identity is invalid");
        }
        let refresh_was_lifetime_capped = expires_at
            <= state
                .expires_at
                .checked_add(Duration::from_secs(5))
                .unwrap_or(state.expires_at);
        state.value = SecretString::new(response.access_token, "execution token")?;
        state.expires_at = expires_at;
        state.refresh_after = if refresh_was_lifetime_capped {
            // The session maximum is authoritative. Avoid rotating the token on
            // every worker operation when a refresh cannot extend that maximum.
            expires_at
        } else {
            token_refresh_after(expires_at)
        };
        state.token_id = response.token_id;
        Ok(())
    }

    fn current_token(&self) -> Result<SecretString> {
        SecretString::new(
            self.auth
                .lock()
                .map_err(|_| anyhow!("execution-token lock is poisoned"))?
                .value
                .expose()
                .to_owned(),
            "execution token",
        )
    }

    fn session_id(&self) -> Result<Uuid> {
        Ok(self
            .auth
            .lock()
            .map_err(|_| anyhow!("execution-token lock is poisoned"))?
            .session_id)
    }

    fn send_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency_key: Option<Uuid>,
        attempts: usize,
    ) -> Result<T> {
        self.ensure_fresh()?;
        let token = self.current_token()?;
        self.send_with_token(
            method,
            path,
            body,
            idempotency_key,
            attempts,
            token.expose(),
        )
    }

    fn send_with_token<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency_key: Option<Uuid>,
        attempts: usize,
        token: &str,
    ) -> Result<T> {
        let url = self
            .api_root
            .join(path)
            .with_context(|| format!("invalid RustGrid API path {path}"))?;
        let attempts = attempts.max(1);
        for attempt in 0..attempts {
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(token)
                .header(header::ACCEPT, "application/json");
            if let Some(body) = body.as_ref() {
                request = request.json(body);
            }
            if let Some(key) = idempotency_key {
                request = request.header("Idempotency-Key", key.to_string());
            }
            match request.send() {
                Ok(response) if retryable_status(response.status()) && attempt + 1 < attempts => {
                    thread::sleep(retry_delay(attempt));
                }
                Ok(response) => return decode_response(response, path),
                Err(_) if attempt + 1 < attempts => thread::sleep(retry_delay(attempt)),
                Err(_) => bail!("RustGrid {path} transport failed"),
            }
        }
        unreachable!("bounded HTTP loop always returns")
    }
}

fn completion_idempotency_key(execution_id: Uuid, completion: &CompletionRequest) -> Result<Uuid> {
    let encoded = serde_json::to_vec(completion)?;
    let key_material = [
        b"completion:".as_slice(),
        execution_id.as_bytes().as_slice(),
        encoded.as_slice(),
    ]
    .concat();
    Ok(Uuid::new_v5(&HOSTED_NAMESPACE, &key_material))
}

#[derive(Clone, Copy, Debug)]
struct AiCallRegistration {
    semantic_call_id: Uuid,
    request_id: Uuid,
    call_index: usize,
    phase: ExecutionPhase,
    registration_attempt: usize,
}

fn ai_call_registration(
    execution_id: Uuid,
    execution_attempt: i32,
    worker_session_id: Uuid,
    call_index: usize,
    phase: ExecutionPhase,
    registration_attempt: usize,
) -> AiCallRegistration {
    let attempt = execution_attempt.to_be_bytes();
    let call_index_bytes = call_index.to_be_bytes();
    let semantic_material = [
        b"ai-semantic-call:".as_slice(),
        execution_id.as_bytes().as_slice(),
        attempt.as_slice(),
        call_index_bytes.as_slice(),
    ]
    .concat();
    let semantic_call_id = Uuid::new_v5(&HOSTED_NAMESPACE, &semantic_material);
    let registration_attempt_bytes = registration_attempt.to_be_bytes();
    let transport_material = [
        b"ai-registration-attempt:".as_slice(),
        semantic_call_id.as_bytes().as_slice(),
        worker_session_id.as_bytes().as_slice(),
        registration_attempt_bytes.as_slice(),
    ]
    .concat();
    AiCallRegistration {
        semantic_call_id,
        request_id: Uuid::new_v5(&HOSTED_NAMESPACE, &transport_material),
        call_index,
        phase,
        registration_attempt,
    }
}

#[derive(Deserialize)]
struct GithubTokenResponse {
    token: String,
    expires_at: String,
    permissions: Value,
    repository: String,
}

#[derive(Clone, Debug, Deserialize)]
struct HostedManifest {
    manifest_version: i32,
    #[serde(default)]
    model_call_budget: Option<i32>,
    #[serde(default)]
    requested_model_call_budget: Option<i32>,
    #[serde(default)]
    resolved_model_call_budget: Option<i32>,
    #[serde(default)]
    budget_source: Option<BudgetSource>,
    #[serde(default)]
    clamped: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_present_nullable")]
    clamp_reason: Option<Option<String>>,
    execution: ManifestExecution,
    run: ManifestRun,
    project_id: Uuid,
    project_key: String,
    project_name: String,
    ticket_id: Uuid,
    ticket_key: String,
    ticket_title: String,
    github: HostedGithubManifest,
    ai_gateway: HostedAiManifest,
    execution_policy: HostedExecutionPolicy,
    execution_policy_sha256: String,
    heartbeat_url: String,
    token_refresh_url: String,
    events_url: String,
    telemetry_url: String,
    state_url: String,
    complete_url: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestExecution {
    execution_id: Uuid,
    status: String,
    attempt_number: i32,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    maximum_input_tokens: Option<i64>,
    #[serde(default)]
    maximum_output_tokens: Option<i64>,
    #[serde(default)]
    maximum_model_calls: Option<i32>,
    #[serde(default)]
    maximum_duration_seconds: Option<i32>,
    #[serde(default)]
    maximum_cost_usd: Option<String>,
    #[serde(default)]
    github_actions: Option<ManifestGithubActionsExecution>,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestGithubActionsExecution {
    workflow_run_id: Option<i64>,
    workflow_run_attempt: Option<i32>,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestRun {
    id: Uuid,
    ticket_id: Uuid,
    input_prompt: String,
    attempt: i32,
    #[serde(default)]
    metadata: Value,
}

#[derive(Clone, Debug, Deserialize)]
struct HostedGithubManifest {
    repository_id: i64,
    repository: String,
    clone_url: String,
    web_base_url: String,
    installation_id: i64,
    base_ref: String,
    base_sha: String,
    branch: String,
    github_token_url: String,
}

#[derive(Clone, Debug, Deserialize)]
struct HostedAiManifest {
    responses_url: String,
    model: String,
    maximum_input_tokens: i64,
    maximum_output_tokens: i64,
    maximum_model_calls: i32,
    maximum_cost_usd: String,
}

fn deserialize_present_nullable<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum BudgetSource {
    UserSelected,
    ProjectDefault,
    SystemDefault,
}

#[derive(Clone, Debug, Serialize)]
struct BudgetAudit {
    requested_model_call_budget: i32,
    resolved_model_call_budget: i32,
    worker_received_model_call_budget: i32,
    budget_source: Option<BudgetSource>,
    clamped: bool,
    clamp_reason: Option<String>,
    contract: &'static str,
}

#[derive(Debug)]
struct ExecutionBudgetMismatch {
    requested: Option<i32>,
    resolved: Option<i32>,
    canonical: Option<i32>,
    execution: Option<i32>,
    worker_received: i32,
}

impl std::fmt::Display for ExecutionBudgetMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "execution_budget_mismatch: requested={:?}, resolved={:?}, canonical={:?}, execution={:?}, worker_received={}",
            self.requested, self.resolved, self.canonical, self.execution, self.worker_received
        )
    }
}

impl std::error::Error for ExecutionBudgetMismatch {}

#[derive(Debug)]
struct HostedProviderContractFailure {
    code: String,
    message: String,
}

impl HostedProviderContractFailure {
    fn from_validation(error: anyhow::Error) -> Self {
        let message = error.to_string();
        let code = message
            .split_once(':')
            .map(|(code, _)| code)
            .filter(|code| {
                matches!(
                    *code,
                    "ai_provider_request_invalid"
                        | "ai_tool_schema_invalid"
                        | "ai_response_schema_invalid"
                )
            })
            .unwrap_or("ai_provider_request_invalid")
            .to_owned();
        Self { code, message }
    }
}

impl std::fmt::Display for HostedProviderContractFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HostedProviderContractFailure {}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HostedExecutionPolicy {
    policy_version: i32,
    codex: HostedCodexPolicy,
    quality_gates: Vec<HostedQualityGate>,
    timeout_seconds: i64,
    sandbox: HostedSandboxPolicy,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default)]
struct ProjectVerificationPolicy {
    browser_e2e_required_for_theme_changes: bool,
    manual_browser_verification_required: bool,
}

impl Default for ProjectVerificationPolicy {
    fn default() -> Self {
        Self {
            browser_e2e_required_for_theme_changes: false,
            manual_browser_verification_required: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HostedCodexPolicy {
    command: Vec<String>,
    environment_allowlist: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HostedQualityGate {
    id: String,
    command: String,
    timeout_seconds: i64,
    required: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HostedSandboxPolicy {
    mode: String,
    network_access: bool,
    writable_roots: Vec<String>,
    approval_policy: String,
}

#[derive(Clone, Debug, Serialize)]
struct CompletionRequest {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mission_outcome: Option<CompletionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    process_health: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completion_evaluation: Option<CompletionEvaluation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pull_request_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pull_request_url: Option<String>,
}

#[derive(Clone, Debug)]
struct HostedResult {
    summary: String,
    branch: String,
    commit: String,
    pull_request: PullRequestResult,
    validation: Vec<ValidationResult>,
    completeness: CompletionEvaluation,
    terminal_telemetry: TerminalTelemetry,
}

#[derive(Clone, Debug, Default, Serialize)]
struct TerminalTelemetry {
    model_calls_used: usize,
    input_tokens: u64,
    output_tokens: u64,
    estimated_cost_micros: u64,
    usage: ToolUsage,
    changed_paths: Vec<String>,
    last_successful_action: Value,
    phase_reached: Option<ExecutionPhase>,
    plan: Vec<PlannedChange>,
    remaining_work: Vec<RemainingWorkItem>,
    validation_evidence: Vec<ValidationEvidence>,
    notebook_revision: u64,
}

#[derive(Clone, Debug)]
struct PullRequestResult {
    number: u64,
    url: String,
}

#[derive(Clone, Debug, Serialize)]
struct ValidationResult {
    id: String,
    command: String,
    status: String,
    output: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CompletionStatus {
    Complete,
    CompletePendingExternalReview,
    Partial,
    Incomplete,
    Blocked,
    Uncertain,
}

impl CompletionStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::CompletePendingExternalReview => "complete_pending_external_review",
            Self::Partial => "partial",
            Self::Incomplete => "incomplete",
            Self::Blocked => "blocked",
            Self::Uncertain => "uncertain",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ImplementationCompleteness {
    Complete,
    Partial,
    Incomplete,
}

impl ImplementationCompleteness {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Incomplete => "incomplete",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum VerificationReadiness {
    Verified,
    AutomatedVerified,
    PendingManualReview,
    Blocked,
}

impl VerificationReadiness {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::AutomatedVerified => "automated_verified",
            Self::PendingManualReview => "pending_manual_review",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EvaluationSource {
    Model,
    OrchestratorFallback,
    Hybrid,
}

impl EvaluationSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::OrchestratorFallback => "orchestrator_fallback",
            Self::Hybrid => "hybrid",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum VerificationType {
    Code,
    AutomatedTest,
    ManualQa,
    AccessibilityReview,
    VisualReview,
    ProductApproval,
    DeploymentEnvironment,
}

impl VerificationType {
    const fn requires_external_review(self) -> bool {
        matches!(
            self,
            Self::ManualQa
                | Self::AccessibilityReview
                | Self::VisualReview
                | Self::ProductApproval
                | Self::DeploymentEnvironment
        )
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::AutomatedTest => "automated_test",
            Self::ManualQa => "manual_qa",
            Self::AccessibilityReview => "accessibility_review",
            Self::VisualReview => "visual_review",
            Self::ProductApproval => "product_approval",
            Self::DeploymentEnvironment => "deployment_environment",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CriterionStatus {
    Satisfied,
    PartiallySatisfied,
    Unsatisfied,
    Uncertain,
    ExternalReviewRequired,
    NotApplicable,
}

impl CriterionStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Satisfied => "satisfied",
            Self::PartiallySatisfied => "partially_satisfied",
            Self::Unsatisfied => "unsatisfied",
            Self::Uncertain => "uncertain",
            Self::ExternalReviewRequired => "external_review_required",
            Self::NotApplicable => "not_applicable",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CompletionEvidence {
    path: String,
    description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CriterionEvaluation {
    criterion_id: String,
    criterion: String,
    verification_type: VerificationType,
    status: CriterionStatus,
    #[serde(default)]
    evidence: Vec<CompletionEvidence>,
    #[serde(default)]
    validation_evidence: Vec<String>,
    #[serde(default)]
    missing_evidence: Vec<String>,
    #[serde(default)]
    required_next_action: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ReviewChecklistItem {
    r#type: VerificationType,
    description: String,
    status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CompletionEvaluation {
    status: CompletionStatus,
    implementation_completeness: ImplementationCompleteness,
    verification_readiness: VerificationReadiness,
    evaluation_source: EvaluationSource,
    confidence: f64,
    #[serde(default)]
    criteria: Vec<CriterionEvaluation>,
    #[serde(default)]
    remaining_implementation_work: Vec<String>,
    #[serde(default)]
    remaining_automated_verification: Vec<String>,
    #[serde(default)]
    pending_external_review: Vec<String>,
    #[serde(default)]
    optional_follow_up: Vec<String>,
    #[serde(default)]
    review_checklist: Vec<ReviewChecklistItem>,
    #[serde(default)]
    unrecovered_tool_failures: Vec<String>,
    summary: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ImplementationPlan {
    implementation_status: String,
    #[serde(default)]
    planned_changes: Vec<PlannedChange>,
    #[serde(default)]
    planned_new_files: Vec<String>,
    #[serde(default)]
    planned_test_changes: Vec<String>,
    #[serde(default)]
    remaining_unknowns: Vec<String>,
    #[serde(default)]
    blocking_unknowns: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PlannedChange {
    #[serde(default)]
    change_id: String,
    #[serde(default)]
    parent_change_id: Option<String>,
    #[serde(default, skip_serializing)]
    path: String,
    #[serde(default, deserialize_with = "deserialize_planned_targets")]
    targets: Vec<PlannedTarget>,
    #[serde(rename = "intent", alias = "change")]
    change: String,
    reason: String,
    #[serde(default)]
    status: IntendedChangeStatus,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    test_coverage: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PlannedTarget {
    path: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    new_file: bool,
    #[serde(default)]
    status: IntendedChangeStatus,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PlannedTargetInput {
    Path(String),
    Target(PlannedTarget),
}

fn deserialize_planned_targets<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<PlannedTarget>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<PlannedTargetInput>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .map(|value| match value {
            PlannedTargetInput::Path(path) => PlannedTarget {
                path,
                role: String::new(),
                new_file: false,
                status: IntendedChangeStatus::Planned,
            },
            PlannedTargetInput::Target(target) => target,
        })
        .collect())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ImplementationDeclaration {
    implementation_status: String,
    #[serde(default)]
    completed_work: Vec<String>,
    #[serde(default)]
    remaining_work: Vec<String>,
    #[serde(default)]
    known_risks: Vec<String>,
    #[serde(default)]
    changed_paths: Vec<String>,
    #[serde(default)]
    criteria_evidence: Vec<ImplementationCriterionEvidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ImplementationCriterionEvidence {
    criterion: String,
    #[serde(default)]
    paths: Vec<String>,
    evidence: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ToolFailureRecord {
    #[serde(default)]
    attempt_index: usize,
    #[serde(default)]
    change_id: Option<String>,
    tool: String,
    target: Option<String>,
    #[serde(default)]
    error_code: String,
    #[serde(default)]
    match_count: Option<usize>,
    error: String,
    recovered: bool,
    #[serde(default)]
    reconciliation: FailureReconciliation,
    #[serde(default)]
    recovery: Option<IntendedChangeRecovery>,
    #[serde(default)]
    intended_change_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FailureReconciliation {
    Recovered,
    Superseded,
    #[default]
    StillUnresolved,
    Unrelated,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct IntendedChangeRecovery {
    recovered: bool,
    method: String,
    #[serde(default)]
    evidence: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum IntendedChangeStatus {
    #[default]
    Planned,
    InProgress,
    Applied,
    Verified,
    Partial,
    Unresolved,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WriteAttemptStatus {
    Applied,
    NoChange,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WriteAttemptRecord {
    #[serde(default)]
    attempt_index: usize,
    change_id: String,
    target: String,
    tool: String,
    status: WriteAttemptStatus,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    match_count: Option<usize>,
    #[serde(default)]
    intended_change_sha256: Option<String>,
    #[serde(default)]
    before_sha256: Option<String>,
    #[serde(default)]
    after_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct MutationPreflightRecord {
    change_id: String,
    target: String,
    failure_code: String,
    plan_revision: u64,
    retryable_with_same_plan: bool,
    repair_strategy: String,
    mutation_attempted: bool,
    mutation_preflight_failed: bool,
    #[serde(default)]
    deterministic_repair_attempted: bool,
    #[serde(default = "one_u32")]
    occurrences: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ImplementationPlanRepair {
    change_id: String,
    targets_before: Vec<String>,
    targets_after: Vec<String>,
    attempted_concrete_path: String,
    validation_error: String,
    repair_source: &'static str,
    model_call_consumed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MutationPreflightDecision {
    repeated: bool,
    halt_orchestration: bool,
}

const fn one_u32() -> u32 {
    1
}

#[derive(Debug)]
struct MutationPreflightError {
    code: &'static str,
    change_id: String,
    target: String,
    message: String,
    repair_strategy: &'static str,
}

impl std::fmt::Display for MutationPreflightError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MutationPreflightError {}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct IntendedChangeRecord {
    change_id: String,
    intent: String,
    status: IntendedChangeStatus,
    #[serde(default, skip_serializing)]
    target: String,
    #[serde(default)]
    targets: Vec<PlannedTarget>,
    #[serde(default)]
    attempts: Vec<WriteAttemptRecord>,
    #[serde(default)]
    recovery: Option<IntendedChangeRecovery>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ArtifactSemanticStatus {
    Partial,
    Sufficient,
    Invalid,
    #[default]
    Missing,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ArtifactSerializationStatus {
    Valid,
    Normalizable,
    #[default]
    Invalid,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ArtifactFailureLayer {
    ProviderToolArgumentGeneration,
    GatewayToolArgumentParsing,
    WorkerToolSchemaValidation,
    ArtifactSemanticValidation,
    ArtifactPersistence,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ArtifactPersistenceStatus {
    Persisted,
    Failed,
    #[default]
    PendingRetry,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ArtifactCheckpoint {
    artifact: String,
    semantic_status: ArtifactSemanticStatus,
    serialization_status: ArtifactSerializationStatus,
    persistence_status: ArtifactPersistenceStatus,
    #[serde(default)]
    artifact_sha256: Option<String>,
    #[serde(default)]
    model_call_index: Option<usize>,
    phase: ExecutionPhase,
    #[serde(default)]
    safe_error: Option<String>,
    #[serde(default)]
    artifact_source: Option<ArtifactSource>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    failure_layer: Option<ArtifactFailureLayer>,
    #[serde(default)]
    validation_errors: Vec<ValidationError>,
    #[serde(default)]
    invalid_payload_shape: Option<InvalidPayloadShape>,
}

impl Default for ArtifactCheckpoint {
    fn default() -> Self {
        Self {
            artifact: "impact_map".into(),
            semantic_status: ArtifactSemanticStatus::Missing,
            serialization_status: ArtifactSerializationStatus::Invalid,
            persistence_status: ArtifactPersistenceStatus::PendingRetry,
            artifact_sha256: None,
            model_call_index: None,
            phase: ExecutionPhase::Discovery,
            safe_error: None,
            artifact_source: None,
            confidence: None,
            failure_layer: None,
            validation_errors: Vec::new(),
            invalid_payload_shape: None,
        }
    }
}

#[derive(Clone, Debug)]
struct ImpactMapFailure {
    code: &'static str,
    safe_error: String,
    errors: Vec<ValidationError>,
    invalid_payload: Value,
    invalid_payload_shape: InvalidPayloadShape,
    failure_layer: ArtifactFailureLayer,
}

#[derive(Clone, Debug)]
struct ImplementationOutcome {
    summary: String,
    budget_exhausted: bool,
    explicit_declaration: Option<ImplementationDeclaration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PartialRunContext {
    pull_request_number: u64,
    changed_paths: Vec<String>,
    remaining_work: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct ToolUsage {
    reads: u32,
    failed_reads: u32,
    searches: u32,
    writes: u32,
    successful_writes: u32,
    failed_writes: u32,
    write_preflight_rejections: u32,
    write_execution_failures: u32,
    validation_commands: u32,
    focused_validations: u32,
    required_validations: u32,
    deduplicated_validations: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ToolProgressRecord {
    #[serde(default)]
    execution_attempt: i32,
    model_call: usize,
    phase: ExecutionPhase,
    tool: String,
    #[serde(default)]
    target: Option<String>,
    class: ToolProgressClass,
    outcome_signature: String,
    detail: String,
    repository_progress: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ImplementationReadProgress {
    consecutive_preparation_reads: usize,
    recoverable_read_failures: usize,
    repeated_identical_read_failures: usize,
}

#[allow(clippy::too_many_arguments)]
fn new_tool_progress_record(
    execution_attempt: i32,
    model_call: usize,
    phase: ExecutionPhase,
    tool: &str,
    target: Option<String>,
    class: ToolProgressClass,
    detail: impl Into<String>,
    repository_progress: bool,
) -> ToolProgressRecord {
    let detail = truncate_text(&detail.into(), 1_000);
    let outcome_signature = sha256_text(&format!(
        "{tool}\0{}\0{detail}",
        target.as_deref().unwrap_or_default()
    ));
    ToolProgressRecord {
        execution_attempt,
        model_call,
        phase,
        tool: tool.to_owned(),
        target,
        class,
        outcome_signature,
        detail,
        repository_progress,
    }
}

fn implementation_read_progress(
    records: &[ToolProgressRecord],
    execution_attempt: i32,
) -> ImplementationReadProgress {
    let records = records
        .iter()
        .filter(|record| {
            record.execution_attempt == execution_attempt
                && matches!(
                    record.phase,
                    ExecutionPhase::Implementation | ExecutionPhase::Repair
                )
                && matches!(
                    record.tool.as_str(),
                    "read_file" | "search_text" | "related_tests"
                )
        })
        .collect::<Vec<_>>();
    let consecutive_preparation_reads = records
        .iter()
        .rev()
        .take_while(|record| {
            matches!(
                record.class,
                ToolProgressClass::Productive
                    | ToolProgressClass::Neutral
                    | ToolProgressClass::Duplicate
            )
        })
        .count();
    let recoverable_read_failures = records
        .iter()
        .filter(|record| record.class == ToolProgressClass::RecoverableFailure)
        .count();
    let repeated_identical_read_failures = records
        .iter()
        .filter(|record| record.class == ToolProgressClass::RecoverableFailure)
        .fold(BTreeMap::<&str, usize>::new(), |mut counts, record| {
            *counts.entry(record.outcome_signature.as_str()).or_default() += 1;
            counts
        })
        .into_values()
        .max()
        .unwrap_or_default();
    ImplementationReadProgress {
        consecutive_preparation_reads,
        recoverable_read_failures,
        repeated_identical_read_failures,
    }
}

fn unresolved_preparation_blockers(
    records: &[ToolProgressRecord],
    execution_attempt: i32,
    implementation_calls: usize,
    successful_writes: u32,
) -> Vec<String> {
    let mut unresolved = BTreeMap::<(String, Option<String>), (usize, String, String)>::new();
    for (index, record) in records
        .iter()
        .enumerate()
        .filter(|(_, record)| record.execution_attempt == execution_attempt)
    {
        let key = (record.tool.clone(), record.target.clone());
        match record.class {
            ToolProgressClass::RecoverableFailure | ToolProgressClass::BlockingFailure => {
                unresolved.insert(
                    key,
                    (
                        index,
                        record.outcome_signature.clone(),
                        format!(
                            "{}{}: {}",
                            record.tool,
                            record
                                .target
                                .as_deref()
                                .map(|target| format!(" `{target}`"))
                                .unwrap_or_default(),
                            truncate_text(&record.detail, 500),
                        ),
                    ),
                );
            }
            ToolProgressClass::Productive => {
                unresolved.remove(&key);
            }
            ToolProgressClass::Neutral | ToolProgressClass::Duplicate => {}
        }
    }
    let mut unresolved = unresolved.into_values().collect::<Vec<_>>();
    unresolved.sort_by_key(|(index, _, _)| *index);
    let mut seen = BTreeSet::new();
    let mut blockers = unresolved
        .into_iter()
        .rev()
        .filter(|(_, signature, _)| seen.insert(signature.clone()))
        .take(6)
        .map(|(_, _, summary)| summary)
        .collect::<Vec<_>>();
    blockers.reverse();
    if blockers.is_empty()
        && successful_writes == 0
        && implementation_calls >= lifecycle::FIRST_WRITE_DELAY_CALL
    {
        blockers.push(format!(
            "{implementation_calls} implementation turns produced no repository operation or verified mutation; the guided recovery turn must act on the current target"
        ));
    }
    blockers
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PlanningRepairState {
    #[serde(default)]
    valid_planned_changes: Vec<PlannedChange>,
    #[serde(default)]
    valid_planned_change_positions: Vec<usize>,
    #[serde(default)]
    original_change_ids: Vec<Option<String>>,
    #[serde(default)]
    original_change_count: usize,
    #[serde(default)]
    invalid_fields: Vec<String>,
    model_call: usize,
}

#[derive(Clone, Debug, Serialize)]
struct ImplementationStartContext {
    goal: String,
    target_order: Vec<ImplementationTarget>,
    acceptance_criteria_ids: Vec<String>,
    exact_files_already_read: Vec<String>,
    missing_file_contents: Vec<String>,
    source_tree_hash: String,
    remaining_call_budget: usize,
    current_target: Option<ImplementationTarget>,
    guided_recovery: bool,
    unresolved_preparation_blockers: Vec<String>,
    instruction: String,
}

#[derive(Clone, Debug, Serialize)]
struct ImplementationTarget {
    change_id: String,
    path: String,
    role: String,
    new_file: bool,
    intent: String,
    acceptance_criteria: Vec<String>,
    status: IntendedChangeStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WorkerNotebook {
    schema_version: u32,
    revision: u64,
    goal: String,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    acceptance_criteria_v2: Vec<impact_map::AcceptanceCriterion>,
    phase: ExecutionPhase,
    #[serde(default)]
    implementation_substate: ImplementationSubstate,
    repository_base_sha: String,
    branch: String,
    repository_fingerprint: String,
    execution_attempt: i32,
    #[serde(default)]
    architecture_findings: Vec<String>,
    #[serde(default)]
    impact_map: Vec<ImpactArea>,
    #[serde(default)]
    impact_map_v2: Option<ImpactMap>,
    #[serde(default)]
    impact_map_artifact: ArtifactCheckpoint,
    #[serde(default)]
    impact_map_invalid_payload: Option<Value>,
    #[serde(default)]
    impact_evidence: Vec<impact_map::EvidenceReference>,
    #[serde(default)]
    files_inspected: Vec<String>,
    #[serde(default)]
    read_ranges_inspected: Vec<String>,
    #[serde(default)]
    searches_completed: Vec<String>,
    #[serde(default)]
    discovery_paths_sampled: Vec<String>,
    #[serde(default)]
    planned_changes: Vec<PlannedChange>,
    #[serde(default)]
    planning_repair: Option<PlanningRepairState>,
    #[serde(default)]
    intended_changes: Vec<IntendedChangeRecord>,
    #[serde(default)]
    write_attempts: Vec<WriteAttemptRecord>,
    #[serde(default)]
    write_preflight_rejections: Vec<MutationPreflightRecord>,
    #[serde(default)]
    completed_changes: Vec<String>,
    #[serde(default)]
    failed_changes: Vec<ToolFailureRecord>,
    #[serde(default)]
    tool_progress: Vec<ToolProgressRecord>,
    #[serde(default)]
    remaining_work: Vec<String>,
    #[serde(default)]
    remaining_work_v2: Vec<RemainingWorkItem>,
    #[serde(default)]
    blocking_unknowns: Vec<String>,
    #[serde(default)]
    validation_failures: Vec<String>,
    #[serde(default)]
    validation_evidence: Vec<ValidationEvidence>,
    #[serde(default)]
    required_gates: Vec<RequiredGate>,
    #[serde(default)]
    phase_budget: Value,
    #[serde(default)]
    last_successful_action: Value,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LocalizedDiscoveryCoverage {
    provider: bool,
    selector: bool,
    token_source: bool,
    focused_tests: bool,
    validation_commands: bool,
    centralized_abstraction: bool,
    representative_consumers: usize,
}

fn localized_visual_goal(goal: &str) -> bool {
    let goal = goal.to_ascii_lowercase();
    [
        "theme",
        "color scheme",
        "colour scheme",
        "design token",
        "css variable",
        "visual system",
    ]
    .iter()
    .any(|needle| goal.contains(needle))
}

fn localized_discovery_core_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    [
        "themeprovider",
        "theme-provider",
        "themetoggle",
        "theme-toggle",
        "token",
        "variable",
        "global.css",
        "globals.css",
        "test",
        "package.json",
        "cargo.toml",
    ]
    .iter()
    .any(|needle| path.contains(needle))
}

fn localized_discovery_coverage(notebook: &WorkerNotebook) -> LocalizedDiscoveryCoverage {
    let evidence = notebook
        .files_inspected
        .iter()
        .chain(notebook.searches_completed.iter())
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let contains = |needles: &[&str]| {
        evidence
            .iter()
            .any(|value| needles.iter().any(|needle| value.contains(needle)))
    };
    let centralized_abstraction = notebook.architecture_findings.iter().any(|finding| {
        let finding = finding.to_ascii_lowercase();
        (finding.contains("central") || finding.contains("semantic"))
            && (finding.contains("token")
                || finding.contains("variable")
                || finding.contains("theme"))
    });
    LocalizedDiscoveryCoverage {
        provider: contains(&["themeprovider", "theme-provider", "theme provider"]),
        selector: contains(&["themetoggle", "theme-toggle", "theme selector"]),
        token_source: contains(&["globals.css", "global.css", "design token", "css variable"]),
        focused_tests: contains(&["theme-provider.test", "theme-tokens.test", "theme test"]),
        validation_commands: contains(&["package.json", "cargo.toml", "npm test", "npm run"]),
        centralized_abstraction,
        representative_consumers: notebook
            .files_inspected
            .iter()
            .chain(notebook.discovery_paths_sampled.iter())
            .filter(|path| !localized_discovery_core_path(path))
            .collect::<BTreeSet<_>>()
            .len(),
    }
}

fn localized_discovery_should_stop(coverage: LocalizedDiscoveryCoverage) -> bool {
    coverage.centralized_abstraction
        && coverage.provider
        && coverage.selector
        && coverage.token_source
        && coverage.focused_tests
        && coverage.validation_commands
}

fn validate_localized_discovery_scope(
    notebook: &WorkerNotebook,
    requested_paths: &[&str],
) -> Result<()> {
    if !localized_visual_goal(&notebook.goal) {
        return Ok(());
    }
    let coverage = localized_discovery_coverage(notebook);
    if localized_discovery_should_stop(coverage) {
        bail!(
            "localized_discovery_complete: record the compact impact map instead of inspecting more repository files"
        );
    }
    if coverage.centralized_abstraction {
        let new_consumers = requested_paths
            .iter()
            .filter(|path| !localized_discovery_core_path(path))
            .filter(|path| {
                !notebook.files_inspected.iter().any(|seen| seen == **path)
                    && !notebook
                        .discovery_paths_sampled
                        .iter()
                        .any(|seen| seen == **path)
            })
            .copied()
            .collect::<BTreeSet<_>>()
            .len();
        if coverage
            .representative_consumers
            .saturating_add(new_consumers)
            > 3
        {
            bail!(
                "localized_discovery_consumer_limit: centralized theme architecture permits at most three representative consumers"
            );
        }
    }
    Ok(())
}

fn discovery_requested_paths<'a>(
    name: &str,
    arguments: &'a serde_json::Map<String, Value>,
) -> Vec<&'a str> {
    match name {
        "read_file" => arguments
            .get("path")
            .and_then(Value::as_str)
            .into_iter()
            .collect(),
        "search_text" => arguments
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| Path::new(path).extension().is_some())
            .into_iter()
            .collect(),
        "read_files" | "related_tests" => arguments
            .get("paths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect(),
        _ => Vec::new(),
    }
}

fn record_centralized_discovery_finding(notebook: &mut WorkerNotebook, reason: &str) {
    if notebook.phase != ExecutionPhase::Discovery || !localized_visual_goal(&notebook.goal) {
        return;
    }
    let reason = reason.to_ascii_lowercase();
    if (reason.contains("central") || reason.contains("semantic"))
        && (reason.contains("token") || reason.contains("variable") || reason.contains("theme"))
    {
        push_unique(
            &mut notebook.architecture_findings,
            "Centralized semantic theme abstraction confirmed by targeted discovery.".into(),
        );
    }
}

#[derive(Clone, Debug, Serialize)]
struct UnderlyingFailure {
    r#type: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stack_reference: Option<String>,
}

#[derive(Debug, Serialize)]
struct HostedAgentExecutionFailure {
    status: &'static str,
    category: &'static str,
    process_health: &'static str,
    mission_outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocker: Option<String>,
    resumable: bool,
    code: String,
    phase: ExecutionPhase,
    message: String,
    underlying_error: UnderlyingFailure,
    model_calls_used: usize,
    model_calls_limit: usize,
    model_calls_remaining: usize,
    phase_calls_used: usize,
    phase_calls_limit: usize,
    last_successful_action: Value,
    usage: ToolUsage,
    estimated_cost_micros: u64,
    input_tokens: u64,
    output_tokens: u64,
    changed_paths: Vec<String>,
    remaining_work: Vec<RemainingWorkItem>,
    failed_tool_operations: Vec<ToolProgressRecord>,
    current_plan: Vec<PlannedChange>,
    validation_evidence: Vec<ValidationEvidence>,
    notebook_revision: u64,
    recoverable: bool,
    resume_phase: String,
    recommended_action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    semantic_status: Option<ArtifactSemanticStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    persistence_status: Option<ArtifactPersistenceStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rustgrid_gateway_status: Option<Option<u16>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_provider_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_contacted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_budget_consumed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reservation_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reservation_reconciliation_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rustgrid_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_error: Option<ProviderErrorDiagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_response_body: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_provider_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    adapter_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_schema_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_attempts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_cost_micros: Option<u64>,
}

impl std::fmt::Display for HostedAgentExecutionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HostedAgentExecutionFailure {}

fn classify_implementation_preparation_failure(
    mut failure: HostedAgentExecutionFailure,
    remaining_work: &[RemainingWorkItem],
) -> HostedAgentExecutionFailure {
    failure.status = "blocked";
    failure.category = "implementation_blocked";
    failure.process_health = "healthy";
    failure.mission_outcome = "blocked";
    failure.blocker = Some("implementation_preparation_failed".into());
    failure.resumable = true;
    failure.code = "implementation_preparation_failed".into();
    failure.resume_phase = ExecutionPhase::Implementation.as_str().into();
    failure.recommended_action = "Resume in implementation at the current planned target using the persisted read failures and recovery data.".into();
    failure.remaining_work = remaining_work.to_vec();
    failure
}

fn blocked_result_event_payload(
    failure: &HostedAgentExecutionFailure,
    diagnostics: Value,
) -> Value {
    json!({
        "status": "blocked",
        "mission_outcome": "blocked",
        "process_health": "healthy",
        "reason_code": failure.code,
        "resumable": failure.resumable,
        "resume_phase": failure.resume_phase,
        "changed_paths": failure.changed_paths,
        "remaining_work": failure.remaining_work,
        "terminal_telemetry": {
            "model_calls_used": failure.model_calls_used,
            "input_tokens": failure.input_tokens,
            "output_tokens": failure.output_tokens,
            "estimated_cost_micros": failure.estimated_cost_micros,
            "usage": failure.usage,
            "changed_paths": failure.changed_paths,
            "last_successful_action": failure.last_successful_action,
            "phase_reached": failure.phase,
            "plan": failure.current_plan,
            "remaining_work": failure.remaining_work,
            "validation_evidence": failure.validation_evidence,
            "notebook_revision": failure.notebook_revision,
        },
        "failure": diagnostics,
    })
}

fn acceptance_criteria_from_ticket(ticket: &str) -> Vec<String> {
    let mut criteria = Vec::new();
    let mut in_acceptance_criteria = false;
    for line in ticket.lines() {
        let trimmed = line.trim();
        let normalized = trimmed.trim_start_matches('#').trim().to_ascii_lowercase();
        if normalized == "acceptance criteria" {
            in_acceptance_criteria = true;
            continue;
        }
        if in_acceptance_criteria && trimmed.starts_with('#') {
            break;
        }
        if !in_acceptance_criteria {
            continue;
        }
        let item = trimmed
            .strip_prefix("- [ ] ")
            .or_else(|| trimmed.strip_prefix("- [x] "))
            .or_else(|| trimmed.strip_prefix("- [X] "))
            .or_else(|| trimmed.strip_prefix("- "))
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| {
                let (number, item) = trimmed.split_once(". ")?;
                number
                    .chars()
                    .all(|character| character.is_ascii_digit())
                    .then_some(item)
            })
            .map(str::trim)
            .filter(|item| !item.is_empty());
        if let Some(item) = item {
            push_unique(&mut criteria, item.to_owned());
        }
    }
    if criteria.is_empty() && !ticket.trim().is_empty() {
        criteria.push(ticket.trim().to_owned());
    }
    criteria
}

fn project_verification_policy(manifest: &HostedManifest) -> ProjectVerificationPolicy {
    manifest
        .run
        .metadata
        .get("project_verification_policy")
        .or_else(|| manifest.run.metadata.get("browser_test_policy"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .or_else(|| serde_json::from_value(manifest.run.metadata.clone()).ok())
        .unwrap_or_default()
}

fn impact_map_fallback_threshold(manifest: &HostedManifest) -> f64 {
    manifest
        .run
        .metadata
        .get("impact_map_fallback_confidence_threshold")
        .and_then(Value::as_f64)
        .filter(|value| (0.0..=1.0).contains(value))
        .unwrap_or(0.8)
}

fn partial_pr_remaining_work(body: Option<&str>) -> Vec<String> {
    let Some(body) = body else {
        return Vec::new();
    };
    let Some((_, remainder)) = body.split_once("Remaining work:\n") else {
        return Vec::new();
    };
    let section = remainder
        .split_once("\n\nTechnical validation:")
        .map(|(section, _)| section)
        .unwrap_or(remainder);
    section
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- "))
        .map(str::trim)
        .filter(|work| !work.is_empty() && *work != "None reported.")
        .map(str::to_owned)
        .collect()
}

fn detect_partial_run(
    pull_request: Option<&PullRequest>,
    resumed_branch: bool,
    execution_attempt: i32,
    changed_paths: Vec<String>,
) -> Option<PartialRunContext> {
    let pull_request = pull_request?;
    let explicitly_incomplete = pull_request.body.as_deref().is_some_and(|body| {
        body.contains("INCOMPLETE")
            && body.contains("continue implementation before review or merge")
            && body.contains("Remaining work:")
    });
    if execution_attempt <= 1
        || !resumed_branch
        || !pull_request.draft
        || !explicitly_incomplete
        || changed_paths.is_empty()
    {
        return None;
    }
    Some(PartialRunContext {
        pull_request_number: pull_request.number,
        changed_paths,
        remaining_work: partial_pr_remaining_work(pull_request.body.as_deref()),
    })
}

fn new_worker_notebook(
    manifest: &HostedManifest,
    repository_fingerprint: String,
    partial_run: Option<&PartialRunContext>,
) -> WorkerNotebook {
    let acceptance_criteria = acceptance_criteria_from_ticket(&manifest.run.input_prompt);
    let mut notebook = WorkerNotebook {
        schema_version: 1,
        revision: 0,
        goal: manifest.ticket_title.clone(),
        acceptance_criteria: acceptance_criteria.clone(),
        acceptance_criteria_v2: impact_map::acceptance_criteria(&acceptance_criteria),
        phase: ExecutionPhase::Discovery,
        implementation_substate: ImplementationSubstate::Preparing,
        repository_base_sha: manifest.github.base_sha.clone(),
        branch: manifest.github.branch.clone(),
        repository_fingerprint,
        execution_attempt: manifest.execution.attempt_number,
        architecture_findings: Vec::new(),
        impact_map: Vec::new(),
        impact_map_v2: None,
        impact_map_artifact: ArtifactCheckpoint::default(),
        impact_map_invalid_payload: None,
        impact_evidence: Vec::new(),
        files_inspected: Vec::new(),
        read_ranges_inspected: Vec::new(),
        searches_completed: Vec::new(),
        discovery_paths_sampled: Vec::new(),
        planned_changes: Vec::new(),
        planning_repair: None,
        intended_changes: Vec::new(),
        write_attempts: Vec::new(),
        write_preflight_rejections: Vec::new(),
        completed_changes: Vec::new(),
        failed_changes: Vec::new(),
        tool_progress: Vec::new(),
        remaining_work: Vec::new(),
        remaining_work_v2: Vec::new(),
        blocking_unknowns: Vec::new(),
        validation_failures: Vec::new(),
        validation_evidence: Vec::new(),
        required_gates: Vec::new(),
        phase_budget: Value::Null,
        last_successful_action: json!({}),
    };
    if let Some(partial_run) = partial_run {
        notebook.phase = ExecutionPhase::Planning;
        notebook.architecture_findings.push(format!(
            "Recovered draft pull request #{} with {} changed path(s); preserve valid prior work.",
            partial_run.pull_request_number,
            partial_run.changed_paths.len()
        ));
        let criteria_ids = (0..acceptance_criteria.len())
            .map(impact_map::criterion_id)
            .collect();
        notebook.impact_map.push(ImpactArea {
            area_id: "area-existing-partial-implementation".into(),
            name: "Existing partial implementation".into(),
            candidate_paths: partial_run.changed_paths.clone(),
            evidence: partial_run.changed_paths.iter().map(|path| impact_map::ImpactEvidence {
                evidence_type: impact_map::EvidenceType::Inference,
                path: Some(path.clone()), query: None,
                description: "Path was preserved from the resumed draft pull request.".into(),
            }).collect(),
            reason: "A later execution attempt resumed a draft pull request and must reconcile its existing diff before changing more code.".into(),
            acceptance_criteria_ids: criteria_ids,
        });
        notebook.remaining_work = if partial_run.remaining_work.is_empty() {
            vec!["Reconcile the preserved diff against every acceptance criterion.".into()]
        } else {
            partial_run.remaining_work.clone()
        };
        let restored_map = ImpactMap {
            schema_version: IMPACT_MAP_SCHEMA_VERSION.into(),
            areas: notebook.impact_map.clone(),
            inspected_files: partial_run.changed_paths.clone(),
            searches: Vec::new(),
            unresolved_questions: Vec::new(),
        };
        notebook.impact_map_v2 = Some(restored_map.clone());
        notebook.impact_map_artifact = ArtifactCheckpoint {
            artifact: "impact_map".into(),
            semantic_status: ArtifactSemanticStatus::Sufficient,
            serialization_status: ArtifactSerializationStatus::Valid,
            persistence_status: ArtifactPersistenceStatus::PendingRetry,
            artifact_sha256: impact_map_sha256(&restored_map),
            model_call_index: None,
            phase: ExecutionPhase::Planning,
            safe_error: None,
            artifact_source: Some(ArtifactSource::OrchestratorFallback),
            confidence: Some(1.0),
            failure_layer: None,
            validation_errors: Vec::new(),
            invalid_payload_shape: None,
        };
    }
    notebook
}

fn notebook_orchestration_state(
    notebook: &WorkerNotebook,
) -> (
    Option<ImpactMap>,
    Option<ImplementationPlan>,
    ExecutionPhase,
) {
    let impact_map = notebook.impact_map_v2.clone().or_else(|| {
        (!notebook.impact_map.is_empty()).then(|| ImpactMap {
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
        })
    });
    let implementation_plan = (!notebook.planned_changes.is_empty()).then(|| ImplementationPlan {
        implementation_status: "ready".into(),
        planned_changes: notebook.planned_changes.clone(),
        planned_new_files: Vec::new(),
        planned_test_changes: Vec::new(),
        remaining_unknowns: Vec::new(),
        blocking_unknowns: notebook.blocking_unknowns.clone(),
    });
    let phase = if implementation_plan.is_some() {
        ExecutionPhase::Implementation
    } else if impact_map.is_some() {
        ExecutionPhase::Planning
    } else if notebook.phase == ExecutionPhase::ArtifactRepair {
        ExecutionPhase::ArtifactRepair
    } else {
        ExecutionPhase::Discovery
    };
    (impact_map, implementation_plan, phase)
}

fn reconcile_failed_write_attempts(
    failures: &mut [ToolFailureRecord],
    planned_changes: &[PlannedChange],
    write_attempts: &[WriteAttemptRecord],
    implementation: &ImplementationOutcome,
    validation: &[ValidationResult],
    changed_paths: &[String],
) {
    let changed = changed_paths
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let all_validation_passed =
        !validation.is_empty() && validation.iter().all(|result| result.status == "passed");
    let declaration = implementation.explicit_declaration.as_ref();
    let declaration_complete =
        declaration.is_some_and(|value| value.implementation_status == "complete");
    let successful_attempts = write_attempts
        .iter()
        .filter(|attempt| {
            attempt.status == WriteAttemptStatus::Applied && attempt_modified_target(attempt)
        })
        .collect::<Vec<_>>();
    let planned_by_id = planned_changes
        .iter()
        .map(|change| (change.change_id.as_str(), change))
        .collect::<BTreeMap<_, _>>();

    for failure in failures {
        if failure.recovered {
            continue;
        }
        let Some(target) = failure.target.as_deref() else {
            failure.reconciliation = FailureReconciliation::Unrelated;
            continue;
        };
        let planned = failure
            .change_id
            .as_deref()
            .and_then(|change_id| planned_by_id.get(change_id).copied())
            .or_else(|| {
                planned_by_id
                    .values()
                    .copied()
                    .find(|change| change.targets.iter().any(|planned| planned.path == target))
            });
        let later_success = successful_attempts.iter().find(|attempt| {
            attempt.attempt_index > failure.attempt_index
                && attempt.target == target
                && (failure.change_id.as_deref() == Some(attempt.change_id.as_str())
                    || matches!(attempt.tool.as_str(), "write_file" | "rewrite_small_file"))
        });
        if let Some(success) = later_success {
            failure.recovered = true;
            failure.reconciliation = FailureReconciliation::Superseded;
            failure.recovery = Some(IntendedChangeRecovery {
                recovered: true,
                method: "later_successful_target_write".into(),
                evidence: vec![
                    format!(
                        "{target} was modified by a later successful {}.",
                        success.tool
                    ),
                    format!(
                        "The final target hash is {}.",
                        success.after_sha256.as_deref().unwrap_or("recorded")
                    ),
                ],
            });
            continue;
        }
        if !changed.contains(target) {
            failure.reconciliation = if planned.is_some() {
                FailureReconciliation::StillUnresolved
            } else {
                FailureReconciliation::Unrelated
            };
            continue;
        }
        let declaration_maps_target = declaration.is_some_and(|value| {
            value.changed_paths.iter().any(|path| path == target)
                && (value
                    .criteria_evidence
                    .iter()
                    .any(|evidence| evidence.paths.iter().any(|path| path == target))
                    || !value.completed_work.is_empty())
        });
        if planned.is_some()
            && declaration_complete
            && declaration_maps_target
            && all_validation_passed
        {
            failure.recovered = true;
            failure.reconciliation = FailureReconciliation::Recovered;
            failure.recovery = Some(IntendedChangeRecovery {
                recovered: true,
                method: "final_diff_and_validation".into(),
                evidence: std::iter::once(format!("{target} is present in the final diff."))
                    .chain(
                        validation
                            .iter()
                            .map(|result| format!("{} passed.", result.command)),
                    )
                    .collect(),
            });
        } else {
            failure.reconciliation = FailureReconciliation::StillUnresolved;
        }
    }
}

fn attempt_modified_target(attempt: &WriteAttemptRecord) -> bool {
    attempt.before_sha256 != attempt.after_sha256
}

fn deterministic_complete_declaration(
    planned_changes: &[PlannedChange],
    acceptance_criteria: &[String],
    changed_paths: &[String],
    remaining_work: &[RemainingWorkItem],
    tool_failures: &[ToolFailureRecord],
) -> Option<ImplementationDeclaration> {
    if planned_changes.is_empty()
        || !remaining_work.is_empty()
        || tool_failures.iter().any(|failure| {
            !failure.recovered && failure.reconciliation == FailureReconciliation::StillUnresolved
        })
    {
        return None;
    }
    let changed = changed_paths
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if planned_changes
        .iter()
        .flat_map(|change| &change.targets)
        .any(|target| !changed.contains(target.path.as_str()))
    {
        return None;
    }
    let criteria_evidence = if acceptance_criteria.is_empty() {
        planned_changes
            .iter()
            .map(|change| ImplementationCriterionEvidence {
                criterion: change.change.clone(),
                paths: change
                    .targets
                    .iter()
                    .map(|target| target.path.clone())
                    .collect(),
                evidence: format!(
                    "The authoritative diff contains every target for planned change `{}` and all required gates passed.",
                    change.change_id
                ),
            })
            .collect::<Vec<_>>()
    } else {
        acceptance_criteria
            .iter()
            .map(|criterion| {
                let mut paths = planned_changes
                    .iter()
                    .filter(|change| {
                        change
                            .acceptance_criteria
                            .iter()
                            .any(|mapped| mapped.trim() == criterion.trim())
                    })
                    .flat_map(|change| change.targets.iter().map(|target| target.path.clone()))
                    .collect::<Vec<_>>();
                paths.sort();
                paths.dedup();
                (!paths.is_empty()).then(|| ImplementationCriterionEvidence {
                    criterion: criterion.clone(),
                    paths,
                    evidence: "The authoritative target reconciliation, repository diff, and required validation gates all passed.".into(),
                })
            })
            .collect::<Option<Vec<_>>>()?
    };
    Some(ImplementationDeclaration {
        implementation_status: "complete".into(),
        completed_work: planned_changes
            .iter()
            .map(|change| change.change.clone())
            .collect(),
        remaining_work: Vec::new(),
        known_risks: Vec::new(),
        changed_paths: changed_paths.to_vec(),
        criteria_evidence,
    })
}

fn deterministic_change_id(index: usize, change: &PlannedChange) -> String {
    let material = format!(
        "{}\0{}\0{}",
        change
            .targets
            .iter()
            .map(|target| target.path.as_str())
            .collect::<Vec<_>>()
            .join("\0"),
        change.change,
        change.reason
    );
    let digest = hex::encode(Sha256::digest(material.as_bytes()));
    format!("change-{}-{}", index + 1, &digest[..12])
}

fn normalized_planned_paths(raw: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for entry in raw.split(';') {
        let path = entry.trim().replace('\\', "/");
        if path.is_empty() {
            bail!("implementation plan target contains an empty path entry");
        }
        let path = path.strip_prefix("./").unwrap_or(&path).to_owned();
        if path.contains(';') || path.contains('\n') || path.contains('\r') {
            bail!("implementation plan target must contain exactly one repository path");
        }
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn normalize_planned_changes(changes: &mut [PlannedChange]) -> Result<usize> {
    let mut ids = BTreeSet::new();
    let mut normalized_legacy_targets = 0;
    for (index, change) in changes.iter_mut().enumerate() {
        if !change.path.trim().is_empty() {
            let normalized = normalized_planned_paths(&change.path)?;
            normalized_legacy_targets += usize::from(normalized.len() > 1);
            for path in normalized {
                if !change.targets.iter().any(|target| target.path == path) {
                    change.targets.push(PlannedTarget {
                        path,
                        role: change.reason.clone(),
                        new_file: false,
                        status: IntendedChangeStatus::Planned,
                    });
                }
            }
            change.path.clear();
        }
        let mut seen_paths = BTreeSet::new();
        let mut targets = Vec::new();
        for mut target in std::mem::take(&mut change.targets) {
            let normalized = normalized_planned_paths(&target.path)?;
            normalized_legacy_targets += usize::from(normalized.len() > 1);
            for path in normalized {
                if seen_paths.insert(path.clone()) {
                    target.path = path;
                    if target.role.trim().is_empty() {
                        target.role = change.reason.clone();
                    }
                    targets.push(target.clone());
                }
            }
        }
        change.targets = targets;
        if change.targets.is_empty() {
            bail!("every implementation plan change requires at least one target");
        }
        if change.change_id.trim().is_empty() {
            change.change_id = deterministic_change_id(index, change);
        }
        if change.change_id.len() > 100
            || !change.change_id.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '-' | '_')
            })
            || !ids.insert(change.change_id.clone())
        {
            bail!("implementation plan change_id values must be unique safe identifiers");
        }
        if change.parent_change_id.as_deref().is_some_and(|parent| {
            parent.is_empty()
                || parent.len() > 100
                || !parent.chars().all(|character| {
                    character.is_ascii_lowercase()
                        || character.is_ascii_digit()
                        || matches!(character, '-' | '_')
                })
        }) {
            bail!("implementation plan parent_change_id must be a safe identifier");
        }
    }
    Ok(normalized_legacy_targets)
}

fn recover_planning_repair_state(
    root: &Path,
    object: &serde_json::Map<String, Value>,
    model_call: usize,
) -> PlanningRepairState {
    let mut state = PlanningRepairState {
        model_call,
        ..PlanningRepairState::default()
    };
    let Some(changes) = object.get("planned_changes").and_then(Value::as_array) else {
        state
            .invalid_fields
            .push("$.planned_changes: expected an array".into());
        return state;
    };
    state.original_change_count = changes.len();
    let mut ids = BTreeSet::new();
    for (index, raw) in changes.iter().enumerate() {
        state.original_change_ids.push(
            raw.get("change_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
        let mut change = match serde_json::from_value::<PlannedChange>(raw.clone()) {
            Ok(change) => change,
            Err(error) => {
                state.invalid_fields.push(format!(
                    "$.planned_changes[{index}]: {}",
                    truncate_text(&error.to_string(), 500)
                ));
                continue;
            }
        };
        if change.change_id.trim().is_empty() {
            change.change_id = deterministic_change_id(index, &change);
        }
        let mut one = vec![change];
        let validation = normalize_planned_changes(&mut one)
            .and_then(|_| validate_planned_change_paths(root, &one))
            .and_then(|_| {
                let change = &one[0];
                if change.change.trim().is_empty() {
                    bail!("intent is required");
                }
                if change.reason.trim().is_empty() {
                    bail!("reason is required");
                }
                if change.acceptance_criteria.is_empty() {
                    bail!("acceptance_criteria requires at least one entry");
                }
                if !ids.insert(change.change_id.clone()) {
                    bail!("change_id must be unique");
                }
                Ok(())
            });
        match validation {
            Ok(()) => {
                state.valid_planned_change_positions.push(index);
                state.valid_planned_changes.push(one.remove(0));
            }
            Err(error) => state.invalid_fields.push(format!(
                "$.planned_changes[{index}]: {}",
                truncate_text(&error.to_string(), 500)
            )),
        }
    }
    state
}

fn merge_preserved_plan_fragments(
    planned_changes: &mut Vec<PlannedChange>,
    repair: Option<&PlanningRepairState>,
) {
    let Some(repair) = repair else {
        return;
    };
    if repair.valid_planned_change_positions.len() == repair.valid_planned_changes.len()
        && repair.original_change_count > 0
    {
        let preserved = repair
            .valid_planned_change_positions
            .iter()
            .copied()
            .zip(repair.valid_planned_changes.iter().cloned())
            .collect::<BTreeMap<_, _>>();
        planned_changes.retain(|candidate| {
            !repair
                .valid_planned_changes
                .iter()
                .any(|valid| valid.change_id == candidate.change_id)
        });
        let mut repaired = std::mem::take(planned_changes);
        let mut merged = Vec::with_capacity(repair.original_change_count + repaired.len());
        for index in 0..repair.original_change_count {
            if let Some(valid) = preserved.get(&index) {
                merged.push(valid.clone());
                continue;
            }
            let original_id = repair
                .original_change_ids
                .get(index)
                .and_then(Option::as_deref);
            let repaired_index = original_id
                .and_then(|id| repaired.iter().position(|change| change.change_id == id))
                .unwrap_or_default();
            if !repaired.is_empty() {
                merged.push(repaired.remove(repaired_index.min(repaired.len() - 1)));
            }
        }
        merged.append(&mut repaired);
        *planned_changes = merged;
        return;
    }
    for preserved in &repair.valid_planned_changes {
        if !planned_changes
            .iter()
            .any(|change| change.change_id == preserved.change_id)
        {
            planned_changes.push(preserved.clone());
        }
    }
}

fn repair_implementation_plan(
    changes: &mut [PlannedChange],
    change_id: &str,
    attempted_concrete_path: &str,
) -> Result<Option<ImplementationPlanRepair>> {
    let targets_before = changes
        .iter()
        .find(|change| change.change_id == change_id)
        .map(|change| {
            change
                .path
                .trim()
                .is_empty()
                .then(Vec::new)
                .unwrap_or_else(|| vec![change.path.clone()])
                .into_iter()
                .chain(change.targets.iter().map(|target| target.path.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let normalized_legacy_targets = normalize_planned_changes(changes)?;
    if normalized_legacy_targets == 0 {
        return Ok(None);
    }
    let targets_after = changes
        .iter()
        .find(|change| change.change_id == change_id)
        .map(|change| {
            change
                .targets
                .iter()
                .map(|target| target.path.clone())
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(ImplementationPlanRepair {
        change_id: change_id.to_owned(),
        targets_before,
        targets_after,
        attempted_concrete_path: attempted_concrete_path.to_owned(),
        validation_error: "legacy compound target metadata required normalization".into(),
        repair_source: "orchestrator_normalization",
        model_call_consumed: false,
    }))
}

fn record_mutation_preflight_rejection(
    notebook: &mut WorkerNotebook,
    usage: &mut ToolUsage,
    preflight: &MutationPreflightError,
) -> MutationPreflightDecision {
    usage.write_preflight_rejections = usage.write_preflight_rejections.saturating_add(1);
    let repeated_index = notebook
        .write_preflight_rejections
        .iter()
        .position(|record| {
            record.change_id == preflight.change_id
                && record.target == preflight.target
                && record.failure_code == preflight.code
                && record.plan_revision == notebook.revision
        });
    let repeated = repeated_index.is_some();
    if let Some(index) = repeated_index {
        notebook.write_preflight_rejections[index].occurrences = notebook
            .write_preflight_rejections[index]
            .occurrences
            .saturating_add(1);
    } else {
        notebook
            .write_preflight_rejections
            .push(MutationPreflightRecord {
                change_id: preflight.change_id.clone(),
                target: preflight.target.clone(),
                failure_code: preflight.code.into(),
                plan_revision: notebook.revision,
                retryable_with_same_plan: false,
                repair_strategy: preflight.repair_strategy.into(),
                mutation_attempted: false,
                mutation_preflight_failed: true,
                deterministic_repair_attempted: preflight.repair_strategy == "repair_plan_metadata",
                occurrences: 1,
            });
    }
    MutationPreflightDecision {
        repeated,
        halt_orchestration: true,
    }
}

fn validate_planned_change_paths(root: &Path, changes: &[PlannedChange]) -> Result<()> {
    for change in changes {
        for target in &change.targets {
            if target.path.contains(';') {
                bail!("invalid multi-path scalar target cannot reach implementation");
            }
            let may_be_absent = target.new_file
                || matches!(
                    target.status,
                    IntendedChangeStatus::Applied | IntendedChangeStatus::Verified
                );
            let resolved = safe_repo_path(root, &target.path, may_be_absent).map_err(|error| {
                anyhow!(
                    "implementation plan target `{}` is invalid: {error:#}",
                    target.path
                )
            })?;
            if !may_be_absent && !resolved.exists() {
                bail!(
                    "implementation plan target `{}` does not exist and is not marked new_file",
                    target.path
                );
            }
        }
    }
    Ok(())
}

fn authorize_planned_target<'a>(
    plan: &'a ImplementationPlan,
    change_id: &str,
    path: &str,
) -> std::result::Result<&'a PlannedTarget, MutationPreflightError> {
    let Some(change) = plan
        .planned_changes
        .iter()
        .find(|change| change.change_id == change_id)
    else {
        return Err(MutationPreflightError {
            code: "mutation_change_id_unknown",
            change_id: change_id.into(),
            target: path.into(),
            message: "source-changing tool change_id is not in the implementation plan".into(),
            repair_strategy: "repair_plan_metadata",
        });
    };
    change
        .targets
        .iter()
        .find(|target| target.path == path)
        .ok_or_else(|| MutationPreflightError {
            code: "mutation_plan_metadata_mismatch",
            change_id: change_id.into(),
            target: path.into(),
            message: "source-changing tool target is not a member of its planned target set".into(),
            repair_strategy: "repair_plan_metadata",
        })
}

fn roll_up_target_statuses(targets: &[PlannedTarget]) -> IntendedChangeStatus {
    if !targets.is_empty()
        && targets
            .iter()
            .all(|target| target.status == IntendedChangeStatus::Verified)
    {
        IntendedChangeStatus::Verified
    } else if !targets.is_empty()
        && targets
            .iter()
            .all(|target| target.status == IntendedChangeStatus::Applied)
    {
        IntendedChangeStatus::Applied
    } else if !targets.is_empty()
        && targets
            .iter()
            .all(|target| target.status == IntendedChangeStatus::Unresolved)
    {
        IntendedChangeStatus::Unresolved
    } else if targets.iter().any(|target| {
        matches!(
            target.status,
            IntendedChangeStatus::InProgress
                | IntendedChangeStatus::Applied
                | IntendedChangeStatus::Verified
                | IntendedChangeStatus::Partial
                | IntendedChangeStatus::Unresolved
        )
    }) {
        IntendedChangeStatus::Partial
    } else {
        IntendedChangeStatus::Planned
    }
}

fn reconcile_changed_target_statuses(
    intended_changes: &mut [IntendedChangeRecord],
    changed_paths: &BTreeSet<String>,
) {
    for intended in intended_changes {
        for target in &mut intended.targets {
            let repository_contains_target_change = changed_paths.contains(&target.path);
            target.status = match (repository_contains_target_change, target.status) {
                (false, IntendedChangeStatus::Applied | IntendedChangeStatus::Verified) => {
                    IntendedChangeStatus::Planned
                }
                (true, IntendedChangeStatus::Planned) => IntendedChangeStatus::InProgress,
                _ => target.status,
            };
        }
        intended.status = roll_up_target_statuses(&intended.targets);
    }
}

fn intended_changes_from_plan(changes: &[PlannedChange]) -> Vec<IntendedChangeRecord> {
    changes
        .iter()
        .map(|change| IntendedChangeRecord {
            change_id: change.change_id.clone(),
            intent: change.change.clone(),
            status: IntendedChangeStatus::Planned,
            target: String::new(),
            targets: change.targets.clone(),
            attempts: Vec::new(),
            recovery: None,
        })
        .collect()
}

fn normalize_notebook_intended_changes(notebook: &mut WorkerNotebook, root: &Path) -> Result<()> {
    normalize_planned_changes(&mut notebook.planned_changes)?;
    if notebook.intended_changes.is_empty() && !notebook.planned_changes.is_empty() {
        notebook.intended_changes = intended_changes_from_plan(&notebook.planned_changes);
    }
    for intended in &mut notebook.intended_changes {
        if !intended.target.trim().is_empty() {
            for path in normalized_planned_paths(&intended.target)? {
                if !intended.targets.iter().any(|target| target.path == path) {
                    intended.targets.push(PlannedTarget {
                        path,
                        role: intended.intent.clone(),
                        new_file: false,
                        status: intended.status,
                    });
                }
            }
            intended.target.clear();
        }
        let mut normalized_targets = Vec::new();
        let mut seen_paths = BTreeSet::new();
        for target in std::mem::take(&mut intended.targets) {
            for path in normalized_planned_paths(&target.path)? {
                if seen_paths.insert(path.clone()) {
                    normalized_targets.push(PlannedTarget {
                        path,
                        role: if target.role.trim().is_empty() {
                            intended.intent.clone()
                        } else {
                            target.role.clone()
                        },
                        new_file: target.new_file,
                        status: target.status,
                    });
                }
            }
        }
        intended.targets = normalized_targets;
        if intended.targets.is_empty() {
            bail!(
                "persisted intended change `{}` requires at least one target",
                intended.change_id
            );
        }
        intended.status = roll_up_target_statuses(&intended.targets);
    }
    for planned in &mut notebook.planned_changes {
        if let Some(intended) = notebook
            .intended_changes
            .iter()
            .find(|intended| intended.change_id == planned.change_id)
        {
            for target in &mut planned.targets {
                if let Some(persisted) = intended
                    .targets
                    .iter()
                    .find(|persisted| persisted.path == target.path)
                {
                    target.status = persisted.status;
                    target.new_file |= persisted.new_file;
                }
            }
            planned.status = intended.status;
        }
    }
    validate_planned_change_paths(root, &notebook.planned_changes)?;
    if notebook.write_attempts.is_empty() {
        notebook.write_attempts = notebook
            .intended_changes
            .iter()
            .flat_map(|change| change.attempts.clone())
            .collect();
    }
    Ok(())
}

fn validate_write_repair_strategy(
    attempts: &[WriteAttemptRecord],
    target: &str,
    change_id: &str,
    tool: &str,
    bounded_repair_read_completed: bool,
) -> Result<()> {
    let target_failures = attempts
        .iter()
        .filter(|attempt| attempt.target == target && attempt.status == WriteAttemptStatus::Failed)
        .count();
    let ambiguous_failures = attempts
        .iter()
        .filter(|attempt| {
            attempt.change_id == change_id
                && attempt.target == target
                && attempt.status == WriteAttemptStatus::Failed
                && (attempt.error_code.as_deref() == Some("replace_match_not_unique")
                    || (attempt.error_code.as_deref() == Some("mutation_content_conflict")
                        && attempt.match_count.is_some_and(|count| count != 1)))
        })
        .count();
    if tool == "replace_text" {
        if ambiguous_failures >= MAX_AMBIGUOUS_REPLACEMENT_FAILURES {
            bail!(
                "replace_text strategy exhausted for {target}; use replace_range, insert_after_symbol, insert_before_symbol, apply_unified_diff, or rewrite_small_file"
            );
        }
        if ambiguous_failures == 1 && !bounded_repair_read_completed {
            bail!(
                "a bounded read_file around the intended location is required before retrying replace_text for {target}"
            );
        }
    }
    if target_failures >= MAX_TARGET_REPAIR_FAILURES {
        bail!(
            "content repair circuit breaker opened for {target} after {MAX_TARGET_REPAIR_FAILURES} executed write failures"
        );
    }
    Ok(())
}

fn validate_impact_map(map: &ImpactMap, notebook: &WorkerNotebook) -> Result<()> {
    let errors = impact_map::validate(map, notebook.acceptance_criteria.len());
    if !errors.is_empty() {
        bail!(
            "{}",
            serde_json::to_string(&json!({
                "code": "impact_map_schema_mismatch",
                "errors": errors,
            }))?
        );
    }
    Ok(())
}

fn impact_map_from_value(
    value: Value,
    notebook: &WorkerNotebook,
) -> Result<(ImpactMap, ArtifactSource)> {
    impact_map::normalize(
        &value,
        &notebook.files_inspected,
        &notebook.searches_completed,
        &notebook.acceptance_criteria,
    )
    .map_err(|errors| {
        anyhow!(
            serde_json::to_string(&json!({
                "code":"impact_map_schema_mismatch", "errors":errors
            }))
            .unwrap_or_else(|_| "impact_map_schema_mismatch".into())
        )
    })
}

fn json_object_from_text(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed)
        && value.is_object()
    {
        return Some(value);
    }
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim);
    if let Some(unfenced) = unfenced
        && let Ok(value) = serde_json::from_str::<Value>(unfenced)
        && value.is_object()
    {
        return Some(value);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (start < end)
        .then(|| serde_json::from_str::<Value>(&trimmed[start..=end]).ok())
        .flatten()
        .filter(Value::is_object)
}

fn recover_impact_map(
    raw_arguments: Option<&str>,
    assistant_text: Option<&str>,
    notebook: &WorkerNotebook,
) -> Result<(ImpactMap, ArtifactSource)> {
    let mut errors = Vec::new();
    for candidate in [
        raw_arguments.and_then(json_object_from_text),
        assistant_text.and_then(json_object_from_text),
    ]
    .into_iter()
    .flatten()
    {
        match impact_map_from_value(candidate, notebook) {
            Ok(map) => return Ok(map),
            Err(error) => errors.push(error.to_string()),
        }
    }
    bail!(
        "impact map recovery found no valid structured artifact{}",
        if errors.is_empty() {
            String::new()
        } else {
            format!(": {}", errors.join("; "))
        }
    )
}

fn impact_map_sha256(map: &ImpactMap) -> Option<String> {
    serde_json::to_vec(map)
        .ok()
        .map(|encoded| hex::encode(Sha256::digest(encoded)))
}

fn classify_impact_map_failure(error: &anyhow::Error) -> ImpactMapFailure {
    let safe_error = truncate_text(&format!("{error:#}"), 2_000);
    let lower = safe_error.to_ascii_lowercase();
    let code = if lower.contains("valid json")
        || lower.contains("strict artifact schema")
        || lower.contains("malformed")
    {
        "impact_map_schema_mismatch"
    } else if lower.contains("persist")
        || lower.contains("worker-events")
        || lower.contains("transport")
    {
        "impact_map_persistence_failed"
    } else if lower.contains("impact map") {
        "impact_map_invalid"
    } else {
        "impact_map_tool_failure"
    };
    ImpactMapFailure {
        code,
        safe_error,
        errors: Vec::new(),
        invalid_payload: Value::Null,
        invalid_payload_shape: impact_map::safe_shape(&Value::Null),
        failure_layer: ArtifactFailureLayer::WorkerToolSchemaValidation,
    }
}

fn invalid_impact_map_semantic_status(value: &Value) -> ArtifactSemanticStatus {
    let areas = value
        .as_array()
        .or_else(|| value.get("areas").and_then(Value::as_array))
        .or_else(|| value.get("impact_map").and_then(Value::as_array));
    if areas.is_some_and(|areas| {
        areas.iter().any(|area| {
            area.get("name")
                .or_else(|| area.get("area"))
                .and_then(Value::as_str)
                .is_some_and(|v| !v.trim().is_empty())
                && area
                    .get("candidate_paths")
                    .and_then(Value::as_array)
                    .is_some_and(|v| !v.is_empty())
                && area
                    .get("reason")
                    .and_then(Value::as_str)
                    .is_some_and(|v| !v.trim().is_empty())
        })
    }) {
        ArtifactSemanticStatus::Partial
    } else if value.is_null() {
        ArtifactSemanticStatus::Missing
    } else {
        ArtifactSemanticStatus::Invalid
    }
}

impl HostedManifest {
    fn budget_audit(&self) -> Result<BudgetAudit> {
        let worker_received = self.ai_gateway.maximum_model_calls;
        let execution = self.execution.maximum_model_calls;
        let has_canonical_contract = self.model_call_budget.is_some()
            || self.requested_model_call_budget.is_some()
            || self.resolved_model_call_budget.is_some()
            || self.budget_source.is_some()
            || self.clamped.is_some()
            || self.clamp_reason.is_some();
        if has_canonical_contract {
            let requested = self.requested_model_call_budget;
            let resolved = self.resolved_model_call_budget;
            let canonical = self.model_call_budget;
            let clamped = self.clamped;
            let exact_match = requested.is_some()
                && requested == resolved
                && resolved == canonical
                && canonical == execution
                && canonical == Some(worker_received)
                && self.budget_source.is_some()
                && clamped == Some(false)
                && self.clamp_reason == Some(None);
            if !exact_match {
                return Err(anyhow!(ExecutionBudgetMismatch {
                    requested,
                    resolved,
                    canonical,
                    execution,
                    worker_received,
                }));
            }
            return Ok(BudgetAudit {
                requested_model_call_budget: requested.expect("checked above"),
                resolved_model_call_budget: resolved.expect("checked above"),
                worker_received_model_call_budget: worker_received,
                budget_source: self.budget_source,
                clamped: false,
                clamp_reason: self.clamp_reason.clone().flatten(),
                contract: "canonical",
            });
        }
        if self.manifest_version >= 4 || execution != Some(worker_received) {
            return Err(anyhow!(ExecutionBudgetMismatch {
                requested: None,
                resolved: None,
                canonical: None,
                execution,
                worker_received,
            }));
        }
        Ok(BudgetAudit {
            requested_model_call_budget: worker_received,
            resolved_model_call_budget: worker_received,
            worker_received_model_call_budget: worker_received,
            budget_source: None,
            clamped: false,
            clamp_reason: None,
            contract: "legacy_signed_manifest",
        })
    }

    fn validate(
        &self,
        execution_id: Uuid,
        environment: &GithubActionsEnvironment,
        api: &HostedApiClient,
    ) -> Result<()> {
        let github_execution = self
            .execution
            .github_actions
            .as_ref()
            .context("RustGrid execution manifest has no GitHub Actions correlation")?;
        if !(3..=4).contains(&self.manifest_version)
            || self.execution.execution_id != execution_id
            || self.run.id != execution_id
            || self.run.ticket_id != self.ticket_id
            || self.project_id.is_nil()
            || self.project_id != api.project_id
            || self.execution.status != "running"
            || self.execution.attempt_number < 1
            || self.execution.attempt_number != api.execution_attempt
            || self.run.attempt != self.execution.attempt_number
        {
            bail!("RustGrid execution manifest identity or state is invalid");
        }
        for (name, value) in [
            ("project key", self.project_key.as_str()),
            ("project name", self.project_name.as_str()),
            ("ticket key", self.ticket_key.as_str()),
            ("ticket title", self.ticket_title.as_str()),
            ("mission prompt", self.run.input_prompt.as_str()),
            ("repository", self.github.repository.as_str()),
        ] {
            if value.trim().is_empty() {
                bail!("RustGrid execution manifest has an empty {name}");
            }
        }
        if self.github.repository_id < 1 || self.github.installation_id < 1 {
            bail!("RustGrid execution manifest has invalid GitHub identities");
        }
        let (owner, name) = self
            .github
            .repository
            .split_once('/')
            .context("execution repository must be owner/name")?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            bail!("execution repository must be owner/name");
        }
        if self.github.repository_id != api.repository_id
            || environment
                .repository
                .as_deref()
                .is_some_and(|repository| !repository.eq_ignore_ascii_case(&self.github.repository))
            || environment
                .repository_id
                .is_some_and(|repository_id| repository_id != self.github.repository_id as u64)
            || environment.workflow_run_id.is_some_and(|run_id| {
                run_id != api.github_workflow_run_id
                    || github_execution.workflow_run_id != Some(run_id)
            })
            || environment
                .workflow_run_attempt
                .is_some_and(|attempt| github_execution.workflow_run_attempt != Some(attempt))
        {
            bail!("GitHub workflow repository does not match the execution manifest");
        }

        let short_id = execution_id.simple().to_string();
        let expected_branch = format!(
            "rustgrid/{}-{}",
            self.ticket_key.to_ascii_lowercase(),
            &short_id[..8]
        );
        if self.github.branch != expected_branch || !safe_git_ref(&self.github.branch) {
            bail!("execution manifest branch is not deterministic or safe");
        }
        let base_ref = normalized_base_ref(&self.github.base_ref)?;
        if !safe_git_ref(base_ref)
            || !commit_sha(&self.github.base_sha)
            || environment.sha.as_deref() != Some(self.github.base_sha.as_str())
        {
            bail!("execution manifest base ref or immutable checkout SHA is invalid");
        }

        let clone = secure_url("execution manifest clone_url", &self.github.clone_url)?;
        let web = secure_url("execution manifest web_base_url", &self.github.web_base_url)?;
        if clone.host_str() != web.host_str() || clone.query().is_some() || web.query().is_some() {
            bail!("execution manifest GitHub URLs use different hosts");
        }
        let expected_base = format!("executions/{execution_id}");
        for (name, value, suffix) in [
            (
                "github_token_url",
                self.github.github_token_url.as_str(),
                "github-token",
            ),
            ("heartbeat_url", self.heartbeat_url.as_str(), "heartbeat"),
            (
                "token_refresh_url",
                self.token_refresh_url.as_str(),
                "token/refresh",
            ),
            ("events_url", self.events_url.as_str(), "worker-events"),
            (
                "telemetry_url",
                self.telemetry_url.as_str(),
                "telemetry/batch",
            ),
            ("state_url", self.state_url.as_str(), "state"),
            ("complete_url", self.complete_url.as_str(), "complete"),
            (
                "responses_url",
                self.ai_gateway.responses_url.as_str(),
                "ai/responses",
            ),
        ] {
            validate_manifest_endpoint(
                name,
                value,
                &environment.api_root,
                &format!("{expected_base}/{suffix}"),
            )?;
        }

        let maximum_cost = self.ai_gateway.maximum_cost_usd.parse::<f64>();
        let budget = self.budget_audit()?;
        if self.ai_gateway.model.trim().is_empty()
            || self.ai_gateway.model.len() > 100
            || self.ai_gateway.model.chars().any(char::is_whitespace)
            || self.execution.model.as_deref() != Some(self.ai_gateway.model.as_str())
            || self.ai_gateway.maximum_input_tokens < 1
            || self.ai_gateway.maximum_output_tokens < 1
            || !(MINIMUM_HOSTED_MODEL_CALLS as i32..=MAX_MODEL_CALLS_HARD_LIMIT as i32)
                .contains(&budget.worker_received_model_call_budget)
            || maximum_cost.is_err()
            || maximum_cost.is_ok_and(|value| !value.is_finite() || value <= 0.0)
            || self.execution.maximum_input_tokens != Some(self.ai_gateway.maximum_input_tokens)
            || self.execution.maximum_output_tokens != Some(self.ai_gateway.maximum_output_tokens)
            || self.execution.maximum_cost_usd.as_deref()
                != Some(self.ai_gateway.maximum_cost_usd.as_str())
            || self
                .execution
                .maximum_duration_seconds
                .is_none_or(|seconds| seconds < 30)
        {
            bail!("execution manifest AI policy does not match the resolved execution");
        }

        let encoded_policy = serde_json::to_vec(&self.execution_policy)
            .context("could not hash execution policy")?;
        let actual_policy_sha256 = hex::encode(Sha256::digest(encoded_policy));
        if actual_policy_sha256 != self.execution_policy_sha256 {
            bail!("execution policy hash does not match the manifest payload");
        }
        self.execution_policy.validate()?;
        if self.execution_policy.timeout_seconds
            > i64::from(
                self.execution
                    .maximum_duration_seconds
                    .expect("validated above"),
            )
        {
            bail!("execution policy timeout exceeds the mission duration limit");
        }
        if !self.run.metadata.is_object() {
            bail!("execution manifest run metadata must be an object");
        }
        Ok(())
    }

    fn repo_config(&self) -> Result<RepoConfig> {
        let (owner, name) = self
            .github
            .repository
            .split_once('/')
            .context("execution repository must be owner/name")?;
        Ok(RepoConfig {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
}

impl HostedExecutionPolicy {
    fn validate(&self) -> Result<()> {
        let mut quality_gate_ids = BTreeSet::new();
        if self
            .quality_gates
            .iter()
            .filter(|gate| gate.required)
            .count()
            > 6
        {
            bail!("hosted execution policy may contain at most 6 required validation gates");
        }
        if self.policy_version != 1
            || !(30..=86_400).contains(&self.timeout_seconds)
            || self.codex.command.is_empty()
            || self.codex.command.len() > 256
            || self.quality_gates.len() > 100
            || !self.quality_gates.iter().any(|gate| gate.required)
            || self.codex.environment_allowlist.len() > 128
            || self.sandbox.mode != "workspace_write"
            || !self.sandbox.network_access
            || self.sandbox.writable_roots != ["."]
            || self.sandbox.approval_policy != "never"
        {
            bail!("unsupported or incomplete hosted execution policy");
        }
        if self
            .codex
            .command
            .iter()
            .any(|value| value.trim().is_empty())
            || self
                .codex
                .environment_allowlist
                .iter()
                .any(|name| !safe_child_environment_name(name))
            || self.quality_gates.iter().any(|gate| {
                gate.id.trim().is_empty()
                    || gate.id.len() > 200
                    || !quality_gate_ids.insert(gate.id.as_str())
                    || gate.command.trim().is_empty()
                    || gate.command.len() > 8 * 1024
                    || !(1..=86_400).contains(&gate.timeout_seconds)
            })
        {
            bail!("hosted execution policy contains an unsafe command, environment, or timeout");
        }
        Ok(())
    }

    fn child_environment_allowlist(&self) -> Vec<String> {
        self.codex
            .environment_allowlist
            .iter()
            .filter(|name| safe_child_environment_name(name) && name.as_str() != "HOME")
            .cloned()
            .collect()
    }
}

pub fn execute_github_actions(execution_id: Uuid) -> Result<()> {
    let environment = GithubActionsEnvironment::load(execution_id)?;
    environment.require_execute_context()?;
    let git_author = environment.git_author()?;
    harden_hosted_process()?;
    let http = hosted_http_client()?;
    let oidc_token = request_github_oidc(&http, &environment)?;
    let exchange = exchange_github_oidc(&http, &environment, execution_id, &oidc_token)?;
    let api =
        HostedApiClient::from_exchange(http, environment.api_root.clone(), execution_id, exchange)?;
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
            });
            return Err(error);
        }
    };

    let running = Arc::new(AtomicBool::new(true));
    let supervisor = HostedSupervisor::start(api.clone(), Arc::clone(&running));
    let started_at = now_rfc3339();
    send_execution_telemetry(
        &api,
        execution_id,
        &started_at,
        None,
        ExecutionStatus::Running,
        1,
    );

    let result = run_hosted_execution(&api, &manifest, &git_author, &running);
    supervisor.stop();
    let terminal_at = now_rfc3339();
    match result {
        Ok(result) if hosted_result_can_succeed(&result) => {
            report_successful_hosted_result(&api, execution_id, &started_at, &terminal_at, &result)
        }
        Ok(result) => {
            send_execution_telemetry(
                &api,
                execution_id,
                &started_at,
                Some(&terminal_at),
                ExecutionStatus::NeedsContinuation,
                2,
            );
            api.append_event(
                "result",
                json!({
                    "status": completion_request_status(result.completeness.status),
                    "mission_outcome": result.completeness.status,
                    "process_health": "healthy",
                    "branch": result.branch,
                    "head_sha": result.commit,
                    "pull_request_number": result.pull_request.number,
                    "pull_request_url": result.pull_request.url,
                    "implementation_completeness": result.completeness,
                    "technical_validation": result.validation,
                    "terminal_telemetry": result.terminal_telemetry,
                    "resumable": requires_implementation_continuation(
                        result.completeness.status
                    )
                }),
            )?;
            api.complete(&CompletionRequest {
                status: completion_request_status(result.completeness.status).into(),
                mission_outcome: Some(result.completeness.status),
                process_health: Some("healthy".into()),
                completion_evaluation: Some(result.completeness.clone()),
                output_summary: Some(truncate_text(
                    &format!("{}\n\n{}", result.summary, result.completeness.summary),
                    16_000,
                )),
                failure_code: None,
                failure_message: None,
                head_branch: Some(result.branch.clone()),
                head_sha: Some(result.commit.clone()),
                pull_request_number: Some(
                    i64::try_from(result.pull_request.number)
                        .context("pull request number is too large")?,
                ),
                pull_request_url: Some(result.pull_request.url.clone()),
            })
            .context("could not report resumable partial hosted execution")?;
            println!(
                "[invalid] Execution {execution_id} published work but produced an invalid terminal result in pull request #{} at {}",
                result.pull_request.number, result.pull_request.url
            );
            Err(anyhow!(
                "hosted execution produced invalid terminal mission outcome `{}`",
                result.completeness.status.as_str()
            ))
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
            api.append_event("result", blocked_result_event_payload(failure, diagnostics))?;
            api.complete(&CompletionRequest {
                status: "blocked".into(),
                mission_outcome: Some(CompletionStatus::Blocked),
                process_health: Some("healthy".into()),
                completion_evaluation: None,
                output_summary: Some(failure.message.clone()),
                failure_code: Some(failure.code.clone()),
                failure_message: Some(failure.message.clone()),
                head_branch: Some(manifest.github.branch.clone()),
                head_sha: None,
                pull_request_number: None,
                pull_request_url: None,
            })
            .context("could not report structured blocked hosted execution")?;
            println!(
                "[blocked] Execution {execution_id} preserved implementation state without running validation or creating a pull request"
            );
            Ok(())
        }
        Err(error) => {
            let cancelled = !running.load(Ordering::SeqCst) || shutdown::requested();
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
            let _ = api.append_event(
                "result",
                json!({
                    "status": if cancelled { "cancelled" } else { "failed" },
                    "code": code,
                    "failure": diagnostics,
                }),
            );
            let completion = unsuccessful_completion(cancelled, code, message);
            if let Err(completion_error) = api.complete(&completion)
                && completion_error
                    .downcast_ref::<HostedHttpError>()
                    .is_none_or(|failure| !failure.invalidates_execution())
            {
                eprintln!(
                    "[warning] could not report hosted execution failure: {completion_error:#}"
                );
            }
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

fn report_successful_hosted_result(
    api: &HostedApiClient,
    execution_id: Uuid,
    started_at: &str,
    terminal_at: &str,
    result: &HostedResult,
) -> Result<()> {
    send_execution_telemetry(
        api,
        execution_id,
        started_at,
        Some(terminal_at),
        ExecutionStatus::Succeeded,
        2,
    );
    if let Err(error) = api.append_event(
        "result",
        json!({
            "status": completion_request_status(result.completeness.status),
            "mission_outcome": result.completeness.status,
            "process_health": "healthy",
            "branch": result.branch,
            "head_sha": result.commit,
            "pull_request_number": result.pull_request.number,
            "pull_request_url": result.pull_request.url,
            "implementation_completeness": result.completeness,
            "technical_validation": result.validation,
            "terminal_telemetry": result.terminal_telemetry
        }),
    ) {
        eprintln!(
            "[warning] hosted result-event delivery failed before terminal completion: {error:#}"
        );
    }
    api.complete(&CompletionRequest {
        status: completion_request_status(result.completeness.status).into(),
        mission_outcome: Some(result.completeness.status),
        process_health: Some("healthy".into()),
        completion_evaluation: Some(result.completeness.clone()),
        output_summary: Some(truncate_text(&result.summary, 16_000)),
        failure_code: None,
        failure_message: None,
        head_branch: Some(result.branch.clone()),
        head_sha: Some(result.commit.clone()),
        pull_request_number: Some(
            i64::try_from(result.pull_request.number)
                .context("pull request number is too large")?,
        ),
        pull_request_url: Some(result.pull_request.url.clone()),
    })
    .context("could not complete the hosted execution")?;
    println!(
        "[complete] Execution {execution_id} opened or reused pull request #{} at {}",
        result.pull_request.number, result.pull_request.url
    );
    Ok(())
}

fn hosted_result_can_succeed(result: &HostedResult) -> bool {
    match result.completeness.status {
        CompletionStatus::Complete | CompletionStatus::CompletePendingExternalReview => result
            .validation
            .iter()
            .all(|validation| validation.status == "passed"),
        CompletionStatus::Partial | CompletionStatus::Blocked => true,
        CompletionStatus::Incomplete | CompletionStatus::Uncertain => false,
    }
}

const fn completion_request_status(status: CompletionStatus) -> &'static str {
    match status {
        CompletionStatus::Complete => "completed",
        CompletionStatus::CompletePendingExternalReview => "awaiting_external_review",
        CompletionStatus::Partial | CompletionStatus::Incomplete | CompletionStatus::Uncertain => {
            "partial_result"
        }
        CompletionStatus::Blocked => "blocked",
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
    let api = HostedApiClient::from_exchange(http, environment.api_root, execution_id, exchange)?;
    let _ = api.claim();
    let _ = api.append_event(
        "result",
        json!({
            "status": "failed",
            "code": "github_actions_step_failed",
            "emergency_callback": true
        }),
    );
    api.complete(&CompletionRequest {
        status: "failed".into(),
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
    })?;
    println!("[complete] Reported emergency failure for execution {execution_id}");
    Ok(())
}

struct HostedSupervisor {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl HostedSupervisor {
    fn start(api: HostedApiClient, running: Arc<AtomicBool>) -> Self {
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
                match api.heartbeat() {
                    Ok(()) => failures = 0,
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        if error
                            .downcast_ref::<HostedHttpError>()
                            .is_some_and(HostedHttpError::invalidates_execution)
                            || failures >= 3
                        {
                            running.store(false, Ordering::SeqCst);
                            break;
                        }
                    }
                }
                next = Instant::now() + HEARTBEAT_INTERVAL;
            }
            if shutdown::requested() {
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

    let partial_run = detect_partial_run(
        existing_pr.as_ref(),
        resumed,
        manifest.execution.attempt_number,
        completion_changed_paths(&repo, &manifest.github.base_sha)?,
    );
    let mut agent = GatewayAgent::new(
        api.clone(),
        manifest,
        &repo,
        running,
        &containment,
        partial_run,
    )?;
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
        bootstrap_hosted_dependencies(api, manifest, &repo, running, &containment)?;
        let mut implementation = if !should_continue_implementation(
            existing_pr.is_some(),
            resumed,
            manifest.execution.attempt_number,
        ) {
            ImplementationOutcome {
                summary: "Recovered an existing hosted execution branch and pull request."
                    .to_owned(),
                budget_exhausted: false,
                explicit_declaration: None,
            }
        } else {
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
            agent
                .implement()
                .context("the RustGrid AI gateway implementation session failed")?
        };
        ensure_running(running)?;
        ensure_hosted_execution_deadline(agent.execution_started_at)?;

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
            agent.write_blocker.get_or_insert_with(|| {
                "repository validation is forbidden until implementation produces relevant changes"
                    .into()
            });
            agent.checkpoint_notebook(false)?;
            return Err(agent.implementation_preparation_failure());
        }

        agent.transition_phase(
            ExecutionPhase::Validation,
            "implementation session ended; worker-owned validation started",
        )?;
        api.update_state("validating")?;
        let mut validation_round = 1_u32;
        let validation_started = Instant::now();
        let mut validation = dispatch_validation_gates(validation_entry, || {
            run_quality_gates(
                api,
                manifest,
                &repo,
                running,
                &manifest.execution_policy,
                &containment,
                validation_round,
                &mut agent.notebook.validation_evidence,
                &mut agent.notebook.required_gates,
                &mut agent.tool_usage,
                validation_started,
                agent.execution_started_at,
            )
        })?
        .context("validation entry policy rejected worker-owned quality gates")?;
        agent.checkpoint_validation_ledger()?;
        agent.ensure_active_or_checkpoint_cancellation()?;
        for repair_attempt in 0..MAX_REPAIR_ATTEMPTS {
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
            let repair_tree_after = repository_state_fingerprint(&repo, &manifest.github.base_sha)?;
            if repair_tree_after == repair_tree_before {
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
            agent.transition_phase(
                ExecutionPhase::Validation,
                "validation repair ended; rerunning required quality gates",
            )?;
            validation_round = validation_round.saturating_add(1);
            validation = run_quality_gates(
                api,
                manifest,
                &repo,
                running,
                &manifest.execution_policy,
                &containment,
                validation_round,
                &mut agent.notebook.validation_evidence,
                &mut agent.notebook.required_gates,
                &mut agent.tool_usage,
                validation_started,
                agent.execution_started_at,
            )?;
            agent.checkpoint_validation_ledger()?;
            agent.ensure_active_or_checkpoint_cancellation()?;
        }
        let validation_passed = validation.iter().all(|result| result.status == "passed");
        let review_paths = if validation_passed {
            agent.deterministic_diff_review()?
        } else {
            completion_changed_paths(&repo, &manifest.github.base_sha)?
        };
        if validation_passed
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
        let completeness =
            agent.evaluate_completion(&implementation, &validation, &review_paths)?;
        api.append_event(
            "result",
            json!({
                "status": "implementation_evaluated",
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

        agent.transition_phase(
            ExecutionPhase::Publication,
            "completion evaluation finished; publishing preserved work",
        )?;
        if repo.hosted_local_config()? != trusted_git_config {
            bail!("repository-controlled execution modified the protected local Git configuration");
        }
        repo.verify_hosted_origin(
            &repo_config.owner,
            &repo_config.name,
            &manifest.github.web_base_url,
        )?;
        if command::checked("git", ["rev-parse", "HEAD"], &repo.root)? != trusted_head {
            bail!("repository-controlled execution modified Git history before publication");
        }
        let dirty = repo.new_agent_paths(&baseline)?;
        let mut commit = if dirty.is_empty() {
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

        ensure_running(running)?;
        publish_hosted_branch(
            HostedPublicationContext {
                api,
                manifest,
                repo: &repo,
                repo_config: &repo_config,
                running,
                trusted_git_config: &trusted_git_config,
                containment: &containment,
                validation_round: &mut validation_round,
                validation_evidence: &mut agent.notebook.validation_evidence,
                required_gates: &mut agent.notebook.required_gates,
                tool_usage: &mut agent.tool_usage,
                validation_started_at: Instant::now(),
                execution_started_at: agent.execution_started_at,
            },
            &mut commit,
            &mut validation,
        )?;
        ensure_hosted_execution_deadline(agent.execution_started_at)?;
        api.update_state("creating_pull_request")?;
        containment.drain()?;
        let publication_token = api.github_token(&manifest.github.repository)?;
        let github = GitHubClient::new(publication_token.expose(), &manifest.github.web_base_url)?;
        let partial = requires_implementation_continuation(completeness.status);
        let pull = find_or_create_hosted_pull_request(
            &github,
            &repo_config,
            manifest,
            &validation,
            &completeness,
            partial,
        )?;
        drop(github);
        drop(publication_token);
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
        };
        Ok(HostedResult {
            summary: implementation.summary,
            branch: manifest.github.branch.clone(),
            commit,
            pull_request: PullRequestResult {
                number: pull.number,
                url: pull.html_url,
            },
            validation,
            completeness,
            terminal_telemetry,
        })
    })();
    match execution_result {
        Ok(result) => Ok(result),
        Err(error)
            if error
                .downcast_ref::<HostedAgentExecutionFailure>()
                .is_some() =>
        {
            Err(error)
        }
        Err(error) => {
            let (code, message) = safe_failure(&error, false);
            Err(agent.execution_failure(
                &code,
                message,
                Some(&error),
                true,
                "Resume from the persisted notebook after resolving the exact validation or publication failure.",
            ))
        }
    }
}

const fn validation_entry_allows_gates(decision: ValidationEntryDecision) -> bool {
    matches!(
        decision,
        ValidationEntryDecision::CompleteImplementation
            | ValidationEntryDecision::UsefulPartialImplementation
            | ValidationEntryDecision::ResumedImplementation
    )
}

fn committed_head_for_publication(
    repo: &Repo,
    base_sha: &str,
) -> Result<Option<(String, Vec<String>)>> {
    let changed_paths = completion_changed_paths(repo, base_sha)?;
    if changed_paths.is_empty() {
        return Ok(None);
    }
    let commit = command::checked("git", ["rev-parse", "HEAD"], &repo.root)?;
    Ok(Some((commit, changed_paths)))
}

fn dispatch_validation_gates<T>(
    decision: ValidationEntryDecision,
    run: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    if validation_entry_allows_gates(decision) {
        run().map(Some)
    } else {
        Ok(None)
    }
}

fn find_or_create_hosted_pull_request(
    github: &GitHubClient,
    repo_config: &RepoConfig,
    manifest: &HostedManifest,
    validation: &[ValidationResult],
    completeness: &CompletionEvaluation,
    draft: bool,
) -> Result<crate::github::PullRequest> {
    let title = hosted_pull_request_title(manifest, draft);
    let body = hosted_pull_request_body(manifest, validation, completeness);
    if let Some(pull) = github.find_open_pull_request(repo_config, &manifest.github.branch)? {
        let pull = github.update_pull_request(repo_config, pull.number, &title, &body)?;
        if pull.draft != draft {
            let node_id = pull
                .node_id
                .as_deref()
                .context("GitHub pull request response has no node identity")?;
            github.set_pull_request_draft(node_id, draft)?;
        }
        return Ok(pull);
    }
    match github.create_pull_request_with_draft(
        repo_config,
        &title,
        &body,
        &manifest.github.branch,
        normalized_base_ref(&manifest.github.base_ref)?,
        draft,
    ) {
        Ok(pull) => Ok(pull),
        Err(create_error) => {
            // POST retries are inherently ambiguous: GitHub may have created
            // the pull request before a response was lost. Resolve by the
            // deterministic head branch before surfacing the original error.
            match github.find_open_pull_request(repo_config, &manifest.github.branch) {
                Ok(Some(pull)) => Ok(pull),
                _ => Err(create_error),
            }
        }
    }
}

fn hosted_pull_request_title(manifest: &HostedManifest, draft: bool) -> String {
    format!(
        "{}{}: {}",
        if draft { "[INCOMPLETE] " } else { "" },
        manifest.ticket_key,
        manifest.ticket_title
    )
}

struct HostedPublicationContext<'a> {
    api: &'a HostedApiClient,
    manifest: &'a HostedManifest,
    repo: &'a Repo,
    repo_config: &'a RepoConfig,
    running: &'a Arc<AtomicBool>,
    trusted_git_config: &'a [u8],
    containment: &'a command::HostedProcessContainment,
    validation_round: &'a mut u32,
    validation_evidence: &'a mut Vec<ValidationEvidence>,
    required_gates: &'a mut Vec<RequiredGate>,
    tool_usage: &'a mut ToolUsage,
    validation_started_at: Instant,
    execution_started_at: Instant,
}

fn publish_hosted_branch(
    context: HostedPublicationContext<'_>,
    commit: &mut String,
    validation: &mut Vec<ValidationResult>,
) -> Result<()> {
    let HostedPublicationContext {
        api,
        manifest,
        repo,
        repo_config,
        running,
        trusted_git_config,
        containment,
        validation_round,
        validation_evidence,
        required_gates,
        tool_usage,
        validation_started_at,
        execution_started_at,
    } = context;
    for attempt in 1..=3 {
        ensure_running(running)?;
        ensure_hosted_execution_deadline(execution_started_at)?;
        ensure_hosted_repository_integrity(
            repo,
            repo_config,
            manifest,
            trusted_git_config,
            commit,
        )?;
        containment.drain()?;
        let reconciled = {
            let token = api.github_token(&manifest.github.repository)?;
            repo.reconcile_remote_branch(
                &manifest.github.branch,
                commit,
                token.expose(),
                &manifest.github.web_base_url,
            )?
        };
        let requires_validation = reconciled.requires_validation();
        *commit = reconciled.commit;
        if requires_validation {
            *validation_round = validation_round.saturating_add(1);
            api.append_event(
                "progress",
                json!({
                    "step": "branch_reconciliation",
                    "status": "completed",
                    "head_sha": commit,
                    "publication_attempt": attempt
                }),
            )?;
            *validation = run_quality_gates(
                api,
                manifest,
                repo,
                running,
                &manifest.execution_policy,
                containment,
                *validation_round,
                validation_evidence,
                required_gates,
                tool_usage,
                validation_started_at,
                execution_started_at,
            )?;
            if validation.iter().any(|result| result.status != "passed") {
                bail!("required hosted execution validation failed after branch reconciliation");
            }
            repo.ensure_safe(false)?;
        }
        ensure_hosted_repository_integrity(
            repo,
            repo_config,
            manifest,
            trusted_git_config,
            commit,
        )?;
        containment.drain()?;
        let token = api.github_token(&manifest.github.repository)?;
        match repo.push(
            &manifest.github.branch,
            commit,
            token.expose(),
            &manifest.github.web_base_url,
        ) {
            Ok(_) => return Ok(()),
            Err(error) if attempt < 3 && error.downcast_ref::<RemoteBranchMoved>().is_some() => {
                api.append_event(
                    "progress",
                    json!({
                        "step": "branch_reconciliation",
                        "status": "retrying",
                        "publication_attempt": attempt + 1
                    }),
                )?;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("bounded hosted publication loop always returns")
}

fn ensure_hosted_repository_integrity(
    repo: &Repo,
    repo_config: &RepoConfig,
    manifest: &HostedManifest,
    trusted_git_config: &[u8],
    expected_head: &str,
) -> Result<()> {
    if repo.hosted_local_config()? != trusted_git_config {
        bail!("repository-controlled execution modified the protected local Git configuration");
    }
    repo.verify_hosted_origin(
        &repo_config.owner,
        &repo_config.name,
        &manifest.github.web_base_url,
    )?;
    if command::checked("git", ["rev-parse", "HEAD"], &repo.root)? != expected_head {
        bail!("repository-controlled execution modified Git history before publication");
    }
    Ok(())
}

struct GatewayAgent<'a> {
    api: HostedApiClient,
    manifest: &'a HostedManifest,
    repo: &'a Repo,
    running: &'a Arc<AtomicBool>,
    containment: &'a command::HostedProcessContainment,
    budget: BudgetAudit,
    phases: PhaseLedger,
    impact_map: Option<ImpactMap>,
    implementation_plan: Option<ImplementationPlan>,
    declaration: Option<ImplementationDeclaration>,
    tool_failures: Vec<ToolFailureRecord>,
    tool_usage: ToolUsage,
    notebook: WorkerNotebook,
    search_guard: SearchGuard,
    repair_read_targets: BTreeSet<String>,
    diff_reviewed: bool,
    diff_review_cursor: usize,
    diff_review_digest: Option<String>,
    write_blocker: Option<String>,
    blocked_plan_recorded_at: Option<usize>,
    impact_map_failure: Option<ImpactMapFailure>,
    last_successful_action: Value,
    partial_run: Option<PartialRunContext>,
    budget_advisory_percent: u8,
    last_cache_prefix_sha256: Option<String>,
    last_tool_order_sha256: Option<String>,
    guided_first_write_recovery_issued: bool,
    last_repository_progress_call: usize,
    cost_guard: CostGuard,
    execution_started_at: Instant,
    phase_started_at: Instant,
    last_source_progress_call: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
struct CostGuard {
    estimated_cost_micros: u64,
    call_count: u32,
    input_tokens: u64,
    output_tokens: u64,
    usage_estimate_fallbacks: u32,
    repository_progress_score: f32,
    hard_limit_micros: u64,
}

const fn model_cost_limit_for_target_count(target_count: usize) -> u64 {
    match target_count {
        0 | 1 => 1_000_000,
        2..=8 => 2_000_000,
        9..=12 => 5_000_000,
        _ => 10_000_000,
    }
}

fn constrain_request_to_cost_limit(request: &mut Value, guard: &CostGuard) -> Result<bool> {
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

fn model_usage_for_accounting(request: &Value, response: &Value) -> Result<(u64, u64, bool)> {
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

#[derive(Clone, Debug, Serialize)]
struct CancellationResult {
    requested_by: &'static str,
    requested_at: String,
    phase: ExecutionPhase,
    changed_paths: Vec<String>,
    completed_changes: Vec<String>,
    remaining_work: Vec<RemainingWorkItem>,
    source_tree_hash: String,
    resumable: bool,
    resume_phase: ExecutionPhase,
}

fn ordered_implementation_targets_from_notebook(
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

fn implementation_start_context_from_notebook(
    notebook: &WorkerNotebook,
    source_tree_hash: String,
    remaining_call_budget: usize,
    guided_recovery: bool,
    implementation_calls: usize,
    successful_writes: u32,
) -> ImplementationStartContext {
    let validation_repair =
        notebook.phase == ExecutionPhase::Repair && !notebook.validation_failures.is_empty();
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
        exact_files_already_read,
        missing_file_contents,
        source_tree_hash,
        remaining_call_budget,
        current_target,
        guided_recovery,
        unresolved_preparation_blockers,
        instruction: if guided_recovery {
            "FIRST-WRITE RECOVERY: work only on current_target. Use its existing read evidence or perform one bounded read of that exact path, then attempt its authorized mutation. Do not inspect another target, re-run discovery, or perform validation.".into()
        } else {
            "Read only the exact files still needed. Begin source mutations as soon as sufficient context exists. Do not re-run discovery. Do not perform final repository validation; the worker runs authoritative validation after all targets are applied.".into()
        },
    }
}

fn validate_current_target_scope(
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

fn mark_post_write_stagnation(write_blocker: &mut Option<String>, successful_writes: u32) -> bool {
    if successful_writes == 0 {
        return false;
    }
    write_blocker.get_or_insert_with(|| {
        "post_write_stagnation: useful repository changes are preserved, but explicit planned work remains unresolved"
            .into()
    });
    true
}

fn mark_mutation_preflight_blocker(write_blocker: &mut Option<String>, target: &str) -> bool {
    write_blocker.get_or_insert_with(|| {
        format!(
            "mutation_preflight_rejected: target `{target}` requires persisted plan repair before implementation can continue"
        )
    });
    true
}

fn record_validation_repair_start(notebook: &mut WorkerNotebook, failures: &[ValidationResult]) {
    notebook.validation_failures.extend(
        failures
            .iter()
            .map(|failure| format!("{}: {}", failure.id, failure.status)),
    );
    notebook.implementation_substate = ImplementationSubstate::Repairing;
}

impl<'a> GatewayAgent<'a> {
    fn new(
        api: HostedApiClient,
        manifest: &'a HostedManifest,
        repo: &'a Repo,
        running: &'a Arc<AtomicBool>,
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
        let restored = manifest
            .run
            .metadata
            .get("worker_notebook")
            .cloned()
            .and_then(|value| serde_json::from_value::<WorkerNotebook>(value).ok())
            .filter(|notebook| {
                notebook.schema_version == 1
                    && notebook.repository_base_sha == manifest.github.base_sha
                    && notebook.branch == manifest.github.branch
                    && (notebook.repository_fingerprint.is_empty()
                        || notebook.repository_fingerprint == repository_fingerprint)
            });
        let mut notebook = restored.unwrap_or_else(|| {
            new_worker_notebook(manifest, repository_fingerprint, partial_run.as_ref())
        });
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
        normalize_notebook_intended_changes(&mut notebook, &repo.root)?;
        if let Some(partial_run) = &partial_run {
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
                artifact_source: Some(ArtifactSource::NormalizedModel),
                confidence: Some(1.0),
                failure_layer: None,
                validation_errors: Vec::new(),
                invalid_payload_shape: None,
            };
        }
        let (impact_map, implementation_plan, initial_phase) =
            notebook_orchestration_state(&notebook);
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
        Ok(Self {
            api,
            manifest,
            repo,
            running,
            containment,
            budget,
            phases,
            impact_map,
            implementation_plan,
            declaration: None,
            tool_failures: notebook.failed_changes.clone(),
            tool_usage: ToolUsage::default(),
            notebook: WorkerNotebook {
                phase: initial_phase,
                execution_attempt: manifest.execution.attempt_number,
                ..notebook
            },
            search_guard: SearchGuard::default(),
            repair_read_targets: BTreeSet::new(),
            diff_reviewed: false,
            diff_review_cursor: 0,
            diff_review_digest: None,
            write_blocker: None,
            blocked_plan_recorded_at: None,
            impact_map_failure,
            last_successful_action: json!({}),
            partial_run,
            budget_advisory_percent: 0,
            last_cache_prefix_sha256: None,
            last_tool_order_sha256: None,
            guided_first_write_recovery_issued: false,
            last_repository_progress_call: 0,
            cost_guard: CostGuard {
                // Localized work is provisionally capped at EUR 2 before planning reveals the
                // authoritative target count. Larger plans may raise this bound explicitly.
                hard_limit_micros: 2_000_000,
                ..CostGuard::default()
            },
            execution_started_at: Instant::now(),
            phase_started_at: Instant::now(),
            last_source_progress_call: 0,
        })
    }

    fn implement(&mut self) -> Result<ImplementationOutcome> {
        let prompt = build_hosted_prompt(self.manifest, self.repo, self.partial_run.as_ref())?;
        if self.reconcile_authoritative_target_state()?
            == ImplementationCompletionStatus::ReadyForValidation
        {
            self.transition_phase(
                ExecutionPhase::Validation,
                "restored target state is complete; worker-owned validation starts without another model call",
            )?;
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

    fn budget_telemetry(&self) -> Value {
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

    fn append_event_recoverable(&self, event_type: &str, data: Value, operation: &str) -> bool {
        match self.api.append_event(event_type, data) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("[warning] {operation} could not be persisted: {error:#}");
                false
            }
        }
    }

    fn ensure_active_or_checkpoint_cancellation(&mut self) -> Result<()> {
        if self.running.load(Ordering::SeqCst) && !shutdown::requested() {
            return Ok(());
        }
        self.reconcile_authoritative_target_state()?;
        self.checkpoint_notebook(true)?;
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
                "cancellation": result,
                "notebook": self.notebook,
            }),
            "cancellation checkpoint",
        );
        bail!("hosted execution was cancelled after preserving a resumable checkpoint")
    }

    fn record_tool_progress(
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

    fn observe_implementation_progress(&mut self) -> Result<bool> {
        if !matches!(
            self.phases.active(),
            ExecutionPhase::Implementation | ExecutionPhase::Repair
        ) {
            return Ok(false);
        }
        let calls = self.phases.implementation_repair_calls();
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let current_tree_hash =
            repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        supersede_stale_validation(&mut self.notebook.validation_evidence, &current_tree_hash);
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
        let halt = matches!(
            action,
            ImplementationProgressAction::BlockedBeforeFirstWrite
                | ImplementationProgressAction::BlockedAfterWrite
        );
        if action == ImplementationProgressAction::FirstWriteDelayed {
            self.guided_first_write_recovery_issued = true;
        }
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": match action {
                    ImplementationProgressAction::FirstWriteDelayed => "worker.first_write_delayed",
                    ImplementationProgressAction::BlockedBeforeFirstWrite
                    | ImplementationProgressAction::BlockedAfterWrite => "worker.no_progress_detected",
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
                "orchestration_action": if halt {
                    "return_resumable_partial_result"
                } else if action == ImplementationProgressAction::FirstWriteDelayed {
                    "guided_single_target_recovery"
                } else {
                    "continue"
                },
            }),
            "implementation progress window",
        );
        Ok(halt)
    }

    fn implementation_progress_halt_summary(&mut self) -> Result<Option<String>> {
        if !self.observe_implementation_progress()? {
            return Ok(None);
        }
        if self.tool_usage.successful_writes == 0 {
            self.write_blocker.get_or_insert_with(|| {
                "implementation preparation remained blocked after the bounded guided first-write recovery"
                    .into()
            });
            Ok(Some(
                "Implementation preparation stopped only after the eight-call allowance and guided single-target recovery were exhausted. No validation should run until a relevant repository change exists."
                    .into(),
            ))
        } else {
            mark_post_write_stagnation(&mut self.write_blocker, self.tool_usage.successful_writes);
            Ok(Some(
                "Implementation paused after four consecutive post-write calls without a new write, changed path, resolved mutation failure, applied target, or phase transition. Preserved changes remain resumable."
                    .into(),
            ))
        }
    }

    fn record_cache_observability(&mut self, request: &Value, response: &Value) {
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

    fn observe_model_cost(&mut self, request: &Value, response: &Value) -> Result<()> {
        let (input_tokens, output_tokens, estimated) =
            model_usage_for_accounting(request, response)?;
        self.cost_guard.call_count = self.cost_guard.call_count.saturating_add(1);
        self.cost_guard.input_tokens = self.cost_guard.input_tokens.saturating_add(input_tokens);
        self.cost_guard.output_tokens = self.cost_guard.output_tokens.saturating_add(output_tokens);
        if estimated {
            self.cost_guard.usage_estimate_fallbacks =
                self.cost_guard.usage_estimate_fallbacks.saturating_add(1);
        }
        // Conservative provider-independent estimate: EUR 5/M input and EUR 15/M output.
        self.cost_guard.estimated_cost_micros = self
            .cost_guard
            .estimated_cost_micros
            .saturating_add(input_tokens.saturating_mul(5))
            .saturating_add(output_tokens.saturating_mul(15));
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

    fn enforce_cost_and_time_guardrails(
        &mut self,
        allow_budget_handoff: bool,
    ) -> Result<Option<ImplementationOutcome>> {
        let calls_without_progress = self
            .phases
            .total_calls()
            .saturating_sub(self.last_source_progress_call);
        let cost_triggered =
            self.cost_guard.estimated_cost_micros >= self.cost_guard.hard_limit_micros;
        let phase_limit = match self.phases.active() {
            ExecutionPhase::Discovery => Duration::from_secs(3 * 60),
            ExecutionPhase::ArtifactRepair | ExecutionPhase::Planning => {
                Duration::from_secs(2 * 60)
            }
            ExecutionPhase::Implementation | ExecutionPhase::Repair => Duration::from_secs(10 * 60),
            ExecutionPhase::Validation => Duration::from_secs(8 * 60),
            ExecutionPhase::DiffReview | ExecutionPhase::CompletionEvaluation => {
                Duration::from_secs(4 * 60)
            }
            ExecutionPhase::Publication => Duration::from_secs(3 * 60),
        };
        let phase_wall_clock_triggered = self.phase_started_at.elapsed() >= phase_limit;
        let execution_wall_clock_triggered =
            self.execution_started_at.elapsed() >= MAX_HOSTED_EXECUTION_DURATION;
        let wall_clock_triggered = phase_wall_clock_triggered || execution_wall_clock_triggered;
        if !cost_triggered && !wall_clock_triggered {
            return Ok(None);
        }
        self.checkpoint_notebook(false)?;
        let event_type = if cost_triggered {
            "worker.cost_guard_triggered"
        } else {
            "worker.wall_clock_guard_triggered"
        };
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": event_type,
                "phase": self.phases.active(),
                "cost_guard": self.cost_guard,
                "phase_elapsed_ms": self.phase_started_at.elapsed().as_millis(),
                "execution_elapsed_ms": self.execution_started_at.elapsed().as_millis(),
                "phase_wall_clock_triggered": phase_wall_clock_triggered,
                "execution_wall_clock_triggered": execution_wall_clock_triggered,
                "execution_limit_seconds": MAX_HOSTED_EXECUTION_DURATION.as_secs(),
                "calls_without_source_progress": calls_without_progress,
                "resumable": true,
            }),
            "execution guardrail",
        );
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
        if allow_budget_handoff && !changed_paths.is_empty() {
            return Ok(Some(ImplementationOutcome {
                summary: format!(
                    "{} preserved {} changed path(s) as a resumable partial result.",
                    event_type,
                    changed_paths.len()
                ),
                budget_exhausted: true,
                explicit_declaration: self.declaration.clone(),
            }));
        }
        if matches!(
            self.phases.active(),
            ExecutionPhase::Implementation | ExecutionPhase::Repair
        ) && self.tool_usage.successful_writes == 0
            && changed_paths.is_empty()
        {
            self.write_blocker.get_or_insert_with(|| {
                "implementation preparation reached its cost or wall-clock guardrail before a verified repository mutation".into()
            });
            return Err(self.implementation_preparation_failure());
        }
        Err(self.execution_failure(
            event_type,
            "The hosted execution reached a cost or wall-clock guardrail without publishable repository progress.",
            None,
            true,
            "Resume from the persisted notebook after narrowing the remaining work.",
        ))
    }

    fn notebook_checkpoint_metadata(&self, artifact_sha256: Option<&str>) -> Value {
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

    fn transition_phase(&mut self, phase: ExecutionPhase, reason: &str) -> Result<Option<String>> {
        let previous = self.phases.active();
        if previous == phase {
            return Ok(None);
        }
        if !legal_phase_transition(previous, phase) {
            bail!(
                "illegal hosted lifecycle transition from `{}` to `{}`",
                previous.as_str(),
                phase.as_str()
            );
        }
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
        Ok(persistence_error)
    }

    fn checkpoint_notebook(&mut self, repository_changed: bool) -> Result<()> {
        self.notebook.revision = self.notebook.revision.saturating_add(1);
        self.notebook.phase = self.phases.active();
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
        Ok(())
    }

    fn ordered_implementation_targets(&self) -> Vec<ImplementationTarget> {
        ordered_implementation_targets_from_notebook(&self.notebook)
    }

    fn current_implementation_target(&self) -> Option<ImplementationTarget> {
        if self.phases.active() == ExecutionPhase::Repair
            && !self.notebook.validation_failures.is_empty()
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

    fn implementation_start_context(&self) -> Result<ImplementationStartContext> {
        Ok(implementation_start_context_from_notebook(
            &self.notebook,
            repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?,
            self.phases
                .phase_limit(self.phases.active())
                .saturating_sub(self.phases.phase_calls(self.phases.active())),
            self.guided_first_write_recovery_issued,
            self.phases.implementation_repair_calls(),
            self.tool_usage.successful_writes,
        ))
    }

    fn reconcile_authoritative_target_state(&mut self) -> Result<ImplementationCompletionStatus> {
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let current_tree_hash =
            repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        supersede_stale_validation(&mut self.notebook.validation_evidence, &current_tree_hash);
        reconcile_changed_target_statuses(&mut self.notebook.intended_changes, &changed_paths);
        if let Some(plan) = self.implementation_plan.as_mut() {
            for planned in &mut plan.planned_changes {
                if let Some(intended) = self
                    .notebook
                    .intended_changes
                    .iter()
                    .find(|change| change.change_id == planned.change_id)
                {
                    planned.status = intended.status;
                    for target in &mut planned.targets {
                        if let Some(source) = intended
                            .targets
                            .iter()
                            .find(|candidate| candidate.path == target.path)
                        {
                            target.status = source.status;
                        }
                    }
                }
            }
            self.notebook.planned_changes = plan.planned_changes.clone();
        }
        let derived = derive_remaining_work(&self.notebook.intended_changes);
        self.notebook.remaining_work = legacy_remaining_work(&derived);
        self.notebook.remaining_work_v2 = derived;
        validate_lifecycle_invariants(
            &self.notebook.intended_changes,
            &self.notebook.remaining_work_v2,
            &self.notebook.validation_evidence,
            &current_tree_hash,
        )
        .map_err(|error| anyhow!("lifecycle invariant violated: {error}"))?;
        let unresolved = self.tool_failures.iter().any(|failure| {
            !failure.recovered && failure.reconciliation == FailureReconciliation::StillUnresolved
        });
        let status = implementation_completion_status(
            &self.notebook.intended_changes,
            &changed_paths,
            unresolved,
            self.write_blocker.is_some() || !self.notebook.blocking_unknowns.is_empty(),
        );
        self.notebook.implementation_substate = match status {
            ImplementationCompletionStatus::ReadyForValidation => {
                ImplementationSubstate::ReadyForValidation
            }
            ImplementationCompletionStatus::PartialReadyForValidation
            | ImplementationCompletionStatus::InProgress
            | ImplementationCompletionStatus::Blocked
                if unresolved =>
            {
                ImplementationSubstate::Repairing
            }
            ImplementationCompletionStatus::InProgress
            | ImplementationCompletionStatus::PartialReadyForValidation => {
                ImplementationSubstate::Mutating
            }
            ImplementationCompletionStatus::Preparing
                if !self.notebook.write_attempts.is_empty() =>
            {
                ImplementationSubstate::Mutating
            }
            ImplementationCompletionStatus::NotStarted
            | ImplementationCompletionStatus::Preparing
            | ImplementationCompletionStatus::Blocked => ImplementationSubstate::Preparing,
        };
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

    fn invalidate_validation_after_mutation(&mut self) -> Result<()> {
        let tree_hash = repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let superseded =
            supersede_stale_validation(&mut self.notebook.validation_evidence, &tree_hash);
        if superseded > 0 {
            self.append_event_recoverable(
                "validation",
                json!({
                    "event_type": "worker.validation_superseded",
                    "source_tree_hash": tree_hash,
                    "superseded_count": superseded,
                }),
                "validation invalidation",
            );
        }
        Ok(())
    }

    fn checkpoint_validation_ledger(&mut self) -> Result<()> {
        self.checkpoint_notebook(false)?;
        self.api.append_event(
            "validation",
            json!({
                "event_type": "worker.validation_ledger_checkpoint",
                "validation_evidence": self.notebook.validation_evidence,
                "required_gates": self.notebook.required_gates,
                "notebook": self.notebook,
                "checkpoint": self.notebook_checkpoint_metadata(None),
            }),
        )
    }

    fn accept_impact_map(
        &mut self,
        map: ImpactMap,
        artifact_source: ArtifactSource,
        confidence: f64,
        triggering_error: Option<&anyhow::Error>,
    ) -> Result<String> {
        validate_impact_map(&map, &self.notebook)?;
        let artifact_sha256 = impact_map_sha256(&map);
        self.notebook.impact_map = map.areas.clone();
        self.notebook.impact_map_v2 = Some(map.clone());
        self.notebook.files_inspected = map.inspected_files.clone();
        self.notebook.searches_completed = map
            .searches
            .iter()
            .map(|search| search.query.clone())
            .collect();
        self.notebook.blocking_unknowns = map.unresolved_questions.clone();
        self.notebook.impact_map_artifact = ArtifactCheckpoint {
            artifact: "impact_map".into(),
            semantic_status: ArtifactSemanticStatus::Sufficient,
            serialization_status: if artifact_source == ArtifactSource::NormalizedModel {
                ArtifactSerializationStatus::Normalizable
            } else {
                ArtifactSerializationStatus::Valid
            },
            persistence_status: ArtifactPersistenceStatus::PendingRetry,
            artifact_sha256: artifact_sha256.clone(),
            model_call_index: Some(self.phases.total_calls()),
            phase: self.phases.active(),
            safe_error: triggering_error.map(|error| truncate_text(&format!("{error:#}"), 2_000)),
            artifact_source: Some(artifact_source),
            confidence: Some(confidence),
            failure_layer: None,
            validation_errors: Vec::new(),
            invalid_payload_shape: None,
        };
        self.notebook.impact_map_invalid_payload = None;
        self.impact_map = Some(map);
        self.impact_map_failure = None;
        let persistence_error = self.transition_phase(
            ExecutionPhase::Planning,
            "valid discovery impact map accepted",
        )?;
        let persisted = persistence_error.is_none();
        self.notebook.impact_map_artifact.persistence_status = if persisted {
            ArtifactPersistenceStatus::Persisted
        } else {
            ArtifactPersistenceStatus::Failed
        };
        self.notebook.impact_map_artifact.phase = ExecutionPhase::Discovery;
        if let Some(error) = persistence_error.as_ref() {
            self.notebook.impact_map_artifact.safe_error = Some(error.clone());
            self.notebook.impact_map_artifact.failure_layer =
                Some(ArtifactFailureLayer::ArtifactPersistence);
        }
        if !persisted {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.artifact_persistence_failed",
                    "artifact": "impact_map",
                    "semantic_status": ArtifactSemanticStatus::Sufficient,
                    "serialization_status": self.notebook.impact_map_artifact.serialization_status,
                    "persistence_status": ArtifactPersistenceStatus::Failed,
                    "recoverable": true,
                    "action": "retry_or_continue",
                    "safe_error": persistence_error,
                    "artifact_source": artifact_source,
                    "confidence": confidence,
                    "tool_schema_version": IMPACT_MAP_SCHEMA_VERSION,
                    "tool_schema_sha256": impact_map::schema_sha256(),
                    "validator_schema_version": IMPACT_MAP_SCHEMA_VERSION,
                    "validator_schema_sha256": impact_map::schema_sha256(),
                    "notebook": self.notebook,
                    "checkpoint": self.notebook_checkpoint_metadata(
                        artifact_sha256.as_deref()
                    ),
                }),
                "impact-map fallback checkpoint",
            );
        }
        Ok(if persisted {
            format!("recorded implementation impact map from {artifact_source:?}")
        } else {
            format!(
                "impact map was semantically accepted from {artifact_source:?}; persistence is degraded and will be retried without another discovery model call"
            )
        })
    }

    fn emit_guardrail(&self, code: &str, action: &str, message: &str) -> Result<()> {
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

    fn emit_phase_budget_warning(&self) -> Result<()> {
        let phase = self.phases.active();
        self.api.append_event(
            "progress",
            json!({
                "event_type": "worker.phase_budget_warning",
                "phase": phase,
                "calls_used": self.phases.phase_calls(phase),
                "calls_limit": self.phases.phase_limit(phase),
                "total_calls_used": self.phases.total_calls(),
                "total_calls_limit": self.phases.total_limit(),
            }),
        )
    }

    fn execution_failure(
        &self,
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
            category: "hosted_agent_execution_failed",
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
            phase_calls_limit: self.phases.phase_limit(phase),
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
                .filter(|record| {
                    matches!(
                        record.class,
                        ToolProgressClass::RecoverableFailure | ToolProgressClass::BlockingFailure
                    )
                })
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

    fn implementation_preparation_failure(&self) -> anyhow::Error {
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

    fn impact_map_execution_failure(
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
            phase_calls_limit: self.phases.phase_limit(phase),
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
                .filter(|record| {
                    matches!(
                        record.class,
                        ToolProgressClass::RecoverableFailure | ToolProgressClass::BlockingFailure
                    )
                })
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

    fn prepare_next_model_call(
        &mut self,
        allow_budget_handoff: bool,
    ) -> Result<Option<ImplementationOutcome>> {
        loop {
            if let Some(outcome) = self.enforce_cost_and_time_guardrails(allow_budget_handoff)? {
                return Ok(Some(outcome));
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
            if used < limit {
                break;
            }
            match phase {
                ExecutionPhase::Discovery if self.impact_map.is_some() => {
                    self.transition_phase(
                        ExecutionPhase::Planning,
                        "discovery impact map completed",
                    )?;
                }
                ExecutionPhase::Discovery if self.impact_map_failure.is_some() => {
                    self.notebook.phase = ExecutionPhase::ArtifactRepair;
                    self.transition_phase(
                        ExecutionPhase::ArtifactRepair,
                        "impact map tool output requires one targeted artifact repair",
                    )?;
                }
                ExecutionPhase::Discovery => {
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
                    self.transition_phase(
                        ExecutionPhase::Planning,
                        "impact map recovered without repeating repository discovery",
                    )?;
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
                    self.transition_phase(
                        ExecutionPhase::Implementation,
                        "machine-readable implementation plan completed",
                    )?;
                }
                ExecutionPhase::Planning => {
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
                        self.transition_phase(
                            ExecutionPhase::Validation,
                            "implementation allocation ended with all targets applied",
                        )?;
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
            >= self.phases.phase_limit(self.phases.active())
        {
            self.emit_phase_budget_warning()?;
        }
        Ok(None)
    }

    fn repair(
        &mut self,
        failures: &[ValidationResult],
        attempt: usize,
    ) -> Result<ImplementationOutcome> {
        record_validation_repair_start(&mut self.notebook, failures);
        self.transition_phase(ExecutionPhase::Repair, "required validation failed")?;
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

    fn run_session(
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
            let active_context_phase = self.phases.active();
            if previous_context_phase.is_some_and(|phase| phase != active_context_phase) {
                turns.clear();
            }
            previous_context_phase = Some(active_context_phase);
            initial["content"] = Value::String(if artifact_repair {
                compact_impact_map_repair_context(self.impact_map_failure.as_ref(), &self.notebook)
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
            let max_output_tokens = self.manifest.ai_gateway.maximum_output_tokens.min(16_384);
            let active_phase = self.phases.active();
            let mut request = json!({
                "model": self.manifest.ai_gateway.model,
                "input": input,
                "instructions": hosted_agent_instructions(active_phase),
                "max_output_tokens": max_output_tokens,
                "reasoning": {"effort": "medium"},
                "tools": hosted_tools_for_phase(active_phase),
                "tool_choice": "auto",
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
            validate_provider_request_envelope(&request)?;
            if !constrain_request_to_cost_limit(&mut request, &self.cost_guard)? {
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
                        "resumable": true,
                    }),
                    "model cost preflight",
                );
                return Ok(ImplementationOutcome {
                    summary: "The next model call could not fit inside the hard estimated-cost envelope; preserved work is resumable.".into(),
                    budget_exhausted: true,
                    explicit_declaration: self.declaration.clone(),
                });
            }
            validate_provider_request_envelope(&request)?;

            let call_phase = self.phases.active();
            let model_call = self.phases.begin_model_call()?;
            let registration = ai_call_registration(
                self.manifest.execution.execution_id,
                self.api.execution_attempt,
                self.api.session_id()?,
                model_call.saturating_sub(1),
                call_phase,
                registration_attempt,
            );
            self.api.append_event(
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
            )?;
            let execution_deadline = hosted_execution_deadline(self.execution_started_at)?;
            let response = match self.api.ai_response_until(
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
                    }
                    let exhaustion_reason = ai_budget_exhaustion_reason(&error);
                    let changed_paths = self.repo.new_agent_paths(&BTreeSet::new())?;
                    if allow_budget_handoff
                        && !changed_paths.is_empty()
                        && exhaustion_reason.is_some()
                    {
                        let exhaustion_reason = exhaustion_reason.unwrap_or_default();
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
            self.observe_model_cost(&request, &response)?;
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
                        Some("inspect the complete repository diff")
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
                    ) && let Some(summary) = self.implementation_progress_halt_summary()?
                    {
                        return Ok(ImplementationOutcome {
                            summary,
                            budget_exhausted: false,
                            explicit_declaration: self.declaration.clone(),
                        });
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
            let mut mutation_preflight_halt = None;
            for (call_id, name, arguments) in function_calls {
                self.ensure_active_or_checkpoint_cancellation()?;
                if name == "record_impact_map" {
                    let supplemental = self.phases.active() == ExecutionPhase::ArtifactRepair;
                    self.api.append_event(
                        "progress",
                        json!({
                            "event_type":"worker.impact_map_artifact_attempt",
                            "failure_layer":ArtifactFailureLayer::ProviderToolArgumentGeneration,
                            "tool_schema_version":IMPACT_MAP_SCHEMA_VERSION,
                            "tool_schema_sha256":impact_map::schema_sha256(),
                            "validator_schema_version":IMPACT_MAP_SCHEMA_VERSION,
                            "validator_schema_sha256":impact_map::schema_sha256(),
                            "provider_call_occurred":true,
                            "configured_mission_budget_consumed":!supplemental,
                            "supplemental_repair_budget_consumed":supplemental,
                            "accounting": artifact_call_accounting(self.phases.active()),
                        }),
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
                let failed_reads_before = self.tool_usage.failed_reads;
                let mut progress_class = ToolProgressClass::Neutral;
                let mut progress_detail = String::new();
                let mut verified_repository_progress = false;
                let result = match self.execute_tool(&name, &arguments) {
                    Ok(output) => {
                        self.last_successful_action = json!({
                            "model_call": self.phases.total_calls(),
                            "phase": self.phases.active(),
                            "tool": name,
                            "target": target,
                        });
                        if is_source_mutation_tool(&name) {
                            self.notebook.implementation_substate =
                                ImplementationSubstate::Mutating;
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
                            self.notebook.write_attempts.push(attempt.clone());
                            if let Some(change) = self
                                .notebook
                                .intended_changes
                                .iter_mut()
                                .find(|change| change.change_id == attempt.change_id)
                            {
                                for target in &mut change.targets {
                                    if target_was_modified && target.path == attempt.target {
                                        target.status = IntendedChangeStatus::Applied;
                                    }
                                }
                                change.status = roll_up_target_statuses(&change.targets);
                                change.attempts.push(attempt);
                            }
                            if target_was_modified {
                                self.tool_usage.successful_writes =
                                    self.tool_usage.successful_writes.saturating_add(1);
                                self.last_source_progress_call = self.phases.total_calls();
                                self.cost_guard.repository_progress_score += 1.0;
                                self.notebook.implementation_substate =
                                    ImplementationSubstate::Mutating;
                                verified_repository_progress = true;
                                progress_class = ToolProgressClass::Productive;
                                progress_detail = "verified repository mutation".into();
                            } else {
                                progress_class = ToolProgressClass::Duplicate;
                                progress_detail =
                                    "mutation tool returned successfully but repository content did not change"
                                        .into();
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
                            self.notebook.failed_changes = self.tool_failures.clone();
                            if target_was_modified {
                                self.invalidate_validation_after_mutation()?;
                            }
                            self.reconcile_authoritative_target_state()?;
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
                                            self.transition_phase(
                                            ExecutionPhase::ArtifactRepair,
                                            "impact map tool failed; repository discovery is preserved",
                                        )?;
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
                        } else if let Some(preflight) =
                            error.downcast_ref::<MutationPreflightError>()
                        {
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
                        } else {
                            let error = truncate_text(&format!("{error:#}"), 4_000);
                            if is_source_mutation_tool(&name) {
                                let (error_code, match_count) = classify_write_failure(&error);
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
                                    status: WriteAttemptStatus::Failed,
                                    error_code: Some(error_code.clone()),
                                    match_count,
                                    intended_change_sha256: intended_change_sha256.clone(),
                                    before_sha256,
                                    after_sha256: None,
                                };
                                self.notebook.write_attempts.push(attempt.clone());
                                if let Some(change) = self
                                    .notebook
                                    .intended_changes
                                    .iter_mut()
                                    .find(|change| change.change_id == attempt.change_id)
                                {
                                    change.attempts.push(attempt);
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
                                self.notebook.failed_changes = self.tool_failures.clone();
                                self.transition_phase(
                                    ExecutionPhase::Repair,
                                    "source-changing tool failed and requires recovery",
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
                    progress_detail = truncate_text(error, 1_000);
                    if matches!(name.as_str(), "read_file" | "read_files" | "related_tests") {
                        if name != "read_files" {
                            self.tool_usage.failed_reads =
                                self.tool_usage.failed_reads.saturating_add(1);
                        }
                        progress_class = read_error_progress_class(error);
                    } else if name == "search_text" {
                        progress_class = if error.contains("duplicate_search") {
                            ToolProgressClass::Duplicate
                        } else {
                            ToolProgressClass::RecoverableFailure
                        };
                    } else if name == "run_focused_command" {
                        progress_class = ToolProgressClass::RecoverableFailure;
                        self.notebook.implementation_substate = ImplementationSubstate::Repairing;
                        self.transition_phase(
                            ExecutionPhase::Repair,
                            "focused implementation validation failed and requires source repair",
                        )?;
                    } else if is_source_mutation_tool(&name) {
                        progress_class = ToolProgressClass::RecoverableFailure;
                        self.notebook.implementation_substate = ImplementationSubstate::Repairing;
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
                let tool_event_persisted = self.append_event_recoverable(
                    "tool",
                    json!({
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
                    "tool event",
                );
                if retrying_impact_map_persistence && tool_event_persisted {
                    self.notebook.impact_map_artifact.persistence_status =
                        ArtifactPersistenceStatus::Persisted;
                } else if retrying_impact_map_persistence {
                    self.notebook.impact_map_artifact.persistence_status =
                        ArtifactPersistenceStatus::Failed;
                }
                turn.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": serde_json::to_string(&result)?
                }));
            }
            if let Some(summary) = self.implementation_progress_halt_summary()? {
                mutation_preflight_halt.get_or_insert(summary);
            }
            if self.reconcile_authoritative_target_state()?
                == ImplementationCompletionStatus::ReadyForValidation
            {
                self.phases.release_unused_implementation_capacity();
                self.transition_phase(
                    ExecutionPhase::Validation,
                    "all required planned targets are applied in the repository; worker-owned validation starts deterministically",
                )?;
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

    fn reconcile_write_failures(
        &mut self,
        implementation: &ImplementationOutcome,
        validation: &[ValidationResult],
        changed_paths: &[String],
    ) -> Vec<ToolFailureRecord> {
        let empty_plan = Vec::new();
        let planned_changes = self
            .implementation_plan
            .as_ref()
            .map(|plan| &plan.planned_changes)
            .unwrap_or(&empty_plan);
        reconcile_failed_write_attempts(
            &mut self.tool_failures,
            planned_changes,
            &self.notebook.write_attempts,
            implementation,
            validation,
            changed_paths,
        );
        let changed = changed_paths
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let all_validation_passed =
            !validation.is_empty() && validation.iter().all(|result| result.status == "passed");
        let declaration = implementation.explicit_declaration.as_ref();
        let declaration_complete =
            declaration.is_some_and(|value| value.implementation_status == "complete");
        let path_completion_evidence = planned_changes
            .iter()
            .flat_map(|change| {
                change.targets.iter().map(|target| {
                    json!({
                        "path": target.path,
                        "planned": true,
                        "changed": changed.contains(target.path.as_str()),
                        "verified": changed.contains(target.path.as_str())
                            && all_validation_passed
                            && declaration_complete,
                        "blocking_criteria": change.acceptance_criteria,
                    })
                })
            })
            .collect::<Vec<_>>();

        for intended in &mut self.notebook.intended_changes {
            intended.attempts = self
                .notebook
                .write_attempts
                .iter()
                .filter(|attempt| attempt.change_id == intended.change_id)
                .cloned()
                .collect();
            let related_failures = self
                .tool_failures
                .iter()
                .filter(|failure| failure.change_id.as_deref() == Some(&intended.change_id))
                .collect::<Vec<_>>();
            let unresolved = related_failures
                .iter()
                .any(|failure| failure.reconciliation == FailureReconciliation::StillUnresolved);
            intended.recovery = related_failures
                .iter()
                .find_map(|failure| failure.recovery.clone());
            for target in &mut intended.targets {
                let target_unresolved = related_failures.iter().any(|failure| {
                    failure.target.as_deref() == Some(target.path.as_str())
                        && failure.reconciliation == FailureReconciliation::StillUnresolved
                });
                target.status = if target_unresolved {
                    IntendedChangeStatus::Unresolved
                } else if changed.contains(target.path.as_str())
                    && all_validation_passed
                    && declaration_complete
                {
                    IntendedChangeStatus::Verified
                } else if changed.contains(target.path.as_str()) {
                    IntendedChangeStatus::Applied
                } else {
                    IntendedChangeStatus::Planned
                };
            }
            intended.status = if unresolved
                && intended
                    .targets
                    .iter()
                    .all(|target| target.status == IntendedChangeStatus::Unresolved)
            {
                IntendedChangeStatus::Unresolved
            } else {
                roll_up_target_statuses(&intended.targets)
            };
        }
        self.notebook.failed_changes = self.tool_failures.clone();
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.intended_changes_reconciled",
                "intended_changes": self.notebook.intended_changes,
                "failed_attempts": self.tool_failures,
                "final_changed_paths": changed_paths,
                "path_completion_evidence": path_completion_evidence,
                "validation": validation,
            }),
            "intended-change reconciliation",
        );
        self.tool_failures
            .iter()
            .filter(|failure| failure.reconciliation == FailureReconciliation::StillUnresolved)
            .cloned()
            .collect()
    }

    fn evaluate_completion(
        &mut self,
        implementation: &ImplementationOutcome,
        validation: &[ValidationResult],
        changed_paths: &[String],
    ) -> Result<CompletionEvaluation> {
        let unrecovered = self.reconcile_write_failures(implementation, validation, changed_paths);
        let fallback = completion_fallback(
            implementation,
            self.impact_map.as_ref(),
            self.implementation_plan.as_ref(),
            &unrecovered,
            changed_paths,
            &self.notebook.acceptance_criteria,
            validation,
            project_verification_policy(self.manifest),
        );
        self.transition_phase(
            ExecutionPhase::CompletionEvaluation,
            "technical validation finished; independent completion evaluation started",
        )?;
        if changed_paths.is_empty() {
            return Ok(fallback);
        }
        if self
            .phases
            .phase_calls(ExecutionPhase::CompletionEvaluation)
            >= self
                .phases
                .phase_limit(ExecutionPhase::CompletionEvaluation)
        {
            return Ok(fallback);
        }

        let diff = match completion_review_diff(
            &self.repo.root,
            changed_paths,
            &self.manifest.github.base_sha,
        ) {
            Ok(diff) => diff,
            Err(_) => return Ok(fallback),
        };
        let prompt = format!(
            "Independently evaluate whether this repository diff fully implements the ticket. \
Regression gates are only technical validation and cannot by themselves satisfy functional \
criteria. Every satisfied criterion must cite concrete diff evidence. Missing evidence is \
uncertain or incomplete. An unrecovered edit failure blocks complete. A broad task with a narrow \
diff needs explicit architectural evidence. Classify human, design, accessibility, visual, \
product-approval, and deployment-environment checks as external review rather than missing source \
implementation. Apply the supplied browser-test policy exactly. Return only one JSON object matching the requested \
schema.\n\nTicket title:\n{}\n\nTicket description and acceptance criteria:\n{}\n\nProject verification policy:\n{}\n\nImpact map:\n{}\n\nImplementation plan:\n{}\n\nWorker notebook:\n{}\n\nImplementation declaration:\n{}\n\nBudget exhausted: {}\n\nChanged paths:\n{}\n\nGenuinely unresolved intended changes:\n{}\n\nReconciled intended changes:\n{}\n\nTechnical validation:\n{}\n\nRepository diff:\n{}",
            self.manifest.ticket_title,
            self.manifest.run.input_prompt,
            serde_json::to_string(&project_verification_policy(self.manifest))
                .unwrap_or_else(|_| "{}".into()),
            serde_json::to_string(&self.impact_map).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(&self.implementation_plan).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(&self.notebook).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(&implementation.explicit_declaration)
                .unwrap_or_else(|_| "null".into()),
            implementation.budget_exhausted,
            changed_paths.join("\n"),
            serde_json::to_string(&unrecovered).unwrap_or_else(|_| "[]".into()),
            serde_json::to_string(&self.notebook.intended_changes).unwrap_or_else(|_| "[]".into()),
            serde_json::to_string(validation).unwrap_or_else(|_| "[]".into()),
            truncate_text(&diff, 96 * 1024),
        );
        let mut request = json!({
            "model": self.manifest.ai_gateway.model,
            "input": [{"role": "user", "content": prompt}],
            "instructions": completion_evaluator_instructions(),
            "max_output_tokens": self.manifest.ai_gateway.maximum_output_tokens.min(8_192),
            "reasoning": {"effort": "medium"},
            "store": false,
            "stream": false,
            "metadata": provider_request_metadata(
                self.manifest.execution.execution_id,
                self.manifest.ticket_key.as_str(),
                "rustgrid-completion-evaluator",
                ExecutionPhase::CompletionEvaluation,
                self.budget.resolved_model_call_budget,
            )
        });
        validate_provider_request_envelope(&request)?;
        let attempts_available = self
            .phases
            .phase_limit(ExecutionPhase::CompletionEvaluation)
            .saturating_sub(
                self.phases
                    .phase_calls(ExecutionPhase::CompletionEvaluation),
            );
        for evaluator_attempt in 0..attempts_available {
            if !constrain_request_to_cost_limit(&mut request, &self.cost_guard)? {
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.model_cost_preflight_stopped",
                        "phase": ExecutionPhase::CompletionEvaluation,
                        "estimated_cost_micros": self.cost_guard.estimated_cost_micros,
                        "hard_limit_micros": self.cost_guard.hard_limit_micros,
                        "model_calls_used": self.phases.total_calls(),
                        "source_mutation_observed": !changed_paths.is_empty(),
                        "resumable": true,
                    }),
                    "completion evaluation model cost preflight",
                );
                break;
            }
            validate_provider_request_envelope(&request)?;
            let model_call = self.phases.begin_model_call()?;
            self.api.append_event(
                "progress",
                json!({
                    "step": "completion_evaluation",
                    "status": if evaluator_attempt == 0 { "running" } else { "retrying" },
                    "evaluation_attempt": evaluator_attempt + 1,
                    "phase": ExecutionPhase::CompletionEvaluation,
                    "model_call": model_call,
                    "budget": self.budget_telemetry(),
                }),
            )?;
            let registration = ai_call_registration(
                self.manifest.execution.execution_id,
                self.api.execution_attempt,
                self.api.session_id()?,
                model_call.saturating_sub(1),
                ExecutionPhase::CompletionEvaluation,
                0,
            );
            let execution_deadline = hosted_execution_deadline(self.execution_started_at)?;
            let evaluated_response = match self.api.ai_response_until(
                request.clone(),
                &registration,
                Some(execution_deadline),
            ) {
                Ok(response) => {
                    self.record_cache_observability(&request, &response);
                    self.observe_model_cost(&request, &response)?;
                    Some(response)
                }
                Err(error) => {
                    let http = error.downcast_ref::<HostedHttpError>();
                    if http.map(HostedHttpError::budget_disposition)
                        == Some(AiBudgetDisposition::Restore)
                    {
                        self.phases
                            .rollback_model_call(ExecutionPhase::CompletionEvaluation)?;
                        if let Some(failure) = http.filter(|failure| {
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
                                "completion evaluator provider rejection telemetry",
                            );
                        }
                    }
                    None
                }
            };
            let evaluated = evaluated_response
                .and_then(|response| response_message_text(&response))
                .and_then(|text| parse_completion_evaluation(&text).ok())
                .map(|evaluation| {
                    reconcile_model_completion_evaluation(
                        evaluation,
                        fallback.clone(),
                        implementation,
                        &unrecovered,
                    )
                })
                .and_then(|evaluation| {
                    validate_completion_evaluation(
                        evaluation,
                        implementation,
                        &unrecovered,
                        changed_paths,
                        &self.notebook.acceptance_criteria,
                    )
                    .ok()
                });
            if let Some(evaluated) = evaluated {
                return Ok(evaluated);
            }
        }
        Ok(fallback)
    }

    fn deterministic_diff_review(&mut self) -> Result<Vec<String>> {
        if self
            .notebook
            .required_gates
            .iter()
            .any(|gate| gate.required && gate.status != ValidationStatus::Passed)
        {
            bail!("lifecycle invariant violated: diff review requires every required gate to pass");
        }
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
        let changed = changed_paths.iter().cloned().collect::<BTreeSet<_>>();
        let planned = self
            .notebook
            .intended_changes
            .iter()
            .flat_map(|change| change.targets.iter().map(|target| target.path.clone()))
            .collect::<BTreeSet<_>>();
        let unplanned = changed.difference(&planned).cloned().collect::<Vec<_>>();
        let planned_but_unchanged = planned.difference(&changed).cloned().collect::<Vec<_>>();
        self.transition_phase(
            ExecutionPhase::DiffReview,
            "required validation gates passed; orchestrator is reviewing the final repository diff",
        )?;
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.diff_review_completed",
                "changed_paths": changed_paths,
                "unplanned_paths": unplanned,
                "planned_but_unchanged": planned_but_unchanged,
                "source_tree_hash": repository_state_fingerprint(
                    self.repo,
                    &self.manifest.github.base_sha,
                )?,
            }),
            "deterministic diff review",
        );
        Ok(changed_paths)
    }

    fn preflight_source_mutation(
        &mut self,
        name: &str,
        object: &serde_json::Map<String, Value>,
    ) -> Result<()> {
        if self.impact_map.is_none() {
            return Err(MutationPreflightError {
                code: "mutation_policy_denied",
                change_id: String::new(),
                target: String::new(),
                message: "record_impact_map is required before source-changing tools".into(),
                repair_strategy: "complete_required_artifact",
            }
            .into());
        }
        let Some(plan) = self.implementation_plan.as_mut() else {
            return Err(MutationPreflightError {
                code: "mutation_policy_denied",
                change_id: String::new(),
                target: String::new(),
                message: "record_implementation_plan is required before source-changing tools"
                    .into(),
                repair_strategy: "complete_required_artifact",
            }
            .into());
        };
        let change_id = required_tool_string(object, "change_id", 100)?.to_owned();
        let raw_path = required_tool_string(object, "path", 4_096)?;
        let normalized_paths =
            normalized_planned_paths(raw_path).map_err(|error| MutationPreflightError {
                code: "mutation_target_path_invalid",
                change_id: change_id.clone(),
                target: raw_path.to_owned(),
                message: error.to_string(),
                repair_strategy: "repair_plan_metadata",
            })?;
        if normalized_paths.len() != 1 {
            return Err(MutationPreflightError {
                code: "mutation_target_path_invalid",
                change_id,
                target: raw_path.to_owned(),
                message: "source-changing tool target must be one concrete repository path".into(),
                repair_strategy: "repair_plan_metadata",
            }
            .into());
        }
        let path = normalized_paths[0].as_str();

        if let Some(repair) =
            repair_implementation_plan(&mut plan.planned_changes, &change_id, path)?
        {
            validate_planned_change_paths(&self.repo.root, &plan.planned_changes)?;
            self.notebook.planned_changes = plan.planned_changes.clone();
            self.notebook.intended_changes = intended_changes_from_plan(&plan.planned_changes);
            self.api.append_event(
                "progress",
                json!({
                    "event_type": "worker.implementation_plan_repaired",
                    "change_id": repair.change_id,
                    "targets_before": repair.targets_before,
                    "targets_after": repair.targets_after,
                    "attempted_concrete_path": repair.attempted_concrete_path,
                    "validation_error": repair.validation_error,
                    "repair_source": repair.repair_source,
                    "model_call_consumed": repair.model_call_consumed,
                }),
            )?;
        }
        let target = authorize_planned_target(plan, &change_id, path)?;
        safe_repo_path(&self.repo.root, path, target.new_file).map_err(|error| {
            MutationPreflightError {
                code: if error.to_string().contains("escape") {
                    "mutation_target_outside_repository"
                } else {
                    "mutation_target_path_invalid"
                },
                change_id: change_id.clone(),
                target: path.to_owned(),
                message: error.to_string(),
                repair_strategy: "repair_plan_metadata",
            }
        })?;
        validate_write_repair_strategy(
            &self.notebook.write_attempts,
            path,
            &change_id,
            name,
            self.repair_read_targets.contains(path),
        )
        .map_err(|error| MutationPreflightError {
            code: "mutation_content_conflict",
            change_id: change_id.clone(),
            target: path.to_owned(),
            message: error.to_string(),
            repair_strategy: "return_partial_result",
        })?;
        Ok(())
    }

    fn execute_tool(&mut self, name: &str, raw_arguments: &str) -> Result<String> {
        let arguments: Value =
            serde_json::from_str(raw_arguments).context("tool arguments are not valid JSON")?;
        let object = arguments
            .as_object()
            .context("tool arguments must be an object")?;
        self.validate_tool_for_phase(name, object)?;
        if is_source_mutation_tool(name) {
            self.preflight_source_mutation(name, object)?;
        }
        if name != "search_text" {
            self.search_guard.record_non_search();
        }
        match name {
            "list_files" => {
                self.tool_usage.reads = self.tool_usage.reads.saturating_add(1);
                let path = object.get("path").and_then(Value::as_str).unwrap_or(".");
                let root = safe_repo_path(&self.repo.root, path, false)?;
                let files = collect_repo_files(&self.repo.root, &root, 1_000)?;
                push_unique(
                    &mut self.notebook.architecture_findings,
                    format!("Repository tree inspected under {path}."),
                );
                Ok(files.join("\n"))
            }
            "read_file" => {
                self.tool_usage.reads = self.tool_usage.reads.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let start_line = object
                    .get("start_line")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1);
                let end_line = object
                    .get("end_line")
                    .and_then(Value::as_u64)
                    .unwrap_or(start_line.saturating_add(399))
                    .min(start_line.saturating_add(999));
                let result = read_repo_file_result(
                    &self.repo.root,
                    path,
                    start_line,
                    end_line,
                    MAX_TOOL_OUTPUT_BYTES,
                );
                if result.status == FileReadStatus::Error {
                    bail!(
                        "{}: {}",
                        result.error_code.as_deref().unwrap_or("read_failed"),
                        result.error_message.as_deref().unwrap_or("read failed")
                    );
                }
                push_unique(
                    &mut self.notebook.read_ranges_inspected,
                    format!("{path}:{start_line}-{end_line}"),
                );
                if let Some(reason) = object.get("reason").and_then(Value::as_str) {
                    record_centralized_discovery_finding(&mut self.notebook, reason);
                }
                push_unique(&mut self.notebook.files_inspected, path.to_owned());
                if self.phases.active() == ExecutionPhase::Repair {
                    self.repair_read_targets.insert(path.to_owned());
                }
                Ok(serde_json::to_string(&result)?)
            }
            "read_files" => {
                self.tool_usage.reads = self.tool_usage.reads.saturating_add(1);
                let paths = object
                    .get("paths")
                    .and_then(Value::as_array)
                    .context("tool argument `paths` is missing")?;
                if paths.is_empty() || paths.len() > 20 {
                    bail!("read_files requires between 1 and 20 paths");
                }
                let maximum_lines = object
                    .get("maximum_lines_per_file")
                    .and_then(Value::as_u64)
                    .unwrap_or(800)
                    .clamp(1, 1_000);
                let inspected_before_batch = self
                    .notebook
                    .files_inspected
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                // Admission is a distinct first pass: every requested path receives structured
                // malformed, unsafe, missing, or ready metadata before any valid file is read.
                let prevalidated = prevalidate_batch_read_paths(&self.repo.root, paths);
                let (batch, batch_failures) = read_prevalidated_repo_files_with_fallback(
                    &self.repo.root,
                    &prevalidated,
                    maximum_lines,
                    MAX_TOOL_OUTPUT_BYTES,
                );
                let files = batch.files;
                for result in &files {
                    if result.status == FileReadStatus::Success {
                        push_unique(&mut self.notebook.files_inspected, result.path.clone());
                        if self.phases.active() == ExecutionPhase::Repair {
                            self.repair_read_targets.insert(result.path.clone());
                        }
                    }
                }
                self.tool_usage.failed_reads =
                    self.tool_usage.failed_reads.saturating_add(batch_failures);
                if files
                    .iter()
                    .any(|result| result.status == FileReadStatus::Success)
                    && let Some(reason) = object.get("reason").and_then(Value::as_str)
                {
                    record_centralized_discovery_finding(&mut self.notebook, reason);
                }
                for failed in files
                    .iter()
                    .filter(|result| result.status == FileReadStatus::Error)
                {
                    self.record_tool_progress(
                        "read_file",
                        Some(failed.path.clone()),
                        read_error_progress_class(
                            failed.error_code.as_deref().unwrap_or("read_failed"),
                        ),
                        format!(
                            "{}: {}; valid_line_range={}; individual_fallback_attempted={}",
                            failed.error_code.as_deref().unwrap_or("read_failed"),
                            failed.error_message.as_deref().unwrap_or("read failed"),
                            failed.valid_line_range.as_deref().unwrap_or("unavailable"),
                            failed.fallback_attempted,
                        ),
                        false,
                    );
                }
                for succeeded in files
                    .iter()
                    .filter(|result| result.status == FileReadStatus::Success)
                {
                    let new_evidence = !inspected_before_batch.contains(succeeded.path.as_str());
                    self.record_tool_progress(
                        "read_file",
                        Some(succeeded.path.clone()),
                        if new_evidence {
                            ToolProgressClass::Productive
                        } else {
                            ToolProgressClass::Duplicate
                        },
                        if !new_evidence {
                            "batch read repeated previously inspected repository content"
                        } else if succeeded.fallback_attempted {
                            "individual fallback recovered the batch read"
                        } else {
                            "batch read returned repository content"
                        },
                        false,
                    );
                }
                let fallback_results = files
                    .iter()
                    .filter(|result| result.fallback_attempted)
                    .map(|result| {
                        json!({
                            "path": result.path,
                            "status": result.status,
                            "error_code": result.error_code,
                            "error_message": result.error_message,
                            "valid_line_range": result.valid_line_range,
                        })
                    })
                    .collect::<Vec<_>>();
                if !fallback_results.is_empty() {
                    self.append_event_recoverable(
                        "progress",
                        json!({
                            "event_type": "worker.batch_read_fallback",
                            "fallback": "individual_read_once",
                            "results": fallback_results,
                            "model_call_consumed": false,
                        }),
                        "batch-read individual fallback",
                    );
                }
                Ok(serde_json::to_string(&BatchReadResult { files })?)
            }
            "search_text" => {
                self.tool_usage.searches = self.tool_usage.searches.saturating_add(1);
                let query = required_tool_string(object, "query", 200)?;
                let path = object.get("path").and_then(Value::as_str).unwrap_or(".");
                let extensions = object
                    .get("extensions")
                    .and_then(Value::as_array)
                    .context("tool argument `extensions` is missing")?
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .filter(|extension| extension.len() <= 20)
                            .map(str::to_owned)
                            .context("search extension is malformed")
                    })
                    .collect::<Result<Vec<_>>>()?;
                let context_lines = object
                    .get("context_lines")
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
                    .min(5);
                let mode = object
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("literal");
                let signature = SearchSignature::new(query, path, &extensions, mode, context_lines);
                if let Err(error) = self.search_guard.validate(&signature) {
                    self.emit_guardrail(
                        "search_loop_detected",
                        if self.phases.active() == ExecutionPhase::Discovery {
                            "force_planning"
                        } else {
                            "reject_search"
                        },
                        &error.to_string(),
                    )?;
                    return Err(error);
                }
                let discovery_coverage = localized_discovery_coverage(&self.notebook);
                let known_consumers = self
                    .notebook
                    .files_inspected
                    .iter()
                    .chain(self.notebook.discovery_paths_sampled.iter())
                    .filter(|path| !localized_discovery_core_path(path))
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let maximum_new_consumers = (self.phases.active() == ExecutionPhase::Discovery
                    && localized_visual_goal(&self.notebook.goal)
                    && discovery_coverage.centralized_abstraction)
                    .then(|| 3_usize.saturating_sub(discovery_coverage.representative_consumers));
                let result = search_repo(
                    &self.repo.root,
                    path,
                    query,
                    &extensions,
                    context_lines,
                    maximum_new_consumers,
                    &known_consumers,
                )?;
                self.search_guard.record(signature, result.truncated);
                if let Some(reason) = object.get("reason").and_then(Value::as_str) {
                    record_centralized_discovery_finding(&mut self.notebook, reason);
                }
                push_unique(
                    &mut self.notebook.searches_completed,
                    format!("{mode}:{path}:{query}"),
                );
                for matched_path in &result.matched_paths {
                    if !localized_discovery_core_path(matched_path) {
                        push_unique(
                            &mut self.notebook.discovery_paths_sampled,
                            matched_path.clone(),
                        );
                    }
                }
                if self.tool_usage.searches == 4 && self.impact_map.is_none() {
                    self.emit_guardrail(
                        "discovery_search_warning",
                        "narrow_impact_map",
                        "Four searches have run without a completed impact map.",
                    )?;
                }
                Ok(result.output)
            }
            "related_tests" => {
                self.tool_usage.reads = self.tool_usage.reads.saturating_add(1);
                let paths = object
                    .get("paths")
                    .and_then(Value::as_array)
                    .context("tool argument `paths` is missing")?
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                if paths.is_empty() || paths.len() > 20 {
                    bail!("related_tests requires between 1 and 20 source paths");
                }
                let stems = paths
                    .iter()
                    .filter_map(|path| Path::new(path).file_stem())
                    .filter_map(|stem| stem.to_str())
                    .map(str::to_ascii_lowercase)
                    .collect::<BTreeSet<_>>();
                let related = collect_repo_files(&self.repo.root, &self.repo.root, 2_000)?
                    .into_iter()
                    .filter(|candidate| {
                        let lower = candidate.to_ascii_lowercase();
                        (lower.contains("test") || lower.contains("spec"))
                            && stems.iter().any(|stem| lower.contains(stem))
                    })
                    .take(100)
                    .collect::<Vec<_>>();
                if let Some(reason) = object.get("reason").and_then(Value::as_str) {
                    record_centralized_discovery_finding(&mut self.notebook, reason);
                }
                for path in &related {
                    push_unique(
                        &mut self.notebook.searches_completed,
                        format!("related_test:{path}"),
                    );
                }
                Ok(if related.is_empty() {
                    "no related test files found".into()
                } else {
                    format!("related_test_paths:\n{}", related.join("\n"))
                })
            }
            "write_file" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let content = required_tool_string(object, "content", MAX_MODEL_FILE_BYTES)?;
                let output = write_repo_file(&self.repo.root, path, content, false)?;
                push_unique(&mut self.notebook.completed_changes, path.to_owned());
                Ok(output)
            }
            "replace_text" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let old_text = required_tool_string(object, "old_text", MAX_MODEL_FILE_BYTES)?;
                let new_text = object
                    .get("new_text")
                    .and_then(Value::as_str)
                    .filter(|value| value.len() <= MAX_MODEL_FILE_BYTES)
                    .context("tool argument `new_text` is missing or too large")?;
                let output = replace_unique_repo_text(&self.repo.root, path, old_text, new_text)?;
                push_unique(&mut self.notebook.completed_changes, path.to_owned());
                Ok(output)
            }
            "replace_range" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let start_line = object
                    .get("start_line")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .context("replace_range start_line is missing or invalid")?;
                let end_line = object
                    .get("end_line")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .context("replace_range end_line is missing or invalid")?;
                let new_text = required_tool_string(object, "new_text", MAX_MODEL_FILE_BYTES)?;
                let output =
                    replace_repo_range(&self.repo.root, path, start_line, end_line, new_text)?;
                push_unique(&mut self.notebook.completed_changes, path.to_owned());
                Ok(output)
            }
            "insert_after_symbol" | "insert_before_symbol" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let symbol = required_tool_string(object, "symbol", MAX_MODEL_FILE_BYTES)?;
                let content = required_tool_string(object, "content", MAX_MODEL_FILE_BYTES)?;
                let output = insert_relative_to_symbol(
                    &self.repo.root,
                    path,
                    symbol,
                    content,
                    name == "insert_after_symbol",
                )?;
                push_unique(&mut self.notebook.completed_changes, path.to_owned());
                Ok(output)
            }
            "apply_unified_diff" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let patch = required_tool_string(object, "patch", MAX_MODEL_FILE_BYTES)?;
                let output = apply_repo_unified_diff(&self.repo.root, path, patch)?;
                push_unique(&mut self.notebook.completed_changes, path.to_owned());
                Ok(output)
            }
            "rewrite_small_file" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let content =
                    required_tool_string(object, "content", MAX_SMALL_FILE_REWRITE_BYTES)?;
                let output = write_repo_file(&self.repo.root, path, content, true)?;
                push_unique(&mut self.notebook.completed_changes, path.to_owned());
                Ok(output)
            }
            "delete_file" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let output = delete_repo_file(&self.repo.root, path)?;
                push_unique(&mut self.notebook.completed_changes, path.to_owned());
                Ok(output)
            }
            "run_focused_command" => {
                self.tool_usage.validation_commands =
                    self.tool_usage.validation_commands.saturating_add(1);
                self.tool_usage.focused_validations =
                    self.tool_usage.focused_validations.saturating_add(1);
                let command_text = required_tool_string(object, "command", 8 * 1024)?;
                validate_model_command(command_text)?;
                let source_tree_hash =
                    repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
                supersede_stale_validation(
                    &mut self.notebook.validation_evidence,
                    &source_tree_hash,
                );
                let dependency_lock_hash = dependency_lock_fingerprint(&self.repo.root)?;
                let environment_fingerprint =
                    relevant_environment_fingerprint(&self.manifest.execution_policy)?;
                let fingerprint = validation_fingerprint(
                    command_text,
                    self.repo.root.to_string_lossy().as_ref(),
                    &source_tree_hash,
                    &dependency_lock_hash,
                    &environment_fingerprint,
                );
                if let Some(evidence) =
                    passed_evidence(&self.notebook.validation_evidence, &fingerprint).cloned()
                {
                    self.tool_usage.deduplicated_validations =
                        self.tool_usage.deduplicated_validations.saturating_add(1);
                    self.append_event_recoverable(
                        "validation",
                        json!({
                            "event_type": "worker.validation_deduplicated",
                            "gate_id": evidence.gate_id,
                            "evidence_id": evidence.evidence_id,
                            "command_fingerprint": fingerprint,
                            "source_tree_hash": source_tree_hash,
                        }),
                        "focused validation deduplication",
                    );
                    return Ok(format!(
                        "reused passed validation evidence {} for unchanged source tree",
                        evidence.evidence_id
                    ));
                }
                if let Some(evidence) =
                    self.notebook
                        .validation_evidence
                        .iter()
                        .rev()
                        .find(|evidence| {
                            evidence.command_fingerprint == fingerprint
                                && matches!(
                                    evidence.status,
                                    ValidationStatus::Failed
                                        | ValidationStatus::TimedOut
                                        | ValidationStatus::Cancelled
                                )
                        })
                {
                    self.tool_usage.deduplicated_validations =
                        self.tool_usage.deduplicated_validations.saturating_add(1);
                    bail!(
                        "focused_validation_duplicate_failed: evidence {} already failed for the unchanged source tree; mutate relevant source or dependency state before retrying",
                        evidence.evidence_id
                    );
                }
                let focused_for_tree = self
                    .notebook
                    .validation_evidence
                    .iter()
                    .filter(|evidence| {
                        evidence.gate_type == ValidationGateType::FocusedTest
                            && evidence.source_tree_hash == source_tree_hash
                            && evidence.status != ValidationStatus::Superseded
                    })
                    .count();
                if focused_for_tree >= 3 {
                    bail!(
                        "focused_validation_limit_reached: at most 3 focused commands may run after a source change"
                    );
                }
                let allowlist = self.manifest.execution_policy.child_environment_allowlist();
                let evidence_id = format!("focused-{}", &fingerprint[..12]);
                self.notebook.validation_evidence.push(new_running_evidence(
                    evidence_id.clone(),
                    evidence_id.clone(),
                    ValidationGateType::FocusedTest,
                    command_text.to_owned(),
                    fingerprint.clone(),
                    source_tree_hash.clone(),
                    dependency_lock_hash,
                    ValidationSource::ModelRequested,
                ));
                let started = Instant::now();
                let captured = command::capture_hosted_cancellable_with_environment(
                    command_text,
                    &self.repo.root,
                    self.running,
                    Duration::from_secs(180),
                    MAX_TOOL_OUTPUT_BYTES,
                    Some(&allowlist),
                    None,
                    self.containment,
                );
                let (status, exit_code, stdout, stderr) = match &captured {
                    Ok(output) => (
                        if output.status.success() {
                            ValidationStatus::Passed
                        } else {
                            ValidationStatus::Failed
                        },
                        output.status.code(),
                        output.stdout.clone(),
                        output.stderr.clone(),
                    ),
                    Err(error) => (
                        if !self.running.load(Ordering::SeqCst) || shutdown::requested() {
                            ValidationStatus::Cancelled
                        } else if error.to_string().to_ascii_lowercase().contains("timed out") {
                            ValidationStatus::TimedOut
                        } else {
                            ValidationStatus::Failed
                        },
                        None,
                        String::new(),
                        truncate_text(&format!("{error:#}"), MAX_TOOL_OUTPUT_BYTES / 2),
                    ),
                };
                if let Some(evidence) = self
                    .notebook
                    .validation_evidence
                    .iter_mut()
                    .find(|evidence| evidence.evidence_id == evidence_id)
                {
                    evidence.completed_at = Some(now_rfc3339());
                    evidence.duration_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    evidence.exit_code = exit_code;
                    evidence.status = status;
                    evidence.stdout_summary = truncate_text(&stdout, 8_000);
                    evidence.stderr_summary = truncate_text(&stderr, 8_000);
                }
                self.append_event_recoverable(
                    "validation",
                    json!({
                        "event_type": "worker.validation_completed",
                        "gate_id": evidence_id,
                        "command_fingerprint": fingerprint,
                        "source_tree_hash": source_tree_hash,
                        "status": status,
                        "source": ValidationSource::ModelRequested,
                    }),
                    "focused validation evidence",
                );
                let output = captured?;
                if !output.status.success() {
                    bail!(
                        "focused command exited with {}\nstdout:\n{}\nstderr:\n{}",
                        output.status,
                        truncate_text(&output.stdout, MAX_TOOL_OUTPUT_BYTES / 2),
                        truncate_text(&output.stderr, MAX_TOOL_OUTPUT_BYTES / 2)
                    );
                }
                Ok(format!(
                    "exit={}\nstdout:\n{}\nstderr:\n{}",
                    output.status,
                    truncate_text(&output.stdout, MAX_TOOL_OUTPUT_BYTES / 2),
                    truncate_text(&output.stderr, MAX_TOOL_OUTPUT_BYTES / 2)
                ))
            }
            "repository_snapshot" => {
                if matches!(
                    self.phases.active(),
                    ExecutionPhase::Implementation | ExecutionPhase::Repair
                ) {
                    let reallocated = self.phases.release_unused_implementation_capacity();
                    for (target, calls) in [
                        ("diff_review", reallocated.diff_review_calls),
                        (
                            "completion_evaluation",
                            reallocated.completion_evaluation_calls,
                        ),
                    ] {
                        if calls > 0 {
                            self.append_event_recoverable(
                                "progress",
                                json!({
                                    "event_type": "worker.phase_budget_reallocated",
                                    "from": "implementation_repair",
                                    "to": target,
                                    "calls": calls,
                                    "reason": "implementation_finished_early",
                                    "budget": self.budget_telemetry(),
                                }),
                                "phase budget reallocation telemetry",
                            );
                        }
                    }
                }
                self.transition_phase(
                    ExecutionPhase::DiffReview,
                    "implementation requested complete repository diff review",
                )?;
                self.tool_usage.reads = self.tool_usage.reads.saturating_add(1);
                let paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
                let diff = completion_review_diff(
                    &self.repo.root,
                    &paths,
                    &self.manifest.github.base_sha,
                )?;
                let digest = hex::encode(Sha256::digest(diff.as_bytes()));
                let requested_cursor = object
                    .get("cursor")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize;
                if requested_cursor != self.diff_review_cursor {
                    bail!(
                        "repository_snapshot cursor mismatch: expected {}, received {requested_cursor}",
                        self.diff_review_cursor
                    );
                }
                if self
                    .diff_review_digest
                    .as_ref()
                    .is_some_and(|previous| previous != &digest)
                {
                    self.diff_review_cursor = 0;
                    self.diff_review_digest = None;
                    bail!("repository diff changed during review; restart at cursor 0");
                }
                self.diff_review_digest = Some(digest.clone());
                let start = requested_cursor.min(diff.len());
                let mut end = start
                    .saturating_add(MAX_TOOL_OUTPUT_BYTES.saturating_sub(8 * 1024))
                    .min(diff.len());
                while end > start && !diff.is_char_boundary(end) {
                    end -= 1;
                }
                let next_cursor = (end < diff.len()).then_some(end);
                self.diff_review_cursor = next_cursor.unwrap_or(diff.len());
                let status = command::checked(
                    "git",
                    ["status", "--short", "--untracked-files=all"],
                    &self.repo.root,
                )?;
                let statistics = command::checked(
                    "git",
                    ["diff", "--stat", "--no-ext-diff", "--"],
                    &self.repo.root,
                )?;
                self.diff_reviewed = next_cursor.is_none();
                Ok(format!(
                    "git_status:\n{status}\n\ndiff_statistics:\n{statistics}\n\nchanged_paths:\n{}\n\ndiff_sha256: {digest}\nreview_cursor: {start}\nnext_cursor: {}\nreview_complete: {}\n\ndiff_page:\n{}",
                    paths.join("\n"),
                    next_cursor
                        .map(|cursor| cursor.to_string())
                        .unwrap_or_else(|| "null".into()),
                    self.diff_reviewed,
                    &diff[start..end],
                ))
            }
            "record_impact_map" => {
                let (map, source) =
                    impact_map_from_value(Value::Object(object.clone()), &self.notebook)
                        .context("impact map is malformed")?;
                self.accept_impact_map(map, source, 1.0, None)
            }
            "record_implementation_plan" => {
                let mut repair = recover_planning_repair_state(
                    &self.repo.root,
                    object,
                    self.phases.total_calls(),
                );
                let mut plan: ImplementationPlan =
                    match serde_json::from_value(Value::Object(object.clone())) {
                        Ok(plan) => plan,
                        Err(error) => {
                            repair
                                .invalid_fields
                                .push(format!("$: {}", truncate_text(&error.to_string(), 500)));
                            self.notebook.planning_repair = Some(repair.clone());
                            self.append_event_recoverable(
                                "progress",
                                json!({
                                    "event_type": "worker.implementation_plan_repair_required",
                                    "valid_planned_changes": repair.valid_planned_changes,
                                    "invalid_fields": repair.invalid_fields,
                                    "repair_scope": "invalid_fields_only",
                                }),
                                "implementation plan repair",
                            );
                            bail!(
                                "implementation_plan_repair_required: {}",
                                serde_json::to_string(&repair.invalid_fields)?
                            );
                        }
                    };
                merge_preserved_plan_fragments(
                    &mut plan.planned_changes,
                    self.notebook.planning_repair.as_ref(),
                );
                let normalized_legacy_targets =
                    match normalize_planned_changes(&mut plan.planned_changes).and_then(|count| {
                        validate_planned_change_paths(&self.repo.root, &plan.planned_changes)?;
                        Ok(count)
                    }) {
                        Ok(count) => count,
                        Err(error) => {
                            repair.invalid_fields.push(format!(
                                "$.planned_changes: {}",
                                truncate_text(&error.to_string(), 500)
                            ));
                            self.notebook.planning_repair = Some(repair.clone());
                            bail!(
                                "implementation_plan_repair_required: {}",
                                serde_json::to_string(&repair.invalid_fields)?
                            );
                        }
                    };
                if !matches!(plan.implementation_status.as_str(), "ready" | "blocked")
                    || (plan.implementation_status == "ready" && plan.planned_changes.is_empty())
                    || plan.planned_changes.iter().any(|change| {
                        change.targets.is_empty()
                            || change.change.trim().is_empty()
                            || change.reason.trim().is_empty()
                            || change.acceptance_criteria.is_empty()
                    })
                {
                    repair.invalid_fields.push(
                        "$: implementation_status and every planned change require complete fields"
                            .into(),
                    );
                    self.notebook.planning_repair = Some(repair.clone());
                    bail!(
                        "implementation_plan_repair_required: {}",
                        serde_json::to_string(&repair.invalid_fields)?
                    );
                }
                if plan.implementation_status == "ready"
                    && self.impact_map.as_ref().is_some_and(|map| {
                        let planned_criteria = plan
                            .planned_changes
                            .iter()
                            .flat_map(|change| &change.acceptance_criteria)
                            .map(|criterion| criterion.trim())
                            .collect::<BTreeSet<_>>();
                        map.areas
                            .iter()
                            .flat_map(|area| &area.acceptance_criteria_ids)
                            .filter_map(|id| {
                                id.strip_prefix("ac-")
                                    .and_then(|n| n.parse::<usize>().ok())
                                    .and_then(|n| {
                                        self.notebook.acceptance_criteria.get(n.saturating_sub(1))
                                    })
                            })
                            .any(|criterion| !planned_criteria.contains(criterion.trim()))
                    })
                {
                    repair.invalid_fields.push(
                        "$.planned_changes[*].acceptance_criteria: ready plan must map every impact-map criterion"
                            .into(),
                    );
                    self.notebook.planning_repair = Some(repair.clone());
                    bail!(
                        "implementation_plan_repair_required: {}",
                        serde_json::to_string(&repair.invalid_fields)?
                    );
                }
                let target_count = plan
                    .planned_changes
                    .iter()
                    .map(|change| change.targets.len())
                    .sum::<usize>();
                let complexity_call_limit = self.phases.apply_ticket_complexity(target_count);
                self.cost_guard.hard_limit_micros = model_cost_limit_for_target_count(target_count);
                self.api.append_event(
                    "progress",
                    json!({
                        "event_type": "worker.implementation_plan_validated",
                        "change_count": plan.planned_changes.len(),
                        "target_count": target_count,
                        "normalized_legacy_targets": normalized_legacy_targets,
                        "normalization_source": (normalized_legacy_targets > 0)
                            .then_some("legacy_semicolon_target"),
                        "ticket_complexity_call_limit": complexity_call_limit,
                    }),
                )?;
                self.notebook.planned_changes = plan.planned_changes.clone();
                self.notebook.intended_changes = intended_changes_from_plan(&plan.planned_changes);
                self.notebook.implementation_substate = ImplementationSubstate::Preparing;
                self.notebook.planning_repair = None;
                self.notebook.write_attempts.clear();
                self.notebook.remaining_work_v2 =
                    derive_remaining_work(&self.notebook.intended_changes);
                self.notebook.remaining_work =
                    legacy_remaining_work(&self.notebook.remaining_work_v2);
                self.notebook.blocking_unknowns = plan.blocking_unknowns.clone();
                let ready = plan.implementation_status == "ready";
                self.implementation_plan = Some(plan);
                self.guided_first_write_recovery_issued = false;
                self.last_repository_progress_call = 0;
                if ready {
                    self.blocked_plan_recorded_at = None;
                    self.transition_phase(
                        ExecutionPhase::Implementation,
                        "machine-readable implementation plan is ready",
                    )?;
                } else {
                    self.blocked_plan_recorded_at =
                        Some(self.phases.phase_calls(ExecutionPhase::Planning));
                }
                Ok(if ready {
                    "recorded implementation plan; transition to implementation".into()
                } else {
                    "recorded blocked implementation plan; one targeted inspection cycle remains"
                        .into()
                })
            }
            "report_write_progress" => {
                let status = required_tool_string(object, "status", 64)?;
                let reason = required_tool_string(object, "reason", 2_000)?;
                informational_write_progress(status, reason)
            }
            "declare_implementation" => {
                if !self.diff_reviewed {
                    bail!(
                        "repository_snapshot is required after the final source change and before implementation declaration"
                    );
                }
                let declaration: ImplementationDeclaration =
                    serde_json::from_value(Value::Object(object.clone()))
                        .context("implementation declaration is malformed")?;
                if !matches!(
                    declaration.implementation_status.as_str(),
                    "complete" | "partial" | "blocked"
                ) {
                    bail!("implementation declaration has an unsupported status");
                }
                let actual_paths =
                    completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
                if declaration.changed_paths != actual_paths {
                    bail!(
                        "implementation declaration changed_paths must exactly match the reviewed repository paths"
                    );
                }
                if declaration.implementation_status == "complete"
                    && (declaration.criteria_evidence.is_empty()
                        || declaration.criteria_evidence.iter().any(|criterion| {
                            criterion.criterion.trim().is_empty()
                                || criterion.evidence.trim().is_empty()
                                || criterion.paths.is_empty()
                                || criterion
                                    .paths
                                    .iter()
                                    .any(|path| !actual_paths.contains(path))
                        }))
                {
                    bail!(
                        "a complete implementation declaration requires criterion evidence tied to changed paths"
                    );
                }
                let authoritative_remaining =
                    legacy_remaining_work(&derive_remaining_work(&self.notebook.intended_changes));
                if declaration.remaining_work != authoritative_remaining {
                    self.append_event_recoverable(
                        "progress",
                        json!({
                            "event_type": "worker.remaining_work_reconciled",
                            "declared_remaining_work": declaration.remaining_work,
                            "authoritative_remaining_work": authoritative_remaining,
                            "reason": "remaining_work_is_derived_from_target_state",
                        }),
                        "remaining work reconciliation",
                    );
                }
                self.declaration = Some(declaration);
                self.transition_phase(
                    ExecutionPhase::CompletionEvaluation,
                    "complete diff reviewed and implementation declared",
                )?;
                Ok("recorded implementation declaration".into())
            }
            _ => bail!("unsupported hosted model tool `{name}`"),
        }
    }

    fn validate_tool_for_phase(
        &self,
        name: &str,
        arguments: &serde_json::Map<String, Value>,
    ) -> Result<()> {
        let phase = self.phases.active();
        if !phase_permits_tool(phase, name) {
            bail!(
                "tool `{name}` is not permitted during phase `{}`",
                phase.as_str()
            );
        }
        if name == "search_text"
            && phase != ExecutionPhase::Discovery
            && arguments
                .get("path")
                .and_then(Value::as_str)
                .is_none_or(|path| matches!(path.trim_matches('/'), "" | "." | "src"))
        {
            bail!(
                "broad repository searches are not permitted during phase `{}`; target a planned path or concrete failure",
                phase.as_str()
            );
        }
        if matches!(
            name,
            "read_file" | "read_files" | "search_text" | "related_tests"
        ) && required_tool_string(arguments, "reason", 2_000)?
            .trim()
            .is_empty()
        {
            bail!("targeted repository inspection requires a concrete reason");
        }
        if phase == ExecutionPhase::Discovery
            && matches!(
                name,
                "list_files" | "read_file" | "read_files" | "search_text" | "related_tests"
            )
        {
            let requested_paths = discovery_requested_paths(name, arguments);
            validate_localized_discovery_scope(&self.notebook, &requested_paths)?;
        }
        if name == "run_focused_command"
            && matches!(
                phase,
                ExecutionPhase::Implementation | ExecutionPhase::Repair
            )
            && self.tool_usage.successful_writes == 0
            && completion_changed_paths(self.repo, &self.manifest.github.base_sha)?.is_empty()
        {
            bail!(
                "validation_forbidden_no_implementation_changes: apply at least one planned target before focused validation"
            );
        }
        if matches!(
            phase,
            ExecutionPhase::Implementation | ExecutionPhase::Repair
        ) {
            let paths = match name {
                "read_file" => arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .into_iter()
                    .collect::<Vec<_>>(),
                "read_files" | "related_tests" => arguments
                    .get("paths")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>(),
                "search_text" => arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .into_iter()
                    .collect::<Vec<_>>(),
                _ if is_source_mutation_tool(name) => arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .into_iter()
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            if !paths.is_empty() && paths.iter().any(|path| !self.path_is_targeted(path)) {
                bail!(
                    "implementation and repair reads must target a planned edit, mapped criterion, or failed write"
                );
            }
            let current_target = self.current_implementation_target();
            validate_current_target_scope(
                current_target.as_ref(),
                self.guided_first_write_recovery_issued,
                self.tool_usage.successful_writes,
                &paths,
                is_source_mutation_tool(name),
            )?;
        }
        Ok(())
    }

    fn path_is_targeted(&self, path: &str) -> bool {
        let path = path.trim_matches('/');
        let related = |candidate: &str| {
            let candidate = candidate.trim_matches('/');
            path == candidate
                || candidate.starts_with(&format!("{path}/"))
                || path.starts_with(&format!("{candidate}/"))
        };
        self.implementation_plan.as_ref().is_some_and(|plan| {
            plan.planned_changes
                .iter()
                .flat_map(|change| &change.targets)
                .any(|target| related(&target.path))
                || plan.planned_new_files.iter().any(|file| related(file))
                || plan.planned_test_changes.iter().any(|file| related(file))
        }) || self.impact_map.as_ref().is_some_and(|map| {
            map.areas
                .iter()
                .flat_map(|area| &area.candidate_paths)
                .any(|candidate| related(candidate))
        }) || self
            .tool_failures
            .iter()
            .filter_map(|failure| failure.target.as_deref())
            .any(related)
    }
}

fn phase_permits_tool(phase: ExecutionPhase, name: &str) -> bool {
    match phase {
        ExecutionPhase::Discovery => matches!(
            name,
            "list_files"
                | "read_file"
                | "read_files"
                | "search_text"
                | "related_tests"
                | "record_impact_map"
        ),
        ExecutionPhase::ArtifactRepair => name == "record_impact_map",
        ExecutionPhase::Planning => matches!(
            name,
            "read_file"
                | "read_files"
                | "search_text"
                | "related_tests"
                | "record_implementation_plan"
        ),
        ExecutionPhase::Implementation | ExecutionPhase::Repair => matches!(
            name,
            "read_file"
                | "read_files"
                | "search_text"
                | "related_tests"
                | "write_file"
                | "replace_text"
                | "replace_range"
                | "insert_after_symbol"
                | "insert_before_symbol"
                | "apply_unified_diff"
                | "rewrite_small_file"
                | "delete_file"
                | "run_focused_command"
                | "report_write_progress"
        ),
        ExecutionPhase::DiffReview => matches!(
            name,
            "read_file"
                | "read_files"
                | "search_text"
                | "related_tests"
                | "write_file"
                | "replace_text"
                | "replace_range"
                | "insert_after_symbol"
                | "insert_before_symbol"
                | "apply_unified_diff"
                | "rewrite_small_file"
                | "delete_file"
                | "run_focused_command"
                | "repository_snapshot"
                | "declare_implementation"
        ),
        ExecutionPhase::CompletionEvaluation
        | ExecutionPhase::Validation
        | ExecutionPhase::Publication => false,
    }
}

fn legal_phase_transition(from: ExecutionPhase, to: ExecutionPhase) -> bool {
    matches!(
        (from, to),
        (ExecutionPhase::Discovery, ExecutionPhase::ArtifactRepair)
            | (ExecutionPhase::Discovery, ExecutionPhase::Planning)
            | (ExecutionPhase::ArtifactRepair, ExecutionPhase::Planning)
            | (ExecutionPhase::Planning, ExecutionPhase::Implementation)
            | (ExecutionPhase::Implementation, ExecutionPhase::Repair)
            | (ExecutionPhase::Implementation, ExecutionPhase::Validation)
            | (ExecutionPhase::Repair, ExecutionPhase::Validation)
            | (ExecutionPhase::Validation, ExecutionPhase::Repair)
            | (ExecutionPhase::Validation, ExecutionPhase::DiffReview)
            | (
                ExecutionPhase::Validation,
                ExecutionPhase::CompletionEvaluation
            )
            | (
                ExecutionPhase::DiffReview,
                ExecutionPhase::CompletionEvaluation
            )
            | (
                ExecutionPhase::CompletionEvaluation,
                ExecutionPhase::Publication
            )
    )
}

fn hosted_tools() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "name": "record_impact_map",
            "description": "Record semantic area mappings. The orchestrator expands evidence references and attaches canonical v2 wrapper fields.",
            "parameters": impact_map::provider_tool_schema(),
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "record_implementation_plan",
            "description": "End planning with a machine-readable mapping from acceptance criteria to edits and tests.",
            "parameters": {
                "type": "object",
                "properties": {
                    "implementation_status": {"type": "string", "enum": ["ready", "blocked"]},
                    "planned_changes": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "change_id": {"type": "string"},
                                "parent_change_id": {"type": ["string", "null"]},
                                "intent": {"type": "string"},
                                "reason": {"type": "string"},
                                "targets": {
                                    "type": "array",
                                    "minItems": 1,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "path": {"type": "string"},
                                            "role": {"type": "string"},
                                            "new_file": {"type": "boolean"},
                                            "status": {"type": "string", "enum": ["planned", "in_progress", "applied", "verified", "partial", "unresolved"]}
                                        },
                                        "required": ["path", "role", "new_file", "status"],
                                        "additionalProperties": false
                                    }
                                },
                                "status": {"type": "string", "enum": ["planned", "in_progress", "applied", "verified", "partial", "unresolved"]},
                                "acceptance_criteria": {"type": "array", "items": {"type": "string"}},
                                "test_coverage": {"type": "array", "items": {"type": "string"}}
                            },
                            "required": ["change_id", "parent_change_id", "intent", "reason", "targets", "status", "acceptance_criteria", "test_coverage"],
                            "additionalProperties": false
                        }
                    },
                    "planned_new_files": {"type": "array", "items": {"type": "string"}},
                    "planned_test_changes": {"type": "array", "items": {"type": "string"}},
                    "remaining_unknowns": {"type": "array", "items": {"type": "string"}},
                    "blocking_unknowns": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["implementation_status", "planned_changes", "planned_new_files", "planned_test_changes", "remaining_unknowns", "blocking_unknowns"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "list_files",
            "description": "List bounded repository-relative files. Use null for the repository root.",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": ["string", "null"]}},
                "required": ["path"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "read_file",
            "description": "Read a bounded line range from one UTF-8 repository file. Use null line bounds for defaults.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "start_line": {"type": ["integer", "null"], "minimum": 1},
                    "end_line": {"type": ["integer", "null"], "minimum": 1},
                    "reason": {"type": "string"}
                },
                "required": ["path", "start_line", "end_line", "reason"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "read_files",
            "description": "Read up to 20 selected UTF-8 repository files with independent per-file results and deterministic individual fallback for failures.",
            "parameters": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": 20
                    },
                    "maximum_lines_per_file": {"type": ["integer", "null"], "minimum": 1, "maximum": 1000},
                    "reason": {"type": "string"}
                },
                "required": ["paths", "maximum_lines_per_file", "reason"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "search_text",
            "description": "Search UTF-8 repository files with grouped, deduplicated results. Broad searches are discovery-only.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "path": {"type": ["string", "null"]},
                    "extensions": {"type": "array", "items": {"type": "string"}, "maxItems": 20},
                    "mode": {"type": "string", "enum": ["literal"]},
                    "context_lines": {"type": "integer", "minimum": 0, "maximum": 5},
                    "reason": {"type": "string"}
                },
                "required": ["query", "path", "extensions", "mode", "context_lines", "reason"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "related_tests",
            "description": "Find concise candidate test and spec paths related to selected source paths.",
            "parameters": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": 20
                    },
                    "reason": {"type": "string"}
                },
                "required": ["paths", "reason"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "write_file",
            "description": "Create or replace one UTF-8 repository file with complete content.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["change_id", "path", "content"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "replace_text",
            "description": "Edit one existing UTF-8 repository file by replacing one exact, unique string. Use this for targeted edits instead of mutation commands.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"}
                },
                "required": ["change_id", "path", "old_text", "new_text"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "replace_range",
            "description": "Replace an inclusive one-based line range in one UTF-8 file. Prefer this after an exact replacement is ambiguous.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1},
                    "new_text": {"type": "string"}
                },
                "required": ["change_id", "path", "start_line", "end_line", "new_text"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "insert_after_symbol",
            "description": "Insert UTF-8 content immediately after one exact unique symbol.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "symbol": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["change_id", "path", "symbol", "content"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "insert_before_symbol",
            "description": "Insert UTF-8 content immediately before one exact unique symbol.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "symbol": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["change_id", "path", "symbol", "content"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "apply_unified_diff",
            "description": "Apply one bounded unified diff that modifies only the declared repository path.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "patch": {"type": "string"}
                },
                "required": ["change_id", "path", "patch"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "rewrite_small_file",
            "description": "Deterministically replace the complete contents of an existing UTF-8 file no larger than 64 KiB. Prefer this for small test files after repeated ambiguous edits.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["change_id", "path", "content"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "delete_file",
            "description": "Delete one regular repository file.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["change_id", "path"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "run_focused_command",
            "description": "Run one focused validation or read-only diagnostic program directly, without a shell. Shell operators, pipelines, redirects, heredocs, and command chaining are unsupported. Use repository edit tools for mutations. The environment contains no RustGrid, GitHub, or OpenAI credential.",
            "parameters": {
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "report_write_progress",
            "description": "At the implementation-progress threshold, report the precise blocker or the next planned write instead of continuing exploration.",
            "parameters": {
                "type": "object",
                "properties": {
                    "status": {"type": "string", "enum": ["blocked", "ready_to_write", "no_change_required"]},
                    "reason": {"type": "string"},
                    "next_write": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "operation": {"type": "string"}
                        },
                        "required": ["path", "operation"],
                        "additionalProperties": false
                    }
                },
                "required": ["status", "reason", "next_write"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "repository_snapshot",
            "description": "Inspect git status, changed paths, diff statistics, and every page of the immutable complete diff before declaring implementation status. Start with cursor 0 and follow next_cursor until review_complete is true.",
            "parameters": {
                "type": "object",
                "properties": {
                    "cursor": {"type": ["integer", "null"], "minimum": 0}
                },
                "required": ["cursor"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "declare_implementation",
            "description": "After reviewing repository status and the complete diff, declare whether implementation is complete, partial, or blocked.",
            "parameters": {
                "type": "object",
                "properties": {
                    "implementation_status": {"type": "string", "enum": ["complete", "partial", "blocked"]},
                    "completed_work": {"type": "array", "items": {"type": "string"}},
                    "remaining_work": {"type": "array", "items": {"type": "string"}},
                    "known_risks": {"type": "array", "items": {"type": "string"}},
                    "changed_paths": {"type": "array", "items": {"type": "string"}},
                    "criteria_evidence": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "criterion": {"type": "string"},
                                "paths": {"type": "array", "items": {"type": "string"}},
                                "evidence": {"type": "string"}
                            },
                            "required": ["criterion", "paths", "evidence"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["implementation_status", "completed_work", "remaining_work", "known_risks", "changed_paths", "criteria_evidence"],
                "additionalProperties": false
            },
            "strict": true
        }),
    ]
}

fn hosted_tools_for_phase(phase: ExecutionPhase) -> Vec<Value> {
    hosted_tools()
        .into_iter()
        .filter(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| phase_permits_tool(phase, name))
        })
        .collect()
}

fn compact_impact_map_repair_context(
    failure: Option<&ImpactMapFailure>,
    notebook: &WorkerNotebook,
) -> String {
    let criteria = notebook
        .acceptance_criteria_v2
        .iter()
        .map(|criterion| json!({"id":criterion.id,"text":truncate_text(&criterion.text, 500)}))
        .collect::<Vec<_>>();
    let evidence = &notebook.impact_evidence;
    let context = json!({
        "instruction":"The previous impact map was semantically useful but failed validation. Correct only the invalid structural portions. Do not perform repository discovery. Call record_impact_map exactly once.",
        "invalid_artifact":failure.map(|failure| &failure.invalid_payload),
        "validation_errors":failure.map(|failure| &failure.errors).unwrap_or(&Vec::new()),
        "canonical_schema":impact_map::schema(),
        "allowed_model_fields":["areas","name","candidate_paths","evidence_refs","acceptance_criteria_ids","reason"],
        "evidence":evidence,
        "acceptance_criteria":criteria,
        "minimal_valid_model_input":{"areas":[{"name":"Affected surface","candidate_paths":["src/example.rs"],"evidence_refs":["read-1"],"acceptance_criteria_ids":["ac-1"],"reason":"Implements the criterion."}]},
        "tool_schema_version":IMPACT_MAP_SCHEMA_VERSION,
        "tool_schema_sha256":impact_map::schema_sha256(),
    });
    truncate_text(&serde_json::to_string(&context).unwrap_or_default(), 19_000)
}

fn artifact_call_accounting(phase: ExecutionPhase) -> Value {
    let supplemental = phase == ExecutionPhase::ArtifactRepair;
    json!({
        "provider_call_occurred": true,
        "configured_mission_budget_consumed": !supplemental,
        "supplemental_repair_budget_consumed": supplemental,
    })
}

fn hosted_agent_instructions(phase: ExecutionPhase) -> String {
    if phase == ExecutionPhase::ArtifactRepair {
        return "You are repairing the structured implementation impact map for an ephemeral \
RustGrid mission. Repository discovery from the previous phase is preserved in the worker \
notebook. Do not repeat reads or searches. Use only record_impact_map, reconstructing a strict \
impact map from the inspected files, searches, architecture findings, candidate paths, and \
acceptance criteria already present. Do not edit source files or perform additional exploration."
            .into();
    }
    format!(
        "You are the implementation model inside an ephemeral RustGrid GitHub Actions worker. \
The active hard execution phase is `{}`. Use only tools admitted for that phase and transition as \
soon as its required structured artifact is complete. Discovery must end with record_impact_map; \
planning must end with record_implementation_plan; implementation must make planned edits rather \
than restart broad exploration; diff review must use repository_snapshot and \
declare_implementation. For localized theme or visual-system work, discovery should identify the \
theme provider, selector, centralized token source, focused tests, and package validation commands. \
Once centralized semantic variables are confirmed, sample at most three representative consumers \
and stop discovery unless a targeted search proves a direct hardcoded-color exception. Keep each \
discovery request below roughly 12,000 input tokens where possible. \
Use only the provided repository tools. Inspect the smallest relevant scope, follow repository \
instructions, and record a repository-level impact map before editing. Batch repository discovery \
within each response instead of spending one model call per file. Implement the mission, add focused \
tests, and inspect repository status plus the complete diff before finishing. Use \
replace_text for the first targeted edit. After an ambiguous replacement, perform one bounded \
read_file; after a second ambiguity, switch to replace_range, a unique-symbol insertion, \
apply_unified_diff, or rewrite_small_file for a small file. write_file is appropriate only when \
replacing a complete file. Every source-changing call must cite the stable change_id from the plan. \
Prefer one planned change per independently editable file; use parent_change_id only to group \
related file changes. When one logical change genuinely has multiple targets, represent them as \
structured target objects and mutate one concrete member path per tool call. Never encode multiple \
paths in one string. A mutation authorization or plan-metadata rejection is not a content-edit \
failure: do not switch editing tools; allow the orchestrator to repair metadata deterministically. \
run_focused_command starts one executable directly without a shell; never pass shell operators, \
pipelines, redirects, heredocs, or chained commands to it, and never use it to mutate files. Never \
commit, push, switch branches, modify Git remotes, open pull requests, read environment variables, \
read files outside the repository, or attempt to discover credentials. The RustGrid worker owns \
full quality gates and publication. Call declare_implementation after diff review, then end with a \
concise implementation and focused-validation summary. Never declare complete while planned work, \
acceptance criteria, or a genuinely unresolved intended change remains. Failed tool attempts are \
diagnostic history and do not invalidate a later verified intended change.",
        phase.as_str()
    )
}

fn build_hosted_prompt(
    manifest: &HostedManifest,
    repo: &Repo,
    partial_run: Option<&PartialRunContext>,
) -> Result<String> {
    let files = collect_repo_files(&repo.root, &repo.root, 1_200)?;
    let continuation_guidance = partial_implementation_guidance(partial_run);
    let instructions = read_repo_instructions(&repo.root)?
        .into_iter()
        .map(|(name, content)| {
            format!(
                "\n\nRepository instruction file {name}:\n{}",
                truncate_text(&content, 24_000)
            )
        })
        .collect::<String>();
    let visual_guidance = visual_impact_guidance(&format!(
        "{}\n{}",
        manifest.ticket_title, manifest.run.input_prompt
    ));
    Ok(format!(
        "Implement RustGrid ticket {key}: {title}\n\nMission instructions:\n{prompt}\n\n\
Execution attempt: {attempt}\nDeterministic branch: {branch}\nResolved model: {model}\n\
Maximum model calls: {calls}\nMaximum cost USD: {cost}{visual_guidance}{continuation_guidance}\n\nRepository files:\n{files}{instructions}",
        key = manifest.ticket_key,
        title = manifest.ticket_title,
        prompt = manifest.run.input_prompt,
        attempt = manifest.execution.attempt_number,
        branch = manifest.github.branch,
        model = manifest.ai_gateway.model,
        calls = manifest.ai_gateway.maximum_model_calls,
        cost = manifest.ai_gateway.maximum_cost_usd,
        visual_guidance = visual_guidance,
        files = files.join("\n"),
        continuation_guidance = continuation_guidance,
    ))
}

fn partial_implementation_guidance(partial_run: Option<&PartialRunContext>) -> String {
    let Some(partial_run) = partial_run else {
        return String::new();
    };
    let remaining_work = if partial_run.remaining_work.is_empty() {
        "- Reconcile the preserved diff against every acceptance criterion.".to_owned()
    } else {
        partial_run
            .remaining_work
            .iter()
            .map(|work| format!("- {work}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "\n\nExisting partial implementation detected in draft pull request #{pull_request_number} \
on the deterministic branch.\nChanged paths relative to the mission base:\n{changed_paths}\n\n\
Previously reported remaining work:\n{remaining_work}\n\n\
Before planning or editing, inspect these paths and compare the existing implementation \
with every mission acceptance criterion. Preserve correct completed work, identify what is \
partial or missing, and continue from the current branch state. Do not restart, duplicate, \
or overwrite valid work merely because a worker notebook is unavailable or stale. Treat \
changed paths as evidence of prior work, not proof that the mission is complete.",
        pull_request_number = partial_run.pull_request_number,
        changed_paths = partial_run.changed_paths.join("\n"),
    )
}

fn visual_impact_guidance(ticket: &str) -> &'static str {
    let ticket = ticket.to_ascii_lowercase();
    if [
        "theme",
        "dark mode",
        "light mode",
        "design system",
        "color palette",
        "visual system",
    ]
    .iter()
    .any(|needle| ticket.contains(needle))
    {
        "\n\nVisual-system discovery scope: identify the theme provider, selector, centralized \
design-token or CSS-variable source, existing focused tests, and package validation commands. \
If centralized semantic variables are confirmed, inspect at most three representative consumers. \
Do not enumerate every shared UI component unless a targeted search reveals direct hardcoded \
colors. Record the compact impact map as soon as those boundaries are established."
    } else {
        ""
    }
}

fn bootstrap_hosted_dependencies(
    api: &HostedApiClient,
    manifest: &HostedManifest,
    repo: &Repo,
    running: &Arc<AtomicBool>,
    containment: &command::HostedProcessContainment,
) -> Result<()> {
    let Some((manager, command_text)) = hosted_dependency_bootstrap(&repo.root) else {
        return Ok(());
    };
    api.append_event(
        "progress",
        json!({
            "step": "dependency_bootstrap",
            "status": "running",
            "manager": manager,
            "command": command_text
        }),
    )?;
    let allowlist = manifest.execution_policy.child_environment_allowlist();
    let output = command::capture_hosted_cancellable_with_environment(
        command_text,
        &repo.root,
        running,
        Duration::from_secs(
            u64::try_from(manifest.execution_policy.timeout_seconds)
                .unwrap_or(1)
                .min(1_800),
        ),
        2 * 1024 * 1024,
        Some(&allowlist),
        None,
        containment,
    )?;
    if !output.status.success() {
        bail!(
            "locked {manager} dependency bootstrap failed: {}",
            truncate_text(&format!("{}\n{}", output.stdout, output.stderr), 8_000)
        );
    }
    api.append_event(
        "progress",
        json!({
            "step": "dependency_bootstrap",
            "status": "completed",
            "manager": manager
        }),
    )?;
    Ok(())
}

fn hosted_dependency_bootstrap(root: &Path) -> Option<(&'static str, &'static str)> {
    if !root.join("package.json").is_file() {
        return None;
    }
    if root.join("pnpm-lock.yaml").is_file() {
        Some((
            "pnpm",
            "pnpm install --frozen-lockfile --prefer-offline --ignore-scripts",
        ))
    } else if root.join("yarn.lock").is_file() {
        Some((
            "yarn",
            "yarn install --frozen-lockfile --prefer-offline --ignore-scripts",
        ))
    } else if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
        Some(("bun", "bun install --frozen-lockfile --ignore-scripts"))
    } else if root.join("package-lock.json").is_file() || root.join("npm-shrinkwrap.json").is_file()
    {
        Some((
            "npm",
            "npm ci --ignore-scripts --no-audit --no-fund --prefer-offline",
        ))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn run_quality_gates(
    api: &HostedApiClient,
    manifest: &HostedManifest,
    repo: &Repo,
    running: &Arc<AtomicBool>,
    policy: &HostedExecutionPolicy,
    containment: &command::HostedProcessContainment,
    validation_round: u32,
    ledger: &mut Vec<ValidationEvidence>,
    required_gates: &mut Vec<RequiredGate>,
    usage: &mut ToolUsage,
    validation_started_at: Instant,
    execution_started_at: Instant,
) -> Result<Vec<ValidationResult>> {
    run_quality_gates_with_capture(
        api,
        manifest,
        repo,
        running,
        policy,
        validation_round,
        ledger,
        required_gates,
        usage,
        validation_started_at,
        execution_started_at,
        |command_text, cwd, running, timeout, max_output_bytes, environment_allowlist, limits| {
            command::capture_hosted_cancellable_with_environment(
                command_text,
                cwd,
                running,
                timeout,
                max_output_bytes,
                environment_allowlist,
                limits,
                containment,
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn run_quality_gates_with_capture<F>(
    api: &HostedApiClient,
    manifest: &HostedManifest,
    repo: &Repo,
    running: &Arc<AtomicBool>,
    policy: &HostedExecutionPolicy,
    validation_round: u32,
    ledger: &mut Vec<ValidationEvidence>,
    required_gates: &mut Vec<RequiredGate>,
    usage: &mut ToolUsage,
    validation_started_at: Instant,
    execution_started_at: Instant,
    capture: F,
) -> Result<Vec<ValidationResult>>
where
    F: Fn(
        &str,
        &Path,
        &AtomicBool,
        Duration,
        usize,
        Option<&[String]>,
        Option<command::ChildLimits>,
    ) -> Result<command::CommandOutput>,
{
    let allowlist = policy.child_environment_allowlist();
    let workflow_run_attempt = manifest
        .execution
        .github_actions
        .as_ref()
        .and_then(|execution| execution.workflow_run_attempt)
        .context("validated manifest has no GitHub workflow run attempt")?;
    let mut results = Vec::new();
    let mut ordered_gates = policy
        .quality_gates
        .iter()
        .filter(|gate| gate.required)
        .collect::<Vec<_>>();
    ordered_gates.sort_by_key(|gate| {
        usize::from(
            classify_validation_gate(&gate.id, &gate.command) != ValidationGateType::FocusedTest,
        )
    });
    for gate in ordered_gates {
        ensure_running(running)?;
        let validation_remaining = Duration::from_secs(8 * 60)
            .checked_sub(validation_started_at.elapsed())
            .filter(|remaining| !remaining.is_zero());
        let execution_remaining = MAX_HOSTED_EXECUTION_DURATION
            .checked_sub(execution_started_at.elapsed())
            .filter(|remaining| !remaining.is_zero());
        let Some(validation_remaining) = validation_remaining
            .zip(execution_remaining)
            .map(|(validation, execution)| validation.min(execution))
        else {
            api.append_event(
                "validation",
                json!({
                    "event_type": "worker.wall_clock_guard_triggered",
                    "phase": "validation",
                    "gate_id": gate.id,
                    "validation_limit_seconds": 8 * 60,
                    "execution_limit_seconds": MAX_HOSTED_EXECUTION_DURATION.as_secs(),
                    "status": "timed_out",
                }),
            )?;
            results.push(ValidationResult {
                id: gate.id.clone(),
                command: gate.command.clone(),
                status: "failed".into(),
                output: "Validation phase reached its 8 minute wall-clock limit.".into(),
            });
            break;
        };
        let source_tree_hash = repository_state_fingerprint(repo, &manifest.github.base_sha)?;
        supersede_stale_validation(ledger, &source_tree_hash);
        let dependency_lock_hash = dependency_lock_fingerprint(&repo.root)?;
        let environment_fingerprint = relevant_environment_fingerprint(policy)?;
        let fingerprint = validation_fingerprint(
            &gate.command,
            repo.root.to_string_lossy().as_ref(),
            &source_tree_hash,
            &dependency_lock_hash,
            &environment_fingerprint,
        );
        let gate_type = classify_validation_gate(&gate.id, &gate.command);
        if let Some(evidence) = passed_evidence(ledger, &fingerprint).cloned() {
            usage.deduplicated_validations = usage.deduplicated_validations.saturating_add(1);
            api.append_event(
                "validation",
                json!({
                    "event_type": "worker.validation_deduplicated",
                    "gate_id": gate.id,
                    "evidence_id": evidence.evidence_id,
                    "command_fingerprint": fingerprint,
                    "source_tree_hash": source_tree_hash,
                    "status": "passed",
                    "source": ValidationSource::ResumeReused,
                }),
            )?;
            required_gates.retain(|required| required.gate_id != gate.id);
            required_gates.push(RequiredGate {
                gate_id: gate.id.clone(),
                gate_type,
                required: true,
                command: gate.command.clone(),
                status: ValidationStatus::Passed,
                evidence_id: Some(evidence.evidence_id.clone()),
            });
            results.push(ValidationResult {
                id: gate.id.clone(),
                command: gate.command.clone(),
                status: "passed".into(),
                output: format!(
                    "Reused validation evidence {} for unchanged source tree {}.",
                    evidence.evidence_id, source_tree_hash
                ),
            });
            continue;
        }
        if let Some(evidence) = ledger
            .iter()
            .rev()
            .find(|evidence| {
                evidence.command_fingerprint == fingerprint
                    && matches!(
                        evidence.status,
                        ValidationStatus::Failed
                            | ValidationStatus::TimedOut
                            | ValidationStatus::Cancelled
                    )
            })
            .cloned()
        {
            usage.deduplicated_validations = usage.deduplicated_validations.saturating_add(1);
            api.append_event(
                "validation",
                json!({
                    "event_type": "worker.validation_deduplicated",
                    "gate_id": gate.id,
                    "evidence_id": evidence.evidence_id,
                    "command_fingerprint": fingerprint,
                    "source_tree_hash": source_tree_hash,
                    "status": evidence.status,
                    "reason": "failed_gate_requires_a_relevant_source_or_dependency_change_before_retry",
                }),
            )?;
            required_gates.retain(|required| required.gate_id != gate.id);
            required_gates.push(RequiredGate {
                gate_id: gate.id.clone(),
                gate_type,
                required: true,
                command: gate.command.clone(),
                status: evidence.status,
                evidence_id: Some(evidence.evidence_id.clone()),
            });
            results.push(ValidationResult {
                id: gate.id.clone(),
                command: gate.command.clone(),
                status: "failed".into(),
                output: format!(
                    "Gate was not rerun because source and dependency state are unchanged; see evidence {}.",
                    evidence.evidence_id
                ),
            });
            if gate_type == ValidationGateType::FocusedTest {
                break;
            }
            continue;
        }
        usage.required_validations = usage.required_validations.saturating_add(1);
        usage.validation_commands = usage.validation_commands.saturating_add(1);
        let phase_started_at = now_rfc3339();
        send_quality_gate_phase_telemetry(
            api,
            manifest.execution.execution_id,
            gate,
            workflow_run_attempt,
            validation_round,
            &phase_started_at,
            None,
            ExecutionStatus::Running,
            1,
        )
        .with_context(|| format!("could not start telemetry for quality gate {}", gate.id))?;
        api.append_event(
            "validation",
            json!({
                "gate_id": gate.id,
                "command": gate.command,
                "status": "running"
            }),
        )?;
        let started = Instant::now();
        let evidence_id = format!("{}-{}", gate.id, &fingerprint[..12]);
        ledger.push(new_running_evidence(
            evidence_id.clone(),
            gate.id.clone(),
            gate_type,
            gate.command.clone(),
            fingerprint.clone(),
            source_tree_hash.clone(),
            dependency_lock_hash,
            ValidationSource::WorkerRequired,
        ));
        let output = capture(
            &gate.command,
            &repo.root,
            running,
            Duration::from_secs(u64::try_from(gate.timeout_seconds).unwrap_or(1))
                .min(validation_remaining),
            2 * 1024 * 1024,
            Some(&allowlist),
            None,
        );
        let (result, evidence_status, exit_code, stdout, stderr) = match output {
            Ok(output) => {
                let combined = format!("{}\n{}", output.stdout, output.stderr);
                let passed = output.status.success();
                (
                    ValidationResult {
                        id: gate.id.clone(),
                        command: gate.command.clone(),
                        status: if passed {
                            "passed".into()
                        } else {
                            "failed".into()
                        },
                        output: truncate_text(&combined, 16_000),
                    },
                    if passed {
                        ValidationStatus::Passed
                    } else {
                        ValidationStatus::Failed
                    },
                    output.status.code(),
                    output.stdout,
                    output.stderr,
                )
            }
            Err(error) => {
                let message = truncate_text(&format!("{error:#}"), 16_000);
                (
                    ValidationResult {
                        id: gate.id.clone(),
                        command: gate.command.clone(),
                        status: "failed".into(),
                        output: message.clone(),
                    },
                    if !running.load(Ordering::SeqCst) || shutdown::requested() {
                        ValidationStatus::Cancelled
                    } else if error.to_string().to_ascii_lowercase().contains("timed out") {
                        ValidationStatus::TimedOut
                    } else {
                        ValidationStatus::Failed
                    },
                    None,
                    String::new(),
                    message,
                )
            }
        };
        if let Some(evidence) = ledger
            .iter_mut()
            .rev()
            .find(|evidence| evidence.evidence_id == evidence_id)
        {
            evidence.completed_at = Some(now_rfc3339());
            evidence.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            evidence.exit_code = exit_code;
            evidence.status = evidence_status;
            evidence.stdout_summary = truncate_text(&stdout, 8_000);
            evidence.stderr_summary = truncate_text(&stderr, 8_000);
        }
        required_gates.retain(|required| required.gate_id != gate.id);
        required_gates.push(RequiredGate {
            gate_id: gate.id.clone(),
            gate_type,
            required: true,
            command: gate.command.clone(),
            status: evidence_status,
            evidence_id: Some(evidence_id.clone()),
        });
        let phase_completed_at = now_rfc3339();
        send_quality_gate_phase_telemetry(
            api,
            manifest.execution.execution_id,
            gate,
            workflow_run_attempt,
            validation_round,
            &phase_started_at,
            Some(&phase_completed_at),
            if result.status == "passed" {
                ExecutionStatus::Succeeded
            } else {
                ExecutionStatus::Failed
            },
            2,
        )
        .with_context(|| format!("could not complete telemetry for quality gate {}", gate.id))?;
        api.append_event(
            "validation",
            json!({
                "gate_id": result.id,
                "command": result.command,
                "status": result.status,
                "output": result.output,
                "execution_id": manifest.execution.execution_id
                ,"event_type": "worker.validation_completed"
                ,"evidence_id": evidence_id
                ,"command_fingerprint": fingerprint
                ,"source_tree_hash": source_tree_hash
            }),
        )?;
        let focused_gate_failed =
            gate_type == ValidationGateType::FocusedTest && result.status != "passed";
        results.push(result);
        if focused_gate_failed {
            break;
        }
        if !running.load(Ordering::SeqCst) || shutdown::requested() {
            break;
        }
    }
    Ok(results)
}

fn classify_validation_gate(id: &str, command: &str) -> ValidationGateType {
    let id = id.to_ascii_lowercase();
    let command = command.to_ascii_lowercase();
    let value = format!("{id} {command}");
    if id.contains("focused") || command.contains("test --") {
        ValidationGateType::FocusedTest
    } else if value.contains("build") {
        ValidationGateType::Build
    } else if value.contains("lint") || value.contains("fmt") {
        ValidationGateType::Lint
    } else if value.contains("typecheck") || value.contains("tsc") || value.contains("check") {
        ValidationGateType::Typecheck
    } else if value.contains("test") {
        ValidationGateType::TestSuite
    } else {
        ValidationGateType::Custom
    }
}

fn dependency_lock_fingerprint(root: &Path) -> Result<String> {
    let mut material = Vec::new();
    for name in [
        "Cargo.lock",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lockb",
    ] {
        let path = root.join(name);
        if path.is_file() {
            material.extend_from_slice(name.as_bytes());
            material.push(0);
            material.extend_from_slice(&fs::read(path)?);
            material.push(0);
        }
    }
    Ok(hex::encode(Sha256::digest(material)))
}

fn relevant_environment_fingerprint(policy: &HostedExecutionPolicy) -> Result<String> {
    let mut names = policy.child_environment_allowlist();
    names.sort();
    names.dedup();
    let mut material = serde_json::to_vec(&policy.sandbox)
        .context("could not fingerprint hosted sandbox policy")?;
    for name in names {
        append_fingerprint_field(&mut material, "environment_name", name.as_bytes());
        append_fingerprint_field(
            &mut material,
            "environment_value",
            env::var(&name).unwrap_or_default().as_bytes(),
        );
    }
    Ok(hex::encode(Sha256::digest(material)))
}

fn request_github_oidc(
    http: &Client,
    environment: &GithubActionsEnvironment,
) -> Result<SecretString> {
    let mut url = environment.oidc_request_url.clone();
    url.query_pairs_mut()
        .append_pair("audience", &environment.audience);
    let response = http
        .get(url)
        .bearer_auth(environment.oidc_request_token.expose())
        .header(header::ACCEPT, "application/json")
        .send()
        .context("GitHub OIDC token request failed")?;
    if !response.status().is_success() {
        bail!("GitHub OIDC token request returned {}", response.status());
    }
    let body: GithubOidcResponse = decode_success(response, "GitHub OIDC token response")?;
    validate_github_oidc_token(&body.value)?;
    SecretString::new(body.value, "GitHub OIDC token")
}

fn exchange_github_oidc(
    http: &Client,
    environment: &GithubActionsEnvironment,
    execution_id: Uuid,
    oidc_token: &SecretString,
) -> Result<ExchangeResponse> {
    let url = environment
        .api_root
        .join("execution-auth/github-actions/exchange")?;
    let response = http
        .post(url)
        .header(header::ACCEPT, "application/json")
        .json(&json!({
            "execution_id": execution_id,
            "dispatch_nonce": environment.dispatch_nonce.expose(),
            "github_oidc_token": oidc_token.expose()
        }))
        .send()
        .context("RustGrid GitHub OIDC exchange transport failed")?;
    decode_response(response, "execution-auth/github-actions/exchange")
}

fn hosted_http_client() -> Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
        .user_agent(concat!(
            "rustgrid-agent/",
            env!("CARGO_PKG_VERSION"),
            " github-actions"
        ))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("could not create hosted execution HTTP client")
}

fn decode_response<T: DeserializeOwned>(response: Response, path: &str) -> Result<T> {
    let status = response.status();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| safe_identifier(value, 200))
        .map(str::to_owned);
    if !status.is_success() {
        let bytes = read_bounded_response(response, MAX_HTTP_ERROR_BYTES)
            .with_context(|| format!("could not read bounded RustGrid {path} error response"))?;
        let error = serde_json::from_slice::<Value>(&bytes).ok();
        let code = error
            .as_ref()
            .and_then(|value| hosted_error_field(value, "code"))
            .and_then(Value::as_str)
            .filter(|value| safe_identifier(value, 100))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("http_{}", status.as_u16()));
        return Err(HostedHttpError {
            status,
            path: path.to_owned(),
            code,
            request_id,
            rustgrid_gateway_status: optional_hosted_http_status(
                error.as_ref(),
                "rustgrid_gateway_status",
            ),
            upstream_provider_status: error
                .as_ref()
                .and_then(|value| hosted_error_field(value, "upstream_provider_status"))
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .filter(|value| (100..=599).contains(value)),
            failure_stage: safe_hosted_error_identifier(error.as_ref(), "failure_stage"),
            provider_contacted: error
                .as_ref()
                .and_then(|value| hosted_error_field(value, "provider_contacted"))
                .and_then(Value::as_bool),
            call_budget_consumed: error
                .as_ref()
                .and_then(|value| hosted_error_field(value, "call_budget_consumed"))
                .and_then(Value::as_bool),
            reservation_state: safe_hosted_error_identifier(error.as_ref(), "reservation_state"),
            reservation_reconciliation_state: safe_hosted_error_identifier(
                error.as_ref(),
                "reservation_reconciliation_state",
            ),
            retryable: error
                .as_ref()
                .and_then(|value| hosted_error_field(value, "retryable"))
                .and_then(Value::as_bool),
            rustgrid_request_id: safe_hosted_error_identifier(
                error.as_ref(),
                "rustgrid_request_id",
            ),
            transport_request_id: safe_hosted_error_identifier(
                error.as_ref(),
                "transport_request_id",
            ),
            provider_request_id: safe_hosted_error_identifier(
                error.as_ref(),
                "provider_request_id",
            ),
            provider_error: safe_provider_error(error.as_ref()),
            provider_response_body: safe_provider_response_body(error.as_ref()),
            model_alias: safe_hosted_error_identifier(error.as_ref(), "model_alias"),
            resolved_provider_model: safe_hosted_error_identifier(
                error.as_ref(),
                "resolved_provider_model",
            ),
            adapter_version: safe_hosted_error_identifier(error.as_ref(), "adapter_version"),
            payload_schema_version: safe_hosted_error_identifier(
                error.as_ref(),
                "payload_schema_version",
            ),
            provider_attempts: error
                .as_ref()
                .and_then(|value| hosted_error_field(value, "provider_attempts"))
                .and_then(Value::as_u64)
                .filter(|value| *value <= 100),
            actual_cost_micros: error
                .as_ref()
                .and_then(|value| hosted_error_field(value, "actual_cost_micros"))
                .and_then(Value::as_u64),
        }
        .into());
    }
    let bytes = Zeroizing::new(
        read_bounded_response(response, MAX_HTTP_RESPONSE_BYTES)
            .with_context(|| format!("could not read RustGrid {path} response"))?,
    );
    serde_json::from_slice(&bytes)
        .with_context(|| format!("RustGrid {path} response did not match its contract"))
}

fn hosted_error_field<'a>(error: &'a Value, field: &str) -> Option<&'a Value> {
    error.get(field).or_else(|| {
        ["details", "diagnostics", "error"]
            .into_iter()
            .find_map(|container| error.get(container).and_then(|value| value.get(field)))
    })
}

fn optional_hosted_http_status(error: Option<&Value>, field: &str) -> Option<Option<u16>> {
    let value = error.and_then(|value| hosted_error_field(value, field))?;
    if value.is_null() {
        return Some(None);
    }
    value
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| (100..=599).contains(value))
        .map(Some)
}

fn safe_hosted_error_identifier(error: Option<&Value>, field: &str) -> Option<String> {
    error
        .and_then(|value| hosted_error_field(value, field))
        .and_then(Value::as_str)
        .filter(|value| safe_identifier(value, 100))
        .map(str::to_owned)
}

fn safe_hosted_error_text(value: Option<&Value>, maximum: usize) -> Option<String> {
    let value = value.and_then(Value::as_str)?;
    let sanitized = value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .collect::<String>();
    (!sanitized.is_empty()).then(|| truncate_text(&sanitized, maximum))
}

fn safe_provider_error(error: Option<&Value>) -> Option<ProviderErrorDiagnostic> {
    let provider_error = error
        .and_then(|value| hosted_error_field(value, "provider_error"))
        .and_then(Value::as_object)?;
    let diagnostic = ProviderErrorDiagnostic {
        error_type: provider_error
            .get("type")
            .and_then(Value::as_str)
            .filter(|value| safe_identifier(value, 200))
            .map(str::to_owned),
        code: provider_error
            .get("code")
            .and_then(Value::as_str)
            .filter(|value| safe_identifier(value, 200))
            .map(str::to_owned),
        message: safe_hosted_error_text(
            provider_error.get("message"),
            MAX_PROVIDER_ERROR_MESSAGE_BYTES,
        ),
        parameter: safe_hosted_error_text(
            provider_error.get("parameter"),
            MAX_PROVIDER_ERROR_PARAMETER_BYTES,
        ),
    };
    (diagnostic.error_type.is_some()
        || diagnostic.code.is_some()
        || diagnostic.message.is_some()
        || diagnostic.parameter.is_some())
    .then_some(diagnostic)
}

fn safe_provider_response_body(error: Option<&Value>) -> Option<Value> {
    let body = error
        .and_then(|value| value.get("details"))
        .and_then(|details| details.get("provider_response_body"))?;
    let encoded = serde_json::to_vec(body).ok()?;
    if encoded.len() <= MAX_PROVIDER_RESPONSE_BODY_BYTES {
        return Some(body.clone());
    }
    Some(json!({
        "truncated": true,
        "preview": truncate_text(
            &String::from_utf8_lossy(&encoded),
            MAX_PROVIDER_RESPONSE_BODY_BYTES,
        ),
    }))
}

fn decode_success<T: DeserializeOwned>(response: Response, label: &str) -> Result<T> {
    let bytes = Zeroizing::new(
        read_bounded_response(response, MAX_HTTP_RESPONSE_BYTES)
            .with_context(|| format!("could not read {label}"))?,
    );
    serde_json::from_slice(&bytes).with_context(|| format!("{label} is malformed"))
}

fn read_bounded_response(mut response: Response, maximum: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        bail!("HTTP response exceeds {maximum} bytes");
    }
    let mut bytes = Vec::with_capacity(maximum.min(64 * 1024));
    response
        .by_ref()
        .take((maximum + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        bail!("HTTP response exceeds {maximum} bytes");
    }
    Ok(bytes)
}

fn normalize_api_root(value: &str) -> Result<Url> {
    let mut url = secure_url("RUSTGRID_API_URL", value)?;
    if url.query().is_some() || url.fragment().is_some() {
        bail!("RUSTGRID_API_URL cannot contain a query or fragment");
    }
    let trimmed = url.path().trim_end_matches('/');
    let path = if trimmed.ends_with("/api/v1") || trimmed == "api/v1" {
        format!("{trimmed}/")
    } else if trimmed.is_empty() || trimmed == "/" {
        "/api/v1/".to_owned()
    } else {
        format!("{trimmed}/api/v1/")
    };
    url.set_path(&path);
    Ok(url)
}

fn secure_url(name: &str, value: &str) -> Result<Url> {
    let url = Url::parse(value).with_context(|| format!("{name} must be a URL"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("{name} must be credential-free HTTPS (or loopback HTTP for tests)");
    }
    Ok(url)
}

fn secure_github_oidc_url(name: &str, value: &str) -> Result<Url> {
    let url = secure_url(name, value)?;
    let host = url.host_str().unwrap_or_default();
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if !loopback
        && host != "actions.githubusercontent.com"
        && !host.ends_with(".actions.githubusercontent.com")
    {
        bail!("{name} must use GitHub's Actions token-service host");
    }
    if url.query_pairs().any(|(key, _)| key == "audience") {
        bail!("{name} cannot predeclare an OIDC audience");
    }
    Ok(url)
}

fn api_origin(url: &Url) -> Result<String> {
    let origin = url.origin().ascii_serialization();
    if origin == "null" {
        bail!("RUSTGRID_API_URL has no secure origin");
    }
    Ok(origin)
}

fn validate_manifest_endpoint(
    name: &str,
    value: &str,
    api_root: &Url,
    expected_relative: &str,
) -> Result<()> {
    let expected_path = format!("/api/v1/{expected_relative}");
    if value == expected_path {
        return Ok(());
    }
    let endpoint = Url::parse(value).with_context(|| {
        format!("execution manifest {name} must be a canonical relative or absolute URL")
    })?;
    if endpoint.origin() != api_root.origin()
        || endpoint.path() != expected_path
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        bail!("execution manifest {name} is outside the mission API scope");
    }
    Ok(())
}

fn required_env(name: &str) -> Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("{name} is required for GitHub Actions execution"))
}

#[cfg(target_os = "linux")]
fn harden_hosted_process() -> Result<()> {
    // Repository commands run as the same ephemeral runner user. Mark the
    // coordinator non-dumpable before any are launched so they cannot inspect
    // its environment, heap, or file descriptors through procfs/ptrace.
    // SAFETY: PR_SET_DUMPABLE takes one integer flag and no pointer arguments.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("could not isolate hosted coordinator credentials");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn harden_hosted_process() -> Result<()> {
    bail!("GitHub Actions hosted execution requires Linux process isolation")
}

fn optional_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn valid_github_actor(value: &str) -> bool {
    if value.is_empty() || value.len() > 100 || !value.is_ascii() {
        return false;
    }
    let login = value.strip_suffix("[bot]").unwrap_or(value);
    !login.is_empty()
        && login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn reject_inherited_provider_credentials() -> Result<()> {
    let forbidden = [
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "CHATGPT_TOKEN",
        "OPENAI_ORG_ID",
    ];
    if forbidden
        .iter()
        .any(|name| env::var_os(name).is_some_and(|value| !value.is_empty()))
    {
        bail!(
            "hosted execution refuses inherited OpenAI or ChatGPT credentials; use only the RustGrid AI gateway"
        );
    }
    Ok(())
}

fn validate_dispatch_nonce(value: &str) -> Result<()> {
    if !(32..=256).contains(&value.len())
        || !value.starts_with("rgdn_")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("RUSTGRID_DISPATCH_NONCE is malformed");
    }
    Ok(())
}

fn validate_github_oidc_token(value: &str) -> Result<()> {
    if !(64..=16 * 1024).contains(&value.len())
        || !value.is_ascii()
        || value.bytes().filter(|byte| *byte == b'.').count() != 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("GitHub returned a malformed OIDC token");
    }
    Ok(())
}

fn validate_execution_token(value: &str) -> Result<()> {
    if !(32..=512).contains(&value.len())
        || !value.starts_with("rge_")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("RustGrid returned a malformed execution token");
    }
    Ok(())
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    )
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(250_u64.saturating_mul(1_u64 << attempt.min(5)))
}

fn ai_request_timeout(execution_deadline: Option<Instant>) -> Result<Duration> {
    execution_deadline
        .map(|deadline| {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .context("hosted execution deadline was reached before the AI gateway request")
                .map(|remaining| remaining.min(Duration::from_secs(90)))
        })
        .transpose()
        .map(|timeout| timeout.unwrap_or(Duration::from_secs(90)))
}

fn sleep_before_execution_retry(
    execution_deadline: Option<Instant>,
    delay: Duration,
    operation: &str,
) -> Result<()> {
    if let Some(deadline) = execution_deadline {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| *remaining > delay)
            .with_context(|| {
                format!("hosted execution deadline was reached before the {operation} could start")
            })?;
        debug_assert!(remaining > delay);
    }
    thread::sleep(delay);
    Ok(())
}

fn sleep_before_ai_retry(execution_deadline: Option<Instant>, attempt: usize) -> Result<()> {
    sleep_before_execution_retry(execution_deadline, retry_delay(attempt), "AI gateway retry")
}

fn registration_retry_delay(attempt: usize, semantic_call_id: Uuid) -> Duration {
    let base_millis = [250_u64, 1_000, 3_000]
        .get(attempt)
        .copied()
        .unwrap_or(3_000);
    let bytes = semantic_call_id.as_bytes();
    let sample = u16::from_be_bytes([bytes[0], bytes[1]]);
    let jitter_percent = 80_u64 + u64::from(sample % 41);
    Duration::from_millis(base_millis.saturating_mul(jitter_percent) / 100)
}

fn token_refresh_after(expires_at: SystemTime) -> SystemTime {
    expires_at
        .checked_sub(TOKEN_REFRESH_MARGIN)
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn safe_identifier(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

fn safe_child_environment_name(name: &str) -> bool {
    let normalized = name.trim().to_ascii_uppercase();
    !normalized.is_empty()
        && normalized.len() <= 128
        && normalized
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !normalized.starts_with("RUSTGRID_")
        && !normalized.starts_with("GITHUB_")
        && !normalized.starts_with("ACTIONS_")
        && normalized != "SSH_AUTH_SOCK"
        && !normalized.contains("TOKEN")
        && !normalized.contains("SECRET")
        && !normalized.contains("PASSWORD")
        && !normalized.contains("CREDENTIAL")
        && !normalized.contains("PRIVATE_KEY")
        && !normalized.contains("API_KEY")
        && !matches!(
            normalized.as_str(),
            "SHELL"
                | "ENV"
                | "BASH_ENV"
                | "ZDOTDIR"
                | "CDPATH"
                | "IFS"
                | "PYTHONPATH"
                | "PYTHONHOME"
                | "NODE_OPTIONS"
                | "RUBYOPT"
                | "PERL5OPT"
                | "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "RUSTDOC_WRAPPER"
                | "GIT_EXEC_PATH"
        )
        && !normalized.starts_with("LD_")
        && !normalized.starts_with("DYLD_")
        && !normalized.starts_with("GIT_CONFIG")
}

fn normalized_base_ref(value: &str) -> Result<&str> {
    let value = value.strip_prefix("refs/heads/").unwrap_or(value);
    if value.is_empty() || value.len() > 255 {
        bail!("execution manifest base ref is invalid");
    }
    Ok(value)
}

fn safe_git_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with(['-', '/'])
        && !value.ends_with(['/', '.'])
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains("//")
        && !value.bytes().any(|byte| {
            byte.is_ascii_control()
                || byte.is_ascii_whitespace()
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
}

fn commit_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn ensure_running(running: &AtomicBool) -> Result<()> {
    if !running.load(Ordering::SeqCst) || shutdown::requested() {
        bail!("hosted execution was cancelled or lost its mission lease");
    }
    Ok(())
}

fn ensure_hosted_execution_deadline(started_at: Instant) -> Result<()> {
    if started_at.elapsed() >= MAX_HOSTED_EXECUTION_DURATION {
        bail!(
            "hosted_execution_wall_clock_exceeded: execution reached its {} second limit",
            MAX_HOSTED_EXECUTION_DURATION.as_secs()
        );
    }
    Ok(())
}

fn hosted_execution_deadline(started_at: Instant) -> Result<Instant> {
    started_at
        .checked_add(MAX_HOSTED_EXECUTION_DURATION)
        .context("hosted execution deadline could not be represented")
}

fn send_execution_telemetry(
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
            execution: ExecutionSnapshot {
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
fn send_quality_gate_phase_telemetry(
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
fn quality_gate_phase_event(
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

fn safe_failure(error: &anyhow::Error, cancelled: bool) -> (String, String) {
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
    if let Some(failure) = error.downcast_ref::<HostedProviderContractFailure>() {
        return (failure.code.clone(), failure.message.clone());
    }
    if error.downcast_ref::<ExecutionBudgetMismatch>().is_some() {
        return (
            "execution_budget_mismatch".into(),
            "The requested, resolved, and worker-received model-call budgets did not match.".into(),
        );
    }
    let text = error.to_string().to_ascii_lowercase();
    if text.contains("validation") {
        (
            "validation_failed".into(),
            "One or more required repository validation commands failed.".into(),
        )
    } else if text.contains("pull request") {
        (
            "pull_request_creation_failed".into(),
            "The hosted execution could not create or verify its pull request.".into(),
        )
    } else if text.contains("model-call budget") || text.contains("budget") {
        (
            "execution_ai_budget_exceeded".into(),
            "The hosted execution exhausted its configured AI budget.".into(),
        )
    } else {
        (
            "hosted_agent_execution_failed".into(),
            "The ephemeral RustGrid agent failed before completing the mission.".into(),
        )
    }
}

fn failure_diagnostics(error: &anyhow::Error, cancelled: bool) -> Value {
    if let Some(failure) = error.downcast_ref::<HostedAgentExecutionFailure>() {
        return serde_json::to_value(failure).unwrap_or_else(|_| {
            json!({
                "status": "failed",
                "category": "hosted_agent_execution_failed",
                "code": failure.code,
                "phase": failure.phase,
                "message": failure.message,
            })
        });
    }
    if let Some(failure) = error.downcast_ref::<HostedProviderContractFailure>() {
        return json!({
            "status": "failed",
            "category": "hosted_agent_execution_failed",
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
            "category": "hosted_agent_execution_failed",
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
            ("worker_error", message.clone(), None)
        };
    json!({
        "status": if cancelled { "cancelled" } else { "failed" },
        "category": "hosted_agent_execution_failed",
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

fn unsuccessful_completion(
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
        mission_outcome: None,
        process_health: Some(if cancelled {
            "cancelled".into()
        } else {
            "failed".into()
        }),
        completion_evaluation: None,
        output_summary: None,
        failure_code: (!cancelled).then_some(failure_code),
        failure_message: (!cancelled).then_some(failure_message),
        head_branch: None,
        head_sha: None,
        pull_request_number: None,
        pull_request_url: None,
    }
}

fn hosted_pull_request_body(
    manifest: &HostedManifest,
    validation: &[ValidationResult],
    completeness: &CompletionEvaluation,
) -> String {
    let checks = validation
        .iter()
        .map(|result| {
            format!(
                "- {} `{}`",
                if result.status == "passed" {
                    "✅"
                } else {
                    "❌"
                },
                result.command
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
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
    let review_checklist = if completeness.review_checklist.is_empty() {
        "- None.".into()
    } else {
        completeness
            .review_checklist
            .iter()
            .map(|item| format!("- [ ] {}", item.description))
            .collect::<Vec<_>>()
            .join("\n")
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
Optional follow-up:\n{}\n\n{}Technical validation:\n{}\n\n\
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
        if checks.is_empty() {
            "- No required validation commands configured.".into()
        } else {
            checks
        }
    )
}

fn sanitized_message_content(item: &Value) -> Vec<Value> {
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

fn cache_observability_payload(
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

fn completion_evaluator_instructions() -> &'static str {
    "You are an independent implementation-completeness evaluator. Return only JSON with keys \
status, implementation_completeness, verification_readiness, evaluation_source, confidence, \
criteria, remaining_implementation_work, remaining_automated_verification, \
pending_external_review, optional_follow_up, review_checklist, unrecovered_tool_failures, and \
summary. Status is complete, complete_pending_external_review, partial, incomplete, blocked, or \
uncertain. implementation_completeness is complete, partial, or incomplete. \
verification_readiness is verified, automated_verified, pending_manual_review, or blocked. \
evaluation_source is model. Each criterion contains criterion_id, criterion, verification_type, \
status, evidence, validation_evidence, missing_evidence, and required_next_action. Verification \
type is code, automated_test, manual_qa, accessibility_review, visual_review, product_approval, \
or deployment_environment. Criterion status is satisfied, partially_satisfied, unsatisfied, \
uncertain, external_review_required, or not_applicable. Evidence contains repository-relative \
path and description. Never use passing tests or builds alone as functional evidence and never \
infer missing implementation optimistically. Human, design, product, visual, manual \
accessibility, and deployment-environment verification is external_review_required, not missing \
source code. Treat the final repository, complete diff, authoritative validation, and reconciled \
intended changes as higher precedence than raw tool-attempt history. Only genuinely unresolved \
intended changes may block completeness. Include exactly one criterion result for every acceptance criterion in the worker \
notebook, preserving its ac-N identifier, order, and text verbatim."
}

fn response_message_text(response: &Value) -> Option<String> {
    let mut text = String::new();
    for item in response.get("output")?.as_array()? {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        for content in item.get("content")?.as_array()? {
            if content.get("type").and_then(Value::as_str) == Some("output_text") {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(content.get("text")?.as_str()?);
            }
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

fn parse_completion_evaluation(text: &str) -> Result<CompletionEvaluation> {
    let trimmed = text.trim();
    let json_text = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    serde_json::from_str(json_text).context("completion evaluator returned malformed JSON")
}

fn validate_completion_evaluation(
    mut evaluation: CompletionEvaluation,
    implementation: &ImplementationOutcome,
    unrecovered: &[ToolFailureRecord],
    changed_paths: &[String],
    ticket_criteria: &[String],
) -> Result<CompletionEvaluation> {
    let authoritative_failures = unrecovered
        .iter()
        .map(|failure| {
            format!(
                "{}{}: {}",
                failure.tool,
                failure
                    .target
                    .as_deref()
                    .map(|target| format!(" ({target})"))
                    .unwrap_or_default(),
                failure.error
            )
        })
        .collect::<Vec<_>>();
    evaluation.unrecovered_tool_failures = authoritative_failures;
    if !evaluation.confidence.is_finite()
        || !(0.0..=1.0).contains(&evaluation.confidence)
        || evaluation.summary.trim().is_empty()
        || evaluation.criteria.is_empty()
        || evaluation.criteria.len() != ticket_criteria.len()
    {
        bail!("completion evaluation is incomplete");
    }
    let valid_paths = changed_paths.iter().collect::<BTreeSet<_>>();
    let mut evaluated_ids = BTreeSet::new();
    for (index, criterion) in evaluation.criteria.iter().enumerate() {
        let expected_id = format!("ac-{}", index + 1);
        if criterion.criterion.trim().is_empty()
            || criterion.criterion_id != expected_id
            || criterion.criterion != ticket_criteria[index]
            || !evaluated_ids.insert(criterion.criterion_id.as_str())
        {
            bail!("completion evaluation contains an invalid criterion");
        }
        if criterion.status == CriterionStatus::Satisfied
            && (criterion.evidence.is_empty()
                || criterion.evidence.iter().any(|evidence| {
                    evidence.description.trim().is_empty() || !valid_paths.contains(&evidence.path)
                }))
        {
            bail!("satisfied completion criterion lacks concrete diff evidence");
        }
        if criterion.status == CriterionStatus::ExternalReviewRequired
            && !criterion.verification_type.requires_external_review()
        {
            bail!("implementation-owned criterion cannot require external review");
        }
        if criterion.verification_type.requires_external_review()
            && !matches!(
                criterion.status,
                CriterionStatus::ExternalReviewRequired
                    | CriterionStatus::Satisfied
                    | CriterionStatus::NotApplicable
            )
        {
            bail!("external verification criterion has an invalid ownership status");
        }
        if criterion.status == CriterionStatus::ExternalReviewRequired
            && criterion.required_next_action.is_none()
        {
            bail!("external verification criterion requires an actionable review step");
        }
    }
    if evaluation.implementation_completeness == ImplementationCompleteness::Complete
        && (!unrecovered.is_empty()
            || implementation
                .explicit_declaration
                .as_ref()
                .is_none_or(|declaration| declaration.implementation_status != "complete")
            || !evaluation.remaining_implementation_work.is_empty()
            || !evaluation.remaining_automated_verification.is_empty()
            || evaluation.criteria.iter().any(|criterion| {
                !criterion.verification_type.requires_external_review()
                    && !matches!(
                        criterion.status,
                        CriterionStatus::Satisfied | CriterionStatus::NotApplicable
                    )
            }))
    {
        bail!("completion evaluator cannot prove implementation completeness");
    }
    if evaluation.status == CompletionStatus::CompletePendingExternalReview
        && (evaluation.implementation_completeness != ImplementationCompleteness::Complete
            || evaluation.verification_readiness != VerificationReadiness::PendingManualReview
            || evaluation.pending_external_review.is_empty()
            || evaluation.review_checklist.is_empty())
    {
        bail!("review-pending completion lacks its external review contract");
    }
    Ok(evaluation)
}

#[allow(clippy::too_many_arguments)]
fn completion_fallback(
    implementation: &ImplementationOutcome,
    impact_map: Option<&ImpactMap>,
    implementation_plan: Option<&ImplementationPlan>,
    unrecovered: &[ToolFailureRecord],
    changed_paths: &[String],
    ticket_criteria: &[String],
    validation: &[ValidationResult],
    policy: ProjectVerificationPolicy,
) -> CompletionEvaluation {
    let declaration = implementation.explicit_declaration.as_ref();
    let valid_paths = changed_paths.iter().collect::<BTreeSet<_>>();
    let all_validation_passed = validation.iter().all(|result| result.status == "passed");
    let mut evaluation = CompletionEvaluation {
        status: CompletionStatus::Uncertain,
        implementation_completeness: ImplementationCompleteness::Incomplete,
        verification_readiness: VerificationReadiness::Blocked,
        evaluation_source: EvaluationSource::OrchestratorFallback,
        confidence: 1.0,
        criteria: ticket_criteria
            .iter()
            .enumerate()
            .map(|(index, criterion)| {
                let mut verification_type = verification_type_for_criterion(criterion);
                let required_planned_paths = implementation_plan
                    .into_iter()
                    .flat_map(|plan| plan.planned_changes.iter())
                    .filter(|change| {
                        change
                            .acceptance_criteria
                            .iter()
                            .any(|mapped| mapped.trim() == criterion.trim())
                    })
                    .flat_map(|change| change.targets.iter().map(|target| target.path.clone()))
                    .collect::<BTreeSet<_>>();
                let unchanged_required_paths = required_planned_paths
                    .iter()
                    .filter(|path| !valid_paths.contains(path))
                    .cloned()
                    .collect::<Vec<_>>();
                let mut evidence = declaration
                    .into_iter()
                    .flat_map(|declaration| declaration.criteria_evidence.iter())
                    .filter(|item| item.criterion.trim() == criterion.trim())
                    .flat_map(|item| {
                        item.paths
                            .iter()
                            .filter(|path| valid_paths.contains(path))
                            .map(|path| CompletionEvidence {
                                path: path.clone(),
                                description: item.evidence.clone(),
                            })
                    })
                    .collect::<Vec<_>>();
                if evidence.is_empty() {
                    evidence = impact_map
                        .into_iter()
                        .flat_map(|map| map.areas.iter())
                        .filter(|area| {
                            area.acceptance_criteria_ids
                                .iter()
                                .any(|mapped| mapped == &impact_map::criterion_id(index))
                        })
                        .flat_map(|area| {
                            area.candidate_paths
                                .iter()
                                .filter(|path| valid_paths.contains(path))
                                .map(|path| CompletionEvidence {
                                    path: path.clone(),
                                    description: area.reason.clone(),
                                })
                        })
                        .collect();
                }
                if evidence.is_empty() {
                    evidence = implementation_plan
                        .into_iter()
                        .flat_map(|plan| plan.planned_changes.iter())
                        .filter(|change| {
                            change
                                .acceptance_criteria
                                .iter()
                                .any(|mapped| mapped.trim() == criterion.trim())
                        })
                        .flat_map(|change| {
                            change
                                .targets
                                .iter()
                                .filter(|target| valid_paths.contains(&target.path))
                                .map(|target| CompletionEvidence {
                                    path: target.path.clone(),
                                    description: if target.role.is_empty() {
                                        change.reason.clone()
                                    } else {
                                        target.role.clone()
                                    },
                                })
                        })
                        .collect();
                }
                let mandatory_e2e_missing = browser_e2e_is_mandatory_and_missing(
                    criterion,
                    policy,
                    changed_paths,
                );
                if mandatory_e2e_missing {
                    verification_type = VerificationType::AutomatedTest;
                }
                let (status, missing_evidence, required_next_action) =
                    if verification_type.requires_external_review() {
                        (
                            CriterionStatus::ExternalReviewRequired,
                            vec!["External review evidence has not been recorded.".into()],
                            Some(criterion.clone()),
                        )
                    } else if mandatory_e2e_missing {
                        (
                            CriterionStatus::Unsatisfied,
                            vec!["Project policy requires browser E2E coverage for this theme change.".into()],
                            Some("Add and pass the required authenticated browser E2E coverage.".into()),
                        )
                    } else if !unchanged_required_paths.is_empty() {
                        (
                            CriterionStatus::Unsatisfied,
                            vec![format!(
                                "Required planned paths were unchanged: {}.",
                                unchanged_required_paths.join(", ")
                            )],
                            Some(format!(
                                "Implement and verify the unchanged required paths: {}.",
                                unchanged_required_paths.join(", ")
                            )),
                        )
                    } else if !unrecovered.is_empty() {
                        (
                            CriterionStatus::Unsatisfied,
                            vec!["A source-changing tool failure remains unrecovered.".into()],
                            Some("Recover the failed implementation change and rerun validation.".into()),
                        )
                    } else if declaration.is_some_and(|value| {
                        value.implementation_status == "complete"
                    }) && !evidence.is_empty()
                        && (verification_type != VerificationType::AutomatedTest
                            || all_validation_passed)
                    {
                        (CriterionStatus::Satisfied, Vec::new(), None)
                    } else {
                        (
                            CriterionStatus::Uncertain,
                            vec!["No complete criterion-to-diff evidence was available.".into()],
                            Some("Provide concrete implementation evidence for this criterion.".into()),
                        )
                    };
                CriterionEvaluation {
                    criterion_id: format!("ac-{}", index + 1),
                    criterion: criterion.clone(),
                    verification_type,
                    status,
                    evidence,
                    validation_evidence: if status == CriterionStatus::Satisfied
                        && matches!(
                            verification_type,
                            VerificationType::Code | VerificationType::AutomatedTest
                        )
                    {
                        validation
                            .iter()
                            .filter(|result| result.status == "passed")
                            .map(|result| result.command.clone())
                            .collect()
                    } else {
                        Vec::new()
                    },
                    missing_evidence,
                    required_next_action,
                }
            })
            .collect(),
        remaining_implementation_work: Vec::new(),
        remaining_automated_verification: Vec::new(),
        pending_external_review: Vec::new(),
        optional_follow_up: Vec::new(),
        review_checklist: Vec::new(),
        unrecovered_tool_failures: unrecovered
            .iter()
            .map(|failure| {
                format!(
                    "{}{}: {}",
                    failure.tool,
                    failure
                        .target
                        .as_deref()
                        .map(|target| format!(" ({target})"))
                        .unwrap_or_default(),
                    failure.error
                )
            })
            .collect(),
        summary: "Completion was classified from the authoritative notebook, diff, declaration, and validation evidence.".into(),
    };
    if let Some(declaration) = declaration {
        for work in &declaration.remaining_work {
            classify_remaining_work(work, &mut evaluation);
        }
    }
    finalize_completion_dimensions(&mut evaluation, implementation, unrecovered);
    evaluation
}

fn reconcile_model_completion_evaluation(
    model: CompletionEvaluation,
    mut fallback: CompletionEvaluation,
    implementation: &ImplementationOutcome,
    unrecovered: &[ToolFailureRecord],
) -> CompletionEvaluation {
    if model.criteria.is_empty() {
        return fallback;
    }
    let mut matched = 0_usize;
    for expected in &mut fallback.criteria {
        if let Some(candidate) = model.criteria.iter().find(|candidate| {
            candidate.criterion_id == expected.criterion_id
                && candidate.criterion == expected.criterion
        }) {
            let mut candidate = candidate.clone();
            if candidate.status == CriterionStatus::Satisfied {
                for validation in &expected.validation_evidence {
                    push_unique(&mut candidate.validation_evidence, validation.clone());
                }
            }
            *expected = candidate;
            matched = matched.saturating_add(1);
        }
    }
    fallback.confidence = model.confidence;
    if !model.summary.trim().is_empty() {
        fallback.summary = model.summary;
    }
    fallback.optional_follow_up = model.optional_follow_up;
    fallback.evaluation_source =
        if matched == fallback.criteria.len() && model.criteria.len() == fallback.criteria.len() {
            EvaluationSource::Model
        } else {
            EvaluationSource::Hybrid
        };
    finalize_completion_dimensions(&mut fallback, implementation, unrecovered);
    fallback
}

fn finalize_completion_dimensions(
    evaluation: &mut CompletionEvaluation,
    implementation: &ImplementationOutcome,
    unrecovered: &[ToolFailureRecord],
) {
    evaluation.review_checklist.clear();
    for criterion in &evaluation.criteria {
        match criterion.status {
            CriterionStatus::ExternalReviewRequired => {
                push_unique(
                    &mut evaluation.pending_external_review,
                    criterion
                        .required_next_action
                        .clone()
                        .unwrap_or_else(|| criterion.criterion.clone()),
                );
                evaluation.review_checklist.push(ReviewChecklistItem {
                    r#type: criterion.verification_type,
                    description: criterion
                        .required_next_action
                        .clone()
                        .unwrap_or_else(|| criterion.criterion.clone()),
                    status: "pending".into(),
                });
            }
            CriterionStatus::Unsatisfied | CriterionStatus::PartiallySatisfied => {
                let work = criterion
                    .required_next_action
                    .clone()
                    .unwrap_or_else(|| criterion.criterion.clone());
                if criterion.verification_type == VerificationType::AutomatedTest {
                    push_unique(&mut evaluation.remaining_automated_verification, work);
                } else if !criterion.verification_type.requires_external_review() {
                    push_unique(&mut evaluation.remaining_implementation_work, work);
                }
            }
            CriterionStatus::Satisfied
            | CriterionStatus::Uncertain
            | CriterionStatus::NotApplicable => {}
        }
    }
    let declaration_status = implementation
        .explicit_declaration
        .as_ref()
        .map(|declaration| declaration.implementation_status.as_str());
    let internal_criteria_complete = evaluation.criteria.iter().all(|criterion| {
        criterion.verification_type.requires_external_review()
            || matches!(
                criterion.status,
                CriterionStatus::Satisfied | CriterionStatus::NotApplicable
            )
    });
    evaluation.implementation_completeness =
        if implementation.explicit_declaration.as_ref().is_none()
            || declaration_status == Some("blocked")
            || implementation.budget_exhausted
            || !unrecovered.is_empty()
            || !evaluation.remaining_implementation_work.is_empty()
            || !evaluation.remaining_automated_verification.is_empty()
            || !internal_criteria_complete
        {
            if declaration_status == Some("blocked")
                || implementation.explicit_declaration.is_none()
                || !unrecovered.is_empty()
            {
                ImplementationCompleteness::Incomplete
            } else {
                ImplementationCompleteness::Partial
            }
        } else {
            ImplementationCompleteness::Complete
        };
    evaluation.verification_readiness = if declaration_status == Some("blocked") {
        VerificationReadiness::Blocked
    } else if !evaluation.pending_external_review.is_empty() {
        VerificationReadiness::PendingManualReview
    } else if evaluation.implementation_completeness == ImplementationCompleteness::Complete {
        VerificationReadiness::AutomatedVerified
    } else {
        VerificationReadiness::Blocked
    };
    evaluation.status = if declaration_status == Some("blocked") {
        CompletionStatus::Blocked
    } else if evaluation.implementation_completeness == ImplementationCompleteness::Complete
        && evaluation.verification_readiness == VerificationReadiness::PendingManualReview
    {
        CompletionStatus::CompletePendingExternalReview
    } else if evaluation.implementation_completeness == ImplementationCompleteness::Complete {
        CompletionStatus::Complete
    } else if implementation.explicit_declaration.is_none() {
        CompletionStatus::Uncertain
    } else if evaluation.implementation_completeness == ImplementationCompleteness::Partial {
        CompletionStatus::Partial
    } else {
        CompletionStatus::Incomplete
    };
}

fn verification_type_for_criterion(criterion: &str) -> VerificationType {
    let normalized = criterion.to_ascii_lowercase();
    if normalized.contains("product")
        || normalized.contains("design owner")
        || normalized.contains("palette approval")
        || normalized.contains("approved by")
    {
        VerificationType::ProductApproval
    } else if normalized.contains("accessibility")
        || normalized.contains("contrast")
        || normalized.contains("keyboard focus")
    {
        VerificationType::AccessibilityReview
    } else if normalized.contains("screenshot") || normalized.contains("visual review") {
        VerificationType::VisualReview
    } else if normalized.contains("deployment")
        || normalized.contains("staging")
        || normalized.contains("production environment")
    {
        VerificationType::DeploymentEnvironment
    } else if normalized.contains("manual")
        || normalized.contains("navigation")
        || normalized.contains("page reload")
        || normalized.contains("browser verification")
    {
        VerificationType::ManualQa
    } else if normalized.contains("test")
        || normalized.contains("coverage")
        || normalized.contains("build")
        || normalized.contains("lint")
    {
        VerificationType::AutomatedTest
    } else {
        VerificationType::Code
    }
}

fn browser_e2e_is_mandatory_and_missing(
    criterion: &str,
    policy: ProjectVerificationPolicy,
    changed_paths: &[String],
) -> bool {
    if !policy.browser_e2e_required_for_theme_changes {
        return false;
    }
    let normalized = criterion.to_ascii_lowercase();
    let is_theme_browser_criterion = (normalized.contains("theme")
        || normalized.contains("palette"))
        && (normalized.contains("browser")
            || normalized.contains("navigation")
            || normalized.contains("reload")
            || normalized.contains("e2e"));
    is_theme_browser_criterion
        && !changed_paths.iter().any(|path| {
            let path = path.to_ascii_lowercase();
            path.contains("e2e")
                || path.contains("playwright")
                || path.ends_with(".spec.ts")
                || path.ends_with(".spec.tsx")
        })
}

fn classify_remaining_work(work: &str, evaluation: &mut CompletionEvaluation) {
    let verification_type = verification_type_for_criterion(work);
    if verification_type.requires_external_review() {
        push_unique(&mut evaluation.pending_external_review, work.to_owned());
    } else if verification_type == VerificationType::AutomatedTest {
        push_unique(
            &mut evaluation.remaining_automated_verification,
            work.to_owned(),
        );
    } else {
        push_unique(
            &mut evaluation.remaining_implementation_work,
            work.to_owned(),
        );
    }
}

fn completion_review_diff(root: &Path, changed_paths: &[String], base_sha: &str) -> Result<String> {
    let diff = command::capture(
        "git",
        ["diff", "--no-ext-diff", "--binary", base_sha, "--"],
        root,
    )?;
    if !diff.status.success() {
        bail!("git diff exited with {}: {}", diff.status, diff.stderr);
    }
    let mut review = diff.stdout;
    for path in changed_paths {
        let target = safe_repo_path(root, path, false)?;
        let tracked = command::capture("git", ["ls-files", "--error-unmatch", "--", path], root)?
            .status
            .success();
        if tracked || !target.is_file() {
            continue;
        }
        review.push_str(&format!("\n\n--- /dev/null\n+++ b/{path}\n"));
        match fs::read(&target) {
            Ok(bytes) if bytes.len() <= MAX_MODEL_FILE_BYTES && !bytes.contains(&0) => {
                review.push_str(&String::from_utf8_lossy(&bytes));
            }
            Ok(bytes) => review.push_str(&format!(
                "[new binary or large file: {} bytes]",
                bytes.len()
            )),
            Err(error) => review.push_str(&format!("[could not read new file: {error}]")),
        }
    }
    Ok(review)
}

fn completion_changed_paths(repo: &Repo, base_sha: &str) -> Result<Vec<String>> {
    let mut changed_paths = repo
        .new_agent_paths(&BTreeSet::new())?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let output = command::capture(
        "git",
        ["diff", "--name-only", "-z", base_sha, "HEAD", "--"],
        &repo.root,
    )?;
    if !output.status.success() {
        bail!(
            "git diff --name-only exited with {}: {}",
            output.status,
            output.stderr
        );
    }
    changed_paths.extend(
        output
            .stdout
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned),
    );
    Ok(changed_paths.into_iter().collect())
}

fn append_fingerprint_field(material: &mut Vec<u8>, name: &str, value: &[u8]) {
    material.extend_from_slice(&(name.len() as u64).to_be_bytes());
    material.extend_from_slice(name.as_bytes());
    material.extend_from_slice(&(value.len() as u64).to_be_bytes());
    material.extend_from_slice(value);
}

fn repository_state_fingerprint(repo: &Repo, base_sha: &str) -> Result<String> {
    let head = command::checked("git", ["rev-parse", "HEAD"], &repo.root)?;
    let comparison_base = if base_sha.trim().is_empty() {
        head.trim()
    } else {
        base_sha.trim()
    };
    let identity = command::capture("git", ["remote", "get-url", "origin"], &repo.root)?;
    let repository_identity = if identity.status.success() {
        identity.stdout.trim().to_owned()
    } else {
        repo.root
            .canonicalize()
            .unwrap_or_else(|_| repo.root.clone())
            .to_string_lossy()
            .into_owned()
    };
    let base_paths = command::capture(
        "git",
        ["ls-tree", "-r", "--name-only", "-z", comparison_base],
        &repo.root,
    )?;
    if !base_paths.status.success() {
        bail!(
            "git base-tree listing for repository fingerprint failed: {}",
            base_paths.stderr
        );
    }
    let current_paths = command::capture(
        "git",
        [
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        &repo.root,
    )?;
    if !current_paths.status.success() {
        bail!(
            "git worktree listing for repository fingerprint failed: {}",
            current_paths.stderr
        );
    }
    let mut material = Vec::new();
    append_fingerprint_field(&mut material, "repository", repository_identity.as_bytes());
    append_fingerprint_field(&mut material, "base_head", comparison_base.as_bytes());
    let paths = base_paths
        .stdout
        .split('\0')
        .chain(current_paths.stdout.split('\0'))
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    for path in paths {
        if Path::new(&path).is_absolute()
            || Path::new(&path)
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            bail!("git returned an unsafe repository path while fingerprinting state");
        }
        append_fingerprint_field(&mut material, "path", path.as_bytes());
        let target = repo.root.join(&path);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                append_fingerprint_field(&mut material, "type", b"symlink");
                let destination = fs::read_link(&target)
                    .with_context(|| format!("could not hash repository symlink target {path}"))?;
                append_fingerprint_field(
                    &mut material,
                    "symlink_target",
                    destination.to_string_lossy().as_bytes(),
                );
            }
            Ok(metadata) if metadata.is_file() => {
                append_fingerprint_field(&mut material, "type", b"file");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    append_fingerprint_field(
                        &mut material,
                        "mode",
                        &metadata.permissions().mode().to_be_bytes(),
                    );
                }
                let bytes = fs::read(&target)
                    .with_context(|| format!("could not hash repository file {path}"))?;
                append_fingerprint_field(
                    &mut material,
                    "content_sha256",
                    hex::encode(Sha256::digest(bytes)).as_bytes(),
                );
            }
            Ok(_) => append_fingerprint_field(&mut material, "type", b"directory_or_gitlink"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                append_fingerprint_field(&mut material, "type", b"missing");
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not inspect repository path {path}"));
            }
        }
    }
    append_fingerprint_field(
        &mut material,
        "dependency_lock",
        dependency_lock_fingerprint(&repo.root)?.as_bytes(),
    );
    Ok(hex::encode(Sha256::digest(material)))
}

fn required_tool_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    name: &str,
    maximum: usize,
) -> Result<&'a str> {
    object
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= maximum)
        .with_context(|| format!("tool argument `{name}` is missing or too large"))
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn is_source_mutation_tool(name: &str) -> bool {
    matches!(
        name,
        "write_file"
            | "replace_text"
            | "replace_range"
            | "insert_after_symbol"
            | "insert_before_symbol"
            | "apply_unified_diff"
            | "rewrite_small_file"
            | "delete_file"
    )
}

fn informational_write_progress(status: &str, reason: &str) -> Result<String> {
    if !matches!(status, "blocked" | "ready_to_write" | "no_change_required") {
        bail!("write progress status is unsupported");
    }
    Ok(format!(
        "recorded informational write progress (repository_progress=false): {status}: {reason}"
    ))
}

const fn informational_write_progress_semantics() -> (ToolProgressClass, bool) {
    (ToolProgressClass::Neutral, false)
}

fn tool_target(arguments: &str) -> Option<String> {
    serde_json::from_str::<Value>(arguments)
        .ok()?
        .get("path")?
        .as_str()
        .map(|path| truncate_text(path, 4_096))
}

fn tool_change_id(arguments: &str) -> Option<String> {
    serde_json::from_str::<Value>(arguments)
        .ok()?
        .get("change_id")?
        .as_str()
        .map(|change_id| truncate_text(change_id, 100))
}

fn repo_file_sha256(root: &Path, path: &str) -> Option<String> {
    let target = safe_repo_path(root, path, false).ok()?;
    let bytes = fs::read(target).ok()?;
    Some(hex::encode(Sha256::digest(bytes)))
}

fn classify_write_failure(error: &str) -> (String, Option<usize>) {
    if error.contains("replace_match_not_unique") {
        let match_count = error
            .split("found ")
            .nth(1)
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse().ok());
        ("mutation_content_conflict".into(), match_count)
    } else if error.contains("symbol_match_not_unique") {
        let match_count = error
            .split("found ")
            .nth(1)
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse().ok());
        ("mutation_content_conflict".into(), match_count)
    } else if error.contains("strategy exhausted") || error.contains("line range") {
        ("mutation_content_conflict".into(), None)
    } else if error.contains("unified diff") {
        ("mutation_patch_failed".into(), None)
    } else {
        ("mutation_content_conflict".into(), None)
    }
}

fn tool_intent_sha256(name: &str, arguments: &str) -> String {
    let mut material = name.as_bytes().to_vec();
    material.push(0);
    material.extend_from_slice(arguments.as_bytes());
    hex::encode(Sha256::digest(material))
}

fn model_budget_handoff_summary(allowed: bool, changed_paths: &[String]) -> Option<String> {
    (allowed && !changed_paths.is_empty()).then(|| {
        format!(
            "The implementation model used its configured call budget after changing {} path(s). RustGrid will preserve the work, run useful technical gates, and classify it through an independent completion evaluation; passing gates alone cannot mark it complete.",
            changed_paths.len()
        )
    })
}

fn ai_budget_exhaustion_reason(error: &anyhow::Error) -> Option<String> {
    error
        .downcast_ref::<HostedHttpError>()
        .filter(|failure| failure.code == "execution_ai_budget_exceeded")
        .map(|failure| failure.code.clone())
}

fn provider_request_metadata(
    execution_id: Uuid,
    ticket_key: &str,
    agent: &str,
    phase: ExecutionPhase,
    model_call_budget: i32,
) -> Value {
    json!({
        "execution_id": execution_id.to_string(),
        "ticket_key": ticket_key,
        "agent": agent,
        "phase": phase.as_str(),
        "model_call_budget": model_call_budget.to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
fn provider_rejected_event(
    failure: &HostedHttpError,
    registration: &AiCallRegistration,
    execution_attempt: i32,
    model_call: usize,
    configured_model: &str,
    model_calls_used: usize,
    budget: Value,
    notebook: Value,
) -> Value {
    json!({
        "event_type": "execution.ai.provider_rejected",
        "semantic_call_id": registration.semantic_call_id,
        "call_index": model_call.saturating_sub(1),
        "execution_attempt": execution_attempt,
        "failure_stage": "provider_dispatch",
        "rustgrid_gateway_status": failure.rustgrid_gateway_status(),
        "upstream_provider_status": failure.upstream_provider_status,
        "provider_contacted": true,
        "rustgrid_request_id": failure.rustgrid_request_id.as_deref(),
        "transport_request_id": failure.transport_request_id.as_deref(),
        "provider_request_id": failure.provider_request_id.as_deref(),
        "reservation_state": failure.reservation_state(),
        "reservation_reconciliation_state": failure.reservation_reconciliation_state(),
        "provider_error": failure.provider_error.as_ref(),
        "provider_response_body": failure.provider_response_body.as_ref(),
        "provider_error_code": failure
            .provider_error
            .as_ref()
            .and_then(|provider| provider.code.as_deref()),
        "provider_error_parameter": failure
            .provider_error
            .as_ref()
            .and_then(|provider| provider.parameter.as_deref()),
        "model_alias": failure.model_alias.as_deref().unwrap_or(configured_model),
        "resolved_provider_model": failure.resolved_provider_model.as_deref(),
        "adapter_version": failure.adapter_version.as_deref(),
        "payload_schema_version": failure.payload_schema_version.as_deref(),
        "provider_attempts": failure.provider_attempts.unwrap_or(1),
        "model_calls_used": model_calls_used,
        "call_budget_consumed": false,
        "actual_cost_micros": failure.actual_cost_micros.unwrap_or(0),
        "retryable": false,
        "message": failure.terminal_message(),
        "budget": budget,
        "notebook": notebook,
    })
}

fn validate_provider_request_envelope(request: &Value) -> Result<()> {
    const ALLOWED_FIELDS: &[&str] = &[
        "model",
        "input",
        "instructions",
        "max_output_tokens",
        "reasoning",
        "text",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "temperature",
        "top_p",
        "metadata",
        "store",
        "stream",
    ];
    let object = request
        .as_object()
        .ok_or_else(|| anyhow!("ai_provider_request_invalid: request must be an object"))?;
    if let Some(field) = object
        .keys()
        .find(|field| !ALLOWED_FIELDS.contains(&field.as_str()))
    {
        bail!("ai_provider_request_invalid: unsupported request field `{field}`");
    }
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty() && model.len() <= 200)
        .ok_or_else(|| anyhow!("ai_provider_request_invalid: model must be a bounded string"))?;
    if !safe_identifier(model, 200) {
        bail!("ai_provider_request_invalid: model contains unsupported characters");
    }
    if request.get("input").is_none() {
        bail!("ai_provider_request_invalid: input is required");
    }
    if request
        .get("max_output_tokens")
        .is_none_or(|value| value.as_i64().is_none_or(|value| value <= 0))
    {
        bail!("ai_provider_request_invalid: max_output_tokens must be a positive integer");
    }
    if request
        .get("store")
        .is_some_and(|value| value != &json!(false))
    {
        bail!("ai_provider_request_invalid: provider-side storage is not allowed");
    }
    if request
        .get("stream")
        .is_some_and(|value| !value.is_boolean())
    {
        bail!("ai_provider_request_invalid: stream must be boolean");
    }
    if request
        .get("parallel_tool_calls")
        .is_some_and(|value| !value.is_boolean())
    {
        bail!("ai_provider_request_invalid: parallel_tool_calls must be boolean");
    }
    if let Some(reasoning) = request.get("reasoning") {
        let reasoning = reasoning
            .as_object()
            .ok_or_else(|| anyhow!("ai_provider_request_invalid: reasoning must be an object"))?;
        if reasoning.keys().any(|key| key != "effort")
            || reasoning.get("effort").is_some_and(|effort| {
                !matches!(
                    effort.as_str(),
                    Some("none" | "low" | "medium" | "high" | "xhigh" | "max")
                )
            })
        {
            bail!("ai_provider_request_invalid: reasoning configuration is unsupported");
        }
    }
    if let Some(tools) = request.get("tools") {
        validate_provider_tool_definitions(tools)?;
    }
    if let Some(tool_choice) = request.get("tool_choice") {
        validate_provider_tool_choice(tool_choice, request.get("tools"))?;
    }
    if let Some(text) = request.get("text") {
        validate_provider_text_configuration(text)?;
    }
    if let Some(metadata) = request.get("metadata") {
        let metadata = metadata
            .as_object()
            .ok_or_else(|| anyhow!("ai_provider_request_invalid: metadata must be an object"))?;
        if metadata.len() > 16 {
            bail!("ai_provider_request_invalid: metadata cannot contain more than 16 entries");
        }
        for (key, value) in metadata {
            if key.is_empty() || key.len() > 64 || key.chars().any(char::is_control) {
                bail!("ai_provider_request_invalid: metadata keys must contain 1 to 64 safe bytes");
            }
            let value = value.as_str().ok_or_else(|| {
                anyhow!("ai_provider_request_invalid: metadata value `{key}` must be a string")
            })?;
            if value.len() > 512 {
                bail!(
                    "ai_provider_request_invalid: metadata value `{key}` cannot exceed 512 bytes"
                );
            }
        }
    }
    Ok(())
}

fn validate_hosted_provider_startup_contract(manifest: &HostedManifest) -> Result<()> {
    let request = json!({
        "model": manifest.ai_gateway.model,
        "input": [{"role": "user", "content": "startup contract validation"}],
        "max_output_tokens": manifest.ai_gateway.maximum_output_tokens.min(16_384),
        "reasoning": {"effort": "medium"},
        "tools": hosted_tools(),
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "metadata": provider_request_metadata(
            manifest.execution.execution_id,
            manifest.ticket_key.as_str(),
            "rustgrid-agent-hosted",
            ExecutionPhase::Discovery,
            manifest
                .model_call_budget
                .unwrap_or(DEFAULT_HOSTED_MODEL_CALLS as i32),
        ),
        "store": false,
        "stream": false,
    });
    validate_provider_request_envelope(&request)
}

fn validate_provider_tool_definitions(tools: &Value) -> Result<()> {
    let tools = tools
        .as_array()
        .ok_or_else(|| anyhow!("ai_tool_schema_invalid: tools must be an array"))?;
    if tools.len() > 64 {
        bail!("ai_tool_schema_invalid: tools cannot contain more than 64 functions");
    }
    let mut names = BTreeSet::new();
    for (index, tool) in tools.iter().enumerate() {
        let path = format!("tools[{index}]");
        let object = tool
            .as_object()
            .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path} must be an object"))?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "type" | "name" | "description" | "parameters" | "strict"
            )
        }) || object.get("type").and_then(Value::as_str) != Some("function")
        {
            bail!("ai_tool_schema_invalid: {path} has an unsupported function shape");
        }
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            })
            .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path}.name is invalid"))?;
        if !names.insert(name.to_owned()) {
            bail!("ai_tool_schema_invalid: duplicate tool name `{name}`");
        }
        if object
            .get("description")
            .is_some_and(|value| value.as_str().is_none_or(|value| value.len() > 8 * 1024))
        {
            bail!("ai_tool_schema_invalid: {path}.description is invalid");
        }
        let strict = match object.get("strict") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => bail!("ai_tool_schema_invalid: {path}.strict must be boolean"),
        };
        let parameters = object
            .get("parameters")
            .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path}.parameters is required"))?;
        validate_provider_json_schema(parameters, &format!("{path}.parameters"), 0, strict, true)?;
    }
    Ok(())
}

fn validate_provider_tool_choice(tool_choice: &Value, tools: Option<&Value>) -> Result<()> {
    if tool_choice
        .as_str()
        .is_some_and(|choice| matches!(choice, "auto" | "none" | "required"))
    {
        return Ok(());
    }
    let choice = tool_choice.as_object().ok_or_else(|| {
        anyhow!("ai_provider_request_invalid: tool_choice must be a supported string or object")
    })?;
    if choice.len() != 2
        || choice.get("type").and_then(Value::as_str) != Some("function")
        || choice.get("name").and_then(Value::as_str).is_none()
    {
        bail!("ai_provider_request_invalid: forced tool_choice must identify one function");
    }
    let selected = choice["name"].as_str().unwrap_or_default();
    let declared = tools.and_then(Value::as_array).is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some(selected))
    });
    if !declared {
        bail!("ai_provider_request_invalid: forced tool_choice is not declared");
    }
    Ok(())
}

fn validate_provider_text_configuration(text: &Value) -> Result<()> {
    let text = text
        .as_object()
        .ok_or_else(|| anyhow!("ai_response_schema_invalid: text must be an object"))?;
    if text
        .keys()
        .any(|key| !matches!(key.as_str(), "format" | "verbosity"))
        || text
            .get("verbosity")
            .is_some_and(|value| !matches!(value.as_str(), Some("low" | "medium" | "high")))
    {
        bail!("ai_response_schema_invalid: text configuration is unsupported");
    }
    let Some(format) = text.get("format") else {
        return Ok(());
    };
    let format = format
        .as_object()
        .ok_or_else(|| anyhow!("ai_response_schema_invalid: text.format must be an object"))?;
    match format.get("type").and_then(Value::as_str) {
        Some("text") if format.len() == 1 => Ok(()),
        Some("json_schema") => {
            if format.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "type" | "name" | "description" | "schema" | "strict"
                )
            }) {
                bail!("ai_response_schema_invalid: text.format contains an unsupported field");
            }
            format
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| {
                    !name.is_empty()
                        && name.len() <= 64
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                })
                .ok_or_else(|| {
                    anyhow!("ai_response_schema_invalid: text.format.name is invalid")
                })?;
            let strict = match format.get("strict") {
                None => false,
                Some(Value::Bool(value)) => *value,
                Some(_) => {
                    bail!("ai_response_schema_invalid: text.format.strict must be boolean")
                }
            };
            let schema = format.get("schema").ok_or_else(|| {
                anyhow!("ai_response_schema_invalid: text.format.schema is required")
            })?;
            validate_provider_json_schema(schema, "text.format.schema", 0, strict, true)
                .map_err(|error| anyhow!("ai_response_schema_invalid: {error}"))
        }
        _ => bail!("ai_response_schema_invalid: text.format.type is unsupported"),
    }
}

fn validate_provider_json_schema(
    schema: &Value,
    path: &str,
    depth: usize,
    strict: bool,
    require_object: bool,
) -> Result<()> {
    const MAX_DEPTH: usize = 10;
    const ALLOWED_KEYWORDS: &[&str] = &[
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "description",
        "minimum",
        "maximum",
        "minItems",
        "maxItems",
    ];
    if depth >= MAX_DEPTH {
        bail!("ai_tool_schema_invalid: {path} exceeds the supported nesting depth");
    }
    let object = schema
        .as_object()
        .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path} must be an object"))?;
    if let Some(keyword) = object
        .keys()
        .find(|keyword| !ALLOWED_KEYWORDS.contains(&keyword.as_str()))
    {
        bail!("ai_tool_schema_invalid: {path}.{keyword} is unsupported");
    }
    let schema_type = provider_schema_type(object.get("type"), path)?;
    if require_object && schema_type.as_deref() != Some("object") {
        bail!("ai_tool_schema_invalid: {path}.type must be object");
    }
    if object
        .get("description")
        .is_some_and(|value| !value.is_string())
    {
        bail!("ai_tool_schema_invalid: {path}.description must be a string");
    }
    if let Some(values) = object.get("enum") {
        let values = values
            .as_array()
            .filter(|values| !values.is_empty())
            .ok_or_else(|| {
                anyhow!("ai_tool_schema_invalid: {path}.enum must be a non-empty array")
            })?;
        let unique = values.iter().map(Value::to_string).collect::<BTreeSet<_>>();
        if unique.len() != values.len() {
            bail!("ai_tool_schema_invalid: {path}.enum contains duplicate values");
        }
        if values
            .iter()
            .any(|value| !provider_schema_type_accepts(object.get("type"), value))
        {
            bail!("ai_tool_schema_invalid: {path}.enum contains a value outside its declared type");
        }
    }
    let has_numeric_bounds = object.contains_key("minimum") || object.contains_key("maximum");
    if has_numeric_bounds && !matches!(schema_type.as_deref(), Some("integer" | "number")) {
        bail!("ai_tool_schema_invalid: {path} uses numeric bounds without a numeric type");
    }
    for keyword in ["minimum", "maximum"] {
        if object.get(keyword).is_some_and(|value| !value.is_number()) {
            bail!("ai_tool_schema_invalid: {path}.{keyword} must be numeric");
        }
    }
    for keyword in ["minItems", "maxItems"] {
        if object
            .get(keyword)
            .is_some_and(|value| value.as_u64().is_none())
        {
            bail!("ai_tool_schema_invalid: {path}.{keyword} must be non-negative");
        }
    }
    if object
        .get("minimum")
        .and_then(Value::as_f64)
        .zip(object.get("maximum").and_then(Value::as_f64))
        .is_some_and(|(minimum, maximum)| minimum > maximum)
        || object
            .get("minItems")
            .and_then(Value::as_u64)
            .zip(object.get("maxItems").and_then(Value::as_u64))
            .is_some_and(|(minimum, maximum)| minimum > maximum)
    {
        bail!("ai_tool_schema_invalid: {path} has inverted bounds");
    }

    if schema_type.as_deref() == Some("object") {
        let empty_properties = serde_json::Map::new();
        let properties = match object.get("properties") {
            Some(Value::Object(properties)) => properties,
            Some(_) => bail!("ai_tool_schema_invalid: {path}.properties must be an object"),
            None => &empty_properties,
        };
        if object
            .get("additionalProperties")
            .is_some_and(|value| value != &Value::Bool(false))
        {
            bail!("ai_tool_schema_invalid: {path}.additionalProperties must be false");
        }
        if strict && object.get("additionalProperties") != Some(&Value::Bool(false)) {
            bail!(
                "ai_tool_schema_invalid: {path}.additionalProperties is required for strict schemas"
            );
        }
        let empty_required = Vec::new();
        let required = match object.get("required") {
            Some(Value::Array(required)) => required,
            Some(_) => bail!("ai_tool_schema_invalid: {path}.required must be an array"),
            None => &empty_required,
        };
        let mut required_names = BTreeSet::new();
        for (index, required) in required.iter().enumerate() {
            let required = required.as_str().ok_or_else(|| {
                anyhow!("ai_tool_schema_invalid: {path}.required[{index}] must be a string")
            })?;
            if !properties.contains_key(required) || !required_names.insert(required) {
                bail!(
                    "ai_tool_schema_invalid: {path}.required[{index}] must name one property once"
                );
            }
        }
        if strict && required_names.len() != properties.len() {
            bail!("ai_tool_schema_invalid: {path}.required must include every strict property");
        }
        for (name, property) in properties {
            if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
                bail!("ai_tool_schema_invalid: {path}.properties has an invalid name");
            }
            validate_provider_json_schema(
                property,
                &format!("{path}.properties.{name}"),
                depth + 1,
                strict,
                false,
            )?;
        }
    } else if object.contains_key("properties")
        || object.contains_key("required")
        || object.contains_key("additionalProperties")
    {
        bail!("ai_tool_schema_invalid: {path} uses object keywords without object type");
    }

    if schema_type.as_deref() == Some("array") {
        let items = object
            .get("items")
            .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path}.items is required"))?;
        validate_provider_json_schema(items, &format!("{path}.items"), depth + 1, strict, false)?;
    } else if object.contains_key("items")
        || object.contains_key("minItems")
        || object.contains_key("maxItems")
    {
        bail!("ai_tool_schema_invalid: {path} uses array keywords without array type");
    }
    Ok(())
}

fn provider_schema_type(value: Option<&Value>, path: &str) -> Result<Option<String>> {
    let supported = |value: &str| {
        matches!(
            value,
            "object" | "array" | "string" | "integer" | "number" | "boolean" | "null"
        )
    };
    let Some(value) = value else {
        return Ok(None);
    };
    if let Some(value) = value.as_str() {
        if supported(value) {
            return Ok(Some(value.to_owned()));
        }
        bail!("ai_tool_schema_invalid: {path}.type is unsupported");
    }
    let values = value
        .as_array()
        .filter(|values| values.len() == 2)
        .ok_or_else(|| {
            anyhow!("ai_tool_schema_invalid: {path}.type nullable union must contain two types")
        })?;
    let first = values[0]
        .as_str()
        .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path}.type must contain strings"))?;
    let second = values[1]
        .as_str()
        .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path}.type must contain strings"))?;
    if first == second
        || !supported(first)
        || !supported(second)
        || !matches!((first, second), ("null", _) | (_, "null"))
    {
        bail!("ai_tool_schema_invalid: {path}.type nullable union is unsupported");
    }
    Ok(Some(
        if first == "null" { second } else { first }.to_owned(),
    ))
}

fn provider_schema_type_accepts(schema_type: Option<&Value>, value: &Value) -> bool {
    let accepts = |schema_type: &str| match schema_type {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    };
    match schema_type {
        None => true,
        Some(Value::String(schema_type)) => accepts(schema_type),
        Some(Value::Array(schema_types)) => {
            schema_types.iter().filter_map(Value::as_str).any(accepts)
        }
        Some(_) => false,
    }
}

fn fit_request_to_input_ceiling(
    request: &mut Value,
    initial: &Value,
    turns: &mut VecDeque<Vec<Value>>,
    maximum_input: usize,
) -> Result<()> {
    while serde_json::to_vec(&request)?.len() > maximum_input && !turns.is_empty() {
        turns.pop_front();
        let mut reduced = vec![initial.clone()];
        for turn in turns.iter() {
            reduced.extend(turn.iter().cloned());
        }
        request["input"] = Value::Array(reduced);
    }
    if serde_json::to_vec(&request)?.len() > maximum_input {
        bail!("hosted agent context exceeds the execution input-token ceiling");
    }
    Ok(())
}

fn phase_request_input_ceiling(phase: ExecutionPhase, signed_maximum: usize) -> usize {
    if matches!(
        phase,
        ExecutionPhase::Discovery | ExecutionPhase::ArtifactRepair
    ) {
        signed_maximum.min(MAX_DISCOVERY_REQUEST_BYTES)
    } else {
        signed_maximum
    }
}

fn hosted_budget_advisory(used: usize, limit: usize) -> Option<(u8, &'static str, &'static str)> {
    let percent = used.saturating_mul(100) / limit.max(1);
    if percent >= 90 {
        Some((
            90,
            "execution_budget_finalization",
            "The signed execution budget is at least 90% consumed. Stop broad exploration, continue the current implementation, and produce the smallest complete validated result.",
        ))
    } else if percent >= 70 {
        Some((
            70,
            "execution_budget_constrained",
            "The signed execution budget is at least 70% consumed. Continue from the notebook and existing diff, avoid repeated reads, and prioritize remaining acceptance criteria.",
        ))
    } else {
        None
    }
}

fn compact_hosted_turns(turns: &mut VecDeque<Vec<Value>>) {
    while turns.len() > MAX_HOSTED_TURN_WINDOWS {
        turns.pop_front();
    }
}

fn compact_notebook_for_phase(notebook: &WorkerNotebook, phase: ExecutionPhase) -> String {
    let validation = notebook
        .validation_evidence
        .iter()
        .rev()
        .take(8)
        .map(|evidence| {
            json!({
                "gate_id": evidence.gate_id,
                "status": evidence.status,
                "command_fingerprint": evidence.command_fingerprint,
                "source_tree_hash": evidence.source_tree_hash,
            })
        })
        .collect::<Vec<_>>();
    let value = match phase {
        ExecutionPhase::Discovery | ExecutionPhase::ArtifactRepair => json!({
            "goal": notebook.goal,
            "criteria_ids": notebook.acceptance_criteria_v2.iter().map(|criterion| &criterion.id).collect::<Vec<_>>(),
            "files_inspected": notebook.files_inspected,
            "searches_completed": notebook.searches_completed,
            "blocking_unknowns": notebook.blocking_unknowns,
        }),
        ExecutionPhase::Planning => {
            if let Some(repair) = &notebook.planning_repair {
                json!({
                    "instruction": "Repair only the listed invalid fields. Preserve the valid planned changes and call record_implementation_plan once without repeating discovery.",
                    "goal": notebook.goal,
                    "valid_planned_changes": repair.valid_planned_changes,
                    "invalid_fields": repair.invalid_fields,
                    "acceptance_criteria_ids": notebook.acceptance_criteria_v2.iter().map(|criterion| &criterion.id).collect::<Vec<_>>(),
                })
            } else {
                json!({
                    "goal": notebook.goal,
                    "criteria": notebook.acceptance_criteria_v2,
                    "impact_areas": notebook.impact_map.iter().map(|area| json!({
                        "area_id": area.area_id,
                        "candidate_paths": area.candidate_paths,
                        "acceptance_criteria_ids": area.acceptance_criteria_ids,
                    })).collect::<Vec<_>>(),
                    "blocking_unknowns": notebook.blocking_unknowns,
                })
            }
        }
        ExecutionPhase::Implementation | ExecutionPhase::Repair => json!({
            "goal": notebook.goal,
            "planned_changes": notebook.planned_changes,
            "intended_changes": notebook.intended_changes,
            "remaining_work": notebook.remaining_work_v2,
            "blocking_unknowns": notebook.blocking_unknowns,
            "recent_failures": notebook.failed_changes.iter().rev().take(4).collect::<Vec<_>>(),
            "validation": validation,
        }),
        ExecutionPhase::DiffReview | ExecutionPhase::CompletionEvaluation => json!({
            "goal": notebook.goal,
            "intended_changes": notebook.intended_changes,
            "remaining_work": notebook.remaining_work_v2,
            "required_gates": notebook.required_gates,
            "validation": validation,
        }),
        ExecutionPhase::Validation | ExecutionPhase::Publication => json!({
            "goal": notebook.goal,
            "remaining_work": notebook.remaining_work_v2,
            "required_gates": notebook.required_gates,
            "validation": validation,
        }),
    };
    truncate_text(
        &serde_json::to_string(&value).unwrap_or_else(|_| "{}".into()),
        28 * 1024,
    )
}

fn should_continue_implementation(
    existing_pull_request: bool,
    resumed_branch: bool,
    execution_attempt: i32,
) -> bool {
    !existing_pull_request || !resumed_branch || execution_attempt > 1
}

fn replace_unique_repo_text(
    root: &Path,
    path: &str,
    old_text: &str,
    new_text: &str,
) -> Result<String> {
    let target = safe_repo_path(root, path, false)?;
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    let positions = content
        .match_indices(old_text)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let matches = positions.len();
    if matches != 1 {
        bail!(
            "replace_match_not_unique: replace_text requires exactly one match in {path}; found {matches}"
        );
    }
    let before_sha256 = sha256_text(&content);
    let start_line = content[..positions[0]]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    let end_line = start_line + old_text.lines().count().max(1) - 1;
    let updated = content.replacen(old_text, new_text, 1);
    if updated.len() > MAX_MODEL_FILE_BYTES {
        bail!("replace_text result exceeds the hosted tool limit");
    }
    fs::write(&target, updated.as_bytes())
        .with_context(|| format!("could not write repository file {path}"))?;
    mutation_output(
        path,
        Some(before_sha256),
        Some(sha256_text(&updated)),
        format!("{start_line}-{end_line}"),
        format!(
            "replaced {} bytes with {} bytes",
            old_text.len(),
            new_text.len()
        ),
    )
}

fn sha256_text(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn mutation_output(
    path: &str,
    before_sha256: Option<String>,
    after_sha256: Option<String>,
    changed_range: String,
    diff_summary: String,
) -> Result<String> {
    serde_json::to_string(&json!({
        "path": path,
        "before_sha256": before_sha256,
        "after_sha256": after_sha256,
        "changed_range": changed_range,
        "diff_summary": diff_summary,
    }))
    .context("could not serialize mutation result")
}

fn write_repo_file(root: &Path, path: &str, content: &str, small_only: bool) -> Result<String> {
    let target = safe_repo_path(root, path, true)?;
    let previous = fs::read_to_string(&target).ok();
    if small_only
        && previous
            .as_ref()
            .is_none_or(|value| value.len() > MAX_SMALL_FILE_REWRITE_BYTES)
    {
        bail!(
            "rewrite_small_file requires an existing UTF-8 file no larger than {MAX_SMALL_FILE_REWRITE_BYTES} bytes"
        );
    }
    if content.len() > MAX_MODEL_FILE_BYTES
        || (small_only && content.len() > MAX_SMALL_FILE_REWRITE_BYTES)
    {
        bail!("complete file content exceeds the hosted tool limit");
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("could not create repository directory {}", parent.display())
        })?;
    }
    fs::write(&target, content.as_bytes())
        .with_context(|| format!("could not write repository file {path}"))?;
    mutation_output(
        path,
        previous.as_deref().map(sha256_text),
        Some(sha256_text(content)),
        "complete_file".into(),
        format!(
            "{} complete UTF-8 file with {} bytes",
            if small_only { "rewrote" } else { "wrote" },
            content.len()
        ),
    )
}

fn replace_repo_range(
    root: &Path,
    path: &str,
    start_line: usize,
    end_line: usize,
    new_text: &str,
) -> Result<String> {
    if start_line == 0 || end_line < start_line {
        bail!("replace_range requires a valid inclusive line range");
    }
    let target = safe_repo_path(root, path, false)?;
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    let lines = content.split_inclusive('\n').collect::<Vec<_>>();
    if end_line > lines.len().max(1) {
        bail!("replace_range line range exceeds {path}");
    }
    let start_offset = lines
        .iter()
        .take(start_line - 1)
        .map(|line| line.len())
        .sum::<usize>();
    let end_offset = lines
        .iter()
        .take(end_line)
        .map(|line| line.len())
        .sum::<usize>();
    let mut updated = String::with_capacity(
        content
            .len()
            .saturating_sub(end_offset.saturating_sub(start_offset))
            .saturating_add(new_text.len()),
    );
    updated.push_str(&content[..start_offset]);
    updated.push_str(new_text);
    updated.push_str(&content[end_offset..]);
    if updated.len() > MAX_MODEL_FILE_BYTES {
        bail!("replace_range result exceeds the hosted tool limit");
    }
    fs::write(&target, updated.as_bytes())
        .with_context(|| format!("could not write repository file {path}"))?;
    mutation_output(
        path,
        Some(sha256_text(&content)),
        Some(sha256_text(&updated)),
        format!("{start_line}-{end_line}"),
        format!("replaced inclusive line range {start_line}-{end_line}"),
    )
}

fn insert_relative_to_symbol(
    root: &Path,
    path: &str,
    symbol: &str,
    inserted: &str,
    after: bool,
) -> Result<String> {
    let target = safe_repo_path(root, path, false)?;
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    let positions = content
        .match_indices(symbol)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if positions.len() != 1 {
        bail!(
            "symbol_match_not_unique: symbol insertion requires one match in {path}; found {}",
            positions.len()
        );
    }
    let offset = positions[0] + usize::from(after) * symbol.len();
    let mut updated = String::with_capacity(content.len().saturating_add(inserted.len()));
    updated.push_str(&content[..offset]);
    updated.push_str(inserted);
    updated.push_str(&content[offset..]);
    if updated.len() > MAX_MODEL_FILE_BYTES {
        bail!("symbol insertion result exceeds the hosted tool limit");
    }
    fs::write(&target, updated.as_bytes())
        .with_context(|| format!("could not write repository file {path}"))?;
    let line = content[..positions[0]]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    mutation_output(
        path,
        Some(sha256_text(&content)),
        Some(sha256_text(&updated)),
        line.to_string(),
        format!(
            "inserted {} bytes {} unique symbol",
            inserted.len(),
            if after { "after" } else { "before" }
        ),
    )
}

fn apply_repo_unified_diff(root: &Path, path: &str, patch: &str) -> Result<String> {
    let target = safe_repo_path(root, path, false)?;
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    let expected_diff = format!("diff --git a/{path} b/{path}");
    let expected_old = format!("--- a/{path}");
    let expected_new = format!("+++ b/{path}");
    let diff_headers = patch
        .lines()
        .filter(|line| line.starts_with("diff --git "))
        .collect::<Vec<_>>();
    let old_headers = patch
        .lines()
        .filter(|line| line.starts_with("--- "))
        .collect::<Vec<_>>();
    let new_headers = patch
        .lines()
        .filter(|line| line.starts_with("+++ "))
        .collect::<Vec<_>>();
    let unsafe_metadata = patch.lines().any(|line| {
        [
            "rename from ",
            "rename to ",
            "copy from ",
            "copy to ",
            "new file mode ",
            "deleted file mode ",
            "old mode ",
            "new mode ",
            "GIT binary patch",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
    });
    if (!diff_headers.is_empty() && diff_headers != [expected_diff.as_str()])
        || old_headers != [expected_old.as_str()]
        || new_headers != [expected_new.as_str()]
        || unsafe_metadata
    {
        bail!("apply_unified_diff must modify exactly the declared existing path");
    }
    if patch.len() > MAX_MODEL_FILE_BYTES {
        bail!("unified diff exceeds the hosted tool limit");
    }
    let patch_path = env::temp_dir().join(format!(
        "rustgrid-agent-unified-diff-{}.patch",
        Uuid::new_v4().simple()
    ));
    fs::write(&patch_path, patch.as_bytes()).context("could not write patch file")?;
    let patch_path_text = patch_path.to_string_lossy().into_owned();
    let checked = command::checked(
        "git",
        [
            "apply",
            "--check",
            "--whitespace=nowarn",
            patch_path_text.as_str(),
        ],
        root,
    )
    .context("unified diff validation failed")
    .and_then(|_| {
        command::checked(
            "git",
            ["apply", "--whitespace=nowarn", patch_path_text.as_str()],
            root,
        )
        .context("unified diff application failed")
    });
    let _ = fs::remove_file(&patch_path);
    checked?;
    let updated = fs::read_to_string(&target)
        .with_context(|| format!("could not read patched UTF-8 repository file {path}"))?;
    mutation_output(
        path,
        Some(sha256_text(&content)),
        Some(sha256_text(&updated)),
        "unified_diff".into(),
        format!("applied {}-byte unified diff", patch.len()),
    )
}

fn delete_repo_file(root: &Path, path: &str) -> Result<String> {
    let target = safe_repo_path(root, path, false)?;
    if !target.is_file() {
        bail!("delete_file target is not a regular file");
    }
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    fs::remove_file(&target).with_context(|| format!("could not delete repository file {path}"))?;
    mutation_output(
        path,
        Some(sha256_text(&content)),
        None,
        "complete_file".into(),
        format!("deleted {}-byte file", content.len()),
    )
}

fn validate_model_command(value: &str) -> Result<()> {
    if value.contains('\n') || value.contains('\r') {
        bail!("focused model command must be one direct command without shell syntax");
    }
    let parts = command::parse(value)?;
    if parts.iter().any(|part| {
        matches!(part.as_str(), "&&" | "||" | "|" | ";" | "<" | ">")
            || part.starts_with("<<")
            || part.starts_with(">>")
    }) {
        bail!(
            "focused model command runs without a shell; operators, redirects, heredocs, and command chaining are unsupported"
        );
    }
    let program = Path::new(&parts[0])
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        program.as_str(),
        "gh" | "curl" | "wget" | "ssh" | "scp" | "nc" | "netcat"
    ) {
        bail!("focused model command cannot access external credential or network tools");
    }
    if program == "git"
        && parts.get(1).is_some_and(|part| {
            matches!(
                part.as_str(),
                "add"
                    | "branch"
                    | "checkout"
                    | "clean"
                    | "commit"
                    | "config"
                    | "fetch"
                    | "merge"
                    | "pull"
                    | "push"
                    | "rebase"
                    | "remote"
                    | "reset"
                    | "restore"
                    | "switch"
                    | "tag"
            )
        })
    {
        bail!("focused model command cannot mutate or publish Git state");
    }
    Ok(())
}

fn safe_repo_path(root: &Path, value: &str, allow_missing: bool) -> Result<PathBuf> {
    let relative = Path::new(value);
    if relative.is_absolute() {
        bail!("repository tool path must be relative");
    }
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) if name != ".git" => normalized.push(name),
            Component::Normal(_) => bail!("repository tools cannot access .git"),
            _ => bail!("repository tool path cannot escape the checkout"),
        }
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    let root = root
        .canonicalize()
        .context("could not canonicalize repository root")?;
    let candidate = root.join(&normalized);
    let mut cursor = root.clone();
    for component in normalized.components() {
        cursor.push(component.as_os_str());
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("repository tools cannot traverse symbolic links")
            }
            Ok(_) => {}
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                break;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not inspect repository path {value}"));
            }
        }
    }
    if candidate.exists() {
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("could not canonicalize repository path {value}"))?;
        if !canonical.starts_with(&root) {
            bail!("repository tool path escaped the checkout");
        }
    }
    Ok(candidate)
}

fn collect_repo_files(root: &Path, start: &Path, maximum: usize) -> Result<Vec<String>> {
    let root = root.canonicalize()?;
    let mut pending = VecDeque::from([start.to_path_buf()]);
    let mut files = Vec::new();
    while let Some(directory) = pending.pop_front() {
        let mut entries = fs::read_dir(&directory)
            .with_context(|| format!("could not list {}", directory.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if metadata.is_dir() {
                if matches!(
                    name.as_ref(),
                    ".git"
                        | "node_modules"
                        | "target"
                        | "dist"
                        | "build"
                        | "coverage"
                        | ".next"
                        | ".turbo"
                        | "vendor"
                ) {
                    continue;
                }
                pending.push_back(path);
            } else if metadata.is_file() {
                files.push(
                    path.strip_prefix(&root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                );
                if files.len() >= maximum {
                    files.push(format!("[file list truncated at {maximum} entries]"));
                    return Ok(files);
                }
            }
        }
    }
    Ok(files)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FileReadStatus {
    Success,
    Error,
}

fn read_error_progress_class(error: &str) -> ToolProgressClass {
    if error.contains("repository_access_failed") {
        ToolProgressClass::BlockingFailure
    } else if error.contains("duplicate") {
        ToolProgressClass::Duplicate
    } else {
        ToolProgressClass::RecoverableFailure
    }
}

fn successful_read_progress(
    tool: &str,
    new_file: bool,
    new_range: bool,
    new_related_test: bool,
    partial_failure: bool,
) -> (ToolProgressClass, &'static str) {
    if new_file || new_range {
        (
            ToolProgressClass::Productive,
            "new planned repository content inspected",
        )
    } else if tool == "related_tests" && new_related_test {
        (
            ToolProgressClass::Productive,
            "new related test file identified",
        )
    } else if partial_failure {
        (
            ToolProgressClass::RecoverableFailure,
            "batch read returned recoverable per-file errors",
        )
    } else {
        (
            ToolProgressClass::Duplicate,
            "repository content was already inspected",
        )
    }
}

#[derive(Clone, Debug, Serialize)]
struct FileReadResult {
    path: String,
    status: FileReadStatus,
    content: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
    line_count: Option<u32>,
    file_size: Option<u64>,
    valid_line_range: Option<String>,
    truncated: bool,
    fallback_attempted: bool,
}

#[derive(Clone, Debug, Serialize)]
struct BatchReadResult {
    files: Vec<FileReadResult>,
}

#[derive(Clone, Debug)]
struct PrevalidatedRepoFile {
    requested_path: String,
    resolved_path: PathBuf,
    file_size: u64,
}

#[derive(Clone, Debug)]
enum PrevalidatedBatchReadPath {
    Ready(PrevalidatedRepoFile),
    Rejected {
        result: FileReadResult,
        fallback_path: Option<String>,
    },
}

fn failed_file_read(
    path: &str,
    code: &str,
    message: impl Into<String>,
    file_size: Option<u64>,
    line_count: Option<u32>,
) -> FileReadResult {
    FileReadResult {
        path: path.to_owned(),
        status: FileReadStatus::Error,
        content: None,
        error_code: Some(code.to_owned()),
        error_message: Some(message.into()),
        line_count,
        file_size,
        valid_line_range: line_count.map(|count| format!("1-{}", count.max(1))),
        truncated: false,
        fallback_attempted: false,
    }
}

fn prevalidate_repo_file(
    root: &Path,
    value: &str,
) -> std::result::Result<PrevalidatedRepoFile, Box<FileReadResult>> {
    let path = match safe_repo_path(root, value, false) {
        Ok(path) => path,
        Err(error) => {
            let message = error.to_string();
            let code = if message.contains("could not inspect repository path") {
                "path_not_found"
            } else if message.contains("symbolic")
                || message.contains("escape")
                || message.contains("absolute")
                || message.contains("cannot access .git")
                || message.contains("must be relative")
            {
                "path_not_allowed"
            } else {
                "repository_access_failed"
            };
            return Err(Box::new(failed_file_read(value, code, message, None, None)));
        }
    };
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Box::new(failed_file_read(
                value,
                "path_not_found",
                format!("repository path `{value}` does not exist"),
                None,
                None,
            )));
        }
        Err(error) => {
            return Err(Box::new(failed_file_read(
                value,
                "repository_access_failed",
                format!("could not inspect repository file `{value}`: {error}"),
                None,
                None,
            )));
        }
    };
    if !metadata.is_file() {
        return Err(Box::new(failed_file_read(
            value,
            "not_regular_file",
            format!("repository path `{value}` is not a regular file"),
            Some(metadata.len()),
            None,
        )));
    }
    if metadata.len() > MAX_MODEL_FILE_BYTES as u64 {
        return Err(Box::new(failed_file_read(
            value,
            "file_too_large",
            format!(
                "repository file `{value}` is {} bytes; the read limit is {MAX_MODEL_FILE_BYTES} bytes",
                metadata.len()
            ),
            Some(metadata.len()),
            None,
        )));
    }
    Ok(PrevalidatedRepoFile {
        requested_path: value.to_owned(),
        resolved_path: path,
        file_size: metadata.len(),
    })
}

fn read_prevalidated_repo_file_result(
    file: &PrevalidatedRepoFile,
    start_line: u64,
    end_line: u64,
    maximum_output_bytes: usize,
) -> FileReadResult {
    let value = file.requested_path.as_str();
    let bytes = match fs::read(&file.resolved_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return failed_file_read(
                value,
                "repository_access_failed",
                format!("could not read repository file `{value}`: {error}"),
                Some(file.file_size),
                None,
            );
        }
    };
    if bytes.contains(&0) {
        return failed_file_read(
            value,
            "binary_file",
            format!("repository file `{value}` is binary and cannot be exposed"),
            Some(file.file_size),
            None,
        );
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return failed_file_read(
                value,
                "not_utf8",
                format!("repository file `{value}` is not valid UTF-8"),
                Some(file.file_size),
                None,
            );
        }
    };
    let lines = text.lines().collect::<Vec<_>>();
    let line_count = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    if start_line == 0 || end_line < start_line || start_line > u64::from(line_count.max(1)) {
        return failed_file_read(
            value,
            "line_range_invalid",
            format!(
                "requested lines {start_line}-{end_line} are outside `{value}`; valid line range is 1-{}",
                line_count.max(1)
            ),
            Some(file.file_size),
            Some(line_count),
        );
    }
    let mut output = String::new();
    let mut truncated = false;
    for (index, line) in lines.iter().enumerate() {
        let line_number = index as u64 + 1;
        if line_number < start_line {
            continue;
        }
        if line_number > end_line {
            break;
        }
        let formatted = format!("{line_number:>6} | {line}\n");
        if output.len().saturating_add(formatted.len()) > maximum_output_bytes {
            output.push_str("[read output truncated]\n");
            truncated = true;
            break;
        }
        output.push_str(&formatted);
    }
    FileReadResult {
        path: value.to_owned(),
        status: FileReadStatus::Success,
        content: Some(output),
        error_code: None,
        error_message: None,
        line_count: Some(line_count),
        file_size: Some(file.file_size),
        valid_line_range: Some(format!("1-{}", line_count.max(1))),
        truncated,
        fallback_attempted: false,
    }
}

fn read_repo_file_result(
    root: &Path,
    value: &str,
    start_line: u64,
    end_line: u64,
    maximum_output_bytes: usize,
) -> FileReadResult {
    match prevalidate_repo_file(root, value) {
        Ok(file) => {
            read_prevalidated_repo_file_result(&file, start_line, end_line, maximum_output_bytes)
        }
        Err(result) => *result,
    }
}

fn prevalidate_batch_read_paths(root: &Path, paths: &[Value]) -> Vec<PrevalidatedBatchReadPath> {
    paths
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let Some(path) = value
                .as_str()
                .filter(|path| !path.is_empty() && path.len() <= 4_096)
            else {
                return PrevalidatedBatchReadPath::Rejected {
                    result: failed_file_read(
                        &format!("<paths[{index}]>"),
                        "path_malformed",
                        "read_files path must be a non-empty repository-relative string",
                        None,
                        None,
                    ),
                    fallback_path: None,
                };
            };
            match prevalidate_repo_file(root, path) {
                Ok(file) => PrevalidatedBatchReadPath::Ready(file),
                Err(result) => PrevalidatedBatchReadPath::Rejected {
                    result: *result,
                    fallback_path: Some(path.to_owned()),
                },
            }
        })
        .collect()
}

fn read_prevalidated_repo_files_with_fallback(
    root: &Path,
    paths: &[PrevalidatedBatchReadPath],
    maximum_lines: u64,
    maximum_output_bytes: usize,
) -> (BatchReadResult, u32) {
    let per_file_bytes = (maximum_output_bytes / paths.len().max(1)).max(1_024);
    let mut fallback_paths = Vec::with_capacity(paths.len());
    let mut files = paths
        .iter()
        .map(|path| match path {
            PrevalidatedBatchReadPath::Ready(file) => {
                fallback_paths.push(Some(file.requested_path.clone()));
                read_prevalidated_repo_file_result(file, 1, maximum_lines, per_file_bytes)
            }
            PrevalidatedBatchReadPath::Rejected {
                result,
                fallback_path,
            } => {
                fallback_paths.push(fallback_path.clone());
                result.clone()
            }
        })
        .collect::<Vec<_>>();
    let initial_failures = u32::try_from(
        files
            .iter()
            .filter(|result| result.status == FileReadStatus::Error)
            .count(),
    )
    .unwrap_or(u32::MAX);
    for (result, fallback_path) in files.iter_mut().zip(fallback_paths) {
        if result.status != FileReadStatus::Error {
            continue;
        }
        let Some(fallback_path) = fallback_path else {
            continue;
        };
        let mut fallback =
            read_repo_file_result(root, &fallback_path, 1, maximum_lines, per_file_bytes);
        fallback.fallback_attempted = true;
        if fallback.status == FileReadStatus::Error
            && let Some(message) = fallback.error_message.as_mut()
        {
            message.push_str("; individual read fallback also failed");
        }
        *result = fallback;
    }
    (BatchReadResult { files }, initial_failures)
}

struct SearchResult {
    output: String,
    truncated: bool,
    matched_paths: Vec<String>,
}

fn search_repo(
    root: &Path,
    value: &str,
    query: &str,
    extensions: &[String],
    context_lines: u64,
    maximum_new_consumers: Option<usize>,
    known_consumers: &BTreeSet<String>,
) -> Result<SearchResult> {
    if query.is_empty() || query.contains('\0') {
        bail!("search query is invalid");
    }
    let start = safe_repo_path(root, value, false)?;
    let candidates = if start.is_file() {
        vec![
            start
                .strip_prefix(root)
                .unwrap_or(&start)
                .to_string_lossy()
                .into_owned(),
        ]
    } else {
        collect_repo_files(root, &start, 2_000)?
    };
    let normalized_extensions = extensions
        .iter()
        .map(|extension| extension.trim_start_matches('.'))
        .collect::<BTreeSet<_>>();
    let mut groups = Vec::<(String, Vec<(usize, String)>, usize)>::new();
    let mut new_consumer_count = 0_usize;
    let mut truncated = false;
    for candidate in candidates {
        if candidate.starts_with('[') {
            truncated = true;
            continue;
        }
        if !normalized_extensions.is_empty()
            && Path::new(&candidate)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_none_or(|extension| !normalized_extensions.contains(extension))
        {
            continue;
        }
        let path = safe_repo_path(root, &candidate, false)?;
        let metadata = fs::metadata(&path)?;
        if metadata.len() > MAX_MODEL_FILE_BYTES as u64 {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let lines = text.lines().collect::<Vec<_>>();
        let mut file_matches = Vec::new();
        let mut file_count = 0usize;
        for (index, line) in lines.iter().enumerate() {
            if line.contains(query) {
                file_count = file_count.saturating_add(1);
                if file_matches.len() < 3 {
                    let start = index.saturating_sub(context_lines as usize);
                    let end = (index + context_lines as usize + 1).min(lines.len());
                    let excerpt = lines[start..end]
                        .iter()
                        .enumerate()
                        .map(|(offset, excerpt)| {
                            format!(
                                "{:>6} | {}",
                                start + offset + 1,
                                truncate_text(excerpt, 1_000)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    file_matches.push((index + 1, excerpt));
                }
            }
        }
        if file_count > 0 {
            if !localized_discovery_core_path(&candidate) && !known_consumers.contains(&candidate) {
                if maximum_new_consumers.is_some_and(|maximum| new_consumer_count >= maximum) {
                    truncated = true;
                    break;
                }
                new_consumer_count = new_consumer_count.saturating_add(1);
            }
            groups.push((candidate, file_matches, file_count));
        }
        if groups.len() >= 40 {
            truncated = true;
            break;
        }
    }
    let total_matches = groups.iter().map(|group| group.2).sum::<usize>();
    let matched_paths = groups
        .iter()
        .map(|group| group.0.clone())
        .collect::<Vec<_>>();
    let mut output = format!(
        "search_summary: {total_matches} match(es) across {} file(s)\n",
        groups.len()
    );
    for (candidate, excerpts, file_count) in groups {
        output.push_str(&format!("\n{candidate} ({file_count} matches)\n"));
        for (line, excerpt) in excerpts {
            output.push_str(&format!(
                "  representative match at line {line}\n{excerpt}\n"
            ));
            if output.len() >= MAX_TOOL_OUTPUT_BYTES {
                truncated = true;
                break;
            }
        }
        if output.len() >= MAX_TOOL_OUTPUT_BYTES {
            break;
        }
    }
    if output.is_empty() {
        output.push_str("no matches\n");
    }
    if truncated {
        output.push_str("[search output truncated]\n");
    }
    Ok(SearchResult {
        output: truncate_text(&output, MAX_TOOL_OUTPUT_BYTES),
        truncated,
        matched_paths,
    })
}

fn truncate_text(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let suffix = "\n[truncated]";
    let mut end = maximum.saturating_sub(suffix.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &value[..end], suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        net::TcpListener,
        sync::mpsc::{self, Receiver},
    };

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4_096];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&bytes);
            let Some(header_end) = text.find("\r\n\r\n") else {
                continue;
            };
            let content_length = text[..header_end]
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + content_length {
                break;
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn one_request_server(
        status: &str,
        body: Value,
    ) -> Option<(Url, Receiver<String>, thread::JoinHandle<()>)> {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("test HTTP server should bind: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let status = status.to_owned();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let _ = sender.send(request);
            let body = body.to_string();
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        (
            Url::parse(&format!("http://{address}/")).unwrap(),
            receiver,
            handle,
        )
            .into()
    }

    fn delayed_no_response_server(
        delay: Duration,
    ) -> Option<(Url, Receiver<String>, thread::JoinHandle<()>)> {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("test HTTP server should bind: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let _ = sender.send(request);
            thread::sleep(delay);
            listener.set_nonblocking(true).unwrap();
            loop {
                match listener.accept() {
                    Ok((_stream, _)) => {
                        let _ = sender.send("additional request".into());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        panic!("test HTTP server should inspect queued requests: {error}")
                    }
                }
            }
        });
        Some((
            Url::parse(&format!("http://{address}/")).unwrap(),
            receiver,
            handle,
        ))
    }

    fn request_sequence_server(
        responses: Vec<(&'static str, Value)>,
    ) -> Option<(Url, Receiver<String>, thread::JoinHandle<()>)> {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("test HTTP server should bind: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                let _ = sender.send(request);
                let body = body.to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        Some((
            Url::parse(&format!("http://{address}/")).unwrap(),
            receiver,
            handle,
        ))
    }

    fn exchange_response(execution_id: Uuid) -> ExchangeResponse {
        ExchangeResponse {
            access_token: format!("rge_{}", "a".repeat(48)),
            token_type: "Bearer".into(),
            expires_in: 900,
            expires_at: "2099-01-01T00:00:00Z".into(),
            token_id: Uuid::from_u128(30),
            tenant_id: Uuid::from_u128(31),
            project_id: Uuid::from_u128(32),
            execution_id,
            execution_attempt: 1,
            session_id: Uuid::from_u128(33),
            worker_id: Uuid::from_u128(34),
            repository_id: 7,
            github_workflow_run_id: 88,
            permissions: EXECUTION_PERMISSIONS.map(str::to_owned).to_vec(),
        }
    }

    fn test_api_client(api_root: Url, execution_id: Uuid) -> HostedApiClient {
        HostedApiClient::from_exchange(
            hosted_http_client().unwrap(),
            api_root.join("api/v1/").unwrap(),
            execution_id,
            exchange_response(execution_id),
        )
        .unwrap()
    }

    fn test_hosted_http_error(
        status: StatusCode,
        code: &str,
        upstream_provider_status: Option<u16>,
        provider_contacted: Option<bool>,
    ) -> HostedHttpError {
        HostedHttpError {
            status,
            path: "executions/id/ai/responses".into(),
            code: code.into(),
            request_id: Some("request-1".into()),
            rustgrid_gateway_status: None,
            upstream_provider_status,
            failure_stage: None,
            provider_contacted,
            call_budget_consumed: None,
            reservation_state: None,
            reservation_reconciliation_state: None,
            retryable: None,
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
        }
    }

    fn test_execution_failure(code: &str, message: &str) -> HostedAgentExecutionFailure {
        HostedAgentExecutionFailure {
            status: "failed",
            category: "hosted_agent_execution_failed",
            process_health: "failed",
            mission_outcome: "failed",
            blocker: None,
            resumable: true,
            code: code.into(),
            phase: ExecutionPhase::Discovery,
            message: message.into(),
            underlying_error: UnderlyingFailure {
                r#type: "orchestration_guardrail".into(),
                message: code.into(),
                stack_reference: None,
            },
            model_calls_used: 0,
            model_calls_limit: 40,
            model_calls_remaining: 40,
            phase_calls_used: 0,
            phase_calls_limit: 8,
            last_successful_action: json!({}),
            usage: ToolUsage::default(),
            estimated_cost_micros: 0,
            input_tokens: 0,
            output_tokens: 0,
            changed_paths: Vec::new(),
            remaining_work: Vec::new(),
            failed_tool_operations: Vec::new(),
            current_plan: Vec::new(),
            validation_evidence: Vec::new(),
            notebook_revision: 0,
            recoverable: true,
            resume_phase: "discovery".into(),
            recommended_action: "Inspect the authoritative failure details.".into(),
            artifact: None,
            semantic_status: None,
            persistence_status: None,
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
        }
    }

    fn test_environment(execution_id: Uuid) -> GithubActionsEnvironment {
        let _ = execution_id;
        GithubActionsEnvironment {
            api_root: Url::parse("http://127.0.0.1:8080/api/v1/").unwrap(),
            audience: "http://127.0.0.1:8080".into(),
            oidc_request_url: Url::parse("http://127.0.0.1:8081/oidc").unwrap(),
            oidc_request_token: SecretString::new("request-token".into(), "test").unwrap(),
            dispatch_nonce: SecretString::new("d".repeat(48), "test").unwrap(),
            repository: Some("RustGrid/example".into()),
            repository_id: Some(7),
            sha: Some("a".repeat(40)),
            workflow_run_id: Some(88),
            workflow_run_attempt: Some(1),
            actor: Some("octocat".into()),
            actor_id: Some(583_231),
        }
    }

    fn test_manifest(execution_id: Uuid) -> HostedManifest {
        let policy = HostedExecutionPolicy {
            policy_version: 1,
            codex: HostedCodexPolicy {
                command: vec!["codex".into(), "exec".into(), "--json".into()],
                environment_allowlist: vec![
                    "PATH".into(),
                    "HOME".into(),
                    "CARGO_HOME".into(),
                    "RUSTUP_HOME".into(),
                ],
            },
            quality_gates: vec![HostedQualityGate {
                id: "test".into(),
                command: "cargo test".into(),
                timeout_seconds: 900,
                required: true,
            }],
            timeout_seconds: 3_600,
            sandbox: HostedSandboxPolicy {
                mode: "workspace_write".into(),
                network_access: true,
                writable_roots: vec![".".into()],
                approval_policy: "never".into(),
            },
        };
        let policy_hash = hex::encode(Sha256::digest(serde_json::to_vec(&policy).unwrap()));
        let base = format!("/api/v1/executions/{execution_id}");
        HostedManifest {
            manifest_version: 3,
            model_call_budget: None,
            requested_model_call_budget: None,
            resolved_model_call_budget: None,
            budget_source: None,
            clamped: None,
            clamp_reason: None,
            execution: ManifestExecution {
                execution_id,
                status: "running".into(),
                attempt_number: 1,
                model: Some("gpt-5.6-sol".into()),
                maximum_input_tokens: Some(100_000),
                maximum_output_tokens: Some(8_000),
                maximum_model_calls: Some(12),
                maximum_duration_seconds: Some(3_600),
                maximum_cost_usd: Some("5.00".into()),
                github_actions: Some(ManifestGithubActionsExecution {
                    workflow_run_id: Some(88),
                    workflow_run_attempt: Some(1),
                }),
            },
            run: ManifestRun {
                id: execution_id,
                ticket_id: Uuid::from_u128(2),
                input_prompt: "Implement the bounded mission.".into(),
                attempt: 1,
                metadata: json!({}),
            },
            project_id: Uuid::from_u128(32),
            project_key: "RG".into(),
            project_name: "RustGrid".into(),
            ticket_id: Uuid::from_u128(2),
            ticket_key: "RG-7".into(),
            ticket_title: "Hosted execution".into(),
            github: HostedGithubManifest {
                repository_id: 7,
                repository: "RustGrid/example".into(),
                clone_url: "https://github.com/RustGrid/example.git".into(),
                web_base_url: "https://github.com".into(),
                installation_id: 42,
                base_ref: "main".into(),
                base_sha: "a".repeat(40),
                branch: format!("rustgrid/rg-7-{}", &execution_id.simple().to_string()[..8]),
                github_token_url: format!("{base}/github-token"),
            },
            ai_gateway: HostedAiManifest {
                responses_url: format!("{base}/ai/responses"),
                model: "gpt-5.6-sol".into(),
                maximum_input_tokens: 100_000,
                maximum_output_tokens: 8_000,
                maximum_model_calls: 12,
                maximum_cost_usd: "5.00".into(),
            },
            execution_policy: policy,
            execution_policy_sha256: policy_hash,
            heartbeat_url: format!("{base}/heartbeat"),
            token_refresh_url: format!("{base}/token/refresh"),
            events_url: format!("{base}/worker-events"),
            telemetry_url: format!("{base}/telemetry/batch"),
            state_url: format!("{base}/state"),
            complete_url: format!("{base}/complete"),
        }
    }

    #[test]
    fn normalizes_hosted_api_roots_without_double_api_prefixes() {
        let production_root = normalize_api_root(DEFAULT_INSTANCE_URL).unwrap();
        assert_eq!(production_root.as_str(), "https://app.rustgrid.com/api/v1/");
        assert_eq!(
            production_root
                .join("execution-auth/github-actions/exchange")
                .unwrap()
                .as_str(),
            "https://app.rustgrid.com/api/v1/execution-auth/github-actions/exchange"
        );
        assert_eq!(
            normalize_api_root("https://app.rustgrid.com/api/v1")
                .unwrap()
                .as_str(),
            "https://app.rustgrid.com/api/v1/"
        );
        assert!(normalize_api_root("http://app.rustgrid.com").is_err());
        assert!(normalize_api_root("https://user:password@app.rustgrid.com").is_err());
        assert!(
            secure_github_oidc_url(
                "RUSTGRID_OIDC_REQUEST_URL",
                "https://pipelines.actions.githubusercontent.com/job/idtoken?api-version=2.0"
            )
            .is_ok()
        );
        assert!(
            secure_github_oidc_url(
                "RUSTGRID_OIDC_REQUEST_URL",
                "https://attacker.invalid/idtoken"
            )
            .is_err()
        );
        assert!(
            secure_github_oidc_url(
                "RUSTGRID_OIDC_REQUEST_URL",
                "https://pipelines.actions.githubusercontent.com/idtoken?audience=attacker"
            )
            .is_err()
        );
        assert!(validate_dispatch_nonce(&format!("rgdn_{}", "a".repeat(40))).is_ok());
        assert!(validate_dispatch_nonce(&"a".repeat(40)).is_err());
    }

    #[test]
    fn secrets_are_always_redacted_from_debug_output() {
        let secret = SecretString::new("rge_super-secret".into(), "test").unwrap();
        assert_eq!(format!("{secret:?}"), "<redacted>");
        assert!(!format!("{secret:?}").contains("super-secret"));
    }

    #[test]
    fn rejects_incomplete_or_mismatched_hosted_execution_identity() {
        let execution_id = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
        let mut incomplete = test_environment(execution_id);
        incomplete.workflow_run_attempt = None;
        assert!(incomplete.require_execute_context().is_err());
        let mut missing_sha = test_environment(execution_id);
        missing_sha.sha = None;
        assert!(missing_sha.require_execute_context().is_err());
        let mut missing_actor = test_environment(execution_id);
        missing_actor.actor = None;
        assert!(missing_actor.require_execute_context().is_err());
        let mut invalid_actor = test_environment(execution_id);
        invalid_actor.actor = Some("octocat@example.com".into());
        assert!(invalid_actor.require_execute_context().is_err());
        let mut missing_actor_id = test_environment(execution_id);
        missing_actor_id.actor_id = None;
        assert!(missing_actor_id.require_execute_context().is_err());

        let author = test_environment(execution_id).git_author().unwrap();
        assert_eq!(author.name, "octocat");
        assert_eq!(author.email, "583231+octocat@users.noreply.github.com");
        let mut bot_environment = test_environment(execution_id);
        bot_environment.actor = Some("rustgrid[bot]".into());
        bot_environment.actor_id = Some(123_456);
        let bot_author = bot_environment.git_author().unwrap();
        assert_eq!(bot_author.name, "rustgrid[bot]");
        assert_eq!(
            bot_author.email,
            "123456+rustgrid[bot]@users.noreply.github.com"
        );

        let mut wrong_permissions = exchange_response(execution_id);
        wrong_permissions.permissions.pop();
        assert!(
            HostedApiClient::from_exchange(
                hosted_http_client().unwrap(),
                Url::parse("http://127.0.0.1:8080/api/v1/").unwrap(),
                execution_id,
                wrong_permissions,
            )
            .is_err()
        );

        let mut environment = test_environment(execution_id);
        environment.workflow_run_id = Some(89);
        let api = test_api_client(Url::parse("http://127.0.0.1:8080/").unwrap(), execution_id);
        assert!(
            test_manifest(execution_id)
                .validate(execution_id, &environment, &api)
                .is_err()
        );
        let environment = test_environment(execution_id);
        let mut wrong_sha = test_manifest(execution_id);
        wrong_sha.github.base_sha = "b".repeat(40);
        assert!(
            wrong_sha
                .validate(execution_id, &environment, &api)
                .is_err()
        );
        let mut malformed_sha = test_manifest(execution_id);
        malformed_sha.github.base_sha = "not-a-commit".into();
        assert!(
            malformed_sha
                .validate(execution_id, &environment, &api)
                .is_err()
        );
    }

    #[test]
    fn cancelled_completion_omits_failure_fields_required_only_for_failures() {
        let cancelled = unsuccessful_completion(
            true,
            "execution_cancelled".into(),
            "The execution was cancelled.".into(),
        );
        let encoded = serde_json::to_value(cancelled).unwrap();
        assert_eq!(encoded["status"], "cancelled");
        assert!(encoded.get("failure_code").is_none());
        assert!(encoded.get("failure_message").is_none());
    }

    #[test]
    fn validates_the_v3_manifest_and_all_scoped_endpoints() {
        let execution_id = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
        let environment = test_environment(execution_id);
        let api = test_api_client(Url::parse("http://127.0.0.1:8080/").unwrap(), execution_id);
        let manifest = test_manifest(execution_id);
        manifest.validate(execution_id, &environment, &api).unwrap();

        let mut forty_call_manifest = manifest.clone();
        forty_call_manifest.execution.maximum_model_calls = Some(40);
        forty_call_manifest.ai_gateway.maximum_model_calls = 40;
        forty_call_manifest
            .validate(execution_id, &environment, &api)
            .unwrap();

        let mut undersized_manifest = manifest.clone();
        undersized_manifest.execution.maximum_model_calls = Some(9);
        undersized_manifest.ai_gateway.maximum_model_calls = 9;
        assert!(
            undersized_manifest
                .validate(execution_id, &environment, &api)
                .is_err()
        );

        let mut wrong_branch = manifest.clone();
        wrong_branch.github.branch = "rustgrid/other".into();
        assert!(
            wrong_branch
                .validate(execution_id, &environment, &api)
                .unwrap_err()
                .to_string()
                .contains("branch")
        );

        let mut wrong_gateway = manifest;
        wrong_gateway.ai_gateway.responses_url = "https://attacker.invalid/responses".into();
        assert!(
            wrong_gateway
                .validate(execution_id, &environment, &api)
                .is_err()
        );
    }

    #[test]
    fn mission_retry_attempt_is_independent_from_github_run_attempt() {
        let execution_id = Uuid::from_u128(0x22222222_2222_4222_8222_222222222222);
        let environment = test_environment(execution_id);
        let mut exchange = exchange_response(execution_id);
        exchange.execution_attempt = 2;
        let api = HostedApiClient::from_exchange(
            hosted_http_client().unwrap(),
            Url::parse("http://127.0.0.1:8080/api/v1/").unwrap(),
            execution_id,
            exchange,
        )
        .unwrap();
        let mut manifest = test_manifest(execution_id);
        manifest.execution.attempt_number = 2;
        manifest.run.attempt = 2;

        manifest.validate(execution_id, &environment, &api).unwrap();

        let mut wrong_workflow_attempt = environment;
        wrong_workflow_attempt.workflow_run_attempt = Some(2);
        assert!(
            manifest
                .validate(execution_id, &wrong_workflow_attempt, &api)
                .is_err()
        );
    }

    #[test]
    fn rejects_sensitive_execution_environments_and_publication_commands() {
        for name in [
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "RUSTGRID_EXECUTION_TOKEN",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "BASH_ENV",
            "NODE_OPTIONS",
            "GIT_CONFIG_COUNT",
        ] {
            assert!(!safe_child_environment_name(name), "{name}");
        }
        assert!(safe_child_environment_name("PATH"));
        assert!(validate_model_command("git diff -- src/lib.rs").is_ok());
        assert!(validate_model_command("git push origin branch").is_err());
        assert!(validate_model_command("curl https://example.com").is_err());
        assert!(validate_model_command("npm test && npm run build").is_err());
        assert!(validate_model_command("python3 - <<PY\nprint('no')\nPY").is_err());
        assert!(validate_model_command("sed -n 1,20p src/lib.rs").is_ok());
    }

    #[test]
    fn quality_gate_phase_telemetry_satisfies_the_completion_contract() {
        let execution_id = Uuid::from_u128(0x1234);
        let gate = HostedQualityGate {
            id: "cargo-test".into(),
            command: "cargo test --locked".into(),
            timeout_seconds: 900,
            required: true,
        };
        let started = quality_gate_phase_event(
            execution_id,
            &gate,
            3,
            2,
            "2026-07-27T10:00:00Z",
            None,
            ExecutionStatus::Running,
            1,
        );
        let completed = quality_gate_phase_event(
            execution_id,
            &gate,
            3,
            2,
            "2026-07-27T10:00:00Z",
            Some("2026-07-27T10:01:00Z"),
            ExecutionStatus::Succeeded,
            2,
        );

        assert_eq!(started.event_type, "phase.started");
        assert_eq!(completed.event_type, "phase.completed");
        assert_eq!(started.entity_revision, 1);
        assert_eq!(completed.entity_revision, 2);
        assert_ne!(started.event_id, completed.event_id);
        let (
            TelemetryPayload::Phase {
                phase: started_phase,
            },
            TelemetryPayload::Phase {
                phase: completed_phase,
            },
        ) = (&started.payload, &completed.payload)
        else {
            panic!("quality gate telemetry must use phase payloads");
        };
        assert_eq!(started_phase.id, completed_phase.id);
        assert_eq!(completed_phase.execution_id, execution_id);
        assert_eq!(completed_phase.name, "quality_gate:cargo-test");
        assert!(completed_phase.completed_at.is_some());
        assert!(matches!(completed_phase.status, ExecutionStatus::Succeeded));

        let replay = quality_gate_phase_event(
            execution_id,
            &gate,
            3,
            2,
            "2026-07-27T10:00:00Z",
            Some("2026-07-27T10:01:00Z"),
            ExecutionStatus::Succeeded,
            2,
        );
        assert_eq!(replay.event_id, completed.event_id);
    }

    #[test]
    fn quality_gate_phase_telemetry_posts_to_the_execution_contract() {
        let execution_id = Uuid::from_u128(0x5678);
        let Some((base, request, server)) = one_request_server("200 OK", json!({})) else {
            return;
        };
        let api = test_api_client(base, execution_id);
        let gate = HostedQualityGate {
            id: "verify".into(),
            command: "cargo test --locked".into(),
            timeout_seconds: 900,
            required: true,
        };
        send_quality_gate_phase_telemetry(
            &api,
            execution_id,
            &gate,
            1,
            1,
            "2026-07-27T10:00:00Z",
            Some("2026-07-27T10:01:00Z"),
            ExecutionStatus::Succeeded,
            2,
        )
        .unwrap();
        server.join().unwrap();
        let request = request.recv().unwrap();
        assert!(request.starts_with(&format!(
            "POST /api/v1/executions/{execution_id}/telemetry/batch HTTP/1.1"
        )));
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["telemetry_version"], TELEMETRY_VERSION);
        assert_eq!(body["events"][0]["type"], "phase.completed");
        assert_eq!(body["events"][0]["entity_revision"], 2);
        assert_eq!(
            body["events"][0]["phase"]["execution_id"],
            execution_id.to_string()
        );
        assert_eq!(body["events"][0]["phase"]["name"], "quality_gate:verify");
        assert_eq!(body["events"][0]["phase"]["status"], "succeeded");
        assert_eq!(
            body["events"][0]["phase"]["completed_at"],
            "2026-07-27T10:01:00Z"
        );
    }

    #[test]
    fn execution_policy_rejects_duplicate_quality_gate_phase_identities() {
        let execution_id = Uuid::from_u128(0x1234);
        let mut manifest = test_manifest(execution_id);
        manifest
            .execution_policy
            .quality_gates
            .push(manifest.execution_policy.quality_gates[0].clone());
        assert!(manifest.execution_policy.validate().is_err());
    }

    #[test]
    fn three_required_gates_execute_once_and_reuse_exact_tree_evidence() {
        assert_eq!(
            classify_validation_gate("focused-theme", "npm test -- theme"),
            ValidationGateType::FocusedTest
        );
        let directory = tempfile::tempdir().unwrap();
        command::checked("git", ["init", "-q"], directory.path()).unwrap();
        command::checked("git", ["config", "user.name", "Test"], directory.path()).unwrap();
        command::checked(
            "git",
            ["config", "user.email", "test@example.com"],
            directory.path(),
        )
        .unwrap();
        fs::write(directory.path().join("theme.ts"), "export {};\n").unwrap();
        command::checked("git", ["add", "theme.ts"], directory.path()).unwrap();
        command::checked(
            "git",
            [
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
            directory.path(),
        )
        .unwrap();
        let base_sha = command::checked("git", ["rev-parse", "HEAD"], directory.path()).unwrap();
        let execution_id = Uuid::from_u128(0xa0226);
        let mut manifest = test_manifest(execution_id);
        manifest.github.base_sha = base_sha;
        manifest.execution_policy.quality_gates = vec![
            HostedQualityGate {
                id: "focused-theme".into(),
                command: "npm test -- theme".into(),
                timeout_seconds: 30,
                required: true,
            },
            HostedQualityGate {
                id: "test".into(),
                command: "npm test".into(),
                timeout_seconds: 30,
                required: true,
            },
            HostedQualityGate {
                id: "build".into(),
                command: "npm run build".into(),
                timeout_seconds: 30,
                required: true,
            },
        ];
        let Some((api_root, requests, server)) = request_sequence_server(
            std::iter::repeat_with(|| ("200 OK", json!({})))
                .take(27)
                .collect(),
        ) else {
            return;
        };
        let api = test_api_client(api_root, execution_id);
        let repo = Repo {
            root: directory.path().to_path_buf(),
        };
        let running = Arc::new(AtomicBool::new(true));
        let validation_started = Instant::now();
        let execution_started = Instant::now();
        let mut ledger = Vec::new();
        let mut required_gates = Vec::new();
        let mut usage = ToolUsage::default();
        let executed_commands = Arc::new(Mutex::new(Vec::new()));

        let first = run_quality_gates_with_capture(
            &api,
            &manifest,
            &repo,
            &running,
            &manifest.execution_policy,
            1,
            &mut ledger,
            &mut required_gates,
            &mut usage,
            validation_started,
            execution_started,
            |command_text,
             cwd,
             running,
             timeout,
             max_output_bytes,
             environment_allowlist,
             limits| {
                executed_commands
                    .lock()
                    .unwrap()
                    .push(command_text.to_owned());
                command::capture_cancellable_with_environment(
                    "git rev-parse --is-inside-work-tree",
                    cwd,
                    running,
                    timeout,
                    max_output_bytes,
                    environment_allowlist,
                    limits,
                )
            },
        )
        .unwrap();
        let second = run_quality_gates_with_capture(
            &api,
            &manifest,
            &repo,
            &running,
            &manifest.execution_policy,
            2,
            &mut ledger,
            &mut required_gates,
            &mut usage,
            validation_started,
            execution_started,
            |command_text,
             cwd,
             running,
             timeout,
             max_output_bytes,
             environment_allowlist,
             limits| {
                executed_commands
                    .lock()
                    .unwrap()
                    .push(command_text.to_owned());
                command::capture_cancellable_with_environment(
                    "git rev-parse --is-inside-work-tree",
                    cwd,
                    running,
                    timeout,
                    max_output_bytes,
                    environment_allowlist,
                    limits,
                )
            },
        )
        .unwrap();

        // A one-byte mutation creates a new authoritative tree and invalidates all three
        // fingerprints exactly once.
        fs::write(directory.path().join("theme.ts"), "Export {};\n").unwrap();
        let third = run_quality_gates_with_capture(
            &api,
            &manifest,
            &repo,
            &running,
            &manifest.execution_policy,
            3,
            &mut ledger,
            &mut required_gates,
            &mut usage,
            validation_started,
            execution_started,
            |command_text,
             cwd,
             running,
             timeout,
             max_output_bytes,
             environment_allowlist,
             limits| {
                executed_commands
                    .lock()
                    .unwrap()
                    .push(command_text.to_owned());
                command::capture_cancellable_with_environment(
                    "git rev-parse --is-inside-work-tree",
                    cwd,
                    running,
                    timeout,
                    max_output_bytes,
                    environment_allowlist,
                    limits,
                )
            },
        )
        .unwrap();

        assert_eq!(first.len(), 3);
        assert_eq!(second.len(), 3);
        assert_eq!(third.len(), 3);
        assert!(first.iter().all(|result| result.status == "passed"));
        assert!(second.iter().all(|result| result.status == "passed"));
        assert!(third.iter().all(|result| result.status == "passed"));
        assert_eq!(usage.validation_commands, 6);
        assert_eq!(usage.required_validations, 6);
        assert_eq!(usage.deduplicated_validations, 3);
        assert_eq!(
            *executed_commands.lock().unwrap(),
            [
                "npm test -- theme",
                "npm test",
                "npm run build",
                "npm test -- theme",
                "npm test",
                "npm run build",
            ]
        );
        assert_eq!(ledger.len(), 6);
        assert_ne!(ledger[0].source_tree_hash, ledger[3].source_tree_hash);
        assert_ne!(ledger[0].command_fingerprint, ledger[3].command_fingerprint);
        assert_eq!(required_gates.len(), 3);
        server.join().unwrap();
        assert_eq!(requests.try_iter().count(), 27);
    }

    #[test]
    fn focused_gate_failure_short_circuits_broad_gates_until_source_repair() {
        let directory = tempfile::tempdir().unwrap();
        command::checked("git", ["init", "-q"], directory.path()).unwrap();
        command::checked("git", ["config", "user.name", "Test"], directory.path()).unwrap();
        command::checked(
            "git",
            ["config", "user.email", "test@example.com"],
            directory.path(),
        )
        .unwrap();
        fs::write(directory.path().join("theme.ts"), "export {};\n").unwrap();
        command::checked("git", ["add", "theme.ts"], directory.path()).unwrap();
        command::checked(
            "git",
            [
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
            directory.path(),
        )
        .unwrap();
        let execution_id = Uuid::from_u128(0xa0227);
        let mut manifest = test_manifest(execution_id);
        manifest.github.base_sha =
            command::checked("git", ["rev-parse", "HEAD"], directory.path()).unwrap();
        manifest.execution_policy.quality_gates = vec![
            HostedQualityGate {
                id: "test".into(),
                command: "npm test".into(),
                timeout_seconds: 30,
                required: true,
            },
            HostedQualityGate {
                id: "build".into(),
                command: "npm run build".into(),
                timeout_seconds: 30,
                required: true,
            },
            HostedQualityGate {
                id: "focused-theme".into(),
                command: "npm test -- theme".into(),
                timeout_seconds: 30,
                required: true,
            },
        ];
        let Some((api_root, requests, server)) = request_sequence_server(
            std::iter::repeat_with(|| ("200 OK", json!({})))
                .take(4)
                .collect(),
        ) else {
            return;
        };
        let api = test_api_client(api_root, execution_id);
        let repo = Repo {
            root: directory.path().to_path_buf(),
        };
        let running = Arc::new(AtomicBool::new(true));
        let executed = Arc::new(Mutex::new(Vec::new()));
        let mut ledger = Vec::new();
        let mut required_gates = Vec::new();
        let mut usage = ToolUsage::default();
        let results = run_quality_gates_with_capture(
            &api,
            &manifest,
            &repo,
            &running,
            &manifest.execution_policy,
            1,
            &mut ledger,
            &mut required_gates,
            &mut usage,
            Instant::now(),
            Instant::now(),
            |command_text, _, _, _, _, _, _| {
                executed.lock().unwrap().push(command_text.to_owned());
                Err(anyhow!("focused theme test failed"))
            },
        )
        .unwrap();

        assert_eq!(*executed.lock().unwrap(), ["npm test -- theme"]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "failed");
        assert_eq!(required_gates.len(), 1);
        assert_eq!(required_gates[0].gate_type, ValidationGateType::FocusedTest);
        server.join().unwrap();
        assert_eq!(requests.try_iter().count(), 4);
    }

    #[test]
    fn repository_tools_cannot_escape_or_traverse_git_metadata() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join(".git")).unwrap();
        fs::write(directory.path().join("safe.txt"), "safe\n").unwrap();
        assert!(safe_repo_path(directory.path(), "safe.txt", false).is_ok());
        assert!(safe_repo_path(directory.path(), "../outside", true).is_err());
        assert!(safe_repo_path(directory.path(), ".git/config", true).is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/tmp", directory.path().join("linked")).unwrap();
            assert!(safe_repo_path(directory.path(), "linked/file", true).is_err());
        }
    }

    #[test]
    fn hosted_repository_fingerprint_tracks_content_and_is_stable_across_commit() {
        let directory = tempfile::tempdir().unwrap();
        command::checked("git", ["init", "-q"], directory.path()).unwrap();
        fs::write(directory.path().join("tracked.ts"), "one\n").unwrap();
        command::checked("git", ["add", "tracked.ts"], directory.path()).unwrap();
        command::checked(
            "git",
            [
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
            directory.path(),
        )
        .unwrap();
        let base = command::checked("git", ["rev-parse", "HEAD"], directory.path()).unwrap();
        let repo = Repo {
            root: directory.path().to_path_buf(),
        };
        let clean = repository_state_fingerprint(&repo, &base).unwrap();
        assert_ne!(clean, hex::encode(Sha256::digest(b"")));

        // Change exactly one byte while preserving file length.
        fs::write(directory.path().join("tracked.ts"), "One\n").unwrap();
        let edited = repository_state_fingerprint(&repo, &base).unwrap();
        assert_ne!(edited, clean);
        fs::write(directory.path().join("tracked.ts"), "one\n").unwrap();
        assert_eq!(repository_state_fingerprint(&repo, &base).unwrap(), clean);

        fs::write(directory.path().join("untracked.test.ts"), "new\n").unwrap();
        let untracked_only = repository_state_fingerprint(&repo, &base).unwrap();
        assert_ne!(untracked_only, clean);
        fs::remove_file(directory.path().join("untracked.test.ts")).unwrap();
        assert_eq!(repository_state_fingerprint(&repo, &base).unwrap(), clean);

        fs::write(directory.path().join("tracked.ts"), "two\n").unwrap();
        fs::write(directory.path().join("new.test.ts"), "new\n").unwrap();
        let before_commit = repository_state_fingerprint(&repo, &base).unwrap();
        command::checked(
            "git",
            ["add", "tracked.ts", "new.test.ts"],
            directory.path(),
        )
        .unwrap();
        command::checked(
            "git",
            [
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "implementation",
            ],
            directory.path(),
        )
        .unwrap();
        let after_commit = repository_state_fingerprint(&repo, &base).unwrap();
        assert_eq!(after_commit, before_commit);

        let other = tempfile::tempdir().unwrap();
        command::checked("git", ["init", "-q"], other.path()).unwrap();
        fs::write(other.path().join("different.ts"), "different\n").unwrap();
        command::checked("git", ["add", "different.ts"], other.path()).unwrap();
        command::checked(
            "git",
            [
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "different base",
            ],
            other.path(),
        )
        .unwrap();
        let other_base = command::checked("git", ["rev-parse", "HEAD"], other.path()).unwrap();
        let other_repo = Repo {
            root: other.path().to_path_buf(),
        };
        assert_ne!(
            repository_state_fingerprint(&other_repo, &other_base).unwrap(),
            clean
        );
    }

    #[test]
    fn batch_reads_prevalidate_every_path_before_preserving_success_and_fallback() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("present.ts"), "one\ntwo\nthree\n").unwrap();
        let paths = vec![
            json!("present.ts"),
            json!(42),
            json!("../outside.ts"),
            json!("missing.ts"),
        ];

        let prevalidated = prevalidate_batch_read_paths(directory.path(), &paths);
        let prevalidation_codes = prevalidated
            .iter()
            .map(|path| match path {
                PrevalidatedBatchReadPath::Ready(_) => None,
                PrevalidatedBatchReadPath::Rejected { result, .. } => result.error_code.as_deref(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            prevalidation_codes,
            vec![
                None,
                Some("path_malformed"),
                Some("path_not_allowed"),
                Some("path_not_found"),
            ]
        );

        let (batch, initial_failures) = read_prevalidated_repo_files_with_fallback(
            directory.path(),
            &prevalidated,
            2,
            8 * 1024,
        );

        assert_eq!(initial_failures, 3);
        assert_eq!(batch.files.len(), 4);
        assert_eq!(batch.files[0].status, FileReadStatus::Success);
        assert!(batch.files[0].content.as_deref().unwrap().contains("one"));
        assert_eq!(batch.files[0].line_count, Some(3));
        assert_eq!(batch.files[0].valid_line_range.as_deref(), Some("1-3"));
        assert_eq!(batch.files[1].status, FileReadStatus::Error);
        assert_eq!(batch.files[1].path, "<paths[1]>");
        assert_eq!(batch.files[1].error_code.as_deref(), Some("path_malformed"));
        assert!(!batch.files[1].fallback_attempted);
        assert_eq!(batch.files[2].status, FileReadStatus::Error);
        assert_eq!(
            batch.files[2].error_code.as_deref(),
            Some("path_not_allowed")
        );
        assert!(batch.files[2].fallback_attempted);
        assert_eq!(batch.files[3].status, FileReadStatus::Error);
        assert_eq!(batch.files[3].error_code.as_deref(), Some("path_not_found"));
        assert!(batch.files[3].fallback_attempted);
        assert!(
            batch.files[3]
                .error_message
                .as_deref()
                .unwrap()
                .contains("individual read fallback also failed")
        );
    }

    #[test]
    fn invalid_read_range_returns_recovery_metadata_without_losing_file_shape() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("short.ts"), "one\ntwo\n").unwrap();

        let result = read_repo_file_result(directory.path(), "short.ts", 9, 12, 8 * 1024);

        assert_eq!(result.status, FileReadStatus::Error);
        assert_eq!(result.error_code.as_deref(), Some("line_range_invalid"));
        assert_eq!(result.line_count, Some(2));
        assert_eq!(result.valid_line_range.as_deref(), Some("1-2"));
        assert!(
            result
                .error_message
                .as_deref()
                .unwrap()
                .contains("valid line range is 1-2")
        );
    }

    #[test]
    fn planning_repair_preserves_valid_fragments_and_reports_only_invalid_paths() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("src")).unwrap();
        for path in ["src/provider.ts", "src/toggle.ts", "src/tokens.ts"] {
            fs::write(directory.path().join(path), "export {};\n").unwrap();
        }
        let payload = json!({
            "planned_changes": [
                {
                    "change_id": "valid-theme-provider",
                    "targets": [{"path": "src/provider.ts", "role": "provider"}],
                    "intent": "Update the provider.",
                    "reason": "Register the new theme.",
                    "acceptance_criteria": ["Theme is selectable"]
                },
                {
                    "change_id": "invalid-theme-toggle",
                    "targets": [{"path": "src/toggle.ts", "role": "selector"}],
                    "intent": "Update the selector."
                },
                {
                    "change_id": "valid-theme-tokens",
                    "targets": [{"path": "src/tokens.ts", "role": "tokens"}],
                    "intent": "Update the tokens.",
                    "reason": "Complete the palette.",
                    "acceptance_criteria": ["Theme is selectable"]
                }
            ]
        });
        let state =
            recover_planning_repair_state(directory.path(), payload.as_object().unwrap(), 7);

        assert_eq!(state.model_call, 7);
        assert_eq!(state.valid_planned_changes.len(), 2);
        assert_eq!(state.valid_planned_change_positions, vec![0, 2]);
        assert_eq!(state.invalid_fields.len(), 1);
        assert!(state.invalid_fields[0].starts_with("$.planned_changes[1]:"));

        let repaired_middle: PlannedChange = serde_json::from_value(json!({
            "change_id": "invalid-theme-toggle",
            "targets": [{"path": "src/toggle.ts", "role": "selector"}],
            "intent": "Update the selector.",
            "reason": "Include the new theme in cycling.",
            "acceptance_criteria": ["Theme is selectable"]
        }))
        .unwrap();
        let mut repaired = vec![repaired_middle];
        merge_preserved_plan_fragments(&mut repaired, Some(&state));
        assert_eq!(
            repaired
                .iter()
                .map(|change| change.change_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "valid-theme-provider",
                "invalid-theme-toggle",
                "valid-theme-tokens"
            ]
        );
    }

    #[test]
    fn reported_write_progress_is_informational_and_cannot_fake_a_repository_write() {
        assert!(!is_source_mutation_tool("report_write_progress"));
        assert_eq!(
            informational_write_progress_semantics(),
            (ToolProgressClass::Neutral, false)
        );
        let report = informational_write_progress("ready_to_write", "target inspected").unwrap();
        assert!(report.contains("repository_progress=false"));
        assert!(informational_write_progress("applied", "claim only").is_err());
    }

    #[test]
    fn guided_recovery_context_and_admission_are_confined_to_the_current_target() {
        let paths = [
            "src/components/theme/ThemeProvider.tsx",
            "src/components/theme/ThemeToggle.tsx",
            "src/styles/globals.css",
            "tests/theme-provider.test.tsx",
            "tests/theme-tokens.test.ts",
        ];
        let mut notebook = test_discovery_notebook(ExecutionPhase::Implementation);
        notebook.planned_changes = paths
            .iter()
            .enumerate()
            .map(|(index, path)| PlannedChange {
                change_id: format!("target-{}", index + 1),
                parent_change_id: None,
                path: String::new(),
                targets: vec![PlannedTarget {
                    path: (*path).into(),
                    role: "required target".into(),
                    new_file: false,
                    status: IntendedChangeStatus::Planned,
                }],
                change: format!("Update {path}"),
                reason: "Implement the localized theme.".into(),
                status: IntendedChangeStatus::Planned,
                acceptance_criteria: vec!["Theme can be selected".into()],
                test_coverage: vec!["focused theme tests".into()],
            })
            .collect();
        notebook.intended_changes = intended_changes_from_plan(&notebook.planned_changes);
        notebook.files_inspected = vec![paths[0].into()];
        notebook.tool_progress.push(new_tool_progress_record(
            notebook.execution_attempt,
            6,
            ExecutionPhase::Implementation,
            "read_file",
            Some(paths[0].into()),
            ToolProgressClass::RecoverableFailure,
            "line_range_invalid: valid line range is 1-87; retry that exact range",
            false,
        ));

        let context = implementation_start_context_from_notebook(
            &notebook,
            "tree-aops-226".into(),
            4,
            true,
            6,
            0,
        );
        assert_eq!(context.target_order.len(), 5);
        assert_eq!(context.current_target.as_ref().unwrap().path, paths[0]);
        assert_eq!(context.exact_files_already_read, vec![paths[0]]);
        assert!(context.missing_file_contents.contains(&paths[1].into()));
        assert!(context.instruction.contains("work only on current_target"));
        assert_eq!(context.unresolved_preparation_blockers.len(), 1);
        assert!(context.unresolved_preparation_blockers[0].contains("valid line range is 1-87"));

        let current = context.current_target.as_ref();
        assert!(validate_current_target_scope(current, true, 0, &[paths[0]], true).is_ok());
        assert!(validate_current_target_scope(current, true, 0, &[paths[1]], false).is_err());
        assert!(validate_current_target_scope(current, true, 0, &[paths[1]], true).is_err());

        notebook.intended_changes[0].targets[0].status = IntendedChangeStatus::Applied;
        let advanced = implementation_start_context_from_notebook(
            &notebook,
            "tree-after-first-write".into(),
            3,
            false,
            7,
            1,
        );
        assert_eq!(advanced.current_target.as_ref().unwrap().path, paths[1]);
        assert_eq!(advanced.target_order.len(), 4);

        for change in &mut notebook.intended_changes {
            for target in &mut change.targets {
                target.status = IntendedChangeStatus::Applied;
            }
        }
        notebook.phase = ExecutionPhase::Repair;
        let focused_failure = ValidationResult {
            id: "focused-theme".into(),
            command: "npm test -- theme".into(),
            status: "failed".into(),
            output: "light-blue restoration assertion failed".into(),
        };
        record_validation_repair_start(&mut notebook, std::slice::from_ref(&focused_failure));
        assert_eq!(
            notebook.implementation_substate,
            ImplementationSubstate::Repairing
        );
        let repair_context = implementation_start_context_from_notebook(
            &notebook,
            "tree-with-failed-focused-gate".into(),
            2,
            false,
            8,
            5,
        );
        assert_eq!(repair_context.target_order.len(), paths.len());
        assert_eq!(
            repair_context.current_target.as_ref().unwrap().path,
            paths[0]
        );
        assert!(
            repair_context
                .target_order
                .iter()
                .all(|target| paths.contains(&target.path.as_str()))
        );
    }

    #[test]
    fn live_progress_records_classify_reads_repeated_failures_and_informational_reports() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("present.ts"), "one\n").unwrap();
        let successful = read_repo_file_result(directory.path(), "present.ts", 1, 1, 8 * 1024);
        assert_eq!(successful.status, FileReadStatus::Success);
        let failed = read_repo_file_result(directory.path(), "missing.ts", 1, 10, 8 * 1024);
        let error_code = failed.error_code.as_deref().unwrap();
        let detail = format!(
            "{}: {}",
            error_code,
            failed.error_message.as_deref().unwrap()
        );
        let records = vec![
            new_tool_progress_record(
                18,
                1,
                ExecutionPhase::Implementation,
                "read_file",
                Some("present.ts".into()),
                ToolProgressClass::Productive,
                "new repository content inspected",
                false,
            ),
            new_tool_progress_record(
                18,
                2,
                ExecutionPhase::Implementation,
                "read_file",
                Some("missing.ts".into()),
                read_error_progress_class(error_code),
                &detail,
                false,
            ),
            new_tool_progress_record(
                18,
                3,
                ExecutionPhase::Implementation,
                "read_file",
                Some("missing.ts".into()),
                read_error_progress_class(error_code),
                &detail,
                false,
            ),
            new_tool_progress_record(
                18,
                4,
                ExecutionPhase::Implementation,
                "report_write_progress",
                None,
                informational_write_progress_semantics().0,
                "ready to write",
                informational_write_progress_semantics().1,
            ),
        ];
        let progress = implementation_read_progress(&records, 18);
        assert_eq!(progress.recoverable_read_failures, 2);
        assert_eq!(progress.repeated_identical_read_failures, 2);
        assert_eq!(records[3].class, ToolProgressClass::Neutral);
        assert!(!records[3].repository_progress);
        assert_eq!(ToolUsage::default().successful_writes, 0);
        let recovered_read = vec![
            new_tool_progress_record(
                18,
                1,
                ExecutionPhase::Implementation,
                "read_file",
                Some("present.ts".into()),
                ToolProgressClass::RecoverableFailure,
                "line_range_invalid: valid line range is 1-1",
                false,
            ),
            new_tool_progress_record(
                18,
                2,
                ExecutionPhase::Implementation,
                "read_file",
                Some("present.ts".into()),
                ToolProgressClass::Productive,
                "recovered exact range",
                false,
            ),
        ];
        assert!(unresolved_preparation_blockers(&recovered_read, 18, 2, 0).is_empty());
        let no_tool_blockers = unresolved_preparation_blockers(&[], 18, 6, 0);
        assert_eq!(no_tool_blockers.len(), 1);
        assert!(no_tool_blockers[0].contains("6 implementation turns"));
        assert_eq!(
            successful_read_progress("read_file", false, true, false, false).0,
            ToolProgressClass::Productive
        );
        assert_eq!(
            successful_read_progress("related_tests", false, false, true, false).0,
            ToolProgressClass::Productive
        );
        assert_eq!(
            implementation_progress_action(4, 0, 0, 2, 2, false, 4),
            ImplementationProgressAction::FirstWriteDelayed
        );
    }

    #[test]
    fn post_write_stagnation_marks_useful_partial_work_for_validation() {
        let mut blocker = None;
        assert!(mark_post_write_stagnation(&mut blocker, 1));
        assert!(
            blocker
                .as_deref()
                .unwrap()
                .contains("post_write_stagnation")
        );
        let mut planned = test_planned_change();
        planned.targets.push(PlannedTarget {
            path: "src/components/theme/ThemeToggle.tsx".into(),
            role: "selector".into(),
            new_file: false,
            status: IntendedChangeStatus::Planned,
        });
        let mut change = intended_changes_from_plan(&[planned]);
        change[0].targets[0].status = IntendedChangeStatus::Applied;
        let changed = BTreeSet::from([change[0].targets[0].path.clone()]);
        let status = implementation_completion_status(&change, &changed, false, true);
        assert_eq!(
            status,
            ImplementationCompletionStatus::PartialReadyForValidation
        );
        assert_eq!(
            validation_entry_decision(status, 1, false, true),
            ValidationEntryDecision::UsefulPartialImplementation
        );
    }

    #[test]
    fn mutation_preflight_block_after_a_write_preserves_useful_partial_validation() {
        let mut blocker = None;
        assert!(mark_mutation_preflight_blocker(
            &mut blocker,
            "src/components/theme/ThemeToggle.tsx",
        ));
        assert!(
            blocker
                .as_deref()
                .unwrap()
                .contains("mutation_preflight_rejected")
        );
        assert_eq!(
            validation_entry_decision(
                ImplementationCompletionStatus::InProgress,
                1,
                false,
                blocker.is_some(),
            ),
            ValidationEntryDecision::UsefulPartialImplementation
        );
    }

    #[test]
    fn replace_text_requires_one_exact_match_and_supports_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("theme.css");
        fs::write(&path, "root {}\nred {}\n").unwrap();

        replace_unique_repo_text(directory.path(), "theme.css", "red {}", "blue {}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "root {}\nblue {}\n");

        replace_unique_repo_text(directory.path(), "theme.css", "blue {}\n", "").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "root {}\n");
        assert!(
            replace_unique_repo_text(directory.path(), "theme.css", "missing", "value").is_err()
        );

        fs::write(&path, "same\nsame\n").unwrap();
        assert!(replace_unique_repo_text(directory.path(), "theme.css", "same", "other").is_err());
    }

    #[test]
    fn safer_write_tools_are_deterministic_and_report_mutation_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("theme.test.ts");
        fs::write(&path, "one\nmarker\ntwo\n").unwrap();

        let range = replace_repo_range(directory.path(), "theme.test.ts", 1, 1, "first").unwrap();
        let range: Value = serde_json::from_str(&range).unwrap();
        assert!(range["before_sha256"].is_string());
        assert!(range["after_sha256"].is_string());
        assert_eq!(range["changed_range"], "1-1");
        assert!(range["diff_summary"].as_str().unwrap().contains("line"));

        let inserted = insert_relative_to_symbol(
            directory.path(),
            "theme.test.ts",
            "marker",
            "\ninserted",
            true,
        )
        .unwrap();
        let inserted: Value = serde_json::from_str(&inserted).unwrap();
        assert_ne!(inserted["before_sha256"], inserted["after_sha256"]);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("marker\ninserted")
        );

        let rewritten =
            write_repo_file(directory.path(), "theme.test.ts", "final\n", true).unwrap();
        let rewritten: Value = serde_json::from_str(&rewritten).unwrap();
        assert_eq!(rewritten["changed_range"], "complete_file");
        assert_eq!(fs::read_to_string(&path).unwrap(), "final\n");
    }

    #[test]
    fn unified_diff_tool_is_path_scoped_and_reports_hashes() {
        let directory = tempfile::tempdir().unwrap();
        command::checked("git", ["init", "-q"], directory.path()).unwrap();
        let path = directory.path().join("theme.test.ts");
        fs::write(&path, "old\n").unwrap();
        let output = apply_repo_unified_diff(
            directory.path(),
            "theme.test.ts",
            "--- a/theme.test.ts\n+++ b/theme.test.ts\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
        let output: Value = serde_json::from_str(&output).unwrap();
        assert_ne!(output["before_sha256"], output["after_sha256"]);
        assert_eq!(output["changed_range"], "unified_diff");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert!(
            apply_repo_unified_diff(
                directory.path(),
                "theme.test.ts",
                "--- a/other.ts\n+++ b/other.ts\n@@ -1 +1 @@\n-old\n+new\n",
            )
            .is_err()
        );
    }

    #[test]
    fn replacement_repair_is_bounded_and_forces_a_safer_strategy() {
        let failed = |error_code: &str| WriteAttemptRecord {
            attempt_index: 0,
            change_id: "theme-tests".into(),
            target: "tests/theme-provider.test.tsx".into(),
            tool: "replace_text".into(),
            status: WriteAttemptStatus::Failed,
            error_code: Some(error_code.into()),
            match_count: Some(2),
            intended_change_sha256: None,
            before_sha256: None,
            after_sha256: None,
        };
        let one = vec![failed("replace_match_not_unique")];
        assert!(
            validate_write_repair_strategy(
                &one,
                "tests/theme-provider.test.tsx",
                "theme-tests",
                "replace_text",
                false,
            )
            .unwrap_err()
            .to_string()
            .contains("bounded read_file")
        );
        assert!(
            validate_write_repair_strategy(
                &one,
                "tests/theme-provider.test.tsx",
                "theme-tests",
                "replace_text",
                true,
            )
            .is_ok()
        );

        let two = vec![
            failed("replace_match_not_unique"),
            failed("replace_match_not_unique"),
        ];
        assert!(
            validate_write_repair_strategy(
                &two,
                "tests/theme-provider.test.tsx",
                "theme-tests",
                "replace_text",
                true,
            )
            .unwrap_err()
            .to_string()
            .contains("strategy exhausted")
        );
        assert!(
            validate_write_repair_strategy(
                &two,
                "tests/theme-provider.test.tsx",
                "theme-tests",
                "rewrite_small_file",
                true,
            )
            .is_ok()
        );

        let four = vec![
            failed("replace_match_not_unique"),
            failed("replace_match_not_unique"),
            failed("replace_match_not_unique"),
            failed("replace_match_not_unique"),
        ];
        assert!(
            validate_write_repair_strategy(
                &four,
                "tests/theme-provider.test.tsx",
                "theme-tests",
                "write_file",
                true,
            )
            .unwrap_err()
            .to_string()
            .contains("content repair circuit breaker")
        );
        assert!(
            validate_write_repair_strategy(
                &four,
                "tests/theme-provider.test.tsx",
                "theme-tests",
                "rewrite_small_file",
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn multi_file_plans_normalize_legacy_targets_and_authorize_membership() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("src/components/theme")).unwrap();
        for path in ["ThemeProvider.tsx", "ThemeToggle.tsx"] {
            fs::write(
                directory.path().join("src/components/theme").join(path),
                "export {};\n",
            )
            .unwrap();
        }
        let mut change = test_planned_change();
        change.change_id = "theme-registry-light-blue".into();
        change.parent_change_id = Some("theme-registry".into());
        change.path = "src/components/theme/ThemeProvider.tsx; src/components/theme/ThemeToggle.tsx; src/components/theme/ThemeProvider.tsx".into();
        change.targets.clear();
        let repair = repair_implementation_plan(
            std::slice::from_mut(&mut change),
            "theme-registry-light-blue",
            "src/components/theme/ThemeProvider.tsx",
        )
        .unwrap()
        .unwrap();
        assert!(!repair.model_call_consumed);
        assert_eq!(repair.repair_source, "orchestrator_normalization");
        assert_eq!(repair.targets_before.len(), 1);
        assert_eq!(
            change
                .targets
                .iter()
                .map(|target| target.path.as_str())
                .collect::<Vec<_>>(),
            [
                "src/components/theme/ThemeProvider.tsx",
                "src/components/theme/ThemeToggle.tsx"
            ]
        );
        validate_planned_change_paths(directory.path(), std::slice::from_ref(&change)).unwrap();
        let plan = ImplementationPlan {
            implementation_status: "ready".into(),
            planned_changes: vec![change],
            planned_new_files: vec![],
            planned_test_changes: vec![],
            remaining_unknowns: vec![],
            blocking_unknowns: vec![],
        };
        assert!(
            authorize_planned_target(
                &plan,
                "theme-registry-light-blue",
                "src/components/theme/ThemeProvider.tsx"
            )
            .is_ok()
        );
        assert!(
            authorize_planned_target(
                &plan,
                "theme-registry-light-blue",
                "src/components/theme/ThemeToggle.tsx"
            )
            .is_ok()
        );
        let rejected =
            authorize_planned_target(&plan, "theme-registry-light-blue", "src/styles/globals.css")
                .unwrap_err();
        assert_eq!(rejected.code, "mutation_plan_metadata_mismatch");
        assert_eq!(rejected.repair_strategy, "repair_plan_metadata");
        let serialized = serde_json::to_value(&plan).unwrap();
        assert!(serialized["planned_changes"][0].get("path").is_none());
        assert!(serialized["planned_changes"][0]["targets"].is_array());
        assert_eq!(
            serialized["planned_changes"][0]["parent_change_id"],
            "theme-registry"
        );
    }

    #[test]
    fn independently_editable_changes_may_share_one_logical_parent() {
        let mut provider = test_planned_change();
        provider.change_id = "theme-provider-light-blue".into();
        provider.parent_change_id = Some("theme-registry-light-blue".into());
        let mut toggle = test_planned_change();
        toggle.change_id = "theme-toggle-light-blue".into();
        toggle.parent_change_id = Some("theme-registry-light-blue".into());
        toggle.targets[0].path = "src/components/theme/ThemeToggle.tsx".into();

        normalize_planned_changes(&mut [provider.clone(), toggle.clone()]).unwrap();

        assert_eq!(provider.parent_change_id, toggle.parent_change_id);
        assert_ne!(provider.change_id, toggle.change_id);
    }

    #[test]
    fn preflight_rejection_is_not_an_executed_write_and_halts_tool_switching() {
        let mut notebook = test_discovery_notebook(ExecutionPhase::Implementation);
        let mut usage = ToolUsage::default();
        let preflight = MutationPreflightError {
            code: "mutation_plan_metadata_mismatch",
            change_id: "theme-registry-light-blue".into(),
            target: "src/components/theme/ThemeProvider.tsx".into(),
            message: "target is not a member of its planned target set".into(),
            repair_strategy: "repair_plan_metadata",
        };

        let first = record_mutation_preflight_rejection(&mut notebook, &mut usage, &preflight);
        assert!(first.halt_orchestration);
        assert!(!first.repeated);
        assert_eq!(usage.write_preflight_rejections, 1);
        assert_eq!(usage.write_execution_failures, 0);
        assert_eq!(usage.failed_writes, 0);
        assert!(notebook.write_attempts.is_empty());
        assert!(!notebook.write_preflight_rejections[0].mutation_attempted);

        let repeated = record_mutation_preflight_rejection(&mut notebook, &mut usage, &preflight);
        assert!(repeated.repeated);
        assert_eq!(notebook.write_preflight_rejections[0].occurrences, 2);
        assert_eq!(usage.write_execution_failures, 0);
    }

    #[test]
    fn preparation_progress_uses_guided_recovery_instead_of_four_call_termination() {
        assert_eq!(
            implementation_progress_action(4, 0, 4, 2, 1, false, 4),
            ImplementationProgressAction::Continue
        );
        assert_eq!(
            implementation_progress_action(6, 0, 6, 2, 1, false, 6),
            ImplementationProgressAction::FirstWriteDelayed
        );
        assert_eq!(
            implementation_progress_action(7, 0, 1, 3, 1, true, 7),
            ImplementationProgressAction::Continue
        );
        assert_eq!(
            implementation_progress_action(8, 0, 1, 3, 1, true, 8),
            ImplementationProgressAction::BlockedBeforeFirstWrite
        );
        assert_eq!(
            implementation_progress_action(12, 1, 0, 0, 0, true, 4),
            ImplementationProgressAction::BlockedAfterWrite
        );
    }

    #[test]
    fn persisted_legacy_intended_change_resumes_with_structured_targets() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("provider.tsx"), "export {};\n").unwrap();
        let mut notebook = test_discovery_notebook(ExecutionPhase::Implementation);
        notebook.planned_changes = vec![
            serde_json::from_value(json!({
                "change_id": "theme-registry-light-blue",
                "path": "provider.tsx; toggle.tsx",
                "change": "Expose light blue theme",
                "reason": "Theme selection",
                "acceptance_criteria": ["ac-1"]
            }))
            .unwrap(),
        ];
        notebook.intended_changes = vec![
            serde_json::from_value(json!({
                "change_id": "theme-registry-light-blue",
                "intent": "Expose light blue theme",
                "status": "applied",
                "target": "provider.tsx; toggle.tsx"
            }))
            .unwrap(),
        ];

        normalize_notebook_intended_changes(&mut notebook, directory.path()).unwrap();

        assert_eq!(
            notebook.intended_changes[0].status,
            IntendedChangeStatus::Applied
        );
        assert_eq!(
            notebook.intended_changes[0]
                .targets
                .iter()
                .map(|target| target.path.as_str())
                .collect::<Vec<_>>(),
            vec!["provider.tsx", "toggle.tsx"]
        );
        let persisted = serde_json::to_value(&notebook.intended_changes[0]).unwrap();
        assert!(persisted.get("target").is_none());
        assert_eq!(persisted["targets"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn per_target_status_rolls_up_without_claiming_multi_file_completion() {
        let mut change = test_planned_change();
        change.targets.push(PlannedTarget {
            path: "src/components/theme/ThemeToggle.tsx".into(),
            role: "selector cycling".into(),
            new_file: false,
            status: IntendedChangeStatus::Planned,
        });
        change.targets[0].status = IntendedChangeStatus::Applied;
        assert_eq!(
            roll_up_target_statuses(&change.targets),
            IntendedChangeStatus::Partial
        );
        for target in &mut change.targets {
            target.status = IntendedChangeStatus::Verified;
        }
        assert_eq!(
            roll_up_target_statuses(&change.targets),
            IntendedChangeStatus::Verified
        );
    }

    #[test]
    fn plan_validation_rejects_missing_paths_unless_explicitly_new() {
        let directory = tempfile::tempdir().unwrap();
        let mut change = test_planned_change();
        change.targets[0].path = "src/new-theme.ts".into();
        assert!(
            validate_planned_change_paths(directory.path(), std::slice::from_ref(&change)).is_err()
        );
        change.targets[0].new_file = true;
        validate_planned_change_paths(directory.path(), std::slice::from_ref(&change)).unwrap();
    }

    #[test]
    fn later_equivalent_write_recovers_different_attempt_hashes_by_change_id() {
        let mut failures = vec![test_write_failure(
            "theme-tests",
            "tests/theme-provider.test.tsx",
            "first-hash",
        )];
        let attempts = vec![WriteAttemptRecord {
            attempt_index: 1,
            change_id: "theme-tests".into(),
            target: "tests/theme-provider.test.tsx".into(),
            tool: "replace_range".into(),
            status: WriteAttemptStatus::Applied,
            error_code: None,
            match_count: None,
            intended_change_sha256: Some("different-hash".into()),
            before_sha256: Some("before".into()),
            after_sha256: Some("after".into()),
        }];
        reconcile_failed_write_attempts(
            &mut failures,
            &[test_planned_change()],
            &attempts,
            &test_complete_implementation(),
            &[test_passed_validation("npm test")],
            &["tests/theme-provider.test.tsx".into()],
        );
        assert!(failures[0].recovered);
        assert_eq!(
            failures[0].reconciliation,
            FailureReconciliation::Superseded
        );
    }

    #[test]
    fn successful_noop_attempt_does_not_supersede_a_failed_change() {
        let mut failures = vec![test_write_failure(
            "theme-tests",
            "tests/theme-provider.test.tsx",
            "first-hash",
        )];
        let attempts = vec![WriteAttemptRecord {
            attempt_index: 1,
            change_id: "theme-tests".into(),
            target: "tests/theme-provider.test.tsx".into(),
            tool: "replace_range".into(),
            status: WriteAttemptStatus::Applied,
            error_code: None,
            match_count: None,
            intended_change_sha256: Some("different-hash".into()),
            before_sha256: Some("unchanged".into()),
            after_sha256: Some("unchanged".into()),
        }];
        reconcile_failed_write_attempts(
            &mut failures,
            &[test_planned_change()],
            &attempts,
            &ImplementationOutcome {
                summary: String::new(),
                budget_exhausted: false,
                explicit_declaration: None,
            },
            &[],
            &[],
        );
        assert!(!failures[0].recovered);
        assert_eq!(
            failures[0].reconciliation,
            FailureReconciliation::StillUnresolved
        );
    }

    #[test]
    fn an_earlier_success_does_not_supersede_a_later_failure() {
        let mut failure = test_write_failure(
            "theme-tests",
            "tests/theme-provider.test.tsx",
            "failed-hash",
        );
        failure.attempt_index = 1;
        let attempts = vec![WriteAttemptRecord {
            attempt_index: 0,
            change_id: "theme-tests".into(),
            target: "tests/theme-provider.test.tsx".into(),
            tool: "replace_range".into(),
            status: WriteAttemptStatus::Applied,
            error_code: None,
            match_count: None,
            intended_change_sha256: Some("earlier-hash".into()),
            before_sha256: Some("before".into()),
            after_sha256: Some("after".into()),
        }];
        reconcile_failed_write_attempts(
            std::slice::from_mut(&mut failure),
            &[test_planned_change()],
            &attempts,
            &ImplementationOutcome {
                summary: String::new(),
                budget_exhausted: false,
                explicit_declaration: None,
            },
            &[],
            &[],
        );
        assert!(!failure.recovered);
    }

    #[test]
    fn whole_file_write_supersedes_all_prior_failures_on_its_target() {
        let mut failures = vec![
            test_write_failure("theme-tests", "tests/theme-provider.test.tsx", "hash-a"),
            test_write_failure("other-intent", "tests/theme-provider.test.tsx", "hash-b"),
        ];
        let attempts = vec![WriteAttemptRecord {
            attempt_index: 2,
            change_id: "theme-tests".into(),
            target: "tests/theme-provider.test.tsx".into(),
            tool: "rewrite_small_file".into(),
            status: WriteAttemptStatus::Applied,
            error_code: None,
            match_count: None,
            intended_change_sha256: Some("hash-c".into()),
            before_sha256: Some("before".into()),
            after_sha256: Some("after".into()),
        }];
        reconcile_failed_write_attempts(
            &mut failures,
            &[test_planned_change()],
            &attempts,
            &test_complete_implementation(),
            &[test_passed_validation("npm test")],
            &["tests/theme-provider.test.tsx".into()],
        );
        assert!(failures.iter().all(|failure| failure.recovered));
        assert!(
            failures
                .iter()
                .all(|failure| { failure.reconciliation == FailureReconciliation::Superseded })
        );
    }

    #[test]
    fn final_diff_and_validation_recover_incident_but_validation_alone_does_not() {
        let mut recovered = vec![test_write_failure(
            "theme-tests",
            "tests/theme-provider.test.tsx",
            "hash-a",
        )];
        let validation = vec![
            test_passed_validation("npm test"),
            test_passed_validation("npm run build"),
        ];
        reconcile_failed_write_attempts(
            &mut recovered,
            &[test_planned_change()],
            &[],
            &test_complete_implementation(),
            &validation,
            &["tests/theme-provider.test.tsx".into()],
        );
        assert_eq!(
            recovered[0].reconciliation,
            FailureReconciliation::Recovered
        );
        assert!(
            recovered[0]
                .recovery
                .as_ref()
                .unwrap()
                .evidence
                .iter()
                .any(|evidence| evidence == "npm run build passed.")
        );

        let mut absent = vec![test_write_failure(
            "theme-tests",
            "tests/theme-provider.test.tsx",
            "hash-a",
        )];
        reconcile_failed_write_attempts(
            &mut absent,
            &[test_planned_change()],
            &[],
            &test_complete_implementation(),
            &validation,
            &[],
        );
        assert!(!absent[0].recovered);
        assert_eq!(
            absent[0].reconciliation,
            FailureReconciliation::StillUnresolved
        );
    }

    #[test]
    fn fallback_populates_code_evidence_and_passed_validation_from_final_state() {
        let implementation = test_complete_implementation();
        let plan = ImplementationPlan {
            implementation_status: "ready".into(),
            planned_changes: vec![test_planned_change()],
            planned_new_files: vec![],
            planned_test_changes: vec!["tests/theme-provider.test.tsx".into()],
            remaining_unknowns: vec![],
            blocking_unknowns: vec![],
        };
        let result = completion_fallback(
            &implementation,
            None,
            Some(&plan),
            &[],
            &["tests/theme-provider.test.tsx".into()],
            &["Theme can be selected".into()],
            &[
                test_passed_validation("npm test"),
                test_passed_validation("npm run build"),
            ],
            ProjectVerificationPolicy {
                browser_e2e_required_for_theme_changes: false,
                manual_browser_verification_required: false,
            },
        );
        let criterion = &result.criteria[0];
        assert_eq!(criterion.status, CriterionStatus::Satisfied);
        assert!(!criterion.evidence.is_empty());
        assert_eq!(
            criterion.validation_evidence,
            vec!["npm test", "npm run build"]
        );
    }

    #[test]
    fn passing_validation_cannot_complete_an_unchanged_required_target() {
        let implementation = test_complete_implementation();
        let mut change = test_planned_change();
        change.targets.push(PlannedTarget {
            path: "src/components/theme/ThemeToggle.tsx".into(),
            role: "selector cycling".into(),
            new_file: false,
            status: IntendedChangeStatus::Planned,
        });
        let plan = ImplementationPlan {
            implementation_status: "ready".into(),
            planned_changes: vec![change],
            planned_new_files: vec![],
            planned_test_changes: vec![],
            remaining_unknowns: vec![],
            blocking_unknowns: vec![],
        };
        let result = completion_fallback(
            &implementation,
            None,
            Some(&plan),
            &[],
            &["tests/theme-provider.test.tsx".into()],
            &["Theme can be selected".into()],
            &[test_passed_validation("npm test")],
            ProjectVerificationPolicy::default(),
        );

        assert_eq!(result.criteria[0].status, CriterionStatus::Unsatisfied);
        assert_ne!(
            result.implementation_completeness,
            ImplementationCompleteness::Complete
        );
        assert!(
            result.criteria[0].missing_evidence[0].contains("src/components/theme/ThemeToggle.tsx")
        );
    }

    #[test]
    fn planned_changes_receive_stable_unique_change_ids() {
        let mut first = test_planned_change();
        first.change_id.clear();
        let mut second = first.clone();
        second.path = "src/components/theme/ThemeProvider.tsx".into();
        normalize_planned_changes(&mut [first.clone(), second]).unwrap();

        let mut repeated = vec![first];
        normalize_planned_changes(&mut repeated).unwrap();
        let first_id = repeated[0].change_id.clone();
        normalize_planned_changes(&mut repeated).unwrap();
        assert_eq!(repeated[0].change_id, first_id);

        let mut duplicates = vec![test_planned_change(), test_planned_change()];
        assert!(normalize_planned_changes(&mut duplicates).is_err());
    }

    #[test]
    fn exact_five_target_diff_produces_an_authoritative_complete_declaration() {
        let criterion = "Light-blue theme behavior is implemented and covered.".to_owned();
        let paths = [
            "src/components/theme/ThemeProvider.tsx",
            "src/components/theme/ThemeToggle.tsx",
            "src/styles/globals.css",
            "tests/theme-provider.test.tsx",
            "tests/theme-tokens.test.ts",
        ];
        let planned = paths
            .iter()
            .enumerate()
            .map(|(index, path)| PlannedChange {
                change_id: format!("aops-226-target-{}", index + 1),
                parent_change_id: None,
                path: String::new(),
                targets: vec![PlannedTarget {
                    path: (*path).into(),
                    role: "required AOPS-226 target".into(),
                    new_file: path.starts_with("tests/"),
                    status: IntendedChangeStatus::Applied,
                }],
                change: format!("Implement light-blue behavior in {path}."),
                reason: "Complete the localized theme change.".into(),
                status: IntendedChangeStatus::Applied,
                acceptance_criteria: vec![criterion.clone()],
                test_coverage: vec!["focused theme tests".into()],
            })
            .collect::<Vec<_>>();
        let changed_paths = paths
            .iter()
            .map(|path| (*path).to_owned())
            .collect::<Vec<_>>();

        let declaration = deterministic_complete_declaration(
            &planned,
            std::slice::from_ref(&criterion),
            &changed_paths,
            &[],
            &[],
        )
        .expect("all five applied targets should produce a declaration");

        assert_eq!(declaration.implementation_status, "complete");
        assert_eq!(declaration.changed_paths, changed_paths);
        assert_eq!(declaration.criteria_evidence.len(), 1);
        assert_eq!(declaration.criteria_evidence[0].paths.len(), 5);
        assert!(declaration.remaining_work.is_empty());
    }

    #[test]
    fn resumed_reconciliation_unions_committed_and_dirty_target_paths() {
        let work = tempfile::tempdir().unwrap();
        command::checked("git", ["init", "-q"], work.path()).unwrap();
        fs::write(work.path().join("provider.tsx"), "provider base\n").unwrap();
        fs::write(work.path().join("toggle.tsx"), "toggle base\n").unwrap();
        command::checked("git", ["add", "."], work.path()).unwrap();
        command::checked(
            "git",
            [
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
            work.path(),
        )
        .unwrap();
        let base_sha = command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap();

        fs::write(work.path().join("provider.tsx"), "provider committed\n").unwrap();
        command::checked("git", ["add", "provider.tsx"], work.path()).unwrap();
        command::checked(
            "git",
            [
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "first resumed target",
            ],
            work.path(),
        )
        .unwrap();
        fs::write(work.path().join("toggle.tsx"), "toggle dirty\n").unwrap();

        let repo = Repo {
            root: work.path().to_path_buf(),
        };
        let changed_paths = completion_changed_paths(&repo, &base_sha).unwrap();
        assert_eq!(changed_paths, ["provider.tsx", "toggle.tsx"]);
        let review = completion_review_diff(work.path(), &changed_paths, &base_sha).unwrap();
        assert!(review.contains("provider committed"));
        assert!(review.contains("toggle dirty"));

        let mut intended_changes = vec![IntendedChangeRecord {
            change_id: "theme-targets".into(),
            intent: "Apply both resumed theme targets".into(),
            status: IntendedChangeStatus::Partial,
            target: String::new(),
            targets: vec![
                PlannedTarget {
                    path: "provider.tsx".into(),
                    role: "previously committed target".into(),
                    new_file: false,
                    status: IntendedChangeStatus::Applied,
                },
                PlannedTarget {
                    path: "toggle.tsx".into(),
                    role: "new dirty target".into(),
                    new_file: false,
                    status: IntendedChangeStatus::Planned,
                },
            ],
            attempts: Vec::new(),
            recovery: None,
        }];
        reconcile_changed_target_statuses(
            &mut intended_changes,
            &changed_paths.into_iter().collect(),
        );
        assert_eq!(
            intended_changes[0].targets[0].status,
            IntendedChangeStatus::Applied
        );
        assert_eq!(
            intended_changes[0].targets[1].status,
            IntendedChangeStatus::InProgress
        );
        assert_eq!(intended_changes[0].status, IntendedChangeStatus::Partial);
    }

    #[test]
    fn aops_226_producing_fixture_reviews_commits_pushes_creates_pr_and_exits_zero() {
        let fixture_started = Instant::now();
        let work = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        command::checked(
            "git",
            ["init", "--bare", "-q", remote.path().to_str().unwrap()],
            work.path(),
        )
        .unwrap();
        command::checked("git", ["init", "-q"], work.path()).unwrap();
        command::checked("git", ["config", "user.name", "Test"], work.path()).unwrap();
        command::checked(
            "git",
            ["config", "user.email", "test@example.com"],
            work.path(),
        )
        .unwrap();
        let paths = [
            "src/components/theme/ThemeProvider.tsx",
            "src/components/theme/ThemeToggle.tsx",
            "src/styles/globals.css",
            "tests/theme-provider.test.tsx",
            "tests/theme-tokens.test.ts",
        ];
        for path in paths {
            let target = work.path().join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, format!("// base {path}\n")).unwrap();
        }
        command::checked("git", ["add", "."], work.path()).unwrap();
        command::checked(
            "git",
            [
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
            work.path(),
        )
        .unwrap();
        command::checked("git", ["branch", "-M", "main"], work.path()).unwrap();
        command::checked(
            "git",
            ["remote", "add", "origin", remote.path().to_str().unwrap()],
            work.path(),
        )
        .unwrap();
        command::checked("git", ["push", "-q", "-u", "origin", "main"], work.path()).unwrap();
        let base_sha = command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap();
        let branch = "rustgrid/aops-226-fixture";
        command::checked("git", ["checkout", "-q", "-b", branch], work.path()).unwrap();
        let mut performance = PhaseLedger::new(60, ExecutionPhase::Discovery);
        for _ in 0..4 {
            performance.begin_model_call().unwrap();
        }
        performance.transition(ExecutionPhase::Planning);
        for _ in 0..2 {
            performance.begin_model_call().unwrap();
        }
        assert_eq!(performance.apply_ticket_complexity(paths.len()), 20);
        performance.transition(ExecutionPhase::Implementation);
        let mut first_successful_write_call = None;
        let implemented_files = [
            (
                paths[0],
                r#"export type Theme = "dark" | "light" | "light-blue" | "red";
export const THEME_STORAGE_KEY = "rustgrid-theme";
export const THEME_ORDER: Theme[] = ["dark", "light", "light-blue", "red"];
export const isLightTheme = (theme: Theme) => theme === "light" || theme === "light-blue";

export function restoreTheme(saved: string | null): Theme {
  return THEME_ORDER.includes(saved as Theme) ? (saved as Theme) : "dark";
}

export function applyTheme(root: HTMLElement, theme: Theme) {
  root.classList.remove(...THEME_ORDER.map((value) => `theme-${value}`));
  root.classList.add(`theme-${theme}`);
  root.style.colorScheme = isLightTheme(theme) ? "light" : "dark";
  localStorage.setItem(THEME_STORAGE_KEY, theme);
}
"#,
            ),
            (
                paths[1],
                r#"import { THEME_ORDER, type Theme } from "./ThemeProvider";

export function nextTheme(theme: Theme): Theme {
  return THEME_ORDER[(THEME_ORDER.indexOf(theme) + 1) % THEME_ORDER.length];
}

export function themeToggleLabel(theme: Theme) {
  return `Switch to ${nextTheme(theme)} theme`;
}

export const themeToggleIcon = (theme: Theme) =>
  theme === "light" || theme === "light-blue" ? "sun" : "moon";
"#,
            ),
            (
                paths[2],
                r#".theme-light-blue {
  color-scheme: light;
  --background: 210 60% 98%;
  --foreground: 218 42% 16%;
  --card: 210 50% 100%;
  --card-foreground: 218 42% 16%;
  --border: 210 32% 78%;
  --input: 210 32% 82%;
  --ring: 216 92% 48%;
  --primary: 224 76% 42%;
  --primary-foreground: 210 50% 100%;
  --info: 199 88% 48%;
  --destructive: 0 72% 45%;
  --success: 151 58% 34%;
}
"#,
            ),
            (
                paths[3],
                r#"import { applyTheme, restoreTheme, THEME_ORDER } from "../src/components/theme/ThemeProvider";

it("cycles dark -> light -> light-blue -> red -> dark", () => {
  expect(THEME_ORDER).toEqual(["dark", "light", "light-blue", "red"]);
});

it("restores and immediately applies a saved light-blue preference", () => {
  const root = document.documentElement;
  applyTheme(root, restoreTheme("light-blue"));
  expect(root.classList.contains("theme-light-blue")).toBe(true);
  expect(root.classList.contains("theme-dark")).toBe(false);
  expect(localStorage.getItem("rustgrid-theme")).toBe("light-blue");
});

it("preserves the dark fallback for missing and invalid preferences", () => {
  expect(restoreTheme(null)).toBe("dark");
  expect(restoreTheme("invalid")).toBe("dark");
});
"#,
            ),
            (
                paths[4],
                r#"const requiredTokens = [
  "background", "foreground", "card", "card-foreground", "border", "input",
  "ring", "primary", "primary-foreground", "info", "destructive", "success",
];

it("defines every semantic token for light-blue without conflating primary and info", () => {
  const lightBlue = readThemeTokens("theme-light-blue");
  expect(requiredTokens.every((token) => lightBlue[token])).toBe(true);
  expect(lightBlue.primary).not.toBe(lightBlue.info);
  expect(contrast(lightBlue.foreground, lightBlue.background)).toBeGreaterThanOrEqual(4.5);
});
"#,
            ),
        ];
        for (path, content) in implemented_files {
            performance.begin_model_call().unwrap();
            fs::write(work.path().join(path), content).unwrap();
            first_successful_write_call
                .get_or_insert(performance.phase_calls(ExecutionPhase::Implementation));
        }
        let repo = Repo {
            root: work.path().to_path_buf(),
        };
        let expected_paths = paths
            .iter()
            .map(|path| (*path).to_owned())
            .collect::<Vec<_>>();
        let changed_paths = completion_changed_paths(&repo, &base_sha).unwrap();
        assert_eq!(changed_paths, expected_paths);
        let reviewed_diff = completion_review_diff(work.path(), &changed_paths, &base_sha).unwrap();
        performance.transition(ExecutionPhase::DiffReview);
        performance.begin_model_call().unwrap();
        for path in paths {
            assert!(reviewed_diff.contains(path));
        }
        assert!(reviewed_diff.contains("dark\", \"light\", \"light-blue\", \"red"));
        assert!(reviewed_diff.contains("theme === \"light-blue\""));
        assert!(reviewed_diff.contains("--primary:"));
        assert!(reviewed_diff.contains("--info:"));
        assert!(reviewed_diff.contains("restoreTheme(\"light-blue\")"));
        assert!(reviewed_diff.contains("toBeGreaterThanOrEqual(4.5)"));

        let criterion = "Light-blue theme behavior is implemented and covered.".to_owned();
        let planned_changes = paths
            .iter()
            .enumerate()
            .map(|(index, path)| PlannedChange {
                change_id: format!("aops-226-target-{}", index + 1),
                parent_change_id: None,
                path: String::new(),
                targets: vec![PlannedTarget {
                    path: (*path).into(),
                    role: "required AOPS-226 target".into(),
                    new_file: false,
                    status: IntendedChangeStatus::Applied,
                }],
                change: format!("Implement light-blue behavior in {path}."),
                reason: "Complete the localized theme change.".into(),
                status: IntendedChangeStatus::Applied,
                acceptance_criteria: vec![criterion.clone()],
                test_coverage: vec!["focused theme tests".into()],
            })
            .collect::<Vec<_>>();
        let declaration = deterministic_complete_declaration(
            &planned_changes,
            std::slice::from_ref(&criterion),
            &changed_paths,
            &[],
            &[],
        )
        .unwrap();
        let implementation = ImplementationOutcome {
            summary: "Implemented all five light-blue theme targets.".into(),
            budget_exhausted: false,
            explicit_declaration: Some(declaration),
        };
        let validation = [
            ("focused-theme", "npm test -- theme"),
            ("test", "npm test"),
            ("build", "npm run build"),
        ]
        .into_iter()
        .map(|(id, command)| ValidationResult {
            id: id.into(),
            command: command.into(),
            status: "passed".into(),
            output: "passed once".into(),
        })
        .collect::<Vec<_>>();
        let plan = ImplementationPlan {
            implementation_status: "ready".into(),
            planned_changes,
            planned_new_files: vec![],
            planned_test_changes: vec![],
            remaining_unknowns: vec![],
            blocking_unknowns: vec![],
        };
        let completeness = completion_fallback(
            &implementation,
            None,
            Some(&plan),
            &[],
            &changed_paths,
            std::slice::from_ref(&criterion),
            &validation,
            ProjectVerificationPolicy::default(),
        );
        assert!(matches!(
            completeness.status,
            CompletionStatus::Complete | CompletionStatus::CompletePendingExternalReview
        ));
        performance.transition(ExecutionPhase::CompletionEvaluation);
        performance.begin_model_call().unwrap();
        let estimated_cost_micros = 360_000;
        assert!(first_successful_write_call.is_some_and(|call| call <= 6));
        assert!(performance.implementation_repair_calls() <= 10);
        assert!(performance.total_calls() <= 20);
        assert!(estimated_cost_micros <= model_cost_limit_for_target_count(paths.len()));
        assert!(fixture_started.elapsed() <= MAX_HOSTED_EXECUTION_DURATION);

        let commit = repo
            .commit_paths(&changed_paths, "AOPS-226: Add light-blue theme")
            .unwrap();
        assert!(
            repo.push(branch, &commit, "fixture-token", "https://github.com")
                .unwrap()
        );
        let remote_commit = command::checked(
            "git",
            [
                "--git-dir",
                remote.path().to_str().unwrap(),
                "rev-parse",
                &format!("refs/heads/{branch}"),
            ],
            work.path(),
        )
        .unwrap();
        assert_eq!(remote_commit, commit);
        assert!(repo.new_agent_paths(&BTreeSet::new()).unwrap().is_empty());
        let (publication_head, publication_paths) =
            committed_head_for_publication(&repo, &base_sha)
                .unwrap()
                .expect("the committed base-to-HEAD implementation must remain publishable");
        assert_eq!(publication_head, commit);
        assert_eq!(publication_paths, expected_paths);

        let Some((github_base, requests, server)) = request_sequence_server(vec![
            ("200 OK", json!([])),
            (
                "201 Created",
                json!({
                    "number": 226,
                    "html_url": "https://github.example/RustGrid/example/pull/226",
                    "node_id": "PR_AOPS_226",
                    "draft": false
                }),
            ),
        ]) else {
            return;
        };
        let github = GitHubClient::new("fixture-token", github_base.as_str()).unwrap();
        let mut manifest = test_manifest(Uuid::from_u128(226));
        manifest.ticket_key = "AOPS-226".into();
        manifest.ticket_title = "Add the light-blue theme".into();
        manifest.github.branch = branch.into();
        manifest.github.base_ref = "main".into();
        let pull = find_or_create_hosted_pull_request(
            &github,
            &RepoConfig {
                owner: "RustGrid".into(),
                name: "example".into(),
            },
            &manifest,
            &validation,
            &completeness,
            false,
        )
        .unwrap();
        assert_eq!(pull.number, 226);
        let lookup = requests.recv().unwrap();
        let creation = requests.recv().unwrap();
        server.join().unwrap();
        assert!(lookup.starts_with("GET /api/v3/repos/RustGrid/example/pulls?"));
        assert!(creation.starts_with("POST /api/v3/repos/RustGrid/example/pulls HTTP/1.1"));
        let creation_body: Value =
            serde_json::from_str(creation.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(creation_body["head"], branch);
        assert_eq!(creation_body["draft"], false);

        let result = HostedResult {
            summary: implementation.summary,
            branch: branch.into(),
            commit: commit.clone(),
            pull_request: PullRequestResult {
                number: pull.number,
                url: pull.html_url,
            },
            validation,
            completeness,
            terminal_telemetry: TerminalTelemetry {
                model_calls_used: performance.total_calls(),
                input_tokens: 48_000,
                output_tokens: 8_000,
                estimated_cost_micros,
                usage: ToolUsage {
                    reads: 5,
                    searches: 2,
                    writes: 5,
                    successful_writes: 5,
                    validation_commands: 3,
                    required_validations: 3,
                    ..ToolUsage::default()
                },
                changed_paths: changed_paths.clone(),
                last_successful_action: json!({
                    "phase": "publication",
                    "action": "pull_request_created",
                    "pull_request_number": 226,
                }),
                phase_reached: Some(ExecutionPhase::Publication),
                plan: plan.planned_changes.clone(),
                remaining_work: Vec::new(),
                validation_evidence: Vec::new(),
                notebook_revision: 18,
            },
        };
        assert!(hosted_result_can_succeed(&result));

        let execution_id = manifest.execution.execution_id;
        let Some((api_root, result_requests, result_server)) = request_sequence_server(vec![
            ("200 OK", json!({})),
            ("200 OK", json!({})),
            ("200 OK", json!({})),
        ]) else {
            return;
        };
        let api = test_api_client(api_root, execution_id);
        report_successful_hosted_result(
            &api,
            execution_id,
            "2026-08-01T10:00:00Z",
            "2026-08-01T10:05:00Z",
            &result,
        )
        .unwrap();

        let telemetry_request = result_requests.recv().unwrap();
        let result_event_request = result_requests.recv().unwrap();
        let completion_request = result_requests.recv().unwrap();
        result_server.join().unwrap();

        assert!(telemetry_request.starts_with(&format!(
            "POST /api/v1/executions/{execution_id}/telemetry/batch HTTP/1.1"
        )));
        let telemetry_body: Value =
            serde_json::from_str(telemetry_request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(telemetry_body["events"][0]["type"], "execution.completed");
        assert_eq!(
            telemetry_body["events"][0]["execution"]["status"],
            "succeeded"
        );

        assert!(result_event_request.starts_with(&format!(
            "POST /api/v1/executions/{execution_id}/worker-events HTTP/1.1"
        )));
        let result_event_body: Value =
            serde_json::from_str(result_event_request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(result_event_body["event_type"], "result");
        assert_eq!(result_event_body["data"]["status"], "completed");
        assert_eq!(result_event_body["data"]["mission_outcome"], "complete");
        assert_eq!(result_event_body["data"]["head_sha"], commit);
        assert_eq!(result_event_body["data"]["pull_request_number"], 226);
        assert_eq!(
            result_event_body["data"]["terminal_telemetry"]["model_calls_used"],
            performance.total_calls()
        );
        assert_eq!(
            result_event_body["data"]["terminal_telemetry"]["usage"]["successful_writes"],
            5
        );
        assert_eq!(
            result_event_body["data"]["terminal_telemetry"]["usage"]["validation_commands"],
            3
        );
        assert_eq!(
            result_event_body["data"]["terminal_telemetry"]["changed_paths"],
            json!(changed_paths)
        );

        assert!(completion_request.starts_with(&format!(
            "POST /api/v1/executions/{execution_id}/complete HTTP/1.1"
        )));
        let completion_body: Value =
            serde_json::from_str(completion_request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(completion_body["status"], "completed");
        assert_eq!(completion_body["mission_outcome"], "complete");
        assert_eq!(completion_body["process_health"], "healthy");
        assert_eq!(
            completion_body["completion_evaluation"]["status"],
            "complete"
        );
        assert_eq!(completion_body["head_branch"], branch);
        assert_eq!(completion_body["head_sha"], commit);
        assert_eq!(completion_body["pull_request_number"], 226);
        assert_eq!(
            completion_body["pull_request_url"],
            "https://github.example/RustGrid/example/pull/226"
        );
        assert_eq!(
            completion_body["output_summary"],
            "Implemented all five light-blue theme targets."
        );
    }

    #[test]
    fn zero_diff_preparation_block_is_a_healthy_domain_result_with_live_telemetry() {
        let mut failure = test_execution_failure(
            "implementation_progress_missing",
            "Implementation could not begin after guided recovery.",
        );
        let current_plan = vec![test_planned_change()];
        let remaining_work = derive_remaining_work(&intended_changes_from_plan(&current_plan));
        let failed_read = ToolProgressRecord {
            execution_attempt: 18,
            model_call: 7,
            phase: ExecutionPhase::Implementation,
            tool: "read_file".into(),
            target: Some("src/components/theme/ThemeProvider.tsx".into()),
            class: ToolProgressClass::RecoverableFailure,
            outcome_signature: "read_file:range_invalid:ThemeProvider.tsx".into(),
            detail: "requested line range exceeded the file length; valid range is 1-120".into(),
            repository_progress: false,
        };
        let mut stale_validation = new_running_evidence(
            "focused-old-tree".into(),
            "focused-theme".into(),
            ValidationGateType::FocusedTest,
            "npm test -- theme".into(),
            "old-fingerprint".into(),
            "old-tree".into(),
            "lock".into(),
            ValidationSource::WorkerRequired,
        );
        stale_validation.status = ValidationStatus::Superseded;

        failure.phase = ExecutionPhase::Implementation;
        failure.model_calls_used = 8;
        failure.model_calls_limit = 20;
        failure.model_calls_remaining = 12;
        failure.phase_calls_used = 8;
        failure.phase_calls_limit = 8;
        failure.input_tokens = 12_345;
        failure.output_tokens = 2_345;
        failure.estimated_cost_micros = 456_789;
        failure.usage = ToolUsage {
            reads: 6,
            failed_reads: 3,
            searches: 2,
            writes: 2,
            failed_writes: 2,
            write_preflight_rejections: 1,
            write_execution_failures: 1,
            ..ToolUsage::default()
        };
        failure.last_successful_action = json!({
            "model_call": 6,
            "phase": "implementation",
            "tool": "search_text",
            "target": "src/components/theme/ThemeProvider.tsx",
        });
        failure.failed_tool_operations = vec![failed_read];
        failure.current_plan = current_plan.clone();
        failure.validation_evidence = vec![stale_validation];
        failure.notebook_revision = 18;
        let failure = classify_implementation_preparation_failure(failure, &remaining_work);

        assert_eq!(failure.status, "blocked");
        assert_eq!(failure.category, "implementation_blocked");
        assert_eq!(failure.process_health, "healthy");
        assert_eq!(failure.mission_outcome, "blocked");
        assert_eq!(
            failure.blocker.as_deref(),
            Some("implementation_preparation_failed")
        );
        assert!(failure.resumable);
        assert_eq!(failure.code, "implementation_preparation_failed");
        assert_eq!(failure.phase, ExecutionPhase::Implementation);
        assert_eq!(
            failure.message,
            "Implementation could not begin after guided recovery."
        );
        assert_eq!(
            failure.underlying_error.message,
            "implementation_progress_missing"
        );
        assert!(failure.recoverable);
        assert_eq!(failure.resume_phase, "implementation");
        assert_eq!(failure.remaining_work, remaining_work);
        assert!(
            failure
                .recommended_action
                .contains("current planned target")
        );

        let error = anyhow::Error::new(failure);
        let (code, _) = safe_failure(&error, false);
        let diagnostics = failure_diagnostics(&error, false);
        let failure = error
            .downcast_ref::<HostedAgentExecutionFailure>()
            .expect("the classified failure must remain structured");
        let event = blocked_result_event_payload(failure, diagnostics.clone());

        assert_eq!(code, "implementation_preparation_failed");
        assert_ne!(code, "hosted_agent_execution_failed");
        assert_eq!(diagnostics["process_health"], "healthy");
        assert_eq!(diagnostics["mission_outcome"], "blocked");
        assert_eq!(diagnostics["resume_phase"], "implementation");
        assert_eq!(
            diagnostics["recommended_action"],
            failure.recommended_action
        );
        assert_eq!(diagnostics["model_calls_used"], 8);
        assert_eq!(diagnostics["model_calls_limit"], 20);
        assert_eq!(diagnostics["model_calls_remaining"], 12);
        assert_eq!(diagnostics["phase_calls_used"], 8);
        assert_eq!(diagnostics["phase_calls_limit"], 8);
        assert_eq!(diagnostics["input_tokens"], 12_345);
        assert_eq!(diagnostics["output_tokens"], 2_345);
        assert_eq!(diagnostics["estimated_cost_micros"], 456_789);
        assert_eq!(diagnostics["usage"]["reads"], 6);
        assert_eq!(diagnostics["usage"]["failed_reads"], 3);
        assert_eq!(diagnostics["usage"]["searches"], 2);
        assert_eq!(diagnostics["usage"]["writes"], 2);
        assert_eq!(diagnostics["usage"]["failed_writes"], 2);
        assert_eq!(diagnostics["usage"]["write_preflight_rejections"], 1);
        assert_eq!(diagnostics["usage"]["write_execution_failures"], 1);
        assert_eq!(diagnostics["usage"]["validation_commands"], 0);
        assert_eq!(diagnostics["last_successful_action"]["tool"], "search_text");
        assert_eq!(diagnostics["phase"], "implementation");
        assert_eq!(diagnostics["current_plan"], json!(current_plan));
        assert_eq!(diagnostics["remaining_work"], json!(remaining_work));
        assert_eq!(
            diagnostics["failed_tool_operations"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            diagnostics["validation_evidence"].as_array().unwrap().len(),
            1
        );
        assert_eq!(diagnostics["notebook_revision"], 18);
        assert_eq!(diagnostics["changed_paths"], json!([]));
        assert_eq!(
            diagnostics["failed_tool_operations"][0]["target"],
            "src/components/theme/ThemeProvider.tsx"
        );
        assert!(
            diagnostics["failed_tool_operations"][0]["detail"]
                .as_str()
                .unwrap()
                .contains("valid range is 1-120")
        );

        assert_eq!(event["status"], "blocked");
        assert_eq!(event["mission_outcome"], "blocked");
        assert_eq!(event["process_health"], "healthy");
        assert_eq!(event["reason_code"], "implementation_preparation_failed");
        assert_eq!(event["resumable"], true);
        assert_eq!(event["resume_phase"], "implementation");
        assert_eq!(event["changed_paths"], json!([]));
        assert_eq!(event["remaining_work"], json!(remaining_work));
        assert_eq!(event["terminal_telemetry"]["model_calls_used"], 8);
        assert_eq!(event["terminal_telemetry"]["input_tokens"], 12_345);
        assert_eq!(event["terminal_telemetry"]["output_tokens"], 2_345);
        assert_eq!(
            event["terminal_telemetry"]["estimated_cost_micros"],
            456_789
        );
        assert_eq!(event["terminal_telemetry"]["usage"]["reads"], 6);
        assert_eq!(event["terminal_telemetry"]["usage"]["failed_reads"], 3);
        assert_eq!(event["terminal_telemetry"]["usage"]["searches"], 2);
        assert_eq!(event["terminal_telemetry"]["usage"]["writes"], 2);
        assert_eq!(event["terminal_telemetry"]["usage"]["failed_writes"], 2);
        assert_eq!(
            event["terminal_telemetry"]["usage"]["write_preflight_rejections"],
            1
        );
        assert_eq!(
            event["terminal_telemetry"]["usage"]["write_execution_failures"],
            1
        );
        assert_eq!(
            event["terminal_telemetry"]["usage"]["validation_commands"],
            0
        );
        assert_eq!(event["terminal_telemetry"]["changed_paths"], json!([]));
        assert_eq!(
            event["terminal_telemetry"]["last_successful_action"]["tool"],
            "search_text"
        );
        assert_eq!(
            event["terminal_telemetry"]["phase_reached"],
            "implementation"
        );
        assert_eq!(event["terminal_telemetry"]["plan"], json!(current_plan));
        assert_eq!(
            event["terminal_telemetry"]["remaining_work"],
            json!(remaining_work)
        );
        assert_eq!(
            event["terminal_telemetry"]["validation_evidence"][0]["evidence_id"],
            "focused-old-tree"
        );
        assert_eq!(event["terminal_telemetry"]["notebook_revision"], 18);
        assert_eq!(
            event["failure"]["failed_tool_operations"][0]["class"],
            "recoverable_failure"
        );
        assert_eq!(event["failure"], diagnostics);
    }

    #[test]
    fn zero_diff_validation_entry_executes_zero_repository_gates() {
        let decision =
            validation_entry_decision(ImplementationCompletionStatus::Preparing, 0, false, false);
        let mut executed_gate_count = 0;
        let dispatched = dispatch_validation_gates(decision, || {
            executed_gate_count += 1;
            Ok("gate ran")
        })
        .unwrap();

        assert_eq!(
            decision,
            ValidationEntryDecision::ForbiddenNoImplementationChanges
        );
        assert!(dispatched.is_none());
        assert_eq!(executed_gate_count, 0);
    }

    #[test]
    fn partial_and_blocked_domain_outcomes_are_healthy_even_with_incomplete_gates() {
        for status in [CompletionStatus::Partial, CompletionStatus::Blocked] {
            let result = HostedResult {
                summary: "Published resumable work.".into(),
                branch: "rustgrid/resumable".into(),
                commit: "a".repeat(40),
                pull_request: PullRequestResult {
                    number: 143,
                    url: "https://github.com/RustGrid/example/pull/143".into(),
                },
                validation: vec![ValidationResult {
                    id: "test".into(),
                    command: "npm test".into(),
                    status: "failed".into(),
                    output: "one remaining failure".into(),
                }],
                completeness: test_completion_evaluation(status),
                terminal_telemetry: TerminalTelemetry::default(),
            };
            assert!(hosted_result_can_succeed(&result));
        }
        let mut complete = HostedResult {
            summary: "Complete.".into(),
            branch: "rustgrid/complete".into(),
            commit: "b".repeat(40),
            pull_request: PullRequestResult {
                number: 144,
                url: "https://github.com/RustGrid/example/pull/144".into(),
            },
            validation: vec![ValidationResult {
                id: "build".into(),
                command: "npm run build".into(),
                status: "failed".into(),
                output: String::new(),
            }],
            completeness: test_completion_evaluation(CompletionStatus::Complete),
            terminal_telemetry: TerminalTelemetry::default(),
        };
        assert!(!hosted_result_can_succeed(&complete));
        complete.completeness.status = CompletionStatus::Uncertain;
        assert!(!hosted_result_can_succeed(&complete));
    }

    #[test]
    fn model_budget_handoff_preserves_work_without_claiming_completion() {
        let empty = Vec::new();
        assert!(model_budget_handoff_summary(true, &empty).is_none());

        let changed = vec!["src/theme.css".to_owned()];
        assert!(model_budget_handoff_summary(false, &changed).is_none());
        assert!(
            model_budget_handoff_summary(true, &changed).is_some_and(
                |summary| summary.contains("passing gates alone cannot mark it complete")
            )
        );
        let implementation = ImplementationOutcome {
            summary: "partial edit".into(),
            budget_exhausted: true,
            explicit_declaration: None,
        };
        let result = completion_fallback(
            &implementation,
            None,
            None,
            &[],
            &changed,
            &["Theme can be selected".into()],
            &[],
            ProjectVerificationPolicy::default(),
        );
        assert_ne!(result.status, CompletionStatus::Complete);
        let hosted_result = HostedResult {
            summary: "partial edit".into(),
            branch: "rustgrid/partial".into(),
            commit: "a".repeat(40),
            pull_request: PullRequestResult {
                number: 1,
                url: "https://github.com/RustGrid/example/pull/1".into(),
            },
            validation: vec![ValidationResult {
                id: "test".into(),
                command: "cargo test".into(),
                status: "passed".into(),
                output: String::new(),
            }],
            completeness: result,
            terminal_telemetry: TerminalTelemetry::default(),
        };
        assert!(!hosted_result_can_succeed(&hosted_result));
    }

    #[test]
    fn forty_call_budget_prioritizes_implementation_and_keeps_finalization_usable() {
        let normal = phase_budget_allocation(DEFAULT_HOSTED_MODEL_CALLS);
        assert_eq!(normal.discovery_maximum, 5);
        assert_eq!(normal.planning_maximum, 3);
        assert_eq!(normal.implementation_repair_reserved, 18);
        assert_eq!(normal.diff_review_reserved, 3);
        assert_eq!(normal.completion_evaluation_reserved, 3);
        assert_eq!(normal.total(), 32);
    }

    #[test]
    fn canonical_forty_call_budget_reaches_the_worker_unchanged() {
        let execution_id = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
        let mut manifest = test_manifest(execution_id);
        manifest.manifest_version = 4;
        manifest.model_call_budget = Some(40);
        manifest.requested_model_call_budget = Some(40);
        manifest.resolved_model_call_budget = Some(40);
        manifest.budget_source = Some(BudgetSource::UserSelected);
        manifest.clamped = Some(false);
        manifest.clamp_reason = Some(None);
        manifest.execution.maximum_model_calls = Some(40);
        manifest.ai_gateway.maximum_model_calls = 40;

        let budget = manifest.budget_audit().unwrap();
        assert_eq!(budget.requested_model_call_budget, 40);
        assert_eq!(budget.resolved_model_call_budget, 40);
        assert_eq!(budget.worker_received_model_call_budget, 40);
        assert_eq!(budget.contract, "canonical");
        let environment = test_environment(execution_id);
        let api = test_api_client(Url::parse("http://127.0.0.1:8080/").unwrap(), execution_id);
        manifest.validate(execution_id, &environment, &api).unwrap();
    }

    #[test]
    fn repository_wide_signed_budget_can_reach_one_hundred_calls() {
        let execution_id = Uuid::from_u128(0x22222222_2222_4222_8222_222222222222);
        let mut manifest = test_manifest(execution_id);
        manifest.manifest_version = 4;
        manifest.model_call_budget = Some(100);
        manifest.requested_model_call_budget = Some(100);
        manifest.resolved_model_call_budget = Some(100);
        manifest.budget_source = Some(BudgetSource::UserSelected);
        manifest.clamped = Some(false);
        manifest.clamp_reason = Some(None);
        manifest.execution.maximum_model_calls = Some(100);
        manifest.ai_gateway.maximum_model_calls = 100;

        let environment = test_environment(execution_id);
        let api = test_api_client(Url::parse("http://127.0.0.1:8080/").unwrap(), execution_id);
        manifest.validate(execution_id, &environment, &api).unwrap();

        manifest.model_call_budget = Some(101);
        manifest.requested_model_call_budget = Some(101);
        manifest.resolved_model_call_budget = Some(101);
        manifest.execution.maximum_model_calls = Some(101);
        manifest.ai_gateway.maximum_model_calls = 101;
        assert!(manifest.validate(execution_id, &environment, &api).is_err());
    }

    #[test]
    fn budget_mismatch_is_typed_before_model_execution() {
        let execution_id = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
        let mut manifest = test_manifest(execution_id);
        manifest.manifest_version = 4;
        manifest.model_call_budget = Some(40);
        manifest.requested_model_call_budget = Some(40);
        manifest.resolved_model_call_budget = Some(40);
        manifest.budget_source = Some(BudgetSource::UserSelected);
        manifest.clamped = Some(false);
        manifest.clamp_reason = Some(None);
        manifest.execution.maximum_model_calls = Some(20);
        manifest.ai_gateway.maximum_model_calls = 20;

        let error = manifest.budget_audit().unwrap_err();
        assert!(error.downcast_ref::<ExecutionBudgetMismatch>().is_some());
        let (code, message) = safe_failure(&error, false);
        assert_eq!(code, "execution_budget_mismatch");
        assert!(message.contains("worker-received"));
        let diagnostics = failure_diagnostics(&error, false);
        assert_eq!(diagnostics["requested_model_call_budget"], 40);
        assert_eq!(diagnostics["resolved_model_call_budget"], 40);
        assert_eq!(diagnostics["worker_received_model_call_budget"], 20);
        assert_eq!(diagnostics["model_calls_used"], 0);
    }

    #[test]
    fn canonical_budget_distinguishes_a_null_clamp_reason_from_a_missing_field() {
        #[derive(Deserialize)]
        struct ClampReasonPresence {
            #[serde(default, deserialize_with = "deserialize_present_nullable")]
            clamp_reason: Option<Option<String>>,
        }

        let missing: ClampReasonPresence = serde_json::from_value(json!({})).unwrap();
        let explicitly_null: ClampReasonPresence =
            serde_json::from_value(json!({"clamp_reason": null})).unwrap();
        assert_eq!(missing.clamp_reason, None);
        assert_eq!(explicitly_null.clamp_reason, Some(None));
    }

    #[test]
    fn explicit_legacy_twenty_call_budget_remains_supported() {
        let execution_id = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
        let mut manifest = test_manifest(execution_id);
        manifest.execution.maximum_model_calls = Some(20);
        manifest.ai_gateway.maximum_model_calls = 20;

        let budget = manifest.budget_audit().unwrap();
        assert_eq!(budget.worker_received_model_call_budget, 20);
        assert_eq!(budget.contract, "legacy_signed_manifest");
        let allocation = phase_budget_allocation(20);
        assert_eq!(
            (
                allocation.discovery_maximum,
                allocation.planning_maximum,
                allocation.implementation_repair_reserved,
                allocation.diff_review_reserved,
                allocation.completion_evaluation_reserved,
            ),
            (5, 3, 10, 1, 1)
        );
    }

    #[test]
    fn hosted_context_keeps_only_recent_turns_after_notebook_checkpointing() {
        let initial = json!({"role": "user", "content": "mission"});
        let mut turns = (0..12)
            .map(|index| vec![json!({"role": "assistant", "content": format!("turn-{index}")})])
            .collect::<VecDeque<_>>();
        compact_hosted_turns(&mut turns);
        assert_eq!(turns.len(), MAX_HOSTED_TURN_WINDOWS);
        assert_eq!(turns[0][0]["content"], "turn-9");

        let mut input = vec![initial.clone()];
        input.extend(turns.iter().flatten().cloned());
        let mut request = json!({
            "model": "gpt-5.6-sol",
            "input": input
        });
        fit_request_to_input_ceiling(&mut request, &initial, &mut turns, 100_000).unwrap();
        assert_eq!(turns.len(), MAX_HOSTED_TURN_WINDOWS);

        let reduced_ceiling = serde_json::to_vec(&request).unwrap().len() - 1;
        fit_request_to_input_ceiling(&mut request, &initial, &mut turns, reduced_ceiling).unwrap();
        assert!(turns.len() < MAX_HOSTED_TURN_WINDOWS);
        assert_eq!(request["input"].as_array().unwrap().first(), Some(&initial));
    }

    #[test]
    fn implementation_context_is_phase_specific_and_below_eight_thousand_tokens() {
        let mut notebook = test_discovery_notebook(ExecutionPhase::Implementation);
        notebook.architecture_findings = vec!["historical-payload-marker ".repeat(20_000)];
        notebook.planned_changes = vec![test_planned_change()];
        notebook.intended_changes = intended_changes_from_plan(&notebook.planned_changes);
        notebook.remaining_work_v2 = derive_remaining_work(&notebook.intended_changes);
        let compact = compact_notebook_for_phase(&notebook, ExecutionPhase::Implementation);
        assert!(compact.len() <= 28 * 1024);
        assert!(!compact.contains("historical-payload-marker"));
        assert!(!compact.contains("occurred_at"));
    }

    #[test]
    fn localized_discovery_has_a_twelve_thousand_token_equivalent_request_ceiling() {
        assert_eq!(
            phase_request_input_ceiling(ExecutionPhase::Discovery, 100 * 1024),
            MAX_DISCOVERY_REQUEST_BYTES
        );
        assert_eq!(
            phase_request_input_ceiling(ExecutionPhase::ArtifactRepair, 100 * 1024),
            MAX_DISCOVERY_REQUEST_BYTES
        );
        assert_eq!(
            phase_request_input_ceiling(ExecutionPhase::Implementation, 100 * 1024),
            100 * 1024
        );
        let guidance = visual_impact_guidance("Add a light-blue theme");
        assert!(guidance.contains("at most three representative consumers"));
        assert!(guidance.contains("Record the compact impact map"));
    }

    #[test]
    fn centralized_localized_discovery_stops_after_core_boundaries_and_three_consumers() {
        let mut notebook = test_discovery_notebook(ExecutionPhase::Discovery);
        notebook.goal = "Add a light-blue theme".into();
        notebook.architecture_findings.clear();
        record_centralized_discovery_finding(
            &mut notebook,
            "Centralized semantic CSS variables are confirmed.",
        );
        notebook.files_inspected = vec![
            "src/components/theme/ThemeProvider.tsx".into(),
            "src/components/theme/ThemeToggle.tsx".into(),
            "src/styles/globals.css".into(),
            "tests/theme-provider.test.tsx".into(),
            "package.json".into(),
            "src/components/Button.tsx".into(),
            "src/components/Input.tsx".into(),
            "src/components/Status.tsx".into(),
        ];
        let coverage = localized_discovery_coverage(&notebook);
        assert!(coverage.centralized_abstraction);
        assert_eq!(coverage.representative_consumers, 3);
        assert!(localized_discovery_should_stop(coverage));
        assert!(
            validate_localized_discovery_scope(&notebook, &["src/components/Card.tsx"])
                .unwrap_err()
                .to_string()
                .contains("localized_discovery_complete")
        );

        notebook.files_inspected = vec!["src/components/theme/ThemeProvider.tsx".into()];
        notebook.discovery_paths_sampled = vec![
            "src/components/Button.tsx".into(),
            "src/components/Input.tsx".into(),
            "src/components/Status.tsx".into(),
        ];
        assert!(
            validate_localized_discovery_scope(&notebook, &["src/components/Card.tsx"])
                .unwrap_err()
                .to_string()
                .contains("localized_discovery_consumer_limit")
        );
        assert!(
            validate_localized_discovery_scope(&notebook, &["src/components/Status.tsx"]).is_ok()
        );
        let directory_search = json!({"path": "src"});
        assert!(
            discovery_requested_paths("search_text", directory_search.as_object().unwrap())
                .is_empty()
        );
        let directory_listing = json!({"path": "."});
        assert!(
            discovery_requested_paths("list_files", directory_listing.as_object().unwrap())
                .is_empty()
        );
        assert!(
            validate_localized_discovery_scope(&notebook, &["tests/theme-provider.test.tsx"])
                .is_ok()
        );

        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("src/components")).unwrap();
        for name in ["Button.tsx", "Card.tsx", "Input.tsx", "Status.tsx"] {
            fs::write(
                directory.path().join("src/components").join(name),
                "const color = 'hardcoded';\n",
            )
            .unwrap();
        }
        let known = BTreeSet::from([
            "src/components/Button.tsx".to_owned(),
            "src/components/Input.tsx".to_owned(),
        ]);
        let search = search_repo(
            directory.path(),
            "src/components",
            "hardcoded",
            &["tsx".into()],
            0,
            Some(1),
            &known,
        )
        .unwrap();
        assert!(
            search
                .matched_paths
                .iter()
                .filter(|path| !known.contains(*path))
                .count()
                <= 1
        );
    }

    #[test]
    fn lifecycle_transition_table_rejects_skipping_validation() {
        assert!(legal_phase_transition(
            ExecutionPhase::Implementation,
            ExecutionPhase::Validation
        ));
        assert!(!legal_phase_transition(
            ExecutionPhase::Implementation,
            ExecutionPhase::DiffReview
        ));
        assert!(!legal_phase_transition(
            ExecutionPhase::Validation,
            ExecutionPhase::Publication
        ));
    }

    #[test]
    fn hosted_budget_thresholds_guide_completion_before_the_signed_limit() {
        assert!(hosted_budget_advisory(27, 40).is_none());
        assert_eq!(
            hosted_budget_advisory(28, 40).map(|advisory| advisory.0),
            Some(70)
        );
        let finalization = hosted_budget_advisory(36, 40).unwrap();
        assert_eq!(finalization.0, 90);
        assert!(
            finalization
                .2
                .contains("smallest complete validated result")
        );
    }

    #[test]
    fn five_target_cost_ceiling_is_enforced_before_provider_dispatch() {
        assert_eq!(model_cost_limit_for_target_count(5), 2_000_000);
        let guard = CostGuard {
            estimated_cost_micros: 1_750_000,
            hard_limit_micros: model_cost_limit_for_target_count(5),
            ..CostGuard::default()
        };
        let mut request = json!({
            "input": "implementation context ".repeat(1_000),
            "max_output_tokens": 16_384,
        });
        assert!(constrain_request_to_cost_limit(&mut request, &guard).unwrap());
        let input_upper = u64::try_from(serde_json::to_vec(&request).unwrap().len()).unwrap();
        let output_upper = request["max_output_tokens"].as_u64().unwrap();
        assert!(output_upper < 16_384);
        assert!(
            guard
                .estimated_cost_micros
                .saturating_add(input_upper.saturating_mul(5))
                .saturating_add(output_upper.saturating_mul(15))
                <= guard.hard_limit_micros
        );

        let exhausted = CostGuard {
            estimated_cost_micros: 1_999_999,
            hard_limit_micros: 2_000_000,
            ..CostGuard::default()
        };
        assert!(!constrain_request_to_cost_limit(&mut request, &exhausted).unwrap());
    }

    #[test]
    fn missing_provider_usage_is_accounted_with_the_dispatched_request_upper_bound() {
        let request = json!({
            "input": [{"role": "user", "content": "bounded implementation context"}],
            "max_output_tokens": 512,
        });
        let request_bytes = u64::try_from(serde_json::to_vec(&request).unwrap().len()).unwrap();
        let (input_tokens, output_tokens, estimated) =
            model_usage_for_accounting(&request, &json!({"output": []})).unwrap();
        assert_eq!(input_tokens, request_bytes);
        assert_eq!(output_tokens, 512);
        assert!(estimated);

        let (input_tokens, output_tokens, estimated) = model_usage_for_accounting(
            &request,
            &json!({"usage": {"input_tokens": 23, "output_tokens": 17}}),
        )
        .unwrap();
        assert_eq!((input_tokens, output_tokens), (23, 17));
        assert!(!estimated);
    }

    #[test]
    fn later_attempt_continues_on_the_same_preserved_branch() {
        assert!(!should_continue_implementation(true, true, 1));
        assert!(should_continue_implementation(true, true, 2));
        assert!(should_continue_implementation(false, true, 1));
        assert!(should_continue_implementation(true, false, 1));
    }

    #[test]
    fn partial_branch_changes_create_explicit_continuation_guidance() {
        let partial_run = PartialRunContext {
            pull_request_number: 138,
            changed_paths: vec![
                "src/components/theme/ThemeProvider.tsx".into(),
                "tests/theme-provider.test.tsx".into(),
            ],
            remaining_work: vec!["Add the planned end-to-end test.".into()],
        };
        let guidance = partial_implementation_guidance(Some(&partial_run));

        assert!(guidance.contains("Existing partial implementation detected"));
        assert!(guidance.contains("draft pull request #138"));
        assert!(guidance.contains("src/components/theme/ThemeProvider.tsx"));
        assert!(guidance.contains("tests/theme-provider.test.tsx"));
        assert!(guidance.contains("Add the planned end-to-end test."));
        assert!(guidance.contains("compare the existing implementation"));
        assert!(guidance.contains("Preserve correct completed work"));
        assert!(guidance.contains("continue from the current branch state"));
        assert!(guidance.contains("not proof that the mission is complete"));
    }

    #[test]
    fn clean_branch_does_not_claim_that_partial_work_exists() {
        assert!(partial_implementation_guidance(None).is_empty());
    }

    #[test]
    fn partial_run_detection_requires_a_later_attempt_resumed_draft_and_existing_diff() {
        let pull_request = PullRequest {
            number: 138,
            html_url: "https://github.com/RustGrid/example/pull/138".into(),
            node_id: Some("PR_node".into()),
            draft: true,
            body: Some(
                "⚠️ **INCOMPLETE — continue implementation before review or merge**\n\n\
Remaining work:\n\
- Add the planned end-to-end test.\n\
- Reconcile the failed source edit.\n\n\
Technical validation:\n- cargo test: passed"
                    .into(),
            ),
        };
        let changed_paths = vec!["src/theme.rs".into()];

        let detected =
            detect_partial_run(Some(&pull_request), true, 2, changed_paths.clone()).unwrap();
        assert_eq!(detected.pull_request_number, 138);
        assert_eq!(detected.changed_paths, changed_paths);
        assert_eq!(
            detected.remaining_work,
            vec![
                "Add the planned end-to-end test.",
                "Reconcile the failed source edit."
            ]
        );
        assert!(
            detect_partial_run(Some(&pull_request), true, 1, vec!["src/theme.rs".into()]).is_none()
        );
        assert!(
            detect_partial_run(Some(&pull_request), false, 2, vec!["src/theme.rs".into()])
                .is_none()
        );
        assert!(detect_partial_run(Some(&pull_request), true, 2, Vec::new()).is_none());

        let complete_pull_request = PullRequest {
            draft: false,
            ..pull_request
        };
        assert!(
            detect_partial_run(
                Some(&complete_pull_request),
                true,
                2,
                vec!["src/theme.rs".into()]
            )
            .is_none()
        );
    }

    #[test]
    fn recovered_partial_run_starts_from_planning_with_authoritative_remaining_work() {
        let mut manifest = test_manifest(Uuid::from_u128(17));
        manifest.execution.attempt_number = 2;
        manifest.run.attempt = 2;
        manifest.run.input_prompt = "\
Implement theme support.\n\n\
## Acceptance criteria\n\
- Theme selection persists.\n\
- Existing views use shared tokens.\n"
            .into();
        let partial_run = PartialRunContext {
            pull_request_number: 138,
            changed_paths: vec!["src/theme.rs".into(), "tests/theme.rs".into()],
            remaining_work: vec!["Add browser coverage.".into()],
        };

        let notebook = new_worker_notebook(&manifest, "fingerprint".into(), Some(&partial_run));
        let (impact_map, implementation_plan, phase) = notebook_orchestration_state(&notebook);

        assert_eq!(phase, ExecutionPhase::Planning);
        assert!(impact_map.is_some());
        assert!(implementation_plan.is_none());
        assert_eq!(notebook.remaining_work, vec!["Add browser coverage."]);
        assert_eq!(
            notebook.acceptance_criteria,
            vec![
                "Theme selection persists.",
                "Existing views use shared tokens."
            ]
        );
        assert_eq!(
            notebook.impact_map[0].candidate_paths,
            vec!["src/theme.rs", "tests/theme.rs"]
        );
    }

    #[test]
    fn resumed_notebook_skips_completed_discovery_and_planning() {
        let notebook = WorkerNotebook {
            schema_version: 1,
            revision: 12,
            goal: "Apply a complete theme".into(),
            acceptance_criteria: vec!["All surfaces use the theme".into()],
            acceptance_criteria_v2: vec![impact_map::AcceptanceCriterion {
                id: "ac-1".into(),
                text: "All surfaces use the theme".into(),
            }],
            phase: ExecutionPhase::DiffReview,
            implementation_substate: ImplementationSubstate::Preparing,
            repository_base_sha: "a".repeat(40),
            branch: "rustgrid/aops-226-deadbeef".into(),
            repository_fingerprint: "b".repeat(64),
            execution_attempt: 2,
            architecture_findings: vec!["Tokens are centralized.".into()],
            impact_map: vec![ImpactArea {
                area_id: "area-tokens".into(),
                name: "tokens".into(),
                candidate_paths: vec!["src/theme.css".into()],
                evidence: vec![impact_map::ImpactEvidence {
                    evidence_type: impact_map::EvidenceType::FileRead,
                    path: Some("src/theme.css".into()),
                    query: None,
                    description: "inspected".into(),
                }],
                reason: "Shared token source".into(),
                acceptance_criteria_ids: vec!["ac-1".into()],
            }],
            impact_map_v2: None,
            impact_map_artifact: ArtifactCheckpoint {
                semantic_status: ArtifactSemanticStatus::Sufficient,
                persistence_status: ArtifactPersistenceStatus::Persisted,
                ..ArtifactCheckpoint::default()
            },
            impact_map_invalid_payload: None,
            impact_evidence: vec![],
            files_inspected: vec!["src/theme.css".into()],
            read_ranges_inspected: vec!["src/theme.css:1-400".into()],
            searches_completed: vec!["literal:src:theme".into()],
            discovery_paths_sampled: vec![],
            planned_changes: vec![PlannedChange {
                change_id: "change-1-theme".into(),
                parent_change_id: None,
                path: "src/theme.css".into(),
                targets: vec![],
                change: "Update tokens".into(),
                reason: "Central propagation".into(),
                status: IntendedChangeStatus::Planned,
                acceptance_criteria: vec!["All surfaces use the theme".into()],
                test_coverage: vec!["theme snapshot".into()],
            }],
            planning_repair: None,
            completed_changes: vec![],
            failed_changes: vec![],
            tool_progress: vec![],
            intended_changes: vec![],
            write_attempts: vec![],
            write_preflight_rejections: vec![],
            remaining_work: vec!["Update tokens".into()],
            remaining_work_v2: vec![],
            blocking_unknowns: vec![],
            validation_failures: vec![],
            validation_evidence: vec![],
            required_gates: vec![],
            phase_budget: json!({}),
            last_successful_action: json!({"tool": "read_file"}),
        };
        let (impact_map, plan, phase) = notebook_orchestration_state(&notebook);
        assert!(impact_map.is_some());
        assert!(plan.is_some());
        assert_eq!(phase, ExecutionPhase::Implementation);
    }

    #[test]
    fn unrecovered_source_edit_failure_blocks_completion() {
        let implementation = ImplementationOutcome {
            summary: "claimed complete".into(),
            budget_exhausted: false,
            explicit_declaration: Some(ImplementationDeclaration {
                implementation_status: "complete".into(),
                completed_work: vec!["theme".into()],
                remaining_work: vec![],
                known_risks: vec![],
                changed_paths: vec!["src/theme.css".into()],
                criteria_evidence: vec![],
            }),
        };
        let failures = vec![ToolFailureRecord {
            attempt_index: 0,
            tool: "replace_text".into(),
            target: Some("src/theme.css".into()),
            error: "found zero matches".into(),
            recovered: false,
            change_id: Some("change-1-theme".into()),
            error_code: "replace_match_not_unique".into(),
            match_count: Some(0),
            reconciliation: FailureReconciliation::StillUnresolved,
            recovery: None,
            intended_change_sha256: Some("a".repeat(64)),
        }];
        let result = completion_fallback(
            &implementation,
            Some(&test_impact_map()),
            None,
            &failures,
            &["src/theme.css".into()],
            &["Theme can be selected".into()],
            &[],
            ProjectVerificationPolicy::default(),
        );
        assert_eq!(result.status, CompletionStatus::Incomplete);
        assert_eq!(result.unrecovered_tool_failures.len(), 1);
    }

    #[test]
    fn complete_evaluation_requires_concrete_evidence_for_every_applicable_criterion() {
        let implementation = ImplementationOutcome {
            summary: "complete".into(),
            budget_exhausted: false,
            explicit_declaration: Some(ImplementationDeclaration {
                implementation_status: "complete".into(),
                completed_work: vec!["theme".into()],
                remaining_work: vec![],
                known_risks: vec![],
                changed_paths: vec!["src/theme.css".into()],
                criteria_evidence: vec![ImplementationCriterionEvidence {
                    criterion: "Theme can be selected".into(),
                    paths: vec!["src/theme.css".into()],
                    evidence: "The diff adds the theme token set.".into(),
                }],
            }),
        };
        let evaluation = CompletionEvaluation {
            status: CompletionStatus::Complete,
            implementation_completeness: ImplementationCompleteness::Complete,
            verification_readiness: VerificationReadiness::AutomatedVerified,
            evaluation_source: EvaluationSource::Model,
            confidence: 0.95,
            criteria: vec![CriterionEvaluation {
                criterion_id: "ac-1".into(),
                criterion: "Theme can be selected".into(),
                verification_type: VerificationType::Code,
                status: CriterionStatus::Satisfied,
                evidence: vec![CompletionEvidence {
                    path: "src/theme.css".into(),
                    description: "Adds the complete theme token set.".into(),
                }],
                validation_evidence: vec!["cargo test".into()],
                missing_evidence: vec![],
                required_next_action: None,
            }],
            remaining_implementation_work: vec![],
            remaining_automated_verification: vec![],
            pending_external_review: vec![],
            optional_follow_up: vec![],
            review_checklist: vec![],
            unrecovered_tool_failures: vec![],
            summary: "All criteria have diff evidence.".into(),
        };
        let criteria = vec!["Theme can be selected".into()];
        assert!(
            validate_completion_evaluation(
                evaluation.clone(),
                &implementation,
                &[],
                &["src/theme.css".into()],
                &criteria,
            )
            .is_ok()
        );
        let hosted_result = HostedResult {
            summary: "complete".into(),
            branch: "rustgrid/complete".into(),
            commit: "b".repeat(40),
            pull_request: PullRequestResult {
                number: 2,
                url: "https://github.com/RustGrid/example/pull/2".into(),
            },
            validation: vec![ValidationResult {
                id: "test".into(),
                command: "cargo test".into(),
                status: "passed".into(),
                output: String::new(),
            }],
            completeness: evaluation.clone(),
            terminal_telemetry: TerminalTelemetry::default(),
        };
        assert!(hosted_result_can_succeed(&hosted_result));
        let mut missing_evidence = evaluation;
        missing_evidence.criteria[0].evidence.clear();
        assert!(
            validate_completion_evaluation(
                missing_evidence,
                &implementation,
                &[],
                &["src/theme.css".into()],
                &criteria,
            )
            .is_err()
        );

        let missing_criterion = CompletionEvaluation {
            status: CompletionStatus::Complete,
            implementation_completeness: ImplementationCompleteness::Complete,
            verification_readiness: VerificationReadiness::AutomatedVerified,
            evaluation_source: EvaluationSource::Model,
            confidence: 0.9,
            criteria: vec![CriterionEvaluation {
                criterion_id: "ac-1".into(),
                criterion: "A different criterion".into(),
                verification_type: VerificationType::Code,
                status: CriterionStatus::Satisfied,
                evidence: vec![CompletionEvidence {
                    path: "src/theme.css".into(),
                    description: "Changed theme code.".into(),
                }],
                validation_evidence: vec![],
                missing_evidence: vec![],
                required_next_action: None,
            }],
            remaining_implementation_work: vec![],
            remaining_automated_verification: vec![],
            pending_external_review: vec![],
            optional_follow_up: vec![],
            review_checklist: vec![],
            unrecovered_tool_failures: vec![],
            summary: "Incomplete mapping.".into(),
        };
        assert!(
            validate_completion_evaluation(
                missing_criterion,
                &implementation,
                &[],
                &["src/theme.css".into()],
                &criteria,
            )
            .is_err()
        );
    }

    #[test]
    fn partial_pull_request_is_prominently_marked_incomplete_and_resumable() {
        let manifest = test_manifest(Uuid::from_u128(0x11111111_1111_4111_8111_111111111111));
        let completeness = CompletionEvaluation {
            status: CompletionStatus::Partial,
            implementation_completeness: ImplementationCompleteness::Partial,
            verification_readiness: VerificationReadiness::Blocked,
            evaluation_source: EvaluationSource::OrchestratorFallback,
            confidence: 1.0,
            criteria: vec![],
            remaining_implementation_work: vec!["Add settings integration".into()],
            remaining_automated_verification: vec![],
            pending_external_review: vec![],
            optional_follow_up: vec![],
            review_checklist: vec![],
            unrecovered_tool_failures: vec![],
            summary: "Budget exhausted after one theme-provider edit.".into(),
        };
        let body = hosted_pull_request_body(&manifest, &[], &completeness);
        let title = hosted_pull_request_title(&manifest, true);
        assert!(body.contains("INCOMPLETE"));
        assert!(body.contains("Add settings integration"));
        assert!(body.contains("partial"));
        assert!(body.contains("### Completed"));
        assert!(body.contains("### Not completed"));
        assert!(body.contains("### Root cause"));
        assert!(body.contains("### Resume action"));
        assert!(body.contains("without repeating discovery, planning, or completed work"));
        assert!(title.starts_with("[INCOMPLETE]"));
    }

    #[test]
    fn deterministic_fallback_classifies_external_review_without_missing_code() {
        let criteria = vec![
            "The designated product owner approves the light-blue palette.".into(),
            "Complete manual accessibility contrast and keyboard focus review.".into(),
        ];
        let implementation = ImplementationOutcome {
            summary: "implementation complete".into(),
            budget_exhausted: false,
            explicit_declaration: Some(ImplementationDeclaration {
                implementation_status: "complete".into(),
                completed_work: vec!["theme implementation".into()],
                remaining_work: vec![],
                known_risks: vec![],
                changed_paths: vec!["src/theme.css".into()],
                criteria_evidence: vec![],
            }),
        };
        let result = completion_fallback(
            &implementation,
            None,
            None,
            &[],
            &["src/theme.css".into()],
            &criteria,
            &[ValidationResult {
                id: "test".into(),
                command: "npm test".into(),
                status: "passed".into(),
                output: String::new(),
            }],
            ProjectVerificationPolicy::default(),
        );

        assert_eq!(result.criteria.len(), criteria.len());
        assert_eq!(
            result.criteria[0].verification_type,
            VerificationType::ProductApproval
        );
        assert_eq!(
            result.criteria[1].verification_type,
            VerificationType::AccessibilityReview
        );
        assert!(
            result
                .criteria
                .iter()
                .all(|criterion| criterion.status == CriterionStatus::ExternalReviewRequired)
        );
        assert_eq!(
            result.implementation_completeness,
            ImplementationCompleteness::Complete
        );
        assert_eq!(
            result.verification_readiness,
            VerificationReadiness::PendingManualReview
        );
        assert_eq!(
            result.status,
            CompletionStatus::CompletePendingExternalReview
        );
        assert!(result.remaining_implementation_work.is_empty());
        assert_eq!(result.review_checklist.len(), 2);
    }

    #[test]
    fn browser_e2e_policy_controls_implementation_completeness() {
        let criterion = "Theme persists through browser navigation and page reload.".to_string();
        let implementation = ImplementationOutcome {
            summary: "theme implementation complete".into(),
            budget_exhausted: false,
            explicit_declaration: Some(ImplementationDeclaration {
                implementation_status: "complete".into(),
                completed_work: vec!["theme persistence".into()],
                remaining_work: vec![],
                known_risks: vec![],
                changed_paths: vec!["src/theme.tsx".into()],
                criteria_evidence: vec![ImplementationCriterionEvidence {
                    criterion: criterion.clone(),
                    paths: vec!["src/theme.tsx".into()],
                    evidence: "The provider persists and restores the selected theme.".into(),
                }],
            }),
        };
        let changed_paths = vec!["src/theme.tsx".into()];
        let criteria = vec![criterion];
        let validation = vec![ValidationResult {
            id: "test".into(),
            command: "npm test".into(),
            status: "passed".into(),
            output: String::new(),
        }];

        let optional = completion_fallback(
            &implementation,
            None,
            None,
            &[],
            &changed_paths,
            &criteria,
            &validation,
            ProjectVerificationPolicy {
                browser_e2e_required_for_theme_changes: false,
                manual_browser_verification_required: true,
            },
        );
        assert_eq!(
            optional.implementation_completeness,
            ImplementationCompleteness::Complete
        );
        assert_eq!(
            optional.status,
            CompletionStatus::CompletePendingExternalReview
        );

        let mandatory = completion_fallback(
            &implementation,
            None,
            None,
            &[],
            &changed_paths,
            &criteria,
            &validation,
            ProjectVerificationPolicy {
                browser_e2e_required_for_theme_changes: true,
                manual_browser_verification_required: false,
            },
        );
        assert_eq!(
            mandatory.criteria[0].verification_type,
            VerificationType::AutomatedTest
        );
        assert_eq!(mandatory.status, CompletionStatus::Partial);
        assert!(!mandatory.remaining_automated_verification.is_empty());
    }

    #[test]
    fn review_pending_pull_request_is_not_marked_implementation_incomplete() {
        let manifest = test_manifest(Uuid::from_u128(0x11111111_1111_4111_8111_111111111111));
        let implementation = ImplementationOutcome {
            summary: "complete".into(),
            budget_exhausted: false,
            explicit_declaration: Some(ImplementationDeclaration {
                implementation_status: "complete".into(),
                completed_work: vec!["palette".into()],
                remaining_work: vec![],
                known_risks: vec![],
                changed_paths: vec!["src/theme.css".into()],
                criteria_evidence: vec![],
            }),
        };
        let completeness = completion_fallback(
            &implementation,
            None,
            None,
            &[],
            &["src/theme.css".into()],
            &["Product owner approves the palette.".into()],
            &[],
            ProjectVerificationPolicy::default(),
        );
        let body = hosted_pull_request_body(&manifest, &[], &completeness);
        let title = hosted_pull_request_title(&manifest, false);
        assert!(body.contains("IMPLEMENTATION COMPLETE"));
        assert!(body.contains("External review checklist"));
        assert!(!body.contains("INCOMPLETE — continue implementation"));
        assert!(!title.starts_with("[INCOMPLETE]"));
        assert!(!requires_implementation_continuation(completeness.status));
    }

    #[test]
    fn cache_observability_explains_zero_reads_without_metadata_churn() {
        let first_request = json!({
            "model": "gpt-5.6",
            "instructions": "stable",
            "tools": [{"type": "function", "name": "read_files"}],
            "metadata": {"phase": "discovery"}
        });
        let second_request = json!({
            "model": "gpt-5.6",
            "instructions": "stable",
            "tools": [{"type": "function", "name": "read_files"}],
            "metadata": {"phase": "implementation"}
        });
        let response = json!({
            "usage": {"input_tokens_details": {"cached_tokens": 0}}
        });
        let (cold, prefix, tools) =
            cache_observability_payload(&first_request, &response, None, None);
        assert_eq!(cold["cache_invalidation_reason"], "cold_start");
        assert_eq!(cold["cache_read"], false);
        assert_eq!(cold["model_cache_support_reported"], true);
        assert_eq!(cold["gateway_forwarded_cache_fields"], false);

        let (stable, second_prefix, second_tools) =
            cache_observability_payload(&second_request, &response, Some(&prefix), Some(&tools));
        assert_eq!(prefix, second_prefix);
        assert_eq!(tools, second_tools);
        assert_eq!(
            stable["cache_invalidation_reason"],
            "provider_reported_zero_cache_read"
        );
        assert_eq!(stable["metadata_excluded_from_stable_prefix"], true);
    }

    #[test]
    fn valid_impact_map_is_recovered_from_tool_arguments_and_notebook_progress() {
        let notebook = test_discovery_notebook(ExecutionPhase::Discovery);
        let mut map = test_impact_map();
        map.inspected_files.clear();
        map.searches.clear();
        let arguments = serde_json::to_string(&map).unwrap();

        let (recovered, _) = recover_impact_map(Some(&arguments), None, &notebook).unwrap();
        assert_eq!(recovered.inspected_files, notebook.files_inspected);
        assert_eq!(
            recovered
                .searches
                .iter()
                .map(|s| &s.query)
                .collect::<Vec<_>>(),
            notebook.searches_completed.iter().collect::<Vec<_>>()
        );
        assert_eq!(recovered.areas, map.areas);
    }

    #[test]
    fn valid_impact_map_is_recovered_from_a_fenced_assistant_response() {
        let notebook = test_discovery_notebook(ExecutionPhase::Discovery);
        let response = format!(
            "```json\n{}\n```",
            serde_json::to_string(&test_impact_map()).unwrap()
        );

        let (recovered, _) = recover_impact_map(None, Some(&response), &notebook).unwrap();
        assert_eq!(recovered.areas, test_impact_map().areas);
    }

    #[test]
    fn impact_map_fallback_rejects_unknown_or_invented_fields() {
        let notebook = test_discovery_notebook(ExecutionPhase::Discovery);
        let mut value = serde_json::to_value(test_impact_map()).unwrap();
        value["untrusted_extra"] = json!("do not accept");
        let arguments = serde_json::to_string(&value).unwrap();
        assert!(recover_impact_map(Some(&arguments), None, &notebook).is_err());
    }

    #[test]
    fn semantic_impact_map_survives_failed_persistence_and_resumes_planning() {
        let mut notebook = test_discovery_notebook(ExecutionPhase::Planning);
        let map = test_impact_map();
        notebook.impact_map = map.areas.clone();
        notebook.impact_map_artifact = ArtifactCheckpoint {
            artifact: "impact_map".into(),
            semantic_status: ArtifactSemanticStatus::Sufficient,
            serialization_status: ArtifactSerializationStatus::Valid,
            persistence_status: ArtifactPersistenceStatus::Failed,
            artifact_sha256: impact_map_sha256(&map),
            model_call_index: Some(8),
            phase: ExecutionPhase::Discovery,
            safe_error: Some("worker event transport failed".into()),
            artifact_source: Some(ArtifactSource::Model),
            confidence: Some(1.0),
            failure_layer: Some(ArtifactFailureLayer::ArtifactPersistence),
            validation_errors: Vec::new(),
            invalid_payload_shape: None,
        };

        let (restored, plan, phase) = notebook_orchestration_state(&notebook);
        assert!(restored.is_some());
        assert!(plan.is_none());
        assert_eq!(phase, ExecutionPhase::Planning);
        assert_eq!(
            notebook.impact_map_artifact.semantic_status,
            ArtifactSemanticStatus::Sufficient
        );
        assert_eq!(
            notebook.impact_map_artifact.persistence_status,
            ArtifactPersistenceStatus::Failed
        );
    }

    #[test]
    fn invalid_impact_map_resume_preserves_discovery_and_targets_artifact_repair() {
        let notebook = test_discovery_notebook(ExecutionPhase::ArtifactRepair);
        let (map, plan, phase) = notebook_orchestration_state(&notebook);
        assert!(map.is_none());
        assert!(plan.is_none());
        assert_eq!(phase, ExecutionPhase::ArtifactRepair);
        assert_eq!(
            notebook.files_inspected,
            vec!["src/components/theme/ThemeProvider.tsx"]
        );
        assert_eq!(hosted_tools_for_phase(phase).len(), 1);
        assert_eq!(
            hosted_tools_for_phase(phase)[0]["name"],
            "record_impact_map"
        );
    }

    #[test]
    fn artifact_repair_context_contains_exact_corrections_without_discovery_transcript() {
        let notebook = test_discovery_notebook(ExecutionPhase::ArtifactRepair);
        let invalid = json!({"areas":[{"name":"Theme","candidate_paths":[]}]});
        let failure = ImpactMapFailure {
            code: "impact_map_schema_mismatch",
            safe_error: "invalid".into(),
            errors: vec![ValidationError {
                path: "$.areas[0].candidate_paths".into(),
                keyword: "minItems".into(),
                message: "At least one candidate path is required.".into(),
            }],
            invalid_payload: invalid.clone(),
            invalid_payload_shape: impact_map::safe_shape(&invalid),
            failure_layer: ArtifactFailureLayer::WorkerToolSchemaValidation,
        };
        let context = compact_impact_map_repair_context(Some(&failure), &notebook);
        assert!(context.contains("$.areas[0].candidate_paths"));
        assert!(context.contains("evidence_id"));
        assert!(context.contains("ac-1"));
        assert!(!context.contains("Theme tokens are centralized"));
    }

    #[test]
    fn artifact_repair_context_remains_below_five_thousand_tokens() {
        let context = compact_impact_map_repair_context(
            None,
            &test_discovery_notebook(ExecutionPhase::ArtifactRepair),
        );
        assert!(context.len().div_ceil(4) < 5_000);
    }

    #[test]
    fn supplemental_repair_accounting_is_separate_from_mission_budget() {
        let accounting = artifact_call_accounting(ExecutionPhase::ArtifactRepair);
        assert_eq!(accounting["provider_call_occurred"], true);
        assert_eq!(accounting["configured_mission_budget_consumed"], false);
        assert_eq!(accounting["supplemental_repair_budget_consumed"], true);
    }

    #[test]
    fn formatting_failure_is_healthy_blocked_and_resumable() {
        let failure = HostedAgentExecutionFailure {
            status: "blocked",
            category: "hosted_agent_execution_failed",
            process_health: "healthy",
            mission_outcome: "blocked",
            blocker: Some("impact_map_artifact_invalid".into()),
            resumable: true,
            code: "impact_map_schema_mismatch".into(),
            phase: ExecutionPhase::ArtifactRepair,
            message: "repair".into(),
            underlying_error: UnderlyingFailure {
                r#type: "orchestration_guardrail".into(),
                message: "schema".into(),
                stack_reference: None,
            },
            model_calls_used: 6,
            model_calls_limit: 10,
            model_calls_remaining: 4,
            phase_calls_used: 1,
            phase_calls_limit: 1,
            last_successful_action: json!({}),
            usage: ToolUsage::default(),
            estimated_cost_micros: 0,
            input_tokens: 0,
            output_tokens: 0,
            changed_paths: vec![],
            remaining_work: vec![],
            failed_tool_operations: vec![],
            current_plan: vec![],
            validation_evidence: vec![],
            notebook_revision: 0,
            recoverable: true,
            resume_phase: "artifact_repair".into(),
            recommended_action: "resume".into(),
            artifact: Some("impact_map".into()),
            semantic_status: Some(ArtifactSemanticStatus::Invalid),
            persistence_status: Some(ArtifactPersistenceStatus::PendingRetry),
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
        };
        let value = serde_json::to_value(failure).unwrap();
        assert_eq!(value["process_health"], "healthy");
        assert_eq!(value["mission_outcome"], "blocked");
        assert_eq!(value["resumable"], true);
    }

    #[test]
    fn resume_revision_eight_reuses_discovery_without_discovery_tools() {
        let mut notebook = test_discovery_notebook(ExecutionPhase::ArtifactRepair);
        notebook.revision = 8;
        let (_, _, phase) = notebook_orchestration_state(&notebook);
        assert_eq!(notebook.revision, 8);
        assert_eq!(phase, ExecutionPhase::ArtifactRepair);
        let names = hosted_tools_for_phase(phase)
            .into_iter()
            .filter_map(|v| v["name"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["record_impact_map"]);
        assert!(!names.contains(&"read_file".into()));
    }

    fn test_discovery_notebook(phase: ExecutionPhase) -> WorkerNotebook {
        WorkerNotebook {
            schema_version: 1,
            revision: 4,
            goal: "Apply a complete theme".into(),
            acceptance_criteria: vec!["All surfaces use the theme".into()],
            acceptance_criteria_v2: vec![impact_map::AcceptanceCriterion {
                id: "ac-1".into(),
                text: "All surfaces use the theme".into(),
            }],
            phase,
            implementation_substate: ImplementationSubstate::Preparing,
            repository_base_sha: "a".repeat(40),
            branch: "rustgrid/aops-226-deadbeef".into(),
            repository_fingerprint: "b".repeat(64),
            execution_attempt: 1,
            architecture_findings: vec!["Theme tokens are centralized.".into()],
            impact_map: vec![],
            impact_map_v2: None,
            impact_map_artifact: ArtifactCheckpoint {
                semantic_status: ArtifactSemanticStatus::Invalid,
                persistence_status: ArtifactPersistenceStatus::PendingRetry,
                ..ArtifactCheckpoint::default()
            },
            impact_map_invalid_payload: None,
            impact_evidence: impact_map::evidence_catalog(
                &["src/components/theme/ThemeProvider.tsx".into()],
                &["literal:src:ThemeProvider".into()],
            ),
            files_inspected: vec!["src/components/theme/ThemeProvider.tsx".into()],
            read_ranges_inspected: vec!["src/components/theme/ThemeProvider.tsx:1-400".into()],
            searches_completed: vec!["literal:src:ThemeProvider".into()],
            discovery_paths_sampled: vec![],
            planned_changes: vec![],
            planning_repair: None,
            completed_changes: vec![],
            failed_changes: vec![],
            tool_progress: vec![],
            intended_changes: vec![],
            write_attempts: vec![],
            write_preflight_rejections: vec![],
            remaining_work: vec![],
            remaining_work_v2: vec![],
            blocking_unknowns: vec![],
            validation_failures: vec![],
            validation_evidence: vec![],
            required_gates: vec![],
            phase_budget: json!({}),
            last_successful_action: json!({"tool": "read_files"}),
        }
    }

    fn test_planned_change() -> PlannedChange {
        PlannedChange {
            change_id: "theme-tests".into(),
            parent_change_id: None,
            path: String::new(),
            targets: vec![PlannedTarget {
                path: "tests/theme-provider.test.tsx".into(),
                role: "focused theme coverage".into(),
                new_file: false,
                status: IntendedChangeStatus::Planned,
            }],
            change: "Add light-blue theme coverage.".into(),
            reason: "Verify registration, persistence, cycling, and fallback behavior.".into(),
            status: IntendedChangeStatus::Planned,
            acceptance_criteria: vec!["Theme can be selected".into()],
            test_coverage: vec!["npm test".into()],
        }
    }

    fn test_write_failure(
        change_id: &str,
        target: &str,
        intended_change_sha256: &str,
    ) -> ToolFailureRecord {
        ToolFailureRecord {
            attempt_index: 0,
            change_id: Some(change_id.into()),
            tool: "replace_text".into(),
            target: Some(target.into()),
            error_code: "replace_match_not_unique".into(),
            match_count: Some(2),
            error: "replace_match_not_unique: found 2 matches".into(),
            recovered: false,
            reconciliation: FailureReconciliation::StillUnresolved,
            recovery: None,
            intended_change_sha256: Some(intended_change_sha256.into()),
        }
    }

    fn test_complete_implementation() -> ImplementationOutcome {
        ImplementationOutcome {
            summary: "Implemented and validated the theme.".into(),
            budget_exhausted: false,
            explicit_declaration: Some(ImplementationDeclaration {
                implementation_status: "complete".into(),
                completed_work: vec!["Added light-blue theme coverage.".into()],
                remaining_work: vec![],
                known_risks: vec![],
                changed_paths: vec!["tests/theme-provider.test.tsx".into()],
                criteria_evidence: vec![ImplementationCriterionEvidence {
                    criterion: "Theme can be selected".into(),
                    paths: vec!["tests/theme-provider.test.tsx".into()],
                    evidence: "Registration and persistence assertions are present.".into(),
                }],
            }),
        }
    }

    fn test_passed_validation(command: &str) -> ValidationResult {
        ValidationResult {
            id: command.replace(' ', "-"),
            command: command.into(),
            status: "passed".into(),
            output: String::new(),
        }
    }

    fn test_completion_evaluation(status: CompletionStatus) -> CompletionEvaluation {
        CompletionEvaluation {
            status,
            implementation_completeness: match status {
                CompletionStatus::Complete | CompletionStatus::CompletePendingExternalReview => {
                    ImplementationCompleteness::Complete
                }
                CompletionStatus::Partial => ImplementationCompleteness::Partial,
                CompletionStatus::Blocked
                | CompletionStatus::Incomplete
                | CompletionStatus::Uncertain => ImplementationCompleteness::Incomplete,
            },
            verification_readiness: VerificationReadiness::Blocked,
            evaluation_source: EvaluationSource::OrchestratorFallback,
            confidence: 1.0,
            criteria: vec![],
            remaining_implementation_work: vec![],
            remaining_automated_verification: vec![],
            pending_external_review: vec![],
            optional_follow_up: vec![],
            review_checklist: vec![],
            unrecovered_tool_failures: vec![],
            summary: status.as_str().into(),
        }
    }

    fn test_impact_map() -> ImpactMap {
        ImpactMap {
            schema_version: IMPACT_MAP_SCHEMA_VERSION.into(),
            areas: vec![ImpactArea {
                area_id: "area-theme".into(),
                name: "theme".into(),
                candidate_paths: vec!["src/theme.css".into()],
                evidence: vec![impact_map::ImpactEvidence {
                    evidence_type: impact_map::EvidenceType::FileRead,
                    path: Some("src/theme.css".into()),
                    query: None,
                    description: "inspected".into(),
                }],
                reason: "The token source propagates to every themed surface.".into(),
                acceptance_criteria_ids: vec!["ac-1".into()],
            }],
            inspected_files: vec!["src/theme.css".into()],
            searches: vec![impact_map::ImpactSearch {
                query: "theme".into(),
                scope: None,
            }],
            unresolved_questions: vec![],
        }
    }

    #[test]
    fn hosted_tools_have_only_the_gateway_allowed_function_shape() {
        let tools = hosted_tools();
        validate_provider_tool_definitions(&json!(&tools)).unwrap();
        for tool in tools {
            let object = tool.as_object().unwrap();
            assert_eq!(object.get("type"), Some(&json!("function")));
            assert!(object.get("name").is_some_and(Value::is_string));
            assert_eq!(object.get("strict"), Some(&json!(true)));
            let parameters = object.get("parameters").and_then(Value::as_object).unwrap();
            assert_eq!(parameters.get("additionalProperties"), Some(&json!(false)));
            let properties = parameters
                .get("properties")
                .and_then(Value::as_object)
                .unwrap()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>();
            let required = parameters
                .get("required")
                .and_then(Value::as_array)
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().to_owned())
                .collect::<BTreeSet<_>>();
            assert_eq!(properties, required);
            assert!(object.len() <= 5);
        }
    }

    #[test]
    fn provider_tool_preflight_rejects_duplicate_and_invalid_strict_schemas() {
        let valid = json!({
            "type": "function",
            "name": "read_file",
            "description": "Read one file.",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            },
            "strict": true
        });
        let duplicate = json!([valid.clone(), valid]);
        assert!(
            validate_provider_tool_definitions(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("duplicate tool name")
        );

        let invalid = json!([{
            "type": "function",
            "name": "write_file",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            },
            "strict": true
        }]);
        let error = validate_provider_tool_definitions(&invalid).unwrap_err();
        assert!(error.to_string().contains("additionalProperties"));
        assert!(error.to_string().contains("tools[0].parameters"));
    }

    #[test]
    fn provider_schema_preflight_rejects_unsupported_keywords_and_excess_depth() {
        let unsupported = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "pattern": "^src/"}
            },
            "required": ["path"],
            "additionalProperties": false
        });
        assert!(
            validate_provider_json_schema(&unsupported, "schema", 0, true, true)
                .unwrap_err()
                .to_string()
                .contains("schema.properties.path.pattern")
        );

        let mut nested = json!({"type": "string"});
        for _ in 0..10 {
            nested = json!({"type": "array", "items": nested});
        }
        assert!(
            validate_provider_json_schema(&nested, "schema", 0, false, false)
                .unwrap_err()
                .to_string()
                .contains("nesting depth")
        );
    }

    #[test]
    fn provider_schema_preflight_rejects_type_mismatches_and_missing_array_items() {
        for (schema, expected_path) in [
            (
                json!({"type": "string", "enum": ["safe", 7]}),
                "schema.enum",
            ),
            (json!({"type": "string", "minimum": 1}), "schema"),
            (json!({"type": "array"}), "schema.items"),
        ] {
            let error =
                validate_provider_json_schema(&schema, "schema", 0, false, false).unwrap_err();
            assert!(
                error.to_string().contains(expected_path),
                "unexpected schema error: {error}"
            );
        }
    }

    #[test]
    fn phase_tool_admission_protects_implementation_and_completion_reserves() {
        assert!(phase_permits_tool(
            ExecutionPhase::Discovery,
            "record_impact_map"
        ));
        assert!(!phase_permits_tool(ExecutionPhase::Discovery, "write_file"));
        assert!(phase_permits_tool(
            ExecutionPhase::Planning,
            "record_implementation_plan"
        ));
        assert!(!phase_permits_tool(
            ExecutionPhase::Planning,
            "replace_text"
        ));
        assert!(phase_permits_tool(
            ExecutionPhase::Implementation,
            "replace_text"
        ));
        assert!(phase_permits_tool(ExecutionPhase::Repair, "read_file"));
        assert!(phase_permits_tool(
            ExecutionPhase::DiffReview,
            "declare_implementation"
        ));
        assert!(!phase_permits_tool(
            ExecutionPhase::CompletionEvaluation,
            "write_file"
        ));
        assert!(!phase_permits_tool(
            ExecutionPhase::Validation,
            "search_text"
        ));
        assert!(!phase_permits_tool(
            ExecutionPhase::Publication,
            "run_focused_command"
        ));
    }

    #[test]
    fn hosted_dependency_bootstrap_is_locked_and_ignores_lifecycle_scripts() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("package.json"), "{}").unwrap();
        fs::write(directory.path().join("package-lock.json"), "{}").unwrap();
        assert_eq!(
            hosted_dependency_bootstrap(directory.path()),
            Some((
                "npm",
                "npm ci --ignore-scripts --no-audit --no-fund --prefer-offline"
            ))
        );
    }

    #[test]
    fn safe_failures_never_include_raw_remote_error_bodies() {
        let error = anyhow::Error::new(HostedHttpError {
            status: StatusCode::BAD_GATEWAY,
            path: "executions/id/ai/responses".into(),
            code: "ai_provider_unavailable".into(),
            request_id: Some("request-1".into()),
            rustgrid_gateway_status: None,
            upstream_provider_status: None,
            failure_stage: None,
            provider_contacted: None,
            call_budget_consumed: None,
            reservation_state: None,
            reservation_reconciliation_state: None,
            retryable: None,
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
        });
        let (code, message) = safe_failure(&error, false);
        assert_eq!(code, "ai_provider_unavailable");
        assert_eq!(
            message,
            "The upstream model provider failed while processing the request."
        );
        assert!(!message.contains("responses"));
    }

    #[test]
    fn structured_failures_preserve_phase_usage_and_actionable_cause() {
        let mut failure = test_execution_failure(
            "search_loop_detected",
            "Repeated discovery search was rejected.",
        );
        failure.underlying_error = UnderlyingFailure {
            r#type: "orchestration_guardrail".into(),
            message: "duplicate_search_rejected".into(),
            stack_reference: Some("request-2".into()),
        };
        failure.model_calls_used = 7;
        failure.model_calls_remaining = 33;
        failure.phase_calls_used = 7;
        failure.last_successful_action = json!({"tool": "read_files"});
        failure.usage = ToolUsage {
            reads: 6,
            searches: 4,
            ..ToolUsage::default()
        };
        failure.recommended_action = "Record the impact map.".into();
        let error = anyhow::Error::new(failure);
        let (terminal_code, terminal_message) = safe_failure(&error, false);
        assert_eq!(terminal_code, "search_loop_detected");
        assert_eq!(terminal_message, "Repeated discovery search was rejected.");
        let diagnostics = failure_diagnostics(&error, false);
        assert_eq!(diagnostics["code"], "search_loop_detected");
        assert_eq!(diagnostics["phase"], "discovery");
        assert_eq!(diagnostics["model_calls_used"], 7);
        assert_eq!(diagnostics["model_calls_limit"], 40);
        assert_eq!(diagnostics["usage"]["searches"], 4);
        assert_eq!(
            diagnostics["underlying_error"]["message"],
            "duplicate_search_rejected"
        );
    }

    #[test]
    fn github_oidc_request_uses_audience_and_bearer_without_logging_the_jwt() {
        let jwt = format!("{}.{}.{}", "a".repeat(30), "b".repeat(30), "c".repeat(30));
        let Some((base, request, server)) = one_request_server("200 OK", json!({"value": jwt}))
        else {
            return;
        };
        let execution_id = Uuid::from_u128(40);
        let environment = GithubActionsEnvironment {
            api_root: base.join("api/v1/").unwrap(),
            audience: base.origin().ascii_serialization(),
            oidc_request_url: base.join("oidc?existing=1").unwrap(),
            oidc_request_token: SecretString::new("oidc-request-bearer".into(), "test").unwrap(),
            dispatch_nonce: SecretString::new("d".repeat(48), "test").unwrap(),
            repository: None,
            repository_id: None,
            sha: None,
            workflow_run_id: None,
            workflow_run_attempt: None,
            actor: None,
            actor_id: None,
        };
        let token = request_github_oidc(&hosted_http_client().unwrap(), &environment).unwrap();
        server.join().unwrap();
        assert_eq!(token.expose(), jwt);
        let request = request.recv().unwrap();
        assert!(request.starts_with("GET /oidc?existing=1&audience="));
        assert!(request.contains("authorization: Bearer oidc-request-bearer"));
        assert!(!format!("{token:?}").contains(&jwt));
        let _ = execution_id;
    }

    #[test]
    fn oidc_exchange_posts_only_the_scoped_identity_contract() {
        let execution_id = Uuid::from_u128(41);
        let response = exchange_response(execution_id);
        let body = json!({
            "access_token": response.access_token,
            "token_type": response.token_type,
            "expires_in": response.expires_in,
            "expires_at": response.expires_at,
            "token_id": response.token_id,
            "tenant_id": response.tenant_id,
            "project_id": response.project_id,
            "execution_id": response.execution_id,
            "execution_attempt": response.execution_attempt,
            "session_id": response.session_id,
            "worker_id": response.worker_id,
            "repository_id": response.repository_id,
            "github_workflow_run_id": response.github_workflow_run_id,
            "permissions": response.permissions
        });
        let Some((base, request, server)) = one_request_server("200 OK", body) else {
            return;
        };
        let environment = GithubActionsEnvironment {
            api_root: base.join("api/v1/").unwrap(),
            audience: base.origin().ascii_serialization(),
            oidc_request_url: base.join("oidc").unwrap(),
            oidc_request_token: SecretString::new("request-bearer".into(), "test").unwrap(),
            dispatch_nonce: SecretString::new("n".repeat(48), "test").unwrap(),
            repository: None,
            repository_id: None,
            sha: None,
            workflow_run_id: None,
            workflow_run_attempt: None,
            actor: None,
            actor_id: None,
        };
        let jwt = SecretString::new(
            format!("{}.{}.{}", "a".repeat(30), "b".repeat(30), "c".repeat(30)),
            "test",
        )
        .unwrap();
        let exchanged = exchange_github_oidc(
            &hosted_http_client().unwrap(),
            &environment,
            execution_id,
            &jwt,
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(exchanged.execution_id, execution_id);
        let request = request.recv().unwrap();
        assert!(
            request.starts_with("POST /api/v1/execution-auth/github-actions/exchange HTTP/1.1")
        );
        assert!(request.contains(&format!("\"execution_id\":\"{execution_id}\"")));
        assert!(request.contains("\"dispatch_nonce\":\"nnnn"));
        assert!(request.contains("\"github_oidc_token\":\"aaaa"));
        assert!(!request.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn execution_token_refresh_rotates_the_in_memory_bearer() {
        let execution_id = Uuid::from_u128(42);
        let refreshed = format!("rge_{}", "b".repeat(48));
        let Some((base, request, server)) = one_request_server(
            "200 OK",
            json!({
                "access_token": refreshed,
                "token_type": "Bearer",
                "expires_at": "2099-01-01T00:00:00Z",
                "token_id": Uuid::from_u128(43),
                "session_id": Uuid::from_u128(33)
            }),
        ) else {
            return;
        };
        let client = test_api_client(base, execution_id);
        {
            let mut state = client.auth.lock().unwrap();
            state.expires_at = SystemTime::now() + Duration::from_secs(1);
            state.refresh_after = SystemTime::now();
        }
        client.ensure_fresh().unwrap();
        server.join().unwrap();
        let request = request.recv().unwrap();
        assert!(request.starts_with(&format!(
            "POST /api/v1/executions/{execution_id}/token/refresh HTTP/1.1"
        )));
        assert!(request.contains(&format!("authorization: Bearer rge_{}", "a".repeat(48))));
        assert_eq!(client.current_token().unwrap().expose(), refreshed);
    }

    #[test]
    fn capped_token_refresh_is_not_repeated_on_every_worker_operation() {
        let execution_id = Uuid::from_u128(0x30303030_3030_4030_8030_303030303030);
        let capped_expiry = "2099-01-01T00:00:00Z";
        let Some((base, _, server)) = one_request_server(
            "200 OK",
            json!({
                "access_token": format!("rge_{}", "c".repeat(48)),
                "token_type": "Bearer",
                "expires_at": capped_expiry,
                "token_id": Uuid::from_u128(0x31),
                "session_id": Uuid::from_u128(33)
            }),
        ) else {
            return;
        };
        let client = test_api_client(base, execution_id);
        {
            let mut state = client.auth.lock().unwrap();
            state.expires_at = parse_rfc3339_utc(capped_expiry).unwrap();
            state.refresh_after = SystemTime::now();
        }
        client.ensure_fresh().unwrap();
        server.join().unwrap();
        let state = client.auth.lock().unwrap();
        assert_eq!(state.refresh_after, state.expires_at);
    }

    #[test]
    fn ai_registration_separates_semantic_calls_from_transport_attempts() {
        let execution_id = Uuid::from_u128(44);
        let session_id = Uuid::from_u128(45);
        let registration =
            ai_call_registration(execution_id, 9, session_id, 0, ExecutionPhase::Discovery, 0);

        assert_eq!(
            registration.semantic_call_id,
            ai_call_registration(
                execution_id,
                9,
                Uuid::from_u128(46),
                0,
                ExecutionPhase::Discovery,
                1
            )
            .semantic_call_id
        );
        assert_ne!(
            registration.semantic_call_id,
            ai_call_registration(
                execution_id,
                10,
                session_id,
                0,
                ExecutionPhase::Discovery,
                0
            )
            .semantic_call_id
        );
        assert_ne!(
            registration.semantic_call_id,
            ai_call_registration(execution_id, 9, session_id, 1, ExecutionPhase::Discovery, 0)
                .semantic_call_id
        );
        assert_ne!(
            registration.request_id,
            ai_call_registration(
                execution_id,
                9,
                Uuid::from_u128(46),
                0,
                ExecutionPhase::Discovery,
                0
            )
            .request_id
        );
        assert_ne!(
            registration.request_id,
            ai_call_registration(execution_id, 9, session_id, 0, ExecutionPhase::Discovery, 1)
                .request_id
        );
    }

    #[test]
    fn gateway_failure_contract_separates_registration_from_provider_status() {
        let execution_id = Uuid::from_u128(47);
        let Some((base, _request, server)) = one_request_server(
            "409 Conflict",
            json!({
                "code": "ai_call_index_conflict",
                "details": {
                    "failure_stage": "request_registration",
                    "provider_contacted": false,
                    "call_budget_consumed": false,
                    "reservation_reconciliation_state": "released",
                    "retryable": true
                }
            }),
        ) else {
            return;
        };
        let client = test_api_client(base, execution_id);
        let error = client
            .ai_response(
                json!({
                    "model": "gpt-5.6-sol",
                    "input": "bounded",
                    "max_output_tokens": 100,
                    "store": false,
                    "stream": false
                }),
                &ai_call_registration(
                    execution_id,
                    1,
                    Uuid::from_u128(49),
                    0,
                    ExecutionPhase::Discovery,
                    0,
                ),
            )
            .unwrap_err();
        server.join().unwrap();

        let failure = error.downcast_ref::<HostedHttpError>().unwrap();
        assert_eq!(failure.status, StatusCode::CONFLICT);
        assert_eq!(failure.effective_code(), "ai_call_index_conflict");
        assert_eq!(failure.failure_stage(), Some("request_registration"));
        assert_eq!(failure.upstream_provider_status, None);
        assert_eq!(failure.provider_contacted(), Some(false));
        assert_eq!(failure.call_budget_consumed(), Some(false));
        assert_eq!(failure.reservation_reconciliation_state(), Some("released"));
        assert!(failure.retryable_registration_failure());
    }

    #[test]
    fn provider_http_400_is_authoritative_and_is_not_retried_as_a_gateway_failure() {
        let execution_id = Uuid::from_u128(50);
        let provider_request_id = "b4dd40ed-d63b-4df9-81c1-3e886f7949d5";
        let rustgrid_request_id = "e24ad61e-ab87-485f-a2e1-6a6d9456ad0e";
        let transport_request_id = "ed798a57-5611-4d47-b060-69c79b34ac3c";
        let provider_message = "Invalid type for 'metadata.model_call_budget': expected a string, but got an integer instead.";
        let Some((base, _request, server)) = one_request_server(
            "502 Bad Gateway",
            json!({
                "code": "ai_provider_invalid_request",
                "details": {
                    "failure_stage": "provider_dispatch",
                    "provider_contacted": true,
                    "upstream_provider_status": 400,
                    "rustgrid_gateway_status": null,
                    "rustgrid_request_id": rustgrid_request_id,
                    "transport_request_id": transport_request_id,
                    "provider_request_id": provider_request_id,
                    "reservation_state": "reconciled",
                    "provider_error": {
                        "type": "invalid_request_error",
                        "code": "invalid_type",
                        "message": provider_message,
                        "parameter": "metadata.model_call_budget"
                    },
                    "provider_response_body": {
                        "error": {
                            "message": provider_message,
                            "parameter": "metadata.model_call_budget"
                        }
                    },
                    "model_alias": "gpt-5.6-sol",
                    "resolved_provider_model": "gpt-5.6-sol",
                    "adapter_version": "openai-responses-v1",
                    "payload_schema_version": "rustgrid.execution_ai.responses.v1",
                    "provider_attempts": 1,
                    "model_calls_used": 0,
                    "call_budget_consumed": false,
                    "actual_cost_micros": 0,
                    "recoverable": true
                }
            }),
        ) else {
            return;
        };
        let client = test_api_client(base, execution_id);
        let registration = ai_call_registration(
            execution_id,
            1,
            Uuid::from_u128(51),
            0,
            ExecutionPhase::Discovery,
            0,
        );
        let error = client
            .ai_response(
                json!({
                    "model": "gpt-5.6-sol",
                    "input": "bounded",
                    "metadata": {"model_call_budget": "40"},
                    "store": false,
                    "stream": false
                }),
                &registration,
            )
            .unwrap_err();
        server.join().unwrap();

        let failure = error.downcast_ref::<HostedHttpError>().unwrap();
        assert_eq!(failure.status, StatusCode::BAD_GATEWAY);
        assert_eq!(failure.failure_class(), AiFailureClass::ProviderValidation);
        assert_eq!(failure.effective_code(), "ai_provider_invalid_request");
        assert_eq!(failure.failure_stage(), Some("provider_dispatch"));
        assert_eq!(failure.rustgrid_gateway_status(), Some(None));
        assert_eq!(failure.upstream_provider_status, Some(400));
        assert_eq!(failure.provider_contacted(), Some(true));
        assert_eq!(failure.call_budget_consumed(), Some(false));
        assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Restore);
        assert!(!failure.retryable_gateway_transport_failure());
        assert!(!failure.retryable_registration_failure());
        assert_eq!(
            failure.terminal_message(),
            "The upstream model provider rejected the request as invalid."
        );
        assert_eq!(
            failure.provider_request_id.as_deref(),
            Some(provider_request_id)
        );
        assert_eq!(
            failure.rustgrid_request_id.as_deref(),
            Some(rustgrid_request_id)
        );
        assert_eq!(
            failure.transport_request_id.as_deref(),
            Some(transport_request_id)
        );
        assert_eq!(failure.reservation_state(), Some("reconciled"));
        let provider_error = failure.provider_error.as_ref().unwrap();
        assert_eq!(
            provider_error.error_type.as_deref(),
            Some("invalid_request_error")
        );
        assert_eq!(provider_error.code.as_deref(), Some("invalid_type"));
        assert_eq!(provider_error.message.as_deref(), Some(provider_message));
        assert_eq!(
            provider_error.parameter.as_deref(),
            Some("metadata.model_call_budget")
        );
        assert_eq!(
            failure.provider_response_body.as_ref().unwrap()["error"]["message"],
            provider_message
        );
        assert_eq!(failure.provider_attempts, Some(1));
        assert_eq!(failure.actual_cost_micros, Some(0));

        let event = provider_rejected_event(
            failure,
            &registration,
            1,
            1,
            "gpt-5.6-sol",
            0,
            json!({"model_calls_used": 0}),
            json!({"phase": "discovery"}),
        );
        assert_eq!(event["event_type"], "execution.ai.provider_rejected");
        assert_eq!(event["failure_stage"], "provider_dispatch");
        assert_eq!(event["rustgrid_gateway_status"], Value::Null);
        assert_eq!(event["upstream_provider_status"], 400);
        assert_eq!(event["provider_attempts"], 1);
        assert_eq!(event["rustgrid_request_id"], rustgrid_request_id);
        assert_eq!(event["transport_request_id"], transport_request_id);
        assert_eq!(event["reservation_state"], "reconciled");
        assert_eq!(event["model_calls_used"], 0);
        assert_eq!(event["call_budget_consumed"], false);
        assert_eq!(event["actual_cost_micros"], 0);
        assert_eq!(
            event["provider_error"]["message"].as_str(),
            Some(provider_message)
        );

        let mut terminal = test_execution_failure(
            "ai_provider_invalid_request",
            "The upstream model provider rejected the request as invalid.",
        );
        terminal.rustgrid_gateway_status = failure.rustgrid_gateway_status();
        terminal.upstream_provider_status = failure.upstream_provider_status;
        terminal.failure_stage = failure.failure_stage().map(str::to_owned);
        terminal.provider_contacted = failure.provider_contacted();
        terminal.call_budget_consumed = failure.call_budget_consumed();
        terminal.reservation_state = failure.reservation_state().map(str::to_owned);
        terminal.rustgrid_request_id = failure.rustgrid_request_id.clone();
        terminal.transport_request_id = failure.transport_request_id.clone();
        terminal.provider_error = failure.provider_error.clone();
        let terminal = serde_json::to_value(terminal).unwrap();
        assert!(
            terminal
                .as_object()
                .unwrap()
                .contains_key("rustgrid_gateway_status")
        );
        assert!(terminal["rustgrid_gateway_status"].is_null());
        assert_eq!(terminal["rustgrid_request_id"], rustgrid_request_id);
        assert_eq!(terminal["transport_request_id"], transport_request_id);
        assert_eq!(terminal["reservation_state"], "reconciled");

        let (terminal_code, terminal_message) = safe_failure(&error, false);
        assert_eq!(terminal_code, "ai_provider_invalid_request");
        assert_eq!(
            terminal_message,
            "The upstream model provider rejected the request as invalid."
        );
        assert!(!terminal_message.contains("registration"));
        assert!(!terminal_message.contains("uncertain"));
    }

    #[test]
    fn provider_request_metadata_is_string_typed_and_preflight_rejects_integer_values() {
        let metadata = provider_request_metadata(
            Uuid::from_u128(52),
            "AOPS-229",
            "rustgrid-agent-hosted",
            ExecutionPhase::Discovery,
            40,
        );
        assert_eq!(metadata["model_call_budget"], "40");
        assert!(metadata.as_object().unwrap().values().all(Value::is_string));
        let request = json!({
            "model": "gpt-5.6-sol",
            "input": [{"role": "user", "content": "bounded"}],
            "max_output_tokens": 100,
            "metadata": metadata,
        });
        validate_provider_request_envelope(&request).unwrap();

        let invalid = json!({
            "model": "gpt-5.6-sol",
            "input": [{"role": "user", "content": "bounded"}],
            "max_output_tokens": 100,
            "metadata": {
                "model_call_budget": 40
            },
        });
        let error = validate_provider_request_envelope(&invalid).unwrap_err();
        assert_eq!(
            error.to_string(),
            "ai_provider_request_invalid: metadata value `model_call_budget` must be a string"
        );
    }

    #[test]
    fn startup_provider_schema_failure_preserves_exact_code_path_and_zero_dispatch_evidence() {
        let invalid = json!([{
            "type": "function",
            "name": "read_file",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "array"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            },
            "strict": true
        }]);
        let validation = validate_provider_tool_definitions(&invalid).unwrap_err();
        let error = anyhow::Error::new(HostedProviderContractFailure::from_validation(validation));

        let (code, message) = safe_failure(&error, false);
        assert_eq!(code, "ai_tool_schema_invalid");
        assert!(message.contains("tools[0].parameters.properties.path.items"));

        let diagnostics = failure_diagnostics(&error, false);
        assert_eq!(diagnostics["code"], "ai_tool_schema_invalid");
        assert_eq!(diagnostics["failure_stage"], "request_validation");
        assert_eq!(diagnostics["provider_contacted"], false);
        assert_eq!(diagnostics["reservation_state"], "not_created");
        assert_eq!(diagnostics["call_budget_consumed"], false);
        assert_eq!(diagnostics["actual_cost_micros"], 0);
        assert!(
            diagnostics["message"]
                .as_str()
                .is_some_and(|value| value.contains("tools[0].parameters.properties.path.items"))
        );
    }

    #[test]
    fn large_safe_provider_400_retains_authoritative_fields_and_boundary_diagnostics() {
        let message = "m".repeat(MAX_PROVIDER_ERROR_MESSAGE_BYTES);
        let parameter = "p".repeat(MAX_PROVIDER_ERROR_PARAMETER_BYTES);
        let provider_response_body = json!({
            "error": {
                "message": "b".repeat(32 * 1024)
            }
        });
        let body = json!({
            "code": "ai_provider_invalid_request",
            "details": {
                "failure_stage": "provider_dispatch",
                "provider_contacted": true,
                "upstream_provider_status": 400,
                "rustgrid_gateway_status": null,
                "call_budget_consumed": false,
                "actual_cost_micros": 0,
                "provider_error": {
                    "type": "invalid_request_error",
                    "code": "invalid_type",
                    "message": message,
                    "parameter": parameter
                },
                "provider_response_body": provider_response_body,
                "provider_attempts": 1
            }
        });
        let Some((url, receiver, handle)) = one_request_server("400 Bad Request", body) else {
            return;
        };
        let response = hosted_http_client()
            .unwrap()
            .get(url)
            .send()
            .expect("provider error response");
        let error = decode_response::<Value>(response, "executions/id/ai/responses").unwrap_err();
        let failure = error
            .downcast_ref::<HostedHttpError>()
            .expect("typed hosted HTTP error");

        assert_eq!(failure.effective_code(), "ai_provider_invalid_request");
        assert_eq!(failure.failure_stage(), Some("provider_dispatch"));
        assert_eq!(failure.provider_contacted(), Some(true));
        assert_eq!(failure.upstream_provider_status, Some(400));
        assert_eq!(failure.rustgrid_gateway_status(), Some(None));
        assert_eq!(failure.call_budget_consumed(), Some(false));
        assert_eq!(failure.actual_cost_micros, Some(0));
        assert_eq!(failure.provider_attempts, Some(1));
        assert_eq!(
            failure
                .provider_error
                .as_ref()
                .and_then(|diagnostic| diagnostic.message.as_deref()),
            Some(message.as_str())
        );
        assert_eq!(
            failure
                .provider_error
                .as_ref()
                .and_then(|diagnostic| diagnostic.parameter.as_deref()),
            Some(parameter.as_str())
        );
        assert_eq!(
            failure.provider_response_body.as_ref(),
            Some(&provider_response_body)
        );

        receiver.recv().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn provider_failure_classes_have_distinct_authoritative_policies() {
        let cases = [
            (
                "ai_provider_request_failed",
                Some(400),
                AiFailureClass::ProviderValidation,
                "ai_provider_invalid_request",
            ),
            (
                "ai_provider_rate_limited",
                Some(429),
                AiFailureClass::ProviderRateLimit,
                "ai_provider_rate_limited",
            ),
            (
                "ai_provider_authentication_failed",
                Some(401),
                AiFailureClass::ProviderAuthentication,
                "ai_provider_authentication_failed",
            ),
            (
                "ai_provider_unavailable",
                Some(503),
                AiFailureClass::ProviderServer,
                "ai_provider_unavailable",
            ),
            (
                "ai_provider_timeout",
                Some(408),
                AiFailureClass::ProviderTimeout,
                "ai_provider_timeout",
            ),
        ];
        for (code, upstream_status, class, effective_code) in cases {
            let failure =
                test_hosted_http_error(StatusCode::BAD_GATEWAY, code, upstream_status, Some(true));
            assert_eq!(failure.failure_class(), class);
            assert_eq!(failure.effective_code(), effective_code);
            assert_eq!(failure.failure_stage(), Some("provider_dispatch"));
            assert_eq!(failure.rustgrid_gateway_status(), None);
            assert_eq!(failure.provider_contacted(), Some(true));
            assert!(!failure.retryable_gateway_transport_failure());
            assert!(!failure.terminal_message().contains("registration"));
        }

        let mut uncertain = test_hosted_http_error(
            StatusCode::BAD_GATEWAY,
            "ai_request_dispatch_uncertain",
            None,
            Some(true),
        );
        uncertain.failure_stage = Some("provider_dispatch".into());
        assert_eq!(
            uncertain.failure_class(),
            AiFailureClass::ProviderDispatchUncertain
        );
        assert_eq!(uncertain.budget_disposition(), AiBudgetDisposition::Unknown);
        assert!(uncertain.terminal_message().contains("could not determine"));
    }

    #[test]
    fn explicit_provider_400_overrides_the_legacy_conflict_template() {
        let mut failure = test_hosted_http_error(
            StatusCode::CONFLICT,
            "ai_provider_request_failed",
            Some(400),
            Some(true),
        );
        failure.failure_stage = Some("request_registration".into());
        failure.call_budget_consumed = Some(false);
        failure.actual_cost_micros = Some(0);

        assert_eq!(failure.failure_class(), AiFailureClass::ProviderValidation);
        assert_eq!(failure.effective_code(), "ai_provider_invalid_request");
        assert_eq!(failure.failure_stage(), Some("provider_dispatch"));
        assert_eq!(failure.rustgrid_gateway_status(), None);
        assert_eq!(
            failure.terminal_message(),
            "The upstream model provider rejected the request as invalid."
        );
        assert!(!failure.retryable_registration_failure());
    }

    #[test]
    fn definite_provider_400_overrides_stale_dispatch_uncertain_code() {
        let mut failure = test_hosted_http_error(
            StatusCode::BAD_GATEWAY,
            "ai_request_dispatch_uncertain",
            Some(400),
            Some(true),
        );
        failure.call_budget_consumed = Some(false);
        failure.actual_cost_micros = Some(0);

        assert_eq!(failure.failure_class(), AiFailureClass::ProviderValidation);
        assert_eq!(failure.effective_code(), "ai_provider_invalid_request");
        assert_eq!(failure.failure_stage(), Some("provider_dispatch"));
        assert_eq!(failure.upstream_provider_status, Some(400));
        assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Restore);
    }

    #[test]
    fn only_confirmed_non_billable_provider_validation_restores_semantic_budget() {
        let mut failure = test_hosted_http_error(
            StatusCode::BAD_GATEWAY,
            "ai_provider_invalid_request",
            Some(400),
            Some(true),
        );
        failure.call_budget_consumed = Some(false);
        failure.actual_cost_micros = Some(0);

        let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        ledger.begin_model_call().unwrap();
        assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Restore);
        ledger
            .rollback_model_call(ExecutionPhase::Discovery)
            .unwrap();
        assert_eq!(ledger.budgeted_calls(), 0);

        failure.actual_cost_micros = None;
        assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Unknown);
        failure.actual_cost_micros = Some(0);
        failure.call_budget_consumed = Some(true);
        assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Consumed);
    }

    #[test]
    fn authoritative_non_billable_adapter_preflight_restores_semantic_budget() {
        let mut failure = test_hosted_http_error(
            StatusCode::BAD_REQUEST,
            "ai_tool_schema_invalid",
            None,
            Some(false),
        );
        failure.failure_stage = Some("request_validation".into());
        failure.call_budget_consumed = Some(false);
        failure.actual_cost_micros = Some(0);
        failure.reservation_state = Some("not_created".into());

        assert_eq!(failure.failure_class(), AiFailureClass::RequestValidation);
        assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Restore);
        assert!(!failure.retryable_gateway_transport_failure());
        assert!(!failure.retryable_registration_failure());

        let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        ledger.begin_model_call().unwrap();
        ledger
            .rollback_model_call(ExecutionPhase::Discovery)
            .unwrap();
        assert_eq!(ledger.budgeted_calls(), 0);

        failure.reservation_state = None;
        assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Unknown);
        failure.failure_stage = None;
        failure.reservation_state = Some("not_created".into());
        assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Restore);
    }

    #[test]
    fn explicit_non_billable_pre_dispatch_release_restores_semantic_budget() {
        let mut failure = test_hosted_http_error(
            StatusCode::BAD_GATEWAY,
            "ai_provider_connection_not_found",
            None,
            Some(false),
        );
        failure.failure_stage = Some("provider_credential_resolution".into());
        failure.call_budget_consumed = Some(false);
        failure.actual_cost_micros = Some(0);
        failure.reservation_state = Some("released".into());

        assert_eq!(failure.failure_class(), AiFailureClass::Gateway);
        assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Restore);

        failure.actual_cost_micros = None;
        assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Unknown);
        failure.actual_cost_micros = Some(0);
        failure.upstream_provider_status = Some(400);
        assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Unknown);
    }

    #[test]
    fn ambiguous_legacy_provider_failure_does_not_fabricate_registration_evidence() {
        let failure = HostedHttpError {
            status: StatusCode::CONFLICT,
            path: "executions/id/ai/responses".into(),
            code: "ai_provider_request_failed".into(),
            request_id: Some("24162c59-38d5-4705-80f9-717c8c26ee29".into()),
            rustgrid_gateway_status: None,
            upstream_provider_status: None,
            failure_stage: None,
            provider_contacted: None,
            call_budget_consumed: None,
            reservation_state: None,
            reservation_reconciliation_state: None,
            retryable: None,
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
        };

        assert_eq!(failure.failure_class(), AiFailureClass::Gateway);
        assert_eq!(failure.effective_code(), "ai_provider_request_failed");
        assert_eq!(failure.failure_stage(), None);
        assert_eq!(failure.provider_contacted(), None);
        assert_eq!(failure.call_budget_consumed(), None);
        assert_eq!(failure.reservation_reconciliation_state(), None);
        assert_eq!(failure.rustgrid_gateway_status(), Some(Some(409)));
        assert_eq!(
            failure.terminal_message(),
            "The RustGrid AI gateway rejected the model call."
        );
        assert!(!failure.retryable_registration_failure());
    }

    #[test]
    fn provider_invalid_code_without_dispatch_evidence_remains_gateway_unknown() {
        let failure = test_hosted_http_error(
            StatusCode::BAD_GATEWAY,
            "ai_provider_invalid_request",
            None,
            None,
        );

        assert_eq!(failure.failure_class(), AiFailureClass::Gateway);
        assert_eq!(failure.effective_code(), "ai_provider_invalid_request");
        assert_eq!(failure.failure_stage(), None);
        assert_eq!(failure.provider_contacted(), None);
        assert_eq!(failure.rustgrid_gateway_status(), Some(Some(502)));
        assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Unknown);
    }

    #[test]
    fn settled_pre_dispatch_registration_is_retryable_even_if_legacy_flag_is_false() {
        let failure = HostedHttpError {
            status: StatusCode::CONFLICT,
            path: "executions/id/ai/responses".into(),
            code: "ai_request_idempotency_conflict".into(),
            request_id: Some("24162c59-38d5-4705-80f9-717c8c26ee29".into()),
            rustgrid_gateway_status: None,
            upstream_provider_status: None,
            failure_stage: Some("request_registration".into()),
            provider_contacted: Some(false),
            call_budget_consumed: Some(false),
            reservation_state: None,
            reservation_reconciliation_state: Some("previous_request_settled".into()),
            retryable: Some(false),
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
        };

        assert!(failure.retryable_registration_failure());
    }

    #[test]
    fn registration_retry_delays_are_bounded_and_jittered() {
        let semantic_call_id = Uuid::from_u128(0x1234);
        let first = registration_retry_delay(0, semantic_call_id);
        let second = registration_retry_delay(1, semantic_call_id);
        let third = registration_retry_delay(2, semantic_call_id);

        assert!((Duration::from_millis(200)..=Duration::from_millis(300)).contains(&first));
        assert!((Duration::from_millis(800)..=Duration::from_millis(1_200)).contains(&second));
        assert!((Duration::from_millis(2_400)..=Duration::from_millis(3_600)).contains(&third));
    }

    #[test]
    fn ai_gateway_request_and_transport_retries_stop_at_execution_deadline() {
        let execution_id = Uuid::from_u128(0xdead_1e18);
        let Some((base, requests, server)) = delayed_no_response_server(Duration::from_millis(500))
        else {
            return;
        };
        let client = test_api_client(base, execution_id);
        let started = Instant::now();
        let error = client
            .ai_response_until(
                json!({
                    "model": "gpt-5.6-sol",
                    "input": "bounded",
                    "max_output_tokens": 100,
                    "store": false,
                    "stream": false
                }),
                &ai_call_registration(
                    execution_id,
                    1,
                    Uuid::from_u128(0xdead_1e19),
                    0,
                    ExecutionPhase::Implementation,
                    0,
                ),
                Some(Instant::now() + Duration::from_millis(75)),
            )
            .unwrap_err();
        let elapsed = started.elapsed();
        server.join().unwrap();
        let requests = requests.try_iter().collect::<Vec<_>>();

        assert!(error.to_string().contains("hosted execution deadline"));
        assert!(
            elapsed < Duration::from_millis(350),
            "deadline-bounded request took {elapsed:?}"
        );
        assert_eq!(
            requests.len(),
            1,
            "deadline must suppress transport retries"
        );
    }

    #[test]
    fn ai_gateway_and_completion_use_execution_bearer_and_idempotency_keys() {
        let execution_id = Uuid::from_u128(44);
        let Some((base, ai_request, ai_server)) =
            one_request_server("200 OK", json!({"output": []}))
        else {
            return;
        };
        let client = test_api_client(base, execution_id);
        client
            .ai_response(
                json!({
                    "model": "gpt-5.6-sol",
                    "input": "bounded",
                    "max_output_tokens": 100,
                    "store": false,
                    "stream": false
                }),
                &ai_call_registration(
                    execution_id,
                    1,
                    Uuid::from_u128(45),
                    0,
                    ExecutionPhase::Discovery,
                    0,
                ),
            )
            .unwrap();
        ai_server.join().unwrap();
        let ai_request = ai_request.recv().unwrap();
        assert!(ai_request.starts_with(&format!(
            "POST /api/v1/executions/{execution_id}/ai/responses HTTP/1.1"
        )));
        assert!(ai_request.contains("idempotency-key:"));
        assert!(ai_request.contains("x-rustgrid-semantic-call-id:"));
        assert!(ai_request.contains("x-rustgrid-call-index: 0"));
        assert!(ai_request.contains("x-rustgrid-call-phase: discovery"));
        assert!(ai_request.contains("x-rustgrid-registration-attempt: 0"));
        assert!(ai_request.contains("authorization: Bearer rge_"));
        assert!(!ai_request.contains("OPENAI_API_KEY"));

        let Some((completion_base, completion_request, completion_server)) =
            one_request_server("200 OK", json!({"status": "failed"}))
        else {
            return;
        };
        let completion_client = test_api_client(completion_base, execution_id);
        completion_client
            .complete(&CompletionRequest {
                status: "failed".into(),
                mission_outcome: None,
                process_health: Some("failed".into()),
                completion_evaluation: None,
                output_summary: None,
                failure_code: Some("validation_failed".into()),
                failure_message: Some("Required validation failed.".into()),
                head_branch: None,
                head_sha: None,
                pull_request_number: None,
                pull_request_url: None,
            })
            .unwrap();
        completion_server.join().unwrap();
        let completion_request = completion_request.recv().unwrap();
        assert!(completion_request.starts_with(&format!(
            "POST /api/v1/executions/{execution_id}/complete HTTP/1.1"
        )));
        assert!(completion_request.contains("idempotency-key:"));
        assert!(completion_request.contains("\"failure_code\":\"validation_failed\""));
        assert!(!completion_request.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn notebook_events_use_stable_idempotency_keys() {
        let execution_id = Uuid::from_u128(0x45454545_4545_4545_8545_454545454545);
        let Some((base, request, server)) = one_request_server("200 OK", json!({})) else {
            return;
        };
        let client = test_api_client(base, execution_id);
        client
            .append_event(
                "progress",
                json!({
                    "event_type": "worker.notebook_checkpoint",
                    "notebook_revision": 7,
                    "artifact_hash": "a".repeat(64),
                }),
            )
            .unwrap();
        server.join().unwrap();
        let request = request.recv().unwrap();
        assert!(request.starts_with(&format!(
            "POST /api/v1/executions/{execution_id}/worker-events HTTP/1.1"
        )));
        assert!(request.contains("idempotency-key:"));
        assert!(request.contains("\"notebook_revision\":7"));
    }

    #[test]
    fn duplicate_partial_completions_have_the_same_idempotency_identity() {
        let execution_id = Uuid::from_u128(0x50505050_5050_4050_8050_505050505050);
        let completion = CompletionRequest {
            status: "partial_result".into(),
            mission_outcome: Some(CompletionStatus::Partial),
            process_health: Some("healthy".into()),
            completion_evaluation: None,
            output_summary: Some("Continue implementation.".into()),
            failure_code: None,
            failure_message: None,
            head_branch: Some("rustgrid/continuation".into()),
            head_sha: Some("a".repeat(40)),
            pull_request_number: Some(17),
            pull_request_url: Some("https://github.com/RustGrid/example/pull/17".into()),
        };
        let first = completion_idempotency_key(execution_id, &completion).unwrap();
        let second = completion_idempotency_key(execution_id, &completion).unwrap();
        assert_eq!(first, second);

        let mut changed = completion;
        changed.output_summary = Some("Different remaining work.".into());
        assert_ne!(
            first,
            completion_idempotency_key(execution_id, &changed).unwrap()
        );
    }

    #[test]
    fn github_repository_token_request_is_bodyless_and_scope_checked() {
        let execution_id = Uuid::from_u128(46);
        let Some((base, request, server)) = one_request_server(
            "200 OK",
            json!({
                "token": "installation-token",
                "expires_at": "2099-01-01T00:00:00Z",
                "permissions": {"contents": "write", "pull_requests": "write"},
                "repository": "RustGrid/example"
            }),
        ) else {
            return;
        };
        let client = test_api_client(base, execution_id);
        let token = client.github_token("rustgrid/EXAMPLE").unwrap();
        server.join().unwrap();
        assert_eq!(token.expose(), "installation-token");
        let request = request.recv().unwrap();
        assert!(request.starts_with(&format!(
            "POST /api/v1/executions/{execution_id}/github-token HTTP/1.1"
        )));
        assert!(request.ends_with("\r\n\r\n"));
        assert!(!request.contains("{}"));
    }

    #[test]
    fn github_repository_token_must_have_a_safe_remaining_lifetime() {
        let execution_id = Uuid::from_u128(47);
        let Some((base, _, server)) = one_request_server(
            "200 OK",
            json!({
                "token": "already-expired-installation-token",
                "expires_at": "2000-01-01T00:00:00Z",
                "permissions": {"contents": "write", "pull_requests": "write"},
                "repository": "RustGrid/example"
            }),
        ) else {
            return;
        };
        let client = test_api_client(base, execution_id);
        assert!(client.github_token("RustGrid/example").is_err());
        server.join().unwrap();
    }
}
