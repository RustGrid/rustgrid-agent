use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use super::{
    ActionId, CandidatePathEvidence, CanonicalResult, DiscoveryConvergence, DiscoveryEffectRequest,
    DiscoveryGoal, EffectId, EventId, ExecutionId, FailureRevisionId, FileEvidence,
    ImpactMapEvidence, ImplementationEffectRequest, ImplementationEvent, ModelCallAdmission,
    ModelCallId, ModelCallReconciliation, MutationEffectRequest, MutationEvent, NodeId, NodeSpec,
    PlanCandidate, PlanningActionRejectionReason, PlanningConvergence, PlanningEffectRequest,
    PreparedDiscoveryAction, PreparedPlanningAction, ProofId, ProofRecord, ProtocolStage,
    ProtocolViolation, PublicationEffectRequest, PublicationEvent, RelationshipEvidence,
    RepositoryProfile, RepositoryRevisionId, ReviewEffectRequest, ReviewEvent, SearchEvidence,
    SearchId, UnresolvedQuestion, ValidationEffectRequest, ValidationEvent, stable_sha256,
};

pub(crate) const PROTOCOL_EVENT_SCHEMA_VERSION: u16 = 2;

const MAX_EVENT_IDENTITY_BYTES: usize = 256;

/// Execution-attempt correlation carried by every persisted Protocol v1 event.
///
/// Correlation is wire authority rather than diagnostic decoration: it is
/// serialized, validated, and included in semantic event identity. The first
/// event chooses the correlation ID; all later events in the aggregate must
/// retain it.
#[derive(Clone, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct CorrelationId(String);

impl CorrelationId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, ProtocolViolation> {
        let value = value.into();
        validate_wire_identity("correlation_id", &value)?;
        Ok(Self(value))
    }

    pub(crate) fn for_execution(execution_id: &ExecutionId, execution_attempt: u32) -> Self {
        Self(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:correlation",
                execution_id.as_str(),
                &execution_attempt.to_string(),
            ])
        ))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CorrelationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CorrelationId")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for CorrelationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CorrelationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Durable, non-secret link from a definitely observed effect result to the
/// exact outbox intent whose resolution committed the event.
///
/// The request itself remains outside the event wire. Its safe digest is
/// nevertheless identity-bearing, so replay cannot silently detach an
/// observation from its request or attach it to another intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectObservationBinding {
    pub(crate) effect_intent_id: EffectId,
    pub(crate) request_digest: String,
}

impl EffectObservationBinding {
    pub(crate) fn new(
        effect_intent_id: EffectId,
        request_digest: String,
    ) -> Result<Self, ProtocolViolation> {
        validate_wire_identity("effect_intent_id", effect_intent_id.as_str())?;
        validate_sha256_digest("effect_request_digest", &request_digest)?;
        Ok(Self {
            effect_intent_id,
            request_digest,
        })
    }
}

impl<'de> Deserialize<'de> for EffectObservationBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireBinding {
            effect_intent_id: EffectId,
            request_digest: String,
        }

        let wire = WireBinding::deserialize(deserializer)?;
        Self::new(wire.effect_intent_id, wire.request_digest).map_err(serde::de::Error::custom)
    }
}

/// Explicit causal, correlation, node-ownership, and effect-observation
/// metadata for one domain event.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProtocolEventContext {
    #[serde(deserialize_with = "deserialize_explicit_option")]
    pub(crate) causation_id: Option<EventId>,
    pub(crate) correlation_id: CorrelationId,
    #[serde(deserialize_with = "deserialize_explicit_option")]
    pub(crate) node_id: Option<NodeId>,
    #[serde(deserialize_with = "deserialize_explicit_option")]
    pub(crate) effect_observation: Option<EffectObservationBinding>,
}

impl ProtocolEventContext {
    pub(crate) fn new(
        causation_id: Option<EventId>,
        correlation_id: CorrelationId,
        node_id: Option<NodeId>,
    ) -> Result<Self, ProtocolViolation> {
        if let Some(causation_id) = &causation_id {
            validate_wire_identity("causation_id", causation_id.as_str())?;
        }
        if let Some(node_id) = &node_id {
            validate_wire_identity("node_id", node_id.as_str())?;
        }
        validate_wire_identity("correlation_id", correlation_id.as_str())?;
        Ok(Self {
            causation_id,
            correlation_id,
            node_id,
            effect_observation: None,
        })
    }

