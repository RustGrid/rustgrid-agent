//! Secret redaction applied before any traced text leaves the machine.
//!
//! Mirrors `sanitize_event_text` in RustGrid
//! (`crates/ticket-service/src/security/event_data.rs`), which redacts again on
//! arrival. Both sides run the cases in `redaction_cases.json`; keep the two
//! copies identical.

use std::sync::LazyLock;

use regex::Regex;

pub const REDACTED: &str = "[REDACTED]";

static PREFIXED_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:rge_|rgk_|sk-|gh[pousr]_|github_pat_|do[por]_v1_|[sr]k_(?:live|test)_|whsec_|xox[abpr]-)[a-z0-9._~+/-]{12,}={0,2}",
    )
    .expect("prefixed secret regex must compile")
});
static BEARER_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bbearer[ \t]+[a-z0-9._~+/-]{16,}={0,2}").expect("bearer regex must compile")
});
static JWT_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\beyJ[a-zA-Z0-9_-]{8,}\.[a-zA-Z0-9_-]{8,}\.[a-zA-Z0-9_-]{8,}\b")
        .expect("JWT regex must compile")
});
static AWS_ACCESS_KEY_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").expect("AWS key id regex must compile")
});
static AWS_SECRET_ACCESS_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(aws_secret_access_key)(\s*[=:]\s*)["']?[a-z0-9/+=]{16,}["']?"#)
        .expect("AWS secret regex must compile")
});
static PRIVATE_KEY_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
    )
    .expect("private key block regex must compile")
});
static URL_PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://[^\s:/@]+):[^\s@/]+@")
        .expect("URL password regex must compile")
});
static SECRET_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"\b((?:[A-Z][A-Z0-9_]*)?(?:KEY|TOKEN|SECRET|PASSWORD|PASSWD|_PASS|DSN|DATABASE_URL|CREDENTIAL|CREDENTIALS))(\s*[=:]\s*)(?:"[^"\n]*"|'[^'\n]*'|[^\s"',;]+)"#,
    )
    .expect("secret assignment regex must compile")
});
static QUOTED_SECRET_PAIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)("[a-z0-9_-]*(?:password|passwd|secret|token|api[_-]?key|private[_-]?key|credential)[a-z0-9_-]*"\s*:\s*)"[^"\n]*""#,
    )
    .expect("quoted secret pair regex must compile")
});

/// Replaces credential-shaped values in free text with `[REDACTED]`.
pub fn redact(value: &str) -> String {
    let value = PRIVATE_KEY_BLOCK.replace_all(value, REDACTED);
    let value = BEARER_SECRET.replace_all(&value, REDACTED);
    let value = PREFIXED_SECRET.replace_all(&value, REDACTED);
    let value = AWS_ACCESS_KEY_ID.replace_all(&value, REDACTED);
    let value = AWS_SECRET_ACCESS_KEY.replace_all(&value, format!("${{1}}${{2}}{REDACTED}"));
    let value = JWT_SECRET.replace_all(&value, REDACTED);
    let value = SECRET_ASSIGNMENT.replace_all(&value, format!("${{1}}${{2}}{REDACTED}"));
    let value = URL_PASSWORD.replace_all(&value, format!("${{1}}:{REDACTED}@"));
    QUOTED_SECRET_PAIR
        .replace_all(&value, format!("${{1}}\"{REDACTED}\""))
        .into_owned()
}

/// Redacts, then keeps at most `max` characters, marking the cut.
pub fn redact_truncated(value: &str, max: usize) -> String {
    let redacted = redact(value);
    if redacted.chars().count() <= max {
        return redacted;
    }
    let mut cut: String = redacted.chars().take(max).collect();
    cut.push_str(" …[truncated]");
    cut
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn expand(template: &str) -> String {
        let pattern = Regex::new(r"<<(.)\*(\d+)>>").unwrap();
        pattern
            .replace_all(template, |captures: &regex::Captures<'_>| {
                captures[1].repeat(captures[2].parse().unwrap())
            })
            .into_owned()
    }

    #[test]
    fn shared_redaction_cases_hold() {
        let cases: Value = serde_json::from_str(include_str!("redaction_cases.json")).unwrap();
        for case in cases["cases"].as_array().unwrap() {
            let input = expand(case["input"].as_str().unwrap());
            let expected = expand(case["expected"].as_str().unwrap());
            assert_eq!(redact(&input), expected, "input: {input}");
        }
    }

    #[test]
    fn truncation_happens_after_redaction() {
        let secret = format!("ghp_{}", "a".repeat(36));
        assert_eq!(
            redact_truncated(&format!("{secret} tail"), 15),
            "[REDACTED] tail"
        );
        assert_eq!(redact_truncated("abcdef", 3), "abc …[truncated]");
    }
}
