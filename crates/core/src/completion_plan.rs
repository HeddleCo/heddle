// SPDX-License-Identifier: Apache-2.0
//! Pure shell-completion validation (no clap / script I/O).
//!
//! Owns shell name parsing. Presentation for unsupported shells is a single
//! typed helper so callers are not coupled to four string factories.

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

/// Facts for RecoveryAdvice when the shell is unsupported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedCompletionShell {
    pub kind: &'static str,
    pub summary: String,
    pub hint: &'static str,
    pub example: &'static str,
}

/// Build unsupported-shell recovery facts (one place for copy + kind token).
pub fn unsupported_completion_shell(shell: &str) -> UnsupportedCompletionShell {
    UnsupportedCompletionShell {
        kind: "completion_shell_unsupported",
        summary: format!("Unsupported shell: {shell}. Supported shells: bash, zsh, fish"),
        hint: "Use one of: bash, zsh, fish.",
        example: "heddle shell completion bash",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_shells() {
        assert_eq!(parse_completion_shell("bash"), Some(CompletionShell::Bash));
        assert_eq!(parse_completion_shell("zsh"), Some(CompletionShell::Zsh));
        assert_eq!(parse_completion_shell("fish"), Some(CompletionShell::Fish));
        assert_eq!(parse_completion_shell("BASH"), None);
        assert_eq!(parse_completion_shell("powershell"), None);
    }

    #[test]
    fn unsupported_summary_includes_shell() {
        let u = unsupported_completion_shell("tcsh");
        assert_eq!(u.kind, "completion_shell_unsupported");
        assert!(u.summary.contains("tcsh"));
        assert!(u.summary.contains("bash"));
    }
}
