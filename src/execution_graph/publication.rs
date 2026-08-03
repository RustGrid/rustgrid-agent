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
