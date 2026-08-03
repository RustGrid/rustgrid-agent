// Extracted from the hosted execution composition root.
use super::*;

pub(super) fn provider_request_metadata(
    execution_id: Uuid,
    ticket_key: &str,
    agent: &str,
    phase: ExecutionPhase,
    model_call_budget: i32,
) -> Value {
    json!({
        "execution_id": execution_id.to_string(),
        "ticket_key": ticket_key,
        "agent": agent,
        "phase": phase.as_str(),
        "model_call_budget": model_call_budget.to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn provider_rejected_event(
    failure: &HostedHttpError,
    registration: &AiCallRegistration,
    execution_attempt: i32,
    model_call: usize,
    configured_model: &str,
    model_calls_used: usize,
    budget: Value,
    notebook: Value,
) -> Value {
    json!({
        "event_type": "execution.ai.provider_rejected",
        "semantic_call_id": registration.semantic_call_id,
        "call_index": model_call.saturating_sub(1),
        "execution_attempt": execution_attempt,
        "failure_stage": "provider_dispatch",
        "rustgrid_gateway_status": failure.rustgrid_gateway_status(),
        "upstream_provider_status": failure.upstream_provider_status,
        "provider_contacted": true,
        "rustgrid_request_id": failure.rustgrid_request_id.as_deref(),
        "transport_request_id": failure.transport_request_id.as_deref(),
        "provider_request_id": failure.provider_request_id.as_deref(),
        "reservation_state": failure.reservation_state(),
        "reservation_reconciliation_state": failure.reservation_reconciliation_state(),
        "provider_error": failure.provider_error.as_ref(),
        "provider_response_body": failure.provider_response_body.as_ref(),
        "provider_error_code": failure
            .provider_error
            .as_ref()
            .and_then(|provider| provider.code.as_deref()),
        "provider_error_parameter": failure
            .provider_error
            .as_ref()
            .and_then(|provider| provider.parameter.as_deref()),
        "model_alias": failure.model_alias.as_deref().unwrap_or(configured_model),
        "resolved_provider_model": failure.resolved_provider_model.as_deref(),
        "adapter_version": failure.adapter_version.as_deref(),
        "payload_schema_version": failure.payload_schema_version.as_deref(),
        "provider_attempts": failure.provider_attempts.unwrap_or(1),
        "model_calls_used": model_calls_used,
        "call_budget_consumed": false,
        "actual_cost_micros": failure.actual_cost_micros.unwrap_or(0),
        "retryable": false,
        "message": failure.terminal_message(),
        "budget": budget,
        "notebook": notebook,
    })
}

pub(super) fn validate_provider_request_envelope(request: &Value) -> Result<()> {
    const ALLOWED_FIELDS: &[&str] = &[
        "model",
        "input",
        "instructions",
        "max_output_tokens",
        "reasoning",
        "text",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "temperature",
        "top_p",
        "metadata",
        "store",
        "stream",
    ];
    let object = request
        .as_object()
        .ok_or_else(|| anyhow!("ai_provider_request_invalid: request must be an object"))?;
    if let Some(field) = object
        .keys()
        .find(|field| !ALLOWED_FIELDS.contains(&field.as_str()))
    {
        bail!("ai_provider_request_invalid: unsupported request field `{field}`");
    }
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty() && model.len() <= 200)
        .ok_or_else(|| anyhow!("ai_provider_request_invalid: model must be a bounded string"))?;
    if !safe_identifier(model, 200) {
        bail!("ai_provider_request_invalid: model contains unsupported characters");
    }
    if request.get("input").is_none() {
        bail!("ai_provider_request_invalid: input is required");
    }
    if request
        .get("max_output_tokens")
        .is_none_or(|value| value.as_i64().is_none_or(|value| value <= 0))
    {
        bail!("ai_provider_request_invalid: max_output_tokens must be a positive integer");
    }
    if request
        .get("store")
        .is_some_and(|value| value != &json!(false))
    {
        bail!("ai_provider_request_invalid: provider-side storage is not allowed");
    }
    if request
        .get("stream")
        .is_some_and(|value| !value.is_boolean())
    {
        bail!("ai_provider_request_invalid: stream must be boolean");
    }
    if request
        .get("parallel_tool_calls")
        .is_some_and(|value| !value.is_boolean())
    {
        bail!("ai_provider_request_invalid: parallel_tool_calls must be boolean");
    }
    if let Some(reasoning) = request.get("reasoning") {
        let reasoning = reasoning
            .as_object()
            .ok_or_else(|| anyhow!("ai_provider_request_invalid: reasoning must be an object"))?;
        if reasoning.keys().any(|key| key != "effort")
            || reasoning.get("effort").is_some_and(|effort| {
                !matches!(
                    effort.as_str(),
                    Some("none" | "low" | "medium" | "high" | "xhigh" | "max")
                )
            })
        {
            bail!("ai_provider_request_invalid: reasoning configuration is unsupported");
        }
    }
    if let Some(tools) = request.get("tools") {
        validate_provider_tool_definitions(tools)?;
    }
    if let Some(tool_choice) = request.get("tool_choice") {
        validate_provider_tool_choice(tool_choice, request.get("tools"))?;
    }
    if let Some(text) = request.get("text") {
        validate_provider_text_configuration(text)?;
    }
    if let Some(metadata) = request.get("metadata") {
        let metadata = metadata
            .as_object()
            .ok_or_else(|| anyhow!("ai_provider_request_invalid: metadata must be an object"))?;
        if metadata.len() > 16 {
            bail!("ai_provider_request_invalid: metadata cannot contain more than 16 entries");
        }
        for (key, value) in metadata {
            if key.is_empty() || key.len() > 64 || key.chars().any(char::is_control) {
                bail!("ai_provider_request_invalid: metadata keys must contain 1 to 64 safe bytes");
            }
            let value = value.as_str().ok_or_else(|| {
                anyhow!("ai_provider_request_invalid: metadata value `{key}` must be a string")
            })?;
            if value.len() > 512 {
                bail!(
                    "ai_provider_request_invalid: metadata value `{key}` cannot exceed 512 bytes"
                );
            }
        }
    }
    Ok(())
}

pub(super) fn validate_hosted_provider_startup_contract(manifest: &HostedManifest) -> Result<()> {
    let request = json!({
        "model": manifest.ai_gateway.model,
        "input": [{"role": "user", "content": "startup contract validation"}],
        "max_output_tokens": manifest.ai_gateway.maximum_output_tokens.min(16_384),
        "reasoning": {"effort": "medium"},
        "tools": hosted_tools(),
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "metadata": provider_request_metadata(
            manifest.execution.execution_id,
            manifest.ticket_key.as_str(),
            "rustgrid-agent-hosted",
            ExecutionPhase::Discovery,
            manifest
                .model_call_budget
                .unwrap_or(DEFAULT_HOSTED_MODEL_CALLS as i32),
        ),
        "store": false,
        "stream": false,
    });
    validate_provider_request_envelope(&request)
}

pub(super) fn validate_provider_tool_definitions(tools: &Value) -> Result<()> {
    let tools = tools
        .as_array()
        .ok_or_else(|| anyhow!("ai_tool_schema_invalid: tools must be an array"))?;
    if tools.len() > 64 {
        bail!("ai_tool_schema_invalid: tools cannot contain more than 64 functions");
    }
    let mut names = BTreeSet::new();
    for (index, tool) in tools.iter().enumerate() {
        let path = format!("tools[{index}]");
        let object = tool
            .as_object()
            .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path} must be an object"))?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "type" | "name" | "description" | "parameters" | "strict"
            )
        }) || object.get("type").and_then(Value::as_str) != Some("function")
        {
            bail!("ai_tool_schema_invalid: {path} has an unsupported function shape");
        }
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            })
            .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path}.name is invalid"))?;
        if !names.insert(name.to_owned()) {
            bail!("ai_tool_schema_invalid: duplicate tool name `{name}`");
        }
        if object
            .get("description")
            .is_some_and(|value| value.as_str().is_none_or(|value| value.len() > 8 * 1024))
        {
            bail!("ai_tool_schema_invalid: {path}.description is invalid");
        }
        let strict = match object.get("strict") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => bail!("ai_tool_schema_invalid: {path}.strict must be boolean"),
        };
        let parameters = object
            .get("parameters")
            .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path}.parameters is required"))?;
        validate_provider_json_schema(parameters, &format!("{path}.parameters"), 0, strict, true)?;
    }
    Ok(())
}

