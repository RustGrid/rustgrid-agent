use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    api::{AgentRun, RustGridClient, Ticket},
    command,
    config::AppContext,
    executor::{ExecutionHandle, Executor, RunCommand},
    git::Repo,
    lifecycle::{RunPhase, StepStatus},
    manifest::ExecutionManifest,
    mission::{BudgetStage, BudgetUsage, MissionBudget, MissionClass, MissionProfile},
    optimization::{DependencyState, process_command_output},
    reporting::Reporter,
    run_error::RunFailure,
    telemetry::SessionOutcome,
};

const CODEX_IDLE_ATTEMPTS: u32 = 3;
const DEPENDENCY_INSTALL_ATTEMPTS: u32 = 3;
pub(crate) const VALIDATION_REPAIR_ATTEMPTS: u32 = 3;

pub(crate) struct ImplementationContext<'a> {
    pub app: &'a AppContext,
    pub api: &'a RustGridClient,
    pub run: &'a AgentRun,
    pub ticket: &'a Ticket,
    pub reporter: &'a Reporter<'a>,
    pub repo: &'a Repo,
    pub baseline: &'a BTreeSet<String>,
    pub prompt: &'a str,
    pub image_paths: &'a [PathBuf],
    pub running: &'a AtomicBool,
    pub manifest: &'a ExecutionManifest,
    pub executor: &'a Executor,
    pub executor_handle: &'a ExecutionHandle,
}

#[derive(Clone, Copy)]
pub(crate) struct QualityGateContext<'a> {
    pub app: &'a AppContext,
    pub api: &'a RustGridClient,
    pub run: &'a AgentRun,
    pub ticket: &'a Ticket,
    pub reporter: &'a Reporter<'a>,
    pub repo: &'a Repo,
    pub running: &'a AtomicBool,
    pub manifest: &'a ExecutionManifest,
    pub executor: &'a Executor,
    pub executor_handle: &'a ExecutionHandle,
}

#[derive(Clone, Copy)]
pub(crate) struct CodexContext<'a> {
    pub app: &'a AppContext,
    pub reporter: &'a Reporter<'a>,
    pub repo: &'a Repo,
    pub running: &'a AtomicBool,
    pub manifest: &'a ExecutionManifest,
    pub executor: &'a Executor,
    pub executor_handle: &'a ExecutionHandle,
    pub mission_class: MissionClass,
    pub budget: MissionBudget,
    pub baseline: &'a BTreeSet<String>,
    pub ticket: &'a Ticket,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FocusedValidationEvidence {
    command: String,
    source_tree_hash: String,
}

#[derive(Debug)]
pub(crate) struct QualityGateFailure {
    pub gate_id: String,
    pub command: String,
    pub status: String,
    pub output: String,
}

#[derive(Debug, Default)]
pub(crate) struct QualityGateOutcome {
    pub failures: Vec<QualityGateFailure>,
}

