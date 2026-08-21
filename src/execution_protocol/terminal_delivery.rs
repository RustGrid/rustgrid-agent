//! Durable, post-terminal callback delivery projection.
//!
//! This module deliberately owns no [`super::ExecutionState`] and no
//! [`CanonicalResult`]. It starts from the final committed terminal event and
//! persists only cryptographic references to that immutable domain result.
//! Callback delivery therefore cannot revise mission outcome, process health,
//! cancellation authority, lease authority, or repository state.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use super::{
    CanonicalResult, DomainEvent, EXECUTION_PROTOCOL_VERSION, EventId, ExecutionId, ExecutionState,
    StoredProtocolEvent, TerminalEvent, stable_sha256,
};

pub(crate) const TERMINAL_DELIVERY_SCHEMA_VERSION_V1: u16 = 1;
pub(crate) const TERMINAL_CALLBACK_PAYLOAD_SCHEMA_VERSION_V1: u16 = 1;
pub(crate) const MAX_TERMINAL_CALLBACK_ATTEMPTS_V1: u16 = 16;
pub(crate) const MAX_TERMINAL_CALLBACK_RECONCILIATIONS_PER_ATTEMPT_V1: u16 = 16;

const MAX_SAFE_CODE_BYTES: usize = 128;
const MAX_OPAQUE_ID_BYTES: usize = 256;

/// A validated SHA-256 value. Persisted callback records contain digests, not
/// callback bodies, response bodies, authorization material, or error detail.
#[derive(Clone, Debug, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct TerminalDeliveryDigestV1(String);

impl TerminalDeliveryDigestV1 {
    fn from_derived(value: String) -> Self {
        debug_assert!(is_lower_hex_sha256(&value));
        Self(value)
    }

    pub(crate) fn new(value: impl Into<String>) -> Result<Self, TerminalDeliveryViolationV1> {
        let value = value.into();
        if !is_lower_hex_sha256(&value) {
            return Err(TerminalDeliveryViolationV1::InvalidDigest);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TerminalDeliveryDigestV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A bounded machine code safe to persist and display. It intentionally does
/// not accept arbitrary transport error text.
#[derive(Clone, Debug, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct TerminalDeliveryCodeV1(String);

impl TerminalDeliveryCodeV1 {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, TerminalDeliveryViolationV1> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= MAX_SAFE_CODE_BYTES
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'-' | b'.' | b':')
            });
        if !valid {
            return Err(TerminalDeliveryViolationV1::InvalidSafeCode);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TerminalDeliveryCodeV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct TerminalDeliveryEventIdV1(String);

impl TerminalDeliveryEventIdV1 {
    fn derive(binding: &TerminalCallbackBindingV1, semantic_key: &str) -> Self {
        Self(format!(
            "tdv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:terminal-delivery-event",
                binding.reference.execution_id.as_str(),
                &binding.reference.execution_attempt.to_string(),
                binding.reference.terminal_event_id.as_str(),
                binding.idempotency_key.as_str(),
                semantic_key,
            ])
        ))
    }

    pub(crate) fn new(value: impl Into<String>) -> Result<Self, TerminalDeliveryViolationV1> {
        let value = value.into();
        if !valid_opaque_id(&value, "tdv1:") {
            return Err(TerminalDeliveryViolationV1::InvalidEventIdentity);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TerminalDeliveryEventIdV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TerminalDeliveryEventIdV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct TerminalCallbackIdempotencyKeyV1(String);

impl TerminalCallbackIdempotencyKeyV1 {
    fn derive(
        reference: &TerminalResultReferenceV1,
        payload_hash: &TerminalDeliveryDigestV1,
    ) -> Self {
        Self(format!(
            "tdcb-v1:{}",
            stable_sha256(&[
                "execution-protocol-v1:terminal-callback-idempotency",
                reference.execution_id.as_str(),
                &reference.execution_attempt.to_string(),
                reference.terminal_event_id.as_str(),
                reference.terminal_event_hash.as_str(),
                reference.canonical_result_hash.as_str(),
                &TERMINAL_CALLBACK_PAYLOAD_SCHEMA_VERSION_V1.to_string(),
                payload_hash.as_str(),
            ])
        ))
    }

    pub(crate) fn new(value: impl Into<String>) -> Result<Self, TerminalDeliveryViolationV1> {
        let value = value.into();
        if !valid_opaque_id(&value, "tdcb-v1:") {
            return Err(TerminalDeliveryViolationV1::InvalidIdempotencyKey);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TerminalCallbackIdempotencyKeyV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TerminalCallbackIdempotencyKeyV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Immutable reference to the exact final domain event and result.
///
/// `terminal_event_hash` binds the complete stored envelope, while
/// `canonical_result_hash` independently binds the domain result within it.
/// Neither the result nor any mutable execution projection is copied here.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalResultReferenceV1 {
    protocol_version: u16,
    execution_id: ExecutionId,
    execution_attempt: u32,
    terminal_event_id: EventId,
    terminal_event_hash: TerminalDeliveryDigestV1,
    canonical_result_hash: TerminalDeliveryDigestV1,
}

impl TerminalResultReferenceV1 {
    /// Enters the delivery domain only from a replay-validated strict terminal
    /// aggregate. The caller remains responsible for obtaining that aggregate
    /// from the authority-fenced durable store; this pure projection cannot
    /// prove physical persistence by itself. A caller-built event slice, even
    /// if internally hash-consistent, is not accepted directly.
    pub(crate) fn from_replay_validated_terminal_state(
        state: &ExecutionState,
    ) -> Result<Self, TerminalDeliveryViolationV1> {
        state.validate_strict_bootstrap_contract().map_err(|_| {
            TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_state_not_strict_v1",
            }
        })?;
        super::validate_state(state).map_err(|_| {
            TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_state_not_replay_validated",
            }
        })?;
        let canonical =
            state
                .terminal
                .as_ref()
                .ok_or(TerminalDeliveryViolationV1::InvalidTerminalSource {
                    code: "canonical_result_missing",
                })?;
        let final_record =
            state
                .event_log
                .last()
                .ok_or(TerminalDeliveryViolationV1::InvalidTerminalSource {
                    code: "terminal_event_missing",
                })?;
        if !matches!(
            &final_record.envelope.payload,
            DomainEvent::Terminal(TerminalEvent::CanonicalResultRecorded { result })
                if result == canonical
        ) {
            return Err(TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_state_event_result_mismatch",
            });
        }
        Self::from_trusted_committed_event_log(&state.event_log)
    }

    /// Hashes an event stream only after its caller has established reducer and
    /// store authority. Production entry points call this solely from
    /// [`Self::from_replay_validated_terminal_state`]. External durability is
    /// established by the runner/store boundary, not inferred by this pure
    /// projection.
    fn from_trusted_committed_event_log(
        event_log: &[StoredProtocolEvent],
    ) -> Result<Self, TerminalDeliveryViolationV1> {
        let Some(final_record) = event_log.last() else {
            return Err(TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_event_missing",
            });
        };
        let terminal_count = event_log
            .iter()
            .filter(|stored| {
                matches!(
                    &stored.envelope.payload,
                    DomainEvent::Terminal(TerminalEvent::CanonicalResultRecorded { .. })
                )
            })
            .count();
        if terminal_count != 1 {
            return Err(TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_event_not_unique",
            });
        }
        let DomainEvent::Terminal(TerminalEvent::CanonicalResultRecorded { result }) =
            &final_record.envelope.payload
        else {
            return Err(TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_event_is_not_final",
            });
        };
        if final_record.envelope.protocol_version != EXECUTION_PROTOCOL_VERSION {
            return Err(TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_protocol_version_mismatch",
            });
        }
        if final_record.envelope.execution_attempt == 0 {
            return Err(TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_execution_attempt_zero",
            });
        }
        if result.repository_revision != final_record.envelope.repository_revision {
            return Err(TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_repository_revision_mismatch",
            });
        }
        if final_record.envelope.expected_event_id().map_err(|_| {
            TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_event_identity_invalid",
            }
        })? != final_record.envelope.event_id
        {
            return Err(TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_event_identity_invalid",
            });
        }
        let terminal_event_hash = final_record.envelope.canonical_hash().map_err(|_| {
            TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_event_serialization_failed",
            }
        })?;
        if terminal_event_hash != final_record.payload_hash {
            return Err(TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_event_hash_mismatch",
            });
        }
        let canonical_result_hash = hash_canonical_result(result)?;
        let reference = Self {
            protocol_version: EXECUTION_PROTOCOL_VERSION,
            execution_id: final_record.envelope.execution_id.clone(),
            execution_attempt: final_record.envelope.execution_attempt,
            terminal_event_id: final_record.envelope.event_id.clone(),
            terminal_event_hash: TerminalDeliveryDigestV1::from_derived(terminal_event_hash),
            canonical_result_hash,
        };
        reference.validate()?;
        Ok(reference)
    }

    pub(crate) const fn execution_id(&self) -> &ExecutionId {
        &self.execution_id
    }

    pub(crate) const fn execution_attempt(&self) -> u32 {
        self.execution_attempt
    }

    pub(crate) const fn terminal_event_id(&self) -> &EventId {
        &self.terminal_event_id
    }

    pub(crate) const fn terminal_event_hash(&self) -> &TerminalDeliveryDigestV1 {
        &self.terminal_event_hash
    }

    pub(crate) const fn canonical_result_hash(&self) -> &TerminalDeliveryDigestV1 {
        &self.canonical_result_hash
    }

    fn validate(&self) -> Result<(), TerminalDeliveryViolationV1> {
        if self.protocol_version != EXECUTION_PROTOCOL_VERSION {
            return Err(TerminalDeliveryViolationV1::UnsupportedSchema {
                field: "protocol_version",
                found: self.protocol_version,
            });
        }
        if self.execution_id.as_str().trim().is_empty() {
            return Err(TerminalDeliveryViolationV1::InvalidBinding {
                field: "execution_id",
            });
        }
        if self.execution_attempt == 0 {
            return Err(TerminalDeliveryViolationV1::InvalidBinding {
                field: "execution_attempt",
            });
        }
        if self.terminal_event_id.as_str().trim().is_empty() {
            return Err(TerminalDeliveryViolationV1::InvalidBinding {
                field: "terminal_event_id",
            });
        }
        Ok(())
    }
}

