use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{
    api::Ticket,
    manifest::{ExecutionManifest, QualityGatePolicy},
};

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
            Self::Metadata => {
                MissionBudget::new(5_000, 2, 3, 30_000, 10_000, 50_000, 4_000, 60_000)
            }
            Self::Configuration => {
                MissionBudget::new(10_000, 8, 12, 60_000, 30_000, 200_000, 8_000, 240_000)
            }
            Self::SingleFile => {
                MissionBudget::new(18_000, 14, 24, 100_000, 70_000, 450_000, 16_000, 480_000)
            }
            Self::MultiFile => {
                MissionBudget::new(40_000, 24, 60, 160_000, 160_000, 900_000, 32_000, 900_000)
            }
            Self::RepositoryWide => MissionBudget::new(
                80_000, 40, 120, 200_000, 320_000, 1_800_000, 64_000, 1_800_000,
            ),
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MissionBudget {
    pub max_initial_prompt_tokens: u64,
    pub max_inference_turns: u32,
    pub max_tool_calls: u32,
    pub max_peak_context_tokens: u64,
    pub max_cumulative_uncached_input_tokens: u64,
    pub max_cumulative_cached_input_tokens: u64,
    pub max_output_tokens: u64,
    pub max_codex_duration_ms: u64,
}

impl MissionBudget {
    #[allow(clippy::too_many_arguments)]
    const fn new(
        max_initial_prompt_tokens: u64,
        max_inference_turns: u32,
        max_tool_calls: u32,
        max_peak_context_tokens: u64,
        max_cumulative_uncached_input_tokens: u64,
        max_cumulative_cached_input_tokens: u64,
        max_output_tokens: u64,
        max_codex_duration_ms: u64,
    ) -> Self {
        Self {
            max_initial_prompt_tokens,
            max_inference_turns,
            max_tool_calls,
            max_peak_context_tokens,
            max_cumulative_uncached_input_tokens,
            max_cumulative_cached_input_tokens,
            max_output_tokens,
            max_codex_duration_ms,
        }
    }

