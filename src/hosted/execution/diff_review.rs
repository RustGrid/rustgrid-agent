// Extracted from the hosted execution composition root.
use super::*;

impl<'a> GatewayAgent<'a> {
    pub(in crate::hosted) fn deterministic_diff_review(&mut self) -> Result<Vec<String>> {
        if self
            .notebook
            .required_gates
            .iter()
            .any(|gate| gate.required && gate.status != ValidationStatus::Passed)
        {
            return Err(anyhow!(HostedInvariantFailure::new(
                "diff_review_before_validation",
                "diff review requires every required gate to pass",
            )));
        }
        let changed_paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
        let changed = changed_paths.iter().cloned().collect::<BTreeSet<_>>();
        let planned = self
            .notebook
            .intended_changes
            .iter()
            .flat_map(|change| change.targets.iter().map(|target| target.path.clone()))
            .collect::<BTreeSet<_>>();
        let unplanned = changed.difference(&planned).cloned().collect::<Vec<_>>();
        let planned_but_unchanged = planned.difference(&changed).cloned().collect::<Vec<_>>();
        let decision = self.reconcile_active_phase(
            "required validation gates passed; orchestrator is reviewing the final repository diff",
        )?;
        if !matches!(
            decision,
            PhaseDecision::Transition(ExecutionPhase::DiffReview)
        ) && self.phases.active() != ExecutionPhase::DiffReview
        {
            return Err(anyhow!(HostedInvariantFailure::new(
                "diff_review_phase_invalid",
                "diff review requires the validation phase",
            )));
        }
        let node_id = self.graph_node_id(crate::execution_graph::ExecutionNodeKind::DiffReview)?;
        let evidence_ids = self
            .notebook
            .validation_evidence
            .iter()
            .filter(|evidence| evidence.status == ValidationStatus::Passed)
            .map(|evidence| evidence.evidence_id.clone())
            .collect::<Vec<_>>();
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::DiffReviewed {
                sequence: self.next_domain_event_sequence(),
                node_id,
                evidence_ids,
            },
        )?;
        self.diff_reviewed = true;
        self.persist_orchestration_checkpoint("diff_review_completed", false)?;
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.diff_review_completed",
                "changed_paths": changed_paths,
                "unplanned_paths": unplanned,
                "planned_but_unchanged": planned_but_unchanged,
                "source_tree_hash": repository_state_fingerprint(
                    self.repo,
                    &self.manifest.github.base_sha,
                )?,
            }),
            "deterministic diff review",
        );
        Ok(changed_paths)
    }
}
