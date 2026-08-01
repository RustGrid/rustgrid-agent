//! Canonical, serializable state for deterministic hosted execution.
//!
//! This module deliberately contains no repository, model, command, persistence,
//! or publication I/O.  It is the domain boundary shared by the hosted
//! orchestrator, its adapters, and the deterministic replay harness.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use crate::lifecycle::HostedExecutionStage;

pub const EXECUTION_GRAPH_SCHEMA_VERSION: u16 = 1;

mod duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let millis = u64::try_from(value.as_millis()).unwrap_or(u64::MAX);
        serializer.serialize_u64(millis)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        u64::deserialize(deserializer).map(Duration::from_millis)
    }
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(
            Clone, Debug, Default, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
    };
}

string_id!(ExecutionNodeId);
string_id!(FailureId);

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionComplexity {
    #[default]
    Tiny,
    Small,
    Medium,
    Large,
}

impl MissionComplexity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tiny => "tiny",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    pub fn default_budget(self) -> MissionBudget {
        MissionBudget::for_complexity(self)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct MissionBudget {
    pub max_model_calls: u32,
    pub max_cost_micros: u64,
    #[serde(with = "duration_millis")]
    pub max_duration: Duration,
    pub max_target_repair_rounds: u32,
}

impl MissionBudget {
    pub const TINY_MAX_COST_MICROS: u64 = 2_000_000;
    pub const SMALL_MAX_COST_MICROS: u64 = 5_000_000;
    pub const MEDIUM_MAX_COST_MICROS: u64 = 10_000_000;
    pub const LARGE_MAX_COST_MICROS: u64 = 20_000_000;

    pub fn for_complexity(complexity: MissionComplexity) -> Self {
        match complexity {
            MissionComplexity::Tiny => Self {
                max_model_calls: 14,
                max_cost_micros: Self::TINY_MAX_COST_MICROS,
                max_duration: Duration::from_secs(8 * 60),
                max_target_repair_rounds: 1,
            },
            MissionComplexity::Small => Self {
                max_model_calls: 25,
                max_cost_micros: Self::SMALL_MAX_COST_MICROS,
                max_duration: Duration::from_secs(15 * 60),
                max_target_repair_rounds: 2,
            },
            MissionComplexity::Medium => Self {
                max_model_calls: 45,
                max_cost_micros: Self::MEDIUM_MAX_COST_MICROS,
                max_duration: Duration::from_secs(35 * 60),
                max_target_repair_rounds: 3,
            },
            MissionComplexity::Large => Self {
                max_model_calls: 80,
                max_cost_micros: Self::LARGE_MAX_COST_MICROS,
                max_duration: Duration::from_secs(75 * 60),
                max_target_repair_rounds: 4,
            },
        }
    }

