use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{
    StatusCode,
    blocking::{Client, RequestBuilder, Response},
};
use serde::{Deserialize, Deserializer};
use serde_json::json;

use crate::config::RepoConfig;

pub struct GitHubClient {
    http: Client,
    token: String,
    api_base_url: String,
}

#[derive(Debug, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub html_url: String,
}

#[derive(Debug, Deserialize)]
pub struct CheckRun {
    pub id: u64,
    pub name: String,
    pub status: CheckStatus,
    pub conclusion: Option<CheckConclusion>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub name: String,
    pub path: String,
    pub status: CheckStatus,
    pub conclusion: Option<CheckConclusion>,
    #[serde(default)]
    pub run_attempt: u64,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum CheckStatus {
    Queued,
    InProgress,
    Completed,
    Unknown(String),
}

impl CheckStatus {
    pub const fn is_completed(&self) -> bool {
        matches!(self, Self::Completed)
    }
}

impl<'de> Deserialize<'de> for CheckStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "queued" => Self::Queued,
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            value => Self::Unknown(value.to_owned()),
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum CheckConclusion {
    Success,
    Failure,
    Cancelled,
    Skipped,
    TimedOut,
    Unknown(String),
}

impl CheckConclusion {
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::TimedOut => "timed_out",
            Self::Unknown(value) => value,
        }
    }
}

impl<'de> Deserialize<'de> for CheckConclusion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "success" => Self::Success,
            "failure" => Self::Failure,
            "cancelled" => Self::Cancelled,
            "skipped" => Self::Skipped,
            "timed_out" => Self::TimedOut,
            value => Self::Unknown(value.to_owned()),
        })
    }
}

