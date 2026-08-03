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
            reporter.update_run(AgentRunStatus::Succeeded, Some(summary.output_summary()))?;
            Ok(summary)
        }
        RunOutcome::LeaseLost(failure) => {
            let _ = reporter.record_error("run lease ownership was lost");
            let error = anyhow::Error::new(failure);
            Err(error.context("skipped stale terminal updates"))
        }
        RunOutcome::Cancelled(failure) => {
            report_consumption_for_unsuccessful_run(reporter);
            reporter.cancel()?;
            Err(anyhow::Error::new(failure))
        }
        RunOutcome::TimedOut(failure) => {
            report_consumption_for_unsuccessful_run(reporter);
            reporter.set_phase(RunPhase::TimedOut);
            let error = anyhow::Error::new(failure);
            reporter.fail(&error)?;
            Err(error)
        }
        RunOutcome::Blocked(failure) => {
            report_consumption_for_unsuccessful_run(reporter);
            let error = anyhow::Error::new(failure);
            reporter.fail(&error)?;
            Err(error)
        }
        RunOutcome::Failed(failure) => {
            report_consumption_for_unsuccessful_run(reporter);
            let error = anyhow::Error::new(failure);
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
