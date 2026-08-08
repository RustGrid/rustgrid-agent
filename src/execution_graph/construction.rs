pub fn build_execution_graph(
    graph_id: impl Into<String>,
    complexity: MissionComplexity,
    repository_fingerprint: impl Into<String>,
    targets: &[PlannedTarget],
    validation_gates: &[ValidationGateSpec],
    mission_budget: &MissionBudget,
) -> ExecutionGraph {
    let mut nodes = Vec::new();
    let discovery_id = ExecutionNodeId::new("discovery");
    let planning_id = ExecutionNodeId::new("planning");
    nodes.push(ExecutionNode {
        id: discovery_id.clone(),
        kind: ExecutionNodeKind::Discovery,
        status: ExecutionNodeStatus::Completed,
        required: true,
        ..ExecutionNode::default()
    });
    nodes.push(ExecutionNode {
        id: planning_id.clone(),
        kind: ExecutionNodeKind::Planning,
        dependencies: vec![discovery_id],
        status: ExecutionNodeStatus::Completed,
        required: true,
        ..ExecutionNode::default()
    });

    let mut ordered_targets = targets.iter().enumerate().collect::<Vec<_>>();
    ordered_targets.sort_by_key(|(index, target)| (target.is_test_target(), *index));
    let mut previous_mutation = planning_id;
    let mut mutation_ids = Vec::new();
    for (original_index, target) in ordered_targets {
        let kind = if target.is_test_target() {
            ExecutionNodeKind::TestMutation
        } else {
            ExecutionNodeKind::SourceMutation
        };
        let id = stable_node_id(kind, &target.path, original_index);
        nodes.push(ExecutionNode {
            id: id.clone(),
            kind,
            dependencies: vec![previous_mutation.clone()],
            status: ExecutionNodeStatus::Pending,
            required: true,
            target: Some(target.clone()),
            ..ExecutionNode::default()
        });
        mutation_ids.push(id.clone());
        previous_mutation = id;
    }

    let validation_base_dependencies = mutation_ids
        .last()
        .cloned()
        .map_or_else(|| vec![previous_mutation], |id| vec![id]);
    let mut ordered_validation_gates = validation_gates.to_vec();
    normalize_validation_gate_order(&mut ordered_validation_gates);
    let mut validation_dependencies = validation_base_dependencies.clone();
    for (index, gate) in ordered_validation_gates.iter().enumerate() {
        let kind = gate.node_kind();
        let id = stable_node_id(kind, &gate.gate_id, index);
        let required = gate.required;
        nodes.push(ExecutionNode {
            id: id.clone(),
            kind,
            dependencies: if required {
                validation_dependencies.clone()
            } else {
                validation_base_dependencies.clone()
            },
            status: if required {
                ExecutionNodeStatus::Pending
            } else {
                ExecutionNodeStatus::Skipped
            },
            required,
            validation: Some(gate.clone()),
            ..ExecutionNode::default()
        });
        if required {
            validation_dependencies = vec![id];
        }
    }

    let diff_dependencies = validation_dependencies;
    let diff_id = ExecutionNodeId::new("diff-review");
    let completion_id = ExecutionNodeId::new("completion-evaluation");
    let publication_id = ExecutionNodeId::new("publication");
    nodes.push(ExecutionNode {
        id: diff_id.clone(),
        kind: ExecutionNodeKind::DiffReview,
        dependencies: diff_dependencies,
        status: ExecutionNodeStatus::Pending,
        required: true,
        ..ExecutionNode::default()
    });
    nodes.push(ExecutionNode {
        id: completion_id.clone(),
        kind: ExecutionNodeKind::CompletionEvaluation,
        dependencies: vec![diff_id],
        status: ExecutionNodeStatus::Pending,
        required: true,
        ..ExecutionNode::default()
    });
    nodes.push(ExecutionNode {
        id: publication_id,
        kind: ExecutionNodeKind::Publication,
        dependencies: vec![completion_id],
        status: ExecutionNodeStatus::Pending,
        required: true,
        ..ExecutionNode::default()
    });

    assign_node_budgets(&mut nodes, mission_budget);
    let mut graph = ExecutionGraph {
        schema_version: EXECUTION_GRAPH_SCHEMA_VERSION,
        graph_id: graph_id.into(),
        complexity,
        complexity_classification_stage: ComplexityClassificationStage::Authoritative,
        nodes,
        created_from_repository_fingerprint: repository_fingerprint.into(),
        revision: 1,
        dependency_satisfaction_overrides: BTreeSet::new(),
        dependency_overrides: Vec::new(),
        recovery_publication_dependency_override: false,
    };
    graph.refresh_readiness();
    graph
}

