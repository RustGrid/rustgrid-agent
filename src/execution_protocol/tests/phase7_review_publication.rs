//! Phase 7 review, completion, and publication-eligibility regressions.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use super::phase6_validation::{
    Phase7ReviewEntrySeed, phase7_golden_a_review_entry_seed_with_policy,
    phase7_golden_b_review_entry_seed_with_policy,
};
use super::*;

const RAW_DIFF_SECRET_SENTINEL: &str = "phase7-raw-diff-secret-ec277a09";
const RAW_PR_SECRET_SENTINEL: &str = "phase7-raw-pr-secret-690c95c8";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionFlavor {
    Normal,
    ExternalReview,
}

struct CompletedReviewContract {
    seed: Phase7ReviewEntrySeed,
    policy: FinalizationPolicyV1,
    materialized: MaterializedDiffManifest,
    manifest: DiffManifestV1,
    page_action: PreparedReviewActionV1,
    page_observation: DiffPageReviewObservationV1,
    diff_review: DiffReviewV1,
    completion_action: PreparedReviewActionV1,
    completion: CompletionEvaluationV1,
    authority: PublicationAuthorityObservationV1,
    eligibility: PublicationEligibilityRecord,
    review_proof_id: ProofId,
    completion_proof_id: ProofId,
    eligibility_proof_id: ProofId,
    review_state: ReviewStateV1,
}

struct AuthorityPendingReviewContract {
    seed: Phase7ReviewEntrySeed,
    policy: FinalizationPolicyV1,
    materialized: MaterializedDiffManifest,
    manifest: DiffManifestV1,
    page_action: PreparedReviewActionV1,
    page_observation: DiffPageReviewObservationV1,
    diff_review: DiffReviewV1,
    completion_action: PreparedReviewActionV1,
    completion: CompletionEvaluationV1,
    authority_request: PublicationAuthorityRequestV1,
    review_proof_id: ProofId,
    completion_proof_id: ProofId,
    review_state: ReviewStateV1,
}

fn hash(label: &str) -> String {
    stable_sha256(&["execution-protocol-v1:phase7-test", label])
}

fn accepted_plan(state: &ExecutionState) -> &AcceptedPlan {
    state
        .planning
        .as_ref()
        .and_then(|planning| planning.accepted_plan.as_ref())
        .expect("Phase 7 review entry retains the accepted plan")
}

fn finalization_policy(plan: &AcceptedPlan, flavor: CompletionFlavor) -> FinalizationPolicyV1 {
    let external_review_criteria = if flavor == CompletionFlavor::ExternalReview {
        let criterion_id = plan
            .targets
            .iter()
            .flat_map(|target| target.acceptance_criteria.iter())
            .next()
            .expect("Phase 7 fixture has at least one acceptance criterion")
            .clone();
        BTreeMap::from([(criterion_id, ExternalReviewKindV1::ManualQa)])
    } else {
        BTreeMap::new()
    };
    let requested_mode = if flavor == CompletionFlavor::ExternalReview {
        PublicationModeV1::NormalWithExternalReview
    } else {
        PublicationModeV1::Normal
    };
    let publication = PublicationContractV1::new(
        requested_mode,
        hash("repository-binding"),
        hash("installation-binding"),
        plan.repository_revision.clone(),
        "refs/heads/main".into(),
        "refs/heads/rustgrid/phase7-review".into(),
        Some("1".repeat(40)),
        hash("commit-identity"),
        2,
        1,
        1,
    )
    .expect("trusted publication contract");
    FinalizationPolicyV1::new(
        EvidenceId::new("policy-evidence:phase7-finalization"),
        8,
        4,
        8 * 1024,
        32 * 1024,
        1,
        external_review_criteria,
        publication,
    )
    .expect("trusted finalization policy")
}

fn different_external_review_kind(kind: ExternalReviewKindV1) -> ExternalReviewKindV1 {
    match kind {
        ExternalReviewKindV1::ManualQa => ExternalReviewKindV1::AccessibilityReview,
        ExternalReviewKindV1::AccessibilityReview
        | ExternalReviewKindV1::VisualReview
        | ExternalReviewKindV1::ProductApproval
        | ExternalReviewKindV1::DeploymentEnvironment => ExternalReviewKindV1::ManualQa,
    }
}

fn exact_diff_manifest(
    request: &DiffManifestRequestV1,
    plan: &AcceptedPlan,
    target: &PlannedTargetV1,
    label: &str,
) -> (MaterializedDiffManifest, DiffManifestV1) {
    let new_content_hash = hash(&format!("{label}:new-content"));
    let (path, old_path, change_kind, old_content_hash, new_content_hash) = match &target.operation
    {
        TargetOperation::ModifyExisting {
            expected_content_hash,
        } => (
            target.path.clone(),
            None,
            DiffChangeKindV1::Modified,
            Some(expected_content_hash.clone()),
            Some(new_content_hash),
        ),
        TargetOperation::CreateFile { .. } => (
            target.path.clone(),
            None,
            DiffChangeKindV1::Created,
            None,
            Some(new_content_hash),
        ),
        TargetOperation::DeleteFile {
            expected_content_hash,
        } => (
            target.path.clone(),
            None,
            DiffChangeKindV1::Deleted,
            Some(expected_content_hash.clone()),
            None,
        ),
        TargetOperation::MoveFile {
            destination,
            expected_content_hash,
        } => (
            destination.clone(),
            Some(target.path.clone()),
            DiffChangeKindV1::Renamed,
            Some(expected_content_hash.clone()),
            Some(new_content_hash),
        ),
    };
    let bytes = format!(
        "diff --git a/{path} b/{path}\n-old value\n+new value\n{RAW_DIFF_SECRET_SENTINEL}\n"
    )
    .into_bytes();
    let path_record = DiffPathRecordV1::new(
        path,
        old_path,
        change_kind,
        old_content_hash,
        new_content_hash,
        Some(0o100644),
        Some(0o100644),
        false,
        hex::encode(Sha256::digest(&bytes)),
        u64::try_from(bytes.len()).expect("diff page length fits u64"),
    )
    .expect("exact changed-path record");
    let page = MaterializedDiffPage::new(0, BTreeSet::from([0]), bytes)
        .expect("one exact materialized diff page");
    let persistence = DiffPagePersistenceReceiptV1 {
        page_index: page.index,
        content_hash: page.content_hash(),
        artifact_locator_hash: hash(&format!("{label}:artifact-locator")),
        persistence_receipt_hash: hash(&format!("{label}:persistence-receipt")),
        byte_len: page.byte_len(),
    };
    let materialized = MaterializedDiffManifest::new(
        request,
        request.repository_revision.clone(),
        request.repository_fingerprint.clone(),
        request.repository_fingerprint.clone(),
        vec![path_record],
        vec![page],
    )
    .expect("materialized diff is bound to the current repository fingerprint");
    let manifest =
        DiffManifestV1::from_materialized(request, plan, &materialized, vec![persistence.clone()])
            .expect("durable receipts cover the complete diff");
    assert!(manifest.plan_assessment.is_safe_and_complete());
    assert_eq!(manifest.changed_paths, materialized.paths);
    assert_eq!(manifest.pages.len(), 1);
    assert_eq!(manifest.pages[0].index, persistence.page_index);
    assert_eq!(manifest.pages[0].content_hash, persistence.content_hash);
    assert_eq!(
        manifest.pages[0].artifact_locator_hash,
        persistence.artifact_locator_hash
    );
    assert_eq!(
        manifest.pages[0].persistence_receipt_hash,
        persistence.persistence_receipt_hash
    );
    assert_eq!(manifest.pages[0].byte_len, persistence.byte_len);
    assert_eq!(manifest.total_bytes, persistence.byte_len);
    (materialized, manifest)
}

fn append_next_authoritative(state: &mut ExecutionState, semantic_key: &str) -> DomainEvent {
    let ProtocolDecision::Emit { event } =
        decide(state).expect("authoritative Phase 7 reducer decision")
    else {
        panic!("expected authoritative Phase 7 event for {semantic_key}");
    };
    append(state, semantic_key, event.clone());
    event
}

fn begin_diff_review(
    mut seed: Phase7ReviewEntrySeed,
    label: &str,
) -> (
    Phase7ReviewEntrySeed,
    FinalizationPolicyV1,
    AcceptedPlan,
    DiffManifestRequestV1,
) {
    let policy = seed
        .state
        .finalization_policy
        .clone()
        .expect("trusted Phase 7 finalization policy");
    let plan = accepted_plan(&seed.state).clone();
    assert_eq!(
        append_next_authoritative(
            &mut seed.state,
            &format!("phase7:{label}:review-node-started"),
        ),
        GraphEvent::NodeStarted {
            node_id: seed.review_node_id.clone(),
            attempt: 1,
        }
        .into()
    );
    let DomainEvent::Review(ReviewEvent::DiffManifestRequested { request }) =
        append_next_authoritative(&mut seed.state, &format!("phase7:{label}:diff-requested"))
    else {
        panic!("active review node must request the current complete diff");
    };
    assert_eq!(
        decide(&seed.state).expect("diff adapter request"),
        ProtocolDecision::Perform {
            effect: EffectRequest::Review(ReviewEffectRequest::BuildDiffManifest {
                request: Box::new(request.clone()),
            }),
        }
    );
    (seed, policy, plan, request)
}

fn dispatch_and_consume_review_action(
    state: &mut ExecutionState,
    prepared: &PreparedReviewActionV1,
    label: &str,
) {
    assert_eq!(
        append_next_authoritative(state, &format!("phase7:{label}:admitted")),
        BudgetEvent::ModelCallAdmitted {
            admission: prepared.admission.clone(),
        }
        .into()
    );
    assert_eq!(
        append_next_authoritative(state, &format!("phase7:{label}:reserved")),
        BudgetEvent::ModelCallReserved {
            call_id: prepared.admission.call_id.clone(),
        }
        .into()
    );
    assert_eq!(
        decide(state).expect("reserved review action dispatches"),
        ProtocolDecision::Perform {
            effect: EffectRequest::Review(ReviewEffectRequest::DispatchProvider {
                envelope: Box::new(prepared.envelope.clone()),
            }),
        }
    );
    append(
        state,
        &format!("phase7:{label}:dispatch-started"),
        BudgetEvent::ProviderDispatchStarted {
            call_id: prepared.admission.call_id.clone(),
            payload_hash: prepared.envelope.payload_identity.clone(),
        },
    );
    assert_eq!(
        decide(state).expect("review provider reconciliation wait"),
        ProtocolDecision::Wait {
            reason: WaitReason::ProviderReconciliation {
                call_id: prepared.admission.call_id.clone(),
            },
        }
    );
    append(
        state,
        &format!("phase7:{label}:reconciled"),
        BudgetEvent::ModelCallReconciled {
            call_id: prepared.admission.call_id.clone(),
            result: ModelCallReconciliation::Consumed {
                actual_cost_micros: 75,
                duration_ms: 45,
            },
        },
    );
    assert_eq!(
        decide(state).expect("review observation wait"),
        ProtocolDecision::Wait {
            reason: WaitReason::ReviewObservation {
                action_id: prepared.envelope.action_id.clone(),
            },
        }
    );
}

