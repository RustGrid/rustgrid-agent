use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DependencyState {
    pub manager: String,
    pub command: String,
    pub lockfile_hash: Option<String>,
    pub manifest_hash: Option<String>,
    pub installed_at: String,
    pub reusable: bool,
    pub invalidation_reason: Option<String>,
}

impl DependencyState {
    pub fn inspect(root: &Path, manager: &str, command: &str) -> Result<Self> {
        let lockfile = match manager {
            "npm" => ["package-lock.json", "npm-shrinkwrap.json"]
                .into_iter()
                .find(|name| root.join(name).is_file()),
            "pnpm" => Some("pnpm-lock.yaml"),
            "yarn" => Some("yarn.lock"),
            "bun" => ["bun.lock", "bun.lockb"]
                .into_iter()
                .find(|name| root.join(name).is_file()),
            _ => None,
        };
        Ok(Self {
            manager: manager.into(),
            command: command.into(),
            lockfile_hash: lockfile
                .map(|name| hash_file(&root.join(name)))
                .transpose()?,
            manifest_hash: root
                .join("package.json")
                .is_file()
                .then(|| hash_file(&root.join("package.json")))
                .transpose()?,
            installed_at: unix_timestamp(),
            reusable: root.join("node_modules").is_dir(),
            invalidation_reason: None,
        })
    }

    pub fn reusable_against(&self, current: &Self) -> bool {
        self.reusable
            && current.reusable
            && self.manager == current.manager
            && self.command == current.command
            && self.lockfile_hash == current.lockfile_hash
            && self.manifest_hash == current.manifest_hash
    }

