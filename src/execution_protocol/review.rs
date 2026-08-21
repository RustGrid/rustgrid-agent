use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use super::{
    AcceptedPlan, ActionId, ContextManifestId, DiscoveryCriterionId, EffectId, EvidenceId,
    ExecutionId, ModelCallAdmission, ModelCallId, NodeId, PlanId, PlanRevisionId, PlannedTargetV1,
    ProfilePath, ProofId, RepositoryRevisionId, ReservationId, TargetId, TargetOperation,
    ValidationExpectationId, stable_sha256,
};

pub(crate) const REVIEW_SCHEMA_VERSION: u16 = 1;

const MAX_CHANGED_PATHS: usize = 4_096;
const MAX_DIFF_PAGES: usize = 128;
const MAX_DIFF_PAGE_BYTES: u64 = 512 * 1024;
const MAX_DIFF_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FINDINGS_PER_PAGE: usize = 128;
const MAX_CRITERIA: usize = 256;
const MAX_SUPPORTING_EVIDENCE: usize = 256;
const MAX_SAFE_CODE_BYTES: usize = 128;
const MAX_GIT_REF_BYTES: usize = 256;
const MAX_UNCONTACTED_RELEASES_PER_BINDING: u32 = 16;
const REVIEW_PROVIDER_FIXED_OVERHEAD_BYTES: u64 = 2_048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReviewContractError {
    Invalid { code: &'static str },
    LimitExceeded { field: &'static str, maximum: usize },
    Serialization,
}

impl ReviewContractError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::Invalid { code } => code,
            Self::LimitExceeded { .. } => "review_contract_limit_exceeded",
            Self::Serialization => "review_contract_serialization_failed",
        }
    }
}

impl fmt::Display for ReviewContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { code } => write!(formatter, "review contract violates `{code}`"),
            Self::LimitExceeded { field, maximum } => {
                write!(formatter, "review field `{field}` exceeds {maximum}")
            }
            Self::Serialization => formatter.write_str("review identity serialization failed"),
        }
    }
}

impl std::error::Error for ReviewContractError {}

macro_rules! review_id {
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

review_id!(FinalizationPolicyId);
review_id!(DiffManifestId);
review_id!(DiffManifestFailureId);
review_id!(DiffPageId);
review_id!(DiffPageReviewId);
review_id!(DiffReviewId);
review_id!(CompletionEvaluationId);
review_id!(PublicationAuthorityId);
review_id!(PublicationAuthorityFailureId);
review_id!(PublicationEligibilityId);
review_id!(ReviewConvergenceId);

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublicationModeV1 {
    Normal,
    NormalWithExternalReview,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExternalReviewKindV1 {
    ManualQa,
    AccessibilityReview,
    VisualReview,
    ProductApproval,
    DeploymentEnvironment,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationContractV1 {
    pub(crate) schema_version: u16,
    pub(crate) contract_id: EvidenceId,
    pub(crate) requested_mode: PublicationModeV1,
    pub(crate) repository_binding_hash: String,
    pub(crate) installation_binding_hash: String,
    pub(crate) base_repository_revision: RepositoryRevisionId,
    pub(crate) base_ref: String,
    pub(crate) head_branch: String,
    pub(crate) expected_remote_head: Option<String>,
    pub(crate) commit_identity_hash: String,
    pub(crate) max_commit_attempts: u32,
    pub(crate) max_push_attempts: u32,
    pub(crate) max_pr_attempts: u32,
}

impl PublicationContractV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        requested_mode: PublicationModeV1,
        repository_binding_hash: String,
        installation_binding_hash: String,
        base_repository_revision: RepositoryRevisionId,
        base_ref: String,
        head_branch: String,
        expected_remote_head: Option<String>,
        commit_identity_hash: String,
        max_commit_attempts: u32,
        max_push_attempts: u32,
        max_pr_attempts: u32,
    ) -> Result<Self, ReviewContractError> {
        let identity = canonical_json(&(
            REVIEW_SCHEMA_VERSION,
            requested_mode,
            &repository_binding_hash,
            &installation_binding_hash,
            &base_repository_revision,
            &base_ref,
            &head_branch,
            &expected_remote_head,
            &commit_identity_hash,
            max_commit_attempts,
            max_push_attempts,
            max_pr_attempts,
        ))?;
        let contract = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            contract_id: EvidenceId::new(format!(
                "epv1:{}",
                stable_sha256(&["execution-protocol-v1:publication-contract", &identity])
            )),
            requested_mode,
            repository_binding_hash,
            installation_binding_hash,
            base_repository_revision,
            base_ref,
            head_branch,
            expected_remote_head,
            commit_identity_hash,
            max_commit_attempts,
            max_push_attempts,
            max_pr_attempts,
        };
        contract.validate()?;
        Ok(contract)
    }

    pub(crate) fn validate(&self) -> Result<(), ReviewContractError> {
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || !is_sha256(&self.repository_binding_hash)
            || !is_sha256(&self.installation_binding_hash)
            || !is_sha256(&self.commit_identity_hash)
            || !git_ref_is_valid(&self.base_ref)
            || !git_ref_is_valid(&self.head_branch)
            || self.base_ref == self.head_branch
            || self
                .expected_remote_head
                .as_ref()
                .is_some_and(|value| !git_oid_is_valid(value))
            || self.max_commit_attempts == 0
            || self.max_push_attempts == 0
            || self.max_pr_attempts == 0
            || self.contract_id != self.expected_id()?
        {
            return Err(ReviewContractError::Invalid {
                code: "publication_contract_invalid",
            });
        }
        Ok(())
    }

    pub(crate) fn expected_id(&self) -> Result<EvidenceId, ReviewContractError> {
        let identity = canonical_json(&(
            self.schema_version,
            self.requested_mode,
            &self.repository_binding_hash,
            &self.installation_binding_hash,
            &self.base_repository_revision,
            &self.base_ref,
            &self.head_branch,
            &self.expected_remote_head,
            &self.commit_identity_hash,
            self.max_commit_attempts,
            self.max_push_attempts,
            self.max_pr_attempts,
        ))?;
        Ok(EvidenceId::new(format!(
            "epv1:{}",
            stable_sha256(&["execution-protocol-v1:publication-contract", &identity])
        )))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FinalizationPolicyV1 {
    pub(crate) schema_version: u16,
    pub(crate) policy_id: FinalizationPolicyId,
    pub(crate) policy_evidence_id: EvidenceId,
    pub(crate) max_changed_paths: u32,
    pub(crate) max_diff_pages: u32,
    pub(crate) max_page_bytes: u64,
    pub(crate) max_total_diff_bytes: u64,
    pub(crate) max_uncontacted_releases_per_binding: u32,
    pub(crate) external_review_criteria: BTreeMap<DiscoveryCriterionId, ExternalReviewKindV1>,
    pub(crate) publication: PublicationContractV1,
}

impl FinalizationPolicyV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        policy_evidence_id: EvidenceId,
        max_changed_paths: u32,
        max_diff_pages: u32,
        max_page_bytes: u64,
        max_total_diff_bytes: u64,
        max_uncontacted_releases_per_binding: u32,
        external_review_criteria: BTreeMap<DiscoveryCriterionId, ExternalReviewKindV1>,
        publication: PublicationContractV1,
    ) -> Result<Self, ReviewContractError> {
        publication.validate()?;
        let identity = canonical_json(&(
            REVIEW_SCHEMA_VERSION,
            &policy_evidence_id,
            max_changed_paths,
            max_diff_pages,
            max_page_bytes,
            max_total_diff_bytes,
            max_uncontacted_releases_per_binding,
            &external_review_criteria,
            &publication,
        ))?;
        let policy = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            policy_id: FinalizationPolicyId::new(format!(
                "epv1:{}",
                stable_sha256(&["execution-protocol-v1:finalization-policy", &identity])
            )),
            policy_evidence_id,
            max_changed_paths,
            max_diff_pages,
            max_page_bytes,
            max_total_diff_bytes,
            max_uncontacted_releases_per_binding,
            external_review_criteria,
            publication,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub(crate) fn validate(&self) -> Result<(), ReviewContractError> {
        self.publication.validate()?;
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || self.max_changed_paths == 0
            || usize::try_from(self.max_changed_paths).unwrap_or(usize::MAX) > MAX_CHANGED_PATHS
            || self.max_diff_pages == 0
            || usize::try_from(self.max_diff_pages).unwrap_or(usize::MAX) > MAX_DIFF_PAGES
            || self.max_page_bytes == 0
            || self.max_page_bytes > MAX_DIFF_PAGE_BYTES
            || self.max_total_diff_bytes == 0
            || self.max_total_diff_bytes > MAX_DIFF_TOTAL_BYTES
            || self.max_page_bytes > self.max_total_diff_bytes
            || self.max_uncontacted_releases_per_binding == 0
            || self.max_uncontacted_releases_per_binding > MAX_UNCONTACTED_RELEASES_PER_BINDING
            || self.external_review_criteria.len() > MAX_CRITERIA
            || self.policy_id != self.expected_id()?
        {
            return Err(ReviewContractError::Invalid {
                code: "finalization_policy_invalid",
            });
        }
        Ok(())
    }

    fn expected_id(&self) -> Result<FinalizationPolicyId, ReviewContractError> {
        let identity = canonical_json(&(
            self.schema_version,
            &self.policy_evidence_id,
            self.max_changed_paths,
            self.max_diff_pages,
            self.max_page_bytes,
            self.max_total_diff_bytes,
            self.max_uncontacted_releases_per_binding,
            &self.external_review_criteria,
            &self.publication,
        ))?;
        Ok(FinalizationPolicyId::new(format!(
            "epv1:{}",
            stable_sha256(&["execution-protocol-v1:finalization-policy", &identity])
        )))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EngineeringAncestryV1 {
    pub(crate) schema_version: u16,
    pub(crate) repository_revision: RepositoryRevisionId,
    /// SHA-256 of the exact repository tree proven by the unique current
    /// `MutationVerified` record selected by the reducer.
    pub(crate) repository_fingerprint: String,
    pub(crate) implementation_barrier_proof_id: ProofId,
    pub(crate) required_validation_proof_id: ProofId,
    /// Canonical root-to-current proof traversal produced by the reducer after
    /// validating every repair/rerun parent. The endpoint proofs are included.
    pub(crate) ordered_revision_proof_ids: Vec<ProofId>,
    pub(crate) ancestry_hash: String,
}

impl EngineeringAncestryV1 {
    pub(crate) fn new(
        repository_revision: RepositoryRevisionId,
        repository_fingerprint: String,
        implementation_barrier_proof_id: ProofId,
        required_validation_proof_id: ProofId,
        ordered_revision_proof_ids: Vec<ProofId>,
    ) -> Result<Self, ReviewContractError> {
        let mut ancestry = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            repository_revision,
            repository_fingerprint,
            implementation_barrier_proof_id,
            required_validation_proof_id,
            ordered_revision_proof_ids,
            ancestry_hash: String::new(),
        };
        ancestry.ancestry_hash = ancestry.expected_hash()?;
        ancestry.validate()?;
        Ok(ancestry)
    }

    pub(crate) fn validate(&self) -> Result<(), ReviewContractError> {
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || !is_sha256(&self.repository_fingerprint)
            || self.ordered_revision_proof_ids.len() < 2
            || self.ordered_revision_proof_ids.first()
                != Some(&self.implementation_barrier_proof_id)
            || self.ordered_revision_proof_ids.last() != Some(&self.required_validation_proof_id)
            || self
                .ordered_revision_proof_ids
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                != self.ordered_revision_proof_ids.len()
            || self.ancestry_hash != self.expected_hash()?
        {
            return Err(ReviewContractError::Invalid {
                code: "engineering_ancestry_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(&self) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:engineering-ancestry",
            &canonical_json(&(
                self.schema_version,
                &self.repository_revision,
                &self.repository_fingerprint,
                &self.implementation_barrier_proof_id,
                &self.required_validation_proof_id,
                &self.ordered_revision_proof_ids,
            ))?,
        ]))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffManifestRequestV1 {
    pub(crate) schema_version: u16,
    pub(crate) effect_id: EffectId,
    pub(crate) review_node_id: NodeId,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) accepted_plan_hash: String,
    pub(crate) policy_id: FinalizationPolicyId,
    pub(crate) repository_binding_hash: String,
    pub(crate) base_ref: String,
    pub(crate) base_repository_revision: RepositoryRevisionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_fingerprint: String,
    pub(crate) required_validation_proof_id: ProofId,
    pub(crate) max_changed_paths: u32,
    pub(crate) max_diff_pages: u32,
    pub(crate) max_page_bytes: u64,
    pub(crate) max_total_diff_bytes: u64,
    pub(crate) request_hash: String,
}

impl DiffManifestRequestV1 {
    pub(crate) fn new(
        review_node_id: NodeId,
        plan: &AcceptedPlan,
        ancestry: &EngineeringAncestryV1,
        policy: &FinalizationPolicyV1,
    ) -> Result<Self, ReviewContractError> {
        ancestry.validate()?;
        policy.validate()?;
        if plan.repository_revision != policy.publication.base_repository_revision {
            return Err(ReviewContractError::Invalid {
                code: "diff_request_base_revision_mismatch",
            });
        }
        let identity = canonical_json(&(
            REVIEW_SCHEMA_VERSION,
            &review_node_id,
            &plan.plan_id,
            &plan.plan_revision_id,
            &accepted_plan_hash(plan)?,
            &policy.policy_id,
            &policy.publication.repository_binding_hash,
            &policy.publication.base_ref,
            &policy.publication.base_repository_revision,
            &ancestry.repository_revision,
            &ancestry.repository_fingerprint,
            &ancestry.required_validation_proof_id,
            policy.max_changed_paths,
            policy.max_diff_pages,
            policy.max_page_bytes,
            policy.max_total_diff_bytes,
        ))?;
        let effect_id = EffectId::new(format!(
            "epv1:{}",
            stable_sha256(&["execution-protocol-v1:build-diff-manifest", &identity])
        ));
        let mut request = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            effect_id,
            review_node_id,
            plan_id: plan.plan_id.clone(),
            plan_revision_id: plan.plan_revision_id.clone(),
            accepted_plan_hash: accepted_plan_hash(plan)?,
            policy_id: policy.policy_id.clone(),
            repository_binding_hash: policy.publication.repository_binding_hash.clone(),
            base_ref: policy.publication.base_ref.clone(),
            base_repository_revision: policy.publication.base_repository_revision.clone(),
            repository_revision: ancestry.repository_revision.clone(),
            repository_fingerprint: ancestry.repository_fingerprint.clone(),
            required_validation_proof_id: ancestry.required_validation_proof_id.clone(),
            max_changed_paths: policy.max_changed_paths,
            max_diff_pages: policy.max_diff_pages,
            max_page_bytes: policy.max_page_bytes,
            max_total_diff_bytes: policy.max_total_diff_bytes,
            request_hash: String::new(),
        };
        request.request_hash = request.expected_hash()?;
        request.validate_against(plan, ancestry, policy)?;
        Ok(request)
    }

    pub(crate) fn validate_against(
        &self,
        plan: &AcceptedPlan,
        ancestry: &EngineeringAncestryV1,
        policy: &FinalizationPolicyV1,
    ) -> Result<(), ReviewContractError> {
        let expected = Self::new_unchecked(self.review_node_id.clone(), plan, ancestry, policy)?;
        if self != &expected {
            return Err(ReviewContractError::Invalid {
                code: "diff_manifest_request_binding_mismatch",
            });
        }
        Ok(())
    }

    fn new_unchecked(
        review_node_id: NodeId,
        plan: &AcceptedPlan,
        ancestry: &EngineeringAncestryV1,
        policy: &FinalizationPolicyV1,
    ) -> Result<Self, ReviewContractError> {
        let identity = canonical_json(&(
            REVIEW_SCHEMA_VERSION,
            &review_node_id,
            &plan.plan_id,
            &plan.plan_revision_id,
            &accepted_plan_hash(plan)?,
            &policy.policy_id,
            &policy.publication.repository_binding_hash,
            &policy.publication.base_ref,
            &policy.publication.base_repository_revision,
            &ancestry.repository_revision,
            &ancestry.repository_fingerprint,
            &ancestry.required_validation_proof_id,
            policy.max_changed_paths,
            policy.max_diff_pages,
            policy.max_page_bytes,
            policy.max_total_diff_bytes,
        ))?;
        let effect_id = EffectId::new(format!(
            "epv1:{}",
            stable_sha256(&["execution-protocol-v1:build-diff-manifest", &identity])
        ));
        let mut request = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            effect_id,
            review_node_id,
            plan_id: plan.plan_id.clone(),
            plan_revision_id: plan.plan_revision_id.clone(),
            accepted_plan_hash: accepted_plan_hash(plan)?,
            policy_id: policy.policy_id.clone(),
            repository_binding_hash: policy.publication.repository_binding_hash.clone(),
            base_ref: policy.publication.base_ref.clone(),
            base_repository_revision: policy.publication.base_repository_revision.clone(),
            repository_revision: ancestry.repository_revision.clone(),
            repository_fingerprint: ancestry.repository_fingerprint.clone(),
            required_validation_proof_id: ancestry.required_validation_proof_id.clone(),
            max_changed_paths: policy.max_changed_paths,
            max_diff_pages: policy.max_diff_pages,
            max_page_bytes: policy.max_page_bytes,
            max_total_diff_bytes: policy.max_total_diff_bytes,
            request_hash: String::new(),
        };
        request.request_hash = request.expected_hash()?;
        Ok(request)
    }

    fn expected_hash(&self) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:diff-manifest-request",
            &canonical_json(&(
                (
                    self.schema_version,
                    &self.effect_id,
                    &self.review_node_id,
                    &self.plan_id,
                    &self.plan_revision_id,
                    &self.accepted_plan_hash,
                    &self.policy_id,
                    &self.repository_binding_hash,
                    &self.base_ref,
                ),
                (
                    &self.base_repository_revision,
                    &self.repository_revision,
                    &self.repository_fingerprint,
                    &self.required_validation_proof_id,
                    self.max_changed_paths,
                    self.max_diff_pages,
                    self.max_page_bytes,
                    self.max_total_diff_bytes,
                ),
            ))?,
        ]))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffManifestLimitV1 {
    ChangedPaths,
    DiffPages,
    PageBytes,
    TotalDiffBytes,
}

impl DiffManifestLimitV1 {
    const fn maximum(self, request: &DiffManifestRequestV1) -> u64 {
        match self {
            Self::ChangedPaths => request.max_changed_paths as u64,
            Self::DiffPages => request.max_diff_pages as u64,
            Self::PageBytes => request.max_page_bytes,
            Self::TotalDiffBytes => request.max_total_diff_bytes,
        }
    }

