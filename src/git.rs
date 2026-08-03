use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::Url;

use crate::command;

#[derive(Debug, Clone)]
pub struct Repo {
    pub root: PathBuf,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ReconciliationKind {
    Unchanged,
    RemoteAdvanced,
    Rebased,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReconciledCommit {
    pub commit: String,
    pub kind: ReconciliationKind,
}

#[derive(Debug)]
pub struct RemoteBranchMoved {
    branch: String,
}

impl RemoteBranchMoved {
    pub(crate) fn new(branch: impl Into<String>) -> Self {
        Self {
            branch: branch.into(),
        }
    }
}

impl std::fmt::Display for RemoteBranchMoved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "remote branch {} changed during publication",
            self.branch
        )
    }
}

impl std::error::Error for RemoteBranchMoved {}

impl ReconciledCommit {
    pub const fn requires_validation(&self) -> bool {
        !matches!(self.kind, ReconciliationKind::Unchanged)
    }
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

    pub fn verify_hosted_origin(&self, owner: &str, name: &str, web_base_url: &str) -> Result<()> {
        let remote = command::checked("git", ["remote", "get-url", "origin"], &self.root)
            .context("repository does not have an origin remote")?;
        let remote_url =
            Url::parse(&remote).context("hosted repository origin must be an HTTPS URL")?;
        let web_url =
            Url::parse(web_base_url).context("hosted repository web base must be an HTTPS URL")?;
        let remote_path = remote_url
            .path()
            .trim_end_matches('/')
            .trim_end_matches(".git");
        let expected_path = format!("/{owner}/{name}");
        if remote_url.scheme() != "https"
            || web_url.scheme() != "https"
            || remote_url.origin() != web_url.origin()
            || !remote_url.username().is_empty()
            || remote_url.password().is_some()
            || remote_url.query().is_some()
            || remote_url.fragment().is_some()
            || !remote_path.eq_ignore_ascii_case(&expected_path)
        {
            bail!("hosted repository origin is not the credential-free manifest repository URL");
        }
        Ok(())
    }

    pub fn hosted_local_config(&self) -> Result<Vec<u8>> {
        let git_dir = self.root.join(".git");
        let git_metadata = std::fs::symlink_metadata(&git_dir)
            .context("hosted checkout has no inspectable .git directory")?;
        if !git_metadata.is_dir() || git_metadata.file_type().is_symlink() {
            bail!("hosted checkout .git path must be a real directory");
        }
        let config = git_dir.join("config");
        let metadata = std::fs::symlink_metadata(&config)
            .context("hosted checkout has no inspectable local Git config")?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 1024 * 1024
        {
            bail!("hosted checkout local Git config is not a bounded regular file");
        }
        std::fs::read(config).context("could not snapshot hosted checkout Git config")
    }

