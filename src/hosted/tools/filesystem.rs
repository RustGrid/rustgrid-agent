// Extracted from the hosted execution composition root.
use super::*;

#[cfg(test)]
pub(in crate::hosted) fn validate_model_command(value: &str) -> Result<()> {
    if value.contains('\n') || value.contains('\r') {
        bail!("focused model command must be one direct command without shell syntax");
    }
    let parts = command::parse(value)?;
    if parts.iter().any(|part| {
        matches!(part.as_str(), "&&" | "||" | "|" | ";" | "<" | ">")
            || part.starts_with("<<")
            || part.starts_with(">>")
    }) {
        bail!(
            "focused model command runs without a shell; operators, redirects, heredocs, and command chaining are unsupported"
        );
    }
    let program = Path::new(&parts[0])
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        program.as_str(),
        "gh" | "curl" | "wget" | "ssh" | "scp" | "nc" | "netcat"
    ) {
        bail!("focused model command cannot access external credential or network tools");
    }
    if program == "git"
        && parts.get(1).is_some_and(|part| {
            matches!(
                part.as_str(),
                "add"
                    | "branch"
                    | "checkout"
                    | "clean"
                    | "commit"
                    | "config"
                    | "fetch"
                    | "merge"
                    | "pull"
                    | "push"
                    | "rebase"
                    | "remote"
                    | "reset"
                    | "restore"
                    | "switch"
                    | "tag"
            )
        })
    {
        bail!("focused model command cannot mutate or publish Git state");
    }
    Ok(())
}

pub(in crate::hosted) fn safe_repo_path(
    root: &Path,
    value: &str,
    allow_missing: bool,
) -> Result<PathBuf> {
    let relative = Path::new(value);
    if relative.is_absolute() {
        bail!("repository tool path must be relative");
    }
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) if name != ".git" => normalized.push(name),
            Component::Normal(_) => bail!("repository tools cannot access .git"),
            _ => bail!("repository tool path cannot escape the checkout"),
        }
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    let root = root
        .canonicalize()
        .context("could not canonicalize repository root")?;
    let candidate = root.join(&normalized);
    let mut cursor = root.clone();
    for component in normalized.components() {
        cursor.push(component.as_os_str());
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("repository tools cannot traverse symbolic links")
            }
            Ok(_) => {}
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                break;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not inspect repository path {value}"));
            }
        }
    }
    if candidate.exists() {
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("could not canonicalize repository path {value}"))?;
        if !canonical.starts_with(&root) {
            bail!("repository tool path escaped the checkout");
        }
    }
    Ok(candidate)
}