    const fn safe_code(self) -> &'static str {
        match self {
            Self::ChangedPaths => "diff_changed_paths_limit_exceeded",
            Self::DiffPages => "diff_page_count_limit_exceeded",
            Self::PageBytes => "diff_page_bytes_limit_exceeded",
            Self::TotalDiffBytes => "diff_total_bytes_limit_exceeded",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DiffManifestEffectFailureReasonV1 {
    LimitExceeded {
        limit: DiffManifestLimitV1,
        observed: u64,
    },
    RepositoryDrift {
        observed_revision: RepositoryRevisionId,
        observed_repository_fingerprint: String,
    },
    ArtifactDurabilityFailed {
        safe_code: String,
    },
}

impl DiffManifestEffectFailureReasonV1 {
    fn validate_against(&self, request: &DiffManifestRequestV1) -> Result<(), ReviewContractError> {
        let valid = match self {
            Self::LimitExceeded { limit, observed } => *observed > limit.maximum(request),
            Self::RepositoryDrift {
                observed_revision,
                observed_repository_fingerprint,
            } => {
                is_sha256(observed_repository_fingerprint)
                    && (observed_revision != &request.repository_revision
                        || observed_repository_fingerprint != &request.repository_fingerprint)
            }
            Self::ArtifactDurabilityFailed { safe_code } => safe_code_is_valid(safe_code),
        };
        if !valid {
            return Err(ReviewContractError::Invalid {
                code: "diff_manifest_effect_failure_reason_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffManifestEffectFailureV1 {
    pub(crate) schema_version: u16,
    pub(crate) failure_id: DiffManifestFailureId,
    pub(crate) effect_id: EffectId,
    pub(crate) request_hash: String,
    pub(crate) review_node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_fingerprint: String,
    pub(crate) reason: DiffManifestEffectFailureReasonV1,
    pub(crate) failure_hash: String,
}

impl DiffManifestEffectFailureV1 {
    pub(crate) fn new(
        request: &DiffManifestRequestV1,
        reason: DiffManifestEffectFailureReasonV1,
    ) -> Result<Self, ReviewContractError> {
        reason.validate_against(request)?;
        let failure_hash = Self::expected_hash(request, &reason)?;
        let failure = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            failure_id: DiffManifestFailureId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:diff-manifest-effect-failure",
                    request.effect_id.as_str(),
                    &failure_hash,
                ])
            )),
            effect_id: request.effect_id.clone(),
            request_hash: request.request_hash.clone(),
            review_node_id: request.review_node_id.clone(),
            repository_revision: request.repository_revision.clone(),
            repository_fingerprint: request.repository_fingerprint.clone(),
            reason,
            failure_hash,
        };
        failure.validate_against(request)?;
        Ok(failure)
    }

    pub(crate) fn validate_against(
        &self,
        request: &DiffManifestRequestV1,
    ) -> Result<(), ReviewContractError> {
        self.reason.validate_against(request)?;
        let expected_hash = Self::expected_hash(request, &self.reason)?;
        let expected_id = DiffManifestFailureId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:diff-manifest-effect-failure",
                request.effect_id.as_str(),
                &expected_hash,
            ])
        ));
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || self.effect_id != request.effect_id
            || self.request_hash != request.request_hash
            || self.review_node_id != request.review_node_id
            || self.repository_revision != request.repository_revision
            || self.repository_fingerprint != request.repository_fingerprint
            || self.failure_hash != expected_hash
            || self.failure_id != expected_id
        {
            return Err(ReviewContractError::Invalid {
                code: "diff_manifest_effect_failure_invalid",
            });
        }
        Ok(())
    }

    pub(crate) fn convergence_reason(&self) -> ReviewConvergenceReasonV1 {
        match &self.reason {
            DiffManifestEffectFailureReasonV1::LimitExceeded { limit, .. } => {
                ReviewConvergenceReasonV1::DiffManifestLimitExceeded {
                    failure_id: self.failure_id.clone(),
                    failure_hash: self.failure_hash.clone(),
                    safe_code: limit.safe_code().into(),
                }
            }
            DiffManifestEffectFailureReasonV1::RepositoryDrift {
                observed_revision, ..
            } => ReviewConvergenceReasonV1::RepositoryDrift {
                failure_id: self.failure_id.clone(),
                failure_hash: self.failure_hash.clone(),
                observed_revision: observed_revision.clone(),
            },
            DiffManifestEffectFailureReasonV1::ArtifactDurabilityFailed { safe_code } => {
                ReviewConvergenceReasonV1::ArtifactDurabilityFailed {
                    failure_id: self.failure_id.clone(),
                    failure_hash: self.failure_hash.clone(),
                    safe_code: safe_code.clone(),
                }
            }
        }
    }

    fn expected_hash(
        request: &DiffManifestRequestV1,
        reason: &DiffManifestEffectFailureReasonV1,
    ) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:diff-manifest-effect-failure-record",
            &canonical_json(&(
                REVIEW_SCHEMA_VERSION,
                &request.effect_id,
                &request.request_hash,
                &request.review_node_id,
                &request.repository_revision,
                &request.repository_fingerprint,
                reason,
            ))?,
        ]))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffChangeKindV1 {
    Created,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffPathRecordV1 {
    pub(crate) path: ProfilePath,
    pub(crate) old_path: Option<ProfilePath>,
    pub(crate) change_kind: DiffChangeKindV1,
    pub(crate) old_content_hash: Option<String>,
    pub(crate) new_content_hash: Option<String>,
    pub(crate) old_mode: Option<u32>,
    pub(crate) new_mode: Option<u32>,
    pub(crate) binary: bool,
    pub(crate) patch_hash: String,
    pub(crate) patch_bytes: u64,
}

impl DiffPathRecordV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        path: ProfilePath,
        old_path: Option<ProfilePath>,
        change_kind: DiffChangeKindV1,
        old_content_hash: Option<String>,
        new_content_hash: Option<String>,
        old_mode: Option<u32>,
        new_mode: Option<u32>,
        binary: bool,
        patch_hash: String,
        patch_bytes: u64,
    ) -> Result<Self, ReviewContractError> {
        let record = Self {
            path,
            old_path,
            change_kind,
            old_content_hash,
            new_content_hash,
            old_mode,
            new_mode,
            binary,
            patch_hash,
            patch_bytes,
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn validate(&self) -> Result<(), ReviewContractError> {
        let hashes_valid = self
            .old_content_hash
            .iter()
            .chain(self.new_content_hash.iter())
            .all(|hash| is_sha256(hash));
        let shape_valid = match self.change_kind {
            DiffChangeKindV1::Created => {
                self.old_path.is_none()
                    && self.old_content_hash.is_none()
                    && self.new_content_hash.is_some()
            }
            DiffChangeKindV1::Modified => {
                self.old_path.is_none()
                    && self.old_content_hash.is_some()
                    && self.new_content_hash.is_some()
                    && self.old_content_hash != self.new_content_hash
            }
            DiffChangeKindV1::Deleted => {
                self.old_path.is_none()
                    && self.old_content_hash.is_some()
                    && self.new_content_hash.is_none()
            }
            DiffChangeKindV1::Renamed => {
                self.old_path
                    .as_ref()
                    .is_some_and(|path| path != &self.path)
                    && self.old_content_hash.is_some()
                    && self.new_content_hash.is_some()
            }
        };
        if !hashes_valid
            || !shape_valid
            || !is_sha256(&self.patch_hash)
            || self.patch_bytes == 0
            || self.old_mode.is_some_and(|mode| mode == 0)
            || self.new_mode.is_some_and(|mode| mode == 0)
        {
            return Err(ReviewContractError::Invalid {
                code: "diff_path_record_invalid",
            });
        }
        Ok(())
    }
}

/// Reducer-replayable ownership of every materialized path by the exact
/// accepted plan. Provider review can add findings, but cannot make an unsafe
/// or incomplete plan-to-diff mapping publishable.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffPlanAssessmentV1 {
    pub(crate) schema_version: u16,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) accepted_plan_hash: String,
    pub(crate) target_path_indexes: BTreeMap<TargetId, u32>,
    pub(crate) missing_target_ids: BTreeSet<TargetId>,
    pub(crate) operation_mismatch_target_ids: BTreeSet<TargetId>,
    pub(crate) unplanned_path_indexes: BTreeSet<u32>,
    pub(crate) assessment_hash: String,
}

impl DiffPlanAssessmentV1 {
    pub(crate) fn derive(
        plan: &AcceptedPlan,
        changed_paths: &[DiffPathRecordV1],
    ) -> Result<Self, ReviewContractError> {
        let assessment = Self::derive_unchecked(plan, changed_paths)?;
        assessment.validate_against(plan, changed_paths)?;
        Ok(assessment)
    }

    pub(crate) fn validate_against(
        &self,
        plan: &AcceptedPlan,
        changed_paths: &[DiffPathRecordV1],
    ) -> Result<(), ReviewContractError> {
        let expected = Self::derive_unchecked(plan, changed_paths)?;
        if self != &expected {
            return Err(ReviewContractError::Invalid {
                code: "diff_plan_assessment_invalid",
            });
        }
        Ok(())
    }

    pub(crate) fn is_safe_and_complete(&self) -> bool {
        !self.target_path_indexes.is_empty()
            && self.missing_target_ids.is_empty()
            && self.operation_mismatch_target_ids.is_empty()
            && self.unplanned_path_indexes.is_empty()
    }

    fn derive_unchecked(
        plan: &AcceptedPlan,
        changed_paths: &[DiffPathRecordV1],
    ) -> Result<Self, ReviewContractError> {
        if plan.targets.len() > MAX_CHANGED_PATHS
            || changed_paths.len() > MAX_CHANGED_PATHS
            || plan
                .targets
                .iter()
                .map(|target| &target.target_id)
                .collect::<BTreeSet<_>>()
                .len()
                != plan.targets.len()
        {
            return Err(ReviewContractError::Invalid {
                code: "diff_plan_assessment_input_invalid",
            });
        }

        let exact_candidates = plan
            .targets
            .iter()
            .map(|target| {
                let indexes = changed_paths
                    .iter()
                    .enumerate()
                    .filter_map(|(index, path)| {
                        diff_path_matches_target(path, target)
                            .then_some(u32::try_from(index).unwrap_or(u32::MAX))
                    })
                    .collect::<BTreeSet<_>>();
                (target.target_id.clone(), indexes)
            })
            .collect::<BTreeMap<_, _>>();
        let candidate_owners = exact_candidates
            .values()
            .flat_map(|indexes| indexes.iter().copied())
            .fold(BTreeMap::<u32, u32>::new(), |mut owners, index| {
                *owners.entry(index).or_default() += 1;
                owners
            });

        let mut target_path_indexes = BTreeMap::new();
        let mut missing_target_ids = BTreeSet::new();
        let mut operation_mismatch_target_ids = BTreeSet::new();
        for target in &plan.targets {
            let candidates = exact_candidates
                .get(&target.target_id)
                .expect("candidate set exists for every target");
            let unique = candidates
                .iter()
                .next()
                .copied()
                .filter(|_| candidates.len() == 1)
                .filter(|index| candidate_owners.get(index) == Some(&1));
            if let Some(index) = unique {
                target_path_indexes.insert(target.target_id.clone(), index);
            } else {
                missing_target_ids.insert(target.target_id.clone());
                if !candidates.is_empty()
                    || changed_paths
                        .iter()
                        .any(|path| diff_path_relates_to_target(path, target))
                {
                    operation_mismatch_target_ids.insert(target.target_id.clone());
                }
            }
        }
        let owned_indexes = target_path_indexes
            .values()
            .copied()
            .collect::<BTreeSet<_>>();
        let unplanned_path_indexes = (0..u32::try_from(changed_paths.len()).unwrap_or(u32::MAX))
            .filter(|index| !owned_indexes.contains(index))
            .collect::<BTreeSet<_>>();
        let accepted_plan_hash = accepted_plan_hash(plan)?;
        let assessment_hash = stable_sha256(&[
            "execution-protocol-v1:diff-plan-assessment",
            &canonical_json(&(
                &plan.plan_id,
                &plan.plan_revision_id,
                &accepted_plan_hash,
                &target_path_indexes,
                &missing_target_ids,
                &operation_mismatch_target_ids,
                &unplanned_path_indexes,
            ))?,
        ]);
        Ok(Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            plan_id: plan.plan_id.clone(),
            plan_revision_id: plan.plan_revision_id.clone(),
            accepted_plan_hash,
            target_path_indexes,
            missing_target_ids,
            operation_mismatch_target_ids,
            unplanned_path_indexes,
            assessment_hash,
        })
    }
}

fn diff_path_matches_target(path: &DiffPathRecordV1, target: &PlannedTargetV1) -> bool {
    match &target.operation {
        TargetOperation::ModifyExisting {
            expected_content_hash,
        } => {
            path.path == target.path
                && path.old_path.is_none()
                && path.change_kind == DiffChangeKindV1::Modified
                && path.old_content_hash.as_ref() == Some(expected_content_hash)
        }
        TargetOperation::CreateFile { .. } => {
            path.path == target.path
                && path.old_path.is_none()
                && path.change_kind == DiffChangeKindV1::Created
        }
        TargetOperation::DeleteFile {
            expected_content_hash,
        } => {
            path.path == target.path
                && path.old_path.is_none()
                && path.change_kind == DiffChangeKindV1::Deleted
                && path.old_content_hash.as_ref() == Some(expected_content_hash)
        }
        TargetOperation::MoveFile {
            destination,
            expected_content_hash,
        } => {
            &path.path == destination
                && path.old_path.as_ref() == Some(&target.path)
                && path.change_kind == DiffChangeKindV1::Renamed
                && path.old_content_hash.as_ref() == Some(expected_content_hash)
        }
    }
}

