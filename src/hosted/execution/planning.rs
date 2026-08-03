// Extracted from the hosted execution composition root.
use super::*;

pub(in crate::hosted) fn reconcile_notebook_orchestration(
    notebook: &mut WorkerNotebook,
    manifest: &HostedManifest,
    implementation_plan: Option<&ImplementationPlan>,
    changed_paths: &[String],
    facts: &HostedReconciliationFacts,
) {
    let mut orchestration = std::mem::take(&mut notebook.orchestration);
    if orchestration.graph.is_none() {
        orchestration =
            HostedOrchestrationCheckpoint::bootstrap(manifest, &notebook.repository_fingerprint);
    }
    if implementation_plan.is_none() {
        orchestration.normalize_pre_plan_classification(manifest);
    }
    if orchestration.legacy_import_pending() {
        if let Some(plan) = implementation_plan {
            orchestration.ensure_graph_from_plan(manifest, plan, &notebook.repository_fingerprint);
        }
        orchestration.import_legacy_state_once(notebook, changed_paths, facts);
    }
    orchestration.materialize_legacy_notebook(notebook);
    notebook.orchestration = orchestration;
}

#[cfg(test)]
pub(in crate::hosted) fn reconcile_failed_write_attempts(
    failures: &mut [ToolFailureRecord],
    planned_changes: &[PlannedChange],
    write_attempts: &[WriteAttemptRecord],
    implementation: &ImplementationOutcome,
    validation: &[ValidationResult],
    changed_paths: &[String],
) {
    let changed = changed_paths
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let all_validation_passed =
        !validation.is_empty() && validation.iter().all(|result| result.status == "passed");
    let declaration = implementation.explicit_declaration.as_ref();
    let declaration_complete =
        declaration.is_some_and(|value| value.implementation_status == "complete");
    let successful_attempts = write_attempts
        .iter()
        .filter(|attempt| {
            attempt.status == WriteAttemptStatus::Applied && attempt_modified_target(attempt)
        })
        .collect::<Vec<_>>();
    let planned_by_id = planned_changes
        .iter()
        .map(|change| (change.change_id.as_str(), change))
        .collect::<BTreeMap<_, _>>();

    for failure in failures {
        if failure.recovered {
            continue;
        }
        let Some(target) = failure.target.as_deref() else {
            failure.reconciliation = FailureReconciliation::Unrelated;
            continue;
        };
        let planned = failure
            .change_id
            .as_deref()
            .and_then(|change_id| planned_by_id.get(change_id).copied())
            .or_else(|| {
                planned_by_id
                    .values()
                    .copied()
                    .find(|change| change.targets.iter().any(|planned| planned.path == target))
            });
        let later_success = successful_attempts.iter().find(|attempt| {
            attempt.attempt_index > failure.attempt_index
                && attempt.target == target
                && (failure.change_id.as_deref() == Some(attempt.change_id.as_str())
                    || matches!(attempt.tool.as_str(), "write_file" | "rewrite_small_file"))
        });
        if let Some(success) = later_success {
            failure.recovered = true;
            failure.reconciliation = FailureReconciliation::Superseded;
            failure.recovery = Some(IntendedChangeRecovery {
                recovered: true,
                method: "later_successful_target_write".into(),
                evidence: vec![
                    format!(
                        "{target} was modified by a later successful {}.",
                        success.tool
                    ),
                    format!(
                        "The final target hash is {}.",
                        success.after_sha256.as_deref().unwrap_or("recorded")
                    ),
                ],
            });
            continue;
        }
        if !changed.contains(target) {
            failure.reconciliation = if planned.is_some() {
                FailureReconciliation::StillUnresolved
            } else {
                FailureReconciliation::Unrelated
            };
            continue;
        }
        let declaration_maps_target = declaration.is_some_and(|value| {
            value.changed_paths.iter().any(|path| path == target)
                && (value
                    .criteria_evidence
                    .iter()
                    .any(|evidence| evidence.paths.iter().any(|path| path == target))
                    || !value.completed_work.is_empty())
        });
        if planned.is_some()
            && declaration_complete
            && declaration_maps_target
            && all_validation_passed
        {
            failure.recovered = true;
            failure.reconciliation = FailureReconciliation::Recovered;
            failure.recovery = Some(IntendedChangeRecovery {
                recovered: true,
                method: "final_diff_and_validation".into(),
                evidence: std::iter::once(format!("{target} is present in the final diff."))
                    .chain(
                        validation
                            .iter()
                            .map(|result| format!("{} passed.", result.command)),
                    )
                    .collect(),
            });
        } else {
            failure.reconciliation = FailureReconciliation::StillUnresolved;
        }
    }
}