fn assert_strict_review_envelope(prepared: &PreparedReviewActionV1, expected: ReviewToolV1) {
    assert_eq!(prepared.envelope.tools, BTreeSet::from([expected]));
    assert_eq!(prepared.envelope.tool_definitions.len(), 1);
    let definition = &prepared.envelope.tool_definitions[0];
    assert_eq!(definition.tool, expected);
    assert!(definition.strict);
    assert_eq!(definition.parameters["type"], "object");
    assert_eq!(definition.parameters["additionalProperties"], false);
    definition
        .validate_against(&prepared.context)
        .expect("provider tool definition is the exact strict context schema");
    assert_eq!(
        prepared.envelope.tool_choice,
        ReviewToolChoiceV1::Named { tool: expected }
    );
    assert!(!prepared.envelope.parallel_tool_calls);
    prepared
        .envelope
        .validate_against(&prepared.context)
        .expect("provider envelope is exactly bound to its review context");
}

fn prepare_review_contract_for_authority(
    mut seed: Phase7ReviewEntrySeed,
    flavor: CompletionFlavor,
    label: &str,
) -> AuthorityPendingReviewContract {
    let policy = seed
        .state
        .finalization_policy
        .clone()
        .expect("trusted Phase 7 finalization policy");
    assert_eq!(
        seed.trusted_initial.finalization_policy.as_ref(),
        Some(&policy),
        "trusted bootstrap and snapshot use one finalization authority"
    );
    let plan = accepted_plan(&seed.state).clone();
    let review_state = seed
        .state
        .review
        .clone()
        .expect("Validation to Review initializes review state through the reducer");
    assert_eq!(review_state.review_node_id, seed.review_node_id);
    assert_eq!(review_state.completion_node_id, seed.completion_node_id);
    assert_eq!(review_state.policy_id, policy.policy_id);

    assert_eq!(
        append_next_authoritative(
            &mut seed.state,
            &format!("phase7:{label}:review-node-started"),
        ),
        GraphEvent::NodeStarted {
            node_id: seed.review_node_id.clone(),
            attempt: 1,
        }
        .into()
    );
    let DomainEvent::Review(ReviewEvent::DiffManifestRequested { request }) =
        append_next_authoritative(&mut seed.state, &format!("phase7:{label}:diff-requested"))
    else {
        panic!("active review node must request the current complete diff");
    };
    assert_eq!(
        request.required_validation_proof_id,
        seed.required_validation_proof_id
    );
    assert_eq!(
        decide(&seed.state).expect("diff adapter request"),
        ProtocolDecision::Perform {
            effect: EffectRequest::Review(ReviewEffectRequest::BuildDiffManifest {
                request: Box::new(request.clone()),
            }),
        }
    );
    let (materialized, manifest) = exact_diff_manifest(&request, &plan, &plan.targets[0], label);
    append(
        &mut seed.state,
        &format!("phase7:{label}:diff-recorded"),
        ReviewEvent::DiffManifestRecorded {
            manifest: Box::new(manifest.clone()),
        },
    );
    let DomainEvent::Review(ReviewEvent::ActionPrepared { prepared }) = append_next_authoritative(
        &mut seed.state,
        &format!("phase7:{label}:page-action-prepared"),
    ) else {
        panic!("recorded diff must prepare its exact page-review action");
    };
    let page_action = *prepared;
    let ReviewContextBindingV1::DiffPage {
        manifest_id,
        diff_hash,
        page_id,
        page_index,
        page_content_hash,
        content_address,
        artifact_locator_hash,
        persistence_receipt_hash,
        page_byte_len,
    } = &page_action.context.binding
    else {
        panic!("recorded diff must prepare a page-bound review action");
    };
    let reviewed_page = &manifest.pages[0];
    assert_eq!(manifest_id, &manifest.manifest_id);
    assert_eq!(diff_hash, &manifest.diff_hash);
    assert_eq!(page_id, &reviewed_page.page_id);
    assert_eq!(*page_index, reviewed_page.index);
    assert_eq!(page_content_hash, &reviewed_page.content_hash);
    assert_eq!(content_address, &reviewed_page.content_address);
    assert_eq!(artifact_locator_hash, &reviewed_page.artifact_locator_hash);
    assert_eq!(
        persistence_receipt_hash,
        &reviewed_page.persistence_receipt_hash
    );
    assert_eq!(*page_byte_len, reviewed_page.byte_len);
    assert_strict_review_envelope(&page_action, ReviewToolV1::RecordDiffReview);
    dispatch_and_consume_review_action(
        &mut seed.state,
        &page_action,
        &format!("{label}:page-review"),
    );
    let page_observation = DiffPageReviewObservationV1::new(&page_action, &manifest, Vec::new())
        .expect("provider observation accepts the exact reviewed page");
    append(
        &mut seed.state,
        &format!("phase7:{label}:page-reviewed"),
        ReviewEvent::DiffPageReviewed {
            observation: Box::new(page_observation.clone()),
        },
    );
    let DomainEvent::Review(ReviewEvent::DiffReviewRecorded { review }) = append_next_authoritative(
        &mut seed.state,
        &format!("phase7:{label}:diff-review-recorded"),
    ) else {
        panic!("all page observations must aggregate to one diff review");
    };
    let diff_review = *review;
    assert_eq!(diff_review.disposition, DiffReviewDispositionV1::Accepted);
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: review_proof,
    }) = append_next_authoritative(&mut seed.state, &format!("phase7:{label}:review-proof"))
    else {
        panic!("accepted complete diff review must produce its proof");
    };
    assert_eq!(review_proof.kind, ProofKind::ReviewCompleted);
    assert_eq!(
        append_next_authoritative(
            &mut seed.state,
            &format!("phase7:{label}:review-node-succeeded"),
        ),
        GraphEvent::NodeSucceeded {
            node_id: seed.review_node_id.clone(),
            proof_id: review_proof.id.clone(),
        }
        .into()
    );
    assert_eq!(
        append_next_authoritative(
            &mut seed.state,
            &format!("phase7:{label}:completion-node-started"),
        ),
        GraphEvent::NodeStarted {
            node_id: seed.completion_node_id.clone(),
            attempt: 1,
        }
        .into()
    );
    let DomainEvent::Review(ReviewEvent::ActionPrepared { prepared }) = append_next_authoritative(
        &mut seed.state,
        &format!("phase7:{label}:completion-action-prepared"),
    ) else {
        panic!("completion node must prepare its exact provider action");
    };
    let completion_action = *prepared;
    assert!(matches!(
        completion_action.context.binding,
        ReviewContextBindingV1::Completion { .. }
    ));
    assert_strict_review_envelope(&completion_action, ReviewToolV1::RecordCompletionEvaluation);
    dispatch_and_consume_review_action(
        &mut seed.state,
        &completion_action,
        &format!("{label}:completion"),
    );
    let supporting_evidence_ids = seed
        .current_validation_evidence_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    assert!(!supporting_evidence_ids.is_empty());
    let criteria = completion_action
        .context
        .criterion_ids
        .iter()
        .cloned()
        .map(|criterion_id| {
            if let Some(kind) = policy.external_review_criteria.get(&criterion_id) {
                let invalid_satisfied = CriterionCompletionStatusV1::Satisfied {
                    supporting_evidence_ids: supporting_evidence_ids.clone(),
                };
                assert_eq!(
                    CriterionCompletionEvaluationV1::new(
                        criterion_id.clone(),
                        invalid_satisfied.clone(),
                        &completion_action.context,
                        &policy,
                        &plan,
                        &manifest,
                        &review_state.ancestry,
                    )
                    .expect_err("a policy-classified external criterion cannot be satisfied")
                    .code(),
                    "completion_external_review_required"
                );
                assert_eq!(
                    CriterionCompletionEvaluationV1::new(
                        criterion_id.clone(),
                        CriterionCompletionStatusV1::ExternalReviewRequired {
                            kind: different_external_review_kind(*kind),
                            requirement_code: "manual_qa_required".into(),
                            detail_hash: hash("manual-qa-requirement"),
                        },
                        &completion_action.context,
                        &policy,
                        &plan,
                        &manifest,
                        &review_state.ancestry,
                    )
                    .expect_err("external review must use the policy's exact mapped kind")
                    .code(),
                    "completion_external_review_not_authorized"
                );
                let evaluation = CriterionCompletionEvaluationV1::new(
                    criterion_id.clone(),
                    CriterionCompletionStatusV1::ExternalReviewRequired {
                        kind: *kind,
                        requirement_code: "manual_qa_required".into(),
                        detail_hash: hash("manual-qa-requirement"),
                    },
                    &completion_action.context,
                    &policy,
                    &plan,
                    &manifest,
                    &review_state.ancestry,
                )
                .expect("criterion completion uses the policy's exact external review kind");
                let mut forged = evaluation.clone();
                forged.status = invalid_satisfied;
                assert_eq!(
                    forged
                        .validate(
                            &completion_action.context,
                            &policy,
                            &plan,
                            &manifest,
                            &review_state.ancestry,
                        )
                        .expect_err("revalidation rejects a forged satisfied external criterion")
                        .code(),
                    "completion_external_review_required"
                );
                return (criterion_id, evaluation);
            }
            let status = CriterionCompletionStatusV1::Satisfied {
                supporting_evidence_ids: supporting_evidence_ids.clone(),
            };
            let evaluation = CriterionCompletionEvaluationV1::new(
                criterion_id.clone(),
                status,
                &completion_action.context,
                &policy,
                &plan,
                &manifest,
                &review_state.ancestry,
            )
            .expect("criterion completion is policy authorized and evidence bound");
            (criterion_id, evaluation)
        })
        .collect::<BTreeMap<_, _>>();
    let completion = CompletionEvaluationV1::new(
        &completion_action,
        &manifest,
        &diff_review,
        &policy,
        &plan,
        &review_state.ancestry,
        criteria,
    )
    .expect("complete criterion evaluation");
    let expected_disposition = match flavor {
        CompletionFlavor::Normal => CompletionDispositionV1::Complete,
        CompletionFlavor::ExternalReview => CompletionDispositionV1::CompletePendingExternalReview,
    };
    assert_eq!(completion.disposition, expected_disposition);
    assert!(
        completion
            .disposition
            .permits(policy.publication.requested_mode)
    );
    append(
        &mut seed.state,
        &format!("phase7:{label}:completion-recorded"),
        ReviewEvent::CompletionEvaluationRecorded {
            evaluation: Box::new(completion.clone()),
        },
    );
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: completion_proof,
    }) = append_next_authoritative(&mut seed.state, &format!("phase7:{label}:completion-proof"))
    else {
        panic!("resolved completion must produce its exact proof");
    };
    assert_eq!(completion_proof.kind, ProofKind::CompletionEvaluated);
    assert_eq!(
        append_next_authoritative(
            &mut seed.state,
            &format!("phase7:{label}:completion-node-succeeded"),
        ),
        GraphEvent::NodeSucceeded {
            node_id: seed.completion_node_id.clone(),
            proof_id: completion_proof.id.clone(),
        }
        .into()
    );
    let DomainEvent::Review(ReviewEvent::PublicationAuthorityRequested {
        request: authority_request,
    }) = append_next_authoritative(
        &mut seed.state,
        &format!("phase7:{label}:authority-requested"),
    )
    else {
        panic!("completed review must request fresh publication authority");
    };
    assert_eq!(
        decide(&seed.state).expect("publication authority adapter request"),
        ProtocolDecision::Perform {
            effect: EffectRequest::Review(ReviewEffectRequest::ObservePublicationAuthority {
                request: Box::new(authority_request.clone()),
            }),
        }
    );
    let review_state = seed
        .state
        .review
        .clone()
        .expect("completed review evidence remains available while authority is pending");
    review_state
        .validate(&plan, &policy)
        .expect("authority-pending review state remains internally valid");
    AuthorityPendingReviewContract {
        seed,
        policy,
        materialized,
        manifest,
        page_action,
        page_observation,
        diff_review,
        completion_action,
        completion,
        authority_request,
        review_proof_id: review_proof.id,
        completion_proof_id: completion_proof.id,
        review_state,
    }
}

