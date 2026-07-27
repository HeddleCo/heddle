// SPDX-License-Identifier: Apache-2.0
//! Pure thread-approval display helpers (no hosted network / repo I/O).
//!
//! Timestamp and change-id helpers take plain `i64` / bytes so callers can
//! strip `prost` types at the CLI boundary. Recovery advice and network I/O
//! stay CLI-owned.

/// Non-negative seconds from an optional protobuf-style `seconds` field.
pub fn timestamp_secs_u64(seconds: Option<i64>) -> u64 {
    seconds.map(|s| s.max(0) as u64).unwrap_or(0)
}

/// RFC3339 (or raw seconds) label for a unix timestamp; empty when `secs == 0`.
pub fn format_unix_secs_label(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    format_unix_secs_display(secs)
}

/// RFC3339 (or raw seconds) for a unix timestamp, including epoch for `0`.
pub fn format_unix_secs_display(secs: u64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| secs.to_string())
}

/// Decode change-id wire bytes to a full hex string; empty/invalid → empty.
pub fn state_id_bytes_to_string(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    objects::object::StateId::try_from_slice(bytes)
        .map(|id| id.to_string_full())
        .unwrap_or_default()
}

/// First 12 characters of a change-id / state string for human display.
pub fn short_state_id(id: &str) -> &str {
    &id[..id.len().min(12)]
}

// ---------------------------------------------------------------------------
// Human message tokens
// ---------------------------------------------------------------------------

/// One-line success after `thread approve`.
pub fn approval_recorded_message(source: &str, target: &str, source_state: &str) -> String {
    format!(
        "Approved {source} -> {target} at {state}",
        state = short_state_id(source_state),
    )
}

/// Empty list message for `thread approvals`.
pub fn approvals_empty_message(source: &str, target: &str) -> String {
    format!("No approvals recorded for {source} -> {target}.")
}

/// Header when one or more approvals are listed.
pub fn approvals_header(count: usize, source: &str, target: &str) -> String {
    format!("{count} approval(s) for {source} -> {target}:")
}

/// Success after `thread revoke-approval`.
pub fn approval_revoked_message(id: &str) -> String {
    format!("Revoked approval {id}.")
}

/// Human summary plan for `thread check-merge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EligibilitySummary {
    /// Merge is allowed; `approval_count` is how many valid approvals counted.
    Allowed { approval_count: usize },
    /// Merge is blocked; `unmet_count` unmet requirements.
    Blocked { unmet_count: usize },
}

/// Plan eligibility text from gate response fields (no RPC types).
pub fn plan_eligibility_summary(
    allowed: bool,
    approval_count: usize,
    unmet_count: usize,
) -> EligibilitySummary {
    if allowed {
        EligibilitySummary::Allowed { approval_count }
    } else {
        EligibilitySummary::Blocked { unmet_count }
    }
}

/// Allowed merge one-liner.
pub fn eligibility_allowed_message(source: &str, target: &str) -> String {
    format!("{source} -> {target} can merge.")
}

/// Parenthetical when valid approvals were counted under an allowed result.
pub fn eligibility_approvals_counted_message(count: usize) -> String {
    format!("  ({count} approval(s) counted)")
}

/// Blocked merge header with unmet count.
pub fn eligibility_blocked_message(source: &str, target: &str, unmet_count: usize) -> String {
    format!("{source} -> {target} BLOCKED by {unmet_count} unmet requirement(s):")
}

/// Single unmet-requirement detail line.
pub fn unmet_requirement_line(kind: &str, reason: &str, have: u32, needed: u32) -> String {
    format!("  [{kind}] {reason} (have {have}/{needed})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_and_state_id_helpers() {
        assert_eq!(timestamp_secs_u64(None), 0);
        assert_eq!(timestamp_secs_u64(Some(-3)), 0);
        assert_eq!(timestamp_secs_u64(Some(42)), 42);
        assert_eq!(format_unix_secs_label(0), "");
        assert!(!format_unix_secs_display(0).is_empty()); // epoch
        let labeled = format_unix_secs_label(1_700_000_000);
        assert!(!labeled.is_empty());

        assert_eq!(state_id_bytes_to_string(&[]), "");
        assert_eq!(state_id_bytes_to_string(&[0xff, 0x00]), "");
        assert_eq!(short_state_id("abcdefghijklmnop"), "abcdefghijkl");
        assert_eq!(short_state_id("abc"), "abc");
    }

    #[test]
    fn approval_messages() {
        assert_eq!(
            approval_recorded_message("feat", "main", "0123456789abcdef"),
            "Approved feat -> main at 0123456789ab"
        );
        assert_eq!(
            approvals_empty_message("a", "b"),
            "No approvals recorded for a -> b."
        );
        assert_eq!(approvals_header(2, "a", "b"), "2 approval(s) for a -> b:");
        assert_eq!(approval_revoked_message("id-1"), "Revoked approval id-1.");
    }

    #[test]
    fn eligibility_plan_and_messages() {
        assert_eq!(
            plan_eligibility_summary(true, 2, 0),
            EligibilitySummary::Allowed { approval_count: 2 }
        );
        assert_eq!(
            plan_eligibility_summary(false, 0, 3),
            EligibilitySummary::Blocked { unmet_count: 3 }
        );
        assert_eq!(eligibility_allowed_message("s", "t"), "s -> t can merge.");
        assert_eq!(
            eligibility_approvals_counted_message(1),
            "  (1 approval(s) counted)"
        );
        assert_eq!(
            eligibility_blocked_message("s", "t", 2),
            "s -> t BLOCKED by 2 unmet requirement(s):"
        );
        assert_eq!(
            unmet_requirement_line("approvals", "need more", 1, 2),
            "  [approvals] need more (have 1/2)"
        );
    }
}
