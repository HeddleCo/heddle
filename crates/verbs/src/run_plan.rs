// SPDX-License-Identifier: Apache-2.0
//! Pure `heddle run` planning helpers (no process spawn / FS I/O).
//!
//! Owns empty-command gate tokens and failure message assembly.
//! Child env, transport config, and process execution stay CLI-owned.

/// Stable recovery-advice kind when no command was supplied after `--`.
pub fn run_command_required_kind() -> &'static str {
    "run_command_required"
}

/// Usage summary for the empty-command refusal.
pub fn run_command_required_summary() -> &'static str {
    "Usage: heddle run --thread <name> -- <cmd...>"
}

/// Hint for the empty-command refusal.
pub fn run_command_required_hint() -> &'static str {
    "Pass a command after `--` so Heddle knows what to execute in the thread checkout."
}

/// Example command for the empty-command refusal.
pub fn run_command_required_example() -> &'static str {
    "heddle run --thread <name> -- <cmd...>"
}

/// Gate: true when the argv after `--` is empty (refuse before spawn).
pub fn plan_run_command_empty(command_empty: bool) -> bool {
    command_empty
}

/// Human/error message when the child process fails.
///
/// `status_code` is `Some(code)` for normal exits; `None` maps to `"signal"`.
pub fn run_failure_message(program: &str, thread_id: &str, status_code: Option<i32>) -> String {
    let status = status_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());
    format!("Command '{program}' failed in thread '{thread_id}' with status {status}")
}

/// Stable harness transport env token from a config kind label.
///
/// Accepts the CLI/config variant names (`spool` / `direct` / `end`);
/// unknown inputs are returned unchanged for forward compatibility.
pub fn transport_token(kind: &str) -> &str {
    match kind {
        "spool" | "direct" | "end" => kind,
        other => other,
    }
}

/// Stable harness transcript env token from a config kind label.
pub fn transcript_token(kind: &str) -> &str {
    match kind {
        "off" | "summary" | "full" => kind,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_command_gate_and_tokens() {
        assert!(plan_run_command_empty(true));
        assert!(!plan_run_command_empty(false));
        assert_eq!(run_command_required_kind(), "run_command_required");
        assert!(run_command_required_summary().contains("heddle run"));
        assert!(!run_command_required_hint().is_empty());
        assert_eq!(
            run_command_required_example(),
            "heddle run --thread <name> -- <cmd...>"
        );
    }

    #[test]
    fn failure_message() {
        assert_eq!(
            run_failure_message("cargo", "agent-1", Some(1)),
            "Command 'cargo' failed in thread 'agent-1' with status 1"
        );
        assert_eq!(
            run_failure_message("sh", "t", None),
            "Command 'sh' failed in thread 't' with status signal"
        );
    }

    #[test]
    fn transport_and_transcript_tokens() {
        assert_eq!(transport_token("spool"), "spool");
        assert_eq!(transport_token("direct"), "direct");
        assert_eq!(transport_token("end"), "end");
        assert_eq!(transport_token("custom"), "custom");
        assert_eq!(transcript_token("off"), "off");
        assert_eq!(transcript_token("summary"), "summary");
        assert_eq!(transcript_token("full"), "full");
    }
}
