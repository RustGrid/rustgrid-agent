use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{api::RustGridClient, token_consumption::TokenConsumption};

pub const TELEMETRY_VERSION: &str = "1.0";
const BATCH_SIZE: usize = 100;
const CHANNEL_CAPACITY: usize = 512;
const FLUSH_INTERVAL: Duration = Duration::from_millis(500);
const MAX_RAW_USAGE_BYTES: usize = 4 * 1024;
const MAX_OUTBOX_BYTES: u64 = 64 * 1024 * 1024;
const TELEMETRY_NAMESPACE: Uuid = Uuid::from_u128(0x9dd66585_d9bd_461e_a47a_671b061a3ef8);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TelemetryBatch {
    pub telemetry_version: String,
    pub events: Vec<TelemetryEvent>,
}

impl TelemetryBatch {
    pub fn new(events: Vec<TelemetryEvent>) -> Self {
        Self {
            telemetry_version: TELEMETRY_VERSION.to_owned(),
            events,
        }
    }

    pub fn stable_id(&self) -> Uuid {
        let mut input = String::new();
        for event in &self.events {
            input.push_str(event.event_id.as_hyphenated().to_string().as_str());
        }
        stable_uuid(&format!("batch:{input}"))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TelemetryEvent {
    pub event_id: Uuid,
    pub entity_revision: u32,
    pub occurred_at: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(flatten)]
    pub payload: TelemetryPayload,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum TelemetryPayload {
    Execution { execution: ExecutionSnapshot },
    Phase { phase: PhaseSnapshot },
    Turn { turn: TurnSnapshot },
    ModelCall { model_call: ModelCallSnapshot },
    ToolCall { tool_call: ToolCallSnapshot },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionSnapshot {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub status: ExecutionStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PhaseSnapshot {
    pub id: Uuid,
    pub execution_id: Uuid,
    pub name: String,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub status: ExecutionStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TurnSnapshot {
    pub id: Uuid,
    pub execution_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_id: Option<Uuid>,
    pub turn_index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub status: ExecutionStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelCallSnapshot {
    pub id: Uuid,
    pub execution_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_name: Option<String>,
    pub turn_index: u32,
    pub call_index: u32,
    pub provider: String,
    pub model: String,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub status: ModelCallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens_before_call: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_of_call_id: Option<Uuid>,
    pub attempt_number: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_usage_payload: Option<Map<String, Value>>,
    pub usage_source: UsageSource,
    pub capture_granularity: CaptureGranularity,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallStatus {
    InProgress,
    Success,
    Error,
    Cancelled,
    Timeout,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    ProviderReported,
    WorkerCalculated,
    Estimated,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureGranularity {
    ModelCall,
    TurnAggregate,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCallSnapshot {
    pub id: Uuid,
    pub execution_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_name: Option<String>,
    pub turn_index: u32,
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_category: Option<String>,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub status: ToolCallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_model_call_id: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    InProgress,
    Success,
    Error,
    Cancelled,
    Timeout,
}

#[derive(Clone, Copy, Debug)]
pub enum SessionOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Timeout,
}

#[derive(Clone, Debug)]
pub struct CodexInvocation {
    pub run_id: String,
    pub execution_sequence: u64,
    pub phase_name: String,
    pub provider: String,
    pub model: String,
    pub attempt_number: u32,
    pub retry_of_call_id: Option<Uuid>,
}

#[derive(Clone, Debug, Default)]
struct NormalizedUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    total_tokens: Option<u64>,
    source: Option<UsageSource>,
    raw: Option<Map<String, Value>>,
}

pub(crate) trait UsageAdapter {
    fn normalize_usage(&self, event: &Value) -> NormalizedUsage;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CodexUsageAdapter;

impl UsageAdapter for CodexUsageAdapter {
    fn normalize_usage(&self, event: &Value) -> NormalizedUsage {
        let Some(usage) = event.get("usage").and_then(Value::as_object) else {
            return NormalizedUsage {
                source: Some(UsageSource::Unavailable),
                ..NormalizedUsage::default()
            };
        };
        let input_tokens = optional_count(usage, "input_tokens");
        let mut cached_input_tokens = optional_count(usage, "cached_input_tokens");
        let output_tokens = optional_count(usage, "output_tokens");
        let reasoning_tokens = optional_count(usage, "reasoning_output_tokens")
            .or_else(|| optional_count(usage, "reasoning_tokens"));
        if input_tokens.zip(cached_input_tokens).is_some_and(|(input, cached)| cached > input) {
            cached_input_tokens = None;
        }
        let reported_total = optional_count(usage, "total_tokens");
        let calculated_total = input_tokens
            .zip(output_tokens)
            .and_then(|(input, output)| input.checked_add(output));
        let total_tokens = reported_total.or(calculated_total);
        let source = if reported_total.is_some() {
            UsageSource::ProviderReported
        } else if calculated_total.is_some() {
            UsageSource::WorkerCalculated
        } else if input_tokens.is_some()
            || cached_input_tokens.is_some()
            || output_tokens.is_some()
            || reasoning_tokens.is_some()
        {
            UsageSource::ProviderReported
        } else {
            UsageSource::Unavailable
        };
        NormalizedUsage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
            reasoning_tokens,
            total_tokens,
            source: Some(source),
            raw: safe_usage_payload(usage),
        }
    }
}

fn optional_count(value: &Map<String, Value>, field: &str) -> Option<u64> {
    value.get(field).and_then(Value::as_u64)
}

fn safe_usage_payload(usage: &Map<String, Value>) -> Option<Map<String, Value>> {
    let safe = usage
        .iter()
        .filter(|(key, value)| {
            key.len() <= 80
                && key
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
                && matches!(value, Value::Null | Value::Bool(_) | Value::Number(_))
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<_, _>>();
    (!safe.is_empty()
        && serde_json::to_vec(&safe)
            .is_ok_and(|bytes| bytes.len() <= MAX_RAW_USAGE_BYTES))
    .then_some(safe)
}

#[derive(Clone)]
pub struct TelemetryEmitter {
    sender: SyncSender<EmitterMessage>,
    dropped_events: Arc<AtomicU64>,
}

enum EmitterMessage {
    Event {
        run_id: String,
        event: TelemetryEvent,
    },
    Flush(mpsc::Sender<()>),
}

#[derive(Debug, Deserialize, Serialize)]
struct OutboxBatch {
    run_id: String,
    batch: TelemetryBatch,
}

impl TelemetryEmitter {
    pub fn start(api: RustGridClient, outbox_root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&outbox_root).with_context(|| {
            format!(
                "could not create telemetry outbox {}",
                outbox_root.display()
            )
        })?;
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let dropped_events = Arc::new(AtomicU64::new(0));
        thread::Builder::new()
            .name("rustgrid-telemetry".into())
            .spawn(move || delivery_loop(api, outbox_root, receiver))
            .context("could not start telemetry delivery worker")?;
        Ok(Self {
            sender,
            dropped_events,
        })
    }

    pub fn emit(&self, run_id: &str, event: TelemetryEvent) {
        match self.sender.try_send(EmitterMessage::Event {
            run_id: run_id.to_owned(),
            event,
        }) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                let dropped = self.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped == 1 || dropped.is_multiple_of(100) {
                    eprintln!(
                        "[warning] telemetry buffer is full; dropped {dropped} event(s) without affecting the run"
                    );
                }
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                let dropped = self.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped == 1 {
                    eprintln!(
                        "[warning] telemetry delivery worker stopped; telemetry will be incomplete"
                    );
                }
            }
        }
    }

    pub fn flush_best_effort(&self, timeout: Duration) {
        let (sender, receiver) = mpsc::channel();
        if self
            .sender
            .try_send(EmitterMessage::Flush(sender))
            .is_err()
        {
            eprintln!("[warning] telemetry flush could not be queued; delivery remains best-effort");
            return;
        }
        if receiver.recv_timeout(timeout).is_err() {
            eprintln!(
                "[warning] telemetry flush did not finish within {}ms; durable batches will retry later",
                timeout.as_millis()
            );
        }
    }
}

fn delivery_loop(
    api: RustGridClient,
    outbox_root: PathBuf,
    receiver: mpsc::Receiver<EmitterMessage>,
) {
    replay_outbox(&api, &outbox_root);
    let mut pending_by_run = HashMap::<String, Vec<TelemetryEvent>>::new();
    loop {
        match receiver.recv_timeout(FLUSH_INTERVAL) {
            Ok(EmitterMessage::Event { run_id, event }) => {
                let events = pending_by_run.entry(run_id.clone()).or_default();
                events.push(event);
                if events.len() >= BATCH_SIZE {
                    let batch = TelemetryBatch::new(std::mem::take(events));
                    deliver_batch(&api, &outbox_root, &run_id, batch);
                }
            }
            Ok(EmitterMessage::Flush(acknowledge)) => {
                flush_pending(&api, &outbox_root, &mut pending_by_run);
                let _ = acknowledge.send(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                flush_pending(&api, &outbox_root, &mut pending_by_run);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                flush_pending(&api, &outbox_root, &mut pending_by_run);
                return;
            }
        }
    }
}

fn flush_pending(
    api: &RustGridClient,
    outbox_root: &Path,
    pending_by_run: &mut HashMap<String, Vec<TelemetryEvent>>,
) {
    for (run_id, events) in pending_by_run.iter_mut() {
        while !events.is_empty() {
            let remainder = events.split_off(events.len().min(BATCH_SIZE));
            let batch = TelemetryBatch::new(std::mem::replace(events, remainder));
            deliver_batch(api, outbox_root, run_id, batch);
        }
    }
    pending_by_run.retain(|_, events| !events.is_empty());
}

fn deliver_batch(
    api: &RustGridClient,
    outbox_root: &Path,
    run_id: &str,
    batch: TelemetryBatch,
) {
    let envelope = OutboxBatch {
        run_id: run_id.to_owned(),
        batch,
    };
    let persisted = persist_batch(outbox_root, &envelope).map_err(|error| {
        eprintln!("[warning] could not persist telemetry batch: {error:#}");
        error
    });
    if let Err(error) = api.report_telemetry_batch(run_id, &envelope.batch) {
        eprintln!("[warning] telemetry batch delivery failed without affecting the run: {error:#}");
        return;
    }
    if let Ok(path) = persisted
        && let Err(error) = fs::remove_file(&path)
    {
        eprintln!(
            "[warning] delivered telemetry batch but could not remove {}: {error}",
            path.display()
        );
    }
}

fn persist_batch(outbox_root: &Path, envelope: &OutboxBatch) -> Result<PathBuf> {
    if directory_bytes(outbox_root)? >= MAX_OUTBOX_BYTES {
        anyhow::bail!(
            "telemetry outbox reached its {} byte safety limit",
            MAX_OUTBOX_BYTES
        );
    }
    let run_dir = outbox_root.join(safe_path_component(&envelope.run_id));
    fs::create_dir_all(&run_dir)?;
    let batch_id = envelope.batch.stable_id();
    let path = run_dir.join(format!("{batch_id}.json"));
    if path.is_file() {
        return Ok(path);
    }
    let temporary = run_dir.join(format!(".{batch_id}.tmp"));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(envelope)?)?;
    file.sync_all()?;
    fs::rename(&temporary, &path)?;
    sync_directory(&run_dir)?;
    Ok(path)
}

fn replay_outbox(api: &RustGridClient, outbox_root: &Path) {
    let Ok(run_directories) = fs::read_dir(outbox_root) else {
        return;
    };
    for run_directory in run_directories.flatten() {
        if !run_directory.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(files) = fs::read_dir(run_directory.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let envelope = fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<OutboxBatch>(&bytes).ok());
            let Some(envelope) = envelope else {
                eprintln!(
                    "[warning] ignored invalid telemetry outbox batch {}",
                    path.display()
                );
                continue;
            };
            if api
                .report_telemetry_batch(&envelope.run_id, &envelope.batch)
                .is_ok()
            {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn directory_bytes(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        total = total.saturating_add(if metadata.is_dir() {
            directory_bytes(&entry.path())?
        } else {
            metadata.len()
        });
    }
    Ok(total)
}

fn safe_path_component(value: &str) -> String {
    let digest = hex::encode(Sha256::digest(value.as_bytes()));
    digest[..32].to_owned()
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

struct ActiveTurn {
    id: Uuid,
    model_call_id: Uuid,
    index: u32,
    started_at: String,
    context_tokens_before_call: Option<u64>,
    context_window_limit: Option<u64>,
}

struct ActiveTool {
    id: Uuid,
    source_id: String,
    name: String,
    category: String,
    turn_index: u32,
    started_at: String,
    request_bytes: Option<u64>,
    related_model_call_id: Option<Uuid>,
}

pub struct CodexTelemetrySession {
    invocation: CodexInvocation,
    emitter: Option<TelemetryEmitter>,
    execution_id: Uuid,
    phase_id: Uuid,
    started_at: String,
    agent_id: Option<Uuid>,
    next_turn_index: u32,
    active_turn: Option<ActiveTurn>,
    active_tools: HashMap<String, ActiveTool>,
    completed_tools: HashSet<String>,
    last_model_call_id: Option<Uuid>,
    legacy_delta: TokenConsumption,
    finished: bool,
}

impl CodexTelemetrySession {
    pub fn start(invocation: CodexInvocation, emitter: Option<TelemetryEmitter>) -> Self {
        let execution_id = stable_uuid(&format!(
            "run:{}:execution:{}",
            invocation.run_id, invocation.execution_sequence
        ));
        let phase_id = stable_uuid(&format!("execution:{execution_id}:phase:0"));
        let started_at = now_rfc3339();
        let session = Self {
            invocation,
            emitter,
            execution_id,
            phase_id,
            started_at: started_at.clone(),
            agent_id: None,
            next_turn_index: 0,
            active_turn: None,
            active_tools: HashMap::new(),
            completed_tools: HashSet::new(),
            last_model_call_id: None,
            legacy_delta: TokenConsumption::default(),
            finished: false,
        };
        session.emit(
            "execution.started",
            1,
            execution_id,
            &started_at,
            TelemetryPayload::Execution {
                execution: session.execution_snapshot(ExecutionStatus::Running, None),
            },
        );
        session.emit(
            "phase.started",
            1,
            phase_id,
            &started_at,
            TelemetryPayload::Phase {
                phase: session.phase_snapshot(ExecutionStatus::Running, None),
            },
        );
        session
    }

    pub fn observe_line(&mut self, line: &str) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let event_type = event.get("type").and_then(Value::as_str);
        match event_type {
            Some("thread.started") => {
                self.agent_id = event
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .and_then(|value| Uuid::parse_str(value).ok());
            }
            Some("turn.started") => {
                if self.active_turn.is_some() {
                    self.finish_turn(SessionOutcome::Failed, None, None);
                }
                self.start_turn(&event);
            }
            Some("turn.completed") => {
                if self.active_turn.is_none() {
                    self.start_turn(&event);
                }
                let usage = CodexUsageAdapter.normalize_usage(&event);
                self.finish_turn(SessionOutcome::Succeeded, Some(usage), event.as_object());
            }
            Some("turn.failed") => {
                if self.active_turn.is_none() {
                    self.start_turn(&event);
                }
                self.finish_turn(SessionOutcome::Failed, None, event.as_object());
            }
            Some("item.started") => {
                if let Some(item) = event.get("item") {
                    self.start_tool(item);
                }
            }
            Some("item.completed") => {
                if let Some(item) = event.get("item") {
                    self.complete_tool(item);
                }
            }
            _ => {}
        }
    }

    pub fn finish(&mut self, outcome: SessionOutcome) {
        if self.finished {
            return;
        }
        if self.active_turn.is_some() {
            self.finish_turn(outcome, None, None);
        } else if self.last_model_call_id.is_none() {
            self.start_turn(&Value::Null);
            self.finish_turn(outcome, None, None);
        }
        let completed_at = now_rfc3339();
        let status = execution_status(outcome);
        self.emit(
            "phase.completed",
            2,
            self.phase_id,
            &completed_at,
            TelemetryPayload::Phase {
                phase: self.phase_snapshot(status.clone(), Some(completed_at.clone())),
            },
        );
        self.emit(
            "execution.completed",
            2,
            self.execution_id,
            &completed_at,
            TelemetryPayload::Execution {
                execution: self.execution_snapshot(status, Some(completed_at)),
            },
        );
        self.finished = true;
    }

    pub fn last_model_call_id(&self) -> Option<Uuid> {
        self.last_model_call_id
    }

    pub fn take_legacy_delta(&mut self) -> TokenConsumption {
        std::mem::take(&mut self.legacy_delta)
    }

    fn start_turn(&mut self, event: &Value) {
        let index = self.next_turn_index;
        self.next_turn_index = self.next_turn_index.saturating_add(1);
        let id = stable_uuid(&format!("execution:{}:turn:{index}", self.execution_id));
        let model_call_id = stable_uuid(&format!("turn:{id}:model_call:0"));
        let started_at = now_rfc3339();
        let context_window_limit = event
            .get("model_context_window")
            .and_then(Value::as_u64);
        let context_tokens_before_call = event
            .get("context_tokens")
            .or_else(|| event.get("context_tokens_before_call"))
            .and_then(Value::as_u64);
        self.active_turn = Some(ActiveTurn {
            id,
            model_call_id,
            index,
            started_at: started_at.clone(),
            context_tokens_before_call,
            context_window_limit,
        });
        self.last_model_call_id = Some(model_call_id);
        self.emit(
            "turn.started",
            1,
            id,
            &started_at,
            TelemetryPayload::Turn {
                turn: TurnSnapshot {
                    id,
                    execution_id: self.execution_id,
                    phase_id: Some(self.phase_id),
                    turn_index: index,
                    started_at: Some(started_at.clone()),
                    completed_at: None,
                    status: ExecutionStatus::Running,
                },
            },
        );
        self.emit(
            "model_call.started",
            1,
            model_call_id,
            &started_at,
            TelemetryPayload::ModelCall {
                model_call: self.model_snapshot(
                    model_call_id,
                    index,
                    &started_at,
                    None,
                    ModelCallStatus::InProgress,
                    None,
                    context_tokens_before_call,
                    context_window_limit,
                    None,
                    None,
                ),
            },
        );
    }

    fn finish_turn(
        &mut self,
        outcome: SessionOutcome,
        usage: Option<NormalizedUsage>,
        terminal: Option<&Map<String, Value>>,
    ) {
        let Some(turn) = self.active_turn.take() else {
            return;
        };
        let completed_at = now_rfc3339();
        let tool_outcome = match outcome {
            SessionOutcome::Succeeded => SessionOutcome::Failed,
            value => value,
        };
        self.finish_open_tools(tool_outcome, &completed_at);
        let usage = usage.unwrap_or_else(|| NormalizedUsage {
            source: Some(UsageSource::Unavailable),
            ..NormalizedUsage::default()
        });
        if let (Some(input), Some(cached), Some(output)) = (
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
        ) && let Err(error) = self
            .legacy_delta
            .add_counts(input, cached, output)
        {
            eprintln!("[warning] legacy token aggregate overflowed: {error:#}");
        }
        let finish_reason = terminal
            .and_then(|value| value.get("finish_reason"))
            .and_then(Value::as_str)
            .and_then(|value| safe_identifier(value, 80));
        let provider_request_id = terminal
            .and_then(|value| {
                value
                    .get("provider_request_id")
                    .or_else(|| value.get("response_id"))
            })
            .and_then(Value::as_str)
            .and_then(|value| safe_identifier(value, 160));
        let model_status = model_status(outcome);
        let model_event_type = if matches!(outcome, SessionOutcome::Succeeded) {
            "model_call.completed"
        } else {
            "model_call.failed"
        };
        self.emit(
            model_event_type,
            2,
            turn.model_call_id,
            &completed_at,
            TelemetryPayload::ModelCall {
                model_call: self.model_snapshot(
                    turn.model_call_id,
                    turn.index,
                    &turn.started_at,
                    Some(completed_at.clone()),
                    model_status,
                    Some(usage),
                    turn.context_tokens_before_call,
                    turn.context_window_limit,
                    finish_reason,
                    provider_request_id,
                ),
            },
        );
        self.emit(
            "turn.completed",
            2,
            turn.id,
            &completed_at,
            TelemetryPayload::Turn {
                turn: TurnSnapshot {
                    id: turn.id,
                    execution_id: self.execution_id,
                    phase_id: Some(self.phase_id),
                    turn_index: turn.index,
                    started_at: Some(turn.started_at),
                    completed_at: Some(completed_at),
                    status: execution_status(outcome),
                },
            },
        );
    }

    fn start_tool(&mut self, item: &Value) {
        let Some((source_id, name, category)) = tool_identity(item) else {
            return;
        };
        if self.completed_tools.contains(&source_id) || self.active_tools.contains_key(&source_id) {
            return;
        }
        if self.active_turn.is_none() {
            self.start_turn(&Value::Null);
        }
        let turn = self.active_turn.as_ref().expect("turn created above");
        let id = stable_uuid(&format!(
            "execution:{}:tool:{}",
            self.execution_id, source_id
        ));
        let started_at = now_rfc3339();
        let tool = ActiveTool {
            id,
            source_id: source_id.clone(),
            name,
            category,
            turn_index: turn.index,
            started_at: started_at.clone(),
            request_bytes: tool_request_bytes(item),
            related_model_call_id: Some(turn.model_call_id),
        };
        self.emit(
            "tool_call.started",
            1,
            id,
            &started_at,
            TelemetryPayload::ToolCall {
                tool_call: self.tool_snapshot(&tool, None, ToolCallStatus::InProgress, None, None),
            },
        );
        self.active_tools.insert(source_id, tool);
    }

    fn complete_tool(&mut self, item: &Value) {
        let Some((source_id, name, category)) = tool_identity(item) else {
            return;
        };
        if !self.active_tools.contains_key(&source_id) {
            if self.completed_tools.contains(&source_id) {
                return;
            }
            self.start_tool(item);
        }
        let Some(mut tool) = self.active_tools.remove(&source_id) else {
            return;
        };
        tool.name = name;
        tool.category = category;
        let completed_at = now_rfc3339();
        let status = tool_terminal_status(item);
        let error_code = tool_error_code(item);
        let response_bytes = tool_response_bytes(item);
        self.emit(
            "tool_call.completed",
            2,
            tool.id,
            &completed_at,
            TelemetryPayload::ToolCall {
                tool_call: self.tool_snapshot(
                    &tool,
                    Some(completed_at),
                    status,
                    error_code,
                    response_bytes,
                ),
            },
        );
        self.completed_tools.insert(tool.source_id);
    }

    fn finish_open_tools(&mut self, outcome: SessionOutcome, completed_at: &str) {
        let tools = self.active_tools.drain().map(|(_, tool)| tool).collect::<Vec<_>>();
        for tool in tools {
            self.emit(
                "tool_call.completed",
                2,
                tool.id,
                completed_at,
                TelemetryPayload::ToolCall {
                    tool_call: self.tool_snapshot(
                        &tool,
                        Some(completed_at.to_owned()),
                        tool_status(outcome),
                        None,
                        None,
                    ),
                },
            );
            self.completed_tools.insert(tool.source_id);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn model_snapshot(
        &self,
        id: Uuid,
        turn_index: u32,
        started_at: &str,
        completed_at: Option<String>,
        status: ModelCallStatus,
        usage: Option<NormalizedUsage>,
        context_tokens_before_call: Option<u64>,
        context_window_limit: Option<u64>,
        finish_reason: Option<String>,
        provider_request_id: Option<String>,
    ) -> ModelCallSnapshot {
        let usage = usage.unwrap_or_default();
        ModelCallSnapshot {
            id,
            execution_id: self.execution_id,
            agent_id: self.agent_id,
            agent_name: Some("Codex".into()),
            phase_id: Some(self.phase_id),
            phase_name: Some(self.invocation.phase_name.clone()),
            turn_index,
            call_index: 0,
            provider: self.invocation.provider.clone(),
            model: self.invocation.model.clone(),
            started_at: started_at.to_owned(),
            completed_at,
            status,
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            total_tokens: usage.total_tokens,
            context_tokens_before_call,
            context_window_limit,
            finish_reason,
            retry_of_call_id: self.invocation.retry_of_call_id,
            attempt_number: self.invocation.attempt_number.max(1),
            provider_request_id,
            provider_usage_payload: usage.raw,
            usage_source: usage.source.unwrap_or(UsageSource::Unavailable),
            capture_granularity: CaptureGranularity::TurnAggregate,
        }
    }

    fn tool_snapshot(
        &self,
        tool: &ActiveTool,
        completed_at: Option<String>,
        status: ToolCallStatus,
        error_code: Option<String>,
        response_bytes: Option<u64>,
    ) -> ToolCallSnapshot {
        ToolCallSnapshot {
            id: tool.id,
            execution_id: self.execution_id,
            agent_id: self.agent_id,
            agent_name: Some("Codex".into()),
            phase_id: Some(self.phase_id),
            phase_name: Some(self.invocation.phase_name.clone()),
            turn_index: tool.turn_index,
            tool_name: tool.name.clone(),
            tool_category: Some(tool.category.clone()),
            started_at: tool.started_at.clone(),
            completed_at,
            status,
            error_code,
            request_bytes: tool.request_bytes,
            response_bytes,
            related_model_call_id: tool.related_model_call_id,
        }
    }

    fn execution_snapshot(
        &self,
        status: ExecutionStatus,
        completed_at: Option<String>,
    ) -> ExecutionSnapshot {
        ExecutionSnapshot {
            id: self.execution_id,
            agent_id: self.agent_id,
            agent_name: Some("Codex".into()),
            role: Some(invocation_role(&self.invocation.phase_name).into()),
            started_at: self.started_at.clone(),
            completed_at,
            status,
        }
    }

    fn phase_snapshot(
        &self,
        status: ExecutionStatus,
        completed_at: Option<String>,
    ) -> PhaseSnapshot {
        PhaseSnapshot {
            id: self.phase_id,
            execution_id: self.execution_id,
            name: self.invocation.phase_name.clone(),
            started_at: self.started_at.clone(),
            completed_at,
            status,
        }
    }

    fn emit(
        &self,
        event_type: &str,
        entity_revision: u32,
        entity_id: Uuid,
        occurred_at: &str,
        payload: TelemetryPayload,
    ) {
        let Some(emitter) = &self.emitter else {
            return;
        };
        let event_id = stable_uuid(&format!(
            "entity:{entity_id}:event:{event_type}:revision:{entity_revision}"
        ));
        emitter.emit(
            &self.invocation.run_id,
            TelemetryEvent {
                event_id,
                entity_revision,
                occurred_at: occurred_at.to_owned(),
                event_type: event_type.to_owned(),
                payload,
            },
        );
    }
}

fn execution_status(outcome: SessionOutcome) -> ExecutionStatus {
    match outcome {
        SessionOutcome::Succeeded => ExecutionStatus::Succeeded,
        SessionOutcome::Cancelled => ExecutionStatus::Cancelled,
        SessionOutcome::Failed | SessionOutcome::Timeout => ExecutionStatus::Failed,
    }
}

fn model_status(outcome: SessionOutcome) -> ModelCallStatus {
    match outcome {
        SessionOutcome::Succeeded => ModelCallStatus::Success,
        SessionOutcome::Failed => ModelCallStatus::Error,
        SessionOutcome::Cancelled => ModelCallStatus::Cancelled,
        SessionOutcome::Timeout => ModelCallStatus::Timeout,
    }
}

fn tool_status(outcome: SessionOutcome) -> ToolCallStatus {
    match outcome {
        SessionOutcome::Succeeded => ToolCallStatus::Success,
        SessionOutcome::Failed => ToolCallStatus::Error,
        SessionOutcome::Cancelled => ToolCallStatus::Cancelled,
        SessionOutcome::Timeout => ToolCallStatus::Timeout,
    }
}

fn invocation_role(phase_name: &str) -> &'static str {
    if phase_name.starts_with("ci_repair") {
        "ci_repair"
    } else if phase_name.starts_with("validation_repair") {
        "validation_repair"
    } else {
        "implementation"
    }
}

fn tool_identity(item: &Value) -> Option<(String, String, String)> {
    let item_type = item.get("type")?.as_str()?;
    let source_id = item.get("id")?.as_str()?.to_owned();
    match item_type {
        "command_execution" => Some((source_id, "shell".into(), "command".into())),
        "mcp_tool_call" => {
            let name = item
                .get("tool")
                .and_then(Value::as_str)
                .and_then(|value| safe_identifier(value, 128))
                .unwrap_or_else(|| "mcp_tool".into());
            Some((source_id, name, "mcp".into()))
        }
        "web_search" => Some((source_id, "web_search".into(), "search".into())),
        "file_change" => Some((source_id, "file_change".into(), "filesystem".into())),
        "image_generation" => Some((source_id, "image_generation".into(), "media".into())),
        "collab_tool_call" => Some((source_id, "agent_control".into(), "collaboration".into())),
        _ => None,
    }
}

fn tool_request_bytes(item: &Value) -> Option<u64> {
    ["command", "arguments", "query", "changes"]
        .iter()
        .find_map(|field| item.get(*field))
        .and_then(serialized_bytes)
}

fn tool_response_bytes(item: &Value) -> Option<u64> {
    ["aggregated_output", "result", "output"]
        .iter()
        .find_map(|field| item.get(*field))
        .and_then(serialized_bytes)
}

fn serialized_bytes(value: &Value) -> Option<u64> {
    serde_json::to_vec(value)
        .ok()
        .and_then(|bytes| u64::try_from(bytes.len()).ok())
}

fn tool_terminal_status(item: &Value) -> ToolCallStatus {
    match item.get("status").and_then(Value::as_str) {
        Some("failed" | "error") => ToolCallStatus::Error,
        Some("cancelled" | "canceled") => ToolCallStatus::Cancelled,
        Some("timeout" | "timed_out") => ToolCallStatus::Timeout,
        _ if item.get("error").is_some_and(|error| !error.is_null()) => ToolCallStatus::Error,
        _ => ToolCallStatus::Success,
    }
}

fn tool_error_code(item: &Value) -> Option<String> {
    item.get("error")
        .and_then(|error| error.get("code"))
        .and_then(|code| match code {
            Value::String(value) => safe_identifier(value, 80),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
}

fn safe_identifier(value: &str, max: usize) -> Option<String> {
    (!value.is_empty()
        && value.len() <= max
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':' | '/')
        }))
    .then(|| value.to_owned())
}

pub fn codex_provider_and_model(args: &[String]) -> (String, String) {
    let mut provider = "openai".to_owned();
    let mut model = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--model" | "-m" if index + 1 < args.len() => {
                model = Some(args[index + 1].clone());
                index += 1;
            }
            "--oss" => provider = "oss".into(),
            "--local-provider" if index + 1 < args.len() => {
                provider = args[index + 1].clone();
                index += 1;
            }
            value if value.starts_with("--model=") => {
                model = value.split_once('=').map(|(_, value)| value.to_owned());
            }
            _ => {}
        }
        index += 1;
    }
    (provider, model.unwrap_or_else(|| "unknown".into()))
}

fn stable_uuid(value: &str) -> Uuid {
    Uuid::new_v5(&TELEMETRY_NAMESPACE, value.as_bytes())
}

fn now_rfc3339() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_unix_millis(elapsed.as_secs(), elapsed.subsec_millis())
}

fn format_unix_millis(seconds: u64, millis: u32) -> String {
    let days = seconds / 86_400;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(i64::MAX));
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096)
            / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}
