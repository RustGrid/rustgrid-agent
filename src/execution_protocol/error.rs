use std::fmt;

use super::{
    ContextBuildError, DiscoveryContractError, EventId, ModelCallId, MutationContractError, NodeId,
    PlanningContractError, ProofKind, ProtocolStage, PublicationContractError,
    RepositoryProfileError, ReviewContractError, SearchId, TargetContextContractError,
    ValidationContractError,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolViolation {
    UnsupportedVersion {
        found: u16,
    },
    InvalidIdentity {
        field: &'static str,
    },
    RevisionConflict {
        expected: u64,
        actual: u64,
    },
    SequenceConflict {
        expected: u64,
        actual: u64,
    },
    EventIdentityConflict {
        event_id: EventId,
    },
    EventSerialization {
        detail: String,
    },
    EnvelopeMismatch {
        field: &'static str,
    },
    InvalidEventIdentity {
        event_id: EventId,
    },
    TerminalImmutable,
    IllegalTransition {
        from: ProtocolStage,
        to: ProtocolStage,
    },
    MissingTransitionProof {
        required: ProofKind,
    },
    UnknownProof {
        proof_id: super::ProofId,
    },
    InvalidProof {
        proof_id: super::ProofId,
        code: &'static str,
    },
    DuplicateProof {
        proof_id: super::ProofId,
    },
    UnknownNode {
        node_id: NodeId,
    },
    DuplicateNode {
        node_id: NodeId,
    },
    InvalidNodeState {
        node_id: NodeId,
        code: &'static str,
    },
    WrongPosition {
        node_id: NodeId,
        position: ProtocolStage,
    },
    ActiveOwnerConflict {
        active_node_id: NodeId,
        requested_node_id: NodeId,
    },
    UnsatisfiedDependency {
        node_id: NodeId,
        dependency_id: NodeId,
    },
    InvalidGraph {
        code: &'static str,
        node_id: Option<NodeId>,
    },
    BudgetExceeded {
        node_id: Option<NodeId>,
        dimension: &'static str,
    },
    ModelCallLifecycle {
        call_id: ModelCallId,
        code: &'static str,
    },
    RepositoryProfile {
        code: &'static str,
    },
    DiscoveryContract {
        code: &'static str,
    },
    PlanningContract {
        code: &'static str,
    },
    ImplementationContract {
        code: &'static str,
    },
    MutationContract {
        code: &'static str,
    },
    ValidationContract {
        code: &'static str,
    },
    ReviewContract {
        code: &'static str,
    },
    PublicationContract {
        code: &'static str,
    },
    DuplicateSearch {
        search_id: SearchId,
    },
    ContextTooLarge {
        required_tokens: u32,
        input_token_ceiling: u32,
    },
    ImplementationContextTooLarge {
        required_tokens: u32,
        input_token_ceiling: u32,
    },
    TerminalPredicate {
        code: &'static str,
    },
    Invariant {
        code: &'static str,
        detail: String,
    },
}

impl ProtocolViolation {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedVersion { .. } => "unsupported_protocol_version",
            Self::InvalidIdentity { .. } => "invalid_identity",
            Self::RevisionConflict { .. } => "aggregate_revision_conflict",
            Self::SequenceConflict { .. } => "event_sequence_conflict",
            Self::EventIdentityConflict { .. } => "event_identity_conflict",
            Self::EventSerialization { .. } => "event_serialization_failed",
            Self::EnvelopeMismatch { .. } => "event_envelope_mismatch",
            Self::InvalidEventIdentity { .. } => "invalid_event_identity",
            Self::TerminalImmutable => "canonical_result_immutable",
            Self::IllegalTransition { .. } => "illegal_protocol_transition",
            Self::MissingTransitionProof { .. } => "transition_proof_missing",
            Self::UnknownProof { .. } => "unknown_proof",
            Self::InvalidProof { code, .. }
            | Self::InvalidNodeState { code, .. }
            | Self::InvalidGraph { code, .. }
            | Self::ModelCallLifecycle { code, .. }
            | Self::RepositoryProfile { code }
            | Self::DiscoveryContract { code }
            | Self::PlanningContract { code }
            | Self::ImplementationContract { code }
            | Self::MutationContract { code }
            | Self::ValidationContract { code }
            | Self::ReviewContract { code }
            | Self::PublicationContract { code }
            | Self::TerminalPredicate { code }
            | Self::Invariant { code, .. } => code,
            Self::DuplicateProof { .. } => "duplicate_proof",
            Self::UnknownNode { .. } => "unknown_node",
            Self::DuplicateNode { .. } => "duplicate_node",
            Self::WrongPosition { .. } => "node_wrong_protocol_position",
            Self::ActiveOwnerConflict { .. } => "active_owner_conflict",
            Self::UnsatisfiedDependency { .. } => "unsatisfied_dependency",
            Self::BudgetExceeded { .. } => "signed_budget_exceeded",
            Self::DuplicateSearch { .. } => "duplicate_discovery_search",
            Self::ContextTooLarge { .. } => "discovery_context_too_large",
            Self::ImplementationContextTooLarge { .. } => "implementation_context_too_large",
        }
    }
}

