use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::{
    api::{AgentRun, RustGridClient, Ticket, Worker},
    command,
    config::AppContext,
    git::{Repo, branch_name},
    github::GitHubClient,
    prompt,
};

#[derive(Debug)]
pub struct RunSummary {
    pub ticket_key: String,
    pub branch: String,
    pub commit: String,
    pub pull_request_url: String,
}

struct Reporter<'a> {
    api: &'a RustGridClient,
    run_id: &'a str,
    row_version: Cell<i64>,
    ticket_id: &'a str,
    ticket_row_version: Cell<i64>,
}

impl Reporter<'_> {
    fn step(
        &self,
        name: &str,
        status: &str,
        message: &str,
        metadata: Option<serde_json::Value>,
    ) -> Result<()> {
        println!("[{status:>9}] {message}");
        self.api
            .append_step(self.run_id, name, status, message, metadata)
            .with_context(|| format!("could not report step {name} to RustGrid"))
    }

    fn update_run(&self, status: &str, message: Option<&str>) -> Result<()> {
        let run = self
            .api
            .update_run(self.run_id, self.row_version.get(), status, message)?;
        self.row_version.set(run.row_version);
        Ok(())
    }

    fn set_ticket_status(&self, status: &str) -> Result<()> {
        let version =
            self.api
                .update_ticket_status(self.ticket_id, self.ticket_row_version.get(), status)?;
        self.ticket_row_version.set(version);
        println!("[   status] Ticket is now {status}");
        Ok(())
    }

    fn feedback(&self, message: &str) -> Result<()> {
        println!("[ feedback] {message}");
        self.api.create_comment(
            self.ticket_id,
            &format!("🤖 **RustGrid Agent update**\n\n{message}"),
        )
    }

    fn fail(&self, error: &anyhow::Error) -> Result<()> {
        let message = format!("{error:#}");
        let step_result = self.step("run_failed", "failed", &message, None);
        let comment_result = self.api.create_comment(
            self.ticket_id,
            &format!(
                "⛔ **RustGrid Agent blocked**\n\n{message}\n\nHuman intervention is required before the agent can continue."
            ),
        );
        let ticket_result = self.set_ticket_status("blocked");
        let update_result = self.update_run("failed", Some(&message));
        if let Err(report_error) = step_result {
            eprintln!("[warning] {report_error:#}");
        }
        if let Err(report_error) = update_result {
            eprintln!("[warning] could not mark RustGrid run failed: {report_error:#}");
        }
        if let Err(report_error) = comment_result {
            eprintln!("[warning] could not append blocked ticket comment: {report_error:#}");
        }
        if let Err(report_error) = ticket_result {
            eprintln!("[warning] could not mark ticket blocked: {report_error:#}");
        }
        Ok(())
    }
}

pub fn register(context: &AppContext) -> Result<()> {
    println!("[ starting] Registering RustGrid worker");
    let api = RustGridClient::new(context)?;
    let worker = api.register()?;
    api.heartbeat(&worker.id)?;
    println!(
        "[ complete] Worker {} is registered and healthy ({})",
        worker.id, worker.status
    );
    Ok(())
}

pub fn run_ticket(context: &AppContext, ticket_id: &str, allow_dirty: bool) -> Result<RunSummary> {
    let api = RustGridClient::new(context)?;
    println!("[ starting] Registering worker");
    let worker = api.register()?;
    api.heartbeat(&worker.id)?;
    run_ticket_with_worker(context, &api, &worker, ticket_id, allow_dirty)
}

