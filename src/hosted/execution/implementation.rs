// Extracted from the hosted execution composition root.
use super::*;

impl<'a> GatewayAgent<'a> {
    pub(in crate::hosted) fn reconcile_write_failures(
        &mut self,
        implementation: &ImplementationOutcome,
        validation: &[ValidationResult],
        changed_paths: &[String],
    ) -> Result<Vec<ToolFailureRecord>> {
        self.reconcile_repository_failure_supersession()?;
        let snapshot = self.build_execution_snapshot()?;
        self.tool_failures = self.notebook.failed_changes.clone();
        let changed = changed_paths
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let all_validation_passed =
            !validation.is_empty() && validation.iter().all(|result| result.status == "passed");
        let declaration = implementation.explicit_declaration.as_ref();
        let declaration_complete =
            declaration.is_some_and(|value| value.implementation_status == "complete");
        let path_completion_evidence = self
            .notebook
            .planned_changes
            .iter()
            .flat_map(|change| {
                change.targets.iter().map(|target| {
                    json!({
                        "path": target.path,
                        "planned": true,
                        "changed": changed.contains(target.path.as_str()),
                        "verified": changed.contains(target.path.as_str())
                            && all_validation_passed
                            && declaration_complete,
                        "blocking_criteria": change.acceptance_criteria,
                    })
                })
            })
            .collect::<Vec<_>>();

        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.intended_changes_reconciled",
                "authority": "execution_graph",
                "graph_revision": snapshot.graph.revision,
                "intended_changes": self.notebook.intended_changes,
                "failed_attempts": self.tool_failures,
                "final_changed_paths": changed_paths,
                "path_completion_evidence": path_completion_evidence,
                "validation": validation,
            }),
            "intended-change reconciliation",
        );
        Ok(self
            .tool_failures
            .iter()
            .filter(|failure| failure.reconciliation == FailureReconciliation::StillUnresolved)
            .cloned()
            .collect())
    }

    pub(in crate::hosted) fn preflight_source_mutation(
        &mut self,
        name: &str,
        object: &serde_json::Map<String, Value>,
    ) -> Result<()> {
        if self.impact_map.is_none() {
            return Err(MutationPreflightError {
                code: "mutation_policy_denied",
                change_id: String::new(),
                target: String::new(),
                message: "record_impact_map is required before source-changing tools".into(),
                repair_strategy: "complete_required_artifact",
            }
            .into());
        }
        if self.implementation_plan.is_none() {
            return Err(MutationPreflightError {
                code: "mutation_policy_denied",
                change_id: String::new(),
                target: String::new(),
                message: "record_implementation_plan is required before source-changing tools"
                    .into(),
                repair_strategy: "complete_required_artifact",
            }
            .into());
        }
        let raw_path = required_tool_string(object, "path", 4_096)?;
        let normalized_paths =
            normalized_planned_paths(raw_path).map_err(|error| MutationPreflightError {
                code: "mutation_target_path_invalid",
                change_id: object
                    .get("change_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                target: raw_path.to_owned(),
                message: error.to_string(),
                repair_strategy: "repair_plan_metadata",
            })?;
        if normalized_paths.len() != 1 {
            return Err(MutationPreflightError {
                code: "mutation_target_path_invalid",
                change_id: object
                    .get("change_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                target: raw_path.to_owned(),
                message: "source-changing tool target must be one concrete repository path".into(),
                repair_strategy: "repair_plan_metadata",
            }
            .into());
        }
        let path = normalized_paths[0].as_str();

        let snapshot = self.build_execution_snapshot()?;
        let current_node_id = self
            .current_decision
            .as_ref()
            .and_then(ExecutionDecision::node_id)
            .cloned();
        let active_validation_repair = matches!(
            self.current_decision.as_ref(),
            Some(ExecutionDecision::ExecuteTarget {
                action: crate::hosted_orchestrator::MutationAction::RepairTarget { failure, .. },
                ..
            }) if failure.category == crate::execution_graph::FailureCategory::ValidationFailure
        );
        if active_validation_repair {
            let (repair, failure) = match self.current_decision.as_ref() {
                Some(ExecutionDecision::ExecuteTarget {
                    action: crate::hosted_orchestrator::MutationAction::RepairTarget { failure, .. },
                    target,
                    ..
                }) => (
                    target
                        .validation_repair
                        .as_ref()
                        .context("validation repair mutation lacks a correction contract")?,
                    failure,
                ),
                _ => unreachable!("active validation repair was matched above"),
            };
            if repair.correction_contracts.is_empty()
                || !repair.correction_contracts.iter().any(|contract| {
                    contract
                        .implicated_paths
                        .iter()
                        .any(|implicated| implicated == path)
                })
            {
                bail!(
                    "wrong_repair_target: `{path}` is not implicated by the active assertion correction contract"
                );
            }
            let test_only_target = failure
                .assertion_failures
                .iter()
                .any(|assertion| assertion.test_file == path);
            if test_only_target
                && !matches!(
                    repair.repair_intent.diagnosis,
                    crate::execution_graph::ValidationRepairDiagnosis::TestExpectationDefect
                        | crate::execution_graph::ValidationRepairDiagnosis::Both
                )
            {
                bail!(
                    "test_repair_requires_specification_evidence: `{path}` is not eligible under the active repair diagnosis"
                );
            }
        }
        if let Some(already_applied) = classify_hosted_mutation_preflight(
            &snapshot,
            current_node_id.as_ref(),
            path,
            active_validation_repair,
        )? {
            // Reconciliation is authoritative for selecting the next node. The
            // duplicate itself records no failure and consumes no repair work.
            let _ = self.record_active_target_applied(path)?;
            self.reconcile_execution_and_apply()?;
            return Err(already_applied.into());
        }

        let current_target = self.current_implementation_target();
        validate_current_target_scope(
            current_target.as_ref(),
            self.guided_first_write_recovery_issued,
            self.tool_usage.successful_writes,
            &[path],
            true,
        )?;
        let change_id = required_tool_string(object, "change_id", 100)?.to_owned();

        let repaired = {
            let plan = self
                .implementation_plan
                .as_mut()
                .expect("the implementation plan was checked above");
            repair_implementation_plan(&mut plan.planned_changes, &change_id, path)?
                .map(|repair| (repair, plan.clone()))
        };
        if let Some((repair, repaired_plan)) = repaired {
            validate_planned_change_paths(&self.repo.root, &repaired_plan.planned_changes)?;
            let repository_fingerprint =
                repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
            self.notebook.orchestration.reconcile_plan_topology(
                self.manifest,
                &repaired_plan,
                &repository_fingerprint,
            );
            let replacement_graph = self
                .notebook
                .orchestration
                .graph
                .clone()
                .context("plan topology repair did not produce an execution graph")?;
            let preserved_node_ids = self
                .notebook
                .orchestration
                .pending_topology_preserved_node_ids
                .clone();
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::PlanRepaired {
                    sequence: self.next_domain_event_sequence(),
                    repaired_criterion_ids: Vec::new(),
                },
            )?;
            self.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::GraphCreated {
                    sequence: self.next_domain_event_sequence(),
                    graph_id: replacement_graph.graph_id.clone(),
                    revision: replacement_graph.revision,
                    graph: Some(replacement_graph),
                    preserved_node_ids,
                },
            )?;
            self.persist_orchestration_checkpoint("implementation_plan_topology_repaired", false)?;
            self.api.append_event(
                "progress",
                json!({
                    "event_type": "worker.implementation_plan_repaired",
                    "change_id": repair.change_id,
                    "targets_before": repair.targets_before,
                    "targets_after": repair.targets_after,
                    "attempted_concrete_path": repair.attempted_concrete_path,
                    "validation_error": repair.validation_error,
                    "repair_source": repair.repair_source,
                    "model_call_consumed": repair.model_call_consumed,
                }),
            )?;
        }
        let plan = self
            .implementation_plan
            .as_ref()
            .expect("the implementation plan was checked above");
        let target = authorize_planned_target(plan, &change_id, path)?;
        let operation = target.effective_operation();
        let compatible = match operation {
            crate::execution_graph::TargetOperation::ModifyExisting => {
                matches!(name, "apply_patch" | "apply_unified_diff" | "replace_file")
            }
            crate::execution_graph::TargetOperation::CreateNew => name == "create_file",
            crate::execution_graph::TargetOperation::DeleteExisting => name == "delete_file",
            crate::execution_graph::TargetOperation::Rename { .. } => {
                matches!(name, "rename_file" | "move_file")
            }
            crate::execution_graph::TargetOperation::Move { .. } => name == "move_file",
        };
        if !compatible {
            return Err(MutationPreflightError {
                code: "mutation_tool_operation_mismatch",
                change_id,
                target: path.to_owned(),
                message: format!(
                    "tool `{name}` is incompatible with operation `{}`",
                    operation.as_str()
                ),
                repair_strategy: "use_operation_bound_tool",
            }
            .into());
        }
        safe_repo_path(
            &self.repo.root,
            path,
            !matches!(
                operation,
                crate::execution_graph::TargetOperation::ModifyExisting
                    | crate::execution_graph::TargetOperation::DeleteExisting
            ),
        )
        .map_err(|error| MutationPreflightError {
            code: if error.kind == RepoPathErrorKind::NotAllowed {
                "mutation_target_outside_repository"
            } else {
                "mutation_target_path_invalid"
            },
            change_id: change_id.clone(),
            target: path.to_owned(),
            message: error.to_string(),
            repair_strategy: "repair_plan_metadata",
        })?;
        validate_write_repair_strategy(
            &self.notebook.write_attempts,
            path,
            &change_id,
            name,
            self.repair_read_targets.contains(path),
        )
        .map_err(|error| MutationPreflightError {
            code: "mutation_content_conflict",
            change_id: change_id.clone(),
            target: path.to_owned(),
            message: error.to_string(),
            repair_strategy: "return_partial_result",
        })?;
        Ok(())
    }
}
