use std::{
    ffi::OsStr,
    io::{BufRead, BufReader, Write},
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
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
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
    streaming_lines_cancellable(command, cwd, stdin_text, &running, on_line)
}

pub fn streaming_lines_cancellable<F>(
    command: &str,
    cwd: &Path,
    stdin_text: Option<&str>,
    running: &AtomicBool,
    mut on_line: F,
) -> Result<ExitStatus>
where
    F: FnMut(&str) -> Result<()>,
{
    let mut parts = parse(command)?;
    add_codex_json_flag(&mut parts);
    println!("  $ {}", display_command(&parts));
    let mut child = Command::new(&parts[0])
        .args(&parts[1..])
        .current_dir(cwd)
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stdout(Stdio::piped())
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

    let stdout = child.stdout.take().context("failed to open child stdout")?;
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    loop {
        if !running.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            bail!("command cancelled");
        }
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                let line = line.context("failed to read command stdout")?;
                if let Err(error) = on_line(&line) {
                    let _ = child.kill();
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
}
