// Extracted from the hosted execution composition root.
use super::*;
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
use std::ffi::CString;
use std::io::Write as _;
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
use std::os::unix::ffi::OsStrExt as _;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(in crate::hosted) struct PatchTargetValidationError {
    pub(in crate::hosted) declared_target: String,
    pub(in crate::hosted) raw_old_paths: Vec<String>,
    pub(in crate::hosted) raw_new_paths: Vec<String>,
    pub(in crate::hosted) normalized_paths: Vec<String>,
    pub(in crate::hosted) file_section_count: usize,
    pub(in crate::hosted) rejection_reason: String,
}

impl std::fmt::Display for PatchTargetValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            &serde_json::to_string(self)
                .unwrap_or_else(|_| "patch target validation failed".into()),
        )
    }
}

impl std::error::Error for PatchTargetValidationError {}

#[derive(Clone, Debug)]
pub(in crate::hosted) struct MutationApplicationError {
    pub(in crate::hosted) failure: MutationApplicationFailure,
    pub(in crate::hosted) message: String,
    pub(in crate::hosted) patch_validation: Option<PatchTargetValidationError>,
    pub(in crate::hosted) git_apply_check: Option<String>,
    pub(in crate::hosted) raw_patch_sha256: Option<String>,
    pub(in crate::hosted) target_content_hash: Option<String>,
}

impl MutationApplicationError {
    fn new(failure: MutationApplicationFailure, message: impl Into<String>) -> Self {
        Self {
            failure,
            message: message.into(),
            patch_validation: None,
            git_apply_check: None,
            raw_patch_sha256: None,
            target_content_hash: None,
        }
    }
}

impl std::fmt::Display for MutationApplicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "mutation_application_failure:{}: {}",
            self.failure.as_str(),
            self.message
        )
    }
}

impl std::error::Error for MutationApplicationError {}

