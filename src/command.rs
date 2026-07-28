use std::{
    ffi::OsStr,
    io::{Read, Write},
    path::Path,
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::shutdown;

#[cfg(target_os = "linux")]
use std::{fs, path::PathBuf, process::Child};

const CONTAINED_CHILD_MARKER: &str = "__rustgrid-contained-child";

#[derive(Debug)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Copy, Debug)]
pub struct ChildLimits {
    pub address_space_bytes: u64,
    pub file_bytes: u64,
    pub open_files: u64,
    pub cpu_seconds: u64,
}

pub struct StreamingCommand<'a> {
    pub args: &'a [String],
    pub cwd: &'a Path,
    pub stdin_text: Option<&'a str>,
    pub running: &'a AtomicBool,
    pub timeout: Duration,
    pub idle_timeout: Option<Duration>,
    pub output_is_activity: Option<fn(&str) -> bool>,
    pub max_output_bytes: usize,
    pub environment_allowlist: Option<&'a [String]>,
    pub limits: Option<ChildLimits>,
}

#[derive(Debug)]
pub enum CommandFailure {
    Cancelled,
    TimedOut { seconds: u64 },
    IdleTimedOut { seconds: u64 },
    OutputLimit { detail: String },
}

/// Linux cgroup-v2 boundary for repository-controlled hosted commands.
///
/// The cgroup is created by the trusted coordinator as a root-owned child of
/// its own cgroup. The coordinator uses a bounded privileged write to move the
/// blocked child into the leaf, while only `cgroup.kill` is delegated to the
/// unprivileged runner user. A command cannot migrate to the parent, and
/// `PR_SET_NO_NEW_PRIVS` prevents it from using the runner's passwordless sudo
/// installation to escape the boundary.
pub struct HostedProcessContainment {
    #[cfg(target_os = "linux")]
    cgroup_path: PathBuf,
    #[cfg(target_os = "linux")]
    expected_cgroup: String,
}

impl std::fmt::Display for CommandFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(formatter, "command cancelled"),
            Self::TimedOut { seconds } => {
                write!(formatter, "command timed out after {seconds} seconds")
            }
            Self::IdleTimedOut { seconds } => {
                write!(
                    formatter,
                    "command produced no output for {seconds} seconds"
                )
            }
            Self::OutputLimit { detail } => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for CommandFailure {}

