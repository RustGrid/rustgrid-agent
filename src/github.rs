use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::json;

use crate::config::RepoConfig;

pub struct GitHubClient {
    http: Client,
    token: String,
}

#[derive(Debug, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub html_url: String,
}

#[derive(Debug, Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CheckRunsResponse {
    check_runs: Vec<CheckRun>,
}

impl GitHubClient {
    pub fn new(token: &str) -> Result<Self> {
        Ok(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("rustgrid-agent/", env!("CARGO_PKG_VERSION")))
                .build()?,
            token: token.to_owned(),
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
            "https://api.github.com/repos/{}/{}/pulls",
            repo.owner, repo.name
        );
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&json!({"title": title, "body": body, "head": head, "base": base}))
            .send()
            .context("GitHub pull request request failed")?;
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
            "https://api.github.com/repos/{}/{}/pulls?state=open&head={}",
            repo.owner,
            repo.name,
            url_encode(&head)
        );
        let response = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .context("GitHub pull request lookup failed")?;
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
        let url = format!(
            "https://api.github.com/repos/{}/{}/commits/{}/check-runs?per_page=100",
            repo.owner,
            repo.name,
            url_encode(reference)
        );
        let response = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .context("GitHub check-runs request failed")?;
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
        let response: CheckRunsResponse =
            serde_json::from_str(&text).context("GitHub returned invalid check-run results")?;
        Ok(response.check_runs)
    }
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
