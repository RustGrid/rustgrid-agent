// Extracted from the hosted execution composition root.
use super::*;
use std::io::Read;

use reqwest::{
    Method, StatusCode, Url,
    blocking::{Client, Response},
    header,
};
use serde::de::DeserializeOwned;

#[derive(Deserialize)]
pub(super) struct GithubOidcResponse {
    pub(super) value: String,
}

#[derive(Deserialize)]
pub(super) struct ExchangeResponse {
    pub(super) access_token: String,
    pub(super) token_type: String,
    pub(super) expires_in: i64,
    pub(super) expires_at: String,
    pub(super) token_id: Uuid,
    pub(super) tenant_id: Uuid,
    pub(super) project_id: Uuid,
    pub(super) execution_id: Uuid,
    pub(super) execution_attempt: i32,
    pub(super) session_id: Uuid,
    pub(super) worker_id: Uuid,
    pub(super) repository_id: i64,
    pub(super) github_workflow_run_id: i64,
    pub(super) permissions: Vec<String>,
}

#[derive(Deserialize)]
pub(super) struct RefreshedTokenResponse {
    pub(super) access_token: String,
    pub(super) token_type: String,
    pub(super) expires_at: String,
    pub(super) token_id: Uuid,
    pub(super) session_id: Uuid,
}

pub(super) struct TokenState {
    pub(super) value: SecretString,
    pub(super) expires_at: SystemTime,
    pub(super) refresh_after: SystemTime,
    pub(super) token_id: Uuid,
    pub(super) session_id: Uuid,
}

