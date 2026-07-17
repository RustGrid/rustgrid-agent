use anyhow::Result;

use crate::{
    lifecycle::{AgentRunStatus, RunPhase},
    outcome::{RunOutcome, RunSummary},
    reporting::Reporter,
};

pub(crate) fn finalize(
    outcome: RunOutcome,
    reporter: &Reporter<'_>,
    supervisor_healthy: bool,
) -> Result<RunSummary> {
    match outcome {
        RunOutcome::Succeeded(summary) => {
            if !supervisor_healthy {
                eprintln!("[warning] supervisor connectivity was degraded during the run");
            }
            reporter.report_token_consumption()?;
            reporter.set_phase(RunPhase::Succeeded);
            reporter.update_run(AgentRunStatus::Succeeded, Some(&summary.pull_request_url))?;
            Ok(summary)
        }
        RunOutcome::LeaseLost(error) => {
            let _ = reporter.record_error("run lease ownership was lost");
            Err(error.context("skipped stale terminal updates"))
        }
        RunOutcome::Cancelled(error) => {
            report_consumption_for_unsuccessful_run(reporter);
            reporter.cancel()?;
            Err(error)
        }
        RunOutcome::TimedOut(error) => {
            report_consumption_for_unsuccessful_run(reporter);
            reporter.set_phase(RunPhase::TimedOut);
            reporter.fail(&error)?;
            Err(error)
        }
        RunOutcome::Blocked(error) => {
            report_consumption_for_unsuccessful_run(reporter);
            reporter.fail(&error)?;
            Err(error)
        }
        RunOutcome::Failed(error) => {
            report_consumption_for_unsuccessful_run(reporter);
            reporter.fail_retryable(&error)?;
            Err(error)
        }
    }
}

fn report_consumption_for_unsuccessful_run(reporter: &Reporter<'_>) {
    if let Err(error) = reporter.report_token_consumption() {
        eprintln!("[warning] {error:#}");
    }
}
