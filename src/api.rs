use std::{
    io::{BufRead, BufReader},
    time::Duration,
};

use anyhow::{Context, Result};
use reqwest::{
    Method, StatusCode,
    blocking::{Client, RequestBuilder},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::AppContext;
use crate::{
    lifecycle::{AgentRunStatus, LifecycleEvent, StepStatus, TicketStatus, WorkerStatus},
    manifest::ExecutionManifest,
};

macro_rules! remote_string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub enum $name {
            $($variant),+,
            Unknown(String),
        }

        impl $name {
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $value),+,
                    Self::Unknown(value) => value,
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Ok(match String::deserialize(deserializer)?.as_str() {
                    $($value => Self::$variant),+,
                    value => Self::Unknown(value.to_owned()),
                })
            }
        }
    };
}

remote_string_enum!(RemoteWorkerStatus {
    Online => "online",
    Busy => "busy",
    Offline => "offline",
});

remote_string_enum!(QueueEventKind {
    WorkAvailable => "work_available",
    TicketAvailable => "ticket.available",
    RunClaimed => "run.claimed",
    RunCancelled => "run.cancelled",
});

remote_string_enum!(QualityGateStatus {
    Passed => "passed",
    Failed => "failed",
});

const WORKERS: &str = "agent-workers";
const RUNS: &str = "agent-runs";
const STANDARD_RETRY_POLICY: RetryPolicy = RetryPolicy::new(3, 250, 30_000);
const GITHUB_TOKEN_RETRY_POLICY: RetryPolicy = RetryPolicy::new(8, 1_000, 30_000);

#[derive(Clone, Copy)]
struct RetryPolicy {
    max_attempts: u32,
    base_delay_ms: u64,
    max_delay_ms: u64,
}

impl RetryPolicy {
    const fn new(max_attempts: u32, base_delay_ms: u64, max_delay_ms: u64) -> Self {
        Self {
            max_attempts,
            base_delay_ms,
            max_delay_ms,
        }
    }
}

