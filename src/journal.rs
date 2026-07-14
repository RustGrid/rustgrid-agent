use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::lifecycle::RunPhase;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryPlan {
    Fresh,
    ResumeFromCommit {
        commit: String,
    },
    ResumeFromPullRequest {
        commit: String,
        url: String,
        number: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutorCheckpoint {
    pub kind: String,
    pub id: String,
    pub state: String,
}

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
    #[serde(default)]
    pub executor: Option<ExecutorCheckpoint>,
    #[serde(default)]
    pub recovery_source_run_id: Option<String>,
    #[serde(skip)]
    path: PathBuf,
}

impl RunJournal {
    pub fn recovery_plan(&self) -> Result<RecoveryPlan> {
        match (
            self.commit.as_ref(),
            self.pull_request_url.as_ref(),
            self.pull_request_number,
        ) {
            (None, None, None) => Ok(RecoveryPlan::Fresh),
            (Some(commit), None, None) => Ok(RecoveryPlan::ResumeFromCommit {
                commit: commit.clone(),
            }),
            (Some(commit), Some(url), Some(number)) => Ok(RecoveryPlan::ResumeFromPullRequest {
                commit: commit.clone(),
                url: url.clone(),
                number,
            }),
            _ => anyhow::bail!("recovery journal contains an incomplete publication checkpoint"),
        }
    }

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
            executor: None,
            recovery_source_run_id: None,
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

    pub fn resume_active_run(&mut self) -> Result<()> {
        if self.phase.is_terminal() {
            self.phase = RunPhase::Claimed;
            self.last_error = None;
            self.persist()?;
        }
        Ok(())
    }

    pub fn adopt_recovery(
        path: &Path,
        run_id: &str,
        ticket_id: &str,
        source_run_id: &str,
    ) -> Result<Self> {
        let bytes = fs::read(path)
            .with_context(|| format!("could not read recovery journal {}", path.display()))?;
        let mut journal: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid recovery journal {}", path.display()))?;
        if journal.schema_version != 1 || journal.ticket_id != ticket_id {
            anyhow::bail!("recovery journal identity does not match the retrying run");
        }
        if journal.run_id == run_id
            && journal.recovery_source_run_id.as_deref() == Some(source_run_id)
        {
            journal.path = path.to_path_buf();
            return Ok(journal);
        }
        if journal.run_id != source_run_id {
            anyhow::bail!("recovery journal does not belong to the requested source run");
        }
        if !journal.phase.is_terminal() || journal.phase == RunPhase::Succeeded {
            anyhow::bail!("recovery source run is not an unsuccessful terminal run");
        }
        if journal
            .executor
            .as_ref()
            .is_none_or(|executor| executor.state != "retained")
        {
            anyhow::bail!("recovery source run has no successfully retained executor");
        }
        journal.run_id = run_id.to_owned();
        journal.phase = RunPhase::Claimed;
        journal.last_sequence = 0;
        journal.progress_sequence = 0;
        journal.last_error = None;
        journal.recovery_source_run_id = Some(source_run_id.to_owned());
        journal.path = path.to_path_buf();
        journal.persist()?;
        Ok(journal)
    }

    pub fn recoverable_executor_id(&self) -> Option<&str> {
        self.executor
            .as_ref()
            .filter(|executor| executor.state != "destroyed")
            .map(|executor| executor.id.as_str())
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

    pub fn record_executor(&mut self, kind: &str, id: &str, state: &str) -> Result<()> {
        self.executor = Some(ExecutorCheckpoint {
            kind: kind.into(),
            id: id.into(),
            state: state.into(),
        });
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

    #[test]
    fn derives_recovery_plan_from_durable_checkpoints() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        let mut journal = RunJournal::create(&path, "run-1", "ticket-1").unwrap();
        assert_eq!(journal.recovery_plan().unwrap(), RecoveryPlan::Fresh);
        journal.record_commit("abc123").unwrap();
        assert!(matches!(
            journal.recovery_plan().unwrap(),
            RecoveryPlan::ResumeFromCommit { .. }
        ));
        journal
            .record_pull_request("https://github.com/o/r/pull/1", 1)
            .unwrap();
        assert!(matches!(
            journal.recovery_plan().unwrap(),
            RecoveryPlan::ResumeFromPullRequest { .. }
        ));
    }

    #[test]
    fn active_recovery_reopens_a_terminal_local_phase_without_losing_progress() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        let mut journal = RunJournal::create(&path, "run-1", "ticket-1").unwrap();
        journal.checkpoint(RunPhase::Cancelled, 17).unwrap();
        journal.record_commit("abc123").unwrap();

        journal.resume_active_run().unwrap();

        assert_eq!(journal.phase, RunPhase::Claimed);
        assert_eq!(journal.last_sequence, 17);
        assert_eq!(journal.commit.as_deref(), Some("abc123"));
    }

    #[test]
    fn cross_attempt_adoption_resets_attempt_state_and_preserves_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        let mut source = RunJournal::create(&path, "run-1", "ticket-1").unwrap();
        source.checkpoint(RunPhase::Failed, 17).unwrap();
        source.record_progress_sequence(23).unwrap();
        source.record_branch("agent/rg-1").unwrap();
        source.record_commit("abc123").unwrap();
        source
            .record_executor(
                "docker_sandbox",
                "rustgrid-0123456789abcdef0123456789abcdef",
                "retained",
            )
            .unwrap();

        let adopted = RunJournal::adopt_recovery(&path, "run-2", "ticket-1", "run-1").unwrap();

        assert_eq!(adopted.run_id, "run-2");
        assert_eq!(adopted.recovery_source_run_id.as_deref(), Some("run-1"));
        assert_eq!(adopted.phase, RunPhase::Claimed);
        assert_eq!(adopted.last_sequence, 0);
        assert_eq!(adopted.progress_sequence, 0);
        assert_eq!(adopted.branch.as_deref(), Some("agent/rg-1"));
        assert_eq!(adopted.commit.as_deref(), Some("abc123"));
        assert_eq!(
            adopted.recoverable_executor_id(),
            Some("rustgrid-0123456789abcdef0123456789abcdef")
        );
        assert!(RunJournal::adopt_recovery(&path, "run-2", "ticket-1", "run-1").is_ok());
    }

    #[test]
    fn cross_attempt_adoption_rejects_live_or_unretained_sources() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        let mut source = RunJournal::create(&path, "run-1", "ticket-1").unwrap();
        source
            .record_executor(
                "docker_sandbox",
                "rustgrid-0123456789abcdef0123456789abcdef",
                "created",
            )
            .unwrap();
        assert!(RunJournal::adopt_recovery(&path, "run-2", "ticket-1", "run-1").is_err());

        source.checkpoint(RunPhase::Failed, 1).unwrap();
        assert!(RunJournal::adopt_recovery(&path, "run-2", "ticket-1", "run-1").is_err());
    }

    #[test]
    fn restores_the_persisted_run_phase() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        let mut journal = RunJournal::create(&path, "run-1", "ticket-1").unwrap();
        journal.checkpoint(RunPhase::Publishing, 7).unwrap();

        let restored = RunJournal::create(&path, "run-1", "ticket-1").unwrap();
        assert_eq!(restored.phase, RunPhase::Publishing);
        assert_eq!(restored.last_sequence, 7);
    }

    #[test]
    fn persists_executor_identity_for_orphan_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        let mut journal = RunJournal::create(&path, "run-1", "ticket-1").unwrap();
        journal
            .record_executor("docker_sandbox", "rustgrid-run-1", "created")
            .unwrap();

        let restored = RunJournal::create(&path, "run-1", "ticket-1").unwrap();
        assert_eq!(
            restored.executor,
            Some(ExecutorCheckpoint {
                kind: "docker_sandbox".into(),
                id: "rustgrid-run-1".into(),
                state: "created".into(),
            })
        );
    }

    #[test]
    fn rejects_truncated_journal_without_overwriting_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        fs::write(&path, br#"{"schema_version":1,"run_id":"run-1""#).unwrap();

        assert!(RunJournal::create(&path, "run-1", "ticket-1").is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            br#"{"schema_version":1,"run_id":"run-1""#
        );
    }

    #[test]
    fn rejects_incomplete_publication_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        let mut journal = RunJournal::create(&path, "run-1", "ticket-1").unwrap();
        journal.pull_request_url = Some("https://github.com/o/r/pull/1".into());

        assert!(journal.recovery_plan().is_err());
    }
}