fn rename_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source path contains NUL")
    })?;
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination path contains NUL",
        )
    })?;

    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_vendor = "apple")]
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    {
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        let _ = (source, destination);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic no-clobber rename is unsupported on this platform",
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ValidatedPatch {
    canonical_patch: String,
    diagnostics: PatchTargetValidationError,
}

fn quoted_git_path(value: &str) -> Result<String> {
    let value = value.trim();
    if !value.starts_with('"') {
        return Ok(value
            .split_once('\t')
            .map_or(value, |(path, _)| path)
            .trim()
            .to_owned());
    }
    let mut escaped = false;
    let mut path = String::new();
    for character in value[1..].chars() {
        if escaped {
            path.push(match character {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            return Ok(path);
        } else {
            path.push(character);
        }
    }
    bail!("Git patch path has unterminated quoting")
}

pub(in crate::hosted) fn normalize_patch_repository_path(root: &Path, raw: &str) -> Result<String> {
    let mut value = quoted_git_path(raw)?.replace('\\', "/");
    if value == "/dev/null" || value == "dev/null" {
        bail!("Git null path is not valid for an existing target mutation");
    }
    if value.starts_with('/')
        || value.starts_with("//")
        || value
            .as_bytes()
            .get(1)
            .is_some_and(|separator| *separator == b':')
    {
        bail!("absolute patch paths are forbidden");
    }
    while let Some(stripped) = value.strip_prefix("./") {
        value = stripped.to_owned();
    }
    if let Some(stripped) = value
        .strip_prefix("a/")
        .or_else(|| value.strip_prefix("b/"))
    {
        value = stripped.to_owned();
    }
    while let Some(stripped) = value.strip_prefix("./") {
        value = stripped.to_owned();
    }
    let mut normalized = Vec::new();
    for component in value.split('/') {
        match component {
            "" | "." => {}
            ".." => bail!("parent traversal is forbidden in patch paths"),
            component => normalized.push(component),
        }
    }
    if normalized.is_empty() {
        bail!("patch path is empty after normalization");
    }
    let normalized = normalized.join("/");
    safe_repo_path(root, &normalized, true)?;
    Ok(normalized)
}

fn patch_header_path(line: &str, prefix: &str) -> Result<String> {
    quoted_git_path(
        line.strip_prefix(prefix)
            .context("patch header has an unexpected prefix")?,
    )
}

fn take_git_path_token(value: &str) -> Result<(&str, &str)> {
    let value = value.trim_start();
    if !value.starts_with('"') {
        let end = value.find(char::is_whitespace).unwrap_or(value.len());
        return Ok((&value[..end], &value[end..]));
    }
    let mut escaped = false;
    for (index, character) in value[1..].char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            let end = index + 2;
            return Ok((&value[..end], &value[end..]));
        }
    }
    bail!("Git patch path has unterminated quoting")
}

fn diff_git_paths(line: &str) -> Result<(String, String)> {
    let remainder = line
        .strip_prefix("diff --git ")
        .context("patch diff header has an unexpected prefix")?;
    let (old, remainder) = take_git_path_token(remainder)?;
    let (new, remainder) = take_git_path_token(remainder)?;
    if !remainder.trim().is_empty() {
        bail!("Git diff header contains unexpected trailing data");
    }
    Ok((quoted_git_path(old)?, quoted_git_path(new)?))
}

fn validate_patch_target(
    root: &Path,
    declared_target: &str,
    patch: &str,
) -> Result<ValidatedPatch> {
    let declared_target = normalize_patch_repository_path(root, declared_target)?;
    let raw_old_paths = patch
        .lines()
        .filter(|line| line.starts_with("--- "))
        .map(|line| patch_header_path(line, "--- "))
        .collect::<Result<Vec<_>>>()?;
    let raw_new_paths = patch
        .lines()
        .filter(|line| line.starts_with("+++ "))
        .map(|line| patch_header_path(line, "+++ "))
        .collect::<Result<Vec<_>>>()?;
    let diff_headers = patch
        .lines()
        .filter(|line| line.starts_with("diff --git "))
        .collect::<Vec<_>>();
    let file_section_count = raw_old_paths
        .len()
        .max(raw_new_paths.len())
        .max(diff_headers.len());
    let parsed_diff_paths = diff_headers
        .iter()
        .map(|line| diff_git_paths(line))
        .collect::<Result<Vec<_>>>();
    let mut normalized_paths = Vec::new();
    let mut normalization_error = None;
    let mut normalize = |raw: &str| -> Option<String> {
        match normalize_patch_repository_path(root, raw) {
            Ok(normalized) => {
                normalized_paths.push(normalized.clone());
                Some(normalized)
            }
            Err(error) => {
                normalization_error.get_or_insert_with(|| error.to_string());
                None
            }
        }
    };
    let normalized_old = raw_old_paths
        .iter()
        .filter_map(|path| normalize(path))
        .collect::<Vec<_>>();
    let normalized_new = raw_new_paths
        .iter()
        .filter_map(|path| normalize(path))
        .collect::<Vec<_>>();
    let normalized_diff = parsed_diff_paths.as_ref().map(|paths| {
        paths
            .iter()
            .filter_map(|(old, new)| Some((normalize(old)?, normalize(new)?)))
            .collect::<Vec<_>>()
    });
    let unsafe_metadata = patch.lines().find(|line| {
        [
            "rename from ",
            "rename to ",
            "copy from ",
            "copy to ",
            "new file mode ",
            "deleted file mode ",
            "GIT binary patch",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
    });
    let rejection_reason = if let Err(error) = parsed_diff_paths {
        Some(format!("invalid_diff_header:{error}"))
    } else if let Some(error) = normalization_error {
        Some(format!("invalid_patch_path:{error}"))
    } else if file_section_count != 1
        || raw_old_paths.len() != 1
        || raw_new_paths.len() != 1
        || diff_headers.len() > 1
    {
        Some("patch_must_contain_exactly_one_complete_file_section".to_owned())
    } else if let Some(metadata) = unsafe_metadata {
        Some(format!("unsupported_patch_metadata:{metadata}"))
    } else if normalized_old.first() != normalized_new.first()
        || normalized_diff
            .as_ref()
            .is_ok_and(|paths| paths.iter().any(|(old, new)| old != new))
    {
        Some("patch_rename_is_forbidden".to_owned())
    } else if normalized_old.first() != Some(&declared_target)
        || normalized_new.first() != Some(&declared_target)
        || normalized_diff.as_ref().is_ok_and(|paths| {
            paths
                .iter()
                .any(|(old, new)| old != &declared_target || new != &declared_target)
        })
    {
        Some("patch_would_modify_unexpected_path".to_owned())
    } else {
        None
    };
    let diagnostics = PatchTargetValidationError {
        declared_target: declared_target.clone(),
        raw_old_paths,
        raw_new_paths,
        normalized_paths,
        file_section_count,
        rejection_reason: rejection_reason.clone().unwrap_or_default(),
    };
    if let Some(rejection_reason) = rejection_reason {
        return Err(anyhow!(MutationApplicationError {
            failure: if rejection_reason == "patch_would_modify_unexpected_path" {
                MutationApplicationFailure::PatchWouldModifyUnexpectedPath
            } else {
                MutationApplicationFailure::InvalidPatchTarget
            },
            message: format!("patch target validation failed: {diagnostics}"),
            patch_validation: Some(diagnostics),
            git_apply_check: None,
            raw_patch_sha256: Some(sha256_text(patch)),
            target_content_hash: None,
        }));
    }
    let mut canonical = Vec::new();
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            canonical.push(format!(
                "diff --git a/{declared_target} b/{declared_target}"
            ));
        } else if line.starts_with("--- ") {
            canonical.push(format!("--- a/{declared_target}"));
        } else if line.starts_with("+++ ") {
            canonical.push(format!("+++ b/{declared_target}"));
        } else {
            canonical.push(line.to_owned());
        }
    }
    let mut canonical_patch = canonical.join("\n");
    if patch.ends_with('\n') {
        canonical_patch.push('\n');
    }
    Ok(ValidatedPatch {
        canonical_patch,
        diagnostics,
    })
}

pub(in crate::hosted) fn patch_target_diagnostics(
    root: &Path,
    declared_target: &str,
    patch: &str,
) -> Result<PatchTargetValidationError> {
    Ok(validate_patch_target(root, declared_target, patch)?.diagnostics)
}

fn hunk_counts(header: &str) -> Option<(usize, usize)> {
    let mut parts = header.split_whitespace();
    (parts.next()? == "@@").then_some(())?;
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let count = |value: &str| {
        value
            .split_once(',')
            .map_or(Some(1), |(_, count)| count.parse::<usize>().ok())
    };
    Some((count(old)?, count(new)?))
}