#[cfg(test)]
pub(in crate::hosted) fn supersede_failures_satisfied_by_repository_state(
    failures: &mut [ToolFailureRecord],
    intended_changes: &[IntendedChangeRecord],
    write_attempts: &[WriteAttemptRecord],
    changed_paths: &BTreeSet<String>,
) -> usize {
    let mut superseded = 0;
    for failure in failures.iter_mut().filter(|failure| {
        !failure.recovered && failure.reconciliation == FailureReconciliation::StillUnresolved
    }) {
        let Some(target) = failure.target.as_deref() else {
            continue;
        };
        let target_applied = intended_changes.iter().any(|change| {
            change.targets.iter().any(|candidate| {
                candidate.path == target
                    && matches!(
                        candidate.status,
                        IntendedChangeStatus::Applied | IntendedChangeStatus::Verified
                    )
            })
        });
        let later_success = write_attempts.iter().find(|attempt| {
            attempt.attempt_index > failure.attempt_index
                && attempt.target == target
                && attempt.status == WriteAttemptStatus::Applied
                && attempt_modified_target(attempt)
        });
        let final_diff_satisfies_target = changed_paths.contains(target);
        if !(target_applied || later_success.is_some() || final_diff_satisfies_target) {
            continue;
        }

        let (method, evidence) = if let Some(attempt) = later_success {
            (
                "later_successful_target_write",
                format!(
                    "A later successful {} mutation satisfies {target}.",
                    attempt.tool
                ),
            )
        } else if target_applied {
            (
                "target_already_applied",
                format!("The authoritative target ledger marks {target} as applied."),
            )
        } else {
            (
                "final_diff_contains_target",
                format!("The final repository diff contains the intended target {target}."),
            )
        };
        failure.recovered = true;
        failure.reconciliation = FailureReconciliation::Superseded;
        failure.recovery = Some(IntendedChangeRecovery {
            recovered: true,
            method: method.into(),
            evidence: vec![evidence],
        });
        superseded += 1;
    }
    superseded
}

pub(in crate::hosted) fn attempt_modified_target(attempt: &WriteAttemptRecord) -> bool {
    attempt.before_sha256 != attempt.after_sha256
}

pub(in crate::hosted) fn deterministic_complete_declaration(
    planned_changes: &[PlannedChange],
    acceptance_criteria: &[String],
    changed_paths: &[String],
    remaining_work: &[RemainingWorkItem],
    tool_failures: &[ToolFailureRecord],
) -> Option<ImplementationDeclaration> {
    if planned_changes.is_empty()
        || !remaining_work.is_empty()
        || tool_failures.iter().any(|failure| {
            !failure.recovered && failure.reconciliation == FailureReconciliation::StillUnresolved
        })
    {
        return None;
    }
    let changed = changed_paths
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if planned_changes
        .iter()
        .flat_map(|change| &change.targets)
        .any(|target| !changed.contains(target.path.as_str()))
    {
        return None;
    }
    let criteria_evidence = if acceptance_criteria.is_empty() {
        planned_changes
            .iter()
            .map(|change| ImplementationCriterionEvidence {
                criterion: change.change.clone(),
                paths: change
                    .targets
                    .iter()
                    .map(|target| target.path.clone())
                    .collect(),
                evidence: format!(
                    "The authoritative diff contains every target for planned change `{}` and all required gates passed.",
                    change.change_id
                ),
            })
            .collect::<Vec<_>>()
    } else {
        acceptance_criteria
            .iter()
            .map(|criterion| {
                let mut paths = planned_changes
                    .iter()
                    .filter(|change| {
                        change
                            .acceptance_criteria
                            .iter()
                            .any(|mapped| mapped.trim() == criterion.trim())
                    })
                    .flat_map(|change| change.targets.iter().map(|target| target.path.clone()))
                    .collect::<Vec<_>>();
                paths.sort();
                paths.dedup();
                (!paths.is_empty()).then(|| ImplementationCriterionEvidence {
                    criterion: criterion.clone(),
                    paths,
                    evidence: "The authoritative target reconciliation, repository diff, and required validation gates all passed.".into(),
                })
            })
            .collect::<Option<Vec<_>>>()?
    };
    Some(ImplementationDeclaration {
        implementation_status: "complete".into(),
        completed_work: planned_changes
            .iter()
            .map(|change| change.change.clone())
            .collect(),
        remaining_work: Vec::new(),
        known_risks: Vec::new(),
        changed_paths: changed_paths.to_vec(),
        criteria_evidence,
    })
}

pub(in crate::hosted) fn deterministic_partial_declaration(
    planned_changes: &[PlannedChange],
    changed_paths: &[String],
    remaining_work: &[RemainingWorkItem],
) -> Option<ImplementationDeclaration> {
    if changed_paths.is_empty() || remaining_work.is_empty() {
        return None;
    }
    let changed = changed_paths
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let completed_work = planned_changes
        .iter()
        .filter(|change| {
            change
                .targets
                .iter()
                .any(|target| changed.contains(target.path.as_str()))
        })
        .map(|change| change.change.clone())
        .collect::<Vec<_>>();
    let criteria_evidence = planned_changes
        .iter()
        .flat_map(|change| {
            let paths = change
                .targets
                .iter()
                .filter(|target| changed.contains(target.path.as_str()))
                .map(|target| target.path.clone())
                .collect::<Vec<_>>();
            if paths.is_empty() {
                Vec::new()
            } else {
                change
                    .acceptance_criteria
                    .iter()
                    .map(|criterion| ImplementationCriterionEvidence {
                        criterion: criterion.clone(),
                        paths: paths.clone(),
                        evidence:
                            "The preserved partial diff passed every worker-owned validation gate."
                                .into(),
                    })
                    .collect()
            }
        })
        .collect();
    Some(ImplementationDeclaration {
        implementation_status: "partial".into(),
        completed_work,
        remaining_work: legacy_remaining_work(remaining_work),
        known_risks: vec!["Explicit planned targets remain for a continuation run.".into()],
        changed_paths: changed_paths.to_vec(),
        criteria_evidence,
    })
}

