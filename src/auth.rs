use std::{
    env,
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use reqwest::{StatusCode, blocking::Client};
use serde::{Deserialize, Serialize};

use crate::{config::AppContext, shutdown};

const DEVICE_PATH: &str = "agent-workers/device-authorizations";

#[derive(Debug, Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct DeviceCredential {
    access_token: String,
    token_type: String,
    worker: DeviceCredentialWorker,
    instance: DeviceCredentialInstance,
    scopes: Vec<String>,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct DeviceCredentialWorker {
    id: String,
    name: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize)]
struct DeviceCredentialInstance {
    url: String,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenError {
    error: String,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Serialize)]
struct StartRequest<'a> {
    client_id: &'a str,
    installation_id: &'a str,
    hostname: &'a str,
    display_name: Option<&'a str>,
    operating_system: &'a str,
    architecture: &'a str,
    agent_version: &'a str,
    requested_scopes: Vec<&'static str>,
}

fn default_interval() -> u64 {
    5
}

pub fn login(context: &mut AppContext, open_browser: bool) -> Result<()> {
    ctrlc::set_handler(shutdown::request).context("could not install Ctrl-C handler")?;
    let client = device_client()?;
    let endpoint = format!("/{DEVICE_PATH}");
    let endpoint = format!("{}{}", context.api_url.trim_end_matches('/'), endpoint);
    let hostname = hostname();
    let display_name = context.config.worker_name.as_deref();
    let response = client
        .post(&endpoint)
        .json(&StartRequest {
            client_id: "rustgrid-agent",
            installation_id: &context.installation_id,
            hostname: &hostname,
            display_name,
            operating_system: env::consts::OS,
            architecture: env::consts::ARCH,
            agent_version: env!("CARGO_PKG_VERSION"),
            requested_scopes: worker_scopes(),
        })
        .send()
        .context("could not start RustGrid device login")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if matches!(
            status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ) {
            bail!(
                "this RustGrid instance does not support worker device authentication; deploy a server that includes the worker device-auth API (migration 0047) or use managed environment credentials temporarily"
            );
        }
        bail!(
            "RustGrid rejected device login ({status}): {}",
            truncate(&body)
        );
    }
    let authorization: DeviceAuthorization = response
        .json()
        .context("RustGrid returned an invalid device login response")?;
    validate_authorization(&authorization)?;

    println!(
        "Your one-time RustGrid code is: {}",
        authorization.user_code
    );
    println!(
        "Authorize this worker at: {}",
        authorization.verification_uri
    );
    println!(
        "This code expires in {} minutes.",
        authorization.expires_in.div_ceil(60)
    );
    if open_browser && let Err(error) = open_url(&authorization.verification_uri_complete) {
        eprintln!("[warning] Could not open a browser: {error:#}");
        eprintln!(
            "Open {} manually and enter {}.",
            authorization.verification_uri, authorization.user_code
        );
    }

    let deadline = Instant::now() + Duration::from_secs(authorization.expires_in);
    let mut interval = authorization.interval.max(1);
    let token_endpoint = format!("{endpoint}/token");
    let mut transient_failures = 0_u32;
    loop {
        if shutdown::requested() {
            bail!("device login cancelled; no credential was stored");
        }
        if Instant::now() >= deadline {
            bail!("device login expired; run `rustgrid-agent login` again");
        }
        sleep_interruptibly(Duration::from_secs(interval), deadline)?;
        let response = match client
            .post(&token_endpoint)
            .json(&serde_json::json!({
                "client_id": "rustgrid-agent",
                "device_code": authorization.device_code,
            }))
            .send()
        {
            Ok(response) => response,
            Err(error) => {
                transient_failures = transient_failures.saturating_add(1);
                let retry = transient_backoff(transient_failures);
                eprintln!(
                    "[warning] RustGrid token polling failed ({error}); retrying in {retry}s"
                );
                interval = retry;
                continue;
            }
        };
        if response.status() == StatusCode::OK {
            let credential: DeviceCredential = response
                .json()
                .context("RustGrid returned an invalid device credential")?;
            validate_credential(&credential)?;
            context.save_login(
                &credential.access_token,
                &credential.worker.id,
                &credential.worker.tenant_id,
                &credential.worker.name,
                credential.expires_in,
            )?;
            println!(
                "[ complete] Worker {} ({}) is authenticated for tenant {} via {}",
                credential.worker.name,
                credential.worker.id,
                credential.worker.tenant_id,
                context.credential_source.as_str()
            );
            return Ok(());
        }
        if response.status().is_server_error() {
            transient_failures = transient_failures.saturating_add(1);
            interval = transient_backoff(transient_failures);
            eprintln!(
                "[warning] RustGrid is temporarily unavailable ({}); retrying in {}s",
                response.status(),
                interval
            );
            continue;
        }
        let status = response.status();
        let bytes = response.bytes().unwrap_or_default();
        let token_error: DeviceTokenError =
            serde_json::from_slice(&bytes).unwrap_or(DeviceTokenError {
                error: "invalid_response".to_owned(),
                interval: None,
            });
        transient_failures = 0;
        match token_error.error.as_str() {
            "authorization_pending" => {
                interval = token_error.interval.unwrap_or(interval).max(1);
            }
            "slow_down" => {
                interval = token_error
                    .interval
                    .unwrap_or_else(|| interval.saturating_add(5))
                    .max(interval);
            }
            "access_denied" => bail!("device login was denied in RustGrid AgentOps"),
            "expired_token" => {
                bail!("device login expired; run `rustgrid-agent login` again")
            }
            "consumed_token" => {
                bail!("device login was already consumed; run `rustgrid-agent login` again")
            }
            "invalid_device_code" => {
                bail!("RustGrid rejected the device code; run `rustgrid-agent login` again")
            }
            _ => bail!(
                "device login failed ({status}): {}",
                truncate(&String::from_utf8_lossy(&bytes))
            ),
        }
    }
}