fn run_ticket_with_worker(
    context: &AppContext,
    api: &RustGridClient,
    worker: &Worker,
    ticket_id: &str,
    allow_dirty: bool,
) -> Result<RunSummary> {
    println!("[ starting] Fetching ticket {ticket_id}");
    let ticket = api.fetch_ticket(ticket_id)?;
    let repo = Repo::discover()?;
    let baseline = repo.ensure_safe(allow_dirty)?;
    let generated_prompt =
        prompt::build(&ticket, &repo.root, &context.config.quality_gate_command)?;
    let run = api
        .claim_ticket(&ticket.id, &worker.id, &generated_prompt)
        .with_context(|| format!("could not claim ticket {}", ticket.key))?;
    execute_claimed(
        context,
        api,
        worker,
        &run,
        &ticket,
        repo,
        baseline,
        generated_prompt,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_claimed(
    context: &AppContext,
    api: &RustGridClient,
    worker: &Worker,
    run: &AgentRun,
    ticket: &Ticket,
    repo: Repo,
    baseline: BTreeSet<String>,
    generated_prompt: String,
) -> Result<RunSummary> {
    let reporter = Reporter {
        api,
        run_id: &run.id,
        row_version: Cell::new(run.row_version),
        ticket_id: &ticket.id,
        ticket_row_version: Cell::new(ticket.row_version),
    };

    let result = (|| {
        reporter.set_ticket_status("in_progress")?;
        reporter.step(
            "ticket_fetched",
            "completed",
            &format!("Fetched ticket {}", ticket.key),
            Some(json!({"ticket_id": ticket.id})),
        )?;
        reporter.step(
            "ticket_claimed",
            "completed",
            &format!("Claimed ticket {}", ticket.key),
            Some(json!({"worker_id": worker.id})),
        )?;
        reporter.step(
            "run_created",
            "completed",
            &format!("Created and claimed agent run {}", run.id),
            None,
        )?;
        execute(
            context,
            api,
            run,
            ticket,
            &reporter,
            repo,
            baseline,
            generated_prompt,
        )
    })();

    match result {
        Ok(summary) => Ok(summary),
        Err(error) => {
            reporter.fail(&error)?;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute(
    context: &AppContext,
    api: &RustGridClient,
    run: &AgentRun,
    ticket: &Ticket,
    reporter: &Reporter<'_>,
    repo: Repo,
    baseline: BTreeSet<String>,
    generated_prompt: String,
) -> Result<RunSummary> {
    if !baseline.is_empty() {
        reporter.step(
            "working_tree_checked",
            "completed",
            &format!(
                "Dirty tree allowed; excluding {} pre-existing path(s) from the commit",
                baseline.len()
            ),
            Some(json!({"excluded_paths": baseline})),
        )?;
    } else {
        reporter.step(
            "working_tree_checked",
            "completed",
            "Git working tree is clean",
            None,
        )?;
    }

    let branch = branch_name(&ticket.key, &ticket.title);
    reporter.step(
        "branch_create",
        "running",
        &format!("Creating branch {branch}"),
        Some(json!({"base": context.config.default_base_branch})),
    )?;
    repo.create_branch(&branch, &context.config.default_base_branch)?;
    reporter.step(
        "branch_create",
        "completed",
        &format!("Created branch {branch}"),
        None,
    )?;

    reporter.step(
        "prompt_built",
        "completed",
        "Built Codex prompt from ticket and repository context",
        Some(json!({"characters": generated_prompt.len()})),
    )?;

    reporter.step("codex", "running", "Running Codex locally", None)?;
    let blocked_action = RefCell::new(None);
    let codex_status = command::streaming_lines(
        &context.codex_command,
        &repo.root,
        Some(&generated_prompt),
        |line| {
            if let Some(message) = feedback_from_output_line(line) {
                if let Some(action) = blocked_action_from_feedback(&message) {
                    blocked_action.replace(Some(action));
                }
                reporter.feedback(&message)?;
            }
            Ok(())
        },
    )?;
    if let Some(action) = blocked_action.into_inner() {
        bail!("human intervention required: {action}");
    }
    if !codex_status.success() {
        bail!("Codex exited with {codex_status}");
    }
    reporter.step("codex", "completed", "Codex finished successfully", None)?;

    reporter.step(
        "quality_gate",
        "running",
        &format!(
            "Running quality gate: {}",
            context.config.quality_gate_command
        ),
        None,
    )?;
    let gate = run_captured(&context.config.quality_gate_command, &repo.root)?;
    print_output(&gate.stdout, &gate.stderr);
    let gate_output = combine_output(&gate.stdout, &gate.stderr);
    let passed = gate.status.success();
    api.report_quality_gate(
        &ticket.id,
        &run.id,
        &context.config.quality_gate_command,
        passed,
        &gate_output,
    )?;
    reporter.step(
        "quality_gate",
        if passed { "completed" } else { "failed" },
        if passed {
            "Quality gate passed"
        } else {
            "Quality gate failed"
        },
        Some(json!({"exit_code": gate.status.code()})),
    )?;
    if !passed {
        bail!("quality gate failed with {}", gate.status);
    }

    let paths = repo.new_agent_paths(&baseline)?;
    if paths.is_empty() {
        bail!("Codex produced no committable changes");
    }
    reporter.step(
        "changes_detected",
        "completed",
        &format!("Found {} agent-created changed path(s)", paths.len()),
        Some(json!({"paths": paths})),
    )?;

    reporter.step("commit", "running", "Committing agent changes", None)?;
    let commit = repo.commit_paths(&paths, &format!("{}: {}", ticket.key, ticket.title))?;
    reporter.step(
        "commit",
        "completed",
        &format!("Created commit {}", short_sha(&commit)),
        Some(json!({"commit": commit})),
    )?;

    reporter.step("push", "running", &format!("Pushing branch {branch}"), None)?;
    let github_token = context.require_github_token()?;
    repo.push(&branch, github_token)?;
    reporter.step(
        "push",
        "completed",
        &format!("Pushed branch {branch}"),
        None,
    )?;

    reporter.step(
        "pull_request",
        "running",
        "Opening GitHub pull request",
        None,
    )?;
    let github = GitHubClient::new(github_token)?;
    let pr = github.create_pull_request(
        &context.config.repo,
        &format!("{}: {}", ticket.key, ticket.title),
        &pull_request_body(ticket, &run.id, &context.config.quality_gate_command),
        &branch,
        &context.config.default_base_branch,
    )?;
    reporter.step(
        "pull_request",
        "completed",
        &format!("Opened pull request #{}", pr.number),
        Some(json!({"url": pr.html_url})),
    )?;

    api.attach_pr(&ticket.id, &run.id, &pr.html_url, pr.number)?;
    reporter.step(
        "rustgrid_attachment",
        "completed",
        "Attached pull request to RustGrid",
        Some(json!({"url": pr.html_url})),
    )?;
    reporter.step(
        "run_complete",
        "completed",
        "Agent run completed successfully",
        None,
    )?;
    reporter.set_ticket_status("done")?;
    reporter.update_run("succeeded", Some(&pr.html_url))?;

    let summary = RunSummary {
        ticket_key: ticket.key.clone(),
        branch,
        commit,
        pull_request_url: pr.html_url,
    };
    print_summary(&summary, &context.config.quality_gate_command);
    Ok(summary)
}

pub fn watch(
    context: &AppContext,
    allow_dirty: bool,
    interval: Duration,
    once: bool,
) -> Result<()> {
    let api = RustGridClient::new(context)?;
    let worker = api.register()?;
    let project_id = api.resolve_project_id(context)?;
    println!("[ watching] Worker {} is polling RustGrid", worker.id);
    let running = Arc::new(AtomicBool::new(true));
    let signal = Arc::clone(&running);
    ctrlc::set_handler(move || signal.store(false, Ordering::SeqCst))
        .context("could not install Ctrl-C handler")?;

    while running.load(Ordering::SeqCst) {
        api.heartbeat(&worker.id)?;
        let repo = Repo::discover()?;
        let baseline = repo.ensure_safe(allow_dirty)?;
        match api.claim_next(&worker.id, &project_id)? {
            Some(run) => {
                let ticket = api.fetch_ticket(&run.ticket_id)?;
                println!("[  claimed] Queue returned {}", ticket.key);
                let generated_prompt =
                    prompt::build(&ticket, &repo.root, &context.config.quality_gate_command)?;
                if let Err(error) = execute_claimed(
                    context,
                    &api,
                    &worker,
                    &run,
                    &ticket,
                    repo,
                    baseline,
                    generated_prompt,
                ) {
                    eprintln!("[error] ticket {} failed: {error:#}", ticket.key);
                }
            }
            None => println!("[     idle] No tickets available"),
        }
        if once {
            break;
        }
        thread::sleep(interval);
    }
    println!("[  stopped] Watcher stopped");
    Ok(())
}

pub fn status(context: &AppContext) -> Result<()> {
    let repo = Repo::discover()?;
    let dirty = repo.dirty_paths()?;
    let parsed_codex = command::parse(&context.codex_command)?;
    let parsed_gate = command::parse(&context.config.quality_gate_command)?;
    let (project_kind, project) = context.project_value();
    println!("RustGrid agent status\n");
    println!("  Config:       {}", context.config_path.display());
    println!("  RustGrid API: {}", context.api_url);
    println!("  Project:      {project_kind}={project}");
    println!(
        "  Repository:   {}/{} ({})",
        context.config.repo.owner,
        context.config.repo.name,
        repo.root.display()
    );
    println!("  Base branch:  {}", context.config.default_base_branch);
    println!("  Codex:        {}", parsed_codex.join(" "));
    println!("  Quality gate: {}", parsed_gate.join(" "));
    println!("  API key:      {}", presence(context.api_key.as_deref()));
    println!(
        "  GitHub token: {}",
        presence(context.github_token.as_deref())
    );
    println!(
        "  Working tree: {}",
        if dirty.is_empty() {
            "clean".into()
        } else {
            format!("dirty ({} path(s))", dirty.len())
        }
    );
    if context.api_key.is_none() || context.github_token.is_none() {
        bail!("status checks failed: required credentials are missing");
    }
    Ok(())
}

fn run_captured(command_text: &str, cwd: &Path) -> Result<command::CommandOutput> {
    let parts = command::parse(command_text)?;
    println!("  $ {}", parts.join(" "));
    command::capture(&parts[0], &parts[1..], cwd)
}

fn combine_output(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_owned(),
        (true, false) => stderr.to_owned(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn print_output(stdout: &str, stderr: &str) {
    if !stdout.is_empty() {
        println!("{stdout}");
    }
    if !stderr.is_empty() {
        eprintln!("{stderr}");
    }
}

fn feedback_from_output_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Ok(event) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return Some(trimmed.to_owned());
    };
    match event.get("type").and_then(serde_json::Value::as_str) {
        Some("item.completed") => {
            let item = event.get("item")?;
            (item.get("type")?.as_str()? == "agent_message")
                .then(|| item.get("text")?.as_str().map(str::to_owned))?
        }
        Some("error") => event
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(|message| format!("Codex error: {message}")),
        _ => None,
    }
}

fn blocked_action_from_feedback(message: &str) -> Option<String> {
    if !message
        .to_ascii_uppercase()
        .contains("RUSTGRID_AGENT_STATUS: BLOCKED")
    {
        return None;
    }
    message
        .lines()
        .find_map(|line| {
            let (label, value) = line.split_once(':')?;
            label
                .trim()
                .eq_ignore_ascii_case("HUMAN_ACTION_REQUIRED")
                .then(|| value.trim().to_owned())
        })
        .filter(|value| !value.is_empty())
        .or_else(|| Some("Codex reported that human intervention is required".to_owned()))
}

fn pull_request_body(ticket: &Ticket, run_id: &str, quality_gate: &str) -> String {
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

fn print_summary(summary: &RunSummary, gate: &str) {
    println!("\nRun complete\n");
    println!("  Ticket:       {}", summary.ticket_key);
    println!("  Branch:       {}", summary.branch);
    println!("  Commit:       {}", summary.commit);
    println!("  Quality gate: passed ({gate})");
    println!("  Pull request: {}", summary.pull_request_url);
}

fn short_sha(sha: &str) -> &str {
    sha.get(..sha.len().min(12)).unwrap_or(sha)
}
fn presence(value: Option<&str>) -> &'static str {
    if value.is_some() { "set" } else { "missing" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_agent_feedback_from_codex_jsonl() {
        let message = r#"{"type":"item.completed","item":{"id":"1","type":"agent_message","text":"Implemented the parser."}}"#;
        assert_eq!(
            feedback_from_output_line(message).as_deref(),
            Some("Implemented the parser.")
        );
        let command = r#"{"type":"item.completed","item":{"id":"2","type":"command_execution","command":"cargo test"}}"#;
        assert_eq!(feedback_from_output_line(command), None);
    }

    #[test]
    fn extracts_explicit_human_action_from_blocked_feedback() {
        let message = "I need a production credential.\nRUSTGRID_AGENT_STATUS: BLOCKED\nHUMAN_ACTION_REQUIRED: Add the signing key.";
        assert_eq!(
            blocked_action_from_feedback(message).as_deref(),
            Some("Add the signing key.")
        );
        assert_eq!(blocked_action_from_feedback("Still working"), None);
    }
}
