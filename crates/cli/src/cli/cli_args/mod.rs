// SPDX-License-Identifier: Apache-2.0
//! CLI argument structures.

mod cli_base;
mod commands_advanced;
mod commands_agent;
mod commands_args;
#[cfg(feature = "client")]
mod commands_client;
mod commands_context;
mod commands_discuss;
#[cfg(feature = "git-overlay")]
mod commands_git_projection;
mod commands_hook;
mod commands_integration;
mod commands_main;
mod commands_oplog;
mod commands_query;
mod commands_redact;
mod commands_remote;
mod commands_review;
#[cfg(feature = "semantic")]
mod commands_semantic;
mod commands_shell;
mod commands_thread;
mod commands_visibility;
mod output_mode;

pub use cli_base::Cli;
pub use cli_shared::OutputMode;
pub use commands_advanced::{
    TransactionAbortArgs, TransactionBeginArgs, TransactionCommands, TransactionIdArgs,
};
pub use commands_agent::{
    AgentCommands, AgentFanoutCommands, AgentPresenceCommands, AgentProvenanceCommands,
    AgentServeArgs, AgentTaskCommands,
};
pub use commands_args::{
    AdoptArgs, AgentApiListArgs, AgentCaptureArgs, AgentFanoutPlanArgs, AgentFanoutStartArgs,
    AgentHeartbeatArgs, AgentPresenceCompleteArgs, AgentPresenceExplainArgs, AgentPresenceListArgs,
    AgentPresenceShowArgs, AgentProvenanceBeginArgs, AgentProvenanceEndArgs,
    AgentProvenanceListArgs, AgentProvenanceSegmentArgs, AgentProvenanceShowArgs, AgentReadyArgs,
    AgentReleaseArgs, AgentReleaseStatusArg, AgentReserveArgs, AgentTaskCreateArgs,
    AgentTaskListArgs, AgentTaskShowArgs, AgentTaskStatusArg, AgentTaskUpdateArgs, CloneArgs,
    CollapseArgs, DiagnoseArgs, DiffArgs, DoctorArgs, DoctorCommands, DoctorDocsArgs,
    DoctorSchemasArgs, ExpandArgs, InitArgs, LandArgs, LogArgs, PullArgs, PushArgs, ReadyArgs,
    ResolveArgs, RetroArgs, RevertArgs, RunArgs, SnapshotArgs, SyncArgs, ThreadAbsorbArgs,
    ThreadApprovalsArgs, ThreadApproveArgs, ThreadCapturesArgs, ThreadCheckMergeArgs,
    ThreadDropArgs, ThreadMoveArgs, ThreadNameArgs, ThreadPromoteArgs, ThreadRenameArgs,
    ThreadResolveArgs, ThreadRevokeApprovalArgs, ThreadShowArgs, ThreadStartArgs, TimelineArgs,
    TimelineCommands, TimelineForkArgs, TimelineRecordFinishArgs, TimelineRecordStartArgs,
    TimelineRecordToolArgs, TimelineRecoverArgs, TimelineResetArgs, TimelineStatusArgs,
    TimelineTargetArgs, TryArgs, UndoArgs, WatchArgs, WorkspaceModeArg,
};
#[cfg(feature = "client")]
pub use commands_client::AuthCommands;
pub use commands_context::ContextCommands;
#[cfg(all(feature = "git-overlay", feature = "ingest"))]
pub use commands_context::ContextReasonCommands;
pub use commands_discuss::{
    DiscussAppendArgs, DiscussCommands, DiscussListArgs, DiscussOpenArgs, DiscussResolveArgs,
    DiscussReopenArgs, DiscussShowArgs, ResolveModeArg,
};
#[cfg(feature = "git-overlay")]
pub use commands_git_projection::{ExportCommands, GitSource, ImportCommands, SyncCommands};
pub use commands_hook::{HookCommands, HookInstallSource};
pub use commands_integration::{
    IntegrationCommands, IntegrationInstallArgs, IntegrationRelayArgs, IntegrationTargetArgs,
};
pub use commands_main::{Commands, DaemonCommands, FsckRepairTarget, MaintenanceCommands};
pub use commands_oplog::OplogCommands;
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
pub use commands_shell::{CompletionSubject, ShellCommands, ShellKind};
pub use commands_thread::{
    ThreadCleanupArgs, ThreadCommands, ThreadListArgs, ThreadMarkerCommands,
};
pub use commands_visibility::{
    VisibilityCommands, VisibilityListArgs, VisibilityPromoteArgs, VisibilitySetArgs,
    VisibilityShowArgs, VisibilityTierArg,
};
pub use output_mode::CliOutputMode;
