use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

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
    if total == DEFAULT_HOSTED_MODEL_CALLS {
        return PhaseBudgetAllocation {
            discovery_maximum: 8,
            planning_maximum: 4,
            implementation_repair_reserved: 20,
            diff_review_reserved: 4,
            completion_evaluation_reserved: 4,
        };
    }
    if total == 0 {
        return PhaseBudgetAllocation {
            discovery_maximum: 0,
            planning_maximum: 0,
            implementation_repair_reserved: 0,
            diff_review_reserved: 0,
            completion_evaluation_reserved: 0,
        };
    }

    // Deterministic largest-remainder apportionment for 20/10/50/10/10.
    // Odd totals first round implementation upward so it never receives less
    // than half; the remaining calls go to the largest fractional shares.
    let weights = [20_usize, 10, 50, 10, 10];
    let mut buckets = weights.map(|weight| total.saturating_mul(weight) / 100);
    let remainders = weights.map(|weight| total.saturating_mul(weight) % 100);
    let implementation_minimum = total.div_ceil(2);
    if buckets[2] < implementation_minimum {
        buckets[2] = implementation_minimum;
    }
    let mut remaining = total.saturating_sub(buckets.iter().sum::<usize>());
    let mut order = [0_usize, 1, 3, 4, 2];
    order.sort_by(|left, right| {
        remainders[*right]
            .cmp(&remainders[*left])
            .then_with(|| left.cmp(right))
    });
    let mut cursor = 0usize;
    while remaining > 0 {
        buckets[order[cursor % order.len()]] =
            buckets[order[cursor % order.len()]].saturating_add(1);
        remaining -= 1;
        cursor += 1;
    }
    PhaseBudgetAllocation {
        discovery_maximum: buckets[0],
        planning_maximum: buckets[1],
        implementation_repair_reserved: buckets[2],
        diff_review_reserved: buckets[3],
        completion_evaluation_reserved: buckets[4],
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

    pub(super) const fn implementation_repair_capacity(&self) -> usize {
        self.total_limit
            .saturating_sub(self.discovery_calls)
            .saturating_sub(self.planning_calls)
            .saturating_sub(self.allocation.diff_review_reserved)
            .saturating_sub(self.allocation.completion_evaluation_reserved)
    }

    pub(super) const fn first_write_attempt_deadline(&self) -> usize {
        self.allocation
            .discovery_maximum
            .saturating_add(self.allocation.planning_maximum)
            .saturating_add(
                self.allocation
                    .implementation_repair_reserved
                    .saturating_mul(20)
                    .div_ceil(100),
            )
    }

    pub(super) const fn successful_write_deadline(&self) -> usize {
        self.allocation
            .discovery_maximum
            .saturating_add(self.allocation.planning_maximum)
            .saturating_add(
                self.allocation
                    .implementation_repair_reserved
                    .saturating_mul(40)
                    .div_ceil(100),
            )
    }

    #[cfg(test)]
    pub(super) const fn diff_review_start_call(&self) -> usize {
        self.total_limit
            .saturating_sub(self.allocation.diff_review_reserved)
            .saturating_sub(self.allocation.completion_evaluation_reserved)
            .saturating_add(1)
    }

    pub(super) const fn phase_limit(&self, phase: ExecutionPhase) -> usize {
        match phase {
            ExecutionPhase::Discovery => self.allocation.discovery_maximum,
            ExecutionPhase::ArtifactRepair => 1,
            ExecutionPhase::Planning => self.allocation.planning_maximum,
            ExecutionPhase::Implementation | ExecutionPhase::Repair => {
                self.implementation_repair_capacity()
            }
            ExecutionPhase::DiffReview => self.allocation.diff_review_reserved,
            ExecutionPhase::CompletionEvaluation => self.allocation.completion_evaluation_reserved,
            ExecutionPhase::Validation | ExecutionPhase::Publication => 0,
        }
    }

    pub(super) fn begin_model_call(&mut self) -> Result<usize> {
        if !self.active.permits_model_call() {
            bail!(
                "phase `{}` does not permit model calls",
                self.active.as_str()
            );
        }
        if self.active != ExecutionPhase::ArtifactRepair
            && self.budgeted_calls() >= self.total_limit
        {
            bail!("execution AI model-call budget was exhausted");
        }
        let used = if matches!(
            self.active,
            ExecutionPhase::Implementation | ExecutionPhase::Repair
        ) {
            self.implementation_repair_calls()
        } else {
            self.phase_calls(self.active)
        };
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

    pub(super) fn telemetry(&self) -> serde_json::Value {
        serde_json::json!({
            "model_calls_used": self.budgeted_calls(),
            "model_calls_maximum": self.total_limit,
            "model_calls_remaining": self.total_limit.saturating_sub(self.budgeted_calls()),
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
                    "counts_against_configured_budget": false,
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
                },
                "completion_evaluation": {
                    "consumed": self.completion_evaluation_calls,
                    "reserved": self.allocation.completion_evaluation_reserved,
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
        assert_eq!(allocation.discovery_maximum, 8);
        assert_eq!(allocation.planning_maximum, 4);
        assert_eq!(allocation.implementation_repair_reserved, 20);
        assert_eq!(allocation.diff_review_reserved, 4);
        assert_eq!(allocation.completion_evaluation_reserved, 4);
        assert_eq!(allocation.total(), 40);
        let ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        assert_eq!(ledger.first_write_attempt_deadline(), 16);
        assert_eq!(ledger.successful_write_deadline(), 20);
        assert_eq!(ledger.diff_review_start_call(), 33);
    }

    #[test]
    fn custom_budgets_keep_at_least_half_for_implementation_and_repair() {
        for total in 1..=64 {
            let allocation = phase_budget_allocation(total);
            assert_eq!(allocation.total(), total);
            assert!(allocation.implementation_repair_reserved >= total.div_ceil(2));
        }
        let twenty = phase_budget_allocation(20);
        assert_eq!(twenty.discovery_maximum, 4);
        assert_eq!(twenty.planning_maximum, 2);
        assert_eq!(twenty.implementation_repair_reserved, 10);
        assert_eq!(twenty.diff_review_reserved, 2);
        assert_eq!(twenty.completion_evaluation_reserved, 2);
    }

    #[test]
    fn discovery_and_planning_cannot_borrow_implementation_capacity() {
        let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        for _ in 0..8 {
            ledger.begin_model_call().unwrap();
        }
        assert!(ledger.begin_model_call().is_err());
        ledger.transition(ExecutionPhase::Planning);
        for _ in 0..4 {
            ledger.begin_model_call().unwrap();
        }
        assert!(ledger.begin_model_call().is_err());
        assert_eq!(ledger.allocation.implementation_repair_reserved, 20);
    }

    #[test]
    fn implementation_and_repair_share_their_reserved_pool() {
        let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        for _ in 0..8 {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::Planning);
        for _ in 0..4 {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::Implementation);
        for _ in 0..12 {
            ledger.begin_model_call().unwrap();
        }
        ledger.transition(ExecutionPhase::Repair);
        for _ in 0..8 {
            ledger.begin_model_call().unwrap();
        }
        assert!(ledger.begin_model_call().is_err());
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
        assert_eq!(ledger.implementation_repair_capacity(), 27);
        for _ in 0..27 {
            ledger.begin_model_call().unwrap();
        }
        assert!(ledger.begin_model_call().is_err());
    }

    #[test]
    fn one_artifact_repair_call_is_accounted_without_spending_coding_capacity() {
        let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
        for _ in 0..8 {
            ledger.begin_model_call().unwrap();
        }
        let implementation_capacity = ledger.implementation_repair_capacity();
        ledger.transition(ExecutionPhase::ArtifactRepair);
        assert_eq!(ledger.begin_model_call().unwrap(), 9);
        assert!(ledger.begin_model_call().is_err());
        assert_eq!(ledger.budgeted_calls(), 8);
        assert_eq!(ledger.total_calls(), 9);
        assert_eq!(
            ledger.implementation_repair_capacity(),
            implementation_capacity
        );
        assert_eq!(ledger.telemetry()["supplemental_artifact_repair_calls"], 1);
        assert_eq!(ledger.telemetry()["model_calls_remaining"], 32);
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
