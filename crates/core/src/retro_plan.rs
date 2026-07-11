// SPDX-License-Identifier: Apache-2.0
//! Pure `heddle retro` planning (no FS / oplog / registry I/O).
//!
//! Owns duration math, window-overlap predicates, free-text excerpt/scrub,
//! verify-signal heuristics, and turn-boundary intent matching from primitive
//! facts. Oplog walks, store lookups, and RecoveryAdvice stay CLI-owned.

/// Maximum oplog batches to scan when assembling the retro.
pub const MAX_OPLOG_BATCHES: usize = 4096;

/// Default fallback window (hours) when `--since` is omitted and no turn
/// boundary capture is found.
pub const DEFAULT_FALLBACK_WINDOW_HOURS: i64 = 1;

/// Length of excerpted free-text fields in non-verbose mode.
pub const EXCERPT_LEN: usize = 160;

/// Minimum confidence for a `verified:` intent to count as a pass signal.
pub const VERIFY_PASS_MIN_CONFIDENCE: f32 = 0.85;

/// Wall-clock window length in seconds: `until - since`, floored at zero.
///
/// Returns `None` when either bound is missing.
pub fn duration_secs(since_ts: Option<i64>, until_ts: Option<i64>) -> Option<i64> {
    match (since_ts, until_ts) {
        (Some(lo), Some(hi)) => Some((hi - lo).max(0)),
        _ => None,
    }
}

/// Whether an activity spanning `[start, end]` (open-ended if `end` is `None`)
/// intersects the half-open window `[since_ts, +∞)`.
///
/// - No lower bound → always overlaps.
/// - Still active (`end` is `None`) → overlaps.
/// - Finished activity → overlaps when `end >= since_ts` or `start >= since_ts`.
pub fn window_overlaps(since_ts: Option<i64>, start: i64, end: Option<i64>) -> bool {
    let Some(lo) = since_ts else {
        return true;
    };
    match end {
        None => true,
        Some(e) => e >= lo || start >= lo,
    }
}

/// Agent registry window filter from primitive activity facts.
///
/// Mirrors the retro agent inclusion rule: active agents always count; otherwise
/// the last activity timestamp must fall at or after `since_ts`.
pub fn agent_window_overlaps(
    since_ts: Option<i64>,
    is_active: bool,
    last_activity_ts: i64,
) -> bool {
    match since_ts {
        Some(lo) => is_active || last_activity_ts >= lo,
        None => true,
    }
}

/// Agent-task window filter from assignment + update/completion timestamps.
pub fn agent_task_window_overlaps(
    since_ts: Option<i64>,
    is_active_assigned: bool,
    updated_at: i64,
    completed_at: Option<i64>,
) -> bool {
    match since_ts {
        Some(lo) => {
            is_active_assigned
                || updated_at >= lo
                || completed_at.is_some_and(|completed| completed >= lo)
        }
        None => true,
    }
}

/// Timeline step inclusion: keep steps whose finish (or start) ms is >= lower bound.
pub fn timeline_step_in_window(since_ms: Option<i64>, step_ms: Option<i64>) -> bool {
    match since_ms {
        None => true,
        Some(lo) => step_ms.is_some_and(|ms| ms >= lo),
    }
}

/// Context annotation inclusion from unix-second timestamps.
pub fn context_annotation_in_window(since_secs: Option<i64>, created_at: i64) -> bool {
    match since_secs {
        None => true,
        Some(lo) => created_at >= lo,
    }
}

/// Choose the default lower-bound timestamp when `--since` is omitted.
///
/// Prefer a recent turn-boundary capture when it is newer than the fallback
/// window; otherwise use `now - fallback_hours` when a HEAD exists.
pub fn choose_default_since_ts(
    now_secs: i64,
    recent_turn_ts: Option<i64>,
    has_head: bool,
    fallback_hours: i64,
) -> Option<i64> {
    let one_window_ago = now_secs.saturating_sub(fallback_hours.saturating_mul(3600));
    match (recent_turn_ts, has_head) {
        (Some(turn_ts), _) if turn_ts > one_window_ago => Some(turn_ts),
        (_, true) => Some(one_window_ago),
        _ => None,
    }
}

