//! Strict, content-addressed fixture schemas for Protocol v1 conformance.
//!
//! This module only loads checked-in data. It never invokes a provider, runs a
//! repository command, mutates a checkout, or contacts Git/GitHub.
//!
//! The initial checked-in fixture is intentionally a schema-foundation fixture:
//! its expected event file is a checkpoint summary, not a claim that it contains
//! every reducer event. Full canonical reducer traces use a future trace kind and
//! must be produced from an actual `ProtocolEventEnvelope` stream.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs, io,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::stable_sha256;

const FIXTURE_SCHEMA_VERSION: u16 = 1;
const PROVIDER_SCRIPT_SCHEMA_VERSION: u16 = 1;
const EXPECTED_EVENTS_SCHEMA_VERSION: u16 = 1;
const EXPECTED_RESULT_SCHEMA_VERSION: u16 = 1;
const MAX_FIXTURE_ARTIFACT_BYTES: u64 = 1024 * 1024;
const MAX_REPOSITORY_FILES: usize = 256;
const MAX_REPOSITORY_BYTES: u64 = 4 * 1024 * 1024;

const MANIFEST_NAME: &str = "fixture.toml";
const REPOSITORY_PATH: &str = "repository";
const PROVIDER_SCRIPT_PATH: &str = "provider-script.json";
const EXPECTED_EVENTS_PATH: &str = "expected-events.json";
const EXPECTED_RESULT_PATH: &str = "expected-result.json";

#[derive(Debug)]
pub(crate) enum CanonicalFixtureError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    ArtifactTooLarge {
        path: PathBuf,
        observed: u64,
        maximum: u64,
    },
    ManifestSyntax {
        line: usize,
        detail: String,
    },
    Json {
        artifact: &'static str,
        source: serde_json::Error,
    },
    SemanticEncoding {
        artifact: &'static str,
        source: serde_json::Error,
    },
    Invalid {
        artifact: &'static str,
        field: &'static str,
        detail: String,
    },
    SemanticHashMismatch {
        artifact: &'static str,
        expected: String,
        observed: String,
    },
}

impl fmt::Display for CanonicalFixtureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "could not read `{}`: {source}", path.display())
            }
            Self::ArtifactTooLarge {
                path,
                observed,
                maximum,
            } => write!(
                formatter,
                "fixture artifact `{}` has {observed} bytes; maximum is {maximum}",
                path.display()
            ),
            Self::ManifestSyntax { line, detail } => {
                write!(formatter, "invalid fixture.toml at line {line}: {detail}")
            }
            Self::Json { artifact, source } => {
                write!(formatter, "invalid {artifact}: {source}")
            }
            Self::SemanticEncoding { artifact, source } => {
                write!(formatter, "could not encode {artifact} semantics: {source}")
            }
            Self::Invalid {
                artifact,
                field,
                detail,
            } => write!(formatter, "invalid {artifact} field `{field}`: {detail}"),
            Self::SemanticHashMismatch {
                artifact,
                expected,
                observed,
            } => write!(
                formatter,
                "{artifact} semantic hash mismatch: expected `{expected}`, observed `{observed}`"
            ),
        }
    }
}

