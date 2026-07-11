use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use anyhow::{Result, bail};
use serde_json::json;

use crate::{
    api::Ticket,
    config::RepoConfig,
    github::{CheckRun, GitHubClient},
    lifecycle::StepStatus,
    outcome::RunSummary,
    reporting::Reporter,
    run_error::RunFailure,
    token::GitHubTokenManager,
};

pub(crate) struct WorkflowRequirements<'a> {
    pub repo: &'a RepoConfig,
    pub web_base_url: &'a str,
    pub commit: &'a str,
    pub required: &'a [String],
    pub timeout: Duration,
}

pub(crate) fn wait_for_required_workflows(
    tokens: &GitHubTokenManager<'_>,
    requirements: WorkflowRequirements<'_>,
    running: &AtomicBool,
    reporter: &Reporter<'_>,
) -> Result<()> {
    if requirements.required.is_empty() {
        return Ok(());
    }
    reporter.step(
        "required_workflows",
        StepStatus::Running,
        "Waiting for required GitHub workflows",
        Some(json!({"required": requirements.required})),
    )?;
    let started = std::time::Instant::now();
    loop {
        if !running.load(Ordering::SeqCst) {
            bail!("required workflow wait cancelled");
        }
        if started.elapsed() >= requirements.timeout {
            return Err(RunFailure::RequiredWorkflowsTimedOut {
                seconds: requirements.timeout.as_secs(),
            }
            .into());
        }
        let token = tokens.token()?;
        let github = GitHubClient::new(&token, requirements.web_base_url)?;
        let checks = github.check_runs(requirements.repo, requirements.commit)?;
        let mut all_passed = true;
        for name in requirements.required {
            match latest_check_run(&checks, name) {
                Some(check)
                    if check.status.is_completed()
                        && check
                            .conclusion
                            .as_ref()
                            .is_some_and(|value| value.is_success()) => {}
                Some(check) if check.status.is_completed() => {
                    bail!(
                        "required GitHub workflow {name} concluded as {}",
                        check
                            .conclusion
                            .as_ref()
                            .map_or("unknown", |value| value.as_str())
                    );
                }
                _ => all_passed = false,
            }
        }
        if all_passed {
            reporter.step(
                "required_workflows",
                StepStatus::Completed,
                "Required GitHub workflows passed",
                Some(json!({"required": requirements.required})),
            )?;
            return Ok(());
        }
        for _ in 0..20 {
            if !running.load(Ordering::SeqCst) {
                bail!("required workflow wait cancelled");
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
}

fn latest_check_run<'a>(checks: &'a [CheckRun], name: &str) -> Option<&'a CheckRun> {
    checks
        .iter()
        .filter(|check| check.name == name)
        .max_by(|a, b| {
            let a_time = a
                .completed_at
                .as_deref()
                .or(a.started_at.as_deref())
                .unwrap_or("");
            let b_time = b
                .completed_at
                .as_deref()
                .or(b.started_at.as_deref())
                .unwrap_or("");
            (a_time, a.id).cmp(&(b_time, b.id))
        })
}

pub(crate) fn pull_request_body(ticket: &Ticket, run_id: &str, quality_gate: &str) -> String {
    format!(
        "Implements RustGrid ticket **{}**.\n\n{}\n\n### Verification\n\n- `{}`\n\nRustGrid agent run: `{}`\n",
        ticket.key,
        ticket
            .description
            .as_deref()
            .unwrap_or("No description provided."),
        quality_gate,
        run_id
    )
}

pub(crate) fn print_summary(summary: &RunSummary, gate: &str) {
    println!("\nRun complete\n");
    println!("  Ticket:       {}", summary.ticket_key);
    println!("  Branch:       {}", summary.branch);
    println!("  Commit:       {}", summary.commit);
    println!("  Quality gate: passed ({gate})");
    println!("  Pull request: {}", summary.pull_request_url);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{CheckConclusion, CheckStatus};

    #[test]
    fn workflow_reconciliation_uses_the_latest_run() {
        let checks = vec![
            CheckRun {
                id: 1,
                name: "CI".into(),
                status: CheckStatus::Completed,
                conclusion: Some(CheckConclusion::Failure),
                started_at: Some("2026-07-11T10:00:00Z".into()),
                completed_at: Some("2026-07-11T10:01:00Z".into()),
            },
            CheckRun {
                id: 2,
                name: "CI".into(),
                status: CheckStatus::Completed,
                conclusion: Some(CheckConclusion::Success),
                started_at: Some("2026-07-11T10:02:00Z".into()),
                completed_at: Some("2026-07-11T10:03:00Z".into()),
            },
        ];
        assert_eq!(latest_check_run(&checks, "CI").unwrap().id, 2);
    }
}