fn normalize_hunk_offsets(patch: &str, content: &str) -> Result<String> {
    let lines = patch.lines().collect::<Vec<_>>();
    let content_lines = content
        .split_terminator('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect::<Vec<_>>();
    let mut normalized = Vec::with_capacity(lines.len());
    let mut index = 0;
    let mut cumulative_delta = 0_i64;
    let mut saw_hunk = false;
    while index < lines.len() {
        let line = lines[index];
        if !line.starts_with("@@ ") {
            normalized.push(line.to_owned());
            index += 1;
            continue;
        }
        saw_hunk = true;
        let (_, _) = hunk_counts(line).ok_or_else(|| {
            anyhow!(MutationApplicationError::new(
                MutationApplicationFailure::InvalidPatchSyntax,
                "unified diff has an invalid hunk header",
            ))
        })?;
        let start = index + 1;
        let mut end = start;
        while end < lines.len()
            && !lines[end].starts_with("@@ ")
            && !lines[end].starts_with("diff --git ")
            && !lines[end].starts_with("--- ")
        {
            end += 1;
        }
        let old_sequence = lines[start..end]
            .iter()
            .filter_map(|line| match line.chars().next() {
                Some(' ') | Some('-') => Some(&line[1..]),
                Some('+') | Some('\\') => None,
                _ => None,
            })
            .collect::<Vec<_>>();
        if old_sequence.is_empty() {
            return Err(anyhow!(MutationApplicationError::new(
                MutationApplicationFailure::PatchContextMismatch,
                "unified diff hunk has no exact old-content context",
            )));
        }
        let matches = content_lines
            .windows(old_sequence.len())
            .enumerate()
            .filter_map(|(position, candidate)| (candidate == old_sequence).then_some(position))
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(anyhow!(MutationApplicationError::new(
                MutationApplicationFailure::PatchContextMismatch,
                if matches.is_empty() {
                    "unified diff old-content context is absent from the exact target content"
                } else {
                    "unified diff old-content context is ambiguous in the exact target content"
                },
            )));
        }
        let old_count = old_sequence.len();
        let new_count = lines[start..end]
            .iter()
            .filter(|line| !line.starts_with('-') && !line.starts_with('\\'))
            .count();
        let old_start = matches[0] + 1;
        let new_start = i64::try_from(old_start)
            .unwrap_or(i64::MAX)
            .saturating_add(cumulative_delta)
            .max(1);
        normalized.push(format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
        ));
        normalized.extend(lines[start..end].iter().map(|line| (*line).to_owned()));
        cumulative_delta = cumulative_delta.saturating_add(
            i64::try_from(new_count).unwrap_or(i64::MAX)
                - i64::try_from(old_count).unwrap_or(i64::MAX),
        );
        index = end;
    }
    if !saw_hunk {
        return Err(anyhow!(MutationApplicationError::new(
            MutationApplicationFailure::InvalidPatchSyntax,
            "unified diff contains no hunks",
        )));
    }
    let mut output = normalized.join("\n");
    if patch.ends_with('\n') {
        output.push('\n');
    }
    Ok(output)
}

#[cfg(test)]
pub(in crate::hosted) fn replace_unique_repo_text(
    root: &Path,
    path: &str,
    old_text: &str,
    new_text: &str,
) -> Result<String> {
    let target = safe_repo_path(root, path, false)?;
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    let positions = content
        .match_indices(old_text)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let matches = positions.len();
    if matches != 1 {
        bail!(
            "replace_match_not_unique: replace_text requires exactly one match in {path}; found {matches}"
        );
    }
    let before_sha256 = sha256_text(&content);
    let start_line = content[..positions[0]]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    let end_line = start_line + old_text.lines().count().max(1) - 1;
    let updated = content.replacen(old_text, new_text, 1);
    if updated.len() > MAX_MODEL_FILE_BYTES {
        bail!("replace_text result exceeds the hosted tool limit");
    }
    fs::write(&target, updated.as_bytes())
        .with_context(|| format!("could not write repository file {path}"))?;
    mutation_output(
        path,
        Some(before_sha256),
        Some(sha256_text(&updated)),
        format!("{start_line}-{end_line}"),
        format!(
            "replaced {} bytes with {} bytes",
            old_text.len(),
            new_text.len()
        ),
    )
}

pub(in crate::hosted) fn sha256_text(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

pub(in crate::hosted) fn mutation_output(
    path: &str,
    before_sha256: Option<String>,
    after_sha256: Option<String>,
    changed_range: String,
    diff_summary: String,
) -> Result<String> {
    serde_json::to_string(&json!({
        "path": path,
        "before_sha256": before_sha256,
        "after_sha256": after_sha256,
        "changed_range": changed_range,
        "diff_summary": diff_summary,
    }))
    .context("could not serialize mutation result")
}

pub(in crate::hosted) fn write_repo_file(
    root: &Path,
    path: &str,
    content: &str,
    small_only: bool,
) -> Result<String> {
    let target = safe_repo_path(root, path, true)?;
    let previous = fs::read_to_string(&target).ok();
    if small_only
        && previous
            .as_ref()
            .is_none_or(|value| value.len() > MAX_SMALL_FILE_REWRITE_BYTES)
    {
        bail!(
            "rewrite_small_file requires an existing UTF-8 file no larger than {MAX_SMALL_FILE_REWRITE_BYTES} bytes"
        );
    }
    if content.len() > MAX_MODEL_FILE_BYTES
        || (small_only && content.len() > MAX_SMALL_FILE_REWRITE_BYTES)
    {
        bail!("complete file content exceeds the hosted tool limit");
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("could not create repository directory {}", parent.display())
        })?;
    }
    fs::write(&target, content.as_bytes())
        .with_context(|| format!("could not write repository file {path}"))?;
    mutation_output(
        path,
        previous.as_deref().map(sha256_text),
        Some(sha256_text(content)),
        "complete_file".into(),
        format!(
            "{} complete UTF-8 file with {} bytes",
            if small_only { "rewrote" } else { "wrote" },
            content.len()
        ),
    )
}

