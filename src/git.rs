use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::command;

#[derive(Debug, Clone)]
pub struct Repo {
    pub root: PathBuf,
}

impl Repo {
    pub fn discover() -> Result<Self> {
        let cwd = std::env::current_dir().context("could not determine current directory")?;
        let root = command::checked("git", ["rev-parse", "--show-toplevel"], &cwd)
            .context("current directory is not inside a Git repository")?;
        Ok(Self {
            root: PathBuf::from(root),
        })
    }

    pub fn dirty_paths(&self) -> Result<BTreeSet<String>> {
        let output = command::capture(
            "git",
            ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            &self.root,
        )?;
        if !output.status.success() {
            bail!(
                "git status exited with {}: {}",
                output.status,
                output.stderr
            );
        }
        Ok(parse_porcelain_z(output.stdout.as_bytes()))
    }

    pub fn ensure_safe(&self, allow_dirty: bool) -> Result<BTreeSet<String>> {
        let dirty = self.dirty_paths()?;
        if !dirty.is_empty() && !allow_dirty {
            bail!(
                "Git working tree is dirty; commit/stash changes or pass --allow-dirty\n  {}",
                dirty.iter().cloned().collect::<Vec<_>>().join("\n  ")
            );
        }
        Ok(dirty)
    }

    pub fn verify_origin(&self, owner: &str, name: &str) -> Result<()> {
        let remote = command::checked("git", ["remote", "get-url", "origin"], &self.root)
            .context("repository does not have an origin remote")?;
        let expected = format!("{owner}/{name}");
        let matches = remote_repository(&remote).is_some_and(|(remote_owner, remote_name)| {
            remote_owner.eq_ignore_ascii_case(owner) && remote_name.eq_ignore_ascii_case(name)
        });
        if !matches {
            bail!("claimed manifest repository {expected} does not match origin {remote}");
        }
        Ok(())
    }