/// Intent strings that mark a session/turn boundary for the default window.
pub fn is_turn_boundary_intent(intent: &str) -> bool {
    intent.contains("Claude Code turn")
        || intent.contains("session segment")
        || intent.contains("UserPromptSubmit")
}

/// High-confidence `verified:` capture → pass verify signal.
pub fn is_verify_pass_signal(intent: &str, confidence: Option<f32>) -> bool {
    intent.starts_with("verified:") && confidence.unwrap_or(0.0) >= VERIFY_PASS_MIN_CONFIDENCE
}

/// `failed-*` marker name → fail verify signal.
pub fn is_verify_fail_marker(name: &str) -> bool {
    name.starts_with("failed-")
}

/// Truncate free text to [`EXCERPT_LEN`] chars, appending `…` when cut.
pub fn excerpt(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= EXCERPT_LEN {
        return trimmed.to_string();
    }
    let take: String = trimmed.chars().take(EXCERPT_LEN).collect();
    format!("{take}…")
}

/// Verbose keeps full text; non-verbose excerpts and scrubs path-like tokens.
pub fn display_free_text(s: &str, verbose: bool) -> String {
    if verbose {
        s.trim().to_string()
    } else {
        scrub_path_like_tokens(&excerpt(s))
    }
}

/// Replace path-like tokens with `[redacted-path]`.
pub fn scrub_path_like_tokens(s: &str) -> String {
    s.split_whitespace()
        .map(|token| {
            if is_path_like_token(token) {
                "[redacted-path]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Heuristic: token looks like a filesystem path or `name.ext` file.
pub fn is_path_like_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\''
                | '`'
                | ','
                | ';'
                | ':'
                | '.'
                | '!'
                | '?'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
        )
    });
    if trimmed.contains('/') || trimmed.contains('\\') {
        return true;
    }
    let Some((stem, extension)) = trimmed.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && !extension.is_empty()
        && extension.len() <= 10
        && extension.chars().all(|c| c.is_ascii_alphanumeric())
        && stem
            .chars()
            .any(|c| c.is_ascii_alphabetic() || matches!(c, '-' | '_'))
}

/// Short display prefix for a change id (`hd-` + 8 hex when present).
pub fn short_change_id(id: &str) -> &str {
    let id_no_prefix = id.strip_prefix("hd-").unwrap_or(id);
    let prefix_len = if id.starts_with("hd-") { 3 } else { 0 };
    let take = std::cmp::min(8, id_no_prefix.len());
    &id[..(prefix_len + take)]
}

/// Human duration field for the retro header.
pub fn format_duration_label(duration_secs: Option<i64>) -> String {
    duration_secs
        .map(|s| format!("{s}s"))
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// Human `since=` field when no explicit since id was resolved.
pub fn default_since_label() -> &'static str {
    "<default-window>"
}

/// Human `until=` field when HEAD is missing.
pub fn no_head_until_label() -> &'static str {
    "<no-head>"
}

