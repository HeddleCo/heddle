// SPDX-License-Identifier: Apache-2.0
//! Pure `heddle try` message assembly and status mapping (no FS / process I/O).
//!
//! Owns command display, thread-name formatting, drop-outcome mapping, and
//! human/JSON status strings for `heddle try`. Thread start/drop I/O,
//! RecoveryAdvice construction, hashing/time nonces, and styled output stay
//! CLI-owned.

/// Join a command argv for human messages (`["cargo", "test"]` → `"cargo test"`).
pub fn display_cmd(cmd: &[String]) -> String {
    cmd.join(" ")
}

/// Format an auto-generated try thread name from a 32-bit digest.
///
/// Callers mix command hash + time nonce into `digest` and pass the low 32 bits
/// (or any u32) here. Pure format only — no hashing or clocks.
pub fn format_try_thread_name(digest: u32) -> String {
    format!("try-{digest:08x}")
}

/// Prefix used by auto-generated try thread names.
pub fn default_try_name_prefix() -> &'static str {
    "try-"
}

/// Stable recovery-advice `kind` for try `--name` collision refusal.
pub fn try_thread_name_collision_kind() -> &'static str {
    "try_thread_name_collision"
}

/// Map a drop attempt to `(thread_dropped, cleanup_error)` without side effects.
///
/// `ok == true` means drop succeeded. When `ok` is false, `err_msg` is stored
/// as `cleanup_error` (CLI emits stderr / tracing around this).
pub fn plan_try_drop_outcome(ok: bool, err_msg: Option<String>) -> (bool, Option<String>) {
    if ok { (true, None) } else { (false, err_msg) }
}

/// Human fragment describing whether the ephemeral thread was dropped.
pub fn try_drop_status_fragment(thread_name: &str, thread_dropped: bool) -> String {
    if thread_dropped {
        format!("thread '{thread_name}' dropped")
    } else {
        format!("thread '{thread_name}' NOT dropped (cleanup failed)")
    }
}

/// Failure-path try message after a non-zero exit (and optional drop).
pub fn try_failed_message(
    cmd_display: &str,
    exit_label: &str,
    thread_name: &str,
    thread_dropped: bool,
) -> String {
    format!(
        "`{cmd_display}` failed (exit {exit_label}); {}",
        try_drop_status_fragment(thread_name, thread_dropped)
    )
}

/// Exit label for messages: decimal code or `"signal"`.
pub fn try_exit_label(exit_code: Option<i32>) -> String {
    exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".into())
}

/// Facts for assembling a success-path try message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrySuccessMessageFacts<'a> {
    pub cmd_display: &'a str,
    pub thread_name: &'a str,
    pub auto_merge: bool,
    pub captured_state: Option<&'a str>,
    pub merge_state: Option<&'a str>,
}

/// Success-path try message from pure capture/merge facts.
pub fn try_success_message(facts: TrySuccessMessageFacts<'_>) -> String {
    if facts.auto_merge {
        match (facts.captured_state, facts.merge_state) {
            (Some(state), Some(merge)) => format!(
                "`{}` succeeded; captured {}, merged into parent as {}",
                facts.cmd_display, state, merge
            ),
            (Some(state), None) => format!(
                "`{}` succeeded; captured {}, merge into parent skipped",
                facts.cmd_display, state
            ),
            _ => format!("`{}` succeeded; nothing to capture", facts.cmd_display),
        }
    } else {
        match facts.captured_state {
            Some(state) => format!(
                "`{}` succeeded; thread '{}' ready (state {}). Check readiness with `heddle ready --thread {}` before landing.",
                facts.cmd_display, facts.thread_name, state, facts.thread_name
            ),
            None => format!(
                "`{}` succeeded; thread '{}' ready (no capture).",
                facts.cmd_display, facts.thread_name
            ),
        }
    }
}

/// Status token for try JSON/human outcome (`"completed"` | `"failed"`).
pub fn try_status_token(success: bool) -> &'static str {
    if success { "completed" } else { "failed" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_cmd_joins_with_space() {
        assert_eq!(display_cmd(&["cargo".into(), "test".into()]), "cargo test");
        assert_eq!(display_cmd(&[]), "");
        assert_eq!(display_cmd(&["true".into()]), "true");
    }

    #[test]
    fn format_try_thread_name_is_hex_padded() {
        assert_eq!(format_try_thread_name(0), "try-00000000");
        assert_eq!(format_try_thread_name(0xdead_beef), "try-deadbeef");
        assert!(format_try_thread_name(1).starts_with(default_try_name_prefix()));
    }

    #[test]
    fn plan_try_drop_outcome_maps_ok_and_err() {
        assert_eq!(plan_try_drop_outcome(true, None), (true, None));
        assert_eq!(
            plan_try_drop_outcome(true, Some("ignored".into())),
            (true, None)
        );
        assert_eq!(
            plan_try_drop_outcome(false, Some("lock held".into())),
            (false, Some("lock held".into()))
        );
        assert_eq!(plan_try_drop_outcome(false, None), (false, None));
    }

    #[test]
    fn try_failed_and_success_messages() {
        assert_eq!(
            try_failed_message("cargo test", "1", "try-abc", true),
            "`cargo test` failed (exit 1); thread 'try-abc' dropped"
        );
        assert_eq!(
            try_failed_message("true", "signal", "t", false),
            "`true` failed (exit signal); thread 't' NOT dropped (cleanup failed)"
        );
        assert_eq!(try_exit_label(Some(2)), "2");
        assert_eq!(try_exit_label(None), "signal");

        let msg = try_success_message(TrySuccessMessageFacts {
            cmd_display: "true",
            thread_name: "try-1",
            auto_merge: false,
            captured_state: Some("state-a"),
            merge_state: None,
        });
        assert!(msg.contains("ready (state state-a)"));
        assert!(msg.contains("heddle ready --thread try-1"));

        let merged = try_success_message(TrySuccessMessageFacts {
            cmd_display: "true",
            thread_name: "try-1",
            auto_merge: true,
            captured_state: Some("s"),
            merge_state: Some("m"),
        });
        assert!(merged.contains("merged into parent as m"));
        assert_eq!(try_status_token(true), "completed");
        assert_eq!(try_status_token(false), "failed");
        assert_eq!(
            try_thread_name_collision_kind(),
            "try_thread_name_collision"
        );
    }
}
