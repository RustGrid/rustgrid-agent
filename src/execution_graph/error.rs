#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphInvariantError {
    pub message: String,
}

impl GraphInvariantError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for GraphInvariantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GraphInvariantError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphTransitionError {
    UnknownNode {
        node_id: ExecutionNodeId,
    },
    IllegalNodeTransition {
        node_id: ExecutionNodeId,
        from: ExecutionNodeStatus,
        to: ExecutionNodeStatus,
    },
    ActiveOwnerConflict {
        active_node_id: ExecutionNodeId,
        requested_node_id: ExecutionNodeId,
    },
    Invariant(GraphInvariantError),
}

impl fmt::Display for GraphTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownNode { node_id } => {
                write!(formatter, "unknown execution node `{node_id}`")
            }
            Self::IllegalNodeTransition { node_id, from, to } => write!(
                formatter,
                "illegal execution node transition for `{node_id}`: {from:?} -> {to:?}"
            ),
            Self::ActiveOwnerConflict {
                active_node_id,
                requested_node_id,
            } => write!(
                formatter,
                "execution node `{active_node_id}` already owns execution; `{requested_node_id}` cannot start"
            ),
            Self::Invariant(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GraphTransitionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invariant(error) => Some(error),
            _ => None,
        }
    }
}

impl From<GraphInvariantError> for GraphTransitionError {
    fn from(error: GraphInvariantError) -> Self {
        Self::Invariant(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionOutcome {
    Applied,
    IdempotentReplay,
}
