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
    pub target_content_hash: Option<String>,
    #[serde(default)]
    pub repository_fingerprint: String,
    #[serde(default)]
    pub accepted_intent_hash: String,
    #[serde(default)]
    pub nearby_context: Vec<FileExcerpt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_repair: Option<ValidationRepairContext>,
    #[serde(default)]
    pub allowed_tools: Vec<ToolKind>,
    pub remaining_node_budget: NodeBudgetRemaining,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationRepairContext {
    pub focused_validation_command: String,
    #[serde(default)]
    pub assertion_failures: Vec<ValidationAssertionFailure>,
    #[serde(default)]
    pub implicated_targets: Vec<FileExcerpt>,
    pub selected_target: String,
    pub repository_fingerprint: String,
    pub accepted_implementation_intent: String,
    #[serde(default)]
    pub existing_diff_paths: Vec<String>,
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

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRepairDiagnosis {
    SourceDefect,
    TestExpectationDefect,
    Both,
    Inconclusive,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationAssertionFailure {
    pub test_file: String,
    pub test_name: String,
    pub source_location: String,
    pub assertion_kind: String,
    pub expected: String,
    pub received: String,
    #[serde(default)]
    pub implicated_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<ValidationRepairDiagnosis>,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub proposed_repair: String,
    #[serde(default)]
    pub expected_validation_effect: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RepairResult {
    MutationProduced {
        selected_target: String,
    },
    NoMutation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnosis: Option<ValidationRepairDiagnosis>,
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationStatus {
    #[default]
    Planned,
    Running,
    Applied,
    FailedRecoverable,
    FailedBlocking,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    #[default]
    Pending,
    Passed,
    FailedCode,
    FailedInfrastructure,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetState {
    pub mutation_status: MutationStatus,
    pub validation_status: ValidationStatus,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_command: Option<String>,
    #[serde(default)]
    pub assertion_failures: Vec<ValidationAssertionFailure>,
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
