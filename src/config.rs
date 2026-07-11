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
    #[serde(default)]
    pub executor: ExecutorConfig,
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
    #[serde(default = "default_max_child_memory_bytes")]
    pub max_child_memory_bytes: u64,
    #[serde(default = "default_max_child_file_bytes")]
    pub max_child_file_bytes: u64,
    #[serde(default = "default_max_child_open_files")]
    pub max_child_open_files: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutorConfig {
    #[default]
    Local,
    DockerSandbox {
        #[serde(default = "default_sbx_command")]
        command: String,
        #[serde(default = "default_sandbox_template")]
        template: String,
        #[serde(default = "default_sandbox_cpus")]
        cpus: u16,
        #[serde(default = "default_sandbox_memory")]
        memory: String,
        #[serde(default = "default_sandbox_capacity_cpus")]
        capacity_cpus: u16,
        #[serde(default = "default_sandbox_capacity_memory")]
        capacity_memory: String,
    },
}

impl ExecutorConfig {
    pub fn is_isolated(&self) -> bool {
        matches!(self, Self::DockerSandbox { .. })
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::DockerSandbox { .. } => "docker_sandbox",
        }
    }

    pub(crate) fn validate_production(&self, max_concurrency: usize) -> Result<()> {
        let Self::DockerSandbox {
            template,
            cpus,
            memory,
            capacity_cpus,
            capacity_memory,
            ..
        } = self
        else {
            bail!("executor.kind=docker_sandbox is required for production");
        };
        let digest = template.split_once("@sha256:").map(|(_, value)| value);
        if !digest
            .is_some_and(|value| value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()))
        {
            bail!("production sandbox template must be pinned by a 64-character sha256 digest");
        }
        let required_cpus = usize::from(*cpus).saturating_mul(max_concurrency);
        if required_cpus > usize::from(*capacity_cpus) {
            bail!("sandbox CPU allocation exceeds configured host capacity");
        }
        let required_memory = parse_binary_size(memory)?.saturating_mul(max_concurrency as u64);
        if required_memory > parse_binary_size(capacity_memory)? {
            bail!("sandbox memory allocation exceeds configured host capacity");
        }
        Ok(())
    }
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

fn default_sbx_command() -> String {
    "sbx".into()
}
fn default_sandbox_template() -> String {
    "docker.io/docker/sandbox-templates@sha256:943c52aa48a4f4473a9c91e43aced8def51667935ad9866ffc29a821d5982f97".into()
}
fn default_sandbox_cpus() -> u16 {
    4
}
fn default_sandbox_memory() -> String {
    "8g".into()
}
fn default_sandbox_capacity_cpus() -> u16 {
    4
}
fn default_sandbox_capacity_memory() -> String {
    "8g".into()
}

fn parse_binary_size(value: &str) -> Result<u64> {
    let normalized = value.trim().to_ascii_lowercase();
    let split = normalized
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(normalized.len());
    let number: u64 = normalized[..split]
        .parse()
        .context("memory size must start with a number")?;
    if number == 0 {
        bail!("memory size must be greater than zero");
    }
    let multiplier = match &normalized[split..] {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => bail!("memory size must use b, k, m, or g binary units"),
    };
    number
        .checked_mul(multiplier)
        .context("memory size overflow")
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

fn default_max_child_memory_bytes() -> u64 {
    8 * 1024 * 1024 * 1024
}

fn default_max_child_file_bytes() -> u64 {
    1024 * 1024 * 1024
}

fn default_max_child_open_files() -> u64 {
    1024
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
            api_key: nonempty_env("RUSTGRID_WORKER_API_KEY"),
            workspace_root,
        })
    }

    pub fn require_api_key(&self) -> Result<&str> {
        self.api_key
            .as_deref()
            .context("RUSTGRID_WORKER_API_KEY is required")
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
        match &self.executor {
            ExecutorConfig::Local if self.max_concurrency != 1 => {
                bail!("local executor requires max_concurrency=1")
            }
            ExecutorConfig::DockerSandbox {
                command,
                template,
                cpus,
                memory,
                capacity_cpus,
                capacity_memory,
            } => {
                if command.trim().is_empty()
                    || template.trim().is_empty()
                    || memory.trim().is_empty()
                    || capacity_memory.trim().is_empty()
                {
                    bail!("docker sandbox command, template, and memory cannot be empty");
                }
                if !(1..=64).contains(cpus) {
                    bail!("docker sandbox cpus must be between 1 and 64");
                }
                if !(1..=256).contains(capacity_cpus) {
                    bail!("docker sandbox capacity_cpus must be between 1 and 256");
                }
                parse_binary_size(memory)?;
                parse_binary_size(capacity_memory)?;
            }
            ExecutorConfig::Local => {}
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
        if self.max_child_memory_bytes < 256 * 1024 * 1024 {
            bail!("max_child_memory_bytes must be at least 268435456");
        }
        if self.max_child_file_bytes < 1024 * 1024 {
            bail!("max_child_file_bytes must be at least 1048576");
        }
        if !(64..=65_536).contains(&self.max_child_open_files) {
            bail!("max_child_open_files must be between 64 and 65536");
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
            executor: ExecutorConfig::Local,
            lease_seconds: 900,
            workspace_root: None,
            command_timeout_seconds: 1800,
            run_timeout_seconds: 7200,
            failed_workspace_retention_hours: 72,
            max_command_output_bytes: 8 * 1024 * 1024,
            max_workspace_bytes: 5 * 1024 * 1024 * 1024,
            max_child_memory_bytes: 8 * 1024 * 1024 * 1024,
            max_child_file_bytes: 1024 * 1024 * 1024,
            max_child_open_files: 1024,
        };
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("only one")
        );
    }

    #[test]
    fn rejects_concurrent_local_execution() {
        let mut config: Config = serde_json::from_str(
            r#"{"project_key":"RG","max_concurrency":2,"executor":{"kind":"local"}}"#,
        )
        .unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("local executor")
        );
        config.executor = ExecutorConfig::DockerSandbox {
            command: "sbx".into(),
            template: "test".into(),
            cpus: 1,
            memory: "1g".into(),
            capacity_cpus: 2,
            capacity_memory: "2g".into(),
        };
        config.validate().unwrap();
    }

    #[test]
    fn production_requires_digest_and_capacity() {
        let mutable = ExecutorConfig::DockerSandbox {
            command: "sbx".into(),
            template: "example:latest".into(),
            cpus: 4,
            memory: "8g".into(),
            capacity_cpus: 8,
            capacity_memory: "16g".into(),
        };
        assert!(mutable.validate_production(2).is_err());
        let pinned = ExecutorConfig::DockerSandbox {
            command: "sbx".into(),
            template: format!("example@sha256:{}", "a".repeat(64)),
            cpus: 4,
            memory: "8g".into(),
            capacity_cpus: 8,
            capacity_memory: "16g".into(),
        };
        pinned.validate_production(2).unwrap();
        assert!(pinned.validate_production(3).is_err());
    }
}