fn stable_node_id(kind: ExecutionNodeKind, label: &str, index: usize) -> ExecutionNodeId {
    let prefix = match kind {
        ExecutionNodeKind::SourceMutation => "source",
        ExecutionNodeKind::TestMutation => "test",
        ExecutionNodeKind::ValidationRepair => "validation-repair",
        ExecutionNodeKind::ValidationRepairSession => "validation-repair-session",
        ExecutionNodeKind::ValidationFocused => "validation-focused",
        ExecutionNodeKind::ValidationSuite => "validation-suite",
        ExecutionNodeKind::ValidationBuild => "validation-build",
        ExecutionNodeKind::ValidationLint => "validation-lint",
        ExecutionNodeKind::Discovery => "discovery",
        ExecutionNodeKind::Planning => "planning",
        ExecutionNodeKind::DiffReview => "diff-review",
        ExecutionNodeKind::CompletionEvaluation => "completion-evaluation",
        ExecutionNodeKind::Publication => "publication",
    };
    let digest = stable_hash(&format!("{prefix}\0{index}\0{label}"));
    ExecutionNodeId::new(format!("{prefix}-{index:03}-{}", &digest[..12]))
}

fn stable_hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn assign_node_budgets(nodes: &mut [ExecutionNode], mission: &MissionBudget) {
    let mut groups = BTreeMap::<BudgetGroup, Vec<usize>>::new();
    for (index, node) in nodes.iter().enumerate() {
        groups
            .entry(BudgetGroup::for_kind(node.kind))
            .or_default()
            .push(index);
    }

    // Review and completion are assigned before mutation work and therefore
    // cannot be consumed by implementation nodes. Publication and validation
    // are also independently bounded even though they normally make no model call.
    let call_percentages = [
        (BudgetGroup::Discovery, 12_u64),
        (BudgetGroup::Planning, 8),
        (BudgetGroup::Mutation, 62),
        (BudgetGroup::Validation, 0),
        (BudgetGroup::Review, 8),
        (BudgetGroup::Completion, 10),
        (BudgetGroup::Publication, 0),
    ];
    let cost_percentages = [
        (BudgetGroup::Discovery, 8_u64),
        (BudgetGroup::Planning, 8),
        (BudgetGroup::Mutation, 52),
        (BudgetGroup::Validation, 15),
        (BudgetGroup::Review, 6),
        (BudgetGroup::Completion, 6),
        (BudgetGroup::Publication, 5),
    ];
    let duration_percentages = [
        (BudgetGroup::Discovery, 10_u64),
        (BudgetGroup::Planning, 8),
        (BudgetGroup::Mutation, 47),
        (BudgetGroup::Validation, 25),
        (BudgetGroup::Review, 4),
        (BudgetGroup::Completion, 3),
        (BudgetGroup::Publication, 3),
    ];

    for (group, indices) in &groups {
        let call_total = percentage_share(
            u64::from(mission.max_model_calls),
            percentage_for(&call_percentages, *group),
        ) as u32;
        let cost_total = percentage_share(
            mission.max_cost_micros,
            percentage_for(&cost_percentages, *group),
        );
        let duration_total = percentage_share(
            u64::try_from(mission.max_duration.as_millis()).unwrap_or(u64::MAX),
            percentage_for(&duration_percentages, *group),
        );
        let count = indices.len();
        for (position, index) in indices.iter().copied().enumerate() {
            let node = &mut nodes[index];
            let distributed_calls = distribute_u32(call_total, count, position);
            let max_model_calls = if node.kind.is_mutation() {
                distributed_calls.clamp(1, 2)
            } else if node.kind.is_validation() && mission.max_target_repair_rounds > 0 {
                // Validation execution itself is deterministic, but a
                // failed gate owns its bounded diagnosis/repair call. Do
                // not charge that call back to an already-applied target.
                distributed_calls.max(1)
            } else {
                distributed_calls
            };
            let max_mutation_fallback_attempts = if node.kind.is_mutation() && max_model_calls >= 2 {
                mission.max_target_repair_rounds.min(1)
            } else {
                // A nominal repair allowance is unsafe when the node cannot
                // afford a distinct call after its primary mutation.
                0
            };
            node.budget = NodeBudget {
                max_model_calls,
                max_cost_micros: distribute_u64(cost_total, count, position),
                max_duration: Duration::from_millis(distribute_u64(
                    duration_total,
                    count,
                    position,
                )),
                max_mutation_fallback_attempts,
            };
        }
    }
}

