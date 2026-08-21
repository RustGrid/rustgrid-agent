use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use super::{
    AcceptedPlan, CommandProvenance, DiscoveryCriterionId, EvidenceId, FailureRevisionId,
    GeneratedPathDisposition, MutationPathState, MutationVerificationEvidence, NodeBudgetContract,
    NodeId, NodeSpec, PlanGraphMaterialization, PlanId, PlanRevisionId, PlannedTargetV1,
    PreparedTargetContext, ProfilePath, RelationshipEvidence, RepairCandidateId, RepairIntentId,
    RepairTargetContextLedger, RepositoryProfile, RepositoryProfileId, RepositoryRevisionId,
    TargetExecutionPurpose, TargetId, TargetOperation, TargetRole, ValidationCommandCandidate,
    ValidationCommandKind, ValidationEvidenceId, ValidationExpectationId, ValidationGateId,
    ValidationPolicyId, ValidationProcessId, ValidationRunId, implementation_node_id,
    stable_sha256,
};

pub(crate) const VALIDATION_SCHEMA_VERSION: u16 = 1;
const MAX_AUTHORIZATIONS: usize = 32;
const MAX_GATES: usize = 32;
const MAX_DIAGNOSTICS: usize = 128;
const MAX_DIAGNOSTIC_PATHS: usize = 32;
const MAX_REPAIR_CANDIDATES: usize = 64;
const MAX_SAFE_CODE_BYTES: usize = 128;
const MAX_COMMAND_PART_BYTES: usize = 1_024;
const MAX_PROCESS_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ValidationContractError {
    Invalid { code: &'static str },
    LimitExceeded { field: &'static str, maximum: usize },
    Serialization,
}

impl ValidationContractError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::Invalid { code } => code,
            Self::LimitExceeded { .. } => "validation_contract_limit_exceeded",
            Self::Serialization => "validation_contract_serialization_failed",
        }
    }
}

impl fmt::Display for ValidationContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { code } => write!(formatter, "validation contract violates `{code}`"),
            Self::LimitExceeded { field, maximum } => {
                write!(formatter, "validation field `{field}` exceeds {maximum}")
            }
            Self::Serialization => formatter.write_str("validation identity serialization failed"),
        }
    }
}

impl std::error::Error for ValidationContractError {}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationGateClass {
    Focused,
    TestSuite,
    Build,
    Typecheck,
    Lint,
    Metadata,
}

impl ValidationGateClass {
    const fn rank(self) -> u8 {
        match self {
            Self::Focused => 0,
            Self::TestSuite => 1,
            Self::Build => 2,
            Self::Typecheck => 3,
            Self::Lint => 4,
            Self::Metadata => 5,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationParserKind {
    Cargo,
    Node,
    Pytest,
    Go,
    Generic,
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ParserConfidence {
    Exact,
    Structured,
    Fallback,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationCommandAuthorization {
    pub(crate) candidate_id: EvidenceId,
    pub(crate) gate_class: ValidationGateClass,
    pub(crate) parser: ValidationParserKind,
    pub(crate) timeout_ms: u64,
    pub(crate) output_limit_bytes: u64,
    pub(crate) max_runs: u32,
    pub(crate) environment_fingerprint: String,
    pub(crate) dependency_fingerprint: String,
}

impl ValidationCommandAuthorization {
    fn validate(&self) -> Result<(), ValidationContractError> {
        if self.candidate_id.is_empty()
            || self.timeout_ms == 0
            || self.output_limit_bytes == 0
            || self.output_limit_bytes > MAX_PROCESS_OUTPUT_BYTES
            || self.max_runs == 0
            || !is_sha256(&self.environment_fingerprint)
            || !is_sha256(&self.dependency_fingerprint)
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_command_authorization_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TestRepairAuthorization {
    pub(crate) target_id: TargetId,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
    pub(crate) specification_evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) stale_expected_hash: String,
    pub(crate) accepted_actual_hash: String,
}

impl TestRepairAuthorization {
    fn validate(&self) -> Result<(), ValidationContractError> {
        if self.target_id.is_empty()
            || self.criterion_ids.is_empty()
            || self.specification_evidence_ids.is_empty()
            || !is_sha256(&self.stale_expected_hash)
            || !is_sha256(&self.accepted_actual_hash)
            || self.stale_expected_hash == self.accepted_actual_hash
        {
            return Err(ValidationContractError::Invalid {
                code: "test_repair_authorization_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationPolicyV1 {
    pub(crate) schema_version: u16,
    pub(crate) policy_id: ValidationPolicyId,
    pub(crate) signed_policy_evidence_id: EvidenceId,
    pub(crate) repository_profile_id: RepositoryProfileId,
    pub(crate) authorizations: Vec<ValidationCommandAuthorization>,
    pub(crate) required_broad_candidates: BTreeSet<EvidenceId>,
    pub(crate) repair_node_budget: NodeBudgetContract,
    pub(crate) max_repair_targets_per_failure: u32,
    pub(crate) test_repair_authorizations: Vec<TestRepairAuthorization>,
}

impl ValidationPolicyV1 {
    pub(crate) fn new(
        signed_policy_evidence_id: EvidenceId,
        profile: &RepositoryProfile,
        mut authorizations: Vec<ValidationCommandAuthorization>,
        required_broad_candidates: BTreeSet<EvidenceId>,
        repair_node_budget: NodeBudgetContract,
        max_repair_targets_per_failure: u32,
        mut test_repair_authorizations: Vec<TestRepairAuthorization>,
    ) -> Result<Self, ValidationContractError> {
        authorizations.sort();
        test_repair_authorizations.sort();
        let mut policy = Self {
            schema_version: VALIDATION_SCHEMA_VERSION,
            policy_id: ValidationPolicyId::new("pending:validation-policy"),
            signed_policy_evidence_id,
            repository_profile_id: profile.profile_id.clone(),
            authorizations,
            required_broad_candidates,
            repair_node_budget,
            max_repair_targets_per_failure,
            test_repair_authorizations,
        };
        policy.policy_id = policy.expected_id()?;
        policy.validate(profile)?;
        Ok(policy)
    }

    pub(crate) fn validate(
        &self,
        profile: &RepositoryProfile,
    ) -> Result<(), ValidationContractError> {
        self.validate_structure()?;
        if self.repository_profile_id != profile.profile_id
            || self.authorizations.iter().any(|authorization| {
                profile
                    .validation_candidates
                    .iter()
                    .all(|candidate| candidate.candidate_id != authorization.candidate_id)
            })
            || self.required_broad_candidates.iter().any(|candidate_id| {
                profile
                    .validation_candidates
                    .iter()
                    .all(|candidate| &candidate.candidate_id != candidate_id)
            })
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_policy_invalid",
            });
        }
        Ok(())
    }

    /// Validates the signed policy independently from repository materialization.
    ///
    /// A strict Protocol v1 bootstrap must reject a malformed policy before any
    /// repository effect is allowed. Candidate/profile membership is checked
    /// later by [`Self::validate`] once the exact repository profile has been
    /// recorded.
    pub(crate) fn validate_structure(&self) -> Result<(), ValidationContractError> {
        if self.schema_version != VALIDATION_SCHEMA_VERSION
            || self.signed_policy_evidence_id.is_empty()
            || self.repository_profile_id.is_empty()
            || self.authorizations.is_empty()
            || self.authorizations.len() > MAX_AUTHORIZATIONS
            || self.required_broad_candidates.len() > MAX_AUTHORIZATIONS
            || self.max_repair_targets_per_failure == 0
            || self
                .authorizations
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || self
                .test_repair_authorizations
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || self
                .authorizations
                .iter()
                .any(|authorization| authorization.validate().is_err())
            || self.required_broad_candidates.iter().any(|candidate_id| {
                self.authorization(candidate_id)
                    .is_none_or(|authorization| {
                        authorization.gate_class == ValidationGateClass::Focused
                    })
            })
            || self
                .test_repair_authorizations
                .iter()
                .any(|authorization| authorization.validate().is_err())
            || !repair_budget_is_viable(&self.repair_node_budget)
            || self.policy_id != self.expected_id()?
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_policy_invalid",
            });
        }
        Ok(())
    }

    fn expected_id(&self) -> Result<ValidationPolicyId, ValidationContractError> {
        Ok(ValidationPolicyId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:validation-policy",
                &canonical_json(&(
                    self.schema_version,
                    &self.signed_policy_evidence_id,
                    &self.repository_profile_id,
                    &self.authorizations,
                    &self.required_broad_candidates,
                    &self.repair_node_budget,
                    self.max_repair_targets_per_failure,
                    &self.test_repair_authorizations,
                ))?,
            ])
        )))
    }

    pub(crate) fn authorization(
        &self,
        candidate_id: &EvidenceId,
    ) -> Option<&ValidationCommandAuthorization> {
        self.authorizations
            .iter()
            .find(|authorization| &authorization.candidate_id == candidate_id)
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthorizedValidationCommand {
    pub(crate) command_id: EvidenceId,
    pub(crate) candidate_id: EvidenceId,
    pub(crate) executable: String,
    pub(crate) args: Vec<String>,
    pub(crate) working_directory: ProfilePath,
    pub(crate) environment_fingerprint: String,
    pub(crate) dependency_fingerprint: String,
}

impl AuthorizedValidationCommand {
    fn from_candidate(
        candidate: &ValidationCommandCandidate,
        authorization: &ValidationCommandAuthorization,
    ) -> Result<Self, ValidationContractError> {
        let argv = candidate.command.argv();
        let executable = argv.first().ok_or(ValidationContractError::Invalid {
            code: "validation_command_argv_empty",
        })?;
        let mut command = Self {
            command_id: EvidenceId::new("pending:validation-command"),
            candidate_id: candidate.candidate_id.clone(),
            executable: (*executable).to_owned(),
            args: argv
                .iter()
                .skip(1)
                .map(|value| (*value).to_owned())
                .collect(),
            working_directory: candidate.working_directory.clone(),
            environment_fingerprint: authorization.environment_fingerprint.clone(),
            dependency_fingerprint: authorization.dependency_fingerprint.clone(),
        };
        command.command_id = command.expected_id()?;
        command.validate()?;
        Ok(command)
    }

    fn validate(&self) -> Result<(), ValidationContractError> {
        if self.executable.is_empty()
            || self.executable.len() > MAX_COMMAND_PART_BYTES
            || self.executable.contains(char::is_whitespace)
            || self.args.len() > 32
            || self
                .args
                .iter()
                .any(|part| part.is_empty() || part.len() > MAX_COMMAND_PART_BYTES)
            || !is_sha256(&self.environment_fingerprint)
            || !is_sha256(&self.dependency_fingerprint)
            || self.command_id != self.expected_id()?
        {
            return Err(ValidationContractError::Invalid {
                code: "authorized_validation_command_invalid",
            });
        }
        Ok(())
    }

    fn expected_id(&self) -> Result<EvidenceId, ValidationContractError> {
        Ok(EvidenceId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:authorized-validation-command",
                &canonical_json(&(
                    &self.candidate_id,
                    &self.executable,
                    &self.args,
                    &self.working_directory,
                    &self.environment_fingerprint,
                    &self.dependency_fingerprint,
                ))?,
            ])
        )))
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationGateProvenance {
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) expectation_id: Option<ValidationExpectationId>,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
    pub(crate) repository_profile_id: RepositoryProfileId,
    pub(crate) validation_policy_id: ValidationPolicyId,
    pub(crate) profile_candidate_id: EvidenceId,
    pub(crate) profile_provenance: CommandProvenance,
    pub(crate) signed_policy_evidence_id: EvidenceId,
    pub(crate) parser: ValidationParserKind,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationGateV1 {
    pub(crate) schema_version: u16,
    pub(crate) gate_id: ValidationGateId,
    pub(crate) node_id: NodeId,
    pub(crate) class: ValidationGateClass,
    pub(crate) command: AuthorizedValidationCommand,
    pub(crate) required: bool,
    pub(crate) provenance: ValidationGateProvenance,
    pub(crate) timeout_ms: u64,
    pub(crate) output_limit_bytes: u64,
    pub(crate) max_runs: u32,
    pub(crate) dependencies: Vec<ValidationGateId>,
    pub(crate) repository_revision: RepositoryRevisionId,
}

impl ValidationGateV1 {
    pub(crate) fn validate(&self) -> Result<(), ValidationContractError> {
        self.command.validate()?;
        if self.schema_version != VALIDATION_SCHEMA_VERSION
            || self.node_id.is_empty()
            || !self.required
            || self.provenance.plan_id.is_empty()
            || self.provenance.plan_revision_id.is_empty()
            || self.provenance.criterion_ids.is_empty()
            || self.provenance.repository_profile_id.is_empty()
            || self.provenance.validation_policy_id.is_empty()
            || self.provenance.profile_candidate_id.is_empty()
            || self.provenance.signed_policy_evidence_id.is_empty()
            || self.timeout_ms == 0
            || self.output_limit_bytes == 0
            || self.output_limit_bytes > MAX_PROCESS_OUTPUT_BYTES
            || self.max_runs == 0
            || self.repository_revision.is_empty()
            || self.dependencies.len() > MAX_GATES
            || self.dependencies.windows(2).any(|pair| pair[0] >= pair[1])
            || self.gate_id != self.expected_id()?
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_gate_invalid",
            });
        }
        Ok(())
    }

    fn expected_id(&self) -> Result<ValidationGateId, ValidationContractError> {
        Ok(ValidationGateId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:validation-gate",
                &canonical_json(&(
                    self.schema_version,
                    &self.node_id,
                    self.class,
                    &self.command,
                    self.required,
                    &self.provenance,
                    self.timeout_ms,
                    self.output_limit_bytes,
                    self.max_runs,
                    &self.dependencies,
                    &self.repository_revision,
                ))?,
            ])
        )))
    }
}

