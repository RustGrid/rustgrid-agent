// Extracted from the hosted execution composition root.
use super::*;

pub(super) fn find_or_create_hosted_pull_request(
    github: &GitHubClient,
    repo_config: &RepoConfig,
    manifest: &HostedManifest,
    validation: &[ValidationResult],
    completeness: &CompletionEvaluation,
    draft: bool,
) -> Result<crate::github::PullRequest> {
    let title = hosted_pull_request_title(manifest, draft);
    let body = hosted_pull_request_body(manifest, validation, completeness);
    if let Some(pull) = github.find_open_pull_request(repo_config, &manifest.github.branch)? {
        let pull = github.update_pull_request(repo_config, pull.number, &title, &body)?;
        return ensure_hosted_pull_request_draft_state(
            github,
            repo_config,
            &manifest.github.branch,
            pull,
            draft,
        );
    }
    let pull = match github.create_pull_request_with_draft(
        repo_config,
        &title,
        &body,
        &manifest.github.branch,
        normalized_base_ref(&manifest.github.base_ref)?,
        draft,
    ) {
        Ok(pull) => pull,
        Err(create_error) => {
            // POST retries are inherently ambiguous: GitHub may have created
            // the pull request before a response was lost. Resolve by the
            // deterministic head branch before surfacing the original error.
            match github.find_open_pull_request(repo_config, &manifest.github.branch) {
                Ok(Some(pull)) => pull,
                _ => return Err(create_error),
            }
        }
    };
    ensure_hosted_pull_request_draft_state(
        github,
        repo_config,
        &manifest.github.branch,
        pull,
        draft,
    )
}

pub(super) fn ensure_hosted_pull_request_draft_state(
    github: &GitHubClient,
    repo_config: &RepoConfig,
    branch: &str,
    pull: crate::github::PullRequest,
    draft: bool,
) -> Result<crate::github::PullRequest> {
    if pull.draft == draft {
        return Ok(pull);
    }
    let node_id = pull
        .node_id
        .as_deref()
        .context("GitHub pull request response has no node identity")?;
    github.set_pull_request_draft(node_id, draft)?;
    let confirmed = github
        .find_open_pull_request(repo_config, branch)?
        .context("GitHub pull request disappeared while confirming its draft state")?;
    if confirmed.number != pull.number {
        bail!(
            "GitHub draft-state confirmation returned pull request #{} instead of #{}",
            confirmed.number,
            pull.number
        );
    }
    if confirmed.draft != draft {
        bail!(
            "GitHub pull request #{} did not reach the requested draft state",
            confirmed.number
        );
    }
    Ok(confirmed)
}

pub(super) fn hosted_pull_request_title(manifest: &HostedManifest, draft: bool) -> String {
    format!(
        "{}{}: {}",
        if draft { "[INCOMPLETE] " } else { "" },
        manifest.ticket_key,
        manifest.ticket_title
    )
}

pub(super) struct HostedPublicationContext<'a> {
    pub(super) api: &'a HostedApiClient,
    pub(super) manifest: &'a HostedManifest,
    pub(super) repo: &'a Repo,
    pub(super) repo_config: &'a RepoConfig,
    pub(super) trusted_git_config: &'a [u8],
    pub(super) containment: &'a command::HostedProcessContainment,
    pub(super) validation_round: &'a mut u32,
}

