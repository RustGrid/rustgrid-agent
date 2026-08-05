#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExecutionSnapshot {
    pub run_id: String,
    pub current_repository: RepositorySnapshot,
    pub graph: ExecutionGraph,
    #[serde(default)]
    pub events: Vec<ExecutionDomainEvent>,
    pub evidence: EvidenceStore,
    pub failures: FailureStore,
    pub budget: BudgetState,
    pub cancellation: Option<CancellationState>,
    pub publication: PublicationState,
}

impl ExecutionSnapshot {
    pub fn stage(&self) -> HostedExecutionStage {
        self.graph.stage()
    }

    pub fn next_event_sequence(&self) -> u64 {
        self.events
            .last()
            .map_or(1, |event| event.sequence().saturating_add(1))
    }

    pub fn terminal_outcome(&self) -> Option<MissionOutcome> {
        current_epoch_terminal_outcome(&self.events)
    }

    pub fn is_terminal(&self) -> bool {
        self.terminal_outcome().is_some()
    }

    pub fn remaining_required_nodes(&self) -> Vec<&ExecutionNode> {
        self.graph.remaining_required_nodes()
    }

    /// Returns the canonical dependency view used by both event application and
    /// reconciliation. Actual success remains distinct from an explicit
    /// partial-review override, so remaining work is never erased.
    pub fn dependency_satisfaction_ids(&self) -> BTreeSet<ExecutionNodeId> {
        let mut satisfied = self
            .graph
            .nodes
            .iter()
            .filter(|node| node.status.satisfies_dependency())
            .map(|node| node.id.clone())
            .chain(self.graph.dependency_satisfaction_overrides.iter().cloned())
            .chain(
                self.graph
                    .dependency_overrides
                    .iter()
                    .filter(|override_| {
                        override_.allowed_outcome == MissionOutcome::PartialReviewable
                    })
                    .map(|override_| override_.unsatisfied_dependency.clone()),
            )
            .collect::<BTreeSet<_>>();

        if self.graph.recovery_publication_dependency_override
            && let Some(publication) = self
                .graph
                .nodes
                .iter()
                .find(|node| node.kind == ExecutionNodeKind::Publication)
        {
            satisfied.extend(publication.dependencies.iter().cloned());
        }

        // A validation failure can reopen an already-applied mutation without
        // invalidating later applied mutation nodes. Preserve only dependency
        // lineage while the explicit validation repair remains unresolved;
        // the target itself still appears in remaining work and is selected
        // by the orchestrator for repair.
        satisfied.extend(
            self.failures
                .unresolved()
                .filter(|failure| failure.category == FailureCategory::ValidationFailure)
                .filter_map(|failure| failure.target_path.as_deref())
                .filter_map(|path| self.graph.unique_mutation_node_for_target_path(path))
                .map(|node| node.id.clone()),
        );

        for node in self
            .graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_validation())
        {
            let dependencies_satisfied = node
                .dependencies
                .iter()
                .all(|dependency| satisfied.contains(dependency));
            if dependencies_satisfied
                && !self.failures.has_unresolved_for_node(&node.id)
                && node.validation.as_ref().is_some_and(|gate| {
                    self.evidence.has_passed_validation(
                        &gate.fingerprint(self.current_repository.validation_source_tree_hash()),
                    )
                })
            {
                satisfied.insert(node.id.clone());
            }
        }