fn complete_review_contract(
    seed: Phase7ReviewEntrySeed,
    flavor: CompletionFlavor,
    label: &str,
    exercise_local_head_denial: bool,
) -> CompletedReviewContract {
    let AuthorityPendingReviewContract {
        mut seed,
        policy,
        materialized,
        manifest,
        page_action,
        page_observation,
        diff_review,
        completion_action,
        completion,
        authority_request,
        review_proof_id,
        completion_proof_id,
        review_state: _,
    } = prepare_review_contract_for_authority(seed, flavor, label);
    let plan = accepted_plan(&seed.state).clone();
    let publication = &policy.publication;
    if exercise_local_head_denial {
        let forged_local_head_authority = PublicationAuthorityObservationV1::new(
            &authority_request,
            "9".repeat(40),
            "3".repeat(40),
            publication.repository_binding_hash.clone(),
            publication.installation_binding_hash.clone(),
            publication.base_repository_revision.clone(),
            publication.base_ref.clone(),
            publication.head_branch.clone(),
            publication.expected_remote_head.clone(),
            publication.expected_remote_head.clone(),
            hash("forged-local-head-lease-epoch"),
            true,
            true,
        )
        .expect(
            "well-shaped authority can report a local head distinct from the signed remote head",
        );
        assert!(forged_local_head_authority.remote_head_unchanged);
        let mut forged_local_head_state = seed.state.clone();
        append(
            &mut forged_local_head_state,
            &format!("phase7:{label}:forged-local-head-authority"),
            ReviewEvent::PublicationAuthorityObserved {
                observation: forged_local_head_authority,
            },
        );
        let DomainEvent::Review(ReviewEvent::PublicationEligibilityEvaluated {
            eligibility: denied,
        }) = append_next_authoritative(
            &mut forged_local_head_state,
            &format!("phase7:{label}:forged-local-head-eligibility"),
        )
        else {
            panic!("forged local head must still receive a canonical denied eligibility record");
        };
        assert_eq!(
            denied
                .predicates
                .get(&PublicationPredicateV1::RemoteHeadUnchanged),
            Some(&PublicationPredicateResultV1::Failed {
                code: "publication_remote_head_moved".into(),
            })
        );
        assert!(matches!(
            &denied.disposition,
            PublicationEligibilityDispositionV1::Denied { failed_predicates }
                if failed_predicates.contains(&PublicationPredicateV1::RemoteHeadUnchanged)
        ));
        assert!(!denied.is_granted());
        assert_eq!(forged_local_head_state.stage(), ProtocolStage::Review);
        assert!(forged_local_head_state.publication.is_none());
        let DomainEvent::Review(ReviewEvent::ConvergenceEvaluated { convergence }) =
            append_next_authoritative(
                &mut forged_local_head_state,
                &format!("phase7:{label}:forged-local-head-convergence"),
            )
        else {
            panic!("denied remote-head predicate must converge before Publication");
        };
        assert_eq!(
            convergence.reason,
            ReviewConvergenceReasonV1::PublicationEligibilityDenied {
                eligibility_id: denied.eligibility_id.clone(),
            }
        );
        assert!(forged_local_head_state.publication.is_none());
        let ProtocolDecision::Finish { result } = decide(&forged_local_head_state).unwrap() else {
            panic!("remote-head eligibility denial has one canonical terminal result");
        };
        assert_eq!(result.mission.outcome(), MissionOutcomeV1::ValidationFailed);
        assert_eq!(result.process_health, ProcessHealth::Healthy);
        assert_eq!(result.reason_code, "publication_remote_head_moved");
        let blocker = result.mission.first_fatal_blocker().unwrap();
        assert_eq!(blocker.category, "validation");
        assert!(blocker.node_id.is_none());
        append(
            &mut forged_local_head_state,
            &format!("phase7:{label}:forged-local-head-terminal"),
            TerminalEvent::CanonicalResultRecorded {
                result: result.clone(),
            },
        );
        assert_eq!(
            decide(&forged_local_head_state).unwrap(),
            ProtocolDecision::Finish { result }
        );
        assert!(forged_local_head_state.publication.is_none());
        assert_state_replays(&seed.trusted_initial, &forged_local_head_state);
    }

    let authority = PublicationAuthorityObservationV1::new(
        &authority_request,
        publication
            .expected_remote_head
            .clone()
            .expect("fixture has an exact remote parent"),
        "3".repeat(40),
        publication.repository_binding_hash.clone(),
        publication.installation_binding_hash.clone(),
        publication.base_repository_revision.clone(),
        publication.base_ref.clone(),
        publication.head_branch.clone(),
        publication.expected_remote_head.clone(),
        publication.expected_remote_head.clone(),
        hash("lease-epoch"),
        true,
        true,
    )
    .expect("current signed repository, remote, cancellation, and lease authority");
    append(
        &mut seed.state,
        &format!("phase7:{label}:authority-observed"),
        ReviewEvent::PublicationAuthorityObserved {
            observation: authority.clone(),
        },
    );
    let DomainEvent::Review(ReviewEvent::PublicationEligibilityEvaluated { eligibility }) =
        append_next_authoritative(
            &mut seed.state,
            &format!("phase7:{label}:eligibility-evaluated"),
        )
    else {
        panic!("fresh authority and complete review must evaluate every publication predicate");
    };
    let eligibility = *eligibility;
    assert!(eligibility.is_granted());
    eligibility
        .validate_for_publication(&policy.publication, &seed.state.repository_revision)
        .expect("granted eligibility is exact publication authority");
    assert_eq!(eligibility.review_proof_id, review_proof_id);
    assert_eq!(eligibility.completion_proof_id, completion_proof_id);
    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded {
        proof: eligibility_proof,
    }) = append_next_authoritative(
        &mut seed.state,
        &format!("phase7:{label}:eligibility-proof"),
    )
    else {
        panic!("granted eligibility must produce its exact transition proof");
    };
    assert_eq!(eligibility_proof.kind, ProofKind::PublicationEligibility);
    assert_eq!(
        append_next_authoritative(
            &mut seed.state,
            &format!("phase7:{label}:publication-transition"),
        ),
        LifecycleEvent::PositionAdvanced {
            from: ProtocolStage::Review,
            to: ProtocolStage::Publication,
            proof_id: eligibility_proof.id.clone(),
        }
        .into()
    );
    assert_eq!(seed.state.stage(), ProtocolStage::Publication);
    assert!(seed.state.publication.is_some());
    assert!(
        seed.state
            .event_log
            .iter()
            .all(|stored| !matches!(stored.envelope.payload, DomainEvent::Publication(_)))
    );
    let review_state = seed
        .state
        .review
        .clone()
        .expect("review evidence remains available in Publication");
    review_state
        .validate(&plan, &policy)
        .expect("completed review state remains internally valid");

    CompletedReviewContract {
        seed,
        policy,
        materialized,
        manifest,
        page_action,
        page_observation,
        diff_review,
        completion_action,
        completion,
        authority,
        eligibility,
        review_proof_id,
        completion_proof_id,
        eligibility_proof_id: eligibility_proof.id,
        review_state,
    }
}

fn expected_clean_ancestry(seed: &Phase7ReviewEntrySeed) -> Vec<ProofId> {
    let mut expected = vec![seed.implementation_barrier_proof_id.clone()];
    let mut current_validation = seed.current_validation_pass_proof_ids.clone();
    current_validation.sort();
    expected.extend(current_validation);
    expected.push(seed.required_validation_proof_id.clone());
    expected
}

fn expected_repaired_ancestry(seed: &Phase7ReviewEntrySeed) -> Vec<ProofId> {
    let repair = seed
        .repair_ancestry
        .as_ref()
        .expect("Golden B retains exact repair ancestry IDs");
    let mut expected = vec![
        seed.implementation_barrier_proof_id.clone(),
        repair.validation_failure_proof_id.clone(),
        repair.repair_eligibility_proof_id.clone(),
        repair.repair_mutation_proof_id.clone(),
        repair.repair_verification_proof_id.clone(),
        repair.validation_rerun_proof_id.clone(),
    ];
    let mut current_validation = seed.current_validation_pass_proof_ids.clone();
    current_validation.sort();
    expected.extend(current_validation);
    expected.push(seed.required_validation_proof_id.clone());
    expected
}

fn assert_seed_replays(seed: &Phase7ReviewEntrySeed) {
    assert_state_replays(&seed.trusted_initial, &seed.state);
}

fn assert_state_replays(trusted_initial: &ExecutionState, state: &ExecutionState) {
    let serialized = serde_json::to_vec(state).expect("execution snapshot serializes");
    let decoded =
        serde_json::from_slice(&serialized).expect("execution snapshot strictly deserializes");
    let restored = InMemoryEventStore::restore(trusted_initial.clone(), decoded)
        .expect("policy-bound Review entry restores from the trusted bootstrap")
        .into_state();
    assert_eq!(&restored, state);
}

