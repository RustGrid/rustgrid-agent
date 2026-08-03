// Extracted from the hosted execution composition root.
use super::*;

impl<'a> GatewayAgent<'a> {
    pub(in crate::hosted) fn evaluate_completion(
        &mut self,
        implementation: &ImplementationOutcome,
        validation: &[ValidationResult],
        changed_paths: &[String],
    ) -> Result<CompletionEvaluation> {
        let unrecovered =
            self.reconcile_write_failures(implementation, validation, changed_paths)?;
        let fallback = completion_fallback(
            implementation,
            self.impact_map.as_ref(),
            self.implementation_plan.as_ref(),
            &unrecovered,
            changed_paths,
            &self.notebook.acceptance_criteria,
            validation,
            project_verification_policy(self.manifest),
        );
        let decision = self.reconcile_active_phase(
            "diff review finished; independent completion evaluation started",
        )?;
        if !matches!(
            decision,
            PhaseDecision::Transition(ExecutionPhase::CompletionEvaluation)
        ) && self.phases.active() != ExecutionPhase::CompletionEvaluation
        {
            bail!(
                "lifecycle invariant violated: completion evaluation requires completed diff review"
            );
        }
        if changed_paths.is_empty() {
            return Ok(fallback);
        }
        if self
            .phases
            .phase_calls(ExecutionPhase::CompletionEvaluation)
            >= self
                .phases
                .phase_limit(ExecutionPhase::CompletionEvaluation)
        {
            return Ok(fallback);
        }

        let diff = match completion_review_diff(
            &self.repo.root,
            changed_paths,
            &self.manifest.github.base_sha,
        ) {
            Ok(diff) => diff,
            Err(_) => return Ok(fallback),
        };
        let prompt = format!(
            "Independently evaluate whether this repository diff fully implements the ticket. \
Regression gates are only technical validation and cannot by themselves satisfy functional \
criteria. Every satisfied criterion must cite concrete diff evidence. Missing evidence is \
uncertain or incomplete. An unrecovered edit failure blocks complete. A broad task with a narrow \
diff needs explicit architectural evidence. Classify human, design, accessibility, visual, \
product-approval, and deployment-environment checks as external review rather than missing source \
implementation. Apply the supplied browser-test policy exactly. Return only one JSON object matching the requested \
schema.\n\nTicket title:\n{}\n\nTicket description and acceptance criteria:\n{}\n\nProject verification policy:\n{}\n\nImpact map:\n{}\n\nImplementation plan:\n{}\n\nWorker notebook:\n{}\n\nImplementation declaration:\n{}\n\nBudget exhausted: {}\n\nChanged paths:\n{}\n\nGenuinely unresolved intended changes:\n{}\n\nReconciled intended changes:\n{}\n\nTechnical validation:\n{}\n\nRepository diff:\n{}",
            self.manifest.ticket_title,
            self.manifest.run.input_prompt,
            serde_json::to_string(&project_verification_policy(self.manifest))
                .unwrap_or_else(|_| "{}".into()),
            serde_json::to_string(&self.impact_map).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(&self.implementation_plan).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(&self.notebook).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(&implementation.explicit_declaration)
                .unwrap_or_else(|_| "null".into()),
            implementation.budget_exhausted,
            changed_paths.join("\n"),
            serde_json::to_string(&unrecovered).unwrap_or_else(|_| "[]".into()),
            serde_json::to_string(&self.notebook.intended_changes).unwrap_or_else(|_| "[]".into()),
            serde_json::to_string(validation).unwrap_or_else(|_| "[]".into()),
            truncate_text(&diff, 96 * 1024),
        );
        let mut request = json!({
            "model": self.manifest.ai_gateway.model,
            "input": [{"role": "user", "content": prompt}],
            "instructions": completion_evaluator_instructions(),
            "max_output_tokens": self.manifest.ai_gateway.maximum_output_tokens.min(8_192),
            "reasoning": {"effort": "medium"},
            "store": false,
            "stream": false,
            "metadata": provider_request_metadata(
                self.manifest.execution.execution_id,
                self.manifest.ticket_key.as_str(),
                "rustgrid-completion-evaluator",
                ExecutionPhase::CompletionEvaluation,
                self.budget.resolved_model_call_budget,
            )
        });
        validate_provider_request_envelope(&request)?;
        let attempts_available = self
            .phases
            .phase_limit(ExecutionPhase::CompletionEvaluation)
            .saturating_sub(
                self.phases
                    .phase_calls(ExecutionPhase::CompletionEvaluation),
            );
        for evaluator_attempt in 0..attempts_available {
            let cost_admitted = constrain_request_to_cost_limit(&mut request, &self.cost_guard)?;
            if cost_admitted {
                validate_provider_request_envelope(&request)?;
            }
            let reservation = cost_admitted
                .then(|| self.reserve_graph_model_call(&request))
                .flatten();
            let Some(reservation) = reservation else {
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.model_cost_preflight_stopped",
                        "phase": ExecutionPhase::CompletionEvaluation,
                        "estimated_cost_micros": self.cost_guard.estimated_cost_micros,
                        "hard_limit_micros": self.cost_guard.hard_limit_micros,
                        "model_calls_used": self.phases.total_calls(),
                        "source_mutation_observed": !changed_paths.is_empty(),
                        "resumable": true,
                    }),
                    "completion evaluation model cost preflight",
                );
                break;
            };
            let model_call = match self.phases.begin_graph_model_call() {
                Ok(model_call) => model_call,
                Err(error) => {
                    self.notebook
                        .orchestration
                        .budget
                        .release_model_call_reservation(&reservation);
                    return Err(error);
                }
            };
            if let Err(error) = self.api.append_event(
                "progress",
                json!({
                    "step": "completion_evaluation",
                    "status": if evaluator_attempt == 0 { "running" } else { "retrying" },
                    "evaluation_attempt": evaluator_attempt + 1,
                    "phase": ExecutionPhase::CompletionEvaluation,
                    "model_call": model_call,
                    "budget": self.budget_telemetry(),
                }),
            ) {
                self.phases
                    .rollback_model_call(ExecutionPhase::CompletionEvaluation)?;
                self.notebook
                    .orchestration
                    .budget
                    .release_model_call_reservation(&reservation);
                return Err(error);
            }
            let registration = ai_call_registration(
                self.manifest.execution.execution_id,
                self.api.execution_attempt,
                self.api.session_id()?,
                model_call.saturating_sub(1),
                ExecutionPhase::CompletionEvaluation,
                0,
            );
            let execution_deadline = match hosted_execution_deadline(
                self.execution_started_at,
                Duration::from_secs(self.cost_guard.max_duration_seconds),
            ) {
                Ok(deadline) => deadline,
                Err(error) => {
                    self.phases
                        .rollback_model_call(ExecutionPhase::CompletionEvaluation)?;
                    self.notebook
                        .orchestration
                        .budget
                        .release_model_call_reservation(&reservation);
                    return Err(error);
                }
            };
            let model_call_started = Instant::now();
            let evaluated_response = match self.api.ai_response_until(
                request.clone(),
                &registration,
                Some(execution_deadline),
            ) {
                Ok(response) => {
                    self.record_cache_observability(&request, &response);
                    self.observe_model_cost(
                        &reservation,
                        &request,
                        &response,
                        model_call_started.elapsed(),
                    )?;
                    Some(response)
                }
                Err(error) => {
                    let http = error.downcast_ref::<HostedHttpError>();
                    let budget_disposition = http
                        .map(HostedHttpError::budget_disposition)
                        .unwrap_or(AiBudgetDisposition::Unknown);
                    if budget_disposition == AiBudgetDisposition::Restore {
                        self.phases
                            .rollback_model_call(ExecutionPhase::CompletionEvaluation)?;
                        self.notebook
                            .orchestration
                            .budget
                            .release_model_call_reservation(&reservation);
                        if let Some(failure) = http.filter(|failure| {
                            failure.failure_class() == AiFailureClass::ProviderValidation
                        }) {
                            self.append_event_recoverable(
                                "progress",
                                provider_rejected_event(
                                    failure,
                                    &registration,
                                    self.api.execution_attempt,
                                    model_call,
                                    self.manifest.ai_gateway.model.as_str(),
                                    self.phases.total_calls(),
                                    self.budget_telemetry(),
                                    json!(&self.notebook),
                                ),
                                "completion evaluator provider rejection telemetry",
                            );
                        }
                    } else {
                        let actual_cost_micros =
                            http.and_then(|failure| failure.actual_cost_micros);
                        self.observe_failed_model_cost(
                            &reservation,
                            &request,
                            actual_cost_micros,
                            model_call_started.elapsed(),
                        );
                    }
                    None
                }
            };
            let evaluated = evaluated_response
                .and_then(|response| response_message_text(&response))
                .and_then(|text| parse_completion_evaluation(&text).ok())
                .map(|evaluation| {
                    reconcile_model_completion_evaluation(
                        evaluation,
                        fallback.clone(),
                        implementation,
                        &unrecovered,
                    )
                })
                .and_then(|evaluation| {
                    validate_completion_evaluation(
                        evaluation,
                        implementation,
                        &unrecovered,
                        changed_paths,
                        &self.notebook.acceptance_criteria,
                    )
                    .ok()
                });
            if let Some(evaluated) = evaluated {
                return Ok(evaluated);
            }
        }
        Ok(fallback)
    }

    pub(in crate::hosted) fn record_completion_evaluated(
        &mut self,
        evaluation: &CompletionEvaluation,
        reviewed_paths: Vec<String>,
        declaration: Option<ImplementationDeclaration>,
        checkpoint_reason: &str,
        repository_changed: bool,
    ) -> Result<()> {
        let sequence = self.next_domain_event_sequence();
        let outcome = mission_outcome_from_completion(evaluation.status);
        let repository_fingerprint =
            repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        let node_id =
            self.graph_node_id(crate::execution_graph::ExecutionNodeKind::CompletionEvaluation)?;
        self.append_execution_domain_event(
            crate::execution_graph::ExecutionDomainEvent::CompletionEvaluated {
                sequence,
                node_id,
                outcome,
            },
        )?;
        let mut validation_evidence_ids = self
            .notebook
            .orchestration
            .evidence
            .validations
            .values()
            .filter(|evidence| {
                evidence.status == crate::execution_graph::ValidationEvidenceStatus::Passed
                    && evidence.repository_fingerprint == repository_fingerprint
            })
            .map(|evidence| evidence.evidence_id.clone())
            .collect::<Vec<_>>();
        validation_evidence_ids.sort();
        self.completion_outcome = Some(outcome);
        self.notebook.completion_artifact = Some(PersistedCompletionArtifact {
            event_sequence: sequence,
            repository_fingerprint,
            validation_evidence_ids,
            reviewed_paths,
            declaration,
            evaluation: evaluation.clone(),
        });
        self.persist_orchestration_checkpoint(checkpoint_reason, repository_changed)
    }
}
pub(in crate::hosted) fn completion_evaluator_instructions() -> &'static str {
    "You are an independent implementation-completeness evaluator. Return only JSON with keys \
status, implementation_completeness, verification_readiness, evaluation_source, confidence, \
criteria, remaining_implementation_work, remaining_automated_verification, \
pending_external_review, optional_follow_up, review_checklist, unrecovered_tool_failures, and \
summary. Status is complete, complete_pending_external_review, partial, incomplete, blocked, or \
uncertain. implementation_completeness is complete, partial, or incomplete. \
verification_readiness is verified, automated_verified, pending_manual_review, or blocked. \
evaluation_source is model. Each criterion contains criterion_id, criterion, verification_type, \
status, evidence, validation_evidence, missing_evidence, and required_next_action. Verification \
type is code, automated_test, manual_qa, accessibility_review, visual_review, product_approval, \
or deployment_environment. Criterion status is satisfied, partially_satisfied, unsatisfied, \
uncertain, external_review_required, or not_applicable. Evidence contains repository-relative \
path and description. Never use passing tests or builds alone as functional evidence and never \
infer missing implementation optimistically. Human, design, product, visual, manual \
accessibility, and deployment-environment verification is external_review_required, not missing \
source code. Treat the final repository, complete diff, authoritative validation, and reconciled \
intended changes as higher precedence than raw tool-attempt history. Only genuinely unresolved \
intended changes may block completeness. Include exactly one criterion result for every acceptance criterion in the worker \
notebook, preserving its ac-N identifier, order, and text verbatim."
}