struct ValidationGateSeed<'a> {
    class: ValidationGateClass,
    command_kind: ValidationCommandKind,
    expectation_id: Option<ValidationExpectationId>,
    criterion_ids: BTreeSet<DiscoveryCriterionId>,
    candidate: &'a ValidationCommandCandidate,
    authorization: &'a ValidationCommandAuthorization,
    node_id: NodeId,
}

pub(crate) fn build_validation_gates(
    plan: &AcceptedPlan,
    graph: &PlanGraphMaterialization,
    profile: &RepositoryProfile,
    policy: &ValidationPolicyV1,
    repository_revision: &RepositoryRevisionId,
) -> Result<Vec<ValidationGateV1>, ValidationContractError> {
    policy.validate(profile)?;
    if plan.plan_id != graph.plan_id || repository_revision.is_empty() {
        return Err(ValidationContractError::Invalid {
            code: "validation_gate_plan_binding_mismatch",
        });
    }
    let expectations = plan
        .targets
        .iter()
        .flat_map(|target| target.expected_validation.iter())
        .map(|expectation| (expectation.expectation_id.clone(), expectation.clone()))
        .collect::<BTreeMap<_, _>>();
    if expectations.is_empty() || expectations.len() > MAX_GATES {
        return Err(ValidationContractError::Invalid {
            code: "validation_gate_expectations_invalid",
        });
    }
    let expected_candidate_ids = expectations
        .values()
        .map(|expectation| expectation.command_candidate_id.clone())
        .collect::<BTreeSet<_>>();
    let broad_count = policy
        .required_broad_candidates
        .difference(&expected_candidate_ids)
        .count();
    if expectations.len().saturating_add(broad_count) > MAX_GATES {
        return Err(ValidationContractError::Invalid {
            code: "validation_gate_limit_exceeded",
        });
    }
    let mut seeds = Vec::with_capacity(expectations.len().saturating_add(broad_count));
    for (expectation_id, expectation) in expectations {
        let candidate = profile
            .validation_candidates
            .iter()
            .find(|candidate| candidate.candidate_id == expectation.command_candidate_id)
            .ok_or(ValidationContractError::Invalid {
                code: "validation_profile_candidate_missing",
            })?;
        let authorization = policy.authorization(&candidate.candidate_id).ok_or(
            ValidationContractError::Invalid {
                code: "validation_command_not_authorized",
            },
        )?;
        let node_id = graph
            .validation_nodes
            .get(&expectation_id)
            .ok_or(ValidationContractError::Invalid {
                code: "validation_graph_node_missing",
            })?
            .clone();
        seeds.push(ValidationGateSeed {
            class: ValidationGateClass::Focused,
            command_kind: candidate.command,
            expectation_id: Some(expectation_id),
            criterion_ids: expectation.criterion_ids,
            candidate,
            authorization,
            node_id,
        });
    }
    seeds.sort_by(|left, right| {
        left.class
            .rank()
            .cmp(&right.class.rank())
            .then_with(|| command_rank(left.command_kind).cmp(&command_rank(right.command_kind)))
            .then_with(|| {
                left.candidate
                    .candidate_id
                    .cmp(&right.candidate.candidate_id)
            })
            .then_with(|| left.expectation_id.cmp(&right.expectation_id))
    });
    let broad_node_id =
        seeds
            .last()
            .map(|seed| seed.node_id.clone())
            .ok_or(ValidationContractError::Invalid {
                code: "validation_graph_node_missing",
            })?;
    let all_criterion_ids = plan
        .targets
        .iter()
        .flat_map(|target| target.acceptance_criteria.iter().cloned())
        .collect::<BTreeSet<_>>();
    for candidate_id in policy
        .required_broad_candidates
        .difference(&expected_candidate_ids)
    {
        let candidate = profile
            .validation_candidates
            .iter()
            .find(|candidate| &candidate.candidate_id == candidate_id)
            .ok_or(ValidationContractError::Invalid {
                code: "validation_profile_candidate_missing",
            })?;
        let authorization =
            policy
                .authorization(candidate_id)
                .ok_or(ValidationContractError::Invalid {
                    code: "validation_command_not_authorized",
                })?;
        seeds.push(ValidationGateSeed {
            class: authorization.gate_class,
            command_kind: candidate.command,
            expectation_id: None,
            criterion_ids: all_criterion_ids.clone(),
            candidate,
            authorization,
            node_id: broad_node_id.clone(),
        });
    }
    seeds.sort_by(|left, right| {
        left.class
            .rank()
            .cmp(&right.class.rank())
            .then_with(|| command_rank(left.command_kind).cmp(&command_rank(right.command_kind)))
            .then_with(|| {
                left.candidate
                    .candidate_id
                    .cmp(&right.candidate.candidate_id)
            })
            .then_with(|| left.expectation_id.cmp(&right.expectation_id))
    });
    let mut gates = Vec::with_capacity(seeds.len());
    for seed in seeds {
        let dependencies = gates
            .last()
            .map(|gate: &ValidationGateV1| vec![gate.gate_id.clone()])
            .unwrap_or_default();
        let mut gate = ValidationGateV1 {
            schema_version: VALIDATION_SCHEMA_VERSION,
            gate_id: ValidationGateId::new("pending:validation-gate"),
            node_id: seed.node_id,
            class: seed.class,
            command: AuthorizedValidationCommand::from_candidate(
                seed.candidate,
                seed.authorization,
            )?,
            required: true,
            provenance: ValidationGateProvenance {
                plan_id: plan.plan_id.clone(),
                plan_revision_id: plan.plan_revision_id.clone(),
                expectation_id: seed.expectation_id,
                criterion_ids: seed.criterion_ids,
                repository_profile_id: profile.profile_id.clone(),
                validation_policy_id: policy.policy_id.clone(),
                profile_candidate_id: seed.candidate.candidate_id.clone(),
                profile_provenance: seed.candidate.provenance.clone(),
                signed_policy_evidence_id: policy.signed_policy_evidence_id.clone(),
                parser: seed.authorization.parser,
            },
            timeout_ms: seed.authorization.timeout_ms,
            output_limit_bytes: seed.authorization.output_limit_bytes,
            max_runs: seed.authorization.max_runs,
            dependencies,
            repository_revision: repository_revision.clone(),
        };
        gate.gate_id = gate.expected_id()?;
        gate.validate()?;
        gates.push(gate);
    }
    Ok(gates)
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "run_kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ValidationRunKind {
    Initial,
    ExactRepairRerun {
        failure_revision_id: FailureRevisionId,
        repair_intent_id: RepairIntentId,
        verified_repair_evidence_id: EvidenceId,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationRunSchedule {
    pub(crate) schema_version: u16,
    pub(crate) run_id: ValidationRunId,
    pub(crate) execution_id: super::ExecutionId,
    pub(crate) execution_attempt: u32,
    pub(crate) gate_id: ValidationGateId,
    pub(crate) node_id: NodeId,
    pub(crate) node_attempt: u32,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) run_attempt: u32,
    pub(crate) kind: ValidationRunKind,
    pub(crate) effect_id: super::EffectId,
}

impl ValidationRunSchedule {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        execution_id: super::ExecutionId,
        execution_attempt: u32,
        gate: &ValidationGateV1,
        node_attempt: u32,
        repository_revision: RepositoryRevisionId,
        run_attempt: u32,
        kind: ValidationRunKind,
    ) -> Result<Self, ValidationContractError> {
        let mut schedule = Self {
            schema_version: VALIDATION_SCHEMA_VERSION,
            run_id: ValidationRunId::new("pending:validation-run"),
            execution_id,
            execution_attempt,
            gate_id: gate.gate_id.clone(),
            node_id: gate.node_id.clone(),
            node_attempt,
            repository_revision,
            run_attempt,
            kind,
            effect_id: super::EffectId::new("pending:validation-process-effect"),
        };
        schedule.run_id = schedule.expected_run_id()?;
        schedule.effect_id = schedule.expected_effect_id()?;
        schedule.validate_against(gate)?;
        Ok(schedule)
    }

    pub(crate) fn validate_against(
        &self,
        gate: &ValidationGateV1,
    ) -> Result<(), ValidationContractError> {
        if self.schema_version != VALIDATION_SCHEMA_VERSION
            || self.execution_id.is_empty()
            || self.execution_attempt == 0
            || self.gate_id != gate.gate_id
            || self.node_id != gate.node_id
            || self.node_attempt == 0
            || self.repository_revision.is_empty()
            || self.run_attempt == 0
            || self.run_attempt > gate.max_runs
            || self.run_id != self.expected_run_id()?
            || self.effect_id != self.expected_effect_id()?
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_run_schedule_invalid",
            });
        }
        Ok(())
    }

    fn expected_run_id(&self) -> Result<ValidationRunId, ValidationContractError> {
        Ok(ValidationRunId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:validation-run",
                &canonical_json(&(
                    &self.execution_id,
                    self.execution_attempt,
                    &self.gate_id,
                    &self.node_id,
                    self.node_attempt,
                    &self.repository_revision,
                    self.run_attempt,
                    &self.kind,
                ))?,
            ])
        )))
    }

    fn expected_effect_id(&self) -> Result<super::EffectId, ValidationContractError> {
        Ok(super::EffectId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:validation-process-effect",
                self.expected_run_id()?.as_str(),
            ])
        )))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationProcessRequest {
    pub(crate) schema_version: u16,
    pub(crate) schedule: ValidationRunSchedule,
    pub(crate) policy_id: ValidationPolicyId,
    pub(crate) command: AuthorizedValidationCommand,
    pub(crate) parser: ValidationParserKind,
    pub(crate) timeout_ms: u64,
    pub(crate) output_limit_bytes: u64,
    pub(crate) payload_hash: String,
}

impl ValidationProcessRequest {
    pub(crate) fn new(
        schedule: ValidationRunSchedule,
        gate: &ValidationGateV1,
        policy: &ValidationPolicyV1,
    ) -> Result<Self, ValidationContractError> {
        let authorization = policy.authorization(&gate.command.candidate_id).ok_or(
            ValidationContractError::Invalid {
                code: "validation_command_not_authorized",
            },
        )?;
        let mut request = Self {
            schema_version: VALIDATION_SCHEMA_VERSION,
            schedule,
            policy_id: policy.policy_id.clone(),
            command: gate.command.clone(),
            parser: authorization.parser,
            timeout_ms: gate.timeout_ms,
            output_limit_bytes: gate.output_limit_bytes,
            payload_hash: String::new(),
        };
        request.payload_hash = request.expected_payload_hash()?;
        request.validate_against(gate, policy)?;
        Ok(request)
    }

    pub(crate) fn validate_against(
        &self,
        gate: &ValidationGateV1,
        policy: &ValidationPolicyV1,
    ) -> Result<(), ValidationContractError> {
        self.schedule.validate_against(gate)?;
        let authorization = policy.authorization(&gate.command.candidate_id).ok_or(
            ValidationContractError::Invalid {
                code: "validation_command_not_authorized",
            },
        )?;
        if self.schema_version != VALIDATION_SCHEMA_VERSION
            || self.policy_id != policy.policy_id
            || gate.provenance.validation_policy_id != policy.policy_id
            || gate.provenance.repository_profile_id != policy.repository_profile_id
            || self.command != gate.command
            || self.parser != authorization.parser
            || self.parser != gate.provenance.parser
            || self.timeout_ms != gate.timeout_ms
            || self.output_limit_bytes != gate.output_limit_bytes
            || self.payload_hash != self.expected_payload_hash()?
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_process_request_invalid",
            });
        }
        Ok(())
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, ValidationContractError> {
        serde_json::to_vec(self).map_err(|_| ValidationContractError::Serialization)
    }

    fn expected_payload_hash(&self) -> Result<String, ValidationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:validation-process-request",
            &canonical_json(&(
                self.schema_version,
                &self.schedule,
                &self.policy_id,
                &self.command,
                self.parser,
                self.timeout_ms,
                self.output_limit_bytes,
            ))?,
        ]))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationProcessStarted {
    pub(crate) schema_version: u16,
    pub(crate) run_id: ValidationRunId,
    pub(crate) effect_id: super::EffectId,
    pub(crate) process_id: ValidationProcessId,
    pub(crate) process_handle_hash: String,
}