    pub fn create_branch(&self, branch: &str, base: &str) -> Result<()> {
        if command::capture(
            "git",
            [
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            &self.root,
        )?
        .status
        .success()
        {
            bail!("local branch {branch} already exists");
        }
        command::checked(
            "git",
            ["-c", hooks_disabled(), "switch", "--create", branch, base],
            &self.root,
        )?;
        Ok(())
    }

    pub fn checkout_or_create_branch(&self, branch: &str, base: &str) -> Result<bool> {
        let exists = command::capture(
            "git",
            [
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            &self.root,
        )?
        .status
        .success();
        if exists {
            command::checked(
                "git",
                ["-c", hooks_disabled(), "switch", branch],
                &self.root,
            )?;
            return Ok(true);
        }
        self.create_branch(branch, base)?;
        Ok(false)
    }

    pub fn new_agent_paths(&self, baseline: &BTreeSet<String>) -> Result<Vec<String>> {
        let now = self.dirty_paths()?;
        Ok(now.difference(baseline).cloned().collect())
    }

    pub fn has_commit(&self, commit: &str) -> Result<bool> {
        Ok(command::capture(
            "git",
            ["cat-file", "-e", &format!("{commit}^{{commit}}")],
            &self.root,
        )?
        .status
        .success())
    }

    pub fn commit_paths(&self, paths: &[String], message: &str) -> Result<String> {
        if paths.is_empty() {
            bail!("Codex produced no committable changes");
        }
        let mut add_args = vec!["add".to_owned(), "--".to_owned()];
        add_args.extend(paths.iter().cloned());
        command::checked("git", add_args, &self.root)?;

        let staged = command::capture(
            "git",
            ["diff", "--cached", "--quiet", "--exit-code"],
            &self.root,
        )?;
        if staged.status.success() {
            bail!("Codex produced no committable changes");
        }
        command::checked(
            "git",
            ["-c", hooks_disabled(), "commit", "-m", message],
            &self.root,
        )?;
        command::checked("git", ["rev-parse", "HEAD"], &self.root)
    }

    pub fn push(
        &self,
        branch: &str,
        expected_commit: &str,
        github_token: &str,
        web_base_url: &str,
    ) -> Result<bool> {
        // Child-only dynamic Git config keeps the token out of argv and remote URLs.
        let authorization = STANDARD.encode(format!("x-access-token:{github_token}"));
        let environment = [
            ("GIT_CONFIG_COUNT", "1".to_owned()),
            (
                "GIT_CONFIG_KEY_0",
                format!("http.{}/.extraheader", web_base_url.trim_end_matches('/')),
            ),
            (
                "GIT_CONFIG_VALUE_0",
                format!("AUTHORIZATION: basic {authorization}"),
            ),
        ];
        if self
            .remote_branch_commit(branch, environment.clone())?
            .as_deref()
            == Some(expected_commit)
        {
            return Ok(false);
        }
        let output = command::capture_with_env(
            "git",
            ["push", "--set-upstream", "origin", branch],
            &self.root,
            environment.clone(),
        )?;
        if !output.status.success() {
            bail!("git push exited with {}: {}", output.status, output.stderr);
        }
        let remote = self.remote_branch_commit(branch, environment)?;
        if remote.as_deref() != Some(expected_commit) {
            bail!(
                "remote branch {branch} resolved to {}, expected {expected_commit}",
                remote.as_deref().unwrap_or("missing")
            );
        }
        Ok(true)
    }

    fn remote_branch_commit<E, K, V>(&self, branch: &str, environment: E) -> Result<Option<String>>
    where
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        let reference = format!("refs/heads/{branch}");
        let output = command::capture_with_env(
            "git",
            ["ls-remote", "--heads", "origin", &reference],
            &self.root,
            environment,
        )?;
        if !output.status.success() {
            bail!(
                "git ls-remote exited with {}: {}",
                output.status,
                output.stderr
            );
        }
        Ok(output.stdout.split_whitespace().next().map(str::to_owned))
    }
}

fn remote_repository(remote: &str) -> Option<(&str, &str)> {
    let normalized = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    let (prefix, name) = normalized.rsplit_once('/')?;
    let owner = prefix.rsplit(['/', ':']).next()?;
    (!owner.is_empty() && !name.is_empty()).then_some((owner, name))
}

fn hooks_disabled() -> &'static str {
    #[cfg(windows)]
    {
        "core.hooksPath=NUL"
    }
    #[cfg(not(windows))]
    {
        "core.hooksPath=/dev/null"
    }
}

fn parse_porcelain_z(bytes: &[u8]) -> BTreeSet<String> {
    let entries = bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let mut paths = BTreeSet::new();
    let mut index = 0;
    while index < entries.len() {
        let entry = String::from_utf8_lossy(entries[index]);
        if entry.len() >= 3 {
            let status = &entry[..2];
            paths.insert(entry[3..].to_owned());
            if status.contains('R') || status.contains('C') {
                index += 1;
                if index < entries.len() {
                    paths.insert(String::from_utf8_lossy(entries[index]).into_owned());
                }
            }
        }
        index += 1;
    }
    paths
}

pub fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut dash = false;
    for c in value.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            output.push(c);
            dash = false;
        } else if !output.is_empty() && !dash {
            output.push('-');
            dash = true;
        }
        if output.len() >= 48 {
            break;
        }
    }
    output.trim_matches('-').to_owned()
}

pub fn branch_name(key: &str, title: &str) -> String {
    let key = slug(key);
    let title = slug(title);
    format!("agent/{key}-{title}")
        .trim_end_matches('-')
        .to_owned()
}

