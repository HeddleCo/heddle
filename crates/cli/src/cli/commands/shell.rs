// SPDX-License-Identifier: Apache-2.0
//! Shell integration helpers — emits the wrapper function that
//! makes `heddle start`, `heddle thread switch`, and
//! `heddle thread cd` auto-`cd` into the target thread's
//! worktree.
//!
//! Install with:
//!   echo 'eval "$(heddle shell init zsh)"' >> ~/.zshrc   # or bash / fish
//!
//! The wrapper does three things:
//!   1. `heddle start <name>` → run the real CLI with `--print-cd-path`,
//!      capture the path on stdout, `cd` there, print a one-line
//!      confirmation. On failure, falls back to running the full
//!      command so the user sees the normal error output.
//!   2. `heddle thread switch <name>` → same shape, but the auto-
//!      capture-on-switch run side-effects are preserved (the rich
//!      output is suppressed in favour of just the path).
//!   3. `heddle thread cd <name>` → read-only lookup of the
//!      thread's path, then `cd`. Equivalent to
//!      `cd "$(heddle thread cd <name>)"` if you'd rather type
//!      it without the hook.
//!
//! Every other subcommand passes straight through to the real
//! `heddle` binary — the wrapper is invisible for non-thread work.

use anyhow::Result;

use super::{cmd_completion, status::prompt_segment};
use crate::cli::{Cli, ShellCommands, ShellKind};

pub fn cmd_shell(cli: &Cli, command: ShellCommands) -> Result<()> {
    match command {
        ShellCommands::Init { kind } => {
            // Stdout — the caller is expected to redirect / `eval`.
            print!("{}", snippet_for(kind));
            Ok(())
        }
        ShellCommands::Completion { shell } => cmd_completion(shell),
        ShellCommands::Prompt => {
            if let Some(segment) = prompt_segment(cli)? {
                println!("{segment}");
            }
            Ok(())
        }
    }
}

/// Return the shell-hook snippet for `kind`. Pure function so the
/// snippet selection is unit-testable without capturing stdout.
fn snippet_for(kind: ShellKind) -> &'static str {
    match kind {
        ShellKind::Zsh | ShellKind::Bash => ZSH_BASH_SNIPPET,
        ShellKind::Fish => FISH_SNIPPET,
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
    fn snippet_for_zsh_and_bash_share_the_same_body() {
        assert!(std::ptr::eq(
            snippet_for(ShellKind::Zsh),
            snippet_for(ShellKind::Bash)
        ));
        assert!(snippet_for(ShellKind::Zsh).contains("heddle() {"));
        assert!(snippet_for(ShellKind::Zsh).contains("__heddle_ps1()"));
    }

    #[test]
    fn snippet_for_fish_uses_fish_function_syntax() {
        let body = snippet_for(ShellKind::Fish);
        assert!(body.contains("function heddle"));
        assert!(body.contains("$argv"));
        assert!(body.contains("function __heddle_ps1"));
    }

    #[test]
    fn cmd_shell_init_runs_for_every_shell_kind() {
        for kind in [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish] {
            let cli = Cli {
                command: crate::cli::Commands::Shell {
                    command: ShellCommands::Init { kind },
                },
                output: None,
                no_color: false,
                repo: None,
                verbose: 0,
                quiet: false,
                op_id: None,
            };
            cmd_shell(&cli, ShellCommands::Init { kind }).expect("init prints");
        }
    }
}
