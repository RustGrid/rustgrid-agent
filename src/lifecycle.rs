use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
    }
}
