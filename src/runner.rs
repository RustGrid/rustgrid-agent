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
    execution::{
        CodexContext, ImplementationContext, QualityGateContext, implement_and_commit,
        run_codex_prompt, run_gates_with_repairs, short_sha,
    },
    executor::{ExecutionHandle, Executor},
    finalization::finalize,
    git::{
        ReconciledCommit, ReconciliationKind, RemoteBranchMoved, Repo, branch_name,
        fresh_branch_name,
    },
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
    run_error::{RunErrorKind, RunFailure, classify},
    shutdown,
    supervisor::{RunSupervisor, RunSupervisorConfig},
    token::GitHubTokenManager,
    workspace::RunWorkspace,
};

const CI_REPAIR_ATTEMPTS: u32 = 3;

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
    let manifest_and_policy = (|| {
        let manifest = api
            .execution_manifest(&run.id)
            .with_context(|| format!("could not retrieve execution manifest for run {}", run.id))?;
        manifest.validate(&run.id, &ticket.id)?;
        let policy = manifest.policy()?;
        let fresh_start = manifest.fresh_start()?;
        let recovery_source = manifest.resume_from_run_id()?.map(str::to_owned);
        Ok::<_, anyhow::Error>((manifest, policy, fresh_start, recovery_source))
    })();
    let (manifest, execution_policy, fresh_start, recovery_source) = match manifest_and_policy {
        Ok(value) => value,
        Err(error) => {
            report_preparation_failure(
                api,
                run,
                ticket,
                Arc::clone(&row_version),
                &journal_path,
                &error,
            )?;
            return Err(error);
        }
    };
    let recovery = match recovery_source.as_deref() {
        Some(source_run_id) => RunWorkspace::adopt_recovery(
            &context.workspace_root,
            &run.id,
            &ticket.id,
            source_run_id,
        )
        .map(|recovery| (recovery.journal, recovery.workspace_id)),
        None => RunJournal::create(&journal_path, &run.id, &ticket.id)
            .map(|journal| (journal, run.id.clone())),
    };
    let (mut journal, workspace_id) = match recovery {
        Ok(recovery) => recovery,
        Err(error) => {
            report_preparation_failure(
                api,
                run,
                ticket,
                Arc::clone(&row_version),
                &journal_path,
                &error,
            )?;
            return Err(error.context("could not adopt requested recovery work"));
        }
    };
    let retained_executor_id = journal.recoverable_executor_id().map(str::to_owned);
    journal.resume_active_run()?;
    let reporter = Reporter::new(
        api,
        &run.id,
        Arc::clone(&row_version),
        &ticket.id,
        ticket.row_version,
        journal,
    );

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
        let prepared = RunWorkspace::prepare(
            &context.workspace_root,
            &workspace_id,
            &manifest,
            &clone_token,
        )?;
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
            retained_executor_id.as_deref(),
        )?;
        // Adopt the executor immediately after it is prepared. Everything below this
        // point may fail (including local journaling and remote step reporting), and
        // the outcome handler must still be able to retain the sandbox for recovery.
        executor_handle.replace(Some(handle.clone()));
        if let Some(id) = handle.id() {
            reporter.record_executor(context.config.executor.kind(), id, "created")?;
            reporter.step(
                "sandbox_created",
                StepStatus::Completed,
                &format!("Created Docker Sandbox {id}"),
                Some(json!({"executor": context.config.executor.kind(), "sandbox_id": id})),
            )?;
        }
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
            if fresh_start {
                "Prepared a fresh isolated repository workspace with no recovery context"
            } else if recovery_source.is_some() {
                "Adopted retained repository workspace from the previous attempt"
            } else {
                "Prepared isolated repository workspace"
            },
            Some(json!({
                "bytes": workspace_bytes,
                "resumed": workspace_resumed,
                "fresh_start": fresh_start,
                "recovery_source_run_id": recovery_source
            })),
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
    if let Some(handle) = executor_handle.borrow().as_ref() {
        let cwd = workspace.borrow().as_ref().map_or_else(
            || context.workspace_root.clone(),
            |item| item.repo.root.clone(),
        );
        if outcome.should_retain_sandbox() {
            match executor.retain(handle, &cwd) {
                Ok(()) => {
                    if let Some(id) = handle.id() {
                        let _ = reporter.record_executor(
                            context.config.executor.kind(),
                            id,
                            "retained",
                        );
                    }
                }
                Err(error) => {
                    eprintln!("[warning] could not stop retained sandbox: {error:#}");
                    if let Some(id) = handle.id() {
                        let _ = reporter.record_executor(
                            context.config.executor.kind(),
                            id,
                            "retain_failed",
                        );
                    }
                }
            }
        }
    }
    if outcome.should_retain_sandbox() {
        return finalize(outcome, &reporter, supervisor_healthy);
    }

    let summary = match finalize(outcome, &reporter, supervisor_healthy) {
        Ok(summary) => summary,
        Err(error) => {
            if let Some(handle) = executor_handle.borrow().as_ref() {
                let cwd = workspace.borrow().as_ref().map_or_else(
                    || context.workspace_root.clone(),
                    |item| item.repo.root.clone(),
                );
                let _ = executor.retain(handle, &cwd);
                if let Some(id) = handle.id() {
                    let _ =
                        reporter.record_executor(context.config.executor.kind(), id, "retained");
                }
            }
            return Err(error);
        }
    };
    if let Some(handle) = executor_handle.borrow().as_ref() {
        let cwd = workspace.borrow().as_ref().map_or_else(
            || context.workspace_root.clone(),
            |item| item.repo.root.clone(),
        );
        if let Err(error) = executor.destroy(handle, &cwd) {
            let cleanup_error = error.context("run completed but sandbox cleanup failed");
            let _ = executor.retain(handle, &cwd);
            if let Some(id) = handle.id() {
                let _ = reporter.record_executor(context.config.executor.kind(), id, "retained");
            }
            return Err(cleanup_error);
        }
        if let Some(id) = handle.id() {
            let _ = reporter.record_executor(context.config.executor.kind(), id, "destroyed");
        }
    }
    if let Some(workspace) = workspace.borrow_mut().take() {
        workspace.cleanup()?;
    }
    Ok(summary)
}

