// Serialized inventory (schema version 1):
//
// Roots: ExecutionSnapshot, ExecutionGraph, ExecutionDomainEvent.
// Graph/model: ComplexityClassificationStage, ComplexityFactor,
// ComplexityFactorKind, ComplexityInput, ComplexityAssessment,
// MissionComplexity, MissionBudget, MissionBudgetOverride, AcceptedPlan,
// PlannedTarget, ExecutionNode, ExecutionNodeKind, NodeCapability, ExecutionNodeStatus,
// NodeAttempt, NodeBudget, NodeBudgetRemaining, DerivedExecutionCollections.
// Validation/evidence: RepositorySnapshot, LineRange, FileEvidence,
// FileExcerpt, EvidenceSummary, EvidenceKind, EvidenceRecord, EvidenceStore,
// ValidationGateType, ValidationGateSpec, ValidationTimeoutPolicy,
// ValidationRetryPolicy, ValidationNodeBudget, ValidationEvidenceStatus,
// ValidationEvidenceRecord.
// Recovery/failure: ToolKind, TargetExecutionContext, MutationResult,
// FailureCategory, FailureStatus, FailureRecord, FailureStore,
// MutationIntentKind, MutationEventContext, RepairAttemptReservation,
// RepairAttemptReservationState, RepairTargetState.
// Accounting/publication: NodeBudgetUsage, BudgetState, ProgressWindow,
// ProgressEventKind, ProgressEvent, PublicationMode, PublicationStatus,
// PublicationState, CancellationState, MissionOutcome, GuardrailReason.
// Stable transparent identifiers: ExecutionNodeId, MutationTargetId,
// ValidationNodeId, RepositoryFingerprint, FailureId, EvidenceId, ArtifactId.
// Type aliases intentionally retain their underlying representation:
// MutationTarget and ValidationGateKind.

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
string_id!(MutationTargetId);
string_id!(ValidationNodeId);
string_id!(RepositoryFingerprint);
string_id!(FailureId);
string_id!(EvidenceId);
string_id!(ArtifactId);
