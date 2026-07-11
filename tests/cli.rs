use std::fs;
use std::process::Command;

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
fn status_can_emit_machine_readable_health() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let config = directory.path().join("agent.json");
    fs::write(
        &config,
        r#"{"project_key":"RG","project_id":null,"max_concurrency":2}"#,
    )
    .expect("configuration should be written");
    let output = Command::new(env!("CARGO_BIN_EXE_rustgrid-agent"))
        .current_dir(directory.path())
        .env("RUSTGRID_API_KEY", "test-key")
        .env("RUSTGRID_AGENT_ISOLATION", "per_run")
        .args(["--config", config.to_str().unwrap(), "status", "--json"])
        .output()
        .expect("rustgrid-agent status should run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("status should emit JSON");
    assert_eq!(value["healthy"], true);
    assert_eq!(value["max_concurrency"], 2);
}

#[test]
fn serve_fails_closed_without_per_run_isolation() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let config = directory.path().join("agent.json");
    fs::write(
        &config,
        r#"{"project_key":"RG","project_id":null,"max_concurrency":1}"#,
    )
    .expect("configuration should be written");
    let output = Command::new(env!("CARGO_BIN_EXE_rustgrid-agent"))
        .current_dir(directory.path())
        .env("RUSTGRID_API_KEY", "test-key")
        .env_remove("RUSTGRID_AGENT_ISOLATION")
        .args(["--config", config.to_str().unwrap(), "serve"])
        .output()
        .expect("rustgrid-agent serve should run");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("RUSTGRID_AGENT_ISOLATION=per_run"));
}
