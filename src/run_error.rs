use crate::{api::is_lease_lost, command::CommandFailure};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunErrorKind {
    Transient,
    LeaseLost,
    Cancelled,
    TimedOut,
    HumanBlocked,
    PolicyViolation,
    Authentication,
    ExternalPermanent,
    Invariant,
}

#[derive(Debug)]
pub enum RunFailure {
    RequiredWorkflowsTimedOut { seconds: u64 },
    HumanIntervention { action: String },
    PolicyViolation { detail: String },
    Invariant { detail: String },
}

impl std::fmt::Display for RunFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequiredWorkflowsTimedOut { seconds } => write!(
                formatter,
                "required GitHub workflows timed out after {seconds} seconds"
            ),
            Self::HumanIntervention { action } => {
                write!(formatter, "human intervention required: {action}")
            }
            Self::PolicyViolation { detail } => write!(formatter, "policy violation: {detail}"),
            Self::Invariant { detail } => write!(formatter, "internal invariant failed: {detail}"),
        }
    }
}

impl std::error::Error for RunFailure {}

pub fn classify(error: &anyhow::Error) -> RunErrorKind {
    if is_lease_lost(error) {
        return RunErrorKind::LeaseLost;
    }
    if let Some(failure) = error.downcast_ref::<CommandFailure>() {
        return match failure {
            CommandFailure::Cancelled => RunErrorKind::Cancelled,
            CommandFailure::TimedOut { .. } => RunErrorKind::TimedOut,
            CommandFailure::OutputLimit { .. } => RunErrorKind::PolicyViolation,
        };
    }
    if let Some(failure) = error.downcast_ref::<RunFailure>() {
        return match failure {
            RunFailure::RequiredWorkflowsTimedOut { .. } => RunErrorKind::TimedOut,
            RunFailure::HumanIntervention { .. } => RunErrorKind::HumanBlocked,
            RunFailure::PolicyViolation { .. } => RunErrorKind::PolicyViolation,
            RunFailure::Invariant { .. } => RunErrorKind::Invariant,
        };
    }

    let message = format!("{error:#}").to_ascii_lowercase();
    if message.contains("401") || message.contains("403") || message.contains("unauthorized") {
        RunErrorKind::Authentication
    } else if message.contains("timed out")
        || message.contains("connection")
        || message.contains("temporarily unavailable")
        || message.contains("502")
        || message.contains("503")
        || message.contains("504")
    {
        RunErrorKind::Transient
    } else {
        RunErrorKind::ExternalPermanent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_typed_failures_without_message_matching() {
        assert_eq!(
            classify(
                &RunFailure::HumanIntervention {
                    action: "approve access".into()
                }
                .into()
            ),
            RunErrorKind::HumanBlocked
        );
        assert_eq!(
            classify(&CommandFailure::TimedOut { seconds: 10 }.into()),
            RunErrorKind::TimedOut
        );
        assert_eq!(
            classify(
                &RunFailure::Invariant {
                    detail: "missing commit".into()
                }
                .into()
            ),
            RunErrorKind::Invariant
        );
    }
}
