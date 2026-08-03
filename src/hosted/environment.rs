// Extracted from the hosted execution composition root.
use super::*;
use reqwest::{StatusCode, Url};

/// Time and sleeping used by hosted orchestration.
///
/// Keeping this port beside retry and expiry policy lets those decisions run
/// against a deterministic clock without teaching the domain about threads or
/// the operating system clock.
pub(crate) trait HostedClock: Send + Sync {
    fn system_now(&self) -> SystemTime;
    fn instant_now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemHostedClock;

impl HostedClock for SystemHostedClock {
    fn system_now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn instant_now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

#[derive(Clone)]
pub(super) struct SecretString(pub(super) String);

impl SecretString {
    pub(super) fn new(value: String, name: &str) -> Result<Self> {
        if value.trim().is_empty() || value.len() > 32 * 1024 || !value.is_ascii() {
            bail!("{name} is missing or malformed");
        }
        Ok(Self(value))
    }

    pub(super) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub(super) struct GithubActionsEnvironment {
    pub(super) api_root: Url,
    pub(super) audience: String,
    pub(super) oidc_request_url: Url,
    pub(super) oidc_request_token: SecretString,
    pub(super) dispatch_nonce: SecretString,
    pub(super) repository: Option<String>,
    pub(super) repository_id: Option<u64>,
    pub(super) sha: Option<String>,
    pub(super) workflow_run_id: Option<i64>,
    pub(super) workflow_run_attempt: Option<i32>,
    pub(super) actor: Option<String>,
    pub(super) actor_id: Option<u64>,
}

pub(super) struct GithubActionsAuthor {
    pub(super) name: String,
    pub(super) email: String,
}

impl GithubActionsEnvironment {
    pub(super) fn load(execution_id: Uuid) -> Result<Self> {
        reject_inherited_provider_credentials()?;
        let configured_execution_id = required_env("RUSTGRID_EXECUTION_ID")?;
        let configured_execution_id = Uuid::parse_str(&configured_execution_id)
            .context("RUSTGRID_EXECUTION_ID must be a UUID")?;
        if configured_execution_id != execution_id {
            bail!("CLI execution ID does not match RUSTGRID_EXECUTION_ID");
        }

        let api_root = normalize_api_root(
            &env::var("RUSTGRID_API_URL").unwrap_or_else(|_| DEFAULT_INSTANCE_URL.to_owned()),
        )?;
        let audience = api_origin(&api_root)?;
        let oidc_request_url = secure_github_oidc_url(
            "RUSTGRID_OIDC_REQUEST_URL",
            &required_env("RUSTGRID_OIDC_REQUEST_URL")?,
        )?;
        let oidc_request_token = SecretString::new(
            required_env("RUSTGRID_OIDC_REQUEST_TOKEN")?,
            "RUSTGRID_OIDC_REQUEST_TOKEN",
        )?;
        let dispatch_nonce = SecretString::new(
            required_env("RUSTGRID_DISPATCH_NONCE")?,
            "RUSTGRID_DISPATCH_NONCE",
        )?;
        validate_dispatch_nonce(dispatch_nonce.expose())?;
        let repository = optional_env("GITHUB_REPOSITORY");
        let repository_id = optional_env("GITHUB_REPOSITORY_ID")
            .map(|value| value.parse::<u64>())
            .transpose()
            .context("GITHUB_REPOSITORY_ID must be an integer")?;
        let sha = optional_env("GITHUB_SHA");
        let workflow_run_id = optional_env("GITHUB_RUN_ID")
            .map(|value| value.parse::<i64>())
            .transpose()
            .context("GITHUB_RUN_ID must be an integer")?;
        let workflow_run_attempt = optional_env("GITHUB_RUN_ATTEMPT")
            .map(|value| value.parse::<i32>())
            .transpose()
            .context("GITHUB_RUN_ATTEMPT must be an integer")?;
        let actor = optional_env("GITHUB_ACTOR");
        let actor_id = optional_env("GITHUB_ACTOR_ID")
            .map(|value| value.parse::<u64>())
            .transpose()
            .context("GITHUB_ACTOR_ID must be an integer")?;
        Ok(Self {
            api_root,
            audience,
            oidc_request_url,
            oidc_request_token,
            dispatch_nonce,
            repository,
            repository_id,
            sha,
            workflow_run_id,
            workflow_run_attempt,
            actor,
            actor_id,
        })
    }

    pub(super) fn require_execute_context(&self) -> Result<()> {
        if self.repository.as_deref().is_none_or(str::is_empty)
            || self.repository_id.is_none_or(|value| value == 0)
            || self.sha.as_deref().is_none_or(|value| !commit_sha(value))
            || self.workflow_run_id.is_none_or(|value| value < 1)
            || self.workflow_run_attempt.is_none_or(|value| value < 1)
        {
            bail!(
                "GitHub Actions execution requires repository, repository ID, run ID, and run-attempt context"
            );
        }
        self.git_author()?;
        Ok(())
    }

