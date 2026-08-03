use std::{error::Error, fmt};

use crate::{command::CommandFailure, git::RemoteBranchMoved, run_error::RunFailure};

pub type ExecutionResult<T> = std::result::Result<T, ExecutionFailure>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalOutcome {
    Cancelled,
    LeaseLost,
    TimedOut,
    Blocked,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryErrorCode {
    ExecutionCancelled,
    WorkerShutdown,
    LeaseLost,
    AuthenticationRejected,
    AuthorizationRejected,
    InvalidSignedManifest,
    InvalidExecutionPolicy,
    ControlPlaneUnavailable,
    ControlPlaneRejected,
    ProviderProtocolFailure,
    ProviderBudgetExhausted,
    InvalidModelArtifact,
    RepositoryConflict,
    RemoteBranchMoved,
    ValidationFailed,
    PublicationFailed,
    RecoveryFailed,
    LocalInfrastructureFailed,
    ExecutionTimedOut,
    HumanInterventionRequired,
    InternalInvariantFailed,
}

impl TelemetryErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExecutionCancelled => "execution_cancelled",
            Self::WorkerShutdown => "worker_shutdown",
            Self::LeaseLost => "lease_lost",
            Self::AuthenticationRejected => "authentication_rejected",
            Self::AuthorizationRejected => "authorization_rejected",
            Self::InvalidSignedManifest => "invalid_signed_manifest",
            Self::InvalidExecutionPolicy => "invalid_execution_policy",
            Self::ControlPlaneUnavailable => "control_plane_unavailable",
            Self::ControlPlaneRejected => "control_plane_rejected",
            Self::ProviderProtocolFailure => "provider_protocol_failure",
            Self::ProviderBudgetExhausted => "provider_budget_exhausted",
            Self::InvalidModelArtifact => "invalid_model_artifact",
            Self::RepositoryConflict => "repository_conflict",
            Self::RemoteBranchMoved => "remote_branch_moved",
            Self::ValidationFailed => "validation_failed",
            Self::PublicationFailed => "publication_failed",
            Self::RecoveryFailed => "recovery_failed",
            Self::LocalInfrastructureFailed => "local_infrastructure_failed",
            Self::ExecutionTimedOut => "execution_timed_out",
            Self::HumanInterventionRequired => "human_intervention_required",
            Self::InternalInvariantFailed => "internal_invariant_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancellationError {
    Requested,
    Command,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessError {
    AuthenticationRejected,
    AuthorizationRejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManifestError {
    InvalidSignature,
    InvalidPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlPlaneError {
    Retryable {
        operation: String,
        status: Option<u16>,
        request_id: Option<String>,
    },
    Rejected {
        operation: String,
        status: Option<u16>,
        request_id: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderError {
    Protocol,
    BudgetExhausted,
    InvalidArtifact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryError {
    Conflict { path: Option<String> },
    RemoteBranchMoved { branch: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationError {
    pub gate: Option<String>,
    pub repairable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationError {
    pub stage: String,
    pub retryable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryError {
    JournalRead,
    JournalWrite,
    InvalidCheckpoint,
    Publication,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InfrastructureError {
    pub component: String,
    pub retryable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DiagnosticSource {
    message: String,
}

impl fmt::Display for DiagnosticSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DiagnosticSource {}

pub struct ExecutionFailure {
    kind: ExecutionFailureKind,
    context: String,
    source: Option<DiagnosticSource>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionFailureKind {
    Cancellation(CancellationError),
    Shutdown,
    LeaseLost { operation: String },
    Access(AccessError),
    Manifest(ManifestError),
    ControlPlane(ControlPlaneError),
    Provider(ProviderError),
    Repository(RepositoryError),
    Validation(ValidationError),
    Publication(PublicationError),
    Recovery(RecoveryError),
    Infrastructure(InfrastructureError),
    TimedOut { seconds: Option<u64> },
    HumanBlocked,
    Invariant,
}

impl ExecutionFailure {
    pub fn new(kind: ExecutionFailureKind, context: impl Into<String>) -> Self {
        Self {
            kind,
            context: redact_diagnostic(context.into()),
            source: None,
        }
    }

    pub fn with_safe_source(
        kind: ExecutionFailureKind,
        context: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            context: redact_diagnostic(context.into()),
            source: Some(DiagnosticSource {
                message: redact_diagnostic(source.into()),
            }),
        }
    }

    pub const fn kind(&self) -> &ExecutionFailureKind {
        &self.kind
    }

    pub const fn retryable(&self) -> bool {
        match &self.kind {
            ExecutionFailureKind::Cancellation(_)
            | ExecutionFailureKind::Shutdown
            | ExecutionFailureKind::LeaseLost { .. }
            | ExecutionFailureKind::Access(_)
            | ExecutionFailureKind::Manifest(_)
            | ExecutionFailureKind::Provider(ProviderError::Protocol)
            | ExecutionFailureKind::Provider(ProviderError::BudgetExhausted)
            | ExecutionFailureKind::Provider(ProviderError::InvalidArtifact)
            | ExecutionFailureKind::Repository(RepositoryError::Conflict { .. })
            | ExecutionFailureKind::Repository(RepositoryError::RemoteBranchMoved { .. })
            | ExecutionFailureKind::Recovery(RecoveryError::InvalidCheckpoint)
            | ExecutionFailureKind::HumanBlocked
            | ExecutionFailureKind::Invariant => false,
            ExecutionFailureKind::ControlPlane(ControlPlaneError::Retryable { .. })
            | ExecutionFailureKind::Recovery(RecoveryError::JournalRead)
            | ExecutionFailureKind::Recovery(RecoveryError::JournalWrite)
            | ExecutionFailureKind::Recovery(RecoveryError::Publication)
            | ExecutionFailureKind::TimedOut { .. } => true,
            ExecutionFailureKind::ControlPlane(ControlPlaneError::Rejected { .. }) => false,
            ExecutionFailureKind::Validation(error) => error.repairable,
            ExecutionFailureKind::Publication(error) => error.retryable,
            ExecutionFailureKind::Infrastructure(error) => error.retryable,
        }
    }

    pub const fn terminal_outcome(&self) -> TerminalOutcome {
        match &self.kind {
            ExecutionFailureKind::Cancellation(_) | ExecutionFailureKind::Shutdown => {
                TerminalOutcome::Cancelled
            }
            ExecutionFailureKind::LeaseLost { .. } => TerminalOutcome::LeaseLost,
            ExecutionFailureKind::TimedOut { .. } => TerminalOutcome::TimedOut,
            ExecutionFailureKind::Access(_)
            | ExecutionFailureKind::Manifest(_)
            | ExecutionFailureKind::ControlPlane(ControlPlaneError::Rejected { .. })
            | ExecutionFailureKind::Provider(ProviderError::Protocol)
            | ExecutionFailureKind::Provider(ProviderError::BudgetExhausted)
            | ExecutionFailureKind::Provider(ProviderError::InvalidArtifact)
            | ExecutionFailureKind::Repository(_)
            | ExecutionFailureKind::Validation(_)
            | ExecutionFailureKind::Publication(PublicationError {
                retryable: false, ..
            })
            | ExecutionFailureKind::Recovery(RecoveryError::InvalidCheckpoint)
            | ExecutionFailureKind::Infrastructure(InfrastructureError {
                retryable: false, ..
            })
            | ExecutionFailureKind::HumanBlocked => TerminalOutcome::Blocked,
            ExecutionFailureKind::ControlPlane(ControlPlaneError::Retryable { .. })
            | ExecutionFailureKind::Publication(PublicationError {
                retryable: true, ..
            })
            | ExecutionFailureKind::Recovery(RecoveryError::JournalRead)
            | ExecutionFailureKind::Recovery(RecoveryError::JournalWrite)
            | ExecutionFailureKind::Recovery(RecoveryError::Publication)
            | ExecutionFailureKind::Infrastructure(InfrastructureError {
                retryable: true, ..
            })
            | ExecutionFailureKind::Invariant => TerminalOutcome::Failed,
        }
    }

    pub const fn telemetry_code(&self) -> TelemetryErrorCode {
        match &self.kind {
            ExecutionFailureKind::Cancellation(_) => TelemetryErrorCode::ExecutionCancelled,
            ExecutionFailureKind::Shutdown => TelemetryErrorCode::WorkerShutdown,
            ExecutionFailureKind::LeaseLost { .. } => TelemetryErrorCode::LeaseLost,
            ExecutionFailureKind::Access(AccessError::AuthenticationRejected) => {
                TelemetryErrorCode::AuthenticationRejected
            }
            ExecutionFailureKind::Access(AccessError::AuthorizationRejected) => {
                TelemetryErrorCode::AuthorizationRejected
            }
            ExecutionFailureKind::Manifest(ManifestError::InvalidSignature) => {
                TelemetryErrorCode::InvalidSignedManifest
            }
            ExecutionFailureKind::Manifest(ManifestError::InvalidPolicy) => {
                TelemetryErrorCode::InvalidExecutionPolicy
            }
            ExecutionFailureKind::ControlPlane(ControlPlaneError::Retryable { .. }) => {
                TelemetryErrorCode::ControlPlaneUnavailable
            }
            ExecutionFailureKind::ControlPlane(ControlPlaneError::Rejected { .. }) => {
                TelemetryErrorCode::ControlPlaneRejected
            }
            ExecutionFailureKind::Provider(ProviderError::Protocol) => {
                TelemetryErrorCode::ProviderProtocolFailure
            }
            ExecutionFailureKind::Provider(ProviderError::BudgetExhausted) => {
                TelemetryErrorCode::ProviderBudgetExhausted
            }
            ExecutionFailureKind::Provider(ProviderError::InvalidArtifact) => {
                TelemetryErrorCode::InvalidModelArtifact
            }
            ExecutionFailureKind::Repository(RepositoryError::Conflict { .. }) => {
                TelemetryErrorCode::RepositoryConflict
            }
            ExecutionFailureKind::Repository(RepositoryError::RemoteBranchMoved { .. }) => {
                TelemetryErrorCode::RemoteBranchMoved
            }
            ExecutionFailureKind::Validation(_) => TelemetryErrorCode::ValidationFailed,
            ExecutionFailureKind::Publication(_) => TelemetryErrorCode::PublicationFailed,
            ExecutionFailureKind::Recovery(_) => TelemetryErrorCode::RecoveryFailed,
            ExecutionFailureKind::Infrastructure(_) => {
                TelemetryErrorCode::LocalInfrastructureFailed
            }
            ExecutionFailureKind::TimedOut { .. } => TelemetryErrorCode::ExecutionTimedOut,
            ExecutionFailureKind::HumanBlocked => TelemetryErrorCode::HumanInterventionRequired,
            ExecutionFailureKind::Invariant => TelemetryErrorCode::InternalInvariantFailed,
        }
    }

    pub const fn may_publish_terminal_state(&self) -> bool {
        !matches!(self.kind, ExecutionFailureKind::LeaseLost { .. })
    }

    pub fn from_anyhow(error: anyhow::Error) -> Self {
        if crate::api::is_lease_lost(&error) {
            return Self::with_safe_source(
                ExecutionFailureKind::LeaseLost {
                    operation: "control-plane lease operation".into(),
                },
                error.to_string(),
                "the control plane rejected the current lease owner",
            );
        }
        if let Some(access) = crate::api::typed_access_error(&error) {
            return Self::with_safe_source(
                ExecutionFailureKind::Access(access),
                error.to_string(),
                "the control plane rejected the worker credential or permission",
            );
        }
        if let Some(failure) = error.downcast_ref::<CommandFailure>() {
            let kind = match failure {
                CommandFailure::Cancelled => {
                    ExecutionFailureKind::Cancellation(CancellationError::Command)
                }
                CommandFailure::TimedOut { seconds } | CommandFailure::IdleTimedOut { seconds } => {
                    ExecutionFailureKind::TimedOut {
                        seconds: Some(*seconds),
                    }
                }
                CommandFailure::OutputLimit { .. } => {
                    ExecutionFailureKind::Manifest(ManifestError::InvalidPolicy)
                }
            };
            return Self::with_safe_source(kind, error.to_string(), failure.to_string());
        }
        if let Some(failure) = error.downcast_ref::<RunFailure>() {
            let kind = match failure {
                RunFailure::RequiredWorkflowsTimedOut { seconds } => {
                    ExecutionFailureKind::TimedOut {
                        seconds: Some(*seconds),
                    }
                }
                RunFailure::RequiredWorkflowFailed { repairable, .. } => {
                    ExecutionFailureKind::Validation(ValidationError {
                        gate: None,
                        repairable: *repairable,
                    })
                }
                RunFailure::ValidationRepairsExhausted { .. }
                | RunFailure::HumanIntervention { .. } => ExecutionFailureKind::HumanBlocked,
                RunFailure::InfrastructureTransient { .. } => {
                    ExecutionFailureKind::Infrastructure(InfrastructureError {
                        component: "worker".into(),
                        retryable: true,
                    })
                }
                RunFailure::PolicyViolation { .. } => {
                    ExecutionFailureKind::Manifest(ManifestError::InvalidPolicy)
                }
                RunFailure::Invariant { .. } => ExecutionFailureKind::Invariant,
            };
            return Self::with_safe_source(kind, error.to_string(), failure.to_string());
        }
        if let Some(failure) = error.downcast_ref::<crate::manifest::ManifestValidationError>() {
            return Self::with_safe_source(
                ExecutionFailureKind::Manifest(failure.kind),
                failure.to_string(),
                "the signed manifest or execution policy failed validation",
            );
        }
        if let Some(failure) = error.downcast_ref::<crate::journal::JournalError>() {
            let kind = match failure.operation {
                crate::journal::JournalOperation::Load => RecoveryError::JournalRead,
                crate::journal::JournalOperation::Persist => RecoveryError::JournalWrite,
                crate::journal::JournalOperation::Validate => RecoveryError::InvalidCheckpoint,
                crate::journal::JournalOperation::AdoptRecovery => RecoveryError::JournalRead,
            };
            return Self::with_safe_source(
                ExecutionFailureKind::Recovery(kind),
                failure.to_string(),
                "the recovery journal boundary rejected the operation",
            );
        }
        if let Some(failure) = error.downcast_ref::<crate::mission::MissionContractError>() {
            return Self::with_safe_source(
                ExecutionFailureKind::Manifest(ManifestError::InvalidPolicy),
                failure.to_string(),
                "the signed mission operation did not match its typed contract",
            );
        }
        if let Some(moved) = error.downcast_ref::<RemoteBranchMoved>() {
            return Self::with_safe_source(
                ExecutionFailureKind::Repository(RepositoryError::RemoteBranchMoved {
                    branch: moved.branch().to_owned(),
                }),
                moved.to_string(),
                "the remote branch changed after reconciliation",
            );
        }
        if let Some(control_plane) = crate::api::typed_control_plane_error(&error) {
            return Self::with_safe_source(
                ExecutionFailureKind::ControlPlane(control_plane),
                error.to_string(),
                "the RustGrid control-plane request failed",
            );
        }
        let message = error.to_string();
        Self::with_safe_source(
            ExecutionFailureKind::Infrastructure(InfrastructureError {
                component: "worker".into(),
                retryable: false,
            }),
            message,
            "an unclassified local worker operation failed",
        )
    }
}

pub(crate) fn redact_diagnostic(mut message: String) -> String {
    for marker in ["Bearer ", "token=", "api_key=", "password="] {
        let mut search_from = 0;
        while let Some(relative) = message[search_from..].find(marker) {
            let start = search_from + relative + marker.len();
            let end = message[start..]
                .find(|character: char| {
                    character.is_whitespace() || matches!(character, '&' | ',' | '}' | ']')
                })
                .map_or(message.len(), |relative| start + relative);
            message.replace_range(start..end, "<redacted>");
            search_from = start + "<redacted>".len();
        }
    }
    message
}

impl From<CancellationError> for ExecutionFailure {
    fn from(error: CancellationError) -> Self {
        Self::new(
            ExecutionFailureKind::Cancellation(error),
            "execution was cancelled",
        )
    }
}

impl From<AccessError> for ExecutionFailure {
    fn from(error: AccessError) -> Self {
        Self::new(ExecutionFailureKind::Access(error), "access was rejected")
    }
}

impl From<ManifestError> for ExecutionFailure {
    fn from(error: ManifestError) -> Self {
        Self::new(
            ExecutionFailureKind::Manifest(error),
            "execution manifest or policy was rejected",
        )
    }
}

impl From<ControlPlaneError> for ExecutionFailure {
    fn from(error: ControlPlaneError) -> Self {
        Self::new(
            ExecutionFailureKind::ControlPlane(error),
            "RustGrid control-plane operation failed",
        )
    }
}

impl From<ProviderError> for ExecutionFailure {
    fn from(error: ProviderError) -> Self {
        Self::new(
            ExecutionFailureKind::Provider(error),
            "model provider operation failed",
        )
    }
}

impl From<RepositoryError> for ExecutionFailure {
    fn from(error: RepositoryError) -> Self {
        Self::new(
            ExecutionFailureKind::Repository(error),
            "repository operation failed",
        )
    }
}

impl From<ValidationError> for ExecutionFailure {
    fn from(error: ValidationError) -> Self {
        Self::new(
            ExecutionFailureKind::Validation(error),
            "repository validation failed",
        )
    }
}

impl From<PublicationError> for ExecutionFailure {
    fn from(error: PublicationError) -> Self {
        Self::new(
            ExecutionFailureKind::Publication(error),
            "publication failed",
        )
    }
}

impl From<RecoveryError> for ExecutionFailure {
    fn from(error: RecoveryError) -> Self {
        Self::new(
            ExecutionFailureKind::Recovery(error),
            "journal or recovery operation failed",
        )
    }
}

impl From<InfrastructureError> for ExecutionFailure {
    fn from(error: InfrastructureError) -> Self {
        Self::new(
            ExecutionFailureKind::Infrastructure(error),
            "local infrastructure failed",
        )
    }
}

impl fmt::Display for ExecutionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.context)
    }
}

impl fmt::Debug for ExecutionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionFailure")
            .field("kind", &self.kind)
            .field("context", &self.context)
            .field("source", &self.source)
            .finish()
    }
}

impl Error for ExecutionFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|source| source as &dyn Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_failures() -> Vec<(ExecutionFailure, TerminalOutcome, TelemetryErrorCode)> {
        use ExecutionFailureKind as K;
        vec![
            (
                ExecutionFailure::new(K::Cancellation(CancellationError::Requested), "cancelled"),
                TerminalOutcome::Cancelled,
                TelemetryErrorCode::ExecutionCancelled,
            ),
            (
                ExecutionFailure::new(K::Shutdown, "shutdown"),
                TerminalOutcome::Cancelled,
                TelemetryErrorCode::WorkerShutdown,
            ),
            (
                ExecutionFailure::new(
                    K::LeaseLost {
                        operation: "heartbeat".into(),
                    },
                    "lease",
                ),
                TerminalOutcome::LeaseLost,
                TelemetryErrorCode::LeaseLost,
            ),
            (
                ExecutionFailure::new(K::Access(AccessError::AuthenticationRejected), "authn"),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::AuthenticationRejected,
            ),
            (
                ExecutionFailure::new(K::Access(AccessError::AuthorizationRejected), "authz"),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::AuthorizationRejected,
            ),
            (
                ExecutionFailure::new(K::Manifest(ManifestError::InvalidSignature), "manifest"),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::InvalidSignedManifest,
            ),
            (
                ExecutionFailure::new(K::Manifest(ManifestError::InvalidPolicy), "policy"),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::InvalidExecutionPolicy,
            ),
            (
                ExecutionFailure::new(
                    K::ControlPlane(ControlPlaneError::Retryable {
                        operation: "claim".into(),
                        status: Some(503),
                        request_id: None,
                    }),
                    "cp",
                ),
                TerminalOutcome::Failed,
                TelemetryErrorCode::ControlPlaneUnavailable,
            ),
            (
                ExecutionFailure::new(
                    K::ControlPlane(ControlPlaneError::Rejected {
                        operation: "claim".into(),
                        status: Some(422),
                        request_id: None,
                    }),
                    "cp",
                ),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::ControlPlaneRejected,
            ),
            (
                ExecutionFailure::new(K::Provider(ProviderError::Protocol), "provider"),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::ProviderProtocolFailure,
            ),
            (
                ExecutionFailure::new(K::Provider(ProviderError::BudgetExhausted), "budget"),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::ProviderBudgetExhausted,
            ),
            (
                ExecutionFailure::new(K::Provider(ProviderError::InvalidArtifact), "artifact"),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::InvalidModelArtifact,
            ),
            (
                ExecutionFailure::new(
                    K::Repository(RepositoryError::Conflict { path: None }),
                    "conflict",
                ),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::RepositoryConflict,
            ),
            (
                ExecutionFailure::new(
                    K::Repository(RepositoryError::RemoteBranchMoved { branch: "b".into() }),
                    "remote",
                ),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::RemoteBranchMoved,
            ),
            (
                ExecutionFailure::new(
                    K::Validation(ValidationError {
                        gate: None,
                        repairable: false,
                    }),
                    "validation",
                ),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::ValidationFailed,
            ),
            (
                ExecutionFailure::new(
                    K::Publication(PublicationError {
                        stage: "push".into(),
                        retryable: false,
                    }),
                    "publish",
                ),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::PublicationFailed,
            ),
            (
                ExecutionFailure::new(K::Recovery(RecoveryError::JournalRead), "journal"),
                TerminalOutcome::Failed,
                TelemetryErrorCode::RecoveryFailed,
            ),
            (
                ExecutionFailure::new(
                    K::Infrastructure(InfrastructureError {
                        component: "sandbox".into(),
                        retryable: true,
                    }),
                    "infra",
                ),
                TerminalOutcome::Failed,
                TelemetryErrorCode::LocalInfrastructureFailed,
            ),
            (
                ExecutionFailure::new(K::TimedOut { seconds: Some(30) }, "timeout"),
                TerminalOutcome::TimedOut,
                TelemetryErrorCode::ExecutionTimedOut,
            ),
            (
                ExecutionFailure::new(K::HumanBlocked, "human"),
                TerminalOutcome::Blocked,
                TelemetryErrorCode::HumanInterventionRequired,
            ),
            (
                ExecutionFailure::new(K::Invariant, "invariant"),
                TerminalOutcome::Failed,
                TelemetryErrorCode::InternalInvariantFailed,
            ),
        ]
    }

    #[test]
    fn every_top_level_failure_has_one_terminal_outcome_and_telemetry_code() {
        for (failure, outcome, code) in all_failures() {
            assert_eq!(failure.terminal_outcome(), outcome);
            assert_eq!(failure.telemetry_code(), code);
            assert!(!failure.telemetry_code().as_str().is_empty());
        }
    }

    #[test]
    fn policy_fields_cover_every_retryability_dependent_outcome() {
        let cases = [
            ExecutionFailure::from(CancellationError::Command),
            ExecutionFailure::from(ValidationError {
                gate: Some("test".into()),
                repairable: true,
            }),
            ExecutionFailure::from(PublicationError {
                stage: "push".into(),
                retryable: true,
            }),
            ExecutionFailure::from(PublicationError {
                stage: "pull_request".into(),
                retryable: false,
            }),
            ExecutionFailure::from(RecoveryError::JournalWrite),
            ExecutionFailure::from(RecoveryError::InvalidCheckpoint),
            ExecutionFailure::from(RecoveryError::Publication),
            ExecutionFailure::from(InfrastructureError {
                component: "sandbox".into(),
                retryable: false,
            }),
        ];
        let expected = [
            (false, TerminalOutcome::Cancelled),
            (true, TerminalOutcome::Blocked),
            (true, TerminalOutcome::Failed),
            (false, TerminalOutcome::Blocked),
            (true, TerminalOutcome::Failed),
            (false, TerminalOutcome::Blocked),
            (true, TerminalOutcome::Failed),
            (false, TerminalOutcome::Blocked),
        ];
        for (failure, expected) in cases.into_iter().zip(expected) {
            assert_eq!(failure.retryable(), expected.0);
            assert_eq!(failure.terminal_outcome(), expected.1);
        }
    }

    #[test]
    fn retryability_survives_anyhow_conversion() {
        let original = RunFailure::InfrastructureTransient {
            detail: "dns".into(),
        };
        let failure = ExecutionFailure::from_anyhow(anyhow::Error::new(original));
        assert!(failure.retryable());

        let control_plane = ExecutionFailure::from(ControlPlaneError::Retryable {
            operation: "heartbeat".into(),
            status: Some(503),
            request_id: Some("request-1".into()),
        });
        assert!(control_plane.retryable());
    }

    #[test]
    fn formatted_errors_and_sources_do_not_contain_sensitive_fields() {
        let secret = "rg_secret_token_123";
        let failure = ExecutionFailure::with_safe_source(
            ExecutionFailureKind::Access(AccessError::AuthenticationRejected),
            format!("authentication was rejected: authorization Bearer {secret}"),
            format!("credential token={secret}"),
        );
        let chain = format!(
            "{}: {}",
            failure,
            failure.source().expect("safe diagnostic source")
        );
        assert!(!format!("{failure:?}").contains(secret));
        assert!(!format!("{failure}").contains(secret));
        assert!(!chain.contains(secret));
        assert!(failure.source().is_some());
    }

    #[test]
    fn lease_loss_forbids_terminal_writes_and_cancellation_is_not_infrastructure() {
        let lease = ExecutionFailure::new(
            ExecutionFailureKind::LeaseLost {
                operation: "heartbeat".into(),
            },
            "lease lost",
        );
        assert!(!lease.may_publish_terminal_state());

        let cancellation = ExecutionFailure::from(CancellationError::Requested);
        assert!(cancellation.may_publish_terminal_state());
        assert_eq!(cancellation.terminal_outcome(), TerminalOutcome::Cancelled);
        assert_eq!(
            cancellation.telemetry_code(),
            TelemetryErrorCode::ExecutionCancelled
        );
    }
}
