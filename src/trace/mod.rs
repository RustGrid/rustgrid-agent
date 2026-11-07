//! Traces Claude Code sessions run in a person's IDE onto RustGrid tickets.
//!
//! Claude Code hooks, installed through managed settings, call
//! `rustgrid-agent trace hook <event>` with the hook's JSON on stdin. A hook
//! only records what it was told and returns at once; a detached
//! `rustgrid-agent trace flush` then reads the session transcript from where it
//! last stopped, redacts it, and reports it to the session's interactive run.
//!
//! A session must be linked to a ticket before Claude sees a prompt: by the
//! branch name (`agent/add-354-…`, `rustgrid/ADD-12`) or with
//! `/rustgrid-link ADD-354`. Until then `UserPromptSubmit` blocks the prompt.
//! RustGrid being unreachable never blocks: data waits on disk and is retried.

pub mod redact;
pub mod transcript;

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::LazyLock,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    api::{self, RustGridClient},
    config::AppContext,
    lifecycle::StepStatus,
    telemetry::{
        CaptureGranularity, ExecutionSnapshot, ExecutionStatus, ModelCallSnapshot, ModelCallStatus,
        TelemetryBatch, TelemetryEvent, TelemetryPayload, UsageSource, now_rfc3339, stable_uuid,
    },
};
use transcript::{TraceItem, TranslatorState};

const AGENT_NAME: &str = "Claude Code (IDE)";
/// How long a person waits for `/rustgrid-link` to answer.
const LINK_TIMEOUT: Duration = Duration::from_secs(8);
/// Lines read per flush pass before saving progress.
const LINES_PER_SAVE: usize = 25;

static LINK_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*/?rustgrid-link\s+([a-z][a-z0-9]{1,9}-[0-9]+)\s*$")
        .expect("link command regex must compile")
});
static BRANCH_TICKET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[/_-])([a-z][a-z0-9]{1,9}-[0-9]+)(?:$|[/_-])")
        .expect("branch ticket regex must compile")
});

