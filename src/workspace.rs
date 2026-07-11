use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::{command, git::Repo, manifest::ExecutionManifest};

pub struct RunWorkspace {
    pub root: PathBuf,
    pub repo: Repo,
    resumed: bool,
}

impl RunWorkspace {
    pub fn prepare(
        workspace_root: &Path,
        run_id: &str,
        manifest: &ExecutionManifest,
        github_token: &str,
    ) -> Result<Self> {
        validate_run_id(run_id)?;
        let root = workspace_root.join(run_id);
        let repo_root = root.join("repo");
        fs::create_dir_all(&root)
            .with_context(|| format!("could not create workspace {}", root.display()))?;

        let resumed = repo_root.join(".git").exists();
        if !resumed {
            if repo_root.exists() {
                fs::remove_dir_all(&repo_root).with_context(|| {
                    format!("could not reset incomplete clone {}", repo_root.display())
                })?;
            }
            let authorization = STANDARD.encode(format!("x-access-token:{github_token}"));
            let base = manifest.default_branch.as_deref().unwrap_or("main");
            let output = command::capture_with_env(
                "git",
                [
                    "clone",
                    "--branch",
                    base,
                    "--single-branch",
                    "--",
                    &manifest.clone_url,
                    repo_root.to_str().context("workspace path is not UTF-8")?,
                ],
                &root,
                [
                    ("GIT_CONFIG_COUNT", "1".to_owned()),
                    (
                        "GIT_CONFIG_KEY_0",
                        format!(
                            "http.{}/.extraheader",
                            manifest.web_base_url.trim_end_matches('/')
                        ),
                    ),
                    (
                        "GIT_CONFIG_VALUE_0",
                        format!("AUTHORIZATION: basic {authorization}"),
                    ),
                ],
            )?;
            if !output.status.success() {
                bail!("git clone exited with {}: {}", output.status, output.stderr);
            }
        }
        let repo = Repo { root: repo_root };
        let identity = manifest.repo_config()?;
        repo.verify_origin(&identity.owner, &identity.name)?;
        Ok(Self {
            root,
            repo,
            resumed,
        })
    }

    pub fn journal_path(workspace_root: &Path, run_id: &str) -> Result<PathBuf> {
        validate_run_id(run_id)?;
        Ok(workspace_root.join(run_id).join("journal.json"))
    }

    pub fn resumed(&self) -> bool {
        self.resumed
    }

    pub fn cleanup(self) -> Result<()> {
        fs::remove_dir_all(&self.root)
            .with_context(|| format!("could not remove workspace {}", self.root.display()))
    }

    pub fn enforce_size_limit(&self, max_bytes: u64) -> Result<u64> {
        let size = directory_size(&self.root)?;
        if size > max_bytes {
            bail!("run workspace uses {size} bytes, exceeding limit {max_bytes}");
        }
        Ok(size)
    }

    pub fn sweep_stale(workspace_root: &Path, retention: Duration) -> Result<usize> {
        if !workspace_root.exists() {
            return Ok(0);
        }
        let mut removed = 0;
        for entry in fs::read_dir(workspace_root)
            .with_context(|| format!("could not scan {}", workspace_root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if validate_run_id(name).is_err() {
                continue;
            }
            let modified = entry.metadata()?.modified()?;
            if modified.elapsed().unwrap_or_default() >= retention {
                fs::remove_dir_all(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn directory_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total = total.saturating_add(directory_size(&entry.path())?);
        } else if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.is_empty()
        || !run_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        bail!("run ID is not safe for a workspace path");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ExecutionManifest, ManifestRun};
    use serde_json::json;

    #[test]
    fn rejects_workspace_path_traversal() {
        assert!(RunWorkspace::journal_path(Path::new("/tmp"), "../escape").is_err());
        assert!(RunWorkspace::journal_path(Path::new("/tmp"), "run-123").is_ok());
    }

    #[test]
    fn creates_and_resumes_an_isolated_clone() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("RustGrid/example");
        fs::create_dir_all(&source).unwrap();
        command::checked("git", ["init", "--initial-branch=main"], &source).unwrap();
        command::checked(
            "git",
            ["config", "user.email", "agent@example.com"],
            &source,
        )
        .unwrap();
        command::checked("git", ["config", "user.name", "Agent Test"], &source).unwrap();
        fs::write(source.join("README.md"), "example\n").unwrap();
        command::checked("git", ["add", "README.md"], &source).unwrap();
        command::checked("git", ["commit", "-m", "initial"], &source).unwrap();
        let manifest = ExecutionManifest {
            manifest_version: 2,
            run: ManifestRun {
                id: "run-1".into(),
                ticket_id: "ticket-1".into(),
            },
            project_id: "project-1".into(),
            project_key: "RG".into(),
            project_name: "RustGrid".into(),
            ticket_id: "ticket-1".into(),
            ticket_key: "RG-1".into(),
            ticket_title: "Task".into(),
            repository_id: 1,
            repository: "RustGrid/example".into(),
            clone_url: source.to_string_lossy().into_owned(),
            web_base_url: "https://github.com".into(),
            installation_id: 1,
            default_branch: Some("main".into()),
            required_workflows: vec![],
            required_permissions: json!({}),
            execution_policy: json!({}),
            execution_policy_sha256: String::new(),
        };
        let root = directory.path().join("workspaces");
        let first = RunWorkspace::prepare(&root, "run-1", &manifest, "token").unwrap();
        assert!(!first.resumed());
        assert!(first.repo.root.join("README.md").is_file());
        let second = RunWorkspace::prepare(&root, "run-1", &manifest, "token").unwrap();
        assert!(second.resumed());
        assert!(second.enforce_size_limit(1).is_err());
    }
}
