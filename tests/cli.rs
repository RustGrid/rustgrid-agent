use std::process::Command;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
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
fn login_completes_device_flow_and_saves_worker_credentials() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let config = directory.path().join("agent.json");
    fs::write(&config, r#"{"max_concurrency":1}"#).expect("configuration should be written");
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("device login server should bind: {error}"),
    };
    let address = listener.local_addr().unwrap();
    let verification_uri = format!("http://{address}/device");
    let start_response = format!(
        r#"{{"device_code":"secret-device-code","user_code":"ABCD-EFGH","verification_uri":"{verification_uri}","expires_in":30,"interval":1}}"#
    );
    let server = thread::spawn(move || {
        for body in [
            start_response,
            r#"{"worker_id":"00000000-0000-4000-8000-000000000001","api_key":"rgk_test"}"#.into(),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let count = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.starts_with("POST /agent-workers/device-authorization"));
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
        .args([
            "--config",
            config.to_str().unwrap(),
            "login",
            "--no-browser",
        ])
        .output()
        .expect("rustgrid-agent login should run");
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ABCD-EFGH"));
    assert!(!stdout.contains("rgk_test"));
    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join("agent.json.credentials")).unwrap())
            .unwrap();
    assert_eq!(stored["worker_id"], "00000000-0000-4000-8000-000000000001");
    assert_eq!(stored["api_key"], "rgk_test");
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
        for body in [
            r#"{"id":"00000000-0000-4000-8000-000000000001","status":"online"}"#,
            r#"{"items":[],"page":1,"size":100,"total":0}"#,
        ] {
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
        }
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
    assert_eq!(value["healthy"], false);
    assert_eq!(value["max_concurrency"], 1);
    assert_eq!(value["executor"], "local");
    assert_eq!(value["executor_ready"], true);
    assert_eq!(value["production_safe_concurrency"], true);
    assert_eq!(value["rustgrid_reachable"], true);
    assert_eq!(value["scope"], "tenant");
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