#[test]
fn golden_a_review_completion_and_normal_eligibility_bind_current_evidence() {
    let seed = phase7_golden_a_review_entry_seed_with_policy(|plan| {
        finalization_policy(plan, CompletionFlavor::Normal)
    });
    assert_seed_replays(&seed);
    let expected_ancestry = expected_clean_ancestry(&seed);
    let completed = complete_review_contract(seed, CompletionFlavor::Normal, "golden-a", true);
    assert_seed_replays(&completed.seed);

    assert_eq!(
        completed.review_state.ancestry.ordered_revision_proof_ids,
        expected_ancestry
    );
    assert_eq!(
        completed.review_state.ancestry.repository_revision,
        completed.seed.state.repository_revision
    );
    assert_eq!(
        completed.manifest.required_validation_proof_id,
        completed.seed.required_validation_proof_id
    );
    assert_eq!(completed.page_action.envelope.retry_index, 1);
    assert_eq!(
        completed.page_action.admission.call_id,
        completed.page_observation.call_id
    );
    assert_eq!(
        completed.diff_review.ordered_page_review_ids.as_slice(),
        std::slice::from_ref(&completed.page_observation.observation_id)
    );
    assert_eq!(
        completed.completion.disposition,
        CompletionDispositionV1::Complete
    );
    assert!(completed.authority.remote_head_unchanged);
    assert!(completed.eligibility.is_granted());
    assert_eq!(
        completed.eligibility.review_proof_id,
        completed.review_proof_id
    );
    assert_eq!(
        completed.eligibility.completion_proof_id,
        completed.completion_proof_id
    );
    assert_eq!(
        completed.seed.state.latest_transition_proof,
        Some(completed.eligibility_proof_id.clone())
    );
    assert!(completed.seed.state.publication.is_some());
}

#[test]
fn diff_page_authority_rejects_raw_bytes_receipt_and_coverage_tampering() {
    let seed = phase7_golden_a_review_entry_seed_with_policy(|plan| {
        finalization_policy(plan, CompletionFlavor::Normal)
    });
    let (_seed, _policy, plan, request) = begin_diff_review(seed, "diff-page-tamper");
    let (materialized, manifest) =
        exact_diff_manifest(&request, &plan, &plan.targets[0], "diff-page-tamper");
    let original_bytes = materialized.pages[0].bytes().to_vec();

    let mut different_raw_bytes = materialized.clone();
    different_raw_bytes.pages[0] = MaterializedDiffPage::new(
        0,
        BTreeSet::from([0]),
        b"different raw patch bytes with no authority from the planned path".to_vec(),
    )
    .unwrap();
    let mut multi_index_coverage = materialized.clone();
    multi_index_coverage.pages[0] =
        MaterializedDiffPage::new(0, BTreeSet::from([0, 1]), original_bytes.clone()).unwrap();
    let mut path_page_cardinality = materialized.clone();
    path_page_cardinality
        .pages
        .push(MaterializedDiffPage::new(1, BTreeSet::from([0]), original_bytes).unwrap());
    for (case, forged) in [
        ("different_raw_bytes", different_raw_bytes),
        ("multi_index_coverage", multi_index_coverage),
        ("path_page_cardinality", path_page_cardinality),
    ] {
        assert_eq!(
            forged.paths, materialized.paths,
            "{case} must not obtain authority by retaining safe planned-path metadata"
        );
        assert_eq!(
            forged
                .validate_against(&request)
                .expect_err("materialized raw-page tampering must fail closed")
                .code(),
            "materialized_diff_manifest_invalid",
            "unexpected materialized rejection for {case}"
        );
    }

    let mut different_content_hash = manifest.clone();
    different_content_hash.pages[0].content_hash = hex::encode(Sha256::digest(
        b"different raw patch bytes with no authority from the planned path",
    ));
    let mut different_byte_len = manifest.clone();
    different_byte_len.pages[0].byte_len = different_byte_len.pages[0].byte_len.saturating_add(1);
    let mut durable_multi_index_coverage = manifest.clone();
    durable_multi_index_coverage.pages[0]
        .covered_path_indexes
        .insert(1);
    let mut durable_cardinality = manifest.clone();
    durable_cardinality.pages.push(manifest.pages[0].clone());
    for (case, forged) in [
        ("different_content_hash", different_content_hash),
        ("different_byte_len", different_byte_len),
        ("multi_index_coverage", durable_multi_index_coverage),
        ("path_page_cardinality", durable_cardinality),
    ] {
        assert_eq!(
            forged.changed_paths, manifest.changed_paths,
            "{case} must not obtain durable authority from unchanged safe paths"
        );
        assert_eq!(
            forged
                .validate_against(&request, &plan)
                .expect_err("durable page-receipt tampering must fail closed")
                .code(),
            "diff_manifest_invalid",
            "unexpected durable rejection for {case}"
        );
    }
}

#[test]
fn diff_manifest_repository_drift_requires_persisted_failure_and_adopts_exact_revision() {
    let seed = phase7_golden_a_review_entry_seed_with_policy(|plan| {
        finalization_policy(plan, CompletionFlavor::Normal)
    });
    let (mut seed, policy, _plan, request) = begin_diff_review(seed, "diff-drift");
    let revision_one = seed.state.repository_revision.clone();
    let revision_two = RepositoryRevisionId::new("repository-revision:phase7-drift-r2");
    let failure = DiffManifestEffectFailureV1::new(
        &request,
        DiffManifestEffectFailureReasonV1::RepositoryDrift {
            observed_revision: revision_two.clone(),
            observed_repository_fingerprint: hash("phase7-drift-r2-fingerprint"),
        },
    )
    .expect("repository movement is bound to the persisted R1 diff request");
    assert_eq!(failure.repository_revision, revision_one);

    let alternate_failure = DiffManifestEffectFailureV1::new(
        &request,
        DiffManifestEffectFailureReasonV1::RepositoryDrift {
            observed_revision: revision_two.clone(),
            observed_repository_fingerprint: hash("phase7-drift-r2-alternate-fingerprint"),
        },
    )
    .expect("a different valid observed fingerprint has a distinct failure identity");
    assert_ne!(alternate_failure.failure_id, failure.failure_id);
    assert_ne!(alternate_failure.failure_hash, failure.failure_hash);
    let alternate_convergence = ReviewConvergenceV1::new(
        revision_one.clone(),
        policy.policy_id.clone(),
        alternate_failure.convergence_reason(),
    )
    .unwrap();

    let direct_convergence = ReviewConvergenceV1::new(
        revision_one.clone(),
        policy.policy_id.clone(),
        failure.convergence_reason(),
    )
    .expect("effect-derived convergence is structurally valid");
    assert_ne!(
        alternate_convergence.convergence_id,
        direct_convergence.convergence_id
    );
    assert_ne!(
        alternate_convergence.convergence_hash,
        direct_convergence.convergence_hash
    );
    let before_direct = seed.state.clone();
    let direct_event = envelope(
        &seed.state,
        "phase7:diff-drift:direct-convergence",
        ReviewEvent::ConvergenceEvaluated {
            convergence: direct_convergence,
        },
    );
    assert!(matches!(
        seed.state
            .append_event(direct_event)
            .expect_err("effect-derived convergence requires its persisted failure"),
        ProtocolViolation::ReviewContract {
            code: "review_convergence_not_authoritative"
        }
    ));
    assert_eq!(seed.state, before_direct);

    let mut tampered = failure.clone();
    tampered.request_hash = hash("forged-diff-request-binding");
    let before_tampered = seed.state.clone();
    let tampered_event = envelope(
        &seed.state,
        "phase7:diff-drift:tampered-failure",
        ReviewEvent::DiffManifestBuildFailed { failure: tampered },
    );
    assert!(matches!(
        seed.state
            .append_event(tampered_event)
            .expect_err("tampered failure cannot claim the persisted effect"),
        ProtocolViolation::ReviewContract {
            code: "diff_manifest_effect_failure_invalid"
        }
    ));
    assert_eq!(seed.state, before_tampered);

    append(
        &mut seed.state,
        "phase7:diff-drift:failure-recorded",
        ReviewEvent::DiffManifestBuildFailed {
            failure: failure.clone(),
        },
    );
    assert_eq!(seed.state.repository_revision, revision_one);
    assert_eq!(
        seed.state
            .review
            .as_ref()
            .and_then(|review| review.diff_manifest_failure.as_ref()),
        Some(&failure)
    );
    let DomainEvent::Review(ReviewEvent::ConvergenceEvaluated { convergence }) =
        append_next_authoritative(&mut seed.state, "phase7:diff-drift:convergence")
    else {
        panic!("persisted drift failure must project its exact convergence");
    };
    assert_eq!(convergence.repository_revision, revision_one);
    assert_eq!(
        convergence.reason,
        ReviewConvergenceReasonV1::RepositoryDrift {
            failure_id: failure.failure_id.clone(),
            failure_hash: failure.failure_hash.clone(),
            observed_revision: revision_two.clone(),
        }
    );
    assert_eq!(seed.state.repository_revision, revision_two);
    assert_eq!(
        seed.state.review.as_ref().unwrap().repository_revision,
        revision_one,
        "review evidence remains bound to the R1 request while the aggregate adopts R2"
    );

    let DomainEvent::Graph(GraphEvent::NodeFailed {
        node_id,
        failure_revision_id,
        terminal,
    }) = append_next_authoritative(&mut seed.state, "phase7:diff-drift:review-node-failed")
    else {
        panic!("drift convergence must fail the exact active Review node");
    };
    assert_eq!(node_id, seed.review_node_id);
    assert!(terminal);
    assert!(matches!(
        seed.state.nodes[&node_id].state,
        NodeState::FailedTerminal {
            failure_revision_id: ref actual,
        } if actual == &failure_revision_id
    ));
    let ProtocolDecision::Finish { result } = decide(&seed.state).unwrap() else {
        panic!("review drift has one canonical terminal result");
    };
    assert_eq!(result.mission.outcome(), MissionOutcomeV1::ValidationFailed);
    assert_eq!(result.process_health, ProcessHealth::Healthy);
    assert_eq!(result.reason_code, "review_repository_drift");
    assert_eq!(result.repository_revision, revision_two);
    let blocker = result.mission.first_fatal_blocker().unwrap();
    assert_eq!(blocker.category, "validation");
    assert!(blocker.node_id.is_none());
    append(
        &mut seed.state,
        "phase7:diff-drift:terminal",
        TerminalEvent::CanonicalResultRecorded {
            result: result.clone(),
        },
    );
    assert_eq!(
        decide(&seed.state).unwrap(),
        ProtocolDecision::Finish { result }
    );
    assert_seed_replays(&seed);
}