impl HostedProcessContainment {
    #[cfg(target_os = "linux")]
    pub fn new() -> Result<Self> {
        let effective_uid = unsafe { libc::geteuid() };
        let effective_gid = unsafe { libc::getegid() };
        if effective_uid == 0 {
            bail!(
                "hosted repository containment refuses to run commands as root; use the unprivileged GitHub runner account"
            );
        }
        if linux_effective_capabilities()? != 0 {
            bail!(
                "hosted repository containment requires an unprivileged process with no effective Linux capabilities"
            );
        }

        let root = Path::new("/sys/fs/cgroup");
        if !root.join("cgroup.controllers").is_file() {
            bail!("GitHub Actions hosted execution requires a cgroup-v2 unified hierarchy");
        }
        let current = linux_current_cgroup()?;
        let parent = root.join(current.trim_start_matches('/'));
        let parent = fs::canonicalize(&parent)
            .context("could not resolve the coordinator cgroup-v2 directory")?;
        let canonical_root =
            fs::canonicalize(root).context("could not resolve the cgroup-v2 mount")?;
        if !parent.starts_with(&canonical_root) || !parent.join("cgroup.procs").is_file() {
            bail!("coordinator cgroup-v2 membership is outside the unified hierarchy");
        }
        match fs::OpenOptions::new()
            .write(true)
            .open(parent.join("cgroup.procs"))
        {
            Ok(_) => {
                bail!(
                    "coordinator parent cgroup is writable by repository commands; refusing an escapable hosted boundary"
                )
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(libc::EPERM) => {}
            Err(error) => {
                return Err(error)
                    .context("could not verify that the coordinator parent cgroup is protected");
            }
        }

        let name = format!(
            "rustgrid-agent-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        let cgroup_path = parent.join(&name);
        run_privileged_cgroup_command("/usr/bin/mkdir", &["--", path_text(&cgroup_path)?])
            .context("could not create the hosted command cgroup")?;

        let configured = (|| {
            let procs = cgroup_path.join("cgroup.procs");
            let kill = cgroup_path.join("cgroup.kill");
            if !procs.is_file() || !kill.is_file() {
                bail!("hosted command cgroup lacks cgroup.kill; Linux 5.14 or newer is required");
            }
            let owner = format!("{effective_uid}:{effective_gid}");
            run_privileged_cgroup_command("/usr/bin/chown", &["--", &owner, path_text(&kill)?])
                .context("could not delegate the hosted command kill control")?;
            Ok::<(), anyhow::Error>(())
        })();
        if let Err(error) = configured {
            let _ =
                run_privileged_cgroup_command("/usr/bin/rmdir", &["--", path_text(&cgroup_path)?]);
            return Err(error);
        }

        let expected_cgroup = if current == "/" {
            format!("/{name}")
        } else {
            format!("{}/{}", current.trim_end_matches('/'), name)
        };
        let containment = Self {
            cgroup_path,
            expected_cgroup,
        };
        containment.drain()?;
        Ok(containment)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn new() -> Result<Self> {
        bail!("GitHub Actions hosted repository commands require Linux cgroup-v2 containment")
    }

    /// Kill and reap every process in the hosted command cgroup.
    ///
    /// This is deliberately called after each repository-controlled command
    /// and immediately before every credentialed publication operation.
    #[cfg(target_os = "linux")]
    pub fn drain(&self) -> Result<()> {
        fs::write(self.cgroup_path.join("cgroup.kill"), b"1\n")
            .context("could not kill all hosted repository command descendants")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let events = fs::read_to_string(self.cgroup_path.join("cgroup.events"))
                .context("could not inspect hosted command cgroup state")?;
            let populated = events
                .lines()
                .find_map(|line| line.strip_prefix("populated "))
                .context("hosted command cgroup has no populated state")?;
            if populated == "0" {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                bail!("hosted repository command descendants did not terminate");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn drain(&self) -> Result<()> {
        bail!("GitHub Actions hosted repository commands require Linux cgroup-v2 containment")
    }

    #[cfg(target_os = "linux")]
    fn attach(&self, pid: u32) -> Result<()> {
        // Moving a process out of the protected coordinator cgroup requires
        // privilege at the common ancestor. The child is still blocked on the
        // trusted pre-exec gate here, so use a bounded sudo write and verify
        // membership before releasing it.
        run_privileged_cgroup_write(&self.cgroup_path.join("cgroup.procs"), &format!("{pid}\n"))
            .context("could not attach the hosted repository command to its cgroup")?;
        let actual = linux_process_cgroup(pid)?;
        if actual != self.expected_cgroup {
            bail!(
                "hosted repository command entered cgroup {actual}, expected {}",
                self.expected_cgroup
            );
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for HostedProcessContainment {
    fn drop(&mut self) {
        let _ = self.drain();
        if let Ok(path) = path_text(&self.cgroup_path) {
            let _ = run_privileged_cgroup_command("/usr/bin/rmdir", &["--", path]);
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl Drop for HostedProcessContainment {
    fn drop(&mut self) {}
}

#[cfg(target_os = "linux")]
fn linux_effective_capabilities() -> Result<u64> {
    let status =
        fs::read_to_string("/proc/self/status").context("could not inspect Linux capabilities")?;
    let encoded = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:\t"))
        .context("Linux process status has no effective-capability field")?;
    u64::from_str_radix(encoded.trim(), 16).context("Linux effective capabilities are malformed")
}

#[cfg(target_os = "linux")]
fn linux_current_cgroup() -> Result<String> {
    let memberships =
        fs::read_to_string("/proc/self/cgroup").context("could not inspect cgroup membership")?;
    let current = memberships
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .context("process is not attached to a cgroup-v2 unified hierarchy")?;
    if !current.starts_with('/')
        || current.split('/').any(|part| part == "." || part == "..")
        || current.contains('\0')
    {
        bail!("process has an unsafe cgroup-v2 membership path");
    }
    Ok(current.to_owned())
}

#[cfg(target_os = "linux")]
fn linux_process_cgroup(pid: u32) -> Result<String> {
    let memberships = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .context("could not verify hosted command cgroup membership")?;
    memberships
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_owned)
        .context("hosted command is not attached to cgroup v2")
}

#[cfg(target_os = "linux")]
fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("cgroup-v2 path is not valid UTF-8")
}

#[cfg(target_os = "linux")]
fn run_privileged_cgroup_command(program: &str, args: &[&str]) -> Result<()> {
    if !Path::new("/usr/bin/sudo").is_file() {
        bail!("GitHub runner is missing /usr/bin/sudo required for cgroup setup");
    }
    let output = Command::new("/usr/bin/sudo")
        .args(["-n", "--", program])
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("could not run trusted cgroup helper {program}"))?;
    if !output.status.success() {
        bail!(
            "trusted cgroup helper {program} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_privileged_cgroup_write(path: &Path, value: &str) -> Result<()> {
    if !Path::new("/usr/bin/sudo").is_file() || !Path::new("/usr/bin/tee").is_file() {
        bail!("GitHub runner is missing the trusted cgroup write helpers");
    }
    let mut child = Command::new("/usr/bin/sudo")
        .args(["-n", "--", "/usr/bin/tee", "--", path_text(path)?])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start the trusted cgroup write helper")?;
    child
        .stdin
        .take()
        .context("trusted cgroup write helper has no stdin")?
        .write_all(value.as_bytes())
        .context("could not provide the cgroup membership to the trusted helper")?;
    let output = child
        .wait_with_output()
        .context("could not wait for the trusted cgroup write helper")?;
    if !output.status.success() {
        bail!(
            "trusted cgroup write helper exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub fn is_timeout(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<CommandFailure>()
        .is_some_and(|failure| {
            matches!(
                failure,
                CommandFailure::TimedOut { .. } | CommandFailure::IdleTimedOut { .. }
            )
        })
}

pub fn is_idle_timeout(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<CommandFailure>()
        .is_some_and(|failure| matches!(failure, CommandFailure::IdleTimedOut { .. }))
}

pub fn parse(command: &str) -> Result<Vec<String>> {
    let parts = shlex::split(command).context("command contains invalid shell quoting")?;
    if parts.is_empty() {
        bail!("command cannot be empty");
    }
    Ok(parts)
}

pub fn capture<I, S>(program: &str, args: I, cwd: &Path) -> Result<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd);
    sanitize_child_environment(&mut command);
    let output = command
        .output()
        .with_context(|| format!("failed to start {program}"))?;
    Ok(CommandOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

pub fn capture_cancellable(
    command: &str,
    cwd: &Path,
    running: &AtomicBool,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<CommandOutput> {
    capture_cancellable_with_environment(
        command,
        cwd,
        running,
        timeout,
        max_output_bytes,
        None,
        None,
    )
}

pub fn capture_cancellable_with_environment(
    command: &str,
    cwd: &Path,
    running: &AtomicBool,
    timeout: Duration,
    max_output_bytes: usize,
    environment_allowlist: Option<&[String]>,
    limits: Option<ChildLimits>,
) -> Result<CommandOutput> {
    capture_cancellable_with_containment(
        command,
        cwd,
        running,
        timeout,
        max_output_bytes,
        environment_allowlist,
        limits,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn capture_hosted_cancellable_with_environment(
    command: &str,
    cwd: &Path,
    running: &AtomicBool,
    timeout: Duration,
    max_output_bytes: usize,
    environment_allowlist: Option<&[String]>,
    limits: Option<ChildLimits>,
    containment: &HostedProcessContainment,
) -> Result<CommandOutput> {
    capture_cancellable_with_containment(
        command,
        cwd,
        running,
        timeout,
        max_output_bytes,
        environment_allowlist,
        limits,
        Some(containment),
    )
}

#[allow(clippy::too_many_arguments)]
fn capture_cancellable_with_containment(
    command: &str,
    cwd: &Path,
    running: &AtomicBool,
    timeout: Duration,
    max_output_bytes: usize,
    environment_allowlist: Option<&[String]>,
    limits: Option<ChildLimits>,
    containment: Option<&HostedProcessContainment>,
) -> Result<CommandOutput> {
    if containment.is_some() && environment_allowlist.is_none() {
        bail!("hosted repository commands require an explicit environment allowlist");
    }
    let parts = parse(command)?;
    println!("  $ {}", display_command(&parts));
    let (mut command, start_gate) = command_for_parts(&parts, containment)?;
    command
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    sanitize_child_environment(&mut command);
    if let Some(allowlist) = environment_allowlist {
        apply_environment_allowlist(&mut command, allowlist);
    }
    configure_child(&mut command, limits);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start {}", parts[0]))?;
    if let Some(start_gate) = start_gate
        && let Err(error) = start_gate.release(&child, containment.expect("paired containment"))
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let mut stdout = child
        .stdout
        .take()
        .context("failed to capture command stdout")?;
    let mut stderr = child
        .stderr
        .take()
        .context("failed to capture command stderr")?;
    let stream_limit = max_output_bytes / 2;
    let stdout_reader = thread::spawn(move || read_bounded(&mut stdout, stream_limit));
    let stderr_reader = thread::spawn(move || read_bounded(&mut stderr, stream_limit));
    let started = std::time::Instant::now();
    let status = loop {
        if !running.load(Ordering::SeqCst) || shutdown::requested() {
            terminate_command(&mut child, containment)?;
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(CommandFailure::Cancelled.into());
        }
        if started.elapsed() >= timeout {
            terminate_command(&mut child, containment)?;
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(CommandFailure::TimedOut {
                seconds: timeout.as_secs(),
            }
            .into());
        }
        if let Some(status) = child.try_wait().context("failed while checking command")? {
            break status;
        }
        thread::sleep(Duration::from_millis(250));
    };
    // A repository command can fork, create a new session, and let its direct
    // child exit. Drain the cgroup before joining output readers so even a
    // double-forked helper cannot survive to observe later credentialed
    // publication operations.
    terminate_command(&mut child, containment)?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
    if stdout.truncated || stderr.truncated {
        return Err(CommandFailure::OutputLimit {
            detail: format!("command output exceeded {max_output_bytes} bytes"),
        }
        .into());
    }
    Ok(CommandOutput {
        status,
        stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
    })
}

pub fn capture_with_env<I, S, E, K, V>(
    program: &str,
    args: I,
    cwd: &Path,
    env: E,
) -> Result<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd);
    sanitize_child_environment(&mut command);
    let output = command
        .envs(env)
        .output()
        .with_context(|| format!("failed to start {program}"))?;
    Ok(CommandOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

pub fn checked<I, S>(program: &str, args: I, cwd: &Path) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = capture(program, args, cwd)?;
    if !output.status.success() {
        let detail = if output.stderr.is_empty() {
            output.stdout
        } else {
            output.stderr
        };
        bail!("{program} exited with {}: {detail}", output.status);
    }
    Ok(output.stdout.trim().to_owned())
}

pub fn streaming(command: &str, cwd: &Path, stdin_text: Option<&str>) -> Result<ExitStatus> {
    let parts = parse(command)?;
    println!("  $ {}", display_command(&parts));
    let mut child = Command::new(&parts[0])
        .args(&parts[1..])
        .current_dir(cwd)
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start {}", parts[0]))?;

    if let Some(input) = stdin_text {
        child
            .stdin
            .take()
            .context("failed to open child stdin")?
            .write_all(input.as_bytes())
            .context("failed to write command stdin")?;
    }
    child.wait().context("failed while waiting for command")
}

/// Runs a command with line-buffered stdout so callers can process machine-readable
/// progress without waiting for the child to exit. Codex commands automatically get
/// `--json`; compatible custom commands may emit their own JSONL or plain text lines.
pub fn streaming_lines<F>(
    command: &str,
    cwd: &Path,
    stdin_text: Option<&str>,
    on_line: F,
) -> Result<ExitStatus>
where
    F: FnMut(&str) -> Result<()>,
{
    let running = Arc::new(AtomicBool::new(true));
    streaming_lines_cancellable(
        command,
        cwd,
        stdin_text,
        &running,
        Duration::from_secs(24 * 60 * 60),
        8 * 1024 * 1024,
        on_line,
    )
}

pub fn streaming_lines_cancellable<F>(
    command: &str,
    cwd: &Path,
    stdin_text: Option<&str>,
    running: &AtomicBool,
    timeout: Duration,
    max_output_bytes: usize,
    on_line: F,
) -> Result<ExitStatus>
where
    F: FnMut(&str) -> Result<()>,
{
    let parts = parse(command)?;
    streaming_args(
        StreamingCommand {
            args: &parts,
            cwd,
            stdin_text,
            running,
            timeout,
            idle_timeout: None,
            output_is_activity: None,
            max_output_bytes,
            environment_allowlist: None,
            limits: None,
        },
        on_line,
    )
}

pub fn streaming_args<F>(execution: StreamingCommand<'_>, mut on_line: F) -> Result<ExitStatus>
where
    F: FnMut(&str) -> Result<()>,
{
    let StreamingCommand {
        args,
        cwd,
        stdin_text,
        running,
        timeout,
        idle_timeout,
        output_is_activity,
        max_output_bytes,
        environment_allowlist,
        limits,
    } = execution;
    if args.is_empty() {
        bail!("command cannot be empty");
    }
    let mut parts = args.to_vec();
    add_codex_json_flag(&mut parts);
    println!("  $ {}", display_command(&parts));
    let mut command = Command::new(&parts[0]);
    command
        .args(&parts[1..])
        .current_dir(cwd)
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    sanitize_child_environment(&mut command);
    if let Some(allowlist) = environment_allowlist {
        apply_environment_allowlist(&mut command, allowlist);
    }
    configure_child(&mut command, limits);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start {}", parts[0]))?;

    if let Some(input) = stdin_text {
        child
            .stdin
            .take()
            .context("failed to open child stdin")?
            .write_all(input.as_bytes())
            .context("failed to write command stdin")?;
    }

    let stdout = child.stdout.take().context("failed to open child stdout")?;
    let mut stderr = child.stderr.take().context("failed to open child stderr")?;
    let stream_limit = max_output_bytes / 2;
    let (sender, receiver) = mpsc::sync_channel(32);
    let reader = spawn_bounded_line_reader(stdout, stream_limit, 64 * 1024, sender);
    let stderr_reader = thread::spawn(move || read_bounded(&mut stderr, stream_limit));
    let started = std::time::Instant::now();
    let mut last_activity = started;
    loop {
        if !running.load(Ordering::SeqCst) || shutdown::requested() {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            drop(receiver);
            let _ = reader.join();
            let _ = stderr_reader.join();
            return Err(CommandFailure::Cancelled.into());
        }
        if started.elapsed() >= timeout {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            drop(receiver);
            let _ = reader.join();
            let _ = stderr_reader.join();
            return Err(CommandFailure::TimedOut {
                seconds: timeout.as_secs(),
            }
            .into());
        }
        if idle_timeout.is_some_and(|limit| last_activity.elapsed() >= limit) {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            drop(receiver);
            let _ = reader.join();
            let _ = stderr_reader.join();
            return Err(CommandFailure::IdleTimedOut {
                seconds: idle_timeout.unwrap_or_default().as_secs(),
            }
            .into());
        }
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(StreamMessage::Line(line)) => {
                if output_is_activity.is_none_or(|filter| filter(&line)) {
                    last_activity = std::time::Instant::now();
                }
                if let Err(error) = on_line(&line) {
                    terminate_process_tree(&mut child);
                    let _ = child.wait();
                    drop(receiver);
                    let _ = reader.join();
                    let _ = stderr_reader.join();
                    return Err(error);
                }
            }
            Ok(StreamMessage::OversizedLine { bytes }) => {
                last_activity = std::time::Instant::now();
                eprintln!(
                    "[warning] omitted oversized command output line ({bytes} bytes); continuing stream"
                );
            }
            Ok(StreamMessage::Failure(error)) => {
                terminate_process_tree(&mut child);
                let _ = child.wait();
                drop(receiver);
                let _ = reader.join();
                let _ = stderr_reader.join();
                return Err(CommandFailure::OutputLimit { detail: error }.into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child
                    .try_wait()
                    .context("failed while checking command")?
                    .is_some()
                {
                    terminate_process_tree(&mut child);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = reader.join();
    let status = child.wait().context("failed while waiting for command")?;
    terminate_process_tree(&mut child);
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
    if stderr.truncated {
        return Err(CommandFailure::OutputLimit {
            detail: format!("command stderr exceeded {stream_limit} bytes"),
        }
        .into());
    }
    if !stderr.bytes.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&stderr.bytes));
    }
    Ok(status)
}

enum StreamMessage {
    Line(String),
    OversizedLine { bytes: usize },
    Failure(String),
}

fn spawn_bounded_line_reader<R: Read + Send + 'static>(
    mut reader: R,
    max_bytes: usize,
    max_line_bytes: usize,
    sender: SyncSender<StreamMessage>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0u8; 8 * 1024];
        let mut pending = Vec::new();
        let mut current_line_bytes = 0usize;
        let mut discarding_oversized_line = false;
        let mut total = 0usize;
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(read) => read,
                Err(error) => {
                    let _ = sender.send(StreamMessage::Failure(format!(
                        "failed to read command stdout: {error}"
                    )));
                    return;
                }
            };
            if read == 0 {
                if discarding_oversized_line {
                    let _ = sender.send(StreamMessage::OversizedLine {
                        bytes: current_line_bytes,
                    });
                } else if !pending.is_empty() {
                    let line = String::from_utf8_lossy(&pending).into_owned();
                    let _ = sender.send(StreamMessage::Line(line));
                }
                return;
            }
            total = total.saturating_add(read);
            if total > max_bytes {
                let _ = sender.send(StreamMessage::Failure(format!(
                    "command stdout exceeded {max_bytes} bytes"
                )));
                return;
            }
            for segment in buffer[..read].split_inclusive(|byte| *byte == b'\n') {
                let terminated = segment.last() == Some(&b'\n');
                let content = if terminated {
                    &segment[..segment.len() - 1]
                } else {
                    segment
                };
                current_line_bytes = current_line_bytes.saturating_add(content.len());
                if !discarding_oversized_line {
                    if pending.len().saturating_add(content.len()) > max_line_bytes {
                        pending.clear();
                        discarding_oversized_line = true;
                    } else {
                        pending.extend_from_slice(content);
                    }
                }
                if terminated {
                    if discarding_oversized_line {
                        if sender
                            .send(StreamMessage::OversizedLine {
                                bytes: current_line_bytes,
                            })
                            .is_err()
                        {
                            return;
                        }
                    } else {
                        if pending.last() == Some(&b'\r') {
                            pending.pop();
                        }
                        let line = String::from_utf8_lossy(&pending).into_owned();
                        if sender.send(StreamMessage::Line(line)).is_err() {
                            return;
                        }
                    }
                    pending.clear();
                    current_line_bytes = 0;
                    discarding_oversized_line = false;
                }
            }
        }
    })
}

fn command_for_parts(
    parts: &[String],
    containment: Option<&HostedProcessContainment>,
) -> Result<(Command, Option<ChildStartGate>)> {
    if containment.is_none() {
        let mut command = Command::new(&parts[0]);
        command.args(&parts[1..]);
        return Ok((command, None));
    }
    #[cfg(target_os = "linux")]
    {
        let gate = ChildStartGate::new()?;
        // /proc/self/exe names the currently executing, already-open agent
        // image. It cannot be replaced by repository writes between commands.
        let mut command = Command::new("/proc/self/exe");
        command
            .arg(CONTAINED_CHILD_MARKER)
            .arg(gate.child_fd().to_string())
            .args(parts);
        Ok((command, Some(gate)))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = parts;
        bail!("hosted repository commands require Linux cgroup-v2 containment")
    }
}

#[cfg(target_os = "linux")]
struct ChildStartGate {
    read_fd: libc::c_int,
    write_fd: libc::c_int,
}

#[cfg(target_os = "linux")]
impl ChildStartGate {
    fn new() -> Result<Self> {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors points to two writable integers. O_CLOEXEC keeps
        // both ends private unless the read end is explicitly delegated below.
        if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("could not create the hosted command start gate");
        }
        // The trusted /proc/self/exe wrapper must inherit the read side across
        // exec. The write side remains CLOEXEC, so only the coordinator can
        // release the command after cgroup membership is verified.
        if unsafe { libc::fcntl(descriptors[0], libc::F_SETFD, 0) } == -1 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::close(descriptors[0]);
                libc::close(descriptors[1]);
            }
            return Err(error).context("could not delegate the hosted command start gate");
        }
        Ok(Self {
            read_fd: descriptors[0],
            write_fd: descriptors[1],
        })
    }

    fn child_fd(&self) -> libc::c_int {
        self.read_fd
    }

    fn release(mut self, child: &Child, containment: &HostedProcessContainment) -> Result<()> {
        unsafe {
            libc::close(self.read_fd);
        }
        self.read_fd = -1;
        containment.attach(child.id())?;
        let byte = [0xa5_u8];
        // SAFETY: write_fd is the live coordinator end of the one-byte gate.
        let written = unsafe { libc::write(self.write_fd, byte.as_ptr().cast(), byte.len()) };
        if written != 1 {
            return Err(std::io::Error::last_os_error())
                .context("could not release the contained hosted command");
        }
        unsafe {
            libc::close(self.write_fd);
        }
        self.write_fd = -1;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for ChildStartGate {
    fn drop(&mut self) {
        if self.read_fd >= 0 {
            unsafe {
                libc::close(self.read_fd);
            }
        }
        if self.write_fd >= 0 {
            unsafe {
                libc::close(self.write_fd);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
struct ChildStartGate;

#[cfg(not(target_os = "linux"))]
impl ChildStartGate {
    fn release(
        self,
        _child: &std::process::Child,
        _containment: &HostedProcessContainment,
    ) -> Result<()> {
        bail!("hosted repository commands require Linux cgroup-v2 containment")
    }
}

/// Returns true only for the private, pre-cgroup command wrapper invocation.
pub fn contained_child_requested() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == OsStr::new(CONTAINED_CHILD_MARKER))
}

/// Blocks the private command wrapper until its parent has attached it to the
/// cgroup boundary, then replaces it with the requested repository command.
pub fn exec_contained_child() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;

        let mut arguments = std::env::args_os();
        let _executable = arguments.next();
        if arguments.next().as_deref() != Some(OsStr::new(CONTAINED_CHILD_MARKER)) {
            bail!("invalid contained child invocation");
        }
        let gate = arguments
            .next()
            .context("contained child invocation has no start gate")?;
        let gate = gate
            .to_str()
            .context("contained child start gate is not UTF-8")?
            .parse::<libc::c_int>()
            .context("contained child start gate is invalid")?;
        let program = arguments
            .next()
            .context("contained child invocation has no program")?;

        // SAFETY: PR_SET_NO_NEW_PRIVS takes one integer flag. Once set it is
        // inherited across fork/exec and cannot be cleared, preventing the
        // repository command from using sudo/setuid to move out of its cgroup.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("could not disable hosted command privilege escalation");
        }
        let mut byte = [0_u8; 1];
        // SAFETY: gate is the inherited read end created by ChildStartGate.
        let read = unsafe { libc::read(gate, byte.as_mut_ptr().cast(), byte.len()) };
        unsafe {
            libc::close(gate);
        }
        if read != 1 || byte[0] != 0xa5 {
            bail!("hosted command start gate closed before cgroup attachment");
        }

        let error = Command::new(&program).args(arguments).exec();
        Err(error).with_context(|| format!("failed to exec {}", program.to_string_lossy()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        bail!("contained hosted commands require Linux")
    }
}

#[cfg(unix)]
fn configure_child(command: &mut Command, limits: Option<ChildLimits>) {
    use std::os::unix::process::CommandExt;
    // SAFETY: pre_exec performs only async-signal-safe setsid/setrlimit syscalls.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(limits) = limits {
                set_limit(libc::RLIMIT_AS, limits.address_space_bytes)?;
                set_limit(libc::RLIMIT_FSIZE, limits.file_bytes)?;
                set_limit(libc::RLIMIT_NOFILE, limits.open_files)?;
                set_limit(libc::RLIMIT_CPU, limits.cpu_seconds)?;
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_child(_command: &mut Command, _limits: Option<ChildLimits>) {}

#[cfg(unix)]
#[cfg(any(target_os = "linux", target_os = "android"))]
type RlimitResource = libc::__rlimit_resource_t;

#[cfg(unix)]
#[cfg(not(any(target_os = "linux", target_os = "android")))]
type RlimitResource = libc::c_int;

#[cfg(unix)]
fn set_limit(resource: RlimitResource, value: u64) -> std::io::Result<()> {
    let mut inherited = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: inherited points to writable initialized storage for getrlimit.
    if unsafe { libc::getrlimit(resource, &mut inherited) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let requested = value as libc::rlim_t;
    let effective = requested.min(inherited.rlim_max);
    let limit = libc::rlimit {
        rlim_cur: effective,
        rlim_max: effective,
    };
    // SAFETY: resource is a supported RLIMIT constant and limit points to a
    // fully initialized rlimit value that lives for the syscall duration.
    if unsafe { libc::setrlimit(resource, &limit) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn terminate_process_tree(child: &mut std::process::Child) {
    let process_group = -(child.id() as i32);
    // SAFETY: the child created its own session/process group immediately before
    // exec, and kill receives only that known process-group identifier.
    unsafe {
        libc::kill(process_group, libc::SIGKILL);
    }
}

fn terminate_command(
    child: &mut std::process::Child,
    containment: Option<&HostedProcessContainment>,
) -> Result<()> {
    if let Some(containment) = containment {
        if let Err(error) = containment.drain() {
            terminate_process_tree(child);
            let _ = child.kill();
            return Err(error);
        }
    } else {
        terminate_process_tree(child);
    }
    Ok(())
}

fn sanitize_child_environment(command: &mut Command) {
    for (name, _) in std::env::vars_os()
        .filter(|(name, _)| protected_child_environment_name(&name.to_string_lossy()))
    {
        command.env_remove(&name);
    }
}

fn protected_child_environment_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name.starts_with("RUSTGRID_")
        || name.starts_with("ACTIONS_")
        || name.starts_with("OPENAI_")
        || name.starts_with("CODEX_")
        || name.starts_with("CHATGPT_")
        || name.starts_with("GIT_CONFIG_")
        || matches!(
            name.as_str(),
            "GITHUB_TOKEN"
                | "GH_TOKEN"
                | "SSH_AUTH_SOCK"
                | "GIT_ASKPASS"
                | "SSH_ASKPASS"
                | "GIT_SSH"
                | "GIT_SSH_COMMAND"
                | "GIT_PROXY_COMMAND"
        )
}

fn apply_environment_allowlist(command: &mut Command, allowlist: &[String]) {
    let values = allowlist
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
        .collect::<Vec<_>>();
    command.env_clear();
    for (name, value) in values {
        command.env(name, value);
    }
}

struct BoundedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_bounded(reader: &mut impl Read, limit: usize) -> std::io::Result<BoundedBytes> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        if remaining > 0 {
            output.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        truncated |= read > remaining;
    }
    if truncated {
        output.extend_from_slice(b"\n[output truncated by rustgrid-agent]\n");
    }
    Ok(BoundedBytes {
        bytes: output,
        truncated,
    })
}

#[cfg(not(unix))]
fn terminate_process_tree(child: &mut std::process::Child) {
    let _ = child.kill();
}

pub(crate) fn add_codex_json_flag(parts: &mut Vec<String>) {
    let is_codex = Path::new(&parts[0])
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name == "codex");
    if !is_codex || parts.iter().any(|part| part == "--json") {
        return;
    }
    let prompt_index = parts
        .iter()
        .position(|part| part == "-")
        .unwrap_or(parts.len());
    parts.insert(prompt_index, "--json".to_owned());
}

fn display_command(parts: &[String]) -> String {
    parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            if index > 0 && parts[index - 1] == "-e" {
                return part
                    .split_once('=')
                    .map_or_else(|| part.clone(), |(key, _)| format!("{key}=<redacted>"));
            }
            if part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._/:=".contains(c))
            {
                part.clone()
            } else {
                format!("{:?}", part)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_without_a_shell() {
        assert_eq!(
            parse("npm run 'test unit'").unwrap(),
            ["npm", "run", "test unit"]
        );
        assert!(parse("echo '").is_err());
    }

    #[test]
    fn adds_json_before_the_stdin_prompt_for_codex() {
        let mut parts = parse("/usr/local/bin/codex exec --full-auto -").unwrap();
        add_codex_json_flag(&mut parts);
        assert_eq!(
            parts,
            ["/usr/local/bin/codex", "exec", "--full-auto", "--json", "-"]
        );

        add_codex_json_flag(&mut parts);
        assert_eq!(parts.iter().filter(|part| *part == "--json").count(), 1);
    }

    #[test]
    fn captured_commands_honor_cancellation() {
        let running = AtomicBool::new(false);
        let error = capture_cancellable(
            "rustc --version",
            Path::new("."),
            &running,
            Duration::from_secs(30),
            1024 * 1024,
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<CommandFailure>(),
            Some(CommandFailure::Cancelled)
        ));
    }

    #[test]
    fn captured_command_timeouts_are_typed() {
        let running = AtomicBool::new(true);
        let error = capture_cancellable(
            "rustc --version",
            Path::new("."),
            &running,
            Duration::ZERO,
            1024 * 1024,
        )
        .unwrap_err();
        assert!(is_timeout(&error));
        assert!(!is_idle_timeout(&error));
    }

    #[test]
    fn hosted_identity_and_provider_credentials_are_protected_child_environment() {
        for name in [
            "RUSTGRID_EXECUTION_TOKEN",
            "RUSTGRID_OIDC_REQUEST_TOKEN",
            "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
            "ACTIONS_RUNTIME_TOKEN",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "CHATGPT_TOKEN",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "SSH_AUTH_SOCK",
            "GIT_ASKPASS",
            "GIT_CONFIG_VALUE_0",
            "GIT_SSH_COMMAND",
        ] {
            assert!(protected_child_environment_name(name), "{name}");
        }
        assert!(!protected_child_environment_name("PATH"));
        assert!(!protected_child_environment_name("RUSTUP_HOME"));
    }

    #[cfg(unix)]
    #[test]
    fn cancellable_commands_do_not_leave_detached_descendants_running() {
        let directory = tempfile::tempdir().unwrap();
        let running = AtomicBool::new(true);
        let started = std::time::Instant::now();
        let output = capture_cancellable(
            "sh -c 'sleep 3 >/dev/null 2>&1 &'",
            directory.path(),
            &running,
            Duration::from_secs(5),
            4_096,
        )
        .unwrap();
        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_boundary_kills_a_setsid_double_fork_before_publication() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("escape.py");
        let release = directory.path().join("release");
        let escaped_pid = directory.path().join("escaped.pid");
        fs::write(
            &script,
            r#"import os
import sys
import time

release, pid_file = sys.argv[1], sys.argv[2]
while not os.path.exists(release):
    time.sleep(0.01)
first = os.fork()
if first == 0:
    os.setsid()
    second = os.fork()
    if second == 0:
        with open(pid_file, "w", encoding="ascii") as handle:
            handle.write(str(os.getpid()))
            handle.flush()
            os.fsync(handle.fileno())
        while True:
            time.sleep(1)
    os._exit(0)
os.waitpid(first, 0)
while not os.path.exists(pid_file):
    time.sleep(0.01)
"#,
        )
        .unwrap();

        let containment = HostedProcessContainment::new().unwrap();
        let mut child = Command::new("/usr/bin/python3")
            .args([
                script.as_os_str(),
                release.as_os_str(),
                escaped_pid.as_os_str(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        containment.attach(child.id()).unwrap();
        fs::write(&release, b"release\n").unwrap();
        assert!(child.wait().unwrap().success());
        let pid = fs::read_to_string(&escaped_pid)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();

        // This is the publication barrier: after it returns, even a process
        // that escaped the original session and process group must be gone.
        containment.drain().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            // SAFETY: signal 0 only probes the parsed test child PID.
            let result = unsafe { libc::kill(pid, 0) };
            if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "escaped descendant {pid} survived the publication barrier"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn streaming_commands_time_out_after_output_goes_idle() {
        let running = AtomicBool::new(true);
        let args = [
            "sh".to_owned(),
            "-c".to_owned(),
            "printf 'started\\n'; sleep 5".to_owned(),
        ];
        let started = std::time::Instant::now();
        let error = streaming_args(
            StreamingCommand {
                args: &args,
                cwd: Path::new("."),
                stdin_text: None,
                running: &running,
                timeout: Duration::from_secs(5),
                idle_timeout: Some(Duration::from_millis(100)),
                output_is_activity: None,
                max_output_bytes: 1024 * 1024,
                environment_allowlist: None,
                limits: None,
            },
            |_| Ok(()),
        )
        .unwrap_err();

        assert!(is_timeout(&error));
        assert!(is_idle_timeout(&error));
        assert!(matches!(
            error.downcast_ref::<CommandFailure>(),
            Some(CommandFailure::IdleTimedOut { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn bounded_reader_drains_and_marks_truncation() {
        let mut input = std::io::Cursor::new(vec![b'x'; 1024]);
        let output = read_bounded(&mut input, 32).unwrap();
        assert!(output.bytes.starts_with(&[b'x'; 32]));
        assert!(output.truncated);
        assert!(String::from_utf8_lossy(&output.bytes).contains("output truncated"));
    }

    #[test]
    fn streaming_reader_omits_an_oversized_line_and_continues() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let mut output = vec![b'x'; 1024];
        output.extend_from_slice(b"\nkept\n");
        let reader = spawn_bounded_line_reader(std::io::Cursor::new(output), 4096, 128, sender);
        assert!(matches!(
            receiver.recv().unwrap(),
            StreamMessage::OversizedLine { bytes: 1024 }
        ));
        match receiver.recv().unwrap() {
            StreamMessage::Line(line) => assert_eq!(line, "kept"),
            StreamMessage::OversizedLine { .. } | StreamMessage::Failure(_) => {
                panic!("expected the line following oversized output")
            }
        }
        reader.join().unwrap();
    }
}
