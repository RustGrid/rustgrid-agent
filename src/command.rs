use std::{
    ffi::OsStr,
    io::{Read, Write},
    path::Path,
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::shutdown;

#[derive(Debug)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Copy, Debug)]
pub struct CommandActivity {
    pub elapsed: Duration,
    pub last_output_age: Duration,
    pub bytes_emitted: u64,
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
    capture_cancellable_observed(
        command,
        cwd,
        running,
        timeout,
        max_output_bytes,
        environment_allowlist,
        limits,
        None,
        &mut |_| {},
    )
}

#[allow(clippy::too_many_arguments)]
fn capture_cancellable_observed(
    command: &str,
    cwd: &Path,
    running: &AtomicBool,
    timeout: Duration,
    max_output_bytes: usize,
    environment_allowlist: Option<&[String]>,
    limits: Option<ChildLimits>,
    inactivity_timeout: Option<Duration>,
    on_activity: &mut dyn FnMut(CommandActivity),
) -> Result<CommandOutput> {
    let parts = parse(command)?;
    println!("  $ {}", display_command(&parts));
    let mut command = Command::new(&parts[0]);
    command
        .args(&parts[1..])
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
    let mut stdout = child
        .stdout
        .take()
        .context("failed to capture command stdout")?;
    let mut stderr = child
        .stderr
        .take()
        .context("failed to capture command stderr")?;
    let stream_limit = max_output_bytes / 2;
    let bytes_emitted = Arc::new(AtomicU64::new(0));
    let last_output_at = Arc::new(Mutex::new(std::time::Instant::now()));
    let stdout_bytes = Arc::clone(&bytes_emitted);
    let stdout_last_output = Arc::clone(&last_output_at);
    let stderr_bytes = Arc::clone(&bytes_emitted);
    let stderr_last_output = Arc::clone(&last_output_at);
    let stdout_reader = thread::spawn(move || {
        read_bounded_observed(
            &mut stdout,
            stream_limit,
            &stdout_bytes,
            &stdout_last_output,
        )
    });
    let stderr_reader = thread::spawn(move || {
        read_bounded_observed(
            &mut stderr,
            stream_limit,
            &stderr_bytes,
            &stderr_last_output,
        )
    });
    let started = std::time::Instant::now();
    let mut observed_bytes = 0;
    let mut last_activity_report = None::<std::time::Instant>;
    let status = loop {
        if !running.load(Ordering::SeqCst) || shutdown::requested() {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(CommandFailure::Cancelled.into());
        }
        if started.elapsed() >= timeout {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(CommandFailure::TimedOut {
                seconds: timeout.as_secs(),
            }
            .into());
        }
        let last_output_age = last_output_at
            .lock()
            .map(|last| last.elapsed())
            .unwrap_or_default();
        // A short-lived command may complete between polling ticks, especially
        // on a loaded macOS runner. Observe terminal state before classifying
        // the same tick as idle so completed work is never reported as an
        // inactivity timeout merely because the polling interval crossed the
        // configured boundary.
        if let Some(status) = child.try_wait().context("failed while checking command")? {
            break status;
        }
        if inactivity_timeout
            .is_some_and(|limit| started.elapsed() >= limit && last_output_age >= limit)
        {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(CommandFailure::IdleTimedOut {
                seconds: inactivity_timeout.unwrap_or_default().as_secs(),
            }
            .into());
        }
        let current_bytes = bytes_emitted.load(Ordering::Relaxed);
        if current_bytes != observed_bytes
            && last_activity_report.is_none_or(|last| last.elapsed() >= Duration::from_secs(5))
        {
            observed_bytes = current_bytes;
            last_activity_report = Some(std::time::Instant::now());
            on_activity(CommandActivity {
                elapsed: started.elapsed(),
                last_output_age,
                bytes_emitted: current_bytes,
            });
        }
        thread::sleep(Duration::from_millis(250));
    };
    // A repository command can leave background descendants in its process
    // group. Kill the group before joining output readers so they cannot hold
    // the pipes open.
    terminate_process_tree(&mut child);
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
    // Reader threads can observe the final stdout/stderr bytes after the
    // process exits and after the polling loop emitted its last throttled
    // update. Always publish that post-drain snapshot when it advances the
    // byte count so completion telemetry cannot omit trailing output.
    let final_bytes = bytes_emitted.load(Ordering::Relaxed);
    if final_bytes != observed_bytes {
        let last_output_age = last_output_at
            .lock()
            .map(|last| last.elapsed())
            .unwrap_or_default();
        on_activity(CommandActivity {
            elapsed: started.elapsed(),
            last_output_age,
            bytes_emitted: final_bytes,
        });
    }
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
        || name.starts_with("ANTHROPIC_")
        || name.starts_with("CLAUDE_CODE_")
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

fn read_bounded_observed(
    reader: &mut impl Read,
    limit: usize,
    bytes_emitted: &AtomicU64,
    last_output_at: &Mutex<std::time::Instant>,
) -> std::io::Result<BoundedBytes> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes_emitted.fetch_add(u64::try_from(read).unwrap_or(u64::MAX), Ordering::Relaxed);
        if let Ok(mut last) = last_output_at.lock() {
            *last = std::time::Instant::now();
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
    fn provider_credentials_are_protected_child_environment() {
        for name in [
            "RUSTGRID_EXECUTION_TOKEN",
            "RUSTGRID_OIDC_REQUEST_TOKEN",
            "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
            "ACTIONS_RUNTIME_TOKEN",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "CHATGPT_TOKEN",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
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

    #[cfg(unix)]
    #[test]
    fn incremental_stdout_and_stderr_refresh_process_activity() {
        let running = AtomicBool::new(true);
        let directory = tempfile::tempdir().unwrap();
        let mut activity = Vec::new();
        let output = capture_cancellable_observed(
            "sh -c 'printf \"first\\n\"; sleep 0.1; printf \"second\\n\" >&2; sleep 0.1; printf \"third\\n\"'",
            directory.path(),
            &running,
            Duration::from_secs(2),
            64 * 1024,
            None,
            None,
            None,
            &mut |observation| activity.push(observation),
        )
        .unwrap();
        assert!(output.status.success());
        assert!(
            activity
                .iter()
                .any(|observation| observation.bytes_emitted >= 5)
        );
        assert!(activity.last().unwrap().bytes_emitted >= 19);
        assert!(activity.last().unwrap().last_output_age <= activity.last().unwrap().elapsed);
    }
}
