use super::*;

const TERMINAL_REVISION: u64 = 1;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CanonicalMissionOutcome {
    Complete,
    CompletePendingExternalReview,
    PartialReviewable,
    Blocked,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProcessHealth {
    Healthy,
    Degraded,
    Failed,
}

impl ProcessHealth {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DomainExecutionStatus {
    Completed,
    AwaitingExternalReview,
    NeedsContinuation,
    Blocked,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TerminalAuthority {
    WorkerDomain,
    InfrastructureFallback,
    AdministrativeOverride,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct TerminalFinality {
    pub(super) terminal_result_id: Uuid,
    pub(super) terminal_revision: u64,
    pub(super) authority: TerminalAuthority,
    pub(super) finalized_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum Resumability {
    NotResumable,
    AwaitingExternalReview,
    Resumable {
        #[serde(skip_serializing_if = "Option::is_none")]
        resume_phase: Option<String>,
    },
}

impl Resumability {
    pub(super) const fn is_resumable(&self) -> bool {
        matches!(self, Self::Resumable { .. })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RemainingEvidenceType {
    ExternalApproval,
    ManualInspection,
    MissingAutomatedValidation,
    UnresolvedImplementation,
    InfrastructureIncomplete,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct CanonicalPublicationResult {
    pub(super) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pull_request_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pull_request_url: Option<String>,
    pub(super) draft: bool,
    pub(super) mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) completed_at: Option<String>,
}

impl CanonicalPublicationResult {
    fn published(result: &HostedResult, completed_at: &str, draft: bool) -> Self {
        Self {
            status: "pull_request_created".into(),
            branch: Some(result.branch.clone()),
            commit_sha: Some(result.commit.clone()),
            pull_request_number: Some(result.pull_request.number),
            pull_request_url: Some(result.pull_request.url.clone()),
            draft,
            mode: "pull_request".into(),
            completed_at: Some(completed_at.into()),
        }
    }

    fn not_published() -> Self {
        Self {
            status: "not_published".into(),
            branch: None,
            commit_sha: None,
            pull_request_number: None,
            pull_request_url: None,
            draft: false,
            mode: "none".into(),
            completed_at: None,
        }
    }

    pub(super) fn is_published(&self) -> bool {
        self.status == "pull_request_created"
            && self.branch.is_some()
            && self.commit_sha.is_some()
            && self.pull_request_number.is_some()
            && self.pull_request_url.is_some()
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CanonicalTerminalResult {
    pub(super) terminal_result_id: Uuid,
    pub(super) mission_outcome: CanonicalMissionOutcome,
    pub(super) process_health: ProcessHealth,
    pub(super) reason_code: String,
    pub(super) execution_status: DomainExecutionStatus,
    pub(super) publication: CanonicalPublicationResult,
    pub(super) completion: CompletionEvaluation,
    pub(super) remaining_work: Vec<RemainingWorkItem>,
    pub(super) remaining_evidence_types: Vec<RemainingEvidenceType>,
    pub(super) resumability: Resumability,
    pub(super) completed_at: String,
    pub(super) finality: TerminalFinality,
}

impl CanonicalTerminalResult {
    pub(super) const fn process_exit_code(&self) -> i32 {
        match self.process_health {
            ProcessHealth::Healthy | ProcessHealth::Degraded => 0,
            ProcessHealth::Failed => 1,
        }
    }

    pub(super) const fn completion_request_status(&self) -> &'static str {
        match self.execution_status {
            DomainExecutionStatus::Completed => "completed",
            DomainExecutionStatus::AwaitingExternalReview => "awaiting_external_review",
            DomainExecutionStatus::NeedsContinuation => "partial_result",
            DomainExecutionStatus::Blocked => "blocked",
            DomainExecutionStatus::Cancelled => "cancelled",
            DomainExecutionStatus::Failed => "failed",
        }
    }

    pub(super) const fn compatibility_completion_status(&self) -> CompletionStatus {
        match self.mission_outcome {
            CanonicalMissionOutcome::Complete => CompletionStatus::Complete,
            CanonicalMissionOutcome::CompletePendingExternalReview => {
                CompletionStatus::CompletePendingExternalReview
            }
            CanonicalMissionOutcome::PartialReviewable => CompletionStatus::Partial,
            CanonicalMissionOutcome::Blocked => CompletionStatus::Blocked,
            CanonicalMissionOutcome::Cancelled | CanonicalMissionOutcome::Failed => {
                CompletionStatus::Incomplete
            }
        }
    }

    pub(super) fn ui_status(&self) -> &'static str {
        match self.mission_outcome {
            CanonicalMissionOutcome::Complete => "completed",
            CanonicalMissionOutcome::CompletePendingExternalReview => "awaiting_review",
            CanonicalMissionOutcome::PartialReviewable if self.publication.is_published() => {
                "draft_pr_ready"
            }
            CanonicalMissionOutcome::PartialReviewable => "needs_continuation",
            CanonicalMissionOutcome::Blocked => "blocked",
            CanonicalMissionOutcome::Cancelled => "cancelled",
            CanonicalMissionOutcome::Failed => "failed",
        }
    }
}

fn terminal_result_id(execution_id: Uuid) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("rustgrid-terminal:{execution_id}:{TERMINAL_REVISION}").as_bytes(),
    )
}

pub(super) fn canonical_terminal_result_id(execution_id: Uuid) -> Uuid {
    terminal_result_id(execution_id)
}

fn remaining_evidence_types(completion: &CompletionEvaluation) -> Vec<RemainingEvidenceType> {
    let mut types = Vec::new();
    let mut push = |value| {
        if !types.contains(&value) {
            types.push(value);
        }
    };
    if !completion.pending_external_review.is_empty()
        || completion
            .criteria
            .iter()
            .any(|criterion| criterion.status == CriterionStatus::ExternalReviewRequired)
    {
        push(RemainingEvidenceType::ExternalApproval);
    }
    if completion.review_checklist.iter().any(|item| {
        item.status != "completed"
            && matches!(
                item.r#type,
                VerificationType::ManualQa
                    | VerificationType::AccessibilityReview
                    | VerificationType::VisualReview
                    | VerificationType::DeploymentEnvironment
            )
    }) {
        push(RemainingEvidenceType::ManualInspection);
    }
    if !completion.remaining_automated_verification.is_empty() {
        push(RemainingEvidenceType::MissingAutomatedValidation);
    }
    if completion.implementation_completeness != ImplementationCompleteness::Complete
        || !completion.remaining_implementation_work.is_empty()
    {
        push(RemainingEvidenceType::UnresolvedImplementation);
    }
    if !completion.unrecovered_tool_failures.is_empty() {
        push(RemainingEvidenceType::InfrastructureIncomplete);
    }
    types
}

fn normalized_completion(
    mut completion: CompletionEvaluation,
    outcome: CanonicalMissionOutcome,
) -> CompletionEvaluation {
    completion.status = match outcome {
        CanonicalMissionOutcome::Complete => CompletionStatus::Complete,
        CanonicalMissionOutcome::CompletePendingExternalReview => {
            CompletionStatus::CompletePendingExternalReview
        }
        CanonicalMissionOutcome::PartialReviewable => CompletionStatus::Partial,
        CanonicalMissionOutcome::Blocked => CompletionStatus::Blocked,
        CanonicalMissionOutcome::Cancelled | CanonicalMissionOutcome::Failed => {
            CompletionStatus::Incomplete
        }
    };
    completion
}

pub(super) fn resolve_published_terminal_result(
    execution_id: Uuid,
    result: &HostedResult,
    completed_at: &str,
) -> CanonicalTerminalResult {
    let remaining_evidence_types = remaining_evidence_types(&result.completeness);
    let automated_gates_passed = result
        .validation
        .iter()
        .all(|validation| validation.status == "passed");
    let engineering_work_remains = remaining_evidence_types.iter().any(|kind| {
        matches!(
            kind,
            RemainingEvidenceType::MissingAutomatedValidation
                | RemainingEvidenceType::UnresolvedImplementation
                | RemainingEvidenceType::InfrastructureIncomplete
        )
    }) || !automated_gates_passed;
    let external_only = !remaining_evidence_types.is_empty()
        && remaining_evidence_types.iter().all(|kind| {
            matches!(
                kind,
                RemainingEvidenceType::ExternalApproval | RemainingEvidenceType::ManualInspection
            )
        });
    let mission_outcome = if !engineering_work_remains
        && (external_only
            || result.completeness.status == CompletionStatus::CompletePendingExternalReview)
    {
        CanonicalMissionOutcome::CompletePendingExternalReview
    } else if !engineering_work_remains
        && result.completeness.implementation_completeness == ImplementationCompleteness::Complete
        && result.completeness.status == CompletionStatus::Complete
    {
        CanonicalMissionOutcome::Complete
    } else {
        // A HostedResult contains a successfully created or recovered pull
        // request. Strong publication evidence therefore resolves ambiguous,
        // incomplete, blocked, and uncertain evaluations as reviewable partial
        // work instead of a process failure.
        CanonicalMissionOutcome::PartialReviewable
    };
    let draft = mission_outcome != CanonicalMissionOutcome::Complete;
    let publication = CanonicalPublicationResult::published(result, completed_at, draft);
    let (reason_code, execution_status, resumability) = match mission_outcome {
        CanonicalMissionOutcome::Complete => (
            "completed",
            DomainExecutionStatus::Completed,
            Resumability::NotResumable,
        ),
        CanonicalMissionOutcome::CompletePendingExternalReview => (
            "external_review_required",
            DomainExecutionStatus::AwaitingExternalReview,
            Resumability::AwaitingExternalReview,
        ),
        CanonicalMissionOutcome::PartialReviewable => (
            "partial_reviewable",
            DomainExecutionStatus::NeedsContinuation,
            Resumability::Resumable {
                resume_phase: Some("implementation".into()),
            },
        ),
        _ => unreachable!("published terminal resolution has three legal outcomes"),
    };
    let terminal_result_id = terminal_result_id(execution_id);
    CanonicalTerminalResult {
        terminal_result_id,
        mission_outcome,
        process_health: ProcessHealth::Healthy,
        reason_code: reason_code.into(),
        execution_status,
        publication,
        completion: normalized_completion(result.completeness.clone(), mission_outcome),
        remaining_work: result.terminal_telemetry.remaining_work.clone(),
        remaining_evidence_types,
        resumability,
        completed_at: completed_at.into(),
        finality: TerminalFinality {
            terminal_result_id,
            terminal_revision: TERMINAL_REVISION,
            authority: TerminalAuthority::WorkerDomain,
            finalized_at: completed_at.into(),
        },
    }
}

pub(super) fn resolve_blocked_terminal_result(
    execution_id: Uuid,
    reason_code: &str,
    remaining_work: Vec<RemainingWorkItem>,
    resume_phase: &str,
    completion: CompletionEvaluation,
    completed_at: &str,
) -> CanonicalTerminalResult {
    let terminal_result_id = terminal_result_id(execution_id);
    CanonicalTerminalResult {
        terminal_result_id,
        mission_outcome: CanonicalMissionOutcome::Blocked,
        process_health: ProcessHealth::Healthy,
        reason_code: reason_code.into(),
        execution_status: DomainExecutionStatus::Blocked,
        publication: CanonicalPublicationResult::not_published(),
        completion: normalized_completion(completion, CanonicalMissionOutcome::Blocked),
        remaining_work,
        remaining_evidence_types: vec![RemainingEvidenceType::UnresolvedImplementation],
        resumability: Resumability::Resumable {
            resume_phase: Some(resume_phase.into()),
        },
        completed_at: completed_at.into(),
        finality: TerminalFinality {
            terminal_result_id,
            terminal_revision: TERMINAL_REVISION,
            authority: TerminalAuthority::WorkerDomain,
            finalized_at: completed_at.into(),
        },
    }
}

pub(super) fn resolve_unsuccessful_terminal_result(
    execution_id: Uuid,
    cancelled: bool,
    reason_code: &str,
    safe_summary: &str,
    completed_at: &str,
) -> CanonicalTerminalResult {
    let terminal_result_id = terminal_result_id(execution_id);
    let mission_outcome = if cancelled {
        CanonicalMissionOutcome::Cancelled
    } else {
        CanonicalMissionOutcome::Failed
    };
    let process_health = if cancelled {
        ProcessHealth::Healthy
    } else {
        ProcessHealth::Failed
    };
    let execution_status = if cancelled {
        DomainExecutionStatus::Cancelled
    } else {
        DomainExecutionStatus::Failed
    };
    let resumability = if cancelled {
        Resumability::Resumable {
            resume_phase: Some("implementation".into()),
        }
    } else {
        Resumability::NotResumable
    };
    CanonicalTerminalResult {
        terminal_result_id,
        mission_outcome,
        process_health,
        reason_code: reason_code.into(),
        execution_status,
        publication: CanonicalPublicationResult::not_published(),
        completion: CompletionEvaluation {
            status: CompletionStatus::Incomplete,
            implementation_completeness: ImplementationCompleteness::Incomplete,
            verification_readiness: VerificationReadiness::Blocked,
            evaluation_source: EvaluationSource::OrchestratorFallback,
            confidence: 1.0,
            criteria: Vec::new(),
            remaining_implementation_work: Vec::new(),
            remaining_automated_verification: Vec::new(),
            pending_external_review: Vec::new(),
            optional_follow_up: Vec::new(),
            review_checklist: Vec::new(),
            unrecovered_tool_failures: (!cancelled)
                .then(|| safe_summary.into())
                .into_iter()
                .collect(),
            summary: safe_summary.into(),
        },
        remaining_work: Vec::new(),
        remaining_evidence_types: vec![if cancelled {
            RemainingEvidenceType::UnresolvedImplementation
        } else {
            RemainingEvidenceType::InfrastructureIncomplete
        }],
        resumability,
        completed_at: completed_at.into(),
        finality: TerminalFinality {
            terminal_result_id,
            terminal_revision: TERMINAL_REVISION,
            authority: TerminalAuthority::WorkerDomain,
            finalized_at: completed_at.into(),
        },
    }
}

pub(super) fn resolve_infrastructure_fallback_terminal_result(
    execution_id: Uuid,
    reason_code: &str,
    safe_summary: &str,
    completed_at: &str,
) -> CanonicalTerminalResult {
    let mut terminal = resolve_unsuccessful_terminal_result(
        execution_id,
        false,
        reason_code,
        safe_summary,
        completed_at,
    );
    terminal.finality.authority = TerminalAuthority::InfrastructureFallback;
    terminal
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct InfrastructureTerminalMetadata {
    pub(super) provider: String,
    pub(super) workflow_run_id: Option<String>,
    pub(super) workflow_job_id: Option<String>,
    pub(super) workflow_status: String,
    pub(super) workflow_conclusion: Option<String>,
    pub(super) runner_name: Option<String>,
    pub(super) observed_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum InfrastructureReconciliationDecision {
    DomainResultPreserved,
    DomainFailureConfirmed,
    InfrastructureFallbackRequired,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct InfrastructureReconciliationResult {
    pub(super) decision: InfrastructureReconciliationDecision,
    pub(super) infrastructure: InfrastructureTerminalMetadata,
    pub(super) anomaly_code: Option<&'static str>,
    pub(super) terminal_result_id: Option<Uuid>,
}

pub(super) fn reconcile_infrastructure_terminal(
    canonical: Option<&CanonicalTerminalResult>,
    infrastructure: InfrastructureTerminalMetadata,
) -> InfrastructureReconciliationResult {
    let workflow_failed = infrastructure.workflow_conclusion.as_deref() == Some("failure");
    match canonical {
        Some(canonical) if workflow_failed && canonical.process_health != ProcessHealth::Failed => {
            debug_assert_eq!(
                reconcile_terminal_replacement(
                    canonical,
                    TerminalAuthority::InfrastructureFallback
                ),
                TerminalReplacementDecision::RejectedFinalWorkerDomain
            );
            InfrastructureReconciliationResult {
                decision: InfrastructureReconciliationDecision::DomainResultPreserved,
                infrastructure,
                anomaly_code: Some("workflow_conclusion_conflicts_with_domain_result"),
                terminal_result_id: Some(canonical.terminal_result_id),
            }
        }
        Some(canonical) if workflow_failed => InfrastructureReconciliationResult {
            decision: InfrastructureReconciliationDecision::DomainFailureConfirmed,
            infrastructure,
            anomaly_code: None,
            terminal_result_id: Some(canonical.terminal_result_id),
        },
        Some(canonical) => InfrastructureReconciliationResult {
            decision: InfrastructureReconciliationDecision::DomainResultPreserved,
            infrastructure,
            anomaly_code: None,
            terminal_result_id: Some(canonical.terminal_result_id),
        },
        None => InfrastructureReconciliationResult {
            decision: InfrastructureReconciliationDecision::InfrastructureFallbackRequired,
            infrastructure,
            anomaly_code: None,
            terminal_result_id: None,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalReplacementDecision {
    RejectedFinalWorkerDomain,
    AdministrativeOverrideAccepted,
}

pub(super) fn reconcile_terminal_replacement(
    existing: &CanonicalTerminalResult,
    replacement_authority: TerminalAuthority,
) -> TerminalReplacementDecision {
    if existing.finality.authority == TerminalAuthority::WorkerDomain
        && replacement_authority != TerminalAuthority::AdministrativeOverride
    {
        TerminalReplacementDecision::RejectedFinalWorkerDomain
    } else {
        TerminalReplacementDecision::AdministrativeOverrideAccepted
    }
}

pub(super) fn record_noncritical_post_publication_failure(
    canonical: &mut CanonicalTerminalResult,
    _failure_layer: &str,
) {
    if canonical.publication.is_published() && canonical.process_health == ProcessHealth::Healthy {
        canonical.process_health = ProcessHealth::Degraded;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(status: CompletionStatus) -> CompletionEvaluation {
        let implementation_completeness = match status {
            CompletionStatus::Complete | CompletionStatus::CompletePendingExternalReview => {
                ImplementationCompleteness::Complete
            }
            CompletionStatus::Partial => ImplementationCompleteness::Partial,
            CompletionStatus::Incomplete
            | CompletionStatus::Blocked
            | CompletionStatus::Uncertain => ImplementationCompleteness::Incomplete,
        };
        CompletionEvaluation {
            status,
            implementation_completeness,
            verification_readiness: VerificationReadiness::Blocked,
            evaluation_source: EvaluationSource::OrchestratorFallback,
            confidence: 1.0,
            criteria: Vec::new(),
            remaining_implementation_work: Vec::new(),
            remaining_automated_verification: Vec::new(),
            pending_external_review: Vec::new(),
            optional_follow_up: Vec::new(),
            review_checklist: Vec::new(),
            unrecovered_tool_failures: Vec::new(),
            summary: "Deterministic terminal fixture".into(),
        }
    }

    fn published(completion: CompletionEvaluation) -> HostedResult {
        HostedResult {
            summary: "Review the published changes.".into(),
            branch: "rustgrid/generic-terminal-fixture".into(),
            commit: "a".repeat(40),
            pull_request: PullRequestResult {
                number: 41,
                url: "https://github.example/repository/pull/41".into(),
            },
            validation: vec![ValidationResult {
                id: "required".into(),
                command: "project-check".into(),
                status: "passed".into(),
                output: String::new(),
            }],
            completeness: completion,
            terminal_telemetry: TerminalTelemetry::default(),
        }
    }

    fn infrastructure(conclusion: &str) -> InfrastructureTerminalMetadata {
        InfrastructureTerminalMetadata {
            provider: "github_actions".into(),
            workflow_run_id: Some("100".into()),
            workflow_job_id: Some("200".into()),
            workflow_status: "completed".into(),
            workflow_conclusion: Some(conclusion.into()),
            runner_name: Some("runner".into()),
            observed_at: "2026-08-04T00:00:00Z".into(),
        }
    }

    #[test]
    fn healthy_non_complete_domain_results_exit_zero() {
        let partial = resolve_published_terminal_result(
            Uuid::nil(),
            &published(completion(CompletionStatus::Partial)),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            partial.mission_outcome,
            CanonicalMissionOutcome::PartialReviewable
        );
        assert_eq!(partial.process_health, ProcessHealth::Healthy);
        assert_eq!(partial.process_exit_code(), 0);
        assert_eq!(partial.ui_status(), "draft_pr_ready");

        let mut external = completion(CompletionStatus::Uncertain);
        external.implementation_completeness = ImplementationCompleteness::Complete;
        external.pending_external_review = vec!["A reviewer approves the result.".into()];
        let pending = resolve_published_terminal_result(
            Uuid::nil(),
            &published(external),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            pending.mission_outcome,
            CanonicalMissionOutcome::CompletePendingExternalReview
        );
        assert_eq!(pending.process_exit_code(), 0);
        assert_eq!(
            pending.completion.status,
            CompletionStatus::CompletePendingExternalReview
        );

        let blocked = resolve_blocked_terminal_result(
            Uuid::nil(),
            "safe_blocker_recorded",
            vec![RemainingWorkItem {
                change_id: "change".into(),
                path: "src/module.ext".into(),
                role: "source".into(),
                status: IntendedChangeStatus::Unresolved,
                reason: "Safe blocker".into(),
            }],
            "implementation",
            completion(CompletionStatus::Blocked),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(blocked.mission_outcome, CanonicalMissionOutcome::Blocked);
        assert!(blocked.resumability.is_resumable());
        assert_eq!(blocked.process_exit_code(), 0);
    }

    #[test]
    fn genuine_process_failure_is_the_only_nonzero_terminal_health() {
        let failed = resolve_unsuccessful_terminal_result(
            Uuid::nil(),
            false,
            "runner_failed",
            "The runner could not execute reliably.",
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(failed.mission_outcome, CanonicalMissionOutcome::Failed);
        assert_eq!(failed.process_health, ProcessHealth::Failed);
        assert_eq!(failed.process_exit_code(), 1);

        let cancelled = resolve_unsuccessful_terminal_result(
            Uuid::nil(),
            true,
            "cancelled",
            "Cancellation was persisted.",
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            cancelled.mission_outcome,
            CanonicalMissionOutcome::Cancelled
        );
        assert_eq!(cancelled.process_health, ProcessHealth::Healthy);
        assert_eq!(cancelled.process_exit_code(), 0);
    }

    #[test]
    fn deterministic_evidence_replaces_uncertain_with_stronger_outcomes() {
        let mut external = completion(CompletionStatus::Uncertain);
        external.implementation_completeness = ImplementationCompleteness::Complete;
        external.pending_external_review = vec!["Manual approval".into()];
        let external = resolve_published_terminal_result(
            Uuid::nil(),
            &published(external),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            external.remaining_evidence_types,
            vec![RemainingEvidenceType::ExternalApproval]
        );
        assert_eq!(
            external.mission_outcome,
            CanonicalMissionOutcome::CompletePendingExternalReview
        );

        let mut automated = completion(CompletionStatus::Uncertain);
        automated.implementation_completeness = ImplementationCompleteness::Complete;
        automated.remaining_automated_verification = vec!["Run required check".into()];
        let automated = resolve_published_terminal_result(
            Uuid::nil(),
            &published(automated),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            automated.mission_outcome,
            CanonicalMissionOutcome::PartialReviewable
        );
        assert_eq!(automated.completion.status, CompletionStatus::Partial);
        assert_ne!(automated.completion.status, CompletionStatus::Uncertain);
    }

    #[test]
    fn workflow_failure_annotates_but_does_not_replace_worker_domain_result() {
        let canonical = resolve_published_terminal_result(
            Uuid::nil(),
            &published(completion(CompletionStatus::Partial)),
            "2026-08-04T00:00:00Z",
        );
        let publication = canonical.publication.clone();
        let reconciled =
            reconcile_infrastructure_terminal(Some(&canonical), infrastructure("failure"));
        assert_eq!(
            reconciled.decision,
            InfrastructureReconciliationDecision::DomainResultPreserved
        );
        assert_eq!(
            reconciled.anomaly_code,
            Some("workflow_conclusion_conflicts_with_domain_result")
        );
        assert_eq!(
            reconciled.infrastructure.workflow_run_id.as_deref(),
            Some("100")
        );
        assert_eq!(canonical.publication, publication);
        assert_eq!(canonical.ui_status(), "draft_pr_ready");
    }

    #[test]
    fn missing_worker_result_allows_infrastructure_fallback_failure() {
        let reconciled = reconcile_infrastructure_terminal(None, infrastructure("failure"));
        assert_eq!(
            reconciled.decision,
            InfrastructureReconciliationDecision::InfrastructureFallbackRequired
        );
        assert_eq!(reconciled.terminal_result_id, None);
        let fallback = resolve_infrastructure_fallback_terminal_result(
            Uuid::nil(),
            "workflow_failed",
            "No worker-domain result exists.",
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            fallback.finality.authority,
            TerminalAuthority::InfrastructureFallback
        );
        assert_eq!(fallback.process_exit_code(), 1);
    }

    #[test]
    fn finalized_worker_result_requires_explicit_administrative_override() {
        let canonical = resolve_published_terminal_result(
            Uuid::nil(),
            &published(completion(CompletionStatus::Complete)),
            "2026-08-04T00:00:00Z",
        );
        assert_eq!(
            reconcile_terminal_replacement(&canonical, TerminalAuthority::InfrastructureFallback),
            TerminalReplacementDecision::RejectedFinalWorkerDomain
        );
        assert_eq!(
            reconcile_terminal_replacement(&canonical, TerminalAuthority::AdministrativeOverride),
            TerminalReplacementDecision::AdministrativeOverrideAccepted
        );
    }

    #[test]
    fn noncritical_post_publication_failure_degrades_health_only() {
        let mut canonical = resolve_published_terminal_result(
            Uuid::nil(),
            &published(completion(CompletionStatus::Partial)),
            "2026-08-04T00:00:00Z",
        );
        let outcome = canonical.mission_outcome;
        let publication = canonical.publication.clone();
        record_noncritical_post_publication_failure(&mut canonical, "cleanup_failed");
        assert_eq!(canonical.process_health, ProcessHealth::Degraded);
        assert_eq!(canonical.mission_outcome, outcome);
        assert_eq!(canonical.publication, publication);
        assert_eq!(canonical.process_exit_code(), 0);
    }
}
