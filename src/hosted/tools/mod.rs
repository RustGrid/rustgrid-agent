mod filesystem;
mod mutation;
mod search;

pub(super) use filesystem::*;
pub(super) use mutation::*;
pub(super) use search::*;

// Extracted from the hosted execution composition root.
use super::*;

impl<'a> GatewayAgent<'a> {
    fn active_target_operation(
        &self,
    ) -> Option<(crate::execution_graph::TargetOperation, Option<String>)> {
        let context = match self.current_decision.as_ref()? {
            ExecutionDecision::ExecuteTarget { target, .. } => target,
            ExecutionDecision::RepairTarget { context, .. } => &context.target,
            _ => return None,
        };
        Some((
            context.target.effective_operation(),
            context.source_content_hash.clone(),
        ))
    }
    fn active_mutation_context(&self) -> Option<(String, Option<String>, String)> {
        let context = match self.current_decision.as_ref()? {
            ExecutionDecision::ExecuteTarget { target, .. } => target,
            ExecutionDecision::RepairTarget { context, .. } => &context.target,
            _ => return None,
        };
        Some((
            context.target.path.clone(),
            context.target_content_hash.clone(),
            context.repository_fingerprint.clone(),
        ))
    }

    fn verify_active_mutation_fingerprint(&self) -> Result<Option<String>> {
        let (_, expected_hash, expected_repository_fingerprint) = self
            .active_mutation_context()
            .context("mutation requires an active deterministic target context")?;
        let current_repository_fingerprint =
            repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
        if current_repository_fingerprint != expected_repository_fingerprint {
            return Err(anyhow!(MutationApplicationError {
                failure: MutationApplicationFailure::RepositoryChangedSinceContext,
                message: "repository changed after deterministic target context preparation".into(),
                patch_validation: None,
                git_apply_check: None,
                raw_patch_sha256: None,
                target_content_hash: expected_hash,
            }));
        }
        Ok(expected_hash)
    }

    fn active_mutation_attempts(
        &self,
    ) -> (Option<crate::execution_graph::ExecutionNodeId>, usize, u32) {
        let node_id = self
            .current_decision
            .as_ref()
            .and_then(ExecutionDecision::node_id)
            .cloned();
        let mutation_attempt = node_id.as_ref().map_or(0, |node_id| {
            self.notebook
                .orchestration
                .graph
                .as_ref()
                .and_then(|graph| graph.node(node_id))
                .map_or(0, |node| node.attempts.len())
        });
        let repair_attempt = node_id.as_ref().map_or(0, |node_id| {
            self.notebook
                .orchestration
                .budget
                .usage_for(node_id)
                .repair_attempts
        });
        (node_id, mutation_attempt, repair_attempt)
    }

