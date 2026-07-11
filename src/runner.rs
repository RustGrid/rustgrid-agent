use std::{
    cell::RefCell,
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
    api::{AgentRun, RustGridClient, Ticket, Worker},
    command,
    config::AppContext,
    coordinator::CoordinatorHealth,
    git::{Repo, branch_name},
    github::GitHubClient,
    journal::{RecoveryPlan, RunJournal},
    lifecycle::{AgentRunStatus, RunPhase, StepStatus, TicketStatus, WorkerStatus},
    manifest::ExecutionManifest,
    outcome::{RunOutcome, RunSummary},
    prompt,
    publishing::{
        WorkflowRequirements, print_summary, pull_request_body, wait_for_required_workflows,
    },
    reporting::{Reporter, console_event},
    run_error::RunFailure,
    shutdown,
    supervisor::{RunSupervisor, RunSupervisorConfig},
    token::GitHubTokenManager,
    workspace::RunWorkspace,
};

pub fn register(context: &AppContext) -> Result<()> {
    console_event("starting", "Registering RustGrid worker", "36");
    let api = RustGridClient::new(context)?;
    let worker = api.register()?;
    api.heartbeat(&worker.id)?;
    println!(
        "[ complete] Worker {} is registered and healthy ({})",
        worker.id,
        worker.status.as_str()
    );
    Ok(())
}

pub fn run_ticket(context: &AppContext, ticket_id: &str) -> Result<RunSummary> {
    sweep_workspaces(context)?;
    let api = RustGridClient::new(context)?;
    console_event("starting", "Registering worker", "36");
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
    console_event("starting", &format!("Fetching ticket {ticket_id}"), "36");
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
    let reporter = Reporter::new(
        api,
        &run.id,
        Arc::clone(&row_version),
        &ticket.id,
        ticket.row_version,
        journal,
    );
    let manifest = api
        .execution_manifest(&run.id)
        .with_context(|| format!("could not retrieve execution manifest for run {}", run.id))?;
    manifest.validate(&run.id, &ticket.id)?;
    let execution_policy = manifest.policy()?;

    let supervisor = RunSupervisor::start(
        api.clone(),
        worker.id.clone(),
        run.id.clone(),
        row_version,
        Arc::clone(&running),
        RunSupervisorConfig {
            heartbeat_interval: Duration::from_secs(context.config.heartbeat_interval_seconds),
            lease_seconds: context.config.lease_seconds,
            run_timeout: Duration::from_secs(execution_policy.timeout_seconds),
        },
    );
    let workspace = RefCell::new(None::<RunWorkspace>);

    let result = (|| {
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
        let workspace_bytes = prepared.enforce_size_limit(context.config.max_workspace_bytes)?;
        let workspace_resumed = prepared.resumed();
        let repo = prepared.repo.clone();
        let baseline = if workspace_resumed {
            BTreeSet::new()
        } else {
            repo.ensure_safe(false)?
        };
        workspace.replace(Some(prepared));
        let required_gates = execution_policy
            .quality_gates
            .iter()
            .filter(|gate| gate.required)
            .map(|gate| gate.command.as_str())
            .collect::<Vec<_>>()
            .join(" && ");
        let generated_prompt = prompt::build(ticket, &repo.root, &required_gates)?;
        reporter.set_ticket_status(TicketStatus::InProgress)?;
        reporter.set_phase(RunPhase::Preparing);
        reporter.step(
            "ticket_fetched",
            StepStatus::Completed,
            &format!("Fetched ticket {}", ticket.key),
            Some(json!({"ticket_id": ticket.id})),
        )?;
        reporter.step(
            "ticket_claimed",
            StepStatus::Completed,
            &format!("Claimed ticket {}", ticket.key),
            Some(json!({"worker_id": worker.id})),
        )?;
        reporter.step(
            "run_created",
            StepStatus::Completed,
            &format!("Created and claimed agent run {}", run.id),
            None,
        )?;
        reporter.step(
            "workspace_prepared",
            StepStatus::Completed,
            "Prepared isolated repository workspace",
            Some(json!({"bytes": workspace_bytes, "resumed": workspace_resumed})),
        )?;
        execute(ExecutionContext {
            app: context,
            api,
            run,
            ticket,
            reporter: &reporter,
            repo,
            baseline,
            prompt: generated_prompt,
            running: &running,
            manifest: &manifest,
        })
    })();
    let result = result.and_then(|summary| {
        if let Some(workspace) = workspace.borrow().as_ref() {
            workspace.enforce_size_limit(context.config.max_workspace_bytes)?;
        }
        Ok(summary)
    });

    let supervisor_healthy = supervisor.is_healthy();
    let lease_lost = supervisor.lease_lost();
    let timed_out = supervisor.timed_out();
    drop(supervisor);
    let outcome = RunOutcome::resolve(
        result,
        lease_lost,
        timed_out,
        running.load(Ordering::SeqCst),
        execution_policy.timeout_seconds,
    );
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
        RunOutcome::Blocked(error) | RunOutcome::Failed(error) => {
            reporter.fail(&error)?;
            Err(error)
        }
    }
}

