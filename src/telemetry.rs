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

use crate::{api::RustGridClient, mission::BudgetUsage, token_consumption::TokenConsumption};

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
    ModelCall { model_call: Box<ModelCallSnapshot> },
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
    NeedsContinuation,
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
    pub uncached_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens_before_call: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_context_tokens_so_far: Option<u64>,
    pub cumulative_input_tokens: u64,
    pub cumulative_cached_input_tokens: u64,
    pub cumulative_uncached_input_tokens: u64,
    pub cumulative_output_tokens: u64,
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
pub struct AgentInvocation {
    pub run_id: String,
    pub agent_name: String,
    pub execution_sequence: u64,
    pub phase_name: String,
    pub provider: String,
    pub model: String,
    pub attempt_number: u32,
    pub retry_of_call_id: Option<Uuid>,
    pub context_estimates: Map<String, Value>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct NormalizedUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    total_tokens: Option<u64>,
    source: Option<UsageSource>,
    raw: Option<Map<String, Value>>,
}

impl NormalizedUsage {
    /// What this aggregate holds beyond the calls already reported, or `None`
    /// when they cover all of it. Providers aggregate a turn's calls, so a
    /// remainder is usage no single call reported.
    fn remainder_after(&self, reported: TokenConsumption) -> Option<Self> {
        let input = self.input_tokens?.saturating_sub(reported.input_tokens);
        let output = self
            .output_tokens
            .unwrap_or(0)
            .saturating_sub(reported.output_tokens);
        let cached = self
            .cached_input_tokens
            .unwrap_or(0)
            .saturating_sub(reported.cached_input_tokens)
            .min(input);
        if input == 0 && output == 0 {
            return None;
        }
        let mut raw = Map::new();
        raw.insert("turn_aggregate_remainder".into(), Value::Bool(true));
        Some(Self {
            input_tokens: Some(input),
            cached_input_tokens: Some(cached),
            output_tokens: Some(output),
            reasoning_tokens: None,
            total_tokens: input.checked_add(output),
            source: Some(UsageSource::WorkerCalculated),
            raw: Some(raw),
        })
    }
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
        if input_tokens
            .zip(cached_input_tokens)
            .is_some_and(|(input, cached)| cached > input)
        {
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

/// Usage fields that are short identifiers rather than counts. RustGrid prices
/// by them (standard processing only), so they travel with the counts.
const PRICING_IDENTIFIERS: [&str; 3] = ["service_tier", "inference_geo", "speed"];

fn safe_usage_payload(usage: &Map<String, Value>) -> Option<Map<String, Value>> {
    let safe = usage
        .iter()
        .filter(|(key, _)| {
            key.len() <= 80
                && key
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
        .filter_map(|(key, value)| match value {
            Value::Null | Value::Bool(_) | Value::Number(_) => Some((key.clone(), value.clone())),
            Value::String(text) if PRICING_IDENTIFIERS.contains(&key.as_str()) => {
                safe_identifier(text, 40).map(|text| (key.clone(), Value::String(text)))
            }
            _ => None,
        })
        .collect::<Map<_, _>>();
    (!safe.is_empty()
        && serde_json::to_vec(&safe).is_ok_and(|bytes| bytes.len() <= MAX_RAW_USAGE_BYTES))
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
        event: Box<TelemetryEvent>,
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
            event: Box::new(event),
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
        if self.sender.try_send(EmitterMessage::Flush(sender)).is_err() {
            eprintln!(
                "[warning] telemetry flush could not be queued; delivery remains best-effort"
            );
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
                events.push(*event);
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

fn deliver_batch(api: &RustGridClient, outbox_root: &Path, run_id: &str, batch: TelemetryBatch) {
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
    index: u32,
    started_at: String,
    context_tokens_before_call: Option<u64>,
    context_window_limit: Option<u64>,
    narration_characters: u64,
    /// The model call started but not yet completed, if any.
    open_call: Option<OpenCall>,
    next_call_index: u32,
    /// Usage of the calls the agent reported one by one during this turn.
    /// The turn's aggregate only adds what these did not already cover.
    reported_calls: u32,
    reported_usage: TokenConsumption,
}

struct OpenCall {
    id: Uuid,
    index: u32,
    started_at: String,
}

/// Everything about one model call that varies between its snapshots.
struct CallRecord {
    id: Uuid,
    turn_index: u32,
    call_index: u32,
    started_at: String,
    completed_at: Option<String>,
    status: ModelCallStatus,
    model: Option<String>,
    usage: Option<NormalizedUsage>,
    context_tokens_before_call: Option<u64>,
    context_window_limit: Option<u64>,
    finish_reason: Option<String>,
    provider_request_id: Option<String>,
    granularity: CaptureGranularity,
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

pub struct AgentTelemetrySession {
    invocation: AgentInvocation,
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
    peak_context_tokens: u64,
    finished: bool,
}

impl AgentTelemetrySession {
    pub fn start(invocation: AgentInvocation, emitter: Option<TelemetryEmitter>) -> Self {
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
            peak_context_tokens: 0,
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
            // A call reported after its turn ended is already in that turn's
            // aggregate, so only calls inside a turn are counted.
            Some("model_call.completed") if self.active_turn.is_some() => {
                self.complete_call(&event);
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
                    if item.get("type").and_then(Value::as_str) == Some("agent_message")
                        && let Some(text) = item.get("text").and_then(Value::as_str)
                        && let Some(turn) = self.active_turn.as_mut()
                    {
                        turn.narration_characters = turn
                            .narration_characters
                            .saturating_add(text.chars().count().try_into().unwrap_or(u64::MAX));
                    }
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
        } else if self.next_turn_index == 0 {
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
                execution: self.execution_snapshot(status, Some(completed_at.clone())),
            },
        );
        self.finished = true;
    }

    /// The agent's own session id, such as the Codex thread id.
    pub fn agent_session_id(&self) -> Option<Uuid> {
        self.agent_id
    }

    pub fn last_model_call_id(&self) -> Option<Uuid> {
        self.last_model_call_id
    }

    pub fn take_legacy_delta(&mut self) -> TokenConsumption {
        std::mem::take(&mut self.legacy_delta)
    }

    pub fn model_call_count(&self) -> u32 {
        self.next_turn_index
    }

    pub fn tool_call_count(&self) -> usize {
        self.completed_tools.len() + self.active_tools.len()
    }

    pub fn budget_usage(&self, initial_prompt_tokens: u64, duration_ms: u64) -> BudgetUsage {
        BudgetUsage {
            initial_prompt_tokens,
            inference_turns: self.model_call_count(),
            tool_calls: self.tool_call_count().try_into().unwrap_or(u32::MAX),
            peak_context_tokens: self.peak_context_tokens,
            cumulative_uncached_input_tokens: self
                .legacy_delta
                .input_tokens
                .saturating_sub(self.legacy_delta.cached_input_tokens),
            cumulative_cached_input_tokens: self.legacy_delta.cached_input_tokens,
            output_tokens: self.legacy_delta.output_tokens,
            codex_duration_ms: duration_ms,
        }
    }

    fn start_turn(&mut self, event: &Value) {
        let index = self.next_turn_index;
        self.next_turn_index = self.next_turn_index.saturating_add(1);
        let id = stable_uuid(&format!("execution:{}:turn:{index}", self.execution_id));
        let started_at = now_rfc3339();
        let context_window_limit = event.get("model_context_window").and_then(Value::as_u64);
        let context_tokens_before_call = event
            .get("context_tokens")
            .or_else(|| event.get("context_tokens_before_call"))
            .and_then(Value::as_u64);
        self.peak_context_tokens = self
            .peak_context_tokens
            .max(context_tokens_before_call.unwrap_or(0));
        self.active_turn = Some(ActiveTurn {
            id,
            index,
            started_at: started_at.clone(),
            context_tokens_before_call,
            context_window_limit,
            narration_characters: 0,
            open_call: None,
            next_call_index: 0,
            reported_calls: 0,
            reported_usage: TokenConsumption::default(),
        });
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
        self.open_call();
    }

    /// Starts the turn's next model call. Its id for call 0 is the id a turn's
    /// single aggregate call always had, so earlier reports stay idempotent.
    fn open_call(&mut self) {
        self.open_call_at(None);
    }

    fn open_call_at(&mut self, started_at: Option<String>) {
        let Some(turn) = self.active_turn.as_mut() else {
            return;
        };
        if turn.open_call.is_some() {
            return;
        }
        let index = turn.next_call_index;
        turn.next_call_index = turn.next_call_index.saturating_add(1);
        let id = stable_uuid(&format!("turn:{}:model_call:{index}", turn.id));
        let started_at = started_at.unwrap_or_else(now_rfc3339);
        turn.open_call = Some(OpenCall {
            id,
            index,
            started_at: started_at.clone(),
        });
        let record = CallRecord {
            id,
            turn_index: turn.index,
            call_index: index,
            started_at: started_at.clone(),
            completed_at: None,
            status: ModelCallStatus::InProgress,
            model: None,
            usage: None,
            context_tokens_before_call: turn.context_tokens_before_call,
            context_window_limit: turn.context_window_limit,
            finish_reason: None,
            provider_request_id: None,
            granularity: CaptureGranularity::TurnAggregate,
        };
        self.last_model_call_id = Some(id);
        self.emit(
            "model_call.started",
            1,
            id,
            &started_at,
            TelemetryPayload::ModelCall {
                model_call: Box::new(self.model_snapshot(record)),
            },
        );
    }

    /// Reports one provider API call as it completes, with the session's
    /// running totals, so consumption is visible over the session. A call read
    /// back from a record carries the times the provider recorded.
    fn complete_call(&mut self, event: &Value) {
        let recorded_at = |field: &str| {
            event
                .get(field)
                .and_then(Value::as_str)
                .and_then(safe_timestamp)
        };
        let started_at = recorded_at("started_at");
        let completed_at = recorded_at("completed_at").unwrap_or_else(now_rfc3339);
        let usage = CodexUsageAdapter.normalize_usage(event);
        let (Some(input), Some(output)) = (usage.input_tokens, usage.output_tokens) else {
            return;
        };
        let cached = usage.cached_input_tokens.unwrap_or(0);
        if let Err(error) = self.legacy_delta.add_counts(input, cached, output) {
            eprintln!("[warning] legacy token aggregate overflowed: {error:#}");
            return;
        }
        self.open_call_at(started_at);
        let Some(turn) = self.active_turn.as_mut() else {
            return;
        };
        let _ = turn.reported_usage.add_counts(input, cached, output);
        turn.reported_calls = turn.reported_calls.saturating_add(1);
        let call = turn.open_call.take().expect("call opened above");
        let record = CallRecord {
            id: call.id,
            turn_index: turn.index,
            call_index: call.index,
            started_at: call.started_at,
            completed_at: Some(completed_at.clone()),
            status: ModelCallStatus::Success,
            model: event
                .get("model")
                .and_then(Value::as_str)
                .and_then(|value| safe_identifier(value, 120)),
            usage: Some(usage),
            context_tokens_before_call: Some(input),
            context_window_limit: turn.context_window_limit,
            finish_reason: event
                .get("finish_reason")
                .and_then(Value::as_str)
                .and_then(|value| safe_identifier(value, 80)),
            provider_request_id: event
                .get("provider_request_id")
                .and_then(Value::as_str)
                .and_then(|value| safe_identifier(value, 160)),
            granularity: CaptureGranularity::ModelCall,
        };
        self.emit(
            "model_call.completed",
            2,
            record.id,
            &completed_at,
            TelemetryPayload::ModelCall {
                model_call: Box::new(self.model_snapshot(record)),
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
        let mut usage = usage.unwrap_or_else(|| NormalizedUsage {
            source: Some(UsageSource::Unavailable),
            ..NormalizedUsage::default()
        });
        if turn.reported_calls > 0 {
            // The calls were already reported one by one. Only usage the
            // aggregate holds beyond them is left to report.
            match usage.remainder_after(turn.reported_usage) {
                Some(remainder) => usage = remainder,
                None if matches!(outcome, SessionOutcome::Succeeded) => {
                    self.close_turn(turn, outcome, &completed_at);
                    return;
                }
                None => {
                    usage = NormalizedUsage {
                        source: Some(UsageSource::Unavailable),
                        ..NormalizedUsage::default()
                    };
                }
            }
        }
        let raw = usage.raw.get_or_insert_with(Map::new);
        raw.extend(self.invocation.context_estimates.clone());
        raw.insert(
            "model_narration_characters".into(),
            Value::from(turn.narration_characters),
        );
        if let (Some(input), Some(cached), Some(output)) = (
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
        ) && let Err(error) = self.legacy_delta.add_counts(input, cached, output)
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
        let model_event_type = if matches!(outcome, SessionOutcome::Succeeded) {
            "model_call.completed"
        } else {
            "model_call.failed"
        };
        self.active_turn = Some(turn);
        self.open_call();
        let mut turn = self.active_turn.take().expect("turn restored above");
        let call = turn.open_call.take().expect("call opened above");
        let record = CallRecord {
            id: call.id,
            turn_index: turn.index,
            call_index: call.index,
            started_at: call.started_at,
            completed_at: Some(completed_at.clone()),
            status: model_status(outcome),
            model: None,
            usage: Some(usage),
            context_tokens_before_call: turn.context_tokens_before_call,
            context_window_limit: turn.context_window_limit,
            finish_reason,
            provider_request_id,
            granularity: CaptureGranularity::TurnAggregate,
        };
        self.emit(
            model_event_type,
            2,
            record.id,
            &completed_at,
            TelemetryPayload::ModelCall {
                model_call: Box::new(self.model_snapshot(record)),
            },
        );
        self.close_turn(turn, outcome, &completed_at);
    }

    fn close_turn(&mut self, turn: ActiveTurn, outcome: SessionOutcome, completed_at: &str) {
        self.emit(
            "turn.completed",
            2,
            turn.id,
            completed_at,
            TelemetryPayload::Turn {
                turn: TurnSnapshot {
                    id: turn.id,
                    execution_id: self.execution_id,
                    phase_id: Some(self.phase_id),
                    turn_index: turn.index,
                    started_at: Some(turn.started_at),
                    completed_at: Some(completed_at.to_owned()),
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
            related_model_call_id: self.last_model_call_id,
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
                    Some(completed_at.clone()),
                    status,
                    error_code,
                    response_bytes,
                ),
            },
        );
        self.completed_tools.insert(tool.source_id);
    }

    fn finish_open_tools(&mut self, outcome: SessionOutcome, completed_at: &str) {
        let tools = self
            .active_tools
            .drain()
            .map(|(_, tool)| tool)
            .collect::<Vec<_>>();
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

    fn model_snapshot(&self, record: CallRecord) -> ModelCallSnapshot {
        let CallRecord {
            id,
            turn_index,
            call_index,
            started_at,
            completed_at,
            status,
            model,
            usage,
            context_tokens_before_call,
            context_window_limit,
            finish_reason,
            provider_request_id,
            granularity,
        } = record;
        let usage = usage.unwrap_or_default();
        ModelCallSnapshot {
            id,
            execution_id: self.execution_id,
            agent_id: self.agent_id,
            agent_name: Some(self.invocation.agent_name.clone()),
            phase_id: Some(self.phase_id),
            phase_name: Some(self.invocation.phase_name.clone()),
            turn_index,
            call_index,
            provider: self.invocation.provider.clone(),
            model: model.unwrap_or_else(|| self.invocation.model.clone()),
            started_at,
            completed_at,
            status,
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            uncached_input_tokens: usage
                .input_tokens
                .zip(usage.cached_input_tokens)
                .map(|(input, cached)| input.saturating_sub(cached)),
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            total_tokens: usage.total_tokens,
            context_tokens_before_call,
            peak_context_tokens_so_far: (self.peak_context_tokens > 0)
                .then_some(self.peak_context_tokens),
            cumulative_input_tokens: self.legacy_delta.input_tokens,
            cumulative_cached_input_tokens: self.legacy_delta.cached_input_tokens,
            cumulative_uncached_input_tokens: self
                .legacy_delta
                .input_tokens
                .saturating_sub(self.legacy_delta.cached_input_tokens),
            cumulative_output_tokens: self.legacy_delta.output_tokens,
            context_window_limit,
            finish_reason,
            retry_of_call_id: self.invocation.retry_of_call_id,
            attempt_number: self.invocation.attempt_number.max(1),
            provider_request_id,
            provider_usage_payload: usage.raw,
            usage_source: usage.source.unwrap_or(UsageSource::Unavailable),
            capture_granularity: granularity,
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
            agent_name: Some(self.invocation.agent_name.clone()),
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
            agent_name: Some(self.invocation.agent_name.clone()),
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

/// An RFC 3339 UTC timestamp as providers record them, such as
/// `2026-11-08T21:18:55.758Z`.
fn safe_timestamp(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let shaped = (20..=35).contains(&bytes.len())
        && bytes[4] == b'-'
        && bytes[10] == b'T'
        && value.ends_with('Z')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || b"-:.TZ".contains(byte));
    shaped.then(|| value.to_owned())
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
            "-c" | "--config" if index + 1 < args.len() => {
                apply_codex_config_identity(&args[index + 1], &mut provider, &mut model);
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

fn apply_codex_config_identity(value: &str, provider: &mut String, model: &mut Option<String>) {
    let Some((key, value)) = value.split_once('=') else {
        return;
    };
    let value = value.trim().trim_matches('"');
    match key.trim() {
        "model" if safe_identifier(value, 200).is_some() => *model = Some(value.to_owned()),
        // The gateway provider is a route to OpenAI, not a model vendor.
        "model_provider" if value == crate::manifest::CODEX_GATEWAY_PROVIDER => {
            *provider = crate::agent::AgentProvider::Codex.model_vendor().to_owned();
        }
        "model_provider" if safe_identifier(value, 100).is_some() => *provider = value.to_owned(),
        _ => {}
    }
}

pub(crate) fn stable_uuid(value: &str) -> Uuid {
    Uuid::new_v5(&TELEMETRY_NAMESPACE, value.as_bytes())
}

pub(crate) fn now_rfc3339() -> String {
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
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_gateway_codex_sessions_as_openai() {
        let args = [
            "codex",
            "exec",
            "--model",
            "gpt-5.6-sol",
            "-c",
            "model_provider=\"rustgrid_gateway\"",
        ]
        .map(String::from);
        assert_eq!(
            codex_provider_and_model(&args),
            ("openai".into(), "gpt-5.6-sol".into())
        );
    }

    #[test]
    fn resolves_model_identity_from_codex_flags_and_config_overrides() {
        let args = vec![
            "codex".into(),
            "exec".into(),
            "--model".into(),
            "gpt-5.6-terra".into(),
            "-c".into(),
            "model_provider=\"rustgrid_chatgpt_http\"".into(),
            "-".into(),
        ];
        assert_eq!(
            codex_provider_and_model(&args),
            ("rustgrid_chatgpt_http".into(), "gpt-5.6-terra".into())
        );

        let config_only = vec![
            "codex".into(),
            "exec".into(),
            "--config".into(),
            "model=\"gpt-5.6-sol\"".into(),
        ];
        assert_eq!(
            codex_provider_and_model(&config_only),
            ("openai".into(), "gpt-5.6-sol".into())
        );
    }

    #[test]
    fn codex_session_emits_detailed_usage_and_preserves_legacy_delta() {
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let emitter = TelemetryEmitter {
            sender,
            dropped_events: Arc::new(AtomicU64::new(0)),
        };
        let mut session = AgentTelemetrySession::start(
            AgentInvocation {
                agent_name: "Codex".into(),
                run_id: "run-1".into(),
                execution_sequence: 7,
                phase_name: "validation_repair_2_codex".into(),
                provider: "openai".into(),
                model: "gpt-5.6-codex".into(),
                attempt_number: 2,
                retry_of_call_id: None,
                context_estimates: Map::from_iter([
                    ("ctx_known_est_tokens".into(), Value::from(321)),
                    ("ctx_ambient_config_loaded".into(), Value::Bool(false)),
                ]),
            },
            Some(emitter),
        );
        session.observe_line(r#"{"type":"turn.started"}"#);
        session.observe_line(
            r#"{"type":"item.completed","item":{"id":"message-1","type":"agent_message","text":"Implementation complete."}}"#,
        );
        session.observe_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":25,"reasoning_output_tokens":5,"total_tokens":125}}"#,
        );
        session.finish(SessionOutcome::Succeeded);

        let events = receiver
            .try_iter()
            .filter_map(|message| match message {
                EmitterMessage::Event { event, .. } => Some(*event),
                EmitterMessage::Flush(_) => None,
            })
            .collect::<Vec<_>>();
        let completed = events
            .iter()
            .find(|event| event.event_type == "model_call.completed")
            .expect("completed model call telemetry");
        let TelemetryPayload::ModelCall { model_call } = &completed.payload else {
            panic!("model_call.completed must contain a model call payload");
        };
        assert_eq!(model_call.input_tokens, Some(100));
        assert_eq!(model_call.cached_input_tokens, Some(40));
        assert_eq!(model_call.output_tokens, Some(25));
        assert_eq!(model_call.reasoning_tokens, Some(5));
        assert_eq!(model_call.total_tokens, Some(125));
        assert_eq!(
            model_call
                .provider_usage_payload
                .as_ref()
                .and_then(|payload| payload.get("ctx_known_est_tokens"))
                .and_then(Value::as_u64),
            Some(321)
        );
        assert_eq!(
            model_call
                .provider_usage_payload
                .as_ref()
                .and_then(|payload| payload.get("model_narration_characters"))
                .and_then(Value::as_u64),
            Some(24)
        );
        assert_eq!(model_call.attempt_number, 2);
        assert!(matches!(
            model_call.usage_source,
            UsageSource::ProviderReported
        ));
        assert_eq!(
            session.take_legacy_delta(),
            TokenConsumption {
                input_tokens: 100,
                cached_input_tokens: 40,
                output_tokens: 25,
            }
        );
    }

    fn recording_session(run_id: &str) -> (AgentTelemetrySession, mpsc::Receiver<EmitterMessage>) {
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let emitter = TelemetryEmitter {
            sender,
            dropped_events: Arc::new(AtomicU64::new(0)),
        };
        let session = AgentTelemetrySession::start(
            AgentInvocation {
                agent_name: "Claude Code".into(),
                run_id: run_id.into(),
                execution_sequence: 1,
                phase_name: "implementation".into(),
                provider: "anthropic".into(),
                model: "claude-opus-5-5".into(),
                attempt_number: 1,
                retry_of_call_id: None,
                context_estimates: Map::new(),
            },
            Some(emitter),
        );
        (session, receiver)
    }

    fn completed_calls(receiver: &mpsc::Receiver<EmitterMessage>) -> Vec<ModelCallSnapshot> {
        receiver
            .try_iter()
            .filter_map(|message| match message {
                EmitterMessage::Event { event, .. } => Some(*event),
                EmitterMessage::Flush(_) => None,
            })
            .filter(|event| event.event_type == "model_call.completed")
            .map(|event| match event.payload {
                TelemetryPayload::ModelCall { model_call } => *model_call,
                _ => panic!("model_call.completed must contain a model call payload"),
            })
            .collect()
    }

    const CALL_A: &str = r#"{"type":"model_call.completed","id":"msg_a","model":"claude-opus-5-5","finish_reason":"tool_use","usage":{"input_tokens":30000,"cached_input_tokens":29000,"output_tokens":200,"total_tokens":30200,"service_tier":"standard"}}"#;
    const CALL_B: &str = r#"{"type":"model_call.completed","id":"msg_b","model":"claude-haiku-5-5","usage":{"input_tokens":31000,"cached_input_tokens":30000,"output_tokens":50,"total_tokens":31050}}"#;

    #[test]
    fn each_model_call_is_reported_as_it_completes_with_running_totals() {
        let (mut session, receiver) = recording_session("run-calls");
        session.observe_line(r#"{"type":"turn.started"}"#);
        session.observe_line(CALL_A);
        let first = completed_calls(&receiver);
        assert_eq!(
            first.len(),
            1,
            "the call is reported before the session ends"
        );
        assert_eq!(session.budget_usage(0, 0).output_tokens, 200);

        session.observe_line(CALL_B);
        session.observe_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":61000,"cached_input_tokens":59000,"output_tokens":250,"total_tokens":61250}}"#,
        );
        session.finish(SessionOutcome::Succeeded);
        let calls = first
            .into_iter()
            .chain(completed_calls(&receiver))
            .collect::<Vec<_>>();

        assert_eq!(calls.len(), 2, "a fully covered aggregate adds no call");
        assert_eq!(
            calls.iter().map(|call| call.call_index).collect::<Vec<_>>(),
            [0, 1]
        );
        assert_ne!(calls[0].id, calls[1].id);
        assert_eq!(calls[0].model, "claude-opus-5-5");
        assert_eq!(calls[1].model, "claude-haiku-5-5");
        assert_eq!(calls[0].finish_reason.as_deref(), Some("tool_use"));
        assert!(matches!(
            calls[0].capture_granularity,
            CaptureGranularity::ModelCall
        ));
        assert_eq!(
            calls[0]
                .provider_usage_payload
                .as_ref()
                .and_then(|payload| payload.get("service_tier"))
                .and_then(Value::as_str),
            Some("standard")
        );
        assert_eq!(calls[0].output_tokens, Some(200));
        assert_eq!(calls[0].cumulative_output_tokens, 200);
        assert_eq!(calls[1].cumulative_output_tokens, 250);
        assert_eq!(calls[1].cumulative_input_tokens, 61_000);
        assert_eq!(calls[1].cumulative_cached_input_tokens, 59_000);
        assert_eq!(
            session.take_legacy_delta(),
            TokenConsumption {
                input_tokens: 61_000,
                cached_input_tokens: 59_000,
                output_tokens: 250,
            }
        );
    }

    #[test]
    fn usage_the_calls_did_not_cover_is_reported_once_at_the_end() {
        let (mut session, receiver) = recording_session("run-remainder");
        session.observe_line(r#"{"type":"turn.started"}"#);
        session.observe_line(CALL_A);
        session.observe_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":32000,"cached_input_tokens":29500,"output_tokens":260}}"#,
        );
        session.finish(SessionOutcome::Succeeded);
        let calls = completed_calls(&receiver);

        assert_eq!(calls.len(), 2);
        let remainder = &calls[1];
        assert_eq!(remainder.call_index, 1);
        assert_eq!(remainder.input_tokens, Some(2_000));
        assert_eq!(remainder.cached_input_tokens, Some(500));
        assert_eq!(remainder.output_tokens, Some(60));
        assert!(matches!(
            remainder.usage_source,
            UsageSource::WorkerCalculated
        ));
        assert_eq!(remainder.cumulative_input_tokens, 32_000);
        assert_eq!(remainder.cumulative_output_tokens, 260);
        assert_eq!(
            session.take_legacy_delta(),
            TokenConsumption {
                input_tokens: 32_000,
                cached_input_tokens: 29_500,
                output_tokens: 260,
            }
        );
    }

    #[test]
    fn codex_calls_read_back_keep_the_times_codex_recorded() {
        let (mut session, receiver) = recording_session("run-codex");
        session.observe_line(r#"{"type":"turn.started"}"#);
        session.observe_line(
            r#"{"type":"model_call.completed","completed_at":"2026-11-08T21:18:55.758Z","usage":{"input_tokens":25838,"cached_input_tokens":13056,"output_tokens":194}}"#,
        );
        session.observe_line(
            r#"{"type":"model_call.completed","started_at":"2026-11-08T21:18:55.758Z","completed_at":"2026-11-08T21:18:59.994Z","usage":{"input_tokens":27585,"cached_input_tokens":13056,"output_tokens":79}}"#,
        );
        session.observe_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":53423,"cached_input_tokens":26112,"output_tokens":273}}"#,
        );
        session.finish(SessionOutcome::Succeeded);
        let calls = completed_calls(&receiver);

        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].completed_at.as_deref(),
            Some("2026-11-08T21:18:55.758Z")
        );
        assert_eq!(calls[1].started_at, "2026-11-08T21:18:55.758Z");
        assert_eq!(
            calls[1].completed_at.as_deref(),
            Some("2026-11-08T21:18:59.994Z")
        );
        assert_eq!(calls[1].model, "claude-opus-5-5", "the invocation's model");
        assert_eq!(calls[1].cumulative_input_tokens, 53_423);
        assert_eq!(calls[1].cumulative_output_tokens, 273);
    }

    #[test]
    fn only_timestamps_shaped_like_rfc3339_are_kept() {
        assert!(safe_timestamp("2026-11-08T21:18:55.758Z").is_some());
        assert!(safe_timestamp("2026-11-08T21:18:55Z").is_some());
        assert!(safe_timestamp("2026-11-08 21:18:55").is_none());
        assert!(safe_timestamp("2026-11-08T21:18:55.758Z<script>").is_none());
        assert!(safe_timestamp("").is_none());
    }

    #[test]
    fn one_agent_session_reports_multiple_inference_turns_and_distinct_context() {
        let mut session = AgentTelemetrySession::start(
            AgentInvocation {
                agent_name: "Codex".into(),
                run_id: "run-turns".into(),
                execution_sequence: 1,
                phase_name: "implementation".into(),
                provider: "openai".into(),
                model: "gpt-test".into(),
                attempt_number: 1,
                retry_of_call_id: None,
                context_estimates: Map::new(),
            },
            None,
        );
        session.observe_line(r#"{"type":"turn.started","context_tokens_before_call":4000}"#);
        session.observe_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":5000,"cached_input_tokens":3000,"output_tokens":500}}"#,
        );
        session.observe_line(r#"{"type":"turn.started","context_tokens_before_call":7000}"#);
        session.observe_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":8000,"cached_input_tokens":6000,"output_tokens":700}}"#,
        );

        let usage = session.budget_usage(2_000, 10_000);
        assert_eq!(session.model_call_count(), 2);
        assert_eq!(usage.inference_turns, 2);
        assert_eq!(usage.peak_context_tokens, 7_000);
        assert_eq!(usage.cumulative_cached_input_tokens, 9_000);
        assert_eq!(usage.cumulative_uncached_input_tokens, 4_000);
        assert_eq!(usage.output_tokens, 1_200);
        assert_ne!(usage.peak_context_tokens, 13_000);
    }
}
