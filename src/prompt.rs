use anyhow::Result;
use serde_json::to_string_pretty;

use crate::{
    api::Ticket,
    attachments::{StagedAttachment, prompt_section},
    git::read_repo_instructions,
    mission::MissionClass,
};

pub fn build(
    ticket: &Ticket,
    repo_root: &std::path::Path,
    quality_gate: &str,
    run_prompt: &str,
    attachments: &[StagedAttachment],
    mission_class: MissionClass,
) -> Result<String> {
    let mut prompt = format!(
        r#"You are implementing RustGrid ticket {key} in the Git repository at the current working directory.

Ticket title:
{title}

Ticket description:
{description}

Run-specific instructions:
{run_prompt}

Mission class: {mission_class}

Work carefully and finish the implementation. Inspect the checked-out repository before deciding the implementation scope. Follow repository instructions and existing conventions. You may begin with targeted search, but expand the inspection whenever correctness requires it. Add or update tests where appropriate. Do not commit, push, create a branch, or open a pull request; the rustgrid-agent runner owns those steps. Do not read or modify files outside this repository. Do not expose environment variables or credentials.

Send concise progress updates at meaningful milestones while you work. The runner publishes each agent update as a separate RustGrid ticket comment, so make every update useful to a human reviewer and do not repeat yourself.

The runner, not Codex, owns final validation and publication. Attempt repository-requested dependency installation, tests, builds, linting, typechecking, dev-server startup, screenshots, or visual inspection when useful, and report exact failures. The runner hydrates locked JavaScript dependencies before starting you. If a later dependency or registry request fails with a transient DNS, proxy, timeout, or connection error, retry it with bounded backoff instead of immediately continuing without the dependency. Inability to perform validation because of transient network, registry, dependency, tool, browser, or dev-server availability is not by itself a human blocker. If the requested code implementation is complete, finish with `RUSTGRID_AGENT_STATUS: COMPLETED`; the runner will independently execute every required quality gate below. A failed gate or required GitHub workflow can return the current workspace to Codex for a bounded repair iteration, so treat those diagnostics as unfinished implementation work.

Use `BLOCKED` only when the code implementation itself cannot continue without a human decision, missing credential, required permission change, or required external-system state change. End your final update with exactly:
RUSTGRID_AGENT_STATUS: BLOCKED
HUMAN_ACTION_REQUIRED: <the specific action a human must take>

If the implementation is complete, end your final update with exactly:
RUSTGRID_AGENT_STATUS: COMPLETED

The runner will execute this quality gate after you finish:
{quality_gate}
"#,
        key = ticket.key,
        title = ticket.title,
        description = ticket.description.as_deref().unwrap_or("(none provided)"),
        run_prompt = run_prompt,
        mission_class = mission_class.as_str(),
    );

    if let Some(section) = prompt_section(attachments) {
        prompt.push('\n');
        prompt.push_str(&section);
    }

    if !ticket.comments.is_empty() {
        prompt.push_str("\nTicket comments (oldest first):\n");
        for comment in &ticket.comments {
            prompt.push_str(&format!(
                "- {}: {}\n",
                author_name(comment.author.as_ref()),
                comment.content
            ));
        }
    }
    if has_value(&ticket.custom_fields) {
        prompt.push_str("\nCustom fields:\n```json\n");
        prompt.push_str(&to_string_pretty(&ticket.custom_fields)?);
        prompt.push_str("\n```\n");
    }
    if !ticket.previous_quality_gate_failures.is_empty() {
        prompt.push_str("\nPrevious quality gate failures to address:\n");
        for failure in &ticket.previous_quality_gate_failures {
            prompt.push_str(&format!(
                "- [{}] {}\n",
                failure.command.as_deref().unwrap_or("unknown command"),
                failure.message
            ));
        }
    }
    for (name, content) in read_repo_instructions(repo_root)? {
        prompt.push_str(&format!(
            "\nRepository instructions from {name}:\n```text\n{content}\n```\n"
        ));
    }
    Ok(prompt)
}

fn author_name(author: Option<&serde_json::Value>) -> &str {
    match author {
        Some(serde_json::Value::String(value)) => value,
        Some(serde_json::Value::Object(value)) => value
            .get("name")
            .or_else(|| value.get("display_name"))
            .or_else(|| value.get("email"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown"),
        _ => "unknown",
    }
}

fn has_value(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Object(values) => !values.is_empty(),
        serde_json::Value::Array(values) => !values.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Comment, QualityGateFailure};
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn includes_all_ticket_context() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Use small modules.").unwrap();
        let ticket = Ticket {
            id: "1".into(),
            key: "RG-1".into(),
            title: "Fix it".into(),
            description: Some("Broken".into()),
            comments: vec![Comment {
                content: "Needs a test".into(),
                author: Some(json!({"name": "Sam"})),
            }],
            custom_fields: json!({"severity": "high"}),
            previous_quality_gate_failures: vec![QualityGateFailure {
                command: Some("cargo test".into()),
                message: "one failed".into(),
            }],
            row_version: 1,
        };
        let value = build(
            &ticket,
            dir.path(),
            "cargo test",
            "Fix the reported regression.",
            &[],
            MissionClass::SingleFile,
        )
        .unwrap();
        for expected in [
            "RG-1",
            "Fix the reported regression.",
            "Needs a test",
            "severity",
            "one failed",
            "Use small modules.",
            "transient network",
            "RUSTGRID_AGENT_STATUS: COMPLETED",
            "runner will independently execute every required quality gate",
        ] {
            assert!(value.contains(expected));
        }
    }
}
