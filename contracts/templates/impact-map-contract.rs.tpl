// Generated from rustgrid-agent/contracts/impact-map-v2.schema.json. Do not edit by hand.
use std::collections::BTreeSet;

use serde_json::{json, Value};

use ticket_core::error::{bad_request, Result};

pub const IMPACT_MAP_SCHEMA_VERSION: &str = "{{SCHEMA_VERSION}}";
pub const IMPACT_MAP_SCHEMA_SHA256: &str =
    "{{SCHEMA_SHA256}}";
pub const IMPACT_MAP_SCHEMA_JSON: &str =
    include_str!("../../../contracts/impact-map-v2.schema.json");

pub fn validate_worker_event(data: &Value) -> Result<()> {
    if let Some(map) = data
        .get("notebook")
        .and_then(|notebook| notebook.get("impact_map_v2"))
        .filter(|map| !map.is_null())
    {
        validate_artifact(map)?;
    }
    let Some(event_type) = data.get("event_type").and_then(Value::as_str) else {
        return Ok(());
    };
    if !matches!(
        event_type,
        "worker.impact_map_artifact_attempt"
            | "worker.artifact_repair_required"
            | "worker.impact_map_fallback_accepted"
    ) {
        return Ok(());
    }
    let matches = data.get("tool_schema_version").and_then(Value::as_str)
        == Some(IMPACT_MAP_SCHEMA_VERSION)
        && data.get("validator_schema_version").and_then(Value::as_str)
            == Some(IMPACT_MAP_SCHEMA_VERSION)
        && data.get("tool_schema_sha256").and_then(Value::as_str) == Some(IMPACT_MAP_SCHEMA_SHA256)
        && data.get("validator_schema_sha256").and_then(Value::as_str)
            == Some(IMPACT_MAP_SCHEMA_SHA256);
    if !matches {
        return Err(
            bad_request("impact-map tool and validator contracts differ")
                .with_code("impact_map_contract_mismatch")
                .at("http.execution_worker.event.impact_map_contract"),
        );
    }
    Ok(())
}

fn validate_artifact(map: &Value) -> Result<()> {
    let mut errors = Vec::new();
    reject_unknown(
        map,
        "$",
        &[
            "schema_version",
            "areas",
            "inspected_files",
            "searches",
            "unresolved_questions",
        ],
        &mut errors,
    );
    if map.get("schema_version").and_then(Value::as_str) != Some(IMPACT_MAP_SCHEMA_VERSION) {
        errors.push(json!({"path":"$.schema_version","keyword":"const","message":"Expected rustgrid.impact_map.v2."}));
    }
    let areas = map.get("areas").and_then(Value::as_array);
    if areas.is_none_or(Vec::is_empty) {
        errors.push(json!({"path":"$.areas","keyword":"minItems","message":"At least one impact area is required."}));
    }
    if map
        .get("inspected_files")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        errors.push(json!({"path":"$.inspected_files","keyword":"minItems","message":"At least one inspected file is required."}));
    }
    for (index, area) in areas.into_iter().flatten().enumerate() {
        let base = format!("$.areas[{index}]");
        reject_unknown(
            area,
            &base,
            &[
                "area_id",
                "name",
                "candidate_paths",
                "evidence",
                "acceptance_criteria_ids",
                "reason",
            ],
            &mut errors,
        );
        for field in ["area_id", "name", "reason"] {
            if area
                .get(field)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                errors.push(json!({"path":format!("{base}.{field}"),"keyword":"required","message":"Required property is missing."}));
            }
        }
        for field in ["candidate_paths", "evidence", "acceptance_criteria_ids"] {
            if area
                .get(field)
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
            {
                errors.push(json!({"path":format!("{base}.{field}"),"keyword":"minItems","message":"At least one item is required."}));
            }
        }
        for (evidence_index, evidence) in area
            .get("evidence")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let evidence_base = format!("{base}.evidence[{evidence_index}]");
            reject_unknown(
                evidence,
                &evidence_base,
                &["type", "path", "query", "description"],
                &mut errors,
            );
            if !matches!(
                evidence.get("type").and_then(Value::as_str),
                Some(
                    {{EVIDENCE_PATTERN}}
                )
            ) {
                errors.push(json!({"path":format!("{evidence_base}.type"),"keyword":"enum","message":"Unsupported evidence type."}));
            }
            if evidence
                .get("description")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                errors.push(json!({"path":format!("{evidence_base}.description"),"keyword":"required","message":"Required property is missing."}));
            }
            for field in ["path", "query"] {
                if evidence.get(field).is_none() {
                    errors.push(json!({"path":format!("{evidence_base}.{field}"),"keyword":"required","message":"Required property is missing."}));
                }
            }
        }
    }
    for (index, search) in map
        .get("searches")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let base = format!("$.searches[{index}]");
        reject_unknown(search, &base, &["query", "scope"], &mut errors);
        if search
            .get("query")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            errors.push(json!({"path":format!("{base}.query"),"keyword":"required","message":"Required property is missing."}));
        }
        if search.get("scope").is_none() {
            errors.push(json!({"path":format!("{base}.scope"),"keyword":"required","message":"Required property is missing."}));
        }
    }
    for field in ["searches", "unresolved_questions"] {
        if map.get(field).and_then(Value::as_array).is_none() {
            errors.push(json!({"path":format!("$.{field}"),"keyword":"required","message":"Required property is missing."}));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(
            bad_request("impact map does not match rustgrid.impact_map.v2")
                .with_code("impact_map_schema_mismatch")
                .with_details(json!({"code":"impact_map_schema_mismatch","errors":errors}))
                .at("http.execution_worker.event.impact_map"),
        )
    }
}