pub fn logout(context: &mut AppContext) -> Result<()> {
    let Some(worker_id) = context.worker_id.clone() else {
        context.clear_login()?;
        println!("RustGrid worker is already logged out.");
        return Ok(());
    };
    let Some(api_key) = context.api_key.as_deref() else {
        eprintln!(
            "[warning] No local worker credential was found, so server-side revocation for {worker_id} could not be confirmed; revoke that worker in AgentOps if it is still active"
        );
        context.clear_login()?;
        println!("RustGrid worker is logged out locally.");
        return Ok(());
    };
    let client = device_client()?;
    let endpoint = format!(
        "{}/agent-workers/{worker_id}/credentials/current/revoke",
        context.api_url.trim_end_matches('/')
    );
    let response = client.post(endpoint).bearer_auth(api_key).send().context(
        "could not revoke the RustGrid worker credential; local credentials were retained",
    )?;
    match response.status() {
        status if status.is_success() => {}
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            eprintln!(
                "[warning] RustGrid no longer accepts this credential; removing the stale local copy"
            );
        }
        status => {
            let body = response.text().unwrap_or_default();
            bail!(
                "RustGrid could not revoke the worker credential ({status}); local credentials were retained: {}",
                truncate(&body)
            );
        }
    }
    context.clear_login()?;
    println!("[ complete] Worker {worker_id} credential revoked and removed locally");
    Ok(())
}

fn device_client() -> Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("rustgrid-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not initialize the RustGrid HTTP client")
}

fn validate_authorization(value: &DeviceAuthorization) -> Result<()> {
    if value.device_code.len() < 32
        || value.user_code.len() < 8
        || value.expires_in == 0
        || value.interval == 0
    {
        bail!("RustGrid returned an incomplete device login response");
    }
    let uri = reqwest::Url::parse(&value.verification_uri)
        .context("RustGrid returned an invalid verification URL")?;
    validate_verification_url(&uri)?;
    let complete = reqwest::Url::parse(&value.verification_uri_complete)
        .context("RustGrid returned an invalid complete verification URL")?;
    validate_verification_url(&complete)?;
    if uri.origin() != complete.origin() || !complete.path().ends_with("/device") {
        bail!("RustGrid returned inconsistent verification URLs");
    }
    Ok(())
}

