// SPDX-License-Identifier: Apache-2.0
//! Near-miss table for Git-shaped verbs that are not Heddle commands.
//!
//! Clap already suggests Levenshtein-near names (`statuz` → `status`).
//! Users who follow the old text vocabulary (`save`) or Git muscle memory
//! (`add`, `stash`) land too far from the real verb for that heuristic.

/// Suggested replacement for an unrecognized top-level subcommand.
pub fn suggested_command(unknown: &str) -> Option<&'static str> {
    match unknown.trim().to_ascii_lowercase().as_str() {
        "save" | "add" => Some("capture"),
        "stash" => Some("start"),
        "presence" => Some("agent presence"),
        "timeline" => Some("agent timeline"),
        "collapse" => Some("thread collapse"),
        "expand" => Some("thread expand"),
        "oplog" => Some("maintenance oplog recover"),
        _ => None,
    }
}

/// Clap-shaped usage error that names the near-miss.
pub fn format_unrecognized_suggestion(unknown: &str, suggested: &str) -> String {
    format!(
        "error: unrecognized subcommand '{unknown}'\n\n  tip: a similar subcommand exists: '{suggested}'\n\nUsage: heddle [OPTIONS] <COMMAND>\n\nFor more information, try '--help'.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_add_suggest_capture() {
        assert_eq!(suggested_command("save"), Some("capture"));
        assert_eq!(suggested_command("SAVE"), Some("capture"));
        assert_eq!(suggested_command("add"), Some("capture"));
    }

    #[test]
    fn stash_suggests_start() {
        assert_eq!(suggested_command("stash"), Some("start"));
    }

    #[test]
    fn moved_commands_suggest_their_nested_paths() {
        for (old, new) in [
            ("presence", "agent presence"),
            ("timeline", "agent timeline"),
            ("collapse", "thread collapse"),
            ("expand", "thread expand"),
            ("oplog", "maintenance oplog recover"),
        ] {
            assert_eq!(suggested_command(old), Some(new));
        }
    }

    #[test]
    fn unknown_verbs_have_no_table_hit() {
        assert_eq!(suggested_command("statuz"), None);
        assert_eq!(suggested_command("capture"), None);
    }

    #[test]
    fn suggestion_text_names_both_verbs() {
        let text = format_unrecognized_suggestion("save", "capture");
        assert!(text.contains("unrecognized subcommand 'save'"));
        assert!(text.contains("capture"));
    }
}
