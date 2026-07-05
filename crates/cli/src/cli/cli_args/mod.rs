// SPDX-License-Identifier: Apache-2.0
//! CLI argument structures.

mod cli_base;
mod commands_advanced;
mod commands_agent;
mod commands_args;
#[cfg(feature = "git-overlay")]
mod commands_git_projection;
#[cfg(feature = "client")]
mod commands_client;
mod commands_context;
mod commands_discuss;
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
#[cfg(feature = "client")]
mod commands_spool;
mod commands_stash;
mod commands_thread;
mod commands_visibility;
mod output_mode;

pub use cli_base::Cli;
pub use cli_shared::OutputMode;
pub use commands_advanced::{
    CheckpointArgs, TransactionAbortArgs, TransactionBeginArgs, TransactionCommands,
    TransactionIdArgs,
};
pub use commands_agent::{AgentCommands, AgentFanoutCommands, AgentServeArgs, AgentTaskCommands};
pub use commands_args::{
    ActorDoneArgs, ActorExplainArgs, ActorListArgs, ActorShowArgs, ActorSpawnArgs, AdoptArgs,
    AgentApiListArgs, AgentCaptureArgs, AgentFanoutPlanArgs, AgentFanoutStartArgs,
    AgentHeartbeatArgs, AgentReadyArgs, AgentReleaseArgs, AgentReleaseStatusArg, AgentReserveArgs,
    AgentTaskCreateArgs, AgentTaskListArgs, AgentTaskShowArgs, AgentTaskStatusArg,
    AgentTaskUpdateArgs, CloneArgs, CollapseArgs, CommitArgs, DiagnoseArgs, DiffArgs, DoctorArgs,
    DoctorCommands, DoctorDocsArgs, DoctorSchemasArgs, ExpandArgs, InitArgs, LandArgs, LogArgs,
    MergeArgs, PullArgs, PushArgs, ReadyArgs, ResolveArgs, RetroArgs, RevertArgs, RunArgs,
    SessionEndArgs, SessionListArgs, SessionSegmentArgs, SessionShowArgs, SessionStartArgs,
    SnapshotArgs, SwitchArgs, SyncArgs, ThreadAbsorbArgs, ThreadApprovalsArgs, ThreadApproveArgs,
    ThreadCapturesArgs, ThreadCheckMergeArgs, ThreadDropArgs, ThreadMoveArgs, ThreadNameArgs,
    ThreadPromoteArgs, ThreadRenameArgs, ThreadResolveArgs, ThreadRevokeApprovalArgs,
    ThreadShowArgs, ThreadStartArgs, TimelineArgs, TimelineCommands, TimelineForkArgs,
    TimelineRecordFinishArgs, TimelineRecordStartArgs, TimelineRecordToolArgs, TimelineRecoverArgs,
    TimelineResetArgs, TimelineStatusArgs, TimelineTargetArgs, TryArgs, UndoArgs, WatchArgs,
    WorkspaceModeArg,
};
#[cfg(feature = "git-overlay")]
pub use commands_git_projection::{ExportCommands, GitSource, ImportCommands, SyncCommands};
#[cfg(feature = "client")]
pub use commands_client::{
    AuthCommands, SupportCommands, SupportGrantArgs, SupportListArgs, SupportRevokeArgs,
};
pub use commands_context::{ContextCommands, ContextReasonCommands};
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
    ActorCommands, Commands, DaemonCommands, FsckRepairTarget, MaintenanceCommands, SessionCommands,
};
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
#[cfg(feature = "client")]
pub use commands_spool::{
    SpoolAttachArgs, SpoolChildrenArgs, SpoolCommands, SpoolDetachArgs, SpoolHistoryArgs,
};
pub use commands_stash::StashCommands;
pub use commands_thread::{
    ThreadCleanupArgs, ThreadCommands, ThreadListArgs, ThreadMarkerCommands,
};
pub use commands_visibility::{
    VisibilityCommands, VisibilityListArgs, VisibilityPromoteArgs, VisibilitySetArgs,
    VisibilityShowArgs, VisibilityTierArg,
};
pub use output_mode::CliOutputMode;