/// The only callback payload this projection knows how to authorize. It is a
/// versioned collection of safe immutable references, never a free-form body.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SafeTerminalCallbackPayloadV1 {
    pub(crate) payload_schema_version: u16,
    pub(crate) protocol_version: u16,
    pub(crate) execution_id: ExecutionId,
    pub(crate) execution_attempt: u32,
    pub(crate) terminal_event_id: EventId,
    pub(crate) terminal_event_hash: TerminalDeliveryDigestV1,
    pub(crate) canonical_result_hash: TerminalDeliveryDigestV1,
}

impl SafeTerminalCallbackPayloadV1 {
    fn from_reference(reference: &TerminalResultReferenceV1) -> Self {
        Self {
            payload_schema_version: TERMINAL_CALLBACK_PAYLOAD_SCHEMA_VERSION_V1,
            protocol_version: reference.protocol_version,
            execution_id: reference.execution_id.clone(),
            execution_attempt: reference.execution_attempt,
            terminal_event_id: reference.terminal_event_id.clone(),
            terminal_event_hash: reference.terminal_event_hash.clone(),
            canonical_result_hash: reference.canonical_result_hash.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalCallbackBindingV1 {
    reference: TerminalResultReferenceV1,
    callback_payload_schema_version: u16,
    callback_payload_hash: TerminalDeliveryDigestV1,
    idempotency_key: TerminalCallbackIdempotencyKeyV1,
}

impl TerminalCallbackBindingV1 {
    pub(crate) fn from_replay_validated_terminal_state(
        state: &ExecutionState,
    ) -> Result<Self, TerminalDeliveryViolationV1> {
        Self::new(TerminalResultReferenceV1::from_replay_validated_terminal_state(state)?)
    }

    fn new(reference: TerminalResultReferenceV1) -> Result<Self, TerminalDeliveryViolationV1> {
        reference.validate()?;
        let payload = SafeTerminalCallbackPayloadV1::from_reference(&reference);
        let callback_payload_hash = hash_serializable(
            "execution-protocol-v1:safe-terminal-callback-payload",
            &payload,
        )?;
        let idempotency_key =
            TerminalCallbackIdempotencyKeyV1::derive(&reference, &callback_payload_hash);
        let binding = Self {
            reference,
            callback_payload_schema_version: TERMINAL_CALLBACK_PAYLOAD_SCHEMA_VERSION_V1,
            callback_payload_hash,
            idempotency_key,
        };
        binding.validate()?;
        Ok(binding)
    }

    pub(crate) fn safe_payload(&self) -> SafeTerminalCallbackPayloadV1 {
        SafeTerminalCallbackPayloadV1::from_reference(&self.reference)
    }

    pub(crate) const fn reference(&self) -> &TerminalResultReferenceV1 {
        &self.reference
    }

    pub(crate) const fn callback_payload_hash(&self) -> &TerminalDeliveryDigestV1 {
        &self.callback_payload_hash
    }

    pub(crate) const fn idempotency_key(&self) -> &TerminalCallbackIdempotencyKeyV1 {
        &self.idempotency_key
    }

    fn validate(&self) -> Result<(), TerminalDeliveryViolationV1> {
        self.reference.validate()?;
        if self.callback_payload_schema_version != TERMINAL_CALLBACK_PAYLOAD_SCHEMA_VERSION_V1 {
            return Err(TerminalDeliveryViolationV1::UnsupportedSchema {
                field: "callback_payload_schema_version",
                found: self.callback_payload_schema_version,
            });
        }
        let expected_payload_hash = hash_serializable(
            "execution-protocol-v1:safe-terminal-callback-payload",
            &self.safe_payload(),
        )?;
        if self.callback_payload_hash != expected_payload_hash {
            return Err(TerminalDeliveryViolationV1::InvalidBinding {
                field: "callback_payload_hash",
            });
        }
        let expected_key =
            TerminalCallbackIdempotencyKeyV1::derive(&self.reference, &self.callback_payload_hash);
        if self.idempotency_key != expected_key {
            return Err(TerminalDeliveryViolationV1::InvalidBinding {
                field: "idempotency_key",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TerminalDeliveryObservationV1 {
    Acknowledged {
        /// Digest of a safe acknowledgement identity, never a response body.
        acknowledgement_hash: TerminalDeliveryDigestV1,
    },
    DefinitelyFailed {
        code: TerminalDeliveryCodeV1,
        retryable: bool,
    },
    Indeterminate {
        code: TerminalDeliveryCodeV1,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum TerminalDeliveryEventV1 {
    AttemptIntentPersisted {
        attempt: u16,
    },
    DeliveryObserved {
        attempt: u16,
        observation_index: u16,
        observation: TerminalDeliveryObservationV1,
    },
}

impl TerminalDeliveryEventV1 {
    fn semantic_key(&self) -> String {
        match self {
            Self::AttemptIntentPersisted { attempt } => format!("attempt:{attempt}:intent"),
            Self::DeliveryObserved {
                attempt,
                observation_index,
                ..
            } => format!("attempt:{attempt}:observation:{observation_index}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalDeliveryEventEnvelopeV1 {
    pub(crate) delivery_schema_version: u16,
    pub(crate) event_id: TerminalDeliveryEventIdV1,
    pub(crate) sequence: u64,
    pub(crate) projection_revision_before: u64,
    pub(crate) binding: TerminalCallbackBindingV1,
    pub(crate) max_attempts: u16,
    pub(crate) max_reconciliation_observations_per_attempt: u16,
    pub(crate) occurred_at_ms: u64,
    pub(crate) event: TerminalDeliveryEventV1,
}

impl TerminalDeliveryEventEnvelopeV1 {
    fn canonical_hash(&self) -> Result<TerminalDeliveryDigestV1, TerminalDeliveryViolationV1> {
        hash_serializable(
            "execution-protocol-v1:terminal-delivery-stored-envelope",
            self,
        )
    }

    fn expected_event_id(&self) -> TerminalDeliveryEventIdV1 {
        TerminalDeliveryEventIdV1::derive(&self.binding, &self.event.semantic_key())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredTerminalDeliveryEventV1 {
    pub(crate) envelope: TerminalDeliveryEventEnvelopeV1,
    pub(crate) envelope_hash: TerminalDeliveryDigestV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TerminalDeliveryStatusV1 {
    Pending,
    ReadyToSend {
        attempt: u16,
    },
    ReconciliationRequired {
        attempt: u16,
        observations: u16,
    },
    ReconciliationExhausted {
        attempt: u16,
        observations: u16,
        last_code: TerminalDeliveryCodeV1,
    },
    RetryReady {
        next_attempt: u16,
    },
    Acknowledged {
        attempt: u16,
        acknowledgement_hash: TerminalDeliveryDigestV1,
    },
    DefinitelyFailed {
        attempt: u16,
        code: TerminalDeliveryCodeV1,
    },
    RetryExhausted {
        attempts: u16,
        last_code: TerminalDeliveryCodeV1,
    },
}

impl TerminalDeliveryStatusV1 {
    pub(crate) const fn is_settled(&self) -> bool {
        matches!(
            self,
            Self::Acknowledged { .. }
                | Self::DefinitelyFailed { .. }
                | Self::RetryExhausted { .. }
                | Self::ReconciliationExhausted { .. }
        )
    }
}

/// A typed safe request. The executor can serialize `payload`; arbitrary raw
/// callback bodies and credentials are intentionally absent from the contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalCallbackSendEffectV1 {
    attempt: u16,
    payload: SafeTerminalCallbackPayloadV1,
    payload_hash: TerminalDeliveryDigestV1,
    idempotency_key: TerminalCallbackIdempotencyKeyV1,
}

impl TerminalCallbackSendEffectV1 {
    pub(crate) const fn attempt(&self) -> u16 {
        self.attempt
    }

    pub(crate) const fn payload(&self) -> &SafeTerminalCallbackPayloadV1 {
        &self.payload
    }

    pub(crate) const fn payload_hash(&self) -> &TerminalDeliveryDigestV1 {
        &self.payload_hash
    }

    pub(crate) const fn idempotency_key(&self) -> &TerminalCallbackIdempotencyKeyV1 {
        &self.idempotency_key
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalCallbackReconcileEffectV1 {
    attempt: u16,
    payload_hash: TerminalDeliveryDigestV1,
    idempotency_key: TerminalCallbackIdempotencyKeyV1,
}

impl TerminalCallbackReconcileEffectV1 {
    pub(crate) const fn attempt(&self) -> u16 {
        self.attempt
    }

    pub(crate) const fn payload_hash(&self) -> &TerminalDeliveryDigestV1 {
        &self.payload_hash
    }

    pub(crate) const fn idempotency_key(&self) -> &TerminalCallbackIdempotencyKeyV1 {
        &self.idempotency_key
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TerminalDeliveryEffectV1 {
    Send(TerminalCallbackSendEffectV1),
    Reconcile(TerminalCallbackReconcileEffectV1),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TerminalDeliveryDecisionV1 {
    Persist(TerminalDeliveryEventEnvelopeV1),
    Execute(TerminalDeliveryEffectV1),
    Settled(TerminalDeliveryStatusV1),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TerminalDeliveryAppendOutcomeV1 {
    Applied { revision: u64 },
    IdempotentReplay { revision: u64 },
}

/// Strictly serialized delivery journal and replayable projection.
///
/// The projection owns transport events only. Its immutable `binding` is
/// repeated in every event envelope and every append revalidates the complete
/// stream before committing a clone.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalDeliveryProjectionV1 {
    /// Deserialized projections are untrusted until rebound to a separately
    /// replay-validated terminal aggregate.
    #[serde(skip)]
    trusted_terminal_source: bool,
    delivery_schema_version: u16,
    binding: TerminalCallbackBindingV1,
    max_attempts: u16,
    max_reconciliation_observations_per_attempt: u16,
    event_log: Vec<StoredTerminalDeliveryEventV1>,
}

impl TerminalDeliveryProjectionV1 {
    pub(crate) fn from_replay_validated_terminal_state(
        state: &ExecutionState,
        max_attempts: u16,
        max_reconciliation_observations_per_attempt: u16,
    ) -> Result<Self, TerminalDeliveryViolationV1> {
        Self::new_trusted(
            TerminalCallbackBindingV1::from_replay_validated_terminal_state(state)?,
            max_attempts,
            max_reconciliation_observations_per_attempt,
        )
    }

    fn new_trusted(
        binding: TerminalCallbackBindingV1,
        max_attempts: u16,
        max_reconciliation_observations_per_attempt: u16,
    ) -> Result<Self, TerminalDeliveryViolationV1> {
        binding.validate()?;
        validate_limits(max_attempts, max_reconciliation_observations_per_attempt)?;
        Ok(Self {
            trusted_terminal_source: true,
            delivery_schema_version: TERMINAL_DELIVERY_SCHEMA_VERSION_V1,
            binding,
            max_attempts,
            max_reconciliation_observations_per_attempt,
            event_log: Vec::new(),
        })
    }

    /// Restores an untrusted transport snapshot only after independently
    /// re-establishing its binding from a replay-validated strict terminal
    /// aggregate supplied by the durable runner/store boundary.
    pub(crate) fn restore_for_terminal_state(
        state: &ExecutionState,
        trusted_max_attempts: u16,
        trusted_max_reconciliation_observations_per_attempt: u16,
        mut snapshot: Self,
    ) -> Result<Self, TerminalDeliveryViolationV1> {
        validate_limits(
            trusted_max_attempts,
            trusted_max_reconciliation_observations_per_attempt,
        )?;
        let expected_binding =
            TerminalCallbackBindingV1::from_replay_validated_terminal_state(state)?;
        if snapshot.binding != expected_binding {
            return Err(TerminalDeliveryViolationV1::InvalidBinding {
                field: "restored_terminal_binding",
            });
        }
        if snapshot.max_attempts != trusted_max_attempts
            || snapshot.max_reconciliation_observations_per_attempt
                != trusted_max_reconciliation_observations_per_attempt
        {
            return Err(TerminalDeliveryViolationV1::InvalidConfiguration {
                code: "terminal_callback_trusted_limits_mismatch",
            });
        }
        snapshot.trusted_terminal_source = true;
        snapshot.validate_replay()?;
        Ok(snapshot)
    }

    pub(crate) fn from_json_for_replay_validated_terminal_state(
        state: &ExecutionState,
        trusted_max_attempts: u16,
        trusted_max_reconciliation_observations_per_attempt: u16,
        bytes: &[u8],
    ) -> Result<Self, TerminalDeliveryViolationV1> {
        let snapshot: Self = serde_json::from_slice(bytes)
            .map_err(|_| TerminalDeliveryViolationV1::Serialization)?;
        Self::restore_for_terminal_state(
            state,
            trusted_max_attempts,
            trusted_max_reconciliation_observations_per_attempt,
            snapshot,
        )
    }

    pub(crate) fn to_json(&self) -> Result<Vec<u8>, TerminalDeliveryViolationV1> {
        self.validate_replay()?;
        serde_json::to_vec(self).map_err(|_| TerminalDeliveryViolationV1::Serialization)
    }

    pub(crate) const fn binding(&self) -> &TerminalCallbackBindingV1 {
        &self.binding
    }

    pub(crate) fn events(&self) -> &[StoredTerminalDeliveryEventV1] {
        &self.event_log
    }

    fn validate_replay(&self) -> Result<(), TerminalDeliveryViolationV1> {
        self.validate_header()?;
        let mut replay = Self::new_trusted(
            self.binding.clone(),
            self.max_attempts,
            self.max_reconciliation_observations_per_attempt,
        )?;
        for stored in &self.event_log {
            let expected_hash = stored.envelope.canonical_hash()?;
            if stored.envelope_hash != expected_hash {
                return Err(TerminalDeliveryViolationV1::StoredEventTampered {
                    event_id: stored.envelope.event_id.clone(),
                });
            }
            let outcome = replay.append_validated_new(stored.envelope.clone())?;
            debug_assert!(matches!(
                outcome,
                TerminalDeliveryAppendOutcomeV1::Applied { .. }
            ));
        }
        if replay != *self {
            return Err(TerminalDeliveryViolationV1::ProjectionReplayMismatch);
        }
        Ok(())
    }

    pub(crate) fn status(&self) -> Result<TerminalDeliveryStatusV1, TerminalDeliveryViolationV1> {
        self.validate_replay()?;
        self.derive_status()
    }

    /// Returns a persist decision before the first send or any bounded retry.
    /// A send effect is impossible until that exact intent has been appended.
    pub(crate) fn decide(
        &self,
        occurred_at_ms: u64,
    ) -> Result<TerminalDeliveryDecisionV1, TerminalDeliveryViolationV1> {
        self.validate_replay()?;
        let status = self.derive_status()?;
        Ok(match status {
            TerminalDeliveryStatusV1::Pending => {
                TerminalDeliveryDecisionV1::Persist(self.new_envelope(
                    TerminalDeliveryEventV1::AttemptIntentPersisted { attempt: 1 },
                    occurred_at_ms,
                ))
            }
            TerminalDeliveryStatusV1::RetryReady { next_attempt } => {
                TerminalDeliveryDecisionV1::Persist(self.new_envelope(
                    TerminalDeliveryEventV1::AttemptIntentPersisted {
                        attempt: next_attempt,
                    },
                    occurred_at_ms,
                ))
            }
            TerminalDeliveryStatusV1::ReadyToSend { attempt } => {
                TerminalDeliveryDecisionV1::Execute(TerminalDeliveryEffectV1::Send(
                    TerminalCallbackSendEffectV1 {
                        attempt,
                        payload: self.binding.safe_payload(),
                        payload_hash: self.binding.callback_payload_hash.clone(),
                        idempotency_key: self.binding.idempotency_key.clone(),
                    },
                ))
            }
            TerminalDeliveryStatusV1::ReconciliationRequired { attempt, .. } => {
                TerminalDeliveryDecisionV1::Execute(TerminalDeliveryEffectV1::Reconcile(
                    TerminalCallbackReconcileEffectV1 {
                        attempt,
                        payload_hash: self.binding.callback_payload_hash.clone(),
                        idempotency_key: self.binding.idempotency_key.clone(),
                    },
                ))
            }
            settled => TerminalDeliveryDecisionV1::Settled(settled),
        })
    }

    /// Builds the durable observation for the currently sent/reconciled
    /// attempt. The caller must append it before asking for another decision.
    pub(crate) fn observe(
        &self,
        observation: TerminalDeliveryObservationV1,
        occurred_at_ms: u64,
    ) -> Result<TerminalDeliveryEventEnvelopeV1, TerminalDeliveryViolationV1> {
        self.validate_replay()?;
        let (attempt, observation_index) = match self.derive_status()? {
            TerminalDeliveryStatusV1::ReadyToSend { attempt } => (attempt, 1),
            TerminalDeliveryStatusV1::ReconciliationRequired {
                attempt,
                observations,
            } => (attempt, observations.saturating_add(1)),
            _ => {
                return Err(TerminalDeliveryViolationV1::IllegalTransition {
                    code: "observation_without_open_attempt",
                });
            }
        };
        Ok(self.new_envelope(
            TerminalDeliveryEventV1::DeliveryObserved {
                attempt,
                observation_index,
                observation,
            },
            occurred_at_ms,
        ))
    }

    pub(crate) fn append(
        &mut self,
        envelope: TerminalDeliveryEventEnvelopeV1,
    ) -> Result<TerminalDeliveryAppendOutcomeV1, TerminalDeliveryViolationV1> {
        self.validate_replay()?;
        let incoming_hash = envelope.canonical_hash()?;
        if let Some((index, existing)) = self
            .event_log
            .iter()
            .enumerate()
            .find(|(_, stored)| stored.envelope.event_id == envelope.event_id)
        {
            if existing.envelope_hash == incoming_hash && existing.envelope == envelope {
                return Ok(TerminalDeliveryAppendOutcomeV1::IdempotentReplay {
                    revision: u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
                });
            }
            return Err(TerminalDeliveryViolationV1::EventIdentityConflict {
                event_id: envelope.event_id,
            });
        }
        let mut next = self.clone();
        let outcome = next.append_validated_new(envelope)?;
        next.validate_replay()?;
        *self = next;
        Ok(outcome)
    }

    fn validate_header(&self) -> Result<(), TerminalDeliveryViolationV1> {
        if !self.trusted_terminal_source {
            return Err(TerminalDeliveryViolationV1::UntrustedTerminalSource);
        }
        if self.delivery_schema_version != TERMINAL_DELIVERY_SCHEMA_VERSION_V1 {
            return Err(TerminalDeliveryViolationV1::UnsupportedSchema {
                field: "delivery_schema_version",
                found: self.delivery_schema_version,
            });
        }
        self.binding.validate()?;
        validate_limits(
            self.max_attempts,
            self.max_reconciliation_observations_per_attempt,
        )
    }

    fn append_validated_new(
        &mut self,
        envelope: TerminalDeliveryEventEnvelopeV1,
    ) -> Result<TerminalDeliveryAppendOutcomeV1, TerminalDeliveryViolationV1> {
        self.validate_header()?;
        if envelope.delivery_schema_version != TERMINAL_DELIVERY_SCHEMA_VERSION_V1 {
            return Err(TerminalDeliveryViolationV1::UnsupportedSchema {
                field: "event.delivery_schema_version",
                found: envelope.delivery_schema_version,
            });
        }
        if envelope.binding != self.binding {
            return Err(TerminalDeliveryViolationV1::InvalidBinding {
                field: "event.binding",
            });
        }
        if envelope.max_attempts != self.max_attempts
            || envelope.max_reconciliation_observations_per_attempt
                != self.max_reconciliation_observations_per_attempt
        {
            return Err(TerminalDeliveryViolationV1::InvalidConfiguration {
                code: "terminal_callback_event_limits_mismatch",
            });
        }
        if envelope.event_id != envelope.expected_event_id() {
            return Err(TerminalDeliveryViolationV1::InvalidEventIdentity);
        }
        let expected_sequence = u64::try_from(self.event_log.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        if envelope.sequence != expected_sequence {
            return Err(TerminalDeliveryViolationV1::SequenceConflict {
                expected: expected_sequence,
                actual: envelope.sequence,
            });
        }
        let revision = expected_sequence.saturating_sub(1);
        if envelope.projection_revision_before != revision {
            return Err(TerminalDeliveryViolationV1::RevisionConflict {
                expected: revision,
                actual: envelope.projection_revision_before,
            });
        }
        self.validate_next_event(&envelope.event)?;
        let envelope_hash = envelope.canonical_hash()?;
        self.event_log.push(StoredTerminalDeliveryEventV1 {
            envelope,
            envelope_hash,
        });
        Ok(TerminalDeliveryAppendOutcomeV1::Applied {
            revision: expected_sequence,
        })
    }

    fn validate_next_event(
        &self,
        event: &TerminalDeliveryEventV1,
    ) -> Result<(), TerminalDeliveryViolationV1> {
        let status = self.derive_status()?;
        match (status, event) {
            (
                TerminalDeliveryStatusV1::Pending,
                TerminalDeliveryEventV1::AttemptIntentPersisted { attempt: 1 },
            ) => Ok(()),
            (
                TerminalDeliveryStatusV1::RetryReady { next_attempt },
                TerminalDeliveryEventV1::AttemptIntentPersisted { attempt },
            ) if next_attempt == *attempt => Ok(()),
            (
                TerminalDeliveryStatusV1::ReadyToSend {
                    attempt: open_attempt,
                },
                TerminalDeliveryEventV1::DeliveryObserved {
                    attempt,
                    observation_index: 1,
                    ..
                },
            ) if open_attempt == *attempt => Ok(()),
            (
                TerminalDeliveryStatusV1::ReconciliationRequired {
                    attempt: open_attempt,
                    observations,
                },
                TerminalDeliveryEventV1::DeliveryObserved {
                    attempt,
                    observation_index,
                    ..
                },
            ) if open_attempt == *attempt
                && observations.checked_add(1) == Some(*observation_index) =>
            {
                Ok(())
            }
            _ => Err(TerminalDeliveryViolationV1::IllegalTransition {
                code: "terminal_delivery_event_not_authorized",
            }),
        }
    }

    fn derive_status(&self) -> Result<TerminalDeliveryStatusV1, TerminalDeliveryViolationV1> {
        let Some(last_intent) = self.event_log.iter().rev().find_map(|stored| {
            if let TerminalDeliveryEventV1::AttemptIntentPersisted { attempt } =
                stored.envelope.event
            {
                Some(attempt)
            } else {
                None
            }
        }) else {
            return Ok(TerminalDeliveryStatusV1::Pending);
        };
        let observations = self
            .event_log
            .iter()
            .filter_map(|stored| match &stored.envelope.event {
                TerminalDeliveryEventV1::DeliveryObserved {
                    attempt,
                    observation,
                    ..
                } if *attempt == last_intent => Some(observation),
                _ => None,
            })
            .collect::<Vec<_>>();
        let Some(last_observation) = observations.last() else {
            return Ok(TerminalDeliveryStatusV1::ReadyToSend {
                attempt: last_intent,
            });
        };
        match last_observation {
            TerminalDeliveryObservationV1::Acknowledged {
                acknowledgement_hash,
            } => Ok(TerminalDeliveryStatusV1::Acknowledged {
                attempt: last_intent,
                acknowledgement_hash: acknowledgement_hash.clone(),
            }),
            TerminalDeliveryObservationV1::DefinitelyFailed {
                code,
                retryable: false,
            } => Ok(TerminalDeliveryStatusV1::DefinitelyFailed {
                attempt: last_intent,
                code: code.clone(),
            }),
            TerminalDeliveryObservationV1::DefinitelyFailed {
                code,
                retryable: true,
            } if last_intent >= self.max_attempts => Ok(TerminalDeliveryStatusV1::RetryExhausted {
                attempts: last_intent,
                last_code: code.clone(),
            }),
            TerminalDeliveryObservationV1::DefinitelyFailed {
                retryable: true, ..
            } => Ok(TerminalDeliveryStatusV1::RetryReady {
                next_attempt: last_intent.saturating_add(1),
            }),
            TerminalDeliveryObservationV1::Indeterminate { code } => {
                let observation_count = u16::try_from(observations.len()).map_err(|_| {
                    TerminalDeliveryViolationV1::IllegalTransition {
                        code: "observation_index_exhausted",
                    }
                })?;
                let reconciliation_observations = observation_count.saturating_sub(1);
                if reconciliation_observations >= self.max_reconciliation_observations_per_attempt {
                    Ok(TerminalDeliveryStatusV1::ReconciliationExhausted {
                        attempt: last_intent,
                        observations: observation_count,
                        last_code: code.clone(),
                    })
                } else {
                    Ok(TerminalDeliveryStatusV1::ReconciliationRequired {
                        attempt: last_intent,
                        observations: observation_count,
                    })
                }
            }
        }
    }

    fn new_envelope(
        &self,
        event: TerminalDeliveryEventV1,
        occurred_at_ms: u64,
    ) -> TerminalDeliveryEventEnvelopeV1 {
        let sequence = u64::try_from(self.event_log.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let event_id = TerminalDeliveryEventIdV1::derive(&self.binding, &event.semantic_key());
        TerminalDeliveryEventEnvelopeV1 {
            delivery_schema_version: TERMINAL_DELIVERY_SCHEMA_VERSION_V1,
            event_id,
            sequence,
            projection_revision_before: sequence.saturating_sub(1),
            binding: self.binding.clone(),
            max_attempts: self.max_attempts,
            max_reconciliation_observations_per_attempt: self
                .max_reconciliation_observations_per_attempt,
            occurred_at_ms,
            event,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TerminalDeliveryViolationV1 {
    InvalidTerminalSource { code: &'static str },
    UntrustedTerminalSource,
    UnsupportedSchema { field: &'static str, found: u16 },
    InvalidDigest,
    InvalidSafeCode,
    InvalidEventIdentity,
    InvalidIdempotencyKey,
    InvalidBinding { field: &'static str },
    InvalidConfiguration { code: &'static str },
    RevisionConflict { expected: u64, actual: u64 },
    SequenceConflict { expected: u64, actual: u64 },
    EventIdentityConflict { event_id: TerminalDeliveryEventIdV1 },
    StoredEventTampered { event_id: TerminalDeliveryEventIdV1 },
    IllegalTransition { code: &'static str },
    ProjectionReplayMismatch,
    Serialization,
}

impl TerminalDeliveryViolationV1 {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::InvalidTerminalSource { code }
            | Self::InvalidConfiguration { code }
            | Self::IllegalTransition { code } => code,
            Self::UnsupportedSchema { .. } => "terminal_delivery_schema_unsupported",
            Self::UntrustedTerminalSource => "terminal_delivery_source_untrusted",
            Self::InvalidDigest => "terminal_delivery_digest_invalid",
            Self::InvalidSafeCode => "terminal_delivery_safe_code_invalid",
            Self::InvalidEventIdentity => "terminal_delivery_event_identity_invalid",
            Self::InvalidIdempotencyKey => "terminal_callback_idempotency_key_invalid",
            Self::InvalidBinding { .. } => "terminal_callback_binding_invalid",
            Self::RevisionConflict { .. } => "terminal_delivery_revision_conflict",
            Self::SequenceConflict { .. } => "terminal_delivery_sequence_conflict",
            Self::EventIdentityConflict { .. } => "terminal_delivery_event_identity_conflict",
            Self::StoredEventTampered { .. } => "terminal_delivery_stored_event_tampered",
            Self::ProjectionReplayMismatch => "terminal_delivery_projection_replay_mismatch",
            Self::Serialization => "terminal_delivery_serialization_failed",
        }
    }
}

impl fmt::Display for TerminalDeliveryViolationV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTerminalSource { code }
            | Self::InvalidConfiguration { code }
            | Self::IllegalTransition { code } => {
                write!(formatter, "terminal delivery contract violation: {code}")
            }
            Self::UnsupportedSchema { field, found } => {
                write!(formatter, "unsupported terminal delivery {field} `{found}`")
            }
            Self::UntrustedTerminalSource => formatter
                .write_str("terminal delivery snapshot is not bound to a validated terminal state"),
            Self::InvalidDigest => formatter.write_str("terminal delivery digest is invalid"),
            Self::InvalidSafeCode => formatter.write_str("terminal delivery safe code is invalid"),
            Self::InvalidEventIdentity => {
                formatter.write_str("terminal delivery event identity is invalid")
            }
            Self::InvalidIdempotencyKey => {
                formatter.write_str("terminal callback idempotency key is invalid")
            }
            Self::InvalidBinding { field } => {
                write!(formatter, "terminal callback binding `{field}` is invalid")
            }
            Self::RevisionConflict { expected, actual } => write!(
                formatter,
                "terminal delivery revision conflict: expected {expected}, actual {actual}"
            ),
            Self::SequenceConflict { expected, actual } => write!(
                formatter,
                "terminal delivery sequence conflict: expected {expected}, actual {actual}"
            ),
            Self::EventIdentityConflict { event_id } => {
                write!(
                    formatter,
                    "terminal delivery event `{event_id}` changed payload"
                )
            }
            Self::StoredEventTampered { event_id } => {
                write!(
                    formatter,
                    "terminal delivery event `{event_id}` failed hash validation"
                )
            }
            Self::ProjectionReplayMismatch => {
                formatter.write_str("terminal delivery projection diverged from event replay")
            }
            Self::Serialization => formatter.write_str("terminal delivery serialization failed"),
        }
    }
}

impl std::error::Error for TerminalDeliveryViolationV1 {}

fn validate_limits(
    max_attempts: u16,
    max_reconciliation_observations_per_attempt: u16,
) -> Result<(), TerminalDeliveryViolationV1> {
    if max_attempts == 0 || max_attempts > MAX_TERMINAL_CALLBACK_ATTEMPTS_V1 {
        return Err(TerminalDeliveryViolationV1::InvalidConfiguration {
            code: "terminal_callback_attempt_limit_invalid",
        });
    }
    if max_reconciliation_observations_per_attempt == 0
        || max_reconciliation_observations_per_attempt
            > MAX_TERMINAL_CALLBACK_RECONCILIATIONS_PER_ATTEMPT_V1
    {
        return Err(TerminalDeliveryViolationV1::InvalidConfiguration {
            code: "terminal_callback_reconciliation_limit_invalid",
        });
    }
    Ok(())
}

fn hash_canonical_result(
    result: &CanonicalResult,
) -> Result<TerminalDeliveryDigestV1, TerminalDeliveryViolationV1> {
    hash_serializable("execution-protocol-v1:canonical-result", result)
}

fn hash_serializable(
    namespace: &str,
    value: &impl Serialize,
) -> Result<TerminalDeliveryDigestV1, TerminalDeliveryViolationV1> {
    let canonical =
        serde_json::to_string(value).map_err(|_| TerminalDeliveryViolationV1::Serialization)?;
    Ok(TerminalDeliveryDigestV1::from_derived(stable_sha256(&[
        namespace, &canonical,
    ])))
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_opaque_id(value: &str, prefix: &str) -> bool {
    value.len() <= MAX_OPAQUE_ID_BYTES
        && value.strip_prefix(prefix).is_some_and(is_lower_hex_sha256)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::execution_protocol::{
        BudgetEvent, CorrelationId, DiscoveryActionConstraints, DiscoveryCriterionId,
        DiscoveryEffectRequest, DiscoveryEvent, DiscoveryGoal, DomainEvent, EffectRequest,
        EvidenceId, ExecutionProtocolModeV1, ExecutionState, FinalizationPolicyV1, GraphEvent,
        MissionBudgetContract, ModelCallReconciliation, NodeBudgetContract,
        PlanGraphBudgetContract, ProfileEvent, ProtocolDecision, ProtocolEventContext,
        ProtocolEventEnvelope, PublicationContractV1, PublicationModeV1, RepositoryFileObservation,
        RepositoryInventory, RepositoryProfile, RepositoryRevisionId, SearchEvidence,
        ValidationCommandAuthorization, ValidationCommandKind, ValidationGateClass,
        ValidationParserKind, ValidationPolicyV1, build_repository_profile, decide_strict_v1,
        reduce_strict_v1, stable_sha256,
    };

    fn model_budget() -> NodeBudgetContract {
        NodeBudgetContract {
            max_model_calls: 1,
            max_cost_micros: 1_000,
            max_duration_ms: 1_000,
            max_mutation_attempts: 1,
            max_context_rebuilds: 1,
            max_input_tokens_per_call: 1_000,
            max_output_tokens_per_call: 500,
        }
    }

    fn plan_graph_budget() -> PlanGraphBudgetContract {
        PlanGraphBudgetContract {
            max_implementation_nodes: 1,
            max_validation_nodes: 1,
            max_total_nodes: 5,
            implementation: model_budget(),
            validation: NodeBudgetContract::deterministic(),
            review: model_budget(),
            completion_evaluation: model_budget(),
            publication: NodeBudgetContract::deterministic(),
        }
    }

    fn strict_profile_and_bootstrap() -> (RepositoryProfile, ExecutionState) {
        let execution_id = ExecutionId::new("execution:terminal-delivery-test");
        let repository_revision = RepositoryRevisionId::new("repository:test-terminal");
        let inventory = RepositoryInventory::new(
            repository_revision.clone(),
            vec![
                RepositoryFileObservation::from_bytes(
                    "Cargo.toml",
                    b"[package]\nname = \"terminal-delivery-test\"\nversion = \"0.1.0\"\n",
                )
                .expect("bounded terminal fixture"),
            ],
        )
        .expect("terminal repository inventory");
        let profile = build_repository_profile(&inventory).expect("terminal repository profile");
        let candidate = profile
            .validation_candidates
            .iter()
            .find(|candidate| candidate.command == ValidationCommandKind::CargoTest)
            .expect("Cargo profile has cargo test");
        let validation_policy = ValidationPolicyV1::new(
            EvidenceId::new("policy-evidence:terminal-delivery-validation"),
            &profile,
            vec![ValidationCommandAuthorization {
                candidate_id: candidate.candidate_id.clone(),
                gate_class: ValidationGateClass::TestSuite,
                parser: ValidationParserKind::Cargo,
                timeout_ms: 30_000,
                output_limit_bytes: 4_096,
                max_runs: 1,
                environment_fingerprint: stable_sha256(&["terminal-delivery-environment"]),
                dependency_fingerprint: stable_sha256(&["terminal-delivery-dependencies"]),
            }],
            BTreeSet::new(),
            model_budget(),
            1,
            Vec::new(),
        )
        .expect("terminal validation policy");
        let publication = PublicationContractV1::new(
            PublicationModeV1::Normal,
            stable_sha256(&["terminal-delivery-repository-binding"]),
            stable_sha256(&["terminal-delivery-installation-binding"]),
            repository_revision.clone(),
            "refs/heads/main".into(),
            "refs/heads/rustgrid/terminal-delivery".into(),
            None,
            stable_sha256(&["terminal-delivery-commit-identity"]),
            1,
            1,
            1,
        )
        .expect("terminal publication contract");
        let finalization_policy = FinalizationPolicyV1::new(
            EvidenceId::new("policy-evidence:terminal-delivery-finalization"),
            8,
            4,
            8 * 1024,
            32 * 1024,
            1,
            BTreeMap::new(),
            publication,
        )
        .expect("terminal finalization policy");
        let state = ExecutionState::bootstrap_strict_v1(
            execution_id,
            3,
            repository_revision,
            MissionBudgetContract {
                max_model_calls: 4,
                max_cost_micros: 4_000,
                max_duration_ms: 4_000,
            },
            model_budget(),
            model_budget(),
            plan_graph_budget(),
            DiscoveryGoal::new(
                stable_sha256(&["terminal-delivery-goal"]),
                BTreeSet::from([DiscoveryCriterionId::new("criterion:terminal-delivery")
                    .expect("valid terminal criterion")]),
                ["deliver canonical terminal result".to_owned()],
            )
            .expect("terminal discovery goal"),
            validation_policy,
            finalization_policy,
        )
        .expect("strict terminal bootstrap");
        (profile, state)
    }

    fn bootstrap_state() -> ExecutionState {
        strict_profile_and_bootstrap().1
    }

    fn append_reducer_event(
        state: &mut ExecutionState,
        semantic_key: &str,
        occurred_at_ms: u64,
        event: DomainEvent,
    ) {
        let causation_id = state
            .event_log
            .last()
            .map(|stored| stored.envelope.event_id.clone());
        let correlation_id = state.event_log.first().map_or_else(
            || CorrelationId::for_execution(&state.execution_id, state.execution_attempt),
            |stored| stored.envelope.correlation_id.clone(),
        );
        let node_id = match &event {
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
            DomainEvent::Discovery(_) => state.active_node().map(|node| node.id.clone()),
            _ => None,
        };
        let envelope = ProtocolEventEnvelope::new_with_context(
            state,
            semantic_key,
            occurred_at_ms,
            ProtocolEventContext::new(causation_id, correlation_id, node_id)
                .expect("valid reducer event context"),
            event,
        )
        .expect("valid reducer event envelope");
        *state = reduce_strict_v1(state, envelope).expect("reducer-owned event applies");
    }

    fn terminal_state() -> (CanonicalResult, ExecutionState) {
        let (profile, mut state) = strict_profile_and_bootstrap();
        append_reducer_event(
            &mut state,
            "terminal-delivery:profile",
            1,
            ProfileEvent::RepositoryProfileRecorded { profile }.into(),
        );
        for index in 0_u64..12 {
            match decide_strict_v1(&state).expect("strict terminal fixture decision") {
                ProtocolDecision::Emit { event } => append_reducer_event(
                    &mut state,
                    &format!("terminal-delivery:reducer:{index}"),
                    index.saturating_add(2),
                    event,
                ),
                ProtocolDecision::Perform {
                    effect:
                        EffectRequest::Discovery(DiscoveryEffectRequest::DispatchProvider { envelope }),
                } => {
                    let prepared = state
                        .current_discovery_action
                        .clone()
                        .expect("discovery action is prepared");
                    assert_eq!(*envelope, prepared.envelope);
                    append_reducer_event(
                        &mut state,
                        "terminal-delivery:provider-dispatched",
                        20,
                        BudgetEvent::ProviderDispatchStarted {
                            call_id: prepared.admission.call_id.clone(),
                            payload_hash: prepared.envelope.payload_identity.clone(),
                        }
                        .into(),
                    );
                    append_reducer_event(
                        &mut state,
                        "terminal-delivery:provider-reconciled",
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
                    let DiscoveryActionConstraints::Search { request } =
                        &prepared.envelope.constraints
                    else {
                        panic!("first discovery action must be a repository search");
                    };
                    let evidence = SearchEvidence::new(
                        prepared.admission.node_id.clone(),
                        request.clone(),
                        BTreeSet::new(),
                        false,
                    )
                    .expect("empty search evidence is canonical");
                    append_reducer_event(
                        &mut state,
                        "terminal-delivery:empty-search",
                        22,
                        DiscoveryEvent::SearchCompleted {
                            action_id: prepared.envelope.action_id,
                            evidence,
                        }
                        .into(),
                    );
                }
                ProtocolDecision::Finish { result } => {
                    append_reducer_event(
                        &mut state,
                        "terminal-delivery:canonical-result",
                        index.saturating_add(2),
                        TerminalEvent::CanonicalResultRecorded {
                            result: result.clone(),
                        }
                        .into(),
                    );
                    assert_eq!(state.terminal.as_ref(), Some(&result));
                    return (result, state);
                }
                decision => panic!("strict terminal fixture stalled: {decision:?}"),
            }
        }
        panic!("strict terminal fixture did not converge")
    }

    fn compatibility_terminal_state() -> ExecutionState {
        let (_, mut state) = terminal_state();
        state.protocol_mode = ExecutionProtocolModeV1::CompatibilityScaffold;
        state
    }

    fn projection(max_attempts: u16) -> (CanonicalResult, TerminalDeliveryProjectionV1) {
        projection_with_limits(max_attempts, 3)
    }

    fn projection_with_limits(
        max_attempts: u16,
        max_reconciliation_observations_per_attempt: u16,
    ) -> (CanonicalResult, TerminalDeliveryProjectionV1) {
        let (canonical, state) = terminal_state();
        let projection = TerminalDeliveryProjectionV1::from_replay_validated_terminal_state(
            &state,
            max_attempts,
            max_reconciliation_observations_per_attempt,
        )
        .expect("valid delivery projection");
        (canonical, projection)
    }

    fn persist_decision(
        projection: &mut TerminalDeliveryProjectionV1,
        occurred_at_ms: u64,
    ) -> TerminalDeliveryEventEnvelopeV1 {
        let TerminalDeliveryDecisionV1::Persist(intent) = projection
            .decide(occurred_at_ms)
            .expect("delivery decision")
        else {
            panic!("expected persisted intent decision");
        };
        assert!(matches!(
            projection.append(intent.clone()),
            Ok(TerminalDeliveryAppendOutcomeV1::Applied { .. })
        ));
        intent
    }

    fn retryable_failure() -> TerminalDeliveryObservationV1 {
        TerminalDeliveryObservationV1::DefinitelyFailed {
            code: TerminalDeliveryCodeV1::new("callback_transport_unavailable")
                .expect("safe failure code"),
            retryable: true,
        }
    }

    #[test]
    fn canonical_result_is_only_referenced_and_observations_cannot_replace_it() {
        let (canonical, mut delivery) = projection(3);
        let original = canonical.clone();
        persist_decision(&mut delivery, 1);
        let acknowledgement = TerminalDeliveryDigestV1::from_derived(stable_sha256(&[
            "terminal-delivery-test:acknowledgement",
        ]));
        let observed = delivery
            .observe(
                TerminalDeliveryObservationV1::Acknowledged {
                    acknowledgement_hash: acknowledgement,
                },
                2,
            )
            .expect("acknowledgement event");
        delivery.append(observed).expect("persist acknowledgement");

        assert_eq!(canonical, original);
        assert!(matches!(
            delivery.status(),
            Ok(TerminalDeliveryStatusV1::Acknowledged { attempt: 1, .. })
        ));
        let persisted = String::from_utf8(delivery.to_json().expect("serialize projection"))
            .expect("utf-8 JSON");
        assert!(!persisted.contains("authorized_user_cancellation"));
        assert!(!persisted.contains("execution_cancelled"));
        assert!(!persisted.contains("\"mission\""));

        let (_, mut failed_projection) = projection(3);
        persist_decision(&mut failed_projection, 3);
        let failure = failed_projection
            .observe(
                TerminalDeliveryObservationV1::DefinitelyFailed {
                    code: TerminalDeliveryCodeV1::new("callback_rejected")
                        .expect("safe failure code"),
                    retryable: false,
                },
                4,
            )
            .expect("definite failure observation");
        failed_projection
            .append(failure)
            .expect("persist definite failure");
        assert!(matches!(
            failed_projection.status(),
            Ok(TerminalDeliveryStatusV1::DefinitelyFailed { attempt: 1, .. })
        ));
        assert_eq!(canonical, original);
    }

    #[test]
    fn intent_is_persisted_before_a_send_effect_is_authorized() {
        let (_, mut projection) = projection(3);
        let TerminalDeliveryDecisionV1::Persist(intent) =
            projection.decide(10).expect("first decision")
        else {
            panic!("send was authorized before intent persistence");
        };
        assert!(matches!(
            intent.event,
            TerminalDeliveryEventV1::AttemptIntentPersisted { attempt: 1 }
        ));
        projection.append(intent).expect("persist intent");

        let TerminalDeliveryDecisionV1::Execute(TerminalDeliveryEffectV1::Send(effect)) =
            projection.decide(11).expect("second decision")
        else {
            panic!("expected send after intent persistence");
        };
        assert_eq!(effect.attempt, 1);
        assert_eq!(
            effect.payload_hash,
            projection.binding.callback_payload_hash
        );
        assert_eq!(effect.idempotency_key, projection.binding.idempotency_key);
    }

    #[test]
    fn exact_event_replay_is_idempotent() {
        let (_, mut projection) = projection(3);
        let intent = persist_decision(&mut projection, 10);
        let before = projection.clone();
        assert_eq!(
            projection.append(intent),
            Ok(TerminalDeliveryAppendOutcomeV1::IdempotentReplay { revision: 1 })
        );
        assert_eq!(projection, before);
    }

    #[test]
    fn same_event_id_with_different_payload_is_rejected() {
        let (_, mut projection) = projection(3);
        let intent = persist_decision(&mut projection, 10);
        let mut conflicting = intent;
        conflicting.event = TerminalDeliveryEventV1::AttemptIntentPersisted { attempt: 2 };
        assert!(matches!(
            projection.append(conflicting),
            Err(TerminalDeliveryViolationV1::EventIdentityConflict { .. })
        ));
    }

    #[test]
    fn indeterminate_observation_reconciles_same_attempt_and_key() {
        let (_, mut projection) = projection(3);
        persist_decision(&mut projection, 1);
        let indeterminate = projection
            .observe(
                TerminalDeliveryObservationV1::Indeterminate {
                    code: TerminalDeliveryCodeV1::new("callback_response_lost")
                        .expect("safe indeterminate code"),
                },
                2,
            )
            .expect("indeterminate observation");
        projection
            .append(indeterminate)
            .expect("persist indeterminate observation");

        let TerminalDeliveryDecisionV1::Execute(TerminalDeliveryEffectV1::Reconcile(effect)) =
            projection.decide(3).expect("reconciliation decision")
        else {
            panic!("indeterminate delivery allocated a retry");
        };
        assert_eq!(effect.attempt, 1);
        assert_eq!(effect.idempotency_key, projection.binding.idempotency_key);

        let acknowledged = projection
            .observe(
                TerminalDeliveryObservationV1::Acknowledged {
                    acknowledgement_hash: TerminalDeliveryDigestV1::from_derived(stable_sha256(&[
                        "terminal-delivery-test:reconciled",
                    ])),
                },
                4,
            )
            .expect("reconciled acknowledgement");
        projection
            .append(acknowledged)
            .expect("persist reconciled acknowledgement");
        assert!(matches!(
            projection.status(),
            Ok(TerminalDeliveryStatusV1::Acknowledged { attempt: 1, .. })
        ));
    }

    #[test]
    fn retry_attempts_are_bounded_and_exhaustion_is_settled() {
        let (_, mut projection) = projection(2);
        persist_decision(&mut projection, 1);
        let first_failure = projection
            .observe(retryable_failure(), 2)
            .expect("first failure event");
        projection
            .append(first_failure)
            .expect("persist first failure");
        persist_decision(&mut projection, 3);
        let second_failure = projection
            .observe(retryable_failure(), 4)
            .expect("second failure event");
        projection
            .append(second_failure)
            .expect("persist second failure");

        let status = projection.status().expect("delivery status");
        assert_eq!(
            status,
            TerminalDeliveryStatusV1::RetryExhausted {
                attempts: 2,
                last_code: TerminalDeliveryCodeV1::new("callback_transport_unavailable")
                    .expect("safe failure code"),
            }
        );
        assert!(status.is_settled());
        assert_eq!(
            projection.decide(5),
            Ok(TerminalDeliveryDecisionV1::Settled(status))
        );
    }

    #[test]
    fn repeated_indeterminate_reconciliation_is_bounded_and_settled() {
        let (_, mut projection) = projection_with_limits(3, 2);
        persist_decision(&mut projection, 1);
        for occurred_at_ms in [2, 3, 4] {
            let observation = projection
                .observe(
                    TerminalDeliveryObservationV1::Indeterminate {
                        code: TerminalDeliveryCodeV1::new("callback_state_unknown")
                            .expect("safe indeterminate code"),
                    },
                    occurred_at_ms,
                )
                .expect("bounded indeterminate observation");
            projection
                .append(observation)
                .expect("persist indeterminate observation");
        }

        let status = projection.status().expect("delivery status");
        assert_eq!(
            status,
            TerminalDeliveryStatusV1::ReconciliationExhausted {
                attempt: 1,
                observations: 3,
                last_code: TerminalDeliveryCodeV1::new("callback_state_unknown")
                    .expect("safe indeterminate code"),
            }
        );
        assert!(status.is_settled());
        assert_eq!(
            projection.decide(5),
            Ok(TerminalDeliveryDecisionV1::Settled(status))
        );
        assert!(matches!(
            projection.observe(
                TerminalDeliveryObservationV1::Indeterminate {
                    code: TerminalDeliveryCodeV1::new("callback_state_unknown")
                        .expect("safe indeterminate code"),
                },
                6,
            ),
            Err(TerminalDeliveryViolationV1::IllegalTransition {
                code: "observation_without_open_attempt"
            })
        ));
    }

    #[test]
    fn one_reconciliation_observation_limit_allows_one_reconcile_call() {
        let (_, mut projection) = projection_with_limits(3, 1);
        persist_decision(&mut projection, 1);
        let initial = projection
            .observe(
                TerminalDeliveryObservationV1::Indeterminate {
                    code: TerminalDeliveryCodeV1::new("callback_response_lost")
                        .expect("safe indeterminate code"),
                },
                2,
            )
            .expect("initial send observation");
        projection
            .append(initial)
            .expect("persist initial send observation");

        assert!(matches!(
            projection.decide(3),
            Ok(TerminalDeliveryDecisionV1::Execute(
                TerminalDeliveryEffectV1::Reconcile(_)
            ))
        ));
        let reconciliation = projection
            .observe(
                TerminalDeliveryObservationV1::Indeterminate {
                    code: TerminalDeliveryCodeV1::new("callback_state_unknown")
                        .expect("safe indeterminate code"),
                },
                4,
            )
            .expect("one reconciliation observation is authorized");
        projection
            .append(reconciliation)
            .expect("persist reconciliation observation");
        assert!(matches!(
            projection.status(),
            Ok(TerminalDeliveryStatusV1::ReconciliationExhausted {
                attempt: 1,
                observations: 2,
                ..
            })
        ));
    }

    #[test]
    fn production_entry_rejects_nonterminal_and_untrusted_snapshots() {
        let nonterminal = bootstrap_state();
        let rejection =
            TerminalDeliveryProjectionV1::from_replay_validated_terminal_state(&nonterminal, 2, 2)
                .expect_err("nonterminal state must not authorize callback delivery");
        assert!(matches!(
            rejection,
            TerminalDeliveryViolationV1::InvalidTerminalSource { .. }
        ));

        assert_eq!(
            TerminalDeliveryProjectionV1::from_replay_validated_terminal_state(
                &compatibility_terminal_state(),
                2,
                2,
            ),
            Err(TerminalDeliveryViolationV1::InvalidTerminalSource {
                code: "terminal_state_not_strict_v1"
            })
        );

        let (_, projection) = projection(2);
        let encoded = projection.to_json().expect("projection JSON");
        let untrusted: TerminalDeliveryProjectionV1 =
            serde_json::from_slice(&encoded).expect("strict projection shape");
        assert_eq!(
            untrusted.status(),
            Err(TerminalDeliveryViolationV1::UntrustedTerminalSource)
        );
    }

    #[test]
    fn restore_rebinds_exact_terminal_state_and_separately_trusted_limits() {
        let (_, state) = terminal_state();
        let mut projection =
            TerminalDeliveryProjectionV1::from_replay_validated_terminal_state(&state, 2, 3)
                .expect("validated terminal source");
        persist_decision(&mut projection, 1);
        let encoded = projection.to_json().expect("projection JSON");
        let restored = TerminalDeliveryProjectionV1::from_json_for_replay_validated_terminal_state(
            &state, 2, 3, &encoded,
        )
        .expect("rebind persisted projection");
        assert_eq!(restored, projection);

        let mut changed_limits: serde_json::Value =
            serde_json::from_slice(&encoded).expect("projection JSON value");
        changed_limits
            .as_object_mut()
            .expect("projection object")
            .insert("max_attempts".into(), serde_json::json!(3));
        let changed_limits = serde_json::to_vec(&changed_limits).expect("changed projection JSON");
        assert!(matches!(
            TerminalDeliveryProjectionV1::from_json_for_replay_validated_terminal_state(
                &state,
                2,
                3,
                &changed_limits,
            ),
            Err(TerminalDeliveryViolationV1::InvalidConfiguration {
                code: "terminal_callback_trusted_limits_mismatch"
            })
        ));
    }

    #[test]
    fn persisted_projection_rejects_hash_tampering_and_unknown_fields() {
        let (_, mut projection) = projection(2);
        persist_decision(&mut projection, 1);
        let mut tampered = projection.clone();
        tampered.event_log[0].envelope.occurred_at_ms = 999;
        assert!(matches!(
            tampered.validate_replay(),
            Err(TerminalDeliveryViolationV1::StoredEventTampered { .. })
        ));

        let mut value: serde_json::Value =
            serde_json::from_slice(&projection.to_json().expect("projection JSON"))
                .expect("JSON value");
        value
            .as_object_mut()
            .expect("projection object")
            .insert("raw_callback_body".into(), serde_json::json!("secret"));
        let bytes = serde_json::to_vec(&value).expect("tampered JSON");
        assert!(serde_json::from_slice::<TerminalDeliveryProjectionV1>(&bytes).is_err());
    }
}
