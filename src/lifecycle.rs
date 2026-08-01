use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value),+
                }
            }
        }
    };
}

wire_enum!(StepStatus {
    Running => "running",
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
});

impl StepStatus {
    pub const fn severity(self) -> &'static str {
        match self {
            Self::Failed => "error",
            _ => "info",
        }
    }

    pub const fn console_color(self) -> &'static str {
        match self {
            Self::Completed => "32",
            Self::Failed => "31",
            Self::Running => "36",
            Self::Cancelled => "35",
        }
    }
}

wire_enum!(TicketStatus {
    Todo => "todo",
    InProgress => "in_progress",
    AwaitingReview => "awaiting_review",
    Blocked => "blocked",
});

wire_enum!(AgentRunStatus {
    Succeeded => "succeeded",
    Failed => "failed",
    Cancelled => "cancelled",
});

wire_enum!(WorkerStatus {
    Online => "online",
    Busy => "busy",
});

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Claimed,
    Preparing,
    Executing,
    Verifying,
    Publishing,
    AwaitingReview,
    Succeeded,
    Blocked,
    Failed,
    Cancelled,
    TimedOut,
}

/// The coarse hosted-worker stage exposed at the lifecycle boundary.
///
/// Fine-grained progress remains owned by the hosted execution graph. In
/// particular, this type cannot identify an individual graph node or target.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedExecutionStage {
    Discovery,
    Planning,
    Implementation,
    Validation,
    Review,
    Publication,
    Terminal,
}

impl HostedExecutionStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Planning => "planning",
            Self::Implementation => "implementation",
            Self::Validation => "validation",
            Self::Review => "review",
            Self::Publication => "publication",
            Self::Terminal => "terminal",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal)
    }

    /// Returns whether the hosted orchestrator may move between two coarse
    /// stages. Node-level readiness must still be decided by the execution
    /// graph; this table is only the lifecycle boundary.
    pub const fn can_transition_to(self, next: Self) -> bool {
        if matches!(
            (self, next),
            (Self::Discovery, Self::Discovery)
                | (Self::Planning, Self::Planning)
                | (Self::Implementation, Self::Implementation)
                | (Self::Validation, Self::Validation)
                | (Self::Review, Self::Review)
                | (Self::Publication, Self::Publication)
                | (Self::Terminal, Self::Terminal)
        ) {
            return true;
        }
        if self.is_terminal() {
            return false;
        }
        matches!(
            (self, next),
            (Self::Discovery, Self::Planning)
                | (Self::Planning, Self::Implementation)
                | (Self::Implementation, Self::Validation)
                | (Self::Validation, Self::Implementation)
                | (Self::Validation, Self::Review)
                | (Self::Review, Self::Publication)
                // A remote branch may move after graph-selected publication
                // begins. Reconciliation must invalidate stale proof and
                // re-enter the full validation -> review -> publication route.
                | (Self::Publication, Self::Validation)
                | (Self::Publication, Self::Terminal)
        )
    }
}

/// Maps the hosted stage into the intentionally coarser public/API lifecycle.
/// Discovery and planning are both preparing; validation and internal review
/// are both verifying. Terminal selects the successful public terminal phase;
/// callers with a concrete terminal outcome must use the corresponding
/// `RunPhase` instead.
impl From<HostedExecutionStage> for RunPhase {
    fn from(stage: HostedExecutionStage) -> Self {
        match stage {
            HostedExecutionStage::Discovery | HostedExecutionStage::Planning => Self::Preparing,
            HostedExecutionStage::Implementation => Self::Executing,
            HostedExecutionStage::Validation | HostedExecutionStage::Review => Self::Verifying,
            HostedExecutionStage::Publication => Self::Publishing,
            HostedExecutionStage::Terminal => Self::Succeeded,
        }
    }
}

