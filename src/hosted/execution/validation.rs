// Extracted from the hosted execution composition root.
use super::*;

impl<'a> GatewayAgent<'a> {
    pub(in crate::hosted) fn checkpoint_validation_ledger(&mut self) -> Result<()> {
        self.checkpoint_notebook(false)?;
        self.api.append_event(
            "validation",
            json!({
                "event_type": "worker.validation_ledger_checkpoint",
                "validation_evidence": self.notebook.validation_evidence,
                "required_gates": self.notebook.required_gates,
                "notebook": self.notebook,
                "checkpoint": self.notebook_checkpoint_metadata(None),
            }),
        )
    }
}
pub(in crate::hosted) fn bootstrap_hosted_dependencies(
    api: &HostedApiClient,
    manifest: &HostedManifest,
    repo: &Repo,
    running: &Arc<AtomicBool>,
    containment: &command::HostedProcessContainment,
    existing: Option<&DependencyBootstrapEvidence>,
) -> Result<Option<DependencyBootstrapEvidence>> {
    let Some((manager, command_text)) = hosted_dependency_bootstrap(&repo.root) else {
        return Ok(None);
    };
    let lock_hash = dependency_lock_fingerprint(&repo.root)?;
    let repository_fingerprint = repository_state_fingerprint(repo, &manifest.github.base_sha)?;
    if existing.is_some_and(|evidence| {
        evidence.command == command_text
            && evidence.lock_hash == lock_hash
            && evidence.repository_fingerprint == repository_fingerprint
            && evidence.status == DependencyBootstrapStatus::Passed
    }) {
        api.append_event(
            "progress",
            json!({
                "step": "dependency_bootstrap",
                "status": "reused",
                "manager": manager,
                "command": command_text,
                "lock_hash": lock_hash,
                "repository_fingerprint": repository_fingerprint,
            }),
        )?;
        return Ok(existing.cloned());
    }
    api.append_event(
        "progress",
        json!({
            "step": "dependency_bootstrap",
            "status": "running",
            "manager": manager,
            "command": command_text
        }),
    )?;
    let allowlist = manifest.execution_policy.child_environment_allowlist();
    let output = command::capture_hosted_cancellable_with_environment(
        command_text,
        &repo.root,
        running,
        crate::execution_graph::ValidationTimeoutPolicy::dependency_install().absolute_timeout,
        2 * 1024 * 1024,
        Some(&allowlist),
        None,
        containment,
    )?;
    if !output.status.success() {
        bail!(
            "locked {manager} dependency bootstrap failed: {}",
            truncate_text(&format!("{}\n{}", output.stdout, output.stderr), 8_000)
        );
    }
    api.append_event(
        "progress",
        json!({
            "step": "dependency_bootstrap",
            "status": "completed",
            "manager": manager
        }),
    )?;
    Ok(Some(DependencyBootstrapEvidence {
        command: command_text.into(),
        lock_hash,
        repository_fingerprint,
        completed_at: now_rfc3339(),
        status: DependencyBootstrapStatus::Passed,
    }))
}