pub(in crate::hosted) fn replace_repo_range(
    root: &Path,
    path: &str,
    start_line: usize,
    end_line: usize,
    new_text: &str,
) -> Result<String> {
    if start_line == 0 || end_line < start_line {
        bail!("replace_range requires a valid inclusive line range");
    }
    let target = safe_repo_path(root, path, false)?;
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    let lines = content.split_inclusive('\n').collect::<Vec<_>>();
    if end_line > lines.len().max(1) {
        bail!("replace_range line range exceeds {path}");
    }
    let start_offset = lines
        .iter()
        .take(start_line - 1)
        .map(|line| line.len())
        .sum::<usize>();
    let end_offset = lines
        .iter()
        .take(end_line)
        .map(|line| line.len())
        .sum::<usize>();
    let mut updated = String::with_capacity(
        content
            .len()
            .saturating_sub(end_offset.saturating_sub(start_offset))
            .saturating_add(new_text.len()),
    );
    updated.push_str(&content[..start_offset]);
    updated.push_str(new_text);
    updated.push_str(&content[end_offset..]);
    if updated.len() > MAX_MODEL_FILE_BYTES {
        bail!("replace_range result exceeds the hosted tool limit");
    }
    fs::write(&target, updated.as_bytes())
        .with_context(|| format!("could not write repository file {path}"))?;
    mutation_output(
        path,
        Some(sha256_text(&content)),
        Some(sha256_text(&updated)),
        format!("{start_line}-{end_line}"),
        format!("replaced inclusive line range {start_line}-{end_line}"),
    )
}

#[cfg(test)]
pub(in crate::hosted) fn insert_relative_to_symbol(
    root: &Path,
    path: &str,
    symbol: &str,
    inserted: &str,
    after: bool,
) -> Result<String> {
    let target = safe_repo_path(root, path, false)?;
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    let positions = content
        .match_indices(symbol)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if positions.len() != 1 {
        bail!(
            "symbol_match_not_unique: symbol insertion requires one match in {path}; found {}",
            positions.len()
        );
    }
    let offset = positions[0] + usize::from(after) * symbol.len();
    let mut updated = String::with_capacity(content.len().saturating_add(inserted.len()));
    updated.push_str(&content[..offset]);
    updated.push_str(inserted);
    updated.push_str(&content[offset..]);
    if updated.len() > MAX_MODEL_FILE_BYTES {
        bail!("symbol insertion result exceeds the hosted tool limit");
    }
    fs::write(&target, updated.as_bytes())
        .with_context(|| format!("could not write repository file {path}"))?;
    let line = content[..positions[0]]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    mutation_output(
        path,
        Some(sha256_text(&content)),
        Some(sha256_text(&updated)),
        line.to_string(),
        format!(
            "inserted {} bytes {} unique symbol",
            inserted.len(),
            if after { "after" } else { "before" }
        ),
    )
}

pub(in crate::hosted) fn apply_repo_unified_diff_with_context(
    root: &Path,
    path: &str,
    patch: &str,
    expected_target_content_hash: Option<&str>,
) -> Result<String> {
    if patch.len() > MAX_MODEL_FILE_BYTES {
        return Err(anyhow!(MutationApplicationError::new(
            MutationApplicationFailure::InvalidPatchSyntax,
            "unified diff exceeds the hosted tool limit",
        )));
    }
    let target = safe_repo_path(root, path, false)?;
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    let target_content_hash = sha256_text(&content);
    if expected_target_content_hash.is_some_and(|expected| expected != target_content_hash.as_str())
    {
        return Err(anyhow!(MutationApplicationError {
            failure: MutationApplicationFailure::RepositoryChangedSinceContext,
            message: "target content changed after deterministic context preparation".into(),
            patch_validation: None,
            git_apply_check: None,
            raw_patch_sha256: Some(sha256_text(patch)),
            target_content_hash: Some(target_content_hash),
        }));
    }
    let validated = validate_patch_target(root, path, patch).map_err(|error| {
        if let Some(application) = error.downcast_ref::<MutationApplicationError>() {
            let mut application = application.clone();
            application.target_content_hash = Some(target_content_hash.clone());
            anyhow!(application)
        } else {
            error
        }
    })?;
    let patch_path = env::temp_dir().join(format!(
        "rustgrid-agent-unified-diff-{}.patch",
        Uuid::new_v4().simple()
    ));
    let applied_patch =
        normalize_hunk_offsets(&validated.canonical_patch, &content).map_err(|error| {
            if let Some(application) = error.downcast_ref::<MutationApplicationError>() {
                let mut application = application.clone();
                application.patch_validation = Some(validated.diagnostics.clone());
                application.raw_patch_sha256 = Some(sha256_text(patch));
                application.target_content_hash = Some(target_content_hash.clone());
                anyhow!(application)
            } else {
                error
            }
        })?;
    fs::write(&patch_path, applied_patch.as_bytes()).context("could not write patch file")?;
    let patch_path_text = patch_path.to_string_lossy().into_owned();
    let checked = command::capture(
        "git",
        [
            "apply",
            "--check",
            "--whitespace=nowarn",
            patch_path_text.as_str(),
        ],
        root,
    )?;
    if !checked.status.success() {
        let stderr = truncate_text(&checked.stderr, 4_000);
        let lower = stderr.to_ascii_lowercase();
        let failure = if lower.contains("corrupt patch")
            || lower.contains("unrecognized input")
            || lower.contains("patch fragment without header")
        {
            MutationApplicationFailure::InvalidPatchSyntax
        } else {
            MutationApplicationFailure::PatchContextMismatch
        };
        let _ = fs::remove_file(&patch_path);
        return Err(anyhow!(MutationApplicationError {
            failure,
            message: format!("unified diff validation failed: {stderr}"),
            patch_validation: Some(validated.diagnostics),
            git_apply_check: Some(stderr),
            raw_patch_sha256: Some(sha256_text(patch)),
            target_content_hash: Some(target_content_hash),
        }));
    }
    let applied = command::capture(
        "git",
        ["apply", "--whitespace=nowarn", patch_path_text.as_str()],
        root,
    )?;
    let _ = fs::remove_file(&patch_path);
    if !applied.status.success() {
        return Err(anyhow!(MutationApplicationError {
            failure: MutationApplicationFailure::PatchContextMismatch,
            message: format!(
                "unified diff application failed after validation: {}",
                truncate_text(&applied.stderr, 4_000)
            ),
            patch_validation: Some(validated.diagnostics),
            git_apply_check: Some(truncate_text(&checked.stderr, 4_000)),
            raw_patch_sha256: Some(sha256_text(patch)),
            target_content_hash: Some(target_content_hash),
        }));
    }
    let updated = fs::read_to_string(&target)
        .with_context(|| format!("could not read patched UTF-8 repository file {path}"))?;
    if updated == content {
        return Err(anyhow!(MutationApplicationError {
            failure: MutationApplicationFailure::MutationProducedNoChange,
            message: "unified diff produced no target content change".into(),
            patch_validation: Some(validated.diagnostics),
            git_apply_check: Some(String::new()),
            raw_patch_sha256: Some(sha256_text(patch)),
            target_content_hash: Some(target_content_hash),
        }));
    }
    mutation_output(
        path,
        Some(sha256_text(&content)),
        Some(sha256_text(&updated)),
        "unified_diff".into(),
        format!("applied {}-byte unified diff", applied_patch.len()),
    )
}

