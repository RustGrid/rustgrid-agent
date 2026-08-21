use serde::{Deserialize, Serialize};
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

pub(crate) const PROTOCOL_EVENT_SCHEMA_VERSION: u16 = 1;

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
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) semantic_key: String,
    pub(crate) semantic_identity: String,
    pub(crate) occurred_at_ms: u64,
    pub(crate) payload: DomainEvent,
}

impl ProtocolEventEnvelope {
    pub(crate) fn new(
        state: &super::ExecutionState,
        semantic_key: &str,
        occurred_at_ms: u64,
        payload: impl Into<DomainEvent>,
    ) -> Result<Self, ProtocolViolation> {
        if semantic_key.trim().is_empty() {
            return Err(ProtocolViolation::InvalidIdentity {
                field: "semantic_key",
            });
        }
        let payload = payload.into();
        let semantic_identity = semantic_identity(semantic_key, &payload)?;
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
        semantic_identity(&self.semantic_key, &self.payload)
    }

    pub(crate) fn expected_event_id(&self) -> Result<EventId, ProtocolViolation> {
        Ok(EventId::derive(
            &self.execution_id,
            self.execution_attempt,
            &self.expected_semantic_identity()?,
        ))
    }
}

fn semantic_identity(
    semantic_key: &str,
    payload: &DomainEvent,
) -> Result<String, ProtocolViolation> {
    let payload =
        serde_json::to_string(payload).map_err(|error| ProtocolViolation::EventSerialization {
            detail: error.to_string(),
        })?;
    Ok(stable_sha256(&[
        "execution-protocol-v1:semantic-event",
        semantic_key,
        &payload,
    ]))
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
