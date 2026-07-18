use anyhow::{Context, Result, bail};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{api::RustGridClient, config::AppContext, executor::Executor, git::Repo};

pub fn status(context: &AppContext, json_output: bool) -> Result<()> {
    let local_repo = Repo::discover().ok();
    let dirty = local_repo
        .as_ref()
        .map(Repo::dirty_paths)
        .transpose()?
        .unwrap_or_default();
    let api_key_present = context
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty());
    let worker_id_present = context.worker_id.is_some();
    let per_run_isolation = context.config.executor.is_isolated();
    let production_config = context
        .config
        .executor
        .validate_production(context.config.max_concurrency);
    let production_config_ready = production_config.is_ok();
    let production_config_error = production_config
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"));
    let executor_check =
        Executor::from_config(&context.config.executor).preflight(&context.workspace_root);
    let executor_ready = executor_check.is_ok();
    let executor_error = executor_check
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"));
    let production_safe_concurrency = per_run_isolation || context.config.max_concurrency == 1;
    let remote_worker = if api_key_present && worker_id_present {
        RustGridClient::new(context).and_then(|api| api.worker_status(context.require_worker_id()?))
    } else {
        Err(anyhow::anyhow!(
            "worker authentication is required; run `rustgrid-agent login`"
        ))
    };
    let rustgrid_reachable = remote_worker.is_ok();
    let remote_error = remote_worker
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"));
    let remote_status = remote_worker
        .as_ref()
        .ok()
        .map(|worker| worker.status.as_str());
    let last_heartbeat_at = remote_worker
        .as_ref()
        .ok()
        .and_then(|worker| worker.last_seen_at.as_deref());
    let worker_runtime_enabled = !matches!(
        remote_status,
        Some("disabled" | "revoked" | "pending_upgrade")
    );
    let current_unix_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let credential_expired = context
        .credential_expires_at_unix
        .is_some_and(|expires_at| expires_at <= current_unix_time);
    let login_required = !api_key_present
        || !worker_id_present
        || credential_expired
        || matches!(remote_status, Some("revoked" | "pending_upgrade"));
    let healthy = api_key_present
        && worker_id_present
        && worker_runtime_enabled
        && per_run_isolation
        && executor_ready
        && production_config_ready
        && production_safe_concurrency
        && rustgrid_reachable;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": 1,
                "healthy": healthy,
                "config": context.config_path,
                "instance": context.instance_url,
                "instance_url": context.instance_url,
                "api_url": context.api_url,
                "agent_version": env!("CARGO_PKG_VERSION"),
                "authenticated": api_key_present && worker_id_present,
                "scope": "tenant",
                "repository": context.config.repo.as_ref().map(|repo| format!("{}/{}", repo.owner, repo.name)),
                "workspace_root": context.workspace_root,
                "local_repository_root": local_repo.as_ref().map(|repo| &repo.root),
                "max_concurrency": context.config.max_concurrency,
                "lease_seconds": context.config.lease_seconds,
                "api_key_present": api_key_present,
                "login_required": login_required,
                "reauthentication_required": login_required,
                "credential_source": context.credential_source.as_str(),
                "credential_expires_at_unix": context.credential_expires_at_unix,
                "credential_expired": credential_expired,
                "installation_id": context.installation_id,
                "worker_id": context.worker_id,
                "worker_name": context.worker_name,
                "tenant_id": context.tenant_id,
                "worker_id_present": worker_id_present,
                "worker_status": remote_status,
                "worker_runtime_enabled": worker_runtime_enabled,
                "last_heartbeat_at": last_heartbeat_at,
                "per_run_isolation": per_run_isolation,
                "executor": context.config.executor.kind(),
                "executor_ready": executor_ready,
                "executor_error": executor_error,
                "production_config_ready": production_config_ready,
                "production_config_error": production_config_error,
                "production_safe_concurrency": production_safe_concurrency,
                "rustgrid_reachable": rustgrid_reachable,
                "remote_error": remote_error,
                "github_credentials": "brokered_per_run"
            }))?
        );
        if healthy {
            return Ok(());
        }
        bail!("status checks failed");
    }
    println!("RustGrid agent status\n");
    println!("  Config:       {}", context.config_path.display());
    println!("  Instance:     {}", context.instance_url);
    println!("  RustGrid API: {}", context.api_url);
    println!("  Agent:        {}", env!("CARGO_PKG_VERSION"));
    println!("  Installation: {}", context.installation_id);
    println!("  Scope:        tenant (all control-plane-authorized projects)");
    println!(
        "  Repository:   {}",
        context
            .config
            .repo
            .as_ref()
            .map(|repo| format!("{}/{} (deprecated local hint)", repo.owner, repo.name))
            .unwrap_or_else(|| "resolved from each run manifest".into())
    );
    println!("  Workspaces:   {}", context.workspace_root.display());
    if let Some(repo) = &local_repo {
        println!("  Local repo:   {}", repo.root.display());
    }
    println!("  Base branch:  {}", context.config.default_base_branch);
    println!("  Execution:    command, gates, timeout, and sandbox are server-owned per run");
    println!(
        "  Heartbeat:    every {}s",
        context.config.heartbeat_interval_seconds
    );
    println!("  Run lease:    {}s", context.config.lease_seconds);
    println!(
        "  Tenant ID:    {}",
        context.tenant_id.as_deref().unwrap_or("missing")
    );
    println!(
        "  Worker name:  {}",
        context.worker_name.as_deref().unwrap_or("missing")
    );
    println!(
        "  Worker ID:    {}",
        context.worker_id.as_deref().unwrap_or("missing")
    );
    println!(
        "  API key:      {}",
        if context.api_key.is_some() {
            context.credential_source.as_str()
        } else {
            "missing"
        }
    );
    println!(
        "  Credential expires: {}",
        context
            .credential_expires_at_unix
            .map_or_else(|| "unknown".to_owned(), |value| value.to_string())
    );
    println!("  Worker state: {}", remote_status.unwrap_or("unknown"));
    println!(
        "  Login needed: {}",
        if login_required { "yes" } else { "no" }
    );
    println!("  Last heartbeat: {}", last_heartbeat_at.unwrap_or("never"));
    println!("  GitHub token: brokered per run by RustGrid");
    println!(
        "  RustGrid:     {}",
        if rustgrid_reachable {
            "authenticated and worker identity loaded"
        } else {
            "unreachable or unauthorized"
        }
    );
    println!(
        "  Isolation:    {}",
        if per_run_isolation {
            "Docker Sandbox per run"
        } else {
            "local development executor"
        }
    );
    println!(
        "  Concurrency:  {}",
        if production_safe_concurrency {
            "safe for configured executor"
        } else {
            "unsafe executor/concurrency combination"
        }
    );
    println!(
        "  Working tree: {}",
        if local_repo.is_none() {
            "not applicable (isolated workspace mode)".into()
        } else if dirty.is_empty() {
            "clean".into()
        } else {
            format!("dirty ({} path(s))", dirty.len())
        }
    );
    if context.api_key.is_none() || context.worker_id.is_none() {
        bail!("status checks failed: worker identity or credentials are missing");
    }
    if !per_run_isolation {
        bail!("status checks failed: executor.kind=docker_sandbox is required for production");
    }
    production_config.context("status checks failed: production executor configuration")?;
    executor_check.context("status checks failed: Docker Sandbox executor")?;
    if !production_safe_concurrency {
        bail!("status checks failed: max_concurrency must be 1 for production");
    }
    remote_worker.context("status checks failed: RustGrid connectivity")?;
    Ok(())
}
