#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    ReadFile,
    SearchRepository,
    ApplyPatch,
    CreateFile,
    DeleteFile,
    RenameFile,
    MoveFile,
    RunFocusedCommand,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetStateProbe {
    pub operation: TargetOperation,
    pub target_path: RepositoryPath,
    pub target_exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_exists: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_content_hash: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_content_hash: Option<ContentHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_result_content_hash: Option<ContentHash>,
    pub repository_fingerprint: RepositoryFingerprint,
}

impl TargetStateProbe {
    pub fn inspection_outcome(&self) -> TargetInspectionOutcome {
        let conflict = |code: &str, message: &str| TargetInspectionOutcome::OperationConflict {
            conflict: TargetOperationConflict {
                code: code.to_owned(),
                operation: self.operation.clone(),
                target_path: self.target_path.clone(),
                source_path: self.operation.source_path().map(str::to_owned),
                message: message.to_owned(),
                recoverable: true,
            },
        };
        match &self.operation {
            TargetOperation::ModifyExisting if self.target_exists => {
                TargetInspectionOutcome::ExistingTargetLoaded
            }
            TargetOperation::ModifyExisting => conflict(
                "expected_existing_target_missing",
                "the accepted modify target is absent",
            ),
            TargetOperation::CreateNew if !self.target_exists => {
                TargetInspectionOutcome::NewTargetConfirmedAbsent
            }
            TargetOperation::CreateNew
                if self.expected_result_content_hash.is_some()
                    && self.expected_result_content_hash == self.target_content_hash =>
            {
                TargetInspectionOutcome::AlreadyApplied
            }
            TargetOperation::CreateNew => conflict(
                "create_target_already_exists",
                "the accepted create destination exists without matching mutation intent",
            ),
            TargetOperation::DeleteExisting if self.target_exists => {
                TargetInspectionOutcome::ExistingTargetLoaded
            }
            TargetOperation::DeleteExisting => TargetInspectionOutcome::AlreadyApplied,
            TargetOperation::Rename { .. } | TargetOperation::Move { .. }
                if self.source_exists == Some(true) && !self.target_exists =>
            {
                TargetInspectionOutcome::ExistingTargetLoaded
            }
            TargetOperation::Rename { .. } | TargetOperation::Move { .. }
                if self.source_exists == Some(false)
                    && self.target_exists
                    && self.expected_result_content_hash.is_some()
                    && self.expected_result_content_hash == self.target_content_hash =>
            {
                TargetInspectionOutcome::AlreadyApplied
            }
            TargetOperation::Rename { .. } | TargetOperation::Move { .. }
                if self.source_exists == Some(false) && self.target_exists =>
            {
                conflict(
                    "destination_content_mismatch",
                    "the destination does not match the accepted source evidence",
                )
            }
            TargetOperation::Rename { .. } | TargetOperation::Move { .. }
                if self.source_exists == Some(true) =>
            {
                conflict(
                    "destination_already_exists",
                    "the accepted destination already exists",
                )
            }
            TargetOperation::Rename { .. } | TargetOperation::Move { .. } => conflict(
                "expected_source_target_missing",
                "the accepted source and destination are both absent",
            ),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetOperationConflict {
    pub code: String,
    pub operation: TargetOperation,
    pub target_path: RepositoryPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<RepositoryPath>,
    pub message: String,
    #[serde(default)]
    pub recoverable: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TargetInspectionOutcome {
    ExistingTargetLoaded,
    NewTargetConfirmedAbsent,
    AlreadyApplied,
    OperationConflict { conflict: TargetOperationConflict },
    UnsafePath,
    #[default]
    InspectionInfrastructureFailure,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct CreateTargetSpecification {
    pub path: RepositoryPath,
    pub role: String,
    pub intent: String,
    #[serde(default)]
    pub acceptance_criteria_ids: Vec<String>,
    #[serde(default)]
    pub related_evidence_ids: Vec<EvidenceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_artifact_kind: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct CreatedTargetEvidence {
    pub path: RepositoryPath,
    pub content_hash: ContentHash,
    pub repository_fingerprint_before: RepositoryFingerprint,
    pub repository_fingerprint_after: RepositoryFingerprint,
    pub creation_tool: String,
    #[serde(default)]
    pub validation_gate_ids: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_state_probe: Option<TargetStateProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspection_outcome: Option<TargetInspectionOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_specification: Option<CreateTargetSpecification>,
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

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRepairAction {
    BuildRepairEvidence,
    DiagnoseFailure,
    SelectRepairTarget,
    MutateRepairTarget,
    VerifyRepair,
    RerunFailedGate,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationAssertionFailure {
    pub test_file: String,
    #[serde(default)]
    pub suite_path: Vec<String>,
    pub test_name: String,
    pub source_location: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_column: Option<u32>,
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
    pub context: String,
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
    PlanRepositoryConflict,
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
                | Self::PlanRepositoryConflict
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
        matches!(
            self,
            Self::ToolRecoverable | Self::MutationConflict | Self::PlanRepositoryConflict
        )
    }

    const fn node_status(self) -> ExecutionNodeStatus {
        match self {
            Self::ModelArtifactRecoverable
            | Self::ToolRecoverable
            | Self::MutationConflict
            | Self::PlanRepositoryConflict
            | Self::ValidationFailure => ExecutionNodeStatus::FailedRecoverable,
            Self::TargetBlocked
            | Self::InfrastructureFailure
            | Self::OrchestrationInvariantViolation
            | Self::UserCancellation => ExecutionNodeStatus::FailedBlocking,
        }
    }

    const fn is_valid_for_node_kind(self, kind: ExecutionNodeKind) -> bool {
        match self {
            Self::MutationConflict | Self::PlanRepositoryConflict | Self::TargetBlocked => {
                kind.is_mutation()
            }
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

/// A repository mutation failure classified before orchestration chooses the
/// next bounded action. This value is persisted as data; callers must never
/// recover it by parsing a human-readable diagnostic.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationApplicationFailure {
    InvalidPatchTarget,
    InvalidPatchSyntax,
    PatchContextMismatch,
    PatchWouldModifyUnexpectedPath,
    ReplacementContentInvalid,
    RepositoryChangedSinceContext,
    MutationProducedNoChange,
    CreateTargetAlreadyExists,
    DeleteTargetMissing,
    RenameDestinationConflict,
}

impl MutationApplicationFailure {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidPatchTarget => "invalid_patch_target",
            Self::InvalidPatchSyntax => "invalid_patch_syntax",
            Self::PatchContextMismatch => "patch_context_mismatch",
            Self::PatchWouldModifyUnexpectedPath => "patch_would_modify_unexpected_path",
            Self::ReplacementContentInvalid => "replacement_content_invalid",
            Self::RepositoryChangedSinceContext => "repository_changed_since_context",
            Self::MutationProducedNoChange => "mutation_produced_no_change",
            Self::CreateTargetAlreadyExists => "create_target_already_exists",
            Self::DeleteTargetMissing => "delete_target_missing",
            Self::RenameDestinationConflict => "rename_destination_conflict",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "invalid_patch_target" => Self::InvalidPatchTarget,
            "invalid_patch_syntax" => Self::InvalidPatchSyntax,
            "patch_context_mismatch" => Self::PatchContextMismatch,
            "patch_would_modify_unexpected_path" => Self::PatchWouldModifyUnexpectedPath,
            "replacement_content_invalid" => Self::ReplacementContentInvalid,
            "repository_changed_since_context" => Self::RepositoryChangedSinceContext,
            "mutation_produced_no_change" => Self::MutationProducedNoChange,
            "create_target_already_exists" => Self::CreateTargetAlreadyExists,
            "delete_target_missing" => Self::DeleteTargetMissing,
            "rename_destination_conflict" | "destination_already_exists" => {
                Self::RenameDestinationConflict
            }
            _ => return None,
        })
    }

    pub const fn uses_replacement_threshold(self) -> bool {
        matches!(
            self,
            Self::InvalidPatchTarget
                | Self::InvalidPatchSyntax
                | Self::PatchContextMismatch
                | Self::PatchWouldModifyUnexpectedPath
                | Self::ReplacementContentInvalid
                | Self::MutationProducedNoChange
        )
    }
}

/// Executable policy for the next target-repair request.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationFallbackPolicy {
    ForceReplaceFile,
    ForceCreateFile,
    ForceDeleteFile,
    ForceRename,
    RebuildTargetContext,
    RetryPatchWithNormalizedPayload,
    #[default]
    NoSafeFallback,
}

impl MutationFallbackPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ForceReplaceFile => "force_replace_file",
            Self::ForceCreateFile => "force_create_file",
            Self::ForceDeleteFile => "force_delete_file",
            Self::ForceRename => "force_rename",
            Self::RebuildTargetContext => "rebuild_target_context",
            Self::RetryPatchWithNormalizedPayload => "retry_patch_with_normalized_payload",
            Self::NoSafeFallback => "no_safe_fallback",
        }
    }

    pub const fn permitted_tools(self) -> &'static [&'static str] {
        match self {
            Self::ForceReplaceFile => &["replace_file"],
            Self::ForceCreateFile => &["create_file"],
            Self::ForceDeleteFile => &["delete_file"],
            Self::ForceRename => &["rename_file"],
            Self::RetryPatchWithNormalizedPayload => &["apply_patch"],
            Self::RebuildTargetContext | Self::NoSafeFallback => &[],
        }
    }

    pub const fn forced_tool(self) -> Option<&'static str> {
        match self.permitted_tools() {
            [tool] => Some(tool),
            _ => None,
        }
    }

