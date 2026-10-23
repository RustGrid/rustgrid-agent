//! Coding-agent providers and the worker's canonical agent event stream.
//!
//! The worker's discovery, validation, completion, and telemetry logic consumes
//! one event vocabulary: the Codex `exec --json` JSONL schema (`thread.started`,
//! `turn.started`, `item.started`, `item.completed`, `turn.completed`,
//! `turn.failed`, `error`). Codex already speaks it. Claude Code's
//! `--output-format stream-json` messages are translated into it here, so every
//! downstream guard (worker-owned command detection, focused-validation
//! evidence, completion readiness, token accounting) applies to both agents
//! without a second implementation.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The coding agent a signed execution policy selects.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProvider {
    #[default]
    Codex,
    Claude,
}

impl AgentProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    /// Human-readable agent name used in run steps, feedback, and telemetry.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }

    /// Model vendor reported on telemetry model calls.
    pub const fn model_vendor(self) -> &'static str {
        match self {
            Self::Codex => "openai",
            Self::Claude => "anthropic",
        }
    }

    /// Docker Sandboxes agent type; it selects the credential proxy and image.
    pub const fn sandbox_agent(self) -> &'static str {
        self.as_str()
    }

    /// The executable a policy command must start with.
    pub const fn executable(self) -> &'static str {
        self.as_str()
    }

    /// Whether a raw output line shows the agent is still making progress.
    pub fn output_is_activity(self) -> fn(&str) -> bool {
        match self {
            Self::Codex => codex_output_is_meaningful_activity,
            Self::Claude => claude_output_is_meaningful_activity,
        }
    }
}

/// Converts one agent's raw stdout lines into canonical agent events.
pub(crate) enum AgentStream {
    Codex,
    Claude(ClaudeStreamTranslator),
}

impl AgentStream {
    pub(crate) fn new(provider: AgentProvider) -> Self {
        match provider {
            AgentProvider::Codex => Self::Codex,
            AgentProvider::Claude => Self::Claude(ClaudeStreamTranslator::default()),
        }
    }

    pub(crate) fn translate(&mut self, line: &str) -> Vec<String> {
        match self {
            Self::Codex => vec![line.to_owned()],
            Self::Claude(translator) => translator.translate(line),
        }
    }
}

pub(crate) fn codex_output_is_meaningful_activity(line: &str) -> bool {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return true;
    };
    match event.get("type").and_then(Value::as_str) {
        Some("item.started" | "item.updated" | "item.completed") => {
            event
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                != Some("reasoning")
        }
        Some("thread.started" | "turn.started" | "turn.completed" | "turn.failed" | "error") => {
            true
        }
        Some(_) => false,
        None => true,
    }
}

/// Mirrors the Codex rule: tool calls, tool results, visible text, and session
/// boundaries count as progress; thinking-only messages, token estimates, and
/// rate-limit notices do not, so a stalled agent still trips the idle watchdog.
pub(crate) fn claude_output_is_meaningful_activity(line: &str) -> bool {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return true;
    };
    match event.get("type").and_then(Value::as_str) {
        Some("assistant") => content_blocks(&event)
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) != Some("thinking")),
        Some("user" | "result") => true,
        Some("system") => matches!(
            event.get("subtype").and_then(Value::as_str),
            Some("init" | "api_retry" | "compact_boundary")
        ),
        Some(_) => false,
        None => true,
    }
}

/// Stateful Claude Code `stream-json` to canonical event translator. Tool
/// results arrive in a later message than their tool call, so pending calls are
/// remembered by `tool_use_id` until they complete.
#[derive(Default)]
pub(crate) struct ClaudeStreamTranslator {
    pending: HashMap<String, Value>,
}

