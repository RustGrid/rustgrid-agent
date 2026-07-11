use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::{api::RustGridClient, config::AppContext, git::Repo};

pub fn status(context: &AppContext, json_output: bool) -> Result<()> {
    let local_repo = Repo::discover().ok();
    let dirty = local_repo
        .as_ref()
        .map(Repo::dirty_paths)
        .transpose()?
        .unwrap_or_default();
    let (project_kind, project) = context.project_value();
    let api_key_present = context
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty());
    let per_run_isolation = std::env::var("RUSTGRID_AGENT_ISOLATION").as_deref() == Ok("per_run");
    let production_safe_concurrency = context.config.max_concurrency == 1;
    let remote_check = if api_key_present {
        RustGridClient::new(context).and_then(|api| api.resolve_project_id(context).map(|_| ()))
    } else {
        Err(anyhow::anyhow!("RustGrid API key is missing"))
    };
    let rustgrid_reachable = remote_check.is_ok();
    let remote_error = remote_check
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"));
    let healthy =
        api_key_present && per_run_isolation && production_safe_concurrency && rustgrid_reachable;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "healthy": healthy,
                "config": context.config_path,
                "api_url": context.api_url,
                "project": {"kind": project_kind, "value": project},
                "repository": context.config.repo.as_ref().map(|repo| format!("{}/{}", repo.owner, repo.name)),
                "workspace_root": context.workspace_root,
                "local_repository_root": local_repo.as_ref().map(|repo| &repo.root),
                "max_concurrency": context.config.max_concurrency,
                "lease_seconds": context.config.lease_seconds,
                "api_key_present": api_key_present,
                "per_run_isolation": per_run_isolation,
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
    println!("  RustGrid API: {}", context.api_url);
    println!("  Project:      {project_kind}={project}");
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
        "  API key:      {}",
        if context.api_key.is_some() {
            "set"
        } else {
            "missing"
        }
    );
    println!("  GitHub token: brokered per run by RustGrid");
    println!(
        "  RustGrid:     {}",
        if rustgrid_reachable {
            "authenticated and project resolved"
        } else {
            "unreachable or unauthorized"
        }
    );
    println!(
        "  Isolation:    {}",
        if per_run_isolation {
            "per-run deployment boundary declared"
        } else {
            "missing RUSTGRID_AGENT_ISOLATION=per_run"
        }
    );
    println!(
        "  Concurrency:  {}",
        if production_safe_concurrency {
            "one run per worker process"
        } else {
            "unsafe for serve; max_concurrency must be 1"
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
    if context.api_key.is_none() {
        bail!("status checks failed: required credentials are missing");
    }
    if !per_run_isolation {
        bail!("status checks failed: per-run deployment isolation is not declared");
    }
    if !production_safe_concurrency {
        bail!("status checks failed: max_concurrency must be 1 for production");
    }
    remote_check.context("status checks failed: RustGrid connectivity")?;
    Ok(())
}
