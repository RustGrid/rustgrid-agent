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
    pub const fn should_retain_sandbox(&self) -> bool {
        !matches!(self, Self::Succeeded(_))
    }

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

    #[test]
    fn resolves_gateway_outages_as_retryable_failures() {
        let result = Err(anyhow::anyhow!(
            "RustGrid github-token returned 504 Gateway Timeout"
        ));
        assert!(matches!(
            RunOutcome::resolve(result, false, false, true, 30),
            RunOutcome::Failed(_)
        ));
    }

    #[test]
    fn retains_sandboxes_for_every_unsuccessful_terminal_outcome() {
        let failure = || anyhow::anyhow!("failed");
        assert!(RunOutcome::Blocked(failure()).should_retain_sandbox());
        assert!(RunOutcome::Failed(failure()).should_retain_sandbox());
        assert!(RunOutcome::TimedOut(failure()).should_retain_sandbox());
        assert!(RunOutcome::Cancelled(failure()).should_retain_sandbox());
        assert!(RunOutcome::LeaseLost(failure()).should_retain_sandbox());
        assert!(
            !RunOutcome::Succeeded(RunSummary {
                ticket_key: "RG-1".into(),
                branch: "agent/rg-1".into(),
                commit: "abc".into(),
                pull_request_url: "https://github.com/o/r/pull/1".into(),
            })
            .should_retain_sandbox()
        );
    }
}
