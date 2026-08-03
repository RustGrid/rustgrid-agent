// Extracted from the hosted execution composition root.
use super::*;

pub(super) const fn validation_entry_allows_gates(decision: ValidationEntryDecision) -> bool {
    matches!(
        decision,
        ValidationEntryDecision::CompleteImplementation
            | ValidationEntryDecision::UsefulPartialImplementation
            | ValidationEntryDecision::ResumedImplementation
    )
}

pub(super) fn validation_failure_category(
    status: &str,
) -> Option<crate::execution_graph::FailureCategory> {
    match status {
        "cancelled" | "pending" | "ready" | "skipped" | "superseded" => None,
        "infrastructure_failed" | "timed_out" => {
            Some(crate::execution_graph::FailureCategory::InfrastructureFailure)
        }
        _ => Some(crate::execution_graph::FailureCategory::ValidationFailure),
    }
}

pub(super) fn validation_failure_target_hint(
    mutation_target_paths: &[String],
    diagnostics: &str,
) -> Option<String> {
    let matching_paths = mutation_target_paths
        .iter()
        .filter(|path| diagnostics.contains(path.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();
    if matching_paths.len() != 1 {
        return None;
    }
    matching_paths.into_iter().next()
}

pub(super) fn parse_validation_assertion_failures(
    command: &str,
    output: &str,
    target_contents: &[(String, String)],
) -> Vec<crate::execution_graph::ValidationAssertionFailure> {
    let clean_output = strip_ansi_sequences(output);
    let lines = clean_output.lines().collect::<Vec<_>>();
    let mut failures = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !line.contains("AssertionError:") {
            continue;
        }
        let end = lines[index + 1..]
            .iter()
            .position(|candidate| candidate.contains("AssertionError:"))
            .map_or(lines.len(), |offset| index + 1 + offset);
        let block = &lines[index..end];
        let expected = block
            .iter()
            .find_map(|candidate| candidate.trim().strip_prefix("Expected:"))
            .map_or_else(String::new, |value| {
                value.trim().trim_matches('"').to_owned()
            });
        let received = block
            .iter()
            .find_map(|candidate| candidate.trim().strip_prefix("Received:"))
            .map_or_else(String::new, |value| {
                value.trim().trim_matches('"').to_owned()
            });
        let source_location = block
            .iter()
            .find(|candidate| candidate.contains('❯'))
            .map_or_else(String::new, |value| {
                value.trim().trim_start_matches('❯').trim().to_owned()
            });
        let (location_path, source_line, source_column) = parse_source_location(&source_location);
        let test_file = target_contents
            .iter()
            .find(|(path, _)| location_path == *path || command.contains(path.as_str()))
            .map_or_else(|| location_path.clone(), |(path, _)| path.clone());
        let heading = lines[..index]
            .iter()
            .rev()
            .find(|candidate| {
                let candidate = candidate.trim();
                candidate.contains(" > ")
                    || candidate.starts_with('×')
                    || candidate.starts_with("FAIL ")
            })
            .copied()
            .unwrap_or_default();
        let (suite_path, test_name) = parse_vitest_heading(heading, &test_file);
        let assertion = line
            .split_once("AssertionError:")
            .map_or(line.trim(), |(_, message)| message.trim());
        let assertion_kind = if assertion.contains("to contain") {
            "contains"
        } else if assertion.contains("to be") || assertion.contains("to equal") {
            "equality"
        } else {
            "assertion"
        }
        .to_owned();
        let assertion_tokens = assertion
            .split(|character: char| !character.is_alphanumeric() && character != '-')
            .filter(|token| token.len() >= 4)
            .collect::<Vec<_>>();
        let test_content = target_contents
            .iter()
            .find(|(path, _)| path == &test_file)
            .map(|(_, content)| content.as_str())
            .unwrap_or_default();
        let imported_stems = import_path_stems(test_content);
        let mut implicated_paths = target_contents
            .iter()
            .filter(|(path, content)| {
                path == &test_file
                    || (!expected.is_empty() && content.contains(&expected))
                    || (!received.is_empty() && content.contains(&received))
                    || assertion_tokens.iter().any(|token| content.contains(token))
                    || imported_stems.iter().any(|stem| path_stem(path) == *stem)
            })
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        if implicated_paths.is_empty() && !test_file.is_empty() {
            implicated_paths.insert(test_file.clone());
        }
        failures.push(crate::execution_graph::ValidationAssertionFailure {
            test_file,
            suite_path,
            test_name,
            source_location,
            source_line,
            source_column,
            assertion_kind,
            expected,
            received,
            implicated_paths: implicated_paths.into_iter().collect(),
            diagnosis: None,
            evidence: assertion.to_owned(),
            context: block
                .iter()
                .take(12)
                .copied()
                .collect::<Vec<_>>()
                .join("\n"),
            proposed_repair: String::new(),
            expected_validation_effect: String::new(),
        });
    }
    failures
}

pub(super) fn looks_like_structured_test_failure(output: &str) -> bool {
    let output = strip_ansi_sequences(output);
    output.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("FAIL ") || line.starts_with('×')
    }) && (output.contains("Expected:") || output.contains("Received:"))
}

pub(super) fn structured_validation_paths(output: &str) -> BTreeSet<String> {
    let output = strip_ansi_sequences(output);
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches('×').trim();
            if let Some(heading) = line.strip_prefix("FAIL ") {
                return heading
                    .split(" > ")
                    .next()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(str::to_owned);
            }
            let location = line.strip_prefix('❯')?.trim();
            let (path, source_line, source_column) = parse_source_location(location);
            (source_line.is_some() && source_column.is_some() && !path.is_empty()).then_some(path)
        })
        .take(4)
        .collect()
}

