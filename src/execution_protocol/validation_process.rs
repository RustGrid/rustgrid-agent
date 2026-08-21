use std::{
    collections::{BTreeSet, VecDeque},
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use super::{
    BoundedOutputStream, BoundedProcessOutput, EvidenceId, GateSemanticsObservation,
    ParserConfidence, ProfilePath, ValidationArtifactReceipt, ValidationContractError,
    ValidationDiagnostic, ValidationDiagnosticKind, ValidationInfrastructureFailureKind,
    ValidationParserKind, ValidationProcessCompleted, ValidationProcessRequest,
    ValidationProcessResult, ValidationProcessStarted, ValidationSourceLocation, stable_sha256,
};

const MAX_ENVIRONMENT_ENTRIES: usize = 128;
const MAX_ENVIRONMENT_NAME_BYTES: usize = 128;
const MAX_ENVIRONMENT_VALUE_BYTES: usize = 64 * 1024;
const MAX_ADAPTER_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PARSED_DIAGNOSTICS: usize = 128;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Materialized environment values are deliberately neither serializable nor
/// printable. Only their sorted names and canonical fingerprint cross the
/// process boundary.
pub(crate) struct ValidationProcessEnvironment {
    entries: Vec<(String, String)>,
    fingerprint: String,
}

impl ValidationProcessEnvironment {
    pub(crate) fn new(mut entries: Vec<(String, String)>) -> Result<Self, ValidationBoundaryError> {
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let mut normalized_names = BTreeSet::new();
        if entries.len() > MAX_ENVIRONMENT_ENTRIES
            || entries
                .iter()
                .any(|(name, _)| !normalized_names.insert(name.to_ascii_uppercase()))
            || entries.iter().any(|(name, value)| {
                !safe_environment_name(name)
                    || value.len() > MAX_ENVIRONMENT_VALUE_BYTES
                    || value.contains('\0')
            })
        {
            return Err(ValidationBoundaryError::new(
                "validation_environment_allowlist_invalid",
            ));
        }
        let fingerprint = environment_fingerprint(&entries);
        Ok(Self {
            entries,
            fingerprint,
        })
    }

    pub(crate) fn empty() -> Self {
        Self::new(Vec::new()).expect("the empty validation environment is valid")
    }

    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(name, _)| name.as_str())
    }
}

impl Drop for ValidationProcessEnvironment {
    fn drop(&mut self) {
        for (_, value) in &mut self.entries {
            value.zeroize();
        }
    }
}

impl fmt::Debug for ValidationProcessEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidationProcessEnvironment")
            .field("names", &self.names().collect::<Vec<_>>())
            .field("fingerprint", &self.fingerprint)
            .field("values", &"<redacted>")
            .finish()
    }
}

/// Supplies the exact non-secret environment committed by the request's
/// environment fingerprint.
pub(crate) trait ValidationEnvironmentSource {
    fn load(
        &self,
        request: &ValidationProcessRequest,
    ) -> Result<ValidationProcessEnvironment, ValidationBoundaryError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValidationProcessAuthorityState {
    Authorized,
    LeaseLost,
}

/// Revalidates the reducer-issued request against live repository and lease
/// authority. Implementations must reject stale revisions, dependency drift,
/// invalid payload/gate/policy bindings, and revoked leases.
pub(crate) trait ValidationProcessAuthority {
    fn validate_before_spawn(
        &self,
        request: &ValidationProcessRequest,
        canonical_repository_root: &Path,
    ) -> Result<ValidationProcessAuthorityState, ValidationBoundaryError>;

    fn current_state(&self, request: &ValidationProcessRequest) -> ValidationProcessAuthorityState;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValidationOutputStreamKind {
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValidationOutputSegment {
    Head,
    Tail,
}

/// Persists one bounded output segment. Implementations must return a receipt
/// for exactly `bytes`; the adapter verifies its content, length, locator, and
/// run/stream/segment binding with `expected_artifact_persistence_receipt`.
pub(crate) trait ValidationArtifactSink {
    fn persist(
        &mut self,
        request: &ValidationProcessRequest,
        stream: ValidationOutputStreamKind,
        segment: ValidationOutputSegment,
        bytes: &[u8],
    ) -> Result<ValidationArtifactReceipt, ValidationBoundaryError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ValidationObservationWrite {
    Recorded,
    AlreadyRecorded,
    DefinitelyNotRecorded(ValidationBoundaryError),
    Indeterminate(ValidationBoundaryError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValidationObservationDurability {
    Recorded,
    AlreadyRecorded,
    DefinitelyNotRecorded,
    Indeterminate,
}

/// Durably records process observations. Scheduling is intentionally absent;
/// the reducer persists `ValidationScheduled` first. Ambiguous writes are
/// explicit so the adapter never invents a conflicting observation hash.
/// `AlreadyRecorded` means the sink reconciled the exact observation hash, not
/// merely its run or effect identity.
pub(crate) trait ValidationObservationSink {
    fn record_started(&mut self, started: &ValidationProcessStarted) -> ValidationObservationWrite;

    fn record_completed(
        &mut self,
        completed: &ValidationProcessCompleted,
    ) -> ValidationObservationWrite;
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ValidationBoundaryError {
    safe_code: String,
}

impl ValidationBoundaryError {
    pub(crate) fn new(safe_code: impl Into<String>) -> Self {
        let safe_code = safe_code.into();
        Self {
            safe_code: if safe_code_is_valid(&safe_code) {
                safe_code
            } else {
                "validation_boundary_failure".into()
            },
        }
    }

    pub(crate) fn safe_code(&self) -> &str {
        &self.safe_code
    }
}

impl fmt::Debug for ValidationBoundaryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidationBoundaryError")
            .field("safe_code", &self.safe_code)
            .finish()
    }
}

impl fmt::Display for ValidationBoundaryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.safe_code)
    }
}

impl std::error::Error for ValidationBoundaryError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MaterializedValidationProcessOutcome {
    pub(crate) started: Option<ValidationProcessStarted>,
    pub(crate) completed: ValidationProcessCompleted,
    pub(crate) parser_confidence: Option<ParserConfidence>,
    pub(crate) semantics: Option<GateSemanticsObservation>,
    pub(crate) diagnostics: Vec<ValidationDiagnostic>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ValidationProcessAdapterError {
    pub(crate) kind: ValidationInfrastructureFailureKind,
    pub(crate) safe_code: String,
    pub(crate) started: Option<Box<ValidationProcessStarted>>,
    pub(crate) completed: Option<Box<ValidationProcessCompleted>>,
    pub(crate) started_durability: Option<ValidationObservationDurability>,
    pub(crate) completed_durability: Option<ValidationObservationDurability>,
}

impl ValidationProcessAdapterError {
    fn new(
        kind: ValidationInfrastructureFailureKind,
        safe_code: impl Into<String>,
        started: Option<ValidationProcessStarted>,
        completed: Option<ValidationProcessCompleted>,
        started_durability: Option<ValidationObservationDurability>,
        completed_durability: Option<ValidationObservationDurability>,
    ) -> Self {
        let safe_code = safe_code.into();
        Self {
            kind,
            safe_code: if safe_code_is_valid(&safe_code) {
                safe_code
            } else {
                "validation_adapter_failure".into()
            },
            started: started.map(Box::new),
            completed: completed.map(Box::new),
            started_durability,
            completed_durability,
        }
    }
}

impl fmt::Debug for ValidationProcessAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidationProcessAdapterError")
            .field("kind", &self.kind)
            .field("safe_code", &self.safe_code)
            .field("started", &self.started)
            .field("completed", &self.completed)
            .field("started_durability", &self.started_durability)
            .field("completed_durability", &self.completed_durability)
            .finish()
    }
}

impl fmt::Display for ValidationProcessAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "validation process adapter failed with {}",
            self.safe_code
        )
    }
}

impl std::error::Error for ValidationProcessAdapterError {}

fn unobserved_adapter_error(
    kind: ValidationInfrastructureFailureKind,
    safe_code: &str,
) -> ValidationProcessAdapterError {
    ValidationProcessAdapterError::new(kind, safe_code, None, None, None, None)
}

fn process_authority_state(
    authority: &dyn ValidationProcessAuthority,
    request: &ValidationProcessRequest,
    canonical_repository_root: &Path,
) -> Result<ValidationProcessAuthorityState, ValidationProcessAdapterError> {
    match authority.validate_before_spawn(request, canonical_repository_root) {
        Ok(state) => Ok(state),
        Err(error) => Err(unobserved_adapter_error(
            ValidationInfrastructureFailureKind::Transport,
            error.safe_code(),
        )),
    }
}

/// Ephemeral parser input. It is deliberately non-serializable and its Debug
/// implementation exposes only byte counts.
struct ValidationParserPathScope {
    repository_root: PathBuf,
    working_directory: ProfilePath,
}

pub(crate) struct RetainedValidationOutput {
    stdout: RetainedOutputStream,
    stderr: RetainedOutputStream,
    path_scope: ValidationParserPathScope,
}

impl RetainedValidationOutput {
    pub(crate) fn stdout_head(&self) -> &[u8] {
        &self.stdout.head
    }

    pub(crate) fn stdout_tail(&self) -> &[u8] {
        &self.stdout.tail
    }

    pub(crate) fn stderr_head(&self) -> &[u8] {
        &self.stderr.head
    }

    pub(crate) fn stderr_tail(&self) -> &[u8] {
        &self.stderr.tail
    }

    fn combined_lossy(&self) -> Zeroizing<String> {
        let mut combined = Zeroizing::new(String::new());
        append_lossy_segment(&mut combined, &self.stdout.head);
        append_lossy_segment(&mut combined, &self.stdout.tail);
        append_lossy_segment(&mut combined, &self.stderr.head);
        append_lossy_segment(&mut combined, &self.stderr.tail);
        combined
    }
}

