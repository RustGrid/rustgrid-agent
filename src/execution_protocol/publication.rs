//! Pure publication journaling and reconciliation contracts.
//!
//! This module proves ordering, identity, retry, and idempotency properties
//! after publication eligibility is granted. The aggregate reducer remains
//! responsible for proving that `CommitTreeBindingV1.manifest_id` and
//! `diff_hash` came from its currently validated `ReviewStateV1` manifest and
//! that the tree/parent OIDs came from its current repository observation; an
//! eligibility record intentionally exposes only the manifest identity.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use super::{
    DiffManifestId, DiffManifestV1, EffectId, EvidenceId, ExecutionId, NodeId,
    PublicationAuthorityObservationV1, PublicationContractV1, PublicationEligibilityId,
    PublicationEligibilityRecord, PublicationModeV1, RepositoryRevisionId, stable_sha256,
};

pub(crate) const PUBLICATION_SCHEMA_VERSION: u16 = 1;

const MAX_SAFE_CODE_BYTES: usize = 128;
const MAX_GIT_REF_BYTES: usize = 256;
const MAX_PULL_REQUEST_TITLE_BYTES: usize = 512;
const MAX_PULL_REQUEST_BODY_BYTES: usize = 256 * 1024;
const MAX_PULL_REQUEST_URL_BYTES: usize = 2_048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PublicationContractError {
    Invalid { code: &'static str },
    LimitExceeded { field: &'static str, maximum: usize },
    Serialization,
    ReviewContract { code: &'static str },
}

impl PublicationContractError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::Invalid { code } | Self::ReviewContract { code } => code,
            Self::LimitExceeded { .. } => "publication_contract_limit_exceeded",
            Self::Serialization => "publication_contract_serialization_failed",
        }
    }
}

impl fmt::Display for PublicationContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { code } => write!(formatter, "publication contract violates `{code}`"),
            Self::LimitExceeded { field, maximum } => {
                write!(formatter, "publication field `{field}` exceeds {maximum}")
            }
            Self::Serialization => formatter.write_str("publication identity serialization failed"),
            Self::ReviewContract { code } => {
                write!(formatter, "publication prerequisite violates `{code}`")
            }
        }
    }
}

impl std::error::Error for PublicationContractError {}

impl From<super::ReviewContractError> for PublicationContractError {
    fn from(error: super::ReviewContractError) -> Self {
        Self::ReviewContract { code: error.code() }
    }
}

macro_rules! publication_id {
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
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
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

publication_id!(PublicationAttemptId);
publication_id!(CommitIntentId);
publication_id!(CommitObservationId);
publication_id!(PushIntentId);
publication_id!(PushObservationId);
publication_id!(PullRequestIntentId);
publication_id!(PullRequestObservationId);
publication_id!(PublicationCompletionId);
publication_id!(PublicationConvergenceId);
publication_id!(RemoteBranchMovementId);

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublicationOperationV1 {
    Commit,
    Push,
    PullRequest,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationAttemptV1 {
    pub(crate) schema_version: u16,
    pub(crate) attempt_id: PublicationAttemptId,
    pub(crate) sequence: u32,
    pub(crate) operation: PublicationOperationV1,
    pub(crate) operation_attempt: u32,
    pub(crate) prior_attempt_id: Option<PublicationAttemptId>,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) eligibility_id: PublicationEligibilityId,
    pub(crate) attempt_hash: String,
}

impl PublicationAttemptV1 {
    fn new(
        sequence: u32,
        operation: PublicationOperationV1,
        operation_attempt: u32,
        prior_attempt_id: Option<PublicationAttemptId>,
        repository_revision: RepositoryRevisionId,
        eligibility_id: PublicationEligibilityId,
    ) -> Result<Self, PublicationContractError> {
        let attempt_hash = Self::expected_hash(
            sequence,
            operation,
            operation_attempt,
            prior_attempt_id.as_ref(),
            &repository_revision,
            &eligibility_id,
        )?;
        let attempt = Self {
            schema_version: PUBLICATION_SCHEMA_VERSION,
            attempt_id: PublicationAttemptId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:publication-attempt",
                    eligibility_id.as_str(),
                    &sequence.to_string(),
                    &attempt_hash,
                ])
            )),
            sequence,
            operation,
            operation_attempt,
            prior_attempt_id,
            repository_revision,
            eligibility_id,
            attempt_hash,
        };
        attempt.validate()?;
        Ok(attempt)
    }

    pub(crate) fn validate(&self) -> Result<(), PublicationContractError> {
        let expected_hash = Self::expected_hash(
            self.sequence,
            self.operation,
            self.operation_attempt,
            self.prior_attempt_id.as_ref(),
            &self.repository_revision,
            &self.eligibility_id,
        )?;
        let expected_id = PublicationAttemptId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:publication-attempt",
                self.eligibility_id.as_str(),
                &self.sequence.to_string(),
                &expected_hash,
            ])
        ));
        if self.schema_version != PUBLICATION_SCHEMA_VERSION
            || self.sequence == 0
            || self.operation_attempt == 0
            || self.attempt_hash != expected_hash
            || self.attempt_id != expected_id
        {
            return Err(PublicationContractError::Invalid {
                code: "publication_attempt_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(
        sequence: u32,
        operation: PublicationOperationV1,
        operation_attempt: u32,
        prior_attempt_id: Option<&PublicationAttemptId>,
        repository_revision: &RepositoryRevisionId,
        eligibility_id: &PublicationEligibilityId,
    ) -> Result<String, PublicationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:publication-attempt-record",
            &canonical_json(&(
                PUBLICATION_SCHEMA_VERSION,
                sequence,
                operation,
                operation_attempt,
                prior_attempt_id,
                repository_revision,
                eligibility_id,
            ))?,
        ]))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitIntentV1 {
    pub(crate) schema_version: u16,
    pub(crate) intent_id: CommitIntentId,
    pub(crate) effect_id: EffectId,
    pub(crate) attempt: PublicationAttemptV1,
    pub(crate) contract_id: EvidenceId,
    /// Bootstrap-authorized commit metadata and identity policy. This is not
    /// the resulting commit, parent, or tree identity.
    pub(crate) commit_identity_hash: String,
    pub(crate) tree: CommitTreeBindingV1,
    pub(crate) intent_hash: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitTreeBindingV1 {
    pub(crate) manifest_id: DiffManifestId,
    pub(crate) diff_hash: String,
    pub(crate) repository_tree_oid: String,
    pub(crate) parent_commit_oid: String,
    pub(crate) binding_hash: String,
}

impl CommitTreeBindingV1 {
    pub(crate) fn from_review_authority(
        eligibility: &PublicationEligibilityRecord,
        manifest: &DiffManifestV1,
        authority: &PublicationAuthorityObservationV1,
    ) -> Result<Self, PublicationContractError> {
        if manifest.manifest_id != eligibility.manifest_id
            || manifest.repository_revision != eligibility.repository_revision
            || authority.authority_id != eligibility.authority_id
            || authority.repository_revision != eligibility.repository_revision
            || !authority.cancellation_absent
            || !authority.lease_valid
            || !authority.remote_head_unchanged
        {
            return Err(PublicationContractError::Invalid {
                code: "commit_tree_review_authority_mismatch",
            });
        }
        Self::new(
            eligibility,
            manifest.diff_hash.clone(),
            authority.repository_tree_oid.clone(),
            authority.repository_head_oid.clone(),
        )
    }

    pub(crate) fn new(
        eligibility: &PublicationEligibilityRecord,
        diff_hash: String,
        repository_tree_oid: String,
        parent_commit_oid: String,
    ) -> Result<Self, PublicationContractError> {
        let binding_hash = Self::expected_hash(
            &eligibility.manifest_id,
            &diff_hash,
            &repository_tree_oid,
            &parent_commit_oid,
        )?;
        let binding = Self {
            manifest_id: eligibility.manifest_id.clone(),
            diff_hash,
            repository_tree_oid,
            parent_commit_oid,
            binding_hash,
        };
        binding.validate_against(eligibility)?;
        Ok(binding)
    }

    pub(crate) fn validate_against(
        &self,
        eligibility: &PublicationEligibilityRecord,
    ) -> Result<(), PublicationContractError> {
        if self.manifest_id != eligibility.manifest_id
            || !is_sha256(&self.diff_hash)
            || !git_oid_is_valid(&self.repository_tree_oid)
            || !git_oid_is_valid(&self.parent_commit_oid)
            || self.binding_hash
                != Self::expected_hash(
                    &self.manifest_id,
                    &self.diff_hash,
                    &self.repository_tree_oid,
                    &self.parent_commit_oid,
                )?
        {
            return Err(PublicationContractError::Invalid {
                code: "commit_tree_binding_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(
        manifest_id: &DiffManifestId,
        diff_hash: &str,
        repository_tree_oid: &str,
        parent_commit_oid: &str,
    ) -> Result<String, PublicationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:commit-tree-binding",
            &canonical_json(&(
                manifest_id,
                diff_hash,
                repository_tree_oid,
                parent_commit_oid,
            ))?,
        ]))
    }
}

