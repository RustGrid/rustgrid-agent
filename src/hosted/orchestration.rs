use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::lifecycle::HostedExecutionStage;

pub(super) const DEFAULT_HOSTED_MODEL_CALLS: usize = 40;
pub(super) const MINIMUM_HOSTED_MODEL_CALLS: usize = 10;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ExecutionPhase {
    Discovery,
    ArtifactRepair,
    Planning,
    Implementation,
    Repair,
    DiffReview,
    CompletionEvaluation,
    Validation,
    Publication,
}

impl ExecutionPhase {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::ArtifactRepair => "artifact_repair",
            Self::Planning => "planning",
            Self::Implementation => "implementation",
            Self::Repair => "repair",
            Self::DiffReview => "diff_review",
            Self::CompletionEvaluation => "completion_evaluation",
            Self::Validation => "validation",
            Self::Publication => "publication",
        }
    }

    pub(super) const fn permits_model_call(self) -> bool {
        matches!(
            self,
            Self::Discovery
                | Self::ArtifactRepair
                | Self::Planning
                | Self::Implementation
                | Self::Repair
                | Self::DiffReview
                | Self::CompletionEvaluation
        )
    }

    pub(super) const fn stage(self) -> HostedExecutionStage {
        match self {
            Self::Discovery | Self::ArtifactRepair => HostedExecutionStage::Discovery,
            Self::Planning => HostedExecutionStage::Planning,
            Self::Implementation | Self::Repair => HostedExecutionStage::Implementation,
            Self::Validation => HostedExecutionStage::Validation,
            Self::DiffReview | Self::CompletionEvaluation => HostedExecutionStage::Review,
            Self::Publication => HostedExecutionStage::Publication,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct PhaseBudgetAllocation {
    pub(super) discovery_maximum: usize,
    pub(super) planning_maximum: usize,
    pub(super) implementation_repair_reserved: usize,
    pub(super) diff_review_reserved: usize,
    pub(super) completion_evaluation_reserved: usize,
}

impl PhaseBudgetAllocation {
    #[cfg(test)]
    pub(super) const fn total(self) -> usize {
        self.discovery_maximum
            + self.planning_maximum
            + self.implementation_repair_reserved
            + self.diff_review_reserved
            + self.completion_evaluation_reserved
    }
}

pub(super) fn phase_budget_allocation(total: usize) -> PhaseBudgetAllocation {
    if total == 0 {
        return PhaseBudgetAllocation {
            discovery_maximum: 0,
            planning_maximum: 0,
            implementation_repair_reserved: 0,
            diff_review_reserved: 0,
            completion_evaluation_reserved: 0,
        };
    }

    let discovery = total.min(5);
    let planning = total.saturating_sub(discovery).min(3);
    let finalization_reserve = if total <= 20 { 1 } else { 3 };
    let completion_evaluation = total
        .saturating_sub(discovery + planning)
        .min(finalization_reserve);
    let diff_review = total
        .saturating_sub(discovery + planning + completion_evaluation)
        .min(finalization_reserve);
    let implementation = total
        .saturating_sub(discovery)
        .saturating_sub(planning)
        .saturating_sub(diff_review)
        .saturating_sub(completion_evaluation);
    PhaseBudgetAllocation {
        discovery_maximum: discovery,
        planning_maximum: planning,
        implementation_repair_reserved: implementation,
        diff_review_reserved: diff_review,
        completion_evaluation_reserved: completion_evaluation,
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct PhaseLedger {
    active: ExecutionPhase,
    total_limit: usize,
    allocation: PhaseBudgetAllocation,
    discovery_calls: usize,
    artifact_repair_calls: usize,
    planning_calls: usize,
    implementation_calls: usize,
    repair_calls: usize,
    diff_review_calls: usize,
    completion_evaluation_calls: usize,
    reallocated_diff_review_calls: usize,
    reallocated_completion_evaluation_calls: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(super) struct PhaseBudgetReallocation {
    pub(super) diff_review_calls: usize,
    pub(super) completion_evaluation_calls: usize,
}

impl PhaseLedger {
    pub(super) fn new(total_limit: usize, initial: ExecutionPhase) -> Self {
        Self {
            active: initial,
            total_limit,
            allocation: phase_budget_allocation(total_limit),
            discovery_calls: 0,
            artifact_repair_calls: 0,
            planning_calls: 0,
            implementation_calls: 0,
            repair_calls: 0,
            diff_review_calls: 0,
            completion_evaluation_calls: 0,
            reallocated_diff_review_calls: 0,
            reallocated_completion_evaluation_calls: 0,
        }
    }

    pub(super) const fn active(&self) -> ExecutionPhase {
        self.active
    }

    pub(super) fn transition(&mut self, phase: ExecutionPhase) {
        self.active = phase;
    }

    pub(super) const fn total_limit(&self) -> usize {
        self.total_limit
    }

    #[cfg(test)]
    pub(super) fn apply_ticket_complexity(&mut self, target_count: usize) -> usize {
        let complexity_limit = match target_count {
            0 | 1 => 14,
            2..=8 => 25,
            9..=12 => 45,
            _ => 80,
        };
        self.apply_complexity_limit(complexity_limit)
    }

    pub(super) fn apply_complexity_limit(&mut self, complexity_limit: usize) -> usize {
        self.total_limit = self
            .total_limit
            .min(complexity_limit)
            .max(self.total_calls());
        self.allocation = phase_budget_allocation(self.total_limit);
        self.reallocated_diff_review_calls = 0;
        self.reallocated_completion_evaluation_calls = 0;
        self.total_limit
    }

    pub(super) const fn total_calls(&self) -> usize {
        self.discovery_calls
            + self.artifact_repair_calls
            + self.planning_calls
            + self.implementation_calls
            + self.repair_calls
            + self.diff_review_calls
            + self.completion_evaluation_calls
    }

    pub(super) const fn budgeted_calls(&self) -> usize {
        self.total_calls()
            .saturating_sub(self.artifact_repair_calls)
    }

    pub(super) const fn implementation_repair_calls(&self) -> usize {
        self.implementation_calls + self.repair_calls
    }

    pub(super) const fn phase_calls(&self, phase: ExecutionPhase) -> usize {
        match phase {
            ExecutionPhase::Discovery => self.discovery_calls,
            ExecutionPhase::ArtifactRepair => self.artifact_repair_calls,
            ExecutionPhase::Planning => self.planning_calls,
            ExecutionPhase::Implementation => self.implementation_calls,
            ExecutionPhase::Repair => self.repair_calls,
            ExecutionPhase::DiffReview => self.diff_review_calls,
            ExecutionPhase::CompletionEvaluation => self.completion_evaluation_calls,
            ExecutionPhase::Validation | ExecutionPhase::Publication => 0,
        }
    }

    pub(super) fn implementation_repair_capacity(&self) -> usize {
        let capacity = self
            .total_limit
            .saturating_sub(self.discovery_calls)
            .saturating_sub(self.artifact_repair_calls)
            .saturating_sub(self.planning_calls)
            .saturating_sub(self.allocation.diff_review_reserved)
            .saturating_sub(self.allocation.completion_evaluation_reserved)
            .saturating_sub(self.reallocated_diff_review_calls)
            .saturating_sub(self.reallocated_completion_evaluation_calls);
        if self.total_limit <= 20 {
            capacity.min(self.allocation.implementation_repair_reserved)
        } else if self.total_limit <= 25 {
            // Preserve the established small-mission shape: unused discovery
            // and planning calls may roll into implementation, while review
            // and completion remain protected.
            capacity.min(18)
        } else {
            // Medium and large graph-classified missions must be able to use
            // their class-specific implementation allocation. A legacy global
            // 18-call ceiling would silently defeat their 45/80-call mission
            // budgets, while this cap still protects the other node pools.
            capacity.min(self.allocation.implementation_repair_reserved)
        }
    }

    pub(super) fn ensure_finalization_minimum(&mut self, criterion_count: usize) {
        if self.total_limit <= 20 {
            self.reallocated_diff_review_calls = 0;
            self.reallocated_completion_evaluation_calls = 0;
            return;
        }
        self.reallocated_diff_review_calls = 4_usize
            .saturating_sub(self.allocation.diff_review_reserved)
            .min(self.allocation.implementation_repair_reserved);
        let evaluation_minimum: usize = if criterion_count > 5 { 3 } else { 1 };
        self.reallocated_completion_evaluation_calls = evaluation_minimum
            .saturating_sub(self.allocation.completion_evaluation_reserved)
            .min(
                self.allocation
                    .implementation_repair_reserved
                    .saturating_sub(self.reallocated_diff_review_calls),
            );
    }

    pub(super) fn release_unused_implementation_capacity(&mut self) -> PhaseBudgetReallocation {
        let unused = self
            .implementation_repair_capacity()
            .saturating_sub(self.implementation_repair_calls());
        let diff_review_calls = unused.min(4);
        let completion_evaluation_calls = unused.saturating_sub(diff_review_calls);
        self.reallocated_diff_review_calls = self
            .reallocated_diff_review_calls
            .saturating_add(diff_review_calls);
        self.reallocated_completion_evaluation_calls = self
            .reallocated_completion_evaluation_calls
            .saturating_add(completion_evaluation_calls);
        PhaseBudgetReallocation {
            diff_review_calls,
            completion_evaluation_calls,
        }
    }

    #[cfg(test)]
    pub(super) const fn diff_review_start_call(&self) -> usize {
        self.total_limit
            .saturating_sub(self.allocation.diff_review_reserved)
            .saturating_sub(self.allocation.completion_evaluation_reserved)
            .saturating_add(1)
    }

    pub(super) fn phase_limit(&self, phase: ExecutionPhase) -> usize {
        match phase {
            ExecutionPhase::Discovery => self.allocation.discovery_maximum,
            ExecutionPhase::ArtifactRepair => 1,
            ExecutionPhase::Planning => self.allocation.planning_maximum,
            ExecutionPhase::Implementation => self
                .implementation_repair_capacity()
                .saturating_sub(self.repair_calls),
            ExecutionPhase::Repair => self
                .implementation_repair_capacity()
                .saturating_sub(self.implementation_calls),
            ExecutionPhase::DiffReview => self
                .allocation
                .diff_review_reserved
                .saturating_add(self.reallocated_diff_review_calls),
            ExecutionPhase::CompletionEvaluation => self
                .allocation
                .completion_evaluation_reserved
                .saturating_add(self.reallocated_completion_evaluation_calls),
            ExecutionPhase::Validation | ExecutionPhase::Publication => 0,
        }
    }

    #[cfg(test)]
    pub(super) fn begin_model_call(&mut self) -> Result<usize> {
        if !self.active.permits_model_call() {
            bail!(
                "phase `{}` does not permit model calls",
                self.active.as_str()
            );
        }
        if self.total_calls() >= self.total_limit {
            bail!("execution AI model-call budget was exhausted");
        }
        if matches!(
            self.active,
            ExecutionPhase::Implementation | ExecutionPhase::Repair
        ) && self.implementation_repair_calls() >= self.implementation_repair_capacity()
        {
            bail!(
                "implementation and repair exhausted their shared model-call allocation ({}/{})",
                self.implementation_repair_calls(),
                self.implementation_repair_capacity()
            );
        }
        let used = self.phase_calls(self.active);
        if used >= self.phase_limit(self.active) {
            bail!(
                "phase `{}` exhausted its model-call allocation ({}/{})",
                self.active.as_str(),
                used,
                self.phase_limit(self.active)
            );
        }
        let consumed = match self.active {
            ExecutionPhase::Discovery => &mut self.discovery_calls,
            ExecutionPhase::ArtifactRepair => &mut self.artifact_repair_calls,
            ExecutionPhase::Planning => &mut self.planning_calls,
            ExecutionPhase::Implementation => &mut self.implementation_calls,
            ExecutionPhase::Repair => &mut self.repair_calls,
            ExecutionPhase::DiffReview => &mut self.diff_review_calls,
            ExecutionPhase::CompletionEvaluation => &mut self.completion_evaluation_calls,
            ExecutionPhase::Validation | ExecutionPhase::Publication => unreachable!(),
        };
        *consumed = consumed.saturating_add(1);
        Ok(self.total_calls())
    }

    /// Records a model call after the authoritative execution graph has
    /// admitted it. Legacy phase allocations remain telemetry/compatibility
    /// data and must not veto a call already authorized by node and mission
    /// budgets.
    pub(super) fn begin_graph_model_call(&mut self) -> Result<usize> {
        if !self.active.permits_model_call() {
            bail!(
                "phase `{}` does not permit model calls",
                self.active.as_str()
            );
        }
        if self.total_calls() >= self.total_limit {
            bail!("execution AI model-call budget was exhausted");
        }
        let consumed = match self.active {
            ExecutionPhase::Discovery => &mut self.discovery_calls,
            ExecutionPhase::ArtifactRepair => &mut self.artifact_repair_calls,
            ExecutionPhase::Planning => &mut self.planning_calls,
            ExecutionPhase::Implementation => &mut self.implementation_calls,
            ExecutionPhase::Repair => &mut self.repair_calls,
            ExecutionPhase::DiffReview => &mut self.diff_review_calls,
            ExecutionPhase::CompletionEvaluation => &mut self.completion_evaluation_calls,
            ExecutionPhase::Validation | ExecutionPhase::Publication => unreachable!(),
        };
        *consumed = consumed.saturating_add(1);
        Ok(self.total_calls())
    }

    pub(super) fn rollback_model_call(&mut self, phase: ExecutionPhase) -> Result<()> {
        if phase != self.active || !phase.permits_model_call() {
            bail!(
                "cannot roll back model call for inactive phase `{}`",
                phase.as_str()
            );
        }
        let consumed = match phase {
            ExecutionPhase::Discovery => &mut self.discovery_calls,
            ExecutionPhase::ArtifactRepair => &mut self.artifact_repair_calls,
            ExecutionPhase::Planning => &mut self.planning_calls,
            ExecutionPhase::Implementation => &mut self.implementation_calls,
            ExecutionPhase::Repair => &mut self.repair_calls,
            ExecutionPhase::DiffReview => &mut self.diff_review_calls,
            ExecutionPhase::CompletionEvaluation => &mut self.completion_evaluation_calls,
            ExecutionPhase::Validation | ExecutionPhase::Publication => unreachable!(),
        };
        if *consumed == 0 {
            bail!(
                "cannot roll back an unconsumed model call for phase `{}`",
                phase.as_str()
            );
        }
        *consumed -= 1;
        Ok(())
    }

    pub(super) fn telemetry(&self) -> serde_json::Value {
        serde_json::json!({
            "model_calls_used": self.total_calls(),
            "model_calls_maximum": self.total_limit,
            "model_calls_remaining": self.total_limit.saturating_sub(self.total_calls()),
            "coding_model_calls_used": self.budgeted_calls(),
            "worker_model_calls_used": self.total_calls(),
            "supplemental_artifact_repair_calls": self.artifact_repair_calls,
            "active_phase": self.active,
            "phase_allocation": self.allocation,
            "phases": {
                "discovery": {
                    "consumed": self.discovery_calls,
                    "limit": self.allocation.discovery_maximum,
                },
                "artifact_repair": {
                    "consumed": self.artifact_repair_calls,
                    "limit": 1,
                    "counts_against_configured_budget": true,
                    "counts_against_coding_allocation": false,
                },
                "planning": {
                    "consumed": self.planning_calls,
                    "limit": self.allocation.planning_maximum,
                },
                "implementation": {
                    "consumed": self.implementation_calls,
                },
                "repair": {
                    "consumed": self.repair_calls,
                },
                "implementation_repair": {
                    "consumed": self.implementation_repair_calls(),
                    "reserved": self.allocation.implementation_repair_reserved,
                    "available": self.implementation_repair_capacity(),
                    "remaining": self
                        .implementation_repair_capacity()
                        .saturating_sub(self.implementation_repair_calls()),
                },
                "diff_review": {
                    "consumed": self.diff_review_calls,
                    "reserved": self.allocation.diff_review_reserved,
                    "reallocated": self.reallocated_diff_review_calls,
                    "limit": self.phase_limit(ExecutionPhase::DiffReview),
                },
                "completion_evaluation": {
                    "consumed": self.completion_evaluation_calls,
                    "reserved": self.allocation.completion_evaluation_reserved,
                    "reallocated": self.reallocated_completion_evaluation_calls,
                    "limit": self.phase_limit(ExecutionPhase::CompletionEvaluation),
                }
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SearchSignature(String);

impl SearchSignature {
    pub(super) fn new(
        query: &str,
        path: &str,
        extensions: &[String],
        mode: &str,
        context_lines: u64,
    ) -> Self {
        let normalized_query = query.split_whitespace().collect::<Vec<_>>().join(" ");
        let normalized_path = path.trim_matches('/');
        let mut normalized_extensions = extensions
            .iter()
            .map(|extension| extension.trim_start_matches('.').to_owned())
            .collect::<Vec<_>>();
        normalized_extensions.sort();
        normalized_extensions.dedup();
        Self(format!(
            "{mode}|{normalized_path}|{}|{context_lines}|{normalized_query}",
            normalized_extensions.join(",")
        ))
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct SearchGuard {
    seen: BTreeMap<String, bool>,
    consecutive: usize,
}

impl SearchGuard {
    pub(super) fn validate(&self, signature: &SearchSignature) -> Result<()> {
        if self.consecutive >= 3 {
            bail!("search_loop_detected: more than three consecutive searches are not allowed");
        }
        if self
            .seen
            .get(&signature.0)
            .is_some_and(|truncated| !truncated)
        {
            bail!("duplicate_search_rejected: an equivalent complete search already ran");
        }
        Ok(())
    }

    pub(super) fn record(&mut self, signature: SearchSignature, truncated: bool) {
        self.seen.insert(signature.0, truncated);
        self.consecutive = self.consecutive.saturating_add(1);
    }

    pub(super) fn record_non_search(&mut self) {
        self.consecutive = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_budget_has_the_required_hard_phase_allocation() {
        let allocation = phase_budget_allocation(DEFAULT_HOSTED_MODEL_CALLS);
        assert_eq!(allocation.discovery_maximum, 5);
        assert_eq!(allocation.planning_maximum, 3);
        assert_eq!(allocation.implementation_repair_reserved, 26);
        assert_eq!(allocation.diff_review_reserved, 3);
        assert_eq!(allocation.completion_evaluation_reserved, 3);
        assert_eq!(allocation.total(), 40);
        let ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        assert_eq!(ledger.diff_review_start_call(), 35);
    }

    #[test]
    fn custom_budgets_bound_each_phase_by_actual_purpose() {
        for total in MINIMUM_HOSTED_MODEL_CALLS..=100 {
            let allocation = phase_budget_allocation(total);
            assert!(allocation.total() <= total);
            assert!(allocation.discovery_maximum <= 5);
            assert!(allocation.planning_maximum <= 3);
            assert!(allocation.implementation_repair_reserved <= total);
            assert!(allocation.diff_review_reserved <= 3);
            assert!(allocation.completion_evaluation_reserved <= 3);
        }
        let twenty = phase_budget_allocation(20);
        assert_eq!(twenty.discovery_maximum, 5);
        assert_eq!(twenty.planning_maximum, 3);
        assert_eq!(twenty.implementation_repair_reserved, 10);
        assert_eq!(twenty.diff_review_reserved, 1);
        assert_eq!(twenty.completion_evaluation_reserved, 1);
    }

    #[test]
    fn sixty_call_budget_reserves_review_and_completion_capacity() {
        let allocation = phase_budget_allocation(60);
        assert_eq!(allocation.discovery_maximum, 5);
        assert_eq!(allocation.planning_maximum, 3);
        assert_eq!(allocation.implementation_repair_reserved, 46);
        assert_eq!(allocation.diff_review_reserved, 3);
        assert_eq!(allocation.completion_evaluation_reserved, 3);
        assert_eq!(allocation.total(), 60);

        let ledger = PhaseLedger::new(60, ExecutionPhase::Discovery);
        assert_eq!(ledger.diff_review_start_call(), 55);
    }

    #[test]
    fn unused_implementation_capacity_is_reassigned_to_finalization() {
        let mut ledger = PhaseLedger::new(60, ExecutionPhase::Implementation);
        ledger.discovery_calls = 5;
        ledger.planning_calls = 3;
        ledger.implementation_calls = 8;
        let reallocated = ledger.release_unused_implementation_capacity();

        assert_eq!(reallocated.diff_review_calls, 4);
        assert_eq!(reallocated.completion_evaluation_calls, 34);
        assert_eq!(ledger.implementation_repair_capacity(), 8);
        assert_eq!(ledger.phase_limit(ExecutionPhase::DiffReview), 7);
        assert_eq!(ledger.phase_limit(ExecutionPhase::CompletionEvaluation), 37);
        assert_eq!(ledger.total_limit, 60);
    }

    #[test]
    fn larger_acceptance_sets_keep_three_evaluation_calls_and_four_review_calls() {
        let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        ledger.discovery_calls = 5;
        ledger.planning_calls = 3;
        ledger.ensure_finalization_minimum(8);

        assert_eq!(ledger.phase_limit(ExecutionPhase::DiffReview), 4);
        assert_eq!(ledger.phase_limit(ExecutionPhase::CompletionEvaluation), 3);
        assert_eq!(ledger.implementation_repair_capacity(), 25);
    }

    #[test]
    fn discovery_and_planning_cannot_borrow_implementation_capacity() {
        let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        for _ in 0..5 {
            ledger.begin_model_call().unwrap();
        }
        assert!(ledger.begin_model_call().is_err());
        ledger.transition(ExecutionPhase::Planning);
        for _ in 0..3 {
            ledger.begin_model_call().unwrap();
        }
        assert!(ledger.begin_model_call().is_err());
        assert_eq!(ledger.allocation.implementation_repair_reserved, 26);
    }

    #[test]
    fn implementation_and_repair_share_their_reserved_pool() {
        let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        for _ in 0..5 {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::Planning);
        for _ in 0..3 {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::Implementation);
        let implementation_limit = ledger.phase_limit(ExecutionPhase::Implementation);
        for _ in 0..implementation_limit {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::Repair);
        let repair_limit = ledger.phase_limit(ExecutionPhase::Repair);
        for _ in 0..repair_limit {
            ledger.begin_model_call().unwrap();
        }
        assert!(ledger.begin_model_call().is_err());
    }

    #[test]
    fn graph_admitted_call_is_not_vetoed_by_legacy_phase_allocation() {
        let mut ledger = PhaseLedger::new(25, ExecutionPhase::Implementation);
        let legacy_capacity = ledger.implementation_repair_capacity();
        for _ in 0..legacy_capacity {
            ledger.begin_model_call().unwrap();
        }
        assert!(ledger.begin_model_call().is_err());
        assert!(ledger.begin_graph_model_call().is_ok());
        assert_eq!(ledger.implementation_repair_calls(), legacy_capacity + 1);
    }

    #[test]
    fn early_implementation_failure_leaves_the_full_shared_pool_for_repair() {
        let mut ledger = PhaseLedger::new(60, ExecutionPhase::Discovery);
        for _ in 0..4 {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::Planning);
        for _ in 0..2 {
            ledger.begin_model_call().unwrap();
        }
        assert_eq!(ledger.apply_ticket_complexity(5), 25);
        assert_eq!(ledger.implementation_repair_capacity(), 13);

        ledger.transition(ExecutionPhase::Implementation);
        ledger.begin_model_call().unwrap();
        assert_eq!(ledger.implementation_repair_calls(), 1);

        ledger.transition(ExecutionPhase::Repair);
        assert_eq!(ledger.phase_limit(ExecutionPhase::Repair), 12);
        for _ in 0..12 {
            ledger.begin_model_call().unwrap();
        }

        assert_eq!(ledger.implementation_repair_calls(), 13);
        assert!(ledger.begin_model_call().is_err());
        ledger.transition(ExecutionPhase::Implementation);
        assert!(ledger.begin_model_call().is_err());
        assert_eq!(ledger.implementation_repair_calls(), 13);
    }

    #[test]
    fn unused_discovery_and_planning_calls_roll_forward_only_to_implementation() {
        let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        for _ in 0..3 {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::Planning);
        for _ in 0..2 {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::Implementation);
        assert_eq!(ledger.implementation_repair_capacity(), 26);
        let limit = ledger.phase_limit(ExecutionPhase::Implementation);
        for _ in 0..limit {
            ledger.begin_model_call().unwrap();
        }
        assert!(ledger.begin_model_call().is_err());
    }

    #[test]
    fn one_artifact_repair_call_is_accounted_without_spending_coding_capacity() {
        let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        for _ in 0..5 {
            ledger.begin_model_call().unwrap();
        }
        let implementation_capacity = ledger.implementation_repair_capacity();
        ledger.transition(ExecutionPhase::ArtifactRepair);
        assert_eq!(ledger.begin_model_call().unwrap(), 6);
        assert!(ledger.begin_model_call().is_err());
        assert_eq!(ledger.budgeted_calls(), 5);
        assert_eq!(ledger.total_calls(), 6);
        assert_eq!(
            ledger.implementation_repair_capacity(),
            implementation_capacity
        );
        assert_eq!(ledger.telemetry()["supplemental_artifact_repair_calls"], 1);
        assert_eq!(ledger.telemetry()["model_calls_used"], 6);
        assert_eq!(ledger.telemetry()["coding_model_calls_used"], 5);
        assert_eq!(ledger.telemetry()["model_calls_remaining"], 34);
        assert_eq!(
            ledger.telemetry()["phases"]["artifact_repair"]["counts_against_configured_budget"],
            true
        );
        assert_eq!(
            ledger.telemetry()["phases"]["artifact_repair"]["counts_against_coding_allocation"],
            false
        );
    }

    #[test]
    fn artifact_repair_counts_toward_the_actual_twenty_call_ceiling() {
        let mut ledger = PhaseLedger::new(20, ExecutionPhase::Discovery);
        for _ in 0..5 {
            ledger.begin_model_call().unwrap();
        }
        let coding_capacity = ledger.implementation_repair_capacity();

        ledger.transition(ExecutionPhase::ArtifactRepair);
        ledger.begin_model_call().unwrap();
        assert_eq!(ledger.implementation_repair_capacity(), coding_capacity);

        ledger.transition(ExecutionPhase::Planning);
        for _ in 0..3 {
            ledger.begin_model_call().unwrap();
        }
        assert_eq!(ledger.implementation_repair_capacity(), coding_capacity - 1);
        ledger.transition(ExecutionPhase::Implementation);
        for _ in 0..9 {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::DiffReview);
        ledger.begin_model_call().unwrap();
        ledger.transition(ExecutionPhase::CompletionEvaluation);
        ledger.begin_model_call().unwrap();

        assert_eq!(ledger.total_calls(), 20);
        assert_eq!(ledger.budgeted_calls(), 19);
        assert_eq!(ledger.telemetry()["model_calls_used"], 20);
        assert_eq!(ledger.telemetry()["coding_model_calls_used"], 19);
        assert_eq!(ledger.telemetry()["model_calls_remaining"], 0);

        ledger.transition(ExecutionPhase::Repair);
        assert!(ledger.begin_model_call().is_err());
        assert_eq!(ledger.total_calls(), 20);
    }

    #[test]
    fn failed_registration_can_restore_the_semantic_call_budget() {
        let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        assert_eq!(ledger.begin_model_call().unwrap(), 1);
        assert_eq!(ledger.budgeted_calls(), 1);

        ledger
            .rollback_model_call(ExecutionPhase::Discovery)
            .unwrap();

        assert_eq!(ledger.budgeted_calls(), 0);
        assert_eq!(ledger.total_calls(), 0);
        assert_eq!(ledger.begin_model_call().unwrap(), 1);
    }

    #[test]
    fn five_target_ticket_uses_the_small_mission_twenty_five_call_envelope() {
        let mut ledger = PhaseLedger::new(60, ExecutionPhase::Planning);
        assert_eq!(ledger.apply_ticket_complexity(5), 25);
        assert_eq!(ledger.total_limit(), 25);
        assert_eq!(ledger.implementation_repair_capacity(), 18);
        assert_eq!(ledger.phase_limit(ExecutionPhase::Implementation), 18);
        assert_eq!(ledger.phase_limit(ExecutionPhase::Repair), 18);
    }

    #[test]
    fn aops_226_call_shape_stays_in_the_small_mission_envelope() {
        let mut ledger = PhaseLedger::new(60, ExecutionPhase::Discovery);
        for _ in 0..4 {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::Planning);
        for _ in 0..2 {
            ledger.begin_model_call().unwrap();
        }
        assert_eq!(ledger.apply_ticket_complexity(5), 25);
        assert_eq!(ledger.implementation_repair_capacity(), 13);

        ledger.transition(ExecutionPhase::Implementation);
        for _ in 0..ledger.phase_limit(ExecutionPhase::Implementation) {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::Repair);
        for _ in 0..ledger.phase_limit(ExecutionPhase::Repair) {
            ledger.begin_model_call().unwrap();
        }
        assert_eq!(ledger.implementation_repair_calls(), 13);

        ledger.transition(ExecutionPhase::DiffReview);
        ledger.begin_model_call().unwrap();
        ledger.transition(ExecutionPhase::CompletionEvaluation);
        ledger.begin_model_call().unwrap();
        assert_eq!(ledger.budgeted_calls(), 21);
        assert!(ledger.budgeted_calls() <= 25);
    }

    #[test]
    fn duplicate_and_fourth_consecutive_searches_are_rejected() {
        let first = SearchSignature::new(
            "Theme Provider",
            "src/",
            &["tsx".into(), ".ts".into()],
            "literal",
            2,
        );
        let equivalent = SearchSignature::new(
            "Theme Provider",
            "src/",
            &["ts".into(), "tsx".into()],
            "literal",
            2,
        );
        let mut guard = SearchGuard::default();
        guard.validate(&first).unwrap();
        guard.record(first, false);
        assert!(guard.validate(&equivalent).is_err());

        for query in ["one", "two"] {
            let signature = SearchSignature::new(query, ".", &[], "literal", 0);
            guard.validate(&signature).unwrap();
            guard.record(signature, false);
        }
        let fourth = SearchSignature::new("four", ".", &[], "literal", 0);
        assert!(guard.validate(&fourth).is_err());
        guard.record_non_search();
        assert!(guard.validate(&fourth).is_ok());
    }
}
