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
};

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
    pub max_output_bytes: usize,
    pub environment_allowlist: &'a [String],
    pub limits: Option<command::ChildLimits>,
    pub max_workspace_bytes: u64,
}

impl Executor {
    pub(crate) fn from_config(config: &ExecutorConfig) -> Self {
        match config {
            ExecutorConfig::Local => Self::Local,
            ExecutorConfig::DockerSandbox {
                command,
                template,
                cpus,
                memory,
                ..
            } => Self::DockerSandbox {
                command: command.clone(),
                template: template.clone(),
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
        }
        Ok(())
    }

    pub(crate) fn reconcile_orphans(
        &self,
        active_run_ids: &HashSet<String>,
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
        let active_names = active_run_ids
            .iter()
            .map(|id| sandbox_name(id))
            .collect::<HashSet<_>>();
        let mut removed = 0;
        for name in orphan_sandbox_names(&value, &active_names) {
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

    pub(crate) fn prepare(&self, run_id: &str, workspace: &Path) -> Result<ExecutionHandle> {
        match self {
            Self::Local => Ok(ExecutionHandle::Local),
            Self::DockerSandbox {
                command,
                template,
                cpus,
                memory,
            } => {
                let started = Instant::now();
                let name = sandbox_name(run_id);
                // The name is deterministic, so a coordinator restart can remove an
                // orphan even if it crashed before persisting the create result.
                let _ = command::capture_with_env(
                    command,
                    ["rm", "--force", &name],
                    workspace,
                    std::iter::empty::<(&str, &str)>(),
                );
                let args = vec![
                    "create".into(),
                    "--quiet".into(),
                    "--name".into(),
                    name.clone(),
                    "--template".into(),
                    template.clone(),
                    "--cpus".into(),
                    cpus.to_string(),
                    "--memory".into(),
                    memory.clone(),
                    "codex".into(),
                    workspace.display().to_string(),
                ];
                let output = command::capture_with_env(
                    command,
                    &args,
                    workspace,
                    std::iter::empty::<(&str, &str)>(),
                )?;
                if !output.status.success() {
                    bail!(
                        "could not create Docker Sandbox {name}: {}",
                        output.stderr.trim()
                    );
                }
                console_event(
                    "sandbox",
                    &format!("created {name} in {}ms", started.elapsed().as_millis()),
                    "36",
                );
                Ok(ExecutionHandle::DockerSandbox { name })
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

    fn stop(&self, handle: &ExecutionHandle, cwd: &Path) {
        let (Self::DockerSandbox { command, .. }, ExecutionHandle::DockerSandbox { name }) =
            (self, handle)
        else {
            return;
        };
        let _ = command::capture_with_env(
            command,
            ["stop", name],
            cwd,
            std::iter::empty::<(&str, &str)>(),
        );
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
}
