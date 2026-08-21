use super::{
    AppendOutcome, ExecutionState, NodeId, ProtocolEventEnvelope, ProtocolViolation,
    StoredProtocolEvent,
};

/// In-memory Phase 1 event store.
///
/// The aggregate owns its committed event stream so a snapshot and its replay
/// index cannot be updated independently. This wrapper is the persistence
/// boundary used by conformance tests and later shadow execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InMemoryEventStore {
    trusted_initial: ExecutionState,
    state: ExecutionState,
}

impl InMemoryEventStore {
    pub(crate) fn new(trusted_initial: ExecutionState) -> Result<Self, ProtocolViolation> {
        validate_pristine_bootstrap(&trusted_initial)?;
        Ok(Self {
            state: trusted_initial.clone(),
            trusted_initial,
        })
    }

    pub(crate) fn restore(
        trusted_initial: ExecutionState,
        snapshot: ExecutionState,
    ) -> Result<Self, ProtocolViolation> {
        validate_pristine_bootstrap(&trusted_initial)?;
        snapshot.validate_invariants()?;
        let state = replay_and_validate(&trusted_initial, &snapshot)?;
        Ok(Self {
            trusted_initial,
            state,
        })
    }

    pub(crate) fn state(&self) -> &ExecutionState {
        &self.state
    }

    pub(crate) fn events(&self) -> &[StoredProtocolEvent] {
        &self.state.event_log
    }

    pub(crate) fn append(
        &mut self,
        event: ProtocolEventEnvelope,
    ) -> Result<AppendOutcome, ProtocolViolation> {
        let mut next = self.state.clone();
        let outcome = next.append_event(event)?;
        self.state = replay_and_validate(&self.trusted_initial, &next)?;
        Ok(outcome)
    }

    pub(crate) fn into_state(self) -> ExecutionState {
        self.state
    }
}

pub(super) fn validate_replay_equivalence(state: &ExecutionState) -> Result<(), ProtocolViolation> {
    state.require_trusted_bootstrap()?;
    let initial = bootstrap_from(state)?;
    replay_and_validate(&initial, state).map(|_| ())
}

fn validate_pristine_bootstrap(state: &ExecutionState) -> Result<(), ProtocolViolation> {
    state.require_trusted_bootstrap()?;
    state.validate_invariants()?;
    let expected = bootstrap_from(state)?;
    if state != &expected {
        return Err(ProtocolViolation::Invariant {
            code: "snapshot_does_not_match_event_replay",
            detail: "event store initialization requires a pristine bootstrap aggregate".into(),
        });
    }
    Ok(())
}

fn bootstrap_from(state: &ExecutionState) -> Result<ExecutionState, ProtocolViolation> {
    let discovery_id = NodeId::new("protocol-v1:discovery");
    let planning_id = NodeId::new("protocol-v1:planning");
    let discovery_budget = state
        .nodes
        .get(&discovery_id)
        .ok_or_else(|| ProtocolViolation::InvalidGraph {
            code: "bootstrap_discovery_node_missing",
            node_id: Some(discovery_id.clone()),
        })?
        .budget
        .clone();
    let planning_budget = state
        .nodes
        .get(&planning_id)
        .ok_or_else(|| ProtocolViolation::InvalidGraph {
            code: "bootstrap_planning_node_missing",
            node_id: Some(planning_id.clone()),
        })?
        .budget
        .clone();
    match state.protocol_mode {
        super::ExecutionProtocolModeV1::CompatibilityScaffold => {
            Ok(ExecutionState::bootstrap_with_finalization_policy(
                state.execution_id.clone(),
                state.execution_attempt,
                state.initial_repository_revision.clone(),
                state.mission_budget.clone(),
                discovery_budget,
                planning_budget,
                state.plan_graph_budget.clone(),
                state.validation_policy.clone(),
                state.finalization_policy.clone(),
            ))
        }
        super::ExecutionProtocolModeV1::StrictV1 => ExecutionState::bootstrap_strict_v1(
            state.execution_id.clone(),
            state.execution_attempt,
            state.initial_repository_revision.clone(),
            state.mission_budget.clone(),
            discovery_budget,
            planning_budget,
            state.plan_graph_budget.clone(),
            state
                .requested_discovery_goal
                .clone()
                .ok_or(ProtocolViolation::DiscoveryContract {
                    code: "strict_v1_discovery_goal_missing",
                })?,
            state
                .validation_policy
                .clone()
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "strict_v1_validation_policy_missing",
                })?,
            state
                .finalization_policy
                .clone()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "strict_v1_finalization_policy_missing",
                })?,
        ),
    }
}

fn replay_and_validate(
    trusted_initial: &ExecutionState,
    state: &ExecutionState,
) -> Result<ExecutionState, ProtocolViolation> {
    let mut replayed = trusted_initial.clone();
    for stored in &state.event_log {
        replayed.append_event(stored.envelope.clone())?;
    }
    let mut normalized = state.clone();
    normalized.trusted_bootstrap = true;
    if replayed != normalized {
        return Err(ProtocolViolation::Invariant {
            code: "snapshot_does_not_match_event_replay",
            detail: "materialized protocol state diverged from committed events".into(),
        });
    }
    Ok(replayed)
}