    pub fn configure_hosted_author(&self, name: &str, email: &str) -> Result<()> {
        command::checked("git", ["config", "--local", "user.name", name], &self.root)?;
        command::checked(
            "git",
            ["config", "--local", "user.email", email],
            &self.root,
        )?;
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

    /// Reuses an existing hosted-execution branch without force-pushing, or creates it
    /// from the requested base when this is the first execution process to publish it.
    pub fn checkout_or_create_hosted_branch(
        &self,
        branch: &str,
        base_sha: &str,
        github_token: &str,
        web_base_url: &str,
    ) -> Result<bool> {
        validate_branch_name(branch)?;
        validate_commit_sha(base_sha)?;
        if !self.has_commit(base_sha)? {
            bail!("immutable hosted checkout commit {base_sha} is not present locally");
        }
        let local_ref = format!("refs/heads/{branch}");
        let local_branch_exists = command::capture(
            "git",
            ["show-ref", "--verify", "--quiet", &local_ref],
            &self.root,
        )?
        .status
        .success();

        let environment = github_environment(github_token, web_base_url);
        if self
            .remote_branch_commit(branch, environment.clone())?
            .is_some()
        {
            let remote_ref = format!("refs/rustgrid-agent/remotes/{branch}");
            let fetch_spec = format!("+refs/heads/{branch}:{remote_ref}");
            let fetch = command::capture_with_env(
                "git",
                ["fetch", "--no-tags", "origin", &fetch_spec],
                &self.root,
                environment,
            )?;
            if !fetch.status.success() {
                bail!(
                    "git fetch for hosted branch exited with {}: {}",
                    fetch.status,
                    fetch.stderr
                );
            }
            let create_mode = if local_branch_exists {
                "-C"
            } else {
                "--create"
            };
            command::checked(
                "git",
                [
                    "-c",
                    hooks_disabled(),
                    "switch",
                    create_mode,
                    branch,
                    &remote_ref,
                ],
                &self.root,
            )?;
            return Ok(true);
        }

        if local_branch_exists {
            command::checked(
                "git",
                ["-c", hooks_disabled(), "switch", "-C", branch, base_sha],
                &self.root,
            )?;
        } else {
            command::checked(
                "git",
                [
                    "-c",
                    hooks_disabled(),
                    "switch",
                    "--create",
                    branch,
                    base_sha,
                ],
                &self.root,
            )?;
        }
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
        self.push_with_lease(branch, expected_commit, None, github_token, web_base_url)
    }

    pub fn push_with_lease(
        &self,
        branch: &str,
        expected_commit: &str,
        expected_remote: Option<&str>,
        github_token: &str,
        web_base_url: &str,
    ) -> Result<bool> {
        // Child-only dynamic Git config keeps the token out of argv and remote URLs.
        let environment = github_environment(github_token, web_base_url);
        let remote_before = self.remote_branch_commit(branch, environment.clone())?;
        if remote_before.as_deref() == Some(expected_commit) {
            return Ok(false);
        }
        if expected_remote.is_some() && remote_before.as_deref() != expected_remote {
            return Err(RemoteBranchMoved::new(branch).into());
        }
        let mut push_args = vec![
            "-c".to_owned(),
            hooks_disabled().to_owned(),
            "push".to_owned(),
            "--set-upstream".to_owned(),
        ];
        if let Some(expected_remote) = expected_remote {
            push_args.push(format!(
                "--force-with-lease=refs/heads/{branch}:{expected_remote}"
            ));
        }
        push_args.extend(["origin".to_owned(), branch.to_owned()]);
        let output = command::capture_with_env("git", push_args, &self.root, environment.clone())?;
        if !output.status.success() {
            let remote_after = self.remote_branch_commit(branch, environment.clone())?;
            if remote_after != remote_before {
                return Err(RemoteBranchMoved::new(branch).into());
            }
            bail!("git push exited with {}: {}", output.status, output.stderr);
        }
        let remote = self.remote_branch_commit(branch, environment)?;
        if remote.as_deref() != Some(expected_commit) {
            return Err(RemoteBranchMoved::new(branch).into());
        }
        Ok(true)
    }

    pub fn remote_branch_head(
        &self,
        branch: &str,
        github_token: &str,
        web_base_url: &str,
    ) -> Result<Option<String>> {
        self.remote_branch_commit(branch, github_environment(github_token, web_base_url))
    }

    pub fn reconcile_remote_branch(
        &self,
        branch: &str,
        expected_commit: &str,
        github_token: &str,
        web_base_url: &str,
    ) -> Result<ReconciledCommit> {
        let environment = github_environment(github_token, web_base_url);
        let Some(remote_commit) = self.remote_branch_commit(branch, environment.clone())? else {
            return Ok(ReconciledCommit {
                commit: expected_commit.to_owned(),
                kind: ReconciliationKind::Unchanged,
            });
        };
        if remote_commit == expected_commit {
            return Ok(ReconciledCommit {
                commit: expected_commit.to_owned(),
                kind: ReconciliationKind::Unchanged,
            });
        }

        let remote_ref = format!("refs/rustgrid-agent/remotes/{branch}");
        let fetch_spec = format!("+refs/heads/{branch}:{remote_ref}");
        let fetch = command::capture_with_env(
            "git",
            ["fetch", "--no-tags", "origin", &fetch_spec],
            &self.root,
            environment,
        )?;
        if !fetch.status.success() {
            bail!("git fetch exited with {}: {}", fetch.status, fetch.stderr);
        }
        let fetched_commit = command::checked("git", ["rev-parse", &remote_ref], &self.root)?;
        if fetched_commit != remote_commit {
            bail!("remote branch {branch} moved while it was being fetched; retry publication");
        }

        if self.is_ancestor(&remote_commit, expected_commit)? {
            return Ok(ReconciledCommit {
                commit: expected_commit.to_owned(),
                kind: ReconciliationKind::Unchanged,
            });
        }

        let head = command::checked("git", ["rev-parse", "HEAD"], &self.root)?;
        if head != expected_commit {
            bail!(
                "local branch {branch} resolved to {head}, expected publication commit {expected_commit}"
            );
        }

        if self.is_ancestor(expected_commit, &remote_commit)? {
            command::checked(
                "git",
                ["-c", hooks_disabled(), "merge", "--ff-only", &remote_ref],
                &self.root,
            )?;
            return Ok(ReconciledCommit {
                commit: remote_commit,
                kind: ReconciliationKind::RemoteAdvanced,
            });
        }

        let parents = command::checked(
            "git",
            ["rev-list", "--parents", "-n", "1", expected_commit],
            &self.root,
        )?;
        let parents = parents.split_whitespace().collect::<Vec<_>>();
        if parents.len() != 2 {
            bail!(
                "cannot safely reconcile publication commit {expected_commit}: expected one parent"
            );
        }
        let merge_base = command::capture(
            "git",
            ["merge-base", expected_commit, &remote_commit],
            &self.root,
        )?;
        if !merge_base.status.success() {
            bail!(
                "cannot safely reconcile branch {branch}: local and remote histories are unrelated"
            );
        }

        let rebase = command::capture(
            "git",
            [
                "-c",
                hooks_disabled(),
                "rebase",
                "--onto",
                &remote_ref,
                parents[1],
                branch,
            ],
            &self.root,
        )?;
        if !rebase.status.success() {
            let _ = command::capture(
                "git",
                ["-c", hooks_disabled(), "rebase", "--abort"],
                &self.root,
            );
            bail!(
                "concurrent changes on branch {branch} conflict with the agent commit; retained workspace requires manual reconciliation: {}",
                rebase.stderr
            );
        }
        let commit = command::checked("git", ["rev-parse", "HEAD"], &self.root)?;
        Ok(ReconciledCommit {
            commit,
            kind: ReconciliationKind::Rebased,
        })
    }

    pub fn rebase_onto_remote_base(
        &self,
        branch: &str,
        base_branch: &str,
        expected_commit: &str,
        github_token: &str,
        web_base_url: &str,
    ) -> Result<ReconciledCommit> {
        let head = command::checked("git", ["rev-parse", "HEAD"], &self.root)?;
        if head != expected_commit {
            bail!(
                "local branch {branch} resolved to {head}, expected publication commit {expected_commit}"
            );
        }
        let remote_ref = format!("refs/rustgrid-agent/bases/{base_branch}");
        let fetch_spec = format!("+refs/heads/{base_branch}:{remote_ref}");
        let fetch = command::capture_with_env(
            "git",
            ["fetch", "--no-tags", "origin", &fetch_spec],
            &self.root,
            github_environment(github_token, web_base_url),
        )?;
        if !fetch.status.success() {
            bail!(
                "could not fetch latest base branch {base_branch}: git fetch exited with {}: {}",
                fetch.status,
                fetch.stderr
            );
        }
        let base_commit = command::checked("git", ["rev-parse", &remote_ref], &self.root)?;
        if self.is_ancestor(&base_commit, expected_commit)? {
            return Ok(ReconciledCommit {
                commit: expected_commit.to_owned(),
                kind: ReconciliationKind::Unchanged,
            });
        }
        let merge_base = command::capture(
            "git",
            ["merge-base", expected_commit, &base_commit],
            &self.root,
        )?;
        if !merge_base.status.success() {
            bail!("cannot rebase branch {branch} onto {base_branch}: histories are unrelated");
        }
        let merge_base = merge_base.stdout.trim();
        if merge_base == expected_commit {
            bail!(
                "branch {branch} has no unpublished commits after latest base {base_branch}; refusing to create an empty pull request"
            );
        }
        let rebase = command::capture(
            "git",
            [
                "-c",
                hooks_disabled(),
                "rebase",
                "--autostash",
                "--onto",
                &remote_ref,
                merge_base,
                branch,
            ],
            &self.root,
        )?;
        if !rebase.status.success() {
            let _ = command::capture(
                "git",
                ["-c", hooks_disabled(), "rebase", "--abort"],
                &self.root,
            );
            bail!(
                "agent changes conflict with latest base branch {base_branch}; automatic rebase was attempted and aborted cleanly: {}",
                rebase.stderr
            );
        }
        let commit = command::checked("git", ["rev-parse", "HEAD"], &self.root)?;
        Ok(ReconciledCommit {
            commit,
            kind: ReconciliationKind::Rebased,
        })
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let output = command::capture(
            "git",
            ["merge-base", "--is-ancestor", ancestor, descendant],
            &self.root,
        )?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!(
                "git merge-base exited with {}: {}",
                output.status,
                output.stderr
            ),
        }
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

fn github_environment(github_token: &str, web_base_url: &str) -> Vec<(&'static str, String)> {
    let authorization = STANDARD.encode(format!("x-access-token:{github_token}"));
    vec![
        ("GIT_CONFIG_COUNT", "3".to_owned()),
        (
            "GIT_CONFIG_KEY_0",
            format!("http.{}/.extraheader", web_base_url.trim_end_matches('/')),
        ),
        (
            "GIT_CONFIG_VALUE_0",
            format!("AUTHORIZATION: basic {authorization}"),
        ),
        ("GIT_CONFIG_KEY_1", "credential.helper".to_owned()),
        ("GIT_CONFIG_VALUE_1", String::new()),
        ("GIT_CONFIG_KEY_2", "protocol.ext.allow".to_owned()),
        ("GIT_CONFIG_VALUE_2", "never".to_owned()),
        ("GIT_CONFIG_NOSYSTEM", "1".to_owned()),
        ("GIT_CONFIG_GLOBAL", null_git_config().to_owned()),
        ("GIT_TERMINAL_PROMPT", "0".to_owned()),
    ]
}

fn null_git_config() -> &'static str {
    #[cfg(windows)]
    {
        "NUL"
    }
    #[cfg(not(windows))]
    {
        "/dev/null"
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

fn validate_branch_name(branch: &str) -> Result<()> {
    if branch.is_empty()
        || branch.len() > 255
        || branch.starts_with('-')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.ends_with('.')
        || branch.contains("..")
        || branch.contains("@{")
        || branch.contains("//")
        || branch.bytes().any(|byte| {
            byte.is_ascii_control()
                || byte.is_ascii_whitespace()
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
    {
        bail!("unsafe Git branch name in execution manifest");
    }
    Ok(())
}

fn validate_commit_sha(commit: &str) -> Result<()> {
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("unsafe immutable Git commit SHA in execution manifest");
    }
    Ok(())
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

pub fn fresh_branch_name(key: &str, title: &str, run_id: &str) -> String {
    let run_id = slug(run_id);
    format!("{}-{run_id}", branch_name(key, title))
        .trim_end_matches('-')
        .to_owned()
}

pub fn read_repo_instructions(root: &Path) -> Result<Vec<(String, String)>> {
    let mut result = Vec::new();
    for name in ["AGENTS.md", "README.md"] {
        let path = root.join(name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("could not inspect {}", path.display()));
            }
        };
        if metadata.is_file() && !metadata.file_type().is_symlink() {
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
    fn fresh_branch_names_are_isolated_by_run() {
        let first = fresh_branch_name(
            "AOPS-102",
            "Production verification and security hardening",
            "11111111-1111-4111-8111-111111111111",
        );
        let second = fresh_branch_name(
            "AOPS-102",
            "Production verification and security hardening",
            "22222222-2222-4222-8222-222222222222",
        );

        assert_ne!(first, second);
        assert!(
            first.starts_with("agent/aops-102-production-verification-and-security-hardening-")
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

    #[cfg(unix)]
    #[test]
    fn repository_instructions_do_not_follow_symlinks_outside_the_checkout() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("AGENTS.md")).unwrap();
        std::fs::write(directory.path().join("README.md"), "safe instructions").unwrap();

        let instructions = read_repo_instructions(directory.path()).unwrap();
        assert_eq!(
            instructions,
            vec![("README.md".into(), "safe instructions".into())]
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
    fn hosted_origin_requires_the_exact_credential_free_https_repository() {
        let directory = tempfile::tempdir().unwrap();
        command::checked("git", ["init", "--initial-branch=main"], directory.path()).unwrap();
        command::checked(
            "git",
            [
                "remote",
                "add",
                "origin",
                "https://github.com/RustGrid/example.git",
            ],
            directory.path(),
        )
        .unwrap();
        let repo = Repo {
            root: directory.path().into(),
        };
        repo.verify_hosted_origin("rustgrid", "EXAMPLE", "https://github.com")
            .unwrap();
        assert!(!repo.hosted_local_config().unwrap().is_empty());

        command::checked(
            "git",
            [
                "remote",
                "set-url",
                "origin",
                "ext::sh -c 'echo stolen' https://github.com/RustGrid/example.git",
            ],
            directory.path(),
        )
        .unwrap();
        assert!(
            repo.verify_hosted_origin("RustGrid", "example", "https://github.com")
                .is_err()
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let hook = local.join(".git/hooks/pre-push");
            std::fs::write(&hook, "#!/bin/sh\ntouch pre-push-hook-ran\nexit 1\n").unwrap();
            let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&hook, permissions).unwrap();
        }
        let repo = Repo { root: local };
        let commit = command::checked("git", ["rev-parse", "HEAD"], &repo.root).unwrap();
        assert!(
            repo.push("main", &commit, "token", "https://github.com")
                .unwrap()
        );
        #[cfg(unix)]
        assert!(!repo.root.join("pre-push-hook-ran").exists());
        assert!(
            !repo
                .push("main", &commit, "token", "https://github.com")
                .unwrap()
        );
    }

    #[test]
    fn hosted_branch_checkout_reuses_remote_attempt_without_force_push() {
        let directory = tempfile::tempdir().unwrap();
        let remote = directory.path().join("remote.git");
        let seed = directory.path().join("seed");
        let worker = directory.path().join("worker");
        command::checked(
            "git",
            ["init", "--bare", remote.to_str().unwrap()],
            directory.path(),
        )
        .unwrap();
        command::checked(
            "git",
            ["init", "--initial-branch=main", seed.to_str().unwrap()],
            directory.path(),
        )
        .unwrap();
        command::checked("git", ["config", "user.email", "agent@example.com"], &seed).unwrap();
        command::checked("git", ["config", "user.name", "Agent Test"], &seed).unwrap();
        std::fs::write(seed.join("README.md"), "base\n").unwrap();
        command::checked("git", ["add", "README.md"], &seed).unwrap();
        command::checked("git", ["commit", "-m", "base"], &seed).unwrap();
        command::checked(
            "git",
            ["remote", "add", "origin", remote.to_str().unwrap()],
            &seed,
        )
        .unwrap();
        command::checked("git", ["push", "-u", "origin", "main"], &seed).unwrap();
        let base_sha = command::checked("git", ["rev-parse", "main"], &seed).unwrap();
        command::checked(
            "git",
            ["switch", "-c", "rustgrid/rg-1-11111111", "main"],
            &seed,
        )
        .unwrap();
        std::fs::write(seed.join("attempt.txt"), "attempt\n").unwrap();
        command::checked("git", ["add", "attempt.txt"], &seed).unwrap();
        command::checked("git", ["commit", "-m", "attempt"], &seed).unwrap();
        let expected = command::checked("git", ["rev-parse", "HEAD"], &seed).unwrap();
        command::checked(
            "git",
            ["push", "-u", "origin", "rustgrid/rg-1-11111111"],
            &seed,
        )
        .unwrap();
        command::checked(
            "git",
            [
                "clone",
                "--branch",
                "main",
                remote.to_str().unwrap(),
                worker.to_str().unwrap(),
            ],
            directory.path(),
        )
        .unwrap();

        let repo = Repo { root: worker };
        command::checked(
            "git",
            ["config", "user.email", "attacker@example.com"],
            &repo.root,
        )
        .unwrap();
        command::checked("git", ["config", "user.name", "Untrusted Step"], &repo.root).unwrap();
        command::checked(
            "git",
            ["switch", "-c", "rustgrid/rg-1-11111111"],
            &repo.root,
        )
        .unwrap();
        std::fs::write(repo.root.join("untrusted-local.txt"), "not authoritative\n").unwrap();
        command::checked("git", ["add", "untrusted-local.txt"], &repo.root).unwrap();
        command::checked(
            "git",
            ["commit", "-m", "untrusted local branch"],
            &repo.root,
        )
        .unwrap();
        command::checked("git", ["switch", "main"], &repo.root).unwrap();
        assert!(
            repo.checkout_or_create_hosted_branch(
                "rustgrid/rg-1-11111111",
                &base_sha,
                "short-lived-token",
                "https://github.com",
            )
            .unwrap()
        );
        assert_eq!(
            command::checked("git", ["rev-parse", "HEAD"], &repo.root).unwrap(),
            expected
        );
        assert!(!repo.root.join("untrusted-local.txt").exists());
        assert_eq!(
            command::checked("git", ["branch", "--show-current"], &repo.root).unwrap(),
            "rustgrid/rg-1-11111111"
        );
        repo.configure_hosted_author("octocat", "583231+octocat@users.noreply.github.com")
            .unwrap();
        assert_eq!(
            command::checked("git", ["config", "--local", "user.name"], &repo.root).unwrap(),
            "octocat"
        );
        assert_eq!(
            command::checked("git", ["config", "--local", "user.email"], &repo.root).unwrap(),
            "583231+octocat@users.noreply.github.com"
        );
    }

    #[test]
    fn hosted_fresh_branch_uses_the_immutable_local_checkout_sha() {
        let directory = tempfile::tempdir().unwrap();
        let (seed, remote) = initialized_repository(directory.path());
        let worker = directory.path().join("worker");
        clone_branch(&remote, &worker);
        let base_sha = command::checked("git", ["rev-parse", "HEAD"], &worker).unwrap();

        std::fs::write(seed.root.join("advanced.txt"), "remote moved\n").unwrap();
        let advanced = commit_all(&seed.root, "advance mutable main");
        command::checked("git", ["push", "origin", "main"], &seed.root).unwrap();
        assert_ne!(base_sha, advanced);

        let repo = Repo { root: worker };
        assert!(
            !repo
                .checkout_or_create_hosted_branch(
                    "rustgrid/rg-1-22222222",
                    &base_sha,
                    "short-lived-token",
                    "https://github.com",
                )
                .unwrap()
        );
        assert_eq!(
            command::checked("git", ["rev-parse", "HEAD"], &repo.root).unwrap(),
            base_sha
        );
        assert!(!repo.root.join("advanced.txt").exists());
    }

    #[test]
    fn reconciliation_rebases_the_agent_commit_onto_concurrent_remote_work() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, remote) = initialized_repository(directory.path());
        std::fs::write(repo.root.join("agent.txt"), "agent work\n").unwrap();
        let agent_commit = commit_all(&repo.root, "agent commit");

        let collaborator = directory.path().join("collaborator");
        clone_branch(&remote, &collaborator);
        std::fs::write(collaborator.join("human.txt"), "human work\n").unwrap();
        let human_commit = commit_all(&collaborator, "human commit");
        command::checked("git", ["push", "origin", "main"], &collaborator).unwrap();

        let reconciled = repo
            .reconcile_remote_branch("main", &agent_commit, "token", "https://github.com")
            .unwrap();
        assert_eq!(reconciled.kind, ReconciliationKind::Rebased);
        assert_ne!(reconciled.commit, agent_commit);
        assert!(repo.is_ancestor(&human_commit, &reconciled.commit).unwrap());
        assert_eq!(
            command::checked("git", ["rev-parse", "main"], &repo.root).unwrap(),
            reconciled.commit
        );
        assert_eq!(
            std::fs::read_to_string(repo.root.join("agent.txt")).unwrap(),
            "agent work\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.root.join("human.txt")).unwrap(),
            "human work\n"
        );
        assert!(
            repo.push("main", &reconciled.commit, "token", "https://github.com")
                .unwrap()
        );
    }

    #[test]
    fn reconciliation_accepts_remote_commits_after_the_agent_commit() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, remote) = initialized_repository(directory.path());
        std::fs::write(repo.root.join("agent.txt"), "agent work\n").unwrap();
        let agent_commit = commit_all(&repo.root, "agent commit");
        repo.push("main", &agent_commit, "token", "https://github.com")
            .unwrap();

        let collaborator = directory.path().join("collaborator");
        clone_branch(&remote, &collaborator);
        std::fs::write(collaborator.join("human.txt"), "human work\n").unwrap();
        let human_commit = commit_all(&collaborator, "human commit");
        command::checked("git", ["push", "origin", "main"], &collaborator).unwrap();

        let reconciled = repo
            .reconcile_remote_branch("main", &agent_commit, "token", "https://github.com")
            .unwrap();
        assert_eq!(reconciled.kind, ReconciliationKind::RemoteAdvanced);
        assert_eq!(reconciled.commit, human_commit);
        assert_eq!(
            command::checked("git", ["rev-parse", "HEAD"], &repo.root).unwrap(),
            human_commit
        );
        assert!(
            !repo
                .push("main", &human_commit, "token", "https://github.com")
                .unwrap()
        );
    }

    #[test]
    fn reconciliation_aborts_cleanly_when_concurrent_changes_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, remote) = initialized_repository(directory.path());
        std::fs::write(repo.root.join("file.txt"), "agent work\n").unwrap();
        let agent_commit = commit_all(&repo.root, "agent commit");

        let collaborator = directory.path().join("collaborator");
        clone_branch(&remote, &collaborator);
        std::fs::write(collaborator.join("file.txt"), "human work\n").unwrap();
        commit_all(&collaborator, "human commit");
        command::checked("git", ["push", "origin", "main"], &collaborator).unwrap();

        let error = repo
            .reconcile_remote_branch("main", &agent_commit, "token", "https://github.com")
            .unwrap_err();
        assert!(error.to_string().contains("requires manual reconciliation"));
        assert_eq!(
            command::checked("git", ["rev-parse", "HEAD"], &repo.root).unwrap(),
            agent_commit
        );
        assert!(repo.dirty_paths().unwrap().is_empty());
    }

