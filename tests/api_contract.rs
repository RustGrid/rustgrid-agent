use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::mpsc,
    thread,
    time::Duration,
};

use rustgrid_agent::{
    api::RustGridClient,
    config::{AppContext, Config, RepoConfig},
    lifecycle::{LifecycleEvent, RunPhase},
    telemetry::{
        ExecutionSnapshot, ExecutionStatus, TelemetryBatch, TelemetryEvent, TelemetryPayload,
    },
    token_consumption::TokenConsumption,
};
use serde_json::json;
use uuid::Uuid;

#[test]
fn model_call_usage_contract_exposes_sanitized_component_metadata() {
    let spec: serde_json::Value =
        serde_json::from_str(include_str!("../openapi.current.json")).expect("valid OpenAPI JSON");
    let schema = &spec["components"]["schemas"]["ModelCallUsageResponse"];
    assert!(schema["properties"]["provider_usage_payload"].is_object());
    assert!(
        schema["required"]
            .as_array()
            .expect("required fields")
            .iter()
            .any(|field| field == "provider_usage_payload")
    );
}

fn context(base_url: String) -> AppContext {
    AppContext {
        config: Config {
            instance_url: Some(base_url.clone()),
            installation_id: Some("00000000-0000-4000-8000-000000000099".into()),
            worker_id: Some("00000000-0000-4000-8000-000000000001".into()),
            tenant_id: Some("00000000-0000-4000-8000-000000000002".into()),
            worker_name: Some("contract-worker".into()),
            credential_store: Some("private_file_fallback".into()),
            credential_expires_at_unix: None,
            project_id: None,
            project_key: None,
            repo: Some(RepoConfig {
                owner: "RustGrid".into(),
                name: "example".into(),
            }),
            default_base_branch: "main".into(),
            quality_gate_command: None,
            codex_command: None,
            heartbeat_interval_seconds: 15,
            max_concurrency: 1,
            executor: rustgrid_agent::config::ExecutorConfig::Local,
            lease_seconds: 900,
            workspace_root: None,
            command_timeout_seconds: 1800,
            run_timeout_seconds: 7200,
            failed_workspace_retention_hours: 72,
            max_command_output_bytes: 8 * 1024 * 1024,
            max_workspace_bytes: 5 * 1024 * 1024 * 1024,
            max_child_memory_bytes: 8 * 1024 * 1024 * 1024,
            max_child_file_bytes: 1024 * 1024 * 1024,
            max_child_open_files: 1024,
        },
        config_path: PathBuf::from("test.json"),
        instance_url: base_url.clone(),
        api_url: base_url.clone(),
        api_key: Some("rgk_test".into()),
        worker_id: Some("00000000-0000-4000-8000-000000000001".into()),
        tenant_id: Some("00000000-0000-4000-8000-000000000002".into()),
        worker_name: Some("contract-worker".into()),
        installation_id: "00000000-0000-4000-8000-000000000099".into(),
        credential_source: rustgrid_agent::credentials::CredentialSource::FallbackFile,
        credential_expires_at_unix: None,
        credential_store: rustgrid_agent::credentials::CredentialStore::new(
            &base_url,
            "00000000-0000-4000-8000-000000000099",
        )
        .unwrap(),
        credentials_path: PathBuf::from("test.json.credentials"),
        workspace_root: PathBuf::from("/tmp/rustgrid-agent-tests"),
    }
}

fn server(response_body: serde_json::Value) -> Option<(String, mpsc::Receiver<String>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(error) => panic!("could not bind contract-test server: {error}"),
    };
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&bytes);
            let Some(header_end) = text.find("\r\n\r\n") else {
                continue;
            };
            let content_length = text[..header_end]
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|v| v.parse::<usize>().ok())
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + content_length {
                break;
            }
        }
        sender
            .send(String::from_utf8_lossy(&bytes).into_owned())
            .unwrap();
        let body = response_body.to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
    });
    Some((format!("http://{address}"), receiver))
}