#[cfg(test)]
pub(in crate::hosted) fn apply_repo_unified_diff(
    root: &Path,
    path: &str,
    patch: &str,
) -> Result<String> {
    apply_repo_unified_diff_with_context(root, path, patch, None)
}

fn normalize_replacement_line_endings(content: &str, replacement: &str) -> String {
    let normalized = replacement.replace("\r\n", "\n").replace('\r', "\n");
    if content.contains("\r\n") {
        normalized.replace('\n', "\r\n")
    } else {
        normalized
    }
}

pub(in crate::hosted) fn replace_repo_file_atomically(
    root: &Path,
    path: &str,
    replacement: &str,
    expected_target_content_hash: Option<&str>,
) -> Result<String> {
    let target = safe_repo_path(root, path, false)?;
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    let target_content_hash = sha256_text(&content);
    if expected_target_content_hash.is_some_and(|expected| expected != target_content_hash.as_str())
    {
        return Err(anyhow!(MutationApplicationError {
            failure: MutationApplicationFailure::RepositoryChangedSinceContext,
            message: "target content changed after deterministic context preparation".into(),
            patch_validation: None,
            git_apply_check: None,
            raw_patch_sha256: None,
            target_content_hash: Some(target_content_hash),
        }));
    }
    if replacement.is_empty()
        || replacement.contains('\0')
        || replacement.len() > MAX_MODEL_FILE_BYTES
    {
        return Err(anyhow!(MutationApplicationError {
            failure: MutationApplicationFailure::ReplacementContentInvalid,
            message: "replacement content is empty, contains NUL, or exceeds the hosted limit"
                .into(),
            patch_validation: None,
            git_apply_check: None,
            raw_patch_sha256: None,
            target_content_hash: Some(target_content_hash),
        }));
    }
    let replacement = normalize_replacement_line_endings(&content, replacement);
    if replacement == content {
        return Err(anyhow!(MutationApplicationError {
            failure: MutationApplicationFailure::MutationProducedNoChange,
            message: "full-file replacement produced no target content change".into(),
            patch_validation: None,
            git_apply_check: None,
            raw_patch_sha256: None,
            target_content_hash: Some(target_content_hash),
        }));
    }
    let parent = target
        .parent()
        .context("replacement target has no repository parent")?;
    let temporary = parent.join(format!(".rustgrid-replacement-{}", Uuid::new_v4().simple()));
    let write_result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .context("could not create atomic replacement file")?;
        file.write_all(replacement.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &target)
            .with_context(|| format!("could not atomically replace repository file {path}"))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result?;
    let updated = fs::read_to_string(&target)
        .with_context(|| format!("could not verify replaced UTF-8 repository file {path}"))?;
    if updated != replacement {
        bail!("atomic replacement verification did not match the requested target content");
    }
    mutation_output(
        path,
        Some(target_content_hash),
        Some(sha256_text(&updated)),
        "complete_file".into(),
        format!("atomically replaced {}-byte file", updated.len()),
    )
}

pub(in crate::hosted) fn create_repo_file_atomically(
    root: &Path,
    path: &str,
    content: &str,
    create_parents: bool,
) -> Result<String> {
    if content.is_empty() || content.contains('\0') || content.len() > MAX_MODEL_FILE_BYTES {
        return Err(anyhow!(MutationApplicationError::new(
            MutationApplicationFailure::ReplacementContentInvalid,
            "creation content is empty, contains NUL, or exceeds the hosted limit",
        )));
    }
    let target = safe_repo_path(root, path, true)?;
    if target.exists() {
        return Err(anyhow!(MutationApplicationError::new(
            MutationApplicationFailure::CreateTargetAlreadyExists,
            "create_file requires an absent target",
        )));
    }
    let parent = target
        .parent()
        .context("creation target has no repository parent")?;
    if !parent.exists() {
        if !create_parents {
            bail!("create_parent_missing: parent creation was not explicitly permitted");
        }
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create safe parent directories for {path}"))?;
        safe_repo_path(root, path, true)?;
    }
    let temporary = parent.join(format!(".rustgrid-creation-{}", Uuid::new_v4().simple()));
    let write_result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .context("could not create atomic creation file")?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        rename_no_replace(&temporary, &target).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow!(MutationApplicationError::new(
                    MutationApplicationFailure::CreateTargetAlreadyExists,
                    "target appeared during atomic creation",
                ))
            } else {
                anyhow!(error).context(format!(
                    "could not atomically create repository file {path}"
                ))
            }
        })?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result?;
    let verified = fs::read_to_string(&target)
        .with_context(|| format!("could not verify created UTF-8 repository file {path}"))?;
    if verified != content {
        bail!("created target verification failed");
    }
    mutation_output(
        path,
        None,
        Some(sha256_text(&verified)),
        "complete_file".into(),
        format!("created {}-byte file", verified.len()),
    )
}

