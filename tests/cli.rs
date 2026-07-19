use std::process::Command;
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};

#[test]
fn version_reports_package_name_and_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_rustgrid-agent"))
        .arg("--version")
        .output()
        .expect("rustgrid-agent should run");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("version output should be UTF-8"),
        format!("rustgrid-agent {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn login_and_logout_complete_the_device_credential_lifecycle() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let config = directory.path().join("agent.json");
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("device login server should bind: {error}"),
    };
    let address = listener.local_addr().unwrap();
    let verification_uri = format!("http://{address}/device");
    let start_response = format!(
        r#"{{"device_code":"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG","user_code":"ABCD-EFGH","verification_uri":"{verification_uri}","verification_uri_complete":"{verification_uri}?code=ABCD-EFGH","expires_in":30,"interval":1}}"#
    );
    let server = thread::spawn(move || {
        for (expected_path, body) in [
            ("/api/v1/agent-workers/device-authorizations ", start_response),
            (
                "/api/v1/agent-workers/device-authorizations/token ",
                r#"{"access_token":"test-worker-credential-00000000000000000000000000000000","token_type":"Bearer","expires_in":2592000,"worker":{"id":"00000000-0000-4000-8000-000000000001","name":"test-worker","tenant_id":"00000000-0000-4000-8000-000000000002"},"instance":{"url":"http://127.0.0.1/device"},"scopes":["agents:workers:heartbeat"]}"#.into(),
            ),
            (
                "/api/v1/agent-workers/00000000-0000-4000-8000-000000000001/credentials/current/revoke ",
                r#"{"revoked":true}"#.into(),
            ),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert!(request.starts_with(&format!("POST {expected_path}")), "{request}");
            if expected_path.contains("device-authorizations/token") {
                assert!(request.contains("\"client_id\":\"rustgrid-agent\""));
            }
            if expected_path.contains("credentials/current/revoke") {
                assert!(request.contains("authorization: Bearer test-worker-credential-"));
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    });

    let output = Command::new(env!("CARGO_BIN_EXE_rustgrid-agent"))
        .current_dir(directory.path())
        .env("RUSTGRID_API_URL", format!("http://{address}"))
        .env_remove("RUSTGRID_WORKER_API_KEY")
        .env_remove("RUSTGRID_WORKER_ID")
        .env("RUSTGRID_CREDENTIAL_STORE", "file")
        .env(
            "RUSTGRID_CREDENTIALS_DIR",
            directory.path().join("credentials"),
        )
        .args([
            "--config",
            config.to_str().unwrap(),
            "login",
            "--no-browser",
        ])
        .output()
        .expect("rustgrid-agent login should run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ABCD-EFGH"));
    assert!(!stdout.contains("test-worker-credential-"));
    assert!(!directory.path().join("agent.json.credentials").exists());
    let stored_config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(
        stored_config["worker_id"],
        "00000000-0000-4000-8000-000000000001"
    );
    assert_eq!(
        stored_config["tenant_id"],
        "00000000-0000-4000-8000-000000000002"
    );
    assert_eq!(stored_config["max_concurrency"], 1);
    assert_eq!(stored_config["executor"]["kind"], "docker_sandbox");
    assert!(
        stored_config["credential_expires_at_unix"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(stored_config.get("api_key").is_none());
    let credential_files = fs::read_dir(directory.path().join("credentials"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(credential_files.len(), 1);

    let logout = Command::new(env!("CARGO_BIN_EXE_rustgrid-agent"))
        .current_dir(directory.path())
        .env("RUSTGRID_API_URL", format!("http://{address}"))
        .env_remove("RUSTGRID_WORKER_API_KEY")
        .env_remove("RUSTGRID_WORKER_ID")
        .env("RUSTGRID_CREDENTIAL_STORE", "file")
        .env(
            "RUSTGRID_CREDENTIALS_DIR",
            directory.path().join("credentials"),
        )
        .args(["--config", config.to_str().unwrap(), "logout"])
        .output()
        .expect("rustgrid-agent logout should run");
    server.join().unwrap();
    assert!(
        logout.status.success(),
        "{}",
        String::from_utf8_lossy(&logout.stderr)
    );
    assert!(String::from_utf8_lossy(&logout.stdout).contains("credential revoked"));
    assert_eq!(
        fs::read_dir(directory.path().join("credentials"))
            .unwrap()
            .count(),
        0
    );
    let logged_out_config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert!(logged_out_config["credential_expires_at_unix"].is_null());
    assert!(logged_out_config["worker_id"].is_null());
    assert!(logged_out_config.get("api_key").is_none());
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).unwrap();
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(str::to_owned)
            })
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_default();
        if request.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8(request).unwrap()
}

#[test]
fn login_reports_an_incompatible_server_without_falling_back_to_registration() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let config = directory.path().join("agent.json");
    fs::write(&config, r#"{"max_concurrency":1}"#).expect("configuration should be written");
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("compatibility server should bind: {error}"),
    };
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request).unwrap();
        let body = r#"{"error":"not_found"}"#;
        write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_rustgrid-agent"))
        .current_dir(directory.path())
        .env("RUSTGRID_API_URL", format!("http://{address}"))
        .env("RUSTGRID_CREDENTIAL_STORE", "file")
        .env(
            "RUSTGRID_CREDENTIALS_DIR",
            directory.path().join("credentials"),
        )
        .args([
            "--config",
            config.to_str().unwrap(),
            "login",
            "--no-browser",
        ])
        .output()
        .expect("rustgrid-agent login should run");
    server.join().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("does not support worker device authentication")
    );
}

