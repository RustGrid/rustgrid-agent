use std::{
    cell::RefCell,
    collections::{BTreeSet, HashSet},
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
    config::AppContext,
    coordinator::CoordinatorHealth,
    execution::{ImplementationContext, implement_and_commit, short_sha},
    executor::{ExecutionHandle, Executor},
    finalization::finalize,
    git::{Repo, branch_name},
    github::GitHubClient,
    journal::{RecoveryPlan, RunJournal},
    lifecycle::{RunPhase, StepStatus, TicketStatus, WorkerStatus},
    manifest::ExecutionManifest,
    outcome::{RunOutcome, RunSummary},
    prompt,
    publishing::{
        WorkflowRequirements, print_summary, pull_request_body, wait_for_required_workflows,
    },
    reporting::{Reporter, console_event},
    run_error::{RunErrorKind, classify},
    shutdown,
    supervisor::{RunSupervisor, RunSupervisorConfig},
    token::GitHubTokenManager,
    workspace::RunWorkspace,
};

pub fn register(context: &AppContext) -> Result<()> {
    console_event("starting", "Connecting to announced RustGrid worker", "36");
    let api = RustGridClient::new(context)?;
    let worker = connect_worker(context, &api)?;
    println!(
        "[ complete] Worker {} is connected and healthy ({})",
        worker.id,
        worker.status.as_str()
    );
    Ok(())
}

pub fn run_ticket(context: &AppContext, ticket_id: &str) -> Result<RunSummary> {
    sweep_workspaces(context, &HashSet::new())?;
    let api = RustGridClient::new(context)?;
    console_event("starting", "Connecting to announced worker", "36");
    let worker = connect_worker(context, &api)?;
    run_ticket_with_worker(context, &api, &worker, ticket_id)
}