pub(in crate::hosted) fn move_repo_file_atomically(
    root: &Path,
    source_path: &str,
    destination_path: &str,
    expected_source_content_hash: Option<&str>,
    create_parents: bool,
) -> Result<String> {
    let source = safe_repo_path(root, source_path, false)?;
    let destination = safe_repo_path(root, destination_path, true)?;
    if !source.is_file() {
        bail!("expected_source_target_missing: source is not a regular file");
    }
    if destination.exists() {
        return Err(anyhow!(MutationApplicationError::new(
            MutationApplicationFailure::RenameDestinationConflict,
            "move destination must be absent",
        )));
    }
    let content = fs::read_to_string(&source)
        .with_context(|| format!("could not read UTF-8 source file {source_path}"))?;
    let source_hash = sha256_text(&content);
    if expected_source_content_hash.is_some_and(|expected| expected != source_hash) {
        return Err(anyhow!(MutationApplicationError {
            failure: MutationApplicationFailure::RepositoryChangedSinceContext,
            message: "source content changed after deterministic context preparation".into(),
            patch_validation: None,
            git_apply_check: None,
            raw_patch_sha256: None,
            target_content_hash: Some(source_hash),
        }));
    }
    let parent = destination
        .parent()
        .context("move destination has no repository parent")?;
    if !parent.exists() {
        if !create_parents {
            bail!("move_parent_missing: parent creation was not explicitly permitted");
        }
        fs::create_dir_all(parent).with_context(|| {
            format!("could not create safe parent directories for {destination_path}")
        })?;
        safe_repo_path(root, destination_path, true)?;
    }
    rename_no_replace(&source, &destination).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow!(MutationApplicationError::new(
                MutationApplicationFailure::RenameDestinationConflict,
                "move destination appeared during relocation",
            ))
        } else {
            anyhow!(error).context(format!(
                "could not move repository file {source_path} to {destination_path}"
            ))
        }
    })?;
    let verified = fs::read_to_string(&destination).with_context(|| {
        format!("could not verify moved UTF-8 repository file {destination_path}")
    })?;
    if source.exists() || verified != content {
        bail!("moved target verification failed");
    }
    mutation_output(
        destination_path,
        None,
        Some(sha256_text(&verified)),
        "complete_file".into(),
        format!("moved {source_path} to {destination_path}"),
    )
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // Names are the persisted mutation contract requested by the worker protocol.
pub(crate) enum StructuredEdit {
    ReplaceExactText {
        path: String,
        expected: String,
        replacement: String,
        expected_occurrences: usize,
    },
    InsertAfterExactText {
        path: String,
        anchor: String,
        content: String,
    },
    InsertBeforeExactText {
        path: String,
        anchor: String,
        content: String,
    },
}

pub(crate) fn apply_structured_edit(
    root: &Path,
    edit: &StructuredEdit,
    expected_target_content_hash: Option<&str>,
) -> Result<String> {
    let (path, expected, replacement, expected_occurrences) = match edit {
        StructuredEdit::ReplaceExactText {
            path,
            expected,
            replacement,
            expected_occurrences,
        } => (path, expected, replacement.clone(), *expected_occurrences),
        StructuredEdit::InsertAfterExactText {
            path,
            anchor,
            content,
        } => (path, anchor, format!("{anchor}{content}"), 1),
        StructuredEdit::InsertBeforeExactText {
            path,
            anchor,
            content,
        } => (path, anchor, format!("{content}{anchor}"), 1),
    };
    if expected.is_empty() || expected_occurrences == 0 {
        return Err(anyhow!(MutationApplicationError::new(
            MutationApplicationFailure::ReplacementContentInvalid,
            "structured edit requires a non-empty exact anchor and positive occurrence count",
        )));
    }
    let target = safe_repo_path(root, path, false)?;
    let current = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    let current_hash = sha256_text(&current);
    if expected_target_content_hash.is_some_and(|expected| expected != current_hash) {
        return Err(anyhow!(MutationApplicationError {
            failure: MutationApplicationFailure::RepositoryChangedSinceContext,
            message: "target content changed after deterministic context preparation".into(),
            patch_validation: None,
            git_apply_check: None,
            raw_patch_sha256: None,
            target_content_hash: Some(current_hash),
        }));
    }
    let occurrences = current.matches(expected).count();
    if occurrences != expected_occurrences {
        return Err(anyhow!(MutationApplicationError::new(
            MutationApplicationFailure::PatchContextMismatch,
            format!(
                "structured edit expected {expected_occurrences} exact occurrence(s), found {occurrences}"
            ),
        )));
    }
    let updated = current.replacen(expected, &replacement, expected_occurrences);
    replace_repo_file_atomically(root, path, &updated, Some(&current_hash))
}

