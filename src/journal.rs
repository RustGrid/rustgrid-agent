use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::lifecycle::RunPhase;

#[derive(Debug, Deserialize, Serialize)]
pub struct RunJournal {
    pub schema_version: u8,
    pub run_id: String,
    pub ticket_id: String,
    pub phase: RunPhase,
    pub last_sequence: u64,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub pull_request_url: Option<String>,
    #[serde(default)]
    pub pull_request_number: Option<u64>,
    #[serde(default)]
    pub progress_sequence: u64,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(skip)]
    path: PathBuf,
}

impl RunJournal {
    pub fn create(path: &Path, run_id: &str, ticket_id: &str) -> Result<Self> {
        let path = path.to_path_buf();
        if path.is_file() {
            let bytes =
                fs::read(&path).with_context(|| format!("could not read {}", path.display()))?;
            let mut journal: Self = serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid recovery journal {}", path.display()))?;
            if journal.run_id != run_id || journal.ticket_id != ticket_id {
                anyhow::bail!("recovery journal identity does not match claimed run");
            }
            if journal.schema_version != 1 {
                anyhow::bail!(
                    "unsupported recovery journal schema version {}",
                    journal.schema_version
                );
            }
            journal.path = path;
            return Ok(journal);
        }
        let journal = Self {
            schema_version: 1,
            run_id: run_id.to_owned(),
            ticket_id: ticket_id.to_owned(),
            phase: RunPhase::Claimed,
            last_sequence: 0,
            branch: None,
            commit: None,
            pull_request_url: None,
            pull_request_number: None,
            progress_sequence: 0,
            last_error: None,
            path,
        };
        journal.persist()?;
        Ok(journal)
    }

    pub fn checkpoint(&mut self, phase: RunPhase, sequence: u64) -> Result<()> {
        self.phase = phase;
        self.last_sequence = sequence;
        self.persist()
    }

    pub fn record_branch(&mut self, branch: &str) -> Result<()> {
        self.branch = Some(branch.to_owned());
        self.persist()
    }

    pub fn record_commit(&mut self, commit: &str) -> Result<()> {
        self.commit = Some(commit.to_owned());
        self.persist()
    }

    pub fn record_pull_request(&mut self, url: &str, number: u64) -> Result<()> {
        self.pull_request_url = Some(url.to_owned());
        self.pull_request_number = Some(number);
        self.persist()
    }

    pub fn record_progress_sequence(&mut self, sequence: u64) -> Result<()> {
        self.progress_sequence = sequence;
        self.persist()
    }

    pub fn record_error(&mut self, error: &str) -> Result<()> {
        self.last_error = Some(error.to_owned());
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("recovery journal path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        let temporary = self.path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("could not open {}", temporary.display()))?;
        file.write_all(&serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("could not write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("could not sync {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("could not replace {}", self.path.display()))?;
        sync_directory(parent)
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("could not open {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("could not sync {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_recovery_checkpoint_outside_worktree_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        let mut journal = RunJournal::create(&path, "run-1", "ticket-1").unwrap();
        journal.checkpoint(RunPhase::Executing, 4).unwrap();
        let text = fs::read_to_string(path).unwrap();
        assert!(text.contains("\"executing\""));
        assert!(text.contains("\"last_sequence\": 4"));
    }

    #[test]
    fn rejects_unknown_journal_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        fs::write(
            &path,
            r#"{"schema_version":2,"run_id":"run-1","ticket_id":"ticket-1","phase":"claimed","last_sequence":0,"branch":null,"commit":null,"pull_request_url":null}"#,
        )
        .unwrap();
        assert!(RunJournal::create(&path, "run-1", "ticket-1").is_err());
    }
}