    pub(in crate::hosted) fn execute_tool(
        &mut self,
        name: &str,
        raw_arguments: &str,
    ) -> Result<String> {
        let arguments: Value =
            serde_json::from_str(raw_arguments).context("tool arguments are not valid JSON")?;
        let object = arguments
            .as_object()
            .context("tool arguments must be an object")?;
        self.validate_tool_for_phase(name, object)?;
        if is_source_mutation_tool(name) {
            self.preflight_source_mutation(name, object)?;
        }
        if name != "search_text" {
            self.search_guard.record_non_search();
        }
        match name {
            "list_files" => {
                self.tool_usage.reads = self.tool_usage.reads.saturating_add(1);
                let path = object.get("path").and_then(Value::as_str).unwrap_or(".");
                let root = safe_repo_path(&self.repo.root, path, false)?;
                let files = collect_repo_files(&self.repo.root, &root, 1_000)?;
                push_unique(
                    &mut self.notebook.architecture_findings,
                    format!("Repository tree inspected under {path}."),
                );
                Ok(files.join("\n"))
            }
            "read_file" => {
                self.tool_usage.reads = self.tool_usage.reads.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let start_line = object
                    .get("start_line")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1);
                let end_line = object
                    .get("end_line")
                    .and_then(Value::as_u64)
                    .unwrap_or(start_line.saturating_add(399))
                    .min(start_line.saturating_add(999));
                let requested_range = crate::execution_graph::LineRange::new(
                    u32::try_from(start_line).unwrap_or(u32::MAX),
                    u32::try_from(end_line).unwrap_or(u32::MAX),
                );
                let fingerprint =
                    repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
                if let Some(cached) = self
                    .notebook
                    .orchestration
                    .evidence
                    .reusable_file(path, &fingerprint, requested_range)
                    .cloned()
                {
                    push_unique(
                        &mut self.notebook.read_ranges_inspected,
                        format!("{path}:{start_line}-{end_line}"),
                    );
                    push_unique(&mut self.notebook.files_inspected, path.to_owned());
                    return Ok(serde_json::to_string(&FileReadResult {
                        path: path.to_owned(),
                        status: FileReadStatus::Success,
                        content: Some(cached.captured_content.clone()),
                        error_code: None,
                        error_message: None,
                        line_count: Some(cached.captured_content.lines().count() as u32),
                        file_size: Some(cached.captured_content.len() as u64),
                        valid_line_range: cached
                            .line_range
                            .map(|range| format!("{}-{}", range.start, range.end)),
                        truncated: cached.truncated,
                        fallback_attempted: false,
                    })?);
                }
                let result = read_repo_file_result(
                    &self.repo.root,
                    path,
                    start_line,
                    end_line,
                    MAX_TOOL_OUTPUT_BYTES,
                );
                if result.status == FileReadStatus::Error {
                    bail!(
                        "{}: {}",
                        result.error_code.as_deref().unwrap_or("read_failed"),
                        result.error_message.as_deref().unwrap_or("read failed")
                    );
                }
                push_unique(
                    &mut self.notebook.read_ranges_inspected,
                    format!("{path}:{start_line}-{end_line}"),
                );
                if let Some(reason) = object.get("reason").and_then(Value::as_str) {
                    record_centralized_discovery_finding(&mut self.notebook, reason);
                }
                push_unique(&mut self.notebook.files_inspected, path.to_owned());
                if self.phases.active() == ExecutionPhase::Repair {
                    self.repair_read_targets.insert(path.to_owned());
                }
                if let Some(content) = result.content.as_ref() {
                    let line_range = requested_range;
                    let already_cached = self
                        .notebook
                        .orchestration
                        .evidence
                        .reusable_file(path, &fingerprint, line_range)
                        .is_some();
                    let evidence = crate::execution_graph::FileEvidence::capture(
                        path,
                        &fingerprint,
                        line_range,
                        content,
                        result.truncated,
                    );
                    if !already_cached {
                        let evidence_id = evidence.evidence_id.clone();
                        let sequence = self
                            .notebook
                            .orchestration
                            .domain_events
                            .last()
                            .map_or(1, |event| event.sequence().saturating_add(1));
                        self.append_execution_domain_event(
                            crate::execution_graph::ExecutionDomainEvent::RepositoryEvidenceRecorded {
                                sequence,
                                evidence_id,
                                repository_fingerprint: fingerprint,
                                evidence: Some(evidence),
                            },
                        )?;
                    }
                }
                Ok(serde_json::to_string(&result)?)
            }
            "read_files" => {
                self.tool_usage.reads = self.tool_usage.reads.saturating_add(1);
                let paths = object
                    .get("paths")
                    .and_then(Value::as_array)
                    .context("tool argument `paths` is missing")?;
                if paths.is_empty() || paths.len() > 20 {
                    bail!("read_files requires between 1 and 20 paths");
                }
                let maximum_lines = object
                    .get("maximum_lines_per_file")
                    .and_then(Value::as_u64)
                    .unwrap_or(800)
                    .clamp(1, 1_000);
                let inspected_before_batch = self
                    .notebook
                    .files_inspected
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                // Admission is a distinct first pass: every requested path receives structured
                // malformed, unsafe, missing, or ready metadata before any valid file is read.
                let prevalidated = prevalidate_batch_read_paths(&self.repo.root, paths);
                let fingerprint =
                    repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
                let (batch, batch_failures) = read_prevalidated_repo_files_with_evidence_cache(
                    &self.repo.root,
                    &prevalidated,
                    maximum_lines,
                    MAX_TOOL_OUTPUT_BYTES,
                    &self.notebook.orchestration.evidence,
                    &fingerprint,
                );
                let files = batch.files;
                for result in &files {
                    if result.status == FileReadStatus::Success {
                        push_unique(&mut self.notebook.files_inspected, result.path.clone());
                        if self.phases.active() == ExecutionPhase::Repair {
                            self.repair_read_targets.insert(result.path.clone());
                        }
                    }
                }
                for result in files
                    .iter()
                    .filter(|result| result.status == FileReadStatus::Success)
                {
                    let Some(content) = result.content.as_ref() else {
                        continue;
                    };
                    let line_range = result.line_count.and_then(|line_count| {
                        crate::execution_graph::LineRange::new(1, line_count.max(1))
                    });
                    let already_cached = self
                        .notebook
                        .orchestration
                        .evidence
                        .reusable_file(&result.path, &fingerprint, line_range)
                        .is_some();
                    let evidence = crate::execution_graph::FileEvidence::capture(
                        &result.path,
                        &fingerprint,
                        line_range,
                        content,
                        result.truncated,
                    );
                    if !already_cached {
                        let evidence_id = evidence.evidence_id.clone();
                        let sequence = self
                            .notebook
                            .orchestration
                            .domain_events
                            .last()
                            .map_or(1, |event| event.sequence().saturating_add(1));
                        self.append_execution_domain_event(
                            crate::execution_graph::ExecutionDomainEvent::RepositoryEvidenceRecorded {
                                sequence,
                                evidence_id,
                                repository_fingerprint: fingerprint.clone(),
                                evidence: Some(evidence),
                            },
                        )?;
                    }
                }
                self.tool_usage.failed_reads =
                    self.tool_usage.failed_reads.saturating_add(batch_failures);
                if files
                    .iter()
                    .any(|result| result.status == FileReadStatus::Success)
                    && let Some(reason) = object.get("reason").and_then(Value::as_str)
                {
                    record_centralized_discovery_finding(&mut self.notebook, reason);
                }
                for failed in files
                    .iter()
                    .filter(|result| result.status == FileReadStatus::Error)
                {
                    self.record_tool_progress(
                        "read_file",
                        Some(failed.path.clone()),
                        read_error_progress_class(
                            failed.error_code.as_deref().unwrap_or("read_failed"),
                        ),
                        format!(
                            "{}: {}; valid_line_range={}; individual_fallback_attempted={}",
                            failed.error_code.as_deref().unwrap_or("read_failed"),
                            failed.error_message.as_deref().unwrap_or("read failed"),
                            failed.valid_line_range.as_deref().unwrap_or("unavailable"),
                            failed.fallback_attempted,
                        ),
                        false,
                    );
                }
                for succeeded in files
                    .iter()
                    .filter(|result| result.status == FileReadStatus::Success)
                {
                    let new_evidence = !inspected_before_batch.contains(succeeded.path.as_str());
                    self.record_tool_progress(
                        "read_file",
                        Some(succeeded.path.clone()),
                        if new_evidence {
                            ToolProgressClass::Productive
                        } else {
                            ToolProgressClass::Duplicate
                        },
                        if !new_evidence {
                            "batch read repeated previously inspected repository content"
                        } else if succeeded.fallback_attempted {
                            "individual fallback recovered the batch read"
                        } else {
                            "batch read returned repository content"
                        },
                        false,
                    );
                }
                let fallback_results = files
                    .iter()
                    .filter(|result| result.fallback_attempted)
                    .map(|result| {
                        json!({
                            "path": result.path,
                            "status": result.status,
                            "error_code": result.error_code,
                            "error_message": result.error_message,
                            "valid_line_range": result.valid_line_range,
                        })
                    })
                    .collect::<Vec<_>>();
                if !fallback_results.is_empty() {
                    self.append_event_recoverable(
                        "progress",
                        json!({
                            "event_type": "worker.batch_read_fallback",
                            "fallback": "individual_read_once",
                            "results": fallback_results,
                            "model_call_consumed": false,
                        }),
                        "batch-read individual fallback",
                    );
                }
                Ok(serde_json::to_string(&BatchReadResult { files })?)
            }
            "search_text" => {
                self.tool_usage.searches = self.tool_usage.searches.saturating_add(1);
                let query = required_tool_string(object, "query", 200)?;
                let path = object.get("path").and_then(Value::as_str).unwrap_or(".");
                let extensions = object
                    .get("extensions")
                    .and_then(Value::as_array)
                    .context("tool argument `extensions` is missing")?
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .filter(|extension| extension.len() <= 20)
                            .map(str::to_owned)
                            .context("search extension is malformed")
                    })
                    .collect::<Result<Vec<_>>>()?;
                let context_lines = object
                    .get("context_lines")
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
                    .min(5);
                let mode = object
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("literal");
                let signature = SearchSignature::new(query, path, &extensions, mode, context_lines);
                if let Err(error) = self.search_guard.validate(&signature) {
                    self.emit_guardrail(
                        "search_loop_detected",
                        if self.phases.active() == ExecutionPhase::Discovery {
                            "force_planning"
                        } else {
                            "reject_search"
                        },
                        &error.to_string(),
                    )?;
                    return Err(error);
                }
                let discovery_coverage = localized_discovery_coverage(&self.notebook);
                let known_consumers = self
                    .notebook
                    .files_inspected
                    .iter()
                    .chain(self.notebook.discovery_paths_sampled.iter())
                    .filter(|path| !localized_discovery_core_path(path))
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let maximum_new_consumers = (self.phases.active() == ExecutionPhase::Discovery
                    && localized_visual_goal(&self.notebook.goal)
                    && discovery_coverage.centralized_abstraction)
                    .then(|| 3_usize.saturating_sub(discovery_coverage.representative_consumers));
                let result = search_repo(
                    &self.repo.root,
                    path,
                    query,
                    &extensions,
                    context_lines,
                    maximum_new_consumers,
                    &known_consumers,
                )?;
                self.search_guard.record(signature, result.truncated);
                if let Some(reason) = object.get("reason").and_then(Value::as_str) {
                    record_centralized_discovery_finding(&mut self.notebook, reason);
                }
                push_unique(
                    &mut self.notebook.searches_completed,
                    format!("{mode}:{path}:{query}"),
                );
                for matched_path in &result.matched_paths {
                    if !localized_discovery_core_path(matched_path) {
                        push_unique(
                            &mut self.notebook.discovery_paths_sampled,
                            matched_path.clone(),
                        );
                    }
                }
                if self.tool_usage.searches == 4 && self.impact_map.is_none() {
                    self.emit_guardrail(
                        "discovery_search_warning",
                        "narrow_impact_map",
                        "Four searches have run without a completed impact map.",
                    )?;
                }
                Ok(result.output)
            }
            "related_tests" => {
                self.tool_usage.reads = self.tool_usage.reads.saturating_add(1);
                let paths = object
                    .get("paths")
                    .and_then(Value::as_array)
                    .context("tool argument `paths` is missing")?
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                if paths.is_empty() || paths.len() > 20 {
                    bail!("related_tests requires between 1 and 20 source paths");
                }
                let stems = paths
                    .iter()
                    .filter_map(|path| Path::new(path).file_stem())
                    .filter_map(|stem| stem.to_str())
                    .map(str::to_ascii_lowercase)
                    .collect::<BTreeSet<_>>();
                let related = collect_repo_files(&self.repo.root, &self.repo.root, 2_000)?
                    .into_iter()
                    .filter(|candidate| {
                        let lower = candidate.to_ascii_lowercase();
                        (lower.contains("test") || lower.contains("spec"))
                            && stems.iter().any(|stem| lower.contains(stem))
                    })
                    .take(100)
                    .collect::<Vec<_>>();
                if let Some(reason) = object.get("reason").and_then(Value::as_str) {
                    record_centralized_discovery_finding(&mut self.notebook, reason);
                }
                for path in &related {
                    push_unique(
                        &mut self.notebook.searches_completed,
                        format!("related_test:{path}"),
                    );
                }
                Ok(if related.is_empty() {
                    "no related test files found".into()
                } else {
                    format!("related_test_paths:\n{}", related.join("\n"))
                })
            }
            "write_file" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let content = required_tool_string(object, "content", MAX_MODEL_FILE_BYTES)?;
                write_repo_file(&self.repo.root, path, content, false)
            }
            "replace_text" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let old_text = required_tool_string(object, "old_text", MAX_MODEL_FILE_BYTES)?;
                let new_text = object
                    .get("new_text")
                    .and_then(Value::as_str)
                    .filter(|value| value.len() <= MAX_MODEL_FILE_BYTES)
                    .context("tool argument `new_text` is missing or too large")?;
                apply_structured_edit(
                    &self.repo.root,
                    &StructuredEdit::ReplaceExactText {
                        path: path.to_owned(),
                        expected: old_text.to_owned(),
                        replacement: new_text.to_owned(),
                        expected_occurrences: 1,
                    },
                    None,
                )
            }
            "replace_range" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let start_line = object
                    .get("start_line")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .context("replace_range start_line is missing or invalid")?;
                let end_line = object
                    .get("end_line")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .context("replace_range end_line is missing or invalid")?;
                let new_text = required_tool_string(object, "new_text", MAX_MODEL_FILE_BYTES)?;
                replace_repo_range(&self.repo.root, path, start_line, end_line, new_text)
            }
            "insert_after_symbol" | "insert_before_symbol" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let symbol = required_tool_string(object, "symbol", MAX_MODEL_FILE_BYTES)?;
                let content = required_tool_string(object, "content", MAX_MODEL_FILE_BYTES)?;
                let edit = if name == "insert_after_symbol" {
                    StructuredEdit::InsertAfterExactText {
                        path: path.to_owned(),
                        anchor: symbol.to_owned(),
                        content: content.to_owned(),
                    }
                } else {
                    StructuredEdit::InsertBeforeExactText {
                        path: path.to_owned(),
                        anchor: symbol.to_owned(),
                        content: content.to_owned(),
                    }
                };
                apply_structured_edit(&self.repo.root, &edit, None)
            }
            "apply_patch" | "apply_unified_diff" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let patch = required_tool_string(object, "patch", MAX_MODEL_FILE_BYTES)?;
                let (_, expected_hash, expected_repository_fingerprint) = self
                    .active_mutation_context()
                    .context("patch mutation requires an active deterministic target context")?;
                let current_repository_fingerprint =
                    repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
                if current_repository_fingerprint != expected_repository_fingerprint {
                    return Err(anyhow!(MutationApplicationError {
                        failure: MutationApplicationFailure::RepositoryChangedSinceContext,
                        message:
                            "repository changed after deterministic target context preparation"
                                .into(),
                        patch_validation: None,
                        git_apply_check: None,
                        raw_patch_sha256: Some(sha256_text(patch)),
                        target_content_hash: expected_hash.clone(),
                    }));
                }
                let diagnostics = patch_target_diagnostics(&self.repo.root, path, patch)?;
                let (node_id, mutation_attempt, repair_attempt) = self.active_mutation_attempts();
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.mutation_payload_validated",
                        "node_id": node_id,
                        "target_path": path,
                        "mutation_tool": name,
                        "target_content_hash": expected_hash,
                        "repository_fingerprint": current_repository_fingerprint,
                        "normalized_patch_paths": diagnostics.normalized_paths,
                        "mutation_attempt": mutation_attempt,
                        "repair_attempt": repair_attempt,
                    }),
                    "mutation payload validation",
                );
                apply_repo_unified_diff_with_context(
                    &self.repo.root,
                    path,
                    patch,
                    expected_hash.as_deref(),
                )
            }
            "replace_file" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let content = required_tool_string(object, "content", MAX_MODEL_FILE_BYTES)?;
                let (_, expected_hash, expected_repository_fingerprint) =
                    self.active_mutation_context().context(
                        "full-file replacement requires an active deterministic target context",
                    )?;
                let current_repository_fingerprint =
                    repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
                if current_repository_fingerprint != expected_repository_fingerprint {
                    return Err(anyhow!(MutationApplicationError {
                        failure: MutationApplicationFailure::RepositoryChangedSinceContext,
                        message:
                            "repository changed after deterministic target context preparation"
                                .into(),
                        patch_validation: None,
                        git_apply_check: None,
                        raw_patch_sha256: None,
                        target_content_hash: expected_hash.clone(),
                    }));
                }
                let output = replace_repo_file_atomically(
                    &self.repo.root,
                    path,
                    content,
                    expected_hash.as_deref(),
                )?;
                let (node_id, mutation_attempt, repair_attempt) = self.active_mutation_attempts();
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.mutation_replacement_applied",
                        "node_id": node_id,
                        "target_path": path,
                        "mutation_tool": name,
                        "target_content_hash": expected_hash,
                        "repository_fingerprint": current_repository_fingerprint,
                        "fallback_strategy": "replace_file",
                        "normalized_patch_paths": [],
                        "failure_category": Value::Null,
                        "mutation_attempt": mutation_attempt,
                        "repair_attempt": repair_attempt,
                    }),
                    "mutation replacement application",
                );
                Ok(output)
            }
            "record_no_valid_repair" => {
                let diagnosis = match required_tool_string(object, "diagnosis", 64)? {
                    "source_defect" => {
                        crate::execution_graph::ValidationRepairDiagnosis::SourceDefect
                    }
                    "test_expectation_defect" => {
                        crate::execution_graph::ValidationRepairDiagnosis::TestExpectationDefect
                    }
                    "both" => crate::execution_graph::ValidationRepairDiagnosis::Both,
                    "inconclusive" => {
                        crate::execution_graph::ValidationRepairDiagnosis::Inconclusive
                    }
                    _ => bail!("validation repair diagnosis is unsupported"),
                };
                let reason = required_tool_string(object, "reason", 8_000)?;
                self.record_validation_no_valid_repair(diagnosis, reason)?;
                Ok("recorded typed no-valid-repair result".into())
            }
            "rewrite_small_file" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let content =
                    required_tool_string(object, "content", MAX_SMALL_FILE_REWRITE_BYTES)?;
                write_repo_file(&self.repo.root, path, content, true)
            }
            "delete_file" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let expected_hash = self.verify_active_mutation_fingerprint()?;
                delete_repo_file(&self.repo.root, path, expected_hash.as_deref())
            }
            "create_file" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let content = required_tool_string(object, "content", MAX_MODEL_FILE_BYTES)?;
                let create_parents = object
                    .get("create_parents")
                    .and_then(Value::as_bool)
                    .context("tool argument `create_parents` is missing")?;
                self.verify_active_mutation_fingerprint()?;
                self.record_active_target_mutation_intent(Some(sha256_text(content)))?;
                let creation_context = self.current_decision.as_ref().and_then(|decision| {
                    let (node_id, target) = match decision {
                        ExecutionDecision::ExecuteTarget {
                            node_id, target, ..
                        } => (node_id, &target.target),
                        _ => return None,
                    };
                    let operation = target.effective_operation();
                    Some((
                        node_id.clone(),
                        operation.as_str().to_owned(),
                        operation.source_path().map(str::to_owned),
                        self.notebook.repository_fingerprint.clone(),
                    ))
                });
                let (node_id, operation, source_path, repository_fingerprint) = creation_context
                    .unwrap_or_else(|| {
                        (
                            crate::execution_graph::ExecutionNodeId::default(),
                            "create_new".to_owned(),
                            None,
                            self.notebook.repository_fingerprint.clone(),
                        )
                    });
                self.append_event_recoverable(
                    "progress",
                    json!({
                        "event_type": "worker.create_target_started",
                        "node_id": node_id,
                        "operation": operation,
                        "target_path": path,
                        "source_path": source_path,
                        "repository_fingerprint": repository_fingerprint,
                        "selected_mutation_tool": name,
                        "verification_result": "pending",
                        "process_health": "healthy",
                        "mission_outcome": "continuing",
                    }),
                    "target creation started",
                );
                match create_repo_file_atomically(&self.repo.root, path, content, create_parents) {
                    Ok(output) => {
                        self.append_event_recoverable(
                            "progress",
                            json!({
                                "event_type": "worker.create_target_completed",
                                "node_id": node_id,
                                "operation": operation,
                                "target_path": path,
                                "source_path": source_path,
                                "content_hash": sha256_text(content),
                                "repository_fingerprint_before": repository_fingerprint,
                                "selected_mutation_tool": name,
                                "verification_result": "content_verified",
                                "process_health": "healthy",
                                "mission_outcome": "continuing",
                            }),
                            "target creation completed",
                        );
                        Ok(output)
                    }
                    Err(error) => {
                        let mutation_error = error.downcast_ref::<MutationApplicationError>();
                        let failure_code = mutation_error
                            .map_or("target_creation_failed", |error| error.failure.as_str());
                        let process_health = if mutation_error.is_some() {
                            "healthy"
                        } else {
                            "degraded"
                        };
                        self.append_event_recoverable(
                            "progress",
                            json!({
                                "event_type": "worker.target_creation_failed",
                                "target_path": path,
                                "creation_tool": name,
                                "failure_code": failure_code,
                                "process_health": process_health,
                                "mission_outcome": "continuing",
                            }),
                            "target creation failed",
                        );
                        Err(error)
                    }
                }
            }
            "rename_file" | "move_file" => {
                self.tool_usage.writes = self.tool_usage.writes.saturating_add(1);
                let path = required_tool_string(object, "path", 4_096)?;
                let source = required_tool_string(object, "source", 4_096)?;
                let create_parents = object
                    .get("create_parents")
                    .and_then(Value::as_bool)
                    .context("tool argument `create_parents` is missing")?;
                let (operation, source_hash) = self
                    .active_target_operation()
                    .context("move requires an active operation-aware target context")?;
                self.verify_active_mutation_fingerprint()?;
                self.record_active_target_mutation_intent(source_hash.clone())?;
                if operation.source_path() != Some(source)
                    || operation.destination_path(path) != path
                {
                    bail!(
                        "mutation_tool_operation_mismatch: source or destination differs from accepted plan"
                    );
                }
                move_repo_file_atomically(
                    &self.repo.root,
                    source,
                    path,
                    source_hash.as_deref(),
                    create_parents,
                )
            }
            "repository_snapshot" => {
                if matches!(
                    self.phases.active(),
                    ExecutionPhase::Implementation | ExecutionPhase::Repair
                ) {
                    let reallocated = self.phases.release_unused_implementation_capacity();
                    for (target, calls) in [
                        ("diff_review", reallocated.diff_review_calls),
                        (
                            "completion_evaluation",
                            reallocated.completion_evaluation_calls,
                        ),
                    ] {
                        if calls > 0 {
                            self.append_event_recoverable(
                                "progress",
                                json!({
                                    "event_type": "worker.phase_budget_reallocated",
                                    "from": "implementation_repair",
                                    "to": target,
                                    "calls": calls,
                                    "reason": "implementation_finished_early",
                                    "budget": self.budget_telemetry(),
                                }),
                                "phase budget reallocation telemetry",
                            );
                        }
                    }
                }
                self.tool_usage.reads = self.tool_usage.reads.saturating_add(1);
                let paths = completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
                let diff = completion_review_diff(
                    &self.repo.root,
                    &paths,
                    &self.manifest.github.base_sha,
                )?;
                let digest = hex::encode(Sha256::digest(diff.as_bytes()));
                let requested_cursor = object
                    .get("cursor")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize;
                if requested_cursor != self.diff_review_cursor {
                    bail!(
                        "repository_snapshot cursor mismatch: expected {}, received {requested_cursor}",
                        self.diff_review_cursor
                    );
                }
                if self
                    .diff_review_digest
                    .as_ref()
                    .is_some_and(|previous| previous != &digest)
                {
                    self.diff_review_cursor = 0;
                    self.diff_review_digest = None;
                    bail!("repository diff changed during review; restart at cursor 0");
                }
                self.diff_review_digest = Some(digest.clone());
                let start = requested_cursor.min(diff.len());
                let mut end = start
                    .saturating_add(MAX_TOOL_OUTPUT_BYTES.saturating_sub(8 * 1024))
                    .min(diff.len());
                while end > start && !diff.is_char_boundary(end) {
                    end -= 1;
                }
                let next_cursor = (end < diff.len()).then_some(end);
                self.diff_review_cursor = next_cursor.unwrap_or(diff.len());
                let status = command::checked(
                    "git",
                    ["status", "--short", "--untracked-files=all"],
                    &self.repo.root,
                )?;
                let statistics = command::checked(
                    "git",
                    ["diff", "--stat", "--no-ext-diff", "--"],
                    &self.repo.root,
                )?;
                self.diff_reviewed = next_cursor.is_none();
                Ok(format!(
                    "git_status:\n{status}\n\ndiff_statistics:\n{statistics}\n\nchanged_paths:\n{}\n\ndiff_sha256: {digest}\nreview_cursor: {start}\nnext_cursor: {}\nreview_complete: {}\n\ndiff_page:\n{}",
                    paths.join("\n"),
                    next_cursor
                        .map(|cursor| cursor.to_string())
                        .unwrap_or_else(|| "null".into()),
                    self.diff_reviewed,
                    &diff[start..end],
                ))
            }
            "record_impact_map" => {
                let (map, source) =
                    impact_map_from_value(Value::Object(object.clone()), &self.notebook)
                        .context("impact map is malformed")?;
                self.accept_impact_map(map, source, 1.0, None)
            }
            "record_implementation_plan" => {
                let mut repair = recover_planning_repair_state(
                    &self.repo.root,
                    object,
                    self.phases.total_calls(),
                );
                let mut plan: ImplementationPlan =
                    match serde_json::from_value(Value::Object(object.clone())) {
                        Ok(plan) => plan,
                        Err(error) => {
                            repair
                                .invalid_fields
                                .push(format!("$: {}", truncate_text(&error.to_string(), 500)));
                            self.notebook.planning_repair = Some(repair.clone());
                            self.append_event_recoverable(
                                "progress",
                                json!({
                                    "event_type": "worker.implementation_plan_repair_required",
                                    "valid_planned_changes": repair.valid_planned_changes,
                                    "invalid_fields": repair.invalid_fields,
                                    "repair_scope": "invalid_fields_only",
                                }),
                                "implementation plan repair",
                            );
                            bail!(
                                "implementation_plan_repair_required: {}",
                                serde_json::to_string(&repair.invalid_fields)?
                            );
                        }
                    };
                merge_preserved_plan_fragments(
                    &mut plan.planned_changes,
                    self.notebook.planning_repair.as_ref(),
                );
                let normalized_legacy_targets =
                    match validate_explicit_target_operations(&plan.planned_changes)
                        .and_then(|()| normalize_planned_changes(&mut plan.planned_changes))
                        .and_then(|count| {
                            validate_planned_change_paths(&self.repo.root, &plan.planned_changes)?;
                            Ok(count)
                        }) {
                        Ok(count) => count,
                        Err(error) => {
                            repair.invalid_fields.push(format!(
                                "$.planned_changes: {}",
                                truncate_text(&error.to_string(), 500)
                            ));
                            self.notebook.planning_repair = Some(repair.clone());
                            bail!(
                                "implementation_plan_repair_required: {}",
                                serde_json::to_string(&repair.invalid_fields)?
                            );
                        }
                    };
                if !matches!(plan.implementation_status.as_str(), "ready" | "blocked")
                    || (plan.implementation_status == "ready" && plan.planned_changes.is_empty())
                    || plan.planned_changes.iter().any(|change| {
                        change.targets.is_empty()
                            || change.change.trim().is_empty()
                            || change.reason.trim().is_empty()
                            || change.acceptance_criteria.is_empty()
                    })
                {
                    repair.invalid_fields.push(
                        "$: implementation_status and every planned change require complete fields"
                            .into(),
                    );
                    self.notebook.planning_repair = Some(repair.clone());
                    bail!(
                        "implementation_plan_repair_required: {}",
                        serde_json::to_string(&repair.invalid_fields)?
                    );
                }
                let criteria = self.notebook.acceptance_criteria_v2.clone();
                let impact_areas = self
                    .impact_map
                    .as_ref()
                    .map(|map| map.areas.as_slice())
                    .unwrap_or(self.notebook.impact_map.as_slice());
                let accepted =
                    match validate_and_repair_plan_criteria(plan, &criteria, impact_areas) {
                        Ok(validated) => validated,
                        Err(error) => {
                            repair
                                .invalid_fields
                                .push(truncate_text(&error.to_string(), 500));
                            self.notebook.planning_repair = Some(repair.clone());
                            bail!(
                                "implementation_plan_repair_required: {}",
                                serde_json::to_string(&repair.invalid_fields)?
                            );
                        }
                    };
                let repaired_criterion_ids = accepted
                    .criterion_assignments
                    .iter()
                    .map(|assignment| assignment.acceptance_criterion_id.clone())
                    .collect::<Vec<_>>();
                // Replace the provider candidate before any later validation or persistence.
                // This prevents the original, coverage-incomplete payload from being reused.
                plan = accepted.plan;
                let target_count = plan
                    .planned_changes
                    .iter()
                    .map(|change| change.targets.len())
                    .sum::<usize>();
                let fingerprint =
                    repository_state_fingerprint(self.repo, &self.manifest.github.base_sha)?;
                let complexity = self
                    .notebook
                    .orchestration
                    .rebuild_from_plan(self.manifest, &plan, &fingerprint)
                    .clone();
                let complexity_call_limit = self.phases.apply_complexity_limit(
                    usize::try_from(complexity.budget.max_model_calls).unwrap_or(usize::MAX),
                );
                self.cost_guard.hard_limit_micros = complexity.budget.max_cost_micros;
                self.cost_guard.max_duration_seconds = complexity.budget.max_duration.as_secs();
                self.api.append_event(
                    "progress",
                    json!({
                        "event_type": "worker.implementation_plan_validated",
                        "change_count": plan.planned_changes.len(),
                        "target_count": target_count,
                        "normalized_legacy_targets": normalized_legacy_targets,
                        "normalization_source": (normalized_legacy_targets > 0)
                            .then_some("legacy_semicolon_target"),
                        "criterion_assignments": accepted.criterion_assignments,
                        "criterion_repair_model_call_consumed": accepted.model_call_consumed,
                        "complexity_class": complexity.class,
                        "complexity_score": complexity.score,
                        "complexity_factors": complexity.factors,
                        "mission_budget": complexity.budget,
                        "ticket_complexity_call_limit": complexity_call_limit,
                    }),
                )?;
                self.notebook.planning_repair = None;
                self.notebook.write_attempts.clear();
                self.notebook.blocking_unknowns = plan.blocking_unknowns.clone();
                let ready = accepted.next_phase == ExecutionPhase::Implementation;
                // The accepted plan is source input for the graph. Legacy plan,
                // intended-change, substate, and remaining-work fields are
                // materialized from that graph when the domain event is applied.
                self.implementation_plan = Some(plan);
                self.record_planning_failures_recovered(&fingerprint)?;
                self.append_execution_domain_event(
                    crate::execution_graph::ExecutionDomainEvent::ComplexityClassified {
                        sequence: self.next_domain_event_sequence(),
                        assessment: complexity.clone(),
                    },
                )?;
                if !repaired_criterion_ids.is_empty() {
                    self.append_execution_domain_event(
                        crate::execution_graph::ExecutionDomainEvent::PlanRepaired {
                            sequence: self.next_domain_event_sequence(),
                            repaired_criterion_ids,
                        },
                    )?;
                }
                self.append_execution_domain_event(
                    crate::execution_graph::ExecutionDomainEvent::PlanAccepted {
                        sequence: self.next_domain_event_sequence(),
                        target_count: u32::try_from(target_count).unwrap_or(u32::MAX),
                    },
                )?;
                let preserved_node_ids = self
                    .notebook
                    .orchestration
                    .pending_topology_preserved_node_ids
                    .clone();
                let graph = self
                    .notebook
                    .orchestration
                    .graph
                    .as_ref()
                    .context("accepted plan did not create an execution graph")?;
                self.append_execution_domain_event(
                    crate::execution_graph::ExecutionDomainEvent::GraphCreated {
                        sequence: self.next_domain_event_sequence(),
                        graph_id: graph.graph_id.clone(),
                        revision: graph.revision,
                        graph: Some(graph.clone()),
                        preserved_node_ids,
                    },
                )?;
                self.persist_orchestration_checkpoint("plan_graph_created", false)?;
                self.guided_first_write_recovery_issued = false;
                self.last_repository_progress_call = 0;
                if ready {
                    self.blocked_plan_recorded_at = None;
                    let decision = self.reconcile_execution_and_apply()?;
                    if !matches!(decision.decision, ExecutionDecision::ExecuteTarget { .. }) {
                        bail!("accepted implementation plan did not produce a runnable target");
                    }
                } else {
                    self.blocked_plan_recorded_at =
                        Some(self.phases.phase_calls(ExecutionPhase::Planning));
                }
                Ok(if ready {
                    "recorded implementation plan; transition to implementation".into()
                } else {
                    "recorded blocked implementation plan; one targeted inspection cycle remains"
                        .into()
                })
            }
            "report_write_progress" => {
                let status = required_tool_string(object, "status", 64)?;
                let reason = required_tool_string(object, "reason", 2_000)?;
                informational_write_progress(status, reason)
            }
            "declare_implementation" => {
                if !self.diff_reviewed {
                    bail!(
                        "repository_snapshot is required after the final source change and before implementation declaration"
                    );
                }
                let declaration: ImplementationDeclaration =
                    serde_json::from_value(Value::Object(object.clone()))
                        .context("implementation declaration is malformed")?;
                if !matches!(
                    declaration.implementation_status.as_str(),
                    "complete" | "partial" | "blocked"
                ) {
                    bail!("implementation declaration has an unsupported status");
                }
                let actual_paths =
                    completion_changed_paths(self.repo, &self.manifest.github.base_sha)?;
                if declaration.changed_paths != actual_paths {
                    bail!(
                        "implementation declaration changed_paths must exactly match the reviewed repository paths"
                    );
                }
                if declaration.implementation_status == "complete"
                    && (declaration.criteria_evidence.is_empty()
                        || declaration.criteria_evidence.iter().any(|criterion| {
                            criterion.criterion.trim().is_empty()
                                || criterion.evidence.trim().is_empty()
                                || criterion.paths.is_empty()
                                || criterion
                                    .paths
                                    .iter()
                                    .any(|path| !actual_paths.contains(path))
                        }))
                {
                    bail!(
                        "a complete implementation declaration requires criterion evidence tied to changed paths"
                    );
                }
                let authoritative_remaining =
                    legacy_remaining_work(&derive_remaining_work(&self.notebook.intended_changes));
                if declaration.remaining_work != authoritative_remaining {
                    self.append_event_recoverable(
                        "progress",
                        json!({
                            "event_type": "worker.remaining_work_reconciled",
                            "declared_remaining_work": declaration.remaining_work,
                            "authoritative_remaining_work": authoritative_remaining,
                            "reason": "remaining_work_is_derived_from_target_state",
                        }),
                        "remaining work reconciliation",
                    );
                }
                self.declaration = Some(declaration);
                Ok("recorded implementation declaration".into())
            }
            _ => bail!("unsupported hosted model tool `{name}`"),
        }
    }

    pub(in crate::hosted) fn validate_tool_for_phase(
        &self,
        name: &str,
        arguments: &serde_json::Map<String, Value>,
    ) -> Result<()> {
        let phase = self.phases.active();
        if !phase_permits_tool(phase, name) {
            bail!(
                "tool `{name}` is not permitted during phase `{}`",
                phase.as_str()
            );
        }
        if matches!(
            phase,
            ExecutionPhase::Discovery | ExecutionPhase::ArtifactRepair
        ) && !discovery_action_permits_tool(self.current_decision.as_ref(), name)
        {
            bail!(
                "discovery_action_tool_not_permitted: tool `{name}` is not available for the selected discovery action"
            );
        }
        if phase == ExecutionPhase::Planning
            && !planning_action_permits_tool(self.current_decision.as_ref(), name)
        {
            bail!(
                "planning_action_tool_not_permitted: tool `{name}` is not available for the selected planning action"
            );
        }
        if name == "search_text"
            && phase != ExecutionPhase::Discovery
            && arguments
                .get("path")
                .and_then(Value::as_str)
                .is_none_or(|path| matches!(path.trim_matches('/'), "" | "." | "src"))
        {
            bail!(
                "broad repository searches are not permitted during phase `{}`; target a planned path or concrete failure",
                phase.as_str()
            );
        }
        if matches!(
            name,
            "read_file" | "read_files" | "search_text" | "related_tests"
        ) && required_tool_string(arguments, "reason", 2_000)?
            .trim()
            .is_empty()
        {
            bail!("targeted repository inspection requires a concrete reason");
        }
        if phase == ExecutionPhase::Discovery
            && matches!(
                name,
                "list_files" | "read_file" | "read_files" | "search_text" | "related_tests"
            )
        {
            let requested_paths = discovery_requested_paths(name, arguments);
            validate_localized_discovery_scope(&self.notebook, &requested_paths)?;
        }
        if matches!(
            phase,
            ExecutionPhase::Implementation | ExecutionPhase::Repair
        ) {
            let paths = match name {
                "read_file" => arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .into_iter()
                    .collect::<Vec<_>>(),
                "read_files" | "related_tests" => arguments
                    .get("paths")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>(),
                "search_text" => arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .into_iter()
                    .collect::<Vec<_>>(),
                _ if is_source_mutation_tool(name) => arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .into_iter()
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            if !paths.is_empty() && paths.iter().any(|path| !self.path_is_targeted(path)) {
                bail!(
                    "implementation and repair reads must target a planned edit, mapped criterion, or failed write"
                );
            }
            if !is_source_mutation_tool(name) {
                let current_target = self.current_implementation_target();
                validate_current_target_scope(
                    current_target.as_ref(),
                    self.guided_first_write_recovery_issued,
                    self.tool_usage.successful_writes,
                    &paths,
                    false,
                )?;
            }
        }
        Ok(())
    }

    pub(in crate::hosted) fn path_is_targeted(&self, path: &str) -> bool {
        let path = path.trim_matches('/');
        let related = |candidate: &str| {
            let candidate = candidate.trim_matches('/');
            path == candidate
                || candidate.starts_with(&format!("{path}/"))
                || path.starts_with(&format!("{candidate}/"))
        };
        self.implementation_plan.as_ref().is_some_and(|plan| {
            plan.planned_changes
                .iter()
                .flat_map(|change| &change.targets)
                .any(|target| related(&target.path))
                || plan.planned_new_files.iter().any(|file| related(file))
                || plan.planned_test_changes.iter().any(|file| related(file))
        }) || self.impact_map.as_ref().is_some_and(|map| {
            map.areas
                .iter()
                .flat_map(|area| &area.candidate_paths)
                .any(|candidate| related(candidate))
        }) || self
            .tool_failures
            .iter()
            .filter_map(|failure| failure.target.as_deref())
            .any(related)
    }
}
pub(in crate::hosted) fn required_tool_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    name: &str,
    maximum: usize,
) -> Result<&'a str> {
    object
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= maximum)
        .with_context(|| format!("tool argument `{name}` is missing or too large"))
}