pub(super) fn validate_provider_tool_choice(
    tool_choice: &Value,
    tools: Option<&Value>,
) -> Result<()> {
    if tool_choice
        .as_str()
        .is_some_and(|choice| matches!(choice, "auto" | "none" | "required"))
    {
        return Ok(());
    }
    let choice = tool_choice.as_object().ok_or_else(|| {
        anyhow!("ai_provider_request_invalid: tool_choice must be a supported string or object")
    })?;
    if choice.len() != 2
        || choice.get("type").and_then(Value::as_str) != Some("function")
        || choice.get("name").and_then(Value::as_str).is_none()
    {
        bail!("ai_provider_request_invalid: forced tool_choice must identify one function");
    }
    let selected = choice["name"].as_str().unwrap_or_default();
    let declared = tools.and_then(Value::as_array).is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some(selected))
    });
    if !declared {
        bail!("ai_provider_request_invalid: forced tool_choice is not declared");
    }
    Ok(())
}

pub(super) fn validate_provider_text_configuration(text: &Value) -> Result<()> {
    let text = text
        .as_object()
        .ok_or_else(|| anyhow!("ai_response_schema_invalid: text must be an object"))?;
    if text
        .keys()
        .any(|key| !matches!(key.as_str(), "format" | "verbosity"))
        || text
            .get("verbosity")
            .is_some_and(|value| !matches!(value.as_str(), Some("low" | "medium" | "high")))
    {
        bail!("ai_response_schema_invalid: text configuration is unsupported");
    }
    let Some(format) = text.get("format") else {
        return Ok(());
    };
    let format = format
        .as_object()
        .ok_or_else(|| anyhow!("ai_response_schema_invalid: text.format must be an object"))?;
    match format.get("type").and_then(Value::as_str) {
        Some("text") if format.len() == 1 => Ok(()),
        Some("json_schema") => {
            if format.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "type" | "name" | "description" | "schema" | "strict"
                )
            }) {
                bail!("ai_response_schema_invalid: text.format contains an unsupported field");
            }
            format
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| {
                    !name.is_empty()
                        && name.len() <= 64
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                })
                .ok_or_else(|| {
                    anyhow!("ai_response_schema_invalid: text.format.name is invalid")
                })?;
            let strict = match format.get("strict") {
                None => false,
                Some(Value::Bool(value)) => *value,
                Some(_) => {
                    bail!("ai_response_schema_invalid: text.format.strict must be boolean")
                }
            };
            let schema = format.get("schema").ok_or_else(|| {
                anyhow!("ai_response_schema_invalid: text.format.schema is required")
            })?;
            validate_provider_json_schema(schema, "text.format.schema", 0, strict, true)
                .map_err(|error| anyhow!("ai_response_schema_invalid: {error}"))
        }
        _ => bail!("ai_response_schema_invalid: text.format.type is unsupported"),
    }
}