fn diff_path_relates_to_target(path: &DiffPathRecordV1, target: &PlannedTargetV1) -> bool {
    path.path == target.path
        || path.old_path.as_ref() == Some(&target.path)
        || target.operation.destination().is_some_and(|destination| {
            &path.path == destination || path.old_path.as_ref() == Some(destination)
        })
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MaterializedDiffPage {
    pub(crate) index: u32,
    pub(crate) covered_path_indexes: BTreeSet<u32>,
    bytes: Vec<u8>,
}

impl MaterializedDiffPage {
    pub(crate) fn new(
        index: u32,
        covered_path_indexes: BTreeSet<u32>,
        bytes: Vec<u8>,
    ) -> Result<Self, ReviewContractError> {
        if covered_path_indexes.is_empty() || bytes.is_empty() {
            return Err(ReviewContractError::Invalid {
                code: "materialized_diff_page_empty",
            });
        }
        Ok(Self {
            index,
            covered_path_indexes,
            bytes,
        })
    }

    pub(crate) fn content_hash(&self) -> String {
        hex::encode(Sha256::digest(&self.bytes))
    }

    pub(crate) fn byte_len(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for MaterializedDiffPage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaterializedDiffPage")
            .field("index", &self.index)
            .field("covered_path_indexes", &self.covered_path_indexes)
            .field("byte_len", &self.bytes.len())
            .field("content", &"<redacted>")
            .finish()
    }
}

impl Drop for MaterializedDiffPage {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MaterializedDiffManifest {
    pub(crate) effect_id: EffectId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_fingerprint_before: String,
    pub(crate) repository_fingerprint_after: String,
    pub(crate) paths: Vec<DiffPathRecordV1>,
    pub(crate) pages: Vec<MaterializedDiffPage>,
}

impl MaterializedDiffManifest {
    pub(crate) fn new(
        request: &DiffManifestRequestV1,
        repository_revision: RepositoryRevisionId,
        repository_fingerprint_before: String,
        repository_fingerprint_after: String,
        paths: Vec<DiffPathRecordV1>,
        pages: Vec<MaterializedDiffPage>,
    ) -> Result<Self, ReviewContractError> {
        let materialized = Self {
            effect_id: request.effect_id.clone(),
            repository_revision,
            repository_fingerprint_before,
            repository_fingerprint_after,
            paths,
            pages,
        };
        materialized.validate_against(request)?;
        Ok(materialized)
    }

    pub(crate) fn validate_against(
        &self,
        request: &DiffManifestRequestV1,
    ) -> Result<(), ReviewContractError> {
        let path_count = u32::try_from(self.paths.len()).unwrap_or(u32::MAX);
        let page_count = u32::try_from(self.pages.len()).unwrap_or(u32::MAX);
        let total_bytes = self
            .pages
            .iter()
            .try_fold(0_u64, |total, page| total.checked_add(page.byte_len()))
            .ok_or(ReviewContractError::Invalid {
                code: "diff_total_bytes_overflow",
            })?;
        let expected_page_indexes = (0..page_count).collect::<Vec<_>>();
        let actual_page_indexes = self.pages.iter().map(|page| page.index).collect::<Vec<_>>();
        let current_paths_unique = self
            .paths
            .iter()
            .map(|path| &path.path)
            .collect::<BTreeSet<_>>()
            .len()
            == self.paths.len();
        let mut covered = BTreeSet::new();
        let coverage_unique = self.pages.iter().all(|page| {
            page.covered_path_indexes
                .iter()
                .all(|path_index| covered.insert(*path_index))
        });
        let expected_coverage = (0..path_count).collect::<BTreeSet<_>>();
        let pages_match_paths =
            self.paths
                .iter()
                .zip(&self.pages)
                .enumerate()
                .all(|(index, (path, page))| {
                    let index = u32::try_from(index).unwrap_or(u32::MAX);
                    page.index == index
                        && page.covered_path_indexes.len() == 1
                        && page.covered_path_indexes.contains(&index)
                        && page.content_hash() == path.patch_hash
                        && page.byte_len() == path.patch_bytes
                });
        if self.effect_id != request.effect_id
            || self.repository_revision != request.repository_revision
            || self.repository_fingerprint_before != request.repository_fingerprint
            || self.repository_fingerprint_after != request.repository_fingerprint
            || path_count > request.max_changed_paths
            || page_count > request.max_diff_pages
            || self.paths.windows(2).any(|pair| pair[0] >= pair[1])
            || !current_paths_unique
            || self.paths.iter().any(|path| path.validate().is_err())
            || (self.paths.is_empty() != self.pages.is_empty())
            || self.paths.len() != self.pages.len()
            || !pages_match_paths
            || actual_page_indexes != expected_page_indexes
            || !coverage_unique
            || covered != expected_coverage
            || self
                .pages
                .iter()
                .any(|page| page.byte_len() == 0 || page.byte_len() > request.max_page_bytes)
            || total_bytes > request.max_total_diff_bytes
        {
            return Err(ReviewContractError::Invalid {
                code: "materialized_diff_manifest_invalid",
            });
        }
        Ok(())
    }
}

impl fmt::Debug for MaterializedDiffManifest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaterializedDiffManifest")
            .field("effect_id", &self.effect_id)
            .field("repository_revision", &self.repository_revision)
            .field(
                "repository_fingerprint_before",
                &self.repository_fingerprint_before,
            )
            .field(
                "repository_fingerprint_after",
                &self.repository_fingerprint_after,
            )
            .field("path_count", &self.paths.len())
            .field("pages", &self.pages)
            .field("raw_diff", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffPagePersistenceReceiptV1 {
    pub(crate) page_index: u32,
    pub(crate) content_hash: String,
    pub(crate) artifact_locator_hash: String,
    pub(crate) persistence_receipt_hash: String,
    pub(crate) byte_len: u64,
}

impl DiffPagePersistenceReceiptV1 {
    pub(crate) fn validate(&self) -> Result<(), ReviewContractError> {
        if !is_sha256(&self.content_hash)
            || !is_sha256(&self.artifact_locator_hash)
            || !is_sha256(&self.persistence_receipt_hash)
            || self.byte_len == 0
        {
            return Err(ReviewContractError::Invalid {
                code: "diff_page_persistence_receipt_invalid",
            });
        }
        Ok(())
    }
}

/// Non-secret durable address for a page in a content-addressed artifact
/// store. Provider adapters resolve this address through their scoped store
/// port; signed URLs and credentials must never be placed in this value.
#[derive(Clone, Debug, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct DiffArtifactAddressV1(String);

impl DiffArtifactAddressV1 {
    pub(crate) fn for_content_hash(content_hash: &str) -> Result<Self, ReviewContractError> {
        if !is_sha256(content_hash) {
            return Err(ReviewContractError::Invalid {
                code: "diff_artifact_content_hash_invalid",
            });
        }
        Ok(Self(format!("sha256:{content_hash}")))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn validate_against(&self, content_hash: &str) -> Result<(), ReviewContractError> {
        if self != &Self::for_content_hash(content_hash)? {
            return Err(ReviewContractError::Invalid {
                code: "diff_artifact_address_invalid",
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for DiffArtifactAddressV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let content_hash = value.strip_prefix("sha256:").ok_or_else(|| {
            serde::de::Error::custom("diff artifact address must be content-addressed")
        })?;
        Self::for_content_hash(content_hash).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffPageReceiptV1 {
    pub(crate) page_id: DiffPageId,
    pub(crate) index: u32,
    pub(crate) content_hash: String,
    pub(crate) content_address: DiffArtifactAddressV1,
    pub(crate) artifact_locator_hash: String,
    pub(crate) persistence_receipt_hash: String,
    pub(crate) byte_len: u64,
    pub(crate) covered_path_indexes: BTreeSet<u32>,
}

impl DiffPageReceiptV1 {
    fn from_materialized(
        request: &DiffManifestRequestV1,
        page: &MaterializedDiffPage,
        persistence: &DiffPagePersistenceReceiptV1,
    ) -> Result<Self, ReviewContractError> {
        persistence.validate()?;
        if persistence.page_index != page.index
            || persistence.content_hash != page.content_hash()
            || persistence.byte_len != page.byte_len()
        {
            return Err(ReviewContractError::Invalid {
                code: "diff_page_persistence_binding_mismatch",
            });
        }
        let page_id = Self::expected_id(
            &request.effect_id,
            page.index,
            &persistence.content_hash,
            &DiffArtifactAddressV1::for_content_hash(&persistence.content_hash)?,
            &persistence.artifact_locator_hash,
            &persistence.persistence_receipt_hash,
            persistence.byte_len,
            &page.covered_path_indexes,
        )?;
        Ok(Self {
            page_id,
            index: page.index,
            content_hash: persistence.content_hash.clone(),
            content_address: DiffArtifactAddressV1::for_content_hash(&persistence.content_hash)?,
            artifact_locator_hash: persistence.artifact_locator_hash.clone(),
            persistence_receipt_hash: persistence.persistence_receipt_hash.clone(),
            byte_len: persistence.byte_len,
            covered_path_indexes: page.covered_path_indexes.clone(),
        })
    }

    pub(crate) fn validate(&self, effect_id: &EffectId) -> Result<(), ReviewContractError> {
        if !is_sha256(&self.content_hash)
            || self
                .content_address
                .validate_against(&self.content_hash)
                .is_err()
            || !is_sha256(&self.artifact_locator_hash)
            || !is_sha256(&self.persistence_receipt_hash)
            || self.byte_len == 0
            || self.covered_path_indexes.is_empty()
            || self.page_id
                != Self::expected_id(
                    effect_id,
                    self.index,
                    &self.content_hash,
                    &self.content_address,
                    &self.artifact_locator_hash,
                    &self.persistence_receipt_hash,
                    self.byte_len,
                    &self.covered_path_indexes,
                )?
        {
            return Err(ReviewContractError::Invalid {
                code: "diff_page_receipt_invalid",
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn expected_id(
        effect_id: &EffectId,
        index: u32,
        content_hash: &str,
        content_address: &DiffArtifactAddressV1,
        artifact_locator_hash: &str,
        persistence_receipt_hash: &str,
        byte_len: u64,
        covered_path_indexes: &BTreeSet<u32>,
    ) -> Result<DiffPageId, ReviewContractError> {
        Ok(DiffPageId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:diff-page",
                effect_id.as_str(),
                &index.to_string(),
                content_hash,
                content_address.as_str(),
                artifact_locator_hash,
                persistence_receipt_hash,
                &byte_len.to_string(),
                &canonical_json(covered_path_indexes)?,
            ])
        )))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffManifestV1 {
    pub(crate) schema_version: u16,
    pub(crate) manifest_id: DiffManifestId,
    pub(crate) effect_id: EffectId,
    pub(crate) review_node_id: NodeId,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) policy_id: FinalizationPolicyId,
    pub(crate) base_repository_revision: RepositoryRevisionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) required_validation_proof_id: ProofId,
    pub(crate) repository_fingerprint: String,
    pub(crate) changed_paths: Vec<DiffPathRecordV1>,
    pub(crate) plan_assessment: DiffPlanAssessmentV1,
    pub(crate) pages: Vec<DiffPageReceiptV1>,
    pub(crate) total_bytes: u64,
    pub(crate) diff_hash: String,
}

impl DiffManifestV1 {
    pub(crate) fn from_materialized(
        request: &DiffManifestRequestV1,
        plan: &AcceptedPlan,
        materialized: &MaterializedDiffManifest,
        mut persistence: Vec<DiffPagePersistenceReceiptV1>,
    ) -> Result<Self, ReviewContractError> {
        materialized.validate_against(request)?;
        persistence.sort_by_key(|receipt| receipt.page_index);
        if persistence.len() != materialized.pages.len()
            || persistence
                .iter()
                .enumerate()
                .any(|(index, receipt)| receipt.page_index != index as u32)
        {
            return Err(ReviewContractError::Invalid {
                code: "diff_page_persistence_set_invalid",
            });
        }
        let pages = materialized
            .pages
            .iter()
            .zip(&persistence)
            .map(|(page, persistence)| {
                DiffPageReceiptV1::from_materialized(request, page, persistence)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let total_bytes = pages.iter().try_fold(0_u64, |total, page| {
            total
                .checked_add(page.byte_len)
                .ok_or(ReviewContractError::Invalid {
                    code: "diff_total_bytes_overflow",
                })
        })?;
        let plan_assessment = DiffPlanAssessmentV1::derive(plan, &materialized.paths)?;
        let diff_hash = Self::expected_diff_hash(
            request,
            &materialized.repository_fingerprint_after,
            &materialized.paths,
            &plan_assessment,
            &pages,
            total_bytes,
        )?;
        let manifest_id = Self::expected_id(request, &diff_hash)?;
        let manifest = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            manifest_id,
            effect_id: request.effect_id.clone(),
            review_node_id: request.review_node_id.clone(),
            plan_id: request.plan_id.clone(),
            plan_revision_id: request.plan_revision_id.clone(),
            policy_id: request.policy_id.clone(),
            base_repository_revision: request.base_repository_revision.clone(),
            repository_revision: request.repository_revision.clone(),
            required_validation_proof_id: request.required_validation_proof_id.clone(),
            repository_fingerprint: materialized.repository_fingerprint_after.clone(),
            changed_paths: materialized.paths.clone(),
            plan_assessment,
            pages,
            total_bytes,
            diff_hash,
        };
        manifest.validate_against(request, plan)?;
        Ok(manifest)
    }

    pub(crate) fn validate_against(
        &self,
        request: &DiffManifestRequestV1,
        plan: &AcceptedPlan,
    ) -> Result<(), ReviewContractError> {
        let path_count = u32::try_from(self.changed_paths.len()).unwrap_or(u32::MAX);
        let page_count = u32::try_from(self.pages.len()).unwrap_or(u32::MAX);
        let path_identities_unique = self
            .changed_paths
            .iter()
            .map(|path| (&path.path, &path.old_path))
            .collect::<BTreeSet<_>>()
            .len()
            == self.changed_paths.len();
        let current_paths_unique = self
            .changed_paths
            .iter()
            .map(|path| &path.path)
            .collect::<BTreeSet<_>>()
            .len()
            == self.changed_paths.len();
        let page_ids_unique = self
            .pages
            .iter()
            .map(|page| &page.page_id)
            .collect::<BTreeSet<_>>()
            .len()
            == self.pages.len();
        let expected_page_indexes = (0..page_count).collect::<Vec<_>>();
        let actual_page_indexes = self.pages.iter().map(|page| page.index).collect::<Vec<_>>();
        let mut covered = BTreeSet::new();
        let coverage_unique = self.pages.iter().all(|page| {
            page.covered_path_indexes
                .iter()
                .all(|path_index| covered.insert(*path_index))
        });
        let expected_coverage = (0..path_count).collect::<BTreeSet<_>>();
        let pages_match_paths =
            self.changed_paths
                .iter()
                .zip(&self.pages)
                .enumerate()
                .all(|(index, (path, page))| {
                    let index = u32::try_from(index).unwrap_or(u32::MAX);
                    page.index == index
                        && page.covered_path_indexes.len() == 1
                        && page.covered_path_indexes.contains(&index)
                        && page.content_hash == path.patch_hash
                        && page.byte_len == path.patch_bytes
                });
        let computed_total = self.pages.iter().try_fold(0_u64, |total, page| {
            total
                .checked_add(page.byte_len)
                .ok_or(ReviewContractError::Invalid {
                    code: "diff_total_bytes_overflow",
                })
        })?;
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || self.effect_id != request.effect_id
            || self.review_node_id != request.review_node_id
            || self.plan_id != request.plan_id
            || self.plan_revision_id != request.plan_revision_id
            || request.plan_id != plan.plan_id
            || request.plan_revision_id != plan.plan_revision_id
            || request.accepted_plan_hash != accepted_plan_hash(plan)?
            || self.policy_id != request.policy_id
            || self.base_repository_revision != request.base_repository_revision
            || self.repository_revision != request.repository_revision
            || self.required_validation_proof_id != request.required_validation_proof_id
            || self.repository_fingerprint != request.repository_fingerprint
            || path_count > request.max_changed_paths
            || page_count > request.max_diff_pages
            || self.changed_paths.windows(2).any(|pair| pair[0] >= pair[1])
            || !path_identities_unique
            || !current_paths_unique
            || self
                .changed_paths
                .iter()
                .any(|path| path.validate().is_err())
            || self
                .plan_assessment
                .validate_against(plan, &self.changed_paths)
                .is_err()
            || (self.changed_paths.is_empty() != self.pages.is_empty())
            || self.changed_paths.len() != self.pages.len()
            || !pages_match_paths
            || !page_ids_unique
            || actual_page_indexes != expected_page_indexes
            || self
                .pages
                .iter()
                .any(|page| page.validate(&self.effect_id).is_err())
            || !coverage_unique
            || covered != expected_coverage
            || self
                .pages
                .iter()
                .any(|page| page.byte_len > request.max_page_bytes)
            || self.total_bytes != computed_total
            || self.total_bytes > request.max_total_diff_bytes
            || self.diff_hash
                != Self::expected_diff_hash(
                    request,
                    &self.repository_fingerprint,
                    &self.changed_paths,
                    &self.plan_assessment,
                    &self.pages,
                    self.total_bytes,
                )?
            || self.manifest_id != Self::expected_id(request, &self.diff_hash)?
        {
            return Err(ReviewContractError::Invalid {
                code: "diff_manifest_invalid",
            });
        }
        Ok(())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.changed_paths.is_empty()
    }

    fn expected_diff_hash(
        request: &DiffManifestRequestV1,
        repository_fingerprint: &str,
        changed_paths: &[DiffPathRecordV1],
        plan_assessment: &DiffPlanAssessmentV1,
        pages: &[DiffPageReceiptV1],
        total_bytes: u64,
    ) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:complete-diff",
            &canonical_json(&(
                &request.base_repository_revision,
                &request.repository_revision,
                &request.required_validation_proof_id,
                repository_fingerprint,
                changed_paths,
                plan_assessment,
                pages,
                total_bytes,
            ))?,
        ]))
    }

    fn expected_id(
        request: &DiffManifestRequestV1,
        diff_hash: &str,
    ) -> Result<DiffManifestId, ReviewContractError> {
        Ok(DiffManifestId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:diff-manifest",
                request.effect_id.as_str(),
                &request.request_hash,
                diff_hash,
            ])
        )))
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "purpose", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ReviewContextBindingV1 {
    DiffPage {
        manifest_id: DiffManifestId,
        diff_hash: String,
        page_id: DiffPageId,
        page_index: u32,
        page_content_hash: String,
        content_address: DiffArtifactAddressV1,
        artifact_locator_hash: String,
        persistence_receipt_hash: String,
        page_byte_len: u64,
    },
    Completion {
        manifest_id: DiffManifestId,
        diff_hash: String,
        diff_review_id: DiffReviewId,
        page_review_ids: Vec<DiffPageReviewId>,
    },
}

impl ReviewContextBindingV1 {
    fn validate(&self) -> Result<(), ReviewContractError> {
        match self {
            Self::DiffPage {
                diff_hash,
                page_content_hash,
                content_address,
                artifact_locator_hash,
                persistence_receipt_hash,
                page_byte_len,
                ..
            } if !is_sha256(diff_hash)
                || !is_sha256(page_content_hash)
                || content_address.validate_against(page_content_hash).is_err()
                || !is_sha256(artifact_locator_hash)
                || !is_sha256(persistence_receipt_hash)
                || *page_byte_len == 0 =>
            {
                Err(ReviewContractError::Invalid {
                    code: "review_diff_page_binding_invalid",
                })
            }
            Self::Completion {
                diff_hash,
                page_review_ids,
                ..
            } if !is_sha256(diff_hash)
                || page_review_ids.iter().collect::<BTreeSet<_>>().len()
                    != page_review_ids.len() =>
            {
                Err(ReviewContractError::Invalid {
                    code: "review_completion_binding_invalid",
                })
            }
            _ => Ok(()),
        }
    }

    const fn tool(&self) -> ReviewToolV1 {
        match self {
            Self::DiffPage { .. } => ReviewToolV1::RecordDiffReview,
            Self::Completion { .. } => ReviewToolV1::RecordCompletionEvaluation,
        }
    }

    pub(crate) fn binding_hash(&self) -> Result<String, ReviewContractError> {
        self.validate()?;
        Ok(stable_sha256(&[
            "execution-protocol-v1:review-context-binding",
            &canonical_json(self)?,
        ]))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewContextManifestV1 {
    pub(crate) schema_version: u16,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) action_id: ActionId,
    pub(crate) node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) policy_id: FinalizationPolicyId,
    pub(crate) required_validation_proof_id: ProofId,
    pub(crate) ancestry_hash: String,
    pub(crate) binding: ReviewContextBindingV1,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
    pub(crate) evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) input_token_ceiling: u32,
    pub(crate) estimated_input_tokens: u32,
    pub(crate) materialized_context_hash: String,
}

impl ReviewContextManifestV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        action_id: ActionId,
        node_id: NodeId,
        repository_revision: RepositoryRevisionId,
        plan_id: PlanId,
        plan_revision_id: PlanRevisionId,
        policy_id: FinalizationPolicyId,
        ancestry: &EngineeringAncestryV1,
        binding: ReviewContextBindingV1,
        criterion_ids: BTreeSet<DiscoveryCriterionId>,
        evidence_ids: BTreeSet<EvidenceId>,
        input_token_ceiling: u32,
        estimated_input_tokens: u32,
        materialized_context_hash: String,
    ) -> Result<Self, ReviewContractError> {
        ancestry.validate()?;
        binding.validate()?;
        let identity = canonical_json(&(
            REVIEW_SCHEMA_VERSION,
            &action_id,
            &node_id,
            &repository_revision,
            &plan_id,
            &plan_revision_id,
            &policy_id,
            &ancestry.required_validation_proof_id,
            &ancestry.ancestry_hash,
            &binding,
            &criterion_ids,
            &evidence_ids,
            input_token_ceiling,
            estimated_input_tokens,
            &materialized_context_hash,
        ))?;
        let context = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            context_manifest_id: ContextManifestId::new(format!(
                "epv1:{}",
                stable_sha256(&["execution-protocol-v1:review-context", &identity])
            )),
            action_id,
            node_id,
            repository_revision,
            plan_id,
            plan_revision_id,
            policy_id,
            required_validation_proof_id: ancestry.required_validation_proof_id.clone(),
            ancestry_hash: ancestry.ancestry_hash.clone(),
            binding,
            criterion_ids,
            evidence_ids,
            input_token_ceiling,
            estimated_input_tokens,
            materialized_context_hash,
        };
        context.validate(ancestry)?;
        Ok(context)
    }

    pub(crate) fn validate(
        &self,
        ancestry: &EngineeringAncestryV1,
    ) -> Result<(), ReviewContractError> {
        ancestry.validate()?;
        self.binding.validate()?;
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || self.repository_revision != ancestry.repository_revision
            || self.required_validation_proof_id != ancestry.required_validation_proof_id
            || self.ancestry_hash != ancestry.ancestry_hash
            || self.criterion_ids.is_empty()
            || self.criterion_ids.len() > MAX_CRITERIA
            || self.evidence_ids.len() > MAX_SUPPORTING_EVIDENCE
            || self.input_token_ceiling == 0
            || self.estimated_input_tokens == 0
            || self.estimated_input_tokens > self.input_token_ceiling
            || !is_sha256(&self.materialized_context_hash)
            || self.context_manifest_id != self.expected_id()?
        {
            return Err(ReviewContractError::Invalid {
                code: "review_context_manifest_invalid",
            });
        }
        Ok(())
    }

    fn expected_id(&self) -> Result<ContextManifestId, ReviewContractError> {
        let identity = canonical_json(&(
            self.schema_version,
            &self.action_id,
            &self.node_id,
            &self.repository_revision,
            &self.plan_id,
            &self.plan_revision_id,
            &self.policy_id,
            &self.required_validation_proof_id,
            &self.ancestry_hash,
            &self.binding,
            &self.criterion_ids,
            &self.evidence_ids,
            self.input_token_ceiling,
            self.estimated_input_tokens,
            &self.materialized_context_hash,
        ))?;
        Ok(ContextManifestId::new(format!(
            "epv1:{}",
            stable_sha256(&["execution-protocol-v1:review-context", &identity])
        )))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewToolV1 {
    RecordDiffReview,
    RecordCompletionEvaluation,
}

impl ReviewToolV1 {
    const fn name(self) -> &'static str {
        match self {
            Self::RecordDiffReview => "record_diff_review",
            Self::RecordCompletionEvaluation => "record_completion_evaluation",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewToolDefinitionV1 {
    pub(crate) schema_version: u16,
    pub(crate) tool: ReviewToolV1,
    pub(crate) name: String,
    pub(crate) strict: bool,
    pub(crate) parameters: Value,
    pub(crate) schema_hash: String,
}

impl ReviewToolDefinitionV1 {
    pub(crate) fn new(
        tool: ReviewToolV1,
        context: &ReviewContextManifestV1,
    ) -> Result<Self, ReviewContractError> {
        if tool != context.binding.tool() {
            return Err(ReviewContractError::Invalid {
                code: "review_tool_schema_binding_mismatch",
            });
        }
        let parameters = review_tool_parameters(tool, context)?;
        let schema_hash = stable_sha256(&[
            "execution-protocol-v1:strict-review-tool-schema",
            tool.name(),
            &canonical_json(&parameters)?,
        ]);
        let definition = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            tool,
            name: tool.name().into(),
            strict: true,
            parameters,
            schema_hash,
        };
        definition.validate_against(context)?;
        Ok(definition)
    }

    pub(crate) fn validate_against(
        &self,
        context: &ReviewContextManifestV1,
    ) -> Result<(), ReviewContractError> {
        let expected_tool = context.binding.tool();
        let expected_parameters = review_tool_parameters(expected_tool, context)?;
        let expected_hash = stable_sha256(&[
            "execution-protocol-v1:strict-review-tool-schema",
            expected_tool.name(),
            &canonical_json(&expected_parameters)?,
        ]);
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || self.tool != expected_tool
            || self.name != expected_tool.name()
            || !self.strict
            || self.parameters != expected_parameters
            || self.schema_hash != expected_hash
        {
            return Err(ReviewContractError::Invalid {
                code: "review_tool_schema_invalid",
            });
        }
        Ok(())
    }
}

fn review_tool_parameters(
    tool: ReviewToolV1,
    context: &ReviewContextManifestV1,
) -> Result<Value, ReviewContractError> {
    review_tool_parameters_for(tool, &context.criterion_ids, &context.evidence_ids)
}

fn review_tool_parameters_for(
    tool: ReviewToolV1,
    criterion_ids: &BTreeSet<DiscoveryCriterionId>,
    evidence_ids: &BTreeSet<EvidenceId>,
) -> Result<Value, ReviewContractError> {
    let criterion_values =
        serde_json::to_value(criterion_ids).map_err(|_| ReviewContractError::Serialization)?;
    let evidence_values =
        serde_json::to_value(evidence_ids).map_err(|_| ReviewContractError::Serialization)?;
    let id_schema = json!({ "type": "string", "enum": evidence_values });
    let evidence_array = json!({
        "type": "array",
        "items": id_schema,
        "uniqueItems": true,
        "maxItems": MAX_SUPPORTING_EVIDENCE,
    });
    let required_evidence_array = json!({
        "type": "array",
        "items": { "type": "string", "enum": evidence_values },
        "uniqueItems": true,
        "minItems": 1,
        "maxItems": MAX_SUPPORTING_EVIDENCE,
    });
    let criterion_array = json!({
        "type": "array",
        "items": { "type": "string", "enum": criterion_values },
        "uniqueItems": true,
        "maxItems": MAX_CRITERIA,
    });
    match tool {
        ReviewToolV1::RecordDiffReview => Ok(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "findings": {
                    "type": "array",
                    "maxItems": MAX_FINDINGS_PER_PAGE,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": ["unplanned_change", "unsafe_change", "incomplete_implementation", "criterion_evidence_gap", "advisory"]
                            },
                            "severity": { "type": "string", "enum": ["blocking", "advisory"] },
                            "path_indexes": {
                                "type": "array",
                                "items": { "type": "integer", "minimum": 0 },
                                "uniqueItems": true,
                                "minItems": 1,
                                "maxItems": MAX_CHANGED_PATHS,
                            },
                            "criterion_ids": criterion_array,
                            "supporting_evidence_ids": evidence_array,
                            "safe_code": { "type": "string", "pattern": "^[a-z0-9_.-]{1,128}$" },
                            "detail_hash": { "type": "string", "pattern": "^[0-9a-f]{64}$" },
                        },
                        "required": ["kind", "severity", "path_indexes", "criterion_ids", "supporting_evidence_ids", "safe_code", "detail_hash"],
                    },
                },
            },
            "required": ["findings"],
        })),
        ReviewToolV1::RecordCompletionEvaluation => {
            let criterion_count = criterion_ids.len();
            Ok(json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "criteria": {
                        "type": "array",
                        "minItems": criterion_count,
                        "maxItems": criterion_count,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "criterion_id": { "type": "string", "enum": criterion_values },
                                "status": {
                                    "oneOf": [
                                        {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "properties": {
                                                "status": { "const": "satisfied" },
                                                "supporting_evidence_ids": required_evidence_array,
                                            },
                                            "required": ["status", "supporting_evidence_ids"],
                                        },
                                        {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "properties": {
                                                "status": { "const": "external_review_required" },
                                                "kind": { "type": "string", "enum": ["manual_qa", "accessibility_review", "visual_review", "product_approval", "deployment_environment"] },
                                                "requirement_code": { "type": "string", "pattern": "^[a-z0-9_.-]{1,128}$" },
                                                "detail_hash": { "type": "string", "pattern": "^[0-9a-f]{64}$" },
                                            },
                                            "required": ["status", "kind", "requirement_code", "detail_hash"],
                                        },
                                        {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "properties": {
                                                "status": { "const": "unsatisfied" },
                                                "reason_code": { "type": "string", "pattern": "^[a-z0-9_.-]{1,128}$" },
                                                "missing_evidence_ids": evidence_array,
                                            },
                                            "required": ["status", "reason_code", "missing_evidence_ids"],
                                        },
                                        {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "properties": {
                                                "status": { "const": "uncertain" },
                                                "reason_code": { "type": "string", "pattern": "^[a-z0-9_.-]{1,128}$" },
                                            },
                                            "required": ["status", "reason_code"],
                                        }
                                    ]
                                },
                            },
                            "required": ["criterion_id", "status"],
                        },
                    },
                },
                "required": ["criteria"],
            }))
        }
    }
}

