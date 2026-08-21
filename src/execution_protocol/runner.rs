//! Durable orchestration boundary for Execution Protocol v1.
//!
//! This module deliberately contains no production routing or side-effect
//! implementation. It defines the smallest persistence and effect seams needed
//! to drive the pure reducer without making an indeterminate operation look as
//! though it definitely failed.

use serde::{Deserialize, Serialize};

use super::{
    AppendOutcome, BudgetEvent, CanonicalResult, CorrelationId, DomainEvent,
    EXECUTION_PROTOCOL_VERSION, EffectId, EffectObservationBinding, EffectRequest, EventId,
    EvidenceEvent, ExecutionId, ExecutionState, GraphEvent, ImplementationEffectRequest,
    MutationEffectRequest, NodeId, PROTOCOL_EVENT_SCHEMA_VERSION, PlanningEffectRequest,
    ProfileEvent, ProtocolDecision, ProtocolEventContext, ProtocolEventEnvelope, ProtocolStage,
    ProtocolViolation, PublicationEffectRequest, RepositoryProfile, RepositoryRevisionId,
    ReviewEffectRequest, TerminalEvent, ValidationEffectRequest, WaitReason, decide_strict_v1,
    reduce_strict_v1, stable_sha256,
};

const RUNNER_EFFECT_INTENT_SCHEMA_VERSION: u16 = 1;
const EFFECT_OBSERVATION_SEMANTIC_KEY: &str = "execution-protocol-v1:effect-observation";

/// Result of crossing a durable or externally observable boundary.
///
/// `Definite(Err(_))` means the operation is known not to have succeeded.
/// `Indeterminate(_)` means the caller must reconcile before retrying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BoundaryOutcome<T, E> {
    Definite(Result<T, E>),
    Indeterminate(E),
}

/// Result of a compare-and-swap operation against the aggregate revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CasOutcome<T> {
    Committed(T),
    Conflict {
        expected: u64,
        actual: u64,
    },
    /// The compare-and-swap was rejected before writing because the exact
    /// execution-attempt authority no longer matches.
    AuthorityRejected {
        actual: ExecutionAttemptAuthorityFence,
    },
}

/// Lease authority observed from the durable control plane.
///
/// `ConfirmedLost` is intentionally distinct from a transport error. Only a
/// definite control-plane observation may use it.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LeaseAuthorityStatus {
    Held,
    ConfirmedLost,
}

impl LeaseAuthorityStatus {
    const fn identity_key(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::ConfirmedLost => "confirmed_lost",
        }
    }
}

/// Durable cancellation observation for this execution attempt.
///
/// The runner cannot convert either cancellation state into a domain event:
/// the reducer does not yet expose cancellation authority. It therefore stops
/// before effects and waits for that reducer contract to exist.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CancellationAuthorityStatus {
    Active,
    Requested,
    Confirmed,
}

impl CancellationAuthorityStatus {
    const fn identity_key(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Requested => "requested",
            Self::Confirmed => "confirmed",
        }
    }
}

/// Exact write/effect authority for one execution attempt.
///
/// The lease epoch is represented only by its non-secret SHA-256 binding. The
/// fence is returned atomically with the event stream and unresolved outbox
/// entry, then supplied to every compare-and-swap and effect adapter call.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionAttemptAuthorityFence {
    pub(crate) execution_id: ExecutionId,
    pub(crate) execution_attempt: u32,
    pub(crate) lease_epoch_hash: String,
    pub(crate) lease_status: LeaseAuthorityStatus,
    pub(crate) cancellation_revision: u64,
    pub(crate) cancellation_status: CancellationAuthorityStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthorityFenceError {
    Invalid { field: &'static str },
}

impl ExecutionAttemptAuthorityFence {
    pub(crate) fn new(
        execution_id: ExecutionId,
        execution_attempt: u32,
        lease_epoch_hash: String,
        lease_status: LeaseAuthorityStatus,
        cancellation_revision: u64,
        cancellation_status: CancellationAuthorityStatus,
    ) -> Result<Self, AuthorityFenceError> {
        let fence = Self {
            execution_id,
            execution_attempt,
            lease_epoch_hash,
            lease_status,
            cancellation_revision,
            cancellation_status,
        };
        fence.validate_shape()?;
        Ok(fence)
    }

    fn validate_shape(&self) -> Result<(), AuthorityFenceError> {
        if self.execution_id.as_str().trim().is_empty() {
            return Err(AuthorityFenceError::Invalid {
                field: "execution_id",
            });
        }
        if self.execution_attempt == 0 {
            return Err(AuthorityFenceError::Invalid {
                field: "execution_attempt",
            });
        }
        if !is_sha256(&self.lease_epoch_hash) {
            return Err(AuthorityFenceError::Invalid {
                field: "lease_epoch_hash",
            });
        }
        Ok(())
    }

    fn validate_for(&self, state: &ExecutionState) -> Result<(), &'static str> {
        self.validate_shape()
            .map_err(|_| "authority_fence_shape_invalid")?;
        if self.execution_id != state.execution_id
            || self.execution_attempt != state.execution_attempt
        {
            return Err("authority_fence_execution_attempt_mismatch");
        }
        Ok(())
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A trusted bootstrap plus the complete committed event stream.
///
/// Stores may use snapshots internally, but they must return the separately
/// trusted bootstrap and all events required to prove the materialized state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoadedEventStream {
    pub(crate) trusted_bootstrap: ExecutionState,
    pub(crate) events: Vec<ProtocolEventEnvelope>,
    pub(crate) stream_revision: u64,
    pub(crate) authority: ExecutionAttemptAuthorityFence,
    pub(crate) unresolved_effect: Option<EffectIntent>,
}

/// Non-secret durable identity for an ephemeral typed effect request.
///
/// `EffectRequest` deliberately has no uniform wire representation. The runner
/// therefore serializes each known variant only long enough to hash it and
/// persists this typed kind/digest pair, never the raw request material.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EffectRequestKind {
    Discovery,
    Planning,
    Implementation,
    Mutation,
    Validation,
    Review,
    Publication,
}

/// Whether the request has a canonical typed wire form or deliberately carries
/// non-serializable, secret-bearing material that must remain ephemeral.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EffectRequestMaterialClass {
    CanonicalTyped,
    EphemeralPullRequestMaterial,
}

impl EffectRequestMaterialClass {
    const fn identity_key(self) -> &'static str {
        match self {
            Self::CanonicalTyped => "canonical_typed",
            Self::EphemeralPullRequestMaterial => "ephemeral_pull_request_material",
        }
    }
}

impl EffectRequestKind {
    const fn identity_key(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Planning => "planning",
            Self::Implementation => "implementation",
            Self::Mutation => "mutation",
            Self::Validation => "validation",
            Self::Review => "review",
            Self::Publication => "publication",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SafeEffectRequestIdentity {
    pub(crate) kind: EffectRequestKind,
    pub(crate) material_class: EffectRequestMaterialClass,
    pub(crate) digest: String,
}

impl SafeEffectRequestIdentity {
    fn from_request(request: &EffectRequest) -> Result<Self, ProtocolViolation> {
        let (kind, material_class, canonical) = match request {
            EffectRequest::Discovery(request) => (
                EffectRequestKind::Discovery,
                EffectRequestMaterialClass::CanonicalTyped,
                canonical_request_json(request)?,
            ),
            EffectRequest::Planning(PlanningEffectRequest::DispatchProvider { envelope }) => (
                EffectRequestKind::Planning,
                EffectRequestMaterialClass::CanonicalTyped,
                canonical_request_json(envelope.as_ref())?,
            ),
            EffectRequest::Implementation(ImplementationEffectRequest::LoadTargetContext {
                request,
            }) => (
                EffectRequestKind::Implementation,
                EffectRequestMaterialClass::CanonicalTyped,
                canonical_request_json(request.as_ref())?,
            ),
            EffectRequest::Mutation(request) => (
                EffectRequestKind::Mutation,
                EffectRequestMaterialClass::CanonicalTyped,
                canonical_mutation_request_json(request)?,
            ),
            EffectRequest::Validation(request) => (
                EffectRequestKind::Validation,
                EffectRequestMaterialClass::CanonicalTyped,
                canonical_validation_request_json(request)?,
            ),
            EffectRequest::Review(request) => (
                EffectRequestKind::Review,
                EffectRequestMaterialClass::CanonicalTyped,
                canonical_review_request_json(request)?,
            ),
            EffectRequest::Publication(PublicationEffectRequest::EnsurePullRequest {
                intent,
                material,
            }) => (
                EffectRequestKind::Publication,
                EffectRequestMaterialClass::EphemeralPullRequestMaterial,
                canonical_request_json(&(
                    intent,
                    material.title_hash(),
                    material.body_hash(),
                    material.title().len(),
                    material.body().len(),
                ))?,
            ),
            EffectRequest::Publication(request) => (
                EffectRequestKind::Publication,
                EffectRequestMaterialClass::CanonicalTyped,
                canonical_publication_request_json(request)?,
            ),
        };
        Ok(Self {
            kind,
            material_class,
            digest: stable_sha256(&[
                "execution-protocol-v1:safe-effect-request",
                kind.identity_key(),
                material_class.identity_key(),
                &canonical,
            ]),
        })
    }
}

fn canonical_request_json<T: Serialize>(request: &T) -> Result<String, ProtocolViolation> {
    serde_json::to_string(request).map_err(|error| ProtocolViolation::EventSerialization {
        detail: error.to_string(),
    })
}

fn canonical_mutation_request_json(
    request: &MutationEffectRequest,
) -> Result<String, ProtocolViolation> {
    match request {
        MutationEffectRequest::DispatchProvider { request } => canonical_request_json(request),
        MutationEffectRequest::ApplyMutation { request } => canonical_request_json(request),
        MutationEffectRequest::VerifyMutation { request } => canonical_request_json(request),
    }
}

fn canonical_validation_request_json(
    request: &ValidationEffectRequest,
) -> Result<String, ProtocolViolation> {
    match request {
        ValidationEffectRequest::RunProcess { request } => canonical_request_json(request),
        ValidationEffectRequest::LoadRepairTargetContext { request } => {
            canonical_request_json(request)
        }
    }
}

fn canonical_review_request_json(
    request: &ReviewEffectRequest,
) -> Result<String, ProtocolViolation> {
    match request {
        ReviewEffectRequest::BuildDiffManifest { request } => canonical_request_json(request),
        ReviewEffectRequest::ObservePublicationAuthority { request } => {
            canonical_request_json(request)
        }
        ReviewEffectRequest::DispatchProvider { envelope } => canonical_request_json(envelope),
    }
}

fn canonical_publication_request_json(
    request: &PublicationEffectRequest,
) -> Result<String, ProtocolViolation> {
    match request {
        PublicationEffectRequest::CreateCommit { intent } => canonical_request_json(intent),
        PublicationEffectRequest::PushExactLease { intent } => canonical_request_json(intent),
        PublicationEffectRequest::EnsurePullRequest { .. } => unreachable!(
            "non-serializable pull-request material uses the explicit ephemeral branch"
        ),
    }
}

/// Stable durable intent recorded before an effect.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectIntent {
    pub(crate) schema_version: u16,
    pub(crate) intent_id: EffectId,
    pub(crate) authority: ExecutionAttemptAuthorityFence,
    pub(crate) triggering_event_id: EventId,
    pub(crate) aggregate_revision: u64,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) request_identity: SafeEffectRequestIdentity,
}