fn connect_worker(context: &AppContext, api: &RustGridClient) -> Result<Worker> {
    let worker_id = context.require_worker_id()?;
    api.heartbeat(worker_id).with_context(|| {
        format!(
            "could not connect to announced worker {worker_id}; verify RUSTGRID_WORKER_ID and that RUSTGRID_WORKER_API_KEY is bound to it"
        )
    })?;
    Ok(Worker {
        id: worker_id.to_owned(),
        status: crate::api::RemoteWorkerStatus::Online,
        max_concurrency: context.config.max_concurrency,
        active_runs: 0,
        available_slots: context.config.max_concurrency,
    })
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
    let mut journal = RunJournal::create(&journal_path, &run.id, &ticket.id)?;
    journal.resume_active_run()?;
    let reporter = Reporter::new(
        api,
        &run.id,
        Arc::clone(&row_version),
        &ticket.id,
        ticket.row_version,
        journal,
    );
    let manifest_and_policy = (|| {
        let manifest = api
            .execution_manifest(&run.id)
            .with_context(|| format!("could not retrieve execution manifest for run {}", run.id))?;
        manifest.validate(&run.id, &ticket.id)?;
        let policy = manifest.policy()?;
        Ok::<_, anyhow::Error>((manifest, policy))
    })();
    let (manifest, execution_policy) = match manifest_and_policy {
        Ok(value) => value,
        Err(error) => {
            match classify(&error) {
                RunErrorKind::LeaseLost => {
                    return Err(error.context("skipped stale terminal updates"));
                }
                RunErrorKind::Transient | RunErrorKind::Invariant => {
                    reporter.fail_retryable(&error)?;
                }
                _ => reporter.fail(&error)?,
            }
            return Err(error);
        }
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
            run_timeout: Duration::from_secs(execution_policy.timeout_seconds),
        },
    );
    let workspace = RefCell::new(None::<RunWorkspace>);
    let executor = Executor::from_config(&context.config.executor);
    let executor_handle = RefCell::new(None::<ExecutionHandle>);

    let result = (|| {
        if manifest.ticket_key != ticket.key {
            bail!("execution manifest does not match the fetched ticket");
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
        let handle = executor.prepare(
            &run.id,
            &repo.root,
            execution_policy.requires_npm_registry(),
        )?;
        if let Some(id) = handle.id() {
            reporter.record_executor(context.config.executor.kind(), id, "created")?;
            reporter.step(
                "sandbox_created",
                StepStatus::Completed,
                &format!("Created Docker Sandbox {id}"),
                Some(json!({"executor": context.config.executor.kind(), "sandbox_id": id})),
            )?;
        }
        executor_handle.replace(Some(handle.clone()));
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
            executor: &executor,
            executor_handle: &handle,
        })
    })();
    let mut result = result.and_then(|summary| {
        if let Some(workspace) = workspace.borrow().as_ref() {
            workspace.enforce_size_limit(context.config.max_workspace_bytes)?;
        }
        Ok(summary)
    });
    if let Some(handle) = executor_handle.borrow().as_ref() {
        if let Err(error) = executor.destroy(
            handle,
            workspace
                .borrow()
                .as_ref()
                .map_or(&context.workspace_root, |item| &item.repo.root),
        ) {
            if result.is_ok() {
                result = Err(error.context("run succeeded but sandbox cleanup failed"));
            }
        } else if let Some(id) = handle.id() {
            let _ = reporter.record_executor(context.config.executor.kind(), id, "destroyed");
        }
    }

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
    finalize(outcome, &reporter, &workspace, supervisor_healthy)
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
    executor: &'a Executor,
    executor_handle: &'a ExecutionHandle,
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
        executor,
        executor_handle,
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
            executor,
            executor_handle,
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

    let required_workflows = manifest.normalized_required_workflows()?;
    if !required_workflows.is_empty() {
        wait_for_required_workflows(
            &tokens,
            WorkflowRequirements {
                repo: &manifest.repo_config()?,
                web_base_url: &manifest.web_base_url,
                commit: &commit,
                required: &required_workflows,
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

pub fn watch(context: &AppContext, interval: Duration, once: bool) -> Result<()> {
    if once && context.config.max_concurrency != 1 {
        bail!(
            "watch --once requires max_concurrency=1 so one runtime boundary owns at most one run"
        );
    }
    let api = RustGridClient::new(context)?;
    let worker = connect_worker(context, &api)?;
    println!(
        "[ watching] Tenant worker {} is streaming RustGrid queue events with capacity {}",
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

    let active_runs = api.active_runs(&worker.id)?;
    let protected_run_ids = active_runs
        .iter()
        .map(|run| run.id.clone())
        .collect::<HashSet<_>>();
    let removed_orphans = Executor::from_config(&context.config.executor)
        .reconcile_orphans(&protected_run_ids, &context.workspace_root)?;
    if removed_orphans > 0 {
        console_event(
            "cleanup",
            &format!("Removed {removed_orphans} orphan Docker Sandbox(es)"),
            "33",
        );
    }
    sweep_workspaces(context, &protected_run_ids)?;
    let mut tasks: Vec<(String, thread::JoinHandle<()>)> = Vec::new();
    let mut started_run_ids = protected_run_ids.clone();
    for run in active_runs.into_iter().take(context.config.max_concurrency) {
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
        let run_id = run.id.clone();
        tasks.push((
            run_id,
            thread::spawn(move || {
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
            }),
        ));
    }
    let mut queue_sequence = api.queue_events(&worker.id, 0)?.next_sequence;
    while running.load(Ordering::SeqCst) && !shutdown::requested() {
        let mut index = 0;
        while index < tasks.len() {
            if tasks[index].1.is_finished() {
                let (_, task) = tasks.swap_remove(index);
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
        let mut assigned = 0usize;
        let assigned_runs = api.active_runs(&worker.id)?;
        for run in select_unstarted_assignments(assigned_runs, &started_run_ids, available_slots) {
            let ticket = match api.fetch_ticket(&run.ticket_id) {
                Ok(ticket) => ticket,
                Err(error) => {
                    eprintln!(
                        "[warning] assigned run {} could not start because ticket {} was unavailable: {error:#}",
                        run.id, run.ticket_id
                    );
                    continue;
                }
            };
            console_event(
                "assigned",
                &format!("RustGrid assigned {}", ticket.key),
                "32",
            );
            let run_id = run.id.clone();
            started_run_ids.insert(run_id.clone());
            let task_context = context.clone();
            let task_api = api.clone();
            let task_worker = worker.clone();
            let task_running = Arc::new(AtomicBool::new(true));
            tasks.push((
                run_id,
                thread::spawn(move || {
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
                }),
            ));
            assigned += 1;
        }
        if once {
            break;
        }
        if shutdown::drain_requested() {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        if assigned == 0 {
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
    for (_, task) in tasks {
        let _ = task.join();
    }
    coordinator.stop();
    console_event("stopped", "Watcher stopped", "33");
    Ok(())
}

fn select_unstarted_assignments(
    assigned_runs: Vec<AgentRun>,
    started_run_ids: &HashSet<String>,
    available_slots: usize,
) -> Vec<AgentRun> {
    assigned_runs
        .into_iter()
        .filter(|run| !started_run_ids.contains(&run.id))
        .take(available_slots)
        .collect()
}

pub fn serve(context: &AppContext, interval: Duration) -> Result<()> {
    context
        .config
        .executor
        .validate_production(context.config.max_concurrency)?;
    Executor::from_config(&context.config.executor).preflight(&context.workspace_root)?;
    watch(context, interval, false)
}

fn sweep_workspaces(context: &AppContext, protected_run_ids: &HashSet<String>) -> Result<()> {
    let removed = RunWorkspace::sweep_stale(
        &context.workspace_root,
        Duration::from_secs(
            context
                .config
                .failed_workspace_retention_hours
                .saturating_mul(3600),
        ),
        protected_run_ids,
    )?;
    if removed > 0 {
        println!("[  cleanup] Removed {removed} expired run workspace(s)");
    }
    Ok(())
}

pub fn status(context: &AppContext, json_output: bool) -> Result<()> {
    crate::health::status(context, json_output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str) -> AgentRun {
        AgentRun {
            id: id.into(),
            ticket_id: format!("ticket-{id}"),
            row_version: 0,
        }
    }

    #[test]
    fn assigned_run_selection_never_restarts_a_seen_run_and_respects_capacity() {
        let started = HashSet::from(["run-1".to_owned()]);

        let selected = select_unstarted_assignments(
            vec![run("run-1"), run("run-2"), run("run-3")],
            &started,
            1,
        );

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "run-2");
    }
}
