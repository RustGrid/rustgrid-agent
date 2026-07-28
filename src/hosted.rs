//! Ephemeral GitHub Actions execution.
//!
//! This module is intentionally separate from the persistent worker client. It
//! never loads an [`AppContext`](crate::config::AppContext), a keyring entry, or
//! Codex/ChatGPT authentication. GitHub OIDC is exchanged once for a
//! short-lived, mission-scoped execution token; that token remains in this
//! process and is stripped from every repository subprocess.

use std::{
    collections::{BTreeSet, VecDeque},
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
    github::GitHubClient,
    shutdown,
    telemetry::{
        ExecutionSnapshot, ExecutionStatus, PhaseSnapshot, TELEMETRY_VERSION, TelemetryBatch,
        TelemetryEvent, TelemetryPayload, now_rfc3339,
    },
    token::parse_rfc3339_utc,
};

const EXECUTION_LEASE_SECONDS: i64 = 900;
const EXECUTION_TOKEN_TTL_SECONDS: i64 = 900;
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(180);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const MAX_HTTP_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_HTTP_ERROR_BYTES: usize = 16 * 1024;
const MAX_TOOL_OUTPUT_BYTES: usize = 48 * 1024;
const MAX_MODEL_FILE_BYTES: usize = 512 * 1024;
const MAX_MODEL_CALLS_HARD_LIMIT: usize = 64;
const MAX_REPAIR_ATTEMPTS: usize = 2;
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
        Ok(())
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