pub fn read_repo_instructions(root: &Path) -> Result<Vec<(String, String)>> {
    let mut result = Vec::new();
    for name in ["AGENTS.md", "README.md"] {
        let path = root.join(name);
        if path.is_file() {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("could not read {}", path.display()))?;
            result.push((name.into(), content));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_safe_branch_names() {
        assert_eq!(
            branch_name("RG-42", "Fix the Café / API!"),
            "agent/rg-42-fix-the-caf-api"
        );
    }

    #[test]
    fn parses_untracked_and_modified_paths() {
        let paths = parse_porcelain_z(b" M src/main.rs\0?? notes/a.txt\0");
        assert_eq!(
            paths.into_iter().collect::<Vec<_>>(),
            ["notes/a.txt", "src/main.rs"]
        );
    }

    #[test]
    fn repository_identity_is_case_insensitive_and_component_based() {
        assert_eq!(
            remote_repository("https://github.com/RustGrid/rustgrid-agentops.git"),
            Some(("RustGrid", "rustgrid-agentops"))
        );
        assert_eq!(
            remote_repository("git@github.com:RustGrid/rustgrid-agentops.git"),
            Some(("RustGrid", "rustgrid-agentops"))
        );
        let (owner, name) =
            remote_repository("https://github.com/RustGrid/rustgrid-agentops.git").unwrap();
        assert!(owner.eq_ignore_ascii_case("rustgrid"));
        assert!(name.eq_ignore_ascii_case("RUSTGRID-AGENTOPS"));
    }

    #[test]
    fn commit_paths_excludes_preexisting_dirty_work() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        command::checked("git", ["init", "--initial-branch=main"], root).unwrap();
        command::checked("git", ["config", "user.email", "agent@example.com"], root).unwrap();
        command::checked("git", ["config", "user.name", "Agent Test"], root).unwrap();
        std::fs::write(root.join("existing.txt"), "original\n").unwrap();
        command::checked("git", ["add", "existing.txt"], root).unwrap();
        command::checked("git", ["commit", "-m", "initial"], root).unwrap();

        std::fs::write(root.join("existing.txt"), "user work\n").unwrap();
        let repo = Repo { root: root.into() };
        let baseline = repo.ensure_safe(true).unwrap();
        std::fs::write(root.join("agent.txt"), "agent work\n").unwrap();
        let paths = repo.new_agent_paths(&baseline).unwrap();
        assert_eq!(paths, ["agent.txt"]);

        repo.commit_paths(&paths, "agent commit").unwrap();
        let committed =
            command::checked("git", ["show", "--pretty=", "--name-only", "HEAD"], root).unwrap();
        assert_eq!(committed, "agent.txt");
        assert!(repo.dirty_paths().unwrap().contains("existing.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn agent_commits_do_not_execute_repository_hooks() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        command::checked("git", ["init", "--initial-branch=main"], root).unwrap();
        command::checked("git", ["config", "user.email", "agent@example.com"], root).unwrap();
        command::checked("git", ["config", "user.name", "Agent Test"], root).unwrap();
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\nexit 1\n").unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        std::fs::write(root.join("agent.txt"), "agent work\n").unwrap();

        Repo { root: root.into() }
            .commit_paths(&["agent.txt".into()], "agent commit")
            .unwrap();
        assert!(!root.join("hook-ran").exists());
    }

    #[test]
    fn push_reconciles_the_expected_remote_commit() {
        let directory = tempfile::tempdir().unwrap();
        let remote = directory.path().join("remote.git");
        let local = directory.path().join("local");
        command::checked(
            "git",
            ["init", "--bare", remote.to_str().unwrap()],
            directory.path(),
        )
        .unwrap();
        command::checked(
            "git",
            ["init", "--initial-branch=main", local.to_str().unwrap()],
            directory.path(),
        )
        .unwrap();
        command::checked("git", ["config", "user.email", "agent@example.com"], &local).unwrap();
        command::checked("git", ["config", "user.name", "Agent Test"], &local).unwrap();
        std::fs::write(local.join("file.txt"), "content\n").unwrap();
        command::checked("git", ["add", "file.txt"], &local).unwrap();
        command::checked("git", ["commit", "-m", "initial"], &local).unwrap();
        command::checked(
            "git",
            ["remote", "add", "origin", remote.to_str().unwrap()],
            &local,
        )
        .unwrap();
        let repo = Repo { root: local };
        let commit = command::checked("git", ["rev-parse", "HEAD"], &repo.root).unwrap();
        assert!(
            repo.push("main", &commit, "token", "https://github.com")
                .unwrap()
        );
        assert!(
            !repo
                .push("main", &commit, "token", "https://github.com")
                .unwrap()
        );
    }
}