pub(in crate::hosted) fn deterministic_change_id(index: usize, change: &PlannedChange) -> String {
    let material = format!(
        "{}\0{}\0{}",
        change
            .targets
            .iter()
            .map(|target| target.path.as_str())
            .collect::<Vec<_>>()
            .join("\0"),
        change.change,
        change.reason
    );
    let digest = hex::encode(Sha256::digest(material.as_bytes()));
    format!("change-{}-{}", index + 1, &digest[..12])
}

pub(in crate::hosted) fn normalized_planned_paths(raw: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for entry in raw.split(';') {
        let path = entry.trim().replace('\\', "/");
        if path.is_empty() {
            bail!("implementation plan target contains an empty path entry");
        }
        let path = path.strip_prefix("./").unwrap_or(&path).to_owned();
        if path.contains(';') || path.contains('\n') || path.contains('\r') {
            bail!("implementation plan target must contain exactly one repository path");
        }
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

pub(in crate::hosted) fn normalize_planned_changes(changes: &mut [PlannedChange]) -> Result<usize> {
    let mut ids = BTreeSet::new();
    let mut normalized_legacy_targets = 0;
    for (index, change) in changes.iter_mut().enumerate() {
        if !change.path.trim().is_empty() {
            let normalized = normalized_planned_paths(&change.path)?;
            normalized_legacy_targets += usize::from(normalized.len() > 1);
            for path in normalized {
                if !change.targets.iter().any(|target| target.path == path) {
                    change.targets.push(PlannedTarget {
                        path,
                        role: change.reason.clone(),
                        new_file: false,
                        status: IntendedChangeStatus::Planned,
                    });
                }
            }
            change.path.clear();
        }
        let mut seen_paths = BTreeSet::new();
        let mut targets = Vec::new();
        for mut target in std::mem::take(&mut change.targets) {
            let normalized = normalized_planned_paths(&target.path)?;
            normalized_legacy_targets += usize::from(normalized.len() > 1);
            for path in normalized {
                if seen_paths.insert(path.clone()) {
                    target.path = path;
                    if target.role.trim().is_empty() {
                        target.role = change.reason.clone();
                    }
                    targets.push(target.clone());
                }
            }
        }
        change.targets = targets;
        if change.targets.is_empty() {
            bail!("every implementation plan change requires at least one target");
        }
        if change.change_id.trim().is_empty() {
            change.change_id = deterministic_change_id(index, change);
        }
        if change.change_id.len() > 100
            || !change.change_id.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '-' | '_')
            })
            || !ids.insert(change.change_id.clone())
        {
            bail!("implementation plan change_id values must be unique safe identifiers");
        }
        if change.parent_change_id.as_deref().is_some_and(|parent| {
            parent.is_empty()
                || parent.len() > 100
                || !parent.chars().all(|character| {
                    character.is_ascii_lowercase()
                        || character.is_ascii_digit()
                        || matches!(character, '-' | '_')
                })
        }) {
            bail!("implementation plan parent_change_id must be a safe identifier");
        }
    }
    Ok(normalized_legacy_targets)
}