pub(super) fn fallback_validation_assertion_failure(
    command: &str,
    output: &str,
    target_contents: &[(String, String)],
) -> Option<crate::execution_graph::ValidationAssertionFailure> {
    let clean = strip_ansi_sequences(output);
    let lines = clean.lines().collect::<Vec<_>>();
    let heading = lines.iter().find(|line| {
        let line = line.trim_start();
        line.starts_with("FAIL ") || line.starts_with('×')
    })?;
    let source_location = lines
        .iter()
        .find(|line| {
            line.contains('❯')
                && line
                    .rsplit(':')
                    .take(2)
                    .all(|part| part.trim().parse::<u32>().is_ok())
        })
        .map_or_else(String::new, |line| {
            line.trim().trim_start_matches('❯').trim().to_owned()
        });
    let (location_path, source_line, source_column) = parse_source_location(&source_location);
    let test_file = target_contents
        .iter()
        .find(|(path, _)| location_path == *path || command.contains(path.as_str()))
        .map_or(location_path, |(path, _)| path.clone());
    let (suite_path, test_name) = parse_vitest_heading(heading, &test_file);
    let value = |prefix: &str| {
        lines
            .iter()
            .find_map(|line| line.trim().strip_prefix(prefix))
            .map_or_else(String::new, |value| {
                value.trim().trim_matches('"').to_owned()
            })
    };
    Some(crate::execution_graph::ValidationAssertionFailure {
        test_file: test_file.clone(),
        suite_path,
        test_name,
        source_location,
        source_line,
        source_column,
        assertion_kind: "assertion".into(),
        expected: value("Expected:"),
        received: value("Received:"),
        implicated_paths: (!test_file.is_empty())
            .then_some(test_file)
            .into_iter()
            .collect(),
        evidence: "structured validation output did not include a recognized assertion header"
            .into(),
        context: lines
            .iter()
            .take(20)
            .copied()
            .collect::<Vec<_>>()
            .join("\n"),
        ..crate::execution_graph::ValidationAssertionFailure::default()
    })
}

fn strip_ansi_sequences(value: &str) -> String {
    let mut clean = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for candidate in chars.by_ref() {
                if ('@'..='~').contains(&candidate) {
                    break;
                }
            }
        } else {
            clean.push(character);
        }
    }
    clean
}

fn parse_source_location(value: &str) -> (String, Option<u32>, Option<u32>) {
    let mut parts = value.rsplitn(3, ':');
    let column = parts.next().and_then(|part| part.parse().ok());
    let line = parts.next().and_then(|part| part.parse().ok());
    let path = parts.next().unwrap_or(value).trim().to_owned();
    (path, line, column)
}

fn parse_vitest_heading(heading: &str, test_file: &str) -> (Vec<String>, String) {
    let clean = heading
        .trim()
        .trim_start_matches('×')
        .trim_start_matches('❯')
        .trim_start_matches("FAIL")
        .trim();
    let semantic = clean
        .strip_prefix(test_file)
        .unwrap_or(clean)
        .trim()
        .trim_start_matches('>')
        .trim();
    let mut parts = semantic
        .split(" > ")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let test_name = parts.pop().unwrap_or_default();
    (parts, test_name)
}

fn path_stem(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .split('.')
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase()
}

fn import_path_stems(content: &str) -> BTreeSet<String> {
    content
        .lines()
        .filter(|line| line.trim_start().starts_with("import "))
        .filter_map(|line| line.rsplit_once("from").map(|(_, path)| path))
        .map(|path| path.trim().trim_matches(|c| matches!(c, '\'' | '"' | ';')))
        .map(path_stem)
        .collect()
}

pub(super) fn validation_repair_target_hint(
    assertions: &[crate::execution_graph::ValidationAssertionFailure],
    mutation_target_paths: &[String],
    target_contents: &[(String, String)],
) -> Option<String> {
    let mut scores = BTreeMap::<String, usize>::new();
    let highest_specificity = assertions
        .iter()
        .map(assertion_specificity)
        .max()
        .unwrap_or_default();
    for assertion in assertions
        .iter()
        .filter(|assertion| assertion_specificity(assertion) == highest_specificity)
    {
        let test_content = target_contents
            .iter()
            .find(|(path, _)| path == &assertion.test_file)
            .map(|(_, content)| content.as_str())
            .unwrap_or_default();
        let imported_stems = import_path_stems(test_content);
        let semantic_tokens =
            normalized_semantic_tokens(&format!("{} {}", assertion.test_name, assertion.evidence));
        let primary_semantic_token = assertion
            .test_name
            .split(|character: char| !character.is_alphanumeric() && character != '_')
            .find(|token| token.len() >= 5)
            .map(normalize_semantic_token);
        for path in assertion
            .implicated_paths
            .iter()
            .filter(|path| mutation_target_paths.contains(path))
        {
            let content = target_contents
                .iter()
                .find(|(candidate, _)| candidate == path)
                .map(|(_, content)| content.as_str())
                .unwrap_or_default();
            let normalized_content = content.to_ascii_lowercase();
            let source_preference = usize::from(!is_test_path(path)) * 4;
            let direct_import = usize::from(imported_stems.contains(&path_stem(path))) * 6;
            let expected_evidence = usize::from(
                !assertion.expected.is_empty() && content.contains(&assertion.expected),
            ) * 2;
            let received_evidence = usize::from(
                !assertion.received.is_empty() && content.contains(&assertion.received),
            ) * 2;
            let paired_value_evidence =
                usize::from(expected_evidence > 0 && received_evidence > 0) * 2;
            let semantic_overlap = semantic_tokens
                .iter()
                .filter(|token| normalized_content.contains(token.as_str()))
                .count()
                .min(4)
                * 3;
            let primary_semantic_overlap = usize::from(
                primary_semantic_token
                    .as_ref()
                    .is_some_and(|token| normalized_content.contains(token)),
            ) * 4;
            *scores.entry(path.clone()).or_default() += 1
                + source_preference
                + direct_import
                + expected_evidence
                + received_evidence
                + paired_value_evidence
                + semantic_overlap
                + primary_semantic_overlap;
        }
    }
    scores
        .into_iter()
        .max_by(|(left_path, left_score), (right_path, right_score)| {
            left_score
                .cmp(right_score)
                .then_with(|| right_path.cmp(left_path))
        })
        .map(|(path, _)| path)
}

pub(super) fn assertion_specificity(
    assertion: &crate::execution_graph::ValidationAssertionFailure,
) -> usize {
    match assertion.assertion_kind.as_str() {
        "equality" => 3,
        "contains" => 2,
        _ => 1,
    }
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower
        .split('/')
        .any(|component| matches!(component, "test" | "tests" | "spec" | "specs"))
        || lower.contains(".test.")
        || lower.contains(".spec.")
        || lower.contains("_test.")
}

fn normalized_semantic_tokens(value: &str) -> BTreeSet<String> {
    value
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| token.len() >= 5)
        .map(normalize_semantic_token)
        .collect()
}

