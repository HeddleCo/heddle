// SPDX-License-Identifier: Apache-2.0
//! CLI argument structures.

mod cli_base;
mod commands_advanced;
mod commands_agent;
mod commands_args;
#[cfg(feature = "git-overlay")]
mod commands_bridge;
mod commands_context;
mod commands_discuss;
mod commands_hook;
mod commands_integration;
mod commands_main;
mod commands_marker;
mod commands_query;
mod commands_redact;
mod commands_remote;
mod commands_review;
#[cfg(feature = "semantic")]
mod commands_semantic;
mod commands_shell;
mod commands_stack;
mod commands_stash;
mod commands_thread;
mod commands_workspace;

pub use cli_base::{Cli, OutputMode};
pub use commands_advanced::{
    CheckpointArgs, ConflictCommands, ConflictShowArgs, TransactionAbortArgs, TransactionBeginArgs,
    TransactionCommands, TransactionIdArgs,
};
pub use commands_agent::{AgentCommands, AgentServeArgs};
pub use commands_args::{
    ActorDoneArgs, ActorExplainArgs, ActorListArgs, ActorShowArgs, ActorSpawnArgs, AdoptArgs,
    AgentApiListArgs, AgentCaptureArgs, AgentHeartbeatArgs, AgentReadyArgs, AgentReleaseArgs,
    AgentReleaseStatusArg, AgentReserveArgs, AttemptArgs, BranchArgs, CloneArgs, CollapseArgs,
    CommandCatalogArgs, CommandCatalogTier, CommitArgs, DelegateArgs, DelegatedTaskSpec,
    DiagnoseArgs, DiffArgs, DoctorArgs, DoctorCommands, DoctorDocsArgs, InitArgs, LogArgs,
    MergeArgs, PullArgs, PushArgs, ReadyArgs, RedoArgs, ResolveArgs, RetroArgs, RevertArgs, RunArgs,
    SessionEndArgs, SessionListArgs, SessionSegmentArgs, SessionShowArgs, SessionStartArgs,
    LandArgs, SnapshotArgs, SwitchArgs, SyncArgs, ThreadAbsorbArgs, ThreadApprovalsArgs,
    ThreadApproveArgs, ThreadCapturesArgs, ThreadCheckMergeArgs, ThreadDropArgs, ThreadMoveArgs,
    ThreadNameArgs, ThreadPromoteArgs, ThreadRenameArgs, ThreadResolveArgs,
    ThreadRevokeApprovalArgs, ThreadShowArgs, ThreadStartArgs, TryArgs, UndoArgs, WatchArgs,
    WorkspaceModeArg,
};
#[cfg(feature = "git-overlay")]
pub use commands_bridge::{BridgeCommands, GitCommands, GitSource};
pub use commands_context::ContextCommands;
pub use commands_discuss::{
    DiscussAppendArgs, DiscussCommands, DiscussListArgs, DiscussOpenArgs, DiscussResolveArgs,
    DiscussShowArgs, ResolveModeArg,
};
pub use commands_hook::{HookCommands, HookInstallSource};
pub use commands_integration::{
    IntegrationCommands, IntegrationInstallArgs, IntegrationRelayArgs, IntegrationTargetArgs,
};
#[cfg(feature = "client")]
pub use commands_main::PresenceCommands;
pub use commands_main::{
    ActorCommands, Commands, DaemonCommands, MaintenanceCommands, SessionCommands,
};
pub use commands_marker::MarkerCommands;
pub use commands_query::QueryArgs;
pub use commands_redact::{
    PurgeApplyArgs, PurgeCommands, PurgeListArgs, RedactApplyArgs, RedactCommands, RedactListArgs,
    RedactShowArgs, RedactTrustAddArgs, RedactTrustCommands, RedactTrustListArgs,
    RedactTrustRemoveArgs,
};
pub use commands_remote::RemoteCommands;
pub use commands_review::{
    ReviewCommands, ReviewHealthArgs, ReviewNextArgs, ReviewShowArgs, ReviewSignArgs, SignKindArg,
};
#[cfg(feature = "semantic")]
pub use commands_semantic::{HotEventKindArg, HotSpotKeyArg, SemanticCommands};
pub use commands_shell::{ShellCommands, ShellKind};
pub use commands_stack::{StackArgs, StackCommands};
pub use commands_stash::StashCommands;
pub use commands_thread::{ThreadCleanupArgs, ThreadCommands, ThreadListArgs};
pub use commands_workspace::{WorkspaceCommands, WorkspaceShowArgs};
#[cfg(feature = "client")]
pub use heddle_client::AuthCommands;
#[cfg(feature = "client")]
pub use heddle_client::{SupportCommands, SupportGrantArgs, SupportListArgs, SupportRevokeArgs};