/// What Claude Code passes every hook on stdin.
#[derive(Debug, Default, Deserialize)]
pub struct HookInput {
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// What a hook tells Claude Code.
#[derive(Debug, PartialEq, Eq)]
pub enum HookOutcome {
    Continue,
    /// Exit code 2: the prompt is not sent and the reason is shown.
    Block(String),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct LinkedTicket {
    key: String,
    id: Option<String>,
    title: Option<String>,
    /// Taken from the branch name and not yet confirmed by RustGrid.
    #[serde(default)]
    unconfirmed: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Session {
    session_id: String,
    transcript_path: Option<PathBuf>,
    cwd: Option<PathBuf>,
    repository: Option<String>,
    branch: Option<String>,
    start_sha: Option<String>,
    claude_code_version: Option<String>,
    ticket: Option<LinkedTicket>,
    /// Why the branch did not link, shown when a prompt is blocked.
    link_problem: Option<String>,
    run_id: Option<String>,
    /// A run to end once the transcript up to the relink is reported.
    #[serde(default)]
    previous_run: Option<PreviousRun>,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    translator: TranslatorState,
    #[serde(default)]
    turn: u32,
    #[serde(default)]
    calls: u32,
    #[serde(default)]
    totals: Totals,
    #[serde(default)]
    execution_reported: bool,
    #[serde(default)]
    pull_requests: Vec<String>,
    #[serde(default)]
    ended: bool,
    #[serde(default)]
    finalized: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PreviousRun {
    run_id: String,
    start_sha: Option<String>,
    pull_requests: Vec<String>,
    /// Transcript bytes before the relink belong to this run.
    until_offset: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Totals {
    input: u64,
    cached: u64,
    uncached: u64,
    output: u64,
}

/// `~/.rustgrid/trace`, private to the person.
fn trace_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let dir = PathBuf::from(home).join(".rustgrid").join("trace");
    fs::create_dir_all(&dir)?;
    let mut permissions = fs::metadata(&dir)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
    fs::set_permissions(&dir, permissions)?;
    Ok(dir)
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

impl Session {
    fn path(dir: &Path, session_id: &str) -> PathBuf {
        dir.join(format!("{session_id}.json"))
    }

    fn load(dir: &Path, session_id: &str) -> Result<Self> {
        match fs::read_to_string(Self::path(dir, session_id)) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                session_id: session_id.to_owned(),
                ..Self::default()
            }),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, dir: &Path) -> Result<()> {
        let path = Self::path(dir, &self.session_id);
        let temporary = path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        fs::rename(temporary, path)?;
        Ok(())
    }
}

/// An exclusive lock on one session's state, held while it is changed and
/// released when the file closes.
struct SessionLock {
    _file: File,
}

impl SessionLock {
    fn acquire(dir: &Path, session_id: &str, name: &str, wait: bool) -> Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(dir.join(format!("{session_id}.{name}.lock")))?;
        let mut flags = libc::LOCK_EX;
        if !wait {
            flags |= libc::LOCK_NB;
        }
        // SAFETY: flock on a file descriptor this struct owns.
        if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
            Ok(Some(Self { _file: file }))
        } else {
            Ok(None)
        }
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// `owner/repo` from a GitHub-style remote URL.
fn repository_from_remote(remote: &str) -> Option<String> {
    let path = remote
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit_once([':', '/'])
        .map(|(rest, name)| {
            let owner = rest.rsplit(['/', ':']).next().unwrap_or_default();
            format!("{owner}/{name}")
        })?;
    (!path.starts_with('/')).then_some(path)
}

/// Ticket keys a branch name may carry, most specific first.
pub fn ticket_keys_in_branch(branch: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut start = 0;
    while let Some(found) = BRANCH_TICKET.captures_at(branch, start) {
        let key = found[1].to_ascii_uppercase();
        if !keys.contains(&key) {
            keys.push(key);
        }
        start = found.get(1).map_or(branch.len(), |key| key.end());
    }
    keys
}

fn is_logged_in(config_path: &Path) -> bool {
    fs::read_to_string(config_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|config| {
            config
                .get("worker_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|id| !id.is_empty())
}

fn start_body(session: &Session, key: &str) -> Value {
    json!({
        "ticket_key": key,
        "claude_session_id": session.session_id,
        "repository": session.repository,
        "branch": session.branch,
        "head_sha": session.start_sha,
        "claude_code_version": session.claude_code_version,
        "agent_version": env!("CARGO_PKG_VERSION"),
    })
}

/// Handles one Claude Code hook. Never fails the hook for a tracing problem:
/// only a missing ticket link or login blocks a prompt.
pub fn hook(config_path: &Path, event: &str, input: HookInput) -> Result<HookOutcome> {
    hook_in(&trace_dir()?, config_path, event, input, true)
}

fn hook_in(
    dir: &Path,
    config_path: &Path,
    event: &str,
    input: HookInput,
    spawn: bool,
) -> Result<HookOutcome> {
    if !valid_session_id(&input.session_id) {
        bail!("Claude Code sent an invalid session id");
    }
    let dir = dir.to_path_buf();
    let _guard = SessionLock::acquire(&dir, &input.session_id, "state", true)?;
    let mut session = Session::load(&dir, &input.session_id)?;
    if input.transcript_path.is_some() {
        session.transcript_path = input.transcript_path.clone();
    }
    if input.cwd.is_some() {
        session.cwd = input.cwd.clone();
    }
    if session.start_sha.is_none()
        && let Some(cwd) = session.cwd.clone()
    {
        session.start_sha = git(&cwd, &["rev-parse", "HEAD"]);
        session.branch = git(&cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
        session.repository = git(&cwd, &["remote", "get-url", "origin"])
            .as_deref()
            .and_then(repository_from_remote);
    }
    if session.ticket.is_none()
        && session.link_problem.is_none()
        && let Some(key) = session
            .branch
            .as_deref()
            .and_then(|branch| ticket_keys_in_branch(branch).into_iter().next())
    {
        session.ticket = Some(LinkedTicket {
            key,
            unconfirmed: true,
            ..LinkedTicket::default()
        });
    }

    let outcome = match event {
        "UserPromptSubmit" => {
            let prompt = input.prompt.as_deref().unwrap_or_default();
            if let Some(found) = LINK_COMMAND.captures(prompt) {
                let key = found[1].to_ascii_uppercase();
                HookOutcome::Block(link(config_path, &mut session, &key))
            } else if !is_logged_in(config_path) {
                HookOutcome::Block(
                    "RustGrid traces this session, but this computer is not signed in. Run `rustgrid-agent login`, then send your prompt again.".to_owned(),
                )
            } else if session.ticket.is_none() {
                HookOutcome::Block(match session.link_problem.as_deref() {
                    Some(problem) => format!("{problem} Link the ticket you are working on: /rustgrid-link ADD-123"),
                    None => "Link this session to the RustGrid ticket you are working on: /rustgrid-link ADD-123 (or work on a branch named after it, like rustgrid/ADD-123-short-title).".to_owned(),
                })
            } else {
                HookOutcome::Continue
            }
        }
        "SessionEnd" => {
            session.ended = true;
            HookOutcome::Continue
        }
        _ => HookOutcome::Continue,
    };
    session.save(&dir)?;
    fs::write(dir.join(format!("{}.dirty", session.session_id)), b"")?;
    if spawn {
        spawn_flush(config_path, &session.session_id);
    }
    Ok(outcome)
}

/// Links the session to a ticket now, while the person waits, and returns
/// what to tell them.
fn link(config_path: &Path, session: &mut Session, key: &str) -> String {
    let context = match AppContext::load(config_path) {
        Ok(context) => context,
        Err(_) => return "This computer is not signed in to RustGrid. Run `rustgrid-agent login`, then link again.".to_owned(),
    };
    let client = match RustGridClient::with_timeout(&context, LINK_TIMEOUT) {
        Ok(client) => client,
        Err(error) => return format!("Could not reach RustGrid to link {key}: {error}"),
    };
    match client.start_interactive_run(start_body(session, key), true) {
        Ok(started) => {
            if let Some(run_id) = session.run_id.take()
                && run_id != started.run.id
            {
                let until_offset = session
                    .transcript_path
                    .as_deref()
                    .and_then(|path| fs::metadata(path).ok())
                    .map_or(0, |metadata| metadata.len());
                session.previous_run = Some(PreviousRun {
                    run_id,
                    start_sha: session.start_sha.clone(),
                    pull_requests: std::mem::take(&mut session.pull_requests),
                    until_offset,
                });
                // The new ticket's changes are counted from here.
                session.start_sha = session
                    .cwd
                    .as_deref()
                    .and_then(|cwd| git(cwd, &["rev-parse", "HEAD"]))
                    .or(session.start_sha.take());
            }
            session.run_id = Some(started.run.id);
            session.ticket = Some(LinkedTicket {
                key: started.ticket.key.clone(),
                id: Some(started.ticket.id),
                title: Some(started.ticket.title.clone()),
                unconfirmed: false,
            });
            session.link_problem = None;
            session.execution_reported = false;
            session.ended = false;
            session.finalized = false;
            format!(
                "Linked to {} · {}. RustGrid will trace this session on that ticket; send your prompt.",
                started.ticket.key, started.ticket.title
            )
        }
        Err(error) if api::http_status(&error) == Some(404) => {
            format!("{key} is not a RustGrid ticket you can open. Check the key and link again.")
        }
        Err(error) => format!("Could not link {key}: {error}"),
    }
}

fn spawn_flush(config_path: &Path, session_id: &str) {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(executable);
    command
        .arg("--config")
        .arg(config_path)
        .args(["trace", "flush", "--session", session_id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid only detaches the child from Claude Code's process group,
    // so the upload outlives the hook.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = command.spawn();
}

/// Reports everything new in the session transcript. Only one flush runs per
/// session; it repeats while hooks keep marking the session dirty.
pub fn flush(config_path: &Path, session_id: &str) -> Result<()> {
    if !valid_session_id(session_id) {
        bail!("invalid session id");
    }
    let dir = trace_dir()?;
    let Some(_flusher) = SessionLock::acquire(&dir, session_id, "flush", false)? else {
        return Ok(());
    };
    let context = AppContext::load(config_path)?;
    let client = RustGridClient::new(&context)?;
    let dirty = dir.join(format!("{session_id}.dirty"));
    for _ in 0..20 {
        let _ = fs::remove_file(&dirty);
        flush_pass(&client, &dir, session_id)?;
        if !dirty.exists() {
            break;
        }
    }
    Ok(())
}

fn flush_pass(client: &RustGridClient, dir: &Path, session_id: &str) -> Result<()> {
    let snapshot = {
        let _guard = SessionLock::acquire(dir, session_id, "state", true)?;
        Session::load(dir, session_id)?
    };
    let mut session = snapshot;

    // Confirm a ticket taken from the branch name.
    if session.run_id.is_none()
        && let Some(ticket) = session.ticket.clone()
    {
        match client.start_interactive_run(start_body(&session, &ticket.key), false) {
            Ok(started) => {
                session.run_id = Some(started.run.id);
                session.ticket = Some(LinkedTicket {
                    key: started.ticket.key,
                    id: Some(started.ticket.id),
                    title: Some(started.ticket.title),
                    unconfirmed: false,
                });
            }
            Err(error) if matches!(api::http_status(&error), Some(403 | 404)) => {
                session.link_problem = Some(format!(
                    "The branch names {}, which is not a RustGrid ticket you can open.",
                    ticket.key
                ));
                session.ticket = None;
                return save_merged(dir, &session);
            }
            Err(error) => return Err(error),
        }
        save_merged(dir, &session)?;
    }
    let Some(run_id) = session.run_id.clone() else {
        return Ok(());
    };
    let ticket_id = session.ticket.as_ref().and_then(|ticket| ticket.id.clone());

    if !session.execution_reported {
        report_execution(client, &run_id, ExecutionStatus::Running)?;
        session.execution_reported = true;
    }

    if let Some(path) = session.transcript_path.clone()
        && let Ok(mut file) = File::open(&path)
    {
        let length = file.metadata()?.len();
        if session.offset > length {
            session.offset = 0;
        }
        file.seek(SeekFrom::Start(session.offset))?;
        let mut buffer = Vec::new();
        file.take(length - session.offset)
            .read_to_end(&mut buffer)?;
        let mut consumed = 0usize;
        let mut since_save = 0usize;
        while let Some(newline) = buffer[consumed..].iter().position(|byte| *byte == b'\n') {
            let line_end = consumed + newline + 1;
            let line = String::from_utf8_lossy(&buffer[consumed..line_end - 1]).into_owned();
            // Lines written before a relink still belong to the earlier ticket.
            if let Some(previous) = session.previous_run.clone()
                && session.offset >= previous.until_offset
            {
                end_run(
                    client,
                    &session,
                    &previous.run_id,
                    previous.start_sha.as_deref(),
                    &previous.pull_requests,
                )?;
                session.previous_run = None;
            }
            let (run, ticket) = match &session.previous_run {
                Some(previous) => (previous.run_id.clone(), None),
                None => (run_id.clone(), ticket_id.clone()),
            };
            let mut translator = session.translator.clone();
            let items = transcript::translate(&mut translator, &line);
            for item in &items {
                report_item(client, &mut session, &run, ticket.as_deref(), item)?;
            }
            session.translator = translator;
            consumed = line_end;
            session.offset += (newline + 1) as u64;
            since_save += 1;
            if since_save >= LINES_PER_SAVE {
                save_merged(dir, &session)?;
                since_save = 0;
            }
        }
        save_merged(dir, &session)?;
    }

    if let Some(previous) = session.previous_run.clone()
        && (session.offset >= previous.until_offset || session.ended)
    {
        end_run(
            client,
            &session,
            &previous.run_id,
            previous.start_sha.as_deref(),
            &previous.pull_requests,
        )?;
        session.previous_run = None;
        save_merged(dir, &session)?;
    }
    if session.ended && !session.finalized {
        report_execution(client, &run_id, ExecutionStatus::Succeeded)?;
        end_run(
            client,
            &session,
            &run_id,
            session.start_sha.as_deref(),
            &session.pull_requests,
        )?;
        session.finalized = true;
        save_merged(dir, &session)?;
    }
    Ok(())
}

/// Saves flush progress without losing link changes a hook made meanwhile.
fn save_merged(dir: &Path, flushed: &Session) -> Result<()> {
    let _guard = SessionLock::acquire(dir, &flushed.session_id, "state", true)?;
    let mut current = Session::load(dir, &flushed.session_id)?;
    let relinked = current.run_id.is_some() && current.run_id != flushed.run_id;
    if !relinked {
        current.run_id = flushed.run_id.clone();
        current.ticket = flushed.ticket.clone();
        current.link_problem = flushed.link_problem.clone();
        current.pull_requests = flushed.pull_requests.clone();
        current.finalized = flushed.finalized;
    }
    current.previous_run = flushed
        .previous_run
        .clone()
        .filter(|_| current.previous_run.is_some());
    current.offset = flushed.offset;
    current.translator = flushed.translator.clone();
    current.turn = flushed.turn;
    current.calls = flushed.calls;
    current.totals = flushed.totals.clone();
    current.execution_reported = flushed.execution_reported && !relinked;
    current.save(dir)
}

fn report_item(
    client: &RustGridClient,
    session: &mut Session,
    run_id: &str,
    ticket_id: Option<&str>,
    item: &TraceItem,
) -> Result<()> {
    let key = |id: &str| format!("ide-{}", stable_uuid(&format!("{run_id}:{id}")));
    match item {
        TraceItem::Prompt {
            id,
            text,
            images,
            at,
        } => {
            session.turn = session.turn.saturating_add(1);
            client.publish_trace_event(
                run_id,
                "message",
                json!({
                    "event_type": "user.prompt",
                    "message": text,
                    "images": images,
                    "turn": session.turn,
                    "at": at,
                    "source": "claude-code-ide"
                }),
                &key(id),
            )
        }
        TraceItem::AgentMessage { id, text, at } => client.publish_trace_event(
            run_id,
            "message",
            json!({"event_type": "agent.message", "message": text, "turn": session.turn, "at": at}),
            &key(id),
        ),
        TraceItem::Phase { id, message } => client.append_step(
            run_id,
            u64::from(stable_uuid(&format!("{run_id}:{id}")).as_u128() as u32),
            &format!("ide_{}", message.to_ascii_lowercase().replace(' ', "_")),
            StepStatus::Running,
            message,
            Some(json!({"source": "claude-code-ide", "turn": session.turn})),
        ),
        TraceItem::Command {
            id,
            command,
            exit_code,
            output,
        } => client.publish_trace_event(
            run_id,
            "progress",
            json!({
                "event_type": "ide.command",
                "message": if *exit_code == Some(0) { "Ran a command" } else { "A command failed" },
                "command": command,
                "exit_code": exit_code,
                "output": output,
                "turn": session.turn
            }),
            &key(id),
        ),
        TraceItem::FileChange { id, paths } => client.publish_trace_event(
            run_id,
            "progress",
            json!({
                "event_type": "ide.file_change",
                "message": format!("Edited {}", paths.join(", ")),
                "paths": paths,
                "turn": session.turn
            }),
            &key(id),
        ),
        TraceItem::Tool {
            id,
            tool,
            target,
            failed,
        } => client.publish_trace_event(
            run_id,
            "progress",
            json!({
                "event_type": "ide.tool",
                "message": match target {
                    Some(target) => format!("{tool} {target}"),
                    None => tool.clone(),
                },
                "tool": tool,
                "target": target,
                "failed": failed,
                "turn": session.turn
            }),
            &key(id),
        ),
        TraceItem::ModelCall {
            message_id,
            request_id,
            model,
            at,
            usage,
        } => {
            session.calls = session.calls.saturating_add(1);
            let count = |field: &str| usage.get(field).and_then(Value::as_u64).unwrap_or(0);
            let input = count("input_tokens");
            let cached = count("cached_input_tokens");
            let output = count("output_tokens");
            session.totals.input += input;
            session.totals.cached += cached;
            session.totals.uncached += input.saturating_sub(cached);
            session.totals.output += output;
            let at = at.clone().unwrap_or_else(now_rfc3339);
            let call_id = stable_uuid(&format!("ide:{run_id}:call:{message_id}"));
            let snapshot = ModelCallSnapshot {
                id: call_id,
                execution_id: execution_id(run_id),
                agent_id: None,
                agent_name: Some(AGENT_NAME.to_owned()),
                phase_id: None,
                phase_name: Some("interactive".to_owned()),
                turn_index: session.turn,
                call_index: session.calls,
                provider: "anthropic".to_owned(),
                model: model.clone(),
                started_at: at.clone(),
                completed_at: Some(at.clone()),
                status: ModelCallStatus::Success,
                input_tokens: Some(input),
                cached_input_tokens: Some(cached),
                uncached_input_tokens: Some(input.saturating_sub(cached)),
                output_tokens: Some(output),
                reasoning_tokens: usage.get("reasoning_output_tokens").and_then(Value::as_u64),
                total_tokens: usage.get("total_tokens").and_then(Value::as_u64),
                context_tokens_before_call: Some(input),
                peak_context_tokens_so_far: None,
                cumulative_input_tokens: session.totals.input,
                cumulative_cached_input_tokens: session.totals.cached,
                cumulative_uncached_input_tokens: session.totals.uncached,
                cumulative_output_tokens: session.totals.output,
                context_window_limit: None,
                finish_reason: None,
                retry_of_call_id: None,
                attempt_number: 1,
                provider_request_id: request_id.clone(),
                provider_usage_payload: usage.as_object().cloned(),
                usage_source: UsageSource::ProviderReported,
                capture_granularity: CaptureGranularity::ModelCall,
            };
            client.report_telemetry_batch(
                run_id,
                &TelemetryBatch::new(vec![TelemetryEvent {
                    event_id: stable_uuid(&format!(
                        "entity:{call_id}:event:model_call.completed:revision:1"
                    )),
                    entity_revision: 1,
                    occurred_at: at,
                    event_type: "model_call.completed".to_owned(),
                    payload: TelemetryPayload::ModelCall {
                        model_call: Box::new(snapshot),
                    },
                }]),
            )
        }
        TraceItem::PullRequest { url, number } => {
            if !session.pull_requests.contains(url) {
                session.pull_requests.push(url.clone());
            }
            if let (Some(ticket_id), Some(number)) = (ticket_id, number) {
                client.attach_pr(ticket_id, run_id, url, *number)?;
            }
            client.publish_trace_event(
                run_id,
                "progress",
                json!({"event_type": "ide.pull_request", "message": format!("Opened {url}"), "url": url}),
                &key(url),
            )
        }
    }
}

fn execution_id(run_id: &str) -> Uuid {
    stable_uuid(&format!("ide:{run_id}:execution"))
}

fn report_execution(client: &RustGridClient, run_id: &str, status: ExecutionStatus) -> Result<()> {
    let id = execution_id(run_id);
    let finished = !matches!(status, ExecutionStatus::Running);
    let (event_type, revision) = if finished {
        ("execution.completed", 2)
    } else {
        ("execution.started", 1)
    };
    let now = now_rfc3339();
    client.report_telemetry_batch(
        run_id,
        &TelemetryBatch::new(vec![TelemetryEvent {
            event_id: stable_uuid(&format!(
                "entity:{id}:event:{event_type}:revision:{revision}"
            )),
            entity_revision: revision,
            occurred_at: now.clone(),
            event_type: event_type.to_owned(),
            payload: TelemetryPayload::Execution {
                execution: ExecutionSnapshot {
                    id,
                    agent_id: None,
                    agent_name: Some(AGENT_NAME.to_owned()),
                    role: Some("interactive".to_owned()),
                    started_at: now.clone(),
                    completed_at: finished.then_some(now),
                    status,
                },
            },
        }]),
    )
}

/// Ends a run with what changed since the session started on this ticket.
fn end_run(
    client: &RustGridClient,
    session: &Session,
    run_id: &str,
    start_sha: Option<&str>,
    pull_requests: &[String],
) -> Result<()> {
    let mut files = Vec::new();
    let mut commits = Vec::new();
    if let (Some(cwd), Some(start)) = (session.cwd.as_deref(), start_sha) {
        let mut changed: Vec<String> = git(cwd, &["diff", "--name-only", start])
            .into_iter()
            .chain(git(cwd, &["ls-files", "--others", "--exclude-standard"]))
            .flat_map(|text| text.lines().map(str::to_owned).collect::<Vec<_>>())
            .collect();
        changed.sort();
        changed.dedup();
        files = changed
            .into_iter()
            .take(1000)
            .map(|path| redact::redact(&path))
            .collect();
        if let Some(log) = git(
            cwd,
            &["log", "--format=%H%x1f%s", &format!("{start}..HEAD")],
        ) {
            commits = log
                .lines()
                .take(200)
                .filter_map(|line| line.split_once('\u{1f}'))
                .map(|(sha, subject)| json!({"sha": sha, "subject": redact::redact_truncated(subject, 400)}))
                .collect();
        }
    }
    let mut body = json!({"files_changed": files, "commits": commits});
    if let Some(url) = pull_requests.last() {
        body["pull_request_url"] = json!(url);
    }
    client.end_interactive_run(run_id, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_carry_ticket_keys() {
        assert_eq!(
            ticket_keys_in_branch("agent/add-354-do-a-final-adjustment"),
            vec!["ADD-354"]
        );
        assert_eq!(ticket_keys_in_branch("rustgrid/ADD-12"), vec!["ADD-12"]);
        assert_eq!(ticket_keys_in_branch("ADD-7_fix"), vec!["ADD-7"]);
        assert!(ticket_keys_in_branch("rustgrid/interactive-runs").is_empty());
        assert!(ticket_keys_in_branch("main").is_empty());
    }

    #[test]
    fn link_commands_are_recognised() {
        assert_eq!(
            &LINK_COMMAND.captures("/rustgrid-link add-354").unwrap()[1],
            "add-354"
        );
        assert_eq!(
            &LINK_COMMAND.captures("rustgrid-link ADD-1 ").unwrap()[1],
            "ADD-1"
        );
        assert!(
            LINK_COMMAND
                .captures("please /rustgrid-link ADD-1 and fix")
                .is_none()
        );
    }

    #[test]
    fn remotes_name_their_repository() {
        assert_eq!(
            repository_from_remote("git@github.com:RustGrid/rustgrid.git").as_deref(),
            Some("RustGrid/rustgrid")
        );
        assert_eq!(
            repository_from_remote("https://github.com/RustGrid/rustgrid-agentops").as_deref(),
            Some("RustGrid/rustgrid-agentops")
        );
    }

    fn prompt(dir: &Path, config: &Path, cwd: &Path, text: &str) -> HookOutcome {
        hook_in(
            dir,
            config,
            "UserPromptSubmit",
            HookInput {
                session_id: "s-1".into(),
                transcript_path: None,
                cwd: Some(cwd.to_path_buf()),
                prompt: Some(text.into()),
            },
            false,
        )
        .unwrap()
    }

    #[test]
    fn prompts_wait_for_a_login_and_a_linked_ticket() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("trace");
        fs::create_dir_all(&dir).unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let config = root.path().join("config.json");

        fs::write(&config, "{}").unwrap();
        let HookOutcome::Block(reason) = prompt(&dir, &config, &repo, "fix it") else {
            panic!("a signed-out session must not prompt");
        };
        assert!(reason.contains("rustgrid-agent login"), "{reason}");

        fs::write(
            &config,
            r#"{"worker_id":"9b0f4c4e-8f43-4a07-9a3e-3d2b0c3e1f10"}"#,
        )
        .unwrap();
        let HookOutcome::Block(reason) = prompt(&dir, &config, &repo, "fix it") else {
            panic!("an unlinked session must not prompt");
        };
        assert!(reason.contains("/rustgrid-link ADD-123"), "{reason}");

        // A branch named after a ticket links the session.
        let mut session = Session::load(&dir, "s-1").unwrap();
        session.ticket = Some(LinkedTicket {
            key: "ADD-354".into(),
            unconfirmed: true,
            ..LinkedTicket::default()
        });
        session.save(&dir).unwrap();
        assert_eq!(
            prompt(&dir, &config, &repo, "fix it"),
            HookOutcome::Continue
        );
        assert!(dir.join("s-1.dirty").exists());
    }

    #[test]
    fn session_ids_cannot_escape_the_trace_directory() {
        assert!(valid_session_id("b1d026a4-cf3a-4f40-a325-5d5cdcea34d4"));
        assert!(!valid_session_id("../../etc/passwd"));
        assert!(!valid_session_id(""));
    }
}
