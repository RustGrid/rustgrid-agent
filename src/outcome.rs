use crate::error::{
    CancellationError, ExecutionFailure, ExecutionFailureKind, ExecutionResult, TerminalOutcome,
};

/// Canonical hosted-domain outcome. The general local-run `RunOutcome` below
/// remains responsible for process and supervisor failures.
pub use crate::execution_graph::MissionOutcome as HostedMissionOutcome;

#[derive(Debug)]
pub struct RunSummary {
    pub ticket_key: String,
    pub branch: String,
    pub commit: String,
    pub pull_request_url: String,
    pub direct_operation_summary: Option<String>,
}

impl RunSummary {
    pub fn output_summary(&self) -> &str {
        self.direct_operation_summary
            .as_deref()
            .unwrap_or(&self.pull_request_url)
    }
}

#[derive(Debug)]
pub enum RunOutcome {
    Succeeded(RunSummary),
    Blocked(ExecutionFailure),
    Cancelled(ExecutionFailure),
    TimedOut(ExecutionFailure),
    LeaseLost(ExecutionFailure),
    Failed(ExecutionFailure),
}

impl RunOutcome {
    pub const fn should_retain_sandbox(&self) -> bool {
        !matches!(self, Self::Succeeded(_))
    }

    pub fn resolve(
        result: ExecutionResult<RunSummary>,
        lease_lost: bool,
        timed_out: bool,
        execution_running: bool,
        shutdown_requested: bool,
        timeout_seconds: u64,
    ) -> Self {
        if lease_lost {
            return Self::LeaseLost(ExecutionFailure::new(
                ExecutionFailureKind::LeaseLost {
                    operation: "run supervision".into(),
                },
                "run lease ownership was lost; stopped local execution without publishing terminal state",
            ));
        }
        if timed_out {
            return Self::TimedOut(ExecutionFailure::new(
                ExecutionFailureKind::TimedOut {
                    seconds: Some(timeout_seconds),
                },
                format!("agent run timed out after {timeout_seconds} seconds"),
            ));
        }
        match result {
            Ok(summary) => Self::Succeeded(summary),
            Err(_error) if shutdown_requested => Self::Cancelled(ExecutionFailure::new(
                ExecutionFailureKind::Shutdown,
                "worker shutdown interrupted the agent run",
            )),
            Err(_error) if !execution_running => Self::Cancelled(ExecutionFailure::new(
                ExecutionFailureKind::Cancellation(CancellationError::Requested),
                "agent run was cancelled",
            )),
            Err(error) => match error.terminal_outcome() {
                TerminalOutcome::LeaseLost => Self::LeaseLost(error),
                TerminalOutcome::Cancelled => Self::Cancelled(error),
                TerminalOutcome::TimedOut => Self::TimedOut(error),
                TerminalOutcome::Blocked => Self::Blocked(error),
                TerminalOutcome::Failed => Self::Failed(error),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::InfrastructureError;
    use crate::run_error::RunFailure;

    #[test]
    fn resolves_terminal_signals_before_generic_errors() {
        let result = Err(ExecutionFailure::new(
            ExecutionFailureKind::Infrastructure(InfrastructureError {
                component: "transport".into(),
                retryable: true,
            }),
            "transport failed",
        ));
        assert!(matches!(
            RunOutcome::resolve(result, true, false, true, false, 30),
            RunOutcome::LeaseLost(_)
        ));
    }

    #[test]
    fn resolves_human_handoffs_as_blocked() {
        let result = Err(ExecutionFailure::from_anyhow(anyhow::Error::new(
            RunFailure::HumanIntervention {
                action: "approve access".into(),
            },
        )));
        assert!(matches!(
            RunOutcome::resolve(result, false, false, true, false, 30),
            RunOutcome::Blocked(_)
        ));
    }

    #[test]
    fn resolves_gateway_outages_as_retryable_failures() {
        let result = Err(ExecutionFailure::new(
            ExecutionFailureKind::Infrastructure(InfrastructureError {
                component: "control plane".into(),
                retryable: true,
            }),
            "RustGrid github-token returned 504 Gateway Timeout",
        ));
        assert!(matches!(
            RunOutcome::resolve(result, false, false, true, false, 30),
            RunOutcome::Failed(_)
        ));
    }

    #[test]
    fn retains_sandboxes_for_every_unsuccessful_terminal_outcome() {
        let failure = || {
            ExecutionFailure::new(
                ExecutionFailureKind::Infrastructure(InfrastructureError {
                    component: "test".into(),
                    retryable: false,
                }),
                "failed",
            )
        };
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
                direct_operation_summary: None,
            })
            .should_retain_sandbox()
        );
    }
}
