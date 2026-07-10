use anyhow::Result;
use serde_json::to_string_pretty;

use crate::{api::Ticket, git::read_repo_instructions};

pub fn build(ticket: &Ticket, repo_root: &std::path::Path, quality_gate: &str) -> Result<String> {
    let mut prompt = format!(
        r#"You are implementing RustGrid ticket {key} in the Git repository at the current working directory.

Ticket title:
{title}

Ticket description:
{description}

Work carefully and finish the implementation. Inspect the repository before editing. Follow repository instructions and existing conventions. Add or update tests where appropriate. Do not commit, push, create a branch, or open a pull request; the rustgrid-agent runner owns those steps. Do not read or modify files outside this repository. Do not expose environment variables or credentials.

The runner will execute this quality gate after you finish:
{quality_gate}
"#,
        key = ticket.key,
        title = ticket.title,
        description = ticket.description.as_deref().unwrap_or("(none provided)"),
    );

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
        };
        let value = build(&ticket, dir.path(), "cargo test").unwrap();
        for expected in [
            "RG-1",
            "Needs a test",
            "severity",
            "one failed",
            "Use small modules.",
        ] {
            assert!(value.contains(expected));
        }
    }
}