/// Conservative deterministic upper bound for the provider input owned by a
/// review action. One token per serialized byte is used, and referenced raw
/// artifact bytes are charged at six bytes each to cover worst-case JSON
/// escaping. Provider adapters must not append ambient context outside this
/// signed material.
#[allow(clippy::too_many_arguments)]
pub(crate) fn conservative_review_input_tokens(
    plan: &AcceptedPlan,
    ancestry: &EngineeringAncestryV1,
    manifest: &DiffManifestV1,
    diff_review: Option<&DiffReviewV1>,
    binding: &ReviewContextBindingV1,
    criterion_ids: &BTreeSet<DiscoveryCriterionId>,
    evidence_ids: &BTreeSet<EvidenceId>,
) -> Result<u32, ReviewContractError> {
    ancestry.validate()?;
    binding.validate()?;
    manifest
        .plan_assessment
        .validate_against(plan, &manifest.changed_paths)?;
    let expected_criterion_ids = plan
        .targets
        .iter()
        .flat_map(|target| target.acceptance_criteria.iter().cloned())
        .collect::<BTreeSet<_>>();
    if manifest.plan_id != plan.plan_id
        || manifest.plan_revision_id != plan.plan_revision_id
        || manifest.plan_assessment.accepted_plan_hash != accepted_plan_hash(plan)?
        || manifest.repository_revision != ancestry.repository_revision
        || manifest.repository_fingerprint != ancestry.repository_fingerprint
        || manifest.required_validation_proof_id != ancestry.required_validation_proof_id
        || criterion_ids != &expected_criterion_ids
        || evidence_ids.len() > MAX_SUPPORTING_EVIDENCE
    {
        return Err(ReviewContractError::Invalid {
            code: "review_input_budget_context_invalid",
        });
    }

    let referenced_raw_bytes = match binding {
        ReviewContextBindingV1::DiffPage {
            manifest_id,
            diff_hash,
            page_id,
            page_index,
            page_content_hash,
            content_address,
            artifact_locator_hash,
            persistence_receipt_hash,
            page_byte_len,
        } => {
            let page = manifest
                .pages
                .get(*page_index as usize)
                .filter(|page| &page.page_id == page_id)
                .ok_or(ReviewContractError::Invalid {
                    code: "review_input_budget_page_missing",
                })?;
            if manifest_id != &manifest.manifest_id
                || diff_hash != &manifest.diff_hash
                || page_content_hash != &page.content_hash
                || content_address != &page.content_address
                || artifact_locator_hash != &page.artifact_locator_hash
                || persistence_receipt_hash != &page.persistence_receipt_hash
                || page_byte_len != &page.byte_len
                || diff_review.is_some()
            {
                return Err(ReviewContractError::Invalid {
                    code: "review_input_budget_page_binding_mismatch",
                });
            }
            page.byte_len
        }
        ReviewContextBindingV1::Completion {
            manifest_id,
            diff_hash,
            diff_review_id,
            page_review_ids,
        } => {
            let review = diff_review.ok_or(ReviewContractError::Invalid {
                code: "review_input_budget_completion_review_missing",
            })?;
            if manifest_id != &manifest.manifest_id
                || diff_hash != &manifest.diff_hash
                || diff_review_id != &review.review_id
                || page_review_ids != &review.ordered_page_review_ids
                || review.repository_revision != manifest.repository_revision
                || review.manifest_id != manifest.manifest_id
                || review.diff_hash != manifest.diff_hash
            {
                return Err(ReviewContractError::Invalid {
                    code: "review_input_budget_completion_binding_mismatch",
                });
            }
            manifest.total_bytes
        }
    };
    let tool = binding.tool();
    let parameters = review_tool_parameters_for(tool, criterion_ids, evidence_ids)?;
    let schema_hash = stable_sha256(&[
        "execution-protocol-v1:strict-review-tool-schema",
        tool.name(),
        &canonical_json(&parameters)?,
    ]);
    let signed_context = canonical_json(&(
        REVIEW_SCHEMA_VERSION,
        plan,
        ancestry,
        manifest,
        diff_review,
        binding,
        criterion_ids,
        evidence_ids,
        (tool, tool.name(), true, &parameters, &schema_hash, false),
    ))?;
    let estimated_bytes = u64::try_from(signed_context.len())
        .unwrap_or(u64::MAX)
        .saturating_add(referenced_raw_bytes.saturating_mul(6))
        .saturating_add(REVIEW_PROVIDER_FIXED_OVERHEAD_BYTES);
    Ok(u32::try_from(estimated_bytes).unwrap_or(u32::MAX).max(1))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "choice", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ReviewToolChoiceV1 {
    Named { tool: ReviewToolV1 },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewActionEnvelopeV1 {
    pub(crate) schema_version: u16,
    pub(crate) action_id: ActionId,
    pub(crate) call_id: ModelCallId,
    pub(crate) node_id: NodeId,
    pub(crate) node_attempt: u32,
    pub(crate) retry_index: u32,
    pub(crate) prior_action_id: Option<ActionId>,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) context_manifest_id: ContextManifestId,
    pub(crate) binding: ReviewContextBindingV1,
    pub(crate) tools: BTreeSet<ReviewToolV1>,
    pub(crate) tool_definitions: Vec<ReviewToolDefinitionV1>,
    pub(crate) tool_choice: ReviewToolChoiceV1,
    pub(crate) parallel_tool_calls: bool,
    pub(crate) input_token_ceiling: u32,
    pub(crate) output_token_allowance: u32,
    pub(crate) budget_owner_node_id: NodeId,
    pub(crate) reservation_id: ReservationId,
    pub(crate) payload_identity: String,
}

impl ReviewActionEnvelopeV1 {
    #[allow(clippy::too_many_arguments)]
    fn new(
        action_id: ActionId,
        call_id: ModelCallId,
        node_id: NodeId,
        node_attempt: u32,
        retry_index: u32,
        prior_action_id: Option<ActionId>,
        repository_revision: RepositoryRevisionId,
        context: &ReviewContextManifestV1,
        output_token_allowance: u32,
        reservation_id: ReservationId,
    ) -> Result<Self, ReviewContractError> {
        let tool = context.binding.tool();
        let tools = BTreeSet::from([tool]);
        let tool_definitions = vec![ReviewToolDefinitionV1::new(tool, context)?];
        let tool_choice = ReviewToolChoiceV1::Named { tool };
        let identity = canonical_json(&(
            (
                REVIEW_SCHEMA_VERSION,
                &action_id,
                &call_id,
                &node_id,
                node_attempt,
                retry_index,
                &prior_action_id,
                &repository_revision,
            ),
            (
                &context.context_manifest_id,
                &context.binding,
                &tools,
                &tool_definitions,
                &tool_choice,
                false,
                context.input_token_ceiling,
                output_token_allowance,
                &node_id,
                &reservation_id,
            ),
        ))?;
        let envelope = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            action_id,
            call_id,
            node_id: node_id.clone(),
            node_attempt,
            retry_index,
            prior_action_id,
            repository_revision,
            context_manifest_id: context.context_manifest_id.clone(),
            binding: context.binding.clone(),
            tools,
            tool_definitions,
            tool_choice,
            parallel_tool_calls: false,
            input_token_ceiling: context.input_token_ceiling,
            output_token_allowance,
            budget_owner_node_id: node_id,
            reservation_id,
            payload_identity: stable_sha256(&[
                "execution-protocol-v1:review-provider-payload",
                &identity,
            ]),
        };
        envelope.validate_against(context)?;
        Ok(envelope)
    }

    pub(crate) fn validate_against(
        &self,
        context: &ReviewContextManifestV1,
    ) -> Result<(), ReviewContractError> {
        let expected_tool = context.binding.tool();
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || self.action_id != context.action_id
            || self.node_id != context.node_id
            || self.repository_revision != context.repository_revision
            || self.context_manifest_id != context.context_manifest_id
            || self.binding != context.binding
            || self.tools != BTreeSet::from([expected_tool])
            || self.tool_definitions.len() != 1
            || self.tool_definitions[0].validate_against(context).is_err()
            || self.tool_definitions[0].tool != expected_tool
            || self.tool_choice
                != (ReviewToolChoiceV1::Named {
                    tool: expected_tool,
                })
            || self.parallel_tool_calls
            || self.input_token_ceiling != context.input_token_ceiling
            || self.output_token_allowance == 0
            || self.budget_owner_node_id != self.node_id
            || self.retry_index == 0
            || (self.retry_index == 1) != self.prior_action_id.is_none()
            || !is_sha256(&self.payload_identity)
            || self.payload_identity != self.expected_payload_identity()?
        {
            return Err(ReviewContractError::Invalid {
                code: "review_provider_envelope_binding_mismatch",
            });
        }
        Ok(())
    }

    fn expected_payload_identity(&self) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:review-provider-payload",
            &canonical_json(&(
                (
                    self.schema_version,
                    &self.action_id,
                    &self.call_id,
                    &self.node_id,
                    self.node_attempt,
                    self.retry_index,
                    &self.prior_action_id,
                    &self.repository_revision,
                ),
                (
                    &self.context_manifest_id,
                    &self.binding,
                    &self.tools,
                    &self.tool_definitions,
                    &self.tool_choice,
                    self.parallel_tool_calls,
                    self.input_token_ceiling,
                    self.output_token_allowance,
                    &self.budget_owner_node_id,
                    &self.reservation_id,
                ),
            ))?,
        ]))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedReviewActionV1 {
    pub(crate) context: ReviewContextManifestV1,
    pub(crate) envelope: ReviewActionEnvelopeV1,
    pub(crate) admission: ModelCallAdmission,
}

impl PreparedReviewActionV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        execution_id: &ExecutionId,
        execution_attempt: u32,
        node_id: NodeId,
        node_attempt: u32,
        retry_index: u32,
        prior_action_id: Option<ActionId>,
        repository_revision: RepositoryRevisionId,
        plan_id: PlanId,
        plan_revision_id: PlanRevisionId,
        policy_id: FinalizationPolicyId,
        ancestry: &EngineeringAncestryV1,
        binding: ReviewContextBindingV1,
        criterion_ids: BTreeSet<DiscoveryCriterionId>,
        evidence_ids: BTreeSet<EvidenceId>,
        input_token_ceiling: u32,
        estimated_input_tokens: u32,
        output_token_allowance: u32,
        reserved_cost_micros: u64,
        duration_allowance_ms: u64,
        materialized_context_hash: String,
    ) -> Result<Self, ReviewContractError> {
        if retry_index == 0 || (retry_index == 1) != prior_action_id.is_none() {
            return Err(ReviewContractError::Invalid {
                code: "review_action_retry_chain_invalid",
            });
        }
        let action_identity = canonical_json(&(
            execution_id,
            execution_attempt,
            &node_id,
            node_attempt,
            retry_index,
            &prior_action_id,
            &repository_revision,
            &plan_id,
            &plan_revision_id,
            &policy_id,
            &ancestry.ancestry_hash,
            &binding,
        ))?;
        let action_id = ActionId::new(format!(
            "epv1:{}",
            stable_sha256(&["execution-protocol-v1:review-action", &action_identity])
        ));
        let context = ReviewContextManifestV1::new(
            action_id.clone(),
            node_id.clone(),
            repository_revision.clone(),
            plan_id,
            plan_revision_id,
            policy_id,
            ancestry,
            binding,
            criterion_ids,
            evidence_ids,
            input_token_ceiling,
            estimated_input_tokens,
            materialized_context_hash,
        )?;
        let call_id = ModelCallId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:review-model-call",
                execution_id.as_str(),
                &execution_attempt.to_string(),
                action_id.as_str(),
            ])
        ));
        let reservation_id = ReservationId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:review-reservation",
                execution_id.as_str(),
                &execution_attempt.to_string(),
                action_id.as_str(),
            ])
        ));
        let envelope = ReviewActionEnvelopeV1::new(
            action_id.clone(),
            call_id.clone(),
            node_id.clone(),
            node_attempt,
            retry_index,
            prior_action_id,
            repository_revision,
            &context,
            output_token_allowance,
            reservation_id,
        )?;
        let admission = ModelCallAdmission {
            call_id,
            node_id,
            action_id,
            payload_hash: envelope.payload_identity.clone(),
            input_tokens: estimated_input_tokens,
            output_tokens: output_token_allowance,
            reserved_cost_micros,
            duration_allowance_ms,
        };
        let prepared = Self {
            context,
            envelope,
            admission,
        };
        prepared.validate(ancestry)?;
        Ok(prepared)
    }

    pub(crate) fn validate(
        &self,
        ancestry: &EngineeringAncestryV1,
    ) -> Result<(), ReviewContractError> {
        self.context.validate(ancestry)?;
        self.envelope.validate_against(&self.context)?;
        if self.admission.call_id != self.envelope.call_id
            || self.admission.node_id != self.envelope.node_id
            || self.admission.action_id != self.envelope.action_id
            || self.admission.payload_hash != self.envelope.payload_identity
            || self.admission.input_tokens != self.context.estimated_input_tokens
            || self.admission.output_tokens != self.envelope.output_token_allowance
            || self.admission.reserved_cost_micros == 0
            || self.admission.duration_allowance_ms == 0
        {
            return Err(ReviewContractError::Invalid {
                code: "prepared_review_action_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewActionRejectionReasonV1 {
    ProviderProtocolViolation,
    InvalidObservation,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffReviewFindingKindV1 {
    UnplannedChange,
    UnsafeChange,
    IncompleteImplementation,
    CriterionEvidenceGap,
    Advisory,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffReviewFindingSeverityV1 {
    Blocking,
    Advisory,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffReviewFindingV1 {
    pub(crate) finding_id: EvidenceId,
    pub(crate) kind: DiffReviewFindingKindV1,
    pub(crate) severity: DiffReviewFindingSeverityV1,
    pub(crate) path_indexes: BTreeSet<u32>,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
    pub(crate) supporting_evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) safe_code: String,
    pub(crate) detail_hash: String,
}

impl DiffReviewFindingV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        kind: DiffReviewFindingKindV1,
        severity: DiffReviewFindingSeverityV1,
        path_indexes: BTreeSet<u32>,
        criterion_ids: BTreeSet<DiscoveryCriterionId>,
        supporting_evidence_ids: BTreeSet<EvidenceId>,
        safe_code: String,
        detail_hash: String,
    ) -> Result<Self, ReviewContractError> {
        let finding_id = Self::expected_id(
            kind,
            severity,
            &path_indexes,
            &criterion_ids,
            &supporting_evidence_ids,
            &safe_code,
            &detail_hash,
        )?;
        let finding = Self {
            finding_id,
            kind,
            severity,
            path_indexes,
            criterion_ids,
            supporting_evidence_ids,
            safe_code,
            detail_hash,
        };
        finding.validate()?;
        Ok(finding)
    }

    pub(crate) fn validate(&self) -> Result<(), ReviewContractError> {
        if self.path_indexes.is_empty()
            || self.criterion_ids.len() > MAX_CRITERIA
            || self.supporting_evidence_ids.len() > MAX_SUPPORTING_EVIDENCE
            || !safe_code_is_valid(&self.safe_code)
            || !is_sha256(&self.detail_hash)
            || (self.kind == DiffReviewFindingKindV1::Advisory
                && self.severity != DiffReviewFindingSeverityV1::Advisory)
            || (self.kind != DiffReviewFindingKindV1::Advisory
                && self.severity != DiffReviewFindingSeverityV1::Blocking)
            || self.finding_id
                != Self::expected_id(
                    self.kind,
                    self.severity,
                    &self.path_indexes,
                    &self.criterion_ids,
                    &self.supporting_evidence_ids,
                    &self.safe_code,
                    &self.detail_hash,
                )?
        {
            return Err(ReviewContractError::Invalid {
                code: "diff_review_finding_invalid",
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn expected_id(
        kind: DiffReviewFindingKindV1,
        severity: DiffReviewFindingSeverityV1,
        path_indexes: &BTreeSet<u32>,
        criterion_ids: &BTreeSet<DiscoveryCriterionId>,
        supporting_evidence_ids: &BTreeSet<EvidenceId>,
        safe_code: &str,
        detail_hash: &str,
    ) -> Result<EvidenceId, ReviewContractError> {
        Ok(EvidenceId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:diff-review-finding",
                &canonical_json(&(
                    kind,
                    severity,
                    path_indexes,
                    criterion_ids,
                    supporting_evidence_ids,
                    safe_code,
                    detail_hash,
                ))?,
            ])
        )))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffPageReviewStatusV1 {
    Accepted,
    Blocking,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffPageReviewObservationV1 {
    pub(crate) schema_version: u16,
    pub(crate) observation_id: DiffPageReviewId,
    pub(crate) action_id: ActionId,
    pub(crate) call_id: ModelCallId,
    pub(crate) node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) manifest_id: DiffManifestId,
    pub(crate) page_id: DiffPageId,
    pub(crate) page_index: u32,
    pub(crate) status: DiffPageReviewStatusV1,
    pub(crate) findings: Vec<DiffReviewFindingV1>,
    pub(crate) observation_hash: String,
}

impl DiffPageReviewObservationV1 {
    pub(crate) fn new(
        prepared: &PreparedReviewActionV1,
        manifest: &DiffManifestV1,
        mut findings: Vec<DiffReviewFindingV1>,
    ) -> Result<Self, ReviewContractError> {
        let ReviewContextBindingV1::DiffPage {
            manifest_id,
            diff_hash,
            page_id,
            page_index,
            page_content_hash,
            content_address,
            artifact_locator_hash,
            persistence_receipt_hash,
            page_byte_len,
        } = &prepared.context.binding
        else {
            return Err(ReviewContractError::Invalid {
                code: "diff_review_observation_wrong_action",
            });
        };
        let page = manifest
            .pages
            .get(*page_index as usize)
            .filter(|page| &page.page_id == page_id)
            .ok_or(ReviewContractError::Invalid {
                code: "diff_review_page_missing",
            })?;
        if manifest_id != &manifest.manifest_id
            || diff_hash != &manifest.diff_hash
            || page_content_hash != &page.content_hash
            || content_address != &page.content_address
            || artifact_locator_hash != &page.artifact_locator_hash
            || persistence_receipt_hash != &page.persistence_receipt_hash
            || page_byte_len != &page.byte_len
            || prepared.context.repository_revision != manifest.repository_revision
        {
            return Err(ReviewContractError::Invalid {
                code: "diff_review_observation_manifest_mismatch",
            });
        }
        findings.sort();
        findings.dedup();
        if findings.len() > MAX_FINDINGS_PER_PAGE
            || findings.iter().any(|finding| {
                finding.validate().is_err()
                    || !finding.path_indexes.is_subset(&page.covered_path_indexes)
                    || !finding
                        .criterion_ids
                        .is_subset(&prepared.context.criterion_ids)
                    || !finding
                        .supporting_evidence_ids
                        .is_subset(&prepared.context.evidence_ids)
            })
        {
            return Err(ReviewContractError::Invalid {
                code: "diff_review_observation_findings_invalid",
            });
        }
        let status = if findings
            .iter()
            .any(|finding| finding.severity == DiffReviewFindingSeverityV1::Blocking)
        {
            DiffPageReviewStatusV1::Blocking
        } else {
            DiffPageReviewStatusV1::Accepted
        };
        let observation_hash = Self::expected_hash(
            &prepared.envelope.action_id,
            &prepared.envelope.call_id,
            &prepared.envelope.node_id,
            &manifest.repository_revision,
            &manifest.manifest_id,
            page,
            status,
            &findings,
        )?;
        let observation = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            observation_id: DiffPageReviewId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:diff-page-review",
                    prepared.envelope.action_id.as_str(),
                    page.page_id.as_str(),
                    &observation_hash,
                ])
            )),
            action_id: prepared.envelope.action_id.clone(),
            call_id: prepared.envelope.call_id.clone(),
            node_id: prepared.envelope.node_id.clone(),
            repository_revision: manifest.repository_revision.clone(),
            manifest_id: manifest.manifest_id.clone(),
            page_id: page.page_id.clone(),
            page_index: page.index,
            status,
            findings,
            observation_hash,
        };
        observation.validate_against(prepared, manifest)?;
        Ok(observation)
    }

    pub(crate) fn validate_against(
        &self,
        prepared: &PreparedReviewActionV1,
        manifest: &DiffManifestV1,
    ) -> Result<(), ReviewContractError> {
        let ReviewContextBindingV1::DiffPage {
            manifest_id,
            diff_hash,
            page_id,
            page_index,
            page_content_hash,
            content_address,
            artifact_locator_hash,
            persistence_receipt_hash,
            page_byte_len,
        } = &prepared.context.binding
        else {
            return Err(ReviewContractError::Invalid {
                code: "diff_review_observation_wrong_action",
            });
        };
        let page = manifest
            .pages
            .get(*page_index as usize)
            .filter(|page| &page.page_id == page_id)
            .ok_or(ReviewContractError::Invalid {
                code: "diff_review_page_missing",
            })?;
        let expected_status = if self
            .findings
            .iter()
            .any(|finding| finding.severity == DiffReviewFindingSeverityV1::Blocking)
        {
            DiffPageReviewStatusV1::Blocking
        } else {
            DiffPageReviewStatusV1::Accepted
        };
        let expected_hash = Self::expected_hash(
            &self.action_id,
            &self.call_id,
            &self.node_id,
            &self.repository_revision,
            &self.manifest_id,
            page,
            self.status,
            &self.findings,
        )?;
        let expected_id = DiffPageReviewId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:diff-page-review",
                self.action_id.as_str(),
                self.page_id.as_str(),
                &expected_hash,
            ])
        ));
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || manifest_id != &manifest.manifest_id
            || diff_hash != &manifest.diff_hash
            || page_content_hash != &page.content_hash
            || content_address != &page.content_address
            || artifact_locator_hash != &page.artifact_locator_hash
            || persistence_receipt_hash != &page.persistence_receipt_hash
            || page_byte_len != &page.byte_len
            || self.action_id != prepared.envelope.action_id
            || self.call_id != prepared.envelope.call_id
            || self.node_id != prepared.envelope.node_id
            || self.repository_revision != manifest.repository_revision
            || self.manifest_id != manifest.manifest_id
            || self.page_id != page.page_id
            || self.page_index != page.index
            || self.findings.len() > MAX_FINDINGS_PER_PAGE
            || self.findings.windows(2).any(|pair| pair[0] >= pair[1])
            || self.findings.iter().any(|finding| {
                finding.validate().is_err()
                    || !finding.path_indexes.is_subset(&page.covered_path_indexes)
                    || !finding
                        .criterion_ids
                        .is_subset(&prepared.context.criterion_ids)
                    || !finding
                        .supporting_evidence_ids
                        .is_subset(&prepared.context.evidence_ids)
            })
            || self.status != expected_status
            || self.observation_hash != expected_hash
            || self.observation_id != expected_id
        {
            return Err(ReviewContractError::Invalid {
                code: "diff_page_review_observation_invalid",
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn expected_hash(
        action_id: &ActionId,
        call_id: &ModelCallId,
        node_id: &NodeId,
        repository_revision: &RepositoryRevisionId,
        manifest_id: &DiffManifestId,
        page: &DiffPageReceiptV1,
        status: DiffPageReviewStatusV1,
        findings: &[DiffReviewFindingV1],
    ) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:diff-page-review-observation",
            &canonical_json(&(
                action_id,
                call_id,
                node_id,
                repository_revision,
                manifest_id,
                &page.page_id,
                page.index,
                status,
                findings,
            ))?,
        ]))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffReviewDispositionV1 {
    Accepted,
    EmptyDiff,
    Blocking,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiffReviewV1 {
    pub(crate) schema_version: u16,
    pub(crate) review_id: DiffReviewId,
    pub(crate) node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) manifest_id: DiffManifestId,
    pub(crate) diff_hash: String,
    pub(crate) ordered_page_review_ids: Vec<DiffPageReviewId>,
    pub(crate) finding_ids: Vec<EvidenceId>,
    pub(crate) disposition: DiffReviewDispositionV1,
    pub(crate) review_hash: String,
}