pub(super) fn publish_hosted_branch(
    agent: &mut GatewayAgent<'_>,
    context: HostedPublicationContext<'_>,
    commit: &mut String,
    validation: &mut Vec<ValidationResult>,
    implementation: &mut ImplementationOutcome,
    completeness: &mut CompletionEvaluation,
) -> Result<()> {
    let HostedPublicationContext {
        api,
        manifest,
        repo,
        repo_config,
        trusted_git_config,
        containment,
        validation_round,
    } = context;
    for attempt in 1..=3 {
        agent.ensure_active_or_checkpoint_cancellation()?;
        agent.reconcile_wall_clock_boundary(HostedWallClockBoundary::PublicationReconciliation)?;
        ensure_hosted_repository_integrity(
            repo,
            repo_config,
            manifest,
            trusted_git_config,
            commit,
        )?;
        containment.drain()?;
        let reconcile_result = (|| {
            let token = api.github_token(&manifest.github.repository)?;
            repo.reconcile_remote_branch(
                &manifest.github.branch,
                commit,
                token.expose(),
                &manifest.github.web_base_url,
            )
        })();
        agent.ensure_active_or_checkpoint_cancellation()?;
        let reconciled = reconcile_result?;
        let requires_validation = reconciled.requires_validation();
        *commit = reconciled.commit;
        if requires_validation {
            *validation_round = validation_round.saturating_add(1);
            api.append_event(
                "progress",
                json!({
                    "step": "branch_reconciliation",
                    "status": "completed",
                    "head_sha": commit,
                    "publication_attempt": attempt
                }),
            )?;
            let reconciled_fingerprint =
                repository_state_fingerprint(repo, &manifest.github.base_sha)?;
            implementation.explicit_declaration = None;
            agent.invalidate_finalization_after_remote_reconciliation(&reconciled_fingerprint)?;
            agent.reconcile_authoritative_target_state()?;

            let validation_result =
                run_graph_validation_sequence(agent, api, manifest, repo, *validation_round);
            agent.ensure_active_or_checkpoint_cancellation()?;
            *validation = validation_result?;
            if validation.iter().any(|result| result.status != "passed") {
                bail!("required hosted execution validation failed after branch reconciliation");
            }
            repo.ensure_safe(false)?;

            let review_paths = agent.deterministic_diff_review()?;
            implementation.explicit_declaration = deterministic_complete_declaration(
                &agent.notebook.planned_changes,
                &agent.notebook.acceptance_criteria,
                &review_paths,
                &agent.notebook.remaining_work_v2,
                &agent.tool_failures,
            )
            .or_else(|| {
                implementation.budget_exhausted.then(|| {
                    deterministic_partial_declaration(
                        &agent.notebook.planned_changes,
                        &review_paths,
                        &agent.notebook.remaining_work_v2,
                    )
                })?
            });
            agent.declaration = implementation.explicit_declaration.clone();
            *completeness = agent.evaluate_completion(implementation, validation, &review_paths)?;
            let completion_outcome = mission_outcome_from_completion(completeness.status);
            agent.record_completion_evaluated(
                completeness,
                review_paths,
                implementation.explicit_declaration.clone(),
                "remote_reconciliation_completion_evaluated",
                true,
            )?;
            let publication_decision = agent.reconcile_execution_and_apply()?;
            let selected_mode = match publication_decision.decision {
                ExecutionDecision::Publish { mode } => mode,
                ref decision => bail!(
                    "hosted orchestrator returned `{}` after reconciled completion evaluation",
                    execution_decision_name(decision)
                ),
            };
            if completion_outcome.publication_mode() != Some(selected_mode) {
                bail!("reconciled completion outcome selected an inconsistent publication mode");
            }
            let publication_node =
                agent.graph_node_id(crate::execution_graph::ExecutionNodeKind::Publication)?;
            agent.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::CommitCreated {
                    sequence: agent.next_domain_event_sequence(),
                    node_id: publication_node,
                    commit_sha: commit.clone(),
                },
            )?;
            agent.persist_orchestration_checkpoint(
                "remote_reconciliation_publication_reauthorized",
                true,
            )?;
        }
        ensure_hosted_repository_integrity(
            repo,
            repo_config,
            manifest,
            trusted_git_config,
            commit,
        )?;
        containment.drain()?;
        let push_result = (|| {
            let token = api.github_token(&manifest.github.repository)?;
            repo.push(
                &manifest.github.branch,
                commit,
                token.expose(),
                &manifest.github.web_base_url,
            )
        })();
        agent.ensure_active_or_checkpoint_cancellation()?;
        match push_result {
            Ok(_) => return Ok(()),
            Err(error) if attempt < 3 && error.downcast_ref::<RemoteBranchMoved>().is_some() => {
                api.append_event(
                    "progress",
                    json!({
                        "step": "branch_reconciliation",
                        "status": "retrying",
                        "publication_attempt": attempt + 1
                    }),
                )?;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("bounded hosted publication loop always returns")
}

pub(super) fn ensure_hosted_repository_integrity(
    repo: &Repo,
    repo_config: &RepoConfig,
    manifest: &HostedManifest,
    trusted_git_config: &[u8],
    expected_head: &str,
) -> Result<()> {
    if repo.hosted_local_config()? != trusted_git_config {
        bail!("repository-controlled execution modified the protected local Git configuration");
    }
    repo.verify_hosted_origin(
        &repo_config.owner,
        &repo_config.name,
        &manifest.github.web_base_url,
    )?;
    if command::checked("git", ["rev-parse", "HEAD"], &repo.root)? != expected_head {
        bail!("repository-controlled execution modified Git history before publication");
    }
    Ok(())
}

pub(super) fn ensure_cancellation_repository_integrity(
    repo: &Repo,
    repo_config: &RepoConfig,
    manifest: &HostedManifest,
    trusted_git_config: &[u8],
) -> Result<()> {
    let short_id = manifest.execution.execution_id.simple().to_string();
    let expected_branch = format!(
        "rustgrid/{}-{}",
        manifest.ticket_key.to_ascii_lowercase(),
        &short_id[..8]
    );
    if manifest.github.branch != expected_branch || !safe_git_ref(&manifest.github.branch) {
        bail!("execution manifest branch is not deterministic or safe");
    }
    if repo.hosted_local_config()? != trusted_git_config {
        bail!("repository-controlled execution modified the protected local Git configuration");
    }
    repo.verify_hosted_origin(
        &repo_config.owner,
        &repo_config.name,
        &manifest.github.web_base_url,
    )?;
    Ok(())
}

pub(super) fn finalization_invalidation_event(
    checkpoint: &HostedOrchestrationCheckpoint,
    sequence: u64,
    repository_fingerprint: &str,
) -> Result<crate::execution_graph::ExecutionDomainEvent> {
    checkpoint
        .graph
        .as_ref()
        .context("remote reconciliation requires an authoritative execution graph")?;
    let snapshot = checkpoint.snapshot(
        "remote-reconciliation-finalization-invalidation",
        crate::execution_graph::RepositorySnapshot {
            fingerprint: repository_fingerprint.to_owned(),
            ..crate::execution_graph::RepositorySnapshot::default()
        },
    );
    Ok(
        crate::execution_graph::ExecutionDomainEvent::FinalizationInvalidated {
            sequence,
            repository_fingerprint: repository_fingerprint.to_owned(),
            stale_validation_evidence_ids: snapshot.finalization_validation_evidence_ids(),
        },
    )
}

pub(super) fn validate_reconciled_finalization_route(
    checkpoint: &HostedOrchestrationCheckpoint,
    revalidation: &FinalizationRevalidation,
    repository_fingerprint: &str,
) -> Result<()> {
    use crate::execution_graph::{ExecutionDomainEvent, ValidationEvidenceStatus};

    if repository_fingerprint != revalidation.repository_fingerprint {
        bail!(
            "publication revalidation fingerprint changed from {} to {}",
            revalidation.repository_fingerprint,
            repository_fingerprint
        );
    }
    let events = checkpoint
        .domain_events
        .iter()
        .filter(|event| event.sequence() > revalidation.invalidated_after_sequence)
        .collect::<Vec<_>>();
    let graph = checkpoint
        .graph
        .as_ref()
        .context("publication revalidation lost its execution graph")?;
    for node in graph
        .nodes
        .iter()
        .filter(|node| node.required && node.kind.is_validation())
    {
        let has_current_pass = events.iter().any(|event| {
            matches!(
                event,
                ExecutionDomainEvent::ValidationPassed {
                    node_id,
                    evidence_id,
                    ..
                } if node_id == &node.id
                    && checkpoint.evidence.validations.get(evidence_id).is_some_and(|evidence| {
                        evidence.status == ValidationEvidenceStatus::Passed
                            && evidence.repository_fingerprint == repository_fingerprint
                    })
            )
        });
        if !has_current_pass {
            bail!(
                "publication revalidation has no current validation proof for node `{}`",
                node.id
            );
        }
    }
    if !events
        .iter()
        .any(|event| matches!(event, ExecutionDomainEvent::DiffReviewed { .. }))
    {
        bail!("publication revalidation did not perform deterministic diff review");
    }
    if !events
        .iter()
        .any(|event| matches!(event, ExecutionDomainEvent::CompletionEvaluated { .. }))
    {
        bail!("publication revalidation did not reevaluate completion");
    }
    if !events
        .iter()
        .any(|event| matches!(event, ExecutionDomainEvent::PublicationStarted { .. }))
    {
        bail!("publication revalidation did not re-enter graph-selected publication");
    }
    Ok(())
}

pub(super) fn restored_validation_results_from_snapshot(
    snapshot: &crate::execution_graph::ExecutionSnapshot,
) -> Result<Vec<ValidationResult>> {
    use crate::execution_graph::{FailureCategory, ValidationEvidenceStatus};

    let satisfied = snapshot.dependency_satisfaction_ids();
    let infrastructure_partial = snapshot.has_partial_reviewable_guardrail()
        && snapshot.failures.unresolved().next().is_some()
        && snapshot
            .failures
            .unresolved()
            .all(|failure| failure.category == FailureCategory::InfrastructureFailure);
    let mut results = Vec::new();
    for node in snapshot
        .graph
        .nodes
        .iter()
        .filter(|node| node.required && node.kind.is_validation())
    {
        if !infrastructure_partial && !satisfied.contains(&node.id) {
            bail!(
                "required validation node `{}` has no effective current pass",
                node.id
            );
        }
        let gate = node
            .validation
            .as_ref()
            .context("effective validation node has no gate specification")?;
        let fingerprint = gate.fingerprint(&snapshot.current_repository.fingerprint);
        let evidence = snapshot
            .evidence
            .validations
            .values()
            .rev()
            .find(|evidence| {
                evidence.node_id == node.id
                    && (evidence.status == ValidationEvidenceStatus::Passed
                        || infrastructure_partial
                            && matches!(
                                evidence.status,
                                ValidationEvidenceStatus::Failed
                                    | ValidationEvidenceStatus::TimedOut
                                    | ValidationEvidenceStatus::Cancelled
                            ))
                    && evidence.fingerprint == fingerprint
                    && evidence.repository_fingerprint == snapshot.current_repository.fingerprint
            })
            .with_context(|| {
                format!(
                    "effective validation node `{}` has no current {} evidence",
                    node.id,
                    if infrastructure_partial {
                        "process observation"
                    } else {
                        "passed"
                    }
                )
            })?;
        results.push(ValidationResult {
            id: evidence.gate_id.clone(),
            command: evidence.command.clone(),
            status: match evidence.status {
                ValidationEvidenceStatus::Passed => "passed",
                ValidationEvidenceStatus::Failed => "failed",
                ValidationEvidenceStatus::TimedOut => "timed_out",
                ValidationEvidenceStatus::Cancelled => "cancelled",
                ValidationEvidenceStatus::Running | ValidationEvidenceStatus::Superseded => {
                    unreachable!("restored validation evidence must be terminal")
                }
            }
            .into(),
            output: evidence.output_summary.clone(),
        });
    }
    Ok(results)
}
