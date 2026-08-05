// Extracted from the hosted execution composition root.
use super::*;

pub(super) fn execution_decision_idempotency_key(
    snapshot: &crate::execution_graph::ExecutionSnapshot,
    decision: &ExecutionDecision,
) -> String {
    let node = decision
        .node_id()
        .and_then(|node_id| snapshot.graph.node(node_id))
        .or_else(|| snapshot.graph.active_node())
        .or_else(|| snapshot.graph.next_runnable_node());
    let node_id = node.map_or("none", |node| node.id.as_str());
    let node_attempt = node.map_or(0, |node| {
        u32::try_from(node.attempts.len()).unwrap_or(u32::MAX)
    });
    format!(
        "{}:{}:{}:{}",
        snapshot.graph.revision,
        node_id,
        node_attempt,
        execution_decision_action_kind(decision),
    )
}

pub(super) fn orchestration_decision_is_new(
    last_applied_key: Option<&str>,
    candidate_key: &str,
) -> bool {
    last_applied_key != Some(candidate_key)
}

pub(super) const fn execution_decision_action_kind(decision: &ExecutionDecision) -> &'static str {
    match decision {
        ExecutionDecision::ContinueDiscovery {
            action: crate::hosted_orchestrator::DiscoveryAction::InspectRepository { .. },
        } => "inspect_repository",
        ExecutionDecision::ContinueDiscovery {
            action: crate::hosted_orchestrator::DiscoveryAction::FinalizeImpactMap { .. },
        } => "finalize_impact_map",
        ExecutionDecision::ContinueDiscovery {
            action: crate::hosted_orchestrator::DiscoveryAction::RepairImpactMap { .. },
        } => "repair_impact_map",
        ExecutionDecision::ContinuePlanning {
            action: crate::hosted_orchestrator::PlanningAction::BuildPlan { .. },
        } => "build_plan",
        ExecutionDecision::ContinuePlanning {
            action: crate::hosted_orchestrator::PlanningAction::RepairPlan { .. },
        } => "repair_plan",
        ExecutionDecision::ContinuePlanning {
            action: crate::hosted_orchestrator::PlanningAction::ResolveEvidenceGap { .. },
        } => "resolve_evidence_gap",
        _ => execution_decision_name(decision),
    }
}

pub(super) fn phase_permits_tool(phase: ExecutionPhase, name: &str) -> bool {
    match phase {
        ExecutionPhase::Discovery => matches!(
            name,
            "list_files"
                | "read_file"
                | "read_files"
                | "search_text"
                | "related_tests"
                | "record_impact_map"
        ),
        ExecutionPhase::ArtifactRepair => name == "record_impact_map",
        ExecutionPhase::Planning => matches!(
            name,
            "read_file"
                | "read_files"
                | "search_text"
                | "related_tests"
                | "record_implementation_plan"
        ),
        ExecutionPhase::Implementation | ExecutionPhase::Repair => matches!(
            name,
            "read_file"
                | "read_files"
                | "search_text"
                | "related_tests"
                | "write_file"
                | "replace_text"
                | "replace_range"
                | "insert_after_symbol"
                | "insert_before_symbol"
                | "apply_patch"
                | "replace_file"
                | "create_file"
                | "rename_file"
                | "move_file"
                | "record_no_valid_repair"
                | "record_repair_intent_satisfied"
                | "apply_unified_diff"
                | "rewrite_small_file"
                | "delete_file"
                | "report_write_progress"
        ),
        ExecutionPhase::DiffReview => matches!(
            name,
            "read_file"
                | "read_files"
                | "search_text"
                | "related_tests"
                | "repository_snapshot"
                | "declare_implementation"
        ),
        ExecutionPhase::CompletionEvaluation
        | ExecutionPhase::Validation
        | ExecutionPhase::Publication => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PhaseDecision {
    Stay,
    Transition(ExecutionPhase),
}

#[derive(Clone, Debug)]
pub(super) struct DecisionExecutionResult {
    pub(super) decision: ExecutionDecision,
    pub(super) phase_decision: PhaseDecision,
    pub(super) persistence_error: Option<PhasePersistenceFailure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PhasePersistenceFailureKind {
    Contract,
    Persistence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct PhasePersistenceFailure {
    pub(super) kind: PhasePersistenceFailureKind,
    pub(super) from_phase: ExecutionPhase,
    pub(super) phase: ExecutionPhase,
    pub(super) safe_error: String,
}

impl PhasePersistenceFailure {
    pub(super) const fn category(&self) -> &'static str {
        match self.kind {
            PhasePersistenceFailureKind::Contract => "OrchestrationContractFailure",
            PhasePersistenceFailureKind::Persistence => "OrchestrationPersistenceFailure",
        }
    }

    pub(super) const fn code(&self) -> &'static str {
        match self.kind {
            PhasePersistenceFailureKind::Contract => "phase_transition_event_invalid",
            PhasePersistenceFailureKind::Persistence => "phase_transition_persistence_failed",
        }
    }

    pub(super) const fn process_health(&self) -> &'static str {
        match self.kind {
            PhasePersistenceFailureKind::Contract => "failed",
            PhasePersistenceFailureKind::Persistence => "degraded",
        }
    }
}

impl std::fmt::Display for PhasePersistenceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} while persisting `{}` to `{}`: {}",
            self.code(),
            self.from_phase.as_str(),
            self.phase.as_str(),
            self.safe_error
        )
    }
}

impl std::error::Error for PhasePersistenceFailure {}

pub(super) const fn execution_decision_name(decision: &ExecutionDecision) -> &'static str {
    match decision {
        ExecutionDecision::ContinueDiscovery { .. } => "continue_discovery",
        ExecutionDecision::ContinuePlanning {
            action: crate::hosted_orchestrator::PlanningAction::BuildPlan { .. },
        } => "build_plan",
        ExecutionDecision::ContinuePlanning {
            action: crate::hosted_orchestrator::PlanningAction::RepairPlan { .. },
        } => "repair_plan",
        ExecutionDecision::ContinuePlanning {
            action: crate::hosted_orchestrator::PlanningAction::ResolveEvidenceGap { .. },
        } => "resolve_evidence_gap",
        ExecutionDecision::ExecuteTarget {
            action: crate::hosted_orchestrator::MutationAction::PrepareTargetContext { .. },
            ..
        } => "prepare_target_context",
        ExecutionDecision::ExecuteTarget {
            action: crate::hosted_orchestrator::MutationAction::MutateTarget { .. },
            ..
        } => "mutate_target",
        ExecutionDecision::ExecuteTarget {
            action: crate::hosted_orchestrator::MutationAction::VerifyTargetState { .. },
            ..
        } => "verify_target_state",
        ExecutionDecision::ExecuteTarget {
            action: crate::hosted_orchestrator::MutationAction::RepairTarget { .. },
            ..
        } => "repair_target",
        ExecutionDecision::RepairTarget { .. } => "repair_target",
        ExecutionDecision::RunValidation { .. } => "run_validation",
        ExecutionDecision::ReviewDiff { .. } => "review_diff",
        ExecutionDecision::ReviewIncompleteDiff { .. } => "review_incomplete_diff",
        ExecutionDecision::EvaluateCompletion { .. } => "evaluate_completion",
        ExecutionDecision::Publish { .. } => "publish",
        ExecutionDecision::Finish { .. } => "finish",
        ExecutionDecision::StopForGuardrail { .. } => "stop_for_guardrail",
    }
}