impl DiffReviewV1 {
    pub(crate) fn aggregate(
        manifest: &DiffManifestV1,
        observations: &BTreeMap<DiffPageId, DiffPageReviewObservationV1>,
    ) -> Result<Self, ReviewContractError> {
        if observations.len() != manifest.pages.len() {
            return Err(ReviewContractError::Invalid {
                code: "diff_review_page_coverage_incomplete",
            });
        }
        let ordered = manifest
            .pages
            .iter()
            .map(|page| {
                observations
                    .get(&page.page_id)
                    .filter(|observation| {
                        observation.manifest_id == manifest.manifest_id
                            && observation.repository_revision == manifest.repository_revision
                            && observation.page_index == page.index
                    })
                    .ok_or(ReviewContractError::Invalid {
                        code: "diff_review_page_observation_mismatch",
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let ordered_page_review_ids = ordered
            .iter()
            .map(|observation| observation.observation_id.clone())
            .collect::<Vec<_>>();
        let mut finding_ids = ordered
            .iter()
            .flat_map(|observation| {
                observation
                    .findings
                    .iter()
                    .map(|finding| finding.finding_id.clone())
            })
            .collect::<Vec<_>>();
        finding_ids.sort();
        finding_ids.dedup();
        let disposition = if !manifest.plan_assessment.is_safe_and_complete()
            || ordered
                .iter()
                .any(|observation| observation.status == DiffPageReviewStatusV1::Blocking)
        {
            DiffReviewDispositionV1::Blocking
        } else if manifest.is_empty() {
            DiffReviewDispositionV1::EmptyDiff
        } else {
            DiffReviewDispositionV1::Accepted
        };
        let review_hash = Self::expected_hash(
            &manifest.review_node_id,
            &manifest.repository_revision,
            &manifest.manifest_id,
            &manifest.diff_hash,
            &ordered_page_review_ids,
            &finding_ids,
            disposition,
        )?;
        let review = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            review_id: DiffReviewId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:diff-review",
                    manifest.manifest_id.as_str(),
                    &review_hash,
                ])
            )),
            node_id: manifest.review_node_id.clone(),
            repository_revision: manifest.repository_revision.clone(),
            manifest_id: manifest.manifest_id.clone(),
            diff_hash: manifest.diff_hash.clone(),
            ordered_page_review_ids,
            finding_ids,
            disposition,
            review_hash,
        };
        review.validate_against(manifest, observations)?;
        Ok(review)
    }

    pub(crate) fn validate_against(
        &self,
        manifest: &DiffManifestV1,
        observations: &BTreeMap<DiffPageId, DiffPageReviewObservationV1>,
    ) -> Result<(), ReviewContractError> {
        let expected = Self::aggregate_unchecked(manifest, observations)?;
        if self != &expected {
            return Err(ReviewContractError::Invalid {
                code: "diff_review_aggregate_invalid",
            });
        }
        Ok(())
    }

    fn aggregate_unchecked(
        manifest: &DiffManifestV1,
        observations: &BTreeMap<DiffPageId, DiffPageReviewObservationV1>,
    ) -> Result<Self, ReviewContractError> {
        if observations.len() != manifest.pages.len() {
            return Err(ReviewContractError::Invalid {
                code: "diff_review_page_coverage_incomplete",
            });
        }
        let ordered = manifest
            .pages
            .iter()
            .map(|page| {
                observations
                    .get(&page.page_id)
                    .filter(|observation| {
                        observation.manifest_id == manifest.manifest_id
                            && observation.repository_revision == manifest.repository_revision
                            && observation.page_index == page.index
                    })
                    .ok_or(ReviewContractError::Invalid {
                        code: "diff_review_page_observation_mismatch",
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let ordered_page_review_ids = ordered
            .iter()
            .map(|observation| observation.observation_id.clone())
            .collect::<Vec<_>>();
        let mut finding_ids = ordered
            .iter()
            .flat_map(|observation| {
                observation
                    .findings
                    .iter()
                    .map(|finding| finding.finding_id.clone())
            })
            .collect::<Vec<_>>();
        finding_ids.sort();
        finding_ids.dedup();
        let disposition = if !manifest.plan_assessment.is_safe_and_complete()
            || ordered
                .iter()
                .any(|observation| observation.status == DiffPageReviewStatusV1::Blocking)
        {
            DiffReviewDispositionV1::Blocking
        } else if manifest.is_empty() {
            DiffReviewDispositionV1::EmptyDiff
        } else {
            DiffReviewDispositionV1::Accepted
        };
        let review_hash = Self::expected_hash(
            &manifest.review_node_id,
            &manifest.repository_revision,
            &manifest.manifest_id,
            &manifest.diff_hash,
            &ordered_page_review_ids,
            &finding_ids,
            disposition,
        )?;
        Ok(Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            review_id: DiffReviewId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:diff-review",
                    manifest.manifest_id.as_str(),
                    &review_hash,
                ])
            )),
            node_id: manifest.review_node_id.clone(),
            repository_revision: manifest.repository_revision.clone(),
            manifest_id: manifest.manifest_id.clone(),
            diff_hash: manifest.diff_hash.clone(),
            ordered_page_review_ids,
            finding_ids,
            disposition,
            review_hash,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn expected_hash(
        node_id: &NodeId,
        repository_revision: &RepositoryRevisionId,
        manifest_id: &DiffManifestId,
        diff_hash: &str,
        ordered_page_review_ids: &[DiffPageReviewId],
        finding_ids: &[EvidenceId],
        disposition: DiffReviewDispositionV1,
    ) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:complete-diff-review",
            &canonical_json(&(
                node_id,
                repository_revision,
                manifest_id,
                diff_hash,
                ordered_page_review_ids,
                finding_ids,
                disposition,
            ))?,
        ]))
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CriterionCompletionEvidenceV1 {
    pub(crate) schema_version: u16,
    pub(crate) criterion_id: DiscoveryCriterionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) accepted_plan_hash: String,
    pub(crate) manifest_id: DiffManifestId,
    pub(crate) diff_hash: String,
    pub(crate) plan_assessment_hash: String,
    pub(crate) target_path_indexes: BTreeMap<TargetId, u32>,
    pub(crate) validation_expectation_ids_by_target:
        BTreeMap<TargetId, BTreeSet<ValidationExpectationId>>,
    pub(crate) required_validation_proof_id: ProofId,
    pub(crate) evidence_hash: String,
}

impl CriterionCompletionEvidenceV1 {
    pub(crate) fn derive(
        criterion_id: DiscoveryCriterionId,
        plan: &AcceptedPlan,
        manifest: &DiffManifestV1,
        ancestry: &EngineeringAncestryV1,
    ) -> Result<Self, ReviewContractError> {
        manifest
            .plan_assessment
            .validate_against(plan, &manifest.changed_paths)?;
        ancestry.validate()?;
        let evidence = Self::derive_unchecked(criterion_id, plan, manifest, ancestry)?;
        evidence.validate_against(plan, manifest, ancestry)?;
        Ok(evidence)
    }

    pub(crate) fn validate_against(
        &self,
        plan: &AcceptedPlan,
        manifest: &DiffManifestV1,
        ancestry: &EngineeringAncestryV1,
    ) -> Result<(), ReviewContractError> {
        manifest
            .plan_assessment
            .validate_against(plan, &manifest.changed_paths)?;
        ancestry.validate()?;
        let expected = Self::derive_unchecked(self.criterion_id.clone(), plan, manifest, ancestry)?;
        if self != &expected {
            return Err(ReviewContractError::Invalid {
                code: "criterion_completion_evidence_invalid",
            });
        }
        Ok(())
    }

    pub(crate) fn is_complete(&self) -> bool {
        !self.target_path_indexes.is_empty()
            && self.target_path_indexes.keys().collect::<BTreeSet<_>>()
                == self
                    .validation_expectation_ids_by_target
                    .keys()
                    .collect::<BTreeSet<_>>()
            && self
                .validation_expectation_ids_by_target
                .values()
                .all(|expectation_ids| !expectation_ids.is_empty())
    }

    fn derive_unchecked(
        criterion_id: DiscoveryCriterionId,
        plan: &AcceptedPlan,
        manifest: &DiffManifestV1,
        ancestry: &EngineeringAncestryV1,
    ) -> Result<Self, ReviewContractError> {
        if manifest.plan_id != plan.plan_id
            || manifest.plan_revision_id != plan.plan_revision_id
            || manifest.plan_assessment.accepted_plan_hash != accepted_plan_hash(plan)?
            || manifest.repository_revision != ancestry.repository_revision
            || manifest.repository_fingerprint != ancestry.repository_fingerprint
            || manifest.required_validation_proof_id != ancestry.required_validation_proof_id
        {
            return Err(ReviewContractError::Invalid {
                code: "criterion_completion_source_binding_mismatch",
            });
        }
        let targets = plan
            .targets
            .iter()
            .filter(|target| target.acceptance_criteria.contains(&criterion_id))
            .collect::<Vec<_>>();
        if targets.is_empty() || targets.len() > MAX_CHANGED_PATHS {
            return Err(ReviewContractError::Invalid {
                code: "criterion_completion_target_coverage_invalid",
            });
        }
        let target_ids = targets
            .iter()
            .map(|target| &target.target_id)
            .collect::<BTreeSet<_>>();
        let target_path_indexes = manifest
            .plan_assessment
            .target_path_indexes
            .iter()
            .filter(|(target_id, _)| target_ids.contains(target_id))
            .map(|(target_id, path_index)| (target_id.clone(), *path_index))
            .collect::<BTreeMap<_, _>>();
        let validation_expectation_ids_by_target = targets
            .iter()
            .map(|target| {
                let expectation_ids = target
                    .expected_validation
                    .iter()
                    .filter(|expectation| expectation.criterion_ids.contains(&criterion_id))
                    .map(|expectation| expectation.expectation_id.clone())
                    .collect::<BTreeSet<_>>();
                (target.target_id.clone(), expectation_ids)
            })
            .collect::<BTreeMap<_, _>>();
        let accepted_plan_hash = accepted_plan_hash(plan)?;
        let evidence_hash = stable_sha256(&[
            "execution-protocol-v1:criterion-completion-evidence",
            &canonical_json(&(
                REVIEW_SCHEMA_VERSION,
                &criterion_id,
                &manifest.repository_revision,
                &accepted_plan_hash,
                &manifest.manifest_id,
                &manifest.diff_hash,
                &manifest.plan_assessment.assessment_hash,
                &target_path_indexes,
                &validation_expectation_ids_by_target,
                &ancestry.required_validation_proof_id,
            ))?,
        ]);
        Ok(Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            criterion_id,
            repository_revision: manifest.repository_revision.clone(),
            accepted_plan_hash,
            manifest_id: manifest.manifest_id.clone(),
            diff_hash: manifest.diff_hash.clone(),
            plan_assessment_hash: manifest.plan_assessment.assessment_hash.clone(),
            target_path_indexes,
            validation_expectation_ids_by_target,
            required_validation_proof_id: ancestry.required_validation_proof_id.clone(),
            evidence_hash,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CriterionCompletionStatusV1 {
    Satisfied {
        supporting_evidence_ids: BTreeSet<EvidenceId>,
    },
    ExternalReviewRequired {
        kind: ExternalReviewKindV1,
        requirement_code: String,
        detail_hash: String,
    },
    Unsatisfied {
        reason_code: String,
        missing_evidence_ids: BTreeSet<EvidenceId>,
    },
    Uncertain {
        reason_code: String,
    },
}

impl CriterionCompletionStatusV1 {
    fn validate(
        &self,
        criterion_id: &DiscoveryCriterionId,
        context: &ReviewContextManifestV1,
        policy: &FinalizationPolicyV1,
        deterministic_evidence: &CriterionCompletionEvidenceV1,
    ) -> Result<(), ReviewContractError> {
        if &deterministic_evidence.criterion_id != criterion_id {
            return Err(ReviewContractError::Invalid {
                code: "completion_deterministic_evidence_criterion_mismatch",
            });
        }
        match self {
            Self::Satisfied {
                supporting_evidence_ids,
            } => {
                if policy.external_review_criteria.contains_key(criterion_id) {
                    return Err(ReviewContractError::Invalid {
                        code: "completion_external_review_required",
                    });
                }
                if !deterministic_evidence.is_complete()
                    || supporting_evidence_ids.is_empty()
                    || supporting_evidence_ids.len() > MAX_SUPPORTING_EVIDENCE
                    || !supporting_evidence_ids.is_subset(&context.evidence_ids)
                {
                    return Err(ReviewContractError::Invalid {
                        code: "completion_satisfied_evidence_invalid",
                    });
                }
            }
            Self::ExternalReviewRequired {
                kind,
                requirement_code,
                detail_hash,
            } => {
                if !deterministic_evidence.is_complete()
                    || policy.external_review_criteria.get(criterion_id) != Some(kind)
                    || !safe_code_is_valid(requirement_code)
                    || !is_sha256(detail_hash)
                {
                    return Err(ReviewContractError::Invalid {
                        code: "completion_external_review_not_authorized",
                    });
                }
            }
            Self::Unsatisfied {
                reason_code,
                missing_evidence_ids,
            } => {
                if !safe_code_is_valid(reason_code)
                    || missing_evidence_ids.len() > MAX_SUPPORTING_EVIDENCE
                {
                    return Err(ReviewContractError::Invalid {
                        code: "completion_unsatisfied_record_invalid",
                    });
                }
            }
            Self::Uncertain { reason_code } => {
                if !safe_code_is_valid(reason_code) {
                    return Err(ReviewContractError::Invalid {
                        code: "completion_uncertain_record_invalid",
                    });
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CriterionCompletionEvaluationV1 {
    pub(crate) criterion_id: DiscoveryCriterionId,
    pub(crate) deterministic_evidence: CriterionCompletionEvidenceV1,
    pub(crate) status: CriterionCompletionStatusV1,
    pub(crate) evaluation_hash: String,
}

impl CriterionCompletionEvaluationV1 {
    pub(crate) fn new(
        criterion_id: DiscoveryCriterionId,
        status: CriterionCompletionStatusV1,
        context: &ReviewContextManifestV1,
        policy: &FinalizationPolicyV1,
        plan: &AcceptedPlan,
        manifest: &DiffManifestV1,
        ancestry: &EngineeringAncestryV1,
    ) -> Result<Self, ReviewContractError> {
        let deterministic_evidence =
            CriterionCompletionEvidenceV1::derive(criterion_id.clone(), plan, manifest, ancestry)?;
        status.validate(&criterion_id, context, policy, &deterministic_evidence)?;
        let evaluation_hash = Self::expected_hash(&criterion_id, &deterministic_evidence, &status)?;
        let evaluation = Self {
            criterion_id,
            deterministic_evidence,
            status,
            evaluation_hash,
        };
        evaluation.validate(context, policy, plan, manifest, ancestry)?;
        Ok(evaluation)
    }

    pub(crate) fn validate(
        &self,
        context: &ReviewContextManifestV1,
        policy: &FinalizationPolicyV1,
        plan: &AcceptedPlan,
        manifest: &DiffManifestV1,
        ancestry: &EngineeringAncestryV1,
    ) -> Result<(), ReviewContractError> {
        self.deterministic_evidence
            .validate_against(plan, manifest, ancestry)?;
        self.status.validate(
            &self.criterion_id,
            context,
            policy,
            &self.deterministic_evidence,
        )?;
        if !context.criterion_ids.contains(&self.criterion_id)
            || self.evaluation_hash
                != Self::expected_hash(
                    &self.criterion_id,
                    &self.deterministic_evidence,
                    &self.status,
                )?
        {
            return Err(ReviewContractError::Invalid {
                code: "criterion_completion_evaluation_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(
        criterion_id: &DiscoveryCriterionId,
        deterministic_evidence: &CriterionCompletionEvidenceV1,
        status: &CriterionCompletionStatusV1,
    ) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:criterion-completion",
            &canonical_json(&(criterion_id, deterministic_evidence, status))?,
        ]))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionDispositionV1 {
    Complete,
    CompletePendingExternalReview,
    Incomplete,
}

impl CompletionDispositionV1 {
    pub(crate) const fn permits(self, mode: PublicationModeV1) -> bool {
        matches!(
            (self, mode),
            (Self::Complete, PublicationModeV1::Normal)
                | (
                    Self::CompletePendingExternalReview,
                    PublicationModeV1::NormalWithExternalReview
                )
        )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompletionEvaluationV1 {
    pub(crate) schema_version: u16,
    pub(crate) evaluation_id: CompletionEvaluationId,
    pub(crate) action_id: ActionId,
    pub(crate) call_id: ModelCallId,
    pub(crate) node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) required_validation_proof_id: ProofId,
    pub(crate) manifest_id: DiffManifestId,
    pub(crate) diff_review_id: DiffReviewId,
    pub(crate) criteria: BTreeMap<DiscoveryCriterionId, CriterionCompletionEvaluationV1>,
    pub(crate) disposition: CompletionDispositionV1,
    pub(crate) evaluation_hash: String,
}

impl CompletionEvaluationV1 {
    pub(crate) fn new(
        prepared: &PreparedReviewActionV1,
        manifest: &DiffManifestV1,
        review: &DiffReviewV1,
        policy: &FinalizationPolicyV1,
        plan: &AcceptedPlan,
        ancestry: &EngineeringAncestryV1,
        criteria: BTreeMap<DiscoveryCriterionId, CriterionCompletionEvaluationV1>,
    ) -> Result<Self, ReviewContractError> {
        let ReviewContextBindingV1::Completion {
            manifest_id,
            diff_hash,
            diff_review_id,
            page_review_ids,
        } = &prepared.context.binding
        else {
            return Err(ReviewContractError::Invalid {
                code: "completion_evaluation_wrong_action",
            });
        };
        if review.disposition != DiffReviewDispositionV1::Accepted
            || !manifest.plan_assessment.is_safe_and_complete()
            || manifest_id != &manifest.manifest_id
            || diff_hash != &manifest.diff_hash
            || diff_review_id != &review.review_id
            || page_review_ids != &review.ordered_page_review_ids
            || prepared.context.repository_revision != manifest.repository_revision
            || review.repository_revision != manifest.repository_revision
            || prepared.context.policy_id != policy.policy_id
            || prepared.context.plan_id != plan.plan_id
            || prepared.context.plan_revision_id != plan.plan_revision_id
            || prepared.context.required_validation_proof_id
                != ancestry.required_validation_proof_id
            || criteria.keys().cloned().collect::<BTreeSet<_>>() != prepared.context.criterion_ids
            || criteria.len() > MAX_CRITERIA
            || criteria.iter().any(|(criterion_id, evaluation)| {
                criterion_id != &evaluation.criterion_id
                    || evaluation
                        .validate(&prepared.context, policy, plan, manifest, ancestry)
                        .is_err()
            })
        {
            return Err(ReviewContractError::Invalid {
                code: "completion_evaluation_binding_mismatch",
            });
        }
        let disposition = completion_disposition(&criteria);
        let evaluation_hash = Self::expected_hash(
            &prepared.envelope.action_id,
            &prepared.envelope.call_id,
            &prepared.envelope.node_id,
            &manifest.repository_revision,
            &prepared.context.plan_id,
            &prepared.context.plan_revision_id,
            &prepared.context.required_validation_proof_id,
            &manifest.manifest_id,
            &review.review_id,
            &criteria,
            disposition,
        )?;
        let evaluation = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            evaluation_id: CompletionEvaluationId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:completion-evaluation",
                    prepared.envelope.action_id.as_str(),
                    review.review_id.as_str(),
                    &evaluation_hash,
                ])
            )),
            action_id: prepared.envelope.action_id.clone(),
            call_id: prepared.envelope.call_id.clone(),
            node_id: prepared.envelope.node_id.clone(),
            repository_revision: manifest.repository_revision.clone(),
            plan_id: prepared.context.plan_id.clone(),
            plan_revision_id: prepared.context.plan_revision_id.clone(),
            required_validation_proof_id: prepared.context.required_validation_proof_id.clone(),
            manifest_id: manifest.manifest_id.clone(),
            diff_review_id: review.review_id.clone(),
            criteria,
            disposition,
            evaluation_hash,
        };
        evaluation.validate_against(prepared, manifest, review, policy, plan, ancestry)?;
        Ok(evaluation)
    }

    pub(crate) fn validate_against(
        &self,
        prepared: &PreparedReviewActionV1,
        manifest: &DiffManifestV1,
        review: &DiffReviewV1,
        policy: &FinalizationPolicyV1,
        plan: &AcceptedPlan,
        ancestry: &EngineeringAncestryV1,
    ) -> Result<(), ReviewContractError> {
        let ReviewContextBindingV1::Completion {
            manifest_id,
            diff_hash,
            diff_review_id,
            page_review_ids,
        } = &prepared.context.binding
        else {
            return Err(ReviewContractError::Invalid {
                code: "completion_evaluation_wrong_action",
            });
        };
        let expected_disposition = completion_disposition(&self.criteria);
        let expected_hash = Self::expected_hash(
            &self.action_id,
            &self.call_id,
            &self.node_id,
            &self.repository_revision,
            &self.plan_id,
            &self.plan_revision_id,
            &self.required_validation_proof_id,
            &self.manifest_id,
            &self.diff_review_id,
            &self.criteria,
            self.disposition,
        )?;
        let expected_id = CompletionEvaluationId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:completion-evaluation",
                self.action_id.as_str(),
                self.diff_review_id.as_str(),
                &expected_hash,
            ])
        ));
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || review.disposition != DiffReviewDispositionV1::Accepted
            || !manifest.plan_assessment.is_safe_and_complete()
            || manifest_id != &manifest.manifest_id
            || diff_hash != &manifest.diff_hash
            || diff_review_id != &review.review_id
            || page_review_ids != &review.ordered_page_review_ids
            || self.action_id != prepared.envelope.action_id
            || self.call_id != prepared.envelope.call_id
            || self.node_id != prepared.envelope.node_id
            || self.repository_revision != manifest.repository_revision
            || self.plan_id != prepared.context.plan_id
            || self.plan_revision_id != prepared.context.plan_revision_id
            || self.required_validation_proof_id != prepared.context.required_validation_proof_id
            || self.manifest_id != manifest.manifest_id
            || self.diff_review_id != review.review_id
            || prepared.context.policy_id != policy.policy_id
            || prepared.context.plan_id != plan.plan_id
            || prepared.context.plan_revision_id != plan.plan_revision_id
            || self.required_validation_proof_id != ancestry.required_validation_proof_id
            || self.criteria.keys().cloned().collect::<BTreeSet<_>>()
                != prepared.context.criterion_ids
            || self.criteria.len() > MAX_CRITERIA
            || self.criteria.iter().any(|(criterion_id, evaluation)| {
                criterion_id != &evaluation.criterion_id
                    || evaluation
                        .validate(&prepared.context, policy, plan, manifest, ancestry)
                        .is_err()
            })
            || self.disposition != expected_disposition
            || self.evaluation_hash != expected_hash
            || self.evaluation_id != expected_id
        {
            return Err(ReviewContractError::Invalid {
                code: "completion_evaluation_invalid",
            });
        }
        Ok(())
    }

    pub(crate) fn external_review_reason_code(&self) -> Option<&'static str> {
        (self.disposition == CompletionDispositionV1::CompletePendingExternalReview)
            .then_some("completion_pending_external_review")
    }

