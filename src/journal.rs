use std::{
    fs,
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
    #[serde(skip)]
    path: PathBuf,
}

impl RunJournal {
    pub fn create(repo_root: &Path, run_id: &str, ticket_id: &str) -> Result<Self> {
        let path = repo_root
            .join(".git")
            .join("rustgrid-agent")
            .join("runs")
            .join(format!("{run_id}.json"));
        let journal = Self {
            schema_version: 1,
            run_id: run_id.to_owned(),
            ticket_id: ticket_id.to_owned(),
            phase: RunPhase::Claimed,
            last_sequence: 0,
            branch: None,
            commit: None,
            pull_request_url: None,
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

    pub fn record_pull_request(&mut self, url: &str) -> Result<()> {
        self.pull_request_url = Some(url.to_owned());
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let parent = self.path.parent().expect("journal path has parent");
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("could not write {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("could not replace {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_recovery_checkpoint_outside_worktree_changes() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join(".git")).unwrap();
        let mut journal = RunJournal::create(directory.path(), "run-1", "ticket-1").unwrap();
        journal.checkpoint(RunPhase::Executing, 4).unwrap();
        let text = fs::read_to_string(directory.path().join(".git/rustgrid-agent/runs/run-1.json"))
            .unwrap();
        assert!(text.contains("\"executing\""));
        assert!(text.contains("\"last_sequence\": 4"));
    }
}
