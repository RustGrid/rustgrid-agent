use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[path = "impact_map_types_generated.rs"]
mod generated;
pub use generated::{EvidenceType, ImpactArea, ImpactEvidence, ImpactMap, ImpactSearch};

pub const IMPACT_MAP_SCHEMA_VERSION: &str = "rustgrid.impact_map.v2";
pub const IMPACT_MAP_SCHEMA_JSON: &str = include_str!("../../contracts/impact-map-v2.schema.json");

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct EvidenceReference {
    pub evidence_id: String,
    #[serde(rename = "type")]
    pub evidence_type: EvidenceType,
    pub path: Option<String>,
    pub query: Option<String>,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub text: String,
}

pub fn acceptance_criteria(criteria: &[String]) -> Vec<AcceptanceCriterion> {
    criteria
        .iter()
        .enumerate()
        .map(|(index, text)| AcceptanceCriterion {
            id: criterion_id(index),
            text: text.clone(),
        })
        .collect()
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationError {
    pub path: String,
    pub keyword: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct InvalidPayloadShape {
    pub top_level_keys: Vec<String>,
    pub area_count: usize,
    pub area_field_presence: Vec<BTreeMap<String, bool>>,
    pub omitted_required_fields: Vec<String>,
    pub unknown_fields: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSource {
    Model,
    NormalizedModel,
    OrchestratorFallback,
}

pub fn schema() -> Value {
    serde_json::from_str(IMPACT_MAP_SCHEMA_JSON).expect("embedded impact-map schema is valid JSON")
}

pub fn schema_sha256() -> String {
    hex::encode(Sha256::digest(IMPACT_MAP_SCHEMA_JSON.as_bytes()))
}

/// The provider view is mechanically projected from the canonical persisted schema.
/// Orchestrator-owned wrapper fields and evidence bodies are attached after the call.
pub fn provider_tool_schema() -> Value {
    let canonical = schema();
    let area = canonical["properties"]["areas"]["items"].clone();
    let mut properties = area["properties"].as_object().cloned().unwrap_or_default();
    properties.remove("area_id");
    properties.remove("evidence");
    properties.insert(
        "evidence_refs".into(),
        json!({"type":"array","items":{"type":"string"}}),
    );
    let mut projected = json!({
        "type":"object",
        "additionalProperties":false,
        "properties":{"areas":{"type":"array","minItems":1,"items":{
            "type":"object","additionalProperties":false,
            "properties":properties,
            "required":["name","candidate_paths","evidence_refs","acceptance_criteria_ids","reason"]
        }}},
        "required":["areas"]
    });
    strip_provider_unsupported_keywords(&mut projected);
    projected
}

fn strip_provider_unsupported_keywords(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("pattern");
            object.remove("minLength");
            for child in object.values_mut() {
                strip_provider_unsupported_keywords(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_provider_unsupported_keywords(item);
            }
        }
        _ => {}
    }
}

pub fn criterion_id(index: usize) -> String {
    format!("ac-{}", index + 1)
}

pub fn evidence_catalog(files: &[String], searches: &[String]) -> Vec<EvidenceReference> {
    let mut result = files
        .iter()
        .enumerate()
        .map(|(index, path)| EvidenceReference {
            evidence_id: format!("read-{}", index + 1),
            evidence_type: EvidenceType::FileRead,
            path: Some(path.clone()),
            query: None,
            description: "File was inspected during repository discovery.".into(),
        })
        .collect::<Vec<_>>();
    result.extend(
        searches
            .iter()
            .enumerate()
            .map(|(index, query)| EvidenceReference {
                evidence_id: format!("search-{}", index + 1),
                evidence_type: EvidenceType::SearchMatch,
                path: None,
                query: Some(query.clone()),
                description: "Search was completed during repository discovery.".into(),
            }),
    );
    result
}

fn split_path_evidence_reference(reference: &str) -> (&str, Option<&str>) {
    let Some((path, suffix)) = reference.rsplit_once(':') else {
        return (reference, None);
    };
    let valid_range = suffix.split_once('-').is_some_and(|(start, end)| {
        !start.is_empty()
            && !end.is_empty()
            && start.bytes().all(|byte| byte.is_ascii_digit())
            && end.bytes().all(|byte| byte.is_ascii_digit())
    });
    if suffix.bytes().all(|byte| byte.is_ascii_digit()) || valid_range {
        (path, Some(suffix))
    } else {
        (reference, None)
    }
}

fn resolve_evidence_reference<'a>(
    reference: &str,
    catalog: &'a [EvidenceReference],
) -> Option<(&'a EvidenceReference, Option<String>)> {
    if let Some(item) = catalog.iter().find(|item| item.evidence_id == reference) {
        return Some((item, None));
    }
    let (path, range) = split_path_evidence_reference(reference);
    let mut matches = catalog.iter().filter(|item| {
        item.evidence_type == EvidenceType::FileRead && item.path.as_deref() == Some(path)
    });
    let item = matches.next()?;
    matches
        .next()
        .is_none()
        .then_some((item, range.map(str::to_owned)))
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect()
}

fn stable_area_id(name: &str, paths: &[String]) -> String {
    let seed = format!("{}\0{}", name.trim(), paths.join("\0"));
    format!(
        "area-{}",
        &hex::encode(Sha256::digest(seed.as_bytes()))[..12]
    )
}

fn searches_from_notebook(searches: &[String]) -> Vec<ImpactSearch> {
    searches
        .iter()
        .map(|query| ImpactSearch {
            query: query.clone(),
            scope: None,
        })
        .collect()
}

pub fn normalize(
    input: &Value,
    inspected_files: &[String],
    completed_searches: &[String],
    criteria: &[String],
) -> Result<(ImpactMap, ArtifactSource), Vec<ValidationError>> {
    if let Ok(mut map) = serde_json::from_value::<ImpactMap>(input.clone()) {
        let mut source = ArtifactSource::Model;
        if map.inspected_files.is_empty() && !inspected_files.is_empty() {
            map.inspected_files = inspected_files.to_vec();
            source = ArtifactSource::NormalizedModel;
        }
        if map.searches.is_empty() && !completed_searches.is_empty() {
            map.searches = searches_from_notebook(completed_searches);
            source = ArtifactSource::NormalizedModel;
        }
        let errors = validate(&map, criteria.len());
        return if errors.is_empty() {
            Ok((map, source))
        } else {
            Err(errors)
        };
    }
    if input.get("schema_version").is_some() {
        let shape = safe_shape(input);
        if !shape.unknown_fields.is_empty() {
            return Err(shape
                .unknown_fields
                .into_iter()
                .map(|field| ValidationError {
                    path: format!("$.{field}"),
                    keyword: "additionalProperties".into(),
                    message: "Unknown property is not allowed.".into(),
                })
                .collect());
        }
    }
    let areas = input
        .as_array()
        .or_else(|| input.get("areas").and_then(Value::as_array))
        .or_else(|| input.get("impact_map").and_then(Value::as_array));
    let Some(areas) = areas else {
        return Err(vec![required("$.areas")]);
    };
    let catalog = evidence_catalog(inspected_files, completed_searches);
    let mut normalized = Vec::new();
    let mut errors = Vec::new();
    for (index, raw) in areas.iter().enumerate() {
        let base = format!("$.areas[{index}]");
        let name = raw
            .get("name")
            .or_else(|| raw.get("area"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let paths = strings(raw.get("candidate_paths"));
        let reason = raw
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let mut criterion_ids = strings(raw.get("acceptance_criteria_ids"));
        for text in strings(raw.get("acceptance_criteria")) {
            if let Some(index) = criteria
                .iter()
                .position(|criterion| criterion.trim() == text)
            {
                criterion_ids.push(criterion_id(index));
            }
        }
        criterion_ids.sort();
        criterion_ids.dedup();
        let refs = strings(raw.get("evidence_refs"));
        let mut evidence = Vec::new();
        for reference in refs {
            match resolve_evidence_reference(&reference, &catalog) {
                Some((item, range)) => evidence.push(ImpactEvidence {
                    evidence_type: item.evidence_type,
                    path: item.path.clone(),
                    query: item.query.clone(),
                    description: range.as_deref().map_or_else(
                        || item.description.clone(),
                        |range| format!("{} Referenced lines {range}.", item.description),
                    ),
                }),
                None => errors.push(ValidationError {
                    path: format!("{base}.evidence_refs"),
                    keyword: "reference".into(),
                    message: format!("Unknown evidence reference `{reference}`."),
                }),
            }
        }
        if evidence.is_empty() {
            evidence.extend(paths.iter().map(|path| ImpactEvidence {
                evidence_type: EvidenceType::Inference,
                path: Some(path.clone()),
                query: None,
                description: "Candidate path was identified during repository discovery.".into(),
            }));
        }
        if name.is_empty() {
            errors.push(required(&format!("{base}.name")));
        }
        if paths.is_empty() {
            errors.push(min_items(
                &format!("{base}.candidate_paths"),
                "At least one candidate path is required.",
            ));
        }
        if reason.is_empty() {
            errors.push(required(&format!("{base}.reason")));
        }
        if criterion_ids.is_empty() && !criteria.is_empty() {
            criterion_ids = (0..criteria.len()).map(criterion_id).collect();
        }
        normalized.push(ImpactArea {
            area_id: stable_area_id(&name, &paths),
            name,
            candidate_paths: paths,
            evidence,
            acceptance_criteria_ids: criterion_ids,
            reason,
        });
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let map = ImpactMap {
        schema_version: IMPACT_MAP_SCHEMA_VERSION.into(),
        areas: normalized,
        inspected_files: inspected_files.to_vec(),
        searches: searches_from_notebook(completed_searches),
        unresolved_questions: Vec::new(),
    };
    let errors = validate(&map, criteria.len());
    if errors.is_empty() {
        Ok((map, ArtifactSource::NormalizedModel))
    } else {
        Err(errors)
    }
}

pub fn fallback(
    inspected_files: &[String],
    completed_searches: &[String],
    criteria: &[String],
    unresolved: &[String],
) -> Option<(ImpactMap, f64)> {
    if inspected_files.is_empty() || criteria.is_empty() || !unresolved.is_empty() {
        return None;
    }
    let paths = inspected_files.to_vec();
    let evidence = evidence_catalog(inspected_files, completed_searches)
        .into_iter()
        .map(|item| ImpactEvidence {
            evidence_type: item.evidence_type,
            path: item.path,
            query: item.query,
            description: item.description,
        })
        .collect();
    Some((
        ImpactMap {
            schema_version: IMPACT_MAP_SCHEMA_VERSION.into(),
            areas: vec![ImpactArea {
                area_id: stable_area_id("repository impact", &paths),
                name: "Repository impact".into(),
                candidate_paths: paths,
                evidence,
                acceptance_criteria_ids: (0..criteria.len()).map(criterion_id).collect(),
                reason:
                    "The orchestrator derived this bounded map from completed repository discovery."
                        .into(),
            }],
            inspected_files: inspected_files.to_vec(),
            searches: searches_from_notebook(completed_searches),
            unresolved_questions: Vec::new(),
        },
        0.85,
    ))
}

pub fn fallback_from_persisted_evidence(
    inspected_files: &[String],
    completed_searches: &[String],
    criteria: &[String],
    unresolved: &[String],
    architecture_findings: &[String],
) -> Option<(ImpactMap, f64)> {
    let (mut map, mut confidence) =
        fallback(inspected_files, completed_searches, criteria, unresolved)?;
    if let Some(area) = map.areas.first_mut()
        && !architecture_findings.is_empty()
    {
        area.reason = format!(
            "The orchestrator derived this conservative map from {} persisted file observation(s), {} search result(s), and {} architecture finding(s).",
            inspected_files.len(),
            completed_searches.len(),
            architecture_findings.len(),
        );
        confidence = 0.9;
    }
    Some((map, confidence))
}

fn required(path: &str) -> ValidationError {
    ValidationError {
        path: path.into(),
        keyword: "required".into(),
        message: "Required property is missing.".into(),
    }
}
fn min_items(path: &str, message: &str) -> ValidationError {
    ValidationError {
        path: path.into(),
        keyword: "minItems".into(),
        message: message.into(),
    }
}

pub fn validate(map: &ImpactMap, criterion_count: usize) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    if map.schema_version != IMPACT_MAP_SCHEMA_VERSION {
        errors.push(ValidationError {
            path: "$.schema_version".into(),
            keyword: "const".into(),
            message: format!("Expected `{IMPACT_MAP_SCHEMA_VERSION}`."),
        });
    }
    if map.areas.is_empty() {
        errors.push(min_items(
            "$.areas",
            "At least one impact area is required.",
        ));
    }
    if map.inspected_files.is_empty() {
        errors.push(min_items(
            "$.inspected_files",
            "At least one inspected file is required.",
        ));
    }
    let valid_ids = (0..criterion_count)
        .map(criterion_id)
        .collect::<BTreeSet<_>>();
    for (i, area) in map.areas.iter().enumerate() {
        let base = format!("$.areas[{i}]");
        if area.area_id.trim().is_empty() {
            errors.push(required(&format!("{base}.area_id")));
        }
        if area.name.trim().is_empty() {
            errors.push(required(&format!("{base}.name")));
        }
        if area.candidate_paths.is_empty() {
            errors.push(min_items(
                &format!("{base}.candidate_paths"),
                "At least one candidate path is required.",
            ));
        }
        if area.evidence.is_empty() {
            errors.push(required(&format!("{base}.evidence")));
        }
        if area.acceptance_criteria_ids.is_empty() {
            errors.push(min_items(
                &format!("{base}.acceptance_criteria_ids"),
                "At least one acceptance criterion ID is required.",
            ));
        }
        for id in &area.acceptance_criteria_ids {
            if !valid_ids.contains(id) {
                errors.push(ValidationError {
                    path: format!("{base}.acceptance_criteria_ids"),
                    keyword: "reference".into(),
                    message: format!("Unknown acceptance criterion ID `{id}`."),
                });
            }
        }
        if area.reason.trim().is_empty() {
            errors.push(required(&format!("{base}.reason")));
        }
    }
    errors
}

pub fn safe_shape(value: &Value) -> InvalidPayloadShape {
    let top = value
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    let areas = value
        .as_array()
        .or_else(|| value.get("areas").and_then(Value::as_array))
        .or_else(|| value.get("impact_map").and_then(Value::as_array));
    let required = [
        "schema_version",
        "areas",
        "inspected_files",
        "searches",
        "unresolved_questions",
    ];
    let omitted = if let Some(object) = value.as_object() {
        required
            .iter()
            .filter(|key| !object.contains_key(**key))
            .map(|s| s.to_string())
            .collect()
    } else {
        required.iter().map(|s| s.to_string()).collect()
    };
    let known = required.into_iter().collect::<BTreeSet<_>>();
    let unknown = value
        .as_object()
        .into_iter()
        .flat_map(|o| o.keys())
        .filter(|key| !known.contains(key.as_str()))
        .cloned()
        .collect();
    let fields = [
        "area_id",
        "name",
        "candidate_paths",
        "evidence",
        "evidence_refs",
        "acceptance_criteria_ids",
        "reason",
    ];
    let matrix = areas
        .into_iter()
        .flatten()
        .map(|area| {
            fields
                .iter()
                .map(|field| ((*field).into(), area.get(*field).is_some()))
                .collect()
        })
        .collect();
    InvalidPayloadShape {
        top_level_keys: top,
        area_count: areas.map_or(0, Vec::len),
        area_field_presence: matrix,
        omitted_required_fields: omitted,
        unknown_fields: unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notebook() -> (Vec<String>, Vec<String>, Vec<String>) {
        (
            vec!["src/theme.rs".into()],
            vec!["light-blue".into()],
            vec!["Theme persists".into(), "Views use tokens".into()],
        )
    }

    fn compact() -> Value {
        json!({"areas":[{"name":"Theme provider","candidate_paths":["src/theme.rs"],"evidence_refs":["read-1","search-1"],"acceptance_criteria_ids":["ac-1"],"reason":"Controls theme state"}]})
    }

    #[test]
    fn canonical_schema_and_provider_projection_share_version() {
        assert_eq!(
            schema()["properties"]["schema_version"]["const"],
            IMPACT_MAP_SCHEMA_VERSION
        );
        assert_eq!(schema_sha256().len(), 64);
        assert_eq!(provider_tool_schema()["required"], json!(["areas"]));
    }
    #[test]
    fn legacy_array_normalizes_to_v2() {
        let (f, s, c) = notebook();
        let (map,source)=normalize(&json!([{"area":"Theme provider","candidate_paths":["src/theme.rs"],"reason":"Controls theme state","acceptance_criteria":["Theme persists"]}]),&f,&s,&c).unwrap();
        assert_eq!(source, ArtifactSource::NormalizedModel);
        assert_eq!(map.schema_version, IMPACT_MAP_SCHEMA_VERSION);
    }
    #[test]
    fn notebook_inspected_files_are_injected() {
        let (f, s, c) = notebook();
        assert_eq!(
            normalize(&compact(), &f, &s, &c).unwrap().0.inspected_files,
            f
        );
    }
    #[test]
    fn notebook_searches_are_injected() {
        let (f, s, c) = notebook();
        assert_eq!(
            normalize(&compact(), &f, &s, &c).unwrap().0.searches[0].query,
            s[0]
        );
    }
    #[test]
    fn criterion_text_maps_to_stable_id() {
        let (f, s, c) = notebook();
        let value = json!([{"area":"Theme","candidate_paths":["src/theme.rs"],"reason":"state","acceptance_criteria":["Views use tokens"]}]);
        assert_eq!(
            normalize(&value, &f, &s, &c).unwrap().0.areas[0].acceptance_criteria_ids,
            vec!["ac-2"]
        );
    }
    #[test]
    fn missing_derivable_wrapper_needs_no_repair() {
        let (f, s, c) = notebook();
        assert!(normalize(&compact(), &f, &s, &c).is_ok());
    }
    #[test]
    fn precise_json_path_errors_are_returned() {
        let map = ImpactMap {
            schema_version: IMPACT_MAP_SCHEMA_VERSION.into(),
            areas: vec![],
            inspected_files: vec![],
            searches: vec![],
            unresolved_questions: vec![],
        };
        let errors = validate(&map, 1);
        assert!(
            errors
                .iter()
                .any(|e| e.path == "$.areas" && e.keyword == "minItems")
        );
        assert!(errors.iter().any(|e| e.path == "$.inspected_files"));
    }
    #[test]
    fn evidence_references_resolve() {
        let (f, s, c) = notebook();
        let map = normalize(&compact(), &f, &s, &c).unwrap().0;
        assert_eq!(map.areas[0].evidence.len(), 2);
        assert_eq!(
            map.areas[0].evidence[0].evidence_type,
            EvidenceType::FileRead
        );
    }
    #[test]
    fn path_evidence_references_normalize_to_file_read_evidence() {
        let (f, s, c) = notebook();
        for (reference, expected_range) in [
            ("src/theme.rs", None),
            ("src/theme.rs:4", Some("4")),
            ("src/theme.rs:4-12", Some("4-12")),
        ] {
            let mut value = compact();
            value["areas"][0]["evidence_refs"] = json!([reference]);

            let (map, source) = normalize(&value, &f, &s, &c).unwrap();
            assert_eq!(source, ArtifactSource::NormalizedModel);
            assert_eq!(map.areas[0].evidence.len(), 1);
            let evidence = &map.areas[0].evidence[0];
            assert_eq!(evidence.evidence_type, EvidenceType::FileRead);
            assert_eq!(evidence.path.as_deref(), Some("src/theme.rs"));
            match expected_range {
                Some(range) => assert!(
                    evidence
                        .description
                        .contains(&format!("Referenced lines {range}.")),
                    "missing range in normalized evidence for {reference}"
                ),
                None => assert!(!evidence.description.contains("Referenced lines")),
            }
        }
    }
    #[test]
    fn unknown_evidence_reference_fails_clearly() {
        let (f, s, c) = notebook();
        let mut value = compact();
        value["areas"][0]["evidence_refs"] = json!(["read-999"]);
        let errors = normalize(&value, &f, &s, &c).unwrap_err();
        assert!(errors[0].message.contains("read-999"));
    }
    #[test]
    fn semantically_sufficient_legacy_output_continues() {
        let (f, s, c) = notebook();
        let value = json!({"impact_map":[{"area":"Theme","candidate_paths":["src/theme.rs"],"reason":"state","acceptance_criteria":["Theme persists"]}]});
        assert!(normalize(&value, &f, &s, &c).is_ok());
    }
    #[test]
    fn deterministic_fallback_builds_valid_map() {
        let (f, s, c) = notebook();
        let (map, confidence) = fallback(&f, &s, &c, &[]).unwrap();
        assert!(confidence >= 0.8);
        assert!(validate(&map, c.len()).is_empty());
    }
    #[test]
    fn persisted_evidence_fallback_is_conservative_and_covers_attempt_25_paths() {
        let paths = vec![
            "src/components/theme/ThemeProvider.tsx".into(),
            "src/components/theme/ThemeToggle.tsx".into(),
            "src/styles/globals.css".into(),
            "tests/theme-provider.test.tsx".into(),
        ];
        let criteria = vec!["Theme selection persists and remains covered by tests".into()];
        let (map, confidence) = fallback_from_persisted_evidence(
            &paths,
            &["ThemeProvider".into()],
            &criteria,
            &[],
            &["Theme state is centralized in the provider".into()],
        )
        .unwrap();
        assert!(confidence >= 0.8);
        assert!(validate(&map, criteria.len()).is_empty());
        assert_eq!(map.areas[0].candidate_paths, paths);
        assert_eq!(map.inspected_files, paths);
    }
    #[test]
    fn genuinely_empty_discovery_blocks_fallback() {
        assert!(fallback(&[], &[], &["criterion".into()], &[]).is_none());
    }
    #[test]
    fn safe_shape_never_contains_repository_values() {
        let shape = safe_shape(&json!({"areas":[{"name":"SECRET","extra":"CONTENT"}]}));
        let text = serde_json::to_string(&shape).unwrap();
        assert!(!text.contains("SECRET"));
        assert!(!text.contains("CONTENT"));
    }
    #[test]
    fn canonical_unknown_fields_are_rejected() {
        let (f, s, c) = notebook();
        let mut value = serde_json::to_value(normalize(&compact(), &f, &s, &c).unwrap().0).unwrap();
        value["extra"] = json!(true);
        assert!(
            normalize(&value, &f, &s, &c)
                .unwrap_err()
                .iter()
                .any(|e| e.keyword == "additionalProperties")
        );
    }
}
