// Extracted from the hosted execution composition root.
use super::*;

#[derive(Deserialize)]
pub(super) struct GithubTokenResponse {
    pub(super) token: String,
    pub(super) expires_at: String,
    pub(super) permissions: Value,
    pub(super) repository: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct HostedManifest {
    pub(super) manifest_version: i32,
    #[serde(default)]
    pub(super) model_call_budget: Option<i32>,
    #[serde(default)]
    pub(super) requested_model_call_budget: Option<i32>,
    #[serde(default)]
    pub(super) resolved_model_call_budget: Option<i32>,
    #[serde(default)]
    pub(super) budget_source: Option<BudgetSource>,
    #[serde(default)]
    pub(super) clamped: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_present_nullable")]
    pub(super) clamp_reason: Option<Option<String>>,
    pub(super) execution: ManifestExecution,
    pub(super) run: ManifestRun,
    pub(super) project_id: Uuid,
    pub(super) project_key: String,
    pub(super) project_name: String,
    pub(super) ticket_id: Uuid,
    pub(super) ticket_key: String,
    pub(super) ticket_title: String,
    pub(super) github: HostedGithubManifest,
    pub(super) ai_gateway: HostedAiManifest,
    pub(super) execution_policy: HostedExecutionPolicy,
    pub(super) execution_policy_sha256: String,
    pub(super) heartbeat_url: String,
    pub(super) token_refresh_url: String,
    pub(super) events_url: String,
    pub(super) telemetry_url: String,
    pub(super) state_url: String,
    pub(super) complete_url: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ManifestExecution {
    pub(super) execution_id: Uuid,
    pub(super) status: String,
    pub(super) attempt_number: i32,
    #[serde(default)]
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) maximum_input_tokens: Option<i64>,
    #[serde(default)]
    pub(super) maximum_output_tokens: Option<i64>,
    #[serde(default)]
    pub(super) maximum_model_calls: Option<i32>,
    #[serde(default)]
    pub(super) maximum_duration_seconds: Option<i32>,
    #[serde(default)]
    pub(super) maximum_cost_usd: Option<String>,
    #[serde(default)]
    pub(super) github_actions: Option<ManifestGithubActionsExecution>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ManifestGithubActionsExecution {
    pub(super) workflow_run_id: Option<i64>,
    pub(super) workflow_run_attempt: Option<i32>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct ManifestRun {
    pub(super) id: Uuid,
    pub(super) ticket_id: Uuid,
    pub(super) input_prompt: String,
    pub(super) attempt: i32,
    #[serde(default)]
    pub(super) metadata: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct HostedGithubManifest {
    pub(super) repository_id: i64,
    pub(super) repository: String,
    pub(super) clone_url: String,
    pub(super) web_base_url: String,
    pub(super) installation_id: i64,
    pub(super) base_ref: String,
    pub(super) base_sha: String,
    pub(super) branch: String,
    pub(super) github_token_url: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct HostedAiManifest {
    pub(super) responses_url: String,
    pub(super) model: String,
    pub(super) maximum_input_tokens: i64,
    pub(super) maximum_output_tokens: i64,
    pub(super) maximum_model_calls: i32,
    pub(super) maximum_cost_usd: String,
}

pub(super) fn deserialize_present_nullable<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum BudgetSource {
    UserSelected,
    ProjectDefault,
    SystemDefault,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BudgetAudit {
    pub(super) requested_model_call_budget: i32,
    pub(super) resolved_model_call_budget: i32,
    pub(super) worker_received_model_call_budget: i32,
    pub(super) budget_source: Option<BudgetSource>,
    pub(super) clamped: bool,
    pub(super) clamp_reason: Option<String>,
    pub(super) contract: &'static str,
}

#[derive(Debug)]
pub(super) struct ExecutionBudgetMismatch {
    pub(super) requested: Option<i32>,
    pub(super) resolved: Option<i32>,
    pub(super) canonical: Option<i32>,
    pub(super) execution: Option<i32>,
    pub(super) worker_received: i32,
}

impl std::fmt::Display for ExecutionBudgetMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "execution_budget_mismatch: requested={:?}, resolved={:?}, canonical={:?}, execution={:?}, worker_received={}",
            self.requested, self.resolved, self.canonical, self.execution, self.worker_received
        )
    }
}

impl std::error::Error for ExecutionBudgetMismatch {}

#[derive(Debug)]
pub(super) struct HostedProviderContractFailure {
    pub(super) code: String,
    pub(super) message: String,
}

impl HostedProviderContractFailure {
    pub(super) fn from_validation(error: anyhow::Error) -> Self {
        let message = error.to_string();
        let code = message
            .split_once(':')
            .map(|(code, _)| code)
            .filter(|code| {
                matches!(
                    *code,
                    "ai_provider_request_invalid"
                        | "ai_tool_schema_invalid"
                        | "ai_response_schema_invalid"
                )
            })
            .unwrap_or("ai_provider_request_invalid")
            .to_owned();
        Self { code, message }
    }
}

impl std::fmt::Display for HostedProviderContractFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HostedProviderContractFailure {}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct HostedExecutionPolicy {
    pub(super) policy_version: i32,
    pub(super) codex: HostedCodexPolicy,
    pub(super) quality_gates: Vec<HostedQualityGate>,
    pub(super) timeout_seconds: i64,
    pub(super) sandbox: HostedSandboxPolicy,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct ProjectVerificationPolicy {
    pub(super) browser_e2e_required_for_theme_changes: bool,
    pub(super) manual_browser_verification_required: bool,
}

impl Default for ProjectVerificationPolicy {
    fn default() -> Self {
        Self {
            browser_e2e_required_for_theme_changes: false,
            manual_browser_verification_required: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct HostedCodexPolicy {
    pub(super) command: Vec<String>,
    pub(super) environment_allowlist: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct HostedQualityGate {
    pub(super) id: String,
    pub(super) command: String,
    pub(super) timeout_seconds: i64,
    pub(super) required: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct HostedSandboxPolicy {
    pub(super) mode: String,
    pub(super) network_access: bool,
    pub(super) writable_roots: Vec<String>,
    pub(super) approval_policy: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CompletionRequest {
    pub(super) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) mission_outcome: Option<CompletionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) process_health: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) completion_evaluation: Option<CompletionEvaluation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) output_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) failure_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) failure_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) head_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pull_request_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pull_request_url: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct HostedResult {
    pub(super) summary: String,
    pub(super) branch: String,
    pub(super) commit: String,
    pub(super) pull_request: PullRequestResult,
    pub(super) validation: Vec<ValidationResult>,
    pub(super) completeness: CompletionEvaluation,
    pub(super) terminal_telemetry: TerminalTelemetry,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct TerminalTelemetry {
    pub(super) model_calls_used: usize,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) estimated_cost_micros: u64,
    pub(super) usage: ToolUsage,
    pub(super) changed_paths: Vec<String>,
    pub(super) last_successful_action: Value,
    pub(super) phase_reached: Option<ExecutionPhase>,
    pub(super) plan: Vec<PlannedChange>,
    pub(super) remaining_work: Vec<RemainingWorkItem>,
    pub(super) validation_evidence: Vec<ValidationEvidence>,
    pub(super) notebook_revision: u64,
}

#[derive(Clone, Debug)]
pub(super) struct PullRequestResult {
    pub(super) number: u64,
    pub(super) url: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ValidationResult {
    pub(super) id: String,
    pub(super) command: String,
    pub(super) status: String,
    pub(super) output: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CompletionStatus {
    Complete,
    CompletePendingExternalReview,
    Partial,
    Incomplete,
    Blocked,
    Uncertain,
}

impl CompletionStatus {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::CompletePendingExternalReview => "complete_pending_external_review",
            Self::Partial => "partial",
            Self::Incomplete => "incomplete",
            Self::Blocked => "blocked",
            Self::Uncertain => "uncertain",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ImplementationCompleteness {
    Complete,
    Partial,
    Incomplete,
}

impl ImplementationCompleteness {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Incomplete => "incomplete",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum VerificationReadiness {
    Verified,
    AutomatedVerified,
    PendingManualReview,
    Blocked,
}

impl VerificationReadiness {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::AutomatedVerified => "automated_verified",
            Self::PendingManualReview => "pending_manual_review",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EvaluationSource {
    Model,
    OrchestratorFallback,
    Hybrid,
}

impl EvaluationSource {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::OrchestratorFallback => "orchestrator_fallback",
            Self::Hybrid => "hybrid",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum VerificationType {
    Code,
    AutomatedTest,
    ManualQa,
    AccessibilityReview,
    VisualReview,
    ProductApproval,
    DeploymentEnvironment,
}

impl VerificationType {
    pub(super) const fn requires_external_review(self) -> bool {
        matches!(
            self,
            Self::ManualQa
                | Self::AccessibilityReview
                | Self::VisualReview
                | Self::ProductApproval
                | Self::DeploymentEnvironment
        )
    }

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::AutomatedTest => "automated_test",
            Self::ManualQa => "manual_qa",
            Self::AccessibilityReview => "accessibility_review",
            Self::VisualReview => "visual_review",
            Self::ProductApproval => "product_approval",
            Self::DeploymentEnvironment => "deployment_environment",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CriterionStatus {
    Satisfied,
    PartiallySatisfied,
    Unsatisfied,
    Uncertain,
    ExternalReviewRequired,
    NotApplicable,
}

impl CriterionStatus {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Satisfied => "satisfied",
            Self::PartiallySatisfied => "partially_satisfied",
            Self::Unsatisfied => "unsatisfied",
            Self::Uncertain => "uncertain",
            Self::ExternalReviewRequired => "external_review_required",
            Self::NotApplicable => "not_applicable",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct CompletionEvidence {
    pub(super) path: String,
    pub(super) description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct CriterionEvaluation {
    pub(super) criterion_id: String,
    pub(super) criterion: String,
    pub(super) verification_type: VerificationType,
    pub(super) status: CriterionStatus,
    #[serde(default)]
    pub(super) evidence: Vec<CompletionEvidence>,
    #[serde(default)]
    pub(super) validation_evidence: Vec<String>,
    #[serde(default)]
    pub(super) missing_evidence: Vec<String>,
    #[serde(default)]
    pub(super) required_next_action: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ReviewChecklistItem {
    pub(super) r#type: VerificationType,
    pub(super) description: String,
    pub(super) status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct CompletionEvaluation {
    pub(super) status: CompletionStatus,
    pub(super) implementation_completeness: ImplementationCompleteness,
    pub(super) verification_readiness: VerificationReadiness,
    pub(super) evaluation_source: EvaluationSource,
    pub(super) confidence: f64,
    #[serde(default)]
    pub(super) criteria: Vec<CriterionEvaluation>,
    #[serde(default)]
    pub(super) remaining_implementation_work: Vec<String>,
    #[serde(default)]
    pub(super) remaining_automated_verification: Vec<String>,
    #[serde(default)]
    pub(super) pending_external_review: Vec<String>,
    #[serde(default)]
    pub(super) optional_follow_up: Vec<String>,
    #[serde(default)]
    pub(super) review_checklist: Vec<ReviewChecklistItem>,
    #[serde(default)]
    pub(super) unrecovered_tool_failures: Vec<String>,
    pub(super) summary: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ImplementationPlan {
    pub(super) implementation_status: String,
    #[serde(default)]
    pub(super) planned_changes: Vec<PlannedChange>,
    #[serde(default)]
    pub(super) planned_new_files: Vec<String>,
    #[serde(default)]
    pub(super) planned_test_changes: Vec<String>,
    #[serde(default)]
    pub(super) remaining_unknowns: Vec<String>,
    #[serde(default)]
    pub(super) blocking_unknowns: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct PlannedChange {
    #[serde(default)]
    pub(super) change_id: String,
    #[serde(default)]
    pub(super) parent_change_id: Option<String>,
    #[serde(default, skip_serializing)]
    pub(super) path: String,
    #[serde(default, deserialize_with = "deserialize_planned_targets")]
    pub(super) targets: Vec<PlannedTarget>,
    #[serde(rename = "intent", alias = "change")]
    pub(super) change: String,
    pub(super) reason: String,
    #[serde(default)]
    pub(super) status: IntendedChangeStatus,
    #[serde(
        default,
        rename = "acceptance_criteria_ids",
        alias = "acceptance_criteria"
    )]
    pub(super) acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub(super) test_coverage: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct PlannedTarget {
    pub(super) path: String,
    #[serde(default)]
    pub(super) role: String,
    #[serde(default)]
    pub(super) new_file: bool,
    #[serde(default)]
    pub(super) status: IntendedChangeStatus,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum PlannedTargetInput {
    Path(String),
    Target(PlannedTarget),
}

pub(super) fn deserialize_planned_targets<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<PlannedTarget>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<PlannedTargetInput>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .map(|value| match value {
            PlannedTargetInput::Path(path) => PlannedTarget {
                path,
                role: String::new(),
                new_file: false,
                status: IntendedChangeStatus::Planned,
            },
            PlannedTargetInput::Target(target) => target,
        })
        .collect())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ImplementationDeclaration {
    pub(super) implementation_status: String,
    #[serde(default)]
    pub(super) completed_work: Vec<String>,
    #[serde(default)]
    pub(super) remaining_work: Vec<String>,
    #[serde(default)]
    pub(super) known_risks: Vec<String>,
    #[serde(default)]
    pub(super) changed_paths: Vec<String>,
    #[serde(default)]
    pub(super) criteria_evidence: Vec<ImplementationCriterionEvidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ImplementationCriterionEvidence {
    pub(super) criterion: String,
    #[serde(default)]
    pub(super) paths: Vec<String>,
    pub(super) evidence: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ToolFailureRecord {
    #[serde(default)]
    pub(super) attempt_index: usize,
    #[serde(default)]
    pub(super) change_id: Option<String>,
    pub(super) tool: String,
    pub(super) target: Option<String>,
    #[serde(default)]
    pub(super) error_code: String,
    #[serde(default)]
    pub(super) match_count: Option<usize>,
    pub(super) error: String,
    pub(super) recovered: bool,
    #[serde(default)]
    pub(super) reconciliation: FailureReconciliation,
    #[serde(default)]
    pub(super) recovery: Option<IntendedChangeRecovery>,
    #[serde(default)]
    pub(super) intended_change_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FailureReconciliation {
    Recovered,
    Superseded,
    #[default]
    StillUnresolved,
    Unrelated,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct IntendedChangeRecovery {
    pub(super) recovered: bool,
    pub(super) method: String,
    #[serde(default)]
    pub(super) evidence: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum IntendedChangeStatus {
    #[default]
    Planned,
    InProgress,
    Applied,
    Verified,
    Partial,
    Unresolved,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum WriteAttemptStatus {
    Applied,
    NoChange,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct WriteAttemptRecord {
    #[serde(default)]
    pub(super) attempt_index: usize,
    pub(super) change_id: String,
    pub(super) target: String,
    pub(super) tool: String,
    pub(super) status: WriteAttemptStatus,
    #[serde(default)]
    pub(super) error_code: Option<String>,
    #[serde(default)]
    pub(super) match_count: Option<usize>,
    #[serde(default)]
    pub(super) intended_change_sha256: Option<String>,
    #[serde(default)]
    pub(super) before_sha256: Option<String>,
    #[serde(default)]
    pub(super) after_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct MutationPreflightRecord {
    pub(super) change_id: String,
    pub(super) target: String,
    pub(super) failure_code: String,
    pub(super) plan_revision: u64,
    pub(super) retryable_with_same_plan: bool,
    pub(super) repair_strategy: String,
    pub(super) mutation_attempted: bool,
    pub(super) mutation_preflight_failed: bool,
    #[serde(default)]
    pub(super) deterministic_repair_attempted: bool,
    #[serde(default = "one_u32")]
    pub(super) occurrences: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct ImplementationPlanRepair {
    pub(super) change_id: String,
    pub(super) targets_before: Vec<String>,
    pub(super) targets_after: Vec<String>,
    pub(super) attempted_concrete_path: String,
    pub(super) validation_error: String,
    pub(super) repair_source: &'static str,
    pub(super) model_call_consumed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MutationPreflightDecision {
    pub(super) repeated: bool,
    pub(super) halt_orchestration: bool,
}

pub(super) const fn one_u32() -> u32 {
    1
}

#[derive(Debug)]
pub(super) struct MutationPreflightError {
    pub(super) code: &'static str,
    pub(super) change_id: String,
    pub(super) target: String,
    pub(super) message: String,
    pub(super) repair_strategy: &'static str,
}

impl std::fmt::Display for MutationPreflightError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for MutationPreflightError {}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct IntendedChangeRecord {
    pub(super) change_id: String,
    pub(super) intent: String,
    pub(super) status: IntendedChangeStatus,
    #[serde(default, skip_serializing)]
    pub(super) target: String,
    #[serde(default)]
    pub(super) targets: Vec<PlannedTarget>,
    #[serde(default)]
    pub(super) attempts: Vec<WriteAttemptRecord>,
    #[serde(default)]
    pub(super) recovery: Option<IntendedChangeRecovery>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ArtifactSemanticStatus {
    Partial,
    Sufficient,
    Invalid,
    #[default]
    Missing,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ArtifactSerializationStatus {
    Valid,
    Normalizable,
    #[default]
    Invalid,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ArtifactFailureLayer {
    ProviderToolArgumentGeneration,
    GatewayToolArgumentParsing,
    WorkerToolSchemaValidation,
    ArtifactSemanticValidation,
    ArtifactPersistence,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ArtifactPersistenceStatus {
    Persisted,
    Failed,
    #[default]
    PendingRetry,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ArtifactCheckpoint {
    pub(super) artifact: String,
    pub(super) semantic_status: ArtifactSemanticStatus,
    pub(super) serialization_status: ArtifactSerializationStatus,
    pub(super) persistence_status: ArtifactPersistenceStatus,
    #[serde(default)]
    pub(super) artifact_sha256: Option<String>,
    #[serde(default)]
    pub(super) model_call_index: Option<usize>,
    pub(super) phase: ExecutionPhase,
    #[serde(default)]
    pub(super) safe_error: Option<String>,
    #[serde(default)]
    pub(super) normalization_metadata: Option<Value>,
    #[serde(default)]
    pub(super) artifact_source: Option<ArtifactSource>,
    #[serde(default)]
    pub(super) confidence: Option<f64>,
    #[serde(default)]
    pub(super) failure_layer: Option<ArtifactFailureLayer>,
    #[serde(default)]
    pub(super) validation_errors: Vec<ValidationError>,
    #[serde(default)]
    pub(super) invalid_payload_shape: Option<InvalidPayloadShape>,
}

impl Default for ArtifactCheckpoint {
    fn default() -> Self {
        Self {
            artifact: "impact_map".into(),
            semantic_status: ArtifactSemanticStatus::Missing,
            serialization_status: ArtifactSerializationStatus::Invalid,
            persistence_status: ArtifactPersistenceStatus::PendingRetry,
            artifact_sha256: None,
            model_call_index: None,
            phase: ExecutionPhase::Discovery,
            safe_error: None,
            normalization_metadata: None,
            artifact_source: None,
            confidence: None,
            failure_layer: None,
            validation_errors: Vec::new(),
            invalid_payload_shape: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ImpactMapFailure {
    pub(super) code: &'static str,
    pub(super) safe_error: String,
    pub(super) errors: Vec<ValidationError>,
    pub(super) invalid_payload: Value,
    pub(super) invalid_payload_shape: InvalidPayloadShape,
    pub(super) failure_layer: ArtifactFailureLayer,
}

#[derive(Clone, Debug)]
pub(super) struct ImplementationOutcome {
    pub(super) summary: String,
    pub(super) budget_exhausted: bool,
    pub(super) explicit_declaration: Option<ImplementationDeclaration>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_request_serialization_contract_is_stable() {
        let request = CompletionRequest {
            status: "partial_result".into(),
            mission_outcome: Some(CompletionStatus::Partial),
            process_health: Some("healthy".into()),
            completion_evaluation: None,
            output_summary: Some("Continue from the persisted branch.".into()),
            failure_code: None,
            failure_message: None,
            head_branch: Some("rustgrid/aops-226-deadbeef".into()),
            head_sha: Some("a".repeat(40)),
            pull_request_number: Some(226),
            pull_request_url: Some("https://github.com/RustGrid/example/pull/226".into()),
        };

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "status": "partial_result",
                "mission_outcome": "partial",
                "process_health": "healthy",
                "output_summary": "Continue from the persisted branch.",
                "head_branch": "rustgrid/aops-226-deadbeef",
                "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "pull_request_number": 226,
                "pull_request_url": "https://github.com/RustGrid/example/pull/226"
            })
        );
    }

    #[test]
    fn implementation_plan_serialization_contract_is_stable() {
        let plan = ImplementationPlan {
            implementation_status: "ready".into(),
            planned_changes: vec![PlannedChange {
                change_id: "theme-provider".into(),
                parent_change_id: Some("theme-system".into()),
                path: "legacy-field-must-not-serialize.rs".into(),
                targets: vec![PlannedTarget {
                    path: "src/theme/provider.rs".into(),
                    role: "theme provider".into(),
                    new_file: false,
                    status: IntendedChangeStatus::Applied,
                }],
                change: "Add the requested theme".into(),
                reason: "Satisfy criterion AC-1".into(),
                status: IntendedChangeStatus::Applied,
                acceptance_criteria: vec!["AC-1".into()],
                test_coverage: vec!["tests/theme.rs".into()],
            }],
            planned_new_files: Vec::new(),
            planned_test_changes: vec!["tests/theme.rs".into()],
            remaining_unknowns: Vec::new(),
            blocking_unknowns: Vec::new(),
        };

        assert_eq!(
            serde_json::to_value(plan).unwrap(),
            json!({
                "implementation_status": "ready",
                "planned_changes": [{
                    "change_id": "theme-provider",
                    "parent_change_id": "theme-system",
                    "targets": [{
                        "path": "src/theme/provider.rs",
                        "role": "theme provider",
                        "new_file": false,
                        "status": "applied"
                    }],
                    "intent": "Add the requested theme",
                    "reason": "Satisfy criterion AC-1",
                    "status": "applied",
                    "acceptance_criteria_ids": ["AC-1"],
                    "test_coverage": ["tests/theme.rs"]
                }],
                "planned_new_files": [],
                "planned_test_changes": ["tests/theme.rs"],
                "remaining_unknowns": [],
                "blocking_unknowns": []
            })
        );
    }
}