pub(in crate::hosted) fn hosted_dependency_bootstrap(
    root: &Path,
) -> Option<(&'static str, &'static str)> {
    if !root.join("package.json").is_file() {
        return None;
    }
    if root.join("pnpm-lock.yaml").is_file() {
        Some((
            "pnpm",
            "pnpm install --frozen-lockfile --prefer-offline --ignore-scripts",
        ))
    } else if root.join("yarn.lock").is_file() {
        Some((
            "yarn",
            "yarn install --frozen-lockfile --prefer-offline --ignore-scripts",
        ))
    } else if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
        Some(("bun", "bun install --frozen-lockfile --ignore-scripts"))
    } else if root.join("package-lock.json").is_file() || root.join("npm-shrinkwrap.json").is_file()
    {
        Some((
            "npm",
            "npm ci --ignore-scripts --no-audit --no-fund --prefer-offline",
        ))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::hosted) fn run_quality_gates(
    api: &HostedApiClient,
    manifest: &HostedManifest,
    repo: &Repo,
    running: &Arc<AtomicBool>,
    policy: &HostedExecutionPolicy,
    containment: &command::HostedProcessContainment,
    validation_round: u32,
    ledger: &mut Vec<ValidationEvidence>,
    required_gates: &mut Vec<RequiredGate>,
    usage: &mut ToolUsage,
    validation_started_at: Instant,
    validation_duration_limit: Duration,
    execution_started_at: Instant,
    execution_limit: Duration,
) -> Result<Vec<ValidationResult>> {
    run_quality_gates_with_capture(
        api,
        manifest,
        repo,
        running,
        policy,
        validation_round,
        ledger,
        required_gates,
        usage,
        validation_started_at,
        validation_duration_limit,
        execution_started_at,
        execution_limit,
        |command_text, cwd, running, timeout, max_output_bytes, environment_allowlist, limits| {
            let gate_type = policy
                .quality_gates
                .iter()
                .find(|gate| gate.command == command_text)
                .map_or(ValidationGateType::Custom, |gate| {
                    classify_validation_gate(&gate.id, &gate.command)
                });
            let timeout_policy =
                validation_timeout_policy(gate_type, command_text).clamped_to(timeout);
            let node_id = policy
                .quality_gates
                .iter()
                .find(|gate| gate.command == command_text)
                .map(|gate| gate.id.as_str())
                .unwrap_or("unknown-validation-node");
            let mut activity = |observation: command::CommandActivity| {
                record_validation_observability(
                    "validation process activity event",
                    api.append_event(
                        "validation",
                        json!({
                            "event_type": "worker.validation_process_activity",
                            "node_id": node_id,
                            "command": command_text,
                            "gate_type": gate_type,
                            "current_elapsed_ms": observation.elapsed.as_millis(),
                            "last_output_age_ms": observation.last_output_age.as_millis(),
                            "last_output_at_elapsed_ms": observation.elapsed.saturating_sub(observation.last_output_age).as_millis(),
                            "bytes_emitted": observation.bytes_emitted,
                            "configured_execution_timeout_ms": timeout_policy.execution_timeout.as_millis(),
                            "configured_inactivity_timeout_ms": timeout_policy.inactivity_timeout.map(|value| value.as_millis()),
                            "configured_absolute_timeout_ms": timeout_policy.absolute_timeout.as_millis(),
                        }),
                    ),
                );
            };
            command::capture_hosted_cancellable_observed_with_environment(
                command_text,
                cwd,
                running,
                timeout_policy.absolute_timeout,
                timeout_policy.inactivity_timeout,
                max_output_bytes,
                environment_allowlist,
                limits,
                containment,
                &mut activity,
            )
        },
    )
}

