use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::{
    command::{self, CommandOutput, StreamingCommand},
    config::ExecutorConfig,
    reporting::console_event,
    run_error::RunFailure,
};

const SANDBOX_NETWORK_ATTEMPTS: u32 = 3;
const NPM_REGISTRY: &str = "https://registry.npmjs.org";

#[derive(Clone, Debug)]
pub(crate) enum ExecutionHandle {
    Local,
    DockerSandbox { name: String },
}

impl ExecutionHandle {
    pub(crate) fn id(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::DockerSandbox { name } => Some(name),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Executor {
    Local,
    DockerSandbox {
        command: String,
        template: String,
        codex_version: String,
        cpus: u16,
        memory: String,
    },
}

pub(crate) struct RunCommand<'a> {
    pub args: &'a [String],
    pub cwd: &'a Path,
    pub stdin_text: Option<&'a str>,
    pub running: &'a AtomicBool,
    pub timeout: Duration,
    pub idle_timeout: Option<Duration>,
    pub output_is_activity: Option<fn(&str) -> bool>,
    pub max_output_bytes: usize,
    pub environment_allowlist: &'a [String],
    pub limits: Option<command::ChildLimits>,
    pub max_workspace_bytes: u64,
}

impl Executor {
    pub(crate) fn sandbox_name_for_run(run_id: &str) -> String {
        sandbox_name(run_id)
    }

    pub(crate) fn from_config(config: &ExecutorConfig) -> Self {
        match config {
            ExecutorConfig::Local => Self::Local,
            ExecutorConfig::DockerSandbox {
                command,
                template,
                codex_version,
                cpus,
                memory,
                ..
            } => Self::DockerSandbox {
                command: command.clone(),
                template: template.clone(),
                codex_version: codex_version.clone(),
                cpus: *cpus,
                memory: memory.clone(),
            },
        }
    }