pub(super) fn validate_provider_json_schema(
    schema: &Value,
    path: &str,
    depth: usize,
    strict: bool,
    require_object: bool,
) -> Result<()> {
    const MAX_DEPTH: usize = 10;
    const ALLOWED_KEYWORDS: &[&str] = &[
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "description",
        "minimum",
        "maximum",
        "minItems",
        "maxItems",
    ];
    if depth >= MAX_DEPTH {
        bail!("ai_tool_schema_invalid: {path} exceeds the supported nesting depth");
    }
    let object = schema
        .as_object()
        .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path} must be an object"))?;
    if let Some(keyword) = object
        .keys()
        .find(|keyword| !ALLOWED_KEYWORDS.contains(&keyword.as_str()))
    {
        bail!("ai_tool_schema_invalid: {path}.{keyword} is unsupported");
    }
    let schema_type = provider_schema_type(object.get("type"), path)?;
    if require_object && schema_type.as_deref() != Some("object") {
        bail!("ai_tool_schema_invalid: {path}.type must be object");
    }
    if object
        .get("description")
        .is_some_and(|value| !value.is_string())
    {
        bail!("ai_tool_schema_invalid: {path}.description must be a string");
    }
    if let Some(values) = object.get("enum") {
        let values = values
            .as_array()
            .filter(|values| !values.is_empty())
            .ok_or_else(|| {
                anyhow!("ai_tool_schema_invalid: {path}.enum must be a non-empty array")
            })?;
        let unique = values.iter().map(Value::to_string).collect::<BTreeSet<_>>();
        if unique.len() != values.len() {
            bail!("ai_tool_schema_invalid: {path}.enum contains duplicate values");
        }
        if values
            .iter()
            .any(|value| !provider_schema_type_accepts(object.get("type"), value))
        {
            bail!("ai_tool_schema_invalid: {path}.enum contains a value outside its declared type");
        }
    }
    let has_numeric_bounds = object.contains_key("minimum") || object.contains_key("maximum");
    if has_numeric_bounds && !matches!(schema_type.as_deref(), Some("integer" | "number")) {
        bail!("ai_tool_schema_invalid: {path} uses numeric bounds without a numeric type");
    }
    for keyword in ["minimum", "maximum"] {
        if object.get(keyword).is_some_and(|value| !value.is_number()) {
            bail!("ai_tool_schema_invalid: {path}.{keyword} must be numeric");
        }
    }
    for keyword in ["minItems", "maxItems"] {
        if object
            .get(keyword)
            .is_some_and(|value| value.as_u64().is_none())
        {
            bail!("ai_tool_schema_invalid: {path}.{keyword} must be non-negative");
        }
    }
    if object
        .get("minimum")
        .and_then(Value::as_f64)
        .zip(object.get("maximum").and_then(Value::as_f64))
        .is_some_and(|(minimum, maximum)| minimum > maximum)
        || object
            .get("minItems")
            .and_then(Value::as_u64)
            .zip(object.get("maxItems").and_then(Value::as_u64))
            .is_some_and(|(minimum, maximum)| minimum > maximum)
    {
        bail!("ai_tool_schema_invalid: {path} has inverted bounds");
    }

    if schema_type.as_deref() == Some("object") {
        let empty_properties = serde_json::Map::new();
        let properties = match object.get("properties") {
            Some(Value::Object(properties)) => properties,
            Some(_) => bail!("ai_tool_schema_invalid: {path}.properties must be an object"),
            None => &empty_properties,
        };
        if object
            .get("additionalProperties")
            .is_some_and(|value| value != &Value::Bool(false))
        {
            bail!("ai_tool_schema_invalid: {path}.additionalProperties must be false");
        }
        if strict && object.get("additionalProperties") != Some(&Value::Bool(false)) {
            bail!(
                "ai_tool_schema_invalid: {path}.additionalProperties is required for strict schemas"
            );
        }
        let empty_required = Vec::new();
        let required = match object.get("required") {
            Some(Value::Array(required)) => required,
            Some(_) => bail!("ai_tool_schema_invalid: {path}.required must be an array"),
            None => &empty_required,
        };
        let mut required_names = BTreeSet::new();
        for (index, required) in required.iter().enumerate() {
            let required = required.as_str().ok_or_else(|| {
                anyhow!("ai_tool_schema_invalid: {path}.required[{index}] must be a string")
            })?;
            if !properties.contains_key(required) || !required_names.insert(required) {
                bail!(
                    "ai_tool_schema_invalid: {path}.required[{index}] must name one property once"
                );
            }
        }
        if strict && required_names.len() != properties.len() {
            bail!("ai_tool_schema_invalid: {path}.required must include every strict property");
        }
        for (name, property) in properties {
            if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
                bail!("ai_tool_schema_invalid: {path}.properties has an invalid name");
            }
            validate_provider_json_schema(
                property,
                &format!("{path}.properties.{name}"),
                depth + 1,
                strict,
                false,
            )?;
        }
    } else if object.contains_key("properties")
        || object.contains_key("required")
        || object.contains_key("additionalProperties")
    {
        bail!("ai_tool_schema_invalid: {path} uses object keywords without object type");
    }

    if schema_type.as_deref() == Some("array") {
        let items = object
            .get("items")
            .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path}.items is required"))?;
        validate_provider_json_schema(items, &format!("{path}.items"), depth + 1, strict, false)?;
    } else if object.contains_key("items")
        || object.contains_key("minItems")
        || object.contains_key("maxItems")
    {
        bail!("ai_tool_schema_invalid: {path} uses array keywords without array type");
    }
    Ok(())
}

