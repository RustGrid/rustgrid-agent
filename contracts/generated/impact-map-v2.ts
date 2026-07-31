// Generated from contracts/impact-map-v2.schema.json. Do not edit by hand.
export const IMPACT_MAP_SCHEMA_VERSION = "rustgrid.impact_map.v2" as const;
export type ImpactEvidenceType = "file_read" | "search_match" | "repository_structure" | "test_reference" | "inference";
export interface ImpactEvidence { type: ImpactEvidenceType; path: string | null; query: string | null; description: string; }
export interface ImpactArea { area_id: string; name: string; candidate_paths: string[]; evidence: ImpactEvidence[]; acceptance_criteria_ids: string[]; reason: string; }
export interface ImpactMapV2 { schema_version: typeof IMPACT_MAP_SCHEMA_VERSION; areas: ImpactArea[]; inspected_files: string[]; searches: Array<{ query: string; scope: string | null }>; unresolved_questions: string[]; }