#[derive(Debug)]
struct HostedHttpError {
    status: StatusCode,
    path: String,
    code: String,
    request_id: Option<String>,
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
        let _: Value = self.send_json(
            Method::POST,
            &format!("executions/{}/worker-events", self.execution_id),
            Some(json!({"event_type": event_type, "data": data})),
            None,
            1,
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

    fn ai_response(&self, body: Value, idempotency_key: Uuid) -> Result<Value> {
        self.send_json(
            Method::POST,
            &format!("executions/{}/ai/responses", self.execution_id),
            Some(body),
            Some(idempotency_key),
            3,
        )
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
        let encoded = serde_json::to_vec(&body)?;
        let key_material = [
            b"completion:".as_slice(),
            self.execution_id.as_bytes().as_slice(),
            encoded.as_slice(),
        ]
        .concat();
        let key = Uuid::new_v5(&HOSTED_NAMESPACE, &key_material);
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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HostedExecutionPolicy {
    policy_version: i32,
    codex: HostedCodexPolicy,
    quality_gates: Vec<HostedQualityGate>,
    timeout_seconds: i64,
    sandbox: HostedSandboxPolicy,
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

impl HostedManifest {
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
        if self.manifest_version != 3
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
        if self.ai_gateway.model.trim().is_empty()
            || self.ai_gateway.model.len() > 100
            || self.ai_gateway.model.chars().any(char::is_whitespace)
            || self.execution.model.as_deref() != Some(self.ai_gateway.model.as_str())
            || self.ai_gateway.maximum_input_tokens < 1
            || self.ai_gateway.maximum_output_tokens < 1
            || !(1..=MAX_MODEL_CALLS_HARD_LIMIT as i32)
                .contains(&self.ai_gateway.maximum_model_calls)
            || maximum_cost.is_err()
            || maximum_cost.is_ok_and(|value| !value.is_finite() || value <= 0.0)
            || self.execution.maximum_input_tokens != Some(self.ai_gateway.maximum_input_tokens)
            || self.execution.maximum_output_tokens != Some(self.ai_gateway.maximum_output_tokens)
            || self.execution.maximum_model_calls != Some(self.ai_gateway.maximum_model_calls)
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
            let _ = api.complete(&CompletionRequest {
                status: "failed".into(),
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

    let result = run_hosted_execution(&api, &manifest, &running);
    supervisor.stop();
    let terminal_at = now_rfc3339();
    match result {
        Ok(result) => {
            send_execution_telemetry(
                &api,
                execution_id,
                &started_at,
                Some(&terminal_at),
                ExecutionStatus::Succeeded,
                2,
            );
            if let Err(error) = api.append_event(
                "result",
                json!({
                    "status": "completed",
                    "branch": result.branch,
                    "head_sha": result.commit,
                    "pull_request_number": result.pull_request.number,
                    "pull_request_url": result.pull_request.url,
                    "validation": result.validation
                }),
            ) {
                eprintln!(
                    "[warning] hosted result-event delivery failed before terminal completion: {error:#}"
                );
            }
            api.complete(&CompletionRequest {
                status: "completed".into(),
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
            let _ = api.append_event(
                "result",
                json!({"status": if cancelled { "cancelled" } else { "failed" }, "code": code}),
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
            Err(error)
        }
    }
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
    running: &Arc<AtomicBool>,
) -> Result<HostedResult> {
    ensure_running(running)?;
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
    repo.configure_hosted_author()?;
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

    let mut agent = GatewayAgent::new(api.clone(), manifest, &repo, running, &containment);
    bootstrap_hosted_dependencies(api, manifest, &repo, running, &containment)?;
    let summary = if existing_pr.is_some() && resumed {
        "Recovered an existing hosted execution branch and pull request.".to_owned()
    } else {
        agent
            .implement()
            .context("the RustGrid AI gateway implementation session failed")?
    };
    ensure_running(running)?;

    api.update_state("validating")?;
    let mut validation_round = 1_u32;
    let mut validation = run_quality_gates(
        api,
        manifest,
        &repo,
        running,
        &manifest.execution_policy,
        &containment,
        validation_round,
    )?;
    for repair_attempt in 0..MAX_REPAIR_ATTEMPTS {
        let failures = validation
            .iter()
            .filter(|result| result.status != "passed")
            .cloned()
            .collect::<Vec<_>>();
        if failures.is_empty() {
            break;
        }
        agent.repair(&failures, repair_attempt + 1)?;
        validation_round = validation_round.saturating_add(1);
        validation = run_quality_gates(
            api,
            manifest,
            &repo,
            running,
            &manifest.execution_policy,
            &containment,
            validation_round,
        )?;
    }
    if validation.iter().any(|result| result.status != "passed") {
        bail!("required hosted execution validation failed");
    }

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
        if existing_pr.is_none() {
            bail!("the hosted execution produced no committable changes");
        }
        command::checked("git", ["rev-parse", "HEAD"], &repo.root)?
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
        },
        &mut commit,
        &mut validation,
    )?;
    api.update_state("creating_pull_request")?;
    containment.drain()?;
    let publication_token = api.github_token(&manifest.github.repository)?;
    let github = GitHubClient::new(publication_token.expose(), &manifest.github.web_base_url)?;
    let pull = match existing_pr {
        Some(pull) => pull,
        None => find_or_create_hosted_pull_request(&github, &repo_config, manifest, &validation)?,
    };
    drop(github);
    drop(publication_token);
    Ok(HostedResult {
        summary,
        branch: manifest.github.branch.clone(),
        commit,
        pull_request: PullRequestResult {
            number: pull.number,
            url: pull.html_url,
        },
        validation,
    })
}

fn find_or_create_hosted_pull_request(
    github: &GitHubClient,
    repo_config: &RepoConfig,
    manifest: &HostedManifest,
    validation: &[ValidationResult],
) -> Result<crate::github::PullRequest> {
    if let Some(pull) = github.find_open_pull_request(repo_config, &manifest.github.branch)? {
        return Ok(pull);
    }
    match github.create_pull_request(
        repo_config,
        &format!("{}: {}", manifest.ticket_key, manifest.ticket_title),
        &hosted_pull_request_body(manifest, validation),
        &manifest.github.branch,
        normalized_base_ref(&manifest.github.base_ref)?,
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

struct HostedPublicationContext<'a> {
    api: &'a HostedApiClient,
    manifest: &'a HostedManifest,
    repo: &'a Repo,
    repo_config: &'a RepoConfig,
    running: &'a Arc<AtomicBool>,
    trusted_git_config: &'a [u8],
    containment: &'a command::HostedProcessContainment,
    validation_round: &'a mut u32,
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
    } = context;
    for attempt in 1..=3 {
        ensure_running(running)?;
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
    calls_used: usize,
}

impl<'a> GatewayAgent<'a> {
    fn new(
        api: HostedApiClient,
        manifest: &'a HostedManifest,
        repo: &'a Repo,
        running: &'a Arc<AtomicBool>,
        containment: &'a command::HostedProcessContainment,
    ) -> Self {
        Self {
            api,
            manifest,
            repo,
            running,
            containment,
            calls_used: 0,
        }
    }

    fn implement(&mut self) -> Result<String> {
        let prompt = build_hosted_prompt(self.manifest, self.repo)?;
        self.run_session(&prompt)
    }

    fn repair(&mut self, failures: &[ValidationResult], attempt: usize) -> Result<String> {
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
        self.run_session(&format!(
            "Repair validation attempt {attempt} for RustGrid ticket {}. Inspect the current diff and make the smallest correct changes needed for these failures. Do not commit, push, create branches, or open pull requests.\n\n{diagnostics}",
            self.manifest.ticket_key
        ))
    }

    fn run_session(&mut self, prompt: &str) -> Result<String> {
        let initial = json!({"role": "user", "content": prompt});
        let mut turns = VecDeque::<Vec<Value>>::new();
        loop {
            ensure_running(self.running)?;
            let maximum_calls = usize::try_from(self.manifest.ai_gateway.maximum_model_calls)
                .unwrap_or_default()
                .min(MAX_MODEL_CALLS_HARD_LIMIT);
            if self.calls_used >= maximum_calls {
                bail!("execution AI model-call budget was exhausted");
            }
            let mut input = vec![initial.clone()];
            for turn in &turns {
                input.extend(turn.iter().cloned());
            }
            let max_output_tokens = self.manifest.ai_gateway.maximum_output_tokens.min(16_384);
            let mut request = json!({
                "model": self.manifest.ai_gateway.model,
                "input": input,
                "instructions": hosted_agent_instructions(),
                "max_output_tokens": max_output_tokens,
                "reasoning": {"effort": "medium"},
                "tools": hosted_tools(),
                "tool_choice": "auto",
                "parallel_tool_calls": false,
                "metadata": {
                    "execution_id": self.manifest.execution.execution_id,
                    "ticket_key": self.manifest.ticket_key,
                    "agent": "rustgrid-agent-hosted"
                },
                "store": false,
                "stream": false
            });
            while serde_json::to_vec(&request)?.len()
                > usize::try_from(self.manifest.ai_gateway.maximum_input_tokens).unwrap_or_default()
                && !turns.is_empty()
            {
                turns.pop_front();
                let mut reduced = vec![initial.clone()];
                for turn in &turns {
                    reduced.extend(turn.iter().cloned());
                }
                request["input"] = Value::Array(reduced);
            }
            if serde_json::to_vec(&request)?.len()
                > usize::try_from(self.manifest.ai_gateway.maximum_input_tokens).unwrap_or_default()
            {
                bail!("hosted agent context exceeds the execution input-token ceiling");
            }

            self.calls_used += 1;
            let request_sha = Sha256::digest(serde_json::to_vec(&request)?);
            let call_number = self.calls_used.to_be_bytes();
            let idempotency_material = [
                b"ai:".as_slice(),
                self.manifest.execution.execution_id.as_bytes().as_slice(),
                call_number.as_slice(),
                request_sha.as_slice(),
            ]
            .concat();
            let idempotency_key = Uuid::new_v5(&HOSTED_NAMESPACE, &idempotency_material);
            self.api.append_event(
                "progress",
                json!({
                    "step": "ai_gateway",
                    "status": "running",
                    "model_call": self.calls_used,
                    "model": self.manifest.ai_gateway.model
                }),
            )?;
            let response = self.api.ai_response(request, idempotency_key)?;
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
                self.api.append_event(
                    "message",
                    json!({
                        "step": "ai_gateway",
                        "status": "completed",
                        "model_calls_used": self.calls_used,
                        "summary": truncate_text(&summary, 4_000)
                    }),
                )?;
                return Ok(truncate_text(&summary, 16_000));
            }
            for (call_id, name, arguments) in function_calls {
                ensure_running(self.running)?;
                let result = match self.execute_tool(&name, &arguments) {
                    Ok(output) => {
                        json!({"ok": true, "output": truncate_text(&output, MAX_TOOL_OUTPUT_BYTES)})
                    }
                    Err(error) => json!({
                        "ok": false,
                        "error": truncate_text(&format!("{error:#}"), 4_000)
                    }),
                };
                self.api.append_event(
                    "tool",
                    json!({
                        "tool": name,
                        "status": if result["ok"] == true { "completed" } else { "failed" }
                    }),
                )?;
                turn.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": serde_json::to_string(&result)?
                }));
            }
            turns.push_back(turn);
            while turns.len() > 8 {
                turns.pop_front();
            }
        }
    }

    fn execute_tool(&self, name: &str, raw_arguments: &str) -> Result<String> {
        let arguments: Value =
            serde_json::from_str(raw_arguments).context("tool arguments are not valid JSON")?;
        let object = arguments
            .as_object()
            .context("tool arguments must be an object")?;
        match name {
            "list_files" => {
                let path = object.get("path").and_then(Value::as_str).unwrap_or(".");
                let root = safe_repo_path(&self.repo.root, path, false)?;
                let files = collect_repo_files(&self.repo.root, &root, 1_000)?;
                Ok(files.join("\n"))
            }
            "read_file" => {
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
                read_repo_file(&self.repo.root, path, start_line, end_line)
            }
            "search_text" => {
                let query = required_tool_string(object, "query", 200)?;
                let path = object.get("path").and_then(Value::as_str).unwrap_or(".");
                search_repo(&self.repo.root, path, query)
            }
            "write_file" => {
                let path = required_tool_string(object, "path", 4_096)?;
                let content = required_tool_string(object, "content", MAX_MODEL_FILE_BYTES)?;
                let target = safe_repo_path(&self.repo.root, path, true)?;
                if content.len() > MAX_MODEL_FILE_BYTES {
                    bail!("write_file content exceeds the hosted tool limit");
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("could not create repository directory {}", parent.display())
                    })?;
                }
                fs::write(&target, content.as_bytes())
                    .with_context(|| format!("could not write repository file {path}"))?;
                Ok(format!("wrote {} bytes to {path}", content.len()))
            }
            "delete_file" => {
                let path = required_tool_string(object, "path", 4_096)?;
                let target = safe_repo_path(&self.repo.root, path, false)?;
                if !target.is_file() {
                    bail!("delete_file target is not a regular file");
                }
                fs::remove_file(&target)
                    .with_context(|| format!("could not delete repository file {path}"))?;
                Ok(format!("deleted {path}"))
            }
            "run_focused_command" => {
                let command_text = required_tool_string(object, "command", 8 * 1024)?;
                validate_model_command(command_text)?;
                let allowlist = self.manifest.execution_policy.child_environment_allowlist();
                let output = command::capture_hosted_cancellable_with_environment(
                    command_text,
                    &self.repo.root,
                    self.running,
                    Duration::from_secs(180),
                    MAX_TOOL_OUTPUT_BYTES,
                    Some(&allowlist),
                    None,
                    self.containment,
                )?;
                Ok(format!(
                    "exit={}\nstdout:\n{}\nstderr:\n{}",
                    output.status,
                    truncate_text(&output.stdout, MAX_TOOL_OUTPUT_BYTES / 2),
                    truncate_text(&output.stderr, MAX_TOOL_OUTPUT_BYTES / 2)
                ))
            }
            _ => bail!("unsupported hosted model tool `{name}`"),
        }
    }
}

fn hosted_tools() -> Vec<Value> {
    vec![
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
                    "end_line": {"type": ["integer", "null"], "minimum": 1}
                },
                "required": ["path", "start_line", "end_line"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "search_text",
            "description": "Search UTF-8 repository files for a literal string. Use a null path for the repository root.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "path": {"type": ["string", "null"]}
                },
                "required": ["query", "path"],
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
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"],
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
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "run_focused_command",
            "description": "Run one focused, non-publication command in the repository. The environment contains no RustGrid, GitHub, or OpenAI credential.",
            "parameters": {
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            },
            "strict": true
        }),
    ]
}

