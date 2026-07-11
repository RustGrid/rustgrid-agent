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
                | (Self::Verifying, Self::Publishing)
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
        assert!(!RunPhase::Succeeded.can_transition_to(RunPhase::Executing));
        assert!(!RunPhase::Claimed.can_transition_to(RunPhase::Succeeded));
        assert_eq!(StepStatus::Failed.severity(), "error");
        assert_eq!(TicketStatus::AwaitingReview.as_str(), "awaiting_review");
        assert_eq!(AgentRunStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(WorkerStatus::Busy.as_str(), "busy");
    }
}