#[test]
fn consumed_rejected_review_call_converges_as_provider_protocol_exhaustion() {
    let seed = phase7_golden_a_review_entry_seed_with_policy(|plan| {
        finalization_policy(plan, CompletionFlavor::Normal)
    });
    let (mut seed, _policy, plan, request) = begin_diff_review(seed, "provider-protocol");
    let (_, manifest) = exact_diff_manifest(&request, &plan, &plan.targets[0], "provider-protocol");
    append(
        &mut seed.state,
        "phase7:provider-protocol:diff-recorded",
        ReviewEvent::DiffManifestRecorded {
            manifest: Box::new(manifest),
        },
    );
    let DomainEvent::Review(ReviewEvent::ActionPrepared { prepared }) =
        append_next_authoritative(&mut seed.state, "phase7:provider-protocol:action-prepared")
    else {
        panic!("the first and only signed Review call must be prepared");
    };
    let prepared = *prepared;
    dispatch_and_consume_review_action(&mut seed.state, &prepared, "provider-protocol");
    append(
        &mut seed.state,
        "phase7:provider-protocol:action-rejected",
        ReviewEvent::ActionRejected {
            action_id: prepared.envelope.action_id.clone(),
            reason: ReviewActionRejectionReasonV1::ProviderProtocolViolation,
        },
    );
    let prepared_count = seed
        .state
        .event_log
        .iter()
        .filter(|stored| {
            matches!(
                stored.envelope.payload,
                DomainEvent::Review(ReviewEvent::ActionPrepared { .. })
            )
        })
        .count();
    let dispatch_count = seed
        .state
        .event_log
        .iter()
        .filter(|stored| {
            matches!(
                &stored.envelope.payload,
                DomainEvent::Budget(BudgetEvent::ProviderDispatchStarted { call_id, .. })
                    if call_id == &prepared.admission.call_id
            )
        })
        .count();
    let ProtocolDecision::Emit {
        event: DomainEvent::Review(ReviewEvent::ConvergenceEvaluated { convergence }),
    } = decide(&seed.state).expect("rejected consumed call exhausts its signed node budget")
    else {
        panic!("provider-protocol exhaustion must converge before another action or dispatch");
    };
    assert_eq!(
        convergence.reason,
        ReviewConvergenceReasonV1::ProviderProtocolExhausted {
            node_id: seed.review_node_id.clone(),
        }
    );
    append(
        &mut seed.state,
        "phase7:provider-protocol:convergence",
        ReviewEvent::ConvergenceEvaluated { convergence },
    );
    assert_eq!(
        seed.state
            .event_log
            .iter()
            .filter(|stored| matches!(
                stored.envelope.payload,
                DomainEvent::Review(ReviewEvent::ActionPrepared { .. })
            ))
            .count(),
        prepared_count
    );
    assert_eq!(
        seed.state
            .event_log
            .iter()
            .filter(|stored| matches!(
                &stored.envelope.payload,
                DomainEvent::Budget(BudgetEvent::ProviderDispatchStarted { call_id, .. })
                    if call_id == &prepared.admission.call_id
            ))
            .count(),
        dispatch_count
    );
    let DomainEvent::Graph(GraphEvent::NodeFailed {
        node_id, terminal, ..
    }) = append_next_authoritative(
        &mut seed.state,
        "phase7:provider-protocol:review-node-failed",
    )
    else {
        panic!("provider-protocol convergence must fail the exact Review owner");
    };
    assert_eq!(node_id, seed.review_node_id);
    assert!(terminal);
    let ProtocolDecision::Finish { result } = decide(&seed.state).unwrap() else {
        panic!("provider-protocol exhaustion has one canonical terminal result");
    };
    assert_eq!(
        result.mission.outcome(),
        MissionOutcomeV1::InfrastructureFailed
    );
    assert_eq!(
        result.process_health,
        ProcessHealth::Failed {
            code: "review_provider_protocol_exhausted".into(),
        }
    );
    assert_eq!(result.reason_code, "review_provider_protocol_exhausted");
    append(
        &mut seed.state,
        "phase7:provider-protocol:terminal",
        TerminalEvent::CanonicalResultRecorded {
            result: result.clone(),
        },
    );
    assert_eq!(
        decide(&seed.state).unwrap(),
        ProtocolDecision::Finish { result }
    );
    assert_seed_replays(&seed);
}

#[test]
fn publication_authority_failure_converges_before_publication_can_start() {
    let seed = phase7_golden_a_review_entry_seed_with_policy(|plan| {
        finalization_policy(plan, CompletionFlavor::Normal)
    });
    let mut pending =
        prepare_review_contract_for_authority(seed, CompletionFlavor::Normal, "authority-failure");
    assert!(pending.seed.state.publication.is_none());
    assert!(
        pending
            .seed
            .state
            .event_log
            .iter()
            .all(|stored| !matches!(stored.envelope.payload, DomainEvent::Publication(_)))
    );
    let failure = PublicationAuthorityEffectFailureV1::new(
        &pending.authority_request,
        PublicationAuthorityEffectFailureReasonV1::AuthorityUnavailable {
            safe_code: "github_authority_unavailable".into(),
        },
    )
    .expect("authority failure is bound to the exact persisted observation request");
    append(
        &mut pending.seed.state,
        "phase7:authority-failure:recorded",
        ReviewEvent::PublicationAuthorityObservationFailed {
            failure: failure.clone(),
        },
    );
    assert_eq!(
        pending
            .seed
            .state
            .review
            .as_ref()
            .and_then(|review| review.authority_failure.as_ref()),
        Some(&failure)
    );
    let DomainEvent::Review(ReviewEvent::ConvergenceEvaluated { convergence }) =
        append_next_authoritative(
            &mut pending.seed.state,
            "phase7:authority-failure:convergence",
        )
    else {
        panic!("persisted authority failure must project exact convergence");
    };
    assert_eq!(
        convergence.reason,
        ReviewConvergenceReasonV1::PublicationAuthorityUnavailable {
            failure_id: failure.failure_id.clone(),
            failure_hash: failure.failure_hash.clone(),
            safe_code: "github_authority_unavailable".into(),
        }
    );
    assert!(pending.seed.state.publication.is_none());
    let ProtocolDecision::Finish { result } = decide(&pending.seed.state).unwrap() else {
        panic!("authority failure has one canonical terminal result");
    };
    assert_eq!(
        result.mission.outcome(),
        MissionOutcomeV1::InfrastructureFailed
    );
    assert_eq!(
        result.process_health,
        ProcessHealth::Failed {
            code: "publication_authority_unavailable".into(),
        }
    );
    assert_eq!(result.reason_code, "publication_authority_unavailable");
    let blocker = result.mission.first_fatal_blocker().unwrap();
    assert_eq!(blocker.category, "infrastructure");
    assert!(blocker.node_id.is_none());
    append(
        &mut pending.seed.state,
        "phase7:authority-failure:terminal",
        TerminalEvent::CanonicalResultRecorded {
            result: result.clone(),
        },
    );
    assert_eq!(
        decide(&pending.seed.state).unwrap(),
        ProtocolDecision::Finish { result }
    );
    assert!(pending.seed.state.publication.is_none());
    assert!(
        pending
            .seed
            .state
            .event_log
            .iter()
            .all(|stored| !matches!(stored.envelope.payload, DomainEvent::Publication(_)))
    );
    assert_seed_replays(&pending.seed);
}

#[test]
fn repaired_golden_b_preserves_full_ancestry_external_review_and_strict_redaction() {
    let seed = phase7_golden_b_review_entry_seed_with_policy(|plan| {
        finalization_policy(plan, CompletionFlavor::ExternalReview)
    });
    let expected_ancestry = expected_repaired_ancestry(&seed);
    let repair = seed.repair_ancestry.as_ref().unwrap();
    assert_ne!(
        seed.state.proofs[&seed.implementation_barrier_proof_id].repository_revision,
        seed.state.repository_revision,
        "repair advances the repository without minting a replacement implementation barrier"
    );
    assert!(
        repair
            .invalidated_validation_evidence_ids
            .contains(&repair.failed_validation_evidence_id)
    );
    let failure_proof = &seed.state.proofs[&repair.validation_failure_proof_id];
    assert!(
        failure_proof
            .related_evidence_ids
            .contains(&EvidenceId::new(
                repair.failed_validation_evidence_id.as_str()
            ))
    );
    assert_eq!(
        seed.state.proofs[&repair.repair_eligibility_proof_id]
            .related_proof_ids
            .as_slice(),
        std::slice::from_ref(&repair.validation_failure_proof_id)
    );
    assert!(
        seed.state.proofs[&repair.repair_mutation_proof_id]
            .related_evidence_ids
            .contains(&repair.repair_mutation_evidence_id)
    );
    assert_eq!(
        seed.state.proofs[&repair.repair_verification_proof_id]
            .related_proof_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            repair.repair_eligibility_proof_id.clone(),
            repair.repair_mutation_proof_id.clone(),
        ])
    );
    let rerun_proof = &seed.state.proofs[&repair.validation_rerun_proof_id];
    assert_eq!(
        rerun_proof.related_proof_ids.as_slice(),
        std::slice::from_ref(&repair.repair_verification_proof_id)
    );
    assert!(
        rerun_proof
            .related_evidence_ids
            .contains(&repair.validation_rerun_id)
    );
    let validation = seed.state.validation.as_ref().unwrap();
    assert!(
        validation
            .failures
            .contains_key(&repair.failure_revision_id)
    );
    assert_eq!(
        validation.selections[&repair.failure_revision_id]
            .intent
            .repair_intent_id,
        repair.repair_intent_id
    );

    let policy = seed.state.finalization_policy.clone().unwrap();
    let plan = accepted_plan(&seed.state).clone();
    let authoritative = seed.state.review.as_ref().unwrap().ancestry.clone();
    let stale_ancestry = EngineeringAncestryV1::new(
        RepositoryRevisionId::new("repository-revision:phase7-stale"),
        hash("stale-repository-fingerprint"),
        authoritative.implementation_barrier_proof_id.clone(),
        authoritative.required_validation_proof_id.clone(),
        authoritative.ordered_revision_proof_ids.clone(),
    )
    .expect("well-shaped but stale ancestry record");
    let stale_request =
        DiffManifestRequestV1::new(seed.review_node_id.clone(), &plan, &stale_ancestry, &policy)
            .expect("stale ancestry can be materialized but has no aggregate authority");
    let mut stale_aggregate = seed.state.clone();
    append_next_authoritative(
        &mut stale_aggregate,
        "phase7:golden-b:stale-review-node-started",
    );
    let stale_event = envelope(
        &stale_aggregate,
        "phase7:golden-b:stale-diff-request",
        ReviewEvent::DiffManifestRequested {
            request: stale_request.clone(),
        },
    );
    let before_stale_aggregate = stale_aggregate.clone();
    assert!(
        stale_aggregate.append_event(stale_event).is_err(),
        "a well-shaped stale request cannot override reducer-derived ancestry"
    );
    assert_eq!(stale_aggregate, before_stale_aggregate);
    let mut stale_target = seed.state.review.clone().unwrap();
    let before_stale = stale_target.clone();
    assert_eq!(
        stale_target
            .apply(
                &ReviewEvent::DiffManifestRequested {
                    request: stale_request,
                },
                &plan,
                &policy,
            )
            .expect_err("stale ancestry cannot enter current review")
            .code(),
        "diff_manifest_request_binding_mismatch"
    );
    assert_eq!(stale_target, before_stale);

    let completed =
        complete_review_contract(seed, CompletionFlavor::ExternalReview, "golden-b", false);
    assert_seed_replays(&completed.seed);
    assert_eq!(
        completed.review_state.ancestry.ordered_revision_proof_ids,
        expected_ancestry
    );
    assert_eq!(
        completed.completion.disposition,
        CompletionDispositionV1::CompletePendingExternalReview
    );
    assert_eq!(
        completed.completion.external_review_reason_code(),
        Some("completion_pending_external_review")
    );
    assert_eq!(
        completed.policy.publication.requested_mode,
        PublicationModeV1::NormalWithExternalReview
    );
    assert!(completed.eligibility.is_granted());

    assert!(
        completed.materialized.pages[0]
            .bytes()
            .windows(RAW_DIFF_SECRET_SENTINEL.len())
            .any(|window| window == RAW_DIFF_SECRET_SENTINEL.as_bytes())
    );
    for formatted in [
        format!("{:?}", completed.materialized),
        serde_json::to_string(&completed.manifest).unwrap(),
        serde_json::to_string(&completed.review_state).unwrap(),
        serde_json::to_string(&completed.eligibility).unwrap(),
    ] {
        assert!(!formatted.contains(RAW_DIFF_SECRET_SENTINEL));
    }

    let serialized = serde_json::to_vec(&completed.review_state).unwrap();
    assert_eq!(
        serde_json::from_slice::<ReviewStateV1>(&serialized).unwrap(),
        completed.review_state
    );
    let mut unknown = serde_json::to_value(&completed.eligibility).unwrap();
    unknown.as_object_mut().unwrap().insert(
        "forged_publication_authority".into(),
        serde_json::json!(true),
    );
    assert!(serde_json::from_value::<PublicationEligibilityRecord>(unknown).is_err());

    assert_eq!(
        completed.completion_action.envelope.node_id,
        completed.seed.completion_node_id
    );
    assert!(completed.seed.state.publication.is_some());
}

