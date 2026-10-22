use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};

pub const SCHEMA_VERSION: &str = "rustgrid.architecture.v1";
const MAX_CONTEXT_BYTES: usize = 256 * 1024;
const MAX_ESCALATION_TEXT: usize = 8_000;

/// Architecture contract delivered in the run manifest (`AgentArchitectureContext`).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArchitectureContext {
    pub version: i32,
    pub document: ArchitectureDocument,
    #[serde(default)]
    pub ticket_boundary_ids: Vec<String>,
    #[serde(default)]
    pub open_escalations: Vec<ArchitectureEscalation>,
    pub escalation_path: String,
    #[serde(default)]
    pub instructions: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArchitectureDocument {
    pub schema_version: String,
    #[serde(default)]
    pub boundaries: Vec<ArchitectureBoundary>,
    #[serde(default)]
    pub rules: Vec<ArchitectureRule>,
    #[serde(default)]
    pub decisions: Vec<ArchitectureDecision>,
    #[serde(default)]
    pub profile: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArchitectureBoundary {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub may_depend_on: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArchitectureRule {
    pub id: String,
    pub kind: String,
    pub statement: String,
    pub enforcement: String,
    #[serde(default)]
    pub applies_to: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArchitectureDecision {
    pub id: String,
    pub title: String,
    pub status: String,
    pub decision: String,
    #[serde(default)]
    pub rationale: String,
}

/// `ArchitectureEscalationResponse`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArchitectureEscalation {
    pub id: String,
    #[serde(default)]
    pub agent_run_id: Option<String>,
    pub architecture_version: i32,
    pub boundary_ids: Vec<String>,
    pub reason: String,
    pub source: String,
    pub status: String,
}

/// `RaiseArchitectureEscalationRequest` body produced by the plan check.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EscalationRequest {
    pub boundary_ids: Vec<String>,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed_change: Option<String>,
}

/// What a session's output or diff requires before the run may continue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanVerdict {
    WithinBoundaries,
    /// An open escalation already covers these boundaries; block without raising another.
    AwaitingEscalation(String),
    Escalate(EscalationRequest),
}

impl ArchitectureContext {
    pub fn validate(&self, run_id: &str) -> Result<()> {
        if self.document.schema_version != SCHEMA_VERSION {
            bail!(
                "unsupported architecture schema version {}",
                self.document.schema_version
            );
        }
        if self.version < 1 {
            bail!("architecture version must be at least 1");
        }
        if serde_json::to_vec(self)?.len() > MAX_CONTEXT_BYTES {
            bail!("architecture context exceeds {MAX_CONTEXT_BYTES} bytes");
        }
        let expected_path = format!("/api/v1/agent-runs/{run_id}/architecture-escalations");
        if self.escalation_path != expected_path {
            bail!("architecture escalation path does not belong to the claimed run");
        }
        let mut ids = HashSet::new();
        for boundary in &self.document.boundaries {
            if !valid_boundary_id(&boundary.id) || !ids.insert(boundary.id.as_str()) {
                bail!("architecture boundary IDs must be unique and match [A-Za-z0-9._-]{{1,64}}");
            }
            if boundary.paths.iter().any(|path| !valid_path_glob(path)) {
                bail!(
                    "architecture boundary {} contains an invalid path glob",
                    boundary.id
                );
            }
        }
        let references = self
            .document
            .boundaries
            .iter()
            .flat_map(|boundary| &boundary.may_depend_on)
            .chain(self.document.rules.iter().flat_map(|rule| &rule.applies_to))
            .chain(&self.ticket_boundary_ids)
            .chain(
                self.open_escalations
                    .iter()
                    .flat_map(|escalation| &escalation.boundary_ids),
            );
        for reference in references {
            if !ids.contains(reference.as_str()) {
                bail!("architecture references unknown boundary {reference}");
            }
        }
        Ok(())
    }

    fn boundary(&self, id: &str) -> Option<&ArchitectureBoundary> {
        self.document
            .boundaries
            .iter()
            .find(|boundary| boundary.id == id)
    }

    /// Boundaries owning `path`, in document order.
    pub fn owners(&self, path: &str) -> Vec<&str> {
        self.document
            .boundaries
            .iter()
            .filter(|boundary| boundary.paths.iter().any(|glob| glob_matches(glob, path)))
            .map(|boundary| boundary.id.as_str())
            .collect()
    }