    #[allow(clippy::too_many_arguments)]
    fn expected_hash(
        action_id: &ActionId,
        call_id: &ModelCallId,
        node_id: &NodeId,
        repository_revision: &RepositoryRevisionId,
        plan_id: &PlanId,
        plan_revision_id: &PlanRevisionId,
        required_validation_proof_id: &ProofId,
        manifest_id: &DiffManifestId,
        diff_review_id: &DiffReviewId,
        criteria: &BTreeMap<DiscoveryCriterionId, CriterionCompletionEvaluationV1>,
        disposition: CompletionDispositionV1,
    ) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:completion-evaluation-record",
            &canonical_json(&(
                action_id,
                call_id,
                node_id,
                repository_revision,
                plan_id,
                plan_revision_id,
                required_validation_proof_id,
                manifest_id,
                diff_review_id,
                criteria,
                disposition,
            ))?,
        ]))
    }
}

fn completion_disposition(
    criteria: &BTreeMap<DiscoveryCriterionId, CriterionCompletionEvaluationV1>,
) -> CompletionDispositionV1 {
    let has_external = criteria.values().any(|evaluation| {
        matches!(
            &evaluation.status,
            CriterionCompletionStatusV1::ExternalReviewRequired { .. }
        )
    });
    let all_resolved = criteria.values().all(|evaluation| {
        matches!(
            &evaluation.status,
            CriterionCompletionStatusV1::Satisfied { .. }
                | CriterionCompletionStatusV1::ExternalReviewRequired { .. }
        )
    });
    if !all_resolved {
        CompletionDispositionV1::Incomplete
    } else if has_external {
        CompletionDispositionV1::CompletePendingExternalReview
    } else {
        CompletionDispositionV1::Complete
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationAuthorityRequestV1 {
    pub(crate) schema_version: u16,
    pub(crate) effect_id: EffectId,
    pub(crate) policy_id: FinalizationPolicyId,
    pub(crate) contract_id: EvidenceId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) completion_evaluation_id: CompletionEvaluationId,
    pub(crate) request_hash: String,
}

impl PublicationAuthorityRequestV1 {
    pub(crate) fn new(
        policy: &FinalizationPolicyV1,
        completion: &CompletionEvaluationV1,
    ) -> Result<Self, ReviewContractError> {
        policy.validate()?;
        let identity = canonical_json(&(
            REVIEW_SCHEMA_VERSION,
            &policy.policy_id,
            &policy.publication.contract_id,
            &completion.repository_revision,
            &completion.evaluation_id,
        ))?;
        let mut request = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            effect_id: EffectId::new(format!(
                "epv1:{}",
                stable_sha256(&["execution-protocol-v1:publication-authority", &identity])
            )),
            policy_id: policy.policy_id.clone(),
            contract_id: policy.publication.contract_id.clone(),
            repository_revision: completion.repository_revision.clone(),
            completion_evaluation_id: completion.evaluation_id.clone(),
            request_hash: String::new(),
        };
        request.request_hash = request.expected_hash()?;
        request.validate_against(policy, completion)?;
        Ok(request)
    }

    pub(crate) fn validate_against(
        &self,
        policy: &FinalizationPolicyV1,
        completion: &CompletionEvaluationV1,
    ) -> Result<(), ReviewContractError> {
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || self.policy_id != policy.policy_id
            || self.contract_id != policy.publication.contract_id
            || self.repository_revision != completion.repository_revision
            || self.completion_evaluation_id != completion.evaluation_id
            || self.request_hash != self.expected_hash()?
        {
            return Err(ReviewContractError::Invalid {
                code: "publication_authority_request_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(&self) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:publication-authority-request",
            &canonical_json(&(
                self.schema_version,
                &self.effect_id,
                &self.policy_id,
                &self.contract_id,
                &self.repository_revision,
                &self.completion_evaluation_id,
            ))?,
        ]))
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PublicationAuthorityEffectFailureReasonV1 {
    AuthorityUnavailable { safe_code: String },
}

impl PublicationAuthorityEffectFailureReasonV1 {
    fn validate(&self) -> Result<(), ReviewContractError> {
        match self {
            Self::AuthorityUnavailable { safe_code } if safe_code_is_valid(safe_code) => Ok(()),
            Self::AuthorityUnavailable { .. } => Err(ReviewContractError::Invalid {
                code: "publication_authority_effect_failure_reason_invalid",
            }),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationAuthorityEffectFailureV1 {
    pub(crate) schema_version: u16,
    pub(crate) failure_id: PublicationAuthorityFailureId,
    pub(crate) effect_id: EffectId,
    pub(crate) request_hash: String,
    pub(crate) policy_id: FinalizationPolicyId,
    pub(crate) contract_id: EvidenceId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) completion_evaluation_id: CompletionEvaluationId,
    pub(crate) reason: PublicationAuthorityEffectFailureReasonV1,
    pub(crate) failure_hash: String,
}

impl PublicationAuthorityEffectFailureV1 {
    pub(crate) fn new(
        request: &PublicationAuthorityRequestV1,
        reason: PublicationAuthorityEffectFailureReasonV1,
    ) -> Result<Self, ReviewContractError> {
        reason.validate()?;
        let failure_hash = Self::expected_hash(request, &reason)?;
        let failure = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            failure_id: PublicationAuthorityFailureId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:publication-authority-effect-failure",
                    request.effect_id.as_str(),
                    &failure_hash,
                ])
            )),
            effect_id: request.effect_id.clone(),
            request_hash: request.request_hash.clone(),
            policy_id: request.policy_id.clone(),
            contract_id: request.contract_id.clone(),
            repository_revision: request.repository_revision.clone(),
            completion_evaluation_id: request.completion_evaluation_id.clone(),
            reason,
            failure_hash,
        };
        failure.validate_against(request)?;
        Ok(failure)
    }

    pub(crate) fn validate_against(
        &self,
        request: &PublicationAuthorityRequestV1,
    ) -> Result<(), ReviewContractError> {
        self.reason.validate()?;
        let expected_hash = Self::expected_hash(request, &self.reason)?;
        let expected_id = PublicationAuthorityFailureId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:publication-authority-effect-failure",
                request.effect_id.as_str(),
                &expected_hash,
            ])
        ));
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || self.effect_id != request.effect_id
            || self.request_hash != request.request_hash
            || self.policy_id != request.policy_id
            || self.contract_id != request.contract_id
            || self.repository_revision != request.repository_revision
            || self.completion_evaluation_id != request.completion_evaluation_id
            || self.failure_hash != expected_hash
            || self.failure_id != expected_id
        {
            return Err(ReviewContractError::Invalid {
                code: "publication_authority_effect_failure_invalid",
            });
        }
        Ok(())
    }

    pub(crate) fn convergence_reason(&self) -> ReviewConvergenceReasonV1 {
        match &self.reason {
            PublicationAuthorityEffectFailureReasonV1::AuthorityUnavailable { safe_code } => {
                ReviewConvergenceReasonV1::PublicationAuthorityUnavailable {
                    failure_id: self.failure_id.clone(),
                    failure_hash: self.failure_hash.clone(),
                    safe_code: safe_code.clone(),
                }
            }
        }
    }

    fn expected_hash(
        request: &PublicationAuthorityRequestV1,
        reason: &PublicationAuthorityEffectFailureReasonV1,
    ) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:publication-authority-effect-failure-record",
            &canonical_json(&(
                REVIEW_SCHEMA_VERSION,
                &request.effect_id,
                &request.request_hash,
                &request.policy_id,
                &request.contract_id,
                &request.repository_revision,
                &request.completion_evaluation_id,
                reason,
            ))?,
        ]))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationAuthorityObservationV1 {
    pub(crate) schema_version: u16,
    pub(crate) authority_id: PublicationAuthorityId,
    pub(crate) effect_id: EffectId,
    pub(crate) policy_id: FinalizationPolicyId,
    pub(crate) contract_id: EvidenceId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) repository_head_oid: String,
    pub(crate) repository_tree_oid: String,
    pub(crate) repository_binding_hash: String,
    pub(crate) installation_binding_hash: String,
    pub(crate) base_repository_revision: RepositoryRevisionId,
    pub(crate) base_ref: String,
    pub(crate) head_branch: String,
    pub(crate) expected_remote_head: Option<String>,
    pub(crate) observed_remote_head: Option<String>,
    pub(crate) lease_epoch_hash: String,
    pub(crate) cancellation_absent: bool,
    pub(crate) lease_valid: bool,
    pub(crate) remote_head_unchanged: bool,
    pub(crate) observation_hash: String,
}

impl PublicationAuthorityObservationV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        request: &PublicationAuthorityRequestV1,
        repository_head_oid: String,
        repository_tree_oid: String,
        repository_binding_hash: String,
        installation_binding_hash: String,
        base_repository_revision: RepositoryRevisionId,
        base_ref: String,
        head_branch: String,
        expected_remote_head: Option<String>,
        observed_remote_head: Option<String>,
        lease_epoch_hash: String,
        cancellation_absent: bool,
        lease_valid: bool,
    ) -> Result<Self, ReviewContractError> {
        let remote_head_unchanged = expected_remote_head == observed_remote_head;
        let observation_hash = Self::expected_hash(
            &request.effect_id,
            &request.policy_id,
            &request.contract_id,
            &request.repository_revision,
            &repository_head_oid,
            &repository_tree_oid,
            &repository_binding_hash,
            &installation_binding_hash,
            &base_repository_revision,
            &base_ref,
            &head_branch,
            &expected_remote_head,
            &observed_remote_head,
            &lease_epoch_hash,
            cancellation_absent,
            lease_valid,
            remote_head_unchanged,
        )?;
        let observation = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            authority_id: PublicationAuthorityId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:publication-authority-observation",
                    request.effect_id.as_str(),
                    &observation_hash,
                ])
            )),
            effect_id: request.effect_id.clone(),
            policy_id: request.policy_id.clone(),
            contract_id: request.contract_id.clone(),
            repository_revision: request.repository_revision.clone(),
            repository_head_oid,
            repository_tree_oid,
            repository_binding_hash,
            installation_binding_hash,
            base_repository_revision,
            base_ref,
            head_branch,
            expected_remote_head,
            observed_remote_head,
            lease_epoch_hash,
            cancellation_absent,
            lease_valid,
            remote_head_unchanged,
            observation_hash,
        };
        observation.validate_against(request)?;
        Ok(observation)
    }

    pub(crate) fn validate_against(
        &self,
        request: &PublicationAuthorityRequestV1,
    ) -> Result<(), ReviewContractError> {
        let expected_hash = Self::expected_hash(
            &self.effect_id,
            &self.policy_id,
            &self.contract_id,
            &self.repository_revision,
            &self.repository_head_oid,
            &self.repository_tree_oid,
            &self.repository_binding_hash,
            &self.installation_binding_hash,
            &self.base_repository_revision,
            &self.base_ref,
            &self.head_branch,
            &self.expected_remote_head,
            &self.observed_remote_head,
            &self.lease_epoch_hash,
            self.cancellation_absent,
            self.lease_valid,
            self.remote_head_unchanged,
        )?;
        let expected_id = PublicationAuthorityId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:publication-authority-observation",
                self.effect_id.as_str(),
                &expected_hash,
            ])
        ));
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || self.effect_id != request.effect_id
            || self.policy_id != request.policy_id
            || self.contract_id != request.contract_id
            || self.repository_revision != request.repository_revision
            || !git_oid_is_valid(&self.repository_head_oid)
            || !git_oid_is_valid(&self.repository_tree_oid)
            || !is_sha256(&self.repository_binding_hash)
            || !is_sha256(&self.installation_binding_hash)
            || !git_ref_is_valid(&self.base_ref)
            || !git_ref_is_valid(&self.head_branch)
            || self
                .expected_remote_head
                .iter()
                .chain(self.observed_remote_head.iter())
                .any(|head| !git_oid_is_valid(head))
            || !is_sha256(&self.lease_epoch_hash)
            || self.remote_head_unchanged
                != (self.expected_remote_head == self.observed_remote_head)
            || self.observation_hash != expected_hash
            || self.authority_id != expected_id
        {
            return Err(ReviewContractError::Invalid {
                code: "publication_authority_observation_invalid",
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn expected_hash(
        effect_id: &EffectId,
        policy_id: &FinalizationPolicyId,
        contract_id: &EvidenceId,
        repository_revision: &RepositoryRevisionId,
        repository_head_oid: &str,
        repository_tree_oid: &str,
        repository_binding_hash: &str,
        installation_binding_hash: &str,
        base_repository_revision: &RepositoryRevisionId,
        base_ref: &str,
        head_branch: &str,
        expected_remote_head: &Option<String>,
        observed_remote_head: &Option<String>,
        lease_epoch_hash: &str,
        cancellation_absent: bool,
        lease_valid: bool,
        remote_head_unchanged: bool,
    ) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:publication-authority-observation-record",
            &canonical_json(&(
                (
                    effect_id,
                    policy_id,
                    contract_id,
                    repository_revision,
                    repository_head_oid,
                    repository_tree_oid,
                    repository_binding_hash,
                    installation_binding_hash,
                ),
                (
                    base_repository_revision,
                    base_ref,
                    head_branch,
                    expected_remote_head,
                    observed_remote_head,
                    lease_epoch_hash,
                    cancellation_absent,
                    lease_valid,
                    remote_head_unchanged,
                ),
            ))?,
        ]))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationEligibilityFactsV1 {
    pub(crate) required_implementation_satisfied: bool,
    pub(crate) required_validation_current: bool,
    pub(crate) no_active_validation_failure: bool,
    pub(crate) no_active_work_or_reservation: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublicationPredicateV1 {
    CurrentRepositoryRevision,
    ImplementationBarrierAncestry,
    VerifiedChangesPresent,
    RequiredValidationCurrent,
    NoActiveValidationFailure,
    CompleteDiffReviewed,
    CompletionPermitsRequestedMode,
    SignedPublicationCoordinates,
    CancellationAbsent,
    LeaseValid,
    RemoteHeadUnchanged,
    NoActiveWorkOrReservation,
}

impl PublicationPredicateV1 {
    const ALL: [Self; 12] = [
        Self::CurrentRepositoryRevision,
        Self::ImplementationBarrierAncestry,
        Self::VerifiedChangesPresent,
        Self::RequiredValidationCurrent,
        Self::NoActiveValidationFailure,
        Self::CompleteDiffReviewed,
        Self::CompletionPermitsRequestedMode,
        Self::SignedPublicationCoordinates,
        Self::CancellationAbsent,
        Self::LeaseValid,
        Self::RemoteHeadUnchanged,
        Self::NoActiveWorkOrReservation,
    ];
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PublicationPredicateResultV1 {
    Passed,
    Failed { code: String },
}

impl PublicationPredicateResultV1 {
    fn failed(code: &'static str) -> Self {
        Self::Failed { code: code.into() }
    }

    fn is_passed(&self) -> bool {
        matches!(self, Self::Passed)
    }

    fn validate(&self) -> bool {
        match self {
            Self::Passed => true,
            Self::Failed { code } => safe_code_is_valid(code),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "disposition", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PublicationEligibilityDispositionV1 {
    Granted,
    Denied {
        failed_predicates: BTreeSet<PublicationPredicateV1>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationEligibilityRecord {
    pub(crate) schema_version: u16,
    pub(crate) eligibility_id: PublicationEligibilityId,
    pub(crate) policy_id: FinalizationPolicyId,
    pub(crate) contract_id: EvidenceId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) requested_mode: PublicationModeV1,
    pub(crate) implementation_barrier_proof_id: ProofId,
    pub(crate) required_validation_proof_id: ProofId,
    pub(crate) review_proof_id: ProofId,
    pub(crate) completion_proof_id: ProofId,
    pub(crate) manifest_id: DiffManifestId,
    pub(crate) diff_review_id: DiffReviewId,
    pub(crate) completion_evaluation_id: CompletionEvaluationId,
    pub(crate) authority_id: PublicationAuthorityId,
    pub(crate) ancestry_hash: String,
    pub(crate) facts: PublicationEligibilityFactsV1,
    pub(crate) predicates: BTreeMap<PublicationPredicateV1, PublicationPredicateResultV1>,
    pub(crate) disposition: PublicationEligibilityDispositionV1,
    pub(crate) decision_hash: String,
}

impl PublicationEligibilityRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        policy: &FinalizationPolicyV1,
        ancestry: &EngineeringAncestryV1,
        manifest: &DiffManifestV1,
        review: &DiffReviewV1,
        review_proof_id: ProofId,
        completion: &CompletionEvaluationV1,
        completion_proof_id: ProofId,
        authority: &PublicationAuthorityObservationV1,
        facts: PublicationEligibilityFactsV1,
    ) -> Result<Self, ReviewContractError> {
        policy.validate()?;
        ancestry.validate()?;
        let predicates = expected_publication_predicates(
            policy, ancestry, manifest, review, completion, authority, &facts,
        )?;
        let failed_predicates = predicates
            .iter()
            .filter_map(|(predicate, result)| (!result.is_passed()).then_some(*predicate))
            .collect::<BTreeSet<_>>();
        let disposition = if failed_predicates.is_empty() {
            PublicationEligibilityDispositionV1::Granted
        } else {
            PublicationEligibilityDispositionV1::Denied { failed_predicates }
        };
        let decision_hash = Self::expected_hash(
            &policy.policy_id,
            &policy.publication.contract_id,
            &manifest.repository_revision,
            policy.publication.requested_mode,
            ancestry,
            &review_proof_id,
            &completion_proof_id,
            manifest,
            review,
            completion,
            authority,
            &facts,
            &predicates,
            &disposition,
        )?;
        let record = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            eligibility_id: PublicationEligibilityId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:publication-eligibility",
                    policy.policy_id.as_str(),
                    manifest.repository_revision.as_str(),
                    &decision_hash,
                ])
            )),
            policy_id: policy.policy_id.clone(),
            contract_id: policy.publication.contract_id.clone(),
            repository_revision: manifest.repository_revision.clone(),
            requested_mode: policy.publication.requested_mode,
            implementation_barrier_proof_id: ancestry.implementation_barrier_proof_id.clone(),
            required_validation_proof_id: ancestry.required_validation_proof_id.clone(),
            review_proof_id,
            completion_proof_id,
            manifest_id: manifest.manifest_id.clone(),
            diff_review_id: review.review_id.clone(),
            completion_evaluation_id: completion.evaluation_id.clone(),
            authority_id: authority.authority_id.clone(),
            ancestry_hash: ancestry.ancestry_hash.clone(),
            facts,
            predicates,
            disposition,
            decision_hash,
        };
        record.validate_against(policy, ancestry, manifest, review, completion, authority)?;
        Ok(record)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn validate_against(
        &self,
        policy: &FinalizationPolicyV1,
        ancestry: &EngineeringAncestryV1,
        manifest: &DiffManifestV1,
        review: &DiffReviewV1,
        completion: &CompletionEvaluationV1,
        authority: &PublicationAuthorityObservationV1,
    ) -> Result<(), ReviewContractError> {
        policy.validate()?;
        ancestry.validate()?;
        let expected_predicates = expected_publication_predicates(
            policy,
            ancestry,
            manifest,
            review,
            completion,
            authority,
            &self.facts,
        )?;
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || self.policy_id != policy.policy_id
            || self.contract_id != policy.publication.contract_id
            || self.repository_revision != manifest.repository_revision
            || self.requested_mode != policy.publication.requested_mode
            || self.implementation_barrier_proof_id != ancestry.implementation_barrier_proof_id
            || self.required_validation_proof_id != ancestry.required_validation_proof_id
            || self.manifest_id != manifest.manifest_id
            || self.diff_review_id != review.review_id
            || self.completion_evaluation_id != completion.evaluation_id
            || self.authority_id != authority.authority_id
            || self.ancestry_hash != ancestry.ancestry_hash
            || self.predicates != expected_predicates
            || self.predicates.values().any(|result| !result.validate())
        {
            return Err(ReviewContractError::Invalid {
                code: "publication_eligibility_record_binding_mismatch",
            });
        }
        let failed_predicates = self
            .predicates
            .iter()
            .filter_map(|(predicate, result)| (!result.is_passed()).then_some(*predicate))
            .collect::<BTreeSet<_>>();
        let expected_disposition = if failed_predicates.is_empty() {
            PublicationEligibilityDispositionV1::Granted
        } else {
            PublicationEligibilityDispositionV1::Denied { failed_predicates }
        };
        let expected_hash = Self::expected_hash(
            &self.policy_id,
            &self.contract_id,
            &self.repository_revision,
            self.requested_mode,
            ancestry,
            &self.review_proof_id,
            &self.completion_proof_id,
            manifest,
            review,
            completion,
            authority,
            &self.facts,
            &self.predicates,
            &self.disposition,
        )?;
        let expected_id = PublicationEligibilityId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:publication-eligibility",
                self.policy_id.as_str(),
                self.repository_revision.as_str(),
                &expected_hash,
            ])
        ));
        if self.disposition != expected_disposition
            || self.decision_hash != expected_hash
            || self.eligibility_id != expected_id
        {
            return Err(ReviewContractError::Invalid {
                code: "publication_eligibility_record_invalid",
            });
        }
        Ok(())
    }

    pub(crate) fn is_granted(&self) -> bool {
        matches!(
            self.disposition,
            PublicationEligibilityDispositionV1::Granted
        )
    }

    pub(crate) fn validate_for_publication(
        &self,
        contract: &PublicationContractV1,
        repository_revision: &RepositoryRevisionId,
    ) -> Result<(), ReviewContractError> {
        contract.validate()?;
        if !self.is_granted()
            || self.contract_id != contract.contract_id
            || self.requested_mode != contract.requested_mode
            || &self.repository_revision != repository_revision
        {
            return Err(ReviewContractError::Invalid {
                code: "publication_eligibility_not_granted",
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn expected_hash(
        policy_id: &FinalizationPolicyId,
        contract_id: &EvidenceId,
        repository_revision: &RepositoryRevisionId,
        requested_mode: PublicationModeV1,
        ancestry: &EngineeringAncestryV1,
        review_proof_id: &ProofId,
        completion_proof_id: &ProofId,
        manifest: &DiffManifestV1,
        review: &DiffReviewV1,
        completion: &CompletionEvaluationV1,
        authority: &PublicationAuthorityObservationV1,
        facts: &PublicationEligibilityFactsV1,
        predicates: &BTreeMap<PublicationPredicateV1, PublicationPredicateResultV1>,
        disposition: &PublicationEligibilityDispositionV1,
    ) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:publication-eligibility-decision",
            &canonical_json(&(
                policy_id,
                contract_id,
                repository_revision,
                requested_mode,
                (
                    &ancestry.implementation_barrier_proof_id,
                    &ancestry.required_validation_proof_id,
                    &ancestry.ancestry_hash,
                ),
                (review_proof_id, completion_proof_id),
                (&manifest.manifest_id, &manifest.diff_hash),
                (
                    &review.review_id,
                    &completion.evaluation_id,
                    &authority.authority_id,
                ),
                facts,
                predicates,
                disposition,
            ))?,
        ]))
    }
}