fn validate_credential(value: &DeviceCredential) -> Result<()> {
    uuid::Uuid::parse_str(&value.worker.id).context("RustGrid returned an invalid worker ID")?;
    uuid::Uuid::parse_str(&value.worker.tenant_id)
        .context("RustGrid returned an invalid tenant ID")?;
    if value.access_token.len() < 32
        || value.token_type != "Bearer"
        || value.worker.name.trim().is_empty()
        || value.instance.url.trim().is_empty()
        || value.expires_in == 0
    {
        bail!("RustGrid returned an incomplete device credential");
    }
    if value.scopes.is_empty() {
        bail!("RustGrid returned a worker credential without scopes");
    }
    Ok(())
}

fn validate_verification_url(uri: &reqwest::Url) -> Result<()> {
    if uri.scheme() != "https"
        && uri.host_str() != Some("127.0.0.1")
        && uri.host_str() != Some("localhost")
        && uri.host_str() != Some("::1")
    {
        bail!("RustGrid verification URL must use HTTPS");
    }
    if !uri.username().is_empty() || uri.password().is_some() {
        bail!("RustGrid verification URL must not include credentials");
    }
    Ok(())
}

fn sleep_interruptibly(duration: Duration, deadline: Instant) -> Result<()> {
    let wake = Instant::now() + duration;
    while Instant::now() < wake.min(deadline) {
        if shutdown::requested() {
            bail!("device login cancelled; no credential was stored");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn transient_backoff(failures: u32) -> u64 {
    2_u64.saturating_pow(failures.min(4)).min(30)
}

fn hostname() -> String {
    env::var("HOSTNAME")
        .or_else(|_| env::var("COMPUTERNAME"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.chars().take(255).collect())
        .unwrap_or_else(|| "rustgrid-worker".to_owned())
}

fn worker_scopes() -> Vec<&'static str> {
    vec![
        "projects:read",
        "tickets:read",
        "tickets:update",
        "comments:read",
        "comments:create",
        "agents:workers:heartbeat",
        "agents:runs:read",
        "agents:runs:create",
        "agents:runs:claim",
        "agents:runs:update",
        "agents:steps:read",
        "agents:steps:create",
        "agents:steps:update",
        "agents:steps:delete",
        "agents:links:read",
        "agents:links:create",
        "agents:links:update",
        "agents:links:delete",
        "agents:quality_gates:read",
        "agents:quality_gates:create",
        "agents:quality_gates:update",
        "agents:quality_gates:delete",
    ]
}

fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut value = Command::new("cmd");
        value.args(["/C", "start", ""]);
        value
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");
    let status = command
        .arg(url)
        .status()
        .context("browser launcher is unavailable")?;
    if !status.success() {
        bail!("browser launcher exited with {status}");
    }
    Ok(())
}

fn truncate(value: &str) -> String {
    value.chars().take(1000).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_insecure_remote_verification_urls() {
        let value = DeviceAuthorization {
            device_code: "d".repeat(32),
            user_code: "ABCD-EFGH".into(),
            verification_uri: "http://example.com/device".into(),
            verification_uri_complete: "http://example.com/device?code=ABCD-EFGH".into(),
            expires_in: 60,
            interval: 1,
        };
        assert!(
            validate_authorization(&value)
                .unwrap_err()
                .to_string()
                .contains("HTTPS")
        );
    }

    #[test]
    fn backoff_is_bounded() {
        assert_eq!(transient_backoff(1), 2);
        assert_eq!(transient_backoff(10), 16);
    }

    #[test]
    fn worker_scopes_do_not_include_administrative_permissions() {
        let scopes = worker_scopes();
        assert!(!scopes.contains(&"agents:workers:register"));
        assert!(!scopes.contains(&"agents:workers:credentials"));
        assert!(!scopes.iter().any(|scope| scope.starts_with("api_keys:")));
    }
}
