use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const DEFAULT_API_URL: &str = "https://app.rustgrid.com/api/v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub project_id: Option<String>,
    pub project_key: Option<String>,
    pub repo: RepoConfig,
    #[serde(default = "default_base_branch")]
    pub default_base_branch: String,
    pub quality_gate_command: String,
    #[serde(default)]
    pub codex_command: Option<String>,
    #[serde(default = "default_heartbeat_interval_seconds")]
    pub heartbeat_interval_seconds: u64,
    #[serde(default = "default_lease_seconds")]
    pub lease_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub owner: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct AppContext {
    pub config: Config,
    pub config_path: PathBuf,
    pub api_url: String,
    pub api_key: Option<String>,
    pub github_token: Option<String>,
    pub codex_command: String,
}

fn default_base_branch() -> String {
    "main".into()
}

fn default_heartbeat_interval_seconds() -> u64 {
    15
}

fn default_lease_seconds() -> u64 {
    900
}

impl AppContext {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)
            .with_context(|| format!("could not read config file {}", path.display()))?;
        let config: Config = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid JSON configuration in {}", path.display()))?;
        config.validate()?;

        let codex_command = env::var("CODEX_COMMAND")
            .ok()
            .or_else(|| config.codex_command.clone())
            .unwrap_or_else(|| "codex exec --full-auto --json -".into());

        Ok(Self {
            config,
            config_path: path.to_path_buf(),
            api_url: env::var("RUSTGRID_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.into()),
            api_key: nonempty_env("RUSTGRID_API_KEY"),
            github_token: nonempty_env("GITHUB_TOKEN"),
            codex_command,
        })
    }

    pub fn require_api_key(&self) -> Result<&str> {
        self.api_key
            .as_deref()
            .context("RUSTGRID_API_KEY is required")
    }

    pub fn require_github_token(&self) -> Result<&str> {
        self.github_token
            .as_deref()
            .context("GITHUB_TOKEN is required")
    }

    pub fn project_value(&self) -> (&'static str, &str) {
        if let Some(id) = self.config.project_id.as_deref() {
            ("project_id", id)
        } else {
            (
                "project_key",
                self.config.project_key.as_deref().expect("validated"),
            )
        }
    }
}

impl Config {
    fn validate(&self) -> Result<()> {
        match (&self.project_id, &self.project_key) {
            (None, None) => bail!("config must contain project_id or project_key"),
            (Some(_), Some(_)) => {
                bail!("config must contain only one of project_id or project_key")
            }
            _ => {}
        }
        for (name, value) in [
            ("repo.owner", self.repo.owner.as_str()),
            ("repo.name", self.repo.name.as_str()),
            ("default_base_branch", self.default_base_branch.as_str()),
            ("quality_gate_command", self.quality_gate_command.as_str()),
        ] {
            if value.trim().is_empty() {
                bail!("config value {name} cannot be empty");
            }
        }
        if !(5..=300).contains(&self.heartbeat_interval_seconds) {
            bail!("heartbeat_interval_seconds must be between 5 and 300");
        }
        if !(30..=86_400).contains(&self.lease_seconds) {
            bail!("lease_seconds must be between 30 and 86400");
        }
        if self.heartbeat_interval_seconds.saturating_mul(3) >= self.lease_seconds {
            bail!("lease_seconds must exceed three heartbeat intervals");
        }
        Ok(())
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_ambiguous_project() {
        let config = Config {
            project_id: Some("1".into()),
            project_key: Some("RG".into()),
            repo: RepoConfig {
                owner: "o".into(),
                name: "r".into(),
            },
            default_base_branch: "main".into(),
            quality_gate_command: "cargo test".into(),
            codex_command: None,
            heartbeat_interval_seconds: 15,
            lease_seconds: 900,
        };
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("only one")
        );
    }
}
