use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{api::Ticket, manifest::ExecutionManifest};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionClass {
    Metadata,
    Configuration,
    SingleFile,
    MultiFile,
    RepositoryWide,
}

impl MissionClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Configuration => "configuration",
            Self::SingleFile => "single_file",
            Self::MultiFile => "multi_file",
            Self::RepositoryWide => "repository_wide",
        }
    }

    pub const fn budget(self) -> MissionBudget {
        match self {
            Self::Metadata => MissionBudget::new(5_000, 1, 3),
            Self::Configuration => MissionBudget::new(10_000, 2, 6),
            Self::SingleFile => MissionBudget::new(25_000, 4, 12),
            Self::MultiFile => MissionBudget::new(80_000, 12, 40),
            Self::RepositoryWide => MissionBudget::new(200_000, 32, 120),
        }
    }

    pub const fn tool_bundles(self) -> &'static [ToolBundle] {
        match self {
            Self::Metadata => &[ToolBundle::Metadata],
            Self::Configuration | Self::SingleFile => &[
                ToolBundle::CodeRead,
                ToolBundle::CodeWrite,
                ToolBundle::Delivery,
            ],
            Self::MultiFile | Self::RepositoryWide => &[
                ToolBundle::CodeRead,
                ToolBundle::CodeWrite,
                ToolBundle::Delivery,
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct MissionBudget {
    pub max_input_tokens: u64,
    pub max_model_calls: u32,
    pub max_tool_calls: u32,
}

impl MissionBudget {
    const fn new(max_input_tokens: u64, max_model_calls: u32, max_tool_calls: u32) -> Self {
        Self {
            max_input_tokens,
            max_model_calls,
            max_tool_calls,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolBundle {
    Metadata,
    CodeRead,
    CodeWrite,
    Delivery,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissionProfile {
    pub class: MissionClass,
    pub reason: String,
    pub explicit: bool,
}

impl MissionProfile {
    pub fn classify_after_checkout(
        ticket: &Ticket,
        manifest: &ExecutionManifest,
        repo_root: &Path,
    ) -> Self {
        if manifest.run.metadata.get("direct_operation").is_some() {
            return Self {
                class: MissionClass::Metadata,
                reason: "validated direct operation".into(),
                explicit: true,
            };
        }
        if let Some(class) = explicit_class(&manifest.run.metadata) {
            return Self {
                class,
                reason: "execution manifest override evaluated after checkout".into(),
                explicit: true,
            };
        }

        let objective = format!(
            "{}\n{}\n{}",
            ticket.title,
            ticket.description.as_deref().unwrap_or_default(),
            manifest.run.input_prompt
        )
        .to_ascii_lowercase();
        let class = if contains_any(
            &objective,
            &[
                "entire repository",
                "repo-wide",
                "repository-wide",
                "everywhere",
                "all modules",
            ],
        ) {
            MissionClass::RepositoryWide
        } else if contains_any(
            &objective,
            &[
                "cargo.toml",
                "package.json",
                "tsconfig",
                "workflow",
                "configuration",
                "config file",
            ],
        ) {
            MissionClass::Configuration
        } else if contains_any(
            &objective,
            &[
                "rename ",
                "replace ",
                "change the text",
                "menu title",
                "navigation label",
                "copy change",
                "typo",
            ],
        ) {
            MissionClass::SingleFile
        } else {
            MissionClass::MultiFile
        };
        let markers = ["Cargo.toml", "package.json", "go.mod", "pyproject.toml"]
            .into_iter()
            .filter(|name| repo_root.join(name).is_file())
            .collect::<Vec<_>>();
        Self {
            class,
            reason: format!(
                "advisory post-checkout objective analysis; repository markers: {}",
                if markers.is_empty() {
                    "none".into()
                } else {
                    markers.join(", ")
                }
            ),
            explicit: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DirectOperation {
    SetStatus { status: String },
    AddComment { body: String },
}

pub fn direct_operation(manifest: &ExecutionManifest) -> anyhow::Result<Option<DirectOperation>> {
    manifest
        .run
        .metadata
        .get("direct_operation")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(Into::into)
}

fn explicit_class(metadata: &serde_json::Value) -> Option<MissionClass> {
    metadata
        .get("mission_class")
        .or_else(|| metadata.pointer("/execution_profile/mission_class"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn contains_any(value: &str, candidates: &[&str]) -> bool {
    candidates.iter().any(|candidate| value.contains(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ticket(title: &str) -> Ticket {
        Ticket {
            id: "ticket-1".into(),
            key: "RG-1".into(),
            title: title.into(),
            description: None,
            comments: vec![],
            custom_fields: json!({}),
            previous_quality_gate_failures: vec![],
            row_version: 1,
        }
    }

    fn manifest(metadata: serde_json::Value) -> ExecutionManifest {
        ExecutionManifest {
            manifest_version: 2,
            run: crate::manifest::ManifestRun {
                id: "run-1".into(),
                ticket_id: "ticket-1".into(),
                input_prompt: "Execute the assigned ticket.".into(),
                attempt: 1,
                metadata,
            },
            attachments: vec![],
            project_id: "project-1".into(),
            project_key: "RG".into(),
            project_name: "RustGrid".into(),
            ticket_id: "ticket-1".into(),
            ticket_key: "RG-1".into(),
            ticket_title: "Task".into(),
            repository_id: 1,
            repository: "RustGrid/example".into(),
            clone_url: "https://github.com/RustGrid/example.git".into(),
            web_base_url: "https://github.com".into(),
            installation_id: 1,
            default_branch: Some("main".into()),
            required_workflows: vec![],
            required_permissions: json!({}),
            execution_policy: json!({}),
            execution_policy_sha256: String::new(),
        }
    }

    #[test]
    fn classifies_the_observed_navigation_rename_as_single_file() {
        let profile = MissionProfile::classify_after_checkout(
            &ticket("Replace Live Fleet menu title to Live Agents"),
            &manifest(json!({})),
            Path::new("."),
        );
        assert_eq!(profile.class, MissionClass::SingleFile);
        assert!(profile.reason.contains("post-checkout"));
        assert_eq!(profile.class.budget().max_input_tokens, 25_000);
        assert_eq!(
            profile.class.tool_bundles(),
            [
                ToolBundle::CodeRead,
                ToolBundle::CodeWrite,
                ToolBundle::Delivery
            ]
        );
    }

    #[test]
    fn explicit_manifest_classification_wins_and_is_auditable() {
        let profile = MissionProfile::classify_after_checkout(
            &ticket("Change several things"),
            &manifest(json!({"mission_class": "configuration"})),
            Path::new("."),
        );
        assert_eq!(profile.class, MissionClass::Configuration);
        assert!(profile.explicit);
    }

    #[test]
    fn validated_status_operation_uses_metadata_path_without_code_tools() {
        let manifest = manifest(json!({
            "direct_operation": {"type": "set_status", "status": "done"}
        }));
        let profile = MissionProfile::classify_after_checkout(
            &ticket("Mark this done"),
            &manifest,
            Path::new("."),
        );
        assert_eq!(profile.class, MissionClass::Metadata);
        assert_eq!(profile.class.tool_bundles(), [ToolBundle::Metadata]);
        assert_eq!(
            direct_operation(&manifest).unwrap(),
            Some(DirectOperation::SetStatus {
                status: "done".into()
            })
        );
    }
}