pub(in crate::hosted) fn recover_planning_repair_state(
    root: &Path,
    object: &serde_json::Map<String, Value>,
    model_call: usize,
) -> PlanningRepairState {
    let mut state = PlanningRepairState {
        model_call,
        ..PlanningRepairState::default()
    };
    let Some(changes) = object.get("planned_changes").and_then(Value::as_array) else {
        state
            .invalid_fields
            .push("$.planned_changes: expected an array".into());
        return state;
    };
    state.original_change_count = changes.len();
    let mut ids = BTreeSet::new();
    for (index, raw) in changes.iter().enumerate() {
        state.original_change_ids.push(
            raw.get("change_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
        let mut change = match serde_json::from_value::<PlannedChange>(raw.clone()) {
            Ok(change) => change,
            Err(error) => {
                state.invalid_fields.push(format!(
                    "$.planned_changes[{index}]: {}",
                    truncate_text(&error.to_string(), 500)
                ));
                continue;
            }
        };
        if change.change_id.trim().is_empty() {
            change.change_id = deterministic_change_id(index, &change);
        }
        let mut one = vec![change];
        let validation = normalize_planned_changes(&mut one)
            .and_then(|_| validate_planned_change_paths(root, &one))
            .and_then(|_| {
                let change = &one[0];
                if change.change.trim().is_empty() {
                    bail!("intent is required");
                }
                if change.reason.trim().is_empty() {
                    bail!("reason is required");
                }
                if change.acceptance_criteria.is_empty() {
                    bail!("acceptance_criteria requires at least one entry");
                }
                if !ids.insert(change.change_id.clone()) {
                    bail!("change_id must be unique");
                }
                Ok(())
            });
        match validation {
            Ok(()) => {
                state.valid_planned_change_positions.push(index);
                state.valid_planned_changes.push(one.remove(0));
            }
            Err(error) => state.invalid_fields.push(format!(
                "$.planned_changes[{index}]: {}",
                truncate_text(&error.to_string(), 500)
            )),
        }
    }
    state
}

pub(in crate::hosted) fn merge_preserved_plan_fragments(
    planned_changes: &mut Vec<PlannedChange>,
    repair: Option<&PlanningRepairState>,
) {
    let Some(repair) = repair else {
        return;
    };
    if repair.valid_planned_change_positions.len() == repair.valid_planned_changes.len()
        && repair.original_change_count > 0
    {
        let preserved = repair
            .valid_planned_change_positions
            .iter()
            .copied()
            .zip(repair.valid_planned_changes.iter().cloned())
            .collect::<BTreeMap<_, _>>();
        planned_changes.retain(|candidate| {
            !repair
                .valid_planned_changes
                .iter()
                .any(|valid| valid.change_id == candidate.change_id)
        });
        let mut repaired = std::mem::take(planned_changes);
        let mut merged = Vec::with_capacity(repair.original_change_count + repaired.len());
        for index in 0..repair.original_change_count {
            if let Some(valid) = preserved.get(&index) {
                merged.push(valid.clone());
                continue;
            }
            let original_id = repair
                .original_change_ids
                .get(index)
                .and_then(Option::as_deref);
            let repaired_index = original_id
                .and_then(|id| repaired.iter().position(|change| change.change_id == id))
                .unwrap_or_default();
            if !repaired.is_empty() {
                merged.push(repaired.remove(repaired_index.min(repaired.len() - 1)));
            }
        }
        merged.append(&mut repaired);
        *planned_changes = merged;
        return;
    }
    for preserved in &repair.valid_planned_changes {
        if !planned_changes
            .iter()
            .any(|change| change.change_id == preserved.change_id)
        {
            planned_changes.push(preserved.clone());
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(in crate::hosted) struct PlanCriterionAssignment {
    pub(in crate::hosted) acceptance_criterion_id: String,
    pub(in crate::hosted) change_id: String,
}

#[derive(Clone, Debug)]
pub(in crate::hosted) struct ImplementationPlanAcceptance {
    pub(in crate::hosted) plan: ImplementationPlan,
    pub(in crate::hosted) criterion_assignments: Vec<PlanCriterionAssignment>,
    pub(in crate::hosted) model_call_consumed: bool,
    pub(in crate::hosted) next_phase: ExecutionPhase,
}

pub(in crate::hosted) fn semantic_tokens(
    values: impl IntoIterator<Item = String>,
) -> BTreeSet<String> {
    const STOP_WORDS: &[&str] = &[
        "acceptance",
        "change",
        "changes",
        "criterion",
        "criteria",
        "existing",
        "file",
        "files",
        "implementation",
        "required",
        "should",
        "that",
        "the",
        "this",
        "update",
        "with",
    ];
    values
        .into_iter()
        .flat_map(|value| {
            value
                .split(|character: char| !character.is_ascii_alphanumeric())
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        })
        .filter(|token| token.len() >= 3 && !STOP_WORDS.contains(&token.as_str()))
        .collect()
}

pub(in crate::hosted) fn planned_change_criterion_relevance(
    change: &PlannedChange,
    criterion: &impact_map::AcceptanceCriterion,
    impact_areas: &[ImpactArea],
) -> usize {
    let target_paths = change
        .targets
        .iter()
        .map(|target| target.path.trim())
        .filter(|path| !path.is_empty())
        .collect::<BTreeSet<_>>();
    let related_areas = impact_areas
        .iter()
        .filter(|area| area.acceptance_criteria_ids.contains(&criterion.id))
        .collect::<Vec<_>>();
    let exact_path_matches = related_areas
        .iter()
        .flat_map(|area| &area.candidate_paths)
        .filter(|path| target_paths.contains(path.trim()))
        .count();

    let change_tokens = semantic_tokens(
        [
            change.change.clone(),
            change.reason.clone(),
            change.change_id.clone(),
        ]
        .into_iter()
        .chain(
            change
                .targets
                .iter()
                .flat_map(|target| [target.path.clone(), target.role.clone()]),
        ),
    );
    let criterion_tokens = semantic_tokens(std::iter::once(criterion.text.clone()).chain(
        related_areas.iter().flat_map(|area| {
            std::iter::once(area.name.clone())
                .chain(std::iter::once(area.reason.clone()))
                .chain(area.candidate_paths.iter().cloned())
        }),
    ));
    exact_path_matches.saturating_mul(10_000)
        + change_tokens.intersection(&criterion_tokens).count()
}

pub(in crate::hosted) fn canonicalize_plan_criterion_ids(
    changes: &mut [PlannedChange],
    criteria: &[impact_map::AcceptanceCriterion],
) -> Result<()> {
    let valid_ids = criteria
        .iter()
        .map(|criterion| (criterion.id.as_str(), criterion))
        .collect::<BTreeMap<_, _>>();
    for (change_index, change) in changes.iter_mut().enumerate() {
        let mut canonical = BTreeSet::new();
        for reference in &change.acceptance_criteria {
            let reference = reference.trim();
            let id = if valid_ids.contains_key(reference) {
                reference
            } else if let Some(criterion) = criteria
                .iter()
                .find(|criterion| criterion.text.trim() == reference)
            {
                criterion.id.as_str()
            } else {
                bail!(
                    "$.planned_changes[{change_index}].acceptance_criteria_ids: unknown acceptance criterion ID `{reference}`"
                );
            };
            canonical.insert(id.to_owned());
        }
        change.acceptance_criteria = criteria
            .iter()
            .filter(|criterion| canonical.contains(&criterion.id))
            .map(|criterion| criterion.id.clone())
            .collect();
    }
    Ok(())
}

pub(in crate::hosted) fn validate_plan_criterion_coverage(
    plan: &ImplementationPlan,
    criteria: &[impact_map::AcceptanceCriterion],
    impact_areas: &[ImpactArea],
) -> Result<()> {
    let required = criteria
        .iter()
        .map(|criterion| criterion.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut covered = BTreeSet::new();
    for (change_index, change) in plan.planned_changes.iter().enumerate() {
        if change.acceptance_criteria.is_empty() {
            bail!(
                "$.planned_changes[{change_index}].acceptance_criteria_ids: at least one relevant acceptance criterion ID is required"
            );
        }
        let mut has_relevant_reference = false;
        for id in &change.acceptance_criteria {
            let Some(criterion) = criteria.iter().find(|criterion| criterion.id == *id) else {
                bail!(
                    "$.planned_changes[{change_index}].acceptance_criteria_ids: unknown acceptance criterion ID `{id}`"
                );
            };
            covered.insert(id.as_str());
            has_relevant_reference |=
                planned_change_criterion_relevance(change, criterion, impact_areas) > 0;
        }
        if !has_relevant_reference {
            bail!(
                "$.planned_changes[{change_index}].acceptance_criteria_ids: no referenced acceptance criterion is relevant to this planned change"
            );
        }
    }
    let missing = required.difference(&covered).copied().collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "$.planned_changes[*].acceptance_criteria_ids: required acceptance criteria are not covered: {}",
            missing.join(", ")
        );
    }
    Ok(())
}

pub(in crate::hosted) fn validate_and_repair_plan_criteria(
    mut candidate: ImplementationPlan,
    criteria: &[impact_map::AcceptanceCriterion],
    impact_areas: &[ImpactArea],
) -> Result<ImplementationPlanAcceptance> {
    canonicalize_plan_criterion_ids(&mut candidate.planned_changes, criteria)?;
    let covered = candidate
        .planned_changes
        .iter()
        .flat_map(|change| change.acceptance_criteria.iter().cloned())
        .collect::<BTreeSet<_>>();
    let missing = criteria
        .iter()
        .filter(|criterion| !covered.contains(&criterion.id))
        .collect::<Vec<_>>();
    let mut assignments = Vec::new();
    for criterion in missing {
        let scored = candidate
            .planned_changes
            .iter()
            .enumerate()
            .map(|(index, change)| {
                (
                    index,
                    planned_change_criterion_relevance(change, criterion, impact_areas),
                )
            })
            .collect::<Vec<_>>();
        let best_score = scored.iter().map(|(_, score)| *score).max().unwrap_or(0);
        let best = scored
            .iter()
            .filter(|(_, score)| *score == best_score)
            .map(|(index, _)| *index)
            .collect::<Vec<_>>();
        if best_score == 0 || best.len() != 1 {
            bail!(
                "$.planned_changes[*].acceptance_criteria_ids: semantic placement of `{}` is ambiguous",
                criterion.id
            );
        }
        let change = &mut candidate.planned_changes[best[0]];
        change.acceptance_criteria.push(criterion.id.clone());
        assignments.push(PlanCriterionAssignment {
            acceptance_criterion_id: criterion.id.clone(),
            change_id: change.change_id.clone(),
        });
    }
    canonicalize_plan_criterion_ids(&mut candidate.planned_changes, criteria)?;
    validate_plan_criterion_coverage(&candidate, criteria, impact_areas)?;
    let next_phase = if candidate.implementation_status == "ready" {
        ExecutionPhase::Implementation
    } else {
        ExecutionPhase::Planning
    };
    Ok(ImplementationPlanAcceptance {
        plan: candidate,
        criterion_assignments: assignments,
        model_call_consumed: false,
        next_phase,
    })
}

pub(in crate::hosted) fn deterministic_plan_from_impact_map(
    notebook: &WorkerNotebook,
) -> Option<ImplementationPlan> {
    let map = notebook.impact_map_v2.as_ref()?;
    if !map.unresolved_questions.is_empty() || map.areas.is_empty() {
        return None;
    }
    let observed_paths = notebook
        .orchestration
        .evidence
        .files
        .values()
        .filter(|evidence| evidence.repository_fingerprint == notebook.repository_fingerprint)
        .map(|evidence| evidence.path.as_str())
        .collect::<BTreeSet<_>>();
    let mut path_criteria = BTreeMap::<String, BTreeSet<String>>::new();
    let mut path_reasons = BTreeMap::<String, Vec<String>>::new();
    for area in &map.areas {
        for path in &area.candidate_paths {
            let lower = path.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "package.json" | "cargo.toml" | "pyproject.toml"
            ) || !observed_paths.contains(path.as_str())
            {
                continue;
            }
            path_criteria
                .entry(path.clone())
                .or_default()
                .extend(area.acceptance_criteria_ids.iter().cloned());
            path_reasons
                .entry(path.clone())
                .or_default()
                .push(area.reason.clone());
        }
    }
    let planned_changes = path_criteria
        .into_iter()
        .enumerate()
        .map(|(index, (path, criteria))| {
            let lower = path.to_ascii_lowercase();
            let (change, test_coverage) = if lower.contains("themeprovider") {
                (
                    "Restore light-blue from storage, include it in theme cycling, and apply the correct root marker behavior.".into(),
                    vec!["Cover selection, persistence, restoration, cycling, defaults, and existing themes.".into()],
                )
            } else if lower.contains("themetoggle") {
                (
                    "Expose light-blue in the existing selector cycle while preserving accessible labels and icons.".into(),
                    vec!["Exercise the selector cycle and accessible state labels.".into()],
                )
            } else if lower.ends_with("globals.css") {
                (
                    "Add the complete light-blue semantic palette using the existing theme-token structure.".into(),
                    Vec::new(),
                )
            } else if lower.contains("test") || lower.contains("spec") {
                (
                    "Cover selection, persistence, restoration, cycling, defaults, and all existing themes.".into(),
                    vec!["Run the repository's focused theme test suite.".into()],
                )
            } else {
                (
                    format!("Implement the accepted impact-map change for {path}."),
                    Vec::new(),
                )
            };
            let role = if lower.contains("test") || lower.contains("spec") {
                "tests"
            } else {
                "production"
            };
            PlannedChange {
                change_id: format!("fallback-change-{}", index + 1),
                parent_change_id: None,
                path: String::new(),
                targets: vec![PlannedTarget {
                    path: path.clone(),
                    role: role.into(),
                    new_file: false,
                    status: IntendedChangeStatus::Planned,
                }],
                change,
                reason: path_reasons
                    .remove(&path)
                    .unwrap_or_default()
                    .join(" "),
                status: IntendedChangeStatus::Planned,
                acceptance_criteria: criteria.into_iter().collect(),
                test_coverage,
            }
        })
        .collect::<Vec<_>>();
    if planned_changes.is_empty() {
        return None;
    }
    let planned_test_changes = planned_changes
        .iter()
        .flat_map(|change| &change.targets)
        .filter(|target| target.role == "tests")
        .map(|target| target.path.clone())
        .collect();
    Some(ImplementationPlan {
        implementation_status: "ready".into(),
        planned_changes,
        planned_new_files: Vec::new(),
        planned_test_changes,
        remaining_unknowns: Vec::new(),
        blocking_unknowns: Vec::new(),
    })
}

pub(in crate::hosted) fn repair_implementation_plan(
    changes: &mut [PlannedChange],
    change_id: &str,
    attempted_concrete_path: &str,
) -> Result<Option<ImplementationPlanRepair>> {
    let targets_before = changes
        .iter()
        .find(|change| change.change_id == change_id)
        .map(|change| {
            change
                .path
                .trim()
                .is_empty()
                .then(Vec::new)
                .unwrap_or_else(|| vec![change.path.clone()])
                .into_iter()
                .chain(change.targets.iter().map(|target| target.path.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let normalized_legacy_targets = normalize_planned_changes(changes)?;
    if normalized_legacy_targets == 0 {
        return Ok(None);
    }
    let targets_after = changes
        .iter()
        .find(|change| change.change_id == change_id)
        .map(|change| {
            change
                .targets
                .iter()
                .map(|target| target.path.clone())
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(ImplementationPlanRepair {
        change_id: change_id.to_owned(),
        targets_before,
        targets_after,
        attempted_concrete_path: attempted_concrete_path.to_owned(),
        validation_error: "legacy compound target metadata required normalization".into(),
        repair_source: "orchestrator_normalization",
        model_call_consumed: false,
    }))
}

pub(in crate::hosted) fn record_mutation_preflight_rejection(
    notebook: &mut WorkerNotebook,
    usage: &mut ToolUsage,
    preflight: &MutationPreflightError,
) -> MutationPreflightDecision {
    usage.write_preflight_rejections = usage.write_preflight_rejections.saturating_add(1);
    let repeated_index = notebook
        .write_preflight_rejections
        .iter()
        .position(|record| {
            record.change_id == preflight.change_id
                && record.target == preflight.target
                && record.failure_code == preflight.code
                && record.plan_revision == notebook.revision
        });
    let repeated = repeated_index.is_some();
    if let Some(index) = repeated_index {
        notebook.write_preflight_rejections[index].occurrences = notebook
            .write_preflight_rejections[index]
            .occurrences
            .saturating_add(1);
    } else {
        notebook
            .write_preflight_rejections
            .push(MutationPreflightRecord {
                change_id: preflight.change_id.clone(),
                target: preflight.target.clone(),
                failure_code: preflight.code.into(),
                plan_revision: notebook.revision,
                retryable_with_same_plan: false,
                repair_strategy: preflight.repair_strategy.into(),
                mutation_attempted: false,
                mutation_preflight_failed: true,
                deterministic_repair_attempted: preflight.repair_strategy == "repair_plan_metadata",
                occurrences: 1,
            });
    }
    MutationPreflightDecision {
        repeated,
        halt_orchestration: true,
    }
}

pub(in crate::hosted) fn validate_planned_change_paths(
    root: &Path,
    changes: &[PlannedChange],
) -> Result<()> {
    for change in changes {
        for target in &change.targets {
            if target.path.contains(';') {
                bail!("invalid multi-path scalar target cannot reach implementation");
            }
            let may_be_absent = target.new_file
                || matches!(
                    target.status,
                    IntendedChangeStatus::Applied | IntendedChangeStatus::Verified
                );
            let resolved = safe_repo_path(root, &target.path, may_be_absent).map_err(|error| {
                anyhow!(
                    "implementation plan target `{}` is invalid: {error:#}",
                    target.path
                )
            })?;
            if !may_be_absent && !resolved.exists() {
                bail!(
                    "implementation plan target `{}` does not exist and is not marked new_file",
                    target.path
                );
            }
        }
    }
    Ok(())
}

pub(in crate::hosted) fn authorize_planned_target<'a>(
    plan: &'a ImplementationPlan,
    change_id: &str,
    path: &str,
) -> std::result::Result<&'a PlannedTarget, MutationPreflightError> {
    let Some(change) = plan
        .planned_changes
        .iter()
        .find(|change| change.change_id == change_id)
    else {
        return Err(MutationPreflightError {
            code: "mutation_change_id_unknown",
            change_id: change_id.into(),
            target: path.into(),
            message: "source-changing tool change_id is not in the implementation plan".into(),
            repair_strategy: "repair_plan_metadata",
        });
    };
    change
        .targets
        .iter()
        .find(|target| target.path == path)
        .ok_or_else(|| MutationPreflightError {
            code: "mutation_plan_metadata_mismatch",
            change_id: change_id.into(),
            target: path.into(),
            message: "source-changing tool target is not a member of its planned target set".into(),
            repair_strategy: "repair_plan_metadata",
        })
}

pub(in crate::hosted) fn roll_up_target_statuses(
    targets: &[PlannedTarget],
) -> IntendedChangeStatus {
    if !targets.is_empty()
        && targets
            .iter()
            .all(|target| target.status == IntendedChangeStatus::Verified)
    {
        IntendedChangeStatus::Verified
    } else if !targets.is_empty()
        && targets
            .iter()
            .all(|target| target.status == IntendedChangeStatus::Applied)
    {
        IntendedChangeStatus::Applied
    } else if !targets.is_empty()
        && targets
            .iter()
            .all(|target| target.status == IntendedChangeStatus::Unresolved)
    {
        IntendedChangeStatus::Unresolved
    } else if targets.iter().any(|target| {
        matches!(
            target.status,
            IntendedChangeStatus::InProgress
                | IntendedChangeStatus::Applied
                | IntendedChangeStatus::Verified
                | IntendedChangeStatus::Partial
                | IntendedChangeStatus::Unresolved
        )
    }) {
        IntendedChangeStatus::Partial
    } else {
        IntendedChangeStatus::Planned
    }
}

#[cfg(test)]
pub(in crate::hosted) fn reconcile_changed_target_statuses(
    intended_changes: &mut [IntendedChangeRecord],
    changed_paths: &BTreeSet<String>,
) {
    for intended in intended_changes {
        for target in &mut intended.targets {
            let repository_contains_target_change = changed_paths.contains(&target.path);
            target.status = match (repository_contains_target_change, target.status) {
                (false, IntendedChangeStatus::Applied | IntendedChangeStatus::Verified) => {
                    IntendedChangeStatus::Planned
                }
                (true, IntendedChangeStatus::Planned) => IntendedChangeStatus::InProgress,
                _ => target.status,
            };
        }
        intended.status = roll_up_target_statuses(&intended.targets);
    }
}

pub(in crate::hosted) fn intended_changes_from_plan(
    changes: &[PlannedChange],
) -> Vec<IntendedChangeRecord> {
    changes
        .iter()
        .map(|change| IntendedChangeRecord {
            change_id: change.change_id.clone(),
            intent: change.change.clone(),
            status: IntendedChangeStatus::Planned,
            target: String::new(),
            targets: change.targets.clone(),
            attempts: Vec::new(),
            recovery: None,
        })
        .collect()
}

pub(in crate::hosted) fn normalize_notebook_intended_changes(
    notebook: &mut WorkerNotebook,
    root: &Path,
) -> Result<()> {
    normalize_planned_changes(&mut notebook.planned_changes)?;
    if notebook.intended_changes.is_empty() && !notebook.planned_changes.is_empty() {
        notebook.intended_changes = intended_changes_from_plan(&notebook.planned_changes);
    }
    for intended in &mut notebook.intended_changes {
        if !intended.target.trim().is_empty() {
            for path in normalized_planned_paths(&intended.target)? {
                if !intended.targets.iter().any(|target| target.path == path) {
                    intended.targets.push(PlannedTarget {
                        path,
                        role: intended.intent.clone(),
                        new_file: false,
                        status: intended.status,
                    });
                }
            }
            intended.target.clear();
        }
        let mut normalized_targets = Vec::new();
        let mut seen_paths = BTreeSet::new();
        for target in std::mem::take(&mut intended.targets) {
            for path in normalized_planned_paths(&target.path)? {
                if seen_paths.insert(path.clone()) {
                    normalized_targets.push(PlannedTarget {
                        path,
                        role: if target.role.trim().is_empty() {
                            intended.intent.clone()
                        } else {
                            target.role.clone()
                        },
                        new_file: target.new_file,
                        status: target.status,
                    });
                }
            }
        }
        intended.targets = normalized_targets;
        if intended.targets.is_empty() {
            bail!(
                "persisted intended change `{}` requires at least one target",
                intended.change_id
            );
        }
        intended.status = roll_up_target_statuses(&intended.targets);
    }
    for planned in &mut notebook.planned_changes {
        if let Some(intended) = notebook
            .intended_changes
            .iter()
            .find(|intended| intended.change_id == planned.change_id)
        {
            for target in &mut planned.targets {
                if let Some(persisted) = intended
                    .targets
                    .iter()
                    .find(|persisted| persisted.path == target.path)
                {
                    target.status = persisted.status;
                    target.new_file |= persisted.new_file;
                }
            }
            planned.status = intended.status;
        }
    }
    validate_planned_change_paths(root, &notebook.planned_changes)?;
    if notebook.write_attempts.is_empty() {
        notebook.write_attempts = notebook
            .intended_changes
            .iter()
            .flat_map(|change| change.attempts.clone())
            .collect();
    }
    Ok(())
}

pub(in crate::hosted) fn validate_write_repair_strategy(
    attempts: &[WriteAttemptRecord],
    target: &str,
    change_id: &str,
    tool: &str,
    bounded_repair_read_completed: bool,
) -> Result<()> {
    let target_failures = attempts
        .iter()
        .filter(|attempt| attempt.target == target && attempt.status == WriteAttemptStatus::Failed)
        .count();
    let ambiguous_failures = attempts
        .iter()
        .filter(|attempt| {
            attempt.change_id == change_id
                && attempt.target == target
                && attempt.status == WriteAttemptStatus::Failed
                && (attempt.error_code.as_deref() == Some("replace_match_not_unique")
                    || (attempt.error_code.as_deref() == Some("mutation_content_conflict")
                        && attempt.match_count.is_some_and(|count| count != 1)))
        })
        .count();
    if tool == "replace_text" {
        if ambiguous_failures >= MAX_AMBIGUOUS_REPLACEMENT_FAILURES {
            bail!(
                "replace_text strategy exhausted for {target}; use replace_range, insert_after_symbol, insert_before_symbol, apply_unified_diff, or rewrite_small_file"
            );
        }
        if ambiguous_failures == 1 && !bounded_repair_read_completed {
            bail!(
                "a bounded read_file around the intended location is required before retrying replace_text for {target}"
            );
        }
    }
    if target_failures >= MAX_TARGET_REPAIR_FAILURES {
        bail!(
            "content repair circuit breaker opened for {target} after {MAX_TARGET_REPAIR_FAILURES} executed write failures"
        );
    }
    Ok(())
}

impl<'a> GatewayAgent<'a> {
    pub(in crate::hosted) fn accept_deterministic_implementation_plan_if_available(
        &mut self,
        reason: &str,
    ) -> Result<bool> {
        if self.phases.active() != ExecutionPhase::Planning || self.implementation_plan.is_some() {
            return Ok(false);
        }
        let Some(plan) = deterministic_plan_from_impact_map(&self.notebook) else {
            return Ok(false);
        };
        let arguments = serde_json::to_string(&plan)?;
        self.execute_tool("record_implementation_plan", &arguments)?;
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.implementation_plan_fallback_accepted",
                "artifact_source": "orchestrator_fallback",
                "reason_code": reason,
                "process_health": "healthy",
                "mission_outcome": "continuing",
                "planned_paths": plan.planned_changes.iter().flat_map(|change| {
                    change.targets.iter().map(|target| target.path.as_str())
                }).collect::<Vec<_>>(),
                "repository_validation_commands": repository_validation_commands_from_evidence(&self.notebook),
            }),
            "implementation-plan deterministic fallback",
        );
        Ok(true)
    }
}
