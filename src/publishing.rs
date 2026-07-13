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
    github::{CheckRun, GitHubClient, WorkflowRun},
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
        let workflows = github.workflow_runs(requirements.repo, requirements.commit)?;
        let needs_check_fallback = requirements
            .required
            .iter()
            .any(|name| latest_workflow_run(&workflows, name).is_none());
        let checks = if needs_check_fallback {
            github.check_runs(requirements.repo, requirements.commit)?
        } else {
            Vec::new()
        };
        let mut all_passed = true;
        for name in requirements.required {
            let state = latest_workflow_run(&workflows, name)
                .map(|run| (&run.status, run.conclusion.as_ref()))
                .or_else(|| {
                    latest_check_run(&checks, name)
                        .map(|check| (&check.status, check.conclusion.as_ref()))
                });
            match state {
                Some((status, conclusion))
                    if status.is_completed()
                        && conclusion.is_some_and(|value| value.is_success()) => {}
                Some((status, conclusion)) if status.is_completed() => {
                    bail!(
                        "required GitHub workflow {name} concluded as {}",
                        conclusion.map_or("unknown", |value| value.as_str())
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
        .filter(|check| check.name.eq_ignore_ascii_case(name))
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

fn latest_workflow_run<'a>(runs: &'a [WorkflowRun], requirement: &str) -> Option<&'a WorkflowRun> {
    runs.iter()
        .filter(|run| workflow_matches(run, requirement))
        .max_by(|a, b| {
            let a_time = a
                .updated_at
                .as_deref()
                .or(a.created_at.as_deref())
                .unwrap_or("");
            let b_time = b
                .updated_at
                .as_deref()
                .or(b.created_at.as_deref())
                .unwrap_or("");
            (a.run_attempt, a_time, a.id).cmp(&(b.run_attempt, b_time, b.id))
        })
}

fn workflow_matches(run: &WorkflowRun, requirement: &str) -> bool {
    let requirement = requirement.trim();
    if run.name.eq_ignore_ascii_case(requirement) || run.path.eq_ignore_ascii_case(requirement) {
        return true;
    }
    let path = std::path::Path::new(&run.path);
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(requirement))
        || path
            .file_stem()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(requirement))
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

    #[test]
    fn workflow_requirement_matches_display_name_path_filename_and_stem() {
        let run = WorkflowRun {
            id: 1,
            name: "CI".into(),
            path: ".github/workflows/ci.yml".into(),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Success),
            run_attempt: 1,
            created_at: None,
            updated_at: None,
        };

        for requirement in ["CI", "ci", "ci.yml", ".github/workflows/ci.yml"] {
            assert!(workflow_matches(&run, requirement));
        }
    }
}