pub(super) const fn execution_decision_requires_model_work(decision: &ExecutionDecision) -> bool {
    matches!(
        decision,
        ExecutionDecision::ContinueDiscovery { .. }
            | ExecutionDecision::ContinuePlanning { .. }
            | ExecutionDecision::ExecuteTarget {
                action: crate::hosted_orchestrator::MutationAction::MutateTarget { .. }
                    | crate::hosted_orchestrator::MutationAction::RepairTarget { .. },
                ..
            }
            | ExecutionDecision::RepairTarget { .. }
            | ExecutionDecision::StopForGuardrail { .. }
    )
}

pub(super) const fn execution_decision_has_completed_validation(
    decision: &ExecutionDecision,
) -> bool {
    matches!(
        decision,
        ExecutionDecision::ReviewDiff { .. }
            | ExecutionDecision::EvaluateCompletion { .. }
            | ExecutionDecision::Publish { .. }
            | ExecutionDecision::Finish { .. }
    )
}

pub(super) fn legal_phase_transition(from: ExecutionPhase, to: ExecutionPhase) -> bool {
    matches!(
        (from, to),
        (ExecutionPhase::Discovery, ExecutionPhase::ArtifactRepair)
            | (ExecutionPhase::Discovery, ExecutionPhase::Planning)
            | (ExecutionPhase::ArtifactRepair, ExecutionPhase::Planning)
            | (ExecutionPhase::Planning, ExecutionPhase::Implementation)
            | (ExecutionPhase::Implementation, ExecutionPhase::Repair)
            | (ExecutionPhase::Implementation, ExecutionPhase::Validation)
            | (ExecutionPhase::Repair, ExecutionPhase::Implementation)
            | (ExecutionPhase::Repair, ExecutionPhase::Validation)
            | (ExecutionPhase::Repair, ExecutionPhase::DiffReview)
            | (ExecutionPhase::Validation, ExecutionPhase::Repair)
            | (ExecutionPhase::Validation, ExecutionPhase::DiffReview)
            | (
                ExecutionPhase::DiffReview,
                ExecutionPhase::CompletionEvaluation
            )
            | (
                ExecutionPhase::CompletionEvaluation,
                ExecutionPhase::Publication
            )
            | (ExecutionPhase::Publication, ExecutionPhase::Validation)
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct PhaseTransitionPreflight {
    pub(super) schema_valid: bool,
    pub(super) transition_allowed: bool,
    pub(super) required_fields_present: bool,
    pub(super) graph_revision_matches: bool,
    pub(super) notebook_revision_matches: bool,
}

impl PhaseTransitionPreflight {
    pub(super) const fn passed(self) -> bool {
        self.schema_valid
            && self.transition_allowed
            && self.required_fields_present
            && self.graph_revision_matches
            && self.notebook_revision_matches
    }
}

/// Validates the worker event against the same local phase table that guards
/// lifecycle mutation. This prevents a deterministic contract mismatch from
/// being discovered only after the backend returns HTTP 400.
pub(super) fn preflight_phase_transition(
    payload: &Value,
    from: ExecutionPhase,
    to: ExecutionPhase,
    graph_revision: u64,
    notebook_revision: u64,
) -> PhaseTransitionPreflight {
    let decision = payload.get("decision").and_then(Value::as_str);
    let schema_valid = payload.is_object()
        && payload.get("event_type").and_then(Value::as_str) == Some("worker.phase_transition")
        && serde_json::from_value::<ExecutionPhase>(
            payload.get("from_phase").cloned().unwrap_or(Value::Null),
        )
        .ok()
            == Some(from)
        && serde_json::from_value::<ExecutionPhase>(
            payload.get("phase").cloned().unwrap_or(Value::Null),
        )
        .ok()
            == Some(to)
        && payload
            .get("transition_payload_version")
            .and_then(Value::as_u64)
            == Some(1);
    let required_fields_present = [
        "decision",
        "reason_code",
        "source",
        "source_tree_hash",
        "occurred_at",
    ]
    .iter()
    .all(|field| {
        payload
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    });
    PhaseTransitionPreflight {
        schema_valid,
        transition_allowed: legal_phase_transition(from, to)
            && !matches!((from, to), (ExecutionPhase::Repair, ExecutionPhase::DiffReview)
                if decision != Some("review_incomplete_diff")),
        required_fields_present,
        graph_revision_matches: payload.get("graph_revision").and_then(Value::as_u64)
            == Some(graph_revision),
        notebook_revision_matches: payload.get("notebook_revision").and_then(Value::as_u64)
            == Some(notebook_revision),
    }
}

pub(in crate::hosted) fn validation_rerun_completed_event(
    session: &crate::execution_graph::ValidationRepairSession,
    gate_id: &crate::execution_graph::ExecutionNodeId,
    evidence: &crate::execution_graph::ValidationEvidenceRecord,
    status: &str,
    command_runs: u32,
    local_model_calls_remaining: u32,
    mission_model_calls_remaining: u32,
) -> Value {
    json!({
        "event_type": "worker.validation_rerun_completed",
        "repair_session_id": session.session_id,
        "originating_validation_gate": gate_id,
        "failure_revision": session.current_assertion_set_revision,
        "repository_fingerprint": evidence.repository_fingerprint,
        "validation_evidence_id": evidence.evidence_id,
        "status": status,
        "command_runs": command_runs,
        "model_calls_consumed": 0,
        "local_model_calls_remaining": local_model_calls_remaining,
        "mission_model_calls_remaining": mission_model_calls_remaining,
    })
}

pub(super) fn hosted_tools() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "name": "record_impact_map",
            "description": "Record semantic area mappings. The orchestrator expands evidence references and attaches canonical v2 wrapper fields.",
            "parameters": impact_map::provider_tool_schema(),
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "record_implementation_plan",
            "description": "End planning with a machine-readable mapping from canonical acceptance-criterion IDs to the relevant edits and tests. Coverage is evaluated across the complete plan; each change needs only its relevant IDs.",
            "parameters": {
                "type": "object",
                "properties": {
                    "implementation_status": {"type": "string", "enum": ["ready", "blocked"]},
                    "planned_changes": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "change_id": {"type": "string"},
                                "parent_change_id": {"type": ["string", "null"]},
                                "intent": {"type": "string"},
                                "reason": {"type": "string"},
                                "targets": {
                                    "type": "array",
                                    "minItems": 1,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "path": {"type": "string"},
                                            "role": {"type": "string"},
                                            "operation": {
                                                "type": "object",
                                                "properties": {
                                                    "kind": {"type": "string", "enum": ["modify_existing", "create_new", "delete_existing", "rename", "move"]},
                                                    "source": {"type": ["string", "null"]},
                                                    "destination": {"type": ["string", "null"]}
                                                },
                                                "required": ["kind", "source", "destination"],
                                                "additionalProperties": false
                                            },
                                            "new_file": {"type": "boolean"},
                                            "status": {"type": "string", "enum": ["planned", "in_progress", "applied", "verified", "partial", "unresolved"]}
                                        },
                                        "required": ["path", "role", "operation", "new_file", "status"],
                                        "additionalProperties": false
                                    }
                                },
                                "status": {"type": "string", "enum": ["planned", "in_progress", "applied", "verified", "partial", "unresolved"]},
                                "acceptance_criteria_ids": {"type": "array", "items": {"type": "string"}},
                                "test_coverage": {"type": "array", "items": {"type": "string"}}
                            },
                            "required": ["change_id", "parent_change_id", "intent", "reason", "targets", "status", "acceptance_criteria_ids", "test_coverage"],
                            "additionalProperties": false
                        }
                    },
                    "planned_new_files": {"type": "array", "items": {"type": "string"}},
                    "planned_test_changes": {"type": "array", "items": {"type": "string"}},
                    "remaining_unknowns": {"type": "array", "items": {"type": "string"}},
                    "blocking_unknowns": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["implementation_status", "planned_changes", "planned_new_files", "planned_test_changes", "remaining_unknowns", "blocking_unknowns"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "list_files",
            "description": "List bounded repository-relative files. Use null for the repository root.",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": ["string", "null"]}},
                "required": ["path"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "read_file",
            "description": "Read a bounded line range from one UTF-8 repository file. Use null line bounds for defaults.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "start_line": {"type": ["integer", "null"], "minimum": 1},
                    "end_line": {"type": ["integer", "null"], "minimum": 1},
                    "reason": {"type": "string"}
                },
                "required": ["path", "start_line", "end_line", "reason"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "read_files",
            "description": "Read up to 20 selected UTF-8 repository files with independent per-file results and deterministic individual fallback for failures.",
            "parameters": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": 20
                    },
                    "maximum_lines_per_file": {"type": ["integer", "null"], "minimum": 1, "maximum": 1000},
                    "reason": {"type": "string"}
                },
                "required": ["paths", "maximum_lines_per_file", "reason"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "search_text",
            "description": "Search UTF-8 repository files with grouped, deduplicated results. Broad searches are discovery-only.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "path": {"type": ["string", "null"]},
                    "extensions": {"type": "array", "items": {"type": "string"}, "maxItems": 20},
                    "mode": {"type": "string", "enum": ["literal"]},
                    "context_lines": {"type": "integer", "minimum": 0, "maximum": 5},
                    "reason": {"type": "string"}
                },
                "required": ["query", "path", "extensions", "mode", "context_lines", "reason"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "related_tests",
            "description": "Find concise candidate test and spec paths related to selected source paths.",
            "parameters": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": 20
                    },
                    "reason": {"type": "string"}
                },
                "required": ["paths", "reason"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "write_file",
            "description": "Create or replace one UTF-8 repository file with complete content.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["change_id", "path", "content"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "replace_text",
            "description": "Edit one existing UTF-8 repository file by replacing one exact, unique string. Use this for targeted edits instead of mutation commands.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"}
                },
                "required": ["change_id", "path", "old_text", "new_text"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "replace_range",
            "description": "Replace an inclusive one-based line range in one UTF-8 file. Prefer this after an exact replacement is ambiguous.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1},
                    "new_text": {"type": "string"}
                },
                "required": ["change_id", "path", "start_line", "end_line", "new_text"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "insert_after_symbol",
            "description": "Insert UTF-8 content immediately after one exact unique symbol.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "symbol": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["change_id", "path", "symbol", "content"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "insert_before_symbol",
            "description": "Insert UTF-8 content immediately before one exact unique symbol.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "symbol": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["change_id", "path", "symbol", "content"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "apply_patch",
            "description": "Apply one bounded unified diff that modifies only the declared repository path.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "patch": {"type": "string"}
                },
                "required": ["change_id", "path", "patch"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "replace_file",
            "description": "Deterministically replace the complete contents of the exact active UTF-8 target, creating it when the accepted plan marks it as new.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["change_id", "path", "content"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "record_no_valid_repair",
            "description": "Record that bounded validation diagnosis found no safe source or test mutation. This is a typed terminal repair result, not a free-form answer.",
            "parameters": {
                "type": "object",
                "properties": {
                    "diagnosis": {
                        "type": "string",
                        "enum": ["source_defect", "test_expectation_defect", "both", "inconclusive"]
                    },
                    "reason": {"type": "string"}
                },
                "required": ["diagnosis", "reason"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "record_repair_intent_satisfied",
            "description": "Record proof that the current target already satisfies the active validation-repair assertion contract.",
            "parameters": {
                "type": "object",
                "properties": {
                    "repair_intent_id": {"type": "string"},
                    "target_path": {"type": "string"},
                    "expected_state_hash": {"type": "string"},
                    "current_state_hash": {"type": "string"},
                    "satisfied_assertions": {"type": "array", "items": {"type": "string"}},
                    "supporting_evidence_ids": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["repair_intent_id", "target_path", "expected_state_hash", "current_state_hash", "satisfied_assertions", "supporting_evidence_ids"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "delete_file",
            "description": "Delete one regular repository file.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["change_id", "path"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "create_file",
            "description": "Atomically create the exact absent target declared by a create_new operation.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "create_parents": {"type": "boolean"}
                },
                "required": ["change_id", "path", "content", "create_parents"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "rename_file",
            "description": "Atomically rename the exact source to the exact absent destination declared by a rename operation.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "source": {"type": "string"},
                    "create_parents": {"type": "boolean"}
                },
                "required": ["change_id", "path", "source", "create_parents"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "move_file",
            "description": "Atomically move the exact source to the exact absent destination declared by a move operation.",
            "parameters": {
                "type": "object",
                "properties": {
                    "change_id": {"type": "string"},
                    "path": {"type": "string"},
                    "source": {"type": "string"},
                    "create_parents": {"type": "boolean"}
                },
                "required": ["change_id", "path", "source", "create_parents"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "report_write_progress",
            "description": "At the implementation-progress threshold, report the precise blocker or the next planned write instead of continuing exploration.",
            "parameters": {
                "type": "object",
                "properties": {
                    "status": {"type": "string", "enum": ["blocked", "ready_to_write", "no_change_required"]},
                    "reason": {"type": "string"},
                    "next_write": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "operation": {"type": "string"}
                        },
                        "required": ["path", "operation"],
                        "additionalProperties": false
                    }
                },
                "required": ["status", "reason", "next_write"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "repository_snapshot",
            "description": "Inspect git status, changed paths, diff statistics, and every page of the immutable complete diff before declaring implementation status. Start with cursor 0 and follow next_cursor until review_complete is true.",
            "parameters": {
                "type": "object",
                "properties": {
                    "cursor": {"type": ["integer", "null"], "minimum": 0}
                },
                "required": ["cursor"],
                "additionalProperties": false
            },
            "strict": true
        }),
        json!({
            "type": "function",
            "name": "declare_implementation",
            "description": "After reviewing repository status and the complete diff, declare whether implementation is complete, partial, or blocked.",
            "parameters": {
                "type": "object",
                "properties": {
                    "implementation_status": {"type": "string", "enum": ["complete", "partial", "blocked"]},
                    "completed_work": {"type": "array", "items": {"type": "string"}},
                    "remaining_work": {"type": "array", "items": {"type": "string"}},
                    "known_risks": {"type": "array", "items": {"type": "string"}},
                    "changed_paths": {"type": "array", "items": {"type": "string"}},
                    "criteria_evidence": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "criterion": {"type": "string"},
                                "paths": {"type": "array", "items": {"type": "string"}},
                                "evidence": {"type": "string"}
                            },
                            "required": ["criterion", "paths", "evidence"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["implementation_status", "completed_work", "remaining_work", "known_risks", "changed_paths", "criteria_evidence"],
                "additionalProperties": false
            },
            "strict": true
        }),
    ]
}

pub(super) fn hosted_tools_for_phase(phase: ExecutionPhase) -> Vec<Value> {
    hosted_tools()
        .into_iter()
        .filter(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| phase_permits_tool(phase, name))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ModelActionProfile {
    pub(super) max_output_tokens: u64,
    pub(super) reasoning_effort: &'static str,
    pub(super) forced_tool: Option<&'static str>,
    pub(super) require_tool: bool,
}

impl ModelActionProfile {
    pub(super) fn for_decision(
        phase: ExecutionPhase,
        decision: Option<&ExecutionDecision>,
        configured_max_output_tokens: u64,
    ) -> Self {
        match decision {
            Some(ExecutionDecision::ExecuteTarget {
                action:
                    crate::hosted_orchestrator::MutationAction::RepairTarget {
                        fallback_policy, ..
                    },
                ..
            }) if fallback_policy.requires_provider_mutation() => Self {
                max_output_tokens: configured_max_output_tokens.min(4_096),
                reasoning_effort: "medium",
                forced_tool: fallback_policy.forced_tool(),
                require_tool: true,
            },
            Some(ExecutionDecision::ExecuteTarget {
                action:
                    crate::hosted_orchestrator::MutationAction::PrepareTargetContext { .. }
                    | crate::hosted_orchestrator::MutationAction::VerifyTargetState { .. },
                ..
            }) => Self {
                max_output_tokens: configured_max_output_tokens.min(4_096),
                reasoning_effort: "low",
                forced_tool: None,
                require_tool: false,
            },
            Some(ExecutionDecision::ExecuteTarget {
                action:
                    crate::hosted_orchestrator::MutationAction::MutateTarget { .. }
                    | crate::hosted_orchestrator::MutationAction::RepairTarget { .. },
                ..
            }) => Self {
                max_output_tokens: configured_max_output_tokens.min(4_096),
                reasoning_effort: "medium",
                forced_tool: None,
                require_tool: true,
            },
            Some(ExecutionDecision::ContinueDiscovery {
                action: crate::hosted_orchestrator::DiscoveryAction::InspectRepository { .. },
            }) => Self {
                max_output_tokens: configured_max_output_tokens.min(4_096),
                reasoning_effort: "medium",
                forced_tool: None,
                require_tool: false,
            },
            Some(ExecutionDecision::ContinueDiscovery {
                action:
                    crate::hosted_orchestrator::DiscoveryAction::FinalizeImpactMap { .. }
                    | crate::hosted_orchestrator::DiscoveryAction::RepairImpactMap { .. },
            }) => Self {
                max_output_tokens: configured_max_output_tokens.min(2_048),
                reasoning_effort: "low",
                forced_tool: Some("record_impact_map"),
                require_tool: true,
            },
            Some(ExecutionDecision::ContinuePlanning {
                action: crate::hosted_orchestrator::PlanningAction::BuildPlan { .. },
            }) => Self {
                max_output_tokens: configured_max_output_tokens.min(4_096),
                reasoning_effort: "medium",
                forced_tool: Some("record_implementation_plan"),
                require_tool: true,
            },
            Some(ExecutionDecision::ContinuePlanning {
                action: crate::hosted_orchestrator::PlanningAction::RepairPlan { .. },
            }) => Self {
                max_output_tokens: configured_max_output_tokens.min(4_096),
                reasoning_effort: "low",
                forced_tool: Some("record_implementation_plan"),
                require_tool: true,
            },
            Some(ExecutionDecision::ContinuePlanning {
                action: crate::hosted_orchestrator::PlanningAction::ResolveEvidenceGap { .. },
            }) => Self {
                max_output_tokens: configured_max_output_tokens.min(4_096),
                reasoning_effort: "medium",
                forced_tool: None,
                require_tool: false,
            },
            _ => Self {
                max_output_tokens: configured_max_output_tokens.min(match phase {
                    ExecutionPhase::ArtifactRepair => 2_048,
                    ExecutionPhase::Discovery => 4_096,
                    _ => 16_384,
                }),
                reasoning_effort: if phase == ExecutionPhase::ArtifactRepair {
                    "low"
                } else {
                    "medium"
                },
                forced_tool: (phase == ExecutionPhase::ArtifactRepair)
                    .then_some("record_impact_map"),
                require_tool: phase == ExecutionPhase::ArtifactRepair,
            },
        }
    }

    pub(super) fn tool_choice(self) -> Value {
        self.forced_tool.map_or_else(
            || {
                if self.require_tool {
                    json!("required")
                } else {
                    json!("auto")
                }
            },
            |name| json!({"type": "function", "name": name}),
        )
    }
}

pub(super) fn hosted_tools_for_action(
    phase: ExecutionPhase,
    decision: Option<&ExecutionDecision>,
) -> Vec<Value> {
    let operation_tools =
        |target: &crate::execution_graph::PlannedTarget| -> &'static [&'static str] {
            match target.effective_operation() {
                crate::execution_graph::TargetOperation::ModifyExisting => {
                    &["apply_patch", "replace_file"]
                }
                crate::execution_graph::TargetOperation::CreateNew => &["create_file"],
                crate::execution_graph::TargetOperation::DeleteExisting => &["delete_file"],
                crate::execution_graph::TargetOperation::Rename { .. } => {
                    &["rename_file", "move_file"]
                }
                crate::execution_graph::TargetOperation::Move { .. } => &["move_file"],
            }
        };
    let active_mutation_target = match decision {
        Some(ExecutionDecision::ExecuteTarget {
            action:
                crate::hosted_orchestrator::MutationAction::MutateTarget { target, .. }
                | crate::hosted_orchestrator::MutationAction::RepairTarget { target, .. },
            ..
        }) => Some(target.path.as_str()),
        _ => None,
    };
    let allowed = match decision {
        Some(ExecutionDecision::ContinueDiscovery {
            action: crate::hosted_orchestrator::DiscoveryAction::InspectRepository { .. },
        }) => Some(
            &[
                "list_files",
                "read_file",
                "read_files",
                "search_text",
                "related_tests",
            ][..],
        ),
        Some(ExecutionDecision::ContinueDiscovery {
            action:
                crate::hosted_orchestrator::DiscoveryAction::FinalizeImpactMap { .. }
                | crate::hosted_orchestrator::DiscoveryAction::RepairImpactMap { .. },
        }) => Some(&["record_impact_map"][..]),
        Some(ExecutionDecision::ContinuePlanning {
            action:
                crate::hosted_orchestrator::PlanningAction::BuildPlan { .. }
                | crate::hosted_orchestrator::PlanningAction::RepairPlan { .. },
        }) => Some(&["record_implementation_plan"][..]),
        Some(ExecutionDecision::ContinuePlanning {
            action: crate::hosted_orchestrator::PlanningAction::ResolveEvidenceGap { .. },
        }) => Some(&["read_file", "read_files", "search_text", "related_tests"][..]),
        Some(ExecutionDecision::ExecuteTarget {
            action:
                crate::hosted_orchestrator::MutationAction::PrepareTargetContext { .. }
                | crate::hosted_orchestrator::MutationAction::VerifyTargetState { .. },
            ..
        }) => Some(&[][..]),
        Some(ExecutionDecision::ExecuteTarget {
            action:
                crate::hosted_orchestrator::MutationAction::RepairTarget {
                    fallback_policy, ..
                },
            ..
        }) if fallback_policy.requires_provider_mutation() => {
            Some(fallback_policy.permitted_tools())
        }
        Some(ExecutionDecision::ExecuteTarget {
            action: crate::hosted_orchestrator::MutationAction::RepairTarget { failure, .. },
            target,
            ..
        }) if failure.category == crate::execution_graph::FailureCategory::ValidationFailure => {
            Some(
                if matches!(
                    target.target.effective_operation(),
                    crate::execution_graph::TargetOperation::ModifyExisting
                ) {
                    &[
                        "apply_patch",
                        "replace_file",
                        "record_no_valid_repair",
                        "record_repair_intent_satisfied",
                    ][..]
                } else {
                    operation_tools(&target.target)
                },
            )
        }
        Some(ExecutionDecision::ExecuteTarget {
            action:
                crate::hosted_orchestrator::MutationAction::MutateTarget { .. }
                | crate::hosted_orchestrator::MutationAction::RepairTarget { .. },
            target,
            ..
        }) => Some(operation_tools(&target.target)),
        _ => None,
    };
    let tools = hosted_tools_for_phase(phase);
    let mut selected = allowed.map_or(tools.clone(), |allowed| {
        tools
            .into_iter()
            .filter(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| allowed.contains(&name))
            })
            .collect()
    });
    if let Some(path) = active_mutation_target {
        for tool in &mut selected {
            if let Some(path_schema) = tool
                .get_mut("parameters")
                .and_then(Value::as_object_mut)
                .and_then(|parameters| parameters.get_mut("properties"))
                .and_then(Value::as_object_mut)
                .and_then(|properties| properties.get_mut("path"))
                .and_then(Value::as_object_mut)
            {
                path_schema.insert("enum".into(), json!([path]));
            }
        }
    }
    selected
}

pub(super) fn active_mutation_fallback(
    decision: Option<&ExecutionDecision>,
) -> Option<(
    &crate::execution_graph::ExecutionNodeId,
    &crate::execution_graph::TargetExecutionContext,
    MutationFallbackPolicy,
    MutationApplicationFailure,
)> {
    match decision {
        Some(ExecutionDecision::ExecuteTarget {
            node_id,
            action:
                crate::hosted_orchestrator::MutationAction::RepairTarget {
                    failure,
                    fallback_policy,
                    ..
                },
            target,
        }) if failure.category == crate::execution_graph::FailureCategory::MutationConflict => {
            let failure_category = failure
                .code
                .as_deref()
                .and_then(MutationApplicationFailure::from_code)
                .unwrap_or(MutationApplicationFailure::InvalidPatchTarget);
            Some((node_id, target, *fallback_policy, failure_category))
        }
        Some(ExecutionDecision::RepairTarget {
            node_id, context, ..
        }) if context.failure.category
            == crate::execution_graph::FailureCategory::MutationConflict =>
        {
            let failure_category = context
                .failure
                .code
                .as_deref()
                .and_then(MutationApplicationFailure::from_code)
                .unwrap_or(MutationApplicationFailure::InvalidPatchTarget);
            Some((
                node_id,
                &context.target,
                context.fallback_policy,
                failure_category,
            ))
        }
        _ => None,
    }
}

pub(super) fn mutation_repair_request_preflight(
    decision: Option<&ExecutionDecision>,
    request: &Value,
) -> Option<RepairRequestPreflight> {
    let (_, target, policy, _) = active_mutation_fallback(decision)?;
    let expected_tools = policy.permitted_tools();
    let request_tools = request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let exact_target_bound = request
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            !tools.is_empty()
                && tools.iter().all(|tool| {
                    tool.pointer("/parameters/properties/path/enum")
                        .and_then(Value::as_array)
                        .is_some_and(|values| {
                            values.len() == 1
                                && values[0].as_str() == Some(target.target.path.as_str())
                        })
                })
        });
    let forced_tool_choice = request.pointer("/tool_choice/name").and_then(Value::as_str);
    let request_context = request
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let target_content_attached = target.current_file_content.as_ref().is_some_and(|content| {
        serde_json::to_string(content).is_ok_and(|encoded| request_context.contains(&encoded))
    });
    let target_hash_attached = target
        .target_content_hash
        .as_ref()
        .or(target.source_content_hash.as_ref())
        .is_some_and(|hash| request_context.contains(hash));
    Some(RepairRequestPreflight {
        policy_present: policy != MutationFallbackPolicy::NoSafeFallback
            && request_context.contains(policy.as_str()),
        policy_compatible_with_operation: policy
            .compatible_with(&target.target.effective_operation()),
        exact_target_bound,
        required_content_present: policy != MutationFallbackPolicy::ForceReplaceFile
            || target_content_attached,
        target_hash_present: matches!(policy, MutationFallbackPolicy::ForceCreateFile)
            || target_hash_attached,
        repository_fingerprint_present: !target.repository_fingerprint.is_empty()
            && request_context.contains(target.repository_fingerprint.as_str()),
        tool_surface_matches_policy: request_tools.as_slice() == expected_tools,
        forced_tool_choice_matches_policy: forced_tool_choice == policy.forced_tool(),
    })
}

pub(super) fn mutation_tool_policy_violation(
    decision: Option<&ExecutionDecision>,
    received_tool: &str,
) -> Option<MutationToolPolicyViolation> {
    let (node_id, target, policy, _) = active_mutation_fallback(decision)?;
    (!policy.permitted_tools().contains(&received_tool)).then(|| MutationToolPolicyViolation {
        node_id: node_id.clone(),
        target_path: target.target.path.clone(),
        active_policy: policy,
        expected_tools: policy
            .permitted_tools()
            .iter()
            .map(|tool| (*tool).to_owned())
            .collect(),
        received_tool: received_tool.to_owned(),
    })
}

pub(super) fn discovery_action_permits_tool(
    decision: Option<&ExecutionDecision>,
    name: &str,
) -> bool {
    match decision {
        Some(ExecutionDecision::ContinueDiscovery {
            action: crate::hosted_orchestrator::DiscoveryAction::InspectRepository { .. },
        }) => matches!(
            name,
            "list_files" | "read_file" | "read_files" | "search_text" | "related_tests"
        ),
        Some(ExecutionDecision::ContinueDiscovery {
            action:
                crate::hosted_orchestrator::DiscoveryAction::FinalizeImpactMap { .. }
                | crate::hosted_orchestrator::DiscoveryAction::RepairImpactMap { .. },
        }) => name == "record_impact_map",
        _ => true,
    }
}

pub(super) fn planning_action_permits_tool(
    decision: Option<&ExecutionDecision>,
    name: &str,
) -> bool {
    match decision {
        Some(ExecutionDecision::ContinuePlanning {
            action:
                crate::hosted_orchestrator::PlanningAction::BuildPlan { .. }
                | crate::hosted_orchestrator::PlanningAction::RepairPlan { .. },
        }) => name == "record_implementation_plan",
        Some(ExecutionDecision::ContinuePlanning {
            action: crate::hosted_orchestrator::PlanningAction::ResolveEvidenceGap { .. },
        }) => matches!(
            name,
            "read_file" | "read_files" | "search_text" | "related_tests"
        ),
        _ => true,
    }
}

pub(super) fn successful_tool_updates_last_action(
    name: &str,
    file_evidence_before: usize,
    file_evidence_after: usize,
) -> bool {
    !matches!(name, "read_file" | "read_files") || file_evidence_after > file_evidence_before
}

pub(super) fn compact_impact_map_finalization_context(notebook: &WorkerNotebook) -> String {
    serde_json::to_string(&json!({
        "instruction": "Repository inspection is complete. Use only the persisted evidence below and call record_impact_map exactly once.",
        "acceptance_criteria": notebook.acceptance_criteria_v2,
        "evidence": notebook.impact_evidence,
        "files_inspected": notebook.files_inspected,
        "searches_completed": notebook.searches_completed,
        "architecture_findings": notebook.architecture_findings,
        "known_related_tests": notebook.files_inspected.iter().filter(|path| {
            let lower = path.to_ascii_lowercase();
            lower.contains("test") || lower.contains("spec")
        }).collect::<Vec<_>>(),
        "blocking_unknowns": notebook.blocking_unknowns,
        "canonical_schema": impact_map::schema(),
    }))
    .unwrap_or_else(|_| "Call record_impact_map exactly once using persisted evidence.".into())
}

pub(super) fn repository_validation_commands_from_evidence(
    notebook: &WorkerNotebook,
) -> Vec<String> {
    let Some(package) = notebook
        .orchestration
        .evidence
        .files
        .values()
        .find(|evidence| {
            evidence.repository_fingerprint == notebook.repository_fingerprint
                && evidence.path == "package.json"
        })
    else {
        return Vec::new();
    };
    serde_json::from_str::<Value>(&package.captured_content)
        .ok()
        .and_then(|value| value.get("scripts").and_then(Value::as_object).cloned())
        .into_iter()
        .flat_map(|scripts| {
            scripts.into_iter().filter_map(|(name, command)| {
                matches!(
                    name.as_str(),
                    "test" | "lint" | "build" | "typecheck" | "check"
                )
                .then(|| command.as_str().map(|_| format!("npm run {name}")))
                .flatten()
            })
        })
        .collect()
}

pub(super) fn compact_implementation_plan_context(
    notebook: &WorkerNotebook,
    decision: Option<&ExecutionDecision>,
) -> String {
    let candidate_paths = notebook
        .impact_map
        .iter()
        .flat_map(|area| area.candidate_paths.iter().cloned())
        .collect::<BTreeSet<_>>();
    let evidence = notebook
        .orchestration
        .evidence
        .files
        .values()
        .filter(|evidence| {
            evidence.repository_fingerprint == notebook.repository_fingerprint
                && (candidate_paths.contains(&evidence.path) || evidence.path == "package.json")
        })
        .take(12)
        .map(|evidence| {
            json!({
                "evidence_id": evidence.evidence_id,
                "path": evidence.path,
                "line_range": evidence.line_range,
                "content_hash": evidence.content_hash,
                "captured_excerpt": truncate_text(&evidence.captured_content, 2_000),
                "truncated": evidence.truncated,
            })
        })
        .collect::<Vec<_>>();
    let (action, validation_errors, previous_plan) = match decision {
        Some(ExecutionDecision::ContinuePlanning {
            action:
                crate::hosted_orchestrator::PlanningAction::RepairPlan {
                    validation_errors,
                    previous_plan,
                },
        }) => (
            "repair_plan",
            json!(validation_errors),
            previous_plan.value.clone(),
        ),
        _ => ("build_plan", Value::Array(Vec::new()), Value::Null),
    };
    let context = json!({
        "instruction": "Discovery is complete. Use only this persisted evidence and call record_implementation_plan exactly once. Do not request repository reads or searches.",
        "action": action,
        "ticket_goal": notebook.goal,
        "acceptance_criteria": notebook.acceptance_criteria_v2,
        "accepted_impact_map": notebook.impact_map_v2,
        "inspected_file_paths": notebook.files_inspected,
        "candidate_file_evidence": evidence,
        "architecture_findings": notebook.architecture_findings,
        "related_tests": notebook.files_inspected.iter().filter(|path| {
            let lower = path.to_ascii_lowercase();
            lower.contains("test") || lower.contains("spec")
        }).collect::<Vec<_>>(),
        "repository_validation_commands": repository_validation_commands_from_evidence(notebook),
        "repository_fingerprint": notebook.repository_fingerprint,
        "validation_errors": validation_errors,
        "previous_plan": previous_plan,
        "preserved_valid_plan_fragments": notebook.planning_repair.as_ref().map(|repair| &repair.valid_planned_changes),
    });
    truncate_text(
        &serde_json::to_string(&context).unwrap_or_else(|_| {
            "Call record_implementation_plan exactly once using persisted discovery evidence."
                .into()
        }),
        28 * 1024,
    )
}

pub(super) fn compact_impact_map_repair_context(
    failure: Option<&ImpactMapFailure>,
    notebook: &WorkerNotebook,
) -> String {
    let criteria = notebook
        .acceptance_criteria_v2
        .iter()
        .map(|criterion| json!({"id":criterion.id,"text":truncate_text(&criterion.text, 500)}))
        .collect::<Vec<_>>();
    let evidence = &notebook.impact_evidence;
    let context = json!({
        "instruction":"The previous impact map was semantically useful but failed validation. Correct only the invalid structural portions. Do not perform repository discovery. Call record_impact_map exactly once.",
        "invalid_artifact":failure.map(|failure| &failure.invalid_payload),
        "validation_errors":failure.map(|failure| &failure.errors).unwrap_or(&Vec::new()),
        "canonical_schema":impact_map::schema(),
        "allowed_model_fields":["areas","name","candidate_paths","evidence_refs","acceptance_criteria_ids","reason"],
        "evidence":evidence,
        "acceptance_criteria":criteria,
        "minimal_valid_model_input":{"areas":[{"name":"Affected surface","candidate_paths":["src/example.rs"],"evidence_refs":["read-1"],"acceptance_criteria_ids":["ac-1"],"reason":"Implements the criterion."}]},
        "tool_schema_version":IMPACT_MAP_SCHEMA_VERSION,
        "tool_schema_sha256":impact_map::schema_sha256(),
    });
    truncate_text(&serde_json::to_string(&context).unwrap_or_default(), 19_000)
}

pub(super) fn artifact_call_accounting(phase: ExecutionPhase) -> Value {
    let supplemental = phase == ExecutionPhase::ArtifactRepair;
    json!({
        "provider_call_occurred": true,
        "configured_mission_budget_consumed": !supplemental,
        "supplemental_repair_budget_consumed": supplemental,
    })
}

pub(super) fn impact_map_artifact_attempt_payload(phase: ExecutionPhase) -> Value {
    let supplemental = phase == ExecutionPhase::ArtifactRepair;
    json!({
        "event_type":"worker.impact_map_artifact_attempt",
        "artifact_status":"attempted",
        "failure_layer":Value::Null,
        "tool_schema_version":IMPACT_MAP_SCHEMA_VERSION,
        "tool_schema_sha256":impact_map::schema_sha256(),
        "validator_schema_version":IMPACT_MAP_SCHEMA_VERSION,
        "validator_schema_sha256":impact_map::schema_sha256(),
        "provider_call_occurred":true,
        "configured_mission_budget_consumed":!supplemental,
        "supplemental_repair_budget_consumed":supplemental,
        "accounting": artifact_call_accounting(phase),
    })
}

pub(super) fn accepted_artifact_normalization_metadata(
    artifact_source: ArtifactSource,
    triggering_error: Option<&anyhow::Error>,
) -> Option<Value> {
    (artifact_source == ArtifactSource::NormalizedModel).then(|| {
        json!({
            "normalized": true,
            "blocking": false,
            "original_diagnostic": triggering_error
                .map(|error| truncate_text(&format!("{error:#}"), 2_000)),
        })
    })
}

pub(super) fn hosted_agent_instructions_for_decision(
    phase: ExecutionPhase,
    decision: Option<&ExecutionDecision>,
) -> String {
    match decision {
        Some(ExecutionDecision::ExecuteTarget {
            action:
                crate::hosted_orchestrator::MutationAction::RepairTarget {
                    target,
                    failure,
                    fallback_policy,
                    ..
                },
            ..
        }) if *fallback_policy == MutationFallbackPolicy::ForceReplaceFile => format!(
            "Repair exactly `{}` under active fallback policy `force_replace_file` with one forced `replace_file` call. Use the exact current target content, content hash, repository fingerprint, accepted implementation intent, and rejected-mutation diagnostic in the authoritative input. The rejected patch was not applied. Return the complete desired file content, preserve unrelated behavior, and do not emit another patch or inspect another path. Original failure category: {:?}.",
            target.path, failure.code
        ),
        Some(ExecutionDecision::ExecuteTarget {
            action:
                crate::hosted_orchestrator::MutationAction::RepairTarget {
                    target,
                    fallback_policy,
                    ..
                },
            ..
        }) if fallback_policy.requires_provider_mutation() => format!(
            "Repair exactly `{}` under active fallback policy `{:?}`. Invoke exactly the forced `{}` tool and no other mutation strategy. Use the exact current target context and rejected-mutation evidence; the rejected mutation was not applied.",
            target.path,
            fallback_policy,
            fallback_policy.forced_tool().unwrap_or("none")
        ),
        Some(ExecutionDecision::ExecuteTarget {
            action:
                crate::hosted_orchestrator::MutationAction::RepairTarget {
                    target, failure, ..
                },
            ..
        }) if failure.category == crate::execution_graph::FailureCategory::ValidationFailure => {
            format!(
                "Diagnose the structured validation assertion failures against the bounded implicated target contents. Classify source_defect, test_expectation_defect, both, or inconclusive. Repair exactly the selected target `{}` with one admitted mutation tool when safe. Invoke record_repair_intent_satisfied only with exact hashes plus assertion and evidence IDs that prove the active repair contract is already satisfied. Otherwise invoke record_no_valid_repair with the diagnosis and concrete evidence-based reason. Do not emit a free-form answer, edit an unlisted path, blindly change a test expectation, or run validation.",
                target.path
            )
        }
        Some(ExecutionDecision::ExecuteTarget {
            action:
                crate::hosted_orchestrator::MutationAction::MutateTarget { target, .. }
                | crate::hosted_orchestrator::MutationAction::RepairTarget { target, .. },
            ..
        }) => format!(
            "You are executing exactly one repository mutation target: `{}`. The accepted intent, assigned acceptance criteria, current cached content, impact evidence, related-test excerpts, and preservation constraints are in the authoritative input context. Do not rediscover the repository and do not request or modify another path. Invoke exactly one admitted mutation tool for this exact target. A free-form response or read request is MutationNotProduced. Preserve unrelated behavior and do not run validation; deterministic verification follows the tool operation.",
            target.path
        ),
        _ => hosted_agent_instructions(phase),
    }
}

pub(super) fn hosted_agent_instructions(phase: ExecutionPhase) -> String {
    if phase == ExecutionPhase::ArtifactRepair {
        return "You are repairing the structured implementation impact map for an ephemeral \
RustGrid mission. Repository discovery from the previous phase is preserved in the worker \
notebook. Do not repeat reads or searches. Use only record_impact_map, reconstructing a strict \
impact map from the inspected files, searches, architecture findings, candidate paths, and \
acceptance criteria already present. Do not edit source files or perform additional exploration."
            .into();
    }
    format!(
        "You are the implementation model inside an ephemeral RustGrid GitHub Actions worker. \
The active hard execution phase is `{}`. Use only tools admitted for that phase and transition as \
soon as its required structured artifact is complete. Discovery must end with record_impact_map; \
planning must end with record_implementation_plan; implementation must make planned edits rather \
than restart broad exploration; diff review must use repository_snapshot and \
declare_implementation. For localized theme or visual-system work, discovery should identify the \
theme provider, selector, centralized token source, focused tests, and package validation commands. \
Once centralized semantic variables are confirmed, sample at most three representative consumers \
and stop discovery unless a targeted search proves a direct hardcoded-color exception. Keep each \
discovery request below roughly 12,000 input tokens where possible. \
Use only the provided repository tools. Inspect the smallest relevant scope, follow repository \
instructions, and record a repository-level impact map before editing. Batch repository discovery \
within each response instead of spending one model call per file. Implement the mission, add focused \
tests, and inspect repository status plus the complete diff before finishing. Use \
replace_text for the first targeted edit. After an ambiguous replacement, perform one bounded \
read_file; after a second ambiguity, switch to replace_range, a unique-symbol insertion, \
apply_unified_diff, or rewrite_small_file for a small file. write_file is appropriate only when \
replacing a complete file. Every source-changing call must cite the stable change_id from the plan. \
Prefer one planned change per independently editable file; use parent_change_id only to group \
related file changes. When one logical change genuinely has multiple targets, represent them as \
structured target objects and mutate one concrete member path per tool call. Never encode multiple \
paths in one string. In the plan, use acceptance_criteria_ids with canonical ac-N values, cover every \
required ID across the complete plan, and attach to each change only the IDs relevant to that change. \
A mutation authorization or plan-metadata rejection is not a content-edit \
failure: do not switch editing tools; allow the orchestrator to repair metadata deterministically. \
The worker executes every configured validation gate through the execution graph; do not attempt \
free-form validation commands from the model tool loop. Never \
commit, push, switch branches, modify Git remotes, open pull requests, read environment variables, \
read files outside the repository, or attempt to discover credentials. The RustGrid worker owns \
full quality gates and publication. Call declare_implementation after diff review, then end with a \
concise implementation and focused-validation summary. Never declare complete while planned work, \
acceptance criteria, or a genuinely unresolved intended change remains. Failed tool attempts are \
diagnostic history and do not invalidate a later verified intended change.",
        phase.as_str()
    )
}

pub(super) fn build_hosted_prompt(
    manifest: &HostedManifest,
    repo: &Repo,
    partial_run: Option<&PartialRunContext>,
) -> Result<String> {
    let files = collect_repo_files(&repo.root, &repo.root, 1_200)?;
    let continuation_guidance = partial_implementation_guidance(partial_run);
    let instructions = read_repo_instructions(&repo.root)?
        .into_iter()
        .map(|(name, content)| {
            format!(
                "\n\nRepository instruction file {name}:\n{}",
                truncate_text(&content, 24_000)
            )
        })
        .collect::<String>();
    let visual_guidance = visual_impact_guidance(&format!(
        "{}\n{}",
        manifest.ticket_title, manifest.run.input_prompt
    ));
    Ok(format!(
        "Implement RustGrid ticket {key}: {title}\n\nMission instructions:\n{prompt}\n\n\
Execution attempt: {attempt}\nDeterministic branch: {branch}\nResolved model: {model}\n\
Maximum model calls: {calls}\nMaximum cost USD: {cost}{visual_guidance}{continuation_guidance}\n\nRepository files:\n{files}{instructions}",
        key = manifest.ticket_key,
        title = manifest.ticket_title,
        prompt = manifest.run.input_prompt,
        attempt = manifest.execution.attempt_number,
        branch = manifest.github.branch,
        model = manifest.ai_gateway.model,
        calls = manifest.ai_gateway.maximum_model_calls,
        cost = manifest.ai_gateway.maximum_cost_usd,
        visual_guidance = visual_guidance,
        files = files.join("\n"),
        continuation_guidance = continuation_guidance,
    ))
}

pub(super) fn partial_implementation_guidance(partial_run: Option<&PartialRunContext>) -> String {
    let Some(partial_run) = partial_run else {
        return String::new();
    };
    let remaining_work = if partial_run.remaining_work.is_empty() {
        "- Reconcile the preserved diff against every acceptance criterion.".to_owned()
    } else {
        partial_run
            .remaining_work
            .iter()
            .map(|work| format!("- {work}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "\n\nExisting partial implementation detected in draft pull request #{pull_request_number} \
on the deterministic branch.\nChanged paths relative to the mission base:\n{changed_paths}\n\n\
Previously reported remaining work:\n{remaining_work}\n\n\
Before planning or editing, inspect these paths and compare the existing implementation \
with every mission acceptance criterion. Preserve correct completed work, identify what is \
partial or missing, and continue from the current branch state. Do not restart, duplicate, \
or overwrite valid work merely because a worker notebook is unavailable or stale. Treat \
changed paths as evidence of prior work, not proof that the mission is complete.",
        pull_request_number = partial_run.pull_request_number,
        changed_paths = partial_run.changed_paths.join("\n"),
    )
}

pub(super) fn visual_impact_guidance(ticket: &str) -> &'static str {
    let ticket = ticket.to_ascii_lowercase();
    if [
        "theme",
        "dark mode",
        "light mode",
        "design system",
        "color palette",
        "visual system",
    ]
    .iter()
    .any(|needle| ticket.contains(needle))
    {
        "\n\nVisual-system discovery scope: identify the theme provider, selector, centralized \
design-token or CSS-variable source, existing focused tests, and package validation commands. \
If centralized semantic variables are confirmed, inspect at most three representative consumers. \
Do not enumerate every shared UI component unless a targeted search reveals direct hardcoded \
colors. Record the compact impact map as soon as those boundaries are established."
    } else {
        ""
    }
}