impl fmt::Debug for RetainedValidationOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedValidationOutput")
            .field("stdout_original_bytes", &self.stdout.original_bytes)
            .field("stdout_captured_bytes", &self.stdout.captured_bytes())
            .field("stderr_original_bytes", &self.stderr.original_bytes)
            .field("stderr_captured_bytes", &self.stderr.captured_bytes())
            .field("content", &"<redacted>")
            .finish()
    }
}

pub(crate) type ValidationParserObservation = (
    ParserConfidence,
    GateSemanticsObservation,
    Vec<ValidationDiagnostic>,
);

pub(crate) trait ValidationOutputParser {
    fn parse(
        &self,
        request: &ValidationProcessRequest,
        output: &RetainedValidationOutput,
    ) -> ValidationParserObservation;
}

/// Executes the already-persisted validation effect without a shell. The
/// caller owns reducer state and cancellation authority; this adapter owns only
/// the subprocess, bounded raw bytes, and ordered durable observations.
pub(crate) fn run_validation_process(
    request: &ValidationProcessRequest,
    repository_root: &Path,
    authority: &dyn ValidationProcessAuthority,
    environment_source: &dyn ValidationEnvironmentSource,
    artifact_sink: &mut dyn ValidationArtifactSink,
    observation_sink: &mut dyn ValidationObservationSink,
    canceled: &AtomicBool,
) -> Result<MaterializedValidationProcessOutcome, ValidationProcessAdapterError> {
    let invoked_at = Instant::now();
    if canceled.load(Ordering::SeqCst) {
        return record_startless_infrastructure_failure(
            request,
            observation_sink,
            elapsed_ms(invoked_at, request.timeout_ms),
            ValidationInfrastructureFailureKind::Canceled,
            "validation_process_canceled_before_spawn",
        );
    }
    let canonical_repository_root = match resolve_repository_root(repository_root) {
        Ok(root) => root,
        Err(code) => {
            return Err(unobserved_adapter_error(
                ValidationInfrastructureFailureKind::Spawn,
                code,
            ));
        }
    };
    if process_authority_state(authority, request, &canonical_repository_root)?
        == ValidationProcessAuthorityState::LeaseLost
    {
        return record_startless_infrastructure_failure(
            request,
            observation_sink,
            elapsed_ms(invoked_at, request.timeout_ms),
            ValidationInfrastructureFailureKind::LeaseLost,
            "validation_process_lease_lost_before_spawn",
        );
    }
    if request.timeout_ms == 0
        || request.output_limit_bytes == 0
        || request.output_limit_bytes > MAX_ADAPTER_OUTPUT_BYTES
        || request.command.executable.is_empty()
    {
        return record_spawn_failure(
            request,
            observation_sink,
            elapsed_ms(invoked_at, request.timeout_ms),
            "validation_process_request_not_executable",
        );
    }

    let working_directory = match resolve_working_directory(&canonical_repository_root, request) {
        Ok(path) => path,
        Err(code) => {
            return record_spawn_failure(
                request,
                observation_sink,
                elapsed_ms(invoked_at, request.timeout_ms),
                code,
            );
        }
    };
    if !process_tree_containment_supported() {
        return record_spawn_failure(
            request,
            observation_sink,
            elapsed_ms(invoked_at, request.timeout_ms),
            "validation_process_tree_containment_unsupported",
        );
    }

    let environment = match environment_source.load(request) {
        Ok(environment) => environment,
        Err(error) => {
            return Err(unobserved_adapter_error(
                ValidationInfrastructureFailureKind::Transport,
                error.safe_code(),
            ));
        }
    };
    if environment.fingerprint() != request.command.environment_fingerprint {
        return Err(unobserved_adapter_error(
            ValidationInfrastructureFailureKind::Transport,
            "validation_environment_fingerprint_mismatch",
        ));
    }

    let mut command = Command::new(&request.command.executable);
    command
        .args(&request.command.args)
        .current_dir(&working_directory)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in &environment.entries {
        command.env(name, value);
    }
    configure_process_group(&mut command);

    if canceled.load(Ordering::SeqCst) {
        return record_startless_infrastructure_failure(
            request,
            observation_sink,
            elapsed_ms(invoked_at, request.timeout_ms),
            ValidationInfrastructureFailureKind::Canceled,
            "validation_process_canceled_before_spawn",
        );
    }
    if process_authority_state(authority, request, &canonical_repository_root)?
        == ValidationProcessAuthorityState::LeaseLost
    {
        return record_startless_infrastructure_failure(
            request,
            observation_sink,
            elapsed_ms(invoked_at, request.timeout_ms),
            ValidationInfrastructureFailureKind::LeaseLost,
            "validation_process_lease_lost_before_spawn",
        );
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return record_spawn_failure(
                request,
                observation_sink,
                elapsed_ms(invoked_at, request.timeout_ms),
                "validation_process_spawn_failed",
            );
        }
    };
    let process_started_at = Instant::now();
    let started =
        match ValidationProcessStarted::new(request, process_handle_hash(request, child.id())) {
            Ok(started) => started,
            Err(_) => {
                let _ = terminate_process_tree(&mut child);
                let _ = child.wait();
                return Err(ValidationProcessAdapterError::new(
                    ValidationInfrastructureFailureKind::Transport,
                    "validation_process_start_observation_invalid",
                    None,
                    None,
                    None,
                    None,
                ));
            }
        };
    let started_durability = match observation_sink.record_started(&started) {
        ValidationObservationWrite::Recorded => ValidationObservationDurability::Recorded,
        ValidationObservationWrite::AlreadyRecorded => {
            ValidationObservationDurability::AlreadyRecorded
        }
        ValidationObservationWrite::DefinitelyNotRecorded(error) => {
            let _ = terminate_process_tree(&mut child);
            let _ = child.wait();
            let completed = infrastructure_completion(
                request,
                Some(&started),
                elapsed_ms(process_started_at, request.timeout_ms),
                ValidationInfrastructureFailureKind::Journal,
                error.safe_code(),
                empty_process_output(),
            )
            .ok();
            let completed_durability = completed
                .as_ref()
                .map(|_| ValidationObservationDurability::DefinitelyNotRecorded);
            return Err(ValidationProcessAdapterError::new(
                ValidationInfrastructureFailureKind::Journal,
                error.safe_code(),
                Some(started),
                completed,
                Some(ValidationObservationDurability::DefinitelyNotRecorded),
                completed_durability,
            ));
        }
        ValidationObservationWrite::Indeterminate(error) => {
            let _ = terminate_process_tree(&mut child);
            let _ = child.wait();
            return Err(ValidationProcessAdapterError::new(
                ValidationInfrastructureFailureKind::Journal,
                error.safe_code(),
                Some(started),
                None,
                Some(ValidationObservationDurability::Indeterminate),
                None,
            ));
        }
    };

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = terminate_process_tree(&mut child);
            let _ = child.wait();
            return record_started_infrastructure_failure(
                request,
                &started,
                started_durability,
                observation_sink,
                elapsed_ms(process_started_at, request.timeout_ms),
                ValidationInfrastructureFailureKind::Transport,
                "validation_stdout_pipe_missing",
                empty_process_output(),
            );
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(stdout);
            let _ = terminate_process_tree(&mut child);
            let _ = child.wait();
            return record_started_infrastructure_failure(
                request,
                &started,
                started_durability,
                observation_sink,
                elapsed_ms(process_started_at, request.timeout_ms),
                ValidationInfrastructureFailureKind::Transport,
                "validation_stderr_pipe_missing",
                empty_process_output(),
            );
        }
    };

    let capture_capacity = usize::try_from(request.output_limit_bytes)
        .unwrap_or(usize::MAX)
        .min(MAX_ADAPTER_OUTPUT_BYTES as usize);
    let stop_readers = Arc::new(AtomicBool::new(false));
    let reader_failed = Arc::new(AtomicBool::new(false));
    let stdout_reader = match spawn_capture_reader(
        stdout,
        capture_capacity,
        Arc::clone(&stop_readers),
        Arc::clone(&reader_failed),
    ) {
        Ok(reader) => reader,
        Err(_) => {
            let _ = terminate_process_tree(&mut child);
            let _ = child.wait();
            return record_started_infrastructure_failure(
                request,
                &started,
                started_durability,
                observation_sink,
                elapsed_ms(process_started_at, request.timeout_ms),
                ValidationInfrastructureFailureKind::Transport,
                "validation_stdout_capture_failed",
                empty_process_output(),
            );
        }
    };
    let stderr_reader = match spawn_capture_reader(
        stderr,
        capture_capacity,
        Arc::clone(&stop_readers),
        Arc::clone(&reader_failed),
    ) {
        Ok(reader) => reader,
        Err(_) => {
            let _ = terminate_process_tree(&mut child);
            let _ = child.wait();
            stop_readers.store(true, Ordering::SeqCst);
            let _ = stdout_reader.join();
            return record_started_infrastructure_failure(
                request,
                &started,
                started_durability,
                observation_sink,
                elapsed_ms(process_started_at, request.timeout_ms),
                ValidationInfrastructureFailureKind::Transport,
                "validation_stderr_capture_failed",
                empty_process_output(),
            );
        }
    };

    let deadline = Duration::from_millis(request.timeout_ms);
    let mut termination = loop {
        match child.try_wait() {
            Ok(Some(status)) => break ProcessTermination::Exited(status),
            Ok(None) => {}
            Err(_) => {
                break ProcessTermination::Infrastructure {
                    code: "validation_process_wait_failed",
                };
            }
        }
        if reader_failed.load(Ordering::SeqCst) {
            break ProcessTermination::Infrastructure {
                code: "validation_output_transport_failed",
            };
        }
        if canceled.load(Ordering::SeqCst) {
            break ProcessTermination::Canceled;
        }
        if authority.current_state(request) == ValidationProcessAuthorityState::LeaseLost {
            break ProcessTermination::LeaseLost;
        }
        if process_started_at.elapsed() >= deadline {
            break ProcessTermination::TimedOut;
        }
        thread::sleep(PROCESS_POLL_INTERVAL);
    };

    if terminate_process_tree(&mut child).is_err()
        && !matches!(termination, ProcessTermination::LeaseLost)
    {
        termination = ProcessTermination::Infrastructure {
            code: "validation_process_tree_cleanup_failed",
        };
    }
    if child.wait().is_err() && !matches!(termination, ProcessTermination::LeaseLost) {
        termination = ProcessTermination::Infrastructure {
            code: "validation_process_reap_failed",
        };
    }
    stop_readers.store(true, Ordering::SeqCst);
    let stdout_capture = join_capture(stdout_reader);
    let stderr_capture = join_capture(stderr_reader);
    if authority.current_state(request) == ValidationProcessAuthorityState::LeaseLost
        || matches!(termination, ProcessTermination::LeaseLost)
    {
        drop(stdout_capture);
        drop(stderr_capture);
        return Err(ValidationProcessAdapterError::new(
            ValidationInfrastructureFailureKind::LeaseLost,
            "validation_process_lease_lost",
            Some(started),
            None,
            Some(started_durability),
            None,
        ));
    }
    let (stdout_capture, stderr_capture) = match (stdout_capture, stderr_capture) {
        (Ok(stdout), Ok(stderr)) => (stdout, stderr),
        _ => {
            return record_started_infrastructure_failure(
                request,
                &started,
                started_durability,
                observation_sink,
                elapsed_ms(process_started_at, request.timeout_ms),
                ValidationInfrastructureFailureKind::Transport,
                "validation_output_capture_failed",
                empty_process_output(),
            );
        }
    };

    let retained = match retain_combined_output(
        stdout_capture,
        stderr_capture,
        request.output_limit_bytes,
        canonical_repository_root,
        request.command.working_directory.clone(),
    ) {
        Ok(retained) => retained,
        Err(code) => {
            return record_started_infrastructure_failure(
                request,
                &started,
                started_durability,
                observation_sink,
                elapsed_ms(process_started_at, request.timeout_ms),
                ValidationInfrastructureFailureKind::Transport,
                code,
                empty_process_output(),
            );
        }
    };
    if authority.current_state(request) == ValidationProcessAuthorityState::LeaseLost {
        return Err(ValidationProcessAdapterError::new(
            ValidationInfrastructureFailureKind::LeaseLost,
            "validation_process_lease_lost",
            Some(started),
            None,
            Some(started_durability),
            None,
        ));
    }
    let bounded_output = match persist_bounded_output(request, &retained, artifact_sink) {
        Ok(output) => output,
        Err(error) => {
            return record_started_infrastructure_failure(
                request,
                &started,
                started_durability,
                observation_sink,
                elapsed_ms(process_started_at, request.timeout_ms),
                ValidationInfrastructureFailureKind::Journal,
                error.safe_code(),
                empty_process_output(),
            );
        }
    };
    if authority.current_state(request) == ValidationProcessAuthorityState::LeaseLost {
        return Err(ValidationProcessAdapterError::new(
            ValidationInfrastructureFailureKind::LeaseLost,
            "validation_process_lease_lost",
            Some(started),
            None,
            Some(started_durability),
            None,
        ));
    }

    let result = match termination {
        ProcessTermination::Exited(status) => process_exit_result(status),
        ProcessTermination::TimedOut => ValidationProcessResult::InfrastructureFailure {
            kind: ValidationInfrastructureFailureKind::Timeout,
            safe_code: "validation_process_timeout".into(),
        },
        ProcessTermination::Canceled => ValidationProcessResult::InfrastructureFailure {
            kind: ValidationInfrastructureFailureKind::Canceled,
            safe_code: "validation_process_canceled".into(),
        },
        ProcessTermination::LeaseLost => ValidationProcessResult::InfrastructureFailure {
            kind: ValidationInfrastructureFailureKind::LeaseLost,
            safe_code: "validation_process_lease_lost".into(),
        },
        ProcessTermination::Infrastructure { code } => {
            ValidationProcessResult::InfrastructureFailure {
                kind: ValidationInfrastructureFailureKind::Transport,
                safe_code: code.into(),
            }
        }
    };
    let completed = completed_or_adapter_error(
        request,
        Some(&started),
        Some(started_durability),
        elapsed_ms(process_started_at, request.timeout_ms),
        result,
        bounded_output,
    )?;
    if authority.current_state(request) == ValidationProcessAuthorityState::LeaseLost {
        return Err(ValidationProcessAdapterError::new(
            ValidationInfrastructureFailureKind::LeaseLost,
            "validation_process_lease_lost",
            Some(started),
            None,
            Some(started_durability),
            None,
        ));
    }
    let completed = record_completion_observation(
        request,
        Some(&started),
        started_durability,
        observation_sink,
        completed,
    )?;

    let Some(exit_code) = exited_code(&completed.result) else {
        return Ok(MaterializedValidationProcessOutcome {
            started: Some(started),
            completed,
            parser_confidence: None,
            semantics: None,
            diagnostics: Vec::new(),
        });
    };
    let (parser_confidence, semantics, mut diagnostics) =
        parser_for(request.parser).parse(request, &retained);
    if exit_code != 0
        && diagnostics.is_empty()
        && let Ok(diagnostic) = ValidationDiagnostic::new(
            ValidationDiagnosticKind::UnclassifiedFailure,
            None,
            None,
            None,
            None,
            BTreeSet::new(),
            BTreeSet::new(),
            "validation_failure_unclassified".into(),
            stable_sha256(&[
                "execution-protocol-v1:validation-parser-fallback",
                &completed.completion_hash,
            ]),
            ParserConfidence::Fallback,
        )
    {
        diagnostics.push(diagnostic);
    }
    Ok(MaterializedValidationProcessOutcome {
        started: Some(started),
        completed,
        parser_confidence: Some(parser_confidence),
        semantics: Some(semantics),
        diagnostics,
    })
}