impl ClaudeStreamTranslator {
    pub(crate) fn translate(&mut self, line: &str) -> Vec<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
            // Non-JSON output (for example a CLI diagnostic) keeps the raw-line
            // behavior the Codex path already has.
            return vec![trimmed.to_owned()];
        };
        let events = match event.get("type").and_then(Value::as_str) {
            Some("system") => Self::system(&event),
            Some("assistant") => self.assistant(&event),
            Some("user") => self.tool_results(&event),
            Some("result") => self.result(&event),
            _ => Vec::new(),
        };
        events.into_iter().map(|event| event.to_string()).collect()
    }

    fn system(event: &Value) -> Vec<Value> {
        match event.get("subtype").and_then(Value::as_str) {
            Some("init") => {
                let mut events = Vec::new();
                if let Some(session_id) = event.get("session_id").and_then(Value::as_str) {
                    events.push(json!({"type": "thread.started", "thread_id": session_id}));
                }
                events.push(json!({"type": "turn.started"}));
                events
            }
            Some("api_retry") => vec![json!({
                "type": "error",
                "message": format!(
                    "Retrying Anthropic API request{}",
                    event
                        .get("attempt")
                        .and_then(Value::as_u64)
                        .map(|attempt| format!(" (attempt {attempt})"))
                        .unwrap_or_default()
                )
            })],
            _ => Vec::new(),
        }
    }

    fn assistant(&mut self, event: &Value) -> Vec<Value> {
        let message_id = event
            .get("message")
            .and_then(|message| message.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("message");
        let mut events = Vec::new();
        for (index, block) in content_blocks(event).iter().enumerate() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let Some(text) = block.get("text").and_then(Value::as_str) else {
                        continue;
                    };
                    if text.trim().is_empty() {
                        continue;
                    }
                    events.push(json!({
                        "type": "item.completed",
                        "item": {"id": format!("{message_id}:{index}"), "type": "agent_message", "text": text}
                    }));
                }
                Some("thinking") => events.push(json!({
                    "type": "item.completed",
                    "item": {
                        "id": format!("{message_id}:{index}"),
                        "type": "reasoning",
                        "text": block.get("thinking").and_then(Value::as_str).unwrap_or_default()
                    }
                })),
                Some("tool_use") => {
                    let Some(item) = tool_item(block) else {
                        continue;
                    };
                    let id = item["id"].as_str().unwrap_or_default().to_owned();
                    events.push(json!({"type": "item.started", "item": item.clone()}));
                    self.pending.insert(id, item);
                }
                _ => {}
            }
        }
        events
    }

    fn tool_results(&mut self, event: &Value) -> Vec<Value> {
        content_blocks(event)
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            .filter_map(|block| {
                let id = block.get("tool_use_id").and_then(Value::as_str)?;
                let mut item = self.pending.remove(id)?;
                let failed = block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let output = tool_result_text(block.get("content"));
                item["status"] = json!(if failed { "failed" } else { "completed" });
                if item["type"] == "command_execution" {
                    item["exit_code"] = json!(if failed {
                        reported_exit_code(&output).unwrap_or(1)
                    } else {
                        0
                    });
                    item["aggregated_output"] = json!(output);
                } else {
                    item["result"] = json!(output);
                    if failed {
                        item["error"] = json!({"message": output});
                    }
                }
                Some(json!({"type": "item.completed", "item": item}))
            })
            .collect()
    }

    fn result(&mut self, event: &Value) -> Vec<Value> {
        // Calls without a result were interrupted by the session ending.
        let mut events = self
            .pending
            .drain()
            .map(|(_, mut item)| {
                item["status"] = json!("cancelled");
                json!({"type": "item.completed", "item": item})
            })
            .collect::<Vec<_>>();
        let failed = event
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || event
                .get("subtype")
                .and_then(Value::as_str)
                .is_some_and(|subtype| subtype != "success");
        if failed {
            let message = event
                .get("result")
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
                .or_else(|| event.get("subtype").and_then(Value::as_str))
                .unwrap_or("Claude Code session failed")
                .to_owned();
            events.push(json!({"type": "error", "message": message}));
            events.push(json!({"type": "turn.failed", "error": {"message": message}}));
        } else {
            let mut completed = json!({"type": "turn.completed"});
            if let Some(usage) = event.get("usage").and_then(canonical_usage) {
                completed["usage"] = usage;
            }
            events.push(completed);
        }
        events
    }
}

