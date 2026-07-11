use std::{
    cell::RefCell, collections::BTreeSet, path::Path, sync::atomic::AtomicBool, time::Duration,
};

use anyhow::{Result, bail};
use serde_json::json;

use crate::{
    api::{AgentRun, RustGridClient, Ticket},
    command,
    config::AppContext,
    git::Repo,
    lifecycle::{RunPhase, StepStatus},
    manifest::ExecutionManifest,
    reporting::Reporter,
    run_error::RunFailure,
};

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
}