fn report_preparation_failure(
    api: &RustGridClient,
    run: &AgentRun,
    ticket: &Ticket,
    row_version: Arc<AtomicI64>,
    journal_path: &std::path::Path,
    error: &anyhow::Error,
) -> Result<()> {
    if classify(error) == RunErrorKind::LeaseLost {
        return Err(anyhow::anyhow!("{error:#}").context("skipped stale terminal updates"));
    }
    let journal = RunJournal::create(journal_path, &run.id, &ticket.id)?;
    let reporter = Reporter::new(
        api,
        &run.id,
        row_version,
        &ticket.id,
        ticket.row_version,
        journal,
    );
    match classify(error) {
        RunErrorKind::Transient | RunErrorKind::Invariant => reporter.fail_retryable(error),
        _ => reporter.fail(error),
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
    executor: &'a Executor,
    executor_handle: &'a ExecutionHandle,
}

struct PublicationContext<'a> {
    app: &'a AppContext,
    api: &'a RustGridClient,
    run: &'a AgentRun,
    ticket: &'a Ticket,
    reporter: &'a Reporter<'a>,
    repo: &'a Repo,
    baseline: &'a BTreeSet<String>,
    base_branch: &'a str,
    running: &'a AtomicBool,
    manifest: &'a ExecutionManifest,
    executor: &'a Executor,
    executor_handle: &'a ExecutionHandle,
}

fn validate_reconciled_commit(
    context: &PublicationContext<'_>,
    step_id: &str,
    branch: &str,
    commit: &mut String,
    reconciled: ReconciledCommit,
    message: &str,
) -> Result<()> {
    if !reconciled.requires_validation() {
        return Ok(());
    }
    *commit = reconciled.commit;
    context.reporter.record_commit(commit)?;
    context.reporter.step(
        step_id,
        StepStatus::Completed,
        message,
        Some(json!({
            "commit": commit,
            "strategy": match reconciled.kind {
                ReconciliationKind::RemoteAdvanced => "remote_advanced",
                ReconciliationKind::Rebased => "rebase",
                ReconciliationKind::Unchanged => "unchanged",
            }
        })),
    )?;
    let codex = CodexContext {
        app: context.app,
        reporter: context.reporter,
        repo: context.repo,
        running: context.running,
        manifest: context.manifest,
        executor: context.executor,
        executor_handle: context.executor_handle,
    };
    run_gates_with_repairs(
        &codex,
        QualityGateContext {
            app: context.app,
            api: context.api,
            run: context.run,
            ticket: context.ticket,
            reporter: context.reporter,
            repo: context.repo,
            running: context.running,
            manifest: context.manifest,
            executor: context.executor,
            executor_handle: context.executor_handle,
        },
        "post-reconciliation quality gates",
    )?;
    let repair_paths = context.repo.new_agent_paths(context.baseline)?;
    if !repair_paths.is_empty() {
        *commit = context.repo.commit_paths(
            &repair_paths,
            &format!(
                "{}: {} (post-reconciliation repair)",
                context.ticket.key, context.ticket.title
            ),
        )?;
        context.reporter.record_commit(commit)?;
        context.reporter.step(
            &format!("{step_id}_repair"),
            StepStatus::Completed,
            &format!(
                "Committed post-reconciliation repair {} on {branch}",
                short_sha(commit)
            ),
            Some(json!({"commit": commit, "paths": repair_paths})),
        )?;
    }
    Ok(())
}

