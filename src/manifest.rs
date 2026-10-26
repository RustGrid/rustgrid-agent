use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use crate::{
    agent::AgentProvider, architecture::ArchitectureContext, config::RepoConfig,
    error::ManifestError,
};

pub type ManifestResult<T> = std::result::Result<T, ManifestValidationError>;

#[derive(Debug)]
pub struct ManifestValidationError {
    pub kind: ManifestError,
    message: String,
    source: ManifestDiagnostic,
}

#[derive(Debug)]
struct ManifestDiagnostic(String);

impl std::fmt::Display for ManifestDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ManifestDiagnostic {}

impl ManifestValidationError {
    fn from_anyhow(kind: ManifestError, error: anyhow::Error) -> Self {
        let nested_kind = error
            .downcast_ref::<Self>()
            .map_or(kind, |failure| failure.kind);
        let message = error.to_string();
        Self {
            kind: nested_kind,
            message: message.clone(),
            source: ManifestDiagnostic(message),
        }
    }
}

impl std::fmt::Display for ManifestValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ManifestValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

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
    /// The project's architecture contract; `None` when the project defines none.
    #[serde(default)]
    pub architecture: Option<ArchitectureContext>,
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

/// The signed run policy. Version 1 names its agent block `codex`; version 2
/// carries a provider-tagged `agent` block. Both normalize to [`AgentPolicy`].
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "WireExecutionPolicy", into = "WireExecutionPolicy")]
pub struct ExecutionPolicy {
    pub policy_version: u32,
    pub agent: AgentPolicy,
    pub quality_gates: Vec<QualityGatePolicy>,
    pub timeout_seconds: u64,
    pub sandbox: SandboxPolicy,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentPolicy {
    pub provider: AgentProvider,
    pub command: Vec<String>,
    pub environment_allowlist: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CodexPolicy {
    command: Vec<String>,
    environment_allowlist: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idle_timeout_seconds: Option<u64>,
}

#[derive(Deserialize, Serialize)]
struct WireExecutionPolicy {
    policy_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codex: Option<CodexPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<AgentPolicy>,
    quality_gates: Vec<QualityGatePolicy>,
    timeout_seconds: u64,
    sandbox: SandboxPolicy,
}

impl TryFrom<WireExecutionPolicy> for ExecutionPolicy {
    type Error = String;

    fn try_from(wire: WireExecutionPolicy) -> std::result::Result<Self, Self::Error> {
        let agent = match (wire.policy_version, wire.codex, wire.agent) {
            (1, Some(codex), None) => AgentPolicy {
                provider: AgentProvider::Codex,
                command: codex.command,
                environment_allowlist: codex.environment_allowlist,
                idle_timeout_seconds: codex.idle_timeout_seconds,
            },
            (2, None, Some(agent)) => agent,
            (1 | 2, _, _) => {
                return Err(
                    "execution policy v1 requires a codex block and v2 requires an agent block"
                        .into(),
                );
            }
            (version, _, _) => {
                return Err(format!("unsupported execution policy version {version}"));
            }
        };
        Ok(Self {
            policy_version: wire.policy_version,
            agent,
            quality_gates: wire.quality_gates,
            timeout_seconds: wire.timeout_seconds,
            sandbox: wire.sandbox,
        })
    }
}

impl From<ExecutionPolicy> for WireExecutionPolicy {
    fn from(policy: ExecutionPolicy) -> Self {
        let (codex, agent) = if policy.policy_version == 1 {
            let agent = policy.agent;
            (
                Some(CodexPolicy {
                    command: agent.command,
                    environment_allowlist: agent.environment_allowlist,
                    idle_timeout_seconds: agent.idle_timeout_seconds,
                }),
                None,
            )
        } else {
            (None, Some(policy.agent))
        };
        Self {
            policy_version: policy.policy_version,
            codex,
            agent,
            quality_gates: policy.quality_gates,
            timeout_seconds: policy.timeout_seconds,
            sandbox: policy.sandbox,
        }
    }
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
    pub fn agent_idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.agent
                .idle_timeout_seconds
                .unwrap_or(self.timeout_seconds.min(300)),
        )
    }

