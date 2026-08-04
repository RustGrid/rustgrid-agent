    #[test]
    fn validation_outcomes_require_recorded_current_evidence() {
        let mut validation_graph = graph();
        let mutation_ids = validation_graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_mutation())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        for node_id in mutation_ids {
            validation_graph
                .set_node_status(&node_id, ExecutionNodeStatus::Applied)
                .unwrap();
        }
        let validation = validation_graph
            .nodes
            .iter()
            .find(|node| node.kind.is_validation())
            .expect("validation node")
            .clone();
        let gate = validation.validation.as_ref().expect("validation gate");
        let fingerprint = gate.fingerprint("tree-1");
        let mut snapshot = ExecutionSnapshot {
            run_id: "validation-evidence-guards".into(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".into(),
                ..RepositorySnapshot::default()
            },
            graph: validation_graph,
            ..ExecutionSnapshot::default()
        };

        let error = snapshot
            .append_event(ExecutionDomainEvent::ValidationPassed {
                sequence: 1,
                node_id: validation.id.clone(),
                evidence_id: "missing-evidence".into(),
                fingerprint: fingerprint.clone(),
            })
            .expect_err("a pass without recorded evidence must fail closed");
        assert!(error.message.contains("unknown evidence"));
        assert!(snapshot.events.is_empty());

        let mut missing_failure_evidence = snapshot.clone();
        missing_failure_evidence
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 1,
                failure: FailureRecord::new(
                    "unproven-validation-failure",
                    validation.id.clone(),
                    FailureCategory::ValidationFailure,
                    1,
                    "tree-1",
                    "validation failed without evidence",
                ),
            })
            .unwrap();
        let before_unproven_failure = missing_failure_evidence.clone();
        let error = missing_failure_evidence
            .append_event(ExecutionDomainEvent::ValidationFailed {
                sequence: 2,
                node_id: validation.id.clone(),
                failure_id: FailureId::new("unproven-validation-failure"),
                fingerprint: fingerprint.clone(),
            })
            .expect_err("a failure without recorded evidence must fail closed");
        assert!(error.message.contains("non-pass evidence"));
        assert_eq!(missing_failure_evidence, before_unproven_failure);

        snapshot
            .append_event(ExecutionDomainEvent::ValidationEvidenceRecorded {
                sequence: 1,
                node_id: validation.id.clone(),
                evidence: ValidationEvidenceRecord {
                    evidence_id: "failed-evidence".into(),
                    node_id: validation.id.clone(),
                    gate_id: gate.gate_id.clone(),
                    fingerprint: fingerprint.clone(),
                    repository_fingerprint: "tree-1".into(),
                    command: gate.command.clone(),
                    working_directory: gate.working_directory.clone(),
                    status: ValidationEvidenceStatus::Failed,
                    exit_code: Some(1),
                    output_summary: "failed".into(),
                    duration: Duration::from_millis(1),
                },
            })
            .unwrap();
        let before_invalid_pass = snapshot.clone();
        let error = snapshot
            .append_event(ExecutionDomainEvent::ValidationPassed {
                sequence: 2,
                node_id: validation.id.clone(),
                evidence_id: "failed-evidence".into(),
                fingerprint: fingerprint.clone(),
            })
            .expect_err("failed evidence cannot prove a validation pass");
        assert!(error.message.contains("requires passed evidence"));
        assert_eq!(snapshot, before_invalid_pass);

        let failure_id = FailureId::new("validation-failure");
        snapshot
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 2,
                failure: FailureRecord::new(
                    failure_id.clone(),
                    validation.id.clone(),
                    FailureCategory::ValidationFailure,
                    1,
                    "tree-1",
                    "validation failed",
                ),
            })
            .unwrap();
        snapshot
            .append_event(ExecutionDomainEvent::ValidationFailed {
                sequence: 3,
                node_id: validation.id,
                failure_id,
                fingerprint,
            })
            .expect("attached current failed evidence proves the validation failure");
    }

    #[test]
    fn failure_events_enforce_identity_category_and_resolution_invariants() {
        let graph = graph();
        let source = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source mutation")
            .id
            .clone();
        let test = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::TestMutation)
            .expect("test mutation")
            .id
            .clone();
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-failure-invariants".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        let before = snapshot.clone();
        let invalid_category = snapshot
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 1,
                failure: FailureRecord::new(
                    "invalid-validation",
                    source.clone(),
                    FailureCategory::ValidationFailure,
                    1,
                    "tree-1",
                    "wrong node category",
                ),
            })
            .expect_err("validation failure cannot belong to a mutation node");
        assert!(invalid_category.message.contains("invalid for node"));
        assert_eq!(snapshot, before, "rejected failure event must be atomic");

        let mut failure = FailureRecord::new(
            "mutation-failure",
            source.clone(),
            FailureCategory::MutationConflict,
            1,
            "tree-1",
            "mutation conflict",
        );
        failure.target_path = Some("src/theme.ts".to_owned());
        snapshot
            .append_event(ExecutionDomainEvent::MutationRejected {
                sequence: 1,
                node_id: source.clone(),
                failure,
            })
            .expect("record valid failure");
        let before_wrong_resolution = snapshot.clone();
        let wrong_node = snapshot
            .append_event(ExecutionDomainEvent::FailureSuperseded {
                sequence: 2,
                node_id: test,
                failure_id: FailureId::new("mutation-failure"),
                repository_fingerprint: "tree-2".to_owned(),
            })
            .expect_err("resolution node must match failure node");
        assert!(wrong_node.message.contains("belongs to node"));
        assert_eq!(snapshot, before_wrong_resolution);

        snapshot
            .append_event(ExecutionDomainEvent::FailureSuperseded {
                sequence: 2,
                node_id: source.clone(),
                failure_id: FailureId::new("mutation-failure"),
                repository_fingerprint: "tree-2".to_owned(),
            })
            .expect("resolve valid mutation failure");
        let already_resolved = snapshot
            .append_event(ExecutionDomainEvent::FailureRecovered {
                sequence: 3,
                node_id: source,
                failure_id: FailureId::new("mutation-failure"),
                repository_fingerprint: "tree-2".to_owned(),
            })
            .expect_err("resolved failure cannot be recovered twice");
        assert!(already_resolved.message.contains("already resolved"));
    }

    #[test]
    fn verified_repair_applies_target_supersedes_failure_and_replays() {
        let graph = graph();
        let node_id = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source mutation")
            .id
            .clone();
        let target_path = graph
            .node(&node_id)
            .and_then(|node| node.target.as_ref())
            .expect("mutation target")
            .path
            .clone();
        let mut snapshot = ExecutionSnapshot {
            run_id: "verified-repair".into(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".into(),
                source_tree_hash: "tree-1".into(),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        snapshot
            .append_event(ExecutionDomainEvent::NodeStarted {
                sequence: 1,
                node_id: node_id.clone(),
                attempt: 1,
                started_at: "primary".into(),
                repository_fingerprint: "tree-1".into(),
            })
            .unwrap();
        let mut failure = FailureRecord::new(
            "patch-failure",
            node_id.clone(),
            FailureCategory::MutationConflict,
            1,
            "tree-1",
            "typed patch target rejection",
        );
        failure.code = Some(MutationApplicationFailure::InvalidPatchTarget.as_str().into());
        failure.target_path = Some(target_path.clone());
        snapshot
            .append_event(ExecutionDomainEvent::MutationRejected {
                sequence: 2,
                node_id: node_id.clone(),
                failure,
            })
            .unwrap();
        snapshot
            .append_event(ExecutionDomainEvent::NodeStarted {
                sequence: 3,
                node_id: node_id.clone(),
                attempt: 2,
                started_at: "forced-replacement".into(),
                repository_fingerprint: "tree-1".into(),
            })
            .unwrap();
        snapshot
            .append_event(ExecutionDomainEvent::TargetMutationProduced {
                sequence: 4,
                node_id: node_id.clone(),
                target_path: target_path.clone(),
                expected_repository_fingerprint: RepositoryFingerprint::new("tree-1"),
                repository_fingerprint: RepositoryFingerprint::new("tree-2"),
                before_content_hash: Some("before".into()),
                after_content_hash: Some("after".into()),
            })
            .unwrap();
        snapshot
            .append_event(ExecutionDomainEvent::MutationApplied {
                sequence: 5,
                node_id: node_id.clone(),
                target_path: target_path.clone(),
                repository_fingerprint: "tree-2".into(),
                evidence_id: "verified-repair-evidence".into(),
                created_target_evidence: None,
            })
            .unwrap();

        assert_eq!(
            snapshot.graph.node(&node_id).map(|node| node.status),
            Some(ExecutionNodeStatus::Applied)
        );
        assert_eq!(
            snapshot
                .failures
                .get(&FailureId::new("patch-failure"))
                .map(|failure| failure.status),
            Some(FailureStatus::Superseded)
        );
        assert!(!snapshot.failures.has_unresolved());

        let encoded = serde_json::to_vec(&snapshot).unwrap();
        let replayed: ExecutionSnapshot = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(replayed, snapshot);
    }

    #[test]
    fn success_events_cannot_bypass_graph_dependencies() {
        let graph = graph();
        let completion = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .expect("completion node")
            .id
            .clone();
        let publication = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Publication)
            .expect("publication node")
            .id
            .clone();
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-ordering".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };

        let completion_error = snapshot
            .append_event(ExecutionDomainEvent::CompletionEvaluated {
                sequence: 1,
                node_id: completion,
                outcome: MissionOutcome::Complete,
            })
            .expect_err("completion must wait for diff review");
        assert!(
            completion_error
                .message
                .contains("cannot advance before dependency")
        );

        let publication_error = snapshot
            .append_event(ExecutionDomainEvent::PublicationStarted {
                sequence: 1,
                node_id: publication,
                mode: PublicationMode::Normal,
            })
            .expect_err("publication must wait for completion evaluation");
        assert!(
            publication_error
                .message
                .contains("cannot advance before dependency")
        );
        assert!(snapshot.events.is_empty(), "rejected events must be atomic");

        let completion = snapshot
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .expect("completion node")
            .id
            .clone();
        snapshot
            .graph
            .set_node_status(&completion, ExecutionNodeStatus::Completed)
            .expect("inject malformed materialized status");
        assert!(
            snapshot.validate_invariants().is_err(),
            "deserialized status drift must not bypass dependency enforcement"
        );
    }

    #[test]
    fn finalization_invalidation_is_authoritative_and_replays_exactly() {
        let (mut initial, publication, validation_evidence_ids) = recovery_publication_snapshot();
        let review = initial
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::DiffReview)
            .expect("diff review")
            .id
            .clone();
        let completion = initial
            .graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .expect("completion evaluation")
            .id
            .clone();
        initial
            .append_event(ExecutionDomainEvent::DiffReviewed {
                sequence: 1,
                node_id: review,
                evidence_ids: validation_evidence_ids,
            })
            .expect("review current diff");
        initial
            .append_event(ExecutionDomainEvent::CompletionEvaluated {
                sequence: 2,
                node_id: completion,
                outcome: MissionOutcome::Complete,
            })
            .expect("evaluate completion");
        initial
            .append_event(ExecutionDomainEvent::PublicationStarted {
                sequence: 3,
                node_id: publication,
                mode: PublicationMode::Normal,
            })
            .expect("start publication");

        let stale_validation_evidence_ids = initial.finalization_validation_evidence_ids();
        let event = ExecutionDomainEvent::FinalizationInvalidated {
            sequence: 4,
            repository_fingerprint: "tree-after-remote-reconciliation".to_owned(),
            stale_validation_evidence_ids: stale_validation_evidence_ids.clone(),
        };
        let encoded = serde_json::to_string(&event).expect("serialize invalidation event");
        let decoded: ExecutionDomainEvent =
            serde_json::from_str(&encoded).expect("deserialize invalidation event");

        let mut persisted = initial.clone();
        persisted
            .append_event(event)
            .expect("invalidate stale finalization");
        let mut replayed = initial;
        replayed
            .append_event(decoded)
            .expect("replay finalization invalidation");

        assert_eq!(replayed.graph, persisted.graph);
        assert_eq!(replayed.evidence, persisted.evidence);
        assert_eq!(replayed.publication, persisted.publication);
        assert_eq!(replayed.current_repository, persisted.current_repository);
        assert_eq!(replayed.events, persisted.events);
        assert_eq!(
            persisted.current_repository.fingerprint,
            "tree-after-remote-reconciliation"
        );
        assert_eq!(persisted.publication, PublicationState::default());
        assert!(persisted.graph.nodes.iter().all(|node| {
            !(node.kind.is_validation()
                || matches!(
                    node.kind,
                    ExecutionNodeKind::DiffReview
                        | ExecutionNodeKind::CompletionEvaluation
                        | ExecutionNodeKind::Publication
                ))
                || !node.status.is_success()
        }));
        for evidence_id in stale_validation_evidence_ids {
            assert_eq!(
                persisted.evidence.validations[&evidence_id].status,
                ValidationEvidenceStatus::Superseded
            );
        }
        persisted
            .validate_invariants()
            .expect("invalidated state remains graph-valid");
    }

    #[test]
    fn finalization_invalidation_rejects_noncanonical_evidence_and_empty_fingerprint() {
        let (snapshot, _, _) = recovery_publication_snapshot();
        let expected = snapshot.finalization_validation_evidence_ids();
        let mut missing = expected.clone();
        missing.pop();
        let error = snapshot
            .with_event(ExecutionDomainEvent::FinalizationInvalidated {
                sequence: 1,
                repository_fingerprint: "tree-2".to_owned(),
                stale_validation_evidence_ids: missing,
            })
            .expect_err("missing stale proof must fail closed");
        assert!(error.message.contains("exactly match"));
        let error = snapshot
            .with_event(ExecutionDomainEvent::FinalizationInvalidated {
                sequence: 1,
                repository_fingerprint: String::new(),
                stale_validation_evidence_ids: expected,
            })
            .expect_err("empty repository fingerprint must fail closed");
        assert!(error.message.contains("repository fingerprint"));
    }

    #[test]
    fn recovery_publication_uses_current_validation_proof_without_fabricating_completion() {
        let (initial, publication, validation_evidence_ids) = recovery_publication_snapshot();
        let events = vec![
            ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: 1,
                node_id: publication.clone(),
                repository_fingerprint: "tree-recovery".to_owned(),
                validation_evidence_ids,
            },
            ExecutionDomainEvent::CommitCreated {
                sequence: 2,
                node_id: publication.clone(),
                commit_sha: "recovery-commit".to_owned(),
            },
            ExecutionDomainEvent::BranchPushed {
                sequence: 3,
                node_id: publication.clone(),
                branch: "rustgrid/recovery".to_owned(),
            },
            ExecutionDomainEvent::PullRequestCreated {
                sequence: 4,
                node_id: publication,
                url: "https://example.test/pull/99".to_owned(),
                number: Some(99),
                draft: true,
            },
            ExecutionDomainEvent::RunFinished {
                sequence: 5,
                outcome: MissionOutcome::PartialReviewable,
            },
        ];
        let encoded = serde_json::to_string(&events).expect("serialize recovery event stream");
        let replay_events: Vec<ExecutionDomainEvent> =
            serde_json::from_str(&encoded).expect("deserialize recovery event stream");

        let mut persisted = initial.clone();
        for event in events {
            persisted.append_event(event).expect("apply recovery event");
        }
        let mut replayed = initial;
        for event in replay_events {
            replayed.append_event(event).expect("replay recovery event");
        }

        assert_eq!(replayed.graph, persisted.graph);
        assert_eq!(replayed.publication, persisted.publication);
        assert_eq!(replayed.events, persisted.events);
        assert!(persisted.graph.recovery_publication_dependency_override);
        assert_eq!(
            persisted.publication.mode,
            Some(PublicationMode::DraftRecovery)
        );
        assert!(persisted.publication.draft);
        assert!(persisted.publication.recovery_requested);
        assert!(persisted.publication.is_published());
        assert_eq!(
            persisted.terminal_outcome(),
            Some(MissionOutcome::PartialReviewable)
        );
        assert!(persisted.graph.nodes.iter().all(|node| {
            !matches!(
                node.kind,
                ExecutionNodeKind::DiffReview | ExecutionNodeKind::CompletionEvaluation
            ) || !node.status.is_success()
        }));
        assert!(persisted.graph.nodes.iter().all(|node| {
            !node.kind.is_validation() || node.status == ExecutionNodeStatus::Passed
        }));
        persisted
            .validate_invariants()
            .expect("draft recovery publication remains graph-valid");
    }

    #[test]
    fn recovery_publication_preserves_commit_and_push_progress_idempotently() {
        let cases = [
            (PublicationStatus::CommitCreated, None),
            (
                PublicationStatus::BranchPushed,
                Some("rustgrid/already-pushed".to_owned()),
            ),
        ];
        for (status, branch) in cases {
            let (mut snapshot, publication, validation_evidence_ids) =
                recovery_publication_snapshot();
            snapshot.publication.status = status;
            snapshot.publication.commit_sha = Some("trusted-existing-head".to_owned());
            snapshot.publication.branch = branch.clone();
            let request = ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: snapshot.next_event_sequence(),
                node_id: publication.clone(),
                repository_fingerprint: snapshot.current_repository.fingerprint.clone(),
                validation_evidence_ids: validation_evidence_ids.clone(),
            };

            snapshot
                .append_event(request)
                .expect("authorize recovery around persisted publication progress");
            assert_eq!(snapshot.publication.status, status);
            assert_eq!(
                snapshot.publication.commit_sha.as_deref(),
                Some("trusted-existing-head")
            );
            assert_eq!(snapshot.publication.branch, branch);
            assert_eq!(
                snapshot.publication.mode,
                Some(PublicationMode::DraftRecovery)
            );
            assert!(snapshot.publication.draft);
            assert!(snapshot.publication.recovery_requested);

            let repeated_request = ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: snapshot.next_event_sequence(),
                node_id: publication,
                repository_fingerprint: snapshot.current_repository.fingerprint.clone(),
                validation_evidence_ids,
            };
            snapshot
                .append_event(repeated_request)
                .expect("repeated recovery authorization is idempotent");
            assert_eq!(snapshot.publication.status, status);
            assert_eq!(
                snapshot.publication.commit_sha.as_deref(),
                Some("trusted-existing-head")
            );
            assert_eq!(snapshot.publication.branch, branch);
        }
    }

    #[test]
    fn resumed_validation_reuses_current_global_evidence_after_node_reset() {
        let (mut snapshot, publication, expected_evidence_ids) = recovery_publication_snapshot();
        for node in snapshot
            .graph
            .nodes
            .iter_mut()
            .filter(|node| node.kind.is_validation())
        {
            node.status = ExecutionNodeStatus::Pending;
            node.evidence_ids.clear();
        }
        snapshot.graph.refresh_readiness();

        assert_eq!(
            snapshot
                .current_required_validation_evidence_ids()
                .expect("current global validation proof remains reusable"),
            expected_evidence_ids
        );
        snapshot
            .append_event(ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: 1,
                node_id: publication,
                repository_fingerprint: "tree-recovery".to_owned(),
                validation_evidence_ids: expected_evidence_ids,
            })
            .expect("resumed current validation authorizes safe recovery publication");
    }

    #[test]
    fn same_fingerprint_validation_failure_revokes_prior_pass_for_recovery() {
        let (mut snapshot, publication, prior_evidence_ids) = recovery_publication_snapshot();
        let validation_id = snapshot
            .graph
            .nodes
            .iter()
            .rfind(|node| node.kind.is_validation())
            .expect("validation node")
            .id
            .clone();
        let validation_gate = snapshot
            .graph
            .node(&validation_id)
            .and_then(|node| node.validation.clone())
            .expect("validation gate");
        let validation_fingerprint = validation_gate.fingerprint("tree-recovery");
        let failure_id = FailureId::new("same-tree-validation-failure");
        snapshot
            .append_event(ExecutionDomainEvent::ValidationEvidenceRecorded {
                sequence: 1,
                node_id: validation_id.clone(),
                evidence: ValidationEvidenceRecord {
                    evidence_id: "same-tree-failed-evidence".to_owned(),
                    node_id: validation_id.clone(),
                    gate_id: validation_gate.gate_id,
                    fingerprint: validation_fingerprint.clone(),
                    repository_fingerprint: "tree-recovery".to_owned(),
                    command: validation_gate.command,
                    working_directory: validation_gate.working_directory,
                    status: ValidationEvidenceStatus::Failed,
                    exit_code: Some(1),
                    output_summary: "the rerun failed".to_owned(),
                    duration: Duration::from_millis(1),
                },
            })
            .expect("record current failed validation evidence");
        snapshot
            .append_event(ExecutionDomainEvent::FailureRecorded {
                sequence: 2,
                failure: FailureRecord::new(
                    failure_id.clone(),
                    validation_id.clone(),
                    FailureCategory::ValidationFailure,
                    1,
                    "tree-recovery",
                    "the rerun failed on the same repository state",
                ),
            })
            .expect("record current validation failure");
        snapshot
            .append_event(ExecutionDomainEvent::ValidationFailed {
                sequence: 3,
                node_id: validation_id,
                failure_id,
                fingerprint: validation_fingerprint,
            })
            .expect("materialize current validation failure");

        let error = snapshot
            .current_required_validation_evidence_ids()
            .expect_err("unresolved current validation failure revokes an older pass");
        assert!(error.message.contains("unresolved failure"));
        let error = snapshot
            .append_event(ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: 4,
                node_id: publication,
                repository_fingerprint: "tree-recovery".to_owned(),
                validation_evidence_ids: prior_evidence_ids,
            })
            .expect_err("same-fingerprint failed rerun must deny recovery publication");
        assert!(error.message.contains("unresolved failure"));
    }

    #[test]
    fn recovery_publication_fails_closed_for_stale_or_incomplete_authorization() {
        let (snapshot, publication, validation_evidence_ids) = recovery_publication_snapshot();
        let request = |snapshot: &ExecutionSnapshot,
                       node_id: ExecutionNodeId,
                       repository_fingerprint: &str,
                       evidence_ids: Vec<String>| {
            snapshot.with_event(ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: snapshot.next_event_sequence(),
                node_id,
                repository_fingerprint: repository_fingerprint.to_owned(),
                validation_evidence_ids: evidence_ids,
            })
        };

        let mut no_diff = snapshot.clone();
        no_diff.current_repository.changed_paths.clear();
        assert!(
            request(
                &no_diff,
                publication.clone(),
                "tree-recovery",
                validation_evidence_ids.clone()
            )
            .expect_err("zero diff cannot be published")
            .message
            .contains("non-empty")
        );
        assert!(
            request(
                &snapshot,
                publication.clone(),
                "tree-stale",
                validation_evidence_ids.clone()
            )
            .expect_err("stale fingerprint cannot authorize publication")
            .message
            .contains("current repository fingerprint")
        );
        let mut missing = validation_evidence_ids.clone();
        missing.pop();
        assert!(
            request(&snapshot, publication.clone(), "tree-recovery", missing)
                .expect_err("missing validation proof cannot authorize publication")
                .message
                .contains("exactly match")
        );
        let mut duplicate = validation_evidence_ids.clone();
        duplicate.push(validation_evidence_ids[0].clone());
        assert!(
            request(&snapshot, publication.clone(), "tree-recovery", duplicate)
                .expect_err("duplicate validation proof cannot authorize publication")
                .message
                .contains("exactly match")
        );
        let mut unknown = validation_evidence_ids.clone();
        unknown[0] = "unknown-validation-evidence".to_owned();
        assert!(
            request(&snapshot, publication.clone(), "tree-recovery", unknown)
                .expect_err("unknown validation proof cannot authorize publication")
                .message
                .contains("exactly match")
        );
        let mut stale_validation = snapshot.clone();
        stale_validation
            .evidence
            .validations
            .get_mut(&validation_evidence_ids[0])
            .expect("validation evidence")
            .status = ValidationEvidenceStatus::Superseded;
        assert!(
            request(
                &stale_validation,
                publication.clone(),
                "tree-recovery",
                validation_evidence_ids.clone()
            )
            .expect_err("superseded validation cannot authorize publication")
            .message
            .contains("no current passed evidence")
        );
        let mutation = snapshot
            .graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .expect("mutation node")
            .id
            .clone();
        assert!(
            request(
                &snapshot,
                mutation,
                "tree-recovery",
                validation_evidence_ids.clone()
            )
            .expect_err("recovery requires publication node")
            .message
            .contains("not a publication node")
        );

        let mut recovery = request(
            &snapshot,
            publication.clone(),
            "tree-recovery",
            validation_evidence_ids,
        )
        .expect("valid recovery request");
        let before_non_draft = recovery.clone();
        let error = recovery
            .append_event(ExecutionDomainEvent::PullRequestCreated {
                sequence: 2,
                node_id: publication.clone(),
                url: "https://example.test/pull/100".to_owned(),
                number: Some(100),
                draft: false,
            })
            .expect_err("recovery pull request must remain draft");
        assert!(error.message.contains("requires a draft"));
        assert_eq!(recovery, before_non_draft, "rejected event must be atomic");

        recovery
            .append_event(ExecutionDomainEvent::CommitCreated {
                sequence: 2,
                node_id: publication.clone(),
                commit_sha: "recovery-commit".to_owned(),
            })
            .expect("commit recovery work");
        recovery
            .append_event(ExecutionDomainEvent::BranchPushed {
                sequence: 3,
                node_id: publication.clone(),
                branch: "rustgrid/recovery".to_owned(),
            })
            .expect("push recovery work");
        recovery
            .append_event(ExecutionDomainEvent::PullRequestCreated {
                sequence: 4,
                node_id: publication.clone(),
                url: "https://example.test/pull/100".to_owned(),
                number: Some(100),
                draft: true,
            })
            .expect("publish recovery draft");
        assert!(
            request(
                &recovery,
                publication.clone(),
                "tree-recovery",
                recovery
                    .current_required_validation_evidence_ids()
                    .expect("current validation evidence")
            )
            .expect_err("completed publication cannot be replaced")
            .message
            .contains("cannot replace completed publication")
        );
        recovery
            .append_event(ExecutionDomainEvent::RunFinished {
                sequence: 5,
                outcome: MissionOutcome::PartialReviewable,
            })
            .expect("finish recovered publication");
        assert!(
            request(
                &recovery,
                publication,
                "tree-recovery",
                recovery
                    .current_required_validation_evidence_ids()
                    .expect("current validation evidence")
            )
            .expect_err("terminal execution cannot request recovery")
            .message
            .contains("cannot be appended after RunFinished")
        );
    }

    #[test]
    fn partial_guardrail_satisfies_edges_without_erasing_remaining_targets() {
        let mut graph = graph();
        let source = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source node")
            .id
            .clone();
        let pending_test = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::TestMutation)
            .expect("test node")
            .id
            .clone();
        graph
            .set_node_status(&source, ExecutionNodeStatus::Applied)
            .expect("apply useful source work");
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-partial".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-2".to_owned(),
                changed_paths: BTreeSet::from(["src/theme.ts".to_owned()]),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        snapshot
            .append_event(ExecutionDomainEvent::GuardrailTriggered {
                sequence: 1,
                reason: GuardrailReason::NodeBudgetExhausted,
                outcome: MissionOutcome::PartialReviewable,
                detail: "useful source work is ready for validation".to_owned(),
            })
            .expect("enter partial validation path");

        assert!(
            snapshot
                .graph
                .dependency_satisfaction_overrides
                .contains(&pending_test)
        );
        assert!(
            snapshot
                .remaining_required_nodes()
                .iter()
                .any(|node| node.id == pending_test),
            "partial dependency satisfaction must not erase remaining work"
        );
        let validation = snapshot
            .graph
            .next_runnable_node()
            .expect("validation becomes runnable");
        assert!(validation.kind.is_validation());
        snapshot.validate_invariants().expect("valid partial graph");
    }

    #[test]
    fn partial_route_reaches_draft_publication_without_erasing_remaining_targets() {
        let mut graph = graph();
        let source = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source node")
            .id
            .clone();
        let pending_test = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::TestMutation)
            .expect("test node")
            .id
            .clone();
        let validations = graph
            .nodes
            .iter()
            .filter(|node| node.kind.is_validation())
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        let review = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::DiffReview)
            .expect("diff review node")
            .id
            .clone();
        let completion = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::CompletionEvaluation)
            .expect("completion node")
            .id
            .clone();
        let publication = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::Publication)
            .expect("publication node")
            .id
            .clone();
        graph
            .set_node_status(&source, ExecutionNodeStatus::Applied)
            .expect("apply useful source work");
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-partial-publication".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-2".to_owned(),
                changed_paths: BTreeSet::from(["src/theme.ts".to_owned()]),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        snapshot
            .append_event(ExecutionDomainEvent::GuardrailTriggered {
                sequence: 1,
                reason: GuardrailReason::NodeBudgetExhausted,
                outcome: MissionOutcome::PartialReviewable,
                detail: "validate and publish the useful partial diff".to_owned(),
            })
            .expect("enter partial validation path");

        let mut sequence = 2;
        for node_id in validations {
            let gate = snapshot
                .graph
                .node(&node_id)
                .and_then(|node| node.validation.clone())
                .expect("validation gate");
            let validation_fingerprint = gate.fingerprint("tree-2");
            snapshot
                .append_event(ExecutionDomainEvent::ValidationStarted {
                    sequence,
                    node_id: node_id.clone(),
                    fingerprint: validation_fingerprint.clone(),
                })
                .expect("start validation in dependency order");
            sequence += 1;
            let evidence_id = format!("validation-{sequence}");
            snapshot
                .append_event(ExecutionDomainEvent::ValidationEvidenceRecorded {
                    sequence,
                    node_id: node_id.clone(),
                    evidence: ValidationEvidenceRecord {
                        evidence_id: evidence_id.clone(),
                        node_id: node_id.clone(),
                        gate_id: gate.gate_id,
                        fingerprint: validation_fingerprint.clone(),
                        repository_fingerprint: "tree-2".to_owned(),
                        command: gate.command,
                        working_directory: gate.working_directory,
                        status: ValidationEvidenceStatus::Passed,
                        exit_code: Some(0),
                        output_summary: "validation passed".to_owned(),
                        duration: Duration::from_millis(1),
                    },
                })
                .expect("record validation evidence in dependency order");
            sequence += 1;
            snapshot
                .append_event(ExecutionDomainEvent::ValidationPassed {
                    sequence,
                    node_id,
                    evidence_id,
                    fingerprint: validation_fingerprint,
                })
                .expect("pass validation in dependency order");
            sequence += 1;
        }
        snapshot
            .append_event(ExecutionDomainEvent::DiffReviewed {
                sequence,
                node_id: review,
                evidence_ids: vec!["diff-review".to_owned()],
            })
            .expect("review validated partial diff");
        sequence += 1;
        snapshot
            .append_event(ExecutionDomainEvent::CompletionEvaluated {
                sequence,
                node_id: completion,
                outcome: MissionOutcome::PartialReviewable,
            })
            .expect("evaluate partial completion after review");
        sequence += 1;
        snapshot
            .append_event(ExecutionDomainEvent::PublicationStarted {
                sequence,
                node_id: publication.clone(),
                mode: PublicationMode::Draft,
            })
            .expect("start draft publication");
        sequence += 1;
        snapshot
            .append_event(ExecutionDomainEvent::CommitCreated {
                sequence,
                node_id: publication.clone(),
                commit_sha: "partial-commit".to_owned(),
            })
            .expect("record partial commit");
        sequence += 1;
        snapshot
            .append_event(ExecutionDomainEvent::BranchPushed {
                sequence,
                node_id: publication.clone(),
                branch: "rustgrid/partial".to_owned(),
            })
            .expect("record partial branch");
        sequence += 1;
        snapshot
            .append_event(ExecutionDomainEvent::PullRequestCreated {
                sequence,
                node_id: publication,
                url: "https://example.test/pull/42".to_owned(),
                number: Some(42),
                draft: true,
            })
            .expect("publish draft pull request");
        sequence += 1;
        snapshot
            .append_event(ExecutionDomainEvent::RunFinished {
                sequence,
                outcome: MissionOutcome::PartialReviewable,
            })
            .expect("finish as partial reviewable");

        assert_eq!(
            snapshot.terminal_outcome(),
            Some(MissionOutcome::PartialReviewable)
        );
        assert!(snapshot.publication.is_published());
        assert!(snapshot.publication.draft);
        assert!(
            snapshot
                .remaining_required_nodes()
                .iter()
                .any(|node| node.id == pending_test),
            "publishing a partial result must preserve explicit remaining mutation work"
        );
        snapshot
            .validate_invariants()
            .expect("partial validation-to-publication route remains graph-valid");
    }

    #[test]
    fn node_started_records_and_bounds_target_repair_attempts() {
        let mut graph = graph();
        let node_id = graph
            .nodes
            .iter()
            .find(|node| node.kind == ExecutionNodeKind::SourceMutation)
            .expect("source node")
            .id
            .clone();
        graph
            .set_node_status(&node_id, ExecutionNodeStatus::FailedRecoverable)
            .expect("mark recoverable failure");
        let mut failure = FailureRecord::new(
            "repairable",
            node_id.clone(),
            FailureCategory::MutationConflict,
            1,
            "tree-1",
            "replacement did not match",
        );
        failure.target_path = Some("src/theme.ts".to_owned());
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-repair-budget".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            failures: FailureStore::default(),
            ..ExecutionSnapshot::default()
        };
        snapshot.failures.record(failure);

        for attempt in 1..=1 {
            snapshot
                .graph
                .set_node_status(&node_id, ExecutionNodeStatus::FailedRecoverable)
                .expect("repair remains recoverable");
            snapshot
                .append_event(ExecutionDomainEvent::NodeStarted {
                    sequence: u64::from(attempt),
                    node_id: node_id.clone(),
                    attempt,
                    started_at: format!("attempt-{attempt}"),
                    repository_fingerprint: "tree-1".to_owned(),
                })
                .expect("bounded repair start");
        }
        assert_eq!(snapshot.budget.usage_for(&node_id).repair_attempts, 1);
        snapshot
            .graph
            .set_node_status(&node_id, ExecutionNodeStatus::FailedRecoverable)
            .expect("third repair request");
        let error = snapshot
            .append_event(ExecutionDomainEvent::NodeStarted {
                sequence: 2,
                node_id: node_id.clone(),
                attempt: 2,
                started_at: "attempt-2".to_owned(),
                repository_fingerprint: "tree-1".to_owned(),
            })
            .expect_err("repair budget must be hard bounded");
        assert!(error.message.contains("cannot start repair beyond"));
        assert_eq!(snapshot.budget.usage_for(&node_id).repair_attempts, 1);
    }

    #[test]
    fn tiny_first_repair_is_counted_once_by_the_authoritative_event() {
        let mut graph = ExecutionGraph::from_targets(
            "tiny-repair",
            MissionComplexity::Tiny,
            "tree-1",
            &[target("src/tiny.rs", "production")],
            &[],
            &MissionBudget::for_complexity(MissionComplexity::Tiny),
        );
        let node_id = graph
            .nodes
            .iter()
            .find(|node| node.kind.is_mutation())
            .expect("tiny mutation node")
            .id
            .clone();
        graph
            .set_node_status(&node_id, ExecutionNodeStatus::FailedRecoverable)
            .unwrap();
        let mut snapshot = ExecutionSnapshot {
            run_id: "tiny-repair".into(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".into(),
                ..RepositorySnapshot::default()
            },
            graph,
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Tiny)),
            ..ExecutionSnapshot::default()
        };
        snapshot
            .append_event(ExecutionDomainEvent::NodeStarted {
                sequence: 1,
                node_id: node_id.clone(),
                attempt: 1,
                started_at: "first-repair".into(),
                repository_fingerprint: "tree-1".into(),
            })
            .expect("first tiny repair starts");
        assert_eq!(snapshot.budget.usage_for(&node_id).repair_attempts, 1);
        assert_eq!(
            snapshot.graph.node(&node_id).map(|node| node.status),
            Some(ExecutionNodeStatus::Running),
            "a repeated production decision is idempotent because it emits no second NodeStarted"
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter(|event| matches!(event, ExecutionDomainEvent::NodeStarted { .. }))
                .count(),
            1
        );

        snapshot
            .append_event(ExecutionDomainEvent::MutationRepairAllowanceRestored {
                sequence: 2,
                node_id: node_id.clone(),
            })
            .expect("provider-contract violation restores the allowance");
        assert_eq!(snapshot.budget.usage_for(&node_id).repair_attempts, 0);
        assert!(mutation_repair_allowance_is_restored(
            &snapshot.events,
            &node_id
        ));

        let encoded = serde_json::to_vec(&snapshot).expect("serialize restored snapshot");
        let mut resumed: ExecutionSnapshot =
            serde_json::from_slice(&encoded).expect("resume restored snapshot");
        assert!(mutation_repair_allowance_is_restored(
            &resumed.events,
            &node_id
        ));
        resumed
            .append_event(ExecutionDomainEvent::MutationRepairAllowanceConsumed {
                sequence: 3,
                node_id: node_id.clone(),
            })
            .expect("compatible retry re-consumes the allowance after restart");
        assert_eq!(resumed.budget.usage_for(&node_id).repair_attempts, 1);
        assert!(!mutation_repair_allowance_is_restored(
            &resumed.events,
            &node_id
        ));
    }

    #[test]
    fn progress_extends_soft_budget_but_never_the_hard_budget() {
        let node_id = ExecutionNodeId::new("target-1");
        let node_budget = NodeBudget {
            max_model_calls: 10,
            max_cost_micros: 10_000,
            max_duration: Duration::from_secs(100),
            max_repair_attempts: 1,
        };
        let mut state = BudgetState::new(MissionBudget {
            max_model_calls: 20,
            max_cost_micros: 20_000,
            max_duration: Duration::from_secs(200),
            max_target_repair_rounds: 2,
        });
        for _ in 0..8 {
            state.record_model_call(node_id.clone(), 100, Duration::from_secs(1));
        }
        assert!(state.should_stop_node(&node_id, &node_budget));
        state.record_progress_kind(
            1,
            ProgressEventKind::SourceMutationApplied,
            Some(node_id.clone()),
        );
        state.record_model_call(node_id.clone(), 100, Duration::from_secs(1));
        assert!(!state.should_stop_node(&node_id, &node_budget));
        state.record_model_call(node_id.clone(), 100, Duration::from_secs(1));
        assert!(state.should_stop_node(&node_id, &node_budget));
    }

    #[test]
    fn newer_attempt_resumes_from_a_cancellation_checkpoint_via_event() {
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-cancelled".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: graph(),
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        snapshot
            .append_event(ExecutionDomainEvent::CancellationRequested {
                sequence: 1,
                state: CancellationState {
                    requested_at: "attempt-1".to_owned(),
                    reason: "user requested cancellation".to_owned(),
                    checkpointed: true,
                    ..CancellationState::default()
                },
            })
            .expect("checkpoint cancellation");
        assert!(snapshot.cancellation.is_some());
        assert!(!snapshot.is_terminal());

        snapshot
            .append_event(ExecutionDomainEvent::ExecutionResumed {
                sequence: 2,
                execution_attempt: 2,
                previous_outcome: None,
            })
            .expect("resume newer attempt");

        assert!(snapshot.cancellation.is_none());
        assert!(!snapshot.is_terminal());
        snapshot.validate_invariants().expect("resumed snapshot");
    }

    #[test]
    fn execution_resume_requires_a_cancellation_checkpoint() {
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-active".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: graph(),
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };

        let error = snapshot
            .append_event(ExecutionDomainEvent::ExecutionResumed {
                sequence: 1,
                execution_attempt: 2,
                previous_outcome: None,
            })
            .expect_err("active execution must not emit a resume event");
        assert!(
            error
                .message
                .contains("cancellation checkpoint or partial-reviewable")
        );
    }

    #[test]
    fn terminal_event_prevents_domain_result_replacement() {
        let mut snapshot = ExecutionSnapshot {
            run_id: "run-1".to_owned(),
            current_repository: RepositorySnapshot {
                fingerprint: "tree-1".to_owned(),
                ..RepositorySnapshot::default()
            },
            graph: graph(),
            budget: BudgetState::new(MissionBudget::for_complexity(MissionComplexity::Small)),
            ..ExecutionSnapshot::default()
        };
        snapshot
            .append_event(ExecutionDomainEvent::RunFinished {
                sequence: 1,
                outcome: MissionOutcome::BlockedNoDiff,
            })
            .expect("finish run");
        let error = snapshot
            .append_event(ExecutionDomainEvent::RunFinished {
                sequence: 2,
                outcome: MissionOutcome::FailedInfrastructure,
            })
            .expect_err("terminal result is authoritative");
        assert!(error.message.contains("after RunFinished"));
        assert_eq!(
            snapshot.terminal_outcome(),
            Some(MissionOutcome::BlockedNoDiff)
        );
    }