impl std::error::Error for CanonicalFixtureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json { source, .. } | Self::SemanticEncoding { source, .. } => Some(source),
            Self::ArtifactTooLarge { .. }
            | Self::ManifestSyntax { .. }
            | Self::Invalid { .. }
            | Self::SemanticHashMismatch { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CanonicalFixtureV1 {
    pub(crate) root: PathBuf,
    pub(crate) manifest: CanonicalFixtureManifestV1,
    pub(crate) provider_script: ProviderScriptV1,
    pub(crate) expected_events: ExpectedEventsV1,
    pub(crate) expected_result: ExpectedResultV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CanonicalFixtureManifestV1 {
    pub(crate) schema_version: u16,
    pub(crate) fixture_id: String,
    pub(crate) fixture_scope: FixtureScopeV1,
    pub(crate) description: String,
    pub(crate) mission: String,
    pub(crate) repository_path: String,
    pub(crate) expected_ecosystem: ExpectedEcosystemV1,
    pub(crate) expected_generated_path_count: u32,
    pub(crate) max_model_calls: u32,
    pub(crate) max_input_tokens_per_call: u32,
    pub(crate) max_output_tokens_per_call: u32,
    pub(crate) provider_script_path: String,
    pub(crate) expected_events_path: String,
    pub(crate) expected_result_path: String,
    pub(crate) repository_tree_hash: String,
    pub(crate) semantic_hash: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FixtureScopeV1 {
    SchemaFoundation,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExpectedEcosystemV1 {
    Rust,
    Node,
    Python,
    Go,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderScriptV1 {
    pub(crate) schema_version: u16,
    pub(crate) steps: Vec<ProviderScriptStepV1>,
    pub(crate) semantic_hash: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderScriptStepV1 {
    pub(crate) ordinal: u32,
    pub(crate) action_class: FixtureActionClassV1,
    pub(crate) tool_name: String,
    pub(crate) response: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FixtureActionClassV1 {
    Discovery,
    Planning,
    Mutation,
    Review,
    Completion,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedEventsV1 {
    pub(crate) schema_version: u16,
    pub(crate) trace_kind: ExpectedTraceKindV1,
    pub(crate) events: Vec<ExpectedEventV1>,
    pub(crate) semantic_hash: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExpectedTraceKindV1 {
    CheckpointSummary,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedEventV1 {
    pub(crate) sequence: u64,
    pub(crate) family: ExpectedEventFamilyV1,
    pub(crate) event_type: String,
    pub(crate) semantic_key: String,
    pub(crate) semantic_fields: BTreeMap<String, serde_json::Value>,
    pub(crate) semantic_hash: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExpectedEventFamilyV1 {
    Profile,
    Discovery,
    Planning,
    Implementation,
    Mutation,
    Validation,
    Review,
    Publication,
    Evidence,
    Graph,
    Budget,
    Lifecycle,
    Terminal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedResultV1 {
    pub(crate) schema_version: u16,
    pub(crate) outcome: ExpectedOutcomeV1,
    pub(crate) process_health: ExpectedProcessHealthV1,
    pub(crate) reason_code: String,
    pub(crate) remaining_work: Vec<String>,
    pub(crate) semantic_fields: BTreeMap<String, serde_json::Value>,
    pub(crate) semantic_hash: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExpectedOutcomeV1 {
    Succeeded,
    SucceededNoOp,
    PartialReviewable,
    BlockedNoDiff,
    NoValidRepair,
    InsufficientEvidence,
    ValidationFailed,
    BudgetBlocked,
    InfrastructureFailed,
    PublicationFailed,
    Canceled,
}

impl ExpectedOutcomeV1 {
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::SucceededNoOp => "succeeded_no_op",
            Self::PartialReviewable => "partial_reviewable",
            Self::BlockedNoDiff => "blocked_no_diff",
            Self::NoValidRepair => "no_valid_repair",
            Self::InsufficientEvidence => "insufficient_evidence",
            Self::ValidationFailed => "validation_failed",
            Self::BudgetBlocked => "budget_blocked",
            Self::InfrastructureFailed => "infrastructure_failed",
            Self::PublicationFailed => "publication_failed",
            Self::Canceled => "canceled",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExpectedProcessHealthV1 {
    Healthy,
    Degraded,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
struct ManifestSemanticView<'a> {
    schema_version: u16,
    fixture_id: &'a str,
    fixture_scope: FixtureScopeV1,
    description: &'a str,
    mission: &'a str,
    repository_path: &'a str,
    expected_ecosystem: ExpectedEcosystemV1,
    expected_generated_path_count: u32,
    max_model_calls: u32,
    max_input_tokens_per_call: u32,
    max_output_tokens_per_call: u32,
    provider_script_path: &'a str,
    expected_events_path: &'a str,
    expected_result_path: &'a str,
    repository_tree_hash: &'a str,
    provider_script_semantic_hash: &'a str,
    expected_events_semantic_hash: &'a str,
    expected_result_semantic_hash: &'a str,
}

pub(crate) fn load_canonical_fixture(
    root: impl AsRef<Path>,
) -> Result<CanonicalFixtureV1, CanonicalFixtureError> {
    let root = root.as_ref();
    if !root.is_dir() {
        return Err(CanonicalFixtureError::Invalid {
            artifact: "fixture root",
            field: "path",
            detail: format!("`{}` is not a directory", root.display()),
        });
    }

    let manifest_bytes = read_limited(&root.join(MANIFEST_NAME))?;
    let manifest_text =
        std::str::from_utf8(&manifest_bytes).map_err(|_| CanonicalFixtureError::Invalid {
            artifact: MANIFEST_NAME,
            field: "encoding",
            detail: "must be UTF-8".into(),
        })?;
    let manifest = parse_manifest(manifest_text)?;
    validate_manifest(&manifest)?;

    let repository_path = resolve_member(root, &manifest.repository_path, "repository_path")?;
    if !repository_path.is_dir() {
        return Err(CanonicalFixtureError::Invalid {
            artifact: MANIFEST_NAME,
            field: "repository_path",
            detail: "must name a directory".into(),
        });
    }
    let observed_repository_hash = hash_repository_tree(&repository_path)?;
    verify_hash(
        "repository tree",
        &manifest.repository_tree_hash,
        &observed_repository_hash,
    )?;

    let provider_script: ProviderScriptV1 =
        read_json(root, &manifest.provider_script_path, "provider-script.json")?;
    validate_provider_script(&provider_script, manifest.max_model_calls)?;
    verify_hash(
        "provider script",
        &provider_script.semantic_hash,
        &provider_script.expected_semantic_hash()?,
    )?;

    let expected_events: ExpectedEventsV1 =
        read_json(root, &manifest.expected_events_path, "expected-events.json")?;
    validate_expected_events(&expected_events)?;
    verify_hash(
        "expected events",
        &expected_events.semantic_hash,
        &expected_events.expected_semantic_hash()?,
    )?;

    let expected_result: ExpectedResultV1 =
        read_json(root, &manifest.expected_result_path, "expected-result.json")?;
    validate_expected_result(&expected_result)?;
    verify_hash(
        "expected result",
        &expected_result.semantic_hash,
        &expected_result.expected_semantic_hash()?,
    )?;
    validate_terminal_binding(&expected_events, &expected_result)?;

    let observed_fixture_hash = manifest.expected_semantic_hash(
        &provider_script.semantic_hash,
        &expected_events.semantic_hash,
        &expected_result.semantic_hash,
    )?;
    verify_hash(
        "canonical fixture",
        &manifest.semantic_hash,
        &observed_fixture_hash,
    )?;

    Ok(CanonicalFixtureV1 {
        root: root.to_path_buf(),
        manifest,
        provider_script,
        expected_events,
        expected_result,
    })
}

impl CanonicalFixtureManifestV1 {
    fn expected_semantic_hash(
        &self,
        provider_script_semantic_hash: &str,
        expected_events_semantic_hash: &str,
        expected_result_semantic_hash: &str,
    ) -> Result<String, CanonicalFixtureError> {
        semantic_hash(
            "execution-protocol-v1:canonical-fixture",
            "canonical fixture",
            &ManifestSemanticView {
                schema_version: self.schema_version,
                fixture_id: &self.fixture_id,
                fixture_scope: self.fixture_scope,
                description: &self.description,
                mission: &self.mission,
                repository_path: &self.repository_path,
                expected_ecosystem: self.expected_ecosystem,
                expected_generated_path_count: self.expected_generated_path_count,
                max_model_calls: self.max_model_calls,
                max_input_tokens_per_call: self.max_input_tokens_per_call,
                max_output_tokens_per_call: self.max_output_tokens_per_call,
                provider_script_path: &self.provider_script_path,
                expected_events_path: &self.expected_events_path,
                expected_result_path: &self.expected_result_path,
                repository_tree_hash: &self.repository_tree_hash,
                provider_script_semantic_hash,
                expected_events_semantic_hash,
                expected_result_semantic_hash,
            },
        )
    }
}

impl ProviderScriptV1 {
    fn expected_semantic_hash(&self) -> Result<String, CanonicalFixtureError> {
        semantic_hash(
            "execution-protocol-v1:fixture-provider-script",
            "provider script",
            &(self.schema_version, &self.steps),
        )
    }
}

impl ExpectedEventV1 {
    fn expected_semantic_hash(&self) -> Result<String, CanonicalFixtureError> {
        semantic_hash(
            "execution-protocol-v1:fixture-expected-event",
            "expected event",
            &(
                self.sequence,
                self.family,
                &self.event_type,
                &self.semantic_key,
                &self.semantic_fields,
            ),
        )
    }
}

impl ExpectedEventsV1 {
    fn expected_semantic_hash(&self) -> Result<String, CanonicalFixtureError> {
        semantic_hash(
            "execution-protocol-v1:fixture-expected-events",
            "expected events",
            &(self.schema_version, self.trace_kind, &self.events),
        )
    }
}

impl ExpectedResultV1 {
    fn expected_semantic_hash(&self) -> Result<String, CanonicalFixtureError> {
        semantic_hash(
            "execution-protocol-v1:fixture-expected-result",
            "expected result",
            &(
                self.schema_version,
                self.outcome,
                self.process_health,
                &self.reason_code,
                &self.remaining_work,
                &self.semantic_fields,
            ),
        )
    }
}

fn semantic_hash<T: Serialize>(
    namespace: &'static str,
    artifact: &'static str,
    value: &T,
) -> Result<String, CanonicalFixtureError> {
    let encoded = serde_json::to_string(value)
        .map_err(|source| CanonicalFixtureError::SemanticEncoding { artifact, source })?;
    Ok(stable_sha256(&[namespace, &encoded]))
}

fn verify_hash(
    artifact: &'static str,
    expected: &str,
    observed: &str,
) -> Result<(), CanonicalFixtureError> {
    if !is_sha256(expected) {
        return Err(CanonicalFixtureError::Invalid {
            artifact,
            field: "semantic_hash",
            detail: "must be a lowercase 64-character SHA-256 hex digest".into(),
        });
    }
    if expected != observed {
        return Err(CanonicalFixtureError::SemanticHashMismatch {
            artifact,
            expected: expected.into(),
            observed: observed.into(),
        });
    }
    Ok(())
}

fn validate_manifest(manifest: &CanonicalFixtureManifestV1) -> Result<(), CanonicalFixtureError> {
    if manifest.schema_version != FIXTURE_SCHEMA_VERSION {
        return invalid(
            MANIFEST_NAME,
            "schema_version",
            format!(
                "observed {}; expected {FIXTURE_SCHEMA_VERSION}",
                manifest.schema_version
            ),
        );
    }
    validate_slug(MANIFEST_NAME, "fixture_id", &manifest.fixture_id)?;
    validate_text(MANIFEST_NAME, "description", &manifest.description, 256)?;
    validate_text(MANIFEST_NAME, "mission", &manifest.mission, 4_096)?;
    for (field, observed, expected) in [
        (
            "repository_path",
            &manifest.repository_path,
            REPOSITORY_PATH,
        ),
        (
            "provider_script_path",
            &manifest.provider_script_path,
            PROVIDER_SCRIPT_PATH,
        ),
        (
            "expected_events_path",
            &manifest.expected_events_path,
            EXPECTED_EVENTS_PATH,
        ),
        (
            "expected_result_path",
            &manifest.expected_result_path,
            EXPECTED_RESULT_PATH,
        ),
    ] {
        validate_relative_path(MANIFEST_NAME, field, observed)?;
        if observed != expected {
            return invalid(
                MANIFEST_NAME,
                field,
                format!("canonical fixture path must be `{expected}`"),
            );
        }
    }
    if manifest.max_model_calls == 0 || manifest.max_model_calls > 32 {
        return invalid(MANIFEST_NAME, "max_model_calls", "must be in 1..=32");
    }
    for (field, value) in [
        (
            "max_input_tokens_per_call",
            manifest.max_input_tokens_per_call,
        ),
        (
            "max_output_tokens_per_call",
            manifest.max_output_tokens_per_call,
        ),
    ] {
        if value == 0 || value > 1_000_000 {
            return invalid(MANIFEST_NAME, field, "must be in 1..=1000000");
        }
    }
    for (field, digest) in [
        ("repository_tree_hash", &manifest.repository_tree_hash),
        ("semantic_hash", &manifest.semantic_hash),
    ] {
        if !is_sha256(digest) {
            return invalid(
                MANIFEST_NAME,
                field,
                "must be a lowercase 64-character SHA-256 hex digest",
            );
        }
    }
    Ok(())
}

fn validate_provider_script(
    script: &ProviderScriptV1,
    max_model_calls: u32,
) -> Result<(), CanonicalFixtureError> {
    if script.schema_version != PROVIDER_SCRIPT_SCHEMA_VERSION {
        return invalid(
            "provider script",
            "schema_version",
            format!(
                "observed {}; expected {PROVIDER_SCRIPT_SCHEMA_VERSION}",
                script.schema_version
            ),
        );
    }
    if script.steps.is_empty()
        || u32::try_from(script.steps.len()).unwrap_or(u32::MAX) > max_model_calls
    {
        return invalid(
            "provider script",
            "steps",
            "must be non-empty and fit the signed model-call budget",
        );
    }
    for (expected, step) in script.steps.iter().enumerate() {
        if usize::try_from(step.ordinal).ok() != Some(expected) {
            return invalid(
                "provider script",
                "steps.ordinal",
                "must be contiguous and zero-based",
            );
        }
        validate_slug("provider script", "steps.tool_name", &step.tool_name)?;
        if step.response.is_empty() {
            return invalid(
                "provider script",
                "steps.response",
                "must be a non-empty JSON object",
            );
        }
    }
    Ok(())
}

fn validate_expected_events(events: &ExpectedEventsV1) -> Result<(), CanonicalFixtureError> {
    if events.schema_version != EXPECTED_EVENTS_SCHEMA_VERSION {
        return invalid(
            "expected events",
            "schema_version",
            format!(
                "observed {}; expected {EXPECTED_EVENTS_SCHEMA_VERSION}",
                events.schema_version
            ),
        );
    }
    if events.events.is_empty() {
        return invalid("expected events", "events", "must not be empty");
    }
    let mut semantic_keys = BTreeSet::new();
    for (index, event) in events.events.iter().enumerate() {
        let expected_sequence = u64::try_from(index)
            .expect("bounded fixture event index fits u64")
            .saturating_add(1);
        if event.sequence != expected_sequence {
            return invalid(
                "expected events",
                "events.sequence",
                "must be contiguous and one-based like ProtocolEventEnvelope.sequence",
            );
        }
        validate_slug("expected events", "events.event_type", &event.event_type)?;
        validate_semantic_key(&event.semantic_key)?;
        if !semantic_keys.insert(&event.semantic_key) {
            return invalid("expected events", "events.semantic_key", "must be unique");
        }
        if event.semantic_fields.is_empty() {
            return invalid(
                "expected events",
                "events.semantic_fields",
                "must not be empty",
            );
        }
        verify_hash(
            "expected event",
            &event.semantic_hash,
            &event.expected_semantic_hash()?,
        )?;
    }
    Ok(())
}

fn validate_expected_result(result: &ExpectedResultV1) -> Result<(), CanonicalFixtureError> {
    if result.schema_version != EXPECTED_RESULT_SCHEMA_VERSION {
        return invalid(
            "expected result",
            "schema_version",
            format!(
                "observed {}; expected {EXPECTED_RESULT_SCHEMA_VERSION}",
                result.schema_version
            ),
        );
    }
    validate_slug("expected result", "reason_code", &result.reason_code)?;
    let mut work = BTreeSet::new();
    for item in &result.remaining_work {
        validate_text("expected result", "remaining_work", item, 1_024)?;
        if !work.insert(item) {
            return invalid(
                "expected result",
                "remaining_work",
                "must be strictly unique",
            );
        }
    }
    if matches!(
        result.outcome,
        ExpectedOutcomeV1::Succeeded | ExpectedOutcomeV1::SucceededNoOp
    ) && !result.remaining_work.is_empty()
    {
        return invalid(
            "expected result",
            "remaining_work",
            "successful completion cannot retain work",
        );
    }
    if result.semantic_fields.is_empty() {
        return invalid("expected result", "semantic_fields", "must not be empty");
    }
    Ok(())
}

fn validate_terminal_binding(
    events: &ExpectedEventsV1,
    result: &ExpectedResultV1,
) -> Result<(), CanonicalFixtureError> {
    let terminal = events
        .events
        .last()
        .expect("expected-event validation rejects an empty trace");
    if terminal.family != ExpectedEventFamilyV1::Terminal
        || terminal.event_type != "canonical_result_recorded"
    {
        return invalid(
            "expected events",
            "events",
            "last event must be terminal canonical_result_recorded",
        );
    }
    let observed_outcome = terminal
        .semantic_fields
        .get("outcome")
        .and_then(serde_json::Value::as_str);
    if observed_outcome != Some(result.outcome.as_str()) {
        return invalid(
            "expected events",
            "events[-1].semantic_fields.outcome",
            "must equal expected-result outcome",
        );
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(
    root: &Path,
    relative: &str,
    artifact: &'static str,
) -> Result<T, CanonicalFixtureError> {
    let path = resolve_member(root, relative, artifact)?;
    let bytes = read_limited(&path)?;
    serde_json::from_slice(&bytes)
        .map_err(|source| CanonicalFixtureError::Json { artifact, source })
}

fn read_limited(path: &Path) -> Result<Vec<u8>, CanonicalFixtureError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| CanonicalFixtureError::Io {
        path: path.into(),
        source,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return invalid(
            "fixture artifact",
            "path",
            format!("`{}` must be a regular non-symlink file", path.display()),
        );
    }
    if metadata.len() > MAX_FIXTURE_ARTIFACT_BYTES {
        return Err(CanonicalFixtureError::ArtifactTooLarge {
            path: path.into(),
            observed: metadata.len(),
            maximum: MAX_FIXTURE_ARTIFACT_BYTES,
        });
    }
    fs::read(path).map_err(|source| CanonicalFixtureError::Io {
        path: path.into(),
        source,
    })
}

fn resolve_member(
    root: &Path,
    relative: &str,
    field: &'static str,
) -> Result<PathBuf, CanonicalFixtureError> {
    validate_relative_path(MANIFEST_NAME, field, relative)?;
    Ok(root.join(relative))
}

fn validate_relative_path(
    artifact: &'static str,
    field: &'static str,
    value: &str,
) -> Result<(), CanonicalFixtureError> {
    if value.is_empty()
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || Path::new(value)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return invalid(
            artifact,
            field,
            "must be a normalized relative path without traversal",
        );
    }
    Ok(())
}

fn hash_repository_tree(root: &Path) -> Result<String, CanonicalFixtureError> {
    let mut files = Vec::new();
    collect_repository_files(root, root, &mut files)?;
    if files.is_empty() {
        return invalid("repository tree", "files", "must not be empty");
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    if files.len() > MAX_REPOSITORY_FILES {
        return invalid(
            "repository tree",
            "files",
            format!("has more than {MAX_REPOSITORY_FILES} files"),
        );
    }
    let total_bytes = files.iter().try_fold(0_u64, |total, (_, bytes)| {
        total.checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
    });
    if total_bytes.is_none_or(|total| total > MAX_REPOSITORY_BYTES) {
        return invalid(
            "repository tree",
            "bytes",
            format!("exceeds {MAX_REPOSITORY_BYTES} bytes"),
        );
    }

    let mut digest = Sha256::new();
    digest.update(b"execution-protocol-v1:fixture-repository-tree\0");
    for (relative, bytes) in files {
        digest.update(
            u64::try_from(relative.len())
                .expect("bounded fixture path length fits u64")
                .to_be_bytes(),
        );
        digest.update(relative.as_bytes());
        digest.update(
            u64::try_from(bytes.len())
                .expect("bounded fixture file length fits u64")
                .to_be_bytes(),
        );
        digest.update(bytes);
    }
    Ok(hex::encode(digest.finalize()))
}

fn collect_repository_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), CanonicalFixtureError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|source| CanonicalFixtureError::Io {
            path: directory.into(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| CanonicalFixtureError::Io {
            path: directory.into(),
            source,
        })?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| CanonicalFixtureError::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return invalid(
                "repository tree",
                "files",
                format!("symlink `{}` is forbidden", path.display()),
            );
        }
        if metadata.is_dir() {
            collect_repository_files(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| CanonicalFixtureError::Invalid {
                    artifact: "repository tree",
                    field: "files",
                    detail: format!("`{}` escaped the repository root", path.display()),
                })?;
            let relative = relative
                .to_str()
                .ok_or_else(|| CanonicalFixtureError::Invalid {
                    artifact: "repository tree",
                    field: "files",
                    detail: "paths must be UTF-8".into(),
                })?;
            validate_relative_path("repository tree", "files", relative)?;
            let bytes = fs::read(&path).map_err(|source| CanonicalFixtureError::Io {
                path: path.clone(),
                source,
            })?;
            files.push((relative.into(), bytes));
        } else {
            return invalid(
                "repository tree",
                "files",
                format!("`{}` is not a regular file or directory", path.display()),
            );
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TomlScalar {
    String(String),
    Integer(u64),
}

fn parse_manifest(input: &str) -> Result<CanonicalFixtureManifestV1, CanonicalFixtureError> {
    let mut values = BTreeMap::new();
    for (index, raw_line) in input.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') || line.contains('#') {
            return manifest_syntax(line_number, "sections and inline comments are not allowed");
        }
        let (key, raw_value) =
            line.split_once('=')
                .ok_or_else(|| CanonicalFixtureError::ManifestSyntax {
                    line: line_number,
                    detail: "expected `key = value`".into(),
                })?;
        let key = key.trim();
        let raw_value = raw_value.trim();
        if !is_slug(key) {
            return manifest_syntax(line_number, "key must use lowercase snake_case");
        }
        let value = if raw_value.starts_with('"') {
            TomlScalar::String(serde_json::from_str(raw_value).map_err(|_| {
                CanonicalFixtureError::ManifestSyntax {
                    line: line_number,
                    detail: "string must use a single valid quoted value".into(),
                }
            })?)
        } else {
            TomlScalar::Integer(raw_value.parse().map_err(|_| {
                CanonicalFixtureError::ManifestSyntax {
                    line: line_number,
                    detail: "value must be a quoted string or unsigned integer".into(),
                }
            })?)
        };
        if values.insert(key.to_owned(), value).is_some() {
            return manifest_syntax(line_number, format!("duplicate key `{key}`"));
        }
    }

    let schema_version = take_u16(&mut values, "schema_version")?;
    let fixture_id = take_string(&mut values, "fixture_id")?;
    let fixture_scope = match take_string(&mut values, "fixture_scope")?.as_str() {
        "schema_foundation" => FixtureScopeV1::SchemaFoundation,
        _ => {
            return invalid(
                MANIFEST_NAME,
                "fixture_scope",
                "must be schema_foundation for this fixture schema",
            );
        }
    };
    let description = take_string(&mut values, "description")?;
    let mission = take_string(&mut values, "mission")?;
    let repository_path = take_string(&mut values, "repository_path")?;
    let expected_ecosystem = match take_string(&mut values, "expected_ecosystem")?.as_str() {
        "rust" => ExpectedEcosystemV1::Rust,
        "node" => ExpectedEcosystemV1::Node,
        "python" => ExpectedEcosystemV1::Python,
        "go" => ExpectedEcosystemV1::Go,
        "unknown" => ExpectedEcosystemV1::Unknown,
        _ => {
            return invalid(
                MANIFEST_NAME,
                "expected_ecosystem",
                "must be rust, node, python, go, or unknown",
            );
        }
    };
    let expected_generated_path_count = take_u32(&mut values, "expected_generated_path_count")?;
    let max_model_calls = take_u32(&mut values, "max_model_calls")?;
    let max_input_tokens_per_call = take_u32(&mut values, "max_input_tokens_per_call")?;
    let max_output_tokens_per_call = take_u32(&mut values, "max_output_tokens_per_call")?;
    let provider_script_path = take_string(&mut values, "provider_script_path")?;
    let expected_events_path = take_string(&mut values, "expected_events_path")?;
    let expected_result_path = take_string(&mut values, "expected_result_path")?;
    let repository_tree_hash = take_string(&mut values, "repository_tree_hash")?;
    let semantic_hash = take_string(&mut values, "semantic_hash")?;
    if let Some(key) = values.keys().next() {
        return invalid(MANIFEST_NAME, "key", format!("unknown key `{key}`"));
    }
    Ok(CanonicalFixtureManifestV1 {
        schema_version,
        fixture_id,
        fixture_scope,
        description,
        mission,
        repository_path,
        expected_ecosystem,
        expected_generated_path_count,
        max_model_calls,
        max_input_tokens_per_call,
        max_output_tokens_per_call,
        provider_script_path,
        expected_events_path,
        expected_result_path,
        repository_tree_hash,
        semantic_hash,
    })
}

fn take_string(
    values: &mut BTreeMap<String, TomlScalar>,
    key: &'static str,
) -> Result<String, CanonicalFixtureError> {
    match values.remove(key) {
        Some(TomlScalar::String(value)) => Ok(value),
        Some(TomlScalar::Integer(_)) => invalid(MANIFEST_NAME, key, "must be a quoted string"),
        None => invalid(MANIFEST_NAME, key, "is required"),
    }
}

fn take_u16(
    values: &mut BTreeMap<String, TomlScalar>,
    key: &'static str,
) -> Result<u16, CanonicalFixtureError> {
    let value = take_u64(values, key)?;
    u16::try_from(value).map_err(|_| CanonicalFixtureError::Invalid {
        artifact: MANIFEST_NAME,
        field: key,
        detail: "does not fit u16".into(),
    })
}

fn take_u32(
    values: &mut BTreeMap<String, TomlScalar>,
    key: &'static str,
) -> Result<u32, CanonicalFixtureError> {
    let value = take_u64(values, key)?;
    u32::try_from(value).map_err(|_| CanonicalFixtureError::Invalid {
        artifact: MANIFEST_NAME,
        field: key,
        detail: "does not fit u32".into(),
    })
}

fn take_u64(
    values: &mut BTreeMap<String, TomlScalar>,
    key: &'static str,
) -> Result<u64, CanonicalFixtureError> {
    match values.remove(key) {
        Some(TomlScalar::Integer(value)) => Ok(value),
        Some(TomlScalar::String(_)) => invalid(MANIFEST_NAME, key, "must be an unsigned integer"),
        None => invalid(MANIFEST_NAME, key, "is required"),
    }
}

fn validate_slug(
    artifact: &'static str,
    field: &'static str,
    value: &str,
) -> Result<(), CanonicalFixtureError> {
    if !is_slug(value) || value.len() > 128 {
        return invalid(
            artifact,
            field,
            "must be non-empty lowercase snake_case with at most 128 bytes",
        );
    }
    Ok(())
}

fn is_slug(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('_')
        && !value.ends_with('_')
        && !value.contains("__")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn validate_semantic_key(value: &str) -> Result<(), CanonicalFixtureError> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b':' | b'-' | b'.')
        })
    {
        return invalid(
            "expected events",
            "events.semantic_key",
            "must be a canonical non-secret semantic key",
        );
    }
    Ok(())
}

fn validate_text(
    artifact: &'static str,
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), CanonicalFixtureError> {
    if value.is_empty()
        || value.len() > maximum
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return invalid(
            artifact,
            field,
            format!("must be canonical printable text with at most {maximum} bytes"),
        );
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid<T>(
    artifact: &'static str,
    field: &'static str,
    detail: impl Into<String>,
) -> Result<T, CanonicalFixtureError> {
    Err(CanonicalFixtureError::Invalid {
        artifact,
        field,
        detail: detail.into(),
    })
}

fn manifest_syntax<T>(line: usize, detail: impl Into<String>) -> Result<T, CanonicalFixtureError> {
    Err(CanonicalFixtureError::ManifestSyntax {
        line,
        detail: detail.into(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    fn canonical_fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/execution_protocol_v1/canonical/tiny_static_change")
    }

    fn copy_fixture() -> tempfile::TempDir {
        let target = tempfile::tempdir().expect("temporary fixture directory");
        copy_directory(&canonical_fixture_path(), target.path());
        target
    }

    fn copy_directory(source: &Path, target: &Path) {
        fs::create_dir_all(target).expect("fixture target directory");
        for entry in fs::read_dir(source).expect("fixture source directory") {
            let entry = entry.expect("fixture directory entry");
            let source_path = entry.path();
            let target_path = target.join(entry.file_name());
            if entry.file_type().expect("fixture entry type").is_dir() {
                copy_directory(&source_path, &target_path);
            } else {
                fs::copy(source_path, target_path).expect("fixture file copy");
            }
        }
    }

    #[test]
    fn checked_in_tiny_static_change_fixture_is_strict_and_content_addressed() {
        let fixture = load_canonical_fixture(canonical_fixture_path())
            .expect("checked-in canonical fixture must load");

        assert_eq!(fixture.manifest.fixture_id, "tiny_static_change");
        assert_eq!(
            fixture.manifest.fixture_scope,
            FixtureScopeV1::SchemaFoundation
        );
        assert_eq!(
            fixture.manifest.expected_ecosystem,
            ExpectedEcosystemV1::Unknown
        );
        assert_eq!(fixture.provider_script.steps.len(), 5);
        assert_eq!(
            fixture.expected_events.trace_kind,
            ExpectedTraceKindV1::CheckpointSummary
        );
        assert_eq!(fixture.expected_events.events.len(), 20);
        assert_eq!(fixture.expected_events.events[0].sequence, 1);
        assert_eq!(fixture.expected_events.events[19].sequence, 20);
        assert_eq!(
            fixture.expected_result.outcome,
            ExpectedOutcomeV1::Succeeded
        );
        assert_eq!(
            fixture.expected_result.process_health,
            ExpectedProcessHealthV1::Healthy
        );
        assert!(fixture.expected_result.remaining_work.is_empty());
    }

    #[test]
    fn repository_content_tampering_breaks_the_manifest_hash_chain() {
        let fixture = copy_fixture();
        fs::write(
            fixture.path().join("repository/index.txt"),
            "status=compromised\n",
        )
        .expect("tamper repository fixture");

        assert!(matches!(
            load_canonical_fixture(fixture.path()),
            Err(CanonicalFixtureError::SemanticHashMismatch {
                artifact: "repository tree",
                ..
            })
        ));
    }

    #[test]
    fn strict_json_rejects_unknown_provider_fields_before_hash_validation() {
        let fixture = copy_fixture();
        let path = fixture.path().join(PROVIDER_SCRIPT_PATH);
        let contents = fs::read_to_string(&path).expect("provider fixture");
        let tampered = contents.replacen(
            "\"schema_version\": 1,",
            "\"schema_version\": 1,\n  \"unexpected\": true,",
            1,
        );
        fs::write(path, tampered).expect("tamper provider fixture");

        assert!(matches!(
            load_canonical_fixture(fixture.path()),
            Err(CanonicalFixtureError::Json {
                artifact: "provider-script.json",
                ..
            })
        ));
    }

    #[test]
    fn strict_manifest_rejects_unknown_or_traversing_fields() {
        let unknown = copy_fixture();
        let path = unknown.path().join(MANIFEST_NAME);
        let mut contents = fs::read_to_string(&path).expect("fixture manifest");
        contents.push_str("unknown_key = 1\n");
        fs::write(path, contents).expect("tamper fixture manifest");
        assert!(matches!(
            load_canonical_fixture(unknown.path()),
            Err(CanonicalFixtureError::Invalid {
                artifact: MANIFEST_NAME,
                field: "key",
                ..
            })
        ));

        let traversing = copy_fixture();
        let path = traversing.path().join(MANIFEST_NAME);
        let contents = fs::read_to_string(&path)
            .expect("fixture manifest")
            .replace(
                "repository_path = \"repository\"",
                "repository_path = \"../repository\"",
            );
        fs::write(path, contents).expect("tamper fixture manifest");
        assert!(matches!(
            load_canonical_fixture(traversing.path()),
            Err(CanonicalFixtureError::Invalid {
                artifact: MANIFEST_NAME,
                field: "repository_path",
                ..
            })
        ));
    }

    #[test]
    fn checkpoint_sequences_must_follow_one_based_protocol_envelopes() {
        let fixture = copy_fixture();
        let path = fixture.path().join(EXPECTED_EVENTS_PATH);
        let contents = fs::read_to_string(&path)
            .expect("expected events fixture")
            .replacen("\"sequence\": 1", "\"sequence\": 0", 1);
        fs::write(path, contents).expect("tamper event sequence");

        assert!(matches!(
            load_canonical_fixture(fixture.path()),
            Err(CanonicalFixtureError::Invalid {
                artifact: "expected events",
                field: "events.sequence",
                ..
            })
        ));
    }

    #[test]
    fn schema_foundation_cannot_claim_an_unimplemented_full_reducer_trace() {
        let fixture = copy_fixture();
        let path = fixture.path().join(EXPECTED_EVENTS_PATH);
        let contents = fs::read_to_string(&path)
            .expect("expected events fixture")
            .replace("\"checkpoint_summary\"", "\"full_reducer_trace\"");
        fs::write(path, contents).expect("tamper trace kind");

        assert!(matches!(
            load_canonical_fixture(fixture.path()),
            Err(CanonicalFixtureError::Json {
                artifact: "expected-events.json",
                ..
            })
        ));
    }

    #[test]
    fn event_and_result_hashes_fail_closed_on_semantic_tampering() {
        let events = copy_fixture();
        let path = events.path().join(EXPECTED_EVENTS_PATH);
        let contents = fs::read_to_string(&path)
            .expect("expected events fixture")
            .replacen("\"path_count\": 1", "\"path_count\": 2", 1);
        fs::write(path, contents).expect("tamper event fixture");
        assert!(matches!(
            load_canonical_fixture(events.path()),
            Err(CanonicalFixtureError::SemanticHashMismatch {
                artifact: "expected event",
                ..
            })
        ));

        let result = copy_fixture();
        let path = result.path().join(EXPECTED_RESULT_PATH);
        let contents = fs::read_to_string(&path)
            .expect("expected result fixture")
            .replace("\"publication_succeeded\"", "\"publication_completed\"");
        fs::write(path, contents).expect("tamper result fixture");
        assert!(matches!(
            load_canonical_fixture(result.path()),
            Err(CanonicalFixtureError::SemanticHashMismatch {
                artifact: "expected result",
                ..
            })
        ));
    }
}
