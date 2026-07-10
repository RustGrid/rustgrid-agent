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