impl fmt::Display for ProtocolViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { found } => {
                write!(
                    formatter,
                    "unsupported execution protocol version `{found}`"
                )
            }
            Self::InvalidIdentity { field } => {
                write!(formatter, "execution protocol identity `{field}` is empty")
            }
            Self::RevisionConflict { expected, actual } => write!(
                formatter,
                "execution protocol revision conflict: expected {expected}, actual {actual}"
            ),
            Self::SequenceConflict { expected, actual } => write!(
                formatter,
                "execution protocol event sequence conflict: expected {expected}, actual {actual}"
            ),
            Self::EventIdentityConflict { event_id } => write!(
                formatter,
                "execution protocol event `{event_id}` was replayed with different content"
            ),
            Self::EventSerialization { detail } => {
                write!(
                    formatter,
                    "execution protocol event serialization failed: {detail}"
                )
            }
            Self::EnvelopeMismatch { field } => {
                write!(formatter, "execution protocol event mismatches `{field}`")
            }
            Self::InvalidEventIdentity { event_id } => {
                write!(
                    formatter,
                    "execution protocol event identity `{event_id}` is invalid"
                )
            }
            Self::TerminalImmutable => {
                formatter.write_str("canonical execution result is immutable")
            }
            Self::IllegalTransition { from, to } => {
                write!(
                    formatter,
                    "illegal execution protocol transition {from:?} -> {to:?}"
                )
            }
            Self::MissingTransitionProof { required } => {
                write!(formatter, "transition requires a {required:?} proof")
            }
            Self::UnknownProof { proof_id } => write!(formatter, "unknown proof `{proof_id}`"),
            Self::InvalidProof { proof_id, code } => {
                write!(formatter, "proof `{proof_id}` is invalid: {code}")
            }
            Self::DuplicateProof { proof_id } => {
                write!(formatter, "proof `{proof_id}` is already recorded")
            }
            Self::UnknownNode { node_id } => write!(formatter, "unknown node `{node_id}`"),
            Self::DuplicateNode { node_id } => {
                write!(formatter, "node `{node_id}` is already recorded")
            }
            Self::InvalidNodeState { node_id, code } => {
                write!(formatter, "node `{node_id}` has invalid state for `{code}`")
            }
            Self::WrongPosition { node_id, position } => write!(
                formatter,
                "node `{node_id}` cannot execute while protocol position is {position:?}"
            ),
            Self::ActiveOwnerConflict {
                active_node_id,
                requested_node_id,
            } => write!(
                formatter,
                "node `{active_node_id}` already owns execution; `{requested_node_id}` cannot start"
            ),
            Self::UnsatisfiedDependency {
                node_id,
                dependency_id,
            } => write!(
                formatter,
                "node `{node_id}` has unsatisfied dependency `{dependency_id}`"
            ),
            Self::InvalidGraph { code, node_id } => {
                if let Some(node_id) = node_id {
                    write!(formatter, "invalid protocol graph at `{node_id}`: {code}")
                } else {
                    write!(formatter, "invalid protocol graph: {code}")
                }
            }
            Self::BudgetExceeded { node_id, dimension } => {
                if let Some(node_id) = node_id {
                    write!(formatter, "node `{node_id}` exhausted `{dimension}` budget")
                } else {
                    write!(formatter, "mission exhausted `{dimension}` budget")
                }
            }
            Self::ModelCallLifecycle { call_id, code } => {
                write!(
                    formatter,
                    "model call `{call_id}` violates lifecycle `{code}`"
                )
            }
            Self::RepositoryProfile { code } => {
                write!(formatter, "repository profile violates `{code}`")
            }
            Self::DiscoveryContract { code } => {
                write!(formatter, "discovery contract violates `{code}`")
            }
            Self::PlanningContract { code } => {
                write!(formatter, "planning contract violates `{code}`")
            }
            Self::ImplementationContract { code } => {
                write!(formatter, "implementation contract violates `{code}`")
            }
            Self::MutationContract { code } => {
                write!(formatter, "mutation contract violates `{code}`")
            }
            Self::ValidationContract { code } => {
                write!(formatter, "validation contract violates `{code}`")
            }
            Self::ReviewContract { code } => {
                write!(formatter, "review contract violates `{code}`")
            }
            Self::PublicationContract { code } => {
                write!(formatter, "publication contract violates `{code}`")
            }
            Self::DuplicateSearch { search_id } => {
                write!(
                    formatter,
                    "discovery search `{search_id}` is already complete"
                )
            }
            Self::ContextTooLarge {
                required_tokens,
                input_token_ceiling,
            } => write!(
                formatter,
                "mandatory discovery context requires {required_tokens} tokens but the signed ceiling is {input_token_ceiling}"
            ),
            Self::ImplementationContextTooLarge {
                required_tokens,
                input_token_ceiling,
            } => write!(
                formatter,
                "mandatory implementation context requires {required_tokens} tokens but the signed ceiling is {input_token_ceiling}"
            ),
            Self::TerminalPredicate { code } => {
                write!(formatter, "canonical result predicate failed: {code}")
            }
            Self::Invariant { code, detail } => {
                write!(
                    formatter,
                    "execution protocol invariant `{code}` failed: {detail}"
                )
            }
        }
    }
}