#[test]
fn blocking_review_rejects_forged_convergence_and_replays_canonical_terminal() {
    let mut seed = phase7_golden_a_review_entry_seed_with_policy(|plan| {
        finalization_policy(plan, CompletionFlavor::Normal)
    });
    let policy = seed.state.finalization_policy.clone().unwrap();
    let plan = accepted_plan(&seed.state).clone();
    assert_eq!(
        append_next_authoritative(&mut seed.state, "phase7:blocking:review-node-started"),
        GraphEvent::NodeStarted {
            node_id: seed.review_node_id.clone(),
            attempt: 1,
        }
        .into()
    );
    let DomainEvent::Review(ReviewEvent::DiffManifestRequested { request }) =
        append_next_authoritative(&mut seed.state, "phase7:blocking:diff-requested")
    else {
        panic!("blocking review begins from the canonical current-revision diff request");
    };
    let (_, manifest) = exact_diff_manifest(&request, &plan, &plan.targets[0], "blocking");
    append(
        &mut seed.state,
        "phase7:blocking:diff-recorded",
        ReviewEvent::DiffManifestRecorded {
            manifest: Box::new(manifest.clone()),
        },
    );
    let DomainEvent::Review(ReviewEvent::ActionPrepared { prepared }) =
        append_next_authoritative(&mut seed.state, "phase7:blocking:action-prepared")
    else {
        panic!("blocking review still uses the canonical strict page action");
    };
    let page_action = *prepared;
    assert_strict_review_envelope(&page_action, ReviewToolV1::RecordDiffReview);
    dispatch_and_consume_review_action(&mut seed.state, &page_action, "blocking");
    let finding = DiffReviewFindingV1::new(
        DiffReviewFindingKindV1::UnsafeChange,
        DiffReviewFindingSeverityV1::Blocking,
        BTreeSet::from([0]),
        BTreeSet::new(),
        BTreeSet::new(),
        "unsafe_change".into(),
        hash("blocking-review-finding"),
    )
    .expect("bounded structured blocking finding");
    let observation = DiffPageReviewObservationV1::new(&page_action, &manifest, vec![finding])
        .expect("blocking finding is bound to the exact reviewed page");
    assert_eq!(observation.status, DiffPageReviewStatusV1::Blocking);
    append(
        &mut seed.state,
        "phase7:blocking:page-reviewed",
        ReviewEvent::DiffPageReviewed {
            observation: Box::new(observation),
        },
    );
    let DomainEvent::Review(ReviewEvent::DiffReviewRecorded { review }) =
        append_next_authoritative(&mut seed.state, "phase7:blocking:review-recorded")
    else {
        panic!("complete page coverage must aggregate before convergence");
    };
    assert_eq!(review.disposition, DiffReviewDispositionV1::Blocking);

    let forged = ReviewConvergenceV1::new(
        seed.state.repository_revision.clone(),
        policy.policy_id.clone(),
        ReviewConvergenceReasonV1::DiffReviewBlocked {
            review_id: DiffReviewId::new("diff-review:forged"),
        },
    )
    .expect("forged convergence is structurally valid but not reducer-authoritative");
    let forged_event = envelope(
        &seed.state,
        "phase7:blocking:forged-convergence",
        ReviewEvent::ConvergenceEvaluated {
            convergence: forged,
        },
    );
    let before_forged = seed.state.clone();
    assert!(matches!(
        seed.state
            .append_event(forged_event)
            .expect_err("a forged review ID cannot force convergence"),
        ProtocolViolation::ReviewContract {
            code: "review_convergence_not_authoritative"
        }
    ));
    assert_eq!(seed.state, before_forged);

    let DomainEvent::Review(ReviewEvent::ConvergenceEvaluated { convergence }) =
        append_next_authoritative(&mut seed.state, "phase7:blocking:convergence")
    else {
        panic!("the reducer must emit its exact blocking convergence");
    };
    assert_eq!(
        convergence.reason,
        ReviewConvergenceReasonV1::DiffReviewBlocked {
            review_id: review.review_id.clone(),
        }
    );
    let DomainEvent::Graph(GraphEvent::NodeFailed {
        node_id,
        failure_revision_id,
        terminal,
    }) = append_next_authoritative(&mut seed.state, "phase7:blocking:node-failed")
    else {
        panic!("canonical convergence must fail its exact active review node");
    };
    assert_eq!(node_id, seed.review_node_id);
    assert!(terminal);
    assert!(matches!(
        seed.state.nodes[&node_id].state,
        NodeState::FailedTerminal {
            failure_revision_id: ref actual,
        } if actual == &failure_revision_id
    ));

    let ProtocolDecision::Finish { result } =
        decide(&seed.state).expect("blocking review has one canonical terminal result")
    else {
        panic!("terminal review convergence cannot remain runnable");
    };
    assert_eq!(result.mission.outcome(), MissionOutcomeV1::BlockedNoDiff);
    assert_eq!(result.process_health, ProcessHealth::Healthy);
    assert_eq!(result.reason_code, "review_diff_blocked");
    let blocker = result.mission.first_fatal_blocker().unwrap();
    assert_eq!(blocker.category, "review");
    assert_eq!(blocker.node_id.as_ref(), Some(&seed.review_node_id));
    append(
        &mut seed.state,
        "phase7:blocking:terminal",
        TerminalEvent::CanonicalResultRecorded {
            result: result.clone(),
        },
    );
    assert_eq!(
        decide(&seed.state).unwrap(),
        ProtocolDecision::Finish { result }
    );
    assert_seed_replays(&seed);
}