    pub(crate) fn preflight(&self, cwd: &Path) -> Result<()> {
        if let Self::DockerSandbox { command, .. } = self {
            fs::create_dir_all(cwd).with_context(|| {
                format!("could not create executor workspace root {}", cwd.display())
            })?;
            let output = command::capture_with_env(
                command,
                ["version"],
                cwd,
                std::iter::empty::<(&str, &str)>(),
            )
            .context("could not invoke Docker Sandboxes CLI")?;
            if !output.status.success() {
                bail!(
                    "Docker Sandboxes CLI preflight failed: {}",
                    output.stderr.trim()
                );
            }
            let version = parse_sbx_version(&output.stdout)
                .context("could not parse Docker Sandboxes CLI version")?;
            if version < (0, 34, 0) {
                bail!("Docker Sandboxes CLI 0.34.0 or newer is required");
            }
            let daemon = command::capture_with_env(
                command,
                ["ls", "--json"],
                cwd,
                std::iter::empty::<(&str, &str)>(),
            )
            .context("could not contact the Docker Sandboxes daemon")?;
            if !daemon.status.success() {
                bail!(
                    "Docker Sandboxes daemon preflight failed: {}",
                    daemon.stderr.trim()
                );
            }
            let policies = command::capture_with_env(
                command,
                ["policy", "ls"],
                cwd,
                std::iter::empty::<(&str, &str)>(),
            )
            .context("could not inspect Docker Sandbox network policy")?;
            if !policies.status.success() {
                bail!(
                    "Docker Sandbox network policy is unavailable: {}",
                    policies.stderr.trim()
                );
            }
            let policy_text = policies.stdout.to_ascii_lowercase();
            if !policy_text.contains("network")
                || !(policy_text.contains("allow") || policy_text.contains("deny"))
            {
                bail!("Docker Sandbox has no inspectable effective network policy");
            }
            let local_kits = command::capture_with_env(
                command,
                ["settings", "get", "kit.allowLocalKits"],
                cwd,
                std::iter::empty::<(&str, &str)>(),
            )
            .context("could not inspect Docker Sandbox local-kit policy")?;
            if !local_kits.status.success() || local_kits.stdout.trim() != "true" {
                bail!(
                    "Docker Sandbox local kits must be enabled to enforce the pinned Codex version"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn reconcile_orphans(
        &self,
        protected_sandbox_names: &HashSet<String>,
        cwd: &Path,
    ) -> Result<usize> {
        let Self::DockerSandbox { command, .. } = self else {
            return Ok(0);
        };
        let output = command::capture_with_env(
            command,
            ["ls", "--json"],
            cwd,
            std::iter::empty::<(&str, &str)>(),
        )?;
        if !output.status.success() {
            bail!(
                "could not list Docker Sandboxes for orphan reconciliation: {}",
                output.stderr.trim()
            );
        }
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).context("sbx ls --json returned invalid JSON")?;
        let mut removed = 0;
        for name in orphan_sandbox_names(&value, protected_sandbox_names) {
            let cleanup = command::capture_with_env(
                command,
                ["rm", "--force", &name],
                cwd,
                std::iter::empty::<(&str, &str)>(),
            )?;
            if !cleanup.status.success() {
                bail!(
                    "could not remove orphan Docker Sandbox {name}: {}",
                    cleanup.stderr.trim()
                );
            }
            removed += 1;
        }
        Ok(removed)
    }

    pub(crate) fn prepare(
        &self,
        run_id: &str,
        workspace: &Path,
        probe_npm_registry: bool,
        retained_sandbox_name: Option<&str>,
    ) -> Result<ExecutionHandle> {
        match self {
            Self::Local => Ok(ExecutionHandle::Local),
            Self::DockerSandbox {
                command,
                template,
                codex_version,
                cpus,
                memory,
            } => {
                let codex_kit = ensure_codex_version_kit(workspace, codex_version)?;
                let name = retained_sandbox_name
                    .map(validate_managed_sandbox_name)
                    .transpose()?
                    .map_or_else(|| sandbox_name(run_id), str::to_owned);
                if sandbox_exists(command, &name, workspace)? {
                    console_event(
                        "recovery",
                        &format!("Reusing retained Docker Sandbox {name}"),
                        "36",
                    );
                    if let Err(error) = ensure_sandbox_codex_version(
                        command,
                        &name,
                        workspace,
                        codex_version,
                        &codex_kit,
                    ) {
                        return Err(RunFailure::InfrastructureTransient {
                            detail: format!(
                                "retained Docker Sandbox Codex upgrade failed; sandbox preserved: {error:#}"
                            ),
                        }
                        .into());
                    }
                    if probe_npm_registry {
                        let mut last_failure = String::new();
                        for attempt in 1..=SANDBOX_NETWORK_ATTEMPTS {
                            match probe_sandbox_network(command, &name, workspace) {
                                Ok(()) => {
                                    console_event(
                                        "completed",
                                        "Retained Docker Sandbox npm registry network preflight passed",
                                        "32",
                                    );
                                    return Ok(ExecutionHandle::DockerSandbox { name });
                                }
                                Err(error) => last_failure = format!("{error:#}"),
                            }
                            let _ = stop_sandbox(command, &name, workspace);
                            if attempt < SANDBOX_NETWORK_ATTEMPTS {
                                let delay = Duration::from_secs(u64::from(attempt));
                                eprintln!(
                                    "[warning] retained Docker Sandbox network check failed; restarting attempt {} of {} in {}s: {}",
                                    attempt + 1,
                                    SANDBOX_NETWORK_ATTEMPTS,
                                    delay.as_secs(),
                                    last_failure
                                );
                                thread::sleep(delay);
                            }
                        }
                        return Err(RunFailure::InfrastructureTransient {
                            detail: format!(
                                "retained Docker Sandbox network check failed after {SANDBOX_NETWORK_ATTEMPTS} attempts; sandbox preserved: {last_failure}"
                            ),
                        }
                        .into());
                    }
                    return Ok(ExecutionHandle::DockerSandbox { name });
                }
                let args = vec![
                    "create".into(),
                    "--quiet".into(),
                    "--name".into(),
                    name.clone(),
                    "--template".into(),
                    template.clone(),
                    "--kit".into(),
                    codex_kit.display().to_string(),
                    "--cpus".into(),
                    cpus.to_string(),
                    "--memory".into(),
                    memory.clone(),
                    "codex".into(),
                    workspace.display().to_string(),
                ];
                let mut last_failure = String::new();
                let attempts = SANDBOX_NETWORK_ATTEMPTS;
                for attempt in 1..=attempts {
                    let started = Instant::now();
                    // The name is deterministic, so a coordinator restart can remove an
                    // orphan even if it crashed before persisting the create result.
                    let _ = command::capture_with_env(
                        command,
                        ["rm", "--force", &name],
                        workspace,
                        std::iter::empty::<(&str, &str)>(),
                    );
                    let output = command::capture_with_env(
                        command,
                        &args,
                        workspace,
                        std::iter::empty::<(&str, &str)>(),
                    )?;
                    if output.status.success() {
                        console_event(
                            "sandbox",
                            &format!("created {name} in {}ms", started.elapsed().as_millis()),
                            "36",
                        );
                        match verify_sandbox_codex_version(command, &name, workspace, codex_version)
                        {
                            Ok(()) if !probe_npm_registry => {
                                return Ok(ExecutionHandle::DockerSandbox { name });
                            }
                            Ok(()) => match probe_sandbox_network(command, &name, workspace) {
                                Ok(()) => {
                                    console_event(
                                        "completed",
                                        "Docker Sandbox npm registry network preflight passed",
                                        "32",
                                    );
                                    return Ok(ExecutionHandle::DockerSandbox { name });
                                }
                                Err(error) => last_failure = format!("{error:#}"),
                            },
                            Err(error) => {
                                last_failure = format!("{error:#}");
                            }
                        }
                    } else {
                        last_failure = format!(
                            "could not create Docker Sandbox {name}: {}",
                            output.stderr.trim()
                        );
                    }
                    let _ = command::capture_with_env(
                        command,
                        ["rm", "--force", &name],
                        workspace,
                        std::iter::empty::<(&str, &str)>(),
                    );
                    if attempt < attempts {
                        let delay = Duration::from_secs(u64::from(attempt));
                        eprintln!(
                            "[warning] Docker Sandbox network admission failed; recreating attempt {} of {} in {}s: {}",
                            attempt + 1,
                            attempts,
                            delay.as_secs(),
                            last_failure
                        );
                        thread::sleep(delay);
                    }
                }
                Err(RunFailure::InfrastructureTransient {
                    detail: format!(
                        "Docker Sandbox Codex/network admission failed after {SANDBOX_NETWORK_ATTEMPTS} attempts: {last_failure}"
                    ),
                }
                .into())
            }
        }
    }

    pub(crate) fn streaming<F>(
        &self,
        handle: &ExecutionHandle,
        request: RunCommand<'_>,
        on_line: F,
    ) -> Result<ExitStatus>
    where
        F: FnMut(&str) -> Result<()>,
    {
        let mut args = request.args.to_vec();
        command::add_codex_json_flag(&mut args);
        let wrapped = self.wrap(
            handle,
            request.cwd,
            &args,
            request.environment_allowlist,
            request.stdin_text.is_some(),
        )?;
        let monitor = WorkspaceMonitor::start(
            self.clone(),
            handle.clone(),
            request.cwd,
            request.max_workspace_bytes,
        );
        let result = command::streaming_args(
            StreamingCommand {
                args: &wrapped.args,
                cwd: request.cwd,
                stdin_text: request.stdin_text,
                running: request.running,
                timeout: request.timeout,
                idle_timeout: request.idle_timeout,
                output_is_activity: request.output_is_activity,
                max_output_bytes: request.max_output_bytes,
                environment_allowlist: matches!(self, Self::Local)
                    .then_some(request.environment_allowlist),
                limits: matches!(self, Self::Local)
                    .then_some(request.limits)
                    .flatten(),
            },
            on_line,
        );
        let quota_exceeded = monitor.finish();
        if quota_exceeded {
            bail!(
                "sandbox workspace exceeded {} bytes",
                request.max_workspace_bytes
            );
        }
        if result.is_err() {
            self.stop(handle, request.cwd);
        }
        result
    }

    pub(crate) fn captured(
        &self,
        handle: &ExecutionHandle,
        command_text: &str,
        request: RunCommand<'_>,
    ) -> Result<CommandOutput> {
        let args = command::parse(command_text)?;
        let wrapped = self.wrap(
            handle,
            request.cwd,
            &args,
            request.environment_allowlist,
            false,
        )?;
        let monitor = WorkspaceMonitor::start(
            self.clone(),
            handle.clone(),
            request.cwd,
            request.max_workspace_bytes,
        );
        let result = command::capture_cancellable_with_environment(
            &shlex::try_join(wrapped.args.iter().map(String::as_str))
                .context("could not encode executor command")?,
            request.cwd,
            request.running,
            request.timeout,
            request.max_output_bytes,
            matches!(self, Self::Local).then_some(request.environment_allowlist),
            matches!(self, Self::Local)
                .then_some(request.limits)
                .flatten(),
        );
        let quota_exceeded = monitor.finish();
        if quota_exceeded {
            bail!(
                "sandbox workspace exceeded {} bytes",
                request.max_workspace_bytes
            );
        }
        if result.is_err() {
            self.stop(handle, request.cwd);
        }
        result
    }

    pub(crate) fn destroy(&self, handle: &ExecutionHandle, cwd: &Path) -> Result<()> {
        let (Self::DockerSandbox { command, .. }, ExecutionHandle::DockerSandbox { name }) =
            (self, handle)
        else {
            return Ok(());
        };
        let started = Instant::now();
        let output = command::capture_with_env(
            command,
            ["rm", "--force", name],
            cwd,
            std::iter::empty::<(&str, &str)>(),
        )?;
        if !output.status.success() {
            bail!(
                "could not destroy Docker Sandbox {name}: {}",
                output.stderr.trim()
            );
        }
        console_event(
            "sandbox",
            &format!("destroyed {name} in {}ms", started.elapsed().as_millis()),
            "36",
        );
        Ok(())
    }

    pub(crate) fn retain(&self, handle: &ExecutionHandle, cwd: &Path) -> Result<()> {
        let (Self::DockerSandbox { command, .. }, ExecutionHandle::DockerSandbox { name }) =
            (self, handle)
        else {
            return Ok(());
        };
        stop_sandbox(command, name, cwd)?;
        console_event(
            "retained",
            &format!("Stopped and retained Docker Sandbox {name} for recovery"),
            "33",
        );
        Ok(())
    }

    fn stop(&self, handle: &ExecutionHandle, cwd: &Path) {
        let (Self::DockerSandbox { command, .. }, ExecutionHandle::DockerSandbox { name }) =
            (self, handle)
        else {
            return;
        };
        let _ = stop_sandbox(command, name, cwd);
    }

    fn wrap(
        &self,
        handle: &ExecutionHandle,
        cwd: &Path,
        args: &[String],
        allowlist: &[String],
        interactive: bool,
    ) -> Result<PreparedCommand> {
        match (self, handle) {
            (Self::Local, ExecutionHandle::Local) => Ok(PreparedCommand {
                args: args.to_vec(),
                _env_file: None,
            }),
            (Self::DockerSandbox { command, .. }, ExecutionHandle::DockerSandbox { name }) => {
                let mut wrapped = vec![command.clone(), "exec".into()];
                if interactive {
                    wrapped.push("-i".into());
                }
                wrapped.extend(["-w".into(), cwd.display().to_string()]);
                wrapped.push(name.clone());
                let env_file = TemporaryEnvFile::create(cwd, allowlist)?;
                if let Some(file) = &env_file {
                    wrapped.extend([
                        "sh".into(),
                        "-c".into(),
                        "set -a; . \"$1\"; set +a; shift; exec \"$@\"".into(),
                        "rustgrid-env".into(),
                        file.path.display().to_string(),
                    ]);
                }
                wrapped.extend_from_slice(args);
                Ok(PreparedCommand {
                    args: wrapped,
                    _env_file: env_file,
                })
            }
            _ => bail!("executor handle does not match configured executor"),
        }
    }
}

fn sandbox_exists(command: &str, name: &str, cwd: &Path) -> Result<bool> {
    let output = command::capture_with_env(
        command,
        ["ls", "--json"],
        cwd,
        std::iter::empty::<(&str, &str)>(),
    )?;
    if !output.status.success() {
        bail!(
            "could not inspect retained Docker Sandboxes: {}",
            output.stderr.trim()
        );
    }
    let value: serde_json::Value =
        serde_json::from_str(&output.stdout).context("sbx ls --json returned invalid JSON")?;
    Ok(sandbox_names(&value)
        .iter()
        .any(|candidate| candidate == name))
}

fn ensure_codex_version_kit(workspace: &Path, version: &str) -> Result<PathBuf> {
    let run_root = workspace
        .parent()
        .context("sandbox workspace has no run directory")?;
    let kit_dir = run_root.join(format!("codex-{version}-kit"));
    fs::create_dir_all(&kit_dir)
        .with_context(|| format!("could not create Codex version kit {}", kit_dir.display()))?;
    let spec = format!(
        r#"schemaVersion: "1"
kind: mixin
name: rustgrid-codex-{name}
displayName: RustGrid Codex {version}
description: Pins the Codex CLI used by rustgrid-agent sandboxes
caps:
  network:
    allow:
      - registry.npmjs.org
environment:
  variables:
    NPM_CONFIG_FETCH_RETRIES: "5"
    NPM_CONFIG_FETCH_RETRY_MINTIMEOUT: "2000"
    NPM_CONFIG_FETCH_RETRY_MAXTIMEOUT: "30000"
    NPM_CONFIG_FETCH_TIMEOUT: "120000"
commands:
  install:
    - command: "npm install --global --no-audit --no-fund @openai/codex@{version}"
      user: "0"
      description: Install Codex CLI {version}
"#,
        name = version.replace('.', "-")
    );
    let path = kit_dir.join("spec.yaml");
    if path.is_file() {
        let existing = fs::read_to_string(&path)
            .with_context(|| format!("could not read Codex version kit {}", path.display()))?;
        if existing == spec {
            return Ok(kit_dir);
        }
    }
    let temporary = kit_dir.join(format!(".spec-{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| {
            format!(
                "could not create temporary Codex kit {}",
                temporary.display()
            )
        })?;
    file.write_all(spec.as_bytes())?;
    file.sync_all()?;
    #[cfg(windows)]
    if path.is_file() {
        fs::remove_file(&path).with_context(|| {
            format!(
                "could not replace stale Codex version kit {}",
                path.display()
            )
        })?;
    }
    fs::rename(&temporary, &path)
        .with_context(|| format!("could not publish Codex version kit {}", path.display()))?;
    Ok(kit_dir)
}

fn verify_sandbox_codex_version(
    command: &str,
    name: &str,
    workspace: &Path,
    expected: &str,
) -> Result<()> {
    let output = command::capture_with_env(
        command,
        ["exec", name, "codex", "--version"],
        workspace,
        std::iter::empty::<(&str, &str)>(),
    )?;
    if !output.status.success() {
        bail!(
            "could not verify Codex in Docker Sandbox {name}: {}",
            output.stderr.trim()
        );
    }
    let actual = output.stdout.trim();
    let required = format!("codex-cli {expected}");
    if actual != required {
        bail!("Docker Sandbox {name} has {actual}, required {required}");
    }
    Ok(())
}

fn ensure_sandbox_codex_version(
    command: &str,
    name: &str,
    workspace: &Path,
    expected: &str,
    kit: &Path,
) -> Result<()> {
    if verify_sandbox_codex_version(command, name, workspace, expected).is_ok() {
        return Ok(());
    }
    let output = command::capture_with_env(
        command,
        ["kit", "add", name, kit.to_string_lossy().as_ref()],
        workspace,
        std::iter::empty::<(&str, &str)>(),
    )?;
    if !output.status.success() {
        bail!(
            "could not upgrade retained Docker Sandbox {name} to Codex {expected}: {}",
            output.stderr.trim()
        );
    }
    verify_sandbox_codex_version(command, name, workspace, expected)?;
    console_event(
        "completed",
        &format!("Upgraded retained Docker Sandbox to Codex {expected}"),
        "32",
    );
    Ok(())
}

fn stop_sandbox(command: &str, name: &str, cwd: &Path) -> Result<()> {
    let output = command::capture_with_env(
        command,
        ["stop", name],
        cwd,
        std::iter::empty::<(&str, &str)>(),
    )?;
    if !output.status.success() {
        bail!(
            "could not stop Docker Sandbox {name}: {}",
            output.stderr.trim()
        );
    }
    Ok(())
}

fn sandbox_network_probe_args(name: &str, workspace: &Path) -> Vec<String> {
    vec![
        "exec".into(),
        "-w".into(),
        workspace.display().to_string(),
        name.into(),
        "npm".into(),
        "ping".into(),
        format!("--registry={NPM_REGISTRY}"),
        "--fetch-retries=0".into(),
        "--fetch-timeout=10000".into(),
    ]
}

fn probe_sandbox_network(command: &str, name: &str, workspace: &Path) -> Result<()> {
    let args = sandbox_network_probe_args(name, workspace);
    let output = command::capture_with_env(
        command,
        &args,
        workspace,
        std::iter::empty::<(&str, &str)>(),
    )?;
    if !output.status.success() {
        let detail = if output.stderr.trim().is_empty() {
            output.stdout.trim()
        } else {
            output.stderr.trim()
        };
        bail!("npm registry network probe failed in Docker Sandbox {name}: {detail}");
    }
    Ok(())
}

struct PreparedCommand {
    args: Vec<String>,
    // Kept alive until the sbx client has finished reading it.
    _env_file: Option<TemporaryEnvFile>,
}

struct TemporaryEnvFile {
    path: PathBuf,
}

impl TemporaryEnvFile {
    fn create(cwd: &Path, allowlist: &[String]) -> Result<Option<Self>> {
        let values = allowlist
            .iter()
            .filter(|key| !is_sandbox_environment_control(key))
            .filter_map(|key| std::env::var(key).ok().map(|value| (key, value)))
            .collect::<Vec<_>>();
        if values.is_empty() {
            return Ok(None);
        }
        let git_directory = cwd.join(".git");
        if !git_directory.is_dir() {
            bail!(
                "run workspace has no Git metadata directory for protected sandbox environment transport"
            );
        }
        let path = git_directory.join(format!("rustgrid-sandbox-env-{}", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .with_context(|| format!("could not create {}", path.display()))?;
        for (key, value) in values {
            if value.contains('\n') || value.contains('\r') {
                bail!("environment variable {key} contains a newline");
            }
            if !key.chars().enumerate().all(|(index, character)| {
                character == '_'
                    || character.is_ascii_alphanumeric()
                        && (index > 0 || !character.is_ascii_digit())
            }) {
                bail!("environment variable name {key} is not shell-safe");
            }
            writeln!(file, "{key}={}", shell_single_quote(&value))?;
        }
        file.sync_all()?;
        Ok(Some(Self { path }))
    }
}

fn is_sandbox_environment_control(name: &str) -> bool {
    let normalized = name.trim().to_ascii_uppercase();
    matches!(
        normalized.as_str(),
        "PATH"
            | "HOME"
            | "SHELL"
            | "ENV"
            | "BASH_ENV"
            | "ZDOTDIR"
            | "CDPATH"
            | "IFS"
            | "TMPDIR"
            | "PYTHONPATH"
            | "NODE_OPTIONS"
            | "RUBYOPT"
            | "PERL5OPT"
            | "RUSTC_WRAPPER"
            | "RUSTDOC_WRAPPER"
            | "CARGO_HOME"
            | "RUSTUP_HOME"
    ) || normalized.starts_with("LD_")
        || normalized.starts_with("DYLD_")
        || normalized.starts_with("GIT_CONFIG")
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

impl Drop for TemporaryEnvFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct WorkspaceMonitor {
    stop: Arc<AtomicBool>,
    exceeded: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl WorkspaceMonitor {
    fn start(executor: Executor, handle: ExecutionHandle, cwd: &Path, max_bytes: u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let exceeded = Arc::new(AtomicBool::new(false));
        if !matches!(executor, Executor::DockerSandbox { .. }) {
            return Self {
                stop,
                exceeded,
                thread: None,
            };
        }
        let thread_stop = Arc::clone(&stop);
        let thread_exceeded = Arc::clone(&exceeded);
        let path = cwd.to_path_buf();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                if crate::workspace::directory_size(&path).is_ok_and(|size| size > max_bytes) {
                    thread_exceeded.store(true, Ordering::SeqCst);
                    console_event(
                        "quota",
                        &format!(
                            "sandbox workspace exceeded {max_bytes} bytes; stopping execution"
                        ),
                        "31",
                    );
                    executor.stop(&handle, &path);
                    break;
                }
                thread::sleep(Duration::from_millis(500));
            }
        });
        Self {
            stop,
            exceeded,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> bool {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.exceeded.load(Ordering::SeqCst)
    }
}

impl Drop for WorkspaceMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn sandbox_name(run_id: &str) -> String {
    let digest = hex::encode(Sha256::digest(run_id.as_bytes()));
    format!("rustgrid-{}", &digest[..32])
}

fn validate_managed_sandbox_name(name: &str) -> Result<&str> {
    let digest = name
        .strip_prefix("rustgrid-")
        .filter(|digest| digest.len() == 32 && digest.chars().all(|c| c.is_ascii_hexdigit()))
        .context("recovery journal contains an invalid managed sandbox name")?;
    let _ = digest;
    Ok(name)
}

fn sandbox_names(value: &serde_json::Value) -> Vec<String> {
    fn collect(value: &serde_json::Value, names: &mut Vec<String>) {
        match value {
            serde_json::Value::Array(entries) => {
                entries.iter().for_each(|entry| collect(entry, names))
            }
            serde_json::Value::Object(object) => {
                if let Some(name) = object
                    .get("name")
                    .or_else(|| object.get("Name"))
                    .and_then(serde_json::Value::as_str)
                {
                    names.push(name.to_owned());
                }
                object.values().for_each(|entry| collect(entry, names));
            }
            _ => {}
        }
    }
    let mut names = Vec::new();
    collect(value, &mut names);
    names.sort();
    names.dedup();
    names
}

fn orphan_sandbox_names(value: &serde_json::Value, active: &HashSet<String>) -> Vec<String> {
    sandbox_names(value)
        .into_iter()
        .filter(|name| name.starts_with("rustgrid-") && !active.contains(name))
        .collect()
}

fn parse_sbx_version(output: &str) -> Option<(u64, u64, u64)> {
    output.split_whitespace().find_map(|word| {
        let candidate = word
            .trim_start_matches('v')
            .trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        let mut parts = candidate.split('.');
        Some((
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_names_are_stable_and_safe() {
        assert_eq!(sandbox_name("run/123?").len(), 41);
        assert_eq!(sandbox_name("run/123?"), sandbox_name("run/123?"));
        assert_ne!(sandbox_name("run/123?"), sandbox_name("run-123"));
    }

    #[test]
    fn validates_recovery_sandbox_names_before_reuse() {
        assert!(validate_managed_sandbox_name("rustgrid-0123456789abcdef0123456789abcdef").is_ok());
        assert!(validate_managed_sandbox_name("developer-box").is_err());
        assert!(validate_managed_sandbox_name("rustgrid-../../escape").is_err());
    }

    #[test]
    fn parses_supported_sbx_list_shapes() {
        assert_eq!(
            sandbox_names(&serde_json::json!([{"name":"rustgrid-a"}])),
            ["rustgrid-a"]
        );
        assert_eq!(
            sandbox_names(&serde_json::json!({"sandboxes":[{"Name":"rustgrid-b"}]})),
            ["rustgrid-b"]
        );
    }

    #[test]
    fn selects_only_unassigned_managed_sandboxes() {
        let active = HashSet::from(["rustgrid-active".to_owned()]);
        let listed = serde_json::json!({"items": [
            {"name":"rustgrid-active"}, {"name":"rustgrid-orphan"}, {"name":"developer-box"}
        ]});
        assert_eq!(orphan_sandbox_names(&listed, &active), ["rustgrid-orphan"]);
    }

    #[test]
    fn builds_non_shell_sbx_exec_invocation() {
        let executor = Executor::DockerSandbox {
            command: "sbx".into(),
            template: "unused".into(),
            codex_version: "0.144.4".into(),
            cpus: 1,
            memory: "1g".into(),
        };
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("repo");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(workspace.join(".git")).unwrap();
        let command = executor
            .wrap(
                &ExecutionHandle::DockerSandbox {
                    name: "rustgrid-test".into(),
                },
                &workspace,
                &["cargo".into(), "test".into()],
                &[],
                false,
            )
            .unwrap();
        assert_eq!(
            command.args[0..4],
            ["sbx", "exec", "-w", workspace.to_str().unwrap()]
        );
        assert_eq!(command.args[4..], ["rustgrid-test", "cargo", "test"]);

        let interactive = executor
            .wrap(
                &ExecutionHandle::DockerSandbox {
                    name: "rustgrid-test".into(),
                },
                &workspace,
                &["codex".into()],
                &[],
                true,
            )
            .unwrap();
        assert_eq!(
            interactive.args[0..5],
            ["sbx", "exec", "-i", "-w", workspace.to_str().unwrap()]
        );
    }

    #[test]
    fn npm_network_probe_uses_bounded_retries_and_the_public_registry() {
        let workspace = Path::new("/tmp/repo");
        assert_eq!(
            sandbox_network_probe_args("rustgrid-test", workspace),
            [
                "exec",
                "-w",
                "/tmp/repo",
                "rustgrid-test",
                "npm",
                "ping",
                "--registry=https://registry.npmjs.org",
                "--fetch-retries=0",
                "--fetch-timeout=10000",
            ]
        );
        assert_eq!(SANDBOX_NETWORK_ATTEMPTS, 3);
    }

    #[test]
    fn parses_and_compares_sbx_versions() {
        assert_eq!(
            parse_sbx_version("Client Version: v0.35.0 abc"),
            Some((0, 35, 0))
        );
        assert_eq!(parse_sbx_version("sbx 0.34.1"), Some((0, 34, 1)));
    }

    #[test]
    fn environment_file_is_private_and_removed() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("repo");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(workspace.join(".git")).unwrap();
        let key = format!("RUSTGRID_TEST_SECRET_{}", uuid::Uuid::new_v4().simple());
        // SAFETY: the unique name is not read by other tests or production code.
        unsafe { std::env::set_var(&key, "top-secret") };
        let temporary = TemporaryEnvFile::create(&workspace, std::slice::from_ref(&key))
            .unwrap()
            .unwrap();
        let path = temporary.path.clone();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{key}='top-secret'\n")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(temporary);
        assert!(!path.exists());
        // SAFETY: this removes the same test-unique environment name.
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn shell_quotes_environment_values_without_interpolation() {
        assert_eq!(shell_single_quote("a'b;$HOME"), "'a'\"'\"'b;$HOME'");
    }

    #[test]
    fn sandbox_environment_keeps_template_execution_paths() {
        for name in [
            "PATH",
            "HOME",
            "BASH_ENV",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "GIT_CONFIG_SYSTEM",
            "PYTHONPATH",
            "NODE_OPTIONS",
        ] {
            assert!(is_sandbox_environment_control(name), "accepted {name}");
        }
        assert!(!is_sandbox_environment_control("CI"));
    }

    #[test]
    fn codex_version_kit_is_exact_and_outside_the_agent_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("run/repo");
        fs::create_dir_all(&workspace).unwrap();

        let kit = ensure_codex_version_kit(&workspace, "0.144.4").unwrap();
        assert_eq!(kit.parent(), workspace.parent());
        assert!(!kit.starts_with(&workspace));
        let spec = fs::read_to_string(kit.join("spec.yaml")).unwrap();
        assert_eq!(
            spec,
            r#"schemaVersion: "1"
kind: mixin
name: rustgrid-codex-0-144-4
displayName: RustGrid Codex 0.144.4
description: Pins the Codex CLI used by rustgrid-agent sandboxes
caps:
  network:
    allow:
      - registry.npmjs.org
environment:
  variables:
    NPM_CONFIG_FETCH_RETRIES: "5"
    NPM_CONFIG_FETCH_RETRY_MINTIMEOUT: "2000"
    NPM_CONFIG_FETCH_RETRY_MAXTIMEOUT: "30000"
    NPM_CONFIG_FETCH_TIMEOUT: "120000"
commands:
  install:
    - command: "npm install --global --no-audit --no-fund @openai/codex@0.144.4"
      user: "0"
      description: Install Codex CLI 0.144.4
"#
        );
        assert_eq!(
            ensure_codex_version_kit(&workspace, "0.144.4").unwrap(),
            kit
        );
        fs::write(kit.join("spec.yaml"), "invalid: stale\n").unwrap();
        ensure_codex_version_kit(&workspace, "0.144.4").unwrap();
        assert!(
            fs::read_to_string(kit.join("spec.yaml"))
                .unwrap()
                .contains("NPM_CONFIG_FETCH_RETRIES: \"5\"")
        );
    }
}
