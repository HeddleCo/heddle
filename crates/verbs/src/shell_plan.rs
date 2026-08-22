// SPDX-License-Identifier: Apache-2.0
//! Pure shell-hook snippet selection (no FS / stdout I/O).
//!
//! Owns the wrapper function bodies emitted by `heddle shell init`.
//! CLI maps its clap `ShellKind` onto [`ShellHookKind`] and prints the result.

/// Shell families that share a hook snippet shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellHookKind {
    Zsh,
    Bash,
    Fish,
}

/// Return the shell-hook snippet for `kind`.
///
/// zsh and bash share one function body; fish uses a separate syntax.
pub fn shell_hook_snippet(kind: ShellHookKind) -> &'static str {
    match kind {
        ShellHookKind::Zsh | ShellHookKind::Bash => ZSH_BASH_SNIPPET,
        ShellHookKind::Fish => FISH_SNIPPET,
    }
}

/// zsh + bash share a function shape. The differences (`local`
/// availability, `${@:N}` slicing) are compatible across both.
const ZSH_BASH_SNIPPET: &str = r#"# heddle shell hook — installed via `heddle shell init zsh` (or bash)
# Wraps `heddle start`, `heddle thread switch`, and `heddle thread cd`
# so they auto-cd into the target thread's worktree.
# Also defines `__heddle_ps1`, a compact prompt segment helper.
heddle() {
    case "$1 $2" in
        "start "*)
            local path
            path=$(command heddle start "${@:2}" --print-cd-path 2>/dev/null) || {
                command heddle "$@"
                return $?
            }
            cd "$path" && printf 'heddle: %s\n' "$path"
            ;;
        "thread switch "*)
            local path
            path=$(command heddle thread switch "${@:3}" --print-cd-path 2>/dev/null) || {
                command heddle "$@"
                return $?
            }
            cd "$path" && printf 'heddle: %s\n' "$path"
            ;;
        "thread cd "*)
            local path
            path=$(command heddle thread cd "${@:3}") || return $?
            cd "$path"
            ;;
        *)
            command heddle "$@"
            ;;
    esac
}

__heddle_ps1() {
    local segment
    segment=$(command heddle shell prompt 2>/dev/null) || return 0
    [ -n "$segment" ] && printf '(%s)' "$segment"
}
"#;

/// fish uses a different function syntax. Wrappable via `function … end`.
const FISH_SNIPPET: &str = r#"# heddle shell hook — installed via `heddle shell init fish`
# Wraps `heddle start`, `heddle thread switch`, and `heddle thread cd`
# so they auto-cd into the target thread's worktree.
# Also defines `__heddle_ps1`, a compact prompt segment helper.
function heddle
    switch "$argv[1] $argv[2]"
        case 'start *'
            set -l path (command heddle start $argv[2..] --print-cd-path 2>/dev/null)
            if test $status -ne 0
                command heddle $argv
                return $status
            end
            cd "$path"; and printf 'heddle: %s\n' "$path"
        case 'thread switch *'
            set -l path (command heddle thread switch $argv[3..] --print-cd-path 2>/dev/null)
            if test $status -ne 0
                command heddle $argv
                return $status
            end
            cd "$path"; and printf 'heddle: %s\n' "$path"
        case 'thread cd *'
            set -l path (command heddle thread cd $argv[3..])
            if test $status -ne 0
                return $status
            end
            cd "$path"
        case '*'
            command heddle $argv
    end
end

function __heddle_ps1
    set -l segment (command heddle shell prompt 2>/dev/null)
    if test -n "$segment"
        printf '(%s)' "$segment"
    end
end
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zsh_and_bash_share_the_same_body() {
        assert!(std::ptr::eq(
            shell_hook_snippet(ShellHookKind::Zsh),
            shell_hook_snippet(ShellHookKind::Bash)
        ));
        let body = shell_hook_snippet(ShellHookKind::Zsh);
        assert!(body.contains("heddle() {"));
        assert!(body.contains("__heddle_ps1()"));
    }

    #[test]
    fn fish_uses_fish_function_syntax() {
        let body = shell_hook_snippet(ShellHookKind::Fish);
        assert!(body.contains("function heddle"));
        assert!(body.contains("$argv"));
        assert!(body.contains("function __heddle_ps1"));
    }
}