fn normalize_semantic_token(token: &str) -> String {
    let mut token = token.to_ascii_lowercase();
    if token.ends_with("ies") && token.len() > 5 {
        token.truncate(token.len() - 3);
        token.push('y');
    } else if token.ends_with("es") && token.len() > 5 {
        token.truncate(token.len() - 1);
    } else if token.ends_with('s') && token.len() > 5 {
        token.pop();
    }
    token
}

pub(super) fn committed_head_for_publication(
    repo: &Repo,
    base_sha: &str,
) -> Result<Option<(String, Vec<String>)>> {
    let changed_paths = completion_changed_paths(repo, base_sha)?;
    if changed_paths.is_empty() {
        return Ok(None);
    }
    let commit = command::checked("git", ["rev-parse", "HEAD"], &repo.root)?;
    Ok(Some((commit, changed_paths)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RecoveryPublicationAuthorization {
    pub(super) publication_node_id: crate::execution_graph::ExecutionNodeId,
    pub(super) repository_fingerprint: String,
    pub(super) changed_paths: Vec<String>,
    pub(super) validation_evidence_ids: Vec<String>,
    pub(super) already_requested: bool,
}

pub(super) fn authorize_recovery_publication(
    snapshot: &crate::execution_graph::ExecutionSnapshot,
    manifest: &HostedManifest,
) -> Result<RecoveryPublicationAuthorization> {
    use crate::execution_graph::{
        ExecutionDomainEvent, ExecutionNodeKind, FailureCategory, MissionOutcome, PublicationStatus,
    };

    if snapshot.terminal_outcome().is_some() {
        bail!("recovery publication cannot replace a terminal domain result");
    }
    if snapshot.cancellation.is_some() {
        bail!("recovery publication is forbidden after cancellation was requested");
    }
    let partial_validation = (snapshot.has_partial_reviewable_guardrail()
        || snapshot.has_incomplete_diff_review_request())
        && snapshot.failures.unresolved().all(|failure| {
            matches!(
                failure.category,
                FailureCategory::ValidationFailure | FailureCategory::InfrastructureFailure
            )
        })
        && snapshot
            .graph
            .nodes
            .iter()
            .filter(|node| node.required && node.kind.is_mutation())
            .all(|node| node.status.satisfies_dependency());
    if (snapshot
        .failures
        .unresolved()
        .any(|failure| failure.category == FailureCategory::InfrastructureFailure)
        || snapshot.events.iter().any(|event| {
            matches!(
                event,
                ExecutionDomainEvent::GuardrailTriggered {
                    outcome: MissionOutcome::FailedInfrastructure,
                    ..
                }
            )
        }))
        && !partial_validation
    {
        bail!("recovery publication is forbidden after an infrastructure failure");
    }
    if snapshot.publication.is_published()
        || snapshot.publication.status == PublicationStatus::PullRequestCreated
    {
        bail!("recovery publication cannot replace completed publication");
    }
    if !snapshot.current_repository.has_changes() {
        bail!("recovery publication requires a non-empty current repository diff");
    }

    let required_gate_ids = manifest
        .execution_policy
        .quality_gates
        .iter()
        .filter(|gate| gate.required && !recovery_bootstrap_command(&gate.command))
        .map(|gate| gate.id.clone())
        .collect::<BTreeSet<_>>();
    if required_gate_ids.is_empty() {
        bail!("recovery publication requires at least one required validation gate");
    }
    let graph_gate_ids = snapshot
        .graph
        .nodes
        .iter()
        .filter(|node| node.required && node.kind.is_validation())
        .filter_map(|node| node.validation.as_ref().map(|gate| gate.gate_id.clone()))
        .collect::<BTreeSet<_>>();
    if !required_gate_ids.is_subset(&graph_gate_ids) {
        bail!("recovery publication validation graph omits a required hosted gate");
    }
    let validation_evidence_ids = snapshot
        .recovery_publication_validation_evidence_ids()
        .map_err(|error| {
            anyhow!("recovery publication validation proof is not current: {error}")
        })?;
    if validation_evidence_ids.is_empty() {
        bail!("recovery publication requires current passed validation evidence");
    }
    let publication_node_id = snapshot
        .graph
        .nodes
        .iter()
        .find(|node| node.kind == ExecutionNodeKind::Publication)
        .map(|node| node.id.clone())
        .context("recovery publication requires a graph publication node")?;

    Ok(RecoveryPublicationAuthorization {
        publication_node_id,
        repository_fingerprint: snapshot.current_repository.fingerprint.clone(),
        changed_paths: snapshot
            .current_repository
            .changed_paths
            .iter()
            .cloned()
            .collect(),
        validation_evidence_ids,
        already_requested: snapshot.publication.recovery_requested,
    })
}

fn recovery_bootstrap_command(command: &str) -> bool {
    let normalized = command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    [
        "npm ci",
        "npm install",
        "pnpm install",
        "yarn install",
        "bun install",
    ]
    .iter()
    .any(|prefix| normalized == *prefix || normalized.starts_with(&format!("{prefix} ")))
}

pub(super) fn is_hosted_orchestration_invariant_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<HostedInvariantFailure>().is_some()
        || error
            .downcast_ref::<crate::hosted_orchestrator::OrchestrationInvariantError>()
            .is_some()
        || error
            .downcast_ref::<crate::execution_graph::GraphInvariantError>()
            .is_some()
}

pub(super) fn hosted_failure_category(error: &anyhow::Error) -> &'static str {
    if let Some(failure) = error.downcast_ref::<HostedStartupFailure>() {
        return failure.category;
    }
    if error
        .downcast_ref::<HostedHttpError>()
        .is_some_and(|failure| failure.provider_contacted() == Some(true))
    {
        "ai_gateway_failed"
    } else if is_hosted_orchestration_invariant_error(error) {
        "execution_graph_initialization_failed"
    } else {
        "orchestration_initialization_failed"
    }
}

pub(super) fn recovery_execution_is_active(running: &Arc<AtomicBool>) -> bool {
    running.load(Ordering::SeqCst) && !shutdown::requested()
}

pub(super) fn ensure_recovery_execution_active(running: &Arc<AtomicBool>) -> Result<()> {
    if !recovery_execution_is_active(running) {
        bail!("recovery publication stopped because cancellation or shutdown was requested");
    }
    Ok(())
}

pub(super) fn recovery_completion_evaluation(
    agent: &mut GatewayAgent<'_>,
    snapshot: &crate::execution_graph::ExecutionSnapshot,
    implementation: &ImplementationOutcome,
    validation: &[ValidationResult],
    changed_paths: &[String],
    original_error: &anyhow::Error,
) -> CompletionEvaluation {
    let unrecovered = agent
        .tool_failures
        .iter()
        .filter(|failure| !failure.recovered)
        .cloned()
        .collect::<Vec<_>>();
    let mut evaluation = completion_fallback(
        implementation,
        agent.impact_map.as_ref(),
        agent.implementation_plan.as_ref(),
        &unrecovered,
        changed_paths,
        &agent.notebook.acceptance_criteria,
        validation,
        project_verification_policy(agent.manifest),
    );
    evaluation.status = CompletionStatus::Partial;
    evaluation.implementation_completeness = ImplementationCompleteness::Partial;
    evaluation.verification_readiness = if validation.iter().all(|result| result.status == "passed")
    {
        VerificationReadiness::AutomatedVerified
    } else {
        VerificationReadiness::PendingManualReview
    };
    evaluation.evaluation_source = EvaluationSource::OrchestratorFallback;
    evaluation.confidence = 1.0;
    let validation_partial = snapshot.has_incomplete_diff_review_request();
    push_unique(
        &mut evaluation.remaining_implementation_work,
        if validation_partial {
            "Resume from validation repair, reconcile the recorded assertion failures, and rerun the pending gates."
                .into()
        } else {
            "Resume from the persisted execution graph and resolve the internal orchestration invariant."
                .into()
        },
    );
    for node in snapshot
        .remaining_required_nodes()
        .into_iter()
        .filter(|node| node.kind != crate::execution_graph::ExecutionNodeKind::Publication)
    {
        push_unique(
            &mut evaluation.remaining_implementation_work,
            format!(
                "Remaining graph node `{}` ({:?}) is {:?}.",
                node.id, node.kind, node.status
            ),
        );
    }
    evaluation.summary = if validation_partial {
        "RustGrid preserved the applied repository diff in a draft recovery pull request after focused validation failed and bounded repair produced no safe mutation."
            .into()
    } else {
        format!(
            "RustGrid preserved the current validated repository changes in a draft recovery pull request after an internal orchestration invariant failed: {}",
            truncate_text(&original_error.to_string(), 1_000)
        )
    };
    evaluation
}

fn incomplete_validation_results(agent: &GatewayAgent<'_>) -> Vec<ValidationResult> {
    agent
        .notebook
        .required_gates
        .iter()
        .filter(|gate| gate.required)
        .map(|gate| {
            let evidence = agent
                .notebook
                .validation_evidence
                .iter()
                .rev()
                .find(|evidence| evidence.gate_id == gate.gate_id);
            let status = match gate.status {
                ValidationStatus::Passed => "passed",
                ValidationStatus::FailedCode => "failed",
                ValidationStatus::FailedInfrastructure => "infrastructure_failed",
                ValidationStatus::TimedOut => "timed_out",
                ValidationStatus::Cancelled => "cancelled",
                ValidationStatus::Skipped => "skipped",
                ValidationStatus::Superseded => "superseded",
                ValidationStatus::Pending | ValidationStatus::Ready | ValidationStatus::Running => {
                    "pending"
                }
            };
            ValidationResult {
                id: gate.gate_id.clone(),
                command: gate.command.clone(),
                status: status.into(),
                output: evidence.map_or_else(
                    || "required validation gate has not run".into(),
                    |evidence| {
                        truncate_text(
                            &format!(
                                "stdout: {}\nstderr: {}",
                                evidence.stdout_summary, evidence.stderr_summary
                            ),
                            16_000,
                        )
                    },
                ),
            }
        })
        .collect()
}

pub(super) struct RecoveryPublicationContext<'a> {
    pub(super) api: &'a HostedApiClient,
    pub(super) manifest: &'a HostedManifest,
    pub(super) repo: &'a Repo,
    pub(super) repo_config: &'a RepoConfig,
    pub(super) trusted_git_config: &'a [u8],
    pub(super) trusted_head: &'a str,
    pub(super) baseline: &'a BTreeSet<String>,
    pub(super) containment: &'a command::HostedProcessContainment,
    pub(super) running: &'a Arc<AtomicBool>,
    pub(super) startup_mode: StartupMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RecoveryPublicationResult {
    NotApplicable,
    SkippedNoDiff,
    PublishedDraft,
    FailedInfrastructure,
}

pub(super) fn recovery_publication_no_op(
    startup_mode: StartupMode,
    snapshot: &crate::execution_graph::ExecutionSnapshot,
) -> Option<RecoveryPublicationResult> {
    if startup_mode != StartupMode::RecoveryPublicationRun
        && !snapshot.has_partial_reviewable_guardrail()
        && !snapshot.has_incomplete_diff_review_request()
    {
        Some(RecoveryPublicationResult::NotApplicable)
    } else if !snapshot.current_repository.has_changes() {
        Some(RecoveryPublicationResult::SkippedNoDiff)
    } else {
        None
    }
}

pub(super) struct RecoveryPublicationOutcome {
    pub(super) result: RecoveryPublicationResult,
    pub(super) published: Option<HostedResult>,
    pub(super) error: Option<anyhow::Error>,
}

pub(super) fn attempt_safe_recovery_publication(
    agent: &mut GatewayAgent<'_>,
    context: RecoveryPublicationContext<'_>,
    original_error: &anyhow::Error,
) -> RecoveryPublicationOutcome {
    let api = context.api;
    let manifest = context.manifest;
    let repo = context.repo;
    let repo_config = context.repo_config;
    match attempt_safe_recovery_publication_with(
        agent,
        context,
        original_error,
        move |branch_already_pushed, commit| {
            let token = api.github_token(&manifest.github.repository)?;
            if branch_already_pushed {
                let remote_head = repo.remote_branch_head(
                    &manifest.github.branch,
                    token.expose(),
                    &manifest.github.web_base_url,
                )?;
                if remote_head.as_deref() != Some(commit) {
                    bail!("persisted recovery branch no longer points to its authorized commit");
                }
            } else {
                repo.push(
                    &manifest.github.branch,
                    commit,
                    token.expose(),
                    &manifest.github.web_base_url,
                )?;
            }
            Ok(())
        },
        move |validation, completeness| {
            let token = api.github_token(&manifest.github.repository)?;
            let github = GitHubClient::new(token.expose(), &manifest.github.web_base_url)?;
            find_or_create_hosted_pull_request(
                &github,
                repo_config,
                manifest,
                validation,
                completeness,
                true,
                true,
            )
        },
    ) {
        Ok((result, published)) => RecoveryPublicationOutcome {
            result,
            published,
            error: None,
        },
        Err(error) => {
            agent.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.recovery_publication_evaluated",
                    "startup_mode": StartupMode::RecoveryPublicationRun,
                    "persisted_graph_presence": agent.notebook.orchestration.graph.is_some(),
                    "persisted_notebook_revision": agent.notebook.revision,
                    "repository_diff_status": "changed",
                    "branch_state": "persisted",
                    "selected_next_decision": "retry_recovery_publication",
                    "result": RecoveryPublicationResult::FailedInfrastructure,
                    "error": truncate_text(&error.to_string(), 2_000),
                }),
                "recovery publication result",
            );
            RecoveryPublicationOutcome {
                result: RecoveryPublicationResult::FailedInfrastructure,
                published: None,
                error: Some(error),
            }
        }
    }
}

