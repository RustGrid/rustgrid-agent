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
        let normalized = remote.trim_end_matches('/').trim_end_matches(".git");
        let expected = format!("{owner}/{name}");
        if !normalized.ends_with(&expected) {
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
        command::checked("git", ["switch", "--create", branch, base], &self.root)?;
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
            command::checked("git", ["switch", branch], &self.root)?;
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
        command::checked("git", ["commit", "-m", message], &self.root)?;
        command::checked("git", ["rev-parse", "HEAD"], &self.root)
    }

    pub fn push(&self, branch: &str, github_token: &str) -> Result<()> {
        // Child-only dynamic Git config keeps the token out of argv and remote URLs.
        let authorization = STANDARD.encode(format!("x-access-token:{github_token}"));
        let output = command::capture_with_env(
            "git",
            ["push", "--set-upstream", "origin", branch],
            &self.root,
            [
                ("GIT_CONFIG_COUNT", "1".to_owned()),
                (
                    "GIT_CONFIG_KEY_0",
                    "http.https://github.com/.extraheader".to_owned(),
                ),
                (
                    "GIT_CONFIG_VALUE_0",
                    format!("AUTHORIZATION: basic {authorization}"),
                ),
            ],
        )?;
        if !output.status.success() {
            bail!("git push exited with {}: {}", output.status, output.stderr);
        }
        Ok(())
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
}