    pub fn requires_npm_registry(&self) -> bool {
        self.quality_gates
            .iter()
            .any(QualityGatePolicy::uses_js_package_manager)
    }
}

impl QualityGatePolicy {
    pub fn uses_js_package_manager(&self) -> bool {
        self.command.split_whitespace().any(|token| {
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
    }

    /// A JS package-manager gate that runs in the repository root cannot apply to a
    /// repository without a root `package.json`; no coding change can make it pass.
    /// Gates that change directory may target a nested project and always apply.
    pub fn applies_to(&self, repo_root: &std::path::Path) -> bool {
        if !self.uses_js_package_manager() || repo_root.join("package.json").is_file() {
            return true;
        }
        self.command.split_whitespace().any(|token| {
            matches!(token, "cd" | "-C" | "--prefix" | "--dir" | "--cwd")
                || token.starts_with("--prefix=")
                || token.starts_with("--dir=")
                || token.starts_with("--cwd=")
        })
    }
}

impl ExecutionManifest {
    pub fn fresh_start(&self) -> ManifestResult<bool> {
        self.fresh_start_impl().map_err(|error| {
            ManifestValidationError::from_anyhow(ManifestError::InvalidSignature, error)
        })
    }

    fn fresh_start_impl(&self) -> Result<bool> {
        match self.run.metadata.get("fresh_start") {
            None => Ok(false),
            Some(serde_json::Value::Bool(value)) => Ok(*value),
            Some(_) => bail!("execution manifest metadata.fresh_start must be a boolean"),
        }
    }

    pub fn resume_from_run_id(&self) -> ManifestResult<Option<&str>> {
        self.resume_from_run_id_impl().map_err(|error| {
            ManifestValidationError::from_anyhow(ManifestError::InvalidSignature, error)
        })
    }

    fn resume_from_run_id_impl(&self) -> Result<Option<&str>> {
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

    pub fn validate(&self, run_id: &str, ticket_id: &str) -> ManifestResult<()> {
        self.validate_impl(run_id, ticket_id).map_err(|error| {
            ManifestValidationError::from_anyhow(ManifestError::InvalidSignature, error)
        })
    }

    fn validate_impl(&self, run_id: &str, ticket_id: &str) -> Result<()> {
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
        if let Some(architecture) = &self.architecture {
            architecture
                .validate(run_id)
                .context("execution manifest contains an invalid architecture contract")?;
        }
        self.repo_config()?;
        self.normalized_required_workflows()?;
        Ok(())
    }

    pub fn policy(&self) -> ManifestResult<ExecutionPolicy> {
        self.policy_impl().map_err(|error| {
            ManifestValidationError::from_anyhow(ManifestError::InvalidPolicy, error)
        })
    }

    fn policy_impl(&self) -> Result<ExecutionPolicy> {
        serde_json::from_value(self.execution_policy.clone())
            .context("execution manifest contains an invalid execution policy")
    }

    pub fn repo_config(&self) -> ManifestResult<RepoConfig> {
        self.repo_config_impl().map_err(|error| {
            ManifestValidationError::from_anyhow(ManifestError::InvalidSignature, error)
        })
    }

    fn repo_config_impl(&self) -> Result<RepoConfig> {
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

    pub fn normalized_required_workflows(&self) -> ManifestResult<Vec<String>> {
        self.normalized_required_workflows_impl().map_err(|error| {
            ManifestValidationError::from_anyhow(ManifestError::InvalidPolicy, error)
        })
    }

    fn normalized_required_workflows_impl(&self) -> Result<Vec<String>> {
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
    pub fn validate(&self) -> ManifestResult<()> {
        self.validate_impl().map_err(|error| {
            ManifestValidationError::from_anyhow(ManifestError::InvalidPolicy, error)
        })
    }

    fn validate_impl(&self) -> Result<()> {
        let agent = &self.agent;
        if !matches!(self.policy_version, 1 | 2)
            || (self.policy_version == 1 && agent.provider != AgentProvider::Codex)
            || !(1..=86_400).contains(&self.timeout_seconds)
            || agent.command.is_empty()
            || agent.command.len() > 256
            || self.quality_gates.len() > 100
            || agent.environment_allowlist.len() > 128
        {
            bail!("unsupported or incomplete execution policy");
        }
        if agent.command.iter().any(|value| value.trim().is_empty())
            || agent
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
        match agent.provider {
            AgentProvider::Codex => validate_codex_command(&agent.command)?,
            AgentProvider::Claude => validate_claude_command(&agent.command)?,
        }
        if agent
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

    /// The hardened agent command line. The worker, not the signed policy, owns
    /// the sandbox, approval, and session-persistence flags for every provider.
    pub fn agent_args(
        &self,
        externally_isolated: bool,
        image_paths: &[std::path::PathBuf],
    ) -> Vec<String> {
        match self.agent.provider {
            AgentProvider::Codex => self.codex_args(externally_isolated, image_paths),
            AgentProvider::Claude => self.claude_args(externally_isolated),
        }
    }

    fn codex_args(
        &self,
        externally_isolated: bool,
        image_paths: &[std::path::PathBuf],
    ) -> Vec<String> {
        let mut command = self.agent.command.clone();
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
        if externally_isolated {
            // Docker Sandboxes inject the host-held ChatGPT OAuth credential through
            // an HTTPS proxy. Codex's WebSocket transport is not reliable through
            // that interception layer, while api.openai.com requires API-key scopes
            // that the OAuth credential intentionally does not have. Force the
            // OAuth-compatible HTTPS/SSE endpoint for Docker-isolated worker runs.
            for override_value in [
                "model_provider=\"rustgrid_chatgpt_http\"",
                "model_providers.rustgrid_chatgpt_http.name=\"RustGrid ChatGPT HTTP\"",
                "model_providers.rustgrid_chatgpt_http.base_url=\"https://chatgpt.com/backend-api/codex\"",
                "model_providers.rustgrid_chatgpt_http.wire_api=\"responses\"",
                "model_providers.rustgrid_chatgpt_http.requires_openai_auth=true",
                "model_providers.rustgrid_chatgpt_http.supports_websockets=false",
            ] {
                let insertion = command
                    .iter()
                    .position(|part| part == "-")
                    .unwrap_or(command.len());
                command.splice(insertion..insertion, ["-c".into(), override_value.into()]);
            }
        }
        command
    }

    /// Claude Code reads the prompt from stdin in print mode and streams
    /// `stream-json` events. Docker Sandboxes are the isolation boundary, so
    /// Claude bypasses its own permission prompts there, mirroring Codex's
    /// `danger-full-access`. Local development keeps Claude Code's OS sandbox
    /// with writes confined to the workspace, mirroring Codex `workspace-write`,
    /// and auto-approves sandboxed shell commands because nobody can answer a
    /// permission prompt.
    fn claude_args(&self, externally_isolated: bool) -> Vec<String> {
        let mut command = self.agent.command.clone();
        if !command.iter().any(|part| part == "-p" || part == "--print") {
            command.insert(1.min(command.len()), "--print".into());
        }
        if !command
            .iter()
            .any(|part| part == "--output-format" || part.starts_with("--output-format="))
        {
            command.extend(["--output-format".into(), "stream-json".into()]);
        }
        for flag in ["--verbose", "--no-session-persistence"] {
            if !command.iter().any(|part| part == flag) {
                command.push(flag.into());
            }
        }
        if externally_isolated {
            command.extend(["--permission-mode".into(), "bypassPermissions".into()]);
        } else {
            command.extend([
                "--permission-mode".into(),
                "acceptEdits".into(),
                "--settings".into(),
                CLAUDE_LOCAL_SANDBOX_SETTINGS.into(),
            ]);
        }
        command
    }
}

/// Claude Code settings for local (not externally isolated) execution: shell
/// commands run inside the OS sandbox with writes confined to the workspace and
/// no unsandboxed escape hatch.
const CLAUDE_LOCAL_SANDBOX_SETTINGS: &str = r#"{"sandbox":{"enabled":true,"autoAllowBashIfSandboxed":true,"allowUnsandboxedCommands":false}}"#;

fn validate_codex_command(command: &[String]) -> Result<()> {
    if command.iter().any(|value| {
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
    }) {
        bail!("execution policy contains an empty command, gate id, or timeout");
    }
    Ok(())
}

/// Rejects Claude Code flags that would widen permissions, load host or
/// caller-controlled configuration, resume another session, or change the
/// stream format the worker parses.
fn validate_claude_command(command: &[String]) -> Result<()> {
    let executable = std::path::Path::new(&command[0])
        .file_name()
        .and_then(|name| name.to_str());
    if executable != Some(AgentProvider::Claude.executable()) {
        bail!("Claude execution policy must invoke the claude executable");
    }
    let mut arguments = command[1..].iter().peekable();
    while let Some(argument) = arguments.next() {
        let lower = argument.to_ascii_lowercase();
        let (flag, inline_value) = match lower.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (lower.as_str(), None),
        };
        let forbidden = matches!(
            flag,
            "--dangerously-skip-permissions"
                | "--allow-dangerously-skip-permissions"
                | "--permission-mode"
                | "--permission-prompt-tool"
                | "--settings"
                | "--setting-sources"
                | "--allowedtools"
                | "--allowed-tools"
                | "--add-dir"
                | "--mcp-config"
                | "--plugin-dir"
                | "--plugin-url"
                | "--agent"
                | "--agents"
                | "--resume"
                | "-r"
                | "--continue"
                | "-c"
                | "--fork-session"
                | "--session-id"
                | "--from-pr"
                | "--teleport"
                | "--remote-control"
                | "--cloud"
                | "--environment"
                | "--bg"
                | "--background"
                | "--worktree"
                | "-w"
                | "--tmux"
                | "--ide"
                | "--chrome"
                | "--input-format"
                | "--fallback-model"
        );
        if forbidden {
            bail!("Claude execution policy contains a worker-owned or unsafe flag: {flag}");
        }
        if flag == "--output-format" {
            let value = inline_value
                .map(str::to_owned)
                .or_else(|| arguments.next().map(|value| value.to_ascii_lowercase()));
            if value.as_deref() != Some("stream-json") {
                bail!("Claude execution policy must use --output-format stream-json");
            }
        }
    }
    Ok(())
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

    fn gate(command: &str) -> QualityGatePolicy {
        QualityGatePolicy {
            id: "gate-1".into(),
            command: command.into(),
            timeout_seconds: 60,
            required: true,
        }
    }

    #[test]
    fn js_gates_do_not_apply_to_a_repository_without_package_json() {
        let repo = tempfile::tempdir().unwrap();

        for command in ["npm ci --maxsockets=1", "npm test", "npx tsc", "yarn build"] {
            assert!(!gate(command).applies_to(repo.path()), "{command}");
        }
        assert!(gate("cargo test").applies_to(repo.path()));
        assert!(gate("cd web && npm ci").applies_to(repo.path()));
        assert!(gate("npm --prefix web test").applies_to(repo.path()));

        std::fs::write(repo.path().join("package.json"), "{}").unwrap();
        assert!(gate("npm test").applies_to(repo.path()));
    }

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
            architecture: None,
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
    fn agent_idle_timeout_is_bounded_by_the_signed_run_policy() {
        let policy = manifest().policy().unwrap();
        assert_eq!(
            policy.agent_idle_timeout(),
            std::time::Duration::from_secs(300)
        );

        let mut configured = manifest().policy().unwrap();
        configured.agent.idle_timeout_seconds = Some(120);
        assert_eq!(
            configured.agent_idle_timeout(),
            std::time::Duration::from_secs(120)
        );
        assert!(configured.validate().is_ok());

        configured.agent.idle_timeout_seconds = Some(configured.timeout_seconds + 1);
        assert!(configured.validate().is_err());
    }

    #[test]
    fn hardens_the_codex_command() {
        let policy: ExecutionPolicy = serde_json::from_value(manifest().execution_policy).unwrap();
        let args = policy.agent_args(false, &[]);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "workspace-write"])
        );
        assert!(args.iter().any(|arg| arg.contains("approval_policy")));
        assert!(args.iter().any(|arg| arg == "--ephemeral"));
        assert!(!args.windows(2).any(|pair| pair[0] == "--disable"));
        assert!(!args.iter().any(|arg| {
            arg.starts_with("tool_output_token_limit=")
                || arg.starts_with("model_auto_compact_token_limit=")
                || arg.starts_with("project_doc_max_bytes=")
        }));
        let image_args = policy.agent_args(
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
                .agent_args(true, &[])
                .windows(2)
                .any(|pair| pair == ["--sandbox", "danger-full-access"])
        );
        let docker_args = policy.agent_args(true, &[]);
        assert!(
            docker_args
                .iter()
                .any(|arg| arg == "model_provider=\"rustgrid_chatgpt_http\"")
        );
        assert!(docker_args.iter().any(|arg| {
            arg == "model_providers.rustgrid_chatgpt_http.supports_websockets=false"
        }));
        assert!(docker_args.iter().any(|arg| {
            arg == "model_providers.rustgrid_chatgpt_http.base_url=\"https://chatgpt.com/backend-api/codex\""
        }));
        assert!(
            !args
                .iter()
                .any(|arg| arg == "model_provider=\"rustgrid_chatgpt_http\"")
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

    fn claude_policy() -> serde_json::Value {
        serde_json::json!({
            "policy_version": 2,
            "agent": {
                "provider": "claude",
                "command": ["claude", "-p", "--output-format", "stream-json", "--verbose", "--model", "claude-opus-5-5"],
                "environment_allowlist": ["PATH", "HOME"]
            },
            "quality_gates": [{"id":"gate-1","command":"cargo test","timeout_seconds":900,"required":true}],
            "timeout_seconds": 3600,
            "sandbox": {"mode":"workspace_write","network_access":true,"writable_roots":["."],"approval_policy":"never"}
        })
    }

    #[test]
    fn version_one_policies_round_trip_byte_for_byte() {
        let value = manifest().execution_policy;
        let policy: ExecutionPolicy = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(policy.agent.provider, AgentProvider::Codex);
        assert_eq!(serde_json::to_value(&policy).unwrap(), value);
    }

    #[test]
    fn accepts_provider_tagged_version_two_policies() {
        let value = claude_policy();
        let policy: ExecutionPolicy = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(policy.agent.provider, AgentProvider::Claude);
        assert!(policy.validate().is_ok());
        assert_eq!(serde_json::to_value(&policy).unwrap(), value);

        let mut codex = claude_policy();
        codex["agent"] = serde_json::json!({
            "provider": "codex",
            "command": ["codex", "exec", "--json"],
            "environment_allowlist": ["PATH"]
        });
        let policy: ExecutionPolicy = serde_json::from_value(codex).unwrap();
        assert_eq!(policy.agent.provider, AgentProvider::Codex);
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn rejects_mismatched_or_unknown_policy_shapes() {
        let mut v1_with_agent = manifest().execution_policy;
        v1_with_agent["agent"] = claude_policy()["agent"].clone();
        assert!(serde_json::from_value::<ExecutionPolicy>(v1_with_agent).is_err());

        let mut v2_with_codex = claude_policy();
        v2_with_codex["codex"] = manifest().execution_policy["codex"].clone();
        assert!(serde_json::from_value::<ExecutionPolicy>(v2_with_codex).is_err());

        let mut future = claude_policy();
        future["policy_version"] = serde_json::json!(3);
        assert!(serde_json::from_value::<ExecutionPolicy>(future).is_err());

        let mut unknown_provider = claude_policy();
        unknown_provider["agent"]["provider"] = serde_json::json!("gemini");
        assert!(serde_json::from_value::<ExecutionPolicy>(unknown_provider).is_err());
    }

    #[test]
    fn hardens_the_claude_command() {
        let policy: ExecutionPolicy = serde_json::from_value(claude_policy()).unwrap();
        let local = policy.agent_args(false, &[]);
        assert_eq!(local[0], "claude");
        assert!(
            local
                .windows(2)
                .any(|pair| pair == ["--model", "claude-opus-5-5"])
        );
        assert!(local.iter().any(|arg| arg == "--no-session-persistence"));
        assert!(
            local
                .windows(2)
                .any(|pair| pair == ["--permission-mode", "acceptEdits"])
        );
        let settings = local
            .windows(2)
            .find(|pair| pair[0] == "--settings")
            .map(|pair| serde_json::from_str::<serde_json::Value>(&pair[1]).unwrap())
            .expect("local Claude runs carry sandbox settings");
        assert_eq!(settings["sandbox"]["enabled"], true);
        assert_eq!(settings["sandbox"]["allowUnsandboxedCommands"], false);

        let isolated = policy.agent_args(true, &[std::path::PathBuf::from("shot.png")]);
        assert!(
            isolated
                .windows(2)
                .any(|pair| pair == ["--permission-mode", "bypassPermissions"])
        );
        assert!(
            !isolated
                .iter()
                .any(|arg| arg == "--settings" || arg == "--image")
        );

        let mut minimal = claude_policy();
        minimal["agent"]["command"] = serde_json::json!(["claude"]);
        let minimal: ExecutionPolicy = serde_json::from_value(minimal).unwrap();
        let args = minimal.agent_args(true, &[]);
        assert_eq!(args[1], "--print");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--output-format", "stream-json"])
        );
        assert!(args.iter().any(|arg| arg == "--verbose"));
    }

    #[test]
    fn rejects_caller_supplied_claude_permission_and_session_overrides() {
        for command in [
            vec!["claude", "-p", "--dangerously-skip-permissions"],
            vec!["claude", "-p", "--permission-mode", "bypassPermissions"],
            vec!["claude", "-p", "--permission-mode=plan"],
            vec!["claude", "-p", "--settings", "{}"],
            vec!["claude", "-p", "--setting-sources", "user"],
            vec!["claude", "-p", "--allowedTools", "Bash"],
            vec!["claude", "-p", "--add-dir", "/"],
            vec!["claude", "-p", "--mcp-config", "servers.json"],
            vec!["claude", "-p", "--resume", "session"],
            vec!["claude", "-p", "-c"],
            vec!["claude", "-p", "--output-format", "text"],
            vec!["claude", "-p", "--output-format=json"],
            vec!["claude", "-p", "--output-format"],
            vec!["claude", "-p", "--input-format", "stream-json"],
            vec!["claude", "-p", "--fallback-model", "claude-haiku-5-5"],
            vec!["codex", "exec"],
            vec!["/usr/bin/env", "claude", "-p"],
        ] {
            let mut value = claude_policy();
            value["agent"]["command"] = serde_json::json!(command);
            let policy: ExecutionPolicy = serde_json::from_value(value).unwrap();
            assert!(policy.validate().is_err(), "accepted command: {command:?}");
        }
        let mut absolute = claude_policy();
        absolute["agent"]["command"] = serde_json::json!(["/usr/local/bin/claude", "-p"]);
        let policy: ExecutionPolicy = serde_json::from_value(absolute).unwrap();
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn version_one_policies_cannot_select_claude() {
        let mut policy: ExecutionPolicy =
            serde_json::from_value(manifest().execution_policy).unwrap();
        policy.agent.provider = AgentProvider::Claude;
        policy.agent.command = vec!["claude".into(), "-p".into()];
        assert!(policy.validate().is_err());
    }
}
