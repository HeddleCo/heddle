// SPDX-License-Identifier: Apache-2.0
//! Pure shell-completion validation (no clap / script I/O / recovery copy).
//!
//! Core owns shell name parsing and a typed error. Presentation
//! (`RecoveryAdvice` kind/summary/hint/example) is CLI-owned.

/// Shells that Heddle emits completion scripts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

/// Failure to parse a completion shell name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionShellError {
    /// Token is not one of `bash` / `zsh` / `fish`.
    Unsupported { shell: String },
}

/// Parse a user-supplied shell name into a known completion target.
///
/// Accepts only the exact lowercase tokens `bash`, `zsh`, and `fish`.
pub fn parse_completion_shell(s: &str) -> Result<CompletionShell, CompletionShellError> {
    match s {
        "bash" => Ok(CompletionShell::Bash),
        "zsh" => Ok(CompletionShell::Zsh),
        "fish" => Ok(CompletionShell::Fish),
        other => Err(CompletionShellError::Unsupported {
            shell: other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_shells() {
        assert_eq!(parse_completion_shell("bash"), Ok(CompletionShell::Bash));
        assert_eq!(parse_completion_shell("zsh"), Ok(CompletionShell::Zsh));
        assert_eq!(parse_completion_shell("fish"), Ok(CompletionShell::Fish));
        assert!(matches!(
            parse_completion_shell("BASH"),
            Err(CompletionShellError::Unsupported { .. })
        ));
        assert!(matches!(
            parse_completion_shell("powershell"),
            Err(CompletionShellError::Unsupported { shell }) if shell == "powershell"
        ));
    }
}