fn assign_bootstrap_node_budgets(nodes: &mut [ExecutionNode], mission: &MissionBudget) {
    let discovery_calls = mission.max_model_calls.min(3);
    let planning_calls = mission
        .max_model_calls
        .saturating_sub(discovery_calls)
        .min(2);
    let discovery_cost = mission.max_cost_micros.min(350_000);
    let planning_cost = mission
        .max_cost_micros
        .saturating_sub(discovery_cost)
        .min(300_000);
    let discovery_duration = mission.max_duration.min(Duration::from_millis(120_000));
    let planning_duration = mission
        .max_duration
        .saturating_sub(discovery_duration)
        .min(Duration::from_millis(90_000));

    for node in nodes {
        node.budget = match node.kind {
            ExecutionNodeKind::Discovery => NodeBudget {
                max_model_calls: discovery_calls,
                max_cost_micros: discovery_cost,
                max_duration: discovery_duration,
                max_mutation_fallback_attempts: 0,
            },
            ExecutionNodeKind::Planning => NodeBudget {
                max_model_calls: planning_calls,
                max_cost_micros: planning_cost,
                max_duration: planning_duration,
                // The second bootstrap call is reserved for one bounded
                // implementation-plan repair after an invalid first artifact.
                max_mutation_fallback_attempts: 1,
            },
            _ => NodeBudget::default(),
        };
    }
}

#[derive(Clone, Copy, Debug, Ord, PartialEq, Eq, PartialOrd)]
enum BudgetGroup {
    Discovery,
    Planning,
    Mutation,
    Validation,
    Review,
    Completion,
    Publication,
}

impl BudgetGroup {
    const fn for_kind(kind: ExecutionNodeKind) -> Self {
        match kind {
            ExecutionNodeKind::Discovery => Self::Discovery,
            ExecutionNodeKind::Planning => Self::Planning,
            ExecutionNodeKind::SourceMutation
            | ExecutionNodeKind::TestMutation
            | ExecutionNodeKind::ValidationRepair
            | ExecutionNodeKind::ValidationRepairSession => Self::Mutation,
            ExecutionNodeKind::ValidationFocused
            | ExecutionNodeKind::ValidationSuite
            | ExecutionNodeKind::ValidationBuild
            | ExecutionNodeKind::ValidationLint => Self::Validation,
            ExecutionNodeKind::DiffReview => Self::Review,
            ExecutionNodeKind::CompletionEvaluation => Self::Completion,
            ExecutionNodeKind::Publication => Self::Publication,
        }
    }
}

fn percentage_for(values: &[(BudgetGroup, u64)], group: BudgetGroup) -> u64 {
    values
        .iter()
        .find_map(|(candidate, value)| (*candidate == group).then_some(*value))
        .unwrap_or(0)
}

fn percentage_share(total: u64, percent: u64) -> u64 {
    total.saturating_mul(percent) / 100
}

fn distribute_u64(total: u64, count: usize, position: usize) -> u64 {
    if count == 0 {
        return 0;
    }
    let count = u64::try_from(count).unwrap_or(u64::MAX);
    let position = u64::try_from(position).unwrap_or(u64::MAX);
    total / count + u64::from(position < total % count)
}

fn distribute_u32(total: u32, count: usize, position: usize) -> u32 {
    u32::try_from(distribute_u64(u64::from(total), count, position)).unwrap_or(u32::MAX)
}
