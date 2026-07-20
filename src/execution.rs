use std::{
    cell::RefCell, collections::BTreeSet, path::PathBuf, sync::atomic::AtomicBool, thread,
    time::Duration,
};

use anyhow::{Result, bail};
use serde_json::json;

use crate::{
    api::{AgentRun, RustGridClient, Ticket},
    command,
    config::AppContext,
    executor::{ExecutionHandle, Executor, RunCommand},
    git::Repo,
    lifecycle::{RunPhase, StepStatus},
    manifest::ExecutionManifest,
    mission::{MissionClass, MissionProfile},
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
    let mission_class = MissionProfile::classify(ticket, manifest).class;
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
        mission_class,
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
    let policy = manifest.policy()?;
    reporter.step(
        "dependency_bootstrap",
        StepStatus::Running,
        &format!("Installing locked {manager} dependencies before Codex execution"),
        Some(json!({"manager": manager, "command": command_text})),
    )?;
    for attempt in 1..=DEPENDENCY_INSTALL_ATTEMPTS {
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
            reporter.step(
                "dependency_bootstrap",
                StepStatus::Completed,
                &format!("Installed locked {manager} dependencies"),
                Some(json!({"manager": manager, "attempt": attempt})),
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
    let codex_args = policy.codex_args(externally_isolated, image_paths, context.mission_class);
    let mut codex_attempt = 1u32;
    let mut retry_of_call_id = None;
    let codex_status = loop {
        let telemetry = RefCell::new(context.reporter.start_codex_telemetry(
            &codex_args,
            prompt,
            context.mission_class,
            step_id,
            codex_attempt,
            retry_of_call_id,
        ));
        let result = context.executor.streaming(
            context.executor_handle,
            RunCommand {
                args: &codex_args,
                cwd: &context.repo.root,
                stdin_text: Some(prompt),
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
                if let Some(message) = feedback_from_output_line(line) {
                    if let Some(action) = blocked_action_from_feedback(&message) {
                        blocked_action.replace(Some(action));
                    }
                    context.reporter.feedback(&message)?;
                }
                Ok(())
            },
        );
        let outcome = match &result {
            Ok(status) if status.success() => SessionOutcome::Succeeded,
            Ok(_) => SessionOutcome::Failed,
            Err(error) if command::is_timeout(error) || command::is_idle_timeout(error) => {
                SessionOutcome::Timeout
            }
            Err(_) => SessionOutcome::Failed,
        };
        let mut telemetry = telemetry.into_inner();
        telemetry.finish(outcome);
        retry_of_call_id = telemetry.last_model_call_id();
        let model_calls = telemetry.model_call_count();
        let tool_calls = telemetry.tool_call_count();
        let delta = telemetry.take_legacy_delta();
        context.reporter.record_token_consumption_delta(delta)?;
        let budget = context.mission_class.budget();
        if delta.input_tokens > budget.max_input_tokens
            || model_calls > budget.max_model_calls
            || tool_calls > budget.max_tool_calls as usize
        {
            context.reporter.step(
                &format!("{step_id}_budget_warning_{codex_attempt}"),
                StepStatus::Running,
                &format!(
                    "{} mission exceeded its advisory execution budget; future calls require compaction or explicit escalation",
                    context.mission_class.as_str()
                ),
                Some(json!({
                    "mission_class": context.mission_class,
                    "input_tokens": delta.input_tokens,
                    "max_input_tokens": budget.max_input_tokens,
                    "model_calls": model_calls,
                    "max_model_calls": budget.max_model_calls,
                    "tool_calls": tool_calls,
                    "max_tool_calls": budget.max_tool_calls,
                    "enforcement": "warning"
                })),
            )?;
        }
        context.reporter.flush_telemetry();
        match result {
            Ok(status) => break status,
            Err(error)
                if command::is_idle_timeout(&error) && codex_attempt < CODEX_IDLE_ATTEMPTS =>
            {
                let delay = Duration::from_secs(u64::from(codex_attempt) * 2);
                context.reporter.step(
                    &format!("{step_id}_idle_retry"),
                    StepStatus::Running,
                    &format!(
                        "Codex stopped producing output; restarting ephemeral attempt {} of {} in {}s",
                        codex_attempt + 1,
                        CODEX_IDLE_ATTEMPTS,
                        delay.as_secs()
                    ),
                    Some(json!({
                        "attempt": codex_attempt + 1,
                        "max_attempts": CODEX_IDLE_ATTEMPTS,
                        "idle_timeout_seconds": policy.codex_idle_timeout().as_secs()
                    })),
                )?;
                thread::sleep(delay);
                codex_attempt += 1;
            }
            Err(error) => return Err(error),
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
    if !codex_status.success() {
        bail!("Codex exited with {codex_status}");
    }
    context.reporter.step(
        step_id,
        StepStatus::Completed,
        "Codex iteration finished successfully",
        None,
    )?;
    Ok(())
}

pub(crate) fn run_gates_with_repairs(
    codex: &CodexContext<'_>,
    gates: QualityGateContext<'_>,
    source: &str,
) -> Result<()> {
    for attempt in 1..=VALIDATION_REPAIR_ATTEMPTS {
        let outcome = run_quality_gates(gates)?;
        if outcome.passed() {
            return Ok(());
        }
        let diagnostics = outcome.diagnostics();
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
        let prompt = format!(
            "The implementation is not complete because required validation failed. Inspect the current workspace and fix the underlying code or configuration. Do not commit, push, or open a pull request. Re-run relevant checks while working. Treat these failures as real blockers and continue until they are resolved.\n\nValidation source: {source}\n\n{}",
            truncate_text(&diagnostics, 20_000)
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

pub(crate) fn run_quality_gates(context: QualityGateContext<'_>) -> Result<QualityGateOutcome> {
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
        reporter.step(
            &gate_policy.id,
            StepStatus::Running,
            &format!("Running quality gate: {}", gate_policy.command),
            Some(json!({"gate_id": gate_policy.id, "required": gate_policy.required})),
        )?;
        let mut gate_attempt = 1u32;
        let gate = loop {
            let gate = executor.captured(
                executor_handle,
                &gate_policy.command,
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
                break gate;
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
            outcome.failures.push(QualityGateFailure {
                gate_id: gate_policy.id.clone(),
                command: gate_policy.command.clone(),
                status: gate.status.to_string(),
                output: truncate_text(&gate_output, 16_000),
            });
        }
    }

    reporter.set_phase(RunPhase::Publishing);
    Ok(outcome)
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
    fn reasoning_protocol_frames_do_not_mask_codex_inactivity() {
        assert!(!codex_output_is_meaningful_activity(
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}"#
        ));
        assert!(codex_output_is_meaningful_activity(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"working"}}"#
        ));
        assert!(codex_output_is_meaningful_activity(
            r#"{"type":"item.started","item":{"type":"command_execution","command":"npm test"}}"#
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
}
