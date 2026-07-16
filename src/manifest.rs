use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use crate::config::RepoConfig;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionManifest {
    pub manifest_version: u32,
    pub run: ManifestRun,
    #[serde(default)]
    pub attachments: Vec<ManifestAttachment>,
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
    #[serde(default)]
    pub input_prompt: String,
    #[serde(default = "default_attempt")]
    pub attempt: u32,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManifestAttachment {
    pub id: String,
    pub ticket_id: String,
    pub filename: String,
    #[serde(default)]
    pub mime: Option<String>,
    pub media_family: String,
    #[serde(default)]
    pub size_bytes: Option<i64>,
    #[serde(default)]
    pub sha256: Option<String>,
    pub status: String,
    pub virus_status: String,
    #[serde(default)]
    pub variants: Vec<ManifestAttachmentVariant>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManifestAttachmentVariant {
    pub kind: String,
    pub mime: String,
    pub ready: bool,
}

fn default_attempt() -> u32 {
    1
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
    #[serde(default)]
    pub idle_timeout_seconds: Option<u64>,
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

impl ExecutionPolicy {
    pub fn codex_idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.codex
                .idle_timeout_seconds
                .unwrap_or(self.timeout_seconds.min(300)),
        )
    }

    pub fn requires_npm_registry(&self) -> bool {
        self.quality_gates.iter().any(|gate| {
            gate.command.split_whitespace().any(|token| {
                let executable = token
                    .trim_matches(|character: char| {
                        !character.is_ascii_alphanumeric()
                            && character != '-'
                            && character != '_'
                            && character != '/'
                    })
                    .rsplit('/')
                    .next()
                    .unwrap_or_default();
                matches!(executable, "npm" | "npx" | "pnpm" | "yarn" | "bun")
            })
        })
    }
}

impl ExecutionManifest {
    pub fn fresh_start(&self) -> Result<bool> {
        match self.run.metadata.get("fresh_start") {
            None => Ok(false),
            Some(serde_json::Value::Bool(value)) => Ok(*value),
            Some(_) => bail!("execution manifest metadata.fresh_start must be a boolean"),
        }
    }

    pub fn resume_from_run_id(&self) -> Result<Option<&str>> {
        let Some(value) = self.run.metadata.get("resume_from_run_id") else {
            return Ok(None);
        };
        if self.fresh_start()? {
            bail!("execution manifest cannot combine fresh_start with resume_from_run_id");
        }
        let source_run_id = value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .context("execution manifest metadata.resume_from_run_id must be a non-empty string")?;
        if self.run.attempt <= 1 {
            bail!("execution manifest cannot resume recovery work on the first attempt");
        }
        if source_run_id == self.run.id {
            bail!("execution manifest cannot resume a run from itself");
        }
        Ok(Some(source_run_id))
    }

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
        if self.run.attempt == 0 {
            bail!("execution manifest run attempt must be at least 1");
        }
        if self.run.input_prompt.trim().is_empty() {
            bail!("execution manifest run input_prompt cannot be empty");
        }
        if self.attachments.len() > 100 {
            bail!("execution manifest contains too many attachments");
        }
        let mut attachment_ids = HashSet::new();
        for attachment in &self.attachments {
            if attachment.ticket_id != ticket_id
                || attachment.filename.trim().is_empty()
                || attachment.status != "ready"
                || attachment.virus_status != "clean"
            {
                bail!("execution manifest contains invalid attachment context");
            }
            if uuid::Uuid::parse_str(&attachment.id).is_err()
                || !attachment_ids.insert(&attachment.id)
            {
                bail!("execution manifest attachment IDs must be unique UUIDs");
            }
            if attachment.size_bytes.is_some_and(|size| size <= 0) {
                bail!("execution manifest attachment sizes must be positive");
            }
            if attachment.sha256.as_ref().is_some_and(|sha256| {
                sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                bail!("execution manifest attachment sha256 must be hexadecimal");
            }
        }
        self.fresh_start()?;
        self.resume_from_run_id()?;
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
        let web = validate_https_url("web_base_url", &self.web_base_url)?;
        let clone = validate_https_url("clone_url", &self.clone_url)?;
        if web.host_str() != clone.host_str() {
            bail!("execution manifest clone_url and web_base_url hosts must match");
        }
        let actual_hash = hex::encode(Sha256::digest(self.execution_policy.to_string().as_bytes()));
        if actual_hash != self.execution_policy_sha256 {
            bail!("execution policy hash does not match the manifest payload");
        }
        self.policy()?.validate()?;
        self.repo_config()?;
        self.normalized_required_workflows()?;
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

    pub fn normalized_required_workflows(&self) -> Result<Vec<String>> {
        let mut workflows = Vec::new();
        let mut seen = HashSet::new();
        for configured in &self.required_workflows {
            let expanded = serde_json::from_str::<Vec<String>>(configured)
                .ok()
                .filter(|_| configured.trim_start().starts_with('['))
                .unwrap_or_else(|| vec![configured.clone()]);
            for name in expanded {
                let name = name.trim();
                if name.is_empty() || name.len() > 200 {
                    bail!("required workflow names must contain 1 to 200 characters");
                }
                if seen.insert(name.to_owned()) {
                    workflows.push(name.to_owned());
                }
            }
        }
        if workflows.len() > 100 {
            bail!("execution manifest cannot require more than 100 workflows");
        }
        Ok(workflows)
    }
}

fn validate_https_url(name: &str, value: &str) -> Result<Url> {
    let url = Url::parse(value).with_context(|| format!("execution manifest {name} is invalid"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("execution manifest {name} must be an HTTPS URL without credentials");
    }
    Ok(url)
}

impl ExecutionPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.policy_version != 1
            || !(1..=86_400).contains(&self.timeout_seconds)
            || self.codex.command.is_empty()
            || self.codex.command.len() > 256
            || self.quality_gates.len() > 100
            || self.codex.environment_allowlist.len() > 128
        {
            bail!("unsupported or incomplete execution policy");
        }
        if self
            .codex
            .command
            .iter()
            .any(|value| value.trim().is_empty())
            || self.codex.command.iter().any(|value| {
                let lower = value.to_ascii_lowercase();
                matches!(
                    lower.as_str(),
                    "--sandbox"
                        | "-s"
                        | "--dangerously-bypass-approvals-and-sandbox"
                        | "--dangerously-bypass-hook-trust"
                ) || lower.starts_with("--sandbox=")
                    || lower.contains("approval_policy")
                    || lower.contains("sandbox_mode")
            })
            || self
                .codex
                .idle_timeout_seconds
                .is_some_and(|seconds| seconds == 0 || seconds > self.timeout_seconds)
            || self.quality_gates.iter().any(|gate| {
                gate.id.trim().is_empty()
                    || gate.command.trim().is_empty()
                    || !(1..=86_400).contains(&gate.timeout_seconds)
            })
        {
            bail!("execution policy contains an empty command, gate id, or timeout");
        }
        if self
            .codex
            .environment_allowlist
            .iter()
            .any(|name| is_sensitive_environment_name(name))
        {
            bail!("execution policy attempts to expose a protected credential variable");
        }
        if self.sandbox.mode != "workspace_write"
            || !self.sandbox.network_access
            || self.sandbox.approval_policy != "never"
            || self.sandbox.writable_roots != ["."]
        {
            bail!("execution sandbox policy is not enforceable by this worker");
        }
        Ok(())
    }

    pub fn codex_args(
        &self,
        externally_isolated: bool,
        image_paths: &[std::path::PathBuf],
    ) -> Vec<String> {
        let mut command = self.codex.command.clone();
        let insertion = command
            .iter()
            .position(|part| part == "-")
            .unwrap_or(command.len());
        command.splice(
            insertion..insertion,
            image_paths
                .iter()
                .flat_map(|path| ["--image".to_owned(), path.to_string_lossy().into_owned()]),
        );
        let insertion = command
            .iter()
            .position(|part| part == "-")
            .unwrap_or(command.len());
        command.splice(
            insertion..insertion,
            [
                "--sandbox".into(),
                if externally_isolated {
                    "danger-full-access".into()
                } else {
                    "workspace-write".into()
                },
            ],
        );
        let insertion = command
            .iter()
            .position(|part| part == "-")
            .unwrap_or(command.len());
        command.splice(
            insertion..insertion,
            ["-c".into(), "approval_policy=\"never\"".into()],
        );
        let insertion = command
            .iter()
            .position(|part| part == "-")
            .unwrap_or(command.len());
        if !command.iter().any(|part| part == "--ephemeral") {
            command.insert(insertion, "--ephemeral".into());
        }
        command
    }
}

fn is_sensitive_environment_name(name: &str) -> bool {
    let normalized = name.trim().to_ascii_uppercase();
    normalized == "SSH_AUTH_SOCK"
        || normalized.contains("TOKEN")
        || normalized.contains("SECRET")
        || normalized.contains("PASSWORD")
        || normalized.contains("CREDENTIAL")
        || normalized.contains("PRIVATE_KEY")
        || normalized.contains("API_KEY")
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
                input_prompt: "Implement the ticket.".into(),
                attempt: 1,
                metadata: serde_json::json!({}),
            },
            attachments: vec![],
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
    fn accepts_only_explicit_later_attempt_recovery_lineage() {
        let mut retry = manifest();
        retry.run.attempt = 2;
        retry.run.metadata = serde_json::json!({"resume_from_run_id": "run-previous"});
        assert_eq!(retry.resume_from_run_id().unwrap(), Some("run-previous"));

        retry.run.attempt = 1;
        assert!(retry.resume_from_run_id().is_err());
        retry.run.attempt = 2;
        retry.run.metadata = serde_json::json!({"resume_from_run_id": "run-1"});
        assert!(retry.resume_from_run_id().is_err());
        retry.run.metadata = serde_json::json!({"resume_from_run_id": 7});
        assert!(retry.resume_from_run_id().is_err());
    }

    #[test]
    fn validates_fresh_start_metadata_and_rejects_recovery_lineage() {
        let mut fresh = manifest();
        fresh.run.attempt = 2;
        fresh.run.metadata = serde_json::json!({"fresh_start": true});
        assert!(fresh.fresh_start().unwrap());
        assert_eq!(fresh.resume_from_run_id().unwrap(), None);

        fresh.run.metadata = serde_json::json!({"fresh_start": "yes"});
        assert!(fresh.fresh_start().is_err());

        fresh.run.metadata = serde_json::json!({
            "fresh_start": true,
            "resume_from_run_id": "run-previous"
        });
        assert!(fresh.resume_from_run_id().is_err());
    }

    #[test]
    fn detects_npm_family_registry_requirements_from_signed_gates() {
        let mut policy = manifest().policy().unwrap();
        assert!(!policy.requires_npm_registry());

        policy.quality_gates[0].command = "npm ci && npm test".into();
        assert!(policy.requires_npm_registry());

        policy.quality_gates[0].command = "/usr/local/bin/pnpm test".into();
        assert!(policy.requires_npm_registry());
    }

    #[test]
    fn codex_idle_timeout_is_bounded_by_the_signed_run_policy() {
        let policy = manifest().policy().unwrap();
        assert_eq!(
            policy.codex_idle_timeout(),
            std::time::Duration::from_secs(300)
        );

        let mut configured = manifest().policy().unwrap();
        configured.codex.idle_timeout_seconds = Some(120);
        assert_eq!(
            configured.codex_idle_timeout(),
            std::time::Duration::from_secs(120)
        );
        assert!(configured.validate().is_ok());

        configured.codex.idle_timeout_seconds = Some(configured.timeout_seconds + 1);
        assert!(configured.validate().is_err());
    }

    #[test]
    fn hardens_the_codex_command() {
        let policy: ExecutionPolicy = serde_json::from_value(manifest().execution_policy).unwrap();
        let args = policy.codex_args(false, &[]);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "workspace-write"])
        );
        assert!(args.iter().any(|arg| arg.contains("approval_policy")));
        assert!(args.iter().any(|arg| arg == "--ephemeral"));
        let image_args = policy.codex_args(
            false,
            &[std::path::PathBuf::from(
                ".git/rustgrid-agent/context/attachments/screenshot.png",
            )],
        );
        assert!(image_args.windows(2).any(|pair| {
            pair == [
                "--image",
                ".git/rustgrid-agent/context/attachments/screenshot.png",
            ]
        }));
        assert!(
            policy
                .codex_args(true, &[])
                .windows(2)
                .any(|pair| pair == ["--sandbox", "danger-full-access"])
        );
    }

    #[test]
    fn normalizes_legacy_double_encoded_required_workflows() {
        let mut value = manifest();
        value.required_workflows = vec![r#"["Typecheck and build"]"#.into()];
        assert_eq!(
            value.normalized_required_workflows().unwrap(),
            ["Typecheck and build"]
        );

        value.required_workflows = vec!["Typecheck and build".into()];
        assert_eq!(
            value.normalized_required_workflows().unwrap(),
            ["Typecheck and build"]
        );
    }

    #[test]
    fn rejects_unsafe_or_cross_origin_repository_urls() {
        let mut unsafe_manifest = manifest();
        unsafe_manifest.clone_url = "file:///tmp/repository".into();
        assert!(unsafe_manifest.validate("run-1", "ticket-1").is_err());

        let mut cross_origin = manifest();
        cross_origin.clone_url = "https://evil.example/RustGrid/example.git".into();
        assert!(cross_origin.validate("run-1", "ticket-1").is_err());
    }

    #[test]
    fn rejects_unbounded_execution_policy() {
        let mut value = manifest().execution_policy;
        value["timeout_seconds"] = serde_json::json!(86_401);
        let policy: ExecutionPolicy = serde_json::from_value(value).unwrap();
        assert!(policy.validate().is_err());
    }

    #[test]
    fn rejects_caller_supplied_sandbox_and_approval_overrides() {
        for command in [
            vec!["codex", "exec", "--sandbox", "danger-full-access"],
            vec!["codex", "exec", "--sandbox=read-only"],
            vec!["codex", "exec", "-s", "workspace-write"],
            vec!["codex", "exec", "-c", "approval_policy=on-request"],
            vec!["codex", "exec", "-capproval_policy=never"],
            vec!["codex", "exec", "-c", "sandbox_mode=workspace-write"],
        ] {
            let mut value = manifest().execution_policy;
            value["codex"]["command"] = serde_json::json!(command);
            let policy: ExecutionPolicy = serde_json::from_value(value).unwrap();
            assert!(policy.validate().is_err(), "accepted command: {command:?}");
        }
    }

    #[test]
    fn rejects_additional_writable_roots() {
        let mut value = manifest().execution_policy;
        value["sandbox"]["writable_roots"] = serde_json::json!([".", "/tmp"]);
        let policy: ExecutionPolicy = serde_json::from_value(value).unwrap();
        assert!(policy.validate().is_err());
    }

    #[test]
    fn rejects_sensitive_environment_aliases() {
        for name in [
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "DATABASE_PASSWORD",
            "DEPLOY_TOKEN",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "SSH_AUTH_SOCK",
        ] {
            let mut value = manifest().execution_policy;
            value["codex"]["environment_allowlist"] = serde_json::json!(["PATH", name]);
            let policy: ExecutionPolicy = serde_json::from_value(value).unwrap();
            assert!(
                policy.validate().is_err(),
                "accepted sensitive variable {name}"
            );
        }
    }
}
