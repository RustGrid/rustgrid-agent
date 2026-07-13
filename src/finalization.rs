use std::cell::RefCell;

use anyhow::Result;

use crate::{
    lifecycle::{AgentRunStatus, RunPhase},
    outcome::{RunOutcome, RunSummary},
    reporting::Reporter,
    workspace::RunWorkspace,
};

pub(crate) fn finalize(
    outcome: RunOutcome,
    reporter: &Reporter<'_>,
    workspace: &RefCell<Option<RunWorkspace>>,
    supervisor_healthy: bool,
) -> Result<RunSummary> {
    match outcome {
        RunOutcome::Succeeded(summary) => {
            if !supervisor_healthy {
                eprintln!("[warning] supervisor connectivity was degraded during the run");
            }
            reporter.set_phase(RunPhase::Succeeded);
            reporter.update_run(AgentRunStatus::Succeeded, Some(&summary.pull_request_url))?;
            if let Some(workspace) = workspace.borrow_mut().take() {
                workspace.cleanup()?;
            }
            Ok(summary)
        }
        RunOutcome::LeaseLost(error) => {
            let _ = reporter.record_error("run lease ownership was lost");
            Err(error.context("skipped stale terminal updates"))
        }
        RunOutcome::Cancelled(error) => {
            reporter.cancel()?;
            Err(error)
        }
        RunOutcome::TimedOut(error) => {
            reporter.set_phase(RunPhase::TimedOut);
            reporter.fail(&error)?;
            Err(error)
        }
        RunOutcome::Blocked(error) => {
            reporter.fail(&error)?;
            Err(error)
        }
        RunOutcome::Failed(error) => {
            reporter.fail_retryable(&error)?;
            Err(error)
        }
    }
}