#[derive(Clone)]
pub struct RustGridClient {
    http: Client,
    base_url: String,
    api_key: String,
    session_id: Uuid,
    max_concurrency: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Worker {
    pub id: String,
    pub status: RemoteWorkerStatus,
    #[serde(default = "default_worker_capacity")]
    pub max_concurrency: usize,
    #[serde(default)]
    pub active_runs: usize,
    #[serde(default)]
    pub available_slots: usize,
}

fn default_worker_capacity() -> usize {
    1
}

#[derive(Clone, Debug, Deserialize)]
pub struct Ticket {
    pub id: String,
    #[serde(alias = "ticket_key", alias = "key", alias = "project_key")]
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub comments: Vec<Comment>,
    #[serde(default, alias = "fields")]
    pub custom_fields: Value,
    #[serde(default)]
    pub previous_quality_gate_failures: Vec<QualityGateFailure>,
    #[serde(skip)]
    pub row_version: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Comment {
    #[serde(default, alias = "body", alias = "text")]
    pub content: String,
    #[serde(default, alias = "author_name", alias = "author_id")]
    pub author: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QualityGateFailure {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default, alias = "output", alias = "error")]
    pub message: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgentRun {
    pub id: String,
    pub ticket_id: String,
    pub row_version: i64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GitHubAccessToken {
    pub token: String,
    pub expires_at: String,
    pub repository: String,
    #[serde(default)]
    pub permissions: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgentRunEvent {
    pub sequence: u64,
    pub data: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgentRunEvents {
    pub items: Vec<AgentRunEvent>,
    pub next_sequence: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgentQueueEvent {
    pub sequence: u64,
    pub event_type: QueueEventKind,
    #[serde(default)]
    pub ticket_id: Option<String>,
    #[serde(default)]
    pub worker_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgentQueueEvents {
    pub worker: Worker,
    pub items: Vec<AgentQueueEvent>,
    pub next_sequence: u64,
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    items: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct AgentRunPage {
    items: Vec<AgentRun>,
}

#[derive(Debug, Deserialize)]
struct QualityGateRecord {
    status: QualityGateStatus,
    #[serde(default)]
    checks: Value,
    #[serde(default)]
    summary: Option<String>,
}

impl RustGridClient {
    pub fn new(context: &AppContext) -> Result<Self> {
        Ok(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("rustgrid-agent/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base_url: context.api_url.trim_end_matches('/').to_owned(),
            api_key: context.require_api_key()?.to_owned(),
            session_id: Uuid::new_v4(),
            max_concurrency: context.config.max_concurrency,
        })
    }

    pub fn heartbeat(&self, worker_id: &str) -> Result<()> {
        self.heartbeat_with_status(worker_id, WorkerStatus::Online)
    }

    pub fn heartbeat_with_status(&self, worker_id: &str, status: WorkerStatus) -> Result<()> {
        self.send_empty(
            Method::POST,
            &format!("{WORKERS}/{worker_id}/heartbeat"),
            Some(json!({"status": status.as_str(), "max_concurrency": self.max_concurrency})),
            None,
        )
    }

    pub fn extend_lease(
        &self,
        run_id: &str,
        worker_id: &str,
        lease_seconds: u64,
    ) -> Result<AgentRun> {
        self.send_json(
            Method::POST,
            &format!("{RUNS}/{run_id}/lease"),
            Some(json!({
                "worker_id": worker_id,
                "lease_seconds": lease_seconds
            })),
            Some(&format!("lease-{run_id}-{}", Uuid::new_v4())),
            &[],
            None,
        )
    }

    pub fn execution_manifest(&self, run_id: &str) -> Result<ExecutionManifest> {
        self.send_json(
            Method::GET,
            &format!("{RUNS}/{run_id}/manifest"),
            None,
            None,
            &[],
            None,
        )
    }

    pub fn queue_events(&self, worker_id: &str, after_sequence: u64) -> Result<AgentQueueEvents> {
        self.send_json(
            Method::GET,
            &format!("{WORKERS}/{worker_id}/queue?after_sequence={after_sequence}&limit=500"),
            None,
            None,
            &[],
            None,
        )
    }

    pub fn wait_for_queue_event(
        &self,
        worker_id: &str,
        after_sequence: u64,
        timeout: Duration,
    ) -> Result<Option<u64>> {
        let path =
            format!("{WORKERS}/{worker_id}/queue/stream?after_sequence={after_sequence}&limit=100");
        let response = match self
            .request(Method::GET, &path)
            .header("Accept", "text/event-stream")
            .header("Last-Event-ID", after_sequence)
            .timeout(timeout)
            .send()
        {
            Ok(response) => response,
            Err(error) if error.is_timeout() => return Ok(None),
            Err(error) => return Err(error).context("agent queue stream failed"),
        };
        if !response.status().is_success() {
            anyhow::bail!("agent queue stream returned {}", response.status());
        }
        for line in BufReader::new(response).lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) if is_idle_stream_timeout(&error) => return Ok(None),
                Err(error) => return Err(error).context("failed to read agent queue stream"),
            };
            if let Some(value) = line.strip_prefix("id:") {
                return value
                    .trim()
                    .parse::<u64>()
                    .map(Some)
                    .context("agent queue stream returned an invalid sequence");
            }
        }
        Ok(None)
    }

    pub fn issue_github_token(&self, run_id: &str) -> Result<GitHubAccessToken> {
        let path = format!("{RUNS}/{run_id}/github-token");
        let idempotency_key = format!("github-token-{run_id}-{}", Uuid::new_v4());
        let token: GitHubAccessToken = deserialize_envelope(
            self.send_value_with_policy(
                Method::POST,
                &path,
                None,
                Some(&idempotency_key),
                None,
                GITHUB_TOKEN_RETRY_POLICY,
            )?,
            &[],
        )?;
        if token.token.trim().is_empty()
            || token.expires_at.trim().is_empty()
            || token.repository.trim().is_empty()
        {
            anyhow::bail!("RustGrid issued an invalid GitHub token response");
        }
        Ok(token)
    }

    pub fn publish_run_event(
        &self,
        run_id: &str,
        event_kind: &str,
        event: &LifecycleEvent,
    ) -> Result<AgentRunEvent> {
        self.send_json(
            Method::POST,
            &format!("{RUNS}/{run_id}/events"),
            Some(json!({
                "event_type": event_kind,
                "data": event.metadata()
            })),
            Some(&format!("progress-{run_id}-{}", event.sequence)),
            &[],
            None,
        )
    }

    pub fn progress_events(&self, run_id: &str, after_sequence: u64) -> Result<AgentRunEvents> {
        self.send_json(
            Method::GET,
            &format!("{RUNS}/{run_id}/events?after_sequence={after_sequence}&limit=500"),
            None,
            None,
            &[],
            None,
        )
    }

    pub fn find_event_by_client_sequence(
        &self,
        run_id: &str,
        mut after_sequence: u64,
        client_sequence: u64,
    ) -> Result<Option<u64>> {
        for _ in 0..20 {
            let page = self.progress_events(run_id, after_sequence)?;
            if let Some(event) = page.items.iter().find(|item| {
                item.data
                    .get("sequence")
                    .and_then(serde_json::Value::as_u64)
                    == Some(client_sequence)
            }) {
                return Ok(Some(event.sequence));
            }
            if page.items.is_empty() || page.next_sequence <= after_sequence {
                return Ok(None);
            }
            after_sequence = page.next_sequence;
        }
        anyhow::bail!("agent event replay exceeded 10,000 events while reconciling cursor")
    }

    pub fn fetch_ticket(&self, ticket_id: &str) -> Result<Ticket> {
        let (ticket_value, etag) = self.send_value_with_etag(
            Method::GET,
            &format!("tickets/{ticket_id}"),
            None,
            None,
            None,
        )?;
        let mut ticket: Ticket = deserialize_envelope(ticket_value, &[])?;
        ticket.row_version = parse_etag_row_version(
            etag.as_deref()
                .context("ticket response did not include an ETag")?,
            "tickets",
            &ticket.id,
        )?;
        ticket.comments = self.ticket_pages::<Comment>(ticket_id, "comments")?;
        ticket.previous_quality_gate_failures = self
            .ticket_pages::<QualityGateRecord>(ticket_id, "quality-gate-results")?
            .into_iter()
            .filter(|gate| gate.status == QualityGateStatus::Failed)
            .map(|gate| QualityGateFailure {
                command: None,
                message: gate.summary.unwrap_or_else(|| gate.checks.to_string()),
            })
            .collect();
        Ok(ticket)
    }

    fn ticket_pages<T: for<'de> Deserialize<'de>>(
        &self,
        ticket_id: &str,
        resource: &str,
    ) -> Result<Vec<T>> {
        let mut items = Vec::new();
        for page_number in 1..=100 {
            let mut page: Page<T> = self.send_json(
                Method::GET,
                &format!("tickets/{ticket_id}/{resource}?page={page_number}&size=100"),
                None,
                None,
                &[],
                None,
            )?;
            let page_len = page.items.len();
            items.append(&mut page.items);
            if page_len < 100 {
                return Ok(items);
            }
        }
        anyhow::bail!("ticket {resource} pagination exceeded 10,000 records")
    }

    pub fn create_comment(&self, ticket_id: &str, body: &str, idempotency_key: &str) -> Result<()> {
        self.send_empty(
            Method::POST,
            &format!("tickets/{ticket_id}/comments"),
            Some(json!({"body": truncate(body, 5000)})),
            Some(idempotency_key),
        )
    }

    pub fn update_ticket_status(
        &self,
        ticket_id: &str,
        row_version: i64,
        status: TicketStatus,
        idempotency_key: &str,
    ) -> Result<i64> {
        let (_, etag) = self.send_value_with_etag(
            Method::PATCH,
            &format!("tickets/{ticket_id}"),
            Some(json!({"status": status.as_str()})),
            Some(idempotency_key),
            Some(&format!("\"tickets:{ticket_id}:{row_version}\"")),
        )?;
        parse_etag_row_version(
            etag.as_deref()
                .context("ticket update did not include an ETag")?,
            "tickets",
            ticket_id,
        )
    }

    pub fn resolve_project_id(&self, context: &AppContext) -> Result<String> {
        if let Some(id) = context.config.project_id.clone() {
            return Ok(id);
        }
        #[derive(Deserialize)]
        struct Project {
            id: String,
        }
        let key = context.config.project_key.as_deref().expect("validated");
        let project: Project = self.send_json(
            Method::GET,
            &format!("projects/key/{}", url_encode(key)),
            None,
            None,
            &[],
            None,
        )?;
        Ok(project.id)
    }

    pub fn claim_ticket(&self, ticket_id: &str, worker_id: &str, prompt: &str) -> Result<AgentRun> {
        self.send_json(
            Method::POST,
            &format!("tickets/{ticket_id}/agent-runs/claim"),
            Some(json!({
                "worker_id": worker_id,
                "input_prompt": prompt,
                "metadata": {"runner": "rustgrid-agent"},
                "lease_seconds": 3600
            })),
            Some(&format!(
                "claim-{ticket_id}-{worker_id}-{}",
                self.session_id
            )),
            &[],
            None,
        )
    }

    pub fn active_runs(&self, project_id: &str, worker_id: &str) -> Result<Vec<AgentRun>> {
        let page: AgentRunPage = self.send_json(
            Method::GET,
            &format!(
                "{RUNS}?project_id={}&status=running&worker_id={}&page=1&size=100",
                url_encode(project_id),
                url_encode(worker_id)
            ),
            None,
            None,
            &[],
            None,
        )?;
        Ok(page.items)
    }

    pub fn append_step(
        &self,
        run_id: &str,
        name: &str,
        status: StepStatus,
        message: &str,
        metadata: Option<Value>,
    ) -> Result<()> {
        let status = status.as_str();
        let step_key = format!("{}-{status}", name.replace('_', "-"));
        self.send_empty(
            Method::POST,
            &format!("{RUNS}/{run_id}/steps"),
            Some(json!({
                "step_key": step_key,
                "title": truncate(message, 300),
                "status": status,
                "summary": truncate(message, 5000),
                "metadata": metadata.unwrap_or_else(|| json!({}))
            })),
            Some(&format!("step-{run_id}-{step_key}")),
        )
    }

    pub fn update_run(
        &self,
        run_id: &str,
        row_version: i64,
        status: AgentRunStatus,
        message: Option<&str>,
    ) -> Result<AgentRun> {
        let status = status.as_str();
        let message = message.map(|value| truncate(value, 20_000));
        let mut body = json!({"status": status});
        if status == "failed" {
            body["error_message"] = json!(message.as_deref());
        } else {
            body["output_summary"] = json!(message.as_deref());
        }
        self.send_json(
            Method::PATCH,
            &format!("{RUNS}/{run_id}"),
            Some(body),
            Some(&format!("run-status-{run_id}-{status}")),
            &[],
            Some(&format!("\"agent-runs:{run_id}:{row_version}\"")),
        )
    }

    pub fn report_quality_gate(
        &self,
        ticket_id: &str,
        run_id: &str,
        gate_id: &str,
        command: &str,
        passed: bool,
        output: &str,
    ) -> Result<()> {
        self.send_empty(
            Method::POST,
            &format!("tickets/{ticket_id}/quality-gate-results"),
            Some(json!({
                "run_id": run_id,
                "status": if passed { "passed" } else { "failed" },
                "checks": [{"id": gate_id, "name": command, "status": if passed { "passed" } else { "failed" }, "summary": truncate(output, 16000)}],
                "summary": truncate(if passed { "Local quality gate passed" } else { output }, 5000)
            })),
            Some(&format!("gate-{run_id}-{gate_id}")),
        )
    }

    pub fn attach_pr(&self, ticket_id: &str, run_id: &str, url: &str, number: u64) -> Result<()> {
        self.send_empty(
            Method::POST,
            &format!("tickets/{ticket_id}/external-links"),
            Some(json!({
                "kind": "github_pr",
                "label": format!("GitHub PR #{number}"),
                "url": url,
                "external_id": number.to_string(),
                "metadata": {"agent_run_id": run_id}
            })),
            Some(&format!("pr-link-{run_id}")),
        )
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}/{}", self.base_url, path))
            .bearer_auth(&self.api_key)
            .header("Accept", "application/json")
    }

    fn send_value(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency: Option<&str>,
        if_match: Option<&str>,
    ) -> std::result::Result<Value, HttpFailure> {
        self.send_value_with_etag_policy(
            method,
            path,
            body,
            idempotency,
            if_match,
            STANDARD_RETRY_POLICY,
        )
        .map(|(value, _)| value)
    }

    fn send_value_with_policy(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency: Option<&str>,
        if_match: Option<&str>,
        retry_policy: RetryPolicy,
    ) -> std::result::Result<Value, HttpFailure> {
        self.send_value_with_etag_policy(method, path, body, idempotency, if_match, retry_policy)
            .map(|(value, _)| value)
    }

    fn send_value_with_etag(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency: Option<&str>,
        if_match: Option<&str>,
    ) -> std::result::Result<(Value, Option<String>), HttpFailure> {
        self.send_value_with_etag_policy(
            method,
            path,
            body,
            idempotency,
            if_match,
            STANDARD_RETRY_POLICY,
        )
    }

    fn send_value_with_etag_policy(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency: Option<&str>,
        if_match: Option<&str>,
        retry_policy: RetryPolicy,
    ) -> std::result::Result<(Value, Option<String>), HttpFailure> {
        let retry_safe = method == Method::GET || idempotency.is_some();
        for attempt in 0..retry_policy.max_attempts {
            let mut request = self.request(method.clone(), path);
            if let Some(body) = body.as_ref() {
                request = request.json(body);
            }
            if let Some(key) = idempotency {
                request = request.header("Idempotency-Key", key);
            }
            if let Some(etag) = if_match {
                request = request.header("If-Match", etag);
            }
            let response = match request.send() {
                Ok(response) => response,
                Err(error) if retry_safe && attempt + 1 < retry_policy.max_attempts => {
                    let delay = retry_delay(attempt, self.session_id, retry_policy);
                    eprintln!(
                        "[warning] RustGrid {path} transport error; retrying attempt {} of {} in {}ms: {error}",
                        attempt + 2,
                        retry_policy.max_attempts,
                        delay.as_millis()
                    );
                    std::thread::sleep(delay);
                    continue;
                }
                Err(error) => return Err(HttpFailure::transport(path, error)),
            };
            let status = response.status();
            let request_id = response
                .headers()
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let etag = response
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let text = response
                .text()
                .map_err(|error| HttpFailure::transport(path, error))?;
            if !status.is_success() {
                if retry_safe
                    && attempt + 1 < retry_policy.max_attempts
                    && (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                {
                    let delay = if let Some(seconds) = retry_after {
                        Duration::from_secs(seconds)
                            .min(Duration::from_millis(retry_policy.max_delay_ms))
                    } else {
                        retry_delay(attempt, self.session_id, retry_policy)
                    };
                    eprintln!(
                        "[warning] RustGrid {path} returned {status}; retrying attempt {} of {} in {}ms",
                        attempt + 2,
                        retry_policy.max_attempts,
                        delay.as_millis()
                    );
                    std::thread::sleep(delay);
                    continue;
                }
                return Err(HttpFailure {
                    status,
                    path: path.to_owned(),
                    request_id,
                    body: truncate(&text, 2000),
                });
            }
            if text.trim().is_empty() {
                return Ok((Value::Null, etag));
            }
            let value = serde_json::from_str(&text).map_err(|error| HttpFailure {
                status,
                path: path.to_owned(),
                request_id,
                body: format!("invalid JSON response: {error}"),
            })?;
            return Ok((value, etag));
        }
        unreachable!("retry loop always returns")
    }

    fn send_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency: Option<&str>,
        keys: &[&str],
        if_match: Option<&str>,
    ) -> Result<T> {
        deserialize_envelope(
            self.send_value(method, path, body, idempotency, if_match)?,
            keys,
        )
    }

    fn send_empty(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency: Option<&str>,
    ) -> Result<()> {
        self.send_value(method, path, body, idempotency, None)
            .map(|_| ())
            .map_err(Into::into)
    }
}

pub fn is_conflict(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<HttpFailure>()
        .is_some_and(|failure| failure.status == StatusCode::CONFLICT)
}

fn retry_delay(attempt: u32, session_id: Uuid, policy: RetryPolicy) -> Duration {
    let backoff_ms = policy
        .base_delay_ms
        .saturating_mul(1u64 << attempt.min(6))
        .min(policy.max_delay_ms);
    let jitter_ms = (session_id.as_u128() % 101) as u64;
    Duration::from_millis(backoff_ms.saturating_add(jitter_ms))
}

pub fn is_lease_lost(error: &anyhow::Error) -> bool {
    error.downcast_ref::<HttpFailure>().is_some_and(|failure| {
        let detail = failure.body.to_ascii_lowercase();
        let identifies_lease_loss = detail.contains("lease")
            || detail.contains("ownership")
            || detail.contains("assigned worker");
        (failure.status == StatusCode::NOT_FOUND && failure.path.ends_with("/lease"))
            || (failure.status == StatusCode::CONFLICT
                && (failure.path.ends_with("/lease")
                    || ((failure.path.ends_with("/events")
                        || failure.path.ends_with("/manifest")
                        || failure.path.ends_with("/github-token"))
                        && identifies_lease_loss)))
    })
}

fn is_idle_stream_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) || error.to_string().to_ascii_lowercase().contains("timed out")
}

#[derive(Debug)]
struct HttpFailure {
    status: StatusCode,
    path: String,
    request_id: Option<String>,
    body: String,
}

impl HttpFailure {
    fn transport(path: &str, error: reqwest::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            path: path.to_owned(),
            request_id: None,
            body: error.to_string(),
        }
    }
}

impl std::fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RustGrid {} returned {}{}: {}",
            self.path,
            self.status,
            self.request_id
                .as_ref()
                .map(|id| format!(" (request {id})"))
                .unwrap_or_default(),
            self.body
        )
    }
}

