use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::{command, git::Repo, journal::RunJournal, manifest::ExecutionManifest};

pub struct RunWorkspace {
    pub root: PathBuf,
    pub repo: Repo,
    resumed: bool,
}

pub struct RecoveryWorkspace {
    pub journal: RunJournal,
    pub workspace_id: String,
}

impl RunWorkspace {
    pub fn adopt_recovery(
        workspace_root: &Path,
        run_id: &str,
        ticket_id: &str,
        source_run_id: &str,
    ) -> Result<RecoveryWorkspace> {
        validate_run_id(run_id)?;
        validate_run_id(source_run_id)?;
        if run_id == source_run_id {
            bail!("a run cannot adopt its own recovery workspace");
        }
        let mut candidates = Vec::new();
        for entry in fs::read_dir(workspace_root)
            .with_context(|| format!("could not scan {}", workspace_root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(workspace_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_run_id(&workspace_id).is_err() {
                continue;
            }
            let journal_path = entry.path().join("journal.json");
            let Ok(bytes) = fs::read(&journal_path) else {
                continue;
            };
            let Ok(journal) = serde_json::from_slice::<RunJournal>(&bytes) else {
                continue;
            };
            let is_source = journal.run_id == source_run_id;
            let is_idempotent = journal.run_id == run_id
                && journal.recovery_source_run_id.as_deref() == Some(source_run_id);
            if is_source || is_idempotent {
                candidates.push((workspace_id, journal_path));
            }
        }
        if candidates.len() != 1 {
            bail!(
                "expected exactly one recovery workspace for source run {source_run_id}, found {}",
                candidates.len()
            );
        }
        let (workspace_id, journal_path) = candidates.pop().expect("one candidate checked");
        let journal = RunJournal::adopt_recovery(&journal_path, run_id, ticket_id, source_run_id)?;
        Ok(RecoveryWorkspace {
            journal,
            workspace_id,
        })
    }

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
                    "-c",
                    git_hooks_disabled(),
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

    pub fn sweep_stale(
        workspace_root: &Path,
        retention: Duration,
        protected_run_ids: &HashSet<String>,
    ) -> Result<usize> {
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
            let journal_run_is_protected = fs::read(entry.path().join("journal.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<RunJournal>(&bytes).ok())
                .is_some_and(|journal| protected_run_ids.contains(&journal.run_id));
            if protected_run_ids.contains(name) || journal_run_is_protected {
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

    pub fn recoverable_sandbox_names(
        workspace_root: &Path,
        retention: Duration,
    ) -> Result<HashSet<String>> {
        let mut recoverable = HashSet::new();
        if !workspace_root.exists() {
            return Ok(recoverable);
        }
        for entry in fs::read_dir(workspace_root)
            .with_context(|| format!("could not scan {}", workspace_root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(run_id) = name.to_str() else {
                continue;
            };
            if validate_run_id(run_id).is_err() {
                continue;
            }
            let journal_path = entry.path().join("journal.json");
            let Ok(metadata) = journal_path.metadata() else {
                continue;
            };
            if metadata.modified()?.elapsed().unwrap_or_default() >= retention {
                continue;
            }
            let Ok(bytes) = fs::read(&journal_path) else {
                continue;
            };
            let Ok(journal) = serde_json::from_slice::<RunJournal>(&bytes) else {
                continue;
            };
            if journal
                .executor
                .as_ref()
                .is_some_and(|executor| executor.state != "destroyed")
                && let Some(executor) = journal.executor
            {
                recoverable.insert(executor.id);
            }
        }
        Ok(recoverable)
    }
}

fn git_hooks_disabled() -> &'static str {
    #[cfg(windows)]
    {
        "core.hooksPath=NUL"
    }
    #[cfg(not(windows))]
    {
        "core.hooksPath=/dev/null"
    }
}

pub(crate) fn directory_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            total = total.saturating_add(metadata.len());
        } else if metadata.is_dir() {
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
    use crate::lifecycle::RunPhase;
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
                attempt: 1,
                metadata: json!({}),
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

    #[cfg(unix)]
    #[test]
    fn workspace_accounting_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let outside = directory.path().join("outside");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("large"), vec![0u8; 1024 * 1024]).unwrap();
        symlink(&outside, workspace.join("escape")).unwrap();

        assert!(directory_size(&workspace).unwrap() < 1024 * 1024);
    }

    #[test]
    fn stale_sweep_never_removes_protected_active_runs() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("workspaces");
        fs::create_dir_all(root.join("active-run")).unwrap();
        fs::create_dir_all(root.join("expired-run")).unwrap();
        let protected = HashSet::from(["active-run".to_owned()]);

        let removed = RunWorkspace::sweep_stale(&root, Duration::ZERO, &protected).unwrap();

        assert_eq!(removed, 1);
        assert!(root.join("active-run").is_dir());
        assert!(!root.join("expired-run").exists());
    }

    #[test]
    fn stale_sweep_protects_an_adopted_attempt_in_a_stable_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("workspaces");
        let workspace = root.join("run-1");
        fs::create_dir_all(&workspace).unwrap();
        let mut journal =
            RunJournal::create(&workspace.join("journal.json"), "run-1", "ticket-1").unwrap();
        journal.checkpoint(RunPhase::Failed, 1).unwrap();
        journal
            .record_executor(
                "docker_sandbox",
                "rustgrid-0123456789abcdef0123456789abcdef",
                "retained",
            )
            .unwrap();
        RunWorkspace::adopt_recovery(&root, "run-2", "ticket-1", "run-1").unwrap();

        let removed =
            RunWorkspace::sweep_stale(&root, Duration::ZERO, &HashSet::from(["run-2".to_owned()]))
                .unwrap();

        assert_eq!(removed, 0);
        assert!(workspace.exists());
    }

    #[test]
    fn recent_retained_executor_is_protected_for_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let run_root = directory.path().join("run-1");
        fs::create_dir_all(&run_root).unwrap();
        let mut journal =
            RunJournal::create(&run_root.join("journal.json"), "run-1", "ticket-1").unwrap();
        journal
            .record_executor("docker_sandbox", "rustgrid-run-1", "retained")
            .unwrap();

        assert_eq!(
            RunWorkspace::recoverable_sandbox_names(directory.path(), Duration::from_secs(60))
                .unwrap(),
            HashSet::from(["rustgrid-run-1".to_owned()])
        );

        journal
            .record_executor("docker_sandbox", "rustgrid-run-1", "destroyed")
            .unwrap();
        assert!(
            RunWorkspace::recoverable_sandbox_names(directory.path(), Duration::from_secs(60))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn retry_atomically_adopts_the_retained_workspace_without_moving_its_mount() {
        let directory = tempfile::tempdir().unwrap();
        let source_root = directory.path().join("run-1");
        fs::create_dir_all(source_root.join("repo")).unwrap();
        fs::write(source_root.join("repo/work.txt"), "salvage me").unwrap();
        let mut journal =
            RunJournal::create(&source_root.join("journal.json"), "run-1", "ticket-1").unwrap();
        journal.checkpoint(RunPhase::Failed, 9).unwrap();
        journal
            .record_executor(
                "docker_sandbox",
                "rustgrid-0123456789abcdef0123456789abcdef",
                "retained",
            )
            .unwrap();

        let mut adopted =
            RunWorkspace::adopt_recovery(directory.path(), "run-2", "ticket-1", "run-1").unwrap();

        assert!(source_root.exists());
        assert!(!directory.path().join("run-2").exists());
        assert_eq!(
            fs::read_to_string(source_root.join("repo/work.txt")).unwrap(),
            "salvage me"
        );
        assert_eq!(adopted.workspace_id, "run-1");
        assert_eq!(adopted.journal.run_id, "run-2");
        assert_eq!(adopted.journal.last_sequence, 0);
        assert!(
            RunWorkspace::adopt_recovery(directory.path(), "run-2", "ticket-1", "run-1").is_ok()
        );

        adopted.journal.checkpoint(RunPhase::Failed, 4).unwrap();
        let third =
            RunWorkspace::adopt_recovery(directory.path(), "run-3", "ticket-1", "run-2").unwrap();
        assert_eq!(third.workspace_id, "run-1");
        assert_eq!(third.journal.run_id, "run-3");
        assert_eq!(
            third.journal.recovery_source_run_id.as_deref(),
            Some("run-2")
        );
    }
}
