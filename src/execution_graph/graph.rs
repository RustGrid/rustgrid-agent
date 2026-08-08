#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExecutionGraph {
    pub schema_version: u16,
    pub graph_id: String,
    pub complexity: MissionComplexity,
    #[serde(default)]
    pub complexity_classification_stage: ComplexityClassificationStage,
    pub(crate) nodes: Vec<ExecutionNode>,
    pub created_from_repository_fingerprint: String,
    /// Monotonically increases whenever authoritative graph state changes.
    #[serde(default)]
    pub revision: u64,
    /// Nodes whose incomplete status remains visible as remaining work, but
    /// whose dependency edge is explicitly satisfied for a reviewable partial
    /// path. Only an authoritative `PartialReviewable` guardrail event may add
    /// entries here.
    #[serde(default)]
    pub(crate) dependency_satisfaction_overrides: BTreeSet<ExecutionNodeId>,
    /// Typed, auditable exceptions that allow review and draft publication
    /// without fabricating validation success. Only `PartialReviewable` may be
    /// authorized by these overrides.
    #[serde(default)]
    pub(crate) dependency_overrides: Vec<DependencyOverride>,
    /// An explicit draft-recovery publication may satisfy only the publication
    /// node's direct dependency without fabricating review or completion
    /// success. The authorizing domain event is the sole writer.
    #[serde(default)]
    pub(crate) recovery_publication_dependency_override: bool,
}

impl Default for ExecutionGraph {
    fn default() -> Self {
        Self {
            schema_version: EXECUTION_GRAPH_SCHEMA_VERSION,
            graph_id: String::new(),
            complexity: MissionComplexity::Tiny,
            complexity_classification_stage: ComplexityClassificationStage::Authoritative,
            nodes: Vec::new(),
            created_from_repository_fingerprint: String::new(),
            revision: 0,
            dependency_satisfaction_overrides: BTreeSet::new(),
            dependency_overrides: Vec::new(),
            recovery_publication_dependency_override: false,
        }
    }
}

impl ExecutionGraph {
    pub fn bootstrap(
        graph_id: impl Into<String>,
        repository_fingerprint: impl Into<String>,
        complexity: MissionComplexity,
        mission_budget: &MissionBudget,
    ) -> Self {
        let mut graph = Self {
            graph_id: graph_id.into(),
            complexity,
            complexity_classification_stage: ComplexityClassificationStage::Provisional,
            created_from_repository_fingerprint: repository_fingerprint.into(),
            ..Self::default()
        };
        graph.nodes = vec![
            ExecutionNode {
                id: ExecutionNodeId::new("discovery"),
                kind: ExecutionNodeKind::Discovery,
                status: ExecutionNodeStatus::Ready,
                required: true,
                ..ExecutionNode::default()
            },
            ExecutionNode {
                id: ExecutionNodeId::new("planning"),
                kind: ExecutionNodeKind::Planning,
                dependencies: vec![ExecutionNodeId::new("discovery")],
                required: true,
                ..ExecutionNode::default()
            },
        ];
        assign_bootstrap_node_budgets(&mut graph.nodes, mission_budget);
        graph
    }

    pub fn from_accepted_plan(
        graph_id: impl Into<String>,
        complexity: MissionComplexity,
        repository_fingerprint: impl Into<String>,
        plan: &AcceptedPlan,
        mission_budget: &MissionBudget,
    ) -> Self {
        build_execution_graph(
            graph_id,
            complexity,
            repository_fingerprint,
            &plan.targets,
            &plan.validation_gates,
            mission_budget,
        )
    }

    pub fn from_targets(
        graph_id: impl Into<String>,
        complexity: MissionComplexity,
        repository_fingerprint: impl Into<String>,
        targets: &[PlannedTarget],
        validation_gates: &[ValidationGateSpec],
        mission_budget: &MissionBudget,
    ) -> Self {
        build_execution_graph(
            graph_id,
            complexity,
            repository_fingerprint,
            targets,
            validation_gates,
            mission_budget,
        )
    }