    /// Checks changed paths against the boundaries the ticket declared. A path is
    /// in scope when it belongs to no boundary or to at least one declared one.
    /// Without declared boundaries the ticket has no path scope to enforce.
    pub fn check_paths<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> PlanVerdict {
        if self.ticket_boundary_ids.is_empty() {
            return PlanVerdict::WithinBoundaries;
        }
        let declared = self
            .ticket_boundary_ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let mut crossings = BTreeMap::<&str, Vec<&str>>::new();
        for path in paths {
            let owners = self.owners(path);
            if owners.is_empty() || owners.iter().any(|owner| declared.contains(owner)) {
                continue;
            }
            for owner in owners {
                crossings.entry(owner).or_default().push(path);
            }
        }
        if crossings.is_empty() {
            return PlanVerdict::WithinBoundaries;
        }
        let detail = crossings
            .iter()
            .map(|(boundary, paths)| format!("{boundary}: {}", paths.join(", ")))
            .collect::<Vec<_>>()
            .join("; ");
        self.verdict(EscalationRequest {
            boundary_ids: crossings.keys().map(|id| (*id).to_owned()).collect(),
            reason: truncate(
                &format!(
                    "The agent's changes leave the ticket's declared boundaries ({}). Changed paths outside them: {detail}",
                    self.ticket_boundary_ids.join(", ")
                ),
                MAX_ESCALATION_TEXT,
            ),
            proposed_change: None,
        })
    }

    /// Validates an escalation the agent requested and folds it into a verdict.
    pub fn agent_escalation(&self, signal: AgentEscalationSignal) -> Result<PlanVerdict> {
        let unknown = signal
            .boundary_ids
            .iter()
            .filter(|id| self.boundary(id).is_none())
            .cloned()
            .collect::<Vec<_>>();
        if signal.boundary_ids.is_empty() || !unknown.is_empty() {
            bail!(
                "The coding agent requested an architecture escalation without valid boundary IDs (unknown: {})",
                if unknown.is_empty() {
                    "none given".to_owned()
                } else {
                    unknown.join(", ")
                }
            );
        }
        Ok(self.verdict(EscalationRequest {
            boundary_ids: signal.boundary_ids,
            reason: truncate(&signal.reason, MAX_ESCALATION_TEXT),
            proposed_change: signal
                .proposed_change
                .map(|change| truncate(&change, MAX_ESCALATION_TEXT)),
        }))
    }

    fn verdict(&self, request: EscalationRequest) -> PlanVerdict {
        let requested = request.boundary_ids.iter().collect::<BTreeSet<_>>();
        let existing = self.open_escalations.iter().find(|escalation| {
            escalation.status == "open"
                && requested.is_subset(&escalation.boundary_ids.iter().collect())
        });
        match existing {
            Some(escalation) => PlanVerdict::AwaitingEscalation(escalation.id.clone()),
            None => PlanVerdict::Escalate(request),
        }
    }

