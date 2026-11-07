//! Translates a Claude Code session transcript (`transcript_path`, one JSON
//! record per line) into what RustGrid records for an interactive run.
//!
//! The transcript's `assistant` and `user` records use the same message and
//! content-block shapes as `claude -p --output-format stream-json`, so tool
//! calls are mapped with the helpers agent runs use (`tool_item`,
//! `derived_progress_from_line`). Unlike an agent run, a session is read over
//! many short-lived `trace flush` processes, so everything that spans lines
//! (pending tool calls, counted model calls) lives in [`TranslatorState`] and
//! is saved with the session.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::redact::{redact, redact_truncated};
use crate::agent::{canonical_usage, reported_exit_code, tool_item, tool_result_text};
use crate::execution::derived_progress_from_line;

const MAX_PROMPT_CHARS: usize = 20_000;
const MAX_MESSAGE_CHARS: usize = 20_000;
const MAX_COMMAND_CHARS: usize = 2_000;
const MAX_OUTPUT_CHARS: usize = 1_500;
const MAX_TARGET_CHARS: usize = 300;
/// Model call ids kept to skip the repeated usage on every record of one API
/// message. Records of one message are adjacent, so a short window suffices.
const SEEN_MESSAGES: usize = 256;

/// One thing worth recording, in transcript order.
#[derive(Clone, Debug, PartialEq)]
pub enum TraceItem {
    Prompt {
        id: String,
        text: String,
        images: usize,
        at: Option<String>,
    },
    AgentMessage {
        id: String,
        text: String,
        at: Option<String>,
    },
    /// A change of activity, such as "Searching relevant source files".
    Phase {
        id: String,
        message: String,
    },
    Command {
        id: String,
        command: String,
        exit_code: Option<i64>,
        output: String,
    },
    FileChange {
        id: String,
        paths: Vec<String>,
    },
    Tool {
        id: String,
        tool: String,
        target: Option<String>,
        failed: bool,
    },
    ModelCall {
        message_id: String,
        request_id: Option<String>,
        model: String,
        at: Option<String>,
        usage: Value,
    },
    PullRequest {
        url: String,
        number: Option<u64>,
    },
}

/// Translator state that must survive between `trace flush` processes.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TranslatorState {
    /// Tool calls awaiting their result, by `tool_use_id`.
    #[serde(default)]
    pending: BTreeMap<String, Value>,
    #[serde(default)]
    seen_messages: VecDeque<String>,
    #[serde(default)]
    last_phase: Option<String>,
    /// Claude Code repeats a session's `pr-link` record every turn.
    #[serde(default)]
    pull_requests: Vec<String>,
    /// Claude's latest reply in the current turn. Only the last one is the
    /// turn's outcome; earlier ones narrate work the steps already show.
    #[serde(default)]
    reply: Option<PendingReply>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PendingReply {
    id: String,
    text: String,
    at: Option<String>,
}

/// Read-only tools: the activity steps already say what Claude was doing.
const QUIET_TOOLS: [&str; 6] = ["Read", "Grep", "Glob", "LS", "TodoWrite", "NotebookRead"];

/// Ends the current turn: Claude's last reply, if any, is its outcome.
pub fn finish_turn(state: &mut TranslatorState) -> Option<TraceItem> {
    state.reply.take().map(|reply| TraceItem::AgentMessage {
        id: reply.id,
        text: reply.text,
        at: reply.at,
    })
}