pub(in crate::hosted) fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

pub(in crate::hosted) fn is_source_mutation_tool(name: &str) -> bool {
    matches!(
        name,
        "write_file"
            | "replace_text"
            | "replace_range"
            | "insert_after_symbol"
            | "insert_before_symbol"
            | "apply_patch"
            | "replace_file"
            | "apply_unified_diff"
            | "rewrite_small_file"
            | "delete_file"
            | "create_file"
            | "rename_file"
            | "move_file"
    )
}

pub(in crate::hosted) fn informational_write_progress(
    status: &str,
    reason: &str,
) -> Result<String> {
    if !matches!(status, "blocked" | "ready_to_write" | "no_change_required") {
        bail!("write progress status is unsupported");
    }
    Ok(format!(
        "recorded informational write progress (repository_progress=false): {status}: {reason}"
    ))
}

pub(in crate::hosted) const fn informational_write_progress_semantics() -> (ToolProgressClass, bool)
{
    (ToolProgressClass::Neutral, false)
}

pub(in crate::hosted) fn tool_target(arguments: &str) -> Option<String> {
    serde_json::from_str::<Value>(arguments)
        .ok()?
        .get("path")?
        .as_str()
        .map(|path| truncate_text(path, 4_096))
}

pub(in crate::hosted) fn tool_change_id(arguments: &str) -> Option<String> {
    serde_json::from_str::<Value>(arguments)
        .ok()?
        .get("change_id")?
        .as_str()
        .map(|change_id| truncate_text(change_id, 100))
}

