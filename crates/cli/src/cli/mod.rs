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
    ActorCommands, AgentCommands, AttemptArgs, BisectCommands, BranchArgs, CheckpointArgs, Cli,
    CloneArgs, CollapseArgs, Commands, CommitArgs, ContextCommands, DaemonCommands, DiagnoseArgs,
    DiffArgs, DoctorArgs, DoctorCommands, DoctorDocsArgs, HookCommands, HookInstallSource,
    InitArgs, IntegrationCommands, IntegrationInstallArgs, IntegrationRelayArgs,
    IntegrationTargetArgs, LogArgs, MaintenanceCommands, MarkerCommands, MergeArgs, OutputMode,
    PullArgs, PurgeApplyArgs, PurgeCommands, PurgeListArgs, PushArgs, ReadyArgs, RedactApplyArgs,
    RedactCommands, RedactListArgs, RedactShowArgs, RedactTrustAddArgs, RedactTrustCommands,
    RedactTrustListArgs, RedactTrustRemoveArgs, RemoteCommands, ResolveArgs, RetroArgs, RevertArgs,
    RunArgs, SessionCommands, SessionEndArgs, SessionListArgs, SessionSegmentArgs, SessionShowArgs,
    SessionStartArgs, ShellCommands, ShellKind, SnapshotArgs, StashCommands, StoreCommands,
    SwitchArgs, ThreadAbsorbArgs, ThreadCleanupArgs, ThreadCommands, ThreadDropArgs,
    ThreadListArgs, ThreadMoveArgs, ThreadNameArgs, ThreadPromoteArgs, ThreadRenameArgs,
    ThreadResolveArgs, ThreadShowArgs, ThreadStartArgs, TryArgs, UndoArgs, WatchArgs,
    WorkspaceCommands, WorkspaceModeArg, WorkspaceShowArgs,
};
#[cfg(feature = "client")]
pub use cli_args::{AuthCommands, SupportCommands};
#[cfg(feature = "git-overlay")]
pub use cli_args::{BridgeCommands, GitCommands};
#[cfg(feature = "semantic")]
pub use cli_args::{HotEventKindArg, HotSpotKeyArg, SemanticCommands};
use repo::{Config, OutputFormat};

use crate::config::UserConfig;

/// Check if stdout is a TTY.
pub fn is_tty() -> bool {
    std::io::stdout().is_terminal()
}

pub fn user_config_or_exit() -> &'static UserConfig {
    static USER_CONFIG: OnceLock<UserConfig> = OnceLock::new();
    USER_CONFIG.get_or_init(|| {
        UserConfig::load_default().unwrap_or_else(|err| {
            eprintln!("failed to load Heddle user config: {err}");
            std::process::exit(2);
        })
    })
}

pub fn load_user_config_or_exit() -> UserConfig {
    user_config_or_exit().clone()
}

/// Determine if output should be JSON.
///
/// Resolution order (later wins):
/// 1. user config `output.format` (or `auto` if unset)
/// 2. repo config `output.format` (falls back to user config)
/// 3. `--output {auto|json|text}` CLI flag
/// 4. `--json` CLI flag (deprecated; routes to JSON without warning so
///    machine-mode stderr stays parseable)
///
/// `auto` resolves by stream type: text on a TTY, JSON when piped.
pub fn should_output_json(cli: &Cli, config: Option<&Config>) -> bool {
    let user_config = user_config_or_exit();
    let mut format = Some(user_config)
        .map(|cfg| cfg.output.format)
        .or_else(|| config.map(|cfg| cfg.output.format))
        .unwrap_or(OutputFormat::Auto);

    if let Some(output) = cli.output {
        format = match output {
            OutputMode::Auto => OutputFormat::Auto,
            OutputMode::Json => OutputFormat::Json,
            OutputMode::Text => OutputFormat::Text,
        };
    }

    if cli.json {
        format = OutputFormat::Json;
    }

    match format {
        OutputFormat::Json => true,
        OutputFormat::Text => false,
        OutputFormat::Auto => !is_tty(),
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
