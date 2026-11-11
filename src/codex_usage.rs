//! Per-call usage for Codex runs.
//!
//! `codex exec --json` prints a turn's usage once, when the turn ends, and the
//! worker runs Codex with `--ephemeral`, so no session rollout is written
//! either. Codex does export an OpenTelemetry log for every model response
//! (`codex.sse_event` with `event.kind = response.completed`) carrying that
//! response's token counts. The worker points Codex's OTLP/HTTP log exporter
//! at a receiver it runs on `127.0.0.1` for the duration of the agent step and
//! turns each of those logs into a canonical `model_call.completed` event, so
//! Codex consumption is reported over the session like Claude Code's.
//!
//! The receiver keeps only token counts, the model, the service tier and the
//! time Codex recorded. The exported logs also carry the signed-in account and
//! other session details, which are dropped unread. Requests must present a
//! per-step token, so other local processes cannot add usage.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

const LOGS_PATH: &str = "/v1/logs";
pub(crate) const TOKEN_HEADER: &str = "x-rustgrid-usage-token";
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Codex batches its logs and exports them about once a second.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(3);

/// One accepted export, with the calls it held (possibly none).
struct Delivery {
    received: Instant,
    calls: Vec<String>,
}

pub(crate) struct CodexUsageReceiver {
    address: SocketAddr,
    token: String,
    deliveries: Receiver<Delivery>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl CodexUsageReceiver {
    pub(crate) fn start() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("could not listen for Codex usage telemetry")?;
        let address = listener.local_addr()?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let (sender, deliveries) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let token = token.clone();
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("codex-usage".into())
                .spawn(move || serve(&listener, &token, &stop, &sender))
                .context("could not start the Codex usage receiver")?
        };
        Ok(Self {
            address,
            token,
            deliveries,
            stop,
            worker: Some(worker),
        })
    }

    pub(crate) fn port(&self) -> u16 {
        self.address.port()
    }

    /// `codex exec` arguments that export Codex's logs to this receiver,
    /// reached at `host` (`host:port`) from wherever Codex runs.
    pub(crate) fn codex_args(&self, host: &str) -> Vec<String> {
        vec![
            "-c".into(),
            format!(
                "otel.exporter={{otlp-http={{endpoint=\"http://{host}{LOGS_PATH}\",protocol=\"json\",headers={{\"{TOKEN_HEADER}\"=\"{}\"}}}}}}",
                self.token
            ),
            "-c".into(),
            "otel.log_user_prompt=false".into(),
        ]
    }

    /// Calls received so far.
    pub(crate) fn take_calls(&self) -> Vec<String> {
        self.deliveries
            .try_iter()
            .flat_map(|delivery| delivery.calls)
            .collect()
    }

    /// Calls received so far, after waiting for Codex's next export once
    /// `since` has passed, so a turn's last response is not missed. Gives up
    /// after a few seconds: anything later is still in the turn's aggregate.
    pub(crate) fn settle(&self, since: Instant) -> Vec<String> {
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        let mut calls = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.deliveries.recv_timeout(remaining) {
                Ok(delivery) => {
                    calls.extend(delivery.calls);
                    if delivery.received > since {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        calls.extend(self.take_calls());
        calls
    }
}

impl Drop for CodexUsageReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept so the thread sees the stop flag.
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(200));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve(listener: &TcpListener, token: &str, stop: &AtomicBool, sender: &Sender<Delivery>) {
    let mut last_completed_at = None;
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(mut stream) = stream else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
        let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
        let status = match read_export(&mut stream, token) {
            Ok(body) => {
                let calls = calls_from_export(&body, &mut last_completed_at);
                let _ = sender.send(Delivery {
                    received: Instant::now(),
                    calls,
                });
                "200 OK"
            }
            Err(status) => status,
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        let _ = stream.flush();
        let _ = stream.shutdown(Shutdown::Both);
    }
}

/// Reads one OTLP/HTTP JSON log export, or the HTTP status to refuse it with.
fn read_export(stream: &mut TcpStream, token: &str) -> Result<Vec<u8>, &'static str> {
    let mut reader = BufReader::new(stream);
    let mut header_bytes = 0;
    let mut line = String::new();
    read_header_line(&mut reader, &mut header_bytes, &mut line)?;
    let mut request = line.split_whitespace();
    if request.next() != Some("POST") {
        return Err("405 Method Not Allowed");
    }
    if request.next() != Some(LOGS_PATH) {
        return Err("404 Not Found");
    }
    let mut length = None;
    let mut authorized = false;
    let mut json = false;
    loop {
        read_header_line(&mut reader, &mut header_bytes, &mut line)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            return Err("400 Bad Request");
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse::<usize>().ok(),
            "content-type" => json = value.to_ascii_lowercase().starts_with("application/json"),
            "content-encoding" if !value.eq_ignore_ascii_case("identity") => {
                return Err("415 Unsupported Media Type");
            }
            "transfer-encoding" => return Err("411 Length Required"),
            TOKEN_HEADER => authorized = value == token,
            _ => {}
        }
    }
    if !authorized {
        return Err("401 Unauthorized");
    }
    if !json {
        return Err("415 Unsupported Media Type");
    }
    let length = length.ok_or("411 Length Required")?;
    if length > MAX_BODY_BYTES {
        return Err("413 Content Too Large");
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|_| "400 Bad Request")?;
    Ok(body)
}