impl EffectIntent {
    fn new(
        state: &ExecutionState,
        authority: &ExecutionAttemptAuthorityFence,
        request: &EffectRequest,
    ) -> Result<Self, ProtocolViolation> {
        let triggering_event_id = state
            .event_log
            .last()
            .map(|stored| stored.envelope.event_id.clone())
            .ok_or(ProtocolViolation::Invariant {
                code: "effect_triggering_event_missing",
                detail: "an effect must be caused by a committed protocol event".into(),
            })?;
        let request_identity = SafeEffectRequestIdentity::from_request(request)?;
        let mut intent = Self {
            schema_version: RUNNER_EFFECT_INTENT_SCHEMA_VERSION,
            intent_id: EffectId::new("pending:runner-effect-intent"),
            authority: authority.clone(),
            triggering_event_id,
            aggregate_revision: state.aggregate_revision,
            repository_revision: state.repository_revision.clone(),
            request_identity,
        };
        intent.intent_id = intent.expected_intent_id();
        Ok(intent)
    }

    fn expected_intent_id(&self) -> EffectId {
        EffectId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:runner-effect-intent",
                self.authority.execution_id.as_str(),
                &self.authority.execution_attempt.to_string(),
                &self.authority.lease_epoch_hash,
                self.authority.lease_status.identity_key(),
                &self.authority.cancellation_revision.to_string(),
                self.authority.cancellation_status.identity_key(),
                self.triggering_event_id.as_str(),
                &self.aggregate_revision.to_string(),
                self.repository_revision.as_str(),
                self.request_identity.kind.identity_key(),
                self.request_identity.material_class.identity_key(),
                &self.request_identity.digest,
            ])
        ))
    }

    /// Produces the non-secret identity that must be persisted on the event
    /// atomically resolving this intent.
    pub(crate) fn observation_binding(
        &self,
    ) -> Result<EffectObservationBinding, ProtocolViolation> {
        EffectObservationBinding::new(self.intent_id.clone(), self.request_identity.digest.clone())
    }

    /// Lets a durable store reject an observation that does not name the exact
    /// intent and safe request digest supplied to its atomic commit method.
    pub(crate) fn matches_observation_event(
        &self,
        event: &ProtocolEventEnvelope,
    ) -> Result<bool, ProtocolViolation> {
        Ok(self.schema_version == RUNNER_EFFECT_INTENT_SCHEMA_VERSION
            && self.intent_id == self.expected_intent_id()
            && self.authority.validate_shape().is_ok()
            && self.authority.lease_status == LeaseAuthorityStatus::Held
            && self.authority.cancellation_status == CancellationAuthorityStatus::Active
            && event.protocol_version == EXECUTION_PROTOCOL_VERSION
            && event.event_schema_version == PROTOCOL_EVENT_SCHEMA_VERSION
            && event.semantic_key == EFFECT_OBSERVATION_SEMANTIC_KEY
            && event.execution_id == self.authority.execution_id
            && event.execution_attempt == self.authority.execution_attempt
            && event.causation_id.as_ref() == Some(&self.triggering_event_id)
            && event.aggregate_revision_before == self.aggregate_revision
            && event.sequence == self.aggregate_revision.saturating_add(1)
            && event.repository_revision == self.repository_revision
            && event.effect_observation.as_ref() == Some(&self.observation_binding()?)
            && event.semantic_identity == event.expected_semantic_identity()?
            && event.event_id == event.expected_event_id()?)
    }

    fn matches_state_and_request(
        &self,
        state: &ExecutionState,
        current_authority: &ExecutionAttemptAuthorityFence,
        request: &EffectRequest,
    ) -> Result<bool, ProtocolViolation> {
        let request_identity = SafeEffectRequestIdentity::from_request(request)?;
        Ok(self.schema_version == RUNNER_EFFECT_INTENT_SCHEMA_VERSION
            && self.authority.execution_id == state.execution_id
            && self.authority.execution_attempt == state.execution_attempt
            && self.authority.validate_shape().is_ok()
            && self.authority.lease_status == LeaseAuthorityStatus::Held
            && self.authority.cancellation_status == CancellationAuthorityStatus::Active
            && current_authority.execution_id == state.execution_id
            && current_authority.execution_attempt == state.execution_attempt
            && current_authority.validate_shape().is_ok()
            && current_authority.lease_status == LeaseAuthorityStatus::Held
            && current_authority.cancellation_status == CancellationAuthorityStatus::Active
            && state
                .event_log
                .last()
                .is_some_and(|stored| stored.envelope.event_id == self.triggering_event_id)
            && self.aggregate_revision == state.aggregate_revision
            && self.repository_revision == state.repository_revision
            && self.request_identity == request_identity
            && self.intent_id == self.expected_intent_id())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IntentPersistOutcome {
    Inserted,
    Existing(EffectIntent),
}

/// A definitely known effect result, including definite domain failures.
///
/// Adapters map every definitely observed success or failure to an exact
/// protocol event. Transport errors whose remote outcome is unknown must be
/// returned as `BoundaryOutcome::Indeterminate` instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EffectObservation {
    pub(crate) occurred_at_ms: u64,
    pub(crate) event: DomainEvent,
}

/// Narrow compare-and-swap event journal.
pub(crate) trait CasEventStore {
    type Error;

    /// Loads stream, unresolved outbox entry, and authority from one strongly
    /// consistent durable snapshot.
    fn load_event_stream(&mut self) -> BoundaryOutcome<LoadedEventStream, Self::Error>;

    /// Appends exactly `event` if `expected_revision` still owns the stream.
    /// Replaying the same event id and payload may return an idempotent
    /// `AppendOutcome` even if the stream has already advanced.
    fn append_event_cas(
        &mut self,
        expected_revision: u64,
        authority: &ExecutionAttemptAuthorityFence,
        event: ProtocolEventEnvelope,
    ) -> BoundaryOutcome<CasOutcome<AppendOutcome>, Self::Error>;
}

/// Durable effect outbox sharing the event journal's transactional authority.
///
/// While an intent is unresolved, the store must reject every stream append
/// except the matching atomic observation commit. Event and outbox reads must
/// also share one strongly consistent view of the execution attempt.
pub(crate) trait EffectOutbox: CasEventStore {
    /// Persists the safe effect identity while comparing both the event
    /// revision and exact execution-attempt authority fence.
    /// Only `Inserted` authorizes the current caller to perform immediately;
    /// `Existing` always requires reconciliation first.
    fn persist_effect_intent_cas(
        &mut self,
        expected_revision: u64,
        authority: &ExecutionAttemptAuthorityFence,
        intent: &EffectIntent,
    ) -> BoundaryOutcome<CasOutcome<IntentPersistOutcome>, Self::Error>;

    /// Atomically appends the observation and resolves its matching intent.
    /// Implementations must never resolve an intent without committing the
    /// event, nor commit a mismatched event for the intent. In particular,
    /// they must verify [`EffectIntent::matches_observation_event`] inside the
    /// same transaction before either write becomes visible.
    fn commit_effect_observation_cas(
        &mut self,
        expected_revision: u64,
        authority: &ExecutionAttemptAuthorityFence,
        intent: &EffectIntent,
        event: ProtocolEventEnvelope,
    ) -> BoundaryOutcome<CasOutcome<AppendOutcome>, Self::Error>;
}

/// Effect boundary result with an explicit, definitely observed authority
/// rejection. Generic adapter failures can never be interpreted as lease loss.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AuthorizedEffectOutcome<T, E> {
    Boundary(BoundaryOutcome<T, E>),
    AuthorityRejected {
        actual: ExecutionAttemptAuthorityFence,
    },
}

/// Adapter boundary for one exact, already persisted effect request.
pub(crate) trait EffectExecutor {
    type Error;

    /// Performs an intent that was newly inserted into the durable outbox, or
    /// was definitively reconciled as not performed. The intent retains the
    /// authority that created it for stable identity; `current_authority` is
    /// the newly loaded Held/Active fence that implementations must enforce at
    /// their own side-effect boundary.
    fn perform(
        &mut self,
        intent: &EffectIntent,
        current_authority: &ExecutionAttemptAuthorityFence,
        request: &EffectRequest,
    ) -> AuthorizedEffectOutcome<EffectObservation, Self::Error>;