pub fn translate(state: &mut TranslatorState, line: &str) -> Vec<TraceItem> {
    let Ok(record) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let at = record
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let uuid = record
        .get("uuid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    match record.get("type").and_then(Value::as_str) {
        Some("user") => user(state, &record, &uuid, at),
        Some("assistant") => assistant(state, &record, &uuid, at),
        Some("pr-link") => match record.get("prUrl").and_then(Value::as_str) {
            Some(url) if !state.pull_requests.iter().any(|seen| seen == url) => {
                state.pull_requests.push(url.to_owned());
                vec![TraceItem::PullRequest {
                    url: url.to_owned(),
                    number: record.get("prNumber").and_then(Value::as_u64),
                }]
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn blocks(record: &Value) -> &[Value] {
    record
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// A person typed this, as opposed to a tool result, an injected reminder,
/// a compaction summary or a background notification.
fn is_human_prompt(record: &Value) -> bool {
    if record.get("toolUseResult").is_some()
        || record.get("isMeta").and_then(Value::as_bool) == Some(true)
        || record.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
    {
        return false;
    }
    if let Some(kind) = record
        .get("origin")
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
    {
        return kind == "human";
    }
    let content = record
        .get("message")
        .and_then(|message| message.get("content"));
    match content {
        Some(Value::String(_)) => true,
        Some(Value::Array(parts)) => parts
            .iter()
            .all(|part| part.get("type").and_then(Value::as_str) != Some("tool_result")),
        _ => false,
    }
}

fn user(
    state: &mut TranslatorState,
    record: &Value,
    uuid: &str,
    at: Option<String>,
) -> Vec<TraceItem> {
    if is_human_prompt(record) {
        let content = record
            .get("message")
            .and_then(|message| message.get("content"));
        let (text, images) = match content {
            Some(Value::String(text)) => (text.clone(), 0),
            Some(Value::Array(parts)) => (
                parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                parts
                    .iter()
                    .filter(|part| {
                        matches!(
                            part.get("type").and_then(Value::as_str),
                            Some("image" | "document")
                        )
                    })
                    .count(),
            ),
            _ => (String::new(), 0),
        };
        if text.trim().is_empty() && images == 0 {
            return Vec::new();
        }
        // A new prompt ends the previous turn and starts a new one.
        state.last_phase = None;
        let mut items: Vec<TraceItem> = finish_turn(state).into_iter().collect();
        items.push(TraceItem::Prompt {
            id: uuid.to_owned(),
            text: redact_truncated(&text, MAX_PROMPT_CHARS),
            images,
            at,
        });
        return items;
    }
    let mut items = Vec::new();
    for block in blocks(record) {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(item) = state.pending.remove(id) else {
            continue;
        };
        let failed = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let output = tool_result_text(block.get("content"));
        match item.get("type").and_then(Value::as_str) {
            Some("command_execution") => items.push(TraceItem::Command {
                id: id.to_owned(),
                command: redact_truncated(
                    item.get("command")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    MAX_COMMAND_CHARS,
                ),
                exit_code: Some(if failed {
                    reported_exit_code(&output).unwrap_or(1)
                } else {
                    0
                }),
                output: redact_truncated(output.trim(), MAX_OUTPUT_CHARS),
            }),
            Some("file_change") => {
                let paths = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|change| change.get("path").and_then(Value::as_str))
                    .map(redact)
                    .collect::<Vec<_>>();
                if !failed && !paths.is_empty() {
                    items.push(TraceItem::FileChange {
                        id: id.to_owned(),
                        paths,
                    });
                }
            }
            _ if QUIET_TOOLS.contains(&tool_name(&item).as_str()) => {}
            _ => items.push(TraceItem::Tool {
                id: id.to_owned(),
                tool: tool_name(&item),
                target: tool_target(&item)
                    .map(|target| redact_truncated(&target, MAX_TARGET_CHARS)),
                failed,
            }),
        }
    }
    items
}

fn assistant(
    state: &mut TranslatorState,
    record: &Value,
    uuid: &str,
    at: Option<String>,
) -> Vec<TraceItem> {
    let mut items = Vec::new();
    let message = record.get("message").cloned().unwrap_or(Value::Null);
    if let (Some(message_id), Some(usage)) = (
        message.get("id").and_then(Value::as_str),
        message.get("usage"),
    ) && !state.seen_messages.iter().any(|seen| seen == message_id)
        && let Some(mut canonical) = canonical_usage(usage)
    {
        state.seen_messages.push_back(message_id.to_owned());
        while state.seen_messages.len() > SEEN_MESSAGES {
            state.seen_messages.pop_front();
        }
        // RustGrid prices standard processing only and fails closed otherwise.
        for field in ["service_tier", "inference_geo", "speed"] {
            if let Some(value) = usage.get(field).filter(|value| value.is_string()) {
                canonical[field] = value.clone();
            }
        }
        items.push(TraceItem::ModelCall {
            message_id: message_id.to_owned(),
            request_id: record
                .get("requestId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            model: message
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            at: at.clone(),
            usage: canonical,
        });
    }
    for (index, block) in blocks(record).iter().enumerate() {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !text.trim().is_empty() {
                    state.reply = Some(PendingReply {
                        id: format!("{uuid}:{index}"),
                        text: redact_truncated(text, MAX_MESSAGE_CHARS),
                        at: at.clone(),
                    });
                }
            }
            Some("tool_use") => {
                let Some(item) = tool_item(block) else {
                    continue;
                };
                let started = json!({"type": "item.started", "item": item}).to_string();
                if let Some((phase, message)) = derived_progress_from_line(&started)
                    && state.last_phase.as_deref() != Some(phase.as_str())
                {
                    items.push(TraceItem::Phase {
                        id: format!("{uuid}:{index}"),
                        message: message.to_owned(),
                    });
                    state.last_phase = Some(phase);
                }
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    state.pending.insert(id.to_owned(), item);
                }
            }
            _ => {}
        }
    }
    items
}

fn tool_name(item: &Value) -> String {
    match item.get("type").and_then(Value::as_str) {
        Some("mcp_tool_call") => item.get("tool").and_then(Value::as_str).unwrap_or("tool"),
        Some("web_search") => "Web",
        Some("collab_tool_call") => "Subagent",
        Some(other) => other,
        None => "tool",
    }
    .to_owned()
}

/// What a tool acted on: a file, a search pattern, a URL or a subagent task.
/// Arguments are never sent whole.
fn tool_target(item: &Value) -> Option<String> {
    let arguments = item.get("arguments");
    let field = |name: &str| {
        arguments
            .and_then(|arguments| arguments.get(name))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match item.get("type").and_then(Value::as_str) {
        Some("web_search") => item.get("query").and_then(Value::as_str).map(str::to_owned),
        Some("collab_tool_call") => field("description"),
        _ => field("file_path")
            .or_else(|| field("path"))
            .or_else(|| field("pattern"))
            .or_else(|| field("url"))
            .or_else(|| field("skill")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Translates the lines, then ends the turn as the Stop hook does.
    fn run(lines: &[&str]) -> Vec<TraceItem> {
        let mut state = TranslatorState::default();
        let mut items: Vec<TraceItem> = lines
            .iter()
            .flat_map(|line| translate(&mut state, &with_token(line)))
            .collect();
        items.extend(finish_turn(&mut state));
        items
    }

    /// Fixtures hold a placeholder so no token-shaped text is committed.
    fn with_token(line: &str) -> String {
        line.replace(
            "GITHUB_TOKEN_PLACEHOLDER",
            &format!("ghp_{}", "a".repeat(36)),
        )
    }

    const PROMPT: &str = r#"{"type":"user","uuid":"u1","timestamp":"2026-11-07T10:00:00Z","origin":{"kind":"human"},"message":{"role":"user","content":[{"type":"text","text":"Fix login. DATABASE_URL=postgres://u:p@h/db"},{"type":"image","source":{}}]}}"#;
    const META: &str = r#"{"type":"user","uuid":"u0","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"<system-reminder>"}]}}"#;
    const NOTIFICATION: &str = r#"{"type":"user","uuid":"u9","origin":{"kind":"task-notification"},"message":{"role":"user","content":"done"}}"#;
    const THINK_AND_GREP: &str = r#"{"type":"assistant","uuid":"a1","requestId":"req_1","timestamp":"2026-11-07T10:00:05Z","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"thinking","thinking":"secret plan"},{"type":"tool_use","id":"t1","name":"Grep","input":{"pattern":"fn login"}}],"usage":{"input_tokens":10,"cache_creation_input_tokens":1000,"cache_read_input_tokens":500,"output_tokens":50,"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":1000}}}}"#;
    const SAME_MESSAGE_BASH: &str = r#"{"type":"assistant","uuid":"a2","requestId":"req_1","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"cargo test -p app"}}],"usage":{"input_tokens":10,"cache_creation_input_tokens":1000,"cache_read_input_tokens":500,"output_tokens":50}}}"#;
    const RESULTS: &str = r#"{"type":"user","uuid":"u2","toolUseResult":{},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"src/login.rs:3"},{"type":"tool_result","tool_use_id":"t2","is_error":true,"content":"Exit code 101\ntest failed: token GITHUB_TOKEN_PLACEHOLDER"}]}}"#;
    const EDIT_AND_TEXT: &str = r#"{"type":"assistant","uuid":"a3","message":{"id":"msg_2","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"t3","name":"Edit","input":{"file_path":"/repo/src/login.rs","old_string":"a","new_string":"b"}},{"type":"text","text":"Fixed the redirect."}],"usage":{"input_tokens":5,"output_tokens":20}}}"#;
    const EDIT_RESULT: &str = r#"{"type":"user","uuid":"u3","toolUseResult":{},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t3","content":"ok"}]}}"#;
    const PR: &str = r#"{"type":"pr-link","prNumber":57,"prUrl":"https://github.com/RustGrid/x/pull/57","prRepository":"RustGrid/x"}"#;

    #[test]
    fn a_session_becomes_prompts_steps_commands_messages_usage_and_a_pr() {
        let items = run(&[
            META,
            PROMPT,
            NOTIFICATION,
            THINK_AND_GREP,
            SAME_MESSAGE_BASH,
            RESULTS,
            EDIT_AND_TEXT,
            EDIT_RESULT,
            PR,
            PR,
        ]);
        let ghp = format!("ghp_{}", "a".repeat(36));
        assert!(!format!("{items:?}").contains(&ghp));
        assert!(!format!("{items:?}").contains("secret plan"));
        assert_eq!(
            items,
            vec![
                TraceItem::Prompt {
                    id: "u1".into(),
                    text: "Fix login. DATABASE_URL=[REDACTED]".into(),
                    images: 1,
                    at: Some("2026-11-07T10:00:00Z".into()),
                },
                TraceItem::ModelCall {
                    message_id: "msg_1".into(),
                    request_id: Some("req_1".into()),
                    model: "claude-opus-5-5".into(),
                    at: Some("2026-11-07T10:00:05Z".into()),
                    usage: json!({
                        "input_tokens": 1510,
                        "cached_input_tokens": 500,
                        "output_tokens": 50,
                        "total_tokens": 1560,
                        "cache_creation_input_tokens": 1000,
                        "cache_creation_1h_input_tokens": 1000,
                        "service_tier": "standard"
                    }),
                },
                TraceItem::Phase {
                    id: "a1:1".into(),
                    message: "Searching relevant source files".into(),
                },
                TraceItem::Phase {
                    id: "a2:0".into(),
                    message: "Running focused validation".into(),
                },
                TraceItem::Command {
                    id: "t2".into(),
                    command: "cargo test -p app".into(),
                    exit_code: Some(101),
                    output: "Exit code 101\ntest failed: token [REDACTED]".into(),
                },
                TraceItem::ModelCall {
                    message_id: "msg_2".into(),
                    request_id: None,
                    model: "claude-opus-5-5".into(),
                    at: None,
                    usage: json!({
                        "input_tokens": 5,
                        "cached_input_tokens": 0,
                        "output_tokens": 20,
                        "total_tokens": 25,
                        "cache_creation_input_tokens": 0
                    }),
                },
                TraceItem::Phase {
                    id: "a3:0".into(),
                    message: "Updating implementation".into(),
                },
                TraceItem::FileChange {
                    id: "t3".into(),
                    paths: vec!["/repo/src/login.rs".into()],
                },
                TraceItem::PullRequest {
                    url: "https://github.com/RustGrid/x/pull/57".into(),
                    number: Some(57),
                },
                // Claude's last reply is the turn's outcome, recorded when it ends.
                TraceItem::AgentMessage {
                    id: "a3:1".into(),
                    text: "Fixed the redirect.".into(),
                    at: None,
                },
            ]
        );
    }

    #[test]
    fn only_the_last_reply_of_a_turn_is_kept() {
        let narration = r#"{"type":"assistant","uuid":"n1","message":{"id":"msg_9","model":"m","content":[{"type":"text","text":"Now let me check the router."}]}}"#;
        let answer = r#"{"type":"assistant","uuid":"n2","message":{"id":"msg_10","model":"m","content":[{"type":"text","text":"The redirect is fixed."}]}}"#;
        let next_prompt = r#"{"type":"user","uuid":"u5","origin":{"kind":"human"},"message":{"role":"user","content":"Thanks, now the docs"}}"#;
        let replies = |items: &[TraceItem]| {
            items
                .iter()
                .filter_map(|item| match item {
                    TraceItem::AgentMessage { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let mut state = TranslatorState::default();
        let mut items = Vec::new();
        for line in [PROMPT, narration, answer, next_prompt] {
            items.extend(translate(&mut state, line));
        }
        // The next prompt ends the first turn with its last reply only.
        assert_eq!(replies(&items), vec!["The redirect is fixed."]);
        let position = |wanted: &str| {
            items.iter().position(|item| match item {
                TraceItem::AgentMessage { text, .. } => text == wanted,
                TraceItem::Prompt { text, .. } => text == wanted,
                _ => false,
            })
        };
        assert!(position("The redirect is fixed.") < position("Thanks, now the docs"));
        assert!(finish_turn(&mut state).is_none());
    }

    #[test]
    fn pending_tool_calls_survive_a_saved_and_reloaded_state() {
        let mut state = TranslatorState::default();
        translate(&mut state, THINK_AND_GREP);
        translate(&mut state, SAME_MESSAGE_BASH);
        let mut reloaded: TranslatorState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        let items = translate(&mut reloaded, &with_token(RESULTS));
        // The Grep result is quiet; the command completes from the saved state.
        assert!(
            matches!(items.as_slice(), [TraceItem::Command { .. }]),
            "{items:?}"
        );
        // The repeated usage on a later record of the same message is not counted twice.
        assert!(
            translate(&mut reloaded, SAME_MESSAGE_BASH)
                .iter()
                .all(|item| !matches!(item, TraceItem::ModelCall { .. }))
        );
    }
}