fn record_spawn_failure(
    request: &ValidationProcessRequest,
    observation_sink: &mut dyn ValidationObservationSink,
    duration_ms: u64,
    safe_code: &str,
) -> Result<MaterializedValidationProcessOutcome, ValidationProcessAdapterError> {
    record_startless_infrastructure_failure(
        request,
        observation_sink,
        duration_ms,
        ValidationInfrastructureFailureKind::Spawn,
        safe_code,
    )
}

fn record_startless_infrastructure_failure(
    request: &ValidationProcessRequest,
    observation_sink: &mut dyn ValidationObservationSink,
    duration_ms: u64,
    kind: ValidationInfrastructureFailureKind,
    safe_code: &str,
) -> Result<MaterializedValidationProcessOutcome, ValidationProcessAdapterError> {
    let completed = infrastructure_completion(
        request,
        None,
        duration_ms,
        kind,
        safe_code,
        empty_process_output(),
    )
    .map_err(|_| {
        ValidationProcessAdapterError::new(
            ValidationInfrastructureFailureKind::Transport,
            "validation_startless_completion_invalid",
            None,
            None,
            None,
            None,
        )
    })?;
    let completed = record_completion_observation(
        request,
        None,
        ValidationObservationDurability::DefinitelyNotRecorded,
        observation_sink,
        completed,
    )?;
    Ok(MaterializedValidationProcessOutcome {
        started: None,
        completed,
        parser_confidence: None,
        semantics: None,
        diagnostics: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
fn record_started_infrastructure_failure(
    request: &ValidationProcessRequest,
    started: &ValidationProcessStarted,
    started_durability: ValidationObservationDurability,
    observation_sink: &mut dyn ValidationObservationSink,
    duration_ms: u64,
    kind: ValidationInfrastructureFailureKind,
    safe_code: &str,
    output: BoundedProcessOutput,
) -> Result<MaterializedValidationProcessOutcome, ValidationProcessAdapterError> {
    let completed =
        infrastructure_completion(request, Some(started), duration_ms, kind, safe_code, output)
            .map_err(|_| {
                ValidationProcessAdapterError::new(
                    ValidationInfrastructureFailureKind::Transport,
                    "validation_infrastructure_completion_invalid",
                    Some(started.clone()),
                    None,
                    Some(started_durability),
                    None,
                )
            })?;
    let completed = record_completion_observation(
        request,
        Some(started),
        started_durability,
        observation_sink,
        completed,
    )?;
    Ok(MaterializedValidationProcessOutcome {
        started: Some(started.clone()),
        completed,
        parser_confidence: None,
        semantics: None,
        diagnostics: Vec::new(),
    })
}

fn record_completion_observation(
    request: &ValidationProcessRequest,
    started: Option<&ValidationProcessStarted>,
    started_durability: ValidationObservationDurability,
    observation_sink: &mut dyn ValidationObservationSink,
    completed: ValidationProcessCompleted,
) -> Result<ValidationProcessCompleted, ValidationProcessAdapterError> {
    match observation_sink.record_completed(&completed) {
        ValidationObservationWrite::Recorded | ValidationObservationWrite::AlreadyRecorded => {
            Ok(completed)
        }
        ValidationObservationWrite::Indeterminate(error) => {
            Err(ValidationProcessAdapterError::new(
                ValidationInfrastructureFailureKind::Journal,
                error.safe_code(),
                started.cloned(),
                Some(completed),
                started.map(|_| started_durability),
                Some(ValidationObservationDurability::Indeterminate),
            ))
        }
        ValidationObservationWrite::DefinitelyNotRecorded(error) => {
            let can_record_journal = started.is_some()
                && !matches!(
                    &completed.result,
                    ValidationProcessResult::InfrastructureFailure {
                        kind: ValidationInfrastructureFailureKind::Journal,
                        ..
                    }
                );
            if !can_record_journal {
                return Err(ValidationProcessAdapterError::new(
                    ValidationInfrastructureFailureKind::Journal,
                    error.safe_code(),
                    started.cloned(),
                    Some(completed),
                    started.map(|_| started_durability),
                    Some(ValidationObservationDurability::DefinitelyNotRecorded),
                ));
            }
            let journal = infrastructure_completion(
                request,
                started,
                completed.duration_ms,
                ValidationInfrastructureFailureKind::Journal,
                error.safe_code(),
                completed.output.clone(),
            )
            .map_err(|_| {
                ValidationProcessAdapterError::new(
                    ValidationInfrastructureFailureKind::Journal,
                    "validation_journal_completion_invalid",
                    started.cloned(),
                    None,
                    started.map(|_| started_durability),
                    None,
                )
            })?;
            match observation_sink.record_completed(&journal) {
                ValidationObservationWrite::Recorded
                | ValidationObservationWrite::AlreadyRecorded => Ok(journal),
                ValidationObservationWrite::DefinitelyNotRecorded(retry_error) => {
                    Err(ValidationProcessAdapterError::new(
                        ValidationInfrastructureFailureKind::Journal,
                        retry_error.safe_code(),
                        started.cloned(),
                        Some(journal),
                        started.map(|_| started_durability),
                        Some(ValidationObservationDurability::DefinitelyNotRecorded),
                    ))
                }
                ValidationObservationWrite::Indeterminate(retry_error) => {
                    Err(ValidationProcessAdapterError::new(
                        ValidationInfrastructureFailureKind::Journal,
                        retry_error.safe_code(),
                        started.cloned(),
                        Some(journal),
                        started.map(|_| started_durability),
                        Some(ValidationObservationDurability::Indeterminate),
                    ))
                }
            }
        }
    }
}

fn infrastructure_completion(
    request: &ValidationProcessRequest,
    started: Option<&ValidationProcessStarted>,
    duration_ms: u64,
    kind: ValidationInfrastructureFailureKind,
    safe_code: &str,
    output: BoundedProcessOutput,
) -> Result<ValidationProcessCompleted, ValidationContractError> {
    ValidationProcessCompleted::new(
        request,
        started,
        duration_ms,
        ValidationProcessResult::InfrastructureFailure {
            kind,
            safe_code: normalized_safe_code(safe_code),
        },
        output,
    )
}

fn completed_or_adapter_error(
    request: &ValidationProcessRequest,
    started: Option<&ValidationProcessStarted>,
    started_durability: Option<ValidationObservationDurability>,
    duration_ms: u64,
    result: ValidationProcessResult,
    output: BoundedProcessOutput,
) -> Result<ValidationProcessCompleted, ValidationProcessAdapterError> {
    ValidationProcessCompleted::new(request, started, duration_ms, result, output).map_err(|_| {
        ValidationProcessAdapterError::new(
            ValidationInfrastructureFailureKind::Transport,
            "validation_process_completion_invalid",
            started.cloned(),
            None,
            started_durability,
            None,
        )
    })
}

fn resolve_repository_root(repository_root: &Path) -> Result<PathBuf, &'static str> {
    let root =
        fs::canonicalize(repository_root).map_err(|_| "validation_repository_root_unavailable")?;
    if !root.is_dir() {
        return Err("validation_repository_root_not_directory");
    }
    Ok(root)
}

fn resolve_working_directory(
    canonical_repository_root: &Path,
    request: &ValidationProcessRequest,
) -> Result<PathBuf, &'static str> {
    let candidate = if request.command.working_directory.is_root() {
        canonical_repository_root.to_path_buf()
    } else {
        canonical_repository_root.join(request.command.working_directory.as_str())
    };
    let candidate =
        fs::canonicalize(candidate).map_err(|_| "validation_working_directory_unavailable")?;
    if !candidate.is_dir() || !candidate.starts_with(canonical_repository_root) {
        return Err("validation_working_directory_outside_repository");
    }
    Ok(candidate)
}

fn process_handle_hash(request: &ValidationProcessRequest, pid: u32) -> String {
    stable_sha256(&[
        "execution-protocol-v1:validation-process-handle",
        request.schedule.run_id.as_str(),
        &pid.to_string(),
    ])
}

const fn process_tree_containment_supported() -> bool {
    cfg!(unix)
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: setsid is async-signal-safe and needs no memory allocation or
    // inherited Rust state in the post-fork child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_process_tree(child: &mut Child) -> io::Result<()> {
    let pid = i32::try_from(child.id())
        .map_err(|_| io::Error::other("child pid does not fit process-group identifier"))?;
    // SAFETY: the child created a new session whose process-group id equals its
    // pid. A negative pid targets that process group and never this process.
    if unsafe { libc::kill(-pid, libc::SIGKILL) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            let _ = child.kill();
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn terminate_process_tree(child: &mut Child) -> io::Result<()> {
    match child.kill() {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn spawn_capture_reader<R>(
    reader: R,
    capacity: usize,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
) -> io::Result<thread::JoinHandle<io::Result<RawStreamCapture>>>
where
    R: Read + std::os::fd::AsRawFd + Send + 'static,
{
    let descriptor = reader.as_raw_fd();
    // SAFETY: descriptor belongs to the live pipe reader. F_GETFL/F_SETFL do
    // not retain pointers and only add O_NONBLOCK to its existing flags.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(thread::spawn(move || {
        read_head_tail(reader, capacity, &stop, &failed, true)
    }))
}

#[cfg(not(unix))]
fn spawn_capture_reader<R>(
    reader: R,
    capacity: usize,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
) -> io::Result<thread::JoinHandle<io::Result<RawStreamCapture>>>
where
    R: Read + Send + 'static,
{
    Ok(thread::spawn(move || {
        read_head_tail(reader, capacity, &stop, &failed, false)
    }))
}

fn read_head_tail(
    mut reader: impl Read,
    capacity: usize,
    stop: &AtomicBool,
    failed: &AtomicBool,
    nonblocking: bool,
) -> io::Result<RawStreamCapture> {
    // Bias the retained odd byte toward the tail. Failure identities are often
    // emitted last, and a one-byte truncation must still have a real tail.
    let head_capacity = capacity / 2;
    let tail_capacity = capacity.saturating_sub(head_capacity);
    let mut capture = RawStreamCapture {
        original_bytes: 0,
        head: Vec::with_capacity(head_capacity.min(64 * 1024)),
        tail: VecDeque::with_capacity(tail_capacity.min(64 * 1024)),
    };
    let mut buffer = Zeroizing::new([0_u8; 16 * 1024]);
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match reader.read(&mut *buffer) {
            Ok(0) => break,
            Ok(read) => capture.push(&buffer[..read], head_capacity, tail_capacity),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if nonblocking && error.kind() == io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => {
                failed.store(true, Ordering::SeqCst);
                return Err(error);
            }
        }
    }
    Ok(capture)
}

struct RawStreamCapture {
    original_bytes: u64,
    head: Vec<u8>,
    tail: VecDeque<u8>,
}

impl RawStreamCapture {
    fn push(&mut self, bytes: &[u8], head_capacity: usize, tail_capacity: usize) {
        self.original_bytes = self
            .original_bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let head_remaining = head_capacity.saturating_sub(self.head.len());
        self.head
            .extend_from_slice(&bytes[..bytes.len().min(head_remaining)]);
        if tail_capacity == 0 {
            return;
        }
        for byte in bytes {
            if self.tail.len() == tail_capacity {
                self.tail.pop_front();
            }
            self.tail.push_back(*byte);
        }
    }

    fn retain(mut self, allocation: u64) -> Result<RetainedOutputStream, &'static str> {
        let allocation =
            usize::try_from(allocation).map_err(|_| "validation_output_allocation_overflow")?;
        let original = usize::try_from(self.original_bytes).unwrap_or(usize::MAX);
        if allocation == 0 && original > 0 {
            return Err("validation_output_limit_cannot_represent_streams");
        }
        let captured = original.min(allocation);
        let truncated = original > captured;
        let (head_len, tail_len) = if truncated {
            (captured / 2, captured - (captured / 2))
        } else {
            (captured, 0)
        };
        let head = if truncated {
            self.head[..head_len.min(self.head.len())].to_vec()
        } else {
            let prefix_len = captured.min(self.head.len());
            let mut full = self.head[..prefix_len].to_vec();
            let remaining = captured.saturating_sub(prefix_len);
            if remaining > 0 {
                full.extend(
                    self.tail
                        .iter()
                        .skip(self.tail.len().saturating_sub(remaining)),
                );
            }
            full
        };
        let tail = if tail_len == 0 {
            Vec::new()
        } else {
            self.tail
                .iter()
                .skip(self.tail.len().saturating_sub(tail_len))
                .copied()
                .collect()
        };
        self.head.zeroize();
        self.tail.iter_mut().for_each(|byte| *byte = 0);
        if head.len().saturating_add(tail.len()) != captured {
            return Err("validation_output_capture_projection_invalid");
        }
        Ok(RetainedOutputStream {
            original_bytes: self.original_bytes,
            head,
            tail,
            truncated,
        })
    }
}