impl CommitIntentV1 {
    fn new(
        attempt: PublicationAttemptV1,
        contract: &PublicationContractV1,
        eligibility: &PublicationEligibilityRecord,
        tree: CommitTreeBindingV1,
    ) -> Result<Self, PublicationContractError> {
        tree.validate_against(eligibility)?;
        let intent_hash = Self::expected_hash(
            &attempt,
            &contract.contract_id,
            &contract.commit_identity_hash,
            &tree,
        )?;
        let intent_id = CommitIntentId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:commit-intent",
                attempt.attempt_id.as_str(),
                &intent_hash,
            ])
        ));
        let intent = Self {
            schema_version: PUBLICATION_SCHEMA_VERSION,
            effect_id: EffectId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:create-commit-effect",
                    intent_id.as_str(),
                ])
            )),
            intent_id,
            attempt,
            contract_id: contract.contract_id.clone(),
            commit_identity_hash: contract.commit_identity_hash.clone(),
            tree,
            intent_hash,
        };
        intent.validate_against(contract, eligibility)?;
        Ok(intent)
    }

    pub(crate) fn validate_against(
        &self,
        contract: &PublicationContractV1,
        eligibility: &PublicationEligibilityRecord,
    ) -> Result<(), PublicationContractError> {
        self.attempt.validate()?;
        self.tree.validate_against(eligibility)?;
        let expected_hash = Self::expected_hash(
            &self.attempt,
            &self.contract_id,
            &self.commit_identity_hash,
            &self.tree,
        )?;
        let expected_id = CommitIntentId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:commit-intent",
                self.attempt.attempt_id.as_str(),
                &expected_hash,
            ])
        ));
        let expected_effect_id = EffectId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:create-commit-effect",
                expected_id.as_str(),
            ])
        ));
        if self.schema_version != PUBLICATION_SCHEMA_VERSION
            || self.attempt.operation != PublicationOperationV1::Commit
            || self.contract_id != contract.contract_id
            || self.commit_identity_hash != contract.commit_identity_hash
            || contract
                .expected_remote_head
                .as_ref()
                .is_some_and(|expected| expected != &self.tree.parent_commit_oid)
            || !is_sha256(&self.commit_identity_hash)
            || self.intent_hash != expected_hash
            || self.intent_id != expected_id
            || self.effect_id != expected_effect_id
        {
            return Err(PublicationContractError::Invalid {
                code: "commit_intent_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(
        attempt: &PublicationAttemptV1,
        contract_id: &EvidenceId,
        commit_identity_hash: &str,
        tree: &CommitTreeBindingV1,
    ) -> Result<String, PublicationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:commit-intent-record",
            &canonical_json(&(attempt, contract_id, commit_identity_hash, tree))?,
        ]))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommitReconciliationV1 {
    Created,
    AlreadySatisfied,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CommitOutcomeV1 {
    Confirmed {
        reconciliation: CommitReconciliationV1,
        commit_oid: String,
        repository_tree_oid: String,
        parent_commit_oid: String,
        commit_identity_hash: String,
    },
    Failed {
        failure: PublicationEffectFailureV1,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitObservationV1 {
    pub(crate) schema_version: u16,
    pub(crate) observation_id: CommitObservationId,
    pub(crate) effect_id: EffectId,
    pub(crate) intent_id: CommitIntentId,
    pub(crate) attempt_id: PublicationAttemptId,
    pub(crate) outcome: CommitOutcomeV1,
    pub(crate) observation_hash: String,
}

impl CommitObservationV1 {
    pub(crate) fn new(
        intent: &CommitIntentV1,
        outcome: CommitOutcomeV1,
    ) -> Result<Self, PublicationContractError> {
        let observation_hash = Self::expected_hash(intent, &outcome)?;
        let observation = Self {
            schema_version: PUBLICATION_SCHEMA_VERSION,
            observation_id: CommitObservationId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:commit-observation",
                    intent.intent_id.as_str(),
                    &observation_hash,
                ])
            )),
            effect_id: intent.effect_id.clone(),
            intent_id: intent.intent_id.clone(),
            attempt_id: intent.attempt.attempt_id.clone(),
            outcome,
            observation_hash,
        };
        observation.validate_against(intent)?;
        Ok(observation)
    }

    pub(crate) fn validate_against(
        &self,
        intent: &CommitIntentV1,
    ) -> Result<(), PublicationContractError> {
        let outcome_valid = match &self.outcome {
            CommitOutcomeV1::Confirmed {
                commit_oid,
                repository_tree_oid,
                parent_commit_oid,
                commit_identity_hash,
                ..
            } => {
                git_oid_is_valid(commit_oid)
                    && repository_tree_oid == &intent.tree.repository_tree_oid
                    && parent_commit_oid == &intent.tree.parent_commit_oid
                    && commit_identity_hash == &intent.commit_identity_hash
            }
            CommitOutcomeV1::Failed { failure } => failure.validate(),
        };
        let expected_hash = Self::expected_hash(intent, &self.outcome)?;
        let expected_id = CommitObservationId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:commit-observation",
                intent.intent_id.as_str(),
                &expected_hash,
            ])
        ));
        if self.schema_version != PUBLICATION_SCHEMA_VERSION
            || self.effect_id != intent.effect_id
            || self.intent_id != intent.intent_id
            || self.attempt_id != intent.attempt.attempt_id
            || !outcome_valid
            || self.observation_hash != expected_hash
            || self.observation_id != expected_id
        {
            return Err(PublicationContractError::Invalid {
                code: "commit_observation_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(
        intent: &CommitIntentV1,
        outcome: &CommitOutcomeV1,
    ) -> Result<String, PublicationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:commit-observation-record",
            &canonical_json(&(
                &intent.effect_id,
                &intent.intent_id,
                &intent.attempt.attempt_id,
                outcome,
            ))?,
        ]))
    }

    pub(crate) fn confirmed_commit_oid(&self) -> Option<&str> {
        match &self.outcome {
            CommitOutcomeV1::Confirmed { commit_oid, .. } => Some(commit_oid),
            CommitOutcomeV1::Failed { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExactLeasePushIntentV1 {
    pub(crate) schema_version: u16,
    pub(crate) intent_id: PushIntentId,
    pub(crate) effect_id: EffectId,
    pub(crate) attempt: PublicationAttemptV1,
    pub(crate) contract_id: EvidenceId,
    pub(crate) repository_binding_hash: String,
    pub(crate) installation_binding_hash: String,
    pub(crate) head_branch: String,
    pub(crate) commit_oid: String,
    pub(crate) expected_remote_head: Option<String>,
    pub(crate) intent_hash: String,
}

impl ExactLeasePushIntentV1 {
    fn new(
        attempt: PublicationAttemptV1,
        contract: &PublicationContractV1,
        commit_oid: String,
    ) -> Result<Self, PublicationContractError> {
        let intent_hash = Self::expected_hash(
            &attempt,
            contract,
            &commit_oid,
            &contract.expected_remote_head,
        )?;
        let intent_id = PushIntentId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:exact-lease-push-intent",
                attempt.attempt_id.as_str(),
                &intent_hash,
            ])
        ));
        let intent = Self {
            schema_version: PUBLICATION_SCHEMA_VERSION,
            effect_id: EffectId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:exact-lease-push-effect",
                    intent_id.as_str(),
                ])
            )),
            intent_id,
            attempt,
            contract_id: contract.contract_id.clone(),
            repository_binding_hash: contract.repository_binding_hash.clone(),
            installation_binding_hash: contract.installation_binding_hash.clone(),
            head_branch: contract.head_branch.clone(),
            commit_oid,
            expected_remote_head: contract.expected_remote_head.clone(),
            intent_hash,
        };
        intent.validate_against(contract)?;
        Ok(intent)
    }

    pub(crate) fn validate_against(
        &self,
        contract: &PublicationContractV1,
    ) -> Result<(), PublicationContractError> {
        self.attempt.validate()?;
        let expected_hash = Self::expected_hash(
            &self.attempt,
            contract,
            &self.commit_oid,
            &self.expected_remote_head,
        )?;
        let expected_id = PushIntentId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:exact-lease-push-intent",
                self.attempt.attempt_id.as_str(),
                &expected_hash,
            ])
        ));
        let expected_effect_id = EffectId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:exact-lease-push-effect",
                expected_id.as_str(),
            ])
        ));
        if self.schema_version != PUBLICATION_SCHEMA_VERSION
            || self.attempt.operation != PublicationOperationV1::Push
            || self.contract_id != contract.contract_id
            || self.repository_binding_hash != contract.repository_binding_hash
            || self.installation_binding_hash != contract.installation_binding_hash
            || self.head_branch != contract.head_branch
            || self.expected_remote_head != contract.expected_remote_head
            || !is_sha256(&self.repository_binding_hash)
            || !is_sha256(&self.installation_binding_hash)
            || !git_ref_is_valid(&self.head_branch)
            || !git_oid_is_valid(&self.commit_oid)
            || self
                .expected_remote_head
                .as_ref()
                .is_some_and(|head| !git_oid_is_valid(head))
            || self.intent_hash != expected_hash
            || self.intent_id != expected_id
            || self.effect_id != expected_effect_id
        {
            return Err(PublicationContractError::Invalid {
                code: "exact_lease_push_intent_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(
        attempt: &PublicationAttemptV1,
        contract: &PublicationContractV1,
        commit_oid: &str,
        expected_remote_head: &Option<String>,
    ) -> Result<String, PublicationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:exact-lease-push-intent-record",
            &canonical_json(&(
                attempt,
                &contract.contract_id,
                &contract.repository_binding_hash,
                &contract.installation_binding_hash,
                &contract.head_branch,
                commit_oid,
                expected_remote_head,
            ))?,
        ]))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteBranchMoved {
    pub(crate) schema_version: u16,
    pub(crate) movement_id: RemoteBranchMovementId,
    pub(crate) push_intent_id: PushIntentId,
    pub(crate) expected_remote_head: Option<String>,
    pub(crate) observed_remote_head: Option<String>,
    pub(crate) movement_hash: String,
}