pub(super) fn provider_schema_type(value: Option<&Value>, path: &str) -> Result<Option<String>> {
    let supported = |value: &str| {
        matches!(
            value,
            "object" | "array" | "string" | "integer" | "number" | "boolean" | "null"
        )
    };
    let Some(value) = value else {
        return Ok(None);
    };
    if let Some(value) = value.as_str() {
        if supported(value) {
            return Ok(Some(value.to_owned()));
        }
        bail!("ai_tool_schema_invalid: {path}.type is unsupported");
    }
    let values = value
        .as_array()
        .filter(|values| values.len() == 2)
        .ok_or_else(|| {
            anyhow!("ai_tool_schema_invalid: {path}.type nullable union must contain two types")
        })?;
    let first = values[0]
        .as_str()
        .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path}.type must contain strings"))?;
    let second = values[1]
        .as_str()
        .ok_or_else(|| anyhow!("ai_tool_schema_invalid: {path}.type must contain strings"))?;
    if first == second
        || !supported(first)
        || !supported(second)
        || !matches!((first, second), ("null", _) | (_, "null"))
    {
        bail!("ai_tool_schema_invalid: {path}.type nullable union is unsupported");
    }
    Ok(Some(
        if first == "null" { second } else { first }.to_owned(),
    ))
}

