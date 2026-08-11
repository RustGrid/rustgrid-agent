use super::*;
use reqwest::{StatusCode, Url};
fn hosted_production_source() -> &'static str {
    concat!(
        include_str!("authentication.rs"),
        include_str!("contracts.rs"),
        include_str!("control_plane.rs"),
        include_str!("environment.rs"),
        include_str!("errors.rs"),
        include_str!("execution/completion.rs"),
        include_str!("execution/diff_review.rs"),
        include_str!("execution/discovery.rs"),
        include_str!("execution/implementation.rs"),
        include_str!("execution/orchestration.rs"),
        include_str!("execution/planning.rs"),
        include_str!("execution/validation.rs"),
        include_str!("lifecycle_state.rs"),
        include_str!("model_session.rs"),
        include_str!("provider.rs"),
        include_str!("provider_protocol.rs"),
        include_str!("mod.rs"),
        include_str!("recovery.rs"),
        include_str!("publication.rs"),
        include_str!("telemetry.rs"),
        include_str!("tools/filesystem.rs"),
        include_str!("tools/mutation.rs"),
        include_str!("tools/search.rs"),
        include_str!("tools/mod.rs"),
        "
#[cfg(test)]
mod tests;
",
    )
}

use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Mutex,
        mpsc::{self, Receiver},
    },
};

struct ManualHostedClock {
    system_origin: SystemTime,
    instant_origin: Instant,
    elapsed: Mutex<Duration>,
}

impl ManualHostedClock {
    fn new(system_origin: SystemTime) -> Self {
        Self {
            system_origin,
            instant_origin: Instant::now(),
            elapsed: Mutex::new(Duration::ZERO),
        }
    }

    fn advance(&self, duration: Duration) {
        let mut elapsed = self.elapsed.lock().unwrap();
        *elapsed = elapsed.saturating_add(duration);
    }
}

impl HostedClock for ManualHostedClock {
    fn system_now(&self) -> SystemTime {
        self.system_origin + *self.elapsed.lock().unwrap()
    }

    fn instant_now(&self) -> Instant {
        self.instant_origin + *self.elapsed.lock().unwrap()
    }

    fn sleep(&self, duration: Duration) {
        self.advance(duration);
    }
}

#[test]
fn hosted_lease_retries_transient_errors_and_stops_on_permanent_loss() {
    let mut failures = 0;
    assert_eq!(
        reconcile_hosted_heartbeat(
            &mut failures,
            Err(HostedLeaseFailure::Temporary("gateway unavailable".into())),
        ),
        HostedHeartbeatAction::Continue
    );
    assert_eq!(failures, 1);
    assert_eq!(
        reconcile_hosted_heartbeat(
            &mut failures,
            Err(HostedLeaseFailure::Invalidated(
                "lease owner changed".into()
            )),
        ),
        HostedHeartbeatAction::Stop(HostedStopReason::LeaseLost("lease owner changed".into()))
    );
}

#[test]
fn successful_lease_renewal_updates_lease_liveness_without_semantic_progress() {
    let lease_renewed_at = Mutex::new(None);
    let semantic_progress_at = Some("semantic-progress-before-heartbeat".to_owned());

    record_successful_lease_renewal(&lease_renewed_at);

    assert!(lease_renewed_at.lock().unwrap().is_some());
    assert_eq!(
        semantic_progress_at.as_deref(),
        Some("semantic-progress-before-heartbeat")
    );
}

#[test]
fn hosted_lease_loss_suppresses_stale_terminal_writes() {
    let error = anyhow!(HostedLeaseLost {
        operation: "heartbeat",
        detail: "lease owner changed".into(),
    });
    assert!(!may_publish_hosted_terminal_state(&error));
    assert_eq!(
        classify_hosted_execution_failure(&error)
            .expect("typed lease loss")
            .terminal_outcome(),
        crate::error::TerminalOutcome::LeaseLost
    );
}

#[test]
fn orchestration_defects_keep_structured_category_code_phase_and_resumability() {
    let invariant = HostedInvariantFailure::new(
        "successful_mutation_not_reduced",
        "verified repository evidence did not converge",
    );
    assert_eq!(invariant.category(), "OrchestrationStateInvariantFailure");
    assert_eq!(invariant.phase, "orchestration");
    assert!(invariant.resumable);
    let rendered = invariant.to_string();
    assert!(rendered.contains("code=successful_mutation_not_reduced"));
    assert!(rendered.contains("resumable=true"));
    let classified = classify_hosted_execution_failure(&anyhow!(invariant))
        .expect("typed orchestration invariant");
    assert_eq!(
        classified.terminal_outcome(),
        crate::error::TerminalOutcome::Failed
    );
    assert_eq!(
        classified.telemetry_code(),
        crate::error::TelemetryErrorCode::InternalInvariantFailed
    );

    let invariant = HostedInvariantFailure::in_phase(
        "verified_operation_missing_operation_evidence",
        "implementation",
        "the verified operation lost its durable evidence",
    );
    let error = anyhow!(invariant);
    assert_eq!(
        hosted_failure_category(&error),
        "OrchestrationStateInvariantFailure"
    );
    let (code, _) = safe_failure(&error, false);
    assert_eq!(code, "verified_operation_missing_operation_evidence");
    let diagnostics = failure_diagnostics(&error, false);
    assert_eq!(
        diagnostics["category"],
        "OrchestrationStateInvariantFailure"
    );
    assert_eq!(
        diagnostics["code"],
        "verified_operation_missing_operation_evidence"
    );
    assert_eq!(diagnostics["phase"], "implementation");
    assert_eq!(diagnostics["resumable"], true);

    let accounting = HostedRepairAccountingFailure::incompatible_scope(
        "validation repair attempted to borrow mutation fallback capacity",
    );
    assert_eq!(accounting.category(), "RepairAccountingFailure");
    assert_eq!(accounting.code, "incompatible_repair_budget_scope");
    assert_eq!(accounting.phase, "validation_repair");
    assert!(accounting.resumable);
    let classified = classify_hosted_execution_failure(&anyhow!(accounting))
        .expect("typed repair accounting failure");
    assert_eq!(
        classified.terminal_outcome(),
        crate::error::TerminalOutcome::Failed
    );
}

#[test]
fn invariant_resumability_is_resolved_once_for_terminal_projections() {
    let mut failure = test_execution_failure(
        "verified_operation_missing_operation_evidence",
        "operation evidence missing after a coherent write",
    );
    failure.category = "OrchestrationStateInvariantFailure";
    failure.phase = ExecutionPhase::Implementation;
    failure.resume_phase = "implementation".into();
    failure.resume_from_node = Some("source-b".into());
    failure.repository_fingerprint = "tree-after-a".into();
    failure.resumable = true;
    let error = anyhow!(failure);
    let decision = resolve_failure_resumability(
        &error,
        false,
        "verified_operation_missing_operation_evidence",
    );
    assert!(decision.status.is_resumable());
    assert_eq!(
        decision.reason_code,
        "verified_operation_missing_operation_evidence"
    );
    assert_eq!(decision.resume_from_node.as_deref(), Some("source-b"));
    assert_eq!(decision.repository_fingerprint, "tree-after-a");

    let terminal = resolve_unsuccessful_terminal_result(
        Uuid::nil(),
        false,
        "verified_operation_missing_operation_evidence",
        "OrchestrationStateInvariantFailure",
        "operation evidence missing after a coherent write",
        "2026-08-08T00:00:00Z",
        decision.clone(),
    );
    assert_eq!(terminal.resumability, decision.status);
    assert_eq!(terminal.resumability_decision, decision);
    assert_eq!(
        terminal.failure_category.as_deref(),
        Some("OrchestrationStateInvariantFailure")
    );
    assert_eq!(
        terminal.reason_code,
        "verified_operation_missing_operation_evidence"
    );
}

#[test]
fn discovery_action_profiles_restrict_finalization_to_the_compact_forced_tool() {
    assert!(!ToolProgressClass::ActionRedirected.is_failure());
    let finalize = ExecutionDecision::ContinueDiscovery {
        action: crate::hosted_orchestrator::DiscoveryAction::FinalizeImpactMap {
            evidence_ids: vec![crate::execution_graph::EvidenceId::new("evidence-1")],
        },
    };
    let profile =
        ModelActionProfile::for_decision(ExecutionPhase::Discovery, Some(&finalize), 16_384);
    assert_eq!(profile.max_output_tokens, 2_048);
    assert_eq!(profile.reasoning_effort, "low");
    assert_eq!(
        profile.tool_choice(),
        json!({"type": "function", "name": "record_impact_map"})
    );
    let tool_names = hosted_tools_for_action(ExecutionPhase::Discovery, Some(&finalize))
        .into_iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(tool_names, vec!["record_impact_map"]);
    assert!(!discovery_action_permits_tool(Some(&finalize), "read_file"));

    let inspect = ExecutionDecision::ContinueDiscovery {
        action: crate::hosted_orchestrator::DiscoveryAction::InspectRepository {
            inspection_scope: crate::hosted_orchestrator::InspectionScope::default(),
        },
    };
    let inspection_tools = hosted_tools_for_action(ExecutionPhase::Discovery, Some(&inspect))
        .into_iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(
        inspection_tools,
        vec![
            "list_files",
            "read_file",
            "read_files",
            "search_text",
            "related_tests",
        ]
    );
    assert!(!inspection_tools.contains(&"record_impact_map".to_owned()));
}

#[test]
fn planning_action_profiles_force_plan_recording_and_isolate_evidence_reads() {
    let build = ExecutionDecision::ContinuePlanning {
        action: crate::hosted_orchestrator::PlanningAction::BuildPlan {
            impact_map_id: crate::execution_graph::ArtifactId::new("impact-map:tree-1"),
            evidence_ids: vec![crate::execution_graph::EvidenceId::new("evidence-1")],
        },
    };
    let repair = ExecutionDecision::ContinuePlanning {
        action: crate::hosted_orchestrator::PlanningAction::RepairPlan {
            validation_errors: vec![crate::hosted_orchestrator::PlanValidationError {
                path: "$.planned_changes[0].intent".into(),
                message: "intent is required".into(),
            }],
            previous_plan: crate::hosted_orchestrator::PlanArtifact {
                value: json!({"implementation_status": "ready"}),
            },
        },
    };
    for (decision, expected_effort) in [(&build, "medium"), (&repair, "low")] {
        let profile =
            ModelActionProfile::for_decision(ExecutionPhase::Planning, Some(decision), 16_384);
        assert_eq!(profile.max_output_tokens, 4_096);
        assert_eq!(profile.reasoning_effort, expected_effort);
        assert_eq!(
            profile.tool_choice(),
            json!({"type": "function", "name": "record_implementation_plan"})
        );
        let names = hosted_tools_for_action(ExecutionPhase::Planning, Some(decision))
            .into_iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["record_implementation_plan"]);
        assert!(!planning_action_permits_tool(Some(decision), "read_file"));
    }

    let resolve = ExecutionDecision::ContinuePlanning {
        action: crate::hosted_orchestrator::PlanningAction::ResolveEvidenceGap {
            missing_evidence: vec![crate::hosted_orchestrator::MissingEvidenceRequirement {
                path: Some("src/theme.ts".into()),
                reason: "implementation detail is absent".into(),
                ..Default::default()
            }],
        },
    };
    let names = hosted_tools_for_action(ExecutionPhase::Planning, Some(&resolve))
        .into_iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["read_file", "read_files", "search_text", "related_tests"]
    );
    assert!(!planning_action_permits_tool(
        Some(&resolve),
        "record_implementation_plan"
    ));
    assert!(!successful_tool_updates_last_action("read_file", 4, 4));
    assert!(!successful_tool_updates_last_action("read_files", 4, 4));
    assert!(successful_tool_updates_last_action("read_file", 4, 5));
}

#[test]
fn mutation_action_forces_two_exact_path_mutation_tools() {
    let node_id = crate::execution_graph::ExecutionNodeId::new("source-000");
    let target = crate::execution_graph::PlannedTarget {
        change_id: "theme-change".into(),
        path: "src/components/theme/ThemeProvider.tsx".into(),
        role: "production".into(),
        intent: "extend the persisted theme state".into(),
        acceptance_criteria_ids: vec!["ac-1".into()],
        operation: Default::default(),
        new_file: false,
    };
    let decision = ExecutionDecision::ExecuteTarget {
        node_id: node_id.clone(),
        action: crate::hosted_orchestrator::MutationAction::MutateTarget {
            node_id: node_id.clone(),
            target: target.clone(),
            expected_repository_fingerprint: crate::execution_graph::RepositoryFingerprint::new(
                "tree-1",
            ),
        },
        target: crate::execution_graph::TargetExecutionContext {
            node_id,
            change_id: target.change_id.clone(),
            target,
            intent: "extend the persisted theme state".into(),
            acceptance_criteria_ids: vec!["ac-1".into()],
            dependency_evidence: Vec::new(),
            current_file_content: Some("export type Theme = 'dark';".into()),
            target_content_hash: Some(hex::encode(Sha256::digest(b"export type Theme = 'dark';"))),
            target_state_probe: None,
            inspection_outcome: None,
            source_file_content: None,
            source_content_hash: None,
            create_specification: None,
            repository_fingerprint: "tree-1".into(),
            accepted_intent_hash: hex::encode(Sha256::digest(b"extend the persisted theme state")),
            nearby_context: Vec::new(),
            validation_repair: None,
            allowed_tools: vec![crate::execution_graph::ToolKind::ApplyPatch],
            remaining_node_budget: Default::default(),
        },
    };

    let profile =
        ModelActionProfile::for_decision(ExecutionPhase::Implementation, Some(&decision), 16_384);
    assert_eq!(profile.max_output_tokens, 4_096);
    assert_eq!(profile.reasoning_effort, "medium");
    assert_eq!(profile.tool_choice(), json!("required"));

    let tools = hosted_tools_for_action(ExecutionPhase::Implementation, Some(&decision));
    assert_eq!(
        tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>(),
        vec!["apply_patch", "replace_file"]
    );
    for tool in tools {
        assert_eq!(
            tool["parameters"]["properties"]["path"]["enum"],
            json!(["src/components/theme/ThemeProvider.tsx"])
        );
    }

    for (operation, expected_tools) in [
        (
            crate::execution_graph::TargetOperation::CreateNew,
            vec!["create_file"],
        ),
        (
            crate::execution_graph::TargetOperation::DeleteExisting,
            vec!["delete_file"],
        ),
        (
            crate::execution_graph::TargetOperation::Rename {
                source: "src/components/theme/OldThemeProvider.tsx".into(),
                destination: "src/components/theme/ThemeProvider.tsx".into(),
            },
            vec!["rename_file", "move_file"],
        ),
        (
            crate::execution_graph::TargetOperation::Move {
                source: "src/theme/ThemeProvider.tsx".into(),
                destination: "src/components/theme/ThemeProvider.tsx".into(),
            },
            vec!["move_file"],
        ),
    ] {
        let mut operation_decision = decision.clone();
        if let ExecutionDecision::ExecuteTarget {
            action: crate::hosted_orchestrator::MutationAction::MutateTarget { target, .. },
            target: context,
            ..
        } = &mut operation_decision
        {
            target.operation = operation.clone();
            target.new_file = matches!(
                operation,
                crate::execution_graph::TargetOperation::CreateNew
            );
            context.target = target.clone();
        }
        assert_eq!(
            hosted_tools_for_action(ExecutionPhase::Implementation, Some(&operation_decision))
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect::<Vec<_>>(),
            expected_tools
        );
    }

    let ExecutionDecision::ExecuteTarget {
        node_id,
        target: context,
        ..
    } = decision
    else {
        unreachable!()
    };
    let mut failure = crate::execution_graph::FailureRecord::new(
        "failure-1",
        node_id.clone(),
        crate::execution_graph::FailureCategory::MutationConflict,
        1,
        "tree-1",
        "mutation_application_failure:patch_context_mismatch: patch context is stale",
    );
    failure.target_path = Some(context.target.path.clone());
    let repair = ExecutionDecision::ExecuteTarget {
        node_id: node_id.clone(),
        action: crate::hosted_orchestrator::MutationAction::RepairTarget {
            node_id,
            target: context.target.clone(),
            failure: Box::new(failure),
            fallback_policy: MutationFallbackPolicy::ForceReplaceFile,
        },
        target: context,
    };
    let profile = ModelActionProfile::for_decision(ExecutionPhase::Repair, Some(&repair), 16_384);
    assert_eq!(
        profile.tool_choice(),
        json!({"type": "function", "name": "replace_file"})
    );
    assert_eq!(
        hosted_tools_for_action(ExecutionPhase::Repair, Some(&repair))
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>(),
        vec!["replace_file"]
    );
    assert!(
        hosted_agent_instructions_for_decision(ExecutionPhase::Repair, Some(&repair))
            .contains("rejected patch was not applied")
    );
}

#[test]
fn validation_repair_exposes_bounded_mutation_or_typed_no_repair_tools() {
    let node_id = crate::execution_graph::ExecutionNodeId::new("validation-repair-000");
    let validation_node = crate::execution_graph::ExecutionNodeId::new("validation-focused-000");
    let target = crate::execution_graph::PlannedTarget {
        change_id: "theme-provider".into(),
        path: "src/components/theme/ThemeProvider.tsx".into(),
        role: "production".into(),
        intent: "apply all four theme classes consistently".into(),
        acceptance_criteria_ids: vec!["ac-1".into()],
        operation: Default::default(),
        new_file: false,
    };
    let mut failure = crate::execution_graph::FailureRecord::new(
        "validation-failure",
        validation_node,
        crate::execution_graph::FailureCategory::ValidationFailure,
        1,
        "tree-2",
        "focused assertions failed",
    );
    failure.target_path = Some(target.path.clone());
    let decision = ExecutionDecision::ExecuteTarget {
        node_id: node_id.clone(),
        action: crate::hosted_orchestrator::MutationAction::RepairTarget {
            node_id: node_id.clone(),
            target: target.clone(),
            failure: Box::new(failure),
            fallback_policy: MutationFallbackPolicy::NoSafeFallback,
        },
        target: crate::execution_graph::TargetExecutionContext {
            node_id,
            change_id: target.change_id.clone(),
            intent: target.intent.clone(),
            target,
            repository_fingerprint: "tree-2".into(),
            allowed_tools: vec![crate::execution_graph::ToolKind::ApplyPatch],
            ..crate::execution_graph::TargetExecutionContext::default()
        },
    };
    assert_eq!(
        hosted_tools_for_action(ExecutionPhase::Repair, Some(&decision))
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>(),
        vec![
            "apply_patch",
            "replace_file",
            "record_no_valid_repair",
            "record_repair_intent_satisfied",
        ]
    );
    assert!(
        hosted_agent_instructions_for_decision(ExecutionPhase::Repair, Some(&decision))
            .contains("Do not emit a free-form answer")
    );
    assert_eq!(
        decision.budget_node_id().unwrap().as_str(),
        "validation-repair-000"
    );
}

fn mutation_fallback_target(content: &str) -> crate::execution_graph::TargetExecutionContext {
    let target = crate::execution_graph::PlannedTarget {
        change_id: "generic-change".into(),
        path: "src/generic_target.rs".into(),
        role: "production".into(),
        intent: "Apply the accepted repository-agnostic implementation intent.".into(),
        acceptance_criteria_ids: vec!["criterion-1".into()],
        operation: crate::execution_graph::TargetOperation::ModifyExisting,
        new_file: false,
    };
    crate::execution_graph::TargetExecutionContext {
        node_id: crate::execution_graph::ExecutionNodeId::new("source-generic"),
        change_id: target.change_id.clone(),
        intent: target.intent.clone(),
        target,
        current_file_content: Some(content.into()),
        target_content_hash: Some(sha256_text(content)),
        repository_fingerprint: "repository-fingerprint".into(),
        allowed_tools: vec![crate::execution_graph::ToolKind::ApplyPatch],
        ..crate::execution_graph::TargetExecutionContext::default()
    }
}

fn mutation_fallback_decision(
    policy: MutationFallbackPolicy,
    failure_category: MutationApplicationFailure,
) -> ExecutionDecision {
    let target = mutation_fallback_target("fn current() {}\n");
    let node_id = target.node_id.clone();
    let mut failure = crate::execution_graph::FailureRecord::new(
        "mutation-failure",
        node_id.clone(),
        crate::execution_graph::FailureCategory::MutationConflict,
        1,
        "repository-fingerprint",
        "bounded mutation application failure",
    );
    failure.code = Some(failure_category.as_str().into());
    failure.target_path = Some(target.target.path.clone());
    ExecutionDecision::ExecuteTarget {
        node_id: node_id.clone(),
        action: crate::hosted_orchestrator::MutationAction::RepairTarget {
            node_id,
            target: target.target.clone(),
            failure: Box::new(failure),
            fallback_policy: policy,
        },
        target,
    }
}

#[test]
fn patch_failures_select_forced_replacement_for_eligible_existing_targets() {
    let target = mutation_fallback_target("small target\n");
    for failure in [
        MutationApplicationFailure::InvalidPatchTarget,
        MutationApplicationFailure::InvalidPatchSyntax,
        MutationApplicationFailure::PatchContextMismatch,
        MutationApplicationFailure::PatchWouldModifyUnexpectedPath,
    ] {
        assert_eq!(
            crate::hosted_orchestrator::select_fallback_with_threshold(
                &crate::execution_graph::TargetOperation::ModifyExisting,
                failure,
                &target,
                4_096,
            ),
            MutationFallbackPolicy::ForceReplaceFile
        );
    }
}

#[test]
fn large_targets_use_one_bounded_patch_specific_recovery_policy() {
    let target = mutation_fallback_target(&"x".repeat(4_097));
    assert_eq!(
        crate::hosted_orchestrator::select_fallback_with_threshold(
            &crate::execution_graph::TargetOperation::ModifyExisting,
            MutationApplicationFailure::InvalidPatchSyntax,
            &target,
            4_096,
        ),
        MutationFallbackPolicy::RetryPatchWithNormalizedPayload
    );
    assert_eq!(
        crate::hosted_orchestrator::select_fallback_with_threshold(
            &crate::execution_graph::TargetOperation::ModifyExisting,
            MutationApplicationFailure::PatchContextMismatch,
            &target,
            4_096,
        ),
        MutationFallbackPolicy::RetryPatchWithNormalizedPayload
    );
}

#[test]
fn forced_replacement_request_is_exactly_bound_and_passes_preflight() {
    let decision = mutation_fallback_decision(
        MutationFallbackPolicy::ForceReplaceFile,
        MutationApplicationFailure::InvalidPatchTarget,
    );
    let profile = ModelActionProfile::for_decision(ExecutionPhase::Repair, Some(&decision), 16_384);
    let target_context = match &decision {
        ExecutionDecision::ExecuteTarget { target, .. } => target,
        _ => unreachable!(),
    };
    let request = json!({
        "input": [{
            "role": "user",
            "content": serde_json::to_string(&json!({
                "target": target_context,
                "fallback_policy": MutationFallbackPolicy::ForceReplaceFile,
            })).unwrap(),
        }],
        "tools": hosted_tools_for_action(ExecutionPhase::Repair, Some(&decision)),
        "tool_choice": profile.tool_choice(),
    });
    let preflight = mutation_repair_request_preflight(Some(&decision), &request)
        .expect("forced replacement preflight");
    assert!(preflight.passed());
    assert!(preflight.required_content_present);
    assert!(preflight.target_hash_present);
    assert!(preflight.repository_fingerprint_present);
    assert_eq!(request["tools"][0]["name"], "replace_file");
    assert_eq!(request["tool_choice"]["name"], "replace_file");
    assert_eq!(
        request["tools"][0]["parameters"]["properties"]["path"]["enum"],
        json!(["src/generic_target.rs"])
    );
}

#[test]
fn persisted_mutation_fallback_request_exposes_only_the_forced_replacement_tool() {
    let target = mutation_fallback_target("fn current() {}\n");
    let node_id = target.node_id.clone();
    let initial = ExecutionDecision::ExecuteTarget {
        node_id: node_id.clone(),
        action: crate::hosted_orchestrator::MutationAction::MutateTarget {
            node_id: node_id.clone(),
            target: target.target.clone(),
            expected_repository_fingerprint: crate::execution_graph::RepositoryFingerprint::new(
                "repository-fingerprint",
            ),
        },
        target: target.clone(),
    };
    let initial_tools = hosted_tools_for_action(ExecutionPhase::Implementation, Some(&initial));
    assert!(
        initial_tools
            .iter()
            .any(|tool| tool["name"] == "apply_patch")
    );

    let mut failure = crate::execution_graph::FailureRecord::new(
        "failed-apply-patch",
        node_id.clone(),
        crate::execution_graph::FailureCategory::MutationConflict,
        1,
        "repository-fingerprint",
        "initial apply_patch mutation failed",
    );
    failure.code = Some(
        MutationApplicationFailure::PatchContextMismatch
            .as_str()
            .into(),
    );
    failure.target_path = Some(target.target.path.clone());
    let fallback = ExecutionDecision::RepairTarget {
        node_id,
        failure_id: failure.id.clone(),
        context: crate::hosted_orchestrator::TargetRepairContext {
            failure,
            target,
            next_repair_attempt: 1,
            fallback_policy: MutationFallbackPolicy::ForceReplaceFile,
        },
    };
    let profile = ModelActionProfile::for_decision(ExecutionPhase::Repair, Some(&fallback), 16_384);
    let request = json!({
        "tools": hosted_tools_for_action(ExecutionPhase::Repair, Some(&fallback)),
        "tool_choice": profile.tool_choice(),
    });
    let tool_names = request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(tool_names, ["replace_file"]);
    assert!(!tool_names.contains(&"apply_patch"));
    assert_eq!(request["tool_choice"]["name"], "replace_file");
}

#[test]
fn repair_preflight_requires_the_policy_in_the_actual_request_context() {
    let decision = mutation_fallback_decision(
        MutationFallbackPolicy::ForceReplaceFile,
        MutationApplicationFailure::InvalidPatchTarget,
    );
    let profile = ModelActionProfile::for_decision(ExecutionPhase::Repair, Some(&decision), 16_384);
    let target_context = match &decision {
        ExecutionDecision::ExecuteTarget { target, .. } => target,
        _ => unreachable!(),
    };
    let request = json!({
        "input": [{
            "role": "user",
            "content": serde_json::to_string(target_context).unwrap(),
        }],
        "tools": hosted_tools_for_action(ExecutionPhase::Repair, Some(&decision)),
        "tool_choice": profile.tool_choice(),
    });
    let preflight = mutation_repair_request_preflight(Some(&decision), &request)
        .expect("repair preflight result");
    assert!(!preflight.passed());
    assert!(!preflight.policy_present);
    assert!(preflight.required_content_present);
    assert!(preflight.target_hash_present);
    assert!(preflight.repository_fingerprint_present);
    assert!(preflight.tool_surface_matches_policy);
    assert!(preflight.forced_tool_choice_matches_policy);
}

#[test]
fn repair_preflight_rejects_missing_bound_context_before_provider_contact() {
    let decision = mutation_fallback_decision(
        MutationFallbackPolicy::ForceReplaceFile,
        MutationApplicationFailure::InvalidPatchTarget,
    );
    let profile = ModelActionProfile::for_decision(ExecutionPhase::Repair, Some(&decision), 16_384);
    let request = json!({
        "input": [{"role": "user", "content": "replace the file"}],
        "tools": hosted_tools_for_action(ExecutionPhase::Repair, Some(&decision)),
        "tool_choice": profile.tool_choice(),
    });
    let preflight = mutation_repair_request_preflight(Some(&decision), &request)
        .expect("repair preflight result");
    assert!(!preflight.passed());
    assert!(!preflight.required_content_present);
    assert!(!preflight.target_hash_present);
    assert!(!preflight.repository_fingerprint_present);
}

#[test]
fn incompatible_repair_tool_is_rejected_without_touching_repository_or_allowance() {
    let decision = mutation_fallback_decision(
        MutationFallbackPolicy::ForceReplaceFile,
        MutationApplicationFailure::InvalidPatchTarget,
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("target.rs");
    fs::write(&path, "unchanged\n").unwrap();
    let before = fs::read(&path).unwrap();
    let violation = mutation_tool_policy_violation(Some(&decision), "apply_patch")
        .expect("apply_patch must violate forced replacement");
    assert_eq!(
        violation.active_policy,
        MutationFallbackPolicy::ForceReplaceFile
    );
    assert_eq!(violation.expected_tools, ["replace_file"]);
    assert_eq!(fs::read(path).unwrap(), before);

    let node_id = crate::execution_graph::ExecutionNodeId::new("source-generic");
    let mut budget =
        crate::execution_graph::BudgetState::new(crate::execution_graph::MissionBudget::default());
    budget.record_repair_attempt(node_id.clone());
    budget.restore_repair_attempt(&node_id);
    assert_eq!(budget.usage_for(&node_id).mutation_fallback_attempts, 0);
}

#[test]
fn fallback_policy_and_rejected_strategy_are_serializable_audit_evidence() {
    let fingerprint = MutationStrategyFingerprint {
        operation: crate::execution_graph::TargetOperation::ModifyExisting,
        tool: "apply_patch".into(),
        fallback_policy: MutationFallbackPolicy::ForceReplaceFile,
        payload_type: "unified_diff".into(),
        failure_category: MutationApplicationFailure::InvalidPatchTarget,
    };
    let encoded = serde_json::to_value(&fingerprint).unwrap();
    assert_eq!(encoded["tool"], "apply_patch");
    assert_eq!(encoded["fallback_policy"], "force_replace_file");
    assert_eq!(encoded["failure_category"], "invalid_patch_target");
}

#[test]
fn target_attempt_accounting_separates_primary_repair_context_and_write_counts() {
    assert_eq!(
        target_attempt_accounting(2, 1, MutationFallbackPolicy::RebuildTargetContext, 1,),
        TargetAttemptAccounting {
            primary_mutation_calls: 1,
            mutation_repair_calls: 1,
            context_rebuilds: 1,
            repository_write_attempts: 1,
        }
    );
}

#[test]
fn repository_context_rebuild_resolution_survives_threshold_refinement() {
    let target = mutation_fallback_target("small target\n");
    assert_eq!(
        crate::hosted_orchestrator::refine_fallback_for_replacement_threshold(
            MutationFallbackPolicy::ForceReplaceFile,
            &crate::execution_graph::TargetOperation::ModifyExisting,
            MutationApplicationFailure::RepositoryChangedSinceContext,
            &target,
            4_096,
        ),
        MutationFallbackPolicy::ForceReplaceFile
    );
}

#[test]
fn bounded_build_and_repair_plan_requests_fit_the_planning_bootstrap_budget() {
    use crate::execution_graph::{BudgetState, ExecutionNodeId, MissionBudget, NodeBudget};

    let notebook = test_theme_planning_notebook();
    let plan = deterministic_plan_from_impact_map(&notebook).unwrap();
    let decisions = [
        ExecutionDecision::ContinuePlanning {
            action: crate::hosted_orchestrator::PlanningAction::BuildPlan {
                impact_map_id: crate::execution_graph::ArtifactId::new("impact-map:tree-1"),
                evidence_ids: Vec::new(),
            },
        },
        ExecutionDecision::ContinuePlanning {
            action: crate::hosted_orchestrator::PlanningAction::RepairPlan {
                validation_errors: vec![crate::hosted_orchestrator::PlanValidationError {
                    path: "$.planned_changes[0]".into(),
                    message: "repair the invalid fragment".into(),
                }],
                previous_plan: crate::hosted_orchestrator::PlanArtifact {
                    value: serde_json::to_value(plan).unwrap(),
                },
            },
        },
    ];
    let estimates = decisions
        .iter()
        .map(|decision| {
            let profile = ModelActionProfile::for_decision(
                ExecutionPhase::Planning,
                Some(decision),
                16_384,
            );
            estimate_model_call_request_cost(&json!({
                "model": "test-model",
                "input": [{"role": "user", "content": compact_implementation_plan_context(&notebook, Some(decision))}],
                "instructions": hosted_agent_instructions(ExecutionPhase::Planning),
                "max_output_tokens": profile.max_output_tokens,
                "reasoning": {"effort": profile.reasoning_effort},
                "tools": hosted_tools_for_action(ExecutionPhase::Planning, Some(decision)),
                "tool_choice": profile.tool_choice(),
            }))
            .estimated_request_cost
        })
        .collect::<Vec<_>>();
    assert!(estimates.iter().sum::<u64>() <= 300_000, "{estimates:?}");

    let node_id = ExecutionNodeId::new("planning");
    let node_budget = NodeBudget {
        max_model_calls: 2,
        max_cost_micros: 300_000,
        max_duration: Duration::from_secs(90),
        max_mutation_fallback_attempts: 0,
    };
    let mut budget = BudgetState::new(MissionBudget::for_complexity(
        crate::execution_graph::MissionComplexity::Small,
    ));
    budget.record_model_call(node_id.clone(), estimates[0], Duration::from_secs(1));
    let admission = budget.evaluate_model_call_admission(
        &node_id,
        &node_budget,
        1,
        estimates[1],
        Duration::ZERO,
    );
    assert!(admission.admitted, "{admission:?}");
}

#[test]
fn successful_normalization_is_metadata_not_an_artifact_failure() {
    let diagnostic = anyhow!("provider returned a recoverable legacy payload shape");
    let metadata = accepted_artifact_normalization_metadata(
        ArtifactSource::NormalizedModel,
        Some(&diagnostic),
    )
    .unwrap();
    let checkpoint = ArtifactCheckpoint {
        semantic_status: ArtifactSemanticStatus::Sufficient,
        serialization_status: ArtifactSerializationStatus::Normalizable,
        persistence_status: ArtifactPersistenceStatus::Persisted,
        safe_error: None,
        normalization_metadata: Some(metadata.clone()),
        artifact_source: Some(ArtifactSource::NormalizedModel),
        failure_layer: None,
        ..ArtifactCheckpoint::default()
    };
    let checkpoint = serde_json::to_value(checkpoint).unwrap();
    assert!(checkpoint["failure_layer"].is_null());
    assert!(checkpoint["safe_error"].is_null());
    assert_eq!(metadata["normalized"], true);
    assert_eq!(metadata["blocking"], false);
    assert!(
        metadata["original_diagnostic"]
            .as_str()
            .unwrap()
            .contains("legacy payload shape")
    );

    let attempted = impact_map_artifact_attempt_payload(ExecutionPhase::Discovery);
    assert_eq!(attempted["artifact_status"], "attempted");
    assert!(attempted["failure_layer"].is_null());
    assert_eq!(attempted["provider_call_occurred"], true);
}

#[test]
fn compact_finalization_cost_admits_the_third_discovery_call() {
    use crate::execution_graph::{BudgetState, ExecutionNodeId, MissionBudget, NodeBudget};

    let request = json!({
        "model": "test-model",
        "input": [{"role": "user", "content": "persisted evidence summary"}],
        "max_output_tokens": 2_048,
        "reasoning": {"effort": "low"},
        "tools": [{"type": "function", "name": "record_impact_map"}],
        "tool_choice": {"type": "function", "name": "record_impact_map"}
    });
    let estimate = estimate_model_call_request_cost(&request);
    let node_id = ExecutionNodeId::new("discovery");
    let node_budget = NodeBudget {
        max_model_calls: 3,
        max_cost_micros: 350_000,
        max_duration: Duration::from_secs(120),
        max_mutation_fallback_attempts: 0,
    };
    let mut budget = BudgetState::new(MissionBudget::for_complexity(
        crate::execution_graph::MissionComplexity::Small,
    ));
    budget.record_model_call(node_id.clone(), 40_000, Duration::from_secs(1));
    budget.record_model_call(node_id.clone(), 39_070, Duration::from_secs(1));
    let admission = budget.evaluate_model_call_admission(
        &node_id,
        &node_budget,
        1,
        estimate.estimated_request_cost,
        Duration::ZERO,
    );
    assert!(admission.admitted, "{admission:?}");
    assert_eq!(
        admission.projected_node_cost,
        79_070 + estimate.estimated_request_cost
    );
    assert!(admission.projected_node_cost <= admission.node_cost_limit);
}

#[test]
fn cost_rejection_telemetry_exposes_the_complete_failed_inequality() {
    use crate::execution_graph::{BudgetState, ExecutionNodeId, MissionBudget, NodeBudget};

    let request = json!({
        "input": [{"role": "user", "content": "bounded context"}],
        "max_output_tokens": 2_048,
        "reasoning": {"effort": "low"}
    });
    let estimate = estimate_model_call_request_cost(&request);
    let node_id = ExecutionNodeId::new("discovery");
    let mut budget = BudgetState::new(MissionBudget::for_complexity(
        crate::execution_graph::MissionComplexity::Small,
    ));
    budget.record_model_call(node_id.clone(), 340_000, Duration::ZERO);
    let admission = budget.evaluate_model_call_admission(
        &node_id,
        &NodeBudget {
            max_model_calls: 3,
            max_cost_micros: 350_000,
            max_duration: Duration::from_secs(120),
            max_mutation_fallback_attempts: 0,
        },
        1,
        estimate.estimated_request_cost,
        Duration::ZERO,
    );
    assert!(!admission.admitted);
    let telemetry = model_call_admission_telemetry(&admission, &estimate);
    assert_eq!(telemetry["rejection_reason"], "node_cost_budget_exhausted");
    for field in [
        "node_cost_limit",
        "node_cost_consumed",
        "node_cost_reserved",
        "estimated_request_cost",
        "projected_node_cost",
        "input_tokens_estimated",
        "max_output_tokens",
        "reasoning_effort",
        "cost_estimation_method",
    ] {
        assert!(
            !telemetry[field].is_null(),
            "missing telemetry field {field}"
        );
    }
    assert!(
        telemetry["projected_node_cost"].as_u64().unwrap()
            > telemetry["node_cost_limit"].as_u64().unwrap()
    );
}

#[test]
fn orchestration_decision_key_changes_only_after_reconciliation_input_changes() {
    let budget = crate::execution_graph::MissionBudget::for_complexity(
        crate::execution_graph::MissionComplexity::Small,
    );
    let mut snapshot = crate::execution_graph::ExecutionSnapshot {
        graph: crate::execution_graph::ExecutionGraph::bootstrap(
            "graph",
            "tree",
            crate::execution_graph::MissionComplexity::Small,
            &budget,
        ),
        ..crate::execution_graph::ExecutionSnapshot::default()
    };
    let decision = ExecutionDecision::ContinueDiscovery {
        action: crate::hosted_orchestrator::DiscoveryAction::InspectRepository {
            inspection_scope: crate::hosted_orchestrator::InspectionScope::default(),
        },
    };
    let first = execution_decision_idempotency_key(&snapshot, &decision);
    assert_eq!(
        first,
        execution_decision_idempotency_key(&snapshot, &decision)
    );
    assert!(!orchestration_decision_is_new(Some(&first), &first));
    snapshot.graph.revision += 1;
    let after_revision_only = execution_decision_idempotency_key(&snapshot, &decision);
    assert_eq!(first, after_revision_only);
    snapshot.current_repository.fingerprint = "tree-2".into();
    let after_repository_change = execution_decision_idempotency_key(&snapshot, &decision);
    assert_ne!(first, after_repository_change);
    assert!(orchestration_decision_is_new(
        Some(&first),
        &after_repository_change
    ));
}

#[test]
fn fresh_clean_branch_does_not_enter_recovery_publication() {
    let manifest = test_manifest(Uuid::from_u128(0x2201));
    let startup = resolve_startup_mode(&manifest, true, &[]);
    assert_eq!(startup.mode, StartupMode::FreshRun);
    assert!(!startup.persisted_graph_present);
    assert!(!startup.recovery_marker_present);
    let changed = resolve_startup_mode(&manifest, true, &["src/lib.rs".into()]);
    assert_eq!(changed.mode, StartupMode::RecoveryPublicationRun);
    assert!(changed.recovery_marker_present);
}

#[test]
fn fresh_clean_branch_begins_discovery() {
    let manifest = test_manifest(Uuid::from_u128(0x2202));
    let notebook = new_worker_notebook(&manifest, "clean-tree".into(), None);
    let graph = notebook.orchestration.graph.as_ref().unwrap().clone();
    graph
        .validate_invariants()
        .expect("fresh graph passes every graph invariant");
    let initial_collections = graph.derived_collections();
    assert!(initial_collections.remaining_mutation_targets.is_empty());
    assert!(initial_collections.applied_mutation_targets.is_empty());
    let mut snapshot = notebook.orchestration.snapshot(
        "fresh-run",
        crate::execution_graph::RepositorySnapshot {
            fingerprint: "clean-tree".into(),
            source_tree_hash: "clean-tree".into(),
            ..crate::execution_graph::RepositorySnapshot::default()
        },
    );
    snapshot
        .append_event(crate::execution_graph::ExecutionDomainEvent::GraphCreated {
            sequence: 1,
            graph_id: graph.graph_id.clone(),
            revision: graph.revision,
            graph: Some(graph),
            preserved_node_ids: Vec::new(),
        })
        .expect("fresh graph creation is a valid first durable event");
    assert!(
        snapshot
            .graph
            .derived_collections()
            .applied_mutation_targets
            .is_empty(),
        "GraphCreated is orchestration history, not repository mutation evidence"
    );
    let next = notebook
        .orchestration
        .graph
        .as_ref()
        .and_then(|graph| graph.next_runnable_node())
        .expect("fresh graph has a runnable discovery node");
    assert_eq!(
        next.kind,
        crate::execution_graph::ExecutionNodeKind::Discovery
    );
    assert!(
        notebook.orchestration.budget.can_start_model_call_for(next),
        "graph initialization must leave the first discovery model call dispatchable"
    );
    assert_eq!(notebook.phase, ExecutionPhase::Discovery);
}

#[test]
fn fresh_graph_initialization_dispatches_the_first_discovery_request() {
    let work = tempfile::tempdir().expect("temporary repository");
    command::checked("git", ["init", "-q"], work.path()).unwrap();
    command::checked("git", ["config", "user.name", "Test"], work.path()).unwrap();
    command::checked(
        "git",
        ["config", "user.email", "test@example.com"],
        work.path(),
    )
    .unwrap();
    fs::write(work.path().join("base.txt"), "base\n").unwrap();
    command::checked("git", ["add", "base.txt"], work.path()).unwrap();
    command::checked(
        "git",
        ["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"],
        work.path(),
    )
    .unwrap();
    let base_sha = command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap();
    let repo = Repo {
        root: work.path().to_path_buf(),
    };
    let execution_id = Uuid::from_u128(0x2205);
    let mut manifest = test_manifest(execution_id);
    manifest.github.base_sha = base_sha;
    manifest.github.branch = "main".into();
    let Some(StoppableOkServer {
        api_root,
        requests,
        stop,
        handle,
    }) = stoppable_ok_server()
    else {
        return;
    };
    let api = test_api_client(api_root, execution_id);
    let running = Arc::new(AtomicBool::new(true));
    let stop_reason = Arc::new(Mutex::new(None));
    let lease_renewed_at = Arc::new(Mutex::new(None));
    let Ok(containment) = command::HostedProcessContainment::new() else {
        let _ = stop.send(());
        handle.join().unwrap();
        return;
    };
    let trusted_git_config = repo.hosted_local_config().unwrap();
    let mut agent = GatewayAgent::new(
        api,
        &manifest,
        &repo,
        &trusted_git_config,
        &running,
        &stop_reason,
        &lease_renewed_at,
        &containment,
        None,
    )
    .unwrap();
    let startup = StartupModeResolution {
        mode: StartupMode::FreshRun,
        persisted_graph_present: false,
        persisted_notebook_revision: None,
        recovery_marker_present: false,
    };

    agent
        .initialize_fresh_execution_snapshot(&startup, false)
        .expect("fresh graph initialization");
    let result = agent.implement();
    assert!(
        result.is_err(),
        "the empty fixture response should end the session after dispatch"
    );

    let _ = stop.send(());
    handle.join().unwrap();
    let requests = requests.into_iter().collect::<Vec<_>>();
    let graph_checkpoint_index = requests
        .iter()
        .position(|request| request.contains("\"event_type\":\"graph_created\""))
        .expect("GraphCreated must be persisted before provider dispatch");
    let discovery_request_index = requests
        .iter()
        .position(|request| {
            request.starts_with(&format!(
                "POST /api/v1/executions/{execution_id}/ai/responses HTTP/1.1"
            ))
        })
        .expect("discovery request reached the AI gateway");
    let admission_index = requests
        .iter()
        .position(|request| {
            request.contains("\"event_type\":\"worker.model_call_admission_evaluated\"")
                && request.contains("\"node_id\":\"discovery\"")
                && request.contains("\"max_model_calls\":3")
                && request.contains("\"consumed_calls\":0")
                && request.contains("\"reserved_calls\":0")
                && request.contains("\"requested_calls\":1")
                && request.contains("\"admitted\":true")
        })
        .expect("first discovery admission was diagnosed as admitted");
    assert!(graph_checkpoint_index < discovery_request_index);
    assert!(admission_index < discovery_request_index);
    let admission_request = &requests[admission_index];
    for field in [
        "node_cost_limit",
        "node_cost_consumed",
        "node_cost_reserved",
        "estimated_request_cost",
        "projected_node_cost",
        "input_tokens_estimated",
        "max_output_tokens",
        "reasoning_effort",
        "cost_estimation_method",
    ] {
        assert!(
            admission_request.contains(&format!("\"{field}\"")),
            "missing admission diagnostic {field}"
        );
    }
    let discovery_request = &requests[discovery_request_index];
    assert!(discovery_request.contains("\"phase\":\"discovery\""));
    assert!(discovery_request.contains("x-rustgrid-call-phase: discovery"));
    let body = discovery_request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("gateway request body");
    let payload: Value = serde_json::from_str(body).expect("gateway request JSON");
    assert_eq!(payload["max_output_tokens"], 4_096);
    assert_eq!(payload["reasoning"]["effort"], "medium");
    let tool_names = payload["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        tool_names,
        vec![
            "list_files",
            "read_file",
            "read_files",
            "search_text",
            "related_tests",
        ]
    );
}

#[test]
fn orchestration_reconciles_stale_active_nodes_and_bounds_identical_cycles() {
    let work = tempfile::tempdir().expect("temporary repository");
    command::checked("git", ["init", "-q"], work.path()).unwrap();
    command::checked("git", ["config", "user.name", "Test"], work.path()).unwrap();
    command::checked(
        "git",
        ["config", "user.email", "test@example.com"],
        work.path(),
    )
    .unwrap();
    fs::write(work.path().join("base.txt"), "base\n").unwrap();
    command::checked("git", ["add", "base.txt"], work.path()).unwrap();
    command::checked(
        "git",
        ["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"],
        work.path(),
    )
    .unwrap();
    let base_sha = command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap();
    let repo = Repo {
        root: work.path().to_path_buf(),
    };
    let execution_id = Uuid::from_u128(0x2206);
    let mut manifest = test_manifest(execution_id);
    manifest.github.base_sha = base_sha;
    manifest.github.branch = "main".into();
    let Some(StoppableOkServer {
        api_root,
        requests,
        stop,
        handle,
    }) = stoppable_ok_server()
    else {
        return;
    };
    let running = Arc::new(AtomicBool::new(true));
    let stop_reason = Arc::new(Mutex::new(None));
    let lease_renewed_at = Arc::new(Mutex::new(Some("lease-before-progress".into())));
    let Ok(containment) = command::HostedProcessContainment::new() else {
        let _ = stop.send(());
        handle.join().unwrap();
        return;
    };
    let mut agent = GatewayAgent::new(
        test_api_client(api_root, execution_id),
        &manifest,
        &repo,
        &repo.hosted_local_config().unwrap(),
        &running,
        &stop_reason,
        &lease_renewed_at,
        &containment,
        None,
    )
    .unwrap();

    let discovery = agent.reconcile_execution_and_apply().unwrap();
    assert!(matches!(
        discovery.decision,
        ExecutionDecision::ContinueDiscovery { .. }
    ));
    agent.record_discovery_completed().unwrap();
    let planning = agent.reconcile_execution_and_apply().unwrap();
    assert!(matches!(
        planning.decision,
        ExecutionDecision::ContinuePlanning { .. }
    ));
    assert_eq!(agent.phases.active(), ExecutionPhase::Planning);
    assert_eq!(
        agent
            .notebook
            .orchestration
            .worker_liveness
            .lease_renewed_at
            .as_deref(),
        Some("lease-before-progress")
    );

    // The first post-transition pass normalizes the new running-node state.
    // It must not falsely refresh semantic progress when nothing changes.
    agent
        .notebook
        .orchestration
        .worker_liveness
        .last_semantic_progress_at = Some("semantic-progress-before-noop".into());
    agent.reconcile_execution_and_apply().unwrap();
    let progress_after_planning = agent
        .notebook
        .orchestration
        .worker_liveness
        .last_semantic_progress_at
        .clone();
    assert_eq!(
        progress_after_planning.as_deref(),
        Some("semantic-progress-before-noop")
    );
    let duplicate = agent.reconcile_execution_and_apply().unwrap();
    assert!(matches!(
        duplicate.decision,
        ExecutionDecision::ContinuePlanning { .. }
    ));
    assert_eq!(
        agent
            .notebook
            .orchestration
            .worker_liveness
            .last_semantic_progress_at,
        progress_after_planning
    );
    let stopped = agent.reconcile_execution_and_apply().unwrap();
    assert!(matches!(
        stopped.decision,
        ExecutionDecision::StopForGuardrail {
            reason: crate::execution_graph::GuardrailReason::NoProgress,
            ..
        }
    ));
    assert_eq!(
        agent
            .notebook
            .orchestration
            .semantic_cycle_history
            .last()
            .map(|observation| observation.repeated_count),
        Some(crate::execution_graph::MAX_IDENTICAL_DETERMINISTIC_CYCLES)
    );
    assert_eq!(
        agent
            .notebook
            .orchestration
            .cycle_cancellation_request
            .as_ref()
            .map(|request| request.initiator),
        Some(crate::execution_graph::CancellationInitiator::CycleGuardrail)
    );

    let _ = stop.send(());
    handle.join().unwrap();
    let requests = requests.into_iter().collect::<Vec<_>>();
    assert!(
        requests
            .iter()
            .filter(|request| request.contains("worker.active_node_pointer_reconciled"))
            .count()
            <= 1,
        "best-effort stale-pointer telemetry must remain bounded"
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.contains("worker.semantic_decision_deduplicated"))
            .count()
            <= usize::from(crate::execution_graph::MAX_IDENTICAL_DETERMINISTIC_CYCLES),
        "best-effort decision-deduplication telemetry must remain bounded"
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.contains("worker.orchestration_cycle_detected"))
            .count()
            <= 1,
        "best-effort cycle telemetry must remain bounded"
    );
}

#[test]
fn recovery_publication_with_no_diff_is_a_successful_no_op() {
    let snapshot = crate::execution_graph::ExecutionSnapshot::default();
    assert_eq!(
        recovery_publication_no_op(StartupMode::RecoveryPublicationRun, &snapshot),
        Some(RecoveryPublicationResult::SkippedNoDiff)
    );
    assert_eq!(
        recovery_publication_no_op(StartupMode::FreshRun, &snapshot),
        Some(RecoveryPublicationResult::NotApplicable)
    );
}

#[test]
fn resumed_run_with_persisted_graph_resumes_next_node() {
    let mut manifest = test_manifest(Uuid::from_u128(0x2203));
    let mut notebook = new_worker_notebook(&manifest, "persisted-tree".into(), None);
    let graph = notebook.orchestration.graph.as_mut().unwrap();
    graph
        .node_mut(&crate::execution_graph::ExecutionNodeId::new("discovery"))
        .unwrap()
        .status = crate::execution_graph::ExecutionNodeStatus::Completed;
    graph.refresh_readiness();
    notebook.revision = 17;
    manifest.run.metadata["worker_notebook"] = serde_json::to_value(&notebook).unwrap();

    let startup = resolve_startup_mode(&manifest, true, &[]);
    assert_eq!(startup.mode, StartupMode::ResumeRun);
    assert_eq!(startup.persisted_notebook_revision, Some(17));
    let restored = compatible_worker_notebook(&manifest).unwrap();
    let next = restored
        .orchestration
        .graph
        .as_ref()
        .and_then(|graph| graph.next_runnable_node())
        .unwrap();
    assert_eq!(
        next.kind,
        crate::execution_graph::ExecutionNodeKind::Planning
    );
}

#[test]
fn interrupted_run_with_changes_selects_recovery_publication() {
    let mut manifest = test_manifest(Uuid::from_u128(0x2204));
    let mut notebook = new_worker_notebook(&manifest, "changed-tree".into(), None);
    notebook.orchestration.publication.status =
        crate::execution_graph::PublicationStatus::CommitCreated;
    manifest.run.metadata["worker_notebook"] = serde_json::to_value(&notebook).unwrap();

    let startup = resolve_startup_mode(&manifest, true, &["src/lib.rs".into()]);
    assert_eq!(startup.mode, StartupMode::RecoveryPublicationRun);
    let snapshot = crate::execution_graph::ExecutionSnapshot {
        current_repository: crate::execution_graph::RepositorySnapshot {
            changed_paths: BTreeSet::from(["src/lib.rs".into()]),
            ..crate::execution_graph::RepositorySnapshot::default()
        },
        ..crate::execution_graph::ExecutionSnapshot::default()
    };
    assert_eq!(
        recovery_publication_no_op(startup.mode, &snapshot),
        None,
        "a changed interrupted run may proceed to recovery authorization"
    );
}

#[test]
fn provider_not_contacted_is_never_reported_as_ai_gateway_failure() {
    let error = anyhow::Error::new(test_hosted_http_error(
        StatusCode::BAD_REQUEST,
        "request_rejected_before_dispatch",
        None,
        Some(false),
    ));
    assert_eq!(
        hosted_failure_category(&error),
        "orchestration_execution_failed"
    );
}

#[test]
fn mutation_capability_contract_failure_preserves_repair_resume_identity() {
    let error = anyhow!(HostedInvariantFailure::for_node_in_phase(
        "mutation_capability_contract_mismatch",
        "repair",
        "validation-repair-node-1",
        "valid repair mutation event was rejected by its reducer",
    ));
    assert_eq!(
        hosted_failure_category(&error),
        "OrchestrationContractFailure"
    );
    let resumability =
        resolve_failure_resumability(&error, false, "orchestration_execution_failed");
    assert_eq!(
        resumability.reason_code,
        "mutation_capability_contract_mismatch"
    );
    assert_eq!(
        resumability.resume_from_node.as_deref(),
        Some("validation-repair-node-1")
    );
    assert!(matches!(
        resumability.status,
        Resumability::Resumable {
            resume_phase: Some(ref phase)
        } if phase == "repair"
    ));
}

#[test]
fn late_phase_persistence_failures_never_use_initialization_taxonomy() {
    for (kind, category, code, health) in [
        (
            PhasePersistenceFailureKind::Persistence,
            "OrchestrationPersistenceFailure",
            "phase_transition_persistence_failed",
            "degraded",
        ),
        (
            PhasePersistenceFailureKind::Contract,
            "OrchestrationContractFailure",
            "phase_transition_event_invalid",
            "failed",
        ),
    ] {
        let failure = PhasePersistenceFailure {
            kind,
            from_phase: ExecutionPhase::Repair,
            phase: ExecutionPhase::DiffReview,
            safe_error: "bounded diagnostic".into(),
        };
        assert_eq!(failure.category(), category);
        assert_eq!(failure.code(), code);
        assert_eq!(failure.process_health(), health);
        assert_ne!(failure.category(), "orchestration_initialization_failed");
        assert_eq!(
            hosted_failure_category(&anyhow::Error::new(failure)),
            category
        );
    }
}

#[test]
fn startup_failure_telemetry_preserves_the_real_error() {
    let error = anyhow!(HostedStartupFailure {
        category: "execution_graph_initialization_failed",
        code: "execution_graph_initialization_failed",
        message: "The fresh execution snapshot could not be persisted".into(),
        underlying: anyhow!("graph reducer rejected persisted revision 17"),
    });
    let diagnostics = failure_diagnostics(&error, false);
    assert_eq!(
        diagnostics["category"],
        "execution_graph_initialization_failed"
    );
    assert_eq!(diagnostics["provider_contacted"], false);
    assert!(
        diagnostics["underlying_error"]["message"]
            .as_str()
            .unwrap()
            .contains("graph reducer rejected persisted revision 17")
    );
}

#[test]
fn only_execution_decision_adapter_may_transition_hosted_lifecycle() {
    let source = hosted_production_source();
    let obsolete_transition = ["transition", "_phase("].concat();
    let ledger_transition = [".phases", ".transition("].concat();
    assert_eq!(source.matches(&obsolete_transition).count(), 0);
    assert_eq!(source.matches(&ledger_transition).count(), 1);

    let adapter_start = source
        .find("fn apply_execution_decision")
        .expect("hosted decision adapter must exist");
    let adapter_end = adapter_start
        + source[adapter_start..]
            .find("fn record_decision_domain_event")
            .expect("decision adapter must have a bounded source section");
    let transition_offset = source
        .find(&ledger_transition)
        .expect("sole phase-ledger transition must exist");
    assert!((adapter_start..adapter_end).contains(&transition_offset));
}

#[test]
fn production_failures_are_reduced_from_domain_events() {
    let source = hosted_production_source();
    let production = &source[..source
        .rfind("#[cfg(test)]\nmod tests")
        .expect("hosted production source must precede its test module")];
    let compact = production.split_whitespace().collect::<String>();
    for forbidden in [
        ".failures.record(",
        ".failures.mark_recovered(",
        ".failures.mark_superseded(",
        ".failures.supersede_where(",
        ".budget.record_repair_attempt(",
    ] {
        assert!(
            !compact.contains(forbidden),
            "production hosted path directly mutates reducer-owned state via {forbidden}"
        );
    }
    assert!(source.contains("ExecutionDomainEvent::FailureRecorded"));
    assert!(source.contains("ExecutionDomainEvent::FailureRecovered"));
    assert!(source.contains("ExecutionDomainEvent::FailureSuperseded"));
}

#[test]
fn no_safe_fallback_still_emits_policy_selection_observability() {
    let source = hosted_production_source();
    assert!(source.contains("worker.mutation_fallback_policy_selected"));
    assert!(!source.contains("if fallback_policy != MutationFallbackPolicy::NoSafeFallback"));
}

#[test]
fn production_domain_events_rematerialize_notebook_compatibility_fields() {
    let source = hosted_production_source();
    let append_start = source
        .find("fn append_execution_domain_event")
        .expect("domain-event adapter");
    let append_end = append_start
        + source[append_start..]
            .find("fn graph_node_id")
            .expect("bounded domain-event adapter");
    assert!(
        source[append_start..append_end].contains("materialize_legacy_notebook"),
        "every authoritative event must refresh notebook compatibility projections"
    );

    let reconciliation_start = source
        .find("fn reconcile_authoritative_target_state")
        .expect("target reconciliation adapter");
    let reconciliation_end = reconciliation_start
        + source[reconciliation_start..]
            .find("fn reconcile_repository_failure_supersession")
            .expect("bounded target reconciliation adapter");
    let reconciliation = &source[reconciliation_start..reconciliation_end];
    for forbidden in [
        "self.notebook.planned_changes =",
        "self.notebook.intended_changes =",
        "self.notebook.remaining_work =",
        "self.notebook.remaining_work_v2 =",
        "self.notebook.implementation_substate =",
    ] {
        assert!(
            !reconciliation.contains(forbidden),
            "target reconciliation independently mutated projection via {forbidden}"
        );
    }
}

#[test]
fn wall_clock_expiry_routes_partial_work_and_authorized_publication() {
    let model_work = ExecutionDecision::ContinuePlanning {
        action: crate::hosted_orchestrator::PlanningAction::BuildPlan {
            impact_map_id: crate::execution_graph::ArtifactId::new("impact-map:test"),
            evidence_ids: Vec::new(),
        },
    };
    assert_eq!(
        hosted_wall_clock_action(
            false,
            HostedWallClockBoundary::BeforeValidation,
            true,
            &model_work,
        ),
        HostedWallClockAction::Continue
    );
    assert_eq!(
        hosted_wall_clock_action(
            true,
            HostedWallClockBoundary::BeforeValidation,
            true,
            &model_work,
        ),
        HostedWallClockAction::EnterPartialValidation
    );
    assert_eq!(
        hosted_wall_clock_action(
            true,
            HostedWallClockBoundary::BeforeValidation,
            false,
            &model_work,
        ),
        HostedWallClockAction::CompleteBlockedNoDiff
    );

    let publish = ExecutionDecision::Publish {
        mode: crate::execution_graph::PublicationMode::Draft,
    };
    for boundary in [
        HostedWallClockBoundary::PublicationReconciliation,
        HostedWallClockBoundary::PullRequestCreation,
    ] {
        assert_eq!(
            hosted_wall_clock_action(true, boundary, true, &publish),
            HostedWallClockAction::ContinueFinalization
        );
    }
    let published = ExecutionDecision::Finish {
        outcome: OrchestratedMissionOutcome::Complete,
    };
    assert_eq!(
        hosted_wall_clock_action(
            true,
            HostedWallClockBoundary::PullRequestCreation,
            true,
            &published,
        ),
        HostedWallClockAction::ContinueFinalization
    );
    let failed = ExecutionDecision::Finish {
        outcome: OrchestratedMissionOutcome::FailedInfrastructure,
    };
    assert_eq!(
        hosted_wall_clock_action(
            true,
            HostedWallClockBoundary::PullRequestCreation,
            true,
            &failed,
        ),
        HostedWallClockAction::InvalidFinalizationRoute
    );
}

#[test]
fn production_wall_clock_boundaries_use_graph_reconciliation_only() {
    let source = hosted_production_source();
    let production = &source[..source
        .rfind("#[cfg(test)]\nmod tests")
        .expect("hosted production source")];
    assert_eq!(
        production
            .matches("ensure_hosted_execution_deadline(")
            .count(),
        0,
        "no production wall-clock boundary may fail outside graph reconciliation"
    );
    assert_eq!(
        production
            .matches(".reconcile_wall_clock_boundary(")
            .count(),
        3,
        "pre-validation, publication reconciliation, and pull-request creation must all use the graph-aware boundary"
    );
    assert!(!production.contains("hosted_execution_wall_clock_exceeded"));
    assert!(production.contains("worker.wall_clock_partial_validation_authorized"));
    assert!(production.contains("worker.wall_clock_finalization_continued"));
}

#[test]
fn failed_dispatched_calls_are_charged_conservatively_to_the_graph() {
    let request = json!({
        "input": [{"role": "user", "content": "apply the target"}],
        "max_output_tokens": 2_000,
    });
    let (known_input, known_output, known_cost, known_estimated) =
        failed_model_usage_for_accounting(&request, Some(12_345));
    assert_eq!((known_input, known_output, known_cost), (0, 0, 12_345));
    assert!(!known_estimated);

    let (input, output, cost, estimated) = failed_model_usage_for_accounting(&request, None);
    assert!(input > 0);
    assert_eq!(output, 2_000);
    assert_eq!(cost, input.saturating_mul(5) + output.saturating_mul(15));
    assert!(estimated);

    let source = hosted_production_source();
    let production = &source[..source
        .rfind("#[cfg(test)]\nmod tests")
        .expect("hosted production source")];
    assert_eq!(
        production
            .matches("self.observe_failed_model_cost(")
            .count(),
        3,
        "implementation failure, completion failure, and invalid successful-response usage must all reconcile canonical budget usage"
    );
}

#[test]
fn validation_timeout_is_infrastructure_not_target_repair() {
    assert_eq!(
        validation_failure_category("infrastructure_failed"),
        Some(crate::execution_graph::FailureCategory::InfrastructureFailure)
    );
    assert_eq!(
        validation_failure_category("timed_out"),
        Some(crate::execution_graph::FailureCategory::InfrastructureFailure)
    );
    assert_eq!(
        validation_failure_category("failed"),
        Some(crate::execution_graph::FailureCategory::ValidationFailure)
    );
    assert_eq!(validation_failure_category("cancelled"), None);

    let source = hosted_production_source();
    let timeout_message = source
        .find("Validation node exhausted its graph-assigned duration budget.")
        .expect("validation duration guard result");
    let timeout_result = &source[timeout_message.saturating_sub(300)..timeout_message];
    assert!(timeout_result.contains("infrastructure_failed"));
}

#[test]
fn one_validation_infrastructure_retry_runs_without_a_model_call() {
    let directory = tempfile::tempdir().unwrap();
    command::checked("git", ["init", "-q"], directory.path()).unwrap();
    command::checked("git", ["config", "user.name", "Test"], directory.path()).unwrap();
    command::checked(
        "git",
        ["config", "user.email", "test@example.com"],
        directory.path(),
    )
    .unwrap();
    fs::write(directory.path().join("fixture.txt"), "fixture\n").unwrap();
    command::checked("git", ["add", "fixture.txt"], directory.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
        directory.path(),
    )
    .unwrap();
    let execution_id = Uuid::from_u128(0xa0228);
    let mut manifest = test_manifest(execution_id);
    manifest.github.base_sha =
        command::checked("git", ["rev-parse", "HEAD"], directory.path()).unwrap();
    let Some((api_root, requests, server)) = request_sequence_server(
        std::iter::repeat_with(|| ("200 OK", json!({})))
            .take(5)
            .collect(),
    ) else {
        return;
    };
    let api = test_api_client(api_root, execution_id);
    let repo = Repo {
        root: directory.path().to_path_buf(),
    };
    let running = Arc::new(AtomicBool::new(true));
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut ledger = Vec::new();
    let mut required_gates = Vec::new();
    let mut usage = ToolUsage::default();
    let results = run_quality_gates_with_capture(
        &api,
        &manifest,
        &repo,
        &running,
        &manifest.execution_policy,
        1,
        &mut ledger,
        &mut required_gates,
        &mut usage,
        Instant::now(),
        MAX_HOSTED_EXECUTION_DURATION,
        Instant::now(),
        MAX_HOSTED_EXECUTION_DURATION,
        |_, _, _, _, _, _, _| {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(anyhow!(command::CommandFailure::TimedOut { seconds: 1 }))
            } else {
                Ok(command::CommandOutput {
                    status: std::process::Command::new("true").status()?,
                    stdout: "passed after refreshed process capacity".into(),
                    stderr: String::new(),
                })
            }
        },
    )
    .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(results[0].status, "passed");
    assert_eq!(usage.validation_commands, 1);
    server.join().unwrap();
    let delivered = requests.try_iter().collect::<Vec<_>>();
    assert!(delivered.iter().any(|request| {
        request.contains("worker.validation_retry_scheduled")
            && request.contains("model_call_required")
    }));
}

#[test]
fn scheduled_validation_repair_rerun_starts_the_existing_validation_process_once() {
    use crate::execution_graph::{
        BudgetState, ExecutionGraph, ExecutionNodeStatus, MissionBudget, MissionComplexity,
        PlannedTarget as GraphTarget, ValidationGateSpec,
        ValidationGateType as GraphValidationGateType, ValidationRepairBudget,
        ValidationRepairSession, ValidationRepairSessionStatus,
    };

    let directory = tempfile::tempdir().unwrap();
    command::checked("git", ["init", "-q"], directory.path()).unwrap();
    command::checked("git", ["config", "user.name", "Test"], directory.path()).unwrap();
    command::checked(
        "git",
        ["config", "user.email", "test@example.com"],
        directory.path(),
    )
    .unwrap();
    fs::write(directory.path().join("fixture.txt"), "before\n").unwrap();
    command::checked("git", ["add", "fixture.txt"], directory.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
        directory.path(),
    )
    .unwrap();
    let base_sha = command::checked("git", ["rev-parse", "HEAD"], directory.path()).unwrap();
    fs::write(directory.path().join("fixture.txt"), "after repair\n").unwrap();

    let execution_id = Uuid::from_u128(0xa0229);
    let mut manifest = test_manifest(execution_id);
    manifest.github.base_sha = base_sha.clone();
    manifest.execution_policy.quality_gates = vec![HostedQualityGate {
        id: "test".into(),
        command: "true".into(),
        timeout_seconds: 30,
        required: true,
    }];
    let repo = Repo {
        root: directory.path().to_path_buf(),
    };
    let repository_fingerprint = repository_state_fingerprint(&repo, &base_sha).unwrap();
    let mission_budget = MissionBudget::for_complexity(MissionComplexity::Small);
    let mut graph = ExecutionGraph::from_targets(
        "validation-rerun",
        MissionComplexity::Small,
        &repository_fingerprint,
        &[GraphTarget {
            change_id: "repair-target".into(),
            path: "fixture.txt".into(),
            role: "production".into(),
            intent: "repair the failed validation".into(),
            acceptance_criteria_ids: vec!["ac-1".into()],
            operation: Default::default(),
            new_file: false,
        }],
        &[ValidationGateSpec {
            gate_id: "test".into(),
            gate_type: GraphValidationGateType::TestSuite,
            command: "true".into(),
            working_directory: directory.path().to_string_lossy().into_owned(),
            required: true,
            ..ValidationGateSpec::default()
        }],
        &mission_budget,
    );
    let mutation_node = graph
        .nodes
        .iter()
        .find(|node| node.kind.is_mutation())
        .unwrap()
        .id
        .clone();
    graph
        .set_node_status(&mutation_node, ExecutionNodeStatus::Completed)
        .unwrap();
    let validation_node = graph
        .nodes
        .iter()
        .find(|node| node.kind.is_validation())
        .unwrap()
        .id
        .clone();
    let gate = graph
        .node(&validation_node)
        .unwrap()
        .validation
        .clone()
        .unwrap();

    let mut notebook = new_worker_notebook(&manifest, repository_fingerprint.clone(), None);
    notebook.phase = ExecutionPhase::Repair;
    notebook.orchestration.graph_revision = graph.revision;
    notebook.orchestration.graph = Some(graph);
    notebook.orchestration.legacy_import_completed = true;
    notebook.orchestration.budget = BudgetState::new(mission_budget);
    notebook
        .orchestration
        .budget
        .validation_repair_sessions
        .insert(
            "validation-repair-session".into(),
            ValidationRepairSession {
                session_id: "validation-repair-session".into(),
                failed_validation_id: "failed-test-r1".into(),
                originating_gate_id: validation_node.clone(),
                budget: ValidationRepairBudget {
                    max_model_calls: 2,
                    max_target_attempts: 1,
                    max_repository_writes: 1,
                    max_context_rebuilds: 1,
                    max_cost_micros: 1,
                },
                status: ValidationRepairSessionStatus::ReadyForRerun,
                current_assertion_set_revision: 1,
                ..ValidationRepairSession::default()
            },
        );
    manifest.run.metadata["worker_notebook"] = serde_json::to_value(&notebook).unwrap();

    let Some(StoppableOkServer {
        api_root,
        requests,
        stop,
        handle,
    }) = stoppable_ok_server()
    else {
        return;
    };
    let api = test_api_client(api_root, execution_id);
    let running = Arc::new(AtomicBool::new(true));
    api.append_event(
        "validation",
        json!({
            "event_type": "worker.validation_rerun_scheduled",
            "repair_session_id": "validation-repair-session",
            "originating_validation_gate": validation_node,
            "failure_revision": 1,
            "command": gate.command,
            "repository_fingerprint": repository_fingerprint,
        }),
    )
    .unwrap();
    let mut current_decision = Some(ExecutionDecision::RunValidation {
        node_id: validation_node.clone(),
        gate,
    });
    let scheduled = recovery::take_scheduled_validation_rerun(
        &mut current_decision,
        &notebook.orchestration.budget.validation_repair_sessions,
    )
    .expect("successful repair must hand its scheduled gate to the validation runner");
    let ExecutionDecision::RunValidation { gate, .. } = scheduled else {
        unreachable!("scheduled repair handoff is a validation decision")
    };
    assert!(current_decision.is_none());

    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut ledger = Vec::new();
    let mut required_gates = Vec::new();
    let mut usage = ToolUsage::default();
    let results = run_quality_gates_with_capture(
        &api,
        &manifest,
        &repo,
        &running,
        &HostedExecutionPolicy {
            quality_gates: vec![HostedQualityGate {
                id: gate.gate_id,
                command: gate.command,
                timeout_seconds: 30,
                required: true,
            }],
            ..manifest.execution_policy.clone()
        },
        2,
        &mut ledger,
        &mut required_gates,
        &mut usage,
        Instant::now(),
        MAX_HOSTED_EXECUTION_DURATION,
        Instant::now(),
        MAX_HOSTED_EXECUTION_DURATION,
        |_, _, _, _, _, _, _| {
            attempts.fetch_add(1, Ordering::SeqCst);
            Ok(command::CommandOutput {
                status: std::process::Command::new("true").status()?,
                stdout: String::new(),
                stderr: String::new(),
            })
        },
    );
    let _ = stop.send(());
    handle.join().unwrap();
    let results = results.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, "passed");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let requests = requests.try_iter().collect::<Vec<_>>();
    assert!(
        requests
            .iter()
            .any(|request| request.contains("worker.validation_rerun_scheduled"))
    );
    assert!(
        requests
            .iter()
            .any(|request| request.contains("worker.validation_process_started"))
    );
    assert!(
        requests
            .iter()
            .any(|request| request.contains("worker.validation_process_completed"))
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.contains("worker.orchestration_cycle_detected"))
    );
}

#[test]
fn validation_failure_target_hint_requires_one_explicit_planned_path() {
    let targets = vec!["src/first.rs".to_owned(), "src/last.rs".to_owned()];
    assert_eq!(
        validation_failure_target_hint(&targets, "assertion failed without a file path"),
        None,
        "an unmapped failure must not inherit the last successful target"
    );
    assert_eq!(
        validation_failure_target_hint(
            &targets,
            "error in src/first.rs: expected true, received false"
        )
        .as_deref(),
        Some("src/first.rs")
    );
    assert_eq!(
        validation_failure_target_hint(
            &targets,
            "src/first.rs imports src/last.rs with an invalid contract"
        ),
        None,
        "multiple distinct matches require explicit blocking or classified repair"
    );
    assert_eq!(
        validation_failure_target_hint(&targets, "first.rs failed"),
        None,
        "a basename-only diagnostic is not an exact planned-path match"
    );

    let duplicate_path_targets = vec!["src/shared.rs".to_owned(), "src/shared.rs".to_owned()];
    assert_eq!(
        validation_failure_target_hint(
            &duplicate_path_targets,
            "validation failed in src/shared.rs"
        )
        .as_deref(),
        Some("src/shared.rs"),
        "the orchestrator must receive the shared path and reject its ambiguous node identity"
    );
}

#[test]
fn vitest_assertions_are_structured_and_select_an_implicated_source_target() {
    let output = r#"[31m
 FAIL  tests/theme-provider.test.tsx > ThemeProvider > applies the light-blue root class
AssertionError: expected 'theme-root' to contain 'light-blue'
Expected: "light-blue"
Received: "theme-root"
 ❯ tests/theme-provider.test.tsx:41:38

 FAIL  tests/theme-provider.test.tsx > ThemeProvider > restores the saved light-blue root class
AssertionError: expected 'theme-root' to contain 'light-blue'
Expected: "light-blue"
Received: "theme-root"
 ❯ tests/theme-provider.test.tsx:58:38

 FAIL  tests/theme-provider.test.tsx > ThemeProvider > cycles through four themes
AssertionError: expected 'light-blue' to be 'red'
Expected: "red"
Received: "light-blue"
 ❯ tests/theme-provider.test.tsx:77:26
[0m"#;
    let candidates = vec![
        (
            "src/components/theme/ThemeProvider.tsx".into(),
            "const themes = ['dark', 'light', 'light-blue', 'red']; document.documentElement.classList.add(theme);".into(),
        ),
        (
            "src/components/theme/ThemeToggle.tsx".into(),
            "const cycle = ['dark', 'light', 'light-blue', 'red'];".into(),
        ),
        (
            "tests/theme-provider.test.tsx".into(),
            "expect(root.className).toContain('light-blue'); expect(theme).toBe('red');".into(),
        ),
    ];
    let assertions = parse_validation_assertion_failures(
        "npx vitest run tests/theme-provider.test.tsx",
        output,
        &candidates,
    );
    assert_eq!(assertions.len(), 3);
    assert_eq!(assertions[0].expected, "light-blue");
    assert_eq!(assertions[0].suite_path, ["ThemeProvider"]);
    assert_eq!(assertions[0].test_name, "applies the light-blue root class");
    assert_eq!(assertions[0].source_line, Some(41));
    assert_eq!(assertions[0].source_column, Some(38));
    assert_eq!(assertions[0].received, "theme-root");
    assert_eq!(assertions[2].expected, "red");
    assert_eq!(assertions[2].received, "light-blue");
    assert!(
        assertions[2]
            .implicated_paths
            .contains(&"src/components/theme/ThemeToggle.tsx".to_owned())
    );
    let targets = candidates
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        validation_repair_target_hint(&assertions, &targets, &candidates).as_deref(),
        Some("src/components/theme/ThemeToggle.tsx")
    );
}

#[test]
fn failed_validation_summary_preserves_large_suite_failure_tail_for_parser() {
    let mut stdout = String::new();
    for index in 0..2_000 {
        stdout.push_str(&format!(
            " PASS  tests/passing-{index}.test.ts > passing suite > passes\n"
        ));
    }
    stdout.push_str(
        r#"
 FAIL  tests/state-machine.test.ts > state machine > reaches the completed state
AssertionError: expected 'running' to be 'completed'
Expected: "completed"
Received: "running"
 ❯ tests/state-machine.test.ts:42:19
"#,
    );

    let summary = validation_output_summary(&stdout, "", false, 16_000);
    assert!(summary.len() <= 16_000);
    let assertions = parse_validation_assertion_failures(
        "npx vitest run",
        &summary,
        &[(
            "tests/state-machine.test.ts".into(),
            "expect(state).toBe('completed');".into(),
        )],
    );
    assert_eq!(assertions.len(), 1);
    assert_eq!(assertions[0].test_name, "reaches the completed state");
    assert_eq!(assertions[0].source_line, Some(42));
    assert!(
        assertions[0]
            .implicated_paths
            .contains(&"tests/state-machine.test.ts".to_owned())
    );
}

#[test]
fn vitest_fallback_preserves_a_bounded_structured_failure_when_header_is_unknown() {
    let output = concat!(
        "\u{1b}[31m",
        " FAIL  tests/theme-provider.test.tsx > ThemeProvider > cycles through all themes\n",
        "Error: values differ\n",
        "Expected: \"red\"\n",
        "Received: \"light-blue\"\n",
        " ❯ tests/theme-provider.test.tsx:111:27\n",
        "\u{1b}[0m"
    );
    let candidates = vec![(
        "tests/theme-provider.test.tsx".into(),
        "expect(label).toBe('red');".into(),
    )];
    assert!(looks_like_structured_test_failure(output));
    assert!(
        parse_validation_assertion_failures(
            "npx vitest run tests/theme-provider.test.tsx",
            output,
            &candidates
        )
        .is_empty()
    );
    let fallback = fallback_validation_assertion_failure(
        "npx vitest run tests/theme-provider.test.tsx",
        output,
        &candidates,
    )
    .expect("fallback assertion");
    assert_eq!(fallback.suite_path, ["ThemeProvider"]);
    assert_eq!(fallback.test_name, "cycles through all themes");
    assert_eq!(fallback.expected, "red");
    assert_eq!(fallback.received, "light-blue");
    assert_eq!(fallback.source_line, Some(111));
    assert_eq!(fallback.source_column, Some(27));
    assert!(fallback.context.len() <= output.len());
}

#[test]
fn repair_target_ranking_depends_on_evidence_not_ticket_vocabulary_or_filenames() {
    let output = r#"
 FAIL  checks/state-machine.spec.ts > arbitrary suite > advances the selected state
AssertionError: expected 'intermediate' to be 'complete'
Expected: "complete"
Received: "intermediate"
 ❯ checks/state-machine.spec.ts:29:17
"#;
    let candidates = vec![
        (
            "lib/catalog.ts".into(),
            "export const states = ['initial', 'intermediate', 'complete'];".into(),
        ),
        (
            "lib/presenter.ts".into(),
            "export const advance = () => current === 'intermediate' ? 'complete' : current;"
                .into(),
        ),
        (
            "checks/state-machine.spec.ts".into(),
            "import { advance } from '../lib/presenter';\nexpect(advance()).toBe('complete');"
                .into(),
        ),
    ];
    let assertions = parse_validation_assertion_failures(
        "npx vitest run checks/state-machine.spec.ts",
        output,
        &candidates,
    );
    let targets = candidates
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        validation_repair_target_hint(&assertions, &targets, &candidates).as_deref(),
        Some("lib/presenter.ts")
    );
    assert_eq!(
        structured_validation_paths(output),
        BTreeSet::from(["checks/state-machine.spec.ts".to_owned()])
    );
}

fn recovery_authorization_fixture() -> (
    HostedManifest,
    crate::execution_graph::ExecutionSnapshot,
    crate::execution_graph::ExecutionNodeId,
    String,
) {
    use crate::execution_graph::{
        BudgetState, EvidenceStore, ExecutionNodeKind, ExecutionNodeStatus, ExecutionSnapshot,
        MissionBudget, MissionComplexity, PlannedTarget as GraphTarget, RepositorySnapshot,
        ValidationEvidenceRecord, ValidationEvidenceStatus, ValidationGateSpec,
        ValidationGateType as GraphValidationGateType, build_execution_graph,
    };

    let execution_id = Uuid::from_u128(0x2260_0000_0000_4000_8000_0000_0000_00d1);
    let manifest = test_manifest(execution_id);
    let repository_fingerprint = "recovery-tree-current".to_owned();
    let budget = MissionBudget::for_complexity(MissionComplexity::Small);
    let mut graph = build_execution_graph(
        "recovery-publication-fixture",
        MissionComplexity::Small,
        &repository_fingerprint,
        &[GraphTarget {
            change_id: "change-1".into(),
            path: "src/recovery.rs".into(),
            role: "production".into(),
            intent: "preserve validated recovery work".into(),
            acceptance_criteria_ids: vec!["ac-1".into()],
            operation: Default::default(),
            new_file: true,
        }],
        &[ValidationGateSpec {
            gate_id: "test".into(),
            gate_type: GraphValidationGateType::TestSuite,
            command: "cargo test".into(),
            working_directory: String::new(),
            required: true,
            ..ValidationGateSpec::default()
        }],
        &budget,
    );
    for node in &mut graph.nodes {
        node.status = match node.kind {
            ExecutionNodeKind::Discovery | ExecutionNodeKind::Planning => {
                ExecutionNodeStatus::Completed
            }
            ExecutionNodeKind::DiffReview
            | ExecutionNodeKind::CompletionEvaluation
            | ExecutionNodeKind::Publication => ExecutionNodeStatus::Pending,
            kind if kind.is_mutation() => ExecutionNodeStatus::Completed,
            kind if kind.is_validation() => ExecutionNodeStatus::Passed,
            _ => node.status,
        };
    }
    graph.refresh_readiness();
    let validation_node = graph
        .nodes
        .iter()
        .find(|node| node.kind.is_validation())
        .expect("validation node")
        .clone();
    let publication_node = graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::Publication)
        .expect("publication node")
        .id
        .clone();
    let gate = validation_node
        .validation
        .as_ref()
        .expect("validation gate");
    let evidence_id = "recovery-validation-current".to_owned();
    graph
        .node_mut(&validation_node.id)
        .expect("validation node")
        .evidence_ids
        .push(evidence_id.clone());
    let mut evidence = EvidenceStore::default();
    evidence.record_validation(ValidationEvidenceRecord {
        evidence_id: evidence_id.clone(),
        node_id: validation_node.id.clone(),
        gate_id: gate.gate_id.clone(),
        fingerprint: gate.fingerprint(&repository_fingerprint),
        repository_fingerprint: repository_fingerprint.clone(),
        command: gate.command.clone(),
        working_directory: gate.working_directory.clone(),
        status: ValidationEvidenceStatus::Passed,
        exit_code: Some(0),
        output_summary: "passed".into(),
        duration: Duration::from_millis(1),
    });
    let snapshot = ExecutionSnapshot {
        run_id: "recovery-run".into(),
        current_repository: RepositorySnapshot {
            fingerprint: repository_fingerprint.clone(),
            source_tree_hash: repository_fingerprint,
            changed_paths: BTreeSet::from(["src/recovery.rs".to_owned()]),
            ..RepositorySnapshot::default()
        },
        graph,
        evidence,
        budget: BudgetState::new(budget),
        ..ExecutionSnapshot::default()
    };
    (manifest, snapshot, publication_node, evidence_id)
}

#[test]
fn validated_live_diff_authorizes_canonical_draft_recovery() {
    use crate::execution_graph::{ExecutionDomainEvent, ExecutionNodeKind, PublicationMode};

    let (manifest, mut snapshot, publication_node, evidence_id) = recovery_authorization_fixture();
    let authorization = authorize_recovery_publication(&snapshot, &manifest).unwrap();
    assert_eq!(authorization.publication_node_id, publication_node);
    assert_eq!(authorization.validation_evidence_ids, [evidence_id]);
    assert_eq!(authorization.changed_paths, ["src/recovery.rs"]);
    assert!(!authorization.already_requested);

    let review_before = snapshot
        .graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::DiffReview)
        .unwrap()
        .status;
    let completion_before = snapshot
        .graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
        .unwrap()
        .status;
    snapshot
        .append_event(ExecutionDomainEvent::RecoveryPublicationRequested {
            sequence: snapshot.next_event_sequence(),
            node_id: authorization.publication_node_id,
            repository_fingerprint: authorization.repository_fingerprint,
            validation_evidence_ids: authorization.validation_evidence_ids,
        })
        .unwrap();
    assert_eq!(
        snapshot.publication.mode,
        Some(PublicationMode::DraftRecovery)
    );
    assert!(snapshot.publication.draft);
    assert!(snapshot.publication.recovery_requested);
    assert_eq!(
        snapshot
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::DiffReview)
            .unwrap()
            .status,
        review_before
    );
    assert_eq!(
        snapshot
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .unwrap()
            .status,
        completion_before
    );
}

#[test]
fn recovery_publication_rejects_zero_diff_stale_validation_and_terminal_state() {
    use crate::execution_graph::{
        ExecutionDomainEvent, FailureCategory, FailureRecord, MissionOutcome, PublicationMode,
    };

    let (manifest, snapshot, publication_node, evidence_id) = recovery_authorization_fixture();

    let mut zero_diff = snapshot.clone();
    zero_diff.current_repository.changed_paths.clear();
    assert!(authorize_recovery_publication(&zero_diff, &manifest).is_err());

    let mut stale = snapshot.clone();
    stale
        .evidence
        .validations
        .get_mut(&evidence_id)
        .unwrap()
        .repository_fingerprint = "stale-tree".into();
    assert!(authorize_recovery_publication(&stale, &manifest).is_err());

    let mut terminal = snapshot.clone();
    terminal.events.push(ExecutionDomainEvent::RunFinished {
        sequence: 1,
        outcome: MissionOutcome::PartialReviewable,
    });
    assert!(authorize_recovery_publication(&terminal, &manifest).is_err());

    let mut resumed_partial = terminal;
    resumed_partial
        .events
        .push(ExecutionDomainEvent::ExecutionResumed {
            sequence: 2,
            execution_attempt: 2,
            previous_outcome: Some(MissionOutcome::PartialReviewable),
        });
    assert!(authorize_recovery_publication(&resumed_partial, &manifest).is_ok());

    let mut infrastructure = snapshot.clone();
    let infrastructure_failure_id = crate::execution_graph::FailureId::new("infra-failure");
    infrastructure.failures.records.push(FailureRecord::new(
        infrastructure_failure_id.clone(),
        publication_node,
        FailureCategory::InfrastructureFailure,
        1,
        infrastructure.current_repository.fingerprint.clone(),
        "lease transport failed",
    ));
    assert!(authorize_recovery_publication(&infrastructure, &manifest).is_err());
    infrastructure
        .events
        .push(ExecutionDomainEvent::GuardrailTriggered {
            sequence: 1,
            reason: crate::execution_graph::GuardrailReason::InfrastructureFailure,
            outcome: MissionOutcome::PartialReviewable,
            detail: "validation process timed out after its model-free retry".into(),
        });
    let partial_authorization = authorize_recovery_publication(&infrastructure, &manifest).expect(
        "applied diffs with only validation infrastructure incomplete are draft-publishable",
    );
    assert!(
        infrastructure
            .append_event(ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: 2,
                node_id: partial_authorization.publication_node_id,
                repository_fingerprint: partial_authorization.repository_fingerprint,
                validation_evidence_ids: partial_authorization.validation_evidence_ids,
            })
            .is_ok(),
        "applied diffs with only validation infrastructure incomplete are draft-publishable"
    );
    infrastructure
        .failures
        .mark_recovered(&infrastructure_failure_id, "recovered-tree");
    assert!(authorize_recovery_publication(&infrastructure, &manifest).is_ok());

    let mut normal_publication = snapshot.clone();
    normal_publication.publication.mode = Some(PublicationMode::Normal);
    normal_publication.publication.status =
        crate::execution_graph::PublicationStatus::CommitCreated;
    normal_publication.publication.commit_sha = Some("c".repeat(40));
    assert!(authorize_recovery_publication(&normal_publication, &manifest).is_ok());

    let mut completed_publication = snapshot;
    completed_publication.publication.status =
        crate::execution_graph::PublicationStatus::PullRequestCreated;
    assert!(authorize_recovery_publication(&completed_publication, &manifest).is_err());
}

#[test]
fn orchestration_recovery_wrapper_is_narrow_and_one_shot() {
    let invariant = anyhow!(HostedInvariantFailure::new(
        "illegal_lifecycle_transition",
        "illegal hosted lifecycle transition from repair to validation",
    ))
    .context("hosted execution failed");
    assert!(is_hosted_orchestration_invariant_error(&invariant));
    assert!(!is_hosted_orchestration_invariant_error(&anyhow!(
        "required validation command failed"
    )));

    let source = hosted_production_source();
    let production = &source[..source
        .rfind("#[cfg(test)]\nmod tests")
        .expect("hosted production source")];
    assert_eq!(
        production
            .matches("attempt_safe_recovery_publication(")
            .count(),
        3,
        "the recovery helper must have one definition and only the explicit startup and invariant-recovery call sites"
    );
    let recovery_start = production
        .find("fn attempt_safe_recovery_publication")
        .expect("recovery helper");
    let recovery_end = recovery_start
        + production[recovery_start..]
            .find("struct CancellationBranchPreservation")
            .expect("bounded recovery helper section");
    assert!(
        !production[recovery_start..recovery_end]
            .contains("ensure_active_or_checkpoint_cancellation")
    );
}

#[test]
fn fresh_run_infrastructure_timeout_publishes_draft_and_finishes_partial() {
    use crate::execution_graph::{
        BudgetState, EvidenceStore, ExecutionDomainEvent, ExecutionNodeKind, ExecutionNodeStatus,
        ExecutionSnapshot, FailureCategory, FailureId, FailureRecord, GuardrailReason,
        MissionBudget, MissionComplexity, MissionOutcome, PlannedTarget as GraphTarget,
        PublicationMode, PublicationStatus, RepositorySnapshot, ValidationEvidenceRecord,
        ValidationEvidenceStatus, ValidationGateSpec,
        ValidationGateType as GraphValidationGateType, build_execution_graph,
    };

    let work = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    command::checked(
        "git",
        ["init", "--bare", "-q", remote.path().to_str().unwrap()],
        work.path(),
    )
    .unwrap();
    command::checked("git", ["init", "-q"], work.path()).unwrap();
    command::checked("git", ["config", "user.name", "Recovery Test"], work.path()).unwrap();
    command::checked(
        "git",
        ["config", "user.email", "recovery@example.com"],
        work.path(),
    )
    .unwrap();
    fs::write(work.path().join("base.txt"), "base\n").unwrap();
    command::checked("git", ["add", "."], work.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
        work.path(),
    )
    .unwrap();
    command::checked("git", ["branch", "-M", "main"], work.path()).unwrap();
    let hosted_origin = "https://github.example/RustGrid/example.git";
    command::checked(
        "git",
        ["remote", "add", "origin", hosted_origin],
        work.path(),
    )
    .unwrap();
    let local_remote = format!("file://{}", remote.path().display());
    let rewrite_key = format!("url.{local_remote}.insteadOf");
    command::checked(
        "git",
        ["config", "--local", rewrite_key.as_str(), hosted_origin],
        work.path(),
    )
    .unwrap();
    command::checked("git", ["push", "-q", "origin", "main"], work.path()).unwrap();
    command::checked(
        "git",
        ["config", "--local", "--unset-all", rewrite_key.as_str()],
        work.path(),
    )
    .unwrap();
    let base_sha = command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap();
    let branch = "rustgrid/recovery-publication";
    command::checked("git", ["switch", "-q", "-c", branch], work.path()).unwrap();
    fs::create_dir_all(work.path().join("src")).unwrap();
    fs::write(
        work.path().join("src/recovery.rs"),
        "pub fn recovered() -> bool { true }\n",
    )
    .unwrap();

    let repo = Repo {
        root: work.path().to_path_buf(),
    };
    let repository_fingerprint = repository_state_fingerprint(&repo, &base_sha).unwrap();
    let execution_id = Uuid::from_u128(0x2260_0000_0000_4000_8000_0000_0000_00d2);
    let mut manifest = test_manifest(execution_id);
    manifest.github.base_sha = base_sha.clone();
    manifest.github.branch = branch.into();
    manifest.github.web_base_url = "https://github.example".into();
    manifest.github.clone_url = hosted_origin.into();
    let budget = MissionBudget::for_complexity(MissionComplexity::Small);
    let mut graph = build_execution_graph(
        "production-recovery-publication",
        MissionComplexity::Small,
        &repository_fingerprint,
        &[GraphTarget {
            change_id: "recovery-change".into(),
            path: "src/recovery.rs".into(),
            role: "production".into(),
            intent: "preserve the validated recovery change".into(),
            acceptance_criteria_ids: vec!["ac-1".into()],
            operation: Default::default(),
            new_file: true,
        }],
        &[ValidationGateSpec {
            gate_id: "test".into(),
            gate_type: GraphValidationGateType::TestSuite,
            command: "cargo test".into(),
            working_directory: work.path().to_string_lossy().into_owned(),
            required: true,
            dependency_lock_hash: dependency_lock_fingerprint(work.path()).unwrap(),
            relevant_environment_fingerprint: relevant_environment_fingerprint(
                &manifest.execution_policy,
            )
            .unwrap(),
        }],
        &budget,
    );
    for node in &mut graph.nodes {
        node.status = match node.kind {
            ExecutionNodeKind::Discovery | ExecutionNodeKind::Planning => {
                ExecutionNodeStatus::Completed
            }
            kind if kind.is_mutation() => ExecutionNodeStatus::Applied,
            kind if kind.is_validation() => ExecutionNodeStatus::FailedRecoverable,
            _ => ExecutionNodeStatus::Pending,
        };
    }
    graph.refresh_readiness();
    let validation_node = graph
        .nodes
        .iter()
        .find(|node| node.kind.is_validation())
        .unwrap()
        .clone();
    let gate = validation_node.validation.as_ref().unwrap();
    let evidence_id = "production-recovery-validation".to_owned();
    graph
        .node_mut(&validation_node.id)
        .unwrap()
        .evidence_ids
        .push(evidence_id.clone());
    let mut evidence = EvidenceStore::default();
    evidence.record_validation(ValidationEvidenceRecord {
        evidence_id,
        node_id: validation_node.id.clone(),
        gate_id: gate.gate_id.clone(),
        fingerprint: gate.fingerprint(&repository_fingerprint),
        repository_fingerprint: repository_fingerprint.clone(),
        command: gate.command.clone(),
        working_directory: gate.working_directory.clone(),
        status: ValidationEvidenceStatus::TimedOut,
        exit_code: None,
        output_summary: "validation process timed out after its model-free retry".into(),
        duration: Duration::from_millis(1),
    });
    let failure_id = FailureId::new("production-validation-timeout");
    let mut snapshot = ExecutionSnapshot {
        run_id: "production-recovery-run".into(),
        current_repository: RepositorySnapshot {
            fingerprint: repository_fingerprint.clone(),
            source_tree_hash: repository_fingerprint.clone(),
            changed_paths: BTreeSet::from(["src/recovery.rs".to_owned()]),
            ..RepositorySnapshot::default()
        },
        graph,
        evidence,
        budget: BudgetState::new(budget),
        ..ExecutionSnapshot::default()
    };
    snapshot.failures.records.push(FailureRecord::new(
        failure_id,
        validation_node.id,
        FailureCategory::InfrastructureFailure,
        1,
        repository_fingerprint.clone(),
        "validation process timed out after its model-free retry",
    ));
    snapshot
        .events
        .push(ExecutionDomainEvent::GuardrailTriggered {
            sequence: 1,
            reason: GuardrailReason::InfrastructureFailure,
            outcome: MissionOutcome::PartialReviewable,
            detail: "validation process timed out after its model-free retry".into(),
        });
    let restored = restored_validation_results_from_snapshot(&snapshot)
        .expect("partial infrastructure recovery restores the timeout observation");
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].status, "timed_out");

    let mut notebook = new_worker_notebook(&manifest, repository_fingerprint.clone(), None);
    notebook.phase = ExecutionPhase::Repair;
    notebook.orchestration = HostedOrchestrationCheckpoint {
        legacy_import_completed: true,
        ..HostedOrchestrationCheckpoint::default()
    };
    notebook.orchestration.replace_from_snapshot(&snapshot);
    let orchestration = notebook.orchestration.clone();
    orchestration.materialize_legacy_notebook(&mut notebook);
    notebook.acceptance_criteria = vec!["The recovered change remains reviewable.".into()];
    manifest.run.metadata["worker_notebook"] = serde_json::to_value(notebook).unwrap();

    let Some(StoppableOkServer {
        api_root,
        requests,
        stop: stop_server,
        handle: server,
    }) = stoppable_ok_server()
    else {
        return;
    };
    let api = test_api_client(api_root, execution_id);
    let running = Arc::new(AtomicBool::new(true));
    let stop_reason = Arc::new(Mutex::new(None));
    let lease_renewed_at = Arc::new(Mutex::new(None));
    let Ok(containment) = command::HostedProcessContainment::new() else {
        return;
    };
    let trusted_git_config = repo.hosted_local_config().unwrap();
    let trusted_head = command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap();
    let mut agent = GatewayAgent::new(
        api.clone(),
        &manifest,
        &repo,
        &trusted_git_config,
        &running,
        &stop_reason,
        &lease_renewed_at,
        &containment,
        None,
    )
    .unwrap();
    let context = RecoveryPublicationContext {
        api: &api,
        manifest: &manifest,
        repo: &repo,
        repo_config: &RepoConfig {
            owner: "RustGrid".into(),
            name: "example".into(),
        },
        trusted_git_config: &trusted_git_config,
        trusted_head: &trusted_head,
        baseline: &BTreeSet::new(),
        containment: &containment,
        running: &running,
        startup_mode: StartupMode::FreshRun,
    };
    let mut draft_requested = false;
    let result = attempt_safe_recovery_publication_with(
        &mut agent,
        context,
        &anyhow!("illegal hosted lifecycle transition from repair to completion_evaluation"),
        |already_pushed, commit| {
            assert!(!already_pushed);
            assert_eq!(
                command::checked("git", ["rev-parse", "HEAD"], work.path())?,
                commit
            );
            command::checked(
                "git",
                [
                    "push",
                    "-q",
                    local_remote.as_str(),
                    manifest.github.branch.as_str(),
                ],
                work.path(),
            )?;
            Ok(())
        },
        |validation, completeness| {
            draft_requested = true;
            assert!(validation.iter().all(|gate| gate.status == "timed_out"));
            assert_eq!(completeness.status, CompletionStatus::Partial);
            assert!(
                completeness
                    .remaining_implementation_work
                    .iter()
                    .any(|item| item.contains("Remaining graph node"))
            );
            Ok(crate::github::PullRequest {
                number: 226,
                html_url: "https://github.example/RustGrid/example/pull/226".into(),
                node_id: Some("PR_RECOVERY_226".into()),
                draft: true,
                body: None,
            })
        },
    )
    .unwrap();
    assert_eq!(result.0, RecoveryPublicationResult::PublishedDraft);
    let result = result.1.expect("validated recovery should publish");
    assert!(draft_requested);
    assert_eq!(result.completeness.status, CompletionStatus::Partial);
    assert_eq!(
        result.completeness.verification_readiness,
        VerificationReadiness::PendingManualReview
    );
    assert_eq!(result.pull_request.number, 226);
    assert!(
        agent
            .notebook
            .orchestration
            .domain_events
            .iter()
            .any(|event| matches!(
                event,
                ExecutionDomainEvent::RecoveryPublicationRequested { .. }
            ))
    );
    assert!(
        agent
            .notebook
            .orchestration
            .domain_events
            .iter()
            .any(|event| matches!(event, ExecutionDomainEvent::CommitCreated { .. }))
    );
    assert!(
        agent
            .notebook
            .orchestration
            .domain_events
            .iter()
            .any(|event| matches!(event, ExecutionDomainEvent::BranchPushed { .. }))
    );
    assert!(
        agent
            .notebook
            .orchestration
            .domain_events
            .iter()
            .any(|event| matches!(
                event,
                ExecutionDomainEvent::PullRequestCreated { draft: true, .. }
            ))
    );
    assert!(matches!(
        agent.notebook.orchestration.domain_events.last(),
        Some(ExecutionDomainEvent::RunFinished {
            outcome: MissionOutcome::PartialReviewable,
            ..
        })
    ));
    assert_eq!(
        agent.notebook.orchestration.publication.mode,
        Some(PublicationMode::DraftRecovery)
    );
    assert_eq!(
        agent.notebook.orchestration.publication.status,
        PublicationStatus::PullRequestCreated
    );
    assert!(agent.notebook.orchestration.publication.recovery_requested);
    let remote_commit = command::checked(
        "git",
        [
            "--git-dir",
            remote.path().to_str().unwrap(),
            "rev-parse",
            &format!("refs/heads/{branch}"),
        ],
        work.path(),
    )
    .unwrap();
    assert_eq!(remote_commit, result.commit);
    assert!(repo.new_agent_paths(&BTreeSet::new()).unwrap().is_empty());

    stop_server.send(()).unwrap();
    server.join().unwrap();
    let delivered = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(
        delivered
            .iter()
            .filter(|request| request.contains("/state"))
            .count(),
        1
    );
    let mut cursor = 0;
    for marker in [
        "publish_recovery_draft",
        "recovery_publication_requested",
        "worker.recovery_publication_started",
        "recovery_commit_created",
        "recovery_branch_pushed",
        "creating_pull_request",
        "recovery_run_finished",
        "finish_recovery_action",
    ] {
        let offset = delivered[cursor..]
            .iter()
            .position(|request| request.contains(marker))
            .unwrap_or_else(|| panic!("missing ordered recovery request marker `{marker}`"));
        cursor += offset + 1;
    }
}

#[test]
fn cancellation_preservation_failure_cannot_preempt_the_resumable_checkpoint() {
    let source = hosted_production_source();
    let start = source
        .find("fn ensure_active_or_checkpoint_cancellation")
        .expect("cancellation checkpoint adapter must exist");
    let end = start
        + source[start..]
            .find("fn record_tool_progress")
            .expect("cancellation adapter must have a bounded source section");
    let cancellation = &source[start..end];
    let preservation_failure = cancellation
        .find("preservation_failure = Some(error)")
        .expect("branch preservation failures must be captured");
    let cancellation_event = cancellation
        .find("ExecutionDomainEvent::CancellationRequested")
        .expect("cancellation must be event sourced");
    let checkpoint = cancellation
        .find("persist_orchestration_checkpoint(\"cancellation_checkpointed\"")
        .expect("cancellation graph checkpoint must be persisted");
    let propagation = cancellation
        .find("if let Some(error) = preservation_failure")
        .expect("preservation failure must be propagated after checkpointing");

    assert!(preservation_failure < cancellation_event);
    assert!(cancellation_event < checkpoint);
    assert!(checkpoint < propagation);
    assert!(!cancellation.contains("preservation_result?"));
}

#[test]
fn post_agent_blocking_operations_checkpoint_terminal_stop_before_propagation() {
    let source = hosted_production_source();

    let execution_start = source
        .find("fn run_hosted_execution(")
        .expect("hosted execution entrypoint must exist");
    let execution_end = execution_start
        + source[execution_start..]
            .find("fn validation_entry_allows_gates")
            .expect("hosted execution entrypoint must have a bounded source section");
    let execution = &source[execution_start..execution_end];
    let bootstrap = execution
        .find("bootstrap_hosted_dependencies(")
        .expect("hosted dependency bootstrap must exist after agent creation");
    let bootstrap_propagation = execution[bootstrap..]
        .find("bootstrap_result?;")
        .map(|offset| bootstrap + offset)
        .expect("bootstrap result must be propagated");
    assert!(
        execution[..bootstrap]
            .rfind("agent.ensure_active_or_checkpoint_cancellation()?;")
            .is_some()
    );
    assert!(
        execution[bootstrap..bootstrap_propagation]
            .contains("agent.ensure_active_or_checkpoint_cancellation()?;")
    );

    let validation_start = source
        .find("fn run_graph_validation_sequence(")
        .expect("graph validation sequence must exist");
    let validation_end = validation_start
        + source[validation_start..]
            .find("fn find_or_create_hosted_pull_request")
            .expect("graph validation sequence must have a bounded source section");
    let validation = &source[validation_start..validation_end];
    let quality_gate = validation
        .find("let gate_results = run_quality_gates(")
        .expect("worker-owned quality gates must exist");
    let gate_propagation = validation[quality_gate..]
        .find("let gate_results = gate_results?;")
        .map(|offset| quality_gate + offset)
        .expect("quality-gate result must be propagated");
    assert!(
        validation[..quality_gate]
            .rfind("agent.ensure_active_or_checkpoint_cancellation()?;")
            .is_some()
    );
    assert!(
        validation[gate_propagation..]
            .find("ValidationEvidenceRecorded")
            .is_some_and(
                |evidence_offset| validation[gate_propagation + evidence_offset..]
                    .contains("agent.ensure_active_or_checkpoint_cancellation()?;")
            ),
        "a completed or cancelled command must be reduced to canonical evidence before the post-command stop checkpoint"
    );
}

#[test]
fn validation_uses_selected_graph_node_duration_budget() {
    let source = hosted_production_source();
    let validation_start = source
        .find("fn run_graph_validation_sequence(")
        .expect("graph validation sequence must exist");
    let validation_end = validation_start
        + source[validation_start..]
            .find("fn find_or_create_hosted_pull_request")
            .expect("graph validation sequence must have a bounded source section");
    let validation = &source[validation_start..validation_end];

    assert!(validation.contains("remaining_for(&node_id, &node.budget)"));
    assert!(validation.contains("node_remaining.min(mission_remaining)"));
    assert!(validation.contains(".record_node_duration(node_id.clone(),"));
    assert!(!validation.contains("Duration::from_secs(8 * 60)"));
}

fn read_http_request(stream: &mut std::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4_096];
    loop {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&bytes);
        let Some(header_end) = text.find("\r\n\r\n") else {
            continue;
        };
        let content_length = text[..header_end]
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        if bytes.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn one_request_server(
    status: &str,
    body: Value,
) -> Option<(Url, Receiver<String>, thread::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(error) => panic!("test HTTP server should bind: {error}"),
    };
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    let status = status.to_owned();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        let _ = sender.send(request);
        let body = body.to_string();
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    (
        Url::parse(&format!("http://{address}/")).unwrap(),
        receiver,
        handle,
    )
        .into()
}

fn delayed_no_response_server(
    delay: Duration,
) -> Option<(Url, Receiver<String>, thread::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(error) => panic!("test HTTP server should bind: {error}"),
    };
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        let _ = sender.send(request);
        thread::sleep(delay);
        listener.set_nonblocking(true).unwrap();
        loop {
            match listener.accept() {
                Ok((_stream, _)) => {
                    let _ = sender.send("additional request".into());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    panic!("test HTTP server should inspect queued requests: {error}")
                }
            }
        }
    });
    Some((
        Url::parse(&format!("http://{address}/")).unwrap(),
        receiver,
        handle,
    ))
}

fn request_sequence_server(
    responses: Vec<(&'static str, Value)>,
) -> Option<(Url, Receiver<String>, thread::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(error) => panic!("test HTTP server should bind: {error}"),
    };
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let _ = sender.send(request);
            let body = body.to_string();
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    Some((
        Url::parse(&format!("http://{address}/")).unwrap(),
        receiver,
        handle,
    ))
}

fn request_outcome_sequence_server(
    responses: Vec<Option<(&'static str, Value)>>,
) -> Option<(Url, Receiver<String>, thread::JoinHandle<()>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(error) => panic!("test HTTP server should bind: {error}"),
    };
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let _ = sender.send(request);
            let Some((status, body)) = response else {
                continue;
            };
            let body = body.to_string();
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    Some((
        Url::parse(&format!("http://{address}/")).unwrap(),
        receiver,
        handle,
    ))
}

struct StoppableOkServer {
    api_root: Url,
    requests: Receiver<String>,
    stop: mpsc::Sender<()>,
    handle: thread::JoinHandle<()>,
}

fn stoppable_ok_server() -> Option<StoppableOkServer> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(error) => panic!("test HTTP server should bind: {error}"),
    };
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (request_sender, request_receiver) = mpsc::channel();
    let (stop_sender, stop_receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        loop {
            match stop_receiver.try_recv() {
                Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let request = read_http_request(&mut stream);
                    let _ = request_sender.send(request);
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                    )
                    .unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("test HTTP server should accept requests: {error}"),
            }
        }
    });
    Some(StoppableOkServer {
        api_root: Url::parse(&format!("http://{address}/")).unwrap(),
        requests: request_receiver,
        stop: stop_sender,
        handle,
    })
}

fn exchange_response(execution_id: Uuid) -> ExchangeResponse {
    ExchangeResponse {
        access_token: format!("rge_{}", "a".repeat(48)),
        token_type: "Bearer".into(),
        expires_in: 900,
        expires_at: "2099-01-01T00:00:00Z".into(),
        token_id: Uuid::from_u128(30),
        tenant_id: Uuid::from_u128(31),
        project_id: Uuid::from_u128(32),
        execution_id,
        execution_attempt: 1,
        session_id: Uuid::from_u128(33),
        worker_id: Uuid::from_u128(34),
        repository_id: 7,
        github_workflow_run_id: 88,
        permissions: EXECUTION_PERMISSIONS.map(str::to_owned).to_vec(),
    }
}

fn test_api_client(api_root: Url, execution_id: Uuid) -> HostedApiClient {
    HostedApiClient::from_exchange(
        hosted_http_client().unwrap(),
        api_root.join("api/v1/").unwrap(),
        execution_id,
        exchange_response(execution_id),
        Arc::new(SystemHostedClock),
    )
    .unwrap()
}

fn test_api_client_with_clock(
    api_root: Url,
    execution_id: Uuid,
    clock: Arc<dyn HostedClock>,
) -> HostedApiClient {
    HostedApiClient::from_exchange(
        hosted_http_client().unwrap(),
        api_root.join("api/v1/").unwrap(),
        execution_id,
        exchange_response(execution_id),
        clock,
    )
    .unwrap()
}

fn test_hosted_http_error(
    status: StatusCode,
    code: &str,
    upstream_provider_status: Option<u16>,
    provider_contacted: Option<bool>,
) -> HostedHttpError {
    HostedHttpError {
        status,
        path: "executions/id/ai/responses".into(),
        code: code.into(),
        request_id: Some("request-1".into()),
        rustgrid_gateway_status: None,
        upstream_provider_status,
        failure_stage: None,
        provider_contacted,
        call_budget_consumed: None,
        reservation_state: None,
        reservation_reconciliation_state: None,
        retryable: None,
        rustgrid_request_id: None,
        transport_request_id: None,
        provider_request_id: None,
        provider_error: None,
        provider_response_body: None,
        model_alias: None,
        resolved_provider_model: None,
        adapter_version: None,
        payload_schema_version: None,
        provider_attempts: None,
        actual_cost_micros: None,
    }
}

fn test_execution_failure(code: &str, message: &str) -> HostedAgentExecutionFailure {
    HostedAgentExecutionFailure {
        status: "failed",
        category: "hosted_agent_execution_failed",
        process_health: "failed",
        mission_outcome: "failed",
        blocker: None,
        resumable: true,
        code: code.into(),
        phase: ExecutionPhase::Discovery,
        message: message.into(),
        underlying_error: UnderlyingFailure {
            r#type: "orchestration_guardrail".into(),
            message: code.into(),
            stack_reference: None,
        },
        model_calls_used: 0,
        model_calls_limit: 40,
        model_calls_remaining: 40,
        phase_calls_used: 0,
        phase_calls_limit: 8,
        last_successful_action: json!({}),
        usage: ToolUsage::default(),
        estimated_cost_micros: 0,
        input_tokens: 0,
        output_tokens: 0,
        changed_paths: Vec::new(),
        remaining_work: Vec::new(),
        failed_tool_operations: Vec::new(),
        current_plan: Vec::new(),
        validation_evidence: Vec::new(),
        notebook_revision: 0,
        recoverable: true,
        resume_phase: "discovery".into(),
        resume_from_node: None,
        repository_fingerprint: String::new(),
        recommended_action: "Inspect the authoritative failure details.".into(),
        artifact: None,
        semantic_status: None,
        persistence_status: None,
        rustgrid_gateway_status: None,
        upstream_provider_status: None,
        failure_stage: None,
        provider_contacted: None,
        call_budget_consumed: None,
        reservation_state: None,
        reservation_reconciliation_state: None,
        rustgrid_request_id: None,
        transport_request_id: None,
        provider_request_id: None,
        provider_error: None,
        provider_response_body: None,
        model_alias: None,
        resolved_provider_model: None,
        adapter_version: None,
        payload_schema_version: None,
        provider_attempts: None,
        actual_cost_micros: None,
    }
}

#[test]
fn exhausted_mutation_is_healthy_blocked_and_never_an_initialization_failure() {
    let mut failure = test_execution_failure(
        "mutation_application_exhausted",
        "bounded mutation strategies were rejected",
    );
    failure.category = "orchestration_initialization_failed";
    let failure = super::errors::classify_mutation_application_exhausted(failure);
    assert_eq!(failure.category, "MutationFailure");
    assert_eq!(failure.code, "mutation_application_exhausted");
    assert_eq!(failure.phase, ExecutionPhase::Implementation);
    assert_eq!(failure.status, "blocked");
    assert_eq!(failure.mission_outcome, "blocked");
    assert_eq!(failure.process_health, "healthy");
    assert!(failure.recoverable);
}

fn test_environment(execution_id: Uuid) -> GithubActionsEnvironment {
    let _ = execution_id;
    GithubActionsEnvironment {
        api_root: Url::parse("http://127.0.0.1:8080/api/v1/").unwrap(),
        audience: "http://127.0.0.1:8080".into(),
        oidc_request_url: Url::parse("http://127.0.0.1:8081/oidc").unwrap(),
        oidc_request_token: SecretString::new("request-token".into(), "test").unwrap(),
        dispatch_nonce: SecretString::new("d".repeat(48), "test").unwrap(),
        repository: Some("RustGrid/example".into()),
        repository_id: Some(7),
        sha: Some("a".repeat(40)),
        workflow_run_id: Some(88),
        workflow_run_attempt: Some(1),
        actor: Some("octocat".into()),
        actor_id: Some(583_231),
    }
}

fn test_manifest(execution_id: Uuid) -> HostedManifest {
    let policy = HostedExecutionPolicy {
        policy_version: 1,
        codex: HostedCodexPolicy {
            command: vec!["codex".into(), "exec".into(), "--json".into()],
            environment_allowlist: vec![
                "PATH".into(),
                "HOME".into(),
                "CARGO_HOME".into(),
                "RUSTUP_HOME".into(),
            ],
        },
        quality_gates: vec![HostedQualityGate {
            id: "test".into(),
            command: "cargo test".into(),
            timeout_seconds: 900,
            required: true,
        }],
        timeout_seconds: 3_600,
        sandbox: HostedSandboxPolicy {
            mode: "workspace_write".into(),
            network_access: true,
            writable_roots: vec![".".into()],
            approval_policy: "never".into(),
        },
        mutation_replacement_max_bytes: Some(
            crate::hosted_orchestrator::DEFAULT_MUTATION_REPLACEMENT_THRESHOLD_BYTES,
        ),
    };
    let policy_hash = hex::encode(Sha256::digest(serde_json::to_vec(&policy).unwrap()));
    let base = format!("/api/v1/executions/{execution_id}");
    HostedManifest {
        manifest_version: 3,
        model_call_budget: None,
        requested_model_call_budget: None,
        resolved_model_call_budget: None,
        budget_source: None,
        clamped: None,
        clamp_reason: None,
        execution: ManifestExecution {
            execution_id,
            status: "running".into(),
            attempt_number: 1,
            model: Some("gpt-5.6-sol".into()),
            maximum_input_tokens: Some(100_000),
            maximum_output_tokens: Some(8_000),
            maximum_model_calls: Some(12),
            maximum_duration_seconds: Some(3_600),
            maximum_cost_usd: Some("5.00".into()),
            github_actions: Some(ManifestGithubActionsExecution {
                workflow_run_id: Some(88),
                workflow_run_attempt: Some(1),
                callback_status: None,
                callback_outbox: None,
            }),
            canonical_terminal_result_id: None,
            terminal_revision: None,
            terminal_authority: None,
            canonical_terminal_result: None,
        },
        run: ManifestRun {
            id: execution_id,
            ticket_id: Uuid::from_u128(2),
            input_prompt: "Implement the bounded mission.".into(),
            attempt: 1,
            metadata: json!({}),
        },
        project_id: Uuid::from_u128(32),
        project_key: "RG".into(),
        project_name: "RustGrid".into(),
        ticket_id: Uuid::from_u128(2),
        ticket_key: "RG-7".into(),
        ticket_title: "Hosted execution".into(),
        github: HostedGithubManifest {
            repository_id: 7,
            repository: "RustGrid/example".into(),
            clone_url: "https://github.com/RustGrid/example.git".into(),
            web_base_url: "https://github.com".into(),
            installation_id: 42,
            base_ref: "main".into(),
            base_sha: "a".repeat(40),
            branch: format!("rustgrid/rg-7-{}", &execution_id.simple().to_string()[..8]),
            github_token_url: format!("{base}/github-token"),
        },
        ai_gateway: HostedAiManifest {
            responses_url: format!("{base}/ai/responses"),
            model: "gpt-5.6-sol".into(),
            maximum_input_tokens: 100_000,
            maximum_output_tokens: 8_000,
            maximum_model_calls: 12,
            maximum_cost_usd: "5.00".into(),
        },
        execution_policy: policy,
        execution_policy_sha256: policy_hash,
        heartbeat_url: format!("{base}/heartbeat"),
        token_refresh_url: format!("{base}/token/refresh"),
        events_url: format!("{base}/worker-events"),
        telemetry_url: format!("{base}/telemetry/batch"),
        state_url: format!("{base}/state"),
        complete_url: format!("{base}/complete"),
    }
}

#[test]
fn normalizes_hosted_api_roots_without_double_api_prefixes() {
    let production_root = normalize_api_root(DEFAULT_INSTANCE_URL).unwrap();
    assert_eq!(production_root.as_str(), "https://app.rustgrid.com/api/v1/");
    assert_eq!(
        production_root
            .join("execution-auth/github-actions/exchange")
            .unwrap()
            .as_str(),
        "https://app.rustgrid.com/api/v1/execution-auth/github-actions/exchange"
    );
    assert_eq!(
        normalize_api_root("https://app.rustgrid.com/api/v1")
            .unwrap()
            .as_str(),
        "https://app.rustgrid.com/api/v1/"
    );
    assert!(normalize_api_root("http://app.rustgrid.com").is_err());
    assert!(normalize_api_root("https://user:password@app.rustgrid.com").is_err());
    assert!(
        secure_github_oidc_url(
            "RUSTGRID_OIDC_REQUEST_URL",
            "https://pipelines.actions.githubusercontent.com/job/idtoken?api-version=2.0"
        )
        .is_ok()
    );
    assert!(
        secure_github_oidc_url(
            "RUSTGRID_OIDC_REQUEST_URL",
            "https://attacker.invalid/idtoken"
        )
        .is_err()
    );
    assert!(
        secure_github_oidc_url(
            "RUSTGRID_OIDC_REQUEST_URL",
            "https://pipelines.actions.githubusercontent.com/idtoken?audience=attacker"
        )
        .is_err()
    );
    assert!(validate_dispatch_nonce(&format!("rgdn_{}", "a".repeat(40))).is_ok());
    assert!(validate_dispatch_nonce(&"a".repeat(40)).is_err());
}

#[test]
fn secrets_are_always_redacted_from_debug_output() {
    let secret = SecretString::new("rge_super-secret".into(), "test").unwrap();
    assert_eq!(format!("{secret:?}"), "<redacted>");
    assert!(!format!("{secret:?}").contains("super-secret"));
}

#[test]
fn rejects_incomplete_or_mismatched_hosted_execution_identity() {
    let execution_id = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
    let mut incomplete = test_environment(execution_id);
    incomplete.workflow_run_attempt = None;
    assert!(incomplete.require_execute_context().is_err());
    let mut missing_sha = test_environment(execution_id);
    missing_sha.sha = None;
    assert!(missing_sha.require_execute_context().is_err());
    let mut missing_actor = test_environment(execution_id);
    missing_actor.actor = None;
    assert!(missing_actor.require_execute_context().is_err());
    let mut invalid_actor = test_environment(execution_id);
    invalid_actor.actor = Some("octocat@example.com".into());
    assert!(invalid_actor.require_execute_context().is_err());
    let mut missing_actor_id = test_environment(execution_id);
    missing_actor_id.actor_id = None;
    assert!(missing_actor_id.require_execute_context().is_err());

    let author = test_environment(execution_id).git_author().unwrap();
    assert_eq!(author.name, "octocat");
    assert_eq!(author.email, "583231+octocat@users.noreply.github.com");
    let mut bot_environment = test_environment(execution_id);
    bot_environment.actor = Some("rustgrid[bot]".into());
    bot_environment.actor_id = Some(123_456);
    let bot_author = bot_environment.git_author().unwrap();
    assert_eq!(bot_author.name, "rustgrid[bot]");
    assert_eq!(
        bot_author.email,
        "123456+rustgrid[bot]@users.noreply.github.com"
    );

    let mut wrong_permissions = exchange_response(execution_id);
    wrong_permissions.permissions.pop();
    assert!(
        HostedApiClient::from_exchange(
            hosted_http_client().unwrap(),
            Url::parse("http://127.0.0.1:8080/api/v1/").unwrap(),
            execution_id,
            wrong_permissions,
            Arc::new(SystemHostedClock),
        )
        .is_err()
    );

    let mut environment = test_environment(execution_id);
    environment.workflow_run_id = Some(89);
    let api = test_api_client(Url::parse("http://127.0.0.1:8080/").unwrap(), execution_id);
    assert!(
        test_manifest(execution_id)
            .validate(execution_id, &environment, &api)
            .is_err()
    );
    let environment = test_environment(execution_id);
    let mut wrong_sha = test_manifest(execution_id);
    wrong_sha.github.base_sha = "b".repeat(40);
    assert!(
        wrong_sha
            .validate(execution_id, &environment, &api)
            .is_err()
    );
    let mut malformed_sha = test_manifest(execution_id);
    malformed_sha.github.base_sha = "not-a-commit".into();
    assert!(
        malformed_sha
            .validate(execution_id, &environment, &api)
            .is_err()
    );
}

#[test]
fn cancelled_completion_omits_failure_fields_required_only_for_failures() {
    let cancelled = unsuccessful_completion(
        true,
        "execution_cancelled".into(),
        "The execution was cancelled.".into(),
    );
    let encoded = serde_json::to_value(cancelled).unwrap();
    assert_eq!(encoded["status"], "cancelled");
    assert!(encoded.get("failure_code").is_none());
    assert!(encoded.get("failure_message").is_none());
}

#[test]
fn validates_the_v3_manifest_and_all_scoped_endpoints() {
    let execution_id = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
    let environment = test_environment(execution_id);
    let api = test_api_client(Url::parse("http://127.0.0.1:8080/").unwrap(), execution_id);
    let manifest = test_manifest(execution_id);
    manifest.validate(execution_id, &environment, &api).unwrap();

    let mut forty_call_manifest = manifest.clone();
    forty_call_manifest.execution.maximum_model_calls = Some(40);
    forty_call_manifest.ai_gateway.maximum_model_calls = 40;
    forty_call_manifest
        .validate(execution_id, &environment, &api)
        .unwrap();

    let mut undersized_manifest = manifest.clone();
    undersized_manifest.execution.maximum_model_calls = Some(9);
    undersized_manifest.ai_gateway.maximum_model_calls = 9;
    assert!(
        undersized_manifest
            .validate(execution_id, &environment, &api)
            .is_err()
    );

    let mut wrong_branch = manifest.clone();
    wrong_branch.github.branch = "rustgrid/other".into();
    assert!(
        wrong_branch
            .validate(execution_id, &environment, &api)
            .unwrap_err()
            .to_string()
            .contains("branch")
    );

    let mut wrong_gateway = manifest;
    wrong_gateway.ai_gateway.responses_url = "https://attacker.invalid/responses".into();
    assert!(
        wrong_gateway
            .validate(execution_id, &environment, &api)
            .is_err()
    );
}

#[test]
fn mission_retry_attempt_is_independent_from_github_run_attempt() {
    let execution_id = Uuid::from_u128(0x22222222_2222_4222_8222_222222222222);
    let environment = test_environment(execution_id);
    let mut exchange = exchange_response(execution_id);
    exchange.execution_attempt = 2;
    let api = HostedApiClient::from_exchange(
        hosted_http_client().unwrap(),
        Url::parse("http://127.0.0.1:8080/api/v1/").unwrap(),
        execution_id,
        exchange,
        Arc::new(SystemHostedClock),
    )
    .unwrap();
    let mut manifest = test_manifest(execution_id);
    manifest.execution.attempt_number = 2;
    manifest.run.attempt = 2;

    manifest.validate(execution_id, &environment, &api).unwrap();

    let mut wrong_workflow_attempt = environment;
    wrong_workflow_attempt.workflow_run_attempt = Some(2);
    assert!(
        manifest
            .validate(execution_id, &wrong_workflow_attempt, &api)
            .is_err()
    );
}

#[test]
fn rejects_sensitive_execution_environments_and_publication_commands() {
    for name in [
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "RUSTGRID_EXECUTION_TOKEN",
        "GITHUB_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "BASH_ENV",
        "NODE_OPTIONS",
        "GIT_CONFIG_COUNT",
    ] {
        assert!(!safe_child_environment_name(name), "{name}");
    }
    assert!(safe_child_environment_name("PATH"));
    assert!(validate_model_command("git diff -- src/lib.rs").is_ok());
    assert!(validate_model_command("git push origin branch").is_err());
    assert!(validate_model_command("curl https://example.com").is_err());
    assert!(validate_model_command("npm test && npm run build").is_err());
    assert!(validate_model_command("python3 - <<PY\nprint('no')\nPY").is_err());
    assert!(validate_model_command("sed -n 1,20p src/lib.rs").is_ok());
}

#[test]
fn quality_gate_phase_telemetry_satisfies_the_completion_contract() {
    let execution_id = Uuid::from_u128(0x1234);
    let gate = HostedQualityGate {
        id: "cargo-test".into(),
        command: "cargo test --locked".into(),
        timeout_seconds: 900,
        required: true,
    };
    let started = quality_gate_phase_event(
        execution_id,
        &gate,
        3,
        2,
        "2026-07-27T10:00:00Z",
        None,
        ExecutionStatus::Running,
        1,
    );
    let completed = quality_gate_phase_event(
        execution_id,
        &gate,
        3,
        2,
        "2026-07-27T10:00:00Z",
        Some("2026-07-27T10:01:00Z"),
        ExecutionStatus::Succeeded,
        2,
    );

    assert_eq!(started.event_type, "phase.started");
    assert_eq!(completed.event_type, "phase.completed");
    assert_eq!(started.entity_revision, 1);
    assert_eq!(completed.entity_revision, 2);
    assert_ne!(started.event_id, completed.event_id);
    let (
        TelemetryPayload::Phase {
            phase: started_phase,
        },
        TelemetryPayload::Phase {
            phase: completed_phase,
        },
    ) = (&started.payload, &completed.payload)
    else {
        panic!("quality gate telemetry must use phase payloads");
    };
    assert_eq!(started_phase.id, completed_phase.id);
    assert_eq!(completed_phase.execution_id, execution_id);
    assert_eq!(completed_phase.name, "quality_gate:cargo-test");
    assert!(completed_phase.completed_at.is_some());
    assert!(matches!(completed_phase.status, ExecutionStatus::Succeeded));

    let replay = quality_gate_phase_event(
        execution_id,
        &gate,
        3,
        2,
        "2026-07-27T10:00:00Z",
        Some("2026-07-27T10:01:00Z"),
        ExecutionStatus::Succeeded,
        2,
    );
    assert_eq!(replay.event_id, completed.event_id);
}

#[test]
fn quality_gate_phase_telemetry_posts_to_the_execution_contract() {
    let execution_id = Uuid::from_u128(0x5678);
    let Some((base, request, server)) = one_request_server("200 OK", json!({})) else {
        return;
    };
    let api = test_api_client(base, execution_id);
    let gate = HostedQualityGate {
        id: "verify".into(),
        command: "cargo test --locked".into(),
        timeout_seconds: 900,
        required: true,
    };
    send_quality_gate_phase_telemetry(
        &api,
        execution_id,
        &gate,
        1,
        1,
        "2026-07-27T10:00:00Z",
        Some("2026-07-27T10:01:00Z"),
        ExecutionStatus::Succeeded,
        2,
    )
    .unwrap();
    server.join().unwrap();
    let request = request.recv().unwrap();
    assert!(request.starts_with(&format!(
        "POST /api/v1/executions/{execution_id}/telemetry/batch HTTP/1.1"
    )));
    let body = request.split("\r\n\r\n").nth(1).unwrap();
    let body: Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["telemetry_version"], TELEMETRY_VERSION);
    assert_eq!(body["events"][0]["type"], "phase.completed");
    assert_eq!(body["events"][0]["entity_revision"], 2);
    assert_eq!(
        body["events"][0]["phase"]["execution_id"],
        execution_id.to_string()
    );
    assert_eq!(body["events"][0]["phase"]["name"], "quality_gate:verify");
    assert_eq!(body["events"][0]["phase"]["status"], "succeeded");
    assert_eq!(
        body["events"][0]["phase"]["completed_at"],
        "2026-07-27T10:01:00Z"
    );
}

#[test]
fn execution_policy_rejects_duplicate_quality_gate_phase_identities() {
    let execution_id = Uuid::from_u128(0x1234);
    let mut manifest = test_manifest(execution_id);
    manifest
        .execution_policy
        .quality_gates
        .push(manifest.execution_policy.quality_gates[0].clone());
    assert!(manifest.execution_policy.validate().is_err());
}

#[test]
fn three_required_gates_execute_once_and_reuse_exact_tree_evidence() {
    assert_eq!(
        classify_validation_gate("focused-theme", "npm test -- theme"),
        ValidationGateType::FocusedTest
    );
    let directory = tempfile::tempdir().unwrap();
    command::checked("git", ["init", "-q"], directory.path()).unwrap();
    command::checked("git", ["config", "user.name", "Test"], directory.path()).unwrap();
    command::checked(
        "git",
        ["config", "user.email", "test@example.com"],
        directory.path(),
    )
    .unwrap();
    fs::write(directory.path().join("theme.ts"), "export {};\n").unwrap();
    command::checked("git", ["add", "theme.ts"], directory.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
        directory.path(),
    )
    .unwrap();
    let base_sha = command::checked("git", ["rev-parse", "HEAD"], directory.path()).unwrap();
    let execution_id = Uuid::from_u128(0xa0226);
    let mut manifest = test_manifest(execution_id);
    manifest.github.base_sha = base_sha;
    manifest.execution_policy.quality_gates = vec![
        HostedQualityGate {
            id: "focused-theme".into(),
            command: "npm test -- theme".into(),
            timeout_seconds: 30,
            required: true,
        },
        HostedQualityGate {
            id: "test".into(),
            command: "npm test".into(),
            timeout_seconds: 30,
            required: true,
        },
        HostedQualityGate {
            id: "build".into(),
            command: "npm run build".into(),
            timeout_seconds: 30,
            required: true,
        },
    ];
    let Some((api_root, requests, server)) = request_sequence_server(
        std::iter::repeat_with(|| ("200 OK", json!({})))
            .take(27)
            .collect(),
    ) else {
        return;
    };
    let api = test_api_client(api_root, execution_id);
    let repo = Repo {
        root: directory.path().to_path_buf(),
    };
    let running = Arc::new(AtomicBool::new(true));
    let validation_started = Instant::now();
    let execution_started = Instant::now();
    let mut ledger = Vec::new();
    let mut required_gates = Vec::new();
    let mut usage = ToolUsage::default();
    let executed_commands = Arc::new(Mutex::new(Vec::new()));

    let first = run_quality_gates_with_capture(
        &api,
        &manifest,
        &repo,
        &running,
        &manifest.execution_policy,
        1,
        &mut ledger,
        &mut required_gates,
        &mut usage,
        validation_started,
        MAX_HOSTED_EXECUTION_DURATION,
        execution_started,
        MAX_HOSTED_EXECUTION_DURATION,
        |command_text, cwd, running, timeout, max_output_bytes, environment_allowlist, limits| {
            executed_commands
                .lock()
                .unwrap()
                .push(command_text.to_owned());
            command::capture_cancellable_with_environment(
                "git rev-parse --is-inside-work-tree",
                cwd,
                running,
                timeout,
                max_output_bytes,
                environment_allowlist,
                limits,
            )
        },
    )
    .unwrap();
    let second = run_quality_gates_with_capture(
        &api,
        &manifest,
        &repo,
        &running,
        &manifest.execution_policy,
        2,
        &mut ledger,
        &mut required_gates,
        &mut usage,
        validation_started,
        MAX_HOSTED_EXECUTION_DURATION,
        execution_started,
        MAX_HOSTED_EXECUTION_DURATION,
        |command_text, cwd, running, timeout, max_output_bytes, environment_allowlist, limits| {
            executed_commands
                .lock()
                .unwrap()
                .push(command_text.to_owned());
            command::capture_cancellable_with_environment(
                "git rev-parse --is-inside-work-tree",
                cwd,
                running,
                timeout,
                max_output_bytes,
                environment_allowlist,
                limits,
            )
        },
    )
    .unwrap();

    // A one-byte mutation creates a new authoritative tree and invalidates all three
    // fingerprints exactly once.
    fs::write(directory.path().join("theme.ts"), "Export {};\n").unwrap();
    let third = run_quality_gates_with_capture(
        &api,
        &manifest,
        &repo,
        &running,
        &manifest.execution_policy,
        3,
        &mut ledger,
        &mut required_gates,
        &mut usage,
        validation_started,
        MAX_HOSTED_EXECUTION_DURATION,
        execution_started,
        MAX_HOSTED_EXECUTION_DURATION,
        |command_text, cwd, running, timeout, max_output_bytes, environment_allowlist, limits| {
            executed_commands
                .lock()
                .unwrap()
                .push(command_text.to_owned());
            command::capture_cancellable_with_environment(
                "git rev-parse --is-inside-work-tree",
                cwd,
                running,
                timeout,
                max_output_bytes,
                environment_allowlist,
                limits,
            )
        },
    )
    .unwrap();

    assert_eq!(first.len(), 3);
    assert_eq!(second.len(), 3);
    assert_eq!(third.len(), 3);
    assert!(first.iter().all(|result| result.status == "passed"));
    assert!(second.iter().all(|result| result.status == "passed"));
    assert!(third.iter().all(|result| result.status == "passed"));
    assert_eq!(usage.validation_commands, 6);
    assert_eq!(usage.required_validations, 6);
    assert_eq!(usage.deduplicated_validations, 3);
    assert_eq!(
        *executed_commands.lock().unwrap(),
        [
            "npm test -- theme",
            "npm test",
            "npm run build",
            "npm test -- theme",
            "npm test",
            "npm run build",
        ]
    );
    assert_eq!(ledger.len(), 6);
    assert_ne!(ledger[0].source_tree_hash, ledger[3].source_tree_hash);
    assert_ne!(ledger[0].command_fingerprint, ledger[3].command_fingerprint);
    assert_eq!(required_gates.len(), 3);
    server.join().unwrap();
    assert_eq!(requests.try_iter().count(), 27);
}

#[test]
fn focused_gate_failure_short_circuits_broad_gates_until_source_repair() {
    let directory = tempfile::tempdir().unwrap();
    command::checked("git", ["init", "-q"], directory.path()).unwrap();
    command::checked("git", ["config", "user.name", "Test"], directory.path()).unwrap();
    command::checked(
        "git",
        ["config", "user.email", "test@example.com"],
        directory.path(),
    )
    .unwrap();
    fs::write(directory.path().join("theme.ts"), "export {};\n").unwrap();
    command::checked("git", ["add", "theme.ts"], directory.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
        directory.path(),
    )
    .unwrap();
    let execution_id = Uuid::from_u128(0xa0227);
    let mut manifest = test_manifest(execution_id);
    manifest.github.base_sha =
        command::checked("git", ["rev-parse", "HEAD"], directory.path()).unwrap();
    manifest.execution_policy.quality_gates = vec![
        HostedQualityGate {
            id: "test".into(),
            command: "npm test".into(),
            timeout_seconds: 30,
            required: true,
        },
        HostedQualityGate {
            id: "build".into(),
            command: "npm run build".into(),
            timeout_seconds: 30,
            required: true,
        },
        HostedQualityGate {
            id: "focused-theme".into(),
            command: "npm test -- theme".into(),
            timeout_seconds: 30,
            required: true,
        },
    ];
    let Some((api_root, requests, server)) = request_sequence_server(
        std::iter::repeat_with(|| ("200 OK", json!({})))
            .take(4)
            .collect(),
    ) else {
        return;
    };
    let api = test_api_client(api_root, execution_id);
    let repo = Repo {
        root: directory.path().to_path_buf(),
    };
    let running = Arc::new(AtomicBool::new(true));
    let executed = Arc::new(Mutex::new(Vec::new()));
    let observed_timeouts = Arc::new(Mutex::new(Vec::new()));
    let mut ledger = Vec::new();
    let mut required_gates = Vec::new();
    let mut usage = ToolUsage::default();
    let results = run_quality_gates_with_capture(
        &api,
        &manifest,
        &repo,
        &running,
        &manifest.execution_policy,
        1,
        &mut ledger,
        &mut required_gates,
        &mut usage,
        Instant::now(),
        Duration::from_secs(17),
        Instant::now(),
        MAX_HOSTED_EXECUTION_DURATION,
        |command_text, _, _, timeout, _, _, _| {
            executed.lock().unwrap().push(command_text.to_owned());
            observed_timeouts.lock().unwrap().push(timeout);
            Ok(command::CommandOutput {
                status: std::process::Command::new("sh")
                    .args(["-c", "exit 1"])
                    .status()?,
                stdout: String::new(),
                stderr: "focused theme test failed".into(),
            })
        },
    )
    .unwrap();

    assert_eq!(*executed.lock().unwrap(), ["npm test -- theme"]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, "failed");
    assert_eq!(required_gates.len(), 1);
    assert_eq!(required_gates[0].gate_type, ValidationGateType::FocusedTest);
    let observed_timeout = observed_timeouts.lock().unwrap()[0];
    assert_eq!(observed_timeout, Duration::from_secs(120));
    server.join().unwrap();
    assert_eq!(requests.try_iter().count(), 4);
}

#[test]
fn repository_tools_cannot_escape_or_traverse_git_metadata() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join(".git")).unwrap();
    fs::write(directory.path().join("safe.txt"), "safe\n").unwrap();
    assert!(safe_repo_path(directory.path(), "safe.txt", false).is_ok());
    assert!(safe_repo_path(directory.path(), "../outside", true).is_err());
    assert!(safe_repo_path(directory.path(), ".git/config", true).is_err());

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/tmp", directory.path().join("linked")).unwrap();
        assert!(safe_repo_path(directory.path(), "linked/file", true).is_err());
    }
}

#[test]
fn hosted_repository_fingerprint_tracks_content_and_is_stable_across_commit() {
    let directory = tempfile::tempdir().unwrap();
    command::checked("git", ["init", "-q"], directory.path()).unwrap();
    fs::write(directory.path().join("tracked.ts"), "one\n").unwrap();
    command::checked("git", ["add", "tracked.ts"], directory.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
        directory.path(),
    )
    .unwrap();
    let base = command::checked("git", ["rev-parse", "HEAD"], directory.path()).unwrap();
    let repo = Repo {
        root: directory.path().to_path_buf(),
    };
    let clean = repository_state_fingerprint(&repo, &base).unwrap();
    assert_ne!(clean, hex::encode(Sha256::digest(b"")));

    // Change exactly one byte while preserving file length.
    fs::write(directory.path().join("tracked.ts"), "One\n").unwrap();
    let edited = repository_state_fingerprint(&repo, &base).unwrap();
    assert_ne!(edited, clean);
    fs::write(directory.path().join("tracked.ts"), "one\n").unwrap();
    assert_eq!(repository_state_fingerprint(&repo, &base).unwrap(), clean);

    fs::write(directory.path().join("untracked.test.ts"), "new\n").unwrap();
    let untracked_only = repository_state_fingerprint(&repo, &base).unwrap();
    assert_ne!(untracked_only, clean);
    fs::remove_file(directory.path().join("untracked.test.ts")).unwrap();
    assert_eq!(repository_state_fingerprint(&repo, &base).unwrap(), clean);

    fs::write(directory.path().join("tracked.ts"), "two\n").unwrap();
    fs::write(directory.path().join("new.test.ts"), "new\n").unwrap();
    let before_commit = repository_state_fingerprint(&repo, &base).unwrap();
    command::checked(
        "git",
        ["add", "tracked.ts", "new.test.ts"],
        directory.path(),
    )
    .unwrap();
    command::checked(
        "git",
        [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "implementation",
        ],
        directory.path(),
    )
    .unwrap();
    let after_commit = repository_state_fingerprint(&repo, &base).unwrap();
    assert_eq!(after_commit, before_commit);

    let other = tempfile::tempdir().unwrap();
    command::checked("git", ["init", "-q"], other.path()).unwrap();
    fs::write(other.path().join("different.ts"), "different\n").unwrap();
    command::checked("git", ["add", "different.ts"], other.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "different base",
        ],
        other.path(),
    )
    .unwrap();
    let other_base = command::checked("git", ["rev-parse", "HEAD"], other.path()).unwrap();
    let other_repo = Repo {
        root: other.path().to_path_buf(),
    };
    assert_ne!(
        repository_state_fingerprint(&other_repo, &other_base).unwrap(),
        clean
    );
}

#[test]
fn batch_reads_prevalidate_every_path_before_preserving_success_and_fallback() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("present.ts"), "one\ntwo\nthree\n").unwrap();
    let paths = vec![
        json!("present.ts"),
        json!(42),
        json!("../outside.ts"),
        json!("missing.ts"),
    ];

    let prevalidated = prevalidate_batch_read_paths(directory.path(), &paths);
    let prevalidation_codes = prevalidated
        .iter()
        .map(|path| match path {
            PrevalidatedBatchReadPath::Ready(_) => None,
            PrevalidatedBatchReadPath::Rejected { result, .. } => result.error_code.as_deref(),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        prevalidation_codes,
        vec![
            None,
            Some("path_malformed"),
            Some("path_not_allowed"),
            Some("path_not_found"),
        ]
    );

    let (batch, initial_failures) =
        read_prevalidated_repo_files_with_fallback(directory.path(), &prevalidated, 2, 8 * 1024);

    assert_eq!(initial_failures, 3);
    assert_eq!(batch.files.len(), 4);
    assert_eq!(batch.files[0].status, FileReadStatus::Success);
    assert!(batch.files[0].content.as_deref().unwrap().contains("one"));
    assert_eq!(batch.files[0].line_count, Some(3));
    assert_eq!(batch.files[0].valid_line_range.as_deref(), Some("1-3"));
    assert_eq!(batch.files[1].status, FileReadStatus::Error);
    assert_eq!(batch.files[1].path, "<paths[1]>");
    assert_eq!(batch.files[1].error_code.as_deref(), Some("path_malformed"));
    assert!(!batch.files[1].fallback_attempted);
    assert_eq!(batch.files[2].status, FileReadStatus::Error);
    assert_eq!(
        batch.files[2].error_code.as_deref(),
        Some("path_not_allowed")
    );
    assert!(batch.files[2].fallback_attempted);
    assert_eq!(batch.files[3].status, FileReadStatus::Error);
    assert_eq!(batch.files[3].error_code.as_deref(), Some("path_not_found"));
    assert!(batch.files[3].fallback_attempted);
    assert!(
        batch.files[3]
            .error_message
            .as_deref()
            .unwrap()
            .contains("individual read fallback also failed")
    );
}

#[test]
fn planning_batch_reads_serve_unchanged_ranges_from_the_evidence_cache() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("theme.ts"),
        "fresh filesystem content\n",
    )
    .unwrap();
    let paths = vec![json!("theme.ts")];
    let prevalidated = prevalidate_batch_read_paths(directory.path(), &paths);
    let mut evidence = crate::execution_graph::EvidenceStore::default();
    evidence.capture_file(
        "theme.ts",
        "tree-1",
        crate::execution_graph::LineRange::new(1, 2),
        "cached discovery evidence\n",
        false,
    );

    let (batch, failures) = read_prevalidated_repo_files_with_evidence_cache(
        directory.path(),
        &prevalidated,
        2,
        8 * 1024,
        &evidence,
        "tree-1",
    );

    assert_eq!(failures, 0);
    assert_eq!(batch.files.len(), 1);
    assert_eq!(batch.files[0].status, FileReadStatus::Success);
    assert_eq!(
        batch.files[0].content.as_deref(),
        Some("cached discovery evidence\n")
    );
    assert!(
        !batch.files[0]
            .content
            .as_deref()
            .unwrap()
            .contains("fresh filesystem content")
    );
}

#[test]
fn invalid_read_range_returns_recovery_metadata_without_losing_file_shape() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("short.ts"), "one\ntwo\n").unwrap();

    let result = read_repo_file_result(directory.path(), "short.ts", 9, 12, 8 * 1024);

    assert_eq!(result.status, FileReadStatus::Error);
    assert_eq!(result.error_code.as_deref(), Some("line_range_invalid"));
    assert_eq!(result.line_count, Some(2));
    assert_eq!(result.valid_line_range.as_deref(), Some("1-2"));
    assert!(
        result
            .error_message
            .as_deref()
            .unwrap()
            .contains("valid line range is 1-2")
    );
}

#[test]
fn planning_repair_preserves_valid_fragments_and_reports_only_invalid_paths() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src")).unwrap();
    for path in ["src/provider.ts", "src/toggle.ts", "src/tokens.ts"] {
        fs::write(directory.path().join(path), "export {};\n").unwrap();
    }
    let payload = json!({
        "planned_changes": [
            {
                "change_id": "valid-theme-provider",
                "targets": [{"path": "src/provider.ts", "role": "provider"}],
                "intent": "Update the provider.",
                "reason": "Register the new theme.",
                "acceptance_criteria": ["Theme is selectable"]
            },
            {
                "change_id": "invalid-theme-toggle",
                "targets": [{"path": "src/toggle.ts", "role": "selector"}],
                "intent": "Update the selector."
            },
            {
                "change_id": "valid-theme-tokens",
                "targets": [{"path": "src/tokens.ts", "role": "tokens"}],
                "intent": "Update the tokens.",
                "reason": "Complete the palette.",
                "acceptance_criteria": ["Theme is selectable"]
            }
        ]
    });
    let state = recover_planning_repair_state(directory.path(), payload.as_object().unwrap(), 7);

    assert_eq!(state.model_call, 7);
    assert_eq!(state.valid_planned_changes.len(), 2);
    assert_eq!(state.valid_planned_change_positions, vec![0, 2]);
    assert_eq!(state.invalid_fields.len(), 1);
    assert!(state.invalid_fields[0].starts_with("$.planned_changes[1]:"));

    let repaired_middle: PlannedChange = serde_json::from_value(json!({
        "change_id": "invalid-theme-toggle",
        "targets": [{"path": "src/toggle.ts", "role": "selector"}],
        "intent": "Update the selector.",
        "reason": "Include the new theme in cycling.",
        "acceptance_criteria": ["Theme is selectable"]
    }))
    .unwrap();
    let mut repaired = vec![repaired_middle];
    merge_preserved_plan_fragments(&mut repaired, Some(&state));
    assert_eq!(
        repaired
            .iter()
            .map(|change| change.change_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "valid-theme-provider",
            "invalid-theme-toggle",
            "valid-theme-tokens"
        ]
    );
}

#[test]
fn implementation_plan_accepts_collective_criterion_coverage() {
    let criteria = impact_map::acceptance_criteria(
        &(1..=9)
            .map(|index| format!("Criterion {index} for its owned surface"))
            .collect::<Vec<_>>(),
    );
    let specifications = [
        ("provider", "src/provider.ts", vec!["ac-1", "ac-2"]),
        ("toggle", "src/toggle.ts", vec!["ac-3", "ac-4"]),
        ("storage", "src/storage.ts", vec!["ac-5", "ac-6"]),
        ("palette", "src/palette.ts", vec!["ac-7", "ac-8", "ac-9"]),
    ];
    let planned_changes = specifications
        .iter()
        .map(|(name, path, ids)| PlannedChange {
            change_id: (*name).into(),
            parent_change_id: None,
            path: String::new(),
            targets: vec![PlannedTarget {
                path: (*path).into(),
                role: format!("{name} implementation"),
                operation: Default::default(),
                new_file: false,
                status: IntendedChangeStatus::Planned,
            }],
            change: format!("Implement the {name} surface."),
            reason: format!("Own the {name} criteria."),
            status: IntendedChangeStatus::Planned,
            acceptance_criteria: ids.iter().map(|id| (*id).into()).collect(),
            test_coverage: vec![format!("test {name}")],
        })
        .collect::<Vec<_>>();
    let areas = specifications
        .iter()
        .map(|(name, path, ids)| ImpactArea {
            area_id: format!("area-{name}"),
            name: format!("{name} surface"),
            candidate_paths: vec![(*path).into()],
            evidence: vec![impact_map::ImpactEvidence {
                evidence_type: impact_map::EvidenceType::Inference,
                path: Some((*path).into()),
                query: None,
                description: format!("{name} path"),
            }],
            acceptance_criteria_ids: ids.iter().map(|id| (*id).into()).collect(),
            reason: format!("Maps {name} criteria."),
        })
        .collect::<Vec<_>>();
    let candidate = ImplementationPlan {
        implementation_status: "ready".into(),
        planned_changes,
        planned_new_files: vec![],
        planned_test_changes: vec![],
        remaining_unknowns: vec![],
        blocking_unknowns: vec![],
    };

    let accepted = validate_and_repair_plan_criteria(candidate, &criteria, &areas).unwrap();

    assert!(accepted.criterion_assignments.is_empty());
    assert_eq!(accepted.next_phase, ExecutionPhase::Implementation);
    assert!(
        accepted
            .plan
            .planned_changes
            .iter()
            .all(|change| change.acceptance_criteria.len() < 9)
    );
    let covered = accepted
        .plan
        .planned_changes
        .iter()
        .flat_map(|change| change.acceptance_criteria.iter().cloned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        covered,
        (1..=9).map(|index| format!("ac-{index}")).collect()
    );
}

#[test]
fn orchestrator_repairs_missing_palette_criterion_without_provider_call() {
    let criteria = impact_map::acceptance_criteria(&[
        "Provider registers the theme".into(),
        "Toggle exposes the theme".into(),
        "Selection persists".into(),
        "Fallback remains safe".into(),
        "Existing themes remain available".into(),
        "Keyboard selection works".into(),
        "Tests cover registration".into(),
        "Tests cover persistence".into(),
        "Palette uses the approved blue tokens".into(),
    ]);
    let mut provider = test_planned_change();
    provider.change_id = "provider".into();
    provider.targets[0].path = "src/provider.ts".into();
    provider.acceptance_criteria = (1..=4).map(|index| format!("ac-{index}")).collect();
    let mut tests = test_planned_change();
    tests.change_id = "tests".into();
    tests.targets[0].path = "tests/theme.test.ts".into();
    tests.acceptance_criteria = (5..=8).map(|index| format!("ac-{index}")).collect();
    let mut palette = test_planned_change();
    palette.change_id = "palette".into();
    palette.targets[0].path = "src/palette.ts".into();
    palette.targets[0].role = "approved blue palette".into();
    palette.change = "Apply the approved blue palette tokens.".into();
    palette.reason = "The palette must use the approved blue tokens.".into();
    palette.acceptance_criteria = vec!["ac-1".into()];
    let areas = vec![
        ImpactArea {
            area_id: "area-provider".into(),
            name: "Theme behavior".into(),
            candidate_paths: vec!["src/provider.ts".into(), "tests/theme.test.ts".into()],
            evidence: vec![impact_map::ImpactEvidence {
                evidence_type: impact_map::EvidenceType::Inference,
                path: Some("src/provider.ts".into()),
                query: None,
                description: "Theme behavior paths".into(),
            }],
            acceptance_criteria_ids: (1..=8).map(|index| format!("ac-{index}")).collect(),
            reason: "Provider and tests cover behavioral criteria.".into(),
        },
        ImpactArea {
            area_id: "area-palette".into(),
            name: "Approved blue palette".into(),
            candidate_paths: vec!["src/palette.ts".into()],
            evidence: vec![impact_map::ImpactEvidence {
                evidence_type: impact_map::EvidenceType::Inference,
                path: Some("src/palette.ts".into()),
                query: None,
                description: "Palette path".into(),
            }],
            acceptance_criteria_ids: vec!["ac-9".into()],
            reason: "Owns the approved palette tokens.".into(),
        },
    ];
    let original_candidate = ImplementationPlan {
        implementation_status: "ready".into(),
        planned_changes: vec![provider, tests, palette],
        planned_new_files: vec![],
        planned_test_changes: vec![],
        remaining_unknowns: vec![],
        blocking_unknowns: vec![],
    };
    assert!(
        !original_candidate
            .planned_changes
            .iter()
            .any(|change| change.acceptance_criteria.contains(&"ac-9".into()))
    );

    let accepted =
        validate_and_repair_plan_criteria(original_candidate, &criteria, &areas).unwrap();

    assert_eq!(
        accepted.criterion_assignments,
        vec![PlanCriterionAssignment {
            acceptance_criterion_id: "ac-9".into(),
            change_id: "palette".into(),
        }]
    );
    assert!(!accepted.model_call_consumed);
    assert_eq!(accepted.next_phase, ExecutionPhase::Implementation);
    assert!(
        accepted
            .plan
            .planned_changes
            .iter()
            .find(|change| change.change_id == "palette")
            .unwrap()
            .acceptance_criteria
            .contains(&"ac-9".into())
    );
    validate_plan_criterion_coverage(&accepted.plan, &criteria, &areas).unwrap();
}

#[test]
fn reported_write_progress_is_informational_and_cannot_fake_a_repository_write() {
    assert!(!is_source_mutation_tool("report_write_progress"));
    assert_eq!(
        informational_write_progress_semantics(),
        (ToolProgressClass::Neutral, false)
    );
    let report = informational_write_progress("ready_to_write", "target inspected").unwrap();
    assert!(report.contains("repository_progress=false"));
    assert!(informational_write_progress("applied", "claim only").is_err());
}

#[test]
fn guided_recovery_context_and_admission_are_confined_to_the_current_target() {
    let paths = [
        "src/components/theme/ThemeProvider.tsx",
        "src/components/theme/ThemeToggle.tsx",
        "src/styles/globals.css",
        "tests/theme-provider.test.tsx",
        "tests/theme-palette.test.ts",
    ];
    let mut notebook = test_discovery_notebook(ExecutionPhase::Implementation);
    notebook.planned_changes = paths
        .iter()
        .enumerate()
        .map(|(index, path)| PlannedChange {
            change_id: format!("target-{}", index + 1),
            parent_change_id: None,
            path: String::new(),
            targets: vec![PlannedTarget {
                path: (*path).into(),
                role: "required target".into(),
                operation: Default::default(),
                new_file: false,
                status: IntendedChangeStatus::Planned,
            }],
            change: format!("Update {path}"),
            reason: "Implement the localized theme.".into(),
            status: IntendedChangeStatus::Planned,
            acceptance_criteria: vec!["Theme can be selected".into()],
            test_coverage: vec!["focused theme tests".into()],
        })
        .collect();
    notebook.intended_changes = intended_changes_from_plan(&notebook.planned_changes);
    notebook.files_inspected = vec![paths[0].into()];
    notebook.tool_progress.push(new_tool_progress_record(
        notebook.execution_attempt,
        6,
        ExecutionPhase::Implementation,
        "read_file",
        Some(paths[0].into()),
        ToolProgressClass::RecoverableFailure,
        "line_range_invalid: valid line range is 1-87; retry that exact range",
        false,
    ));

    let context = implementation_start_context_from_notebook(
        &notebook,
        "tree-aops-226".into(),
        4,
        true,
        6,
        0,
    );
    assert_eq!(context.target_order.len(), 5);
    assert_eq!(context.current_target.as_ref().unwrap().path, paths[0]);
    assert_eq!(context.exact_files_already_read, vec![paths[0]]);
    assert!(context.missing_file_contents.contains(&paths[1].into()));
    assert!(context.instruction.contains("work only on current_target"));
    assert_eq!(context.unresolved_preparation_blockers.len(), 1);
    assert!(context.unresolved_preparation_blockers[0].contains("valid line range is 1-87"));

    let current = context.current_target.as_ref();
    assert!(validate_current_target_scope(current, true, 0, &[paths[0]], true).is_ok());
    assert!(validate_current_target_scope(current, true, 0, &[paths[1]], false).is_err());
    assert!(validate_current_target_scope(current, true, 0, &[paths[1]], true).is_err());

    notebook.intended_changes[0].targets[0].status = IntendedChangeStatus::Applied;
    let advanced = implementation_start_context_from_notebook(
        &notebook,
        "tree-after-first-write".into(),
        3,
        false,
        7,
        1,
    );
    assert_eq!(advanced.current_target.as_ref().unwrap().path, paths[1]);
    assert_eq!(advanced.target_order.len(), 4);

    for change in &mut notebook.intended_changes {
        for target in &mut change.targets {
            target.status = IntendedChangeStatus::Applied;
        }
    }
    notebook.phase = ExecutionPhase::Repair;
    notebook.validation_failures = vec!["poisoned legacy validation failure".into()];
    let poisoned_resume_context = implementation_start_context_from_notebook(
        &notebook,
        "tree-with-poisoned-legacy-state".into(),
        2,
        false,
        8,
        5,
    );
    assert!(poisoned_resume_context.target_order.is_empty());
    assert!(poisoned_resume_context.current_target.is_none());

    notebook.validation_failures.clear();
    notebook
        .orchestration
        .failures
        .record(crate::execution_graph::FailureRecord::new(
            "focused-theme-failure",
            crate::execution_graph::ExecutionNodeId::new("validation-focused-theme"),
            crate::execution_graph::FailureCategory::ValidationFailure,
            1,
            "tree-with-failed-focused-gate",
            "light-blue restoration assertion failed",
        ));
    let repair_context = implementation_start_context_from_notebook(
        &notebook,
        "tree-with-failed-focused-gate".into(),
        2,
        false,
        8,
        5,
    );
    assert_eq!(repair_context.target_order.len(), paths.len());
    assert_eq!(
        repair_context.current_target.as_ref().unwrap().path,
        paths[0]
    );
    assert!(
        repair_context
            .target_order
            .iter()
            .all(|target| paths.contains(&target.path.as_str()))
    );
}

#[test]
fn live_progress_records_classify_reads_repeated_failures_and_informational_reports() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("present.ts"), "one\n").unwrap();
    let successful = read_repo_file_result(directory.path(), "present.ts", 1, 1, 8 * 1024);
    assert_eq!(successful.status, FileReadStatus::Success);
    let failed = read_repo_file_result(directory.path(), "missing.ts", 1, 10, 8 * 1024);
    let error_code = failed.error_code.as_deref().unwrap();
    let detail = format!(
        "{}: {}",
        error_code,
        failed.error_message.as_deref().unwrap()
    );
    let records = vec![
        new_tool_progress_record(
            18,
            1,
            ExecutionPhase::Implementation,
            "read_file",
            Some("present.ts".into()),
            ToolProgressClass::Productive,
            "new repository content inspected",
            false,
        ),
        new_tool_progress_record(
            18,
            2,
            ExecutionPhase::Implementation,
            "read_file",
            Some("missing.ts".into()),
            read_error_progress_class(error_code),
            &detail,
            false,
        ),
        new_tool_progress_record(
            18,
            3,
            ExecutionPhase::Implementation,
            "read_file",
            Some("missing.ts".into()),
            read_error_progress_class(error_code),
            &detail,
            false,
        ),
        new_tool_progress_record(
            18,
            4,
            ExecutionPhase::Implementation,
            "report_write_progress",
            None,
            informational_write_progress_semantics().0,
            "ready to write",
            informational_write_progress_semantics().1,
        ),
    ];
    let progress = implementation_read_progress(&records, 18);
    assert_eq!(progress.recoverable_read_failures, 2);
    assert_eq!(progress.repeated_identical_read_failures, 2);
    assert_eq!(records[3].class, ToolProgressClass::Neutral);
    assert!(!records[3].repository_progress);
    assert_eq!(ToolUsage::default().successful_writes, 0);
    let recovered_read = vec![
        new_tool_progress_record(
            18,
            1,
            ExecutionPhase::Implementation,
            "read_file",
            Some("present.ts".into()),
            ToolProgressClass::RecoverableFailure,
            "line_range_invalid: valid line range is 1-1",
            false,
        ),
        new_tool_progress_record(
            18,
            2,
            ExecutionPhase::Implementation,
            "read_file",
            Some("present.ts".into()),
            ToolProgressClass::Productive,
            "recovered exact range",
            false,
        ),
    ];
    assert!(unresolved_preparation_blockers(&recovered_read, 18, 2, 0).is_empty());
    let no_tool_blockers = unresolved_preparation_blockers(&[], 18, 6, 0);
    assert_eq!(no_tool_blockers.len(), 1);
    assert!(no_tool_blockers[0].contains("6 implementation turns"));
    assert_eq!(
        successful_read_progress("read_file", false, true, false, false).0,
        ToolProgressClass::Productive
    );
    assert_eq!(
        successful_read_progress("related_tests", false, false, true, false).0,
        ToolProgressClass::Productive
    );
    assert_eq!(
        implementation_progress_action(4, 0, 0, 2, 2, false, 4),
        ImplementationProgressAction::FirstWriteDelayed
    );
}

#[test]
fn legacy_progress_observation_cannot_stop_before_the_graph_soft_bound() {
    use crate::execution_graph::{BudgetState, ExecutionNodeId, MissionBudget, NodeBudget};

    assert_eq!(
        implementation_progress_action(4, 1, 0, 0, 0, false, 4),
        ImplementationProgressAction::Continue,
        "four post-write calls are telemetry, not terminal authority"
    );

    let node_id = ExecutionNodeId::new("mutation-progress-boundary");
    let node_budget = NodeBudget {
        max_model_calls: 10,
        max_cost_micros: 10_000,
        max_duration: Duration::from_secs(100),
        max_mutation_fallback_attempts: 2,
    };
    let mut budget = BudgetState::new(MissionBudget::default());
    for _ in 0..4 {
        budget.record_model_call(node_id.clone(), 1, Duration::from_secs(1));
    }
    assert!(!budget.should_stop_node(&node_id, &node_budget));
    let source = hosted_production_source();
    let production = source
        .split_once("#[cfg(test)]\nmod tests")
        .map_or(source, |(production, _)| production);
    assert!(!production.contains("implementation_progress_halt_summary"));
}

#[test]
fn mutation_preflight_block_after_a_write_preserves_useful_partial_validation() {
    let mut blocker = None;
    assert!(mark_mutation_preflight_blocker(
        &mut blocker,
        "src/components/theme/ThemeToggle.tsx",
    ));
    assert!(
        blocker
            .as_deref()
            .unwrap()
            .contains("mutation_preflight_rejected")
    );
    assert_eq!(
        validation_entry_decision(
            ImplementationCompletionStatus::InProgress,
            1,
            false,
            blocker.is_some(),
        ),
        ValidationEntryDecision::UsefulPartialImplementation
    );
}

#[test]
fn replace_text_requires_one_exact_match_and_supports_deletion() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("theme.css");
    fs::write(&path, "root {}\nred {}\n").unwrap();

    replace_unique_repo_text(directory.path(), "theme.css", "red {}", "blue {}").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "root {}\nblue {}\n");

    replace_unique_repo_text(directory.path(), "theme.css", "blue {}\n", "").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "root {}\n");
    assert!(replace_unique_repo_text(directory.path(), "theme.css", "missing", "value").is_err());

    fs::write(&path, "same\nsame\n").unwrap();
    assert!(replace_unique_repo_text(directory.path(), "theme.css", "same", "other").is_err());
}

#[test]
fn safer_write_tools_are_deterministic_and_report_mutation_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("theme.test.ts");
    fs::write(&path, "one\nmarker\ntwo\n").unwrap();

    let range = replace_repo_range(directory.path(), "theme.test.ts", 1, 1, "first").unwrap();
    let range: Value = serde_json::from_str(&range).unwrap();
    assert!(range["before_sha256"].is_string());
    assert!(range["after_sha256"].is_string());
    assert_eq!(range["changed_range"], "1-1");
    assert!(range["diff_summary"].as_str().unwrap().contains("line"));

    let inserted = insert_relative_to_symbol(
        directory.path(),
        "theme.test.ts",
        "marker",
        "\ninserted",
        true,
    )
    .unwrap();
    let inserted: Value = serde_json::from_str(&inserted).unwrap();
    assert_ne!(inserted["before_sha256"], inserted["after_sha256"]);
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("marker\ninserted")
    );

    let rewritten = write_repo_file(directory.path(), "theme.test.ts", "final\n", true).unwrap();
    let rewritten: Value = serde_json::from_str(&rewritten).unwrap();
    assert_eq!(rewritten["changed_range"], "complete_file");
    assert_eq!(fs::read_to_string(&path).unwrap(), "final\n");
}

#[test]
fn unified_diff_tool_is_path_scoped_and_reports_hashes() {
    let directory = tempfile::tempdir().unwrap();
    command::checked("git", ["init", "-q"], directory.path()).unwrap();
    let path = directory.path().join("theme.test.ts");
    fs::write(&path, "old\n").unwrap();
    let output = apply_repo_unified_diff(
        directory.path(),
        "theme.test.ts",
        "--- a/theme.test.ts\n+++ b/theme.test.ts\n@@ -1 +1 @@\n-old\n+new\n",
    )
    .unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_ne!(output["before_sha256"], output["after_sha256"]);
    assert_eq!(output["changed_range"], "unified_diff");
    assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
    assert!(
        apply_repo_unified_diff(
            directory.path(),
            "theme.test.ts",
            "--- a/other.ts\n+++ b/other.ts\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .is_err()
    );
}

#[test]
fn replacement_repair_is_bounded_and_forces_a_safer_strategy() {
    let failed = |error_code: &str| WriteAttemptRecord {
        attempt_index: 0,
        change_id: "theme-tests".into(),
        target: "tests/theme-provider.test.tsx".into(),
        tool: "replace_text".into(),
        status: WriteAttemptStatus::Failed,
        error_code: Some(error_code.into()),
        match_count: Some(2),
        intended_change_sha256: None,
        before_sha256: None,
        after_sha256: None,
    };
    let one = vec![failed("replace_match_not_unique")];
    assert!(
        validate_write_repair_strategy(
            &one,
            "tests/theme-provider.test.tsx",
            "theme-tests",
            "replace_text",
            false,
        )
        .unwrap_err()
        .to_string()
        .contains("bounded read_file")
    );
    assert!(
        validate_write_repair_strategy(
            &one,
            "tests/theme-provider.test.tsx",
            "theme-tests",
            "replace_text",
            true,
        )
        .is_ok()
    );

    let two = vec![
        failed("replace_match_not_unique"),
        failed("replace_match_not_unique"),
    ];
    assert!(
        validate_write_repair_strategy(
            &two,
            "tests/theme-provider.test.tsx",
            "theme-tests",
            "replace_text",
            true,
        )
        .unwrap_err()
        .to_string()
        .contains("strategy exhausted")
    );
    assert!(
        validate_write_repair_strategy(
            &two,
            "tests/theme-provider.test.tsx",
            "theme-tests",
            "rewrite_small_file",
            true,
        )
        .is_ok()
    );

    let four = vec![
        failed("replace_match_not_unique"),
        failed("replace_match_not_unique"),
        failed("replace_match_not_unique"),
        failed("replace_match_not_unique"),
    ];
    assert!(
        validate_write_repair_strategy(
            &four,
            "tests/theme-provider.test.tsx",
            "theme-tests",
            "write_file",
            true,
        )
        .unwrap_err()
        .to_string()
        .contains("content repair circuit breaker")
    );
    assert!(
        validate_write_repair_strategy(
            &four,
            "tests/theme-provider.test.tsx",
            "theme-tests",
            "rewrite_small_file",
            true,
        )
        .is_err()
    );
}

#[test]
fn multi_file_plans_normalize_legacy_targets_and_authorize_membership() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src/components/theme")).unwrap();
    for path in ["ThemeProvider.tsx", "ThemeToggle.tsx"] {
        fs::write(
            directory.path().join("src/components/theme").join(path),
            "export {};\n",
        )
        .unwrap();
    }
    let mut change = test_planned_change();
    change.change_id = "theme-registry-light-blue".into();
    change.parent_change_id = Some("theme-registry".into());
    change.path = "src/components/theme/ThemeProvider.tsx; src/components/theme/ThemeToggle.tsx; src/components/theme/ThemeProvider.tsx".into();
    change.targets.clear();
    let repair = repair_implementation_plan(
        std::slice::from_mut(&mut change),
        "theme-registry-light-blue",
        "src/components/theme/ThemeProvider.tsx",
    )
    .unwrap()
    .unwrap();
    assert!(!repair.model_call_consumed);
    assert_eq!(repair.repair_source, "orchestrator_normalization");
    assert_eq!(repair.targets_before.len(), 1);
    assert_eq!(
        change
            .targets
            .iter()
            .map(|target| target.path.as_str())
            .collect::<Vec<_>>(),
        [
            "src/components/theme/ThemeProvider.tsx",
            "src/components/theme/ThemeToggle.tsx"
        ]
    );
    validate_planned_change_paths(directory.path(), std::slice::from_ref(&change)).unwrap();
    let plan = ImplementationPlan {
        implementation_status: "ready".into(),
        planned_changes: vec![change],
        planned_new_files: vec![],
        planned_test_changes: vec![],
        remaining_unknowns: vec![],
        blocking_unknowns: vec![],
    };
    assert!(
        authorize_planned_target(
            &plan,
            "theme-registry-light-blue",
            "src/components/theme/ThemeProvider.tsx"
        )
        .is_ok()
    );
    assert!(
        authorize_planned_target(
            &plan,
            "theme-registry-light-blue",
            "src/components/theme/ThemeToggle.tsx"
        )
        .is_ok()
    );
    let rejected =
        authorize_planned_target(&plan, "theme-registry-light-blue", "src/styles/globals.css")
            .unwrap_err();
    assert_eq!(rejected.code, "mutation_plan_metadata_mismatch");
    assert_eq!(rejected.repair_strategy, "repair_plan_metadata");
    let serialized = serde_json::to_value(&plan).unwrap();
    assert!(serialized["planned_changes"][0].get("path").is_none());
    assert!(serialized["planned_changes"][0]["targets"].is_array());
    assert_eq!(
        serialized["planned_changes"][0]["parent_change_id"],
        "theme-registry"
    );
}

#[test]
fn independently_editable_changes_may_share_one_logical_parent() {
    let mut provider = test_planned_change();
    provider.change_id = "theme-provider-light-blue".into();
    provider.parent_change_id = Some("theme-registry-light-blue".into());
    let mut toggle = test_planned_change();
    toggle.change_id = "theme-toggle-light-blue".into();
    toggle.parent_change_id = Some("theme-registry-light-blue".into());
    toggle.targets[0].path = "src/components/theme/ThemeToggle.tsx".into();

    normalize_planned_changes(&mut [provider.clone(), toggle.clone()]).unwrap();

    assert_eq!(provider.parent_change_id, toggle.parent_change_id);
    assert_ne!(provider.change_id, toggle.change_id);
}

#[test]
fn preflight_rejection_is_not_an_executed_write_and_halts_tool_switching() {
    let mut notebook = test_discovery_notebook(ExecutionPhase::Implementation);
    let mut usage = ToolUsage::default();
    let preflight = MutationPreflightError {
        code: "mutation_plan_metadata_mismatch",
        change_id: "theme-registry-light-blue".into(),
        target: "src/components/theme/ThemeProvider.tsx".into(),
        message: "target is not a member of its planned target set".into(),
        repair_strategy: "repair_plan_metadata",
    };

    let first = record_mutation_preflight_rejection(&mut notebook, &mut usage, &preflight);
    assert!(first.halt_orchestration);
    assert!(!first.repeated);
    assert_eq!(usage.write_preflight_rejections, 1);
    assert_eq!(usage.write_execution_failures, 0);
    assert_eq!(usage.failed_writes, 0);
    assert!(notebook.write_attempts.is_empty());
    assert!(!notebook.write_preflight_rejections[0].mutation_attempted);

    let repeated = record_mutation_preflight_rejection(&mut notebook, &mut usage, &preflight);
    assert!(repeated.repeated);
    assert_eq!(notebook.write_preflight_rejections[0].occurrences, 2);
    assert_eq!(usage.write_execution_failures, 0);
}

#[test]
fn preparation_progress_is_guidance_only_until_the_graph_stops_the_node() {
    assert_eq!(
        implementation_progress_action(4, 0, 4, 2, 1, false, 4),
        ImplementationProgressAction::Continue
    );
    assert_eq!(
        implementation_progress_action(6, 0, 6, 2, 1, false, 6),
        ImplementationProgressAction::FirstWriteDelayed
    );
    assert_eq!(
        implementation_progress_action(7, 0, 1, 3, 1, true, 7),
        ImplementationProgressAction::Continue
    );
    assert_eq!(
        implementation_progress_action(8, 0, 1, 3, 1, true, 8),
        ImplementationProgressAction::Continue
    );
    assert_eq!(
        implementation_progress_action(12, 1, 0, 0, 0, true, 4),
        ImplementationProgressAction::Continue
    );
}

#[test]
fn persisted_legacy_intended_change_resumes_with_structured_targets() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("provider.tsx"), "export {};\n").unwrap();
    let mut notebook = test_discovery_notebook(ExecutionPhase::Implementation);
    notebook.planned_changes = vec![
        serde_json::from_value(json!({
            "change_id": "theme-registry-light-blue",
            "path": "provider.tsx; toggle.tsx",
            "change": "Expose light blue theme",
            "reason": "Theme selection",
            "acceptance_criteria": ["ac-1"]
        }))
        .unwrap(),
    ];
    notebook.intended_changes = vec![
        serde_json::from_value(json!({
            "change_id": "theme-registry-light-blue",
            "intent": "Expose light blue theme",
            "status": "applied",
            "target": "provider.tsx; toggle.tsx"
        }))
        .unwrap(),
    ];

    normalize_notebook_intended_changes(&mut notebook, directory.path()).unwrap();

    assert_eq!(
        notebook.intended_changes[0].status,
        IntendedChangeStatus::Applied
    );
    assert_eq!(
        notebook.intended_changes[0]
            .targets
            .iter()
            .map(|target| target.path.as_str())
            .collect::<Vec<_>>(),
        vec!["provider.tsx", "toggle.tsx"]
    );
    let persisted = serde_json::to_value(&notebook.intended_changes[0]).unwrap();
    assert!(persisted.get("target").is_none());
    assert_eq!(persisted["targets"].as_array().unwrap().len(), 2);
}

#[test]
fn per_target_status_rolls_up_without_claiming_multi_file_completion() {
    let mut change = test_planned_change();
    change.targets.push(PlannedTarget {
        path: "src/components/theme/ThemeToggle.tsx".into(),
        role: "selector cycling".into(),
        operation: Default::default(),
        new_file: false,
        status: IntendedChangeStatus::Planned,
    });
    change.targets[0].status = IntendedChangeStatus::Applied;
    assert_eq!(
        roll_up_target_statuses(&change.targets),
        IntendedChangeStatus::Partial
    );
    for target in &mut change.targets {
        target.status = IntendedChangeStatus::Verified;
    }
    assert_eq!(
        roll_up_target_statuses(&change.targets),
        IntendedChangeStatus::Verified
    );
}

#[test]
fn plan_validation_rejects_missing_paths_unless_explicitly_new() {
    let directory = tempfile::tempdir().unwrap();
    let mut change = test_planned_change();
    change.targets[0].path = "src/new-theme.ts".into();
    assert!(
        validate_planned_change_paths(directory.path(), std::slice::from_ref(&change)).is_err()
    );
    change.targets[0].new_file = true;
    validate_planned_change_paths(directory.path(), std::slice::from_ref(&change)).unwrap();
}

#[test]
fn later_equivalent_write_recovers_different_attempt_hashes_by_change_id() {
    let mut failures = vec![test_write_failure(
        "theme-tests",
        "tests/theme-provider.test.tsx",
        "first-hash",
    )];
    let attempts = vec![WriteAttemptRecord {
        attempt_index: 1,
        change_id: "theme-tests".into(),
        target: "tests/theme-provider.test.tsx".into(),
        tool: "replace_range".into(),
        status: WriteAttemptStatus::Applied,
        error_code: None,
        match_count: None,
        intended_change_sha256: Some("different-hash".into()),
        before_sha256: Some("before".into()),
        after_sha256: Some("after".into()),
    }];
    reconcile_failed_write_attempts(
        &mut failures,
        &[test_planned_change()],
        &attempts,
        &test_complete_implementation(),
        &[test_passed_validation("npm test")],
        &["tests/theme-provider.test.tsx".into()],
    );
    assert!(failures[0].recovered);
    assert_eq!(
        failures[0].reconciliation,
        FailureReconciliation::Superseded
    );
}

#[test]
fn successful_noop_attempt_does_not_supersede_a_failed_change() {
    let mut failures = vec![test_write_failure(
        "theme-tests",
        "tests/theme-provider.test.tsx",
        "first-hash",
    )];
    let attempts = vec![WriteAttemptRecord {
        attempt_index: 1,
        change_id: "theme-tests".into(),
        target: "tests/theme-provider.test.tsx".into(),
        tool: "replace_range".into(),
        status: WriteAttemptStatus::Applied,
        error_code: None,
        match_count: None,
        intended_change_sha256: Some("different-hash".into()),
        before_sha256: Some("unchanged".into()),
        after_sha256: Some("unchanged".into()),
    }];
    reconcile_failed_write_attempts(
        &mut failures,
        &[test_planned_change()],
        &attempts,
        &ImplementationOutcome {
            summary: String::new(),
            budget_exhausted: false,
            explicit_declaration: None,
        },
        &[],
        &[],
    );
    assert!(!failures[0].recovered);
    assert_eq!(
        failures[0].reconciliation,
        FailureReconciliation::StillUnresolved
    );
}

#[test]
fn an_earlier_success_does_not_supersede_a_later_failure() {
    let mut failure = test_write_failure(
        "theme-tests",
        "tests/theme-provider.test.tsx",
        "failed-hash",
    );
    failure.attempt_index = 1;
    let attempts = vec![WriteAttemptRecord {
        attempt_index: 0,
        change_id: "theme-tests".into(),
        target: "tests/theme-provider.test.tsx".into(),
        tool: "replace_range".into(),
        status: WriteAttemptStatus::Applied,
        error_code: None,
        match_count: None,
        intended_change_sha256: Some("earlier-hash".into()),
        before_sha256: Some("before".into()),
        after_sha256: Some("after".into()),
    }];
    reconcile_failed_write_attempts(
        std::slice::from_mut(&mut failure),
        &[test_planned_change()],
        &attempts,
        &ImplementationOutcome {
            summary: String::new(),
            budget_exhausted: false,
            explicit_declaration: None,
        },
        &[],
        &[],
    );
    assert!(!failure.recovered);
}

#[test]
fn whole_file_write_supersedes_all_prior_failures_on_its_target() {
    let mut failures = vec![
        test_write_failure("theme-tests", "tests/theme-provider.test.tsx", "hash-a"),
        test_write_failure("other-intent", "tests/theme-provider.test.tsx", "hash-b"),
    ];
    let attempts = vec![WriteAttemptRecord {
        attempt_index: 2,
        change_id: "theme-tests".into(),
        target: "tests/theme-provider.test.tsx".into(),
        tool: "rewrite_small_file".into(),
        status: WriteAttemptStatus::Applied,
        error_code: None,
        match_count: None,
        intended_change_sha256: Some("hash-c".into()),
        before_sha256: Some("before".into()),
        after_sha256: Some("after".into()),
    }];
    reconcile_failed_write_attempts(
        &mut failures,
        &[test_planned_change()],
        &attempts,
        &test_complete_implementation(),
        &[test_passed_validation("npm test")],
        &["tests/theme-provider.test.tsx".into()],
    );
    assert!(failures.iter().all(|failure| failure.recovered));
    assert!(
        failures
            .iter()
            .all(|failure| { failure.reconciliation == FailureReconciliation::Superseded })
    );
}

#[test]
fn final_diff_and_validation_recover_incident_but_validation_alone_does_not() {
    let mut recovered = vec![test_write_failure(
        "theme-tests",
        "tests/theme-provider.test.tsx",
        "hash-a",
    )];
    let validation = vec![
        test_passed_validation("npm test"),
        test_passed_validation("npm run build"),
    ];
    reconcile_failed_write_attempts(
        &mut recovered,
        &[test_planned_change()],
        &[],
        &test_complete_implementation(),
        &validation,
        &["tests/theme-provider.test.tsx".into()],
    );
    assert_eq!(
        recovered[0].reconciliation,
        FailureReconciliation::Recovered
    );
    assert!(
        recovered[0]
            .recovery
            .as_ref()
            .unwrap()
            .evidence
            .iter()
            .any(|evidence| evidence == "npm run build passed.")
    );

    let mut absent = vec![test_write_failure(
        "theme-tests",
        "tests/theme-provider.test.tsx",
        "hash-a",
    )];
    reconcile_failed_write_attempts(
        &mut absent,
        &[test_planned_change()],
        &[],
        &test_complete_implementation(),
        &validation,
        &[],
    );
    assert!(!absent[0].recovered);
    assert_eq!(
        absent[0].reconciliation,
        FailureReconciliation::StillUnresolved
    );
}

#[test]
fn fallback_populates_code_evidence_and_passed_validation_from_final_state() {
    let implementation = test_complete_implementation();
    let plan = ImplementationPlan {
        implementation_status: "ready".into(),
        planned_changes: vec![test_planned_change()],
        planned_new_files: vec![],
        planned_test_changes: vec!["tests/theme-provider.test.tsx".into()],
        remaining_unknowns: vec![],
        blocking_unknowns: vec![],
    };
    let result = completion_fallback(
        &implementation,
        None,
        Some(&plan),
        &[],
        &["tests/theme-provider.test.tsx".into()],
        &["Theme can be selected".into()],
        &[
            test_passed_validation("npm test"),
            test_passed_validation("npm run build"),
        ],
        ProjectVerificationPolicy {
            browser_e2e_required_for_theme_changes: false,
            manual_browser_verification_required: false,
        },
    );
    let criterion = &result.criteria[0];
    assert_eq!(criterion.status, CriterionStatus::Satisfied);
    assert!(!criterion.evidence.is_empty());
    assert_eq!(
        criterion.validation_evidence,
        vec!["npm test", "npm run build"]
    );
}

#[test]
fn passing_validation_cannot_complete_an_unchanged_required_target() {
    let implementation = test_complete_implementation();
    let mut change = test_planned_change();
    change.targets.push(PlannedTarget {
        path: "src/components/theme/ThemeToggle.tsx".into(),
        role: "selector cycling".into(),
        operation: Default::default(),
        new_file: false,
        status: IntendedChangeStatus::Planned,
    });
    let plan = ImplementationPlan {
        implementation_status: "ready".into(),
        planned_changes: vec![change],
        planned_new_files: vec![],
        planned_test_changes: vec![],
        remaining_unknowns: vec![],
        blocking_unknowns: vec![],
    };
    let result = completion_fallback(
        &implementation,
        None,
        Some(&plan),
        &[],
        &["tests/theme-provider.test.tsx".into()],
        &["Theme can be selected".into()],
        &[test_passed_validation("npm test")],
        ProjectVerificationPolicy::default(),
    );

    assert_eq!(result.criteria[0].status, CriterionStatus::Unsatisfied);
    assert_ne!(
        result.implementation_completeness,
        ImplementationCompleteness::Complete
    );
    assert!(
        result.criteria[0].missing_evidence[0].contains("src/components/theme/ThemeToggle.tsx")
    );
}

#[test]
fn planned_changes_receive_stable_unique_change_ids() {
    let mut first = test_planned_change();
    first.change_id.clear();
    let mut second = first.clone();
    second.path = "src/components/theme/ThemeProvider.tsx".into();
    normalize_planned_changes(&mut [first.clone(), second]).unwrap();

    let mut repeated = vec![first];
    normalize_planned_changes(&mut repeated).unwrap();
    let first_id = repeated[0].change_id.clone();
    normalize_planned_changes(&mut repeated).unwrap();
    assert_eq!(repeated[0].change_id, first_id);

    let mut duplicates = vec![test_planned_change(), test_planned_change()];
    assert!(normalize_planned_changes(&mut duplicates).is_err());
}

#[test]
fn exact_five_target_diff_produces_an_authoritative_complete_declaration() {
    let criterion = "Light-blue theme behavior is implemented and covered.".to_owned();
    let paths = [
        "src/components/theme/ThemeProvider.tsx",
        "src/components/theme/ThemeToggle.tsx",
        "src/styles/globals.css",
        "tests/theme-provider.test.tsx",
        "tests/theme-tokens.test.ts",
    ];
    let planned = paths
        .iter()
        .enumerate()
        .map(|(index, path)| PlannedChange {
            change_id: format!("aops-226-target-{}", index + 1),
            parent_change_id: None,
            path: String::new(),
            targets: vec![PlannedTarget {
                path: (*path).into(),
                role: "required AOPS-226 target".into(),
                operation: Default::default(),
                new_file: path.starts_with("tests/"),
                status: IntendedChangeStatus::Applied,
            }],
            change: format!("Implement light-blue behavior in {path}."),
            reason: "Complete the localized theme change.".into(),
            status: IntendedChangeStatus::Applied,
            acceptance_criteria: vec![criterion.clone()],
            test_coverage: vec!["focused theme tests".into()],
        })
        .collect::<Vec<_>>();
    let changed_paths = paths
        .iter()
        .map(|path| (*path).to_owned())
        .collect::<Vec<_>>();

    let declaration = deterministic_complete_declaration(
        &planned,
        std::slice::from_ref(&criterion),
        &changed_paths,
        &[],
        &[],
    )
    .expect("all five applied targets should produce a declaration");

    assert_eq!(declaration.implementation_status, "complete");
    assert_eq!(declaration.changed_paths, changed_paths);
    assert_eq!(declaration.criteria_evidence.len(), 1);
    assert_eq!(declaration.criteria_evidence[0].paths.len(), 5);
    assert!(declaration.remaining_work.is_empty());
}

#[test]
fn resumed_reconciliation_unions_committed_and_dirty_target_paths() {
    let work = tempfile::tempdir().unwrap();
    command::checked("git", ["init", "-q"], work.path()).unwrap();
    fs::write(work.path().join("provider.tsx"), "provider base\n").unwrap();
    fs::write(work.path().join("toggle.tsx"), "toggle base\n").unwrap();
    command::checked("git", ["add", "."], work.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
        work.path(),
    )
    .unwrap();
    let base_sha = command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap();

    fs::write(work.path().join("provider.tsx"), "provider committed\n").unwrap();
    command::checked("git", ["add", "provider.tsx"], work.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "first resumed target",
        ],
        work.path(),
    )
    .unwrap();
    fs::write(work.path().join("toggle.tsx"), "toggle dirty\n").unwrap();

    let repo = Repo {
        root: work.path().to_path_buf(),
    };
    let changed_paths = completion_changed_paths(&repo, &base_sha).unwrap();
    assert_eq!(changed_paths, ["provider.tsx", "toggle.tsx"]);
    let review = completion_review_diff(work.path(), &changed_paths, &base_sha).unwrap();
    assert!(review.contains("provider committed"));
    assert!(review.contains("toggle dirty"));

    let mut intended_changes = vec![IntendedChangeRecord {
        change_id: "theme-targets".into(),
        intent: "Apply both resumed theme targets".into(),
        status: IntendedChangeStatus::Partial,
        target: String::new(),
        targets: vec![
            PlannedTarget {
                path: "provider.tsx".into(),
                role: "previously committed target".into(),
                operation: Default::default(),
                new_file: false,
                status: IntendedChangeStatus::Applied,
            },
            PlannedTarget {
                path: "toggle.tsx".into(),
                role: "new dirty target".into(),
                operation: Default::default(),
                new_file: false,
                status: IntendedChangeStatus::Planned,
            },
        ],
        attempts: Vec::new(),
        recovery: None,
    }];
    reconcile_changed_target_statuses(&mut intended_changes, &changed_paths.into_iter().collect());
    assert_eq!(
        intended_changes[0].targets[0].status,
        IntendedChangeStatus::Applied
    );
    assert_eq!(
        intended_changes[0].targets[1].status,
        IntendedChangeStatus::InProgress
    );
    assert_eq!(intended_changes[0].status, IntendedChangeStatus::Partial);
}

fn cancellation_preservation_fixture()
-> (tempfile::TempDir, tempfile::TempDir, Repo, String, String) {
    let work = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    command::checked(
        "git",
        ["init", "--bare", "-q", remote.path().to_str().unwrap()],
        work.path(),
    )
    .unwrap();
    command::checked("git", ["init", "-q"], work.path()).unwrap();
    command::checked("git", ["config", "user.name", "Test"], work.path()).unwrap();
    command::checked(
        "git",
        ["config", "user.email", "test@example.com"],
        work.path(),
    )
    .unwrap();
    command::checked("git", ["config", "commit.gpgsign", "false"], work.path()).unwrap();
    fs::write(work.path().join("base.txt"), "base\n").unwrap();
    command::checked("git", ["add", "base.txt"], work.path()).unwrap();
    command::checked("git", ["commit", "--quiet", "-m", "base"], work.path()).unwrap();
    command::checked("git", ["branch", "-M", "main"], work.path()).unwrap();
    command::checked(
        "git",
        ["remote", "add", "origin", remote.path().to_str().unwrap()],
        work.path(),
    )
    .unwrap();
    command::checked("git", ["push", "-q", "origin", "main"], work.path()).unwrap();
    let base_sha = command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap();
    let branch = "rustgrid/aops-226-cancellation".to_owned();
    command::checked("git", ["switch", "-q", "-c", branch.as_str()], work.path()).unwrap();
    let repo = Repo {
        root: work.path().to_path_buf(),
    };
    (work, remote, repo, base_sha, branch)
}

#[test]
fn cancellation_preservation_commits_dirty_agent_paths_and_pushes_them() {
    let (work, remote, repo, base_sha, branch) = cancellation_preservation_fixture();
    fs::write(
        work.path().join("worker.txt"),
        "durable cancellation work\n",
    )
    .unwrap();

    let preserved = preserve_cancellation_branch_with(
        &repo,
        &base_sha,
        &branch,
        "AOPS-226: cancellation checkpoint",
        |branch, commit_sha| repo.push(branch, commit_sha, "fixture-token", "https://github.com"),
    )
    .unwrap()
    .expect("dirty cancellation work must be preserved");

    assert!(preserved.commit_created);
    assert!(preserved.push_performed);
    assert!(!preserved.remote_already_current);
    assert_eq!(preserved.committed_paths, ["worker.txt"]);
    assert_eq!(preserved.changed_paths, ["worker.txt"]);
    assert!(repo.new_agent_paths(&BTreeSet::new()).unwrap().is_empty());
    let remote_head = command::checked(
        "git",
        [
            "--git-dir",
            remote.path().to_str().unwrap(),
            "rev-parse",
            &format!("refs/heads/{branch}"),
        ],
        work.path(),
    )
    .unwrap();
    assert_eq!(remote_head, preserved.commit_sha);
}

#[test]
fn cancellation_preservation_reuses_an_already_committed_head() {
    let (work, remote, repo, base_sha, branch) = cancellation_preservation_fixture();
    fs::write(work.path().join("worker.txt"), "already committed work\n").unwrap();
    let committed = repo
        .commit_paths(&["worker.txt".into()], "AOPS-226: prior worker commit")
        .unwrap();

    let preserved = preserve_cancellation_branch_with(
        &repo,
        &base_sha,
        &branch,
        "AOPS-226: cancellation checkpoint",
        |branch, commit_sha| repo.push(branch, commit_sha, "fixture-token", "https://github.com"),
    )
    .unwrap()
    .expect("the committed base-to-head diff must be preserved");

    assert!(!preserved.commit_created);
    assert!(preserved.push_performed);
    assert!(!preserved.remote_already_current);
    assert!(preserved.committed_paths.is_empty());
    assert_eq!(preserved.changed_paths, ["worker.txt"]);
    assert_eq!(preserved.commit_sha, committed);
    assert_eq!(
        command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap(),
        committed
    );
    let remote_head = command::checked(
        "git",
        [
            "--git-dir",
            remote.path().to_str().unwrap(),
            "rev-parse",
            &format!("refs/heads/{branch}"),
        ],
        work.path(),
    )
    .unwrap();
    assert_eq!(remote_head, committed);
}

#[test]
fn cancellation_preservation_recognizes_an_already_current_remote() {
    let (work, _remote, repo, base_sha, branch) = cancellation_preservation_fixture();
    fs::write(work.path().join("worker.txt"), "already remote work\n").unwrap();
    let committed = repo
        .commit_paths(&["worker.txt".into()], "AOPS-226: prior worker commit")
        .unwrap();
    assert!(
        repo.push(&branch, &committed, "fixture-token", "https://github.com")
            .unwrap()
    );

    let preserved = preserve_cancellation_branch_with(
        &repo,
        &base_sha,
        &branch,
        "AOPS-226: cancellation checkpoint",
        |branch, commit_sha| {
            repo.push(
                branch,
                commit_sha,
                "fresh-fixture-token",
                "https://github.com",
            )
        },
    )
    .unwrap()
    .expect("an already-current remote is still durably preserved");

    assert!(!preserved.commit_created);
    assert!(!preserved.push_performed);
    assert!(preserved.remote_already_current);
    assert_eq!(preserved.commit_sha, committed);
}

#[test]
fn restored_validation_results_reuse_current_unattached_graph_evidence() {
    use crate::execution_graph::{
        ExecutionNodeKind, ExecutionNodeStatus, ExecutionSnapshot, MissionBudget,
        MissionComplexity, PlannedTarget as GraphTarget, RepositorySnapshot,
        ValidationEvidenceRecord, ValidationEvidenceStatus, ValidationGateSpec,
        ValidationGateType as GraphValidationGateType, build_execution_graph,
    };

    let repository_fingerprint = "resumed-tree".to_owned();
    let mut graph = build_execution_graph(
        "resumed-validation-results",
        MissionComplexity::Tiny,
        &repository_fingerprint,
        &[GraphTarget {
            change_id: "change-one".into(),
            path: "src/one.rs".into(),
            role: "production".into(),
            intent: "update one".into(),
            acceptance_criteria_ids: vec!["ac-1".into()],
            operation: Default::default(),
            new_file: false,
        }],
        &[ValidationGateSpec {
            gate_id: "tests".into(),
            gate_type: GraphValidationGateType::TestSuite,
            command: "cargo test".into(),
            working_directory: ".".into(),
            required: true,
            dependency_lock_hash: "lock".into(),
            relevant_environment_fingerprint: "env".into(),
        }],
        &MissionBudget::for_complexity(MissionComplexity::Tiny),
    );
    let mutation_id = graph
        .nodes
        .iter()
        .find(|node| node.kind.is_mutation())
        .expect("mutation node")
        .id
        .clone();
    graph
        .set_node_status(&mutation_id, ExecutionNodeStatus::Applied)
        .unwrap();
    let validation = graph
        .nodes
        .iter()
        .find(|node| node.kind.is_validation())
        .expect("validation node")
        .clone();
    let gate = validation.validation.clone().unwrap();
    let mut snapshot = ExecutionSnapshot {
        run_id: "resumed-validation-results".into(),
        current_repository: RepositorySnapshot {
            fingerprint: repository_fingerprint.clone(),
            changed_paths: BTreeSet::from(["src/one.rs".into()]),
            ..RepositorySnapshot::default()
        },
        graph,
        ..ExecutionSnapshot::default()
    };
    snapshot
        .evidence
        .record_validation(ValidationEvidenceRecord {
            evidence_id: "resumed-tests".into(),
            node_id: validation.id,
            gate_id: gate.gate_id.clone(),
            fingerprint: gate.fingerprint(&repository_fingerprint),
            repository_fingerprint,
            command: gate.command.clone(),
            working_directory: gate.working_directory.clone(),
            status: ValidationEvidenceStatus::Passed,
            exit_code: Some(0),
            output_summary: "reused".into(),
            duration: Duration::from_millis(1),
        });

    let results = restored_validation_results_from_snapshot(&snapshot).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "tests");
    assert_eq!(results[0].output, "reused");
    assert!(
        snapshot
            .graph
            .nodes
            .iter()
            .filter(|node| node.kind == ExecutionNodeKind::ValidationSuite)
            .all(|node| !node.status.is_success() && node.evidence_ids.is_empty()),
        "restoration must not require stale node materialization"
    );
}

#[test]
fn remote_reconciliation_reestablishes_fingerprint_bound_graph_finalization() {
    use crate::execution_graph::{
        ExecutionDomainEvent, ExecutionNodeKind, ExecutionNodeStatus, MissionBudget,
        MissionComplexity, MissionOutcome, PlannedTarget as GraphTarget, PublicationMode,
        PublicationStatus, RepositorySnapshot, ValidationEvidenceRecord, ValidationEvidenceStatus,
        ValidationGateSpec, ValidationGateType as GraphValidationGateType, build_execution_graph,
    };

    let work = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    command::checked(
        "git",
        ["init", "--bare", "-q", remote.path().to_str().unwrap()],
        work.path(),
    )
    .unwrap();
    command::checked("git", ["init", "-q"], work.path()).unwrap();
    command::checked("git", ["config", "user.name", "Test"], work.path()).unwrap();
    command::checked(
        "git",
        ["config", "user.email", "test@example.com"],
        work.path(),
    )
    .unwrap();
    fs::write(work.path().join("base.txt"), "base\n").unwrap();
    command::checked("git", ["add", "."], work.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
        work.path(),
    )
    .unwrap();
    command::checked("git", ["branch", "-M", "main"], work.path()).unwrap();
    command::checked(
        "git",
        ["remote", "add", "origin", remote.path().to_str().unwrap()],
        work.path(),
    )
    .unwrap();
    command::checked("git", ["push", "-q", "origin", "main"], work.path()).unwrap();
    let base_sha = command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap();
    let branch = "rustgrid/reconciled-finalization";
    command::checked("git", ["switch", "-q", "-c", branch], work.path()).unwrap();
    fs::write(work.path().join("worker.txt"), "worker change\n").unwrap();
    command::checked("git", ["add", "worker.txt"], work.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "worker change",
        ],
        work.path(),
    )
    .unwrap();
    let worker_commit = command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap();

    let collaborator = tempfile::tempdir().unwrap();
    command::checked("git", ["init", "-q"], collaborator.path()).unwrap();
    command::checked(
        "git",
        ["config", "user.name", "Collaborator"],
        collaborator.path(),
    )
    .unwrap();
    command::checked(
        "git",
        ["config", "user.email", "collaborator@example.com"],
        collaborator.path(),
    )
    .unwrap();
    command::checked(
        "git",
        ["remote", "add", "origin", remote.path().to_str().unwrap()],
        collaborator.path(),
    )
    .unwrap();
    command::checked(
        "git",
        ["fetch", "-q", "origin", "main"],
        collaborator.path(),
    )
    .unwrap();
    command::checked(
        "git",
        ["switch", "-q", "-c", branch, "FETCH_HEAD"],
        collaborator.path(),
    )
    .unwrap();
    fs::write(collaborator.path().join("remote.txt"), "remote change\n").unwrap();
    command::checked("git", ["add", "remote.txt"], collaborator.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "remote change",
        ],
        collaborator.path(),
    )
    .unwrap();
    command::checked("git", ["push", "-q", "origin", branch], collaborator.path()).unwrap();

    let repo = Repo {
        root: work.path().to_path_buf(),
    };
    let old_fingerprint = repository_state_fingerprint(&repo, &base_sha).unwrap();
    let reconciled = repo
        .reconcile_remote_branch(branch, &worker_commit, "token", "https://github.com")
        .unwrap();
    assert!(reconciled.requires_validation());
    assert_ne!(reconciled.commit, worker_commit);
    assert_eq!(
        command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap(),
        reconciled.commit
    );
    let reconciled_fingerprint = repository_state_fingerprint(&repo, &base_sha).unwrap();
    assert_ne!(reconciled_fingerprint, old_fingerprint);

    let mut graph = build_execution_graph(
        "remote-reconciliation-route",
        MissionComplexity::Small,
        &old_fingerprint,
        &[GraphTarget {
            change_id: "worker-change".into(),
            path: "worker.txt".into(),
            role: "source change".into(),
            intent: "apply the worker change".into(),
            acceptance_criteria_ids: vec!["ac-1".into()],
            operation: Default::default(),
            new_file: true,
        }],
        &[ValidationGateSpec {
            gate_id: "test".into(),
            gate_type: GraphValidationGateType::TestSuite,
            command: "true".into(),
            working_directory: work.path().to_string_lossy().into_owned(),
            required: true,
            ..ValidationGateSpec::default()
        }],
        &MissionBudget::for_complexity(MissionComplexity::Small),
    );
    let mutation_node = graph
        .nodes
        .iter()
        .find(|node| node.kind.is_mutation())
        .unwrap()
        .id
        .clone();
    let validation_node = graph
        .nodes
        .iter()
        .find(|node| node.kind.is_validation())
        .unwrap()
        .id
        .clone();
    let review_node = graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::DiffReview)
        .unwrap()
        .id
        .clone();
    let completion_node = graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
        .unwrap()
        .id
        .clone();
    let publication_node = graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::Publication)
        .unwrap()
        .id
        .clone();
    graph
        .set_node_status(&mutation_node, ExecutionNodeStatus::Completed)
        .unwrap();
    graph
        .set_node_status(&validation_node, ExecutionNodeStatus::Passed)
        .unwrap();
    graph
        .node_mut(&validation_node)
        .unwrap()
        .evidence_ids
        .push("old-validation".into());
    graph
        .set_node_status(&review_node, ExecutionNodeStatus::Completed)
        .unwrap();
    graph
        .set_node_status(&completion_node, ExecutionNodeStatus::Completed)
        .unwrap();
    graph
        .set_node_status(&publication_node, ExecutionNodeStatus::Running)
        .unwrap();

    let mut checkpoint = HostedOrchestrationCheckpoint {
        graph_revision: graph.revision,
        graph: Some(graph),
        legacy_import_completed: true,
        ..HostedOrchestrationCheckpoint::default()
    };
    checkpoint
        .evidence
        .record_validation(ValidationEvidenceRecord {
            evidence_id: "old-validation".into(),
            node_id: validation_node.clone(),
            gate_id: "test".into(),
            fingerprint: "old-command-fingerprint".into(),
            repository_fingerprint: old_fingerprint.clone(),
            command: "true".into(),
            working_directory: work.path().to_string_lossy().into_owned(),
            status: ValidationEvidenceStatus::Passed,
            exit_code: Some(0),
            output_summary: String::new(),
            duration: Duration::from_millis(1),
        });
    checkpoint.publication.status = PublicationStatus::CommitCreated;
    checkpoint.publication.commit_sha = Some(worker_commit.clone());
    checkpoint.domain_events = vec![
        ExecutionDomainEvent::ValidationPassed {
            sequence: 1,
            node_id: validation_node.clone(),
            evidence_id: "old-validation".into(),
            fingerprint: "old-command-fingerprint".into(),
        },
        ExecutionDomainEvent::DiffReviewed {
            sequence: 2,
            node_id: review_node.clone(),
            evidence_ids: vec!["old-validation".into()],
        },
        ExecutionDomainEvent::CompletionEvaluated {
            sequence: 3,
            node_id: completion_node.clone(),
            outcome: MissionOutcome::Complete,
        },
        ExecutionDomainEvent::PublicationStarted {
            sequence: 4,
            node_id: publication_node.clone(),
            mode: PublicationMode::Normal,
        },
        ExecutionDomainEvent::CommitCreated {
            sequence: 5,
            node_id: publication_node.clone(),
            commit_sha: worker_commit,
        },
    ];
    let revalidation = FinalizationRevalidation {
        repository_fingerprint: reconciled_fingerprint.clone(),
        invalidated_after_sequence: 5,
    };
    let stale_manifest = test_manifest(Uuid::from_u128(9_225));
    let mut stale_notebook =
        new_worker_notebook(&stale_manifest, reconciled_fingerprint.clone(), None);
    stale_notebook.orchestration = checkpoint.clone();
    assert!(notebook_finalization_requires_revalidation(
        &stale_notebook,
        &reconciled_fingerprint,
        &completion_changed_paths(&repo, &base_sha).unwrap(),
    ));
    let invalidation =
        finalization_invalidation_event(&checkpoint, 6, &reconciled_fingerprint).unwrap();
    assert!(matches!(
        &invalidation,
        ExecutionDomainEvent::FinalizationInvalidated {
            repository_fingerprint,
            stale_validation_evidence_ids,
            ..
        } if repository_fingerprint == &reconciled_fingerprint
            && stale_validation_evidence_ids == &["old-validation".to_owned()]
    ));
    let mut invalidated_snapshot = checkpoint.snapshot(
        "remote-reconciliation-invalidation",
        RepositorySnapshot {
            fingerprint: reconciled_fingerprint.clone(),
            source_tree_hash: reconciled_fingerprint.clone(),
            changed_paths: completion_changed_paths(&repo, &base_sha)
                .unwrap()
                .into_iter()
                .collect(),
            ..RepositorySnapshot::default()
        },
    );
    invalidated_snapshot.append_event(invalidation).unwrap();
    checkpoint.replace_from_snapshot(&invalidated_snapshot);
    assert_eq!(
        checkpoint
            .domain_events
            .last()
            .map(ExecutionDomainEvent::event_type),
        Some("finalization_invalidated")
    );
    assert_eq!(
        checkpoint.evidence.validations["old-validation"].status,
        ValidationEvidenceStatus::Superseded
    );
    assert_eq!(checkpoint.publication.status, PublicationStatus::NotStarted);
    assert_eq!(
        checkpoint
            .graph
            .as_ref()
            .unwrap()
            .node(&completion_node)
            .unwrap()
            .status,
        ExecutionNodeStatus::Pending,
        "stale completion must be reset before the dispatcher can choose publication",
    );
    assert!(
        validate_reconciled_finalization_route(
            &checkpoint,
            &revalidation,
            &reconciled_fingerprint,
        )
        .is_err()
    );

    if let Ok(containment) = command::HostedProcessContainment::new()
        && let Some((api_root, request, server)) = one_request_server("200 OK", json!({}))
    {
        let execution_id = Uuid::from_u128(9_226);
        let mut manifest = test_manifest(execution_id);
        manifest.github.base_sha.clone_from(&base_sha);
        manifest.github.branch = branch.into();
        manifest.execution_policy.quality_gates[0].command = "true".into();
        let mut notebook = new_worker_notebook(&manifest, reconciled_fingerprint.clone(), None);
        notebook.phase = ExecutionPhase::Validation;
        notebook.finalization_revalidation = Some(revalidation.clone());
        notebook.orchestration = checkpoint.clone();
        notebook.planned_changes = vec![PlannedChange {
            change_id: "worker-change".into(),
            parent_change_id: None,
            path: String::new(),
            targets: vec![PlannedTarget {
                path: "worker.txt".into(),
                role: "source change".into(),
                operation: Default::default(),
                new_file: true,
                status: IntendedChangeStatus::Applied,
            }],
            change: "Apply the worker change".into(),
            reason: "Exercise publication revalidation".into(),
            status: IntendedChangeStatus::Applied,
            acceptance_criteria: vec!["ac-1".into()],
            test_coverage: vec!["true".into()],
        }];
        notebook.intended_changes = intended_changes_from_plan(&notebook.planned_changes);
        for change in &mut notebook.intended_changes {
            change.status = IntendedChangeStatus::Applied;
            for target in &mut change.targets {
                target.status = IntendedChangeStatus::Applied;
            }
        }
        manifest.run.metadata["worker_notebook"] = serde_json::to_value(notebook).unwrap();
        let api = test_api_client(api_root, execution_id);
        let running = Arc::new(AtomicBool::new(true));
        let stop_reason = Arc::new(Mutex::new(None));
        let lease_renewed_at = Arc::new(Mutex::new(None));
        let mut agent = GatewayAgent::new(
            api,
            &manifest,
            &repo,
            &repo.hosted_local_config().unwrap(),
            &running,
            &stop_reason,
            &lease_renewed_at,
            &containment,
            None,
        )
        .unwrap();
        agent.phases = PhaseLedger::new(25, ExecutionPhase::Publication);
        let gate = agent
            .notebook
            .orchestration
            .graph
            .as_ref()
            .unwrap()
            .node(&validation_node)
            .unwrap()
            .validation
            .clone()
            .unwrap();
        let transition = agent
            .apply_execution_decision(ExecutionDecision::RunValidation {
                node_id: validation_node.clone(),
                gate,
            })
            .unwrap();
        assert_eq!(
            transition.phase_decision,
            PhaseDecision::Transition(ExecutionPhase::Validation)
        );
        assert_eq!(agent.phases.active(), ExecutionPhase::Validation);
        server.join().unwrap();
        assert!(request.recv().unwrap().contains("worker-events"));
    }

    let mut snapshot = checkpoint.snapshot(
        "remote-reconciliation-run",
        RepositorySnapshot {
            fingerprint: reconciled_fingerprint.clone(),
            source_tree_hash: reconciled_fingerprint.clone(),
            changed_paths: completion_changed_paths(&repo, &base_sha)
                .unwrap()
                .into_iter()
                .collect(),
            ..RepositorySnapshot::default()
        },
    );
    let new_evidence = "reconciled-validation".to_owned();
    let reconciled_command_fingerprint = snapshot
        .graph
        .node(&validation_node)
        .and_then(|node| node.validation.as_ref())
        .expect("reconciled validation node must retain its gate")
        .fingerprint(&reconciled_fingerprint);
    let reconciled_evidence = ValidationEvidenceRecord {
        evidence_id: new_evidence.clone(),
        node_id: validation_node.clone(),
        gate_id: "test".into(),
        fingerprint: reconciled_command_fingerprint.clone(),
        repository_fingerprint: reconciled_fingerprint.clone(),
        command: "true".into(),
        working_directory: work.path().to_string_lossy().into_owned(),
        status: ValidationEvidenceStatus::Passed,
        exit_code: Some(0),
        output_summary: String::new(),
        duration: Duration::from_millis(1),
    };
    snapshot
        .append_event(ExecutionDomainEvent::ValidationStarted {
            sequence: 7,
            node_id: validation_node.clone(),
            fingerprint: reconciled_command_fingerprint.clone(),
        })
        .unwrap();
    snapshot
        .append_event(ExecutionDomainEvent::ValidationEvidenceRecorded {
            sequence: 8,
            node_id: validation_node.clone(),
            evidence: reconciled_evidence,
        })
        .unwrap();
    snapshot
        .append_event(ExecutionDomainEvent::ValidationPassed {
            sequence: 9,
            node_id: validation_node.clone(),
            evidence_id: new_evidence.clone(),
            fingerprint: reconciled_command_fingerprint,
        })
        .unwrap();
    let resumed_after_validation = reconcile_execution(&snapshot).unwrap();
    assert!(matches!(
        resumed_after_validation,
        ExecutionDecision::ReviewDiff { .. }
    ));
    assert!(!execution_decision_requires_model_work(
        &resumed_after_validation
    ));
    assert!(execution_decision_has_completed_validation(
        &resumed_after_validation
    ));
    snapshot
        .append_event(ExecutionDomainEvent::DiffReviewed {
            sequence: 10,
            node_id: review_node,
            evidence_ids: vec![new_evidence],
        })
        .unwrap();
    snapshot
        .append_event(ExecutionDomainEvent::CompletionEvaluated {
            sequence: 11,
            node_id: completion_node,
            outcome: MissionOutcome::Complete,
        })
        .unwrap();
    snapshot
        .append_event(ExecutionDomainEvent::PublicationStarted {
            sequence: 12,
            node_id: publication_node,
            mode: PublicationMode::Normal,
        })
        .unwrap();
    checkpoint.replace_from_snapshot(&snapshot);

    validate_reconciled_finalization_route(&checkpoint, &revalidation, &reconciled_fingerprint)
        .unwrap();
    assert!(
        validate_reconciled_finalization_route(&checkpoint, &revalidation, &old_fingerprint,)
            .is_err(),
        "the stale pre-reconciliation fingerprint cannot authorize publication"
    );
    let route = checkpoint
        .domain_events
        .iter()
        .filter(|event| event.sequence() > revalidation.invalidated_after_sequence)
        .map(ExecutionDomainEvent::event_type)
        .collect::<Vec<_>>();
    assert_eq!(
        route,
        [
            "finalization_invalidated",
            "validation_started",
            "validation_evidence_recorded",
            "validation_passed",
            "diff_reviewed",
            "completion_evaluated",
            "publication_started",
        ]
    );
    assert_eq!(
        checkpoint.evidence.validations["reconciled-validation"].repository_fingerprint,
        reconciled_fingerprint
    );
}

#[test]
fn generic_hosted_golden_path_reaches_pull_request_and_preserves_canonical_success() {
    use crate::execution_graph::{
        ExecutionDomainEvent, ExecutionNodeStatus, MissionComplexity, MissionOutcome,
        PlannedTarget as GraphPlannedTarget, PublicationStatus,
        ValidationGateSpec as GraphValidationGateSpec,
        ValidationGateType as GraphValidationGateType,
    };
    use crate::hosted_simulation::{
        ScriptedMission, ScriptedValidationResult, SimulationHarness, SimulationPhase,
    };

    let target_path = "src/feature.rs";
    let acceptance_criterion = "ac-external-review";
    let mission = ScriptedMission::new("generic hosted golden path", MissionComplexity::Small)
        .with_target(GraphPlannedTarget {
            change_id: "implement-feature".into(),
            path: target_path.into(),
            role: "production implementation".into(),
            intent: "implement the requested repository behavior".into(),
            acceptance_criteria_ids: vec![acceptance_criterion.into()],
            operation: crate::execution_graph::TargetOperation::ModifyExisting,
            new_file: false,
        })
        .with_required_acceptance_criteria([acceptance_criterion])
        .with_validation_gate(GraphValidationGateSpec {
            gate_id: "focused".into(),
            gate_type: GraphValidationGateType::FocusedTest,
            command: "cargo test --test focused".into(),
            working_directory: ".".into(),
            required: true,
            dependency_lock_hash: "generic-lock".into(),
            relevant_environment_fingerprint: "generic-env".into(),
        })
        .with_validation_gate(GraphValidationGateSpec {
            gate_id: "required".into(),
            gate_type: GraphValidationGateType::TestSuite,
            command: "cargo test".into(),
            working_directory: ".".into(),
            required: true,
            dependency_lock_hash: "generic-lock".into(),
            relevant_environment_fingerprint: "generic-env".into(),
        })
        .with_validation_gate(GraphValidationGateSpec {
            gate_id: "build".into(),
            gate_type: GraphValidationGateType::Build,
            command: "cargo build".into(),
            working_directory: ".".into(),
            required: true,
            dependency_lock_hash: "generic-lock".into(),
            relevant_environment_fingerprint: "generic-env".into(),
        })
        .with_completion_outcome(MissionOutcome::PartialReviewable);

    let report = SimulationHarness::new(mission)
        .run()
        .expect("generic hosted golden path");

    assert_eq!(report.outcome, MissionOutcome::PartialReviewable);
    assert!(report.snapshot.graph.implementation_barrier_satisfied());
    assert!(
        report
            .snapshot
            .graph
            .nodes
            .iter()
            .filter(|node| node.required && node.kind.is_mutation())
            .all(|node| matches!(node.status, ExecutionNodeStatus::Completed))
    );
    for gate_id in ["focused", "required", "build"] {
        assert_eq!(report.validation_run_count(gate_id), 1);
        assert!(report.validation_runs.iter().any(|run| {
            run.gate_id == gate_id && run.result == ScriptedValidationResult::Passed
        }));
    }
    let phase_position = |phase| {
        report
            .phase_trace
            .iter()
            .position(|candidate| *candidate == phase)
            .expect("golden-path phase")
    };
    let phases = [
        SimulationPhase::Discovery,
        SimulationPhase::Planning,
        SimulationPhase::Implementation,
        SimulationPhase::Validation,
        SimulationPhase::DiffReview,
        SimulationPhase::CompletionEvaluation,
        SimulationPhase::Publication,
        SimulationPhase::Terminal,
    ];
    assert!(
        phases
            .windows(2)
            .all(|pair| phase_position(pair[0]) < phase_position(pair[1]))
    );
    assert!(report.has_only_legal_adjacent_transitions());
    assert_eq!(
        report.snapshot.publication.status,
        PublicationStatus::PullRequestCreated
    );
    assert!(report.snapshot.publication.commit_sha.is_some());
    assert!(report.snapshot.publication.branch.is_some());
    assert!(report.snapshot.publication.pull_request_url.is_some());
    for event_type in [
        "discovery_completed",
        "plan_accepted",
        "validation_started",
        "validation_passed",
        "diff_reviewed",
        "completion_evaluated",
        "commit_created",
        "branch_pushed",
        "pull_request_created",
        "run_finished",
    ] {
        assert!(
            report
                .snapshot
                .events
                .iter()
                .any(|event| event.event_type() == event_type),
            "golden path did not record {event_type}"
        );
    }
    assert_eq!(
        report
            .snapshot
            .events
            .iter()
            .filter(|event| matches!(event, ExecutionDomainEvent::ValidationStarted { .. }))
            .count(),
        3
    );

    let execution_id = Uuid::from_u128(0x600d_0000_0000_4000_8000_0000_0000_0001);
    let completion = CompletionEvaluation {
        status: CompletionStatus::Partial,
        implementation_completeness: ImplementationCompleteness::Complete,
        verification_readiness: VerificationReadiness::PendingManualReview,
        evaluation_source: EvaluationSource::OrchestratorFallback,
        confidence: 1.0,
        criteria: vec![],
        remaining_implementation_work: vec![],
        remaining_automated_verification: vec![],
        pending_external_review: vec![],
        optional_follow_up: vec![],
        review_checklist: vec![ReviewChecklistItem {
            r#type: VerificationType::ProductApproval,
            description: "A human reviewer approves the acceptance criterion.".into(),
            status: "pending".into(),
        }],
        unrecovered_tool_failures: vec![],
        summary: "Implementation and automated gates passed; external review remains.".into(),
    };
    let validation = [
        ("focused", "cargo test --test focused"),
        ("required", "cargo test"),
        ("build", "cargo build"),
    ]
    .into_iter()
    .map(|(id, command)| ValidationResult {
        id: id.into(),
        command: command.into(),
        status: "passed".into(),
        output: "passed".into(),
    })
    .collect::<Vec<_>>();
    let result = HostedResult {
        summary: "Implemented the generic ticket and published it for review.".into(),
        branch: report.snapshot.publication.branch.clone().unwrap(),
        commit: report.snapshot.publication.commit_sha.clone().unwrap(),
        pull_request: PullRequestResult {
            number: report.snapshot.publication.pull_request_number.unwrap(),
            url: report
                .snapshot
                .publication
                .pull_request_url
                .clone()
                .unwrap(),
        },
        validation,
        completeness: completion,
        terminal_telemetry: TerminalTelemetry {
            phase_reached: Some(ExecutionPhase::Publication),
            changed_paths: vec![target_path.into()],
            notebook_revision: report.snapshot.graph.revision,
            ..TerminalTelemetry::default()
        },
    };
    let canonical =
        resolve_published_terminal_result(execution_id, &result, "2026-08-11T08:00:00Z");
    assert_eq!(
        canonical.mission_outcome,
        CanonicalMissionOutcome::PartialReviewable
    );
    assert_eq!(canonical.process_health, ProcessHealth::Healthy);
    assert_eq!(
        canonical.execution_status,
        DomainExecutionStatus::NeedsContinuation
    );
    assert!(canonical.publication.is_published());
    assert_eq!(canonical.process_exit_code(), 0);
    assert!(
        canonical
            .completion
            .remaining_implementation_work
            .is_empty()
    );
    assert!(
        canonical
            .completion
            .remaining_automated_verification
            .is_empty()
    );
    assert!(canonical.completion.unrecovered_tool_failures.is_empty());
    assert_eq!(
        canonical.completion.review_checklist[0].r#type,
        VerificationType::ProductApproval
    );

    let Some((api_root, requests, server)) = request_sequence_server(vec![
        ("200 OK", json!({})),
        ("200 OK", json!({})),
        ("200 OK", json!({})),
        ("200 OK", json!({})),
        ("200 OK", json!({})),
    ]) else {
        return;
    };
    let api = test_api_client(api_root, execution_id);
    report_hosted_result(
        &api,
        execution_id,
        "2026-08-11T07:55:00Z",
        "2026-08-11T08:00:00Z",
        &result,
    )
    .expect("missing callback transport must preserve canonical success");
    server.join().unwrap();
    let delivered = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(delivered.len(), 5);
    let canonical_event: Value =
        serde_json::from_str(delivered[2].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        canonical_event["data"]["mission_outcome"],
        "partial_reviewable"
    );
    assert_eq!(canonical_event["data"]["process_health"], "healthy");
    assert_eq!(canonical_event["data"]["status"], "partial_result");
    assert!(canonical_event["data"]["pull_request_url"].is_string());
    assert!(delivered[3].contains("worker.terminal_callback_outbox_persisted"));
    assert!(delivered[4].contains("worker.terminal_callback_attempted"));

    let reconciled = reconcile_terminal_execution(
        Some(&canonical),
        false,
        CallbackStatus::FailedTransport,
        &InfrastructureTerminalMetadata {
            provider: "github_actions".into(),
            workflow_run_id: Some("golden-path-run".into()),
            workflow_job_id: Some("golden-path-job".into()),
            workflow_status: "completed".into(),
            workflow_conclusion: Some("success".into()),
            runner_name: Some("hosted-runner".into()),
            observed_at: "2026-08-11T08:00:01Z".into(),
        },
        true,
        true,
        false,
    );
    assert_eq!(
        reconciled.decision,
        InfrastructureReconciliationDecision::DomainResultPreserved
    );
    assert!(reconciled.domain_status_preserved);
    assert_eq!(reconciled.anomaly_code, Some("final_callback_missing"));
    assert_eq!(
        reconciled.terminal_result_id,
        Some(canonical.terminal_result_id)
    );
    assert_eq!(canonical.process_health, ProcessHealth::Healthy);
}

#[test]
fn aops_226_producing_fixture_reviews_commits_pushes_creates_pr_and_exits_zero() {
    let fixture_started = Instant::now();
    let work = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    command::checked(
        "git",
        ["init", "--bare", "-q", remote.path().to_str().unwrap()],
        work.path(),
    )
    .unwrap();
    command::checked("git", ["init", "-q"], work.path()).unwrap();
    command::checked("git", ["config", "user.name", "Test"], work.path()).unwrap();
    command::checked(
        "git",
        ["config", "user.email", "test@example.com"],
        work.path(),
    )
    .unwrap();
    let paths = [
        "src/components/theme/ThemeProvider.tsx",
        "src/components/theme/ThemeToggle.tsx",
        "src/styles/globals.css",
        "tests/theme-provider.test.tsx",
        "tests/theme-tokens.test.ts",
    ];
    for path in paths {
        let target = work.path().join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, format!("// base {path}\n")).unwrap();
    }
    command::checked("git", ["add", "."], work.path()).unwrap();
    command::checked(
        "git",
        [
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
        work.path(),
    )
    .unwrap();
    command::checked("git", ["branch", "-M", "main"], work.path()).unwrap();
    command::checked(
        "git",
        ["remote", "add", "origin", remote.path().to_str().unwrap()],
        work.path(),
    )
    .unwrap();
    command::checked("git", ["push", "-q", "-u", "origin", "main"], work.path()).unwrap();
    let base_sha = command::checked("git", ["rev-parse", "HEAD"], work.path()).unwrap();
    let branch = "rustgrid/aops-226-fixture";
    command::checked("git", ["checkout", "-q", "-b", branch], work.path()).unwrap();
    let mut performance = PhaseLedger::new(60, ExecutionPhase::Discovery);
    for _ in 0..4 {
        performance.begin_model_call().unwrap();
    }
    performance.transition(ExecutionPhase::Planning);
    for _ in 0..2 {
        performance.begin_model_call().unwrap();
    }
    assert_eq!(performance.apply_ticket_complexity(paths.len()), 25);
    performance.transition(ExecutionPhase::Implementation);
    let mut first_successful_write_call = None;
    let implemented_files = [
        (
            paths[0],
            r#"export type Theme = "dark" | "light" | "light-blue" | "red";
export const THEME_STORAGE_KEY = "rustgrid-theme";
export const THEME_ORDER: Theme[] = ["dark", "light", "light-blue", "red"];
export const isLightTheme = (theme: Theme) => theme === "light" || theme === "light-blue";

export function restoreTheme(saved: string | null): Theme {
  return THEME_ORDER.includes(saved as Theme) ? (saved as Theme) : "dark";
}

export function applyTheme(root: HTMLElement, theme: Theme) {
  root.classList.remove(...THEME_ORDER.map((value) => `theme-${value}`));
  root.classList.add(`theme-${theme}`);
  root.style.colorScheme = isLightTheme(theme) ? "light" : "dark";
  localStorage.setItem(THEME_STORAGE_KEY, theme);
}
"#,
        ),
        (
            paths[1],
            r#"import { THEME_ORDER, type Theme } from "./ThemeProvider";

export function nextTheme(theme: Theme): Theme {
  return THEME_ORDER[(THEME_ORDER.indexOf(theme) + 1) % THEME_ORDER.length];
}

export function themeToggleLabel(theme: Theme) {
  return `Switch to ${nextTheme(theme)} theme`;
}

export const themeToggleIcon = (theme: Theme) =>
  theme === "light" || theme === "light-blue" ? "sun" : "moon";
"#,
        ),
        (
            paths[2],
            r#".theme-light-blue {
  color-scheme: light;
  --background: 210 60% 98%;
  --foreground: 218 42% 16%;
  --card: 210 50% 100%;
  --card-foreground: 218 42% 16%;
  --border: 210 32% 78%;
  --input: 210 32% 82%;
  --ring: 216 92% 48%;
  --primary: 224 76% 42%;
  --primary-foreground: 210 50% 100%;
  --info: 199 88% 48%;
  --destructive: 0 72% 45%;
  --success: 151 58% 34%;
}
"#,
        ),
        (
            paths[3],
            r#"import { applyTheme, restoreTheme, THEME_ORDER } from "../src/components/theme/ThemeProvider";

it("cycles dark -> light -> light-blue -> red -> dark", () => {
  expect(THEME_ORDER).toEqual(["dark", "light", "light-blue", "red"]);
});

it("restores and immediately applies a saved light-blue preference", () => {
  const root = document.documentElement;
  applyTheme(root, restoreTheme("light-blue"));
  expect(root.classList.contains("theme-light-blue")).toBe(true);
  expect(root.classList.contains("theme-dark")).toBe(false);
  expect(localStorage.getItem("rustgrid-theme")).toBe("light-blue");
});

it("preserves the dark fallback for missing and invalid preferences", () => {
  expect(restoreTheme(null)).toBe("dark");
  expect(restoreTheme("invalid")).toBe("dark");
});
"#,
        ),
        (
            paths[4],
            r#"const requiredTokens = [
  "background", "foreground", "card", "card-foreground", "border", "input",
  "ring", "primary", "primary-foreground", "info", "destructive", "success",
];

it("defines every semantic token for light-blue without conflating primary and info", () => {
  const lightBlue = readThemeTokens("theme-light-blue");
  expect(requiredTokens.every((token) => lightBlue[token])).toBe(true);
  expect(lightBlue.primary).not.toBe(lightBlue.info);
  expect(contrast(lightBlue.foreground, lightBlue.background)).toBeGreaterThanOrEqual(4.5);
});
"#,
        ),
    ];
    let mut target_sequence = paths
        .iter()
        .enumerate()
        .map(|(index, path)| ImplementationTarget {
            change_id: format!("aops-226-target-{}", index + 1),
            path: (*path).into(),
            role: "required AOPS-226 target".into(),
            new_file: false,
            intent: "Implement light-blue theme behavior.".into(),
            acceptance_criteria: vec!["ac-1".into()],
            status: IntendedChangeStatus::Planned,
        })
        .collect::<Vec<_>>();
    for (index, (path, content)) in implemented_files.into_iter().enumerate() {
        let active = target_sequence
            .iter()
            .find(|target| target.status == IntendedChangeStatus::Planned)
            .unwrap();
        assert_eq!(active.path, path);
        performance.begin_model_call().unwrap();
        fs::write(work.path().join(path), content).unwrap();
        target_sequence[index].status = IntendedChangeStatus::Applied;
        first_successful_write_call
            .get_or_insert(performance.phase_calls(ExecutionPhase::Implementation));
    }
    let repo = Repo {
        root: work.path().to_path_buf(),
    };
    let expected_paths = paths
        .iter()
        .map(|path| (*path).to_owned())
        .collect::<Vec<_>>();
    let changed_paths = completion_changed_paths(&repo, &base_sha).unwrap();
    assert_eq!(changed_paths, expected_paths);
    let reviewed_diff = completion_review_diff(work.path(), &changed_paths, &base_sha).unwrap();
    performance.transition(ExecutionPhase::Validation);
    performance.transition(ExecutionPhase::DiffReview);
    performance.begin_model_call().unwrap();
    for path in paths {
        assert!(reviewed_diff.contains(path));
    }
    assert!(reviewed_diff.contains("dark\", \"light\", \"light-blue\", \"red"));
    assert!(reviewed_diff.contains("theme === \"light-blue\""));
    assert!(reviewed_diff.contains("--primary:"));
    assert!(reviewed_diff.contains("--info:"));
    assert!(reviewed_diff.contains("restoreTheme(\"light-blue\")"));
    assert!(reviewed_diff.contains("toBeGreaterThanOrEqual(4.5)"));

    let criterion = "Light-blue theme behavior is implemented and covered.".to_owned();
    let planned_changes = paths
        .iter()
        .enumerate()
        .map(|(index, path)| PlannedChange {
            change_id: format!("aops-226-target-{}", index + 1),
            parent_change_id: None,
            path: String::new(),
            targets: vec![PlannedTarget {
                path: (*path).into(),
                role: "required AOPS-226 target".into(),
                operation: Default::default(),
                new_file: false,
                status: IntendedChangeStatus::Applied,
            }],
            change: format!("Implement light-blue behavior in {path}."),
            reason: "Complete the localized theme change.".into(),
            status: IntendedChangeStatus::Applied,
            acceptance_criteria: vec![criterion.clone()],
            test_coverage: vec!["focused theme tests".into()],
        })
        .collect::<Vec<_>>();
    let declaration = deterministic_complete_declaration(
        &planned_changes,
        std::slice::from_ref(&criterion),
        &changed_paths,
        &[],
        &[],
    )
    .unwrap();
    let implementation = ImplementationOutcome {
        summary: "Implemented all five light-blue theme targets.".into(),
        budget_exhausted: false,
        explicit_declaration: Some(declaration),
    };
    let validation = [
        ("focused-theme", "npm test -- theme"),
        ("test", "npm test"),
        ("build", "npm run build"),
    ]
    .into_iter()
    .map(|(id, command)| ValidationResult {
        id: id.into(),
        command: command.into(),
        status: "passed".into(),
        output: "passed once".into(),
    })
    .collect::<Vec<_>>();
    let plan = ImplementationPlan {
        implementation_status: "ready".into(),
        planned_changes,
        planned_new_files: vec![],
        planned_test_changes: vec![],
        remaining_unknowns: vec![],
        blocking_unknowns: vec![],
    };
    let completeness = completion_fallback(
        &implementation,
        None,
        Some(&plan),
        &[],
        &changed_paths,
        std::slice::from_ref(&criterion),
        &validation,
        ProjectVerificationPolicy::default(),
    );
    assert!(matches!(
        completeness.status,
        CompletionStatus::Complete | CompletionStatus::CompletePendingExternalReview
    ));
    performance.transition(ExecutionPhase::CompletionEvaluation);
    performance.begin_model_call().unwrap();
    performance.transition(ExecutionPhase::Publication);
    let estimated_cost_micros = 360_000;
    assert!(first_successful_write_call.is_some_and(|call| call <= 6));
    assert!(performance.implementation_repair_calls() <= 13);
    assert!(performance.total_calls() <= 25);
    assert!(estimated_cost_micros <= model_cost_limit_for_target_count(paths.len()));
    assert!(fixture_started.elapsed() <= MAX_HOSTED_EXECUTION_DURATION);

    let commit = repo
        .commit_paths(&changed_paths, "AOPS-226: Add light-blue theme")
        .unwrap();
    assert!(
        repo.push(branch, &commit, "fixture-token", "https://github.com")
            .unwrap()
    );
    let remote_commit = command::checked(
        "git",
        [
            "--git-dir",
            remote.path().to_str().unwrap(),
            "rev-parse",
            &format!("refs/heads/{branch}"),
        ],
        work.path(),
    )
    .unwrap();
    assert_eq!(remote_commit, commit);
    assert!(repo.new_agent_paths(&BTreeSet::new()).unwrap().is_empty());
    let (publication_head, publication_paths) = committed_head_for_publication(&repo, &base_sha)
        .unwrap()
        .expect("the committed base-to-HEAD implementation must remain publishable");
    assert_eq!(publication_head, commit);
    assert_eq!(publication_paths, expected_paths);

    let Some((github_base, requests, server)) = request_sequence_server(vec![
        ("200 OK", json!([])),
        (
            "201 Created",
            json!({
                "number": 226,
                "html_url": "https://github.example/RustGrid/example/pull/226",
                "node_id": "PR_AOPS_226",
                "draft": false
            }),
        ),
    ]) else {
        return;
    };
    let github = GitHubClient::new("fixture-token", github_base.as_str()).unwrap();
    let mut manifest = test_manifest(Uuid::from_u128(226));
    manifest.ticket_key = "AOPS-226".into();
    manifest.ticket_title = "Add the light-blue theme".into();
    manifest.github.branch = branch.into();
    manifest.github.base_ref = "main".into();
    let pull = find_or_create_hosted_pull_request(
        &github,
        &RepoConfig {
            owner: "RustGrid".into(),
            name: "example".into(),
        },
        &manifest,
        &validation,
        &completeness,
        false,
        false,
    )
    .unwrap();
    assert_eq!(pull.number, 226);
    let lookup = requests.recv().unwrap();
    let creation = requests.recv().unwrap();
    server.join().unwrap();
    assert!(lookup.starts_with("GET /api/v3/repos/RustGrid/example/pulls?"));
    assert!(creation.starts_with("POST /api/v3/repos/RustGrid/example/pulls HTTP/1.1"));
    let creation_body: Value =
        serde_json::from_str(creation.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(creation_body["head"], branch);
    assert_eq!(creation_body["draft"], false);

    let result = HostedResult {
        summary: implementation.summary,
        branch: branch.into(),
        commit: commit.clone(),
        pull_request: PullRequestResult {
            number: pull.number,
            url: pull.html_url,
        },
        validation,
        completeness,
        terminal_telemetry: TerminalTelemetry {
            phase_persistence_failure_code: None,
            model_calls_used: performance.total_calls(),
            input_tokens: 48_000,
            output_tokens: 8_000,
            estimated_cost_micros,
            usage: ToolUsage {
                reads: 5,
                searches: 2,
                writes: 5,
                successful_writes: 5,
                validation_commands: 3,
                required_validations: 3,
                ..ToolUsage::default()
            },
            changed_paths: changed_paths.clone(),
            last_successful_action: json!({
                "phase": "publication",
                "action": "pull_request_created",
                "pull_request_number": 226,
            }),
            phase_reached: Some(ExecutionPhase::Publication),
            plan: plan.planned_changes.clone(),
            remaining_work: Vec::new(),
            validation_evidence: Vec::new(),
            notebook_revision: 18,
            discovery_calls: performance.phase_calls(ExecutionPhase::Discovery),
            planning_calls: performance.phase_calls(ExecutionPhase::Planning),
            initial_target_mutation_calls: 5,
            target_mutation_repair_calls: 0,
            validation_diagnosis_calls: 0,
            validation_repair_mutation_calls: 0,
            diff_review_calls: performance.phase_calls(ExecutionPhase::DiffReview),
            completion_evaluation_calls: performance
                .phase_calls(ExecutionPhase::CompletionEvaluation),
        },
    };
    assert_eq!(
        resolve_published_terminal_result(
            manifest.execution.execution_id,
            &result,
            "2026-08-01T10:05:00Z"
        )
        .process_exit_code(),
        0
    );

    let execution_id = manifest.execution.execution_id;
    let Some((api_root, result_requests, result_server)) = request_sequence_server(vec![
        ("200 OK", json!({})),
        ("200 OK", json!({})),
        ("200 OK", json!({})),
        ("200 OK", json!({})),
        ("200 OK", json!({})),
        ("200 OK", json!({})),
        ("200 OK", json!({})),
        ("200 OK", json!({})),
    ]) else {
        return;
    };
    let api = test_api_client(api_root, execution_id);
    report_hosted_result(
        &api,
        execution_id,
        "2026-08-01T10:00:00Z",
        "2026-08-01T10:05:00Z",
        &result,
    )
    .unwrap();

    let telemetry_request = result_requests.recv().unwrap();
    let resolved_event_request = result_requests.recv().unwrap();
    let result_event_request = result_requests.recv().unwrap();
    let outbox_request = result_requests.recv().unwrap();
    let attempted_request = result_requests.recv().unwrap();
    let completion_request = result_requests.recv().unwrap();
    let acknowledged_request = result_requests.recv().unwrap();
    let exit_event_request = result_requests.recv().unwrap();
    result_server.join().unwrap();

    assert!(telemetry_request.starts_with(&format!(
        "POST /api/v1/executions/{execution_id}/telemetry/batch HTTP/1.1"
    )));
    let telemetry_body: Value =
        serde_json::from_str(telemetry_request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(telemetry_body["events"][0]["type"], "execution.completed");
    assert_eq!(
        telemetry_body["events"][0]["execution"]["status"],
        "succeeded"
    );

    let resolved_event_body: Value =
        serde_json::from_str(resolved_event_request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        resolved_event_body["data"]["event_type"],
        "worker.canonical_terminal_result_resolved"
    );

    assert!(result_event_request.starts_with(&format!(
        "POST /api/v1/executions/{execution_id}/worker-events HTTP/1.1"
    )));
    let result_event_body: Value =
        serde_json::from_str(result_event_request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(result_event_body["event_type"], "result");
    assert_eq!(
        result_event_body["data"]["event_type"],
        "worker.canonical_terminal_result_persisted"
    );
    assert_eq!(result_event_body["data"]["status"], "completed");
    assert_eq!(result_event_body["data"]["mission_outcome"], "complete");
    assert_eq!(
        result_event_body["data"]["canonical_terminal_result_id"],
        resolved_event_body["data"]["canonical_terminal_result_id"]
    );
    assert_eq!(result_event_body["data"]["head_sha"], commit);
    assert_eq!(result_event_body["data"]["pull_request_number"], 226);
    assert_eq!(
        result_event_body["data"]["terminal_telemetry"]["model_calls_used"],
        performance.total_calls()
    );
    assert_eq!(
        result_event_body["data"]["terminal_telemetry"]["usage"]["successful_writes"],
        5
    );
    assert_eq!(
        result_event_body["data"]["terminal_telemetry"]["usage"]["validation_commands"],
        3
    );
    assert_eq!(
        result_event_body["data"]["terminal_telemetry"]["changed_paths"],
        json!(changed_paths)
    );

    assert!(outbox_request.contains("worker.terminal_callback_outbox_persisted"));
    assert!(attempted_request.contains("worker.terminal_callback_attempted"));

    assert!(completion_request.starts_with(&format!(
        "POST /api/v1/executions/{execution_id}/complete HTTP/1.1"
    )));
    let completion_body: Value =
        serde_json::from_str(completion_request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(completion_body["status"], "completed");
    assert_eq!(completion_body["mission_outcome"], "complete");
    assert_eq!(
        completion_body["canonical_terminal_result_id"],
        resolved_event_body["data"]["canonical_terminal_result_id"]
    );
    assert_eq!(
        completion_body["canonical_terminal_result"]["terminal_result_id"],
        resolved_event_body["data"]["canonical_terminal_result_id"]
    );
    assert_eq!(completion_body["process_health"], "healthy");
    assert_eq!(
        completion_body["completion_evaluation"]["status"],
        "complete"
    );
    assert_eq!(
        completion_body["final_callback"]["canonical_terminal_result_id"],
        resolved_event_body["data"]["canonical_terminal_result_id"]
    );
    assert_eq!(completion_body["final_callback"]["terminal_revision"], 1);
    assert_eq!(completion_body["final_callback"]["process_exit_code"], 0);
    assert_eq!(
        completion_body["final_callback"]["final_notebook_revision"],
        18
    );
    assert_eq!(completion_body["head_branch"], branch);
    assert_eq!(completion_body["head_sha"], commit);
    assert_eq!(completion_body["pull_request_number"], 226);
    assert_eq!(
        completion_body["pull_request_url"],
        "https://github.example/RustGrid/example/pull/226"
    );
    assert_eq!(
        completion_body["output_summary"],
        "Implemented all five light-blue theme targets."
    );
    assert!(acknowledged_request.contains("worker.terminal_callback_acknowledged"));
    assert!(exit_event_request.contains("worker.process_exit_code_resolved"));
}

#[test]
fn successful_run_remains_successful_when_terminal_callback_transport_fails() {
    let execution_id = Uuid::from_u128(0x2260_0000_0000_4000_8000_0000_0000_0020);
    let Some((api_root, requests, server)) = request_sequence_server(vec![
        ("200 OK", json!({})),
        ("200 OK", json!({})),
        ("200 OK", json!({})),
        ("200 OK", json!({})),
        ("200 OK", json!({})),
    ]) else {
        return;
    };
    let api = test_api_client(api_root, execution_id);
    let result = HostedResult {
        summary: "Canonical RunFinished already records success.".into(),
        branch: "rustgrid/callback-recovery".into(),
        commit: "a".repeat(40),
        pull_request: PullRequestResult {
            number: 226,
            url: "https://github.example/RustGrid/example/pull/226".into(),
        },
        validation: vec![ValidationResult {
            id: "tests".into(),
            command: "cargo test".into(),
            status: "passed".into(),
            output: String::new(),
        }],
        completeness: test_completion_evaluation(CompletionStatus::Complete),
        terminal_telemetry: TerminalTelemetry::default(),
    };

    report_hosted_result(
        &api,
        execution_id,
        "2026-08-01T10:00:00Z",
        "2026-08-01T10:05:00Z",
        &result,
    )
    .expect("callback delivery cannot reverse canonical success");

    server.join().unwrap();
    let delivered = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(delivered.len(), 5);
    assert!(delivered[0].contains("/telemetry/batch"));
    assert!(delivered[1].contains("/worker-events"));
    assert!(delivered[2].contains("worker.canonical_terminal_result_persisted"));
    assert!(delivered[3].contains("worker.terminal_callback_outbox_persisted"));
    assert!(delivered[4].contains("worker.terminal_callback_attempted"));
}

#[test]
fn accepted_callback_timeout_retries_with_the_same_idempotency_identity() {
    let execution_id = Uuid::from_u128(0x2260_0000_0000_4000_8000_0000_0000_0023);
    let Some((api_root, requests, server)) = request_outcome_sequence_server(vec![
        Some(("200 OK", json!({}))),
        Some(("200 OK", json!({}))),
        None,
        Some(("200 OK", json!({}))),
        Some(("200 OK", json!({}))),
        Some(("200 OK", json!({}))),
        Some(("200 OK", json!({}))),
    ]) else {
        return;
    };
    let clock = Arc::new(ManualHostedClock::new(SystemTime::now()));
    let api = test_api_client_with_clock(api_root, execution_id, clock.clone());
    let result = HostedResult {
        summary: "Canonical result persisted before callback delivery.".into(),
        branch: "rustgrid/callback-timeout".into(),
        commit: "a".repeat(40),
        pull_request: PullRequestResult {
            number: 23,
            url: "https://github.example/RustGrid/example/pull/23".into(),
        },
        validation: vec![ValidationResult {
            id: "tests".into(),
            command: "cargo test".into(),
            status: "passed".into(),
            output: String::new(),
        }],
        completeness: test_completion_evaluation(CompletionStatus::Complete),
        terminal_telemetry: TerminalTelemetry {
            notebook_revision: 11,
            ..TerminalTelemetry::default()
        },
    };
    let terminal = resolve_published_terminal_result(execution_id, &result, "2026-08-04T00:00:00Z");
    let completion = CompletionRequest {
        status: terminal.completion_request_status().into(),
        canonical_terminal_result_id: Some(terminal.terminal_result_id),
        terminal_revision: Some(terminal.finality.terminal_revision),
        terminal_authority: Some("worker_domain".into()),
        canonical_terminal_result: Some(serde_json::to_value(&terminal).unwrap()),
        mission_outcome: Some(terminal.compatibility_completion_status()),
        process_health: Some(terminal.process_health.as_str().into()),
        completion_evaluation: Some(terminal.completion.clone()),
        output_summary: Some(result.summary.clone()),
        failure_code: None,
        failure_message: None,
        head_branch: terminal.publication.branch.clone(),
        head_sha: terminal.publication.commit_sha.clone(),
        pull_request_number: Some(23),
        pull_request_url: terminal.publication.pull_request_url.clone(),
        final_callback: None,
    };

    assert_eq!(
        deliver_terminal_callback(
            &api,
            &terminal,
            &completion,
            result.terminal_telemetry.notebook_revision,
            "2026-08-04T00:00:00Z",
        )
        .unwrap(),
        TerminalCallbackDelivery::Acknowledged { attempts: 2 }
    );
    server.join().unwrap();
    let delivered = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(delivered.len(), 7);
    let callback_requests = delivered
        .iter()
        .filter(|request| request.contains(&format!("/executions/{execution_id}/complete")))
        .collect::<Vec<_>>();
    assert_eq!(callback_requests.len(), 2);
    let idempotency_key = |request: &str| {
        request
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("idempotency-key:"))
            .unwrap()
            .to_ascii_lowercase()
    };
    assert_eq!(
        idempotency_key(callback_requests[0]),
        idempotency_key(callback_requests[1])
    );
    assert!(delivered[3].contains("worker.terminal_callback_retry_scheduled"));
    assert!(delivered[6].contains("worker.terminal_callback_acknowledged"));
}

#[test]
fn persisted_callback_outbox_is_replayed_after_worker_restart() {
    let execution_id = Uuid::from_u128(0x2260_0000_0000_4000_8000_0000_0000_0024);
    let Some((api_root, requests, server)) =
        request_sequence_server(vec![("200 OK", json!({})), ("200 OK", json!({}))])
    else {
        return;
    };
    let api = test_api_client(api_root, execution_id);
    let result = HostedResult {
        summary: "Canonical result and callback envelope survived restart.".into(),
        branch: "rustgrid/callback-restart".into(),
        commit: "a".repeat(40),
        pull_request: PullRequestResult {
            number: 24,
            url: "https://github.example/RustGrid/example/pull/24".into(),
        },
        validation: vec![ValidationResult {
            id: "tests".into(),
            command: "cargo test".into(),
            status: "passed".into(),
            output: String::new(),
        }],
        completeness: test_completion_evaluation(CompletionStatus::Complete),
        terminal_telemetry: TerminalTelemetry::default(),
    };
    let canonical =
        resolve_published_terminal_result(execution_id, &result, "2026-08-04T10:00:00Z");
    let callback_key = terminal_callback_idempotency_key(
        execution_id,
        canonical.terminal_result_id,
        canonical.finality.terminal_revision,
    );
    let callback = FinalExecutionCallback {
        execution_id,
        canonical_terminal_result_id: canonical.terminal_result_id,
        terminal_revision: canonical.finality.terminal_revision,
        final_notebook_revision: 12,
        process_exit_code: 0,
        workflow_run_id: "88".into(),
        sent_at: "2026-08-04T10:00:01Z".into(),
        idempotency_key: callback_key.to_string(),
    };
    let completion = CompletionRequest {
        status: canonical.completion_request_status().into(),
        canonical_terminal_result_id: Some(canonical.terminal_result_id),
        terminal_revision: Some(canonical.finality.terminal_revision),
        terminal_authority: Some("worker_domain".into()),
        canonical_terminal_result: Some(serde_json::to_value(&canonical).unwrap()),
        mission_outcome: Some(canonical.compatibility_completion_status()),
        process_health: Some("healthy".into()),
        completion_evaluation: Some(canonical.completion.clone()),
        output_summary: Some(result.summary),
        failure_code: None,
        failure_message: None,
        head_branch: canonical.publication.branch.clone(),
        head_sha: canonical.publication.commit_sha.clone(),
        pull_request_number: Some(24),
        pull_request_url: canonical.publication.pull_request_url.clone(),
        final_callback: Some(callback.clone()),
    };
    let mut manifest = test_manifest(execution_id);
    manifest.execution.canonical_terminal_result_id = Some(canonical.terminal_result_id);
    manifest.execution.terminal_revision =
        Some(i64::try_from(canonical.finality.terminal_revision).expect("test revision fits i64"));
    manifest.execution.terminal_authority = Some("worker_domain".into());
    manifest.execution.canonical_terminal_result = Some(serde_json::to_value(&canonical).unwrap());
    let github = manifest.execution.github_actions.as_mut().unwrap();
    github.callback_status = Some("pending".into());
    github.callback_outbox = Some(json!({
        "outbox": {
            "execution_id": execution_id,
            "canonical_terminal_result_id": canonical.terminal_result_id,
            "payload_hash": "a".repeat(64),
            "attempts": 1,
            "created_at": "2026-08-04T10:00:00Z"
        },
        "callback": callback,
        "completion": completion,
    }));

    assert!(recover_persisted_terminal_callback(&api, &manifest).unwrap());
    server.join().unwrap();
    let delivered = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(delivered.len(), 2);
    assert!(delivered[0].contains("worker.terminal_callback_attempted"));
    assert!(
        delivered[1]
            .to_ascii_lowercase()
            .contains(&format!("idempotency-key: {callback_key}"))
    );
    assert!(delivered[1].contains(&canonical.terminal_result_id.to_string()));
}

#[test]
fn emergency_failure_is_ignored_when_claim_finds_terminal_execution() {
    let execution_id = Uuid::from_u128(0x2260_0000_0000_4000_8000_0000_0000_0021);
    let Some((api_root, requests, server)) = request_sequence_server(vec![(
        "409 Conflict",
        json!({"code": "execution_terminal_state"}),
    )]) else {
        return;
    };
    let api = test_api_client(api_root, execution_id);

    report_emergency_failure_with_api(&api, execution_id)
        .expect("terminal claim conflict is a harmless no-op");

    server.join().unwrap();
    let delivered = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].contains(&format!("/executions/{execution_id}/claim")));
    assert!(!delivered[0].contains("/complete"));
    assert!(!delivered[0].contains("/worker-events"));
}

#[test]
fn emergency_failure_does_not_post_when_claim_is_unconfirmed() {
    let execution_id = Uuid::from_u128(0x2260_0000_0000_4000_8000_0000_0000_0022);
    let Some((api_root, requests, server)) = request_sequence_server(vec![
        (
            "503 Service Unavailable",
            json!({"code": "gateway_unavailable"}),
        ),
        (
            "503 Service Unavailable",
            json!({"code": "gateway_unavailable"}),
        ),
    ]) else {
        return;
    };
    let api = test_api_client(api_root, execution_id);

    let error = report_emergency_failure_with_api(&api, execution_id).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("could not confirm ownership before reporting")
    );

    server.join().unwrap();
    let delivered = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(delivered.len(), 2);
    assert!(
        delivered
            .iter()
            .all(|request| request.contains(&format!("/executions/{execution_id}/claim")))
    );
    assert!(
        delivered
            .iter()
            .all(|request| !request.contains("/complete") && !request.contains("/worker-events"))
    );
}

#[test]
fn zero_diff_preparation_block_is_a_healthy_domain_result_with_live_telemetry() {
    let mut failure = test_execution_failure(
        "implementation_progress_missing",
        "Implementation could not begin after guided recovery.",
    );
    let current_plan = vec![test_planned_change()];
    let remaining_work = derive_remaining_work(&intended_changes_from_plan(&current_plan));
    let failed_read = ToolProgressRecord {
        execution_attempt: 18,
        model_call: 7,
        phase: ExecutionPhase::Implementation,
        tool: "read_file".into(),
        target: Some("src/components/theme/ThemeProvider.tsx".into()),
        class: ToolProgressClass::RecoverableFailure,
        outcome_signature: "read_file:range_invalid:ThemeProvider.tsx".into(),
        detail: "requested line range exceeded the file length; valid range is 1-120".into(),
        repository_progress: false,
    };
    let mut stale_validation = new_running_evidence(
        "focused-old-tree".into(),
        "focused-theme".into(),
        ValidationGateType::FocusedTest,
        "npm test -- theme".into(),
        "old-fingerprint".into(),
        "old-tree".into(),
        "lock".into(),
        ValidationSource::WorkerRequired,
    );
    stale_validation.status = ValidationStatus::Superseded;

    failure.phase = ExecutionPhase::Implementation;
    failure.model_calls_used = 8;
    failure.model_calls_limit = 20;
    failure.model_calls_remaining = 12;
    failure.phase_calls_used = 8;
    failure.phase_calls_limit = 8;
    failure.input_tokens = 12_345;
    failure.output_tokens = 2_345;
    failure.estimated_cost_micros = 456_789;
    failure.usage = ToolUsage {
        reads: 6,
        failed_reads: 3,
        searches: 2,
        writes: 2,
        failed_writes: 2,
        write_preflight_rejections: 1,
        write_execution_failures: 1,
        ..ToolUsage::default()
    };
    failure.last_successful_action = json!({
        "model_call": 6,
        "phase": "implementation",
        "tool": "search_text",
        "target": "src/components/theme/ThemeProvider.tsx",
    });
    failure.failed_tool_operations = vec![failed_read];
    failure.current_plan = current_plan.clone();
    failure.validation_evidence = vec![stale_validation];
    failure.notebook_revision = 18;
    let failure = classify_implementation_preparation_failure(failure, &remaining_work);

    assert_eq!(failure.status, "blocked");
    assert_eq!(failure.category, "implementation_blocked");
    assert_eq!(failure.process_health, "healthy");
    assert_eq!(failure.mission_outcome, "blocked");
    assert_eq!(
        failure.blocker.as_deref(),
        Some("implementation_preparation_failed")
    );
    assert!(failure.resumable);
    assert_eq!(failure.code, "implementation_preparation_failed");
    assert_eq!(failure.phase, ExecutionPhase::Implementation);
    assert_eq!(
        failure.message,
        "Implementation could not begin after guided recovery."
    );
    assert_eq!(
        failure.underlying_error.message,
        "implementation_progress_missing"
    );
    assert!(failure.recoverable);
    assert_eq!(failure.resume_phase, "implementation");
    assert_eq!(failure.remaining_work, remaining_work);
    assert!(
        failure
            .recommended_action
            .contains("current planned target")
    );

    let error = anyhow::Error::new(failure);
    let (code, _) = safe_failure(&error, false);
    let diagnostics = failure_diagnostics(&error, false);
    let failure = error
        .downcast_ref::<HostedAgentExecutionFailure>()
        .expect("the classified failure must remain structured");
    let event = blocked_result_event_payload(failure, diagnostics.clone());
    let completion = blocked_completion_evaluation(failure);

    assert_eq!(code, "implementation_preparation_failed");
    assert_ne!(code, "hosted_agent_execution_failed");
    assert_eq!(diagnostics["process_health"], "healthy");
    assert_eq!(diagnostics["mission_outcome"], "blocked");
    assert_eq!(diagnostics["resume_phase"], "implementation");
    assert_eq!(
        diagnostics["recommended_action"],
        failure.recommended_action
    );
    assert_eq!(diagnostics["model_calls_used"], 8);
    assert_eq!(diagnostics["model_calls_limit"], 20);
    assert_eq!(diagnostics["model_calls_remaining"], 12);
    assert_eq!(diagnostics["phase_calls_used"], 8);
    assert_eq!(diagnostics["phase_calls_limit"], 8);
    assert_eq!(diagnostics["input_tokens"], 12_345);
    assert_eq!(diagnostics["output_tokens"], 2_345);
    assert_eq!(diagnostics["estimated_cost_micros"], 456_789);
    assert_eq!(diagnostics["usage"]["reads"], 6);
    assert_eq!(diagnostics["usage"]["failed_reads"], 3);
    assert_eq!(diagnostics["usage"]["searches"], 2);
    assert_eq!(diagnostics["usage"]["writes"], 2);
    assert_eq!(diagnostics["usage"]["failed_writes"], 2);
    assert_eq!(diagnostics["usage"]["write_preflight_rejections"], 1);
    assert_eq!(diagnostics["usage"]["write_execution_failures"], 1);
    assert_eq!(diagnostics["usage"]["validation_commands"], 0);
    assert_eq!(diagnostics["last_successful_action"]["tool"], "search_text");
    assert_eq!(diagnostics["phase"], "implementation");
    assert_eq!(diagnostics["current_plan"], json!(current_plan));
    assert_eq!(diagnostics["remaining_work"], json!(remaining_work));
    assert_eq!(
        diagnostics["failed_tool_operations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        diagnostics["validation_evidence"].as_array().unwrap().len(),
        1
    );
    assert_eq!(diagnostics["notebook_revision"], 18);
    assert_eq!(diagnostics["changed_paths"], json!([]));
    assert_eq!(
        diagnostics["failed_tool_operations"][0]["target"],
        "src/components/theme/ThemeProvider.tsx"
    );
    assert!(
        diagnostics["failed_tool_operations"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("valid range is 1-120")
    );

    assert_eq!(event["status"], "blocked");
    assert_eq!(event["mission_outcome"], "blocked");
    assert_eq!(event["process_health"], "healthy");
    assert_eq!(event["reason_code"], "implementation_preparation_failed");
    assert_eq!(event["resumable"], true);
    assert_eq!(event["resume_phase"], "implementation");
    assert_eq!(event["changed_paths"], json!([]));
    assert_eq!(event["remaining_work"], json!(remaining_work));
    assert_eq!(completion.status, CompletionStatus::Blocked);
    assert_eq!(
        completion.verification_readiness,
        VerificationReadiness::Blocked
    );
    assert_eq!(
        completion.evaluation_source,
        EvaluationSource::OrchestratorFallback
    );
    assert_eq!(completion.remaining_implementation_work.len(), 1);
    assert!(completion.remaining_implementation_work[0].contains("tests/theme-provider.test.tsx"));
    assert_eq!(event["terminal_telemetry"]["model_calls_used"], 8);
    assert_eq!(event["terminal_telemetry"]["input_tokens"], 12_345);
    assert_eq!(event["terminal_telemetry"]["output_tokens"], 2_345);
    assert_eq!(
        event["terminal_telemetry"]["estimated_cost_micros"],
        456_789
    );
    assert_eq!(event["terminal_telemetry"]["usage"]["reads"], 6);
    assert_eq!(event["terminal_telemetry"]["usage"]["failed_reads"], 3);
    assert_eq!(event["terminal_telemetry"]["usage"]["searches"], 2);
    assert_eq!(event["terminal_telemetry"]["usage"]["writes"], 2);
    assert_eq!(event["terminal_telemetry"]["usage"]["failed_writes"], 2);
    assert_eq!(
        event["terminal_telemetry"]["usage"]["write_preflight_rejections"],
        1
    );
    assert_eq!(
        event["terminal_telemetry"]["usage"]["write_execution_failures"],
        1
    );
    assert_eq!(
        event["terminal_telemetry"]["usage"]["validation_commands"],
        0
    );
    assert_eq!(event["terminal_telemetry"]["changed_paths"], json!([]));
    assert_eq!(
        event["terminal_telemetry"]["last_successful_action"]["tool"],
        "search_text"
    );
    assert_eq!(
        event["terminal_telemetry"]["phase_reached"],
        "implementation"
    );
    assert_eq!(event["terminal_telemetry"]["plan"], json!(current_plan));
    assert_eq!(
        event["terminal_telemetry"]["remaining_work"],
        json!(remaining_work)
    );
    assert_eq!(
        event["terminal_telemetry"]["validation_evidence"][0]["evidence_id"],
        "focused-old-tree"
    );
    assert_eq!(event["terminal_telemetry"]["notebook_revision"], 18);
    assert_eq!(
        event["failure"]["failed_tool_operations"][0]["class"],
        "recoverable_failure"
    );
    assert_eq!(event["failure"], diagnostics);
}

#[test]
fn zero_diff_validation_entry_executes_zero_repository_gates() {
    let decision =
        validation_entry_decision(ImplementationCompletionStatus::Preparing, 0, false, false);
    let mut executed_gate_count = 0;
    let dispatched = dispatch_validation_gates(decision, || {
        executed_gate_count += 1;
        Ok("gate ran")
    })
    .unwrap();

    assert_eq!(
        decision,
        ValidationEntryDecision::ForbiddenNoImplementationChanges
    );
    assert!(dispatched.is_none());
    assert_eq!(executed_gate_count, 0);
}

#[test]
fn partial_and_blocked_domain_outcomes_are_healthy_even_with_incomplete_gates() {
    for status in [CompletionStatus::Partial, CompletionStatus::Blocked] {
        let result = HostedResult {
            summary: "Published resumable work.".into(),
            branch: "rustgrid/resumable".into(),
            commit: "a".repeat(40),
            pull_request: PullRequestResult {
                number: 143,
                url: "https://github.com/RustGrid/example/pull/143".into(),
            },
            validation: vec![ValidationResult {
                id: "test".into(),
                command: "npm test".into(),
                status: "failed".into(),
                output: "one remaining failure".into(),
            }],
            completeness: test_completion_evaluation(status),
            terminal_telemetry: TerminalTelemetry::default(),
        };
        let terminal =
            resolve_published_terminal_result(Uuid::nil(), &result, "2026-08-03T12:00:00Z");
        assert_eq!(
            terminal.mission_outcome,
            CanonicalMissionOutcome::PartialReviewable
        );
        assert_eq!(terminal.process_health, ProcessHealth::Healthy);
        assert_eq!(terminal.process_exit_code(), 0);
    }
    let mut complete = HostedResult {
        summary: "Complete.".into(),
        branch: "rustgrid/complete".into(),
        commit: "b".repeat(40),
        pull_request: PullRequestResult {
            number: 144,
            url: "https://github.com/RustGrid/example/pull/144".into(),
        },
        validation: vec![ValidationResult {
            id: "build".into(),
            command: "npm run build".into(),
            status: "failed".into(),
            output: String::new(),
        }],
        completeness: test_completion_evaluation(CompletionStatus::Complete),
        terminal_telemetry: TerminalTelemetry::default(),
    };
    assert_eq!(
        resolve_published_terminal_result(Uuid::nil(), &complete, "2026-08-03T12:00:00Z")
            .mission_outcome,
        CanonicalMissionOutcome::PartialReviewable
    );
    complete.completeness.status = CompletionStatus::Uncertain;
    assert_eq!(
        resolve_published_terminal_result(Uuid::nil(), &complete, "2026-08-03T12:00:00Z")
            .process_exit_code(),
        0
    );
}

#[test]
fn model_budget_handoff_preserves_work_without_claiming_completion() {
    let empty = Vec::new();
    assert!(model_budget_handoff_summary(true, &empty).is_none());

    let changed = vec!["src/theme.css".to_owned()];
    assert!(model_budget_handoff_summary(false, &changed).is_none());
    assert!(
        model_budget_handoff_summary(true, &changed)
            .is_some_and(|summary| summary.contains("passing gates alone cannot mark it complete"))
    );
    let implementation = ImplementationOutcome {
        summary: "partial edit".into(),
        budget_exhausted: true,
        explicit_declaration: None,
    };
    let result = completion_fallback(
        &implementation,
        None,
        None,
        &[],
        &changed,
        &["Theme can be selected".into()],
        &[],
        ProjectVerificationPolicy::default(),
    );
    assert_ne!(result.status, CompletionStatus::Complete);
    let hosted_result = HostedResult {
        summary: "partial edit".into(),
        branch: "rustgrid/partial".into(),
        commit: "a".repeat(40),
        pull_request: PullRequestResult {
            number: 1,
            url: "https://github.com/RustGrid/example/pull/1".into(),
        },
        validation: vec![ValidationResult {
            id: "test".into(),
            command: "cargo test".into(),
            status: "passed".into(),
            output: String::new(),
        }],
        completeness: result,
        terminal_telemetry: TerminalTelemetry::default(),
    };
    let terminal =
        resolve_published_terminal_result(Uuid::nil(), &hosted_result, "2026-08-03T12:00:00Z");
    assert_eq!(
        terminal.mission_outcome,
        CanonicalMissionOutcome::PartialReviewable
    );
    assert_eq!(terminal.process_exit_code(), 0);
}

#[test]
fn forty_call_budget_prioritizes_implementation_and_keeps_finalization_usable() {
    let normal = phase_budget_allocation(DEFAULT_HOSTED_MODEL_CALLS);
    assert_eq!(normal.discovery_maximum, 5);
    assert_eq!(normal.planning_maximum, 3);
    assert_eq!(normal.implementation_repair_reserved, 26);
    assert_eq!(normal.diff_review_reserved, 3);
    assert_eq!(normal.completion_evaluation_reserved, 3);
    assert_eq!(normal.total(), 40);
}

#[test]
fn canonical_forty_call_budget_reaches_the_worker_unchanged() {
    let execution_id = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
    let mut manifest = test_manifest(execution_id);
    manifest.manifest_version = 4;
    manifest.model_call_budget = Some(40);
    manifest.requested_model_call_budget = Some(40);
    manifest.resolved_model_call_budget = Some(40);
    manifest.budget_source = Some(BudgetSource::UserSelected);
    manifest.clamped = Some(false);
    manifest.clamp_reason = Some(None);
    manifest.execution.maximum_model_calls = Some(40);
    manifest.ai_gateway.maximum_model_calls = 40;

    let budget = manifest.budget_audit().unwrap();
    assert_eq!(budget.requested_model_call_budget, 40);
    assert_eq!(budget.resolved_model_call_budget, 40);
    assert_eq!(budget.worker_received_model_call_budget, 40);
    assert_eq!(budget.contract, "canonical");
    let environment = test_environment(execution_id);
    let api = test_api_client(Url::parse("http://127.0.0.1:8080/").unwrap(), execution_id);
    manifest.validate(execution_id, &environment, &api).unwrap();
}

#[test]
fn repository_wide_signed_budget_can_reach_one_hundred_calls() {
    let execution_id = Uuid::from_u128(0x22222222_2222_4222_8222_222222222222);
    let mut manifest = test_manifest(execution_id);
    manifest.manifest_version = 4;
    manifest.model_call_budget = Some(100);
    manifest.requested_model_call_budget = Some(100);
    manifest.resolved_model_call_budget = Some(100);
    manifest.budget_source = Some(BudgetSource::UserSelected);
    manifest.clamped = Some(false);
    manifest.clamp_reason = Some(None);
    manifest.execution.maximum_model_calls = Some(100);
    manifest.ai_gateway.maximum_model_calls = 100;

    let environment = test_environment(execution_id);
    let api = test_api_client(Url::parse("http://127.0.0.1:8080/").unwrap(), execution_id);
    manifest.validate(execution_id, &environment, &api).unwrap();

    manifest.model_call_budget = Some(101);
    manifest.requested_model_call_budget = Some(101);
    manifest.resolved_model_call_budget = Some(101);
    manifest.execution.maximum_model_calls = Some(101);
    manifest.ai_gateway.maximum_model_calls = 101;
    assert!(manifest.validate(execution_id, &environment, &api).is_err());
}

#[test]
fn budget_mismatch_is_typed_before_model_execution() {
    let execution_id = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
    let mut manifest = test_manifest(execution_id);
    manifest.manifest_version = 4;
    manifest.model_call_budget = Some(40);
    manifest.requested_model_call_budget = Some(40);
    manifest.resolved_model_call_budget = Some(40);
    manifest.budget_source = Some(BudgetSource::UserSelected);
    manifest.clamped = Some(false);
    manifest.clamp_reason = Some(None);
    manifest.execution.maximum_model_calls = Some(20);
    manifest.ai_gateway.maximum_model_calls = 20;

    let error = manifest.budget_audit().unwrap_err();
    assert!(error.downcast_ref::<ExecutionBudgetMismatch>().is_some());
    let (code, message) = safe_failure(&error, false);
    assert_eq!(code, "execution_budget_mismatch");
    assert!(message.contains("worker-received"));
    let diagnostics = failure_diagnostics(&error, false);
    assert_eq!(diagnostics["requested_model_call_budget"], 40);
    assert_eq!(diagnostics["resolved_model_call_budget"], 40);
    assert_eq!(diagnostics["worker_received_model_call_budget"], 20);
    assert_eq!(diagnostics["model_calls_used"], 0);
}

#[test]
fn canonical_budget_distinguishes_a_null_clamp_reason_from_a_missing_field() {
    #[derive(Deserialize)]
    struct ClampReasonPresence {
        #[serde(default, deserialize_with = "deserialize_present_nullable")]
        clamp_reason: Option<Option<String>>,
    }

    let missing: ClampReasonPresence = serde_json::from_value(json!({})).unwrap();
    let explicitly_null: ClampReasonPresence =
        serde_json::from_value(json!({"clamp_reason": null})).unwrap();
    assert_eq!(missing.clamp_reason, None);
    assert_eq!(explicitly_null.clamp_reason, Some(None));
}

#[test]
fn explicit_legacy_twenty_call_budget_remains_supported() {
    let execution_id = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
    let mut manifest = test_manifest(execution_id);
    manifest.execution.maximum_model_calls = Some(20);
    manifest.ai_gateway.maximum_model_calls = 20;

    let budget = manifest.budget_audit().unwrap();
    assert_eq!(budget.worker_received_model_call_budget, 20);
    assert_eq!(budget.contract, "legacy_signed_manifest");
    let allocation = phase_budget_allocation(20);
    assert_eq!(
        (
            allocation.discovery_maximum,
            allocation.planning_maximum,
            allocation.implementation_repair_reserved,
            allocation.diff_review_reserved,
            allocation.completion_evaluation_reserved,
        ),
        (5, 3, 10, 1, 1)
    );
}

#[test]
fn hosted_context_keeps_only_recent_turns_after_notebook_checkpointing() {
    let initial = json!({"role": "user", "content": "mission"});
    let mut turns = (0..12)
        .map(|index| vec![json!({"role": "assistant", "content": format!("turn-{index}")})])
        .collect::<VecDeque<_>>();
    compact_hosted_turns(&mut turns);
    assert_eq!(turns.len(), MAX_HOSTED_TURN_WINDOWS);
    assert_eq!(turns[0][0]["content"], "turn-9");

    let mut input = vec![initial.clone()];
    input.extend(turns.iter().flatten().cloned());
    let mut request = json!({
        "model": "gpt-5.6-sol",
        "input": input
    });
    fit_request_to_input_ceiling(&mut request, &initial, &mut turns, 100_000).unwrap();
    assert_eq!(turns.len(), MAX_HOSTED_TURN_WINDOWS);

    let reduced_ceiling = serde_json::to_vec(&request).unwrap().len() - 1;
    fit_request_to_input_ceiling(&mut request, &initial, &mut turns, reduced_ceiling).unwrap();
    assert!(turns.len() < MAX_HOSTED_TURN_WINDOWS);
    assert_eq!(request["input"].as_array().unwrap().first(), Some(&initial));
}

#[test]
fn implementation_context_is_phase_specific_and_below_eight_thousand_tokens() {
    let mut notebook = test_discovery_notebook(ExecutionPhase::Implementation);
    notebook.architecture_findings = vec!["historical-payload-marker ".repeat(20_000)];
    notebook.planned_changes = vec![test_planned_change()];
    notebook.intended_changes = intended_changes_from_plan(&notebook.planned_changes);
    notebook.remaining_work_v2 = derive_remaining_work(&notebook.intended_changes);
    let compact = compact_notebook_for_phase(&notebook, ExecutionPhase::Implementation);
    assert!(compact.len() <= 28 * 1024);
    assert!(!compact.contains("historical-payload-marker"));
    assert!(!compact.contains("occurred_at"));
}

#[test]
fn localized_discovery_has_a_twelve_thousand_token_equivalent_request_ceiling() {
    assert_eq!(
        phase_request_input_ceiling(ExecutionPhase::Discovery, 100 * 1024),
        MAX_DISCOVERY_REQUEST_BYTES
    );
    assert_eq!(
        phase_request_input_ceiling(ExecutionPhase::ArtifactRepair, 100 * 1024),
        MAX_DISCOVERY_REQUEST_BYTES
    );
    assert_eq!(
        phase_request_input_ceiling(ExecutionPhase::Implementation, 100 * 1024),
        100 * 1024
    );
    let guidance = visual_impact_guidance("Add a light-blue theme");
    assert!(guidance.contains("at most three representative consumers"));
    assert!(guidance.contains("Record the compact impact map"));
}

#[test]
fn centralized_localized_discovery_stops_after_core_boundaries_and_three_consumers() {
    let mut notebook = test_discovery_notebook(ExecutionPhase::Discovery);
    notebook.goal = "Add a light-blue theme".into();
    notebook.architecture_findings.clear();
    record_centralized_discovery_finding(
        &mut notebook,
        "Centralized semantic CSS variables are confirmed.",
    );
    notebook.files_inspected = vec![
        "src/components/theme/ThemeProvider.tsx".into(),
        "src/components/theme/ThemeToggle.tsx".into(),
        "src/styles/globals.css".into(),
        "tests/theme-provider.test.tsx".into(),
        "package.json".into(),
        "src/components/Button.tsx".into(),
        "src/components/Input.tsx".into(),
        "src/components/Status.tsx".into(),
    ];
    let coverage = localized_discovery_coverage(&notebook);
    assert!(coverage.centralized_abstraction);
    assert_eq!(coverage.representative_consumers, 3);
    assert!(localized_discovery_should_stop(coverage));
    assert!(
        validate_localized_discovery_scope(&notebook, &["src/components/Card.tsx"])
            .unwrap_err()
            .to_string()
            .contains("localized_discovery_complete")
    );

    notebook.files_inspected = vec!["src/components/theme/ThemeProvider.tsx".into()];
    notebook.discovery_paths_sampled = vec![
        "src/components/Button.tsx".into(),
        "src/components/Input.tsx".into(),
        "src/components/Status.tsx".into(),
    ];
    assert!(
        validate_localized_discovery_scope(&notebook, &["src/components/Card.tsx"])
            .unwrap_err()
            .to_string()
            .contains("localized_discovery_consumer_limit")
    );
    assert!(validate_localized_discovery_scope(&notebook, &["src/components/Status.tsx"]).is_ok());
    let directory_search = json!({"path": "src"});
    assert!(
        discovery_requested_paths("search_text", directory_search.as_object().unwrap()).is_empty()
    );
    let directory_listing = json!({"path": "."});
    assert!(
        discovery_requested_paths("list_files", directory_listing.as_object().unwrap()).is_empty()
    );
    assert!(
        validate_localized_discovery_scope(&notebook, &["tests/theme-provider.test.tsx"]).is_ok()
    );

    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("src/components")).unwrap();
    for name in ["Button.tsx", "Card.tsx", "Input.tsx", "Status.tsx"] {
        fs::write(
            directory.path().join("src/components").join(name),
            "const color = 'hardcoded';\n",
        )
        .unwrap();
    }
    let known = BTreeSet::from([
        "src/components/Button.tsx".to_owned(),
        "src/components/Input.tsx".to_owned(),
    ]);
    let search = search_repo(
        directory.path(),
        "src/components",
        "hardcoded",
        &["tsx".into()],
        0,
        Some(1),
        &known,
    )
    .unwrap();
    assert!(
        search
            .matched_paths
            .iter()
            .filter(|path| !known.contains(*path))
            .count()
            <= 1
    );
}

#[test]
fn lifecycle_transition_table_rejects_skipping_validation() {
    assert!(legal_phase_transition(
        ExecutionPhase::Implementation,
        ExecutionPhase::Validation
    ));
    assert!(!legal_phase_transition(
        ExecutionPhase::Implementation,
        ExecutionPhase::DiffReview
    ));
    assert!(!legal_phase_transition(
        ExecutionPhase::Validation,
        ExecutionPhase::Publication
    ));
    assert!(legal_phase_transition(
        ExecutionPhase::Repair,
        ExecutionPhase::Implementation
    ));
    assert!(legal_phase_transition(
        ExecutionPhase::Repair,
        ExecutionPhase::Validation
    ));
    assert!(legal_phase_transition(
        ExecutionPhase::Repair,
        ExecutionPhase::DiffReview
    ));
    assert!(!legal_phase_transition(
        ExecutionPhase::Repair,
        ExecutionPhase::CompletionEvaluation
    ));
    assert!(!legal_phase_transition(
        ExecutionPhase::Validation,
        ExecutionPhase::CompletionEvaluation
    ));
    assert!(legal_phase_transition(
        ExecutionPhase::Validation,
        ExecutionPhase::DiffReview
    ));
    assert!(legal_phase_transition(
        ExecutionPhase::Publication,
        ExecutionPhase::Validation
    ));
    assert!(
        ExecutionPhase::Publication
            .stage()
            .can_transition_to(ExecutionPhase::Validation.stage())
    );
}

#[test]
fn phase_transition_preflight_accepts_repair_to_incomplete_diff_review_contract() {
    let payload = json!({
        "event_type": "worker.phase_transition",
        "transition_payload_version": 1,
        "from_phase": "repair",
        "phase": "diff_review",
        "decision": "review_incomplete_diff",
        "reason_code": "phase_reconciled",
        "source": "orchestrator",
        "source_tree_hash": "tree-2",
        "occurred_at": "2026-08-05T00:00:00Z",
        "graph_revision": 12,
        "notebook_revision": 18,
    });
    let preflight = preflight_phase_transition(
        &payload,
        ExecutionPhase::Repair,
        ExecutionPhase::DiffReview,
        12,
        18,
    );
    assert!(preflight.passed());

    let mut invalid = payload;
    invalid.as_object_mut().unwrap().remove("decision");
    let rejected = preflight_phase_transition(
        &invalid,
        ExecutionPhase::Repair,
        ExecutionPhase::DiffReview,
        12,
        18,
    );
    assert!(!rejected.passed());
    assert!(!rejected.required_fields_present);
}

#[test]
fn validation_repair_attempt_binds_the_active_semantic_model_call() {
    let mut event = crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
        sequence: 1,
        validation_node_id: crate::execution_graph::ExecutionNodeId::new("validation-focused"),
        failure_id: crate::execution_graph::FailureId::new("failure-1"),
        result: crate::execution_graph::RepairResult::NoMutation {
            diagnosis: None,
            reason: "admission rejected".into(),
            outcome: crate::execution_graph::ValidationRepairMutationOutcome::AdmissionRejected,
            unresolved: None,
        },
        attempt: Some(crate::execution_graph::ValidationRepairAttempt::default()),
    };
    bind_validation_repair_model_call(&mut event, Some("semantic-model-call-7"));
    assert!(matches!(
        event,
        crate::execution_graph::ExecutionDomainEvent::ValidationRepairCompleted {
            attempt: Some(crate::execution_graph::ValidationRepairAttempt {
                model_call_id: Some(ref model_call_id),
                ..
            }),
            ..
        } if model_call_id == "semantic-model-call-7"
    ));
}

#[test]
fn validation_rerun_telemetry_is_session_revision_and_budget_bound() {
    let session = crate::execution_graph::ValidationRepairSession {
        session_id: "repair-session-1".into(),
        originating_gate_id: crate::execution_graph::ExecutionNodeId::new("validation-focused"),
        current_assertion_set_revision: 3,
        ..crate::execution_graph::ValidationRepairSession::default()
    };
    let evidence = crate::execution_graph::ValidationEvidenceRecord {
        evidence_id: "validation-evidence-2".into(),
        repository_fingerprint: "tree-2".into(),
        ..crate::execution_graph::ValidationEvidenceRecord::default()
    };
    let event = validation_rerun_completed_event(
        &session,
        &session.originating_gate_id,
        &evidence,
        "failed",
        2,
        1,
        4,
    );
    assert_eq!(event["event_type"], "worker.validation_rerun_completed");
    assert_eq!(event["repair_session_id"], "repair-session-1");
    assert_eq!(event["failure_revision"], 3);
    assert_eq!(event["repository_fingerprint"], "tree-2");
    assert_eq!(event["command_runs"], 2);
    assert_eq!(event["model_calls_consumed"], 0);
    assert_eq!(event["local_model_calls_remaining"], 1);
    assert_eq!(event["mission_model_calls_remaining"], 4);
}

#[test]
fn production_preflight_classifies_duplicate_without_change_id_and_advances() {
    use crate::execution_graph::{
        ExecutionNodeStatus, ExecutionSnapshot, MissionBudget, MissionComplexity, MutationResult,
        PlannedTarget as GraphTarget, RepositorySnapshot, build_execution_graph,
    };

    let paths = [
        "src/components/theme/ThemeProvider.tsx",
        "src/components/theme/ThemeToggle.tsx",
    ];
    let targets = paths
        .iter()
        .enumerate()
        .map(|(index, path)| GraphTarget {
            change_id: format!("theme-{}", index + 1),
            path: (*path).into(),
            role: "required attempt-20 target".into(),
            operation: Default::default(),
            new_file: false,
            intent: "implement light-blue theme".into(),
            acceptance_criteria_ids: vec!["ac-1".into()],
        })
        .collect::<Vec<_>>();
    let budget = MissionBudget::for_complexity(MissionComplexity::Tiny);
    let mut graph = build_execution_graph(
        "production-duplicate-preflight",
        MissionComplexity::Tiny,
        "tree-after-first-target",
        &targets,
        &[],
        &budget,
    );
    let mutation_nodes = graph
        .nodes
        .iter()
        .filter(|node| node.kind.is_mutation())
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    graph
        .set_node_status(&mutation_nodes[0], ExecutionNodeStatus::Applied)
        .unwrap();
    let snapshot = ExecutionSnapshot {
        run_id: "production-duplicate-preflight".into(),
        current_repository: RepositorySnapshot {
            fingerprint: "tree-after-first-target".into(),
            changed_paths: BTreeSet::from([paths[0].to_owned()]),
            ..RepositorySnapshot::default()
        },
        graph,
        ..ExecutionSnapshot::default()
    };

    // The production adapter receives only the attempted path here. It
    // must derive identity from the graph, never from model `change_id`.
    let duplicate =
        classify_hosted_mutation_preflight(&snapshot, Some(&mutation_nodes[1]), paths[0], false)
            .unwrap()
            .expect("the first canonical node is already applied");
    assert_eq!(duplicate.code, "target_already_applied");
    assert_eq!(duplicate.change_id, "theme-1");
    assert_eq!(duplicate.target, paths[0]);
    assert_eq!(duplicate.repair_strategy, "continue_next_target");
    assert!(duplicate.message.contains(paths[1]));
    assert!(
        classify_hosted_mutation_preflight(&snapshot, Some(&mutation_nodes[0]), paths[0], true,)
            .unwrap()
            .is_none(),
        "implementation AlreadyApplied must not satisfy a validation repair intent"
    );
    assert!(matches!(
        classify_mutation_request(&snapshot, &mutation_nodes[0]).unwrap(),
        Some(MutationResult::AlreadyApplied { .. })
    ));
    assert!(!snapshot.failures.has_unresolved());

    match reconcile_execution(&snapshot).unwrap() {
        ExecutionDecision::ExecuteTarget {
            node_id,
            action,
            target,
        } => {
            assert_eq!(node_id, mutation_nodes[1]);
            assert_eq!(target.target.path, paths[1]);
            assert!(matches!(
                action,
                crate::hosted_orchestrator::MutationAction::PrepareTargetContext { .. }
            ));
            assert!(target.allowed_tools.is_empty());
        }
        decision => panic!(
            "already-applied production preflight must advance to target two, got {decision:?}"
        ),
    }
}

#[test]
fn repair_with_repository_superseded_failure_returns_to_implementation() {
    let mut intended = intended_changes_from_plan(&[test_planned_change()]);
    intended[0].targets[0].status = IntendedChangeStatus::InProgress;
    intended[0].status = IntendedChangeStatus::InProgress;
    let target = intended[0].targets[0].path.clone();
    let mut failures = vec![test_write_failure("theme-tests", &target, "failed-hash")];
    let changed = BTreeSet::from([target.clone()]);
    assert_eq!(
        supersede_failures_satisfied_by_repository_state(&mut failures, &intended, &[], &changed,),
        1
    );
    assert!(failures[0].recovered);
    assert_eq!(
        failures[0].reconciliation,
        FailureReconciliation::Superseded
    );
}

#[test]
fn validated_useful_partial_is_publishable_as_a_draft() {
    let mut planned = test_planned_change();
    planned.targets.push(PlannedTarget {
        path: "tests/theme-palette.test.ts".into(),
        role: "remaining palette coverage".into(),
        operation: Default::default(),
        new_file: false,
        status: IntendedChangeStatus::Planned,
    });
    let changed_paths = vec![planned.targets[0].path.clone()];
    let remaining = vec![RemainingWorkItem {
        change_id: planned.change_id.clone(),
        path: planned.targets[1].path.clone(),
        role: planned.targets[1].role.clone(),
        status: IntendedChangeStatus::Planned,
        reason: "planned target has not been applied".into(),
    }];
    let declaration =
        deterministic_partial_declaration(&[planned.clone()], &changed_paths, &remaining).unwrap();
    let implementation = ImplementationOutcome {
        summary: "Preserved a validated useful partial implementation.".into(),
        budget_exhausted: true,
        explicit_declaration: Some(declaration),
    };
    let plan = ImplementationPlan {
        implementation_status: "ready".into(),
        planned_changes: vec![planned],
        planned_new_files: vec![],
        planned_test_changes: vec![],
        remaining_unknowns: vec![],
        blocking_unknowns: vec![],
    };
    let completion = completion_fallback(
        &implementation,
        None,
        Some(&plan),
        &[],
        &changed_paths,
        &[],
        &[test_passed_validation("npm test && npm run build")],
        ProjectVerificationPolicy::default(),
    );
    assert_eq!(completion.status, CompletionStatus::Partial);
    assert!(requires_implementation_continuation(completion.status));
    let manifest = test_manifest(Uuid::from_u128(226));
    assert!(hosted_pull_request_title(&manifest, true).starts_with("[INCOMPLETE]"));
    let Some((github_base, requests, server)) = request_sequence_server(vec![
        ("200 OK", json!([])),
        (
            "201 Created",
            json!({
                "number": 227,
                "html_url": "https://github.example/RustGrid/example/pull/227",
                "node_id": "PR_AOPS_226_PARTIAL",
                "draft": true
            }),
        ),
    ]) else {
        return;
    };
    let github = GitHubClient::new("fixture-token", github_base.as_str()).unwrap();
    let pull = find_or_create_hosted_pull_request(
        &github,
        &RepoConfig {
            owner: "RustGrid".into(),
            name: "example".into(),
        },
        &manifest,
        &[test_passed_validation("npm test && npm run build")],
        &completion,
        true,
        true,
    )
    .unwrap();
    assert_eq!(pull.number, 227);
    let _lookup = requests.recv().unwrap();
    let creation = requests.recv().unwrap();
    server.join().unwrap();
    let creation_body: Value =
        serde_json::from_str(creation.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(creation_body["draft"], true);
}

#[test]
fn existing_non_draft_pull_request_is_confirmed_draft_before_recovery_returns() {
    let branch = "rustgrid/recovery-existing-draft";
    let pull_response = |draft| {
        json!({
            "number": 301,
            "html_url": "https://github.example/RustGrid/example/pull/301",
            "node_id": "PR_RECOVERY_EXISTING_301",
            "draft": draft
        })
    };
    let Some((github_base, requests, server)) = request_sequence_server(vec![
        ("200 OK", json!([pull_response(false)])),
        ("200 OK", pull_response(false)),
        (
            "200 OK",
            json!({
                "data": {
                    "convertPullRequestToDraft": {
                        "pullRequest": {"id": "PR_RECOVERY_EXISTING_301"}
                    }
                }
            }),
        ),
        ("200 OK", json!([pull_response(true)])),
    ]) else {
        return;
    };
    let github = GitHubClient::new("fixture-token", github_base.as_str()).unwrap();
    let mut manifest = test_manifest(Uuid::from_u128(301));
    manifest.github.branch = branch.into();
    let pull = find_or_create_hosted_pull_request(
        &github,
        &RepoConfig {
            owner: "RustGrid".into(),
            name: "example".into(),
        },
        &manifest,
        &[test_passed_validation("cargo test")],
        &test_completion_evaluation(CompletionStatus::Partial),
        true,
        true,
    )
    .unwrap();

    assert_eq!(pull.number, 301);
    assert!(pull.draft);
    let lookup = requests.recv().unwrap();
    let update = requests.recv().unwrap();
    let conversion = requests.recv().unwrap();
    let confirmation = requests.recv().unwrap();
    server.join().unwrap();
    assert!(lookup.starts_with("GET /api/v3/repos/RustGrid/example/pulls?"));
    assert!(update.starts_with("PATCH /api/v3/repos/RustGrid/example/pulls/301 "));
    assert!(conversion.starts_with("POST /api/graphql "));
    assert!(conversion.contains("convertPullRequestToDraft"));
    assert!(confirmation.starts_with("GET /api/v3/repos/RustGrid/example/pulls?"));
}

#[test]
fn ambiguous_create_fallback_confirms_recovered_pull_request_is_draft() {
    let branch = "rustgrid/recovery-ambiguous-draft";
    let pull_response = |draft| {
        json!({
            "number": 302,
            "html_url": "https://github.example/RustGrid/example/pull/302",
            "node_id": "PR_RECOVERY_AMBIGUOUS_302",
            "draft": draft
        })
    };
    let Some((github_base, requests, server)) = request_sequence_server(vec![
        ("200 OK", json!([])),
        (
            "422 Unprocessable Entity",
            json!({"message": "A pull request already exists"}),
        ),
        ("200 OK", json!([pull_response(false)])),
        (
            "200 OK",
            json!({
                "data": {
                    "convertPullRequestToDraft": {
                        "pullRequest": {"id": "PR_RECOVERY_AMBIGUOUS_302"}
                    }
                }
            }),
        ),
        ("200 OK", json!([pull_response(true)])),
    ]) else {
        return;
    };
    let github = GitHubClient::new("fixture-token", github_base.as_str()).unwrap();
    let mut manifest = test_manifest(Uuid::from_u128(302));
    manifest.github.branch = branch.into();
    let pull = find_or_create_hosted_pull_request(
        &github,
        &RepoConfig {
            owner: "RustGrid".into(),
            name: "example".into(),
        },
        &manifest,
        &[test_passed_validation("cargo test")],
        &test_completion_evaluation(CompletionStatus::Partial),
        true,
        true,
    )
    .unwrap();

    assert_eq!(pull.number, 302);
    assert!(pull.draft);
    let initial_lookup = requests.recv().unwrap();
    let creation = requests.recv().unwrap();
    let fallback_lookup = requests.recv().unwrap();
    let conversion = requests.recv().unwrap();
    let confirmation = requests.recv().unwrap();
    server.join().unwrap();
    assert!(initial_lookup.starts_with("GET /api/v3/repos/RustGrid/example/pulls?"));
    assert!(creation.starts_with("POST /api/v3/repos/RustGrid/example/pulls "));
    let creation_body: Value =
        serde_json::from_str(creation.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(creation_body["draft"], true);
    assert!(fallback_lookup.starts_with("GET /api/v3/repos/RustGrid/example/pulls?"));
    assert!(conversion.starts_with("POST /api/graphql "));
    assert!(conversion.contains("convertPullRequestToDraft"));
    assert!(confirmation.starts_with("GET /api/v3/repos/RustGrid/example/pulls?"));
}

#[test]
fn hosted_budget_thresholds_guide_completion_before_the_signed_limit() {
    assert!(hosted_budget_advisory(27, 40).is_none());
    assert_eq!(
        hosted_budget_advisory(28, 40).map(|advisory| advisory.0),
        Some(70)
    );
    let finalization = hosted_budget_advisory(36, 40).unwrap();
    assert_eq!(finalization.0, 90);
    assert!(
        finalization
            .2
            .contains("smallest complete validated result")
    );
}

#[test]
fn five_target_cost_ceiling_is_enforced_before_provider_dispatch() {
    assert_eq!(model_cost_limit_for_target_count(5), 5_000_000);
    let guard = CostGuard {
        estimated_cost_micros: 4_750_000,
        hard_limit_micros: model_cost_limit_for_target_count(5),
        ..CostGuard::default()
    };
    let mut request = json!({
        "input": "implementation context ".repeat(1_000),
        "max_output_tokens": 16_384,
    });
    assert!(constrain_request_to_cost_limit(&mut request, &guard).unwrap());
    let input_upper = u64::try_from(serde_json::to_vec(&request).unwrap().len()).unwrap();
    let output_upper = request["max_output_tokens"].as_u64().unwrap();
    assert!(output_upper < 16_384);
    assert!(
        guard
            .estimated_cost_micros
            .saturating_add(input_upper.saturating_mul(5))
            .saturating_add(output_upper.saturating_mul(15))
            <= guard.hard_limit_micros
    );

    let exhausted = CostGuard {
        estimated_cost_micros: 4_999_999,
        hard_limit_micros: 5_000_000,
        ..CostGuard::default()
    };
    assert!(!constrain_request_to_cost_limit(&mut request, &exhausted).unwrap());
}

#[test]
fn missing_provider_usage_is_accounted_with_the_dispatched_request_upper_bound() {
    let request = json!({
        "input": [{"role": "user", "content": "bounded implementation context"}],
        "max_output_tokens": 512,
    });
    let request_bytes = u64::try_from(serde_json::to_vec(&request).unwrap().len()).unwrap();
    let (input_tokens, output_tokens, estimated) =
        model_usage_for_accounting(&request, &json!({"output": []})).unwrap();
    assert_eq!(input_tokens, request_bytes);
    assert_eq!(output_tokens, 512);
    assert!(estimated);

    let (input_tokens, output_tokens, estimated) = model_usage_for_accounting(
        &request,
        &json!({"usage": {"input_tokens": 23, "output_tokens": 17}}),
    )
    .unwrap();
    assert_eq!((input_tokens, output_tokens), (23, 17));
    assert!(!estimated);
}

#[test]
fn authoritative_decision_controls_resume_work_instead_of_attempt_metadata() {
    use crate::execution_graph::ExecutionNodeId;

    assert!(execution_decision_requires_model_work(
        &ExecutionDecision::ContinuePlanning {
            action: crate::hosted_orchestrator::PlanningAction::BuildPlan {
                impact_map_id: crate::execution_graph::ArtifactId::new("impact-map:test"),
                evidence_ids: Vec::new(),
            },
        }
    ));
    let validation_passed = ExecutionDecision::ReviewDiff {
        node_id: ExecutionNodeId::new("diff-review"),
    };
    assert!(!execution_decision_requires_model_work(&validation_passed));
    assert!(execution_decision_has_completed_validation(
        &validation_passed
    ));
}

#[test]
fn partial_branch_changes_create_explicit_continuation_guidance() {
    let partial_run = PartialRunContext {
        pull_request_number: 138,
        changed_paths: vec![
            "src/components/theme/ThemeProvider.tsx".into(),
            "tests/theme-provider.test.tsx".into(),
        ],
        remaining_work: vec!["Add the planned end-to-end test.".into()],
    };
    let guidance = partial_implementation_guidance(Some(&partial_run));

    assert!(guidance.contains("Existing partial implementation detected"));
    assert!(guidance.contains("draft pull request #138"));
    assert!(guidance.contains("src/components/theme/ThemeProvider.tsx"));
    assert!(guidance.contains("tests/theme-provider.test.tsx"));
    assert!(guidance.contains("Add the planned end-to-end test."));
    assert!(guidance.contains("compare the existing implementation"));
    assert!(guidance.contains("Preserve correct completed work"));
    assert!(guidance.contains("continue from the current branch state"));
    assert!(guidance.contains("not proof that the mission is complete"));
}

#[test]
fn clean_branch_does_not_claim_that_partial_work_exists() {
    assert!(partial_implementation_guidance(None).is_empty());
}

#[test]
fn partial_run_detection_requires_a_later_attempt_resumed_draft_and_existing_diff() {
    let pull_request = PullRequest {
        number: 138,
        html_url: "https://github.com/RustGrid/example/pull/138".into(),
        node_id: Some("PR_node".into()),
        draft: true,
        body: Some(
            "⚠️ **INCOMPLETE — continue implementation before review or merge**\n\n\
Remaining work:\n\
- Add the planned end-to-end test.\n\
- Reconcile the failed source edit.\n\n\
Technical validation:\n- cargo test: passed"
                .into(),
        ),
    };
    let changed_paths = vec!["src/theme.rs".into()];

    let detected = detect_partial_run(Some(&pull_request), true, 2, changed_paths.clone()).unwrap();
    assert_eq!(detected.pull_request_number, 138);
    assert_eq!(detected.changed_paths, changed_paths);
    assert_eq!(
        detected.remaining_work,
        vec![
            "Add the planned end-to-end test.",
            "Reconcile the failed source edit."
        ]
    );
    assert!(
        detect_partial_run(Some(&pull_request), true, 1, vec!["src/theme.rs".into()]).is_none()
    );
    assert!(
        detect_partial_run(Some(&pull_request), false, 2, vec!["src/theme.rs".into()]).is_none()
    );
    assert!(detect_partial_run(Some(&pull_request), true, 2, Vec::new()).is_none());

    let complete_pull_request = PullRequest {
        draft: false,
        ..pull_request
    };
    assert!(
        detect_partial_run(
            Some(&complete_pull_request),
            true,
            2,
            vec!["src/theme.rs".into()]
        )
        .is_none()
    );
}

#[test]
fn recovered_partial_run_starts_from_planning_with_authoritative_remaining_work() {
    let mut manifest = test_manifest(Uuid::from_u128(17));
    manifest.execution.attempt_number = 2;
    manifest.run.attempt = 2;
    manifest.run.input_prompt = "\
Implement theme support.\n\n\
## Acceptance criteria\n\
- Theme selection persists.\n\
- Existing views use shared tokens.\n"
        .into();
    let partial_run = PartialRunContext {
        pull_request_number: 138,
        changed_paths: vec!["src/theme.rs".into(), "tests/theme.rs".into()],
        remaining_work: vec!["Add browser coverage.".into()],
    };

    let notebook = new_worker_notebook(&manifest, "fingerprint".into(), Some(&partial_run));
    let (impact_map, implementation_plan, phase) = notebook_orchestration_state(&notebook);

    assert_eq!(phase, ExecutionPhase::Planning);
    assert!(impact_map.is_some());
    assert!(implementation_plan.is_none());
    assert_eq!(notebook.remaining_work, vec!["Add browser coverage."]);
    assert_eq!(
        notebook.acceptance_criteria,
        vec![
            "Theme selection persists.",
            "Existing views use shared tokens."
        ]
    );
    assert_eq!(
        notebook.impact_map[0].candidate_paths,
        vec!["src/theme.rs", "tests/theme.rs"]
    );
}

#[test]
fn resumed_notebook_skips_completed_discovery_and_planning() {
    let notebook = WorkerNotebook {
        schema_version: 1,
        revision: 12,
        goal: "Apply a complete theme".into(),
        acceptance_criteria: vec!["All surfaces use the theme".into()],
        acceptance_criteria_v2: vec![impact_map::AcceptanceCriterion {
            id: "ac-1".into(),
            text: "All surfaces use the theme".into(),
        }],
        phase: ExecutionPhase::DiffReview,
        implementation_substate: ImplementationSubstate::Preparing,
        repository_base_sha: "a".repeat(40),
        branch: "rustgrid/aops-226-deadbeef".into(),
        repository_fingerprint: "b".repeat(64),
        execution_attempt: 2,
        architecture_findings: vec!["Tokens are centralized.".into()],
        impact_map: vec![ImpactArea {
            area_id: "area-tokens".into(),
            name: "tokens".into(),
            candidate_paths: vec!["src/theme.css".into()],
            evidence: vec![impact_map::ImpactEvidence {
                evidence_type: impact_map::EvidenceType::FileRead,
                path: Some("src/theme.css".into()),
                query: None,
                description: "inspected".into(),
            }],
            reason: "Shared token source".into(),
            acceptance_criteria_ids: vec!["ac-1".into()],
        }],
        impact_map_v2: None,
        impact_map_artifact: ArtifactCheckpoint {
            semantic_status: ArtifactSemanticStatus::Sufficient,
            persistence_status: ArtifactPersistenceStatus::Persisted,
            ..ArtifactCheckpoint::default()
        },
        impact_map_invalid_payload: None,
        impact_evidence: vec![],
        files_inspected: vec!["src/theme.css".into()],
        read_ranges_inspected: vec!["src/theme.css:1-400".into()],
        searches_completed: vec!["literal:src:theme".into()],
        discovery_paths_sampled: vec![],
        planned_changes: vec![PlannedChange {
            change_id: "change-1-theme".into(),
            parent_change_id: None,
            path: "src/theme.css".into(),
            targets: vec![],
            change: "Update tokens".into(),
            reason: "Central propagation".into(),
            status: IntendedChangeStatus::Planned,
            acceptance_criteria: vec!["All surfaces use the theme".into()],
            test_coverage: vec!["theme snapshot".into()],
        }],
        planning_repair: None,
        completed_changes: vec![],
        failed_changes: vec![],
        tool_progress: vec![],
        intended_changes: vec![],
        write_attempts: vec![],
        mutation_diagnostics: vec![],
        write_preflight_rejections: vec![],
        remaining_work: vec!["Update tokens".into()],
        remaining_work_v2: vec![],
        blocking_unknowns: vec![],
        validation_failures: vec![],
        validation_evidence: vec![],
        required_gates: vec![],
        dependency_bootstrap_evidence: None,
        phase_budget: json!({}),
        last_successful_action: json!({"tool": "read_file"}),
        last_orchestration_decision_key: None,
        finalization_revalidation: None,
        completion_artifact: None,
        phase_persistence_failure_code: None,
        orchestration: HostedOrchestrationCheckpoint::default(),
    };
    let (impact_map, plan, phase) = notebook_orchestration_state(&notebook);
    assert!(impact_map.is_some());
    assert!(plan.is_some());
    assert_eq!(phase, ExecutionPhase::Implementation);
}

#[test]
fn unrecovered_source_edit_failure_blocks_completion() {
    let implementation = ImplementationOutcome {
        summary: "claimed complete".into(),
        budget_exhausted: false,
        explicit_declaration: Some(ImplementationDeclaration {
            implementation_status: "complete".into(),
            completed_work: vec!["theme".into()],
            remaining_work: vec![],
            known_risks: vec![],
            changed_paths: vec!["src/theme.css".into()],
            criteria_evidence: vec![],
        }),
    };
    let failures = vec![ToolFailureRecord {
        attempt_index: 0,
        tool: "replace_text".into(),
        target: Some("src/theme.css".into()),
        error: "found zero matches".into(),
        recovered: false,
        change_id: Some("change-1-theme".into()),
        error_code: "replace_match_not_unique".into(),
        match_count: Some(0),
        reconciliation: FailureReconciliation::StillUnresolved,
        recovery: None,
        intended_change_sha256: Some("a".repeat(64)),
    }];
    let result = completion_fallback(
        &implementation,
        Some(&test_impact_map()),
        None,
        &failures,
        &["src/theme.css".into()],
        &["Theme can be selected".into()],
        &[],
        ProjectVerificationPolicy::default(),
    );
    assert_eq!(result.status, CompletionStatus::Incomplete);
    assert_eq!(result.unrecovered_tool_failures.len(), 1);
}

#[test]
fn complete_evaluation_requires_concrete_evidence_for_every_applicable_criterion() {
    let implementation = ImplementationOutcome {
        summary: "complete".into(),
        budget_exhausted: false,
        explicit_declaration: Some(ImplementationDeclaration {
            implementation_status: "complete".into(),
            completed_work: vec!["theme".into()],
            remaining_work: vec![],
            known_risks: vec![],
            changed_paths: vec!["src/theme.css".into()],
            criteria_evidence: vec![ImplementationCriterionEvidence {
                criterion: "Theme can be selected".into(),
                paths: vec!["src/theme.css".into()],
                evidence: "The diff adds the theme token set.".into(),
            }],
        }),
    };
    let evaluation = CompletionEvaluation {
        status: CompletionStatus::Complete,
        implementation_completeness: ImplementationCompleteness::Complete,
        verification_readiness: VerificationReadiness::AutomatedVerified,
        evaluation_source: EvaluationSource::Model,
        confidence: 0.95,
        criteria: vec![CriterionEvaluation {
            criterion_id: "ac-1".into(),
            criterion: "Theme can be selected".into(),
            verification_type: VerificationType::Code,
            status: CriterionStatus::Satisfied,
            evidence: vec![CompletionEvidence {
                path: "src/theme.css".into(),
                description: "Adds the complete theme token set.".into(),
            }],
            validation_evidence: vec!["cargo test".into()],
            missing_evidence: vec![],
            required_next_action: None,
        }],
        remaining_implementation_work: vec![],
        remaining_automated_verification: vec![],
        pending_external_review: vec![],
        optional_follow_up: vec![],
        review_checklist: vec![],
        unrecovered_tool_failures: vec![],
        summary: "All criteria have diff evidence.".into(),
    };
    let criteria = vec!["Theme can be selected".into()];
    assert!(
        validate_completion_evaluation(
            evaluation.clone(),
            &implementation,
            &[],
            &["src/theme.css".into()],
            &criteria,
        )
        .is_ok()
    );
    let hosted_result = HostedResult {
        summary: "complete".into(),
        branch: "rustgrid/complete".into(),
        commit: "b".repeat(40),
        pull_request: PullRequestResult {
            number: 2,
            url: "https://github.com/RustGrid/example/pull/2".into(),
        },
        validation: vec![ValidationResult {
            id: "test".into(),
            command: "cargo test".into(),
            status: "passed".into(),
            output: String::new(),
        }],
        completeness: evaluation.clone(),
        terminal_telemetry: TerminalTelemetry::default(),
    };
    assert_eq!(
        resolve_published_terminal_result(Uuid::nil(), &hosted_result, "2026-08-03T12:00:00Z")
            .mission_outcome,
        CanonicalMissionOutcome::Complete
    );
    let mut missing_evidence = evaluation;
    missing_evidence.criteria[0].evidence.clear();
    assert!(
        validate_completion_evaluation(
            missing_evidence,
            &implementation,
            &[],
            &["src/theme.css".into()],
            &criteria,
        )
        .is_err()
    );

    let missing_criterion = CompletionEvaluation {
        status: CompletionStatus::Complete,
        implementation_completeness: ImplementationCompleteness::Complete,
        verification_readiness: VerificationReadiness::AutomatedVerified,
        evaluation_source: EvaluationSource::Model,
        confidence: 0.9,
        criteria: vec![CriterionEvaluation {
            criterion_id: "ac-1".into(),
            criterion: "A different criterion".into(),
            verification_type: VerificationType::Code,
            status: CriterionStatus::Satisfied,
            evidence: vec![CompletionEvidence {
                path: "src/theme.css".into(),
                description: "Changed theme code.".into(),
            }],
            validation_evidence: vec![],
            missing_evidence: vec![],
            required_next_action: None,
        }],
        remaining_implementation_work: vec![],
        remaining_automated_verification: vec![],
        pending_external_review: vec![],
        optional_follow_up: vec![],
        review_checklist: vec![],
        unrecovered_tool_failures: vec![],
        summary: "Incomplete mapping.".into(),
    };
    assert!(
        validate_completion_evaluation(
            missing_criterion,
            &implementation,
            &[],
            &["src/theme.css".into()],
            &criteria,
        )
        .is_err()
    );
}

#[test]
fn partial_pull_request_is_prominently_marked_incomplete_and_resumable() {
    let manifest = test_manifest(Uuid::from_u128(0x11111111_1111_4111_8111_111111111111));
    let completeness = CompletionEvaluation {
        status: CompletionStatus::Partial,
        implementation_completeness: ImplementationCompleteness::Partial,
        verification_readiness: VerificationReadiness::Blocked,
        evaluation_source: EvaluationSource::OrchestratorFallback,
        confidence: 1.0,
        criteria: vec![],
        remaining_implementation_work: vec!["Add settings integration".into()],
        remaining_automated_verification: vec![],
        pending_external_review: vec![],
        optional_follow_up: vec![],
        review_checklist: vec![],
        unrecovered_tool_failures: vec![],
        summary: "Budget exhausted after one theme-provider edit.".into(),
    };
    let body = hosted_pull_request_body(&manifest, &[], &completeness);
    let title = hosted_pull_request_title(&manifest, true);
    assert!(body.contains("INCOMPLETE"));
    assert!(body.contains("Add settings integration"));
    assert!(body.contains("partial"));
    assert!(body.contains("### Completed"));
    assert!(body.contains("### Not completed"));
    assert!(body.contains("### Root cause"));
    assert!(body.contains("### Resume action"));
    assert!(body.contains("without repeating discovery, planning, or completed work"));
    assert!(title.starts_with("[INCOMPLETE]"));
}

#[test]
fn validation_incomplete_draft_lists_failed_and_pending_gates() {
    let manifest = test_manifest(Uuid::from_u128(0x11111111_1111_4111_8111_111111111111));
    let validation = vec![
        ValidationResult {
            id: "focused".into(),
            command: "npx vitest run tests/theme-provider.test.tsx".into(),
            status: "failed_code".into(),
            output: "AssertionError: expected root class\nExpected: light-blue\nReceived:\n❯ tests/theme-provider.test.tsx:42:17".into(),
        },
        ValidationResult {
            id: "suite".into(),
            command: "npm test".into(),
            status: "pending".into(),
            output: String::new(),
        },
        ValidationResult {
            id: "build".into(),
            command: "npm run build".into(),
            status: "pending".into(),
            output: String::new(),
        },
    ];
    let completeness = CompletionEvaluation {
        status: CompletionStatus::Partial,
        implementation_completeness: ImplementationCompleteness::Partial,
        verification_readiness: VerificationReadiness::PendingManualReview,
        evaluation_source: EvaluationSource::OrchestratorFallback,
        confidence: 1.0,
        criteria: vec![],
        remaining_implementation_work: vec!["Reconcile root class behavior.".into()],
        remaining_automated_verification: vec![
            "Rerun focused tests.".into(),
            "Run full suite and build.".into(),
        ],
        pending_external_review: vec!["Obtain product/design approval.".into()],
        optional_follow_up: vec![],
        review_checklist: vec![],
        unrecovered_tool_failures: vec!["Validation repair produced no mutation.".into()],
        summary: "Applied changes are preserved for draft review; validation is incomplete.".into(),
    };

    let body = hosted_pull_request_body(&manifest, &validation, &completeness);
    assert!(body.contains("Known validation failures"));
    assert!(body.contains("Expected: light-blue"));
    assert!(body.contains("Received:"));
    assert!(body.contains("- npm test not yet run"));
    assert!(body.contains("- npm run build not yet run"));
    assert!(body.contains("Obtain product/design approval"));
    assert!(body.contains("without repeating discovery, planning, or completed work"));
}

#[test]
fn deterministic_fallback_classifies_external_review_without_missing_code() {
    let criteria = vec![
        "The designated product owner approves the light-blue palette.".into(),
        "Complete manual accessibility contrast and keyboard focus review.".into(),
    ];
    let implementation = ImplementationOutcome {
        summary: "implementation complete".into(),
        budget_exhausted: false,
        explicit_declaration: Some(ImplementationDeclaration {
            implementation_status: "complete".into(),
            completed_work: vec!["theme implementation".into()],
            remaining_work: vec![],
            known_risks: vec![],
            changed_paths: vec!["src/theme.css".into()],
            criteria_evidence: vec![],
        }),
    };
    let result = completion_fallback(
        &implementation,
        None,
        None,
        &[],
        &["src/theme.css".into()],
        &criteria,
        &[ValidationResult {
            id: "test".into(),
            command: "npm test".into(),
            status: "passed".into(),
            output: String::new(),
        }],
        ProjectVerificationPolicy::default(),
    );

    assert_eq!(result.criteria.len(), criteria.len());
    assert_eq!(
        result.criteria[0].verification_type,
        VerificationType::ProductApproval
    );
    assert_eq!(
        result.criteria[1].verification_type,
        VerificationType::AccessibilityReview
    );
    assert!(
        result
            .criteria
            .iter()
            .all(|criterion| criterion.status == CriterionStatus::ExternalReviewRequired)
    );
    assert_eq!(
        result.implementation_completeness,
        ImplementationCompleteness::Complete
    );
    assert_eq!(
        result.verification_readiness,
        VerificationReadiness::PendingManualReview
    );
    assert_eq!(
        result.status,
        CompletionStatus::CompletePendingExternalReview
    );
    assert!(result.remaining_implementation_work.is_empty());
    assert_eq!(result.review_checklist.len(), 2);
}

#[test]
fn browser_e2e_policy_controls_implementation_completeness() {
    let criterion = "Theme persists through browser navigation and page reload.".to_string();
    let implementation = ImplementationOutcome {
        summary: "theme implementation complete".into(),
        budget_exhausted: false,
        explicit_declaration: Some(ImplementationDeclaration {
            implementation_status: "complete".into(),
            completed_work: vec!["theme persistence".into()],
            remaining_work: vec![],
            known_risks: vec![],
            changed_paths: vec!["src/theme.tsx".into()],
            criteria_evidence: vec![ImplementationCriterionEvidence {
                criterion: criterion.clone(),
                paths: vec!["src/theme.tsx".into()],
                evidence: "The provider persists and restores the selected theme.".into(),
            }],
        }),
    };
    let changed_paths = vec!["src/theme.tsx".into()];
    let criteria = vec![criterion];
    let validation = vec![ValidationResult {
        id: "test".into(),
        command: "npm test".into(),
        status: "passed".into(),
        output: String::new(),
    }];

    let optional = completion_fallback(
        &implementation,
        None,
        None,
        &[],
        &changed_paths,
        &criteria,
        &validation,
        ProjectVerificationPolicy {
            browser_e2e_required_for_theme_changes: false,
            manual_browser_verification_required: true,
        },
    );
    assert_eq!(
        optional.implementation_completeness,
        ImplementationCompleteness::Complete
    );
    assert_eq!(optional.status, CompletionStatus::Complete);
    assert_eq!(
        optional.criteria[0].verification_type,
        VerificationType::AutomatedTest
    );

    let mandatory = completion_fallback(
        &implementation,
        None,
        None,
        &[],
        &changed_paths,
        &criteria,
        &validation,
        ProjectVerificationPolicy {
            browser_e2e_required_for_theme_changes: true,
            manual_browser_verification_required: false,
        },
    );
    assert_eq!(
        mandatory.criteria[0].verification_type,
        VerificationType::AutomatedTest
    );
    assert_eq!(mandatory.status, CompletionStatus::Partial);
    assert!(!mandatory.remaining_automated_verification.is_empty());
}

#[test]
fn review_pending_pull_request_is_not_marked_implementation_incomplete() {
    let manifest = test_manifest(Uuid::from_u128(0x11111111_1111_4111_8111_111111111111));
    let implementation = ImplementationOutcome {
        summary: "complete".into(),
        budget_exhausted: false,
        explicit_declaration: Some(ImplementationDeclaration {
            implementation_status: "complete".into(),
            completed_work: vec!["palette".into()],
            remaining_work: vec![],
            known_risks: vec![],
            changed_paths: vec!["src/theme.css".into()],
            criteria_evidence: vec![],
        }),
    };
    let completeness = completion_fallback(
        &implementation,
        None,
        None,
        &[],
        &["src/theme.css".into()],
        &["Product owner approves the palette.".into()],
        &[],
        ProjectVerificationPolicy::default(),
    );
    let body = hosted_pull_request_body(&manifest, &[], &completeness);
    let title = hosted_pull_request_title(&manifest, false);
    assert!(body.contains("IMPLEMENTATION COMPLETE"));
    assert!(body.contains("External review checklist"));
    assert!(!body.contains("INCOMPLETE — continue implementation"));
    assert!(!title.starts_with("[INCOMPLETE]"));
    assert!(!requires_implementation_continuation(completeness.status));
}

#[test]
fn behavioral_verification_types_are_automated_and_approval_types_are_external() {
    for criterion in [
        "Theme selection works",
        "Theme cycling wraps around",
        "Selection persists after page reload",
        "Stored selection is restored",
        "Invalid storage uses the fallback",
        "Regression coverage remains green",
        "The application build succeeds",
    ] {
        assert_eq!(
            verification_type_for_criterion(criterion),
            VerificationType::AutomatedTest,
            "{criterion}"
        );
    }
    assert_eq!(
        verification_type_for_criterion("Product owner approval is recorded"),
        VerificationType::ProductApproval
    );
    assert_eq!(
        verification_type_for_criterion("Complete a visual review"),
        VerificationType::VisualReview
    );
}

#[test]
fn model_interpretation_cannot_downgrade_deterministic_satisfied_evidence() {
    let implementation = ImplementationOutcome {
        summary: "complete".into(),
        budget_exhausted: false,
        explicit_declaration: Some(ImplementationDeclaration {
            implementation_status: "complete".into(),
            completed_work: vec![],
            remaining_work: vec![],
            known_risks: vec![],
            changed_paths: vec!["src/theme.ts".into()],
            criteria_evidence: vec![],
        }),
    };
    let mut deterministic = test_completion_evaluation(CompletionStatus::Complete);
    deterministic.criteria = vec![CriterionEvaluation {
        criterion_id: "ac-1".into(),
        criterion: "Theme selection works".into(),
        verification_type: VerificationType::AutomatedTest,
        status: CriterionStatus::Satisfied,
        evidence: vec![CompletionEvidence {
            path: "src/theme.ts".into(),
            description: "Applied and diff-reviewed target".into(),
        }],
        validation_evidence: vec!["npm test".into()],
        missing_evidence: vec![],
        required_next_action: None,
    }];
    let mut model = deterministic.clone();
    model.criteria[0].status = CriterionStatus::Uncertain;
    model.criteria[0].evidence.clear();
    model.criteria[0].missing_evidence = vec!["Model could not infer behavior".into()];

    let reconciled =
        reconcile_model_completion_evaluation(model, deterministic, &implementation, &[]);
    assert_eq!(reconciled.criteria[0].status, CriterionStatus::Satisfied);
    assert_eq!(reconciled.criteria[0].evidence[0].path, "src/theme.ts");
    assert!(reconciled.criteria[0].missing_evidence.is_empty());
    assert_eq!(reconciled.status, CompletionStatus::Complete);
}

#[test]
fn canonical_terminal_mapping_is_exhaustive_for_healthy_results() {
    let cases = [
        (
            CompletionStatus::Complete,
            CanonicalMissionOutcome::Complete,
            "completed",
            "completed",
            false,
        ),
        (
            CompletionStatus::CompletePendingExternalReview,
            CanonicalMissionOutcome::CompletePendingExternalReview,
            "awaiting_external_review",
            "external_review_required",
            true,
        ),
        (
            CompletionStatus::Partial,
            CanonicalMissionOutcome::PartialReviewable,
            "partial_result",
            "partial_reviewable",
            true,
        ),
        (
            CompletionStatus::Blocked,
            CanonicalMissionOutcome::PartialReviewable,
            "partial_result",
            "partial_reviewable",
            true,
        ),
        (
            CompletionStatus::Incomplete,
            CanonicalMissionOutcome::PartialReviewable,
            "partial_result",
            "partial_reviewable",
            true,
        ),
        (
            CompletionStatus::Uncertain,
            CanonicalMissionOutcome::PartialReviewable,
            "partial_result",
            "partial_reviewable",
            true,
        ),
    ];
    for (completion_status, mission_outcome, status, reason, draft) in cases {
        let result = HostedResult {
            summary: "summary".into(),
            branch: "rustgrid/test".into(),
            commit: "a".repeat(40),
            pull_request: PullRequestResult {
                number: 31,
                url: "https://github.example/pull/31".into(),
            },
            validation: vec![],
            completeness: test_completion_evaluation(completion_status),
            terminal_telemetry: TerminalTelemetry::default(),
        };
        let terminal =
            resolve_published_terminal_result(Uuid::nil(), &result, "2026-08-03T12:00:00Z");
        assert_eq!(terminal.completion_request_status(), status);
        assert_eq!(terminal.reason_code, reason);
        assert_eq!(terminal.publication.draft, draft);
        assert_eq!(terminal.mission_outcome, mission_outcome);
        assert_eq!(terminal.process_health, ProcessHealth::Healthy);
        assert_eq!(terminal.process_exit_code(), 0);
        assert_ne!(terminal.completion.status, CompletionStatus::Uncertain);
    }
}

#[test]
fn compact_completion_packet_contains_no_validation_output_or_notebook_payload() {
    let packet = CompletionEvidencePacket {
        acceptance_criteria: vec!["Selection persists".into()],
        criterion_evidence: vec![CriterionEvidence {
            criterion_id: "ac-1".into(),
            changed_paths: vec!["src/theme.ts".into()],
            ..CriterionEvidence::default()
        }],
        validation_gate_statuses: vec![CompletionValidationGateStatus {
            gate_id: "focused".into(),
            command: "npm test".into(),
            status: "passed".into(),
        }],
        unresolved_failures: vec![],
        publication_intent: "publish_reviewable_pull_request".into(),
        diff_summary: "modified: src/theme.ts".into(),
    };
    let serialized = serde_json::to_string(&packet).unwrap();
    assert!(!serialized.contains("command output"));
    assert!(!serialized.contains("worker_notebook"));
    assert!(!serialized.contains("input_prompt"));
    assert!(serialized.len() < 3_072);
}

#[test]
fn compact_completion_request_fits_the_small_mission_node_budget() {
    let packet = CompletionEvidencePacket {
        acceptance_criteria: (1..=8)
            .map(|index| format!("Acceptance criterion {index}"))
            .collect(),
        criterion_evidence: (1..=8)
            .map(|index| CriterionEvidence {
                criterion_id: format!("ac-{index}"),
                applied_change_ids: vec![format!("change-{index}")],
                changed_paths: vec![format!("src/target-{index}.tsx")],
                verified_target_ids: vec![format!("source-{index}")],
                relevant_validation_gate_ids: vec!["focused".into(), "suite".into()],
                diff_review_findings: vec![
                    "diff_review_completed_without_blocking_findings".into(),
                ],
                external_evidence_requirements: vec![],
            })
            .collect(),
        validation_gate_statuses: vec![
            CompletionValidationGateStatus {
                gate_id: "focused".into(),
                command: "npx vitest run tests/theme-provider.test.tsx".into(),
                status: "passed".into(),
            },
            CompletionValidationGateStatus {
                gate_id: "suite".into(),
                command: "npm test".into(),
                status: "passed".into(),
            },
            CompletionValidationGateStatus {
                gate_id: "build".into(),
                command: "npm run build".into(),
                status: "passed".into(),
            },
        ],
        unresolved_failures: vec![],
        publication_intent: "publish_reviewable_pull_request".into(),
        diff_summary: (1..=8)
            .map(|index| format!("modified: src/target-{index}.tsx"))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let request = json!({
        "model": "gpt-5.6",
        "input": [{"role": "user", "content": serde_json::to_string(&packet).unwrap()}],
        "instructions": completion_evaluator_instructions(),
        "max_output_tokens": 3_072,
        "reasoning": {"effort": "low"},
    });
    let estimate = estimate_model_call_request_cost(&request);
    assert!(estimate.estimated_request_cost <= 300_000, "{estimate:?}");
}

#[test]
fn deterministic_criterion_evidence_joins_plan_graph_diff_and_relevant_gates() {
    use crate::execution_graph::{
        ExecutionNodeKind, ExecutionNodeStatus, MissionBudget, MissionComplexity,
        PlannedTarget as GraphTarget, ValidationGateSpec,
        ValidationGateType as GraphValidationGateType, build_execution_graph,
    };
    let target = GraphTarget {
        change_id: "change-theme-provider".into(),
        path: "src/theme-provider.tsx".into(),
        role: "production".into(),
        intent: "persist and restore theme selection".into(),
        acceptance_criteria_ids: vec!["ac-1".into()],
        operation: Default::default(),
        new_file: false,
    };
    let mut graph = build_execution_graph(
        "completion-evidence",
        MissionComplexity::Small,
        "tree-1",
        std::slice::from_ref(&target),
        &[ValidationGateSpec {
            gate_id: "focused".into(),
            gate_type: GraphValidationGateType::FocusedTest,
            command: "npm test -- theme-provider".into(),
            working_directory: String::new(),
            required: true,
            ..ValidationGateSpec::default()
        }],
        &MissionBudget::for_complexity(MissionComplexity::Small),
    );
    for node in &mut graph.nodes {
        if node.kind.is_mutation() {
            node.status = ExecutionNodeStatus::Applied;
        } else if node.kind == ExecutionNodeKind::DiffReview {
            node.status = ExecutionNodeStatus::Completed;
        }
    }
    let plan = ImplementationPlan {
        implementation_status: "ready".into(),
        planned_changes: vec![PlannedChange {
            change_id: target.change_id,
            parent_change_id: None,
            path: target.path.clone(),
            targets: vec![PlannedTarget {
                path: target.path.clone(),
                role: target.role,
                operation: Default::default(),
                new_file: false,
                status: IntendedChangeStatus::Applied,
            }],
            change: target.intent,
            reason: "Acceptance behavior".into(),
            status: IntendedChangeStatus::Applied,
            acceptance_criteria: vec!["ac-1".into()],
            test_coverage: vec!["focused".into()],
        }],
        planned_new_files: vec![],
        planned_test_changes: vec![],
        remaining_unknowns: vec![],
        blocking_unknowns: vec![],
    };
    let evidence = build_deterministic_criterion_evidence(
        Some(&plan),
        Some(&graph),
        &["Selection persists and is restored after refresh".into()],
        &["src/theme-provider.tsx".into()],
        &[test_passed_validation("npm test -- theme-provider")],
    );

    assert_eq!(evidence[0].criterion_id, "ac-1");
    assert_eq!(evidence[0].applied_change_ids, ["change-theme-provider"]);
    assert_eq!(evidence[0].changed_paths, ["src/theme-provider.tsx"]);
    assert_eq!(evidence[0].verified_target_ids.len(), 1);
    assert_eq!(
        evidence[0].relevant_validation_gate_ids,
        ["npm-test----theme-provider"]
    );
    assert_eq!(
        evidence[0].diff_review_findings,
        ["diff_review_completed_without_blocking_findings"]
    );
}

#[test]
fn cache_observability_explains_zero_reads_without_metadata_churn() {
    let first_request = json!({
        "model": "gpt-5.6",
        "instructions": "stable",
        "tools": [{"type": "function", "name": "read_files"}],
        "metadata": {"phase": "discovery"}
    });
    let second_request = json!({
        "model": "gpt-5.6",
        "instructions": "stable",
        "tools": [{"type": "function", "name": "read_files"}],
        "metadata": {"phase": "implementation"}
    });
    let response = json!({
        "usage": {"input_tokens_details": {"cached_tokens": 0}}
    });
    let (cold, prefix, tools) = cache_observability_payload(&first_request, &response, None, None);
    assert_eq!(cold["cache_invalidation_reason"], "cold_start");
    assert_eq!(cold["cache_read"], false);
    assert_eq!(cold["model_cache_support_reported"], true);
    assert_eq!(cold["gateway_forwarded_cache_fields"], false);

    let (stable, second_prefix, second_tools) =
        cache_observability_payload(&second_request, &response, Some(&prefix), Some(&tools));
    assert_eq!(prefix, second_prefix);
    assert_eq!(tools, second_tools);
    assert_eq!(
        stable["cache_invalidation_reason"],
        "provider_reported_zero_cache_read"
    );
    assert_eq!(stable["metadata_excluded_from_stable_prefix"], true);
}

#[test]
fn valid_impact_map_is_recovered_from_tool_arguments_and_notebook_progress() {
    let notebook = test_discovery_notebook(ExecutionPhase::Discovery);
    let mut map = test_impact_map();
    map.inspected_files.clear();
    map.searches.clear();
    let arguments = serde_json::to_string(&map).unwrap();

    let (recovered, _) = recover_impact_map(Some(&arguments), None, &notebook).unwrap();
    assert_eq!(recovered.inspected_files, notebook.files_inspected);
    assert_eq!(
        recovered
            .searches
            .iter()
            .map(|s| &s.query)
            .collect::<Vec<_>>(),
        notebook.searches_completed.iter().collect::<Vec<_>>()
    );
    assert_eq!(recovered.areas, map.areas);
}

#[test]
fn valid_impact_map_is_recovered_from_a_fenced_assistant_response() {
    let notebook = test_discovery_notebook(ExecutionPhase::Discovery);
    let response = format!(
        "```json\n{}\n```",
        serde_json::to_string(&test_impact_map()).unwrap()
    );

    let (recovered, _) = recover_impact_map(None, Some(&response), &notebook).unwrap();
    assert_eq!(recovered.areas, test_impact_map().areas);
}

#[test]
fn impact_map_fallback_rejects_unknown_or_invented_fields() {
    let notebook = test_discovery_notebook(ExecutionPhase::Discovery);
    let mut value = serde_json::to_value(test_impact_map()).unwrap();
    value["untrusted_extra"] = json!("do not accept");
    let arguments = serde_json::to_string(&value).unwrap();
    assert!(recover_impact_map(Some(&arguments), None, &notebook).is_err());
}

#[test]
fn semantic_impact_map_survives_failed_persistence_and_resumes_planning() {
    let mut notebook = test_discovery_notebook(ExecutionPhase::Planning);
    let map = test_impact_map();
    notebook.impact_map = map.areas.clone();
    notebook.impact_map_artifact = ArtifactCheckpoint {
        artifact: "impact_map".into(),
        semantic_status: ArtifactSemanticStatus::Sufficient,
        serialization_status: ArtifactSerializationStatus::Valid,
        persistence_status: ArtifactPersistenceStatus::Failed,
        artifact_sha256: impact_map_sha256(&map),
        model_call_index: Some(8),
        phase: ExecutionPhase::Discovery,
        safe_error: Some("worker event transport failed".into()),
        normalization_metadata: None,
        artifact_source: Some(ArtifactSource::Model),
        confidence: Some(1.0),
        failure_layer: Some(ArtifactFailureLayer::ArtifactPersistence),
        validation_errors: Vec::new(),
        invalid_payload_shape: None,
    };

    let (restored, plan, phase) = notebook_orchestration_state(&notebook);
    assert!(restored.is_some());
    assert!(plan.is_none());
    assert_eq!(phase, ExecutionPhase::Planning);
    assert_eq!(
        notebook.impact_map_artifact.semantic_status,
        ArtifactSemanticStatus::Sufficient
    );
    assert_eq!(
        notebook.impact_map_artifact.persistence_status,
        ArtifactPersistenceStatus::Failed
    );
}

#[test]
fn invalid_impact_map_resume_preserves_discovery_and_targets_artifact_repair() {
    let notebook = test_discovery_notebook(ExecutionPhase::ArtifactRepair);
    let (map, plan, phase) = notebook_orchestration_state(&notebook);
    assert!(map.is_none());
    assert!(plan.is_none());
    assert_eq!(phase, ExecutionPhase::ArtifactRepair);
    assert_eq!(
        notebook.files_inspected,
        vec!["src/components/theme/ThemeProvider.tsx"]
    );
    assert_eq!(hosted_tools_for_phase(phase).len(), 1);
    assert_eq!(
        hosted_tools_for_phase(phase)[0]["name"],
        "record_impact_map"
    );
}

#[test]
fn artifact_repair_context_contains_exact_corrections_without_discovery_transcript() {
    let notebook = test_discovery_notebook(ExecutionPhase::ArtifactRepair);
    let invalid = json!({"areas":[{"name":"Theme","candidate_paths":[]}]});
    let failure = ImpactMapFailure {
        code: "impact_map_schema_mismatch",
        safe_error: "invalid".into(),
        errors: vec![ValidationError {
            path: "$.areas[0].candidate_paths".into(),
            keyword: "minItems".into(),
            message: "At least one candidate path is required.".into(),
        }],
        invalid_payload: invalid.clone(),
        invalid_payload_shape: impact_map::safe_shape(&invalid),
        failure_layer: ArtifactFailureLayer::WorkerToolSchemaValidation,
    };
    let context = compact_impact_map_repair_context(Some(&failure), &notebook);
    assert!(context.contains("$.areas[0].candidate_paths"));
    assert!(context.contains("evidence_id"));
    assert!(context.contains("ac-1"));
    assert!(!context.contains("Theme tokens are centralized"));
}

#[test]
fn artifact_repair_context_remains_below_five_thousand_tokens() {
    let context = compact_impact_map_repair_context(
        None,
        &test_discovery_notebook(ExecutionPhase::ArtifactRepair),
    );
    assert!(context.len().div_ceil(4) < 5_000);
}

#[test]
fn supplemental_repair_accounting_is_separate_from_mission_budget() {
    let accounting = artifact_call_accounting(ExecutionPhase::ArtifactRepair);
    assert_eq!(accounting["provider_call_occurred"], true);
    assert_eq!(accounting["configured_mission_budget_consumed"], false);
    assert_eq!(accounting["supplemental_repair_budget_consumed"], true);
}

#[test]
fn formatting_failure_is_healthy_blocked_and_resumable() {
    let failure = HostedAgentExecutionFailure {
        status: "blocked",
        category: "hosted_agent_execution_failed",
        process_health: "healthy",
        mission_outcome: "blocked",
        blocker: Some("impact_map_artifact_invalid".into()),
        resumable: true,
        code: "impact_map_schema_mismatch".into(),
        phase: ExecutionPhase::ArtifactRepair,
        message: "repair".into(),
        underlying_error: UnderlyingFailure {
            r#type: "orchestration_guardrail".into(),
            message: "schema".into(),
            stack_reference: None,
        },
        model_calls_used: 6,
        model_calls_limit: 10,
        model_calls_remaining: 4,
        phase_calls_used: 1,
        phase_calls_limit: 1,
        last_successful_action: json!({}),
        usage: ToolUsage::default(),
        estimated_cost_micros: 0,
        input_tokens: 0,
        output_tokens: 0,
        changed_paths: vec![],
        remaining_work: vec![],
        failed_tool_operations: vec![],
        current_plan: vec![],
        validation_evidence: vec![],
        notebook_revision: 0,
        recoverable: true,
        resume_phase: "artifact_repair".into(),
        resume_from_node: None,
        repository_fingerprint: String::new(),
        recommended_action: "resume".into(),
        artifact: Some("impact_map".into()),
        semantic_status: Some(ArtifactSemanticStatus::Invalid),
        persistence_status: Some(ArtifactPersistenceStatus::PendingRetry),
        rustgrid_gateway_status: None,
        upstream_provider_status: None,
        failure_stage: None,
        provider_contacted: None,
        call_budget_consumed: None,
        reservation_state: None,
        reservation_reconciliation_state: None,
        rustgrid_request_id: None,
        transport_request_id: None,
        provider_request_id: None,
        provider_error: None,
        provider_response_body: None,
        model_alias: None,
        resolved_provider_model: None,
        adapter_version: None,
        payload_schema_version: None,
        provider_attempts: None,
        actual_cost_micros: None,
    };
    let value = serde_json::to_value(failure).unwrap();
    assert_eq!(value["process_health"], "healthy");
    assert_eq!(value["mission_outcome"], "blocked");
    assert_eq!(value["resumable"], true);
}

#[test]
fn resume_revision_eight_reuses_discovery_without_discovery_tools() {
    let mut notebook = test_discovery_notebook(ExecutionPhase::ArtifactRepair);
    notebook.revision = 8;
    let (_, _, phase) = notebook_orchestration_state(&notebook);
    assert_eq!(notebook.revision, 8);
    assert_eq!(phase, ExecutionPhase::ArtifactRepair);
    let names = hosted_tools_for_phase(phase)
        .into_iter()
        .filter_map(|v| v["name"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["record_impact_map"]);
    assert!(!names.contains(&"read_file".into()));
}

fn test_discovery_notebook(phase: ExecutionPhase) -> WorkerNotebook {
    WorkerNotebook {
        schema_version: 1,
        revision: 4,
        goal: "Apply a complete theme".into(),
        acceptance_criteria: vec!["All surfaces use the theme".into()],
        acceptance_criteria_v2: vec![impact_map::AcceptanceCriterion {
            id: "ac-1".into(),
            text: "All surfaces use the theme".into(),
        }],
        phase,
        implementation_substate: ImplementationSubstate::Preparing,
        repository_base_sha: "a".repeat(40),
        branch: "rustgrid/aops-226-deadbeef".into(),
        repository_fingerprint: "b".repeat(64),
        execution_attempt: 1,
        architecture_findings: vec!["Theme tokens are centralized.".into()],
        impact_map: vec![],
        impact_map_v2: None,
        impact_map_artifact: ArtifactCheckpoint {
            semantic_status: ArtifactSemanticStatus::Invalid,
            persistence_status: ArtifactPersistenceStatus::PendingRetry,
            ..ArtifactCheckpoint::default()
        },
        impact_map_invalid_payload: None,
        impact_evidence: impact_map::evidence_catalog(
            &["src/components/theme/ThemeProvider.tsx".into()],
            &["literal:src:ThemeProvider".into()],
        ),
        files_inspected: vec!["src/components/theme/ThemeProvider.tsx".into()],
        read_ranges_inspected: vec!["src/components/theme/ThemeProvider.tsx:1-400".into()],
        searches_completed: vec!["literal:src:ThemeProvider".into()],
        discovery_paths_sampled: vec![],
        planned_changes: vec![],
        planning_repair: None,
        completed_changes: vec![],
        failed_changes: vec![],
        tool_progress: vec![],
        intended_changes: vec![],
        write_attempts: vec![],
        mutation_diagnostics: vec![],
        write_preflight_rejections: vec![],
        remaining_work: vec![],
        remaining_work_v2: vec![],
        blocking_unknowns: vec![],
        validation_failures: vec![],
        validation_evidence: vec![],
        required_gates: vec![],
        dependency_bootstrap_evidence: None,
        phase_budget: json!({}),
        last_successful_action: json!({"tool": "read_files"}),
        last_orchestration_decision_key: None,
        finalization_revalidation: None,
        completion_artifact: None,
        phase_persistence_failure_code: None,
        orchestration: HostedOrchestrationCheckpoint::default(),
    }
}

fn test_theme_planning_notebook() -> WorkerNotebook {
    let mut notebook = test_discovery_notebook(ExecutionPhase::Planning);
    notebook.acceptance_criteria = vec![
        "Light-blue can be selected and restored.".into(),
        "Theme cycling and existing themes continue to work.".into(),
    ];
    notebook.acceptance_criteria_v2 = vec![
        impact_map::AcceptanceCriterion {
            id: "ac-1".into(),
            text: notebook.acceptance_criteria[0].clone(),
        },
        impact_map::AcceptanceCriterion {
            id: "ac-2".into(),
            text: notebook.acceptance_criteria[1].clone(),
        },
    ];
    let paths = [
        "src/components/theme/ThemeProvider.tsx",
        "src/components/theme/ThemeToggle.tsx",
        "src/app/globals.css",
        "tests/theme-provider.test.tsx",
    ];
    let map = ImpactMap {
        schema_version: IMPACT_MAP_SCHEMA_VERSION.into(),
        areas: paths
            .iter()
            .enumerate()
            .map(|(index, path)| ImpactArea {
                area_id: format!("area-theme-{index}"),
                name: format!("theme surface {index}"),
                candidate_paths: vec![(*path).into()],
                evidence: vec![impact_map::ImpactEvidence {
                    evidence_type: impact_map::EvidenceType::FileRead,
                    path: Some((*path).into()),
                    query: None,
                    description: "inspected during discovery".into(),
                }],
                reason: "This existing theme surface implements selection, persistence, cycling, tokens, or regression coverage.".into(),
                acceptance_criteria_ids: vec!["ac-1".into(), "ac-2".into()],
            })
            .collect(),
        inspected_files: paths.iter().map(|path| (*path).into()).collect(),
        searches: vec![],
        unresolved_questions: vec![],
    };
    notebook.impact_map = map.areas.clone();
    notebook.impact_map_v2 = Some(map);
    notebook.files_inspected = paths.iter().map(|path| (*path).into()).collect();
    for path in paths {
        notebook.orchestration.evidence.capture_file(
            path,
            &notebook.repository_fingerprint,
            None,
            format!("current repository evidence for {path}"),
            false,
        );
    }
    notebook.orchestration.evidence.capture_file(
        "package.json",
        &notebook.repository_fingerprint,
        None,
        r#"{"scripts":{"test":"vitest","lint":"eslint .","build":"vite build"}}"#,
        false,
    );
    notebook
}

#[test]
fn deterministic_theme_plan_reuses_impact_evidence_and_repository_validation_contracts() {
    let notebook = test_theme_planning_notebook();
    let plan = deterministic_plan_from_impact_map(&notebook).expect("fallback plan");
    let accepted = validate_and_repair_plan_criteria(
        plan,
        &notebook.acceptance_criteria_v2,
        &notebook.impact_map,
    )
    .expect("fallback plan must pass the normal acceptance path");
    let changes = accepted
        .plan
        .planned_changes
        .iter()
        .map(|change| {
            (
                change.targets[0].path.as_str(),
                change.change.to_ascii_lowercase(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(changes.len(), 4);
    assert!(changes["src/components/theme/ThemeProvider.tsx"].contains("storage"));
    assert!(changes["src/components/theme/ThemeToggle.tsx"].contains("accessible"));
    assert!(changes["src/app/globals.css"].contains("semantic palette"));
    assert!(changes["tests/theme-provider.test.tsx"].contains("persistence"));
    assert_eq!(
        repository_validation_commands_from_evidence(&notebook),
        vec!["npm run build", "npm run lint", "npm run test"]
    );
    let context = compact_implementation_plan_context(&notebook, None);
    assert!(context.contains("current repository evidence for"));
    assert!(context.contains("npm run test"));
}

fn test_planned_change() -> PlannedChange {
    PlannedChange {
        change_id: "theme-tests".into(),
        parent_change_id: None,
        path: String::new(),
        targets: vec![PlannedTarget {
            path: "tests/theme-provider.test.tsx".into(),
            role: "focused theme coverage".into(),
            operation: Default::default(),
            new_file: false,
            status: IntendedChangeStatus::Planned,
        }],
        change: "Add light-blue theme coverage.".into(),
        reason: "Verify registration, persistence, cycling, and fallback behavior.".into(),
        status: IntendedChangeStatus::Planned,
        acceptance_criteria: vec!["Theme can be selected".into()],
        test_coverage: vec!["npm test".into()],
    }
}

fn test_write_failure(
    change_id: &str,
    target: &str,
    intended_change_sha256: &str,
) -> ToolFailureRecord {
    ToolFailureRecord {
        attempt_index: 0,
        change_id: Some(change_id.into()),
        tool: "replace_text".into(),
        target: Some(target.into()),
        error_code: "replace_match_not_unique".into(),
        match_count: Some(2),
        error: "replace_match_not_unique: found 2 matches".into(),
        recovered: false,
        reconciliation: FailureReconciliation::StillUnresolved,
        recovery: None,
        intended_change_sha256: Some(intended_change_sha256.into()),
    }
}

fn test_complete_implementation() -> ImplementationOutcome {
    ImplementationOutcome {
        summary: "Implemented and validated the theme.".into(),
        budget_exhausted: false,
        explicit_declaration: Some(ImplementationDeclaration {
            implementation_status: "complete".into(),
            completed_work: vec!["Added light-blue theme coverage.".into()],
            remaining_work: vec![],
            known_risks: vec![],
            changed_paths: vec!["tests/theme-provider.test.tsx".into()],
            criteria_evidence: vec![ImplementationCriterionEvidence {
                criterion: "Theme can be selected".into(),
                paths: vec!["tests/theme-provider.test.tsx".into()],
                evidence: "Registration and persistence assertions are present.".into(),
            }],
        }),
    }
}

fn test_passed_validation(command: &str) -> ValidationResult {
    ValidationResult {
        id: command.replace(' ', "-"),
        command: command.into(),
        status: "passed".into(),
        output: String::new(),
    }
}

fn test_completion_evaluation(status: CompletionStatus) -> CompletionEvaluation {
    CompletionEvaluation {
        status,
        implementation_completeness: match status {
            CompletionStatus::Complete | CompletionStatus::CompletePendingExternalReview => {
                ImplementationCompleteness::Complete
            }
            CompletionStatus::Partial => ImplementationCompleteness::Partial,
            CompletionStatus::Blocked
            | CompletionStatus::Incomplete
            | CompletionStatus::Uncertain => ImplementationCompleteness::Incomplete,
        },
        verification_readiness: VerificationReadiness::Blocked,
        evaluation_source: EvaluationSource::OrchestratorFallback,
        confidence: 1.0,
        criteria: vec![],
        remaining_implementation_work: vec![],
        remaining_automated_verification: vec![],
        pending_external_review: vec![],
        optional_follow_up: vec![],
        review_checklist: vec![],
        unrecovered_tool_failures: vec![],
        summary: status.as_str().into(),
    }
}

fn test_impact_map() -> ImpactMap {
    ImpactMap {
        schema_version: IMPACT_MAP_SCHEMA_VERSION.into(),
        areas: vec![ImpactArea {
            area_id: "area-theme".into(),
            name: "theme".into(),
            candidate_paths: vec!["src/theme.css".into()],
            evidence: vec![impact_map::ImpactEvidence {
                evidence_type: impact_map::EvidenceType::FileRead,
                path: Some("src/theme.css".into()),
                query: None,
                description: "inspected".into(),
            }],
            reason: "The token source propagates to every themed surface.".into(),
            acceptance_criteria_ids: vec!["ac-1".into()],
        }],
        inspected_files: vec!["src/theme.css".into()],
        searches: vec![impact_map::ImpactSearch {
            query: "theme".into(),
            scope: None,
        }],
        unresolved_questions: vec![],
    }
}

#[test]
fn hosted_tools_have_only_the_gateway_allowed_function_shape() {
    let tools = hosted_tools();
    validate_provider_tool_definitions(&json!(&tools)).unwrap();
    for tool in tools {
        let object = tool.as_object().unwrap();
        assert_eq!(object.get("type"), Some(&json!("function")));
        assert!(object.get("name").is_some_and(Value::is_string));
        assert_eq!(object.get("strict"), Some(&json!(true)));
        let parameters = object.get("parameters").and_then(Value::as_object).unwrap();
        assert_eq!(parameters.get("additionalProperties"), Some(&json!(false)));
        let properties = parameters
            .get("properties")
            .and_then(Value::as_object)
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let required = parameters
            .get("required")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(properties, required);
        assert!(object.len() <= 5);
    }
}

#[test]
fn provider_tool_preflight_rejects_duplicate_and_invalid_strict_schemas() {
    let valid = json!({
        "type": "function",
        "name": "read_file",
        "description": "Read one file.",
        "parameters": {
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        },
        "strict": true
    });
    let duplicate = json!([valid.clone(), valid]);
    assert!(
        validate_provider_tool_definitions(&duplicate)
            .unwrap_err()
            .to_string()
            .contains("duplicate tool name")
    );

    let invalid = json!([{
        "type": "function",
        "name": "write_file",
        "parameters": {
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        },
        "strict": true
    }]);
    let error = validate_provider_tool_definitions(&invalid).unwrap_err();
    assert!(error.to_string().contains("additionalProperties"));
    assert!(error.to_string().contains("tools[0].parameters"));
}

#[test]
fn provider_schema_preflight_rejects_unsupported_keywords_and_excess_depth() {
    let unsupported = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "pattern": "^src/"}
        },
        "required": ["path"],
        "additionalProperties": false
    });
    assert!(
        validate_provider_json_schema(&unsupported, "schema", 0, true, true)
            .unwrap_err()
            .to_string()
            .contains("schema.properties.path.pattern")
    );

    let mut nested = json!({"type": "string"});
    for _ in 0..10 {
        nested = json!({"type": "array", "items": nested});
    }
    assert!(
        validate_provider_json_schema(&nested, "schema", 0, false, false)
            .unwrap_err()
            .to_string()
            .contains("nesting depth")
    );
}

#[test]
fn provider_schema_preflight_rejects_type_mismatches_and_missing_array_items() {
    for (schema, expected_path) in [
        (
            json!({"type": "string", "enum": ["safe", 7]}),
            "schema.enum",
        ),
        (json!({"type": "string", "minimum": 1}), "schema"),
        (json!({"type": "array"}), "schema.items"),
    ] {
        let error = validate_provider_json_schema(&schema, "schema", 0, false, false).unwrap_err();
        assert!(
            error.to_string().contains(expected_path),
            "unexpected schema error: {error}"
        );
    }
}

#[test]
fn phase_tool_admission_protects_implementation_and_completion_reserves() {
    assert!(phase_permits_tool(
        ExecutionPhase::Discovery,
        "record_impact_map"
    ));
    assert!(!phase_permits_tool(ExecutionPhase::Discovery, "write_file"));
    assert!(phase_permits_tool(
        ExecutionPhase::Planning,
        "record_implementation_plan"
    ));
    assert!(!phase_permits_tool(
        ExecutionPhase::Planning,
        "replace_text"
    ));
    assert!(phase_permits_tool(
        ExecutionPhase::Implementation,
        "replace_text"
    ));
    assert!(!phase_permits_tool(
        ExecutionPhase::Implementation,
        "run_focused_command"
    ));
    assert!(phase_permits_tool(ExecutionPhase::Repair, "read_file"));
    assert!(phase_permits_tool(
        ExecutionPhase::DiffReview,
        "declare_implementation"
    ));
    assert!(!phase_permits_tool(
        ExecutionPhase::DiffReview,
        "replace_text"
    ));
    assert!(!phase_permits_tool(
        ExecutionPhase::CompletionEvaluation,
        "write_file"
    ));
    assert!(!phase_permits_tool(
        ExecutionPhase::Validation,
        "search_text"
    ));
    assert!(!phase_permits_tool(
        ExecutionPhase::Publication,
        "run_focused_command"
    ));
}

#[test]
fn hosted_dependency_bootstrap_is_locked_and_ignores_lifecycle_scripts() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("package.json"), "{}").unwrap();
    fs::write(directory.path().join("package-lock.json"), "{}").unwrap();
    assert_eq!(
        hosted_dependency_bootstrap(directory.path()),
        Some((
            "npm",
            "npm ci --ignore-scripts --no-audit --no-fund --prefer-offline"
        ))
    );
}

#[test]
fn safe_failures_never_include_raw_remote_error_bodies() {
    let error = anyhow::Error::new(HostedHttpError {
        status: StatusCode::BAD_GATEWAY,
        path: "executions/id/ai/responses".into(),
        code: "ai_provider_unavailable".into(),
        request_id: Some("request-1".into()),
        rustgrid_gateway_status: None,
        upstream_provider_status: None,
        failure_stage: None,
        provider_contacted: None,
        call_budget_consumed: None,
        reservation_state: None,
        reservation_reconciliation_state: None,
        retryable: None,
        rustgrid_request_id: None,
        transport_request_id: None,
        provider_request_id: None,
        provider_error: None,
        provider_response_body: None,
        model_alias: None,
        resolved_provider_model: None,
        adapter_version: None,
        payload_schema_version: None,
        provider_attempts: None,
        actual_cost_micros: None,
    });
    let (code, message) = safe_failure(&error, false);
    assert_eq!(code, "ai_provider_unavailable");
    assert_eq!(
        message,
        "The upstream model provider failed while processing the request."
    );
    assert!(!message.contains("responses"));
}

#[test]
fn structured_failures_preserve_phase_usage_and_actionable_cause() {
    let mut failure = test_execution_failure(
        "search_loop_detected",
        "Repeated discovery search was rejected.",
    );
    failure.underlying_error = UnderlyingFailure {
        r#type: "orchestration_guardrail".into(),
        message: "duplicate_search_rejected".into(),
        stack_reference: Some("request-2".into()),
    };
    failure.model_calls_used = 7;
    failure.model_calls_remaining = 33;
    failure.phase_calls_used = 7;
    failure.last_successful_action = json!({"tool": "read_files"});
    failure.usage = ToolUsage {
        reads: 6,
        searches: 4,
        ..ToolUsage::default()
    };
    failure.recommended_action = "Record the impact map.".into();
    let error = anyhow::Error::new(failure);
    let (terminal_code, terminal_message) = safe_failure(&error, false);
    assert_eq!(terminal_code, "search_loop_detected");
    assert_eq!(terminal_message, "Repeated discovery search was rejected.");
    let diagnostics = failure_diagnostics(&error, false);
    assert_eq!(diagnostics["code"], "search_loop_detected");
    assert_eq!(diagnostics["phase"], "discovery");
    assert_eq!(diagnostics["model_calls_used"], 7);
    assert_eq!(diagnostics["model_calls_limit"], 40);
    assert_eq!(diagnostics["usage"]["searches"], 4);
    assert_eq!(
        diagnostics["underlying_error"]["message"],
        "duplicate_search_rejected"
    );
}

#[test]
fn github_oidc_request_uses_audience_and_bearer_without_logging_the_jwt() {
    let jwt = format!("{}.{}.{}", "a".repeat(30), "b".repeat(30), "c".repeat(30));
    let Some((base, request, server)) = one_request_server("200 OK", json!({"value": jwt})) else {
        return;
    };
    let execution_id = Uuid::from_u128(40);
    let environment = GithubActionsEnvironment {
        api_root: base.join("api/v1/").unwrap(),
        audience: base.origin().ascii_serialization(),
        oidc_request_url: base.join("oidc?existing=1").unwrap(),
        oidc_request_token: SecretString::new("oidc-request-bearer".into(), "test").unwrap(),
        dispatch_nonce: SecretString::new("d".repeat(48), "test").unwrap(),
        repository: None,
        repository_id: None,
        sha: None,
        workflow_run_id: None,
        workflow_run_attempt: None,
        actor: None,
        actor_id: None,
    };
    let token = request_github_oidc(&hosted_http_client().unwrap(), &environment).unwrap();
    server.join().unwrap();
    assert_eq!(token.expose(), jwt);
    let request = request.recv().unwrap();
    assert!(request.starts_with("GET /oidc?existing=1&audience="));
    assert!(request.contains("authorization: Bearer oidc-request-bearer"));
    assert!(!format!("{token:?}").contains(&jwt));
    let _ = execution_id;
}

#[test]
fn oidc_exchange_posts_only_the_scoped_identity_contract() {
    let execution_id = Uuid::from_u128(41);
    let response = exchange_response(execution_id);
    let body = json!({
        "access_token": response.access_token,
        "token_type": response.token_type,
        "expires_in": response.expires_in,
        "expires_at": response.expires_at,
        "token_id": response.token_id,
        "tenant_id": response.tenant_id,
        "project_id": response.project_id,
        "execution_id": response.execution_id,
        "execution_attempt": response.execution_attempt,
        "session_id": response.session_id,
        "worker_id": response.worker_id,
        "repository_id": response.repository_id,
        "github_workflow_run_id": response.github_workflow_run_id,
        "permissions": response.permissions
    });
    let Some((base, request, server)) = one_request_server("200 OK", body) else {
        return;
    };
    let environment = GithubActionsEnvironment {
        api_root: base.join("api/v1/").unwrap(),
        audience: base.origin().ascii_serialization(),
        oidc_request_url: base.join("oidc").unwrap(),
        oidc_request_token: SecretString::new("request-bearer".into(), "test").unwrap(),
        dispatch_nonce: SecretString::new("n".repeat(48), "test").unwrap(),
        repository: None,
        repository_id: None,
        sha: None,
        workflow_run_id: None,
        workflow_run_attempt: None,
        actor: None,
        actor_id: None,
    };
    let jwt = SecretString::new(
        format!("{}.{}.{}", "a".repeat(30), "b".repeat(30), "c".repeat(30)),
        "test",
    )
    .unwrap();
    let exchanged = exchange_github_oidc(
        &hosted_http_client().unwrap(),
        &environment,
        execution_id,
        &jwt,
    )
    .unwrap();
    server.join().unwrap();
    assert_eq!(exchanged.execution_id, execution_id);
    let request = request.recv().unwrap();
    assert!(request.starts_with("POST /api/v1/execution-auth/github-actions/exchange HTTP/1.1"));
    assert!(request.contains(&format!("\"execution_id\":\"{execution_id}\"")));
    assert!(request.contains("\"dispatch_nonce\":\"nnnn"));
    assert!(request.contains("\"github_oidc_token\":\"aaaa"));
    assert!(!request.contains("OPENAI_API_KEY"));
}

#[test]
fn execution_token_refresh_rotates_the_in_memory_bearer() {
    let execution_id = Uuid::from_u128(42);
    let refreshed = format!("rge_{}", "b".repeat(48));
    let Some((base, request, server)) = one_request_server(
        "200 OK",
        json!({
            "access_token": refreshed,
            "token_type": "Bearer",
            "expires_at": "2099-01-01T00:00:00Z",
            "token_id": Uuid::from_u128(43),
            "session_id": Uuid::from_u128(33)
        }),
    ) else {
        return;
    };
    let clock = Arc::new(ManualHostedClock::new(
        parse_rfc3339_utc("2026-08-03T00:00:00Z").unwrap(),
    ));
    let client = test_api_client_with_clock(base, execution_id, clock.clone());
    {
        let mut state = client.auth.lock().unwrap();
        state.expires_at = clock.system_now() + Duration::from_secs(600);
        state.refresh_after = clock.system_now() + Duration::from_secs(300);
    }
    clock.advance(Duration::from_secs(301));
    client.ensure_fresh().unwrap();
    server.join().unwrap();
    let request = request.recv().unwrap();
    assert!(request.starts_with(&format!(
        "POST /api/v1/executions/{execution_id}/token/refresh HTTP/1.1"
    )));
    assert!(request.contains(&format!("authorization: Bearer rge_{}", "a".repeat(48))));
    assert_eq!(client.current_token().unwrap().expose(), refreshed);
}

#[test]
fn capped_token_refresh_is_not_repeated_on_every_worker_operation() {
    let execution_id = Uuid::from_u128(0x30303030_3030_4030_8030_303030303030);
    let capped_expiry = "2099-01-01T00:00:00Z";
    let Some((base, _, server)) = one_request_server(
        "200 OK",
        json!({
            "access_token": format!("rge_{}", "c".repeat(48)),
            "token_type": "Bearer",
            "expires_at": capped_expiry,
            "token_id": Uuid::from_u128(0x31),
            "session_id": Uuid::from_u128(33)
        }),
    ) else {
        return;
    };
    let client = test_api_client(base, execution_id);
    {
        let mut state = client.auth.lock().unwrap();
        state.expires_at = parse_rfc3339_utc(capped_expiry).unwrap();
        state.refresh_after = SystemTime::now();
    }
    client.ensure_fresh().unwrap();
    server.join().unwrap();
    let state = client.auth.lock().unwrap();
    assert_eq!(state.refresh_after, state.expires_at);
}

#[test]
fn ai_registration_separates_semantic_calls_from_transport_attempts() {
    let execution_id = Uuid::from_u128(44);
    let session_id = Uuid::from_u128(45);
    let registration =
        ai_call_registration(execution_id, 9, session_id, 0, ExecutionPhase::Discovery, 0);

    assert_eq!(
        registration.semantic_call_id,
        ai_call_registration(
            execution_id,
            9,
            Uuid::from_u128(46),
            0,
            ExecutionPhase::Discovery,
            1
        )
        .semantic_call_id
    );
    assert_ne!(
        registration.semantic_call_id,
        ai_call_registration(
            execution_id,
            10,
            session_id,
            0,
            ExecutionPhase::Discovery,
            0
        )
        .semantic_call_id
    );
    assert_ne!(
        registration.semantic_call_id,
        ai_call_registration(execution_id, 9, session_id, 1, ExecutionPhase::Discovery, 0)
            .semantic_call_id
    );
    assert_ne!(
        registration.request_id,
        ai_call_registration(
            execution_id,
            9,
            Uuid::from_u128(46),
            0,
            ExecutionPhase::Discovery,
            0
        )
        .request_id
    );
    assert_ne!(
        registration.request_id,
        ai_call_registration(execution_id, 9, session_id, 0, ExecutionPhase::Discovery, 1)
            .request_id
    );
}

#[test]
fn gateway_failure_contract_separates_registration_from_provider_status() {
    let execution_id = Uuid::from_u128(47);
    let Some((base, _request, server)) = one_request_server(
        "409 Conflict",
        json!({
            "code": "ai_call_index_conflict",
            "details": {
                "failure_stage": "request_registration",
                "provider_contacted": false,
                "call_budget_consumed": false,
                "reservation_reconciliation_state": "released",
                "retryable": true
            }
        }),
    ) else {
        return;
    };
    let client = test_api_client(base, execution_id);
    let error = client
        .ai_response(
            json!({
                "model": "gpt-5.6-sol",
                "input": "bounded",
                "max_output_tokens": 100,
                "store": false,
                "stream": false
            }),
            &ai_call_registration(
                execution_id,
                1,
                Uuid::from_u128(49),
                0,
                ExecutionPhase::Discovery,
                0,
            ),
        )
        .unwrap_err();
    server.join().unwrap();

    let failure = error.downcast_ref::<HostedHttpError>().unwrap();
    assert_eq!(failure.status, StatusCode::CONFLICT);
    assert_eq!(failure.effective_code(), "ai_call_index_conflict");
    assert_eq!(failure.failure_stage(), Some("request_registration"));
    assert_eq!(failure.upstream_provider_status, None);
    assert_eq!(failure.provider_contacted(), Some(false));
    assert_eq!(failure.call_budget_consumed(), Some(false));
    assert_eq!(failure.reservation_reconciliation_state(), Some("released"));
    assert!(failure.retryable_registration_failure());
}

#[test]
fn provider_http_400_is_authoritative_and_is_not_retried_as_a_gateway_failure() {
    let execution_id = Uuid::from_u128(50);
    let provider_request_id = "b4dd40ed-d63b-4df9-81c1-3e886f7949d5";
    let rustgrid_request_id = "e24ad61e-ab87-485f-a2e1-6a6d9456ad0e";
    let transport_request_id = "ed798a57-5611-4d47-b060-69c79b34ac3c";
    let provider_message = "Invalid type for 'metadata.model_call_budget': expected a string, but got an integer instead.";
    let Some((base, _request, server)) = one_request_server(
        "502 Bad Gateway",
        json!({
            "code": "ai_provider_invalid_request",
            "details": {
                "failure_stage": "provider_dispatch",
                "provider_contacted": true,
                "upstream_provider_status": 400,
                "rustgrid_gateway_status": null,
                "rustgrid_request_id": rustgrid_request_id,
                "transport_request_id": transport_request_id,
                "provider_request_id": provider_request_id,
                "reservation_state": "reconciled",
                "provider_error": {
                    "type": "invalid_request_error",
                    "code": "invalid_type",
                    "message": provider_message,
                    "parameter": "metadata.model_call_budget"
                },
                "provider_response_body": {
                    "error": {
                        "message": provider_message,
                        "parameter": "metadata.model_call_budget"
                    }
                },
                "model_alias": "gpt-5.6-sol",
                "resolved_provider_model": "gpt-5.6-sol",
                "adapter_version": "openai-responses-v1",
                "payload_schema_version": "rustgrid.execution_ai.responses.v1",
                "provider_attempts": 1,
                "model_calls_used": 0,
                "call_budget_consumed": false,
                "actual_cost_micros": 0,
                "recoverable": true
            }
        }),
    ) else {
        return;
    };
    let client = test_api_client(base, execution_id);
    let registration = ai_call_registration(
        execution_id,
        1,
        Uuid::from_u128(51),
        0,
        ExecutionPhase::Discovery,
        0,
    );
    let error = client
        .ai_response(
            json!({
                "model": "gpt-5.6-sol",
                "input": "bounded",
                "metadata": {"model_call_budget": "40"},
                "store": false,
                "stream": false
            }),
            &registration,
        )
        .unwrap_err();
    server.join().unwrap();

    let failure = error.downcast_ref::<HostedHttpError>().unwrap();
    assert_eq!(failure.status, StatusCode::BAD_GATEWAY);
    assert_eq!(failure.failure_class(), AiFailureClass::ProviderValidation);
    assert_eq!(failure.effective_code(), "ai_provider_invalid_request");
    assert_eq!(failure.failure_stage(), Some("provider_dispatch"));
    assert_eq!(failure.rustgrid_gateway_status(), Some(None));
    assert_eq!(failure.upstream_provider_status, Some(400));
    assert_eq!(failure.provider_contacted(), Some(true));
    assert_eq!(failure.call_budget_consumed(), Some(false));
    assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Restore);
    assert!(!failure.retryable_gateway_transport_failure());
    assert!(!failure.retryable_registration_failure());
    assert_eq!(
        failure.terminal_message(),
        "The upstream model provider rejected the request as invalid."
    );
    assert_eq!(
        failure.provider_request_id.as_deref(),
        Some(provider_request_id)
    );
    assert_eq!(
        failure.rustgrid_request_id.as_deref(),
        Some(rustgrid_request_id)
    );
    assert_eq!(
        failure.transport_request_id.as_deref(),
        Some(transport_request_id)
    );
    assert_eq!(failure.reservation_state(), Some("reconciled"));
    let provider_error = failure.provider_error.as_ref().unwrap();
    assert_eq!(
        provider_error.error_type.as_deref(),
        Some("invalid_request_error")
    );
    assert_eq!(provider_error.code.as_deref(), Some("invalid_type"));
    assert_eq!(provider_error.message.as_deref(), Some(provider_message));
    assert_eq!(
        provider_error.parameter.as_deref(),
        Some("metadata.model_call_budget")
    );
    assert_eq!(
        failure.provider_response_body.as_ref().unwrap()["error"]["message"],
        provider_message
    );
    assert_eq!(failure.provider_attempts, Some(1));
    assert_eq!(failure.actual_cost_micros, Some(0));

    let event = provider_rejected_event(
        failure,
        &registration,
        1,
        1,
        "gpt-5.6-sol",
        0,
        json!({"model_calls_used": 0}),
        json!({"phase": "discovery"}),
    );
    assert_eq!(event["event_type"], "execution.ai.provider_rejected");
    assert_eq!(event["failure_stage"], "provider_dispatch");
    assert_eq!(event["rustgrid_gateway_status"], Value::Null);
    assert_eq!(event["upstream_provider_status"], 400);
    assert_eq!(event["provider_attempts"], 1);
    assert_eq!(event["rustgrid_request_id"], rustgrid_request_id);
    assert_eq!(event["transport_request_id"], transport_request_id);
    assert_eq!(event["reservation_state"], "reconciled");
    assert_eq!(event["model_calls_used"], 0);
    assert_eq!(event["call_budget_consumed"], false);
    assert_eq!(event["actual_cost_micros"], 0);
    assert_eq!(
        event["provider_error"]["message"].as_str(),
        Some(provider_message)
    );

    let mut terminal = test_execution_failure(
        "ai_provider_invalid_request",
        "The upstream model provider rejected the request as invalid.",
    );
    terminal.rustgrid_gateway_status = failure.rustgrid_gateway_status();
    terminal.upstream_provider_status = failure.upstream_provider_status;
    terminal.failure_stage = failure.failure_stage().map(str::to_owned);
    terminal.provider_contacted = failure.provider_contacted();
    terminal.call_budget_consumed = failure.call_budget_consumed();
    terminal.reservation_state = failure.reservation_state().map(str::to_owned);
    terminal.rustgrid_request_id = failure.rustgrid_request_id.clone();
    terminal.transport_request_id = failure.transport_request_id.clone();
    terminal.provider_error = failure.provider_error.clone();
    let terminal = serde_json::to_value(terminal).unwrap();
    assert!(
        terminal
            .as_object()
            .unwrap()
            .contains_key("rustgrid_gateway_status")
    );
    assert!(terminal["rustgrid_gateway_status"].is_null());
    assert_eq!(terminal["rustgrid_request_id"], rustgrid_request_id);
    assert_eq!(terminal["transport_request_id"], transport_request_id);
    assert_eq!(terminal["reservation_state"], "reconciled");

    let (terminal_code, terminal_message) = safe_failure(&error, false);
    assert_eq!(terminal_code, "ai_provider_invalid_request");
    assert_eq!(
        terminal_message,
        "The upstream model provider rejected the request as invalid."
    );
    assert!(!terminal_message.contains("registration"));
    assert!(!terminal_message.contains("uncertain"));
}

#[test]
fn provider_request_metadata_is_string_typed_and_preflight_rejects_integer_values() {
    let metadata = provider_request_metadata(
        Uuid::from_u128(52),
        "AOPS-229",
        "rustgrid-agent-hosted",
        ExecutionPhase::Discovery,
        40,
    );
    assert_eq!(metadata["model_call_budget"], "40");
    assert!(metadata.as_object().unwrap().values().all(Value::is_string));
    let request = json!({
        "model": "gpt-5.6-sol",
        "input": [{"role": "user", "content": "bounded"}],
        "max_output_tokens": 100,
        "metadata": metadata,
    });
    validate_provider_request_envelope(&request).unwrap();

    let invalid = json!({
        "model": "gpt-5.6-sol",
        "input": [{"role": "user", "content": "bounded"}],
        "max_output_tokens": 100,
        "metadata": {
            "model_call_budget": 40
        },
    });
    let error = validate_provider_request_envelope(&invalid).unwrap_err();
    assert_eq!(
        error.to_string(),
        "ai_provider_request_invalid: metadata value `model_call_budget` must be a string"
    );
}

#[test]
fn startup_provider_schema_failure_preserves_exact_code_path_and_zero_dispatch_evidence() {
    let invalid = json!([{
        "type": "function",
        "name": "read_file",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "array"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        },
        "strict": true
    }]);
    let validation = validate_provider_tool_definitions(&invalid).unwrap_err();
    let error = anyhow::Error::new(HostedProviderContractFailure::from_validation(anyhow!(
        ProviderProtocolDiagnostic::new("ai_tool_schema_invalid", validation)
    )));

    let (code, message) = safe_failure(&error, false);
    assert_eq!(code, "ai_tool_schema_invalid");
    assert!(message.contains("tools[0].parameters.properties.path.items"));

    let diagnostics = failure_diagnostics(&error, false);
    assert_eq!(diagnostics["code"], "ai_tool_schema_invalid");
    assert_eq!(diagnostics["failure_stage"], "request_validation");
    assert_eq!(diagnostics["provider_contacted"], false);
    assert_eq!(diagnostics["reservation_state"], "not_created");
    assert_eq!(diagnostics["call_budget_consumed"], false);
    assert_eq!(diagnostics["actual_cost_micros"], 0);
    assert!(
        diagnostics["message"]
            .as_str()
            .is_some_and(|value| value.contains("tools[0].parameters.properties.path.items"))
    );
}

#[test]
fn large_safe_provider_400_retains_authoritative_fields_and_boundary_diagnostics() {
    let message = "m".repeat(MAX_PROVIDER_ERROR_MESSAGE_BYTES);
    let parameter = "p".repeat(MAX_PROVIDER_ERROR_PARAMETER_BYTES);
    let provider_response_body = json!({
        "error": {
            "message": "b".repeat(32 * 1024)
        }
    });
    let body = json!({
        "code": "ai_provider_invalid_request",
        "details": {
            "failure_stage": "provider_dispatch",
            "provider_contacted": true,
            "upstream_provider_status": 400,
            "rustgrid_gateway_status": null,
            "call_budget_consumed": false,
            "actual_cost_micros": 0,
            "provider_error": {
                "type": "invalid_request_error",
                "code": "invalid_type",
                "message": message,
                "parameter": parameter
            },
            "provider_response_body": provider_response_body,
            "provider_attempts": 1
        }
    });
    let Some((url, receiver, handle)) = one_request_server("400 Bad Request", body) else {
        return;
    };
    let response = hosted_http_client()
        .unwrap()
        .get(url)
        .send()
        .expect("provider error response");
    let error = decode_response::<Value>(response, "executions/id/ai/responses").unwrap_err();
    let failure = error
        .downcast_ref::<HostedHttpError>()
        .expect("typed hosted HTTP error");

    assert_eq!(failure.effective_code(), "ai_provider_invalid_request");
    assert_eq!(failure.failure_stage(), Some("provider_dispatch"));
    assert_eq!(failure.provider_contacted(), Some(true));
    assert_eq!(failure.upstream_provider_status, Some(400));
    assert_eq!(failure.rustgrid_gateway_status(), Some(None));
    assert_eq!(failure.call_budget_consumed(), Some(false));
    assert_eq!(failure.actual_cost_micros, Some(0));
    assert_eq!(failure.provider_attempts, Some(1));
    assert_eq!(
        failure
            .provider_error
            .as_ref()
            .and_then(|diagnostic| diagnostic.message.as_deref()),
        Some(message.as_str())
    );
    assert_eq!(
        failure
            .provider_error
            .as_ref()
            .and_then(|diagnostic| diagnostic.parameter.as_deref()),
        Some(parameter.as_str())
    );
    assert_eq!(
        failure.provider_response_body.as_ref(),
        Some(&provider_response_body)
    );

    receiver.recv().unwrap();
    handle.join().unwrap();
}

#[test]
fn provider_failure_classes_have_distinct_authoritative_policies() {
    let cases = [
        (
            "ai_provider_request_failed",
            Some(400),
            AiFailureClass::ProviderValidation,
            "ai_provider_invalid_request",
        ),
        (
            "ai_provider_rate_limited",
            Some(429),
            AiFailureClass::ProviderRateLimit,
            "ai_provider_rate_limited",
        ),
        (
            "ai_provider_authentication_failed",
            Some(401),
            AiFailureClass::ProviderAuthentication,
            "ai_provider_authentication_failed",
        ),
        (
            "ai_provider_unavailable",
            Some(503),
            AiFailureClass::ProviderServer,
            "ai_provider_unavailable",
        ),
        (
            "ai_provider_timeout",
            Some(408),
            AiFailureClass::ProviderTimeout,
            "ai_provider_timeout",
        ),
    ];
    for (code, upstream_status, class, effective_code) in cases {
        let failure =
            test_hosted_http_error(StatusCode::BAD_GATEWAY, code, upstream_status, Some(true));
        assert_eq!(failure.failure_class(), class);
        assert_eq!(failure.effective_code(), effective_code);
        assert_eq!(failure.failure_stage(), Some("provider_dispatch"));
        assert_eq!(failure.rustgrid_gateway_status(), None);
        assert_eq!(failure.provider_contacted(), Some(true));
        assert!(!failure.retryable_gateway_transport_failure());
        assert!(!failure.terminal_message().contains("registration"));
    }

    let mut uncertain = test_hosted_http_error(
        StatusCode::BAD_GATEWAY,
        "ai_request_dispatch_uncertain",
        None,
        Some(true),
    );
    uncertain.failure_stage = Some("provider_dispatch".into());
    assert_eq!(
        uncertain.failure_class(),
        AiFailureClass::ProviderDispatchUncertain
    );
    assert_eq!(uncertain.budget_disposition(), AiBudgetDisposition::Unknown);
    assert!(uncertain.terminal_message().contains("could not determine"));
}

#[test]
fn explicit_provider_400_overrides_the_legacy_conflict_template() {
    let mut failure = test_hosted_http_error(
        StatusCode::CONFLICT,
        "ai_provider_request_failed",
        Some(400),
        Some(true),
    );
    failure.failure_stage = Some("request_registration".into());
    failure.call_budget_consumed = Some(false);
    failure.actual_cost_micros = Some(0);

    assert_eq!(failure.failure_class(), AiFailureClass::ProviderValidation);
    assert_eq!(failure.effective_code(), "ai_provider_invalid_request");
    assert_eq!(failure.failure_stage(), Some("provider_dispatch"));
    assert_eq!(failure.rustgrid_gateway_status(), None);
    assert_eq!(
        failure.terminal_message(),
        "The upstream model provider rejected the request as invalid."
    );
    assert!(!failure.retryable_registration_failure());
}

#[test]
fn definite_provider_400_overrides_stale_dispatch_uncertain_code() {
    let mut failure = test_hosted_http_error(
        StatusCode::BAD_GATEWAY,
        "ai_request_dispatch_uncertain",
        Some(400),
        Some(true),
    );
    failure.call_budget_consumed = Some(false);
    failure.actual_cost_micros = Some(0);

    assert_eq!(failure.failure_class(), AiFailureClass::ProviderValidation);
    assert_eq!(failure.effective_code(), "ai_provider_invalid_request");
    assert_eq!(failure.failure_stage(), Some("provider_dispatch"));
    assert_eq!(failure.upstream_provider_status, Some(400));
    assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Restore);
}

#[test]
fn only_confirmed_non_billable_provider_validation_restores_semantic_budget() {
    let mut failure = test_hosted_http_error(
        StatusCode::BAD_GATEWAY,
        "ai_provider_invalid_request",
        Some(400),
        Some(true),
    );
    failure.call_budget_consumed = Some(false);
    failure.actual_cost_micros = Some(0);

    let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
    ledger.begin_model_call().unwrap();
    assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Restore);
    ledger
        .rollback_model_call(ExecutionPhase::Discovery)
        .unwrap();
    assert_eq!(ledger.budgeted_calls(), 0);

    failure.actual_cost_micros = None;
    assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Unknown);
    failure.actual_cost_micros = Some(0);
    failure.call_budget_consumed = Some(true);
    assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Consumed);
}

#[test]
fn authoritative_non_billable_adapter_preflight_restores_semantic_budget() {
    let mut failure = test_hosted_http_error(
        StatusCode::BAD_REQUEST,
        "ai_tool_schema_invalid",
        None,
        Some(false),
    );
    failure.failure_stage = Some("request_validation".into());
    failure.call_budget_consumed = Some(false);
    failure.actual_cost_micros = Some(0);
    failure.reservation_state = Some("not_created".into());

    assert_eq!(failure.failure_class(), AiFailureClass::RequestValidation);
    assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Restore);
    assert!(!failure.retryable_gateway_transport_failure());
    assert!(!failure.retryable_registration_failure());

    let mut ledger = PhaseLedger::new(40, ExecutionPhase::Discovery);
    ledger.begin_model_call().unwrap();
    ledger
        .rollback_model_call(ExecutionPhase::Discovery)
        .unwrap();
    assert_eq!(ledger.budgeted_calls(), 0);

    failure.reservation_state = None;
    assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Unknown);
    failure.failure_stage = None;
    failure.reservation_state = Some("not_created".into());
    assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Restore);
}

#[test]
fn explicit_non_billable_pre_dispatch_release_restores_semantic_budget() {
    let mut failure = test_hosted_http_error(
        StatusCode::BAD_GATEWAY,
        "ai_provider_connection_not_found",
        None,
        Some(false),
    );
    failure.failure_stage = Some("provider_credential_resolution".into());
    failure.call_budget_consumed = Some(false);
    failure.actual_cost_micros = Some(0);
    failure.reservation_state = Some("released".into());

    assert_eq!(failure.failure_class(), AiFailureClass::Gateway);
    assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Restore);

    failure.actual_cost_micros = None;
    assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Unknown);
    failure.actual_cost_micros = Some(0);
    failure.upstream_provider_status = Some(400);
    assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Unknown);
}

#[test]
fn ambiguous_legacy_provider_failure_does_not_fabricate_registration_evidence() {
    let failure = HostedHttpError {
        status: StatusCode::CONFLICT,
        path: "executions/id/ai/responses".into(),
        code: "ai_provider_request_failed".into(),
        request_id: Some("24162c59-38d5-4705-80f9-717c8c26ee29".into()),
        rustgrid_gateway_status: None,
        upstream_provider_status: None,
        failure_stage: None,
        provider_contacted: None,
        call_budget_consumed: None,
        reservation_state: None,
        reservation_reconciliation_state: None,
        retryable: None,
        rustgrid_request_id: None,
        transport_request_id: None,
        provider_request_id: None,
        provider_error: None,
        provider_response_body: None,
        model_alias: None,
        resolved_provider_model: None,
        adapter_version: None,
        payload_schema_version: None,
        provider_attempts: None,
        actual_cost_micros: None,
    };

    assert_eq!(failure.failure_class(), AiFailureClass::Gateway);
    assert_eq!(failure.effective_code(), "ai_provider_request_failed");
    assert_eq!(failure.failure_stage(), None);
    assert_eq!(failure.provider_contacted(), None);
    assert_eq!(failure.call_budget_consumed(), None);
    assert_eq!(failure.reservation_reconciliation_state(), None);
    assert_eq!(failure.rustgrid_gateway_status(), Some(Some(409)));
    assert_eq!(
        failure.terminal_message(),
        "The RustGrid AI gateway rejected the model call."
    );
    assert!(!failure.retryable_registration_failure());
}

#[test]
fn provider_invalid_code_without_dispatch_evidence_remains_gateway_unknown() {
    let failure = test_hosted_http_error(
        StatusCode::BAD_GATEWAY,
        "ai_provider_invalid_request",
        None,
        None,
    );

    assert_eq!(failure.failure_class(), AiFailureClass::Gateway);
    assert_eq!(failure.effective_code(), "ai_provider_invalid_request");
    assert_eq!(failure.failure_stage(), None);
    assert_eq!(failure.provider_contacted(), None);
    assert_eq!(failure.rustgrid_gateway_status(), Some(Some(502)));
    assert_eq!(failure.budget_disposition(), AiBudgetDisposition::Unknown);
}

#[test]
fn settled_pre_dispatch_registration_is_retryable_even_if_legacy_flag_is_false() {
    let failure = HostedHttpError {
        status: StatusCode::CONFLICT,
        path: "executions/id/ai/responses".into(),
        code: "ai_request_idempotency_conflict".into(),
        request_id: Some("24162c59-38d5-4705-80f9-717c8c26ee29".into()),
        rustgrid_gateway_status: None,
        upstream_provider_status: None,
        failure_stage: Some("request_registration".into()),
        provider_contacted: Some(false),
        call_budget_consumed: Some(false),
        reservation_state: None,
        reservation_reconciliation_state: Some("previous_request_settled".into()),
        retryable: Some(false),
        rustgrid_request_id: None,
        transport_request_id: None,
        provider_request_id: None,
        provider_error: None,
        provider_response_body: None,
        model_alias: None,
        resolved_provider_model: None,
        adapter_version: None,
        payload_schema_version: None,
        provider_attempts: None,
        actual_cost_micros: None,
    };

    assert!(failure.retryable_registration_failure());
}

#[test]
fn registration_retry_delays_are_bounded_and_jittered() {
    let semantic_call_id = Uuid::from_u128(0x1234);
    let first = registration_retry_delay(0, semantic_call_id);
    let second = registration_retry_delay(1, semantic_call_id);
    let third = registration_retry_delay(2, semantic_call_id);

    assert!((Duration::from_millis(200)..=Duration::from_millis(300)).contains(&first));
    assert!((Duration::from_millis(800)..=Duration::from_millis(1_200)).contains(&second));
    assert!((Duration::from_millis(2_400)..=Duration::from_millis(3_600)).contains(&third));
}

#[test]
fn ai_gateway_request_and_transport_retries_stop_at_execution_deadline() {
    let execution_id = Uuid::from_u128(0xdead_1e18);
    let Some((base, requests, server)) = delayed_no_response_server(Duration::from_millis(500))
    else {
        return;
    };
    let client = test_api_client(base, execution_id);
    let started = Instant::now();
    let error = client
        .ai_response_until(
            json!({
                "model": "gpt-5.6-sol",
                "input": "bounded",
                "max_output_tokens": 100,
                "store": false,
                "stream": false
            }),
            &ai_call_registration(
                execution_id,
                1,
                Uuid::from_u128(0xdead_1e19),
                0,
                ExecutionPhase::Implementation,
                0,
            ),
            Some(Instant::now() + Duration::from_millis(75)),
        )
        .unwrap_err();
    let elapsed = started.elapsed();
    server.join().unwrap();
    let requests = requests.try_iter().collect::<Vec<_>>();

    assert!(error.to_string().contains("hosted execution deadline"));
    assert!(
        elapsed < Duration::from_millis(350),
        "deadline-bounded request took {elapsed:?}"
    );
    assert_eq!(
        requests.len(),
        1,
        "deadline must suppress transport retries"
    );
}

#[test]
fn ai_gateway_and_completion_use_execution_bearer_and_idempotency_keys() {
    let execution_id = Uuid::from_u128(44);
    let Some((base, ai_request, ai_server)) = one_request_server("200 OK", json!({"output": []}))
    else {
        return;
    };
    let client = test_api_client(base, execution_id);
    client
        .ai_response(
            json!({
                "model": "gpt-5.6-sol",
                "input": "bounded",
                "max_output_tokens": 100,
                "store": false,
                "stream": false
            }),
            &ai_call_registration(
                execution_id,
                1,
                Uuid::from_u128(45),
                0,
                ExecutionPhase::Discovery,
                0,
            ),
        )
        .unwrap();
    ai_server.join().unwrap();
    let ai_request = ai_request.recv().unwrap();
    assert!(ai_request.starts_with(&format!(
        "POST /api/v1/executions/{execution_id}/ai/responses HTTP/1.1"
    )));
    assert!(ai_request.contains("idempotency-key:"));
    assert!(ai_request.contains("x-rustgrid-semantic-call-id:"));
    assert!(ai_request.contains("x-rustgrid-call-index: 0"));
    assert!(ai_request.contains("x-rustgrid-call-phase: discovery"));
    assert!(ai_request.contains("x-rustgrid-registration-attempt: 0"));
    assert!(ai_request.contains("authorization: Bearer rge_"));
    assert!(!ai_request.contains("OPENAI_API_KEY"));

    let Some((completion_base, completion_request, completion_server)) =
        one_request_server("200 OK", json!({"status": "failed"}))
    else {
        return;
    };
    let completion_client = test_api_client(completion_base, execution_id);
    completion_client
        .complete(&CompletionRequest {
            status: "failed".into(),
            canonical_terminal_result_id: None,
            terminal_revision: None,
            terminal_authority: None,
            canonical_terminal_result: None,
            mission_outcome: None,
            process_health: Some("failed".into()),
            completion_evaluation: None,
            output_summary: None,
            failure_code: Some("validation_failed".into()),
            failure_message: Some("Required validation failed.".into()),
            head_branch: None,
            head_sha: None,
            pull_request_number: None,
            pull_request_url: None,
            final_callback: None,
        })
        .unwrap();
    completion_server.join().unwrap();
    let completion_request = completion_request.recv().unwrap();
    assert!(completion_request.starts_with(&format!(
        "POST /api/v1/executions/{execution_id}/complete HTTP/1.1"
    )));
    assert!(completion_request.contains("idempotency-key:"));
    assert!(completion_request.contains("\"failure_code\":\"validation_failed\""));
    assert!(!completion_request.contains("OPENAI_API_KEY"));
}

#[test]
fn notebook_events_use_stable_idempotency_keys() {
    let execution_id = Uuid::from_u128(0x45454545_4545_4545_8545_454545454545);
    let Some((base, request, server)) = one_request_server("200 OK", json!({})) else {
        return;
    };
    let client = test_api_client(base, execution_id);
    client
        .append_event(
            "progress",
            json!({
                "event_type": "worker.notebook_checkpoint",
                "notebook_revision": 7,
                "artifact_hash": "a".repeat(64),
            }),
        )
        .unwrap();
    server.join().unwrap();
    let request = request.recv().unwrap();
    assert!(request.starts_with(&format!(
        "POST /api/v1/executions/{execution_id}/worker-events HTTP/1.1"
    )));
    assert!(request.contains("idempotency-key:"));
    assert!(request.contains("\"notebook_revision\":7"));
}

#[test]
fn worker_event_persistence_retry_reuses_identical_body_and_idempotency_key() {
    let execution_id = Uuid::from_u128(0x45454545_4545_4545_8545_454545454546);
    let Some((base, requests, server)) = request_sequence_server(vec![
        (
            "503 Service Unavailable",
            json!({"code": "temporary_failure"}),
        ),
        ("200 OK", json!({})),
    ]) else {
        return;
    };
    let client = test_api_client(base, execution_id);
    client
        .append_event(
            "progress",
            json!({
                "event_type": "worker.phase_transition",
                "from_phase": "repair",
                "phase": "diff_review",
                "decision": "review_incomplete_diff",
                "reason_code": "validation_rerun_pending",
            }),
        )
        .unwrap();
    server.join().unwrap();
    let requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    let identity = |request: &str| {
        request
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("idempotency-key:"))
            .map(str::to_owned)
            .expect("idempotency key")
    };
    let body = |request: &str| request.split_once("\r\n\r\n").unwrap().1.to_owned();
    assert_eq!(identity(&requests[0]), identity(&requests[1]));
    assert_eq!(body(&requests[0]), body(&requests[1]));
}

#[test]
fn worker_event_contract_rejection_is_permanent_and_not_retried() {
    let execution_id = Uuid::from_u128(0x45454545_4545_4545_8545_454545454547);
    let Some((base, request, server)) = one_request_server(
        "400 Bad Request",
        json!({"code": "worker_event_contract_invalid", "message": "invalid transition"}),
    ) else {
        return;
    };
    let client = test_api_client(base, execution_id);
    let error = client
        .append_event(
            "progress",
            json!({"event_type": "worker.phase_transition", "phase": "diff_review"}),
        )
        .unwrap_err();
    server.join().unwrap();
    assert_eq!(request.try_iter().count(), 1);
    let http = error
        .downcast_ref::<HostedHttpError>()
        .expect("structured control-plane rejection");
    assert_eq!(http.status, StatusCode::BAD_REQUEST);
    assert_eq!(http.code, "worker_event_contract_invalid");
    assert!(!http.retryable_gateway_transport_failure());
}

#[test]
fn duplicate_partial_completions_have_the_same_idempotency_identity() {
    let execution_id = Uuid::from_u128(0x50505050_5050_4050_8050_505050505050);
    let completion = CompletionRequest {
        status: "partial_result".into(),
        canonical_terminal_result_id: None,
        terminal_revision: None,
        terminal_authority: None,
        canonical_terminal_result: None,
        mission_outcome: Some(CompletionStatus::Partial),
        process_health: Some("healthy".into()),
        completion_evaluation: None,
        output_summary: Some("Continue implementation.".into()),
        failure_code: None,
        failure_message: None,
        head_branch: Some("rustgrid/continuation".into()),
        head_sha: Some("a".repeat(40)),
        pull_request_number: Some(17),
        pull_request_url: Some("https://github.com/RustGrid/example/pull/17".into()),
        final_callback: None,
    };
    let first = completion_idempotency_key(execution_id, &completion).unwrap();
    let second = completion_idempotency_key(execution_id, &completion).unwrap();
    assert_eq!(first, second);

    let mut changed = completion;
    changed.output_summary = Some("Different remaining work.".into());
    assert_ne!(
        first,
        completion_idempotency_key(execution_id, &changed).unwrap()
    );
}

#[test]
fn canonical_callback_idempotency_survives_mutable_projection_changes() {
    let execution_id = Uuid::from_u128(0x51515151_5151_4151_8151_515151515151);
    let terminal_id = Uuid::from_u128(0x52525252_5252_4252_8252_525252525252);
    let mut completion = CompletionRequest {
        status: "completed".into(),
        canonical_terminal_result_id: Some(terminal_id),
        terminal_revision: Some(3),
        terminal_authority: Some("worker_domain".into()),
        canonical_terminal_result: Some(json!({"terminal_result_id": terminal_id})),
        mission_outcome: Some(CompletionStatus::Complete),
        process_health: Some("healthy".into()),
        completion_evaluation: None,
        output_summary: Some("First transport attempt.".into()),
        failure_code: None,
        failure_message: None,
        head_branch: Some("rustgrid/canonical".into()),
        head_sha: Some("a".repeat(40)),
        pull_request_number: Some(19),
        pull_request_url: Some("https://github.com/RustGrid/example/pull/19".into()),
        final_callback: None,
    };
    let first = completion_idempotency_key(execution_id, &completion).unwrap();
    completion.output_summary = Some("Retry after an accepted timeout.".into());
    completion.process_health = Some("degraded".into());
    assert_eq!(
        first,
        completion_idempotency_key(execution_id, &completion).unwrap()
    );
    assert_eq!(
        first,
        terminal_callback_idempotency_key(execution_id, terminal_id, 3)
    );
}

#[test]
fn github_repository_token_request_is_bodyless_and_scope_checked() {
    let execution_id = Uuid::from_u128(46);
    let Some((base, request, server)) = one_request_server(
        "200 OK",
        json!({
            "token": "installation-token",
            "expires_at": "2099-01-01T00:00:00Z",
            "permissions": {"contents": "write", "pull_requests": "write"},
            "repository": "RustGrid/example"
        }),
    ) else {
        return;
    };
    let client = test_api_client(base, execution_id);
    let token = client.github_token("rustgrid/EXAMPLE").unwrap();
    server.join().unwrap();
    assert_eq!(token.expose(), "installation-token");
    let request = request.recv().unwrap();
    assert!(request.starts_with(&format!(
        "POST /api/v1/executions/{execution_id}/github-token HTTP/1.1"
    )));
    assert!(request.ends_with("\r\n\r\n"));
    assert!(!request.contains("{}"));
}

#[test]
fn github_repository_token_must_have_a_safe_remaining_lifetime() {
    let execution_id = Uuid::from_u128(47);
    let Some((base, _, server)) = one_request_server(
        "200 OK",
        json!({
            "token": "already-expired-installation-token",
            "expires_at": "2000-01-01T00:00:00Z",
            "permissions": {"contents": "write", "pull_requests": "write"},
            "repository": "RustGrid/example"
        }),
    ) else {
        return;
    };
    let client = test_api_client(base, execution_id);
    assert!(client.github_token("RustGrid/example").is_err());
    server.join().unwrap();
}

#[derive(Default)]
struct FakeGitHubPublisher {
    finds: std::sync::atomic::AtomicUsize,
    creates: std::sync::atomic::AtomicUsize,
    updates: std::sync::atomic::AtomicUsize,
}

impl GitHubPublisher for FakeGitHubPublisher {
    type Error = anyhow::Error;

    fn find_open_pull_request(
        &self,
        _repo: &RepoConfig,
        _branch: &str,
    ) -> Result<Option<crate::github::PullRequest>> {
        self.finds.fetch_add(1, Ordering::SeqCst);
        Ok(Some(crate::github::PullRequest {
            number: 17,
            html_url: "https://github.com/RustGrid/example/pull/17".into(),
            node_id: Some("PR_node".into()),
            draft: true,
            body: None,
        }))
    }

    fn update_pull_request(
        &self,
        _repo: &RepoConfig,
        number: u64,
        _title: &str,
        body: &str,
    ) -> Result<crate::github::PullRequest> {
        self.updates.fetch_add(1, Ordering::SeqCst);
        Ok(crate::github::PullRequest {
            number,
            html_url: format!("https://github.com/RustGrid/example/pull/{number}"),
            node_id: Some("PR_node".into()),
            draft: true,
            body: Some(body.into()),
        })
    }

    fn create_pull_request(
        &self,
        _repo: &RepoConfig,
        _title: &str,
        _body: &str,
        _head: &str,
        _base: &str,
        _draft: bool,
    ) -> Result<crate::github::PullRequest> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("duplicate publication must not create another pull request")
    }

    fn set_draft(&self, _node_id: &str, _draft: bool) -> Result<()> {
        Ok(())
    }
}

#[test]
fn duplicate_publication_requests_reconcile_the_existing_pull_request() {
    let manifest = test_manifest(Uuid::from_u128(0x71717171_7171_4171_8171_717171717171));
    let repo = manifest.repo_config().unwrap();
    let publisher = FakeGitHubPublisher::default();

    let pull = find_or_create_hosted_pull_request(
        &publisher,
        &repo,
        &manifest,
        &[],
        &test_completion_evaluation(CompletionStatus::Partial),
        true,
        true,
    )
    .unwrap();

    assert_eq!(pull.number, 17);
    assert_eq!(publisher.finds.load(Ordering::SeqCst), 1);
    assert_eq!(publisher.updates.load(Ordering::SeqCst), 1);
    assert_eq!(publisher.creates.load(Ordering::SeqCst), 0);
}

struct MovedRepository;

impl RepositoryPublisher for MovedRepository {
    type Error = anyhow::Error;

    fn reconcile_remote_branch(
        &self,
        branch: &str,
        _commit: &str,
    ) -> Result<crate::git::ReconciledCommit> {
        Err(crate::git::RemoteBranchMoved::new(branch).into())
    }

    fn push(&self, _branch: &str, _commit: &str) -> Result<()> {
        Ok(())
    }
}

#[test]
fn remote_branch_movement_remains_a_typed_publication_failure() {
    let error = reconcile_publication_repository(&MovedRepository, "rustgrid/rg-1", "abc123")
        .expect_err("remote movement must stop this publication attempt");

    assert!(error.downcast_ref::<RemoteBranchMoved>().is_some());
}

#[derive(Debug)]
struct ProviderExhausted;

impl std::fmt::Display for ProviderExhausted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("scripted provider responses exhausted")
    }
}

impl std::error::Error for ProviderExhausted {}

struct ExhaustedModelProvider;

impl ModelProvider for ExhaustedModelProvider {
    type Error = ProviderExhausted;

    fn invoke(
        &self,
        _request: Value,
        _registration: &AiCallRegistration,
        _execution_deadline: Option<Instant>,
    ) -> std::result::Result<Value, Self::Error> {
        Err(ProviderExhausted)
    }
}

#[test]
fn model_provider_exhaustion_is_testable_without_gateway_transport() {
    let error = invoke_model(
        &ExhaustedModelProvider,
        json!({"input": "bounded"}),
        &ai_call_registration(
            Uuid::from_u128(0x72727272_7272_4272_8272_727272727272),
            1,
            Uuid::from_u128(0x73737373_7373_4373_8373_737373737373),
            0,
            ExecutionPhase::Discovery,
            0,
        ),
        None,
    )
    .expect_err("the fake provider has no response remaining");

    assert!(error.downcast_ref::<ProviderExhausted>().is_some());
}