struct ExecutionContext<'a> {
    app: &'a AppContext,
    api: &'a RustGridClient,
    run: &'a AgentRun,
    ticket: &'a Ticket,
    reporter: &'a Reporter<'a>,
    repo: Repo,
    baseline: BTreeSet<String>,
    prompt: String,
    running: &'a AtomicBool,
    manifest: &'a ExecutionManifest,
}

fn execute(execution: ExecutionContext<'_>) -> Result<RunSummary> {
    let ExecutionContext {
        app: context,
        api,
        run,
        ticket,
        reporter,
        repo,
        baseline,
        prompt: generated_prompt,
        running,
        manifest,
    } = execution;
    let policy = manifest.policy()?;
    let gate_summary = policy
        .quality_gates
        .iter()
        .filter(|gate| gate.required)
        .map(|gate| gate.command.as_str())
        .collect::<Vec<_>>()
        .join(" && ");
    let base_branch = manifest
        .default_branch
        .as_deref()
        .unwrap_or(&context.config.default_base_branch);
    if !baseline.is_empty() {
        reporter.step(
            "working_tree_checked",
            StepStatus::Completed,
            &format!(
                "Dirty tree allowed; excluding {} pre-existing path(s) from the commit",
                baseline.len()
            ),
            Some(json!({"excluded_paths": baseline})),
        )?;
    } else {
        reporter.step(
            "working_tree_checked",
            StepStatus::Completed,
            "Git working tree is clean",
            None,
        )?;
    }

    let branch = branch_name(&ticket.key, &ticket.title);
    reporter.step(
        "branch_create",
        StepStatus::Running,
        &format!("Creating branch {branch}"),
        Some(json!({"base": base_branch})),
    )?;
    let resumed_branch = repo.checkout_or_create_branch(&branch, base_branch)?;
    reporter.record_branch(&branch)?;
    reporter.step(
        "branch_create",
        StepStatus::Completed,
        &format!(
            "{} branch {branch}",
            if resumed_branch { "Resumed" } else { "Created" }
        ),
        Some(json!({"resumed": resumed_branch})),
    )?;

    let recovery = reporter.recovery_plan()?;
    let recovered_commit = match &recovery {
        RecoveryPlan::Fresh => None,
        RecoveryPlan::ResumeFromCommit { commit }
        | RecoveryPlan::ResumeFromPullRequest { commit, .. } => Some(commit.clone()),
    };
    let commit = if let Some(commit) = recovered_commit {
        if !repo.has_commit(&commit)? {
            bail!("recovery journal commit {commit} is missing from the run workspace");
        }
        reporter.step(
            "recovery",
            StepStatus::Completed,
            &format!("Resuming from commit {}", short_sha(&commit)),
            Some(json!({"commit": commit})),
        )?;
        commit
    } else {
        implement_and_commit(ImplementationContext {
            app: context,
            api,
            run,
            ticket,
            reporter,
            repo: &repo,
            baseline: &baseline,
            prompt: &generated_prompt,
            running,
            manifest,
        })?
    };

    if reporter.phase() == RunPhase::Preparing {
        reporter.set_phase(RunPhase::Publishing);
    }

    reporter.step(
        "push",
        StepStatus::Running,
        &format!("Pushing branch {branch}"),
        None,
    )?;
    let tokens = GitHubTokenManager::new(
        api,
        &run.id,
        &manifest.repository,
        &manifest.required_permissions,
    );
    let push_token = tokens.token()?;
    let pushed = repo.push(&branch, &commit, &push_token, &manifest.web_base_url)?;
    reporter.step(
        "push",
        StepStatus::Completed,
        &format!("Pushed branch {branch}"),
        Some(json!({"pushed": pushed, "commit": commit})),
    )?;

    reporter.step(
        "pull_request",
        StepStatus::Running,
        "Opening GitHub pull request",
        None,
    )?;
    let recovered_pr = match recovery {
        RecoveryPlan::ResumeFromPullRequest { url, number, .. } => Some((url, number)),
        RecoveryPlan::Fresh | RecoveryPlan::ResumeFromCommit { .. } => None,
    };
    let pr = if let Some((html_url, number)) = recovered_pr {
        crate::github::PullRequest { number, html_url }
    } else {
        let pr_token = tokens.token()?;
        let github = GitHubClient::new(&pr_token, &manifest.web_base_url)?;
        let repo_config = manifest.repo_config()?;
        if let Some(existing) = github.find_open_pull_request(&repo_config, &branch)? {
            existing
        } else {
            github.create_pull_request(
                &repo_config,
                &format!("{}: {}", ticket.key, ticket.title),
                &pull_request_body(ticket, &run.id, &gate_summary),
                &branch,
                base_branch,
            )?
        }
    };
    reporter.record_pull_request(&pr.html_url, pr.number)?;
    reporter.step(
        "pull_request",
        StepStatus::Completed,
        &format!("Opened pull request #{}", pr.number),
        Some(json!({"url": pr.html_url})),
    )?;

    if !manifest.required_workflows.is_empty() {
        wait_for_required_workflows(
            &tokens,
            WorkflowRequirements {
                repo: &manifest.repo_config()?,
                web_base_url: &manifest.web_base_url,
                commit: &commit,
                required: &manifest.required_workflows,
                timeout: Duration::from_secs(policy.timeout_seconds),
            },
            running,
            reporter,
        )?;
    }

    api.attach_pr(&ticket.id, &run.id, &pr.html_url, pr.number)?;
    reporter.set_phase(RunPhase::AwaitingReview);
    reporter.step(
        "rustgrid_attachment",
        StepStatus::Completed,
        "Attached pull request to RustGrid",
        Some(json!({"url": pr.html_url})),
    )?;
    reporter.step(
        "run_complete",
        StepStatus::Completed,
        "Agent run completed successfully",
        None,
    )?;
    reporter.set_ticket_status(TicketStatus::AwaitingReview)?;
    let summary = RunSummary {
        ticket_key: ticket.key.clone(),
        branch,
        commit,
        pull_request_url: pr.html_url,
    };
    print_summary(&summary, &gate_summary);
    Ok(summary)
}