impl std::error::Error for ProtocolViolation {}

impl From<RepositoryProfileError> for ProtocolViolation {
    fn from(error: RepositoryProfileError) -> Self {
        let code = match error {
            RepositoryProfileError::InvalidPath(_) => "repository_profile_path_invalid",
            RepositoryProfileError::InvalidContentHash => "repository_profile_content_hash_invalid",
            RepositoryProfileError::InventoryFileLimitExceeded { .. } => {
                "repository_profile_file_limit_exceeded"
            }
            RepositoryProfileError::CapturedFileLimitExceeded { .. } => {
                "repository_profile_capture_limit_exceeded"
            }
            RepositoryProfileError::CapturedTotalLimitExceeded { .. } => {
                "repository_profile_total_capture_limit_exceeded"
            }
            RepositoryProfileError::ConflictingObservation { .. } => {
                "repository_profile_observation_conflict"
            }
            RepositoryProfileError::IdentityEncoding => "repository_profile_identity_encoding",
            RepositoryProfileError::UnsupportedSchema { .. } => {
                "repository_profile_schema_unsupported"
            }
            RepositoryProfileError::NonCanonicalField { .. } => "repository_profile_non_canonical",
            RepositoryProfileError::InconsistentProfile { code } => code,
            RepositoryProfileError::ProfileIdentityMismatch => {
                "repository_profile_identity_mismatch"
            }
        };
        Self::RepositoryProfile { code }
    }
}

impl From<DiscoveryContractError> for ProtocolViolation {
    fn from(error: DiscoveryContractError) -> Self {
        Self::DiscoveryContract { code: error.code() }
    }
}

impl From<ContextBuildError> for ProtocolViolation {
    fn from(error: ContextBuildError) -> Self {
        match error {
            ContextBuildError::Contract(error) => error.into(),
            ContextBuildError::MandatoryTooLarge {
                required_tokens,
                input_token_ceiling,
            } => Self::ContextTooLarge {
                required_tokens,
                input_token_ceiling,
            },
        }
    }
}

impl From<PlanningContractError> for ProtocolViolation {
    fn from(error: PlanningContractError) -> Self {
        Self::PlanningContract { code: error.code() }
    }
}

impl From<TargetContextContractError> for ProtocolViolation {
    fn from(error: TargetContextContractError) -> Self {
        match error {
            TargetContextContractError::MandatoryContextTooLarge {
                required_tokens,
                input_token_ceiling,
            } => Self::ImplementationContextTooLarge {
                required_tokens,
                input_token_ceiling,
            },
            other => Self::ImplementationContract { code: other.code() },
        }
    }
}

impl From<MutationContractError> for ProtocolViolation {
    fn from(error: MutationContractError) -> Self {
        Self::MutationContract { code: error.code() }
    }
}

impl From<ValidationContractError> for ProtocolViolation {
    fn from(error: ValidationContractError) -> Self {
        Self::ValidationContract { code: error.code() }
    }
}

impl From<ReviewContractError> for ProtocolViolation {
    fn from(error: ReviewContractError) -> Self {
        Self::ReviewContract { code: error.code() }
    }
}

impl From<PublicationContractError> for ProtocolViolation {
    fn from(error: PublicationContractError) -> Self {
        Self::PublicationContract { code: error.code() }
    }
}