pub(in crate::hosted) fn repo_file_sha256(root: &Path, path: &str) -> Option<String> {
    let target = safe_repo_path(root, path, false).ok()?;
    let bytes = fs::read(target).ok()?;
    Some(hex::encode(Sha256::digest(bytes)))
}

pub(in crate::hosted) fn classify_write_failure(_error: &str) -> (String, Option<usize>) {
    ("mutation_content_conflict".into(), None)
}

pub(in crate::hosted) fn tool_intent_sha256(name: &str, arguments: &str) -> String {
    let mut material = name.as_bytes().to_vec();
    material.push(0);
    material.extend_from_slice(arguments.as_bytes());
    hex::encode(Sha256::digest(material))
}

pub(in crate::hosted) fn model_budget_handoff_summary(
    allowed: bool,
    changed_paths: &[String],
) -> Option<String> {
    (allowed && !changed_paths.is_empty()).then(|| {
        format!(
            "The implementation model used its configured call budget after changing {} path(s). RustGrid will preserve the work, run useful technical gates, and classify it through an independent completion evaluation; passing gates alone cannot mark it complete.",
            changed_paths.len()
        )
    })
}

pub(in crate::hosted) fn ai_budget_exhaustion_reason(error: &anyhow::Error) -> Option<String> {
    error
        .downcast_ref::<HostedHttpError>()
        .filter(|failure| failure.code == "execution_ai_budget_exceeded")
        .map(|failure| failure.code.clone())
}