impl ValidationProcessStarted {
    pub(crate) fn new(
        request: &ValidationProcessRequest,
        process_handle_hash: String,
    ) -> Result<Self, ValidationContractError> {
        if !is_sha256(&process_handle_hash) {
            return Err(ValidationContractError::Invalid {
                code: "validation_process_handle_invalid",
            });
        }
        let process_id = ValidationProcessId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:validation-process",
                request.schedule.run_id.as_str(),
                &process_handle_hash,
            ])
        ));
        Ok(Self {
            schema_version: VALIDATION_SCHEMA_VERSION,
            run_id: request.schedule.run_id.clone(),
            effect_id: request.schedule.effect_id.clone(),
            process_id,
            process_handle_hash,
        })
    }

    pub(crate) fn validate_against(
        &self,
        request: &ValidationProcessRequest,
    ) -> Result<(), ValidationContractError> {
        if self.schema_version != VALIDATION_SCHEMA_VERSION
            || self.run_id != request.schedule.run_id
            || self.effect_id != request.schedule.effect_id
            || !is_sha256(&self.process_handle_hash)
            || self != &Self::new(request, self.process_handle_hash.clone())?
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_process_started_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationArtifactReceipt {
    pub(crate) content_hash: String,
    pub(crate) artifact_locator_hash: String,
    pub(crate) persistence_receipt_hash: String,
    pub(crate) byte_len: u64,
}

impl ValidationArtifactReceipt {
    pub(crate) fn validate(&self) -> Result<(), ValidationContractError> {
        if !is_sha256(&self.content_hash)
            || !is_sha256(&self.artifact_locator_hash)
            || !is_sha256(&self.persistence_receipt_hash)
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_output_artifact_receipt_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BoundedOutputStream {
    pub(crate) original_bytes: u64,
    pub(crate) captured_bytes: u64,
    pub(crate) dropped_bytes: u64,
    pub(crate) truncated: bool,
    pub(crate) head: Option<ValidationArtifactReceipt>,
    pub(crate) tail: Option<ValidationArtifactReceipt>,
}

impl BoundedOutputStream {
    pub(crate) fn validate(&self, limit: u64) -> Result<(), ValidationContractError> {
        if self.captured_bytes > limit
            || self.original_bytes != self.captured_bytes.saturating_add(self.dropped_bytes)
            || self.truncated != (self.dropped_bytes > 0)
            || self
                .head
                .as_ref()
                .is_some_and(|receipt| receipt.validate().is_err())
            || self
                .tail
                .as_ref()
                .is_some_and(|receipt| receipt.validate().is_err())
            || self
                .head
                .iter()
                .chain(self.tail.iter())
                .map(|receipt| receipt.byte_len)
                .sum::<u64>()
                != self.captured_bytes
            || (self.captured_bytes == 0 && (self.head.is_some() || self.tail.is_some()))
            || (self.captured_bytes > 0 && self.head.is_none())
            || (self.truncated && self.tail.is_none())
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_bounded_output_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BoundedProcessOutput {
    pub(crate) stdout: BoundedOutputStream,
    pub(crate) stderr: BoundedOutputStream,
}

impl BoundedProcessOutput {
    pub(crate) fn validate(&self, limit: u64) -> Result<(), ValidationContractError> {
        self.stdout.validate(limit)?;
        self.stderr.validate(limit)?;
        if self
            .stdout
            .captured_bytes
            .saturating_add(self.stderr.captured_bytes)
            > limit
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_combined_output_limit_exceeded",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationInfrastructureFailureKind {
    Spawn,
    Timeout,
    Journal,
    Transport,
    Canceled,
    LeaseLost,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ValidationProcessResult {
    Exited {
        exit_code: i32,
    },
    InfrastructureFailure {
        kind: ValidationInfrastructureFailureKind,
        safe_code: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationProcessCompleted {
    pub(crate) schema_version: u16,
    pub(crate) run_id: ValidationRunId,
    pub(crate) effect_id: super::EffectId,
    pub(crate) process_id: Option<ValidationProcessId>,
    pub(crate) duration_ms: u64,
    pub(crate) result: ValidationProcessResult,
    pub(crate) output: BoundedProcessOutput,
    pub(crate) completion_hash: String,
}

impl ValidationProcessCompleted {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        request: &ValidationProcessRequest,
        started: Option<&ValidationProcessStarted>,
        duration_ms: u64,
        result: ValidationProcessResult,
        output: BoundedProcessOutput,
    ) -> Result<Self, ValidationContractError> {
        let mut completed = Self {
            schema_version: VALIDATION_SCHEMA_VERSION,
            run_id: request.schedule.run_id.clone(),
            effect_id: request.schedule.effect_id.clone(),
            process_id: started.map(|started| started.process_id.clone()),
            duration_ms,
            result,
            output,
            completion_hash: String::new(),
        };
        completed.completion_hash = completed.expected_hash()?;
        completed.validate_against(request, started)?;
        Ok(completed)
    }

    pub(crate) fn validate_against(
        &self,
        request: &ValidationProcessRequest,
        started: Option<&ValidationProcessStarted>,
    ) -> Result<(), ValidationContractError> {
        if let Some(started) = started {
            started.validate_against(request)?;
        }
        self.output.validate(request.output_limit_bytes)?;
        let process_start_binding_valid = match &self.result {
            ValidationProcessResult::InfrastructureFailure {
                kind: ValidationInfrastructureFailureKind::Spawn,
                ..
            } => started.is_none(),
            ValidationProcessResult::InfrastructureFailure {
                kind:
                    ValidationInfrastructureFailureKind::Canceled
                    | ValidationInfrastructureFailureKind::LeaseLost,
                ..
            } => true,
            ValidationProcessResult::Exited { .. }
            | ValidationProcessResult::InfrastructureFailure { .. } => started.is_some(),
        };
        if self.schema_version != VALIDATION_SCHEMA_VERSION
            || self.run_id != request.schedule.run_id
            || self.effect_id != request.schedule.effect_id
            || self.process_id != started.map(|started| started.process_id.clone())
            || !process_start_binding_valid
            || self.duration_ms > request.timeout_ms.saturating_add(5_000)
            || matches!(&self.result, ValidationProcessResult::InfrastructureFailure { safe_code, .. } if !safe_code_is_valid(safe_code))
            || self.completion_hash != self.expected_hash()?
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_process_completion_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(&self) -> Result<String, ValidationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:validation-process-completion",
            &canonical_json(&(
                self.schema_version,
                &self.run_id,
                &self.effect_id,
                &self.process_id,
                self.duration_ms,
                &self.result,
                &self.output,
            ))?,
        ]))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationDiagnosticKind {
    TestAssertion,
    CompileError,
    TypeError,
    LintFinding,
    MetadataFailure,
    UnclassifiedFailure,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationSourceLocation {
    pub(crate) path: ProfilePath,
    pub(crate) line: Option<u32>,
    pub(crate) column: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationDiagnostic {
    pub(crate) diagnostic_id: EvidenceId,
    pub(crate) kind: ValidationDiagnosticKind,
    pub(crate) test_identity_hash: Option<String>,
    pub(crate) source_location: Option<ValidationSourceLocation>,
    pub(crate) expected_value_hash: Option<String>,
    pub(crate) actual_value_hash: Option<String>,
    pub(crate) implicated_paths: BTreeSet<ProfilePath>,
    pub(crate) relationship_evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) safe_summary_code: String,
    pub(crate) safe_summary_hash: String,
    pub(crate) confidence: ParserConfidence,
}

impl ValidationDiagnostic {
    // Parser adapters provide this bounded, versioned diagnostic record in one
    // atomic call so no partially bound diagnostic can enter protocol state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        kind: ValidationDiagnosticKind,
        test_identity_hash: Option<String>,
        source_location: Option<ValidationSourceLocation>,
        expected_value_hash: Option<String>,
        actual_value_hash: Option<String>,
        implicated_paths: BTreeSet<ProfilePath>,
        relationship_evidence_ids: BTreeSet<EvidenceId>,
        safe_summary_code: String,
        safe_summary_hash: String,
        confidence: ParserConfidence,
    ) -> Result<Self, ValidationContractError> {
        let mut diagnostic = Self {
            diagnostic_id: EvidenceId::new("pending:validation-diagnostic"),
            kind,
            test_identity_hash,
            source_location,
            expected_value_hash,
            actual_value_hash,
            implicated_paths,
            relationship_evidence_ids,
            safe_summary_code,
            safe_summary_hash,
            confidence,
        };
        diagnostic.diagnostic_id = diagnostic.expected_id()?;
        diagnostic.validate()?;
        Ok(diagnostic)
    }

    pub(crate) fn validate(&self) -> Result<(), ValidationContractError> {
        if self
            .test_identity_hash
            .as_ref()
            .is_some_and(|value| !is_sha256(value))
            || self
                .expected_value_hash
                .as_ref()
                .is_some_and(|value| !is_sha256(value))
            || self
                .actual_value_hash
                .as_ref()
                .is_some_and(|value| !is_sha256(value))
            || self.implicated_paths.len() > MAX_DIAGNOSTIC_PATHS
            || !safe_code_is_valid(&self.safe_summary_code)
            || !is_sha256(&self.safe_summary_hash)
            || self.diagnostic_id != self.expected_id()?
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_diagnostic_invalid",
            });
        }
        Ok(())
    }

    fn expected_id(&self) -> Result<EvidenceId, ValidationContractError> {
        Ok(EvidenceId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:validation-diagnostic",
                &canonical_json(&(
                    self.kind,
                    &self.test_identity_hash,
                    &self.source_location,
                    &self.expected_value_hash,
                    &self.actual_value_hash,
                    &self.implicated_paths,
                    &self.relationship_evidence_ids,
                    &self.safe_summary_code,
                    &self.safe_summary_hash,
                    self.confidence,
                ))?,
            ])
        )))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GateSemanticsObservation {
    ExpectedSemanticsObserved,
    ExpectedSemanticsMissing,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ValidationEvidenceOutcome {
    Passed,
    DomainFailed {
        failure_revision_id: FailureRevisionId,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationEvidenceV1 {
    pub(crate) schema_version: u16,
    pub(crate) evidence_id: ValidationEvidenceId,
    pub(crate) run_id: ValidationRunId,
    pub(crate) gate_id: ValidationGateId,
    pub(crate) node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) command_id: EvidenceId,
    pub(crate) process_completion_hash: String,
    pub(crate) parser: ValidationParserKind,
    pub(crate) parser_confidence: ParserConfidence,
    pub(crate) semantics: GateSemanticsObservation,
    pub(crate) diagnostics: Vec<ValidationDiagnostic>,
    pub(crate) outcome: ValidationEvidenceOutcome,
}

impl ValidationEvidenceV1 {
    pub(crate) fn from_completed(
        request: &ValidationProcessRequest,
        started: &ValidationProcessStarted,
        completed: &ValidationProcessCompleted,
        parser_confidence: ParserConfidence,
        semantics: GateSemanticsObservation,
        mut diagnostics: Vec<ValidationDiagnostic>,
    ) -> Result<Self, ValidationContractError> {
        completed.validate_against(request, Some(started))?;
        diagnostics.sort();
        diagnostics.dedup();
        if diagnostics.len() > MAX_DIAGNOSTICS
            || diagnostics
                .iter()
                .any(|diagnostic| diagnostic.validate().is_err())
        {
            return Err(ValidationContractError::LimitExceeded {
                field: "validation_diagnostics",
                maximum: MAX_DIAGNOSTICS,
            });
        }
        let exit_code = match completed.result {
            ValidationProcessResult::Exited { exit_code } => exit_code,
            ValidationProcessResult::InfrastructureFailure { .. } => {
                return Err(ValidationContractError::Invalid {
                    code: "infrastructure_result_cannot_create_validation_evidence",
                });
            }
        };
        let outcome =
            if exit_code == 0 && semantics == GateSemanticsObservation::ExpectedSemanticsObserved {
                if !diagnostics.is_empty() {
                    return Err(ValidationContractError::Invalid {
                        code: "passing_validation_has_failure_diagnostics",
                    });
                }
                ValidationEvidenceOutcome::Passed
            } else {
                if diagnostics.is_empty() {
                    diagnostics.push(ValidationDiagnostic::new(
                        ValidationDiagnosticKind::UnclassifiedFailure,
                        None,
                        None,
                        None,
                        None,
                        BTreeSet::new(),
                        BTreeSet::new(),
                        "validation_failure_unclassified".into(),
                        stable_sha256(&[
                            "execution-protocol-v1:validation-unclassified",
                            &completed.completion_hash,
                        ]),
                        ParserConfidence::Fallback,
                    )?);
                }
                let failure_revision_id = FailureRevisionId::new(format!(
                    "epv1:{}",
                    stable_sha256(&[
                        "execution-protocol-v1:validation-failure-revision",
                        request.schedule.run_id.as_str(),
                        &request.schedule.repository_revision.to_string(),
                        &completed.completion_hash,
                        &canonical_json(&diagnostics)?,
                    ])
                ));
                ValidationEvidenceOutcome::DomainFailed {
                    failure_revision_id,
                }
            };
        let mut evidence = Self {
            schema_version: VALIDATION_SCHEMA_VERSION,
            evidence_id: ValidationEvidenceId::new("pending:validation-evidence"),
            run_id: request.schedule.run_id.clone(),
            gate_id: request.schedule.gate_id.clone(),
            node_id: request.schedule.node_id.clone(),
            repository_revision: request.schedule.repository_revision.clone(),
            command_id: request.command.command_id.clone(),
            process_completion_hash: completed.completion_hash.clone(),
            parser: request.parser,
            parser_confidence,
            semantics,
            diagnostics,
            outcome,
        };
        evidence.evidence_id = evidence.expected_id()?;
        evidence.validate_against(request, completed)?;
        Ok(evidence)
    }

    pub(crate) fn validate_against(
        &self,
        request: &ValidationProcessRequest,
        completed: &ValidationProcessCompleted,
    ) -> Result<(), ValidationContractError> {
        let exited = match completed.result {
            ValidationProcessResult::Exited { exit_code } => exit_code,
            ValidationProcessResult::InfrastructureFailure { .. } => {
                return Err(ValidationContractError::Invalid {
                    code: "infrastructure_result_cannot_create_validation_evidence",
                });
            }
        };
        let passed = matches!(self.outcome, ValidationEvidenceOutcome::Passed);
        if self.schema_version != VALIDATION_SCHEMA_VERSION
            || self.run_id != request.schedule.run_id
            || self.gate_id != request.schedule.gate_id
            || self.node_id != request.schedule.node_id
            || self.repository_revision != request.schedule.repository_revision
            || self.command_id != request.command.command_id
            || self.process_completion_hash != completed.completion_hash
            || self.parser != request.parser
            || self.diagnostics.len() > MAX_DIAGNOSTICS
            || self.diagnostics.windows(2).any(|pair| pair[0] >= pair[1])
            || self
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.validate().is_err())
            || passed
                != (exited == 0
                    && self.semantics == GateSemanticsObservation::ExpectedSemanticsObserved
                    && self.diagnostics.is_empty())
            || matches!(self.outcome, ValidationEvidenceOutcome::DomainFailed { .. })
                && self.diagnostics.is_empty()
            || self.evidence_id != self.expected_id()?
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_evidence_invalid",
            });
        }
        if let ValidationEvidenceOutcome::DomainFailed {
            failure_revision_id,
        } = &self.outcome
        {
            let expected = FailureRevisionId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:validation-failure-revision",
                    request.schedule.run_id.as_str(),
                    &request.schedule.repository_revision.to_string(),
                    &completed.completion_hash,
                    &canonical_json(&self.diagnostics)?,
                ])
            ));
            if failure_revision_id != &expected {
                return Err(ValidationContractError::Invalid {
                    code: "validation_failure_revision_identity_mismatch",
                });
            }
        }
        Ok(())
    }

    fn expected_id(&self) -> Result<ValidationEvidenceId, ValidationContractError> {
        Ok(ValidationEvidenceId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:validation-evidence",
                &canonical_json(&(
                    self.schema_version,
                    &self.run_id,
                    &self.gate_id,
                    &self.node_id,
                    &self.repository_revision,
                    &self.command_id,
                    &self.process_completion_hash,
                    self.parser,
                    self.parser_confidence,
                    self.semantics,
                    &self.diagnostics,
                    &self.outcome,
                ))?,
            ])
        )))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationFailureRevisionV1 {
    pub(crate) schema_version: u16,
    pub(crate) failure_revision_id: FailureRevisionId,
    pub(crate) validation_evidence_id: ValidationEvidenceId,
    pub(crate) run_id: ValidationRunId,
    pub(crate) gate_id: ValidationGateId,
    pub(crate) node_id: NodeId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) diagnostic_ids: BTreeSet<EvidenceId>,
}