/// Reads one header line into `line`, keeping all headers within
/// `MAX_HEADER_BYTES`.
fn read_header_line(
    reader: &mut BufReader<&mut TcpStream>,
    header_bytes: &mut usize,
    line: &mut String,
) -> Result<(), &'static str> {
    line.clear();
    let read = reader
        .by_ref()
        .take((MAX_HEADER_BYTES - *header_bytes) as u64)
        .read_line(line)
        .map_err(|_| "400 Bad Request")?;
    *header_bytes += read;
    if read == 0 || !line.ends_with('\n') {
        return Err("431 Request Header Fields Too Large");
    }
    Ok(())
}

/// The model calls in one export: each completed Codex response that reported
/// its token counts, in the order Codex recorded them.
fn calls_from_export(body: &[u8], last_completed_at: &mut Option<String>) -> Vec<String> {
    let Ok(export) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    let records = export
        .get("resourceLogs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|resource| resource.get("scopeLogs")?.as_array())
        .flatten()
        .filter_map(|scope| scope.get("logRecords")?.as_array())
        .flatten();
    let mut calls = Vec::new();
    for record in records {
        let attributes = attributes(record);
        let text = |key: &str| attributes.get(key).and_then(Value::as_str);
        let count = |key: &str| attributes.get(key).and_then(Value::as_u64);
        if text("event.name") != Some("codex.sse_event")
            || text("event.kind") != Some("response.completed")
        {
            continue;
        }
        let (Some(input), Some(output)) = (count("input_token_count"), count("output_token_count"))
        else {
            continue;
        };
        let cached = count("cached_token_count").unwrap_or(0).min(input);
        let mut usage = json!({
            "input_tokens": input,
            "cached_input_tokens": cached,
            "output_tokens": output,
            "total_tokens": input.saturating_add(output)
        });
        if let Some(reasoning) = count("reasoning_token_count") {
            usage["reasoning_output_tokens"] = json!(reasoning);
        }
        if let Some(tier) = text("service_tier") {
            usage["service_tier"] = json!(tier);
        }
        let mut call = json!({"type": "model_call.completed", "usage": usage});
        if let Some(model) = text("model") {
            call["model"] = json!(model);
        }
        if let Some(started) = last_completed_at.take() {
            call["started_at"] = json!(started);
        }
        if let Some(at) = text("event.timestamp") {
            call["completed_at"] = json!(at);
            *last_completed_at = Some(at.to_owned());
        }
        calls.push(call.to_string());
    }
    calls
}

/// A log record's attributes as plain JSON values. OTLP/JSON encodes 64-bit
/// integers as strings, so integer values are parsed back into numbers.
fn attributes(record: &Value) -> Map<String, Value> {
    record
        .get("attributes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|attribute| {
            let key = attribute.get("key")?.as_str()?;
            let value = attribute.get("value")?;
            let value = if let Some(text) = value.get("stringValue") {
                text.clone()
            } else if let Some(integer) = value.get("intValue") {
                match integer {
                    Value::String(text) => json!(text.parse::<u64>().ok()?),
                    other => other.clone(),
                }
            } else {
                value
                    .get("doubleValue")
                    .or_else(|| value.get("boolValue"))?
                    .clone()
            };
            Some((key.to_owned(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute(key: &str, value: Value) -> Value {
        json!({"key": key, "value": value})
    }

    /// An export shaped like codex-cli 0.144's `codex_otel.log_only` scope.
    fn export(records: Vec<Value>) -> Vec<u8> {
        json!({"resourceLogs": [{
            "resource": {"attributes": [attribute("service.name", json!({"stringValue": "codex_exec"}))]},
            "scopeLogs": [{"scope": {"name": "codex_otel.log_only"}, "logRecords": records}]
        }]})
        .to_string()
        .into_bytes()
    }

    fn response(at: &str, input: &str, cached: u64, output: u64) -> Value {
        json!({"timeUnixNano": "0", "attributes": [
            attribute("event.name", json!({"stringValue": "codex.sse_event"})),
            attribute("event.kind", json!({"stringValue": "response.completed"})),
            attribute("input_token_count", json!({"intValue": input})),
            attribute("cached_token_count", json!({"intValue": cached})),
            attribute("output_token_count", json!({"intValue": output})),
            attribute("reasoning_token_count", json!({"intValue": "12"})),
            attribute("service_tier", json!({"stringValue": "default"})),
            attribute("event.timestamp", json!({"stringValue": at})),
            attribute("conversation.id", json!({"stringValue": "01a1c937-5c0a-7100-bd2b-746c8067e68f"})),
            attribute("user.email", json!({"stringValue": "someone@example.com"})),
            attribute("model", json!({"stringValue": "gpt-5.6-sol"}))
        ]})
    }

    #[test]
    fn completed_responses_become_calls_and_nothing_else_is_kept() {
        let failed = json!({"attributes": [
            attribute("event.name", json!({"stringValue": "codex.sse_event"})),
            attribute("event.kind", json!({"stringValue": "response.completed"})),
            attribute("error.message", json!({"stringValue": "400"}))
        ]});
        let other = json!({"attributes": [
            attribute("event.name", json!({"stringValue": "codex.user_prompt"})),
            attribute("prompt", json!({"stringValue": "[REDACTED]"}))
        ]});
        let mut last = None;
        let calls = calls_from_export(
            &export(vec![
                other,
                response("2026-11-11T06:06:53.009Z", "25838", 13_056, 194),
                failed,
                response("2026-11-11T06:06:57.120Z", "27585", 13_056, 79),
            ]),
            &mut last,
        )
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();

        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            json!({
                "type": "model_call.completed",
                "model": "gpt-5.6-sol",
                "completed_at": "2026-11-11T06:06:53.009Z",
                "usage": {
                    "input_tokens": 25_838,
                    "cached_input_tokens": 13_056,
                    "output_tokens": 194,
                    "reasoning_output_tokens": 12,
                    "total_tokens": 26_032,
                    "service_tier": "default"
                }
            }),
            "account details and session ids are not kept"
        );
        assert_eq!(calls[1]["started_at"], "2026-11-11T06:06:53.009Z");
        assert_eq!(last.as_deref(), Some("2026-11-11T06:06:57.120Z"));
        assert!(calls_from_export(b"not json", &mut None).is_empty());
    }

    fn post(receiver: &CodexUsageReceiver, token: &str, body: &[u8]) -> String {
        let mut stream = TcpStream::connect(receiver.address).unwrap();
        write!(
            stream,
            "POST /v1/logs HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n{TOKEN_HEADER}: {token}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn the_receiver_takes_exports_that_carry_its_token() {
        let receiver = CodexUsageReceiver::start().unwrap();
        let body = export(vec![response("2026-11-11T06:06:53.009Z", "100", 40, 25)]);

        let refused = post(&receiver, "wrong", &body);
        assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
        assert!(receiver.take_calls().is_empty());

        let before = Instant::now();
        let accepted = post(&receiver, &receiver.token, &body);
        assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");
        assert!(accepted.ends_with("{}"));
        let calls = receiver.settle(before);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("\"input_tokens\":100"));

        let args = receiver.codex_args(&format!("127.0.0.1:{}", receiver.port()));
        assert_eq!(args[0], "-c");
        assert!(args[1].starts_with(&format!(
            "otel.exporter={{otlp-http={{endpoint=\"http://127.0.0.1:{}/v1/logs\",protocol=\"json\"",
            receiver.port()
        )));
        assert!(args[1].contains(&receiver.token));
    }

    #[test]
    fn settling_gives_up_when_codex_exports_nothing_more() {
        let receiver = CodexUsageReceiver::start().unwrap();
        let started = Instant::now();
        assert!(receiver.settle(Instant::now()).is_empty());
        assert!(started.elapsed() >= SETTLE_TIMEOUT);
    }
}
