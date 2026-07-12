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
    pub max_output_bytes: usize,
    pub environment_allowlist: Option<&'a [String]>,
    pub limits: Option<ChildLimits>,
}

#[derive(Debug)]
pub enum CommandFailure {
    Cancelled,
    TimedOut { seconds: u64 },
    OutputLimit { detail: String },
}

impl std::fmt::Display for CommandFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(formatter, "command cancelled"),
            Self::TimedOut { seconds } => {
                write!(formatter, "command timed out after {seconds} seconds")
            }
            Self::OutputLimit { detail } => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for CommandFailure {}

pub fn is_timeout(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<CommandFailure>()
        .is_some_and(|failure| matches!(failure, CommandFailure::TimedOut { .. }))
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
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
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
    let stdout_reader = thread::spawn(move || read_bounded(&mut stdout, stream_limit));
    let stderr_reader = thread::spawn(move || read_bounded(&mut stderr, stream_limit));
    let started = std::time::Instant::now();
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
        if let Some(status) = child.try_wait().context("failed while checking command")? {
            break status;
        }
        thread::sleep(Duration::from_millis(250));
    };
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
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(StreamMessage::Line(line)) => {
                if let Err(error) = on_line(&line) {
                    terminate_process_tree(&mut child);
                    let _ = child.wait();
                    drop(receiver);
                    let _ = reader.join();
                    let _ = stderr_reader.join();
                    return Err(error);
                }
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
                let _ = child.try_wait().context("failed while checking command")?;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = reader.join();
    let status = child.wait().context("failed while waiting for command")?;
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
                if !pending.is_empty() {
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
                pending.extend_from_slice(content);
                if pending.len() > max_line_bytes {
                    let _ = sender.send(StreamMessage::Failure(format!(
                        "command output line exceeded {max_line_bytes} bytes"
                    )));
                    return;
                }
                if terminated {
                    if pending.last() == Some(&b'\r') {
                        pending.pop();
                    }
                    let line = String::from_utf8_lossy(&pending).into_owned();
                    if sender.send(StreamMessage::Line(line)).is_err() {
                        return;
                    }
                    pending.clear();
                }
            }
        }
    })
}

#[cfg(unix)]
fn configure_child(command: &mut Command, limits: Option<ChildLimits>) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    if let Some(limits) = limits {
        // SAFETY: pre_exec performs only async-signal-safe setrlimit syscalls.
        unsafe {
            command.pre_exec(move || {
                set_limit(libc::RLIMIT_AS, limits.address_space_bytes)?;
                set_limit(libc::RLIMIT_FSIZE, limits.file_bytes)?;
                set_limit(libc::RLIMIT_NOFILE, limits.open_files)?;
                set_limit(libc::RLIMIT_CPU, limits.cpu_seconds)?;
                Ok(())
            });
        }
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
    // SAFETY: the child was placed in its own process group immediately before
    // spawn, and kill receives only that known process-group identifier.
    unsafe {
        libc::kill(process_group, libc::SIGKILL);
    }
}

fn sanitize_child_environment(command: &mut Command) {
    for name in ["RUSTGRID_WORKER_API_KEY", "GITHUB_TOKEN", "GH_TOKEN"] {
        command.env_remove(name);
    }
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
    fn streaming_reader_rejects_an_unbounded_line() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let reader =
            spawn_bounded_line_reader(std::io::Cursor::new(vec![b'x'; 1024]), 4096, 128, sender);
        match receiver.recv().unwrap() {
            StreamMessage::Failure(message) => assert!(message.contains("line exceeded")),
            StreamMessage::Line(_) => panic!("oversized line should not be emitted"),
        }
        reader.join().unwrap();
    }
}
