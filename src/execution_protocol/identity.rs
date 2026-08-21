use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

macro_rules! protocol_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn new(value: impl Into<String>) -> Self {
                let value = value.into();
                assert!(
                    !value.trim().is_empty(),
                    concat!(stringify!($name), " must not be empty")
                );
                Self(value)
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }

            pub(crate) fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                if value.trim().is_empty() {
                    return Err(serde::de::Error::custom(concat!(
                        stringify!($name),
                        " must not be empty"
                    )));
                }
                Ok(Self(value))
            }
        }
    };
}

protocol_id!(ExecutionId);
protocol_id!(NodeId);
protocol_id!(ProofId);
protocol_id!(EventId);
protocol_id!(EvidenceId);
protocol_id!(FailureRevisionId);
protocol_id!(ModelCallId);
protocol_id!(ActionId);
protocol_id!(EffectId);
protocol_id!(RepositoryRevisionId);
protocol_id!(RepositoryProfileId);
protocol_id!(SearchId);
protocol_id!(ContextManifestId);
protocol_id!(ReservationId);
protocol_id!(PlanId);
protocol_id!(PlanRevisionId);
protocol_id!(ChangeId);
protocol_id!(TargetId);
protocol_id!(ValidationExpectationId);
protocol_id!(ValidationPolicyId);
protocol_id!(ValidationGateId);
protocol_id!(ValidationRunId);
protocol_id!(ValidationProcessId);
protocol_id!(ValidationEvidenceId);
protocol_id!(RepairCandidateId);
protocol_id!(RepairIntentId);

pub(crate) fn stable_sha256(parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        let length = u64::try_from(part.len()).expect("protocol identity input length fits u64");
        digest.update(length.to_be_bytes());
        digest.update(part.as_bytes());
    }
    hex::encode(digest.finalize())
}

impl EventId {
    pub(crate) fn derive(
        execution_id: &ExecutionId,
        execution_attempt: u32,
        semantic_key: &str,
    ) -> Self {
        Self::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:event",
                execution_id.as_str(),
                &execution_attempt.to_string(),
                semantic_key,
            ])
        ))
    }
}
