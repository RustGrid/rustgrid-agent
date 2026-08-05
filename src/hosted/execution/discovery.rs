// Extracted from the hosted execution composition root.
use super::*;

pub(in crate::hosted) fn validate_impact_map(
    map: &ImpactMap,
    notebook: &WorkerNotebook,
) -> Result<()> {
    let errors = impact_map::validate(map, notebook.acceptance_criteria.len());
    if !errors.is_empty() {
        bail!(
            "{}",
            serde_json::to_string(&json!({
                "code": "impact_map_schema_mismatch",
                "errors": errors,
            }))?
        );
    }
    Ok(())
}

pub(in crate::hosted) fn impact_map_from_value(
    value: Value,
    notebook: &WorkerNotebook,
) -> Result<(ImpactMap, ArtifactSource)> {
    impact_map::normalize(
        &value,
        &notebook.files_inspected,
        &notebook.searches_completed,
        &notebook.acceptance_criteria,
    )
    .map_err(|errors| {
        anyhow!(
            serde_json::to_string(&json!({
                "code":"impact_map_schema_mismatch", "errors":errors
            }))
            .unwrap_or_else(|_| "impact_map_schema_mismatch".into())
        )
    })
}

pub(in crate::hosted) fn json_object_from_text(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed)
        && value.is_object()
    {
        return Some(value);
    }
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim);
    if let Some(unfenced) = unfenced
        && let Ok(value) = serde_json::from_str::<Value>(unfenced)
        && value.is_object()
    {
        return Some(value);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (start < end)
        .then(|| serde_json::from_str::<Value>(&trimmed[start..=end]).ok())
        .flatten()
        .filter(Value::is_object)
}

pub(in crate::hosted) fn recover_impact_map(
    raw_arguments: Option<&str>,
    assistant_text: Option<&str>,
    notebook: &WorkerNotebook,
) -> Result<(ImpactMap, ArtifactSource)> {
    let mut errors = Vec::new();
    for candidate in [
        raw_arguments.and_then(json_object_from_text),
        assistant_text.and_then(json_object_from_text),
    ]
    .into_iter()
    .flatten()
    {
        match impact_map_from_value(candidate, notebook) {
            Ok(map) => return Ok(map),
            Err(error) => errors.push(error.to_string()),
        }
    }
    bail!(
        "impact map recovery found no valid structured artifact{}",
        if errors.is_empty() {
            String::new()
        } else {
            format!(": {}", errors.join("; "))
        }
    )
}

pub(in crate::hosted) fn impact_map_sha256(map: &ImpactMap) -> Option<String> {
    serde_json::to_vec(map)
        .ok()
        .map(|encoded| hex::encode(Sha256::digest(encoded)))
}

pub(in crate::hosted) fn classify_impact_map_failure(error: &anyhow::Error) -> ImpactMapFailure {
    let safe_error = truncate_text(&format!("{error:#}"), 2_000);
    let lower = safe_error.to_ascii_lowercase();
    let code = if lower.contains("valid json")
        || lower.contains("strict artifact schema")
        || lower.contains("malformed")
    {
        "impact_map_schema_mismatch"
    } else if lower.contains("persist")
        || lower.contains("worker-events")
        || lower.contains("transport")
    {
        "impact_map_persistence_failed"
    } else if lower.contains("impact map") {
        "impact_map_invalid"
    } else {
        "impact_map_tool_failure"
    };
    ImpactMapFailure {
        code,
        safe_error,
        errors: Vec::new(),
        invalid_payload: Value::Null,
        invalid_payload_shape: impact_map::safe_shape(&Value::Null),
        failure_layer: ArtifactFailureLayer::WorkerToolSchemaValidation,
    }
}

pub(in crate::hosted) fn invalid_impact_map_semantic_status(
    value: &Value,
) -> ArtifactSemanticStatus {
    let areas = value
        .as_array()
        .or_else(|| value.get("areas").and_then(Value::as_array))
        .or_else(|| value.get("impact_map").and_then(Value::as_array));
    if areas.is_some_and(|areas| {
        areas.iter().any(|area| {
            area.get("name")
                .or_else(|| area.get("area"))
                .and_then(Value::as_str)
                .is_some_and(|v| !v.trim().is_empty())
                && area
                    .get("candidate_paths")
                    .and_then(Value::as_array)
                    .is_some_and(|v| !v.is_empty())
                && area
                    .get("reason")
                    .and_then(Value::as_str)
                    .is_some_and(|v| !v.trim().is_empty())
        })
    }) {
        ArtifactSemanticStatus::Partial
    } else if value.is_null() {
        ArtifactSemanticStatus::Missing
    } else {
        ArtifactSemanticStatus::Invalid
    }
}