        // Checkpoint compatibility: an older serialized graph may contain the
        // event but predate the explicit override field.
        if self.has_partial_reviewable_guardrail() {
            satisfied.extend(
                self.graph
                    .nodes
                    .iter()
                    .filter(|node| node.kind.is_mutation())
                    .map(|node| node.id.clone()),
            );
        }
        satisfied
    }

    pub fn has_partial_reviewable_guardrail(&self) -> bool {
        current_execution_epoch(&self.events)
            .iter()
            .rev()
            .any(|event| {
                matches!(
                    event,
                    ExecutionDomainEvent::GuardrailTriggered {
                        outcome: MissionOutcome::PartialReviewable,
                        ..
                    }
                )
            })
    }

    pub fn has_incomplete_diff_review_request(&self) -> bool {
        current_execution_epoch(&self.events).iter().any(|event| {
            matches!(event, ExecutionDomainEvent::IncompleteDiffReviewRequested { .. })
        })
    }

    pub fn incomplete_diff_dependency_overrides(
        &self,
        diff_review_node: &ExecutionNodeId,
        reason: IncompleteReason,
    ) -> Vec<DependencyOverride> {
        let reason = match reason {
            IncompleteReason::ValidationRerunPending => {
                "draft publication with an applied repair whose validation rerun is still pending"
            }
            IncompleteReason::ValidationRepairProducedNoMutation
            | IncompleteReason::ValidationRepairProducedNoMeaningfulMutation => {
                "draft publication after failed code validation and no valid repair mutation"
            }
            IncompleteReason::ValidationInfrastructureFailure => {
                "draft publication after validation infrastructure failure"
            }
            IncompleteReason::TargetOperationConflict => {
                "draft publication after a late repository target operation conflict"
            }
        };
        self.graph
            .nodes
            .iter()
            .filter(|node| {
                node.required
                    && node.kind.is_validation()
                    && !node.status.satisfies_dependency()
            })
            .map(|node| DependencyOverride {
                dependent_node: diff_review_node.clone(),
                unsatisfied_dependency: node.id.clone(),
                reason: reason.to_owned(),
                allowed_outcome: MissionOutcome::PartialReviewable,
            })
            .collect()
    }

    pub fn target_state(&self, node_id: &ExecutionNodeId) -> Option<TargetState> {
        let node = self.graph.node(node_id)?;
        let target = node.target.as_ref()?;
        let mutation_status = match node.status {
            ExecutionNodeStatus::Applied
            | ExecutionNodeStatus::Passed
            | ExecutionNodeStatus::Completed
            | ExecutionNodeStatus::Superseded => MutationStatus::Applied,
            ExecutionNodeStatus::Running => MutationStatus::Running,
            ExecutionNodeStatus::FailedRecoverable => MutationStatus::FailedRecoverable,
            ExecutionNodeStatus::FailedBlocking => MutationStatus::FailedBlocking,
            ExecutionNodeStatus::Pending
            | ExecutionNodeStatus::Ready
            | ExecutionNodeStatus::Skipped => MutationStatus::Planned,
        };
        let target_failure = self.failures.unresolved().find(|failure| {
            failure.category == FailureCategory::ValidationFailure
                && (failure.target_path.as_deref() == Some(target.path.as_str())
                    || failure.assertion_failures.iter().any(|assertion| {
                        assertion.test_file == target.path
                            || assertion.implicated_paths.contains(&target.path)
                    }))
        });
        let validation_status = if target_failure.is_some() {
            ValidationStatus::FailedCode
        } else if self
            .failures
            .unresolved()
            .any(|failure| failure.category == FailureCategory::InfrastructureFailure)
        {
            ValidationStatus::FailedInfrastructure
        } else if self
            .graph
            .nodes
            .iter()
            .filter(|candidate| candidate.required && candidate.kind.is_validation())
            .all(|candidate| candidate.status == ExecutionNodeStatus::Passed)
        {
            ValidationStatus::Passed
        } else {
            ValidationStatus::Pending
        };
        Some(TargetState {
            mutation_status,
            validation_status,
        })
    }

    /// Returns implementation, repair, and validation state independently.
    /// A completed implementation therefore remains `Applied` while a later
    /// validation failure and its repair are still unresolved.
    pub fn target_execution_state(
        &self,
        node_id: &ExecutionNodeId,
    ) -> Option<TargetExecutionState> {
        let state = self.target_state(node_id)?;
        let target = self.graph.node(node_id)?.target.as_ref()?;
        let validation_failure = self.failures.unresolved().find(|failure| {
            failure.category == FailureCategory::ValidationFailure
                && (failure.target_path.as_deref() == Some(target.path.as_str())
                    || failure.assertion_failures.iter().any(|assertion| {
                        assertion.test_file == target.path
                            || assertion.implicated_paths.contains(&target.path)
                    }))
        });
        let repair_status = validation_failure.map_or(RepairStatus::NotRequired, |failure| {
            let latest = current_execution_epoch(&self.events)
                .iter()
                .rev()
                .find_map(|event| match event {
                    ExecutionDomainEvent::ValidationRepairCompleted {
                        failure_id, result, ..
                    } if failure_id == &failure.id => Some(result),
                    _ => None,
                });
            match latest {
                Some(RepairResult::MutationProduced { .. }) => RepairStatus::CandidateApplied,
                Some(RepairResult::AlreadySatisfiesRepairIntent { .. }) => {
                    RepairStatus::AlreadySatisfied
                }
                Some(RepairResult::NoMutation { .. }) => {
                    let exhausted = self
                        .budget
                        .repair_session_for_failure(&failure.id)
                        .is_some_and(|session| {
                            let owner = ExecutionNodeId::new(session.session_id.clone());
                            session.attempted_targets.len()
                                >= usize::try_from(session.budget.max_target_attempts)
                                    .unwrap_or(usize::MAX)
                                || self.budget.usage_for(&owner).validation_repair_attempts
                                    >= session.budget.max_target_attempts
                        });
                    if exhausted {
                        RepairStatus::Exhausted
                    } else {
                        RepairStatus::Unresolved
                    }
                }
                None => RepairStatus::Pending,
            }
        });
        Some(TargetExecutionState {
            implementation_status: state.mutation_status,
            repair_status,
            validation_status: state.validation_status,
        })
    }

    /// Returns the deterministic set of current validation proof required to
    /// authorize recovery publication. Every required gate must be represented
    /// by attached, passed evidence for the current repository fingerprint.
    pub fn current_required_validation_evidence_ids(
        &self,
    ) -> Result<Vec<String>, GraphInvariantError> {
        let mut evidence_ids = BTreeSet::new();
        let satisfied = self.dependency_satisfaction_ids();
        for node in self
            .graph
            .nodes
            .iter()
            .filter(|node| node.required && node.kind.is_validation())
        {
            if self.failures.has_unresolved_for_node(&node.id) {
                return Err(GraphInvariantError::new(format!(
                    "required validation node `{}` has an unresolved failure",
                    node.id
                )));
            }
            if !node
                .dependencies
                .iter()
                .all(|dependency| satisfied.contains(dependency))
            {
                return Err(GraphInvariantError::new(format!(
                    "required validation node `{}` has unsatisfied dependencies",
                    node.id
                )));
            }
            let gate = node.validation.as_ref().ok_or_else(|| {
                GraphInvariantError::new(format!(
                    "required validation node `{}` has no gate specification",
                    node.id
                ))
            })?;
            let source_tree_hash = self.current_repository.validation_source_tree_hash();
            let expected_fingerprint = gate.fingerprint(source_tree_hash);
            let matching = self
                .evidence
                .validations
                .iter()
                .filter(|(_, evidence)| {
                    evidence.node_id == node.id
                        && evidence.status == ValidationEvidenceStatus::Passed
                        && evidence.repository_fingerprint == source_tree_hash
                        && evidence.fingerprint == expected_fingerprint
                })
                .map(|(evidence_id, _)| evidence_id.clone())
                .collect::<Vec<_>>();
            if matching.is_empty() {
                return Err(GraphInvariantError::new(format!(
                    "required validation node `{}` has no current passed evidence",
                    node.id
                )));
            }
            evidence_ids.extend(matching);
        }
        Ok(evidence_ids.into_iter().collect())
    }

    /// Returns either complete validation proof or, for the explicit
    /// partial-reviewable infrastructure route, the current observations that
    /// explain why validation is incomplete. Unstarted gates intentionally
    /// contribute no fabricated evidence.
    pub fn recovery_publication_validation_evidence_ids(
        &self,
    ) -> Result<Vec<String>, GraphInvariantError> {
        let validation_partial = (self.has_partial_reviewable_guardrail()
            || self.has_incomplete_diff_review_request())
            && self.failures.unresolved().next().is_some()
            && self
                .failures
                .unresolved()
                .all(|failure| {
                    matches!(
                        failure.category,
                        FailureCategory::ValidationFailure
                            | FailureCategory::InfrastructureFailure
                    )
                })
            && self
                .graph
                .nodes
                .iter()
                .filter(|node| node.required && node.kind.is_mutation())
                .all(|node| node.status.satisfies_dependency());
        if !validation_partial {
            return self.current_required_validation_evidence_ids();
        }

        let graph_gate_ids = self
            .graph
            .nodes
            .iter()
            .filter(|node| node.required && node.kind.is_validation())
            .filter_map(|node| node.validation.as_ref().map(|gate| gate.gate_id.clone()))
            .collect::<BTreeSet<_>>();
        let evidence_ids = self
            .evidence
            .validations
            .iter()
            .filter(|(_, evidence)| {
                evidence.repository_fingerprint
                    == self.current_repository.validation_source_tree_hash()
                    && graph_gate_ids.contains(&evidence.gate_id)
                    && matches!(
                        evidence.status,
                        ValidationEvidenceStatus::Passed
                            | ValidationEvidenceStatus::Failed
                            | ValidationEvidenceStatus::TimedOut
                            | ValidationEvidenceStatus::Cancelled
                    )
            })
            .map(|(evidence_id, _)| evidence_id.clone())
            .collect::<Vec<_>>();
        if evidence_ids.is_empty() {
            let stale_after_repair = self
                .budget
                .validation_repair_sessions
                .values()
                .filter(|session| {
                    matches!(
                        session.status,
                        ValidationRepairSessionStatus::ReadyForRerun
                            | ValidationRepairSessionStatus::Stopped
                    ) && session.attempted_targets.iter().any(|attempt| {
                        attempt.outcome == ValidationRepairMutationOutcome::MutationApplied
                            && attempt.repository_fingerprint_after.as_str()
                                == self.current_repository.fingerprint
                            && attempt.repository_fingerprint_before
                                != attempt.repository_fingerprint_after
                    })
                })
                .flat_map(|session| {
                    self.evidence.validations.iter().filter_map(|(evidence_id, evidence)| {
                        (evidence.node_id == session.originating_gate_id
                            && graph_gate_ids.contains(&evidence.gate_id)
                            && (evidence.status == ValidationEvidenceStatus::Failed
                                || self.events.iter().any(|event| {
                                    matches!(
                                        event,
                                        ExecutionDomainEvent::ValidationEvidenceRecorded {
                                            evidence: recorded,
                                            ..
                                        } if recorded.evidence_id == *evidence_id
                                            && recorded.status == ValidationEvidenceStatus::Failed
                                    )
                                }))
                            && evidence.repository_fingerprint
                                != self.current_repository.fingerprint)
                            .then_some(evidence_id.clone())
                    })
                })
                .collect::<BTreeSet<_>>();
            if !stale_after_repair.is_empty() {
                return Ok(stale_after_repair.into_iter().collect());
            }
            return Err(GraphInvariantError::new(
                "partial recovery publication requires a current validation observation or an explicit stale-after-repair observation with a pending rerun",
            ));
        }
        Ok(evidence_ids)
    }

    /// Returns the canonical complete set of validation proof invalidated when
    /// finalization is rebound to a new repository observation.
    pub fn finalization_validation_evidence_ids(&self) -> Vec<String> {
        self.graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_validation())
            .flat_map(|node| node.evidence_ids.iter().cloned())
            .chain(
                self.evidence
                    .validations
                    .iter()
                    .filter(|(_, evidence)| evidence.status == ValidationEvidenceStatus::Passed)
                    .map(|(evidence_id, _)| evidence_id.clone()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn target_execution_context(
        &self,
        node_id: &ExecutionNodeId,
        allowed_tools: Vec<ToolKind>,
    ) -> Result<TargetExecutionContext, GraphInvariantError> {
        let node = self.graph.node(node_id).ok_or_else(|| {
            GraphInvariantError::new(format!("unknown execution node `{node_id}`"))
        })?;
        let target = node.target.clone().ok_or_else(|| {
            GraphInvariantError::new(format!("node `{node_id}` is not a mutation target"))
        })?;
        let dependency_evidence = node
            .dependencies
            .iter()
            .filter_map(|dependency_id| self.graph.node(dependency_id))
            .flat_map(|dependency| dependency.evidence_ids.iter())
            .filter_map(|evidence_id| self.evidence.summary(evidence_id))
            .collect::<Vec<_>>();
        let reusable_file =
            self.evidence
                .reusable_file(&target.path, &self.current_repository.fingerprint, None);
        let current_file_content = reusable_file.map(|evidence| evidence.captured_content.clone());
        let target_content_hash = reusable_file.map(|evidence| evidence.content_hash.clone());
        let operation = target.effective_operation();
        let expected_result_content_hash = self.events.iter().rev().find_map(|event| match event {
            ExecutionDomainEvent::TargetMutationIntentRecorded {
                node_id: recorded_node_id,
                target_path,
                operation: recorded_operation,
                expected_result_content_hash,
                accepted_intent_hash,
                ..
            } if recorded_node_id == node_id
                && target_path == &target.path
                && recorded_operation == &operation
                && accepted_intent_hash == &hex::encode(Sha256::digest(target.intent.as_bytes())) =>
            {
                expected_result_content_hash.clone()
            }
            _ => None,
        });
        let prepared_probe = self.events.iter().rev().find_map(|event| match event {
            ExecutionDomainEvent::TargetContextPrepared {
                node_id: prepared_node_id,
                target_path,
                operation: prepared_operation,
                source_path,
                target_exists,
                source_exists,
                repository_fingerprint,
                target_content_hash,
                source_content_hash,
                ..
            } if prepared_node_id == node_id
                && target_path == &target.path
                && prepared_operation == &operation
                && repository_fingerprint.as_str() == self.current_repository.fingerprint =>
            {
                Some(TargetStateProbe {
                    operation: prepared_operation.clone(),
                    target_path: target_path.clone(),
                    target_exists: target_exists.unwrap_or(target_content_hash.is_some()),
                    source_exists: *source_exists,
                    target_content_hash: target_content_hash.clone(),
                    source_content_hash: source_content_hash.clone(),
                    expected_result_content_hash: expected_result_content_hash.clone(),
                    repository_fingerprint: repository_fingerprint.clone(),
                })
            }
            _ => None,
        });
        let source_file = operation.source_path().and_then(|path| {
            self.evidence
                .reusable_file(path, &self.current_repository.fingerprint, None)
        });
        let accepted_intent_hash = hex::encode(Sha256::digest(target.intent.as_bytes()));
        let nearby_context = reusable_file
            .filter(|evidence| evidence.line_range.is_some())
            .map(FileExcerpt::from)
            .into_iter()
            .collect();
        Ok(TargetExecutionContext {
            node_id: node.id.clone(),
            change_id: target.change_id.clone(),
            intent: target.intent.clone(),
            acceptance_criteria_ids: target.acceptance_criteria_ids.clone(),
            target: target.clone(),
            dependency_evidence: dependency_evidence.clone(),
            current_file_content,
            target_content_hash,
            target_state_probe: prepared_probe.clone(),
            inspection_outcome: prepared_probe.as_ref().map(TargetStateProbe::inspection_outcome),
            source_file_content: source_file.map(|evidence| evidence.captured_content.clone()),
            source_content_hash: source_file.map(|evidence| evidence.content_hash.clone()),
            create_specification: matches!(operation, TargetOperation::CreateNew).then(|| {
                CreateTargetSpecification {
                    path: target.path.clone(),
                    role: target.role.clone(),
                    intent: target.intent.clone(),
                    acceptance_criteria_ids: target.acceptance_criteria_ids.clone(),
                    related_evidence_ids: dependency_evidence
                        .iter()
                        .map(|evidence| EvidenceId::new(evidence.evidence_id.clone()))
                        .collect(),
                    expected_artifact_kind: (!target.role.trim().is_empty())
                        .then(|| target.role.clone()),
                }
            }),
            repository_fingerprint: self.current_repository.fingerprint.clone(),
            accepted_intent_hash,
            nearby_context,
            validation_repair: None,
            allowed_tools,
            remaining_node_budget: self.budget.remaining_for(&node.id, &node.budget),
        })
    }

    /// Appends one authoritative event and updates the graph-backed materialized
    /// state. Events after `RunFinished` are rejected so infrastructure updates
    /// cannot replace a terminal domain result.
    pub fn append_event(&mut self, event: ExecutionDomainEvent) -> Result<(), GraphInvariantError> {
        let mut next = self.clone();
        next.append_event_in_place(event)?;
        *self = next;
        Ok(())
    }

    fn append_event_in_place(
        &mut self,
        mut event: ExecutionDomainEvent,
    ) -> Result<(), GraphInvariantError> {
        let terminal_outcome = self.terminal_outcome();
        let resumes_partial_terminal = matches!(
            &event,
            ExecutionDomainEvent::ExecutionResumed {
                previous_outcome: Some(MissionOutcome::PartialReviewable),
                ..
            }
        ) && terminal_outcome
            == Some(MissionOutcome::PartialReviewable);
        if terminal_outcome.is_some() && !resumes_partial_terminal {
            return Err(GraphInvariantError::new(
                "domain events cannot be appended after RunFinished",
            ));
        }
        if let Some(previous) = self.events.last()
            && event.sequence() <= previous.sequence()
        {
            return Err(GraphInvariantError::new(format!(
                "event sequence {} does not follow {}",
                event.sequence(),
                previous.sequence()
            )));
        }
        if let ExecutionDomainEvent::ValidationRepairCompleted {
            failure_id,
            attempt: Some(attempt),
            ..
        } = &mut event
            && let Some(session) = self.budget.repair_session_for_failure(failure_id)
        {
            if attempt.attempt_number == 0 {
                attempt.attempt_number = u32::try_from(session.attempted_targets.len())
                    .unwrap_or(u32::MAX)
                    .saturating_add(1);
            }
            if attempt.failure_revision == 0 {
                attempt.failure_revision = session.current_assertion_set_revision;
            }
        }

        self.graph
            .validate_invariants_with_dependency_satisfaction(
                &self.dependency_satisfaction_ids(),
            )?;
        self.validate_event_semantics(&event)?;
        let repair_started = event
            .node_id()
            .is_some_and(|node_id| self.node_start_is_target_repair(&event, node_id));
        if repair_started {
            let node_id = event
                .node_id()
                .expect("a target repair start always refers to a node");
            let node = self.graph.node(node_id).ok_or_else(|| {
                GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
            })?;
            if self.budget.usage_for(node_id).repair_attempts >= node.budget.max_repair_attempts {
                return Err(GraphInvariantError::new(format!(
                    "node `{node_id}` cannot start repair beyond its {}-attempt budget",
                    node.budget.max_repair_attempts
                )));
            }
        }
        match &event {
            ExecutionDomainEvent::MutationRepairAllowanceRestored { node_id, .. } => {
                if self.budget.usage_for(node_id).repair_attempts == 0 {
                    return Err(GraphInvariantError::new(format!(
                        "node `{node_id}` cannot restore an unconsumed mutation repair allowance"
                    )));
                }
            }
            ExecutionDomainEvent::MutationRepairAllowanceConsumed { node_id, .. } => {
                let node = self.graph.node(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
                })?;
                if self.budget.usage_for(node_id).repair_attempts >= node.budget.max_repair_attempts
                {
                    return Err(GraphInvariantError::new(format!(
                        "node `{node_id}` cannot consume mutation repair allowance beyond its {}-attempt budget",
                        node.budget.max_repair_attempts
                    )));
                }
            }
            _ => {}
        }
        if let ExecutionDomainEvent::ValidationRepairStarted {
            validation_node_id,
            failure_id,
            implicated_paths,
            correction_contracts,
            ..
        } = &event
        {
            let validation_node = self.graph.node(validation_node_id).ok_or_else(|| {
                GraphInvariantError::new(format!(
                    "validation repair refers to unknown node `{validation_node_id}`"
                ))
            })?;
            let failure = self.failures.get(failure_id).cloned().ok_or_else(|| {
                GraphInvariantError::new(format!(
                    "validation repair refers to unknown failure `{failure_id}`"
                ))
            })?;
            let implicated_targets = implicated_paths
                .iter()
                .chain(
                    correction_contracts
                        .iter()
                        .flat_map(|contract| contract.implicated_paths.iter()),
                )
                .cloned()
                .collect::<BTreeSet<_>>();
            let implicated_target_bytes = self
                .evidence
                .files
                .values()
                .filter(|evidence| {
                    implicated_targets.contains(&evidence.path)
                        && evidence.repository_fingerprint
                            == self.current_repository.fingerprint
                })
                .map(|evidence| u64::try_from(evidence.captured_content.len()).unwrap_or(u64::MAX))
                .fold(0_u64, u64::saturating_add);
            let budget_inputs = ValidationRepairBudgetInputs {
                failed_assertion_count: u32::try_from(failure.assertion_failures.len())
                    .unwrap_or(u32::MAX)
                    .max(1),
                implicated_target_count: u32::try_from(implicated_targets.len())
                    .unwrap_or(u32::MAX)
                    .max(1),
                originating_gate_required: validation_node.required,
                implicated_target_bytes,
            };
            let session = self
                .budget
                .ensure_validation_repair_session(&failure, budget_inputs)?
                .clone();
            let session_owner = ExecutionNodeId::new(session.session_id.clone());
            if session.attempted_targets.len()
                >= usize::try_from(session.budget.max_target_attempts).unwrap_or(usize::MAX)
                || self
                    .budget
                    .usage_for(&session_owner)
                    .validation_repair_attempts
                    >= session.budget.max_target_attempts
            {
                return Err(GraphInvariantError::new(format!(
                    "validation repair session `{}` cannot start another bounded target attempt",
                    session.session_id
                )));
            }
            self.budget
                .record_validation_repair_context_rebuild(failure_id)?;
        }
        if let ExecutionDomainEvent::ValidationRepairCompleted {
            failure_id,
            result: RepairResult::AlreadySatisfiesRepairIntent { evidence },
            ..
        } = &event
        {
            let repair_intent = current_execution_epoch(&self.events)
                .iter()
                .rev()
                .find_map(|prior| match prior {
                    ExecutionDomainEvent::ValidationRepairStarted {
                        failure_id: started_failure,
                        repair_intent,
                        ..
                    } if started_failure == failure_id => Some(repair_intent),
                    _ => None,
                })
                .ok_or_else(|| {
                    GraphInvariantError::new(
                        "already-satisfied validation repair lacks an active repair intent",
                    )
                })?;
            if !evidence.proves(repair_intent) {
                return Err(GraphInvariantError::new(
                    "already-satisfied validation repair evidence does not prove the active assertion contract",
                ));
            }
        }

        if let ExecutionDomainEvent::ValidationRepairStarted { failure_id, .. } = &event {
            let session = self
                .budget
                .repair_session_for_failure(failure_id)
                .cloned()
                .ok_or_else(|| {
                    GraphInvariantError::new(
                        "validation repair start did not materialize its repair session",
                    )
                })?;
            let session_node_id = ExecutionNodeId::new(session.session_id.clone());
            if let Some(node) = self.graph.node_mut(&session_node_id) {
                node.status = ExecutionNodeStatus::Running;
                node.budget = session.budget.as_node_budget();
            } else {
                self.graph.nodes.push(ExecutionNode {
                    id: session_node_id,
                    kind: ExecutionNodeKind::ValidationRepairSession,
                    // Ownership is represented by the session's typed
                    // originating gate. A failed gate cannot satisfy a graph
                    // edge, so making it a dependency would deadlock repair.
                    dependencies: Vec::new(),
                    status: ExecutionNodeStatus::Running,
                    required: false,
                    budget: session.budget.as_node_budget(),
                    ..ExecutionNode::default()
                });
            }
            self.graph.revision = self.graph.revision.saturating_add(1);
        }

        let dependency_satisfaction = self.dependency_satisfaction_ids();
        self.graph
            .apply_domain_event_with_dependency_satisfaction(&event, &dependency_satisfaction)?;
        if let ExecutionDomainEvent::ValidationRepairCompleted { failure_id, .. } = &event
            && let Some(session) = self.budget.repair_session_for_failure(failure_id)
        {
            let session_node_id = ExecutionNodeId::new(session.session_id.clone());
            if let Some(node) = self.graph.node_mut(&session_node_id) {
                node.status = ExecutionNodeStatus::Skipped;
                self.graph.revision = self.graph.revision.saturating_add(1);
            }
        }
        if repair_started {
            self.budget.record_repair_attempt(
                event
                    .node_id()
                    .expect("a target repair start always refers to a node")
                    .clone(),
            );
        }
        match &event {
            ExecutionDomainEvent::MutationRepairAllowanceRestored { node_id, .. } => {
                self.budget.restore_repair_attempt(node_id);
                self.budget.restore_model_call_purpose(
                    ModelCallPurpose::TargetMutationRepair,
                );
            }
            ExecutionDomainEvent::MutationRepairAllowanceConsumed { node_id, .. } => {
                self.budget.record_repair_attempt(node_id.clone());
                self.budget.record_model_call_purpose(
                    ModelCallPurpose::TargetMutationRepair,
                );
            }
            _ => {}
        }
        if let ExecutionDomainEvent::ValidationRepairStarted { failure_id, .. } = &event {
            let owner = ExecutionNodeId::new(BudgetState::repair_session_id(failure_id));
            self.budget.record_validation_repair_attempt(owner);
        }
        match &event {
            ExecutionDomainEvent::RepositoryEvidenceRecorded {
                sequence,
                evidence_id,
                repository_fingerprint,
                evidence,
            } => {
                if let Some(evidence) = evidence {
                    if &evidence.evidence_id != evidence_id
                        || &evidence.repository_fingerprint != repository_fingerprint
                        || !evidence.content_hash_is_valid()
                    {
                        return Err(GraphInvariantError::new(
                            "repository evidence event payload does not match its identity",
                        ));
                    }
                    self.evidence.record_file(evidence.clone());
                }
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::NewRelevantEvidenceRecorded,
                    None,
                )
            }
            ExecutionDomainEvent::ComplexityClassified { assessment, .. } => {
                self.budget.mission = assessment.budget.clone();
            }
            ExecutionDomainEvent::GraphCreated {
                graph_id,
                revision,
                graph: Some(replacement),
                preserved_node_ids,
                ..
            } => {
                if replacement.graph_id != *graph_id || replacement.revision != *revision {
                    return Err(GraphInvariantError::new(
                        "graph-created payload does not match its graph id and revision",
                    ));
                }
                replacement.validate_invariants()?;
                let replacement_ids = replacement
                    .nodes
                    .iter()
                    .map(|node| node.id.clone())
                    .collect::<BTreeSet<_>>();
                let retained = preserved_node_ids.iter().cloned().collect::<BTreeSet<_>>();
                if !retained.is_subset(&replacement_ids) {
                    return Err(GraphInvariantError::new(
                        "graph-created preserved node set is not contained in the replacement graph",
                    ));
                }
                self.failures
                    .records
                    .retain(|failure| retained.contains(&failure.node_id));
                self.evidence
                    .validations
                    .retain(|_, evidence| retained.contains(&evidence.node_id));
                self.evidence.records.retain(|_, evidence| {
                    evidence
                        .node_id
                        .as_ref()
                        .is_none_or(|node_id| retained.contains(node_id))
                });
                let repair_session_ids = self
                    .budget
                    .validation_repair_sessions
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                self.budget.node_usage.retain(|node_id, _| {
                    retained.contains(node_id) || repair_session_ids.contains(node_id.as_str())
                });
                self.budget.progress_events.retain(|progress| {
                    progress
                        .node_id
                        .as_ref()
                        .is_none_or(|node_id| retained.contains(node_id))
                });
                self.budget.progress_score = self
                    .budget
                    .progress_events
                    .iter()
                    .map(|progress| u64::from(progress.kind.score()))
                    .sum();
                let publication_retained = replacement
                    .nodes
                    .iter()
                    .find(|node| node.kind == ExecutionNodeKind::Publication)
                    .is_some_and(|node| retained.contains(&node.id));
                if !publication_retained {
                    self.publication = PublicationState::default();
                }
                self.graph = replacement.clone();
            }
            ExecutionDomainEvent::GraphCreated { graph: None, .. } => {}
            ExecutionDomainEvent::PlanAccepted { sequence, .. } => self
                .budget
                .record_progress_kind(*sequence, ProgressEventKind::PlanAccepted, None),
            ExecutionDomainEvent::MutationApplied {
                sequence,
                node_id,
                target_path,
                repository_fingerprint,
                evidence_id,
                ..
            } => {
                let progress = self.graph.node(node_id).map_or(
                    ProgressEventKind::SourceMutationApplied,
                    |node| {
                        if node.kind == ExecutionNodeKind::TestMutation {
                            ProgressEventKind::TestMutationApplied
                        } else {
                            ProgressEventKind::SourceMutationApplied
                        }
                    },
                );
                self.budget
                    .record_progress_kind(*sequence, progress, Some(node_id.clone()));
                self.current_repository.fingerprint = repository_fingerprint.clone();
                self.current_repository.source_tree_hash = repository_fingerprint.clone();
                self.current_repository
                    .changed_paths
                    .insert(target_path.clone());
                self.evidence.record(EvidenceRecord {
                    evidence_id: evidence_id.clone(),
                    kind: EvidenceKind::Mutation,
                    node_id: Some(node_id.clone()),
                    repository_fingerprint: repository_fingerprint.clone(),
                    summary: format!("authoritative repository mutation applied `{target_path}`"),
                });
                self.failures.supersede_for_applied_target(
                    node_id,
                    target_path,
                    repository_fingerprint,
                );
                self.evidence
                    .supersede_stale_validation(repository_fingerprint);
            }
            ExecutionDomainEvent::MutationRejected { failure, .. } => {
                self.failures.record(failure.clone());
                self.materialize_unresolved_failure_status(&failure.node_id, false)?;
            }
            ExecutionDomainEvent::MutationSuperseded {
                sequence,
                node_id,
                failure_id,
                repository_fingerprint,
            } => {
                self.failures
                    .mark_superseded(failure_id, repository_fingerprint.clone());
                self.materialize_unresolved_failure_status(node_id, true)?;
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::FailureSuperseded,
                    Some(node_id.clone()),
                );
            }
            ExecutionDomainEvent::FailureRecorded { failure, .. } => {
                self.failures.record(failure.clone());
                self.materialize_unresolved_failure_status(&failure.node_id, false)?;
            }
            ExecutionDomainEvent::FailureRecovered {
                sequence,
                node_id,
                failure_id,
                repository_fingerprint,
            } => {
                self.failures
                    .mark_recovered(failure_id, repository_fingerprint.clone());
                self.materialize_unresolved_failure_status(node_id, false)?;
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::FailureRepaired,
                    Some(node_id.clone()),
                );
            }
            ExecutionDomainEvent::FailureSuperseded {
                sequence,
                node_id,
                failure_id,
                repository_fingerprint,
            } => {
                self.failures
                    .mark_superseded(failure_id, repository_fingerprint.clone());
                self.materialize_unresolved_failure_status(node_id, true)?;
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::FailureSuperseded,
                    Some(node_id.clone()),
                );
            }
            ExecutionDomainEvent::ValidationEvidenceRecorded { evidence, .. } => {
                self.evidence.record_validation(evidence.clone());
            }
            ExecutionDomainEvent::ValidationFailed {
                sequence,
                node_id,
                failure_id,
                ..
            } => {
                self.materialize_unresolved_failure_status(node_id, false)?;
                if let Some(failure) = self.failures.get(failure_id).cloned() {
                    let revision = self
                        .budget
                        .create_validation_failure_revision(&failure, *sequence);
                    self.budget
                        .continue_validation_repair_session(&failure, revision.revision);
                    if let Some(session) = self
                        .budget
                        .repair_session_for_failure_mut(failure_id)
                    {
                        session.current_assertion_set_revision = revision.revision;
                        session.status = ValidationRepairSessionStatus::Active;
                        session.stop_reason = None;
                    }
                }
            }
            ExecutionDomainEvent::ValidationPassed {
                sequence, node_id, ..
            } => {
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::ValidationPassed,
                    Some(node_id.clone()),
                );
                for session in self
                    .budget
                    .validation_repair_sessions
                    .values_mut()
                    .filter(|session| &session.originating_gate_id == node_id)
                {
                    session.status = ValidationRepairSessionStatus::ValidationPassed;
                    session.stop_reason = Some(ValidationRepairStopReason::ValidationPassed);
                }
            }
            ExecutionDomainEvent::ValidationRepairCompleted {
                failure_id,
                result,
                attempt,
                ..
            } => {
                let admission_rejection_reason = attempt
                    .as_ref()
                    .and_then(|attempt| attempt.admission_rejection_reason.as_deref());
                if let Some(attempt) = attempt.clone() {
                    if attempt.outcome.consumes_repository_write_allowance() {
                        self.budget
                            .record_validation_repair_repository_write(failure_id)?;
                    }
                    self.budget
                        .record_validation_repair_attempt_for_failure(failure_id, attempt)?;
                }
                if let Some(session) = self.budget.repair_session_for_failure_mut(failure_id) {
                    match result {
                        RepairResult::MutationProduced { .. }
                        | RepairResult::AlreadySatisfiesRepairIntent { .. } => {
                            session.status = ValidationRepairSessionStatus::ReadyForRerun;
                        }
                        RepairResult::NoMutation { outcome, .. } => {
                            session.status = ValidationRepairSessionStatus::Stopped;
                            session.stop_reason = Some(match outcome {
                                ValidationRepairMutationOutcome::AdmissionRejected => {
                                    if admission_rejection_reason
                                        == Some("mission_model_call_budget_exhausted")
                                    {
                                        ValidationRepairStopReason::MissionBudgetExhausted
                                    } else if admission_rejection_reason
                                        == Some("repair_session_model_call_budget_exhausted")
                                    {
                                        ValidationRepairStopReason::RepairBudgetExhausted
                                    } else {
                                        ValidationRepairStopReason::AdmissionPolicyMisconfigured
                                    }
                                }
                                ValidationRepairMutationOutcome::NoValidRepair
                                | ValidationRepairMutationOutcome::WrongRepairTarget => {
                                    ValidationRepairStopReason::NoSafeRepair
                                }
                                _ => ValidationRepairStopReason::RepairBudgetExhausted,
                            });
                        }
                    }
                }
            }
            ExecutionDomainEvent::ValidationSuperseded { evidence_id, .. } => {
                if let Some(evidence) = self.evidence.validations.get_mut(evidence_id) {
                    evidence.status = ValidationEvidenceStatus::Superseded;
                }
            }
            ExecutionDomainEvent::FinalizationInvalidated {
                repository_fingerprint,
                stale_validation_evidence_ids,
                ..
            } => {
                self.current_repository.fingerprint = repository_fingerprint.clone();
                self.current_repository.source_tree_hash = repository_fingerprint.clone();
                for evidence_id in stale_validation_evidence_ids {
                    if let Some(evidence) = self.evidence.validations.get_mut(evidence_id) {
                        evidence.status = ValidationEvidenceStatus::Superseded;
                    }
                }
                self.publication = PublicationState::default();
            }
            ExecutionDomainEvent::DiffReviewed {
                sequence, node_id, ..
            } => self.budget.record_progress_kind(
                *sequence,
                ProgressEventKind::DiffReviewed,
                Some(node_id.clone()),
            ),
            ExecutionDomainEvent::RecoveryPublicationRequested { .. } => {
                self.publication.status = match self.publication.status {
                    PublicationStatus::CommitCreated | PublicationStatus::BranchPushed => {
                        self.publication.status
                    }
                    PublicationStatus::NotStarted
                    | PublicationStatus::InProgress
                    | PublicationStatus::Failed
                    | PublicationStatus::PullRequestCreated => PublicationStatus::InProgress,
                };
                self.publication.mode = Some(PublicationMode::DraftRecovery);
                self.publication.draft = true;
                self.publication.recovery_requested = true;
            }
            ExecutionDomainEvent::PublicationStarted { mode, .. } => {
                self.publication.status = PublicationStatus::InProgress;
                self.publication.mode = Some(*mode);
                self.publication.draft = matches!(
                    mode,
                    PublicationMode::Draft | PublicationMode::DraftRecovery
                );
                self.publication.recovery_requested = false;
            }
            ExecutionDomainEvent::CommitCreated {
                sequence,
                node_id,
                commit_sha,
            } => {
                self.publication.status = PublicationStatus::CommitCreated;
                self.publication.commit_sha = Some(commit_sha.clone());
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::CommitCreated,
                    Some(node_id.clone()),
                );
            }
            ExecutionDomainEvent::BranchPushed { branch, .. } => {
                self.publication.status = PublicationStatus::BranchPushed;
                self.publication.branch = Some(branch.clone());
            }
            ExecutionDomainEvent::PullRequestCreated {
                sequence,
                node_id,
                url,
                number,
                draft,
            } => {
                self.publication.status = PublicationStatus::PullRequestCreated;
                self.publication.pull_request_url = Some(url.clone());
                self.publication.pull_request_number = *number;
                self.publication.draft = *draft;
                self.budget.record_progress_kind(
                    *sequence,
                    ProgressEventKind::PullRequestCreated,
                    Some(node_id.clone()),
                );
            }
            ExecutionDomainEvent::CancellationRequested { state, .. } => {
                self.cancellation = Some(state.clone());
            }
            ExecutionDomainEvent::ExecutionResumed {
                previous_outcome, ..
            } => {
                self.cancellation = None;
                if *previous_outcome == Some(MissionOutcome::PartialReviewable) {
                    let infrastructure_failure_ids = self
                        .failures
                        .unresolved()
                        .filter(|failure| {
                            failure.category == FailureCategory::InfrastructureFailure
                        })
                        .map(|failure| failure.id.clone())
                        .collect::<Vec<_>>();
                    for failure_id in infrastructure_failure_ids {
                        self.failures.mark_recovered(
                            &failure_id,
                            self.current_repository.fingerprint.clone(),
                        );
                    }
                    self.publication.status = PublicationStatus::NotStarted;
                    self.publication.mode = None;
                    self.publication.commit_sha = None;
                    self.publication.recovery_requested = false;
                }
            }
            _ => {}
        }
        self.events.push(event);
        Ok(())
    }

    fn materialize_unresolved_failure_status(
        &mut self,
        node_id: &ExecutionNodeId,
        superseded_target: bool,
    ) -> Result<(), GraphInvariantError> {
        let unresolved = self
            .failures
            .unresolved_for_node(node_id)
            .map(|failure| failure.category.node_status())
            .collect::<Vec<_>>();
        let desired = if unresolved.contains(&ExecutionNodeStatus::FailedBlocking) {
            Some(ExecutionNodeStatus::FailedBlocking)
        } else if !unresolved.is_empty() {
            Some(ExecutionNodeStatus::FailedRecoverable)
        } else if superseded_target {
            Some(ExecutionNodeStatus::Superseded)
        } else {
            self.graph.node(node_id).and_then(|node| {
                matches!(
                    node.status,
                    ExecutionNodeStatus::FailedRecoverable | ExecutionNodeStatus::FailedBlocking
                )
                .then_some(ExecutionNodeStatus::Pending)
            })
        };
        if let Some(status) = desired
            && self
                .graph
                .node(node_id)
                .is_some_and(|node| node.status != status)
        {
            self.graph.set_node_status(node_id, status)?;
        }
        Ok(())
    }

    fn node_start_is_target_repair(
        &self,
        event: &ExecutionDomainEvent,
        node_id: &ExecutionNodeId,
    ) -> bool {
        if !matches!(event, ExecutionDomainEvent::NodeStarted { .. }) {
            return false;
        }
        let Some(node) = self.graph.node(node_id) else {
            return false;
        };
        if !node.kind.is_mutation() {
            return false;
        }
        node.status == ExecutionNodeStatus::FailedRecoverable
            || self.failures.unresolved().any(|failure| {
                failure.category.creates_repair_work()
                    && (&failure.node_id == node_id
                        || node.target.as_ref().is_some_and(|target| {
                            failure.target_path.as_deref() == Some(target.path.as_str())
                        }))
            })
    }

    fn validate_event_semantics(
        &self,
        event: &ExecutionDomainEvent,
    ) -> Result<(), GraphInvariantError> {
        match event {
            ExecutionDomainEvent::GraphCreated { revision, .. } => {
                if self.events.iter().rev().find_map(|event| match event {
                    ExecutionDomainEvent::GraphCreated { revision, .. } => Some(*revision),
                    _ => None,
                }).is_some_and(|previous| *revision < previous) {
                    return Err(GraphInvariantError::new(
                        "persisted graph revisions must be monotonic",
                    ));
                }
            }
            ExecutionDomainEvent::FinalizationInvalidated {
                repository_fingerprint,
                stale_validation_evidence_ids,
                ..
            } => {
                if repository_fingerprint.trim().is_empty() {
                    return Err(GraphInvariantError::new(
                        "finalization invalidation requires a repository fingerprint",
                    ));
                }
                let expected = self.finalization_validation_evidence_ids();
                if stale_validation_evidence_ids != &expected {
                    return Err(GraphInvariantError::new(format!(
                        "finalization invalidation validation evidence ids must exactly match {:?}",
                        expected
                    )));
                }
            }
            ExecutionDomainEvent::RecoveryPublicationRequested {
                node_id,
                repository_fingerprint,
                validation_evidence_ids,
                ..
            } => {
                if self.terminal_outcome().is_some() {
                    return Err(GraphInvariantError::new(
                        "recovery publication cannot be requested after RunFinished",
                    ));
                }
                if repository_fingerprint.trim().is_empty()
                    || repository_fingerprint != &self.current_repository.fingerprint
                {
                    return Err(GraphInvariantError::new(
                        "recovery publication requires the current repository fingerprint",
                    ));
                }
                if !self.current_repository.has_changes() {
                    return Err(GraphInvariantError::new(
                        "recovery publication requires a non-empty repository diff",
                    ));
                }
                let publication = self.graph.node(node_id).ok_or_else(|| {
                    GraphInvariantError::new(format!(
                        "recovery publication refers to unknown node `{node_id}`"
                    ))
                })?;
                if publication.kind != ExecutionNodeKind::Publication {
                    return Err(GraphInvariantError::new(format!(
                        "recovery publication node `{node_id}` is not a publication node"
                    )));
                }
                if publication.status == ExecutionNodeStatus::Completed
                    || self.publication.status == PublicationStatus::PullRequestCreated
                {
                    return Err(GraphInvariantError::new(
                        "recovery publication cannot replace completed publication",
                    ));
                }
                let expected = self.recovery_publication_validation_evidence_ids()?;
                if validation_evidence_ids != &expected {
                    return Err(GraphInvariantError::new(format!(
                        "recovery publication validation evidence ids must exactly match {:?}",
                        expected
                    )));
                }
            }
            ExecutionDomainEvent::PublicationStarted {
                mode: PublicationMode::DraftRecovery,
                ..
            } => {
                return Err(GraphInvariantError::new(
                    "draft recovery publication must start with RecoveryPublicationRequested",
                ));
            }
            ExecutionDomainEvent::PublicationStarted { .. }
                if self.publication.recovery_requested =>
            {
                return Err(GraphInvariantError::new(
                    "recovery publication is already authorized",
                ));
            }
            ExecutionDomainEvent::CommitCreated { commit_sha, .. }
                if self
                    .publication
                    .commit_sha
                    .as_ref()
                    .is_some_and(|existing| existing != commit_sha) =>
            {
                return Err(GraphInvariantError::new(
                    "recovery cannot replace an already-created commit",
                ));
            }
            ExecutionDomainEvent::BranchPushed { branch, .. }
                if self
                    .publication
                    .branch
                    .as_ref()
                    .is_some_and(|existing| existing != branch) =>
            {
                return Err(GraphInvariantError::new(
                    "recovery cannot replace an already-pushed branch",
                ));
            }
            ExecutionDomainEvent::PullRequestCreated { url, number, .. }
                if self.publication.pull_request_url.as_ref().is_some_and(|existing| {
                    existing != url || self.publication.pull_request_number != *number
                }) =>
            {
                return Err(GraphInvariantError::new(
                    "recovery cannot replace an already-created pull request",
                ));
            }
            ExecutionDomainEvent::GuardrailTriggered {
                outcome: MissionOutcome::PartialReviewable,
                ..
            } if !self.current_repository.has_changes() => {
                return Err(GraphInvariantError::new(
                    "partial-reviewable guardrail requires a non-empty repository diff",
                ));
            }
            ExecutionDomainEvent::MutationRejected {
                node_id, failure, ..
            } => self.validate_failure_record(failure, Some(node_id), true)?,
            ExecutionDomainEvent::FailureRecorded { failure, .. } => {
                self.validate_failure_record(failure, None, false)?
            }
            ExecutionDomainEvent::MutationSuperseded {
                node_id,
                failure_id,
                repository_fingerprint,
                ..
            }
            | ExecutionDomainEvent::FailureSuperseded {
                node_id,
                failure_id,
                repository_fingerprint,
                ..
            } => {
                self.validate_failure_resolution(node_id, failure_id, repository_fingerprint, true)?
            }
            ExecutionDomainEvent::FailureRecovered {
                node_id,
                failure_id,
                repository_fingerprint,
                ..
            } => self.validate_failure_resolution(
                node_id,
                failure_id,
                repository_fingerprint,
                false,
            )?,
            ExecutionDomainEvent::ValidationEvidenceRecorded {
                node_id, evidence, ..
            } => self.validate_validation_evidence(node_id, evidence)?,
            ExecutionDomainEvent::ExecutionResumed {
                execution_attempt,
                previous_outcome,
                ..
            } => {
                if *execution_attempt == 0 {
                    return Err(GraphInvariantError::new(
                        "execution resume requires a non-zero execution attempt",
                    ));
                }
                let resumes_cancellation = previous_outcome.is_none()
                    && self.cancellation.is_some()
                    && self.terminal_outcome().is_none();
                let resumes_partial = *previous_outcome == Some(MissionOutcome::PartialReviewable)
                    && self.terminal_outcome() == Some(MissionOutcome::PartialReviewable);
                if !resumes_cancellation && !resumes_partial {
                    return Err(GraphInvariantError::new(
                        "execution resume requires a cancellation checkpoint or partial-reviewable terminal outcome",
                    ));
                }
            }
            ExecutionDomainEvent::ValidationPassed {
                node_id,
                evidence_id,
                fingerprint,
                ..
            } => {
                let evidence = self.evidence.validations.get(evidence_id).ok_or_else(|| {
                    GraphInvariantError::new(format!(
                        "validation pass refers to unknown evidence `{evidence_id}`"
                    ))
                })?;
                if &evidence.node_id != node_id {
                    return Err(GraphInvariantError::new(format!(
                        "validation evidence `{evidence_id}` belongs to node `{}`, not `{node_id}`",
                        evidence.node_id
                    )));
                }
                if evidence.status != ValidationEvidenceStatus::Passed {
                    return Err(GraphInvariantError::new(format!(
                        "validation pass requires passed evidence `{evidence_id}`"
                    )));
                }
                self.validate_current_attached_validation_evidence(node_id, evidence, fingerprint)?;
            }
            ExecutionDomainEvent::ValidationFailed {
                node_id,
                failure_id,
                fingerprint,
                ..
            } => {
                let has_failed_evidence = self.evidence.validations.values().any(|evidence| {
                    &evidence.node_id == node_id
                        && matches!(
                            evidence.status,
                            ValidationEvidenceStatus::Failed
                                | ValidationEvidenceStatus::TimedOut
                                | ValidationEvidenceStatus::Cancelled
                        )
                        && self
                            .validate_current_attached_validation_evidence(
                                node_id,
                                evidence,
                                fingerprint,
                            )
                            .is_ok()
                });
                if !has_failed_evidence {
                    return Err(GraphInvariantError::new(format!(
                        "validation failure for node `{node_id}` requires attached current non-pass evidence matching fingerprint `{fingerprint}`"
                    )));
                }
                let failure = self.failures.get(failure_id).ok_or_else(|| {
                    GraphInvariantError::new(format!(
                        "validation failure refers to unknown failure `{failure_id}`"
                    ))
                })?;
                if &failure.node_id != node_id {
                    return Err(GraphInvariantError::new(format!(
                        "validation failure `{failure_id}` belongs to node `{}`, not `{node_id}`",
                        failure.node_id
                    )));
                }
                if !failure.is_unresolved() {
                    return Err(GraphInvariantError::new(format!(
                        "validation failure `{failure_id}` is already resolved"
                    )));
                }
                if !matches!(
                    failure.category,
                    FailureCategory::ValidationFailure | FailureCategory::InfrastructureFailure
                ) {
                    return Err(GraphInvariantError::new(format!(
                        "validation event cannot materialize failure `{failure_id}` of category `{:?}`",
                        failure.category
                    )));
                }
            }
            ExecutionDomainEvent::RunFinished { outcome, .. }
                if outcome.is_successful_domain_result()
                    && (!self.publication.is_published()
                        || !self.graph.nodes.iter().any(|node| {
                            node.kind == ExecutionNodeKind::Publication
                                && node.status.satisfies_dependency()
                        })) =>
            {
                return Err(GraphInvariantError::new(
                    "successful RunFinished requires completed pull-request publication",
                ));
            }
            ExecutionDomainEvent::PullRequestCreated { draft: false, .. }
                if self.publication.recovery_requested =>
            {
                return Err(GraphInvariantError::new(
                    "recovery publication requires a draft pull request",
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn validate_current_attached_validation_evidence(
        &self,
        node_id: &ExecutionNodeId,
        evidence: &ValidationEvidenceRecord,
        fingerprint: &str,
    ) -> Result<(), GraphInvariantError> {
        if evidence.fingerprint != fingerprint {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` fingerprint does not match outcome fingerprint",
                evidence.evidence_id
            )));
        }
        if evidence.repository_fingerprint
            != self.current_repository.validation_source_tree_hash()
        {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` is not current for the repository",
                evidence.evidence_id
            )));
        }
        let node = self.graph.node(node_id).ok_or_else(|| {
            GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
        })?;
        if !node.evidence_ids.contains(&evidence.evidence_id) {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` is not attached to node `{node_id}`",
                evidence.evidence_id
            )));
        }
        Ok(())
    }

    fn validate_validation_evidence(
        &self,
        node_id: &ExecutionNodeId,
        evidence: &ValidationEvidenceRecord,
    ) -> Result<(), GraphInvariantError> {
        if evidence.evidence_id.trim().is_empty() {
            return Err(GraphInvariantError::new(
                "validation evidence requires a non-empty evidence id",
            ));
        }
        if &evidence.node_id != node_id {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` belongs to node `{}`, not event node `{node_id}`",
                evidence.evidence_id, evidence.node_id
            )));
        }
        if self
            .evidence
            .validations
            .contains_key(&evidence.evidence_id)
        {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` is already recorded",
                evidence.evidence_id
            )));
        }
        let node = self.graph.node(node_id).ok_or_else(|| {
            GraphInvariantError::new(format!(
                "validation evidence `{}` refers to unknown node `{node_id}`",
                evidence.evidence_id
            ))
        })?;
        let gate = node.validation.as_ref().ok_or_else(|| {
            GraphInvariantError::new(format!(
                "validation evidence `{}` refers to non-validation node `{node_id}`",
                evidence.evidence_id
            ))
        })?;
        if evidence.gate_id != gate.gate_id {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` gate `{}` does not match node gate `{}`",
                evidence.evidence_id, evidence.gate_id, gate.gate_id
            )));
        }
        let source_tree_hash = self.current_repository.validation_source_tree_hash();
        if evidence.repository_fingerprint.trim().is_empty()
            || evidence.repository_fingerprint != source_tree_hash
        {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` requires the current repository fingerprint",
                evidence.evidence_id
            )));
        }
        let expected_fingerprint = gate.fingerprint(source_tree_hash);
        if evidence.fingerprint != expected_fingerprint {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` fingerprint does not match gate `{}` at the current repository state",
                evidence.evidence_id, gate.gate_id
            )));
        }
        if evidence.command != gate.command || evidence.working_directory != gate.working_directory
        {
            return Err(GraphInvariantError::new(format!(
                "validation evidence `{}` command context does not match gate `{}`",
                evidence.evidence_id, gate.gate_id
            )));
        }
        Ok(())
    }

    fn validate_failure_record(
        &self,
        failure: &FailureRecord,
        event_node_id: Option<&ExecutionNodeId>,
        mutation_only: bool,
    ) -> Result<(), GraphInvariantError> {
        if failure.id.as_str().trim().is_empty() {
            return Err(GraphInvariantError::new(
                "failure event requires a non-empty failure id",
            ));
        }
        if failure.node_id.as_str().trim().is_empty() {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` requires a non-empty node id",
                failure.id
            )));
        }
        if let Some(event_node_id) = event_node_id
            && event_node_id != &failure.node_id
        {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` belongs to node `{}`, not event node `{event_node_id}`",
                failure.id, failure.node_id
            )));
        }
        if self.failures.get(&failure.id).is_some() {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` is already recorded",
                failure.id
            )));
        }
        if !failure.is_unresolved()
            || failure.status != FailureStatus::Active
            || failure.resolved_repository_fingerprint.is_some()
        {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` must be recorded in active unresolved state",
                failure.id
            )));
        }
        if failure.attempt == 0 {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` requires a positive attempt",
                failure.id
            )));
        }
        if failure.repository_fingerprint.trim().is_empty() {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` requires a repository fingerprint",
                failure.id
            )));
        }
        if failure.message.trim().is_empty() {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` requires a diagnostic message",
                failure.id
            )));
        }
        let node = self.graph.node(&failure.node_id).ok_or_else(|| {
            GraphInvariantError::new(format!(
                "failure `{}` refers to unknown node `{}`",
                failure.id, failure.node_id
            ))
        })?;
        if mutation_only && !node.kind.is_mutation() {
            return Err(GraphInvariantError::new(format!(
                "mutation failure `{}` refers to non-mutation node `{}`",
                failure.id, failure.node_id
            )));
        }
        if !failure.category.is_valid_for_node_kind(node.kind) {
            return Err(GraphInvariantError::new(format!(
                "failure `{}` category `{:?}` is invalid for node `{}` of kind `{:?}`",
                failure.id, failure.category, failure.node_id, node.kind
            )));
        }
        if mutation_only && failure.target_path.is_none() {
            return Err(GraphInvariantError::new(format!(
                "mutation failure `{}` requires its planned target path",
                failure.id
            )));
        }
        if let Some(target_path) = failure.target_path.as_deref() {
            let path_matches = if node.kind.is_mutation() {
                node.target
                    .as_ref()
                    .is_some_and(|target| target.path == target_path)
            } else {
                self.graph.nodes.iter().any(|candidate| {
                    candidate.kind.is_mutation()
                        && candidate
                            .target
                            .as_ref()
                            .is_some_and(|target| target.path == target_path)
                })
            };
            if !path_matches {
                return Err(GraphInvariantError::new(format!(
                    "failure `{}` target path `{target_path}` is not a matching planned target",
                    failure.id
                )));
            }
        }
        Ok(())
    }

    fn validate_failure_resolution(
        &self,
        node_id: &ExecutionNodeId,
        failure_id: &FailureId,
        repository_fingerprint: &str,
        superseded: bool,
    ) -> Result<(), GraphInvariantError> {
        if repository_fingerprint.trim().is_empty() {
            return Err(GraphInvariantError::new(format!(
                "failure resolution for `{failure_id}` requires a repository fingerprint"
            )));
        }
        let failure = self.failures.get(failure_id).ok_or_else(|| {
            GraphInvariantError::new(format!(
                "failure resolution refers to unknown failure `{failure_id}`"
            ))
        })?;
        if &failure.node_id != node_id {
            return Err(GraphInvariantError::new(format!(
                "failure `{failure_id}` belongs to node `{}`, not `{node_id}`",
                failure.node_id
            )));
        }
        if !failure.is_unresolved() {
            return Err(GraphInvariantError::new(format!(
                "failure `{failure_id}` is already resolved"
            )));
        }
        let node = self.graph.node(node_id).ok_or_else(|| {
            GraphInvariantError::new(format!(
                "failure `{failure_id}` refers to unknown node `{node_id}`"
            ))
        })?;
        if !failure.category.is_valid_for_node_kind(node.kind) {
            return Err(GraphInvariantError::new(format!(
                "failure `{failure_id}` category `{:?}` is invalid for node `{node_id}` of kind `{:?}`",
                failure.category, node.kind
            )));
        }
        if superseded {
            if !failure.category.is_supersedable_by_applied_target() {
                return Err(GraphInvariantError::new(format!(
                    "failure `{failure_id}` of category `{:?}` cannot be superseded",
                    failure.category
                )));
            }
            if !node.kind.is_mutation() {
                return Err(GraphInvariantError::new(format!(
                    "superseded failure `{failure_id}` must belong to a mutation node"
                )));
            }
        }
        Ok(())
    }

    pub fn with_event(&self, event: ExecutionDomainEvent) -> Result<Self, GraphInvariantError> {
        let mut next = self.clone();
        next.append_event(event)?;
        Ok(next)
    }

    pub fn validate_invariants(&self) -> Result<(), GraphInvariantError> {
        if self.run_id.trim().is_empty() {
            return Err(GraphInvariantError::new(
                "execution run id must not be empty",
            ));
        }
        self.graph
            .validate_invariants_with_dependency_satisfaction(
                &self.dependency_satisfaction_ids(),
            )?;
        self.budget.validate_invariants(&self.graph)?;
        let historical_node_ids = self
            .graph
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .chain(
                self.events
                    .iter()
                    .filter_map(ExecutionDomainEvent::node_id)
                    .cloned(),
            )
            .chain(self.events.iter().flat_map(|event| {
                match event {
                    ExecutionDomainEvent::GraphCreated {
                        graph: Some(graph), ..
                    } => graph
                        .nodes
                        .iter()
                        .map(|node| node.id.clone())
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                }
            }))
            .collect::<BTreeSet<_>>();
        let mut previous_sequence = None;
        let mut previous_graph_revision = None;
        let mut terminal_seen = None;
        for event in &self.events {
            if let Some(outcome) = terminal_seen {
                let valid_partial_resume = matches!(
                    event,
                    ExecutionDomainEvent::ExecutionResumed {
                        previous_outcome: Some(MissionOutcome::PartialReviewable),
                        ..
                    }
                ) && outcome == MissionOutcome::PartialReviewable;
                if !valid_partial_resume {
                    return Err(GraphInvariantError::new(
                        "domain event occurs after terminal RunFinished",
                    ));
                }
                terminal_seen = None;
            }
            if previous_sequence.is_some_and(|previous| event.sequence() <= previous) {
                return Err(GraphInvariantError::new(
                    "domain event sequence is not strictly increasing",
                ));
            }
            if let Some(node_id) = event.node_id()
                && !historical_node_ids.contains(node_id)
            {
                return Err(GraphInvariantError::new(format!(
                    "event `{}` refers to unknown node `{node_id}`",
                    event.event_type()
                )));
            }
            if let ExecutionDomainEvent::RunFinished { outcome, .. } = event {
                terminal_seen = Some(*outcome);
            }
            if let ExecutionDomainEvent::GraphCreated { revision, .. } = event {
                if previous_graph_revision.is_some_and(|previous| *revision < previous) {
                    return Err(GraphInvariantError::new(
                        "persisted graph revisions are not monotonic",
                    ));
                }
                previous_graph_revision = Some(*revision);
            }
            previous_sequence = Some(event.sequence());
        }
        if previous_graph_revision.is_some_and(|revision| self.graph.revision < revision) {
            return Err(GraphInvariantError::new(
                "materialized graph revision precedes its persisted event revision",
            ));
        }
        for failure in &self.failures.records {
            if self.graph.node(&failure.node_id).is_none() {
                return Err(GraphInvariantError::new(format!(
                    "failure `{}` refers to unknown node `{}`",
                    failure.id, failure.node_id
                )));
            }
        }
        if self.publication.recovery_requested {
            if self.publication.mode != Some(PublicationMode::DraftRecovery)
                || !self.publication.draft
                || self.publication.status == PublicationStatus::NotStarted
                || !self.graph.recovery_publication_dependency_override
            {
                return Err(GraphInvariantError::new(
                    "recovery publication state requires draft-recovery mode and its graph dependency override",
                ));
            }
        } else if self.graph.recovery_publication_dependency_override {
            return Err(GraphInvariantError::new(
                "recovery publication graph dependency override has no authorizing publication state",
            ));
        }
        if let Some(outcome) = self.terminal_outcome()
            && outcome.is_successful_domain_result()
            && (!self.publication.is_published()
                || !self.graph.nodes.iter().any(|node| {
                    node.kind == ExecutionNodeKind::Publication
                        && node.status.satisfies_dependency()
                }))
        {
            return Err(GraphInvariantError::new(
                "successful terminal outcome has no completed pull-request publication",
            ));
        }
        Ok(())
    }
}