    pub fn completed(mut self) -> Self {
        self.installed_at = unix_timestamp();
        self.reusable = true;
        self.invalidation_reason = None;
        self
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("could not hash {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn unix_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputMode {
    Summary,
    FailureExcerpt,
    FullRequested,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProcessedCommandOutput {
    pub raw_output_location: PathBuf,
    pub normalized_output: String,
    pub model_summary: String,
    pub mode: ToolOutputMode,
    pub truncated: bool,
    pub original_characters: usize,
    pub model_characters: usize,
}

pub fn process_command_output(
    repo_root: &Path,
    command: &str,
    output: &str,
    success: bool,
    exit_code: Option<i32>,
    duration: Duration,
) -> Result<ProcessedCommandOutput> {
    let normalized = normalize_output(output);
    let audit_root = repo_root.join(".git/rustgrid-agent-audit/commands");
    fs::create_dir_all(&audit_root)?;
    let digest = hex::encode(Sha256::digest(
        format!("{command}\0{}", normalized).as_bytes(),
    ));
    let raw_output_location = audit_root.join(format!("{}.log", &digest[..24]));
    if !raw_output_location.is_file() {
        fs::write(&raw_output_location, output)?;
    }
    let (mode, model_summary, truncated) = if success {
        (
            ToolOutputMode::Summary,
            summarize_success(
                command,
                &normalized,
                exit_code,
                duration,
                &raw_output_location,
            ),
            true,
        )
    } else {
        let excerpt = bounded_excerpt(&normalized, 160, 24_000);
        (
            ToolOutputMode::FailureExcerpt,
            format!(
                "Command failed.\nExit code: {}\nDuration: {:.2}s\nRelevant output:\n{}\nFull output stored at {}",
                exit_code.map_or_else(|| "unavailable".into(), |code| code.to_string()),
                duration.as_secs_f64(),
                excerpt,
                raw_output_location.display()
            ),
            excerpt.len() < normalized.len(),
        )
    };
    Ok(ProcessedCommandOutput {
        raw_output_location,
        normalized_output: normalized,
        model_characters: model_summary.chars().count(),
        model_summary,
        mode,
        truncated,
        original_characters: output.chars().count(),
    })
}

fn normalize_output(output: &str) -> String {
    let stripped = strip_ansi(output);
    let mut seen = HashSet::new();
    stripped
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .filter(|line| seen.insert((*line).to_owned()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_ansi(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for control in chars.by_ref() {
                if ('@'..='~').contains(&control) {
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

fn summarize_success(
    command: &str,
    output: &str,
    exit_code: Option<i32>,
    duration: Duration,
    location: &Path,
) -> String {
    let lower = command.to_ascii_lowercase();
    let interesting = output
        .lines()
        .filter(|line| {
            let line = line.to_ascii_lowercase();
            line.contains("tests")
                || line.contains("test files")
                || line.contains("modules transformed")
                || line.contains("packages")
                || line.contains("vulnerabilities")
                || line.contains("warning")
                || line.contains("built in")
        })
        .take(8)
        .collect::<Vec<_>>();
    let kind = if lower.contains("install") || lower.contains("npm ci") {
        "Dependency installation"
    } else if lower.contains("build") {
        "Build"
    } else if lower.contains("test") {
        "Test command"
    } else {
        "Command"
    };
    format!(
        "{kind} succeeded.\nExit code: {}\nDuration: {:.2}s\n{}{}Full output stored at {}",
        exit_code.map_or_else(|| "unavailable".into(), |code| code.to_string()),
        duration.as_secs_f64(),
        if interesting.is_empty() {
            String::new()
        } else {
            format!("Summary:\n{}\n", interesting.join("\n"))
        },
        if output.to_ascii_lowercase().contains("warning") {
            "Warnings were present.\n"
        } else {
            ""
        },
        location.display()
    )
}

fn bounded_excerpt(value: &str, max_lines: usize, max_characters: usize) -> String {
    let mut output = String::new();
    for line in value.lines().take(max_lines) {
        if output
            .chars()
            .count()
            .saturating_add(line.chars().count() + 1)
            > max_characters
        {
            break;
        }
        output.push_str(line);
        output.push('\n');
    }
    output.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_success_and_preserves_raw_audit_output() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join(".git")).unwrap();
        let raw =
            "\u{1b}[32mTest Files  103 passed\u{1b}[0m\nTests 523 passed\nasset-a.js\nasset-b.js\n";
        let processed = process_command_output(
            directory.path(),
            "npm test",
            raw,
            true,
            Some(0),
            Duration::from_millis(22_490),
        )
        .unwrap();
        assert_eq!(processed.mode, ToolOutputMode::Summary);
        assert!(processed.model_summary.contains("103 passed"));
        assert!(!processed.model_summary.contains("asset-a.js"));
        assert!(!processed.model_summary.contains("\u{1b}"));
        assert_eq!(
            fs::read_to_string(processed.raw_output_location).unwrap(),
            raw
        );
    }

    #[test]
    fn bounds_failure_output_by_lines_and_characters() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join(".git")).unwrap();
        let raw = (0..500)
            .map(|index| format!("failure {index}: {}", "x".repeat(300)))
            .collect::<Vec<_>>()
            .join("\n");
        let processed = process_command_output(
            directory.path(),
            "npm test",
            &raw,
            false,
            Some(1),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(processed.truncated);
        assert!(processed.model_summary.chars().count() < 25_000);
        assert!(processed.model_summary.contains("failure 0"));
    }

    #[test]
    fn dependency_state_invalidates_when_the_lockfile_changes() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("package.json"), "{}").unwrap();
        fs::write(directory.path().join("package-lock.json"), "one").unwrap();
        fs::create_dir(directory.path().join("node_modules")).unwrap();
        let installed = DependencyState::inspect(directory.path(), "npm", "npm ci")
            .unwrap()
            .completed();
        assert!(installed.reusable_against(
            &DependencyState::inspect(directory.path(), "npm", "npm ci").unwrap()
        ));
        fs::write(directory.path().join("package-lock.json"), "two").unwrap();
        assert!(!installed.reusable_against(
            &DependencyState::inspect(directory.path(), "npm", "npm ci").unwrap()
        ));
    }
}