pub(in crate::hosted) fn collect_repo_files(
    root: &Path,
    start: &Path,
    maximum: usize,
) -> Result<Vec<String>> {
    let root = root.canonicalize()?;
    let mut pending = VecDeque::from([start.to_path_buf()]);
    let mut files = Vec::new();
    while let Some(directory) = pending.pop_front() {
        let mut entries = fs::read_dir(&directory)
            .with_context(|| format!("could not list {}", directory.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if metadata.is_dir() {
                if matches!(
                    name.as_ref(),
                    ".git"
                        | "node_modules"
                        | "target"
                        | "dist"
                        | "build"
                        | "coverage"
                        | ".next"
                        | ".turbo"
                        | "vendor"
                ) {
                    continue;
                }
                pending.push_back(path);
            } else if metadata.is_file() {
                files.push(
                    path.strip_prefix(&root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                );
                if files.len() >= maximum {
                    files.push(format!("[file list truncated at {maximum} entries]"));
                    return Ok(files);
                }
            }
        }
    }
    Ok(files)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::hosted) enum FileReadStatus {
    Success,
    Error,
}

pub(in crate::hosted) fn read_error_progress_class(error: &str) -> ToolProgressClass {
    if error.contains("repository_access_failed") {
        ToolProgressClass::BlockingFailure
    } else if error.contains("duplicate") {
        ToolProgressClass::Duplicate
    } else {
        ToolProgressClass::RecoverableFailure
    }
}

pub(in crate::hosted) fn successful_read_progress(
    tool: &str,
    new_file: bool,
    new_range: bool,
    new_related_test: bool,
    partial_failure: bool,
) -> (ToolProgressClass, &'static str) {
    if new_file || new_range {
        (
            ToolProgressClass::Productive,
            "new planned repository content inspected",
        )
    } else if tool == "related_tests" && new_related_test {
        (
            ToolProgressClass::Productive,
            "new related test file identified",
        )
    } else if partial_failure {
        (
            ToolProgressClass::RecoverableFailure,
            "batch read returned recoverable per-file errors",
        )
    } else {
        (
            ToolProgressClass::Duplicate,
            "repository content was already inspected",
        )
    }
}

#[derive(Clone, Debug, Serialize)]
pub(in crate::hosted) struct FileReadResult {
    pub(in crate::hosted) path: String,
    pub(in crate::hosted) status: FileReadStatus,
    pub(in crate::hosted) content: Option<String>,
    pub(in crate::hosted) error_code: Option<String>,
    pub(in crate::hosted) error_message: Option<String>,
    pub(in crate::hosted) line_count: Option<u32>,
    pub(in crate::hosted) file_size: Option<u64>,
    pub(in crate::hosted) valid_line_range: Option<String>,
    pub(in crate::hosted) truncated: bool,
    pub(in crate::hosted) fallback_attempted: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(in crate::hosted) struct BatchReadResult {
    pub(in crate::hosted) files: Vec<FileReadResult>,
}

#[derive(Clone, Debug)]
pub(in crate::hosted) struct PrevalidatedRepoFile {
    pub(in crate::hosted) requested_path: String,
    pub(in crate::hosted) resolved_path: PathBuf,
    pub(in crate::hosted) file_size: u64,
}

#[derive(Clone, Debug)]
pub(in crate::hosted) enum PrevalidatedBatchReadPath {
    Ready(PrevalidatedRepoFile),
    Rejected {
        result: FileReadResult,
        fallback_path: Option<String>,
    },
}

pub(in crate::hosted) fn failed_file_read(
    path: &str,
    code: &str,
    message: impl Into<String>,
    file_size: Option<u64>,
    line_count: Option<u32>,
) -> FileReadResult {
    FileReadResult {
        path: path.to_owned(),
        status: FileReadStatus::Error,
        content: None,
        error_code: Some(code.to_owned()),
        error_message: Some(message.into()),
        line_count,
        file_size,
        valid_line_range: line_count.map(|count| format!("1-{}", count.max(1))),
        truncated: false,
        fallback_attempted: false,
    }
}

pub(in crate::hosted) fn prevalidate_repo_file(
    root: &Path,
    value: &str,
) -> std::result::Result<PrevalidatedRepoFile, Box<FileReadResult>> {
    let path = match safe_repo_path(root, value, false) {
        Ok(path) => path,
        Err(error) => {
            let message = error.to_string();
            let code = if message.contains("could not inspect repository path") {
                "path_not_found"
            } else if message.contains("symbolic")
                || message.contains("escape")
                || message.contains("absolute")
                || message.contains("cannot access .git")
                || message.contains("must be relative")
            {
                "path_not_allowed"
            } else {
                "repository_access_failed"
            };
            return Err(Box::new(failed_file_read(value, code, message, None, None)));
        }
    };
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Box::new(failed_file_read(
                value,
                "path_not_found",
                format!("repository path `{value}` does not exist"),
                None,
                None,
            )));
        }
        Err(error) => {
            return Err(Box::new(failed_file_read(
                value,
                "repository_access_failed",
                format!("could not inspect repository file `{value}`: {error}"),
                None,
                None,
            )));
        }
    };
    if !metadata.is_file() {
        return Err(Box::new(failed_file_read(
            value,
            "not_regular_file",
            format!("repository path `{value}` is not a regular file"),
            Some(metadata.len()),
            None,
        )));
    }
    if metadata.len() > MAX_MODEL_FILE_BYTES as u64 {
        return Err(Box::new(failed_file_read(
            value,
            "file_too_large",
            format!(
                "repository file `{value}` is {} bytes; the read limit is {MAX_MODEL_FILE_BYTES} bytes",
                metadata.len()
            ),
            Some(metadata.len()),
            None,
        )));
    }
    Ok(PrevalidatedRepoFile {
        requested_path: value.to_owned(),
        resolved_path: path,
        file_size: metadata.len(),
    })
}