impl ValidationFailureRevisionV1 {
    pub(crate) fn from_evidence(
        evidence: &ValidationEvidenceV1,
    ) -> Result<Self, ValidationContractError> {
        let ValidationEvidenceOutcome::DomainFailed {
            failure_revision_id,
        } = &evidence.outcome
        else {
            return Err(ValidationContractError::Invalid {
                code: "validation_failure_revision_without_failure",
            });
        };
        Ok(Self {
            schema_version: VALIDATION_SCHEMA_VERSION,
            failure_revision_id: failure_revision_id.clone(),
            validation_evidence_id: evidence.evidence_id.clone(),
            run_id: evidence.run_id.clone(),
            gate_id: evidence.gate_id.clone(),
            node_id: evidence.node_id.clone(),
            repository_revision: evidence.repository_revision.clone(),
            diagnostic_ids: evidence
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.diagnostic_id.clone())
                .collect(),
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepairScoreComponentKind {
    ExactSourceLocation,
    ExactTestLocation,
    ImplicatedPath,
    RelationshipEvidence,
    AcceptanceCriterion,
    SourceRole,
    TestRole,
    IntentEvidence,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepairScoreComponent {
    pub(crate) kind: RepairScoreComponentKind,
    pub(crate) points: u32,
    pub(crate) supporting_evidence_ids: BTreeSet<EvidenceId>,
}

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepairCandidateV1 {
    pub(crate) candidate_id: RepairCandidateId,
    pub(crate) failure_revision_id: FailureRevisionId,
    pub(crate) target_id: TargetId,
    pub(crate) target_path: ProfilePath,
    pub(crate) target_role: TargetRole,
    pub(crate) score_components: Vec<RepairScoreComponent>,
    pub(crate) total_score: u32,
}

impl RepairCandidateV1 {
    fn validate(&self) -> Result<(), ValidationContractError> {
        let expected_total = self
            .score_components
            .iter()
            .map(|component| component.points)
            .fold(0_u32, u32::saturating_add);
        if self.failure_revision_id.is_empty()
            || self.target_id.is_empty()
            || self.score_components.is_empty()
            || self
                .score_components
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || self
                .score_components
                .iter()
                .any(|component| component.points == 0)
            || self.total_score != expected_total
            || self.candidate_id != self.expected_id()?
        {
            return Err(ValidationContractError::Invalid {
                code: "repair_candidate_invalid",
            });
        }
        Ok(())
    }

    fn expected_id(&self) -> Result<RepairCandidateId, ValidationContractError> {
        Ok(RepairCandidateId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:repair-candidate",
                self.failure_revision_id.as_str(),
                self.target_id.as_str(),
                &canonical_json(&(
                    &self.target_path,
                    self.target_role,
                    &self.score_components,
                    self.total_score,
                ))?,
            ])
        )))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepairCandidateRanking {
    pub(crate) schema_version: u16,
    pub(crate) failure_revision_id: FailureRevisionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) candidates: Vec<RepairCandidateV1>,
    pub(crate) ranking_hash: String,
}

pub(crate) fn rank_repair_candidates(
    failure: &ValidationFailureRevisionV1,
    evidence: &ValidationEvidenceV1,
    plan: &AcceptedPlan,
    relationships: &BTreeMap<EvidenceId, RelationshipEvidence>,
) -> Result<RepairCandidateRanking, ValidationContractError> {
    if failure.validation_evidence_id != evidence.evidence_id
        || failure.repository_revision != evidence.repository_revision
        || plan.targets.len() > MAX_REPAIR_CANDIDATES
    {
        return Err(ValidationContractError::Invalid {
            code: "repair_ranking_failure_binding_mismatch",
        });
    }
    let mut candidates = Vec::new();
    for target in &plan.targets {
        let mut components = BTreeMap::<RepairScoreComponentKind, RepairScoreComponent>::new();
        for diagnostic in &evidence.diagnostics {
            let direct_source = diagnostic
                .source_location
                .as_ref()
                .is_some_and(|location| location.path == target.path);
            if direct_source {
                add_score(
                    &mut components,
                    if target.role == TargetRole::Test {
                        RepairScoreComponentKind::ExactTestLocation
                    } else {
                        RepairScoreComponentKind::ExactSourceLocation
                    },
                    if target.role == TargetRole::Test {
                        28
                    } else {
                        36
                    },
                    BTreeSet::from([diagnostic.diagnostic_id.clone()]),
                );
            }
            if diagnostic.implicated_paths.contains(&target.path) {
                add_score(
                    &mut components,
                    RepairScoreComponentKind::ImplicatedPath,
                    18,
                    BTreeSet::from([diagnostic.diagnostic_id.clone()]),
                );
            }
            let diagnostic_paths = diagnostic
                .source_location
                .iter()
                .map(|location| &location.path)
                .chain(diagnostic.implicated_paths.iter())
                .collect::<BTreeSet<_>>();
            let connecting_relationships = diagnostic
                .relationship_evidence_ids
                .iter()
                .filter_map(|relationship_id| {
                    let relationship = relationships.get(relationship_id)?;
                    let target_is_from = relationship.from.as_str() == target.path.as_str();
                    let target_is_to = relationship.to.as_str() == target.path.as_str();
                    let connects_diagnostic = diagnostic_paths.iter().any(|path| {
                        (target_is_from && relationship.to.as_str() == path.as_str())
                            || (target_is_to && relationship.from.as_str() == path.as_str())
                    });
                    (relationship.evidence_id == *relationship_id
                        && relationship.repository_revision == failure.repository_revision
                        && connects_diagnostic)
                        .then_some(relationship_id.clone())
                })
                .collect::<BTreeSet<_>>();
            if !connecting_relationships.is_empty() {
                add_score(
                    &mut components,
                    RepairScoreComponentKind::RelationshipEvidence,
                    16,
                    connecting_relationships,
                );
            }
        }
        if !target.expected_validation.is_empty()
            && !target.acceptance_criteria.is_empty()
            && components.len() > 1
        {
            add_score(
                &mut components,
                RepairScoreComponentKind::AcceptanceCriterion,
                12,
                target.required_evidence.clone(),
            );
        }
        add_score(
            &mut components,
            if target.role == TargetRole::Test {
                RepairScoreComponentKind::TestRole
            } else {
                RepairScoreComponentKind::SourceRole
            },
            if target.role == TargetRole::Test {
                2
            } else {
                8
            },
            BTreeSet::new(),
        );
        if components.len() == 1 && components.contains_key(&RepairScoreComponentKind::SourceRole) {
            continue;
        }
        let score_components = components.into_values().collect::<Vec<_>>();
        let total_score = score_components
            .iter()
            .map(|component| component.points)
            .fold(0_u32, u32::saturating_add);
        let mut candidate = RepairCandidateV1 {
            candidate_id: RepairCandidateId::new("pending:repair-candidate"),
            failure_revision_id: failure.failure_revision_id.clone(),
            target_id: target.target_id.clone(),
            target_path: target.path.clone(),
            target_role: target.role,
            score_components,
            total_score,
        };
        candidate.candidate_id = candidate.expected_id()?;
        candidate.validate()?;
        candidates.push(candidate);
    }
    candidates.sort_by(|left, right| {
        right
            .total_score
            .cmp(&left.total_score)
            .then_with(|| left.target_id.cmp(&right.target_id))
    });
    let mut ranking = RepairCandidateRanking {
        schema_version: VALIDATION_SCHEMA_VERSION,
        failure_revision_id: failure.failure_revision_id.clone(),
        repository_revision: failure.repository_revision.clone(),
        candidates,
        ranking_hash: String::new(),
    };
    ranking.ranking_hash = expected_ranking_hash(&ranking)?;
    ranking.validate()?;
    Ok(ranking)
}

impl RepairCandidateRanking {
    pub(crate) fn validate(&self) -> Result<(), ValidationContractError> {
        if self.schema_version != VALIDATION_SCHEMA_VERSION
            || self.failure_revision_id.is_empty()
            || self.repository_revision.is_empty()
            || self.candidates.len() > MAX_REPAIR_CANDIDATES
            || self.candidates.iter().any(|candidate| {
                candidate.failure_revision_id != self.failure_revision_id
                    || candidate.validate().is_err()
            })
            || self.candidates.windows(2).any(|pair| {
                pair[0].total_score < pair[1].total_score
                    || (pair[0].total_score == pair[1].total_score
                        && pair[0].target_id >= pair[1].target_id)
            })
            || self.ranking_hash != expected_ranking_hash(self)?
        {
            return Err(ValidationContractError::Invalid {
                code: "repair_candidate_ranking_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepairEligibilityReason {
    EligibleDirectSourceEvidence,
    EligibleRelationshipEvidence,
    EligibleStaleTestSpecification,
    IneligibleNoTargetEvidence,
    IneligibleTestRequiresSpecification,
    IneligibleGeneratedOutput,
    IneligibleMutationBaselineMissing,
    IneligibleMutationBaselineNotCurrent,
    IneligibleUnsupportedMutationBaseline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RepairMutationBaselineOwner {
    Implementation {
        node_id: NodeId,
    },
    ValidationRepair {
        node_id: NodeId,
        repair_intent_id: RepairIntentId,
        failure_revision_id: FailureRevisionId,
        baseline_mutation_evidence_id: EvidenceId,
    },
}

/// Transient authority identifying which verified mutation established the
/// current contents of a repair target.
///
/// The evidence ID remains the persisted authority. This wrapper is rebuilt
/// from the event log so callers cannot treat a structurally valid mutation
/// observation as a repair baseline without also proving who owned it. The
/// reducer remains responsible for requiring the exact recorded mutation and
/// terminal owner proof before invoking either constructor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepairMutationBaseline {
    owner: RepairMutationBaselineOwner,
    evidence: MutationVerificationEvidence,
}

impl RepairMutationBaseline {
    pub(crate) fn from_implementation(
        plan: &AcceptedPlan,
        target: &PlannedTargetV1,
        evidence: MutationVerificationEvidence,
    ) -> Result<Self, ValidationContractError> {
        let canonical_target = target_by_id(plan, &target.target_id)?;
        let node_id = implementation_node_id(plan, canonical_target);
        if canonical_target != target
            || evidence.validate().is_err()
            || evidence.node_id != node_id
            || evidence.target_id != target.target_id
        {
            return Err(ValidationContractError::Invalid {
                code: "repair_implementation_baseline_owner_invalid",
            });
        }
        Ok(Self {
            owner: RepairMutationBaselineOwner::Implementation { node_id },
            evidence,
        })
    }

    pub(crate) fn from_verified_repair(
        plan: &AcceptedPlan,
        prior_failure: &ValidationFailureRevisionV1,
        prior_selection: &RepairTargetSelection,
        prior_baseline: &Self,
        evidence: MutationVerificationEvidence,
    ) -> Result<Self, ValidationContractError> {
        prior_selection.validate_execution_binding(prior_failure, plan, prior_baseline)?;
        if prior_baseline.evidence.evidence_id
            != prior_selection.intent.baseline_mutation_evidence_id
            || evidence.validate().is_err()
            || evidence.node_id != prior_selection.repair_node.id
            || evidence.target_id != prior_selection.intent.target_id
            || evidence.repository_revision_before != prior_selection.intent.repository_revision
            || evidence.changed_paths
                != BTreeSet::from([prior_selection.intent.target_path.clone()])
            || evidence.path_transitions.len() != 1
        {
            return Err(ValidationContractError::Invalid {
                code: "repair_verified_baseline_owner_invalid",
            });
        }
        let transition = evidence
            .path_transitions
            .get(&prior_selection.intent.target_path)
            .ok_or(ValidationContractError::Invalid {
                code: "repair_verified_baseline_owner_invalid",
            })?;
        let TargetOperation::ModifyExisting {
            expected_content_hash,
        } = &prior_selection.intent.target_operation
        else {
            return Err(ValidationContractError::Invalid {
                code: "repair_verified_baseline_owner_invalid",
            });
        };
        if !matches!(
            (&transition.before, &transition.after),
            (
                MutationPathState::File {
                    content_hash: before_content_hash,
                    ..
                },
                MutationPathState::File {
                    content_hash: after_content_hash,
                    ..
                }
            ) if before_content_hash == expected_content_hash
                && after_content_hash != before_content_hash
        ) {
            return Err(ValidationContractError::Invalid {
                code: "repair_verified_baseline_owner_invalid",
            });
        }
        Ok(Self {
            owner: RepairMutationBaselineOwner::ValidationRepair {
                node_id: prior_selection.repair_node.id.clone(),
                repair_intent_id: prior_selection.intent.repair_intent_id.clone(),
                failure_revision_id: prior_selection.intent.failure_revision_id.clone(),
                baseline_mutation_evidence_id: prior_selection
                    .intent
                    .baseline_mutation_evidence_id
                    .clone(),
            },
            evidence,
        })
    }

    pub(crate) const fn owner(&self) -> &RepairMutationBaselineOwner {
        &self.owner
    }

    pub(crate) const fn evidence(&self) -> &MutationVerificationEvidence {
        &self.evidence
    }
}

pub(crate) type RepairMutationBaselines = BTreeMap<TargetId, RepairMutationBaseline>;

#[derive(Clone, Debug, Deserialize, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepairEligibilityDecision {
    pub(crate) candidate_id: RepairCandidateId,
    pub(crate) target_id: TargetId,
    pub(crate) eligible: bool,
    pub(crate) reason: RepairEligibilityReason,
    pub(crate) supporting_evidence_ids: BTreeSet<EvidenceId>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepairEligibilityEvaluation {
    pub(crate) schema_version: u16,
    pub(crate) failure_revision_id: FailureRevisionId,
    pub(crate) ranking_hash: String,
    pub(crate) decisions: Vec<RepairEligibilityDecision>,
    pub(crate) evaluation_hash: String,
}

pub(crate) fn evaluate_repair_eligibility(
    ranking: &RepairCandidateRanking,
    failure: &ValidationFailureRevisionV1,
    evidence: &ValidationEvidenceV1,
    plan: &AcceptedPlan,
    profile: &RepositoryProfile,
    policy: &ValidationPolicyV1,
    baselines: &RepairMutationBaselines,
) -> Result<RepairEligibilityEvaluation, ValidationContractError> {
    ranking.validate()?;
    policy.validate(profile)?;
    if ranking.failure_revision_id != failure.failure_revision_id
        || ranking.repository_revision != failure.repository_revision
        || failure.validation_evidence_id != evidence.evidence_id
        || failure.repository_revision != evidence.repository_revision
    {
        return Err(ValidationContractError::Invalid {
            code: "repair_eligibility_failure_binding_mismatch",
        });
    }
    let mut decisions = Vec::with_capacity(ranking.candidates.len());
    for candidate in &ranking.candidates {
        let target = target_by_id(plan, &candidate.target_id)?;
        let generated = profile.generated_disposition(&target.path);
        let direct_ids = candidate
            .score_components
            .iter()
            .filter(|component| {
                matches!(
                    component.kind,
                    RepairScoreComponentKind::ExactSourceLocation
                        | RepairScoreComponentKind::ImplicatedPath
                )
            })
            .flat_map(|component| component.supporting_evidence_ids.iter().cloned())
            .collect::<BTreeSet<_>>();
        let relationship_ids = candidate
            .score_components
            .iter()
            .filter(|component| component.kind == RepairScoreComponentKind::RelationshipEvidence)
            .flat_map(|component| component.supporting_evidence_ids.iter().cloned())
            .collect::<BTreeSet<_>>();
        let stale_test = policy
            .test_repair_authorizations
            .iter()
            .find(|authorization| {
                authorization.target_id == target.target_id
                    && authorization
                        .criterion_ids
                        .is_subset(&target.acceptance_criteria)
                    && authorization
                        .specification_evidence_ids
                        .is_subset(&target.required_evidence)
                    && evidence.diagnostics.iter().any(|diagnostic| {
                        (diagnostic
                            .source_location
                            .as_ref()
                            .is_some_and(|location| location.path == target.path)
                            || diagnostic.implicated_paths.contains(&target.path))
                            && diagnostic.expected_value_hash.as_ref()
                                == Some(&authorization.stale_expected_hash)
                            && diagnostic.actual_value_hash.as_ref()
                                == Some(&authorization.accepted_actual_hash)
                    })
            });
        let (mut eligible, mut reason, mut supporting_evidence_ids) =
            if generated != GeneratedPathDisposition::OrdinarySource {
                (
                    false,
                    RepairEligibilityReason::IneligibleGeneratedOutput,
                    BTreeSet::new(),
                )
            } else if target.role == TargetRole::Test {
                stale_test.map_or(
                    (
                        false,
                        RepairEligibilityReason::IneligibleTestRequiresSpecification,
                        BTreeSet::new(),
                    ),
                    |authorization| {
                        (
                            true,
                            RepairEligibilityReason::EligibleStaleTestSpecification,
                            authorization.specification_evidence_ids.clone(),
                        )
                    },
                )
            } else if !direct_ids.is_empty() {
                (
                    true,
                    RepairEligibilityReason::EligibleDirectSourceEvidence,
                    direct_ids,
                )
            } else if !relationship_ids.is_empty() {
                (
                    true,
                    RepairEligibilityReason::EligibleRelationshipEvidence,
                    relationship_ids,
                )
            } else {
                (
                    false,
                    RepairEligibilityReason::IneligibleNoTargetEvidence,
                    BTreeSet::new(),
                )
            };
        if eligible {
            match baselines.get(&target.target_id) {
                None => {
                    eligible = false;
                    reason = RepairEligibilityReason::IneligibleMutationBaselineMissing;
                    supporting_evidence_ids.clear();
                }
                Some(baseline) => {
                    match supported_repair_operation_from_baseline(plan, target, failure, baseline)
                    {
                        Ok(_) => {
                            supporting_evidence_ids.insert(baseline.evidence().evidence_id.clone());
                        }
                        Err(RepairBaselineRejection::NotCurrent) => {
                            eligible = false;
                            reason = RepairEligibilityReason::IneligibleMutationBaselineNotCurrent;
                            supporting_evidence_ids.clear();
                        }
                        Err(RepairBaselineRejection::Unsupported) => {
                            eligible = false;
                            reason = RepairEligibilityReason::IneligibleUnsupportedMutationBaseline;
                            supporting_evidence_ids.clear();
                        }
                    }
                }
            }
        }
        decisions.push(RepairEligibilityDecision {
            candidate_id: candidate.candidate_id.clone(),
            target_id: candidate.target_id.clone(),
            eligible,
            reason,
            supporting_evidence_ids,
        });
    }
    decisions.sort();
    let mut evaluation = RepairEligibilityEvaluation {
        schema_version: VALIDATION_SCHEMA_VERSION,
        failure_revision_id: ranking.failure_revision_id.clone(),
        ranking_hash: ranking.ranking_hash.clone(),
        decisions,
        evaluation_hash: String::new(),
    };
    evaluation.evaluation_hash = expected_evaluation_hash(&evaluation)?;
    evaluation.validate(ranking)?;
    Ok(evaluation)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepairBaselineRejection {
    NotCurrent,
    Unsupported,
}

fn supported_repair_operation_from_baseline(
    plan: &AcceptedPlan,
    target: &PlannedTargetV1,
    failure: &ValidationFailureRevisionV1,
    baseline: &RepairMutationBaseline,
) -> Result<TargetOperation, RepairBaselineRejection> {
    let evidence = baseline.evidence();
    if evidence.repository_revision_after != failure.repository_revision {
        return Err(RepairBaselineRejection::NotCurrent);
    }
    if evidence.validate().is_err()
        || evidence.target_id != target.target_id
        || evidence.changed_paths != BTreeSet::from([target.path.clone()])
        || evidence.path_transitions.len() != 1
    {
        return Err(RepairBaselineRejection::Unsupported);
    }
    let transition = evidence
        .path_transitions
        .get(&target.path)
        .ok_or(RepairBaselineRejection::Unsupported)?;
    let after_content_hash = match baseline.owner() {
        RepairMutationBaselineOwner::Implementation { node_id } => {
            if node_id != &implementation_node_id(plan, target) {
                return Err(RepairBaselineRejection::Unsupported);
            }
            match (&target.operation, &transition.before, &transition.after) {
                (
                    TargetOperation::ModifyExisting {
                        expected_content_hash,
                    },
                    MutationPathState::File {
                        content_hash: before_content_hash,
                        ..
                    },
                    MutationPathState::File {
                        content_hash: after_content_hash,
                        ..
                    },
                ) if before_content_hash == expected_content_hash
                    && after_content_hash != before_content_hash =>
                {
                    after_content_hash
                }
                (
                    TargetOperation::CreateFile { .. },
                    MutationPathState::Absent,
                    MutationPathState::File {
                        content_hash: after_content_hash,
                        ..
                    },
                ) => after_content_hash,
                _ => return Err(RepairBaselineRejection::Unsupported),
            }
        }
        RepairMutationBaselineOwner::ValidationRepair { node_id, .. } => {
            if node_id != &evidence.node_id {
                return Err(RepairBaselineRejection::Unsupported);
            }
            match (&transition.before, &transition.after) {
                (
                    MutationPathState::File {
                        content_hash: before_content_hash,
                        ..
                    },
                    MutationPathState::File {
                        content_hash: after_content_hash,
                        ..
                    },
                ) if after_content_hash != before_content_hash => after_content_hash,
                _ => return Err(RepairBaselineRejection::Unsupported),
            }
        }
    };
    Ok(TargetOperation::ModifyExisting {
        expected_content_hash: after_content_hash.clone(),
    })
}

pub(crate) fn repair_target_operation_from_baseline(
    plan: &AcceptedPlan,
    target: &PlannedTargetV1,
    failure: &ValidationFailureRevisionV1,
    baseline: &RepairMutationBaseline,
) -> Result<TargetOperation, ValidationContractError> {
    supported_repair_operation_from_baseline(plan, target, failure, baseline).map_err(|reason| {
        ValidationContractError::Invalid {
            code: match reason {
                RepairBaselineRejection::NotCurrent => "repair_mutation_baseline_not_current",
                RepairBaselineRejection::Unsupported => "repair_mutation_baseline_unsupported",
            },
        }
    })
}

impl RepairEligibilityEvaluation {
    pub(crate) fn validate(
        &self,
        ranking: &RepairCandidateRanking,
    ) -> Result<(), ValidationContractError> {
        let candidate_ids = ranking
            .candidates
            .iter()
            .map(|candidate| &candidate.candidate_id)
            .collect::<BTreeSet<_>>();
        let decision_ids = self
            .decisions
            .iter()
            .map(|decision| &decision.candidate_id)
            .collect::<BTreeSet<_>>();
        if self.schema_version != VALIDATION_SCHEMA_VERSION
            || self.failure_revision_id != ranking.failure_revision_id
            || self.ranking_hash != ranking.ranking_hash
            || candidate_ids != decision_ids
            || self.decisions.windows(2).any(|pair| pair[0] >= pair[1])
            || self.decisions.iter().any(|decision| {
                decision.eligible
                    != matches!(
                        decision.reason,
                        RepairEligibilityReason::EligibleDirectSourceEvidence
                            | RepairEligibilityReason::EligibleRelationshipEvidence
                            | RepairEligibilityReason::EligibleStaleTestSpecification
                    )
                    || (decision.eligible && decision.supporting_evidence_ids.is_empty())
            })
            || self.evaluation_hash != expected_evaluation_hash(self)?
        {
            return Err(ValidationContractError::Invalid {
                code: "repair_eligibility_evaluation_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepairIntentV1 {
    pub(crate) schema_version: u16,
    pub(crate) repair_intent_id: RepairIntentId,
    pub(crate) failure_revision_id: FailureRevisionId,
    pub(crate) originating_gate_id: ValidationGateId,
    pub(crate) target_id: TargetId,
    pub(crate) target_path: ProfilePath,
    pub(crate) target_operation: TargetOperation,
    pub(crate) baseline_mutation_evidence_id: EvidenceId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) validation_policy_id: ValidationPolicyId,
    pub(crate) repair_budget_hash: String,
    pub(crate) supporting_evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) required_evidence_ids: BTreeSet<EvidenceId>,
    pub(crate) criterion_ids: BTreeSet<DiscoveryCriterionId>,
}

impl RepairIntentV1 {
    pub(crate) fn validate(&self) -> Result<(), ValidationContractError> {
        let rebased_modify_hash = match &self.target_operation {
            TargetOperation::ModifyExisting {
                expected_content_hash,
            } => Some(expected_content_hash),
            TargetOperation::CreateFile { .. }
            | TargetOperation::DeleteFile { .. }
            | TargetOperation::MoveFile { .. } => None,
        };
        if self.failure_revision_id.is_empty()
            || self.target_id.is_empty()
            || self.baseline_mutation_evidence_id.is_empty()
            || self.repository_revision.is_empty()
            || self.validation_policy_id.is_empty()
            || rebased_modify_hash.is_none_or(|hash| !is_sha256(hash))
            || !is_sha256(&self.repair_budget_hash)
            || self.supporting_evidence_ids.is_empty()
            || self.required_evidence_ids.is_empty()
            || self.criterion_ids.is_empty()
            || self.repair_intent_id != self.expected_id()?
        {
            return Err(ValidationContractError::Invalid {
                code: "repair_intent_invalid",
            });
        }
        Ok(())
    }

    fn expected_id(&self) -> Result<RepairIntentId, ValidationContractError> {
        Ok(RepairIntentId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:validation-repair-intent",
                &canonical_json(&(
                    self.schema_version,
                    &self.failure_revision_id,
                    &self.originating_gate_id,
                    &self.target_id,
                    &self.target_path,
                    &self.target_operation,
                    &self.baseline_mutation_evidence_id,
                    &self.repository_revision,
                    &self.validation_policy_id,
                    &self.repair_budget_hash,
                    &self.supporting_evidence_ids,
                    &self.required_evidence_ids,
                    &self.criterion_ids,
                ))?,
            ])
        )))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepairTargetSelection {
    pub(crate) schema_version: u16,
    pub(crate) candidate_id: RepairCandidateId,
    pub(crate) evaluation_hash: String,
    pub(crate) intent: RepairIntentV1,
    pub(crate) repair_node: NodeSpec,
    pub(crate) selection_hash: String,
}

pub(crate) fn select_repair_target(
    ranking: &RepairCandidateRanking,
    evaluation: &RepairEligibilityEvaluation,
    failure: &ValidationFailureRevisionV1,
    gate: &ValidationGateV1,
    plan: &AcceptedPlan,
    policy: &ValidationPolicyV1,
    baselines: &RepairMutationBaselines,
) -> Result<Option<RepairTargetSelection>, ValidationContractError> {
    evaluation.validate(ranking)?;
    if failure.failure_revision_id != ranking.failure_revision_id || gate.gate_id != failure.gate_id
    {
        return Err(ValidationContractError::Invalid {
            code: "repair_selection_failure_binding_mismatch",
        });
    }
    let eligible = ranking.candidates.iter().filter_map(|candidate| {
        evaluation
            .decisions
            .iter()
            .find(|decision| decision.candidate_id == candidate.candidate_id && decision.eligible)
            .map(|decision| (candidate, decision))
    });
    let Some((candidate, decision)) = eligible.into_iter().next() else {
        return Ok(None);
    };
    let target = target_by_id(plan, &candidate.target_id)?;
    let baseline = baselines
        .get(&target.target_id)
        .ok_or(ValidationContractError::Invalid {
            code: "selected_repair_mutation_baseline_missing",
        })?;
    let target_operation = repair_target_operation_from_baseline(plan, target, failure, baseline)?;
    let mut intent = RepairIntentV1 {
        schema_version: VALIDATION_SCHEMA_VERSION,
        repair_intent_id: RepairIntentId::new("pending:repair-intent"),
        failure_revision_id: failure.failure_revision_id.clone(),
        originating_gate_id: gate.gate_id.clone(),
        target_id: target.target_id.clone(),
        target_path: target.path.clone(),
        target_operation,
        baseline_mutation_evidence_id: baseline.evidence().evidence_id.clone(),
        repository_revision: failure.repository_revision.clone(),
        validation_policy_id: policy.policy_id.clone(),
        repair_budget_hash: stable_sha256(&[
            "execution-protocol-v1:validation-repair-budget",
            &canonical_json(&policy.repair_node_budget)?,
        ]),
        supporting_evidence_ids: decision.supporting_evidence_ids.clone(),
        required_evidence_ids: target.required_evidence.clone(),
        criterion_ids: target.acceptance_criteria.clone(),
    };
    intent.repair_intent_id = intent.expected_id()?;
    intent.validate()?;
    let repair_node_id = NodeId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:validation-repair-node",
            intent.failure_revision_id.as_str(),
            intent.repair_intent_id.as_str(),
            intent.target_id.as_str(),
            intent.repository_revision.as_str(),
        ])
    ));
    let mut selection = RepairTargetSelection {
        schema_version: VALIDATION_SCHEMA_VERSION,
        candidate_id: candidate.candidate_id.clone(),
        evaluation_hash: evaluation.evaluation_hash.clone(),
        intent,
        repair_node: NodeSpec {
            id: repair_node_id,
            kind: super::NodeKind::ValidationRepair,
            required: true,
            dependencies: Vec::new(),
            budget: policy.repair_node_budget.clone(),
        },
        selection_hash: String::new(),
    };
    selection.selection_hash = selection.expected_hash()?;
    selection.validate(ranking, evaluation, failure, gate, plan, policy, baselines)?;
    Ok(Some(selection))
}

impl RepairTargetSelection {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn validate(
        &self,
        ranking: &RepairCandidateRanking,
        evaluation: &RepairEligibilityEvaluation,
        failure: &ValidationFailureRevisionV1,
        gate: &ValidationGateV1,
        plan: &AcceptedPlan,
        policy: &ValidationPolicyV1,
        baselines: &RepairMutationBaselines,
    ) -> Result<(), ValidationContractError> {
        let candidate = ranking
            .candidates
            .iter()
            .find(|candidate| candidate.candidate_id == self.candidate_id)
            .ok_or(ValidationContractError::Invalid {
                code: "selected_repair_candidate_missing",
            })?;
        let decision = evaluation
            .decisions
            .iter()
            .find(|decision| decision.candidate_id == self.candidate_id && decision.eligible)
            .ok_or(ValidationContractError::Invalid {
                code: "selected_repair_candidate_ineligible",
            })?;
        let highest = ranking
            .candidates
            .iter()
            .find(|candidate| {
                evaluation.decisions.iter().any(|decision| {
                    decision.candidate_id == candidate.candidate_id && decision.eligible
                })
            })
            .ok_or(ValidationContractError::Invalid {
                code: "selected_repair_candidate_missing",
            })?;
        let target = target_by_id(plan, &candidate.target_id)?;
        let baseline =
            baselines
                .get(&target.target_id)
                .ok_or(ValidationContractError::Invalid {
                    code: "selected_repair_mutation_baseline_missing",
                })?;
        let target_operation =
            repair_target_operation_from_baseline(plan, target, failure, baseline)?;
        if self.schema_version != VALIDATION_SCHEMA_VERSION
            || self.candidate_id != highest.candidate_id
            || self.evaluation_hash != evaluation.evaluation_hash
            || self.intent.failure_revision_id != failure.failure_revision_id
            || self.intent.originating_gate_id != gate.gate_id
            || self.intent.target_id != target.target_id
            || self.intent.target_path != target.path
            || self.intent.target_operation != target_operation
            || self.intent.baseline_mutation_evidence_id != baseline.evidence().evidence_id
            || self.intent.repository_revision != failure.repository_revision
            || self.intent.validation_policy_id != policy.policy_id
            || self.intent.repair_budget_hash
                != stable_sha256(&[
                    "execution-protocol-v1:validation-repair-budget",
                    &canonical_json(&policy.repair_node_budget)?,
                ])
            || self.intent.supporting_evidence_ids != decision.supporting_evidence_ids
            || !self
                .intent
                .supporting_evidence_ids
                .contains(&self.intent.baseline_mutation_evidence_id)
            || self.intent.required_evidence_ids != target.required_evidence
            || self.intent.criterion_ids != target.acceptance_criteria
            || self.intent.validate().is_err()
            || self.repair_node.kind != super::NodeKind::ValidationRepair
            || !self.repair_node.required
            || !self.repair_node.dependencies.is_empty()
            || self.repair_node.budget != policy.repair_node_budget
            || self.repair_node.id
                != NodeId::new(format!(
                    "epv1:{}",
                    stable_sha256(&[
                        "execution-protocol-v1:validation-repair-node",
                        self.intent.failure_revision_id.as_str(),
                        self.intent.repair_intent_id.as_str(),
                        self.intent.target_id.as_str(),
                        self.intent.repository_revision.as_str(),
                    ])
                ))
            || self.selection_hash != self.expected_hash()?
        {
            return Err(ValidationContractError::Invalid {
                code: "repair_target_selection_invalid",
            });
        }
        Ok(())
    }

