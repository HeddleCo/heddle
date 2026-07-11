// SPDX-License-Identifier: Apache-2.0
//! Pure shell-completion validation (no clap / script I/O).
//!
//! Owns shell name parsing and unsupported-shell summary text for
//! `heddle shell completion` / `heddle completion`. Dynamic completion
//! script bodies stay CLI-owned (large shell sources).

/// Shells that Heddle emits completion scripts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

/// Parse a user-supplied shell name into a known completion target.
///
/// Accepts only the exact lowercase tokens `bash`, `zsh`, and `fish`.
pub fn parse_completion_shell(s: &str) -> Option<CompletionShell> {
    match s {
        "bash" => Some(CompletionShell::Bash),
        "zsh" => Some(CompletionShell::Zsh),
        "fish" => Some(CompletionShell::Fish),
        _ => None,
    }
}

/// Human summary when `shell` is not one of the supported completion shells.
pub fn completion_shell_unsupported_summary(shell: &str) -> String {
    format!("Unsupported shell: {shell}. Supported shells: bash, zsh, fish")
}

/// Stable recovery-advice kind for unsupported completion shells.
pub fn completion_shell_unsupported_kind() -> &'static str {
    "completion_shell_unsupported"
}

/// Hint line listing supported shells.
pub fn completion_shell_unsupported_hint() -> &'static str {
    "Use one of: bash, zsh, fish."
}

/// Example command for recovery advice.
pub fn completion_shell_example_command() -> &'static str {
    "heddle shell completion bash"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_shells() {
        assert_eq!(parse_completion_shell("bash"), Some(CompletionShell::Bash));
        assert_eq!(parse_completion_shell("zsh"), Some(CompletionShell::Zsh));
        assert_eq!(parse_completion_shell("fish"), Some(CompletionShell::Fish));
    }

    #[test]
    fn parse_rejects_unknown() {
        assert_eq!(parse_completion_shell(""), None);
        assert_eq!(parse_completion_shell("Bash"), None);
        assert_eq!(parse_completion_shell("powershell"), None);
        assert_eq!(parse_completion_shell("bash "), None);
    }

    #[test]
    fn unsupported_summary_and_tokens() {
        let summary = completion_shell_unsupported_summary("powershell");
        assert!(summary.contains("powershell"));
        assert!(summary.contains("bash"));
        assert!(summary.contains("zsh"));
        assert!(summary.contains("fish"));
        assert_eq!(
            completion_shell_unsupported_kind(),
            "completion_shell_unsupported"
        );
        assert_eq!(
            completion_shell_unsupported_hint(),
            "Use one of: bash, zsh, fish."
        );
        assert_eq!(
            completion_shell_example_command(),
            "heddle shell completion bash"
        );
    }
}
