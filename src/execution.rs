use std::{cell::RefCell, collections::BTreeSet, sync::atomic::AtomicBool, thread, time::Duration};

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
    reporting::Reporter,
    run_error::RunFailure,
};

const CODEX_IDLE_ATTEMPTS: u32 = 3;

pub(crate) struct ImplementationContext<'a> {
    pub app: &'a AppContext,
    pub api: &'a RustGridClient,
    pub run: &'a AgentRun,
    pub ticket: &'a Ticket,
    pub reporter: &'a Reporter<'a>,
    pub repo: &'a Repo,
    pub baseline: &'a BTreeSet<String>,
    pub prompt: &'a str,
    pub running: &'a AtomicBool,
    pub manifest: &'a ExecutionManifest,
    pub executor: &'a Executor,
    pub executor_handle: &'a ExecutionHandle,
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
        running,
        manifest,
        executor,
        executor_handle,
    } = implementation;
    let policy = manifest.policy()?;
    reporter.step(
        "prompt_built",
        StepStatus::Completed,
        "Built Codex prompt from ticket and repository context",
        Some(json!({"characters": generated_prompt.len()})),
    )?;
    reporter.set_phase(RunPhase::Executing);
    reporter.step(
        "codex",
        StepStatus::Running,
        "Running Codex in the configured executor",
        None,
    )?;
    let blocked_action = RefCell::new(None);
    let codex_args = policy.codex_args();
    let mut codex_attempt = 1u32;
    let codex_status = loop {
        let result = executor.streaming(
            executor_handle,
            RunCommand {
                args: &codex_args,
                cwd: &repo.root,
                stdin_text: Some(generated_prompt),
                running,
                timeout: Duration::from_secs(policy.timeout_seconds),
                idle_timeout: Some(policy.codex_idle_timeout()),
                max_output_bytes: context.config.max_command_output_bytes as usize,
                environment_allowlist: &policy.codex.environment_allowlist,
                limits: Some(child_limits(
                    context,
                    Duration::from_secs(policy.timeout_seconds),
                )),
                max_workspace_bytes: context.config.max_workspace_bytes,
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
        );
        match result {
            Ok(status) => break status,
            Err(error)
                if command::is_idle_timeout(&error) && codex_attempt < CODEX_IDLE_ATTEMPTS =>
            {
                let delay = Duration::from_secs(u64::from(codex_attempt) * 2);
                reporter.step(
                    "codex_idle_retry",
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
            reporter.step(
                "codex_validation_handoff",
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
                    max_output_bytes: context.config.max_command_output_bytes as usize,
                    environment_allowlist: &policy.codex.environment_allowlist,
                    limits: Some(child_limits(
                        context,
                        Duration::from_secs(gate_policy.timeout_seconds),
                    )),
                    max_workspace_bytes: context.config.max_workspace_bytes,
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
}