    fn expected_hash(&self) -> Result<String, ValidationContractError> {
        Ok(stable_sha256(&[
            "execution-protocol-v1:repair-target-selection",
            &canonical_json(&(
                self.schema_version,
                &self.candidate_id,
                &self.evaluation_hash,
                &self.intent,
                &self.repair_node,
            ))?,
        ]))
    }

    pub(crate) fn validate_execution_binding(
        &self,
        failure: &ValidationFailureRevisionV1,
        plan: &AcceptedPlan,
        baseline: &RepairMutationBaseline,
    ) -> Result<(), ValidationContractError> {
        let target = target_by_id(plan, &self.intent.target_id)?;
        let operation = repair_target_operation_from_baseline(plan, target, failure, baseline)?;
        if self.schema_version != VALIDATION_SCHEMA_VERSION
            || self.intent.failure_revision_id != failure.failure_revision_id
            || self.intent.originating_gate_id != failure.gate_id
            || self.intent.target_path != target.path
            || self.intent.target_operation != operation
            || self.intent.baseline_mutation_evidence_id != baseline.evidence().evidence_id
            || self.intent.repository_revision != failure.repository_revision
            || !self
                .intent
                .supporting_evidence_ids
                .contains(&baseline.evidence().evidence_id)
            || self.intent.required_evidence_ids != target.required_evidence
            || self.intent.criterion_ids != target.acceptance_criteria
            || self.intent.validate().is_err()
            || self.repair_node.kind != super::NodeKind::ValidationRepair
            || !self.repair_node.required
            || !self.repair_node.dependencies.is_empty()
            || self.repair_node.id
                != NodeId::new(format!(
                    "epv1:{}",
                    stable_sha256(&[
                        "execution-protocol-v1:validation-repair-node",
                        self.intent.failure_revision_id.as_str(),
                        self.intent.repair_intent_id.as_str(),
                        self.intent.target_id.as_str(),
                        self.intent.repository_revision.as_str(),
                    ])
                ))
            || self.selection_hash != self.expected_hash()?
        {
            return Err(ValidationContractError::Invalid {
                code: "repair_execution_binding_invalid",
            });
        }
        Ok(())
    }