struct ImplementationContext<'a> {
    app: &'a AppContext,
    api: &'a RustGridClient,
    run: &'a AgentRun,
    ticket: &'a Ticket,
    reporter: &'a Reporter<'a>,
    repo: &'a Repo,
    baseline: &'a BTreeSet<String>,
    prompt: &'a str,
    running: &'a AtomicBool,
    manifest: &'a ExecutionManifest,
}

fn implement_and_commit(implementation: ImplementationContext<'_>) -> Result<String> {
    let ImplementationContext {
        app: context,
        api,
        run,
        ticket,
        reporter,
        repo,
        baseline,
        prompt: generated_prompt,
        running,
        manifest,
    } = implementation;
    let policy = manifest.policy()?;
    reporter.step(
        "prompt_built",
        StepStatus::Completed,
        "Built Codex prompt from ticket and repository context",
        Some(json!({"characters": generated_prompt.len()})),
    )?;
    reporter.set_phase(RunPhase::Executing);
    reporter.step("codex", StepStatus::Running, "Running Codex locally", None)?;
    let blocked_action = RefCell::new(None);
    let codex_args = policy.codex_args();
    let codex_status = command::streaming_args(
        command::StreamingCommand {
            args: &codex_args,
            cwd: &repo.root,
            stdin_text: Some(generated_prompt),
            running,
            timeout: Duration::from_secs(policy.timeout_seconds),
            max_output_bytes: context.config.max_command_output_bytes as usize,
            environment_allowlist: Some(&policy.codex.environment_allowlist),
            limits: Some(child_limits(
                context,
                Duration::from_secs(policy.timeout_seconds),
            )),
        },
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
        return Err(RunFailure::HumanIntervention { action }.into());
    }
    if !codex_status.success() {
        bail!("Codex exited with {codex_status}");
    }
    reporter.step(
        "codex",
        StepStatus::Completed,
        "Codex finished successfully",
        None,
    )?;

    reporter.set_phase(RunPhase::Verifying);
    for gate_policy in &policy.quality_gates {
        reporter.step(
            &gate_policy.id,
            StepStatus::Running,
            &format!("Running quality gate: {}", gate_policy.command),
            Some(json!({"gate_id": gate_policy.id, "required": gate_policy.required})),
        )?;
        let gate = run_captured(
            context,
            &gate_policy.command,
            &repo.root,
            running,
            Duration::from_secs(gate_policy.timeout_seconds),
            context.config.max_command_output_bytes as usize,
            &policy.codex.environment_allowlist,
        )?;
        print_output(&gate.stdout, &gate.stderr);
        let gate_output = combine_output(&gate.stdout, &gate.stderr);
        reporter.log(&gate_output)?;
        let passed = gate.status.success();
        api.report_quality_gate(
            &ticket.id,
            &run.id,
            &gate_policy.id,
            &gate_policy.command,
            passed,
            &gate_output,
        )?;
        reporter.step(
            &gate_policy.id,
            if passed {
                StepStatus::Completed
            } else {
                StepStatus::Failed
            },
            if passed {
                "Quality gate passed"
            } else {
                "Quality gate failed"
            },
            Some(json!({"gate_id": gate_policy.id, "exit_code": gate.status.code()})),
        )?;
        if gate_policy.required && !passed {
            bail!(
                "required quality gate {} failed with {}",
                gate_policy.id,
                gate.status
            );
        }
    }

    reporter.set_phase(RunPhase::Publishing);
    let paths = repo.new_agent_paths(baseline)?;
    if paths.is_empty() {
        bail!("Codex produced no committable changes");
    }
    reporter.step(
        "changes_detected",
        StepStatus::Completed,
        &format!("Found {} agent-created changed path(s)", paths.len()),
        Some(json!({"paths": paths})),
    )?;
    reporter.step(
        "commit",
        StepStatus::Running,
        "Committing agent changes",
        None,
    )?;
    let commit = repo.commit_paths(&paths, &format!("{}: {}", ticket.key, ticket.title))?;
    reporter.record_commit(&commit)?;
    reporter.step(
        "commit",
        StepStatus::Completed,
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
    println!(
        "[ watching] Worker {} is streaming RustGrid queue events with capacity {}",
        worker.id, context.config.max_concurrency
    );
    let running = Arc::new(AtomicBool::new(true));
    let mut coordinator = CoordinatorHealth::starting();
    coordinator.record_success();
    let signal = Arc::clone(&running);
    ctrlc::set_handler(move || {
        shutdown::request();
        signal.store(false, Ordering::SeqCst);
    })
    .context("could not install Ctrl-C handler")?;

    let mut tasks: Vec<thread::JoinHandle<()>> = Vec::new();
    for run in api
        .active_runs(&project_id, &worker.id)?
        .into_iter()
        .take(context.config.max_concurrency)
    {
        let ticket = match api.fetch_ticket(&run.ticket_id) {
            Ok(ticket) => ticket,
            Err(error) => {
                eprintln!(
                    "[warning] could not recover run {} because ticket {} was unavailable: {error:#}",
                    run.id, run.ticket_id
                );
                continue;
            }
        };
        println!(
            "[ recovery] Resuming assigned run {} for {}",
            run.id, ticket.key
        );
        let task_context = context.clone();
        let task_api = api.clone();
        let task_worker = worker.clone();
        tasks.push(thread::spawn(move || {
            if let Err(error) = execute_claimed(
                &task_context,
                &task_api,
                &task_worker,
                &run,
                &ticket,
                Arc::new(AtomicBool::new(true)),
            ) {
                eprintln!("[error] recovered ticket {} failed: {error:#}", ticket.key);
            }
        }));
    }
    let mut queue_sequence = api.queue_events(&worker.id, 0)?.next_sequence;
    while running.load(Ordering::SeqCst) && !shutdown::requested() {
        let mut index = 0;
        while index < tasks.len() {
            if tasks[index].is_finished() {
                let task = tasks.swap_remove(index);
                if task.join().is_err() {
                    eprintln!("[error] worker execution thread panicked");
                }
            } else {
                index += 1;
            }
        }
        if shutdown::drain_requested() && tasks.is_empty() {
            console_event("drained", "All active runs finished", "33");
            break;
        }
        if shutdown::drain_requested() {
            coordinator.start_draining();
        }
        let worker_status = if tasks.is_empty() {
            WorkerStatus::Online
        } else {
            WorkerStatus::Busy
        };
        if let Err(error) = api.heartbeat_with_status(&worker.id, worker_status) {
            let delay = coordinator.record_transient_failure();
            eprintln!(
                "[warning] coordinator heartbeat failed; pausing claims for {}ms: {error:#}",
                delay.as_millis()
            );
            thread::sleep(delay.min(interval));
            continue;
        }
        coordinator.record_success();
        let available_slots = if shutdown::drain_requested() {
            0
        } else {
            context.config.max_concurrency.saturating_sub(tasks.len())
        };
        let mut claimed = 0usize;
        for _ in 0..available_slots {
            match api.claim_next(&worker.id, &project_id) {
                Err(error) => {
                    let delay = coordinator.record_transient_failure();
                    eprintln!(
                        "[warning] queue claim failed; coordinator is degraded for {}ms: {error:#}",
                        delay.as_millis()
                    );
                    thread::sleep(delay.min(interval));
                    break;
                }
                Ok(Some(run)) => {
                    let ticket = match api.fetch_ticket(&run.ticket_id) {
                        Ok(ticket) => ticket,
                        Err(error) => {
                            eprintln!(
                                "[warning] claimed run {} but ticket {} could not be fetched; the lease will expire for safe reconciliation: {error:#}",
                                run.id, run.ticket_id
                            );
                            continue;
                        }
                    };
                    console_event("claimed", &format!("Queue returned {}", ticket.key), "32");
                    let task_context = context.clone();
                    let task_api = api.clone();
                    let task_worker = worker.clone();
                    let task_running = Arc::new(AtomicBool::new(true));
                    tasks.push(thread::spawn(move || {
                        if let Err(error) = execute_claimed(
                            &task_context,
                            &task_api,
                            &task_worker,
                            &run,
                            &ticket,
                            task_running,
                        ) {
                            eprintln!("[error] ticket {} failed: {error:#}", ticket.key);
                        }
                    }));
                    claimed += 1;
                }
                Ok(None) => break,
            }
        }
        if once {
            break;
        }
        if shutdown::drain_requested() {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        if claimed == 0 {
            match api.wait_for_queue_event(&worker.id, queue_sequence, interval) {
                Ok(Some(sequence)) => queue_sequence = sequence,
                Ok(None) => {}
                Err(error) => {
                    eprintln!(
                        "[warning] queue stream unavailable; falling back to poll: {error:#}"
                    );
                    thread::sleep(interval.min(Duration::from_secs(5)));
                }
            }
            match api.queue_events(&worker.id, queue_sequence) {
                Ok(events) => queue_sequence = events.next_sequence,
                Err(error) => {
                    eprintln!(
                        "[warning] queue replay failed; restarting retained replay: {error:#}"
                    );
                    queue_sequence = api.queue_events(&worker.id, 0)?.next_sequence;
                }
            }
        }
    }
    for task in tasks {
        let _ = task.join();
    }
    coordinator.stop();
    console_event("stopped", "Watcher stopped", "33");
    Ok(())
}

pub fn serve(context: &AppContext, interval: Duration) -> Result<()> {
    if std::env::var("RUSTGRID_AGENT_ISOLATION").as_deref() != Ok("per_run") {
        bail!(
            "serve requires RUSTGRID_AGENT_ISOLATION=per_run after the deployment runtime gives each run an isolated filesystem and resource boundary"
        );
    }
    if context.config.max_concurrency != 1 {
        bail!(
            "serve requires max_concurrency=1 until each run is launched in a separately enforced container or equivalent runtime boundary"
        );
    }
    watch(context, interval, false)
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

pub fn status(context: &AppContext, json_output: bool) -> Result<()> {
    crate::health::status(context, json_output)
}

fn run_captured(
    context: &AppContext,
    command_text: &str,
    cwd: &Path,
    running: &AtomicBool,
    timeout: Duration,
    max_output_bytes: usize,
    environment_allowlist: &[String],
) -> Result<command::CommandOutput> {
    command::capture_cancellable_with_environment(
        command_text,
        cwd,
        running,
        timeout,
        max_output_bytes,
        Some(environment_allowlist),
        Some(child_limits(context, timeout)),
    )
}

fn child_limits(context: &AppContext, timeout: Duration) -> command::ChildLimits {
    command::ChildLimits {
        address_space_bytes: context.config.max_child_memory_bytes,
        file_bytes: context.config.max_child_file_bytes,
        open_files: context.config.max_child_open_files,
        cpu_seconds: timeout.as_secs().saturating_add(1),
    }
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

fn short_sha(sha: &str) -> &str {
    sha.get(..sha.len().min(12)).unwrap_or(sha)
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