    fn override_from(self, metadata: &serde_json::Value) -> Self {
        let Some(value) = metadata.get("execution_budget") else {
            return self;
        };
        let mut budget = self;
        macro_rules! override_field {
            ($field:ident, $ty:ty) => {
                if let Some(value) = value
                    .get(stringify!($field))
                    .and_then(serde_json::Value::as_u64)
                    .filter(|value| *value > 0)
                {
                    match <$ty>::try_from(value) {
                        Ok(converted) => budget.$field = converted,
                        Err(_) => {}
                    }
                }
            };
        }
        override_field!(max_initial_prompt_tokens, u64);
        override_field!(max_inference_turns, u32);
        override_field!(max_tool_calls, u32);
        override_field!(max_peak_context_tokens, u64);
        override_field!(max_cumulative_uncached_input_tokens, u64);
        override_field!(max_cumulative_cached_input_tokens, u64);
        override_field!(max_output_tokens, u64);
        override_field!(max_codex_duration_ms, u64);
        budget
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetStage {
    Normal,
    Constrained,
    FinalizationRequired,
    HardLimit,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct BudgetUsage {
    pub initial_prompt_tokens: u64,
    pub inference_turns: u32,
    pub tool_calls: u32,
    pub peak_context_tokens: u64,
    pub cumulative_uncached_input_tokens: u64,
    pub cumulative_cached_input_tokens: u64,
    pub output_tokens: u64,
    pub codex_duration_ms: u64,
}

impl BudgetUsage {
    pub fn combined_with(self, other: Self) -> Self {
        Self {
            initial_prompt_tokens: self.initial_prompt_tokens.max(other.initial_prompt_tokens),
            inference_turns: self.inference_turns.saturating_add(other.inference_turns),
            tool_calls: self.tool_calls.saturating_add(other.tool_calls),
            peak_context_tokens: self.peak_context_tokens.max(other.peak_context_tokens),
            cumulative_uncached_input_tokens: self
                .cumulative_uncached_input_tokens
                .saturating_add(other.cumulative_uncached_input_tokens),
            cumulative_cached_input_tokens: self
                .cumulative_cached_input_tokens
                .saturating_add(other.cumulative_cached_input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            codex_duration_ms: self
                .codex_duration_ms
                .saturating_add(other.codex_duration_ms),
        }
    }

    pub fn stage(self, budget: MissionBudget) -> BudgetStage {
        let utilization = [
            ratio(self.initial_prompt_tokens, budget.max_initial_prompt_tokens),
            ratio(
                u64::from(self.inference_turns),
                u64::from(budget.max_inference_turns),
            ),
            ratio(u64::from(self.tool_calls), u64::from(budget.max_tool_calls)),
            ratio(self.peak_context_tokens, budget.max_peak_context_tokens),
            ratio(
                self.cumulative_uncached_input_tokens,
                budget.max_cumulative_uncached_input_tokens,
            ),
            ratio(
                self.cumulative_cached_input_tokens,
                budget.max_cumulative_cached_input_tokens,
            ),
            ratio(self.output_tokens, budget.max_output_tokens),
            ratio(self.codex_duration_ms, budget.max_codex_duration_ms),
        ]
        .into_iter()
        .fold(0.0_f64, f64::max);
        if utilization >= 1.0 {
            BudgetStage::HardLimit
        } else if utilization >= 0.9 {
            BudgetStage::FinalizationRequired
        } else if utilization >= 0.7 {
            BudgetStage::Constrained
        } else {
            BudgetStage::Normal
        }
    }
}

fn ratio(value: u64, limit: u64) -> f64 {
    value as f64 / limit.max(1) as f64
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExecutionOwnership {
    pub codex_owned: CodexOwnership,
    pub worker_owned: WorkerOwnership,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CodexOwnership {
    pub discovery: bool,
    pub implementation: bool,
    pub focused_validation: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkerOwnership {
    pub dependency_bootstrap: bool,
    pub full_tests: bool,
    pub lint: bool,
    pub type_check: bool,
    pub build: bool,
    pub commit: bool,
    pub push: bool,
    pub pull_request: bool,
    pub github_checks: bool,
}

impl Default for ExecutionOwnership {
    fn default() -> Self {
        Self {
            codex_owned: CodexOwnership {
                discovery: true,
                implementation: true,
                focused_validation: true,
            },
            worker_owned: WorkerOwnership {
                dependency_bootstrap: true,
                full_tests: true,
                lint: true,
                type_check: true,
                build: true,
                commit: true,
                push: true,
                pull_request: true,
                github_checks: true,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ValidationPlan {
    pub focused_candidates: Vec<String>,
    pub worker_gates: Vec<String>,
    pub codex_instructions: String,
}

impl ValidationPlan {
    pub fn build(class: MissionClass, root: &Path, gates: &[QualityGatePolicy]) -> Self {
        let mut focused_candidates = vec![
            "Run one directly relevant unit or component test file".into(),
            "Search changed identifiers for stale references".into(),
        ];
        if root.join("package.json").is_file() {
            focused_candidates
                .push("Use the repository-local test binary with a file filter".into());
        }
        if root.join("Cargo.toml").is_file() {
            focused_candidates.push("Run one package, module, or named Rust test".into());
        }
        if matches!(class, MissionClass::Configuration) {
            focused_candidates.push("Inspect one representative neighboring test".into());
        }
        let worker_gates = gates
            .iter()
            .map(|gate| gate.command.clone())
            .collect::<Vec<_>>();
        let codex_instructions = format!(
            "Use only the smallest focused validation needed. Do not reinstall dependencies or run full repository tests, lint, type-check, or builds. The worker will run {} authoritative gate(s) after you finish.",
            worker_gates.len()
        );
        Self {
            focused_candidates,
            worker_gates,
            codex_instructions,
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
    pub budget: MissionBudget,
}

impl MissionProfile {
    pub fn classify_after_checkout(
        ticket: &Ticket,
        manifest: &ExecutionManifest,
        repo_root: &Path,
    ) -> Self {
        if manifest.run.metadata.get("direct_operation").is_some() {
            let class = MissionClass::Metadata;
            return Self {
                class: MissionClass::Metadata,
                reason: "validated direct operation".into(),
                explicit: true,
                budget: class.budget().override_from(&manifest.run.metadata),
            };
        }
        if let Some(class) = explicit_class(&manifest.run.metadata) {
            return Self {
                class,
                reason: "execution manifest override evaluated after checkout".into(),
                explicit: true,
                budget: class.budget().override_from(&manifest.run.metadata),
            };
        }

        let objective = format!(
            "{}\n{}\n{}",
            ticket.title,
            ticket.description.as_deref().unwrap_or_default(),
            manifest.run.input_prompt
        )
        .to_ascii_lowercase();
        let sensitive_small_task = contains_any(
            &objective,
            &[
                "generated",
                "localization",
                "i18n",
                "security",
                "billing",
                "cross-platform",
            ],
        );
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
        } else if sensitive_small_task {
            MissionClass::MultiFile
        } else if contains_any(
            &objective,
            &[
                "cargo.toml",
                "package.json",
                "tsconfig",
                "workflow",
                "configuration",
                "config file",
                "rename ",
                "label",
                "replace ",
                "change the text",
                "menu title",
                "navigation label",
                "copy change",
                "typo",
                "feature flag",
                "route metadata",
                "styling token",
            ],
        ) {
            MissionClass::Configuration
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
            budget: class.budget().override_from(&manifest.run.metadata),
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
    fn classifies_the_observed_navigation_rename_as_configuration() {
        let profile = MissionProfile::classify_after_checkout(
            &ticket("Replace Live Fleet menu title to Live Agents"),
            &manifest(json!({})),
            Path::new("."),
        );
        assert_eq!(profile.class, MissionClass::Configuration);
        assert!(profile.reason.contains("post-checkout"));
        assert_eq!(profile.budget.max_initial_prompt_tokens, 10_000);
        assert_eq!(profile.budget.max_inference_turns, 8);
        assert_eq!(profile.budget.max_tool_calls, 12);
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

    #[test]
    fn configuration_budget_intervenes_before_exhaustion() {
        let budget = MissionClass::Configuration.budget();
        assert_eq!(
            BudgetUsage {
                inference_turns: 6,
                ..BudgetUsage::default()
            }
            .stage(budget),
            BudgetStage::Constrained
        );
        assert_eq!(
            BudgetUsage {
                tool_calls: 11,
                ..BudgetUsage::default()
            }
            .stage(budget),
            BudgetStage::FinalizationRequired
        );
        assert_eq!(
            BudgetUsage {
                cumulative_uncached_input_tokens: 30_000,
                ..BudgetUsage::default()
            }
            .stage(budget),
            BudgetStage::HardLimit
        );
    }

    #[test]
    fn manifest_can_tighten_or_expand_each_budget_dimension() {
        let profile = MissionProfile::classify_after_checkout(
            &ticket("Change a feature flag"),
            &manifest(json!({
                "execution_budget": {
                    "max_inference_turns": 5,
                    "max_tool_calls": 9,
                    "max_codex_duration_ms": 120000
                }
            })),
            Path::new("."),
        );
        assert_eq!(profile.budget.max_inference_turns, 5);
        assert_eq!(profile.budget.max_tool_calls, 9);
        assert_eq!(profile.budget.max_codex_duration_ms, 120_000);
    }

    #[test]
    fn validation_plan_keeps_full_gates_worker_owned() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("package.json"), "{}").unwrap();
        let plan = ValidationPlan::build(
            MissionClass::Configuration,
            directory.path(),
            &[QualityGatePolicy {
                id: "full".into(),
                command: "npm test && npm run build".into(),
                timeout_seconds: 300,
                required: true,
            }],
        );
        assert_eq!(plan.worker_gates, ["npm test && npm run build"]);
        assert!(
            plan.codex_instructions
                .contains("smallest focused validation")
        );
        assert!(
            plan.codex_instructions
                .contains("Do not reinstall dependencies")
        );
        assert!(ExecutionOwnership::default().worker_owned.full_tests);
        assert!(ExecutionOwnership::default().codex_owned.focused_validation);
    }
}
