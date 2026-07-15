// SPDX-License-Identifier: Apache-2.0
//! Pure `heddle watch` planning (no FS / notify / oplog I/O).
//!
//! Owns filter validation, relative `--since` duration parsing, notify-class
//! relevance, and filter matching against event kind strings. Repo open,
//! oplog drain, notify watcher setup, and RecoveryAdvice mapping stay CLI-owned.

use chrono::{DateTime, Duration as ChronoDuration, Utc};

/// Default debounce interval for the notify tail loop (milliseconds).
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 200;

/// Hard cap on the in-process recent-entries window.
pub const MAX_TAIL_WINDOW: usize = 100_000;

/// Invalid `--filter` values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchFilterPlanError {
    /// A comma-separated token was empty after trim (e.g. `snapshot,,merge`).
    EmptyToken,
    /// Token is not in the caller-supplied valid kinds list.
    UnknownKind { kind: String, valid: Vec<String> },
}

impl WatchFilterPlanError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::EmptyToken => "watch_filter_empty_token",
            Self::UnknownKind { .. } => "watch_filter_invalid",
        }
    }
}

/// Parse `--filter snapshot,merge` into a set of kind strings.
///
/// - `None` / blank / only empty tokens after trim → `Ok(None)` (no filter).
/// - Empty token between commas (e.g. `a,,b`) → `Err(EmptyToken)`.
/// - Unknown kind → `Err(UnknownKind { .. })` with the valid list for messaging.
pub fn plan_watch_filter(
    spec: Option<&str>,
    valid_kinds: &[&str],
) -> Result<Option<Vec<String>>, WatchFilterPlanError> {
    let Some(raw) = spec else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let mut kinds = Vec::new();
    for token in trimmed.split(',') {
        let kind = token.trim();
        if kind.is_empty() {
            return Err(WatchFilterPlanError::EmptyToken);
        }
        if !valid_kinds.contains(&kind) {
            return Err(WatchFilterPlanError::UnknownKind {
                kind: kind.to_string(),
                valid: valid_kinds.iter().map(|s| (*s).to_string()).collect(),
            });
        }
        kinds.push(kind.to_string());
    }
    if kinds.is_empty() {
        return Ok(None);
    }
    Ok(Some(kinds))
}

/// Invalid `--since` duration specs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchSincePlanError {
    /// Empty or whitespace-only string.
    Empty,
    /// Leading number portion failed to parse.
    InvalidNumber { spec: String },
    /// Unit is not `s` / `m` / `h` / `d` (or empty = seconds).
    UnknownUnit { unit: String },
}

impl WatchSincePlanError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Empty => "watch_since_empty",
            Self::InvalidNumber { .. } => "watch_since_invalid_number",
            Self::UnknownUnit { .. } => "watch_since_unknown_unit",
        }
    }
}

/// Parse a relative duration like `30s` / `5m` / `1h` / `2d` into a second count.
///
/// Empty unit means seconds. Does not touch wall-clock; pair with
/// [`plan_watch_since_cutoff`] for a UTC instant.
pub fn parse_since_duration_secs(spec: &str) -> Result<i64, WatchSincePlanError> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(WatchSincePlanError::Empty);
    }
    let (num_part, unit) = trimmed.split_at(
        trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len()),
    );
    if num_part.is_empty() {
        return Err(WatchSincePlanError::InvalidNumber {
            spec: trimmed.to_string(),
        });
    }
    let n: i64 = num_part
        .parse()
        .map_err(|_| WatchSincePlanError::InvalidNumber {
            spec: trimmed.to_string(),
        })?;
    let secs = match unit {
        "s" | "" => n,
        "m" => n.saturating_mul(60),
        "h" => n.saturating_mul(60 * 60),
        "d" => n.saturating_mul(60 * 60 * 24),
        other => {
            return Err(WatchSincePlanError::UnknownUnit {
                unit: other.to_string(),
            });
        }
    };
    Ok(secs)
}

/// Parse `--since` relative to a provided `now` (pure — no ambient clock).
pub fn plan_watch_since_cutoff(
    spec: &str,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, WatchSincePlanError> {
    let secs = parse_since_duration_secs(spec)?;
    Ok(now - ChronoDuration::seconds(secs))
}

/// Notify-side event class without depending on the `notify` crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchNotifyClass {
    Modify,
    Create,
    Remove,
    Other,
}

/// Whether a notify event class should trigger an oplog re-read.
///
/// Atomic `write_file_atomic` produces Create (temp) and Modify/Remove (rename).
pub fn is_relevant_watch_event(class: WatchNotifyClass) -> bool {
    matches!(
        class,
        WatchNotifyClass::Modify | WatchNotifyClass::Create | WatchNotifyClass::Remove
    )
}