pub(in crate::hosted) fn delete_repo_file(
    root: &Path,
    path: &str,
    expected_target_content_hash: Option<&str>,
) -> Result<String> {
    let target = safe_repo_path(root, path, false)?;
    if !target.is_file() {
        return Err(anyhow!(MutationApplicationError::new(
            MutationApplicationFailure::DeleteTargetMissing,
            "delete_file target is not a regular file",
        )));
    }
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    let content_hash = sha256_text(&content);
    if expected_target_content_hash.is_some_and(|expected| expected != content_hash) {
        return Err(anyhow!(MutationApplicationError {
            failure: MutationApplicationFailure::RepositoryChangedSinceContext,
            message: "delete target changed after deterministic context preparation".into(),
            patch_validation: None,
            git_apply_check: None,
            raw_patch_sha256: None,
            target_content_hash: Some(content_hash),
        }));
    }
    fs::remove_file(&target).with_context(|| format!("could not delete repository file {path}"))?;
    mutation_output(
        path,
        Some(content_hash),
        None,
        "complete_file".into(),
        format!("deleted {}-byte file", content.len()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = "src/components/theme/ThemeProvider.tsx";

    fn repository(content: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temporary repository");
        let target = directory.path().join(TARGET);
        fs::create_dir_all(target.parent().expect("target parent")).expect("target directory");
        fs::write(target, content).expect("target content");
        directory
    }

    #[test]
    fn create_file_requires_an_absent_target() {
        let directory = repository("existing\n");
        let error = create_repo_file_atomically(directory.path(), TARGET, "new\n", false)
            .expect_err("existing creation target must conflict");
        assert!(error.to_string().contains("create_target_already_exists"));
        assert_eq!(
            fs::read_to_string(directory.path().join(TARGET)).unwrap(),
            "existing\n"
        );
    }

    #[test]
    fn no_clobber_rename_preserves_a_destination_that_already_exists() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::write(&source, "source\n").unwrap();
        fs::write(&destination, "destination\n").unwrap();
        let error = rename_no_replace(&source, &destination).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(source).unwrap(), "source\n");
        assert_eq!(fs::read_to_string(destination).unwrap(), "destination\n");
    }

    #[test]
    fn create_file_is_atomic_and_verifies_content() {
        let directory = tempfile::tempdir().unwrap();
        let output =
            create_repo_file_atomically(directory.path(), "src/new.rs", "pub fn new() {}\n", true)
                .unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join("src/new.rs")).unwrap(),
            "pub fn new() {}\n"
        );
        let output: Value = serde_json::from_str(&output).unwrap();
        assert!(output["before_sha256"].is_null());
        assert_eq!(output["after_sha256"], sha256_text("pub fn new() {}\n"));
    }

    #[test]
    fn create_file_does_not_create_parents_without_permission() {
        let directory = tempfile::tempdir().unwrap();
        let error = create_repo_file_atomically(directory.path(), "new/deep/file.rs", "x\n", false)
            .unwrap_err();
        assert!(error.to_string().contains("create_parent_missing"));
        assert!(!directory.path().join("new").exists());
    }

    #[test]
    fn create_file_rejects_empty_nul_and_traversal_content_or_paths() {
        let directory = tempfile::tempdir().unwrap();
        assert!(create_repo_file_atomically(directory.path(), "empty", "", false).is_err());
        assert!(create_repo_file_atomically(directory.path(), "nul", "a\0b", false).is_err());
        assert!(create_repo_file_atomically(directory.path(), "../escape", "x", true).is_err());
    }

    #[test]
    fn move_file_preserves_exact_content_and_removes_source() {
        let directory = repository("source\n");
        let hash = sha256_text("source\n");
        let output = move_repo_file_atomically(
            directory.path(),
            TARGET,
            "src/moved/provider.tsx",
            Some(&hash),
            true,
        )
        .unwrap();
        assert!(!directory.path().join(TARGET).exists());
        assert_eq!(
            fs::read_to_string(directory.path().join("src/moved/provider.tsx")).unwrap(),
            "source\n"
        );
        assert!(output.contains("moved"));
    }

    #[test]
    fn move_file_rejects_existing_destination_without_mutation() {
        let directory = repository("source\n");
        let destination = directory.path().join("src/destination.tsx");
        fs::write(&destination, "destination\n").unwrap();
        assert!(
            move_repo_file_atomically(directory.path(), TARGET, "src/destination.tsx", None, false)
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(directory.path().join(TARGET)).unwrap(),
            "source\n"
        );
        assert_eq!(fs::read_to_string(destination).unwrap(), "destination\n");
    }

    #[test]
    fn move_file_rejects_a_stale_source_hash() {
        let directory = repository("source\n");
        let error = move_repo_file_atomically(
            directory.path(),
            TARGET,
            "src/destination.tsx",
            Some("stale"),
            false,
        )
        .unwrap_err();
        let typed = error.downcast_ref::<MutationApplicationError>().unwrap();
        assert_eq!(
            typed.failure,
            MutationApplicationFailure::RepositoryChangedSinceContext
        );
        assert!(directory.path().join(TARGET).exists());
    }

    #[test]
    fn delete_file_verifies_hash_and_removes_only_the_target() {
        let directory = repository("delete me\n");
        fs::write(directory.path().join("keep.txt"), "keep\n").unwrap();
        let hash = sha256_text("delete me\n");
        let output = delete_repo_file(directory.path(), TARGET, Some(&hash)).unwrap();
        assert!(!directory.path().join(TARGET).exists());
        assert_eq!(
            fs::read_to_string(directory.path().join("keep.txt")).unwrap(),
            "keep\n"
        );
        let output: Value = serde_json::from_str(&output).unwrap();
        assert!(output["after_sha256"].is_null());
    }

    #[test]
    fn delete_file_rejects_a_stale_hash_without_deleting() {
        let directory = repository("current\n");
        let error = delete_repo_file(directory.path(), TARGET, Some("stale")).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<MutationApplicationError>()
                .unwrap()
                .failure,
            MutationApplicationFailure::RepositoryChangedSinceContext
        );
        assert!(directory.path().join(TARGET).exists());
    }

    fn patch(old_path: &str, new_path: &str, hunk: &str) -> String {
        format!("diff --git {old_path} {new_path}\n--- {old_path}\n+++ {new_path}\n{hunk}")
    }

    fn application_failure(error: &anyhow::Error) -> MutationApplicationFailure {
        error
            .downcast_ref::<MutationApplicationError>()
            .expect("typed mutation failure")
            .failure
    }

    #[test]
    fn standard_git_paths_normalize_to_declared_target() {
        let repository = repository("old\n");
        let validated = validate_patch_target(
            repository.path(),
            TARGET,
            &patch(
                &format!("a/{TARGET}"),
                &format!("b/{TARGET}"),
                "@@ -1 +1 @@\n-old\n+new\n",
            ),
        )
        .expect("standard Git patch");
        assert!(
            validated
                .diagnostics
                .normalized_paths
                .iter()
                .all(|path| path == TARGET)
        );
    }

    #[test]
    fn quoted_leading_dot_paths_normalize_to_declared_target() {
        let repository = repository("old\n");
        let raw = format!("\"./{TARGET}\"");
        assert_eq!(
            normalize_patch_repository_path(repository.path(), &raw).unwrap(),
            TARGET
        );
    }

    #[test]
    fn absolute_and_parent_traversal_paths_are_rejected() {
        let repository = repository("old\n");
        for path in ["/etc/passwd", "../outside", "a/../../outside"] {
            assert!(normalize_patch_repository_path(repository.path(), path).is_err());
        }
    }

    #[test]
    fn multiple_file_sections_are_rejected_with_diagnostics() {
        let repository = repository("old\n");
        let mut supplied = patch(
            &format!("a/{TARGET}"),
            &format!("b/{TARGET}"),
            "@@ -1 +1 @@\n-old\n+new\n",
        );
        supplied.push_str(
            "diff --git a/src/other.ts b/src/other.ts\n--- a/src/other.ts\n+++ b/src/other.ts\n@@ -1 +1 @@\n-a\n+b\n",
        );
        let error = validate_patch_target(repository.path(), TARGET, &supplied).unwrap_err();
        let application = error.downcast_ref::<MutationApplicationError>().unwrap();
        assert_eq!(
            application.failure,
            MutationApplicationFailure::InvalidPatchTarget
        );
        assert_eq!(
            application
                .patch_validation
                .as_ref()
                .unwrap()
                .file_section_count,
            2
        );
    }

    #[test]
    fn different_file_is_rejected() {
        let repository = repository("old\n");
        let supplied = patch(
            "a/src/other.ts",
            "b/src/other.ts",
            "@@ -1 +1 @@\n-old\n+new\n",
        );
        let error = validate_patch_target(repository.path(), TARGET, &supplied).unwrap_err();
        assert_eq!(
            application_failure(&error),
            MutationApplicationFailure::PatchWouldModifyUnexpectedPath
        );
    }

    #[test]
    fn current_content_patch_applies() {
        let repository = repository("old\nsecond\n");
        let supplied = patch(
            &format!("a/{TARGET}"),
            &format!("b/{TARGET}"),
            "@@ -1,2 +1,2 @@\n-old\n+new\n second\n",
        );
        apply_repo_unified_diff_with_context(
            repository.path(),
            TARGET,
            &supplied,
            Some(&sha256_text("old\nsecond\n")),
        )
        .expect("patch applies");
        assert_eq!(
            fs::read_to_string(repository.path().join(TARGET)).unwrap(),
            "new\nsecond\n"
        );
    }

    #[test]
    fn inaccurate_hunk_line_number_is_normalized_when_context_is_unique() {
        let repository = repository("first\nold\nlast\n");
        let supplied = patch(
            &format!("a/{TARGET}"),
            &format!("b/{TARGET}"),
            "@@ -99,2 +99,2 @@\n old\n-last\n+final\n",
        );
        apply_repo_unified_diff_with_context(repository.path(), TARGET, &supplied, None)
            .expect("unique context permits offset normalization");
        assert_eq!(
            fs::read_to_string(repository.path().join(TARGET)).unwrap(),
            "first\nold\nfinal\n"
        );
    }

    #[test]
    fn ambiguous_patch_context_is_rejected() {
        let repository = repository("same\nold\nsame\nold\n");
        let supplied = patch(
            &format!("a/{TARGET}"),
            &format!("b/{TARGET}"),
            "@@ -99,2 +99,2 @@\n same\n-old\n+new\n",
        );
        let error =
            apply_repo_unified_diff_with_context(repository.path(), TARGET, &supplied, None)
                .unwrap_err();
        assert_eq!(
            application_failure(&error),
            MutationApplicationFailure::PatchContextMismatch
        );
    }

    #[test]
    fn replacement_changes_only_target_and_rejects_invalid_or_noop_content() {
        let repository = repository("old\r\n");
        let other = repository.path().join("src/other.ts");
        fs::write(&other, "untouched\n").unwrap();
        replace_repo_file_atomically(
            repository.path(),
            TARGET,
            "new\n",
            Some(&sha256_text("old\r\n")),
        )
        .expect("valid replacement");
        assert_eq!(
            fs::read_to_string(repository.path().join(TARGET)).unwrap(),
            "new\r\n"
        );
        assert_eq!(fs::read_to_string(other).unwrap(), "untouched\n");

        for replacement in ["", "bad\0content"] {
            let error = replace_repo_file_atomically(
                repository.path(),
                TARGET,
                replacement,
                Some(&sha256_text("new\r\n")),
            )
            .unwrap_err();
            assert_eq!(
                application_failure(&error),
                MutationApplicationFailure::ReplacementContentInvalid
            );
        }
        let error = replace_repo_file_atomically(
            repository.path(),
            TARGET,
            "new\n",
            Some(&sha256_text("new\r\n")),
        )
        .unwrap_err();
        assert_eq!(
            application_failure(&error),
            MutationApplicationFailure::MutationProducedNoChange
        );
    }

    #[test]
    fn structured_edit_requires_exact_persisted_anchor_count() {
        let repository = repository("one\ntwo\ntwo\n");
        let edit = StructuredEdit::ReplaceExactText {
            path: TARGET.into(),
            expected: "two".into(),
            replacement: "changed".into(),
            expected_occurrences: 1,
        };
        let error = apply_structured_edit(repository.path(), &edit, None).unwrap_err();
        assert_eq!(
            application_failure(&error),
            MutationApplicationFailure::PatchContextMismatch
        );
        assert_eq!(
            fs::read_to_string(repository.path().join(TARGET)).unwrap(),
            "one\ntwo\ntwo\n"
        );
    }
}
