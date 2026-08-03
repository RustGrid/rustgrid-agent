// Extracted from the hosted execution composition root.
use super::*;

pub(in crate::hosted) struct SearchResult {
    pub(in crate::hosted) output: String,
    pub(in crate::hosted) truncated: bool,
    pub(in crate::hosted) matched_paths: Vec<String>,
}

pub(in crate::hosted) fn search_repo(
    root: &Path,
    value: &str,
    query: &str,
    extensions: &[String],
    context_lines: u64,
    maximum_new_consumers: Option<usize>,
    known_consumers: &BTreeSet<String>,
) -> Result<SearchResult> {
    if query.is_empty() || query.contains('\0') {
        bail!("search query is invalid");
    }
    let start = safe_repo_path(root, value, false)?;
    let candidates = if start.is_file() {
        vec![
            start
                .strip_prefix(root)
                .unwrap_or(&start)
                .to_string_lossy()
                .into_owned(),
        ]
    } else {
        collect_repo_files(root, &start, 2_000)?
    };
    let normalized_extensions = extensions
        .iter()
        .map(|extension| extension.trim_start_matches('.'))
        .collect::<BTreeSet<_>>();
    let mut groups = Vec::<(String, Vec<(usize, String)>, usize)>::new();
    let mut new_consumer_count = 0_usize;
    let mut truncated = false;
    for candidate in candidates {
        if candidate.starts_with('[') {
            truncated = true;
            continue;
        }
        if !normalized_extensions.is_empty()
            && Path::new(&candidate)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_none_or(|extension| !normalized_extensions.contains(extension))
        {
            continue;
        }
        let path = safe_repo_path(root, &candidate, false)?;
        let metadata = fs::metadata(&path)?;
        if metadata.len() > MAX_MODEL_FILE_BYTES as u64 {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let lines = text.lines().collect::<Vec<_>>();
        let mut file_matches = Vec::new();
        let mut file_count = 0usize;
        for (index, line) in lines.iter().enumerate() {
            if line.contains(query) {
                file_count = file_count.saturating_add(1);
                if file_matches.len() < 3 {
                    let start = index.saturating_sub(context_lines as usize);
                    let end = (index + context_lines as usize + 1).min(lines.len());
                    let excerpt = lines[start..end]
                        .iter()
                        .enumerate()
                        .map(|(offset, excerpt)| {
                            format!(
                                "{:>6} | {}",
                                start + offset + 1,
                                truncate_text(excerpt, 1_000)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    file_matches.push((index + 1, excerpt));
                }
            }
        }
        if file_count > 0 {
            if !localized_discovery_core_path(&candidate) && !known_consumers.contains(&candidate) {
                if maximum_new_consumers.is_some_and(|maximum| new_consumer_count >= maximum) {
                    truncated = true;
                    break;
                }
                new_consumer_count = new_consumer_count.saturating_add(1);
            }
            groups.push((candidate, file_matches, file_count));
        }
        if groups.len() >= 40 {
            truncated = true;
            break;
        }
    }
    let total_matches = groups.iter().map(|group| group.2).sum::<usize>();
    let matched_paths = groups
        .iter()
        .map(|group| group.0.clone())
        .collect::<Vec<_>>();
    let mut output = format!(
        "search_summary: {total_matches} match(es) across {} file(s)\n",
        groups.len()
    );
    for (candidate, excerpts, file_count) in groups {
        output.push_str(&format!("\n{candidate} ({file_count} matches)\n"));
        for (line, excerpt) in excerpts {
            output.push_str(&format!(
                "  representative match at line {line}\n{excerpt}\n"
            ));
            if output.len() >= MAX_TOOL_OUTPUT_BYTES {
                truncated = true;
                break;
            }
        }
        if output.len() >= MAX_TOOL_OUTPUT_BYTES {
            break;
        }
    }
    if output.is_empty() {
        output.push_str("no matches\n");
    }
    if truncated {
        output.push_str("[search output truncated]\n");
    }
    Ok(SearchResult {
        output: truncate_text(&output, MAX_TOOL_OUTPUT_BYTES),
        truncated,
        matched_paths,
    })
}

pub(in crate::hosted) fn truncate_text(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let suffix = "\n[truncated]";
    let mut end = maximum.saturating_sub(suffix.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &value[..end], suffix)
}
