use std::{
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use reqwest::{StatusCode, blocking::Client};
use serde::{Deserialize, Serialize};

use crate::config::{AppContext, StoredCredentials};

const DEVICE_PATH: &str = "agent-workers/device-authorization";

#[derive(Debug, Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct DeviceToken {
    worker_id: String,
    api_key: String,
}

#[derive(Serialize)]
struct StartRequest<'a> {
    client_name: &'a str,
}

fn default_interval() -> u64 {
    5
}

pub fn login(context: &AppContext, open_browser: bool) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("rustgrid-agent/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let endpoint = format!("{}/{DEVICE_PATH}", context.api_url.trim_end_matches('/'));
    let response = client
        .post(&endpoint)
        .json(&StartRequest {
            client_name: "rustgrid-agent",
        })
        .send()
        .context("could not start RustGrid device login")?
        .error_for_status()
        .context("RustGrid rejected the device login request")?;
    let authorization: DeviceAuthorization = response
        .json()
        .context("RustGrid returned an invalid device login response")?;
    validate_authorization(&authorization)?;

    println!("First copy your one-time code: {}", authorization.user_code);
    println!(
        "Open {} to authorize this worker.",
        authorization.verification_uri
    );
    if open_browser {
        let url = authorization
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&authorization.verification_uri);
        if let Err(error) = open_url(url) {
            eprintln!("[warning] Could not open a browser: {error:#}");
        }
    }

    let deadline = Instant::now() + Duration::from_secs(authorization.expires_in);
    let mut interval = authorization.interval.max(1);
    loop {
        if Instant::now() >= deadline {
            bail!("device login expired; run `rustgrid-agent login` again");
        }
        std::thread::sleep(Duration::from_secs(interval));
        let response = client
            .post(format!("{endpoint}/token"))
            .json(&serde_json::json!({"device_code": authorization.device_code}))
            .send()
            .context("could not complete RustGrid device login")?;
        match response.status() {
            StatusCode::OK => {
                let token: DeviceToken = response
                    .json()
                    .context("RustGrid returned an invalid device credential")?;
                StoredCredentials {
                    worker_id: token.worker_id.clone(),
                    api_key: token.api_key,
                }
                .save(&context.credentials_path)?;
                println!("[ complete] Worker {} is authenticated", token.worker_id);
                return Ok(());
            }
            StatusCode::ACCEPTED => continue,
            StatusCode::TOO_MANY_REQUESTS => {
                interval = interval.saturating_add(5);
            }
            StatusCode::GONE => bail!("device login expired; run `rustgrid-agent login` again"),
            status => {
                let body = response.text().unwrap_or_default();
                bail!("device login failed ({status}): {}", truncate(&body));
            }
        }
    }
}

fn validate_authorization(value: &DeviceAuthorization) -> Result<()> {
    if value.device_code.is_empty() || value.user_code.is_empty() || value.expires_in == 0 {
        bail!("RustGrid returned an incomplete device login response");
    }
    let uri = reqwest::Url::parse(&value.verification_uri)
        .context("RustGrid returned an invalid verification URL")?;
    validate_verification_url(&uri)?;
    if let Some(complete) = &value.verification_uri_complete {
        let uri = reqwest::Url::parse(complete)
            .context("RustGrid returned an invalid complete verification URL")?;
        validate_verification_url(&uri)?;
    }
    Ok(())
}

fn validate_verification_url(uri: &reqwest::Url) -> Result<()> {
    if uri.scheme() != "https"
        && uri.host_str() != Some("127.0.0.1")
        && uri.host_str() != Some("localhost")
    {
        bail!("RustGrid verification URL must use HTTPS");
    }
    Ok(())
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
            device_code: "device".into(),
            user_code: "ABCD-EFGH".into(),
            verification_uri: "http://example.com/device".into(),
            verification_uri_complete: None,
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
}