fn publish_commit(
    context: PublicationContext<'_>,
    tokens: &GitHubTokenManager<'_>,
    branch: &str,
    commit: &mut String,
    cycle: u32,
) -> Result<bool> {
    let step_id = if cycle == 0 {
        "push".to_owned()
    } else {
        format!("push_ci_repair_{cycle}")
    };
    if context.reporter.phase() != RunPhase::Publishing {
        context.reporter.set_phase(RunPhase::Publishing);
    }
    context.reporter.step(
        &step_id,
        StepStatus::Running,
        &format!("Pushing branch {branch}"),
        Some(json!({"cycle": cycle})),
    )?;
    let mut pushed = false;
    for publish_attempt in 1..=3u32 {
        let reconcile_token = tokens.token()?;
        let reconciled = context.repo.reconcile_remote_branch(
            branch,
            commit,
            &reconcile_token,
            &context.manifest.web_base_url,
        )?;
        let reconciliation = match reconciled.kind {
            ReconciliationKind::RemoteAdvanced => "accepted remote commits after the agent commit",
            ReconciliationKind::Rebased => "rebased the agent commit onto concurrent remote work",
            ReconciliationKind::Unchanged => "remote branch was unchanged",
        };
        validate_reconciled_commit(
            &context,
            &format!("{step_id}_reconciliation"),
            branch,
            commit,
            reconciled,
            &format!("Reconciled branch {branch}: {reconciliation}"),
        )?;

        let base_token = tokens.token()?;
        let base_lease =
            context
                .repo
                .remote_branch_head(branch, &base_token, &context.manifest.web_base_url)?;
        let base_reconciled = context.repo.rebase_onto_remote_base(
            branch,
            context.base_branch,
            commit,
            &base_token,
            &context.manifest.web_base_url,
        )?;
        let base_changed = base_reconciled.requires_validation();
        validate_reconciled_commit(
            &context,
            &format!("{step_id}_base_reconciliation"),
            branch,
            commit,
            base_reconciled,
            &format!(
                "Rebased all agent commits onto latest remote base {}",
                context.base_branch
            ),
        )?;

        let push_token = tokens.token()?;
        let expected_remote = if base_changed {
            base_lease.as_deref()
        } else {
            None
        };
        match context.repo.push_with_lease(
            branch,
            commit,
            expected_remote,
            &push_token,
            &context.manifest.web_base_url,
        ) {
            Ok(did_push) => {
                pushed |= did_push;
                break;
            }
            Err(error)
                if !base_changed
                    && publish_attempt < 3
                    && error.downcast_ref::<RemoteBranchMoved>().is_some() =>
            {
                context.reporter.step(
                    &format!("{step_id}_race_retry"),
                    StepStatus::Running,
                    &format!(
                        "Remote branch changed during publication; reconciling attempt {} of 3",
                        publish_attempt + 1
                    ),
                    Some(json!({"attempt": publish_attempt + 1})),
                )?;
                eprintln!("[warning] git push race on attempt {publish_attempt}: {error:#}");
            }
            Err(error) => return Err(error),
        }
    }
    context.reporter.step(
        &step_id,
        StepStatus::Completed,
        &format!("Pushed branch {branch}"),
        Some(json!({"pushed": pushed, "commit": commit})),
    )?;
    Ok(pushed)
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

    let branch = if manifest.fresh_start()? {
        fresh_branch_name(&ticket.key, &ticket.title, &run.id)
    } else {
        branch_name(&ticket.key, &ticket.title)
    };
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
    let mut commit = if let Some(commit) = recovered_commit {
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

    let tokens = GitHubTokenManager::new(
        api,
        &run.id,
        &manifest.repository,
        &manifest.required_permissions,
    );
    publish_commit(
        PublicationContext {
            app: context,
            api,
            run,
            ticket,
            reporter,
            repo: &repo,
            baseline: &baseline,
            base_branch,
            running,
            manifest,
            executor,
            executor_handle,
        },
        &tokens,
        &branch,
        &mut commit,
        0,
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
        let repo_config = manifest.repo_config()?;
        let mut repair_attempt = 0u32;
        loop {
            let failure = match wait_for_required_workflows(
                &tokens,
                WorkflowRequirements {
                    repo: &repo_config,
                    web_base_url: &manifest.web_base_url,
                    commit: &commit,
                    required: &required_workflows,
                    timeout: Duration::from_secs(policy.timeout_seconds),
                },
                running,
                reporter,
            ) {
                Ok(()) => break,
                Err(error) => match error.downcast_ref::<RunFailure>() {
                    Some(RunFailure::RequiredWorkflowFailed {
                        diagnostics,
                        repairable: true,
                    }) => diagnostics.clone(),
                    _ => return Err(error),
                },
            };
            if repair_attempt == CI_REPAIR_ATTEMPTS {
                return Err(RunFailure::ValidationRepairsExhausted {
                    attempts: CI_REPAIR_ATTEMPTS,
                    diagnostics: failure,
                }
                .into());
            }
            repair_attempt += 1;
            reporter.step(
                &format!("ci_repair_{repair_attempt}"),
                StepStatus::Running,
                &format!(
                    "Required CI failed; asking Codex to repair attempt {repair_attempt} of {CI_REPAIR_ATTEMPTS}"
                ),
                Some(json!({"attempt": repair_attempt, "commit": commit})),
            )?;
            let codex = CodexContext {
                app: context,
                reporter,
                repo: &repo,
                running,
                manifest,
                executor,
                executor_handle,
            };
            let repair_prompt = format!(
                "Required GitHub CI failed for the pull request. Inspect the current workspace and the CI evidence below, fix the underlying issue, and run relevant checks. Do not commit, push, or open a pull request; the runner owns publication. CI failures are real blockers, so do not declare success while they remain.\n\n{failure}"
            );
            run_codex_prompt(
                &codex,
                &repair_prompt,
                &format!("ci_repair_{repair_attempt}_codex"),
                "Running Codex CI repair",
            )?;
            run_gates_with_repairs(
                &codex,
                QualityGateContext {
                    app: context,
                    api,
                    run,
                    ticket,
                    reporter,
                    repo: &repo,
                    running,
                    manifest,
                    executor,
                    executor_handle,
                },
                "CI repair local quality gates",
            )?;
            let paths = repo.new_agent_paths(&baseline)?;
            if paths.is_empty() {
                reporter.step(
                    &format!("ci_repair_{repair_attempt}"),
                    StepStatus::Failed,
                    "Codex CI repair produced no committable changes; another repair attempt is required",
                    Some(json!({"attempt": repair_attempt})),
                )?;
                continue;
            }
            reporter.step(
                &format!("ci_repair_{repair_attempt}_commit"),
                StepStatus::Running,
                "Committing CI repair",
                Some(json!({"paths": paths})),
            )?;
            commit = repo.commit_paths(
                &paths,
                &format!(
                    "{}: {} (CI repair {repair_attempt})",
                    ticket.key, ticket.title
                ),
            )?;
            reporter.record_commit(&commit)?;
            reporter.step(
                &format!("ci_repair_{repair_attempt}_commit"),
                StepStatus::Completed,
                &format!("Created CI repair commit {}", short_sha(&commit)),
                Some(json!({"commit": commit})),
            )?;
            publish_commit(
                PublicationContext {
                    app: context,
                    api,
                    run,
                    ticket,
                    reporter,
                    repo: &repo,
                    baseline: &baseline,
                    base_branch,
                    running,
                    manifest,
                    executor,
                    executor_handle,
                },
                &tokens,
                &branch,
                &mut commit,
                repair_attempt,
            )?;
            reporter.step(
                &format!("ci_repair_{repair_attempt}"),
                StepStatus::Completed,
                "Published CI repair; waiting for required workflows on the new commit",
                Some(json!({"attempt": repair_attempt, "commit": commit})),
            )?;
        }
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
    let active_run_ids = active_runs
        .iter()
        .map(|run| run.id.clone())
        .collect::<HashSet<_>>();
    let retention = Duration::from_secs(
        context
            .config
            .failed_workspace_retention_hours
            .saturating_mul(3600),
    );
    let mut protected_sandbox_names = active_run_ids
        .iter()
        .map(|run_id| Executor::sandbox_name_for_run(run_id))
        .collect::<HashSet<_>>();
    protected_sandbox_names.extend(RunWorkspace::recoverable_sandbox_names(
        &context.workspace_root,
        retention,
    )?);
    let removed_orphans = Executor::from_config(&context.config.executor)
        .reconcile_orphans(&protected_sandbox_names, &context.workspace_root)?;
    if removed_orphans > 0 {
        console_event(
            "cleanup",
            &format!("Removed {removed_orphans} orphan Docker Sandbox(es)"),
            "33",
        );
    }
    sweep_workspaces(context, &active_run_ids)?;
    let mut tasks: Vec<(String, thread::JoinHandle<()>)> = Vec::new();
    let mut started_run_ids = active_run_ids;
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
