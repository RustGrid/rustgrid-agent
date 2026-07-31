// Generated from contracts/impact-map-v2.schema.json. Do not edit by hand.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactMap {
    pub schema_version: String,
    pub areas: Vec<ImpactArea>,
    pub inspected_files: Vec<String>,
    pub searches: Vec<ImpactSearch>,
    pub unresolved_questions: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactArea {
    pub area_id: String,
    pub name: String,
    pub candidate_paths: Vec<String>,
    pub evidence: Vec<ImpactEvidence>,
    pub acceptance_criteria_ids: Vec<String>,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactSearch {
    pub query: String,
    pub scope: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactEvidence {
    #[serde(rename = "type")]
    pub evidence_type: EvidenceType,
    pub path: Option<String>,
    pub query: Option<String>,
    pub description: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceType {
    FileRead,
    SearchMatch,
    RepositoryStructure,
    TestReference,
    Inference,
}
