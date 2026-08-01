use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::ExecutionPhase;
use super::{IntendedChangeRecord, IntendedChangeStatus, PlannedTarget, now_rfc3339};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CanonicalExecutionState {
    Queued,
    Dispatching,
    Authenticating,
    RunningDiscovery,
    RunningPlanning,
    RunningImplementation,
    RunningValidation,
    RunningDiffReview,
    RunningCompletionEvaluation,
    RunningPublication,
    TerminalComplete,
    TerminalReview,
    TerminalPartial,
    TerminalBlocked,
    TerminalCancelled,
    TerminalFailed,
}

pub(super) fn canonical_running_state(phase: ExecutionPhase) -> CanonicalExecutionState {
    match phase {
        ExecutionPhase::Discovery | ExecutionPhase::ArtifactRepair => {
            CanonicalExecutionState::RunningDiscovery
        }
        ExecutionPhase::Planning => CanonicalExecutionState::RunningPlanning,
        ExecutionPhase::Implementation | ExecutionPhase::Repair => {
            CanonicalExecutionState::RunningImplementation
        }
        ExecutionPhase::Validation => CanonicalExecutionState::RunningValidation,
        ExecutionPhase::DiffReview => CanonicalExecutionState::RunningDiffReview,
        ExecutionPhase::CompletionEvaluation => {
            CanonicalExecutionState::RunningCompletionEvaluation
        }
        ExecutionPhase::Publication => CanonicalExecutionState::RunningPublication,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ImplementationCompletionStatus {
    NotStarted,
    InProgress,
    ReadyForValidation,
    Blocked,
    Partial,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct RemainingWorkItem {
    pub(super) change_id: String,
    pub(super) path: String,
    pub(super) role: String,
    pub(super) status: IntendedChangeStatus,
    pub(super) reason: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ValidationGateType {
    FocusedTest,
    TestSuite,
    Build,
    Lint,
    Typecheck,
    Custom,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ValidationStatus {
    Running,
    Passed,
    Failed,
    TimedOut,
    Cancelled,
    Superseded,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ValidationSource {
    ModelRequested,
    WorkerRequired,
    ResumeReused,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct ValidationEvidence {
    pub(super) evidence_id: String,
    pub(super) gate_id: String,
    pub(super) gate_type: ValidationGateType,
    pub(super) command: String,
    pub(super) normalized_command: String,
    pub(super) command_fingerprint: String,
    pub(super) source_tree_hash: String,
    pub(super) dependency_lock_hash: String,
    pub(super) started_at: String,
    pub(super) completed_at: Option<String>,
    pub(super) duration_ms: u64,
    pub(super) exit_code: Option<i32>,
    pub(super) status: ValidationStatus,
    pub(super) stdout_summary: String,
    pub(super) stderr_summary: String,
    pub(super) source: ValidationSource,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct RequiredGate {
    pub(super) gate_id: String,
    pub(super) gate_type: ValidationGateType,
    pub(super) required: bool,
    pub(super) command: String,
    pub(super) status: ValidationStatus,
    pub(super) evidence_id: Option<String>,
}

pub(super) fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn validation_fingerprint(
    command: &str,
    cwd: &str,
    source_tree_hash: &str,
    dependency_lock_hash: &str,
    relevant_environment_fingerprint: &str,
) -> String {
    let normalized = normalize_command(command);
    let material = format!(
        "{normalized}\0{cwd}\0{source_tree_hash}\0{dependency_lock_hash}\0{relevant_environment_fingerprint}"
    );
    hex::encode(Sha256::digest(material.as_bytes()))
}

pub(super) fn passed_evidence<'a>(
    ledger: &'a [ValidationEvidence],
    fingerprint: &str,
) -> Option<&'a ValidationEvidence> {
    ledger.iter().rev().find(|evidence| {
        evidence.command_fingerprint == fingerprint && evidence.status == ValidationStatus::Passed
    })
}

pub(super) fn supersede_stale_validation(
    ledger: &mut [ValidationEvidence],
    current_tree_hash: &str,
) -> usize {
    let mut count = 0;
    for evidence in ledger.iter_mut().filter(|evidence| {
        evidence.status == ValidationStatus::Passed
            && evidence.source_tree_hash != current_tree_hash
    }) {
        evidence.status = ValidationStatus::Superseded;
        count += 1;
    }
    count
}

pub(super) fn derive_remaining_work(changes: &[IntendedChangeRecord]) -> Vec<RemainingWorkItem> {
    changes
        .iter()
        .flat_map(|change| {
            change
                .targets
                .iter()
                .filter(|target| {
                    !matches!(
                        target.status,
                        IntendedChangeStatus::Applied | IntendedChangeStatus::Verified
                    )
                })
                .map(move |target| RemainingWorkItem {
                    change_id: change.change_id.clone(),
                    path: target.path.clone(),
                    role: target.role.clone(),
                    status: target.status,
                    reason: remaining_reason(target),
                })
        })
        .collect()
}

fn remaining_reason(target: &PlannedTarget) -> String {
    match target.status {
        IntendedChangeStatus::Planned => "planned target has not been applied",
        IntendedChangeStatus::InProgress => "target mutation is in progress",
        IntendedChangeStatus::Unresolved => "target has an unresolved implementation failure",
        IntendedChangeStatus::Partial => "target is only partially applied",
        IntendedChangeStatus::Applied | IntendedChangeStatus::Verified => "target is complete",
    }
    .into()
}

pub(super) fn implementation_completion_status(
    changes: &[IntendedChangeRecord],
    changed_paths: &BTreeSet<String>,
    has_unresolved_failure: bool,
    has_blocker: bool,
) -> ImplementationCompletionStatus {
    let targets = changes
        .iter()
        .flat_map(|change| change.targets.iter())
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return ImplementationCompletionStatus::NotStarted;
    }
    let applied = targets
        .iter()
        .filter(|target| {
            matches!(
                target.status,
                IntendedChangeStatus::Applied | IntendedChangeStatus::Verified
            ) && changed_paths.contains(&target.path)
        })
        .count();
    if applied == targets.len() && !has_unresolved_failure {
        return ImplementationCompletionStatus::ReadyForValidation;
    }
    if has_blocker && applied == 0 {
        return ImplementationCompletionStatus::Blocked;
    }
    if applied > 0 && (has_blocker || has_unresolved_failure) {
        return ImplementationCompletionStatus::Partial;
    }
    ImplementationCompletionStatus::InProgress
}

pub(super) fn legacy_remaining_work(items: &[RemainingWorkItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| format!("{}: {}", item.path, item.reason))
        .collect()
}

pub(super) fn validate_lifecycle_invariants(
    changes: &[IntendedChangeRecord],
    remaining_work: &[RemainingWorkItem],
    ledger: &[ValidationEvidence],
    current_tree_hash: &str,
) -> Result<(), String> {
    if derive_remaining_work(changes) != remaining_work {
        return Err("applied target is still present in remaining work".into());
    }
    let mut passed = BTreeSet::new();
    for evidence in ledger
        .iter()
        .filter(|evidence| evidence.status == ValidationStatus::Passed)
    {
        if evidence.source_tree_hash != current_tree_hash {
            return Err("passed validation evidence does not match the current source tree".into());
        }
        if !passed.insert((&evidence.command_fingerprint, &evidence.source_tree_hash)) {
            return Err("identical validation passed more than once for one source tree".into());
        }
    }
    if changes
        .iter()
        .flat_map(|change| &change.targets)
        .any(|target| {
            target.status == IntendedChangeStatus::Verified
                && !ledger.iter().any(|evidence| {
                    evidence.status == ValidationStatus::Passed
                        && evidence.source_tree_hash == current_tree_hash
                })
        })
    {
        return Err("verified target has no current validation evidence".into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn new_running_evidence(
    evidence_id: String,
    gate_id: String,
    gate_type: ValidationGateType,
    command: String,
    fingerprint: String,
    source_tree_hash: String,
    dependency_lock_hash: String,
    source: ValidationSource,
) -> ValidationEvidence {
    ValidationEvidence {
        evidence_id,
        gate_id,
        gate_type,
        normalized_command: normalize_command(&command),
        command,
        command_fingerprint: fingerprint,
        source_tree_hash,
        dependency_lock_hash,
        started_at: now_rfc3339(),
        completed_at: None,
        duration_ms: 0,
        exit_code: None,
        status: ValidationStatus::Running,
        stdout_summary: String::new(),
        stderr_summary: String::new(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(statuses: &[IntendedChangeStatus]) -> IntendedChangeRecord {
        IntendedChangeRecord {
            change_id: "theme".into(),
            intent: "add theme".into(),
            status: IntendedChangeStatus::Planned,
            target: String::new(),
            targets: statuses
                .iter()
                .enumerate()
                .map(|(index, status)| PlannedTarget {
                    path: format!("src/{index}.tsx"),
                    role: "source".into(),
                    new_file: false,
                    status: *status,
                })
                .collect(),
            attempts: Vec::new(),
            recovery: None,
        }
    }

    #[test]
    fn all_applied_targets_are_ready_without_repository_snapshot_or_budget_state() {
        let changes = vec![change(&[
            IntendedChangeStatus::Applied,
            IntendedChangeStatus::Verified,
        ])];
        let paths = ["src/0.tsx".into(), "src/1.tsx".into()]
            .into_iter()
            .collect();
        assert_eq!(
            implementation_completion_status(&changes, &paths, false, false),
            ImplementationCompletionStatus::ReadyForValidation
        );
        assert!(derive_remaining_work(&changes).is_empty());
    }

    #[test]
    fn validation_fingerprint_is_normalized_and_tree_bound() {
        let first = validation_fingerprint("npm   test", ".", "tree-a", "lock", "env");
        assert_eq!(
            first,
            validation_fingerprint("npm test", ".", "tree-a", "lock", "env")
        );
        assert_ne!(
            first,
            validation_fingerprint("npm test", ".", "tree-b", "lock", "env")
        );
    }

    #[test]
    fn aops_226_four_target_fixture_completes_and_reuses_three_gates() {
        let changes = vec![change(&[
            IntendedChangeStatus::Applied,
            IntendedChangeStatus::Applied,
            IntendedChangeStatus::Applied,
            IntendedChangeStatus::Applied,
        ])];
        let paths = (0..4).map(|index| format!("src/{index}.tsx")).collect();
        assert_eq!(
            implementation_completion_status(&changes, &paths, false, false),
            ImplementationCompletionStatus::ReadyForValidation
        );
        let mut ledger = Vec::new();
        for (id, command) in [
            ("focused", "npm test -- theme"),
            ("test", "npm test"),
            ("build", "npm run build"),
        ] {
            let fingerprint = validation_fingerprint(command, ".", "tree", "lock", "env");
            let mut evidence = new_running_evidence(
                id.into(),
                id.into(),
                ValidationGateType::TestSuite,
                command.into(),
                fingerprint.clone(),
                "tree".into(),
                "lock".into(),
                ValidationSource::WorkerRequired,
            );
            evidence.status = ValidationStatus::Passed;
            ledger.push(evidence);
            assert!(passed_evidence(&ledger, &fingerprint).is_some());
        }
        assert!(validate_lifecycle_invariants(&changes, &[], &ledger, "tree").is_ok());
    }

    #[test]
    fn contradictory_remaining_work_and_duplicate_validation_fail_invariants() {
        let changes = vec![change(&[IntendedChangeStatus::Applied])];
        let stale_remaining = vec![RemainingWorkItem {
            change_id: "theme".into(),
            path: "src/0.tsx".into(),
            role: "source".into(),
            status: IntendedChangeStatus::Planned,
            reason: "stale".into(),
        }];
        assert!(validate_lifecycle_invariants(&changes, &stale_remaining, &[], "tree").is_err());

        let fingerprint = validation_fingerprint("npm test", ".", "tree", "lock", "env");
        let mut evidence = new_running_evidence(
            "one".into(),
            "test".into(),
            ValidationGateType::TestSuite,
            "npm test".into(),
            fingerprint,
            "tree".into(),
            "lock".into(),
            ValidationSource::WorkerRequired,
        );
        evidence.status = ValidationStatus::Passed;
        let mut duplicate = evidence.clone();
        duplicate.evidence_id = "two".into();
        assert!(
            validate_lifecycle_invariants(&changes, &[], &[evidence, duplicate], "tree").is_err()
        );
    }

    #[test]
    fn source_mutation_supersedes_passed_evidence_and_round_trip_preserves_it() {
        let fingerprint = validation_fingerprint("cargo test", ".", "old", "lock", "env");
        let mut evidence = new_running_evidence(
            "test-old".into(),
            "test".into(),
            ValidationGateType::TestSuite,
            "cargo test".into(),
            fingerprint,
            "old".into(),
            "lock".into(),
            ValidationSource::WorkerRequired,
        );
        evidence.status = ValidationStatus::Passed;
        let encoded = serde_json::to_string(&evidence).unwrap();
        let mut ledger = vec![serde_json::from_str(&encoded).unwrap()];
        assert_eq!(supersede_stale_validation(&mut ledger, "new"), 1);
        assert_eq!(ledger[0].status, ValidationStatus::Superseded);
    }
}