/// UX alias: `--filter merge` matches the wire verb `thread_update`.
pub fn watch_kind_matches_filter(filter_kind: &str, event_kind: &str) -> bool {
    filter_kind == event_kind || (filter_kind == "merge" && event_kind == "thread_update")
}

/// Whether an event kind passes an optional filter list.
pub fn watch_passes_filter(filter: Option<&[String]>, event_kind: &str) -> bool {
    match filter {
        None => true,
        Some(allowed) => allowed
            .iter()
            .any(|k| watch_kind_matches_filter(k.as_str(), event_kind)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Vec<&'static str> {
        vec![
            "snapshot",
            "merge",
            "thread_create",
            "thread_update",
            "remote_thread_update",
            "purge",
        ]
    }

    #[test]
    fn plan_watch_filter_validates_kinds() {
        assert!(plan_watch_filter(None, &valid()).unwrap().is_none());
        assert!(plan_watch_filter(Some(""), &valid()).unwrap().is_none());
        assert!(plan_watch_filter(Some("  "), &valid()).unwrap().is_none());
        let parsed = plan_watch_filter(Some("snapshot,merge"), &valid())
            .unwrap()
            .unwrap();
        assert_eq!(parsed, vec!["snapshot", "merge"]);
        assert!(matches!(
            plan_watch_filter(Some("not_a_real_kind"), &valid()),
            Err(WatchFilterPlanError::UnknownKind { kind, .. }) if kind == "not_a_real_kind"
        ));
        assert_eq!(
            plan_watch_filter(Some("snapshot,,merge"), &valid()),
            Err(WatchFilterPlanError::EmptyToken)
        );
    }

    #[test]
    fn filter_accepts_catalog_kinds_when_listed() {
        for kind in ["remote_thread_update", "purge", "thread_create"] {
            assert!(
                plan_watch_filter(Some(kind), &valid()).is_ok(),
                "filter kind {kind:?} must be accepted when listed as valid"
            );
        }
    }

    #[test]
    fn parse_since_accepts_common_units() {
        assert_eq!(parse_since_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_since_duration_secs("5m").unwrap(), 5 * 60);
        assert_eq!(parse_since_duration_secs("2h").unwrap(), 2 * 60 * 60);
        assert_eq!(parse_since_duration_secs("1d").unwrap(), 86_400);
        assert_eq!(parse_since_duration_secs("45").unwrap(), 45);
    }

    #[test]
    fn parse_since_rejects_bad_input() {
        assert_eq!(
            parse_since_duration_secs(""),
            Err(WatchSincePlanError::Empty)
        );
        assert_eq!(
            parse_since_duration_secs("   "),
            Err(WatchSincePlanError::Empty)
        );
        assert!(matches!(
            parse_since_duration_secs("5x"),
            Err(WatchSincePlanError::UnknownUnit { unit }) if unit == "x"
        ));
        assert!(matches!(
            parse_since_duration_secs("m"),
            Err(WatchSincePlanError::InvalidNumber { .. })
        ));
    }

    #[test]
    fn plan_watch_since_cutoff_subtracts_from_now() {
        let now = DateTime::parse_from_rfc3339("2026-05-02T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let cutoff = plan_watch_since_cutoff("5m", now).unwrap();
        assert_eq!((now - cutoff).num_seconds(), 300);
    }

    #[test]
    fn relevant_events_and_filter_matching() {
        assert!(is_relevant_watch_event(WatchNotifyClass::Modify));
        assert!(is_relevant_watch_event(WatchNotifyClass::Create));
        assert!(is_relevant_watch_event(WatchNotifyClass::Remove));
        assert!(!is_relevant_watch_event(WatchNotifyClass::Other));

        assert!(watch_kind_matches_filter("snapshot", "snapshot"));
        assert!(watch_kind_matches_filter("merge", "thread_update"));
        assert!(!watch_kind_matches_filter("snapshot", "thread_create"));
        assert!(watch_passes_filter(None, "anything"));
        let filter = vec!["snapshot".into()];
        assert!(watch_passes_filter(Some(&filter), "snapshot"));
        assert!(!watch_passes_filter(Some(&filter), "thread_create"));
    }

    #[test]
    fn constants_match_historical_cli() {
        assert_eq!(DEFAULT_POLL_INTERVAL_MS, 200);
        assert_eq!(MAX_TAIL_WINDOW, 100_000);
    }
}