pub(super) fn attempt_safe_recovery_publication_with(
    agent: &mut GatewayAgent<'_>,
    context: RecoveryPublicationContext<'_>,
    original_error: &anyhow::Error,
    synchronize_branch: impl FnOnce(bool, &str) -> Result<()>,
    create_draft_pull_request: impl FnOnce(
        &[ValidationResult],
        &CompletionEvaluation,
    ) -> Result<crate::github::PullRequest>,
) -> Result<(RecoveryPublicationResult, Option<HostedResult>)> {
    use crate::execution_graph::{ExecutionDomainEvent, MissionOutcome, PublicationStatus};

    let RecoveryPublicationContext {
        api,
        manifest,
        repo,
        repo_config,
        trusted_git_config,
        trusted_head,
        baseline,
        containment,
        running,
        startup_mode,
    } = context;
    let snapshot = agent.build_execution_snapshot()?;
    if startup_mode != StartupMode::RecoveryPublicationRun
        && !snapshot.has_partial_reviewable_guardrail()
        && !snapshot.has_incomplete_diff_review_request()
    {
        agent.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.recovery_publication_evaluated",
                "startup_mode": startup_mode,
                "persisted_graph_presence": agent.notebook.orchestration.graph.is_some(),
                "persisted_notebook_revision": agent.notebook.revision,
                "repository_diff_status": "not_evaluated",
                "branch_state": "not_recovery_mode",
                "selected_next_decision": "continue_mission",
                "result": RecoveryPublicationResult::NotApplicable,
            }),
            "recovery publication evaluation",
        );
        return Ok((RecoveryPublicationResult::NotApplicable, None));
    }
    if !recovery_execution_is_active(running) {
        return Ok((RecoveryPublicationResult::NotApplicable, None));
    }
    if recovery_publication_no_op(startup_mode, &snapshot)
        == Some(RecoveryPublicationResult::SkippedNoDiff)
    {
        agent.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.recovery_publication_skipped",
                "result": RecoveryPublicationResult::SkippedNoDiff,
                "reason": "repository diff is empty",
                "mission_outcome": "continuing",
            }),
            "recovery publication no-diff skip",
        );
        agent.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.recovery_publication_evaluated",
                "startup_mode": startup_mode,
                "persisted_graph_presence": agent.notebook.orchestration.graph.is_some(),
                "persisted_notebook_revision": agent.notebook.revision,
                "repository_diff_status": "clean",
                "branch_state": "persisted",
                "selected_next_decision": "continue_mission",
                "result": RecoveryPublicationResult::SkippedNoDiff,
            }),
            "recovery publication evaluation",
        );
        return Ok((RecoveryPublicationResult::SkippedNoDiff, None));
    }
    let authorization = match authorize_recovery_publication(&snapshot, manifest) {
        Ok(authorization) => authorization,
        Err(error) => {
            agent.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.recovery_publication_skipped",
                    "reason": error.to_string(),
                }),
                "recovery publication eligibility",
            );
            agent.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.recovery_publication_evaluated",
                    "startup_mode": startup_mode,
                    "persisted_graph_presence": agent.notebook.orchestration.graph.is_some(),
                    "persisted_notebook_revision": agent.notebook.revision,
                    "repository_diff_status": "changed",
                    "branch_state": "persisted",
                    "selected_next_decision": "continue_mission",
                    "result": RecoveryPublicationResult::NotApplicable,
                    "reason": error.to_string(),
                }),
                "recovery publication evaluation",
            );
            return Ok((RecoveryPublicationResult::NotApplicable, None));
        }
    };
    agent.append_event_recoverable(
        "progress",
        json!({
            "event_type": "worker.recovery_publication_evaluated",
            "startup_mode": startup_mode,
            "persisted_graph_presence": agent.notebook.orchestration.graph.is_some(),
            "persisted_notebook_revision": agent.notebook.revision,
            "repository_diff_status": "changed",
            "branch_state": "persisted",
            "selected_next_decision": "publish_recovery_draft",
            "result": "applicable",
        }),
        "recovery publication evaluation",
    );
    let validation = if snapshot.has_incomplete_diff_review_request() {
        incomplete_validation_results(agent)
    } else {
        agent.restored_validation_results()?
    };
    let mut implementation = agent.reconstruct_implementation_outcome()?;
    implementation.budget_exhausted = true;
    let completeness = recovery_completion_evaluation(
        agent,
        &snapshot,
        &implementation,
        &validation,
        &authorization.changed_paths,
        original_error,
    );

    if !authorization.already_requested {
        agent.append_execution_domain_event(
            ExecutionDomainEvent::RecoveryPublicationRequested {
                sequence: agent.next_domain_event_sequence(),
                node_id: authorization.publication_node_id.clone(),
                repository_fingerprint: authorization.repository_fingerprint.clone(),
                validation_evidence_ids: authorization.validation_evidence_ids.clone(),
            },
        )?;
        agent.persist_orchestration_checkpoint("recovery_publication_requested", true)?;
    }
    agent.append_event_recoverable(
        "progress",
        json!({
            "event_type": "worker.recovery_publication_started",
            "publication_mode": "draft_recovery",
            "changed_paths": authorization.changed_paths,
            "validation_evidence_ids": authorization.validation_evidence_ids,
            "original_failure": truncate_text(&original_error.to_string(), 2_000),
        }),
        "recovery publication start",
    );

    ensure_recovery_execution_active(running)?;
    if repo.hosted_local_config()? != trusted_git_config {
        bail!("recovery publication found modified protected local Git configuration");
    }
    repo.verify_hosted_origin(
        &repo_config.owner,
        &repo_config.name,
        &manifest.github.web_base_url,
    )?;
    let publication = agent.notebook.orchestration.publication.clone();
    let local_head = command::checked("git", ["rev-parse", "HEAD"], &repo.root)?;
    let commit = if let Some(commit) = publication.commit_sha.clone() {
        if commit != local_head {
            bail!("persisted recovery publication commit does not match local HEAD");
        }
        commit
    } else {
        if local_head != trusted_head {
            bail!(
                "repository-controlled execution modified Git history before recovery publication"
            );
        }
        let dirty = repo.new_agent_paths(baseline)?;
        let commit = if dirty.is_empty() {
            committed_head_for_publication(repo, &manifest.github.base_sha)?
                .map(|(commit, _)| commit)
                .context("recovery publication found no committable repository changes")?
        } else {
            repo.commit_paths(
                &dirty,
                &format!(
                    "{}: {} (recovery)",
                    manifest.ticket_key, manifest.ticket_title
                ),
            )?
        };
        if !repo.new_agent_paths(baseline)?.is_empty() {
            bail!("repository changed while preparing the recovery publication commit");
        }
        agent.append_execution_domain_event(ExecutionDomainEvent::CommitCreated {
            sequence: agent.next_domain_event_sequence(),
            node_id: authorization.publication_node_id.clone(),
            commit_sha: commit.clone(),
        })?;
        agent.persist_orchestration_checkpoint("recovery_commit_created", true)?;
        commit
    };
    let post_commit_fingerprint = repository_state_fingerprint(repo, &manifest.github.base_sha)?;
    if post_commit_fingerprint != authorization.repository_fingerprint {
        bail!("repository fingerprint changed after recovery publication authorization");
    }

    ensure_recovery_execution_active(running)?;
    containment.drain()?;
    let publication = agent.notebook.orchestration.publication.clone();
    let branch_already_pushed = matches!(
        publication.status,
        PublicationStatus::BranchPushed | PublicationStatus::PullRequestCreated
    );
    synchronize_branch(branch_already_pushed, &commit)?;
    if !branch_already_pushed {
        agent.append_execution_domain_event(ExecutionDomainEvent::BranchPushed {
            sequence: agent.next_domain_event_sequence(),
            node_id: authorization.publication_node_id.clone(),
            branch: manifest.github.branch.clone(),
        })?;
        agent.persist_orchestration_checkpoint("recovery_branch_pushed", true)?;
    }

    ensure_recovery_execution_active(running)?;
    if repository_state_fingerprint(repo, &manifest.github.base_sha)?
        != authorization.repository_fingerprint
    {
        bail!("repository fingerprint changed before recovery pull request creation");
    }
    api.update_state("creating_pull_request")?;
    containment.drain()?;
    let created = create_draft_pull_request(&validation, &completeness)?;
    agent.append_execution_domain_event(ExecutionDomainEvent::PullRequestCreated {
        sequence: agent.next_domain_event_sequence(),
        node_id: authorization.publication_node_id,
        url: created.html_url.clone(),
        number: Some(created.number),
        draft: true,
    })?;
    agent.finalize_guardrail_outcome(MissionOutcome::PartialReviewable)?;
    agent.persist_orchestration_checkpoint("recovery_run_finished", true)?;

    let terminal_telemetry = TerminalTelemetry {
        model_calls_used: agent.phases.total_calls(),
        input_tokens: agent.cost_guard.input_tokens,
        output_tokens: agent.cost_guard.output_tokens,
        estimated_cost_micros: agent.cost_guard.estimated_cost_micros,
        usage: agent.tool_usage.clone(),
        changed_paths: authorization.changed_paths,
        last_successful_action: agent.last_successful_action.clone(),
        phase_reached: Some(agent.phases.active()),
        plan: agent.notebook.planned_changes.clone(),
        remaining_work: agent.notebook.remaining_work_v2.clone(),
        validation_evidence: agent.notebook.validation_evidence.clone(),
        notebook_revision: agent.notebook.revision,
        discovery_calls: agent.phases.phase_calls(ExecutionPhase::Discovery),
        planning_calls: agent.phases.phase_calls(ExecutionPhase::Planning),
        initial_target_mutation_calls: agent
            .notebook
            .orchestration
            .budget
            .model_call_breakdown
            .initial_target_mutation_calls,
        target_mutation_repair_calls: agent
            .notebook
            .orchestration
            .budget
            .model_call_breakdown
            .target_mutation_repair_calls,
        validation_diagnosis_calls: agent
            .notebook
            .orchestration
            .budget
            .model_call_breakdown
            .validation_diagnosis_calls,
        validation_repair_mutation_calls: agent
            .notebook
            .orchestration
            .budget
            .model_call_breakdown
            .validation_repair_mutation_calls,
        diff_review_calls: agent.phases.phase_calls(ExecutionPhase::DiffReview),
        completion_evaluation_calls: agent
            .phases
            .phase_calls(ExecutionPhase::CompletionEvaluation),
    };
    agent.append_event_recoverable(
        "progress",
        json!({
            "event_type": "worker.recovery_publication_evaluated",
            "startup_mode": startup_mode,
            "persisted_graph_presence": true,
            "persisted_notebook_revision": agent.notebook.revision,
            "repository_diff_status": "changed",
            "branch_state": "persisted",
            "selected_next_decision": "finish_recovery_action",
            "result": RecoveryPublicationResult::PublishedDraft,
        }),
        "recovery publication result",
    );
    Ok((
        RecoveryPublicationResult::PublishedDraft,
        Some(HostedResult {
            summary: completeness.summary.clone(),
            branch: manifest.github.branch.clone(),
            commit,
            pull_request: PullRequestResult {
                number: created.number,
                url: created.html_url,
            },
            validation,
            completeness,
            terminal_telemetry,
        }),
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CancellationBranchPreservation {
    pub(super) commit_sha: String,
    pub(super) changed_paths: Vec<String>,
    pub(super) committed_paths: Vec<String>,
    pub(super) commit_created: bool,
    pub(super) push_performed: bool,
    pub(super) remote_already_current: bool,
}

pub(super) fn preserve_cancellation_branch_with(
    repo: &Repo,
    base_sha: &str,
    branch: &str,
    commit_message: &str,
    push: impl FnOnce(&str, &str) -> Result<bool>,
) -> Result<Option<CancellationBranchPreservation>> {
    if !safe_git_ref(branch) {
        bail!("cancellation checkpoint branch is not a safe Git ref");
    }
    let current_branch = command::checked("git", ["branch", "--show-current"], &repo.root)?;
    if current_branch != branch {
        bail!(
            "cancellation checkpoint branch `{current_branch}` does not match manifest branch `{branch}`"
        );
    }

    let dirty_paths = repo.new_agent_paths(&BTreeSet::new())?;
    let (commit_sha, changed_paths, commit_created) = if dirty_paths.is_empty() {
        let Some((commit_sha, changed_paths)) = committed_head_for_publication(repo, base_sha)?
        else {
            return Ok(None);
        };
        (commit_sha, changed_paths, false)
    } else {
        let commit_sha = repo.commit_paths(&dirty_paths, commit_message)?;
        let changed_paths = completion_changed_paths(repo, base_sha)?;
        (commit_sha, changed_paths, true)
    };

    if !repo.new_agent_paths(&BTreeSet::new())?.is_empty() {
        bail!("repository changed while preparing the cancellation checkpoint commit");
    }
    let current_head = command::checked("git", ["rev-parse", "HEAD"], &repo.root)?;
    if current_head != commit_sha {
        bail!("cancellation checkpoint commit no longer matches local HEAD");
    }
    let push_performed = push(branch, &commit_sha)?;
    Ok(Some(CancellationBranchPreservation {
        commit_sha,
        changed_paths,
        committed_paths: dirty_paths,
        commit_created,
        push_performed,
        remote_already_current: !push_performed,
    }))
}

pub(super) fn dispatch_validation_gates<T>(
    decision: ValidationEntryDecision,
    run: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    if validation_entry_allows_gates(decision) {
        run().map(Some)
    } else {
        Ok(None)
    }
}

pub(super) fn canonical_validation_evidence_status(
    status: ValidationStatus,
) -> crate::execution_graph::ValidationEvidenceStatus {
    match status {
        ValidationStatus::Pending | ValidationStatus::Ready | ValidationStatus::Running => {
            crate::execution_graph::ValidationEvidenceStatus::Running
        }
        ValidationStatus::Passed => crate::execution_graph::ValidationEvidenceStatus::Passed,
        ValidationStatus::FailedCode => crate::execution_graph::ValidationEvidenceStatus::Failed,
        ValidationStatus::FailedInfrastructure | ValidationStatus::TimedOut => {
            crate::execution_graph::ValidationEvidenceStatus::TimedOut
        }
        ValidationStatus::Cancelled => crate::execution_graph::ValidationEvidenceStatus::Cancelled,
        ValidationStatus::Skipped | ValidationStatus::Superseded => {
            crate::execution_graph::ValidationEvidenceStatus::Superseded
        }
    }
}

pub(super) fn canonical_validation_evidence_record(
    node_id: crate::execution_graph::ExecutionNodeId,
    gate: &crate::execution_graph::ValidationGateSpec,
    repository_fingerprint: &str,
    result: &ValidationResult,
    legacy: Option<&ValidationEvidence>,
    fallback_attempt: usize,
    duration: Duration,
) -> crate::execution_graph::ValidationEvidenceRecord {
    let repository_fingerprint = legacy.map_or_else(
        || repository_fingerprint.to_owned(),
        |evidence| evidence.source_tree_hash.clone(),
    );
    let fingerprint = legacy.map_or_else(
        || gate.fingerprint(&repository_fingerprint),
        |evidence| evidence.command_fingerprint.clone(),
    );
    let evidence_id = legacy.map_or_else(
        || {
            format!(
                "{}-{}-a{fallback_attempt}",
                gate.gate_id,
                &fingerprint[..12]
            )
        },
        |evidence| evidence.evidence_id.clone(),
    );
    let status = legacy.map_or_else(
        || match result.status.as_str() {
            "passed" => crate::execution_graph::ValidationEvidenceStatus::Passed,
            "failed" => crate::execution_graph::ValidationEvidenceStatus::Failed,
            "cancelled" => crate::execution_graph::ValidationEvidenceStatus::Cancelled,
            _ => crate::execution_graph::ValidationEvidenceStatus::TimedOut,
        },
        |evidence| canonical_validation_evidence_status(evidence.status),
    );
    let output_summary = legacy.map_or_else(
        || truncate_text(&result.output, 4_000),
        |evidence| {
            truncate_text(
                &format!(
                    "stdout: {}\nstderr: {}",
                    evidence.stdout_summary, evidence.stderr_summary
                ),
                4_000,
            )
        },
    );
    crate::execution_graph::ValidationEvidenceRecord {
        evidence_id,
        node_id,
        gate_id: gate.gate_id.clone(),
        fingerprint,
        repository_fingerprint,
        command: gate.command.clone(),
        working_directory: gate.working_directory.clone(),
        status,
        exit_code: legacy.and_then(|evidence| evidence.exit_code),
        output_summary,
        duration: legacy.map_or(duration, |evidence| {
            Duration::from_millis(evidence.duration_ms)
        }),
    }
}

pub(super) fn run_graph_validation_sequence(
    agent: &mut GatewayAgent<'_>,
    api: &HostedApiClient,
    manifest: &HostedManifest,
    repo: &Repo,
    validation_round: u32,
) -> Result<Vec<ValidationResult>> {
    let mut results = Vec::<ValidationResult>::new();
    let maximum_steps = agent
        .notebook
        .orchestration
        .graph
        .as_ref()
        .map(|graph| {
            graph
                .nodes
                .iter()
                .filter(|node| node.required && node.kind.is_validation())
                .count()
        })
        .unwrap_or_else(|| manifest.execution_policy.quality_gates.len())
        .saturating_add(1);
    for _ in 0..maximum_steps {
        let reconciled = agent.reconcile_execution_and_apply()?;
        let (node_id, gate) = match reconciled.decision {
            ExecutionDecision::RunValidation { node_id, gate } => (node_id, gate),
            ExecutionDecision::ReviewDiff { .. } => return Ok(results),
            ExecutionDecision::StopForGuardrail { outcome, reason } => {
                bail!("hosted validation stopped for guardrail {reason:?} with outcome {outcome:?}")
            }
            decision => {
                bail!(
                    "hosted orchestrator returned `{}` while validation work remained",
                    execution_decision_name(&decision)
                )
            }
        };
        let mut policy = manifest.execution_policy.clone();
        policy
            .quality_gates
            .retain(|candidate| candidate.id == gate.gate_id);
        if policy.quality_gates.is_empty()
            && gate.gate_type == crate::execution_graph::ValidationGateType::FocusedTest
        {
            policy.quality_gates.push(HostedQualityGate {
                id: gate.gate_id.clone(),
                command: gate.command.clone(),
                timeout_seconds: i64::try_from(
                    crate::execution_graph::ValidationTimeoutPolicy::for_gate(
                        crate::execution_graph::ValidationGateType::FocusedTest,
                    )
                    .absolute_timeout
                    .as_secs(),
                )
                .unwrap_or(120),
                required: true,
            });
        } else if policy.quality_gates.len() != 1 {
            bail!(
                "execution graph selected unknown validation gate `{}`",
                gate.gate_id
            );
        }
        let validation_duration_limit = {
            let orchestration = &agent.notebook.orchestration;
            let node = orchestration
                .graph
                .as_ref()
                .and_then(|graph| graph.node(&node_id))
                .with_context(|| {
                    format!(
                        "execution graph selected missing validation node `{}`",
                        node_id.as_str()
                    )
                })?;
            let node_remaining = orchestration
                .budget
                .remaining_for(&node_id, &node.budget)
                .duration;
            let mission_remaining = orchestration
                .budget
                .mission
                .max_duration
                .saturating_sub(orchestration.budget.elapsed);
            node_remaining.min(mission_remaining)
        };
        agent.ensure_active_or_checkpoint_cancellation()?;
        let validation_started = Instant::now();
        let mut validation_ledger = agent.notebook.validation_evidence.clone();
        let mut required_gate_projection = agent.notebook.required_gates.clone();
        let gate_results = run_quality_gates(
            api,
            manifest,
            repo,
            agent.running,
            &policy,
            agent.containment,
            validation_round,
            &mut validation_ledger,
            &mut required_gate_projection,
            &mut agent.tool_usage,
            validation_started,
            validation_duration_limit,
            agent.execution_started_at,
            Duration::from_secs(agent.cost_guard.max_duration_seconds),
        );
        agent
            .notebook
            .orchestration
            .budget
            .record_node_duration(node_id.clone(), validation_started.elapsed());
        let gate_results = gate_results?;
        let gate_result = gate_results
            .iter()
            .find(|result| result.id == gate.gate_id)
            .cloned()
            .with_context(|| {
                format!(
                    "validation gate `{}` completed without a result",
                    gate.gate_id
                )
            })?;
        // Capture the command observation before applying any domain event:
        // event application rematerializes the notebook from canonical state
        // and intentionally discards live projection-only writes.
        let legacy_evidence = validation_ledger
            .iter()
            .rev()
            .find(|evidence| {
                evidence.gate_id == gate.gate_id && evidence.status != ValidationStatus::Running
            })
            .cloned();
        let fallback_attempt = agent
            .notebook
            .orchestration
            .evidence
            .validations
            .values()
            .filter(|evidence| {
                evidence.node_id == node_id
                    && evidence.fingerprint
                        == gate.fingerprint(&agent.notebook.repository_fingerprint)
            })
            .count()
            .saturating_add(1);
        let evidence = canonical_validation_evidence_record(
            node_id.clone(),
            &gate,
            &agent.notebook.repository_fingerprint,
            &gate_result,
            legacy_evidence.as_ref(),
            fallback_attempt,
            validation_started.elapsed(),
        );
        if !agent
            .notebook
            .orchestration
            .evidence
            .validations
            .contains_key(&evidence.evidence_id)
        {
            agent.append_execution_domain_event(
                crate::execution_graph::ExecutionDomainEvent::ValidationEvidenceRecorded {
                    sequence: agent.next_domain_event_sequence(),
                    node_id: node_id.clone(),
                    evidence: evidence.clone(),
                },
            )?;
        }
        for result in gate_results {
            if let Some(existing) = results.iter_mut().find(|existing| existing.id == result.id) {
                *existing = result;
            } else {
                results.push(result);
            }
        }
        if evidence.status == crate::execution_graph::ValidationEvidenceStatus::Passed {
            let already_recorded = agent
                .notebook
                .orchestration
                .domain_events
                .iter()
                .any(|event| {
                    matches!(
                        event,
                        crate::execution_graph::ExecutionDomainEvent::ValidationPassed {
                            evidence_id,
                            ..
                        } if evidence_id == &evidence.evidence_id
                    )
                });
            if !already_recorded {
                let recovered_failures = agent
                    .notebook
                    .orchestration
                    .failures
                    .unresolved_for_node(&node_id)
                    .map(|failure| failure.id.clone())
                    .collect::<Vec<_>>();
                for failure_id in recovered_failures {
                    agent.append_execution_domain_event(
                        crate::execution_graph::ExecutionDomainEvent::FailureRecovered {
                            sequence: agent.next_domain_event_sequence(),
                            node_id: node_id.clone(),
                            failure_id,
                            repository_fingerprint: evidence.repository_fingerprint.clone(),
                        },
                    )?;
                }
                agent.append_execution_domain_event(
                    crate::execution_graph::ExecutionDomainEvent::ValidationPassed {
                        sequence: agent.next_domain_event_sequence(),
                        node_id,
                        evidence_id: evidence.evidence_id,
                        fingerprint: evidence.fingerprint,
                    },
                )?;
            }
        }
        agent.checkpoint_validation_ledger()?;
        agent.ensure_active_or_checkpoint_cancellation()?;
        if results.iter().any(|result| result.status != "passed") {
            if let Some(graph) = agent.notebook.orchestration.graph.as_ref() {
                for pending in graph
                    .nodes
                    .iter()
                    .filter(|node| node.required && node.kind.is_validation())
                {
                    let Some(pending_gate) = pending.validation.as_ref() else {
                        continue;
                    };
                    if results
                        .iter()
                        .any(|result| result.id == pending_gate.gate_id)
                    {
                        continue;
                    }
                    results.push(ValidationResult {
                        id: pending_gate.gate_id.clone(),
                        command: pending_gate.command.clone(),
                        status: match pending.status {
                            crate::execution_graph::ExecutionNodeStatus::Ready => "ready",
                            crate::execution_graph::ExecutionNodeStatus::Skipped => "skipped",
                            crate::execution_graph::ExecutionNodeStatus::Superseded => "superseded",
                            _ => "pending",
                        }
                        .into(),
                        output: "Validation gate has not started.".into(),
                    });
                }
            }
            return Ok(results);
        }
    }
    bail!("hosted validation graph exceeded its deterministic gate bound")
}
