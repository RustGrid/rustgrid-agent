import { createHash } from "node:crypto";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { resolve } from "node:path";

const root = resolve(new URL("..", import.meta.url).pathname);
const schemaPath = resolve(root, "contracts/impact-map-v2.schema.json");
const schemaText = readFileSync(schemaPath, "utf8");
const schema = JSON.parse(schemaText);
const version = schema.properties?.schema_version?.const;
if (version !== "rustgrid.impact_map.v2") throw new Error(`unexpected schema version: ${version}`);
const hash = createHash("sha256").update(schemaText).digest("hex");
const evidenceTypes = schema.properties.areas.items.properties.evidence.items.properties.type.enum;
const pascal = (value) => value.split("_").map((part) => part[0].toUpperCase() + part.slice(1)).join("");

const rustTypes = `// Generated from contracts/impact-map-v2.schema.json. Do not edit by hand.\nuse serde::{Deserialize, Serialize};\n\n#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]\n#[serde(deny_unknown_fields)]\npub struct ImpactMap {\n    pub schema_version: String,\n    pub areas: Vec<ImpactArea>,\n    pub inspected_files: Vec<String>,\n    pub searches: Vec<ImpactSearch>,\n    pub unresolved_questions: Vec<String>,\n}\n\n#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]\n#[serde(deny_unknown_fields)]\npub struct ImpactArea {\n    pub area_id: String,\n    pub name: String,\n    pub candidate_paths: Vec<String>,\n    pub evidence: Vec<ImpactEvidence>,\n    pub acceptance_criteria_ids: Vec<String>,\n    pub reason: String,\n}\n\n#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]\n#[serde(deny_unknown_fields)]\npub struct ImpactSearch {\n    pub query: String,\n    pub scope: Option<String>,\n}\n\n#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]\n#[serde(deny_unknown_fields)]\npub struct ImpactEvidence {\n    #[serde(rename = "type")]\n    pub evidence_type: EvidenceType,\n    pub path: Option<String>,\n    pub query: Option<String>,\n    pub description: String,\n}\n\n#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]\n#[serde(rename_all = "snake_case")]\npub enum EvidenceType {\n${evidenceTypes.map((value) => `    ${pascal(value)},`).join("\n")}\n}\n`;

const tsTypes = `// Generated from contracts/impact-map-v2.schema.json. Do not edit by hand.\nexport const IMPACT_MAP_SCHEMA_VERSION = "${version}" as const;\nexport type ImpactEvidenceType = ${evidenceTypes.map((value) => JSON.stringify(value)).join(" | ")};\nexport interface ImpactEvidence { type: ImpactEvidenceType; path: string | null; query: string | null; description: string; }\nexport interface ImpactArea { area_id: string; name: string; candidate_paths: string[]; evidence: ImpactEvidence[]; acceptance_criteria_ids: string[]; reason: string; }\nexport interface ImpactMapV2 { schema_version: typeof IMPACT_MAP_SCHEMA_VERSION; areas: ImpactArea[]; inspected_files: string[]; searches: Array<{ query: string; scope: string | null }>; unresolved_questions: string[]; }\n`;
const serverTemplate = readFileSync(resolve(root, "contracts/templates/impact-map-contract.rs.tpl"), "utf8");
const serverContract = serverTemplate
  .replaceAll("{{SCHEMA_VERSION}}", version)
  .replaceAll("{{SCHEMA_SHA256}}", hash)
  .replaceAll(
    "{{EVIDENCE_PATTERN}}",
    evidenceTypes.map((value, index) => `${index ? "| " : ""}${JSON.stringify(value)}`).join("\n                        "),
  );
const serverHash = createHash("sha256").update(serverContract).digest("hex");

const outputs = [
  [resolve(root, "src/hosted/impact_map_types_generated.rs"), rustTypes],
  [resolve(root, "contracts/generated/impact-map-v2.ts"), tsTypes],
  [resolve(root, "contracts/generated/impact-map-contract.rs"), serverContract],
  [resolve(root, "contracts/generated/impact-map-validator.sha256"), `${serverHash}\n`],
];

function output(path, content, write) {
  if (write) { mkdirSync(resolve(path, ".."), { recursive: true }); writeFileSync(path, content); return; }
  if (readFileSync(path, "utf8") !== content) throw new Error(`generated contract drifted: ${path}`);
}

const write = process.argv.includes("--write");
for (const [path, content] of outputs) output(path, content, write);

if (process.argv.includes("--sync-siblings")) {
  const rustgrid = resolve(root, "../rustgrid");
  const agentops = resolve(root, "../rustgrid-agentops");
  output(resolve(rustgrid, "contracts/impact-map-v2.schema.json"), schemaText, true);
  output(resolve(rustgrid, "crates/ticket-http/src/impact_map_contract.rs"), serverContract, true);
  output(resolve(rustgrid, "contracts/impact-map-validator.sha256"), `${serverHash}\n`, true);
  output(resolve(agentops, "contracts/impact-map-v2.schema.json"), schemaText, true);
  output(resolve(agentops, "src/domain/impact-map-v2.ts"), tsTypes, true);
}

process.stdout.write(`${version} ${hash}\n`);
