use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManifestRun {
    pub id: String,
    pub ticket_id: String,
}

impl ExecutionManifest {
    pub fn validate(&self, run_id: &str, ticket_id: &str) -> Result<()> {
        if self.manifest_version != 1 {
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
        self.repo_config()?;
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> ExecutionManifest {
        ExecutionManifest {
            manifest_version: 1,
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
        }
    }

    #[test]
    fn validates_claim_identity_and_version() {
        assert!(manifest().validate("run-1", "ticket-1").is_ok());
        assert!(manifest().validate("different", "ticket-1").is_err());
        let mut future = manifest();
        future.manifest_version = 2;
        assert!(future.validate("run-1", "ticket-1").is_err());
    }
}