fn retrying_server(
    response_body: serde_json::Value,
) -> Option<(String, mpsc::Receiver<Vec<String>>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(error) => panic!("could not bind retry contract-test server: {error}"),
    };
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut requests = Vec::new();
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                if String::from_utf8_lossy(&bytes).contains("\r\n\r\n") {
                    break;
                }
            }
            requests.push(String::from_utf8_lossy(&bytes).into_owned());
            if attempt == 0 {
                write!(
                    stream,
                    "HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            } else {
                let body = response_body.to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        }
        sender.send(requests).unwrap();
    });
    Some((format!("http://{address}"), receiver))
}

fn header_value<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (header, value) = line.split_once(':')?;
        header.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

#[test]
fn retrieves_the_run_manifest_contract() {
    let Some((url, request)) = server(json!({
        "manifest_version": 2,
        "run": {
            "id": "run-1", "ticket_id": "ticket-1", "attempt": 2,
            "input_prompt": "Use the attached screenshot.",
            "metadata": {"resume_from_run_id": "run-previous"}
        },
        "attachments": [{
            "id": "11111111-1111-4111-8111-111111111111",
            "ticket_id": "ticket-1",
            "filename": "failure.png",
            "mime": "image/png",
            "media_family": "image",
            "size_bytes": 4,
            "sha256": "07".repeat(32),
            "status": "ready",
            "virus_status": "clean",
            "variants": []
        }],
        "project_id": "project-1", "project_key": "RG", "project_name": "RustGrid",
        "ticket_id": "ticket-1", "ticket_key": "RG-1", "ticket_title": "Task",
        "repository_id": 7, "repository": "RustGrid/example",
        "clone_url": "https://github.com/RustGrid/example.git", "web_base_url": "https://github.com",
        "installation_id": 42, "default_branch": "main",
        "required_workflows": [], "required_permissions": {},
        "execution_policy": {
          "policy_version": 1,
          "codex": {"command":["codex","exec","--json"],"environment_allowlist":["PATH","HOME"]},
          "quality_gates": [], "timeout_seconds": 3600,
          "sandbox":{"mode":"workspace_write","network_access":true,"writable_roots":["."],"approval_policy":"never"}
        },
        "execution_policy_sha256": "unused-by-deserialization-test"
    })) else {
        return;
    };
    let manifest = RustGridClient::new(&context(url))
        .unwrap()
        .execution_manifest("run-1")
        .unwrap();
    assert_eq!(manifest.repository, "RustGrid/example");
    assert_eq!(manifest.attachments[0].filename, "failure.png");
    assert_eq!(manifest.resume_from_run_id().unwrap(), Some("run-previous"));
    assert!(
        request
            .recv()
            .unwrap()
            .starts_with("GET /agent-runs/run-1/manifest HTTP/1.1")
    );
}

#[test]
fn heartbeat_advertises_configured_capacity() {
    let Some((url, request)) = server(json!({})) else {
        return;
    };
    RustGridClient::new(&context(url))
        .unwrap()
        .heartbeat("worker-1")
        .unwrap();
    let request = request.recv().unwrap();
    assert!(request.starts_with("POST /agent-workers/worker-1/heartbeat HTTP/1.1"));
    assert!(request.contains("\"max_concurrency\":1"));
}

#[test]
fn reports_final_run_token_consumption() {
    let Some((url, request)) = server(json!({})) else {
        return;
    };
    RustGridClient::new(&context(url))
        .unwrap()
        .report_token_consumption(
            "run-1",
            TokenConsumption {
                input_tokens: 100,
                cached_input_tokens: 40,
                output_tokens: 25,
            },
        )
        .unwrap();
    let request = request.recv().unwrap();
    assert!(request.starts_with("PUT /agent-runs/run-1/token-consumption HTTP/1.1"));
    assert!(request.contains("\"provider\":\"codex\""));
    assert!(request.contains("\"input_tokens\":100"));
    assert!(request.contains("\"cached_input_tokens\":40"));
    assert!(request.contains("\"output_tokens\":25"));
    assert!(request.contains("\"total_tokens\":125"));
    assert_eq!(
        header_value(&request, "idempotency-key"),
        Some("token-consumption-run-1")
    );
}

