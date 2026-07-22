use anyhow::Result;
use serde_json::to_string_pretty;

use crate::{
    api::Ticket,
    attachments::{StagedAttachment, prompt_section},
    git::read_repo_instructions,
    mission::{ExecutionOwnership, MissionClass, ValidationPlan},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextComposition {
    pub ticket_tokens: usize,
    pub repository_instruction_tokens: usize,
    pub worker_instruction_tokens: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltPrompt {
    pub text: String,
    pub composition: ContextComposition,
}

pub fn build(
    ticket: &Ticket,
    repo_root: &std::path::Path,
    validation_plan: &ValidationPlan,
    ownership: &ExecutionOwnership,
    run_prompt: &str,
    attachments: &[StagedAttachment],
    mission_class: MissionClass,
) -> Result<String> {
    Ok(build_with_composition(
        ticket,
        repo_root,
        validation_plan,
        ownership,
        run_prompt,
        attachments,
        mission_class,
    )?
    .text)
}

pub fn build_with_composition(
    ticket: &Ticket,
    repo_root: &std::path::Path,
    validation_plan: &ValidationPlan,
    ownership: &ExecutionOwnership,
    run_prompt: &str,
    attachments: &[StagedAttachment],
    mission_class: MissionClass,
) -> Result<BuiltPrompt> {
    let mut ticket_characters = ticket.key.chars().count()
        + ticket.title.chars().count()
        + ticket
            .description
            .as_deref()
            .unwrap_or("(none provided)")
            .chars()
            .count()
        + run_prompt.chars().count();
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

Routine progress is derived from tool activity by the worker. You may send one brief initial message, one message for a meaningful scope decision or blocker, and one final summary. Do not narrate searches, file reads, edits, or routine validation.

Codex owns targeted discovery, implementation, focused validation, and focused repair. The RustGrid worker owns dependency bootstrap, full repository tests, lint, type-checking, builds, commit, push, pull-request creation, and GitHub checks. The worker already hydrated locked dependencies before starting you. Do not reinstall dependencies. Do not run a worker-owned full command yourself. You may run only the smallest focused test, lint, type-check, or diagnostic command necessary for this ticket. If a focused alternative cannot diagnose a failure, explain the exceptional override before attempting broader validation; the worker records and enforces this boundary.

For a small configuration task use this sequence:
1. Read applicable repository instructions.
2. Search for the exact label, key, or option.
3. Open direct matches and nearby tests only.
4. Inspect one representative neighboring test.
5. Change the smallest correct set of files.
6. Run one focused validation.
7. Inspect the final diff.
8. Finish.

Do not broadly read generated or oversized content such as `dist`, `build`, `coverage`, `node_modules`, minified bundles, lockfiles unrelated to the change, binary files, or generated API specifications. For a text file larger than 64 KiB, search first and read only the relevant bounded range.

Focused-validation plan:
{focused_validation}
{validation_instructions}

The RustGrid worker will run these authoritative commands after your implementation:
{worker_gates}

Do not run those full repository commands yourself. A failed worker gate may start a new compact repair session containing only the failure, current diff, and remaining budget. Inability to perform optional local validation because of transient infrastructure is not by itself a human blocker.

Use `BLOCKED` only when the code implementation itself cannot continue without a human decision, missing credential, required permission change, or required external-system state change. End your final update with exactly:
RUSTGRID_AGENT_STATUS: BLOCKED
HUMAN_ACTION_REQUIRED: <the specific action a human must take>

If the implementation is complete, end your final update with exactly these three lines. Use PASSED only after a focused command actually succeeded. Use NOT_APPLICABLE only for documentation-only changes. If a code change has no viable focused command or focused validation is blocked by transient infrastructure, use DEFERRED_TO_WORKER. NOT_APPLICABLE and DEFERRED_TO_WORKER both require a fourth `RUSTGRID_VALIDATION_REASON:` line:
RUSTGRID_IMPLEMENTATION_COMPLETE: YES
RUSTGRID_FOCUSED_VALIDATION: PASSED
RUSTGRID_AGENT_STATUS: COMPLETED

Execution ownership (audit record):
{ownership}
"#,
        key = ticket.key,
        title = ticket.title,
        description = ticket.description.as_deref().unwrap_or("(none provided)"),
        run_prompt = run_prompt,
        mission_class = mission_class.as_str(),
        focused_validation = validation_plan
            .focused_candidates
            .iter()
            .map(|candidate| format!("- {candidate}"))
            .collect::<Vec<_>>()
            .join("\n"),
        validation_instructions = validation_plan.codex_instructions,
        worker_gates = if validation_plan.worker_gates.is_empty() {
            "- (none configured)".into()
        } else {
            validation_plan
                .worker_gates
                .iter()
                .map(|gate| format!("- {gate}"))
                .collect::<Vec<_>>()
                .join("\n")
        },
        ownership = serde_json::to_string(ownership)?,
    );

    if let Some(section) = prompt_section(attachments) {
        ticket_characters = ticket_characters.saturating_add(section.chars().count());
        prompt.push('\n');
        prompt.push_str(&section);
    }

    if !ticket.comments.is_empty() {
        prompt.push_str("\nTicket comments (oldest first):\n");
        for comment in &ticket.comments {
            ticket_characters = ticket_characters
                .saturating_add(author_name(comment.author.as_ref()).chars().count())
                .saturating_add(comment.content.chars().count());
            prompt.push_str(&format!(
                "- {}: {}\n",
                author_name(comment.author.as_ref()),
                comment.content
            ));
        }
    }
    if has_value(&ticket.custom_fields) {
        let custom_fields = to_string_pretty(&ticket.custom_fields)?;
        ticket_characters = ticket_characters.saturating_add(custom_fields.chars().count());
        prompt.push_str("\nCustom fields:\n```json\n");
        prompt.push_str(&custom_fields);
        prompt.push_str("\n```\n");
    }
    if !ticket.previous_quality_gate_failures.is_empty() {
        prompt.push_str("\nPrevious quality gate failures to address:\n");
        for failure in &ticket.previous_quality_gate_failures {
            ticket_characters = ticket_characters
                .saturating_add(
                    failure
                        .command
                        .as_deref()
                        .unwrap_or("unknown command")
                        .chars()
                        .count(),
                )
                .saturating_add(failure.message.chars().count());
            prompt.push_str(&format!(
                "- [{}] {}\n",
                failure.command.as_deref().unwrap_or("unknown command"),
                failure.message
            ));
        }
    }
    let mut repository_instruction_characters = 0usize;
    for (name, content) in read_repo_instructions(repo_root)? {
        repository_instruction_characters = repository_instruction_characters
            .saturating_add(name.chars().count())
            .saturating_add(content.chars().count());
        prompt.push_str(&format!(
            "\nRepository instructions from {name}:\n```text\n{content}\n```\n"
        ));
    }
    let total_characters = prompt.chars().count();
    let worker_instruction_characters = total_characters
        .saturating_sub(ticket_characters)
        .saturating_sub(repository_instruction_characters);
    Ok(BuiltPrompt {
        text: prompt,
        composition: ContextComposition {
            ticket_tokens: ticket_characters.div_ceil(4),
            repository_instruction_tokens: repository_instruction_characters.div_ceil(4),
            worker_instruction_tokens: worker_instruction_characters.div_ceil(4),
        },
    })
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
            &ValidationPlan {
                focused_candidates: vec!["Run parser_test".into()],
                worker_gates: vec!["cargo test".into()],
                codex_instructions: "focused only".into(),
            },
            &ExecutionOwnership::default(),
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
            "transient infrastructure",
            "RUSTGRID_IMPLEMENTATION_COMPLETE: YES",
            "RUSTGRID_FOCUSED_VALIDATION: PASSED",
            "RUSTGRID_AGENT_STATUS: COMPLETED",
            "worker will run these authoritative commands",
            "Do not run those full repository commands yourself",
            "Run parser_test",
        ] {
            assert!(value.contains(expected));
        }
    }

    #[test]
    fn reports_bounded_prompt_source_estimates() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Use small modules.").unwrap();
        let ticket = Ticket {
            id: "1".into(),
            key: "RG-2".into(),
            title: "Rename navigation".into(),
            description: Some("Replace the old label.".into()),
            comments: Vec::new(),
            custom_fields: serde_json::Value::Null,
            previous_quality_gate_failures: Vec::new(),
            row_version: 1,
        };
        let built = build_with_composition(
            &ticket,
            dir.path(),
            &ValidationPlan {
                focused_candidates: vec!["Run the navigation test".into()],
                worker_gates: vec!["npm test".into()],
                codex_instructions: "focused only".into(),
            },
            &ExecutionOwnership::default(),
            "Keep the change narrow.",
            &[],
            MissionClass::Configuration,
        )
        .unwrap();
        assert!(built.composition.ticket_tokens > 0);
        assert!(built.composition.repository_instruction_tokens > 0);
        assert!(built.composition.worker_instruction_tokens > 0);
        assert!(
            built.composition.ticket_tokens
                + built.composition.repository_instruction_tokens
                + built.composition.worker_instruction_tokens
                <= built.text.len().div_ceil(4) + 2
        );
    }
}