#[allow(clippy::too_many_arguments)]
fn expected_publication_predicates(
    policy: &FinalizationPolicyV1,
    ancestry: &EngineeringAncestryV1,
    manifest: &DiffManifestV1,
    review: &DiffReviewV1,
    completion: &CompletionEvaluationV1,
    authority: &PublicationAuthorityObservationV1,
    facts: &PublicationEligibilityFactsV1,
) -> Result<BTreeMap<PublicationPredicateV1, PublicationPredicateResultV1>, ReviewContractError> {
    let completion_bound = completion.manifest_id == manifest.manifest_id
        && completion.diff_review_id == review.review_id
        && completion.required_validation_proof_id == ancestry.required_validation_proof_id;
    if !completion_bound {
        return Err(ReviewContractError::Invalid {
            code: "publication_eligibility_completion_binding_mismatch",
        });
    }

    let records_current = ancestry.repository_revision == manifest.repository_revision
        && ancestry.repository_fingerprint == manifest.repository_fingerprint
        && review.repository_revision == manifest.repository_revision
        && completion.repository_revision == manifest.repository_revision
        && authority.repository_revision == manifest.repository_revision;
    let review_complete = review.manifest_id == manifest.manifest_id
        && review.diff_hash == manifest.diff_hash
        && review.disposition == DiffReviewDispositionV1::Accepted;
    let coordinates_match = authority.policy_id == policy.policy_id
        && authority.contract_id == policy.publication.contract_id
        && authority.repository_binding_hash == policy.publication.repository_binding_hash
        && authority.installation_binding_hash == policy.publication.installation_binding_hash
        && authority.base_repository_revision == policy.publication.base_repository_revision
        && authority.base_ref == policy.publication.base_ref
        && authority.head_branch == policy.publication.head_branch
        && authority.expected_remote_head == policy.publication.expected_remote_head;

    let mut predicates = BTreeMap::new();
    insert_predicate(
        &mut predicates,
        PublicationPredicateV1::CurrentRepositoryRevision,
        records_current,
        "publication_repository_revision_stale",
    );
    insert_predicate(
        &mut predicates,
        PublicationPredicateV1::ImplementationBarrierAncestry,
        facts.required_implementation_satisfied,
        "publication_implementation_barrier_missing",
    );
    insert_predicate(
        &mut predicates,
        PublicationPredicateV1::VerifiedChangesPresent,
        !manifest.is_empty(),
        "publication_diff_empty",
    );
    insert_predicate(
        &mut predicates,
        PublicationPredicateV1::RequiredValidationCurrent,
        facts.required_validation_current
            && manifest.required_validation_proof_id == ancestry.required_validation_proof_id,
        "publication_validation_stale",
    );
    insert_predicate(
        &mut predicates,
        PublicationPredicateV1::NoActiveValidationFailure,
        facts.no_active_validation_failure,
        "publication_validation_failure_active",
    );
    insert_predicate(
        &mut predicates,
        PublicationPredicateV1::CompleteDiffReviewed,
        review_complete,
        "publication_diff_review_incomplete",
    );
    insert_predicate(
        &mut predicates,
        PublicationPredicateV1::CompletionPermitsRequestedMode,
        completion
            .disposition
            .permits(policy.publication.requested_mode),
        "publication_completion_mode_denied",
    );
    insert_predicate(
        &mut predicates,
        PublicationPredicateV1::SignedPublicationCoordinates,
        coordinates_match,
        "publication_coordinates_mismatch",
    );
    insert_predicate(
        &mut predicates,
        PublicationPredicateV1::CancellationAbsent,
        authority.cancellation_absent,
        "publication_cancellation_observed",
    );
    insert_predicate(
        &mut predicates,
        PublicationPredicateV1::LeaseValid,
        authority.lease_valid,
        "publication_lease_invalid",
    );
    insert_predicate(
        &mut predicates,
        PublicationPredicateV1::RemoteHeadUnchanged,
        authority.remote_head_unchanged
            && policy
                .publication
                .expected_remote_head
                .as_ref()
                .is_none_or(|expected| expected == &authority.repository_head_oid),
        "publication_remote_head_moved",
    );
    insert_predicate(
        &mut predicates,
        PublicationPredicateV1::NoActiveWorkOrReservation,
        facts.no_active_work_or_reservation,
        "publication_work_or_reservation_active",
    );
    debug_assert_eq!(predicates.len(), PublicationPredicateV1::ALL.len());
    Ok(predicates)
}