    pub fn prompt_section(&self) -> Result<String> {
        let declared = if self.ticket_boundary_ids.is_empty() {
            "(none declared; stay consistent with every boundary's dependency rules)".to_owned()
        } else {
            self.ticket_boundary_ids.join(", ")
        };
        let open = if self.open_escalations.is_empty() {
            "(none)".to_owned()
        } else {
            self.open_escalations
                .iter()
                .map(|escalation| {
                    format!(
                        "- {} [{}] boundaries {}: {}",
                        escalation.id,
                        escalation.status,
                        escalation.boundary_ids.join(", "),
                        escalation.reason
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(format!(
            r#"
Project architecture contract (version {version}):
```json
{document}
```

Architect instructions:
{instructions}

Boundaries this ticket declared: {declared}

Open architecture escalations (do not make these crossings while they are pending):
{open}

Architecture plan check. Before editing any file, plan the change: list each file you intend to create or modify, the boundary that owns it (from the boundary `paths` globs), and every new dependency between boundaries. Check that plan against the contract:
1. Every changed file belongs to a declared boundary or to no boundary.
2. Every new cross-boundary dependency is listed in the depending boundary's `may_depend_on`.
3. No rule or accepted decision is contradicted.
If the plan passes, implement it and keep the implementation inside it. If any check fails and no compliant implementation exists, do not make the crossing and do not edit files for it. Instead end your final update with exactly:
RUSTGRID_AGENT_STATUS: ESCALATE
RUSTGRID_ESCALATION_BOUNDARIES: <comma-separated boundary IDs the work needs to cross>
RUSTGRID_ESCALATION_REASON: <one line: why the plan cannot stay inside the declared boundaries>
RUSTGRID_PROPOSED_CHANGE: <one line: what the change would look like if approved>
The worker raises the escalation for an architect and independently checks the final diff against the declared boundaries.
"#,
            version = self.version,
            document = serde_json::to_string_pretty(&self.document)?,
            instructions = if self.instructions.trim().is_empty() {
                "(none)"
            } else {
                self.instructions.trim()
            },
        ))
    }
}

/// Escalation fields the coding agent reported in its final update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentEscalationSignal {
    pub boundary_ids: Vec<String>,
    pub reason: String,
    pub proposed_change: Option<String>,
}

pub fn escalation_from_feedback(message: &str) -> Option<AgentEscalationSignal> {
    let mut status = None;
    let mut boundaries = None;
    let mut reason = None;
    let mut proposed = None;
    for line in message.lines() {
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match label.trim().to_ascii_uppercase().as_str() {
            "RUSTGRID_AGENT_STATUS" => status = Some(value.to_ascii_uppercase()),
            "RUSTGRID_ESCALATION_BOUNDARIES" => boundaries = Some(value),
            "RUSTGRID_ESCALATION_REASON" => reason = Some(value),
            "RUSTGRID_PROPOSED_CHANGE" => proposed = Some(value),
            _ => {}
        }
    }
    if status.as_deref() != Some("ESCALATE") {
        return None;
    }
    Some(AgentEscalationSignal {
        boundary_ids: boundaries
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect(),
        reason: reason
            .filter(|reason| !reason.is_empty())
            .unwrap_or("The coding agent reported that the plan cannot stay inside the declared boundaries")
            .to_owned(),
        proposed_change: proposed
            .filter(|change| !change.is_empty())
            .map(str::to_owned),
    })
}

fn valid_boundary_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_path_glob(glob: &str) -> bool {
    !glob.trim().is_empty()
        && !glob.starts_with('/')
        && !glob.contains('\\')
        && glob.split('/').all(|segment| segment != "..")
}

/// Matches repository-relative paths against `*`, `?`, and `**` globs. A trailing
/// `/` matches everything below the directory.
pub fn glob_matches(glob: &str, path: &str) -> bool {
    let glob = glob.strip_prefix("./").unwrap_or(glob);
    let owned;
    let glob = if let Some(directory) = glob.strip_suffix('/') {
        owned = format!("{directory}/**");
        owned.as_str()
    } else {
        glob
    };
    let pattern = glob.split('/').collect::<Vec<_>>();
    let segments = path.split('/').collect::<Vec<_>>();
    match_segments(&pattern, &segments)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| match_segments(rest, &path[skip..])),
        Some((first, rest)) => path.split_first().is_some_and(|(segment, tail)| {
            match_segment(first.as_bytes(), segment.as_bytes()) && match_segments(rest, tail)
        }),
    }
}

fn match_segment(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| match_segment(rest, &text[skip..])),
        Some((b'?', rest)) => !text.is_empty() && match_segment(rest, &text[1..]),
        Some((byte, rest)) => text.first() == Some(byte) && match_segment(rest, &text[1..]),
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ArchitectureContext {
        serde_json::from_value(serde_json::json!({
            "version": 3,
            "document": {
                "schema_version": SCHEMA_VERSION,
                "boundaries": [
                    {"id": "api", "name": "API", "paths": ["crates/api/**"], "may_depend_on": ["security-authz"]},
                    {"id": "security-authz", "name": "Authorization", "paths": ["crates/ticket-service/src/security/**"]},
                    {"id": "storage", "name": "Storage", "paths": ["crates/storage/", "migrations/*.sql"]}
                ],
                "rules": [{"id": "authz-location", "kind": "cross_cutting", "statement": "Authorization checks live in security::authz.", "enforcement": "agent_check", "applies_to": ["api"]}],
                "decisions": [],
                "profile": {"style": "modular_monolith"}
            },
            "ticket_boundary_ids": ["api"],
            "open_escalations": [],
            "escalation_path": "/api/v1/agent-runs/run-1/architecture-escalations",
            "instructions": "Escalate instead of crossing a boundary."
        }))
        .unwrap()
    }