fn content_blocks(event: &Value) -> &[Value] {
    event
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Maps a Claude tool call onto the canonical item type telemetry and the
/// worker guards already understand.
fn tool_item(block: &Value) -> Option<Value> {
    let id = block.get("id").and_then(Value::as_str)?;
    let name = block.get("name").and_then(Value::as_str)?;
    let input = block.get("input").cloned().unwrap_or(Value::Null);
    let item = match name {
        "Bash" => json!({
            "id": id,
            "type": "command_execution",
            "command": input.get("command").and_then(Value::as_str).unwrap_or_default(),
            "status": "in_progress"
        }),
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
            let path = input
                .get("file_path")
                .or_else(|| input.get("notebook_path"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let kind = if name == "Write" { "add" } else { "update" };
            json!({
                "id": id,
                "type": "file_change",
                "changes": [{"path": path, "kind": kind}],
                "status": "in_progress"
            })
        }
        "WebSearch" | "WebFetch" => json!({
            "id": id,
            "type": "web_search",
            "query": input.get("query").or_else(|| input.get("url")).cloned().unwrap_or(Value::Null),
            "status": "in_progress"
        }),
        "Task" | "Agent" => json!({
            "id": id,
            "type": "collab_tool_call",
            "arguments": input,
            "status": "in_progress"
        }),
        _ => json!({
            "id": id,
            "type": "mcp_tool_call",
            "server": "claude_code",
            "tool": name,
            "arguments": input,
            "status": "in_progress"
        }),
    };
    Some(item)
}

fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Claude Code reports a failing shell command as `Exit code N` on the first
/// line of the tool result.
fn reported_exit_code(output: &str) -> Option<i64> {
    output
        .lines()
        .next()?
        .trim()
        .strip_prefix("Exit code ")?
        .trim()
        .parse()
        .ok()
}

/// Converts Anthropic usage into the canonical `turn.completed` usage shape.
/// Anthropic reports uncached, cache-write, and cache-read input separately;
/// the canonical `input_tokens` is their sum and `cached_input_tokens` is the
/// cache-read share, matching how Codex reports cached input. Cache writes
/// stay visible as `cache_creation_input_tokens`, with the 1-hour TTL share in
/// `cache_creation_1h_input_tokens`, because Anthropic bills 1-hour writes at
/// a higher rate than 5-minute writes.
fn canonical_usage(usage: &Value) -> Option<Value> {
    let count = |field: &str| usage.get(field).and_then(Value::as_u64);
    let output_tokens = count("output_tokens")?;
    let uncached = count("input_tokens")?;
    let cache_write = count("cache_creation_input_tokens").unwrap_or(0);
    let cache_read = count("cache_read_input_tokens").unwrap_or(0);
    let input_tokens = uncached.checked_add(cache_write)?.checked_add(cache_read)?;
    let mut canonical = json!({
        "input_tokens": input_tokens,
        "cached_input_tokens": cache_read,
        "output_tokens": output_tokens,
        "total_tokens": input_tokens.checked_add(output_tokens)?,
        "cache_creation_input_tokens": cache_write
    });
    if let Some(one_hour) = usage
        .get("cache_creation")
        .and_then(|breakdown| breakdown.get("ephemeral_1h_input_tokens"))
        .and_then(Value::as_u64)
    {
        canonical["cache_creation_1h_input_tokens"] = json!(one_hour.min(cache_write));
    }
    if let Some(thinking) = usage
        .get("output_tokens_details")
        .and_then(|details| details.get("thinking_tokens"))
        .and_then(Value::as_u64)
    {
        canonical["reasoning_output_tokens"] = json!(thinking);
    }
    Some(canonical)
}

/// Resolves the telemetry vendor and model from a Claude Code command line.
pub(crate) fn claude_vendor_and_model(args: &[String]) -> (String, String) {
    let mut model = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--model" if index + 1 < args.len() => {
                model = Some(args[index + 1].clone());
                index += 1;
            }
            value if value.starts_with("--model=") => {
                model = value.split_once('=').map(|(_, value)| value.to_owned());
            }
            _ => {}
        }
        index += 1;
    }
    (
        AgentProvider::Claude.model_vendor().to_owned(),
        model.unwrap_or_else(|| "unknown".into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate_all(lines: &[&str]) -> Vec<Value> {
        let mut translator = ClaudeStreamTranslator::default();
        lines
            .iter()
            .flat_map(|line| translator.translate(line))
            .map(|line| serde_json::from_str(&line).unwrap())
            .collect()
    }

    // Shapes captured from Claude Code 2.1.246 `-p --output-format stream-json --verbose`.
    const INIT: &str = r#"{"type":"system","subtype":"init","cwd":"/w","session_id":"c9e824c1-b3c3-4567-9684-efb9b977dc93","model":"claude-opus-5-5","permissionMode":"bypassPermissions"}"#;
    const FAILED_LS: &str = r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls missing"}}]}}"#;
    const FAILED_LS_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"Exit code 2\nls: missing: No such file or directory","is_error":true,"tool_use_id":"toolu_1"}]}}"#;
    const TEST: &str = r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"cargo test parser::"}}]}}"#;
    const TEST_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_2","type":"tool_result","content":"test result: ok","is_error":false}]}}"#;
    const WRITE: &str = r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"tool_use","id":"toolu_3","name":"Write","input":{"file_path":"/w/a.txt","content":"hi"}}]}}"#;
    const WRITE_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_3","type":"tool_result","content":"File created successfully at: /w/a.txt"}]}}"#;
    const TEXT: &str = r#"{"type":"assistant","message":{"id":"msg_2","content":[{"type":"text","text":"Done.\nRUSTGRID_AGENT_STATUS: COMPLETED"}]}}"#;
    const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.","usage":{"input_tokens":18,"cache_creation_input_tokens":64807,"cache_read_input_tokens":1200,"output_tokens":419,"output_tokens_details":{"thinking_tokens":164}}}"#;

    #[test]
    fn translates_a_claude_session_into_canonical_agent_events() {
        let events = translate_all(&[
            INIT,
            FAILED_LS,
            FAILED_LS_RESULT,
            TEST,
            TEST_RESULT,
            WRITE,
            WRITE_RESULT,
            TEXT,
            RESULT,
        ]);
        let types = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            [
                "thread.started",
                "turn.started",
                "item.started",
                "item.completed",
                "item.started",
                "item.completed",
                "item.started",
                "item.completed",
                "item.completed",
                "turn.completed"
            ]
        );
        assert_eq!(
            events[0]["thread_id"],
            "c9e824c1-b3c3-4567-9684-efb9b977dc93"
        );

        let failed = &events[3]["item"];
        assert_eq!(failed["type"], "command_execution");
        assert_eq!(failed["command"], "ls missing");
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["exit_code"], 2);

        let passed = &events[5]["item"];
        assert_eq!(passed["command"], "cargo test parser::");
        assert_eq!(passed["exit_code"], 0);

        let written = &events[7]["item"];
        assert_eq!(written["type"], "file_change");
        assert_eq!(written["changes"][0]["path"], "/w/a.txt");
        assert_eq!(written["status"], "completed");

        assert_eq!(events[8]["item"]["type"], "agent_message");
        assert!(
            events[8]["item"]["text"]
                .as_str()
                .unwrap()
                .contains("RUSTGRID_AGENT_STATUS: COMPLETED")
        );

        let usage = &events[9]["usage"];
        assert_eq!(usage["input_tokens"], 18 + 64_807 + 1_200);
        assert_eq!(usage["cached_input_tokens"], 1_200);
        assert_eq!(usage["output_tokens"], 419);
        assert_eq!(usage["reasoning_output_tokens"], 164);
        assert_eq!(usage["total_tokens"], 18 + 64_807 + 1_200 + 419);
    }

    #[test]
    fn canonical_usage_feeds_the_existing_token_accounting() {
        let mut consumption = crate::token_consumption::TokenConsumption::default();
        for line in ClaudeStreamTranslator::default().translate(RESULT) {
            consumption.observe_codex_jsonl(&line).unwrap();
        }
        assert_eq!(consumption.input_tokens, 66_025);
        assert_eq!(consumption.cached_input_tokens, 1_200);
        assert_eq!(consumption.output_tokens, 419);
    }

    #[test]
    fn canonical_usage_keeps_the_one_hour_cache_write_share() {
        let usage = canonical_usage(&json!({
            "input_tokens": 10,
            "cache_creation_input_tokens": 5_000,
            "cache_read_input_tokens": 300,
            "output_tokens": 20,
            "cache_creation": {"ephemeral_5m_input_tokens": 3_000, "ephemeral_1h_input_tokens": 2_000}
        }))
        .unwrap();
        assert_eq!(usage["input_tokens"], 10 + 5_000 + 300);
        assert_eq!(usage["cache_creation_input_tokens"], 5_000);
        assert_eq!(usage["cache_creation_1h_input_tokens"], 2_000);

        let without_breakdown = canonical_usage(&json!({
            "input_tokens": 10, "cache_creation_input_tokens": 5_000, "output_tokens": 20
        }))
        .unwrap();
        assert!(
            without_breakdown
                .get("cache_creation_1h_input_tokens")
                .is_none()
        );
    }

    #[test]
    fn failed_sessions_cancel_open_tools_and_report_the_error() {
        let events = translate_all(&[
            INIT,
            TEST,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"API Error: 529 overloaded"}"#,
        ]);
        let types = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            [
                "thread.started",
                "turn.started",
                "item.started",
                "item.completed",
                "error",
                "turn.failed"
            ]
        );
        assert_eq!(events[3]["item"]["status"], "cancelled");
        assert_eq!(events[4]["message"], "API Error: 529 overloaded");
    }

    #[test]
    fn maps_non_shell_tools_to_known_telemetry_categories() {
        let events = translate_all(&[
            r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"t1","name":"Grep","input":{"pattern":"fn main"}},{"type":"tool_use","id":"t2","name":"Task","input":{"prompt":"explore"}},{"type":"tool_use","id":"t3","name":"WebFetch","input":{"url":"https://example.com"}}]}}"#,
        ]);
        assert_eq!(events[0]["item"]["type"], "mcp_tool_call");
        assert_eq!(events[0]["item"]["tool"], "Grep");
        assert_eq!(events[1]["item"]["type"], "collab_tool_call");
        assert_eq!(events[2]["item"]["type"], "web_search");
    }

    #[test]
    fn ignores_unmatched_results_and_bookkeeping_messages() {
        assert!(translate_all(&[FAILED_LS_RESULT]).is_empty());
        assert!(
            translate_all(&[
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
                r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":50}"#,
            ])
            .is_empty()
        );
        assert_eq!(
            ClaudeStreamTranslator::default().translate("plain diagnostic"),
            ["plain diagnostic"]
        );
    }

    #[test]
    fn claude_thinking_and_bookkeeping_do_not_reset_the_idle_watchdog() {
        assert!(claude_output_is_meaningful_activity(INIT));
        assert!(claude_output_is_meaningful_activity(TEST));
        assert!(claude_output_is_meaningful_activity(TEST_RESULT));
        assert!(claude_output_is_meaningful_activity(RESULT));
        assert!(!claude_output_is_meaningful_activity(
            r#"{"type":"assistant","message":{"id":"m","content":[{"type":"thinking","thinking":""}]}}"#
        ));
        assert!(!claude_output_is_meaningful_activity(
            r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":50}"#
        ));
        assert!(!claude_output_is_meaningful_activity(
            r#"{"type":"rate_limit_event"}"#
        ));
    }

    #[test]
    fn all_non_reasoning_codex_output_preserves_a_working_session() {
        assert!(!codex_output_is_meaningful_activity(
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}"#
        ));
        assert!(codex_output_is_meaningful_activity(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"working"}}"#
        ));
        assert!(codex_output_is_meaningful_activity(
            r#"{"type":"error","message":"Reconnecting... 4/5"}"#
        ));
        assert!(codex_output_is_meaningful_activity(
            r#"{"type":"item.started","item":{"type":"command_execution","command":"npm test"}}"#
        ));
        assert!(codex_output_is_meaningful_activity(
            r#"{"type":"item.completed","item":{"type":"file_change","changes":[]}}"#
        ));
        assert!(!codex_output_is_meaningful_activity(
            r#"{"type":"response.keepalive"}"#
        ));
    }

    #[test]
    fn resolves_the_claude_model_from_the_command() {
        let args = ["claude", "-p", "--model", "claude-opus-5-5"].map(String::from);
        assert_eq!(
            claude_vendor_and_model(&args),
            ("anthropic".into(), "claude-opus-5-5".into())
        );
        assert_eq!(
            claude_vendor_and_model(&["claude".into(), "--model=claude-haiku-5-5".into()]).1,
            "claude-haiku-5-5"
        );
    }
}