pub(in crate::hosted) fn read_prevalidated_repo_file_result(
    file: &PrevalidatedRepoFile,
    start_line: u64,
    end_line: u64,
    maximum_output_bytes: usize,
) -> FileReadResult {
    let value = file.requested_path.as_str();
    let bytes = match fs::read(&file.resolved_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return failed_file_read(
                value,
                "repository_access_failed",
                format!("could not read repository file `{value}`: {error}"),
                Some(file.file_size),
                None,
            );
        }
    };
    if bytes.contains(&0) {
        return failed_file_read(
            value,
            "binary_file",
            format!("repository file `{value}` is binary and cannot be exposed"),
            Some(file.file_size),
            None,
        );
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return failed_file_read(
                value,
                "not_utf8",
                format!("repository file `{value}` is not valid UTF-8"),
                Some(file.file_size),
                None,
            );
        }
    };
    let lines = text.lines().collect::<Vec<_>>();
    let line_count = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    if start_line == 0 || end_line < start_line || start_line > u64::from(line_count.max(1)) {
        return failed_file_read(
            value,
            "line_range_invalid",
            format!(
                "requested lines {start_line}-{end_line} are outside `{value}`; valid line range is 1-{}",
                line_count.max(1)
            ),
            Some(file.file_size),
            Some(line_count),
        );
    }
    let mut output = String::new();
    let mut truncated = false;
    for (index, line) in lines.iter().enumerate() {
        let line_number = index as u64 + 1;
        if line_number < start_line {
            continue;
        }
        if line_number > end_line {
            break;
        }
        let formatted = format!("{line_number:>6} | {line}\n");
        if output.len().saturating_add(formatted.len()) > maximum_output_bytes {
            output.push_str("[read output truncated]\n");
            truncated = true;
            break;
        }
        output.push_str(&formatted);
    }
    FileReadResult {
        path: value.to_owned(),
        status: FileReadStatus::Success,
        content: Some(output),
        error_code: None,
        error_message: None,
        line_count: Some(line_count),
        file_size: Some(file.file_size),
        valid_line_range: Some(format!("1-{}", line_count.max(1))),
        truncated,
        fallback_attempted: false,
    }
}

pub(in crate::hosted) fn read_repo_file_result(
    root: &Path,
    value: &str,
    start_line: u64,
    end_line: u64,
    maximum_output_bytes: usize,
) -> FileReadResult {
    match prevalidate_repo_file(root, value) {
        Ok(file) => {
            read_prevalidated_repo_file_result(&file, start_line, end_line, maximum_output_bytes)
        }
        Err(result) => *result,
    }
}

pub(in crate::hosted) fn prevalidate_batch_read_paths(
    root: &Path,
    paths: &[Value],
) -> Vec<PrevalidatedBatchReadPath> {
    paths
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let Some(path) = value
                .as_str()
                .filter(|path| !path.is_empty() && path.len() <= 4_096)
            else {
                return PrevalidatedBatchReadPath::Rejected {
                    result: failed_file_read(
                        &format!("<paths[{index}]>"),
                        "path_malformed",
                        "read_files path must be a non-empty repository-relative string",
                        None,
                        None,
                    ),
                    fallback_path: None,
                };
            };
            match prevalidate_repo_file(root, path) {
                Ok(file) => PrevalidatedBatchReadPath::Ready(file),
                Err(result) => PrevalidatedBatchReadPath::Rejected {
                    result: *result,
                    fallback_path: Some(path.to_owned()),
                },
            }
        })
        .collect()
}

