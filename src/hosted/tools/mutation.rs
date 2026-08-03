// Extracted from the hosted execution composition root.
use super::*;

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

pub(in crate::hosted) fn apply_repo_unified_diff(
    root: &Path,
    path: &str,
    patch: &str,
) -> Result<String> {
    let target = safe_repo_path(root, path, false)?;
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    let expected_diff = format!("diff --git a/{path} b/{path}");
    let expected_old = format!("--- a/{path}");
    let expected_new = format!("+++ b/{path}");
    let diff_headers = patch
        .lines()
        .filter(|line| line.starts_with("diff --git "))
        .collect::<Vec<_>>();
    let old_headers = patch
        .lines()
        .filter(|line| line.starts_with("--- "))
        .collect::<Vec<_>>();
    let new_headers = patch
        .lines()
        .filter(|line| line.starts_with("+++ "))
        .collect::<Vec<_>>();
    let unsafe_metadata = patch.lines().any(|line| {
        [
            "rename from ",
            "rename to ",
            "copy from ",
            "copy to ",
            "new file mode ",
            "deleted file mode ",
            "old mode ",
            "new mode ",
            "GIT binary patch",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
    });
    if (!diff_headers.is_empty() && diff_headers != [expected_diff.as_str()])
        || old_headers != [expected_old.as_str()]
        || new_headers != [expected_new.as_str()]
        || unsafe_metadata
    {
        bail!("apply_unified_diff must modify exactly the declared existing path");
    }
    if patch.len() > MAX_MODEL_FILE_BYTES {
        bail!("unified diff exceeds the hosted tool limit");
    }
    let patch_path = env::temp_dir().join(format!(
        "rustgrid-agent-unified-diff-{}.patch",
        Uuid::new_v4().simple()
    ));
    fs::write(&patch_path, patch.as_bytes()).context("could not write patch file")?;
    let patch_path_text = patch_path.to_string_lossy().into_owned();
    let checked = command::checked(
        "git",
        [
            "apply",
            "--check",
            "--whitespace=nowarn",
            patch_path_text.as_str(),
        ],
        root,
    )
    .context("unified diff validation failed")
    .and_then(|_| {
        command::checked(
            "git",
            ["apply", "--whitespace=nowarn", patch_path_text.as_str()],
            root,
        )
        .context("unified diff application failed")
    });
    let _ = fs::remove_file(&patch_path);
    checked?;
    let updated = fs::read_to_string(&target)
        .with_context(|| format!("could not read patched UTF-8 repository file {path}"))?;
    mutation_output(
        path,
        Some(sha256_text(&content)),
        Some(sha256_text(&updated)),
        "unified_diff".into(),
        format!("applied {}-byte unified diff", patch.len()),
    )
}

pub(in crate::hosted) fn delete_repo_file(root: &Path, path: &str) -> Result<String> {
    let target = safe_repo_path(root, path, false)?;
    if !target.is_file() {
        bail!("delete_file target is not a regular file");
    }
    let content = fs::read_to_string(&target)
        .with_context(|| format!("could not read UTF-8 repository file {path}"))?;
    fs::remove_file(&target).with_context(|| format!("could not delete repository file {path}"))?;
    mutation_output(
        path,
        Some(sha256_text(&content)),
        None,
        "complete_file".into(),
        format!("deleted {}-byte file", content.len()),
    )
}
