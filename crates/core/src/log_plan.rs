// SPDX-License-Identifier: Apache-2.0
//! Pure log/reflog helpers: parse, summarize, short ids.
//!
//! FS traversal and render stay in CLI.

/// Parsed reflog line (git-style: `old new actor... timestamp\tmessage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogLine {
    pub source: String,
    pub reference: String,
    pub old_oid: String,
    pub new_oid: String,
    pub actor: String,
    pub timestamp: Option<String>,
    pub message: String,
}

/// Parse one reflog line. Returns `None` when old/new oids are missing.
pub fn parse_reflog_line(source: &str, reference: &str, line: &str) -> Option<ReflogLine> {
    let (metadata, message) = line.split_once('\t').unwrap_or((line, ""));
    let mut parts = metadata.split_whitespace();
    let old_oid = parts.next()?.to_string();
    let new_oid = parts.next()?.to_string();
    let mut actor_parts = Vec::new();
    let mut timestamp = None;

    for part in parts {
        if part.parse::<i64>().is_ok() {
            timestamp = Some(part.to_string());
            break;
        }
        actor_parts.push(part);
    }

    Some(ReflogLine {
        source: source.to_string(),
        reference: reference.to_string(),
        old_oid,
        new_oid,
        actor: actor_parts.join(" "),
        timestamp,
        message: message.to_string(),
    })
}

/// First 12 hex chars of an object id (or the whole string if shorter).
pub fn short_oid(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

/// Compact path list for timeline one-liners.
pub fn summarize_paths(paths: &[String]) -> String {
    match paths {
        [] => String::new(),
        [one] => one.clone(),
        [one, two] => format!("{one}, {two}"),
        [one, two, rest @ ..] => format!("{one}, {two} +{}", rest.len()),
    }
}

/// Stable string labels for timeline enums (CLI render + JSON fields).
pub fn timeline_label_str(label: &str) -> &'static str {
    match label {
        "RepoReversible" | "repo-reversible" => "repo-reversible",
        "ExternalSideEffectsUnknown" | "external-side-effects-unknown" => {
            "external-side-effects-unknown"
        }
        "IgnoredPathTouched" | "ignored-path-touched" => "ignored-path-touched",
        "OutsideRepoTouched" | "outside-repo-touched" => "outside-repo-touched",
        "PurgeBoundary" | "purge-boundary" => "purge-boundary",
        "CaptureFailed" | "capture-failed" => "capture-failed",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reflog_line_with_tab_message() {
        let line = "abc123 def456 Alice <a@b.com> 1700000000 +0000\tcommit: hello";
        let parsed = parse_reflog_line("checkout", "HEAD", line).expect("parse");
        assert_eq!(parsed.old_oid, "abc123");
        assert_eq!(parsed.new_oid, "def456");
        assert_eq!(parsed.actor, "Alice <a@b.com>");
        assert_eq!(parsed.timestamp.as_deref(), Some("1700000000"));
        assert_eq!(parsed.message, "commit: hello");
        assert_eq!(parsed.source, "checkout");
        assert_eq!(parsed.reference, "HEAD");
    }

    #[test]
    fn parse_reflog_line_rejects_incomplete() {
        assert!(parse_reflog_line("s", "r", "onlyone").is_none());
    }

    #[test]
    fn short_oid_and_summarize_paths() {
        assert_eq!(short_oid("0123456789abcdef"), "0123456789ab");
        assert_eq!(short_oid("short"), "short");
        assert_eq!(summarize_paths(&[]), "");
        assert_eq!(summarize_paths(&["a".into()]), "a");
        assert_eq!(summarize_paths(&["a".into(), "b".into()]), "a, b");
        assert_eq!(
            summarize_paths(&["a".into(), "b".into(), "c".into(), "d".into()]),
            "a, b +2"
        );
    }
}