impl Drop for RawStreamCapture {
    fn drop(&mut self) {
        self.head.zeroize();
        self.tail.iter_mut().for_each(|byte| *byte = 0);
    }
}

struct RetainedOutputStream {
    original_bytes: u64,
    head: Vec<u8>,
    tail: Vec<u8>,
    truncated: bool,
}

impl RetainedOutputStream {
    fn captured_bytes(&self) -> u64 {
        u64::try_from(self.head.len().saturating_add(self.tail.len())).unwrap_or(u64::MAX)
    }
}

impl Drop for RetainedOutputStream {
    fn drop(&mut self) {
        self.head.zeroize();
        self.tail.zeroize();
    }
}

fn join_capture(
    reader: thread::JoinHandle<io::Result<RawStreamCapture>>,
) -> Result<RawStreamCapture, ()> {
    reader.join().map_err(|_| ())?.map_err(|_| ())
}

fn retain_combined_output(
    stdout: RawStreamCapture,
    stderr: RawStreamCapture,
    limit: u64,
    repository_root: PathBuf,
    working_directory: ProfilePath,
) -> Result<RetainedValidationOutput, &'static str> {
    let (stdout_limit, stderr_limit) =
        allocate_stream_limits(stdout.original_bytes, stderr.original_bytes, limit)?;
    Ok(RetainedValidationOutput {
        stdout: stdout.retain(stdout_limit)?,
        stderr: stderr.retain(stderr_limit)?,
        path_scope: ValidationParserPathScope {
            repository_root,
            working_directory,
        },
    })
}

