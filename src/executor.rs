use std::{path::Path, process::ExitStatus, sync::atomic::AtomicBool, time::Duration};

use anyhow::{Context, Result, bail};

use crate::{
    command::{self, CommandOutput, StreamingCommand},
    config::ExecutorConfig,
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
        }
        Ok(())
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
        let wrapped = self.wrap(handle, request.cwd, &args, request.environment_allowlist)?;
        let result = command::streaming_args(
            StreamingCommand {
                args: &wrapped,
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
        let wrapped = self.wrap(handle, request.cwd, &args, request.environment_allowlist)?;
        let result = command::capture_cancellable_with_environment(
            &shlex::try_join(wrapped.iter().map(String::as_str))
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
    ) -> Result<Vec<String>> {
        match (self, handle) {
            (Self::Local, ExecutionHandle::Local) => Ok(args.to_vec()),
            (Self::DockerSandbox { command, .. }, ExecutionHandle::DockerSandbox { name }) => {
                let mut wrapped = vec![
                    command.clone(),
                    "exec".into(),
                    "-i".into(),
                    "-w".into(),
                    cwd.display().to_string(),
                ];
                for key in allowlist {
                    if let Ok(value) = std::env::var(key) {
                        wrapped.extend(["-e".into(), format!("{key}={value}")]);
                    }
                }
                wrapped.push(name.clone());
                wrapped.extend_from_slice(args);
                Ok(wrapped)
            }
            _ => bail!("executor handle does not match configured executor"),
        }
    }
}

fn sandbox_name(run_id: &str) -> String {
    let suffix: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(48)
        .collect();
    format!("rustgrid-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_names_are_stable_and_safe() {
        assert_eq!(sandbox_name("run/123?"), "rustgrid-run123");
    }
}