    pub(crate) fn execution_purpose(
        &self,
        failure: &ValidationFailureRevisionV1,
    ) -> Result<TargetExecutionPurpose, ValidationContractError> {
        if self.intent.failure_revision_id != failure.failure_revision_id
            || self.intent.originating_gate_id != failure.gate_id
            || self.intent.repository_revision != failure.repository_revision
        {
            return Err(ValidationContractError::Invalid {
                code: "repair_execution_purpose_failure_mismatch",
            });
        }
        Ok(TargetExecutionPurpose::ValidationRepair {
            repair_intent_id: self.intent.repair_intent_id.clone(),
            failure_revision_id: self.intent.failure_revision_id.clone(),
            originating_gate_id: self.intent.originating_gate_id.clone(),
            validation_evidence_id: failure.validation_evidence_id.clone(),
            baseline_mutation_evidence_id: self.intent.baseline_mutation_evidence_id.clone(),
        })
    }
}

pub(crate) fn repair_target_for_selection(
    selection: &RepairTargetSelection,
    failure: &ValidationFailureRevisionV1,
    plan: &AcceptedPlan,
    baseline: &RepairMutationBaseline,
) -> Result<PlannedTargetV1, ValidationContractError> {
    selection.validate_execution_binding(failure, plan, baseline)?;
    let mut target = target_by_id(plan, &selection.intent.target_id)?.clone();
    target.operation = selection.intent.target_operation.clone();
    Ok(target)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationInvalidation {
    pub(crate) failure_revision_id: FailureRevisionId,
    pub(crate) repair_intent_id: RepairIntentId,
    pub(crate) repository_revision_before: RepositoryRevisionId,
    pub(crate) repository_revision_after: RepositoryRevisionId,
    pub(crate) invalidated_evidence_ids: BTreeSet<ValidationEvidenceId>,
    pub(crate) verified_repair_evidence_id: EvidenceId,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationRerunSchedule {
    pub(crate) schema_version: u16,
    pub(crate) rerun_id: EvidenceId,
    pub(crate) failure_revision_id: FailureRevisionId,
    pub(crate) repair_intent_id: RepairIntentId,
    pub(crate) originating_gate_id: ValidationGateId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) verified_repair_evidence_id: EvidenceId,
    pub(crate) invalidated_evidence_ids: BTreeSet<ValidationEvidenceId>,
}

impl ValidationRerunSchedule {
    pub(crate) fn new(
        invalidation: &ValidationInvalidation,
        selection: &RepairTargetSelection,
        gate: &ValidationGateV1,
    ) -> Result<Self, ValidationContractError> {
        if invalidation.failure_revision_id != selection.intent.failure_revision_id
            || invalidation.repair_intent_id != selection.intent.repair_intent_id
            || invalidation.repository_revision_before != selection.intent.repository_revision
            || invalidation.repository_revision_after == invalidation.repository_revision_before
            || invalidation.invalidated_evidence_ids.is_empty()
            || gate.gate_id != selection.intent.originating_gate_id
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_invalidation_binding_invalid",
            });
        }
        let mut schedule = Self {
            schema_version: VALIDATION_SCHEMA_VERSION,
            rerun_id: EvidenceId::new("pending:validation-rerun"),
            failure_revision_id: invalidation.failure_revision_id.clone(),
            repair_intent_id: invalidation.repair_intent_id.clone(),
            originating_gate_id: gate.gate_id.clone(),
            repository_revision: invalidation.repository_revision_after.clone(),
            verified_repair_evidence_id: invalidation.verified_repair_evidence_id.clone(),
            invalidated_evidence_ids: invalidation.invalidated_evidence_ids.clone(),
        };
        schedule.rerun_id = schedule.expected_id()?;
        Ok(schedule)
    }