fn allocate_stream_limits(
    stdout_bytes: u64,
    stderr_bytes: u64,
    limit: u64,
) -> Result<(u64, u64), &'static str> {
    if stdout_bytes.saturating_add(stderr_bytes) <= limit {
        return Ok((stdout_bytes, stderr_bytes));
    }
    if limit == 0 || (limit == 1 && stdout_bytes > 0 && stderr_bytes > 0) {
        return Err("validation_output_limit_cannot_represent_streams");
    }
    let mut stderr_limit = stderr_bytes.min(limit.div_ceil(2));
    let mut stdout_limit = stdout_bytes.min(limit.saturating_sub(stderr_limit));
    let mut remaining = limit.saturating_sub(stdout_limit.saturating_add(stderr_limit));
    let stdout_extra = stdout_bytes.saturating_sub(stdout_limit).min(remaining);
    stdout_limit = stdout_limit.saturating_add(stdout_extra);
    remaining = remaining.saturating_sub(stdout_extra);
    stderr_limit =
        stderr_limit.saturating_add(stderr_bytes.saturating_sub(stderr_limit).min(remaining));
    if stdout_bytes > 0 && stdout_limit == 0 {
        stdout_limit = 1;
        stderr_limit = stderr_limit.saturating_sub(1);
    }
    if stderr_bytes > 0 && stderr_limit == 0 {
        stderr_limit = 1;
        stdout_limit = stdout_limit.saturating_sub(1);
    }
    Ok((stdout_limit, stderr_limit))
}

fn persist_bounded_output(
    request: &ValidationProcessRequest,
    retained: &RetainedValidationOutput,
    sink: &mut dyn ValidationArtifactSink,
) -> Result<BoundedProcessOutput, ValidationBoundaryError> {
    let stdout = persist_stream(
        request,
        ValidationOutputStreamKind::Stdout,
        &retained.stdout,
        sink,
    )?;
    let stderr = persist_stream(
        request,
        ValidationOutputStreamKind::Stderr,
        &retained.stderr,
        sink,
    )?;
    let output = BoundedProcessOutput { stdout, stderr };
    output
        .validate(request.output_limit_bytes)
        .map_err(|_| ValidationBoundaryError::new("validation_output_contract_invalid"))?;
    Ok(output)
}

fn persist_stream(
    request: &ValidationProcessRequest,
    stream: ValidationOutputStreamKind,
    retained: &RetainedOutputStream,
    sink: &mut dyn ValidationArtifactSink,
) -> Result<BoundedOutputStream, ValidationBoundaryError> {
    let captured_bytes = retained.captured_bytes();
    if captured_bytes == 0 {
        return Ok(BoundedOutputStream {
            original_bytes: 0,
            captured_bytes: 0,
            dropped_bytes: 0,
            truncated: false,
            head: None,
            tail: None,
        });
    }
    let head = persist_verified_artifact(
        sink,
        request,
        stream,
        ValidationOutputSegment::Head,
        &retained.head,
    )?;
    let tail = if retained.truncated {
        Some(persist_verified_artifact(
            sink,
            request,
            stream,
            ValidationOutputSegment::Tail,
            &retained.tail,
        )?)
    } else {
        None
    };
    Ok(BoundedOutputStream {
        original_bytes: retained.original_bytes,
        captured_bytes,
        dropped_bytes: retained.original_bytes.saturating_sub(captured_bytes),
        truncated: retained.truncated,
        head: Some(head),
        tail,
    })
}

fn persist_verified_artifact(
    sink: &mut dyn ValidationArtifactSink,
    request: &ValidationProcessRequest,
    stream: ValidationOutputStreamKind,
    segment: ValidationOutputSegment,
    bytes: &[u8],
) -> Result<ValidationArtifactReceipt, ValidationBoundaryError> {
    let receipt = sink.persist(request, stream, segment, bytes)?;
    receipt
        .validate()
        .map_err(|_| ValidationBoundaryError::new("validation_artifact_receipt_invalid"))?;
    if receipt.byte_len != u64::try_from(bytes.len()).unwrap_or(u64::MAX)
        || receipt.content_hash != hex::encode(Sha256::digest(bytes))
        || receipt.persistence_receipt_hash
            != expected_artifact_persistence_receipt(request, stream, segment, &receipt)
    {
        return Err(ValidationBoundaryError::new(
            "validation_artifact_receipt_mismatch",
        ));
    }
    Ok(receipt)
}

pub(crate) fn expected_artifact_persistence_receipt(
    request: &ValidationProcessRequest,
    stream: ValidationOutputStreamKind,
    segment: ValidationOutputSegment,
    receipt: &ValidationArtifactReceipt,
) -> String {
    stable_sha256(&[
        "execution-protocol-v1:validation-output-persistence-receipt",
        request.schedule.run_id.as_str(),
        match stream {
            ValidationOutputStreamKind::Stdout => "stdout",
            ValidationOutputStreamKind::Stderr => "stderr",
        },
        match segment {
            ValidationOutputSegment::Head => "head",
            ValidationOutputSegment::Tail => "tail",
        },
        &receipt.content_hash,
        &receipt.artifact_locator_hash,
        &receipt.byte_len.to_string(),
    ])
}

fn empty_process_output() -> BoundedProcessOutput {
    let stream = || BoundedOutputStream {
        original_bytes: 0,
        captured_bytes: 0,
        dropped_bytes: 0,
        truncated: false,
        head: None,
        tail: None,
    };
    BoundedProcessOutput {
        stdout: stream(),
        stderr: stream(),
    }
}

