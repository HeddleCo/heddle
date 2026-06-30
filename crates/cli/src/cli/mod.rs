// SPDX-License-Identifier: Apache-2.0
//! Command-line interface for Heddle.

use std::{io::IsTerminal, sync::OnceLock};

pub mod cli_args;
pub mod commands;
pub mod help;
pub mod render;
pub mod style;
pub mod tips;
pub mod transaction_sentinel;

#[cfg(feature = "client")]
pub use cli_args::PresenceCommands;
pub use cli_args::{
    ActorCommands, AdoptArgs, AgentCommands, CheckpointArgs, Cli, CloneArgs, CollapseArgs,
    Commands, CommitArgs, CompletionSubject, ContextCommands, DaemonCommands, DiagnoseArgs,
    DiffArgs, DoctorArgs, DoctorCommands, DoctorDocsArgs, DoctorSchemasArgs, ExpandArgs,
    HookCommands, HookInstallSource, InitArgs, IntegrationCommands, IntegrationInstallArgs,
    IntegrationRelayArgs, IntegrationTargetArgs, LogArgs, MaintenanceCommands, MergeArgs,
    OplogCommands, OutputMode, PullArgs, PurgeApplyArgs, PurgeCommands, PurgeListArgs, PushArgs,
    ReadyArgs, RedactApplyArgs, RedactCommands, RedactListArgs, RedactShowArgs, RedactTrustAddArgs,
    RedactTrustCommands, RedactTrustListArgs, RedactTrustRemoveArgs, RemoteCommands, ResolveArgs,
    RetroArgs, RevertArgs, RunArgs, SessionCommands, SessionEndArgs, SessionListArgs,
    SessionSegmentArgs, SessionShowArgs, SessionStartArgs, ShellCommands, ShellKind, SnapshotArgs,
    StashCommands, SwitchArgs, ThreadAbsorbArgs, ThreadCleanupArgs, ThreadCommands, ThreadDropArgs,
    ThreadListArgs, ThreadMarkerCommands, ThreadMoveArgs, ThreadNameArgs, ThreadPromoteArgs,
    ThreadRenameArgs, ThreadResolveArgs, ThreadShowArgs, ThreadStartArgs, TimelineArgs,
    TimelineCommands, TimelineForkArgs, TimelineRecoverArgs, TimelineResetArgs, TimelineTargetArgs,
    TryArgs, UndoArgs, VisibilityCommands, VisibilityListArgs, VisibilityPromoteArgs,
    VisibilitySetArgs, VisibilityShowArgs, VisibilityTierArg, WatchArgs, WorkspaceModeArg,
};
#[cfg(feature = "client")]
pub use cli_args::{AuthCommands, SupportCommands};
#[cfg(feature = "git-overlay")]
pub use cli_args::{BridgeCommands, BridgeGitReconcileArgs, GitCommands};
#[cfg(feature = "semantic")]
pub use cli_args::{HotEventKindArg, HotSpotKeyArg, SemanticCommands};
use repo::{Config, OutputFormat};

use crate::config::UserConfig;

/// Check if stdout is a TTY.
pub fn is_tty() -> bool {
    std::io::stdout().is_terminal()
}

pub fn execution_context_from_cli(cli: &Cli) -> anyhow::Result<heddle_core::ExecutionContext> {
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
    static USER_CONFIG: OnceLock<UserConfig> = OnceLock::new();
    USER_CONFIG.get_or_init(|| {
        // Failure here MUST NOT short-circuit with a raw `eprintln` +
        // exit(2) — that path bypassed the typed `Next:` envelope when
        // the global user config carried `output.format = "auto"`
        // (Codex R2 on #271). The early-load in `main` already routes
        // that failure through `print_error_with_hint`; this fallback
        // exists only so re-entrant callers (e.g. `should_output_json`
        // invoked from inside the error printer itself) get a usable
        // default instead of a recursive load failure.
        UserConfig::load_default().unwrap_or_default()
    })
}

pub fn load_user_config_or_exit() -> UserConfig {
    user_config_or_exit().clone()
}

/// Determine if output should be JSON.
///
/// Resolution order (later wins):
/// 1. user config `output.format` (default: `text`)
/// 2. repo config `output.format` (falls back to user config)
/// 3. `--output {json|text}` CLI flag
///
/// No TTY/pipe auto-detection: the default is always text, and JSON
/// is opt-in. Surprises like `heddle status | less` rendering JSON
/// are gone.
pub fn should_output_json(cli: &Cli, config: Option<&Config>) -> bool {
    let user_config = user_config_or_exit();
    let mut format = config
        .and_then(|cfg| cfg.output.format)
        .unwrap_or(user_config.output.format);

    if let Some(output) = cli.output_mode() {
        format = match output {
            // `json-compact` is still JSON output — it only narrows
            // *which* fields are emitted (see `output_is_compact`).
            OutputMode::Json | OutputMode::JsonCompact => OutputFormat::Json,
            OutputMode::Text => OutputFormat::Text,
        };
    }

    matches!(format, OutputFormat::Json)
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

impl weft_client_shim::CliContext for Cli {
    fn repo_path(&self) -> Option<&std::path::Path> {
        self.repo.as_deref()
    }

    fn operation_id_wire(&self) -> String {
        crate::operation_id::wire(self)
    }

    fn should_output_json(&self, repo_config: Option<&Config>) -> bool {
        should_output_json(self, repo_config)
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
