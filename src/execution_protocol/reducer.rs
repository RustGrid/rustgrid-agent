use std::collections::{BTreeMap, BTreeSet, VecDeque, btree_map::Entry};

use super::*;

pub(crate) fn decide(state: &ExecutionState) -> Result<ProtocolDecision, ProtocolViolation> {
    validate_state(state)?;
    if let Some(result) = &state.terminal {
        return Ok(ProtocolDecision::Finish {
            result: result.clone(),
        });
    }
    if let Some(record) = state
        .budgets
        .model_calls
        .values()
        .find(|record| record.state == ModelCallState::Dispatched)
    {
        return Ok(ProtocolDecision::Wait {
            reason: WaitReason::ProviderReconciliation {
                call_id: record.admission.call_id.clone(),
            },
        });
    }
    if state.repository_profile.is_some() && state.stage() == ProtocolStage::Discovery {
        return decide_phase2_discovery(state);
    }
    if state.planning.is_some() && state.stage() == ProtocolStage::Planning {
        return decide_phase3_planning(state);
    }
    if state.implementation.is_some() && state.stage() == ProtocolStage::Implementation {
        return decide_phase4_implementation(state);
    }
    if state.validation.is_some()
        && matches!(
            state.stage(),
            ProtocolStage::Validation | ProtocolStage::Repair
        )
    {
        return decide_phase6_validation(state);
    }
    if state.finalization_policy.is_some()
        && state.review.is_some()
        && state.stage() == ProtocolStage::Review
    {
        return decide_phase7_review(state);
    }
    if state.finalization_policy.is_some()
        && state.publication.is_some()
        && state.stage() == ProtocolStage::Publication
    {
        return decide_phase7_publication(state);
    }
    if let Some(node) = state.active_node() {
        return Ok(ProtocolDecision::Wait {
            reason: WaitReason::ActiveNode {
                node_id: node.id.clone(),
            },
        });
    }
    if let Some(node_id) = state.node_order.iter().find(|node_id| {
        state.nodes.get(*node_id).is_some_and(|node| {
            node.kind.stage() == state.stage() && node.state == NodeState::Ready
        })
    }) {
        let node = state
            .nodes
            .get(node_id)
            .expect("ready node was selected from the aggregate");
        return Ok(ProtocolDecision::Emit {
            event: GraphEvent::NodeStarted {
                node_id: node_id.clone(),
                attempt: node.attempts_started.saturating_add(1),
            }
            .into(),
        });
    }
    Ok(ProtocolDecision::Wait {
        reason: WaitReason::NoRunnableNode {
            stage: state.stage(),
        },
    })
}

fn decide_phase7_review(state: &ExecutionState) -> Result<ProtocolDecision, ProtocolViolation> {
    let review = state
        .review
        .as_ref()
        .ok_or(ProtocolViolation::ReviewContract {
            code: "review_state_missing",
        })?;
    let policy = state
        .finalization_policy
        .as_ref()
        .ok_or(ProtocolViolation::ReviewContract {
            code: "finalization_policy_missing",
        })?;
    let plan = state
        .planning
        .as_ref()
        .and_then(|planning| planning.accepted_plan.as_ref())
        .ok_or(ProtocolViolation::ReviewContract {
            code: "review_accepted_plan_missing",
        })?;

    if let Some(convergence) = &review.convergence {
        if let Some(node) = state.active_node() {
            return Ok(ProtocolDecision::Emit {
                event: GraphEvent::NodeFailed {
                    node_id: node.id.clone(),
                    failure_revision_id: review_convergence_failure_revision(convergence),
                    terminal: true,
                }
                .into(),
            });
        }
        return Ok(ProtocolDecision::Finish {
            result: state.authoritative_review_terminal_result()?,
        });
    }

    if let Some(reason) = review.effect_failure_convergence_reason()? {
        return Ok(ProtocolDecision::Emit {
            event: ReviewEvent::ConvergenceEvaluated {
                convergence: ReviewConvergenceV1::new(
                    state.repository_revision.clone(),
                    policy.policy_id.clone(),
                    reason,
                )?,
            }
            .into(),
        });
    }

    if let Some(node) = state.active_node() {
        return match node.kind {
            NodeKind::Review => {
                if review.diff_request.is_none() {
                    return Ok(ProtocolDecision::Emit {
                        event: ReviewEvent::DiffManifestRequested {
                            request: DiffManifestRequestV1::new(
                                node.id.clone(),
                                plan,
                                &review.ancestry,
                                policy,
                            )?,
                        }
                        .into(),
                    });
                }
                if review.diff_manifest.is_none() {
                    return Ok(ProtocolDecision::Perform {
                        effect: EffectRequest::Review(ReviewEffectRequest::BuildDiffManifest {
                            request: Box::new(
                                review
                                    .diff_request
                                    .as_ref()
                                    .expect("diff request was checked")
                                    .clone(),
                            ),
                        }),
                    });
                }
                if let Some(prepared) = review.current_action() {
                    return drive_phase7_review_action(state, prepared);
                }
                if review.next_unreviewed_page().is_some() {
                    return prepare_phase7_review_action(state, review, policy, plan, node);
                }
                if review.diff_review.is_none() {
                    let manifest = review
                        .diff_manifest
                        .as_deref()
                        .expect("diff manifest was checked");
                    return Ok(ProtocolDecision::Emit {
                        event: ReviewEvent::DiffReviewRecorded {
                            review: Box::new(DiffReviewV1::aggregate(
                                manifest,
                                &review.page_reviews,
                            )?),
                        }
                        .into(),
                    });
                }
                let diff_review = review.diff_review.as_deref().expect("review was checked");
                if diff_review.disposition != DiffReviewDispositionV1::Accepted {
                    return Ok(ProtocolDecision::Emit {
                        event: ReviewEvent::ConvergenceEvaluated {
                            convergence: ReviewConvergenceV1::new(
                                state.repository_revision.clone(),
                                policy.policy_id.clone(),
                                ReviewConvergenceReasonV1::DiffReviewBlocked {
                                    review_id: diff_review.review_id.clone(),
                                },
                            )?,
                        }
                        .into(),
                    });
                }
                let proof = state.review_completion_proof()?;
                if state.proofs.get(&proof.id) != Some(&proof) {
                    return Ok(ProtocolDecision::Emit {
                        event: EvidenceEvent::ProofRecorded { proof }.into(),
                    });
                }
                Ok(ProtocolDecision::Emit {
                    event: GraphEvent::NodeSucceeded {
                        node_id: node.id.clone(),
                        proof_id: proof.id,
                    }
                    .into(),
                })
            }
            NodeKind::CompletionEvaluation => {
                if review.completion.is_none() {
                    if let Some(prepared) = review.current_action() {
                        return drive_phase7_review_action(state, prepared);
                    }
                    return prepare_phase7_review_action(state, review, policy, plan, node);
                }
                let completion = review
                    .completion
                    .as_deref()
                    .expect("completion was checked");
                if completion.disposition == CompletionDispositionV1::Incomplete {
                    return Ok(ProtocolDecision::Emit {
                        event: ReviewEvent::ConvergenceEvaluated {
                            convergence: ReviewConvergenceV1::new(
                                state.repository_revision.clone(),
                                policy.policy_id.clone(),
                                ReviewConvergenceReasonV1::CompletionIncomplete {
                                    evaluation_id: completion.evaluation_id.clone(),
                                },
                            )?,
                        }
                        .into(),
                    });
                }
                let proof = state.completion_evaluation_proof()?;
                if state.proofs.get(&proof.id) != Some(&proof) {
                    return Ok(ProtocolDecision::Emit {
                        event: EvidenceEvent::ProofRecorded { proof }.into(),
                    });
                }
                Ok(ProtocolDecision::Emit {
                    event: GraphEvent::NodeSucceeded {
                        node_id: node.id.clone(),
                        proof_id: proof.id,
                    }
                    .into(),
                })
            }
            _ => Err(ProtocolViolation::WrongPosition {
                node_id: node.id.clone(),
                position: ProtocolStage::Review,
            }),
        };
    }

    if let Some(node) = [NodeKind::Review, NodeKind::CompletionEvaluation]
        .into_iter()
        .flat_map(|kind| state.required_nodes(kind))
        .find(|node| node.state == NodeState::Ready)
    {
        return Ok(ProtocolDecision::Emit {
            event: GraphEvent::NodeStarted {
                node_id: node.id.clone(),
                attempt: node.attempts_started.saturating_add(1),
            }
            .into(),
        });
    }

    if state
        .required_nodes(NodeKind::Review)
        .into_iter()
        .chain(state.required_nodes(NodeKind::CompletionEvaluation))
        .any(|node| !matches!(node.state, NodeState::Succeeded { .. }))
    {
        return Ok(ProtocolDecision::Wait {
            reason: WaitReason::NoRunnableNode {
                stage: ProtocolStage::Review,
            },
        });
    }

    let completion = review
        .completion
        .as_deref()
        .ok_or(ProtocolViolation::ReviewContract {
            code: "review_completion_missing",
        })?;
    if review.authority_request.is_none() {
        return Ok(ProtocolDecision::Emit {
            event: ReviewEvent::PublicationAuthorityRequested {
                request: PublicationAuthorityRequestV1::new(policy, completion)?,
            }
            .into(),
        });
    }
    if review.authority.is_none() {
        return Ok(ProtocolDecision::Perform {
            effect: EffectRequest::Review(ReviewEffectRequest::ObservePublicationAuthority {
                request: Box::new(
                    review
                        .authority_request
                        .as_ref()
                        .expect("authority request was checked")
                        .clone(),
                ),
            }),
        });
    }
    if review.eligibility.is_none() {
        let manifest =
            review
                .diff_manifest
                .as_deref()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "review_diff_manifest_missing",
                })?;
        let diff_review =
            review
                .diff_review
                .as_deref()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "review_diff_review_missing",
                })?;
        let authority = review.authority.as_ref().expect("authority was checked");
        let review_proof = state.review_completion_proof()?;
        let completion_proof = state.completion_evaluation_proof()?;
        let facts = state.publication_eligibility_facts(&review.ancestry)?;
        return Ok(ProtocolDecision::Emit {
            event: ReviewEvent::PublicationEligibilityEvaluated {
                eligibility: Box::new(PublicationEligibilityRecord::new(
                    policy,
                    &review.ancestry,
                    manifest,
                    diff_review,
                    review_proof.id,
                    completion,
                    completion_proof.id,
                    authority,
                    facts,
                )?),
            }
            .into(),
        });
    }
    let eligibility = review
        .eligibility
        .as_deref()
        .expect("eligibility was checked");
    if !eligibility.is_granted() {
        return Ok(ProtocolDecision::Emit {
            event: ReviewEvent::ConvergenceEvaluated {
                convergence: ReviewConvergenceV1::new(
                    state.repository_revision.clone(),
                    policy.policy_id.clone(),
                    ReviewConvergenceReasonV1::PublicationEligibilityDenied {
                        eligibility_id: eligibility.eligibility_id.clone(),
                    },
                )?,
            }
            .into(),
        });
    }
    let proof = state.publication_eligibility_proof()?;
    if state.proofs.get(&proof.id) != Some(&proof) {
        return Ok(ProtocolDecision::Emit {
            event: EvidenceEvent::ProofRecorded { proof }.into(),
        });
    }
    Ok(ProtocolDecision::Emit {
        event: LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Review,
            to: ProtocolStage::Publication,
            proof_id: proof.id,
        }
        .into(),
    })
}

fn drive_phase7_review_action(
    state: &ExecutionState,
    prepared: &PreparedReviewActionV1,
) -> Result<ProtocolDecision, ProtocolViolation> {
    Ok(
        match state
            .budgets
            .model_calls
            .get(&prepared.admission.call_id)
            .map(|record| &record.state)
        {
            None => ProtocolDecision::Emit {
                event: BudgetEvent::ModelCallAdmitted {
                    admission: prepared.admission.clone(),
                }
                .into(),
            },
            Some(ModelCallState::Admitted) => ProtocolDecision::Emit {
                event: BudgetEvent::ModelCallReserved {
                    call_id: prepared.admission.call_id.clone(),
                }
                .into(),
            },
            Some(ModelCallState::Reserved) => ProtocolDecision::Perform {
                effect: EffectRequest::Review(ReviewEffectRequest::DispatchProvider {
                    envelope: Box::new(prepared.envelope.clone()),
                }),
            },
            Some(ModelCallState::Dispatched) => ProtocolDecision::Wait {
                reason: WaitReason::ProviderReconciliation {
                    call_id: prepared.admission.call_id.clone(),
                },
            },
            Some(ModelCallState::ReconciledConsumed { .. }) => ProtocolDecision::Wait {
                reason: WaitReason::ReviewObservation {
                    action_id: prepared.envelope.action_id.clone(),
                },
            },
            Some(ModelCallState::ReconciledReleased) => ProtocolDecision::Emit {
                event: ReviewEvent::ActionReleased {
                    action_id: prepared.envelope.action_id.clone(),
                }
                .into(),
            },
        },
    )
}

fn decide_phase7_publication(
    state: &ExecutionState,
) -> Result<ProtocolDecision, ProtocolViolation> {
    let policy =
        state
            .finalization_policy
            .as_ref()
            .ok_or(ProtocolViolation::PublicationContract {
                code: "finalization_policy_missing",
            })?;
    let review = state
        .review
        .as_ref()
        .ok_or(ProtocolViolation::PublicationContract {
            code: "review_state_missing",
        })?;
    let eligibility =
        review
            .eligibility
            .as_deref()
            .ok_or(ProtocolViolation::PublicationContract {
                code: "publication_eligibility_missing",
            })?;
    let publication = state
        .publication
        .as_ref()
        .ok_or(ProtocolViolation::PublicationContract {
            code: "publication_state_missing",
        })?;

    if let Some(convergence) = &publication.convergence {
        if let Some(node) = state.active_node() {
            return Ok(ProtocolDecision::Emit {
                event: GraphEvent::NodeFailed {
                    node_id: node.id.clone(),
                    failure_revision_id: publication_convergence_failure_revision(convergence),
                    terminal: true,
                }
                .into(),
            });
        }
        return Ok(ProtocolDecision::Finish {
            result: state.authoritative_publication_terminal_result()?,
        });
    }

    if let Some(node) = state.active_node() {
        if node.kind != NodeKind::Publication || node.id != publication.publication_node_id {
            return Err(ProtocolViolation::WrongPosition {
                node_id: node.id.clone(),
                position: ProtocolStage::Publication,
            });
        }
        if publication.completion.is_some() {
            let proof = state.publication_completion_proof()?;
            if state.proofs.get(&proof.id) != Some(&proof) {
                return Ok(ProtocolDecision::Emit {
                    event: EvidenceEvent::ProofRecorded { proof }.into(),
                });
            }
            return Ok(ProtocolDecision::Emit {
                event: GraphEvent::NodeSucceeded {
                    node_id: node.id.clone(),
                    proof_id: proof.id,
                }
                .into(),
            });
        }

        if let Some(last) = publication.attempts.last()
            && last.observation.is_none()
        {
            let material = matches!(last.intent, PublicationAttemptIntentV1::PullRequest(_))
                .then(|| publication_pull_request_material(publication))
                .transpose()?;
            let effect = publication.pending_effect(material)?.ok_or(
                ProtocolViolation::PublicationContract {
                    code: "publication_open_intent_has_no_effect",
                },
            )?;
            return Ok(ProtocolDecision::Perform {
                effect: EffectRequest::Publication(effect),
            });
        }

        if let Some(convergence) = publication.build_convergence(&policy.publication)? {
            return Ok(ProtocolDecision::Emit {
                event: PublicationEvent::ConvergenceEvaluated { convergence }.into(),
            });
        }

        let next_operation = match publication.attempts.last() {
            None => Some(PublicationOperationV1::Commit),
            Some(last) => match last.observation.as_ref() {
                Some(PublicationAttemptObservationV1::Commit(observation)) => {
                    match &observation.outcome {
                        CommitOutcomeV1::Confirmed { .. } => Some(PublicationOperationV1::Push),
                        CommitOutcomeV1::Failed {
                            failure: PublicationEffectFailureV1::Retryable { .. },
                        } => Some(PublicationOperationV1::Commit),
                        CommitOutcomeV1::Failed { .. } => None,
                    }
                }
                Some(PublicationAttemptObservationV1::Push(observation)) => {
                    match &observation.outcome {
                        ExactLeasePushOutcomeV1::Confirmed { .. } => {
                            Some(PublicationOperationV1::PullRequest)
                        }
                        ExactLeasePushOutcomeV1::Failed {
                            failure: PublicationEffectFailureV1::Retryable { .. },
                        } => Some(PublicationOperationV1::Push),
                        ExactLeasePushOutcomeV1::RemoteBranchMoved { .. }
                        | ExactLeasePushOutcomeV1::Failed { .. } => None,
                    }
                }
                Some(PublicationAttemptObservationV1::PullRequest(observation)) => {
                    match &observation.outcome {
                        PullRequestOutcomeV1::Confirmed { .. } => None,
                        PullRequestOutcomeV1::Failed {
                            failure: PublicationEffectFailureV1::Retryable { .. },
                        } => Some(PublicationOperationV1::PullRequest),
                        PullRequestOutcomeV1::Failed { .. } => None,
                    }
                }
                None => None,
            },
        };

        if let Some(operation) = next_operation {
            let event = match operation {
                PublicationOperationV1::Commit => {
                    let manifest = review.diff_manifest.as_deref().ok_or(
                        ProtocolViolation::PublicationContract {
                            code: "publication_diff_manifest_missing",
                        },
                    )?;
                    let authority = review.authority.as_ref().ok_or(
                        ProtocolViolation::PublicationContract {
                            code: "publication_authority_missing",
                        },
                    )?;
                    let tree = CommitTreeBindingV1::from_review_authority(
                        eligibility,
                        manifest,
                        authority,
                    )?;
                    PublicationEvent::CommitIntentPersisted {
                        intent: publication.prepare_commit_intent(
                            &policy.publication,
                            eligibility,
                            tree,
                        )?,
                    }
                }
                PublicationOperationV1::Push => PublicationEvent::PushIntentPersisted {
                    intent: publication.prepare_push_intent(&policy.publication, eligibility)?,
                },
                PublicationOperationV1::PullRequest => {
                    let material = publication_pull_request_material(publication)?;
                    PublicationEvent::PullRequestIntentPersisted {
                        intent: publication.prepare_pull_request_intent(
                            &policy.publication,
                            eligibility,
                            &material,
                        )?,
                    }
                }
            };
            return Ok(ProtocolDecision::Emit {
                event: event.into(),
            });
        }

        if publication.attempts.last().is_some_and(|last| {
            matches!(
                last.observation,
                Some(PublicationAttemptObservationV1::PullRequest(
                    PullRequestObservationV1 {
                        outcome: PullRequestOutcomeV1::Confirmed { .. },
                        ..
                    }
                ))
            )
        }) {
            return Ok(ProtocolDecision::Emit {
                event: PublicationEvent::CompletionRecorded {
                    completion: publication.build_completion(&policy.publication)?,
                }
                .into(),
            });
        }

        return Err(ProtocolViolation::PublicationContract {
            code: "publication_has_no_authoritative_next_step",
        });
    }

    if let Some(node) = state
        .required_nodes(NodeKind::Publication)
        .into_iter()
        .find(|node| node.state == NodeState::Ready)
    {
        return Ok(ProtocolDecision::Emit {
            event: GraphEvent::NodeStarted {
                node_id: node.id.clone(),
                attempt: node.attempts_started.saturating_add(1),
            }
            .into(),
        });
    }

    if publication.completion.is_some()
        && state
            .required_nodes(NodeKind::Publication)
            .iter()
            .all(|node| matches!(node.state, NodeState::Succeeded { .. }))
    {
        return Ok(ProtocolDecision::Finish {
            result: state.authoritative_publication_terminal_result()?,
        });
    }

    Ok(ProtocolDecision::Wait {
        reason: WaitReason::NoRunnableNode {
            stage: ProtocolStage::Publication,
        },
    })
}

fn publication_pull_request_material(
    publication: &PublicationStateV1,
) -> Result<RawPullRequestMaterialV1, ProtocolViolation> {
    let marker = publication.pull_request_execution_marker_hash();
    RawPullRequestMaterialV1::new(
        format!("RustGrid execution {}", publication.execution_id.as_str()).into_bytes(),
        format!(
            "RustGrid publication reconciliation\nexecution_marker={marker}\neligibility={}",
            publication.eligibility_id.as_str()
        )
        .into_bytes(),
    )
    .map_err(Into::into)
}

fn prepare_phase7_review_action(
    state: &ExecutionState,
    review: &ReviewStateV1,
    policy: &FinalizationPolicyV1,
    plan: &AcceptedPlan,
    node: &ExecutionNode,
) -> Result<ProtocolDecision, ProtocolViolation> {
    let remaining = state.planning_budget_remaining(&node.id)?;
    let binding = match node.kind {
        NodeKind::Review => {
            let manifest =
                review
                    .diff_manifest
                    .as_deref()
                    .ok_or(ProtocolViolation::ReviewContract {
                        code: "review_diff_manifest_missing",
                    })?;
            let page = review
                .next_unreviewed_page()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "review_page_missing",
                })?;
            ReviewContextBindingV1::DiffPage {
                manifest_id: manifest.manifest_id.clone(),
                diff_hash: manifest.diff_hash.clone(),
                page_id: page.page_id.clone(),
                page_index: page.index,
                page_content_hash: page.content_hash.clone(),
                content_address: page.content_address.clone(),
                artifact_locator_hash: page.artifact_locator_hash.clone(),
                persistence_receipt_hash: page.persistence_receipt_hash.clone(),
                page_byte_len: page.byte_len,
            }
        }
        NodeKind::CompletionEvaluation => {
            let manifest =
                review
                    .diff_manifest
                    .as_deref()
                    .ok_or(ProtocolViolation::ReviewContract {
                        code: "completion_diff_manifest_missing",
                    })?;
            let diff_review =
                review
                    .diff_review
                    .as_deref()
                    .ok_or(ProtocolViolation::ReviewContract {
                        code: "completion_diff_review_missing",
                    })?;
            ReviewContextBindingV1::Completion {
                manifest_id: manifest.manifest_id.clone(),
                diff_hash: manifest.diff_hash.clone(),
                diff_review_id: diff_review.review_id.clone(),
                page_review_ids: diff_review.ordered_page_review_ids.clone(),
            }
        }
        _ => {
            return Err(ProtocolViolation::WrongPosition {
                node_id: node.id.clone(),
                position: ProtocolStage::Review,
            });
        }
    };
    if remaining.is_exhausted() {
        let rejected_current_binding = review.rejected_actions.keys().any(|action_id| {
            review.actions.get(action_id).is_some_and(|prepared| {
                prepared.context.node_id == node.id && prepared.context.binding == binding
            })
        });
        let reason = if rejected_current_binding {
            ReviewConvergenceReasonV1::ProviderProtocolExhausted {
                node_id: node.id.clone(),
            }
        } else if node.kind == NodeKind::Review {
            ReviewConvergenceReasonV1::ReviewBudgetExhausted {
                node_id: node.id.clone(),
            }
        } else {
            ReviewConvergenceReasonV1::CompletionBudgetExhausted {
                node_id: node.id.clone(),
            }
        };
        return Ok(ProtocolDecision::Emit {
            event: ReviewEvent::ConvergenceEvaluated {
                convergence: ReviewConvergenceV1::new(
                    state.repository_revision.clone(),
                    policy.policy_id.clone(),
                    reason,
                )?,
            }
            .into(),
        });
    }
    if let Some(reason) =
        review.uncontacted_release_convergence(&binding, node.id.clone(), policy)?
    {
        return Ok(ProtocolDecision::Emit {
            event: ReviewEvent::ConvergenceEvaluated {
                convergence: ReviewConvergenceV1::new(
                    state.repository_revision.clone(),
                    policy.policy_id.clone(),
                    reason,
                )?,
            }
            .into(),
        });
    }
    let prior = review
        .actions
        .values()
        .filter(|prepared| prepared.context.binding == binding)
        .max_by_key(|prepared| prepared.envelope.retry_index);
    let (retry_index, prior_action_id) = prior.map_or((1, None), |prepared| {
        (
            prepared.envelope.retry_index.saturating_add(1),
            Some(prepared.envelope.action_id.clone()),
        )
    });
    let criterion_ids = review.criterion_ids.clone();
    let mut evidence_ids = state
        .validation
        .as_ref()
        .into_iter()
        .flat_map(|validation| validation.current_evidence_by_gate.values())
        .map(|evidence_id| EvidenceId::new(evidence_id.as_str()))
        .collect::<BTreeSet<_>>();
    evidence_ids.insert(policy.policy_evidence_id.clone());
    let manifest = review
        .diff_manifest
        .as_deref()
        .ok_or(ProtocolViolation::ReviewContract {
            code: "review_diff_manifest_missing",
        })?;
    let estimated_input_tokens = conservative_review_input_tokens(
        plan,
        &review.ancestry,
        manifest,
        review.diff_review.as_deref(),
        &binding,
        &criterion_ids,
        &evidence_ids,
    )?;
    if estimated_input_tokens > node.budget.max_input_tokens_per_call {
        return Ok(ProtocolDecision::Emit {
            event: ReviewEvent::ConvergenceEvaluated {
                convergence: ReviewConvergenceV1::new(
                    state.repository_revision.clone(),
                    policy.policy_id.clone(),
                    if node.kind == NodeKind::Review {
                        ReviewConvergenceReasonV1::ReviewBudgetExhausted {
                            node_id: node.id.clone(),
                        }
                    } else {
                        ReviewConvergenceReasonV1::CompletionBudgetExhausted {
                            node_id: node.id.clone(),
                        }
                    },
                )?,
            }
            .into(),
        });
    }
    let materialized_context_hash = stable_sha256(&[
        "execution-protocol-v1:review-materialized-context",
        &hex::encode(
            serde_json::to_vec(&(
                &review.ancestry.ancestry_hash,
                &binding,
                &criterion_ids,
                &evidence_ids,
                estimated_input_tokens,
            ))
            .map_err(|error| ProtocolViolation::EventSerialization {
                detail: error.to_string(),
            })?,
        ),
    ]);
    let prepared = PreparedReviewActionV1::new(
        &state.execution_id,
        state.execution_attempt,
        node.id.clone(),
        node.attempts_started,
        retry_index,
        prior_action_id,
        state.repository_revision.clone(),
        plan.plan_id.clone(),
        plan.plan_revision_id.clone(),
        policy.policy_id.clone(),
        &review.ancestry,
        binding,
        criterion_ids,
        evidence_ids,
        node.budget.max_input_tokens_per_call,
        estimated_input_tokens,
        node.budget.max_output_tokens_per_call,
        remaining.cost_micros,
        remaining.duration_ms,
        materialized_context_hash,
    )?;
    Ok(ProtocolDecision::Emit {
        event: ReviewEvent::ActionPrepared {
            prepared: Box::new(prepared),
        }
        .into(),
    })
}

fn decide_phase4_implementation(
    state: &ExecutionState,
) -> Result<ProtocolDecision, ProtocolViolation> {
    let implementation =
        state
            .implementation
            .as_ref()
            .ok_or(ProtocolViolation::ImplementationContract {
                code: "implementation_state_missing",
            })?;
    let planning = state
        .planning
        .as_ref()
        .ok_or(ProtocolViolation::ImplementationContract {
            code: "implementation_planning_state_missing",
        })?;
    let plan =
        planning
            .accepted_plan
            .as_ref()
            .ok_or(ProtocolViolation::ImplementationContract {
                code: "implementation_accepted_plan_missing",
            })?;

    if let Some(node) = state.active_node() {
        if node.kind != NodeKind::Implementation {
            return Err(ProtocolViolation::WrongPosition {
                node_id: node.id.clone(),
                position: ProtocolStage::Implementation,
            });
        }
        if matches!(node.state, NodeState::Waiting { .. }) {
            return Ok(ProtocolDecision::Wait {
                reason: WaitReason::ActiveNode {
                    node_id: node.id.clone(),
                },
            });
        }
        if let Some(prepared) = implementation.prepared_context_for_node(&node.id) {
            if let Some(decision) = decide_phase5_mutation(state, node, prepared)? {
                return Ok(decision);
            }
            if prepared.manifest.repository_revision != state.repository_revision {
                return Ok(ProtocolDecision::Emit {
                    event: ImplementationEvent::TargetContextSuperseded {
                        supersession: Box::new(TargetContextSupersession::new(
                            prepared,
                            state.repository_revision.clone(),
                        )?),
                    }
                    .into(),
                });
            }
            return Ok(ProtocolDecision::Wait {
                reason: WaitReason::ImplementationContextReady {
                    node_id: node.id.clone(),
                    context_manifest_id: prepared.context_manifest_id.clone(),
                },
            });
        }
        let discovery =
            state
                .discovery
                .as_ref()
                .ok_or(ProtocolViolation::ImplementationContract {
                    code: "implementation_discovery_state_missing",
                })?;
        let request = build_target_context_load_request(
            &state.execution_id,
            state.execution_attempt,
            &state.repository_revision,
            node,
            plan,
            discovery,
        )?;
        return Ok(ProtocolDecision::Perform {
            effect: EffectRequest::Implementation(ImplementationEffectRequest::LoadTargetContext {
                request: Box::new(request),
            }),
        });
    }

    if let Some(result) = state.authoritative_mutation_terminal_result()? {
        return Ok(ProtocolDecision::Finish { result });
    }

    if let Some(node) = state.node_order.iter().find_map(|node_id| {
        state
            .nodes
            .get(node_id)
            .filter(|node| node.kind == NodeKind::Implementation && node.state == NodeState::Ready)
    }) {
        return Ok(ProtocolDecision::Emit {
            event: GraphEvent::NodeStarted {
                node_id: node.id.clone(),
                attempt: node.attempts_started.saturating_add(1),
            }
            .into(),
        });
    }

    let required_implementation = state.required_nodes(NodeKind::Implementation);
    if !required_implementation.is_empty()
        && required_implementation
            .iter()
            .all(|node| matches!(node.state, NodeState::Succeeded { .. }))
    {
        let barrier = state.implementation_barrier_proof()?;
        if state.proofs.get(&barrier.id) != Some(&barrier) {
            return Ok(ProtocolDecision::Emit {
                event: EvidenceEvent::ProofRecorded { proof: barrier }.into(),
            });
        }
        return Ok(ProtocolDecision::Emit {
            event: LifecycleEvent::PositionAdvanced {
                from: ProtocolStage::Implementation,
                to: ProtocolStage::Validation,
                proof_id: barrier.id,
            }
            .into(),
        });
    }

    Ok(ProtocolDecision::Wait {
        reason: WaitReason::NoRunnableNode {
            stage: ProtocolStage::Implementation,
        },
    })
}

fn decide_phase6_validation(state: &ExecutionState) -> Result<ProtocolDecision, ProtocolViolation> {
    let validation = state
        .validation
        .as_ref()
        .ok_or(ProtocolViolation::ValidationContract {
            code: "validation_state_missing",
        })?;
    let policy = state
        .validation_policy
        .as_ref()
        .ok_or(ProtocolViolation::ValidationContract {
            code: "validation_policy_missing",
        })?;

    if state.stage() == ProtocolStage::Repair
        && let Some(result) = state.authoritative_repair_mutation_terminal_result()?
    {
        return Ok(ProtocolDecision::Finish { result });
    }

    if let Some(convergence) = &validation.convergence {
        if let Some(node) = state.active_node() {
            return Ok(ProtocolDecision::Emit {
                event: GraphEvent::NodeFailed {
                    node_id: node.id.clone(),
                    failure_revision_id: convergence.failure_revision_id.clone(),
                    terminal: true,
                }
                .into(),
            });
        }
        return Ok(ProtocolDecision::Finish {
            result: state.authoritative_validation_terminal_result()?,
        });
    }

    match state.stage() {
        ProtocolStage::Validation => decide_phase6_gate(state, validation, policy),
        ProtocolStage::Repair => decide_phase6_repair(state, validation, policy),
        stage => Err(ProtocolViolation::IllegalTransition {
            from: stage,
            to: ProtocolStage::Validation,
        }),
    }
}

fn decide_phase6_gate(
    state: &ExecutionState,
    validation: &ValidationState,
    policy: &ValidationPolicyV1,
) -> Result<ProtocolDecision, ProtocolViolation> {
    if let Some(failure) = validation.current_failure() {
        let node =
            state
                .nodes
                .get(&failure.node_id)
                .ok_or_else(|| ProtocolViolation::UnknownNode {
                    node_id: failure.node_id.clone(),
                })?;
        match &node.state {
            NodeState::Active { .. } => {
                return Ok(ProtocolDecision::Emit {
                    event: GraphEvent::NodeFailed {
                        node_id: node.id.clone(),
                        failure_revision_id: failure.failure_revision_id.clone(),
                        terminal: false,
                    }
                    .into(),
                });
            }
            NodeState::FailedRecoverable {
                failure_revision_id,
            } if failure_revision_id == &failure.failure_revision_id => {
                let proof = state.validation_failure_proof(failure)?;
                if state.proofs.get(&proof.id) != Some(&proof) {
                    return Ok(ProtocolDecision::Emit {
                        event: EvidenceEvent::ProofRecorded { proof }.into(),
                    });
                }
                return Ok(ProtocolDecision::Emit {
                    event: LifecycleEvent::PositionAdvanced {
                        from: ProtocolStage::Validation,
                        to: ProtocolStage::Repair,
                        proof_id: proof.id,
                    }
                    .into(),
                });
            }
            _ => {
                return Err(ProtocolViolation::ValidationContract {
                    code: "validation_failure_node_state_mismatch",
                });
            }
        }
    }

    if let Some(node) = state.active_node() {
        if node.kind != NodeKind::Validation {
            return Err(ProtocolViolation::WrongPosition {
                node_id: node.id.clone(),
                position: ProtocolStage::Validation,
            });
        }
        let failed_evidence = validation
            .node_gates
            .get(&node.id)
            .into_iter()
            .flatten()
            .filter_map(|gate_id| validation.run_for_gate(gate_id))
            .filter_map(|run| run.evidence.as_ref())
            .find(|evidence| {
                matches!(
                    evidence.outcome,
                    ValidationEvidenceOutcome::DomainFailed { .. }
                )
            });
        if let Some(evidence) = failed_evidence {
            let failure = ValidationFailureRevisionV1::from_evidence(evidence)?;
            return Ok(ProtocolDecision::Emit {
                event: ValidationEvent::ValidationFailureRevisionRecorded { failure }.into(),
            });
        }

        let next_gate = validation.next_gate();
        if next_gate.is_none_or(|gate| gate.node_id != node.id) {
            let proof = state.validation_pass_proof(&node.id)?;
            if state.proofs.get(&proof.id) != Some(&proof) {
                return Ok(ProtocolDecision::Emit {
                    event: EvidenceEvent::ProofRecorded { proof }.into(),
                });
            }
            return Ok(ProtocolDecision::Emit {
                event: GraphEvent::NodeSucceeded {
                    node_id: node.id.clone(),
                    proof_id: proof.id,
                }
                .into(),
            });
        }
        let gate = next_gate.expect("active validation gate was checked");
        let Some(run) = validation.run_for_gate(&gate.gate_id) else {
            let run_attempt = validation
                .runs
                .values()
                .filter(|run| run.request.schedule.gate_id == gate.gate_id)
                .count()
                .saturating_add(1) as u32;
            if run_attempt > gate.max_runs {
                let failure_revision_id = FailureRevisionId::new(format!(
                    "epv1:{}",
                    stable_sha256(&[
                        "execution-protocol-v1:validation-run-budget-exhausted",
                        gate.gate_id.as_str(),
                        state.repository_revision.as_str(),
                        &gate.max_runs.to_string(),
                    ])
                ));
                let convergence = ValidationConvergence::new(
                    failure_revision_id,
                    state.repository_revision.clone(),
                    ValidationConvergenceReason::GateRunBudgetExhausted {
                        gate_id: gate.gate_id.clone(),
                    },
                )?;
                return Ok(ProtocolDecision::Emit {
                    event: ValidationEvent::ConvergenceEvaluated { convergence }.into(),
                });
            }
            let kind =
                validation
                    .pending_rerun
                    .as_ref()
                    .map_or(ValidationRunKind::Initial, |rerun| {
                        ValidationRunKind::ExactRepairRerun {
                            failure_revision_id: rerun.failure_revision_id.clone(),
                            repair_intent_id: rerun.repair_intent_id.clone(),
                            verified_repair_evidence_id: rerun.verified_repair_evidence_id.clone(),
                        }
                    });
            let schedule = ValidationRunSchedule::new(
                state.execution_id.clone(),
                state.execution_attempt,
                gate,
                node.attempts_started,
                state.repository_revision.clone(),
                run_attempt,
                kind,
            )?;
            let request = ValidationProcessRequest::new(schedule, gate, policy)?;
            return Ok(ProtocolDecision::Emit {
                event: ValidationEvent::ValidationScheduled { request }.into(),
            });
        };
        if run.started.is_none() && run.completed.is_none() {
            return Ok(ProtocolDecision::Perform {
                effect: EffectRequest::Validation(ValidationEffectRequest::RunProcess {
                    request: Box::new(run.request.clone()),
                }),
            });
        }
        if run.completed.is_none() {
            return Ok(ProtocolDecision::Wait {
                reason: WaitReason::ValidationProcessObservation {
                    run_id: run.request.schedule.run_id.clone(),
                    process_id: run
                        .started
                        .as_ref()
                        .map(|started| started.process_id.clone()),
                },
            });
        }
        let completed = run.completed.as_ref().expect("completion was checked");
        if let ValidationProcessResult::InfrastructureFailure { kind, .. } = completed.result {
            let failure_revision_id = FailureRevisionId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:validation-infrastructure-failure",
                    run.request.schedule.run_id.as_str(),
                    &completed.completion_hash,
                ])
            ));
            let convergence = ValidationConvergence::new(
                failure_revision_id,
                state.repository_revision.clone(),
                ValidationConvergenceReason::InfrastructureFailure {
                    kind,
                    run_id: run.request.schedule.run_id.clone(),
                },
            )?;
            return Ok(ProtocolDecision::Emit {
                event: ValidationEvent::ConvergenceEvaluated { convergence }.into(),
            });
        }
        return Ok(ProtocolDecision::Wait {
            reason: WaitReason::ValidationProcessObservation {
                run_id: run.request.schedule.run_id.clone(),
                process_id: run
                    .started
                    .as_ref()
                    .map(|started| started.process_id.clone()),
            },
        });
    }

    if let Some(gate) = validation.next_gate() {
        let node =
            state
                .nodes
                .get(&gate.node_id)
                .ok_or_else(|| ProtocolViolation::UnknownNode {
                    node_id: gate.node_id.clone(),
                })?;
        if node.state != NodeState::Ready {
            return Err(ProtocolViolation::ValidationContract {
                code: "canonical_validation_gate_not_ready",
            });
        }
        return Ok(ProtocolDecision::Emit {
            event: GraphEvent::NodeStarted {
                node_id: node.id.clone(),
                attempt: node.attempts_started.saturating_add(1),
            }
            .into(),
        });
    }

    let proof = state.required_validation_proof()?;
    if state.proofs.get(&proof.id) != Some(&proof) {
        return Ok(ProtocolDecision::Emit {
            event: EvidenceEvent::ProofRecorded { proof }.into(),
        });
    }
    Ok(ProtocolDecision::Emit {
        event: LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Validation,
            to: ProtocolStage::Review,
            proof_id: proof.id,
        }
        .into(),
    })
}

fn decide_phase6_repair(
    state: &ExecutionState,
    validation: &ValidationState,
    policy: &ValidationPolicyV1,
) -> Result<ProtocolDecision, ProtocolViolation> {
    if let Some(rerun) = &validation.pending_rerun {
        let proof = state.validation_rerun_proof(rerun)?;
        if state.proofs.get(&proof.id) != Some(&proof) {
            return Ok(ProtocolDecision::Emit {
                event: EvidenceEvent::ProofRecorded { proof }.into(),
            });
        }
        return Ok(ProtocolDecision::Emit {
            event: LifecycleEvent::PositionAdvanced {
                from: ProtocolStage::Repair,
                to: ProtocolStage::Validation,
                proof_id: proof.id,
            }
            .into(),
        });
    }
    let failure = validation
        .current_failure()
        .ok_or(ProtocolViolation::ValidationContract {
            code: "repair_without_active_validation_failure",
        })?;
    let evidence = validation
        .evidence
        .get(&failure.validation_evidence_id)
        .ok_or(ProtocolViolation::ValidationContract {
            code: "repair_failure_evidence_missing",
        })?;
    let plan = state
        .planning
        .as_ref()
        .and_then(|planning| planning.accepted_plan.as_ref())
        .ok_or(ProtocolViolation::ValidationContract {
            code: "repair_accepted_plan_missing",
        })?;
    let profile =
        state
            .repository_profile
            .as_ref()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "repair_repository_profile_missing",
            })?;
    let gate =
        validation
            .gates
            .get(&failure.gate_id)
            .ok_or(ProtocolViolation::ValidationContract {
                code: "repair_originating_gate_missing",
            })?;
    if let Some(exhausted_gate) = validation_repair_rerun_budget_exhausted_gate(validation) {
        let convergence =
            validation_gate_run_budget_convergence(exhausted_gate, &state.repository_revision)?;
        return Ok(ProtocolDecision::Emit {
            event: ValidationEvent::ConvergenceEvaluated { convergence }.into(),
        });
    }

    let ranking = if let Some(ranking) = validation.rankings.get(&failure.failure_revision_id) {
        ranking
    } else {
        let relationships = state
            .discovery
            .as_ref()
            .map(|discovery| &discovery.relationships)
            .ok_or(ProtocolViolation::ValidationContract {
                code: "repair_discovery_evidence_missing",
            })?;
        let ranking = rank_repair_candidates(failure, evidence, plan, relationships)?;
        return Ok(ProtocolDecision::Emit {
            event: ValidationEvent::RepairCandidatesRanked { ranking }.into(),
        });
    };
    let baselines = state.repair_mutation_baselines(failure);
    let evaluation =
        if let Some(evaluation) = validation.eligibility.get(&failure.failure_revision_id) {
            evaluation
        } else {
            let evaluation = evaluate_repair_eligibility(
                ranking, failure, evidence, plan, profile, policy, &baselines,
            )?;
            return Ok(ProtocolDecision::Emit {
                event: ValidationEvent::RepairEligibilityEvaluated { evaluation }.into(),
            });
        };
    let selection = if let Some(selection) = validation.selections.get(&failure.failure_revision_id)
    {
        selection
    } else {
        let Some(selection) =
            select_repair_target(ranking, evaluation, failure, gate, plan, policy, &baselines)?
        else {
            let convergence = ValidationConvergence::new(
                failure.failure_revision_id.clone(),
                state.repository_revision.clone(),
                ValidationConvergenceReason::NoValidRepair,
            )?;
            return Ok(ProtocolDecision::Emit {
                event: ValidationEvent::ConvergenceEvaluated { convergence }.into(),
            });
        };
        return Ok(ProtocolDecision::Emit {
            event: ValidationEvent::RepairTargetSelected { selection }.into(),
        });
    };

    let eligibility_proof = state.repair_eligibility_proof(selection)?;
    if state.proofs.get(&eligibility_proof.id) != Some(&eligibility_proof) {
        return Ok(ProtocolDecision::Emit {
            event: EvidenceEvent::ProofRecorded {
                proof: eligibility_proof,
            }
            .into(),
        });
    }
    if !state.nodes.contains_key(&selection.repair_node.id) {
        return Ok(ProtocolDecision::Emit {
            event: GraphEvent::ValidationRepairNodeAdded {
                eligibility_proof_id: eligibility_proof.id,
                node: selection.repair_node.clone(),
            }
            .into(),
        });
    }
    if let Some(node) = state.active_node() {
        if node.kind != NodeKind::ValidationRepair || node.id != selection.repair_node.id {
            return Err(ProtocolViolation::ValidationContract {
                code: "repair_active_owner_mismatch",
            });
        }
        if matches!(node.state, NodeState::Waiting { .. }) {
            return Ok(ProtocolDecision::Wait {
                reason: WaitReason::ActiveNode {
                    node_id: node.id.clone(),
                },
            });
        }
        if let Some(prepared) = validation
            .repair_contexts
            .prepared_context_for_node(&node.id)
        {
            if let Some(decision) = decide_phase5_mutation(state, node, prepared)? {
                return Ok(decision);
            }
            return Ok(ProtocolDecision::Wait {
                reason: WaitReason::ImplementationContextReady {
                    node_id: node.id.clone(),
                    context_manifest_id: prepared.context_manifest_id.clone(),
                },
            });
        }
        let baseline = baselines.get(&selection.intent.target_id).ok_or(
            ProtocolViolation::ValidationContract {
                code: "repair_target_context_baseline_missing",
            },
        )?;
        let discovery = state
            .discovery
            .as_ref()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "repair_target_context_discovery_missing",
            })?;
        let request = build_validation_repair_target_context_load_request(
            &state.execution_id,
            state.execution_attempt,
            &state.repository_revision,
            node,
            selection,
            failure,
            baseline,
            plan,
            discovery,
        )?;
        return Ok(ProtocolDecision::Perform {
            effect: EffectRequest::Validation(ValidationEffectRequest::LoadRepairTargetContext {
                request: Box::new(request),
            }),
        });
    }
    let node = state.nodes.get(&selection.repair_node.id).ok_or_else(|| {
        ProtocolViolation::UnknownNode {
            node_id: selection.repair_node.id.clone(),
        }
    })?;
    if let NodeState::Succeeded { proof_id } = &node.state {
        state
            .proofs
            .get(proof_id)
            .ok_or_else(|| ProtocolViolation::UnknownProof {
                proof_id: proof_id.clone(),
            })?;
        if !validation
            .invalidations
            .contains_key(&failure.failure_revision_id)
        {
            let invalidation = state.authoritative_repair_invalidation(failure, selection)?;
            return Ok(ProtocolDecision::Emit {
                event: ValidationEvent::PriorValidationInvalidated { invalidation }.into(),
            });
        }
        if !validation.reruns.contains_key(&failure.failure_revision_id) {
            let invalidation = validation
                .invalidations
                .get(&failure.failure_revision_id)
                .expect("repair invalidation presence was checked");
            return Ok(ProtocolDecision::Emit {
                event: ValidationEvent::ValidationRerunScheduled {
                    rerun: ValidationRerunSchedule::new(invalidation, selection, gate)?,
                }
                .into(),
            });
        }
    }
    if node.state == NodeState::Ready {
        return Ok(ProtocolDecision::Emit {
            event: GraphEvent::NodeStarted {
                node_id: node.id.clone(),
                attempt: node.attempts_started.saturating_add(1),
            }
            .into(),
        });
    }
    Ok(ProtocolDecision::Wait {
        reason: WaitReason::NoRunnableNode {
            stage: ProtocolStage::Repair,
        },
    })
}

fn validation_gate_run_budget_convergence(
    gate: &ValidationGateV1,
    repository_revision: &RepositoryRevisionId,
) -> Result<ValidationConvergence, ProtocolViolation> {
    let failure_revision_id = FailureRevisionId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:validation-run-budget-exhausted",
            gate.gate_id.as_str(),
            repository_revision.as_str(),
            &gate.max_runs.to_string(),
        ])
    ));
    Ok(ValidationConvergence::new(
        failure_revision_id,
        repository_revision.clone(),
        ValidationConvergenceReason::GateRunBudgetExhausted {
            gate_id: gate.gate_id.clone(),
        },
    )?)
}

fn review_convergence_failure_revision(convergence: &ReviewConvergenceV1) -> FailureRevisionId {
    FailureRevisionId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:review-convergence-failure-revision",
            convergence.convergence_id.as_str(),
            &convergence.convergence_hash,
        ])
    ))
}

fn publication_convergence_failure_revision(
    convergence: &PublicationConvergenceV1,
) -> FailureRevisionId {
    FailureRevisionId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:publication-convergence-failure-revision",
            convergence.convergence_id.as_str(),
            &convergence.convergence_hash,
        ])
    ))
}

fn review_blocked_terminal(
    code: &'static str,
    node_id: Option<NodeId>,
) -> (MissionResult, ProcessHealth, &'static str) {
    (
        MissionResult::BlockedNoDiff {
            failure: FirstFatalBlocker {
                category: "review".into(),
                code: code.into(),
                node_id,
            },
        },
        ProcessHealth::Healthy,
        code,
    )
}

fn review_infrastructure_terminal(
    code: &'static str,
    node_id: Option<NodeId>,
) -> (MissionResult, ProcessHealth, &'static str) {
    (
        MissionResult::InfrastructureFailed {
            failure: FirstFatalBlocker {
                category: "infrastructure".into(),
                code: code.into(),
                node_id,
            },
        },
        ProcessHealth::Failed { code: code.into() },
        code,
    )
}

fn external_review_reason_code(
    completion: &CompletionEvaluationV1,
) -> Result<String, ProtocolViolation> {
    let external = completion
        .criteria
        .iter()
        .filter_map(|(criterion_id, evaluation)| match &evaluation.status {
            CriterionCompletionStatusV1::ExternalReviewRequired {
                kind,
                requirement_code,
                detail_hash,
            } => Some((criterion_id, kind, requirement_code, detail_hash)),
            CriterionCompletionStatusV1::Satisfied { .. }
            | CriterionCompletionStatusV1::Unsatisfied { .. }
            | CriterionCompletionStatusV1::Uncertain { .. } => None,
        })
        .collect::<Vec<_>>();
    if external.is_empty() {
        return Err(ProtocolViolation::PublicationContract {
            code: "publication_external_review_authority_missing",
        });
    }
    let encoded =
        serde_json::to_vec(&external).map_err(|error| ProtocolViolation::EventSerialization {
            detail: error.to_string(),
        })?;
    Ok(format!(
        "external_review_required:{}",
        stable_sha256(&[
            "execution-protocol-v1:external-review-reason",
            &hex::encode(encoded),
        ])
    ))
}

fn publication_eligibility_denial_is_stale(eligibility: &PublicationEligibilityRecord) -> bool {
    let PublicationEligibilityDispositionV1::Denied { failed_predicates } =
        &eligibility.disposition
    else {
        return false;
    };
    !failed_predicates.is_empty()
        && failed_predicates.iter().all(|predicate| {
            matches!(
                predicate,
                PublicationPredicateV1::CurrentRepositoryRevision
                    | PublicationPredicateV1::RequiredValidationCurrent
                    | PublicationPredicateV1::NoActiveValidationFailure
                    | PublicationPredicateV1::RemoteHeadUnchanged
            )
        })
}

fn authoritative_publication_step(publication: &PublicationStateV1) -> PublicationStep {
    let Some(last) = publication.attempts.last() else {
        return PublicationStep::Commit;
    };
    match (&last.attempt.operation, last.observation.as_ref()) {
        (
            PublicationOperationV1::Commit,
            Some(PublicationAttemptObservationV1::Commit(CommitObservationV1 {
                outcome: CommitOutcomeV1::Confirmed { .. },
                ..
            })),
        ) => PublicationStep::Push,
        (
            PublicationOperationV1::Push,
            Some(PublicationAttemptObservationV1::Push(ExactLeasePushObservationV1 {
                outcome: ExactLeasePushOutcomeV1::Confirmed { .. },
                ..
            })),
        ) => PublicationStep::PullRequest,
        (PublicationOperationV1::Commit, _) => PublicationStep::Commit,
        (PublicationOperationV1::Push, _) => PublicationStep::Push,
        (PublicationOperationV1::PullRequest, _) => PublicationStep::PullRequest,
    }
}

fn validation_repair_rerun_budget_exhausted_gate(
    validation: &ValidationState,
) -> Option<&ValidationGateV1> {
    validation.gate_order.iter().find_map(|gate_id| {
        let gate = validation.gates.get(gate_id)?;
        let completed_runs = validation
            .runs
            .values()
            .filter(|run| run.request.schedule.gate_id == *gate_id)
            .count() as u32;
        (completed_runs >= gate.max_runs).then_some(gate)
    })
}

fn decide_phase5_mutation(
    state: &ExecutionState,
    node: &ExecutionNode,
    prepared_context: &PreparedTargetContext,
) -> Result<Option<ProtocolDecision>, ProtocolViolation> {
    let context = &prepared_context.manifest;
    if let Some(mutation_target) = state.mutation.current_target(&node.id) {
        if let Some(convergence) = &mutation_target.readiness_convergence {
            return Ok(Some(ProtocolDecision::Emit {
                event: GraphEvent::NodeFailed {
                    node_id: node.id.clone(),
                    failure_revision_id: convergence.failure_revision_id.clone(),
                    terminal: true,
                }
                .into(),
            }));
        }
        if let Some(convergence) = &mutation_target.convergence {
            return Ok(Some(ProtocolDecision::Emit {
                event: GraphEvent::NodeFailed {
                    node_id: node.id.clone(),
                    failure_revision_id: convergence.last_failure_revision_id.clone(),
                    terminal: true,
                }
                .into(),
            }));
        }
    }
    let (_, target, authoritative_context) =
        state.mutation_binding(&node.id, &context.context_manifest_id)?;
    if authoritative_context != context {
        return Err(ProtocolViolation::MutationContract {
            code: "mutation_prepared_context_not_authoritative",
        });
    }
    if context.repository_revision != state.repository_revision {
        let has_pending_verification = state.event_log.iter().any(|stored| {
            matches!(
                &stored.envelope.payload,
                DomainEvent::Mutation(MutationEvent::MutationVerified { evidence })
                    if evidence.node_id == node.id
                        && evidence.context_manifest_id == context.context_manifest_id
                        && evidence.repository_revision_after == state.repository_revision
            )
        });
        if !has_pending_verification {
            return Ok(None);
        }
    }
    let feasibility =
        state
            .event_log
            .iter()
            .rev()
            .find_map(|stored| match &stored.envelope.payload {
                DomainEvent::Mutation(MutationEvent::FeasibilityEvaluated { feasibility })
                    if feasibility.node_id == node.id
                        && feasibility.context_manifest_id == context.context_manifest_id =>
                {
                    Some(feasibility)
                }
                _ => None,
            });
    let Some(feasibility) = feasibility else {
        return Ok(Some(ProtocolDecision::Emit {
            event: MutationEvent::FeasibilityEvaluated {
                feasibility: evaluate_mutation_feasibility(node, &target, context)?,
            }
            .into(),
        }));
    };
    let mutation_target =
        state
            .mutation
            .current_target(&node.id)
            .ok_or(ProtocolViolation::MutationContract {
                code: "mutation_current_target_missing",
            })?;
    if mutation_target.context_manifest_id != context.context_manifest_id {
        return Err(ProtocolViolation::MutationContract {
            code: "mutation_current_target_context_mismatch",
        });
    }
    if feasibility.feasible_strategies().is_empty() {
        return Ok(Some(ProtocolDecision::Emit {
            event: MutationEvent::ReadinessConvergenceEvaluated {
                convergence: MutationReadinessConvergence::no_feasible_strategy(
                    &state.execution_id,
                    state.execution_attempt,
                    feasibility,
                )?,
            }
            .into(),
        }));
    }
    let policy = state
        .event_log
        .iter()
        .rev()
        .find_map(|stored| match &stored.envelope.payload {
            DomainEvent::Mutation(MutationEvent::AttemptPolicySelected { policy })
                if policy.node_id == node.id
                    && policy.context_manifest_id == context.context_manifest_id =>
            {
                Some(policy)
            }
            _ => None,
        });
    let Some(policy) = policy else {
        let previous =
            state
                .event_log
                .iter()
                .rev()
                .find_map(|stored| match &stored.envelope.payload {
                    DomainEvent::Mutation(MutationEvent::AttemptPolicySelected { policy })
                        if policy.node_id == node.id =>
                    {
                        Some(policy)
                    }
                    _ => None,
                });
        let policy = if let Some(previous) = previous {
            let previous_context = state
                .prepared_mutation_context(&previous.context_manifest_id)
                .ok_or(ProtocolViolation::MutationContract {
                    code: "mutation_rebuilt_previous_context_missing",
                })?;
            let failure = state.mutation_failure(&previous.attempt_id)?;
            select_rebuilt_mutation_policy(
                &state.execution_id,
                state.execution_attempt,
                node,
                &target,
                context,
                feasibility,
                &previous_context.manifest,
                previous,
                failure,
            )?
        } else {
            select_initial_mutation_policy(
                &state.execution_id,
                state.execution_attempt,
                node,
                &target,
                context,
                feasibility,
            )?
        };
        return Ok(Some(ProtocolDecision::Emit {
            event: MutationEvent::AttemptPolicySelected { policy }.into(),
        }));
    };
    let failure = state
        .event_log
        .iter()
        .rev()
        .find_map(|stored| match &stored.envelope.payload {
            DomainEvent::Mutation(
                MutationEvent::ActionRejected { failure }
                | MutationEvent::AttemptFailed { failure },
            ) if failure.attempt_id == policy.attempt_id => Some(failure),
            _ => None,
        });
    if let Some(failure) = failure {
        return match select_mutation_recovery(node, &target, context, feasibility, policy, failure)?
        {
            MutationRecoveryDecision::ModelRetry { policy }
            | MutationRecoveryDecision::SelectFallback { policy } => {
                Ok(Some(ProtocolDecision::Emit {
                    event: MutationEvent::AttemptPolicySelected { policy }.into(),
                }))
            }
            MutationRecoveryDecision::RebuildContext { drift } => {
                if node.kind == NodeKind::ValidationRepair {
                    Ok(Some(ProtocolDecision::Emit {
                        event: MutationEvent::ConvergenceEvaluated {
                            convergence: MutationConvergence::new(
                                policy,
                                failure,
                                MutationConvergenceReason::ContextRebuildUnavailable,
                            )?,
                        }
                        .into(),
                    }))
                } else {
                    Ok(Some(ProtocolDecision::Emit {
                        event: ImplementationEvent::TargetContextSuperseded {
                            supersession: Box::new(TargetContextSupersession::new(
                                prepared_context,
                                drift.observed_revision,
                            )?),
                        }
                        .into(),
                    }))
                }
            }
            MutationRecoveryDecision::NoSafeFallback { reason, .. } => {
                Ok(Some(ProtocolDecision::Emit {
                    event: MutationEvent::ConvergenceEvaluated {
                        convergence: MutationConvergence::new(policy, failure, reason)?,
                    }
                    .into(),
                }))
            }
        };
    }
    let attempt = mutation_target
        .attempts
        .get(&policy.attempt_index)
        .filter(|attempt| attempt.policy.attempt_id == policy.attempt_id)
        .ok_or(ProtocolViolation::MutationContract {
            code: "mutation_current_attempt_missing",
        })?;
    let candidate = attempt.candidate.as_ref();
    if candidate.is_none() {
        let Some(prepared) = attempt.active_action() else {
            let released_actions = attempt.released_action_count();
            if released_actions >= MUTATION_UNCONTACTED_RELEASE_RETRY_LIMIT.saturating_add(1) {
                let last_released_action_id = attempt
                    .last_released_action_id()
                    .ok_or(ProtocolViolation::MutationContract {
                        code: "mutation_last_released_action_missing",
                    })?
                    .clone();
                return Ok(Some(ProtocolDecision::Emit {
                    event: MutationEvent::ReadinessConvergenceEvaluated {
                        convergence:
                            MutationReadinessConvergence::uncontacted_action_retry_exhausted(
                                policy,
                                feasibility,
                                released_actions,
                                last_released_action_id,
                            )?,
                    }
                    .into(),
                }));
            }
            let remaining = state.planning_budget_remaining(&node.id)?;
            let admission_remaining = MutationAdmissionBudgetRemaining::new(
                remaining.model_calls,
                remaining.cost_micros,
                remaining.duration_ms,
            );
            if admission_remaining.is_exhausted() {
                return Ok(Some(ProtocolDecision::Emit {
                    event: MutationEvent::ReadinessConvergenceEvaluated {
                        convergence: MutationReadinessConvergence::admission_budget_exhausted(
                            policy,
                            feasibility,
                            admission_remaining,
                        )?,
                    }
                    .into(),
                }));
            }
            let (action_index, prior_released_action_id) =
                attempt
                    .next_action_binding()?
                    .ok_or(ProtocolViolation::MutationContract {
                        code: "mutation_action_retry_not_authorized",
                    })?;
            return Ok(Some(ProtocolDecision::Emit {
                event: MutationEvent::ActionPrepared {
                    prepared: Box::new(build_prepared_mutation_action_retry(
                        node,
                        &target,
                        context,
                        feasibility,
                        policy.clone(),
                        action_index,
                        prior_released_action_id,
                        remaining.cost_micros,
                        remaining.duration_ms,
                    )?),
                }
                .into(),
            }));
        };
        return Ok(Some(
            match state
                .budgets
                .model_calls
                .get(&prepared.admission.call_id)
                .map(|record| &record.state)
            {
                None => ProtocolDecision::Emit {
                    event: BudgetEvent::ModelCallAdmitted {
                        admission: prepared.admission.clone(),
                    }
                    .into(),
                },
                Some(ModelCallState::Admitted) => ProtocolDecision::Emit {
                    event: BudgetEvent::ModelCallReserved {
                        call_id: prepared.admission.call_id.clone(),
                    }
                    .into(),
                },
                Some(ModelCallState::Reserved) => ProtocolDecision::Perform {
                    effect: EffectRequest::Mutation(MutationEffectRequest::DispatchProvider {
                        request: Box::new(prepared.provider_request.clone()),
                    }),
                },
                Some(ModelCallState::Dispatched) => ProtocolDecision::Wait {
                    reason: WaitReason::ProviderReconciliation {
                        call_id: prepared.admission.call_id.clone(),
                    },
                },
                Some(ModelCallState::ReconciledConsumed { .. }) => ProtocolDecision::Wait {
                    reason: WaitReason::MutationObservation {
                        action_id: prepared.provider_request.action_id.clone(),
                    },
                },
                Some(ModelCallState::ReconciledReleased) => ProtocolDecision::Emit {
                    event: MutationEvent::ActionReleased {
                        action_id: prepared.provider_request.action_id.clone(),
                    }
                    .into(),
                },
            },
        ));
    }
    let candidate = candidate.expect("candidate presence was checked");
    let prepared = attempt
        .actions
        .values()
        .find(|action| {
            action.prepared.provider_request.action_id == candidate.action_id
                && !action.released_uncontacted
        })
        .map(|action| &action.prepared)
        .ok_or(ProtocolViolation::MutationContract {
            code: "mutation_candidate_action_missing",
        })?;
    let application =
        state
            .event_log
            .iter()
            .rev()
            .find_map(|stored| match &stored.envelope.payload {
                DomainEvent::Mutation(MutationEvent::ApplicationObserved {
                    request,
                    observation,
                }) if request.attempt_id == policy.attempt_id => Some((request, observation)),
                _ => None,
            });
    let Some((apply, observation)) = application else {
        return Ok(Some(ProtocolDecision::Perform {
            effect: EffectRequest::Mutation(MutationEffectRequest::ApplyMutation {
                request: Box::new(MutationApplyRequest::new(
                    prepared, candidate, &target, context,
                )?),
            }),
        }));
    };
    let verified = state
        .event_log
        .iter()
        .rev()
        .find_map(|stored| match &stored.envelope.payload {
            DomainEvent::Mutation(MutationEvent::MutationVerified { evidence })
                if evidence.attempt_id == policy.attempt_id =>
            {
                Some(evidence)
            }
            _ => None,
        });
    let Some(evidence) = verified else {
        return Ok(Some(ProtocolDecision::Perform {
            effect: EffectRequest::Mutation(MutationEffectRequest::VerifyMutation {
                request: Box::new(MutationVerifyRequest::new(apply, observation)?),
            }),
        }));
    };
    let mutation_proof = state.mutation_verification_proof(evidence)?;
    if state.proofs.get(&mutation_proof.id) != Some(&mutation_proof) {
        return Ok(Some(ProtocolDecision::Emit {
            event: EvidenceEvent::ProofRecorded {
                proof: mutation_proof,
            }
            .into(),
        }));
    }
    let proof = if node.kind == NodeKind::ValidationRepair {
        state.repair_verification_proof(evidence)?
    } else {
        mutation_proof
    };
    if state.proofs.get(&proof.id) != Some(&proof) {
        return Ok(Some(ProtocolDecision::Emit {
            event: EvidenceEvent::ProofRecorded { proof }.into(),
        }));
    }
    Ok(Some(ProtocolDecision::Emit {
        event: GraphEvent::NodeSucceeded {
            node_id: node.id.clone(),
            proof_id: proof.id,
        }
        .into(),
    }))
}

fn decide_phase3_planning(state: &ExecutionState) -> Result<ProtocolDecision, ProtocolViolation> {
    let planning = state
        .planning
        .as_ref()
        .ok_or(ProtocolViolation::PlanningContract {
            code: "planning_state_missing",
        })?;
    let node =
        state
            .nodes
            .get(&planning.node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: planning.node_id.clone(),
            })?;

    if let Some(prepared) = &state.current_planning_action {
        return match state
            .budgets
            .model_calls
            .get(&prepared.admission.call_id)
            .map(|record| &record.state)
        {
            None => Ok(ProtocolDecision::Emit {
                event: BudgetEvent::ModelCallAdmitted {
                    admission: prepared.admission.clone(),
                }
                .into(),
            }),
            Some(ModelCallState::Admitted) => Ok(ProtocolDecision::Emit {
                event: BudgetEvent::ModelCallReserved {
                    call_id: prepared.admission.call_id.clone(),
                }
                .into(),
            }),
            Some(ModelCallState::Reserved) => Ok(ProtocolDecision::Perform {
                effect: EffectRequest::Planning(PlanningEffectRequest::DispatchProvider {
                    envelope: Box::new(prepared.envelope.clone()),
                }),
            }),
            Some(ModelCallState::Dispatched) => Ok(ProtocolDecision::Wait {
                reason: WaitReason::ProviderReconciliation {
                    call_id: prepared.admission.call_id.clone(),
                },
            }),
            Some(ModelCallState::ReconciledConsumed { .. }) => Ok(ProtocolDecision::Wait {
                reason: WaitReason::PlanningObservation {
                    action_id: prepared.envelope.action_id.clone(),
                },
            }),
            Some(ModelCallState::ReconciledReleased) => Ok(ProtocolDecision::Emit {
                event: PlanningEvent::ActionReleased {
                    action_id: prepared.envelope.action_id.clone(),
                }
                .into(),
            }),
        };
    }

    match &node.state {
        NodeState::Ready => {
            return Ok(ProtocolDecision::Emit {
                event: GraphEvent::NodeStarted {
                    node_id: node.id.clone(),
                    attempt: node.attempts_started.saturating_add(1),
                }
                .into(),
            });
        }
        NodeState::Succeeded { proof_id } => {
            if planning.accepted_no_op.is_some() {
                return Ok(ProtocolDecision::Finish {
                    result: CanonicalResult {
                        mission: MissionResult::SucceededNoOp {
                            no_op_proof_id: proof_id.clone(),
                        },
                        process_health: ProcessHealth::Healthy,
                        reason_code: "planning_proved_no_op".into(),
                        repository_revision: state.repository_revision.clone(),
                        remaining_work: state.unresolved_required_nodes().into_iter().collect(),
                    },
                });
            }
            return Ok(ProtocolDecision::Emit {
                event: LifecycleEvent::PositionAdvanced {
                    from: ProtocolStage::Planning,
                    to: ProtocolStage::Implementation,
                    proof_id: proof_id.clone(),
                }
                .into(),
            });
        }
        NodeState::FailedTerminal { .. } => {
            let convergence =
                planning
                    .convergence
                    .as_ref()
                    .ok_or(ProtocolViolation::PlanningContract {
                        code: "failed_planning_has_no_convergence",
                    })?;
            return Ok(ProtocolDecision::Finish {
                result: planning_terminal_result(state, planning, convergence),
            });
        }
        NodeState::Active { .. } => {}
        _ => {
            return Ok(ProtocolDecision::Wait {
                reason: WaitReason::NoRunnableNode {
                    stage: ProtocolStage::Planning,
                },
            });
        }
    }

    if let Some(plan) = &planning.accepted_plan {
        let proof = state.proofs.values().find(|proof| {
            proof.kind == ProofKind::PlanAccepted
                && proof.node_ids == [planning.node_id.clone()]
                && proof.detail_hash == plan_accepted_proof_hash(plan)
        });
        let Some(proof) = proof else {
            return Ok(ProtocolDecision::Emit {
                event: EvidenceEvent::ProofRecorded {
                    proof: state.planning_acceptance_proof(plan),
                }
                .into(),
            });
        };
        let graph_was_materialized = state.event_log.iter().any(|stored| {
            matches!(
                stored.envelope.payload,
                DomainEvent::Graph(GraphEvent::NodesAdded { ref plan_proof_id, .. })
                    if plan_proof_id == &proof.id
            )
        });
        if !graph_was_materialized {
            return Ok(ProtocolDecision::Emit {
                event: GraphEvent::NodesAdded {
                    plan_proof_id: proof.id.clone(),
                    nodes: state.materialized_planning_nodes(plan)?,
                }
                .into(),
            });
        }
        return Ok(ProtocolDecision::Emit {
            event: GraphEvent::NodeSucceeded {
                node_id: planning.node_id.clone(),
                proof_id: proof.id.clone(),
            }
            .into(),
        });
    }

    if let Some(no_op) = &planning.accepted_no_op {
        let proof = state.proofs.values().find(|proof| {
            proof.kind == ProofKind::NoOpSatisfied
                && proof.node_ids == [planning.node_id.clone()]
                && proof.detail_hash == no_op_satisfied_proof_hash(no_op)
        });
        let Some(proof) = proof else {
            return Ok(ProtocolDecision::Emit {
                event: EvidenceEvent::ProofRecorded {
                    proof: state.planning_no_op_proof(no_op),
                }
                .into(),
            });
        };
        return Ok(ProtocolDecision::Emit {
            event: GraphEvent::NodeSucceeded {
                node_id: planning.node_id.clone(),
                proof_id: proof.id.clone(),
            }
            .into(),
        });
    }

    if let Some(convergence) = &planning.convergence {
        let serialized = serde_json::to_string(convergence).map_err(|error| {
            ProtocolViolation::EventSerialization {
                detail: error.to_string(),
            }
        })?;
        return Ok(ProtocolDecision::Emit {
            event: GraphEvent::NodeFailed {
                node_id: planning.node_id.clone(),
                failure_revision_id: FailureRevisionId::new(format!(
                    "epv1:{}",
                    stable_sha256(&[
                        "execution-protocol-v1:planning-failure",
                        state.execution_id.as_str(),
                        &serialized,
                    ])
                )),
                terminal: true,
            }
            .into(),
        });
    }

    let remaining = state.planning_budget_remaining(&planning.node_id)?;
    if remaining.is_exhausted() {
        return Ok(ProtocolDecision::Emit {
            event: PlanningEvent::ConvergenceEvaluated {
                convergence: state.authoritative_planning_convergence(planning)?,
            }
            .into(),
        });
    }
    Ok(ProtocolDecision::Emit {
        event: PlanningEvent::ActionPrepared {
            prepared: Box::new(build_prepared_planning_action(state)?),
        }
        .into(),
    })
}

fn planning_terminal_result(
    state: &ExecutionState,
    planning: &PlanningState,
    convergence: &PlanningConvergence,
) -> CanonicalResult {
    let (mission, reason_code) = match convergence {
        PlanningConvergence::InsufficientEvidence { .. } => (
            MissionResult::InsufficientEvidence {
                failure: FirstFatalBlocker {
                    category: "planning".into(),
                    code: "planning_insufficient_evidence".into(),
                    node_id: Some(planning.node_id.clone()),
                },
            },
            "planning_insufficient_evidence",
        ),
        PlanningConvergence::BudgetBlocked { .. } => (
            MissionResult::BudgetBlocked {
                node_id: planning.node_id.clone(),
                failure: FirstFatalBlocker {
                    category: "planning".into(),
                    code: "planning_budget_exhausted".into(),
                    node_id: Some(planning.node_id.clone()),
                },
            },
            "planning_budget_exhausted",
        ),
    };
    CanonicalResult {
        mission,
        process_health: ProcessHealth::Healthy,
        reason_code: reason_code.into(),
        repository_revision: state.repository_revision.clone(),
        remaining_work: state.unresolved_required_nodes().into_iter().collect(),
    }
}

fn decide_phase2_discovery(state: &ExecutionState) -> Result<ProtocolDecision, ProtocolViolation> {
    let discovery = state
        .discovery
        .as_ref()
        .ok_or(ProtocolViolation::DiscoveryContract {
            code: "discovery_state_missing",
        })?;
    let node =
        state
            .nodes
            .get(&discovery.node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: discovery.node_id.clone(),
            })?;

    if let Some(prepared) = &state.current_discovery_action {
        return match state
            .budgets
            .model_calls
            .get(&prepared.admission.call_id)
            .map(|record| &record.state)
        {
            None => Ok(ProtocolDecision::Emit {
                event: BudgetEvent::ModelCallAdmitted {
                    admission: prepared.admission.clone(),
                }
                .into(),
            }),
            Some(ModelCallState::Admitted) => Ok(ProtocolDecision::Emit {
                event: BudgetEvent::ModelCallReserved {
                    call_id: prepared.admission.call_id.clone(),
                }
                .into(),
            }),
            Some(ModelCallState::Reserved) => Ok(ProtocolDecision::Perform {
                effect: EffectRequest::Discovery(DiscoveryEffectRequest::DispatchProvider {
                    envelope: Box::new(prepared.envelope.clone()),
                }),
            }),
            Some(ModelCallState::Dispatched) => Ok(ProtocolDecision::Wait {
                reason: WaitReason::ProviderReconciliation {
                    call_id: prepared.admission.call_id.clone(),
                },
            }),
            Some(ModelCallState::ReconciledConsumed { .. }) => Ok(ProtocolDecision::Wait {
                reason: WaitReason::DiscoveryObservation {
                    action_id: prepared.envelope.action_id.clone(),
                },
            }),
            Some(ModelCallState::ReconciledReleased) => Ok(ProtocolDecision::Emit {
                event: DiscoveryEvent::ActionReleased {
                    action_id: prepared.envelope.action_id.clone(),
                }
                .into(),
            }),
        };
    }

    match &node.state {
        NodeState::Ready => {
            return Ok(ProtocolDecision::Emit {
                event: GraphEvent::NodeStarted {
                    node_id: node.id.clone(),
                    attempt: node.attempts_started.saturating_add(1),
                }
                .into(),
            });
        }
        NodeState::Succeeded { proof_id } => {
            return Ok(ProtocolDecision::Emit {
                event: LifecycleEvent::PositionAdvanced {
                    from: ProtocolStage::Discovery,
                    to: ProtocolStage::Planning,
                    proof_id: proof_id.clone(),
                }
                .into(),
            });
        }
        NodeState::FailedTerminal { .. } => {
            let convergence =
                discovery
                    .convergence
                    .as_ref()
                    .ok_or(ProtocolViolation::DiscoveryContract {
                        code: "failed_discovery_has_no_convergence",
                    })?;
            let (mission, reason_code) = discovery_terminal_result(state, convergence)?;
            return Ok(ProtocolDecision::Finish {
                result: CanonicalResult {
                    mission,
                    process_health: ProcessHealth::Healthy,
                    reason_code: reason_code.into(),
                    repository_revision: state.repository_revision.clone(),
                    remaining_work: state.unresolved_required_nodes().into_iter().collect(),
                },
            });
        }
        NodeState::Active { .. } => {}
        _ => {
            return Ok(ProtocolDecision::Wait {
                reason: WaitReason::NoRunnableNode {
                    stage: ProtocolStage::Discovery,
                },
            });
        }
    }

    if let Some((search_id, candidates)) = next_candidate_projection(discovery)? {
        return Ok(ProtocolDecision::Emit {
            event: DiscoveryEvent::CandidatesRecorded {
                search_id,
                candidates,
            }
            .into(),
        });
    }

    if let Some(convergence) = &discovery.convergence {
        return match convergence {
            DiscoveryConvergence::EvidenceSufficientForDeterministicImpactMap { .. } => {
                Ok(ProtocolDecision::Emit {
                    event: DiscoveryEvent::ImpactMapRecorded {
                        action_id: None,
                        evidence: state.deterministic_discovery_impact_map()?,
                    }
                    .into(),
                })
            }
            DiscoveryConvergence::ImpactMapAccepted { evidence_id } => {
                let proof = state.proofs.values().find(|proof| {
                    proof.kind == ProofKind::DiscoveryImpactMap
                        && proof.node_ids == [discovery.node_id.clone()]
                        && proof.related_evidence_ids == [evidence_id.clone()]
                });
                if let Some(proof) = proof {
                    Ok(ProtocolDecision::Emit {
                        event: GraphEvent::NodeSucceeded {
                            node_id: discovery.node_id.clone(),
                            proof_id: proof.id.clone(),
                        }
                        .into(),
                    })
                } else {
                    let proof_id = ProofId::new(format!(
                        "epv1:{}",
                        stable_sha256(&[
                            "execution-protocol-v1:discovery-impact-proof",
                            state.execution_id.as_str(),
                            &state.execution_attempt.to_string(),
                            evidence_id.as_str(),
                        ])
                    ));
                    Ok(ProtocolDecision::Emit {
                        event: EvidenceEvent::ProofRecorded {
                            proof: ProofRecord {
                                id: proof_id,
                                kind: ProofKind::DiscoveryImpactMap,
                                repository_revision: state.repository_revision.clone(),
                                node_ids: vec![discovery.node_id.clone()],
                                related_proof_ids: Vec::new(),
                                related_evidence_ids: vec![evidence_id.clone()],
                                detail_hash: discovery_impact_map_proof_hash(evidence_id),
                            },
                        }
                        .into(),
                    })
                }
            }
            DiscoveryConvergence::InsufficientEvidence { .. }
            | DiscoveryConvergence::BudgetBlocked { .. } => {
                let serialized = serde_json::to_string(convergence).map_err(|error| {
                    ProtocolViolation::EventSerialization {
                        detail: error.to_string(),
                    }
                })?;
                Ok(ProtocolDecision::Emit {
                    event: GraphEvent::NodeFailed {
                        node_id: discovery.node_id.clone(),
                        failure_revision_id: FailureRevisionId::new(format!(
                            "epv1:{}",
                            stable_sha256(&[
                                "execution-protocol-v1:discovery-failure",
                                state.execution_id.as_str(),
                                &serialized,
                            ])
                        )),
                        terminal: true,
                    }
                    .into(),
                })
            }
        };
    }

    let remaining = state.discovery_budget_remaining(&discovery.node_id)?;
    match select_next_discovery_step(discovery, remaining.admissible_model_calls()) {
        DiscoveryNextStep::Action(_) => Ok(ProtocolDecision::Emit {
            event: DiscoveryEvent::ActionPrepared {
                prepared: Box::new(build_prepared_discovery_action(state)?),
            }
            .into(),
        }),
        DiscoveryNextStep::Converge(convergence) => Ok(ProtocolDecision::Emit {
            event: DiscoveryEvent::ConvergenceEvaluated { convergence }.into(),
        }),
    }
}

fn next_candidate_projection(
    discovery: &DiscoveryState,
) -> Result<Option<(SearchId, Vec<CandidatePathEvidence>)>, ProtocolViolation> {
    for search_id in discovery.completed_searches.keys() {
        let candidates = canonical_candidate_projection(discovery, search_id)?;
        if !candidates.is_empty() {
            return Ok(Some((search_id.clone(), candidates)));
        }
    }
    Ok(None)
}

fn canonical_candidate_projection(
    discovery: &DiscoveryState,
    search_id: &SearchId,
) -> Result<Vec<CandidatePathEvidence>, ProtocolViolation> {
    let search = discovery.completed_searches.get(search_id).ok_or(
        ProtocolViolation::DiscoveryContract {
            code: "candidate_source_search_missing",
        },
    )?;
    let mut next_rank = discovery
        .candidates
        .values()
        .map(|candidate| candidate.rank)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let mut remaining_new = MAX_DISCOVERY_CANDIDATES.saturating_sub(discovery.candidates.len());
    let mut projected = Vec::new();
    for path in &search.matched_paths {
        let mut candidate = if let Some(existing) = discovery.candidates.get(path) {
            existing.clone()
        } else {
            if remaining_new == 0 {
                continue;
            }
            remaining_new = remaining_new.saturating_sub(1);
            let rank = next_rank;
            next_rank = next_rank.saturating_add(1);
            CandidatePathEvidence {
                schema_version: DISCOVERY_SCHEMA_VERSION,
                evidence_id: EvidenceId::new("pending:candidate-evidence"),
                producer_node_id: discovery.node_id.clone(),
                repository_revision: discovery.repository_revision.clone(),
                path: path.clone(),
                rank,
                reasons: BTreeSet::new(),
                source_search_ids: BTreeSet::new(),
                criterion_ids: BTreeSet::new(),
            }
        };
        candidate.reasons.insert(CandidateReason::SearchMatch);
        candidate.source_search_ids.insert(search_id.clone());
        candidate
            .criterion_ids
            .extend(search.request.criterion_ids.iter().cloned());
        candidate = candidate.canonicalize_id()?;
        if discovery.candidates.get(path) != Some(&candidate) {
            projected.push(candidate);
        }
    }
    Ok(projected)
}

fn discovery_terminal_result(
    state: &ExecutionState,
    convergence: &DiscoveryConvergence,
) -> Result<(MissionResult, &'static str), ProtocolViolation> {
    let discovery_node = NodeId::new("protocol-v1:discovery");
    let blocker = |code: &'static str| FirstFatalBlocker {
        category: "discovery".into(),
        code: code.into(),
        node_id: Some(discovery_node.clone()),
    };
    match convergence {
        DiscoveryConvergence::InsufficientEvidence { .. } => Ok((
            MissionResult::InsufficientEvidence {
                failure: blocker("insufficient_discovery_evidence"),
            },
            "insufficient_discovery_evidence",
        )),
        DiscoveryConvergence::BudgetBlocked { .. } => {
            if !state.discovery_budget_is_exhausted(&discovery_node)? {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "discovery_budget_block_without_exact_exhaustion",
                });
            }
            Ok((
                MissionResult::BudgetBlocked {
                    node_id: discovery_node.clone(),
                    failure: blocker("discovery_budget_exhausted"),
                },
                "discovery_budget_exhausted",
            ))
        }
        DiscoveryConvergence::ImpactMapAccepted { .. }
        | DiscoveryConvergence::EvidenceSufficientForDeterministicImpactMap { .. } => {
            Err(ProtocolViolation::DiscoveryContract {
                code: "successful_discovery_cannot_resolve_terminal_failure",
            })
        }
    }
}

pub(crate) fn repository_profile_proof_hash(profile_id: &RepositoryProfileId) -> String {
    stable_sha256(&[
        "execution-protocol-v1:repository-profile-proof",
        profile_id.as_str(),
    ])
}

pub(crate) fn discovery_impact_map_proof_hash(evidence_id: &EvidenceId) -> String {
    stable_sha256(&[
        "execution-protocol-v1:discovery-impact-map-proof",
        evidence_id.as_str(),
    ])
}

pub(crate) fn build_prepared_discovery_action(
    state: &ExecutionState,
) -> Result<PreparedDiscoveryAction, ProtocolViolation> {
    validate_state(state)?;
    authoritative_prepared_discovery_action(state)
}

pub(crate) fn build_prepared_planning_action(
    state: &ExecutionState,
) -> Result<PreparedPlanningAction, ProtocolViolation> {
    validate_state(state)?;
    authoritative_prepared_planning_action(state)
}

fn authoritative_prepared_planning_action(
    state: &ExecutionState,
) -> Result<PreparedPlanningAction, ProtocolViolation> {
    let planning = state
        .planning
        .as_ref()
        .ok_or(ProtocolViolation::PlanningContract {
            code: "planning_state_missing",
        })?;
    state.require_active_planning_node()?;
    if state.current_planning_action.is_some() {
        return Err(ProtocolViolation::PlanningContract {
            code: "planning_action_already_active",
        });
    }
    if planning.accepted_plan.is_some()
        || planning.accepted_no_op.is_some()
        || planning.convergence.is_some()
    {
        return Err(ProtocolViolation::PlanningContract {
            code: "planning_action_after_convergence",
        });
    }
    let node =
        state
            .nodes
            .get(&planning.node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: planning.node_id.clone(),
            })?;
    let NodeState::Active { attempt } = node.state else {
        return Err(ProtocolViolation::InvalidNodeState {
            node_id: node.id.clone(),
            code: "planning_action_owner_not_active",
        });
    };
    let remaining = state.planning_budget_remaining(&planning.node_id)?;
    if remaining.is_exhausted() {
        return Err(ProtocolViolation::PlanningContract {
            code: "planning_action_after_budget_exhaustion",
        });
    }
    let action_ordinal = state
        .event_log
        .iter()
        .filter(|stored| {
            matches!(
                stored.envelope.payload,
                DomainEvent::Planning(PlanningEvent::ActionPrepared { .. })
            )
        })
        .count()
        .saturating_add(1);
    let action_id = ActionId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:planning-action",
            state.execution_id.as_str(),
            &state.execution_attempt.to_string(),
            planning.node_id.as_str(),
            &attempt.to_string(),
            &action_ordinal.to_string(),
            &planning.next_revision_index().to_string(),
            state.repository_revision.as_str(),
            planning.discovery_impact_map_id.as_str(),
        ])
    ));
    let reservation_value = format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:planning-reservation",
            action_id.as_str(),
            planning.node_id.as_str(),
            &action_ordinal.to_string(),
        ])
    );
    let discovery = state
        .discovery
        .as_ref()
        .ok_or(ProtocolViolation::PlanningContract {
            code: "planning_discovery_state_missing",
        })?;
    let context = build_planning_context(
        planning,
        discovery,
        action_id.clone(),
        node.budget.max_input_tokens_per_call,
    )?;
    let envelope = PlanningActionEnvelope::new(
        action_id.clone(),
        planning.node_id.clone(),
        state.repository_revision.clone(),
        &context,
        node.budget.max_input_tokens_per_call,
        node.budget.max_output_tokens_per_call,
        planning.node_id.clone(),
        ReservationId::new(reservation_value.clone()),
    )?;
    let admission = ModelCallAdmission {
        call_id: ModelCallId::new(reservation_value),
        node_id: planning.node_id.clone(),
        action_id,
        payload_hash: envelope.payload_identity.clone(),
        input_tokens: context.estimated_input_tokens,
        output_tokens: envelope.output_token_allowance,
        reserved_cost_micros: remaining.cost_micros,
        duration_allowance_ms: remaining.duration_ms,
    };
    Ok(PreparedPlanningAction {
        context,
        envelope,
        admission,
    })
}

fn authoritative_prepared_discovery_action(
    state: &ExecutionState,
) -> Result<PreparedDiscoveryAction, ProtocolViolation> {
    let discovery = state
        .discovery
        .as_ref()
        .ok_or(ProtocolViolation::DiscoveryContract {
            code: "discovery_state_missing",
        })?;
    state.require_active_discovery_node()?;
    if state.current_discovery_action.is_some() {
        return Err(ProtocolViolation::DiscoveryContract {
            code: "discovery_action_already_active",
        });
    }
    let node =
        state
            .nodes
            .get(&discovery.node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: discovery.node_id.clone(),
            })?;
    let NodeState::Active { attempt } = node.state else {
        return Err(ProtocolViolation::InvalidNodeState {
            node_id: node.id.clone(),
            code: "discovery_action_owner_not_active",
        });
    };
    let remaining = state.discovery_budget_remaining(&discovery.node_id)?;
    let DiscoveryNextStep::Action(action_class) =
        select_next_discovery_step(discovery, remaining.admissible_model_calls())
    else {
        return Err(ProtocolViolation::DiscoveryContract {
            code: "discovery_action_prepared_when_convergence_required",
        });
    };
    let constraints = match action_class {
        DiscoveryActionClass::DiscoverCandidates => {
            let mut selected_search = None;
            'criteria: for criterion_id in &discovery.goal.criterion_ids {
                for term in &discovery.goal.normalized_search_terms {
                    let criterion_ids = BTreeSet::from([criterion_id.clone()]);
                    let already_completed = discovery.completed_searches.values().any(|evidence| {
                        evidence.request.criterion_ids == criterion_ids
                            && evidence.request.normalized_query == *term
                    });
                    if !already_completed {
                        selected_search = Some((criterion_ids, term));
                        break 'criteria;
                    }
                }
            }
            let (criterion_ids, query) =
                selected_search.ok_or(ProtocolViolation::DiscoveryContract {
                    code: "discovery_search_terms_exhausted",
                })?;
            DiscoveryActionConstraints::Search {
                request: SearchRequest::new(
                    discovery.repository_revision.clone(),
                    discovery.repository_profile_id.clone(),
                    criterion_ids,
                    query,
                    SearchScope::repository(),
                    Vec::<String>::new(),
                    SearchMode::LiteralCaseInsensitive,
                    BTreeSet::new(),
                )?,
            }
        }
        DiscoveryActionClass::GroundCandidateEvidence => DiscoveryActionConstraints::ExactPaths {
            paths: discovery.ranked_candidate_paths().into_iter().collect(),
        },
        DiscoveryActionClass::ResolveNamedRelationship => {
            let question = discovery
                .unresolved_questions
                .values()
                .next()
                .cloned()
                .ok_or(ProtocolViolation::DiscoveryContract {
                    code: "discovery_relationship_question_missing",
                })?;
            DiscoveryActionConstraints::NamedRelationship {
                paths: BTreeSet::from([question.subject_path.clone()]),
                question,
                targeted_search: None,
            }
        }
        DiscoveryActionClass::RecordImpactMap => DiscoveryActionConstraints::ImpactMap {
            criterion_ids: discovery.goal.criterion_ids.clone(),
            evidence_ids: discovery.impact_map_evidence_ids(),
        },
    };
    let action_ordinal = state
        .budgets
        .model_calls
        .values()
        .filter(|record| record.admission.node_id == discovery.node_id)
        .count()
        .saturating_add(1);
    let constraints_identity = serde_json::to_string(&constraints).map_err(|error| {
        ProtocolViolation::EventSerialization {
            detail: error.to_string(),
        }
    })?;
    let action_id = ActionId::new(format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:discovery-action",
            state.execution_id.as_str(),
            &state.execution_attempt.to_string(),
            discovery.node_id.as_str(),
            &attempt.to_string(),
            &action_ordinal.to_string(),
            state.repository_revision.as_str(),
            &constraints_identity,
        ])
    ));
    let reservation_value = format!(
        "epv1:{}",
        stable_sha256(&[
            "execution-protocol-v1:discovery-reservation",
            action_id.as_str(),
            discovery.node_id.as_str(),
            &action_ordinal.to_string(),
        ])
    );
    let context = build_discovery_context(
        discovery,
        action_id.clone(),
        &constraints,
        node.budget.max_input_tokens_per_call,
    )?;
    let envelope = ActionEnvelope::new(
        action_id.clone(),
        discovery.node_id.clone(),
        state.repository_revision.clone(),
        &context,
        constraints,
        node.budget.max_input_tokens_per_call,
        node.budget.max_output_tokens_per_call,
        discovery.node_id.clone(),
        ReservationId::new(reservation_value.clone()),
    )?;
    let admission = ModelCallAdmission {
        call_id: ModelCallId::new(reservation_value),
        node_id: discovery.node_id.clone(),
        action_id,
        payload_hash: envelope.payload_identity.clone(),
        input_tokens: context.estimated_input_tokens,
        output_tokens: envelope.output_token_allowance,
        reserved_cost_micros: remaining.cost_micros,
        duration_allowance_ms: remaining.duration_ms,
    };
    Ok(PreparedDiscoveryAction {
        context,
        envelope,
        admission,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DiscoveryBudgetRemaining {
    model_calls: u32,
    cost_micros: u64,
    duration_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlanningBudgetRemaining {
    model_calls: u32,
    cost_micros: u64,
    duration_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MutationTerminalDisposition {
    BlockedNoDiff,
    BudgetBlocked,
    InfrastructureFailed,
}

impl PlanningBudgetRemaining {
    const fn is_exhausted(self) -> bool {
        self.model_calls == 0 || self.cost_micros == 0 || self.duration_ms == 0
    }
}

impl DiscoveryBudgetRemaining {
    const fn is_exhausted(self) -> bool {
        self.model_calls == 0 || self.cost_micros == 0 || self.duration_ms == 0
    }

    const fn admissible_model_calls(self) -> u32 {
        if self.is_exhausted() {
            0
        } else {
            self.model_calls
        }
    }
}

pub(crate) fn reduce(
    state: &ExecutionState,
    event: ProtocolEventEnvelope,
) -> Result<ExecutionState, ProtocolViolation> {
    validate_state(state)?;
    let mut next = state.clone();
    next.append_event(event)?;
    Ok(next)
}

pub(crate) fn validate_state(state: &ExecutionState) -> Result<(), ProtocolViolation> {
    state.require_trusted_bootstrap()?;
    state.validate_invariants()?;
    super::store::validate_replay_equivalence(state)
}

impl ExecutionState {
    pub(crate) fn append_event(
        &mut self,
        event: ProtocolEventEnvelope,
    ) -> Result<AppendOutcome, ProtocolViolation> {
        self.require_trusted_bootstrap()?;
        self.validate_invariants()?;
        let canonical_hash = event.canonical_hash()?;
        if let Some(existing_hash) = self.event_payload_hashes.get(&event.event_id) {
            if existing_hash == &canonical_hash {
                let stored = self
                    .event_log
                    .iter()
                    .find(|stored| stored.envelope.event_id == event.event_id)
                    .ok_or_else(|| ProtocolViolation::Invariant {
                        code: "event_index_missing_stored_event",
                        detail: format!("event `{}` is indexed but not stored", event.event_id),
                    });
                let original_revision =
                    stored?.envelope.aggregate_revision_before.saturating_add(1);
                return Ok(AppendOutcome::IdempotentReplay {
                    revision: original_revision,
                });
            }
            return Err(ProtocolViolation::EventIdentityConflict {
                event_id: event.event_id,
            });
        }

        self.validate_event_envelope(&event)?;
        if self.terminal.is_some() {
            return Err(ProtocolViolation::TerminalImmutable);
        }
        if self.implementation.is_some()
            && self.stage() == ProtocolStage::Implementation
            && let Some(expected) = self.authoritative_mutation_terminal_result()?
            && !matches!(
                &event.payload,
                DomainEvent::Terminal(TerminalEvent::CanonicalResultRecorded { result })
                    if result == &expected
            )
        {
            return Err(ProtocolViolation::MutationContract {
                code: "mutation_terminal_progress_frozen",
            });
        }
        if self.validation.is_some() && self.stage() == ProtocolStage::Repair {
            if let Some(node) = self
                .active_node()
                .filter(|node| node.kind == NodeKind::ValidationRepair)
                && let Ok(failure_revision_id) = self.mutation_terminal_failure_revision(&node.id)
                && !matches!(
                    &event.payload,
                    DomainEvent::Graph(GraphEvent::NodeFailed {
                        node_id,
                        failure_revision_id: actual,
                        terminal: true,
                    }) if node_id == &node.id && actual == failure_revision_id
                )
            {
                return Err(ProtocolViolation::ValidationContract {
                    code: "repair_mutation_terminal_progress_frozen",
                });
            } else if let Some(expected) = self.authoritative_repair_mutation_terminal_result()?
                && !matches!(
                    &event.payload,
                    DomainEvent::Terminal(TerminalEvent::CanonicalResultRecorded { result })
                        if result == &expected
                )
            {
                return Err(ProtocolViolation::ValidationContract {
                    code: "repair_mutation_terminal_progress_frozen",
                });
            }
        }
        if self
            .validation
            .as_ref()
            .is_some_and(|validation| validation.convergence.is_some())
            && matches!(
                self.stage(),
                ProtocolStage::Validation | ProtocolStage::Repair
            )
        {
            let convergence = self
                .validation
                .as_ref()
                .and_then(|validation| validation.convergence.as_ref())
                .expect("validation convergence was checked");
            let exact_node_failure = self.active_node().is_some_and(|node| {
                matches!(
                    &event.payload,
                    DomainEvent::Graph(GraphEvent::NodeFailed {
                        node_id,
                        failure_revision_id,
                        terminal: true,
                    }) if node_id == &node.id
                        && failure_revision_id == &convergence.failure_revision_id
                )
            });
            let expected = self.authoritative_validation_terminal_result()?;
            let exact_terminal = matches!(
                &event.payload,
                DomainEvent::Terminal(TerminalEvent::CanonicalResultRecorded { result })
                    if result == &expected
            );
            if !exact_node_failure && !exact_terminal {
                return Err(ProtocolViolation::ValidationContract {
                    code: "validation_terminal_progress_frozen",
                });
            }
        }
        if self
            .review
            .as_ref()
            .is_some_and(|review| review.convergence.is_some())
            && self.stage() == ProtocolStage::Review
        {
            let convergence = self
                .review
                .as_ref()
                .and_then(|review| review.convergence.as_ref())
                .expect("review convergence was checked");
            let failure_revision_id = review_convergence_failure_revision(convergence);
            let exact_node_failure = self.active_node().is_some_and(|node| {
                matches!(
                    &event.payload,
                    DomainEvent::Graph(GraphEvent::NodeFailed {
                        node_id,
                        failure_revision_id: actual,
                        terminal: true,
                    }) if node_id == &node.id && actual == &failure_revision_id
                )
            });
            let exact_terminal = if self.active_node().is_none() {
                let expected = self.authoritative_review_terminal_result()?;
                matches!(
                    &event.payload,
                    DomainEvent::Terminal(TerminalEvent::CanonicalResultRecorded { result })
                        if result == &expected
                )
            } else {
                false
            };
            if !exact_node_failure && !exact_terminal {
                return Err(ProtocolViolation::ReviewContract {
                    code: "review_terminal_progress_frozen",
                });
            }
        }
        if self
            .publication
            .as_ref()
            .is_some_and(|publication| publication.convergence.is_some())
            && self.stage() == ProtocolStage::Publication
        {
            let convergence = self
                .publication
                .as_ref()
                .and_then(|publication| publication.convergence.as_ref())
                .expect("publication convergence was checked");
            let failure_revision_id = publication_convergence_failure_revision(convergence);
            let exact_node_failure = self.active_node().is_some_and(|node| {
                matches!(
                    &event.payload,
                    DomainEvent::Graph(GraphEvent::NodeFailed {
                        node_id,
                        failure_revision_id: actual,
                        terminal: true,
                    }) if node_id == &node.id && actual == &failure_revision_id
                )
            });
            let exact_terminal = if self.active_node().is_none() {
                let expected = self.authoritative_publication_terminal_result()?;
                matches!(
                    &event.payload,
                    DomainEvent::Terminal(TerminalEvent::CanonicalResultRecorded { result })
                        if result == &expected
                )
            } else {
                false
            };
            if !exact_node_failure && !exact_terminal {
                return Err(ProtocolViolation::PublicationContract {
                    code: "publication_terminal_progress_frozen",
                });
            }
        }
        self.validate_pending_repair_handoff_event(&event.payload)?;

        let mut next = self.clone();
        next.apply_payload(&event.payload)?;
        next.aggregate_revision = next.aggregate_revision.saturating_add(1);
        next.event_payload_hashes
            .insert(event.event_id.clone(), canonical_hash.clone());
        next.event_log.push(StoredProtocolEvent {
            envelope: event,
            payload_hash: canonical_hash,
        });
        next.validate_invariants()?;
        let revision = next.aggregate_revision;
        *self = next;
        Ok(AppendOutcome::Applied { revision })
    }

    fn validate_event_envelope(
        &self,
        event: &ProtocolEventEnvelope,
    ) -> Result<(), ProtocolViolation> {
        if event.protocol_version != EXECUTION_PROTOCOL_VERSION {
            return Err(ProtocolViolation::UnsupportedVersion {
                found: event.protocol_version,
            });
        }
        if event.event_schema_version != PROTOCOL_EVENT_SCHEMA_VERSION {
            return Err(ProtocolViolation::EnvelopeMismatch {
                field: "event_schema_version",
            });
        }
        if event.execution_id != self.execution_id {
            return Err(ProtocolViolation::EnvelopeMismatch {
                field: "execution_id",
            });
        }
        if event.execution_attempt != self.execution_attempt {
            return Err(ProtocolViolation::EnvelopeMismatch {
                field: "execution_attempt",
            });
        }
        if event.repository_revision != self.repository_revision {
            return Err(ProtocolViolation::EnvelopeMismatch {
                field: "repository_revision",
            });
        }
        if event.semantic_key.trim().is_empty()
            || event.semantic_identity != event.expected_semantic_identity()?
        {
            return Err(ProtocolViolation::InvalidEventIdentity {
                event_id: event.event_id.clone(),
            });
        }
        if event.event_id != event.expected_event_id()? {
            return Err(ProtocolViolation::InvalidEventIdentity {
                event_id: event.event_id.clone(),
            });
        }
        if event.aggregate_revision_before != self.aggregate_revision {
            return Err(ProtocolViolation::RevisionConflict {
                expected: event.aggregate_revision_before,
                actual: self.aggregate_revision,
            });
        }
        let expected_sequence = self.next_sequence();
        if event.sequence != expected_sequence {
            return Err(ProtocolViolation::SequenceConflict {
                expected: expected_sequence,
                actual: event.sequence,
            });
        }
        Ok(())
    }

    fn apply_payload(&mut self, payload: &DomainEvent) -> Result<(), ProtocolViolation> {
        match payload {
            DomainEvent::Profile(event) => self.apply_profile_event(event),
            DomainEvent::Discovery(event) => self.apply_discovery_event(event),
            DomainEvent::Planning(event) => self.apply_planning_event(event),
            DomainEvent::Implementation(event) => self.apply_implementation_event(event),
            DomainEvent::Mutation(event) => self.apply_mutation_event(event),
            DomainEvent::Validation(event) => self.apply_validation_event(event),
            DomainEvent::Review(event) => self.apply_review_event(event),
            DomainEvent::Publication(event) => self.apply_publication_event(event),
            DomainEvent::Evidence(event) => self.apply_evidence_event(event),
            DomainEvent::Graph(event) => self.apply_graph_event(event),
            DomainEvent::Budget(event) => self.apply_budget_event(event),
            DomainEvent::Lifecycle(event) => self.apply_lifecycle_event(event),
            DomainEvent::Terminal(event) => self.apply_terminal_event(event),
        }
    }

    fn apply_validation_event(&mut self, event: &ValidationEvent) -> Result<(), ProtocolViolation> {
        self.validate_validation_event_authority(event)?;
        let policy = self
            .validation_policy
            .as_ref()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_policy_missing",
            })?
            .clone();
        let validation = self
            .validation
            .as_mut()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_state_missing",
            })?;
        validation.apply(event, &policy)?;
        let reopened_validation_nodes = match event {
            ValidationEvent::PriorValidationInvalidated { invalidation } => invalidation
                .invalidated_evidence_ids
                .iter()
                .filter_map(|evidence_id| validation.evidence.get(evidence_id))
                .filter_map(|evidence| validation.gates.get(&evidence.gate_id))
                .map(|gate| gate.node_id.clone())
                .collect::<BTreeSet<_>>(),
            _ => BTreeSet::new(),
        };
        for node_id in reopened_validation_nodes {
            if let Some(node) = self.nodes.get_mut(&node_id)
                && matches!(node.state, NodeState::Succeeded { .. })
            {
                node.state = NodeState::Pending;
            }
        }
        self.refresh_validation_step();
        Ok(())
    }

    fn apply_review_event(&mut self, event: &ReviewEvent) -> Result<(), ProtocolViolation> {
        if self.stage() != ProtocolStage::Review {
            return Err(ProtocolViolation::ReviewContract {
                code: "review_event_outside_review",
            });
        }
        self.validate_review_event_authority(event)?;
        let policy = self
            .finalization_policy
            .as_ref()
            .ok_or(ProtocolViolation::ReviewContract {
                code: "finalization_policy_missing",
            })?
            .clone();
        let plan = self
            .planning
            .as_ref()
            .and_then(|planning| planning.accepted_plan.as_ref())
            .ok_or(ProtocolViolation::ReviewContract {
                code: "review_accepted_plan_missing",
            })?
            .clone();
        let next_repository_revision = match event {
            ReviewEvent::ConvergenceEvaluated {
                convergence:
                    ReviewConvergenceV1 {
                        reason:
                            ReviewConvergenceReasonV1::RepositoryDrift {
                                observed_revision, ..
                            },
                        ..
                    },
            } => (observed_revision != &self.repository_revision)
                .then_some(observed_revision.clone()),
            _ => None,
        };
        self.review
            .as_mut()
            .ok_or(ProtocolViolation::ReviewContract {
                code: "review_state_missing",
            })?
            .apply(event, &plan, &policy)?;
        if let Some(repository_revision) = next_repository_revision {
            self.repository_revision = repository_revision;
        }
        self.refresh_review_step();
        Ok(())
    }

    fn validate_review_event_authority(
        &self,
        event: &ReviewEvent,
    ) -> Result<(), ProtocolViolation> {
        let review = self
            .review
            .as_ref()
            .ok_or(ProtocolViolation::ReviewContract {
                code: "review_state_missing",
            })?;
        let policy =
            self.finalization_policy
                .as_ref()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "finalization_policy_missing",
                })?;
        let plan = self
            .planning
            .as_ref()
            .and_then(|planning| planning.accepted_plan.as_ref())
            .ok_or(ProtocolViolation::ReviewContract {
                code: "review_accepted_plan_missing",
            })?;
        if review.repository_revision != self.repository_revision
            || review.ancestry != self.engineering_ancestry()?
        {
            return Err(ProtocolViolation::ReviewContract {
                code: "review_ancestry_not_current",
            });
        }
        let active = self.active_node();
        match event {
            ReviewEvent::DiffManifestRequested { request } => {
                let node = active.filter(|node| node.kind == NodeKind::Review).ok_or(
                    ProtocolViolation::ReviewContract {
                        code: "diff_manifest_request_without_review_owner",
                    },
                )?;
                let expected =
                    DiffManifestRequestV1::new(node.id.clone(), plan, &review.ancestry, policy)?;
                if request != &expected {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "diff_manifest_request_not_canonical",
                    });
                }
            }
            ReviewEvent::DiffManifestBuildFailed { failure } => {
                let node = active.filter(|node| node.kind == NodeKind::Review).ok_or(
                    ProtocolViolation::ReviewContract {
                        code: "diff_manifest_failure_without_review_owner",
                    },
                )?;
                let request =
                    review
                        .diff_request
                        .as_ref()
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "diff_manifest_failure_without_request",
                        })?;
                failure.validate_against(request)?;
                if node.id != request.review_node_id
                    || !matches!(
                        decide_phase7_review(self)?,
                        ProtocolDecision::Perform {
                            effect: EffectRequest::Review(
                                ReviewEffectRequest::BuildDiffManifest {
                                    request: expected,
                                },
                            ),
                        } if *expected == *request
                    )
                {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "diff_manifest_failure_not_current",
                    });
                }
            }
            ReviewEvent::DiffManifestRecorded { manifest } => {
                if active.is_none_or(|node| node.kind != NodeKind::Review) {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "diff_manifest_without_review_owner",
                    });
                }
                let request =
                    review
                        .diff_request
                        .as_ref()
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "diff_manifest_without_request",
                        })?;
                manifest.validate_against(request, plan)?;
            }
            ReviewEvent::ActionPrepared { prepared } => {
                let node = active.ok_or(ProtocolViolation::ReviewContract {
                    code: "review_action_without_owner",
                })?;
                if !matches!(node.kind, NodeKind::Review | NodeKind::CompletionEvaluation) {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_action_wrong_owner",
                    });
                }
                let expected = prepare_phase7_review_action(self, review, policy, plan, node)?;
                if !matches!(
                    expected,
                    ProtocolDecision::Emit {
                        event: DomainEvent::Review(ReviewEvent::ActionPrepared {
                            prepared: expected
                        })
                    } if *expected == **prepared
                ) {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_action_not_canonical",
                    });
                }
            }
            ReviewEvent::ActionReleased { action_id } => {
                let prepared =
                    review
                        .actions
                        .get(action_id)
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "review_action_release_unknown",
                        })?;
                if self
                    .budgets
                    .model_calls
                    .get(&prepared.admission.call_id)
                    .is_none_or(|record| record.state != ModelCallState::ReconciledReleased)
                {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_action_release_without_reconciliation",
                    });
                }
            }
            ReviewEvent::ActionRejected { action_id, .. } => {
                let prepared =
                    review
                        .actions
                        .get(action_id)
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "review_action_rejection_unknown",
                        })?;
                if self
                    .budgets
                    .model_calls
                    .get(&prepared.admission.call_id)
                    .is_none_or(|record| {
                        !matches!(record.state, ModelCallState::ReconciledConsumed { .. })
                    })
                {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_action_rejection_before_consumed_call",
                    });
                }
            }
            ReviewEvent::DiffPageReviewed { observation } => {
                let node = active.filter(|node| node.kind == NodeKind::Review).ok_or(
                    ProtocolViolation::ReviewContract {
                        code: "diff_review_observation_without_review_owner",
                    },
                )?;
                let prepared = review.actions.get(&observation.action_id).ok_or(
                    ProtocolViolation::ReviewContract {
                        code: "diff_review_observation_action_missing",
                    },
                )?;
                if observation.node_id != node.id
                    || self
                        .budgets
                        .model_calls
                        .get(&prepared.admission.call_id)
                        .is_none_or(|record| {
                            !matches!(record.state, ModelCallState::ReconciledConsumed { .. })
                        })
                {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "diff_review_observation_before_consumed_call",
                    });
                }
            }
            ReviewEvent::DiffReviewRecorded { review: supplied } => {
                if active.is_none_or(|node| node.kind != NodeKind::Review) {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "diff_review_without_review_owner",
                    });
                }
                let manifest =
                    review
                        .diff_manifest
                        .as_deref()
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "diff_review_manifest_missing",
                        })?;
                if **supplied != DiffReviewV1::aggregate(manifest, &review.page_reviews)? {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "diff_review_not_canonical",
                    });
                }
            }
            ReviewEvent::CompletionEvaluationRecorded { evaluation } => {
                let node = active
                    .filter(|node| node.kind == NodeKind::CompletionEvaluation)
                    .ok_or(ProtocolViolation::ReviewContract {
                        code: "completion_evaluation_without_owner",
                    })?;
                let prepared = review.actions.get(&evaluation.action_id).ok_or(
                    ProtocolViolation::ReviewContract {
                        code: "completion_evaluation_action_missing",
                    },
                )?;
                if evaluation.node_id != node.id
                    || self
                        .budgets
                        .model_calls
                        .get(&prepared.admission.call_id)
                        .is_none_or(|record| {
                            !matches!(record.state, ModelCallState::ReconciledConsumed { .. })
                        })
                {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "completion_evaluation_before_consumed_call",
                    });
                }
            }
            ReviewEvent::PublicationAuthorityRequested { request } => {
                let completion =
                    review
                        .completion
                        .as_deref()
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "publication_authority_completion_missing",
                        })?;
                if active.is_some()
                    || self.has_open_model_call()
                    || request != &PublicationAuthorityRequestV1::new(policy, completion)?
                {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "publication_authority_request_not_canonical",
                    });
                }
            }
            ReviewEvent::PublicationAuthorityObservationFailed { failure } => {
                let request =
                    review
                        .authority_request
                        .as_ref()
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "publication_authority_failure_without_request",
                        })?;
                failure.validate_against(request)?;
                if active.is_some()
                    || self.has_open_model_call()
                    || !matches!(
                        decide_phase7_review(self)?,
                        ProtocolDecision::Perform {
                            effect: EffectRequest::Review(
                                ReviewEffectRequest::ObservePublicationAuthority {
                                    request: expected,
                                },
                            ),
                        } if *expected == *request
                    )
                {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "publication_authority_failure_not_current",
                    });
                }
            }
            ReviewEvent::PublicationAuthorityObserved { observation } => {
                let request =
                    review
                        .authority_request
                        .as_ref()
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "publication_authority_request_missing",
                        })?;
                observation.validate_against(request)?;
            }
            ReviewEvent::PublicationEligibilityEvaluated { eligibility } => {
                if active.is_some() || self.has_open_model_call() {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "publication_eligibility_with_active_work",
                    });
                }
                let manifest =
                    review
                        .diff_manifest
                        .as_deref()
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "publication_eligibility_manifest_missing",
                        })?;
                let diff_review =
                    review
                        .diff_review
                        .as_deref()
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "publication_eligibility_review_missing",
                        })?;
                let completion =
                    review
                        .completion
                        .as_deref()
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "publication_eligibility_completion_missing",
                        })?;
                let authority =
                    review
                        .authority
                        .as_ref()
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "publication_eligibility_authority_missing",
                        })?;
                let expected = PublicationEligibilityRecord::new(
                    policy,
                    &review.ancestry,
                    manifest,
                    diff_review,
                    self.review_completion_proof()?.id,
                    completion,
                    self.completion_evaluation_proof()?.id,
                    authority,
                    self.publication_eligibility_facts(&review.ancestry)?,
                )?;
                if **eligibility != expected {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "publication_eligibility_not_canonical",
                    });
                }
            }
            ReviewEvent::ConvergenceEvaluated { convergence } => {
                convergence.validate()?;
                if convergence.repository_revision != self.repository_revision
                    || convergence.policy_id != policy.policy_id
                {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_convergence_not_current",
                    });
                }
                let ProtocolDecision::Emit {
                    event:
                        DomainEvent::Review(ReviewEvent::ConvergenceEvaluated {
                            convergence: expected,
                        }),
                } = decide_phase7_review(self)?
                else {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_convergence_not_authoritative",
                    });
                };
                if convergence != &expected {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_convergence_not_authoritative",
                    });
                }
            }
        }
        Ok(())
    }

    fn publication_eligibility_facts(
        &self,
        ancestry: &EngineeringAncestryV1,
    ) -> Result<PublicationEligibilityFactsV1, ProtocolViolation> {
        let required_implementation_satisfied = ancestry == &self.engineering_ancestry()?
            && self
                .required_nodes(NodeKind::Implementation)
                .into_iter()
                .all(|node| matches!(node.state, NodeState::Succeeded { .. }));
        let required = self.required_validation_proof()?;
        Ok(PublicationEligibilityFactsV1 {
            required_implementation_satisfied,
            required_validation_current: self.proofs.get(&required.id) == Some(&required),
            no_active_validation_failure: self
                .validation
                .as_ref()
                .is_none_or(|validation| validation.current_failure().is_none()),
            no_active_work_or_reservation: self.active_node().is_none()
                && !self.has_open_model_call(),
        })
    }

    fn apply_publication_event(
        &mut self,
        event: &PublicationEvent,
    ) -> Result<(), ProtocolViolation> {
        if self.stage() != ProtocolStage::Publication {
            return Err(ProtocolViolation::PublicationContract {
                code: "publication_event_outside_publication",
            });
        }
        self.validate_publication_event_authority(event)?;
        let contract = self
            .finalization_policy
            .as_ref()
            .ok_or(ProtocolViolation::PublicationContract {
                code: "finalization_policy_missing",
            })?
            .publication
            .clone();
        let eligibility = self
            .review
            .as_ref()
            .and_then(|review| review.eligibility.as_deref())
            .ok_or(ProtocolViolation::PublicationContract {
                code: "publication_eligibility_missing",
            })?
            .clone();
        self.publication
            .as_mut()
            .ok_or(ProtocolViolation::PublicationContract {
                code: "publication_state_missing",
            })?
            .apply(event, &contract, &eligibility)?;
        self.refresh_publication_step();
        Ok(())
    }

    fn validate_publication_event_authority(
        &self,
        event: &PublicationEvent,
    ) -> Result<(), ProtocolViolation> {
        let node = self
            .active_node()
            .filter(|node| node.kind == NodeKind::Publication)
            .ok_or(ProtocolViolation::PublicationContract {
                code: "publication_event_without_active_owner",
            })?;
        let policy =
            self.finalization_policy
                .as_ref()
                .ok_or(ProtocolViolation::PublicationContract {
                    code: "finalization_policy_missing",
                })?;
        let review = self
            .review
            .as_ref()
            .ok_or(ProtocolViolation::PublicationContract {
                code: "review_state_missing",
            })?;
        let eligibility =
            review
                .eligibility
                .as_deref()
                .ok_or(ProtocolViolation::PublicationContract {
                    code: "publication_eligibility_missing",
                })?;
        let publication =
            self.publication
                .as_ref()
                .ok_or(ProtocolViolation::PublicationContract {
                    code: "publication_state_missing",
                })?;
        if node.id != publication.publication_node_id
            || node.id != self.single_required_node_id(NodeKind::Publication)?
            || publication.repository_revision != self.repository_revision
        {
            return Err(ProtocolViolation::PublicationContract {
                code: "publication_owner_binding_mismatch",
            });
        }
        match event {
            PublicationEvent::CommitIntentPersisted { intent } => {
                let manifest = review.diff_manifest.as_deref().ok_or(
                    ProtocolViolation::PublicationContract {
                        code: "publication_diff_manifest_missing",
                    },
                )?;
                let authority =
                    review
                        .authority
                        .as_ref()
                        .ok_or(ProtocolViolation::PublicationContract {
                            code: "publication_authority_missing",
                        })?;
                let tree =
                    CommitTreeBindingV1::from_review_authority(eligibility, manifest, authority)?;
                let expected =
                    publication.prepare_commit_intent(&policy.publication, eligibility, tree)?;
                if intent != &expected {
                    return Err(ProtocolViolation::PublicationContract {
                        code: "publication_commit_intent_not_canonical",
                    });
                }
            }
            PublicationEvent::PushIntentPersisted { intent } => {
                let expected = publication.prepare_push_intent(&policy.publication, eligibility)?;
                if intent != &expected {
                    return Err(ProtocolViolation::PublicationContract {
                        code: "publication_push_intent_not_canonical",
                    });
                }
            }
            PublicationEvent::PullRequestIntentPersisted { intent } => {
                let material = publication_pull_request_material(publication)?;
                let expected = publication.prepare_pull_request_intent(
                    &policy.publication,
                    eligibility,
                    &material,
                )?;
                if intent != &expected {
                    return Err(ProtocolViolation::PublicationContract {
                        code: "publication_pull_request_intent_not_canonical",
                    });
                }
            }
            PublicationEvent::CommitObserved { observation } => {
                let Some(PublicationAttemptRecordV1 {
                    intent: PublicationAttemptIntentV1::Commit(intent),
                    observation: None,
                    ..
                }) = publication.attempts.last()
                else {
                    return Err(ProtocolViolation::PublicationContract {
                        code: "commit_observation_without_current_intent",
                    });
                };
                observation.validate_against(intent)?;
            }
            PublicationEvent::PushObserved { observation } => {
                let Some(PublicationAttemptRecordV1 {
                    intent: PublicationAttemptIntentV1::Push(intent),
                    observation: None,
                    ..
                }) = publication.attempts.last()
                else {
                    return Err(ProtocolViolation::PublicationContract {
                        code: "push_observation_without_current_intent",
                    });
                };
                observation.validate_against(intent)?;
            }
            PublicationEvent::PullRequestObserved { observation } => {
                let Some(PublicationAttemptRecordV1 {
                    intent: PublicationAttemptIntentV1::PullRequest(intent),
                    observation: None,
                    ..
                }) = publication.attempts.last()
                else {
                    return Err(ProtocolViolation::PublicationContract {
                        code: "pull_request_observation_without_current_intent",
                    });
                };
                observation.validate_against(intent)?;
            }
            PublicationEvent::CompletionRecorded { completion } => {
                if completion != &publication.build_completion(&policy.publication)? {
                    return Err(ProtocolViolation::PublicationContract {
                        code: "publication_completion_not_canonical",
                    });
                }
            }
            PublicationEvent::ConvergenceEvaluated { convergence } => {
                let expected = publication.build_convergence(&policy.publication)?.ok_or(
                    ProtocolViolation::PublicationContract {
                        code: "publication_convergence_not_available",
                    },
                )?;
                if convergence != &expected {
                    return Err(ProtocolViolation::PublicationContract {
                        code: "publication_convergence_not_canonical",
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_validation_event_authority(
        &self,
        event: &ValidationEvent,
    ) -> Result<(), ProtocolViolation> {
        let validation = self
            .validation
            .as_ref()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_state_missing",
            })?;
        let policy =
            self.validation_policy
                .as_ref()
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "validation_policy_missing",
                })?;
        let profile =
            self.repository_profile
                .as_ref()
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "validation_repository_profile_missing",
                })?;
        let plan = self
            .planning
            .as_ref()
            .and_then(|planning| planning.accepted_plan.as_ref())
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_accepted_plan_missing",
            })?;
        policy.validate(profile)?;
        let verified_repair_handoff = matches!(event,
            ValidationEvent::PriorValidationInvalidated { invalidation }
                if validation.repository_revision == invalidation.repository_revision_before
                    && self.repository_revision == invalidation.repository_revision_after
        );
        if validation.repository_revision != self.repository_revision && !verified_repair_handoff {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_repository_revision_mismatch",
            });
        }
        match event {
            ValidationEvent::ValidationScheduled { request } => {
                if self.stage() != ProtocolStage::Validation {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_schedule_outside_validation",
                    });
                }
                let gate = validation
                    .next_gate()
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "validation_schedule_without_next_gate",
                    })?;
                if validation.run_for_gate(&gate.gate_id).is_some() {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_schedule_has_current_run",
                    });
                }
                let node = self.require_active_validation_node(&gate.node_id)?;
                let run_attempt = validation
                    .runs
                    .values()
                    .filter(|run| run.request.schedule.gate_id == gate.gate_id)
                    .count()
                    .saturating_add(1) as u32;
                let kind =
                    validation
                        .pending_rerun
                        .as_ref()
                        .map_or(ValidationRunKind::Initial, |rerun| {
                            ValidationRunKind::ExactRepairRerun {
                                failure_revision_id: rerun.failure_revision_id.clone(),
                                repair_intent_id: rerun.repair_intent_id.clone(),
                                verified_repair_evidence_id: rerun
                                    .verified_repair_evidence_id
                                    .clone(),
                            }
                        });
                let expected_schedule = ValidationRunSchedule::new(
                    self.execution_id.clone(),
                    self.execution_attempt,
                    gate,
                    node.attempts_started,
                    self.repository_revision.clone(),
                    run_attempt,
                    kind,
                )?;
                let expected = ValidationProcessRequest::new(expected_schedule, gate, policy)?;
                if request != &expected {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_schedule_not_authoritative",
                    });
                }
            }
            ValidationEvent::ValidationProcessStarted { started } => {
                if self.stage() != ProtocolStage::Validation {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_process_start_outside_validation",
                    });
                }
                let run = validation.runs.get(&started.run_id).ok_or(
                    ProtocolViolation::ValidationContract {
                        code: "validation_process_start_without_schedule",
                    },
                )?;
                if validation
                    .current_run_by_gate
                    .get(&run.request.schedule.gate_id)
                    != Some(&started.run_id)
                {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_process_start_not_current",
                    });
                }
                self.require_active_validation_node(&run.request.schedule.node_id)?;
                started.validate_against(&run.request)?;
            }
            ValidationEvent::ValidationProcessCompleted { completed } => {
                if self.stage() != ProtocolStage::Validation {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_process_completion_outside_validation",
                    });
                }
                let run = validation.runs.get(&completed.run_id).ok_or(
                    ProtocolViolation::ValidationContract {
                        code: "validation_process_completion_without_schedule",
                    },
                )?;
                if validation
                    .current_run_by_gate
                    .get(&run.request.schedule.gate_id)
                    != Some(&completed.run_id)
                {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_process_completion_not_current",
                    });
                }
                self.require_active_validation_node(&run.request.schedule.node_id)?;
                completed.validate_against(&run.request, run.started.as_ref())?;
            }
            ValidationEvent::ValidationEvidenceRecorded { evidence } => {
                if self.stage() != ProtocolStage::Validation {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_evidence_outside_validation",
                    });
                }
                let run = validation.runs.get(&evidence.run_id).ok_or(
                    ProtocolViolation::ValidationContract {
                        code: "validation_evidence_without_run",
                    },
                )?;
                if validation
                    .current_run_by_gate
                    .get(&run.request.schedule.gate_id)
                    != Some(&evidence.run_id)
                {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_evidence_run_not_current",
                    });
                }
                self.require_active_validation_node(&run.request.schedule.node_id)?;
                let started =
                    run.started
                        .as_ref()
                        .ok_or(ProtocolViolation::ValidationContract {
                            code: "validation_evidence_without_process_start",
                        })?;
                let completed =
                    run.completed
                        .as_ref()
                        .ok_or(ProtocolViolation::ValidationContract {
                            code: "validation_evidence_before_completion",
                        })?;
                let expected = ValidationEvidenceV1::from_completed(
                    &run.request,
                    started,
                    completed,
                    evidence.parser_confidence,
                    evidence.semantics,
                    evidence.diagnostics.clone(),
                )?;
                if evidence != &expected {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_evidence_not_authoritative",
                    });
                }
            }
            ValidationEvent::ValidationFailureRevisionRecorded { failure } => {
                if self.stage() != ProtocolStage::Validation {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_failure_outside_validation",
                    });
                }
                let evidence = validation
                    .evidence
                    .get(&failure.validation_evidence_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "validation_failure_evidence_missing",
                    })?;
                self.require_active_validation_node(&failure.node_id)?;
                if failure != &ValidationFailureRevisionV1::from_evidence(evidence)? {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_failure_not_authoritative",
                    });
                }
            }
            ValidationEvent::RepairCandidatesRanked { ranking } => {
                self.require_repair_stage_without_active_node()?;
                let failure =
                    validation
                        .current_failure()
                        .ok_or(ProtocolViolation::ValidationContract {
                            code: "repair_ranking_without_failure",
                        })?;
                let evidence = validation
                    .evidence
                    .get(&failure.validation_evidence_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "repair_ranking_evidence_missing",
                    })?;
                let relationships = self
                    .discovery
                    .as_ref()
                    .map(|discovery| &discovery.relationships)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "repair_discovery_evidence_missing",
                    })?;
                if ranking != &rank_repair_candidates(failure, evidence, plan, relationships)? {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "repair_ranking_not_authoritative",
                    });
                }
            }
            ValidationEvent::RepairEligibilityEvaluated { evaluation } => {
                self.require_repair_stage_without_active_node()?;
                let failure =
                    validation
                        .current_failure()
                        .ok_or(ProtocolViolation::ValidationContract {
                            code: "repair_eligibility_without_failure",
                        })?;
                let evidence = validation
                    .evidence
                    .get(&failure.validation_evidence_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "repair_eligibility_evidence_missing",
                    })?;
                let ranking = validation
                    .rankings
                    .get(&failure.failure_revision_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "repair_eligibility_without_ranking",
                    })?;
                let baselines = self.repair_mutation_baselines(failure);
                if evaluation
                    != &evaluate_repair_eligibility(
                        ranking, failure, evidence, plan, profile, policy, &baselines,
                    )?
                {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "repair_eligibility_not_authoritative",
                    });
                }
            }
            ValidationEvent::RepairTargetSelected { selection } => {
                self.require_repair_stage_without_active_node()?;
                let failure =
                    validation
                        .current_failure()
                        .ok_or(ProtocolViolation::ValidationContract {
                            code: "repair_selection_without_failure",
                        })?;
                let ranking = validation
                    .rankings
                    .get(&failure.failure_revision_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "repair_selection_without_ranking",
                    })?;
                let evaluation = validation
                    .eligibility
                    .get(&failure.failure_revision_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "repair_selection_without_eligibility",
                    })?;
                let gate = validation.gates.get(&failure.gate_id).ok_or(
                    ProtocolViolation::ValidationContract {
                        code: "repair_originating_gate_missing",
                    },
                )?;
                let baselines = self.repair_mutation_baselines(failure);
                if Some(selection)
                    != select_repair_target(
                        ranking, evaluation, failure, gate, plan, policy, &baselines,
                    )?
                    .as_ref()
                {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "repair_selection_not_authoritative",
                    });
                }
            }
            ValidationEvent::RepairTargetContextPrepared { prepared } => {
                if self.stage() != ProtocolStage::Repair {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "repair_target_context_outside_repair",
                    });
                }
                let failure =
                    validation
                        .current_failure()
                        .ok_or(ProtocolViolation::ValidationContract {
                            code: "repair_target_context_without_failure",
                        })?;
                let selection = validation
                    .selections
                    .get(&failure.failure_revision_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "repair_target_context_without_selection",
                    })?;
                let node = self.nodes.get(&selection.repair_node.id).ok_or_else(|| {
                    ProtocolViolation::UnknownNode {
                        node_id: selection.repair_node.id.clone(),
                    }
                })?;
                let NodeState::Active { attempt } = node.state else {
                    return Err(ProtocolViolation::InvalidNodeState {
                        node_id: node.id.clone(),
                        code: "repair_target_context_owner_not_active",
                    });
                };
                if prepared.node_id != node.id || prepared.node_attempt != attempt {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "repair_target_context_node_attempt_mismatch",
                    });
                }
                let baselines = self.repair_mutation_baselines(failure);
                let baseline = baselines.get(&selection.intent.target_id).ok_or(
                    ProtocolViolation::ValidationContract {
                        code: "repair_target_context_baseline_missing",
                    },
                )?;
                if prepared.manifest.repository_fingerprint
                    != baseline.evidence().repository_fingerprint_after
                {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "repair_target_context_repository_fingerprint_mismatch",
                    });
                }
                let discovery =
                    self.discovery
                        .as_ref()
                        .ok_or(ProtocolViolation::ValidationContract {
                            code: "repair_target_context_discovery_missing",
                        })?;
                let expected = build_validation_repair_target_context_load_request(
                    &self.execution_id,
                    self.execution_attempt,
                    &self.repository_revision,
                    node,
                    selection,
                    failure,
                    baseline,
                    plan,
                    discovery,
                )?;
                prepared.validate_against_request(&expected)?;
            }
            ValidationEvent::PriorValidationInvalidated { invalidation } => {
                if self.stage() != ProtocolStage::Repair
                    || invalidation.repository_revision_after != self.repository_revision
                {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_invalidation_outside_verified_repair",
                    });
                }
                let selection = validation
                    .selections
                    .get(&invalidation.failure_revision_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "validation_invalidation_without_selection",
                    })?;
                let failure = validation
                    .failures
                    .get(&invalidation.failure_revision_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "validation_invalidation_failure_missing",
                    })?;
                if invalidation != &self.authoritative_repair_invalidation(failure, selection)? {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_invalidation_not_authoritative",
                    });
                }
            }
            ValidationEvent::ValidationRerunScheduled { rerun } => {
                if self.stage() != ProtocolStage::Repair {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_rerun_outside_repair",
                    });
                }
                let invalidation = validation
                    .invalidations
                    .get(&rerun.failure_revision_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "validation_rerun_without_invalidation",
                    })?;
                let selection = validation
                    .selections
                    .get(&rerun.failure_revision_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "validation_rerun_without_selection",
                    })?;
                let gate = validation.gates.get(&rerun.originating_gate_id).ok_or(
                    ProtocolViolation::ValidationContract {
                        code: "validation_rerun_gate_missing",
                    },
                )?;
                if rerun != &ValidationRerunSchedule::new(invalidation, selection, gate)? {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_rerun_not_authoritative",
                    });
                }
            }
            ValidationEvent::ConvergenceEvaluated { convergence } => {
                let expected = self.authoritative_validation_convergence(validation, policy)?;
                if convergence != &expected {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_convergence_not_authoritative",
                    });
                }
            }
        }
        Ok(())
    }

    fn require_active_validation_node(
        &self,
        expected_node_id: &NodeId,
    ) -> Result<&ExecutionNode, ProtocolViolation> {
        if self.stage() != ProtocolStage::Validation {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_owner_outside_validation",
            });
        }
        let node = self
            .active_node()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_active_owner_missing",
            })?;
        if node.kind != NodeKind::Validation || &node.id != expected_node_id {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_active_owner_mismatch",
            });
        }
        let transition_kind = self
            .latest_transition_proof
            .as_ref()
            .and_then(|proof_id| self.proof_kind(proof_id));
        if !matches!(
            transition_kind,
            Some(ProofKind::ImplementationBarrier | ProofKind::ValidationRerunScheduled)
        ) {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_barrier_authority_missing",
            });
        }
        Ok(node)
    }

    fn require_repair_stage_without_active_node(&self) -> Result<(), ProtocolViolation> {
        if self.stage() != ProtocolStage::Repair || self.active_node().is_some() {
            return Err(ProtocolViolation::ValidationContract {
                code: "repair_decision_has_active_or_wrong_owner",
            });
        }
        if self
            .latest_transition_proof
            .as_ref()
            .and_then(|proof_id| self.proof_kind(proof_id))
            != Some(ProofKind::ValidationFailure)
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "repair_validation_failure_authority_missing",
            });
        }
        Ok(())
    }

    fn authoritative_validation_convergence(
        &self,
        validation: &ValidationState,
        policy: &ValidationPolicyV1,
    ) -> Result<ValidationConvergence, ProtocolViolation> {
        match self.stage() {
            ProtocolStage::Validation => {
                let gate = validation
                    .next_gate()
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "validation_convergence_without_gate",
                    })?;
                self.require_active_validation_node(&gate.node_id)?;
                if let Some(run) = validation.run_for_gate(&gate.gate_id)
                    && let Some(completed) = &run.completed
                    && let ValidationProcessResult::InfrastructureFailure { kind, .. } =
                        completed.result
                {
                    let failure_revision_id = FailureRevisionId::new(format!(
                        "epv1:{}",
                        stable_sha256(&[
                            "execution-protocol-v1:validation-infrastructure-failure",
                            run.request.schedule.run_id.as_str(),
                            &completed.completion_hash,
                        ])
                    ));
                    return Ok(ValidationConvergence::new(
                        failure_revision_id,
                        self.repository_revision.clone(),
                        ValidationConvergenceReason::InfrastructureFailure {
                            kind,
                            run_id: run.request.schedule.run_id.clone(),
                        },
                    )?);
                }
                if validation.run_for_gate(&gate.gate_id).is_some() {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_convergence_with_current_gate_run",
                    });
                }
                let runs = validation
                    .runs
                    .values()
                    .filter(|run| run.request.schedule.gate_id == gate.gate_id)
                    .count() as u32;
                if runs.saturating_add(1) <= gate.max_runs {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_convergence_before_gate_budget_exhaustion",
                    });
                }
                let failure_revision_id = FailureRevisionId::new(format!(
                    "epv1:{}",
                    stable_sha256(&[
                        "execution-protocol-v1:validation-run-budget-exhausted",
                        gate.gate_id.as_str(),
                        self.repository_revision.as_str(),
                        &gate.max_runs.to_string(),
                    ])
                ));
                Ok(ValidationConvergence::new(
                    failure_revision_id,
                    self.repository_revision.clone(),
                    ValidationConvergenceReason::GateRunBudgetExhausted {
                        gate_id: gate.gate_id.clone(),
                    },
                )?)
            }
            ProtocolStage::Repair => {
                self.require_repair_stage_without_active_node()?;
                let failure =
                    validation
                        .current_failure()
                        .ok_or(ProtocolViolation::ValidationContract {
                            code: "validation_convergence_without_failure",
                        })?;
                let gate = validation.gates.get(&failure.gate_id).ok_or(
                    ProtocolViolation::ValidationContract {
                        code: "validation_convergence_gate_missing",
                    },
                )?;
                if let Some(exhausted_gate) =
                    validation_repair_rerun_budget_exhausted_gate(validation)
                {
                    return validation_gate_run_budget_convergence(
                        exhausted_gate,
                        &self.repository_revision,
                    );
                }
                let ranking = validation
                    .rankings
                    .get(&failure.failure_revision_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "validation_convergence_without_ranking",
                    })?;
                let evaluation = validation
                    .eligibility
                    .get(&failure.failure_revision_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "validation_convergence_without_eligibility",
                    })?;
                let plan = self
                    .planning
                    .as_ref()
                    .and_then(|planning| planning.accepted_plan.as_ref())
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "validation_convergence_plan_missing",
                    })?;
                let baselines = self.repair_mutation_baselines(failure);
                if select_repair_target(
                    ranking, evaluation, failure, gate, plan, policy, &baselines,
                )?
                .is_some()
                {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "validation_convergence_with_eligible_repair",
                    });
                }
                Ok(ValidationConvergence::new(
                    failure.failure_revision_id.clone(),
                    self.repository_revision.clone(),
                    ValidationConvergenceReason::NoValidRepair,
                )?)
            }
            _ => Err(ProtocolViolation::ValidationContract {
                code: "validation_convergence_outside_validation_or_repair",
            }),
        }
    }

    fn repair_mutation_baselines(
        &self,
        failure: &ValidationFailureRevisionV1,
    ) -> RepairMutationBaselines {
        let Some(implementation) = &self.implementation else {
            return RepairMutationBaselines::new();
        };
        let Some(plan) = self
            .planning
            .as_ref()
            .and_then(|planning| planning.accepted_plan.as_ref())
        else {
            return RepairMutationBaselines::new();
        };
        let mut baselines_by_evidence = BTreeMap::<EvidenceId, RepairMutationBaseline>::new();
        for (node_id, target_id) in &implementation.node_targets {
            let Some(node) = self.nodes.get(node_id) else {
                continue;
            };
            let NodeState::Succeeded { proof_id } = &node.state else {
                continue;
            };
            let Some(proof) = self.proofs.get(proof_id) else {
                continue;
            };
            let Some(evidence) = self
                .mutation
                .current_target(node_id)
                .and_then(|target| target.verified.as_ref())
            else {
                continue;
            };
            if proof.kind != ProofKind::MutationVerified
                || proof.node_ids != vec![node_id.clone()]
                || proof.related_evidence_ids != vec![evidence.evidence_id.clone()]
                || &evidence.node_id != node_id
                || &evidence.target_id != target_id
                || evidence.validate().is_err()
                || !self.event_log.iter().any(|stored| {
                    matches!(
                        &stored.envelope.payload,
                        DomainEvent::Mutation(MutationEvent::MutationVerified {
                            evidence: recorded
                        }) if recorded == evidence
                    )
                })
            {
                continue;
            }
            let Some(target) = plan
                .targets
                .iter()
                .find(|target| &target.target_id == target_id)
            else {
                continue;
            };
            let Ok(baseline) =
                RepairMutationBaseline::from_implementation(plan, target, evidence.clone())
            else {
                continue;
            };
            baselines_by_evidence.insert(evidence.evidence_id.clone(), baseline);
        }

        if let Some(validation) = &self.validation {
            let mut unresolved = validation
                .selections
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>();
            for _ in 0..validation.selections.len() {
                let mut resolved_any = false;
                for failure_id in unresolved.clone() {
                    let Some(selection) = validation.selections.get(&failure_id) else {
                        continue;
                    };
                    let Some(prior_failure) = validation.failures.get(&failure_id) else {
                        continue;
                    };
                    let Some(prior_baseline) =
                        baselines_by_evidence.get(&selection.intent.baseline_mutation_evidence_id)
                    else {
                        continue;
                    };
                    let Some(node) = self.nodes.get(&selection.repair_node.id) else {
                        continue;
                    };
                    let NodeState::Succeeded { proof_id } = &node.state else {
                        continue;
                    };
                    let Some(proof) = self.proofs.get(proof_id) else {
                        continue;
                    };
                    let Some(evidence) = self
                        .mutation
                        .current_target(&node.id)
                        .and_then(|target| target.verified.as_ref())
                    else {
                        continue;
                    };
                    let mutation_parents = proof
                        .related_proof_ids
                        .iter()
                        .filter_map(|proof_id| self.proofs.get(proof_id))
                        .filter(|parent| parent.kind == ProofKind::MutationVerified)
                        .collect::<Vec<_>>();
                    let eligibility_parents = proof
                        .related_proof_ids
                        .iter()
                        .filter_map(|proof_id| self.proofs.get(proof_id))
                        .filter(|parent| parent.kind == ProofKind::RepairEligibility)
                        .collect::<Vec<_>>();
                    if proof.kind != ProofKind::RepairVerified
                        || proof.repository_revision != evidence.repository_revision_after
                        || proof.node_ids != vec![node.id.clone()]
                        || proof.related_proof_ids.len() != 2
                        || proof.related_evidence_ids != vec![evidence.evidence_id.clone()]
                        || mutation_parents.len() != 1
                        || eligibility_parents.len() != 1
                        || !self
                            .canonical_repair_verification_proof(
                                selection,
                                evidence,
                                eligibility_parents[0],
                                mutation_parents[0],
                            )
                            .is_ok_and(|canonical| &canonical == proof)
                        || !self.event_log.iter().any(|stored| {
                            matches!(
                                &stored.envelope.payload,
                                DomainEvent::Mutation(MutationEvent::MutationVerified {
                                    evidence: recorded
                                }) if recorded == evidence
                            )
                        })
                    {
                        continue;
                    }
                    let Ok(baseline) = RepairMutationBaseline::from_verified_repair(
                        plan,
                        prior_failure,
                        selection,
                        prior_baseline,
                        evidence.clone(),
                    ) else {
                        continue;
                    };
                    baselines_by_evidence.insert(evidence.evidence_id.clone(), baseline);
                    unresolved.remove(&failure_id);
                    resolved_any = true;
                }
                if !resolved_any {
                    break;
                }
            }
        }

        let mut baselines = RepairMutationBaselines::new();
        let mut ambiguous_targets = BTreeSet::new();
        for baseline in baselines_by_evidence.into_values().filter(|baseline| {
            baseline.evidence().repository_revision_after == failure.repository_revision
        }) {
            let target_id = baseline.evidence().target_id.clone();
            if ambiguous_targets.contains(&target_id) {
                continue;
            }
            match baselines.entry(target_id.clone()) {
                Entry::Occupied(entry) => {
                    entry.remove();
                    ambiguous_targets.insert(target_id);
                }
                Entry::Vacant(entry) => {
                    entry.insert(baseline);
                }
            }
        }
        baselines
    }

    fn pending_verified_repair_handoff(
        &self,
    ) -> Result<Option<&MutationVerificationEvidence>, ProtocolViolation> {
        let Some(validation) = &self.validation else {
            return Ok(None);
        };
        if validation.repository_revision == self.repository_revision {
            return Ok(None);
        }
        if self.stage() != ProtocolStage::Repair {
            return Ok(None);
        }
        let failure =
            validation
                .current_failure()
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "verified_repair_handoff_failure_missing",
                })?;
        if validation
            .invalidations
            .contains_key(&failure.failure_revision_id)
        {
            return Ok(None);
        }
        let selection = validation
            .selections
            .get(&failure.failure_revision_id)
            .ok_or(ProtocolViolation::ValidationContract {
                code: "verified_repair_handoff_selection_missing",
            })?;
        let node = self.nodes.get(&selection.repair_node.id).ok_or_else(|| {
            ProtocolViolation::UnknownNode {
                node_id: selection.repair_node.id.clone(),
            }
        })?;
        if !matches!(
            node.state,
            NodeState::Active { .. } | NodeState::Succeeded { .. }
        ) {
            return Ok(None);
        }
        let context = validation
            .repair_contexts
            .context_for_node(&node.id)
            .ok_or(ProtocolViolation::ValidationContract {
                code: "verified_repair_handoff_context_missing",
            })?;
        let Some(evidence) = self
            .mutation
            .current_target(&node.id)
            .and_then(|target| target.verified.as_ref())
        else {
            return Ok(None);
        };
        if evidence.node_id != node.id
            || evidence.target_id != selection.intent.target_id
            || evidence.context_manifest_id != context.context_manifest_id
            || evidence.repository_revision_before != validation.repository_revision
            || evidence.repository_revision_before != selection.intent.repository_revision
            || evidence.repository_revision_after != self.repository_revision
            || evidence.validate().is_err()
            || !self.event_log.iter().any(|stored| {
                matches!(
                    &stored.envelope.payload,
                    DomainEvent::Mutation(MutationEvent::MutationVerified { evidence: recorded })
                        if recorded == evidence
                )
            })
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "verified_repair_handoff_binding_invalid",
            });
        }
        Ok(Some(evidence))
    }

    fn pending_repair_convergence_revision_adoption(&self) -> Result<bool, ProtocolViolation> {
        let Some(validation) = &self.validation else {
            return Ok(false);
        };
        if validation.repository_revision == self.repository_revision
            || !matches!(
                self.stage(),
                ProtocolStage::Repair | ProtocolStage::Terminal
            )
        {
            return Ok(false);
        }
        let Some(failure) = validation.current_failure() else {
            return Ok(false);
        };
        let Some(selection) = validation.selections.get(&failure.failure_revision_id) else {
            return Ok(false);
        };
        let Some(node) = self.nodes.get(&selection.repair_node.id) else {
            return Ok(false);
        };
        if !matches!(
            node.state,
            NodeState::Active { .. } | NodeState::FailedTerminal { .. }
        ) {
            return Ok(false);
        }
        let Some(convergence) = self
            .mutation
            .current_target(&node.id)
            .and_then(|target| target.convergence.as_ref())
        else {
            return Ok(false);
        };
        let valid = convergence.node_id == node.id
            && convergence.target_id == selection.intent.target_id
            && convergence.repository_revision == validation.repository_revision
            && convergence.repository_revision_after == self.repository_revision
            && convergence.repository_drift.is_some()
            && matches!(
                convergence.reason,
                MutationConvergenceReason::ContextRebuildUnavailable
                    | MutationConvergenceReason::MutationAttemptBudgetExhausted
                    | MutationConvergenceReason::ContextRebuildBudgetExhausted
            )
            && self.event_log.iter().any(|stored| {
                matches!(
                    &stored.envelope.payload,
                    DomainEvent::Mutation(MutationEvent::ConvergenceEvaluated {
                        convergence: recorded
                    }) if recorded == convergence
                )
            });
        if !valid {
            return Ok(false);
        }
        if self.stage() == ProtocolStage::Terminal {
            let Some(terminal) = self.terminal.as_ref() else {
                return Ok(false);
            };
            return Ok(self
                .authoritative_repair_mutation_terminal_result()?
                .as_ref()
                == Some(terminal));
        }
        Ok(true)
    }

    fn pending_review_drift_revision_adoption(&self) -> Result<bool, ProtocolViolation> {
        let Some(review) = &self.review else {
            return Ok(false);
        };
        if review.repository_revision == self.repository_revision
            || !matches!(
                self.stage(),
                ProtocolStage::Review | ProtocolStage::Terminal
            )
        {
            return Ok(false);
        }
        let Some(request) = &review.diff_request else {
            return Ok(false);
        };
        let Some(failure) = &review.diff_manifest_failure else {
            return Ok(false);
        };
        let Some(convergence) = &review.convergence else {
            return Ok(false);
        };
        let DiffManifestEffectFailureReasonV1::RepositoryDrift {
            observed_revision,
            observed_repository_fingerprint,
        } = &failure.reason
        else {
            return Ok(false);
        };
        let ReviewConvergenceReasonV1::RepositoryDrift {
            observed_revision: convergence_revision,
            failure_id,
            failure_hash,
        } = &convergence.reason
        else {
            return Ok(false);
        };
        Ok(request.repository_revision == review.repository_revision
            && request.repository_fingerprint == review.ancestry.repository_fingerprint
            && failure.repository_revision == review.repository_revision
            && failure.repository_fingerprint == review.ancestry.repository_fingerprint
            && failure.validate_against(request).is_ok()
            && (observed_revision != &review.repository_revision
                || observed_repository_fingerprint != &review.ancestry.repository_fingerprint)
            && observed_revision == convergence_revision
            && failure_id == &failure.failure_id
            && failure_hash == &failure.failure_hash
            && observed_revision == &self.repository_revision
            && convergence.repository_revision == review.repository_revision
            && convergence.policy_id == review.policy_id
            && self.event_log.iter().any(|stored| {
                matches!(
                    &stored.envelope.payload,
                    DomainEvent::Review(ReviewEvent::DiffManifestBuildFailed {
                        failure: recorded,
                    }) if recorded == failure
                )
            })
            && self.event_log.iter().any(|stored| {
                matches!(
                    &stored.envelope.payload,
                    DomainEvent::Review(ReviewEvent::ConvergenceEvaluated {
                        convergence: recorded,
                    }) if recorded == convergence
                )
            }))
    }

    fn authoritative_repair_invalidation(
        &self,
        failure: &ValidationFailureRevisionV1,
        selection: &RepairTargetSelection,
    ) -> Result<ValidationInvalidation, ProtocolViolation> {
        let validation = self
            .validation
            .as_ref()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_state_missing",
            })?;
        if validation.current_failure() != Some(failure)
            || selection.intent.failure_revision_id != failure.failure_revision_id
            || selection.intent.repository_revision != validation.repository_revision
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_invalidation_failure_binding_mismatch",
            });
        }
        let repair_node = self.nodes.get(&selection.repair_node.id).ok_or_else(|| {
            ProtocolViolation::UnknownNode {
                node_id: selection.repair_node.id.clone(),
            }
        })?;
        let NodeState::Succeeded { proof_id } = &repair_node.state else {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_invalidation_without_verified_repair",
            });
        };
        let proof = self
            .proofs
            .get(proof_id)
            .ok_or_else(|| ProtocolViolation::UnknownProof {
                proof_id: proof_id.clone(),
            })?;
        let evidence = self
            .mutation
            .current_target(&repair_node.id)
            .and_then(|target| target.verified.as_ref())
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_invalidation_repair_evidence_missing",
            })?;
        if proof.kind != ProofKind::RepairVerified
            || proof.node_ids != vec![repair_node.id.clone()]
            || proof.related_evidence_ids != vec![evidence.evidence_id.clone()]
            || evidence.node_id != repair_node.id
            || evidence.target_id != selection.intent.target_id
            || evidence.repository_revision_before != validation.repository_revision
            || evidence.repository_revision_after != self.repository_revision
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_invalidation_repair_binding_mismatch",
            });
        }
        let invalidated_evidence_ids = validation
            .evidence
            .values()
            .filter(|evidence| evidence.repository_revision == validation.repository_revision)
            .map(|evidence| evidence.evidence_id.clone())
            .collect::<BTreeSet<_>>();
        if invalidated_evidence_ids.is_empty() {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_invalidation_evidence_empty",
            });
        }
        Ok(ValidationInvalidation {
            failure_revision_id: failure.failure_revision_id.clone(),
            repair_intent_id: selection.intent.repair_intent_id.clone(),
            repository_revision_before: validation.repository_revision.clone(),
            repository_revision_after: self.repository_revision.clone(),
            invalidated_evidence_ids,
            verified_repair_evidence_id: evidence.evidence_id.clone(),
        })
    }

    fn validate_pending_repair_handoff_event(
        &self,
        payload: &DomainEvent,
    ) -> Result<(), ProtocolViolation> {
        let Some(evidence) = self.pending_verified_repair_handoff()? else {
            return Ok(());
        };
        let validation = self
            .validation
            .as_ref()
            .expect("repair handoff has validation");
        let failure = validation
            .current_failure()
            .expect("repair handoff has a failure");
        let selection = validation
            .selections
            .get(&failure.failure_revision_id)
            .expect("repair handoff has a selection");
        let node = self
            .nodes
            .get(&selection.repair_node.id)
            .expect("repair handoff has a node");
        let allowed = match &node.state {
            NodeState::Active { .. } => {
                let mutation_proof = self.mutation_verification_proof(evidence)?;
                if self.proofs.get(&mutation_proof.id) != Some(&mutation_proof) {
                    matches!(
                        payload,
                        DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof })
                            if proof == &mutation_proof
                    )
                } else {
                    let repair_proof = self.repair_verification_proof(evidence)?;
                    if self.proofs.get(&repair_proof.id) != Some(&repair_proof) {
                        matches!(
                            payload,
                            DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof })
                                if proof == &repair_proof
                        )
                    } else {
                        matches!(
                            payload,
                            DomainEvent::Graph(GraphEvent::NodeSucceeded { node_id, proof_id })
                                if node_id == &node.id && proof_id == &repair_proof.id
                        )
                    }
                }
            }
            NodeState::Succeeded { .. } => {
                let invalidation = self.authoritative_repair_invalidation(failure, selection)?;
                matches!(
                    payload,
                    DomainEvent::Validation(
                        ValidationEvent::PriorValidationInvalidated { invalidation: actual }
                    ) if actual == &invalidation
                )
            }
            _ => false,
        };
        if !allowed {
            return Err(ProtocolViolation::ValidationContract {
                code: "verified_repair_handoff_progress_frozen",
            });
        }
        Ok(())
    }

    fn apply_profile_event(&mut self, event: &ProfileEvent) -> Result<(), ProtocolViolation> {
        match event {
            ProfileEvent::RepositoryProfileRecorded { profile } => {
                if self.stage() != ProtocolStage::Profiling {
                    return Err(ProtocolViolation::RepositoryProfile {
                        code: "repository_profile_recorded_outside_profiling",
                    });
                }
                if self.repository_profile.is_some() {
                    return Err(ProtocolViolation::RepositoryProfile {
                        code: "repository_profile_already_recorded",
                    });
                }
                profile.validate()?;
                if profile.repository_revision != self.repository_revision {
                    return Err(ProtocolViolation::RepositoryProfile {
                        code: "repository_profile_revision_mismatch",
                    });
                }
                self.repository_profile = Some(profile.clone());
                Ok(())
            }
        }
    }

    fn apply_discovery_event(&mut self, event: &DiscoveryEvent) -> Result<(), ProtocolViolation> {
        match event {
            DiscoveryEvent::GoalRecorded { goal } => self.record_discovery_goal(goal.clone())?,
            DiscoveryEvent::ActionPrepared { prepared } => {
                self.prepare_discovery_action((**prepared).clone())?
            }
            DiscoveryEvent::ActionReleased { action_id } => {
                self.release_discovery_action(action_id)?
            }
            DiscoveryEvent::ActionRejected { action_id, reason } => {
                self.reject_discovery_action(action_id, *reason)?
            }
            DiscoveryEvent::SearchCompleted {
                action_id,
                evidence,
            } => self.record_discovery_search(action_id, evidence.clone())?,
            DiscoveryEvent::CandidatesRecorded {
                search_id,
                candidates,
            } => self.record_discovery_candidates(search_id, candidates)?,
            DiscoveryEvent::FileEvidenceRecorded {
                action_id,
                evidence,
                unresolved_questions,
            } => self.record_discovery_files(action_id, evidence, unresolved_questions)?,
            DiscoveryEvent::RelationshipEvidenceRecorded {
                action_id,
                evidence,
            } => self.record_discovery_relationships(action_id, evidence)?,
            DiscoveryEvent::ImpactMapRecorded {
                action_id,
                evidence,
            } => self.record_discovery_impact_map(action_id.as_ref(), evidence.clone())?,
            DiscoveryEvent::ConvergenceEvaluated { convergence } => {
                self.record_discovery_convergence(convergence.clone())?
            }
        }
        self.refresh_discovery_position();
        Ok(())
    }

    fn record_discovery_goal(&mut self, goal: DiscoveryGoal) -> Result<(), ProtocolViolation> {
        if self.stage() != ProtocolStage::Profiling {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_goal_recorded_outside_profiling",
            });
        }
        let profile =
            self.repository_profile
                .as_ref()
                .ok_or(ProtocolViolation::DiscoveryContract {
                    code: "discovery_goal_requires_repository_profile",
                })?;
        if self.discovery.is_some() {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_goal_already_recorded",
            });
        }
        let discovery = DiscoveryState::new(
            NodeId::new("protocol-v1:discovery"),
            self.repository_revision.clone(),
            profile.profile_id.clone(),
            goal,
        );
        discovery.validate()?;
        self.discovery = Some(discovery);
        Ok(())
    }

    fn prepare_discovery_action(
        &mut self,
        prepared: PreparedDiscoveryAction,
    ) -> Result<(), ProtocolViolation> {
        self.require_active_discovery_node()?;
        if self.current_discovery_action.is_some() {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_action_already_active",
            });
        }
        let discovery = self
            .discovery
            .as_ref()
            .ok_or(ProtocolViolation::DiscoveryContract {
                code: "discovery_state_missing",
            })?;
        if discovery.convergence.is_some() {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_action_after_convergence",
            });
        }
        prepared.context.validate()?;
        prepared
            .envelope
            .validate_against_context(&prepared.context)?;
        if prepared.envelope.node_id != discovery.node_id
            || prepared.envelope.repository_revision != self.repository_revision
            || prepared.envelope.budget_owner != discovery.node_id
            || prepared.admission.node_id != discovery.node_id
            || prepared.admission.action_id != prepared.envelope.action_id
            || prepared.admission.call_id.as_str() != prepared.envelope.reservation_id.as_str()
            || prepared.admission.payload_hash != prepared.envelope.payload_identity
            || prepared.admission.input_tokens != prepared.context.estimated_input_tokens
            || prepared.admission.output_tokens != prepared.envelope.output_token_allowance
        {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_action_admission_binding_mismatch",
            });
        }
        self.validate_discovery_action_constraints(discovery, &prepared.envelope)?;
        let remaining = self.discovery_budget_remaining(&discovery.node_id)?;
        let DiscoveryNextStep::Action(expected_class) =
            select_next_discovery_step(discovery, remaining.admissible_model_calls())
        else {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_action_prepared_when_convergence_required",
            });
        };
        if prepared.envelope.action_class != expected_class {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_action_class_not_authoritative",
            });
        }
        let expected = authoritative_prepared_discovery_action(self)?;
        if prepared != expected {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_action_not_authoritative",
            });
        }
        self.current_discovery_action = Some(prepared);
        Ok(())
    }

    fn release_discovery_action(&mut self, action_id: &ActionId) -> Result<(), ProtocolViolation> {
        let prepared = self.require_current_discovery_action(action_id)?;
        let record = self
            .budgets
            .model_calls
            .get(&prepared.admission.call_id)
            .ok_or_else(|| ProtocolViolation::ModelCallLifecycle {
                call_id: prepared.admission.call_id.clone(),
                code: "discovery_action_release_without_call",
            })?;
        if record.state != ModelCallState::ReconciledReleased {
            return Err(ProtocolViolation::ModelCallLifecycle {
                call_id: prepared.admission.call_id.clone(),
                code: "discovery_action_release_before_reconciliation",
            });
        }
        self.current_discovery_action = None;
        Ok(())
    }

    fn reject_discovery_action(
        &mut self,
        action_id: &ActionId,
        _reason: DiscoveryActionRejectionReason,
    ) -> Result<(), ProtocolViolation> {
        self.require_consumed_discovery_action(action_id)?;
        self.current_discovery_action = None;
        Ok(())
    }

    fn record_discovery_search(
        &mut self,
        action_id: &ActionId,
        evidence: SearchEvidence,
    ) -> Result<(), ProtocolViolation> {
        let prepared = self.require_consumed_discovery_action(action_id)?;
        let DiscoveryActionConstraints::Search { request } = &prepared.envelope.constraints else {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "search_observation_for_non_search_action",
            });
        };
        if request != &evidence.request {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "search_observation_request_mismatch",
            });
        }
        let discovery = self.discovery.as_mut().expect("discovery was checked");
        if discovery
            .completed_searches
            .contains_key(&evidence.request.search_id)
        {
            return Err(ProtocolViolation::DuplicateSearch {
                search_id: evidence.request.search_id,
            });
        }
        discovery
            .completed_searches
            .insert(evidence.request.search_id.clone(), evidence);
        self.current_discovery_action = None;
        self.discovery
            .as_ref()
            .expect("discovery exists")
            .validate()?;
        Ok(())
    }

    fn record_discovery_candidates(
        &mut self,
        search_id: &SearchId,
        candidates: &[CandidatePathEvidence],
    ) -> Result<(), ProtocolViolation> {
        self.require_active_discovery_node()?;
        if self.current_discovery_action.is_some() {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "candidate_projection_with_active_action",
            });
        }
        let discovery = self
            .discovery
            .as_ref()
            .ok_or(ProtocolViolation::DiscoveryContract {
                code: "discovery_state_missing",
            })?;
        let expected = canonical_candidate_projection(discovery, search_id)?;
        if candidates.is_empty() || candidates != expected {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "candidate_projection_not_authoritative",
            });
        }
        let discovery = self.discovery.as_mut().expect("discovery was checked");
        for candidate in candidates.iter().cloned() {
            discovery
                .candidates
                .insert(candidate.path.clone(), candidate);
        }
        discovery.validate()?;
        Ok(())
    }

    fn record_discovery_files(
        &mut self,
        action_id: &ActionId,
        evidence: &[FileEvidence],
        unresolved_questions: &[UnresolvedQuestion],
    ) -> Result<(), ProtocolViolation> {
        let prepared = self.require_consumed_discovery_action(action_id)?;
        let permitted_paths = match &prepared.envelope.constraints {
            DiscoveryActionConstraints::ExactPaths { paths }
            | DiscoveryActionConstraints::NamedRelationship { paths, .. } => paths,
            _ => {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "file_observation_for_non_read_action",
                });
            }
        };
        let observed_paths = evidence
            .iter()
            .map(|item| item.path.clone())
            .collect::<BTreeSet<_>>();
        if evidence.is_empty()
            || observed_paths.len() != evidence.len()
            || &observed_paths != permitted_paths
        {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "file_observation_paths_incomplete",
            });
        }
        if unresolved_questions
            .iter()
            .map(|question| &question.id)
            .collect::<BTreeSet<_>>()
            .len()
            != unresolved_questions.len()
        {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "unresolved_question_observation_duplicate",
            });
        }
        let discovery = self.discovery.as_mut().expect("discovery was checked");
        for item in evidence {
            if discovery.file_evidence.contains_key(&item.evidence_id) {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "file_evidence_already_recorded",
                });
            }
        }
        for question in unresolved_questions {
            let subject_candidate = discovery.candidates.get(&question.subject_path);
            if discovery.unresolved_questions.contains_key(&question.id)
                || !permitted_paths.contains(&question.subject_path)
                || question.criterion_ids.is_empty()
                || subject_candidate.is_none()
                || !question.criterion_ids.is_subset(
                    &subject_candidate
                        .expect("question subject candidate was checked")
                        .criterion_ids,
                )
            {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "unresolved_question_observation_invalid",
                });
            }
        }
        for item in evidence.iter().cloned() {
            discovery
                .file_evidence
                .insert(item.evidence_id.clone(), item);
        }
        for question in unresolved_questions.iter().cloned() {
            discovery
                .unresolved_questions
                .insert(question.id.clone(), question);
        }
        self.current_discovery_action = None;
        self.discovery
            .as_ref()
            .expect("discovery exists")
            .validate()?;
        Ok(())
    }

    fn record_discovery_relationships(
        &mut self,
        action_id: &ActionId,
        evidence: &[RelationshipEvidence],
    ) -> Result<(), ProtocolViolation> {
        let prepared = self.require_consumed_discovery_action(action_id)?;
        let DiscoveryActionConstraints::NamedRelationship {
            question, paths, ..
        } = &prepared.envelope.constraints
        else {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "relationship_observation_for_wrong_action",
            });
        };
        let discovery = self.discovery.as_ref().expect("discovery was checked");
        let subject_path = &question.subject_path;
        if evidence.is_empty()
            || evidence.len() > MAX_ACTION_PATHS
            || evidence.iter().any(|item| {
                let touches_subject = item.from == *subject_path || item.to == *subject_path;
                let related_path = if item.from == *subject_path {
                    &item.to
                } else {
                    &item.from
                };
                let related_path_is_authorized = paths.contains(related_path)
                    || discovery.candidates.contains_key(related_path)
                    || matches!(
                        question.kind,
                        RelationshipKind::Tests | RelationshipKind::TestedBy
                    );
                item.kind != question.kind
                    || !touches_subject
                    || !paths.contains(subject_path)
                    || !related_path_is_authorized
            })
        {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "relationship_observation_outside_authorized_paths",
            });
        }
        if evidence.iter().any(|item| {
            !item
                .supporting_evidence_ids
                .is_subset(&prepared.context.evidence_ids)
        }) {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "relationship_support_outside_prepared_context",
            });
        }
        if evidence.iter().any(|item| {
            !item.supporting_evidence_ids.iter().all(|support| {
                discovery.non_relationship_evidence_touches_path(support, subject_path)
            })
        }) {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "relationship_support_not_subject_evidence",
            });
        }
        let discovery = self.discovery.as_mut().expect("discovery was checked");
        for item in evidence {
            if discovery.relationships.contains_key(&item.evidence_id) {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "relationship_evidence_already_recorded",
                });
            }
        }
        for item in evidence.iter().cloned() {
            discovery
                .relationships
                .insert(item.evidence_id.clone(), item);
        }
        discovery.unresolved_questions.remove(&question.id);
        self.current_discovery_action = None;
        self.discovery
            .as_ref()
            .expect("discovery exists")
            .validate()?;
        Ok(())
    }

    fn record_discovery_impact_map(
        &mut self,
        action_id: Option<&ActionId>,
        evidence: ImpactMapEvidence,
    ) -> Result<(), ProtocolViolation> {
        self.require_active_discovery_node()?;
        if self.discovery.as_ref().is_some_and(|state| {
            state
                .impact_map
                .as_ref()
                .is_some_and(|impact_map| impact_map_is_complete(state, impact_map))
        }) {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "complete_impact_map_already_recorded",
            });
        }
        if let Some(action_id) = action_id {
            let prepared = self.require_consumed_discovery_action(action_id)?;
            let DiscoveryActionConstraints::ImpactMap {
                criterion_ids,
                evidence_ids,
            } = &prepared.envelope.constraints
            else {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "impact_map_for_wrong_action",
                });
            };
            if evidence.areas.iter().any(|area| {
                !criterion_ids.contains(&area.criterion_id)
                    || !area.evidence_ids.is_subset(evidence_ids)
            }) {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "impact_map_observation_outside_action_context",
                });
            }
        } else {
            let Some(DiscoveryConvergence::EvidenceSufficientForDeterministicImpactMap { .. }) =
                self.discovery
                    .as_ref()
                    .and_then(|state| state.convergence.as_ref())
            else {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "deterministic_impact_map_without_convergence",
                });
            };
            let expected = self.deterministic_discovery_impact_map()?;
            if expected != evidence {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "deterministic_impact_map_mismatch",
                });
            }
        }
        let discovery = self.discovery.as_mut().expect("discovery was checked");
        discovery.impact_map = Some(evidence);
        if action_id.is_some() {
            self.current_discovery_action = None;
        }
        // The accepted convergence is a separate authoritative event. Clearing
        // the prior deterministic-synthesis decision preserves the ordering:
        // convergence evaluated -> impact map recorded -> map accepted.
        discovery.convergence = None;
        discovery.validate()?;
        Ok(())
    }

    fn record_discovery_convergence(
        &mut self,
        convergence: DiscoveryConvergence,
    ) -> Result<(), ProtocolViolation> {
        self.require_active_discovery_node()?;
        if self.current_discovery_action.is_some() {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "convergence_with_active_discovery_action",
            });
        }
        let discovery_node_id = self
            .discovery
            .as_ref()
            .ok_or(ProtocolViolation::DiscoveryContract {
                code: "discovery_state_missing",
            })?
            .node_id
            .clone();
        let remaining = self.discovery_budget_remaining(&discovery_node_id)?;
        let discovery = self
            .discovery
            .as_mut()
            .ok_or(ProtocolViolation::DiscoveryContract {
                code: "discovery_state_missing",
            })?;
        let requires_exact_exhaustion =
            !matches!(convergence, DiscoveryConvergence::ImpactMapAccepted { .. });
        let early_search_exhaustion = matches!(
            convergence,
            DiscoveryConvergence::InsufficientEvidence { .. }
        ) && all_goal_searches_completed(discovery);
        if requires_exact_exhaustion
            && remaining.admissible_model_calls() != 0
            && !early_search_exhaustion
        {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_convergence_before_exact_exhaustion",
            });
        }
        if discovery.convergence.is_some()
            || convergence != evaluate_discovery_convergence(discovery)
        {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_convergence_not_authoritative",
            });
        }
        discovery.convergence = Some(convergence);
        discovery.validate()?;
        Ok(())
    }

    fn require_active_discovery_node(&self) -> Result<(), ProtocolViolation> {
        if self.stage() != ProtocolStage::Discovery {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_event_outside_discovery",
            });
        }
        let Some(node) = self.active_node() else {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_event_without_active_node",
            });
        };
        if node.kind != NodeKind::Discovery || node.id.as_str() != "protocol-v1:discovery" {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_event_owner_mismatch",
            });
        }
        Ok(())
    }

    fn require_current_discovery_action(
        &self,
        action_id: &ActionId,
    ) -> Result<PreparedDiscoveryAction, ProtocolViolation> {
        self.require_active_discovery_node()?;
        let prepared =
            self.current_discovery_action
                .as_ref()
                .ok_or(ProtocolViolation::DiscoveryContract {
                    code: "discovery_action_missing",
                })?;
        if &prepared.envelope.action_id != action_id {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_action_identity_mismatch",
            });
        }
        Ok(prepared.clone())
    }

    fn require_consumed_discovery_action(
        &self,
        action_id: &ActionId,
    ) -> Result<PreparedDiscoveryAction, ProtocolViolation> {
        let prepared = self.require_current_discovery_action(action_id)?;
        let record = self
            .budgets
            .model_calls
            .get(&prepared.admission.call_id)
            .ok_or_else(|| ProtocolViolation::ModelCallLifecycle {
                call_id: prepared.admission.call_id.clone(),
                code: "discovery_observation_without_call",
            })?;
        if !matches!(record.state, ModelCallState::ReconciledConsumed { .. }) {
            return Err(ProtocolViolation::ModelCallLifecycle {
                call_id: prepared.admission.call_id.clone(),
                code: "discovery_observation_before_consumed_reconciliation",
            });
        }
        Ok(prepared)
    }

    fn require_active_planning_node(&self) -> Result<(), ProtocolViolation> {
        if self.stage() != ProtocolStage::Planning {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_event_outside_planning",
            });
        }
        let planning = self
            .planning
            .as_ref()
            .ok_or(ProtocolViolation::PlanningContract {
                code: "planning_state_missing",
            })?;
        let Some(node) = self.active_node() else {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_event_without_active_node",
            });
        };
        if node.kind != NodeKind::Planning || node.id != planning.node_id {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_event_owner_mismatch",
            });
        }
        Ok(())
    }

    fn require_current_planning_action(
        &self,
        action_id: &ActionId,
    ) -> Result<PreparedPlanningAction, ProtocolViolation> {
        self.require_active_planning_node()?;
        let prepared =
            self.current_planning_action
                .as_ref()
                .ok_or(ProtocolViolation::PlanningContract {
                    code: "planning_action_missing",
                })?;
        if &prepared.envelope.action_id != action_id {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_action_identity_mismatch",
            });
        }
        Ok(prepared.clone())
    }

    fn require_consumed_planning_action(
        &self,
        action_id: &ActionId,
        call_id: Option<&ModelCallId>,
    ) -> Result<PreparedPlanningAction, ProtocolViolation> {
        let prepared = self.require_current_planning_action(action_id)?;
        if call_id.is_some_and(|call_id| call_id != &prepared.admission.call_id) {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_action_call_identity_mismatch",
            });
        }
        let record = self
            .budgets
            .model_calls
            .get(&prepared.admission.call_id)
            .ok_or_else(|| ProtocolViolation::ModelCallLifecycle {
                call_id: prepared.admission.call_id.clone(),
                code: "planning_observation_without_call",
            })?;
        if !matches!(record.state, ModelCallState::ReconciledConsumed { .. }) {
            return Err(ProtocolViolation::ModelCallLifecycle {
                call_id: prepared.admission.call_id.clone(),
                code: "planning_observation_before_consumed_reconciliation",
            });
        }
        Ok(prepared)
    }

    fn discovery_budget_remaining(
        &self,
        node_id: &NodeId,
    ) -> Result<DiscoveryBudgetRemaining, ProtocolViolation> {
        let node = self
            .nodes
            .get(node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: node_id.clone(),
            })?;
        let node_model_calls = node.budget.max_model_calls.saturating_sub(
            node.usage
                .model_calls_consumed
                .saturating_add(node.usage.model_calls_reserved),
        );
        let mission_model_calls = self.mission_budget.max_model_calls.saturating_sub(
            self.budgets
                .mission_usage
                .model_calls_consumed
                .saturating_add(self.budgets.mission_usage.model_calls_reserved),
        );
        let node_cost = node.budget.max_cost_micros.saturating_sub(
            node.usage
                .cost_micros_consumed
                .saturating_add(node.usage.cost_micros_reserved),
        );
        let mission_cost = self.mission_budget.max_cost_micros.saturating_sub(
            self.budgets
                .mission_usage
                .cost_micros_consumed
                .saturating_add(self.budgets.mission_usage.cost_micros_reserved),
        );
        let node_duration = node.budget.max_duration_ms.saturating_sub(
            node.usage
                .duration_ms_consumed
                .saturating_add(node.usage.duration_ms_reserved),
        );
        let mission_duration = self.mission_budget.max_duration_ms.saturating_sub(
            self.budgets
                .mission_usage
                .duration_ms_consumed
                .saturating_add(self.budgets.mission_usage.duration_ms_reserved),
        );
        Ok(DiscoveryBudgetRemaining {
            model_calls: node_model_calls.min(mission_model_calls),
            cost_micros: node_cost.min(mission_cost),
            duration_ms: node_duration.min(mission_duration),
        })
    }

    fn discovery_budget_is_exhausted(&self, node_id: &NodeId) -> Result<bool, ProtocolViolation> {
        Ok(self.discovery_budget_remaining(node_id)?.is_exhausted())
    }

    fn planning_budget_remaining(
        &self,
        node_id: &NodeId,
    ) -> Result<PlanningBudgetRemaining, ProtocolViolation> {
        let node = self
            .nodes
            .get(node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: node_id.clone(),
            })?;
        Ok(PlanningBudgetRemaining {
            model_calls: node
                .budget
                .max_model_calls
                .saturating_sub(
                    node.usage
                        .model_calls_consumed
                        .saturating_add(node.usage.model_calls_reserved),
                )
                .min(
                    self.mission_budget.max_model_calls.saturating_sub(
                        self.budgets
                            .mission_usage
                            .model_calls_consumed
                            .saturating_add(self.budgets.mission_usage.model_calls_reserved),
                    ),
                ),
            cost_micros: node
                .budget
                .max_cost_micros
                .saturating_sub(
                    node.usage
                        .cost_micros_consumed
                        .saturating_add(node.usage.cost_micros_reserved),
                )
                .min(
                    self.mission_budget.max_cost_micros.saturating_sub(
                        self.budgets
                            .mission_usage
                            .cost_micros_consumed
                            .saturating_add(self.budgets.mission_usage.cost_micros_reserved),
                    ),
                ),
            duration_ms: node
                .budget
                .max_duration_ms
                .saturating_sub(
                    node.usage
                        .duration_ms_consumed
                        .saturating_add(node.usage.duration_ms_reserved),
                )
                .min(
                    self.mission_budget.max_duration_ms.saturating_sub(
                        self.budgets
                            .mission_usage
                            .duration_ms_consumed
                            .saturating_add(self.budgets.mission_usage.duration_ms_reserved),
                    ),
                ),
        })
    }

    fn plan_graph_mission_capacity(&self) -> PlanMissionCapacity {
        let mut consumed_model_calls = 0_u32;
        let mut consumed_cost_micros = 0_u64;
        let mut consumed_duration_ms = 0_u64;
        for node in self
            .nodes
            .values()
            .filter(|node| matches!(node.kind, NodeKind::Discovery | NodeKind::Planning))
        {
            consumed_model_calls =
                consumed_model_calls.saturating_add(node.usage.model_calls_consumed);
            consumed_cost_micros =
                consumed_cost_micros.saturating_add(node.usage.cost_micros_consumed);
            consumed_duration_ms =
                consumed_duration_ms.saturating_add(node.usage.duration_ms_consumed);
        }
        PlanMissionCapacity {
            remaining_model_calls: self
                .mission_budget
                .max_model_calls
                .saturating_sub(consumed_model_calls),
            remaining_cost_micros: self
                .mission_budget
                .max_cost_micros
                .saturating_sub(consumed_cost_micros),
            remaining_duration_ms: self
                .mission_budget
                .max_duration_ms
                .saturating_sub(consumed_duration_ms),
        }
    }

    fn authoritative_planning_convergence(
        &self,
        planning: &PlanningState,
    ) -> Result<PlanningConvergence, ProtocolViolation> {
        if !self
            .planning_budget_remaining(&planning.node_id)?
            .is_exhausted()
        {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_convergence_before_budget_exhaustion",
            });
        }
        let violations = planning.latest_violations();
        if violations
            .iter()
            .any(|violation| matches!(violation, PlanViolation::EvidenceGapReported { .. }))
        {
            Ok(PlanningConvergence::InsufficientEvidence { violations })
        } else {
            Ok(PlanningConvergence::BudgetBlocked { violations })
        }
    }

    fn materialized_planning_nodes(
        &self,
        plan: &AcceptedPlan,
    ) -> Result<Vec<NodeSpec>, ProtocolViolation> {
        Ok(materialize_accepted_plan(plan, &self.plan_graph_budget)?.nodes)
    }

    fn planning_acceptance_proof(&self, plan: &AcceptedPlan) -> ProofRecord {
        let planning = self.planning.as_ref().expect("typed planning exists");
        let related_evidence_ids = std::iter::once(planning.discovery_impact_map_id.clone())
            .chain(
                plan.targets
                    .iter()
                    .flat_map(|target| target.required_evidence.iter().cloned()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let source_proof_id =
            self.event_log
                .iter()
                .find_map(|stored| match &stored.envelope.payload {
                    DomainEvent::Lifecycle(LifecycleEvent::PositionAdvanced {
                        from: ProtocolStage::Discovery,
                        to: ProtocolStage::Planning,
                        proof_id,
                    }) => Some(proof_id.clone()),
                    _ => None,
                });
        ProofRecord {
            id: ProofId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:plan-accepted-proof-id",
                    self.execution_id.as_str(),
                    &self.execution_attempt.to_string(),
                    plan.plan_revision_id.as_str(),
                ])
            )),
            kind: ProofKind::PlanAccepted,
            repository_revision: self.repository_revision.clone(),
            node_ids: vec![planning.node_id.clone()],
            related_proof_ids: source_proof_id.into_iter().collect(),
            related_evidence_ids,
            detail_hash: plan_accepted_proof_hash(plan),
        }
    }

    fn planning_no_op_proof(&self, no_op: &AcceptedNoOp) -> ProofRecord {
        let planning = self.planning.as_ref().expect("typed planning exists");
        let related_evidence_ids = std::iter::once(planning.discovery_impact_map_id.clone())
            .chain(
                no_op
                    .criterion_satisfaction
                    .iter()
                    .flat_map(|observation| observation.supporting_evidence_ids.iter().cloned()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let source_proof_id =
            self.event_log
                .iter()
                .find_map(|stored| match &stored.envelope.payload {
                    DomainEvent::Lifecycle(LifecycleEvent::PositionAdvanced {
                        from: ProtocolStage::Discovery,
                        to: ProtocolStage::Planning,
                        proof_id,
                    }) => Some(proof_id.clone()),
                    _ => None,
                });
        let related_proof_ids = source_proof_id
            .into_iter()
            .chain(
                no_op
                    .criterion_satisfaction
                    .iter()
                    .map(|observation| observation.authority.proof_id().clone()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        ProofRecord {
            id: ProofId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:no-op-satisfied-proof-id",
                    self.execution_id.as_str(),
                    &self.execution_attempt.to_string(),
                    no_op.plan_revision_id.as_str(),
                ])
            )),
            kind: ProofKind::NoOpSatisfied,
            repository_revision: self.repository_revision.clone(),
            node_ids: vec![planning.node_id.clone()],
            related_proof_ids,
            related_evidence_ids,
            detail_hash: no_op_satisfied_proof_hash(no_op),
        }
    }

    fn mutation_verification_proof(
        &self,
        evidence: &MutationVerificationEvidence,
    ) -> Result<ProofRecord, ProtocolViolation> {
        self.validate_mutation_verification_chain(evidence)?;
        if evidence.repository_revision_after != self.repository_revision {
            return Err(ProtocolViolation::MutationContract {
                code: "mutation_proof_repository_revision_mismatch",
            });
        }
        let related_proof_ids = if self
            .nodes
            .get(&evidence.node_id)
            .is_some_and(|node| node.kind == NodeKind::ValidationRepair)
        {
            let validation =
                self.validation
                    .as_ref()
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "repair_mutation_proof_validation_state_missing",
                    })?;
            let failure =
                validation
                    .current_failure()
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "repair_mutation_proof_failure_missing",
                    })?;
            let selection = validation
                .selections
                .get(&failure.failure_revision_id)
                .filter(|selection| selection.repair_node.id == evidence.node_id)
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "repair_mutation_proof_selection_missing",
                })?;
            let eligibility = self.repair_eligibility_proof(selection)?;
            if self.proofs.get(&eligibility.id) != Some(&eligibility) {
                return Err(ProtocolViolation::ValidationContract {
                    code: "repair_mutation_proof_eligibility_missing",
                });
            }
            vec![eligibility.id]
        } else {
            self.latest_transition_proof
                .as_ref()
                .filter(|proof_id| self.proof_kind(proof_id) == Some(ProofKind::PlanAccepted))
                .cloned()
                .into_iter()
                .collect::<Vec<_>>()
        };
        let detail_hash = stable_sha256(&[
            "execution-protocol-v1:mutation-verification-proof",
            evidence.evidence_id.as_str(),
            &evidence.detail_hash,
            evidence.repository_revision_before.as_str(),
            evidence.repository_revision_after.as_str(),
        ]);
        Ok(ProofRecord {
            id: ProofId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:mutation-verification-proof-id",
                    self.execution_id.as_str(),
                    &self.execution_attempt.to_string(),
                    evidence.node_id.as_str(),
                    evidence.evidence_id.as_str(),
                    &detail_hash,
                ])
            )),
            kind: ProofKind::MutationVerified,
            repository_revision: evidence.repository_revision_after.clone(),
            node_ids: vec![evidence.node_id.clone()],
            related_proof_ids,
            related_evidence_ids: vec![evidence.evidence_id.clone()],
            detail_hash,
        })
    }

    fn repair_verification_proof(
        &self,
        evidence: &MutationVerificationEvidence,
    ) -> Result<ProofRecord, ProtocolViolation> {
        self.validate_mutation_verification_chain(evidence)?;
        if evidence.repository_revision_after != self.repository_revision {
            return Err(ProtocolViolation::ValidationContract {
                code: "repair_verification_repository_revision_mismatch",
            });
        }
        let validation = self
            .validation
            .as_ref()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "repair_verification_validation_state_missing",
            })?;
        let failure =
            validation
                .current_failure()
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "repair_verification_failure_missing",
                })?;
        let selection = validation
            .selections
            .get(&failure.failure_revision_id)
            .filter(|selection| selection.repair_node.id == evidence.node_id)
            .ok_or(ProtocolViolation::ValidationContract {
                code: "repair_verification_selection_missing",
            })?;
        let eligibility = self.repair_eligibility_proof(selection)?;
        let mutation = self.mutation_verification_proof(evidence)?;
        if self.proofs.get(&eligibility.id) != Some(&eligibility)
            || self.proofs.get(&mutation.id) != Some(&mutation)
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "repair_verification_parent_proof_missing",
            });
        }
        self.canonical_repair_verification_proof(selection, evidence, &eligibility, &mutation)
    }

    fn canonical_repair_verification_proof(
        &self,
        selection: &RepairTargetSelection,
        evidence: &MutationVerificationEvidence,
        eligibility: &ProofRecord,
        mutation: &ProofRecord,
    ) -> Result<ProofRecord, ProtocolViolation> {
        if evidence.node_id != selection.repair_node.id
            || evidence.target_id != selection.intent.target_id
            || evidence.repository_revision_before != selection.intent.repository_revision
            || eligibility.kind != ProofKind::RepairEligibility
            || eligibility.repository_revision != selection.intent.repository_revision
            || !eligibility.node_ids.is_empty()
            || mutation.kind != ProofKind::MutationVerified
            || mutation.repository_revision != evidence.repository_revision_after
            || mutation.node_ids != vec![evidence.node_id.clone()]
            || mutation.related_evidence_ids != vec![evidence.evidence_id.clone()]
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "repair_verification_parent_proof_mismatch",
            });
        }
        let mut related_proof_ids = vec![eligibility.id.clone(), mutation.id.clone()];
        related_proof_ids.sort();
        let related_evidence_ids = vec![evidence.evidence_id.clone()];
        let canonical = serde_json::to_string(&(
            &selection.selection_hash,
            &selection.intent,
            &evidence.context_manifest_id,
            &evidence.repository_revision_before,
            &evidence.repository_revision_after,
            &evidence.detail_hash,
            &related_proof_ids,
            &related_evidence_ids,
        ))
        .map_err(|error| ProtocolViolation::EventSerialization {
            detail: error.to_string(),
        })?;
        let detail_hash = stable_sha256(&[
            "execution-protocol-v1:repair-verification-proof",
            &canonical,
        ]);
        Ok(ProofRecord {
            id: ProofId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:repair-verification-proof-id",
                    self.execution_id.as_str(),
                    &self.execution_attempt.to_string(),
                    selection.intent.repair_intent_id.as_str(),
                    evidence.evidence_id.as_str(),
                    &detail_hash,
                ])
            )),
            kind: ProofKind::RepairVerified,
            repository_revision: evidence.repository_revision_after.clone(),
            node_ids: vec![selection.repair_node.id.clone()],
            related_proof_ids,
            related_evidence_ids,
            detail_hash,
        })
    }

    fn validation_pass_proof(&self, node_id: &NodeId) -> Result<ProofRecord, ProtocolViolation> {
        let validation = self
            .validation
            .as_ref()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_state_missing",
            })?;
        let gate_ids =
            validation
                .node_gates
                .get(node_id)
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "validation_node_has_no_gate",
                })?;
        let mut evidence_ids = Vec::with_capacity(gate_ids.len());
        for gate_id in gate_ids {
            let evidence = validation
                .current_evidence_by_gate
                .get(gate_id)
                .and_then(|evidence_id| validation.evidence.get(evidence_id))
                .filter(|evidence| {
                    evidence.repository_revision == self.repository_revision
                        && matches!(evidence.outcome, ValidationEvidenceOutcome::Passed)
                })
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "validation_node_gate_not_passed",
                })?;
            evidence_ids.push(EvidenceId::new(evidence.evidence_id.as_str()));
        }
        evidence_ids.sort();
        let canonical =
            serde_json::to_string(&(node_id, &self.repository_revision, gate_ids, &evidence_ids))
                .map_err(|error| ProtocolViolation::EventSerialization {
                detail: error.to_string(),
            })?;
        let detail_hash =
            stable_sha256(&["execution-protocol-v1:validation-passed-proof", &canonical]);
        Ok(ProofRecord {
            id: ProofId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:validation-passed-proof-id",
                    self.execution_id.as_str(),
                    &self.execution_attempt.to_string(),
                    node_id.as_str(),
                    &detail_hash,
                ])
            )),
            kind: ProofKind::ValidationPassed,
            repository_revision: self.repository_revision.clone(),
            node_ids: vec![node_id.clone()],
            related_proof_ids: self.latest_transition_proof.clone().into_iter().collect(),
            related_evidence_ids: evidence_ids,
            detail_hash,
        })
    }

    fn validation_failure_proof(
        &self,
        failure: &ValidationFailureRevisionV1,
    ) -> Result<ProofRecord, ProtocolViolation> {
        let validation = self
            .validation
            .as_ref()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_state_missing",
            })?;
        if validation.current_failure() != Some(failure) {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_failure_not_current",
            });
        }
        let evidence = validation
            .evidence
            .get(&failure.validation_evidence_id)
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_failure_evidence_missing",
            })?;
        let mut related_evidence_ids = evidence
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.diagnostic_id.clone())
            .collect::<Vec<_>>();
        related_evidence_ids.push(EvidenceId::new(evidence.evidence_id.as_str()));
        related_evidence_ids.sort();
        let canonical =
            serde_json::to_string(&(failure, &related_evidence_ids)).map_err(|error| {
                ProtocolViolation::EventSerialization {
                    detail: error.to_string(),
                }
            })?;
        let detail_hash =
            stable_sha256(&["execution-protocol-v1:validation-failure-proof", &canonical]);
        Ok(ProofRecord {
            id: ProofId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:validation-failure-proof-id",
                    self.execution_id.as_str(),
                    &self.execution_attempt.to_string(),
                    failure.failure_revision_id.as_str(),
                    &detail_hash,
                ])
            )),
            kind: ProofKind::ValidationFailure,
            repository_revision: self.repository_revision.clone(),
            node_ids: vec![failure.node_id.clone()],
            related_proof_ids: self.latest_transition_proof.clone().into_iter().collect(),
            related_evidence_ids,
            detail_hash,
        })
    }

    fn repair_eligibility_proof(
        &self,
        selection: &RepairTargetSelection,
    ) -> Result<ProofRecord, ProtocolViolation> {
        let validation = self
            .validation
            .as_ref()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_state_missing",
            })?;
        if validation
            .selections
            .get(&selection.intent.failure_revision_id)
            != Some(selection)
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "repair_selection_not_current",
            });
        }
        let failure_proof_id = self
            .latest_transition_proof
            .as_ref()
            .filter(|proof_id| self.proof_kind(proof_id) == Some(ProofKind::ValidationFailure))
            .ok_or(ProtocolViolation::ValidationContract {
                code: "repair_failure_proof_missing",
            })?
            .clone();
        let related_evidence_ids = selection
            .intent
            .supporting_evidence_ids
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let detail_hash = stable_sha256(&[
            "execution-protocol-v1:repair-eligibility-proof",
            &selection.selection_hash,
            selection.intent.repair_intent_id.as_str(),
        ]);
        Ok(ProofRecord {
            id: ProofId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:repair-eligibility-proof-id",
                    self.execution_id.as_str(),
                    &self.execution_attempt.to_string(),
                    selection.intent.failure_revision_id.as_str(),
                    &selection.selection_hash,
                ])
            )),
            kind: ProofKind::RepairEligibility,
            repository_revision: selection.intent.repository_revision.clone(),
            node_ids: Vec::new(),
            related_proof_ids: vec![failure_proof_id],
            related_evidence_ids,
            detail_hash,
        })
    }

    fn required_validation_proof(&self) -> Result<ProofRecord, ProtocolViolation> {
        let validation = self
            .validation
            .as_ref()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_state_missing",
            })?;
        if validation.next_gate_id().is_some()
            || validation.active_failure.is_some()
            || validation.pending_rerun.is_some()
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "required_validation_not_complete",
            });
        }
        let node_ids = self
            .required_nodes(NodeKind::Validation)
            .into_iter()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        let related_proof_ids = node_ids
            .iter()
            .map(|node_id| {
                let node = self.nodes.get(node_id).expect("required node exists");
                let NodeState::Succeeded { proof_id } = &node.state else {
                    return Err(ProtocolViolation::ValidationContract {
                        code: "required_validation_node_not_succeeded",
                    });
                };
                Ok(proof_id.clone())
            })
            .collect::<Result<Vec<_>, ProtocolViolation>>()?;
        let related_evidence_ids = validation
            .current_evidence_by_gate
            .values()
            .map(|evidence_id| EvidenceId::new(evidence_id.as_str()))
            .collect::<Vec<_>>();
        let canonical = serde_json::to_string(&(
            &self.repository_revision,
            &node_ids,
            &related_proof_ids,
            &related_evidence_ids,
        ))
        .map_err(|error| ProtocolViolation::EventSerialization {
            detail: error.to_string(),
        })?;
        let detail_hash = stable_sha256(&[
            "execution-protocol-v1:required-validation-proof",
            &canonical,
        ]);
        Ok(ProofRecord {
            id: ProofId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:required-validation-proof-id",
                    self.execution_id.as_str(),
                    &self.execution_attempt.to_string(),
                    &detail_hash,
                ])
            )),
            kind: ProofKind::RequiredValidationPassed,
            repository_revision: self.repository_revision.clone(),
            node_ids,
            related_proof_ids,
            related_evidence_ids,
            detail_hash,
        })
    }

    fn review_completion_proof(&self) -> Result<ProofRecord, ProtocolViolation> {
        let review = self
            .review
            .as_ref()
            .ok_or(ProtocolViolation::ReviewContract {
                code: "review_state_missing",
            })?;
        let manifest =
            review
                .diff_manifest
                .as_deref()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "review_diff_manifest_missing",
                })?;
        let diff_review =
            review
                .diff_review
                .as_deref()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "review_diff_review_missing",
                })?;
        if diff_review.disposition != DiffReviewDispositionV1::Accepted {
            return Err(ProtocolViolation::ReviewContract {
                code: "review_completion_not_accepted",
            });
        }
        let required = self.required_validation_proof()?;
        if self.proofs.get(&required.id) != Some(&required) {
            return Err(ProtocolViolation::ReviewContract {
                code: "review_required_validation_proof_missing",
            });
        }
        let mut related_evidence_ids = vec![
            EvidenceId::new(manifest.manifest_id.as_str()),
            EvidenceId::new(diff_review.review_id.as_str()),
        ];
        related_evidence_ids.extend(
            diff_review
                .ordered_page_review_ids
                .iter()
                .map(|id| EvidenceId::new(id.as_str())),
        );
        related_evidence_ids.sort();
        related_evidence_ids.dedup();
        let node_ids = vec![review.review_node_id.clone()];
        let related_proof_ids = vec![required.id];
        self.phase7_proof(
            ProofKind::ReviewCompleted,
            node_ids,
            related_proof_ids,
            related_evidence_ids,
            &(
                &manifest.manifest_id,
                &manifest.diff_hash,
                &diff_review.review_id,
                &diff_review.review_hash,
                &review.ancestry.ancestry_hash,
            ),
        )
    }

    fn completion_evaluation_proof(&self) -> Result<ProofRecord, ProtocolViolation> {
        let review = self
            .review
            .as_ref()
            .ok_or(ProtocolViolation::ReviewContract {
                code: "review_state_missing",
            })?;
        let completion = review
            .completion
            .as_deref()
            .ok_or(ProtocolViolation::ReviewContract {
                code: "completion_evaluation_missing",
            })?;
        if completion.disposition == CompletionDispositionV1::Incomplete {
            return Err(ProtocolViolation::ReviewContract {
                code: "completion_evaluation_incomplete",
            });
        }
        let required = self.required_validation_proof()?;
        let review_proof = self.review_completion_proof()?;
        if self.proofs.get(&required.id) != Some(&required)
            || self.proofs.get(&review_proof.id) != Some(&review_proof)
        {
            return Err(ProtocolViolation::ReviewContract {
                code: "completion_parent_proof_missing",
            });
        }
        let mut related_proof_ids = vec![required.id, review_proof.id];
        related_proof_ids.sort();
        let mut related_evidence_ids = vec![EvidenceId::new(completion.evaluation_id.as_str())];
        for criterion in completion.criteria.values() {
            if let CriterionCompletionStatusV1::Satisfied {
                supporting_evidence_ids,
            } = &criterion.status
            {
                related_evidence_ids.extend(supporting_evidence_ids.iter().cloned());
            }
        }
        related_evidence_ids.sort();
        related_evidence_ids.dedup();
        self.phase7_proof(
            ProofKind::CompletionEvaluated,
            vec![review.completion_node_id.clone()],
            related_proof_ids,
            related_evidence_ids,
            &(
                &completion.evaluation_id,
                &completion.evaluation_hash,
                completion.disposition,
                &review.ancestry.ancestry_hash,
            ),
        )
    }

    fn publication_eligibility_proof(&self) -> Result<ProofRecord, ProtocolViolation> {
        let review = self
            .review
            .as_ref()
            .ok_or(ProtocolViolation::ReviewContract {
                code: "review_state_missing",
            })?;
        let policy =
            self.finalization_policy
                .as_ref()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "finalization_policy_missing",
                })?;
        let manifest =
            review
                .diff_manifest
                .as_deref()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "review_diff_manifest_missing",
                })?;
        let diff_review =
            review
                .diff_review
                .as_deref()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "review_diff_review_missing",
                })?;
        let completion = review
            .completion
            .as_deref()
            .ok_or(ProtocolViolation::ReviewContract {
                code: "completion_evaluation_missing",
            })?;
        let authority = review
            .authority
            .as_ref()
            .ok_or(ProtocolViolation::ReviewContract {
                code: "publication_authority_missing",
            })?;
        let eligibility =
            review
                .eligibility
                .as_deref()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "publication_eligibility_missing",
                })?;
        if !eligibility.is_granted() {
            return Err(ProtocolViolation::ReviewContract {
                code: "publication_eligibility_not_granted",
            });
        }
        let required = self.required_validation_proof()?;
        let review_proof = self.review_completion_proof()?;
        let completion_proof = self.completion_evaluation_proof()?;
        for expected in [&required, &review_proof, &completion_proof] {
            if self.proofs.get(&expected.id) != Some(expected) {
                return Err(ProtocolViolation::ReviewContract {
                    code: "publication_eligibility_parent_proof_missing",
                });
            }
        }
        let mut related_proof_ids = vec![
            review.ancestry.implementation_barrier_proof_id.clone(),
            required.id,
            review_proof.id,
            completion_proof.id,
        ];
        related_proof_ids.sort();
        related_proof_ids.dedup();
        if related_proof_ids.len() != 4 {
            return Err(ProtocolViolation::ReviewContract {
                code: "publication_eligibility_parent_set_invalid",
            });
        }
        let mut related_evidence_ids = vec![
            policy.policy_evidence_id.clone(),
            EvidenceId::new(manifest.manifest_id.as_str()),
            EvidenceId::new(diff_review.review_id.as_str()),
            EvidenceId::new(completion.evaluation_id.as_str()),
            EvidenceId::new(authority.authority_id.as_str()),
            EvidenceId::new(eligibility.eligibility_id.as_str()),
        ];
        related_evidence_ids.sort();
        related_evidence_ids.dedup();
        self.phase7_proof(
            ProofKind::PublicationEligibility,
            Vec::new(),
            related_proof_ids,
            related_evidence_ids,
            &(
                &eligibility.eligibility_id,
                &eligibility.decision_hash,
                &review.ancestry.ancestry_hash,
                &manifest.diff_hash,
            ),
        )
    }

    fn publication_completion_proof(&self) -> Result<ProofRecord, ProtocolViolation> {
        let publication =
            self.publication
                .as_ref()
                .ok_or(ProtocolViolation::PublicationContract {
                    code: "publication_state_missing",
                })?;
        let completion =
            publication
                .completion
                .as_ref()
                .ok_or(ProtocolViolation::PublicationContract {
                    code: "publication_completion_missing",
                })?;
        let eligibility_proof = self.publication_eligibility_proof()?;
        if self.proofs.get(&eligibility_proof.id) != Some(&eligibility_proof) {
            return Err(ProtocolViolation::PublicationContract {
                code: "publication_completion_eligibility_proof_missing",
            });
        }
        self.phase7_proof(
            ProofKind::PublicationCompleted,
            vec![publication.publication_node_id.clone()],
            vec![eligibility_proof.id],
            vec![
                EvidenceId::new(completion.commit_observation_id.as_str()),
                EvidenceId::new(completion.push_observation_id.as_str()),
                EvidenceId::new(completion.pull_request_observation_id.as_str()),
                EvidenceId::new(completion.completion_id.as_str()),
            ],
            &(
                &completion.completion_id,
                &completion.completion_hash,
                &completion.commit_oid,
                &completion.head_branch,
                completion.pull_request_number,
                completion.draft,
            ),
        )
    }

    fn phase7_proof(
        &self,
        kind: ProofKind,
        mut node_ids: Vec<NodeId>,
        mut related_proof_ids: Vec<ProofId>,
        mut related_evidence_ids: Vec<EvidenceId>,
        detail: &impl serde::Serialize,
    ) -> Result<ProofRecord, ProtocolViolation> {
        node_ids.sort();
        node_ids.dedup();
        related_proof_ids.sort();
        related_proof_ids.dedup();
        related_evidence_ids.sort();
        related_evidence_ids.dedup();
        let detail_json = serde_json::to_string(&(
            kind,
            &self.repository_revision,
            &node_ids,
            &related_proof_ids,
            &related_evidence_ids,
            detail,
        ))
        .map_err(|error| ProtocolViolation::EventSerialization {
            detail: error.to_string(),
        })?;
        let detail_hash = stable_sha256(&["execution-protocol-v1:phase7-proof", &detail_json]);
        Ok(ProofRecord {
            id: ProofId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:phase7-proof-id",
                    self.execution_id.as_str(),
                    &self.execution_attempt.to_string(),
                    &format!("{kind:?}"),
                    &detail_hash,
                ])
            )),
            kind,
            repository_revision: self.repository_revision.clone(),
            node_ids,
            related_proof_ids,
            related_evidence_ids,
            detail_hash,
        })
    }

    /// Reconstructs the exact validation-to-engineering proof chain for the
    /// current repository revision. A repair advances the repository without
    /// minting a replacement implementation barrier, so publication authority
    /// must follow the recorded failure/eligibility/mutation/repair/rerun DAG
    /// back to the one original barrier.
    fn engineering_ancestry(&self) -> Result<EngineeringAncestryV1, ProtocolViolation> {
        let required = self.required_validation_proof()?;
        if self.proofs.get(&required.id) != Some(&required) {
            return Err(ProtocolViolation::ReviewContract {
                code: "review_required_validation_proof_missing",
            });
        }
        let mut anchors = BTreeSet::new();
        let mut validation_pass_ids = required.related_proof_ids.clone();
        validation_pass_ids.sort();
        for proof_id in &validation_pass_ids {
            let proof =
                self.proofs
                    .get(proof_id)
                    .ok_or_else(|| ProtocolViolation::UnknownProof {
                        proof_id: proof_id.clone(),
                    })?;
            if proof.kind != ProofKind::ValidationPassed
                || proof.repository_revision != self.repository_revision
                || proof.related_proof_ids.len() != 1
            {
                return Err(ProtocolViolation::ReviewContract {
                    code: "review_validation_pass_ancestry_invalid",
                });
            }
            anchors.insert(proof.related_proof_ids[0].clone());
        }
        let anchors = anchors.into_iter().collect::<Vec<_>>();
        let [current_anchor] = anchors.as_slice() else {
            return Err(ProtocolViolation::ReviewContract {
                code: "review_validation_anchor_ambiguous",
            });
        };
        let mut visited = BTreeSet::new();
        let mut ordered = self.trace_engineering_anchor(current_anchor, &mut visited)?;
        let implementation_barrier_proof_id =
            ordered
                .first()
                .cloned()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "review_implementation_barrier_missing",
                })?;
        ordered.extend(validation_pass_ids);
        ordered.push(required.id.clone());
        let repository_fingerprints = self
            .event_log
            .iter()
            .filter_map(|stored| match &stored.envelope.payload {
                DomainEvent::Mutation(MutationEvent::MutationVerified { evidence })
                    if evidence.repository_revision_after == self.repository_revision =>
                {
                    Some(evidence.repository_fingerprint_after.clone())
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let repository_fingerprints = repository_fingerprints.into_iter().collect::<Vec<_>>();
        let [repository_fingerprint] = repository_fingerprints.as_slice() else {
            return Err(ProtocolViolation::ReviewContract {
                code: "review_repository_fingerprint_ambiguous",
            });
        };
        EngineeringAncestryV1::new(
            self.repository_revision.clone(),
            repository_fingerprint.clone(),
            implementation_barrier_proof_id,
            required.id,
            ordered,
        )
        .map_err(Into::into)
    }

    fn trace_engineering_anchor(
        &self,
        anchor_id: &ProofId,
        visited: &mut BTreeSet<ProofId>,
    ) -> Result<Vec<ProofId>, ProtocolViolation> {
        if !visited.insert(anchor_id.clone()) {
            return Err(ProtocolViolation::ReviewContract {
                code: "review_engineering_ancestry_cycle",
            });
        }
        let anchor = self
            .proofs
            .get(anchor_id)
            .ok_or_else(|| ProtocolViolation::UnknownProof {
                proof_id: anchor_id.clone(),
            })?;
        match anchor.kind {
            ProofKind::ImplementationBarrier => {
                let lifecycle_barriers = self
                    .event_log
                    .iter()
                    .filter_map(|stored| match &stored.envelope.payload {
                        DomainEvent::Lifecycle(LifecycleEvent::PositionAdvanced {
                            from: ProtocolStage::Implementation,
                            to: ProtocolStage::Validation,
                            proof_id,
                        }) => Some(proof_id),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if lifecycle_barriers != [anchor_id] {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_implementation_barrier_not_authoritative",
                    });
                }
                Ok(vec![anchor_id.clone()])
            }
            ProofKind::ValidationRerunScheduled => {
                let [repair_id] = anchor.related_proof_ids.as_slice() else {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_rerun_parent_invalid",
                    });
                };
                let repair =
                    self.proofs
                        .get(repair_id)
                        .ok_or_else(|| ProtocolViolation::UnknownProof {
                            proof_id: repair_id.clone(),
                        })?;
                if repair.kind != ProofKind::RepairVerified {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_rerun_repair_parent_invalid",
                    });
                }
                let eligibility_id = repair
                    .related_proof_ids
                    .iter()
                    .find(|proof_id| {
                        self.proof_kind(proof_id) == Some(ProofKind::RepairEligibility)
                    })
                    .cloned()
                    .ok_or(ProtocolViolation::ReviewContract {
                        code: "review_repair_eligibility_parent_missing",
                    })?;
                let mutation_id = repair
                    .related_proof_ids
                    .iter()
                    .find(|proof_id| self.proof_kind(proof_id) == Some(ProofKind::MutationVerified))
                    .cloned()
                    .ok_or(ProtocolViolation::ReviewContract {
                        code: "review_repair_mutation_parent_missing",
                    })?;
                if repair.related_proof_ids.len() != 2 {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_repair_parent_set_invalid",
                    });
                }
                let eligibility = self.proofs.get(&eligibility_id).ok_or_else(|| {
                    ProtocolViolation::UnknownProof {
                        proof_id: eligibility_id.clone(),
                    }
                })?;
                let [failure_id] = eligibility.related_proof_ids.as_slice() else {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_repair_failure_parent_invalid",
                    });
                };
                let failure =
                    self.proofs
                        .get(failure_id)
                        .ok_or_else(|| ProtocolViolation::UnknownProof {
                            proof_id: failure_id.clone(),
                        })?;
                if failure.kind != ProofKind::ValidationFailure {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_repair_failure_parent_invalid",
                    });
                }
                let [previous_anchor] = failure.related_proof_ids.as_slice() else {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "review_previous_validation_anchor_invalid",
                    });
                };
                let mut ordered = self.trace_engineering_anchor(previous_anchor, visited)?;
                for proof_id in [failure_id, &eligibility_id, &mutation_id, repair_id] {
                    if !visited.insert(proof_id.clone()) {
                        return Err(ProtocolViolation::ReviewContract {
                            code: "review_engineering_ancestry_duplicate",
                        });
                    }
                    ordered.push(proof_id.clone());
                }
                ordered.push(anchor_id.clone());
                Ok(ordered)
            }
            _ => Err(ProtocolViolation::ReviewContract {
                code: "review_validation_anchor_kind_invalid",
            }),
        }
    }

    fn validation_rerun_proof(
        &self,
        rerun: &ValidationRerunSchedule,
    ) -> Result<ProofRecord, ProtocolViolation> {
        let validation = self
            .validation
            .as_ref()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_state_missing",
            })?;
        if validation.pending_rerun.as_ref() != Some(rerun)
            || rerun.repository_revision != self.repository_revision
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_rerun_not_current",
            });
        }
        let selection = validation
            .selections
            .get(&rerun.failure_revision_id)
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_rerun_selection_missing",
            })?;
        let gate = validation.gates.get(&rerun.originating_gate_id).ok_or(
            ProtocolViolation::ValidationContract {
                code: "validation_rerun_gate_missing",
            },
        )?;
        let repair_node = self.nodes.get(&selection.repair_node.id).ok_or_else(|| {
            ProtocolViolation::UnknownNode {
                node_id: selection.repair_node.id.clone(),
            }
        })?;
        let NodeState::Succeeded {
            proof_id: repair_proof_id,
        } = &repair_node.state
        else {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_rerun_repair_not_succeeded",
            });
        };
        let repair_proof =
            self.proofs
                .get(repair_proof_id)
                .ok_or_else(|| ProtocolViolation::UnknownProof {
                    proof_id: repair_proof_id.clone(),
                })?;
        if repair_proof.kind != ProofKind::RepairVerified
            || !repair_proof.node_ids.contains(&selection.repair_node.id)
            || !repair_proof
                .related_evidence_ids
                .contains(&rerun.verified_repair_evidence_id)
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_rerun_without_verified_repair",
            });
        }
        let mut related_evidence_ids = rerun
            .invalidated_evidence_ids
            .iter()
            .map(|evidence_id| EvidenceId::new(evidence_id.as_str()))
            .collect::<Vec<_>>();
        related_evidence_ids.push(rerun.rerun_id.clone());
        related_evidence_ids.push(rerun.verified_repair_evidence_id.clone());
        related_evidence_ids.sort();
        related_evidence_ids.dedup();
        let detail_hash = stable_sha256(&[
            "execution-protocol-v1:validation-rerun-proof",
            &serde_json::to_string(&(rerun, repair_proof_id, &related_evidence_ids)).map_err(
                |error| ProtocolViolation::EventSerialization {
                    detail: error.to_string(),
                },
            )?,
        ]);
        Ok(ProofRecord {
            id: ProofId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:validation-rerun-proof-id",
                    self.execution_id.as_str(),
                    &self.execution_attempt.to_string(),
                    rerun.rerun_id.as_str(),
                    &detail_hash,
                ])
            )),
            kind: ProofKind::ValidationRerunScheduled,
            repository_revision: self.repository_revision.clone(),
            node_ids: vec![gate.node_id.clone()],
            related_proof_ids: vec![repair_proof_id.clone()],
            related_evidence_ids,
            detail_hash,
        })
    }

    fn validate_mutation_verification_chain(
        &self,
        evidence: &MutationVerificationEvidence,
    ) -> Result<(), ProtocolViolation> {
        evidence.validate()?;
        let (node, target, context) =
            self.mutation_binding(&evidence.node_id, &evidence.context_manifest_id)?;
        let feasibility = self
            .event_log
            .iter()
            .find_map(|stored| match &stored.envelope.payload {
                DomainEvent::Mutation(MutationEvent::FeasibilityEvaluated { feasibility })
                    if feasibility.node_id == node.id
                        && feasibility.context_manifest_id == context.context_manifest_id =>
                {
                    Some(feasibility)
                }
                _ => None,
            })
            .ok_or(ProtocolViolation::MutationContract {
                code: "mutation_verification_feasibility_missing",
            })?;
        let candidate = self
            .mutation_candidate(&evidence.candidate_id)
            .map_err(|_| ProtocolViolation::MutationContract {
                code: "mutation_verification_candidate_missing",
            })?;
        let prepared = self.mutation_action(&candidate.action_id).map_err(|_| {
            ProtocolViolation::MutationContract {
                code: "mutation_verification_action_missing",
            }
        })?;
        if prepared.policy.attempt_id != evidence.attempt_id {
            return Err(ProtocolViolation::MutationContract {
                code: "mutation_verification_action_attempt_mismatch",
            });
        }
        prepared.validate_against(node, &target, context, feasibility)?;
        candidate.validate_against(prepared, &target)?;
        let (apply, application) = self
            .event_log
            .iter()
            .find_map(|stored| match &stored.envelope.payload {
                DomainEvent::Mutation(MutationEvent::ApplicationObserved {
                    request,
                    observation,
                }) if request.application_id == evidence.application_id => {
                    Some((request, observation))
                }
                _ => None,
            })
            .ok_or(ProtocolViolation::MutationContract {
                code: "mutation_verification_application_missing",
            })?;
        let expected_apply = MutationApplyRequest::new(prepared, candidate, &target, context)?;
        if apply != &expected_apply {
            return Err(ProtocolViolation::MutationContract {
                code: "mutation_verification_apply_chain_mismatch",
            });
        }
        application.validate_against(apply)?;
        let verify = MutationVerifyRequest::new(apply, application)?;
        if evidence.verification_request_id != verify.request_id
            || evidence.application_id != verify.application_id
            || evidence.node_id != verify.node_id
            || evidence.target_id != verify.target_id
            || evidence.context_manifest_id != verify.context_manifest_id
            || evidence.candidate_id != verify.candidate_id
            || evidence.repository_revision_before != verify.repository_revision
            || evidence.repository_fingerprint_before != verify.repository_fingerprint
            || evidence.changed_paths != verify.owned_paths
            || evidence
                .path_transitions
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
                != verify.owned_paths
            || !operation_transition_is_verified(&candidate.operation, &evidence.path_transitions)
        {
            return Err(ProtocolViolation::MutationContract {
                code: "mutation_verification_chain_mismatch",
            });
        }
        Ok(())
    }

    fn implementation_barrier_proof(&self) -> Result<ProofRecord, ProtocolViolation> {
        let node_ids = self
            .node_order
            .iter()
            .filter(|node_id| {
                self.nodes
                    .get(*node_id)
                    .is_some_and(|node| node.kind == NodeKind::Implementation && node.required)
            })
            .cloned()
            .collect::<Vec<_>>();
        if node_ids.is_empty() {
            return Err(ProtocolViolation::ImplementationContract {
                code: "implementation_barrier_has_no_required_nodes",
            });
        }
        let related_proof_ids = node_ids
            .iter()
            .map(|node_id| {
                let node = self.nodes.get(node_id).expect("ordered node exists");
                let NodeState::Succeeded { proof_id } = &node.state else {
                    return Err(ProtocolViolation::InvalidNodeState {
                        node_id: node_id.clone(),
                        code: "implementation_barrier_node_not_succeeded",
                    });
                };
                if !matches!(
                    self.proof_kind(proof_id),
                    Some(ProofKind::MutationVerified | ProofKind::AlreadySatisfied)
                ) {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof_id.clone(),
                        code: "implementation_barrier_node_proof_invalid",
                    });
                }
                Ok(proof_id.clone())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let canonical =
            serde_json::to_string(&(&self.repository_revision, &node_ids, &related_proof_ids))
                .map_err(|error| ProtocolViolation::EventSerialization {
                    detail: error.to_string(),
                })?;
        let detail_hash = stable_sha256(&[
            "execution-protocol-v1:implementation-barrier-proof",
            &canonical,
        ]);
        Ok(ProofRecord {
            id: ProofId::new(format!(
                "epv1:{}",
                stable_sha256(&[
                    "execution-protocol-v1:implementation-barrier-proof-id",
                    self.execution_id.as_str(),
                    &self.execution_attempt.to_string(),
                    &detail_hash,
                ])
            )),
            kind: ProofKind::ImplementationBarrier,
            repository_revision: self.repository_revision.clone(),
            node_ids,
            related_proof_ids,
            related_evidence_ids: Vec::new(),
            detail_hash,
        })
    }

    fn validate_discovery_action_constraints(
        &self,
        discovery: &DiscoveryState,
        envelope: &ActionEnvelope,
    ) -> Result<(), ProtocolViolation> {
        match &envelope.constraints {
            DiscoveryActionConstraints::Search { request } => {
                let known_evidence = discovery
                    .completed_searches
                    .values()
                    .map(|evidence| evidence.evidence_id.clone())
                    .chain(
                        discovery
                            .candidates
                            .values()
                            .map(|evidence| evidence.evidence_id.clone()),
                    )
                    .chain(discovery.file_evidence.keys().cloned())
                    .chain(discovery.relationships.keys().cloned())
                    .collect::<BTreeSet<_>>();
                if request.repository_profile_id != discovery.repository_profile_id
                    || request.repository_revision != discovery.repository_revision
                    || !request.context_evidence_ids.is_subset(&known_evidence)
                {
                    return Err(ProtocolViolation::DiscoveryContract {
                        code: "discovery_search_profile_binding_mismatch",
                    });
                }
                if let SearchAdmission::DuplicateCompleted { search_id } =
                    discovery.classify_search(request)
                {
                    return Err(ProtocolViolation::DuplicateSearch { search_id });
                }
            }
            DiscoveryActionConstraints::ExactPaths { paths } => {
                let expected = discovery
                    .ranked_candidate_paths()
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                if paths != &expected {
                    return Err(ProtocolViolation::DiscoveryContract {
                        code: "discovery_read_paths_not_ranked_candidates",
                    });
                }
            }
            DiscoveryActionConstraints::NamedRelationship {
                question,
                paths,
                targeted_search,
            } => {
                let selected_question = discovery.unresolved_questions.values().next();
                let expected_paths = BTreeSet::from([question.subject_path.clone()]);
                if selected_question != Some(question)
                    || paths != &expected_paths
                    || !discovery.candidates.contains_key(&question.subject_path)
                    || targeted_search.is_some()
                {
                    return Err(ProtocolViolation::DiscoveryContract {
                        code: "discovery_relationship_action_not_authoritative",
                    });
                }
            }
            DiscoveryActionConstraints::ImpactMap {
                criterion_ids,
                evidence_ids,
            } => {
                let expected_evidence_ids = discovery.impact_map_evidence_ids();
                if criterion_ids != &discovery.goal.criterion_ids
                    || evidence_ids.is_empty()
                    || evidence_ids != &expected_evidence_ids
                {
                    return Err(ProtocolViolation::DiscoveryContract {
                        code: "discovery_impact_map_inputs_not_authoritative",
                    });
                }
            }
        }
        Ok(())
    }

    fn deterministic_discovery_impact_map(&self) -> Result<ImpactMapEvidence, ProtocolViolation> {
        let discovery = self
            .discovery
            .as_ref()
            .ok_or(ProtocolViolation::DiscoveryContract {
                code: "discovery_state_missing",
            })?;
        let Some(DiscoveryConvergence::EvidenceSufficientForDeterministicImpactMap {
            criterion_paths,
            evidence_ids,
        }) = discovery.convergence.as_ref()
        else {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "deterministic_impact_map_not_available",
            });
        };
        let mut areas = Vec::new();
        for (criterion_id, paths) in criterion_paths {
            let area_evidence = discovery
                .file_evidence
                .values()
                .filter(|evidence| paths.contains(&evidence.path))
                .map(|evidence| evidence.evidence_id.clone())
                .chain(discovery.relationships.values().filter_map(|evidence| {
                    (paths.contains(&evidence.from) || paths.contains(&evidence.to))
                        .then_some(evidence.evidence_id.clone())
                }))
                .filter(|evidence_id| evidence_ids.contains(evidence_id))
                .collect::<BTreeSet<_>>();
            if paths.is_empty() || area_evidence.is_empty() {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "deterministic_impact_area_has_no_grounding",
                });
            }
            areas.push(ImpactArea {
                criterion_id: criterion_id.clone(),
                paths: paths.clone(),
                evidence_ids: area_evidence,
                confidence: EvidenceConfidence::Medium,
            });
        }
        Ok(ImpactMapEvidence::new(
            discovery.node_id.clone(),
            discovery.repository_revision.clone(),
            areas,
            BTreeSet::new(),
        )?)
    }

    fn refresh_discovery_position(&mut self) {
        if self.stage() != ProtocolStage::Discovery {
            return;
        }
        let Some(discovery) = &self.discovery else {
            return;
        };
        let step = match discovery.substate() {
            DiscoverySubstate::NeedCandidates => DiscoveryStep::NeedCandidates,
            DiscoverySubstate::NeedGroundedReads => DiscoveryStep::NeedGroundedReads,
            DiscoverySubstate::NeedRelations => DiscoveryStep::NeedRelations,
            DiscoverySubstate::ReadyToSynthesize => DiscoveryStep::ReadyToSynthesize,
        };
        self.position = ProtocolPosition::Discovery(step);
    }

    fn apply_planning_event(&mut self, event: &PlanningEvent) -> Result<(), ProtocolViolation> {
        match event {
            PlanningEvent::ActionPrepared { prepared } => {
                self.prepare_planning_action((**prepared).clone())?
            }
            PlanningEvent::ActionReleased { action_id } => {
                self.release_planning_action(action_id)?
            }
            PlanningEvent::ActionRejected { action_id, reason } => {
                self.reject_planning_action(action_id, *reason)?
            }
            PlanningEvent::CandidateRecorded {
                action_id,
                call_id,
                candidate,
            } => self.record_plan_candidate(action_id, call_id, candidate.clone())?,
            PlanningEvent::ConvergenceEvaluated { convergence } => {
                self.record_planning_convergence(convergence.clone())?
            }
        }
        self.refresh_planning_position();
        Ok(())
    }

    fn prepare_planning_action(
        &mut self,
        prepared: PreparedPlanningAction,
    ) -> Result<(), ProtocolViolation> {
        self.require_active_planning_node()?;
        if self.current_planning_action.is_some() {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_action_already_active",
            });
        }
        prepared
            .envelope
            .validate_against_context(&prepared.context)?;
        let planning = self
            .planning
            .as_ref()
            .ok_or(ProtocolViolation::PlanningContract {
                code: "planning_state_missing",
            })?;
        if planning.accepted_plan.is_some()
            || planning.accepted_no_op.is_some()
            || planning.convergence.is_some()
            || prepared.envelope.node_id != planning.node_id
            || prepared.envelope.repository_revision != self.repository_revision
            || prepared.envelope.budget_owner_node_id != planning.node_id
            || prepared.admission.node_id != planning.node_id
            || prepared.admission.action_id != prepared.envelope.action_id
            || prepared.admission.call_id.as_str() != prepared.envelope.reservation_id.as_str()
            || prepared.admission.payload_hash != prepared.envelope.payload_identity
            || prepared.admission.input_tokens != prepared.context.estimated_input_tokens
            || prepared.admission.output_tokens != prepared.envelope.output_token_allowance
        {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_action_admission_binding_mismatch",
            });
        }
        let expected = authoritative_prepared_planning_action(self)?;
        if prepared != expected {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_action_not_authoritative",
            });
        }
        self.current_planning_action = Some(prepared);
        Ok(())
    }

    fn release_planning_action(&mut self, action_id: &ActionId) -> Result<(), ProtocolViolation> {
        let prepared = self.require_current_planning_action(action_id)?;
        let record = self
            .budgets
            .model_calls
            .get(&prepared.admission.call_id)
            .ok_or_else(|| ProtocolViolation::ModelCallLifecycle {
                call_id: prepared.admission.call_id.clone(),
                code: "planning_action_release_without_call",
            })?;
        if record.state != ModelCallState::ReconciledReleased {
            return Err(ProtocolViolation::ModelCallLifecycle {
                call_id: prepared.admission.call_id.clone(),
                code: "planning_action_release_before_reconciliation",
            });
        }
        self.current_planning_action = None;
        Ok(())
    }

    fn reject_planning_action(
        &mut self,
        action_id: &ActionId,
        _reason: PlanningActionRejectionReason,
    ) -> Result<(), ProtocolViolation> {
        self.require_consumed_planning_action(action_id, None)?;
        self.current_planning_action = None;
        Ok(())
    }

    fn record_plan_candidate(
        &mut self,
        action_id: &ActionId,
        call_id: &ModelCallId,
        candidate: PlanCandidate,
    ) -> Result<(), ProtocolViolation> {
        let prepared = self.require_consumed_planning_action(action_id, Some(call_id))?;
        if &prepared.admission.call_id != call_id {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_candidate_call_binding_mismatch",
            });
        }
        candidate.validate_output_allowance(prepared.envelope.output_token_allowance)?;
        let profile =
            self.repository_profile
                .as_ref()
                .ok_or(ProtocolViolation::PlanningContract {
                    code: "planning_repository_profile_missing",
                })?;
        let discovery = self
            .discovery
            .as_ref()
            .ok_or(ProtocolViolation::PlanningContract {
                code: "planning_discovery_state_missing",
            })?;
        let planning = self
            .planning
            .as_ref()
            .ok_or(ProtocolViolation::PlanningContract {
                code: "planning_state_missing",
            })?;
        candidate.validate_identity()?;
        if candidate.revision_index != planning.next_revision_index()
            || candidate.repository_revision != planning.repository_revision
            || candidate.discovery_impact_map_id != planning.discovery_impact_map_id
        {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_candidate_binding_mismatch",
            });
        }
        let mission_capacity = self.plan_graph_mission_capacity();
        let validation = validate_plan_candidate(
            &candidate,
            profile,
            discovery,
            &self.plan_graph_budget,
            mission_capacity,
        );
        let planning = self.planning.as_mut().expect("typed planning was checked");
        planning.candidate_records.push(PlanCandidateRecord {
            candidate,
            mission_capacity,
            validation: validation.clone(),
        });
        match validation {
            PlanValidationResult::Accepted { plan } => planning.accepted_plan = Some(plan),
            PlanValidationResult::AcceptedNoOp { no_op } => planning.accepted_no_op = Some(no_op),
            PlanValidationResult::Rejected { .. } => {}
        }
        self.current_planning_action = None;
        planning.validate(profile, discovery, &self.plan_graph_budget)?;
        Ok(())
    }

    fn record_planning_convergence(
        &mut self,
        convergence: PlanningConvergence,
    ) -> Result<(), ProtocolViolation> {
        self.require_active_planning_node()?;
        if self.current_planning_action.is_some() {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_convergence_with_active_action",
            });
        }
        let planning = self
            .planning
            .as_ref()
            .ok_or(ProtocolViolation::PlanningContract {
                code: "planning_state_missing",
            })?;
        if planning.accepted_plan.is_some()
            || planning.accepted_no_op.is_some()
            || planning.convergence.is_some()
            || convergence != self.authoritative_planning_convergence(planning)?
        {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_convergence_not_authoritative",
            });
        }
        self.planning
            .as_mut()
            .expect("typed planning was checked")
            .convergence = Some(convergence);
        Ok(())
    }

    fn refresh_planning_position(&mut self) {
        if self.stage() != ProtocolStage::Planning {
            return;
        }
        let Some(planning) = &self.planning else {
            return;
        };
        self.position = if self.current_planning_action.is_some() {
            ProtocolPosition::Planning(PlanningStep::AwaitingPlan)
        } else if planning.candidate_records.last().is_some_and(|record| {
            matches!(record.validation, PlanValidationResult::Rejected { .. })
        }) {
            ProtocolPosition::Planning(PlanningStep::EvidenceGap)
        } else {
            ProtocolPosition::Planning(PlanningStep::ReadyToSynthesize)
        };
    }

    fn authoritative_implementation_step(&self) -> ImplementationStep {
        let Some(implementation) = &self.implementation else {
            return ImplementationStep::SelectTarget;
        };
        let Some(node) = self.active_node() else {
            let required = self.required_nodes(NodeKind::Implementation);
            return if !required.is_empty()
                && required
                    .iter()
                    .all(|node| matches!(node.state, NodeState::Succeeded { .. }))
            {
                ImplementationStep::Barrier
            } else {
                ImplementationStep::SelectTarget
            };
        };
        if node.kind != NodeKind::Implementation {
            return ImplementationStep::SelectTarget;
        }
        let Some(context) = implementation.context_for_node(&node.id) else {
            return ImplementationStep::PrepareContext;
        };
        let Some(target) = self.mutation.contexts.get(&context.context_manifest_id) else {
            return ImplementationStep::GenerateCandidate;
        };
        if target.verified.is_some() {
            return ImplementationStep::VerifyRepository;
        }
        let Some(attempt) = target.attempts.values().next_back() else {
            return ImplementationStep::GenerateCandidate;
        };
        if attempt.application.is_some() {
            return ImplementationStep::VerifyRepository;
        }
        if attempt.candidate.is_some() {
            ImplementationStep::ApplyCandidate
        } else {
            ImplementationStep::GenerateCandidate
        }
    }

    fn refresh_implementation_step(&mut self) {
        if self.stage() == ProtocolStage::Implementation {
            self.position =
                ProtocolPosition::Implementation(self.authoritative_implementation_step());
        }
    }

    fn authoritative_validation_step(&self) -> ValidationStep {
        let Some(validation) = &self.validation else {
            return ValidationStep::ScheduleGate;
        };
        if validation.convergence.is_some() {
            return ValidationStep::Completed;
        }
        if validation.current_failure().is_some() {
            return ValidationStep::DiagnoseFailure;
        }
        let Some(node) = self.active_node() else {
            return if validation.next_gate().is_some() {
                ValidationStep::ScheduleGate
            } else {
                ValidationStep::AllRequiredPassed
            };
        };
        if node.kind != NodeKind::Validation {
            return ValidationStep::ScheduleGate;
        }
        let Some(gate) = validation.next_gate() else {
            return ValidationStep::Completed;
        };
        if gate.node_id != node.id {
            return ValidationStep::Completed;
        }
        let Some(run) = validation.run_for_gate(&gate.gate_id) else {
            return ValidationStep::ScheduleGate;
        };
        if run.evidence.as_ref().is_some_and(|evidence| {
            matches!(
                evidence.outcome,
                ValidationEvidenceOutcome::DomainFailed { .. }
            )
        }) {
            ValidationStep::DiagnoseFailure
        } else if run.completed.is_some() {
            ValidationStep::Completed
        } else {
            ValidationStep::Running
        }
    }

    fn authoritative_repair_step(&self) -> RepairStep {
        let Some(validation) = &self.validation else {
            return RepairStep::RankCandidates;
        };
        if validation.convergence.is_some() || validation.pending_rerun.is_some() {
            return RepairStep::ScheduleRerun;
        }
        let Some(failure) = validation.current_failure() else {
            return RepairStep::ScheduleRerun;
        };
        if !validation
            .rankings
            .contains_key(&failure.failure_revision_id)
        {
            return RepairStep::RankCandidates;
        }
        if !validation
            .eligibility
            .contains_key(&failure.failure_revision_id)
        {
            return RepairStep::CheckEligibility;
        }
        let Some(selection) = validation.selections.get(&failure.failure_revision_id) else {
            return RepairStep::TargetSelected;
        };
        let Some(node) = self.nodes.get(&selection.repair_node.id) else {
            return RepairStep::TargetSelected;
        };
        if matches!(node.state, NodeState::Succeeded { .. }) {
            RepairStep::ScheduleRerun
        } else {
            RepairStep::ExecuteTarget
        }
    }

    fn refresh_validation_step(&mut self) {
        self.position = match self.stage() {
            ProtocolStage::Validation => {
                ProtocolPosition::Validation(self.authoritative_validation_step())
            }
            ProtocolStage::Repair => ProtocolPosition::Repair(self.authoritative_repair_step()),
            _ => return,
        };
    }

    fn refresh_review_step(&mut self) {
        if self.stage() != ProtocolStage::Review {
            return;
        }
        let step = self
            .review
            .as_ref()
            .map_or(ReviewStep::DiffReview, |review| {
                if review.completion.is_none() {
                    if review.diff_review.is_some() {
                        ReviewStep::CompletionEvaluation
                    } else {
                        ReviewStep::DiffReview
                    }
                } else {
                    ReviewStep::PublicationEligibility
                }
            });
        self.position = ProtocolPosition::Review(step);
    }

    fn refresh_publication_step(&mut self) {
        if self.stage() != ProtocolStage::Publication {
            return;
        }
        let step = self
            .publication
            .as_ref()
            .map_or(PublicationStep::Commit, authoritative_publication_step);
        self.position = ProtocolPosition::Publication(step);
    }

    fn single_required_node_id(&self, kind: NodeKind) -> Result<NodeId, ProtocolViolation> {
        let nodes = self.required_nodes(kind);
        let [node] = nodes.as_slice() else {
            return Err(ProtocolViolation::Invariant {
                code: "phase7_required_node_cardinality_invalid",
                detail: format!("expected exactly one required {kind:?} node"),
            });
        };
        Ok(node.id.clone())
    }

    fn apply_evidence_event(&mut self, event: &EvidenceEvent) -> Result<(), ProtocolViolation> {
        match event {
            EvidenceEvent::ProofRecorded { proof } => self.record_proof(proof.clone()),
        }
    }

    fn apply_implementation_event(
        &mut self,
        event: &ImplementationEvent,
    ) -> Result<(), ProtocolViolation> {
        if self.stage() != ProtocolStage::Implementation {
            return Err(ProtocolViolation::ImplementationContract {
                code: "implementation_event_outside_implementation",
            });
        }
        let node_id = match event {
            ImplementationEvent::TargetContextPrepared { prepared } => &prepared.node_id,
            ImplementationEvent::TargetContextSuperseded { supersession } => &supersession.node_id,
        };
        if self.mutation.current_target(node_id).is_some_and(|target| {
            target.verified.is_some()
                || target.convergence.is_some()
                || target.readiness_convergence.is_some()
        }) {
            return Err(ProtocolViolation::MutationContract {
                code: "implementation_context_change_after_mutation_terminal",
            });
        }
        match event {
            ImplementationEvent::TargetContextPrepared { prepared } => {
                let node = self.nodes.get(&prepared.node_id).ok_or_else(|| {
                    ProtocolViolation::UnknownNode {
                        node_id: prepared.node_id.clone(),
                    }
                })?;
                if node.kind != NodeKind::Implementation {
                    return Err(ProtocolViolation::ImplementationContract {
                        code: "target_context_node_kind_mismatch",
                    });
                }
                let NodeState::Active { attempt } = node.state else {
                    return Err(ProtocolViolation::InvalidNodeState {
                        node_id: node.id.clone(),
                        code: "target_context_owner_not_active",
                    });
                };
                if attempt != prepared.node_attempt {
                    return Err(ProtocolViolation::ImplementationContract {
                        code: "target_context_node_attempt_mismatch",
                    });
                }
                let planning =
                    self.planning
                        .as_ref()
                        .ok_or(ProtocolViolation::ImplementationContract {
                            code: "implementation_planning_state_missing",
                        })?;
                let plan = planning.accepted_plan.as_ref().ok_or(
                    ProtocolViolation::ImplementationContract {
                        code: "implementation_accepted_plan_missing",
                    },
                )?;
                let discovery =
                    self.discovery
                        .as_ref()
                        .ok_or(ProtocolViolation::ImplementationContract {
                            code: "implementation_discovery_state_missing",
                        })?;
                let expected = build_target_context_load_request(
                    &self.execution_id,
                    self.execution_attempt,
                    &self.repository_revision,
                    node,
                    plan,
                    discovery,
                )?;
                prepared.validate_against_request(&expected)?;
                self.implementation
                    .as_mut()
                    .ok_or(ProtocolViolation::ImplementationContract {
                        code: "implementation_state_missing",
                    })?
                    .record_prepared_context((**prepared).clone())?;
                self.refresh_implementation_step();
                Ok(())
            }
            ImplementationEvent::TargetContextSuperseded { supersession } => {
                let node = self.nodes.get(&supersession.node_id).ok_or_else(|| {
                    ProtocolViolation::UnknownNode {
                        node_id: supersession.node_id.clone(),
                    }
                })?;
                if node.kind != NodeKind::Implementation {
                    return Err(ProtocolViolation::ImplementationContract {
                        code: "target_context_supersession_node_kind_mismatch",
                    });
                }
                let NodeState::Active { attempt } = node.state else {
                    return Err(ProtocolViolation::InvalidNodeState {
                        node_id: node.id.clone(),
                        code: "target_context_supersession_owner_not_active",
                    });
                };
                if attempt != supersession.node_attempt {
                    return Err(ProtocolViolation::ImplementationContract {
                        code: "target_context_supersession_binding_mismatch",
                    });
                }
                let authoritative_drift = {
                    let (bound_node, target, context) =
                        self.mutation_binding(&node.id, &supersession.context_manifest_id)?;
                    let mutation_target = self.mutation.current_target(&node.id).ok_or(
                        ProtocolViolation::MutationContract {
                            code: "mutation_current_target_missing",
                        },
                    )?;
                    let mutation_attempt = mutation_target.attempts.values().next_back().ok_or(
                        ProtocolViolation::MutationContract {
                            code: "mutation_attempt_missing",
                        },
                    )?;
                    let failure = mutation_attempt.failure.as_ref().ok_or(
                        ProtocolViolation::MutationContract {
                            code: "target_context_drift_adoption_not_authoritative",
                        },
                    )?;
                    match select_mutation_recovery(
                        bound_node,
                        &target,
                        context,
                        &mutation_target.feasibility,
                        &mutation_attempt.policy,
                        failure,
                    )? {
                        MutationRecoveryDecision::RebuildContext { drift } => drift,
                        _ => {
                            return Err(ProtocolViolation::MutationContract {
                                code: "target_context_drift_adoption_not_authoritative",
                            });
                        }
                    }
                };
                if supersession.prepared_repository_revision != self.repository_revision
                    || authoritative_drift.expected_revision != self.repository_revision
                    || authoritative_drift.observed_revision
                        != supersession.replacement_repository_revision
                    || !authoritative_drift.context_rebuild_required
                {
                    return Err(ProtocolViolation::MutationContract {
                        code: "target_context_drift_adoption_not_authoritative",
                    });
                }
                let rebuilds = node.usage.context_rebuilds.saturating_add(1);
                if rebuilds > node.budget.max_context_rebuilds {
                    return Err(ProtocolViolation::BudgetExceeded {
                        node_id: Some(node.id.clone()),
                        dimension: "context_rebuilds",
                    });
                }
                let node_id = node.id.clone();
                self.implementation
                    .as_mut()
                    .ok_or(ProtocolViolation::ImplementationContract {
                        code: "implementation_state_missing",
                    })?
                    .supersede_context((**supersession).clone())?;
                self.nodes
                    .get_mut(&node_id)
                    .expect("context owner was checked")
                    .usage
                    .context_rebuilds = rebuilds;
                self.budgets.mission_usage.context_rebuilds = self
                    .budgets
                    .mission_usage
                    .context_rebuilds
                    .saturating_add(1);
                self.repository_revision = supersession.replacement_repository_revision.clone();
                self.refresh_implementation_step();
                Ok(())
            }
        }
    }

    fn apply_mutation_event(&mut self, event: &MutationEvent) -> Result<(), ProtocolViolation> {
        if !matches!(
            self.stage(),
            ProtocolStage::Implementation | ProtocolStage::Repair
        ) {
            return Err(ProtocolViolation::MutationContract {
                code: "mutation_event_outside_target_execution",
            });
        }
        self.validate_mutation_event_authority(event)?;
        let next_repository_revision = match event {
            MutationEvent::MutationVerified { evidence } => {
                self.validate_mutation_verification_chain(evidence)?;
                if evidence.repository_revision_before != self.repository_revision {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_verification_revision_mismatch",
                    });
                }
                Some(evidence.repository_revision_after.clone())
            }
            MutationEvent::ConvergenceEvaluated { convergence }
                if convergence.repository_revision_after != self.repository_revision =>
            {
                Some(convergence.repository_revision_after.clone())
            }
            _ => None,
        };
        self.mutation.apply(event)?;
        if let Some(repository_revision) = next_repository_revision {
            self.repository_revision = repository_revision;
        }
        self.refresh_implementation_step();
        self.refresh_validation_step();
        Ok(())
    }

    fn validate_mutation_event_authority(
        &self,
        event: &MutationEvent,
    ) -> Result<(), ProtocolViolation> {
        match event {
            MutationEvent::FeasibilityEvaluated { feasibility } => {
                let (node, target, context) =
                    self.mutation_binding(&feasibility.node_id, &feasibility.context_manifest_id)?;
                if feasibility.repository_revision != self.repository_revision
                    || feasibility != &evaluate_mutation_feasibility(node, &target, context)?
                {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_feasibility_not_authoritative",
                    });
                }
            }
            MutationEvent::AttemptPolicySelected { policy } => {
                let (node, target, context) =
                    self.mutation_binding(&policy.node_id, &policy.context_manifest_id)?;
                let feasibility =
                    self.mutation_feasibility(&policy.node_id, &policy.context_manifest_id)?;
                policy.validate_against(node, &target, context, feasibility)?;
                let previous =
                    self.event_log
                        .iter()
                        .rev()
                        .find_map(|stored| match &stored.envelope.payload {
                            DomainEvent::Mutation(MutationEvent::AttemptPolicySelected {
                                policy,
                            }) if policy.node_id == node.id => Some(policy),
                            _ => None,
                        });
                let expected = if let Some(previous) = previous {
                    let failure = self.mutation_failure(&previous.attempt_id)?;
                    if previous.context_manifest_id != context.context_manifest_id {
                        if failure.repository_drift.as_ref().is_none_or(|drift| {
                            drift.observed_revision != context.repository_revision
                        }) {
                            return Err(ProtocolViolation::MutationContract {
                                code: "mutation_rebuilt_context_not_authoritative",
                            });
                        }
                        let previous_context = self
                            .prepared_mutation_context(&previous.context_manifest_id)
                            .ok_or(ProtocolViolation::MutationContract {
                                code: "mutation_rebuilt_previous_context_missing",
                            })?;
                        select_rebuilt_mutation_policy(
                            &self.execution_id,
                            self.execution_attempt,
                            node,
                            &target,
                            context,
                            feasibility,
                            &previous_context.manifest,
                            previous,
                            failure,
                        )?
                    } else {
                        match select_mutation_recovery(
                            node,
                            &target,
                            context,
                            feasibility,
                            previous,
                            failure,
                        )? {
                            MutationRecoveryDecision::ModelRetry { policy }
                            | MutationRecoveryDecision::SelectFallback { policy } => policy,
                            _ => {
                                return Err(ProtocolViolation::MutationContract {
                                    code: "mutation_fallback_policy_not_authoritative",
                                });
                            }
                        }
                    }
                } else {
                    select_initial_mutation_policy(
                        &self.execution_id,
                        self.execution_attempt,
                        node,
                        &target,
                        context,
                        feasibility,
                    )?
                };
                if policy != &expected {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_policy_not_authoritative",
                    });
                }
            }
            MutationEvent::ActionPrepared { prepared } => {
                let (node, target, context) = self.mutation_binding(
                    &prepared.policy.node_id,
                    &prepared.policy.context_manifest_id,
                )?;
                let feasibility = self.mutation_feasibility(
                    &prepared.policy.node_id,
                    &prepared.policy.context_manifest_id,
                )?;
                let policy = self.mutation_policy(&prepared.policy.attempt_id)?;
                if policy != &prepared.policy {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_action_policy_mismatch",
                    });
                }
                let attempt = self.mutation.attempt(&policy.attempt_id).ok_or(
                    ProtocolViolation::MutationContract {
                        code: "mutation_action_attempt_missing",
                    },
                )?;
                let (action_index, prior_released_action_id) = attempt
                    .next_action_binding()?
                    .ok_or(ProtocolViolation::MutationContract {
                        code: "mutation_action_not_authorized",
                    })?;
                let remaining = self.planning_budget_remaining(&node.id)?;
                if MutationAdmissionBudgetRemaining::new(
                    remaining.model_calls,
                    remaining.cost_micros,
                    remaining.duration_ms,
                )
                .is_exhausted()
                {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_action_after_admission_budget_exhaustion",
                    });
                }
                let expected = build_prepared_mutation_action_retry(
                    node,
                    &target,
                    context,
                    feasibility,
                    policy.clone(),
                    action_index,
                    prior_released_action_id,
                    remaining.cost_micros,
                    remaining.duration_ms,
                )?;
                if **prepared != expected {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_action_not_authoritative",
                    });
                }
            }
            MutationEvent::ActionReleased { action_id } => {
                let prepared = self.mutation_action(action_id)?;
                let record = self
                    .budgets
                    .model_calls
                    .get(&prepared.admission.call_id)
                    .ok_or_else(|| ProtocolViolation::ModelCallLifecycle {
                        call_id: prepared.admission.call_id.clone(),
                        code: "mutation_action_release_without_call",
                    })?;
                if record.state != ModelCallState::ReconciledReleased {
                    return Err(ProtocolViolation::ModelCallLifecycle {
                        call_id: prepared.admission.call_id.clone(),
                        code: "mutation_action_release_before_reconciliation",
                    });
                }
            }
            MutationEvent::ActionRejected { failure } => {
                if self.mutation_failure(&failure.attempt_id).is_ok() {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_attempt_failure_already_recorded",
                    });
                }
                if failure.candidate_id.is_some()
                    || self.event_log.iter().any(|stored| {
                        matches!(
                            &stored.envelope.payload,
                            DomainEvent::Mutation(MutationEvent::CandidateRecorded { candidate })
                                if candidate.attempt_id == failure.attempt_id
                        )
                    })
                {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_action_rejection_after_candidate",
                    });
                }
                let policy = self.mutation_policy(&failure.attempt_id)?;
                let (_, _, context) =
                    self.mutation_binding(&failure.node_id, &failure.context_manifest_id)?;
                failure.validate_against(policy, context)?;
                let prepared = self.mutation_action_for_attempt(&failure.attempt_id)?;
                let record = self
                    .budgets
                    .model_calls
                    .get(&prepared.admission.call_id)
                    .ok_or_else(|| ProtocolViolation::ModelCallLifecycle {
                        call_id: prepared.admission.call_id.clone(),
                        code: "mutation_rejection_without_call",
                    })?;
                if !matches!(record.state, ModelCallState::ReconciledConsumed { .. }) {
                    return Err(ProtocolViolation::ModelCallLifecycle {
                        call_id: prepared.admission.call_id.clone(),
                        code: "mutation_rejection_before_consumed_reconciliation",
                    });
                }
            }
            MutationEvent::CandidateRecorded { candidate } => {
                if self.mutation_failure(&candidate.attempt_id).is_ok() {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_candidate_after_attempt_failure",
                    });
                }
                let prepared = self.mutation_action(&candidate.action_id)?;
                if prepared.policy.attempt_id != candidate.attempt_id {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_candidate_action_attempt_mismatch",
                    });
                }
                let (_, target, _) =
                    self.mutation_binding(&candidate.node_id, &candidate.context_manifest_id)?;
                candidate.validate_against(prepared, &target)?;
                let record = self
                    .budgets
                    .model_calls
                    .get(&prepared.admission.call_id)
                    .ok_or_else(|| ProtocolViolation::ModelCallLifecycle {
                        call_id: prepared.admission.call_id.clone(),
                        code: "mutation_candidate_without_call",
                    })?;
                if !matches!(record.state, ModelCallState::ReconciledConsumed { .. }) {
                    return Err(ProtocolViolation::ModelCallLifecycle {
                        call_id: prepared.admission.call_id.clone(),
                        code: "mutation_candidate_before_consumed_reconciliation",
                    });
                }
            }
            MutationEvent::AttemptFailed { failure } => {
                if self.mutation_failure(&failure.attempt_id).is_ok() {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_attempt_failure_already_recorded",
                    });
                }
                let policy = self.mutation_policy(&failure.attempt_id)?;
                let (_, _, context) =
                    self.mutation_binding(&failure.node_id, &failure.context_manifest_id)?;
                failure.validate_against(policy, context)?;
                let candidate_id =
                    failure
                        .candidate_id
                        .as_ref()
                        .ok_or(ProtocolViolation::MutationContract {
                            code: "mutation_attempt_failure_without_candidate",
                        })?;
                let candidate = self.mutation_candidate(candidate_id).map_err(|_| {
                    ProtocolViolation::MutationContract {
                        code: "mutation_failure_candidate_missing",
                    }
                })?;
                if candidate.attempt_id != policy.attempt_id
                    || candidate.node_id != failure.node_id
                    || candidate.target_id != failure.target_id
                    || candidate.context_manifest_id != failure.context_manifest_id
                    || failure.strategy != Some(candidate.strategy)
                {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_failure_candidate_binding_mismatch",
                    });
                }
                let application =
                    self.event_log
                        .iter()
                        .rev()
                        .find_map(|stored| match &stored.envelope.payload {
                            DomainEvent::Mutation(MutationEvent::ApplicationObserved {
                                request,
                                observation,
                            }) if request.candidate_id == candidate.candidate_id => {
                                Some(observation)
                            }
                            _ => None,
                        });
                if !mutation_failure_matches_stage(failure, Some(candidate), application) {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_attempt_failure_stage_mismatch",
                    });
                }
            }
            MutationEvent::ApplicationObserved {
                request,
                observation,
            } => {
                if self.mutation_failure(&request.attempt_id).is_ok() {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_application_after_attempt_failure",
                    });
                }
                let candidate = self.mutation_candidate(&request.candidate_id)?;
                let prepared = self.mutation_action(&candidate.action_id)?;
                if prepared.policy.attempt_id != request.attempt_id {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_apply_action_attempt_mismatch",
                    });
                }
                let (_, target, context) =
                    self.mutation_binding(&request.node_id, &request.context_manifest_id)?;
                let expected = MutationApplyRequest::new(prepared, candidate, &target, context)?;
                if request != &expected {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_apply_request_not_authoritative",
                    });
                }
                observation.validate_against(request)?;
            }
            MutationEvent::MutationVerified { evidence } => {
                if self.mutation_failure(&evidence.attempt_id).is_ok() {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_verification_after_attempt_failure",
                    });
                }
                self.validate_mutation_verification_chain(evidence)?;
            }
            MutationEvent::ConvergenceEvaluated { convergence } => {
                let policy = self.mutation_policy(&convergence.final_attempt_id)?;
                let failure = self.mutation_failure(&convergence.final_attempt_id)?;
                let node = self.nodes.get(&policy.node_id).ok_or_else(|| {
                    ProtocolViolation::UnknownNode {
                        node_id: policy.node_id.clone(),
                    }
                })?;
                let (_, target, context) =
                    self.mutation_binding(&policy.node_id, &policy.context_manifest_id)?;
                let feasibility =
                    self.mutation_feasibility(&policy.node_id, &policy.context_manifest_id)?;
                let (failure_revision_id, reason) = match select_mutation_recovery(
                    node,
                    &target,
                    context,
                    feasibility,
                    policy,
                    failure,
                )? {
                    MutationRecoveryDecision::NoSafeFallback {
                        failure_revision_id,
                        reason,
                    } => (failure_revision_id, reason),
                    MutationRecoveryDecision::RebuildContext { .. }
                        if node.kind == NodeKind::ValidationRepair =>
                    {
                        (
                            failure.failure_revision_id.clone(),
                            MutationConvergenceReason::ContextRebuildUnavailable,
                        )
                    }
                    _ => {
                        return Err(ProtocolViolation::MutationContract {
                            code: "mutation_convergence_not_authoritative",
                        });
                    }
                };
                if failure_revision_id != failure.failure_revision_id
                    || convergence != &MutationConvergence::new(policy, failure, reason)?
                {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_convergence_not_authoritative",
                    });
                }
            }
            MutationEvent::ReadinessConvergenceEvaluated { convergence } => {
                let (node, _, _) =
                    self.mutation_binding(&convergence.node_id, &convergence.context_manifest_id)?;
                let feasibility = self
                    .mutation_feasibility(&convergence.node_id, &convergence.context_manifest_id)?;
                if convergence.execution_id != self.execution_id
                    || convergence.execution_attempt != self.execution_attempt
                    || convergence.repository_revision != self.repository_revision
                    || convergence.feasibility_hash != feasibility.feasibility_hash
                {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_readiness_convergence_not_authoritative",
                    });
                }
                let expected = match &convergence.reason {
                    MutationReadinessConvergenceReason::NoFeasibleStrategy => {
                        MutationReadinessConvergence::no_feasible_strategy(
                            &self.execution_id,
                            self.execution_attempt,
                            feasibility,
                        )?
                    }
                    MutationReadinessConvergenceReason::AdmissionBudgetExhausted { .. } => {
                        let attempt_id = convergence.attempt_id.as_ref().ok_or(
                            ProtocolViolation::MutationContract {
                                code: "mutation_readiness_policy_missing",
                            },
                        )?;
                        let policy = self.mutation_policy(attempt_id)?;
                        let remaining = self.planning_budget_remaining(&node.id)?;
                        MutationReadinessConvergence::admission_budget_exhausted(
                            policy,
                            feasibility,
                            MutationAdmissionBudgetRemaining::new(
                                remaining.model_calls,
                                remaining.cost_micros,
                                remaining.duration_ms,
                            ),
                        )?
                    }
                    MutationReadinessConvergenceReason::UncontactedActionRetryExhausted {
                        ..
                    } => {
                        let attempt_id = convergence.attempt_id.as_ref().ok_or(
                            ProtocolViolation::MutationContract {
                                code: "mutation_readiness_policy_missing",
                            },
                        )?;
                        let policy = self.mutation_policy(attempt_id)?;
                        let attempt = self.mutation.attempt(attempt_id).ok_or(
                            ProtocolViolation::MutationContract {
                                code: "mutation_readiness_attempt_missing",
                            },
                        )?;
                        MutationReadinessConvergence::uncontacted_action_retry_exhausted(
                            policy,
                            feasibility,
                            attempt.released_action_count(),
                            attempt
                                .last_released_action_id()
                                .ok_or(ProtocolViolation::MutationContract {
                                    code: "mutation_last_released_action_missing",
                                })?
                                .clone(),
                        )?
                    }
                };
                if convergence != &expected {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_readiness_convergence_not_authoritative",
                    });
                }
            }
        }
        Ok(())
    }

    fn mutation_binding(
        &self,
        node_id: &NodeId,
        context_manifest_id: &ContextManifestId,
    ) -> Result<(&ExecutionNode, PlannedTargetV1, &TargetContextManifest), ProtocolViolation> {
        let node = self
            .nodes
            .get(node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: node_id.clone(),
            })?;
        if !matches!(node.state, NodeState::Active { .. }) {
            return Err(ProtocolViolation::MutationContract {
                code: "mutation_owner_not_active",
            });
        }
        let plan = self
            .planning
            .as_ref()
            .and_then(|planning| planning.accepted_plan.as_ref())
            .ok_or(ProtocolViolation::MutationContract {
                code: "mutation_plan_target_missing",
            })?;
        let (target, context) = match node.kind {
            NodeKind::Implementation => {
                let implementation = self.implementation.as_ref().ok_or(
                    ProtocolViolation::ImplementationContract {
                        code: "implementation_state_missing",
                    },
                )?;
                let context = implementation.context_for_node(node_id).ok_or(
                    ProtocolViolation::MutationContract {
                        code: "mutation_current_context_missing",
                    },
                )?;
                let target_id = implementation.node_targets.get(node_id).ok_or(
                    ProtocolViolation::MutationContract {
                        code: "mutation_target_mapping_missing",
                    },
                )?;
                let target = plan
                    .targets
                    .iter()
                    .find(|target| &target.target_id == target_id)
                    .cloned()
                    .ok_or(ProtocolViolation::MutationContract {
                        code: "mutation_plan_target_missing",
                    })?;
                (target, context)
            }
            NodeKind::ValidationRepair => {
                if self.stage() != ProtocolStage::Repair {
                    return Err(ProtocolViolation::MutationContract {
                        code: "repair_mutation_owner_outside_repair",
                    });
                }
                let validation =
                    self.validation
                        .as_ref()
                        .ok_or(ProtocolViolation::ValidationContract {
                            code: "validation_state_missing",
                        })?;
                let failure =
                    validation
                        .current_failure()
                        .ok_or(ProtocolViolation::ValidationContract {
                            code: "repair_mutation_without_failure",
                        })?;
                let selection = validation
                    .selections
                    .get(&failure.failure_revision_id)
                    .filter(|selection| selection.repair_node.id == *node_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "repair_mutation_selection_missing",
                    })?;
                let context = validation.repair_contexts.context_for_node(node_id).ok_or(
                    ProtocolViolation::MutationContract {
                        code: "repair_mutation_current_context_missing",
                    },
                )?;
                let baseline = self
                    .repair_mutation_baselines(failure)
                    .remove(&selection.intent.target_id)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "repair_mutation_baseline_missing",
                    })?;
                if context.repository_fingerprint
                    != baseline.evidence().repository_fingerprint_after
                {
                    return Err(ProtocolViolation::MutationContract {
                        code: "repair_mutation_repository_fingerprint_mismatch",
                    });
                }
                let target = repair_target_for_selection(selection, failure, plan, &baseline)?;
                (target, context)
            }
            _ => {
                return Err(ProtocolViolation::MutationContract {
                    code: "mutation_owner_kind_invalid",
                });
            }
        };
        let current_after_verified_mutation = self.event_log.iter().any(|stored| {
            matches!(
                &stored.envelope.payload,
                DomainEvent::Mutation(MutationEvent::MutationVerified { evidence })
                    if evidence.node_id == *node_id
                        && evidence.context_manifest_id == *context_manifest_id
                        && evidence.repository_revision_before == context.repository_revision
                        && evidence.repository_revision_after == self.repository_revision
            )
        });
        if &context.context_manifest_id != context_manifest_id
            || (context.repository_revision != self.repository_revision
                && !current_after_verified_mutation)
        {
            return Err(ProtocolViolation::MutationContract {
                code: "mutation_current_context_binding_mismatch",
            });
        }
        Ok((node, target, context))
    }

    fn prepared_mutation_context(
        &self,
        context_manifest_id: &ContextManifestId,
    ) -> Option<&PreparedTargetContext> {
        self.implementation
            .as_ref()
            .and_then(|implementation| implementation.prepared_context(context_manifest_id))
            .or_else(|| {
                self.validation.as_ref().and_then(|validation| {
                    validation
                        .repair_contexts
                        .prepared_context(context_manifest_id)
                })
            })
    }

    fn mutation_feasibility(
        &self,
        node_id: &NodeId,
        context_manifest_id: &ContextManifestId,
    ) -> Result<&MutationFeasibilitySet, ProtocolViolation> {
        self.event_log
            .iter()
            .rev()
            .find_map(|stored| match &stored.envelope.payload {
                DomainEvent::Mutation(MutationEvent::FeasibilityEvaluated { feasibility })
                    if &feasibility.node_id == node_id
                        && &feasibility.context_manifest_id == context_manifest_id =>
                {
                    Some(feasibility)
                }
                _ => None,
            })
            .ok_or(ProtocolViolation::MutationContract {
                code: "mutation_feasibility_missing",
            })
    }

    fn mutation_policy(
        &self,
        attempt_id: &MutationAttemptId,
    ) -> Result<&MutationAttemptPolicy, ProtocolViolation> {
        self.event_log
            .iter()
            .rev()
            .find_map(|stored| match &stored.envelope.payload {
                DomainEvent::Mutation(MutationEvent::AttemptPolicySelected { policy })
                    if &policy.attempt_id == attempt_id =>
                {
                    Some(policy)
                }
                _ => None,
            })
            .ok_or(ProtocolViolation::MutationContract {
                code: "mutation_policy_missing",
            })
    }

    fn mutation_failure(
        &self,
        attempt_id: &MutationAttemptId,
    ) -> Result<&MutationFailure, ProtocolViolation> {
        self.event_log
            .iter()
            .rev()
            .find_map(|stored| match &stored.envelope.payload {
                DomainEvent::Mutation(
                    MutationEvent::ActionRejected { failure }
                    | MutationEvent::AttemptFailed { failure },
                ) if &failure.attempt_id == attempt_id => Some(failure),
                _ => None,
            })
            .ok_or(ProtocolViolation::MutationContract {
                code: "mutation_failure_missing",
            })
    }

    fn mutation_action(
        &self,
        action_id: &ActionId,
    ) -> Result<&PreparedMutationAction, ProtocolViolation> {
        self.event_log
            .iter()
            .find_map(|stored| match &stored.envelope.payload {
                DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared })
                    if &prepared.provider_request.action_id == action_id =>
                {
                    Some(&**prepared)
                }
                _ => None,
            })
            .ok_or(ProtocolViolation::MutationContract {
                code: "mutation_action_missing",
            })
    }

    fn mutation_action_for_attempt(
        &self,
        attempt_id: &MutationAttemptId,
    ) -> Result<&PreparedMutationAction, ProtocolViolation> {
        self.event_log
            .iter()
            .rev()
            .find_map(|stored| match &stored.envelope.payload {
                DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared })
                    if &prepared.policy.attempt_id == attempt_id =>
                {
                    Some(&**prepared)
                }
                _ => None,
            })
            .ok_or(ProtocolViolation::MutationContract {
                code: "mutation_action_missing",
            })
    }

    fn mutation_candidate(
        &self,
        candidate_id: &MutationCandidateId,
    ) -> Result<&MutationCandidateRecord, ProtocolViolation> {
        self.event_log
            .iter()
            .find_map(|stored| match &stored.envelope.payload {
                DomainEvent::Mutation(MutationEvent::CandidateRecorded { candidate })
                    if &candidate.candidate_id == candidate_id =>
                {
                    Some(candidate)
                }
                _ => None,
            })
            .ok_or(ProtocolViolation::MutationContract {
                code: "mutation_candidate_missing",
            })
    }

    fn apply_graph_event(&mut self, event: &GraphEvent) -> Result<(), ProtocolViolation> {
        let continuing_node_id = match event {
            GraphEvent::NodeWaiting { node_id, .. }
            | GraphEvent::NodeResumed { node_id, .. }
            | GraphEvent::NodeSucceeded { node_id, .. } => Some(node_id),
            _ => None,
        };
        if continuing_node_id.is_some_and(|node_id| {
            self.nodes
                .get(node_id)
                .is_some_and(|node| node.kind == NodeKind::Implementation)
                && self.mutation.current_target(node_id).is_some_and(|target| {
                    target.convergence.is_some() || target.readiness_convergence.is_some()
                })
        }) {
            return Err(ProtocolViolation::MutationContract {
                code: "implementation_progress_after_mutation_convergence",
            });
        }
        match event {
            GraphEvent::NodesAdded {
                plan_proof_id,
                nodes,
            } => self.add_plan_nodes(plan_proof_id, nodes),
            GraphEvent::ValidationRepairNodeAdded {
                eligibility_proof_id,
                node,
            } => self.add_validation_repair_node(eligibility_proof_id, node),
            GraphEvent::NodeStarted { node_id, attempt } => self.start_node(node_id, *attempt),
            GraphEvent::NodeWaiting { node_id, effect_id } => self.wait_node(node_id, effect_id),
            GraphEvent::NodeResumed { node_id, effect_id } => self.resume_node(node_id, effect_id),
            GraphEvent::NodeSucceeded { node_id, proof_id } => self.succeed_node(node_id, proof_id),
            GraphEvent::NodeFailed {
                node_id,
                failure_revision_id,
                terminal,
            } => self.fail_node(node_id, failure_revision_id, *terminal),
        }?;
        self.refresh_implementation_step();
        self.refresh_validation_step();
        Ok(())
    }

    fn apply_budget_event(&mut self, event: &BudgetEvent) -> Result<(), ProtocolViolation> {
        match event {
            BudgetEvent::ModelCallAdmitted { admission } => {
                self.admit_model_call(admission.clone())
            }
            BudgetEvent::ModelCallReserved { call_id } => self.reserve_model_call(call_id),
            BudgetEvent::ProviderDispatchStarted {
                call_id,
                payload_hash,
            } => self.start_provider_dispatch(call_id, payload_hash),
            BudgetEvent::ModelCallReconciled { call_id, result } => {
                self.reconcile_model_call(call_id, result)
            }
        }
    }

    fn apply_lifecycle_event(&mut self, event: &LifecycleEvent) -> Result<(), ProtocolViolation> {
        match event {
            LifecycleEvent::PositionAdvanced { from, to, proof_id } => {
                self.advance_position(*from, *to, proof_id)
            }
        }
    }

    fn apply_terminal_event(&mut self, event: &TerminalEvent) -> Result<(), ProtocolViolation> {
        match event {
            TerminalEvent::CanonicalResultRecorded { result } => {
                self.record_canonical_result(result.clone())
            }
        }
    }

    fn record_proof(&mut self, proof: ProofRecord) -> Result<(), ProtocolViolation> {
        if proof.node_ids.iter().any(|node_id| {
            self.nodes
                .get(node_id)
                .is_some_and(|node| node.kind == NodeKind::Implementation)
                && self.mutation.current_target(node_id).is_some_and(|target| {
                    target.convergence.is_some() || target.readiness_convergence.is_some()
                })
        }) {
            return Err(ProtocolViolation::MutationContract {
                code: "implementation_proof_after_mutation_convergence",
            });
        }
        if self.proofs.contains_key(&proof.id) {
            return Err(ProtocolViolation::DuplicateProof { proof_id: proof.id });
        }
        self.validate_new_proof(&proof)?;
        self.proofs.insert(proof.id.clone(), proof);
        Ok(())
    }

    fn validate_new_proof(&self, proof: &ProofRecord) -> Result<(), ProtocolViolation> {
        if proof.repository_revision != self.repository_revision {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "repository_revision_mismatch",
            });
        }
        if proof.detail_hash.trim().is_empty() {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "detail_hash_missing",
            });
        }
        let unique_nodes = proof.node_ids.iter().collect::<BTreeSet<_>>();
        if unique_nodes.len() != proof.node_ids.len() {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "duplicate_node_identity",
            });
        }
        let unique_evidence = proof.related_evidence_ids.iter().collect::<BTreeSet<_>>();
        if unique_evidence.len() != proof.related_evidence_ids.len() {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "duplicate_evidence_identity",
            });
        }
        for node_id in &proof.node_ids {
            if !self.nodes.contains_key(node_id) {
                return Err(ProtocolViolation::UnknownNode {
                    node_id: node_id.clone(),
                });
            }
        }
        match proof.kind {
            ProofKind::RepositoryProfile => {
                self.require_stage_for_proof(proof, ProtocolStage::Profiling)?;
                if let Some(profile) = &self.repository_profile
                    && (proof.detail_hash != repository_profile_proof_hash(&profile.profile_id)
                        || !proof.node_ids.is_empty()
                        || !proof.related_evidence_ids.is_empty())
                {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof.id.clone(),
                        code: "repository_profile_proof_binding_mismatch",
                    });
                }
            }
            ProofKind::DiscoveryImpactMap => {
                self.require_active_proof_node(proof, NodeKind::Discovery)?;
                if let Some(discovery) = &self.discovery {
                    let Some(impact_map) = &discovery.impact_map else {
                        return Err(ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "discovery_impact_map_evidence_missing",
                        });
                    };
                    if proof.related_evidence_ids != [impact_map.evidence_id.clone()]
                        || proof.detail_hash
                            != discovery_impact_map_proof_hash(&impact_map.evidence_id)
                        || discovery.convergence
                            != Some(DiscoveryConvergence::ImpactMapAccepted {
                                evidence_id: impact_map.evidence_id.clone(),
                            })
                    {
                        return Err(ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "discovery_impact_map_proof_binding_mismatch",
                        });
                    }
                }
            }
            ProofKind::PlanAccepted => {
                self.require_active_proof_node(proof, NodeKind::Planning)?;
                if let Some(planning) = &self.planning {
                    let plan = planning.accepted_plan.as_ref().ok_or_else(|| {
                        ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "plan_accepted_proof_without_accepted_plan",
                        }
                    })?;
                    if proof != &self.planning_acceptance_proof(plan) {
                        return Err(ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "plan_accepted_proof_binding_mismatch",
                        });
                    }
                }
            }
            ProofKind::MutationVerified => {
                self.require_one_active_node_matching(proof, |kind| {
                    matches!(kind, NodeKind::Implementation | NodeKind::ValidationRepair)
                })?;
                let node_id = proof
                    .node_ids
                    .first()
                    .expect("one active proof node was required");
                let is_typed_implementation_node =
                    self.implementation.as_ref().is_some_and(|implementation| {
                        implementation.node_targets.contains_key(node_id)
                    });
                let is_typed_repair_node = self.validation.as_ref().is_some_and(|validation| {
                    validation
                        .current_failure()
                        .and_then(|failure| validation.selections.get(&failure.failure_revision_id))
                        .is_some_and(|selection| selection.repair_node.id == *node_id)
                });
                if is_typed_implementation_node || is_typed_repair_node {
                    let evidence = self
                        .mutation
                        .current_target(node_id)
                        .and_then(|target| target.verified.as_ref())
                        .ok_or_else(|| ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "mutation_verification_evidence_missing",
                        })?;
                    if proof != &self.mutation_verification_proof(evidence)? {
                        return Err(ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "mutation_verification_proof_binding_mismatch",
                        });
                    }
                } else {
                    let evidence = self.event_log.iter().find_map(|stored| {
                        let DomainEvent::Mutation(MutationEvent::MutationVerified { evidence }) =
                            &stored.envelope.payload
                        else {
                            return None;
                        };
                        proof
                            .related_evidence_ids
                            .contains(&evidence.evidence_id)
                            .then_some(evidence)
                    });
                    let has_recorded_mutation_for_node =
                        proof.node_ids.first().is_some_and(|node_id| {
                            self.event_log.iter().any(|stored| {
                                matches!(
                                    &stored.envelope.payload,
                                    DomainEvent::Mutation(MutationEvent::MutationVerified { evidence })
                                        if &evidence.node_id == node_id
                                )
                            })
                        });
                    if has_recorded_mutation_for_node && evidence.is_none() {
                        return Err(ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "mutation_verification_evidence_missing",
                        });
                    }
                    if let Some(evidence) = evidence
                        && proof != &self.mutation_verification_proof(evidence)?
                    {
                        return Err(ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "mutation_verification_proof_binding_mismatch",
                        });
                    }
                }
            }
            ProofKind::AlreadySatisfied => {
                self.require_one_active_node_matching(proof, |kind| {
                    matches!(kind, NodeKind::Implementation | NodeKind::ValidationRepair)
                })?;
                let node_id = proof
                    .node_ids
                    .first()
                    .expect("one active proof node was required");
                if self.validation.is_some()
                    && self
                        .nodes
                        .get(node_id)
                        .is_some_and(|node| node.kind == NodeKind::ValidationRepair)
                {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof.id.clone(),
                        code: "repair_already_satisfied_proof_unavailable",
                    });
                }
                if self
                    .implementation
                    .as_ref()
                    .is_some_and(|implementation| implementation.node_targets.contains_key(node_id))
                {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof.id.clone(),
                        code: "already_satisfied_proof_unavailable",
                    });
                }
            }
            ProofKind::ImplementationBarrier => {
                self.validate_implementation_barrier_proof(proof)?;
            }
            ProofKind::ValidationPassed => {
                self.require_active_proof_node(proof, NodeKind::Validation)?;
                let node_id = proof
                    .node_ids
                    .first()
                    .expect("one active validation node was required");
                if self.validation.is_some() && proof != &self.validation_pass_proof(node_id)? {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof.id.clone(),
                        code: "validation_pass_proof_binding_mismatch",
                    });
                }
            }
            ProofKind::ValidationFailure => {
                self.require_failed_validation_proof(proof)?;
                if let Some(validation) = &self.validation {
                    let failure = validation.current_failure().ok_or_else(|| {
                        ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "validation_failure_revision_missing",
                        }
                    })?;
                    if proof != &self.validation_failure_proof(failure)? {
                        return Err(ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "validation_failure_proof_binding_mismatch",
                        });
                    }
                }
            }
            ProofKind::RepairEligibility => {
                self.require_stage_for_proof(proof, ProtocolStage::Repair)?;
                let Some(current_failure) = self.latest_transition_proof.as_ref() else {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof.id.clone(),
                        code: "current_validation_failure_proof_missing",
                    });
                };
                let [related_failure] = proof.related_proof_ids.as_slice() else {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof.id.clone(),
                        code: "repair_eligibility_not_bound_to_current_failure",
                    });
                };
                if self.proof_kind(current_failure) != Some(ProofKind::ValidationFailure)
                    || related_failure != current_failure
                {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof.id.clone(),
                        code: "repair_eligibility_not_bound_to_current_failure",
                    });
                }
                if let Some(validation) = &self.validation {
                    let failure_id = validation.active_failure.as_ref().ok_or_else(|| {
                        ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "repair_failure_revision_missing",
                        }
                    })?;
                    let selection = validation.selections.get(failure_id).ok_or_else(|| {
                        ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "repair_selection_missing",
                        }
                    })?;
                    if proof != &self.repair_eligibility_proof(selection)? {
                        return Err(ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "repair_eligibility_proof_binding_mismatch",
                        });
                    }
                }
            }
            ProofKind::RepairVerified => {
                self.require_active_proof_node(proof, NodeKind::ValidationRepair)?;
                if self.validation.is_some() {
                    let evidence = proof
                        .related_evidence_ids
                        .first()
                        .and_then(|evidence_id| {
                            self.mutation
                                .current_target(
                                    proof
                                        .node_ids
                                        .first()
                                        .expect("active repair proof node was required"),
                                )
                                .and_then(|target| target.verified.as_ref())
                                .filter(|evidence| &evidence.evidence_id == evidence_id)
                        })
                        .ok_or_else(|| ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "repair_verification_evidence_missing",
                        })?;
                    if proof != &self.repair_verification_proof(evidence)? {
                        return Err(ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "repair_verification_proof_binding_mismatch",
                        });
                    }
                }
            }
            ProofKind::ValidationRerunScheduled => {
                self.validate_validation_rerun_proof(proof)?;
                if let Some(validation) = &self.validation {
                    let rerun = validation.pending_rerun.as_ref().ok_or_else(|| {
                        ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "validation_rerun_record_missing",
                        }
                    })?;
                    if proof != &self.validation_rerun_proof(rerun)? {
                        return Err(ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "validation_rerun_proof_binding_mismatch",
                        });
                    }
                }
            }
            ProofKind::RequiredValidationPassed => {
                self.validate_required_validation_proof(proof)?;
                if self.validation.is_some() && proof != &self.required_validation_proof()? {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof.id.clone(),
                        code: "required_validation_proof_binding_mismatch",
                    });
                }
            }
            ProofKind::ReviewCompleted => {
                self.require_active_proof_node(proof, NodeKind::Review)?;
                if self.review.is_some() && proof != &self.review_completion_proof()? {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof.id.clone(),
                        code: "review_completion_proof_binding_mismatch",
                    });
                }
            }
            ProofKind::CompletionEvaluated => {
                self.require_active_proof_node(proof, NodeKind::CompletionEvaluation)?;
                if self.review.is_some() && proof != &self.completion_evaluation_proof()? {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof.id.clone(),
                        code: "completion_evaluation_proof_binding_mismatch",
                    });
                }
            }
            ProofKind::PublicationEligibility => {
                self.validate_publication_eligibility_proof(proof)?;
            }
            ProofKind::PublicationCompleted => {
                self.require_active_proof_node(proof, NodeKind::Publication)?;
                if self.publication.is_some() && proof != &self.publication_completion_proof()? {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof.id.clone(),
                        code: "publication_completion_proof_binding_mismatch",
                    });
                }
            }
            ProofKind::NoOpSatisfied => {
                self.require_active_proof_node(proof, NodeKind::Planning)?;
                if let Some(planning) = &self.planning {
                    let no_op = planning.accepted_no_op.as_ref().ok_or_else(|| {
                        ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "no_op_proof_without_accepted_no_op",
                        }
                    })?;
                    if proof != &self.planning_no_op_proof(no_op) {
                        return Err(ProtocolViolation::InvalidProof {
                            proof_id: proof.id.clone(),
                            code: "no_op_proof_binding_mismatch",
                        });
                    }
                }
            }
        }
        for related_proof_id in &proof.related_proof_ids {
            if !self.proofs.contains_key(related_proof_id) {
                return Err(ProtocolViolation::UnknownProof {
                    proof_id: related_proof_id.clone(),
                });
            }
        }
        for evidence_id in &proof.related_evidence_ids {
            if !self.has_protocol_evidence(evidence_id) {
                return Err(ProtocolViolation::InvalidProof {
                    proof_id: proof.id.clone(),
                    code: "related_evidence_missing",
                });
            }
        }
        Ok(())
    }

    fn require_stage_for_proof(
        &self,
        proof: &ProofRecord,
        expected: ProtocolStage,
    ) -> Result<(), ProtocolViolation> {
        if self.stage() != expected {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "proof_recorded_in_wrong_position",
            });
        }
        Ok(())
    }

    fn require_active_proof_node(
        &self,
        proof: &ProofRecord,
        expected: NodeKind,
    ) -> Result<(), ProtocolViolation> {
        self.require_one_active_node_matching(proof, |kind| kind == expected)
    }

    fn require_one_active_node_matching(
        &self,
        proof: &ProofRecord,
        accepts: impl FnOnce(NodeKind) -> bool,
    ) -> Result<(), ProtocolViolation> {
        let [node_id] = proof.node_ids.as_slice() else {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "exactly_one_node_required",
            });
        };
        let node = self
            .nodes
            .get(node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: node_id.clone(),
            })?;
        if !accepts(node.kind) || !matches!(&node.state, NodeState::Active { .. }) {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "active_node_kind_mismatch",
            });
        }
        Ok(())
    }

    fn require_failed_validation_proof(
        &self,
        proof: &ProofRecord,
    ) -> Result<(), ProtocolViolation> {
        self.require_stage_for_proof(proof, ProtocolStage::Validation)?;
        let [node_id] = proof.node_ids.as_slice() else {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "exactly_one_validation_node_required",
            });
        };
        let node = self
            .nodes
            .get(node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: node_id.clone(),
            })?;
        if node.kind != NodeKind::Validation
            || !matches!(node.state, NodeState::FailedRecoverable { .. })
        {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "validation_node_is_not_recoverably_failed",
            });
        }
        Ok(())
    }

    fn validate_implementation_barrier_proof(
        &self,
        proof: &ProofRecord,
    ) -> Result<(), ProtocolViolation> {
        self.require_stage_for_proof(proof, ProtocolStage::Implementation)?;
        let required = self
            .required_nodes(NodeKind::Implementation)
            .into_iter()
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>();
        let supplied = proof.node_ids.iter().cloned().collect::<BTreeSet<_>>();
        if required.is_empty() || supplied != required {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "implementation_barrier_node_set_mismatch",
            });
        }
        for node_id in required {
            let node = self.nodes.get(&node_id).expect("required node exists");
            if !matches!(
                self.succeeded_proof_kind(node),
                Some(ProofKind::MutationVerified | ProofKind::AlreadySatisfied)
            ) {
                return Err(ProtocolViolation::InvalidProof {
                    proof_id: proof.id.clone(),
                    code: "implementation_node_not_verified",
                });
            }
        }
        if self.event_log.iter().any(|stored| {
            matches!(
                stored.envelope.payload,
                DomainEvent::Mutation(MutationEvent::MutationVerified { .. })
            )
        }) && proof != &self.implementation_barrier_proof()?
        {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "implementation_barrier_proof_binding_mismatch",
            });
        }
        Ok(())
    }

    fn validate_required_validation_proof(
        &self,
        proof: &ProofRecord,
    ) -> Result<(), ProtocolViolation> {
        self.require_stage_for_proof(proof, ProtocolStage::Validation)?;
        let required = self
            .required_nodes(NodeKind::Validation)
            .into_iter()
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>();
        let supplied = proof.node_ids.iter().cloned().collect::<BTreeSet<_>>();
        if required.is_empty() || supplied != required {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "required_validation_node_set_mismatch",
            });
        }
        for node_id in required {
            let node = self.nodes.get(&node_id).expect("required node exists");
            let Some(ProofKind::ValidationPassed) = self.succeeded_proof_kind(node) else {
                return Err(ProtocolViolation::InvalidProof {
                    proof_id: proof.id.clone(),
                    code: "required_validation_not_passed",
                });
            };
            let NodeState::Succeeded { proof_id } = &node.state else {
                unreachable!("proof kind came from a succeeded node")
            };
            if self
                .proofs
                .get(proof_id)
                .is_none_or(|validation| validation.repository_revision != self.repository_revision)
            {
                return Err(ProtocolViolation::InvalidProof {
                    proof_id: proof.id.clone(),
                    code: "required_validation_is_stale",
                });
            }
        }
        Ok(())
    }

    fn validate_validation_rerun_proof(
        &self,
        proof: &ProofRecord,
    ) -> Result<(), ProtocolViolation> {
        self.require_stage_for_proof(proof, ProtocolStage::Repair)?;
        let Some(current_failure_id) = self.latest_transition_proof.as_ref() else {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "current_validation_failure_proof_missing",
            });
        };
        let Some(current_failure) = self.proofs.get(current_failure_id) else {
            return Err(ProtocolViolation::UnknownProof {
                proof_id: current_failure_id.clone(),
            });
        };
        if current_failure.kind != ProofKind::ValidationFailure {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "current_validation_failure_proof_missing",
            });
        }
        let [originating_validation_id] = current_failure.node_ids.as_slice() else {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "originating_validation_gate_missing",
            });
        };
        let Some(originating_validation) = self.nodes.get(originating_validation_id) else {
            return Err(ProtocolViolation::UnknownNode {
                node_id: originating_validation_id.clone(),
            });
        };
        let [scheduled_validation_id] = proof.node_ids.as_slice() else {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "originating_validation_gate_mismatch",
            });
        };
        if originating_validation.kind != NodeKind::Validation
            || !matches!(
                originating_validation.state,
                NodeState::FailedRecoverable { .. }
            )
            || scheduled_validation_id != originating_validation_id
        {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "originating_validation_gate_mismatch",
            });
        }

        let repair_nodes = self
            .event_log
            .iter()
            .filter_map(|stored| match &stored.envelope.payload {
                DomainEvent::Graph(GraphEvent::ValidationRepairNodeAdded {
                    eligibility_proof_id,
                    node,
                }) if self
                    .proofs
                    .get(eligibility_proof_id)
                    .is_some_and(|eligibility| {
                        eligibility.kind == ProofKind::RepairEligibility
                            && eligibility.related_proof_ids.contains(current_failure_id)
                    }) =>
                {
                    Some(node.id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let [repair_node_id] = repair_nodes.as_slice() else {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "selected_repair_node_missing",
            });
        };
        let repair_node =
            self.nodes
                .get(repair_node_id)
                .ok_or_else(|| ProtocolViolation::UnknownNode {
                    node_id: repair_node_id.clone(),
                })?;
        let NodeState::Succeeded {
            proof_id: repair_proof_id,
        } = &repair_node.state
        else {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "selected_repair_node_not_verified",
            });
        };
        let repair_proof =
            self.proofs
                .get(repair_proof_id)
                .ok_or_else(|| ProtocolViolation::UnknownProof {
                    proof_id: repair_proof_id.clone(),
                })?;
        let [scheduled_repair_proof_id] = proof.related_proof_ids.as_slice() else {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "verified_repair_proof_mismatch",
            });
        };
        if !repair_node.required
            || repair_node.kind != NodeKind::ValidationRepair
            || repair_proof.kind != ProofKind::RepairVerified
            || !repair_proof.node_ids.contains(repair_node_id)
            || scheduled_repair_proof_id != repair_proof_id
        {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "verified_repair_proof_mismatch",
            });
        }
        Ok(())
    }

    fn validate_publication_eligibility_proof(
        &self,
        proof: &ProofRecord,
    ) -> Result<(), ProtocolViolation> {
        self.require_stage_for_proof(proof, ProtocolStage::Review)?;
        if self.review.is_some() {
            let expected = self.publication_eligibility_proof()?;
            if proof != &expected {
                return Err(ProtocolViolation::InvalidProof {
                    proof_id: proof.id.clone(),
                    code: "publication_eligibility_proof_binding_mismatch",
                });
            }
            return Ok(());
        }
        if self.active_node().is_some() || self.has_open_model_call() {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "work_or_reservation_still_active",
            });
        }
        if !self.has_proof_kind_for_current_revision(ProofKind::ImplementationBarrier) {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "implementation_barrier_missing",
            });
        }
        if !self.has_proof_kind_for_current_revision(ProofKind::RequiredValidationPassed) {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "required_validation_proof_missing",
            });
        }
        for kind in [NodeKind::Review, NodeKind::CompletionEvaluation] {
            let required_nodes = self.required_nodes(kind);
            if required_nodes.is_empty()
                || required_nodes
                    .into_iter()
                    .any(|node| !matches!(node.state, NodeState::Succeeded { .. }))
            {
                return Err(ProtocolViolation::InvalidProof {
                    proof_id: proof.id.clone(),
                    code: "review_or_completion_incomplete",
                });
            }
        }
        if proof
            .related_proof_ids
            .iter()
            .all(|proof_id| self.proof_kind(proof_id) != Some(ProofKind::CompletionEvaluated))
        {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "completion_evaluation_proof_missing",
            });
        }
        if self.nodes.values().any(|node| {
            node.required
                && matches!(
                    node.state,
                    NodeState::FailedRecoverable { .. } | NodeState::FailedTerminal { .. }
                )
        }) {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof.id.clone(),
                code: "required_failure_unresolved",
            });
        }
        Ok(())
    }

    fn add_plan_nodes(
        &mut self,
        plan_proof_id: &ProofId,
        nodes: &[NodeSpec],
    ) -> Result<(), ProtocolViolation> {
        if self.stage() != ProtocolStage::Planning {
            return Err(ProtocolViolation::IllegalTransition {
                from: self.stage(),
                to: ProtocolStage::Implementation,
            });
        }
        if self.proof_kind(plan_proof_id) != Some(ProofKind::PlanAccepted) {
            return Err(ProtocolViolation::MissingTransitionProof {
                required: ProofKind::PlanAccepted,
            });
        }
        if let Some(planning) = &self.planning {
            let plan =
                planning
                    .accepted_plan
                    .as_ref()
                    .ok_or(ProtocolViolation::PlanningContract {
                        code: "plan_graph_without_accepted_plan",
                    })?;
            let expected_proof = self.planning_acceptance_proof(plan);
            if plan_proof_id != &expected_proof.id
                || self.proofs.get(plan_proof_id) != Some(&expected_proof)
                || nodes != self.materialized_planning_nodes(plan)?
            {
                return Err(ProtocolViolation::PlanningContract {
                    code: "plan_graph_not_accepted_plan_projection",
                });
            }
        }
        if self.event_log.iter().any(|stored| {
            matches!(
                stored.envelope.payload,
                DomainEvent::Graph(GraphEvent::NodesAdded { .. })
            )
        }) {
            return Err(ProtocolViolation::InvalidGraph {
                code: "plan_graph_already_materialized",
                node_id: None,
            });
        }
        if nodes.is_empty() {
            return Err(ProtocolViolation::InvalidGraph {
                code: "accepted_mutation_plan_has_no_nodes",
                node_id: None,
            });
        }
        let incoming_ids = nodes
            .iter()
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>();
        if incoming_ids.len() != nodes.len() {
            return Err(ProtocolViolation::InvalidGraph {
                code: "duplicate_incoming_node_id",
                node_id: None,
            });
        }
        for node in nodes {
            if self.nodes.contains_key(&node.id) {
                return Err(ProtocolViolation::DuplicateNode {
                    node_id: node.id.clone(),
                });
            }
            if matches!(
                node.kind,
                NodeKind::Discovery | NodeKind::Planning | NodeKind::ValidationRepair
            ) {
                return Err(ProtocolViolation::InvalidGraph {
                    code: "plan_contains_illegal_node_kind",
                    node_id: Some(node.id.clone()),
                });
            }
            self.validate_node_budget(node)?;
            for dependency in &node.dependencies {
                if !self.nodes.contains_key(dependency) && !incoming_ids.contains(dependency) {
                    return Err(ProtocolViolation::InvalidGraph {
                        code: "unknown_dependency",
                        node_id: Some(node.id.clone()),
                    });
                }
                if dependency == &node.id {
                    return Err(ProtocolViolation::InvalidGraph {
                        code: "self_dependency",
                        node_id: Some(node.id.clone()),
                    });
                }
            }
        }
        for spec in nodes.iter().cloned() {
            self.node_order.push(spec.id.clone());
            self.nodes
                .insert(spec.id.clone(), ExecutionNode::from(spec));
        }
        self.validate_graph_acyclic()?;
        self.validate_plan_topology()?;
        Ok(())
    }

    fn add_validation_repair_node(
        &mut self,
        eligibility_proof_id: &ProofId,
        node: &NodeSpec,
    ) -> Result<(), ProtocolViolation> {
        if self.stage() != ProtocolStage::Repair {
            return Err(ProtocolViolation::WrongPosition {
                node_id: node.id.clone(),
                position: self.stage(),
            });
        }
        if self.proof_kind(eligibility_proof_id) != Some(ProofKind::RepairEligibility) {
            return Err(ProtocolViolation::MissingTransitionProof {
                required: ProofKind::RepairEligibility,
            });
        }
        if let Some(validation) = &self.validation {
            let failure_id = validation.active_failure.as_ref().ok_or(
                ProtocolViolation::ValidationContract {
                    code: "repair_node_without_active_failure",
                },
            )?;
            let selection = validation.selections.get(failure_id).ok_or(
                ProtocolViolation::ValidationContract {
                    code: "repair_node_without_authoritative_selection",
                },
            )?;
            let expected_proof = self.repair_eligibility_proof(selection)?;
            if node != &selection.repair_node
                || eligibility_proof_id != &expected_proof.id
                || self.proofs.get(eligibility_proof_id) != Some(&expected_proof)
            {
                return Err(ProtocolViolation::ValidationContract {
                    code: "repair_node_not_authoritative_selection",
                });
            }
        }
        let Some(current_failure_id) = self.latest_transition_proof.as_ref() else {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: eligibility_proof_id.clone(),
                code: "current_validation_failure_proof_missing",
            });
        };
        if self
            .proofs
            .get(eligibility_proof_id)
            .is_none_or(|eligibility| !eligibility.related_proof_ids.contains(current_failure_id))
        {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: eligibility_proof_id.clone(),
                code: "repair_eligibility_not_bound_to_current_failure",
            });
        }
        if node.kind != NodeKind::ValidationRepair {
            return Err(ProtocolViolation::InvalidGraph {
                code: "repair_node_kind_mismatch",
                node_id: Some(node.id.clone()),
            });
        }
        if !node.required || !node.dependencies.is_empty() {
            return Err(ProtocolViolation::InvalidGraph {
                code: "repair_node_must_be_required_and_independent",
                node_id: Some(node.id.clone()),
            });
        }
        if self.event_log.iter().any(|stored| {
            matches!(
                &stored.envelope.payload,
                DomainEvent::Graph(GraphEvent::ValidationRepairNodeAdded {
                    eligibility_proof_id: recorded_eligibility,
                    ..
                }) if self.proofs.get(recorded_eligibility).is_some_and(|eligibility| {
                    eligibility.related_proof_ids.contains(current_failure_id)
                })
            )
        }) {
            return Err(ProtocolViolation::InvalidGraph {
                code: "repair_node_already_selected_for_failure",
                node_id: Some(node.id.clone()),
            });
        }
        if self.nodes.contains_key(&node.id) {
            return Err(ProtocolViolation::DuplicateNode {
                node_id: node.id.clone(),
            });
        }
        self.validate_node_budget(node)?;
        for dependency in &node.dependencies {
            if !self.nodes.contains_key(dependency) {
                return Err(ProtocolViolation::InvalidGraph {
                    code: "unknown_dependency",
                    node_id: Some(node.id.clone()),
                });
            }
        }
        self.node_order.push(node.id.clone());
        self.nodes
            .insert(node.id.clone(), ExecutionNode::from(node.clone()));
        self.refresh_ready_nodes();
        Ok(())
    }

    fn validate_node_budget(&self, node: &NodeSpec) -> Result<(), ProtocolViolation> {
        if node.kind.requires_model()
            && (node.budget.max_model_calls == 0
                || node.budget.max_input_tokens_per_call == 0
                || node.budget.max_output_tokens_per_call == 0)
        {
            return Err(ProtocolViolation::InvalidGraph {
                code: "model_node_has_no_viable_budget",
                node_id: Some(node.id.clone()),
            });
        }
        Ok(())
    }

    fn start_node(&mut self, node_id: &NodeId, attempt: u32) -> Result<(), ProtocolViolation> {
        if let Some(active) = self.active_node() {
            return Err(ProtocolViolation::ActiveOwnerConflict {
                active_node_id: active.id.clone(),
                requested_node_id: node_id.clone(),
            });
        }
        let node = self
            .nodes
            .get(node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: node_id.clone(),
            })?;
        if node.kind == NodeKind::Discovery && self.repository_profile.is_some() {
            let discovery =
                self.discovery
                    .as_ref()
                    .ok_or(ProtocolViolation::DiscoveryContract {
                        code: "discovery_start_without_goal",
                    })?;
            if discovery.node_id != node.id
                || self
                    .repository_profile
                    .as_ref()
                    .is_none_or(|profile| profile.profile_id != discovery.repository_profile_id)
            {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "discovery_start_profile_binding_mismatch",
                });
            }
        }
        if node.kind == NodeKind::Planning
            && self.repository_profile.is_some()
            && self
                .planning
                .as_ref()
                .is_none_or(|planning| planning.node_id != node.id)
        {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_start_without_authoritative_state",
            });
        }
        if node.kind == NodeKind::Validation
            && self.validation.as_ref().is_some_and(|validation| {
                validation
                    .next_gate()
                    .is_none_or(|gate| gate.node_id != node.id)
            })
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_node_is_not_canonical_next_gate",
            });
        }
        if node.kind == NodeKind::ValidationRepair
            && self.validation.as_ref().is_some_and(|validation| {
                validation
                    .active_failure
                    .as_ref()
                    .and_then(|failure_id| validation.selections.get(failure_id))
                    .is_none_or(|selection| selection.repair_node.id != node.id)
            })
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "repair_node_is_not_authoritative_selection",
            });
        }
        if node.kind.stage() != self.stage() {
            return Err(ProtocolViolation::WrongPosition {
                node_id: node_id.clone(),
                position: self.stage(),
            });
        }
        if node.state != NodeState::Ready || attempt != node.attempts_started.saturating_add(1) {
            return Err(ProtocolViolation::InvalidNodeState {
                node_id: node_id.clone(),
                code: "node_started",
            });
        }
        self.ensure_dependencies_satisfied(node_id)?;
        let node = self.nodes.get_mut(node_id).expect("node was checked");
        node.attempts_started = attempt;
        node.state = NodeState::Active { attempt };
        Ok(())
    }

    fn wait_node(
        &mut self,
        node_id: &NodeId,
        effect_id: &EffectId,
    ) -> Result<(), ProtocolViolation> {
        if (self.review.is_some() || self.publication.is_some())
            && self.nodes.get(node_id).is_some_and(|node| {
                matches!(
                    node.kind,
                    NodeKind::Review | NodeKind::CompletionEvaluation | NodeKind::Publication
                )
            })
        {
            return Err(ProtocolViolation::ReviewContract {
                code: "phase7_graph_wait_unavailable",
            });
        }
        if self.validation.is_some()
            && self
                .nodes
                .get(node_id)
                .is_some_and(|node| node.kind == NodeKind::ValidationRepair)
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "repair_execution_effect_unavailable",
            });
        }
        if self.has_open_model_call_for_node(node_id) {
            return Err(ProtocolViolation::InvalidNodeState {
                node_id: node_id.clone(),
                code: "node_waiting_with_open_model_call",
            });
        }
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: node_id.clone(),
            })?;
        let NodeState::Active { attempt } = node.state else {
            return Err(ProtocolViolation::InvalidNodeState {
                node_id: node_id.clone(),
                code: "node_waiting",
            });
        };
        node.state = NodeState::Waiting {
            attempt,
            effect_id: effect_id.clone(),
        };
        Ok(())
    }

    fn resume_node(
        &mut self,
        node_id: &NodeId,
        effect_id: &EffectId,
    ) -> Result<(), ProtocolViolation> {
        if (self.review.is_some() || self.publication.is_some())
            && self.nodes.get(node_id).is_some_and(|node| {
                matches!(
                    node.kind,
                    NodeKind::Review | NodeKind::CompletionEvaluation | NodeKind::Publication
                )
            })
        {
            return Err(ProtocolViolation::ReviewContract {
                code: "phase7_graph_resume_unavailable",
            });
        }
        if self.validation.is_some()
            && self
                .nodes
                .get(node_id)
                .is_some_and(|node| node.kind == NodeKind::ValidationRepair)
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "repair_execution_effect_unavailable",
            });
        }
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: node_id.clone(),
            })?;
        let NodeState::Waiting {
            attempt,
            effect_id: active_effect_id,
        } = &node.state
        else {
            return Err(ProtocolViolation::InvalidNodeState {
                node_id: node_id.clone(),
                code: "node_resumed",
            });
        };
        if active_effect_id != effect_id {
            return Err(ProtocolViolation::InvalidNodeState {
                node_id: node_id.clone(),
                code: "effect_identity_mismatch",
            });
        }
        node.state = NodeState::Active { attempt: *attempt };
        Ok(())
    }

    fn succeed_node(
        &mut self,
        node_id: &NodeId,
        proof_id: &ProofId,
    ) -> Result<(), ProtocolViolation> {
        if self.has_open_model_call_for_node(node_id) {
            return Err(ProtocolViolation::InvalidNodeState {
                node_id: node_id.clone(),
                code: "node_succeeded_with_open_model_call",
            });
        }
        let proof = self
            .proofs
            .get(proof_id)
            .ok_or_else(|| ProtocolViolation::UnknownProof {
                proof_id: proof_id.clone(),
            })?;
        let node = self
            .nodes
            .get(node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: node_id.clone(),
            })?;
        if node.kind == NodeKind::Discovery && self.repository_profile.is_some() {
            let discovery =
                self.discovery
                    .as_ref()
                    .ok_or(ProtocolViolation::DiscoveryContract {
                        code: "discovery_state_missing",
                    })?;
            let impact_map_id = discovery.impact_map.as_ref().map(|map| &map.evidence_id);
            if self.current_discovery_action.is_some()
                || !matches!(
                    (&discovery.convergence, impact_map_id),
                    (
                        Some(DiscoveryConvergence::ImpactMapAccepted { evidence_id }),
                        Some(actual_id)
                    ) if evidence_id == actual_id
                )
            {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "discovery_success_without_accepted_impact_map",
                });
            }
        }
        if node.kind == NodeKind::Planning
            && let Some(planning) = &self.planning
        {
            let accepted_plan_graph_exists = planning.accepted_plan.as_ref().is_some_and(|plan| {
                self.event_log.iter().any(|stored| {
                    matches!(
                        &stored.envelope.payload,
                        DomainEvent::Graph(GraphEvent::NodesAdded { plan_proof_id, nodes })
                            if plan_proof_id == proof_id
                                && self.materialized_planning_nodes(plan).ok().as_ref() == Some(nodes)
                    )
                })
            });
            let accepted_no_op = planning.accepted_no_op.is_some()
                && !self
                    .nodes
                    .values()
                    .any(|candidate| is_planned_node(candidate.kind));
            if self.current_planning_action.is_some()
                || planning.convergence.is_some()
                || !(accepted_plan_graph_exists || accepted_no_op)
            {
                return Err(ProtocolViolation::PlanningContract {
                    code: "planning_success_without_accepted_projection",
                });
            }
        }
        if node.kind == NodeKind::Validation && self.validation.is_some() {
            let expected = self.validation_pass_proof(node_id).map_err(|_| {
                ProtocolViolation::InvalidProof {
                    proof_id: proof_id.clone(),
                    code: "validation_node_success_proof_not_current",
                }
            })?;
            if proof_id != &expected.id || proof != &expected {
                return Err(ProtocolViolation::InvalidProof {
                    proof_id: proof_id.clone(),
                    code: "validation_node_success_proof_not_current",
                });
            }
        }
        if node.kind == NodeKind::Review && self.review.is_some() {
            let expected = self.review_completion_proof()?;
            if proof_id != &expected.id || proof != &expected {
                return Err(ProtocolViolation::InvalidProof {
                    proof_id: proof_id.clone(),
                    code: "review_node_success_proof_not_current",
                });
            }
        }
        if node.kind == NodeKind::CompletionEvaluation && self.review.is_some() {
            let expected = self.completion_evaluation_proof()?;
            if proof_id != &expected.id || proof != &expected {
                return Err(ProtocolViolation::InvalidProof {
                    proof_id: proof_id.clone(),
                    code: "completion_node_success_proof_not_current",
                });
            }
        }
        if node.kind == NodeKind::Publication && self.publication.is_some() {
            let expected = self.publication_completion_proof()?;
            if proof_id != &expected.id || proof != &expected {
                return Err(ProtocolViolation::InvalidProof {
                    proof_id: proof_id.clone(),
                    code: "publication_node_success_proof_not_current",
                });
            }
        }
        if !matches!(&node.state, NodeState::Active { .. }) || !proof.node_ids.contains(node_id) {
            return Err(ProtocolViolation::InvalidNodeState {
                node_id: node_id.clone(),
                code: "node_succeeded",
            });
        }
        if !proof_satisfies_node(node.kind, proof.kind) {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof_id.clone(),
                code: "proof_kind_does_not_satisfy_node",
            });
        }
        self.nodes.get_mut(node_id).expect("node was checked").state = NodeState::Succeeded {
            proof_id: proof_id.clone(),
        };
        self.refresh_ready_nodes();
        Ok(())
    }

    fn fail_node(
        &mut self,
        node_id: &NodeId,
        failure_revision_id: &FailureRevisionId,
        terminal: bool,
    ) -> Result<(), ProtocolViolation> {
        if self.has_open_model_call_for_node(node_id) {
            return Err(ProtocolViolation::InvalidNodeState {
                node_id: node_id.clone(),
                code: "node_failed_with_open_model_call",
            });
        }
        if self.repository_profile.is_some()
            && self
                .nodes
                .get(node_id)
                .is_some_and(|node| node.kind == NodeKind::Discovery)
            && (!terminal
                || self.current_discovery_action.is_some()
                || !self.discovery.as_ref().is_some_and(|discovery| {
                    matches!(
                        discovery.convergence,
                        Some(
                            DiscoveryConvergence::InsufficientEvidence { .. }
                                | DiscoveryConvergence::BudgetBlocked { .. }
                        )
                    )
                }))
        {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "discovery_failure_without_blocking_convergence",
            });
        }
        if self.planning.is_some()
            && self
                .nodes
                .get(node_id)
                .is_some_and(|node| node.kind == NodeKind::Planning)
            && (!terminal
                || self.current_planning_action.is_some()
                || !self
                    .planning
                    .as_ref()
                    .is_some_and(|planning| planning.convergence.is_some()))
        {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_failure_without_blocking_convergence",
            });
        }
        if self
            .nodes
            .get(node_id)
            .is_some_and(|node| node.kind == NodeKind::Implementation)
        {
            let expected = self
                .mutation_terminal_failure_revision(node_id)
                .map_err(|_| ProtocolViolation::MutationContract {
                    code: "implementation_failure_without_exact_mutation_convergence",
                })?;
            if !terminal || expected != failure_revision_id {
                return Err(ProtocolViolation::MutationContract {
                    code: "implementation_failure_without_exact_mutation_convergence",
                });
            }
        }
        if self.validation.is_some()
            && self
                .nodes
                .get(node_id)
                .is_some_and(|node| node.kind == NodeKind::ValidationRepair)
        {
            let expected = self
                .mutation_terminal_failure_revision(node_id)
                .map_err(|_| ProtocolViolation::ValidationContract {
                    code: "repair_failure_without_exact_mutation_convergence",
                })?;
            if !terminal || expected != failure_revision_id {
                return Err(ProtocolViolation::ValidationContract {
                    code: "repair_failure_without_exact_mutation_convergence",
                });
            }
        }
        if self
            .nodes
            .get(node_id)
            .is_some_and(|node| node.kind == NodeKind::Validation)
            && let Some(validation) = &self.validation
        {
            let authorized = if terminal {
                validation.convergence.as_ref().is_some_and(|convergence| {
                    &convergence.failure_revision_id == failure_revision_id
                })
            } else {
                validation.current_failure().is_some_and(|failure| {
                    &failure.node_id == node_id
                        && &failure.failure_revision_id == failure_revision_id
                })
            };
            if !authorized {
                return Err(ProtocolViolation::ValidationContract {
                    code: "validation_failure_without_exact_convergence",
                });
            }
        }
        if self.review.is_some()
            && self.nodes.get(node_id).is_some_and(|node| {
                matches!(node.kind, NodeKind::Review | NodeKind::CompletionEvaluation)
            })
        {
            let convergence = self
                .review
                .as_ref()
                .and_then(|review| review.convergence.as_ref())
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "review_failure_without_exact_convergence",
                })?;
            let expected = review_convergence_failure_revision(convergence);
            if !terminal || failure_revision_id != &expected {
                return Err(ProtocolViolation::ReviewContract {
                    code: "review_failure_without_exact_convergence",
                });
            }
        }
        if self.publication.is_some()
            && self
                .nodes
                .get(node_id)
                .is_some_and(|node| node.kind == NodeKind::Publication)
        {
            let convergence = self
                .publication
                .as_ref()
                .and_then(|publication| publication.convergence.as_ref())
                .ok_or(ProtocolViolation::PublicationContract {
                    code: "publication_failure_without_exact_convergence",
                })?;
            let expected = publication_convergence_failure_revision(convergence);
            if !terminal || failure_revision_id != &expected {
                return Err(ProtocolViolation::PublicationContract {
                    code: "publication_failure_without_exact_convergence",
                });
            }
        }
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: node_id.clone(),
            })?;
        if !matches!(node.state, NodeState::Active { .. }) {
            return Err(ProtocolViolation::InvalidNodeState {
                node_id: node_id.clone(),
                code: "node_failed",
            });
        }
        node.state = if terminal {
            NodeState::FailedTerminal {
                failure_revision_id: failure_revision_id.clone(),
            }
        } else {
            NodeState::FailedRecoverable {
                failure_revision_id: failure_revision_id.clone(),
            }
        };
        Ok(())
    }

    fn admit_model_call(&mut self, admission: ModelCallAdmission) -> Result<(), ProtocolViolation> {
        if self.budgets.model_calls.contains_key(&admission.call_id) {
            return Err(ProtocolViolation::ModelCallLifecycle {
                call_id: admission.call_id,
                code: "call_identity_already_exists",
            });
        }
        let node =
            self.nodes
                .get(&admission.node_id)
                .ok_or_else(|| ProtocolViolation::UnknownNode {
                    node_id: admission.node_id.clone(),
                })?;
        if node.kind == NodeKind::Discovery && self.repository_profile.is_some() {
            let prepared = self.current_discovery_action.as_ref().ok_or(
                ProtocolViolation::DiscoveryContract {
                    code: "discovery_call_without_prepared_action",
                },
            )?;
            if prepared.admission != admission {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "discovery_call_does_not_match_prepared_admission",
                });
            }
        }
        if node.kind == NodeKind::Planning && self.planning.is_some() {
            let prepared = self.current_planning_action.as_ref().ok_or(
                ProtocolViolation::PlanningContract {
                    code: "planning_call_without_prepared_action",
                },
            )?;
            if prepared.admission != admission {
                return Err(ProtocolViolation::PlanningContract {
                    code: "planning_call_does_not_match_prepared_admission",
                });
            }
        }
        if matches!(
            node.kind,
            NodeKind::Implementation | NodeKind::ValidationRepair
        ) && (self.implementation.is_some() || self.validation.is_some())
        {
            let prepared = self
                .event_log
                .iter()
                .rev()
                .find_map(|stored| match &stored.envelope.payload {
                    DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared })
                        if prepared.admission.call_id == admission.call_id =>
                    {
                        Some(&**prepared)
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    if node.kind == NodeKind::ValidationRepair {
                        ProtocolViolation::ValidationContract {
                            code: "repair_model_call_without_authoritative_action",
                        }
                    } else {
                        ProtocolViolation::MutationContract {
                            code: "mutation_call_without_prepared_action",
                        }
                    }
                })?;
            if prepared.admission != admission {
                return Err(ProtocolViolation::MutationContract {
                    code: "mutation_call_does_not_match_prepared_admission",
                });
            }
        }
        if matches!(node.kind, NodeKind::Review | NodeKind::CompletionEvaluation)
            && let Some(review) = &self.review
        {
            let prepared = review
                .current_action()
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "review_call_without_prepared_action",
                })?;
            if prepared.admission != admission {
                return Err(ProtocolViolation::ReviewContract {
                    code: "review_call_does_not_match_prepared_admission",
                });
            }
        }
        if !matches!(node.state, NodeState::Active { .. }) || !node.kind.requires_model() {
            return Err(ProtocolViolation::InvalidNodeState {
                node_id: admission.node_id,
                code: "model_call_admission",
            });
        }
        if admission.payload_hash.trim().is_empty() {
            return Err(ProtocolViolation::ModelCallLifecycle {
                call_id: admission.call_id,
                code: "payload_hash_missing",
            });
        }
        if admission.input_tokens > node.budget.max_input_tokens_per_call {
            return Err(ProtocolViolation::BudgetExceeded {
                node_id: Some(node.id.clone()),
                dimension: "input_tokens_per_call",
            });
        }
        if admission.output_tokens > node.budget.max_output_tokens_per_call {
            return Err(ProtocolViolation::BudgetExceeded {
                node_id: Some(node.id.clone()),
                dimension: "output_tokens_per_call",
            });
        }
        if self.has_open_model_call_for_node(&node.id) {
            return Err(ProtocolViolation::ModelCallLifecycle {
                call_id: admission.call_id,
                code: "node_already_has_open_call",
            });
        }
        self.ensure_model_call_capacity(node, &admission)?;
        self.budgets.model_calls.insert(
            admission.call_id.clone(),
            ModelCallRecord {
                admission,
                state: ModelCallState::Admitted,
            },
        );
        Ok(())
    }

    fn reserve_model_call(&mut self, call_id: &ModelCallId) -> Result<(), ProtocolViolation> {
        let record = self
            .budgets
            .model_calls
            .get(call_id)
            .ok_or_else(|| ProtocolViolation::ModelCallLifecycle {
                call_id: call_id.clone(),
                code: "reservation_without_admission",
            })?
            .clone();
        if record.state != ModelCallState::Admitted {
            return Err(ProtocolViolation::ModelCallLifecycle {
                call_id: call_id.clone(),
                code: "reservation_not_admitted",
            });
        }
        let node = self.nodes.get(&record.admission.node_id).ok_or_else(|| {
            ProtocolViolation::UnknownNode {
                node_id: record.admission.node_id.clone(),
            }
        })?;
        if !matches!(node.state, NodeState::Active { .. }) {
            return Err(ProtocolViolation::InvalidNodeState {
                node_id: node.id.clone(),
                code: "reservation_owner_not_active",
            });
        }
        self.ensure_model_call_capacity(node, &record.admission)?;
        let node = self
            .nodes
            .get_mut(&record.admission.node_id)
            .expect("reservation node was checked");
        reserve_usage(&mut node.usage, &record.admission);
        reserve_usage(&mut self.budgets.mission_usage, &record.admission);
        self.budgets
            .model_calls
            .get_mut(call_id)
            .expect("model call was checked")
            .state = ModelCallState::Reserved;
        Ok(())
    }

    fn start_provider_dispatch(
        &mut self,
        call_id: &ModelCallId,
        payload_hash: &str,
    ) -> Result<(), ProtocolViolation> {
        if let Some(prepared) = &self.current_discovery_action
            && &prepared.admission.call_id == call_id
            && (prepared.envelope.payload_identity != payload_hash
                || prepared.admission.payload_hash != payload_hash)
        {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "serialized_discovery_payload_mismatch",
            });
        }
        if let Some(prepared) = &self.current_planning_action
            && &prepared.admission.call_id == call_id
            && (prepared.envelope.payload_identity != payload_hash
                || prepared.admission.payload_hash != payload_hash)
        {
            return Err(ProtocolViolation::PlanningContract {
                code: "serialized_planning_payload_mismatch",
            });
        }
        let mutation_action =
            self.event_log
                .iter()
                .rev()
                .find_map(|stored| match &stored.envelope.payload {
                    DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared })
                        if &prepared.admission.call_id == call_id =>
                    {
                        Some((**prepared).clone())
                    }
                    _ => None,
                });
        if let Some(prepared) = &mutation_action
            && (prepared.provider_request.payload_hash()? != payload_hash
                || prepared.admission.payload_hash != payload_hash
                || prepared.provider_request.call_id != *call_id)
        {
            return Err(ProtocolViolation::MutationContract {
                code: "serialized_mutation_payload_mismatch",
            });
        }
        if let Some(prepared) = self
            .review
            .as_ref()
            .and_then(ReviewStateV1::current_action)
            .filter(|prepared| &prepared.admission.call_id == call_id)
            && (prepared.envelope.payload_identity != payload_hash
                || prepared.admission.payload_hash != payload_hash
                || &prepared.envelope.call_id != call_id)
        {
            return Err(ProtocolViolation::ReviewContract {
                code: "serialized_review_payload_mismatch",
            });
        }
        let record = self.budgets.model_calls.get_mut(call_id).ok_or_else(|| {
            ProtocolViolation::ModelCallLifecycle {
                call_id: call_id.clone(),
                code: "dispatch_without_reservation",
            }
        })?;
        if record.state != ModelCallState::Reserved {
            return Err(ProtocolViolation::ModelCallLifecycle {
                call_id: call_id.clone(),
                code: "dispatch_not_reserved",
            });
        }
        if record.admission.payload_hash != payload_hash {
            return Err(ProtocolViolation::ModelCallLifecycle {
                call_id: call_id.clone(),
                code: "dispatch_payload_hash_mismatch",
            });
        }
        record.state = ModelCallState::Dispatched;
        Ok(())
    }

    fn reconcile_model_call(
        &mut self,
        call_id: &ModelCallId,
        result: &ModelCallReconciliation,
    ) -> Result<(), ProtocolViolation> {
        let record = self
            .budgets
            .model_calls
            .get(call_id)
            .ok_or_else(|| ProtocolViolation::ModelCallLifecycle {
                call_id: call_id.clone(),
                code: "reconciliation_without_admission",
            })?
            .clone();
        if !record.state.owns_reservation() {
            return Err(ProtocolViolation::ModelCallLifecycle {
                call_id: call_id.clone(),
                code: "reservation_already_reconciled",
            });
        }
        match result {
            ModelCallReconciliation::Consumed {
                actual_cost_micros,
                duration_ms,
            } => {
                if record.state != ModelCallState::Dispatched {
                    return Err(ProtocolViolation::ModelCallLifecycle {
                        call_id: call_id.clone(),
                        code: "consumed_call_was_not_dispatched",
                    });
                }
                if *actual_cost_micros > record.admission.reserved_cost_micros
                    || *duration_ms > record.admission.duration_allowance_ms
                {
                    return Err(ProtocolViolation::ModelCallLifecycle {
                        call_id: call_id.clone(),
                        code: "actual_usage_exceeds_reservation",
                    });
                }
                let mutation_attempt = self.event_log.iter().rev().find_map(|stored| {
                    let DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared }) =
                        &stored.envelope.payload
                    else {
                        return None;
                    };
                    (prepared.admission.call_id == *call_id).then(|| {
                        (
                            prepared.policy.node_id.clone(),
                            prepared.policy.attempt_id.clone(),
                        )
                    })
                });
                if let Some((_, attempt_id)) = &mutation_attempt
                    && self.budgets.model_calls.values().any(|existing| {
                        matches!(existing.state, ModelCallState::ReconciledConsumed { .. })
                            && self.event_log.iter().any(|stored| {
                                matches!(
                                    &stored.envelope.payload,
                                    DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared })
                                        if prepared.admission.call_id
                                            == existing.admission.call_id
                                            && prepared.policy.attempt_id == *attempt_id
                                )
                            })
                    })
                {
                    return Err(ProtocolViolation::ModelCallLifecycle {
                        call_id: call_id.clone(),
                        code: "mutation_attempt_already_consumed",
                    });
                }
                if let Some((node_id, _)) = &mutation_attempt {
                    let node =
                        self.nodes
                            .get(node_id)
                            .ok_or_else(|| ProtocolViolation::UnknownNode {
                                node_id: node_id.clone(),
                            })?;
                    if node.usage.mutation_attempts.saturating_add(1)
                        > node.budget.max_mutation_attempts
                    {
                        return Err(ProtocolViolation::BudgetExceeded {
                            node_id: Some(node_id.clone()),
                            dimension: "mutation_attempts",
                        });
                    }
                }
                let node = self
                    .nodes
                    .get_mut(&record.admission.node_id)
                    .expect("reservation owner exists");
                consume_usage(
                    &mut node.usage,
                    &record.admission,
                    *actual_cost_micros,
                    *duration_ms,
                );
                consume_usage(
                    &mut self.budgets.mission_usage,
                    &record.admission,
                    *actual_cost_micros,
                    *duration_ms,
                );
                self.budgets
                    .model_calls
                    .get_mut(call_id)
                    .expect("model call exists")
                    .state = ModelCallState::ReconciledConsumed {
                    actual_cost_micros: *actual_cost_micros,
                    duration_ms: *duration_ms,
                };
                if let Some((node_id, _)) = mutation_attempt {
                    let usage = &mut self
                        .nodes
                        .get_mut(&node_id)
                        .expect("mutation budget owner was checked")
                        .usage;
                    usage.mutation_attempts = usage.mutation_attempts.saturating_add(1);
                    self.budgets.mission_usage.mutation_attempts = self
                        .budgets
                        .mission_usage
                        .mutation_attempts
                        .saturating_add(1);
                }
            }
            ModelCallReconciliation::ReleasedUncontacted => {
                let node = self
                    .nodes
                    .get_mut(&record.admission.node_id)
                    .expect("reservation owner exists");
                release_usage(&mut node.usage, &record.admission);
                release_usage(&mut self.budgets.mission_usage, &record.admission);
                self.budgets
                    .model_calls
                    .get_mut(call_id)
                    .expect("model call exists")
                    .state = ModelCallState::ReconciledReleased;
            }
        }
        Ok(())
    }

    fn ensure_model_call_capacity(
        &self,
        node: &ExecutionNode,
        admission: &ModelCallAdmission,
    ) -> Result<(), ProtocolViolation> {
        ensure_usage_capacity(&node.usage, &node.budget, admission, Some(node.id.clone()))?;
        if self
            .budgets
            .mission_usage
            .model_calls_consumed
            .saturating_add(self.budgets.mission_usage.model_calls_reserved)
            .saturating_add(1)
            > self.mission_budget.max_model_calls
        {
            return Err(ProtocolViolation::BudgetExceeded {
                node_id: None,
                dimension: "model_calls",
            });
        }
        if self
            .budgets
            .mission_usage
            .cost_micros_consumed
            .saturating_add(self.budgets.mission_usage.cost_micros_reserved)
            .saturating_add(admission.reserved_cost_micros)
            > self.mission_budget.max_cost_micros
        {
            return Err(ProtocolViolation::BudgetExceeded {
                node_id: None,
                dimension: "cost_micros",
            });
        }
        if self
            .budgets
            .mission_usage
            .duration_ms_consumed
            .saturating_add(self.budgets.mission_usage.duration_ms_reserved)
            .saturating_add(admission.duration_allowance_ms)
            > self.mission_budget.max_duration_ms
        {
            return Err(ProtocolViolation::BudgetExceeded {
                node_id: None,
                dimension: "duration_ms",
            });
        }
        Ok(())
    }

    fn advance_position(
        &mut self,
        from: ProtocolStage,
        to: ProtocolStage,
        proof_id: &ProofId,
    ) -> Result<(), ProtocolViolation> {
        if self.stage() != from {
            return Err(ProtocolViolation::IllegalTransition {
                from: self.stage(),
                to,
            });
        }
        if self.repository_profile.is_some() {
            match (from, to) {
                (ProtocolStage::Profiling, ProtocolStage::Discovery) => {
                    let discovery =
                        self.discovery
                            .as_ref()
                            .ok_or(ProtocolViolation::DiscoveryContract {
                                code: "discovery_transition_without_goal",
                            })?;
                    if self
                        .repository_profile
                        .as_ref()
                        .is_none_or(|profile| profile.profile_id != discovery.repository_profile_id)
                    {
                        return Err(ProtocolViolation::DiscoveryContract {
                            code: "discovery_transition_profile_binding_mismatch",
                        });
                    }
                }
                (ProtocolStage::Discovery, ProtocolStage::Planning)
                    if self.current_discovery_action.is_some()
                        || !self.discovery.as_ref().is_some_and(|discovery| {
                            matches!(
                                &discovery.convergence,
                                Some(DiscoveryConvergence::ImpactMapAccepted { .. })
                            )
                        }) =>
                {
                    return Err(ProtocolViolation::DiscoveryContract {
                        code: "planning_transition_without_discovery_convergence",
                    });
                }
                _ => {}
            }
        }
        if self.active_node().is_some() || self.has_open_model_call() {
            return Err(ProtocolViolation::Invariant {
                code: "transition_with_active_owner",
                detail: format!("cannot leave {from:?} while work remains active"),
            });
        }
        if self.repository_profile.is_some()
            && from == ProtocolStage::Planning
            && to == ProtocolStage::Implementation
        {
            let planning = self
                .planning
                .as_ref()
                .ok_or(ProtocolViolation::PlanningContract {
                    code: "planning_transition_without_state",
                })?;
            if self.current_planning_action.is_some()
                || planning.accepted_plan.is_none()
                || planning.accepted_no_op.is_some()
                || planning.convergence.is_some()
            {
                return Err(ProtocolViolation::PlanningContract {
                    code: "implementation_transition_without_accepted_plan",
                });
            }
        }
        let required =
            transition_proof(from, to).ok_or(ProtocolViolation::IllegalTransition { from, to })?;
        if self.proof_kind(proof_id) != Some(required) {
            return Err(ProtocolViolation::MissingTransitionProof { required });
        }
        self.validate_transition_completion(from, to, proof_id)?;
        if let Some(profile) = self.repository_profile.as_ref()
            && from == ProtocolStage::Discovery
            && to == ProtocolStage::Planning
        {
            if self.planning.is_some() || self.current_planning_action.is_some() {
                return Err(ProtocolViolation::PlanningContract {
                    code: "planning_state_already_initialized",
                });
            }
            let discovery = self
                .discovery
                .as_ref()
                .ok_or(ProtocolViolation::PlanningContract {
                    code: "planning_discovery_state_missing",
                })?;
            self.planning = Some(PlanningState::new(
                NodeId::new("protocol-v1:planning"),
                profile,
                discovery,
            )?);
        }
        if self.repository_profile.is_some()
            && from == ProtocolStage::Planning
            && to == ProtocolStage::Implementation
        {
            if self.implementation.is_some() {
                return Err(ProtocolViolation::ImplementationContract {
                    code: "implementation_state_already_initialized",
                });
            }
            let plan = self
                .planning
                .as_ref()
                .and_then(|planning| planning.accepted_plan.as_ref())
                .ok_or(ProtocolViolation::ImplementationContract {
                    code: "implementation_accepted_plan_missing",
                })?;
            self.implementation = Some(ImplementationState::new(plan)?);
        }
        if from == ProtocolStage::Implementation
            && to == ProtocolStage::Validation
            && let Some(policy) = self.validation_policy.as_ref()
        {
            if self.validation.is_some() {
                return Err(ProtocolViolation::ValidationContract {
                    code: "validation_state_already_initialized",
                });
            }
            let profile =
                self.repository_profile
                    .as_ref()
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "validation_repository_profile_missing",
                    })?;
            let plan = self
                .planning
                .as_ref()
                .and_then(|planning| planning.accepted_plan.as_ref())
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "validation_accepted_plan_missing",
                })?;
            policy.validate(profile)?;
            let graph = materialize_accepted_plan(plan, &self.plan_graph_budget)?;
            let gates =
                build_validation_gates(plan, &graph, profile, policy, &self.repository_revision)?;
            self.validation = Some(ValidationState::new(gates, policy, plan)?);
        }
        if from == ProtocolStage::Validation && to == ProtocolStage::Repair {
            for node in self
                .nodes
                .values_mut()
                .filter(|node| node.kind == NodeKind::Validation && node.state == NodeState::Ready)
            {
                node.state = NodeState::Pending;
            }
        }
        if from == ProtocolStage::Validation
            && to == ProtocolStage::Review
            && let Some(policy) = self.finalization_policy.as_ref().cloned()
        {
            if self.review.is_some() {
                return Err(ProtocolViolation::ReviewContract {
                    code: "review_state_already_initialized",
                });
            }
            policy.validate()?;
            let plan = self
                .planning
                .as_ref()
                .and_then(|planning| planning.accepted_plan.as_ref())
                .ok_or(ProtocolViolation::ReviewContract {
                    code: "review_accepted_plan_missing",
                })?;
            let review_node_id = self.single_required_node_id(NodeKind::Review)?;
            let completion_node_id =
                self.single_required_node_id(NodeKind::CompletionEvaluation)?;
            let ancestry = self.engineering_ancestry()?;
            self.review = Some(ReviewStateV1::new(
                plan,
                &policy,
                ancestry,
                review_node_id,
                completion_node_id,
            )?);
        }
        if from == ProtocolStage::Review
            && to == ProtocolStage::Publication
            && let Some(policy) = self.finalization_policy.as_ref().cloned()
        {
            if self.publication.is_some() {
                return Err(ProtocolViolation::PublicationContract {
                    code: "publication_state_already_initialized",
                });
            }
            let eligibility = self
                .review
                .as_ref()
                .and_then(|review| review.eligibility.as_deref())
                .ok_or(ProtocolViolation::PublicationContract {
                    code: "publication_eligibility_missing",
                })?;
            let publication_node_id = self.single_required_node_id(NodeKind::Publication)?;
            self.publication = Some(PublicationStateV1::new(
                self.execution_id.clone(),
                publication_node_id,
                &policy.publication,
                eligibility,
            )?);
        }
        self.position = ProtocolPosition::initial(to);
        self.latest_transition_proof = Some(proof_id.clone());
        self.refresh_ready_nodes();
        self.refresh_implementation_step();
        self.refresh_validation_step();
        self.refresh_review_step();
        self.refresh_publication_step();
        Ok(())
    }

    fn validate_transition_completion(
        &self,
        from: ProtocolStage,
        to: ProtocolStage,
        proof_id: &ProofId,
    ) -> Result<(), ProtocolViolation> {
        let expected_node = match (from, to) {
            (ProtocolStage::Discovery, ProtocolStage::Planning) => Some(NodeKind::Discovery),
            (ProtocolStage::Planning, ProtocolStage::Implementation) => Some(NodeKind::Planning),
            (ProtocolStage::Validation, ProtocolStage::Repair) if self.validation.is_some() => {
                let failure = self
                    .validation
                    .as_ref()
                    .and_then(ValidationState::current_failure)
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "validation_transition_failure_missing",
                    })?;
                let expected = self.validation_failure_proof(failure)?;
                if proof_id != &expected.id || self.proofs.get(proof_id) != Some(&expected) {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof_id.clone(),
                        code: "validation_failure_transition_proof_mismatch",
                    });
                }
                None
            }
            (ProtocolStage::Repair, ProtocolStage::Validation) if self.validation.is_some() => {
                if !self
                    .required_nodes(NodeKind::ValidationRepair)
                    .into_iter()
                    .all(|node| matches!(node.state, NodeState::Succeeded { .. }))
                {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof_id.clone(),
                        code: "repair_node_not_completed",
                    });
                }
                let rerun = self
                    .validation
                    .as_ref()
                    .and_then(|validation| validation.pending_rerun.as_ref())
                    .ok_or(ProtocolViolation::ValidationContract {
                        code: "repair_transition_rerun_missing",
                    })?;
                let expected = self.validation_rerun_proof(rerun)?;
                if proof_id != &expected.id || self.proofs.get(proof_id) != Some(&expected) {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof_id.clone(),
                        code: "validation_rerun_transition_proof_mismatch",
                    });
                }
                None
            }
            (ProtocolStage::Validation, ProtocolStage::Review) if self.validation.is_some() => {
                let expected = self.required_validation_proof()?;
                if proof_id != &expected.id || self.proofs.get(proof_id) != Some(&expected) {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof_id.clone(),
                        code: "required_validation_transition_proof_mismatch",
                    });
                }
                None
            }
            (ProtocolStage::Review, ProtocolStage::Publication) if self.review.is_some() => {
                let expected = self.publication_eligibility_proof()?;
                if proof_id != &expected.id || self.proofs.get(proof_id) != Some(&expected) {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof_id.clone(),
                        code: "publication_eligibility_transition_proof_mismatch",
                    });
                }
                None
            }
            _ => None,
        };
        if let Some(expected_node) = expected_node
            && self
                .required_nodes(expected_node)
                .into_iter()
                .any(|node| !matches!(node.state, NodeState::Succeeded { .. }))
        {
            return Err(ProtocolViolation::InvalidProof {
                proof_id: proof_id.clone(),
                code: "phase_owner_node_not_completed",
            });
        }
        Ok(())
    }

    fn record_canonical_result(
        &mut self,
        result: CanonicalResult,
    ) -> Result<(), ProtocolViolation> {
        if self.terminal.is_some() {
            return Err(ProtocolViolation::TerminalImmutable);
        }
        if self.active_node().is_some() || self.has_open_model_call() {
            return Err(ProtocolViolation::TerminalPredicate {
                code: "active_work_or_reservation",
            });
        }
        let source_stage = self.stage();
        self.validate_canonical_result(&result, source_stage)?;
        self.position = ProtocolPosition::Terminal;
        self.terminal = Some(result);
        Ok(())
    }

    fn validate_canonical_result(
        &self,
        result: &CanonicalResult,
        source_stage: ProtocolStage,
    ) -> Result<(), ProtocolViolation> {
        if result.repository_revision != self.repository_revision {
            return Err(ProtocolViolation::TerminalPredicate {
                code: "repository_revision_mismatch",
            });
        }
        if result.reason_code.trim().is_empty() {
            return Err(ProtocolViolation::TerminalPredicate {
                code: "reason_code_missing",
            });
        }
        let unresolved = self
            .unresolved_required_nodes()
            .into_iter()
            .collect::<Vec<_>>();
        if result.remaining_work != unresolved {
            return Err(ProtocolViolation::TerminalPredicate {
                code: "remaining_work_is_not_canonical",
            });
        }
        self.validate_terminal_mission(result, source_stage)
    }

    fn mutation_terminal_failure_revision(
        &self,
        node_id: &NodeId,
    ) -> Result<&FailureRevisionId, ProtocolViolation> {
        let target =
            self.mutation
                .current_target(node_id)
                .ok_or(ProtocolViolation::MutationContract {
                    code: "implementation_failure_without_mutation_convergence",
                })?;
        match (&target.readiness_convergence, &target.convergence) {
            (Some(readiness), None) => Ok(&readiness.failure_revision_id),
            (None, Some(convergence)) => Ok(&convergence.last_failure_revision_id),
            _ => Err(ProtocolViolation::MutationContract {
                code: "implementation_failure_without_exact_mutation_convergence",
            }),
        }
    }

    fn mutation_terminal_classification(
        &self,
        target: &TargetMutationState,
    ) -> Result<(MutationTerminalDisposition, &'static str), ProtocolViolation> {
        if let Some(readiness) = &target.readiness_convergence {
            return Ok(match &readiness.reason {
                MutationReadinessConvergenceReason::NoFeasibleStrategy => (
                    MutationTerminalDisposition::BlockedNoDiff,
                    "mutation_no_feasible_strategy",
                ),
                MutationReadinessConvergenceReason::AdmissionBudgetExhausted { .. } => (
                    MutationTerminalDisposition::BudgetBlocked,
                    "mutation_admission_budget_exhausted",
                ),
                MutationReadinessConvergenceReason::UncontactedActionRetryExhausted { .. } => (
                    MutationTerminalDisposition::InfrastructureFailed,
                    "mutation_uncontacted_action_retry_exhausted",
                ),
            });
        }
        let convergence =
            target
                .convergence
                .as_ref()
                .ok_or(ProtocolViolation::MutationContract {
                    code: "mutation_failure_without_mutation_convergence",
                })?;
        Ok(match convergence.reason {
            MutationConvergenceReason::MutationAttemptBudgetExhausted => (
                MutationTerminalDisposition::BudgetBlocked,
                "mutation_attempt_budget_exhausted",
            ),
            MutationConvergenceReason::ContextRebuildBudgetExhausted => (
                MutationTerminalDisposition::BudgetBlocked,
                "mutation_context_rebuild_budget_exhausted",
            ),
            MutationConvergenceReason::ContextRebuildUnavailable => (
                MutationTerminalDisposition::BlockedNoDiff,
                "mutation_context_rebuild_unavailable",
            ),
            MutationConvergenceReason::NoSafeFallback => {
                let failure = self.mutation_failure(&convergence.final_attempt_id)?;
                if failure.failure_revision_id != convergence.last_failure_revision_id {
                    return Err(ProtocolViolation::MutationContract {
                        code: "mutation_terminal_failure_binding_mismatch",
                    });
                }
                if failure.class == MutationFailureClass::ProviderProtocol {
                    (
                        MutationTerminalDisposition::InfrastructureFailed,
                        "mutation_provider_protocol_failure",
                    )
                } else if failure.detail_code == MutationFailureDetailCode::ArtifactNotDurable {
                    (
                        MutationTerminalDisposition::InfrastructureFailed,
                        "mutation_artifact_not_durable",
                    )
                } else {
                    (
                        MutationTerminalDisposition::BlockedNoDiff,
                        "mutation_no_safe_fallback",
                    )
                }
            }
        })
    }

    fn authoritative_mutation_terminal_result(
        &self,
    ) -> Result<Option<CanonicalResult>, ProtocolViolation> {
        let mut failed = self.nodes.values().filter(|node| {
            node.kind == NodeKind::Implementation
                && matches!(node.state, NodeState::FailedTerminal { .. })
        });
        let Some(node) = failed.next() else {
            return Ok(None);
        };
        if failed.next().is_some() {
            return Err(ProtocolViolation::MutationContract {
                code: "multiple_terminal_implementation_failures",
            });
        }
        let NodeState::FailedTerminal {
            failure_revision_id,
        } = &node.state
        else {
            unreachable!("failed implementation node was selected")
        };
        if self.mutation_terminal_failure_revision(&node.id)? != failure_revision_id {
            return Err(ProtocolViolation::MutationContract {
                code: "implementation_failure_revision_not_authoritative",
            });
        }
        let target = self
            .mutation
            .current_target(&node.id)
            .expect("terminal mutation target was checked");
        let (disposition, code) = self.mutation_terminal_classification(target)?;
        let blocker = FirstFatalBlocker {
            category: match disposition {
                MutationTerminalDisposition::BlockedNoDiff => "mutation",
                MutationTerminalDisposition::BudgetBlocked => "budget",
                MutationTerminalDisposition::InfrastructureFailed => "infrastructure",
            }
            .into(),
            code: code.into(),
            node_id: Some(node.id.clone()),
        };
        let mission = match disposition {
            MutationTerminalDisposition::BlockedNoDiff => {
                MissionResult::BlockedNoDiff { failure: blocker }
            }
            MutationTerminalDisposition::BudgetBlocked => MissionResult::BudgetBlocked {
                node_id: node.id.clone(),
                failure: blocker,
            },
            MutationTerminalDisposition::InfrastructureFailed => {
                MissionResult::InfrastructureFailed { failure: blocker }
            }
        };
        let process_health = match disposition {
            MutationTerminalDisposition::InfrastructureFailed => {
                ProcessHealth::Failed { code: code.into() }
            }
            MutationTerminalDisposition::BlockedNoDiff
            | MutationTerminalDisposition::BudgetBlocked => ProcessHealth::Healthy,
        };
        Ok(Some(CanonicalResult {
            mission,
            process_health,
            reason_code: code.into(),
            repository_revision: self.repository_revision.clone(),
            remaining_work: self.unresolved_required_nodes().into_iter().collect(),
        }))
    }

    fn authoritative_repair_mutation_terminal_result(
        &self,
    ) -> Result<Option<CanonicalResult>, ProtocolViolation> {
        let mut failed = self.nodes.values().filter(|node| {
            node.kind == NodeKind::ValidationRepair
                && matches!(node.state, NodeState::FailedTerminal { .. })
        });
        let Some(node) = failed.next() else {
            return Ok(None);
        };
        if failed.next().is_some() {
            return Err(ProtocolViolation::ValidationContract {
                code: "multiple_terminal_repair_failures",
            });
        }
        let NodeState::FailedTerminal {
            failure_revision_id,
        } = &node.state
        else {
            unreachable!("failed repair node was selected")
        };
        if self.mutation_terminal_failure_revision(&node.id)? != failure_revision_id {
            return Err(ProtocolViolation::ValidationContract {
                code: "repair_failure_revision_not_authoritative",
            });
        }
        let target = self
            .mutation
            .current_target(&node.id)
            .expect("terminal repair mutation target was checked");
        let (disposition, mutation_code) = self.mutation_terminal_classification(target)?;
        let (mission, process_health, reason_code) = match disposition {
            MutationTerminalDisposition::BlockedNoDiff => {
                let code = match mutation_code {
                    "mutation_no_feasible_strategy" => "repair_no_feasible_strategy",
                    "mutation_context_rebuild_unavailable" => "repair_context_rebuild_unavailable",
                    _ => "repair_no_safe_fallback",
                };
                let blocker = FirstFatalBlocker {
                    category: "validation".into(),
                    code: code.into(),
                    node_id: Some(node.id.clone()),
                };
                (
                    MissionResult::NoValidRepair { failure: blocker },
                    ProcessHealth::Healthy,
                    code,
                )
            }
            MutationTerminalDisposition::BudgetBlocked => {
                let code = match mutation_code {
                    "mutation_admission_budget_exhausted" => "repair_admission_budget_exhausted",
                    "mutation_attempt_budget_exhausted" => {
                        "repair_mutation_attempt_budget_exhausted"
                    }
                    "mutation_context_rebuild_budget_exhausted" => {
                        "repair_context_rebuild_budget_exhausted"
                    }
                    _ => "repair_budget_exhausted",
                };
                let blocker = FirstFatalBlocker {
                    category: "budget".into(),
                    code: code.into(),
                    node_id: Some(node.id.clone()),
                };
                (
                    MissionResult::BudgetBlocked {
                        node_id: node.id.clone(),
                        failure: blocker,
                    },
                    ProcessHealth::Healthy,
                    code,
                )
            }
            MutationTerminalDisposition::InfrastructureFailed => {
                let code = match mutation_code {
                    "mutation_uncontacted_action_retry_exhausted" => {
                        "repair_uncontacted_action_retry_exhausted"
                    }
                    "mutation_provider_protocol_failure" => "repair_provider_protocol_failure",
                    "mutation_artifact_not_durable" => "repair_artifact_not_durable",
                    _ => "repair_infrastructure_failure",
                };
                let blocker = FirstFatalBlocker {
                    category: "infrastructure".into(),
                    code: code.into(),
                    node_id: Some(node.id.clone()),
                };
                (
                    MissionResult::InfrastructureFailed { failure: blocker },
                    ProcessHealth::Failed { code: code.into() },
                    code,
                )
            }
        };
        Ok(Some(CanonicalResult {
            mission,
            process_health,
            reason_code: reason_code.into(),
            repository_revision: self.repository_revision.clone(),
            remaining_work: self.unresolved_required_nodes().into_iter().collect(),
        }))
    }

    fn authoritative_review_terminal_result(&self) -> Result<CanonicalResult, ProtocolViolation> {
        let review = self
            .review
            .as_ref()
            .ok_or(ProtocolViolation::ReviewContract {
                code: "review_state_missing",
            })?;
        let convergence = review
            .convergence
            .as_ref()
            .ok_or(ProtocolViolation::ReviewContract {
                code: "review_terminal_without_convergence",
            })?;
        convergence.validate()?;
        if convergence.repository_revision != self.repository_revision
            && !self.pending_review_drift_revision_adoption()?
        {
            return Err(ProtocolViolation::ReviewContract {
                code: "review_terminal_revision_mismatch",
            });
        }
        let failure_revision_id = review_convergence_failure_revision(convergence);
        let owner_node_id = match &convergence.reason {
            ReviewConvergenceReasonV1::ReviewBudgetExhausted { node_id }
            | ReviewConvergenceReasonV1::CompletionBudgetExhausted { node_id }
            | ReviewConvergenceReasonV1::ProviderProtocolExhausted { node_id }
            | ReviewConvergenceReasonV1::UncontactedReleaseRetryExhausted { node_id, .. } => {
                Some(node_id.clone())
            }
            ReviewConvergenceReasonV1::DiffManifestLimitExceeded { .. }
            | ReviewConvergenceReasonV1::ArtifactDurabilityFailed { .. }
            | ReviewConvergenceReasonV1::DiffReviewBlocked { .. } => {
                Some(review.review_node_id.clone())
            }
            ReviewConvergenceReasonV1::CompletionIncomplete { .. } => {
                Some(review.completion_node_id.clone())
            }
            ReviewConvergenceReasonV1::RepositoryDrift { .. }
            | ReviewConvergenceReasonV1::PublicationAuthorityUnavailable { .. }
            | ReviewConvergenceReasonV1::PublicationEligibilityDenied { .. } => None,
        };
        if let Some(node_id) = owner_node_id.as_ref() {
            let node = self
                .nodes
                .get(node_id)
                .ok_or_else(|| ProtocolViolation::UnknownNode {
                    node_id: node_id.clone(),
                })?;
            if !matches!(
                &node.state,
                NodeState::FailedTerminal {
                    failure_revision_id: actual,
                } if actual == &failure_revision_id
            ) {
                return Err(ProtocolViolation::ReviewContract {
                    code: "review_terminal_node_failure_missing",
                });
            }
        }

        let (mission, process_health, reason_code) = match &convergence.reason {
            ReviewConvergenceReasonV1::ReviewBudgetExhausted { node_id } => {
                let code = "review_budget_exhausted";
                (
                    MissionResult::BudgetBlocked {
                        node_id: node_id.clone(),
                        failure: FirstFatalBlocker {
                            category: "budget".into(),
                            code: code.into(),
                            node_id: Some(node_id.clone()),
                        },
                    },
                    ProcessHealth::Healthy,
                    code,
                )
            }
            ReviewConvergenceReasonV1::CompletionBudgetExhausted { node_id } => {
                let code = "completion_budget_exhausted";
                (
                    MissionResult::BudgetBlocked {
                        node_id: node_id.clone(),
                        failure: FirstFatalBlocker {
                            category: "budget".into(),
                            code: code.into(),
                            node_id: Some(node_id.clone()),
                        },
                    },
                    ProcessHealth::Healthy,
                    code,
                )
            }
            ReviewConvergenceReasonV1::RepositoryDrift { .. } => {
                let code = "review_repository_drift";
                (
                    MissionResult::ValidationFailed {
                        failure: FirstFatalBlocker {
                            category: "validation".into(),
                            code: code.into(),
                            node_id: None,
                        },
                    },
                    ProcessHealth::Healthy,
                    code,
                )
            }
            ReviewConvergenceReasonV1::ArtifactDurabilityFailed { .. } => {
                review_infrastructure_terminal("review_artifact_durability_failed", owner_node_id)
            }
            ReviewConvergenceReasonV1::ProviderProtocolExhausted { .. } => {
                review_infrastructure_terminal("review_provider_protocol_exhausted", owner_node_id)
            }
            ReviewConvergenceReasonV1::UncontactedReleaseRetryExhausted { .. } => {
                review_infrastructure_terminal(
                    "review_uncontacted_release_retry_exhausted",
                    owner_node_id,
                )
            }
            ReviewConvergenceReasonV1::PublicationAuthorityUnavailable { .. } => {
                review_infrastructure_terminal("publication_authority_unavailable", None)
            }
            ReviewConvergenceReasonV1::DiffManifestLimitExceeded { .. } => {
                review_blocked_terminal("review_diff_manifest_limit_exceeded", owner_node_id)
            }
            ReviewConvergenceReasonV1::DiffReviewBlocked { .. } => {
                review_blocked_terminal("review_diff_blocked", owner_node_id)
            }
            ReviewConvergenceReasonV1::CompletionIncomplete { .. } => {
                review_blocked_terminal("completion_incomplete", owner_node_id)
            }
            ReviewConvergenceReasonV1::PublicationEligibilityDenied { eligibility_id } => {
                let eligibility =
                    review
                        .eligibility
                        .as_deref()
                        .ok_or(ProtocolViolation::ReviewContract {
                            code: "publication_eligibility_denial_missing",
                        })?;
                if &eligibility.eligibility_id != eligibility_id {
                    return Err(ProtocolViolation::ReviewContract {
                        code: "publication_eligibility_denial_mismatch",
                    });
                }
                if publication_eligibility_denial_is_stale(eligibility) {
                    let remote_moved = matches!(
                        &eligibility.disposition,
                        PublicationEligibilityDispositionV1::Denied { failed_predicates }
                            if failed_predicates
                                .contains(&PublicationPredicateV1::RemoteHeadUnchanged)
                    );
                    let code = if remote_moved {
                        "publication_remote_head_moved"
                    } else {
                        "publication_validation_stale"
                    };
                    (
                        MissionResult::ValidationFailed {
                            failure: FirstFatalBlocker {
                                category: "validation".into(),
                                code: code.into(),
                                node_id: None,
                            },
                        },
                        ProcessHealth::Healthy,
                        code,
                    )
                } else {
                    review_blocked_terminal("publication_eligibility_denied", None)
                }
            }
        };
        Ok(CanonicalResult {
            mission,
            process_health,
            reason_code: reason_code.into(),
            repository_revision: self.repository_revision.clone(),
            remaining_work: self.unresolved_required_nodes().into_iter().collect(),
        })
    }

    fn authoritative_publication_terminal_result(
        &self,
    ) -> Result<CanonicalResult, ProtocolViolation> {
        let publication =
            self.publication
                .as_ref()
                .ok_or(ProtocolViolation::PublicationContract {
                    code: "publication_state_missing",
                })?;
        if let Some(convergence) = &publication.convergence {
            let failure_revision_id = publication_convergence_failure_revision(convergence);
            let node = self
                .nodes
                .get(&publication.publication_node_id)
                .ok_or_else(|| ProtocolViolation::UnknownNode {
                    node_id: publication.publication_node_id.clone(),
                })?;
            if !matches!(
                &node.state,
                NodeState::FailedTerminal {
                    failure_revision_id: actual,
                } if actual == &failure_revision_id
            ) {
                return Err(ProtocolViolation::PublicationContract {
                    code: "publication_terminal_node_failure_missing",
                });
            }
            let code = match &convergence.reason {
                PublicationConvergenceReasonV1::AttemptsExhausted {
                    operation: PublicationOperationV1::Commit,
                } => "publication_commit_attempts_exhausted",
                PublicationConvergenceReasonV1::AttemptsExhausted {
                    operation: PublicationOperationV1::Push,
                } => "publication_push_attempts_exhausted",
                PublicationConvergenceReasonV1::AttemptsExhausted {
                    operation: PublicationOperationV1::PullRequest,
                } => "publication_pull_request_attempts_exhausted",
                PublicationConvergenceReasonV1::PermanentFailure {
                    operation: PublicationOperationV1::Commit,
                    ..
                } => "publication_commit_permanent_failure",
                PublicationConvergenceReasonV1::PermanentFailure {
                    operation: PublicationOperationV1::Push,
                    ..
                } => "publication_push_permanent_failure",
                PublicationConvergenceReasonV1::PermanentFailure {
                    operation: PublicationOperationV1::PullRequest,
                    ..
                } => "publication_pull_request_permanent_failure",
                PublicationConvergenceReasonV1::RemoteBranchMoved { .. } => {
                    "publication_remote_branch_moved"
                }
            };
            return Ok(CanonicalResult {
                mission: MissionResult::PublicationFailed {
                    failure: FirstFatalBlocker {
                        category: "publication".into(),
                        code: code.into(),
                        node_id: Some(node.id.clone()),
                    },
                },
                process_health: ProcessHealth::Failed { code: code.into() },
                reason_code: code.into(),
                repository_revision: self.repository_revision.clone(),
                remaining_work: self.unresolved_required_nodes().into_iter().collect(),
            });
        }

        let completion =
            publication
                .completion
                .as_ref()
                .ok_or(ProtocolViolation::PublicationContract {
                    code: "publication_terminal_without_completion_or_convergence",
                })?;
        let proof = self.publication_completion_proof()?;
        let node = self
            .nodes
            .get(&publication.publication_node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: publication.publication_node_id.clone(),
            })?;
        if !matches!(
            &node.state,
            NodeState::Succeeded { proof_id } if proof_id == &proof.id
        ) || self.proofs.get(&proof.id) != Some(&proof)
        {
            return Err(ProtocolViolation::PublicationContract {
                code: "publication_terminal_completion_proof_missing",
            });
        }
        let review_completion = self
            .review
            .as_ref()
            .and_then(|review| review.completion.as_deref())
            .ok_or(ProtocolViolation::PublicationContract {
                code: "publication_review_completion_missing",
            })?;
        let (mission, reason_code) = match (
            completion.requested_mode,
            review_completion.disposition,
            completion.draft,
        ) {
            (PublicationModeV1::Normal, CompletionDispositionV1::Complete, false) => (
                MissionResult::Succeeded {
                    publication_proof_id: proof.id,
                },
                "publication_succeeded".to_owned(),
            ),
            (
                PublicationModeV1::NormalWithExternalReview,
                CompletionDispositionV1::CompletePendingExternalReview,
                true,
            ) => (
                MissionResult::PartialReviewable {
                    publication_proof_id: proof.id,
                    external_review_reason_code: external_review_reason_code(review_completion)?,
                },
                "publication_pending_external_review".to_owned(),
            ),
            _ => {
                return Err(ProtocolViolation::PublicationContract {
                    code: "publication_terminal_mode_mismatch",
                });
            }
        };
        Ok(CanonicalResult {
            mission,
            process_health: ProcessHealth::Healthy,
            reason_code,
            repository_revision: self.repository_revision.clone(),
            remaining_work: self.unresolved_required_nodes().into_iter().collect(),
        })
    }

    fn authoritative_validation_terminal_result(
        &self,
    ) -> Result<CanonicalResult, ProtocolViolation> {
        let validation = self
            .validation
            .as_ref()
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_state_missing",
            })?;
        let convergence =
            validation
                .convergence
                .as_ref()
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "validation_terminal_without_convergence",
                })?;
        convergence.validate()?;
        if convergence.repository_revision != self.repository_revision {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_terminal_revision_mismatch",
            });
        }
        let (mission, process_health, reason_code) =
            match &convergence.reason {
                ValidationConvergenceReason::NoValidRepair => {
                    let failure = validation
                        .failures
                        .get(&convergence.failure_revision_id)
                        .ok_or(ProtocolViolation::ValidationContract {
                            code: "validation_terminal_failure_missing",
                        })?;
                    let code = "validation_no_valid_repair";
                    (
                        MissionResult::NoValidRepair {
                            failure: FirstFatalBlocker {
                                category: "validation".into(),
                                code: code.into(),
                                node_id: Some(failure.node_id.clone()),
                            },
                        },
                        ProcessHealth::Healthy,
                        code,
                    )
                }
                ValidationConvergenceReason::GateRunBudgetExhausted { gate_id } => {
                    let gate = validation.gates.get(gate_id).ok_or(
                        ProtocolViolation::ValidationContract {
                            code: "validation_terminal_gate_missing",
                        },
                    )?;
                    let code = "validation_gate_run_budget_exhausted";
                    (
                        MissionResult::BudgetBlocked {
                            node_id: gate.node_id.clone(),
                            failure: FirstFatalBlocker {
                                category: "budget".into(),
                                code: code.into(),
                                node_id: Some(gate.node_id.clone()),
                            },
                        },
                        ProcessHealth::Healthy,
                        code,
                    )
                }
                ValidationConvergenceReason::InfrastructureFailure {
                    kind: ValidationInfrastructureFailureKind::Canceled,
                    run_id,
                } => {
                    let _run = validation.runs.get(run_id).ok_or(
                        ProtocolViolation::ValidationContract {
                            code: "validation_terminal_run_missing",
                        },
                    )?;
                    let code = "validation_process_canceled";
                    (
                        MissionResult::Canceled {
                            cancellation_reason_code: code.into(),
                        },
                        ProcessHealth::Healthy,
                        code,
                    )
                }
                ValidationConvergenceReason::InfrastructureFailure { kind, run_id } => {
                    let run = validation.runs.get(run_id).ok_or(
                        ProtocolViolation::ValidationContract {
                            code: "validation_terminal_run_missing",
                        },
                    )?;
                    let code = match kind {
                        ValidationInfrastructureFailureKind::Spawn => {
                            "validation_process_spawn_failed"
                        }
                        ValidationInfrastructureFailureKind::Timeout => {
                            "validation_process_timeout"
                        }
                        ValidationInfrastructureFailureKind::Journal => {
                            "validation_process_journal_failed"
                        }
                        ValidationInfrastructureFailureKind::Transport => {
                            "validation_process_transport_failed"
                        }
                        ValidationInfrastructureFailureKind::Canceled => unreachable!(
                            "validation cancellation is handled as a canonical canceled mission"
                        ),
                        ValidationInfrastructureFailureKind::LeaseLost => {
                            "validation_process_lease_lost"
                        }
                    };
                    (
                        MissionResult::InfrastructureFailed {
                            failure: FirstFatalBlocker {
                                category: "infrastructure".into(),
                                code: code.into(),
                                node_id: Some(run.request.schedule.node_id.clone()),
                            },
                        },
                        ProcessHealth::Failed { code: code.into() },
                        code,
                    )
                }
            };
        Ok(CanonicalResult {
            mission,
            process_health,
            reason_code: reason_code.into(),
            repository_revision: self.repository_revision.clone(),
            remaining_work: self.unresolved_required_nodes().into_iter().collect(),
        })
    }

    fn validate_terminal_mission(
        &self,
        result: &CanonicalResult,
        source_stage: ProtocolStage,
    ) -> Result<(), ProtocolViolation> {
        if source_stage == ProtocolStage::Publication && self.publication.is_some() {
            if result != &self.authoritative_publication_terminal_result()? {
                return Err(ProtocolViolation::TerminalPredicate {
                    code: "publication_terminal_result_not_authoritative",
                });
            }
            return Ok(());
        }
        if source_stage == ProtocolStage::Review && self.review.is_some() {
            if self
                .review
                .as_ref()
                .is_none_or(|review| review.convergence.is_none())
            {
                return Err(ProtocolViolation::TerminalPredicate {
                    code: "review_terminal_without_convergence",
                });
            }
            if result != &self.authoritative_review_terminal_result()? {
                return Err(ProtocolViolation::TerminalPredicate {
                    code: "review_terminal_result_not_authoritative",
                });
            }
            return Ok(());
        }
        if source_stage == ProtocolStage::Implementation
            && let Some(expected) = self.authoritative_mutation_terminal_result()?
        {
            if result != &expected {
                return Err(ProtocolViolation::TerminalPredicate {
                    code: "mutation_terminal_result_not_authoritative",
                });
            }
            return Ok(());
        }
        if source_stage == ProtocolStage::Repair
            && let Some(expected) = self.authoritative_repair_mutation_terminal_result()?
        {
            if result != &expected {
                return Err(ProtocolViolation::TerminalPredicate {
                    code: "repair_mutation_terminal_result_not_authoritative",
                });
            }
            return Ok(());
        }
        if matches!(
            source_stage,
            ProtocolStage::Validation | ProtocolStage::Repair
        ) && self.validation.is_some()
        {
            if self
                .validation
                .as_ref()
                .is_none_or(|validation| validation.convergence.is_none())
            {
                return Err(ProtocolViolation::TerminalPredicate {
                    code: "validation_terminal_without_convergence",
                });
            }
            if result != &self.authoritative_validation_terminal_result()? {
                return Err(ProtocolViolation::TerminalPredicate {
                    code: "validation_terminal_result_not_authoritative",
                });
            }
            return Ok(());
        }
        match &result.mission {
            MissionResult::Succeeded {
                publication_proof_id,
            }
            | MissionResult::PartialReviewable {
                publication_proof_id,
                ..
            } => {
                let required_publication = self.required_nodes(NodeKind::Publication);
                if source_stage != ProtocolStage::Publication
                    || self.proof_kind(publication_proof_id)
                        != Some(ProofKind::PublicationCompleted)
                    || required_publication.is_empty()
                    || required_publication
                        .iter()
                        .any(|node| !matches!(node.state, NodeState::Succeeded { .. }))
                    || required_publication.iter().all(|node| {
                        !matches!(
                            &node.state,
                            NodeState::Succeeded { proof_id }
                                if proof_id == publication_proof_id
                        )
                    })
                {
                    return Err(ProtocolViolation::TerminalPredicate {
                        code: "successful_publication_proof_missing",
                    });
                }
                if let MissionResult::PartialReviewable {
                    external_review_reason_code,
                    ..
                } = &result.mission
                    && external_review_reason_code.trim().is_empty()
                {
                    return Err(ProtocolViolation::TerminalPredicate {
                        code: "external_review_reason_missing",
                    });
                }
            }
            MissionResult::SucceededNoOp { no_op_proof_id } => {
                if source_stage != ProtocolStage::Planning
                    || self.proof_kind(no_op_proof_id) != Some(ProofKind::NoOpSatisfied)
                {
                    return Err(ProtocolViolation::TerminalPredicate {
                        code: "no_op_proof_missing",
                    });
                }
                if let Some(planning) = &self.planning
                    && (planning.accepted_no_op.is_none()
                        || planning.accepted_plan.is_some()
                        || planning.convergence.is_some()
                        || self.required_nodes(NodeKind::Planning).iter().any(|node| {
                            !matches!(
                                &node.state,
                                NodeState::Succeeded { proof_id } if proof_id == no_op_proof_id
                            )
                        }))
                {
                    return Err(ProtocolViolation::TerminalPredicate {
                        code: "typed_planning_no_op_not_satisfied",
                    });
                }
                if self.nodes.values().any(|node| is_planned_node(node.kind))
                    || self.event_log.iter().any(|stored| {
                        matches!(
                            stored.envelope.payload,
                            DomainEvent::Graph(GraphEvent::NodesAdded { .. })
                        )
                    })
                {
                    return Err(ProtocolViolation::TerminalPredicate {
                        code: "no_op_conflicts_with_materialized_plan",
                    });
                }
            }
            MissionResult::BlockedNoDiff { .. } => {
                require_terminal_stage(
                    source_stage,
                    &[ProtocolStage::Implementation, ProtocolStage::Review],
                )?;
            }
            MissionResult::NoValidRepair { .. } => {
                require_terminal_stage(source_stage, &[ProtocolStage::Repair])?;
            }
            MissionResult::InsufficientEvidence { .. } => {
                require_terminal_stage(
                    source_stage,
                    &[ProtocolStage::Discovery, ProtocolStage::Planning],
                )?;
                if source_stage == ProtocolStage::Planning
                    && let Some(planning) = &self.planning
                    && (!matches!(
                        planning.convergence,
                        Some(PlanningConvergence::InsufficientEvidence { .. })
                    ) || self
                        .required_nodes(NodeKind::Planning)
                        .iter()
                        .any(|node| !matches!(node.state, NodeState::FailedTerminal { .. })))
                {
                    return Err(ProtocolViolation::TerminalPredicate {
                        code: "planning_insufficient_evidence_not_converged",
                    });
                }
            }
            MissionResult::ValidationFailed { .. } => {
                require_terminal_stage(
                    source_stage,
                    &[ProtocolStage::Validation, ProtocolStage::Repair],
                )?;
            }
            MissionResult::BudgetBlocked { node_id, .. } => {
                let node =
                    self.nodes
                        .get(node_id)
                        .ok_or_else(|| ProtocolViolation::UnknownNode {
                            node_id: node_id.clone(),
                        })?;
                let budget_is_exhausted =
                    if node.kind == NodeKind::Discovery && self.repository_profile.is_some() {
                        self.discovery_budget_is_exhausted(node_id)?
                    } else if node.kind == NodeKind::Planning && self.planning.is_some() {
                        self.planning_budget_remaining(node_id)?.is_exhausted()
                    } else {
                        node_budget_is_exhausted(node)
                    };
                if !node.required
                    || node.kind.stage() != source_stage
                    || node.state.satisfies_dependency()
                    || !budget_is_exhausted
                {
                    return Err(ProtocolViolation::TerminalPredicate {
                        code: "budget_owner_is_not_current_unresolved_work",
                    });
                }
                if node.kind == NodeKind::Planning
                    && let Some(planning) = &self.planning
                    && !matches!(
                        planning.convergence,
                        Some(PlanningConvergence::BudgetBlocked { .. })
                    )
                {
                    return Err(ProtocolViolation::TerminalPredicate {
                        code: "planning_budget_not_converged",
                    });
                }
            }
            MissionResult::InfrastructureFailed { .. } => {}
            MissionResult::Canceled {
                cancellation_reason_code,
            } => {
                if cancellation_reason_code.trim().is_empty() {
                    return Err(ProtocolViolation::TerminalPredicate {
                        code: "cancellation_reason_missing",
                    });
                }
            }
            MissionResult::PublicationFailed { .. } => {
                require_terminal_stage(source_stage, &[ProtocolStage::Publication])?;
            }
        }
        if result.mission.outcome().is_success()
            && matches!(result.process_health, ProcessHealth::Failed { .. })
        {
            return Err(ProtocolViolation::TerminalPredicate {
                code: "successful_result_has_failed_process_health",
            });
        }
        if let Some(failure) = result.mission.first_fatal_blocker()
            && (failure.category.trim().is_empty() || failure.code.trim().is_empty())
        {
            return Err(ProtocolViolation::TerminalPredicate {
                code: "first_fatal_blocker_is_incomplete",
            });
        }
        if let Some(node_id) = result
            .mission
            .first_fatal_blocker()
            .and_then(|failure| failure.node_id.as_ref())
            && !self.nodes.contains_key(node_id)
        {
            return Err(ProtocolViolation::TerminalPredicate {
                code: "first_fatal_blocker_node_unknown",
            });
        }
        match &result.process_health {
            ProcessHealth::Degraded { code } | ProcessHealth::Failed { code }
                if code.trim().is_empty() =>
            {
                Err(ProtocolViolation::TerminalPredicate {
                    code: "process_health_code_missing",
                })
            }
            _ => Ok(()),
        }
    }

    fn ensure_dependencies_satisfied(&self, node_id: &NodeId) -> Result<(), ProtocolViolation> {
        let node = self
            .nodes
            .get(node_id)
            .ok_or_else(|| ProtocolViolation::UnknownNode {
                node_id: node_id.clone(),
            })?;
        for dependency_id in &node.dependencies {
            let dependency =
                self.nodes
                    .get(dependency_id)
                    .ok_or_else(|| ProtocolViolation::InvalidGraph {
                        code: "unknown_dependency",
                        node_id: Some(node_id.clone()),
                    })?;
            if !dependency.state.satisfies_dependency() {
                return Err(ProtocolViolation::UnsatisfiedDependency {
                    node_id: node_id.clone(),
                    dependency_id: dependency_id.clone(),
                });
            }
        }
        Ok(())
    }

    fn refresh_ready_nodes(&mut self) {
        let stage = self.stage();
        let ready = self
            .node_order
            .iter()
            .filter_map(|node_id| self.nodes.get(node_id))
            .filter(|node| {
                node.kind.stage() == stage
                    && (matches!(&node.state, NodeState::Pending)
                        || (node.kind == NodeKind::Validation
                            && matches!(&node.state, NodeState::FailedRecoverable { .. })))
                    && node.dependencies.iter().all(|dependency_id| {
                        self.nodes
                            .get(dependency_id)
                            .is_some_and(|dependency| dependency.state.satisfies_dependency())
                    })
            })
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        for node_id in ready {
            if let Some(node) = self.nodes.get_mut(&node_id) {
                node.state = NodeState::Ready;
            }
        }
    }

    fn has_open_model_call(&self) -> bool {
        self.budgets.model_calls.values().any(|record| {
            matches!(
                record.state,
                ModelCallState::Admitted | ModelCallState::Reserved | ModelCallState::Dispatched
            )
        })
    }

    fn has_open_model_call_for_node(&self, node_id: &NodeId) -> bool {
        self.budgets.model_calls.values().any(|record| {
            &record.admission.node_id == node_id
                && matches!(
                    record.state,
                    ModelCallState::Admitted
                        | ModelCallState::Reserved
                        | ModelCallState::Dispatched
                )
        })
    }

    fn has_proof_kind_for_current_revision(&self, kind: ProofKind) -> bool {
        self.proofs.values().any(|proof| {
            proof.kind == kind && proof.repository_revision == self.repository_revision
        })
    }

    fn has_discovery_evidence(&self, evidence_id: &EvidenceId) -> bool {
        self.discovery.as_ref().is_some_and(|discovery| {
            discovery
                .completed_searches
                .values()
                .any(|evidence| &evidence.evidence_id == evidence_id)
                || discovery
                    .candidates
                    .values()
                    .any(|evidence| &evidence.evidence_id == evidence_id)
                || discovery.file_evidence.contains_key(evidence_id)
                || discovery.relationships.contains_key(evidence_id)
                || discovery
                    .impact_map
                    .as_ref()
                    .is_some_and(|evidence| &evidence.evidence_id == evidence_id)
        })
    }

    fn has_protocol_evidence(&self, evidence_id: &EvidenceId) -> bool {
        self.has_discovery_evidence(evidence_id)
            || self
                .finalization_policy
                .as_ref()
                .is_some_and(|policy| &policy.policy_evidence_id == evidence_id)
            || self.review.as_ref().is_some_and(|review| {
                review
                    .diff_manifest
                    .as_ref()
                    .is_some_and(|manifest| manifest.manifest_id.as_str() == evidence_id.as_str())
                    || review.page_reviews.values().any(|page| {
                        page.observation_id.as_str() == evidence_id.as_str()
                            || page
                                .findings
                                .iter()
                                .any(|finding| &finding.finding_id == evidence_id)
                    })
                    || review
                        .diff_review
                        .as_ref()
                        .is_some_and(|record| record.review_id.as_str() == evidence_id.as_str())
                    || review
                        .completion
                        .as_ref()
                        .is_some_and(|record| record.evaluation_id.as_str() == evidence_id.as_str())
                    || review
                        .authority
                        .as_ref()
                        .is_some_and(|record| record.authority_id.as_str() == evidence_id.as_str())
                    || review.eligibility.as_ref().is_some_and(|record| {
                        record.eligibility_id.as_str() == evidence_id.as_str()
                    })
            })
            || self.publication.as_ref().is_some_and(|publication| {
                publication.attempts.iter().any(|record| {
                    record
                        .observation
                        .as_ref()
                        .is_some_and(|observation| match observation {
                            PublicationAttemptObservationV1::Commit(observation) => {
                                observation.observation_id.as_str() == evidence_id.as_str()
                            }
                            PublicationAttemptObservationV1::Push(observation) => {
                                observation.observation_id.as_str() == evidence_id.as_str()
                            }
                            PublicationAttemptObservationV1::PullRequest(observation) => {
                                observation.observation_id.as_str() == evidence_id.as_str()
                            }
                        })
                }) || publication.completion.as_ref().is_some_and(|completion| {
                    completion.completion_id.as_str() == evidence_id.as_str()
                })
            })
            || self.validation.as_ref().is_some_and(|validation| {
                validation.evidence.values().any(|evidence| {
                    evidence.evidence_id.as_str() == evidence_id.as_str()
                        || evidence
                            .diagnostics
                            .iter()
                            .any(|diagnostic| &diagnostic.diagnostic_id == evidence_id)
                }) || validation
                    .reruns
                    .values()
                    .any(|rerun| &rerun.rerun_id == evidence_id)
            })
            || self.event_log.iter().any(|stored| {
                matches!(
                    &stored.envelope.payload,
                    DomainEvent::Mutation(MutationEvent::MutationVerified { evidence })
                        if &evidence.evidence_id == evidence_id
                )
            })
    }

    pub(crate) fn validate_invariants(&self) -> Result<(), ProtocolViolation> {
        if self.protocol_version != EXECUTION_PROTOCOL_VERSION {
            return Err(ProtocolViolation::UnsupportedVersion {
                found: self.protocol_version,
            });
        }
        if self.execution_id.is_empty() {
            return Err(ProtocolViolation::InvalidIdentity {
                field: "execution_id",
            });
        }
        if self.execution_attempt == 0 {
            return Err(ProtocolViolation::InvalidIdentity {
                field: "execution_attempt",
            });
        }
        if self.initial_repository_revision.is_empty() || self.repository_revision.is_empty() {
            return Err(ProtocolViolation::InvalidIdentity {
                field: "repository_revision",
            });
        }
        self.plan_graph_budget.validate()?;
        self.validate_event_log()?;
        self.validate_phase2_invariants()?;
        self.validate_phase3_invariants()?;
        self.validate_phase4_invariants()?;
        self.validate_phase5_invariants()?;
        self.validate_phase6_invariants()?;
        self.validate_phase7_invariants()?;
        self.validate_cached_position()?;
        self.validate_graph()?;
        self.validate_budget_invariants()?;
        self.validate_terminal_invariants()?;
        Ok(())
    }

    fn validate_phase2_invariants(&self) -> Result<(), ProtocolViolation> {
        match (&self.repository_profile, &self.discovery) {
            (None, None) => {
                if self.current_discovery_action.is_some() {
                    return Err(ProtocolViolation::Invariant {
                        code: "profileless_state_has_discovery_action",
                        detail: "a discovery action requires a typed repository profile".into(),
                    });
                }
                return Ok(());
            }
            (None, Some(_)) => {
                return Err(ProtocolViolation::Invariant {
                    code: "discovery_state_has_no_repository_profile",
                    detail: "typed discovery cannot outlive its repository profile".into(),
                });
            }
            (Some(profile), discovery) => {
                profile.validate()?;
                if profile.repository_revision != self.initial_repository_revision {
                    return Err(ProtocolViolation::RepositoryProfile {
                        code: "repository_profile_revision_mismatch",
                    });
                }
                let profile_event_count = self
                    .event_log
                    .iter()
                    .filter(|stored| {
                        matches!(
                            stored.envelope.payload,
                            DomainEvent::Profile(ProfileEvent::RepositoryProfileRecorded { .. })
                        )
                    })
                    .count();
                if profile_event_count != 1 {
                    return Err(ProtocolViolation::Invariant {
                        code: "repository_profile_event_mismatch",
                        detail: format!("profile events={profile_event_count}"),
                    });
                }
                if let Some(discovery) = discovery {
                    discovery.validate()?;
                    if discovery.repository_revision != self.initial_repository_revision
                        || discovery.repository_profile_id != profile.profile_id
                        || discovery.node_id.as_str() != "protocol-v1:discovery"
                    {
                        return Err(ProtocolViolation::DiscoveryContract {
                            code: "discovery_state_aggregate_binding_mismatch",
                        });
                    }
                    let goal_event_count = self
                        .event_log
                        .iter()
                        .filter(|stored| {
                            matches!(
                                stored.envelope.payload,
                                DomainEvent::Discovery(DiscoveryEvent::GoalRecorded { .. })
                            )
                        })
                        .count();
                    if goal_event_count != 1 {
                        return Err(ProtocolViolation::Invariant {
                            code: "discovery_goal_event_mismatch",
                            detail: format!("goal events={goal_event_count}"),
                        });
                    }
                } else if self.stage() != ProtocolStage::Profiling {
                    return Err(ProtocolViolation::DiscoveryContract {
                        code: "profiled_execution_left_profiling_without_discovery_goal",
                    });
                }
            }
        }

        if let Some(prepared) = &self.current_discovery_action {
            self.require_active_discovery_node()?;
            prepared.context.validate()?;
            prepared
                .envelope
                .validate_against_context(&prepared.context)?;
            let discovery = self.discovery.as_ref().expect("typed discovery exists");
            let expected_context = build_discovery_context(
                discovery,
                prepared.envelope.action_id.clone(),
                &prepared.envelope.constraints,
                prepared.context.input_token_ceiling,
            )?;
            let node = self.nodes.get(&discovery.node_id).ok_or_else(|| {
                ProtocolViolation::UnknownNode {
                    node_id: discovery.node_id.clone(),
                }
            })?;
            if prepared.admission.node_id != discovery.node_id
                || prepared.admission.action_id != prepared.envelope.action_id
                || prepared.admission.call_id.as_str() != prepared.envelope.reservation_id.as_str()
                || prepared.admission.payload_hash != prepared.envelope.payload_identity
                || prepared.admission.input_tokens != prepared.context.estimated_input_tokens
                || prepared.admission.output_tokens != prepared.envelope.output_token_allowance
                || prepared.context != expected_context
                || prepared.context.input_token_ceiling != node.budget.max_input_tokens_per_call
                || prepared.envelope.input_token_ceiling != node.budget.max_input_tokens_per_call
                || prepared.envelope.output_token_allowance
                    != node.budget.max_output_tokens_per_call
            {
                return Err(ProtocolViolation::DiscoveryContract {
                    code: "prepared_discovery_action_binding_mismatch",
                });
            }
            self.validate_discovery_action_constraints(discovery, &prepared.envelope)?;
            if let Some(record) = self.budgets.model_calls.get(&prepared.admission.call_id)
                && record.admission != prepared.admission
            {
                return Err(ProtocolViolation::ModelCallLifecycle {
                    call_id: prepared.admission.call_id.clone(),
                    code: "prepared_action_call_record_mismatch",
                });
            }
        }

        if self.stage() != ProtocolStage::Profiling && self.discovery.is_none() {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "typed_profile_has_no_discovery_state",
            });
        }
        let terminal_discovery_block = self.terminal.as_ref().is_some_and(|terminal| {
            matches!(
                (
                    &terminal.mission,
                    self.discovery
                        .as_ref()
                        .and_then(|state| state.convergence.as_ref())
                ),
                (
                    MissionResult::InsufficientEvidence { .. },
                    Some(DiscoveryConvergence::InsufficientEvidence { .. })
                ) | (
                    MissionResult::BudgetBlocked { .. },
                    Some(DiscoveryConvergence::BudgetBlocked { .. })
                )
            )
        });
        if matches!(
            self.stage(),
            ProtocolStage::Planning
                | ProtocolStage::Implementation
                | ProtocolStage::Validation
                | ProtocolStage::Repair
                | ProtocolStage::Review
                | ProtocolStage::Publication
        ) && !self.discovery.as_ref().is_some_and(|discovery| {
            matches!(
                &discovery.convergence,
                Some(DiscoveryConvergence::ImpactMapAccepted { .. })
            )
        }) || self.stage() == ProtocolStage::Terminal
            && !terminal_discovery_block
            && !self.discovery.as_ref().is_some_and(|discovery| {
                matches!(
                    &discovery.convergence,
                    Some(DiscoveryConvergence::ImpactMapAccepted { .. })
                )
            })
        {
            return Err(ProtocolViolation::DiscoveryContract {
                code: "post_discovery_state_has_no_accepted_impact_map",
            });
        }
        Ok(())
    }

    fn validate_phase3_invariants(&self) -> Result<(), ProtocolViolation> {
        let Some(planning) = &self.planning else {
            if self.current_planning_action.is_some()
                || self
                    .event_log
                    .iter()
                    .any(|stored| matches!(stored.envelope.payload, DomainEvent::Planning(_)))
            {
                return Err(ProtocolViolation::PlanningContract {
                    code: "planning_state_missing_for_planning_events",
                });
            }
            if self.repository_profile.is_some()
                && matches!(
                    self.stage(),
                    ProtocolStage::Planning
                        | ProtocolStage::Implementation
                        | ProtocolStage::Validation
                        | ProtocolStage::Repair
                        | ProtocolStage::Review
                        | ProtocolStage::Publication
                )
            {
                return Err(ProtocolViolation::PlanningContract {
                    code: "post_discovery_state_has_no_planning_state",
                });
            }
            return Ok(());
        };
        let profile =
            self.repository_profile
                .as_ref()
                .ok_or(ProtocolViolation::PlanningContract {
                    code: "planning_state_has_no_repository_profile",
                })?;
        let discovery = self
            .discovery
            .as_ref()
            .ok_or(ProtocolViolation::PlanningContract {
                code: "planning_state_has_no_discovery_state",
            })?;
        let node =
            self.nodes
                .get(&planning.node_id)
                .ok_or_else(|| ProtocolViolation::UnknownNode {
                    node_id: planning.node_id.clone(),
                })?;
        if planning.node_id.as_str() != "protocol-v1:planning"
            || node.kind != NodeKind::Planning
            || (self.stage() == ProtocolStage::Planning
                && planning.repository_revision != self.repository_revision)
            || self.stage() == ProtocolStage::Profiling
            || self.stage() == ProtocolStage::Discovery
        {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_state_aggregate_binding_mismatch",
            });
        }
        planning.validate(profile, discovery, &self.plan_graph_budget)?;
        let recorded_candidates = self
            .event_log
            .iter()
            .filter_map(|stored| match &stored.envelope.payload {
                DomainEvent::Planning(PlanningEvent::CandidateRecorded { candidate, .. }) => {
                    Some(candidate)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if recorded_candidates.len() != planning.candidate_records.len()
            || recorded_candidates
                .iter()
                .zip(&planning.candidate_records)
                .any(|(candidate, record)| *candidate != &record.candidate)
        {
            return Err(ProtocolViolation::PlanningContract {
                code: "planning_candidate_event_projection_mismatch",
            });
        }
        if let Some(prepared) = &self.current_planning_action {
            self.require_active_planning_node()?;
            prepared
                .envelope
                .validate_against_context(&prepared.context)?;
            let expected_context = build_planning_context(
                planning,
                discovery,
                prepared.envelope.action_id.clone(),
                node.budget.max_input_tokens_per_call,
            )?;
            if prepared.context != expected_context
                || prepared.admission.node_id != planning.node_id
                || prepared.admission.action_id != prepared.envelope.action_id
                || prepared.admission.call_id.as_str() != prepared.envelope.reservation_id.as_str()
                || prepared.admission.payload_hash != prepared.envelope.payload_identity
                || prepared.admission.input_tokens != prepared.context.estimated_input_tokens
                || prepared.admission.output_tokens != prepared.envelope.output_token_allowance
                || prepared.envelope.input_token_ceiling != node.budget.max_input_tokens_per_call
                || prepared.envelope.output_token_allowance
                    != node.budget.max_output_tokens_per_call
            {
                return Err(ProtocolViolation::PlanningContract {
                    code: "prepared_planning_action_binding_mismatch",
                });
            }
            if let Some(record) = self.budgets.model_calls.get(&prepared.admission.call_id)
                && record.admission != prepared.admission
            {
                return Err(ProtocolViolation::ModelCallLifecycle {
                    call_id: prepared.admission.call_id.clone(),
                    code: "prepared_planning_call_record_mismatch",
                });
            }
        }
        if planning.accepted_plan.is_some() {
            let materialization_events = self
                .event_log
                .iter()
                .filter(|stored| {
                    matches!(
                        stored.envelope.payload,
                        DomainEvent::Graph(GraphEvent::NodesAdded { .. })
                    )
                })
                .count();
            if self.stage() != ProtocolStage::Planning && materialization_events != 1 {
                return Err(ProtocolViolation::PlanningContract {
                    code: "accepted_plan_not_materialized",
                });
            }
        }
        Ok(())
    }

    fn validate_phase4_invariants(&self) -> Result<(), ProtocolViolation> {
        let Some(implementation) = &self.implementation else {
            if self
                .event_log
                .iter()
                .any(|stored| matches!(stored.envelope.payload, DomainEvent::Implementation(_)))
            {
                return Err(ProtocolViolation::ImplementationContract {
                    code: "implementation_state_missing_for_events",
                });
            }
            if self.repository_profile.is_some()
                && matches!(
                    self.stage(),
                    ProtocolStage::Implementation
                        | ProtocolStage::Validation
                        | ProtocolStage::Repair
                        | ProtocolStage::Review
                        | ProtocolStage::Publication
                )
            {
                return Err(ProtocolViolation::ImplementationContract {
                    code: "post_planning_state_has_no_implementation_state",
                });
            }
            return Ok(());
        };
        if matches!(
            self.stage(),
            ProtocolStage::Profiling | ProtocolStage::Discovery | ProtocolStage::Planning
        ) {
            return Err(ProtocolViolation::ImplementationContract {
                code: "implementation_state_before_implementation_transition",
            });
        }
        let planning = self
            .planning
            .as_ref()
            .ok_or(ProtocolViolation::ImplementationContract {
                code: "implementation_planning_state_missing",
            })?;
        let plan =
            planning
                .accepted_plan
                .as_ref()
                .ok_or(ProtocolViolation::ImplementationContract {
                    code: "implementation_accepted_plan_missing",
                })?;
        implementation.validate(plan)?;
        if implementation.repository_revision != self.initial_repository_revision {
            return Err(ProtocolViolation::ImplementationContract {
                code: "implementation_repository_revision_mismatch",
            });
        }
        let discovery =
            self.discovery
                .as_ref()
                .ok_or(ProtocolViolation::ImplementationContract {
                    code: "implementation_discovery_state_missing",
                })?;
        let mut recorded = ImplementationState::new(plan)?;
        let mut active_attempts = BTreeMap::<NodeId, u32>::new();
        let mut current_mutation_attempts = BTreeMap::<ContextManifestId, MutationAttemptId>::new();
        let mut mutation_failures = BTreeMap::<MutationAttemptId, MutationFailure>::new();
        for stored in &self.event_log {
            match &stored.envelope.payload {
                DomainEvent::Graph(GraphEvent::NodeStarted { node_id, attempt })
                    if self
                        .nodes
                        .get(node_id)
                        .is_some_and(|node| node.kind == NodeKind::Implementation) =>
                {
                    active_attempts.insert(node_id.clone(), *attempt);
                }
                DomainEvent::Graph(
                    GraphEvent::NodeSucceeded { node_id, .. }
                    | GraphEvent::NodeFailed { node_id, .. },
                ) => {
                    active_attempts.remove(node_id);
                }
                DomainEvent::Implementation(ImplementationEvent::TargetContextPrepared {
                    prepared,
                }) => {
                    let node = self.nodes.get(&prepared.node_id).ok_or_else(|| {
                        ProtocolViolation::UnknownNode {
                            node_id: prepared.node_id.clone(),
                        }
                    })?;
                    if node.kind != NodeKind::Implementation
                        || active_attempts.get(&prepared.node_id) != Some(&prepared.node_attempt)
                    {
                        return Err(ProtocolViolation::ImplementationContract {
                            code: "prepared_target_context_node_binding_mismatch",
                        });
                    }
                    let expected = build_target_context_load_request_for_attempt(
                        &self.execution_id,
                        self.execution_attempt,
                        &stored.envelope.repository_revision,
                        node,
                        prepared.node_attempt,
                        plan,
                        discovery,
                    )?;
                    prepared.validate_against_request(&expected)?;
                    recorded.record_prepared_context((**prepared).clone())?;
                }
                DomainEvent::Implementation(ImplementationEvent::TargetContextSuperseded {
                    supersession,
                }) => {
                    if active_attempts.get(&supersession.node_id)
                        != Some(&supersession.node_attempt)
                    {
                        return Err(ProtocolViolation::ImplementationContract {
                            code: "target_context_supersession_event_binding_mismatch",
                        });
                    }
                    if supersession.replacement_repository_revision
                        != stored.envelope.repository_revision
                    {
                        let drift = current_mutation_attempts
                            .get(&supersession.context_manifest_id)
                            .and_then(|attempt_id| mutation_failures.get(attempt_id))
                            .filter(|failure| {
                                failure.node_id == supersession.node_id
                                    && failure.context_manifest_id
                                        == supersession.context_manifest_id
                            })
                            .and_then(|failure| failure.repository_drift.as_ref());
                        if supersession.prepared_repository_revision
                            != stored.envelope.repository_revision
                            || drift.is_none_or(|drift| {
                                drift.expected_revision != stored.envelope.repository_revision
                                    || drift.observed_revision
                                        != supersession.replacement_repository_revision
                                    || !drift.context_rebuild_required
                            })
                        {
                            return Err(ProtocolViolation::MutationContract {
                                code: "target_context_drift_adoption_not_authoritative",
                            });
                        }
                    }
                    recorded.supersede_context((**supersession).clone())?;
                }
                DomainEvent::Mutation(MutationEvent::AttemptPolicySelected { policy }) => {
                    current_mutation_attempts.insert(
                        policy.context_manifest_id.clone(),
                        policy.attempt_id.clone(),
                    );
                }
                DomainEvent::Mutation(
                    MutationEvent::ActionRejected { failure }
                    | MutationEvent::AttemptFailed { failure },
                ) => {
                    mutation_failures.insert(failure.attempt_id.clone(), failure.clone());
                }
                _ => {}
            }
        }
        if &recorded != implementation {
            return Err(ProtocolViolation::ImplementationContract {
                code: "implementation_context_event_projection_mismatch",
            });
        }
        Ok(())
    }

    fn validate_phase5_invariants(&self) -> Result<(), ProtocolViolation> {
        self.mutation.validate()?;
        let Some(implementation) = &self.implementation else {
            if self.mutation != MutationLedger::default()
                || self
                    .event_log
                    .iter()
                    .any(|stored| matches!(stored.envelope.payload, DomainEvent::Mutation(_)))
            {
                return Err(ProtocolViolation::MutationContract {
                    code: "mutation_state_without_implementation",
                });
            }
            return Ok(());
        };
        for (context_id, target) in &self.mutation.contexts {
            let prepared = self.prepared_mutation_context(context_id).ok_or(
                ProtocolViolation::MutationContract {
                    code: "mutation_context_has_no_prepared_manifest",
                },
            )?;
            let owner_target_matches = match &prepared.manifest.purpose {
                TargetExecutionPurpose::Implementation { .. } => {
                    implementation.node_targets.get(&target.node_id) == Some(&target.target_id)
                }
                TargetExecutionPurpose::ValidationRepair {
                    failure_revision_id,
                    ..
                } => self.validation.as_ref().is_some_and(|validation| {
                    validation
                        .selections
                        .get(failure_revision_id)
                        .is_some_and(|selection| {
                            selection.repair_node.id == target.node_id
                                && selection.intent.target_id == target.target_id
                        })
                }),
            };
            if target.node_id != prepared.node_id
                || target.target_id != prepared.target_id
                || target.context_manifest_id != prepared.context_manifest_id
                || target.repository_revision != prepared.manifest.repository_revision
                || target.feasibility.node_attempt != prepared.node_attempt
                || !owner_target_matches
            {
                return Err(ProtocolViolation::MutationContract {
                    code: "mutation_context_projection_binding_mismatch",
                });
            }
        }
        for (node_id, context_id) in &self.mutation.current_by_node {
            if self
                .mutation
                .contexts
                .get(context_id)
                .is_none_or(|target| &target.node_id != node_id)
            {
                return Err(ProtocolViolation::MutationContract {
                    code: "mutation_current_context_projection_mismatch",
                });
            }
            let current = implementation.context_for_node(node_id).or_else(|| {
                self.validation
                    .as_ref()
                    .and_then(|validation| validation.repair_contexts.context_for_node(node_id))
            });
            if let Some(current) = current
                && current.context_manifest_id != *context_id
                && self.mutation.contexts.get(context_id).is_none_or(|target| {
                    target.attempts.values().next_back().is_none_or(|attempt| {
                        attempt.failure.as_ref().is_none_or(|failure| {
                            failure.retryability != MutationRetryability::RebuildContext
                        })
                    })
                })
            {
                return Err(ProtocolViolation::MutationContract {
                    code: "mutation_current_context_not_latest_prepared_context",
                });
            }
        }
        let mut recorded = MutationLedger::default();
        for event in self.event_log.iter().filter_map(|stored| {
            let DomainEvent::Mutation(event) = &stored.envelope.payload else {
                return None;
            };
            Some(event)
        }) {
            recorded.apply(event)?;
        }
        if recorded != self.mutation {
            return Err(ProtocolViolation::MutationContract {
                code: "mutation_event_projection_mismatch",
            });
        }
        Ok(())
    }

    fn validate_phase6_invariants(&self) -> Result<(), ProtocolViolation> {
        let validation_events = self
            .event_log
            .iter()
            .filter_map(|stored| match &stored.envelope.payload {
                DomainEvent::Validation(event) => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();
        let Some(validation) = &self.validation else {
            if !validation_events.is_empty() {
                return Err(ProtocolViolation::ValidationContract {
                    code: "validation_state_missing_for_events",
                });
            }
            if self.validation_policy.is_some()
                && matches!(
                    self.stage(),
                    ProtocolStage::Validation
                        | ProtocolStage::Repair
                        | ProtocolStage::Review
                        | ProtocolStage::Publication
                )
            {
                return Err(ProtocolViolation::ValidationContract {
                    code: "post_implementation_state_has_no_validation_state",
                });
            }
            return Ok(());
        };
        if matches!(
            self.stage(),
            ProtocolStage::Profiling
                | ProtocolStage::Discovery
                | ProtocolStage::Planning
                | ProtocolStage::Implementation
        ) {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_state_before_validation_transition",
            });
        }
        let policy =
            self.validation_policy
                .as_ref()
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "validation_policy_missing",
                })?;
        let profile =
            self.repository_profile
                .as_ref()
                .ok_or(ProtocolViolation::ValidationContract {
                    code: "validation_repository_profile_missing",
                })?;
        let plan = self
            .planning
            .as_ref()
            .and_then(|planning| planning.accepted_plan.as_ref())
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_accepted_plan_missing",
            })?;
        policy.validate(profile)?;
        let pending_verified_repair_handoff = self.pending_verified_repair_handoff()?.is_some();
        let pending_repair_convergence_revision_adoption =
            self.pending_repair_convergence_revision_adoption()?;
        let pending_review_drift_revision_adoption =
            self.pending_review_drift_revision_adoption()?;
        if validation.schema_version != VALIDATION_SCHEMA_VERSION
            || validation.policy_id != policy.policy_id
            || validation.plan_id != plan.plan_id
            || validation.plan_revision_id != plan.plan_revision_id
            || (validation.repository_revision != self.repository_revision
                && !pending_verified_repair_handoff
                && !pending_repair_convergence_revision_adoption
                && !pending_review_drift_revision_adoption)
        {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_state_aggregate_binding_mismatch",
            });
        }
        let initial_revision = validation
            .gate_order
            .first()
            .and_then(|gate_id| validation.gates.get(gate_id))
            .map(|gate| gate.repository_revision.clone())
            .ok_or(ProtocolViolation::ValidationContract {
                code: "validation_gate_set_missing",
            })?;
        let graph = materialize_accepted_plan(plan, &self.plan_graph_budget)?;
        let gates = build_validation_gates(plan, &graph, profile, policy, &initial_revision)?;
        let mut recorded = ValidationState::new(gates, policy, plan)?;
        for event in validation_events {
            recorded.apply(event, policy)?;
        }
        if &recorded != validation {
            return Err(ProtocolViolation::ValidationContract {
                code: "validation_event_projection_mismatch",
            });
        }
        for gate in validation.gates.values() {
            if self
                .nodes
                .get(&gate.node_id)
                .is_none_or(|node| node.kind != NodeKind::Validation)
            {
                return Err(ProtocolViolation::ValidationContract {
                    code: "validation_gate_graph_binding_mismatch",
                });
            }
        }
        Ok(())
    }

    fn validate_phase7_invariants(&self) -> Result<(), ProtocolViolation> {
        let review_events = self
            .event_log
            .iter()
            .filter_map(|stored| match &stored.envelope.payload {
                DomainEvent::Review(event) => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();
        let publication_events = self
            .event_log
            .iter()
            .filter_map(|stored| match &stored.envelope.payload {
                DomainEvent::Publication(event) => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();
        let Some(policy) = &self.finalization_policy else {
            if self.review.is_some()
                || self.publication.is_some()
                || !review_events.is_empty()
                || !publication_events.is_empty()
            {
                return Err(ProtocolViolation::ReviewContract {
                    code: "phase7_state_without_finalization_policy",
                });
            }
            return Ok(());
        };
        policy.validate()?;
        if matches!(
            self.stage(),
            ProtocolStage::Profiling
                | ProtocolStage::Discovery
                | ProtocolStage::Planning
                | ProtocolStage::Implementation
                | ProtocolStage::Validation
                | ProtocolStage::Repair
        ) {
            if self.review.is_some()
                || self.publication.is_some()
                || !review_events.is_empty()
                || !publication_events.is_empty()
            {
                return Err(ProtocolViolation::ReviewContract {
                    code: "phase7_state_before_review",
                });
            }
            return Ok(());
        }
        let plan = self
            .planning
            .as_ref()
            .and_then(|planning| planning.accepted_plan.as_ref())
            .ok_or(ProtocolViolation::ReviewContract {
                code: "review_accepted_plan_missing",
            })?;
        let review = self
            .review
            .as_ref()
            .ok_or(ProtocolViolation::ReviewContract {
                code: "review_state_missing",
            })?;
        let pending_review_drift_revision_adoption =
            self.pending_review_drift_revision_adoption()?;
        let ancestry = if pending_review_drift_revision_adoption {
            review.ancestry.clone()
        } else {
            self.engineering_ancestry()?
        };
        if review.ancestry != ancestry
            || (review.repository_revision != self.repository_revision
                && !pending_review_drift_revision_adoption)
            || review.review_node_id != self.single_required_node_id(NodeKind::Review)?
            || review.completion_node_id
                != self.single_required_node_id(NodeKind::CompletionEvaluation)?
        {
            return Err(ProtocolViolation::ReviewContract {
                code: "review_state_aggregate_binding_mismatch",
            });
        }
        review.validate(plan, policy)?;
        let mut recorded = ReviewStateV1::new(
            plan,
            policy,
            ancestry,
            review.review_node_id.clone(),
            review.completion_node_id.clone(),
        )?;
        for event in review_events {
            recorded.apply(event, plan, policy)?;
        }
        if &recorded != review {
            return Err(ProtocolViolation::ReviewContract {
                code: "review_event_projection_mismatch",
            });
        }
        let review_converged_terminal =
            self.stage() == ProtocolStage::Terminal && review.convergence.is_some();
        if review_converged_terminal {
            if self.publication.is_some() || !publication_events.is_empty() {
                return Err(ProtocolViolation::PublicationContract {
                    code: "publication_state_after_review_convergence",
                });
            }
        } else if matches!(
            self.stage(),
            ProtocolStage::Publication | ProtocolStage::Terminal
        ) {
            let eligibility =
                review
                    .eligibility
                    .as_deref()
                    .ok_or(ProtocolViolation::PublicationContract {
                        code: "publication_eligibility_missing",
                    })?;
            let publication =
                self.publication
                    .as_ref()
                    .ok_or(ProtocolViolation::PublicationContract {
                        code: "publication_state_missing",
                    })?;
            if publication.publication_node_id
                != self.single_required_node_id(NodeKind::Publication)?
                || publication.repository_revision != self.repository_revision
            {
                return Err(ProtocolViolation::PublicationContract {
                    code: "publication_state_aggregate_binding_mismatch",
                });
            }
            publication.validate(&policy.publication, eligibility)?;
            let manifest =
                review
                    .diff_manifest
                    .as_deref()
                    .ok_or(ProtocolViolation::PublicationContract {
                        code: "publication_diff_manifest_missing",
                    })?;
            let authority =
                review
                    .authority
                    .as_ref()
                    .ok_or(ProtocolViolation::PublicationContract {
                        code: "publication_authority_missing",
                    })?;
            let expected_tree =
                CommitTreeBindingV1::from_review_authority(eligibility, manifest, authority)?;
            let expected_pr_material = publication_pull_request_material(publication)?;
            if publication
                .attempts
                .iter()
                .any(|record| match &record.intent {
                    PublicationAttemptIntentV1::Commit(intent) => intent.tree != expected_tree,
                    PublicationAttemptIntentV1::Push(_) => false,
                    PublicationAttemptIntentV1::PullRequest(intent) => {
                        intent.execution_marker_hash
                            != publication.pull_request_execution_marker_hash()
                            || intent.title_hash != expected_pr_material.title_hash()
                            || intent.body_hash != expected_pr_material.body_hash()
                            || intent.title_bytes
                                != u64::try_from(expected_pr_material.title().len())
                                    .unwrap_or(u64::MAX)
                            || intent.body_bytes
                                != u64::try_from(expected_pr_material.body().len())
                                    .unwrap_or(u64::MAX)
                    }
                })
            {
                return Err(ProtocolViolation::PublicationContract {
                    code: "publication_event_aggregate_authority_mismatch",
                });
            }
            let mut replay = PublicationStateV1::new(
                self.execution_id.clone(),
                publication.publication_node_id.clone(),
                &policy.publication,
                eligibility,
            )?;
            for event in publication_events {
                replay.apply(event, &policy.publication, eligibility)?;
            }
            if &replay != publication {
                return Err(ProtocolViolation::PublicationContract {
                    code: "publication_event_projection_mismatch",
                });
            }
        } else if self.publication.is_some() || !publication_events.is_empty() {
            return Err(ProtocolViolation::PublicationContract {
                code: "publication_state_before_publication",
            });
        }
        Ok(())
    }

    pub(super) fn require_trusted_bootstrap(&self) -> Result<(), ProtocolViolation> {
        if !self.trusted_bootstrap {
            return Err(ProtocolViolation::Invariant {
                code: "untrusted_execution_snapshot",
                detail: "snapshot must be restored from a separately trusted bootstrap".into(),
            });
        }
        Ok(())
    }

    fn validate_event_log(&self) -> Result<(), ProtocolViolation> {
        if self.aggregate_revision != self.event_log.len() as u64 {
            return Err(ProtocolViolation::Invariant {
                code: "aggregate_revision_event_count_mismatch",
                detail: format!(
                    "revision={} events={}",
                    self.aggregate_revision,
                    self.event_log.len()
                ),
            });
        }
        if self.event_payload_hashes.len() != self.event_log.len() {
            return Err(ProtocolViolation::Invariant {
                code: "event_index_size_mismatch",
                detail: "event hash index does not match event log".into(),
            });
        }
        let mut recorded_proofs = BTreeMap::new();
        let mut recorded_model_calls = BTreeMap::new();
        let mut derived_repository_revision = self.initial_repository_revision.clone();
        for (index, stored) in self.event_log.iter().enumerate() {
            let expected_sequence = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            if stored.envelope.sequence != expected_sequence
                || stored.envelope.aggregate_revision_before != expected_sequence.saturating_sub(1)
            {
                return Err(ProtocolViolation::Invariant {
                    code: "event_sequence_is_not_contiguous",
                    detail: format!(
                        "event `{}` has sequence {}",
                        stored.envelope.event_id, stored.envelope.sequence
                    ),
                });
            }
            if stored.envelope.protocol_version != EXECUTION_PROTOCOL_VERSION
                || stored.envelope.event_schema_version != PROTOCOL_EVENT_SCHEMA_VERSION
                || stored.envelope.execution_id != self.execution_id
                || stored.envelope.execution_attempt != self.execution_attempt
                || stored.envelope.repository_revision != derived_repository_revision
                || stored.envelope.semantic_key.trim().is_empty()
                || stored.envelope.semantic_identity
                    != stored.envelope.expected_semantic_identity()?
                || stored.envelope.event_id != stored.envelope.expected_event_id()?
            {
                return Err(ProtocolViolation::Invariant {
                    code: "stored_event_envelope_invalid",
                    detail: format!(
                        "event `{}` does not bind to the aggregate",
                        stored.envelope.event_id
                    ),
                });
            }
            let hash = stored.envelope.canonical_hash()?;
            if stored.payload_hash != hash
                || self.event_payload_hashes.get(&stored.envelope.event_id) != Some(&hash)
            {
                return Err(ProtocolViolation::Invariant {
                    code: "stored_event_hash_mismatch",
                    detail: format!("event `{}` hash is inconsistent", stored.envelope.event_id),
                });
            }
            match &stored.envelope.payload {
                DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof }) => {
                    if recorded_proofs
                        .insert(proof.id.clone(), proof.clone())
                        .is_some()
                    {
                        return Err(ProtocolViolation::Invariant {
                            code: "event_log_contains_duplicate_proof",
                            detail: format!("proof `{}` appears more than once", proof.id),
                        });
                    }
                }
                DomainEvent::Budget(BudgetEvent::ModelCallAdmitted { admission }) => {
                    if recorded_model_calls
                        .insert(
                            admission.call_id.clone(),
                            ModelCallRecord {
                                admission: admission.clone(),
                                state: ModelCallState::Admitted,
                            },
                        )
                        .is_some()
                    {
                        return Err(ProtocolViolation::Invariant {
                            code: "event_log_model_call_lifecycle_invalid",
                            detail: format!(
                                "model call `{}` was admitted more than once",
                                admission.call_id
                            ),
                        });
                    }
                }
                DomainEvent::Budget(BudgetEvent::ModelCallReserved { call_id }) => {
                    let Some(record) = recorded_model_calls.get_mut(call_id) else {
                        return Err(ProtocolViolation::Invariant {
                            code: "event_log_model_call_lifecycle_invalid",
                            detail: format!("model call `{call_id}` was reserved before admission"),
                        });
                    };
                    if record.state != ModelCallState::Admitted {
                        return Err(ProtocolViolation::Invariant {
                            code: "event_log_model_call_lifecycle_invalid",
                            detail: format!("model call `{call_id}` has duplicate reservation"),
                        });
                    }
                    record.state = ModelCallState::Reserved;
                }
                DomainEvent::Budget(BudgetEvent::ProviderDispatchStarted {
                    call_id,
                    payload_hash,
                }) => {
                    let Some(record) = recorded_model_calls.get_mut(call_id) else {
                        return Err(ProtocolViolation::Invariant {
                            code: "event_log_model_call_lifecycle_invalid",
                            detail: format!("model call `{call_id}` dispatched before admission"),
                        });
                    };
                    if record.state != ModelCallState::Reserved
                        || record.admission.payload_hash != *payload_hash
                    {
                        return Err(ProtocolViolation::Invariant {
                            code: "event_log_model_call_lifecycle_invalid",
                            detail: format!(
                                "model call `{call_id}` dispatch did not match reservation"
                            ),
                        });
                    }
                    record.state = ModelCallState::Dispatched;
                }
                DomainEvent::Budget(BudgetEvent::ModelCallReconciled { call_id, result }) => {
                    let Some(record) = recorded_model_calls.get_mut(call_id) else {
                        return Err(ProtocolViolation::Invariant {
                            code: "event_log_model_call_lifecycle_invalid",
                            detail: format!("model call `{call_id}` reconciled before admission"),
                        });
                    };
                    record.state = match result {
                        ModelCallReconciliation::Consumed {
                            actual_cost_micros,
                            duration_ms,
                        } if record.state == ModelCallState::Dispatched => {
                            ModelCallState::ReconciledConsumed {
                                actual_cost_micros: *actual_cost_micros,
                                duration_ms: *duration_ms,
                            }
                        }
                        ModelCallReconciliation::ReleasedUncontacted
                            if record.state.owns_reservation() =>
                        {
                            ModelCallState::ReconciledReleased
                        }
                        _ => {
                            return Err(ProtocolViolation::Invariant {
                                code: "event_log_model_call_lifecycle_invalid",
                                detail: format!(
                                    "model call `{call_id}` has invalid reconciliation order"
                                ),
                            });
                        }
                    };
                }
                _ => {}
            }
            derived_repository_revision = repository_revision_after_event(
                &stored.envelope.payload,
                &derived_repository_revision,
            )?;
        }
        if derived_repository_revision != self.repository_revision {
            return Err(ProtocolViolation::Invariant {
                code: "repository_revision_event_chain_mismatch",
                detail: format!(
                    "derived={} aggregate={}",
                    derived_repository_revision, self.repository_revision
                ),
            });
        }
        if recorded_proofs != self.proofs {
            return Err(ProtocolViolation::Invariant {
                code: "proof_ledger_does_not_match_event_log",
                detail: "proof ledger diverged from committed proof events".into(),
            });
        }
        if recorded_model_calls != self.budgets.model_calls {
            return Err(ProtocolViolation::Invariant {
                code: "model_call_ledger_does_not_match_event_log",
                detail: "model call ledger diverged from committed budget events".into(),
            });
        }
        Ok(())
    }

    fn validate_cached_position(&self) -> Result<(), ProtocolViolation> {
        let mut derived = ProtocolPosition::Profiling(ProfileStep::InspectingMetadata);
        let mut latest_transition_proof = None;
        for stored in &self.event_log {
            match &stored.envelope.payload {
                DomainEvent::Lifecycle(LifecycleEvent::PositionAdvanced { from, to, proof_id }) => {
                    if derived.stage() != *from {
                        return Err(ProtocolViolation::Invariant {
                            code: "event_log_transition_source_mismatch",
                            detail: format!("derived={:?} event_from={from:?}", derived.stage()),
                        });
                    }
                    derived = ProtocolPosition::initial(*to);
                    latest_transition_proof = Some(proof_id.clone());
                }
                DomainEvent::Terminal(TerminalEvent::CanonicalResultRecorded { .. }) => {
                    derived = ProtocolPosition::Terminal;
                }
                _ => {}
            }
        }
        if derived.stage() == ProtocolStage::Discovery
            && let Some(discovery) = &self.discovery
        {
            derived = ProtocolPosition::Discovery(match discovery.substate() {
                DiscoverySubstate::NeedCandidates => DiscoveryStep::NeedCandidates,
                DiscoverySubstate::NeedGroundedReads => DiscoveryStep::NeedGroundedReads,
                DiscoverySubstate::NeedRelations => DiscoveryStep::NeedRelations,
                DiscoverySubstate::ReadyToSynthesize => DiscoveryStep::ReadyToSynthesize,
            });
        }
        if derived.stage() == ProtocolStage::Planning
            && let Some(planning) = &self.planning
        {
            derived = if self.current_planning_action.is_some() {
                ProtocolPosition::Planning(PlanningStep::AwaitingPlan)
            } else if planning.candidate_records.last().is_some_and(|record| {
                matches!(record.validation, PlanValidationResult::Rejected { .. })
            }) {
                ProtocolPosition::Planning(PlanningStep::EvidenceGap)
            } else {
                ProtocolPosition::Planning(PlanningStep::ReadyToSynthesize)
            };
        }
        if derived.stage() == ProtocolStage::Implementation {
            derived = ProtocolPosition::Implementation(self.authoritative_implementation_step());
        }
        if derived.stage() == ProtocolStage::Validation {
            derived = ProtocolPosition::Validation(self.authoritative_validation_step());
        }
        if derived.stage() == ProtocolStage::Repair {
            derived = ProtocolPosition::Repair(self.authoritative_repair_step());
        }
        if derived.stage() == ProtocolStage::Review {
            let step = self
                .review
                .as_ref()
                .map_or(ReviewStep::DiffReview, |review| {
                    if review.completion.is_none() {
                        if review.diff_review.is_some() {
                            ReviewStep::CompletionEvaluation
                        } else {
                            ReviewStep::DiffReview
                        }
                    } else {
                        ReviewStep::PublicationEligibility
                    }
                });
            derived = ProtocolPosition::Review(step);
        }
        if derived.stage() == ProtocolStage::Publication {
            let step = self
                .publication
                .as_ref()
                .map_or(PublicationStep::Commit, authoritative_publication_step);
            derived = ProtocolPosition::Publication(step);
        }
        if self.position != derived || self.latest_transition_proof != latest_transition_proof {
            return Err(ProtocolViolation::Invariant {
                code: "cached_protocol_position_mismatch",
                detail: format!("cached={:?} derived={derived:?}", self.position),
            });
        }
        Ok(())
    }

    fn validate_graph(&self) -> Result<(), ProtocolViolation> {
        if self.nodes.iter().any(|(node_id, node)| node_id != &node.id) {
            return Err(ProtocolViolation::InvalidGraph {
                code: "node_map_key_identity_mismatch",
                node_id: None,
            });
        }
        let ordered_ids = self.node_order.iter().cloned().collect::<BTreeSet<_>>();
        let mapped_ids = self.nodes.keys().cloned().collect::<BTreeSet<_>>();
        if ordered_ids.len() != self.node_order.len() || ordered_ids != mapped_ids {
            return Err(ProtocolViolation::InvalidGraph {
                code: "stable_order_node_map_mismatch",
                node_id: None,
            });
        }
        self.validate_graph_acyclic()?;
        let planned_nodes_present = self.nodes.values().any(|node| is_planned_node(node.kind));
        let materialization_events = self
            .event_log
            .iter()
            .filter(|stored| {
                matches!(
                    stored.envelope.payload,
                    DomainEvent::Graph(GraphEvent::NodesAdded { .. })
                )
            })
            .count();
        if materialization_events != usize::from(planned_nodes_present) {
            return Err(ProtocolViolation::InvalidGraph {
                code: "plan_graph_materialization_event_mismatch",
                node_id: None,
            });
        }
        if planned_nodes_present {
            self.validate_plan_topology()?;
            if let Some(planning) = &self.planning {
                let plan =
                    planning
                        .accepted_plan
                        .as_ref()
                        .ok_or(ProtocolViolation::PlanningContract {
                            code: "materialized_graph_has_no_accepted_plan",
                        })?;
                let expected_nodes = self.materialized_planning_nodes(plan)?;
                let expected_proof = self.planning_acceptance_proof(plan);
                let exact_event =
                    self.event_log
                        .iter()
                        .find_map(|stored| match &stored.envelope.payload {
                            DomainEvent::Graph(GraphEvent::NodesAdded {
                                plan_proof_id,
                                nodes,
                            }) => Some((plan_proof_id, nodes)),
                            _ => None,
                        });
                if exact_event != Some((&expected_proof.id, &expected_nodes)) {
                    return Err(ProtocolViolation::PlanningContract {
                        code: "materialized_graph_projection_mismatch",
                    });
                }
            }
        }
        let owners = self
            .nodes
            .values()
            .filter(|node| node.state.owns_execution())
            .collect::<Vec<_>>();
        if owners.len() > 1 {
            return Err(ProtocolViolation::Invariant {
                code: "multiple_active_owners",
                detail: owners
                    .iter()
                    .map(|node| node.id.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            });
        }
        if let Some(owner) = owners.first()
            && owner.kind.stage() != self.stage()
        {
            return Err(ProtocolViolation::WrongPosition {
                node_id: owner.id.clone(),
                position: self.stage(),
            });
        }
        for node in self.nodes.values() {
            if node.kind.requires_model()
                && (node.budget.max_model_calls == 0
                    || node.budget.max_input_tokens_per_call == 0
                    || node.budget.max_output_tokens_per_call == 0)
            {
                return Err(ProtocolViolation::InvalidGraph {
                    code: "model_node_has_no_viable_budget",
                    node_id: Some(node.id.clone()),
                });
            }
            let dependencies_satisfied = node.dependencies.iter().all(|dependency_id| {
                self.nodes
                    .get(dependency_id)
                    .is_some_and(|dependency| dependency.state.satisfies_dependency())
            });
            let ready_eligible = node.kind.stage() == self.stage() && dependencies_satisfied;
            match &node.state {
                NodeState::Pending if ready_eligible => {
                    return Err(ProtocolViolation::InvalidNodeState {
                        node_id: node.id.clone(),
                        code: "eligible_node_is_not_ready",
                    });
                }
                NodeState::Ready if !ready_eligible && self.terminal.is_none() => {
                    return Err(ProtocolViolation::InvalidNodeState {
                        node_id: node.id.clone(),
                        code: "ready_node_is_not_eligible",
                    });
                }
                NodeState::FailedRecoverable { .. }
                    if ready_eligible
                        && node.kind == NodeKind::Validation
                        && self
                            .validation
                            .as_ref()
                            .is_some_and(|validation| validation.pending_rerun.is_some())
                        && self
                            .latest_transition_proof
                            .as_ref()
                            .is_some_and(|proof_id| {
                                self.proof_kind(proof_id)
                                    == Some(ProofKind::ValidationRerunScheduled)
                            }) =>
                {
                    return Err(ProtocolViolation::InvalidNodeState {
                        node_id: node.id.clone(),
                        code: "scheduled_validation_rerun_is_not_ready",
                    });
                }
                _ => {}
            }
            for dependency_id in &node.dependencies {
                if !self.nodes.contains_key(dependency_id) {
                    return Err(ProtocolViolation::InvalidGraph {
                        code: "unknown_dependency",
                        node_id: Some(node.id.clone()),
                    });
                }
            }
            if matches!(
                node.state,
                NodeState::Ready | NodeState::Active { .. } | NodeState::Waiting { .. }
            ) {
                self.ensure_dependencies_satisfied(&node.id)?;
            }
            if let NodeState::Succeeded { proof_id } | NodeState::Skipped { proof_id } = &node.state
            {
                let proof =
                    self.proofs
                        .get(proof_id)
                        .ok_or_else(|| ProtocolViolation::UnknownProof {
                            proof_id: proof_id.clone(),
                        })?;
                if !proof.node_ids.contains(&node.id)
                    || !proof_satisfies_node(node.kind, proof.kind)
                {
                    return Err(ProtocolViolation::InvalidProof {
                        proof_id: proof_id.clone(),
                        code: "node_success_proof_mismatch",
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_plan_topology(&self) -> Result<(), ProtocolViolation> {
        for kind in [
            NodeKind::Implementation,
            NodeKind::Validation,
            NodeKind::Review,
            NodeKind::CompletionEvaluation,
            NodeKind::Publication,
        ] {
            if self.required_nodes(kind).is_empty() {
                return Err(ProtocolViolation::InvalidGraph {
                    code: "required_plan_stage_missing",
                    node_id: None,
                });
            }
        }

        let stable_indexes = self
            .node_order
            .iter()
            .enumerate()
            .map(|(index, node_id)| (node_id.clone(), index))
            .collect::<BTreeMap<_, _>>();
        for node in self.nodes.values() {
            let unique_dependencies = node.dependencies.iter().collect::<BTreeSet<_>>();
            if unique_dependencies.len() != node.dependencies.len() {
                return Err(ProtocolViolation::InvalidGraph {
                    code: "duplicate_dependency",
                    node_id: Some(node.id.clone()),
                });
            }
            let node_rank = protocol_node_order(node.kind);
            let node_index = stable_indexes
                .get(&node.id)
                .copied()
                .expect("stable order was validated before topology");
            for dependency_id in &node.dependencies {
                let dependency = self.nodes.get(dependency_id).ok_or_else(|| {
                    ProtocolViolation::InvalidGraph {
                        code: "unknown_dependency",
                        node_id: Some(node.id.clone()),
                    }
                })?;
                let dependency_rank = protocol_node_order(dependency.kind);
                let dependency_index = stable_indexes
                    .get(dependency_id)
                    .copied()
                    .expect("stable order was validated before topology");
                if dependency_rank > node_rank
                    || (dependency_rank == node_rank && dependency_index >= node_index)
                {
                    return Err(ProtocolViolation::InvalidGraph {
                        code: "dependency_is_not_stage_monotonic",
                        node_id: Some(node.id.clone()),
                    });
                }
            }
        }

        let required_reviews = self
            .required_nodes(NodeKind::Review)
            .into_iter()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        for completion in self.required_nodes(NodeKind::CompletionEvaluation) {
            if required_reviews
                .iter()
                .any(|review_id| !self.has_dependency_path(&completion.id, review_id))
            {
                return Err(ProtocolViolation::InvalidGraph {
                    code: "completion_does_not_depend_on_required_review",
                    node_id: Some(completion.id.clone()),
                });
            }
        }

        let required_completions = self
            .required_nodes(NodeKind::CompletionEvaluation)
            .into_iter()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        for publication in self.required_nodes(NodeKind::Publication) {
            if required_completions
                .iter()
                .any(|completion_id| !self.has_dependency_path(&publication.id, completion_id))
            {
                return Err(ProtocolViolation::InvalidGraph {
                    code: "publication_does_not_depend_on_completion",
                    node_id: Some(publication.id.clone()),
                });
            }
        }
        Ok(())
    }

    fn has_dependency_path(&self, node_id: &NodeId, ancestor_id: &NodeId) -> bool {
        let mut pending = self
            .nodes
            .get(node_id)
            .map_or_else(Vec::new, |node| node.dependencies.clone());
        let mut visited = BTreeSet::new();
        while let Some(candidate) = pending.pop() {
            if &candidate == ancestor_id {
                return true;
            }
            if visited.insert(candidate.clone())
                && let Some(node) = self.nodes.get(&candidate)
            {
                pending.extend(node.dependencies.iter().cloned());
            }
        }
        false
    }

    fn validate_graph_acyclic(&self) -> Result<(), ProtocolViolation> {
        let mut indegree = self
            .nodes
            .keys()
            .map(|node_id| (node_id.clone(), 0_usize))
            .collect::<BTreeMap<_, _>>();
        let mut dependents = BTreeMap::<NodeId, Vec<NodeId>>::new();
        for node in self.nodes.values() {
            for dependency in &node.dependencies {
                if dependency == &node.id {
                    return Err(ProtocolViolation::InvalidGraph {
                        code: "self_dependency",
                        node_id: Some(node.id.clone()),
                    });
                }
                if !self.nodes.contains_key(dependency) {
                    return Err(ProtocolViolation::InvalidGraph {
                        code: "unknown_dependency",
                        node_id: Some(node.id.clone()),
                    });
                }
                *indegree.get_mut(&node.id).expect("node has indegree") += 1;
                dependents
                    .entry(dependency.clone())
                    .or_default()
                    .push(node.id.clone());
            }
        }
        let mut ready = indegree
            .iter()
            .filter_map(|(node_id, degree)| (*degree == 0).then_some(node_id.clone()))
            .collect::<VecDeque<_>>();
        let mut visited = 0_usize;
        while let Some(node_id) = ready.pop_front() {
            visited += 1;
            for dependent in dependents.get(&node_id).into_iter().flatten() {
                let degree = indegree.get_mut(dependent).expect("dependent has indegree");
                *degree = degree.saturating_sub(1);
                if *degree == 0 {
                    ready.push_back(dependent.clone());
                }
            }
        }
        if visited != self.nodes.len() {
            return Err(ProtocolViolation::InvalidGraph {
                code: "dependency_cycle",
                node_id: None,
            });
        }
        Ok(())
    }

    fn validate_budget_invariants(&self) -> Result<(), ProtocolViolation> {
        let mut open_calls_by_node = BTreeMap::<NodeId, usize>::new();
        for (call_id, record) in &self.budgets.model_calls {
            if call_id != &record.admission.call_id {
                return Err(ProtocolViolation::Invariant {
                    code: "model_call_map_key_identity_mismatch",
                    detail: format!("model call map key `{call_id}` does not match its record"),
                });
            }
            let node = self.nodes.get(&record.admission.node_id).ok_or_else(|| {
                ProtocolViolation::UnknownNode {
                    node_id: record.admission.node_id.clone(),
                }
            })?;
            if !node.kind.requires_model() {
                return Err(ProtocolViolation::ModelCallLifecycle {
                    call_id: call_id.clone(),
                    code: "model_call_owned_by_deterministic_node",
                });
            }
            if record.admission.payload_hash.trim().is_empty() {
                return Err(ProtocolViolation::ModelCallLifecycle {
                    call_id: call_id.clone(),
                    code: "payload_hash_missing",
                });
            }
            if record.admission.input_tokens > node.budget.max_input_tokens_per_call {
                return Err(ProtocolViolation::BudgetExceeded {
                    node_id: Some(node.id.clone()),
                    dimension: "input_tokens_per_call",
                });
            }
            if record.admission.output_tokens > node.budget.max_output_tokens_per_call {
                return Err(ProtocolViolation::BudgetExceeded {
                    node_id: Some(node.id.clone()),
                    dimension: "output_tokens_per_call",
                });
            }
            if matches!(
                record.state,
                ModelCallState::Admitted | ModelCallState::Reserved | ModelCallState::Dispatched
            ) {
                if !matches!(node.state, NodeState::Active { .. }) {
                    return Err(ProtocolViolation::ModelCallLifecycle {
                        call_id: call_id.clone(),
                        code: "open_call_owner_is_not_active",
                    });
                }
                let open_count = open_calls_by_node.entry(node.id.clone()).or_default();
                *open_count = open_count.saturating_add(1);
                if *open_count > 1 {
                    return Err(ProtocolViolation::ModelCallLifecycle {
                        call_id: call_id.clone(),
                        code: "node_has_multiple_open_calls",
                    });
                }
            }
            if let ModelCallState::ReconciledConsumed {
                actual_cost_micros,
                duration_ms,
            } = record.state
                && (actual_cost_micros > record.admission.reserved_cost_micros
                    || duration_ms > record.admission.duration_allowance_ms)
            {
                return Err(ProtocolViolation::ModelCallLifecycle {
                    call_id: call_id.clone(),
                    code: "actual_usage_exceeds_reservation",
                });
            }
        }
        let mut computed_node_usage = self
            .nodes
            .keys()
            .map(|node_id| (node_id.clone(), BudgetUsage::default()))
            .collect::<BTreeMap<_, _>>();
        let mut computed_mission_usage = BudgetUsage::default();
        for record in self.budgets.model_calls.values() {
            let usage = computed_node_usage
                .get_mut(&record.admission.node_id)
                .ok_or_else(|| ProtocolViolation::UnknownNode {
                    node_id: record.admission.node_id.clone(),
                })?;
            match record.state {
                ModelCallState::Admitted | ModelCallState::ReconciledReleased => {}
                ModelCallState::Reserved | ModelCallState::Dispatched => {
                    reserve_usage(usage, &record.admission);
                    reserve_usage(&mut computed_mission_usage, &record.admission);
                }
                ModelCallState::ReconciledConsumed {
                    actual_cost_micros,
                    duration_ms,
                } => {
                    usage.model_calls_consumed = usage.model_calls_consumed.saturating_add(1);
                    usage.cost_micros_consumed = usage
                        .cost_micros_consumed
                        .saturating_add(actual_cost_micros);
                    usage.duration_ms_consumed =
                        usage.duration_ms_consumed.saturating_add(duration_ms);
                    computed_mission_usage.model_calls_consumed = computed_mission_usage
                        .model_calls_consumed
                        .saturating_add(1);
                    computed_mission_usage.cost_micros_consumed = computed_mission_usage
                        .cost_micros_consumed
                        .saturating_add(actual_cost_micros);
                    computed_mission_usage.duration_ms_consumed = computed_mission_usage
                        .duration_ms_consumed
                        .saturating_add(duration_ms);
                }
            }
        }
        let mutation_calls = self
            .event_log
            .iter()
            .filter_map(|stored| match &stored.envelope.payload {
                DomainEvent::Mutation(MutationEvent::ActionPrepared { prepared }) => Some((
                    prepared.admission.call_id.clone(),
                    (
                        prepared.policy.node_id.clone(),
                        prepared.policy.attempt_id.clone(),
                    ),
                )),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        let mut consumed_mutation_attempts = BTreeSet::new();
        for (call_id, (node_id, attempt_id)) in &mutation_calls {
            if !self.budgets.model_calls.get(call_id).is_some_and(|record| {
                matches!(record.state, ModelCallState::ReconciledConsumed { .. })
            }) {
                continue;
            }
            if !consumed_mutation_attempts.insert(attempt_id.clone()) {
                return Err(ProtocolViolation::Invariant {
                    code: "mutation_attempt_has_multiple_consumed_calls",
                    detail: format!(
                        "mutation attempt `{}` was consumed more than once",
                        attempt_id.as_str()
                    ),
                });
            }
            let usage = computed_node_usage.get_mut(node_id).ok_or_else(|| {
                ProtocolViolation::UnknownNode {
                    node_id: node_id.clone(),
                }
            })?;
            usage.mutation_attempts = usage.mutation_attempts.saturating_add(1);
            computed_mission_usage.mutation_attempts =
                computed_mission_usage.mutation_attempts.saturating_add(1);
        }
        for stored in &self.event_log {
            if let DomainEvent::Implementation(ImplementationEvent::TargetContextSuperseded {
                supersession,
            }) = &stored.envelope.payload
            {
                let usage = computed_node_usage
                    .get_mut(&supersession.node_id)
                    .ok_or_else(|| ProtocolViolation::UnknownNode {
                        node_id: supersession.node_id.clone(),
                    })?;
                usage.context_rebuilds = usage.context_rebuilds.saturating_add(1);
                computed_mission_usage.context_rebuilds =
                    computed_mission_usage.context_rebuilds.saturating_add(1);
            }
        }
        for node in self.nodes.values() {
            let computed = computed_node_usage.get(&node.id).expect("usage exists");
            if node.usage.model_calls_reserved != computed.model_calls_reserved
                || node.usage.model_calls_consumed != computed.model_calls_consumed
                || node.usage.cost_micros_reserved != computed.cost_micros_reserved
                || node.usage.cost_micros_consumed != computed.cost_micros_consumed
                || node.usage.duration_ms_reserved != computed.duration_ms_reserved
                || node.usage.duration_ms_consumed != computed.duration_ms_consumed
                || node.usage.mutation_attempts != computed.mutation_attempts
                || node.usage.context_rebuilds != computed.context_rebuilds
            {
                return Err(ProtocolViolation::Invariant {
                    code: "node_budget_usage_does_not_match_calls",
                    detail: format!("node `{}` accounting diverged", node.id),
                });
            }
            validate_usage_limits(&node.usage, &node.budget, Some(node.id.clone()))?;
            if node.usage.mutation_attempts > node.budget.max_mutation_attempts {
                return Err(ProtocolViolation::BudgetExceeded {
                    node_id: Some(node.id.clone()),
                    dimension: "mutation_attempts",
                });
            }
            if node.usage.context_rebuilds > node.budget.max_context_rebuilds {
                return Err(ProtocolViolation::BudgetExceeded {
                    node_id: Some(node.id.clone()),
                    dimension: "context_rebuilds",
                });
            }
        }
        if self.budgets.mission_usage.model_calls_reserved
            != computed_mission_usage.model_calls_reserved
            || self.budgets.mission_usage.model_calls_consumed
                != computed_mission_usage.model_calls_consumed
            || self.budgets.mission_usage.cost_micros_reserved
                != computed_mission_usage.cost_micros_reserved
            || self.budgets.mission_usage.cost_micros_consumed
                != computed_mission_usage.cost_micros_consumed
            || self.budgets.mission_usage.duration_ms_reserved
                != computed_mission_usage.duration_ms_reserved
            || self.budgets.mission_usage.duration_ms_consumed
                != computed_mission_usage.duration_ms_consumed
            || self.budgets.mission_usage.mutation_attempts
                != computed_mission_usage.mutation_attempts
            || self.budgets.mission_usage.context_rebuilds
                != computed_mission_usage.context_rebuilds
        {
            return Err(ProtocolViolation::Invariant {
                code: "mission_budget_usage_does_not_match_calls",
                detail: "mission accounting diverged".into(),
            });
        }
        validate_mission_usage_limits(&self.budgets.mission_usage, &self.mission_budget)?;
        Ok(())
    }

    fn validate_terminal_invariants(&self) -> Result<(), ProtocolViolation> {
        let mut source_stage = ProtocolStage::Profiling;
        let mut terminal_event = None;
        for (index, stored) in self.event_log.iter().enumerate() {
            match &stored.envelope.payload {
                DomainEvent::Lifecycle(LifecycleEvent::PositionAdvanced { to, .. }) => {
                    source_stage = *to;
                }
                DomainEvent::Terminal(TerminalEvent::CanonicalResultRecorded { result }) => {
                    match terminal_event {
                        None => terminal_event = Some((index, result)),
                        Some(_) => {
                            return Err(ProtocolViolation::Invariant {
                                code: "multiple_canonical_result_events",
                                detail: "event log contains more than one canonical result".into(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        match (&self.terminal, self.position) {
            (Some(result), ProtocolPosition::Terminal) => {
                if self.active_node().is_some() || self.has_open_model_call() {
                    return Err(ProtocolViolation::Invariant {
                        code: "terminal_state_has_active_work",
                        detail: "terminal state retains an owner or model call".into(),
                    });
                }
                let Some((terminal_index, event_result)) = terminal_event else {
                    return Err(ProtocolViolation::Invariant {
                        code: "canonical_result_event_missing",
                        detail: "terminal snapshot has no canonical result event".into(),
                    });
                };
                if terminal_index.saturating_add(1) != self.event_log.len() {
                    return Err(ProtocolViolation::Invariant {
                        code: "canonical_result_event_is_not_final",
                        detail: "protocol progress appears after the canonical result".into(),
                    });
                }
                if event_result != result {
                    return Err(ProtocolViolation::Invariant {
                        code: "canonical_result_does_not_match_event_log",
                        detail: "terminal snapshot differs from its committed event".into(),
                    });
                }
                self.validate_canonical_result(result, source_stage)?;
            }
            (None, ProtocolPosition::Terminal) => {
                return Err(ProtocolViolation::Invariant {
                    code: "terminal_position_without_result",
                    detail: "position is terminal but canonical result is absent".into(),
                });
            }
            (Some(_), _) => {
                return Err(ProtocolViolation::Invariant {
                    code: "canonical_result_without_terminal_position",
                    detail: "canonical result is present outside terminal position".into(),
                });
            }
            (None, _) => {
                if terminal_event.is_some() {
                    return Err(ProtocolViolation::Invariant {
                        code: "canonical_result_event_without_snapshot_result",
                        detail: "event log is terminal but snapshot result is absent".into(),
                    });
                }
            }
        }
        Ok(())
    }
}

const fn is_planned_node(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Implementation
            | NodeKind::Validation
            | NodeKind::Review
            | NodeKind::CompletionEvaluation
            | NodeKind::Publication
    )
}

const fn protocol_node_order(kind: NodeKind) -> u8 {
    match kind {
        NodeKind::Discovery => 0,
        NodeKind::Planning => 1,
        NodeKind::Implementation => 2,
        NodeKind::Validation => 3,
        NodeKind::ValidationRepair => 4,
        NodeKind::Review => 5,
        NodeKind::CompletionEvaluation => 6,
        NodeKind::Publication => 7,
    }
}

fn transition_proof(from: ProtocolStage, to: ProtocolStage) -> Option<ProofKind> {
    match (from, to) {
        (ProtocolStage::Profiling, ProtocolStage::Discovery) => Some(ProofKind::RepositoryProfile),
        (ProtocolStage::Discovery, ProtocolStage::Planning) => Some(ProofKind::DiscoveryImpactMap),
        (ProtocolStage::Planning, ProtocolStage::Implementation) => Some(ProofKind::PlanAccepted),
        (ProtocolStage::Implementation, ProtocolStage::Validation) => {
            Some(ProofKind::ImplementationBarrier)
        }
        (ProtocolStage::Validation, ProtocolStage::Repair) => Some(ProofKind::ValidationFailure),
        (ProtocolStage::Repair, ProtocolStage::Validation) => {
            Some(ProofKind::ValidationRerunScheduled)
        }
        (ProtocolStage::Validation, ProtocolStage::Review) => {
            Some(ProofKind::RequiredValidationPassed)
        }
        (ProtocolStage::Review, ProtocolStage::Publication) => {
            Some(ProofKind::PublicationEligibility)
        }
        _ => None,
    }
}

fn proof_satisfies_node(kind: NodeKind, proof: ProofKind) -> bool {
    matches!(
        (kind, proof),
        (NodeKind::Discovery, ProofKind::DiscoveryImpactMap)
            | (
                NodeKind::Planning,
                ProofKind::PlanAccepted | ProofKind::NoOpSatisfied
            )
            | (
                NodeKind::Implementation,
                ProofKind::MutationVerified | ProofKind::AlreadySatisfied
            )
            | (NodeKind::Validation, ProofKind::ValidationPassed)
            | (NodeKind::ValidationRepair, ProofKind::RepairVerified)
            | (NodeKind::Review, ProofKind::ReviewCompleted)
            | (
                NodeKind::CompletionEvaluation,
                ProofKind::CompletionEvaluated
            )
            | (NodeKind::Publication, ProofKind::PublicationCompleted)
    )
}

fn reserve_usage(usage: &mut BudgetUsage, admission: &ModelCallAdmission) {
    usage.model_calls_reserved = usage.model_calls_reserved.saturating_add(1);
    usage.cost_micros_reserved = usage
        .cost_micros_reserved
        .saturating_add(admission.reserved_cost_micros);
    usage.duration_ms_reserved = usage
        .duration_ms_reserved
        .saturating_add(admission.duration_allowance_ms);
}

fn release_usage(usage: &mut BudgetUsage, admission: &ModelCallAdmission) {
    usage.model_calls_reserved = usage.model_calls_reserved.saturating_sub(1);
    usage.cost_micros_reserved = usage
        .cost_micros_reserved
        .saturating_sub(admission.reserved_cost_micros);
    usage.duration_ms_reserved = usage
        .duration_ms_reserved
        .saturating_sub(admission.duration_allowance_ms);
}

fn repository_revision_after_event(
    event: &DomainEvent,
    current: &RepositoryRevisionId,
) -> Result<RepositoryRevisionId, ProtocolViolation> {
    match event {
        DomainEvent::Mutation(MutationEvent::MutationVerified { evidence }) => {
            evidence.validate()?;
            if &evidence.repository_revision_before != current {
                return Err(ProtocolViolation::MutationContract {
                    code: "mutation_verification_revision_chain_mismatch",
                });
            }
            Ok(evidence.repository_revision_after.clone())
        }
        DomainEvent::Mutation(MutationEvent::ConvergenceEvaluated { convergence })
            if convergence.repository_revision_after != *current =>
        {
            convergence.validate()?;
            if &convergence.repository_revision != current
                || convergence.repository_drift.as_ref().is_none_or(|drift| {
                    &drift.expected_revision != current
                        || drift.observed_revision != convergence.repository_revision_after
                })
            {
                return Err(ProtocolViolation::MutationContract {
                    code: "mutation_convergence_revision_chain_mismatch",
                });
            }
            Ok(convergence.repository_revision_after.clone())
        }
        DomainEvent::Implementation(ImplementationEvent::TargetContextSuperseded {
            supersession,
        }) if supersession.replacement_repository_revision != *current => {
            if supersession.prepared_repository_revision != *current {
                return Err(ProtocolViolation::ImplementationContract {
                    code: "target_context_supersession_revision_chain_mismatch",
                });
            }
            Ok(supersession.replacement_repository_revision.clone())
        }
        DomainEvent::Review(ReviewEvent::ConvergenceEvaluated {
            convergence:
                ReviewConvergenceV1 {
                    repository_revision,
                    reason:
                        ReviewConvergenceReasonV1::RepositoryDrift {
                            observed_revision, ..
                        },
                    ..
                },
        }) if observed_revision != current => {
            if repository_revision != current {
                return Err(ProtocolViolation::ReviewContract {
                    code: "review_drift_revision_chain_mismatch",
                });
            }
            Ok(observed_revision.clone())
        }
        _ => Ok(current.clone()),
    }
}

fn consume_usage(
    usage: &mut BudgetUsage,
    admission: &ModelCallAdmission,
    actual_cost_micros: u64,
    duration_ms: u64,
) {
    release_usage(usage, admission);
    usage.model_calls_consumed = usage.model_calls_consumed.saturating_add(1);
    usage.cost_micros_consumed = usage
        .cost_micros_consumed
        .saturating_add(actual_cost_micros);
    usage.duration_ms_consumed = usage.duration_ms_consumed.saturating_add(duration_ms);
}

fn ensure_usage_capacity(
    usage: &BudgetUsage,
    budget: &NodeBudgetContract,
    admission: &ModelCallAdmission,
    node_id: Option<NodeId>,
) -> Result<(), ProtocolViolation> {
    if usage
        .model_calls_consumed
        .saturating_add(usage.model_calls_reserved)
        .saturating_add(1)
        > budget.max_model_calls
    {
        return Err(ProtocolViolation::BudgetExceeded {
            node_id,
            dimension: "model_calls",
        });
    }
    if usage
        .cost_micros_consumed
        .saturating_add(usage.cost_micros_reserved)
        .saturating_add(admission.reserved_cost_micros)
        > budget.max_cost_micros
    {
        return Err(ProtocolViolation::BudgetExceeded {
            node_id,
            dimension: "cost_micros",
        });
    }
    if usage
        .duration_ms_consumed
        .saturating_add(usage.duration_ms_reserved)
        .saturating_add(admission.duration_allowance_ms)
        > budget.max_duration_ms
    {
        return Err(ProtocolViolation::BudgetExceeded {
            node_id,
            dimension: "duration_ms",
        });
    }
    Ok(())
}

fn validate_usage_limits(
    usage: &BudgetUsage,
    budget: &NodeBudgetContract,
    node_id: Option<NodeId>,
) -> Result<(), ProtocolViolation> {
    if usage
        .model_calls_consumed
        .saturating_add(usage.model_calls_reserved)
        > budget.max_model_calls
    {
        return Err(ProtocolViolation::BudgetExceeded {
            node_id,
            dimension: "model_calls",
        });
    }
    if usage
        .cost_micros_consumed
        .saturating_add(usage.cost_micros_reserved)
        > budget.max_cost_micros
    {
        return Err(ProtocolViolation::BudgetExceeded {
            node_id,
            dimension: "cost_micros",
        });
    }
    if usage
        .duration_ms_consumed
        .saturating_add(usage.duration_ms_reserved)
        > budget.max_duration_ms
    {
        return Err(ProtocolViolation::BudgetExceeded {
            node_id,
            dimension: "duration_ms",
        });
    }
    Ok(())
}

fn validate_mission_usage_limits(
    usage: &BudgetUsage,
    budget: &MissionBudgetContract,
) -> Result<(), ProtocolViolation> {
    if usage
        .model_calls_consumed
        .saturating_add(usage.model_calls_reserved)
        > budget.max_model_calls
    {
        return Err(ProtocolViolation::BudgetExceeded {
            node_id: None,
            dimension: "model_calls",
        });
    }
    if usage
        .cost_micros_consumed
        .saturating_add(usage.cost_micros_reserved)
        > budget.max_cost_micros
    {
        return Err(ProtocolViolation::BudgetExceeded {
            node_id: None,
            dimension: "cost_micros",
        });
    }
    if usage
        .duration_ms_consumed
        .saturating_add(usage.duration_ms_reserved)
        > budget.max_duration_ms
    {
        return Err(ProtocolViolation::BudgetExceeded {
            node_id: None,
            dimension: "duration_ms",
        });
    }
    Ok(())
}

fn require_terminal_stage(
    actual: ProtocolStage,
    allowed: &[ProtocolStage],
) -> Result<(), ProtocolViolation> {
    if allowed.contains(&actual) {
        Ok(())
    } else {
        Err(ProtocolViolation::TerminalPredicate {
            code: "outcome_not_allowed_from_position",
        })
    }
}

fn node_budget_is_exhausted(node: &ExecutionNode) -> bool {
    node.usage
        .model_calls_consumed
        .saturating_add(node.usage.model_calls_reserved)
        == node.budget.max_model_calls
        || node
            .usage
            .cost_micros_consumed
            .saturating_add(node.usage.cost_micros_reserved)
            == node.budget.max_cost_micros
        || node
            .usage
            .duration_ms_consumed
            .saturating_add(node.usage.duration_ms_reserved)
            == node.budget.max_duration_ms
}