#[test]
fn golden_a_publication_journals_intent_before_effect_and_replays_completion() {
    let seed = phase7_golden_a_review_entry_seed_with_policy(|plan| {
        finalization_policy(plan, CompletionFlavor::Normal)
    });
    let mut completed =
        complete_review_contract(seed, CompletionFlavor::Normal, "publication", false);
    let contract = completed.policy.publication.clone();
    let eligibility = completed.eligibility.clone();
    assert_eq!(
        append_next_authoritative(&mut completed.seed.state, "phase7:publication:node-started",),
        GraphEvent::NodeStarted {
            node_id: completed.seed.publication_node_id.clone(),
            attempt: 1,
        }
        .into()
    );
    let wait_event = envelope(
        &completed.seed.state,
        "phase7:publication:forged-wait",
        GraphEvent::NodeWaiting {
            node_id: completed.seed.publication_node_id.clone(),
            effect_id: EffectId::new("effect:phase7-forged-publication-wait"),
        },
    );
    let before_wait = completed.seed.state.clone();
    assert!(matches!(
        completed
            .seed
            .state
            .append_event(wait_event)
            .expect_err("publication effects cannot escape typed intent reconciliation"),
        ProtocolViolation::ReviewContract {
            code: "phase7_graph_wait_unavailable"
        }
    ));
    assert_eq!(completed.seed.state, before_wait);

    let expected_tree = CommitTreeBindingV1::from_review_authority(
        &eligibility,
        &completed.manifest,
        &completed.authority,
    )
    .expect("reviewed diff and fresh authority canonically bind the commit tree");
    let publication_before_commit = completed.seed.state.publication.clone().unwrap();
    let DomainEvent::Publication(PublicationEvent::CommitIntentPersisted {
        intent: commit_intent,
    }) = append_next_authoritative(
        &mut completed.seed.state,
        "phase7:publication:commit-intent",
    )
    else {
        panic!("the active publication node must durably emit its commit intent first");
    };
    assert_eq!(commit_intent.tree, expected_tree);
    assert_eq!(
        commit_intent.tree.manifest_id,
        completed.manifest.manifest_id
    );
    assert_eq!(commit_intent.tree.diff_hash, completed.manifest.diff_hash);
    assert_eq!(
        commit_intent.tree.repository_tree_oid,
        completed.authority.repository_tree_oid
    );
    assert_eq!(
        commit_intent.tree.parent_commit_oid,
        completed.authority.repository_head_oid
    );
    let commit_oid = "4".repeat(40);
    let commit_observation = CommitObservationV1::new(
        &commit_intent,
        CommitOutcomeV1::Confirmed {
            reconciliation: CommitReconciliationV1::AlreadySatisfied,
            commit_oid: commit_oid.clone(),
            repository_tree_oid: commit_intent.tree.repository_tree_oid.clone(),
            parent_commit_oid: commit_intent.tree.parent_commit_oid.clone(),
            commit_identity_hash: commit_intent.commit_identity_hash.clone(),
        },
    )
    .expect("already-present commit exactly reconciles the persisted intent identity");

    assert_eq!(
        publication_before_commit.pending_effect(None).unwrap(),
        None
    );
    assert_eq!(
        publication_before_commit
            .prepare_push_intent(&contract, &eligibility)
            .expect_err("push cannot be prepared before a confirmed commit")
            .code(),
        "push_requires_confirmed_commit"
    );
    let mut observation_first = publication_before_commit.clone();
    let before_observation_first = observation_first.clone();
    assert_eq!(
        observation_first
            .apply(
                &PublicationEvent::CommitObserved {
                    observation: commit_observation.clone(),
                },
                &contract,
                &eligibility,
            )
            .expect_err("an observation cannot authorize its own external effect")
            .code(),
        "commit_observation_without_persisted_intent"
    );
    assert_eq!(observation_first, before_observation_first);

    let mut retryable_failure = publication_before_commit;
    retryable_failure
        .apply(
            &PublicationEvent::CommitIntentPersisted {
                intent: commit_intent.clone(),
            },
            &contract,
            &eligibility,
        )
        .unwrap();
    let failed_observation = CommitObservationV1::new(
        &commit_intent,
        CommitOutcomeV1::Failed {
            failure: PublicationEffectFailureV1::Retryable {
                safe_code: "commit_not_applied".into(),
            },
        },
    )
    .expect("definitive retryable commit failure");
    retryable_failure
        .apply(
            &PublicationEvent::CommitObserved {
                observation: failed_observation,
            },
            &contract,
            &eligibility,
        )
        .unwrap();
    assert_eq!(
        retryable_failure.build_convergence(&contract).unwrap(),
        None,
        "a definitive retryable failure cannot converge while budget remains"
    );
    let retry_intent = retryable_failure
        .prepare_commit_intent(&contract, &eligibility, commit_intent.tree.clone())
        .expect("remaining commit budget admits an identity-stable retry");
    assert_eq!(retry_intent.attempt.operation_attempt, 2);
    assert_eq!(
        retry_intent.attempt.prior_attempt_id.as_ref(),
        Some(&commit_intent.attempt.attempt_id)
    );

    let mut exhausted_with_code_a = retryable_failure.clone();
    exhausted_with_code_a
        .apply(
            &PublicationEvent::CommitIntentPersisted {
                intent: retry_intent.clone(),
            },
            &contract,
            &eligibility,
        )
        .unwrap();
    let final_observation_a = CommitObservationV1::new(
        &retry_intent,
        CommitOutcomeV1::Failed {
            failure: PublicationEffectFailureV1::Retryable {
                safe_code: "commit_still_not_applied".into(),
            },
        },
    )
    .expect("final retry failure A is exact and well shaped");
    exhausted_with_code_a
        .apply(
            &PublicationEvent::CommitObserved {
                observation: final_observation_a.clone(),
            },
            &contract,
            &eligibility,
        )
        .unwrap();
    let convergence_a = exhausted_with_code_a
        .build_convergence(&contract)
        .unwrap()
        .expect("the second commit attempt exhausts its signed limit");

    let mut exhausted_with_code_b = retryable_failure;
    exhausted_with_code_b
        .apply(
            &PublicationEvent::CommitIntentPersisted {
                intent: retry_intent.clone(),
            },
            &contract,
            &eligibility,
        )
        .unwrap();
    let final_observation_b = CommitObservationV1::new(
        &retry_intent,
        CommitOutcomeV1::Failed {
            failure: PublicationEffectFailureV1::Retryable {
                safe_code: "commit_reconciliation_failed".into(),
            },
        },
    )
    .expect("final retry failure B is exact and well shaped");
    exhausted_with_code_b
        .apply(
            &PublicationEvent::CommitObserved {
                observation: final_observation_b.clone(),
            },
            &contract,
            &eligibility,
        )
        .unwrap();
    let convergence_b = exhausted_with_code_b
        .build_convergence(&contract)
        .unwrap()
        .expect("the alternate final observation also exhausts the same signed limit");
    assert_ne!(
        final_observation_a.observation_id,
        final_observation_b.observation_id
    );
    assert_ne!(
        final_observation_a.observation_hash,
        final_observation_b.observation_hash
    );
    assert_ne!(convergence_a.convergence_id, convergence_b.convergence_id);
    assert_ne!(
        convergence_a.convergence_hash,
        convergence_b.convergence_hash
    );
    assert_eq!(
        convergence_a.final_observation_id,
        PublicationObservationIdV1::Commit(final_observation_a.observation_id.clone())
    );
    assert_eq!(
        convergence_a.final_observation_hash,
        final_observation_a.observation_hash
    );
    convergence_a
        .validate_against(&exhausted_with_code_a, &contract)
        .expect("convergence is bound to its exact final failure observation");
    let mut tampered_convergence = convergence_a;
    tampered_convergence.final_observation_id = convergence_b.final_observation_id;
    tampered_convergence.final_observation_hash = convergence_b.final_observation_hash;
    assert_eq!(
        tampered_convergence
            .validate_against(&exhausted_with_code_a, &contract)
            .expect_err("a different final observation cannot authorize convergence")
            .code(),
        "publication_convergence_final_observation_mismatch"
    );

    assert_eq!(
        decide(&completed.seed.state).unwrap(),
        ProtocolDecision::Perform {
            effect: EffectRequest::Publication(PublicationEffectRequest::CreateCommit {
                intent: commit_intent.clone(),
            }),
        },
        "the external commit effect is visible only after its intent event"
    );
    append(
        &mut completed.seed.state,
        "phase7:publication:commit-observed",
        PublicationEvent::CommitObserved {
            observation: commit_observation,
        },
    );

    assert_eq!(
        completed
            .seed
            .state
            .publication
            .as_ref()
            .unwrap()
            .pending_effect(None)
            .unwrap(),
        None,
        "preparing an intent does not expose an effect before persistence"
    );
    let DomainEvent::Publication(PublicationEvent::PushIntentPersisted {
        intent: push_intent,
    }) = append_next_authoritative(&mut completed.seed.state, "phase7:publication:push-intent")
    else {
        panic!("confirmed commit must produce the reducer-owned exact-lease push intent");
    };
    assert_eq!(push_intent.commit_oid, commit_oid);
    assert_eq!(
        push_intent.expected_remote_head,
        contract.expected_remote_head
    );
    assert_eq!(
        decide(&completed.seed.state).unwrap(),
        ProtocolDecision::Perform {
            effect: EffectRequest::Publication(PublicationEffectRequest::PushExactLease {
                intent: push_intent.clone(),
            }),
        },
        "the external push effect is visible only after its intent event"
    );
    let push_observation = ExactLeasePushObservationV1::new(
        &push_intent,
        ExactLeasePushOutcomeV1::Confirmed {
            reconciliation: PushReconciliationV1::AlreadySatisfied,
            remote_head: commit_oid.clone(),
        },
    )
    .expect("remote head already at the commit is exact reconciliation");
    append(
        &mut completed.seed.state,
        "phase7:publication:push-observed",
        PublicationEvent::PushObserved {
            observation: push_observation,
        },
    );

    let publication_before_pull_request = completed.seed.state.publication.clone().unwrap();
    let marker = publication_before_pull_request.pull_request_execution_marker_hash();
    let raw_pull_request = RawPullRequestMaterialV1::new(
        format!("Phase 7 {RAW_PR_SECRET_SENTINEL}").into_bytes(),
        format!("Reviewed publication\nmarker: {marker}\n{RAW_PR_SECRET_SENTINEL}").into_bytes(),
    )
    .expect("bounded UTF-8 pull-request material includes its execution marker");
    assert!(
        raw_pull_request
            .body()
            .windows(marker.len())
            .any(|window| window == marker.as_bytes())
    );
    assert!(!format!("{raw_pull_request:?}").contains(RAW_PR_SECRET_SENTINEL));
    assert_eq!(
        publication_before_pull_request
            .pending_effect(Some(raw_pull_request.clone()))
            .expect_err("raw material alone cannot create a pull request")
            .code(),
        "pull_request_material_without_open_intent"
    );
    let raw_pull_request_intent = publication_before_pull_request
        .prepare_pull_request_intent(&contract, &eligibility, &raw_pull_request)
        .expect("confirmed exact-lease push enables the pull-request intent");
    assert_eq!(raw_pull_request_intent.execution_marker_hash, marker);
    assert_eq!(
        raw_pull_request_intent.title_hash,
        raw_pull_request.title_hash()
    );
    assert_eq!(
        raw_pull_request_intent.body_hash,
        raw_pull_request.body_hash()
    );
    assert!(
        !serde_json::to_string(&raw_pull_request_intent)
            .unwrap()
            .contains(RAW_PR_SECRET_SENTINEL)
    );
    let mut raw_contract_state = publication_before_pull_request;
    raw_contract_state
        .apply(
            &PublicationEvent::PullRequestIntentPersisted {
                intent: raw_pull_request_intent.clone(),
            },
            &contract,
            &eligibility,
        )
        .expect("pure journal accepts the exact hash-only pull-request intent");
    let raw_effect = raw_contract_state
        .pending_effect(Some(raw_pull_request.clone()))
        .expect("persisted raw-material intent can expose its matching effect")
        .expect("pure pull-request effect remains pending");
    let PublicationEffectRequest::EnsurePullRequest {
        intent: raw_effect_intent,
        material: raw_effect_material,
    } = &raw_effect
    else {
        panic!("pure pull-request journal must expose only its matching effect");
    };
    assert_eq!(raw_effect_intent, &raw_pull_request_intent);
    assert_eq!(raw_effect_material.title(), raw_pull_request.title());
    assert_eq!(raw_effect_material.body(), raw_pull_request.body());
    assert!(!format!("{raw_effect:?}").contains(RAW_PR_SECRET_SENTINEL));
    assert!(
        !serde_json::to_string(&raw_contract_state)
            .unwrap()
            .contains(RAW_PR_SECRET_SENTINEL)
    );

    let DomainEvent::Publication(PublicationEvent::PullRequestIntentPersisted {
        intent: pull_request_intent,
    }) = append_next_authoritative(
        &mut completed.seed.state,
        "phase7:publication:pull-request-intent",
    )
    else {
        panic!("confirmed push must produce the reducer-owned pull-request intent");
    };
    let effect = decide(&completed.seed.state)
        .expect("persisted pull-request intent exposes the reducer-owned effect");
    let ProtocolDecision::Perform {
        effect:
            EffectRequest::Publication(PublicationEffectRequest::EnsurePullRequest { intent, material }),
    } = &effect
    else {
        panic!("persisted pull-request intent must expose only its matching effect");
    };
    assert_eq!(intent, &pull_request_intent);
    assert!(
        material
            .body()
            .windows(marker.len())
            .any(|window| window == marker.as_bytes())
    );
    let generated_title = std::str::from_utf8(material.title()).unwrap();
    let generated_body = std::str::from_utf8(material.body()).unwrap();
    assert!(!format!("{effect:?}").contains(generated_title));
    assert!(!format!("{effect:?}").contains(generated_body));

    let confirmed_pull_request = |pull_request_url: String| PullRequestOutcomeV1::Confirmed {
        reconciliation: PullRequestReconciliationV1::AlreadySatisfied,
        pull_request_number: 42,
        pull_request_url,
        node_id_hash: hash("pull-request-node-id"),
        base_ref: pull_request_intent.base_ref.clone(),
        head_branch: pull_request_intent.head_branch.clone(),
        observed_head: pull_request_intent.commit_oid.clone(),
        execution_marker_hash: pull_request_intent.execution_marker_hash.clone(),
        draft: pull_request_intent.draft,
    };
    let invalid_pull_request_urls = [
        format!("https://rustgrid:{RAW_PR_SECRET_SENTINEL}@github.com/rustgrid/rustgrid/pull/42"),
        format!("https://github.com/rustgrid/rustgrid/pull/42?token={RAW_PR_SECRET_SENTINEL}"),
        format!("https://github.com/rustgrid/rustgrid/pull/42#{RAW_PR_SECRET_SENTINEL}"),
    ];
    for invalid_url in &invalid_pull_request_urls {
        let error = PullRequestObservationV1::new(
            &pull_request_intent,
            confirmed_pull_request(invalid_url.clone()),
        )
        .expect_err("credential, query, and fragment material cannot enter a PR URL");
        assert_eq!(error.code(), "pull_request_observation_invalid");
        assert!(!format!("{error:?} {error}").contains(RAW_PR_SECRET_SENTINEL));
    }
    let pull_request_observation = PullRequestObservationV1::new(
        &pull_request_intent,
        confirmed_pull_request("https://github.com/rustgrid/rustgrid/pull/42".into()),
    )
    .expect("existing pull request exactly reconciles every persisted coordinate");
    for invalid_url in invalid_pull_request_urls {
        let mut forged = pull_request_observation.clone();
        let PullRequestOutcomeV1::Confirmed {
            pull_request_url, ..
        } = &mut forged.outcome
        else {
            panic!("fixture observation is confirmed");
        };
        *pull_request_url = invalid_url;
        let error = forged
            .validate_against(&pull_request_intent)
            .expect_err("revalidation rejects non-canonical PR URLs before persistence");
        assert_eq!(error.code(), "pull_request_observation_invalid");
        assert!(!format!("{error:?} {error}").contains(RAW_PR_SECRET_SENTINEL));
    }
    append(
        &mut completed.seed.state,
        "phase7:publication:pull-request-observed",
        PublicationEvent::PullRequestObserved {
            observation: pull_request_observation,
        },
    );
    let DomainEvent::Publication(PublicationEvent::CompletionRecorded { completion }) =
        append_next_authoritative(&mut completed.seed.state, "phase7:publication:completion")
    else {
        panic!("all exact observations must produce reducer-owned publication completion");
    };
    assert_eq!(completion.commit_oid, commit_oid);
    assert_eq!(completion.pull_request_number, 42);
    assert!(!completion.draft);

    {
        let publication = completed.seed.state.publication.as_ref().unwrap();
        assert_eq!(publication.completion.as_ref(), Some(&completion));
        assert_eq!(publication.attempts.len(), 3);
        publication
            .validate(&contract, &eligibility)
            .expect("publication state reconstructs exactly from its journal");
        let serialized = serde_json::to_vec(publication).unwrap();
        assert!(
            !serialized
                .windows(RAW_PR_SECRET_SENTINEL.len())
                .any(|window| window == RAW_PR_SECRET_SENTINEL.as_bytes())
        );
        let decoded: PublicationStateV1 = serde_json::from_slice(&serialized).unwrap();
        assert_eq!(&decoded, publication);
        decoded
            .validate(&contract, &eligibility)
            .expect("strictly deserialized publication state replays identically");
        let mut unknown = serde_json::to_value(publication).unwrap();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("raw_pull_request_body".into(), serde_json::json!("forged"));
        assert!(serde_json::from_value::<PublicationStateV1>(unknown).is_err());
    }

    let DomainEvent::Evidence(EvidenceEvent::ProofRecorded { proof }) = append_next_authoritative(
        &mut completed.seed.state,
        "phase7:publication:completion-proof",
    ) else {
        panic!("canonical completion must produce its publication proof");
    };
    assert_eq!(proof.kind, ProofKind::PublicationCompleted);
    assert_eq!(
        append_next_authoritative(
            &mut completed.seed.state,
            "phase7:publication:node-succeeded",
        ),
        GraphEvent::NodeSucceeded {
            node_id: completed.seed.publication_node_id.clone(),
            proof_id: proof.id.clone(),
        }
        .into()
    );
    let ProtocolDecision::Finish { result } =
        decide(&completed.seed.state).expect("published Golden A has a canonical terminal result")
    else {
        panic!("completed publication cannot remain runnable");
    };
    assert_eq!(
        result.mission,
        MissionResult::Succeeded {
            publication_proof_id: proof.id,
        }
    );
    assert_eq!(result.process_health, ProcessHealth::Healthy);
    assert_eq!(result.reason_code, "publication_succeeded");
    assert!(result.remaining_work.is_empty());
    append(
        &mut completed.seed.state,
        "phase7:publication:terminal",
        TerminalEvent::CanonicalResultRecorded {
            result: result.clone(),
        },
    );
    assert_eq!(
        decide(&completed.seed.state).unwrap(),
        ProtocolDecision::Finish { result }
    );
    assert_seed_replays(&completed.seed);
}

