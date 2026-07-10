use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{
    Method,
    blocking::{Client, RequestBuilder},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::AppContext;

// Agent endpoints are intentionally centralized here while the authenticated RustGrid
// agent API is evolving. The configured base URL should include `/api/v1`.
const WORKERS: &str = "agent-workers";
const RUNS: &str = "agent-runs";

#[derive(Clone)]
pub struct RustGridClient {
    http: Client,
    base_url: String,
    api_key: String,
    project_field: &'static str,
    project_value: String,
    session_id: Uuid,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Worker {
    #[serde(alias = "worker_id")]
    pub id: String,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Ticket {
    #[serde(alias = "ticket_id")]
    pub id: String,
    #[serde(alias = "ticket_key", alias = "key")]
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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Comment {
    #[serde(default, alias = "body", alias = "text")]
    pub content: String,
    #[serde(default, alias = "author_name")]
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
    #[serde(alias = "run_id")]
    pub id: String,
}

#[derive(Debug, Serialize)]
pub struct StepPayload<'a> {
    pub name: &'a str,
    pub status: &'a str,
    pub message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl RustGridClient {
    pub fn new(context: &AppContext) -> Result<Self> {
        let (project_field, project_value) = context.project_value();
        Ok(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("rustgrid-agent/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base_url: context.api_url.trim_end_matches('/').to_owned(),
            api_key: context.require_api_key()?.to_owned(),
            project_field,
            project_value: project_value.to_owned(),
            session_id: Uuid::new_v4(),
        })
    }

    pub fn register(&self) -> Result<Worker> {
        let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "local-worker".into());
        let mut payload = json!({
            "name": hostname,
            "capabilities": ["codex", "git", "github_pull_requests"],
            "version": env!("CARGO_PKG_VERSION")
        });
        payload[self.project_field] = json!(self.project_value);
        self.send_json(
            Method::POST,
            &format!("{WORKERS}/register"),
            Some(payload),
            Some(&format!("register-worker-{}", self.project_value)),
            &["worker", "data"],
        )
    }

    pub fn heartbeat(&self, worker_id: &str) -> Result<()> {
        self.send_empty(
            Method::POST,
            &format!("{WORKERS}/{worker_id}/heartbeat"),
            Some(json!({})),
            None,
        )
    }

    pub fn fetch_ticket(&self, ticket_id: &str) -> Result<Ticket> {
        self.send_json(
            Method::GET,
            &format!("tickets/{ticket_id}?include=comments,custom_fields,quality_gate_failures"),
            None,
            None,
            &["ticket", "data"],
        )
    }

    pub fn next_ticket(&self, worker_id: &str) -> Result<Option<Ticket>> {
        let path = format!(
            "agent-tickets/next?{}={}&worker_id={}",
            self.project_field,
            url_encode(&self.project_value),
            url_encode(worker_id)
        );
        let value = self.send_value(Method::GET, &path, None, None)?;
        if value.is_null() || value.get("ticket").is_some_and(Value::is_null) {
            return Ok(None);
        }
        deserialize_envelope(value, &["ticket", "data"]).map(Some)
    }

    pub fn claim_ticket(&self, ticket_id: &str, worker_id: &str) -> Result<()> {
        self.send_empty(
            Method::POST,
            &format!("tickets/{ticket_id}/claim"),
            Some(json!({"worker_id": worker_id})),
            Some(&format!("claim-{ticket_id}-{worker_id}")),
        )
    }

    pub fn create_run(&self, ticket: &Ticket, worker_id: &str) -> Result<AgentRun> {
        let mut payload =
            json!({"ticket_id": ticket.id, "worker_id": worker_id, "status": "running"});
        payload[self.project_field] = json!(self.project_value);
        self.send_json(
            Method::POST,
            RUNS,
            Some(payload),
            Some(&format!(
                "run-{}-{worker_id}-{}",
                ticket.id, self.session_id
            )),
            &["agent_run", "run", "data"],
        )
    }

    pub fn append_step(&self, run_id: &str, step: &StepPayload<'_>) -> Result<()> {
        let value = serde_json::to_value(step)?;
        self.send_empty(
            Method::POST,
            &format!("{RUNS}/{run_id}/steps"),
            Some(value),
            None,
        )
    }

    pub fn update_run(&self, run_id: &str, status: &str, message: Option<&str>) -> Result<()> {
        self.send_empty(
            Method::PATCH,
            &format!("{RUNS}/{run_id}"),
            Some(json!({"status": status, "message": message})),
            None,
        )
    }

    pub fn report_quality_gate(
        &self,
        run_id: &str,
        command: &str,
        passed: bool,
        output: &str,
    ) -> Result<()> {
        self.send_empty(Method::POST, &format!("{RUNS}/{run_id}/quality-gates"), Some(json!({
            "command": command, "status": if passed { "passed" } else { "failed" }, "output": truncate(output, 16_000)
        })), None)
    }

    pub fn attach_pr(&self, run_id: &str, url: &str, number: u64) -> Result<()> {
        self.send_empty(
            Method::POST,
            &format!("{RUNS}/{run_id}/attachments"),
            Some(json!({
                "type": "github_pull_request", "url": url, "pull_request_number": number
            })),
            None,
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
    ) -> Result<Value> {
        let mut request = self.request(method, path);
        if let Some(body) = body {
            request = request.json(&body);
        }
        if let Some(key) = idempotency {
            request = request.header("Idempotency-Key", key);
        }
        let response = request
            .send()
            .with_context(|| format!("RustGrid request failed: {path}"))?;
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let text = response
            .text()
            .context("could not read RustGrid response")?;
        if !status.is_success() {
            bail!(
                "RustGrid {path} returned {status}{}: {}",
                request_id
                    .map(|id| format!(" (request {id})"))
                    .unwrap_or_default(),
                truncate(&text, 2_000)
            );
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text)
            .with_context(|| format!("RustGrid {path} returned invalid JSON"))
    }

    fn send_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency: Option<&str>,
        keys: &[&str],
    ) -> Result<T> {
        deserialize_envelope(self.send_value(method, path, body, idempotency)?, keys)
    }

    fn send_empty(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        idempotency: Option<&str>,
    ) -> Result<()> {
        self.send_value(method, path, body, idempotency).map(|_| ())
    }
}

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
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
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