pub(in crate::hosted) fn response_message_text(response: &Value) -> Option<String> {
    let mut text = String::new();
    for item in response.get("output")?.as_array()? {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        for content in item.get("content")?.as_array()? {
            if content.get("type").and_then(Value::as_str) == Some("output_text") {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(content.get("text")?.as_str()?);
            }
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

pub(in crate::hosted) fn parse_completion_evaluation(text: &str) -> Result<CompletionEvaluation> {
    let trimmed = text.trim();
    let json_text = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    serde_json::from_str(json_text).context("completion evaluator returned malformed JSON")
}

pub(in crate::hosted) fn validate_completion_evaluation(
    mut evaluation: CompletionEvaluation,
    implementation: &ImplementationOutcome,
    unrecovered: &[ToolFailureRecord],
    changed_paths: &[String],
    ticket_criteria: &[String],
) -> Result<CompletionEvaluation> {
    let authoritative_failures = unrecovered
        .iter()
        .map(|failure| {
            format!(
                "{}{}: {}",
                failure.tool,
                failure
                    .target
                    .as_deref()
                    .map(|target| format!(" ({target})"))
                    .unwrap_or_default(),
                failure.error
            )
        })
        .collect::<Vec<_>>();
    evaluation.unrecovered_tool_failures = authoritative_failures;
    if !evaluation.confidence.is_finite()
        || !(0.0..=1.0).contains(&evaluation.confidence)
        || evaluation.summary.trim().is_empty()
        || evaluation.criteria.is_empty()
        || evaluation.criteria.len() != ticket_criteria.len()
    {
        bail!("completion evaluation is incomplete");
    }
    let valid_paths = changed_paths.iter().collect::<BTreeSet<_>>();
    let mut evaluated_ids = BTreeSet::new();
    for (index, criterion) in evaluation.criteria.iter().enumerate() {
        let expected_id = format!("ac-{}", index + 1);
        if criterion.criterion.trim().is_empty()
            || criterion.criterion_id != expected_id
            || criterion.criterion != ticket_criteria[index]
            || !evaluated_ids.insert(criterion.criterion_id.as_str())
        {
            bail!("completion evaluation contains an invalid criterion");
        }
        if criterion.status == CriterionStatus::Satisfied
            && (criterion.evidence.is_empty()
                || criterion.evidence.iter().any(|evidence| {
                    evidence.description.trim().is_empty() || !valid_paths.contains(&evidence.path)
                }))
        {
            bail!("satisfied completion criterion lacks concrete diff evidence");
        }
        if criterion.status == CriterionStatus::ExternalReviewRequired
            && !criterion.verification_type.requires_external_review()
        {
            bail!("implementation-owned criterion cannot require external review");
        }
        if criterion.verification_type.requires_external_review()
            && !matches!(
                criterion.status,
                CriterionStatus::ExternalReviewRequired
                    | CriterionStatus::Satisfied
                    | CriterionStatus::NotApplicable
            )
        {
            bail!("external verification criterion has an invalid ownership status");
        }
        if criterion.status == CriterionStatus::ExternalReviewRequired
            && criterion.required_next_action.is_none()
        {
            bail!("external verification criterion requires an actionable review step");
        }
    }
    if evaluation.implementation_completeness == ImplementationCompleteness::Complete
        && (!unrecovered.is_empty()
            || implementation
                .explicit_declaration
                .as_ref()
                .is_none_or(|declaration| declaration.implementation_status != "complete")
            || !evaluation.remaining_implementation_work.is_empty()
            || !evaluation.remaining_automated_verification.is_empty()
            || evaluation.criteria.iter().any(|criterion| {
                !criterion.verification_type.requires_external_review()
                    && !matches!(
                        criterion.status,
                        CriterionStatus::Satisfied | CriterionStatus::NotApplicable
                    )
            }))
    {
        bail!("completion evaluator cannot prove implementation completeness");
    }
    if evaluation.status == CompletionStatus::CompletePendingExternalReview
        && (evaluation.implementation_completeness != ImplementationCompleteness::Complete
            || evaluation.verification_readiness != VerificationReadiness::PendingManualReview
            || evaluation.pending_external_review.is_empty()
            || evaluation.review_checklist.is_empty())
    {
        bail!("review-pending completion lacks its external review contract");
    }
    Ok(evaluation)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::hosted) fn completion_fallback(
    implementation: &ImplementationOutcome,
    impact_map: Option<&ImpactMap>,
    implementation_plan: Option<&ImplementationPlan>,
    unrecovered: &[ToolFailureRecord],
    changed_paths: &[String],
    ticket_criteria: &[String],
    validation: &[ValidationResult],
    policy: ProjectVerificationPolicy,
) -> CompletionEvaluation {
    let declaration = implementation.explicit_declaration.as_ref();
    let valid_paths = changed_paths.iter().collect::<BTreeSet<_>>();
    let all_validation_passed = validation.iter().all(|result| result.status == "passed");
    let mut evaluation = CompletionEvaluation {
        status: CompletionStatus::Uncertain,
        implementation_completeness: ImplementationCompleteness::Incomplete,
        verification_readiness: VerificationReadiness::Blocked,
        evaluation_source: EvaluationSource::OrchestratorFallback,
        confidence: 1.0,
        criteria: ticket_criteria
            .iter()
            .enumerate()
            .map(|(index, criterion)| {
                let mut verification_type = verification_type_for_criterion(criterion);
                let required_planned_paths = implementation_plan
                    .into_iter()
                    .flat_map(|plan| plan.planned_changes.iter())
                    .filter(|change| {
                        change
                            .acceptance_criteria
                            .iter()
                            .any(|mapped| mapped.trim() == criterion.trim())
                    })
                    .flat_map(|change| change.targets.iter().map(|target| target.path.clone()))
                    .collect::<BTreeSet<_>>();
                let unchanged_required_paths = required_planned_paths
                    .iter()
                    .filter(|path| !valid_paths.contains(path))
                    .cloned()
                    .collect::<Vec<_>>();
                let mut evidence = declaration
                    .into_iter()
                    .flat_map(|declaration| declaration.criteria_evidence.iter())
                    .filter(|item| item.criterion.trim() == criterion.trim())
                    .flat_map(|item| {
                        item.paths
                            .iter()
                            .filter(|path| valid_paths.contains(path))
                            .map(|path| CompletionEvidence {
                                path: path.clone(),
                                description: item.evidence.clone(),
                            })
                    })
                    .collect::<Vec<_>>();
                if evidence.is_empty() {
                    evidence = impact_map
                        .into_iter()
                        .flat_map(|map| map.areas.iter())
                        .filter(|area| {
                            area.acceptance_criteria_ids
                                .iter()
                                .any(|mapped| mapped == &impact_map::criterion_id(index))
                        })
                        .flat_map(|area| {
                            area.candidate_paths
                                .iter()
                                .filter(|path| valid_paths.contains(path))
                                .map(|path| CompletionEvidence {
                                    path: path.clone(),
                                    description: area.reason.clone(),
                                })
                        })
                        .collect();
                }
                if evidence.is_empty() {
                    evidence = implementation_plan
                        .into_iter()
                        .flat_map(|plan| plan.planned_changes.iter())
                        .filter(|change| {
                            change
                                .acceptance_criteria
                                .iter()
                                .any(|mapped| mapped.trim() == criterion.trim())
                        })
                        .flat_map(|change| {
                            change
                                .targets
                                .iter()
                                .filter(|target| valid_paths.contains(&target.path))
                                .map(|target| CompletionEvidence {
                                    path: target.path.clone(),
                                    description: if target.role.is_empty() {
                                        change.reason.clone()
                                    } else {
                                        target.role.clone()
                                    },
                                })
                        })
                        .collect();
                }
                let mandatory_e2e_missing = browser_e2e_is_mandatory_and_missing(
                    criterion,
                    policy,
                    changed_paths,
                );
                if mandatory_e2e_missing {
                    verification_type = VerificationType::AutomatedTest;
                }
                let (status, missing_evidence, required_next_action) =
                    if verification_type.requires_external_review() {
                        (
                            CriterionStatus::ExternalReviewRequired,
                            vec!["External review evidence has not been recorded.".into()],
                            Some(criterion.clone()),
                        )
                    } else if mandatory_e2e_missing {
                        (
                            CriterionStatus::Unsatisfied,
                            vec!["Project policy requires browser E2E coverage for this theme change.".into()],
                            Some("Add and pass the required authenticated browser E2E coverage.".into()),
                        )
                    } else if !unchanged_required_paths.is_empty() {
                        (
                            CriterionStatus::Unsatisfied,
                            vec![format!(
                                "Required planned paths were unchanged: {}.",
                                unchanged_required_paths.join(", ")
                            )],
                            Some(format!(
                                "Implement and verify the unchanged required paths: {}.",
                                unchanged_required_paths.join(", ")
                            )),
                        )
                    } else if !unrecovered.is_empty() {
                        (
                            CriterionStatus::Unsatisfied,
                            vec!["A source-changing tool failure remains unrecovered.".into()],
                            Some("Recover the failed implementation change and rerun validation.".into()),
                        )
                    } else if declaration.is_some_and(|value| {
                        value.implementation_status == "complete"
                    }) && !evidence.is_empty()
                        && (verification_type != VerificationType::AutomatedTest
                            || all_validation_passed)
                    {
                        (CriterionStatus::Satisfied, Vec::new(), None)
                    } else {
                        (
                            CriterionStatus::Uncertain,
                            vec!["No complete criterion-to-diff evidence was available.".into()],
                            Some("Provide concrete implementation evidence for this criterion.".into()),
                        )
                    };
                CriterionEvaluation {
                    criterion_id: format!("ac-{}", index + 1),
                    criterion: criterion.clone(),
                    verification_type,
                    status,
                    evidence,
                    validation_evidence: if status == CriterionStatus::Satisfied
                        && matches!(
                            verification_type,
                            VerificationType::Code | VerificationType::AutomatedTest
                        )
                    {
                        validation
                            .iter()
                            .filter(|result| result.status == "passed")
                            .map(|result| result.command.clone())
                            .collect()
                    } else {
                        Vec::new()
                    },
                    missing_evidence,
                    required_next_action,
                }
            })
            .collect(),
        remaining_implementation_work: Vec::new(),
        remaining_automated_verification: Vec::new(),
        pending_external_review: Vec::new(),
        optional_follow_up: Vec::new(),
        review_checklist: Vec::new(),
        unrecovered_tool_failures: unrecovered
            .iter()
            .map(|failure| {
                format!(
                    "{}{}: {}",
                    failure.tool,
                    failure
                        .target
                        .as_deref()
                        .map(|target| format!(" ({target})"))
                        .unwrap_or_default(),
                    failure.error
                )
            })
            .collect(),
        summary: "Completion was classified from the authoritative notebook, diff, declaration, and validation evidence.".into(),
    };
    if let Some(declaration) = declaration {
        for work in &declaration.remaining_work {
            classify_remaining_work(work, &mut evaluation);
        }
    }
    finalize_completion_dimensions(&mut evaluation, implementation, unrecovered);
    evaluation
}

pub(in crate::hosted) fn reconcile_model_completion_evaluation(
    model: CompletionEvaluation,
    mut fallback: CompletionEvaluation,
    implementation: &ImplementationOutcome,
    unrecovered: &[ToolFailureRecord],
) -> CompletionEvaluation {
    if model.criteria.is_empty() {
        return fallback;
    }
    let mut matched = 0_usize;
    for expected in &mut fallback.criteria {
        if let Some(candidate) = model.criteria.iter().find(|candidate| {
            candidate.criterion_id == expected.criterion_id
                && candidate.criterion == expected.criterion
        }) {
            let mut candidate = candidate.clone();
            if candidate.status == CriterionStatus::Satisfied {
                for validation in &expected.validation_evidence {
                    push_unique(&mut candidate.validation_evidence, validation.clone());
                }
            }
            *expected = candidate;
            matched = matched.saturating_add(1);
        }
    }
    fallback.confidence = model.confidence;
    if !model.summary.trim().is_empty() {
        fallback.summary = model.summary;
    }
    fallback.optional_follow_up = model.optional_follow_up;
    fallback.evaluation_source =
        if matched == fallback.criteria.len() && model.criteria.len() == fallback.criteria.len() {
            EvaluationSource::Model
        } else {
            EvaluationSource::Hybrid
        };
    finalize_completion_dimensions(&mut fallback, implementation, unrecovered);
    fallback
}

pub(in crate::hosted) fn finalize_completion_dimensions(
    evaluation: &mut CompletionEvaluation,
    implementation: &ImplementationOutcome,
    unrecovered: &[ToolFailureRecord],
) {
    evaluation.review_checklist.clear();
    for criterion in &evaluation.criteria {
        match criterion.status {
            CriterionStatus::ExternalReviewRequired => {
                push_unique(
                    &mut evaluation.pending_external_review,
                    criterion
                        .required_next_action
                        .clone()
                        .unwrap_or_else(|| criterion.criterion.clone()),
                );
                evaluation.review_checklist.push(ReviewChecklistItem {
                    r#type: criterion.verification_type,
                    description: criterion
                        .required_next_action
                        .clone()
                        .unwrap_or_else(|| criterion.criterion.clone()),
                    status: "pending".into(),
                });
            }
            CriterionStatus::Unsatisfied | CriterionStatus::PartiallySatisfied => {
                let work = criterion
                    .required_next_action
                    .clone()
                    .unwrap_or_else(|| criterion.criterion.clone());
                if criterion.verification_type == VerificationType::AutomatedTest {
                    push_unique(&mut evaluation.remaining_automated_verification, work);
                } else if !criterion.verification_type.requires_external_review() {
                    push_unique(&mut evaluation.remaining_implementation_work, work);
                }
            }
            CriterionStatus::Satisfied
            | CriterionStatus::Uncertain
            | CriterionStatus::NotApplicable => {}
        }
    }
    let declaration_status = implementation
        .explicit_declaration
        .as_ref()
        .map(|declaration| declaration.implementation_status.as_str());
    let internal_criteria_complete = evaluation.criteria.iter().all(|criterion| {
        criterion.verification_type.requires_external_review()
            || matches!(
                criterion.status,
                CriterionStatus::Satisfied | CriterionStatus::NotApplicable
            )
    });
    evaluation.implementation_completeness =
        if implementation.explicit_declaration.as_ref().is_none()
            || declaration_status == Some("blocked")
            || implementation.budget_exhausted
            || !unrecovered.is_empty()
            || !evaluation.remaining_implementation_work.is_empty()
            || !evaluation.remaining_automated_verification.is_empty()
            || !internal_criteria_complete
        {
            if declaration_status == Some("blocked")
                || implementation.explicit_declaration.is_none()
                || !unrecovered.is_empty()
            {
                ImplementationCompleteness::Incomplete
            } else {
                ImplementationCompleteness::Partial
            }
        } else {
            ImplementationCompleteness::Complete
        };
    evaluation.verification_readiness = if declaration_status == Some("blocked") {
        VerificationReadiness::Blocked
    } else if !evaluation.pending_external_review.is_empty() {
        VerificationReadiness::PendingManualReview
    } else if evaluation.implementation_completeness == ImplementationCompleteness::Complete {
        VerificationReadiness::AutomatedVerified
    } else {
        VerificationReadiness::Blocked
    };
    evaluation.status = if declaration_status == Some("blocked") {
        CompletionStatus::Blocked
    } else if evaluation.implementation_completeness == ImplementationCompleteness::Complete
        && evaluation.verification_readiness == VerificationReadiness::PendingManualReview
    {
        CompletionStatus::CompletePendingExternalReview
    } else if evaluation.implementation_completeness == ImplementationCompleteness::Complete {
        CompletionStatus::Complete
    } else if implementation.explicit_declaration.is_none() {
        CompletionStatus::Uncertain
    } else if evaluation.implementation_completeness == ImplementationCompleteness::Partial {
        CompletionStatus::Partial
    } else {
        CompletionStatus::Incomplete
    };
}

pub(in crate::hosted) fn verification_type_for_criterion(criterion: &str) -> VerificationType {
    let normalized = criterion.to_ascii_lowercase();
    if normalized.contains("product")
        || normalized.contains("design owner")
        || normalized.contains("palette approval")
        || normalized.contains("approved by")
    {
        VerificationType::ProductApproval
    } else if normalized.contains("accessibility")
        || normalized.contains("contrast")
        || normalized.contains("keyboard focus")
    {
        VerificationType::AccessibilityReview
    } else if normalized.contains("screenshot") || normalized.contains("visual review") {
        VerificationType::VisualReview
    } else if normalized.contains("deployment")
        || normalized.contains("staging")
        || normalized.contains("production environment")
    {
        VerificationType::DeploymentEnvironment
    } else if normalized.contains("manual")
        || normalized.contains("navigation")
        || normalized.contains("page reload")
        || normalized.contains("browser verification")
    {
        VerificationType::ManualQa
    } else if normalized.contains("test")
        || normalized.contains("coverage")
        || normalized.contains("build")
        || normalized.contains("lint")
    {
        VerificationType::AutomatedTest
    } else {
        VerificationType::Code
    }
}

pub(in crate::hosted) fn browser_e2e_is_mandatory_and_missing(
    criterion: &str,
    policy: ProjectVerificationPolicy,
    changed_paths: &[String],
) -> bool {
    if !policy.browser_e2e_required_for_theme_changes {
        return false;
    }
    let normalized = criterion.to_ascii_lowercase();
    let is_theme_browser_criterion = (normalized.contains("theme")
        || normalized.contains("palette"))
        && (normalized.contains("browser")
            || normalized.contains("navigation")
            || normalized.contains("reload")
            || normalized.contains("e2e"));
    is_theme_browser_criterion
        && !changed_paths.iter().any(|path| {
            let path = path.to_ascii_lowercase();
            path.contains("e2e")
                || path.contains("playwright")
                || path.ends_with(".spec.ts")
                || path.ends_with(".spec.tsx")
        })
}

pub(in crate::hosted) fn classify_remaining_work(
    work: &str,
    evaluation: &mut CompletionEvaluation,
) {
    let verification_type = verification_type_for_criterion(work);
    if verification_type.requires_external_review() {
        push_unique(&mut evaluation.pending_external_review, work.to_owned());
    } else if verification_type == VerificationType::AutomatedTest {
        push_unique(
            &mut evaluation.remaining_automated_verification,
            work.to_owned(),
        );
    } else {
        push_unique(
            &mut evaluation.remaining_implementation_work,
            work.to_owned(),
        );
    }
}

pub(in crate::hosted) fn completion_review_diff(
    root: &Path,
    changed_paths: &[String],
    base_sha: &str,
) -> Result<String> {
    let diff = command::capture(
        "git",
        ["diff", "--no-ext-diff", "--binary", base_sha, "--"],
        root,
    )?;
    if !diff.status.success() {
        bail!("git diff exited with {}: {}", diff.status, diff.stderr);
    }
    let mut review = diff.stdout;
    for path in changed_paths {
        let target = safe_repo_path(root, path, false)?;
        let tracked = command::capture("git", ["ls-files", "--error-unmatch", "--", path], root)?
            .status
            .success();
        if tracked || !target.is_file() {
            continue;
        }
        review.push_str(&format!("\n\n--- /dev/null\n+++ b/{path}\n"));
        match fs::read(&target) {
            Ok(bytes) if bytes.len() <= MAX_MODEL_FILE_BYTES && !bytes.contains(&0) => {
                review.push_str(&String::from_utf8_lossy(&bytes));
            }
            Ok(bytes) => review.push_str(&format!(
                "[new binary or large file: {} bytes]",
                bytes.len()
            )),
            Err(error) => review.push_str(&format!("[could not read new file: {error}]")),
        }
    }
    Ok(review)
}

pub(in crate::hosted) fn completion_changed_paths(
    repo: &Repo,
    base_sha: &str,
) -> Result<Vec<String>> {
    let mut changed_paths = repo
        .new_agent_paths(&BTreeSet::new())?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let output = command::capture(
        "git",
        ["diff", "--name-only", "-z", base_sha, "HEAD", "--"],
        &repo.root,
    )?;
    if !output.status.success() {
        bail!(
            "git diff --name-only exited with {}: {}",
            output.status,
            output.stderr
        );
    }
    changed_paths.extend(
        output
            .stdout
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned),
    );
    Ok(changed_paths.into_iter().collect())
}

pub(in crate::hosted) fn append_fingerprint_field(
    material: &mut Vec<u8>,
    name: &str,
    value: &[u8],
) {
    material.extend_from_slice(&(name.len() as u64).to_be_bytes());
    material.extend_from_slice(name.as_bytes());
    material.extend_from_slice(&(value.len() as u64).to_be_bytes());
    material.extend_from_slice(value);
}

pub(in crate::hosted) fn repository_state_fingerprint(
    repo: &Repo,
    base_sha: &str,
) -> Result<String> {
    let head = command::checked("git", ["rev-parse", "HEAD"], &repo.root)?;
    let comparison_base = if base_sha.trim().is_empty() {
        head.trim()
    } else {
        base_sha.trim()
    };
    let identity = command::capture("git", ["remote", "get-url", "origin"], &repo.root)?;
    let repository_identity = if identity.status.success() {
        identity.stdout.trim().to_owned()
    } else {
        repo.root
            .canonicalize()
            .unwrap_or_else(|_| repo.root.clone())
            .to_string_lossy()
            .into_owned()
    };
    let base_paths = command::capture(
        "git",
        ["ls-tree", "-r", "--name-only", "-z", comparison_base],
        &repo.root,
    )?;
    if !base_paths.status.success() {
        bail!(
            "git base-tree listing for repository fingerprint failed: {}",
            base_paths.stderr
        );
    }
    let current_paths = command::capture(
        "git",
        [
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        &repo.root,
    )?;
    if !current_paths.status.success() {
        bail!(
            "git worktree listing for repository fingerprint failed: {}",
            current_paths.stderr
        );
    }
    let mut material = Vec::new();
    append_fingerprint_field(&mut material, "repository", repository_identity.as_bytes());
    append_fingerprint_field(&mut material, "base_head", comparison_base.as_bytes());
    let paths = base_paths
        .stdout
        .split('\0')
        .chain(current_paths.stdout.split('\0'))
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    for path in paths {
        if Path::new(&path).is_absolute()
            || Path::new(&path)
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            bail!("git returned an unsafe repository path while fingerprinting state");
        }
        append_fingerprint_field(&mut material, "path", path.as_bytes());
        let target = repo.root.join(&path);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                append_fingerprint_field(&mut material, "type", b"symlink");
                let destination = fs::read_link(&target)
                    .with_context(|| format!("could not hash repository symlink target {path}"))?;
                append_fingerprint_field(
                    &mut material,
                    "symlink_target",
                    destination.to_string_lossy().as_bytes(),
                );
            }
            Ok(metadata) if metadata.is_file() => {
                append_fingerprint_field(&mut material, "type", b"file");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    append_fingerprint_field(
                        &mut material,
                        "mode",
                        &metadata.permissions().mode().to_be_bytes(),
                    );
                }
                let bytes = fs::read(&target)
                    .with_context(|| format!("could not hash repository file {path}"))?;
                append_fingerprint_field(
                    &mut material,
                    "content_sha256",
                    hex::encode(Sha256::digest(bytes)).as_bytes(),
                );
            }
            Ok(_) => append_fingerprint_field(&mut material, "type", b"directory_or_gitlink"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                append_fingerprint_field(&mut material, "type", b"missing");
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not inspect repository path {path}"));
            }
        }
    }
    append_fingerprint_field(
        &mut material,
        "dependency_lock",
        dependency_lock_fingerprint(&repo.root)?.as_bytes(),
    );
    Ok(hex::encode(Sha256::digest(material)))
}
