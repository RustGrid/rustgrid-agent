impl ExecutionGraph {
    pub fn validate_invariants(&self) -> Result<(), GraphInvariantError> {
        self.validate_invariants_with_dependency_satisfaction(&BTreeSet::new())
    }

    /// Validates both graph topology and materialized node state. The extra
    /// satisfaction set is used by `ExecutionSnapshot` for evidence-backed
    /// validation and explicit partial-review dependency overrides.
    pub fn validate_invariants_with_dependency_satisfaction(
        &self,
        additionally_satisfied: &BTreeSet<ExecutionNodeId>,
    ) -> Result<(), GraphInvariantError> {
        if self.schema_version == 0 {
            return Err(GraphInvariantError::new(
                "execution graph schema version must be non-zero",
            ));
        }
        if self.graph_id.trim().is_empty() {
            return Err(GraphInvariantError::new(
                "execution graph id must not be empty",
            ));
        }

        let mut ids = BTreeSet::new();
        let active_nodes = self
            .nodes
            .iter()
            .filter(|node| node.status == ExecutionNodeStatus::Running)
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        if active_nodes.len() > 1 {
            return Err(GraphInvariantError::new(format!(
                "execution graph has multiple active owners: {active_nodes:?}"
            )));
        }
        for node in &self.nodes {
            if node.id.is_empty() {
                return Err(GraphInvariantError::new(
                    "execution node id must not be empty",
                ));
            }
            if !ids.insert(node.id.clone()) {
                return Err(GraphInvariantError::new(format!(
                    "duplicate execution node id `{}`",
                    node.id
                )));
            }
            if node.kind.is_mutation() && node.target.is_none() {
                return Err(GraphInvariantError::new(format!(
                    "mutation node `{}` has no planned target",
                    node.id
                )));
            }
            if node.kind.is_validation() && node.validation.is_none() {
                return Err(GraphInvariantError::new(format!(
                    "validation node `{}` has no gate specification",
                    node.id
                )));
            }
            let minimum_viable_node_cost = minimum_viable_node_cost(node);
            if minimum_viable_node_cost > 0
                && node.budget.max_cost_micros < minimum_viable_node_cost
            {
                return Err(GraphInvariantError::new(format!(
                    "budget_configuration_invalid: node `{}` kind={:?} max_model_calls={} max_cost_micros={} minimum_viable_node_cost={minimum_viable_node_cost}",
                    node.id, node.kind, node.budget.max_model_calls, node.budget.max_cost_micros,
                )));
            }
        }
        let collections = self.derived_collections();
        let overlapping_targets = collections
            .remaining_mutation_targets
            .intersection(&collections.applied_mutation_targets)
            .cloned()
            .collect::<BTreeSet<_>>();
        if !overlapping_targets.is_empty() {
            let offending_nodes = self
                .nodes
                .iter()
                .filter(|node| {
                    node.kind.is_mutation()
                        && node.target.as_ref().is_some_and(|target| {
                            overlapping_targets.contains(&target.mutation_target_id())
                        })
                })
                .map(|node| {
                    format!(
                        "id={} kind={:?} status={:?}",
                        node.id, node.kind, node.status
                    )
                })
                .collect::<Vec<_>>();
            return Err(GraphInvariantError::new(format!(
                "invariant=applied_mutation_target_excluded_from_remaining; offending_nodes=[{}]; remaining_mutation_target_ids={:?}; applied_mutation_target_ids={:?}",
                offending_nodes.join(", "),
                collections.remaining_mutation_targets,
                collections.applied_mutation_targets,
            )));
        }
        for node in &self.nodes {
            let mut dependencies = BTreeSet::new();
            for dependency in &node.dependencies {
                if dependency == &node.id {
                    return Err(GraphInvariantError::new(format!(
                        "node `{}` depends on itself",
                        node.id
                    )));
                }
                if !ids.contains(dependency) {
                    return Err(GraphInvariantError::new(format!(
                        "node `{}` has unknown dependency `{dependency}`",
                        node.id
                    )));
                }
                if !dependencies.insert(dependency) {
                    return Err(GraphInvariantError::new(format!(
                        "node `{}` repeats dependency `{dependency}`",
                        node.id
                    )));
                }
            }
        }
        for node_id in &self.dependency_satisfaction_overrides {
            let node = self.node(node_id).ok_or_else(|| {
                GraphInvariantError::new(format!(
                    "dependency satisfaction override refers to unknown node `{node_id}`"
                ))
            })?;
            if !node.kind.is_mutation() {
                return Err(GraphInvariantError::new(format!(
                    "dependency satisfaction override `{node_id}` is not a mutation node"
                )));
            }
        }
        for override_ in &self.dependency_overrides {
            let dependent = self.node(&override_.dependent_node).ok_or_else(|| {
                GraphInvariantError::new(format!(
                    "dependency override refers to unknown dependent node `{}`",
                    override_.dependent_node
                ))
            })?;
            self.node(&override_.unsatisfied_dependency).ok_or_else(|| {
                GraphInvariantError::new(format!(
                    "dependency override refers to unknown dependency `{}`",
                    override_.unsatisfied_dependency
                ))
            })?;
            if dependent.kind != ExecutionNodeKind::DiffReview
                || override_.allowed_outcome != MissionOutcome::PartialReviewable
                || !self.transitively_depends_on(
                    &override_.dependent_node,
                    &override_.unsatisfied_dependency,
                )
            {
                return Err(GraphInvariantError::new(format!(
                    "dependency override from `{}` to `{}` is not a draft-only diff-review override",
                    override_.dependent_node, override_.unsatisfied_dependency
                )));
            }
        }
        if self.recovery_publication_dependency_override {
            let publication = self
                .nodes
                .iter()
                .find(|node| node.kind == ExecutionNodeKind::Publication)
                .ok_or_else(|| {
                    GraphInvariantError::new(
                        "recovery publication dependency override has no publication node",
                    )
                })?;
            if !matches!(
                publication.status,
                ExecutionNodeStatus::Running | ExecutionNodeStatus::Completed
            ) {
                return Err(GraphInvariantError::new(
                    "recovery publication dependency override requires an active or completed publication node",
                ));
            }
        }
        self.validate_acyclic()?;

        let diff_nodes = self.nodes_of_kind(ExecutionNodeKind::DiffReview);
        let completion_nodes = self.nodes_of_kind(ExecutionNodeKind::CompletionEvaluation);
        let publication_nodes = self.nodes_of_kind(ExecutionNodeKind::Publication);
        if diff_nodes.len() > 1 || completion_nodes.len() > 1 || publication_nodes.len() > 1 {
            return Err(GraphInvariantError::new(
                "execution graph may contain only one review, completion, and publication node",
            ));
        }
        if let Some(completion) = completion_nodes.first() {
            let Some(diff) = diff_nodes.first() else {
                return Err(GraphInvariantError::new(
                    "completion evaluation requires a diff review node",
                ));
            };
            if !self.transitively_depends_on(&completion.id, &diff.id) {
                return Err(GraphInvariantError::new(
                    "completion evaluation must depend on diff review",
                ));
            }
        }
        if let Some(publication) = publication_nodes.first() {
            let Some(completion) = completion_nodes.first() else {
                return Err(GraphInvariantError::new(
                    "publication requires a completion evaluation node",
                ));
            };
            if !self.transitively_depends_on(&publication.id, &completion.id) {
                return Err(GraphInvariantError::new(
                    "publication must depend on completion evaluation",
                ));
            }
        }
        if let Some(diff) = diff_nodes.first() {
            for validation in self
                .nodes
                .iter()
                .filter(|node| node.required && node.kind.is_validation())
            {
                if !self.transitively_depends_on(&diff.id, &validation.id) {
                    return Err(GraphInvariantError::new(format!(
                        "diff review does not depend on required validation `{}`",
                        validation.id
                    )));
                }
            }
        }

        let satisfied = self.dependency_satisfaction_ids(additionally_satisfied);
        for node in &self.nodes {
            let requires_completed_dependencies = matches!(
                node.status,
                ExecutionNodeStatus::Running
                    | ExecutionNodeStatus::Applied
                    | ExecutionNodeStatus::Passed
                    | ExecutionNodeStatus::Superseded
                    | ExecutionNodeStatus::Completed
            );
            if requires_completed_dependencies {
                self.ensure_node_dependencies_satisfied(&node.id, &satisfied)?;
            }
        }
        Ok(())
    }

    fn dependency_satisfaction_ids(
        &self,
        additionally_satisfied: &BTreeSet<ExecutionNodeId>,
    ) -> BTreeSet<ExecutionNodeId> {
        let mut satisfied = self
            .nodes
            .iter()
            .filter(|node| node.status.satisfies_dependency())
            .map(|node| node.id.clone())
            .chain(self.dependency_satisfaction_overrides.iter().cloned())
            .chain(
                self.dependency_overrides
                    .iter()
                    .filter(|override_| {
                        override_.allowed_outcome == MissionOutcome::PartialReviewable
                    })
                    .map(|override_| override_.unsatisfied_dependency.clone()),
            )
            .chain(additionally_satisfied.iter().cloned())
            .collect::<BTreeSet<_>>();
        if self.recovery_publication_dependency_override
            && let Some(publication) = self
                .nodes
                .iter()
                .find(|node| node.kind == ExecutionNodeKind::Publication)
        {
            satisfied.extend(publication.dependencies.iter().cloned());
        }
        satisfied
    }

    fn ensure_node_dependencies_satisfied(
        &self,
        node_id: &ExecutionNodeId,
        satisfied: &BTreeSet<ExecutionNodeId>,
    ) -> Result<(), GraphInvariantError> {
        let node = self.node(node_id).ok_or_else(|| {
            GraphInvariantError::new(format!("event refers to unknown node `{node_id}`"))
        })?;
        if let Some(dependency) = node
            .dependencies
            .iter()
            .find(|dependency| !satisfied.contains(*dependency))
        {
            return Err(GraphInvariantError::new(format!(
                "node `{node_id}` cannot advance before dependency `{dependency}` succeeds"
            )));
        }
        Ok(())
    }

    fn nodes_of_kind(&self, kind: ExecutionNodeKind) -> Vec<&ExecutionNode> {
        self.nodes.iter().filter(|node| node.kind == kind).collect()
    }

    fn transitively_depends_on(
        &self,
        node_id: &ExecutionNodeId,
        expected: &ExecutionNodeId,
    ) -> bool {
        let mut pending = self
            .node(node_id)
            .map(|node| node.dependencies.clone())
            .unwrap_or_default();
        let mut visited = BTreeSet::new();
        while let Some(candidate) = pending.pop() {
            if &candidate == expected {
                return true;
            }
            if visited.insert(candidate.clone())
                && let Some(node) = self.node(&candidate)
            {
                pending.extend(node.dependencies.iter().cloned());
            }
        }
        false
    }

    fn validate_acyclic(&self) -> Result<(), GraphInvariantError> {
        fn visit(
            graph: &ExecutionGraph,
            id: &ExecutionNodeId,
            visiting: &mut BTreeSet<ExecutionNodeId>,
            visited: &mut BTreeSet<ExecutionNodeId>,
        ) -> Result<(), GraphInvariantError> {
            if visited.contains(id) {
                return Ok(());
            }
            if !visiting.insert(id.clone()) {
                return Err(GraphInvariantError::new(format!(
                    "execution graph contains a dependency cycle at `{id}`"
                )));
            }
            if let Some(node) = graph.node(id) {
                for dependency in &node.dependencies {
                    visit(graph, dependency, visiting, visited)?;
                }
            }
            visiting.remove(id);
            visited.insert(id.clone());
            Ok(())
        }

        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for node in &self.nodes {
            visit(self, &node.id, &mut visiting, &mut visited)?;
        }
        Ok(())
    }
}

fn minimum_viable_node_cost(node: &ExecutionNode) -> u64 {
    match node.kind {
        ExecutionNodeKind::Discovery => {
            let calls = u64::from(node.budget.max_model_calls);
            match calls {
                0 => 0,
                // One normal bounded inspection plus 60k for every additional
                // compact inspection/finalization or repair profile.
                _ => 100_000_u64.saturating_add(calls.saturating_sub(1) * 60_000),
            }
        }
        // One bounded 4,096-token BuildPlan request plus one compact repair
        // request must fit the bootstrap envelope without pricing either as a
        // generic 16,384-token agent turn.
        ExecutionNodeKind::Planning => {
            u64::from(node.budget.max_model_calls).saturating_mul(130_000)
        }
        _ => 0,
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct DerivedExecutionCollections {
    pub remaining_graph_nodes: BTreeSet<ExecutionNodeId>,
    pub remaining_mutation_targets: BTreeSet<MutationTargetId>,
    pub applied_mutation_targets: BTreeSet<MutationTargetId>,
    pub completed_validation_nodes: BTreeSet<ValidationNodeId>,
}