fn reject_unknown(value: &Value, base: &str, allowed: &[&str], errors: &mut Vec<Value>) {
    let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
    for field in value
        .as_object()
        .into_iter()
        .flat_map(|object| object.keys())
    {
        if !allowed.contains(field.as_str()) {
            errors.push(json!({
                "path":format!("{base}.{field}"),
                "keyword":"additionalProperties",
                "message":"Unknown property is not allowed."
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn accepts_matching_contract_and_rejects_hash_drift() {
        let matching = json!({
            "event_type":"worker.impact_map_artifact_attempt",
            "tool_schema_version":IMPACT_MAP_SCHEMA_VERSION,
            "validator_schema_version":IMPACT_MAP_SCHEMA_VERSION,
            "tool_schema_sha256":IMPACT_MAP_SCHEMA_SHA256,
            "validator_schema_sha256":IMPACT_MAP_SCHEMA_SHA256,
        });
        assert!(validate_worker_event(&matching).is_ok());
        let mut drifted = matching;
        drifted["tool_schema_sha256"] = json!("drifted");
        assert!(validate_worker_event(&drifted).is_err());
    }

    #[test]
    fn generated_schema_hash_matches_server_validator() {
        assert_eq!(
            hex::encode(Sha256::digest(IMPACT_MAP_SCHEMA_JSON.as_bytes())),
            IMPACT_MAP_SCHEMA_SHA256,
        );
    }

    #[test]
    fn generated_validator_source_has_not_drifted() {
        assert_eq!(
            hex::encode(Sha256::digest(
                include_str!("impact_map_contract.rs").as_bytes()
            )),
            include_str!("../../../contracts/impact-map-validator.sha256").trim(),
        );
    }

    #[test]
    fn persisted_artifact_returns_precise_json_path_errors() {
        let error = validate_worker_event(&json!({
            "event_type":"worker.phase_transition",
            "notebook":{"impact_map_v2":{
                "schema_version":IMPACT_MAP_SCHEMA_VERSION,
                "areas":[{"area_id":"area-1","name":"Theme","candidate_paths":["src/theme.rs"],"evidence":[],"acceptance_criteria_ids":["ac-1"],"reason":"state"}],
                "inspected_files":[],
                "searches":[],
                "unresolved_questions":[]
            }}
        })).unwrap_err();
        let details = error.details().unwrap().to_string();
        assert!(details.contains("$.areas[0].evidence"));
        assert!(details.contains("$.inspected_files"));
    }
}