pub(super) fn provider_schema_type_accepts(schema_type: Option<&Value>, value: &Value) -> bool {
    let accepts = |schema_type: &str| match schema_type {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    };
    match schema_type {
        None => true,
        Some(Value::String(schema_type)) => accepts(schema_type),
        Some(Value::Array(schema_types)) => {
            schema_types.iter().filter_map(Value::as_str).any(accepts)
        }
        Some(_) => false,
    }
}

pub(super) fn fit_request_to_input_ceiling(
    request: &mut Value,
    initial: &Value,
    turns: &mut VecDeque<Vec<Value>>,
    maximum_input: usize,
) -> Result<()> {
    while serde_json::to_vec(&request)?.len() > maximum_input && !turns.is_empty() {
        turns.pop_front();
        let mut reduced = vec![initial.clone()];
        for turn in turns.iter() {
            reduced.extend(turn.iter().cloned());
        }
        request["input"] = Value::Array(reduced);
    }
    if serde_json::to_vec(&request)?.len() > maximum_input {
        bail!("hosted agent context exceeds the execution input-token ceiling");
    }
    Ok(())
}

pub(super) fn phase_request_input_ceiling(phase: ExecutionPhase, signed_maximum: usize) -> usize {
    if matches!(
        phase,
        ExecutionPhase::Discovery | ExecutionPhase::ArtifactRepair
    ) {
        signed_maximum.min(MAX_DISCOVERY_REQUEST_BYTES)
    } else {
        signed_maximum
    }
}