    fn expected_id(&self) -> Result<EvidenceId, ValidationContractError> {
        Ok(EvidenceId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:validation-rerun",
                &canonical_json(&(
                    self.schema_version,
                    &self.failure_revision_id,
                    &self.repair_intent_id,
                    &self.originating_gate_id,
                    &self.repository_revision,
                    &self.verified_repair_evidence_id,
                    &self.invalidated_evidence_ids,
                ))?,
            ])
        )))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationRunRecord {
    pub(crate) request: ValidationProcessRequest,
    pub(crate) started: Option<ValidationProcessStarted>,
    pub(crate) completed: Option<ValidationProcessCompleted>,
    pub(crate) evidence: Option<ValidationEvidenceV1>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ValidationConvergenceReason {
    NoValidRepair,
    GateRunBudgetExhausted {
        gate_id: ValidationGateId,
    },
    InfrastructureFailure {
        kind: ValidationInfrastructureFailureKind,
        run_id: ValidationRunId,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationConvergence {
    pub(crate) convergence_id: EvidenceId,
    pub(crate) failure_revision_id: FailureRevisionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) reason: ValidationConvergenceReason,
}

impl ValidationConvergence {
    pub(crate) fn new(
        failure_revision_id: FailureRevisionId,
        repository_revision: RepositoryRevisionId,
        reason: ValidationConvergenceReason,
    ) -> Result<Self, ValidationContractError> {
        let mut convergence = Self {
            convergence_id: EvidenceId::new("pending:validation-convergence"),
            failure_revision_id,
            repository_revision,
            reason,
        };
        convergence.convergence_id = EvidenceId::new(format!(
            "epv1:{}",
            stable_sha256(&[
                "execution-protocol-v1:validation-convergence",
                &canonical_json(&(
                    &convergence.failure_revision_id,
                    &convergence.repository_revision,
                    &convergence.reason,
                ))?,
            ])
        ));
        Ok(convergence)
    }

    pub(crate) fn validate(&self) -> Result<(), ValidationContractError> {
        if self.failure_revision_id.is_empty()
            || self.repository_revision.is_empty()
            || self
                != &Self::new(
                    self.failure_revision_id.clone(),
                    self.repository_revision.clone(),
                    self.reason.clone(),
                )?
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_convergence_invalid",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationState {
    pub(crate) schema_version: u16,
    pub(crate) policy_id: ValidationPolicyId,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_revision_id: PlanRevisionId,
    pub(crate) repository_revision: RepositoryRevisionId,
    pub(crate) gates: BTreeMap<ValidationGateId, ValidationGateV1>,
    pub(crate) gate_order: Vec<ValidationGateId>,
    pub(crate) node_gates: BTreeMap<NodeId, Vec<ValidationGateId>>,
    pub(crate) runs: BTreeMap<ValidationRunId, ValidationRunRecord>,
    pub(crate) current_run_by_gate: BTreeMap<ValidationGateId, ValidationRunId>,
    pub(crate) evidence: BTreeMap<ValidationEvidenceId, ValidationEvidenceV1>,
    pub(crate) current_evidence_by_gate: BTreeMap<ValidationGateId, ValidationEvidenceId>,
    pub(crate) invalidated_evidence: BTreeSet<ValidationEvidenceId>,
    pub(crate) failures: BTreeMap<FailureRevisionId, ValidationFailureRevisionV1>,
    pub(crate) active_failure: Option<FailureRevisionId>,
    pub(crate) rankings: BTreeMap<FailureRevisionId, RepairCandidateRanking>,
    pub(crate) eligibility: BTreeMap<FailureRevisionId, RepairEligibilityEvaluation>,
    pub(crate) selections: BTreeMap<FailureRevisionId, RepairTargetSelection>,
    pub(crate) repair_contexts: RepairTargetContextLedger,
    pub(crate) invalidations: BTreeMap<FailureRevisionId, ValidationInvalidation>,
    pub(crate) reruns: BTreeMap<FailureRevisionId, ValidationRerunSchedule>,
    pub(crate) pending_rerun: Option<ValidationRerunSchedule>,
    pub(crate) convergence: Option<ValidationConvergence>,
}

impl ValidationState {
    pub(crate) fn new(
        gates: Vec<ValidationGateV1>,
        policy: &ValidationPolicyV1,
        plan: &AcceptedPlan,
    ) -> Result<Self, ValidationContractError> {
        if gates.is_empty() || gates.len() > MAX_GATES {
            return Err(ValidationContractError::Invalid {
                code: "validation_gate_set_invalid",
            });
        }
        let gate_order = gates.iter().map(|gate| gate.gate_id.clone()).collect();
        let repository_revision = gates
            .first()
            .map(|gate| gate.repository_revision.clone())
            .ok_or(ValidationContractError::Invalid {
                code: "validation_gate_set_invalid",
            })?;
        if gates
            .iter()
            .any(|gate| gate.repository_revision != repository_revision)
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_gate_revision_mismatch",
            });
        }
        let mut node_gates = BTreeMap::<NodeId, Vec<ValidationGateId>>::new();
        for gate in &gates {
            node_gates
                .entry(gate.node_id.clone())
                .or_default()
                .push(gate.gate_id.clone());
        }
        let gate_map = gates
            .into_iter()
            .map(|gate| (gate.gate_id.clone(), gate))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            schema_version: VALIDATION_SCHEMA_VERSION,
            policy_id: policy.policy_id.clone(),
            plan_id: plan.plan_id.clone(),
            plan_revision_id: plan.plan_revision_id.clone(),
            repository_revision,
            gates: gate_map,
            gate_order,
            node_gates,
            runs: BTreeMap::new(),
            current_run_by_gate: BTreeMap::new(),
            evidence: BTreeMap::new(),
            current_evidence_by_gate: BTreeMap::new(),
            invalidated_evidence: BTreeSet::new(),
            failures: BTreeMap::new(),
            active_failure: None,
            rankings: BTreeMap::new(),
            eligibility: BTreeMap::new(),
            selections: BTreeMap::new(),
            repair_contexts: RepairTargetContextLedger::new(),
            invalidations: BTreeMap::new(),
            reruns: BTreeMap::new(),
            pending_rerun: None,
            convergence: None,
        })
    }

    pub(crate) fn gate_for_node(&self, node_id: &NodeId) -> Option<&ValidationGateV1> {
        self.node_gates
            .get(node_id)
            .and_then(|gate_ids| gate_ids.first())
            .and_then(|gate_id| self.gates.get(gate_id))
    }

    pub(crate) fn next_gate(&self) -> Option<&ValidationGateV1> {
        self.next_gate_id()
            .and_then(|gate_id| self.gates.get(gate_id))
    }

    pub(crate) fn run_for_gate(&self, gate_id: &ValidationGateId) -> Option<&ValidationRunRecord> {
        self.current_run_by_gate
            .get(gate_id)
            .and_then(|run_id| self.runs.get(run_id))
            .filter(|run| run.request.schedule.repository_revision == self.repository_revision)
    }

    pub(crate) fn current_failure(&self) -> Option<&ValidationFailureRevisionV1> {
        self.active_failure
            .as_ref()
            .and_then(|failure_id| self.failures.get(failure_id))
    }

    pub(crate) fn next_gate_id(&self) -> Option<&ValidationGateId> {
        if self.active_failure.is_some() || self.convergence.is_some() {
            return None;
        }
        if let Some(rerun) = &self.pending_rerun {
            return Some(&rerun.originating_gate_id);
        }
        // An exact repair rerun may target a broad gate that shares its graph
        // node with the canonically last focused gate. Once that exact gate
        // passes, keep the same node as owner until every gate attached to it
        // has current-revision passing evidence. Otherwise global ordering
        // would select an earlier invalidated gate on another node while the
        // rerun owner is still active, leaving neither node able to progress.
        if let Some(origin_node_id) = self
            .reruns
            .values()
            .find(|rerun| rerun.repository_revision == self.repository_revision)
            .and_then(|rerun| self.gates.get(&rerun.originating_gate_id))
            .map(|gate| &gate.node_id)
            && let Some(gate_id) = self
                .node_gates
                .get(origin_node_id)
                .into_iter()
                .flatten()
                .find(|gate_id| self.gate_requires_current_pass(gate_id))
        {
            return Some(gate_id);
        }
        self.gate_order
            .iter()
            .find(|gate_id| self.gate_requires_current_pass(gate_id))
    }

    fn gate_requires_current_pass(&self, gate_id: &ValidationGateId) -> bool {
        self.current_evidence_by_gate
            .get(gate_id)
            .and_then(|evidence_id| self.evidence.get(evidence_id))
            .is_none_or(|evidence| {
                evidence.repository_revision != self.repository_revision
                    || !matches!(evidence.outcome, ValidationEvidenceOutcome::Passed)
            })
    }

    pub(crate) fn apply(
        &mut self,
        event: &ValidationEvent,
        policy: &ValidationPolicyV1,
    ) -> Result<(), ValidationContractError> {
        if self.policy_id != policy.policy_id {
            return Err(ValidationContractError::Invalid {
                code: "validation_state_policy_mismatch",
            });
        }
        if self.convergence.is_some()
            && !matches!(event, ValidationEvent::ConvergenceEvaluated { .. })
        {
            return Err(ValidationContractError::Invalid {
                code: "validation_event_after_convergence",
            });
        }
        match event {
            ValidationEvent::ValidationScheduled { request } => {
                let gate = self.gates.get(&request.schedule.gate_id).ok_or(
                    ValidationContractError::Invalid {
                        code: "validation_schedule_gate_unknown",
                    },
                )?;
                request.validate_against(gate, policy)?;
                let expected_kind =
                    self.pending_rerun
                        .as_ref()
                        .map_or(ValidationRunKind::Initial, |rerun| {
                            ValidationRunKind::ExactRepairRerun {
                                failure_revision_id: rerun.failure_revision_id.clone(),
                                repair_intent_id: rerun.repair_intent_id.clone(),
                                verified_repair_evidence_id: rerun
                                    .verified_repair_evidence_id
                                    .clone(),
                            }
                        });
                if self.runs.contains_key(&request.schedule.run_id)
                    || self.next_gate_id() != Some(&gate.gate_id)
                    || self.run_for_gate(&gate.gate_id).is_some()
                    || request.schedule.repository_revision != self.repository_revision
                    || request.schedule.kind != expected_kind
                    || request.schedule.run_attempt
                        != self
                            .runs
                            .values()
                            .filter(|run| run.request.schedule.gate_id == gate.gate_id)
                            .count()
                            .saturating_add(1) as u32
                {
                    return Err(ValidationContractError::Invalid {
                        code: "validation_schedule_not_next",
                    });
                }
                self.current_run_by_gate
                    .insert(gate.gate_id.clone(), request.schedule.run_id.clone());
                self.runs.insert(
                    request.schedule.run_id.clone(),
                    ValidationRunRecord {
                        request: request.clone(),
                        started: None,
                        completed: None,
                        evidence: None,
                    },
                );
            }
            ValidationEvent::ValidationProcessStarted { started } => {
                let run =
                    self.runs
                        .get_mut(&started.run_id)
                        .ok_or(ValidationContractError::Invalid {
                            code: "validation_process_start_without_schedule",
                        })?;
                if self.current_run_by_gate.get(&run.request.schedule.gate_id)
                    != Some(&started.run_id)
                {
                    return Err(ValidationContractError::Invalid {
                        code: "validation_process_start_not_current",
                    });
                }
                started.validate_against(&run.request)?;
                if run.started.is_some() || run.completed.is_some() {
                    return Err(ValidationContractError::Invalid {
                        code: "validation_process_start_duplicate",
                    });
                }
                run.started = Some(started.clone());
            }
            ValidationEvent::ValidationProcessCompleted { completed } => {
                let run = self.runs.get_mut(&completed.run_id).ok_or(
                    ValidationContractError::Invalid {
                        code: "validation_process_completion_without_schedule",
                    },
                )?;
                if self.current_run_by_gate.get(&run.request.schedule.gate_id)
                    != Some(&completed.run_id)
                {
                    return Err(ValidationContractError::Invalid {
                        code: "validation_process_completion_not_current",
                    });
                }
                completed.validate_against(&run.request, run.started.as_ref())?;
                if run.completed.is_some() {
                    return Err(ValidationContractError::Invalid {
                        code: "validation_process_completion_duplicate",
                    });
                }
                run.completed = Some(completed.clone());
            }
            ValidationEvent::ValidationEvidenceRecorded { evidence } => {
                let run = self.runs.get_mut(&evidence.run_id).ok_or(
                    ValidationContractError::Invalid {
                        code: "validation_evidence_without_run",
                    },
                )?;
                if self.current_run_by_gate.get(&run.request.schedule.gate_id)
                    != Some(&evidence.run_id)
                {
                    return Err(ValidationContractError::Invalid {
                        code: "validation_evidence_run_not_current",
                    });
                }
                let completed = run
                    .completed
                    .as_ref()
                    .ok_or(ValidationContractError::Invalid {
                        code: "validation_evidence_before_completion",
                    })?;
                evidence.validate_against(&run.request, completed)?;
                if run.evidence.is_some() || self.evidence.contains_key(&evidence.evidence_id) {
                    return Err(ValidationContractError::Invalid {
                        code: "validation_evidence_duplicate",
                    });
                }
                run.evidence = Some(evidence.clone());
                self.current_evidence_by_gate
                    .insert(evidence.gate_id.clone(), evidence.evidence_id.clone());
                self.evidence
                    .insert(evidence.evidence_id.clone(), evidence.clone());
                if matches!(evidence.outcome, ValidationEvidenceOutcome::Passed)
                    && self.pending_rerun.as_ref().is_some_and(|rerun| {
                        rerun.originating_gate_id == evidence.gate_id
                            && rerun.repository_revision == evidence.repository_revision
                    })
                {
                    self.pending_rerun = None;
                }
            }
            ValidationEvent::ValidationFailureRevisionRecorded { failure } => {
                let evidence = self.evidence.get(&failure.validation_evidence_id).ok_or(
                    ValidationContractError::Invalid {
                        code: "validation_failure_without_evidence",
                    },
                )?;
                if failure != &ValidationFailureRevisionV1::from_evidence(evidence)?
                    || self.active_failure.is_some()
                    || self
                        .failures
                        .insert(failure.failure_revision_id.clone(), failure.clone())
                        .is_some()
                {
                    return Err(ValidationContractError::Invalid {
                        code: "validation_failure_revision_invalid",
                    });
                }
                self.active_failure = Some(failure.failure_revision_id.clone());
                self.pending_rerun = None;
            }
            ValidationEvent::RepairCandidatesRanked { ranking } => {
                if self.active_failure.as_ref() != Some(&ranking.failure_revision_id)
                    || self.rankings.contains_key(&ranking.failure_revision_id)
                {
                    return Err(ValidationContractError::Invalid {
                        code: "repair_ranking_not_current",
                    });
                }
                ranking.validate()?;
                self.rankings
                    .insert(ranking.failure_revision_id.clone(), ranking.clone());
            }
            ValidationEvent::RepairEligibilityEvaluated { evaluation } => {
                let ranking = self.rankings.get(&evaluation.failure_revision_id).ok_or(
                    ValidationContractError::Invalid {
                        code: "repair_eligibility_without_ranking",
                    },
                )?;
                evaluation.validate(ranking)?;
                if self
                    .eligibility
                    .insert(evaluation.failure_revision_id.clone(), evaluation.clone())
                    .is_some()
                {
                    return Err(ValidationContractError::Invalid {
                        code: "repair_eligibility_duplicate",
                    });
                }
            }
            ValidationEvent::RepairTargetSelected { selection } => {
                let failure_id = &selection.intent.failure_revision_id;
                if self.active_failure.as_ref() != Some(failure_id)
                    || !self.eligibility.contains_key(failure_id)
                    || self
                        .selections
                        .insert(failure_id.clone(), selection.clone())
                        .is_some()
                {
                    return Err(ValidationContractError::Invalid {
                        code: "repair_selection_not_authorized",
                    });
                }
            }
            ValidationEvent::RepairTargetContextPrepared { prepared } => {
                let failure = self
                    .current_failure()
                    .ok_or(ValidationContractError::Invalid {
                        code: "repair_target_context_without_active_failure",
                    })?;
                let selection = self.selections.get(&failure.failure_revision_id).ok_or(
                    ValidationContractError::Invalid {
                        code: "repair_target_context_without_selection",
                    },
                )?;
                let expected_purpose = selection.execution_purpose(failure)?;
                if prepared.node_id != selection.repair_node.id
                    || prepared.target_id != selection.intent.target_id
                    || prepared.manifest.purpose != expected_purpose
                    || prepared.manifest.plan_id != self.plan_id
                    || prepared.manifest.plan_revision_id != self.plan_revision_id
                    || prepared.manifest.repository_revision != self.repository_revision
                {
                    return Err(ValidationContractError::Invalid {
                        code: "repair_target_context_not_authorized",
                    });
                }
                self.repair_contexts
                    .record_prepared_context((**prepared).clone())
                    .map_err(|_| ValidationContractError::Invalid {
                        code: "repair_target_context_not_authorized",
                    })?;
                self.repair_contexts
                    .validate()
                    .map_err(|_| ValidationContractError::Invalid {
                        code: "repair_target_context_ledger_invalid",
                    })?;
            }
            ValidationEvent::PriorValidationInvalidated { invalidation } => {
                if self.active_failure.as_ref() != Some(&invalidation.failure_revision_id)
                    || invalidation.repository_revision_before != self.repository_revision
                    || invalidation.repository_revision_after
                        == invalidation.repository_revision_before
                    || invalidation.invalidated_evidence_ids.is_empty()
                    || invalidation.verified_repair_evidence_id.is_empty()
                    || invalidation
                        .invalidated_evidence_ids
                        .iter()
                        .any(|evidence_id| {
                            self.evidence.get(evidence_id).is_none_or(|evidence| {
                                evidence.repository_revision
                                    != invalidation.repository_revision_before
                            })
                        })
                    || self
                        .selections
                        .get(&invalidation.failure_revision_id)
                        .is_none_or(|selection| {
                            selection.intent.repair_intent_id != invalidation.repair_intent_id
                        })
                    || self
                        .invalidations
                        .contains_key(&invalidation.failure_revision_id)
                {
                    return Err(ValidationContractError::Invalid {
                        code: "validation_invalidation_not_authorized",
                    });
                }
                self.invalidated_evidence
                    .extend(invalidation.invalidated_evidence_ids.iter().cloned());
                self.invalidations.insert(
                    invalidation.failure_revision_id.clone(),
                    invalidation.clone(),
                );
                self.current_evidence_by_gate.retain(|_, evidence_id| {
                    !invalidation.invalidated_evidence_ids.contains(evidence_id)
                });
                self.repository_revision = invalidation.repository_revision_after.clone();
            }
            ValidationEvent::ValidationRerunScheduled { rerun } => {
                if self.active_failure.as_ref() != Some(&rerun.failure_revision_id)
                    || rerun.rerun_id != rerun.expected_id()?
                    || self
                        .invalidations
                        .get(&rerun.failure_revision_id)
                        .is_none_or(|invalidation| {
                            invalidation.repair_intent_id != rerun.repair_intent_id
                                || invalidation.repository_revision_after
                                    != rerun.repository_revision
                                || invalidation.verified_repair_evidence_id
                                    != rerun.verified_repair_evidence_id
                                || invalidation.invalidated_evidence_ids
                                    != rerun.invalidated_evidence_ids
                        })
                    || self
                        .selections
                        .get(&rerun.failure_revision_id)
                        .is_none_or(|selection| {
                            selection.intent.repair_intent_id != rerun.repair_intent_id
                                || selection.intent.originating_gate_id != rerun.originating_gate_id
                        })
                {
                    return Err(ValidationContractError::Invalid {
                        code: "validation_rerun_not_authorized",
                    });
                }
                self.reruns
                    .insert(rerun.failure_revision_id.clone(), rerun.clone());
                self.pending_rerun = Some(rerun.clone());
                self.active_failure = None;
                self.convergence = None;
            }
            ValidationEvent::ConvergenceEvaluated { convergence } => {
                if self.convergence.is_some()
                    || convergence.repository_revision != self.repository_revision
                    || convergence.validate().is_err()
                {
                    return Err(ValidationContractError::Invalid {
                        code: "validation_convergence_invalid",
                    });
                }
                self.convergence = Some(convergence.clone());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ValidationEvent {
    ValidationScheduled {
        request: ValidationProcessRequest,
    },
    ValidationProcessStarted {
        started: ValidationProcessStarted,
    },
    ValidationProcessCompleted {
        completed: ValidationProcessCompleted,
    },
    ValidationEvidenceRecorded {
        evidence: ValidationEvidenceV1,
    },
    ValidationFailureRevisionRecorded {
        failure: ValidationFailureRevisionV1,
    },
    RepairCandidatesRanked {
        ranking: RepairCandidateRanking,
    },
    RepairEligibilityEvaluated {
        evaluation: RepairEligibilityEvaluation,
    },
    RepairTargetSelected {
        selection: RepairTargetSelection,
    },
    RepairTargetContextPrepared {
        prepared: Box<PreparedTargetContext>,
    },
    PriorValidationInvalidated {
        invalidation: ValidationInvalidation,
    },
    ValidationRerunScheduled {
        rerun: ValidationRerunSchedule,
    },
    ConvergenceEvaluated {
        convergence: ValidationConvergence,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ValidationEffectRequest {
    RunProcess {
        request: Box<ValidationProcessRequest>,
    },
    LoadRepairTargetContext {
        request: Box<super::TargetContextLoadRequest>,
    },
}

fn add_score(
    components: &mut BTreeMap<RepairScoreComponentKind, RepairScoreComponent>,
    kind: RepairScoreComponentKind,
    points: u32,
    supporting_evidence_ids: BTreeSet<EvidenceId>,
) {
    components
        .entry(kind)
        .and_modify(|component| {
            component.points = component.points.saturating_add(points);
            component
                .supporting_evidence_ids
                .extend(supporting_evidence_ids.iter().cloned());
        })
        .or_insert(RepairScoreComponent {
            kind,
            points,
            supporting_evidence_ids,
        });
}

fn expected_ranking_hash(
    ranking: &RepairCandidateRanking,
) -> Result<String, ValidationContractError> {
    Ok(stable_sha256(&[
        "execution-protocol-v1:repair-candidate-ranking",
        &canonical_json(&(
            ranking.schema_version,
            &ranking.failure_revision_id,
            &ranking.repository_revision,
            &ranking.candidates,
        ))?,
    ]))
}

fn expected_evaluation_hash(
    evaluation: &RepairEligibilityEvaluation,
) -> Result<String, ValidationContractError> {
    Ok(stable_sha256(&[
        "execution-protocol-v1:repair-eligibility-evaluation",
        &canonical_json(&(
            evaluation.schema_version,
            &evaluation.failure_revision_id,
            &evaluation.ranking_hash,
            &evaluation.decisions,
        ))?,
    ]))
}

fn target_by_id<'a>(
    plan: &'a AcceptedPlan,
    target_id: &TargetId,
) -> Result<&'a PlannedTargetV1, ValidationContractError> {
    plan.targets
        .iter()
        .find(|target| &target.target_id == target_id)
        .ok_or(ValidationContractError::Invalid {
            code: "repair_plan_target_missing",
        })
}

fn repair_budget_is_viable(budget: &NodeBudgetContract) -> bool {
    budget.max_model_calls > 0
        && budget.max_cost_micros > 0
        && budget.max_duration_ms > 0
        && budget.max_mutation_attempts > 0
        && budget.max_input_tokens_per_call > 0
        && budget.max_output_tokens_per_call > 0
}

const fn command_rank(command: ValidationCommandKind) -> u8 {
    match command {
        ValidationCommandKind::CargoTest => 0,
        ValidationCommandKind::NpmTest => 1,
        ValidationCommandKind::PythonPytest => 2,
        ValidationCommandKind::GoTestAll => 3,
        ValidationCommandKind::CargoBuild => 4,
        ValidationCommandKind::NpmBuild => 5,
        ValidationCommandKind::PythonBuild => 6,
        ValidationCommandKind::GoBuildAll => 7,
        ValidationCommandKind::NpmTypecheck => 8,
        ValidationCommandKind::NpmLint => 9,
    }
}

fn safe_code_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SAFE_CODE_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_json(value: &impl Serialize) -> Result<String, ValidationContractError> {
    serde_json::to_string(value).map_err(|_| ValidationContractError::Serialization)
}