impl QualityGateOutcome {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    pub fn diagnostics(&self) -> String {
        self.failures
            .iter()
            .map(|failure| {
                format!(
                    "Gate {} (`{}`) failed with {}:\n{}",
                    failure.gate_id, failure.command, failure.status, failure.output
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

pub(crate) fn implement_and_commit(implementation: ImplementationContext<'_>) -> Result<String> {
    let ImplementationContext {
        app: context,
        api,
        run,
        ticket,
        reporter,
        repo,
        baseline,
        prompt: generated_prompt,
        image_paths,
        running,
        manifest,
        executor,
        executor_handle,
    } = implementation;
    let mission_profile = MissionProfile::classify_after_checkout(ticket, manifest, &repo.root);
    reporter.step(
        "prompt_built",
        StepStatus::Completed,
        "Built Codex prompt from ticket and repository context",
        Some(json!({"characters": generated_prompt.len()})),
    )?;
    let codex_context = CodexContext {
        app: context,
        reporter,
        repo,
        running,
        manifest,
        executor,
        executor_handle,
        mission_class: mission_profile.class,
        budget: mission_profile.budget,
        baseline,
        ticket,
    };
    run_codex_prompt(
        &codex_context,
        generated_prompt,
        image_paths,
        "codex",
        "Running Codex",
    )?;
    run_gates_with_repairs(
        &codex_context,
        QualityGateContext {
            app: context,
            api,
            run,
            ticket,
            reporter,
            repo,
            running,
            manifest,
            executor,
            executor_handle,
        },
        "local quality gates",
    )?;

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

pub(crate) fn bootstrap_dependencies(
    app: &AppContext,
    reporter: &Reporter<'_>,
    repo: &Repo,
    running: &AtomicBool,
    manifest: &ExecutionManifest,
    executor: &Executor,
    executor_handle: &ExecutionHandle,
) -> Result<()> {
    let Some((manager, command_text)) = dependency_bootstrap_command(&repo.root) else {
        return Ok(());
    };
    let current_state = DependencyState::inspect(&repo.root, manager, command_text)?;
    if reporter
        .dependency_state()
        .is_some_and(|installed| installed.reusable_against(&current_state))
    {
        reporter.step(
            "dependency_bootstrap",
            StepStatus::Completed,
            &format!("Reused previously installed locked {manager} dependencies"),
            Some(json!({
                "manager": manager,
                "command": command_text,
                "reused": true,
                "dependency_state": current_state
            })),
        )?;
        return Ok(());
    }
    let policy = manifest.policy()?;
    reporter.step(
        "dependency_bootstrap",
        StepStatus::Running,
        &format!("Installing locked {manager} dependencies before Codex execution"),
        Some(json!({"manager": manager, "command": command_text})),
    )?;
    let bootstrap_started = Instant::now();
    for attempt in 1..=DEPENDENCY_INSTALL_ATTEMPTS {
        let started = Instant::now();
        let install = executor.captured(
            executor_handle,
            command_text,
            RunCommand {
                args: &[],
                cwd: &repo.root,
                stdin_text: None,
                running,
                timeout: Duration::from_secs(policy.timeout_seconds.min(1_800)),
                idle_timeout: None,
                output_is_activity: None,
                max_output_bytes: app.config.max_command_output_bytes as usize,
                environment_allowlist: &policy.codex.environment_allowlist,
                limits: Some(child_limits(
                    app,
                    Duration::from_secs(policy.timeout_seconds.min(1_800)),
                )),
                max_workspace_bytes: app.config.max_workspace_bytes,
            },
        )?;
        let output = combine_output(&install.stdout, &install.stderr);
        if install.status.success() {
            let processed = process_command_output(
                &repo.root,
                command_text,
                &output,
                true,
                install.status.code(),
                started.elapsed(),
            )?;
            let completed_state =
                DependencyState::inspect(&repo.root, manager, command_text)?.completed();
            reporter.record_dependency_state(completed_state.clone())?;
            reporter.step(
                "dependency_bootstrap",
                StepStatus::Completed,
                &format!("Installed locked {manager} dependencies"),
                Some(json!({
                    "manager": manager,
                    "attempt": attempt,
                    "dependency_state": completed_state,
                    "output": processed.model_summary,
                    "raw_output_location": processed.raw_output_location,
                    "original_characters": processed.original_characters,
                    "model_characters": processed.model_characters,
                    "duration_ms": bootstrap_started.elapsed().as_millis()
                })),
            )?;
            return Ok(());
        }
        if !is_transient_gate_failure(&output) {
            reporter.step(
                "dependency_bootstrap",
                StepStatus::Failed,
                "Dependency bootstrap failed for a repository-specific reason; Codex will inspect and repair it",
                Some(json!({
                    "manager": manager,
                    "attempt": attempt,
                    "output": truncate_text(&output, 8_000)
                })),
            )?;
            return Ok(());
        }
        if attempt < DEPENDENCY_INSTALL_ATTEMPTS {
            let delay = Duration::from_secs(u64::from(attempt) * 2);
            reporter.step(
                "dependency_bootstrap_retry",
                StepStatus::Running,
                &format!(
                    "Dependency registry request failed transiently; retrying attempt {} of {} in {}s",
                    attempt + 1,
                    DEPENDENCY_INSTALL_ATTEMPTS,
                    delay.as_secs()
                ),
                Some(json!({"manager": manager, "attempt": attempt + 1})),
            )?;
            thread::sleep(delay);
            continue;
        }
        return Err(RunFailure::InfrastructureTransient {
            detail: format!(
                "locked {manager} dependency installation remained unavailable after {DEPENDENCY_INSTALL_ATTEMPTS} attempts: {}",
                truncate_text(&output, 8_000)
            ),
        }
        .into());
    }
    unreachable!("bounded dependency bootstrap loop always returns")
}

fn dependency_bootstrap_command(root: &std::path::Path) -> Option<(&'static str, &'static str)> {
    if !root.join("package.json").is_file() {
        return None;
    }
    if root.join("pnpm-lock.yaml").is_file() {
        Some((
            "pnpm",
            "pnpm install --frozen-lockfile --prefer-offline --ignore-scripts",
        ))
    } else if root.join("yarn.lock").is_file() {
        Some((
            "yarn",
            "yarn install --frozen-lockfile --prefer-offline --ignore-scripts",
        ))
    } else if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
        Some(("bun", "bun install --frozen-lockfile --ignore-scripts"))
    } else if root.join("package-lock.json").is_file() || root.join("npm-shrinkwrap.json").is_file()
    {
        Some((
            "npm",
            "npm ci --ignore-scripts --no-audit --no-fund --prefer-offline",
        ))
    } else {
        None
    }
}

#[derive(Clone, Debug)]
enum CodexControl {
    Budget(BudgetStage),
    WorkerOwnedCommand(String),
    Completion,
}

impl std::fmt::Display for CodexControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(stage) => write!(formatter, "Codex budget intervention: {stage:?}"),
            Self::WorkerOwnedCommand(command) => {
                write!(formatter, "Codex attempted worker-owned command: {command}")
            }
            Self::Completion => formatter.write_str("Codex completed with a committable diff"),
        }
    }
}

impl std::error::Error for CodexControl {}

pub(crate) fn run_codex_prompt(
    context: &CodexContext<'_>,
    prompt: &str,
    image_paths: &[PathBuf],
    step_id: &str,
    message: &str,
) -> Result<()> {
    let policy = context.manifest.policy()?;
    let externally_isolated = context.executor.externally_isolated();
    let codex_sandbox_mode = if externally_isolated {
        "danger-full-access"
    } else {
        "workspace-write"
    };
    context.reporter.set_phase(RunPhase::Executing);
    context.reporter.step(
        step_id,
        StepStatus::Running,
        message,
        Some(json!({
            "idle_timeout_seconds": policy.codex_idle_timeout().as_secs(),
            "max_idle_attempts": CODEX_IDLE_ATTEMPTS,
            "codex_sandbox_mode": codex_sandbox_mode,
            "external_isolation": externally_isolated
        })),
    )?;
    let blocked_action = RefCell::new(None);
    let codex_args = policy.codex_args(externally_isolated, image_paths);
    let mut agent_sessions = 0u32;
    let mut idle_retries = 0u32;
    let mut completion_restarts = 0u32;
    let mut retry_of_call_id = None;
    let initial_prompt_tokens = prompt.len().div_ceil(4) as u64;
    let mut accumulated_usage = BudgetUsage {
        initial_prompt_tokens,
        ..BudgetUsage::default()
    };
    let highest_intervention = Cell::new(BudgetStage::Normal);
    let published_feedback = Cell::new(0u32);
    let derived_progress = RefCell::new(HashSet::<String>::new());
    let ownership_interrupts = Cell::new(0u32);
    let ownership_override_reason = RefCell::new(None::<String>);
    let focused_validation = RefCell::new(Vec::<FocusedValidationEvidence>::new());
    let completion_ready = Cell::new(false);
    let completion_rejection_reported = Cell::new(false);
    let started = Instant::now();
    let mut session_prompt = prompt.to_owned();
    match accumulated_usage.stage(context.budget) {
        BudgetStage::Normal => {}
        BudgetStage::Constrained => {
            highest_intervention.set(BudgetStage::Constrained);
            session_prompt.push_str("\n\n");
            session_prompt.push_str(&constrained_prompt(false));
            context.reporter.step(
                &format!("{step_id}_initial_prompt_constrained"),
                StepStatus::Running,
                "Initial prompt reached 70% of its budget; broad discovery is disabled",
                Some(json!({"threshold_percent": 70, "usage": accumulated_usage, "action": "constrain_initial_session"})),
            )?;
        }
        BudgetStage::FinalizationRequired => {
            highest_intervention.set(BudgetStage::FinalizationRequired);
            session_prompt.push_str("\n\n");
            session_prompt.push_str(&constrained_prompt(true));
            context.reporter.step(
                &format!("{step_id}_initial_prompt_finalization"),
                StepStatus::Running,
                "Initial prompt reached 90% of its budget; immediate focused finalization is required",
                Some(json!({"threshold_percent": 90, "usage": accumulated_usage, "action": "finalize_initial_session"})),
            )?;
        }
        BudgetStage::HardLimit => {
            bail!(
                "initial Codex prompt estimate {initial_prompt_tokens} exceeded the configured {} token hard limit",
                context.budget.max_initial_prompt_tokens
            );
        }
    }
    let codex_succeeded = loop {
        agent_sessions = agent_sessions.saturating_add(1);
        let codex_attempt = agent_sessions;
        let session_started = Instant::now();
        let telemetry = RefCell::new(context.reporter.start_codex_telemetry(
            &codex_args,
            &session_prompt,
            context.mission_class,
            context.budget,
            step_id,
            codex_attempt,
            retry_of_call_id,
        ));
        let result = context.executor.streaming(
            context.executor_handle,
            RunCommand {
                args: &codex_args,
                cwd: &context.repo.root,
                stdin_text: Some(&session_prompt),
                running: context.running,
                timeout: Duration::from_secs(policy.timeout_seconds),
                idle_timeout: Some(policy.codex_idle_timeout()),
                output_is_activity: Some(codex_output_is_meaningful_activity),
                max_output_bytes: context.app.config.max_command_output_bytes as usize,
                environment_allowlist: &policy.codex.environment_allowlist,
                limits: Some(child_limits(
                    context.app,
                    Duration::from_secs(policy.timeout_seconds),
                )),
                max_workspace_bytes: context.app.config.max_workspace_bytes,
            },
            |line| {
                telemetry.borrow_mut().observe_line(line);
                if let Some(command) = attempted_worker_owned_command(line, &policy.quality_gates) {
                    if let Some(reason) = ownership_override_reason.borrow_mut().take()
                        && accumulated_usage
                            .combined_with(telemetry.borrow().budget_usage(
                                initial_prompt_tokens,
                                session_started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                            ))
                            .stage(context.budget)
                            == BudgetStage::Normal
                    {
                        context.reporter.step(
                            &format!("{step_id}_ownership_exception"),
                            StepStatus::Running,
                            "Codex used an explicitly justified exceptional validation override",
                            Some(json!({"command": command, "reason": reason, "override_allowed": true})),
                        )?;
                    } else {
                        return Err(CodexControl::WorkerOwnedCommand(command).into());
                    }
                }
                if let Some((progress_id, progress_message)) = derived_progress_from_line(line)
                    && derived_progress.borrow_mut().insert(progress_id.clone())
                {
                    context.reporter.step(
                        &format!("{step_id}_{progress_id}"),
                        StepStatus::Running,
                        progress_message,
                        Some(json!({"source": "worker_derived"})),
                    )?;
                }
                if let Some(command) = successful_focused_validation_command(
                    line,
                    &policy.quality_gates,
                ) {
                    let source_tree_hash = source_tree_hash(&context.repo.root)?;
                    let evidence = FocusedValidationEvidence {
                        command: command.clone(),
                        source_tree_hash: source_tree_hash.clone(),
                    };
                    if !focused_validation.borrow().contains(&evidence) {
                        focused_validation.borrow_mut().push(evidence);
                        context.reporter.step(
                            &format!("{step_id}_focused_validation_passed"),
                            StepStatus::Completed,
                            "Recorded successful focused validation against the current source tree",
                            Some(json!({
                                "command": command,
                                "source_tree_hash": source_tree_hash,
                                "reusable": true
                            })),
                        )?;
                    }
                }
                if let Some(message) = feedback_from_output_line(line) {
                    let normalized = message.to_ascii_lowercase();
                    if normalized.contains("focused alternative cannot")
                        || normalized.contains("exceptional override")
                    {
                        ownership_override_reason.replace(Some(message.clone()));
                    }
                    if let Some(action) = blocked_action_from_feedback(&message) {
                        blocked_action.replace(Some(action));
                    }
                    if should_publish_feedback(&message, published_feedback.get()) {
                        context.reporter.feedback(&message)?;
                        published_feedback.set(published_feedback.get().saturating_add(1));
                    }
                    if is_completion_feedback(&message) {
                        let changed_paths = context.repo.new_agent_paths(context.baseline)?;
                        let tree_hash = source_tree_hash(&context.repo.root)?;
                        let readiness = completion_readiness(
                            &message,
                            &changed_paths,
                            &tree_hash,
                            &focused_validation.borrow(),
                        );
                        if readiness.ready {
                            context.reporter.step(
                                &format!("{step_id}_completion_ready"),
                                StepStatus::Completed,
                                "Codex supplied complete delivery and validation evidence",
                                Some(json!({
                                    "validation_mode": readiness.validation_mode,
                                    "changed_paths": changed_paths,
                                    "source_tree_hash": tree_hash
                                })),
                            )?;
                            completion_ready.set(true);
                            return Err(CodexControl::Completion.into());
                        }
                        if !completion_rejection_reported.replace(true) {
                            context.reporter.step(
                                &format!("{step_id}_completion_not_ready"),
                                StepStatus::Running,
                                "Codex requested completion without the required implementation and validation evidence",
                                Some(json!({
                                    "missing": readiness.missing,
                                    "changed_paths": changed_paths,
                                    "source_tree_hash": tree_hash
                                })),
                            )?;
                        }
                    }
                }
                let current = accumulated_usage.combined_with(
                    telemetry.borrow().budget_usage(
                        initial_prompt_tokens,
                        session_started
                            .elapsed()
                            .as_millis()
                            .try_into()
                            .unwrap_or(u64::MAX),
                    ),
                );
                let stage = current.stage(context.budget);
                if stage > highest_intervention.get() {
                    highest_intervention.set(stage);
                    return Err(CodexControl::Budget(stage).into());
                }
                Ok(())
            },
        );
        let outcome = match &result {
            Ok(status) if status.success() => SessionOutcome::Succeeded,
            Ok(_) => SessionOutcome::Failed,
            Err(error)
                if error
                    .downcast_ref::<CodexControl>()
                    .is_some_and(|control| matches!(control, CodexControl::Completion)) =>
            {
                SessionOutcome::Succeeded
            }
            Err(error) if command::is_timeout(error) || command::is_idle_timeout(error) => {
                SessionOutcome::Timeout
            }
            Err(_) => SessionOutcome::Failed,
        };
        let mut telemetry = telemetry.into_inner();
        telemetry.finish(outcome);
        retry_of_call_id = telemetry.last_model_call_id();
        let session_usage = telemetry.budget_usage(
            initial_prompt_tokens,
            session_started
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        );
        accumulated_usage = accumulated_usage.combined_with(session_usage);
        let delta = telemetry.take_legacy_delta();
        context.reporter.record_token_consumption_delta(delta)?;
        context.reporter.flush_telemetry();
        match result {
            Ok(status) if status.success() && completion_ready.get() => break true,
            Ok(status) if status.success() && completion_restarts < 1 => {
                completion_restarts = completion_restarts.saturating_add(1);
                session_prompt = compact_handoff_prompt(
                    context,
                    "The previous session exited without complete delivery evidence. Inspect the existing implementation, resolve anything incomplete, perform one focused validation when applicable, and return the required structured completion lines.",
                    &focused_validation.borrow(),
                    accumulated_usage,
                )?;
                context.reporter.step(
                    &format!("{step_id}_completion_retry"),
                    StepStatus::Running,
                    "Restarting once with a compact handoff to obtain complete delivery evidence",
                    Some(json!({"agent_sessions_completed": agent_sessions})),
                )?;
                continue;
            }
            Ok(status) => break status.success() && completion_ready.get(),
            Err(error) => {
                if let Some(control) = error.downcast_ref::<CodexControl>().cloned() {
                    match control {
                        CodexControl::Completion => {
                            context.reporter.step(
                                &format!("{step_id}_completion_controller"),
                                StepStatus::Completed,
                                "Codex completion marker and committable diff detected; transitioning immediately to worker validation",
                                Some(json!({"action": "terminate_codex_and_run_worker_gates"})),
                            )?;
                            break true;
                        }
                        CodexControl::Budget(BudgetStage::Constrained) => {
                            context.reporter.step(
                            &format!("{step_id}_budget_constrained"),
                            StepStatus::Running,
                            "Codex reached 70% of an execution budget; restarting with constrained scope",
                            Some(json!({"threshold_percent": 70, "usage": accumulated_usage, "action": "compact_constrained_restart"})),
                        )?;
                            session_prompt = compact_handoff_prompt(
                                context,
                                &constrained_prompt(false),
                                &focused_validation.borrow(),
                                accumulated_usage,
                            )?;
                        }
                        CodexControl::Budget(BudgetStage::FinalizationRequired) => {
                            context.reporter.step(
                            &format!("{step_id}_budget_finalization"),
                            StepStatus::Running,
                            "Codex reached 90% of an execution budget; requiring finalization",
                            Some(json!({"threshold_percent": 90, "usage": accumulated_usage, "action": "compact_finalization_restart"})),
                        )?;
                            session_prompt = compact_handoff_prompt(
                                context,
                                &constrained_prompt(true),
                                &focused_validation.borrow(),
                                accumulated_usage,
                            )?;
                        }
                        CodexControl::Budget(BudgetStage::HardLimit) => {
                            let paths = context.repo.new_agent_paths(context.baseline)?;
                            let tree_hash = source_tree_hash(&context.repo.root)?;
                            let has_current_validation = focused_validation
                                .borrow()
                                .iter()
                                .any(|evidence| evidence.source_tree_hash == tree_hash);
                            let can_verify = !paths.is_empty() && has_current_validation;
                            context.reporter.step(
                            &format!("{step_id}_budget_hard_limit"),
                            if can_verify { StepStatus::Completed } else { StepStatus::Failed },
                            if can_verify {
                                "Codex reached the hard budget with current focused-validation evidence; worker gates will determine correctness"
                            } else {
                                "Codex exhausted its execution budget without enough evidence to continue safely"
                            },
                            Some(json!({"threshold_percent": 100, "usage": accumulated_usage, "action": if can_verify { "run_worker_gates" } else { "fail" }, "changed_paths": paths, "focused_validation_current": has_current_validation})),
                        )?;
                            if !can_verify {
                                bail!(
                                    "Codex execution budget exhausted before producing a complete, focused-validated change"
                                );
                            }
                            break true;
                        }
                        CodexControl::Budget(BudgetStage::Normal) => unreachable!(),
                        CodexControl::WorkerOwnedCommand(command) => {
                            let attempts = ownership_interrupts.get().saturating_add(1);
                            ownership_interrupts.set(attempts);
                            context.reporter.step(
                            &format!("{step_id}_ownership_override_{attempts}"),
                            StepStatus::Running,
                            "Stopped a Codex attempt before a worker-owned full command could duplicate deterministic validation",
                            Some(json!({"command": command, "override_allowed": false, "action": "compact_restart"})),
                        )?;
                            if attempts >= 2 {
                                bail!(
                                    "Codex repeatedly attempted worker-owned full validation command `{command}`"
                                );
                            }
                            session_prompt = compact_handoff_prompt(
                                context,
                                &format!(
                                    "Do not run `{command}` or any full repository gate; the worker owns it. Complete the implementation and use one focused validation that targets the changed code."
                                ),
                                &focused_validation.borrow(),
                                accumulated_usage,
                            )?;
                        }
                    }
                    continue;
                }
                if command::is_idle_timeout(&error) && idle_retries + 1 < CODEX_IDLE_ATTEMPTS {
                    idle_retries = idle_retries.saturating_add(1);
                    let delay = Duration::from_secs(u64::from(idle_retries) * 2);
                    context.reporter.step(
                        &format!("{step_id}_idle_retry"),
                        StepStatus::Running,
                        &format!(
                            "Codex stopped producing output; restarting ephemeral attempt {} of {} in {}s",
                            agent_sessions + 1,
                            CODEX_IDLE_ATTEMPTS,
                            delay.as_secs()
                        ),
                        Some(json!({
                            "attempt": agent_sessions + 1,
                            "max_attempts": CODEX_IDLE_ATTEMPTS,
                            "idle_timeout_seconds": policy.codex_idle_timeout().as_secs()
                        })),
                    )?;
                    thread::sleep(delay);
                    session_prompt = compact_handoff_prompt(
                        context,
                        &constrained_prompt(false),
                        &focused_validation.borrow(),
                        accumulated_usage,
                    )?;
                    continue;
                }
                return Err(error);
            }
        }
    };
    if let Some(action) = blocked_action.into_inner() {
        if is_validation_only_blocker(&action)
            && policy.quality_gates.iter().any(|gate| gate.required)
        {
            context.reporter.step(
                &format!("{step_id}_validation_handoff"),
                StepStatus::Completed,
                "Codex could not complete local validation; continuing with runner-owned quality gates",
                Some(json!({"reported_action": action})),
            )?;
        } else {
            return Err(RunFailure::HumanIntervention { action }.into());
        }
    }
    if !codex_succeeded {
        bail!("Codex exited unsuccessfully");
    }
    context.reporter.step(
        step_id,
        StepStatus::Completed,
        "Codex iteration finished successfully",
        Some(json!({
            "usage": accumulated_usage,
            "budget": context.budget,
            "agent_sessions": agent_sessions,
            "model_inference_turns": accumulated_usage.inference_turns,
            "duration_ms": started.elapsed().as_millis()
        })),
    )?;
    Ok(())
}

fn constrained_prompt(finalization: bool) -> String {
    if finalization {
        "The mission budget is nearly exhausted. Continue from the existing workspace. Do not perform additional discovery unless required to avoid an incorrect change. Finalize the smallest correct implementation, run at most one focused validation command, inspect the changed files, and return a concise summary ending with RUSTGRID_AGENT_STATUS: COMPLETED. Do not run full repository gates; the RustGrid worker owns them.".into()
    } else {
        "You are approaching the mission execution budget. Continue from the existing workspace. Stop broad exploration. Use the files already identified, complete the smallest correct implementation, run one focused validation, inspect the diff, and finish. Do not run full repository gates; the RustGrid worker owns them.".into()
    }
}

#[derive(Debug, Eq, PartialEq)]
struct CompletionReadiness {
    ready: bool,
    missing: Vec<&'static str>,
    validation_mode: &'static str,
}

fn completion_readiness(
    message: &str,
    changed_paths: &[String],
    source_tree_hash: &str,
    focused_validation: &[FocusedValidationEvidence],
) -> CompletionReadiness {
    let normalized = message.to_ascii_uppercase();
    let implementation_complete = normalized
        .lines()
        .any(|line| line.trim() == "RUSTGRID_IMPLEMENTATION_COMPLETE: YES");
    let validation_passed = normalized
        .lines()
        .any(|line| line.trim() == "RUSTGRID_FOCUSED_VALIDATION: PASSED");
    let validation_not_applicable = normalized
        .lines()
        .any(|line| line.trim() == "RUSTGRID_FOCUSED_VALIDATION: NOT_APPLICABLE");
    let validation_deferred = normalized
        .lines()
        .any(|line| line.trim() == "RUSTGRID_FOCUSED_VALIDATION: DEFERRED_TO_WORKER");
    let validation_reason = message.lines().any(|line| {
        line.trim()
            .strip_prefix("RUSTGRID_VALIDATION_REASON:")
            .is_some_and(|reason| !reason.trim().is_empty())
    });
    let current_validation = focused_validation
        .iter()
        .any(|evidence| evidence.source_tree_hash == source_tree_hash);
    let not_applicable_allowed = !changed_paths.is_empty()
        && changed_paths.iter().all(|path| {
            let lower = path.to_ascii_lowercase();
            lower.ends_with(".md")
                || lower.ends_with(".mdx")
                || lower.ends_with(".txt")
                || lower.ends_with(".adoc")
                || lower.ends_with("license")
        });
    let validation_ready = (validation_passed && current_validation)
        || (validation_not_applicable && validation_reason && not_applicable_allowed)
        || (validation_deferred && validation_reason);
    let mut missing = Vec::new();
    if changed_paths.is_empty() {
        missing.push("committable_diff");
    }
    if !implementation_complete {
        missing.push("implementation_complete_declaration");
    }
    if !validation_ready {
        missing.push("current_focused_validation_or_not_applicable_reason");
    }
    CompletionReadiness {
        ready: missing.is_empty(),
        missing,
        validation_mode: if validation_passed && current_validation {
            "focused_passed"
        } else if validation_not_applicable && validation_reason && not_applicable_allowed {
            "not_applicable"
        } else if validation_deferred && validation_reason {
            "deferred_to_worker"
        } else {
            "missing"
        },
    }
}

fn compact_handoff_prompt(
    context: &CodexContext<'_>,
    directive: &str,
    focused_validation: &[FocusedValidationEvidence],
    usage: BudgetUsage,
) -> Result<String> {
    let changed_paths = context.repo.new_agent_paths(context.baseline)?;
    let diff = command::capture(
        "git",
        ["diff", "--no-ext-diff", "--unified=3", "HEAD", "--"],
        &context.repo.root,
    )?
    .stdout;
    let validation = if focused_validation.is_empty() {
        "none recorded".into()
    } else {
        focused_validation
            .iter()
            .map(|evidence| {
                format!(
                    "- `{}` at source tree {}",
                    evidence.command, evidence.source_tree_hash
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    render_compact_handoff(
        context.ticket,
        context.mission_class,
        &context.manifest.run.input_prompt,
        directive,
        &changed_paths,
        &validation,
        usage,
        &diff,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_compact_handoff(
    ticket: &Ticket,
    mission_class: MissionClass,
    run_prompt: &str,
    directive: &str,
    changed_paths: &[String],
    validation: &str,
    usage: BudgetUsage,
    diff: &str,
) -> Result<String> {
    Ok(format!(
        "This is a compact continuation in the existing workspace. Preserve the current implementation and do not restart broad discovery.\n\nTicket: {} - {}\nTicket requirements:\n{}\nRun-specific requirements:\n{}\nMission class: {}\n\nRequired next action:\n{}\n\nChanged paths: {}\n\nFocused-validation evidence:\n{}\n\nBudget used so far:\n{}\n\nCurrent bounded diff:\n{}\n\nDo not reinstall dependencies, commit, push, open a pull request, or run full repository gates. Finish only when the implementation is complete. End with `RUSTGRID_IMPLEMENTATION_COMPLETE: YES`, then `RUSTGRID_FOCUSED_VALIDATION: PASSED` after a successful focused command, `NOT_APPLICABLE` for documentation-only changes, or `DEFERRED_TO_WORKER` when a code change has no viable focused command. The latter two require `RUSTGRID_VALIDATION_REASON: <reason>`. Finish with `RUSTGRID_AGENT_STATUS: COMPLETED`.",
        ticket.key,
        ticket.title,
        truncate_text(
            ticket.description.as_deref().unwrap_or("(none provided)"),
            8_000
        ),
        truncate_text(run_prompt, 8_000),
        mission_class.as_str(),
        directive,
        if changed_paths.is_empty() {
            "(none)".into()
        } else {
            changed_paths.join(", ")
        },
        validation,
        serde_json::to_string(&usage)?,
        truncate_text(diff, 20_000),
    ))
}

fn successful_focused_validation_command(
    line: &str,
    gates: &[crate::manifest::QualityGatePolicy],
) -> Option<String> {
    let event = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if event.get("type").and_then(serde_json::Value::as_str) != Some("item.completed") {
        return None;
    }
    let item = event.get("item")?;
    if item.get("type").and_then(serde_json::Value::as_str) != Some("command_execution") {
        return None;
    }
    let command = item
        .get("command")
        .and_then(serde_json::Value::as_str)?
        .trim();
    let failed = matches!(
        item.get("status").and_then(serde_json::Value::as_str),
        Some("failed" | "error" | "cancelled" | "canceled" | "timeout" | "timed_out")
    ) || item
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .is_some_and(|code| code != 0)
        || item.get("error").is_some_and(|error| !error.is_null());
    if failed
        || attempted_worker_owned_command_for(command, gates).is_some()
        || !is_validation_command(command)
    {
        return None;
    }
    Some(command.to_owned())
}

fn is_validation_command(command: &str) -> bool {
    let command = command.to_ascii_lowercase();
    [
        " test",
        "test ",
        "npm test",
        "vitest",
        "jest",
        "pytest",
        "rspec",
        "cargo check",
        "clippy",
        " lint",
        "lint ",
        "typecheck",
        "type-check",
        "tsc ",
    ]
    .iter()
    .any(|needle| command.contains(needle))
}

pub(crate) fn run_gates_with_repairs(
    codex: &CodexContext<'_>,
    gates: QualityGateContext<'_>,
    source: &str,
) -> Result<()> {
    let mut executed = HashSet::<(String, String)>::new();
    let mut failure_repetitions = HashMap::<(String, String), u32>::new();
    for attempt in 1..=VALIDATION_REPAIR_ATTEMPTS {
        let outcome = run_quality_gates(gates, &mut executed)?;
        if outcome.passed() {
            return Ok(());
        }
        let diagnostics = outcome.diagnostics();
        let failure_fingerprint = hex::encode(Sha256::digest(diagnostics.as_bytes()));
        let tree_hash = source_tree_hash(&gates.repo.root)?;
        let repetitions = failure_repetitions
            .entry((failure_fingerprint.clone(), tree_hash.clone()))
            .or_default();
        *repetitions = repetitions.saturating_add(1);
        codex.reporter.step(
            &format!("validation_failure_signature_{attempt}"),
            StepStatus::Running,
            "Recorded worker-gate failure signature and source-tree state",
            Some(json!({
                "failure_fingerprint": failure_fingerprint,
                "source_tree_hash": tree_hash,
                "unchanged_repetitions": repetitions,
                "max_repetitions": VALIDATION_REPAIR_ATTEMPTS
            })),
        )?;
        if attempt == VALIDATION_REPAIR_ATTEMPTS {
            return Err(RunFailure::ValidationRepairsExhausted {
                attempts: VALIDATION_REPAIR_ATTEMPTS,
                diagnostics: truncate_text(&diagnostics, 20_000),
            }
            .into());
        }
        let repair_attempt = attempt + 1;
        codex.reporter.step(
            &format!("validation_repair_{repair_attempt}"),
            StepStatus::Running,
            &format!(
                "{source} failed; asking Codex to repair attempt {repair_attempt} of {VALIDATION_REPAIR_ATTEMPTS}"
            ),
            Some(json!({"attempt": repair_attempt, "source": source})),
        )?;
        let diff = command::capture(
            "git",
            ["diff", "--no-ext-diff", "--unified=3", "HEAD", "--"],
            &codex.repo.root,
        )?
        .stdout;
        let changed = codex.repo.new_agent_paths(codex.baseline)?;
        let prompt = format!(
            "This is a new compact repair session. Fix only the worker-gate failure below in the existing workspace. Do not replay broad discovery, reinstall dependencies, run full repository gates, commit, push, or open a pull request. Run at most one focused diagnostic or test, inspect the diff, and finish. {}\n\nTicket: {} - {}\nValidation source: {source}\nRemaining repair cycles: {}\nFailure fingerprint: {}\nChanged files: {}\n\nFailure summary:\n{}\n\nCurrent bounded diff:\n{}",
            if *repetitions > 1 {
                "The same failure repeated without a meaningful source-tree change; inspect the relevant test setup or one neighboring test instead of retrying the same command blindly."
            } else {
                ""
            },
            gates.ticket.key,
            gates.ticket.title,
            VALIDATION_REPAIR_ATTEMPTS - attempt,
            failure_fingerprint,
            changed.join(", "),
            truncate_text(&diagnostics, 20_000),
            truncate_text(&diff, 20_000),
        );
        run_codex_prompt(
            codex,
            &prompt,
            &[],
            &format!("validation_repair_{repair_attempt}_codex"),
            "Running Codex validation repair",
        )?;
    }
    unreachable!("bounded validation repair loop always returns")
}

fn run_quality_gates(
    context: QualityGateContext<'_>,
    executed: &mut HashSet<(String, String)>,
) -> Result<QualityGateOutcome> {
    let QualityGateContext {
        app,
        api,
        run,
        ticket,
        reporter,
        repo,
        running,
        manifest,
        executor,
        executor_handle,
    } = context;
    let policy = manifest.policy()?;
    reporter.set_phase(RunPhase::Verifying);
    let mut outcome = QualityGateOutcome::default();
    for gate_policy in &policy.quality_gates {
        let source_tree_hash = source_tree_hash(&repo.root)?;
        if executed.contains(&(gate_policy.command.clone(), source_tree_hash.clone())) {
            reporter.step(
                &format!("{}_duplicate_avoided", gate_policy.id),
                StepStatus::Completed,
                "Skipped a duplicate full quality gate against an unchanged source tree",
                Some(json!({
                    "gate_id": gate_policy.id,
                    "command": gate_policy.command,
                    "source_tree_hash": source_tree_hash,
                    "duplicate_avoided": true
                })),
            )?;
            continue;
        }
        let (effective_command, dependency_bootstrap_reused) =
            gate_command_without_redundant_bootstrap(
                &repo.root,
                &gate_policy.command,
                reporter.dependency_state().as_ref(),
            )?;
        reporter.step(
            &gate_policy.id,
            StepStatus::Running,
            &format!("Running quality gate: {effective_command}"),
            Some(json!({
                "gate_id": gate_policy.id,
                "required": gate_policy.required,
                "configured_command": gate_policy.command,
                "effective_command": effective_command,
                "dependency_bootstrap_reused": dependency_bootstrap_reused,
                "source_tree_hash": source_tree_hash
            })),
        )?;
        if effective_command.is_empty() {
            reporter.step(
                &gate_policy.id,
                StepStatus::Completed,
                "Quality gate satisfied by reusable dependency bootstrap state",
                Some(json!({"gate_id": gate_policy.id, "source_tree_hash": source_tree_hash})),
            )?;
            continue;
        }
        let gate_total_started = Instant::now();
        let mut gate_attempt = 1u32;
        let gate = loop {
            let gate_started = Instant::now();
            let gate = executor.captured(
                executor_handle,
                &effective_command,
                RunCommand {
                    args: &[],
                    cwd: &repo.root,
                    stdin_text: None,
                    running,
                    timeout: Duration::from_secs(gate_policy.timeout_seconds),
                    idle_timeout: None,
                    output_is_activity: None,
                    max_output_bytes: app.config.max_command_output_bytes as usize,
                    environment_allowlist: &policy.codex.environment_allowlist,
                    limits: Some(child_limits(
                        app,
                        Duration::from_secs(gate_policy.timeout_seconds),
                    )),
                    max_workspace_bytes: app.config.max_workspace_bytes,
                },
            )?;
            let output = combine_output(&gate.stdout, &gate.stderr);
            if gate.status.success() || gate_attempt >= 3 || !is_transient_gate_failure(&output) {
                break (gate, gate_started.elapsed());
            }
            let delay = Duration::from_secs(u64::from(gate_attempt) * 2);
            let message = format!(
                "Quality gate hit a transient network failure; retrying attempt {} of 3 in {}s",
                gate_attempt + 1,
                delay.as_secs()
            );
            eprintln!("[warning] {message}");
            reporter.log(&format!("{message}\n{output}"))?;
            thread::sleep(delay);
            gate_attempt += 1;
        };
        let (gate, gate_duration) = gate;
        print_output(&gate.stdout, &gate.stderr);
        let gate_output = combine_output(&gate.stdout, &gate.stderr);
        let passed = gate.status.success();
        if passed {
            executed.insert((gate_policy.command.clone(), source_tree_hash.clone()));
            if !dependency_bootstrap_reused
                && let Some((manager, canonical_command)) = dependency_bootstrap_command(&repo.root)
                && command_starts_with_dependency_bootstrap(&gate_policy.command, manager)
            {
                reporter.record_dependency_state(
                    DependencyState::inspect(&repo.root, manager, canonical_command)?.completed(),
                )?;
            }
        }
        let processed = process_command_output(
            &repo.root,
            &effective_command,
            &gate_output,
            passed,
            gate.status.code(),
            gate_duration,
        )?;
        reporter.log(&processed.model_summary)?;
        api.report_quality_gate(
            &ticket.id,
            &run.id,
            &gate_policy.id,
            &effective_command,
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
            Some(json!({
                "gate_id": gate_policy.id,
                "exit_code": gate.status.code(),
                "source_tree_hash": source_tree_hash,
                "raw_output_location": processed.raw_output_location,
                "original_characters": processed.original_characters,
                "model_characters": processed.model_characters,
                "duration_ms": gate_total_started.elapsed().as_millis(),
                "output_mode": processed.mode,
                "truncated": processed.truncated
            })),
        )?;
        if gate_policy.required && !passed {
            outcome.failures.push(QualityGateFailure {
                gate_id: gate_policy.id.clone(),
                command: effective_command,
                status: gate.status.to_string(),
                output: processed.model_summary,
            });
        }
    }

    reporter.set_phase(RunPhase::Publishing);
    Ok(outcome)
}

fn source_tree_hash(root: &std::path::Path) -> Result<String> {
    let status = command::capture("git", ["status", "--porcelain=v1", "-z"], root)?;
    let diff = command::capture("git", ["diff", "--binary", "HEAD", "--"], root)?;
    let untracked = command::capture(
        "git",
        ["ls-files", "--others", "--exclude-standard", "-z"],
        root,
    )?;
    let mut digest = Sha256::new();
    digest.update(status.stdout.as_bytes());
    digest.update(diff.stdout.as_bytes());
    for relative in untracked
        .stdout
        .split('\0')
        .filter(|value| !value.is_empty())
    {
        digest.update(relative.as_bytes());
        let path = root.join(relative);
        if path.is_file() {
            digest.update(
                std::fs::read(&path)
                    .with_context(|| format!("could not fingerprint {}", path.display()))?,
            );
        }
    }
    Ok(hex::encode(digest.finalize()))
}

fn gate_command_without_redundant_bootstrap(
    root: &std::path::Path,
    command: &str,
    installed: Option<&DependencyState>,
) -> Result<(String, bool)> {
    let Some(installed) = installed else {
        return Ok((command.into(), false));
    };
    let current = DependencyState::inspect(root, &installed.manager, &installed.command)?;
    if !installed.reusable_against(&current) {
        return Ok((command.into(), false));
    }
    let mut segments = command.split("&&").map(str::trim).collect::<Vec<_>>();
    let Some(first) = segments.first() else {
        return Ok((command.into(), false));
    };
    let is_bootstrap = command_starts_with_dependency_bootstrap(first, &installed.manager);
    if !is_bootstrap {
        return Ok((command.into(), false));
    }
    segments.remove(0);
    Ok((segments.join(" && "), true))
}

fn command_starts_with_dependency_bootstrap(command: &str, manager: &str) -> bool {
    let first = command.split("&&").next().unwrap_or_default().trim();
    match manager {
        "npm" => first.starts_with("npm ci"),
        "pnpm" => first.starts_with("pnpm install"),
        "yarn" => first.starts_with("yarn install"),
        "bun" => first.starts_with("bun install"),
        _ => false,
    }
}

fn truncate_text(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...[truncated]", &value[..end])
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

fn codex_output_is_meaningful_activity(line: &str) -> bool {
    let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
        return true;
    };
    match event.get("type").and_then(serde_json::Value::as_str) {
        Some("item.started" | "item.updated" | "item.completed") => {
            event
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(serde_json::Value::as_str)
                != Some("reasoning")
        }
        Some("thread.started" | "turn.started" | "turn.completed" | "turn.failed" | "error") => {
            true
        }
        Some(_) => false,
        None => true,
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

fn attempted_worker_owned_command(
    line: &str,
    gates: &[crate::manifest::QualityGatePolicy],
) -> Option<String> {
    let event = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if event.get("type").and_then(serde_json::Value::as_str) != Some("item.started") {
        return None;
    }
    let item = event.get("item")?;
    if item.get("type").and_then(serde_json::Value::as_str) != Some("command_execution") {
        return None;
    }
    let command = item
        .get("command")
        .and_then(serde_json::Value::as_str)?
        .trim();
    attempted_worker_owned_command_for(command, gates)
}

fn attempted_worker_owned_command_for(
    command: &str,
    gates: &[crate::manifest::QualityGatePolicy],
) -> Option<String> {
    command
        .split("&&")
        .flat_map(|segment| segment.split(';'))
        .map(str::trim)
        .any(|candidate| {
            gates
                .iter()
                .flat_map(|gate| gate.command.split("&&"))
                .any(|worker_command| command_matches_full_gate(candidate, worker_command))
        })
        .then(|| command.to_owned())
}

fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn command_matches_full_gate(candidate: &str, configured: &str) -> bool {
    let candidate = normalize_command(candidate);
    let configured = normalize_command(configured);
    if candidate == configured {
        return true;
    }
    let Some(suffix) = candidate.strip_prefix(&format!("{configured} ")) else {
        return false;
    };
    if configured.contains(" build") || configured.ends_with("build") {
        return true;
    }
    let broad_flags = [
        "--",
        "--run",
        "--all",
        "--all-targets",
        "--all-features",
        "--workspace",
        "--coverage",
        "--runinband",
        "--watch=false",
        "--no-watch",
        "--ci",
    ];
    let normalized_suffix = suffix.to_ascii_lowercase();
    let tokens = normalized_suffix.split_whitespace().collect::<Vec<_>>();
    !tokens.is_empty()
        && tokens
            .iter()
            .all(|token| broad_flags.contains(token) || token.starts_with("--color"))
}

fn derived_progress_from_line(line: &str) -> Option<(String, &'static str)> {
    let event = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if event.get("type").and_then(serde_json::Value::as_str) != Some("item.started") {
        return None;
    }
    let item = event.get("item")?;
    match item.get("type").and_then(serde_json::Value::as_str)? {
        "file_change" => Some(("updating_implementation".into(), "Updating implementation")),
        "command_execution" => {
            let command = item
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if command.contains("test") || command.contains("check") || command.contains("lint") {
                Some(("focused_validation".into(), "Running focused validation"))
            } else if command.contains("rg ")
                || command.contains("grep ")
                || command.contains("find ")
            {
                Some(("searching_source".into(), "Searching relevant source files"))
            } else {
                Some((
                    "inspecting_repository".into(),
                    "Inspecting relevant repository state",
                ))
            }
        }
        _ => None,
    }
}

fn should_publish_feedback(message: &str, published: u32) -> bool {
    let normalized = message.to_ascii_lowercase();
    published == 0
        || normalized.contains("rustgrid_agent_status:")
        || normalized.contains("human_action_required:")
        || normalized.contains("scope decision")
        || normalized.contains("preserving")
        || normalized.contains("cannot safely")
}

fn is_completion_feedback(message: &str) -> bool {
    message
        .to_ascii_uppercase()
        .contains("RUSTGRID_AGENT_STATUS: COMPLETED")
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

fn is_validation_only_blocker(action: &str) -> bool {
    let action = action.to_ascii_lowercase();
    let mentions_validation = [
        "test",
        "build",
        "lint",
        "typecheck",
        "type check",
        "visual inspection",
        "screenshot",
        "dependencies",
        "dependency install",
    ]
    .iter()
    .any(|term| action.contains(term));
    let mentions_validation_infrastructure = [
        "network",
        "registry",
        "dependencies",
        "dependency",
        "browser",
        "dev server",
        "dev-server",
        "tool",
    ]
    .iter()
    .any(|term| action.contains(term));
    let mentions_implementation_blocker = [
        "credential",
        "secret",
        "permission",
        "approval",
        "decision",
        "requirement",
        "production access",
    ]
    .iter()
    .any(|term| action.contains(term));

    mentions_validation && mentions_validation_infrastructure && !mentions_implementation_blocker
}

fn is_transient_gate_failure(output: &str) -> bool {
    let output = output.to_ascii_lowercase();
    [
        "eai_again",
        "econnreset",
        "etimedout",
        "temporary failure in name resolution",
        "could not resolve host",
        "network is unreachable",
        "request or response body error: operation timed out",
    ]
    .iter()
    .any(|signature| output.contains(signature))
}

pub(crate) fn short_sha(sha: &str) -> &str {
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
    fn all_non_reasoning_codex_output_preserves_a_working_session() {
        assert!(!codex_output_is_meaningful_activity(
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}"#
        ));
        assert!(codex_output_is_meaningful_activity(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"working"}}"#
        ));
        assert!(codex_output_is_meaningful_activity(
            r#"{"type":"error","message":"Reconnecting... 4/5"}"#
        ));
        assert!(codex_output_is_meaningful_activity(
            r#"{"type":"item.started","item":{"type":"command_execution","command":"npm test"}}"#
        ));
        assert!(codex_output_is_meaningful_activity(
            r#"{"type":"item.completed","item":{"type":"file_change","changes":[]}}"#
        ));
        assert!(!codex_output_is_meaningful_activity(
            r#"{"type":"response.keepalive"}"#
        ));
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

    #[test]
    fn distinguishes_validation_handoffs_from_real_human_blockers() {
        assert!(is_validation_only_blocker(
            "Provide npm registry/network access so dependencies can install, then run npm test, npm run build, and visual inspection."
        ));
        assert!(!is_validation_only_blocker(
            "Provide the production credential and approve access."
        ));
        assert!(!is_validation_only_blocker(
            "Provide network access to implement the external API integration."
        ));
    }

    #[test]
    fn retries_only_recognized_transient_gate_failures() {
        assert!(is_transient_gate_failure(
            "npm error code EAI_AGAIN gateway.docker.internal"
        ));
        assert!(is_transient_gate_failure(
            "curl: could not resolve host: registry.npmjs.org"
        ));
        assert!(!is_transient_gate_failure(
            "FAIL src/theme.test.ts expected dark but received light"
        ));
    }

    #[test]
    fn required_gate_diagnostics_include_each_failure() {
        let outcome = QualityGateOutcome {
            failures: vec![
                QualityGateFailure {
                    gate_id: "test".into(),
                    command: "npm test".into(),
                    status: "exit status: 1".into(),
                    output: "one failed".into(),
                },
                QualityGateFailure {
                    gate_id: "lint".into(),
                    command: "npm run lint".into(),
                    status: "exit status: 2".into(),
                    output: "lint failed".into(),
                },
            ],
        };
        let diagnostics = outcome.diagnostics();
        assert!(diagnostics.contains("Gate test (`npm test`)"));
        assert!(diagnostics.contains("Gate lint (`npm run lint`)"));
    }

    #[test]
    fn selects_locked_dependency_bootstrap_without_lifecycle_scripts() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("package.json"), "{}").unwrap();
        assert_eq!(dependency_bootstrap_command(directory.path()), None);

        std::fs::write(directory.path().join("package-lock.json"), "{}").unwrap();
        assert_eq!(
            dependency_bootstrap_command(directory.path()),
            Some((
                "npm",
                "npm ci --ignore-scripts --no-audit --no-fund --prefer-offline"
            ))
        );

        std::fs::write(directory.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(
            dependency_bootstrap_command(directory.path()),
            Some((
                "pnpm",
                "pnpm install --frozen-lockfile --prefer-offline --ignore-scripts"
            ))
        );
    }

    #[test]
    fn detects_worker_owned_full_gate_but_allows_focused_variant() {
        let gates = vec![crate::manifest::QualityGatePolicy {
            id: "full".into(),
            command: "npm ci && npm test && npm run build".into(),
            timeout_seconds: 300,
            required: true,
        }];
        let full =
            r#"{"type":"item.started","item":{"type":"command_execution","command":"npm test"}}"#;
        let focused = r#"{"type":"item.started","item":{"type":"command_execution","command":"npm test -- TicketDetailPage"}}"#;
        assert_eq!(
            attempted_worker_owned_command(full, &gates).as_deref(),
            Some("npm test")
        );
        assert_eq!(attempted_worker_owned_command(focused, &gates), None);
        assert_eq!(
            attempted_worker_owned_command_for("npm test -- --run", &gates).as_deref(),
            Some("npm test -- --run")
        );
        assert_eq!(
            attempted_worker_owned_command_for("npm run build -- --mode production", &gates)
                .as_deref(),
            Some("npm run build -- --mode production")
        );
        assert_eq!(
            attempted_worker_owned_command_for("npm test && echo done", &gates).as_deref(),
            Some("npm test && echo done")
        );
        assert_eq!(
            attempted_worker_owned_command_for("npm test -- TicketDetailPage", &gates),
            None
        );
    }

    #[test]
    fn records_only_successful_focused_validation_commands() {
        let gates = vec![crate::manifest::QualityGatePolicy {
            id: "full".into(),
            command: "npm test".into(),
            timeout_seconds: 300,
            required: true,
        }];
        let focused = r#"{"type":"item.completed","item":{"type":"command_execution","command":"npm test -- TicketDetailPage","status":"completed","exit_code":0}}"#;
        let failed = r#"{"type":"item.completed","item":{"type":"command_execution","command":"npm test -- TicketDetailPage","status":"failed","exit_code":1}}"#;
        let full = r#"{"type":"item.completed","item":{"type":"command_execution","command":"npm test","status":"completed","exit_code":0}}"#;
        assert_eq!(
            successful_focused_validation_command(focused, &gates).as_deref(),
            Some("npm test -- TicketDetailPage")
        );
        assert_eq!(successful_focused_validation_command(failed, &gates), None);
        assert_eq!(successful_focused_validation_command(full, &gates), None);
    }

    #[test]
    fn completion_requires_current_validation_for_code_changes() {
        let message = "RUSTGRID_IMPLEMENTATION_COMPLETE: YES\nRUSTGRID_FOCUSED_VALIDATION: PASSED\nRUSTGRID_AGENT_STATUS: COMPLETED";
        let evidence = vec![FocusedValidationEvidence {
            command: "npm test -- TicketDetailPage".into(),
            source_tree_hash: "tree-1".into(),
        }];
        assert!(completion_readiness(message, &["src/nav.tsx".into()], "tree-1", &evidence).ready);
        let stale = completion_readiness(message, &["src/nav.tsx".into()], "tree-2", &evidence);
        assert!(!stale.ready);
        assert!(
            stale
                .missing
                .contains(&"current_focused_validation_or_not_applicable_reason")
        );
        let no_validation = "RUSTGRID_IMPLEMENTATION_COMPLETE: YES\nRUSTGRID_FOCUSED_VALIDATION: NOT_APPLICABLE\nRUSTGRID_VALIDATION_REASON: copy-only documentation change\nRUSTGRID_AGENT_STATUS: COMPLETED";
        assert!(completion_readiness(no_validation, &["README.md".into()], "tree-2", &[]).ready);
        assert!(!completion_readiness(no_validation, &["src/nav.tsx".into()], "tree-2", &[]).ready);
        let deferred = "RUSTGRID_IMPLEMENTATION_COMPLETE: YES\nRUSTGRID_FOCUSED_VALIDATION: DEFERRED_TO_WORKER\nRUSTGRID_VALIDATION_REASON: repository exposes only the authoritative full gate\nRUSTGRID_AGENT_STATUS: COMPLETED";
        assert!(completion_readiness(deferred, &["src/nav.tsx".into()], "tree-2", &[]).ready);
    }

    #[test]
    fn derives_routine_progress_from_tool_activity() {
        let search = r#"{"type":"item.started","item":{"type":"command_execution","command":"rg -n old src"}}"#;
        assert_eq!(
            derived_progress_from_line(search),
            Some(("searching_source".into(), "Searching relevant source files"))
        );
        assert!(should_publish_feedback("Starting now", 0));
        assert!(!should_publish_feedback("Reading the next file", 1));
        assert!(should_publish_feedback(
            "Done\nRUSTGRID_AGENT_STATUS: COMPLETED",
            1
        ));
        assert!(is_completion_feedback(
            "Done\nRUSTGRID_AGENT_STATUS: COMPLETED"
        ));
    }

    #[test]
    fn removes_redundant_bootstrap_from_combined_worker_gate() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("package.json"), "{}").unwrap();
        std::fs::write(directory.path().join("package-lock.json"), "{}").unwrap();
        std::fs::create_dir(directory.path().join("node_modules")).unwrap();
        let state = DependencyState::inspect(
            directory.path(),
            "npm",
            "npm ci --ignore-scripts --no-audit --no-fund --prefer-offline",
        )
        .unwrap()
        .completed();
        let (command, reused) = gate_command_without_redundant_bootstrap(
            directory.path(),
            "npm ci --maxsockets=1 && npm test && npm run build",
            Some(&state),
        )
        .unwrap();
        assert!(reused);
        assert_eq!(command, "npm test && npm run build");

        std::fs::write(directory.path().join("package-lock.json"), "changed").unwrap();
        let (command, reused) = gate_command_without_redundant_bootstrap(
            directory.path(),
            "npm ci --maxsockets=1 && npm test",
            Some(&state),
        )
        .unwrap();
        assert!(!reused);
        assert!(command.starts_with("npm ci"));
    }

    #[test]
    fn compact_repair_controls_do_not_replay_full_history() {
        let prompt = constrained_prompt(true);
        assert!(prompt.contains("existing workspace"));
        assert!(prompt.contains("at most one focused validation"));
        assert!(prompt.contains("worker owns them"));
        assert!(!prompt.contains("tool history"));
    }

    #[test]
    fn compact_handoff_preserves_ticket_diff_validation_and_budget_state() {
        let ticket = Ticket {
            id: "ticket-1".into(),
            key: "AOPS-199".into(),
            title: "Rename the navigation label".into(),
            description: Some("Change Live Fleet to Live Agents without altering routes.".into()),
            comments: vec![],
            custom_fields: serde_json::json!({}),
            previous_quality_gate_failures: vec![],
            row_version: 1,
        };
        let prompt = render_compact_handoff(
            &ticket,
            MissionClass::Configuration,
            "Preserve the existing route.",
            "Finalize the smallest correct implementation.",
            &["src/Nav.tsx".into(), "src/Nav.test.tsx".into()],
            "- `npm test -- Nav.test.tsx` at source tree abc123",
            BudgetUsage {
                inference_turns: 6,
                tool_calls: 9,
                ..BudgetUsage::default()
            },
            "- Live Fleet\n+ Live Agents",
        )
        .unwrap();
        for expected in [
            "AOPS-199",
            "without altering routes",
            "Preserve the existing route",
            "src/Nav.test.tsx",
            "npm test -- Nav.test.tsx",
            "\"inference_turns\":6",
            "Live Agents",
            "RUSTGRID_IMPLEMENTATION_COMPLETE: YES",
        ] {
            assert!(prompt.contains(expected), "missing {expected}");
        }
        assert!(!prompt.contains("prior tool history"));
    }

    #[test]
    fn configuration_regression_fixture_intervenes_and_preserves_gate_ownership() {
        let budget = MissionClass::Configuration.budget();
        let usage = BudgetUsage {
            initial_prompt_tokens: 4_161,
            inference_turns: 6,
            tool_calls: 9,
            cumulative_uncached_input_tokens: 21_500,
            cumulative_cached_input_tokens: 120_000,
            output_tokens: 4_000,
            ..BudgetUsage::default()
        };
        assert_eq!(usage.stage(budget), BudgetStage::Constrained);
        let gates = vec![crate::manifest::QualityGatePolicy {
            id: "ui".into(),
            command: "npm test && npm run build".into(),
            timeout_seconds: 300,
            required: true,
        }];
        assert!(attempted_worker_owned_command_for("npm test", &gates).is_some());
        assert!(
            attempted_worker_owned_command_for("npm test -- Navigation.test.tsx", &gates).is_none()
        );
        assert_eq!(budget.max_inference_turns, 8);
        assert_eq!(budget.max_tool_calls, 12);
        assert_eq!(budget.max_cumulative_uncached_input_tokens, 30_000);
        assert_eq!(budget.max_cumulative_cached_input_tokens, 200_000);
    }

    #[test]
    fn source_tree_hash_changes_with_untracked_file_contents() {
        let directory = tempfile::tempdir().unwrap();
        command::capture("git", ["init"], directory.path()).unwrap();
        command::capture(
            "git",
            [
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "base",
            ],
            directory.path(),
        )
        .unwrap();
        std::fs::write(directory.path().join("new.txt"), "one").unwrap();
        let first = source_tree_hash(directory.path()).unwrap();
        std::fs::write(directory.path().join("new.txt"), "two").unwrap();
        let second = source_tree_hash(directory.path()).unwrap();
        assert_ne!(first, second);
    }
}