impl RemoteBranchMoved {
    pub(crate) fn new(
        intent: &ExactLeasePushIntentV1,
        observed_remote_head: Option<String>,
    ) -> Result<Self, PublicationContractError> {
        let movement_hash = Self::expected_hash(
            &intent.intent_id,
            &intent.expected_remote_head,
            &observed_remote_head,
        )?;
        let movement = Self {
            schema_version: PUBLICATION_SCHEMA_VERSION,
            movement_id: RemoteBranchMovementId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:remote-branch-moved",
                    intent.intent_id.as_str(),
                    &movement_hash,
                ])
            )),
            push_intent_id: intent.intent_id.clone(),
            expected_remote_head: intent.expected_remote_head.clone(),
            observed_remote_head,
            movement_hash,
        };
        movement.validate_against(intent)?;
        Ok(movement)
    }

    pub(crate) fn validate_against(
        &self,
        intent: &ExactLeasePushIntentV1,
    ) -> Result<(), PublicationContractError> {
        let expected_hash = Self::expected_hash(
            &self.push_intent_id,
            &self.expected_remote_head,
            &self.observed_remote_head,
        )?;
        let expected_id = RemoteBranchMovementId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:remote-branch-moved",
                intent.intent_id.as_str(),
                &expected_hash,
            ])
        ));
        if self.schema_version != PUBLICATION_SCHEMA_VERSION
            || self.push_intent_id != intent.intent_id
            || self.expected_remote_head != intent.expected_remote_head
            || self.observed_remote_head == self.expected_remote_head
            || self.observed_remote_head.as_deref() == Some(intent.commit_oid.as_str())
            || self
                .observed_remote_head
                .as_ref()
                .is_some_and(|head| !git_oid_is_valid(head))
            || self.movement_hash != expected_hash
            || self.movement_id != expected_id
        {
            return Err(PublicationContractError::Invalid {
                code: "remote_branch_movement_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(
        push_intent_id: &PushIntentId,
        expected_remote_head: &Option<String>,
        observed_remote_head: &Option<String>,
    ) -> Result<String, PublicationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:remote-branch-movement-record",
            &canonical_json(&(push_intent_id, expected_remote_head, observed_remote_head))?,
        ]))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PushReconciliationV1 {
    Pushed,
    AlreadySatisfied,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ExactLeasePushOutcomeV1 {
    Confirmed {
        reconciliation: PushReconciliationV1,
        remote_head: String,
    },
    RemoteBranchMoved {
        movement: RemoteBranchMoved,
    },
    Failed {
        failure: PublicationEffectFailureV1,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExactLeasePushObservationV1 {
    pub(crate) schema_version: u16,
    pub(crate) observation_id: PushObservationId,
    pub(crate) effect_id: EffectId,
    pub(crate) intent_id: PushIntentId,
    pub(crate) attempt_id: PublicationAttemptId,
    pub(crate) outcome: ExactLeasePushOutcomeV1,
    pub(crate) observation_hash: String,
}

impl ExactLeasePushObservationV1 {
    pub(crate) fn new(
        intent: &ExactLeasePushIntentV1,
        outcome: ExactLeasePushOutcomeV1,
    ) -> Result<Self, PublicationContractError> {
        let observation_hash = Self::expected_hash(intent, &outcome)?;
        let observation = Self {
            schema_version: PUBLICATION_SCHEMA_VERSION,
            observation_id: PushObservationId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:exact-lease-push-observation",
                    intent.intent_id.as_str(),
                    &observation_hash,
                ])
            )),
            effect_id: intent.effect_id.clone(),
            intent_id: intent.intent_id.clone(),
            attempt_id: intent.attempt.attempt_id.clone(),
            outcome,
            observation_hash,
        };
        observation.validate_against(intent)?;
        Ok(observation)
    }

    pub(crate) fn validate_against(
        &self,
        intent: &ExactLeasePushIntentV1,
    ) -> Result<(), PublicationContractError> {
        let outcome_valid = match &self.outcome {
            ExactLeasePushOutcomeV1::Confirmed { remote_head, .. } => {
                remote_head == &intent.commit_oid
            }
            ExactLeasePushOutcomeV1::RemoteBranchMoved { movement } => {
                movement.validate_against(intent).is_ok()
            }
            ExactLeasePushOutcomeV1::Failed { failure } => failure.validate(),
        };
        let expected_hash = Self::expected_hash(intent, &self.outcome)?;
        let expected_id = PushObservationId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:exact-lease-push-observation",
                intent.intent_id.as_str(),
                &expected_hash,
            ])
        ));
        if self.schema_version != PUBLICATION_SCHEMA_VERSION
            || self.effect_id != intent.effect_id
            || self.intent_id != intent.intent_id
            || self.attempt_id != intent.attempt.attempt_id
            || !outcome_valid
            || self.observation_hash != expected_hash
            || self.observation_id != expected_id
        {
            return Err(PublicationContractError::Invalid {
                code: "exact_lease_push_observation_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(
        intent: &ExactLeasePushIntentV1,
        outcome: &ExactLeasePushOutcomeV1,
    ) -> Result<String, PublicationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:exact-lease-push-observation-record",
            &canonical_json(&(
                &intent.effect_id,
                &intent.intent_id,
                &intent.attempt.attempt_id,
                outcome,
            ))?,
        ]))
    }

    pub(crate) fn confirmed_remote_head(&self) -> Option<&str> {
        match &self.outcome {
            ExactLeasePushOutcomeV1::Confirmed { remote_head, .. } => Some(remote_head),
            ExactLeasePushOutcomeV1::RemoteBranchMoved { .. }
            | ExactLeasePushOutcomeV1::Failed { .. } => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RawPullRequestMaterialV1 {
    title: Vec<u8>,
    body: Vec<u8>,
}

impl RawPullRequestMaterialV1 {
    pub(crate) fn new(title: Vec<u8>, body: Vec<u8>) -> Result<Self, PublicationContractError> {
        if title.is_empty() || title.len() > MAX_PULL_REQUEST_TITLE_BYTES {
            return Err(PublicationContractError::LimitExceeded {
                field: "pull_request_title",
                maximum: MAX_PULL_REQUEST_TITLE_BYTES,
            });
        }
        if body.len() > MAX_PULL_REQUEST_BODY_BYTES {
            return Err(PublicationContractError::LimitExceeded {
                field: "pull_request_body",
                maximum: MAX_PULL_REQUEST_BODY_BYTES,
            });
        }
        if std::str::from_utf8(&title).is_err() || std::str::from_utf8(&body).is_err() {
            return Err(PublicationContractError::Invalid {
                code: "pull_request_material_not_utf8",
            });
        }
        Ok(Self { title, body })
    }

    pub(crate) fn title(&self) -> &[u8] {
        &self.title
    }

    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }

    pub(crate) fn title_hash(&self) -> String {
        hex::encode(Sha256::digest(&self.title))
    }

    pub(crate) fn body_hash(&self) -> String {
        hex::encode(Sha256::digest(&self.body))
    }

    fn validate_against(
        &self,
        intent: &PullRequestIntentV1,
    ) -> Result<(), PublicationContractError> {
        if self.title.is_empty()
            || self.title.len() > MAX_PULL_REQUEST_TITLE_BYTES
            || self.body.len() > MAX_PULL_REQUEST_BODY_BYTES
            || std::str::from_utf8(&self.title).is_err()
            || std::str::from_utf8(&self.body).is_err()
            || self.title_hash() != intent.title_hash
            || self.body_hash() != intent.body_hash
            || u64::try_from(self.title.len()).unwrap_or(u64::MAX) != intent.title_bytes
            || u64::try_from(self.body.len()).unwrap_or(u64::MAX) != intent.body_bytes
            || !is_sha256(&intent.execution_marker_hash)
            || !self
                .body
                .windows(intent.execution_marker_hash.len())
                .any(|window| window == intent.execution_marker_hash.as_bytes())
        {
            return Err(PublicationContractError::Invalid {
                code: "pull_request_material_binding_mismatch",
            });
        }
        Ok(())
    }
}

impl fmt::Debug for RawPullRequestMaterialV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawPullRequestMaterialV1")
            .field("title_bytes", &self.title.len())
            .field("body_bytes", &self.body.len())
            .field("title", &"<redacted>")
            .field("body", &"<redacted>")
            .finish()
    }
}