impl RunPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Preparing => "preparing",
            Self::Executing => "executing",
            Self::Verifying => "verifying",
            Self::Publishing => "publishing",
            Self::AwaitingReview => "awaiting_review",
            Self::Succeeded => "succeeded",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Blocked | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }

    /// Recovers a hosted stage only when this public phase identifies one
    /// unambiguously. `Preparing` aggregates discovery and planning, while
    /// `Verifying` aggregates validation and internal diff review, so those
    /// phases deliberately return `None`. Graph state, not `RunPhase`, must be
    /// consulted to recover the finer progression.
    pub const fn hosted_execution_stage(self) -> Option<HostedExecutionStage> {
        match self {
            Self::Executing => Some(HostedExecutionStage::Implementation),
            Self::Publishing => Some(HostedExecutionStage::Publication),
            Self::Succeeded | Self::Blocked | Self::Failed | Self::Cancelled | Self::TimedOut => {
                Some(HostedExecutionStage::Terminal)
            }
            Self::Claimed | Self::Preparing | Self::Verifying | Self::AwaitingReview => None,
        }
    }

    pub const fn can_transition_to(self, next: Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        matches!(
            (self, next),
            (Self::Claimed, Self::Preparing)
                | (Self::Preparing, Self::Executing)
                | (Self::Preparing, Self::Publishing)
                | (Self::Executing, Self::Verifying)
                | (Self::Verifying, Self::Executing)
                | (Self::Verifying, Self::Publishing)
                | (Self::Publishing, Self::Executing)
                | (Self::Publishing, Self::Verifying)
                | (Self::Publishing, Self::AwaitingReview)
                | (Self::AwaitingReview, Self::Succeeded)
                | (
                    _,
                    Self::Blocked | Self::Failed | Self::Cancelled | Self::TimedOut
                )
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LifecycleEvent {
    pub schema_version: u8,
    pub sequence: u64,
    pub timestamp_unix_ms: u128,
    pub phase: RunPhase,
    pub event_type: String,
    pub severity: String,
    pub message: String,
    pub data: Value,
}

impl LifecycleEvent {
    pub fn new(
        sequence: u64,
        phase: RunPhase,
        event_type: impl Into<String>,
        severity: impl Into<String>,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Self {
        Self {
            schema_version: 1,
            sequence,
            timestamp_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            phase,
            event_type: event_type.into(),
            severity: severity.into(),
            message: message.into(),
            data: data.unwrap_or_else(|| json!({})),
        }
    }

    pub fn metadata(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({"schema_version": 1}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_event_has_replay_fields() {
        let event = LifecycleEvent::new(
            7,
            RunPhase::Executing,
            "agent.started",
            "info",
            "Started",
            None,
        );
        assert_eq!(event.sequence, 7);
        assert_eq!(event.metadata()["phase"], "executing");
        assert!(!RunPhase::Executing.is_terminal());
        assert!(RunPhase::Succeeded.is_terminal());
        assert!(RunPhase::Claimed.can_transition_to(RunPhase::Preparing));
        assert!(RunPhase::Executing.can_transition_to(RunPhase::TimedOut));
        assert!(RunPhase::Verifying.can_transition_to(RunPhase::Executing));
        assert!(RunPhase::Publishing.can_transition_to(RunPhase::Executing));
        assert!(RunPhase::Publishing.can_transition_to(RunPhase::Verifying));
        assert!(!RunPhase::Succeeded.can_transition_to(RunPhase::Executing));
        assert!(!RunPhase::Claimed.can_transition_to(RunPhase::Succeeded));
        assert_eq!(StepStatus::Failed.severity(), "error");
        assert_eq!(TicketStatus::AwaitingReview.as_str(), "awaiting_review");
        assert_eq!(AgentRunStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(WorkerStatus::Busy.as_str(), "busy");
    }

    #[test]
    fn hosted_stage_serializes_and_maps_to_public_phases() {
        assert_eq!(
            serde_json::to_string(&HostedExecutionStage::Review).expect("serialize hosted stage"),
            "\"review\""
        );
        assert_eq!(HostedExecutionStage::Planning.as_str(), "planning");
        assert_eq!(
            RunPhase::from(HostedExecutionStage::Discovery),
            RunPhase::Preparing
        );
        assert_eq!(
            RunPhase::from(HostedExecutionStage::Planning),
            RunPhase::Preparing
        );
        assert_eq!(
            RunPhase::from(HostedExecutionStage::Implementation),
            RunPhase::Executing
        );
        assert_eq!(
            RunPhase::from(HostedExecutionStage::Validation),
            RunPhase::Verifying
        );
        assert_eq!(
            RunPhase::from(HostedExecutionStage::Review),
            RunPhase::Verifying
        );
        assert_eq!(
            RunPhase::from(HostedExecutionStage::Publication),
            RunPhase::Publishing
        );
        assert_eq!(
            RunPhase::from(HostedExecutionStage::Terminal),
            RunPhase::Succeeded
        );
    }

    #[test]
    fn hosted_stage_transition_table_enforces_the_authoritative_route() {
        use HostedExecutionStage as Stage;

        assert!(Stage::Discovery.can_transition_to(Stage::Discovery));
        assert!(Stage::Discovery.can_transition_to(Stage::Planning));
        assert!(Stage::Planning.can_transition_to(Stage::Implementation));
        assert!(Stage::Implementation.can_transition_to(Stage::Validation));
        assert!(Stage::Validation.can_transition_to(Stage::Implementation));
        assert!(Stage::Validation.can_transition_to(Stage::Review));
        assert!(Stage::Review.can_transition_to(Stage::Publication));
        assert!(Stage::Publication.can_transition_to(Stage::Validation));
        assert!(Stage::Publication.can_transition_to(Stage::Terminal));
        assert!(Stage::Terminal.can_transition_to(Stage::Terminal));

        assert!(!Stage::Discovery.can_transition_to(Stage::Implementation));
        assert!(!Stage::Implementation.can_transition_to(Stage::Review));
        assert!(!Stage::Review.can_transition_to(Stage::Terminal));
        assert!(!Stage::Terminal.can_transition_to(Stage::Publication));
    }

    #[test]
    fn public_phase_cannot_infer_aggregated_hosted_progress() {
        assert_eq!(RunPhase::Preparing.hosted_execution_stage(), None);
        assert_eq!(RunPhase::Verifying.hosted_execution_stage(), None);
        assert_eq!(RunPhase::AwaitingReview.hosted_execution_stage(), None);
        assert_eq!(
            RunPhase::Executing.hosted_execution_stage(),
            Some(HostedExecutionStage::Implementation)
        );
        assert_eq!(
            RunPhase::Publishing.hosted_execution_stage(),
            Some(HostedExecutionStage::Publication)
        );
        assert_eq!(
            RunPhase::Failed.hosted_execution_stage(),
            Some(HostedExecutionStage::Terminal)
        );
    }
}