fn hosted_agent_instructions() -> &'static str {
    "You are the implementation model inside an ephemeral RustGrid GitHub Actions worker. \
Use only the provided repository tools. Inspect the smallest relevant scope, follow repository \
instructions, implement the mission, add focused tests, and inspect your final changes. Never \
commit, push, switch branches, modify Git remotes, open pull requests, read environment variables, \
read files outside the repository, or attempt to discover credentials. The RustGrid worker owns \
full quality gates and publication. End with a concise implementation and focused-validation summary."
}

fn build_hosted_prompt(manifest: &HostedManifest, repo: &Repo) -> Result<String> {
    let files = collect_repo_files(&repo.root, &repo.root, 1_200)?;
    let instructions = read_repo_instructions(&repo.root)?
        .into_iter()
        .map(|(name, content)| {
            format!(
                "\n\nRepository instruction file {name}:\n{}",
                truncate_text(&content, 24_000)
            )
        })
        .collect::<String>();
    Ok(format!(
        "Implement RustGrid ticket {key}: {title}\n\nMission instructions:\n{prompt}\n\n\
Execution attempt: {attempt}\nDeterministic branch: {branch}\nResolved model: {model}\n\
Maximum model calls: {calls}\nMaximum cost USD: {cost}\n\nRepository files:\n{files}{instructions}",
        key = manifest.ticket_key,
        title = manifest.ticket_title,
        prompt = manifest.run.input_prompt,
        attempt = manifest.execution.attempt_number,
        branch = manifest.github.branch,
        model = manifest.ai_gateway.model,
        calls = manifest.ai_gateway.maximum_model_calls,
        cost = manifest.ai_gateway.maximum_cost_usd,
        files = files.join("\n"),
    ))
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

fn run_quality_gates(
    api: &HostedApiClient,
    manifest: &HostedManifest,
    repo: &Repo,
    running: &Arc<AtomicBool>,
    policy: &HostedExecutionPolicy,
    containment: &command::HostedProcessContainment,
    validation_round: u32,
) -> Result<Vec<ValidationResult>> {
    let allowlist = policy.child_environment_allowlist();
    let workflow_run_attempt = manifest
        .execution
        .github_actions
        .as_ref()
        .and_then(|execution| execution.workflow_run_attempt)
        .context("validated manifest has no GitHub workflow run attempt")?;
    let mut results = Vec::new();
    for gate in policy.quality_gates.iter().filter(|gate| gate.required) {
        ensure_running(running)?;
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
        let output = command::capture_hosted_cancellable_with_environment(
            &gate.command,
            &repo.root,
            running,
            Duration::from_secs(u64::try_from(gate.timeout_seconds).unwrap_or(1)),
            2 * 1024 * 1024,
            Some(&allowlist),
            None,
            containment,
        );
        let result = match output {
            Ok(output) => {
                let combined = format!("{}\n{}", output.stdout, output.stderr);
                ValidationResult {
                    id: gate.id.clone(),
                    command: gate.command.clone(),
                    status: if output.status.success() {
                        "passed".into()
                    } else {
                        "failed".into()
                    },
                    output: truncate_text(&combined, 16_000),
                }
            }
            Err(error) => ValidationResult {
                id: gate.id.clone(),
                command: gate.command.clone(),
                status: "failed".into(),
                output: truncate_text(&format!("{error:#}"), 16_000),
            },
        };
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
            }),
        )?;
        results.push(result);
    }
    Ok(results)
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
        let bytes = read_bounded_response(response, MAX_HTTP_ERROR_BYTES).unwrap_or_default();
        let code = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("code")
                    .and_then(Value::as_str)
                    .filter(|value| safe_identifier(value, 100))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("http_{}", status.as_u16()));
        return Err(HostedHttpError {
            status,
            path: path.to_owned(),
            code,
            request_id,
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
        return (
            failure.code.clone(),
            format!(
                "RustGrid rejected a hosted execution operation with {}.",
                failure.code
            ),
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
        output_summary: None,
        failure_code: (!cancelled).then_some(failure_code),
        failure_message: (!cancelled).then_some(failure_message),
        head_branch: None,
        head_sha: None,
        pull_request_number: None,
        pull_request_url: None,
    }
}

fn hosted_pull_request_body(manifest: &HostedManifest, validation: &[ValidationResult]) -> String {
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
    format!(
        "Implements RustGrid ticket **{}** through the ephemeral GitHub Actions provider.\n\n\
Execution: `{}` (attempt {})\nModel: `{}`\nMaximum cost: `${}`\n\n\
Validation:\n{}\n\n_The OpenAI credential remained encrypted in RustGrid and was never sent to this runner._",
        manifest.ticket_key,
        manifest.execution.execution_id,
        manifest.execution.attempt_number,
        manifest.ai_gateway.model,
        manifest.ai_gateway.maximum_cost_usd,
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

fn validate_model_command(value: &str) -> Result<()> {
    let parts = command::parse(value)?;
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

fn read_repo_file(root: &Path, value: &str, start_line: u64, end_line: u64) -> Result<String> {
    let path = safe_repo_path(root, value, false)?;
    let metadata = fs::metadata(&path)?;
    if !metadata.is_file() || metadata.len() > MAX_MODEL_FILE_BYTES as u64 {
        bail!("read_file target is not a bounded regular file");
    }
    let bytes = fs::read(&path)?;
    if bytes.contains(&0) {
        bail!("read_file does not expose binary files");
    }
    let text = String::from_utf8(bytes).context("read_file target is not UTF-8")?;
    let mut output = String::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index as u64 + 1;
        if line_number < start_line {
            continue;
        }
        if line_number > end_line {
            break;
        }
        output.push_str(&format!("{line_number:>6} | {line}\n"));
        if output.len() >= MAX_TOOL_OUTPUT_BYTES {
            output.push_str("[read output truncated]\n");
            break;
        }
    }
    Ok(output)
}

fn search_repo(root: &Path, value: &str, query: &str) -> Result<String> {
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
    let mut output = String::new();
    let mut matches = 0usize;
    for candidate in candidates {
        if candidate.starts_with('[') {
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
        for (index, line) in text.lines().enumerate() {
            if line.contains(query) {
                output.push_str(&format!(
                    "{}:{}:{}\n",
                    candidate,
                    index + 1,
                    truncate_text(line, 1_000)
                ));
                matches += 1;
                if matches >= 100 || output.len() >= MAX_TOOL_OUTPUT_BYTES {
                    output.push_str("[search output truncated]\n");
                    return Ok(output);
                }
            }
        }
    }
    if output.is_empty() {
        output.push_str("no matches\n");
    }
    Ok(output)
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
    fn hosted_tools_have_only_the_gateway_allowed_function_shape() {
        for tool in hosted_tools() {
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
        });
        let (code, message) = safe_failure(&error, false);
        assert_eq!(code, "ai_provider_unavailable");
        assert_eq!(
            message,
            "RustGrid rejected a hosted execution operation with ai_provider_unavailable."
        );
        assert!(!message.contains("responses"));
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
                Uuid::from_u128(45),
            )
            .unwrap();
        ai_server.join().unwrap();
        let ai_request = ai_request.recv().unwrap();
        assert!(ai_request.starts_with(&format!(
            "POST /api/v1/executions/{execution_id}/ai/responses HTTP/1.1"
        )));
        assert!(ai_request.contains("idempotency-key: 00000000-0000-0000-0000-00000000002d"));
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