    /// Reconciles an intent loaded from durable storage.
    ///
    /// `Ok(Some(_))` is an authoritative observation. `Ok(None)` proves that
    /// the effect was not performed and therefore permits one new attempt.
    /// Reconciliation must itself enforce `current_authority`; a recovered
    /// intent may have been created under an earlier lease epoch belonging to
    /// the same execution attempt.
    fn reconcile(
        &mut self,
        intent: &EffectIntent,
        current_authority: &ExecutionAttemptAuthorityFence,
        request: &EffectRequest,
    ) -> AuthorizedEffectOutcome<Option<EffectObservation>, Self::Error>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteSuppressionReason {
    ConfirmedLeaseLoss,
    CancellationRequiresReducerAuthority,
    AuthorityFenceChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RunnerStep {
    /// The strict aggregate is pristine and requires a separately trusted,
    /// policy-matching repository profile before ordinary reduction can run.
    BootstrapProfileRequired {
        repository_revision: RepositoryRevisionId,
    },
    EventPersisted {
        event_id: super::EventId,
        outcome: AppendOutcome,
    },
    EffectObservationPersisted {
        intent_id: EffectId,
        event_id: super::EventId,
        outcome: AppendOutcome,
        reconciled: bool,
    },
    ReloadRequired {
        expected_revision: u64,
        actual_revision: u64,
    },
    /// No event or effect may be written under the observed authority. This is
    /// control-plane state, never a synthetic protocol failure.
    WriteSuppressed {
        reason: WriteSuppressionReason,
        authority: ExecutionAttemptAuthorityFence,
    },
    Waiting {
        reason: WaitReason,
    },
    Finished {
        result: CanonicalResult,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RunnerError<StoreError, EffectError> {
    Protocol(ProtocolViolation),
    InvalidLoadedStream {
        code: &'static str,
    },
    OutboxInvariant {
        code: &'static str,
    },
    StoreDefinite(StoreError),
    StoreIndeterminate(StoreError),
    EffectDefinite {
        intent_id: EffectId,
        error: EffectError,
    },
    EffectIndeterminate {
        intent_id: EffectId,
        error: EffectError,
    },
}

/// Replays committed history, decides once, and durably advances at most one
/// reducer/effect observation event.
///
/// Effects are invoked only after their exact request is durably inserted. A
/// previously persisted request is reconciled before any retry.
pub(crate) fn run_once<S, E>(
    store: &mut S,
    executor: &mut E,
    occurred_at_ms: u64,
) -> Result<RunnerStep, RunnerError<S::Error, E::Error>>
where
    S: EffectOutbox,
    E: EffectExecutor,
{
    let loaded = load_replayed_state::<S, E::Error>(store)?;
    if let Some(step) = suppression_for_authority(&loaded.authority) {
        return Ok(step);
    }
    if let Some(intent) = loaded.unresolved_effect {
        let request = validate_pending_intent::<S::Error, E::Error>(
            &loaded.state,
            &loaded.authority,
            &intent,
        )?;
        return reconcile_or_resume(
            store,
            executor,
            &loaded.state,
            &loaded.authority,
            intent,
            request,
        );
    }
    if loaded.state.aggregate_revision == 0
        && loaded.state.stage() == ProtocolStage::Profiling
        && loaded.state.repository_profile.is_none()
    {
        return Ok(RunnerStep::BootstrapProfileRequired {
            repository_revision: loaded.state.repository_revision.clone(),
        });
    }

    match decide_strict_v1(&loaded.state).map_err(RunnerError::Protocol)? {
        ProtocolDecision::Emit { event } => persist_reducer_event::<S, E::Error>(
            store,
            &loaded.state,
            &loaded.authority,
            occurred_at_ms,
            event,
        ),
        ProtocolDecision::Perform { effect } => {
            persist_and_perform(store, executor, &loaded.state, &loaded.authority, effect)
        }
        ProtocolDecision::Finish { result } => persist_finish_decision::<S, E::Error>(
            store,
            &loaded.state,
            &loaded.authority,
            occurred_at_ms,
            result,
        ),
        ProtocolDecision::Wait { reason } => Ok(RunnerStep::Waiting { reason }),
    }
}

fn persist_finish_decision<S, EffectError>(
    store: &mut S,
    state: &ExecutionState,
    authority: &ExecutionAttemptAuthorityFence,
    occurred_at_ms: u64,
    result: CanonicalResult,
) -> Result<RunnerStep, RunnerError<S::Error, EffectError>>
where
    S: CasEventStore,
{
    if let Some(recorded) = &state.terminal {
        if recorded != &result {
            return Err(RunnerError::InvalidLoadedStream {
                code: "terminal_result_differs_from_reducer_decision",
            });
        }
        return Ok(RunnerStep::Finished { result });
    }
    persist_reducer_event::<S, EffectError>(
        store,
        state,
        authority,
        occurred_at_ms,
        TerminalEvent::CanonicalResultRecorded { result }.into(),
    )
}

/// Records the separately trusted repository profile that seeds a strict
/// execution. Profile acquisition is an adapter precondition, not an effect
/// performed by this runner.
///
/// This entry point accepts only a pristine revision-zero aggregate, verifies
/// the profile against the current repository revision and signed validation
/// policy, and authority-fenced CAS-appends exactly one profile event.
pub(crate) fn initialize_strict_profile<S>(
    store: &mut S,
    profile: RepositoryProfile,
    occurred_at_ms: u64,
) -> Result<RunnerStep, RunnerError<S::Error, std::convert::Infallible>>
where
    S: CasEventStore,
{
    let loaded = load_replayed_state::<S, std::convert::Infallible>(store)?;
    if let Some(step) = suppression_for_authority(&loaded.authority) {
        return Ok(step);
    }
    if loaded.unresolved_effect.is_some() {
        return Err(RunnerError::OutboxInvariant {
            code: "bootstrap_profile_with_unresolved_effect",
        });
    }
    if loaded.state.aggregate_revision != 0
        || loaded.state.stage() != ProtocolStage::Profiling
        || loaded.state.repository_profile.is_some()
    {
        return Err(RunnerError::InvalidLoadedStream {
            code: "bootstrap_profile_requires_pristine_revision_zero",
        });
    }
    profile
        .validate()
        .map_err(ProtocolViolation::from)
        .map_err(RunnerError::Protocol)?;
    if profile.repository_revision != loaded.state.repository_revision {
        return Err(RunnerError::Protocol(
            ProtocolViolation::RepositoryProfile {
                code: "bootstrap_profile_repository_revision_mismatch",
            },
        ));
    }
    loaded
        .state
        .validation_policy
        .as_ref()
        .ok_or(RunnerError::Protocol(
            ProtocolViolation::ValidationContract {
                code: "strict_v1_validation_policy_missing",
            },
        ))?
        .validate(&profile)
        .map_err(ProtocolViolation::from)
        .map_err(RunnerError::Protocol)?;

    persist_reducer_event::<S, std::convert::Infallible>(
        store,
        &loaded.state,
        &loaded.authority,
        occurred_at_ms,
        ProfileEvent::RepositoryProfileRecorded { profile }.into(),
    )
}

/// Recovery entry point for a worker that wants to process only a previously
/// persisted intent. A definite "not performed" reconciliation is immediately
/// followed by one attempt; an indeterminate reconciliation leaves the outbox
/// unresolved.
pub(crate) fn reconcile_or_resume_pending_effect<S, E>(
    store: &mut S,
    executor: &mut E,
) -> Result<Option<RunnerStep>, RunnerError<S::Error, E::Error>>
where
    S: EffectOutbox,
    E: EffectExecutor,
{
    let loaded = load_replayed_state::<S, E::Error>(store)?;
    if let Some(step) = suppression_for_authority(&loaded.authority) {
        return Ok(Some(step));
    }
    let Some(intent) = loaded.unresolved_effect else {
        return Ok(None);
    };
    let request =
        validate_pending_intent::<S::Error, E::Error>(&loaded.state, &loaded.authority, &intent)?;
    reconcile_or_resume(
        store,
        executor,
        &loaded.state,
        &loaded.authority,
        intent,
        request,
    )
    .map(Some)
}

struct LoadedRunnerState {
    state: ExecutionState,
    authority: ExecutionAttemptAuthorityFence,
    unresolved_effect: Option<EffectIntent>,
}

fn load_replayed_state<S, EffectError>(
    store: &mut S,
) -> Result<LoadedRunnerState, RunnerError<S::Error, EffectError>>
where
    S: CasEventStore,
{
    let loaded = store_result(store.load_event_stream())?;
    let expected_revision = loaded.stream_revision;
    loaded
        .trusted_bootstrap
        .validate_strict_bootstrap_contract()
        .map_err(RunnerError::Protocol)?;
    let mut state = loaded.trusted_bootstrap;
    for event in loaded.events {
        state = reduce_strict_v1(&state, event).map_err(RunnerError::Protocol)?;
    }
    if state.aggregate_revision != expected_revision {
        return Err(RunnerError::InvalidLoadedStream {
            code: "stream_revision_does_not_match_replay",
        });
    }
    loaded
        .authority
        .validate_for(&state)
        .map_err(|code| RunnerError::InvalidLoadedStream { code })?;
    Ok(LoadedRunnerState {
        state,
        authority: loaded.authority,
        unresolved_effect: loaded.unresolved_effect,
    })
}

fn persist_reducer_event<S, EffectError>(
    store: &mut S,
    state: &ExecutionState,
    authority: &ExecutionAttemptAuthorityFence,
    occurred_at_ms: u64,
    event: DomainEvent,
) -> Result<RunnerStep, RunnerError<S::Error, EffectError>>
where
    S: CasEventStore,
{
    let context =
        runner_event_context(state, &event, last_event_id(state)).map_err(RunnerError::Protocol)?;
    let envelope = ProtocolEventEnvelope::new_with_context(
        state,
        &format!(
            "execution-protocol-v1:runner-decision:{}",
            state.aggregate_revision
        ),
        occurred_at_ms,
        context,
        event,
    )
    .map_err(RunnerError::Protocol)?;
    reduce_strict_v1(state, envelope.clone()).map_err(RunnerError::Protocol)?;
    let event_id = envelope.event_id.clone();
    match store_result(store.append_event_cas(state.aggregate_revision, authority, envelope))? {
        CasOutcome::Committed(outcome) => {
            validate_append_outcome::<S::Error, EffectError>(state, outcome)?;
            Ok(RunnerStep::EventPersisted { event_id, outcome })
        }
        CasOutcome::Conflict { expected, actual } => Ok(RunnerStep::ReloadRequired {
            expected_revision: expected,
            actual_revision: actual,
        }),
        CasOutcome::AuthorityRejected { actual } => {
            authority_rejection_step::<S::Error, EffectError>(authority, actual)
        }
    }
}

fn persist_and_perform<S, E>(
    store: &mut S,
    executor: &mut E,
    state: &ExecutionState,
    authority: &ExecutionAttemptAuthorityFence,
    effect: EffectRequest,
) -> Result<RunnerStep, RunnerError<S::Error, E::Error>>
where
    S: EffectOutbox,
    E: EffectExecutor,
{
    match decide_strict_v1(state).map_err(RunnerError::Protocol)? {
        ProtocolDecision::Perform { effect: expected } if expected == effect => {}
        _ => {
            return Err(RunnerError::OutboxInvariant {
                code: "effect_request_is_not_current_reducer_decision",
            });
        }
    }
    let intent = EffectIntent::new(state, authority, &effect).map_err(RunnerError::Protocol)?;
    match store_result(store.persist_effect_intent_cas(
        state.aggregate_revision,
        authority,
        &intent,
    ))? {
        CasOutcome::Conflict { expected, actual } => Ok(RunnerStep::ReloadRequired {
            expected_revision: expected,
            actual_revision: actual,
        }),
        CasOutcome::Committed(IntentPersistOutcome::Inserted) => {
            perform_and_commit(store, executor, state, authority, intent, &effect, false)
        }
        CasOutcome::Committed(IntentPersistOutcome::Existing(existing)) => {
            if existing != intent {
                return Err(RunnerError::OutboxInvariant {
                    code: "existing_effect_intent_mismatch",
                });
            }
            reconcile_or_resume(store, executor, state, authority, existing, effect)
        }
        CasOutcome::AuthorityRejected { actual } => {
            authority_rejection_step::<S::Error, E::Error>(authority, actual)
        }
    }
}

fn reconcile_or_resume<S, E>(
    store: &mut S,
    executor: &mut E,
    state: &ExecutionState,
    authority: &ExecutionAttemptAuthorityFence,
    intent: EffectIntent,
    request: EffectRequest,
) -> Result<RunnerStep, RunnerError<S::Error, E::Error>>
where
    S: EffectOutbox,
    E: EffectExecutor,
{
    match executor.reconcile(&intent, authority, &request) {
        AuthorizedEffectOutcome::Boundary(BoundaryOutcome::Definite(Ok(Some(observation)))) => {
            commit_observation::<S, E>(store, state, authority, &intent, observation, true)
        }
        AuthorizedEffectOutcome::Boundary(BoundaryOutcome::Definite(Ok(None))) => {
            perform_and_commit(store, executor, state, authority, intent, &request, true)
        }
        AuthorizedEffectOutcome::Boundary(BoundaryOutcome::Definite(Err(error))) => {
            Err(RunnerError::EffectDefinite {
                intent_id: intent.intent_id,
                error,
            })
        }
        AuthorizedEffectOutcome::Boundary(BoundaryOutcome::Indeterminate(error)) => {
            Err(RunnerError::EffectIndeterminate {
                intent_id: intent.intent_id,
                error,
            })
        }
        AuthorizedEffectOutcome::AuthorityRejected { actual } => {
            authority_rejection_step::<S::Error, E::Error>(authority, actual)
        }
    }
}

fn perform_and_commit<S, E>(
    store: &mut S,
    executor: &mut E,
    state: &ExecutionState,
    authority: &ExecutionAttemptAuthorityFence,
    intent: EffectIntent,
    request: &EffectRequest,
    reconciled: bool,
) -> Result<RunnerStep, RunnerError<S::Error, E::Error>>
where
    S: EffectOutbox,
    E: EffectExecutor,
{
    match executor.perform(&intent, authority, request) {
        AuthorizedEffectOutcome::Boundary(BoundaryOutcome::Definite(Ok(observation))) => {
            commit_observation::<S, E>(store, state, authority, &intent, observation, reconciled)
        }
        AuthorizedEffectOutcome::Boundary(BoundaryOutcome::Definite(Err(error))) => {
            Err(RunnerError::EffectDefinite {
                intent_id: intent.intent_id,
                error,
            })
        }
        AuthorizedEffectOutcome::Boundary(BoundaryOutcome::Indeterminate(error)) => {
            Err(RunnerError::EffectIndeterminate {
                intent_id: intent.intent_id,
                error,
            })
        }
        AuthorizedEffectOutcome::AuthorityRejected { actual } => {
            authority_rejection_step::<S::Error, E::Error>(authority, actual)
        }
    }
}

fn commit_observation<S, E>(
    store: &mut S,
    state: &ExecutionState,
    authority: &ExecutionAttemptAuthorityFence,
    intent: &EffectIntent,
    observation: EffectObservation,
    reconciled: bool,
) -> Result<RunnerStep, RunnerError<S::Error, E::Error>>
where
    S: EffectOutbox,
    E: EffectExecutor,
{
    if last_event_id(state).as_ref() != Some(&intent.triggering_event_id) {
        return Err(RunnerError::OutboxInvariant {
            code: "effect_observation_triggering_event_mismatch",
        });
    }
    let context = runner_effect_observation_context(state, &observation.event, intent)
        .map_err(RunnerError::Protocol)?;
    let envelope = ProtocolEventEnvelope::new_with_context(
        state,
        EFFECT_OBSERVATION_SEMANTIC_KEY,
        observation.occurred_at_ms,
        context,
        observation.event,
    )
    .map_err(RunnerError::Protocol)?;
    if !intent
        .matches_observation_event(&envelope)
        .map_err(RunnerError::Protocol)?
    {
        return Err(RunnerError::OutboxInvariant {
            code: "effect_observation_binding_mismatch",
        });
    }
    reduce_strict_v1(state, envelope.clone()).map_err(RunnerError::Protocol)?;
    let event_id = envelope.event_id.clone();
    match store_result(store.commit_effect_observation_cas(
        state.aggregate_revision,
        authority,
        intent,
        envelope,
    ))? {
        CasOutcome::Committed(outcome) => {
            validate_append_outcome::<S::Error, E::Error>(state, outcome)?;
            Ok(RunnerStep::EffectObservationPersisted {
                intent_id: intent.intent_id.clone(),
                event_id,
                outcome,
                reconciled,
            })
        }
        CasOutcome::Conflict { expected, actual } => Ok(RunnerStep::ReloadRequired {
            expected_revision: expected,
            actual_revision: actual,
        }),
        CasOutcome::AuthorityRejected { actual } => {
            authority_rejection_step::<S::Error, E::Error>(authority, actual)
        }
    }
}

fn validate_pending_intent<StoreError, EffectError>(
    state: &ExecutionState,
    authority: &ExecutionAttemptAuthorityFence,
    intent: &EffectIntent,
) -> Result<EffectRequest, RunnerError<StoreError, EffectError>> {
    let ProtocolDecision::Perform { effect } =
        decide_strict_v1(state).map_err(RunnerError::Protocol)?
    else {
        return Err(RunnerError::OutboxInvariant {
            code: "pending_effect_request_does_not_match_reducer_decision",
        });
    };
    if !intent
        .matches_state_and_request(state, authority, &effect)
        .map_err(RunnerError::Protocol)?
    {
        return Err(RunnerError::OutboxInvariant {
            code: "pending_effect_intent_does_not_match_replayed_state",
        });
    }
    Ok(effect)
}

fn last_event_id(state: &ExecutionState) -> Option<EventId> {
    state
        .event_log
        .last()
        .map(|stored| stored.envelope.event_id.clone())
}

fn runner_event_context(
    state: &ExecutionState,
    event: &DomainEvent,
    causation_id: Option<EventId>,
) -> Result<ProtocolEventContext, ProtocolViolation> {
    let correlation_id = state.event_log.first().map_or_else(
        || CorrelationId::for_execution(&state.execution_id, state.execution_attempt),
        |stored| stored.envelope.correlation_id.clone(),
    );
    ProtocolEventContext::new(
        causation_id,
        correlation_id,
        runner_event_node_owner(state, event),
    )
}

fn runner_effect_observation_context(
    state: &ExecutionState,
    event: &DomainEvent,
    intent: &EffectIntent,
) -> Result<ProtocolEventContext, ProtocolViolation> {
    let correlation_id = state.event_log.first().map_or_else(
        || CorrelationId::for_execution(&state.execution_id, state.execution_attempt),
        |stored| stored.envelope.correlation_id.clone(),
    );
    ProtocolEventContext::for_effect_observation(
        intent.triggering_event_id.clone(),
        correlation_id,
        runner_event_node_owner(state, event),
        intent.observation_binding()?,
    )
}

fn runner_event_node_owner(state: &ExecutionState, event: &DomainEvent) -> Option<NodeId> {
    match event {
        DomainEvent::Graph(event) => match event {
            GraphEvent::NodesAdded { .. } => None,
            GraphEvent::ValidationRepairNodeAdded { node, .. } => Some(node.id.clone()),
            GraphEvent::NodeStarted { node_id, .. }
            | GraphEvent::NodeWaiting { node_id, .. }
            | GraphEvent::NodeResumed { node_id, .. }
            | GraphEvent::NodeSucceeded { node_id, .. }
            | GraphEvent::NodeFailed { node_id, .. } => Some(node_id.clone()),
        },
        DomainEvent::Budget(BudgetEvent::ModelCallAdmitted { admission }) => {
            Some(admission.node_id.clone())
        }
        DomainEvent::Budget(BudgetEvent::ModelCallReserved { call_id })
        | DomainEvent::Budget(BudgetEvent::ProviderDispatchStarted { call_id, .. })
        | DomainEvent::Budget(BudgetEvent::ModelCallReconciled { call_id, .. }) => state
            .budgets
            .model_calls
            .get(call_id)
            .map(|record| record.admission.node_id.clone()),
        DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof })
            if proof.node_ids.len() == 1 =>
        {
            proof.node_ids.first().cloned()
        }
        DomainEvent::Profile(_)
        | DomainEvent::Evidence(_)
        | DomainEvent::Lifecycle(_)
        | DomainEvent::Terminal(_) => None,
        DomainEvent::Discovery(_)
        | DomainEvent::Planning(_)
        | DomainEvent::Implementation(_)
        | DomainEvent::Mutation(_)
        | DomainEvent::Validation(_)
        | DomainEvent::Review(_)
        | DomainEvent::Publication(_) => state.active_node().map(|node| node.id.clone()),
    }
}

fn suppression_for_authority(authority: &ExecutionAttemptAuthorityFence) -> Option<RunnerStep> {
    let reason = if authority.lease_status == LeaseAuthorityStatus::ConfirmedLost {
        WriteSuppressionReason::ConfirmedLeaseLoss
    } else if authority.cancellation_status != CancellationAuthorityStatus::Active {
        WriteSuppressionReason::CancellationRequiresReducerAuthority
    } else {
        return None;
    };
    Some(RunnerStep::WriteSuppressed {
        reason,
        authority: authority.clone(),
    })
}

fn authority_rejection_step<StoreError, EffectError>(
    expected: &ExecutionAttemptAuthorityFence,
    actual: ExecutionAttemptAuthorityFence,
) -> Result<RunnerStep, RunnerError<StoreError, EffectError>> {
    actual
        .validate_shape()
        .map_err(|_| RunnerError::InvalidLoadedStream {
            code: "authority_rejection_fence_invalid",
        })?;
    if actual.execution_id != expected.execution_id
        || actual.execution_attempt != expected.execution_attempt
    {
        return Err(RunnerError::OutboxInvariant {
            code: "authority_rejection_execution_attempt_mismatch",
        });
    }
    if let Some(step) = suppression_for_authority(&actual) {
        return Ok(step);
    }
    if &actual == expected {
        return Err(RunnerError::OutboxInvariant {
            code: "authority_rejected_matching_fence",
        });
    }
    Ok(RunnerStep::WriteSuppressed {
        reason: WriteSuppressionReason::AuthorityFenceChanged,
        authority: actual,
    })
}

fn validate_append_outcome<StoreError, EffectError>(
    state: &ExecutionState,
    outcome: AppendOutcome,
) -> Result<(), RunnerError<StoreError, EffectError>> {
    let expected_revision = state.aggregate_revision.saturating_add(1);
    let actual_revision = match outcome {
        AppendOutcome::Applied { revision } | AppendOutcome::IdempotentReplay { revision } => {
            revision
        }
    };
    if actual_revision != expected_revision {
        return Err(RunnerError::InvalidLoadedStream {
            code: "store_append_returned_unexpected_revision",
        });
    }
    Ok(())
}

fn store_result<T, StoreError, EffectError>(
    outcome: BoundaryOutcome<T, StoreError>,
) -> Result<T, RunnerError<StoreError, EffectError>> {
    match outcome {
        BoundaryOutcome::Definite(Ok(value)) => Ok(value),
        BoundaryOutcome::Definite(Err(error)) => Err(RunnerError::StoreDefinite(error)),
        BoundaryOutcome::Indeterminate(error) => Err(RunnerError::StoreIndeterminate(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::rc::Rc;

    use super::super::{
        DiscoveryActionConstraints, DiscoveryCriterionId, DiscoveryEffectRequest, DiscoveryEvent,
        DiscoveryGoal, EvidenceId, FinalizationPolicyV1, MissionBudgetContract,
        ModelCallReconciliation, NodeBudgetContract, PlanGraphBudgetContract,
        PublicationContractV1, PublicationModeV1, RepositoryFileObservation, RepositoryInventory,
        SearchEvidence, ValidationCommandAuthorization, ValidationCommandKind, ValidationGateClass,
        ValidationParserKind, ValidationPolicyV1, build_repository_profile,
    };
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum FakeStoreError {
        InvalidEvent,
        IntentMismatch,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum FakeEffectError {
        Ambiguous,
    }

    struct FakeStore {
        bootstrap: ExecutionState,
        events: Vec<ProtocolEventEnvelope>,
        pending: Option<EffectIntent>,
        authority: ExecutionAttemptAuthorityFence,
        reject_next_cas_with: Option<ExecutionAttemptAuthorityFence>,
        calls: Rc<RefCell<Vec<&'static str>>>,
    }

    impl FakeStore {
        fn new(bootstrap: ExecutionState, calls: Rc<RefCell<Vec<&'static str>>>) -> Self {
            let authority = authority(&bootstrap);
            Self {
                bootstrap,
                events: Vec::new(),
                pending: None,
                authority,
                reject_next_cas_with: None,
                calls,
            }
        }

        fn replayed(&self) -> Result<ExecutionState, FakeStoreError> {
            let mut state = self.bootstrap.clone();
            for event in &self.events {
                state = reduce_strict_v1(&state, event.clone())
                    .map_err(|_| FakeStoreError::InvalidEvent)?;
            }
            Ok(state)
        }

        fn check_authority<T>(
            &mut self,
            expected: &ExecutionAttemptAuthorityFence,
        ) -> Option<BoundaryOutcome<CasOutcome<T>, FakeStoreError>> {
            if let Some(actual) = self.reject_next_cas_with.take() {
                self.authority = actual.clone();
                return Some(BoundaryOutcome::Definite(Ok(
                    CasOutcome::AuthorityRejected { actual },
                )));
            }
            if expected != &self.authority {
                return Some(BoundaryOutcome::Definite(Ok(
                    CasOutcome::AuthorityRejected {
                        actual: self.authority.clone(),
                    },
                )));
            }
            None
        }

        fn append_at(
            &mut self,
            expected_revision: u64,
            event: ProtocolEventEnvelope,
        ) -> Result<CasOutcome<AppendOutcome>, FakeStoreError> {
            let state = self.replayed()?;
            let actual = state.aggregate_revision;
            if actual != expected_revision {
                return Ok(CasOutcome::Conflict {
                    expected: expected_revision,
                    actual,
                });
            }
            reduce_strict_v1(&state, event.clone()).map_err(|_| FakeStoreError::InvalidEvent)?;
            self.events.push(event);
            Ok(CasOutcome::Committed(AppendOutcome::Applied {
                revision: actual.saturating_add(1),
            }))
        }
    }

    impl CasEventStore for FakeStore {
        type Error = FakeStoreError;

        fn load_event_stream(&mut self) -> BoundaryOutcome<LoadedEventStream, Self::Error> {
            self.calls.borrow_mut().push("load_events");
            BoundaryOutcome::Definite(Ok(LoadedEventStream {
                trusted_bootstrap: self.bootstrap.clone(),
                events: self.events.clone(),
                stream_revision: u64::try_from(self.events.len())
                    .expect("test event count fits u64"),
                authority: self.authority.clone(),
                unresolved_effect: self.pending.clone(),
            }))
        }

        fn append_event_cas(
            &mut self,
            expected_revision: u64,
            authority: &ExecutionAttemptAuthorityFence,
            event: ProtocolEventEnvelope,
        ) -> BoundaryOutcome<CasOutcome<AppendOutcome>, Self::Error> {
            self.calls.borrow_mut().push("append_event");
            if let Some(rejected) = self.check_authority(authority) {
                return rejected;
            }
            BoundaryOutcome::Definite(self.append_at(expected_revision, event))
        }
    }

    impl EffectOutbox for FakeStore {
        fn persist_effect_intent_cas(
            &mut self,
            expected_revision: u64,
            authority: &ExecutionAttemptAuthorityFence,
            intent: &EffectIntent,
        ) -> BoundaryOutcome<CasOutcome<IntentPersistOutcome>, Self::Error> {
            self.calls.borrow_mut().push("persist_intent");
            if let Some(rejected) = self.check_authority(authority) {
                return rejected;
            }
            let actual = match self.replayed() {
                Ok(state) => state.aggregate_revision,
                Err(error) => return BoundaryOutcome::Definite(Err(error)),
            };
            if actual != expected_revision {
                return BoundaryOutcome::Definite(Ok(CasOutcome::Conflict {
                    expected: expected_revision,
                    actual,
                }));
            }
            if let Some(existing) = &self.pending {
                if existing != intent {
                    return BoundaryOutcome::Definite(Err(FakeStoreError::IntentMismatch));
                }
                return BoundaryOutcome::Definite(Ok(CasOutcome::Committed(
                    IntentPersistOutcome::Existing(existing.clone()),
                )));
            }
            self.pending = Some(intent.clone());
            BoundaryOutcome::Definite(Ok(CasOutcome::Committed(IntentPersistOutcome::Inserted)))
        }

        fn commit_effect_observation_cas(
            &mut self,
            expected_revision: u64,
            authority: &ExecutionAttemptAuthorityFence,
            intent: &EffectIntent,
            event: ProtocolEventEnvelope,
        ) -> BoundaryOutcome<CasOutcome<AppendOutcome>, Self::Error> {
            self.calls.borrow_mut().push("commit_observation");
            if let Some(rejected) = self.check_authority(authority) {
                return rejected;
            }
            if self.pending.as_ref() != Some(intent) {
                return BoundaryOutcome::Definite(Err(FakeStoreError::IntentMismatch));
            }
            if !matches!(intent.matches_observation_event(&event), Ok(true)) {
                return BoundaryOutcome::Definite(Err(FakeStoreError::IntentMismatch));
            }
            match self.append_at(expected_revision, event) {
                Ok(CasOutcome::Committed(outcome)) => {
                    self.pending = None;
                    BoundaryOutcome::Definite(Ok(CasOutcome::Committed(outcome)))
                }
                Ok(conflict @ CasOutcome::Conflict { .. }) => {
                    BoundaryOutcome::Definite(Ok(conflict))
                }
                Ok(rejected @ CasOutcome::AuthorityRejected { .. }) => {
                    BoundaryOutcome::Definite(Ok(rejected))
                }
                Err(error) => BoundaryOutcome::Definite(Err(error)),
            }
        }
    }

    struct FakeExecutor {
        perform: VecDeque<AuthorizedEffectOutcome<EffectObservation, FakeEffectError>>,
        reconcile: VecDeque<AuthorizedEffectOutcome<Option<EffectObservation>, FakeEffectError>>,
        calls: Rc<RefCell<Vec<&'static str>>>,
        authority_calls: Vec<(&'static str, ExecutionAttemptAuthorityFence)>,
    }

    impl FakeExecutor {
        fn new(calls: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                perform: VecDeque::new(),
                reconcile: VecDeque::new(),
                calls,
                authority_calls: Vec::new(),
            }
        }
    }

    impl EffectExecutor for FakeExecutor {
        type Error = FakeEffectError;

        fn perform(
            &mut self,
            _intent: &EffectIntent,
            current_authority: &ExecutionAttemptAuthorityFence,
            _request: &EffectRequest,
        ) -> AuthorizedEffectOutcome<EffectObservation, Self::Error> {
            self.calls.borrow_mut().push("perform");
            self.authority_calls
                .push(("perform", current_authority.clone()));
            self.perform
                .pop_front()
                .expect("test configured a perform outcome")
        }

        fn reconcile(
            &mut self,
            _intent: &EffectIntent,
            current_authority: &ExecutionAttemptAuthorityFence,
            _request: &EffectRequest,
        ) -> AuthorizedEffectOutcome<Option<EffectObservation>, Self::Error> {
            self.calls.borrow_mut().push("reconcile");
            self.authority_calls
                .push(("reconcile", current_authority.clone()));
            self.reconcile
                .pop_front()
                .expect("test configured a reconciliation outcome")
        }
    }

    fn model_budget(max_model_calls: u32) -> NodeBudgetContract {
        NodeBudgetContract {
            max_model_calls,
            max_cost_micros: 10_000,
            max_duration_ms: 10_000,
            max_mutation_attempts: 3,
            max_context_rebuilds: 2,
            max_input_tokens_per_call: 4_096,
            max_output_tokens_per_call: 2_048,
        }
    }

    fn hash(label: &str) -> String {
        stable_sha256(&["execution-protocol-v1:runner-test", label])
    }

    fn profile_and_strict_bootstrap() -> (RepositoryProfile, ExecutionState) {
        let repository_revision = RepositoryRevisionId::new("repository-revision:runner-test");
        let inventory = RepositoryInventory::new(
            repository_revision.clone(),
            vec![RepositoryFileObservation::from_bytes(
                "Cargo.toml",
                b"[package]\nname = \"runner-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            )
            .expect("captured Cargo manifest")],
        )
        .expect("bounded repository inventory");
        let profile = build_repository_profile(&inventory).expect("canonical repository profile");
        let candidate = profile
            .validation_candidates
            .iter()
            .find(|candidate| candidate.command == ValidationCommandKind::CargoTest)
            .expect("Cargo profile exposes cargo test");
        let validation_policy = ValidationPolicyV1::new(
            EvidenceId::new("policy-evidence:runner-validation"),
            &profile,
            vec![ValidationCommandAuthorization {
                candidate_id: candidate.candidate_id.clone(),
                gate_class: ValidationGateClass::TestSuite,
                parser: ValidationParserKind::Cargo,
                timeout_ms: 30_000,
                output_limit_bytes: 4_096,
                max_runs: 1,
                environment_fingerprint: hash("environment"),
                dependency_fingerprint: hash("dependencies"),
            }],
            BTreeSet::new(),
            model_budget(1),
            1,
            Vec::new(),
        )
        .expect("strict validation policy");
        let publication = PublicationContractV1::new(
            PublicationModeV1::Normal,
            hash("repository-binding"),
            hash("installation-binding"),
            repository_revision.clone(),
            "refs/heads/main".into(),
            "refs/heads/rustgrid/runner-test".into(),
            Some("1".repeat(40)),
            hash("commit-identity"),
            1,
            1,
            1,
        )
        .expect("publication contract");
        let finalization_policy = FinalizationPolicyV1::new(
            EvidenceId::new("policy-evidence:runner-finalization"),
            8,
            4,
            8 * 1024,
            32 * 1024,
            1,
            BTreeMap::new(),
            publication,
        )
        .expect("strict finalization policy");
        let state = ExecutionState::bootstrap_strict_v1(
            ExecutionId::new("execution-protocol-v1:runner-test"),
            1,
            repository_revision,
            MissionBudgetContract {
                max_model_calls: 8,
                max_cost_micros: 100_000,
                max_duration_ms: 100_000,
            },
            model_budget(1),
            model_budget(3),
            PlanGraphBudgetContract {
                max_implementation_nodes: 8,
                max_validation_nodes: 4,
                max_total_nodes: 15,
                implementation: model_budget(3),
                validation: NodeBudgetContract::deterministic(),
                review: model_budget(1),
                completion_evaluation: model_budget(1),
                publication: NodeBudgetContract::deterministic(),
            },
            DiscoveryGoal::new(
                hash("requested-goal"),
                BTreeSet::from([DiscoveryCriterionId::new("criterion:runner")
                    .expect("valid discovery criterion")]),
                ["runner behavior".to_owned()],
            )
            .expect("trusted discovery goal"),
            validation_policy,
            finalization_policy,
        )
        .expect("strict Protocol v1 bootstrap");
        (profile, state)
    }

    fn authority(state: &ExecutionState) -> ExecutionAttemptAuthorityFence {
        ExecutionAttemptAuthorityFence::new(
            state.execution_id.clone(),
            state.execution_attempt,
            hash("lease-epoch"),
            LeaseAuthorityStatus::Held,
            0,
            CancellationAuthorityStatus::Active,
        )
        .expect("valid authority fence")
    }

    fn store_with_profile(calls: Rc<RefCell<Vec<&'static str>>>) -> (FakeStore, ExecutionState) {
        let (profile, bootstrap) = profile_and_strict_bootstrap();
        let mut store = FakeStore::new(bootstrap.clone(), calls);
        let event: DomainEvent = ProfileEvent::RepositoryProfileRecorded { profile }.into();
        let context = runner_event_context(&bootstrap, &event, None).expect("profile context");
        let envelope = ProtocolEventEnvelope::new_with_context(
            &bootstrap,
            "runner-test:profile",
            1,
            context,
            event,
        )
        .expect("profile envelope");
        store.events.push(envelope.clone());
        let state = reduce_strict_v1(&bootstrap, envelope).expect("profile event reduces");
        (store, state)
    }

    fn store_ready_for_discovery(
        calls: Rc<RefCell<Vec<&'static str>>>,
    ) -> (FakeStore, ExecutionState) {
        let (mut store, mut state) = store_with_profile(calls);
        for index in 0_u64..3 {
            let ProtocolDecision::Emit { event } =
                decide_strict_v1(&state).expect("strict profiling decision")
            else {
                panic!("profiling decision {index} must emit canonical progress");
            };
            let context = runner_event_context(&state, &event, last_event_id(&state))
                .expect("profiling event context");
            let envelope = ProtocolEventEnvelope::new_with_context(
                &state,
                &format!("runner-test:profiling:{index}"),
                2 + index,
                context,
                event,
            )
            .expect("profiling event envelope");
            store.events.push(envelope.clone());
            state = reduce_strict_v1(&state, envelope).expect("profiling event reduces");
        }
        assert_eq!(state.stage(), ProtocolStage::Discovery);
        (store, state)
    }

    fn append_domain(
        store: &mut FakeStore,
        state: &mut ExecutionState,
        semantic_key: &str,
        occurred_at_ms: u64,
        event: DomainEvent,
    ) {
        let context = runner_event_context(state, &event, last_event_id(state))
            .expect("runner test event context");
        let envelope = ProtocolEventEnvelope::new_with_context(
            state,
            semantic_key,
            occurred_at_ms,
            context,
            event,
        )
        .expect("runner test event envelope");
        *state = reduce_strict_v1(state, envelope.clone()).expect("runner test event reduces");
        store.events.push(envelope);
    }

    fn store_at_first_effect(
        calls: Rc<RefCell<Vec<&'static str>>>,
    ) -> (FakeStore, ExecutionState, EffectRequest) {
        let (mut store, mut state) = store_ready_for_discovery(calls);
        for index in 0_u64..8 {
            match decide_strict_v1(&state).expect("strict discovery decision") {
                ProtocolDecision::Emit { event } => append_domain(
                    &mut store,
                    &mut state,
                    &format!("runner-test:effect-prelude:{index}"),
                    10 + index,
                    event,
                ),
                ProtocolDecision::Perform { effect } => return (store, state, effect),
                decision => panic!("unexpected pre-effect decision: {decision:?}"),
            }
        }
        panic!("strict discovery did not converge on a bounded first effect")
    }

    fn store_at_preterminal_finish(
        calls: Rc<RefCell<Vec<&'static str>>>,
    ) -> (FakeStore, ExecutionState, CanonicalResult) {
        let (mut store, mut state, effect) = store_at_first_effect(calls);
        let EffectRequest::Discovery(DiscoveryEffectRequest::DispatchProvider { envelope }) =
            effect
        else {
            panic!("first discovery effect must dispatch its provider envelope");
        };
        let prepared = state
            .current_discovery_action
            .clone()
            .expect("first discovery action is prepared");
        assert_eq!(*envelope, prepared.envelope);
        append_domain(
            &mut store,
            &mut state,
            "runner-test:provider-dispatched",
            20,
            BudgetEvent::ProviderDispatchStarted {
                call_id: prepared.admission.call_id.clone(),
                payload_hash: prepared.envelope.payload_identity.clone(),
            }
            .into(),
        );
        append_domain(
            &mut store,
            &mut state,
            "runner-test:provider-reconciled",
            21,
            BudgetEvent::ModelCallReconciled {
                call_id: prepared.admission.call_id.clone(),
                result: ModelCallReconciliation::Consumed {
                    actual_cost_micros: 1,
                    duration_ms: 1,
                },
            }
            .into(),
        );
        let DiscoveryActionConstraints::Search { request } = &prepared.envelope.constraints else {
            panic!("first discovery action must be a bounded repository search");
        };
        let evidence = SearchEvidence::new(
            prepared.admission.node_id.clone(),
            request.clone(),
            BTreeSet::new(),
            false,
        )
        .expect("empty search evidence is canonical");
        append_domain(
            &mut store,
            &mut state,
            "runner-test:empty-search",
            22,
            DiscoveryEvent::SearchCompleted {
                action_id: prepared.envelope.action_id,
                evidence,
            }
            .into(),
        );
        for (index, expected) in ["convergence", "node-failed"].into_iter().enumerate() {
            let ProtocolDecision::Emit { event } =
                decide_strict_v1(&state).expect("terminal discovery decision")
            else {
                panic!("{expected} must be a reducer-owned event");
            };
            append_domain(
                &mut store,
                &mut state,
                &format!("runner-test:{expected}"),
                23 + u64::try_from(index).expect("bounded index"),
                event,
            );
        }
        let ProtocolDecision::Finish { result } =
            decide_strict_v1(&state).expect("failed discovery terminal decision")
        else {
            panic!("failed discovery must yield a canonical Finish decision");
        };
        (store, state, result)
    }

    fn compatibility_bootstrap() -> ExecutionState {
        ExecutionState::bootstrap(
            ExecutionId::new("execution-protocol-v1:runner-compatibility"),
            1,
            RepositoryRevisionId::new("repository-revision:runner-compatibility"),
            MissionBudgetContract {
                max_model_calls: 8,
                max_cost_micros: 100_000,
                max_duration_ms: 100_000,
            },
            model_budget(3),
            model_budget(3),
            PlanGraphBudgetContract {
                max_implementation_nodes: 8,
                max_validation_nodes: 4,
                max_total_nodes: 15,
                implementation: model_budget(3),
                validation: NodeBudgetContract::deterministic(),
                review: model_budget(1),
                completion_evaluation: model_budget(1),
                publication: NodeBudgetContract::deterministic(),
            },
            None,
        )
    }

    fn provider_dispatched_observation(state: &ExecutionState) -> EffectObservation {
        let prepared = state
            .current_discovery_action
            .as_ref()
            .expect("discovery action is prepared");
        EffectObservation {
            occurred_at_ms: 10,
            event: BudgetEvent::ProviderDispatchStarted {
                call_id: prepared.admission.call_id.clone(),
                payload_hash: prepared.envelope.payload_identity.clone(),
            }
            .into(),
        }
    }

    #[test]
    fn strict_runner_requires_explicit_profile_initialization() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (profile, bootstrap) = profile_and_strict_bootstrap();
        let mut store = FakeStore::new(bootstrap, calls.clone());
        let mut executor = FakeExecutor::new(calls.clone());

        assert!(matches!(
            run_once(&mut store, &mut executor, 10).expect("runner defers"),
            RunnerStep::BootstrapProfileRequired { .. }
        ));
        assert_eq!(calls.borrow().as_slice(), ["load_events"]);

        let step = initialize_strict_profile(&mut store, profile, 11)
            .expect("trusted profile commits at revision zero");
        assert!(matches!(
            step,
            RunnerStep::EventPersisted {
                outcome: AppendOutcome::Applied { revision: 1 },
                ..
            }
        ));
        assert_eq!(store.events.len(), 1);
        assert_eq!(store.events[0].causation_id, None);

        for expected_revision in 2..=4 {
            let step = run_once(&mut store, &mut executor, 10 + expected_revision)
                .expect("reducer owns strict profiling progress");
            assert!(matches!(
                step,
                RunnerStep::EventPersisted {
                    outcome: AppendOutcome::Applied { revision },
                    ..
                } if revision == expected_revision
            ));
        }
        let discovery_state = store.replayed().expect("profiling events replay");
        assert_eq!(discovery_state.stage(), ProtocolStage::Discovery);

        let start = run_once(&mut store, &mut executor, 20)
            .expect("the next reducer decision starts Discovery");
        assert!(matches!(
            start,
            RunnerStep::EventPersisted {
                outcome: AppendOutcome::Applied { revision: 5 },
                ..
            }
        ));
        assert!(matches!(
            store.events.last().map(|event| &event.payload),
            Some(DomainEvent::Graph(GraphEvent::NodeStarted { node_id, .. }))
                if node_id == &NodeId::new("protocol-v1:discovery")
        ));
    }

    #[test]
    fn compatibility_bootstrap_cannot_drive_runner() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut store = FakeStore::new(compatibility_bootstrap(), calls.clone());
        let mut executor = FakeExecutor::new(calls);

        assert!(matches!(
            run_once(&mut store, &mut executor, 10),
            Err(RunnerError::Protocol(ProtocolViolation::Invariant {
                code: "strict_v1_bootstrap_required",
                ..
            }))
        ));
    }

    #[test]
    fn compatibility_envelope_keeps_effect_observation_binding_explicitly_absent() {
        let state = compatibility_bootstrap();
        let (profile, _) = profile_and_strict_bootstrap();
        let event = ProtocolEventEnvelope::new_legacy_test_compatible(
            &state,
            "runner-test:compatibility-profile",
            10,
            ProfileEvent::RepositoryProfileRecorded { profile },
        )
        .expect("compatibility envelope remains supported");

        assert_eq!(event.effect_observation, None);
        assert_eq!(
            serde_json::to_value(event)
                .expect("compatibility envelope serializes")
                .get("effect_observation"),
            Some(&serde_json::Value::Null)
        );
    }

    #[test]
    fn confirmed_lease_loss_and_cancellation_suppress_before_effects() {
        for (lease_status, cancellation_status, expected_reason) in [
            (
                LeaseAuthorityStatus::ConfirmedLost,
                CancellationAuthorityStatus::Active,
                WriteSuppressionReason::ConfirmedLeaseLoss,
            ),
            (
                LeaseAuthorityStatus::Held,
                CancellationAuthorityStatus::Requested,
                WriteSuppressionReason::CancellationRequiresReducerAuthority,
            ),
        ] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let (_profile, bootstrap) = profile_and_strict_bootstrap();
            let mut store = FakeStore::new(bootstrap, calls.clone());
            store.authority.lease_status = lease_status;
            store.authority.cancellation_status = cancellation_status;
            store.authority.cancellation_revision = 3;
            let mut executor = FakeExecutor::new(calls.clone());

            assert!(matches!(
                run_once(&mut store, &mut executor, 10).expect("write is suppressed"),
                RunnerStep::WriteSuppressed { reason, .. } if reason == expected_reason
            ));
            assert_eq!(calls.borrow().as_slice(), ["load_events"]);
            assert!(store.events.is_empty());
        }
    }

    #[test]
    fn intent_is_fenced_causal_and_persists_only_safe_request_identity() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (mut store, state, effect) = store_at_first_effect(calls.clone());
        let expected_authority = store.authority.clone();
        let triggering_event_id = last_event_id(&state).expect("profile event triggers effect");
        let mut executor = FakeExecutor::new(calls.clone());
        executor
            .perform
            .push_back(AuthorizedEffectOutcome::Boundary(
                BoundaryOutcome::Indeterminate(FakeEffectError::Ambiguous),
            ));

        assert!(matches!(
            persist_and_perform(
                &mut store,
                &mut executor,
                &state,
                &expected_authority,
                effect,
            ),
            Err(RunnerError::EffectIndeterminate { .. })
        ));
        let intent = store.pending.as_ref().expect("intent remains durable");
        assert_eq!(intent.authority, expected_authority);
        assert_eq!(intent.triggering_event_id, triggering_event_id);
        assert_eq!(intent.repository_revision, state.repository_revision);
        assert!(is_sha256(&intent.request_identity.digest));
        let persisted = serde_json::to_string(intent).expect("intent has durable wire form");
        assert!(persisted.contains("request_identity"));
        assert!(!persisted.contains("grounded_evidence_missing"));
        assert_eq!(calls.borrow().as_slice(), ["persist_intent", "perform"]);
    }

    #[test]
    fn authority_rejection_before_effect_is_write_suppressed() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (mut store, state, effect) = store_at_first_effect(calls.clone());
        let expected_authority = store.authority.clone();
        let mut lost = expected_authority.clone();
        lost.lease_status = LeaseAuthorityStatus::ConfirmedLost;
        store.reject_next_cas_with = Some(lost);
        let mut executor = FakeExecutor::new(calls.clone());

        let step = persist_and_perform(
            &mut store,
            &mut executor,
            &state,
            &expected_authority,
            effect,
        )
        .expect("confirmed loss suppresses the write");
        assert!(matches!(
            step,
            RunnerStep::WriteSuppressed {
                reason: WriteSuppressionReason::ConfirmedLeaseLoss,
                ..
            }
        ));
        assert_eq!(calls.borrow().as_slice(), ["persist_intent"]);
        assert!(store.pending.is_none());
    }

    #[test]
    fn cross_stream_authority_rejection_is_boundary_corruption() {
        for mismatch in ["execution", "attempt"] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let (mut store, state, effect) = store_at_first_effect(calls.clone());
            let expected_authority = store.authority.clone();
            let mut cross_stream = expected_authority.clone();
            match mismatch {
                "execution" => {
                    cross_stream.execution_id =
                        ExecutionId::new("execution-protocol-v1:wrong-execution")
                }
                "attempt" => cross_stream.execution_attempt += 1,
                _ => unreachable!("the test enumerates every mismatch"),
            }
            store.reject_next_cas_with = Some(cross_stream);
            let mut executor = FakeExecutor::new(calls.clone());

            assert!(matches!(
                persist_and_perform(
                    &mut store,
                    &mut executor,
                    &state,
                    &expected_authority,
                    effect,
                ),
                Err(RunnerError::OutboxInvariant {
                    code: "authority_rejection_execution_attempt_mismatch"
                })
            ));
            assert_eq!(calls.borrow().as_slice(), ["persist_intent"]);
            assert!(store.pending.is_none());
            assert!(executor.authority_calls.is_empty());
        }
    }

    #[test]
    fn indeterminate_effect_is_reconciled_without_blind_reexecution() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (mut store, state, request) = store_at_first_effect(calls.clone());
        let expected_authority = store.authority.clone();
        let mut first_executor = FakeExecutor::new(calls.clone());
        first_executor
            .perform
            .push_back(AuthorizedEffectOutcome::Boundary(
                BoundaryOutcome::Indeterminate(FakeEffectError::Ambiguous),
            ));

        let first = persist_and_perform(
            &mut store,
            &mut first_executor,
            &state,
            &expected_authority,
            request.clone(),
        );
        assert!(matches!(
            first,
            Err(RunnerError::EffectIndeterminate { .. })
        ));
        let persisted_intent = store.pending.clone().expect("intent remains durable");

        let mut recovery_executor = FakeExecutor::new(calls.clone());
        recovery_executor
            .reconcile
            .push_back(AuthorizedEffectOutcome::Boundary(
                BoundaryOutcome::Definite(Ok(Some(provider_dispatched_observation(&state)))),
            ));
        let recovered = reconcile_or_resume_pending_effect(&mut store, &mut recovery_executor)
            .expect("recovery loads a strict aggregate")
            .expect("pending intent is reconciled");

        assert!(matches!(
            recovered,
            RunnerStep::EffectObservationPersisted {
                reconciled: true,
                outcome: AppendOutcome::Applied { revision: 9 },
                ..
            }
        ));
        assert_eq!(
            calls
                .borrow()
                .iter()
                .filter(|call| **call == "perform")
                .count(),
            1
        );
        assert!(store.pending.is_none());
        assert_eq!(store.events.len(), 9);
        assert_eq!(
            store.events[8].causation_id,
            Some(store.events[7].event_id.clone())
        );
        assert_eq!(
            store.events[8].semantic_key,
            "execution-protocol-v1:effect-observation"
        );
        assert_eq!(
            store.events[8].effect_observation,
            Some(
                persisted_intent
                    .observation_binding()
                    .expect("persisted intent has a valid replay binding")
            )
        );
    }