    pub fn node(&self, id: &ExecutionNodeId) -> Option<&ExecutionNode> {
        self.nodes.iter().find(|node| &node.id == id)
    }

    /// Read-only traversal in stable persisted graph order.
    pub fn nodes(&self) -> impl ExactSizeIterator<Item = &ExecutionNode> {
        self.nodes.iter()
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn node_mut(&mut self, id: &ExecutionNodeId) -> Option<&mut ExecutionNode> {
        self.nodes.iter_mut().find(|node| &node.id == id)
    }

    pub fn node_by_str(&self, id: &str) -> Option<&ExecutionNode> {
        self.nodes.iter().find(|node| node.id.as_str() == id)
    }

    pub fn unique_mutation_node_for_target_path(&self, path: &str) -> Option<&ExecutionNode> {
        let mut matches = self.nodes.iter().filter(|node| {
            node.kind.is_mutation()
                && node
                    .target
                    .as_ref()
                    .is_some_and(|target| target.path == path)
        });
        let node = matches.next()?;
        matches.next().is_none().then_some(node)
    }

    pub fn active_node(&self) -> Option<&ExecutionNode> {
        self.nodes.iter().find(|node| {
            node.status == ExecutionNodeStatus::Running
                && !self.dependency_satisfaction_overrides.contains(&node.id)
        })
    }

    /// Selects deterministically by persisted graph order. A running node owns
    /// execution; otherwise a recoverable failure is repaired before new work;
    /// otherwise the first ready node runs.
    pub fn next_runnable_node(&self) -> Option<&ExecutionNode> {
        self.active_node()
            .or_else(|| {
                self.nodes.iter().find(|node| {
                    node.status == ExecutionNodeStatus::FailedRecoverable
                        && !self.dependency_satisfaction_overrides.contains(&node.id)
                })
            })
            .or_else(|| {
                self.nodes.iter().find(|node| {
                    node.status == ExecutionNodeStatus::Ready
                        && !self.dependency_satisfaction_overrides.contains(&node.id)
                })
            })
    }

    pub fn ready_nodes(&self) -> impl Iterator<Item = &ExecutionNode> {
        self.nodes.iter().filter(|node| {
            node.status == ExecutionNodeStatus::Ready
                && !self.dependency_satisfaction_overrides.contains(&node.id)
        })
    }

    pub fn remaining_required_nodes(&self) -> Vec<&ExecutionNode> {
        self.nodes
            .iter()
            .filter(|node| node.required && !node.status.is_success())
            .collect()
    }

    pub fn derived_collections(&self) -> DerivedExecutionCollections {
        let remaining_graph_nodes = self
            .nodes
            .iter()
            .filter(|node| node.required && !node.status.is_success())
            .map(|node| node.id.clone())
            .collect();
        let remaining_mutation_targets = self
            .nodes
            .iter()
            .filter(|node| {
                node.kind.is_mutation()
                    && !matches!(
                        node.status,
                        ExecutionNodeStatus::Applied | ExecutionNodeStatus::Completed
                    )
            })
            .filter_map(|node| node.target.as_ref().map(PlannedTarget::mutation_target_id))
            .collect();
        let applied_mutation_targets = self
            .nodes
            .iter()
            .filter(|node| {
                node.kind.is_mutation()
                    && matches!(
                        node.status,
                        ExecutionNodeStatus::Applied | ExecutionNodeStatus::Completed
                    )
            })
            .filter_map(|node| node.target.as_ref().map(PlannedTarget::mutation_target_id))
            .collect();
        let completed_validation_nodes = self
            .nodes
            .iter()
            .filter(|node| {
                node.kind.is_validation()
                    && matches!(
                        node.status,
                        ExecutionNodeStatus::Passed | ExecutionNodeStatus::Completed
                    )
            })
            .map(|node| ValidationNodeId::new(node.id.as_str()))
            .collect();
        DerivedExecutionCollections {
            remaining_graph_nodes,
            remaining_mutation_targets,
            applied_mutation_targets,
            completed_validation_nodes,
        }
    }

    pub fn all_required_nodes_succeeded(&self) -> bool {
        self.nodes
            .iter()
            .filter(|node| node.required)
            .all(|node| node.status.is_success())
    }

    /// Hard boundary between implementation and validation. Partial-review
    /// dependency overrides never satisfy this proof.
    pub fn implementation_barrier_satisfied(&self) -> bool {
        self.nodes
            .iter()
            .filter(|node| node.required && node.kind.is_mutation())
            .all(|node| {
                matches!(
                    node.status,
                    ExecutionNodeStatus::Completed | ExecutionNodeStatus::Skipped
                )
            })
    }

    pub fn validation_readiness_proof(
        &self,
    ) -> Result<ValidationReadinessProof, GraphInvariantError> {
        if !self.implementation_barrier_satisfied() {
            let incomplete = self
                .nodes
                .iter()
                .filter(|node| {
                    node.required
                        && node.kind.is_mutation()
                        && !matches!(
                            node.status,
                            ExecutionNodeStatus::Completed | ExecutionNodeStatus::Skipped
                        )
                })
                .map(|node| format!("{}:{:?}", node.id, node.status))
                .collect::<Vec<_>>();
            return Err(GraphInvariantError::new(format!(
                "implementation_barrier_unsatisfied: required implementation nodes remain [{}]",
                incomplete.join(", ")
            )));
        }
        Ok(ValidationReadinessProof {
            graph_revision: self.revision,
            satisfied_implementation_nodes: self
                .nodes
                .iter()
                .filter(|node| node.required && node.kind.is_mutation())
                .map(|node| node.id.clone())
                .collect(),
        })
    }

    pub fn has_blocking_required_node(&self) -> bool {
        self.nodes
            .iter()
            .any(|node| node.required && node.status == ExecutionNodeStatus::FailedBlocking)
    }

    pub fn stage(&self) -> HostedExecutionStage {
        if let Some(node) = self.next_runnable_node() {
            return node.kind.stage();
        }
        if let Some(node) = self
            .nodes
            .iter()
            .find(|node| node.required && !node.status.is_success())
        {
            return if node.status == ExecutionNodeStatus::FailedBlocking {
                HostedExecutionStage::Terminal
            } else {
                node.kind.stage()
            };
        }
        HostedExecutionStage::Terminal
    }

    pub fn set_node_status(
        &mut self,
        id: &ExecutionNodeId,
        status: ExecutionNodeStatus,
    ) -> Result<(), GraphInvariantError> {
        self.set_node_status_if_changed(id, status)?;
        Ok(())
    }

    pub fn set_node_status_if_changed(
        &mut self,
        id: &ExecutionNodeId,
        status: ExecutionNodeStatus,
    ) -> Result<GraphMutationResult, GraphInvariantError> {
        let previous_revision = self.revision;
        let node = self
            .node_mut(id)
            .ok_or_else(|| GraphInvariantError::new(format!("unknown execution node `{id}`")))?;
        if node.status == status {
            return Ok(GraphMutationResult::NoChange { current_revision: previous_revision });
        }
        node.status = status;
        if status.is_success() {
            self.dependency_satisfaction_overrides.remove(id);
        }
        self.refresh_readiness_without_revision();
        self.revision = previous_revision.saturating_add(1);
        Ok(GraphMutationResult::Changed { new_revision: self.revision })
    }

    /// Applies a normal forward node-state transition atomically. Recovery and
    /// topology reconciliation deliberately use their dedicated event paths.
    pub fn transition_node(
        &mut self,
        id: &ExecutionNodeId,
        status: ExecutionNodeStatus,
    ) -> Result<TransitionOutcome, GraphTransitionError> {
        let current = self
            .node(id)
            .map(|node| node.status)
            .ok_or_else(|| GraphTransitionError::UnknownNode {
                node_id: id.clone(),
            })?;
        if current == status {
            return Ok(TransitionOutcome::IdempotentReplay);
        }
        if status == ExecutionNodeStatus::Running
            && let Some(active) = self.active_node()
            && active.id != *id
        {
            return Err(GraphTransitionError::ActiveOwnerConflict {
                active_node_id: active.id.clone(),
                requested_node_id: id.clone(),
            });
        }
        if !legal_forward_transition(current, status) {
            return Err(GraphTransitionError::IllegalNodeTransition {
                node_id: id.clone(),
                from: current,
                to: status,
            });
        }

        let mut next = self.clone();
        next.set_node_status(id, status)?;
        next.validate_invariants()?;
        *self = next;
        Ok(TransitionOutcome::Applied)
    }

    /// Materializes readiness from dependency state. It never changes active,
    /// completed, or failed nodes.
    pub fn refresh_readiness(&mut self) {
        if self.refresh_readiness_without_revision() {
            self.revision = self.revision.saturating_add(1);
        }
    }

    fn refresh_readiness_without_revision(&mut self) -> bool {
        let successful = self
            .nodes
            .iter()
            .filter(|node| node.status.satisfies_dependency())
            .map(|node| node.id.clone())
            .chain(self.dependency_satisfaction_overrides.iter().cloned())
            .collect::<BTreeSet<_>>();
        let implementation_barrier_satisfied = self.implementation_barrier_satisfied();
        let mut changed = false;
        for node in &mut self.nodes {
            let dependencies_satisfied = (!node.kind.is_validation()
                || implementation_barrier_satisfied)
                && node
                .dependencies
                .iter()
                .all(|dependency| successful.contains(dependency));
            match (node.status, dependencies_satisfied) {
                (ExecutionNodeStatus::Pending, true) => {
                    node.status = ExecutionNodeStatus::Ready;
                    changed = true;
                }
                (ExecutionNodeStatus::Ready, false) => {
                    node.status = ExecutionNodeStatus::Pending;
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }

    pub fn apply_already_applied_transition(
        &mut self,
        execution_id: &str,
        attempt: u32,
        transition: &AlreadyAppliedTransition,
    ) -> Result<GraphMutationResult, TransitionError> {
        let semantic_id = transition.semantic_id(execution_id, attempt);
        reduce_repository_operation(
            self,
            transition.node_id.clone(),
            OperationIntent {
                operation: transition.operation.clone(),
                target_path: transition.target_path.clone(),
                expected_result_hash: transition.expected_result_hash.clone(),
                satisfied_intent: SatisfiedIntent::OriginalImplementation,
            },
            RepositoryOperationResult::Verified {
                outcome: RepositoryOperationOutcome::AlreadyApplied,
                evidence: SuccessfulOperationEvidence::AlreadyApplied {
                    observed: transition.repository_fingerprint.clone(),
                },
                observed_result_hash: transition.observed_result_hash.clone(),
                semantic_id,
                attempt,
                completed_at: transition.completed_at.clone(),
            },
        )
    }

    pub fn validation_node_for_fingerprint(
        &self,
        fingerprint: &str,
        repository_fingerprint: &str,
    ) -> Option<&ExecutionNode> {
        self.nodes.iter().find(|node| {
            node.validation
                .as_ref()
                .is_some_and(|gate| gate.fingerprint(repository_fingerprint) == fingerprint)
        })
    }
}

const fn legal_forward_transition(
    from: ExecutionNodeStatus,
    to: ExecutionNodeStatus,
) -> bool {
    use ExecutionNodeStatus as Status;

    matches!(
        (from, to),
        (Status::Pending, Status::Ready)
            | (Status::Ready | Status::FailedRecoverable, Status::Running)
            | (
                Status::Running,
                Status::Applied
                    | Status::Passed
                    | Status::FailedRecoverable
                    | Status::FailedBlocking
                    | Status::Superseded
                    | Status::Skipped
                    | Status::Completed
            )
            | (
                Status::FailedRecoverable,
                Status::Pending | Status::FailedBlocking | Status::Superseded
            )
            | (
                Status::Applied | Status::Passed | Status::Superseded | Status::Skipped,
                Status::Completed
            )
    )
}
