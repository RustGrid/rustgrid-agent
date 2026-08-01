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
    Preparing,
    InProgress,
    ReadyForValidation,
    PartialReadyForValidation,
    Blocked,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ImplementationSubstate {
    #[default]
    Preparing,
    Mutating,
    Repairing,
    ReadyForValidation,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ToolProgressClass {
    Productive,
    Neutral,
    RecoverableFailure,
    BlockingFailure,
    Duplicate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ImplementationProgressAction {
    Continue,
    FirstWriteDelayed,
    BlockedBeforeFirstWrite,
    BlockedAfterWrite,
}

pub(super) const MAX_PREPARATION_MODEL_CALLS: usize = 8;
pub(super) const FIRST_WRITE_DELAY_CALL: usize = 6;
pub(super) const MAX_CONSECUTIVE_PREPARATION_READS: usize = 6;
pub(super) const MAX_RECOVERABLE_READ_FAILURES: usize = 3;
pub(super) const MAX_POST_WRITE_STAGNANT_CALLS: usize = 4;

pub(super) fn implementation_progress_action(
    implementation_calls: usize,
    successful_writes: u32,
    consecutive_preparation_reads: usize,
    recoverable_read_failures: usize,
    repeated_identical_read_failures: usize,
    guided_recovery_issued: bool,
    calls_since_repository_progress: usize,
) -> ImplementationProgressAction {
    if successful_writes > 0 {
        return if calls_since_repository_progress >= MAX_POST_WRITE_STAGNANT_CALLS {
            ImplementationProgressAction::BlockedAfterWrite
        } else {
            ImplementationProgressAction::Continue
        };
    }

    if guided_recovery_issued
        && (implementation_calls >= MAX_PREPARATION_MODEL_CALLS
            || recoverable_read_failures > MAX_RECOVERABLE_READ_FAILURES
            || repeated_identical_read_failures >= 3)
    {
        return ImplementationProgressAction::BlockedBeforeFirstWrite;
    }

    if !guided_recovery_issued
        && (implementation_calls >= FIRST_WRITE_DELAY_CALL
            || consecutive_preparation_reads >= MAX_CONSECUTIVE_PREPARATION_READS
            || repeated_identical_read_failures >= 2)
    {
        return ImplementationProgressAction::FirstWriteDelayed;
    }

    ImplementationProgressAction::Continue
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
    let changed_in_progress = targets
        .iter()
        .filter(|target| {
            matches!(
                target.status,
                IntendedChangeStatus::InProgress | IntendedChangeStatus::Partial
            ) && changed_paths.contains(&target.path)
        })
        .count();
    if applied == targets.len() && !has_unresolved_failure {
        return ImplementationCompletionStatus::ReadyForValidation;
    }
    if has_blocker && applied == 0 && changed_in_progress == 0 {
        return ImplementationCompletionStatus::Blocked;
    }
    if applied > 0 && (has_blocker || has_unresolved_failure) {
        return ImplementationCompletionStatus::PartialReadyForValidation;
    }
    if applied == 0 && (has_unresolved_failure || changed_in_progress > 0) {
        return ImplementationCompletionStatus::InProgress;
    }
    if applied == 0 {
        return ImplementationCompletionStatus::Preparing;
    }
    ImplementationCompletionStatus::InProgress
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ValidationEntryDecision {
    CompleteImplementation,
    UsefulPartialImplementation,
    ResumedImplementation,
    ForbiddenNoImplementationChanges,
    ForbiddenIncompletePreparation,
}

pub(super) fn validation_entry_decision(
    status: ImplementationCompletionStatus,
    changed_path_count: usize,
    resumed_relevant_changes: bool,
    partial_work_explicitly_unresolved: bool,
) -> ValidationEntryDecision {
    if changed_path_count == 0 {
        return ValidationEntryDecision::ForbiddenNoImplementationChanges;
    }
    if resumed_relevant_changes
        && matches!(
            status,
            ImplementationCompletionStatus::ReadyForValidation
                | ImplementationCompletionStatus::PartialReadyForValidation
                | ImplementationCompletionStatus::InProgress
        )
    {
        return ValidationEntryDecision::ResumedImplementation;
    }
    match status {
        ImplementationCompletionStatus::ReadyForValidation => {
            ValidationEntryDecision::CompleteImplementation
        }
        ImplementationCompletionStatus::PartialReadyForValidation => {
            ValidationEntryDecision::UsefulPartialImplementation
        }
        ImplementationCompletionStatus::InProgress if partial_work_explicitly_unresolved => {
            ValidationEntryDecision::UsefulPartialImplementation
        }
        ImplementationCompletionStatus::NotStarted
        | ImplementationCompletionStatus::Preparing
        | ImplementationCompletionStatus::InProgress
        | ImplementationCompletionStatus::Blocked => {
            ValidationEntryDecision::ForbiddenIncompletePreparation
        }
    }
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
    use crate::hosted::orchestration::PhaseLedger;

    const AOPS_226_TARGETS: [&str; 5] = [
        "src/components/theme/ThemeProvider.tsx",
        "src/components/theme/ThemeToggle.tsx",
        "src/styles/globals.css",
        "tests/theme-provider.test.tsx",
        "tests/theme-tokens.test.ts",
    ];

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

    fn aops_226_change(statuses: &[IntendedChangeStatus]) -> IntendedChangeRecord {
        assert_eq!(statuses.len(), AOPS_226_TARGETS.len());
        let mut change = change(statuses);
        change.change_id = "aops-226-light-blue-theme".into();
        change.intent = "implement the light-blue theme and focused coverage".into();
        for (target, path) in change.targets.iter_mut().zip(AOPS_226_TARGETS) {
            target.path = path.into();
            target.role = if path.starts_with("tests/") {
                "test"
            } else {
                "source"
            }
            .into();
        }
        change
    }

    #[test]
    fn implementation_substates_and_tool_progress_classes_have_stable_wire_names() {
        assert_eq!(
            ImplementationSubstate::default(),
            ImplementationSubstate::Preparing
        );
        for (substate, expected) in [
            (ImplementationSubstate::Preparing, "preparing"),
            (ImplementationSubstate::Mutating, "mutating"),
            (ImplementationSubstate::Repairing, "repairing"),
            (
                ImplementationSubstate::ReadyForValidation,
                "ready_for_validation",
            ),
        ] {
            assert_eq!(serde_json::to_value(substate).unwrap(), expected);
        }
        for (class, expected) in [
            (ToolProgressClass::Productive, "productive"),
            (ToolProgressClass::Neutral, "neutral"),
            (ToolProgressClass::RecoverableFailure, "recoverable_failure"),
            (ToolProgressClass::BlockingFailure, "blocking_failure"),
            (ToolProgressClass::Duplicate, "duplicate"),
        ] {
            assert_eq!(serde_json::to_value(class).unwrap(), expected);
        }
    }

    #[test]
    fn preparation_gets_six_productive_calls_then_one_guided_recovery_turn() {
        for implementation_calls in 1..FIRST_WRITE_DELAY_CALL {
            assert_eq!(
                implementation_progress_action(
                    implementation_calls,
                    0,
                    implementation_calls,
                    0,
                    0,
                    false,
                    implementation_calls,
                ),
                ImplementationProgressAction::Continue
            );
        }
        assert_eq!(
            implementation_progress_action(
                FIRST_WRITE_DELAY_CALL,
                0,
                MAX_CONSECUTIVE_PREPARATION_READS,
                0,
                0,
                false,
                FIRST_WRITE_DELAY_CALL,
            ),
            ImplementationProgressAction::FirstWriteDelayed
        );
        assert_eq!(
            implementation_progress_action(7, 0, 0, 0, 0, true, 7),
            ImplementationProgressAction::Continue
        );
        assert_eq!(
            implementation_progress_action(MAX_PREPARATION_MODEL_CALLS, 0, 0, 0, 0, true, 8),
            ImplementationProgressAction::BlockedBeforeFirstWrite
        );
    }

    #[test]
    fn recoverable_read_failures_preserve_partial_progress_and_bound_identical_loops() {
        // A partially successful batch contributes useful preparation even when one path needs
        // deterministic individual fallback.
        let partial_batch = [
            ToolProgressClass::Productive,
            ToolProgressClass::RecoverableFailure,
        ];
        assert!(partial_batch.contains(&ToolProgressClass::Productive));
        assert!(partial_batch.contains(&ToolProgressClass::RecoverableFailure));
        assert_eq!(
            implementation_progress_action(3, 0, 1, MAX_RECOVERABLE_READ_FAILURES, 1, false, 3),
            ImplementationProgressAction::Continue
        );

        // The second identical range/path failure requests recovery but does not terminate the
        // implementation. Only a third identical failure after that recovery turn blocks it.
        assert_eq!(
            implementation_progress_action(2, 0, 0, 2, 2, false, 2),
            ImplementationProgressAction::FirstWriteDelayed
        );
        assert_eq!(
            implementation_progress_action(3, 0, 0, 3, 3, true, 3),
            ImplementationProgressAction::BlockedBeforeFirstWrite
        );

        // A successful individual fallback is productive and therefore provides the caller a
        // healthy continuation turn.
        let fallback = [
            ToolProgressClass::RecoverableFailure,
            ToolProgressClass::Productive,
        ];
        assert_eq!(fallback.last(), Some(&ToolProgressClass::Productive));
    }

    #[test]
    fn post_write_stagnation_requires_four_consecutive_calls_and_resets_on_progress() {
        assert_eq!(
            implementation_progress_action(8, 1, 0, 0, 0, false, 3),
            ImplementationProgressAction::Continue
        );
        assert_eq!(
            implementation_progress_action(9, 1, 0, 0, 0, false, MAX_POST_WRITE_STAGNANT_CALLS,),
            ImplementationProgressAction::BlockedAfterWrite
        );
        assert_eq!(
            implementation_progress_action(10, 2, 0, 0, 0, false, 0),
            ImplementationProgressAction::Continue
        );
    }

    #[test]
    fn completion_reconciliation_distinguishes_every_implementation_state() {
        let no_paths = BTreeSet::new();
        assert_eq!(
            implementation_completion_status(&[], &no_paths, false, false),
            ImplementationCompletionStatus::NotStarted
        );

        let planned = vec![change(&[
            IntendedChangeStatus::Planned,
            IntendedChangeStatus::Planned,
        ])];
        assert_eq!(
            implementation_completion_status(&planned, &no_paths, false, false),
            ImplementationCompletionStatus::Preparing
        );
        assert_eq!(
            implementation_completion_status(&planned, &no_paths, false, true),
            ImplementationCompletionStatus::Blocked
        );

        let partial = vec![change(&[
            IntendedChangeStatus::Applied,
            IntendedChangeStatus::Planned,
        ])];
        let first_path = BTreeSet::from(["src/0.tsx".into()]);
        assert_eq!(
            implementation_completion_status(&partial, &first_path, false, false),
            ImplementationCompletionStatus::InProgress
        );
        assert_eq!(
            implementation_completion_status(&partial, &first_path, true, false),
            ImplementationCompletionStatus::PartialReadyForValidation
        );

        let restored = vec![change(&[
            IntendedChangeStatus::InProgress,
            IntendedChangeStatus::InProgress,
        ])];
        let restored_paths = BTreeSet::from(["src/0.tsx".into(), "src/1.tsx".into()]);
        let restored_status =
            implementation_completion_status(&restored, &restored_paths, false, false);
        assert_eq!(restored_status, ImplementationCompletionStatus::InProgress);
        assert_eq!(
            validation_entry_decision(restored_status, restored_paths.len(), true, false),
            ValidationEntryDecision::ResumedImplementation
        );

        let complete = vec![change(&[
            IntendedChangeStatus::Applied,
            IntendedChangeStatus::Verified,
        ])];
        let all_paths = BTreeSet::from(["src/0.tsx".into(), "src/1.tsx".into()]);
        assert_eq!(
            implementation_completion_status(&complete, &all_paths, false, false),
            ImplementationCompletionStatus::ReadyForValidation
        );
    }

    #[test]
    fn validation_entry_requires_changed_and_reconciled_repository_state() {
        assert_eq!(
            validation_entry_decision(ImplementationCompletionStatus::Preparing, 0, false, false,),
            ValidationEntryDecision::ForbiddenNoImplementationChanges
        );
        assert_eq!(
            validation_entry_decision(ImplementationCompletionStatus::Preparing, 1, false, false,),
            ValidationEntryDecision::ForbiddenIncompletePreparation
        );
        assert_eq!(
            validation_entry_decision(
                ImplementationCompletionStatus::ReadyForValidation,
                5,
                false,
                false,
            ),
            ValidationEntryDecision::CompleteImplementation
        );
        assert_eq!(
            validation_entry_decision(
                ImplementationCompletionStatus::PartialReadyForValidation,
                2,
                false,
                true,
            ),
            ValidationEntryDecision::UsefulPartialImplementation
        );
        assert_eq!(
            validation_entry_decision(ImplementationCompletionStatus::InProgress, 2, true, true,),
            ValidationEntryDecision::ResumedImplementation
        );
        assert_eq!(
            validation_entry_decision(
                ImplementationCompletionStatus::ReadyForValidation,
                0,
                true,
                false,
            ),
            ValidationEntryDecision::ForbiddenNoImplementationChanges
        );
    }

    #[test]
    fn aops_226_targets_advance_in_order_and_derive_remaining_work_after_each_write() {
        let mut changes = vec![aops_226_change(&[
            IntendedChangeStatus::Planned,
            IntendedChangeStatus::Planned,
            IntendedChangeStatus::Planned,
            IntendedChangeStatus::Planned,
            IntendedChangeStatus::Planned,
        ])];
        assert_eq!(
            derive_remaining_work(&changes)
                .into_iter()
                .map(|item| item.path)
                .collect::<Vec<_>>(),
            AOPS_226_TARGETS
        );

        let mut changed_paths = BTreeSet::new();
        for (index, path) in AOPS_226_TARGETS.iter().enumerate() {
            changes[0].targets[index].status = IntendedChangeStatus::Applied;
            changed_paths.insert((*path).to_owned());
            let remaining = derive_remaining_work(&changes);
            assert_eq!(remaining.len(), AOPS_226_TARGETS.len() - index - 1);
            assert_eq!(
                remaining
                    .iter()
                    .map(|item| item.path.as_str())
                    .collect::<Vec<_>>(),
                AOPS_226_TARGETS[index + 1..]
            );
            assert_eq!(
                implementation_completion_status(&changes, &changed_paths, false, false),
                if index + 1 == AOPS_226_TARGETS.len() {
                    ImplementationCompletionStatus::ReadyForValidation
                } else {
                    ImplementationCompletionStatus::InProgress
                }
            );
        }
    }

    #[test]
    fn aops_226_applies_all_five_targets_within_the_shared_implementation_repair_budget() {
        let mut ledger = PhaseLedger::new(60, ExecutionPhase::Discovery);
        for _ in 0..4 {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::Planning);
        for _ in 0..2 {
            ledger.begin_model_call().unwrap();
        }
        assert_eq!(ledger.apply_ticket_complexity(AOPS_226_TARGETS.len()), 20);

        let mut changes = vec![aops_226_change(&[
            IntendedChangeStatus::Planned,
            IntendedChangeStatus::Planned,
            IntendedChangeStatus::Planned,
            IntendedChangeStatus::Planned,
            IntendedChangeStatus::Planned,
        ])];
        let mut inspected_targets = BTreeSet::new();
        let mut changed_paths = BTreeSet::new();
        ledger.transition(ExecutionPhase::Implementation);

        for (index, path) in AOPS_226_TARGETS.iter().enumerate() {
            ledger.begin_model_call().unwrap();
            inspected_targets.insert((*path).to_owned());

            // Reading the current target is sufficient to write it; later targets do not have to
            // be inspected before this mutation is applied.
            assert!(
                AOPS_226_TARGETS[index + 1..]
                    .iter()
                    .all(|later| !inspected_targets.contains(*later))
            );

            // Exercise the shared repair reserve deterministically: the fifth implementation
            // call encounters a recoverable mutation failure, and one repair call applies it.
            if index + 1 == AOPS_226_TARGETS.len() {
                assert_eq!(derive_remaining_work(&changes).len(), 1);
                ledger.transition(ExecutionPhase::Repair);
                ledger.begin_model_call().unwrap();
            }

            changes[0].targets[index].status = IntendedChangeStatus::Applied;
            changed_paths.insert((*path).to_owned());
            assert_eq!(
                derive_remaining_work(&changes)
                    .into_iter()
                    .map(|item| item.path)
                    .collect::<Vec<_>>(),
                AOPS_226_TARGETS[index + 1..]
            );
        }

        assert_eq!(
            changed_paths,
            AOPS_226_TARGETS.into_iter().map(str::to_owned).collect()
        );
        assert!(derive_remaining_work(&changes).is_empty());
        assert_eq!(
            implementation_completion_status(&changes, &changed_paths, false, false),
            ImplementationCompletionStatus::ReadyForValidation
        );
        assert_eq!(ledger.implementation_repair_calls(), 6);
        assert!(ledger.implementation_repair_calls() <= 10);

        ledger.transition(ExecutionPhase::DiffReview);
        ledger.begin_model_call().unwrap();
        ledger.transition(ExecutionPhase::CompletionEvaluation);
        ledger.begin_model_call().unwrap();
        assert!(ledger.budgeted_calls() <= 20);
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
        assert_ne!(
            first,
            validation_fingerprint("npm test", "packages/ui", "tree-a", "lock", "env")
        );
        assert_ne!(
            first,
            validation_fingerprint("npm test", ".", "tree-a", "lock-updated", "env")
        );
        assert_ne!(
            first,
            validation_fingerprint("npm test", ".", "tree-a", "lock", "CI=true")
        );
    }

    #[test]
    fn aops_226_five_target_fixture_completes_and_reuses_three_tree_bound_gates() {
        let changes = vec![aops_226_change(&[
            IntendedChangeStatus::Applied,
            IntendedChangeStatus::Applied,
            IntendedChangeStatus::Applied,
            IntendedChangeStatus::Applied,
            IntendedChangeStatus::Applied,
        ])];
        let paths = AOPS_226_TARGETS.into_iter().map(str::to_owned).collect();
        assert_eq!(
            implementation_completion_status(&changes, &paths, false, false),
            ImplementationCompletionStatus::ReadyForValidation
        );
        let mut ledger = Vec::new();
        for (id, command, gate_type) in [
            (
                "focused",
                "npm test -- theme",
                ValidationGateType::FocusedTest,
            ),
            ("test", "npm test", ValidationGateType::TestSuite),
            ("build", "npm run build", ValidationGateType::Build),
        ] {
            let fingerprint = validation_fingerprint(command, ".", "tree-aops-226", "lock", "env");
            let mut evidence = new_running_evidence(
                id.into(),
                id.into(),
                gate_type,
                command.into(),
                fingerprint.clone(),
                "tree-aops-226".into(),
                "lock".into(),
                ValidationSource::WorkerRequired,
            );
            evidence.status = ValidationStatus::Passed;
            ledger.push(evidence);
            assert!(passed_evidence(&ledger, &fingerprint).is_some());
            let changed_tree_fingerprint =
                validation_fingerprint(command, ".", "tree-after-one-byte-change", "lock", "env");
            assert!(passed_evidence(&ledger, &changed_tree_fingerprint).is_none());
        }
        assert_eq!(ledger.len(), 3);
        assert!(validate_lifecycle_invariants(&changes, &[], &ledger, "tree-aops-226").is_ok());
        assert_eq!(
            supersede_stale_validation(&mut ledger, "tree-after-one-byte-change"),
            3
        );
        assert!(
            ledger
                .iter()
                .all(|evidence| evidence.status == ValidationStatus::Superseded)
        );
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