#[derive(Debug, Deserialize)]
struct CheckRunsResponse {
    check_runs: Vec<CheckRun>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRunsResponse {
    workflow_runs: Vec<WorkflowRun>,
}

impl GitHubClient {
    pub fn new(token: &str, web_base_url: &str) -> Result<Self> {
        let web_base_url = web_base_url.trim_end_matches('/');
        let api_base_url = if web_base_url == "https://github.com" {
            "https://api.github.com".to_owned()
        } else {
            format!("{web_base_url}/api/v3")
        };
        Ok(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("rustgrid-agent/", env!("CARGO_PKG_VERSION")))
                .build()?,
            token: token.to_owned(),
            api_base_url,
        })
    }

    pub fn create_pull_request(
        &self,
        repo: &RepoConfig,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Result<PullRequest> {
        let url = format!(
            "{}/repos/{}/{}/pulls",
            self.api_base_url, repo.owner, repo.name
        );
        let payload = json!({"title": title, "body": body, "head": head, "base": base});
        let response = self.send_with_retry("create pull request", || {
            self.http
                .post(&url)
                .bearer_auth(&self.token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .json(&payload)
        })?;
        let status = response.status();
        let text = response.text().context("could not read GitHub response")?;
        if !status.is_success() {
            bail!(
                "GitHub create pull request returned {status}: {}",
                truncate(&text, 2_000)
            );
        }
        serde_json::from_str(&text).context("GitHub returned an invalid pull request response")
    }

    pub fn find_open_pull_request(
        &self,
        repo: &RepoConfig,
        head: &str,
    ) -> Result<Option<PullRequest>> {
        let head = format!("{}:{head}", repo.owner);
        let url = format!(
            "{}/repos/{}/{}/pulls?state=open&head={}",
            self.api_base_url,
            repo.owner,
            repo.name,
            url_encode(&head)
        );
        let response = self.send_with_retry("look up pull request", || {
            self.http
                .get(&url)
                .bearer_auth(&self.token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
        })?;
        let status = response.status();
        let text = response.text().context("could not read GitHub response")?;
        if !status.is_success() {
            bail!(
                "GitHub pull request lookup returned {status}: {}",
                truncate(&text, 2_000)
            );
        }
        let mut pulls: Vec<PullRequest> =
            serde_json::from_str(&text).context("GitHub returned invalid pull request results")?;
        Ok(pulls.pop())
    }

    pub fn check_runs(&self, repo: &RepoConfig, reference: &str) -> Result<Vec<CheckRun>> {
        let mut all_checks = Vec::new();
        for page in 1..=20 {
            let url = format!(
                "{}/repos/{}/{}/commits/{}/check-runs?per_page=100&page={page}",
                self.api_base_url,
                repo.owner,
                repo.name,
                url_encode(reference)
            );
            let response = self.send_with_retry("list check runs", || {
                self.http
                    .get(&url)
                    .bearer_auth(&self.token)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
            })?;
            let status = response.status();
            let text = response
                .text()
                .context("could not read GitHub check-runs response")?;
            if !status.is_success() {
                bail!(
                    "GitHub check-runs request returned {status}: {}",
                    truncate(&text, 2_000)
                );
            }
            let mut response: CheckRunsResponse =
                serde_json::from_str(&text).context("GitHub returned invalid check-run results")?;
            let page_len = response.check_runs.len();
            all_checks.append(&mut response.check_runs);
            if page_len < 100 {
                return Ok(all_checks);
            }
        }
        bail!("GitHub check-run pagination exceeded 2,000 results")
    }

    pub fn workflow_runs(&self, repo: &RepoConfig, commit: &str) -> Result<Vec<WorkflowRun>> {
        let mut all_runs = Vec::new();
        for page in 1..=20 {
            let url = format!(
                "{}/repos/{}/{}/actions/runs?head_sha={}&per_page=100&page={page}",
                self.api_base_url,
                repo.owner,
                repo.name,
                url_encode(commit)
            );
            let response = self.send_with_retry("list workflow runs", || {
                self.http
                    .get(&url)
                    .bearer_auth(&self.token)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
            })?;
            let status = response.status();
            let text = response
                .text()
                .context("could not read GitHub workflow-runs response")?;
            if !status.is_success() {
                bail!(
                    "GitHub workflow-runs request returned {status}: {}",
                    truncate(&text, 2_000)
                );
            }
            let mut response: WorkflowRunsResponse = serde_json::from_str(&text)
                .context("GitHub returned invalid workflow-run results")?;
            let page_len = response.workflow_runs.len();
            all_runs.append(&mut response.workflow_runs);
            if page_len < 100 {
                return Ok(all_runs);
            }
        }
        bail!("GitHub workflow-run pagination exceeded 2,000 results")
    }

    fn send_with_retry<F>(&self, operation: &str, mut build: F) -> Result<Response>
    where
        F: FnMut() -> RequestBuilder,
    {
        for attempt in 0..3u32 {
            let response = match build().send() {
                Ok(response) => response,
                Err(error) if attempt < 2 => {
                    eprintln!(
                        "[warning] retrying GitHub {operation} after transport error: {error}"
                    );
                    std::thread::sleep(Duration::from_millis(300 * (1u64 << attempt)));
                    continue;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("GitHub {operation} failed"));
                }
            };
            let status = response.status();
            if attempt < 2 && (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
            {
                let delay = github_retry_delay(&response, attempt);
                eprintln!(
                    "[warning] GitHub {operation} returned {status}; retrying in {}s",
                    delay.as_secs()
                );
                std::thread::sleep(delay);
                continue;
            }
            return Ok(response);
        }
        unreachable!("GitHub retry loop always returns")
    }
}

fn github_retry_delay(response: &Response, attempt: u32) -> Duration {
    if let Some(seconds) = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_secs(seconds.min(60));
    }
    if let Some(reset) = response
        .headers()
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        return Duration::from_secs(reset.saturating_sub(now).clamp(1, 60));
    }
    Duration::from_millis(300 * (1u64 << attempt))
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

fn truncate(value: &str, max: usize) -> &str {
    if value.len() <= max {
        return value;
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_dotcom_and_enterprise_api_origins() {
        assert_eq!(
            GitHubClient::new("token", "https://github.com")
                .unwrap()
                .api_base_url,
            "https://api.github.com"
        );
        assert_eq!(
            GitHubClient::new("token", "https://github.example.com/")
                .unwrap()
                .api_base_url,
            "https://github.example.com/api/v3"
        );
    }
}