/// Retro header line body (without trailing newline).
pub fn retro_header_line(
    since: Option<&str>,
    until: Option<&str>,
    duration_secs: Option<i64>,
) -> String {
    format!(
        "Retro: since={} until={} duration={}",
        since.unwrap_or(default_since_label()),
        until.unwrap_or(no_head_until_label()),
        format_duration_label(duration_secs),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_and_window_overlap() {
        assert_eq!(duration_secs(Some(100), Some(150)), Some(50));
        assert_eq!(duration_secs(Some(150), Some(100)), Some(0));
        assert_eq!(duration_secs(None, Some(100)), None);
        assert_eq!(duration_secs(Some(100), None), None);

        assert!(window_overlaps(None, 0, Some(1)));
        assert!(window_overlaps(Some(100), 50, None));
        assert!(window_overlaps(Some(100), 50, Some(120)));
        assert!(window_overlaps(Some(100), 110, Some(120)));
        assert!(!window_overlaps(Some(100), 10, Some(20)));
    }

    #[test]
    fn agent_and_task_predicates() {
        assert!(agent_window_overlaps(None, false, 0));
        assert!(agent_window_overlaps(Some(100), true, 0));
        assert!(agent_window_overlaps(Some(100), false, 100));
        assert!(!agent_window_overlaps(Some(100), false, 50));

        assert!(agent_task_window_overlaps(None, false, 0, None));
        assert!(agent_task_window_overlaps(Some(100), true, 0, None));
        assert!(agent_task_window_overlaps(Some(100), false, 100, None));
        assert!(agent_task_window_overlaps(Some(100), false, 0, Some(150)));
        assert!(!agent_task_window_overlaps(Some(100), false, 10, Some(20)));

        assert!(timeline_step_in_window(None, None));
        assert!(!timeline_step_in_window(Some(10), None));
        assert!(timeline_step_in_window(Some(10), Some(10)));
        assert!(context_annotation_in_window(None, 0));
        assert!(!context_annotation_in_window(Some(10), 5));
    }

    #[test]
    fn default_since_prefers_recent_turn() {
        let now = 1_000_000;
        // Turn inside the last hour.
        assert_eq!(
            choose_default_since_ts(now, Some(now - 60), true, DEFAULT_FALLBACK_WINDOW_HOURS),
            Some(now - 60)
        );
        // Stale turn → fallback hour window.
        assert_eq!(
            choose_default_since_ts(now, Some(now - 10_000), true, DEFAULT_FALLBACK_WINDOW_HOURS),
            Some(now - 3600)
        );
        // No head, no turn → none.
        assert_eq!(
            choose_default_since_ts(now, None, false, DEFAULT_FALLBACK_WINDOW_HOURS),
            None
        );
    }

    #[test]
    fn verify_and_turn_heuristics() {
        assert!(is_verify_pass_signal("verified: ok", Some(0.9)));
        assert!(!is_verify_pass_signal("verified: ok", Some(0.5)));
        assert!(!is_verify_pass_signal("other", Some(0.9)));
        assert!(is_verify_fail_marker("failed-123"));
        assert!(!is_verify_fail_marker("v1.0.0"));
        assert!(is_turn_boundary_intent("Claude Code turn start"));
        assert!(is_turn_boundary_intent("session segment boundary"));
        assert!(is_turn_boundary_intent("UserPromptSubmit hook"));
        assert!(!is_turn_boundary_intent("regular capture"));
    }

    #[test]
    fn excerpt_and_scrub() {
        let long = "a".repeat(EXCERPT_LEN + 50);
        let out = excerpt(&long);
        assert_eq!(out.chars().count(), EXCERPT_LEN + 1);
        assert!(out.ends_with('…'));
        assert_eq!(excerpt("short content"), "short content");

        let s =
            "Review src/lib.rs and private-secret-name.txt. Check secret.env! Maybe docs/plan.md?";
        let scrubbed = display_free_text(s, false);
        assert!(!scrubbed.contains("src/lib.rs"));
        assert!(!scrubbed.contains("private-secret-name.txt"));
        assert!(scrubbed.contains("[redacted-path]"));
        assert_eq!(display_free_text(s, true), s);
    }

    #[test]
    fn short_id_and_header() {
        assert_eq!(short_change_id("hd-abcdef0123456789"), "hd-abcdef01");
        assert_eq!(short_change_id("short"), "short");
        let line = retro_header_line(None, None, Some(42));
        assert!(line.contains("since=<default-window>"));
        assert!(line.contains("until=<no-head>"));
        assert!(line.contains("duration=42s"));
    }
}