fn insert_predicate(
    predicates: &mut BTreeMap<PublicationPredicateV1, PublicationPredicateResultV1>,
    predicate: PublicationPredicateV1,
    passed: bool,
    failure_code: &'static str,
) {
    predicates.insert(
        predicate,
        if passed {
            PublicationPredicateResultV1::Passed
        } else {
            PublicationPredicateResultV1::failed(failure_code)
        },
    );
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ReviewConvergenceReasonV1 {
    DiffManifestLimitExceeded {
        failure_id: DiffManifestFailureId,
        failure_hash: String,
        safe_code: String,
    },
    RepositoryDrift {
        failure_id: DiffManifestFailureId,
        failure_hash: String,
        observed_revision: RepositoryRevisionId,
    },
    ArtifactDurabilityFailed {
        failure_id: DiffManifestFailureId,
        failure_hash: String,
        safe_code: String,
    },
    ReviewBudgetExhausted {
        node_id: NodeId,
    },
    CompletionBudgetExhausted {
        node_id: NodeId,
    },
    ProviderProtocolExhausted {
        node_id: NodeId,
    },
    UncontactedReleaseRetryExhausted {
        node_id: NodeId,
        binding_hash: String,
        released_attempts: u32,
        ceiling: u32,
    },
    DiffReviewBlocked {
        review_id: DiffReviewId,
    },
    CompletionIncomplete {
        evaluation_id: CompletionEvaluationId,
    },
    PublicationAuthorityUnavailable {
        failure_id: PublicationAuthorityFailureId,
        failure_hash: String,
        safe_code: String,
    },
    PublicationEligibilityDenied {
        eligibility_id: PublicationEligibilityId,
    },
}

impl ReviewConvergenceReasonV1 {
    fn validate(&self) -> bool {
        match self {
            Self::DiffManifestLimitExceeded {
                failure_hash,
                safe_code,
                ..
            }
            | Self::ArtifactDurabilityFailed {
                failure_hash,
                safe_code,
                ..
            } => is_sha256(failure_hash) && safe_code_is_valid(safe_code),
            Self::RepositoryDrift { failure_hash, .. } => is_sha256(failure_hash),
            Self::PublicationAuthorityUnavailable {
                failure_hash,
                safe_code,
                ..
            } => is_sha256(failure_hash) && safe_code_is_valid(safe_code),
            Self::UncontactedReleaseRetryExhausted {
                binding_hash,
                released_attempts,
                ceiling,
                ..
            } => {
                is_sha256(binding_hash)
                    && *ceiling > 0
                    && *ceiling <= MAX_UNCONTACTED_RELEASES_PER_BINDING
                    && released_attempts >= ceiling
            }
            _ => true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewConvergenceV1 {
    pub(crate) schema_version: u16,
    pub(crate) convergence_id: ReviewConvergenceId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) policy_id: FinalizationPolicyId,
    pub(crate) reason: ReviewConvergenceReasonV1,
    pub(crate) convergence_hash: String,
}

impl ReviewConvergenceV1 {
    pub(crate) fn new(
        repository_revision: RepositoryRevisionId,
        policy_id: FinalizationPolicyId,
        reason: ReviewConvergenceReasonV1,
    ) -> Result<Self, ReviewContractError> {
        if !reason.validate() {
            return Err(ReviewContractError::Invalid {
                code: "review_convergence_reason_invalid",
            });
        }
        let convergence_hash = Self::expected_hash(&repository_revision, &policy_id, &reason)?;
        let convergence = Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            convergence_id: ReviewConvergenceId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:review-convergence",
                    repository_revision.as_str(),
                    &convergence_hash,
                ])
            )),
            repository_revision,
            policy_id,
            reason,
            convergence_hash,
        };
        convergence.validate()?;
        Ok(convergence)
    }

    pub(crate) fn validate(&self) -> Result<(), ReviewContractError> {
        let expected_hash =
            Self::expected_hash(&self.repository_revision, &self.policy_id, &self.reason)?;
        let expected_id = ReviewConvergenceId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:review-convergence",
                self.repository_revision.as_str(),
                &expected_hash,
            ])
        ));
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || !self.reason.validate()
            || self.convergence_hash != expected_hash
            || self.convergence_id != expected_id
        {
            return Err(ReviewContractError::Invalid {
                code: "review_convergence_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(
        repository_revision: &RepositoryRevisionId,
        policy_id: &FinalizationPolicyId,
        reason: &ReviewConvergenceReasonV1,
    ) -> Result<String, ReviewContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:review-convergence-record",
            &canonical_json(&(repository_revision, policy_id, reason))?,
        ]))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ReviewEvent {
    DiffManifestRequested {
        request: DiffManifestRequestV1,
    },
    DiffManifestBuildFailed {
        failure: DiffManifestEffectFailureV1,
    },
    DiffManifestRecorded {
        manifest: Box<DiffManifestV1>,
    },
    ActionPrepared {
        prepared: Box<PreparedReviewActionV1>,
    },
    ActionReleased {
        action_id: ActionId,
    },
    ActionRejected {
        action_id: ActionId,
        reason: ReviewActionRejectionReasonV1,
    },
    DiffPageReviewed {
        observation: Box<DiffPageReviewObservationV1>,
    },
    DiffReviewRecorded {
        review: Box<DiffReviewV1>,
    },
    CompletionEvaluationRecorded {
        evaluation: Box<CompletionEvaluationV1>,
    },
    PublicationAuthorityRequested {
        request: PublicationAuthorityRequestV1,
    },
    PublicationAuthorityObservationFailed {
        failure: PublicationAuthorityEffectFailureV1,
    },
    PublicationAuthorityObserved {
        observation: PublicationAuthorityObservationV1,
    },
    PublicationEligibilityEvaluated {
        eligibility: Box<PublicationEligibilityRecord>,
    },
    ConvergenceEvaluated {
        convergence: ReviewConvergenceV1,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReviewEffectRequest {
    BuildDiffManifest {
        request: Box<DiffManifestRequestV1>,
    },
    DispatchProvider {
        envelope: Box<ReviewActionEnvelopeV1>,
    },
    ObservePublicationAuthority {
        request: Box<PublicationAuthorityRequestV1>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewStateV1 {
    pub(crate) schema_version: u16,
    pub(crate) policy_id: FinalizationPolicyId,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) review_node_id: NodeId,
    pub(crate) completion_node_id: NodeId,
    pub(crate) ancestry: EngineeringAncestryV1,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
    pub(crate) diff_request: Option<DiffManifestRequestV1>,
    pub(crate) diff_manifest_failure: Option<DiffManifestEffectFailureV1>,
    pub(crate) diff_manifest: Option<Box<DiffManifestV1>>,
    pub(crate) actions: BTreeMap<ActionId, PreparedReviewActionV1>,
    pub(crate) released_actions: BTreeSet<ActionId>,
    pub(crate) rejected_actions: BTreeMap<ActionId, ReviewActionRejectionReasonV1>,
    pub(crate) page_reviews: BTreeMap<DiffPageId, DiffPageReviewObservationV1>,
    pub(crate) diff_review: Option<Box<DiffReviewV1>>,
    pub(crate) completion: Option<Box<CompletionEvaluationV1>>,
    pub(crate) authority_request: Option<PublicationAuthorityRequestV1>,
    pub(crate) authority_failure: Option<PublicationAuthorityEffectFailureV1>,
    pub(crate) authority: Option<PublicationAuthorityObservationV1>,
    pub(crate) eligibility: Option<Box<PublicationEligibilityRecord>>,
    pub(crate) convergence: Option<ReviewConvergenceV1>,
}

impl ReviewStateV1 {
    pub(crate) fn new(
        plan: &AcceptedPlan,
        policy: &FinalizationPolicyV1,
        ancestry: EngineeringAncestryV1,
        review_node_id: NodeId,
        completion_node_id: NodeId,
    ) -> Result<Self, ReviewContractError> {
        policy.validate()?;
        ancestry.validate()?;
        let criterion_ids = plan
            .targets
            .iter()
            .flat_map(|target| target.acceptance_criteria.iter().cloned())
            .collect::<BTreeSet<_>>();
        if plan.plan_id.as_str().trim().is_empty()
            || plan.plan_revision_id.as_str().trim().is_empty()
            || plan.repository_revision != policy.publication.base_repository_revision
            || criterion_ids.is_empty()
            || criterion_ids.len() > MAX_CRITERIA
            || review_node_id == completion_node_id
        {
            return Err(ReviewContractError::Invalid {
                code: "review_state_bootstrap_invalid",
            });
        }
        Ok(Self {
            schema_version: REVIEW_SCHEMA_VERSION,
            policy_id: policy.policy_id.clone(),
            plan_id: plan.plan_id.clone(),
            plan_revision_id: plan.plan_revision_id.clone(),
            repository_revision: ancestry.repository_revision.clone(),
            review_node_id,
            completion_node_id,
            ancestry,
            criterion_ids,
            diff_request: None,
            diff_manifest_failure: None,
            diff_manifest: None,
            actions: BTreeMap::new(),
            released_actions: BTreeSet::new(),
            rejected_actions: BTreeMap::new(),
            page_reviews: BTreeMap::new(),
            diff_review: None,
            completion: None,
            authority_request: None,
            authority_failure: None,
            authority: None,
            eligibility: None,
            convergence: None,
        })
    }

    pub(crate) fn apply(
        &mut self,
        event: &ReviewEvent,
        plan: &AcceptedPlan,
        policy: &FinalizationPolicyV1,
    ) -> Result<(), ReviewContractError> {
        if self.convergence.is_some() && !matches!(event, ReviewEvent::ConvergenceEvaluated { .. })
        {
            return Err(ReviewContractError::Invalid {
                code: "review_event_after_convergence",
            });
        }
        if (self.diff_manifest_failure.is_some() || self.authority_failure.is_some())
            && !matches!(event, ReviewEvent::ConvergenceEvaluated { .. })
        {
            return Err(ReviewContractError::Invalid {
                code: "review_event_after_effect_failure",
            });
        }
        match event {
            ReviewEvent::DiffManifestRequested { request } => {
                request.validate_against(plan, &self.ancestry, policy)?;
                if self.diff_request.is_some()
                    || request.review_node_id != self.review_node_id
                    || request.plan_id != self.plan_id
                    || request.plan_revision_id != self.plan_revision_id
                    || request.repository_revision != self.repository_revision
                {
                    return Err(ReviewContractError::Invalid {
                        code: "diff_manifest_request_not_next",
                    });
                }
                self.diff_request = Some(request.clone());
            }
            ReviewEvent::DiffManifestBuildFailed { failure } => {
                let request = self
                    .diff_request
                    .as_ref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "diff_manifest_failure_without_request",
                    })?;
                failure.validate_against(request)?;
                if self.diff_manifest_failure.is_some()
                    || self.diff_manifest.is_some()
                    || !self.actions.is_empty()
                {
                    return Err(ReviewContractError::Invalid {
                        code: "diff_manifest_effect_failure_not_next",
                    });
                }
                self.diff_manifest_failure = Some(failure.clone());
            }
            ReviewEvent::DiffManifestRecorded { manifest } => {
                let request = self
                    .diff_request
                    .as_ref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "diff_manifest_without_request",
                    })?;
                manifest.validate_against(request, plan)?;
                if self.diff_manifest_failure.is_some() || self.diff_manifest.is_some() {
                    return Err(ReviewContractError::Invalid {
                        code: "diff_manifest_already_recorded",
                    });
                }
                self.diff_manifest = Some(manifest.clone());
            }
            ReviewEvent::ActionPrepared { prepared } => {
                self.validate_new_action(prepared, policy)?;
                self.actions
                    .insert(prepared.envelope.action_id.clone(), (**prepared).clone());
            }
            ReviewEvent::ActionReleased { action_id } => {
                if !self.action_is_open(action_id) {
                    return Err(ReviewContractError::Invalid {
                        code: "review_action_release_invalid",
                    });
                }
                self.released_actions.insert(action_id.clone());
            }
            ReviewEvent::ActionRejected { action_id, reason } => {
                if !self.action_is_open(action_id) {
                    return Err(ReviewContractError::Invalid {
                        code: "review_action_rejection_invalid",
                    });
                }
                self.rejected_actions.insert(action_id.clone(), *reason);
            }
            ReviewEvent::DiffPageReviewed { observation } => {
                let prepared = self.actions.get(&observation.action_id).ok_or(
                    ReviewContractError::Invalid {
                        code: "diff_review_observation_action_missing",
                    },
                )?;
                let manifest =
                    self.diff_manifest
                        .as_deref()
                        .ok_or(ReviewContractError::Invalid {
                            code: "diff_review_observation_manifest_missing",
                        })?;
                if !self.action_is_open(&observation.action_id)
                    || self.page_reviews.contains_key(&observation.page_id)
                {
                    return Err(ReviewContractError::Invalid {
                        code: "diff_review_observation_not_next",
                    });
                }
                observation.validate_against(prepared, manifest)?;
                self.page_reviews
                    .insert(observation.page_id.clone(), (**observation).clone());
            }
            ReviewEvent::DiffReviewRecorded { review } => {
                let manifest =
                    self.diff_manifest
                        .as_deref()
                        .ok_or(ReviewContractError::Invalid {
                            code: "diff_review_manifest_missing",
                        })?;
                if self.diff_review.is_some() || self.current_action().is_some() {
                    return Err(ReviewContractError::Invalid {
                        code: "diff_review_not_ready",
                    });
                }
                review.validate_against(manifest, &self.page_reviews)?;
                self.diff_review = Some(review.clone());
            }
            ReviewEvent::CompletionEvaluationRecorded { evaluation } => {
                let prepared = self.actions.get(&evaluation.action_id).ok_or(
                    ReviewContractError::Invalid {
                        code: "completion_evaluation_action_missing",
                    },
                )?;
                let manifest =
                    self.diff_manifest
                        .as_deref()
                        .ok_or(ReviewContractError::Invalid {
                            code: "completion_evaluation_manifest_missing",
                        })?;
                let review = self
                    .diff_review
                    .as_deref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "completion_evaluation_review_missing",
                    })?;
                if !self.action_is_open(&evaluation.action_id) || self.completion.is_some() {
                    return Err(ReviewContractError::Invalid {
                        code: "completion_evaluation_not_next",
                    });
                }
                evaluation.validate_against(
                    prepared,
                    manifest,
                    review,
                    policy,
                    plan,
                    &self.ancestry,
                )?;
                self.completion = Some(evaluation.clone());
            }
            ReviewEvent::PublicationAuthorityRequested { request } => {
                let completion =
                    self.completion
                        .as_deref()
                        .ok_or(ReviewContractError::Invalid {
                            code: "publication_authority_completion_missing",
                        })?;
                request.validate_against(policy, completion)?;
                if self.authority_request.is_some() || self.current_action().is_some() {
                    return Err(ReviewContractError::Invalid {
                        code: "publication_authority_request_not_next",
                    });
                }
                self.authority_request = Some(request.clone());
            }
            ReviewEvent::PublicationAuthorityObservationFailed { failure } => {
                let request =
                    self.authority_request
                        .as_ref()
                        .ok_or(ReviewContractError::Invalid {
                            code: "publication_authority_failure_without_request",
                        })?;
                failure.validate_against(request)?;
                if self.authority_failure.is_some()
                    || self.authority.is_some()
                    || self.eligibility.is_some()
                {
                    return Err(ReviewContractError::Invalid {
                        code: "publication_authority_effect_failure_not_next",
                    });
                }
                self.authority_failure = Some(failure.clone());
            }
            ReviewEvent::PublicationAuthorityObserved { observation } => {
                let request =
                    self.authority_request
                        .as_ref()
                        .ok_or(ReviewContractError::Invalid {
                            code: "publication_authority_request_missing",
                        })?;
                observation.validate_against(request)?;
                if self.authority_failure.is_some() || self.authority.is_some() {
                    return Err(ReviewContractError::Invalid {
                        code: "publication_authority_already_observed",
                    });
                }
                self.authority = Some(observation.clone());
            }
            ReviewEvent::PublicationEligibilityEvaluated { eligibility } => {
                let manifest =
                    self.diff_manifest
                        .as_deref()
                        .ok_or(ReviewContractError::Invalid {
                            code: "publication_eligibility_manifest_missing",
                        })?;
                let review = self
                    .diff_review
                    .as_deref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "publication_eligibility_review_missing",
                    })?;
                let completion =
                    self.completion
                        .as_deref()
                        .ok_or(ReviewContractError::Invalid {
                            code: "publication_eligibility_completion_missing",
                        })?;
                let authority = self
                    .authority
                    .as_ref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "publication_eligibility_authority_missing",
                    })?;
                eligibility.validate_against(
                    policy,
                    &self.ancestry,
                    manifest,
                    review,
                    completion,
                    authority,
                )?;
                if self.eligibility.is_some() {
                    return Err(ReviewContractError::Invalid {
                        code: "publication_eligibility_already_evaluated",
                    });
                }
                self.eligibility = Some(eligibility.clone());
            }
            ReviewEvent::ConvergenceEvaluated { convergence } => {
                convergence.validate()?;
                if self.convergence.is_some()
                    || convergence.repository_revision != self.repository_revision
                    || convergence.policy_id != self.policy_id
                    || self
                        .validate_release_exhaustion_reason(&convergence.reason, policy)
                        .is_err()
                    || self
                        .validate_effect_failure_convergence_reason(&convergence.reason)
                        .is_err()
                {
                    return Err(ReviewContractError::Invalid {
                        code: "review_convergence_not_next",
                    });
                }
                self.convergence = Some(convergence.clone());
            }
        }
        Ok(())
    }

    pub(crate) fn validate(
        &self,
        plan: &AcceptedPlan,
        policy: &FinalizationPolicyV1,
    ) -> Result<(), ReviewContractError> {
        policy.validate()?;
        self.ancestry.validate()?;
        let expected_criteria = plan
            .targets
            .iter()
            .flat_map(|target| target.acceptance_criteria.iter().cloned())
            .collect::<BTreeSet<_>>();
        if self.schema_version != REVIEW_SCHEMA_VERSION
            || self.policy_id != policy.policy_id
            || self.plan_id != plan.plan_id
            || self.plan_revision_id != plan.plan_revision_id
            || self.repository_revision != self.ancestry.repository_revision
            || self.criterion_ids != expected_criteria
            || self.review_node_id == self.completion_node_id
            || self.current_action_count() > 1
            || self
                .released_actions
                .iter()
                .any(|action_id| self.rejected_actions.contains_key(action_id))
        {
            return Err(ReviewContractError::Invalid {
                code: "review_state_invalid",
            });
        }
        if let Some(request) = &self.diff_request {
            request.validate_against(plan, &self.ancestry, policy)?;
        } else if self.diff_manifest_failure.is_some()
            || self.diff_manifest.is_some()
            || !self.actions.is_empty()
            || !self.page_reviews.is_empty()
            || self.diff_review.is_some()
            || self.completion.is_some()
            || self.authority_request.is_some()
            || self.authority.is_some()
            || self.eligibility.is_some()
        {
            return Err(ReviewContractError::Invalid {
                code: "review_state_missing_diff_request",
            });
        }
        if let Some(manifest) = self.diff_manifest.as_deref() {
            if self.diff_manifest_failure.is_some() {
                return Err(ReviewContractError::Invalid {
                    code: "review_state_conflicting_diff_outcomes",
                });
            }
            manifest
                .validate_against(self.diff_request.as_ref().expect("request checked"), plan)?;
        } else if self.diff_manifest_failure.is_none()
            && (!self.actions.is_empty()
                || !self.page_reviews.is_empty()
                || self.diff_review.is_some()
                || self.completion.is_some()
                || self.authority_request.is_some()
                || self.authority_failure.is_some()
                || self.authority.is_some()
                || self.eligibility.is_some())
        {
            return Err(ReviewContractError::Invalid {
                code: "review_state_missing_diff_manifest",
            });
        }
        if let Some(failure) = &self.diff_manifest_failure {
            failure.validate_against(
                self.diff_request
                    .as_ref()
                    .expect("diff request checked before failure"),
            )?;
            if !self.actions.is_empty()
                || !self.page_reviews.is_empty()
                || self.diff_review.is_some()
                || self.completion.is_some()
                || self.authority_request.is_some()
                || self.authority_failure.is_some()
                || self.authority.is_some()
                || self.eligibility.is_some()
            {
                return Err(ReviewContractError::Invalid {
                    code: "review_state_after_diff_manifest_failure",
                });
            }
        }
        for (action_id, prepared) in &self.actions {
            prepared.validate(&self.ancestry)?;
            if action_id != &prepared.envelope.action_id {
                return Err(ReviewContractError::Invalid {
                    code: "review_state_action_index_invalid",
                });
            }
        }
        let binding_hashes = self
            .actions
            .values()
            .map(|prepared| prepared.context.binding.binding_hash())
            .collect::<Result<BTreeSet<_>, _>>()?;
        if binding_hashes.iter().any(|binding_hash| {
            self.uncontacted_release_count_by_hash(binding_hash)
                > policy.max_uncontacted_releases_per_binding
        }) {
            return Err(ReviewContractError::Invalid {
                code: "review_uncontacted_release_limit_exceeded",
            });
        }
        if self
            .released_actions
            .iter()
            .chain(self.rejected_actions.keys())
            .any(|action_id| !self.actions.contains_key(action_id))
        {
            return Err(ReviewContractError::Invalid {
                code: "review_state_action_outcome_invalid",
            });
        }
        if let Some(manifest) = self.diff_manifest.as_deref() {
            for (page_id, observation) in &self.page_reviews {
                let prepared = self.actions.get(&observation.action_id).ok_or(
                    ReviewContractError::Invalid {
                        code: "review_state_observation_action_missing",
                    },
                )?;
                if page_id != &observation.page_id
                    || self.released_actions.contains(&observation.action_id)
                    || self.rejected_actions.contains_key(&observation.action_id)
                {
                    return Err(ReviewContractError::Invalid {
                        code: "review_state_observation_index_invalid",
                    });
                }
                observation.validate_against(prepared, manifest)?;
            }
            if let Some(review) = self.diff_review.as_deref() {
                review.validate_against(manifest, &self.page_reviews)?;
            }
        }
        if let Some(completion) = self.completion.as_deref() {
            let prepared =
                self.actions
                    .get(&completion.action_id)
                    .ok_or(ReviewContractError::Invalid {
                        code: "review_state_completion_action_missing",
                    })?;
            completion.validate_against(
                prepared,
                self.diff_manifest.as_deref().expect("manifest checked"),
                self.diff_review
                    .as_deref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "review_state_completion_review_missing",
                    })?,
                policy,
                plan,
                &self.ancestry,
            )?;
        }
        if let Some(request) = &self.authority_request {
            request.validate_against(
                policy,
                self.completion
                    .as_deref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "review_state_authority_completion_missing",
                    })?,
            )?;
        } else if self.authority_failure.is_some()
            || self.authority.is_some()
            || self.eligibility.is_some()
        {
            return Err(ReviewContractError::Invalid {
                code: "review_state_missing_authority_request",
            });
        }
        if let Some(authority) = &self.authority {
            if self.authority_failure.is_some() {
                return Err(ReviewContractError::Invalid {
                    code: "review_state_conflicting_authority_outcomes",
                });
            }
            authority.validate_against(self.authority_request.as_ref().ok_or(
                ReviewContractError::Invalid {
                    code: "review_state_authority_request_missing",
                },
            )?)?;
        }
        if let Some(failure) = &self.authority_failure {
            failure.validate_against(self.authority_request.as_ref().ok_or(
                ReviewContractError::Invalid {
                    code: "review_state_authority_failure_request_missing",
                },
            )?)?;
            if self.eligibility.is_some() {
                return Err(ReviewContractError::Invalid {
                    code: "review_state_eligibility_after_authority_failure",
                });
            }
        }
        if let Some(eligibility) = self.eligibility.as_deref() {
            eligibility.validate_against(
                policy,
                &self.ancestry,
                self.diff_manifest.as_deref().expect("manifest checked"),
                self.diff_review
                    .as_deref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "review_state_eligibility_review_missing",
                    })?,
                self.completion
                    .as_deref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "review_state_eligibility_completion_missing",
                    })?,
                self.authority
                    .as_ref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "review_state_eligibility_authority_missing",
                    })?,
            )?;
        }
        if let Some(convergence) = &self.convergence {
            convergence.validate()?;
            if convergence.repository_revision != self.repository_revision
                || convergence.policy_id != self.policy_id
                || self
                    .validate_release_exhaustion_reason(&convergence.reason, policy)
                    .is_err()
                || self
                    .validate_effect_failure_convergence_reason(&convergence.reason)
                    .is_err()
            {
                return Err(ReviewContractError::Invalid {
                    code: "review_state_convergence_invalid",
                });
            }
        }
        Ok(())
    }

    pub(crate) fn current_action(&self) -> Option<&PreparedReviewActionV1> {
        self.actions
            .values()
            .find(|prepared| self.action_is_open(&prepared.envelope.action_id))
    }

    pub(crate) fn next_unreviewed_page(&self) -> Option<&DiffPageReceiptV1> {
        self.diff_manifest.as_deref().and_then(|manifest| {
            manifest
                .pages
                .iter()
                .find(|page| !self.page_reviews.contains_key(&page.page_id))
        })
    }

    pub(crate) fn effect_failure_convergence_reason(
        &self,
    ) -> Result<Option<ReviewConvergenceReasonV1>, ReviewContractError> {
        match (&self.diff_manifest_failure, &self.authority_failure) {
            (Some(_), Some(_)) => Err(ReviewContractError::Invalid {
                code: "review_multiple_effect_failures",
            }),
            (Some(failure), None) => {
                let request = self
                    .diff_request
                    .as_ref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "review_diff_failure_request_missing",
                    })?;
                failure.validate_against(request)?;
                Ok(Some(failure.convergence_reason()))
            }
            (None, Some(failure)) => {
                let request =
                    self.authority_request
                        .as_ref()
                        .ok_or(ReviewContractError::Invalid {
                            code: "review_authority_failure_request_missing",
                        })?;
                failure.validate_against(request)?;
                Ok(Some(failure.convergence_reason()))
            }
            (None, None) => Ok(None),
        }
    }

    pub(crate) fn uncontacted_release_count(
        &self,
        binding: &ReviewContextBindingV1,
    ) -> Result<u32, ReviewContractError> {
        Ok(self.uncontacted_release_count_by_hash(&binding.binding_hash()?))
    }

    pub(crate) fn uncontacted_release_convergence(
        &self,
        binding: &ReviewContextBindingV1,
        node_id: NodeId,
        policy: &FinalizationPolicyV1,
    ) -> Result<Option<ReviewConvergenceReasonV1>, ReviewContractError> {
        policy.validate()?;
        let binding_hash = binding.binding_hash()?;
        let released_attempts = self.uncontacted_release_count_by_hash(&binding_hash);
        Ok(
            (released_attempts >= policy.max_uncontacted_releases_per_binding).then_some(
                ReviewConvergenceReasonV1::UncontactedReleaseRetryExhausted {
                    node_id,
                    binding_hash,
                    released_attempts,
                    ceiling: policy.max_uncontacted_releases_per_binding,
                },
            ),
        )
    }

    fn current_action_count(&self) -> usize {
        self.actions
            .keys()
            .filter(|action_id| self.action_is_open(action_id))
            .count()
    }

    fn action_is_open(&self, action_id: &ActionId) -> bool {
        self.actions.contains_key(action_id)
            && !self.released_actions.contains(action_id)
            && !self.rejected_actions.contains_key(action_id)
            && !self
                .page_reviews
                .values()
                .any(|observation| &observation.action_id == action_id)
            && self
                .completion
                .as_ref()
                .is_none_or(|completion| &completion.action_id != action_id)
    }

    fn uncontacted_release_count_by_hash(&self, binding_hash: &str) -> u32 {
        u32::try_from(
            self.released_actions
                .iter()
                .filter(|action_id| {
                    self.actions.get(*action_id).is_some_and(|prepared| {
                        prepared.context.binding.binding_hash().as_deref() == Ok(binding_hash)
                    })
                })
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    fn validate_release_exhaustion_reason(
        &self,
        reason: &ReviewConvergenceReasonV1,
        policy: &FinalizationPolicyV1,
    ) -> Result<(), ReviewContractError> {
        let ReviewConvergenceReasonV1::UncontactedReleaseRetryExhausted {
            node_id,
            binding_hash,
            released_attempts,
            ceiling,
        } = reason
        else {
            return Ok(());
        };
        let actual = self.uncontacted_release_count_by_hash(binding_hash);
        let node_matches = self.released_actions.iter().any(|action_id| {
            self.actions.get(action_id).is_some_and(|prepared| {
                &prepared.context.node_id == node_id
                    && prepared.context.binding.binding_hash().as_deref() == Ok(binding_hash)
            })
        });
        if *ceiling != policy.max_uncontacted_releases_per_binding
            || *released_attempts != actual
            || actual < *ceiling
            || !node_matches
        {
            return Err(ReviewContractError::Invalid {
                code: "review_uncontacted_release_convergence_invalid",
            });
        }
        Ok(())
    }

    fn validate_effect_failure_convergence_reason(
        &self,
        reason: &ReviewConvergenceReasonV1,
    ) -> Result<(), ReviewContractError> {
        if let Some(expected) = self.effect_failure_convergence_reason()? {
            if reason != &expected {
                return Err(ReviewContractError::Invalid {
                    code: "review_effect_failure_convergence_mismatch",
                });
            }
        } else if matches!(
            reason,
            ReviewConvergenceReasonV1::DiffManifestLimitExceeded { .. }
                | ReviewConvergenceReasonV1::RepositoryDrift { .. }
                | ReviewConvergenceReasonV1::ArtifactDurabilityFailed { .. }
                | ReviewConvergenceReasonV1::PublicationAuthorityUnavailable { .. }
        ) {
            return Err(ReviewContractError::Invalid {
                code: "review_effect_failure_convergence_without_record",
            });
        }
        Ok(())
    }

    fn validate_new_action(
        &self,
        prepared: &PreparedReviewActionV1,
        policy: &FinalizationPolicyV1,
    ) -> Result<(), ReviewContractError> {
        prepared.validate(&self.ancestry)?;
        if self.current_action().is_some()
            || self.actions.contains_key(&prepared.envelope.action_id)
            || prepared.context.repository_revision != self.repository_revision
            || prepared.context.plan_id != self.plan_id
            || prepared.context.plan_revision_id != self.plan_revision_id
            || prepared.context.policy_id != policy.policy_id
            || prepared.context.criterion_ids != self.criterion_ids
        {
            return Err(ReviewContractError::Invalid {
                code: "review_action_not_next",
            });
        }
        if self.uncontacted_release_count(&prepared.context.binding)?
            >= policy.max_uncontacted_releases_per_binding
        {
            return Err(ReviewContractError::Invalid {
                code: "review_uncontacted_release_retry_exhausted",
            });
        }
        match &prepared.context.binding {
            ReviewContextBindingV1::DiffPage {
                manifest_id,
                diff_hash,
                page_id,
                page_index,
                page_content_hash,
                content_address,
                artifact_locator_hash,
                persistence_receipt_hash,
                page_byte_len,
            } => {
                let manifest =
                    self.diff_manifest
                        .as_deref()
                        .ok_or(ReviewContractError::Invalid {
                            code: "review_action_manifest_missing",
                        })?;
                let next_page =
                    self.next_unreviewed_page()
                        .ok_or(ReviewContractError::Invalid {
                            code: "review_action_no_page_remaining",
                        })?;
                if prepared.context.node_id != self.review_node_id
                    || self.diff_review.is_some()
                    || manifest_id != &manifest.manifest_id
                    || diff_hash != &manifest.diff_hash
                    || page_id != &next_page.page_id
                    || page_index != &next_page.index
                    || page_content_hash != &next_page.content_hash
                    || content_address != &next_page.content_address
                    || artifact_locator_hash != &next_page.artifact_locator_hash
                    || persistence_receipt_hash != &next_page.persistence_receipt_hash
                    || page_byte_len != &next_page.byte_len
                {
                    return Err(ReviewContractError::Invalid {
                        code: "diff_page_review_action_not_next",
                    });
                }
            }
            ReviewContextBindingV1::Completion {
                manifest_id,
                diff_hash,
                diff_review_id,
                page_review_ids,
            } => {
                let manifest =
                    self.diff_manifest
                        .as_deref()
                        .ok_or(ReviewContractError::Invalid {
                            code: "completion_action_manifest_missing",
                        })?;
                let review = self
                    .diff_review
                    .as_deref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "completion_action_review_missing",
                    })?;
                if prepared.context.node_id != self.completion_node_id
                    || self.completion.is_some()
                    || review.disposition != DiffReviewDispositionV1::Accepted
                    || manifest_id != &manifest.manifest_id
                    || diff_hash != &manifest.diff_hash
                    || diff_review_id != &review.review_id
                    || page_review_ids != &review.ordered_page_review_ids
                {
                    return Err(ReviewContractError::Invalid {
                        code: "completion_action_not_next",
                    });
                }
            }
        }
        if prepared.envelope.retry_index > 1 {
            let prior_id =
                prepared
                    .envelope
                    .prior_action_id
                    .as_ref()
                    .ok_or(ReviewContractError::Invalid {
                        code: "review_retry_prior_action_missing",
                    })?;
            let prior = self
                .actions
                .get(prior_id)
                .ok_or(ReviewContractError::Invalid {
                    code: "review_retry_prior_action_unknown",
                })?;
            if prepared.envelope.retry_index != prior.envelope.retry_index.saturating_add(1)
                || prepared.context.binding != prior.context.binding
                || (!self.released_actions.contains(prior_id)
                    && !self.rejected_actions.contains_key(prior_id))
            {
                return Err(ReviewContractError::Invalid {
                    code: "review_retry_chain_invalid",
                });
            }
        }
        Ok(())
    }
}

fn canonical_json(value: &impl Serialize) -> Result<String, ReviewContractError> {
    serde_json::to_string(value).map_err(|_| ReviewContractError::Serialization)
}

fn accepted_plan_hash(plan: &AcceptedPlan) -> Result<String, ReviewContractError> {
    Ok(stable_sha256(&[
        "execution-protocol-v1:accepted-plan-review-binding",
        &canonical_json(&(
            plan.schema_version,
            &plan.plan_id,
            &plan.plan_revision_id,
            &plan.repository_revision,
            &plan.discovery_impact_map_id,
            &plan.targets,
        ))?,
    ]))
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