    #[test]
    fn durable_store_rejects_observation_with_mismatched_intent_or_request_digest() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (mut store, state, request) = store_at_first_effect(calls);
        let authority = store.authority.clone();
        let intent = EffectIntent::new(&state, &authority, &request).expect("valid intent");
        store.pending = Some(intent.clone());
        let observation = provider_dispatched_observation(&state);
        let correlation_id = state
            .event_log
            .first()
            .expect("nonempty event stream")
            .envelope
            .correlation_id
            .clone();
        let events_before = store.events.len();

        for wrong_binding in [
            EffectObservationBinding::new(
                EffectId::new("effect:another-intent"),
                intent.request_identity.digest.clone(),
            )
            .expect("well-formed mismatched intent id"),
            EffectObservationBinding::new(intent.intent_id.clone(), "b".repeat(64))
                .expect("well-formed mismatched request digest"),
        ] {
            let context = ProtocolEventContext::for_effect_observation(
                intent.triggering_event_id.clone(),
                correlation_id.clone(),
                runner_event_node_owner(&state, &observation.event),
                wrong_binding,
            )
            .expect("well-formed observation context");
            let envelope = ProtocolEventEnvelope::new_with_context(
                &state,
                EFFECT_OBSERVATION_SEMANTIC_KEY,
                observation.occurred_at_ms,
                context,
                observation.event.clone(),
            )
            .expect("well-formed but mismatched observation event");

            assert_eq!(
                store.commit_effect_observation_cas(
                    state.aggregate_revision,
                    &authority,
                    &intent,
                    envelope,
                ),
                BoundaryOutcome::Definite(Err(FakeStoreError::IntentMismatch))
            );
        }
        assert_eq!(store.pending, Some(intent));
        assert_eq!(store.events.len(), events_before);
    }

    #[test]
    fn effect_intent_requires_complete_observation_envelope_association() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (store, state, request) = store_at_first_effect(calls);
        let intent = EffectIntent::new(&state, &store.authority, &request).expect("valid intent");
        let observation = provider_dispatched_observation(&state);
        let context = runner_effect_observation_context(&state, &observation.event, &intent)
            .expect("valid bound observation context");
        let valid = ProtocolEventEnvelope::new_with_context(
            &state,
            EFFECT_OBSERVATION_SEMANTIC_KEY,
            observation.occurred_at_ms,
            context,
            observation.event,
        )
        .expect("valid bound observation event");
        assert!(
            intent
                .matches_observation_event(&valid)
                .expect("valid event shape")
        );

        let mut wrong_cause = valid.clone();
        wrong_cause.causation_id = Some(EventId::new("event:another-trigger"));
        let mut wrong_execution = valid.clone();
        wrong_execution.execution_id = ExecutionId::new("execution-protocol-v1:another");
        let mut wrong_attempt = valid.clone();
        wrong_attempt.execution_attempt = wrong_attempt.execution_attempt.saturating_add(1);
        let mut wrong_revision = valid.clone();
        wrong_revision.aggregate_revision_before =
            wrong_revision.aggregate_revision_before.saturating_add(1);
        let mut wrong_sequence = valid.clone();
        wrong_sequence.sequence = wrong_sequence.sequence.saturating_add(1);
        let mut wrong_repository = valid.clone();
        wrong_repository.repository_revision =
            RepositoryRevisionId::new("repository-revision:another");
        let mut wrong_protocol_schema = valid.clone();
        wrong_protocol_schema.protocol_version = EXECUTION_PROTOCOL_VERSION.saturating_add(1);
        let mut wrong_event_schema = valid.clone();
        wrong_event_schema.event_schema_version = PROTOCOL_EVENT_SCHEMA_VERSION.saturating_add(1);

        for mismatched in [
            wrong_cause,
            wrong_execution,
            wrong_attempt,
            wrong_revision,
            wrong_sequence,
            wrong_repository,
            wrong_protocol_schema,
            wrong_event_schema,
        ] {
            assert!(
                !intent
                    .matches_observation_event(&mismatched)
                    .expect("mismatched event remains structurally inspectable")
            );
        }
    }

    #[test]
    fn rotated_held_authority_reconciles_before_reexecution() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (mut store, state, request) = store_at_first_effect(calls.clone());
        let original_authority = store.authority.clone();
        let mut first_executor = FakeExecutor::new(calls.clone());
        first_executor
            .perform
            .push_back(AuthorizedEffectOutcome::Boundary(
                BoundaryOutcome::Indeterminate(FakeEffectError::Ambiguous),
            ));

        assert!(matches!(
            persist_and_perform(
                &mut store,
                &mut first_executor,
                &state,
                &original_authority,
                request,
            ),
            Err(RunnerError::EffectIndeterminate { .. })
        ));
        let persisted_intent = store.pending.clone().expect("intent remains durable");
        assert_eq!(persisted_intent.authority, original_authority);

        let mut rotated_authority = original_authority.clone();
        rotated_authority.lease_epoch_hash = hash("rotated-lease-epoch");
        store.authority = rotated_authority.clone();
        let events_before_recovery = store.events.len();
        let recovery_call_start = calls.borrow().len();
        let mut recovery_executor = FakeExecutor::new(calls.clone());
        recovery_executor
            .reconcile
            .push_back(AuthorizedEffectOutcome::Boundary(
                BoundaryOutcome::Definite(Ok(None)),
            ));
        recovery_executor
            .perform
            .push_back(AuthorizedEffectOutcome::Boundary(
                BoundaryOutcome::Definite(Ok(provider_dispatched_observation(&state))),
            ));

        let recovered = reconcile_or_resume_pending_effect(&mut store, &mut recovery_executor)
            .expect("rotated same-attempt authority is valid")
            .expect("the pending intent is resumed");

        assert!(matches!(
            recovered,
            RunnerStep::EffectObservationPersisted {
                reconciled: true,
                outcome: AppendOutcome::Applied { revision: 9 },
                ..
            }
        ));
        assert_eq!(
            &calls.borrow()[recovery_call_start..],
            ["load_events", "reconcile", "perform", "commit_observation"]
        );
        assert_eq!(
            recovery_executor.authority_calls,
            vec![
                ("reconcile", rotated_authority.clone()),
                ("perform", rotated_authority.clone()),
            ]
        );
        assert_ne!(persisted_intent.authority, rotated_authority);
        assert!(store.pending.is_none());
        assert_eq!(store.events.len(), events_before_recovery + 1);
    }

    #[test]
    fn finish_decision_persists_terminal_truth_before_reporting_finished() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (mut store, state, result) = store_at_preterminal_finish(calls.clone());
        let expected_authority = store.authority.clone();

        let first = persist_finish_decision::<_, FakeEffectError>(
            &mut store,
            &state,
            &expected_authority,
            20,
            result.clone(),
        )
        .expect("terminal result is authority-fenced and persisted");
        assert!(matches!(
            first,
            RunnerStep::EventPersisted {
                outcome: AppendOutcome::Applied { revision: 14 },
                ..
            }
        ));
        assert!(matches!(
            store.events.last().map(|event| &event.payload),
            Some(DomainEvent::Terminal(
                TerminalEvent::CanonicalResultRecorded { .. }
            ))
        ));

        let terminal_state = store.replayed().expect("terminal event replays");
        let calls_before = calls.borrow().len();
        let second = persist_finish_decision::<_, FakeEffectError>(
            &mut store,
            &terminal_state,
            &expected_authority,
            21,
            result.clone(),
        )
        .expect("recorded terminal truth may now finish");
        assert_eq!(second, RunnerStep::Finished { result });
        assert_eq!(calls.borrow().len(), calls_before);
    }

    #[test]
    fn authority_fence_rejects_noncanonical_sha256() {
        let (_profile, bootstrap) = profile_and_strict_bootstrap();
        assert_eq!(
            ExecutionAttemptAuthorityFence::new(
                bootstrap.execution_id,
                bootstrap.execution_attempt,
                "A".repeat(64),
                LeaseAuthorityStatus::Held,
                0,
                CancellationAuthorityStatus::Active,
            ),
            Err(AuthorityFenceError::Invalid {
                field: "lease_epoch_hash"
            })
        );
    }
}