    #[test]
    fn validates_contract_references_and_run_scoped_path() {
        let valid = context();
        assert!(valid.validate("run-1").is_ok());
        assert!(valid.validate("run-2").is_err());

        let mut unknown = context();
        unknown.ticket_boundary_ids = vec!["billing".into()];
        assert!(unknown.validate("run-1").is_err());

        let mut duplicate = context();
        duplicate
            .document
            .boundaries
            .push(duplicate.document.boundaries[0].clone());
        assert!(duplicate.validate("run-1").is_err());

        let mut escaping = context();
        escaping.document.boundaries[0].paths = vec!["../outside/**".into()];
        assert!(escaping.validate("run-1").is_err());

        let mut schema = context();
        schema.document.schema_version = "rustgrid.architecture.v2".into();
        assert!(schema.validate("run-1").is_err());
    }

    #[test]
    fn matches_path_globs() {
        assert!(glob_matches("crates/api/**", "crates/api/src/lib.rs"));
        assert!(glob_matches("crates/api/**", "crates/api"));
        assert!(glob_matches("crates/storage/", "crates/storage/src/db.rs"));
        assert!(glob_matches("migrations/*.sql", "migrations/001_init.sql"));
        assert!(!glob_matches(
            "migrations/*.sql",
            "migrations/nested/001.sql"
        ));
        assert!(glob_matches("**/*.proto", "a/b/c.proto"));
        assert!(glob_matches("src/?.rs", "src/a.rs"));
        assert!(!glob_matches("crates/api/**", "crates/apix/src/lib.rs"));
    }

    #[test]
    fn diff_inside_declared_or_unowned_paths_passes() {
        let context = context();
        assert_eq!(
            context.check_paths(["crates/api/src/routes.rs", "README.md"]),
            PlanVerdict::WithinBoundaries
        );
    }

    #[test]
    fn diff_crossing_undeclared_boundaries_escalates() {
        let PlanVerdict::Escalate(request) = context().check_paths([
            "crates/api/src/routes.rs",
            "crates/ticket-service/src/security/authz.rs",
            "migrations/002_roles.sql",
        ]) else {
            panic!("expected an escalation");
        };
        assert_eq!(request.boundary_ids, ["security-authz", "storage"]);
        assert!(request.reason.contains("migrations/002_roles.sql"));
    }

    #[test]
    fn undeclared_ticket_scope_does_not_restrict_paths() {
        let mut context = context();
        context.ticket_boundary_ids.clear();
        assert_eq!(
            context.check_paths(["migrations/002_roles.sql"]),
            PlanVerdict::WithinBoundaries
        );
    }

    #[test]
    fn open_escalation_for_same_boundaries_is_not_raised_again() {
        let mut context = context();
        context.open_escalations.push(ArchitectureEscalation {
            id: "esc-1".into(),
            agent_run_id: Some("run-0".into()),
            architecture_version: 3,
            boundary_ids: vec!["storage".into(), "security-authz".into()],
            reason: "needs a roles table".into(),
            source: "agent".into(),
            status: "open".into(),
        });
        assert_eq!(
            context.check_paths(["migrations/002_roles.sql"]),
            PlanVerdict::AwaitingEscalation("esc-1".into())
        );
    }

    #[test]
    fn parses_agent_escalation_signal() {
        let message = "Plan crosses storage.\nRUSTGRID_AGENT_STATUS: ESCALATE\nRUSTGRID_ESCALATION_BOUNDARIES: storage, security-authz\nRUSTGRID_ESCALATION_REASON: Roles need a new table.\nRUSTGRID_PROPOSED_CHANGE: Add migrations/002_roles.sql.";
        let signal = escalation_from_feedback(message).unwrap();
        assert_eq!(signal.boundary_ids, ["storage", "security-authz"]);
        assert_eq!(signal.reason, "Roles need a new table.");
        assert_eq!(
            signal.proposed_change.as_deref(),
            Some("Add migrations/002_roles.sql.")
        );
        assert!(escalation_from_feedback("RUSTGRID_AGENT_STATUS: COMPLETED").is_none());

        assert!(matches!(
            context().agent_escalation(signal),
            Ok(PlanVerdict::Escalate(_))
        ));
        let unknown = escalation_from_feedback(
            "RUSTGRID_AGENT_STATUS: ESCALATE\nRUSTGRID_ESCALATION_BOUNDARIES: billing",
        )
        .unwrap();
        assert!(context().agent_escalation(unknown).is_err());
    }

    #[test]
    fn prompt_section_carries_contract_and_escalation_protocol() {
        let section = context().prompt_section().unwrap();
        assert!(section.contains("Boundaries this ticket declared: api"));
        assert!(section.contains("security::authz"));
        assert!(section.contains("RUSTGRID_AGENT_STATUS: ESCALATE"));
    }
}
