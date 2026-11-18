//! The project's architecture contract, made available to the IDE session.
//!
//! RustGrid returns the contract a session works under when the session
//! starts its run, pinned to the version current then, as a dispatched run's
//! manifest does. The link hook writes it next to `ticket.md`, gives Claude
//! its guardrails and protected paths inline, and points it to the full
//! contract and plan check. When a reply ends with the escalation lines the
//! plan check asks for, the flush hook raises the escalation for an architect.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::{
    api::RustGridClient,
    architecture::{ArchitectureContext, PlanVerdict, PromptAudience, escalation_from_feedback},
};

const CONTEXT_FILE: &str = "architecture.json";
const DOCUMENT_FILE: &str = "architecture.md";
const PROBLEM_FILE: &str = "architecture.error";

/// Writes the session's contract into the ticket's directory and returns the
/// context Claude gets about it; `None` when the project defines none, in
/// which case a contract left from an earlier link is removed.
pub(crate) fn prepare(dir: &Path, run_id: &str, architecture: Option<&Value>) -> Option<String> {
    let Some(value) = architecture.filter(|value| !value.is_null()) else {
        for file in [CONTEXT_FILE, DOCUMENT_FILE, PROBLEM_FILE] {
            let _ = fs::remove_file(dir.join(file));
        }
        return None;
    };
    Some(match write(dir, run_id, value) {
        Ok((context, path)) => context_text(&context, &path),
        Err(error) => format!(
            "Architecture: RustGrid sent this project's architecture contract, but it could not be used ({error:#}). Tell the user, and do not change code across module boundaries until they confirm the contract."
        ),
    })
}

fn write(dir: &Path, run_id: &str, value: &Value) -> Result<(ArchitectureContext, PathBuf)> {
    let context: ArchitectureContext =
        serde_json::from_value(value.clone()).context("the contract is malformed")?;
    context.validate(run_id)?;
    fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    super::ticket::write_private(
        &dir.join(CONTEXT_FILE),
        serde_json::to_string_pretty(&context)?.as_bytes(),
    )?;
    let path = dir.join(DOCUMENT_FILE);
    let document = format!(
        "# Architecture contract (version {})\n{}",
        context.version,
        context.prompt_for(PromptAudience::IdeSession)?
    );
    super::ticket::write_private(&path, document.as_bytes())?;
    let _ = fs::remove_file(dir.join(PROBLEM_FILE));
    Ok((context, path))
}

fn context_text(context: &ArchitectureContext, path: &Path) -> String {
    let declared = if context.ticket_boundary_ids.is_empty() {
        "none".to_owned()
    } else {
        context.ticket_boundary_ids.join(", ")
    };
    format!(
        "Architecture: this project has an architecture contract (version {version}), which the session works under. Before planning any code change, read {path} and follow its plan check. The ticket declared these boundaries: {declared}.\nGuardrails (block: never do it; ask: get the user's approval first):\n{guardrails}\nProtected paths (read_only: never modify; ask: an architect must approve changes):\n{protected}\nArchitect decisions on this ticket's earlier escalations (approved: that crossing is allowed as the resolution says; rejected: follow the resolution instead):\n{decisions}\n\n",
        version = context.version,
        path = path.display(),
        guardrails = context.guardrail_lines(),
        protected = context.protected_path_lines(),
        decisions = context.decision_lines(),
    )
}

