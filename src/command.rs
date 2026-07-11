use std::{
    ffi::OsStr,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
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
    capture_cancellable_with_environment(command, cwd, running, timeout, max_output_bytes, None)
}

pub fn capture_cancellable_with_environment(
    command: &str,
    cwd: &Path,
    running: &AtomicBool,
    timeout: Duration,
    max_output_bytes: usize,
    environment_allowlist: Option<&[String]>,
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
    configure_process_group(&mut command);
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
            bail!("command cancelled");
        }
        if started.elapsed() >= timeout {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            bail!("command timed out after {} seconds", timeout.as_secs());
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
    Ok(CommandOutput {
        status,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
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
            Stdio::inherit()
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
    streaming_lines_cancellable_with_environment(
        command,
        cwd,
        stdin_text,
        running,
        timeout,
        max_output_bytes,
        None,
        on_line,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn streaming_lines_cancellable_with_environment<F>(
    command: &str,
    cwd: &Path,
    stdin_text: Option<&str>,
    running: &AtomicBool,
    timeout: Duration,
    max_output_bytes: usize,
    environment_allowlist: Option<&[String]>,
    mut on_line: F,
) -> Result<ExitStatus>
where
    F: FnMut(&str) -> Result<()>,
{
    let mut parts = parse(command)?;
    add_codex_json_flag(&mut parts);
    println!("  $ {}", display_command(&parts));
    let mut command = Command::new(&parts[0]);
    command
        .args(&parts[1..])
        .current_dir(cwd)
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    sanitize_child_environment(&mut command);
    if let Some(allowlist) = environment_allowlist {
        apply_environment_allowlist(&mut command, allowlist);
    }
    configure_process_group(&mut command);
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
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let started = std::time::Instant::now();
    let mut output_bytes = 0usize;
    loop {
        if !running.load(Ordering::SeqCst) || shutdown::requested() {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            let _ = reader.join();
            bail!("command cancelled");
        }
        if started.elapsed() >= timeout {
            terminate_process_tree(&mut child);
            let _ = child.wait();
            let _ = reader.join();
            bail!("command timed out after {} seconds", timeout.as_secs());
        }
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                let line = line.context("failed to read command stdout")?;
                output_bytes = output_bytes.saturating_add(line.len());
                if output_bytes > max_output_bytes {
                    terminate_process_tree(&mut child);
                    let _ = child.wait();
                    let _ = reader.join();
                    bail!("command output exceeded {max_output_bytes} bytes");
                }
                if let Err(error) = on_line(&line) {
                    terminate_process_tree(&mut child);
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(error);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child
                    .try_wait()
                    .context("failed while checking command")?
                    .is_some()
                {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = reader.join();
    child.wait().context("failed while waiting for command")
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

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
    for name in ["RUSTGRID_API_KEY", "GITHUB_TOKEN", "GH_TOKEN"] {
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

fn read_bounded(reader: &mut impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
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
    Ok(output)
}

#[cfg(not(unix))]
fn terminate_process_tree(child: &mut std::process::Child) {
    let _ = child.kill();
}

fn add_codex_json_flag(parts: &mut Vec<String>) {
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
        .map(|part| {
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
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn bounded_reader_drains_and_marks_truncation() {
        let mut input = std::io::Cursor::new(vec![b'x'; 1024]);
        let output = read_bounded(&mut input, 32).unwrap();
        assert!(output.starts_with(&[b'x'; 32]));
        assert!(String::from_utf8_lossy(&output).contains("output truncated"));
    }
}