impl<'a> GatewayAgent<'a> {
    pub(in crate::hosted) fn accept_impact_map(
        &mut self,
        map: ImpactMap,
        artifact_source: ArtifactSource,
        confidence: f64,
        triggering_error: Option<&anyhow::Error>,
    ) -> Result<String> {
        validate_impact_map(&map, &self.notebook)?;
        let artifact_sha256 = impact_map_sha256(&map);
        self.notebook.impact_map = map.areas.clone();
        self.notebook.impact_map_v2 = Some(map.clone());
        self.notebook.files_inspected = map.inspected_files.clone();
        self.notebook.searches_completed = map
            .searches
            .iter()
            .map(|search| search.query.clone())
            .collect();
        self.notebook.blocking_unknowns = map.unresolved_questions.clone();
        self.notebook.impact_map_artifact = ArtifactCheckpoint {
            artifact: "impact_map".into(),
            semantic_status: ArtifactSemanticStatus::Sufficient,
            serialization_status: if artifact_source == ArtifactSource::NormalizedModel {
                ArtifactSerializationStatus::Normalizable
            } else {
                ArtifactSerializationStatus::Valid
            },
            persistence_status: ArtifactPersistenceStatus::PendingRetry,
            artifact_sha256: artifact_sha256.clone(),
            model_call_index: Some(self.phases.total_calls()),
            phase: self.phases.active(),
            safe_error: None,
            normalization_metadata: accepted_artifact_normalization_metadata(
                artifact_source,
                triggering_error,
            ),
            artifact_source: Some(artifact_source),
            confidence: Some(confidence),
            failure_layer: None,
            validation_errors: Vec::new(),
            invalid_payload_shape: None,
        };
        self.notebook.impact_map_invalid_payload = None;
        self.impact_map = Some(map);
        self.impact_map_failure = None;
        self.record_discovery_completed()?;
        let persistence_error = self.reconcile_execution_and_apply()?.persistence_error;
        let persisted = persistence_error.is_none();
        self.notebook.impact_map_artifact.persistence_status = if persisted {
            ArtifactPersistenceStatus::Persisted
        } else {
            ArtifactPersistenceStatus::Failed
        };
        self.notebook.impact_map_artifact.phase = ExecutionPhase::Discovery;
        if artifact_source == ArtifactSource::NormalizedModel {
            self.api.append_event(
                "progress",
                json!({
                    "event_type": "worker.artifact_normalized",
                    "artifact": "impact_map",
                    "artifact_sha256": artifact_sha256,
                    "failure_layer": Value::Null,
                    "normalization_metadata": self.notebook.impact_map_artifact.normalization_metadata,
                }),
            )?;
        }
        if let Some(error) = persistence_error.as_ref() {
            self.notebook.impact_map_artifact.safe_error = Some(error.safe_error.clone());
            self.notebook.impact_map_artifact.failure_layer =
                Some(ArtifactFailureLayer::ArtifactPersistence);
        }
        if !persisted {
            self.append_event_recoverable(
                "progress",
                json!({
                    "event_type": "worker.artifact_persistence_failed",
                    "artifact": "impact_map",
                    "semantic_status": ArtifactSemanticStatus::Sufficient,
                    "serialization_status": self.notebook.impact_map_artifact.serialization_status,
                    "persistence_status": ArtifactPersistenceStatus::Failed,
                    "recoverable": true,
                    "action": "retry_or_continue",
                    "safe_error": persistence_error,
                    "artifact_source": artifact_source,
                    "confidence": confidence,
                    "tool_schema_version": IMPACT_MAP_SCHEMA_VERSION,
                    "tool_schema_sha256": impact_map::schema_sha256(),
                    "validator_schema_version": IMPACT_MAP_SCHEMA_VERSION,
                    "validator_schema_sha256": impact_map::schema_sha256(),
                    "notebook": self.notebook,
                    "checkpoint": self.notebook_checkpoint_metadata(
                        artifact_sha256.as_deref()
                    ),
                }),
                "impact-map fallback checkpoint",
            );
        }
        Ok(if persisted {
            format!("recorded implementation impact map from {artifact_source:?}")
        } else {
            format!(
                "impact map was semantically accepted from {artifact_source:?}; persistence is degraded and will be retried without another discovery model call"
            )
        })
    }

    pub(in crate::hosted) fn accept_deterministic_impact_map_if_available(
        &mut self,
        reason: &str,
    ) -> Result<bool> {
        let Some((map, confidence)) = impact_map::fallback_from_persisted_evidence(
            &self.notebook.files_inspected,
            &self.notebook.searches_completed,
            &self.notebook.acceptance_criteria,
            &self.notebook.blocking_unknowns,
            &self.notebook.architecture_findings,
        )
        .filter(|(map, confidence)| {
            *confidence >= impact_map_fallback_threshold(self.manifest)
                && impact_map::validate(map, self.notebook.acceptance_criteria.len()).is_empty()
        }) else {
            return Ok(false);
        };
        self.accept_impact_map(
            map,
            ArtifactSource::OrchestratorFallback,
            confidence,
            Some(&anyhow!(reason.to_owned())),
        )?;
        self.append_event_recoverable(
            "progress",
            json!({
                "event_type": "worker.impact_map_fallback_accepted",
                "artifact_source": "orchestrator_fallback",
                "confidence": confidence,
                "reason_code": reason,
                "process_health": "healthy",
                "mission_outcome": "continuing",
                "files_inspected": self.notebook.files_inspected,
            }),
            "impact-map deterministic fallback",
        );
        Ok(true)
    }
}