enum ProcessTermination {
    Exited(ExitStatus),
    TimedOut,
    Canceled,
    LeaseLost,
    Infrastructure { code: &'static str },
}

fn exited_code(result: &ValidationProcessResult) -> Option<i32> {
    match result {
        ValidationProcessResult::Exited { exit_code } => Some(*exit_code),
        ValidationProcessResult::InfrastructureFailure { .. } => None,
    }
}

fn process_exit_result(status: ExitStatus) -> ValidationProcessResult {
    if let Some(code) = status.code() {
        ValidationProcessResult::Exited { exit_code: code }
    } else {
        ValidationProcessResult::InfrastructureFailure {
            kind: ValidationInfrastructureFailureKind::Transport,
            safe_code: "validation_process_signaled".into(),
        }
    }
}

fn elapsed_ms(started: Instant, timeout_ms: u64) -> u64 {
    u64::try_from(started.elapsed().as_millis())
        .unwrap_or(u64::MAX)
        .min(timeout_ms.saturating_add(5_000))
}

fn environment_fingerprint(entries: &[(String, String)]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"execution-protocol-v1:validation-environment\0");
    for (name, value) in entries {
        digest.update(u64::try_from(name.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(value.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn safe_environment_name(name: &str) -> bool {
    let normalized = name.trim().to_ascii_uppercase();
    !normalized.is_empty()
        && name.trim() == name
        && normalized.len() <= MAX_ENVIRONMENT_NAME_BYTES
        && normalized
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !normalized.starts_with("RUSTGRID_")
        && !normalized.starts_with("GITHUB_")
        && !normalized.starts_with("ACTIONS_")
        && !normalized.starts_with("OPENAI_")
        && !normalized.starts_with("CODEX_")
        && !normalized.starts_with("CHATGPT_")
        && !normalized.starts_with("LD_")
        && !normalized.starts_with("DYLD_")
        && !normalized.starts_with("GIT_CONFIG")
        && !normalized.contains("TOKEN")
        && !normalized.contains("SECRET")
        && !normalized.contains("PASSWORD")
        && !normalized.contains("CREDENTIAL")
        && !normalized.contains("PRIVATE_KEY")
        && !normalized.contains("API_KEY")
        && !matches!(
            normalized.as_str(),
            "SSH_AUTH_SOCK"
                | "SHELL"
                | "ENV"
                | "BASH_ENV"
                | "ZDOTDIR"
                | "CDPATH"
                | "IFS"
                | "PYTHONPATH"
                | "PYTHONHOME"
                | "NODE_OPTIONS"
                | "RUBYOPT"
                | "PERL5OPT"
                | "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "RUSTDOC_WRAPPER"
                | "GIT_EXEC_PATH"
                | "GIT_ASKPASS"
                | "SSH_ASKPASS"
                | "GIT_SSH"
                | "GIT_SSH_COMMAND"
                | "GIT_PROXY_COMMAND"
        )
}

fn safe_code_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn normalized_safe_code(value: &str) -> String {
    if safe_code_is_valid(value) {
        value.into()
    } else {
        "validation_boundary_failure".into()
    }
}

fn append_lossy_segment(output: &mut String, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    match String::from_utf8_lossy(bytes) {
        std::borrow::Cow::Borrowed(value) => output.push_str(value),
        std::borrow::Cow::Owned(mut value) => {
            output.push_str(&value);
            value.zeroize();
        }
    }
}

struct CargoOutputParser;
struct NodeOutputParser;
struct PytestOutputParser;
struct GoOutputParser;
struct GenericOutputParser;

static CARGO_OUTPUT_PARSER: CargoOutputParser = CargoOutputParser;
static NODE_OUTPUT_PARSER: NodeOutputParser = NodeOutputParser;
static PYTEST_OUTPUT_PARSER: PytestOutputParser = PytestOutputParser;
static GO_OUTPUT_PARSER: GoOutputParser = GoOutputParser;
static GENERIC_OUTPUT_PARSER: GenericOutputParser = GenericOutputParser;

fn parser_for(kind: ValidationParserKind) -> &'static dyn ValidationOutputParser {
    match kind {
        ValidationParserKind::Cargo => &CARGO_OUTPUT_PARSER,
        ValidationParserKind::Node => &NODE_OUTPUT_PARSER,
        ValidationParserKind::Pytest => &PYTEST_OUTPUT_PARSER,
        ValidationParserKind::Go => &GO_OUTPUT_PARSER,
        ValidationParserKind::Generic => &GENERIC_OUTPUT_PARSER,
    }
}

macro_rules! validation_parser {
    ($parser:ty, $flavor:expr) => {
        impl ValidationOutputParser for $parser {
            fn parse(
                &self,
                request: &ValidationProcessRequest,
                output: &RetainedValidationOutput,
            ) -> ValidationParserObservation {
                parse_structured_output($flavor, request, output)
            }
        }
    };
}

validation_parser!(CargoOutputParser, ParserFlavor::Cargo);
validation_parser!(NodeOutputParser, ParserFlavor::Node);
validation_parser!(PytestOutputParser, ParserFlavor::Pytest);
validation_parser!(GoOutputParser, ParserFlavor::Go);
validation_parser!(GenericOutputParser, ParserFlavor::Generic);

#[derive(Clone, Copy)]
enum ParserFlavor {
    Cargo,
    Node,
    Pytest,
    Go,
    Generic,
}

fn parse_structured_output(
    flavor: ParserFlavor,
    request: &ValidationProcessRequest,
    output: &RetainedValidationOutput,
) -> ValidationParserObservation {
    let text = strip_ansi(&output.combined_lossy());
    let lines = text.lines().collect::<Vec<_>>();
    let mut diagnostics = Vec::new();
    let mut identities = BTreeSet::new();
    for (line_index, line) in lines.iter().enumerate() {
        let current_location = parse_source_location(flavor, line, &output.path_scope);
        if !failure_line(flavor, line, current_location.is_some()) {
            continue;
        }
        let source_location = current_location
            .or_else(|| nearby_following_location(flavor, &lines, line_index, &output.path_scope));
        let mut test_identity = parse_test_identity(flavor, line);
        let kind = diagnostic_kind(request, test_identity.is_some());
        let safe_summary_code = summary_code(flavor, kind).to_owned();
        let test_identity_hash = test_identity.as_deref().map(|identity| {
            stable_sha256(&["execution-protocol-v1:validation-test-identity", identity])
        });
        if let Some(identity) = &mut test_identity {
            identity.zeroize();
        }
        let (expected_value_hash, actual_value_hash) =
            diagnostic_expected_actual_hashes(flavor, &lines, line_index);
        let safe_summary_hash =
            stable_sha256(&["execution-protocol-v1:validation-diagnostic-line", line]);
        let identity = (
            kind,
            test_identity_hash.clone(),
            source_location.clone(),
            expected_value_hash.clone(),
            actual_value_hash.clone(),
            safe_summary_code.clone(),
            safe_summary_hash.clone(),
        );
        if !identities.insert(identity) {
            continue;
        }
        let implicated_paths = source_location
            .as_ref()
            .map(|location| BTreeSet::from([location.path.clone()]))
            .unwrap_or_default();
        let confidence = if test_identity_hash.is_some() || source_location.is_some() {
            ParserConfidence::Exact
        } else {
            ParserConfidence::Structured
        };
        if let Ok(diagnostic) = ValidationDiagnostic::new(
            kind,
            test_identity_hash,
            source_location,
            expected_value_hash.clone(),
            actual_value_hash.clone(),
            implicated_paths,
            BTreeSet::<EvidenceId>::new(),
            safe_summary_code,
            safe_summary_hash,
            confidence,
        ) {
            diagnostics.push(diagnostic);
        }
        if diagnostics.len() == MAX_PARSED_DIAGNOSTICS {
            break;
        }
    }
    let success_marker = success_marker(flavor, &text);
    let semantics =
        if !diagnostics.is_empty() || success_marker || quiet_completion_has_semantics(request) {
            GateSemanticsObservation::ExpectedSemanticsObserved
        } else {
            GateSemanticsObservation::ExpectedSemanticsMissing
        };
    let confidence = if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.confidence == ParserConfidence::Exact)
    {
        ParserConfidence::Exact
    } else if !diagnostics.is_empty() || success_marker {
        ParserConfidence::Structured
    } else {
        ParserConfidence::Fallback
    };
    (confidence, semantics, diagnostics)
}

fn nearby_following_location(
    flavor: ParserFlavor,
    lines: &[&str],
    failure_index: usize,
    scope: &ValidationParserPathScope,
) -> Option<ValidationSourceLocation> {
    lines
        .iter()
        .skip(failure_index.saturating_add(1))
        .take(3)
        .take_while(|line| !line.trim().is_empty())
        .find_map(|line| parse_source_location(flavor, line, scope))
}

fn diagnostic_expected_actual_hashes(
    flavor: ParserFlavor,
    lines: &[&str],
    failure_index: usize,
) -> (Option<String>, Option<String>) {
    let end = lines
        .iter()
        .enumerate()
        .skip(failure_index.saturating_add(1))
        .take(12)
        .find_map(|(index, line)| failure_line(flavor, line, false).then_some(index))
        .unwrap_or_else(|| lines.len().min(failure_index.saturating_add(12)));
    expected_actual_hashes(&lines[failure_index..end.max(failure_index.saturating_add(1))])
}

fn diagnostic_kind(
    request: &ValidationProcessRequest,
    explicit_test_identity: bool,
) -> ValidationDiagnosticKind {
    let command = command_words(request);
    if explicit_test_identity || is_test_command(&command) {
        ValidationDiagnosticKind::TestAssertion
    } else if command.contains("clippy")
        || command.contains("lint")
        || command.contains("eslint")
        || command.contains("ruff")
        || command.contains("fmt")
    {
        ValidationDiagnosticKind::LintFinding
    } else if command.contains("typecheck")
        || command.contains("tsc")
        || command.contains("mypy")
        || command.contains("pyright")
        || command.contains(" check")
    {
        ValidationDiagnosticKind::TypeError
    } else if command.contains("metadata") {
        ValidationDiagnosticKind::MetadataFailure
    } else {
        ValidationDiagnosticKind::CompileError
    }
}

fn summary_code(flavor: ParserFlavor, kind: ValidationDiagnosticKind) -> &'static str {
    match (flavor, kind) {
        (_, ValidationDiagnosticKind::TestAssertion) => "validation_test_assertion_failed",
        (_, ValidationDiagnosticKind::TypeError) => "validation_typecheck_failed",
        (_, ValidationDiagnosticKind::LintFinding) => "validation_lint_failed",
        (_, ValidationDiagnosticKind::MetadataFailure) => "validation_metadata_failed",
        (ParserFlavor::Cargo, _) => "validation_cargo_compile_failed",
        (ParserFlavor::Node, _) => "validation_node_build_failed",
        (ParserFlavor::Pytest, _) => "validation_python_build_failed",
        (ParserFlavor::Go, _) => "validation_go_build_failed",
        (ParserFlavor::Generic, _) => "validation_command_failed",
    }
}

fn failure_line(flavor: ParserFlavor, line: &str, has_location: bool) -> bool {
    let trimmed = line.trim();
    let lower = Zeroizing::new(trimmed.to_ascii_lowercase());
    if lower.contains("0 failed") || lower.contains("no failures") {
        return false;
    }
    match flavor {
        ParserFlavor::Cargo => {
            (lower.starts_with("test ") && lower.ends_with("... failed"))
                || lower.starts_with("error")
                || lower.contains("panicked at")
                || lower.starts_with("failures:")
        }
        ParserFlavor::Node => {
            lower.starts_with("fail ")
                || lower.starts_with("failed ")
                || lower.contains("assertionerror")
                || lower.contains("error ts")
                || trimmed.starts_with('×')
                || trimmed.starts_with('✕')
        }
        ParserFlavor::Pytest => {
            lower.starts_with("failed ")
                || lower.starts_with("e   assert")
                || lower.contains("assertionerror")
                || lower.starts_with("error ")
        }
        ParserFlavor::Go => {
            lower.starts_with("--- fail:")
                || lower.starts_with("fail\t")
                || lower.as_str() == "fail"
                || (has_location && lower.contains("error"))
        }
        ParserFlavor::Generic => {
            lower.starts_with("error")
                || lower.starts_with("failed")
                || lower.contains("assertion failed")
                || (has_location && lower.contains(" error"))
        }
    }
}

fn success_marker(flavor: ParserFlavor, text: &str) -> bool {
    let lower = Zeroizing::new(text.to_ascii_lowercase());
    match flavor {
        ParserFlavor::Cargo => {
            lower.contains("test result: ok")
                || lower.contains("finished `")
                || lower.contains("finished dev")
                || lower.contains("finished test")
        }
        ParserFlavor::Node => {
            lower
                .lines()
                .any(|line| line.trim_start().starts_with("pass "))
                || lower.contains("tests passed")
                || lower.contains("passed (")
        }
        ParserFlavor::Pytest => lower
            .lines()
            .any(|line| line.contains(" passed") && !line.contains(" 0 passed")),
        ParserFlavor::Go => lower
            .lines()
            .any(|line| line.starts_with("ok\t") || line.starts_with("ok ")),
        ParserFlavor::Generic => lower.contains("success") || lower.contains("passed"),
    }
}

fn quiet_completion_has_semantics(request: &ValidationProcessRequest) -> bool {
    let command = command_words(request);
    !is_test_command(&command)
        && [
            "build",
            "check",
            "typecheck",
            "tsc",
            "lint",
            "clippy",
            "fmt",
            "metadata",
        ]
        .iter()
        .any(|word| command.split_whitespace().any(|part| part.contains(word)))
}

fn command_words(request: &ValidationProcessRequest) -> String {
    std::iter::once(request.command.executable.as_str())
        .chain(request.command.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn is_test_command(command: &str) -> bool {
    command.split_whitespace().any(|part| {
        part == "test"
            || part == "pytest"
            || part.ends_with("/pytest")
            || part == "go" && command.contains("go test")
    })
}

fn expected_actual_hashes(lines: &[&str]) -> (Option<String>, Option<String>) {
    let mut expected = None;
    let mut actual = None;
    for line in lines {
        let trimmed = line.trim().trim_start_matches(['E', '>']).trim();
        let lower = Zeroizing::new(trimmed.to_ascii_lowercase());
        if expected.is_none() {
            expected = value_after_prefix(trimmed, &lower, &["expected:", "right:"])
                .map(|value| stable_sha256(&["execution-protocol-v1:expected-value", value]));
        }
        if actual.is_none() {
            actual = value_after_prefix(trimmed, &lower, &["received:", "actual:", "left:"])
                .map(|value| stable_sha256(&["execution-protocol-v1:actual-value", value]));
        }
        if expected.is_some() && actual.is_some() {
            break;
        }
    }
    (expected, actual)
}

fn value_after_prefix<'a>(original: &'a str, lower: &str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|prefix| {
        lower
            .strip_prefix(prefix)
            .map(|suffix| &original[original.len().saturating_sub(suffix.len())..])
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn parse_test_identity(flavor: ParserFlavor, line: &str) -> Option<String> {
    let trimmed = line.trim();
    match flavor {
        ParserFlavor::Cargo => trimmed
            .strip_prefix("test ")
            .and_then(|value| value.strip_suffix(" ... FAILED"))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        ParserFlavor::Node => trimmed
            .strip_prefix("FAIL ")
            .or_else(|| trimmed.strip_prefix('×'))
            .or_else(|| trimmed.strip_prefix('✕'))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        ParserFlavor::Pytest => trimmed
            .strip_prefix("FAILED ")
            .and_then(|value| value.split_whitespace().next())
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        ParserFlavor::Go => trimmed
            .strip_prefix("--- FAIL:")
            .and_then(|value| value.split_whitespace().next())
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        ParserFlavor::Generic => None,
    }
}

fn parse_source_location(
    flavor: ParserFlavor,
    line: &str,
    scope: &ValidationParserPathScope,
) -> Option<ValidationSourceLocation> {
    let trimmed = line.trim().trim_start_matches("-->").trim();
    for token in trimmed.split_whitespace() {
        let token = token
            .trim_matches(|character: char| matches!(character, '(' | ')' | '[' | ']' | ',' | ';'));
        if let Some(location) = parse_parenthesized_location(token, scope) {
            return Some(location);
        }
        if let Some(location) = parse_colon_location(token, scope) {
            return Some(location);
        }
        if matches!(flavor, ParserFlavor::Pytest)
            && let Some(path) = token.split("::").next().filter(|_| token.contains("::"))
            && let Some(path) = scoped_profile_path(path, scope)
        {
            return Some(ValidationSourceLocation {
                path,
                line: None,
                column: None,
            });
        }
    }
    None
}

fn parse_parenthesized_location(
    token: &str,
    scope: &ValidationParserPathScope,
) -> Option<ValidationSourceLocation> {
    let (path, position) = token.rsplit_once('(')?;
    let position = position.trim_end_matches([')', ':']);
    let (line, column) = position.split_once(',')?;
    Some(ValidationSourceLocation {
        path: scoped_profile_path(path, scope)?,
        line: line.parse::<u32>().ok(),
        column: column.parse::<u32>().ok(),
    })
}

fn parse_colon_location(
    token: &str,
    scope: &ValidationParserPathScope,
) -> Option<ValidationSourceLocation> {
    let token = token.trim_end_matches([':', ')', ',']);
    let mut parts = token.rsplitn(3, ':');
    let last = parts.next()?;
    let middle = parts.next()?;
    let first = parts.next();
    if let Some(path) = first
        && let (Ok(line), Ok(column)) = (middle.parse::<u32>(), last.parse::<u32>())
    {
        return Some(ValidationSourceLocation {
            path: scoped_profile_path(path, scope)?,
            line: Some(line),
            column: Some(column),
        });
    }
    let line = last.parse::<u32>().ok()?;
    Some(ValidationSourceLocation {
        path: scoped_profile_path(middle, scope)?,
        line: Some(line),
        column: None,
    })
}

fn scoped_profile_path(value: &str, scope: &ValidationParserPathScope) -> Option<ProfilePath> {
    let value = Zeroizing::new(value.trim().replace('\\', "/"));
    if value.contains("://") {
        return None;
    }
    let parsed = PathBuf::from(value.as_str());
    let candidates = if parsed.is_absolute() {
        vec![parsed]
    } else if scope.working_directory.is_root() {
        vec![scope.repository_root.join(parsed)]
    } else {
        vec![
            scope.repository_root.join(&parsed),
            scope
                .repository_root
                .join(scope.working_directory.as_str())
                .join(parsed),
        ]
    };
    candidates.into_iter().find_map(|candidate| {
        let canonical = fs::canonicalize(candidate).ok()?;
        if !canonical.is_file() || !canonical.starts_with(&scope.repository_root) {
            return None;
        }
        let relative = canonical.strip_prefix(&scope.repository_root).ok()?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        ProfilePath::new(relative).ok()
    })
}

fn strip_ansi(value: &str) -> Zeroizing<String> {
    let bytes = value.as_bytes();
    let mut retained = Zeroizing::new(Vec::with_capacity(bytes.len()));
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            retained.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        if bytes.get(index) != Some(&b'[') {
            continue;
        }
        index += 1;
        while index < bytes.len() {
            let control = bytes[index];
            index += 1;
            if (0x40..=0x7e).contains(&control) {
                break;
            }
        }
    }
    Zeroizing::new(String::from_utf8_lossy(&retained).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_protocol::{
        AuthorizedValidationCommand, EffectId, ExecutionId, NodeId, RepositoryRevisionId,
        VALIDATION_SCHEMA_VERSION, ValidationGateId, ValidationPolicyId, ValidationRunId,
        ValidationRunKind, ValidationRunSchedule,
    };

    struct EmptyEnvironmentSource;

    struct AlwaysAuthorized;

    struct LeaseLostBeforeSpawn;

    impl ValidationProcessAuthority for AlwaysAuthorized {
        fn validate_before_spawn(
            &self,
            _request: &ValidationProcessRequest,
            _canonical_repository_root: &Path,
        ) -> Result<ValidationProcessAuthorityState, ValidationBoundaryError> {
            Ok(ValidationProcessAuthorityState::Authorized)
        }

        fn current_state(
            &self,
            _request: &ValidationProcessRequest,
        ) -> ValidationProcessAuthorityState {
            ValidationProcessAuthorityState::Authorized
        }
    }

    impl ValidationProcessAuthority for LeaseLostBeforeSpawn {
        fn validate_before_spawn(
            &self,
            _request: &ValidationProcessRequest,
            _canonical_repository_root: &Path,
        ) -> Result<ValidationProcessAuthorityState, ValidationBoundaryError> {
            Ok(ValidationProcessAuthorityState::LeaseLost)
        }

        fn current_state(
            &self,
            _request: &ValidationProcessRequest,
        ) -> ValidationProcessAuthorityState {
            ValidationProcessAuthorityState::LeaseLost
        }
    }

    impl ValidationEnvironmentSource for EmptyEnvironmentSource {
        fn load(
            &self,
            _request: &ValidationProcessRequest,
        ) -> Result<ValidationProcessEnvironment, ValidationBoundaryError> {
            Ok(ValidationProcessEnvironment::empty())
        }
    }

    #[derive(Default)]
    struct RecordingArtifactSink {
        chunks: Vec<(ValidationOutputStreamKind, ValidationOutputSegment, Vec<u8>)>,
    }

    impl ValidationArtifactSink for RecordingArtifactSink {
        fn persist(
            &mut self,
            request: &ValidationProcessRequest,
            stream: ValidationOutputStreamKind,
            segment: ValidationOutputSegment,
            bytes: &[u8],
        ) -> Result<ValidationArtifactReceipt, ValidationBoundaryError> {
            let content_hash = hex::encode(Sha256::digest(bytes));
            let artifact_locator_hash = stable_sha256(&[
                "validation-test-artifact-locator",
                request.schedule.run_id.as_str(),
                match stream {
                    ValidationOutputStreamKind::Stdout => "stdout",
                    ValidationOutputStreamKind::Stderr => "stderr",
                },
                match segment {
                    ValidationOutputSegment::Head => "head",
                    ValidationOutputSegment::Tail => "tail",
                },
            ]);
            self.chunks.push((stream, segment, bytes.to_vec()));
            let mut receipt = ValidationArtifactReceipt {
                content_hash,
                artifact_locator_hash,
                persistence_receipt_hash: String::new(),
                byte_len: u64::try_from(bytes.len()).unwrap(),
            };
            receipt.persistence_receipt_hash =
                expected_artifact_persistence_receipt(request, stream, segment, &receipt);
            Ok(receipt)
        }
    }

    #[derive(Default)]
    struct RecordingObservationSink {
        order: Vec<&'static str>,
        started: Option<ValidationProcessStarted>,
        completed: Option<ValidationProcessCompleted>,
    }

    impl ValidationObservationSink for RecordingObservationSink {
        fn record_started(
            &mut self,
            started: &ValidationProcessStarted,
        ) -> ValidationObservationWrite {
            self.order.push("started");
            self.started = Some(started.clone());
            ValidationObservationWrite::Recorded
        }

        fn record_completed(
            &mut self,
            completed: &ValidationProcessCompleted,
        ) -> ValidationObservationWrite {
            self.order.push("completed");
            self.completed = Some(completed.clone());
            ValidationObservationWrite::Recorded
        }
    }

    fn process_request(
        executable: &Path,
        args: &[&str],
        timeout_ms: u64,
        output_limit_bytes: u64,
    ) -> ValidationProcessRequest {
        let environment_fingerprint = ValidationProcessEnvironment::empty()
            .fingerprint()
            .to_owned();
        ValidationProcessRequest {
            schema_version: VALIDATION_SCHEMA_VERSION,
            schedule: ValidationRunSchedule {
                schema_version: VALIDATION_SCHEMA_VERSION,
                run_id: ValidationRunId::new("validation-test-run"),
                execution_id: ExecutionId::new("validation-test-execution"),
                execution_attempt: 1,
                gate_id: ValidationGateId::new("validation-test-gate"),
                node_id: NodeId::new("validation-test-node"),
                node_attempt: 1,
                repository_revision: RepositoryRevisionId::new("validation-test-revision"),
                run_attempt: 1,
                kind: ValidationRunKind::Initial,
                effect_id: EffectId::new("validation-test-effect"),
            },
            policy_id: ValidationPolicyId::new("validation-test-policy"),
            command: AuthorizedValidationCommand {
                command_id: EvidenceId::new("validation-test-command"),
                candidate_id: EvidenceId::new("validation-test-candidate"),
                executable: executable.to_string_lossy().into_owned(),
                args: args.iter().map(|value| (*value).into()).collect(),
                working_directory: ProfilePath::root(),
                environment_fingerprint,
                dependency_fingerprint: "0".repeat(64),
            },
            parser: ValidationParserKind::Generic,
            timeout_ms,
            output_limit_bytes,
            payload_hash: "0".repeat(64),
        }
    }

    #[cfg(unix)]
    fn executable(candidates: &[&str]) -> PathBuf {
        candidates
            .iter()
            .map(PathBuf::from)
            .find(|candidate| candidate.is_file())
            .expect("standard Unix validation test executable exists")
    }

    #[test]
    fn environment_is_sorted_fingerprinted_and_redacted() {
        let environment = ValidationProcessEnvironment::new(vec![
            ("PATH".into(), "/safe/bin".into()),
            ("lang".into(), "C.UTF-8".into()),
        ])
        .unwrap();

        assert_eq!(environment.names().collect::<Vec<_>>(), ["PATH", "lang"]);
        let debug = format!("{environment:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("/safe/bin"));
        assert!(!debug.contains("C.UTF-8"));

        let reordered = ValidationProcessEnvironment::new(vec![
            ("lang".into(), "C.UTF-8".into()),
            ("PATH".into(), "/safe/bin".into()),
        ])
        .unwrap();
        assert_eq!(environment.fingerprint(), reordered.fingerprint());
    }

    #[test]
    fn environment_rejects_secret_and_process_injection_names() {
        for name in [
            "GITHUB_TOKEN",
            "OPENAI_API_KEY",
            "LD_PRELOAD",
            "RUSTC_WRAPPER",
            "BASH_ENV",
        ] {
            assert!(
                ValidationProcessEnvironment::new(vec![(name.into(), "value".into())]).is_err(),
                "accepted {name}"
            );
        }
    }

    #[test]
    fn retained_output_keeps_failure_tail_and_exact_counts() {
        let mut capture = RawStreamCapture {
            original_bytes: 0,
            head: Vec::new(),
            tail: VecDeque::new(),
        };
        capture.push(b"abcdefghij", 2, 3);
        let retained = capture.retain(5).unwrap();

        assert_eq!(retained.original_bytes, 10);
        assert_eq!(retained.head, b"ab");
        assert_eq!(retained.tail, b"hij");
        assert_eq!(retained.captured_bytes(), 5);
        assert!(retained.truncated);
    }

    #[test]
    fn combined_limit_reserves_a_byte_for_each_nonempty_stream() {
        assert_eq!(allocate_stream_limits(100, 100, 2), Ok((1, 1)));
        assert_eq!(
            allocate_stream_limits(100, 100, 1),
            Err("validation_output_limit_cannot_represent_streams")
        );
        assert_eq!(allocate_stream_limits(10, 0, 5), Ok((5, 0)));
    }

    #[test]
    fn pre_spawn_cancellation_records_a_startless_completion() {
        let request = process_request(Path::new("not-invoked"), &[], 2_000, 1_024);
        let mut artifacts = RecordingArtifactSink::default();
        let mut observations = RecordingObservationSink::default();

        let outcome = run_validation_process(
            &request,
            Path::new("not-inspected"),
            &AlwaysAuthorized,
            &EmptyEnvironmentSource,
            &mut artifacts,
            &mut observations,
            &AtomicBool::new(true),
        )
        .unwrap();

        assert_eq!(observations.order, ["completed"]);
        assert!(outcome.started.is_none());
        assert!(matches!(
            outcome.completed.result,
            ValidationProcessResult::InfrastructureFailure {
                kind: ValidationInfrastructureFailureKind::Canceled,
                ..
            }
        ));
        assert!(artifacts.chunks.is_empty());
    }

    #[test]
    fn pre_spawn_lease_loss_records_a_startless_completion() {
        let root = tempfile::tempdir().unwrap();
        let request = process_request(Path::new("not-invoked"), &[], 2_000, 1_024);
        let mut artifacts = RecordingArtifactSink::default();
        let mut observations = RecordingObservationSink::default();

        let outcome = run_validation_process(
            &request,
            root.path(),
            &LeaseLostBeforeSpawn,
            &EmptyEnvironmentSource,
            &mut artifacts,
            &mut observations,
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(observations.order, ["completed"]);
        assert!(outcome.started.is_none());
        assert!(matches!(
            outcome.completed.result,
            ValidationProcessResult::InfrastructureFailure {
                kind: ValidationInfrastructureFailureKind::LeaseLost,
                ..
            }
        ));
        assert!(artifacts.chunks.is_empty());
    }

    #[test]
    fn parser_scopes_absolute_paths_and_expected_actual_values_per_failure() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        let first = root.path().join("src/first.rs");
        let second = root.path().join("src/second.rs");
        fs::write(&first, "first").unwrap();
        fs::write(&second, "second").unwrap();
        let raw = format!(
            "error: first failure\n --> {}:4:2\n expected: one\n actual: two\nerror: second failure\n --> {}:8:3\n expected: three\n actual: four\n",
            first.display(),
            second.display()
        )
        .into_bytes();
        let output = RetainedValidationOutput {
            stdout: RetainedOutputStream {
                original_bytes: u64::try_from(raw.len()).unwrap(),
                head: raw,
                tail: Vec::new(),
                truncated: false,
            },
            stderr: RetainedOutputStream {
                original_bytes: 0,
                head: Vec::new(),
                tail: Vec::new(),
                truncated: false,
            },
            path_scope: ValidationParserPathScope {
                repository_root: fs::canonicalize(root.path()).unwrap(),
                working_directory: ProfilePath::root(),
            },
        };
        let mut request = process_request(Path::new("cargo"), &["check"], 2_000, 1_024);
        request.parser = ValidationParserKind::Cargo;

        let (_, _, diagnostics) = CARGO_OUTPUT_PARSER.parse(&request, &output);

        assert_eq!(diagnostics.len(), 2);
        assert_eq!(
            diagnostics[0]
                .source_location
                .as_ref()
                .map(|location| location.path.as_str()),
            Some("src/first.rs")
        );
        assert_eq!(
            diagnostics[1]
                .source_location
                .as_ref()
                .map(|location| location.path.as_str()),
            Some("src/second.rs")
        );
        assert_ne!(
            diagnostics[0].expected_value_hash,
            diagnostics[1].expected_value_hash
        );
        assert_ne!(
            diagnostics[0].actual_value_hash,
            diagnostics[1].actual_value_hash
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_nonzero_exit_is_a_domain_result_with_ordered_observations() {
        let root = tempfile::tempdir().unwrap();
        let program = executable(&["/usr/bin/false", "/bin/false"]);
        let request = process_request(&program, &[], 2_000, 1_024);
        let mut artifacts = RecordingArtifactSink::default();
        let mut observations = RecordingObservationSink::default();

        let outcome = run_validation_process(
            &request,
            root.path(),
            &AlwaysAuthorized,
            &EmptyEnvironmentSource,
            &mut artifacts,
            &mut observations,
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(observations.order, ["started", "completed"]);
        assert!(matches!(
            outcome.completed.result,
            ValidationProcessResult::Exited { exit_code } if exit_code != 0
        ));
        assert_eq!(outcome.started, observations.started);
        assert_eq!(Some(outcome.completed.clone()), observations.completed);
        assert_eq!(outcome.parser_confidence, Some(ParserConfidence::Fallback));
        assert_eq!(outcome.diagnostics.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn real_large_output_preserves_bounded_head_and_tail_on_timeout() {
        let root = tempfile::tempdir().unwrap();
        let program = executable(&["/usr/bin/yes", "/bin/yes"]);
        let request = process_request(&program, &["validation-tail-marker"], 50, 256);
        let mut artifacts = RecordingArtifactSink::default();
        let mut observations = RecordingObservationSink::default();

        let outcome = run_validation_process(
            &request,
            root.path(),
            &AlwaysAuthorized,
            &EmptyEnvironmentSource,
            &mut artifacts,
            &mut observations,
            &AtomicBool::new(false),
        )
        .unwrap();

        assert!(matches!(
            outcome.completed.result,
            ValidationProcessResult::InfrastructureFailure {
                kind: ValidationInfrastructureFailureKind::Timeout,
                ..
            }
        ));
        let stdout = &outcome.completed.output.stdout;
        assert!(stdout.original_bytes > stdout.captured_bytes);
        assert_eq!(stdout.captured_bytes, 256);
        assert!(stdout.truncated);
        assert!(stdout.head.is_some());
        assert!(stdout.tail.is_some());
        assert_eq!(observations.order, ["started", "completed"]);
        assert!(artifacts.chunks.iter().any(|(stream, segment, bytes)| {
            *stream == ValidationOutputStreamKind::Stdout
                && *segment == ValidationOutputSegment::Tail
                && !bytes.is_empty()
        }));
    }
}