#[test]
fn reports_detailed_run_telemetry_to_the_ingestion_endpoint() {
    let Some((url, request)) = server(json!({
        "accepted": 1, "duplicates": 0, "stale": 0, "latest_sequence": 1
    })) else {
        return;
    };
    let execution_id = Uuid::parse_str("00000000-0000-4000-8000-000000000010").unwrap();
    let batch = TelemetryBatch::new(vec![TelemetryEvent {
        event_id: Uuid::parse_str("00000000-0000-4000-8000-000000000011").unwrap(),
        entity_revision: 1,
        occurred_at: "2026-07-20T12:00:00.000Z".into(),
        event_type: "execution.started".into(),
        payload: TelemetryPayload::Execution {
            execution: ExecutionSnapshot {
                id: execution_id,
                agent_id: None,
                agent_name: Some("Codex".into()),
                role: Some("implementation".into()),
                started_at: "2026-07-20T12:00:00.000Z".into(),
                completed_at: None,
                status: ExecutionStatus::Running,
            },
        },
    }]);
    RustGridClient::new(&context(url))
        .unwrap()
        .report_telemetry_batch("run-1", &batch)
        .unwrap();

    let request = request.recv().unwrap();
    assert!(request.starts_with("POST /agent-runs/run-1/telemetry/batch HTTP/1.1"));
    assert!(request.contains("\"telemetry_version\":\"1.0\""));
    assert!(request.contains("\"type\":\"execution.started\""));
    assert!(request.contains("\"execution\":"));
    assert!(header_value(&request, "idempotency-key").is_some());
}

#[test]
fn replays_the_durable_worker_queue_contract() {
    let Some((url, request)) = server(json!({
      "worker": {"id":"00000000-0000-4000-8000-000000000001","status":"online","max_concurrency":2,"active_runs":1,"available_slots":1},
      "items": [{
        "sequence":7,
        "event_type":"work_claimed",
        "run_id":"00000000-0000-4000-8000-000000000003",
        "project_id":"00000000-0000-4000-8000-000000000004",
        "ticket_id":"00000000-0000-4000-8000-000000000002",
        "worker_id":"00000000-0000-4000-8000-000000000001",
        "data":{},
        "created_at":"2026-07-13T12:00:00Z"
      }],
      "next_sequence": 7
    })) else {
        return;
    };
    let queue = RustGridClient::new(&context(url))
        .unwrap()
        .queue_events("worker-1", 3)
        .unwrap();
    assert_eq!(queue.next_sequence, 7);
    assert_eq!(queue.worker.available_slots, 1);
    assert_eq!(queue.items[0].event_type.as_str(), "work_claimed");
    assert_eq!(
        queue.items[0].run_id.as_deref(),
        Some("00000000-0000-4000-8000-000000000003")
    );
    assert_eq!(
        queue.items[0].project_id.as_deref(),
        Some("00000000-0000-4000-8000-000000000004")
    );
    assert!(
        request
            .recv()
            .unwrap()
            .starts_with("GET /agent-workers/worker-1/queue?after_sequence=3&limit=500 HTTP/1.1")
    );
}

#[test]
fn queue_stream_keepalive_wakes_the_coordinator_without_an_error() {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("could not bind keepalive contract-test server: {error}"),
    };
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request).unwrap();
        let body = ": keepalive\n\n";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });

    let event = RustGridClient::new(&context(format!("http://{address}")))
        .unwrap()
        .wait_for_queue_event("worker-1", 0, Duration::from_secs(1))
        .unwrap();

    server.join().unwrap();
    assert_eq!(event, None);
}

#[test]
fn issues_a_bodyless_github_token_request() {
    let Some((url, request)) = server(json!({
        "token": "ghs_secret", "expires_at": "2026-07-11T12:00:00Z",
        "repository": "RustGrid/example", "permissions": {"contents": "write"}
    })) else {
        return;
    };
    let token = RustGridClient::new(&context(url))
        .unwrap()
        .issue_github_token("run-1")
        .unwrap();
    assert_eq!(token.repository, "RustGrid/example");
    let request = request.recv().unwrap();
    assert!(request.starts_with("POST /agent-runs/run-1/github-token HTTP/1.1"));
    assert!(!request.to_ascii_lowercase().contains("content-length:"));
}