/// The contract the linked session works under, if the link wrote one.
fn load(dir: &Path) -> Option<ArchitectureContext> {
    let bytes = fs::read(dir.join(CONTEXT_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Raises the escalation a reply asks for, if it asks for one. A failure is
/// recorded in the ticket's directory instead of holding up the trace, and
/// RustGrid deduplicates retries of the same escalation.
pub(crate) fn escalate_from_reply(client: &RustGridClient, dir: &Path, run_id: &str, reply: &str) {
    let Some(signal) = escalation_from_feedback(reply) else {
        return;
    };
    let outcome = load(dir)
        .context("the session has no architecture contract to escalate against")
        .and_then(|context| context.agent_escalation(signal))
        .and_then(|verdict| match verdict {
            PlanVerdict::Escalate(request) => client
                .raise_architecture_escalation(run_id, &request)
                .map(|_| ()),
            PlanVerdict::WithinBoundaries | PlanVerdict::AwaitingEscalation(_) => Ok(()),
        });
    let problem = dir.join(PROBLEM_FILE);
    match outcome {
        Ok(()) => {
            let _ = fs::remove_file(problem);
        }
        Err(error) => {
            let _ = super::ticket::write_private(&problem, format!("{error:#}\n").as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn contract(run_id: &str) -> Value {
        json!({
            "version": 2,
            "document": {
                "schema_version": crate::architecture::SCHEMA_VERSION,
                "standard": "rustgrid",
                "boundaries": [
                    {"id": "core", "name": "Core", "paths": ["crates/core/**"], "access": "read_only"},
                    {"id": "http", "name": "HTTP", "paths": ["crates/http/**"], "may_depend_on": ["core"]}
                ],
                "rules": [],
                "decisions": []
            },
            "ticket_boundary_ids": ["http"],
            "standard": "rustgrid",
            "guardrails": [
                {"id": "scope.dependencies", "group": "scope", "statement": "Ask before adding dependencies.", "level": "ask", "locked": false},
                {"id": "secrets.commit", "group": "secrets", "statement": "Never commit secrets.", "level": "block", "locked": true}
            ],
            "protected_paths": [{"boundary_id": "core", "paths": ["crates/core/**"], "access": "read_only"}],
            "open_escalations": [],
            "escalation_path": format!("/api/v1/agent-runs/{run_id}/architecture-escalations"),
            "instructions": "Escalate instead of crossing a boundary."
        })
    }

    #[test]
    fn writes_the_contract_and_summarizes_its_guardrails() {
        let dir = tempfile::tempdir().unwrap();
        let text = prepare(dir.path(), "run-1", Some(&contract("run-1"))).unwrap();
        assert!(text.contains("architecture contract (version 2)"));
        assert!(text.contains(&dir.path().join(DOCUMENT_FILE).display().to_string()));
        assert!(text.contains("declared these boundaries: http"));
        let block = text.find("[block] Never commit secrets.").unwrap();
        let ask = text.find("[ask] Ask before adding dependencies.").unwrap();
        assert!(block < ask, "block guardrails come first: {text}");
        assert!(text.contains("[read_only] core: crates/core/**"));

        let document = fs::read_to_string(dir.path().join(DOCUMENT_FILE)).unwrap();
        assert!(document.contains("RUSTGRID_AGENT_STATUS: ESCALATE"));
        assert!(document.contains("RustGrid's session hook raises the escalation"));
        // Fields the worker does not model still reach the agent.
        assert!(document.contains("\"access\": \"read_only\""));
        assert!(document.contains("\"standard\": \"rustgrid\""));
        assert_eq!(load(dir.path()).unwrap().version, 2);
    }

    #[test]
    fn a_project_without_a_contract_clears_an_earlier_one() {
        let dir = tempfile::tempdir().unwrap();
        prepare(dir.path(), "run-1", Some(&contract("run-1"))).unwrap();
        assert!(prepare(dir.path(), "run-2", Some(&Value::Null)).is_none());
        assert!(prepare(dir.path(), "run-2", None).is_none());
        assert!(!dir.path().join(CONTEXT_FILE).exists());
        assert!(!dir.path().join(DOCUMENT_FILE).exists());
    }

    #[test]
    fn a_contract_for_another_run_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let text = prepare(dir.path(), "run-1", Some(&contract("run-9"))).unwrap();
        assert!(text.contains("could not be used"), "{text}");
        assert!(load(dir.path()).is_none());
    }
}
