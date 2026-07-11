use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::{
    api::{AgentRun, RustGridClient, Ticket, Worker, is_lease_lost},
    command,
    config::AppContext,
    git::{Repo, branch_name},
    github::GitHubClient,
    journal::RunJournal,
    lifecycle::{LifecycleEvent, RunPhase},
    manifest::ExecutionManifest,
    prompt,
    supervisor::{RunSupervisor, RunSupervisorConfig},
    token::GitHubTokenManager,
    workspace::RunWorkspace,
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
    row_version: Arc<AtomicI64>,
    ticket_id: &'a str,
    ticket_row_version: Cell<i64>,
    phase: Cell<RunPhase>,
    sequence: Cell<u64>,
    journal: RefCell<RunJournal>,
    progress_sequence: Cell<u64>,
    run_started: std::time::Instant,
    phase_started: RefCell<std::time::Instant>,
}

impl Reporter<'_> {
    fn enrich_event(&self, event: &mut LifecycleEvent) {
        if let Some(data) = event.data.as_object_mut() {
            data.insert(
                "run_elapsed_ms".into(),
                json!(self.run_started.elapsed().as_millis()),
            );
            data.insert(
                "phase_elapsed_ms".into(),
                json!(self.phase_started.borrow().elapsed().as_millis()),
            );
        }
    }

    fn publish_event(&self, event_kind: &str, event: &LifecycleEvent) -> Result<()> {
        let published_sequence = match self.api.publish_run_event(self.run_id, event_kind, event) {
            Ok(published) => published.sequence,
            Err(first_error) => {
                if is_lease_lost(&first_error) {
                    return Err(first_error);
                }
                if let Some(accepted_sequence) = self.api.find_event_by_client_sequence(
                    self.run_id,
                    self.progress_sequence.get(),
                    event.sequence,
                )? {
                    accepted_sequence
                } else {
                    eprintln!(
                        "[warning] event publish outcome was ambiguous; retrying once: {first_error:#}"
                    );
                    self.api
                        .publish_run_event(self.run_id, event_kind, event)?
                        .sequence
                }
            }
        };
        self.progress_sequence.set(published_sequence);
        self.journal
            .borrow_mut()
            .record_progress_sequence(published_sequence)
    }

    fn step(
        &self,
        name: &str,
        status: &str,
        message: &str,
        metadata: Option<serde_json::Value>,
    ) -> Result<()> {
        println!("[{status:>9}] {message}");
        let sequence = self.sequence.get() + 1;
        self.sequence.set(sequence);
        let mut event = LifecycleEvent::new(
            sequence,
            self.phase.get(),
            format!("step.{name}.{status}"),
            if status == "failed" { "error" } else { "info" },
            message,
            metadata,
        );
        self.enrich_event(&mut event);
        if let Err(error) = self
            .journal
            .borrow_mut()
            .checkpoint(self.phase.get(), sequence)
        {
            eprintln!("[warning] could not persist run checkpoint: {error:#}");
        }
        self.publish_event("progress", &event)?;
        self.api
            .append_step(self.run_id, name, status, message, Some(event.metadata()))
            .with_context(|| format!("could not report step {name} to RustGrid"))
    }

    fn set_phase(&self, phase: RunPhase) {
        self.phase.set(phase);
        self.phase_started.replace(std::time::Instant::now());
        println!("[    phase] {}", phase.as_str());
        if let Err(error) = self
            .journal
            .borrow_mut()
            .checkpoint(phase, self.sequence.get())
        {
            eprintln!("[warning] could not persist run phase: {error:#}");
        }
    }

    fn record_branch(&self, branch: &str) -> Result<()> {
        self.journal.borrow_mut().record_branch(branch)
    }

    fn record_commit(&self, commit: &str) -> Result<()> {
        self.journal.borrow_mut().record_commit(commit)
    }

    fn record_pull_request(&self, url: &str, number: u64) -> Result<()> {
        self.journal.borrow_mut().record_pull_request(url, number)
    }

    fn update_run(&self, status: &str, message: Option<&str>) -> Result<()> {
        let run = self.api.update_run(
            self.run_id,
            self.row_version.load(Ordering::SeqCst),
            status,
            message,
        )?;
        self.row_version.store(run.row_version, Ordering::SeqCst);
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
        let sequence = self.sequence.get() + 1;
        self.sequence.set(sequence);
        let mut event = LifecycleEvent::new(
            sequence,
            self.phase.get(),
            "agent.message",
            "info",
            message,
            None,
        );
        self.enrich_event(&mut event);
        self.publish_event("message", &event)?;
        self.api.create_comment(
            self.ticket_id,
            &format!("🤖 **RustGrid Agent update**\n\n{message}"),
        )
    }

    fn log(&self, message: &str) -> Result<()> {
        if message.trim().is_empty() {
            return Ok(());
        }
        let sequence = self.sequence.get() + 1;
        self.sequence.set(sequence);
        let bounded = truncate(message, 16_000);
        let mut event = LifecycleEvent::new(
            sequence,
            self.phase.get(),
            "quality_gate.output",
            "info",
            "Quality gate produced output",
            Some(json!({"output": bounded})),
        );
        self.enrich_event(&mut event);
        self.publish_event("log", &event)
    }

    fn fail(&self, error: &anyhow::Error) -> Result<()> {
        let message = format!("{error:#}");
        if self.phase.get() != RunPhase::TimedOut {
            self.set_phase(RunPhase::Blocked);
        }
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

    fn cancel(&self) -> Result<()> {
        self.set_phase(RunPhase::Cancelled);
        let step_result = self.step(
            "run_cancelled",
            "cancelled",
            "Agent run cancelled by operator",
            None,
        );
        let comment_result = self.api.create_comment(
            self.ticket_id,
            "🛑 **RustGrid Agent stopped**\n\nThe run was cancelled by the worker operator and can be retried.",
        );
        let ticket_result = self.set_ticket_status("todo");
        let update_result = self.update_run("cancelled", Some("cancelled by operator"));
        step_result?;
        comment_result?;
        ticket_result?;
        update_result
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

pub fn run_ticket(context: &AppContext, ticket_id: &str) -> Result<RunSummary> {
    sweep_workspaces(context)?;
    let api = RustGridClient::new(context)?;
    println!("[ starting] Registering worker");
    let worker = api.register()?;
    api.heartbeat(&worker.id)?;
    run_ticket_with_worker(context, &api, &worker, ticket_id)
}

fn run_ticket_with_worker(
    context: &AppContext,
    api: &RustGridClient,
    worker: &Worker,
    ticket_id: &str,
) -> Result<RunSummary> {
    println!("[ starting] Fetching ticket {ticket_id}");
    let ticket = api.fetch_ticket(ticket_id)?;
    let run = api
        .claim_ticket(
            &ticket.id,
            &worker.id,
            "Claimed by rustgrid-agent; execution context is resolved from the run manifest.",
        )
        .with_context(|| format!("could not claim ticket {}", ticket.key))?;
    execute_claimed(
        context,
        api,
        worker,
        &run,
        &ticket,
        Arc::new(AtomicBool::new(true)),
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_claimed(
    context: &AppContext,
    api: &RustGridClient,
    worker: &Worker,
    run: &AgentRun,
    ticket: &Ticket,
    running: Arc<AtomicBool>,
) -> Result<RunSummary> {
    let row_version = Arc::new(AtomicI64::new(run.row_version));
    let journal_path = RunWorkspace::journal_path(&context.workspace_root, &run.id)?;
    let journal = RunJournal::create(&journal_path, &run.id, &ticket.id)?;
    let progress_sequence = journal.progress_sequence;
    let last_sequence = journal.last_sequence;
    let reporter = Reporter {
        api,
        run_id: &run.id,
        row_version: Arc::clone(&row_version),
        ticket_id: &ticket.id,
        ticket_row_version: Cell::new(ticket.row_version),
        phase: Cell::new(RunPhase::Claimed),
        sequence: Cell::new(last_sequence),
        journal: RefCell::new(journal),
        progress_sequence: Cell::new(progress_sequence),
        run_started: std::time::Instant::now(),
        phase_started: RefCell::new(std::time::Instant::now()),
    };

    let supervisor = RunSupervisor::start(
        api.clone(),
        worker.id.clone(),
        run.id.clone(),
        row_version,
        Arc::clone(&running),
        RunSupervisorConfig {
            heartbeat_interval: Duration::from_secs(context.config.heartbeat_interval_seconds),
            lease_seconds: context.config.lease_seconds,
            run_timeout: Duration::from_secs(context.config.run_timeout_seconds),
        },
    );
    let workspace = RefCell::new(None::<RunWorkspace>);

    let result = (|| {
        let manifest = api
            .execution_manifest(&run.id)
            .with_context(|| format!("could not retrieve execution manifest for run {}", run.id))?;
        manifest.validate(&run.id, &ticket.id)?;
        let manifest_project_matches = context
            .config
            .project_id
            .as_deref()
            .is_some_and(|id| id == manifest.project_id)
            || context
                .config
                .project_key
                .as_deref()
                .is_some_and(|key| key.eq_ignore_ascii_case(&manifest.project_key));
        if !manifest_project_matches || manifest.ticket_key != ticket.key {
            bail!("execution manifest does not match the configured project and fetched ticket");
        }
        let tokens = GitHubTokenManager::new(
            api,
            &run.id,
            &manifest.repository,
            &manifest.required_permissions,
        );
        let clone_token = tokens.token()?;
        let prepared =
            RunWorkspace::prepare(&context.workspace_root, &run.id, &manifest, &clone_token)?;
        let repo = prepared.repo.clone();
        let baseline = if prepared.resumed() {
            BTreeSet::new()
        } else {
            repo.ensure_safe(false)?
        };
        workspace.replace(Some(prepared));
        let generated_prompt =
            prompt::build(ticket, &repo.root, &context.config.quality_gate_command)?;
        reporter.set_ticket_status("in_progress")?;
        reporter.set_phase(RunPhase::Preparing);
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
            &running,
            &manifest,
        )
    })();

    let supervisor_healthy = supervisor.is_healthy();
    let lease_lost = supervisor.lease_lost();
    let timed_out = supervisor.timed_out();
    drop(supervisor);
    if lease_lost {
        bail!(
            "run lease ownership was lost; stopped local execution without publishing terminal state"
        );
    }
    if timed_out {
        reporter.set_phase(RunPhase::TimedOut);
        let timeout = anyhow::anyhow!(
            "agent run timed out after {} seconds",
            context.config.run_timeout_seconds
        );
        reporter.fail(&timeout)?;
        return Err(timeout);
    }
    match result {
        Ok(summary) => {
            if !supervisor_healthy {
                eprintln!("[warning] supervisor connectivity was degraded during the run");
            }
            reporter.set_phase(RunPhase::Succeeded);
            reporter.update_run("succeeded", Some(&summary.pull_request_url))?;
            if let Some(workspace) = workspace.borrow_mut().take() {
                workspace.cleanup()?;
            }
            Ok(summary)
        }
        Err(error) => {
            if is_lease_lost(&error) {
                return Err(
                    error.context("run lease ownership was lost; skipped stale terminal updates")
                );
            }
            if !running.load(Ordering::SeqCst) {
                reporter.cancel()?;
                return Err(error);
            }
            if format!("{error:#}").contains("timed out") {
                reporter.set_phase(RunPhase::TimedOut);
            }
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
    running: &AtomicBool,
    manifest: &ExecutionManifest,
) -> Result<RunSummary> {
    let base_branch = manifest
        .default_branch
        .as_deref()
        .unwrap_or(&context.config.default_base_branch);
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
        Some(json!({"base": base_branch})),
    )?;
    let resumed_branch = repo.checkout_or_create_branch(&branch, base_branch)?;
    reporter.record_branch(&branch)?;
    reporter.step(
        "branch_create",
        "completed",
        &format!(
            "{} branch {branch}",
            if resumed_branch { "Resumed" } else { "Created" }
        ),
        Some(json!({"resumed": resumed_branch})),
    )?;

    let recovered_commit = reporter.journal.borrow().commit.clone();
    let commit = if let Some(commit) = recovered_commit {
        if !repo.has_commit(&commit)? {
            bail!("recovery journal commit {commit} is missing from the run workspace");
        }
        reporter.step(
            "recovery",
            "completed",
            &format!("Resuming from commit {}", short_sha(&commit)),
            Some(json!({"commit": commit})),
        )?;
        commit
    } else {
        implement_and_commit(
            context,
            api,
            run,
            ticket,
            reporter,
            &repo,
            &baseline,
            &generated_prompt,
            running,
        )?
    };

    reporter.step("push", "running", &format!("Pushing branch {branch}"), None)?;
    let tokens = GitHubTokenManager::new(
        api,
        &run.id,
        &manifest.repository,
        &manifest.required_permissions,
    );
    let push_token = tokens.token()?;
    repo.push(&branch, &push_token)?;
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
    let recovered_pr = {
        let journal = reporter.journal.borrow();
        journal
            .pull_request_url
            .clone()
            .zip(journal.pull_request_number)
    };
    let pr = if let Some((html_url, number)) = recovered_pr {
        crate::github::PullRequest { number, html_url }
    } else {
        let pr_token = tokens.token()?;
        let github = GitHubClient::new(&pr_token)?;
        let repo_config = manifest.repo_config()?;
        if let Some(existing) = github.find_open_pull_request(&repo_config, &branch)? {
            existing
        } else {
            github.create_pull_request(
                &repo_config,
                &format!("{}: {}", ticket.key, ticket.title),
                &pull_request_body(ticket, &run.id, &context.config.quality_gate_command),
                &branch,
                base_branch,
            )?
        }
    };
    reporter.record_pull_request(&pr.html_url, pr.number)?;
    reporter.step(
        "pull_request",
        "completed",
        &format!("Opened pull request #{}", pr.number),
        Some(json!({"url": pr.html_url})),
    )?;

    if !manifest.required_workflows.is_empty() {
        wait_for_required_workflows(
            &tokens,
            &manifest.repo_config()?,
            &commit,
            &manifest.required_workflows,
            Duration::from_secs(context.config.command_timeout_seconds),
            running,
            reporter,
        )?;
    }

    api.attach_pr(&ticket.id, &run.id, &pr.html_url, pr.number)?;
    reporter.set_phase(RunPhase::AwaitingReview);
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
    let summary = RunSummary {
        ticket_key: ticket.key.clone(),
        branch,
        commit,
        pull_request_url: pr.html_url,
    };
    print_summary(&summary, &context.config.quality_gate_command);
    Ok(summary)
}

fn wait_for_required_workflows(
    tokens: &GitHubTokenManager<'_>,
    repo: &crate::config::RepoConfig,
    commit: &str,
    required: &[String],
    timeout: Duration,
    running: &AtomicBool,
    reporter: &Reporter<'_>,
) -> Result<()> {
    if required.is_empty() {
        return Ok(());
    }
    reporter.step(
        "required_workflows",
        "running",
        "Waiting for required GitHub workflows",
        Some(json!({"required": required})),
    )?;
    let started = std::time::Instant::now();
    loop {
        if !running.load(Ordering::SeqCst) {
            bail!("required workflow wait cancelled");
        }
        if started.elapsed() >= timeout {
            bail!(
                "required GitHub workflows timed out after {} seconds",
                timeout.as_secs()
            );
        }
        let token = tokens.token()?;
        let github = GitHubClient::new(&token)?;
        let checks = github.check_runs(repo, commit)?;
        let mut all_passed = true;
        for name in required {
            let matching = checks.iter().find(|check| check.name == *name);
            match matching {
                Some(check)
                    if check.status == "completed"
                        && matches!(
                            check.conclusion.as_deref(),
                            Some("success" | "neutral" | "skipped")
                        ) => {}
                Some(check) if check.status == "completed" => {
                    bail!(
                        "required GitHub workflow {name} concluded as {}",
                        check.conclusion.as_deref().unwrap_or("unknown")
                    );
                }
                _ => all_passed = false,
            }
        }
        if all_passed {
            reporter.step(
                "required_workflows",
                "completed",
                "Required GitHub workflows passed",
                Some(json!({"required": required})),
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

#[allow(clippy::too_many_arguments)]
fn implement_and_commit(
    context: &AppContext,
    api: &RustGridClient,
    run: &AgentRun,
    ticket: &Ticket,
    reporter: &Reporter<'_>,
    repo: &Repo,
    baseline: &BTreeSet<String>,
    generated_prompt: &str,
    running: &AtomicBool,
) -> Result<String> {
    reporter.step(
        "prompt_built",
        "completed",
        "Built Codex prompt from ticket and repository context",
        Some(json!({"characters": generated_prompt.len()})),
    )?;
    reporter.set_phase(RunPhase::Executing);
    reporter.step("codex", "running", "Running Codex locally", None)?;
    let blocked_action = RefCell::new(None);
    let codex_status = command::streaming_lines_cancellable(
        &context.codex_command,
        &repo.root,
        Some(generated_prompt),
        running,
        Duration::from_secs(context.config.command_timeout_seconds),
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

    reporter.set_phase(RunPhase::Verifying);
    reporter.step(
        "quality_gate",
        "running",
        &format!(
            "Running quality gate: {}",
            context.config.quality_gate_command
        ),
        None,
    )?;
    let gate = run_captured(
        &context.config.quality_gate_command,
        &repo.root,
        running,
        Duration::from_secs(context.config.command_timeout_seconds),
    )?;
    print_output(&gate.stdout, &gate.stderr);
    let gate_output = combine_output(&gate.stdout, &gate.stderr);
    reporter.log(&gate_output)?;
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

    reporter.set_phase(RunPhase::Publishing);
    let paths = repo.new_agent_paths(baseline)?;
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
    reporter.record_commit(&commit)?;
    reporter.step(
        "commit",
        "completed",
        &format!("Created commit {}", short_sha(&commit)),
        Some(json!({"commit": commit})),
    )?;
    Ok(commit)
}

pub fn watch(context: &AppContext, interval: Duration, once: bool) -> Result<()> {
    sweep_workspaces(context)?;
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
        match api.claim_next(&worker.id, &project_id)? {
            Some(run) => {
                let ticket = api.fetch_ticket(&run.ticket_id)?;
                println!("[  claimed] Queue returned {}", ticket.key);
                if let Err(error) =
                    execute_claimed(context, &api, &worker, &run, &ticket, Arc::clone(&running))
                {
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

fn sweep_workspaces(context: &AppContext) -> Result<()> {
    let removed = RunWorkspace::sweep_stale(
        &context.workspace_root,
        Duration::from_secs(
            context
                .config
                .failed_workspace_retention_hours
                .saturating_mul(3600),
        ),
    )?;
    if removed > 0 {
        println!("[  cleanup] Removed {removed} expired run workspace(s)");
    }
    Ok(())
}

pub fn status(context: &AppContext) -> Result<()> {
    let local_repo = Repo::discover().ok();
    let dirty = local_repo
        .as_ref()
        .map(Repo::dirty_paths)
        .transpose()?
        .unwrap_or_default();
    let parsed_codex = command::parse(&context.codex_command)?;
    let parsed_gate = command::parse(&context.config.quality_gate_command)?;
    let (project_kind, project) = context.project_value();
    println!("RustGrid agent status\n");
    println!("  Config:       {}", context.config_path.display());
    println!("  RustGrid API: {}", context.api_url);
    println!("  Project:      {project_kind}={project}");
    println!(
        "  Repository:   {}/{}",
        context.config.repo.owner, context.config.repo.name
    );
    println!("  Workspaces:   {}", context.workspace_root.display());
    if let Some(repo) = &local_repo {
        println!("  Local repo:   {}", repo.root.display());
    }
    println!("  Base branch:  {}", context.config.default_base_branch);
    println!("  Codex:        {}", parsed_codex.join(" "));
    println!("  Quality gate: {}", parsed_gate.join(" "));
    println!(
        "  Heartbeat:    every {}s",
        context.config.heartbeat_interval_seconds
    );
    println!("  Run lease:    {}s", context.config.lease_seconds);
    println!("  API key:      {}", presence(context.api_key.as_deref()));
    println!("  GitHub token: brokered per run by RustGrid");
    println!(
        "  Working tree: {}",
        if local_repo.is_none() {
            "not applicable (isolated workspace mode)".into()
        } else if dirty.is_empty() {
            "clean".into()
        } else {
            format!("dirty ({} path(s))", dirty.len())
        }
    );
    if context.api_key.is_none() {
        bail!("status checks failed: required credentials are missing");
    }
    Ok(())
}

fn run_captured(
    command_text: &str,
    cwd: &Path,
    running: &AtomicBool,
    timeout: Duration,
) -> Result<command::CommandOutput> {
    command::capture_cancellable(command_text, cwd, running, timeout)
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

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
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