#[derive(Clone)]
pub(super) struct HostedApiClient {
    pub(super) http: Client,
    pub(super) api_root: Url,
    pub(super) execution_id: Uuid,
    pub(super) project_id: Uuid,
    pub(super) repository_id: i64,
    pub(super) execution_attempt: i32,
    pub(super) github_workflow_run_id: i64,
    pub(super) auth: Arc<Mutex<TokenState>>,
    pub(super) refresh_lock: Arc<Mutex<()>>,
    pub(super) clock: Arc<dyn HostedClock>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct ProviderErrorDiagnostic {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub(super) error_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) parameter: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AiFailureClass {
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
    pub(super) const fn is_provider_failure(self) -> bool {
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
pub(super) enum AiBudgetDisposition {
    Restore,
    Consumed,
    Unknown,
}

#[derive(Debug)]
pub(super) struct HostedHttpError {
    pub(super) status: StatusCode,
    pub(super) path: String,
    pub(super) code: String,
    pub(super) request_id: Option<String>,
    pub(super) rustgrid_gateway_status: Option<Option<u16>>,
    pub(super) upstream_provider_status: Option<u16>,
    pub(super) failure_stage: Option<String>,
    pub(super) provider_contacted: Option<bool>,
    pub(super) call_budget_consumed: Option<bool>,
    pub(super) reservation_state: Option<String>,
    pub(super) reservation_reconciliation_state: Option<String>,
    pub(super) retryable: Option<bool>,
    pub(super) rustgrid_request_id: Option<String>,
    pub(super) transport_request_id: Option<String>,
    pub(super) provider_request_id: Option<String>,
    pub(super) provider_error: Option<ProviderErrorDiagnostic>,
    pub(super) provider_response_body: Option<Value>,
    pub(super) model_alias: Option<String>,
    pub(super) resolved_provider_model: Option<String>,
    pub(super) adapter_version: Option<String>,
    pub(super) payload_schema_version: Option<String>,
    pub(super) provider_attempts: Option<u64>,
    pub(super) actual_cost_micros: Option<u64>,
}

impl HostedHttpError {
    pub(super) fn invalidates_execution(&self) -> bool {
        self.status == StatusCode::UNAUTHORIZED
            || matches!(
                self.code.as_str(),
                "execution_token_invalid"
                    | "execution_token_scope_invalid"
                    | "execution_ai_access_revoked"
                    | "execution_cancelled"
                    | "execution_timed_out"
                    | "execution_lost"
                    | "execution_terminal_state"
                    | "execution_completion_preempted_by_cancellation"
            )
    }

    pub(super) fn effective_code(&self) -> &str {
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

    pub(super) fn failure_stage(&self) -> Option<&str> {
        if self.failure_class().is_provider_failure() {
            Some("provider_dispatch")
        } else {
            self.failure_stage.as_deref()
        }
    }

    pub(super) fn provider_contacted(&self) -> Option<bool> {
        self.provider_contacted
    }

    pub(super) fn call_budget_consumed(&self) -> Option<bool> {
        self.call_budget_consumed
    }

    pub(super) fn reservation_state(&self) -> Option<&str> {
        self.reservation_state
            .as_deref()
            .or(self.reservation_reconciliation_state.as_deref())
    }

    pub(super) fn reservation_reconciliation_state(&self) -> Option<&str> {
        self.reservation_reconciliation_state.as_deref()
    }

    pub(super) fn has_definite_provider_response(&self) -> bool {
        self.provider_contacted == Some(true) && self.upstream_provider_status.is_some()
    }

    pub(super) fn failure_class(&self) -> AiFailureClass {
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

    pub(super) fn rustgrid_gateway_status(&self) -> Option<Option<u16>> {
        if self.failure_class().is_provider_failure() {
            self.rustgrid_gateway_status
        } else {
            self.rustgrid_gateway_status
                .or(Some(Some(self.status.as_u16())))
        }
    }

    pub(super) fn terminal_message(&self) -> &'static str {
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

    pub(super) fn recommended_action(&self) -> &'static str {
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

    pub(super) fn budget_disposition(&self) -> AiBudgetDisposition {
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

    pub(super) fn retryable_gateway_transport_failure(&self) -> bool {
        !self.has_definite_provider_response()
            && self.failure_class() == AiFailureClass::Gateway
            && retryable_status(self.status)
    }

    pub(super) fn retryable_registration_failure(&self) -> bool {
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
    pub(super) fn from_exchange(
        http: Client,
        api_root: Url,
        execution_id: Uuid,
        exchange: ExchangeResponse,
        clock: Arc<dyn HostedClock>,
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
            .duration_since(clock.system_now())
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
            clock,
        })
    }

    pub(super) fn claim(&self) -> Result<Value> {
        self.send_json(
            Method::POST,
            &format!("executions/{}/claim", self.execution_id),
            Some(json!({"lease_seconds": EXECUTION_LEASE_SECONDS})),
            None,
            2,
        )
    }

    pub(super) fn manifest(&self) -> Result<HostedManifest> {
        self.send_json(
            Method::GET,
            &format!("executions/{}/manifest", self.execution_id),
            None,
            None,
            2,
        )
    }

    pub(super) fn heartbeat(&self) -> Result<()> {
        let _: Value = self.send_json(
            Method::POST,
            &format!("executions/{}/heartbeat", self.execution_id),
            Some(json!({"lease_seconds": EXECUTION_LEASE_SECONDS})),
            None,
            2,
        )?;
        Ok(())
    }

    pub(super) fn append_event(&self, event_type: &str, data: Value) -> Result<()> {
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

    pub(super) fn update_state(&self, state: &str) -> Result<()> {
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

    pub(super) fn github_token(&self, expected_repository: &str) -> Result<SecretString> {
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
            .duration_since(self.clock.system_now())
            .unwrap_or_default()
            < Duration::from_secs(30)
        {
            bail!("RustGrid returned an already-expired GitHub repository token");
        }
        SecretString::new(issued.token, "GitHub repository token")
    }

    #[cfg(test)]
    pub(super) fn ai_response(
        &self,
        body: Value,
        registration: &AiCallRegistration,
    ) -> Result<Value> {
        self.ai_response_until(body, registration, None)
    }

    pub(super) fn ai_response_until(
        &self,
        body: Value,
        registration: &AiCallRegistration,
        execution_deadline: Option<Instant>,
    ) -> Result<Value> {
        ai_request_timeout(self.clock.as_ref(), execution_deadline)?;
        self.ensure_fresh()?;
        let token = self.current_token()?;
        let path = format!("executions/{}/ai/responses", self.execution_id);
        let url = self
            .api_root
            .join(&path)
            .with_context(|| format!("invalid RustGrid API path {path}"))?;
        for attempt in 0..3 {
            let request_timeout = ai_request_timeout(self.clock.as_ref(), execution_deadline)?;
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
                        sleep_before_ai_retry(self.clock.as_ref(), execution_deadline, attempt)?;
                    } else {
                        return Err(error);
                    }
                }
                Err(_) if attempt < 2 => {
                    sleep_before_ai_retry(self.clock.as_ref(), execution_deadline, attempt)?;
                }
                Err(_) => bail!("RustGrid {path} transport failed"),
            }
        }
        unreachable!("bounded AI gateway transport loop always returns")
    }

    pub(super) fn telemetry(&self, batch: &TelemetryBatch) -> Result<()> {
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

    pub(super) fn complete(&self, completion: &CompletionRequest) -> Result<Value> {
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

    pub(super) fn ensure_fresh(&self) -> Result<()> {
        let refresh_required = {
            let state = self
                .auth
                .lock()
                .map_err(|_| anyhow!("execution-token lock is poisoned"))?;
            self.clock.system_now() >= state.refresh_after
        };
        if refresh_required {
            self.refresh_token()?;
        }
        Ok(())
    }

    pub(super) fn refresh_token(&self) -> Result<()> {
        let _refresh = self
            .refresh_lock
            .lock()
            .map_err(|_| anyhow!("execution-token refresh lock is poisoned"))?;
        let refresh_required = {
            let state = self
                .auth
                .lock()
                .map_err(|_| anyhow!("execution-token lock is poisoned"))?;
            self.clock.system_now() >= state.refresh_after
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
                .duration_since(self.clock.system_now())
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

    pub(super) fn current_token(&self) -> Result<SecretString> {
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

    pub(super) fn session_id(&self) -> Result<Uuid> {
        Ok(self
            .auth
            .lock()
            .map_err(|_| anyhow!("execution-token lock is poisoned"))?
            .session_id)
    }

    pub(super) fn send_json<T: DeserializeOwned>(
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

    pub(super) fn send_with_token<T: DeserializeOwned>(
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
                    self.clock.sleep(retry_delay(attempt));
                }
                Ok(response) => return decode_response(response, path),
                Err(_) if attempt + 1 < attempts => self.clock.sleep(retry_delay(attempt)),
                Err(_) => bail!("RustGrid {path} transport failed"),
            }
        }
        unreachable!("bounded HTTP loop always returns")
    }
}

pub(super) fn completion_idempotency_key(
    execution_id: Uuid,
    completion: &CompletionRequest,
) -> Result<Uuid> {
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
pub(super) struct AiCallRegistration {
    pub(super) semantic_call_id: Uuid,
    pub(super) request_id: Uuid,
    pub(super) call_index: usize,
    pub(super) phase: ExecutionPhase,
    pub(super) registration_attempt: usize,
}

pub(super) fn ai_call_registration(
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

impl HostedManifest {
    pub(super) fn budget_audit(&self) -> Result<BudgetAudit> {
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

    pub(super) fn validate(
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

    pub(super) fn repo_config(&self) -> Result<RepoConfig> {
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
    pub(super) fn validate(&self) -> Result<()> {
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
            || self
                .mutation_replacement_max_bytes
                .is_some_and(|bytes| bytes == 0 || bytes > MAX_MODEL_FILE_BYTES)
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

    pub(super) fn child_environment_allowlist(&self) -> Vec<String> {
        self.codex
            .environment_allowlist
            .iter()
            .filter(|name| safe_child_environment_name(name) && name.as_str() != "HOME")
            .cloned()
            .collect()
    }
}

pub(super) fn hosted_http_client() -> Result<Client> {
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

pub(super) fn decode_response<T: DeserializeOwned>(response: Response, path: &str) -> Result<T> {
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

pub(super) fn hosted_error_field<'a>(error: &'a Value, field: &str) -> Option<&'a Value> {
    error.get(field).or_else(|| {
        ["details", "diagnostics", "error"]
            .into_iter()
            .find_map(|container| error.get(container).and_then(|value| value.get(field)))
    })
}

pub(super) fn optional_hosted_http_status(
    error: Option<&Value>,
    field: &str,
) -> Option<Option<u16>> {
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

pub(super) fn safe_hosted_error_identifier(error: Option<&Value>, field: &str) -> Option<String> {
    error
        .and_then(|value| hosted_error_field(value, field))
        .and_then(Value::as_str)
        .filter(|value| safe_identifier(value, 100))
        .map(str::to_owned)
}

pub(super) fn safe_hosted_error_text(value: Option<&Value>, maximum: usize) -> Option<String> {
    let value = value.and_then(Value::as_str)?;
    let sanitized = value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .collect::<String>();
    (!sanitized.is_empty()).then(|| truncate_text(&sanitized, maximum))
}

pub(super) fn safe_provider_error(error: Option<&Value>) -> Option<ProviderErrorDiagnostic> {
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

pub(super) fn safe_provider_response_body(error: Option<&Value>) -> Option<Value> {
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

pub(super) fn decode_success<T: DeserializeOwned>(response: Response, label: &str) -> Result<T> {
    let bytes = Zeroizing::new(
        read_bounded_response(response, MAX_HTTP_RESPONSE_BYTES)
            .with_context(|| format!("could not read {label}"))?,
    );
    serde_json::from_slice(&bytes).with_context(|| format!("{label} is malformed"))
}

pub(super) fn read_bounded_response(mut response: Response, maximum: usize) -> Result<Vec<u8>> {
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
