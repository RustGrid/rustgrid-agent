use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::mpsc,
    thread,
};

use rustgrid_agent::{
    api::RustGridClient,
    config::{AppContext, Config, RepoConfig},
    lifecycle::{LifecycleEvent, RunPhase},
};
use serde_json::json;

fn context(base_url: String) -> AppContext {
    AppContext {
        config: Config {
            project_id: None,
            project_key: Some("RG".into()),
            repo: RepoConfig {
                owner: "RustGrid".into(),
                name: "example".into(),
            },
            default_base_branch: "main".into(),
            quality_gate_command: "cargo test".into(),
            codex_command: None,
            heartbeat_interval_seconds: 15,
            lease_seconds: 900,
            workspace_root: None,
            command_timeout_seconds: 1800,
            run_timeout_seconds: 7200,
            failed_workspace_retention_hours: 72,
            max_command_output_bytes: 8 * 1024 * 1024,
            max_workspace_bytes: 5 * 1024 * 1024 * 1024,
        },
        config_path: PathBuf::from("test.json"),
        api_url: base_url,
        api_key: Some("rgk_test".into()),
        codex_command: "codex exec -".into(),
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

#[test]
fn retrieves_the_run_manifest_contract() {
    let Some((url, request)) = server(json!({
        "manifest_version": 1,
        "run": {"id": "run-1", "ticket_id": "ticket-1"},
        "project_id": "project-1", "project_key": "RG", "project_name": "RustGrid",
        "ticket_id": "ticket-1", "ticket_key": "RG-1", "ticket_title": "Task",
        "repository_id": 7, "repository": "RustGrid/example",
        "clone_url": "https://github.com/RustGrid/example.git", "web_base_url": "https://github.com",
        "installation_id": 42, "default_branch": "main",
        "required_workflows": [], "required_permissions": {}
    })) else {
        return;
    };
    let manifest = RustGridClient::new(&context(url))
        .unwrap()
        .execution_manifest("run-1")
        .unwrap();
    assert_eq!(manifest.repository, "RustGrid/example");
    assert!(
        request
            .recv()
            .unwrap()
            .starts_with("GET /agent-runs/run-1/manifest HTTP/1.1")
    );
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
