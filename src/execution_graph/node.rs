#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionNodeKind {
    Discovery,
    Planning,
    #[default]
    SourceMutation,
    TestMutation,
    ValidationRepairSession,
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
            Self::SourceMutation | Self::TestMutation | Self::ValidationRepairSession => {
                HostedExecutionStage::Implementation
            }
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
                | Self::ValidationRepairSession
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

pub type RepositoryPath = String;
pub type ContentHash = String;

#[derive(Clone, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetOperation {
    #[default]
    ModifyExisting,
    CreateNew,
    DeleteExisting,
    Rename {
        source: RepositoryPath,
        destination: RepositoryPath,
    },
    Move {
        source: RepositoryPath,
        destination: RepositoryPath,
    },
}

impl TargetOperation {
    pub fn source_path(&self) -> Option<&str> {
        match self {
            Self::Rename { source, .. } | Self::Move { source, .. } => Some(source),
            Self::ModifyExisting | Self::CreateNew | Self::DeleteExisting => None,
        }
    }

    pub fn destination_path<'a>(&'a self, fallback: &'a str) -> &'a str {
        match self {
            Self::Rename { destination, .. } | Self::Move { destination, .. } => destination,
            Self::ModifyExisting | Self::CreateNew | Self::DeleteExisting => fallback,
        }
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::ModifyExisting => "modify_existing",
            Self::CreateNew => "create_new",
            Self::DeleteExisting => "delete_existing",
            Self::Rename { .. } => "rename",
            Self::Move { .. } => "move",
        }
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
    #[serde(default)]
    pub operation: TargetOperation,
}

/// A repository mutation target is plan-owned data, not a generic graph id.
pub type MutationTarget = PlannedTarget;

impl PlannedTarget {
    pub fn effective_operation(&self) -> TargetOperation {
        if self.new_file && self.operation == TargetOperation::ModifyExisting {
            TargetOperation::CreateNew
        } else {
            self.operation.clone()
        }
    }
    pub fn mutation_target_id(&self) -> MutationTargetId {
        let operation = self.effective_operation();
        MutationTargetId::new(format!(
            "{}:{}:{}:{}:{}:{}",
            self.change_id,
            self.path,
            self.role,
            operation.as_str(),
            operation.source_path().unwrap_or_default(),
            operation.destination_path(&self.path),
        ))
    }

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

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationTimeoutPolicy {
    #[serde(with = "duration_millis")]
    pub startup_grace: Duration,
    #[serde(with = "duration_millis")]
    pub execution_timeout: Duration,
    #[serde(default, with = "optional_duration_millis")]
    pub inactivity_timeout: Option<Duration>,
    #[serde(with = "duration_millis")]
    pub absolute_timeout: Duration,
}

impl ValidationTimeoutPolicy {
    pub fn for_gate(gate_type: ValidationGateType) -> Self {
        let (execution_seconds, absolute_seconds) = match gate_type {
            ValidationGateType::FocusedTest => (90, 120),
            ValidationGateType::TestSuite => (240, 300),
            ValidationGateType::Build => (180, 240),
            ValidationGateType::Lint | ValidationGateType::Typecheck => (120, 180),
            ValidationGateType::Custom => (120, 180),
        };
        Self {
            startup_grace: Duration::from_secs(15),
            execution_timeout: Duration::from_secs(execution_seconds),
            inactivity_timeout: Some(Duration::from_secs(execution_seconds)),
            absolute_timeout: Duration::from_secs(absolute_seconds),
        }
    }

    pub fn dependency_install() -> Self {
        Self {
            startup_grace: Duration::from_secs(30),
            execution_timeout: Duration::from_secs(300),
            inactivity_timeout: Some(Duration::from_secs(300)),
            absolute_timeout: Duration::from_secs(360),
        }
    }

    pub fn clamped_to(&self, remaining: Duration) -> Self {
        Self {
            startup_grace: self.startup_grace.min(remaining),
            execution_timeout: self.execution_timeout.min(remaining),
            inactivity_timeout: self.inactivity_timeout.map(|value| value.min(remaining)),
            absolute_timeout: self.absolute_timeout.min(remaining),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRetryPolicy {
    Never,
    TransientInfrastructureOnce,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationNodeBudget {
    #[serde(with = "duration_millis")]
    pub scheduling_deadline: Duration,
    pub process_timeout: ValidationTimeoutPolicy,
    pub retry_policy: ValidationRetryPolicy,
}

impl ValidationNodeBudget {
    pub fn for_gate(gate_type: ValidationGateType, scheduling_deadline: Duration) -> Self {
        Self {
            scheduling_deadline,
            process_timeout: ValidationTimeoutPolicy::for_gate(gate_type),
            retry_policy: ValidationRetryPolicy::TransientInfrastructureOnce,
        }
    }
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
        let normalized = normalize_command(&gate.command).to_ascii_lowercase();
        let dependency_install = [
            "npm ci",
            "npm install",
            "pnpm install",
            "yarn install",
            "bun install",
        ]
        .iter()
        .any(|prefix| normalized.starts_with(prefix));
        let browser_e2e = ["playwright", "cypress", "browser", " e2e"]
            .iter()
            .any(|needle| normalized.contains(needle));
        let class = if dependency_install {
            0_u8
        } else if gate.gate_type == ValidationGateType::FocusedTest {
            1
        } else if matches!(
            gate.gate_type,
            ValidationGateType::Lint | ValidationGateType::Typecheck
        ) {
            2
        } else if gate.gate_type == ValidationGateType::TestSuite && !browser_e2e {
            3
        } else if gate.gate_type == ValidationGateType::Build {
            4
        } else if browser_e2e {
            5
        } else {
            6
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
            model_calls_remaining: self.max_model_calls.saturating_sub(
                usage
                    .model_calls_consumed
                    .saturating_add(usage.model_calls_reserved),
            ),
            cost_micros: self
                .max_cost_micros
                .saturating_sub(usage.cost_micros.saturating_add(usage.cost_micros_reserved)),
            duration: self.max_duration.saturating_sub(usage.duration),
            repair_attempts: self
                .max_repair_attempts
                .saturating_sub(usage.repair_attempts),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct NodeBudgetRemaining {
    #[serde(default, alias = "model_calls")]
    pub model_calls_remaining: u32,
    pub cost_micros: u64,
    #[serde(with = "duration_millis")]
    pub duration: Duration,
    pub repair_attempts: u32,
}

impl NodeBudgetRemaining {
    pub fn exhausted(&self) -> bool {
        self.model_calls_remaining == 0 || self.cost_micros == 0 || self.duration.is_zero()
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
    #[serde(default)]
    pub operation_evidence: Vec<OperationEvidence>,
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