impl std::error::Error for HttpFailure {}

fn deserialize_envelope<T: DeserializeOwned>(mut value: Value, keys: &[&str]) -> Result<T> {
    for key in keys {
        if let Some(inner) = value.get(*key).cloned() {
            value = inner;
            break;
        }
    }
    serde_json::from_value(value).context("RustGrid response did not match the expected schema")
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let suffix = "…";
    if max < suffix.len() {
        return String::new();
    }
    let mut end = max - suffix.len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &value[..end])
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn parse_etag_row_version(etag: &str, prefix: &str, id: &str) -> Result<i64> {
    let value = etag.trim().trim_matches('"');
    let expected_prefix = format!("{prefix}:{id}:");
    let version = value
        .strip_prefix(&expected_prefix)
        .with_context(|| format!("unexpected ETag {etag}"))?;
    version
        .parse::<i64>()
        .with_context(|| format!("invalid row version in ETag {etag}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ticket_etag_versions() {
        assert_eq!(
            parse_etag_row_version("\"tickets:abc:7\"", "tickets", "abc").unwrap(),
            7
        );
        assert!(parse_etag_row_version("\"agent-runs:abc:7\"", "tickets", "abc").is_err());
    }

    #[test]
    fn classifies_cursor_conflicts_and_lost_leases() {
        let conflict = anyhow::Error::new(HttpFailure {
            status: StatusCode::CONFLICT,
            path: "agent-runs/run/lease".into(),
            request_id: None,
            body: "run lease ownership was lost".into(),
        });
        assert!(is_conflict(&conflict));
        assert!(is_lease_lost(&conflict));

        let ambiguous_manifest = anyhow::Error::new(HttpFailure {
            status: StatusCode::CONFLICT,
            path: "agent-runs/run/manifest".into(),
            request_id: None,
            body: "multiple repositories".into(),
        });
        assert!(!is_lease_lost(&ambiguous_manifest));

        let expired_manifest = anyhow::Error::new(HttpFailure {
            status: StatusCode::CONFLICT,
            path: "agent-runs/run/manifest".into(),
            request_id: None,
            body: "agent run lease is not active for this worker".into(),
        });
        assert!(is_lease_lost(&expired_manifest));

        let github_permissions = anyhow::Error::new(HttpFailure {
            status: StatusCode::CONFLICT,
            path: "agent-runs/run/github-token".into(),
            request_id: None,
            body: "GitHub App installation requires contents:write".into(),
        });
        assert!(!is_lease_lost(&github_permissions));

        let transient = anyhow::Error::new(HttpFailure {
            status: StatusCode::SERVICE_UNAVAILABLE,
            path: "agent-runs/run/lease".into(),
            request_id: None,
            body: "retry".into(),
        });
        assert!(!is_conflict(&transient));
        assert!(!is_lease_lost(&transient));
    }

    #[test]
    fn parses_progress_replay_cursor() {
        let events: AgentRunEvents = serde_json::from_value(json!({
            "items": [{"sequence": 7, "data": {"sequence": 4}}],
            "next_sequence": 7
        }))
        .unwrap();
        assert_eq!(events.next_sequence, 7);
    }

    #[test]
    fn truncation_includes_the_suffix_in_the_limit() {
        assert_eq!(truncate("abcdef", 5), "ab…");
        assert!(truncate("éééé", 7).len() <= 7);
        assert_eq!(truncate("abc", 2), "");
    }

    #[test]
    fn github_token_retries_use_bounded_production_backoff() {
        let session = Uuid::nil();
        assert_eq!(
            retry_delay(0, session, GITHUB_TOKEN_RETRY_POLICY),
            Duration::from_secs(1)
        );
        assert_eq!(
            retry_delay(6, session, GITHUB_TOKEN_RETRY_POLICY),
            Duration::from_secs(30)
        );
        assert_eq!(GITHUB_TOKEN_RETRY_POLICY.max_attempts, 8);
    }

    #[test]
    fn idle_stream_timeouts_are_not_reported_as_queue_failures() {
        assert!(is_idle_stream_timeout(&std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "operation timed out",
        )));
        assert!(!is_idle_stream_timeout(&std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid event stream",
        )));
    }
}
