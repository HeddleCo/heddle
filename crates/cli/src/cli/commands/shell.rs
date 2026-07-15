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
use heddle_core::shell_plan::{ShellHookKind, shell_hook_snippet};

use super::{cmd_completion, status::prompt_segment};
use crate::cli::{Cli, ShellCommands, ShellKind};

pub fn cmd_shell(cli: &Cli, command: ShellCommands) -> Result<()> {
    match command {
        ShellCommands::Init { kind } => {
            // Stdout — the caller is expected to redirect / `eval`.
            print!("{}", shell_hook_snippet(shell_hook_kind(kind)));
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

/// Map CLI clap shell kind onto the pure hook kind in heddle-core.
fn shell_hook_kind(kind: ShellKind) -> ShellHookKind {
    match kind {
        ShellKind::Zsh => ShellHookKind::Zsh,
        ShellKind::Bash => ShellHookKind::Bash,
        ShellKind::Fish => ShellHookKind::Fish,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn shell_hook_kind_maps_all_variants() {
        assert_eq!(shell_hook_kind(ShellKind::Zsh), ShellHookKind::Zsh);
        assert_eq!(shell_hook_kind(ShellKind::Bash), ShellHookKind::Bash);
        assert_eq!(shell_hook_kind(ShellKind::Fish), ShellHookKind::Fish);
    }
}