    pub const fn requires_provider_mutation(self) -> bool {
        self.forced_tool().is_some()
    }

    pub const fn compatible_with(self, operation: &TargetOperation) -> bool {
        matches!(
            (self, operation),
            (
                Self::ForceReplaceFile | Self::RetryPatchWithNormalizedPayload,
                TargetOperation::ModifyExisting
            ) | (Self::ForceCreateFile, TargetOperation::CreateNew)
                | (Self::ForceDeleteFile, TargetOperation::DeleteExisting)
                | (
                    Self::ForceRename,
                    TargetOperation::Rename { .. } | TargetOperation::Move { .. }
                )
                | (Self::RebuildTargetContext | Self::NoSafeFallback, _)
        )
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TargetAttemptAccounting {
    pub primary_mutation_calls: u32,
    pub mutation_repair_calls: u32,
    pub context_rebuilds: u32,
    pub repository_write_attempts: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct MutationStrategyFingerprint {
    pub operation: TargetOperation,
    pub tool: String,
    pub fallback_policy: MutationFallbackPolicy,
    pub payload_type: String,
    pub failure_category: MutationApplicationFailure,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct MutationDiagnostics {
    pub message: String,
    #[serde(default)]
    pub normalized_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_check: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RejectedMutation {
    pub tool: String,
    pub payload_hash: String,
    pub failure_category: MutationApplicationFailure,
    pub failure_diagnostics: MutationDiagnostics,
    pub repository_fingerprint: RepositoryFingerprint,
    pub applied: bool,
    #[serde(default)]
    pub status: FailureStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_repository_fingerprint: Option<RepositoryFingerprint>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct RepairRequestPreflight {
    pub policy_present: bool,
    pub policy_compatible_with_operation: bool,
    pub exact_target_bound: bool,
    pub required_content_present: bool,
    pub target_hash_present: bool,
    pub repository_fingerprint_present: bool,
    pub tool_surface_matches_policy: bool,
    pub forced_tool_choice_matches_policy: bool,
}

impl RepairRequestPreflight {
    pub const fn passed(&self) -> bool {
        self.policy_present
            && self.policy_compatible_with_operation
            && self.exact_target_bound
            && self.required_content_present
            && self.target_hash_present
            && self.repository_fingerprint_present
            && self.tool_surface_matches_policy
            && self.forced_tool_choice_matches_policy
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct MutationToolPolicyViolation {
    pub node_id: ExecutionNodeId,
    pub target_path: String,
    pub active_policy: MutationFallbackPolicy,
    pub expected_tools: Vec<String>,
    pub received_tool: String,
}

impl fmt::Display for MutationToolPolicyViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "mutation_tool_policy_violation: policy {:?} permits {:?}, received `{}` for `{}`",
            self.active_policy, self.expected_tools, self.received_tool, self.target_path
        )
    }
}

impl std::error::Error for MutationToolPolicyViolation {}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct FailureRecord {
    pub id: FailureId,
    pub node_id: ExecutionNodeId,
    pub target_path: Option<String>,
    pub category: FailureCategory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
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