#[test]
fn status_can_emit_machine_readable_health() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let config = directory.path().join("agent.json");
    fs::write(&config, r#"{"max_concurrency":1}"#).expect("configuration should be written");
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("health server should bind: {error}"),
    };
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let body = r#"{"items":[{"id":"00000000-0000-4000-8000-000000000001","name":"test-worker","status":"online","last_seen_at":"2026-07-18T12:00:00Z","agent_version":"0.1.0"}]}"#;
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_rustgrid-agent"))
        .current_dir(directory.path())
        .env("RUSTGRID_WORKER_API_KEY", "test-key")
        .env("RUSTGRID_WORKER_ID", "00000000-0000-4000-8000-000000000001")
        .env("RUSTGRID_API_URL", format!("http://{address}"))
        .args(["--config", config.to_str().unwrap(), "status", "--json"])
        .output()
        .expect("rustgrid-agent status should run");
    server.join().unwrap();
    assert!(!output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("status should emit JSON");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["agent_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["healthy"], false);
    assert_eq!(value["max_concurrency"], 1);
    assert_eq!(value["executor"], "local");
    assert_eq!(value["executor_ready"], true);
    assert_eq!(value["production_safe_concurrency"], true);
    assert_eq!(value["rustgrid_reachable"], true);
    assert_eq!(value["scope"], "tenant");
    assert_eq!(value["credential_expired"], false);
    assert!(value.get("credential_expires_at_unix").is_some());
}

#[test]
fn serve_fails_closed_with_local_executor() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let config = directory.path().join("agent.json");
    fs::write(&config, r#"{"max_concurrency":1}"#).expect("configuration should be written");
    let output = Command::new(env!("CARGO_BIN_EXE_rustgrid-agent"))
        .current_dir(directory.path())
        .env("RUSTGRID_WORKER_API_KEY", "test-key")
        .args(["--config", config.to_str().unwrap(), "serve"])
        .output()
        .expect("rustgrid-agent serve should run");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("executor.kind=docker_sandbox"));
}

#[test]
fn local_executor_rejects_shared_process_concurrency() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let config = directory.path().join("agent.json");
    std::fs::write(&config, r#"{"max_concurrency":2}"#).expect("configuration should be written");
    let output = Command::new(env!("CARGO_BIN_EXE_rustgrid-agent"))
        .current_dir(directory.path())
        .env("RUSTGRID_WORKER_API_KEY", "test-key")
        .args(["--config", config.to_str().unwrap(), "serve"])
        .output()
        .expect("rustgrid-agent serve should run");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("local executor requires max_concurrency=1")
    );
}

#[test]
fn watch_once_fails_closed_with_multiple_run_capacity() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let config = directory.path().join("agent.json");
    fs::write(&config, r#"{"max_concurrency":2}"#).expect("configuration should be written");
    let output = Command::new(env!("CARGO_BIN_EXE_rustgrid-agent"))
        .current_dir(directory.path())
        .env("RUSTGRID_WORKER_API_KEY", "test-key")
        .args(["--config", config.to_str().unwrap(), "watch", "--once"])
        .output()
        .expect("rustgrid-agent watch should run");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("max_concurrency=1"));
}