pub(in crate::hosted) fn read_prevalidated_repo_files_with_fallback(
    root: &Path,
    paths: &[PrevalidatedBatchReadPath],
    maximum_lines: u64,
    maximum_output_bytes: usize,
) -> (BatchReadResult, u32) {
    let per_file_bytes = (maximum_output_bytes / paths.len().max(1)).max(1_024);
    let mut fallback_paths = Vec::with_capacity(paths.len());
    let mut files = paths
        .iter()
        .map(|path| match path {
            PrevalidatedBatchReadPath::Ready(file) => {
                fallback_paths.push(Some(file.requested_path.clone()));
                read_prevalidated_repo_file_result(file, 1, maximum_lines, per_file_bytes)
            }
            PrevalidatedBatchReadPath::Rejected {
                result,
                fallback_path,
            } => {
                fallback_paths.push(fallback_path.clone());
                result.clone()
            }
        })
        .collect::<Vec<_>>();
    let initial_failures = u32::try_from(
        files
            .iter()
            .filter(|result| result.status == FileReadStatus::Error)
            .count(),
    )
    .unwrap_or(u32::MAX);
    for (result, fallback_path) in files.iter_mut().zip(fallback_paths) {
        if result.status != FileReadStatus::Error {
            continue;
        }
        let Some(fallback_path) = fallback_path else {
            continue;
        };
        let mut fallback =
            read_repo_file_result(root, &fallback_path, 1, maximum_lines, per_file_bytes);
        fallback.fallback_attempted = true;
        if fallback.status == FileReadStatus::Error
            && let Some(message) = fallback.error_message.as_mut()
        {
            message.push_str("; individual read fallback also failed");
        }
        *result = fallback;
    }
    (BatchReadResult { files }, initial_failures)
}

pub(in crate::hosted) fn read_prevalidated_repo_files_with_evidence_cache(
    root: &Path,
    paths: &[PrevalidatedBatchReadPath],
    maximum_lines: u64,
    maximum_output_bytes: usize,
    evidence: &crate::execution_graph::EvidenceStore,
    repository_fingerprint: &str,
) -> (BatchReadResult, u32) {
    let per_file_bytes = (maximum_output_bytes / paths.len().max(1)).max(1_024);
    let required_range =
        crate::execution_graph::LineRange::new(1, u32::try_from(maximum_lines).unwrap_or(u32::MAX));
    let mut resolved = vec![None; paths.len()];
    let mut uncached = Vec::new();
    let mut uncached_indexes = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let PrevalidatedBatchReadPath::Ready(file) = path else {
            uncached.push(path.clone());
            uncached_indexes.push(index);
            continue;
        };
        let Some(cached) =
            evidence.reusable_file(&file.requested_path, repository_fingerprint, required_range)
        else {
            uncached.push(path.clone());
            uncached_indexes.push(index);
            continue;
        };
        let content = truncate_text(&cached.captured_content, per_file_bytes);
        resolved[index] = Some(FileReadResult {
            path: file.requested_path.clone(),
            status: FileReadStatus::Success,
            content: Some(content.clone()),
            error_code: None,
            error_message: None,
            line_count: Some(
                cached
                    .line_range
                    .map_or_else(
                        || cached.captured_content.lines().count(),
                        |range| usize::try_from(range.line_count()).unwrap_or(usize::MAX),
                    )
                    .try_into()
                    .unwrap_or(u32::MAX),
            ),
            file_size: Some(u64::try_from(cached.captured_content.len()).unwrap_or(u64::MAX)),
            valid_line_range: cached
                .line_range
                .map(|range| format!("{}-{}", range.start, range.end)),
            truncated: cached.truncated || content.len() < cached.captured_content.len(),
            fallback_attempted: false,
        });
    }
    let (uncached_batch, failures) = if uncached.is_empty() {
        (BatchReadResult { files: Vec::new() }, 0)
    } else {
        read_prevalidated_repo_files_with_fallback(
            root,
            &uncached,
            maximum_lines,
            maximum_output_bytes,
        )
    };
    for (index, result) in uncached_indexes.into_iter().zip(uncached_batch.files) {
        resolved[index] = Some(result);
    }
    (
        BatchReadResult {
            files: resolved
                .into_iter()
                .map(|result| result.expect("every prevalidated batch path must resolve"))
                .collect(),
        },
        failures,
    )
}
