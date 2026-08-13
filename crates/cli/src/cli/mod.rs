// SPDX-License-Identifier: Apache-2.0
//! Command-line interface for Heddle.

use std::io::IsTerminal;

pub mod commands;

pub use heddle_cli_args as cli_args;
pub use heddle_cli_args::*;
pub use heddle_cli_contract::cli::help;
pub use heddle_cli_render::cli::{progress_render, render, style, tips};
use repo::Config;

use crate::config::UserConfig;

/// Check if stdout is a TTY.
pub fn is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// Check whether the process has a real interactive terminal on every stream
/// needed by a selection prompt.
pub fn is_interactive_tty() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}

pub fn execution_context_from_cli(cli: &Cli) -> anyhow::Result<heddle_core::ExecutionContext> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd).to_path_buf();
    let repo = cli.open_repo()?;
    let config = UserConfig::load_default()?;
    let verbosity = if cli.quiet {
        heddle_core::Verbosity::Quiet
    } else if cli.verbose > 0 {
        heddle_core::Verbosity::Verbose
    } else {
        heddle_core::Verbosity::Normal
    };
    let mut builder = heddle_core::ExecutionContext::builder()
        .repo(repo)
        .start_path(start)
        .config(config)
        .verbosity(verbosity)
        .progress(std::sync::Arc::new(heddle_core::NoopProgress))
        .warnings(std::sync::Arc::new(heddle_core::NoopWarnings));

    if let Some(op_id) = crate::operation_id::resolve_operation_id(cli)? {
        builder = builder.op_id(op_id.to_string());
    }

    Ok(builder.build())
}

pub fn user_config_or_exit() -> &'static UserConfig {
    // Failure here MUST NOT short-circuit with a raw `eprintln` +
    // exit(2) — that path bypassed the typed `Next:` envelope when
    // the global user config carried `output.format = "auto"`
    // (Codex R2 on #271). The early-load in `main` already routes
    // that failure through `print_error_with_hint`; this fallback
    // exists only so re-entrant callers (e.g. `should_output_json`
    // invoked from inside the error printer itself) get a usable
    // default instead of a recursive load failure.
    Cli::user_config_or_exit()
}

pub fn load_user_config_or_exit() -> UserConfig {
    user_config_or_exit().clone()
}

/// Whether the caller asked for the compact decision-surface projection
/// (`--output json-compact`, heddle#470). Compact is a CLI-only modifier
/// on top of JSON output — it is never reachable from config
/// (`output.format` is `json`/`text` only), so the full machine contract
/// stays the default for piped/configured JSON.
pub fn output_is_compact(cli: &Cli) -> bool {
    matches!(cli.output_mode(), Some(OutputMode::JsonCompact))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonOutputMode {
    Text,
    Json,
    Jsonl,
}

/// Resolve the runtime JSON mode promised by a command contract's
/// `json_kind`.
///
/// JSONL commands are stream-shaped and opt into machine output only
/// when explicitly requested. That prevents commands like `watch` from
/// silently changing format when piped through human tools.
pub fn json_output_mode_for_kind(
    cli: &Cli,
    config: Option<&Config>,
    json_kind: &str,
) -> JsonOutputMode {
    match json_kind {
        "jsonl" => {
            // Stream-shaped commands (e.g. `watch`) have no compact
            // projection; `json-compact` falls back to the full jsonl
            // stream rather than silently downgrading to text.
            if matches!(
                cli.output_mode(),
                Some(OutputMode::Json | OutputMode::JsonCompact)
            ) {
                JsonOutputMode::Jsonl
            } else {
                JsonOutputMode::Text
            }
        }
        "json" | "json_or_jsonl" => {
            if should_output_json(cli, config) {
                JsonOutputMode::Json
            } else {
                JsonOutputMode::Text
            }
        }
        "none" => JsonOutputMode::Text,
        _ => JsonOutputMode::Text,
    }
}

/// Resolve worktree status options from user, repo, and env config.
pub fn worktree_status_options(config: Option<&Config>) -> repo::WorktreeStatusOptions {
    user_config_or_exit().worktree_status_options(config)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn jsonl_commands_require_explicit_json_output() {
        let auto = Cli::try_parse_from(["heddle", "watch"]).expect("watch should parse");
        assert_eq!(
            json_output_mode_for_kind(&auto, None, "jsonl"),
            JsonOutputMode::Text
        );

        let json = Cli::try_parse_from(["heddle", "--output", "json", "watch"])
            .expect("watch --output json should parse");
        assert_eq!(
            json_output_mode_for_kind(&json, None, "jsonl"),
            JsonOutputMode::Jsonl
        );

        let text = Cli::try_parse_from(["heddle", "--output", "text", "watch"])
            .expect("watch --output text should parse");
        assert_eq!(
            json_output_mode_for_kind(&text, None, "jsonl"),
            JsonOutputMode::Text
        );
    }
}