    pub(super) fn git_author(&self) -> Result<GithubActionsAuthor> {
        let name = self
            .actor
            .as_deref()
            .filter(|value| valid_github_actor(value))
            .context("GITHUB_ACTOR must identify a valid GitHub account")?;
        let actor_id = self
            .actor_id
            .filter(|value| *value > 0)
            .context("GITHUB_ACTOR_ID must identify a valid GitHub account")?;
        Ok(GithubActionsAuthor {
            name: name.to_owned(),
            email: format!("{actor_id}+{name}@users.noreply.github.com"),
        })
    }
}

pub(super) fn normalize_api_root(value: &str) -> Result<Url> {
    let mut url = secure_url("RUSTGRID_API_URL", value)?;
    if url.query().is_some() || url.fragment().is_some() {
        bail!("RUSTGRID_API_URL cannot contain a query or fragment");
    }
    let trimmed = url.path().trim_end_matches('/');
    let path = if trimmed.ends_with("/api/v1") || trimmed == "api/v1" {
        format!("{trimmed}/")
    } else if trimmed.is_empty() || trimmed == "/" {
        "/api/v1/".to_owned()
    } else {
        format!("{trimmed}/api/v1/")
    };
    url.set_path(&path);
    Ok(url)
}

pub(super) fn secure_url(name: &str, value: &str) -> Result<Url> {
    let url = Url::parse(value).with_context(|| format!("{name} must be a URL"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("{name} must be credential-free HTTPS (or loopback HTTP for tests)");
    }
    Ok(url)
}

pub(super) fn secure_github_oidc_url(name: &str, value: &str) -> Result<Url> {
    let url = secure_url(name, value)?;
    let host = url.host_str().unwrap_or_default();
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if !loopback
        && host != "actions.githubusercontent.com"
        && !host.ends_with(".actions.githubusercontent.com")
    {
        bail!("{name} must use GitHub's Actions token-service host");
    }
    if url.query_pairs().any(|(key, _)| key == "audience") {
        bail!("{name} cannot predeclare an OIDC audience");
    }
    Ok(url)
}

pub(super) fn api_origin(url: &Url) -> Result<String> {
    let origin = url.origin().ascii_serialization();
    if origin == "null" {
        bail!("RUSTGRID_API_URL has no secure origin");
    }
    Ok(origin)
}

pub(super) fn validate_manifest_endpoint(
    name: &str,
    value: &str,
    api_root: &Url,
    expected_relative: &str,
) -> Result<()> {
    let expected_path = format!("/api/v1/{expected_relative}");
    if value == expected_path {
        return Ok(());
    }
    let endpoint = Url::parse(value).with_context(|| {
        format!("execution manifest {name} must be a canonical relative or absolute URL")
    })?;
    if endpoint.origin() != api_root.origin()
        || endpoint.path() != expected_path
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        bail!("execution manifest {name} is outside the mission API scope");
    }
    Ok(())
}

pub(super) fn required_env(name: &str) -> Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("{name} is required for GitHub Actions execution"))
}

#[cfg(target_os = "linux")]
pub(super) fn harden_hosted_process() -> Result<()> {
    // Repository commands run as the same ephemeral runner user. Mark the
    // coordinator non-dumpable before any are launched so they cannot inspect
    // its environment, heap, or file descriptors through procfs/ptrace.
    // SAFETY: PR_SET_DUMPABLE takes one integer flag and no pointer arguments.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("could not isolate hosted coordinator credentials");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn harden_hosted_process() -> Result<()> {
    bail!("GitHub Actions hosted execution requires Linux process isolation")
}

pub(super) fn optional_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

pub(super) fn valid_github_actor(value: &str) -> bool {
    if value.is_empty() || value.len() > 100 || !value.is_ascii() {
        return false;
    }
    let login = value.strip_suffix("[bot]").unwrap_or(value);
    !login.is_empty()
        && login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(super) fn reject_inherited_provider_credentials() -> Result<()> {
    let forbidden = [
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "CHATGPT_TOKEN",
        "OPENAI_ORG_ID",
    ];
    if forbidden
        .iter()
        .any(|name| env::var_os(name).is_some_and(|value| !value.is_empty()))
    {
        bail!(
            "hosted execution refuses inherited OpenAI or ChatGPT credentials; use only the RustGrid AI gateway"
        );
    }
    Ok(())
}

pub(super) fn validate_dispatch_nonce(value: &str) -> Result<()> {
    if !(32..=256).contains(&value.len())
        || !value.starts_with("rgdn_")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("RUSTGRID_DISPATCH_NONCE is malformed");
    }
    Ok(())
}

pub(super) fn validate_github_oidc_token(value: &str) -> Result<()> {
    if !(64..=16 * 1024).contains(&value.len())
        || !value.is_ascii()
        || value.bytes().filter(|byte| *byte == b'.').count() != 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("GitHub returned a malformed OIDC token");
    }
    Ok(())
}

