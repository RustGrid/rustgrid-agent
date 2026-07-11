use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::RepoConfig;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionManifest {
    pub manifest_version: u32,
    pub run: ManifestRun,
    pub project_id: String,
    pub project_key: String,
    pub project_name: String,
    pub ticket_id: String,
    pub ticket_key: String,
    pub ticket_title: String,
    pub repository_id: u64,
    pub repository: String,
    pub clone_url: String,
    pub web_base_url: String,
    pub installation_id: u64,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub required_workflows: Vec<String>,
    #[serde(default)]
    pub required_permissions: serde_json::Value,
    pub execution_policy: serde_json::Value,
    pub execution_policy_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManifestRun {
    pub id: String,
    pub ticket_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionPolicy {
    pub policy_version: u32,
    pub codex: CodexPolicy,
    pub quality_gates: Vec<QualityGatePolicy>,
    pub timeout_seconds: u64,
    pub sandbox: SandboxPolicy,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CodexPolicy {
    pub command: Vec<String>,
    pub environment_allowlist: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QualityGatePolicy {
    pub id: String,
    pub command: String,
    pub timeout_seconds: u64,
    pub required: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SandboxPolicy {
    pub mode: String,
    pub network_access: bool,
    pub writable_roots: Vec<String>,
    pub approval_policy: String,
}

impl ExecutionManifest {
    pub fn validate(&self, run_id: &str, ticket_id: &str) -> Result<()> {
        if self.manifest_version != 2 {
            bail!(
                "unsupported execution manifest version {}",
                self.manifest_version
            );
        }
        if self.run.id != run_id || self.run.ticket_id != ticket_id || self.ticket_id != ticket_id {
            bail!("execution manifest identity does not match the claimed run");
        }
        for (name, value) in [
            ("project_id", self.project_id.as_str()),
            ("project_key", self.project_key.as_str()),
            ("ticket_key", self.ticket_key.as_str()),
            ("repository", self.repository.as_str()),
            ("clone_url", self.clone_url.as_str()),
            ("web_base_url", self.web_base_url.as_str()),
        ] {
            if value.trim().is_empty() {
                bail!("execution manifest field {name} cannot be empty");
            }
        }
        if self.repository_id == 0 || self.installation_id == 0 {
            bail!("execution manifest repository and installation IDs must be non-zero");
        }
        let actual_hash = hex::encode(Sha256::digest(self.execution_policy.to_string().as_bytes()));
        if actual_hash != self.execution_policy_sha256 {
            bail!("execution policy hash does not match the manifest payload");
        }
        self.policy()?.validate()?;
        self.repo_config()?;
        Ok(())
    }

    pub fn policy(&self) -> Result<ExecutionPolicy> {
        serde_json::from_value(self.execution_policy.clone())
            .context("execution manifest contains an invalid execution policy")
    }

    pub fn repo_config(&self) -> Result<RepoConfig> {
        let (owner, name) = self
            .repository
            .split_once('/')
            .context("execution manifest repository must be owner/name")?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            bail!("execution manifest repository must be owner/name");
        }
        Ok(RepoConfig {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
}

impl ExecutionPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.policy_version != 1 || self.timeout_seconds == 0 || self.codex.command.is_empty() {
            bail!("unsupported or incomplete execution policy");
        }
        if self
            .codex
            .command
            .iter()
            .any(|value| value.trim().is_empty())
            || self.codex.command.iter().any(|value| {
                matches!(
                    value.as_str(),
                    "--dangerously-bypass-approvals-and-sandbox"
                        | "--dangerously-bypass-hook-trust"
                )
            })
            || self.quality_gates.iter().any(|gate| {
                gate.id.trim().is_empty()
                    || gate.command.trim().is_empty()
                    || gate.timeout_seconds == 0
            })
        {
            bail!("execution policy contains an empty command, gate id, or timeout");
        }
        if self.codex.environment_allowlist.iter().any(|name| {
            matches!(
                name.as_str(),
                "RUSTGRID_API_KEY" | "GITHUB_TOKEN" | "GH_TOKEN"
            )
        }) {
            bail!("execution policy attempts to expose a protected credential variable");
        }
        if self.sandbox.mode != "workspace_write"
            || !self.sandbox.network_access
            || self.sandbox.approval_policy != "never"
            || !self.sandbox.writable_roots.iter().any(|root| root == ".")
        {
            bail!("execution sandbox policy is not enforceable by this worker");
        }
        Ok(())
    }

    pub fn codex_command(&self) -> String {
        let mut command = self.codex.command.clone();
        let insertion = command
            .iter()
            .position(|part| part == "-")
            .unwrap_or(command.len());
        if !command
            .iter()
            .any(|part| part == "--sandbox" || part == "-s")
        {
            command.splice(
                insertion..insertion,
                ["--sandbox".into(), "workspace-write".into()],
            );
        }
        let insertion = command
            .iter()
            .position(|part| part == "-")
            .unwrap_or(command.len());
        if !command
            .iter()
            .any(|part| part.starts_with("approval_policy="))
        {
            command.splice(
                insertion..insertion,
                ["-c".into(), "approval_policy=\"never\"".into()],
            );
        }
        let insertion = command
            .iter()
            .position(|part| part == "-")
            .unwrap_or(command.len());
        if !command.iter().any(|part| part == "--ephemeral") {
            command.insert(insertion, "--ephemeral".into());
        }
        command
            .iter()
            .map(|part| {
                shlex::try_quote(part).map_or_else(|_| part.clone(), |quoted| quoted.into_owned())
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> ExecutionManifest {
        ExecutionManifest {
            manifest_version: 2,
            run: ManifestRun {
                id: "run-1".into(),
                ticket_id: "ticket-1".into(),
            },
            project_id: "project-1".into(),
            project_key: "RG".into(),
            project_name: "RustGrid".into(),
            ticket_id: "ticket-1".into(),
            ticket_key: "RG-1".into(),
            ticket_title: "Task".into(),
            repository_id: 7,
            repository: "RustGrid/agent".into(),
            clone_url: "https://github.com/RustGrid/agent.git".into(),
            web_base_url: "https://github.com".into(),
            installation_id: 42,
            default_branch: Some("main".into()),
            required_workflows: vec![],
            required_permissions: serde_json::json!({}),
            execution_policy: serde_json::json!({
                "policy_version": 1,
                "codex": {"command": ["codex", "exec", "--json"], "environment_allowlist": ["PATH", "HOME"]},
                "quality_gates": [{"id":"gate-1","command":"cargo test","timeout_seconds":900,"required":true}],
                "timeout_seconds": 3600,
                "sandbox": {"mode":"workspace_write","network_access":true,"writable_roots":["."],"approval_policy":"never"}
            }),
            execution_policy_sha256: String::new(),
        }
    }

    #[test]
    fn validates_claim_identity_and_version() {
        let mut valid = manifest();
        valid.execution_policy_sha256 = hex::encode(Sha256::digest(
            valid.execution_policy.to_string().as_bytes(),
        ));
        assert!(valid.validate("run-1", "ticket-1").is_ok());
        assert!(valid.validate("different", "ticket-1").is_err());
        let mut future = manifest();
        future.manifest_version = 3;
        assert!(future.validate("run-1", "ticket-1").is_err());
    }

    #[test]
    fn hardens_the_codex_command() {
        let policy: ExecutionPolicy = serde_json::from_value(manifest().execution_policy).unwrap();
        let command = policy.codex_command();
        assert!(command.contains("--sandbox workspace-write"));
        assert!(command.contains("approval_policy"));
        assert!(command.contains("--ephemeral"));
    }
}