impl Drop for RawPullRequestMaterialV1 {
    fn drop(&mut self) {
        self.title.zeroize();
        self.body.zeroize();
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullRequestIntentV1 {
    pub(crate) schema_version: u16,
    pub(crate) intent_id: PullRequestIntentId,
    pub(crate) effect_id: EffectId,
    pub(crate) attempt: PublicationAttemptV1,
    pub(crate) contract_id: EvidenceId,
    pub(crate) repository_binding_hash: String,
    pub(crate) installation_binding_hash: String,
    pub(crate) base_ref: String,
    pub(crate) head_branch: String,
    pub(crate) commit_oid: String,
    pub(crate) requested_mode: PublicationModeV1,
    pub(crate) draft: bool,
    /// Opaque execution-scoped lookup marker used to reconcile an ambiguous
    /// create response without persisting raw title or body material.
    pub(crate) execution_marker_hash: String,
    pub(crate) title_hash: String,
    pub(crate) title_bytes: u64,
    pub(crate) body_hash: String,
    pub(crate) body_bytes: u64,
    pub(crate) intent_hash: String,
}

impl PullRequestIntentV1 {
    fn new(
        attempt: PublicationAttemptV1,
        contract: &PublicationContractV1,
        commit_oid: String,
        material: &RawPullRequestMaterialV1,
        execution_marker_hash: String,
    ) -> Result<Self, PublicationContractError> {
        let draft = matches!(
            contract.requested_mode,
            PublicationModeV1::NormalWithExternalReview
        );
        let title_hash = material.title_hash();
        let body_hash = material.body_hash();
        let title_bytes = u64::try_from(material.title.len()).unwrap_or(u64::MAX);
        let body_bytes = u64::try_from(material.body.len()).unwrap_or(u64::MAX);
        let intent_hash = Self::expected_hash(
            &attempt,
            contract,
            &commit_oid,
            draft,
            &execution_marker_hash,
            &title_hash,
            title_bytes,
            &body_hash,
            body_bytes,
        )?;
        let intent_id = PullRequestIntentId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:pull-request-intent",
                attempt.attempt_id.as_str(),
                &intent_hash,
            ])
        ));
        let intent = Self {
            schema_version: PUBLICATION_SCHEMA_VERSION,
            effect_id: EffectId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:ensure-pull-request-effect",
                    intent_id.as_str(),
                ])
            )),
            intent_id,
            attempt,
            contract_id: contract.contract_id.clone(),
            repository_binding_hash: contract.repository_binding_hash.clone(),
            installation_binding_hash: contract.installation_binding_hash.clone(),
            base_ref: contract.base_ref.clone(),
            head_branch: contract.head_branch.clone(),
            commit_oid,
            requested_mode: contract.requested_mode,
            draft,
            execution_marker_hash,
            title_hash,
            title_bytes,
            body_hash,
            body_bytes,
            intent_hash,
        };
        intent.validate_against(contract)?;
        material.validate_against(&intent)?;
        Ok(intent)
    }

    pub(crate) fn validate_against(
        &self,
        contract: &PublicationContractV1,
    ) -> Result<(), PublicationContractError> {
        self.attempt.validate()?;
        let expected_draft = matches!(
            contract.requested_mode,
            PublicationModeV1::NormalWithExternalReview
        );
        let expected_hash = Self::expected_hash(
            &self.attempt,
            contract,
            &self.commit_oid,
            self.draft,
            &self.execution_marker_hash,
            &self.title_hash,
            self.title_bytes,
            &self.body_hash,
            self.body_bytes,
        )?;
        let expected_id = PullRequestIntentId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:pull-request-intent",
                self.attempt.attempt_id.as_str(),
                &expected_hash,
            ])
        ));
        let expected_effect_id = EffectId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:ensure-pull-request-effect",
                expected_id.as_str(),
            ])
        ));
        if self.schema_version != PUBLICATION_SCHEMA_VERSION
            || self.attempt.operation != PublicationOperationV1::PullRequest
            || self.contract_id != contract.contract_id
            || self.repository_binding_hash != contract.repository_binding_hash
            || self.installation_binding_hash != contract.installation_binding_hash
            || self.base_ref != contract.base_ref
            || self.head_branch != contract.head_branch
            || self.requested_mode != contract.requested_mode
            || self.draft != expected_draft
            || !is_sha256(&self.execution_marker_hash)
            || !git_ref_is_valid(&self.base_ref)
            || !git_ref_is_valid(&self.head_branch)
            || !git_oid_is_valid(&self.commit_oid)
            || !is_sha256(&self.title_hash)
            || !is_sha256(&self.body_hash)
            || self.title_bytes == 0
            || self.title_bytes > MAX_PULL_REQUEST_TITLE_BYTES as u64
            || self.body_bytes > MAX_PULL_REQUEST_BODY_BYTES as u64
            || self.intent_hash != expected_hash
            || self.intent_id != expected_id
            || self.effect_id != expected_effect_id
        {
            return Err(PublicationContractError::Invalid {
                code: "pull_request_intent_invalid",
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn expected_hash(
        attempt: &PublicationAttemptV1,
        contract: &PublicationContractV1,
        commit_oid: &str,
        draft: bool,
        execution_marker_hash: &str,
        title_hash: &str,
        title_bytes: u64,
        body_hash: &str,
        body_bytes: u64,
    ) -> Result<String, PublicationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:pull-request-intent-record",
            &canonical_json(&(
                attempt,
                &contract.contract_id,
                &contract.repository_binding_hash,
                &contract.installation_binding_hash,
                &contract.base_ref,
                &contract.head_branch,
                commit_oid,
                contract.requested_mode,
                draft,
                execution_marker_hash,
                title_hash,
                title_bytes,
                body_hash,
                body_bytes,
            ))?,
        ]))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PullRequestReconciliationV1 {
    Created,
    Updated,
    AlreadySatisfied,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PullRequestOutcomeV1 {
    Confirmed {
        reconciliation: PullRequestReconciliationV1,
        pull_request_number: u64,
        pull_request_url: String,
        node_id_hash: String,
        base_ref: String,
        head_branch: String,
        observed_head: String,
        execution_marker_hash: String,
        draft: bool,
    },
    Failed {
        failure: PublicationEffectFailureV1,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullRequestObservationV1 {
    pub(crate) schema_version: u16,
    pub(crate) observation_id: PullRequestObservationId,
    pub(crate) effect_id: EffectId,
    pub(crate) intent_id: PullRequestIntentId,
    pub(crate) attempt_id: PublicationAttemptId,
    pub(crate) outcome: PullRequestOutcomeV1,
    pub(crate) observation_hash: String,
}

impl PullRequestObservationV1 {
    pub(crate) fn new(
        intent: &PullRequestIntentV1,
        outcome: PullRequestOutcomeV1,
    ) -> Result<Self, PublicationContractError> {
        let observation_hash = Self::expected_hash(intent, &outcome)?;
        let observation = Self {
            schema_version: PUBLICATION_SCHEMA_VERSION,
            observation_id: PullRequestObservationId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:pull-request-observation",
                    intent.intent_id.as_str(),
                    &observation_hash,
                ])
            )),
            effect_id: intent.effect_id.clone(),
            intent_id: intent.intent_id.clone(),
            attempt_id: intent.attempt.attempt_id.clone(),
            outcome,
            observation_hash,
        };
        observation.validate_against(intent)?;
        Ok(observation)
    }

    pub(crate) fn validate_against(
        &self,
        intent: &PullRequestIntentV1,
    ) -> Result<(), PublicationContractError> {
        let outcome_valid = match &self.outcome {
            PullRequestOutcomeV1::Confirmed {
                pull_request_number,
                pull_request_url,
                node_id_hash,
                base_ref,
                head_branch,
                observed_head,
                execution_marker_hash,
                draft,
                ..
            } => {
                *pull_request_number > 0
                    && pull_request_url_is_valid(pull_request_url)
                    && is_sha256(node_id_hash)
                    && base_ref == &intent.base_ref
                    && head_branch == &intent.head_branch
                    && observed_head == &intent.commit_oid
                    && execution_marker_hash == &intent.execution_marker_hash
                    && draft == &intent.draft
            }
            PullRequestOutcomeV1::Failed { failure } => failure.validate(),
        };
        let expected_hash = Self::expected_hash(intent, &self.outcome)?;
        let expected_id = PullRequestObservationId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:pull-request-observation",
                intent.intent_id.as_str(),
                &expected_hash,
            ])
        ));
        if self.schema_version != PUBLICATION_SCHEMA_VERSION
            || self.effect_id != intent.effect_id
            || self.intent_id != intent.intent_id
            || self.attempt_id != intent.attempt.attempt_id
            || !outcome_valid
            || self.observation_hash != expected_hash
            || self.observation_id != expected_id
        {
            return Err(PublicationContractError::Invalid {
                code: "pull_request_observation_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(
        intent: &PullRequestIntentV1,
        outcome: &PullRequestOutcomeV1,
    ) -> Result<String, PublicationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:pull-request-observation-record",
            &canonical_json(&(
                &intent.effect_id,
                &intent.intent_id,
                &intent.attempt.attempt_id,
                outcome,
            ))?,
        ]))
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PublicationEffectFailureV1 {
    /// The adapter has definitive evidence that this attempt did not apply.
    /// An ambiguous transport result must leave the intent open so replay
    /// reconciles that same effect identity instead of allocating a retry.
    Retryable { safe_code: String },
    /// A definitive rejection for which repeating this intent cannot help.
    Permanent { safe_code: String },
}

impl PublicationEffectFailureV1 {
    fn validate(&self) -> bool {
        match self {
            Self::Retryable { safe_code } | Self::Permanent { safe_code } => {
                safe_code_is_valid(safe_code)
            }
        }
    }

    fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable { .. })
    }

    fn safe_code(&self) -> &str {
        match self {
            Self::Retryable { safe_code } | Self::Permanent { safe_code } => safe_code,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "intent",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum PublicationAttemptIntentV1 {
    Commit(CommitIntentV1),
    Push(ExactLeasePushIntentV1),
    PullRequest(PullRequestIntentV1),
}

impl PublicationAttemptIntentV1 {
    fn attempt(&self) -> &PublicationAttemptV1 {
        match self {
            Self::Commit(intent) => &intent.attempt,
            Self::Push(intent) => &intent.attempt,
            Self::PullRequest(intent) => &intent.attempt,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "observation",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum PublicationAttemptObservationV1 {
    Commit(CommitObservationV1),
    Push(ExactLeasePushObservationV1),
    PullRequest(PullRequestObservationV1),
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(
    tag = "operation",
    content = "observation_id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum PublicationObservationIdV1 {
    Commit(CommitObservationId),
    Push(PushObservationId),
    PullRequest(PullRequestObservationId),
}

impl PublicationAttemptObservationV1 {
    fn exact_identity_and_hash(&self) -> (PublicationObservationIdV1, &str) {
        match self {
            Self::Commit(observation) => (
                PublicationObservationIdV1::Commit(observation.observation_id.clone()),
                &observation.observation_hash,
            ),
            Self::Push(observation) => (
                PublicationObservationIdV1::Push(observation.observation_id.clone()),
                &observation.observation_hash,
            ),
            Self::PullRequest(observation) => (
                PublicationObservationIdV1::PullRequest(observation.observation_id.clone()),
                &observation.observation_hash,
            ),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationAttemptRecordV1 {
    pub(crate) attempt: PublicationAttemptV1,
    pub(crate) intent: PublicationAttemptIntentV1,
    pub(crate) observation: Option<PublicationAttemptObservationV1>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationCompletionV1 {
    pub(crate) schema_version: u16,
    pub(crate) completion_id: PublicationCompletionId,
    pub(crate) eligibility_id: PublicationEligibilityId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) requested_mode: PublicationModeV1,
    pub(crate) commit_observation_id: CommitObservationId,
    pub(crate) push_observation_id: PushObservationId,
    pub(crate) pull_request_observation_id: PullRequestObservationId,
    pub(crate) commit_oid: String,
    pub(crate) head_branch: String,
    pub(crate) pull_request_number: u64,
    pub(crate) pull_request_url: String,
    pub(crate) draft: bool,
    pub(crate) completion_hash: String,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PublicationConvergenceReasonV1 {
    AttemptsExhausted {
        operation: PublicationOperationV1,
    },
    PermanentFailure {
        operation: PublicationOperationV1,
        safe_code: String,
    },
    RemoteBranchMoved {
        movement_id: RemoteBranchMovementId,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationConvergenceV1 {
    pub(crate) schema_version: u16,
    pub(crate) convergence_id: PublicationConvergenceId,
    pub(crate) eligibility_id: PublicationEligibilityId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) final_attempt_id: PublicationAttemptId,
    pub(crate) final_observation_id: PublicationObservationIdV1,
    pub(crate) final_observation_hash: String,
    pub(crate) reason: PublicationConvergenceReasonV1,
    pub(crate) convergence_hash: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum PublicationEvent {
    CommitIntentPersisted {
        intent: CommitIntentV1,
    },
    CommitObserved {
        observation: CommitObservationV1,
    },
    PushIntentPersisted {
        intent: ExactLeasePushIntentV1,
    },
    PushObserved {
        observation: ExactLeasePushObservationV1,
    },
    PullRequestIntentPersisted {
        intent: PullRequestIntentV1,
    },
    PullRequestObserved {
        observation: PullRequestObservationV1,
    },
    CompletionRecorded {
        completion: PublicationCompletionV1,
    },
    ConvergenceEvaluated {
        convergence: PublicationConvergenceV1,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PublicationEffectRequest {
    CreateCommit {
        intent: CommitIntentV1,
    },
    PushExactLease {
        intent: ExactLeasePushIntentV1,
    },
    EnsurePullRequest {
        intent: PullRequestIntentV1,
        material: RawPullRequestMaterialV1,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationStateV1 {
    pub(crate) schema_version: u16,
    pub(crate) execution_id: ExecutionId,
    pub(crate) publication_node_id: NodeId,
    pub(crate) contract_id: EvidenceId,
    pub(crate) eligibility_id: PublicationEligibilityId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) requested_mode: PublicationModeV1,
    pub(crate) attempts: Vec<PublicationAttemptRecordV1>,
    pub(crate) completion: Option<PublicationCompletionV1>,
    pub(crate) convergence: Option<PublicationConvergenceV1>,
}

impl PublicationStateV1 {
    pub(crate) fn new(
        execution_id: ExecutionId,
        publication_node_id: NodeId,
        contract: &PublicationContractV1,
        eligibility: &PublicationEligibilityRecord,
    ) -> Result<Self, PublicationContractError> {
        contract.validate()?;
        eligibility.validate_for_publication(contract, &eligibility.repository_revision)?;
        if execution_id.as_str().trim().is_empty() || publication_node_id.as_str().trim().is_empty()
        {
            return Err(PublicationContractError::Invalid {
                code: "publication_state_identity_invalid",
            });
        }
        Ok(Self {
            schema_version: PUBLICATION_SCHEMA_VERSION,
            execution_id,
            publication_node_id,
            contract_id: contract.contract_id.clone(),
            eligibility_id: eligibility.eligibility_id.clone(),
            repository_revision: eligibility.repository_revision.clone(),
            requested_mode: contract.requested_mode,
            attempts: Vec::new(),
            completion: None,
            convergence: None,
        })
    }

    pub(crate) fn prepare_commit_intent(
        &self,
        contract: &PublicationContractV1,
        eligibility: &PublicationEligibilityRecord,
        tree: CommitTreeBindingV1,
    ) -> Result<CommitIntentV1, PublicationContractError> {
        self.validate_prerequisites(contract, eligibility)?;
        tree.validate_against(eligibility)?;
        if let Some(expected_parent) = &contract.expected_remote_head
            && expected_parent != &tree.parent_commit_oid
        {
            return Err(PublicationContractError::Invalid {
                code: "commit_parent_exact_lease_mismatch",
            });
        }
        let attempt = self.next_attempt(PublicationOperationV1::Commit, contract)?;
        let intent = CommitIntentV1::new(attempt, contract, eligibility, tree)?;
        if let Some(prior) = self.last_commit_intent()
            && (prior.commit_identity_hash != intent.commit_identity_hash
                || prior.tree != intent.tree)
        {
            return Err(PublicationContractError::Invalid {
                code: "commit_retry_identity_changed",
            });
        }
        Ok(intent)
    }

    pub(crate) fn prepare_push_intent(
        &self,
        contract: &PublicationContractV1,
        eligibility: &PublicationEligibilityRecord,
    ) -> Result<ExactLeasePushIntentV1, PublicationContractError> {
        self.validate_prerequisites(contract, eligibility)?;
        let commit_oid = self
            .confirmed_commit_oid()
            .ok_or(PublicationContractError::Invalid {
                code: "push_requires_confirmed_commit",
            })?;
        let attempt = self.next_attempt(PublicationOperationV1::Push, contract)?;
        ExactLeasePushIntentV1::new(attempt, contract, commit_oid.to_owned())
    }

    pub(crate) fn prepare_pull_request_intent(
        &self,
        contract: &PublicationContractV1,
        eligibility: &PublicationEligibilityRecord,
        material: &RawPullRequestMaterialV1,
    ) -> Result<PullRequestIntentV1, PublicationContractError> {
        self.validate_prerequisites(contract, eligibility)?;
        let commit_oid = self
            .confirmed_remote_head()
            .ok_or(PublicationContractError::Invalid {
                code: "pull_request_requires_confirmed_push",
            })?;
        let attempt = self.next_attempt(PublicationOperationV1::PullRequest, contract)?;
        let intent = PullRequestIntentV1::new(
            attempt,
            contract,
            commit_oid.to_owned(),
            material,
            self.pull_request_execution_marker_hash(),
        )?;
        if let Some(prior) = self.last_pull_request_intent()
            && (prior.title_hash != intent.title_hash
                || prior.title_bytes != intent.title_bytes
                || prior.body_hash != intent.body_hash
                || prior.body_bytes != intent.body_bytes
                || prior.draft != intent.draft
                || prior.execution_marker_hash != intent.execution_marker_hash)
        {
            return Err(PublicationContractError::Invalid {
                code: "pull_request_retry_material_changed",
            });
        }
        Ok(intent)
    }

    pub(crate) fn apply(
        &mut self,
        event: &PublicationEvent,
        contract: &PublicationContractV1,
        eligibility: &PublicationEligibilityRecord,
    ) -> Result<(), PublicationContractError> {
        self.validate_prerequisites(contract, eligibility)?;
        if self.completion.is_some() || self.convergence.is_some() {
            return Err(PublicationContractError::Invalid {
                code: "publication_event_after_terminal_record",
            });
        }
        match event {
            PublicationEvent::CommitIntentPersisted { intent } => {
                intent.validate_against(contract, eligibility)?;
                self.persist_intent(PublicationAttemptIntentV1::Commit(intent.clone()), contract)?;
            }
            PublicationEvent::CommitObserved { observation } => {
                let intent = match self.open_intent() {
                    Some(PublicationAttemptIntentV1::Commit(intent)) => intent,
                    _ => {
                        return Err(PublicationContractError::Invalid {
                            code: "commit_observation_without_persisted_intent",
                        });
                    }
                };
                observation.validate_against(intent)?;
                self.record_observation(PublicationAttemptObservationV1::Commit(
                    observation.clone(),
                ))?;
            }
            PublicationEvent::PushIntentPersisted { intent } => {
                intent.validate_against(contract)?;
                self.persist_intent(PublicationAttemptIntentV1::Push(intent.clone()), contract)?;
            }
            PublicationEvent::PushObserved { observation } => {
                let intent = match self.open_intent() {
                    Some(PublicationAttemptIntentV1::Push(intent)) => intent,
                    _ => {
                        return Err(PublicationContractError::Invalid {
                            code: "push_observation_without_persisted_intent",
                        });
                    }
                };
                observation.validate_against(intent)?;
                self.record_observation(PublicationAttemptObservationV1::Push(
                    observation.clone(),
                ))?;
            }
            PublicationEvent::PullRequestIntentPersisted { intent } => {
                intent.validate_against(contract)?;
                self.persist_intent(
                    PublicationAttemptIntentV1::PullRequest(intent.clone()),
                    contract,
                )?;
            }
            PublicationEvent::PullRequestObserved { observation } => {
                let intent = match self.open_intent() {
                    Some(PublicationAttemptIntentV1::PullRequest(intent)) => intent,
                    _ => {
                        return Err(PublicationContractError::Invalid {
                            code: "pull_request_observation_without_persisted_intent",
                        });
                    }
                };
                observation.validate_against(intent)?;
                self.record_observation(PublicationAttemptObservationV1::PullRequest(
                    observation.clone(),
                ))?;
            }
            PublicationEvent::CompletionRecorded { completion } => {
                let expected = self.build_completion(contract)?;
                if completion != &expected {
                    return Err(PublicationContractError::Invalid {
                        code: "publication_completion_not_canonical",
                    });
                }
                self.completion = Some(completion.clone());
            }
            PublicationEvent::ConvergenceEvaluated { convergence } => {
                let expected =
                    self.build_convergence(contract)?
                        .ok_or(PublicationContractError::Invalid {
                            code: "publication_convergence_not_available",
                        })?;
                if convergence != &expected {
                    return Err(PublicationContractError::Invalid {
                        code: "publication_convergence_not_canonical",
                    });
                }
                self.convergence = Some(convergence.clone());
            }
        }
        Ok(())
    }

    pub(crate) fn pending_effect(
        &self,
        raw_pull_request: Option<RawPullRequestMaterialV1>,
    ) -> Result<Option<PublicationEffectRequest>, PublicationContractError> {
        let Some(intent) = self.open_intent() else {
            if raw_pull_request.is_some() {
                return Err(PublicationContractError::Invalid {
                    code: "pull_request_material_without_open_intent",
                });
            }
            return Ok(None);
        };
        match intent {
            PublicationAttemptIntentV1::Commit(intent) => {
                if raw_pull_request.is_some() {
                    return Err(PublicationContractError::Invalid {
                        code: "pull_request_material_for_commit_effect",
                    });
                }
                Ok(Some(PublicationEffectRequest::CreateCommit {
                    intent: intent.clone(),
                }))
            }
            PublicationAttemptIntentV1::Push(intent) => {
                if raw_pull_request.is_some() {
                    return Err(PublicationContractError::Invalid {
                        code: "pull_request_material_for_push_effect",
                    });
                }
                Ok(Some(PublicationEffectRequest::PushExactLease {
                    intent: intent.clone(),
                }))
            }
            PublicationAttemptIntentV1::PullRequest(intent) => {
                let material = raw_pull_request.ok_or(PublicationContractError::Invalid {
                    code: "pull_request_material_missing",
                })?;
                material.validate_against(intent)?;
                Ok(Some(PublicationEffectRequest::EnsurePullRequest {
                    intent: intent.clone(),
                    material,
                }))
            }
        }
    }

    pub(crate) fn build_completion(
        &self,
        contract: &PublicationContractV1,
    ) -> Result<PublicationCompletionV1, PublicationContractError> {
        if self.open_intent().is_some() || self.convergence.is_some() {
            return Err(PublicationContractError::Invalid {
                code: "publication_completion_not_ready",
            });
        }
        let completion = self.build_completion_unchecked(contract)?;
        completion.validate_against(self, contract)?;
        Ok(completion)
    }

    pub(crate) fn build_convergence(
        &self,
        contract: &PublicationContractV1,
    ) -> Result<Option<PublicationConvergenceV1>, PublicationContractError> {
        if self.open_intent().is_some() || self.completion.is_some() {
            return Ok(None);
        }
        let Some(last) = self.attempts.last() else {
            return Ok(None);
        };
        let Some(final_observation) = last.observation.as_ref() else {
            return Ok(None);
        };
        let (final_observation_id, final_observation_hash) =
            final_observation.exact_identity_and_hash();
        if !is_sha256(final_observation_hash) {
            return Err(PublicationContractError::Invalid {
                code: "publication_final_observation_hash_invalid",
            });
        }
        let final_observation_hash = final_observation_hash.to_owned();
        if let Some(failure) = publication_attempt_failure(last) {
            if !failure.validate() {
                return Err(PublicationContractError::Invalid {
                    code: "publication_failure_invalid",
                });
            }
            if failure.is_retryable()
                && last.attempt.operation_attempt
                    < operation_attempt_limit(contract, last.attempt.operation)
            {
                return Ok(None);
            }
        }
        let reason = match final_observation {
            PublicationAttemptObservationV1::Commit(observation) => match &observation.outcome {
                CommitOutcomeV1::Failed { failure } => {
                    convergence_reason(failure, PublicationOperationV1::Commit, last, contract)?
                }
                CommitOutcomeV1::Confirmed { .. } => return Ok(None),
            },
            PublicationAttemptObservationV1::Push(observation) => match &observation.outcome {
                ExactLeasePushOutcomeV1::Failed { failure } => {
                    convergence_reason(failure, PublicationOperationV1::Push, last, contract)?
                }
                ExactLeasePushOutcomeV1::RemoteBranchMoved { movement } => {
                    PublicationConvergenceReasonV1::RemoteBranchMoved {
                        movement_id: movement.movement_id.clone(),
                    }
                }
                ExactLeasePushOutcomeV1::Confirmed { .. } => return Ok(None),
            },
            PublicationAttemptObservationV1::PullRequest(observation) => {
                match &observation.outcome {
                    PullRequestOutcomeV1::Failed { failure } => convergence_reason(
                        failure,
                        PublicationOperationV1::PullRequest,
                        last,
                        contract,
                    )?,
                    PullRequestOutcomeV1::Confirmed { .. } => return Ok(None),
                }
            }
        };
        let convergence_hash = PublicationConvergenceV1::expected_hash(
            &self.eligibility_id,
            &self.repository_revision,
            &last.attempt.attempt_id,
            &final_observation_id,
            &final_observation_hash,
            &reason,
        )?;
        Ok(Some(PublicationConvergenceV1 {
            schema_version: PUBLICATION_SCHEMA_VERSION,
            convergence_id: PublicationConvergenceV1::expected_id(
                &self.eligibility_id,
                &final_observation_id,
                &final_observation_hash,
                &convergence_hash,
            )?,
            eligibility_id: self.eligibility_id.clone(),
            repository_revision: self.repository_revision.clone(),
            final_attempt_id: last.attempt.attempt_id.clone(),
            final_observation_id,
            final_observation_hash,
            reason,
            convergence_hash,
        }))
    }

    pub(crate) fn validate(
        &self,
        contract: &PublicationContractV1,
        eligibility: &PublicationEligibilityRecord,
    ) -> Result<(), PublicationContractError> {
        self.validate_prerequisites(contract, eligibility)?;
        let mut replay = Self::new(
            self.execution_id.clone(),
            self.publication_node_id.clone(),
            contract,
            eligibility,
        )?;
        for record in &self.attempts {
            let intent_event = match &record.intent {
                PublicationAttemptIntentV1::Commit(intent) => {
                    PublicationEvent::CommitIntentPersisted {
                        intent: intent.clone(),
                    }
                }
                PublicationAttemptIntentV1::Push(intent) => PublicationEvent::PushIntentPersisted {
                    intent: intent.clone(),
                },
                PublicationAttemptIntentV1::PullRequest(intent) => {
                    PublicationEvent::PullRequestIntentPersisted {
                        intent: intent.clone(),
                    }
                }
            };
            replay.apply(&intent_event, contract, eligibility)?;
            if let Some(observation) = &record.observation {
                let observation_event = match observation {
                    PublicationAttemptObservationV1::Commit(observation) => {
                        PublicationEvent::CommitObserved {
                            observation: observation.clone(),
                        }
                    }
                    PublicationAttemptObservationV1::Push(observation) => {
                        PublicationEvent::PushObserved {
                            observation: observation.clone(),
                        }
                    }
                    PublicationAttemptObservationV1::PullRequest(observation) => {
                        PublicationEvent::PullRequestObserved {
                            observation: observation.clone(),
                        }
                    }
                };
                replay.apply(&observation_event, contract, eligibility)?;
            }
        }
        if let Some(completion) = &self.completion {
            replay.apply(
                &PublicationEvent::CompletionRecorded {
                    completion: completion.clone(),
                },
                contract,
                eligibility,
            )?;
        }
        if let Some(convergence) = &self.convergence {
            replay.apply(
                &PublicationEvent::ConvergenceEvaluated {
                    convergence: convergence.clone(),
                },
                contract,
                eligibility,
            )?;
        }
        if &replay != self {
            return Err(PublicationContractError::Invalid {
                code: "publication_state_replay_mismatch",
            });
        }
        Ok(())
    }

    fn validate_prerequisites(
        &self,
        contract: &PublicationContractV1,
        eligibility: &PublicationEligibilityRecord,
    ) -> Result<(), PublicationContractError> {
        contract.validate()?;
        eligibility.validate_for_publication(contract, &self.repository_revision)?;
        if self.schema_version != PUBLICATION_SCHEMA_VERSION
            || self.execution_id.as_str().trim().is_empty()
            || self.publication_node_id.as_str().trim().is_empty()
            || self.contract_id != contract.contract_id
            || self.eligibility_id != eligibility.eligibility_id
            || self.repository_revision != eligibility.repository_revision
            || self.requested_mode != contract.requested_mode
            || (self.completion.is_some() && self.convergence.is_some())
        {
            return Err(PublicationContractError::Invalid {
                code: "publication_state_prerequisite_mismatch",
            });
        }
        Ok(())
    }

    fn persist_intent(
        &mut self,
        intent: PublicationAttemptIntentV1,
        contract: &PublicationContractV1,
    ) -> Result<(), PublicationContractError> {
        if self.open_intent().is_some() {
            return Err(PublicationContractError::Invalid {
                code: "publication_intent_already_open",
            });
        }
        let operation = intent.attempt().operation;
        let expected = self.next_attempt(operation, contract)?;
        if intent.attempt() != &expected {
            return Err(PublicationContractError::Invalid {
                code: "publication_intent_attempt_not_next",
            });
        }
        match &intent {
            PublicationAttemptIntentV1::Commit(commit) => {
                if let Some(prior) = self.last_commit_intent()
                    && (prior.commit_identity_hash != commit.commit_identity_hash
                        || prior.tree != commit.tree)
                {
                    return Err(PublicationContractError::Invalid {
                        code: "commit_retry_identity_changed",
                    });
                }
            }
            PublicationAttemptIntentV1::Push(push) => {
                if self.confirmed_commit_oid() != Some(push.commit_oid.as_str()) {
                    return Err(PublicationContractError::Invalid {
                        code: "push_intent_commit_binding_mismatch",
                    });
                }
            }
            PublicationAttemptIntentV1::PullRequest(pull_request) => {
                if self.confirmed_remote_head() != Some(pull_request.commit_oid.as_str()) {
                    return Err(PublicationContractError::Invalid {
                        code: "pull_request_intent_push_binding_mismatch",
                    });
                }
                if pull_request.execution_marker_hash != self.pull_request_execution_marker_hash() {
                    return Err(PublicationContractError::Invalid {
                        code: "pull_request_execution_marker_mismatch",
                    });
                }
                if let Some(prior) = self.last_pull_request_intent()
                    && (prior.title_hash != pull_request.title_hash
                        || prior.title_bytes != pull_request.title_bytes
                        || prior.body_hash != pull_request.body_hash
                        || prior.body_bytes != pull_request.body_bytes
                        || prior.draft != pull_request.draft
                        || prior.execution_marker_hash != pull_request.execution_marker_hash)
                {
                    return Err(PublicationContractError::Invalid {
                        code: "pull_request_retry_identity_changed",
                    });
                }
            }
        }
        self.attempts.push(PublicationAttemptRecordV1 {
            attempt: expected,
            intent,
            observation: None,
        });
        Ok(())
    }

    fn record_observation(
        &mut self,
        observation: PublicationAttemptObservationV1,
    ) -> Result<(), PublicationContractError> {
        let record = self
            .attempts
            .last_mut()
            .ok_or(PublicationContractError::Invalid {
                code: "publication_observation_without_attempt",
            })?;
        if record.observation.is_some() || !observation_matches_intent(&observation, &record.intent)
        {
            return Err(PublicationContractError::Invalid {
                code: "publication_observation_not_next",
            });
        }
        record.observation = Some(observation);
        Ok(())
    }

    fn next_attempt(
        &self,
        requested_operation: PublicationOperationV1,
        contract: &PublicationContractV1,
    ) -> Result<PublicationAttemptV1, PublicationContractError> {
        if self.completion.is_some() || self.convergence.is_some() || self.open_intent().is_some() {
            return Err(PublicationContractError::Invalid {
                code: "publication_attempt_not_available",
            });
        }
        let expected_operation =
            self.expected_next_operation(contract)?
                .ok_or(PublicationContractError::Invalid {
                    code: "publication_has_no_next_operation",
                })?;
        if requested_operation != expected_operation {
            return Err(PublicationContractError::Invalid {
                code: "publication_operation_out_of_sequence",
            });
        }
        let operation_attempt = u32::try_from(
            self.attempts
                .iter()
                .filter(|record| record.attempt.operation == requested_operation)
                .count(),
        )
        .unwrap_or(u32::MAX)
        .saturating_add(1);
        let maximum = operation_attempt_limit(contract, requested_operation);
        if operation_attempt > maximum {
            return Err(PublicationContractError::Invalid {
                code: "publication_operation_attempt_budget_exhausted",
            });
        }
        let sequence = u32::try_from(self.attempts.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        PublicationAttemptV1::new(
            sequence,
            requested_operation,
            operation_attempt,
            self.attempts
                .last()
                .map(|record| record.attempt.attempt_id.clone()),
            self.repository_revision.clone(),
            self.eligibility_id.clone(),
        )
    }

    fn expected_next_operation(
        &self,
        contract: &PublicationContractV1,
    ) -> Result<Option<PublicationOperationV1>, PublicationContractError> {
        let Some(last) = self.attempts.last() else {
            return Ok(Some(PublicationOperationV1::Commit));
        };
        let Some(observation) = &last.observation else {
            return Ok(None);
        };
        let next = match observation {
            PublicationAttemptObservationV1::Commit(observation) => match &observation.outcome {
                CommitOutcomeV1::Confirmed { .. } => Some(PublicationOperationV1::Push),
                CommitOutcomeV1::Failed { failure }
                    if failure.is_retryable()
                        && last.attempt.operation_attempt
                            < operation_attempt_limit(contract, PublicationOperationV1::Commit) =>
                {
                    Some(PublicationOperationV1::Commit)
                }
                CommitOutcomeV1::Failed { .. } => None,
            },
            PublicationAttemptObservationV1::Push(observation) => match &observation.outcome {
                ExactLeasePushOutcomeV1::Confirmed { .. } => {
                    Some(PublicationOperationV1::PullRequest)
                }
                ExactLeasePushOutcomeV1::Failed { failure }
                    if failure.is_retryable()
                        && last.attempt.operation_attempt
                            < operation_attempt_limit(contract, PublicationOperationV1::Push) =>
                {
                    Some(PublicationOperationV1::Push)
                }
                ExactLeasePushOutcomeV1::RemoteBranchMoved { .. }
                | ExactLeasePushOutcomeV1::Failed { .. } => None,
            },
            PublicationAttemptObservationV1::PullRequest(observation) => match &observation.outcome
            {
                PullRequestOutcomeV1::Confirmed { .. } => None,
                PullRequestOutcomeV1::Failed { failure }
                    if failure.is_retryable()
                        && last.attempt.operation_attempt
                            < operation_attempt_limit(
                                contract,
                                PublicationOperationV1::PullRequest,
                            ) =>
                {
                    Some(PublicationOperationV1::PullRequest)
                }
                PullRequestOutcomeV1::Failed { .. } => None,
            },
        };
        Ok(next)
    }

    fn open_intent(&self) -> Option<&PublicationAttemptIntentV1> {
        self.attempts
            .last()
            .filter(|record| record.observation.is_none())
            .map(|record| &record.intent)
    }

    fn confirmed_commit(&self) -> Option<(&CommitObservationV1, &str)> {
        self.attempts
            .iter()
            .rev()
            .find_map(|record| match &record.observation {
                Some(PublicationAttemptObservationV1::Commit(observation)) => observation
                    .confirmed_commit_oid()
                    .map(|commit_oid| (observation, commit_oid)),
                _ => None,
            })
    }

    fn confirmed_commit_oid(&self) -> Option<&str> {
        self.confirmed_commit().map(|(_, commit_oid)| commit_oid)
    }

    fn confirmed_push(&self) -> Option<&ExactLeasePushObservationV1> {
        self.attempts
            .iter()
            .rev()
            .find_map(|record| match &record.observation {
                Some(PublicationAttemptObservationV1::Push(observation))
                    if observation.confirmed_remote_head().is_some() =>
                {
                    Some(observation)
                }
                _ => None,
            })
    }

    fn confirmed_remote_head(&self) -> Option<&str> {
        self.confirmed_push()
            .and_then(ExactLeasePushObservationV1::confirmed_remote_head)
    }

    fn confirmed_pull_request(&self) -> Option<&PullRequestObservationV1> {
        self.attempts
            .iter()
            .rev()
            .find_map(|record| match &record.observation {
                Some(PublicationAttemptObservationV1::PullRequest(observation))
                    if matches!(observation.outcome, PullRequestOutcomeV1::Confirmed { .. }) =>
                {
                    Some(observation)
                }
                _ => None,
            })
    }

    fn last_pull_request_intent(&self) -> Option<&PullRequestIntentV1> {
        self.attempts
            .iter()
            .rev()
            .find_map(|record| match &record.intent {
                PublicationAttemptIntentV1::PullRequest(intent) => Some(intent),
                PublicationAttemptIntentV1::Commit(_) | PublicationAttemptIntentV1::Push(_) => None,
            })
    }

    fn last_commit_intent(&self) -> Option<&CommitIntentV1> {
        self.attempts
            .iter()
            .rev()
            .find_map(|record| match &record.intent {
                PublicationAttemptIntentV1::Commit(intent) => Some(intent),
                PublicationAttemptIntentV1::Push(_)
                | PublicationAttemptIntentV1::PullRequest(_) => None,
            })
    }

    pub(crate) fn pull_request_execution_marker_hash(&self) -> String {
        stable_sha256(&[
            "execution-protocol-v1:pull-request-execution-marker",
            self.execution_id.as_str(),
            self.eligibility_id.as_str(),
        ])
    }
}

impl PublicationCompletionV1 {
    #[allow(clippy::too_many_arguments)]
    fn expected_hash(
        eligibility_id: &PublicationEligibilityId,
        repository_revision: &RepositoryRevisionId,
        requested_mode: PublicationModeV1,
        commit_observation_id: &CommitObservationId,
        push_observation_id: &PushObservationId,
        pull_request_observation_id: &PullRequestObservationId,
        commit_oid: &str,
        head_branch: &str,
        pull_request_number: u64,
        pull_request_url: &str,
        draft: bool,
    ) -> Result<String, PublicationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:publication-completion-record",
            &canonical_json(&(
                eligibility_id,
                repository_revision,
                requested_mode,
                commit_observation_id,
                push_observation_id,
                pull_request_observation_id,
                commit_oid,
                head_branch,
                pull_request_number,
                pull_request_url,
                draft,
            ))?,
        ]))
    }

    pub(crate) fn validate_against(
        &self,
        state: &PublicationStateV1,
        contract: &PublicationContractV1,
    ) -> Result<(), PublicationContractError> {
        let expected = state.build_completion_unchecked(contract)?;
        if self != &expected {
            return Err(PublicationContractError::Invalid {
                code: "publication_completion_invalid",
            });
        }
        Ok(())
    }
}

impl PublicationStateV1 {
    fn build_completion_unchecked(
        &self,
        contract: &PublicationContractV1,
    ) -> Result<PublicationCompletionV1, PublicationContractError> {
        let (commit_observation, commit_oid) =
            self.confirmed_commit()
                .ok_or(PublicationContractError::Invalid {
                    code: "publication_completion_commit_missing",
                })?;
        let push_observation = self
            .confirmed_push()
            .ok_or(PublicationContractError::Invalid {
                code: "publication_completion_push_missing",
            })?;
        let pull_request_observation =
            self.confirmed_pull_request()
                .ok_or(PublicationContractError::Invalid {
                    code: "publication_completion_pull_request_missing",
                })?;
        let PullRequestOutcomeV1::Confirmed {
            pull_request_number,
            pull_request_url,
            draft,
            ..
        } = &pull_request_observation.outcome
        else {
            return Err(PublicationContractError::Invalid {
                code: "publication_completion_pull_request_not_confirmed",
            });
        };
        let completion_hash = PublicationCompletionV1::expected_hash(
            &self.eligibility_id,
            &self.repository_revision,
            self.requested_mode,
            &commit_observation.observation_id,
            &push_observation.observation_id,
            &pull_request_observation.observation_id,
            commit_oid,
            &contract.head_branch,
            *pull_request_number,
            pull_request_url,
            *draft,
        )?;
        Ok(PublicationCompletionV1 {
            schema_version: PUBLICATION_SCHEMA_VERSION,
            completion_id: PublicationCompletionId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:publication-completion",
                    self.eligibility_id.as_str(),
                    &completion_hash,
                ])
            )),
            eligibility_id: self.eligibility_id.clone(),
            repository_revision: self.repository_revision.clone(),
            requested_mode: self.requested_mode,
            commit_observation_id: commit_observation.observation_id.clone(),
            push_observation_id: push_observation.observation_id.clone(),
            pull_request_observation_id: pull_request_observation.observation_id.clone(),
            commit_oid: commit_oid.to_owned(),
            head_branch: contract.head_branch.clone(),
            pull_request_number: *pull_request_number,
            pull_request_url: pull_request_url.clone(),
            draft: *draft,
            completion_hash,
        })
    }
}

impl PublicationConvergenceV1 {
    fn expected_hash(
        eligibility_id: &PublicationEligibilityId,
        repository_revision: &RepositoryRevisionId,
        final_attempt_id: &PublicationAttemptId,
        final_observation_id: &PublicationObservationIdV1,
        final_observation_hash: &str,
        reason: &PublicationConvergenceReasonV1,
    ) -> Result<String, PublicationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:publication-convergence-record",
            &canonical_json(&(
                eligibility_id,
                repository_revision,
                final_attempt_id,
                final_observation_id,
                final_observation_hash,
                reason,
            ))?,
        ]))
    }

    fn expected_id(
        eligibility_id: &PublicationEligibilityId,
        final_observation_id: &PublicationObservationIdV1,
        final_observation_hash: &str,
        convergence_hash: &str,
    ) -> Result<PublicationConvergenceId, PublicationContractError> {
        Ok(PublicationConvergenceId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:publication-convergence",
                eligibility_id.as_str(),
                &canonical_json(final_observation_id)?,
                final_observation_hash,
                convergence_hash,
            ])
        )))
    }

    pub(crate) fn validate_against(
        &self,
        state: &PublicationStateV1,
        contract: &PublicationContractV1,
    ) -> Result<(), PublicationContractError> {
        let (expected_observation_id, expected_observation_hash) = state
            .attempts
            .last()
            .and_then(|record| record.observation.as_ref())
            .map(PublicationAttemptObservationV1::exact_identity_and_hash)
            .ok_or(PublicationContractError::Invalid {
                code: "publication_convergence_final_observation_missing",
            })?;
        if self.final_observation_id != expected_observation_id
            || self.final_observation_hash != expected_observation_hash
        {
            return Err(PublicationContractError::Invalid {
                code: "publication_convergence_final_observation_mismatch",
            });
        }
        let expected =
            state
                .build_convergence(contract)?
                .ok_or(PublicationContractError::Invalid {
                    code: "publication_convergence_not_available",
                })?;
        if self != &expected {
            return Err(PublicationContractError::Invalid {
                code: "publication_convergence_invalid",
            });
        }
        Ok(())
    }
}