    #[test]
    fn rebases_all_agent_commits_onto_the_latest_remote_base() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, remote) = initialized_repository(directory.path());
        command::checked("git", ["switch", "-c", "agent/work"], &repo.root).unwrap();
        std::fs::write(repo.root.join("agent-one.txt"), "one\n").unwrap();
        commit_all(&repo.root, "agent one");
        std::fs::write(repo.root.join("agent-two.txt"), "two\n").unwrap();
        let agent_commit = commit_all(&repo.root, "agent two");

        let collaborator = directory.path().join("collaborator");
        clone_branch(&remote, &collaborator);
        std::fs::write(collaborator.join("merged-pr.txt"), "merged\n").unwrap();
        let latest_base = commit_all(&collaborator, "merged pull request");
        command::checked("git", ["push", "origin", "main"], &collaborator).unwrap();

        let reconciled = repo
            .rebase_onto_remote_base(
                "agent/work",
                "main",
                &agent_commit,
                "token",
                "https://github.com",
            )
            .unwrap();
        assert_eq!(reconciled.kind, ReconciliationKind::Rebased);
        assert!(repo.is_ancestor(&latest_base, &reconciled.commit).unwrap());
        assert_eq!(
            command::checked(
                "git",
                [
                    "rev-list",
                    "--count",
                    &format!("{latest_base}..{}", reconciled.commit)
                ],
                &repo.root,
            )
            .unwrap(),
            "2"
        );
    }

    #[test]
    fn safely_replaces_an_existing_agent_branch_after_base_rebase() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, remote) = initialized_repository(directory.path());
        command::checked("git", ["switch", "-c", "agent/work"], &repo.root).unwrap();
        std::fs::write(repo.root.join("agent.txt"), "agent\n").unwrap();
        let old_agent_commit = commit_all(&repo.root, "agent work");
        repo.push(
            "agent/work",
            &old_agent_commit,
            "token",
            "https://github.com",
        )
        .unwrap();

        let collaborator = directory.path().join("collaborator");
        clone_branch(&remote, &collaborator);
        std::fs::write(collaborator.join("merged-pr.txt"), "merged\n").unwrap();
        let latest_base = commit_all(&collaborator, "merged pull request");
        command::checked("git", ["push", "origin", "main"], &collaborator).unwrap();

        let reconciled = repo
            .rebase_onto_remote_base(
                "agent/work",
                "main",
                &old_agent_commit,
                "token",
                "https://github.com",
            )
            .unwrap();
        assert!(
            repo.push_with_lease(
                "agent/work",
                &reconciled.commit,
                Some(&old_agent_commit),
                "token",
                "https://github.com",
            )
            .unwrap()
        );
        assert!(repo.is_ancestor(&latest_base, &reconciled.commit).unwrap());
        assert_eq!(
            repo.remote_branch_head("agent/work", "token", "https://github.com")
                .unwrap()
                .as_deref(),
            Some(reconciled.commit.as_str())
        );
    }

    #[test]
    fn stale_force_with_lease_never_overwrites_concurrent_agent_work() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, remote) = initialized_repository(directory.path());
        command::checked("git", ["switch", "-c", "agent/work"], &repo.root).unwrap();
        std::fs::write(repo.root.join("agent.txt"), "agent\n").unwrap();
        let observed_remote = commit_all(&repo.root, "agent work");
        repo.push(
            "agent/work",
            &observed_remote,
            "token",
            "https://github.com",
        )
        .unwrap();
        std::fs::write(repo.root.join("repair.txt"), "repair\n").unwrap();
        let local_repair = commit_all(&repo.root, "local repair");

        let concurrent = directory.path().join("concurrent");
        command::checked(
            "git",
            [
                "clone",
                "--branch",
                "agent/work",
                remote.to_str().unwrap(),
                concurrent.to_str().unwrap(),
            ],
            directory.path(),
        )
        .unwrap();
        configure_identity(&concurrent);
        std::fs::write(concurrent.join("concurrent.txt"), "concurrent\n").unwrap();
        let concurrent_commit = commit_all(&concurrent, "concurrent work");
        command::checked("git", ["push", "origin", "agent/work"], &concurrent).unwrap();

        let error = repo
            .push_with_lease(
                "agent/work",
                &local_repair,
                Some(&observed_remote),
                "token",
                "https://github.com",
            )
            .unwrap_err();
        assert!(error.downcast_ref::<RemoteBranchMoved>().is_some());
        assert_eq!(
            repo.remote_branch_head("agent/work", "token", "https://github.com")
                .unwrap()
                .as_deref(),
            Some(concurrent_commit.as_str())
        );
    }

    #[test]
    fn base_rebase_conflict_is_attempted_and_aborted_cleanly() {
        let directory = tempfile::tempdir().unwrap();
        let (repo, remote) = initialized_repository(directory.path());
        command::checked("git", ["switch", "-c", "agent/work"], &repo.root).unwrap();
        std::fs::write(repo.root.join("file.txt"), "agent\n").unwrap();
        let agent_commit = commit_all(&repo.root, "agent work");

        let collaborator = directory.path().join("collaborator");
        clone_branch(&remote, &collaborator);
        std::fs::write(collaborator.join("file.txt"), "merged\n").unwrap();
        commit_all(&collaborator, "merged pull request");
        command::checked("git", ["push", "origin", "main"], &collaborator).unwrap();

        let error = repo
            .rebase_onto_remote_base(
                "agent/work",
                "main",
                &agent_commit,
                "token",
                "https://github.com",
            )
            .unwrap_err();
        assert!(error.to_string().contains("automatic rebase was attempted"));
        assert_eq!(
            command::checked("git", ["rev-parse", "HEAD"], &repo.root).unwrap(),
            agent_commit
        );
        assert!(repo.dirty_paths().unwrap().is_empty());
    }

    fn initialized_repository(directory: &Path) -> (Repo, PathBuf) {
        let remote = directory.join("remote.git");
        let local = directory.join("local");
        command::checked(
            "git",
            ["init", "--bare", remote.to_str().unwrap()],
            directory,
        )
        .unwrap();
        command::checked(
            "git",
            ["init", "--initial-branch=main", local.to_str().unwrap()],
            directory,
        )
        .unwrap();
        configure_identity(&local);
        std::fs::write(local.join("file.txt"), "initial\n").unwrap();
        commit_all(&local, "initial");
        command::checked(
            "git",
            ["remote", "add", "origin", remote.to_str().unwrap()],
            &local,
        )
        .unwrap();
        command::checked("git", ["push", "-u", "origin", "main"], &local).unwrap();
        (Repo { root: local }, remote)
    }

    fn clone_branch(remote: &Path, target: &Path) {
        command::checked(
            "git",
            [
                "clone",
                "--branch",
                "main",
                remote.to_str().unwrap(),
                target.to_str().unwrap(),
            ],
            target.parent().unwrap(),
        )
        .unwrap();
        configure_identity(target);
    }

    fn configure_identity(root: &Path) {
        command::checked("git", ["config", "user.email", "agent@example.com"], root).unwrap();
        command::checked("git", ["config", "user.name", "Agent Test"], root).unwrap();
    }

    fn commit_all(root: &Path, message: &str) -> String {
        command::checked("git", ["add", "--all"], root).unwrap();
        command::checked("git", ["commit", "-m", message], root).unwrap();
        command::checked("git", ["rev-parse", "HEAD"], root).unwrap()
    }
}