pub(super) fn validate_execution_token(value: &str) -> Result<()> {
    if !(32..=512).contains(&value.len())
        || !value.starts_with("rge_")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("RustGrid returned a malformed execution token");
    }
    Ok(())
}

pub(super) fn retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    )
}

pub(super) fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(250_u64.saturating_mul(1_u64 << attempt.min(5)))
}

pub(super) fn ai_request_timeout(
    clock: &dyn HostedClock,
    execution_deadline: Option<Instant>,
) -> Result<Duration> {
    execution_deadline
        .map(|deadline| {
            deadline
                .checked_duration_since(clock.instant_now())
                .filter(|remaining| !remaining.is_zero())
                .context("hosted execution deadline was reached before the AI gateway request")
                .map(|remaining| remaining.min(Duration::from_secs(90)))
        })
        .transpose()
        .map(|timeout| timeout.unwrap_or(Duration::from_secs(90)))
}

pub(super) fn sleep_before_execution_retry(
    clock: &dyn HostedClock,
    execution_deadline: Option<Instant>,
    delay: Duration,
    operation: &str,
) -> Result<()> {
    if let Some(deadline) = execution_deadline {
        let remaining = deadline
            .checked_duration_since(clock.instant_now())
            .filter(|remaining| *remaining > delay)
            .with_context(|| {
                format!("hosted execution deadline was reached before the {operation} could start")
            })?;
        debug_assert!(remaining > delay);
    }
    clock.sleep(delay);
    Ok(())
}

pub(super) fn sleep_before_ai_retry(
    clock: &dyn HostedClock,
    execution_deadline: Option<Instant>,
    attempt: usize,
) -> Result<()> {
    sleep_before_execution_retry(
        clock,
        execution_deadline,
        retry_delay(attempt),
        "AI gateway retry",
    )
}

pub(super) fn registration_retry_delay(attempt: usize, semantic_call_id: Uuid) -> Duration {
    let base_millis = [250_u64, 1_000, 3_000]
        .get(attempt)
        .copied()
        .unwrap_or(3_000);
    let bytes = semantic_call_id.as_bytes();
    let sample = u16::from_be_bytes([bytes[0], bytes[1]]);
    let jitter_percent = 80_u64 + u64::from(sample % 41);
    Duration::from_millis(base_millis.saturating_mul(jitter_percent) / 100)
}

pub(super) fn token_refresh_after(expires_at: SystemTime) -> SystemTime {
    expires_at
        .checked_sub(TOKEN_REFRESH_MARGIN)
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

pub(super) fn safe_identifier(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

pub(super) fn safe_child_environment_name(name: &str) -> bool {
    let normalized = name.trim().to_ascii_uppercase();
    !normalized.is_empty()
        && normalized.len() <= 128
        && normalized
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !normalized.starts_with("RUSTGRID_")
        && !normalized.starts_with("GITHUB_")
        && !normalized.starts_with("ACTIONS_")
        && normalized != "SSH_AUTH_SOCK"
        && !normalized.contains("TOKEN")
        && !normalized.contains("SECRET")
        && !normalized.contains("PASSWORD")
        && !normalized.contains("CREDENTIAL")
        && !normalized.contains("PRIVATE_KEY")
        && !normalized.contains("API_KEY")
        && !matches!(
            normalized.as_str(),
            "SHELL"
                | "ENV"
                | "BASH_ENV"
                | "ZDOTDIR"
                | "CDPATH"
                | "IFS"
                | "PYTHONPATH"
                | "PYTHONHOME"
                | "NODE_OPTIONS"
                | "RUBYOPT"
                | "PERL5OPT"
                | "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "RUSTDOC_WRAPPER"
                | "GIT_EXEC_PATH"
        )
        && !normalized.starts_with("LD_")
        && !normalized.starts_with("DYLD_")
        && !normalized.starts_with("GIT_CONFIG")
}

pub(super) fn normalized_base_ref(value: &str) -> Result<&str> {
    let value = value.strip_prefix("refs/heads/").unwrap_or(value);
    if value.is_empty() || value.len() > 255 {
        bail!("execution manifest base ref is invalid");
    }
    Ok(value)
}

pub(super) fn safe_git_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with(['-', '/'])
        && !value.ends_with(['/', '.'])
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains("//")
        && !value.bytes().any(|byte| {
            byte.is_ascii_control()
                || byte.is_ascii_whitespace()
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
}

pub(super) fn commit_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn ensure_running(running: &AtomicBool) -> Result<()> {
    if !running.load(Ordering::SeqCst) || shutdown::requested() {
        bail!("hosted execution was cancelled or lost its mission lease");
    }
    Ok(())
}

pub(super) fn hosted_execution_deadline(started_at: Instant, limit: Duration) -> Result<Instant> {
    started_at
        .checked_add(limit.min(MAX_HOSTED_EXECUTION_DURATION))
        .context("hosted execution deadline could not be represented")
}
