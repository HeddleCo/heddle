// SPDX-License-Identifier: Apache-2.0
//! Pure log/reflog helpers: parse, summarize, short ids, timeline labels.
//!
//! FS traversal and styled render stay in CLI.

use objects::object::{
    TimelineBranchReason, TimelineCursorMoveReason, TimelineLabel, TimelineToolCallStatus,
};
use repo::TimelineNavigationRecoveryStatus;

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

/// Stable timeline label string for machine/text output.
pub fn timeline_label(label: &TimelineLabel) -> &'static str {
    match label {
        TimelineLabel::RepoReversible => "repo-reversible",
        TimelineLabel::ExternalSideEffectsUnknown => "external-side-effects-unknown",
        TimelineLabel::IgnoredPathTouched => "ignored-path-touched",
        TimelineLabel::OutsideRepoTouched => "outside-repo-touched",
        TimelineLabel::PurgeBoundary => "purge-boundary",
        TimelineLabel::CaptureFailed => "capture-failed",
    }
}

/// Tool-call status label.
pub fn timeline_tool_status(status: &TimelineToolCallStatus) -> &'static str {
    match status {
        TimelineToolCallStatus::Succeeded => "succeeded",
        TimelineToolCallStatus::Failed => "failed",
        TimelineToolCallStatus::Cancelled => "cancelled",
    }
}

/// Branch reason label.
pub fn timeline_branch_reason(reason: &TimelineBranchReason) -> &'static str {
    match reason {
        TimelineBranchReason::EditFromRewoundCursor => "edit-from-rewound-cursor",
        TimelineBranchReason::ExplicitFork => "explicit-fork",
        TimelineBranchReason::Retry => "retry",
        TimelineBranchReason::FanOut => "fan-out",
    }
}

/// Cursor move reason label.
pub fn timeline_cursor_reason(reason: &TimelineCursorMoveReason) -> &'static str {
    match reason {
        TimelineCursorMoveReason::SeekToolCall => "seek-tool-call",
        TimelineCursorMoveReason::Undo => "undo",
        TimelineCursorMoveReason::Redo => "redo",
        TimelineCursorMoveReason::Reset => "reset",
        TimelineCursorMoveReason::AutoAdvance => "auto-advance",
    }
}

/// Navigation recovery status label.
pub fn timeline_recovery_status(status: TimelineNavigationRecoveryStatus) -> &'static str {
    match status {
        TimelineNavigationRecoveryStatus::PendingCursorRecord => "pending-cursor-record",
        TimelineNavigationRecoveryStatus::Blocked => "blocked",
        TimelineNavigationRecoveryStatus::AlreadyApplied => "already-applied",
    }
}

/// Session list row status (active vs ended).
pub fn session_list_status(is_active: bool) -> &'static str {
    if is_active { "active" } else { "ended" }
}

/// yes/no for compact timeline fields.
pub fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// Truncate a string with an ellipsis when longer than `max_len`.
pub fn truncate_with_ellipsis(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

/// Fit `Name <email>` attribution into `max_len` without dropping the name.
pub fn fit_author(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    if let Some(angle) = s.find(" <") {
        let name = &s[..angle];
        if name.len() <= max_len {
            return name.to_string();
        }
    }
    truncate_with_ellipsis(s, max_len)
}

/// First non-empty line of content, truncated for blame/context snippets.
pub fn summarize_context_line(content: &str) -> String {
    let first_line = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    if first_line.len() <= 88 {
        first_line.to_string()
    } else {
        format!("{}...", &first_line[..85])
    }
}

/// Extract bytes for a 1-indexed inclusive line range from source content.
///
/// If `range` is `None`, returns the full source. Invalid/out-of-range
/// starts yield empty bytes.
pub fn extract_scope_bytes(source: &[u8], range: Option<(u32, u32)>) -> Vec<u8> {
    let Some((start, end)) = range else {
        return source.to_vec();
    };
    let text = std::str::from_utf8(source).unwrap_or("");
    let lines: Vec<&str> = text.lines().collect();
    let start_idx = (start as usize).saturating_sub(1);
    let end_idx = (end as usize).min(lines.len());
    if start_idx >= lines.len() {
        return Vec::new();
    }
    lines[start_idx..end_idx].join("\n").into_bytes()
}

/// Format missing blob shorts for fetch error augmentation.
pub fn format_missing_blobs_suffix(missing_shorts: &[String]) -> Option<String> {
    if missing_shorts.is_empty() {
        None
    } else {
        Some(format!("missing blobs: {}", missing_shorts.join(", ")))
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

    #[test]
    fn timeline_and_session_labels() {
        assert_eq!(
            timeline_label(&TimelineLabel::RepoReversible),
            "repo-reversible"
        );
        assert_eq!(
            timeline_tool_status(&TimelineToolCallStatus::Failed),
            "failed"
        );
        assert_eq!(
            timeline_branch_reason(&TimelineBranchReason::FanOut),
            "fan-out"
        );
        assert_eq!(
            timeline_cursor_reason(&TimelineCursorMoveReason::Undo),
            "undo"
        );
        assert_eq!(
            timeline_recovery_status(TimelineNavigationRecoveryStatus::Blocked),
            "blocked"
        );
        assert_eq!(session_list_status(true), "active");
        assert_eq!(session_list_status(false), "ended");
        assert_eq!(yes_no(true), "yes");
        assert_eq!(truncate_with_ellipsis("abcdef", 5), "ab...");
        assert_eq!(
            fit_author("Ada Lovelace <ada@really.long.example.com>", 12),
            "Ada Lovelace"
        );
        assert_eq!(summarize_context_line("\n  hello world\n"), "  hello world");
        let src = b"a\nb\nc\n";
        assert_eq!(extract_scope_bytes(src, None), src.to_vec());
        assert_eq!(extract_scope_bytes(src, Some((2, 3))), b"b\nc".to_vec());
        assert!(extract_scope_bytes(src, Some((10, 12))).is_empty());
        assert_eq!(
            format_missing_blobs_suffix(&["aa".into(), "bb".into()]).as_deref(),
            Some("missing blobs: aa, bb")
        );
        assert!(format_missing_blobs_suffix(&[]).is_none());
    }
}
