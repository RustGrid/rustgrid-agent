use crate::run_error::{RunErrorKind, classify};

#[derive(Debug)]
pub struct RunSummary {
    pub ticket_key: String,
    pub branch: String,
    pub commit: String,
    pub pull_request_url: String,
}

#[derive(Debug)]
pub enum RunOutcome {
    Succeeded(RunSummary),
    Blocked(anyhow::Error),
    Cancelled(anyhow::Error),
    TimedOut(anyhow::Error),
    LeaseLost(anyhow::Error),
    Failed(anyhow::Error),
}

impl RunOutcome {
    pub fn resolve(
        result: anyhow::Result<RunSummary>,
        lease_lost: bool,
        timed_out: bool,
        execution_running: bool,
        timeout_seconds: u64,
    ) -> Self {
        if lease_lost {
            return Self::LeaseLost(anyhow::anyhow!(
                "run lease ownership was lost; stopped local execution without publishing terminal state"
            ));
        }
        if timed_out {
            return Self::TimedOut(anyhow::anyhow!(
                "agent run timed out after {timeout_seconds} seconds"
            ));
        }
        match result {
            Ok(summary) => Self::Succeeded(summary),
            Err(error) if !execution_running => Self::Cancelled(error),
            Err(error) => match classify(&error) {
                RunErrorKind::LeaseLost => Self::LeaseLost(error),
                RunErrorKind::Cancelled => Self::Cancelled(error),
                RunErrorKind::TimedOut => Self::TimedOut(error),
                RunErrorKind::HumanBlocked
                | RunErrorKind::PolicyViolation
                | RunErrorKind::Authentication
                | RunErrorKind::ExternalPermanent => Self::Blocked(error),
                RunErrorKind::Transient | RunErrorKind::Invariant => Self::Failed(error),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_error::RunFailure;

    #[test]
    fn resolves_terminal_signals_before_generic_errors() {
        let result = Err(anyhow::anyhow!("transport failed"));
        assert!(matches!(
            RunOutcome::resolve(result, true, false, true, 30),
            RunOutcome::LeaseLost(_)
        ));
    }

    #[test]
    fn resolves_human_handoffs_as_blocked() {
        let result = Err(RunFailure::HumanIntervention {
            action: "approve access".into(),
        }
        .into());
        assert!(matches!(
            RunOutcome::resolve(result, false, false, true, 30),
            RunOutcome::Blocked(_)
        ));
    }
}