#[test]
fn remote_branch_movement_converges_to_exact_publication_failure_and_replays() {
    let seed = phase7_golden_a_review_entry_seed_with_policy(|plan| {
        finalization_policy(plan, CompletionFlavor::Normal)
    });
    let mut completed =
        complete_review_contract(seed, CompletionFlavor::Normal, "remote-moved", false);
    assert_eq!(
        append_next_authoritative(
            &mut completed.seed.state,
            "phase7:remote-moved:node-started",
        ),
        GraphEvent::NodeStarted {
            node_id: completed.seed.publication_node_id.clone(),
            attempt: 1,
        }
        .into()
    );
    let DomainEvent::Publication(PublicationEvent::CommitIntentPersisted {
        intent: commit_intent,
    }) = append_next_authoritative(
        &mut completed.seed.state,
        "phase7:remote-moved:commit-intent",
    )
    else {
        panic!("publication starts from a reducer-owned commit intent");
    };
    assert_eq!(
        decide(&completed.seed.state).unwrap(),
        ProtocolDecision::Perform {
            effect: EffectRequest::Publication(PublicationEffectRequest::CreateCommit {
                intent: commit_intent.clone(),
            }),
        }
    );
    let commit_oid = "4".repeat(40);
    let commit_observation = CommitObservationV1::new(
        &commit_intent,
        CommitOutcomeV1::Confirmed {
            reconciliation: CommitReconciliationV1::AlreadySatisfied,
            commit_oid: commit_oid.clone(),
            repository_tree_oid: commit_intent.tree.repository_tree_oid.clone(),
            parent_commit_oid: commit_intent.tree.parent_commit_oid.clone(),
            commit_identity_hash: commit_intent.commit_identity_hash.clone(),
        },
    )
    .unwrap();
    append(
        &mut completed.seed.state,
        "phase7:remote-moved:commit-observed",
        PublicationEvent::CommitObserved {
            observation: commit_observation,
        },
    );
    let DomainEvent::Publication(PublicationEvent::PushIntentPersisted {
        intent: push_intent,
    }) = append_next_authoritative(&mut completed.seed.state, "phase7:remote-moved:push-intent")
    else {
        panic!("confirmed commit must produce the exact-lease push intent");
    };
    assert_eq!(
        decide(&completed.seed.state).unwrap(),
        ProtocolDecision::Perform {
            effect: EffectRequest::Publication(PublicationEffectRequest::PushExactLease {
                intent: push_intent.clone(),
            }),
        }
    );
    let movement = RemoteBranchMoved::new(&push_intent, Some("5".repeat(40)))
        .expect("observed remote head differs from both lease and intended commit");
    let push_observation = ExactLeasePushObservationV1::new(
        &push_intent,
        ExactLeasePushOutcomeV1::RemoteBranchMoved {
            movement: movement.clone(),
        },
    )
    .unwrap();
    append(
        &mut completed.seed.state,
        "phase7:remote-moved:push-observed",
        PublicationEvent::PushObserved {
            observation: push_observation,
        },
    );
    let DomainEvent::Publication(PublicationEvent::ConvergenceEvaluated { convergence }) =
        append_next_authoritative(&mut completed.seed.state, "phase7:remote-moved:convergence")
    else {
        panic!("remote movement must converge without allocating a blind retry");
    };
    assert_eq!(
        convergence.reason,
        PublicationConvergenceReasonV1::RemoteBranchMoved {
            movement_id: movement.movement_id,
        }
    );
    let DomainEvent::Graph(GraphEvent::NodeFailed {
        node_id,
        failure_revision_id,
        terminal,
    }) = append_next_authoritative(&mut completed.seed.state, "phase7:remote-moved:node-failed")
    else {
        panic!("publication convergence must fail its exact active owner");
    };
    assert_eq!(node_id, completed.seed.publication_node_id);
    assert!(terminal);
    assert!(matches!(
        completed.seed.state.nodes[&node_id].state,
        NodeState::FailedTerminal {
            failure_revision_id: ref actual,
        } if actual == &failure_revision_id
    ));
    let ProtocolDecision::Finish { result } = decide(&completed.seed.state).unwrap() else {
        panic!("remote movement convergence has one canonical terminal result");
    };
    assert_eq!(
        result.mission.outcome(),
        MissionOutcomeV1::PublicationFailed
    );
    assert_eq!(
        result.process_health,
        ProcessHealth::Failed {
            code: "publication_remote_branch_moved".into(),
        }
    );
    assert_eq!(result.reason_code, "publication_remote_branch_moved");
    let blocker = result.mission.first_fatal_blocker().unwrap();
    assert_eq!(blocker.category, "publication");
    assert_eq!(blocker.node_id.as_ref(), Some(&node_id));
    append(
        &mut completed.seed.state,
        "phase7:remote-moved:terminal",
        TerminalEvent::CanonicalResultRecorded {
            result: result.clone(),
        },
    );
    assert_eq!(
        decide(&completed.seed.state).unwrap(),
        ProtocolDecision::Finish { result }
    );
    assert_seed_replays(&completed.seed);
}