fn observation_matches_intent(
    observation: &PublicationAttemptObservationV1,
    intent: &PublicationAttemptIntentV1,
) -> bool {
    matches!(
        (observation, intent),
        (
            PublicationAttemptObservationV1::Commit(_),
            PublicationAttemptIntentV1::Commit(_)
        ) | (
            PublicationAttemptObservationV1::Push(_),
            PublicationAttemptIntentV1::Push(_)
        ) | (
            PublicationAttemptObservationV1::PullRequest(_),
            PublicationAttemptIntentV1::PullRequest(_)
        )
    )
}

fn convergence_reason(
    failure: &PublicationEffectFailureV1,
    operation: PublicationOperationV1,
    last: &PublicationAttemptRecordV1,
    contract: &PublicationContractV1,
) -> Result<PublicationConvergenceReasonV1, PublicationContractError> {
    if !failure.validate() {
        return Err(PublicationContractError::Invalid {
            code: "publication_failure_invalid",
        });
    }
    debug_assert!(
        !failure.is_retryable()
            || last.attempt.operation_attempt >= operation_attempt_limit(contract, operation)
    );
    Ok(if failure.is_retryable() {
        PublicationConvergenceReasonV1::AttemptsExhausted { operation }
    } else {
        PublicationConvergenceReasonV1::PermanentFailure {
            operation,
            safe_code: failure.safe_code().to_owned(),
        }
    })
}