    pub(crate) fn for_effect_observation(
        causation_id: EventId,
        correlation_id: CorrelationId,
        node_id: Option<NodeId>,
        binding: EffectObservationBinding,
    ) -> Result<Self, ProtocolViolation> {
        let mut context = Self::new(Some(causation_id), correlation_id, node_id)?;
        context.effect_observation = Some(binding);
        Ok(context)
    }
}

// Event families remain inline so their persisted JSON schema has one stable,
// explicit shape; runtime size is bounded and events are reduced one at a time.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "family",
    content = "event",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum DomainEvent {
    Profile(ProfileEvent),
    Discovery(DiscoveryEvent),
    Planning(PlanningEvent),
    Implementation(ImplementationEvent),
    Mutation(MutationEvent),
    Validation(ValidationEvent),
    Review(ReviewEvent),
    Publication(PublicationEvent),
    Evidence(EvidenceEvent),
    Graph(GraphEvent),
    Budget(BudgetEvent),
    Lifecycle(LifecycleEvent),
    Terminal(TerminalEvent),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ProfileEvent {
    RepositoryProfileRecorded { profile: RepositoryProfile },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum DiscoveryEvent {
    GoalRecorded {
        goal: DiscoveryGoal,
    },
    ActionPrepared {
        prepared: Box<PreparedDiscoveryAction>,
    },
    ActionReleased {
        action_id: ActionId,
    },
    ActionRejected {
        action_id: ActionId,
        reason: super::DiscoveryActionRejectionReason,
    },
    SearchCompleted {
        action_id: ActionId,
        evidence: SearchEvidence,
    },
    CandidatesRecorded {
        search_id: SearchId,
        candidates: Vec<CandidatePathEvidence>,
    },
    FileEvidenceRecorded {
        action_id: ActionId,
        evidence: Vec<FileEvidence>,
        unresolved_questions: Vec<UnresolvedQuestion>,
    },
    RelationshipEvidenceRecorded {
        action_id: ActionId,
        evidence: Vec<RelationshipEvidence>,
    },
    ImpactMapRecorded {
        action_id: Option<ActionId>,
        evidence: ImpactMapEvidence,
    },
    ConvergenceEvaluated {
        convergence: DiscoveryConvergence,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum PlanningEvent {
    ActionPrepared {
        prepared: Box<PreparedPlanningAction>,
    },
    ActionReleased {
        action_id: ActionId,
    },
    ActionRejected {
        action_id: ActionId,
        reason: PlanningActionRejectionReason,
    },
    CandidateRecorded {
        action_id: ActionId,
        call_id: ModelCallId,
        candidate: PlanCandidate,
    },
    ConvergenceEvaluated {
        convergence: PlanningConvergence,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum EvidenceEvent {
    ProofRecorded { proof: ProofRecord },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum GraphEvent {
    NodesAdded {
        plan_proof_id: ProofId,
        nodes: Vec<NodeSpec>,
    },
    ValidationRepairNodeAdded {
        eligibility_proof_id: ProofId,
        node: NodeSpec,
    },
    NodeStarted {
        node_id: NodeId,
        attempt: u32,
    },
    NodeWaiting {
        node_id: NodeId,
        effect_id: super::EffectId,
    },
    NodeResumed {
        node_id: NodeId,
        effect_id: super::EffectId,
    },
    NodeSucceeded {
        node_id: NodeId,
        proof_id: ProofId,
    },
    NodeFailed {
        node_id: NodeId,
        failure_revision_id: FailureRevisionId,
        terminal: bool,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum BudgetEvent {
    ModelCallAdmitted {
        admission: ModelCallAdmission,
    },
    ModelCallReserved {
        call_id: ModelCallId,
    },
    ProviderDispatchStarted {
        call_id: ModelCallId,
        payload_hash: String,
    },
    ModelCallReconciled {
        call_id: ModelCallId,
        result: ModelCallReconciliation,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum LifecycleEvent {
    PositionAdvanced {
        from: ProtocolStage,
        to: ProtocolStage,
        proof_id: ProofId,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum TerminalEvent {
    CanonicalResultRecorded { result: CanonicalResult },
}

impl From<EvidenceEvent> for DomainEvent {
    fn from(event: EvidenceEvent) -> Self {
        Self::Evidence(event)
    }
}

impl From<ProfileEvent> for DomainEvent {
    fn from(event: ProfileEvent) -> Self {
        Self::Profile(event)
    }
}

impl From<DiscoveryEvent> for DomainEvent {
    fn from(event: DiscoveryEvent) -> Self {
        Self::Discovery(event)
    }
}

impl From<PlanningEvent> for DomainEvent {
    fn from(event: PlanningEvent) -> Self {
        Self::Planning(event)
    }
}

impl From<ImplementationEvent> for DomainEvent {
    fn from(event: ImplementationEvent) -> Self {
        Self::Implementation(event)
    }
}

impl From<MutationEvent> for DomainEvent {
    fn from(event: MutationEvent) -> Self {
        Self::Mutation(event)
    }
}

impl From<ValidationEvent> for DomainEvent {
    fn from(event: ValidationEvent) -> Self {
        Self::Validation(event)
    }
}

impl From<ReviewEvent> for DomainEvent {
    fn from(event: ReviewEvent) -> Self {
        Self::Review(event)
    }
}

impl From<PublicationEvent> for DomainEvent {
    fn from(event: PublicationEvent) -> Self {
        Self::Publication(event)
    }
}

impl From<GraphEvent> for DomainEvent {
    fn from(event: GraphEvent) -> Self {
        Self::Graph(event)
    }
}

impl From<BudgetEvent> for DomainEvent {
    fn from(event: BudgetEvent) -> Self {
        Self::Budget(event)
    }
}

impl From<LifecycleEvent> for DomainEvent {
    fn from(event: LifecycleEvent) -> Self {
        Self::Lifecycle(event)
    }
}

impl From<TerminalEvent> for DomainEvent {
    fn from(event: TerminalEvent) -> Self {
        Self::Terminal(event)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProtocolEventEnvelope {
    pub(crate) protocol_version: u16,
    pub(crate) event_schema_version: u16,
    pub(crate) event_id: EventId,
    pub(crate) execution_id: ExecutionId,
    pub(crate) execution_attempt: u32,
    pub(crate) sequence: u64,
    pub(crate) aggregate_revision_before: u64,
    #[serde(deserialize_with = "deserialize_explicit_option")]
    pub(crate) causation_id: Option<EventId>,
    pub(crate) correlation_id: CorrelationId,
    #[serde(deserialize_with = "deserialize_explicit_option")]
    pub(crate) node_id: Option<NodeId>,
    #[serde(deserialize_with = "deserialize_explicit_option")]
    pub(crate) effect_observation: Option<EffectObservationBinding>,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) semantic_key: String,
    pub(crate) semantic_identity: String,
    pub(crate) occurred_at_ms: u64,
    pub(crate) payload: DomainEvent,
}

impl ProtocolEventEnvelope {
    /// Compatibility constructor for private pre-freeze fixtures only.
    ///
    /// It deterministically derives a single execution-attempt correlation,
    /// chains causation to the last committed event, and derives node ownership
    /// from the payload/current owner. New wire call sites should use
    /// [`Self::new_with_context`] and pass the observed causal context
    /// explicitly.
    pub(crate) fn new_legacy_test_compatible(
        state: &super::ExecutionState,
        semantic_key: &str,
        occurred_at_ms: u64,
        payload: impl Into<DomainEvent>,
    ) -> Result<Self, ProtocolViolation> {
        if state.protocol_mode == super::ExecutionProtocolModeV1::StrictV1 {
            return Err(ProtocolViolation::Invariant {
                code: "strict_v1_explicit_event_context_required",
                detail: "strict Protocol v1 events must supply causal context explicitly".into(),
            });
        }
        let payload = payload.into();
        let context = ProtocolEventContext::new(
            state
                .event_log
                .last()
                .map(|stored| stored.envelope.event_id.clone()),
            CorrelationId::for_execution(&state.execution_id, state.execution_attempt),
            expected_node_owner(state, &payload),
        )?;
        Self::new_with_context(state, semantic_key, occurred_at_ms, context, payload)
    }

    pub(crate) fn new_with_context(
        state: &super::ExecutionState,
        semantic_key: &str,
        occurred_at_ms: u64,
        context: ProtocolEventContext,
        payload: impl Into<DomainEvent>,
    ) -> Result<Self, ProtocolViolation> {
        validate_wire_identity("semantic_key", semantic_key)?;
        let payload = payload.into();
        validate_context_against_state(state, &context, &payload)?;
        let semantic_identity = semantic_identity(semantic_key, &context, &payload)?;
        Ok(Self {
            protocol_version: super::EXECUTION_PROTOCOL_VERSION,
            event_schema_version: PROTOCOL_EVENT_SCHEMA_VERSION,
            event_id: EventId::derive(
                &state.execution_id,
                state.execution_attempt,
                &semantic_identity,
            ),
            execution_id: state.execution_id.clone(),
            execution_attempt: state.execution_attempt,
            sequence: state.next_sequence(),
            aggregate_revision_before: state.aggregate_revision,
            causation_id: context.causation_id,
            correlation_id: context.correlation_id,
            node_id: context.node_id,
            effect_observation: context.effect_observation,
            repository_revision: state.repository_revision.clone(),
            semantic_key: semantic_key.to_owned(),
            semantic_identity,
            occurred_at_ms,
            payload,
        })
    }

    pub(crate) fn canonical_hash(&self) -> Result<String, ProtocolViolation> {
        let bytes =
            serde_json::to_vec(self).map_err(|error| ProtocolViolation::EventSerialization {
                detail: error.to_string(),
            })?;
        let mut digest = Sha256::new();
        digest.update(b"execution-protocol-v1:stored-envelope\0");
        digest.update(bytes);
        Ok(hex::encode(digest.finalize()))
    }

    pub(crate) fn expected_semantic_identity(&self) -> Result<String, ProtocolViolation> {
        self.validate_intrinsic_context()?;
        semantic_identity(
            &self.semantic_key,
            &ProtocolEventContext {
                causation_id: self.causation_id.clone(),
                correlation_id: self.correlation_id.clone(),
                node_id: self.node_id.clone(),
                effect_observation: self.effect_observation.clone(),
            },
            &self.payload,
        )
    }

    pub(crate) fn expected_event_id(&self) -> Result<EventId, ProtocolViolation> {
        Ok(EventId::derive(
            &self.execution_id,
            self.execution_attempt,
            &self.expected_semantic_identity()?,
        ))
    }

    /// Validates the envelope metadata that depends on aggregate history.
    /// Reducers and durable stores should call this before applying the payload.
    pub(crate) fn validate_context_against(
        &self,
        state: &super::ExecutionState,
    ) -> Result<(), ProtocolViolation> {
        self.validate_intrinsic_context()?;
        validate_context_against_state(
            state,
            &ProtocolEventContext {
                causation_id: self.causation_id.clone(),
                correlation_id: self.correlation_id.clone(),
                node_id: self.node_id.clone(),
                effect_observation: self.effect_observation.clone(),
            },
            &self.payload,
        )
    }

    fn validate_intrinsic_context(&self) -> Result<(), ProtocolViolation> {
        validate_wire_identity("semantic_key", &self.semantic_key)?;
        validate_wire_identity("correlation_id", self.correlation_id.as_str())?;
        if let Some(causation_id) = &self.causation_id {
            validate_wire_identity("causation_id", causation_id.as_str())?;
            if causation_id == &self.event_id {
                return Err(ProtocolViolation::EnvelopeMismatch {
                    field: "causation_id",
                });
            }
        }
        if let Some(node_id) = &self.node_id {
            validate_wire_identity("node_id", node_id.as_str())?;
        }
        if let Some(binding) = &self.effect_observation {
            validate_wire_identity("effect_intent_id", binding.effect_intent_id.as_str())?;
            validate_sha256_digest("effect_request_digest", &binding.request_digest)?;
        }
        Ok(())
    }
}

fn semantic_identity(
    semantic_key: &str,
    context: &ProtocolEventContext,
    payload: &DomainEvent,
) -> Result<String, ProtocolViolation> {
    let identity_payload = serde_json::to_string(&(
        context.causation_id.as_ref(),
        &context.correlation_id,
        context.node_id.as_ref(),
        context.effect_observation.as_ref(),
        payload,
    ))
    .map_err(|error| ProtocolViolation::EventSerialization {
        detail: error.to_string(),
    })?;
    Ok(stable_sha256(&[
        "execution-protocol-v1:semantic-event",
        semantic_key,
        &identity_payload,
    ]))
}

fn validate_context_against_state(
    state: &super::ExecutionState,
    context: &ProtocolEventContext,
    payload: &DomainEvent,
) -> Result<(), ProtocolViolation> {
    if state.event_log.is_empty() != context.causation_id.is_none() {
        return Err(ProtocolViolation::EnvelopeMismatch {
            field: "causation_id",
        });
    }
    if let Some(causation_id) = &context.causation_id
        && !state.event_payload_hashes.contains_key(causation_id)
    {
        return Err(ProtocolViolation::EnvelopeMismatch {
            field: "causation_id",
        });
    }
    if context.effect_observation.is_some()
        && context.causation_id
            != state
                .event_log
                .last()
                .map(|stored| stored.envelope.event_id.clone())
    {
        return Err(ProtocolViolation::EnvelopeMismatch {
            field: "effect_observation",
        });
    }
    if let Some(existing) = state.event_log.first()
        && context.correlation_id != existing.envelope.correlation_id
    {
        return Err(ProtocolViolation::EnvelopeMismatch {
            field: "correlation_id",
        });
    }
    let expected_node_id = expected_node_owner(state, payload);
    if context.node_id != expected_node_id {
        return Err(ProtocolViolation::EnvelopeMismatch { field: "node_id" });
    }
    if let Some(node_id) = &context.node_id
        && !state.nodes.contains_key(node_id)
        && !payload_adds_node(payload, node_id)
    {
        return Err(ProtocolViolation::UnknownNode {
            node_id: node_id.clone(),
        });
    }
    Ok(())
}

fn expected_node_owner(state: &super::ExecutionState, payload: &DomainEvent) -> Option<NodeId> {
    match payload {
        DomainEvent::Graph(event) => graph_event_node_id(event).cloned(),
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

fn graph_event_node_id(event: &GraphEvent) -> Option<&NodeId> {
    match event {
        GraphEvent::NodesAdded { .. } => None,
        GraphEvent::ValidationRepairNodeAdded { node, .. } => Some(&node.id),
        GraphEvent::NodeStarted { node_id, .. }
        | GraphEvent::NodeWaiting { node_id, .. }
        | GraphEvent::NodeResumed { node_id, .. }
        | GraphEvent::NodeSucceeded { node_id, .. }
        | GraphEvent::NodeFailed { node_id, .. } => Some(node_id),
    }
}

fn payload_adds_node(payload: &DomainEvent, node_id: &NodeId) -> bool {
    matches!(
        payload,
        DomainEvent::Graph(GraphEvent::ValidationRepairNodeAdded { node, .. })
            if &node.id == node_id
    )
}

fn validate_wire_identity(field: &'static str, value: &str) -> Result<(), ProtocolViolation> {
    if value.is_empty()
        || value.trim() != value
        || value.len() > MAX_EVENT_IDENTITY_BYTES
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(ProtocolViolation::InvalidIdentity { field });
    }
    Ok(())
}

fn validate_sha256_digest(field: &'static str, value: &str) -> Result<(), ProtocolViolation> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ProtocolViolation::InvalidIdentity { field });
    }
    Ok(())
}

fn deserialize_explicit_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[cfg(test)]
mod envelope_contract_tests {
    use super::*;

    fn context(
        causation_id: Option<&str>,
        correlation_id: &str,
        node_id: Option<&str>,
    ) -> ProtocolEventContext {
        ProtocolEventContext::new(
            causation_id.map(EventId::new),
            CorrelationId::new(correlation_id).expect("valid correlation"),
            node_id.map(NodeId::new),
        )
        .expect("valid event context")
    }

    fn lifecycle_payload() -> DomainEvent {
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Discovery,
            to: ProtocolStage::Planning,
            proof_id: ProofId::new("proof:event-envelope"),
        }
        .into()
    }

    #[test]
    fn correlation_id_rejects_noncanonical_wire_values() {
        assert!(CorrelationId::new(" correlation:event ").is_err());
        assert!(CorrelationId::new("correlation:\nevent").is_err());
        assert!(CorrelationId::new("x".repeat(MAX_EVENT_IDENTITY_BYTES + 1)).is_err());
    }

    #[test]
    fn causal_and_node_context_are_semantic_identity() {
        let payload = lifecycle_payload();
        let base = context(None, "correlation:event", None);
        let caused = context(Some("event:cause"), "correlation:event", Some("node:owner"));
        assert_ne!(
            semantic_identity("event:key", &base, &payload).expect("base identity"),
            semantic_identity("event:key", &caused, &payload).expect("caused identity")
        );
    }

    #[test]
    fn effect_observation_binding_is_typed_and_semantic_identity_bearing() {
        let payload = lifecycle_payload();
        let unbound = context(Some("event:cause"), "correlation:event", None);
        let bound = ProtocolEventContext::for_effect_observation(
            EventId::new("event:cause"),
            CorrelationId::new("correlation:event").expect("valid correlation"),
            None,
            EffectObservationBinding::new(EffectId::new("effect:intent"), "a".repeat(64))
                .expect("valid effect observation binding"),
        )
        .expect("valid effect observation context");

        assert_ne!(
            semantic_identity("event:key", &unbound, &payload).expect("unbound identity"),
            semantic_identity("event:key", &bound, &payload).expect("bound identity")
        );
        assert_eq!(
            bound
                .effect_observation
                .as_ref()
                .expect("typed binding")
                .effect_intent_id,
            EffectId::new("effect:intent")
        );
    }

    #[test]
    fn effect_observation_binding_rejects_noncanonical_identity_and_digest() {
        assert!(
            EffectObservationBinding::new(EffectId::new(" effect:intent "), "a".repeat(64))
                .is_err()
        );
        assert!(
            EffectObservationBinding::new(EffectId::new("effect:intent"), "A".repeat(64)).is_err()
        );
        assert!(
            EffectObservationBinding::new(EffectId::new("effect:intent"), "a".repeat(63)).is_err()
        );
    }

    #[test]
    fn event_context_serde_requires_the_frozen_fields() {
        assert!(
            serde_json::from_str::<ProtocolEventContext>(
                r#"{"causation_id":null,"correlation_id":"correlation:event","node_id":null,"effect_observation":null}"#,
            )
            .is_ok()
        );
        assert!(
            serde_json::from_str::<ProtocolEventContext>(
                r#"{"causation_id":null,"correlation_id":"correlation:event"}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ProtocolEventContext>(
                r#"{"causation_id":null,"correlation_id":"correlation:event","node_id":null}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ProtocolEventContext>(
                r#"{"causation_id":null,"correlation_id":"correlation:event","node_id":null,"effect_observation":null,"extra":true}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ProtocolEventContext>(
                r#"{"causation_id":"event:cause","correlation_id":"correlation:event","node_id":null,"effect_observation":{"effect_intent_id":"effect:intent","request_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
            )
            .is_ok()
        );
        assert!(
            serde_json::from_str::<ProtocolEventContext>(
                r#"{"causation_id":"event:cause","correlation_id":"correlation:event","node_id":null,"effect_observation":{"effect_intent_id":"effect:intent","request_digest":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}"#,
            )
            .is_err()
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppendOutcome {
    Applied { revision: u64 },
    IdempotentReplay { revision: u64 },
}

// Decisions are short-lived reducer values and deliberately carry the exact
// domain event that will be appended, without a second ownership abstraction.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolDecision {
    Emit { event: DomainEvent },
    Perform { effect: EffectRequest },
    Finish { result: CanonicalResult },
    Wait { reason: WaitReason },
}

#[derive(Clone, Debug, PartialEq, Eq)]
// Effect requests are short-lived reducer outputs. Keeping the exact typed
// request inline avoids a second wire shape at the adapter boundary.
#[allow(clippy::large_enum_variant)]
pub(crate) enum EffectRequest {
    Discovery(DiscoveryEffectRequest),
    Planning(PlanningEffectRequest),
    Implementation(ImplementationEffectRequest),
    Mutation(MutationEffectRequest),
    Validation(ValidationEffectRequest),
    Review(ReviewEffectRequest),
    Publication(PublicationEffectRequest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WaitReason {
    ActiveNode {
        node_id: NodeId,
    },
    ProviderReconciliation {
        call_id: ModelCallId,
    },
    DiscoveryObservation {
        action_id: ActionId,
    },
    PlanningObservation {
        action_id: ActionId,
    },
    ImplementationContextReady {
        node_id: NodeId,
        context_manifest_id: super::ContextManifestId,
    },
    MutationObservation {
        action_id: ActionId,
    },
    ValidationProcessObservation {
        run_id: super::ValidationRunId,
        process_id: Option<super::ValidationProcessId>,
    },
    ReviewObservation {
        action_id: ActionId,
    },
    PublicationObservation {
        effect_id: EffectId,
    },
    NoRunnableNode {
        stage: ProtocolStage,
    },
}
