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
    #[serde(default)]
    pub repo: Option<RepoConfig>,
    #[serde(default = "default_base_branch")]
    pub default_base_branch: String,
    #[serde(default)]
    pub quality_gate_command: Option<String>,
    #[serde(default)]
    pub codex_command: Option<String>,
    #[serde(default = "default_heartbeat_interval_seconds")]
    pub heartbeat_interval_seconds: u64,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
    #[serde(default = "default_lease_seconds")]
    pub lease_seconds: u64,
    #[serde(default)]
    pub workspace_root: Option<PathBuf>,
    #[serde(default = "default_command_timeout_seconds")]
    pub command_timeout_seconds: u64,
    #[serde(default = "default_run_timeout_seconds")]
    pub run_timeout_seconds: u64,
    #[serde(default = "default_failed_workspace_retention_hours")]
    pub failed_workspace_retention_hours: u64,
    #[serde(default = "default_max_command_output_bytes")]
    pub max_command_output_bytes: u64,
    #[serde(default = "default_max_workspace_bytes")]
    pub max_workspace_bytes: u64,
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
    pub workspace_root: PathBuf,
}

fn default_base_branch() -> String {
    "main".into()
}

fn default_heartbeat_interval_seconds() -> u64 {
    15
}

fn default_max_concurrency() -> usize {
    1
}

fn default_lease_seconds() -> u64 {
    900
}

fn default_command_timeout_seconds() -> u64 {
    1800
}

fn default_run_timeout_seconds() -> u64 {
    7200
}

fn default_failed_workspace_retention_hours() -> u64 {
    72
}

fn default_max_command_output_bytes() -> u64 {
    8 * 1024 * 1024
}

fn default_max_workspace_bytes() -> u64 {
    5 * 1024 * 1024 * 1024
}

impl AppContext {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)
            .with_context(|| format!("could not read config file {}", path.display()))?;
        let config: Config = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid JSON configuration in {}", path.display()))?;
        config.validate()?;

        let workspace_root = config.workspace_root.clone().unwrap_or_else(|| {
            std::env::temp_dir()
                .join("rustgrid-agent")
                .join("workspaces")
        });
        Ok(Self {
            config,
            config_path: path.to_path_buf(),
            api_url: env::var("RUSTGRID_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.into()),
            api_key: nonempty_env("RUSTGRID_API_KEY"),
            workspace_root,
        })
    }

    pub fn require_api_key(&self) -> Result<&str> {
        self.api_key
            .as_deref()
            .context("RUSTGRID_API_KEY is required")
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
        for (name, value) in [("default_base_branch", self.default_base_branch.as_str())] {
            if value.trim().is_empty() {
                bail!("config value {name} cannot be empty");
            }
        }
        if !(5..=300).contains(&self.heartbeat_interval_seconds) {
            bail!("heartbeat_interval_seconds must be between 5 and 300");
        }
        if !(1..=100).contains(&self.max_concurrency) {
            bail!("max_concurrency must be between 1 and 100");
        }
        if !(30..=86_400).contains(&self.lease_seconds) {
            bail!("lease_seconds must be between 30 and 86400");
        }
        if self.heartbeat_interval_seconds.saturating_mul(3) >= self.lease_seconds {
            bail!("lease_seconds must exceed three heartbeat intervals");
        }
        if self.failed_workspace_retention_hours > 24 * 30 {
            bail!("failed_workspace_retention_hours cannot exceed 720");
        }
        if self.max_command_output_bytes < 64 * 1024 {
            bail!("max_command_output_bytes must be at least 65536");
        }
        if self.max_workspace_bytes < 64 * 1024 * 1024 {
            bail!("max_workspace_bytes must be at least 67108864");
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
            repo: Some(RepoConfig {
                owner: "o".into(),
                name: "r".into(),
            }),
            default_base_branch: "main".into(),
            quality_gate_command: None,
            codex_command: None,
            heartbeat_interval_seconds: 15,
            max_concurrency: 1,
            lease_seconds: 900,
            workspace_root: None,
            command_timeout_seconds: 1800,
            run_timeout_seconds: 7200,
            failed_workspace_retention_hours: 72,
            max_command_output_bytes: 8 * 1024 * 1024,
            max_workspace_bytes: 5 * 1024 * 1024 * 1024,
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