fn publication_attempt_failure(
    record: &PublicationAttemptRecordV1,
) -> Option<&PublicationEffectFailureV1> {
    match record.observation.as_ref()? {
        PublicationAttemptObservationV1::Commit(CommitObservationV1 {
            outcome: CommitOutcomeV1::Failed { failure },
            ..
        })
        | PublicationAttemptObservationV1::Push(ExactLeasePushObservationV1 {
            outcome: ExactLeasePushOutcomeV1::Failed { failure },
            ..
        })
        | PublicationAttemptObservationV1::PullRequest(PullRequestObservationV1 {
            outcome: PullRequestOutcomeV1::Failed { failure },
            ..
        }) => Some(failure),
        PublicationAttemptObservationV1::Commit(CommitObservationV1 {
            outcome: CommitOutcomeV1::Confirmed { .. },
            ..
        })
        | PublicationAttemptObservationV1::Push(ExactLeasePushObservationV1 {
            outcome:
                ExactLeasePushOutcomeV1::Confirmed { .. }
                | ExactLeasePushOutcomeV1::RemoteBranchMoved { .. },
            ..
        })
        | PublicationAttemptObservationV1::PullRequest(PullRequestObservationV1 {
            outcome: PullRequestOutcomeV1::Confirmed { .. },
            ..
        }) => None,
    }
}

fn operation_attempt_limit(
    contract: &PublicationContractV1,
    operation: PublicationOperationV1,
) -> u32 {
    match operation {
        PublicationOperationV1::Commit => contract.max_commit_attempts,
        PublicationOperationV1::Push => contract.max_push_attempts,
        PublicationOperationV1::PullRequest => contract.max_pr_attempts,
    }
}

fn canonical_json(value: &impl Serialize) -> Result<String, PublicationContractError> {
    serde_json::to_string(value).map_err(|_| PublicationContractError::Serialization)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn git_oid_is_valid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn safe_code_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SAFE_CODE_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
        })
}

fn git_ref_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_GIT_REF_BYTES
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.ends_with('.')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains("//")
        && !value.ends_with(".lock")
        && value.bytes().all(|byte| {
            !byte.is_ascii_control()
                && !byte.is_ascii_whitespace()
                && !matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
}

fn pull_request_url_is_valid(value: &str) -> bool {
    if value.is_empty()
        || value.len() > MAX_PULL_REQUEST_URL_BYTES
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.as_str() == value
}