pub(in crate::hosted) fn record_validation_observability(operation: &str, result: Result<()>) {
    if let Err(error) = result {
        // Validation evidence is mission state; telemetry and human-readable
        // event delivery are observability. Once a command has run, an
        // observability outage must not discard its canonical result or cause
        // an unchanged-tree rerun.
        eprintln!("[warning] {operation} could not be persisted: {error:#}");
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::hosted) fn run_quality_gates_with_capture<F>(
    api: &HostedApiClient,
    manifest: &HostedManifest,
    repo: &Repo,
    running: &Arc<AtomicBool>,
    policy: &HostedExecutionPolicy,
    validation_round: u32,
    ledger: &mut Vec<ValidationEvidence>,
    required_gates: &mut Vec<RequiredGate>,
    usage: &mut ToolUsage,
    validation_started_at: Instant,
    validation_duration_limit: Duration,
    execution_started_at: Instant,
    execution_limit: Duration,
    capture: F,
) -> Result<Vec<ValidationResult>>
where
    F: Fn(
        &str,
        &Path,
        &AtomicBool,
        Duration,
        usize,
        Option<&[String]>,
        Option<command::ChildLimits>,
    ) -> Result<command::CommandOutput>,
{
    let allowlist = policy.child_environment_allowlist();
    let workflow_run_attempt = manifest
        .execution
        .github_actions
        .as_ref()
        .and_then(|execution| execution.workflow_run_attempt)
        .context("validated manifest has no GitHub workflow run attempt")?;
    let mut results = Vec::new();
    let mut ordered_gates = policy
        .quality_gates
        .iter()
        .filter(|gate| gate.required)
        .collect::<Vec<_>>();
    ordered_gates.sort_by_cached_key(|gate| validation_gate_order_key(&gate.id, &gate.command));
    for gate in ordered_gates {
        ensure_running(running)?;
        let scheduling_remaining = validation_duration_limit
            .checked_sub(validation_started_at.elapsed())
            .filter(|remaining| !remaining.is_zero());
        let execution_remaining = execution_limit
            .min(MAX_HOSTED_EXECUTION_DURATION)
            .checked_sub(execution_started_at.elapsed())
            .filter(|remaining| !remaining.is_zero());
        let Some(execution_remaining) = execution_remaining else {
            record_validation_observability(
                "validation wall-clock event",
                api.append_event(
                    "validation",
                    json!({
                        "event_type": "worker.wall_clock_guard_triggered",
                        "phase": "validation",
                        "gate_id": gate.id,
                        "validation_node_limit_seconds": validation_duration_limit.as_secs(),
                        "execution_limit_seconds": execution_limit
                            .min(MAX_HOSTED_EXECUTION_DURATION)
                            .as_secs(),
                        "status": "timed_out",
                    }),
                ),
            );
            results.push(ValidationResult {
                id: gate.id.clone(),
                command: gate.command.clone(),
                status: "infrastructure_failed".into(),
                output: "Validation node exhausted its graph-assigned duration budget.".into(),
            });
            break;
        };
        let source_tree_hash = repository_state_fingerprint(repo, &manifest.github.base_sha)?;
        supersede_stale_validation(ledger, &source_tree_hash);
        let dependency_lock_hash = dependency_lock_fingerprint(&repo.root)?;
        let environment_fingerprint = relevant_environment_fingerprint(policy)?;
        let fingerprint = validation_fingerprint(
            &gate.command,
            repo.root.to_string_lossy().as_ref(),
            &source_tree_hash,
            &dependency_lock_hash,
            &environment_fingerprint,
        );
        let gate_type = classify_validation_gate(&gate.id, &gate.command);
        if scheduling_remaining.is_none() {
            results.push(ValidationResult {
                id: gate.id.clone(),
                command: gate.command.clone(),
                status: "timed_out".into(),
                output: "Validation node scheduling deadline elapsed before the process started."
                    .into(),
            });
            break;
        }
        if let Some(evidence) = passed_evidence(ledger, &fingerprint).cloned() {
            usage.deduplicated_validations = usage.deduplicated_validations.saturating_add(1);
            record_validation_observability(
                "validation deduplication event",
                api.append_event(
                    "validation",
                    json!({
                        "event_type": "worker.validation_deduplicated",
                        "gate_id": gate.id,
                        "evidence_id": evidence.evidence_id,
                        "command_fingerprint": fingerprint,
                        "source_tree_hash": source_tree_hash,
                        "status": "passed",
                        "source": ValidationSource::ResumeReused,
                    }),
                ),
            );
            required_gates.retain(|required| required.gate_id != gate.id);
            required_gates.push(RequiredGate {
                gate_id: gate.id.clone(),
                gate_type,
                required: true,
                command: gate.command.clone(),
                status: ValidationStatus::Passed,
                evidence_id: Some(evidence.evidence_id.clone()),
            });
            results.push(ValidationResult {
                id: gate.id.clone(),
                command: gate.command.clone(),
                status: "passed".into(),
                output: format!(
                    "Reused validation evidence {} for unchanged source tree {}.",
                    evidence.evidence_id, source_tree_hash
                ),
            });
            continue;
        }
        if let Some(evidence) = ledger
            .iter()
            .rev()
            .find(|evidence| {
                evidence.command_fingerprint == fingerprint
                    && evidence.status == ValidationStatus::FailedCode
            })
            .cloned()
        {
            usage.deduplicated_validations = usage.deduplicated_validations.saturating_add(1);
            record_validation_observability(
                "failed validation deduplication event",
                api.append_event(
                "validation",
                json!({
                    "event_type": "worker.validation_deduplicated",
                    "gate_id": gate.id,
                    "evidence_id": evidence.evidence_id,
                    "command_fingerprint": fingerprint,
                    "source_tree_hash": source_tree_hash,
                    "status": evidence.status,
                    "reason": "failed_gate_requires_a_relevant_source_or_dependency_change_before_retry",
                }),
                ),
            );
            required_gates.retain(|required| required.gate_id != gate.id);
            required_gates.push(RequiredGate {
                gate_id: gate.id.clone(),
                gate_type,
                required: true,
                command: gate.command.clone(),
                status: evidence.status,
                evidence_id: Some(evidence.evidence_id.clone()),
            });
            results.push(ValidationResult {
                id: gate.id.clone(),
                command: gate.command.clone(),
                status: match evidence.status {
                    ValidationStatus::TimedOut => "timed_out".into(),
                    ValidationStatus::FailedInfrastructure => "infrastructure_failed".into(),
                    ValidationStatus::Cancelled => "cancelled".into(),
                    _ => "failed".into(),
                },
                output: format!(
                    "Gate was not rerun because source and dependency state are unchanged; see evidence {}.",
                    evidence.evidence_id
                ),
            });
            if gate_type == ValidationGateType::FocusedTest {
                break;
            }
            continue;
        }
        usage.required_validations = usage.required_validations.saturating_add(1);
        usage.validation_commands = usage.validation_commands.saturating_add(1);
        let phase_started_at = now_rfc3339();
        record_validation_observability(
            "quality-gate start telemetry",
            send_quality_gate_phase_telemetry(
                api,
                manifest.execution.execution_id,
                gate,
                workflow_run_attempt,
                validation_round,
                &phase_started_at,
                None,
                ExecutionStatus::Running,
                1,
            ),
        );
        record_validation_observability(
            "quality-gate running event",
            api.append_event(
                "validation",
                json!({
                    "event_type": "worker.validation_process_started",
                    "node_id": gate.id,
                    "gate_id": gate.id,
                    "command": gate.command,
                    "gate_type": gate_type,
                    "status": "running",
                    "process_started_at": phase_started_at,
                    "repository_fingerprint": source_tree_hash,
                    "configured_timeouts": validation_timeout_policy(gate_type, &gate.command),
                    "retry_count": 0,
                }),
            ),
        );
        let started = Instant::now();
        let evidence_attempt = ledger
            .iter()
            .filter(|evidence| evidence.command_fingerprint == fingerprint)
            .count()
            .saturating_add(1);
        let evidence_id = format!("{}-{}-a{evidence_attempt}", gate.id, &fingerprint[..12]);
        ledger.push(new_running_evidence(
            evidence_id.clone(),
            gate.id.clone(),
            gate_type,
            gate.command.clone(),
            fingerprint.clone(),
            source_tree_hash.clone(),
            dependency_lock_hash,
            ValidationSource::WorkerRequired,
        ));
        let timeout_policy =
            validation_timeout_policy(gate_type, &gate.command).clamped_to(execution_remaining);
        let mut retry_count = 0_u32;
        let output = loop {
            let output = capture(
                &gate.command,
                &repo.root,
                running,
                timeout_policy.absolute_timeout,
                2 * 1024 * 1024,
                Some(&allowlist),
                None,
            );
            if output.is_ok() || retry_count > 0 || !running.load(Ordering::SeqCst) {
                break output;
            }
            let remaining_after_failure = execution_limit
                .min(MAX_HOSTED_EXECUTION_DURATION)
                .saturating_sub(execution_started_at.elapsed());
            if remaining_after_failure < timeout_policy.absolute_timeout {
                break output;
            }
            retry_count = 1;
            record_validation_observability(
                "validation retry event",
                api.append_event(
                    "validation",
                    json!({
                        "event_type": "worker.validation_retry_scheduled",
                        "node_id": gate.id,
                        "gate_id": gate.id,
                        "command": gate.command,
                        "gate_type": gate_type,
                        "repository_fingerprint": source_tree_hash,
                        "retry_count": retry_count,
                        "model_call_required": false,
                        "configured_timeouts": timeout_policy,
                    }),
                ),
            );
        };
        let (result, evidence_status, exit_code, stdout, stderr) = match output {
            Ok(output) => {
                let combined = format!("{}\n{}", output.stdout, output.stderr);
                let passed = output.status.success();
                (
                    ValidationResult {
                        id: gate.id.clone(),
                        command: gate.command.clone(),
                        status: if passed {
                            "passed".into()
                        } else {
                            "failed".into()
                        },
                        output: truncate_text(&combined, 16_000),
                    },
                    if passed {
                        ValidationStatus::Passed
                    } else {
                        ValidationStatus::FailedCode
                    },
                    output.status.code(),
                    output.stdout,
                    output.stderr,
                )
            }
            Err(error) => {
                let raw_message = truncate_text(&format!("{error:#}"), 12_000);
                let cancelled = !running.load(Ordering::SeqCst) || shutdown::requested();
                let timed_out = command::is_timeout(&error);
                let message = truncate_text(
                    &format!(
                        "{raw_message}\ncode={}\ngate_id={}\ngate_type={gate_type:?}\nconfigured_execution_timeout_ms={}\nconfigured_inactivity_timeout_ms={:?}\nconfigured_absolute_timeout_ms={}\nelapsed_ms={}\nrepository_fingerprint={}\nretry_count={}\nretry_eligible=false",
                        if timed_out {
                            "validation_process_timeout"
                        } else {
                            "validation_process_infrastructure_failure"
                        },
                        gate.id,
                        timeout_policy.execution_timeout.as_millis(),
                        timeout_policy
                            .inactivity_timeout
                            .map(|value| value.as_millis()),
                        timeout_policy.absolute_timeout.as_millis(),
                        started.elapsed().as_millis(),
                        source_tree_hash,
                        retry_count,
                    ),
                    16_000,
                );
                (
                    ValidationResult {
                        id: gate.id.clone(),
                        command: gate.command.clone(),
                        status: if cancelled {
                            "cancelled".into()
                        } else if timed_out {
                            "timed_out".into()
                        } else {
                            "infrastructure_failed".into()
                        },
                        output: message.clone(),
                    },
                    if cancelled {
                        ValidationStatus::Cancelled
                    } else if timed_out {
                        ValidationStatus::TimedOut
                    } else {
                        ValidationStatus::FailedInfrastructure
                    },
                    None,
                    String::new(),
                    message,
                )
            }
        };
        if let Some(evidence) = ledger
            .iter_mut()
            .rev()
            .find(|evidence| evidence.evidence_id == evidence_id)
        {
            evidence.completed_at = Some(now_rfc3339());
            evidence.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            evidence.exit_code = exit_code;
            evidence.status = evidence_status;
            evidence.stdout_summary = truncate_text(&stdout, 8_000);
            evidence.stderr_summary = truncate_text(&stderr, 8_000);
        }
        required_gates.retain(|required| required.gate_id != gate.id);
        required_gates.push(RequiredGate {
            gate_id: gate.id.clone(),
            gate_type,
            required: true,
            command: gate.command.clone(),
            status: evidence_status,
            evidence_id: Some(evidence_id.clone()),
        });
        let phase_completed_at = now_rfc3339();
        record_validation_observability(
            "quality-gate completion telemetry",
            send_quality_gate_phase_telemetry(
                api,
                manifest.execution.execution_id,
                gate,
                workflow_run_attempt,
                validation_round,
                &phase_started_at,
                Some(&phase_completed_at),
                if result.status == "passed" {
                    ExecutionStatus::Succeeded
                } else {
                    ExecutionStatus::Failed
                },
                2,
            ),
        );
        record_validation_observability(
            "quality-gate completion event",
            api.append_event(
                "validation",
                json!({
                    "gate_id": result.id,
                    "command": result.command,
                    "status": result.status,
                    "output": result.output,
                    "execution_id": manifest.execution.execution_id
                    ,"event_type": if result.status == "timed_out" {
                        "worker.validation_process_timed_out"
                    } else {
                        "worker.validation_process_completed"
                    }
                    ,"evidence_id": evidence_id
                    ,"node_id": gate.id
                    ,"command_fingerprint": fingerprint
                    ,"source_tree_hash": source_tree_hash
                    ,"gate_type": gate_type
                    ,"elapsed_ms": started.elapsed().as_millis()
                    ,"process_started_at": phase_started_at
                    ,"process_completed_at": phase_completed_at
                    ,"exit_code": exit_code
                    ,"retry_count": retry_count
                    ,"configured_timeouts": timeout_policy
                }),
            ),
        );
        let focused_gate_failed =
            gate_type == ValidationGateType::FocusedTest && result.status != "passed";
        results.push(result);
        if focused_gate_failed {
            break;
        }
        if !running.load(Ordering::SeqCst) || shutdown::requested() {
            break;
        }
    }
    Ok(results)
}

pub(in crate::hosted) fn classify_validation_gate(id: &str, command: &str) -> ValidationGateType {
    let id = id.to_ascii_lowercase();
    let command = command.to_ascii_lowercase();
    let value = format!("{id} {command}");
    if id.contains("focused") || command.contains("test --") {
        ValidationGateType::FocusedTest
    } else if value.contains("build") {
        ValidationGateType::Build
    } else if value.contains("lint") || value.contains("fmt") {
        ValidationGateType::Lint
    } else if value.contains("typecheck") || value.contains("tsc") || value.contains("check") {
        ValidationGateType::Typecheck
    } else if value.contains("test") {
        ValidationGateType::TestSuite
    } else {
        ValidationGateType::Custom
    }
}

pub(in crate::hosted) fn validation_timeout_policy(
    gate_type: ValidationGateType,
    command: &str,
) -> crate::execution_graph::ValidationTimeoutPolicy {
    if is_dependency_install_command(command) {
        crate::execution_graph::ValidationTimeoutPolicy::dependency_install()
    } else {
        let graph_type = match gate_type {
            ValidationGateType::FocusedTest => {
                crate::execution_graph::ValidationGateType::FocusedTest
            }
            ValidationGateType::TestSuite => crate::execution_graph::ValidationGateType::TestSuite,
            ValidationGateType::Build => crate::execution_graph::ValidationGateType::Build,
            ValidationGateType::Lint => crate::execution_graph::ValidationGateType::Lint,
            ValidationGateType::Typecheck => crate::execution_graph::ValidationGateType::Typecheck,
            ValidationGateType::Custom => crate::execution_graph::ValidationGateType::Custom,
        };
        crate::execution_graph::ValidationTimeoutPolicy::for_gate(graph_type)
    }
}

pub(in crate::hosted) fn is_dependency_install_command(command: &str) -> bool {
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
    .any(|prefix| normalized.starts_with(prefix))
}

pub(in crate::hosted) fn validation_gate_order_key(
    id: &str,
    command: &str,
) -> (u8, String, String) {
    let gate_type = classify_validation_gate(id, command);
    let normalized = command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let browser_e2e = ["playwright", "cypress", "browser", " e2e"]
        .iter()
        .any(|needle| normalized.contains(needle));
    let class = if is_dependency_install_command(command) {
        0
    } else if gate_type == ValidationGateType::FocusedTest {
        1
    } else if matches!(
        gate_type,
        ValidationGateType::Lint | ValidationGateType::Typecheck
    ) {
        2
    } else if gate_type == ValidationGateType::TestSuite && !browser_e2e {
        3
    } else if gate_type == ValidationGateType::Build {
        4
    } else if browser_e2e {
        5
    } else {
        6
    };
    (class, id.to_owned(), normalized)
}

pub(in crate::hosted) fn dependency_lock_fingerprint(root: &Path) -> Result<String> {
    let mut material = Vec::new();
    for name in [
        "Cargo.lock",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lockb",
    ] {
        let path = root.join(name);
        if path.is_file() {
            material.extend_from_slice(name.as_bytes());
            material.push(0);
            material.extend_from_slice(&fs::read(path)?);
            material.push(0);
        }
    }
    Ok(hex::encode(Sha256::digest(material)))
}

pub(in crate::hosted) fn relevant_environment_fingerprint(
    policy: &HostedExecutionPolicy,
) -> Result<String> {
    let mut names = policy.child_environment_allowlist();
    names.sort();
    names.dedup();
    let mut material = serde_json::to_vec(&policy.sandbox)
        .context("could not fingerprint hosted sandbox policy")?;
    for name in names {
        append_fingerprint_field(&mut material, "environment_name", name.as_bytes());
        append_fingerprint_field(
            &mut material,
            "environment_value",
            env::var(&name).unwrap_or_default().as_bytes(),
        );
    }
    Ok(hex::encode(Sha256::digest(material)))
}