pub(super) fn hosted_budget_advisory(
    used: usize,
    limit: usize,
) -> Option<(u8, &'static str, &'static str)> {
    let percent = used.saturating_mul(100) / limit.max(1);
    if percent >= 90 {
        Some((
            90,
            "execution_budget_finalization",
            "The signed execution budget is at least 90% consumed. Stop broad exploration, continue the current implementation, and produce the smallest complete validated result.",
        ))
    } else if percent >= 70 {
        Some((
            70,
            "execution_budget_constrained",
            "The signed execution budget is at least 70% consumed. Continue from the notebook and existing diff, avoid repeated reads, and prioritize remaining acceptance criteria.",
        ))
    } else {
        None
    }
}

pub(super) fn compact_hosted_turns(turns: &mut VecDeque<Vec<Value>>) {
    while turns.len() > MAX_HOSTED_TURN_WINDOWS {
        turns.pop_front();
    }
}

pub(super) fn compact_notebook_for_phase(
    notebook: &WorkerNotebook,
    phase: ExecutionPhase,
) -> String {
    let validation = notebook
        .validation_evidence
        .iter()
        .rev()
        .take(8)
        .map(|evidence| {
            json!({
                "gate_id": evidence.gate_id,
                "status": evidence.status,
                "command_fingerprint": evidence.command_fingerprint,
                "source_tree_hash": evidence.source_tree_hash,
            })
        })
        .collect::<Vec<_>>();
    let value = match phase {
        ExecutionPhase::Discovery | ExecutionPhase::ArtifactRepair => json!({
            "goal": notebook.goal,
            "criteria_ids": notebook.acceptance_criteria_v2.iter().map(|criterion| &criterion.id).collect::<Vec<_>>(),
            "files_inspected": notebook.files_inspected,
            "searches_completed": notebook.searches_completed,
            "blocking_unknowns": notebook.blocking_unknowns,
        }),
        ExecutionPhase::Planning => {
            if let Some(repair) = &notebook.planning_repair {
                json!({
                    "instruction": "Repair only the listed invalid fields. Preserve the valid planned changes and call record_implementation_plan once without repeating discovery.",
                    "goal": notebook.goal,
                    "valid_planned_changes": repair.valid_planned_changes,
                    "invalid_fields": repair.invalid_fields,
                    "acceptance_criteria_ids": notebook.acceptance_criteria_v2.iter().map(|criterion| &criterion.id).collect::<Vec<_>>(),
                })
            } else {
                json!({
                    "goal": notebook.goal,
                    "criteria": notebook.acceptance_criteria_v2,
                    "impact_areas": notebook.impact_map.iter().map(|area| json!({
                        "area_id": area.area_id,
                        "candidate_paths": area.candidate_paths,
                        "acceptance_criteria_ids": area.acceptance_criteria_ids,
                    })).collect::<Vec<_>>(),
                    "blocking_unknowns": notebook.blocking_unknowns,
                })
            }
        }
        ExecutionPhase::Implementation | ExecutionPhase::Repair => json!({
            "goal": notebook.goal,
            "planned_changes": notebook.planned_changes,
            "intended_changes": notebook.intended_changes,
            "remaining_work": notebook.remaining_work_v2,
            "blocking_unknowns": notebook.blocking_unknowns,
            "recent_failures": notebook.failed_changes.iter().rev().take(4).collect::<Vec<_>>(),
            "validation": validation,
        }),
        ExecutionPhase::DiffReview | ExecutionPhase::CompletionEvaluation => json!({
            "goal": notebook.goal,
            "intended_changes": notebook.intended_changes,
            "remaining_work": notebook.remaining_work_v2,
            "required_gates": notebook.required_gates,
            "validation": validation,
        }),
        ExecutionPhase::Validation | ExecutionPhase::Publication => json!({
            "goal": notebook.goal,
            "remaining_work": notebook.remaining_work_v2,
            "required_gates": notebook.required_gates,
            "validation": validation,
        }),
    };
    truncate_text(
        &serde_json::to_string(&value).unwrap_or_else(|_| "{}".into()),
        28 * 1024,
    )
}
