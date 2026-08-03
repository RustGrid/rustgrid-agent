# Hosted production symbol inventory

Generated from the refactored source layout. Test-only items and generated impact-map types are intentionally excluded; generated types remain owned by `impact_map_types_generated.rs`.

| Module | Line | Kind | Symbol | Visibility |
| --- | ---: | --- | --- | --- |
| `src/hosted/authentication.rs` | 4 | fn | `request_github_oidc` | `pub(super)` |
| `src/hosted/authentication.rs` | 25 | fn | `exchange_github_oidc` | `pub(super)` |
| `src/hosted/contracts.rs` | 5 | struct | `GithubTokenResponse` | `pub(super)` |
| `src/hosted/contracts.rs` | 13 | struct | `HostedManifest` | `pub(super)` |
| `src/hosted/contracts.rs` | 48 | struct | `ManifestExecution` | `pub(super)` |
| `src/hosted/contracts.rs` | 69 | struct | `ManifestGithubActionsExecution` | `pub(super)` |
| `src/hosted/contracts.rs` | 75 | struct | `ManifestRun` | `pub(super)` |
| `src/hosted/contracts.rs` | 85 | struct | `HostedGithubManifest` | `pub(super)` |
| `src/hosted/contracts.rs` | 98 | struct | `HostedAiManifest` | `pub(super)` |
| `src/hosted/contracts.rs` | 107 | fn | `deserialize_present_nullable` | `pub(super)` |
| `src/hosted/contracts.rs` | 118 | enum | `BudgetSource` | `pub(super)` |
| `src/hosted/contracts.rs` | 125 | struct | `BudgetAudit` | `pub(super)` |
| `src/hosted/contracts.rs` | 136 | struct | `ExecutionBudgetMismatch` | `pub(super)` |
| `src/hosted/contracts.rs` | 145 | fn | `fmt` | `private` |
| `src/hosted/contracts.rs` | 157 | struct | `HostedProviderContractFailure` | `pub(super)` |
| `src/hosted/contracts.rs` | 163 | fn | `from_validation` | `pub(super)` |
| `src/hosted/contracts.rs` | 183 | fn | `fmt` | `private` |
| `src/hosted/contracts.rs` | 191 | struct | `HostedExecutionPolicy` | `pub(super)` |
| `src/hosted/contracts.rs` | 201 | struct | `ProjectVerificationPolicy` | `pub(super)` |
| `src/hosted/contracts.rs` | 207 | fn | `default` | `private` |
| `src/hosted/contracts.rs` | 216 | struct | `HostedCodexPolicy` | `pub(super)` |
| `src/hosted/contracts.rs` | 222 | struct | `HostedQualityGate` | `pub(super)` |
| `src/hosted/contracts.rs` | 230 | struct | `HostedSandboxPolicy` | `pub(super)` |
| `src/hosted/contracts.rs` | 238 | struct | `CompletionRequest` | `pub(super)` |
| `src/hosted/contracts.rs` | 263 | struct | `HostedResult` | `pub(super)` |
| `src/hosted/contracts.rs` | 274 | struct | `TerminalTelemetry` | `pub(super)` |
| `src/hosted/contracts.rs` | 290 | struct | `PullRequestResult` | `pub(super)` |
| `src/hosted/contracts.rs` | 296 | struct | `ValidationResult` | `pub(super)` |
| `src/hosted/contracts.rs` | 305 | enum | `CompletionStatus` | `pub(super)` |
| `src/hosted/contracts.rs` | 315 | fn | `as_str` | `pub(super)` |
| `src/hosted/contracts.rs` | 329 | enum | `ImplementationCompleteness` | `pub(super)` |
| `src/hosted/contracts.rs` | 336 | fn | `as_str` | `pub(super)` |
| `src/hosted/contracts.rs` | 347 | enum | `VerificationReadiness` | `pub(super)` |
| `src/hosted/contracts.rs` | 355 | fn | `as_str` | `pub(super)` |
| `src/hosted/contracts.rs` | 367 | enum | `EvaluationSource` | `pub(super)` |
| `src/hosted/contracts.rs` | 374 | fn | `as_str` | `pub(super)` |
| `src/hosted/contracts.rs` | 385 | enum | `VerificationType` | `pub(super)` |
| `src/hosted/contracts.rs` | 396 | fn | `requires_external_review` | `pub(super)` |
| `src/hosted/contracts.rs` | 407 | fn | `as_str` | `pub(super)` |
| `src/hosted/contracts.rs` | 422 | enum | `CriterionStatus` | `pub(super)` |
| `src/hosted/contracts.rs` | 432 | fn | `as_str` | `pub(super)` |
| `src/hosted/contracts.rs` | 445 | struct | `CompletionEvidence` | `pub(super)` |
| `src/hosted/contracts.rs` | 451 | struct | `CriterionEvaluation` | `pub(super)` |
| `src/hosted/contracts.rs` | 467 | struct | `ReviewChecklistItem` | `pub(super)` |
| `src/hosted/contracts.rs` | 474 | struct | `CompletionEvaluation` | `pub(super)` |
| `src/hosted/contracts.rs` | 498 | struct | `ImplementationPlan` | `pub(super)` |
| `src/hosted/contracts.rs` | 513 | struct | `PlannedChange` | `pub(super)` |
| `src/hosted/contracts.rs` | 538 | struct | `PlannedTarget` | `pub(super)` |
| `src/hosted/contracts.rs` | 550 | enum | `PlannedTargetInput` | `pub(super)` |
| `src/hosted/contracts.rs` | 555 | fn | `deserialize_planned_targets` | `pub(super)` |
| `src/hosted/contracts.rs` | 577 | struct | `ImplementationDeclaration` | `pub(super)` |
| `src/hosted/contracts.rs` | 592 | struct | `ImplementationCriterionEvidence` | `pub(super)` |
| `src/hosted/contracts.rs` | 600 | struct | `ToolFailureRecord` | `pub(super)` |
| `src/hosted/contracts.rs` | 623 | enum | `FailureReconciliation` | `pub(super)` |
| `src/hosted/contracts.rs` | 632 | struct | `IntendedChangeRecovery` | `pub(super)` |
| `src/hosted/contracts.rs` | 641 | enum | `IntendedChangeStatus` | `pub(super)` |
| `src/hosted/contracts.rs` | 653 | enum | `WriteAttemptStatus` | `pub(super)` |
| `src/hosted/contracts.rs` | 660 | struct | `WriteAttemptRecord` | `pub(super)` |
| `src/hosted/contracts.rs` | 680 | struct | `MutationPreflightRecord` | `pub(super)` |
| `src/hosted/contracts.rs` | 696 | struct | `ImplementationPlanRepair` | `pub(super)` |
| `src/hosted/contracts.rs` | 707 | struct | `MutationPreflightDecision` | `pub(super)` |
| `src/hosted/contracts.rs` | 712 | fn | `one_u32` | `pub(super)` |
| `src/hosted/contracts.rs` | 717 | struct | `MutationPreflightError` | `pub(super)` |
| `src/hosted/contracts.rs` | 726 | fn | `fmt` | `private` |
| `src/hosted/contracts.rs` | 734 | struct | `IntendedChangeRecord` | `pub(super)` |
| `src/hosted/contracts.rs` | 750 | enum | `ArtifactSemanticStatus` | `pub(super)` |
| `src/hosted/contracts.rs` | 760 | enum | `ArtifactSerializationStatus` | `pub(super)` |
| `src/hosted/contracts.rs` | 769 | enum | `ArtifactFailureLayer` | `pub(super)` |
| `src/hosted/contracts.rs` | 779 | enum | `ArtifactPersistenceStatus` | `pub(super)` |
| `src/hosted/contracts.rs` | 787 | struct | `ArtifactCheckpoint` | `pub(super)` |
| `src/hosted/contracts.rs` | 814 | fn | `default` | `private` |
| `src/hosted/contracts.rs` | 835 | struct | `ImpactMapFailure` | `pub(super)` |
| `src/hosted/contracts.rs` | 845 | struct | `ImplementationOutcome` | `pub(super)` |
| `src/hosted/control_plane.rs` | 5 | struct | `GithubOidcResponse` | `pub(super)` |
| `src/hosted/control_plane.rs` | 10 | struct | `ExchangeResponse` | `pub(super)` |
| `src/hosted/control_plane.rs` | 28 | struct | `RefreshedTokenResponse` | `pub(super)` |
| `src/hosted/control_plane.rs` | 36 | struct | `TokenState` | `pub(super)` |
| `src/hosted/control_plane.rs` | 45 | struct | `HostedApiClient` | `pub(super)` |
| `src/hosted/control_plane.rs` | 58 | struct | `ProviderErrorDiagnostic` | `pub(super)` |
| `src/hosted/control_plane.rs` | 70 | enum | `AiFailureClass` | `pub(super)` |
| `src/hosted/control_plane.rs` | 83 | fn | `is_provider_failure` | `pub(super)` |
| `src/hosted/control_plane.rs` | 97 | enum | `AiBudgetDisposition` | `pub(super)` |
| `src/hosted/control_plane.rs` | 104 | struct | `HostedHttpError` | `pub(super)` |
| `src/hosted/control_plane.rs` | 131 | fn | `invalidates_execution` | `pub(super)` |
| `src/hosted/control_plane.rs` | 146 | fn | `effective_code` | `pub(super)` |
| `src/hosted/control_plane.rs` | 166 | fn | `failure_stage` | `pub(super)` |
| `src/hosted/control_plane.rs` | 174 | fn | `provider_contacted` | `pub(super)` |
| `src/hosted/control_plane.rs` | 178 | fn | `call_budget_consumed` | `pub(super)` |
| `src/hosted/control_plane.rs` | 182 | fn | `reservation_state` | `pub(super)` |
| `src/hosted/control_plane.rs` | 188 | fn | `reservation_reconciliation_state` | `pub(super)` |
| `src/hosted/control_plane.rs` | 192 | fn | `has_definite_provider_response` | `pub(super)` |
| `src/hosted/control_plane.rs` | 196 | fn | `failure_class` | `pub(super)` |
| `src/hosted/control_plane.rs` | 248 | fn | `rustgrid_gateway_status` | `pub(super)` |
| `src/hosted/control_plane.rs` | 257 | fn | `terminal_message` | `pub(super)` |
| `src/hosted/control_plane.rs` | 287 | fn | `recommended_action` | `pub(super)` |
| `src/hosted/control_plane.rs` | 305 | fn | `budget_disposition` | `pub(super)` |
| `src/hosted/control_plane.rs` | 348 | fn | `retryable_gateway_transport_failure` | `pub(super)` |
| `src/hosted/control_plane.rs` | 354 | fn | `retryable_registration_failure` | `pub(super)` |
| `src/hosted/control_plane.rs` | 378 | fn | `fmt` | `private` |
| `src/hosted/control_plane.rs` | 396 | fn | `from_exchange` | `pub(super)` |
| `src/hosted/control_plane.rs` | 450 | fn | `claim` | `pub(super)` |
| `src/hosted/control_plane.rs` | 460 | fn | `manifest` | `pub(super)` |
| `src/hosted/control_plane.rs` | 470 | fn | `heartbeat` | `pub(super)` |
| `src/hosted/control_plane.rs` | 481 | fn | `append_event` | `pub(super)` |
| `src/hosted/control_plane.rs` | 510 | fn | `update_state` | `pub(super)` |
| `src/hosted/control_plane.rs` | 524 | fn | `github_token` | `pub(super)` |
| `src/hosted/control_plane.rs` | 565 | fn | `ai_response_until` | `pub(super)` |
| `src/hosted/control_plane.rs` | 625 | fn | `telemetry` | `pub(super)` |
| `src/hosted/control_plane.rs` | 637 | fn | `complete` | `pub(super)` |
| `src/hosted/control_plane.rs` | 649 | fn | `ensure_fresh` | `pub(super)` |
| `src/hosted/control_plane.rs` | 663 | fn | `refresh_token` | `pub(super)` |
| `src/hosted/control_plane.rs` | 727 | fn | `current_token` | `pub(super)` |
| `src/hosted/control_plane.rs` | 739 | fn | `session_id` | `pub(super)` |
| `src/hosted/control_plane.rs` | 747 | fn | `send_json` | `pub(super)` |
| `src/hosted/control_plane.rs` | 767 | fn | `send_with_token` | `pub(super)` |
| `src/hosted/control_plane.rs` | 806 | fn | `completion_idempotency_key` | `pub(super)` |
| `src/hosted/control_plane.rs` | 821 | struct | `AiCallRegistration` | `pub(super)` |
| `src/hosted/control_plane.rs` | 829 | fn | `ai_call_registration` | `pub(super)` |
| `src/hosted/control_plane.rs` | 865 | fn | `budget_audit` | `pub(super)` |
| `src/hosted/control_plane.rs` | 926 | fn | `validate` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1095 | fn | `repo_config` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1109 | fn | `validate` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1158 | fn | `child_environment_allowlist` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1168 | fn | `hosted_http_client` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1182 | fn | `decode_response` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1278 | fn | `hosted_error_field` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1286 | fn | `optional_hosted_http_status` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1301 | fn | `safe_hosted_error_identifier` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1309 | fn | `safe_hosted_error_text` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1318 | fn | `safe_provider_error` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1349 | fn | `safe_provider_response_body` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1366 | fn | `decode_success` | `pub(super)` |
| `src/hosted/control_plane.rs` | 1374 | fn | `read_bounded_response` | `pub(super)` |
| `src/hosted/environment.rs` | 5 | struct | `SecretString` | `pub(super)` |
| `src/hosted/environment.rs` | 8 | fn | `new` | `pub(super)` |
| `src/hosted/environment.rs` | 15 | fn | `expose` | `pub(super)` |
| `src/hosted/environment.rs` | 21 | fn | `fmt` | `private` |
| `src/hosted/environment.rs` | 27 | fn | `drop` | `private` |
| `src/hosted/environment.rs` | 32 | struct | `GithubActionsEnvironment` | `pub(super)` |
| `src/hosted/environment.rs` | 47 | struct | `GithubActionsAuthor` | `pub(super)` |
| `src/hosted/environment.rs` | 53 | fn | `load` | `pub(super)` |
| `src/hosted/environment.rs` | 114 | fn | `require_execute_context` | `pub(super)` |
| `src/hosted/environment.rs` | 129 | fn | `git_author` | `pub(super)` |
| `src/hosted/environment.rs` | 146 | fn | `normalize_api_root` | `pub(super)` |
| `src/hosted/environment.rs` | 163 | fn | `secure_url` | `pub(super)` |
| `src/hosted/environment.rs` | 177 | fn | `secure_github_oidc_url` | `pub(super)` |
| `src/hosted/environment.rs` | 193 | fn | `api_origin` | `pub(super)` |
| `src/hosted/environment.rs` | 201 | fn | `validate_manifest_endpoint` | `pub(super)` |
| `src/hosted/environment.rs` | 224 | fn | `required_env` | `pub(super)` |
| `src/hosted/environment.rs` | 232 | fn | `harden_hosted_process` | `pub(super)` |
| `src/hosted/environment.rs` | 245 | fn | `harden_hosted_process` | `pub(super)` |
| `src/hosted/environment.rs` | 249 | fn | `optional_env` | `pub(super)` |
| `src/hosted/environment.rs` | 253 | fn | `valid_github_actor` | `pub(super)` |
| `src/hosted/environment.rs` | 264 | fn | `reject_inherited_provider_credentials` | `pub(super)` |
| `src/hosted/environment.rs` | 282 | fn | `validate_dispatch_nonce` | `pub(super)` |
| `src/hosted/environment.rs` | 294 | fn | `validate_github_oidc_token` | `pub(super)` |
| `src/hosted/environment.rs` | 307 | fn | `validate_execution_token` | `pub(super)` |
| `src/hosted/environment.rs` | 319 | fn | `retryable_status` | `pub(super)` |
| `src/hosted/environment.rs` | 326 | fn | `retry_delay` | `pub(super)` |
| `src/hosted/environment.rs` | 330 | fn | `ai_request_timeout` | `pub(super)` |
| `src/hosted/environment.rs` | 343 | fn | `sleep_before_execution_retry` | `pub(super)` |
| `src/hosted/environment.rs` | 361 | fn | `sleep_before_ai_retry` | `pub(super)` |
| `src/hosted/environment.rs` | 368 | fn | `registration_retry_delay` | `pub(super)` |
| `src/hosted/environment.rs` | 379 | fn | `token_refresh_after` | `pub(super)` |
| `src/hosted/environment.rs` | 385 | fn | `safe_identifier` | `pub(super)` |
| `src/hosted/environment.rs` | 393 | fn | `safe_child_environment_name` | `pub(super)` |
| `src/hosted/environment.rs` | 433 | fn | `normalized_base_ref` | `pub(super)` |
| `src/hosted/environment.rs` | 441 | fn | `safe_git_ref` | `pub(super)` |
| `src/hosted/environment.rs` | 456 | fn | `commit_sha` | `pub(super)` |
| `src/hosted/environment.rs` | 460 | fn | `ensure_running` | `pub(super)` |
| `src/hosted/environment.rs` | 467 | fn | `hosted_execution_deadline` | `pub(super)` |
| `src/hosted/errors.rs` | 5 | fn | `emit_guardrail` | `pub(super)` |
| `src/hosted/errors.rs` | 20 | fn | `emit_phase_budget_warning` | `pub(super)` |
| `src/hosted/errors.rs` | 35 | fn | `execution_failure` | `pub(super)` |
| `src/hosted/errors.rs` | 57 | fn | `categorized_execution_failure` | `pub(super)` |
| `src/hosted/errors.rs` | 158 | fn | `implementation_preparation_failure` | `pub(super)` |
| `src/hosted/errors.rs` | 174 | fn | `blocked_no_diff_failure` | `pub(super)` |
| `src/hosted/errors.rs` | 193 | fn | `infrastructure_stop_failure` | `pub(super)` |
| `src/hosted/errors.rs` | 210 | fn | `impact_map_execution_failure` | `pub(super)` |
| `src/hosted/errors.rs` | 287 | fn | `emit_mutation_no_progress_diagnostics` | `pub(super)` |
| `src/hosted/execution/completion.rs` | 5 | fn | `evaluate_completion` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 268 | fn | `record_completion_evaluated` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 314 | fn | `completion_evaluator_instructions` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 324 | type | `is` | `private` |
| `src/hosted/execution/completion.rs` | 336 | fn | `response_message_text` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 354 | fn | `parse_completion_evaluation` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 365 | fn | `validate_completion_evaluation` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 466 | fn | `completion_fallback` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 675 | fn | `reconcile_model_completion_evaluation` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 715 | fn | `finalize_completion_dimensions` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 813 | fn | `verification_type_for_criterion` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 850 | fn | `browser_e2e_is_mandatory_and_missing` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 875 | fn | `classify_remaining_work` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 895 | fn | `completion_review_diff` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 932 | fn | `completion_changed_paths` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 962 | fn | `append_fingerprint_field` | `pub(in crate::hosted)` |
| `src/hosted/execution/completion.rs` | 973 | fn | `repository_state_fingerprint` | `pub(in crate::hosted)` |
| `src/hosted/execution/diff_review.rs` | 5 | fn | `deterministic_diff_review` | `pub(in crate::hosted)` |
| `src/hosted/execution/discovery.rs` | 4 | fn | `validate_impact_map` | `pub(in crate::hosted)` |
| `src/hosted/execution/discovery.rs` | 21 | fn | `impact_map_from_value` | `pub(in crate::hosted)` |
| `src/hosted/execution/discovery.rs` | 41 | fn | `json_object_from_text` | `pub(in crate::hosted)` |
| `src/hosted/execution/discovery.rs` | 67 | fn | `recover_impact_map` | `pub(in crate::hosted)` |
| `src/hosted/execution/discovery.rs` | 95 | fn | `impact_map_sha256` | `pub(in crate::hosted)` |
| `src/hosted/execution/discovery.rs` | 101 | fn | `classify_impact_map_failure` | `pub(in crate::hosted)` |
| `src/hosted/execution/discovery.rs` | 129 | fn | `invalid_impact_map_semantic_status` | `pub(in crate::hosted)` |
| `src/hosted/execution/discovery.rs` | 161 | fn | `accept_impact_map` | `pub(in crate::hosted)` |
| `src/hosted/execution/discovery.rs` | 266 | fn | `accept_deterministic_impact_map_if_available` | `pub(in crate::hosted)` |
| `src/hosted/execution/implementation.rs` | 5 | fn | `reconcile_write_failures` | `pub(in crate::hosted)` |
| `src/hosted/execution/implementation.rs` | 64 | fn | `preflight_source_mutation` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 6 | fn | `new` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 288 | fn | `implement` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 322 | fn | `budget_telemetry` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 382 | fn | `append_event_recoverable` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 397 | fn | `ensure_active_or_checkpoint_cancellation` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 574 | fn | `record_tool_progress` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 603 | fn | `observe_implementation_progress` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 669 | fn | `reconcile_wall_clock_boundary` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 759 | fn | `record_partial_reviewable_handoff` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 795 | fn | `record_cache_observability` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 811 | fn | `reserve_graph_model_call` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 881 | fn | `observe_model_cost` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 936 | fn | `observe_failed_model_cost` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 977 | fn | `notebook_checkpoint_metadata` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 995 | fn | `apply_execution_decision` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1124 | fn | `record_decision_domain_event` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1242 | fn | `next_domain_event_sequence` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1250 | fn | `initialize_fresh_execution_snapshot` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1322 | fn | `checkpoint_notebook` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1362 | fn | `persist_orchestration_checkpoint` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1388 | fn | `ordered_implementation_targets` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1392 | fn | `current_implementation_target` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1439 | fn | `implementation_start_context` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1520 | fn | `reconcile_authoritative_target_state` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1561 | fn | `reconcile_repository_failure_supersession` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1631 | fn | `build_execution_snapshot` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1705 | fn | `reconcile_execution_and_apply` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1755 | fn | `finalize_guardrail_outcome` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1780 | fn | `peek_execution_decision` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1787 | fn | `restored_validation_results` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1794 | fn | `reconstruct_implementation_outcome` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1839 | fn | `restored_completion_evaluation` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1875 | fn | `finalization_requires_revalidation` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1887 | fn | `append_execution_domain_event` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1902 | fn | `graph_node_id` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1915 | fn | `record_discovery_completed` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1964 | fn | `record_discovery_failure` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 1987 | fn | `record_planning_failure` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 2022 | fn | `record_planning_failures_recovered` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 2047 | fn | `record_active_target_failure` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 2089 | fn | `record_validation_failures` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 2175 | fn | `record_active_target_applied` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 2248 | fn | `prepare_active_target_context` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 2299 | fn | `record_active_target_mutation_produced` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 2339 | fn | `verify_active_target_state` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 2384 | fn | `reconcile_active_phase` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 2410 | fn | `invalidate_finalization_after_remote_reconciliation` | `pub(in crate::hosted)` |
| `src/hosted/execution/orchestration.rs` | 2465 | fn | `complete_finalization_revalidation` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 4 | fn | `reconcile_notebook_orchestration` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 206 | fn | `attempt_modified_target` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 210 | fn | `deterministic_complete_declaration` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 289 | fn | `deterministic_partial_declaration` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 347 | fn | `deterministic_change_id` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 363 | fn | `normalized_planned_paths` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 381 | fn | `normalize_planned_changes` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 447 | fn | `recover_planning_repair_state` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 516 | fn | `merge_preserved_plan_fragments` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 571 | struct | `PlanCriterionAssignment` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 577 | struct | `ImplementationPlanAcceptance` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 584 | fn | `semantic_tokens` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 587 | const | `STOP_WORDS` | `private` |
| `src/hosted/execution/planning.rs` | 617 | fn | `planned_change_criterion_relevance` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 663 | fn | `canonicalize_plan_criterion_ids` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 698 | fn | `validate_plan_criterion_coverage` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 741 | fn | `validate_and_repair_plan_criteria` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 803 | fn | `deterministic_plan_from_impact_map` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 916 | fn | `repair_implementation_plan` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 962 | fn | `record_mutation_preflight_rejection` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 1005 | fn | `validate_planned_change_paths` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 1036 | fn | `authorize_planned_target` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 1067 | fn | `roll_up_target_statuses` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 1124 | fn | `intended_changes_from_plan` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 1141 | fn | `normalize_notebook_intended_changes` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 1220 | fn | `validate_write_repair_strategy` | `pub(in crate::hosted)` |
| `src/hosted/execution/planning.rs` | 1263 | fn | `accept_deterministic_implementation_plan_if_available` | `pub(in crate::hosted)` |
| `src/hosted/execution/validation.rs` | 5 | fn | `checkpoint_validation_ledger` | `pub(in crate::hosted)` |
| `src/hosted/execution/validation.rs` | 19 | fn | `bootstrap_hosted_dependencies` | `pub(in crate::hosted)` |
| `src/hosted/execution/validation.rs` | 94 | fn | `hosted_dependency_bootstrap` | `pub(in crate::hosted)` |
| `src/hosted/execution/validation.rs` | 124 | fn | `run_quality_gates` | `pub(in crate::hosted)` |
| `src/hosted/execution/validation.rs` | 207 | fn | `record_validation_observability` | `pub(in crate::hosted)` |
| `src/hosted/execution/validation.rs` | 218 | fn | `run_quality_gates_with_capture` | `pub(in crate::hosted)` |
| `src/hosted/execution/validation.rs` | 655 | fn | `classify_validation_gate` | `pub(in crate::hosted)` |
| `src/hosted/execution/validation.rs` | 674 | fn | `validation_timeout_policy` | `pub(in crate::hosted)` |
| `src/hosted/execution/validation.rs` | 695 | fn | `is_dependency_install_command` | `pub(in crate::hosted)` |
| `src/hosted/execution/validation.rs` | 712 | fn | `validation_gate_order_key` | `pub(in crate::hosted)` |
| `src/hosted/execution/validation.rs` | 746 | fn | `dependency_lock_fingerprint` | `pub(in crate::hosted)` |
| `src/hosted/execution/validation.rs` | 766 | fn | `relevant_environment_fingerprint` | `pub(in crate::hosted)` |
| `src/hosted/graph_bridge.rs` | 43 | struct | `HostedOrchestrationCheckpoint` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 69 | enum | `HostedResumeReason` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 74 | fn | `completed_change_ids` | `private` |
| `src/hosted/graph_bridge.rs` | 93 | struct | `MaterializedLegacyChange` | `private` |
| `src/hosted/graph_bridge.rs` | 100 | fn | `materialize_legacy_changes` | `private` |
| `src/hosted/graph_bridge.rs` | 171 | fn | `remaining_work_item` | `private` |
| `src/hosted/graph_bridge.rs` | 238 | fn | `materialize_failed_changes` | `private` |
| `src/hosted/graph_bridge.rs` | 284 | fn | `materialize_validation_evidence` | `private` |
| `src/hosted/graph_bridge.rs` | 316 | fn | `materialize_validation_failures` | `private` |
| `src/hosted/graph_bridge.rs` | 324 | fn | `materialize_required_gates` | `private` |
| `src/hosted/graph_bridge.rs` | 359 | fn | `last_successful_domain_action` | `private` |
| `src/hosted/graph_bridge.rs` | 390 | fn | `failure_category_code` | `private` |
| `src/hosted/graph_bridge.rs` | 407 | struct | `HostedReconciliationFacts` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 417 | fn | `bootstrap` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 439 | fn | `normalize_pre_plan_classification` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 468 | fn | `rebuild_from_plan` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 527 | fn | `ensure_graph_from_plan` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 561 | fn | `reconcile_plan_topology` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 605 | fn | `import_legacy_state_once` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 619 | fn | `legacy_import_pending` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 627 | fn | `resume_for_new_attempt` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 658 | fn | `import_legacy_state` | `private` |
| `src/hosted/graph_bridge.rs` | 786 | fn | `snapshot` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 806 | fn | `replace_from_snapshot` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 817 | fn | `hosted_stage` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 823 | fn | `execution_phase` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 868 | fn | `materialize_legacy_notebook` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 911 | fn | `synchronize_failures` | `private` |
| `src/hosted/graph_bridge.rs` | 986 | fn | `synchronize_validation` | `private` |
| `src/hosted/graph_bridge.rs` | 1071 | fn | `synchronize_review_and_publication` | `private` |
| `src/hosted/graph_bridge.rs` | 1180 | fn | `graph_matches_plan_topology` | `private` |
| `src/hosted/graph_bridge.rs` | 1203 | fn | `graph_topology_signature` | `private` |
| `src/hosted/graph_bridge.rs` | 1236 | fn | `preserve_pre_plan_graph_progress` | `private` |
| `src/hosted/graph_bridge.rs` | 1256 | fn | `preserve_unchanged_graph_progress` | `private` |
| `src/hosted/graph_bridge.rs` | 1333 | fn | `validation_gate_topology_signature` | `private` |
| `src/hosted/graph_bridge.rs` | 1343 | fn | `validation_gate_topology_matches` | `private` |
| `src/hosted/graph_bridge.rs` | 1357 | fn | `graph_validation_gate_type_label` | `private` |
| `src/hosted/graph_bridge.rs` | 1368 | fn | `retain_checkpoint_progress_for_nodes` | `private` |
| `src/hosted/graph_bridge.rs` | 1416 | fn | `node_semantic_identity` | `private` |
| `src/hosted/graph_bridge.rs` | 1431 | fn | `node_kind_label` | `private` |
| `src/hosted/graph_bridge.rs` | 1447 | fn | `canonical_plan_targets` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 1493 | fn | `canonical_validation_gates_for_targets` | `private` |
| `src/hosted/graph_bridge.rs` | 1546 | fn | `is_dependency_install_command` | `private` |
| `src/hosted/graph_bridge.rs` | 1563 | fn | `is_vitest_test_path` | `private` |
| `src/hosted/graph_bridge.rs` | 1579 | fn | `focused_gate_label` | `private` |
| `src/hosted/graph_bridge.rs` | 1593 | fn | `mission_outcome_from_completion` | `pub(super)` |
| `src/hosted/graph_bridge.rs` | 1606 | fn | `graph_id` | `private` |
| `src/hosted/graph_bridge.rs` | 1610 | fn | `next_event_sequence` | `private` |
| `src/hosted/graph_bridge.rs` | 1616 | fn | `complexity_assessment` | `private` |
| `src/hosted/graph_bridge.rs` | 1625 | fn | `provisional_complexity_assessment` | `private` |
| `src/hosted/graph_bridge.rs` | 1650 | fn | `manifest_budget_override` | `private` |
| `src/hosted/graph_bridge.rs` | 1679 | fn | `parse_usd_micros` | `private` |
| `src/hosted/graph_bridge.rs` | 1703 | fn | `complexity_input` | `private` |
| `src/hosted/graph_bridge.rs` | 1743 | fn | `count_paths` | `private` |
| `src/hosted/graph_bridge.rs` | 1753 | fn | `dependency_path` | `private` |
| `src/hosted/graph_bridge.rs` | 1762 | fn | `schema_path` | `private` |
| `src/hosted/graph_bridge.rs` | 1766 | fn | `security_path` | `private` |
| `src/hosted/graph_bridge.rs` | 1772 | fn | `integration_path` | `private` |
| `src/hosted/graph_bridge.rs` | 1778 | fn | `canonical_change_id` | `private` |
| `src/hosted/graph_bridge.rs` | 1787 | fn | `canonical_criterion_ids` | `private` |
| `src/hosted/graph_bridge.rs` | 1797 | fn | `infer_validation_gate_type` | `private` |
| `src/hosted/graph_bridge.rs` | 1814 | fn | `legacy_target_statuses` | `private` |
| `src/hosted/graph_bridge.rs` | 1843 | fn | `mutation_path_counts` | `private` |
| `src/hosted/graph_bridge.rs` | 1856 | fn | `authoritative_mutation_node_ids` | `private` |
| `src/hosted/graph_bridge.rs` | 1876 | fn | `graph_status_from_legacy` | `private` |
| `src/hosted/graph_bridge.rs` | 1892 | fn | `legacy_status_from_graph` | `private` |
| `src/hosted/graph_bridge.rs` | 1907 | fn | `aggregate_legacy_status` | `private` |
| `src/hosted/graph_bridge.rs` | 1942 | fn | `mutation_node_for_failure` | `private` |
| `src/hosted/graph_bridge.rs` | 1991 | fn | `stable_failure_id` | `private` |
| `src/hosted/graph_bridge.rs` | 2004 | fn | `graph_validation_status` | `private` |
| `src/hosted/graph_bridge.rs` | 2021 | fn | `legacy_validation_status` | `private` |
| `src/hosted/graph_bridge.rs` | 2032 | fn | `legacy_validation_status_from_node` | `private` |
| `src/hosted/graph_bridge.rs` | 2046 | fn | `legacy_validation_gate_type` | `private` |
| `src/hosted/graph_bridge.rs` | 2057 | fn | `graph_node_status_from_validation` | `private` |
| `src/hosted/graph_bridge.rs` | 2071 | fn | `validation_output_summary` | `private` |
| `src/hosted/graph_bridge.rs` | 2087 | fn | `legacy_gate_type` | `private` |
| `src/hosted/impact_map.rs` | 11 | const | `IMPACT_MAP_SCHEMA_VERSION` | `pub` |
| `src/hosted/impact_map.rs` | 12 | const | `IMPACT_MAP_SCHEMA_JSON` | `pub` |
| `src/hosted/impact_map.rs` | 15 | struct | `EvidenceReference` | `pub` |
| `src/hosted/impact_map.rs` | 25 | struct | `AcceptanceCriterion` | `pub` |
| `src/hosted/impact_map.rs` | 30 | fn | `acceptance_criteria` | `pub` |
| `src/hosted/impact_map.rs` | 42 | struct | `ValidationError` | `pub` |
| `src/hosted/impact_map.rs` | 49 | struct | `InvalidPayloadShape` | `pub` |
| `src/hosted/impact_map.rs` | 59 | enum | `ArtifactSource` | `pub` |
| `src/hosted/impact_map.rs` | 65 | fn | `schema` | `pub` |
| `src/hosted/impact_map.rs` | 69 | fn | `schema_sha256` | `pub` |
| `src/hosted/impact_map.rs` | 75 | fn | `provider_tool_schema` | `pub` |
| `src/hosted/impact_map.rs` | 99 | fn | `strip_provider_unsupported_keywords` | `private` |
| `src/hosted/impact_map.rs` | 117 | fn | `criterion_id` | `pub` |
| `src/hosted/impact_map.rs` | 121 | fn | `evidence_catalog` | `pub` |
| `src/hosted/impact_map.rs` | 148 | fn | `split_path_evidence_reference` | `private` |
| `src/hosted/impact_map.rs` | 165 | fn | `resolve_evidence_reference` | `private` |
| `src/hosted/impact_map.rs` | 183 | fn | `strings` | `private` |
| `src/hosted/impact_map.rs` | 195 | fn | `stable_area_id` | `private` |
| `src/hosted/impact_map.rs` | 203 | fn | `searches_from_notebook` | `private` |
| `src/hosted/impact_map.rs` | 213 | fn | `normalize` | `pub` |
| `src/hosted/impact_map.rs` | 357 | fn | `fallback` | `pub` |
| `src/hosted/impact_map.rs` | 397 | fn | `fallback_from_persisted_evidence` | `pub` |
| `src/hosted/impact_map.rs` | 420 | fn | `required` | `private` |
| `src/hosted/impact_map.rs` | 427 | fn | `min_items` | `private` |
| `src/hosted/impact_map.rs` | 435 | fn | `validate` | `pub` |
| `src/hosted/impact_map.rs` | 498 | fn | `safe_shape` | `pub` |
| `src/hosted/lifecycle.rs` | 11 | enum | `CanonicalExecutionState` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 30 | fn | `canonical_running_state` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 50 | enum | `ImplementationCompletionStatus` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 61 | enum | `ImplementationSubstate` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 71 | enum | `ToolProgressClass` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 81 | fn | `is_failure` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 87 | enum | `ImplementationProgressAction` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 92 | const | `FIRST_WRITE_DELAY_CALL` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 93 | const | `MAX_CONSECUTIVE_PREPARATION_READS` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 95 | fn | `implementation_progress_action` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 120 | struct | `RemainingWorkItem` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 130 | enum | `ValidationGateType` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 141 | enum | `ValidationStatus` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 157 | enum | `ValidationSource` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 164 | struct | `ValidationEvidence` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 184 | struct | `RequiredGate` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 193 | fn | `normalize_command` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 197 | fn | `validation_fingerprint` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 211 | fn | `passed_evidence` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 220 | fn | `supersede_stale_validation` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 235 | fn | `derive_remaining_work` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 259 | fn | `remaining_reason` | `private` |
| `src/hosted/lifecycle.rs` | 270 | fn | `implementation_completion_status` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 320 | enum | `ValidationEntryDecision` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 328 | fn | `validation_entry_decision` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 365 | fn | `legacy_remaining_work` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 372 | fn | `validate_lifecycle_invariants` | `pub(super)` |
| `src/hosted/lifecycle.rs` | 406 | fn | `new_running_evidence` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 5 | struct | `PartialRunContext` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 14 | enum | `StartupMode` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 21 | fn | `next_decision` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 31 | struct | `StartupModeResolution` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 38 | fn | `compatible_worker_notebook` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 52 | fn | `resolve_startup_mode` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 104 | struct | `ToolUsage` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 120 | struct | `ToolProgressRecord` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 135 | struct | `ImplementationReadProgress` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 142 | fn | `new_tool_progress_record` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 170 | fn | `implementation_read_progress` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 222 | fn | `unresolved_preparation_blockers` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 286 | struct | `PlanningRepairState` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 301 | struct | `ImplementationStartContext` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 327 | struct | `ImplementationTarget` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 338 | struct | `FinalizationRevalidation` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 344 | struct | `PersistedCompletionArtifact` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 358 | enum | `DependencyBootstrapStatus` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 363 | struct | `DependencyBootstrapEvidence` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 372 | struct | `WorkerNotebook` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 451 | fn | `canonical_finalization_state` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 496 | fn | `valid_completion_artifact` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 543 | fn | `notebook_finalization_requires_revalidation` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 603 | struct | `LocalizedDiscoveryCoverage` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 613 | fn | `localized_visual_goal` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 627 | fn | `localized_discovery_core_path` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 646 | fn | `localized_discovery_coverage` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 684 | fn | `localized_discovery_should_stop` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 693 | fn | `validate_localized_discovery_scope` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 733 | fn | `discovery_requested_paths` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 760 | fn | `record_centralized_discovery_finding` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 776 | struct | `UnderlyingFailure` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 784 | struct | `HostedStartupFailure` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 792 | fn | `fmt` | `private` |
| `src/hosted/lifecycle_state.rs` | 798 | fn | `source` | `private` |
| `src/hosted/lifecycle_state.rs` | 804 | struct | `HostedAgentExecutionFailure` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 880 | fn | `fmt` | `private` |
| `src/hosted/lifecycle_state.rs` | 887 | fn | `classify_implementation_preparation_failure` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 904 | fn | `blocked_result_event_payload` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 935 | fn | `blocked_completion_evaluation` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 970 | fn | `acceptance_criteria_from_ticket` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 1011 | fn | `project_verification_policy` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 1023 | fn | `impact_map_fallback_threshold` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 1033 | fn | `partial_pr_remaining_work` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 1053 | fn | `detect_partial_run` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 1080 | fn | `new_worker_notebook` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 1185 | fn | `notebook_orchestration_state` | `pub(super)` |
| `src/hosted/lifecycle_state.rs` | 1221 | fn | `implementation_plan_from_notebook` | `pub(super)` |
| `src/hosted/mod.rs` | 111 | const | `EXECUTION_LEASE_SECONDS` | `private` |
| `src/hosted/mod.rs` | 112 | const | `EXECUTION_TOKEN_TTL_SECONDS` | `private` |
| `src/hosted/mod.rs` | 113 | const | `TOKEN_REFRESH_MARGIN` | `private` |
| `src/hosted/mod.rs` | 114 | const | `HEARTBEAT_INTERVAL` | `private` |
| `src/hosted/mod.rs` | 115 | const | `MAX_HTTP_RESPONSE_BYTES` | `private` |
| `src/hosted/mod.rs` | 116 | const | `MAX_HTTP_ERROR_BYTES` | `private` |
| `src/hosted/mod.rs` | 117 | const | `MAX_PROVIDER_ERROR_MESSAGE_BYTES` | `private` |
| `src/hosted/mod.rs` | 118 | const | `MAX_PROVIDER_ERROR_PARAMETER_BYTES` | `private` |
| `src/hosted/mod.rs` | 119 | const | `MAX_PROVIDER_RESPONSE_BODY_BYTES` | `private` |
| `src/hosted/mod.rs` | 120 | const | `MAX_TOOL_OUTPUT_BYTES` | `private` |
| `src/hosted/mod.rs` | 121 | const | `MAX_DISCOVERY_REQUEST_BYTES` | `private` |
| `src/hosted/mod.rs` | 122 | const | `MAX_MODEL_FILE_BYTES` | `private` |
| `src/hosted/mod.rs` | 126 | const | `MAX_MODEL_CALLS_HARD_LIMIT` | `private` |
| `src/hosted/mod.rs` | 127 | const | `MAX_HOSTED_TURN_WINDOWS` | `private` |
| `src/hosted/mod.rs` | 128 | const | `MAX_REPAIR_ATTEMPTS` | `private` |
| `src/hosted/mod.rs` | 129 | const | `MAX_HOSTED_EXECUTION_DURATION` | `private` |
| `src/hosted/mod.rs` | 130 | const | `MAX_AI_REGISTRATION_ATTEMPTS` | `private` |
| `src/hosted/mod.rs` | 131 | const | `MAX_SMALL_FILE_REWRITE_BYTES` | `private` |
| `src/hosted/mod.rs` | 132 | const | `MAX_AMBIGUOUS_REPLACEMENT_FAILURES` | `private` |
| `src/hosted/mod.rs` | 133 | const | `MAX_TARGET_REPAIR_FAILURES` | `private` |
| `src/hosted/mod.rs` | 134 | const | `HOSTED_NAMESPACE` | `private` |
| `src/hosted/mod.rs` | 135 | const | `EXECUTION_PERMISSIONS` | `private` |
| `src/hosted/mod.rs` | 145 | fn | `execute_github_actions` | `pub` |
| `src/hosted/mod.rs` | 370 | fn | `report_successful_hosted_result` | `private` |
| `src/hosted/mod.rs` | 436 | fn | `hosted_result_can_succeed` | `private` |
| `src/hosted/mod.rs` | 447 | fn | `completion_request_status` | `private` |
| `src/hosted/mod.rs` | 458 | fn | `requires_implementation_continuation` | `private` |
| `src/hosted/mod.rs` | 468 | fn | `report_emergency_failure` | `pub` |
| `src/hosted/mod.rs` | 478 | fn | `report_emergency_failure_with_api` | `private` |
| `src/hosted/mod.rs` | 532 | struct | `HostedSupervisor` | `private` |
| `src/hosted/mod.rs` | 538 | enum | `HostedStopReason` | `private` |
| `src/hosted/mod.rs` | 544 | fn | `start` | `private` |
| `src/hosted/mod.rs` | 611 | fn | `stop` | `private` |
| `src/hosted/mod.rs` | 619 | fn | `run_hosted_execution` | `private` |
| `src/hosted/model_session.rs` | 4 | struct | `GatewayAgent` | `pub(super)` |
| `src/hosted/model_session.rs` | 45 | struct | `CostGuard` | `pub(super)` |
| `src/hosted/model_session.rs` | 57 | struct | `RequestCostEstimate` | `pub(super)` |
| `src/hosted/model_session.rs` | 65 | fn | `estimate_model_call_request_cost` | `pub(super)` |
| `src/hosted/model_session.rs` | 93 | fn | `model_call_admission_telemetry` | `pub(super)` |
| `src/hosted/model_session.rs` | 128 | enum | `HostedWallClockBoundary` | `pub(super)` |
| `src/hosted/model_session.rs` | 135 | fn | `as_str` | `pub(super)` |
| `src/hosted/model_session.rs` | 143 | fn | `is_publication` | `pub(super)` |
| `src/hosted/model_session.rs` | 152 | enum | `HostedWallClockAction` | `pub(super)` |
| `src/hosted/model_session.rs` | 160 | fn | `hosted_wall_clock_action` | `pub(super)` |
| `src/hosted/model_session.rs` | 216 | fn | `constrain_request_to_cost_limit` | `pub(super)` |
| `src/hosted/model_session.rs` | 247 | fn | `model_usage_for_accounting` | `pub(super)` |
| `src/hosted/model_session.rs` | 269 | fn | `failed_model_usage_for_accounting` | `pub(super)` |
| `src/hosted/model_session.rs` | 295 | struct | `CancellationResult` | `pub(super)` |
| `src/hosted/model_session.rs` | 307 | fn | `ordered_implementation_targets_from_notebook` | `pub(super)` |
| `src/hosted/model_session.rs` | 335 | fn | `implementation_start_context_from_notebook` | `pub(super)` |
| `src/hosted/model_session.rs` | 431 | fn | `has_unresolved_validation_failure` | `pub(super)` |
| `src/hosted/model_session.rs` | 437 | fn | `validate_current_target_scope` | `pub(super)` |
| `src/hosted/model_session.rs` | 465 | fn | `classify_hosted_mutation_preflight` | `pub(super)` |
| `src/hosted/model_session.rs` | 522 | fn | `mark_mutation_preflight_blocker` | `pub(super)` |
| `src/hosted/model_session.rs` | 535 | fn | `prepare_next_model_call` | `pub(super)` |
| `src/hosted/model_session.rs` | 822 | fn | `repair` | `pub(super)` |
| `src/hosted/model_session.rs` | 911 | fn | `run_session` | `pub(super)` |
| `src/hosted/orchestration.rs` | 8 | const | `DEFAULT_HOSTED_MODEL_CALLS` | `pub(super)` |
| `src/hosted/orchestration.rs` | 9 | const | `MINIMUM_HOSTED_MODEL_CALLS` | `pub(super)` |
| `src/hosted/orchestration.rs` | 13 | enum | `ExecutionPhase` | `pub(super)` |
| `src/hosted/orchestration.rs` | 26 | fn | `as_str` | `pub(super)` |
| `src/hosted/orchestration.rs` | 40 | fn | `permits_model_call` | `pub(super)` |
| `src/hosted/orchestration.rs` | 53 | fn | `stage` | `pub(super)` |
| `src/hosted/orchestration.rs` | 66 | struct | `PhaseBudgetAllocation` | `pub(super)` |
| `src/hosted/orchestration.rs` | 85 | fn | `phase_budget_allocation` | `pub(super)` |
| `src/hosted/orchestration.rs` | 120 | struct | `PhaseLedger` | `pub(super)` |
| `src/hosted/orchestration.rs` | 136 | struct | `PhaseBudgetReallocation` | `pub(super)` |
| `src/hosted/orchestration.rs` | 142 | fn | `new` | `pub(super)` |
| `src/hosted/orchestration.rs` | 159 | fn | `active` | `pub(super)` |
| `src/hosted/orchestration.rs` | 163 | fn | `transition` | `pub(super)` |
| `src/hosted/orchestration.rs` | 167 | fn | `total_limit` | `pub(super)` |
| `src/hosted/orchestration.rs` | 182 | fn | `apply_complexity_limit` | `pub(super)` |
| `src/hosted/orchestration.rs` | 193 | fn | `total_calls` | `pub(super)` |
| `src/hosted/orchestration.rs` | 203 | fn | `budgeted_calls` | `pub(super)` |
| `src/hosted/orchestration.rs` | 208 | fn | `implementation_repair_calls` | `pub(super)` |
| `src/hosted/orchestration.rs` | 212 | fn | `phase_calls` | `pub(super)` |
| `src/hosted/orchestration.rs` | 225 | fn | `implementation_repair_capacity` | `pub(super)` |
| `src/hosted/orchestration.rs` | 251 | fn | `ensure_finalization_minimum` | `pub(super)` |
| `src/hosted/orchestration.rs` | 270 | fn | `release_unused_implementation_capacity` | `pub(super)` |
| `src/hosted/orchestration.rs` | 296 | fn | `phase_limit` | `pub(super)` |
| `src/hosted/orchestration.rs` | 368 | fn | `begin_graph_model_call` | `pub(super)` |
| `src/hosted/orchestration.rs` | 392 | fn | `rollback_model_call` | `pub(super)` |
| `src/hosted/orchestration.rs` | 419 | fn | `telemetry` | `pub(super)` |
| `src/hosted/orchestration.rs` | 476 | struct | `SearchSignature` | `pub(super)` |
| `src/hosted/orchestration.rs` | 479 | fn | `new` | `pub(super)` |
| `src/hosted/orchestration.rs` | 502 | struct | `SearchGuard` | `pub(super)` |
| `src/hosted/orchestration.rs` | 508 | fn | `validate` | `pub(super)` |
| `src/hosted/orchestration.rs` | 522 | fn | `record` | `pub(super)` |
| `src/hosted/orchestration.rs` | 527 | fn | `record_non_search` | `pub(super)` |
| `src/hosted/provider.rs` | 4 | fn | `execution_decision_idempotency_key` | `pub(super)` |
| `src/hosted/provider.rs` | 26 | fn | `orchestration_decision_is_new` | `pub(super)` |
| `src/hosted/provider.rs` | 33 | fn | `execution_decision_action_kind` | `pub(super)` |
| `src/hosted/provider.rs` | 57 | fn | `phase_permits_tool` | `pub(super)` |
| `src/hosted/provider.rs` | 111 | enum | `PhaseDecision` | `pub(super)` |
| `src/hosted/provider.rs` | 117 | struct | `DecisionExecutionResult` | `pub(super)` |
| `src/hosted/provider.rs` | 123 | fn | `execution_decision_name` | `pub(super)` |
| `src/hosted/provider.rs` | 161 | fn | `execution_decision_requires_model_work` | `pub(super)` |
| `src/hosted/provider.rs` | 176 | fn | `execution_decision_has_completed_validation` | `pub(super)` |
| `src/hosted/provider.rs` | 188 | fn | `legal_phase_transition` | `pub(super)` |
| `src/hosted/provider.rs` | 213 | fn | `hosted_tools` | `pub(super)` |
| `src/hosted/provider.rs` | 565 | fn | `hosted_tools_for_phase` | `pub(super)` |
| `src/hosted/provider.rs` | 577 | struct | `ModelActionProfile` | `pub(super)` |
| `src/hosted/provider.rs` | 585 | fn | `for_decision` | `pub(super)` |
| `src/hosted/provider.rs` | 673 | fn | `tool_choice` | `pub(super)` |
| `src/hosted/provider.rs` | 687 | fn | `hosted_tools_for_action` | `pub(super)` |
| `src/hosted/provider.rs` | 767 | fn | `discovery_action_permits_tool` | `pub(super)` |
| `src/hosted/provider.rs` | 787 | fn | `planning_action_permits_tool` | `pub(super)` |
| `src/hosted/provider.rs` | 807 | fn | `successful_tool_updates_last_action` | `pub(super)` |
| `src/hosted/provider.rs` | 815 | fn | `compact_impact_map_finalization_context` | `pub(super)` |
| `src/hosted/provider.rs` | 833 | fn | `repository_validation_commands_from_evidence` | `pub(super)` |
| `src/hosted/provider.rs` | 865 | fn | `compact_implementation_plan_context` | `pub(super)` |
| `src/hosted/provider.rs` | 937 | fn | `compact_impact_map_repair_context` | `pub(super)` |
| `src/hosted/provider.rs` | 962 | fn | `artifact_call_accounting` | `pub(super)` |
| `src/hosted/provider.rs` | 971 | fn | `impact_map_artifact_attempt_payload` | `pub(super)` |
| `src/hosted/provider.rs` | 988 | fn | `accepted_artifact_normalization_metadata` | `pub(super)` |
| `src/hosted/provider.rs` | 1002 | fn | `hosted_agent_instructions_for_decision` | `pub(super)` |
| `src/hosted/provider.rs` | 1020 | fn | `hosted_agent_instructions` | `pub(super)` |
| `src/hosted/provider.rs` | 1067 | fn | `build_hosted_prompt` | `pub(super)` |
| `src/hosted/provider.rs` | 1105 | fn | `partial_implementation_guidance` | `pub(super)` |
| `src/hosted/provider.rs` | 1133 | fn | `visual_impact_guidance` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 4 | fn | `provider_request_metadata` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 21 | fn | `provider_rejected_event` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 70 | fn | `validate_provider_request_envelope` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 71 | const | `ALLOWED_FIELDS` | `private` |
| `src/hosted/provider_protocol.rs` | 179 | fn | `validate_hosted_provider_startup_contract` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 203 | fn | `validate_provider_tool_definitions` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 258 | fn | `validate_provider_tool_choice` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 289 | fn | `validate_provider_text_configuration` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 349 | fn | `validate_provider_json_schema` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 356 | const | `MAX_DEPTH` | `private` |
| `src/hosted/provider_protocol.rs` | 357 | const | `ALLOWED_KEYWORDS` | `private` |
| `src/hosted/provider_protocol.rs` | 512 | fn | `provider_schema_type` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 552 | fn | `provider_schema_type_accepts` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 573 | fn | `fit_request_to_input_ceiling` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 593 | fn | `phase_request_input_ceiling` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 604 | fn | `hosted_budget_advisory` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 626 | fn | `compact_hosted_turns` | `pub(super)` |
| `src/hosted/provider_protocol.rs` | 632 | fn | `compact_notebook_for_phase` | `pub(super)` |
| `src/hosted/publication.rs` | 4 | fn | `find_or_create_hosted_pull_request` | `pub(super)` |
| `src/hosted/publication.rs` | 52 | fn | `ensure_hosted_pull_request_draft_state` | `pub(super)` |
| `src/hosted/publication.rs` | 86 | fn | `hosted_pull_request_title` | `pub(super)` |
| `src/hosted/publication.rs` | 95 | struct | `HostedPublicationContext` | `pub(super)` |
| `src/hosted/publication.rs` | 105 | fn | `publish_hosted_branch` | `pub(super)` |
| `src/hosted/publication.rs` | 260 | fn | `ensure_hosted_repository_integrity` | `pub(super)` |
| `src/hosted/publication.rs` | 281 | fn | `ensure_cancellation_repository_integrity` | `pub(super)` |
| `src/hosted/publication.rs` | 307 | fn | `finalization_invalidation_event` | `pub(super)` |
| `src/hosted/publication.rs` | 332 | fn | `validate_reconciled_finalization_route` | `pub(super)` |
| `src/hosted/publication.rs` | 402 | fn | `restored_validation_results_from_snapshot` | `pub(super)` |
| `src/hosted/recovery.rs` | 4 | fn | `validation_entry_allows_gates` | `pub(super)` |
| `src/hosted/recovery.rs` | 13 | fn | `validation_failure_category` | `pub(super)` |
| `src/hosted/recovery.rs` | 25 | fn | `validation_failure_target_hint` | `pub(super)` |
| `src/hosted/recovery.rs` | 40 | fn | `committed_head_for_publication` | `pub(super)` |
| `src/hosted/recovery.rs` | 53 | struct | `RecoveryPublicationAuthorization` | `pub(super)` |
| `src/hosted/recovery.rs` | 61 | fn | `authorize_recovery_publication` | `pub(super)` |
| `src/hosted/recovery.rs` | 162 | fn | `is_hosted_orchestration_invariant_error` | `pub(super)` |
| `src/hosted/recovery.rs` | 175 | fn | `hosted_failure_category` | `pub(super)` |
| `src/hosted/recovery.rs` | 191 | fn | `recovery_execution_is_active` | `pub(super)` |
| `src/hosted/recovery.rs` | 195 | fn | `ensure_recovery_execution_active` | `pub(super)` |
| `src/hosted/recovery.rs` | 202 | fn | `recovery_completion_evaluation` | `pub(super)` |
| `src/hosted/recovery.rs` | 261 | struct | `RecoveryPublicationContext` | `pub(super)` |
| `src/hosted/recovery.rs` | 276 | enum | `RecoveryPublicationResult` | `pub(super)` |
| `src/hosted/recovery.rs` | 283 | fn | `recovery_publication_no_op` | `pub(super)` |
| `src/hosted/recovery.rs` | 298 | struct | `RecoveryPublicationOutcome` | `pub(super)` |
| `src/hosted/recovery.rs` | 304 | fn | `attempt_safe_recovery_publication` | `pub(super)` |
| `src/hosted/recovery.rs` | 381 | fn | `attempt_safe_recovery_publication_with` | `pub(super)` |
| `src/hosted/recovery.rs` | 669 | struct | `CancellationBranchPreservation` | `pub(super)` |
| `src/hosted/recovery.rs` | 678 | fn | `preserve_cancellation_branch_with` | `pub(super)` |
| `src/hosted/recovery.rs` | 726 | fn | `dispatch_validation_gates` | `pub(super)` |
| `src/hosted/recovery.rs` | 737 | fn | `canonical_validation_evidence_status` | `pub(super)` |
| `src/hosted/recovery.rs` | 756 | fn | `canonical_validation_evidence_record` | `pub(super)` |
| `src/hosted/recovery.rs` | 821 | fn | `run_graph_validation_sequence` | `pub(super)` |
| `src/hosted/telemetry.rs` | 4 | fn | `send_execution_telemetry` | `pub(super)` |
| `src/hosted/telemetry.rs` | 47 | fn | `send_quality_gate_phase_telemetry` | `pub(super)` |
| `src/hosted/telemetry.rs` | 71 | fn | `quality_gate_phase_event` | `pub(super)` |
| `src/hosted/telemetry.rs` | 115 | fn | `safe_failure` | `pub(super)` |
| `src/hosted/telemetry.rs` | 177 | fn | `failure_diagnostics` | `pub(super)` |
| `src/hosted/telemetry.rs` | 301 | fn | `unsuccessful_completion` | `pub(super)` |
| `src/hosted/telemetry.rs` | 329 | fn | `hosted_pull_request_body` | `pub(super)` |
| `src/hosted/telemetry.rs` | 504 | fn | `sanitized_message_content` | `pub(super)` |
| `src/hosted/telemetry.rs` | 523 | fn | `cache_observability_payload` | `pub(super)` |
| `src/hosted/tools/filesystem.rs` | 58 | fn | `safe_repo_path` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 111 | fn | `collect_repo_files` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 167 | enum | `FileReadStatus` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 172 | fn | `read_error_progress_class` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 182 | fn | `successful_read_progress` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 213 | struct | `FileReadResult` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 227 | struct | `BatchReadResult` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 232 | struct | `PrevalidatedRepoFile` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 239 | enum | `PrevalidatedBatchReadPath` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 247 | fn | `failed_file_read` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 268 | fn | `prevalidate_repo_file` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 340 | fn | `read_prevalidated_repo_file_result` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 426 | fn | `read_repo_file_result` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 441 | fn | `prevalidate_batch_read_paths` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 475 | fn | `read_prevalidated_repo_files_with_fallback` | `pub(in crate::hosted)` |
| `src/hosted/tools/filesystem.rs` | 526 | fn | `read_prevalidated_repo_files_with_evidence_cache` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 13 | fn | `execute_tool` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 868 | fn | `validate_tool_for_phase` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 974 | fn | `path_is_targeted` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 1001 | fn | `required_tool_string` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 1013 | fn | `push_unique` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 1019 | fn | `is_source_mutation_tool` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 1035 | fn | `informational_write_progress` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 1047 | fn | `informational_write_progress_semantics` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 1052 | fn | `tool_target` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 1060 | fn | `tool_change_id` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 1068 | fn | `repo_file_sha256` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 1074 | fn | `classify_write_failure` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 1098 | fn | `tool_intent_sha256` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 1105 | fn | `model_budget_handoff_summary` | `pub(in crate::hosted)` |
| `src/hosted/tools/mod.rs` | 1117 | fn | `ai_budget_exhaustion_reason` | `pub(in crate::hosted)` |
| `src/hosted/tools/mutation.rs` | 4 | fn | `replace_unique_repo_text` | `pub(in crate::hosted)` |
| `src/hosted/tools/mutation.rs` | 49 | fn | `sha256_text` | `pub(in crate::hosted)` |
| `src/hosted/tools/mutation.rs` | 53 | fn | `mutation_output` | `pub(in crate::hosted)` |
| `src/hosted/tools/mutation.rs` | 70 | fn | `write_repo_file` | `pub(in crate::hosted)` |
| `src/hosted/tools/mutation.rs` | 112 | fn | `replace_repo_range` | `pub(in crate::hosted)` |
| `src/hosted/tools/mutation.rs` | 162 | fn | `insert_relative_to_symbol` | `pub(in crate::hosted)` |
| `src/hosted/tools/mutation.rs` | 210 | fn | `apply_repo_unified_diff` | `pub(in crate::hosted)` |
| `src/hosted/tools/mutation.rs` | 296 | fn | `delete_repo_file` | `pub(in crate::hosted)` |
| `src/hosted/tools/search.rs` | 4 | struct | `SearchResult` | `pub(in crate::hosted)` |
| `src/hosted/tools/search.rs` | 10 | fn | `search_repo` | `pub(in crate::hosted)` |
| `src/hosted/tools/search.rs` | 139 | fn | `truncate_text` | `pub(in crate::hosted)` |