#[test]
fn retries_github_token_gateway_timeouts_with_one_idempotency_key() {
    let Some((url, requests)) = retrying_server(json!({
        "token": "ghs_secret", "expires_at": "2026-07-11T12:00:00Z",
        "repository": "RustGrid/example", "permissions": {"contents": "write"}
    })) else {
        return;
    };
    RustGridClient::new(&context(url))
        .unwrap()
        .issue_github_token("run-1")
        .unwrap();
    let requests = requests.recv().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        header_value(&requests[0], "idempotency-key"),
        header_value(&requests[1], "idempotency-key")
    );
    assert!(header_value(&requests[0], "idempotency-key").is_some());
}

#[test]
fn publishes_a_sequenced_progress_event() {
    let Some((url, request)) = server(json!({
        "sequence": 9, "run_id": "run-1", "event_type": "progress",
        "data": {"sequence": 4}, "created_at": "2026-07-11T12:00:00Z"
    })) else {
        return;
    };
    let event = LifecycleEvent::new(
        4,
        RunPhase::Executing,
        "step.codex.running",
        "info",
        "Running",
        None,
    );
    let published = RustGridClient::new(&context(url))
        .unwrap()
        .publish_run_event("run-1", "progress", &event)
        .unwrap();
    assert_eq!(published.sequence, 9);
    let request = request.recv().unwrap();
    assert!(request.starts_with("POST /agent-runs/run-1/events HTTP/1.1"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("idempotency-key: progress-run-1-4")
    );
    assert!(request.contains("\"event_type\":\"progress\""));
}

#[test]
fn lists_tenant_wide_assigned_runs_for_worker_recovery() {
    let Some((url, request)) = server(json!({
        "items": [{
            "id": "run-1", "ticket_id": "ticket-1", "project_id": "project-1",
            "worker_id": "worker-1", "status": "running", "row_version": 3
        }],
        "page": 1, "size": 100, "total": 1
    })) else {
        return;
    };
    let runs = RustGridClient::new(&context(url))
        .unwrap()
        .active_runs("worker-1")
        .unwrap();
    assert_eq!(runs[0].id, "run-1");
    let request = request.recv().unwrap();
    assert!(
        request.starts_with(
            "GET /agent-workers/worker-1/runs?status=running&page=1&size=100 HTTP/1.1"
        )
    );
}

#[test]
fn rejects_worker_recovery_records_outside_the_bound_worker() {
    let Some((url, _request)) = server(json!({
        "items": [{
            "id": "run-1", "ticket_id": "ticket-1", "project_id": "project-1",
            "worker_id": "different-worker", "status": "running", "row_version": 3
        }],
        "page": 1, "size": 100, "total": 1
    })) else {
        return;
    };

    let error = RustGridClient::new(&context(url))
        .unwrap()
        .active_runs("worker-1")
        .unwrap_err();
    assert!(error.to_string().contains("outside worker worker-1"));
}

#[test]
fn lifecycle_side_effects_use_stable_idempotency_keys() {
    let Some((url, request)) = server(json!({})) else {
        return;
    };
    RustGridClient::new(&context(url))
        .unwrap()
        .create_comment("ticket-1", "progress", "agent-comment-run-1-message")
        .unwrap();
    let request = request.recv().unwrap().to_ascii_lowercase();
    assert!(request.contains("idempotency-key: agent-comment-run-1-message"));

    let Some((url, request)) = server(json!({})) else {
        return;
    };
    RustGridClient::new(&context(url))
        .unwrap()
        .append_step(
            "run-1",
            7,
            "codex",
            rustgrid_agent::lifecycle::StepStatus::Completed,
            "done",
            None,
        )
        .unwrap();
    let request = request.recv().unwrap().to_ascii_lowercase();
    assert!(request.contains("idempotency-key: step-run-1-7-"));

    let Some((url, request)) = server(json!({
        "id": "run-1", "ticket_id": "ticket-1", "row_version": 4
    })) else {
        return;
    };
    RustGridClient::new(&context(url))
        .unwrap()
        .update_run(
            "run-1",
            3,
            rustgrid_agent::lifecycle::AgentRunStatus::Succeeded,
            Some("complete"),
        )
        .unwrap();
    let request = request.recv().unwrap().to_ascii_lowercase();
    assert!(request.contains("idempotency-key: run-status-run-1-succeeded-3"));
}