    pub fn applying_override(&self, policy: &MissionBudgetOverride) -> Self {
        Self {
            max_model_calls: policy.max_model_calls.unwrap_or(self.max_model_calls),
            max_cost_micros: policy.max_cost_micros.unwrap_or(self.max_cost_micros),
            max_duration: policy.max_duration.unwrap_or(self.max_duration),
            max_target_repair_rounds: policy
                .max_target_repair_rounds
                .unwrap_or(self.max_target_repair_rounds),
        }
    }
}

impl Default for MissionBudget {
    fn default() -> Self {
        Self::for_complexity(MissionComplexity::Tiny)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct MissionBudgetOverride {
    pub max_model_calls: Option<u32>,
    pub max_cost_micros: Option<u64>,
    #[serde(default, with = "optional_duration_millis")]
    pub max_duration: Option<Duration>,
    pub max_target_repair_rounds: Option<u32>,
}

mod optional_duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<u64>::deserialize(deserializer).map(|value| value.map(Duration::from_millis))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplexityFactorKind {
    PlannedTargetCount,
    NewFileCount,
    RepositoryCount,
    DependencyChanges,
    DatabaseSchemaChanges,
    ExternalIntegrations,
    SecuritySensitiveChanges,
    ArchitecturalUncertainty,
    TestSurface,
    ExpectedValidationDuration,
    CrossModuleImpact,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ComplexityFactor {
    pub kind: ComplexityFactorKind,
    pub value: u64,
    pub score: u32,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ComplexityInput {
    pub planned_target_count: u32,
    pub new_file_count: u32,
    pub repository_count: u32,
    pub dependency_change_count: u32,
    pub database_schema_change_count: u32,
    pub external_integration_count: u32,
    pub security_sensitive_change_count: u32,
    pub architectural_uncertainty: u32,
    pub test_surface: u32,
    #[serde(with = "duration_millis")]
    pub expected_validation_duration: Duration,
    pub cross_module_impact: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ComplexityAssessment {
    pub class: MissionComplexity,
    pub score: u32,
    pub factors: Vec<ComplexityFactor>,
    pub budget: MissionBudget,
}

impl ComplexityAssessment {
    pub fn classify(input: &ComplexityInput) -> Self {
        classify_complexity(input, None)
    }

    pub fn classify_with_policy(input: &ComplexityInput, policy: &MissionBudgetOverride) -> Self {
        classify_complexity(input, Some(policy))
    }
}

pub fn classify_complexity(
    input: &ComplexityInput,
    policy: Option<&MissionBudgetOverride>,
) -> ComplexityAssessment {
    let target_score = input.planned_target_count.saturating_sub(1).min(8);
    let new_file_score = input.new_file_count.min(4);
    let repository_score = input.repository_count.saturating_sub(1).saturating_mul(4);
    let dependency_score = input.dependency_change_count.saturating_mul(4).min(8);
    let schema_score = input.database_schema_change_count.saturating_mul(7).min(14);
    let integration_score = input.external_integration_count.saturating_mul(5).min(10);
    let security_score = input
        .security_sensitive_change_count
        .saturating_mul(6)
        .min(12);
    let uncertainty_score = input.architectural_uncertainty.min(5).saturating_mul(2);
    let test_score = match input.test_surface {
        0..=2 => 0,
        3..=7 => 2,
        8..=15 => 4,
        _ => 7,
    };
    let validation_minutes = input.expected_validation_duration.as_secs().div_ceil(60);
    let validation_score = match validation_minutes {
        0..=5 => 0,
        6..=15 => 2,
        16..=30 => 4,
        _ => 7,
    };
    let cross_module_score = input.cross_module_impact.saturating_mul(2).min(8);

    let raw_factors = [
        (
            ComplexityFactorKind::PlannedTargetCount,
            u64::from(input.planned_target_count),
            target_score,
            "planned targets",
        ),
        (
            ComplexityFactorKind::NewFileCount,
            u64::from(input.new_file_count),
            new_file_score,
            "new files",
        ),
        (
            ComplexityFactorKind::RepositoryCount,
            u64::from(input.repository_count),
            repository_score,
            "repositories",
        ),
        (
            ComplexityFactorKind::DependencyChanges,
            u64::from(input.dependency_change_count),
            dependency_score,
            "dependency changes",
        ),
        (
            ComplexityFactorKind::DatabaseSchemaChanges,
            u64::from(input.database_schema_change_count),
            schema_score,
            "database or schema changes",
        ),
        (
            ComplexityFactorKind::ExternalIntegrations,
            u64::from(input.external_integration_count),
            integration_score,
            "external integrations",
        ),
        (
            ComplexityFactorKind::SecuritySensitiveChanges,
            u64::from(input.security_sensitive_change_count),
            security_score,
            "security-sensitive changes",
        ),
        (
            ComplexityFactorKind::ArchitecturalUncertainty,
            u64::from(input.architectural_uncertainty),
            uncertainty_score,
            "architectural uncertainty",
        ),
        (
            ComplexityFactorKind::TestSurface,
            u64::from(input.test_surface),
            test_score,
            "test surface",
        ),
        (
            ComplexityFactorKind::ExpectedValidationDuration,
            validation_minutes,
            validation_score,
            "expected validation minutes",
        ),
        (
            ComplexityFactorKind::CrossModuleImpact,
            u64::from(input.cross_module_impact),
            cross_module_score,
            "cross-module impact",
        ),
    ];
    let factors = raw_factors
        .into_iter()
        .map(|(kind, value, score, detail)| ComplexityFactor {
            kind,
            value,
            score,
            detail: detail.to_owned(),
        })
        .collect::<Vec<_>>();
    let score = factors.iter().map(|factor| factor.score).sum();
    let class = match score {
        0..=2 => MissionComplexity::Tiny,
        3..=9 => MissionComplexity::Small,
        10..=19 => MissionComplexity::Medium,
        _ => MissionComplexity::Large,
    };
    let default_budget = MissionBudget::for_complexity(class);
    let budget = policy.map_or(default_budget.clone(), |value| {
        default_budget.applying_override(value)
    });

    ComplexityAssessment {
        class,
        score,
        factors,
        budget,
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionNodeKind {
    Discovery,
    Planning,
    #[default]
    SourceMutation,
    TestMutation,
    ValidationFocused,
    ValidationSuite,
    ValidationBuild,
    ValidationLint,
    DiffReview,
    CompletionEvaluation,
    Publication,
}

impl ExecutionNodeKind {
    pub const fn stage(self) -> HostedExecutionStage {
        match self {
            Self::Discovery => HostedExecutionStage::Discovery,
            Self::Planning => HostedExecutionStage::Planning,
            Self::SourceMutation | Self::TestMutation => HostedExecutionStage::Implementation,
            Self::ValidationFocused
            | Self::ValidationSuite
            | Self::ValidationBuild
            | Self::ValidationLint => HostedExecutionStage::Validation,
            Self::DiffReview | Self::CompletionEvaluation => HostedExecutionStage::Review,
            Self::Publication => HostedExecutionStage::Publication,
        }
    }

    pub const fn is_mutation(self) -> bool {
        matches!(self, Self::SourceMutation | Self::TestMutation)
    }

    pub const fn is_validation(self) -> bool {
        matches!(
            self,
            Self::ValidationFocused
                | Self::ValidationSuite
                | Self::ValidationBuild
                | Self::ValidationLint
        )
    }

    pub const fn requires_model(self) -> bool {
        matches!(
            self,
            Self::Discovery
                | Self::Planning
                | Self::SourceMutation
                | Self::TestMutation
                | Self::DiffReview
                | Self::CompletionEvaluation
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionNodeStatus {
    #[default]
    Pending,
    Ready,
    Running,
    Applied,
    Passed,
    FailedRecoverable,
    FailedBlocking,
    Superseded,
    Skipped,
    Completed,
}

impl ExecutionNodeStatus {
    pub const fn is_success(self) -> bool {
        matches!(
            self,
            Self::Applied | Self::Passed | Self::Superseded | Self::Skipped | Self::Completed
        )
    }

    pub const fn satisfies_dependency(self) -> bool {
        self.is_success()
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Applied
                | Self::Passed
                | Self::FailedBlocking
                | Self::Superseded
                | Self::Skipped
                | Self::Completed
        )
    }

    pub const fn is_failure(self) -> bool {
        matches!(self, Self::FailedRecoverable | Self::FailedBlocking)
    }

    pub const fn is_runnable(self) -> bool {
        matches!(self, Self::Ready | Self::Running | Self::FailedRecoverable)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct PlannedTarget {
    pub change_id: String,
    pub path: String,
    pub role: String,
    pub intent: String,
    #[serde(default)]
    pub acceptance_criteria_ids: Vec<String>,
    #[serde(default)]
    pub new_file: bool,
}

impl PlannedTarget {
    pub fn is_test_target(&self) -> bool {
        let path = self.path.to_ascii_lowercase();
        let role = self.role.to_ascii_lowercase();
        role.contains("test")
            || path.starts_with("tests/")
            || path.contains("/tests/")
            || path.contains("/__tests__/")
            || path.contains(".test.")
            || path.contains(".spec.")
            || path.ends_with("_test.rs")
            || path.ends_with("_tests.rs")
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationGateType {
    FocusedTest,
    #[default]
    TestSuite,
    Build,
    Lint,
    Typecheck,
    Custom,
}

pub type ValidationGateKind = ValidationGateType;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationGateSpec {
    pub gate_id: String,
    pub gate_type: ValidationGateType,
    pub command: String,
    #[serde(default)]
    pub working_directory: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub dependency_lock_hash: String,
    #[serde(default)]
    pub relevant_environment_fingerprint: String,
}

impl ValidationGateSpec {
    pub fn fingerprint(&self, repository_fingerprint: &str) -> String {
        validation_fingerprint(
            &self.command,
            &self.working_directory,
            repository_fingerprint,
            &self.dependency_lock_hash,
            &self.relevant_environment_fingerprint,
        )
    }

    pub fn node_kind(&self) -> ExecutionNodeKind {
        match self.gate_type {
            ValidationGateType::FocusedTest => ExecutionNodeKind::ValidationFocused,
            ValidationGateType::Build => ExecutionNodeKind::ValidationBuild,
            ValidationGateType::Lint => ExecutionNodeKind::ValidationLint,
            ValidationGateType::TestSuite
            | ValidationGateType::Typecheck
            | ValidationGateType::Custom => ExecutionNodeKind::ValidationSuite,
        }
    }
}

/// Places validation gates in the conservative execution order used by every
/// graph constructor. The complete key makes the result independent of the
/// order in which an otherwise equivalent manifest supplied its gates.
pub fn normalize_validation_gate_order(gates: &mut [ValidationGateSpec]) {
    gates.sort_by_cached_key(|gate| {
        let class = match gate.gate_type {
            ValidationGateType::FocusedTest => 0_u8,
            ValidationGateType::TestSuite => 1,
            ValidationGateType::Build => 2,
            ValidationGateType::Lint | ValidationGateType::Typecheck => 3,
            ValidationGateType::Custom => 4,
        };
        (
            class,
            gate.gate_id.clone(),
            normalize_command(&gate.command),
            gate.working_directory.clone(),
            gate.required,
            gate.dependency_lock_hash.clone(),
            gate.relevant_environment_fingerprint.clone(),
        )
    });
}

pub fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn validation_fingerprint(
    command: &str,
    working_directory: &str,
    repository_fingerprint: &str,
    dependency_lock_hash: &str,
    relevant_environment_fingerprint: &str,
) -> String {
    stable_hash(&format!(
        "{}\0{}\0{}\0{}\0{}",
        normalize_command(command),
        working_directory,
        repository_fingerprint,
        dependency_lock_hash,
        relevant_environment_fingerprint
    ))
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct NodeBudget {
    pub max_model_calls: u32,
    pub max_cost_micros: u64,
    #[serde(with = "duration_millis")]
    pub max_duration: Duration,
    pub max_repair_attempts: u32,
}

impl NodeBudget {
    pub fn remaining(&self, usage: &NodeBudgetUsage) -> NodeBudgetRemaining {
        NodeBudgetRemaining {
            model_calls: self.max_model_calls.saturating_sub(usage.model_calls),
            cost_micros: self.max_cost_micros.saturating_sub(usage.cost_micros),
            duration: self.max_duration.saturating_sub(usage.duration),
            repair_attempts: self
                .max_repair_attempts
                .saturating_sub(usage.repair_attempts),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct NodeBudgetRemaining {
    pub model_calls: u32,
    pub cost_micros: u64,
    #[serde(with = "duration_millis")]
    pub duration: Duration,
    pub repair_attempts: u32,
}

impl NodeBudgetRemaining {
    pub fn exhausted(&self) -> bool {
        self.model_calls == 0 || self.cost_micros == 0 || self.duration.is_zero()
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct NodeAttempt {
    pub attempt: u32,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub repository_fingerprint_before: String,
    pub repository_fingerprint_after: Option<String>,
    pub model_calls: u32,
    pub cost_micros: u64,
    #[serde(with = "duration_millis")]
    pub duration: Duration,
    pub outcome: Option<ExecutionNodeStatus>,
    pub failure_id: Option<FailureId>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExecutionNode {
    pub id: ExecutionNodeId,
    pub kind: ExecutionNodeKind,
    #[serde(default)]
    pub dependencies: Vec<ExecutionNodeId>,
    pub status: ExecutionNodeStatus,
    pub required: bool,
    pub target: Option<PlannedTarget>,
    pub validation: Option<ValidationGateSpec>,
    #[serde(default)]
    pub attempts: Vec<NodeAttempt>,
    pub budget: NodeBudget,
    #[serde(default)]
    pub evidence_ids: Vec<String>,
}

impl ExecutionNode {
    pub fn new(id: impl Into<ExecutionNodeId>, kind: ExecutionNodeKind) -> Self {
        Self {
            id: id.into(),
            kind,
            ..Self::default()
        }
    }

    pub fn is_runnable(&self) -> bool {
        self.status.is_runnable()
    }

    pub fn is_successful(&self) -> bool {
        self.status.is_success()
    }

    pub fn remaining_budget(&self, usage: &NodeBudgetUsage) -> NodeBudgetRemaining {
        self.budget.remaining(usage)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct AcceptedPlan {
    #[serde(default)]
    pub targets: Vec<PlannedTarget>,
    #[serde(default)]
    pub validation_gates: Vec<ValidationGateSpec>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExecutionGraph {
    pub schema_version: u16,
    pub graph_id: String,
    pub complexity: MissionComplexity,
    pub nodes: Vec<ExecutionNode>,
    pub created_from_repository_fingerprint: String,
    /// Monotonically increases whenever authoritative graph state changes.
    #[serde(default)]
    pub revision: u64,
    /// Nodes whose incomplete status remains visible as remaining work, but
    /// whose dependency edge is explicitly satisfied for a reviewable partial
    /// path. Only an authoritative `PartialReviewable` guardrail event may add
    /// entries here.
    #[serde(default)]
    pub dependency_satisfaction_overrides: BTreeSet<ExecutionNodeId>,
    /// An explicit draft-recovery publication may satisfy only the publication
    /// node's direct dependency without fabricating review or completion
    /// success. The authorizing domain event is the sole writer.
    #[serde(default)]
    pub recovery_publication_dependency_override: bool,
}

impl Default for ExecutionGraph {
    fn default() -> Self {
        Self {
            schema_version: EXECUTION_GRAPH_SCHEMA_VERSION,
            graph_id: String::new(),
            complexity: MissionComplexity::Tiny,
            nodes: Vec::new(),
            created_from_repository_fingerprint: String::new(),
            revision: 0,
            dependency_satisfaction_overrides: BTreeSet::new(),
            recovery_publication_dependency_override: false,
        }
    }
}

impl ExecutionGraph {
    pub fn bootstrap(
        graph_id: impl Into<String>,
        repository_fingerprint: impl Into<String>,
        complexity: MissionComplexity,
        mission_budget: &MissionBudget,
    ) -> Self {
        let mut graph = Self {
            graph_id: graph_id.into(),
            complexity,
            created_from_repository_fingerprint: repository_fingerprint.into(),
            ..Self::default()
        };
        graph.nodes = vec![
            ExecutionNode {
                id: ExecutionNodeId::new("discovery"),
                kind: ExecutionNodeKind::Discovery,
                status: ExecutionNodeStatus::Ready,
                required: true,
                ..ExecutionNode::default()
            },
            ExecutionNode {
                id: ExecutionNodeId::new("planning"),
                kind: ExecutionNodeKind::Planning,
                dependencies: vec![ExecutionNodeId::new("discovery")],
                required: true,
                ..ExecutionNode::default()
            },
        ];
        assign_node_budgets(&mut graph.nodes, mission_budget);
        graph
    }

    pub fn from_accepted_plan(
        graph_id: impl Into<String>,
        complexity: MissionComplexity,
        repository_fingerprint: impl Into<String>,
        plan: &AcceptedPlan,
        mission_budget: &MissionBudget,
    ) -> Self {
        build_execution_graph(
            graph_id,
            complexity,
            repository_fingerprint,
            &plan.targets,
            &plan.validation_gates,
            mission_budget,
        )
    }

    pub fn from_targets(
        graph_id: impl Into<String>,
        complexity: MissionComplexity,
        repository_fingerprint: impl Into<String>,
        targets: &[PlannedTarget],
        validation_gates: &[ValidationGateSpec],
        mission_budget: &MissionBudget,
    ) -> Self {
        build_execution_graph(
            graph_id,
            complexity,
            repository_fingerprint,
            targets,
            validation_gates,
            mission_budget,
        )
    }

    pub fn node(&self, id: &ExecutionNodeId) -> Option<&ExecutionNode> {
        self.nodes.iter().find(|node| &node.id == id)
    }

    pub fn node_mut(&mut self, id: &ExecutionNodeId) -> Option<&mut ExecutionNode> {
        self.nodes.iter_mut().find(|node| &node.id == id)
    }

    pub fn node_by_str(&self, id: &str) -> Option<&ExecutionNode> {
        self.nodes.iter().find(|node| node.id.as_str() == id)
    }

    pub fn unique_mutation_node_for_target_path(&self, path: &str) -> Option<&ExecutionNode> {
        let mut matches = self.nodes.iter().filter(|node| {
            node.kind.is_mutation()
                && node
                    .target
                    .as_ref()
                    .is_some_and(|target| target.path == path)
        });
        let node = matches.next()?;
        matches.next().is_none().then_some(node)
    }

    pub fn active_node(&self) -> Option<&ExecutionNode> {
        self.nodes.iter().find(|node| {
            node.status == ExecutionNodeStatus::Running
                && !self.dependency_satisfaction_overrides.contains(&node.id)
        })
    }

    /// Selects deterministically by persisted graph order. A running node owns
    /// execution; otherwise a recoverable failure is repaired before new work;
    /// otherwise the first ready node runs.
    pub fn next_runnable_node(&self) -> Option<&ExecutionNode> {
        self.active_node()
            .or_else(|| {
                self.nodes.iter().find(|node| {
                    node.status == ExecutionNodeStatus::FailedRecoverable
                        && !self.dependency_satisfaction_overrides.contains(&node.id)
                })
            })
            .or_else(|| {
                self.nodes.iter().find(|node| {
                    node.status == ExecutionNodeStatus::Ready
                        && !self.dependency_satisfaction_overrides.contains(&node.id)
                })
            })
    }

    pub fn ready_nodes(&self) -> impl Iterator<Item = &ExecutionNode> {
        self.nodes.iter().filter(|node| {
            node.status == ExecutionNodeStatus::Ready
                && !self.dependency_satisfaction_overrides.contains(&node.id)
        })
    }

    pub fn remaining_required_nodes(&self) -> Vec<&ExecutionNode> {
        self.nodes
            .iter()
            .filter(|node| node.required && !node.status.is_success())
            .collect()
    }

    pub fn all_required_nodes_succeeded(&self) -> bool {
        self.nodes
            .iter()
            .filter(|node| node.required)
            .all(|node| node.status.is_success())
    }

    pub fn has_blocking_required_node(&self) -> bool {
        self.nodes
            .iter()
            .any(|node| node.required && node.status == ExecutionNodeStatus::FailedBlocking)
    }

    pub fn stage(&self) -> HostedExecutionStage {
        if let Some(node) = self.next_runnable_node() {
            return node.kind.stage();
        }
        if let Some(node) = self
            .nodes
            .iter()
            .find(|node| node.required && !node.status.is_success())
        {
            return if node.status == ExecutionNodeStatus::FailedBlocking {
                HostedExecutionStage::Terminal
            } else {
                node.kind.stage()
            };
        }
        HostedExecutionStage::Terminal
    }

    pub fn set_node_status(
        &mut self,
        id: &ExecutionNodeId,
        status: ExecutionNodeStatus,
    ) -> Result<(), GraphInvariantError> {
        let node = self
            .node_mut(id)
            .ok_or_else(|| GraphInvariantError::new(format!("unknown execution node `{id}`")))?;
        node.status = status;
        if status.is_success() {
            self.dependency_satisfaction_overrides.remove(id);
        }
        self.revision = self.revision.saturating_add(1);
        self.refresh_readiness();
        Ok(())
    }

    /// Materializes readiness from dependency state. It never changes active,
    /// completed, or failed nodes.
    pub fn refresh_readiness(&mut self) {
        let successful = self
            .nodes
            .iter()
            .filter(|node| node.status.satisfies_dependency())
            .map(|node| node.id.clone())
            .chain(self.dependency_satisfaction_overrides.iter().cloned())
            .collect::<BTreeSet<_>>();
        let mut changed = false;
        for node in &mut self.nodes {
            let dependencies_satisfied = node
                .dependencies
                .iter()
                .all(|dependency| successful.contains(dependency));
            match (node.status, dependencies_satisfied) {
                (ExecutionNodeStatus::Pending, true) => {
                    node.status = ExecutionNodeStatus::Ready;
                    changed = true;
                }
                (ExecutionNodeStatus::Ready, false) => {
                    node.status = ExecutionNodeStatus::Pending;
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            self.revision = self.revision.saturating_add(1);
        }
    }

    pub fn validation_node_for_fingerprint(
        &self,
        fingerprint: &str,
        repository_fingerprint: &str,
    ) -> Option<&ExecutionNode> {
        self.nodes.iter().find(|node| {
            node.validation
                .as_ref()
                .is_some_and(|gate| gate.fingerprint(repository_fingerprint) == fingerprint)
        })
    }

    pub fn validate_invariants(&self) -> Result<(), GraphInvariantError> {
        self.validate_invariants_with_dependency_satisfaction(&BTreeSet::new())
    }

    /// Validates both graph topology and materialized node state. The extra
    /// satisfaction set is used by `ExecutionSnapshot` for evidence-backed
    /// validation and explicit partial-review dependency overrides.
    pub fn validate_invariants_with_dependency_satisfaction(
        &self,
        additionally_satisfied: &BTreeSet<ExecutionNodeId>,
    ) -> Result<(), GraphInvariantError> {
        if self.schema_version == 0 {
            return Err(GraphInvariantError::new(
                "execution graph schema version must be non-zero",
            ));
        }
        if self.graph_id.trim().is_empty() {
            return Err(GraphInvariantError::new(
                "execution graph id must not be empty",
            ));
        }

        let mut ids = BTreeSet::new();
        for node in &self.nodes {
            if node.id.is_empty() {
                return Err(GraphInvariantError::new(
                    "execution node id must not be empty",
                ));
            }
            if !ids.insert(node.id.clone()) {
                return Err(GraphInvariantError::new(format!(
                    "duplicate execution node id `{}`",
                    node.id
                )));
            }
            if node.kind.is_mutation() && node.target.is_none() {
                return Err(GraphInvariantError::new(format!(
                    "mutation node `{}` has no planned target",
                    node.id
                )));
            }
            if node.kind.is_validation() && node.validation.is_none() {
                return Err(GraphInvariantError::new(format!(
                    "validation node `{}` has no gate specification",
                    node.id
                )));
            }
        }
        for node in &self.nodes {
            let mut dependencies = BTreeSet::new();
            for dependency in &node.dependencies {
                if dependency == &node.id {
                    return Err(GraphInvariantError::new(format!(
                        "node `{}` depends on itself",
                        node.id
                    )));
                }
                if !ids.contains(dependency) {
                    return Err(GraphInvariantError::new(format!(
                        "node `{}` has unknown dependency `{dependency}`",
                        node.id
                    )));
                }
                if !dependencies.insert(dependency) {
                    return Err(GraphInvariantError::new(format!(
                        "node `{}` repeats dependency `{dependency}`",
                        node.id
                    )));
                }
            }
        }
        for node_id in &self.dependency_satisfaction_overrides {
            let node = self.node(node_id).ok_or_else(|| {
                GraphInvariantError::new(format!(
                    "dependency satisfaction override refers to unknown node `{node_id}`"
                ))
            })?;
            if !node.kind.is_mutation() {
                return Err(GraphInvariantError::new(format!(
                    "dependency satisfaction override `{node_id}` is not a mutation node"
                )));
            }
        }
        if self.recovery_publication_dependency_override {
            let publication = self
                .nodes
                .iter()
                .find(|node| node.kind == ExecutionNodeKind::Publication)
                .ok_or_else(|| {
                    GraphInvariantError::new(
                        "recovery publication dependency override has no publication node",
                    )
                })?;
            if !matches!(
                publication.status,
                ExecutionNodeStatus::Running | ExecutionNodeStatus::Completed
            ) {
                return Err(GraphInvariantError::new(
                    "recovery publication dependency override requires an active or completed publication node",
                ));
            }
        }
        self.validate_acyclic()?;

        let diff_nodes = self.nodes_of_kind(ExecutionNodeKind::DiffReview);
        let completion_nodes = self.nodes_of_kind(ExecutionNodeKind::CompletionEvaluation);
        let publication_nodes = self.nodes_of_kind(ExecutionNodeKind::Publication);
        if diff_nodes.len() > 1 || completion_nodes.len() > 1 || publication_nodes.len() > 1 {
            return Err(GraphInvariantError::new(
                "execution graph may contain only one review, completion, and publication node",
            ));
        }
        if let Some(completion) = completion_nodes.first() {
            let Some(diff) = diff_nodes.first() else {
                return Err(GraphInvariantError::new(
                    "completion evaluation requires a diff review node",
                ));
            };
            if !self.transitively_depends_on(&completion.id, &diff.id) {
                return Err(GraphInvariantError::new(
                    "completion evaluation must depend on diff review",
                ));
            }
        }
        if let Some(publication) = publication_nodes.first() {
            let Some(completion) = completion_nodes.first() else {
                return Err(GraphInvariantError::new(
                    "publication requires a completion evaluation node",
                ));
            };
            if !self.transitively_depends_on(&publication.id, &completion.id) {
                return Err(GraphInvariantError::new(
                    "publication must depend on completion evaluation",
                ));
            }
        }
        if let Some(diff) = diff_nodes.first() {
            for validation in self
                .nodes
                .iter()
                .filter(|node| node.required && node.kind.is_validation())
            {
                if !self.transitively_depends_on(&diff.id, &validation.id) {
                    return Err(GraphInvariantError::new(format!(
                        "diff review does not depend on required validation `{}`",
                        validation.id
                    )));
                }
            }
        }

        let satisfied = self.dependency_satisfaction_ids(additionally_satisfied);
        for node in &self.nodes {
            let requires_completed_dependencies = matches!(
                node.status,
                ExecutionNodeStatus::Running
                    | ExecutionNodeStatus::Applied
                    | ExecutionNodeStatus::Passed
                    | ExecutionNodeStatus::Superseded
                    | ExecutionNodeStatus::Completed
            );
            if requires_completed_dependencies {
                self.ensure_node_dependencies_satisfied(&node.id, &satisfied)?;
            }
        }
        Ok(())
    }

    fn dependency_satisfaction_ids(
        &self,
        additionally_satisfied: &BTreeSet<ExecutionNodeId>,
    ) -> BTreeSet<ExecutionNodeId> {
        let mut satisfied = self
            .nodes
            .iter()
            .filter(|node| node.status.satisfies_dependency())
            .map(|node| node.id.clone())
            .chain(self.dependency_satisfaction_overrides.iter().cloned())
            .chain(additionally_satisfied.iter().cloned())
            .collect::<BTreeSet<_>>();
        if self.recovery_publication_dependency_override
            && let Some(publication) = self
                .nodes
                .iter()
                .find(|node| node.kind == ExecutionNodeKind::Publication)
        {
            satisfied.extend(publication.dependencies.iter().cloned());
        }
        satisfied
    }

    fn ensure_node_dependencies_satisfied(
        &self,
        node_id: &ExecutionNodeId,
        satisfied: &BTreeSet<ExecutionNodeId>,
    ) -> Result<(), GraphInvariantError> {
        let node = self.node(node_id).ok_or_else(|| {
            GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
        })?;
        if let Some(dependency) = node
            .dependencies
            .iter()
            .find(|dependency| !satisfied.contains(*dependency))
        {
            return Err(GraphInvariantError::new(format!(
                "node `{node_id}` cannot advance before dependency `{dependency}` succeeds"
            )));
        }
        Ok(())
    }

    fn nodes_of_kind(&self, kind: ExecutionNodeKind) -> Vec<&ExecutionNode> {
        self.nodes.iter().filter(|node| node.kind == kind).collect()
    }

    fn transitively_depends_on(
        &self,
        node_id: &ExecutionNodeId,
        expected: &ExecutionNodeId,
    ) -> bool {
        let mut pending = self
            .node(node_id)
            .map(|node| node.dependencies.clone())
            .unwrap_or_default();
        let mut visited = BTreeSet::new();
        while let Some(candidate) = pending.pop() {
            if &candidate == expected {
                return true;
            }
            if visited.insert(candidate.clone())
                && let Some(node) = self.node(&candidate)
            {
                pending.extend(node.dependencies.iter().cloned());
            }
        }
        false
    }

    fn validate_acyclic(&self) -> Result<(), GraphInvariantError> {
        fn visit(
            graph: &ExecutionGraph,
            id: &ExecutionNodeId,
            visiting: &mut BTreeSet<ExecutionNodeId>,
            visited: &mut BTreeSet<ExecutionNodeId>,
        ) -> Result<(), GraphInvariantError> {
            if visited.contains(id) {
                return Ok(());
            }
            if !visiting.insert(id.clone()) {
                return Err(GraphInvariantError::new(format!(
                    "execution graph contains a dependency cycle at `{id}`"
                )));
            }
            if let Some(node) = graph.node(id) {
                for dependency in &node.dependencies {
                    visit(graph, dependency, visiting, visited)?;
                }
            }
            visiting.remove(id);
            visited.insert(id.clone());
            Ok(())
        }

        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for node in &self.nodes {
            visit(self, &node.id, &mut visiting, &mut visited)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphInvariantError {
    pub message: String,
}

impl GraphInvariantError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for GraphInvariantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GraphInvariantError {}

pub fn build_execution_graph(
    graph_id: impl Into<String>,
    complexity: MissionComplexity,
    repository_fingerprint: impl Into<String>,
    targets: &[PlannedTarget],
    validation_gates: &[ValidationGateSpec],
    mission_budget: &MissionBudget,
) -> ExecutionGraph {
    let mut nodes = Vec::new();
    let discovery_id = ExecutionNodeId::new("discovery");
    let planning_id = ExecutionNodeId::new("planning");
    nodes.push(ExecutionNode {
        id: discovery_id.clone(),
        kind: ExecutionNodeKind::Discovery,
        status: ExecutionNodeStatus::Completed,
        required: true,
        ..ExecutionNode::default()
    });
    nodes.push(ExecutionNode {
        id: planning_id.clone(),
        kind: ExecutionNodeKind::Planning,
        dependencies: vec![discovery_id],
        status: ExecutionNodeStatus::Completed,
        required: true,
        ..ExecutionNode::default()
    });

    let mut ordered_targets = targets.iter().enumerate().collect::<Vec<_>>();
    ordered_targets.sort_by_key(|(index, target)| (target.is_test_target(), *index));
    let mut previous_mutation = planning_id;
    let mut mutation_ids = Vec::new();
    for (original_index, target) in ordered_targets {
        let kind = if target.is_test_target() {
            ExecutionNodeKind::TestMutation
        } else {
            ExecutionNodeKind::SourceMutation
        };
        let id = stable_node_id(kind, &target.path, original_index);
        nodes.push(ExecutionNode {
            id: id.clone(),
            kind,
            dependencies: vec![previous_mutation.clone()],
            status: ExecutionNodeStatus::Pending,
            required: true,
            target: Some(target.clone()),
            ..ExecutionNode::default()
        });
        mutation_ids.push(id.clone());
        previous_mutation = id;
    }

    let validation_base_dependencies = mutation_ids
        .last()
        .cloned()
        .map_or_else(|| vec![previous_mutation], |id| vec![id]);
    let mut ordered_validation_gates = validation_gates.to_vec();
    normalize_validation_gate_order(&mut ordered_validation_gates);
    let mut validation_dependencies = validation_base_dependencies.clone();
    for (index, gate) in ordered_validation_gates.iter().enumerate() {
        let kind = gate.node_kind();
        let id = stable_node_id(kind, &gate.gate_id, index);
        let required = gate.required;
        nodes.push(ExecutionNode {
            id: id.clone(),
            kind,
            dependencies: if required {
                validation_dependencies.clone()
            } else {
                validation_base_dependencies.clone()
            },
            status: if required {
                ExecutionNodeStatus::Pending
            } else {
                ExecutionNodeStatus::Skipped
            },
            required,
            validation: Some(gate.clone()),
            ..ExecutionNode::default()
        });
        if required {
            validation_dependencies = vec![id];
        }
    }

    let diff_dependencies = validation_dependencies;
    let diff_id = ExecutionNodeId::new("diff-review");
    let completion_id = ExecutionNodeId::new("completion-evaluation");
    let publication_id = ExecutionNodeId::new("publication");
    nodes.push(ExecutionNode {
        id: diff_id.clone(),
        kind: ExecutionNodeKind::DiffReview,
        dependencies: diff_dependencies,
        status: ExecutionNodeStatus::Pending,
        required: true,
        ..ExecutionNode::default()
    });
    nodes.push(ExecutionNode {
        id: completion_id.clone(),
        kind: ExecutionNodeKind::CompletionEvaluation,
        dependencies: vec![diff_id],
        status: ExecutionNodeStatus::Pending,
        required: true,
        ..ExecutionNode::default()
    });
    nodes.push(ExecutionNode {
        id: publication_id,
        kind: ExecutionNodeKind::Publication,
        dependencies: vec![completion_id],
        status: ExecutionNodeStatus::Pending,
        required: true,
        ..ExecutionNode::default()
    });

    assign_node_budgets(&mut nodes, mission_budget);
    let mut graph = ExecutionGraph {
        schema_version: EXECUTION_GRAPH_SCHEMA_VERSION,
        graph_id: graph_id.into(),
        complexity,
        nodes,
        created_from_repository_fingerprint: repository_fingerprint.into(),
        revision: 1,
        dependency_satisfaction_overrides: BTreeSet::new(),
        recovery_publication_dependency_override: false,
    };
    graph.refresh_readiness();
    graph
}

fn stable_node_id(kind: ExecutionNodeKind, label: &str, index: usize) -> ExecutionNodeId {
    let prefix = match kind {
        ExecutionNodeKind::SourceMutation => "source",
        ExecutionNodeKind::TestMutation => "test",
        ExecutionNodeKind::ValidationFocused => "validation-focused",
        ExecutionNodeKind::ValidationSuite => "validation-suite",
        ExecutionNodeKind::ValidationBuild => "validation-build",
        ExecutionNodeKind::ValidationLint => "validation-lint",
        ExecutionNodeKind::Discovery => "discovery",
        ExecutionNodeKind::Planning => "planning",
        ExecutionNodeKind::DiffReview => "diff-review",
        ExecutionNodeKind::CompletionEvaluation => "completion-evaluation",
        ExecutionNodeKind::Publication => "publication",
    };
    let digest = stable_hash(&format!("{prefix}\0{index}\0{label}"));
    ExecutionNodeId::new(format!("{prefix}-{index:03}-{}", &digest[..12]))
}

fn stable_hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn assign_node_budgets(nodes: &mut [ExecutionNode], mission: &MissionBudget) {
    let mut groups = BTreeMap::<BudgetGroup, Vec<usize>>::new();
    for (index, node) in nodes.iter().enumerate() {
        groups
            .entry(BudgetGroup::for_kind(node.kind))
            .or_default()
            .push(index);
    }

    // Review and completion are assigned before mutation work and therefore
    // cannot be consumed by implementation nodes. Publication and validation
    // are also independently bounded even though they normally make no model call.
    let call_percentages = [
        (BudgetGroup::Discovery, 12_u64),
        (BudgetGroup::Planning, 8),
        (BudgetGroup::Mutation, 62),
        (BudgetGroup::Validation, 0),
        (BudgetGroup::Review, 8),
        (BudgetGroup::Completion, 10),
        (BudgetGroup::Publication, 0),
    ];
    let cost_percentages = [
        (BudgetGroup::Discovery, 8_u64),
        (BudgetGroup::Planning, 8),
        (BudgetGroup::Mutation, 52),
        (BudgetGroup::Validation, 15),
        (BudgetGroup::Review, 6),
        (BudgetGroup::Completion, 6),
        (BudgetGroup::Publication, 5),
    ];
    let duration_percentages = [
        (BudgetGroup::Discovery, 10_u64),
        (BudgetGroup::Planning, 8),
        (BudgetGroup::Mutation, 47),
        (BudgetGroup::Validation, 25),
        (BudgetGroup::Review, 4),
        (BudgetGroup::Completion, 3),
        (BudgetGroup::Publication, 3),
    ];

    for (group, indices) in &groups {
        let call_total = percentage_share(
            u64::from(mission.max_model_calls),
            percentage_for(&call_percentages, *group),
        ) as u32;
        let cost_total = percentage_share(
            mission.max_cost_micros,
            percentage_for(&cost_percentages, *group),
        );
        let duration_total = percentage_share(
            u64::try_from(mission.max_duration.as_millis()).unwrap_or(u64::MAX),
            percentage_for(&duration_percentages, *group),
        );
        let count = indices.len();
        for (position, index) in indices.iter().copied().enumerate() {
            let node = &mut nodes[index];
            node.budget = NodeBudget {
                max_model_calls: distribute_u32(call_total, count, position),
                max_cost_micros: distribute_u64(cost_total, count, position),
                max_duration: Duration::from_millis(distribute_u64(
                    duration_total,
                    count,
                    position,
                )),
                max_repair_attempts: if node.kind.is_mutation() {
                    mission.max_target_repair_rounds
                } else if node.kind.is_validation() {
                    mission.max_target_repair_rounds.min(1)
                } else {
                    0
                },
            };
        }
    }
}

#[derive(Clone, Copy, Debug, Ord, PartialEq, Eq, PartialOrd)]
enum BudgetGroup {
    Discovery,
    Planning,
    Mutation,
    Validation,
    Review,
    Completion,
    Publication,
}

impl BudgetGroup {
    const fn for_kind(kind: ExecutionNodeKind) -> Self {
        match kind {
            ExecutionNodeKind::Discovery => Self::Discovery,
            ExecutionNodeKind::Planning => Self::Planning,
            ExecutionNodeKind::SourceMutation | ExecutionNodeKind::TestMutation => Self::Mutation,
            ExecutionNodeKind::ValidationFocused
            | ExecutionNodeKind::ValidationSuite
            | ExecutionNodeKind::ValidationBuild
            | ExecutionNodeKind::ValidationLint => Self::Validation,
            ExecutionNodeKind::DiffReview => Self::Review,
            ExecutionNodeKind::CompletionEvaluation => Self::Completion,
            ExecutionNodeKind::Publication => Self::Publication,
        }
    }
}

fn percentage_for(values: &[(BudgetGroup, u64)], group: BudgetGroup) -> u64 {
    values
        .iter()
        .find_map(|(candidate, value)| (*candidate == group).then_some(*value))
        .unwrap_or(0)
}

fn percentage_share(total: u64, percent: u64) -> u64 {
    total.saturating_mul(percent) / 100
}

fn distribute_u64(total: u64, count: usize, position: usize) -> u64 {
    if count == 0 {
        return 0;
    }
    let count = u64::try_from(count).unwrap_or(u64::MAX);
    let position = u64::try_from(position).unwrap_or(u64::MAX);
    total / count + u64::from(position < total % count)
}

fn distribute_u32(total: u32, count: usize, position: usize) -> u32 {
    u32::try_from(distribute_u64(u64::from(total), count, position)).unwrap_or(u32::MAX)
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct RepositorySnapshot {
    pub fingerprint: String,
    #[serde(default)]
    pub source_tree_hash: String,
    #[serde(default)]
    pub dependency_lock_hash: String,
    #[serde(default)]
    pub relevant_environment_fingerprint: String,
    #[serde(default)]
    pub changed_paths: BTreeSet<String>,
}

impl RepositorySnapshot {
    pub fn has_changes(&self) -> bool {
        !self.changed_paths.is_empty()
    }

    pub fn contains_changed_path(&self, path: &str) -> bool {
        self.changed_paths.contains(path)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl LineRange {
    pub fn new(start: u32, end: u32) -> Option<Self> {
        (start > 0 && end >= start).then_some(Self { start, end })
    }

    pub const fn contains(self, other: Self) -> bool {
        self.start <= other.start && self.end >= other.end
    }

    pub const fn line_count(self) -> u32 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }
}

impl Default for LineRange {
    fn default() -> Self {
        Self { start: 1, end: 1 }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct FileEvidence {
    pub evidence_id: String,
    pub path: String,
    pub content_hash: String,
    pub repository_fingerprint: String,
    pub line_range: Option<LineRange>,
    pub captured_content: String,
    #[serde(default)]
    pub truncated: bool,
}

impl FileEvidence {
    pub fn capture(
        path: impl Into<String>,
        repository_fingerprint: impl Into<String>,
        line_range: Option<LineRange>,
        captured_content: impl Into<String>,
        truncated: bool,
    ) -> Self {
        let path = path.into();
        let repository_fingerprint = repository_fingerprint.into();
        let captured_content = captured_content.into();
        let content_hash = stable_hash(&captured_content);
        let range_key = line_range
            .map(|range| format!("{}-{}", range.start, range.end))
            .unwrap_or_else(|| "full".to_owned());
        let evidence_id = format!(
            "file-{}",
            &stable_hash(&format!(
                "{path}\0{repository_fingerprint}\0{range_key}\0{content_hash}"
            ))[..20]
        );
        Self {
            evidence_id,
            path,
            content_hash,
            repository_fingerprint,
            line_range,
            captured_content,
            truncated,
        }
    }

    pub fn content_hash_is_valid(&self) -> bool {
        stable_hash(&self.captured_content) == self.content_hash
    }

    pub fn satisfies_range(&self, required: Option<LineRange>) -> bool {
        match required {
            Some(required) => {
                self.line_range
                    .is_some_and(|range| range.contains(required))
                    || (self.line_range.is_none() && !self.truncated)
            }
            None => !self.truncated && self.line_range.is_none(),
        }
    }

    pub fn summary(&self) -> EvidenceSummary {
        EvidenceSummary {
            evidence_id: self.evidence_id.clone(),
            path: Some(self.path.clone()),
            content_hash: Some(self.content_hash.clone()),
            repository_fingerprint: self.repository_fingerprint.clone(),
            line_range: self.line_range,
            summary: if let Some(range) = self.line_range {
                format!("{} lines {}-{}", self.path, range.start, range.end)
            } else {
                format!("{} complete content", self.path)
            },
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct EvidenceSummary {
    pub evidence_id: String,
    pub path: Option<String>,
    pub content_hash: Option<String>,
    pub repository_fingerprint: String,
    pub line_range: Option<LineRange>,
    pub summary: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct FileExcerpt {
    pub path: String,
    pub line_range: LineRange,
    pub content: String,
    pub content_hash: String,
}

impl From<&FileEvidence> for FileExcerpt {
    fn from(evidence: &FileEvidence) -> Self {
        Self {
            path: evidence.path.clone(),
            line_range: evidence.line_range.unwrap_or_default(),
            content: evidence.captured_content.clone(),
            content_hash: evidence.content_hash.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationEvidenceStatus {
    Running,
    #[default]
    Passed,
    Failed,
    TimedOut,
    Cancelled,
    Superseded,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationEvidenceRecord {
    pub evidence_id: String,
    pub node_id: ExecutionNodeId,
    pub gate_id: String,
    pub fingerprint: String,
    pub repository_fingerprint: String,
    pub command: String,
    pub working_directory: String,
    pub status: ValidationEvidenceStatus,
    pub exit_code: Option<i32>,
    pub output_summary: String,
    #[serde(with = "duration_millis")]
    pub duration: Duration,
}

impl ValidationEvidenceRecord {
    pub fn is_reusable_pass(&self, fingerprint: &str) -> bool {
        self.status == ValidationEvidenceStatus::Passed && self.fingerprint == fingerprint
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    #[default]
    RepositoryObservation,
    AcceptanceCriterion,
    Mutation,
    DiffReview,
    Completion,
    Publication,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct EvidenceRecord {
    pub evidence_id: String,
    pub kind: EvidenceKind,
    pub node_id: Option<ExecutionNodeId>,
    pub repository_fingerprint: String,
    pub summary: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct EvidenceStore {
    #[serde(default)]
    pub files: BTreeMap<String, FileEvidence>,
    #[serde(default)]
    pub validations: BTreeMap<String, ValidationEvidenceRecord>,
    #[serde(default)]
    pub records: BTreeMap<String, EvidenceRecord>,
}

impl EvidenceStore {
    /// Inserts evidence once and returns its stable id. An identical read is a
    /// cache hit, not a second evidence record or progress event.
    pub fn record_file(&mut self, evidence: FileEvidence) -> String {
        let id = evidence.evidence_id.clone();
        self.files.entry(id.clone()).or_insert(evidence);
        id
    }

    pub fn capture_file(
        &mut self,
        path: impl Into<String>,
        repository_fingerprint: impl Into<String>,
        line_range: Option<LineRange>,
        content: impl Into<String>,
        truncated: bool,
    ) -> String {
        self.record_file(FileEvidence::capture(
            path,
            repository_fingerprint,
            line_range,
            content,
            truncated,
        ))
    }

    pub fn reusable_file(
        &self,
        path: &str,
        repository_fingerprint: &str,
        required_range: Option<LineRange>,
    ) -> Option<&FileEvidence> {
        self.files
            .values()
            .filter(|evidence| {
                evidence.path == path
                    && evidence.repository_fingerprint == repository_fingerprint
                    && evidence.content_hash_is_valid()
                    && evidence.satisfies_range(required_range)
            })
            .max_by_key(|evidence| {
                evidence
                    .line_range
                    .map(LineRange::line_count)
                    .unwrap_or(u32::MAX)
            })
    }

    pub fn lookup_file(
        &self,
        path: &str,
        repository_fingerprint: &str,
        required_range: Option<LineRange>,
    ) -> Option<&FileEvidence> {
        self.reusable_file(path, repository_fingerprint, required_range)
    }

    pub fn record_validation(&mut self, evidence: ValidationEvidenceRecord) -> String {
        let id = evidence.evidence_id.clone();
        self.validations.entry(id.clone()).or_insert(evidence);
        id
    }

    pub fn passed_validation(&self, fingerprint: &str) -> Option<&ValidationEvidenceRecord> {
        self.validations
            .values()
            .find(|evidence| evidence.is_reusable_pass(fingerprint))
    }

    pub fn has_passed_validation(&self, fingerprint: &str) -> bool {
        self.passed_validation(fingerprint).is_some()
    }

    pub fn supersede_stale_validation(&mut self, repository_fingerprint: &str) -> usize {
        let mut count = 0;
        for evidence in self.validations.values_mut().filter(|evidence| {
            evidence.status == ValidationEvidenceStatus::Passed
                && evidence.repository_fingerprint != repository_fingerprint
        }) {
            evidence.status = ValidationEvidenceStatus::Superseded;
            count += 1;
        }
        count
    }

    pub fn record(&mut self, evidence: EvidenceRecord) -> String {
        let id = evidence.evidence_id.clone();
        self.records.entry(id.clone()).or_insert(evidence);
        id
    }

    pub fn summary(&self, evidence_id: &str) -> Option<EvidenceSummary> {
        if let Some(file) = self.files.get(evidence_id) {
            return Some(file.summary());
        }
        self.records.get(evidence_id).map(|record| EvidenceSummary {
            evidence_id: record.evidence_id.clone(),
            path: None,
            content_hash: None,
            repository_fingerprint: record.repository_fingerprint.clone(),
            line_range: None,
            summary: record.summary.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    ReadFile,
    SearchRepository,
    ApplyPatch,
    CreateFile,
    DeleteFile,
    RunFocusedCommand,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetExecutionContext {
    pub node_id: ExecutionNodeId,
    pub change_id: String,
    pub target: PlannedTarget,
    pub intent: String,
    #[serde(default)]
    pub acceptance_criteria_ids: Vec<String>,
    #[serde(default)]
    pub dependency_evidence: Vec<EvidenceSummary>,
    pub current_file_content: Option<String>,
    #[serde(default)]
    pub nearby_context: Vec<FileExcerpt>,
    #[serde(default)]
    pub allowed_tools: Vec<ToolKind>,
    pub remaining_node_budget: NodeBudgetRemaining,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum MutationResult {
    Applied {
        node_id: ExecutionNodeId,
        target: PlannedTarget,
        repository_fingerprint: String,
        evidence_id: String,
    },
    AlreadyApplied {
        node_id: ExecutionNodeId,
        target: PlannedTarget,
        repository_fingerprint: String,
    },
    RecoverableFailure {
        failure: FailureRecord,
    },
    BlockingFailure {
        failure: FailureRecord,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    ModelArtifactRecoverable,
    #[default]
    ToolRecoverable,
    MutationConflict,
    TargetBlocked,
    ValidationFailure,
    InfrastructureFailure,
    OrchestrationInvariantViolation,
    UserCancellation,
}

impl FailureCategory {
    pub const fn creates_repair_work(self) -> bool {
        matches!(
            self,
            Self::ModelArtifactRecoverable
                | Self::ToolRecoverable
                | Self::MutationConflict
                | Self::TargetBlocked
                | Self::ValidationFailure
        )
    }

    pub const fn is_infrastructure(self) -> bool {
        matches!(self, Self::InfrastructureFailure)
    }

    /// Only failures caused by a repository mutation/tool conflict may be
    /// inferred obsolete from a later successful write. Validation,
    /// infrastructure, invariant, cancellation, and semantic blocker failures
    /// require their own explicit recovery event.
    pub const fn is_supersedable_by_applied_target(self) -> bool {
        matches!(self, Self::ToolRecoverable | Self::MutationConflict)
    }

    const fn node_status(self) -> ExecutionNodeStatus {
        match self {
            Self::ModelArtifactRecoverable
            | Self::ToolRecoverable
            | Self::MutationConflict
            | Self::ValidationFailure => ExecutionNodeStatus::FailedRecoverable,
            Self::TargetBlocked
            | Self::InfrastructureFailure
            | Self::OrchestrationInvariantViolation
            | Self::UserCancellation => ExecutionNodeStatus::FailedBlocking,
        }
    }

    const fn is_valid_for_node_kind(self, kind: ExecutionNodeKind) -> bool {
        match self {
            Self::MutationConflict | Self::TargetBlocked => kind.is_mutation(),
            Self::ValidationFailure => kind.is_validation(),
            Self::ModelArtifactRecoverable => kind.requires_model(),
            Self::ToolRecoverable => matches!(
                kind,
                ExecutionNodeKind::Discovery
                    | ExecutionNodeKind::Planning
                    | ExecutionNodeKind::SourceMutation
                    | ExecutionNodeKind::TestMutation
                    | ExecutionNodeKind::ValidationFocused
                    | ExecutionNodeKind::ValidationSuite
                    | ExecutionNodeKind::ValidationBuild
                    | ExecutionNodeKind::ValidationLint
            ),
            Self::InfrastructureFailure
            | Self::OrchestrationInvariantViolation
            | Self::UserCancellation => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStatus {
    #[default]
    Active,
    Recovered,
    Superseded,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct FailureRecord {
    pub id: FailureId,
    pub node_id: ExecutionNodeId,
    pub target_path: Option<String>,
    pub category: FailureCategory,
    pub status: FailureStatus,
    /// Compatibility flags are serialized explicitly while `status` remains
    /// canonical. Constructors and store methods keep all three in sync.
    #[serde(default)]
    pub recovered: bool,
    #[serde(default)]
    pub superseded: bool,
    pub attempt: u32,
    pub repository_fingerprint: String,
    pub message: String,
    #[serde(default)]
    pub resolved_repository_fingerprint: Option<String>,
}

impl FailureRecord {
    pub fn new(
        id: impl Into<FailureId>,
        node_id: impl Into<ExecutionNodeId>,
        category: FailureCategory,
        attempt: u32,
        repository_fingerprint: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            node_id: node_id.into(),
            category,
            attempt,
            repository_fingerprint: repository_fingerprint.into(),
            message: message.into(),
            ..Self::default()
        }
    }

    pub fn is_unresolved(&self) -> bool {
        self.status == FailureStatus::Active && !self.recovered && !self.superseded
    }

    pub fn mark_recovered(&mut self, repository_fingerprint: impl Into<String>) {
        self.status = FailureStatus::Recovered;
        self.recovered = true;
        self.superseded = false;
        self.resolved_repository_fingerprint = Some(repository_fingerprint.into());
    }

    pub fn mark_superseded(&mut self, repository_fingerprint: impl Into<String>) {
        self.status = FailureStatus::Superseded;
        self.recovered = false;
        self.superseded = true;
        self.resolved_repository_fingerprint = Some(repository_fingerprint.into());
    }

    pub fn normalize_compatibility_flags(&mut self) {
        match self.status {
            FailureStatus::Active => {
                if self.superseded {
                    self.status = FailureStatus::Superseded;
                    self.recovered = false;
                } else if self.recovered {
                    self.status = FailureStatus::Recovered;
                }
            }
            FailureStatus::Recovered => {
                self.recovered = true;
                self.superseded = false;
            }
            FailureStatus::Superseded => {
                self.recovered = false;
                self.superseded = true;
            }
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct FailureStore {
    #[serde(default)]
    pub records: Vec<FailureRecord>,
}

impl FailureStore {
    pub fn record(&mut self, mut failure: FailureRecord) -> FailureId {
        failure.normalize_compatibility_flags();
        let id = failure.id.clone();
        if let Some(existing) = self.records.iter_mut().find(|record| record.id == id) {
            *existing = failure;
        } else {
            self.records.push(failure);
        }
        id
    }

    pub fn get(&self, id: &FailureId) -> Option<&FailureRecord> {
        self.records.iter().find(|failure| &failure.id == id)
    }

    pub fn get_mut(&mut self, id: &FailureId) -> Option<&mut FailureRecord> {
        self.records.iter_mut().find(|failure| &failure.id == id)
    }

    pub fn unresolved(&self) -> impl Iterator<Item = &FailureRecord> {
        self.records
            .iter()
            .filter(|failure| failure.is_unresolved())
    }

    pub fn unresolved_for_node(
        &self,
        node_id: &ExecutionNodeId,
    ) -> impl Iterator<Item = &FailureRecord> {
        self.unresolved()
            .filter(move |failure| &failure.node_id == node_id)
    }

    pub fn has_unresolved(&self) -> bool {
        self.unresolved().next().is_some()
    }

    pub fn has_unresolved_for_node(&self, node_id: &ExecutionNodeId) -> bool {
        self.unresolved_for_node(node_id).next().is_some()
    }

    pub fn mark_recovered(
        &mut self,
        id: &FailureId,
        repository_fingerprint: impl Into<String>,
    ) -> bool {
        let Some(failure) = self.get_mut(id) else {
            return false;
        };
        failure.mark_recovered(repository_fingerprint);
        true
    }

    pub fn mark_superseded(
        &mut self,
        id: &FailureId,
        repository_fingerprint: impl Into<String>,
    ) -> bool {
        let Some(failure) = self.get_mut(id) else {
            return false;
        };
        failure.mark_superseded(repository_fingerprint);
        true
    }

    /// Supersedes every unresolved failure for the applied node or target. This
    /// covers duplicate requests and later successful mutations of the same path.
    pub fn supersede_for_applied_target(
        &mut self,
        node_id: &ExecutionNodeId,
        target_path: &str,
        repository_fingerprint: &str,
    ) -> Vec<FailureId> {
        let mut superseded = Vec::new();
        for failure in self.records.iter_mut().filter(|failure| {
            failure.is_unresolved()
                && failure.category.is_supersedable_by_applied_target()
                && (&failure.node_id == node_id
                    || failure.target_path.as_deref() == Some(target_path))
        }) {
            failure.mark_superseded(repository_fingerprint.to_owned());
            superseded.push(failure.id.clone());
        }
        superseded
    }

    /// Reconciles failures against any authoritative predicate, such as final
    /// diff inspection proving that an intended target change is present.
    pub fn supersede_where<F>(
        &mut self,
        repository_fingerprint: &str,
        mut intended_change_is_present: F,
    ) -> Vec<FailureId>
    where
        F: FnMut(&FailureRecord) -> bool,
    {
        let mut superseded = Vec::new();
        for failure in self.records.iter_mut().filter(|failure| {
            failure.is_unresolved() && failure.category.is_supersedable_by_applied_target()
        }) {
            if intended_change_is_present(failure) {
                failure.mark_superseded(repository_fingerprint.to_owned());
                superseded.push(failure.id.clone());
            }
        }
        superseded
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct NodeBudgetUsage {
    pub model_calls: u32,
    pub cost_micros: u64,
    #[serde(with = "duration_millis")]
    pub duration: Duration,
    pub repair_attempts: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ProgressWindow {
    pub max_model_calls_without_progress: u32,
    #[serde(with = "duration_millis")]
    pub max_duration_without_progress: Duration,
}

impl Default for ProgressWindow {
    fn default() -> Self {
        Self {
            max_model_calls_without_progress: 3,
            max_duration_without_progress: Duration::from_secs(3 * 60),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEventKind {
    NewRelevantEvidenceRecorded,
    PlanAccepted,
    NodeMadeReady,
    SourceMutationApplied,
    TestMutationApplied,
    FailureSuperseded,
    FailureRepaired,
    ValidationPassed,
    DiffReviewed,
    CriterionEvidenced,
    CommitCreated,
    PullRequestCreated,
}

impl ProgressEventKind {
    pub const fn score(self) -> u32 {
        match self {
            Self::NewRelevantEvidenceRecorded | Self::NodeMadeReady => 1,
            Self::FailureSuperseded | Self::CriterionEvidenced => 3,
            Self::PlanAccepted | Self::DiffReviewed => 4,
            Self::FailureRepaired | Self::CommitCreated => 5,
            Self::ValidationPassed => 6,
            Self::TestMutationApplied => 7,
            Self::SourceMutationApplied => 8,
            Self::PullRequestCreated => 10,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ProgressEvent {
    pub sequence: u64,
    pub kind: ProgressEventKind,
    pub node_id: Option<ExecutionNodeId>,
    pub model_calls_at_event: u32,
    pub cost_micros_at_event: u64,
    #[serde(with = "duration_millis")]
    pub elapsed_at_event: Duration,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct BudgetState {
    pub mission: MissionBudget,
    pub total_model_calls: u32,
    pub total_cost_micros: u64,
    #[serde(with = "duration_millis")]
    pub elapsed: Duration,
    #[serde(default)]
    pub node_usage: BTreeMap<ExecutionNodeId, NodeBudgetUsage>,
    #[serde(default)]
    pub progress_events: Vec<ProgressEvent>,
    pub progress_score: u64,
    pub progress_window: ProgressWindow,
}

impl Default for BudgetState {
    fn default() -> Self {
        Self::new(MissionBudget::default())
    }
}

impl BudgetState {
    pub fn new(mission: MissionBudget) -> Self {
        Self {
            mission,
            total_model_calls: 0,
            total_cost_micros: 0,
            elapsed: Duration::ZERO,
            node_usage: BTreeMap::new(),
            progress_events: Vec::new(),
            progress_score: 0,
            progress_window: ProgressWindow::default(),
        }
    }

    pub fn usage_for(&self, node_id: &ExecutionNodeId) -> NodeBudgetUsage {
        self.node_usage.get(node_id).cloned().unwrap_or_default()
    }

    pub fn remaining_for(
        &self,
        node_id: &ExecutionNodeId,
        node_budget: &NodeBudget,
    ) -> NodeBudgetRemaining {
        node_budget.remaining(&self.usage_for(node_id))
    }

    pub fn can_start_model_call(
        &self,
        node_id: &ExecutionNodeId,
        node_budget: &NodeBudget,
    ) -> bool {
        self.can_spend_model_call(node_id, node_budget, 0, Duration::ZERO)
    }

    pub fn can_start_model_call_for(&self, node: &ExecutionNode) -> bool {
        self.can_start_model_call(&node.id, &node.budget)
    }

    pub fn can_spend_model_call(
        &self,
        node_id: &ExecutionNodeId,
        node_budget: &NodeBudget,
        estimated_cost_micros: u64,
        estimated_duration: Duration,
    ) -> bool {
        let usage = self.usage_for(node_id);
        self.total_model_calls < self.mission.max_model_calls
            && self.total_cost_micros.saturating_add(estimated_cost_micros)
                <= self.mission.max_cost_micros
            && self.elapsed.saturating_add(estimated_duration) <= self.mission.max_duration
            && usage.model_calls < node_budget.max_model_calls
            && usage.cost_micros.saturating_add(estimated_cost_micros)
                <= node_budget.max_cost_micros
            && usage.duration.saturating_add(estimated_duration) <= node_budget.max_duration
    }

    pub fn record_model_call(
        &mut self,
        node_id: ExecutionNodeId,
        cost_micros: u64,
        duration: Duration,
    ) {
        self.total_model_calls = self.total_model_calls.saturating_add(1);
        self.total_cost_micros = self.total_cost_micros.saturating_add(cost_micros);
        self.elapsed = self.elapsed.saturating_add(duration);
        let usage = self.node_usage.entry(node_id).or_default();
        usage.model_calls = usage.model_calls.saturating_add(1);
        usage.cost_micros = usage.cost_micros.saturating_add(cost_micros);
        usage.duration = usage.duration.saturating_add(duration);
    }

    /// Accounts for bounded node work that does not invoke a model, such as a
    /// worker-owned validation command. Validation therefore consumes the same
    /// node and mission duration envelopes as model-backed work.
    pub fn record_node_duration(&mut self, node_id: ExecutionNodeId, duration: Duration) {
        self.elapsed = self.elapsed.saturating_add(duration);
        let usage = self.node_usage.entry(node_id).or_default();
        usage.duration = usage.duration.saturating_add(duration);
    }

    pub fn record_repair_attempt(&mut self, node_id: ExecutionNodeId) {
        let usage = self.node_usage.entry(node_id).or_default();
        usage.repair_attempts = usage.repair_attempts.saturating_add(1);
    }

    pub fn record_progress(&mut self, event: ProgressEvent) {
        // Event-sequence deduplication makes resume/replay idempotent.
        if self
            .progress_events
            .iter()
            .any(|existing| existing.sequence == event.sequence)
        {
            return;
        }
        self.progress_score = self
            .progress_score
            .saturating_add(u64::from(event.kind.score()));
        self.progress_events.push(event);
        self.progress_events.sort_by_key(|event| event.sequence);
    }

    pub fn record_progress_kind(
        &mut self,
        sequence: u64,
        kind: ProgressEventKind,
        node_id: Option<ExecutionNodeId>,
    ) {
        self.record_progress(ProgressEvent {
            sequence,
            kind,
            node_id,
            model_calls_at_event: self.total_model_calls,
            cost_micros_at_event: self.total_cost_micros,
            elapsed_at_event: self.elapsed,
        });
    }

    /// Stops immediately at a hard bound. At the deterministic 80% soft bound,
    /// it stops only after the configured call/time window produced no progress.
    pub fn should_stop_node(&self, node_id: &ExecutionNodeId, node_budget: &NodeBudget) -> bool {
        let usage = self.usage_for(node_id);
        let hard_exceeded = (node_budget.max_model_calls > 0
            && usage.model_calls >= node_budget.max_model_calls)
            || (node_budget.max_cost_micros > 0
                && usage.cost_micros >= node_budget.max_cost_micros)
            || (!node_budget.max_duration.is_zero() && usage.duration >= node_budget.max_duration)
            || usage.repair_attempts > node_budget.max_repair_attempts
            || self.total_model_calls >= self.mission.max_model_calls
            || self.total_cost_micros >= self.mission.max_cost_micros
            || self.elapsed >= self.mission.max_duration;
        if hard_exceeded {
            return true;
        }

        let soft_exceeded = ratio_at_least(usage.model_calls, node_budget.max_model_calls, 80)
            || ratio_at_least(usage.cost_micros, node_budget.max_cost_micros, 80)
            || duration_ratio_at_least(usage.duration, node_budget.max_duration, 80);
        if !soft_exceeded {
            return false;
        }

        let last_progress = self
            .progress_events
            .iter()
            .rev()
            .find(|event| event.node_id.as_ref() == Some(node_id));
        let calls_at_progress = last_progress.map_or(0, |event| event.model_calls_at_event);
        let elapsed_at_progress =
            last_progress.map_or(Duration::ZERO, |event| event.elapsed_at_event);
        self.total_model_calls.saturating_sub(calls_at_progress)
            >= self.progress_window.max_model_calls_without_progress
            || self.elapsed.saturating_sub(elapsed_at_progress)
                >= self.progress_window.max_duration_without_progress
    }
}

fn ratio_at_least(value: impl Into<u64>, maximum: impl Into<u64>, percent: u64) -> bool {
    let value = value.into();
    let maximum = maximum.into();
    maximum > 0 && value.saturating_mul(100) >= maximum.saturating_mul(percent)
}

fn duration_ratio_at_least(value: Duration, maximum: Duration, percent: u64) -> bool {
    let value = u64::try_from(value.as_millis()).unwrap_or(u64::MAX);
    let maximum = u64::try_from(maximum.as_millis()).unwrap_or(u64::MAX);
    ratio_at_least(value, maximum, percent)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationMode {
    #[default]
    Normal,
    NormalWithExternalReview,
    Draft,
    DraftRecovery,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationStatus {
    #[default]
    NotStarted,
    InProgress,
    CommitCreated,
    BranchPushed,
    PullRequestCreated,
    Failed,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct PublicationState {
    pub status: PublicationStatus,
    pub mode: Option<PublicationMode>,
    pub commit_sha: Option<String>,
    pub branch: Option<String>,
    pub pull_request_url: Option<String>,
    pub pull_request_number: Option<u64>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub recovery_requested: bool,
}

impl PublicationState {
    pub fn is_published(&self) -> bool {
        self.status == PublicationStatus::PullRequestCreated
            && self.commit_sha.is_some()
            && self.branch.is_some()
            && self.pull_request_url.is_some()
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct CancellationState {
    pub requested_at: String,
    pub reason: String,
    pub requested_by: Option<String>,
    #[serde(default)]
    pub active_validation_terminated: bool,
    #[serde(default)]
    pub checkpointed: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionOutcome {
    Complete,
    CompletePendingExternalReview,
    PartialReviewable,
    BlockedNoDiff,
    FailedInfrastructure,
    Cancelled,
}

impl MissionOutcome {
    pub const fn publication_mode(self) -> Option<PublicationMode> {
        match self {
            Self::Complete => Some(PublicationMode::Normal),
            Self::CompletePendingExternalReview => Some(PublicationMode::NormalWithExternalReview),
            Self::PartialReviewable => Some(PublicationMode::Draft),
            Self::BlockedNoDiff | Self::FailedInfrastructure | Self::Cancelled => None,
        }
    }

    pub const fn is_successful_domain_result(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::CompletePendingExternalReview | Self::PartialReviewable
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailReason {
    MissionBudgetExhausted,
    NodeBudgetExhausted,
    NoProgress,
    BlockingFailure,
    InfrastructureFailure,
    OrchestrationInvariantViolation,
    Cancellation,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum ExecutionDomainEvent {
    DiscoveryStarted {
        sequence: u64,
    },
    RepositoryEvidenceRecorded {
        sequence: u64,
        evidence_id: String,
        repository_fingerprint: String,
        /// New checkpoints carry the complete immutable observation so event
        /// replay reconstructs the EvidenceStore. Older checkpoints omit it
        /// and retain their already-materialized store for compatibility.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<FileEvidence>,
    },
    DiscoveryCompleted {
        sequence: u64,
        repository_fingerprint: String,
    },
    ComplexityClassified {
        sequence: u64,
        assessment: ComplexityAssessment,
    },
    PlanAccepted {
        sequence: u64,
        target_count: u32,
    },
    PlanRepaired {
        sequence: u64,
        repaired_criterion_ids: Vec<String>,
    },
    GraphCreated {
        sequence: u64,
        graph_id: String,
        revision: u64,
        /// Carries the authoritative topology for append-only replay. Legacy
        /// checkpoints may omit it because they already persist a materialized
        /// graph alongside the event stream.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph: Option<ExecutionGraph>,
        /// Exact semantic identities retained from the previous topology.
        /// Stores and budget usage are reduced against this set during replay.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        preserved_node_ids: Vec<ExecutionNodeId>,
    },
    NodeStarted {
        sequence: u64,
        node_id: ExecutionNodeId,
        attempt: u32,
        started_at: String,
        repository_fingerprint: String,
    },
    MutationApplied {
        sequence: u64,
        node_id: ExecutionNodeId,
        target_path: String,
        repository_fingerprint: String,
        evidence_id: String,
    },
    MutationRejected {
        sequence: u64,
        node_id: ExecutionNodeId,
        failure: FailureRecord,
    },
    MutationSuperseded {
        sequence: u64,
        node_id: ExecutionNodeId,
        failure_id: FailureId,
        repository_fingerprint: String,
    },
    /// Records a failure for any execution node. Mutation-specific callers may
    /// continue to use `MutationRejected`; this variant exists for discovery,
    /// planning, validation, review, publication, and infrastructure failures
    /// whose full state must be reconstructible from the event stream.
    FailureRecorded {
        sequence: u64,
        failure: FailureRecord,
    },
    FailureRecovered {
        sequence: u64,
        node_id: ExecutionNodeId,
        failure_id: FailureId,
        repository_fingerprint: String,
    },
    FailureSuperseded {
        sequence: u64,
        node_id: ExecutionNodeId,
        failure_id: FailureId,
        repository_fingerprint: String,
    },
    NodeCompleted {
        sequence: u64,
        node_id: ExecutionNodeId,
        status: ExecutionNodeStatus,
    },
    ValidationStarted {
        sequence: u64,
        node_id: ExecutionNodeId,
        fingerprint: String,
    },
    ValidationEvidenceRecorded {
        sequence: u64,
        node_id: ExecutionNodeId,
        evidence: ValidationEvidenceRecord,
    },
    ValidationPassed {
        sequence: u64,
        node_id: ExecutionNodeId,
        evidence_id: String,
        fingerprint: String,
    },
    ValidationFailed {
        sequence: u64,
        node_id: ExecutionNodeId,
        failure_id: FailureId,
        fingerprint: String,
    },
    ValidationSuperseded {
        sequence: u64,
        node_id: ExecutionNodeId,
        evidence_id: String,
        repository_fingerprint: String,
    },
    /// Invalidates all finalization derived from an earlier repository state.
    /// The evidence ids are the canonical, complete set of validation passes
    /// invalidated by this repository observation.
    FinalizationInvalidated {
        sequence: u64,
        repository_fingerprint: String,
        stale_validation_evidence_ids: Vec<String>,
    },
    DiffReviewed {
        sequence: u64,
        node_id: ExecutionNodeId,
        evidence_ids: Vec<String>,
    },
    CompletionEvaluated {
        sequence: u64,
        node_id: ExecutionNodeId,
        outcome: MissionOutcome,
    },
    /// Authorizes a draft recovery publication from current validation proof
    /// without claiming that diff review or completion evaluation succeeded.
    RecoveryPublicationRequested {
        sequence: u64,
        node_id: ExecutionNodeId,
        repository_fingerprint: String,
        validation_evidence_ids: Vec<String>,
    },
    PublicationStarted {
        sequence: u64,
        node_id: ExecutionNodeId,
        mode: PublicationMode,
    },
    CommitCreated {
        sequence: u64,
        node_id: ExecutionNodeId,
        commit_sha: String,
    },
    BranchPushed {
        sequence: u64,
        node_id: ExecutionNodeId,
        branch: String,
    },
    PullRequestCreated {
        sequence: u64,
        node_id: ExecutionNodeId,
        url: String,
        number: Option<u64>,
        draft: bool,
    },
    GuardrailTriggered {
        sequence: u64,
        reason: GuardrailReason,
        outcome: MissionOutcome,
        detail: String,
    },
    CancellationRequested {
        sequence: u64,
        state: CancellationState,
    },
    /// Starts a newer execution attempt from a resumable cancellation
    /// checkpoint. The reducer, rather than startup compatibility code,
    /// clears the canonical cancellation state.
    ExecutionResumed {
        sequence: u64,
        execution_attempt: u32,
        /// A prior partial terminal outcome starts a new continuation epoch;
        /// cancellation-only resumes leave this empty.
        previous_outcome: Option<MissionOutcome>,
    },
    RunFinished {
        sequence: u64,
        outcome: MissionOutcome,
    },
}

impl ExecutionDomainEvent {
    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::DiscoveryStarted { .. } => "discovery_started",
            Self::RepositoryEvidenceRecorded { .. } => "repository_evidence_recorded",
            Self::DiscoveryCompleted { .. } => "discovery_completed",
            Self::ComplexityClassified { .. } => "complexity_classified",
            Self::PlanAccepted { .. } => "plan_accepted",
            Self::PlanRepaired { .. } => "plan_repaired",
            Self::GraphCreated { .. } => "graph_created",
            Self::NodeStarted { .. } => "node_started",
            Self::MutationApplied { .. } => "mutation_applied",
            Self::MutationRejected { .. } => "mutation_rejected",
            Self::MutationSuperseded { .. } => "mutation_superseded",
            Self::FailureRecorded { .. } => "failure_recorded",
            Self::FailureRecovered { .. } => "failure_recovered",
            Self::FailureSuperseded { .. } => "failure_superseded",
            Self::NodeCompleted { .. } => "node_completed",
            Self::ValidationStarted { .. } => "validation_started",
            Self::ValidationEvidenceRecorded { .. } => "validation_evidence_recorded",
            Self::ValidationPassed { .. } => "validation_passed",
            Self::ValidationFailed { .. } => "validation_failed",
            Self::ValidationSuperseded { .. } => "validation_superseded",
            Self::FinalizationInvalidated { .. } => "finalization_invalidated",
            Self::DiffReviewed { .. } => "diff_reviewed",
            Self::CompletionEvaluated { .. } => "completion_evaluated",
            Self::RecoveryPublicationRequested { .. } => "recovery_publication_requested",
            Self::PublicationStarted { .. } => "publication_started",
            Self::CommitCreated { .. } => "commit_created",
            Self::BranchPushed { .. } => "branch_pushed",
            Self::PullRequestCreated { .. } => "pull_request_created",
            Self::GuardrailTriggered { .. } => "guardrail_triggered",
            Self::CancellationRequested { .. } => "cancellation_requested",
            Self::ExecutionResumed { .. } => "execution_resumed",
            Self::RunFinished { .. } => "run_finished",
        }
    }

    pub const fn sequence(&self) -> u64 {
        match self {
            Self::DiscoveryStarted { sequence }
            | Self::RepositoryEvidenceRecorded { sequence, .. }
            | Self::DiscoveryCompleted { sequence, .. }
            | Self::ComplexityClassified { sequence, .. }
            | Self::PlanAccepted { sequence, .. }
            | Self::PlanRepaired { sequence, .. }
            | Self::GraphCreated { sequence, .. }
            | Self::NodeStarted { sequence, .. }
            | Self::MutationApplied { sequence, .. }
            | Self::MutationRejected { sequence, .. }
            | Self::MutationSuperseded { sequence, .. }
            | Self::FailureRecorded { sequence, .. }
            | Self::FailureRecovered { sequence, .. }
            | Self::FailureSuperseded { sequence, .. }
            | Self::NodeCompleted { sequence, .. }
            | Self::ValidationStarted { sequence, .. }
            | Self::ValidationEvidenceRecorded { sequence, .. }
            | Self::ValidationPassed { sequence, .. }
            | Self::ValidationFailed { sequence, .. }
            | Self::ValidationSuperseded { sequence, .. }
            | Self::FinalizationInvalidated { sequence, .. }
            | Self::DiffReviewed { sequence, .. }
            | Self::CompletionEvaluated { sequence, .. }
            | Self::RecoveryPublicationRequested { sequence, .. }
            | Self::PublicationStarted { sequence, .. }
            | Self::CommitCreated { sequence, .. }
            | Self::BranchPushed { sequence, .. }
            | Self::PullRequestCreated { sequence, .. }
            | Self::GuardrailTriggered { sequence, .. }
            | Self::CancellationRequested { sequence, .. }
            | Self::ExecutionResumed { sequence, .. }
            | Self::RunFinished { sequence, .. } => *sequence,
        }
    }

    pub fn node_id(&self) -> Option<&ExecutionNodeId> {
        match self {
            Self::NodeStarted { node_id, .. }
            | Self::MutationApplied { node_id, .. }
            | Self::MutationRejected { node_id, .. }
            | Self::MutationSuperseded { node_id, .. }
            | Self::FailureRecovered { node_id, .. }
            | Self::FailureSuperseded { node_id, .. }
            | Self::NodeCompleted { node_id, .. }
            | Self::ValidationStarted { node_id, .. }
            | Self::ValidationEvidenceRecorded { node_id, .. }
            | Self::ValidationPassed { node_id, .. }
            | Self::ValidationFailed { node_id, .. }
            | Self::ValidationSuperseded { node_id, .. }
            | Self::DiffReviewed { node_id, .. }
            | Self::CompletionEvaluated { node_id, .. }
            | Self::RecoveryPublicationRequested { node_id, .. }
            | Self::PublicationStarted { node_id, .. }
            | Self::CommitCreated { node_id, .. }
            | Self::BranchPushed { node_id, .. }
            | Self::PullRequestCreated { node_id, .. } => Some(node_id),
            Self::FailureRecorded { failure, .. } => Some(&failure.node_id),
            _ => None,
        }
    }

    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::RunFinished { .. })
    }
}

/// Returns only events in the active execution epoch. `ExecutionResumed`
/// starts a new epoch, so terminal and guardrail decisions from a published
/// partial attempt cannot suppress decisions in its continuation.
pub fn current_execution_epoch(events: &[ExecutionDomainEvent]) -> &[ExecutionDomainEvent] {
    let start = events
        .iter()
        .rposition(|event| matches!(event, ExecutionDomainEvent::ExecutionResumed { .. }))
        .map_or(0, |position| position.saturating_add(1));
    &events[start..]
}

pub fn current_epoch_terminal_outcome(events: &[ExecutionDomainEvent]) -> Option<MissionOutcome> {
    current_execution_epoch(events)
        .iter()
        .rev()
        .find_map(|event| match event {
            ExecutionDomainEvent::RunFinished { outcome, .. } => Some(*outcome),
            _ => None,
        })
}

impl ExecutionGraph {
    pub fn apply_domain_event(
        &mut self,
        event: &ExecutionDomainEvent,
    ) -> Result<(), GraphInvariantError> {
        self.apply_domain_event_with_dependency_satisfaction(event, &BTreeSet::new())
    }

    fn apply_domain_event_with_dependency_satisfaction(
        &mut self,
        event: &ExecutionDomainEvent,
        additionally_satisfied: &BTreeSet<ExecutionNodeId>,
    ) -> Result<(), GraphInvariantError> {
        let satisfied = self.dependency_satisfaction_ids(additionally_satisfied);
        let guarded_node = match event {
            ExecutionDomainEvent::NodeStarted { node_id, .. }
            | ExecutionDomainEvent::MutationApplied { node_id, .. }
            | ExecutionDomainEvent::MutationSuperseded { node_id, .. }
            | ExecutionDomainEvent::FailureSuperseded { node_id, .. }
            | ExecutionDomainEvent::ValidationStarted { node_id, .. }
            | ExecutionDomainEvent::ValidationPassed { node_id, .. }
            | ExecutionDomainEvent::DiffReviewed { node_id, .. }
            | ExecutionDomainEvent::CompletionEvaluated { node_id, .. }
            | ExecutionDomainEvent::PublicationStarted { node_id, .. }
            | ExecutionDomainEvent::CommitCreated { node_id, .. }
            | ExecutionDomainEvent::BranchPushed { node_id, .. }
            | ExecutionDomainEvent::PullRequestCreated { node_id, .. } => Some(node_id),
            ExecutionDomainEvent::NodeCompleted {
                node_id, status, ..
            } if status.satisfies_dependency() && *status != ExecutionNodeStatus::Skipped => {
                Some(node_id)
            }
            _ => None,
        };
        if let Some(node_id) = guarded_node {
            self.ensure_node_dependencies_satisfied(node_id, &satisfied)?;
        }
        self.validate_event_node_kind(event)?;

        match event {
            ExecutionDomainEvent::NodeStarted {
                node_id,
                attempt,
                started_at,
                repository_fingerprint,
                ..
            } => {
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.status = ExecutionNodeStatus::Running;
                if !node
                    .attempts
                    .iter()
                    .any(|existing| existing.attempt == *attempt)
                {
                    node.attempts.push(NodeAttempt {
                        attempt: *attempt,
                        started_at: started_at.clone(),
                        repository_fingerprint_before: repository_fingerprint.clone(),
                        ..NodeAttempt::default()
                    });
                }
                self.revision = self.revision.saturating_add(1);
            }
            ExecutionDomainEvent::MutationApplied {
                node_id,
                repository_fingerprint,
                evidence_id,
                ..
            } => {
                self.dependency_satisfaction_overrides.remove(node_id);
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.status = ExecutionNodeStatus::Applied;
                if !node.evidence_ids.contains(evidence_id) {
                    node.evidence_ids.push(evidence_id.clone());
                }
                if let Some(attempt) = node.attempts.last_mut() {
                    attempt.repository_fingerprint_after = Some(repository_fingerprint.clone());
                    attempt.outcome = Some(ExecutionNodeStatus::Applied);
                }
                self.revision = self.revision.saturating_add(1);
                self.refresh_readiness();
            }
            ExecutionDomainEvent::MutationRejected {
                node_id, failure, ..
            } => {
                let status = failure.category.node_status();
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.status = status;
                if let Some(attempt) = node.attempts.last_mut() {
                    attempt.outcome = Some(status);
                    attempt.failure_id = Some(failure.id.clone());
                }
                self.revision = self.revision.saturating_add(1);
            }
            ExecutionDomainEvent::MutationSuperseded { node_id, .. } => {
                self.set_node_status(node_id, ExecutionNodeStatus::Applied)?;
            }
            ExecutionDomainEvent::FailureRecorded { failure, .. } => {
                self.set_node_status(&failure.node_id, failure.category.node_status())?;
                if failure.category == FailureCategory::ValidationFailure
                    && let Some(target_id) = failure.target_path.as_deref().and_then(|path| {
                        self.unique_mutation_node_for_target_path(path)
                            .map(|node| node.id.clone())
                    })
                    && self
                        .node(&target_id)
                        .is_some_and(|node| node.status.is_success())
                {
                    self.set_node_status(&target_id, ExecutionNodeStatus::FailedRecoverable)?;
                }
            }
            ExecutionDomainEvent::FailureRecovered { node_id, .. } => {
                let reset_validation_evidence = self
                    .node(node_id)
                    .is_some_and(|node| node.kind.is_validation());
                if let Some(node) = self.node_mut(node_id)
                    && matches!(
                        node.status,
                        ExecutionNodeStatus::FailedRecoverable
                            | ExecutionNodeStatus::FailedBlocking
                    )
                {
                    node.status = ExecutionNodeStatus::Pending;
                    if reset_validation_evidence {
                        node.evidence_ids.clear();
                    }
                    self.revision = self.revision.saturating_add(1);
                    self.refresh_readiness();
                }
            }
            ExecutionDomainEvent::FailureSuperseded { node_id, .. } => {
                self.set_node_status(node_id, ExecutionNodeStatus::Superseded)?;
            }
            ExecutionDomainEvent::NodeCompleted {
                node_id, status, ..
            } => {
                self.set_node_status(node_id, *status)?;
            }
            ExecutionDomainEvent::ValidationStarted { node_id, .. } => {
                self.set_node_status(node_id, ExecutionNodeStatus::Running)?;
            }
            ExecutionDomainEvent::ValidationEvidenceRecorded {
                node_id, evidence, ..
            } => {
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                if !node.evidence_ids.contains(&evidence.evidence_id) {
                    node.evidence_ids.push(evidence.evidence_id.clone());
                    self.revision = self.revision.saturating_add(1);
                }
            }
            ExecutionDomainEvent::ValidationPassed {
                node_id,
                evidence_id,
                ..
            } => {
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.status = ExecutionNodeStatus::Passed;
                if !node.evidence_ids.contains(evidence_id) {
                    node.evidence_ids.push(evidence_id.clone());
                }
                self.revision = self.revision.saturating_add(1);
                self.refresh_readiness();
            }
            ExecutionDomainEvent::ValidationFailed { node_id, .. } => {
                let status = if self
                    .node(node_id)
                    .is_some_and(|node| node.status == ExecutionNodeStatus::FailedBlocking)
                {
                    ExecutionNodeStatus::FailedBlocking
                } else {
                    ExecutionNodeStatus::FailedRecoverable
                };
                self.set_node_status(node_id, status)?;
            }
            ExecutionDomainEvent::ValidationSuperseded {
                node_id,
                evidence_id,
                ..
            } => {
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.evidence_ids.retain(|id| id != evidence_id);
                node.status = ExecutionNodeStatus::Pending;
                self.revision = self.revision.saturating_add(1);
                self.refresh_readiness();
            }
            ExecutionDomainEvent::FinalizationInvalidated { .. } => {
                self.recovery_publication_dependency_override = false;
                for node in &mut self.nodes {
                    if node.kind.is_validation()
                        || matches!(
                            node.kind,
                            ExecutionNodeKind::DiffReview
                                | ExecutionNodeKind::CompletionEvaluation
                                | ExecutionNodeKind::Publication
                        )
                    {
                        node.status = ExecutionNodeStatus::Pending;
                        node.evidence_ids.clear();
                    }
                }
                self.revision = self.revision.saturating_add(1);
                self.refresh_readiness();
            }
            ExecutionDomainEvent::DiffReviewed {
                node_id,
                evidence_ids,
                ..
            } => {
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.status = ExecutionNodeStatus::Completed;
                for evidence_id in evidence_ids {
                    if !node.evidence_ids.contains(evidence_id) {
                        node.evidence_ids.push(evidence_id.clone());
                    }
                }
                self.revision = self.revision.saturating_add(1);
                self.refresh_readiness();
            }
            ExecutionDomainEvent::CompletionEvaluated { node_id, .. } => {
                self.set_node_status(node_id, ExecutionNodeStatus::Completed)?;
            }
            ExecutionDomainEvent::RecoveryPublicationRequested { node_id, .. } => {
                self.recovery_publication_dependency_override = true;
                let node = self.node_mut(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                node.status = ExecutionNodeStatus::Running;
                self.revision = self.revision.saturating_add(1);
                self.refresh_readiness();
            }
            ExecutionDomainEvent::PublicationStarted { node_id, .. } => {
                self.recovery_publication_dependency_override = false;
                self.set_node_status(node_id, ExecutionNodeStatus::Running)?;
            }
            ExecutionDomainEvent::PullRequestCreated { node_id, .. } => {
                self.set_node_status(node_id, ExecutionNodeStatus::Completed)?;
            }
            ExecutionDomainEvent::DiscoveryStarted { .. } => {
                if let Some(id) = self
                    .nodes
                    .iter()
                    .find(|node| node.kind == ExecutionNodeKind::Discovery)
                    .map(|node| node.id.clone())
                {
                    self.set_node_status(&id, ExecutionNodeStatus::Running)?;
                }
            }
            ExecutionDomainEvent::DiscoveryCompleted { .. } => {
                if let Some(id) = self
                    .nodes
                    .iter()
                    .find(|node| node.kind == ExecutionNodeKind::Discovery)
                    .map(|node| node.id.clone())
                {
                    self.set_node_status(&id, ExecutionNodeStatus::Completed)?;
                }
            }
            ExecutionDomainEvent::GuardrailTriggered {
                outcome: MissionOutcome::PartialReviewable,
                ..
            } => {
                let mut changed = false;
                for node in self
                    .nodes
                    .iter_mut()
                    .filter(|node| node.kind.is_mutation() && !node.status.satisfies_dependency())
                {
                    changed |= self
                        .dependency_satisfaction_overrides
                        .insert(node.id.clone());
                    if node.status == ExecutionNodeStatus::Running {
                        node.status = ExecutionNodeStatus::Pending;
                        changed = true;
                    }
                }
                if changed {
                    self.revision = self.revision.saturating_add(1);
                    self.refresh_readiness();
                }
            }
            ExecutionDomainEvent::ExecutionResumed {
                previous_outcome: Some(MissionOutcome::PartialReviewable),
                ..
            } => {
                let mut changed = !self.dependency_satisfaction_overrides.is_empty()
                    || self.recovery_publication_dependency_override;
                self.dependency_satisfaction_overrides.clear();
                self.recovery_publication_dependency_override = false;
                for node in &mut self.nodes {
                    if node.kind.is_mutation() {
                        if node.status == ExecutionNodeStatus::Running {
                            node.status = ExecutionNodeStatus::Pending;
                            changed = true;
                        }
                        continue;
                    }
                    if node.kind.is_validation()
                        || matches!(
                            node.kind,
                            ExecutionNodeKind::DiffReview
                                | ExecutionNodeKind::CompletionEvaluation
                                | ExecutionNodeKind::Publication
                        )
                    {
                        changed |= node.status != ExecutionNodeStatus::Pending
                            || !node.evidence_ids.is_empty();
                        node.status = ExecutionNodeStatus::Pending;
                        node.evidence_ids.clear();
                    }
                }
                if changed {
                    self.revision = self.revision.saturating_add(1);
                }
                self.refresh_readiness();
            }
            ExecutionDomainEvent::CommitCreated { .. }
            | ExecutionDomainEvent::BranchPushed { .. }
            | ExecutionDomainEvent::RepositoryEvidenceRecorded { .. }
            | ExecutionDomainEvent::ComplexityClassified { .. }
            | ExecutionDomainEvent::PlanAccepted { .. }
            | ExecutionDomainEvent::PlanRepaired { .. }
            | ExecutionDomainEvent::GraphCreated { .. }
            | ExecutionDomainEvent::GuardrailTriggered { .. }
            | ExecutionDomainEvent::CancellationRequested { .. }
            | ExecutionDomainEvent::ExecutionResumed { .. }
            | ExecutionDomainEvent::RunFinished { .. } => {}
        }
        Ok(())
    }

    fn validate_event_node_kind(
        &self,
        event: &ExecutionDomainEvent,
    ) -> Result<(), GraphInvariantError> {
        let Some(node_id) = event.node_id() else {
            return Ok(());
        };
        let node = self.node(node_id).ok_or_else(|| {
            GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
        })?;
        let kind_matches = match event {
            ExecutionDomainEvent::MutationApplied { .. }
            | ExecutionDomainEvent::MutationRejected { .. }
            | ExecutionDomainEvent::MutationSuperseded { .. } => node.kind.is_mutation(),
            ExecutionDomainEvent::FailureRecorded { failure, .. } => {
                failure.category.is_valid_for_node_kind(node.kind)
            }
            ExecutionDomainEvent::FailureSuperseded { .. } => node.kind.is_mutation(),
            ExecutionDomainEvent::ValidationStarted { .. }
            | ExecutionDomainEvent::ValidationEvidenceRecorded { .. }
            | ExecutionDomainEvent::ValidationPassed { .. }
            | ExecutionDomainEvent::ValidationFailed { .. }
            | ExecutionDomainEvent::ValidationSuperseded { .. } => node.kind.is_validation(),
            ExecutionDomainEvent::DiffReviewed { .. } => node.kind == ExecutionNodeKind::DiffReview,
            ExecutionDomainEvent::CompletionEvaluated { .. } => {
                node.kind == ExecutionNodeKind::CompletionEvaluation
            }
            ExecutionDomainEvent::PublicationStarted { .. }
            | ExecutionDomainEvent::RecoveryPublicationRequested { .. }
            | ExecutionDomainEvent::CommitCreated { .. }
            | ExecutionDomainEvent::BranchPushed { .. }
            | ExecutionDomainEvent::PullRequestCreated { .. } => {
                node.kind == ExecutionNodeKind::Publication
            }
            ExecutionDomainEvent::FailureRecovered { .. } => true,
            _ => true,
        };
        if !kind_matches {
            return Err(GraphInvariantError::new(format!(
                "event `{}` is incompatible with node `{node_id}` of kind `{:?}`",
                event.event_type(),
                node.kind
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExecutionSnapshot {
    pub run_id: String,
    pub current_repository: RepositorySnapshot,
    pub graph: ExecutionGraph,
    #[serde(default)]
    pub events: Vec<ExecutionDomainEvent>,
    pub evidence: EvidenceStore,
    pub failures: FailureStore,
    pub budget: BudgetState,
    pub cancellation: Option<CancellationState>,
    pub publication: PublicationState,
}

impl ExecutionSnapshot {
    pub fn stage(&self) -> HostedExecutionStage {
        self.graph.stage()
    }

    pub fn next_event_sequence(&self) -> u64 {
        self.events
            .last()
            .map_or(1, |event| event.sequence().saturating_add(1))
    }

    pub fn terminal_outcome(&self) -> Option<MissionOutcome> {
        current_epoch_terminal_outcome(&self.events)
    }

    pub fn is_terminal(&self) -> bool {
        self.terminal_outcome().is_some()
    }

    pub fn remaining_required_nodes(&self) -> Vec<&ExecutionNode> {
        self.graph.remaining_required_nodes()
    }

    /// Returns the canonical dependency view used by both event application and
    /// reconciliation. Actual success remains distinct from an explicit
    /// partial-review override, so remaining work is never erased.
    pub fn dependency_satisfaction_ids(&self) -> BTreeSet<ExecutionNodeId> {
        let mut satisfied = self
            .graph
            .nodes
            .iter()
            .filter(|node| node.status.satisfies_dependency())
            .map(|node| node.id.clone())
            .chain(self.graph.dependency_satisfaction_overrides.iter().cloned())
            .collect::<BTreeSet<_>>();

        if self.graph.recovery_publication_dependency_override
            && let Some(publication) = self
                .graph
                .nodes
                .iter()
                .find(|node| node.kind == ExecutionNodeKind::Publication)
        {
            satisfied.extend(publication.dependencies.iter().cloned());
        }

        // A validation failure can reopen an already-applied mutation without
        // invalidating later applied mutation nodes. Preserve only dependency
        // lineage while the explicit validation repair remains unresolved;
        // the target itself still appears in remaining work and is selected
        // by the orchestrator for repair.
        satisfied.extend(
            self.failures
                .unresolved()
                .filter(|failure| failure.category == FailureCategory::ValidationFailure)
                .filter_map(|failure| failure.target_path.as_deref())
                .filter_map(|path| self.graph.unique_mutation_node_for_target_path(path))
                .map(|node| node.id.clone()),
        );

        for node in self
            .graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_validation())
        {
            let dependencies_satisfied = node
                .dependencies
                .iter()
                .all(|dependency| satisfied.contains(dependency));
            if dependencies_satisfied
                && !self.failures.has_unresolved_for_node(&node.id)
                && node.validation.as_ref().is_some_and(|gate| {
                    self.evidence.has_passed_validation(
                        &gate.fingerprint(&self.current_repository.fingerprint),
                    )
                })
            {
                satisfied.insert(node.id.clone());
            }
        }

        // Checkpoint compatibility: an older serialized graph may contain the
        // event but predate the explicit override field.
        if self.has_partial_reviewable_guardrail() {
            satisfied.extend(
                self.graph
                    .nodes
                    .iter()
                    .filter(|node| node.kind.is_mutation())
                    .map(|node| node.id.clone()),
            );
        }
        satisfied
    }

    pub fn has_partial_reviewable_guardrail(&self) -> bool {
        current_execution_epoch(&self.events)
            .iter()
            .rev()
            .any(|event| {
                matches!(
                    event,
                    ExecutionDomainEvent::GuardrailTriggered {
                        outcome: MissionOutcome::PartialReviewable,
                        ..
                    }
                )
            })
    }

    /// Returns the deterministic set of current validation proof required to
    /// authorize recovery publication. Every required gate must be represented
    /// by attached, passed evidence for the current repository fingerprint.
    pub fn current_required_validation_evidence_ids(
        &self,
    ) -> Result<Vec<String>, GraphInvariantError> {
        let mut evidence_ids = BTreeSet::new();
        let satisfied = self.dependency_satisfaction_ids();
        for node in self
            .graph
            .nodes
            .iter()
            .filter(|node| node.required && node.kind.is_validation())
        {
            if self.failures.has_unresolved_for_node(&node.id) {
                return Err(GraphInvariantError::new(format!(
                    "required validation node `{}` has an unresolved failure",
                    node.id
                )));
            }
            if !node
                .dependencies
                .iter()
                .all(|dependency| satisfied.contains(dependency))
            {
                return Err(GraphInvariantError::new(format!(
                    "required validation node `{}` has unsatisfied dependencies",
                    node.id
                )));
            }
            let gate = node.validation.as_ref().ok_or_else(|| {
                GraphInvariantError::new(format!(
                    "required validation node `{}` has no gate specification",
                    node.id
                ))
            })?;
            let expected_fingerprint = gate.fingerprint(&self.current_repository.fingerprint);
            let matching = self
                .evidence
                .validations
                .iter()
                .filter(|(_, evidence)| {
                    evidence.node_id == node.id
                        && evidence.status == ValidationEvidenceStatus::Passed
                        && evidence.repository_fingerprint == self.current_repository.fingerprint
                        && evidence.fingerprint == expected_fingerprint
                })
                .map(|(evidence_id, _)| evidence_id.clone())
                .collect::<Vec<_>>();
            if matching.is_empty() {
                return Err(GraphInvariantError::new(format!(
                    "required validation node `{}` has no current passed evidence",
                    node.id
                )));
            }
            evidence_ids.extend(matching);
        }
        Ok(evidence_ids.into_iter().collect())
    }

    /// Returns the canonical complete set of validation proof invalidated when
    /// finalization is rebound to a new repository observation.
    pub fn finalization_validation_evidence_ids(&self) -> Vec<String> {
        self.graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_validation())
            .flat_map(|node| node.evidence_ids.iter().cloned())
            .chain(
                self.evidence
                    .validations
                    .iter()
                    .filter(|(_, evidence)| evidence.status == ValidationEvidenceStatus::Passed)
                    .map(|(evidence_id, _)| evidence_id.clone()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn target_execution_context(
        &self,
        node_id: &ExecutionNodeId,
        allowed_tools: Vec<ToolKind>,
    ) -> Result<TargetExecutionContext, GraphInvariantError> {
        let node = self.graph.node(node_id).ok_or_else(|| {
            GraphInvariantError::new(format!("unknown execution node `{node_id}`"))
        })?;
        let target = node.target.clone().ok_or_else(|| {
            GraphInvariantError::new(format!("node `{node_id}` is not a mutation target"))
        })?;
        let dependency_evidence = node
            .dependencies
            .iter()
            .filter_map(|dependency_id| self.graph.node(dependency_id))
            .flat_map(|dependency| dependency.evidence_ids.iter())
            .filter_map(|evidence_id| self.evidence.summary(evidence_id))
            .collect::<Vec<_>>();
        let reusable_file =
            self.evidence
                .reusable_file(&target.path, &self.current_repository.fingerprint, None);
        let current_file_content = reusable_file.map(|evidence| evidence.captured_content.clone());
        let nearby_context = reusable_file
            .filter(|evidence| evidence.line_range.is_some())
            .map(FileExcerpt::from)
            .into_iter()
            .collect();
        Ok(TargetExecutionContext {
            node_id: node.id.clone(),
            change_id: target.change_id.clone(),
            intent: target.intent.clone(),
            acceptance_criteria_ids: target.acceptance_criteria_ids.clone(),
            target,
            dependency_evidence,
            current_file_content,
            nearby_context,
            allowed_tools,
            remaining_node_budget: self.budget.remaining_for(&node.id, &node.budget),
        })
    }

    /// Appends one authoritative event and updates the graph-backed materialized
    /// state. Events after `RunFinished` are rejected so infrastructure updates
    /// cannot replace a terminal domain result.
    pub fn append_event(&mut self, event: ExecutionDomainEvent) -> Result<(), GraphInvariantError> {
        let terminal_outcome = self.terminal_outcome();
        let resumes_partial_terminal = matches!(
            &event,
            ExecutionDomainEvent::ExecutionResumed {
                previous_outcome: Some(MissionOutcome::PartialReviewable),
                ..
            }
        ) && terminal_outcome
            == Some(MissionOutcome::PartialReviewable);
        if terminal_outcome.is_some() && !resumes_partial_terminal {
            return Err(GraphInvariantError::new(
                "domain events cannot be appended after RunFinished",
            ));
        }
        if let Some(previous) = self.events.last()
            && event.sequence() <= previous.sequence()
        {
            return Err(GraphInvariantError::new(format!(
                "event sequence {} does not follow {}",
                event.sequence(),
                previous.sequence()
            )));
        }

        self.graph
            .validate_invariants_with_dependency_satisfaction(
                &self.dependency_satisfaction_ids(),
            )?;
        self.validate_event_semantics(&event)?;
        let repair_started = event
            .node_id()
            .is_some_and(|node_id| self.node_start_is_target_repair(&event, node_id));
        if repair_started {
            let node_id = event
                .node_id()
                .expect("a target repair start always refers to a node");
            let node = self.graph.node(node_id).ok_or_else(|| {
                GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
            })?;
            if self.budget.usage_for(node_id).repair_attempts >= node.budget.max_repair_attempts {
                return Err(GraphInvariantError::new(format!(
                    "node `{node_id}` cannot start repair beyond its {}-attempt budget",
                    node.budget.max_repair_attempts
                )));
            }
        }

        let dependency_satisfaction = self.dependency_satisfaction_ids();
        self.graph
            .apply_domain_event_with_dependency_satisfaction(&event, &dependency_satisfaction)?;
        if repair_started {
            self.budget.record_repair_attempt(
                event
                    .node_id()
                    .expect("a target repair start always refers to a node")
                    .clone(),
            );
        }
        match &event {
            ExecutionDomainEvent::RepositoryEvidenceRecorded {
                sequence,
                evidence_id,
                repository_fingerprint,
                evidence,
            } => {
                if let Some(evidence) = evidence {
                    if &evidence.evidence_id != evidence_id
                        || &evidence.repository_fingerprint != repository_fingerprint
                        || !evidence.content_hash_is_valid()
                    {
                        return Err(GraphInvariantError::new(
                            "repository evidence event payload does not match its identity",
                        ));
                    }
                    self.evidence.record_file(evidence.clone());
                }
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::NewRelevantEvidenceRecorded,
                    None,
                )
            }
            ExecutionDomainEvent::ComplexityClassified { assessment, .. } => {
                self.budget.mission = assessment.budget.clone();
            }
            ExecutionDomainEvent::GraphCreated {
                graph_id,
                revision,
                graph: Some(replacement),
                preserved_node_ids,
                ..
            } => {
                if replacement.graph_id != *graph_id || replacement.revision != *revision {
                    return Err(GraphInvariantError::new(
                        "graph-created payload does not match its graph id and revision",
                    ));
                }
                replacement.validate_invariants()?;
                let replacement_ids = replacement
                    .nodes
                    .iter()
                    .map(|node| node.id.clone())
                    .collect::<BTreeSet<_>>();
                let retained = preserved_node_ids.iter().cloned().collect::<BTreeSet<_>>();
                if !retained.is_subset(&replacement_ids) {
                    return Err(GraphInvariantError::new(
                        "graph-created preserved node set is not contained in the replacement graph",
                    ));
                }
                self.failures
                    .records
                    .retain(|failure| retained.contains(&failure.node_id));
                self.evidence
                    .validations
                    .retain(|_, evidence| retained.contains(&evidence.node_id));
                self.evidence.records.retain(|_, evidence| {
                    evidence
                        .node_id
                        .as_ref()
                        .is_none_or(|node_id| retained.contains(node_id))
                });
                self.budget
                    .node_usage
                    .retain(|node_id, _| retained.contains(node_id));
                self.budget.progress_events.retain(|progress| {
                    progress
                        .node_id
                        .as_ref()
                        .is_none_or(|node_id| retained.contains(node_id))
                });
                self.budget.progress_score = self
                    .budget
                    .progress_events
                    .iter()
                    .map(|progress| u64::from(progress.kind.score()))
                    .sum();
                let publication_retained = replacement
                    .nodes
                    .iter()
                    .find(|node| node.kind == ExecutionNodeKind::Publication)
                    .is_some_and(|node| retained.contains(&node.id));
                if !publication_retained {
                    self.publication = PublicationState::default();
                }
                self.graph = replacement.clone();
            }
            ExecutionDomainEvent::GraphCreated { graph: None, .. } => {}
            ExecutionDomainEvent::PlanAccepted { sequence, .. } => self
                .budget
                .record_progress_kind(*sequence, ProgressEventKind::PlanAccepted, None),
            ExecutionDomainEvent::MutationApplied {
                sequence,
                node_id,
                target_path,
                repository_fingerprint,
                evidence_id,
                ..
            } => {
                let progress = self.graph.node(node_id).map_or(
                    ProgressEventKind::SourceMutationApplied,
                    |node| {
                        if node.kind == ExecutionNodeKind::TestMutation {
                            ProgressEventKind::TestMutationApplied
                        } else {
                            ProgressEventKind::SourceMutationApplied
                        }
                    },
                );
                self.budget
                    .record_progress_kind(*sequence, progress, Some(node_id.clone()));
                self.current_repository.fingerprint = repository_fingerprint.clone();
                self.current_repository
                    .changed_paths
                    .insert(target_path.clone());
                self.evidence.record(EvidenceRecord {
                    evidence_id: evidence_id.clone(),
                    kind: EvidenceKind::Mutation,
                    node_id: Some(node_id.clone()),
                    repository_fingerprint: repository_fingerprint.clone(),
                    summary: format!("authoritative repository mutation applied `{target_path}`"),
                });
                self.failures.supersede_for_applied_target(
                    node_id,
                    target_path,
                    repository_fingerprint,
                );
                self.evidence
                    .supersede_stale_validation(repository_fingerprint);
            }
            ExecutionDomainEvent::MutationRejected { failure, .. } => {
                self.failures.record(failure.clone());
                self.materialize_unresolved_failure_status(&failure.node_id, false)?;
            }
            ExecutionDomainEvent::MutationSuperseded {
                sequence,
                node_id,
                failure_id,
                repository_fingerprint,
            } => {
                self.failures
                    .mark_superseded(failure_id, repository_fingerprint.clone());
                self.materialize_unresolved_failure_status(node_id, true)?;
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::FailureSuperseded,
                    Some(node_id.clone()),
                );
            }
            ExecutionDomainEvent::FailureRecorded { failure, .. } => {
                self.failures.record(failure.clone());
                self.materialize_unresolved_failure_status(&failure.node_id, false)?;
            }
            ExecutionDomainEvent::FailureRecovered {
                sequence,
                node_id,
                failure_id,
                repository_fingerprint,
            } => {
                self.failures
                    .mark_recovered(failure_id, repository_fingerprint.clone());
                self.materialize_unresolved_failure_status(node_id, false)?;
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::FailureRepaired,
                    Some(node_id.clone()),
                );
            }
            ExecutionDomainEvent::FailureSuperseded {
                sequence,
                node_id,
                failure_id,
                repository_fingerprint,
            } => {
                self.failures
                    .mark_superseded(failure_id, repository_fingerprint.clone());
                self.materialize_unresolved_failure_status(node_id, true)?;
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::FailureSuperseded,
                    Some(node_id.clone()),
                );
            }
            ExecutionDomainEvent::ValidationEvidenceRecorded { evidence, .. } => {
                self.evidence.record_validation(evidence.clone());
            }
            ExecutionDomainEvent::ValidationFailed { node_id, .. } => {
                self.materialize_unresolved_failure_status(node_id, false)?;
            }
            ExecutionDomainEvent::ValidationPassed {
                sequence, node_id, ..
            } => self.budget.record_progress_kind(
                *sequence,
                ProgressEventKind::ValidationPassed,
                Some(node_id.clone()),
            ),
            ExecutionDomainEvent::ValidationSuperseded { evidence_id, .. } => {
                if let Some(evidence) = self.evidence.validations.get_mut(evidence_id) {
                    evidence.status = ValidationEvidenceStatus::Superseded;
                }
            }
            ExecutionDomainEvent::FinalizationInvalidated {
                repository_fingerprint,
                stale_validation_evidence_ids,
                ..
            } => {
                self.current_repository.fingerprint = repository_fingerprint.clone();
                for evidence_id in stale_validation_evidence_ids {
                    if let Some(evidence) = self.evidence.validations.get_mut(evidence_id) {
                        evidence.status = ValidationEvidenceStatus::Superseded;
                    }
                }
                self.publication = PublicationState::default();
            }
            ExecutionDomainEvent::DiffReviewed {
                sequence, node_id, ..
            } => self.budget.record_progress_kind(
                *sequence,
                ProgressEventKind::DiffReviewed,
                Some(node_id.clone()),
            ),
            ExecutionDomainEvent::RecoveryPublicationRequested { .. } => {
                self.publication.status = match self.publication.status {
                    PublicationStatus::CommitCreated | PublicationStatus::BranchPushed => {
                        self.publication.status
                    }
                    PublicationStatus::NotStarted
                    | PublicationStatus::InProgress
                    | PublicationStatus::Failed
                    | PublicationStatus::PullRequestCreated => PublicationStatus::InProgress,
                };
                self.publication.mode = Some(PublicationMode::DraftRecovery);
                self.publication.draft = true;
                self.publication.recovery_requested = true;
            }
            ExecutionDomainEvent::PublicationStarted { mode, .. } => {
                self.publication.status = PublicationStatus::InProgress;
                self.publication.mode = Some(*mode);
                self.publication.draft = matches!(
                    mode,
                    PublicationMode::Draft | PublicationMode::DraftRecovery
                );
                self.publication.recovery_requested = false;
            }
            ExecutionDomainEvent::CommitCreated {
                sequence,
                node_id,
                commit_sha,
            } => {
                self.publication.status = PublicationStatus::CommitCreated;
                self.publication.commit_sha = Some(commit_sha.clone());
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::CommitCreated,
                    Some(node_id.clone()),
                );
            }
            ExecutionDomainEvent::BranchPushed { branch, .. } => {
                self.publication.status = PublicationStatus::BranchPushed;
                self.publication.branch = Some(branch.clone());
            }
            ExecutionDomainEvent::PullRequestCreated {
                sequence,
                node_id,
                url,
                number,
                draft,
            } => {
                self.publication.status = PublicationStatus::PullRequestCreated;
                self.publication.pull_request_url = Some(url.clone());
                self.publication.pull_request_number = *number;
                self.publication.draft = *draft;
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::PullRequestCreated,
                    Some(node_id.clone()),
                );
            }
            ExecutionDomainEvent::CancellationRequested { state, .. } => {
                self.cancellation = Some(state.clone());
            }
            ExecutionDomainEvent::ExecutionResumed {
                previous_outcome, ..
            } => {
                self.cancellation = None;
                if *previous_outcome == Some(MissionOutcome::PartialReviewable) {
                    self.publication.status = PublicationStatus::NotStarted;
                    self.publication.mode = None;
                    self.publication.commit_sha = None;
                    self.publication.recovery_requested = false;
                }
            }
            _ => {}
        }
        self.events.push(event);
        Ok(())
    }

    fn materialize_unresolved_failure_status(
        &mut self,
        node_id: &ExecutionNodeId,
        superseded_target: bool,
    ) -> Result<(), GraphInvariantError> {
        let unresolved = self
            .failures
            .unresolved_for_node(node_id)
            .map(|failure| failure.category.node_status())
            .collect::<Vec<_>>();
        let desired = if unresolved.contains(&ExecutionNodeStatus::FailedBlocking) {
            Some(ExecutionNodeStatus::FailedBlocking)
        } else if !unresolved.is_empty() {
            Some(ExecutionNodeStatus::FailedRecoverable)
        } else if superseded_target {
            Some(ExecutionNodeStatus::Superseded)
        } else {
            self.graph.node(node_id).and_then(|node| {
                matches!(
                    node.status,
                    ExecutionNodeStatus::FailedRecoverable | ExecutionNodeStatus::FailedBlocking
                )
                .then_some(ExecutionNodeStatus::Pending)
            })
        };
        if let Some(status) = desired
            && self
                .graph
                .node(node_id)
                .is_some_and(|node| node.status != status)
        {
            self.graph.set_node_status(node_id, status)?;
        }
        Ok(())
    }

    fn node_start_is_target_repair(
        &self,
        event: &ExecutionDomainEvent,
        node_id: &ExecutionNodeId,
    ) -> bool {
        if !matches!(event, ExecutionDomainEvent::NodeStarted { .. }) {
            return false;
        }
        let Some(node) = self.graph.node(node_id) else {
            return false;
        };
        if !node.kind.is_mutation() {
            return false;
        }
        node.status == ExecutionNodeStatus::FailedRecoverable
            || self.failures.unresolved().any(|failure| {
                failure.category.creates_repair_work()
                    && (&failure.node_id == node_id
                        || node.target.as_ref().is_some_and(|target| {
                            failure.target_path.as_deref() == Some(target.path.as_str())
                        }))
            })
    }

    fn validate_event_semantics(
        &self,
        event: &ExecutionDomainEvent,
    ) -> Result<(), GraphInvariantError> {
        match event {
            ExecutionDomainEvent::FinalizationInvalidated {
                repository_fingerprint,
                stale_validation_evidence_ids,
                ..
            } => {
                if repository_fingerprint.trim().is_empty() {
                    return Err(GraphInvariantError::new(
                        "finalization invalidation requires a repository fingerprint",
                    ));
                }
                let expected = self.finalization_validation_evidence_ids();
                if stale_validation_evidence_ids != &expected {
                    return Err(GraphInvariantError::new(format!(
                        "finalization invalidation validation evidence ids must exactly match {:?}",
                        expected
                    )));
                }
            }
            ExecutionDomainEvent::RecoveryPublicationRequested {
                node_id,
                repository_fingerprint,
                validation_evidence_ids,
                ..
            } => {
                if self.terminal_outcome().is_some() {
                    return Err(GraphInvariantError::new(
                        "recovery publication cannot be requested after RunFinished",
                    ));
                }
                if repository_fingerprint.trim().is_empty()
                    || repository_fingerprint != &self.current_repository.fingerprint
                {
                    return Err(GraphInvariantError::new(
                        "recovery publication requires the current repository fingerprint",
                    ));
                }
                if !self.current_repository.has_changes() {
                    return Err(GraphInvariantError::new(
                        "recovery publication requires a non-empty repository diff",
                    ));
                }
                let publication = self.graph.node(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!(
                        "recovery publication refers to unknown node `{node_id}`"
                    ))
                })?;
                if publication.kind != ExecutionNodeKind::Publication {
                    return Err(GraphInvariantError::new(format!(
                        "recovery publication node `{node_id}` is not a publication node"
                    )));
                }
                if publication.status == ExecutionNodeStatus::Completed
                    || self.publication.status == PublicationStatus::PullRequestCreated
                {
                    return Err(GraphInvariantError::new(
                        "recovery publication cannot replace completed publication",
                    ));
                }
                let expected = self.current_required_validation_evidence_ids()?;
                if validation_evidence_ids != &expected {
                    return Err(GraphInvariantError::new(format!(
                        "recovery publication validation evidence ids must exactly match {:?}",
                        expected
                    )));
                }
            }
            ExecutionDomainEvent::PublicationStarted {
                mode: PublicationMode::DraftRecovery,
                ..
            } => {
                return Err(GraphInvariantError::new(
                    "draft recovery publication must start with RecoveryPublicationRequested",
                ));
            }
            ExecutionDomainEvent::PublicationStarted { .. }
                if self.publication.recovery_requested =>
            {
                return Err(GraphInvariantError::new(
                    "recovery publication is already authorized",
                ));
            }
            ExecutionDomainEvent::GuardrailTriggered {
                outcome: MissionOutcome::PartialReviewable,
                ..
            } if !self.current_repository.has_changes() => {
                return Err(GraphInvariantError::new(
                    "partial-reviewable guardrail requires a non-empty repository diff",
                ));
            }
            ExecutionDomainEvent::MutationRejected {
                node_id, failure, ..
            } => self.validate_failure_record(failure, Some(node_id), true)?,
            ExecutionDomainEvent::FailureRecorded { failure, .. } => {
                self.validate_failure_record(failure, None, false)?
            }
            ExecutionDomainEvent::MutationSuperseded {
                node_id,
                failure_id,
                repository_fingerprint,
                ..
            }
            | ExecutionDomainEvent::FailureSuperseded {
                node_id,
                failure_id,
                repository_fingerprint,
                ..
            } => {
                self.validate_failure_resolution(node_id, failure_id, repository_fingerprint, true)?
            }
            ExecutionDomainEvent::FailureRecovered {
                node_id,
                failure_id,
                repository_fingerprint,
                ..
            } => self.validate_failure_resolution(
                node_id,
                failure_id,
                repository_fingerprint,
                false,
            )?,
            ExecutionDomainEvent::ValidationEvidenceRecorded {
                node_id, evidence, ..
            } => self.validate_validation_evidence(node_id, evidence)?,
            ExecutionDomainEvent::ExecutionResumed {
                execution_attempt,
                previous_outcome,
                ..
            } => {
                if *execution_attempt == 0 {
                    return Err(GraphInvariantError::new(
                        "execution resume requires a non-zero execution attempt",
                    ));
                }
                let resumes_cancellation = previous_outcome.is_none()
                    && self.cancellation.is_some()
                    && self.terminal_outcome().is_none();
                let resumes_partial = *previous_outcome == Some(MissionOutcome::PartialReviewable)
                    && self.terminal_outcome() == Some(MissionOutcome::PartialReviewable);
                if !resumes_cancellation && !resumes_partial {
                    return Err(GraphInvariantError::new(
                        "execution resume requires a cancellation checkpoint or partial-reviewable terminal outcome",
                    ));
                }
            }
            ExecutionDomainEvent::ValidationPassed {
                node_id,
                evidence_id,
                fingerprint,
                ..
            } => {
                let evidence = self.evidence.validations.get(evidence_id).ok_or_else(|| {
                    GraphInvariantError::new(format!(
                        "validation pass refers to unknown evidence `{evidence_id}`"
                    ))
                })?;
                if &evidence.node_id != node_id {
                    return Err(GraphInvariantError::new(format!(
                        "validation evidence `{evidence_id}` belongs to node `{}`, not `{node_id}`",
                        evidence.node_id
                    )));
                }
                if evidence.status != ValidationEvidenceStatus::Passed {
                    return Err(GraphInvariantError::new(format!(
                        "validation pass requires passed evidence `{evidence_id}`"
                    )));
                }
                self.validate_current_attached_validation_evidence(node_id, evidence, fingerprint)?;
            }
            ExecutionDomainEvent::ValidationFailed {
                node_id,
                failure_id,
                fingerprint,
                ..
            } => {
                let has_failed_evidence = self.evidence.validations.values().any(|evidence| {
                    &evidence.node_id == node_id
                        && matches!(
                            evidence.status,
                            ValidationEvidenceStatus::Failed
                                | ValidationEvidenceStatus::TimedOut
                                | ValidationEvidenceStatus::Cancelled
                        )
                        && self
                            .validate_current_attached_validation_evidence(
                                node_id,
                                evidence,
                                fingerprint,
                            )
                            .is_ok()
                });
                if !has_failed_evidence {
                    return Err(GraphInvariantError::new(format!(
                        "validation failure for node `{node_id}` requires attached current non-pass evidence matching fingerprint `{fingerprint}`"
                    )));
                }
                let failure = self.failures.get(failure_id).ok_or_else(|| {
                    GraphInvariantError::new(format!(
                        "validation failure refers to unknown failure `{failure_id}`"
                    ))
                })?;
                if &failure.node_id != node_id {
                    return Err(GraphInvariantError::new(format!(
                        "validation failure `{failure_id}` belongs to node `{}`, not `{node_id}`",
                        failure.node_id
                    )));
                }
                if !failure.is_unresolved() {
                    return Err(GraphInvariantError::new(format!(
                        "validation failure `{failure_id}` is already resolved"
                    )));
                }
                if !matches!(
                    failure.category,
                    FailureCategory::ValidationFailure | FailureCategory::InfrastructureFailure
                ) {
                    return Err(GraphInvariantError::new(format!(
                        "validation event cannot materialize failure `{failure_id}` of category `{:?}`",
                        failure.category
                    )));
                }
            }
            ExecutionDomainEvent::RunFinished { outcome, .. }
                if outcome.is_successful_domain_result()
                    && (!self.publication.is_published()
                        || !self.graph.nodes.iter().any(|node| {
                            node.kind == ExecutionNodeKind::Publication
                                && node.status.satisfies_dependency()
                        })) =>
            {
                return Err(GraphInvariantError::new(
                    "successful RunFinished requires completed pull-request publication",
                ));
            }
            ExecutionDomainEvent::PullRequestCreated { draft: false, .. }
                if self.publication.recovery_requested =>
            {
                return Err(GraphInvariantError::new(
                    "recovery publication requires a draft pull request",
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn validate_current_attached_validation_evidence(
        &self,
        node_id: &ExecutionNodeId,
        evidence: &ValidationEvidenceRecord,
        fingerprint: &str,
    ) -> Result<(), GraphInvariantError> {
        if evidence.fingerprint != fingerprint {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` fingerprint does not match outcome fingerprint",
                evidence.evidence_id
            )));
        }
        if evidence.repository_fingerprint != self.current_repository.fingerprint {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` is not current for the repository",
                evidence.evidence_id
            )));
        }
        let node = self.graph.node(node_id).ok_or_else(|| {
            GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
        })?;
        if !node.evidence_ids.contains(&evidence.evidence_id) {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` is not attached to node `{node_id}`",
                evidence.evidence_id
            )));
        }
        Ok(())
    }

    fn validate_validation_evidence(
        &self,
        node_id: &ExecutionNodeId,
        evidence: &ValidationEvidenceRecord,
    ) -> Result<(), GraphInvariantError> {
        if evidence.evidence_id.trim().is_empty() {
            return Err(GraphInvariantError::new(
                "validation evidence requires a non-empty evidence id",
            ));
        }
        if &evidence.node_id != node_id {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` belongs to node `{}`, not event node `{node_id}`",
                evidence.evidence_id, evidence.node_id
            )));
        }
        if self
            .evidence
            .validations
            .contains_key(&evidence.evidence_id)
        {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` is already recorded",
                evidence.evidence_id
            )));
        }
        let node = self.graph.node(node_id).ok_or_else(|| {
            GraphInvariantError::new(format!(
                "validation evidence `{}` refers to unknown node `{node_id}`",
                evidence.evidence_id
            ))
        })?;
        let gate = node.validation.as_ref().ok_or_else(|| {
            GraphInvariantError::new(format!(
                "validation evidence `{}` refers to non-validation node `{node_id}`",
                evidence.evidence_id
            ))
        })?;
        if evidence.gate_id != gate.gate_id {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` gate `{}` does not match node gate `{}`",
                evidence.evidence_id, evidence.gate_id, gate.gate_id
            )));
        }
        if evidence.repository_fingerprint.trim().is_empty()
            || evidence.repository_fingerprint != self.current_repository.fingerprint
        {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` requires the current repository fingerprint",
                evidence.evidence_id
            )));
        }
        let expected_fingerprint = gate.fingerprint(&self.current_repository.fingerprint);
        if evidence.fingerprint != expected_fingerprint {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` fingerprint does not match gate `{}` at the current repository state",
                evidence.evidence_id, gate.gate_id
            )));
        }
        if evidence.command != gate.command || evidence.working_directory != gate.working_directory
        {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` command context does not match gate `{}`",
                evidence.evidence_id, gate.gate_id
            )));
        }
        Ok(())
    }

    fn validate_failure_record(
        &self,
        failure: &FailureRecord,
        event_node_id: Option<&ExecutionNodeId>,
        mutation_only: bool,
    ) -> Result<(), GraphInvariantError> {
        if failure.id.as_str().trim().is_empty() {
            return Err(GraphInvariantError::new(
                "failure event requires a non-empty failure id",
            ));
        }
        if failure.node_id.as_str().trim().is_empty() {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` requires a non-empty node id",
                failure.id
            )));
        }
        if let Some(event_node_id) = event_node_id
            && event_node_id != &failure.node_id
        {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` belongs to node `{}`, not event node `{event_node_id}`",
                failure.id, failure.node_id
            )));
        }
        if self.failures.get(&failure.id).is_some() {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` is already recorded",
                failure.id
            )));
        }
        if !failure.is_unresolved()
            || failure.status != FailureStatus::Active
            || failure.resolved_repository_fingerprint.is_some()
        {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` must be recorded in active unresolved state",
                failure.id
            )));
        }
        if failure.attempt == 0 {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` requires a positive attempt",
                failure.id
            )));
        }
        if failure.repository_fingerprint.trim().is_empty() {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` requires a repository fingerprint",
                failure.id
            )));
        }
        if failure.message.trim().is_empty() {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` requires a diagnostic message",
                failure.id
            )));
        }
        let node = self.graph.node(&failure.node_id).ok_or_else(|| {
            GraphInvariantError::new(format!(
                "failure `{}` refers to unknown node `{}`",
                failure.id, failure.node_id
            ))
        })?;
        if mutation_only && !node.kind.is_mutation() {
            return Err(GraphInvariantError::new(format!(
                "mutation failure `{}` refers to non-mutation node `{}`",
                failure.id, failure.node_id
            )));
        }
        if !failure.category.is_valid_for_node_kind(node.kind) {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` category `{:?}` is invalid for node `{}` of kind `{:?}`",
                failure.id, failure.category, failure.node_id, node.kind
            )));
        }
        if mutation_only && failure.target_path.is_none() {
            return Err(GraphInvariantError::new(format!(
                "mutation failure `{}` requires its planned target path",
                failure.id
            )));
        }
        if let Some(target_path) = failure.target_path.as_deref() {
            let path_matches = if node.kind.is_mutation() {
                node.target
                    .as_ref()
                    .is_some_and(|target| target.path == target_path)
            } else {
                self.graph.nodes.iter().any(|candidate| {
                    candidate.kind.is_mutation()
                        && candidate
                            .target
                            .as_ref()
                            .is_some_and(|target| target.path == target_path)
                })
            };
            if !path_matches {
                return Err(GraphInvariantError::new(format!(
                    "failure `{}` target path `{target_path}` is not a matching planned target",
                    failure.id
                )));
            }
        }
        Ok(())
    }

    fn validate_failure_resolution(
        &self,
        node_id: &ExecutionNodeId,
        failure_id: &FailureId,
        repository_fingerprint: &str,
        superseded: bool,
    ) -> Result<(), GraphInvariantError> {
        if repository_fingerprint.trim().is_empty() {
            return Err(GraphInvariantError::new(format!(
                "failure resolution for `{failure_id}` requires a repository fingerprint"
            )));
        }
        let failure = self.failures.get(failure_id).ok_or_else(|| {
            GraphInvariantError::new(format!(
                "failure resolution refers to unknown failure `{failure_id}`"
            ))
        })?;
        if &failure.node_id != node_id {
            return Err(GraphInvariantError::new(format!(
                "failure `{failure_id}` belongs to node `{}`, not `{node_id}`",
                failure.node_id
            )));
        }
        if !failure.is_unresolved() {
            return Err(GraphInvariantError::new(format!(
                "failure `{failure_id}` is already resolved"
            )));
        }
        let node = self.graph.node(node_id).ok_or_else(|| {
            GraphInvariantError::new(format!(
                "failure `{failure_id}` refers to unknown node `{node_id}`"
            ))
        })?;
        if !failure.category.is_valid_for_node_kind(node.kind) {
            return Err(GraphInvariantError::new(format!(
                "failure `{failure_id}` category `{:?}` is invalid for node `{node_id}` of kind `{:?}`",
                failure.category, node.kind
            )));
        }
        if superseded {
            if !failure.category.is_supersedable_by_applied_target() {
                return Err(GraphInvariantError::new(format!(
                    "failure `{failure_id}` of category `{:?}` cannot be superseded",
                    failure.category
                )));
            }
            if !node.kind.is_mutation() {
                return Err(GraphInvariantError::new(format!(
                    "superseded failure `{failure_id}` must belong to a mutation node"
                )));
            }
        }
        Ok(())
    }

    pub fn with_event(&self, event: ExecutionDomainEvent) -> Result<Self, GraphInvariantError> {
        let mut next = self.clone();
        next.append_event(event)?;
        Ok(next)
    }

    pub fn validate_invariants(&self) -> Result<(), GraphInvariantError> {
        if self.run_id.trim().is_empty() {
            return Err(GraphInvariantError::new(
                "execution run id must not be empty",
            ));
        }
        self.graph
            .validate_invariants_with_dependency_satisfaction(
                &self.dependency_satisfaction_ids(),
            )?;
        let historical_node_ids = self
            .graph
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .chain(
                self.events
                    .iter()
                    .filter_map(ExecutionDomainEvent::node_id)
                    .cloned(),
            )
            .chain(self.events.iter().flat_map(|event| {
                match event {
                    ExecutionDomainEvent::GraphCreated {
                        graph: Some(graph), ..
                    } => graph
                        .nodes
                        .iter()
                        .map(|node| node.id.clone())
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                }
            }))
            .collect::<BTreeSet<_>>();
        let mut previous_sequence = None;
        let mut terminal_seen = None;
        for event in &self.events {
            if let Some(outcome) = terminal_seen {
                let valid_partial_resume = matches!(
                    event,
                    ExecutionDomainEvent::ExecutionResumed {
                        previous_outcome: Some(MissionOutcome::PartialReviewable),
                        ..
                    }
                ) && outcome == MissionOutcome::PartialReviewable;
                if !valid_partial_resume {
                    return Err(GraphInvariantError::new(
                        "domain event occurs after terminal RunFinished",
                    ));
                }
                terminal_seen = None;
            }
            if previous_sequence.is_some_and(|previous| event.sequence() <= previous) {
                return Err(GraphInvariantError::new(
                    "domain event sequence is not strictly increasing",
                ));
            }
            if let Some(node_id) = event.node_id()
                && !historical_node_ids.contains(node_id)
            {
                return Err(GraphInvariantError::new(format!(
                    "event `{}` refers to unknown node `{node_id}`",
                    event.event_type()
                )));
            }
            if let ExecutionDomainEvent::RunFinished { outcome, .. } = event {
                terminal_seen = Some(*outcome);
            }
            previous_sequence = Some(event.sequence());
        }
        for failure in &self.failures.records {
            if self.graph.node(&failure.node_id).is_none() {
                return Err(GraphInvariantError::new(format!(
                    "failure `{}` refers to unknown node `{}`",
                    failure.id, failure.node_id
                )));
            }
        }
        if self.publication.recovery_requested {
            if self.publication.mode != Some(PublicationMode::DraftRecovery)
                || !self.publication.draft
                || self.publication.status == PublicationStatus::NotStarted
                || !self.graph.recovery_publication_dependency_override
            {
                return Err(GraphInvariantError::new(
                    "recovery publication state requires draft-recovery mode and its graph dependency override",
                ));
            }
        } else if self.graph.recovery_publication_dependency_override {
            return Err(GraphInvariantError::new(
                "recovery publication graph dependency override has no authorizing publication state",
            ));
        }
        if let Some(outcome) = self.terminal_outcome()
            && outcome.is_successful_domain_result()
            && (!self.publication.is_published()
                || !self.graph.nodes.iter().any(|node| {
                    node.kind == ExecutionNodeKind::Publication
                        && node.status.satisfies_dependency()
                }))
        {
            return Err(GraphInvariantError::new(
                "successful terminal outcome has no completed pull-request publication",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(path: &str, role: &str) -> PlannedTarget {
        PlannedTarget {
            change_id: format!("change-{path}"),
            path: path.to_owned(),
            role: role.to_owned(),
            intent: format!("update {path}"),
            acceptance_criteria_ids: vec!["ac-1".to_owned()],
            new_file: false,
        }
    }

    fn gate(id: &str, gate_type: ValidationGateType) -> ValidationGateSpec {
        ValidationGateSpec {
            gate_id: id.to_owned(),
            gate_type,
            command: format!("run {id}"),
            working_directory: ".".to_owned(),
            required: true,
            dependency_lock_hash: "lock".to_owned(),
            relevant_environment_fingerprint: "env".to_owned(),
        }
    }

    fn graph() -> ExecutionGraph {
        ExecutionGraph::from_targets(
            "graph-1",
            MissionComplexity::Small,
            "tree-1",
            &[
                // Input ordering must not permit a test mutation before source work.
                target("tests/theme.test.ts", "test"),
                target("src/theme.ts", "production"),
            ],
            &[
                gate("focused", ValidationGateType::FocusedTest),
                gate("suite", ValidationGateType::TestSuite),
                gate("build", ValidationGateType::Build),
            ],
            &MissionBudget::for_complexity(MissionComplexity::Small),
        )
    }

    fn recovery_publication_snapshot() -> (ExecutionSnapshot, ExecutionNodeId, Vec<String>) {
        let mut graph = graph();
        let mutation_ids = graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_mutation())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        for node_id in mutation_ids {
            graph
                .set_node_status(&node_id, ExecutionNodeStatus::Applied)
                .expect("apply recovery fixture target");
        }

        let repository_fingerprint = "tree-recovery".to_owned();
        let mut evidence = EvidenceStore::default();
        let validation_ids = graph
            .nodes
            .iter()
            .filter(|node| node.required && node.kind.is_validation())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        let mut evidence_ids = Vec::new();
        for (index, node_id) in validation_ids.into_iter().enumerate() {
            let gate = graph
                .node(&node_id)
                .and_then(|node| node.validation.clone())
                .expect("validation gate");
            let evidence_id = format!("recovery-validation-{index}");
            let validation_fingerprint = gate.fingerprint(&repository_fingerprint);
            evidence.record_validation(ValidationEvidenceRecord {
                evidence_id: evidence_id.clone(),
                node_id: node_id.clone(),
                gate_id: gate.gate_id,
                fingerprint: validation_fingerprint,
                repository_fingerprint: repository_fingerprint.clone(),
                command: gate.command,
                working_directory: gate.working_directory,
                status: ValidationEvidenceStatus::Passed,
                exit_code: Some(0),
                output_summary: "passed".to_owned(),
                duration: Duration::from_millis(1),
            });
            let node = graph.node_mut(&node_id).expect("validation node");
            node.status = ExecutionNodeStatus::Passed;
            node.evidence_ids.push(evidence_id.clone());
            graph.refresh_readiness();
            evidence_ids.push(evidence_id);
        }
        evidence_ids.sort();
        let publication = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Publication)
            .expect("publication node")
            .id
            .clone();
        let snapshot = ExecutionSnapshot {
            run_id: "run-recovery-publication".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: repository_fingerprint.clone(),
                source_tree_hash: repository_fingerprint,
                changed_paths: BTreeSet::from(["src/theme.ts".to_owned()]),
                ..RepositorySnapshot::default()
            },
            graph,
            evidence,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        assert_eq!(
            snapshot
                .current_required_validation_evidence_ids()
                .expect("current validation evidence"),
            evidence_ids
        );
        (snapshot, publication, evidence_ids)
    }

    #[test]
    fn default_complexity_envelopes_are_exact() {
        let cases = [
            (MissionComplexity::Tiny, 2_000_000, 14, 8, 1),
            (MissionComplexity::Small, 5_000_000, 25, 15, 2),
            (MissionComplexity::Medium, 10_000_000, 45, 35, 3),
            (MissionComplexity::Large, 20_000_000, 80, 75, 4),
        ];
        for (complexity, cost, calls, minutes, repairs) in cases {
            let budget = MissionBudget::for_complexity(complexity);
            assert_eq!(budget.max_cost_micros, cost);
            assert_eq!(budget.max_model_calls, calls);
            assert_eq!(budget.max_duration, Duration::from_secs(minutes * 60));
            assert_eq!(budget.max_target_repair_rounds, repairs);
        }
    }

    #[test]
    fn policy_overrides_do_not_change_the_classification() {
        let input = ComplexityInput {
            planned_target_count: 5,
            ..ComplexityInput::default()
        };
        let assessment = ComplexityAssessment::classify_with_policy(
            &input,
            &MissionBudgetOverride {
                max_model_calls: Some(30),
                max_cost_micros: Some(4_500_000),
                ..MissionBudgetOverride::default()
            },
        );
        assert_eq!(assessment.class, MissionComplexity::Small);
        assert_eq!(assessment.budget.max_model_calls, 30);
        assert_eq!(assessment.budget.max_cost_micros, 4_500_000);
    }

    #[test]
    fn accepted_plan_builds_the_mandatory_dependency_chain() {
        let graph = graph();
        graph.validate_invariants().expect("valid graph");

        let source = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source node");
        let test = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::TestMutation)
            .expect("test node");
        let focused = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::ValidationFocused)
            .expect("focused node");
        let suite = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::ValidationSuite)
            .expect("suite node");
        let build = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::ValidationBuild)
            .expect("build node");
        let review = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::DiffReview)
            .expect("review node");
        let completion = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .expect("completion node");
        let publication = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Publication)
            .expect("publication node");

        assert_eq!(test.dependencies, vec![source.id.clone()]);
        assert_eq!(focused.dependencies, vec![test.id.clone()]);
        assert_eq!(suite.dependencies, vec![focused.id.clone()]);
        assert_eq!(build.dependencies, vec![suite.id.clone()]);
        assert_eq!(review.dependencies, vec![build.id.clone()]);
        assert_eq!(completion.dependencies, vec![review.id.clone()]);
        assert_eq!(publication.dependencies, vec![completion.id.clone()]);
        assert_eq!(
            graph.next_runnable_node().map(|node| &node.id),
            Some(&source.id)
        );
    }

    #[test]
    fn scrambled_validation_input_builds_one_canonical_dependency_chain() {
        let scrambled = vec![
            gate("lint-z", ValidationGateType::Lint),
            gate("build", ValidationGateType::Build),
            gate("suite-z", ValidationGateType::TestSuite),
            gate("focused-z", ValidationGateType::FocusedTest),
            gate("custom", ValidationGateType::Custom),
            gate("typecheck-a", ValidationGateType::Typecheck),
            gate("focused-a", ValidationGateType::FocusedTest),
            gate("suite-a", ValidationGateType::TestSuite),
        ];
        let build = |gates: &[ValidationGateSpec]| {
            ExecutionGraph::from_targets(
                "graph-scrambled-validation",
                MissionComplexity::Small,
                "tree-1",
                &[target("src/theme.ts", "production")],
                gates,
                &MissionBudget::for_complexity(MissionComplexity::Small),
            )
        };
        let graph = build(&scrambled);
        graph.validate_invariants().expect("canonical graph");
        let validation_nodes = graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_validation())
            .collect::<Vec<_>>();
        assert_eq!(
            validation_nodes
                .iter()
                .map(|node| {
                    node.validation
                        .as_ref()
                        .expect("validation gate")
                        .gate_id
                        .as_str()
                })
                .collect::<Vec<_>>(),
            vec![
                "focused-a",
                "focused-z",
                "suite-a",
                "suite-z",
                "build",
                "lint-z",
                "typecheck-a",
                "custom",
            ]
        );
        for pair in validation_nodes.windows(2) {
            assert_eq!(pair[1].dependencies, vec![pair[0].id.clone()]);
        }

        let mut reversed = scrambled;
        reversed.reverse();
        let reversed_graph = build(&reversed);
        let canonical_projection = |graph: &ExecutionGraph| {
            graph
                .nodes
                .iter()
                .filter(|node| node.kind.is_validation())
                .map(|node| {
                    (
                        node.id.clone(),
                        node.dependencies.clone(),
                        node.validation.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            canonical_projection(&graph),
            canonical_projection(&reversed_graph),
            "equivalent gate sets must not produce manifest-order-dependent topology"
        );
    }

    #[test]
    fn remaining_work_is_exactly_the_pending_required_graph_nodes() {
        let mut graph = graph();
        let expected = graph
            .nodes
            .iter()
            .filter(|node| node.required && !node.status.is_success())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            graph
                .remaining_required_nodes()
                .iter()
                .map(|node| node.id.clone())
                .collect::<Vec<_>>(),
            expected
        );

        let first_target = graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .expect("first mutation")
            .id
            .clone();
        graph
            .set_node_status(&first_target, ExecutionNodeStatus::Applied)
            .expect("apply first target");
        let remaining = graph.remaining_required_nodes();
        assert!(!remaining.iter().any(|node| node.id == first_target));
        assert!(
            remaining
                .iter()
                .all(|node| node.required && !node.status.is_success())
        );
    }

    #[test]
    fn graph_ids_and_serialization_are_deterministic() {
        let first = graph();
        let second = graph();
        assert_eq!(first, second);
        let encoded = serde_json::to_string(&first).expect("serialize graph");
        let decoded: ExecutionGraph = serde_json::from_str(&encoded).expect("deserialize graph");
        assert_eq!(decoded, first);
    }

    #[test]
    fn evidence_cache_reuses_only_compatible_content() {
        let mut evidence = EvidenceStore::default();
        let range = LineRange::new(10, 30).expect("range");
        let id = evidence.capture_file("src/lib.rs", "tree-1", Some(range), "bounded", true);
        assert_eq!(
            evidence
                .reusable_file("src/lib.rs", "tree-1", LineRange::new(15, 20),)
                .map(|entry| entry.evidence_id.as_str()),
            Some(id.as_str())
        );
        assert!(
            evidence
                .reusable_file("src/lib.rs", "tree-1", None)
                .is_none(),
            "a truncated excerpt cannot satisfy a full-file read"
        );
        assert!(
            evidence
                .reusable_file("src/lib.rs", "tree-2", LineRange::new(15, 20),)
                .is_none(),
            "repository changes invalidate cached reads"
        );
        assert_eq!(evidence.record_file(evidence.files[&id].clone()), id);
        assert_eq!(evidence.files.len(), 1, "duplicate reads are deduplicated");
    }

    #[test]
    fn applied_target_supersedes_failures_and_unblocks_the_store() {
        let node_id = ExecutionNodeId::new("target-1");
        let failure = FailureRecord {
            id: FailureId::new("failure-1"),
            node_id: node_id.clone(),
            target_path: Some("src/theme.ts".to_owned()),
            category: FailureCategory::MutationConflict,
            status: FailureStatus::Active,
            attempt: 1,
            repository_fingerprint: "tree-1".to_owned(),
            message: "replace text did not match".to_owned(),
            ..FailureRecord::default()
        };
        let mut failures = FailureStore::default();
        failures.record(failure);
        assert!(failures.has_unresolved_for_node(&node_id));
        assert_eq!(
            failures.supersede_for_applied_target(&node_id, "src/theme.ts", "tree-2"),
            vec![FailureId::new("failure-1")]
        );
        assert!(!failures.has_unresolved());
        assert_eq!(
            failures
                .get(&FailureId::new("failure-1"))
                .map(|failure| failure.status),
            Some(FailureStatus::Superseded)
        );
    }

    #[test]
    fn applied_target_does_not_supersede_non_mutation_failures() {
        let node_id = ExecutionNodeId::new("target-1");
        let preserved = [
            FailureCategory::ModelArtifactRecoverable,
            FailureCategory::TargetBlocked,
            FailureCategory::ValidationFailure,
            FailureCategory::InfrastructureFailure,
            FailureCategory::OrchestrationInvariantViolation,
            FailureCategory::UserCancellation,
        ];
        let mut failures = FailureStore::default();
        for (index, category) in preserved.into_iter().enumerate() {
            let mut failure = FailureRecord::new(
                format!("failure-{index}"),
                node_id.clone(),
                category,
                1,
                "tree-1",
                "must remain explicit",
            );
            failure.target_path = Some("src/theme.ts".to_owned());
            failures.record(failure);
        }

        assert!(
            failures
                .supersede_for_applied_target(&node_id, "src/theme.ts", "tree-2")
                .is_empty()
        );
        assert_eq!(failures.unresolved().count(), preserved.len());
    }

    #[test]
    fn failure_event_stream_replays_graph_store_and_progress_exactly() {
        let initial_graph = graph();
        let source = initial_graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source mutation")
            .clone();
        let test = initial_graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::TestMutation)
            .expect("test mutation")
            .clone();
        let initial = ExecutionSnapshot {
            run_id: "run-event-replay".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                source_tree_hash: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: initial_graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        let mut persisted = initial.clone();
        persisted
            .append_event(ExecutionDomainEvent::NodeStarted {
                sequence: 1,
                node_id: source.id.clone(),
                attempt: 1,
                started_at: "attempt-1".to_owned(),
                repository_fingerprint: "tree-1".to_owned(),
            })
            .expect("start mutation");
        let mut mutation_failure = FailureRecord::new(
            "mutation-failure",
            source.id.clone(),
            FailureCategory::MutationConflict,
            1,
            "tree-1",
            "replacement no longer matched",
        );
        mutation_failure.target_path = source.target.as_ref().map(|target| target.path.clone());
        persisted
            .append_event(ExecutionDomainEvent::MutationRejected {
                sequence: 2,
                node_id: source.id.clone(),
                failure: mutation_failure,
            })
            .expect("record mutation rejection");
        persisted
            .append_event(ExecutionDomainEvent::FailureSuperseded {
                sequence: 3,
                node_id: source.id.clone(),
                failure_id: FailureId::new("mutation-failure"),
                repository_fingerprint: "tree-2".to_owned(),
            })
            .expect("supersede mutation failure from final state");

        let mut infrastructure_failure = FailureRecord::new(
            "infrastructure-failure",
            test.id.clone(),
            FailureCategory::InfrastructureFailure,
            1,
            "tree-2",
            "repository transport unavailable",
        );
        infrastructure_failure.target_path = test.target.as_ref().map(|target| target.path.clone());
        persisted
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 4,
                failure: infrastructure_failure,
            })
            .expect("record infrastructure failure");

        let encoded = serde_json::to_string(&persisted.events).expect("serialize event stream");
        let replay_events: Vec<ExecutionDomainEvent> =
            serde_json::from_str(&encoded).expect("deserialize event stream");
        let mut replayed = initial;
        for event in replay_events {
            replayed.append_event(event).expect("replay event");
        }

        assert_eq!(replayed.events, persisted.events);
        assert_eq!(replayed.graph, persisted.graph);
        assert_eq!(replayed.failures, persisted.failures);
        assert_eq!(
            replayed.budget.progress_events,
            persisted.budget.progress_events
        );
        assert_eq!(
            replayed
                .failures
                .get(&FailureId::new("mutation-failure"))
                .map(|failure| failure.status),
            Some(FailureStatus::Superseded)
        );
        assert_eq!(
            replayed.graph.node(&source.id).map(|node| node.status),
            Some(ExecutionNodeStatus::Superseded)
        );
        assert_eq!(
            replayed.graph.node(&test.id).map(|node| node.status),
            Some(ExecutionNodeStatus::FailedBlocking)
        );
        assert_eq!(
            replayed
                .failures
                .get(&FailureId::new("infrastructure-failure"))
                .map(|failure| failure.category),
            Some(FailureCategory::InfrastructureFailure)
        );
        assert!(replayed.budget.progress_events.iter().any(|progress| {
            progress.sequence == 3
                && progress.kind == ProgressEventKind::FailureSuperseded
                && progress.node_id.as_ref() == Some(&source.id)
        }));
    }

    #[test]
    fn evidence_and_topology_events_replay_without_deleting_history() {
        let initial_graph = graph();
        let source = initial_graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source mutation")
            .clone();
        let validation_node = initial_graph
            .nodes
            .iter()
            .find(|node| {
                node.validation
                    .as_ref()
                    .is_some_and(|gate| gate.gate_id == "suite")
            })
            .expect("stable suite validation")
            .clone();
        let validation_fingerprint = validation_node
            .validation
            .as_ref()
            .expect("suite gate")
            .fingerprint("tree-1");
        let mut initial = ExecutionSnapshot {
            run_id: "run-topology-replay".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                source_tree_hash: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: initial_graph.clone(),
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            publication: PublicationState {
                status: PublicationStatus::BranchPushed,
                branch: Some("rustgrid/replay".to_owned()),
                ..PublicationState::default()
            },
            ..ExecutionSnapshot::default()
        };
        initial
            .evidence
            .record_validation(ValidationEvidenceRecord {
                evidence_id: "stale-suite-evidence".to_owned(),
                node_id: validation_node.id.clone(),
                gate_id: "suite".to_owned(),
                fingerprint: validation_fingerprint,
                repository_fingerprint: "tree-1".to_owned(),
                command: "run suite".to_owned(),
                working_directory: ".".to_owned(),
                status: ValidationEvidenceStatus::Failed,
                output_summary: "old topology failure".to_owned(),
                ..ValidationEvidenceRecord::default()
            });
        initial
            .budget
            .record_model_call(validation_node.id.clone(), 75, Duration::from_millis(5));
        initial.budget.record_progress_kind(
            0,
            ProgressEventKind::NodeMadeReady,
            Some(validation_node.id.clone()),
        );
        let repository_evidence =
            FileEvidence::capture("src/theme.ts", "tree-1", None, "export {};\n", false);
        let repository_evidence_id = repository_evidence.evidence_id.clone();
        let mut persisted = initial.clone();
        persisted
            .append_event(ExecutionDomainEvent::RepositoryEvidenceRecorded {
                sequence: 1,
                evidence_id: repository_evidence_id.clone(),
                repository_fingerprint: "tree-1".to_owned(),
                evidence: Some(repository_evidence),
            })
            .expect("record repository evidence");
        persisted
            .append_event(ExecutionDomainEvent::MutationApplied {
                sequence: 2,
                node_id: source.id.clone(),
                target_path: "src/theme.ts".to_owned(),
                repository_fingerprint: "tree-2".to_owned(),
                evidence_id: "mutation-theme-tree-2".to_owned(),
            })
            .expect("record mutation evidence");
        assert!(
            persisted
                .evidence
                .records
                .contains_key("mutation-theme-tree-2")
        );

        let mut replacement = ExecutionGraph::from_targets(
            "graph-1",
            MissionComplexity::Small,
            "tree-2",
            &[target("src/replacement.ts", "production")],
            &[gate("suite", ValidationGateType::TestSuite)],
            &MissionBudget::for_complexity(MissionComplexity::Small),
        );
        replacement.revision = initial_graph.revision.saturating_add(1);
        persisted
            .append_event(ExecutionDomainEvent::GraphCreated {
                sequence: 3,
                graph_id: replacement.graph_id.clone(),
                revision: replacement.revision,
                graph: Some(replacement.clone()),
                preserved_node_ids: Vec::new(),
            })
            .expect("append replacement topology");

        assert_eq!(persisted.events.len(), 3);
        assert!(matches!(
            &persisted.events[1],
            ExecutionDomainEvent::MutationApplied { node_id, .. } if node_id == &source.id
        ));
        assert_eq!(persisted.graph, replacement);
        assert!(
            persisted
                .evidence
                .files
                .contains_key(&repository_evidence_id)
        );
        assert!(
            !persisted
                .evidence
                .validations
                .contains_key("stale-suite-evidence"),
            "a stable validation node invalidated by changed dependencies must lose stale evidence"
        );
        assert_eq!(
            persisted.budget.usage_for(&validation_node.id).model_calls,
            0
        );
        assert!(
            persisted
                .budget
                .progress_events
                .iter()
                .all(|progress| progress.node_id.as_ref() != Some(&validation_node.id))
        );
        assert_eq!(persisted.publication, PublicationState::default());
        persisted
            .validate_invariants()
            .expect("persisted invariants");

        let events = persisted.events.clone();
        let mut replayed = initial;
        for event in events {
            replayed.append_event(event).expect("replay event");
        }
        assert_eq!(replayed.graph, persisted.graph);
        assert_eq!(replayed.events, persisted.events);
        assert_eq!(replayed.evidence, persisted.evidence);
        assert_eq!(replayed.failures, persisted.failures);
        assert_eq!(replayed.budget, persisted.budget);
    }

    #[test]
    fn generic_failure_events_recover_discovery_and_validation_nodes() {
        let budget = MissionBudget::for_complexity(MissionComplexity::Small);
        let discovery_graph =
            ExecutionGraph::bootstrap("bootstrap", "tree-1", MissionComplexity::Small, &budget);
        let discovery = discovery_graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Discovery)
            .expect("discovery node")
            .id
            .clone();
        let mut discovery_snapshot = ExecutionSnapshot {
            run_id: "run-discovery-recovery".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: discovery_graph,
            budget: BudgetState::new(budget),
            ..ExecutionSnapshot::default()
        };
        discovery_snapshot
            .append_event(ExecutionDomainEvent::DiscoveryStarted { sequence: 1 })
            .expect("start discovery");
        discovery_snapshot
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 2,
                failure: FailureRecord::new(
                    "discovery-artifact",
                    discovery.clone(),
                    FailureCategory::ModelArtifactRecoverable,
                    1,
                    "tree-1",
                    "discovery artifact was malformed",
                ),
            })
            .expect("record discovery failure");
        assert_eq!(
            discovery_snapshot
                .graph
                .node(&discovery)
                .map(|node| node.status),
            Some(ExecutionNodeStatus::FailedRecoverable)
        );
        discovery_snapshot
            .append_event(ExecutionDomainEvent::FailureRecovered {
                sequence: 3,
                node_id: discovery.clone(),
                failure_id: FailureId::new("discovery-artifact"),
                repository_fingerprint: "tree-1".to_owned(),
            })
            .expect("recover discovery failure");
        assert_eq!(
            discovery_snapshot
                .graph
                .node(&discovery)
                .map(|node| node.status),
            Some(ExecutionNodeStatus::Ready)
        );
        assert_eq!(
            discovery_snapshot
                .failures
                .get(&FailureId::new("discovery-artifact"))
                .map(|failure| failure.status),
            Some(FailureStatus::Recovered)
        );
        assert!(
            discovery_snapshot
                .budget
                .progress_events
                .iter()
                .any(|progress| {
                    progress.kind == ProgressEventKind::FailureRepaired
                        && progress.node_id.as_ref() == Some(&discovery)
                })
        );

        let mut validation_graph = graph();
        let mutation_ids = validation_graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_mutation())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        for mutation_id in mutation_ids {
            validation_graph
                .set_node_status(&mutation_id, ExecutionNodeStatus::Applied)
                .expect("apply prerequisite mutation");
        }
        let validation = validation_graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::ValidationFocused)
            .expect("focused validation")
            .id
            .clone();
        let mut validation_snapshot = ExecutionSnapshot {
            run_id: "run-validation-recovery".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-2".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: validation_graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        validation_snapshot
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 1,
                failure: FailureRecord::new(
                    "validation-failure",
                    validation.clone(),
                    FailureCategory::ValidationFailure,
                    1,
                    "tree-2",
                    "focused validation failed",
                ),
            })
            .expect("record validation failure");
        validation_snapshot
            .append_event(ExecutionDomainEvent::FailureRecovered {
                sequence: 2,
                node_id: validation.clone(),
                failure_id: FailureId::new("validation-failure"),
                repository_fingerprint: "tree-2".to_owned(),
            })
            .expect("recover validation failure");
        assert_eq!(
            validation_snapshot
                .graph
                .node(&validation)
                .map(|node| node.status),
            Some(ExecutionNodeStatus::Ready)
        );
        assert_eq!(validation_snapshot.failures.unresolved().count(), 0);
    }

    #[test]
    fn validation_failed_preserves_blocking_infrastructure_state_on_replay() {
        for (category, expected) in [
            (
                FailureCategory::ValidationFailure,
                ExecutionNodeStatus::FailedRecoverable,
            ),
            (
                FailureCategory::InfrastructureFailure,
                ExecutionNodeStatus::FailedBlocking,
            ),
        ] {
            let mut initial_graph = graph();
            let mutation_ids = initial_graph
                .nodes
                .iter()
                .filter(|node| node.kind.is_mutation())
                .map(|node| node.id.clone())
                .collect::<Vec<_>>();
            for node_id in mutation_ids {
                initial_graph
                    .set_node_status(&node_id, ExecutionNodeStatus::Applied)
                    .unwrap();
            }
            let validation_id = initial_graph
                .nodes
                .iter()
                .find(|node| node.kind.is_validation())
                .expect("validation node")
                .id
                .clone();
            let validation_gate = initial_graph
                .node(&validation_id)
                .and_then(|node| node.validation.clone())
                .expect("validation gate");
            let initial = ExecutionSnapshot {
                run_id: format!("validation-{category:?}"),
                current_repository: RepositorySnapshot {
                    fingerprint: "tree-1".into(),
                    changed_paths: BTreeSet::from(["src/theme.ts".into()]),
                    ..RepositorySnapshot::default()
                },
                graph: initial_graph,
                ..ExecutionSnapshot::default()
            };
            let failure_id = FailureId::new(format!("validation-{category:?}"));
            let validation_fingerprint = validation_gate.fingerprint("tree-1");
            let evidence_id = format!("evidence-{category:?}");
            let evidence = ValidationEvidenceRecord {
                evidence_id: evidence_id.clone(),
                node_id: validation_id.clone(),
                gate_id: validation_gate.gate_id,
                fingerprint: validation_fingerprint.clone(),
                repository_fingerprint: "tree-1".into(),
                command: validation_gate.command,
                working_directory: validation_gate.working_directory,
                status: if category == FailureCategory::InfrastructureFailure {
                    ValidationEvidenceStatus::TimedOut
                } else {
                    ValidationEvidenceStatus::Failed
                },
                exit_code: Some(1),
                output_summary: "validation did not complete successfully".into(),
                duration: Duration::from_millis(5),
            };
            let events = vec![
                ExecutionDomainEvent::ValidationEvidenceRecorded {
                    sequence: 1,
                    node_id: validation_id.clone(),
                    evidence: evidence.clone(),
                },
                ExecutionDomainEvent::FailureRecorded {
                    sequence: 2,
                    failure: FailureRecord::new(
                        failure_id.clone(),
                        validation_id.clone(),
                        category,
                        1,
                        "tree-1",
                        "validation did not complete successfully",
                    ),
                },
                ExecutionDomainEvent::ValidationFailed {
                    sequence: 3,
                    node_id: validation_id.clone(),
                    failure_id,
                    fingerprint: validation_fingerprint,
                },
            ];
            let mut persisted = initial.clone();
            let status_before_evidence =
                persisted.graph.node(&validation_id).map(|node| node.status);
            persisted.append_event(events[0].clone()).unwrap();
            assert_eq!(
                persisted.graph.node(&validation_id).map(|node| node.status),
                status_before_evidence,
                "recording evidence must not change validation lifecycle status"
            );
            assert_eq!(
                persisted
                    .graph
                    .node(&validation_id)
                    .map(|node| node.evidence_ids.as_slice()),
                Some(std::slice::from_ref(&evidence_id))
            );
            for event in events.iter().skip(1) {
                persisted.append_event(event.clone()).unwrap();
            }
            assert_eq!(
                persisted.graph.node(&validation_id).map(|node| node.status),
                Some(expected)
            );
            assert_eq!(
                persisted.evidence.validations.get(&evidence_id),
                Some(&evidence)
            );

            let encoded = serde_json::to_string(&events).unwrap();
            let replay_events: Vec<ExecutionDomainEvent> = serde_json::from_str(&encoded).unwrap();
            let mut replayed = initial;
            for event in replay_events {
                replayed.append_event(event).unwrap();
            }
            assert_eq!(replayed.graph, persisted.graph);
            assert_eq!(replayed.failures, persisted.failures);
            assert_eq!(replayed.evidence, persisted.evidence);
        }
    }

    #[test]
    fn validation_outcomes_require_recorded_current_evidence() {
        let mut validation_graph = graph();
        let mutation_ids = validation_graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_mutation())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        for node_id in mutation_ids {
            validation_graph
                .set_node_status(&node_id, ExecutionNodeStatus::Applied)
                .unwrap();
        }
        let validation = validation_graph
            .nodes
            .iter()
            .find(|node| node.kind.is_validation())
            .expect("validation node")
            .clone();
        let gate = validation.validation.as_ref().expect("validation gate");
        let fingerprint = gate.fingerprint("tree-1");
        let mut snapshot = ExecutionSnapshot {
            run_id: "validation-evidence-guards".into(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".into(),
                ..RepositorySnapshot::default()
            },
            graph: validation_graph,
            ..ExecutionSnapshot::default()
        };

        let error = snapshot
            .append_event(ExecutionDomainEvent::ValidationPassed {
                sequence: 1,
                node_id: validation.id.clone(),
                evidence_id: "missing-evidence".into(),
                fingerprint: fingerprint.clone(),
            })
            .expect_err("a pass without recorded evidence must fail closed");
        assert!(error.message.contains("unknown evidence"));
        assert!(snapshot.events.is_empty());

        let mut missing_failure_evidence = snapshot.clone();
        missing_failure_evidence
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 1,
                failure: FailureRecord::new(
                    "unproven-validation-failure",
                    validation.id.clone(),
                    FailureCategory::ValidationFailure,
                    1,
                    "tree-1",
                    "validation failed without evidence",
                ),
            })
            .unwrap();
        let before_unproven_failure = missing_failure_evidence.clone();
        let error = missing_failure_evidence
            .append_event(ExecutionDomainEvent::ValidationFailed {
                sequence: 2,
                node_id: validation.id.clone(),
                failure_id: FailureId::new("unproven-validation-failure"),
                fingerprint: fingerprint.clone(),
            })
            .expect_err("a failure without recorded evidence must fail closed");
        assert!(error.message.contains("non-pass evidence"));
        assert_eq!(missing_failure_evidence, before_unproven_failure);

        snapshot
            .append_event(ExecutionDomainEvent::ValidationEvidenceRecorded {
                sequence: 1,
                node_id: validation.id.clone(),
                evidence: ValidationEvidenceRecord {
                    evidence_id: "failed-evidence".into(),
                    node_id: validation.id.clone(),
                    gate_id: gate.gate_id.clone(),
                    fingerprint: fingerprint.clone(),
                    repository_fingerprint: "tree-1".into(),
                    command: gate.command.clone(),
                    working_directory: gate.working_directory.clone(),
                    status: ValidationEvidenceStatus::Failed,
                    exit_code: Some(1),
                    output_summary: "failed".into(),
                    duration: Duration::from_millis(1),
                },
            })
            .unwrap();
        let before_invalid_pass = snapshot.clone();
        let error = snapshot
            .append_event(ExecutionDomainEvent::ValidationPassed {
                sequence: 2,
                node_id: validation.id.clone(),
                evidence_id: "failed-evidence".into(),
                fingerprint: fingerprint.clone(),
            })
            .expect_err("failed evidence cannot prove a validation pass");
        assert!(error.message.contains("requires passed evidence"));
        assert_eq!(snapshot, before_invalid_pass);

        let failure_id = FailureId::new("validation-failure");
        snapshot
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 2,
                failure: FailureRecord::new(
                    failure_id.clone(),
                    validation.id.clone(),
                    FailureCategory::ValidationFailure,
                    1,
                    "tree-1",
                    "validation failed",
                ),
            })
            .unwrap();
        snapshot
            .append_event(ExecutionDomainEvent::ValidationFailed {
                sequence: 3,
                node_id: validation.id,
                failure_id,
                fingerprint,
            })
            .expect("attached current failed evidence proves the validation failure");
    }

    #[test]
    fn failure_events_enforce_identity_category_and_resolution_invariants() {
        let graph = graph();
        let source = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source mutation")
            .id
            .clone();
        let test = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::TestMutation)
            .expect("test mutation")
            .id
            .clone();
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-failure-invariants".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        let before = snapshot.clone();
        let invalid_category = snapshot
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 1,
                failure: FailureRecord::new(
                    "invalid-validation",
                    source.clone(),
                    FailureCategory::ValidationFailure,
                    1,
                    "tree-1",
                    "wrong node category",
                ),
            })
            .expect_err("validation failure cannot belong to a mutation node");
        assert!(invalid_category.message.contains("invalid for node"));
        assert_eq!(snapshot, before, "rejected failure event must be atomic");

        let mut failure = FailureRecord::new(
            "mutation-failure",
            source.clone(),
            FailureCategory::MutationConflict,
            1,
            "tree-1",
            "mutation conflict",
        );
        failure.target_path = Some("src/theme.ts".to_owned());
        snapshot
            .append_event(ExecutionDomainEvent::MutationRejected {
                sequence: 1,
                node_id: source.clone(),
                failure,
            })
            .expect("record valid failure");
        let before_wrong_resolution = snapshot.clone();
        let wrong_node = snapshot
            .append_event(ExecutionDomainEvent::FailureSuperseded {
                sequence: 2,
                node_id: test,
                failure_id: FailureId::new("mutation-failure"),
                repository_fingerprint: "tree-2".to_owned(),
            })
            .expect_err("resolution node must match failure node");
        assert!(wrong_node.message.contains("belongs to node"));
        assert_eq!(snapshot, before_wrong_resolution);

        snapshot
            .append_event(ExecutionDomainEvent::FailureSuperseded {
                sequence: 2,
                node_id: source.clone(),
                failure_id: FailureId::new("mutation-failure"),
                repository_fingerprint: "tree-2".to_owned(),
            })
            .expect("resolve valid mutation failure");
        let already_resolved = snapshot
            .append_event(ExecutionDomainEvent::FailureRecovered {
                sequence: 3,
                node_id: source,
                failure_id: FailureId::new("mutation-failure"),
                repository_fingerprint: "tree-2".to_owned(),
            })
            .expect_err("resolved failure cannot be recovered twice");
        assert!(already_resolved.message.contains("already resolved"));
    }

    #[test]
    fn success_events_cannot_bypass_graph_dependencies() {
        let graph = graph();
        let completion = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .expect("completion node")
            .id
            .clone();
        let publication = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Publication)
            .expect("publication node")
            .id
            .clone();
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-ordering".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };

        let completion_error = snapshot
            .append_event(ExecutionDomainEvent::CompletionEvaluated {
                sequence: 1,
                node_id: completion,
                outcome: MissionOutcome::Complete,
            })
            .expect_err("completion must wait for diff review");
        assert!(
            completion_error
                .message
                .contains("cannot advance before dependency")
        );

        let publication_error = snapshot
            .append_event(ExecutionDomainEvent::PublicationStarted {
                sequence: 1,
                node_id: publication,
                mode: PublicationMode::Normal,
            })
            .expect_err("publication must wait for completion evaluation");
        assert!(
            publication_error
                .message
                .contains("cannot advance before dependency")
        );
        assert!(snapshot.events.is_empty(), "rejected events must be atomic");

        let completion = snapshot
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .expect("completion node")
            .id
            .clone();
        snapshot
            .graph
            .set_node_status(&completion, ExecutionNodeStatus::Completed)
            .expect("inject malformed materialized status");
        assert!(
            snapshot.validate_invariants().is_err(),
            "deserialized status drift must not bypass dependency enforcement"
        );
    }

    #[test]
    fn finalization_invalidation_is_authoritative_and_replays_exactly() {
        let (mut initial, publication, validation_evidence_ids) = recovery_publication_snapshot();
        let review = initial
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::DiffReview)
            .expect("diff review")
            .id
            .clone();
        let completion = initial
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .expect("completion evaluation")
            .id
            .clone();
        initial
            .append_event(ExecutionDomainEvent::DiffReviewed {
                sequence: 1,
                node_id: review,
                evidence_ids: validation_evidence_ids,
            })
            .expect("review current diff");
        initial
            .append_event(ExecutionDomainEvent::CompletionEvaluated {
                sequence: 2,
                node_id: completion,
                outcome: MissionOutcome::Complete,
            })
            .expect("evaluate completion");
        initial
            .append_event(ExecutionDomainEvent::PublicationStarted {
                sequence: 3,
                node_id: publication,
                mode: PublicationMode::Normal,
            })
            .expect("start publication");

        let stale_validation_evidence_ids = initial.finalization_validation_evidence_ids();
        let event = ExecutionDomainEvent::FinalizationInvalidated {
            sequence: 4,
            repository_fingerprint: "tree-after-remote-reconciliation".to_owned(),
            stale_validation_evidence_ids: stale_validation_evidence_ids.clone(),
        };
        let encoded = serde_json::to_string(&event).expect("serialize invalidation event");
        let decoded: ExecutionDomainEvent =
            serde_json::from_str(&encoded).expect("deserialize invalidation event");

        let mut persisted = initial.clone();
        persisted
            .append_event(event)
            .expect("invalidate stale finalization");
        let mut replayed = initial;
        replayed
            .append_event(decoded)
            .expect("replay finalization invalidation");

        assert_eq!(replayed.graph, persisted.graph);
        assert_eq!(replayed.evidence, persisted.evidence);
        assert_eq!(replayed.publication, persisted.publication);
        assert_eq!(replayed.current_repository, persisted.current_repository);
        assert_eq!(replayed.events, persisted.events);
        assert_eq!(
            persisted.current_repository.fingerprint,
            "tree-after-remote-reconciliation"
        );
        assert_eq!(persisted.publication, PublicationState::default());
        assert!(persisted.graph.nodes.iter().all(|node| {
            !(node.kind.is_validation()
                || matches!(
                    node.kind,
                    ExecutionNodeKind::DiffReview
                        | ExecutionNodeKind::CompletionEvaluation
                        | ExecutionNodeKind::Publication
                ))
                || !node.status.is_success()
        }));
        for evidence_id in stale_validation_evidence_ids {
            assert_eq!(
                persisted.evidence.validations[&evidence_id].status,
                ValidationEvidenceStatus::Superseded
            );
        }
        persisted
            .validate_invariants()
            .expect("invalidated state remains graph-valid");
    }

    #[test]
    fn finalization_invalidation_rejects_noncanonical_evidence_and_empty_fingerprint() {
        let (snapshot, _, _) = recovery_publication_snapshot();
        let expected = snapshot.finalization_validation_evidence_ids();
        let mut missing = expected.clone();
        missing.pop();
        let error = snapshot
            .with_event(ExecutionDomainEvent::FinalizationInvalidated {
                sequence: 1,
                repository_fingerprint: "tree-2".to_owned(),
                stale_validation_evidence_ids: missing,
            })
            .expect_err("missing stale proof must fail closed");
        assert!(error.message.contains("exactly match"));
        let error = snapshot
            .with_event(ExecutionDomainEvent::FinalizationInvalidated {
                sequence: 1,
                repository_fingerprint: String::new(),
                stale_validation_evidence_ids: expected,
            })
            .expect_err("empty repository fingerprint must fail closed");
        assert!(error.message.contains("repository fingerprint"));
    }

    #[test]
    fn recovery_publication_uses_current_validation_proof_without_fabricating_completion() {
        let (initial, publication, validation_evidence_ids) = recovery_publication_snapshot();
        let events = vec![
            ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: 1,
                node_id: publication.clone(),
                repository_fingerprint: "tree-recovery".to_owned(),
                validation_evidence_ids,
            },
            ExecutionDomainEvent::CommitCreated {
                sequence: 2,
                node_id: publication.clone(),
                commit_sha: "recovery-commit".to_owned(),
            },
            ExecutionDomainEvent::BranchPushed {
                sequence: 3,
                node_id: publication.clone(),
                branch: "rustgrid/recovery".to_owned(),
            },
            ExecutionDomainEvent::PullRequestCreated {
                sequence: 4,
                node_id: publication,
                url: "https://example.test/pull/99".to_owned(),
                number: Some(99),
                draft: true,
            },
            ExecutionDomainEvent::RunFinished {
                sequence: 5,
                outcome: MissionOutcome::PartialReviewable,
            },
        ];
        let encoded = serde_json::to_string(&events).expect("serialize recovery event stream");
        let replay_events: Vec<ExecutionDomainEvent> =
            serde_json::from_str(&encoded).expect("deserialize recovery event stream");

        let mut persisted = initial.clone();
        for event in events {
            persisted.append_event(event).expect("apply recovery event");
        }
        let mut replayed = initial;
        for event in replay_events {
            replayed.append_event(event).expect("replay recovery event");
        }

        assert_eq!(replayed.graph, persisted.graph);
        assert_eq!(replayed.publication, persisted.publication);
        assert_eq!(replayed.events, persisted.events);
        assert!(persisted.graph.recovery_publication_dependency_override);
        assert_eq!(
            persisted.publication.mode,
            Some(PublicationMode::DraftRecovery)
        );
        assert!(persisted.publication.draft);
        assert!(persisted.publication.recovery_requested);
        assert!(persisted.publication.is_published());
        assert_eq!(
            persisted.terminal_outcome(),
            Some(MissionOutcome::PartialReviewable)
        );
        assert!(persisted.graph.nodes.iter().all(|node| {
            !matches!(
                node.kind,
                ExecutionNodeKind::DiffReview | ExecutionNodeKind::CompletionEvaluation
            ) || !node.status.is_success()
        }));
        assert!(persisted.graph.nodes.iter().all(|node| {
            !node.kind.is_validation() || node.status == ExecutionNodeStatus::Passed
        }));
        persisted
            .validate_invariants()
            .expect("draft recovery publication remains graph-valid");
    }

    #[test]
    fn recovery_publication_preserves_commit_and_push_progress_idempotently() {
        let cases = [
            (PublicationStatus::CommitCreated, None),
            (
                PublicationStatus::BranchPushed,
                Some("rustgrid/already-pushed".to_owned()),
            ),
        ];
        for (status, branch) in cases {
            let (mut snapshot, publication, validation_evidence_ids) =
                recovery_publication_snapshot();
            snapshot.publication.status = status;
            snapshot.publication.commit_sha = Some("trusted-existing-head".to_owned());
            snapshot.publication.branch = branch.clone();
            let request = ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: snapshot.next_event_sequence(),
                node_id: publication.clone(),
                repository_fingerprint: snapshot.current_repository.fingerprint.clone(),
                validation_evidence_ids: validation_evidence_ids.clone(),
            };

            snapshot
                .append_event(request)
                .expect("authorize recovery around persisted publication progress");
            assert_eq!(snapshot.publication.status, status);
            assert_eq!(
                snapshot.publication.commit_sha.as_deref(),
                Some("trusted-existing-head")
            );
            assert_eq!(snapshot.publication.branch, branch);
            assert_eq!(
                snapshot.publication.mode,
                Some(PublicationMode::DraftRecovery)
            );
            assert!(snapshot.publication.draft);
            assert!(snapshot.publication.recovery_requested);

            let repeated_request = ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: snapshot.next_event_sequence(),
                node_id: publication,
                repository_fingerprint: snapshot.current_repository.fingerprint.clone(),
                validation_evidence_ids,
            };
            snapshot
                .append_event(repeated_request)
                .expect("repeated recovery authorization is idempotent");
            assert_eq!(snapshot.publication.status, status);
            assert_eq!(
                snapshot.publication.commit_sha.as_deref(),
                Some("trusted-existing-head")
            );
            assert_eq!(snapshot.publication.branch, branch);
        }
    }

    #[test]
    fn resumed_validation_reuses_current_global_evidence_after_node_reset() {
        let (mut snapshot, publication, expected_evidence_ids) = recovery_publication_snapshot();
        for node in snapshot
            .graph
            .nodes
            .iter_mut()
            .filter(|node| node.kind.is_validation())
        {
            node.status = ExecutionNodeStatus::Pending;
            node.evidence_ids.clear();
        }
        snapshot.graph.refresh_readiness();

        assert_eq!(
            snapshot
                .current_required_validation_evidence_ids()
                .expect("current global validation proof remains reusable"),
            expected_evidence_ids
        );
        snapshot
            .append_event(ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: 1,
                node_id: publication,
                repository_fingerprint: "tree-recovery".to_owned(),
                validation_evidence_ids: expected_evidence_ids,
            })
            .expect("resumed current validation authorizes safe recovery publication");
    }

    #[test]
    fn same_fingerprint_validation_failure_revokes_prior_pass_for_recovery() {
        let (mut snapshot, publication, prior_evidence_ids) = recovery_publication_snapshot();
        let validation_id = snapshot
            .graph
            .nodes
            .iter()
            .rfind(|node| node.kind.is_validation())
            .expect("validation node")
            .id
            .clone();
        let validation_gate = snapshot
            .graph
            .node(&validation_id)
            .and_then(|node| node.validation.clone())
            .expect("validation gate");
        let validation_fingerprint = validation_gate.fingerprint("tree-recovery");
        let failure_id = FailureId::new("same-tree-validation-failure");
        snapshot
            .append_event(ExecutionDomainEvent::ValidationEvidenceRecorded {
                sequence: 1,
                node_id: validation_id.clone(),
                evidence: ValidationEvidenceRecord {
                    evidence_id: "same-tree-failed-evidence".to_owned(),
                    node_id: validation_id.clone(),
                    gate_id: validation_gate.gate_id,
                    fingerprint: validation_fingerprint.clone(),
                    repository_fingerprint: "tree-recovery".to_owned(),
                    command: validation_gate.command,
                    working_directory: validation_gate.working_directory,
                    status: ValidationEvidenceStatus::Failed,
                    exit_code: Some(1),
                    output_summary: "the rerun failed".to_owned(),
                    duration: Duration::from_millis(1),
                },
            })
            .expect("record current failed validation evidence");
        snapshot
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 2,
                failure: FailureRecord::new(
                    failure_id.clone(),
                    validation_id.clone(),
                    FailureCategory::ValidationFailure,
                    1,
                    "tree-recovery",
                    "the rerun failed on the same repository state",
                ),
            })
            .expect("record current validation failure");
        snapshot
            .append_event(ExecutionDomainEvent::ValidationFailed {
                sequence: 3,
                node_id: validation_id,
                failure_id,
                fingerprint: validation_fingerprint,
            })
            .expect("materialize current validation failure");

        let error = snapshot
            .current_required_validation_evidence_ids()
            .expect_err("unresolved current validation failure revokes an older pass");
        assert!(error.message.contains("unresolved failure"));
        let error = snapshot
            .append_event(ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: 4,
                node_id: publication,
                repository_fingerprint: "tree-recovery".to_owned(),
                validation_evidence_ids: prior_evidence_ids,
            })
            .expect_err("same-fingerprint failed rerun must deny recovery publication");
        assert!(error.message.contains("unresolved failure"));
    }

    #[test]
    fn recovery_publication_fails_closed_for_stale_or_incomplete_authorization() {
        let (snapshot, publication, validation_evidence_ids) = recovery_publication_snapshot();
        let request = |snapshot: &ExecutionSnapshot,
                       node_id: ExecutionNodeId,
                       repository_fingerprint: &str,
                       evidence_ids: Vec<String>| {
            snapshot.with_event(ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: snapshot.next_event_sequence(),
                node_id,
                repository_fingerprint: repository_fingerprint.to_owned(),
                validation_evidence_ids: evidence_ids,
            })
        };

        let mut no_diff = snapshot.clone();
        no_diff.current_repository.changed_paths.clear();
        assert!(
            request(
                &no_diff,
                publication.clone(),
                "tree-recovery",
                validation_evidence_ids.clone()
            )
            .expect_err("zero diff cannot be published")
            .message
            .contains("non-empty")
        );
        assert!(
            request(
                &snapshot,
                publication.clone(),
                "tree-stale",
                validation_evidence_ids.clone()
            )
            .expect_err("stale fingerprint cannot authorize publication")
            .message
            .contains("current repository fingerprint")
        );
        let mut missing = validation_evidence_ids.clone();
        missing.pop();
        assert!(
            request(&snapshot, publication.clone(), "tree-recovery", missing)
                .expect_err("missing validation proof cannot authorize publication")
                .message
                .contains("exactly match")
        );
        let mut duplicate = validation_evidence_ids.clone();
        duplicate.push(validation_evidence_ids[0].clone());
        assert!(
            request(&snapshot, publication.clone(), "tree-recovery", duplicate)
                .expect_err("duplicate validation proof cannot authorize publication")
                .message
                .contains("exactly match")
        );
        let mut unknown = validation_evidence_ids.clone();
        unknown[0] = "unknown-validation-evidence".to_owned();
        assert!(
            request(&snapshot, publication.clone(), "tree-recovery", unknown)
                .expect_err("unknown validation proof cannot authorize publication")
                .message
                .contains("exactly match")
        );
        let mut stale_validation = snapshot.clone();
        stale_validation
            .evidence
            .validations
            .get_mut(&validation_evidence_ids[0])
            .expect("validation evidence")
            .status = ValidationEvidenceStatus::Superseded;
        assert!(
            request(
                &stale_validation,
                publication.clone(),
                "tree-recovery",
                validation_evidence_ids.clone()
            )
            .expect_err("superseded validation cannot authorize publication")
            .message
            .contains("no current passed evidence")
        );
        let mutation = snapshot
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .expect("mutation node")
            .id
            .clone();
        assert!(
            request(
                &snapshot,
                mutation,
                "tree-recovery",
                validation_evidence_ids.clone()
            )
            .expect_err("recovery requires publication node")
            .message
            .contains("not a publication node")
        );

        let mut recovery = request(
            &snapshot,
            publication.clone(),
            "tree-recovery",
            validation_evidence_ids,
        )
        .expect("valid recovery request");
        let before_non_draft = recovery.clone();
        let error = recovery
            .append_event(ExecutionDomainEvent::PullRequestCreated {
                sequence: 2,
                node_id: publication.clone(),
                url: "https://example.test/pull/100".to_owned(),
                number: Some(100),
                draft: false,
            })
            .expect_err("recovery pull request must remain draft");
        assert!(error.message.contains("requires a draft"));
        assert_eq!(recovery, before_non_draft, "rejected event must be atomic");

        recovery
            .append_event(ExecutionDomainEvent::CommitCreated {
                sequence: 2,
                node_id: publication.clone(),
                commit_sha: "recovery-commit".to_owned(),
            })
            .expect("commit recovery work");
        recovery
            .append_event(ExecutionDomainEvent::BranchPushed {
                sequence: 3,
                node_id: publication.clone(),
                branch: "rustgrid/recovery".to_owned(),
            })
            .expect("push recovery work");
        recovery
            .append_event(ExecutionDomainEvent::PullRequestCreated {
                sequence: 4,
                node_id: publication.clone(),
                url: "https://example.test/pull/100".to_owned(),
                number: Some(100),
                draft: true,
            })
            .expect("publish recovery draft");
        assert!(
            request(
                &recovery,
                publication.clone(),
                "tree-recovery",
                recovery
                    .current_required_validation_evidence_ids()
                    .expect("current validation evidence")
            )
            .expect_err("completed publication cannot be replaced")
            .message
            .contains("cannot replace completed publication")
        );
        recovery
            .append_event(ExecutionDomainEvent::RunFinished {
                sequence: 5,
                outcome: MissionOutcome::PartialReviewable,
            })
            .expect("finish recovered publication");
        assert!(
            request(
                &recovery,
                publication,
                "tree-recovery",
                recovery
                    .current_required_validation_evidence_ids()
                    .expect("current validation evidence")
            )
            .expect_err("terminal execution cannot request recovery")
            .message
            .contains("cannot be appended after RunFinished")
        );
    }

    #[test]
    fn partial_guardrail_satisfies_edges_without_erasing_remaining_targets() {
        let mut graph = graph();
        let source = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source node")
            .id
            .clone();
        let pending_test = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::TestMutation)
            .expect("test node")
            .id
            .clone();
        graph
            .set_node_status(&source, ExecutionNodeStatus::Applied)
            .expect("apply useful source work");
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-partial".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-2".to_owned(),
                changed_paths: BTreeSet::from(["src/theme.ts".to_owned()]),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        snapshot
            .append_event(ExecutionDomainEvent::GuardrailTriggered {
                sequence: 1,
                reason: GuardrailReason::NodeBudgetExhausted,
                outcome: MissionOutcome::PartialReviewable,
                detail: "useful source work is ready for validation".to_owned(),
            })
            .expect("enter partial validation path");

        assert!(
            snapshot
                .graph
                .dependency_satisfaction_overrides
                .contains(&pending_test)
        );
        assert!(
            snapshot
                .remaining_required_nodes()
                .iter()
                .any(|node| node.id == pending_test),
            "partial dependency satisfaction must not erase remaining work"
        );
        let validation = snapshot
            .graph
            .next_runnable_node()
            .expect("validation becomes runnable");
        assert!(validation.kind.is_validation());
        snapshot.validate_invariants().expect("valid partial graph");
    }

    #[test]
    fn partial_route_reaches_draft_publication_without_erasing_remaining_targets() {
        let mut graph = graph();
        let source = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source node")
            .id
            .clone();
        let pending_test = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::TestMutation)
            .expect("test node")
            .id
            .clone();
        let validations = graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_validation())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        let review = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::DiffReview)
            .expect("diff review node")
            .id
            .clone();
        let completion = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .expect("completion node")
            .id
            .clone();
        let publication = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Publication)
            .expect("publication node")
            .id
            .clone();
        graph
            .set_node_status(&source, ExecutionNodeStatus::Applied)
            .expect("apply useful source work");
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-partial-publication".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-2".to_owned(),
                changed_paths: BTreeSet::from(["src/theme.ts".to_owned()]),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        snapshot
            .append_event(ExecutionDomainEvent::GuardrailTriggered {
                sequence: 1,
                reason: GuardrailReason::NodeBudgetExhausted,
                outcome: MissionOutcome::PartialReviewable,
                detail: "validate and publish the useful partial diff".to_owned(),
            })
            .expect("enter partial validation path");

        let mut sequence = 2;
        for node_id in validations {
            let gate = snapshot
                .graph
                .node(&node_id)
                .and_then(|node| node.validation.clone())
                .expect("validation gate");
            let validation_fingerprint = gate.fingerprint("tree-2");
            snapshot
                .append_event(ExecutionDomainEvent::ValidationStarted {
                    sequence,
                    node_id: node_id.clone(),
                    fingerprint: validation_fingerprint.clone(),
                })
                .expect("start validation in dependency order");
            sequence += 1;
            let evidence_id = format!("validation-{sequence}");
            snapshot
                .append_event(ExecutionDomainEvent::ValidationEvidenceRecorded {
                    sequence,
                    node_id: node_id.clone(),
                    evidence: ValidationEvidenceRecord {
                        evidence_id: evidence_id.clone(),
                        node_id: node_id.clone(),
                        gate_id: gate.gate_id,
                        fingerprint: validation_fingerprint.clone(),
                        repository_fingerprint: "tree-2".to_owned(),
                        command: gate.command,
                        working_directory: gate.working_directory,
                        status: ValidationEvidenceStatus::Passed,
                        exit_code: Some(0),
                        output_summary: "validation passed".to_owned(),
                        duration: Duration::from_millis(1),
                    },
                })
                .expect("record validation evidence in dependency order");
            sequence += 1;
            snapshot
                .append_event(ExecutionDomainEvent::ValidationPassed {
                    sequence,
                    node_id,
                    evidence_id,
                    fingerprint: validation_fingerprint,
                })
                .expect("pass validation in dependency order");
            sequence += 1;
        }
        snapshot
            .append_event(ExecutionDomainEvent::DiffReviewed {
                sequence,
                node_id: review,
                evidence_ids: vec!["diff-review".to_owned()],
            })
            .expect("review validated partial diff");
        sequence += 1;
        snapshot
            .append_event(ExecutionDomainEvent::CompletionEvaluated {
                sequence,
                node_id: completion,
                outcome: MissionOutcome::PartialReviewable,
            })
            .expect("evaluate partial completion after review");
        sequence += 1;
        snapshot
            .append_event(ExecutionDomainEvent::PublicationStarted {
                sequence,
                node_id: publication.clone(),
                mode: PublicationMode::Draft,
            })
            .expect("start draft publication");
        sequence += 1;
        snapshot
            .append_event(ExecutionDomainEvent::CommitCreated {
                sequence,
                node_id: publication.clone(),
                commit_sha: "partial-commit".to_owned(),
            })
            .expect("record partial commit");
        sequence += 1;
        snapshot
            .append_event(ExecutionDomainEvent::BranchPushed {
                sequence,
                node_id: publication.clone(),
                branch: "rustgrid/partial".to_owned(),
            })
            .expect("record partial branch");
        sequence += 1;
        snapshot
            .append_event(ExecutionDomainEvent::PullRequestCreated {
                sequence,
                node_id: publication,
                url: "https://example.test/pull/42".to_owned(),
                number: Some(42),
                draft: true,
            })
            .expect("publish draft pull request");
        sequence += 1;
        snapshot
            .append_event(ExecutionDomainEvent::RunFinished {
                sequence,
                outcome: MissionOutcome::PartialReviewable,
            })
            .expect("finish as partial reviewable");

        assert_eq!(
            snapshot.terminal_outcome(),
            Some(MissionOutcome::PartialReviewable)
        );
        assert!(snapshot.publication.is_published());
        assert!(snapshot.publication.draft);
        assert!(
            snapshot
                .remaining_required_nodes()
                .iter()
                .any(|node| node.id == pending_test),
            "publishing a partial result must preserve explicit remaining mutation work"
        );
        snapshot
            .validate_invariants()
            .expect("partial validation-to-publication route remains graph-valid");
    }

    #[test]
    fn node_started_records_and_bounds_target_repair_attempts() {
        let mut graph = graph();
        let node_id = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source node")
            .id
            .clone();
        graph
            .set_node_status(&node_id, ExecutionNodeStatus::FailedRecoverable)
            .expect("mark recoverable failure");
        let mut failure = FailureRecord::new(
            "repairable",
            node_id.clone(),
            FailureCategory::MutationConflict,
            1,
            "tree-1",
            "replacement did not match",
        );
        failure.target_path = Some("src/theme.ts".to_owned());
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-repair-budget".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            failures: FailureStore::default(),
            ..ExecutionSnapshot::default()
        };
        snapshot.failures.record(failure);

        for attempt in 1..=2 {
            snapshot
                .graph
                .set_node_status(&node_id, ExecutionNodeStatus::FailedRecoverable)
                .expect("repair remains recoverable");
            snapshot
                .append_event(ExecutionDomainEvent::NodeStarted {
                    sequence: u64::from(attempt),
                    node_id: node_id.clone(),
                    attempt,
                    started_at: format!("attempt-{attempt}"),
                    repository_fingerprint: "tree-1".to_owned(),
                })
                .expect("bounded repair start");
        }
        assert_eq!(snapshot.budget.usage_for(&node_id).repair_attempts, 2);
        snapshot
            .graph
            .set_node_status(&node_id, ExecutionNodeStatus::FailedRecoverable)
            .expect("third repair request");
        let error = snapshot
            .append_event(ExecutionDomainEvent::NodeStarted {
                sequence: 3,
                node_id: node_id.clone(),
                attempt: 3,
                started_at: "attempt-3".to_owned(),
                repository_fingerprint: "tree-1".to_owned(),
            })
            .expect_err("repair budget must be hard bounded");
        assert!(error.message.contains("cannot start repair beyond"));
        assert_eq!(snapshot.budget.usage_for(&node_id).repair_attempts, 2);
    }

    #[test]
    fn tiny_first_repair_is_counted_once_by_the_authoritative_event() {
        let mut graph = ExecutionGraph::from_targets(
            "tiny-repair",
            MissionComplexity::Tiny,
            "tree-1",
            &[target("src/tiny.rs", "production")],
            &[],
            &MissionBudget::for_complexity(MissionComplexity::Tiny),
        );
        let node_id = graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .expect("tiny mutation node")
            .id
            .clone();
        graph
            .set_node_status(&node_id, ExecutionNodeStatus::FailedRecoverable)
            .unwrap();
        let mut snapshot = ExecutionSnapshot {
            run_id: "tiny-repair".into(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".into(),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Tiny)),
            ..ExecutionSnapshot::default()
        };
        snapshot
            .append_event(ExecutionDomainEvent::NodeStarted {
                sequence: 1,
                node_id: node_id.clone(),
                attempt: 1,
                started_at: "first-repair".into(),
                repository_fingerprint: "tree-1".into(),
            })
            .expect("first tiny repair starts");
        assert_eq!(snapshot.budget.usage_for(&node_id).repair_attempts, 1);
        assert_eq!(
            snapshot.graph.node(&node_id).map(|node| node.status),
            Some(ExecutionNodeStatus::Running),
            "a repeated production decision is idempotent because it emits no second NodeStarted"
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter(|event| matches!(event, ExecutionDomainEvent::NodeStarted { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn progress_extends_soft_budget_but_never_the_hard_budget() {
        let node_id = ExecutionNodeId::new("target-1");
        let node_budget = NodeBudget {
            max_model_calls: 10,
            max_cost_micros: 10_000,
            max_duration: Duration::from_secs(100),
            max_repair_attempts: 1,
        };
        let mut state = BudgetState::new(MissionBudget {
            max_model_calls: 20,
            max_cost_micros: 20_000,
            max_duration: Duration::from_secs(200),
            max_target_repair_rounds: 2,
        });
        for _ in 0..8 {
            state.record_model_call(node_id.clone(), 100, Duration::from_secs(1));
        }
        assert!(state.should_stop_node(&node_id, &node_budget));
        state.record_progress_kind(
            1,
            ProgressEventKind::SourceMutationApplied,
            Some(node_id.clone()),
        );
        state.record_model_call(node_id.clone(), 100, Duration::from_secs(1));
        assert!(!state.should_stop_node(&node_id, &node_budget));
        state.record_model_call(node_id.clone(), 100, Duration::from_secs(1));
        assert!(state.should_stop_node(&node_id, &node_budget));
    }

    #[test]
    fn newer_attempt_resumes_from_a_cancellation_checkpoint_via_event() {
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-cancelled".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: graph(),
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        snapshot
            .append_event(ExecutionDomainEvent::CancellationRequested {
                sequence: 1,
                state: CancellationState {
                    requested_at: "attempt-1".to_owned(),
                    reason: "user requested cancellation".to_owned(),
                    checkpointed: true,
                    ..CancellationState::default()
                },
            })
            .expect("checkpoint cancellation");
        assert!(snapshot.cancellation.is_some());
        assert!(!snapshot.is_terminal());

        snapshot
            .append_event(ExecutionDomainEvent::ExecutionResumed {
                sequence: 2,
                execution_attempt: 2,
                previous_outcome: None,
            })
            .expect("resume newer attempt");

        assert!(snapshot.cancellation.is_none());
        assert!(!snapshot.is_terminal());
        snapshot.validate_invariants().expect("resumed snapshot");
    }

    #[test]
    fn execution_resume_requires_a_cancellation_checkpoint() {
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-active".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: graph(),
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };

        let error = snapshot
            .append_event(ExecutionDomainEvent::ExecutionResumed {
                sequence: 1,
                execution_attempt: 2,
                previous_outcome: None,
            })
            .expect_err("active execution must not emit a resume event");
        assert!(
            error
                .message
                .contains("cancellation checkpoint or partial-reviewable")
        );
    }

    #[test]
    fn terminal_event_prevents_domain_result_replacement() {
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-1".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: graph(),
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        snapshot
            .append_event(ExecutionDomainEvent::RunFinished {
                sequence: 1,
                outcome: MissionOutcome::BlockedNoDiff,
            })
            .expect("finish run");
        let error = snapshot
            .append_event(ExecutionDomainEvent::RunFinished {
                sequence: 2,
                outcome: MissionOutcome::FailedInfrastructure,
            })
            .expect_err("terminal result is authoritative");
        assert!(error.message.contains("after RunFinished"));
        assert_eq!(
            snapshot.terminal_outcome(),
            Some(MissionOutcome::BlockedNoDiff)
        );
    }
}
