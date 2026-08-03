#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct RepositorySnapshot {
    pub fingerprint: String,
    #[serde(default)]
    pub source_tree_hash: String,
    #[serde(default)]
    pub dependency_lock_hash: String,
    #[serde(default)]
    pub relevant_environment_fingerprint: String,
    #[serde(default)]
    pub changed_paths: BTreeSet<String>,
}

impl RepositorySnapshot {
    pub fn has_changes(&self) -> bool {
        !self.changed_paths.is_empty()
    }

    pub fn contains_changed_path(&self, path: &str) -> bool {
        self.changed_paths.contains(path)
    }

    /// Stable source identity used by validation evidence. Older serialized
    /// snapshots did not carry `source_tree_hash`, so their repository
    /// fingerprint remains the compatibility fallback.
    pub fn validation_source_tree_hash(&self) -> &str {
        if self.source_tree_hash.is_empty() {
            &self.fingerprint
        } else {
            &self.source_tree_hash
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl LineRange {
    pub fn new(start: u32, end: u32) -> Option<Self> {
        (start > 0 && end >= start).then_some(Self { start, end })
    }

    pub const fn contains(self, other: Self) -> bool {
        self.start <= other.start && self.end >= other.end
    }

    pub const fn line_count(self) -> u32 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }
}

impl Default for LineRange {
    fn default() -> Self {
        Self { start: 1, end: 1 }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct FileEvidence {
    pub evidence_id: String,
    pub path: String,
    pub content_hash: String,
    pub repository_fingerprint: String,
    pub line_range: Option<LineRange>,
    pub captured_content: String,
    #[serde(default)]
    pub truncated: bool,
}

impl FileEvidence {
    pub fn capture(
        path: impl Into<String>,
        repository_fingerprint: impl Into<String>,
        line_range: Option<LineRange>,
        captured_content: impl Into<String>,
        truncated: bool,
    ) -> Self {
        let path = path.into();
        let repository_fingerprint = repository_fingerprint.into();
        let captured_content = captured_content.into();
        let content_hash = stable_hash(&captured_content);
        let range_key = line_range
            .map(|range| format!("{}-{}", range.start, range.end))
            .unwrap_or_else(|| "full".to_owned());
        let evidence_id = format!(
            "file-{}",
            &stable_hash(&format!(
                "{path}\0{repository_fingerprint}\0{range_key}\0{content_hash}"
            ))[..20]
        );
        Self {
            evidence_id,
            path,
            content_hash,
            repository_fingerprint,
            line_range,
            captured_content,
            truncated,
        }
    }

    pub fn content_hash_is_valid(&self) -> bool {
        stable_hash(&self.captured_content) == self.content_hash
    }

    pub fn satisfies_range(&self, required: Option<LineRange>) -> bool {
        match required {
            Some(required) => {
                self.line_range
                    .is_some_and(|range| range.contains(required))
                    || (self.line_range.is_none() && !self.truncated)
            }
            None => !self.truncated && self.line_range.is_none(),
        }
    }

    pub fn summary(&self) -> EvidenceSummary {
        EvidenceSummary {
            evidence_id: self.evidence_id.clone(),
            path: Some(self.path.clone()),
            content_hash: Some(self.content_hash.clone()),
            repository_fingerprint: self.repository_fingerprint.clone(),
            line_range: self.line_range,
            summary: if let Some(range) = self.line_range {
                format!("{} lines {}-{}", self.path, range.start, range.end)
            } else {
                format!("{} complete content", self.path)
            },
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct EvidenceSummary {
    pub evidence_id: String,
    pub path: Option<String>,
    pub content_hash: Option<String>,
    pub repository_fingerprint: String,
    pub line_range: Option<LineRange>,
    pub summary: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct FileExcerpt {
    pub path: String,
    pub line_range: LineRange,
    pub content: String,
    pub content_hash: String,
}

impl From<&FileEvidence> for FileExcerpt {
    fn from(evidence: &FileEvidence) -> Self {
        Self {
            path: evidence.path.clone(),
            line_range: evidence.line_range.unwrap_or_default(),
            content: evidence.captured_content.clone(),
            content_hash: evidence.content_hash.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationEvidenceStatus {
    Running,
    #[default]
    Passed,
    Failed,
    TimedOut,
    Cancelled,
    Superseded,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ValidationEvidenceRecord {
    pub evidence_id: String,
    pub node_id: ExecutionNodeId,
    pub gate_id: String,
    pub fingerprint: String,
    pub repository_fingerprint: String,
    pub command: String,
    pub working_directory: String,
    pub status: ValidationEvidenceStatus,
    pub exit_code: Option<i32>,
    pub output_summary: String,
    #[serde(with = "duration_millis")]
    pub duration: Duration,
}

impl ValidationEvidenceRecord {
    pub fn is_reusable_pass(&self, fingerprint: &str) -> bool {
        self.status == ValidationEvidenceStatus::Passed && self.fingerprint == fingerprint
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    #[default]
    RepositoryObservation,
    AcceptanceCriterion,
    Mutation,
    DiffReview,
    Completion,
    Publication,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct EvidenceRecord {
    pub evidence_id: String,
    pub kind: EvidenceKind,
    pub node_id: Option<ExecutionNodeId>,
    pub repository_fingerprint: String,
    pub summary: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct EvidenceStore {
    #[serde(default)]
    pub files: BTreeMap<String, FileEvidence>,
    #[serde(default)]
    pub validations: BTreeMap<String, ValidationEvidenceRecord>,
    #[serde(default)]
    pub records: BTreeMap<String, EvidenceRecord>,
}

impl EvidenceStore {
    /// Inserts evidence once and returns its stable id. An identical read is a
    /// cache hit, not a second evidence record or progress event.
    pub fn record_file(&mut self, evidence: FileEvidence) -> String {
        let id = evidence.evidence_id.clone();
        self.files.entry(id.clone()).or_insert(evidence);
        id
    }

    pub fn capture_file(
        &mut self,
        path: impl Into<String>,
        repository_fingerprint: impl Into<String>,
        line_range: Option<LineRange>,
        content: impl Into<String>,
        truncated: bool,
    ) -> String {
        self.record_file(FileEvidence::capture(
            path,
            repository_fingerprint,
            line_range,
            content,
            truncated,
        ))
    }

    pub fn reusable_file(
        &self,
        path: &str,
        repository_fingerprint: &str,
        required_range: Option<LineRange>,
    ) -> Option<&FileEvidence> {
        self.files
            .values()
            .filter(|evidence| {
                evidence.path == path
                    && evidence.repository_fingerprint == repository_fingerprint
                    && evidence.content_hash_is_valid()
                    && evidence.satisfies_range(required_range)
            })
            .max_by_key(|evidence| {
                evidence
                    .line_range
                    .map(LineRange::line_count)
                    .unwrap_or(u32::MAX)
            })
    }

    pub fn lookup_file(
        &self,
        path: &str,
        repository_fingerprint: &str,
        required_range: Option<LineRange>,
    ) -> Option<&FileEvidence> {
        self.reusable_file(path, repository_fingerprint, required_range)
    }

    pub fn record_validation(&mut self, evidence: ValidationEvidenceRecord) -> String {
        let id = evidence.evidence_id.clone();
        self.validations.entry(id.clone()).or_insert(evidence);
        id
    }

    pub fn passed_validation(&self, fingerprint: &str) -> Option<&ValidationEvidenceRecord> {
        self.validations
            .values()
            .find(|evidence| evidence.is_reusable_pass(fingerprint))
    }

    pub fn has_passed_validation(&self, fingerprint: &str) -> bool {
        self.passed_validation(fingerprint).is_some()
    }

    pub fn supersede_stale_validation(&mut self, repository_fingerprint: &str) -> usize {
        let mut count = 0;
        for evidence in self.validations.values_mut().filter(|evidence| {
            evidence.status == ValidationEvidenceStatus::Passed
                && evidence.repository_fingerprint != repository_fingerprint
        }) {
            evidence.status = ValidationEvidenceStatus::Superseded;
            count += 1;
        }
        count
    }

    pub fn record(&mut self, evidence: EvidenceRecord) -> String {
        let id = evidence.evidence_id.clone();
        self.records.entry(id.clone()).or_insert(evidence);
        id
    }

    pub fn summary(&self, evidence_id: &str) -> Option<EvidenceSummary> {
        if let Some(file) = self.files.get(evidence_id) {
            return Some(file.summary());
        }
        self.records.get(evidence_id).map(|record| EvidenceSummary {
            evidence_id: record.evidence_id.clone(),
            path: None,
            content_hash: None,
            repository_fingerprint: record.repository_fingerprint.clone(),
            line_range: None,
            summary: record.summary.clone(),
        })
    }
}
