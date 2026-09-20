// SPDX-License-Identifier: Apache-2.0
//! CLI argument structures.

mod cli_base;
mod command_suggestions;
mod commands_agent;
mod commands_args;
#[cfg(feature = "ci")]
mod commands_ci;
#[cfg(feature = "client")]
mod commands_client;
mod commands_context;
mod commands_discuss;
mod commands_env;
#[cfg(feature = "git-overlay")]
mod commands_git_projection;
mod commands_hook;
mod commands_import;
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
mod shared;

pub use cli_base::{Cli, CliContext, should_output_json};
pub use command_suggestions::{format_unrecognized_suggestion, suggested_command};
pub use commands_agent::{
    AgentCommands, AgentFanoutCommands, AgentProvenanceCommands, AgentTaskCommands,
    PresenceCommands,
};
pub use commands_args::{
    AgentApiListArgs, AgentCaptureArgs, AgentFanoutPlanArgs, AgentFanoutStartArgs,
    AgentHeartbeatArgs, AgentPresenceCompleteArgs, AgentPresenceExplainArgs, AgentPresenceListArgs,
    AgentPresenceShowArgs, AgentProvenanceBeginArgs, AgentProvenanceEndArgs,
    AgentProvenanceListArgs, AgentProvenanceSegmentArgs, AgentProvenanceShowArgs, AgentReadyArgs,
    AgentReleaseArgs, AgentReleaseStatusArg, AgentReserveArgs, AgentTaskCreateArgs,
    AgentTaskListArgs, AgentTaskShowArgs, AgentTaskStatusArg, AgentTaskUpdateArgs, CloneArgs,
    CollapseArgs, DiffArgs, DiffBaseArg, DoctorArgs, DoctorCommands, DoctorDocsArgs, ExpandArgs,
    IMPORT_VERB, INIT_VERB, ImportLocalArgs, InitArgs, LandArgs, LogArgs, PullArgs, PushArgs,
    ReadyArgs, ResolveArgs, RevertArgs, SnapshotArgs, SyncArgs, ThreadAbsorbArgs,
    ThreadCapturesArgs, ThreadCheckoutArgs, ThreadDropArgs, ThreadMoveArgs, ThreadNameArgs,
    ThreadRenameArgs, ThreadResolveArgs, ThreadShowArgs, ThreadStartArgs, TimelineCommands,
    TimelineForkArgs, TimelineRecordFinishArgs, TimelineRecordStartArgs, TimelineRecordToolArgs,
    TimelineRecoverArgs, TimelineResetArgs, TimelineStatusArgs, TimelineTargetArgs, UndoArgs,
    WatchArgs, WorkspaceModeArg,
};
#[cfg(feature = "ci")]
pub use commands_ci::{CiCommands, CiRunArgs};
#[cfg(feature = "client")]
pub use commands_client::{
    AgentTemplateArg, AuthCommands, AuthInviteCommands, AuthTrustCommands, AuthTrustReplaceArgs,
    AuthTrustShowArgs, ClaimArgs, DEFAULT_CLAIM_WEB_ORIGIN, GrantCommands, GrantCreateArgs,
    GrantDeleteArgs, GrantListArgs, GrantRoleArg, PromoteArgs,
};
#[cfg(all(feature = "git-overlay", feature = "ingest"))]
pub use commands_context::ContextReasonCommands;
pub use commands_context::{
    ContextCommands, ContextGetArgs, ContextRmArgs, ContextSetArgs, ContextSupersedeArgs,
};
pub use commands_discuss::{
    DiscussArgs, DiscussCommands, DiscussListArgs, DiscussNewArgs, DiscussReopenArgs,
    DiscussReplyArgs, DiscussResolveArgs, DiscussShowArgs, DiscussWaitArgs, ResolveModeArg,
};
pub use commands_env::{EnvCommands, EnvCreateArgs, EnvListArgs, EnvRunArgs};
#[cfg(feature = "git-overlay")]
pub use commands_git_projection::{BridgeCommands, BridgeGitCommands, GitSource, SyncCommands};
pub use commands_hook::{HookCommands, HookInstallSource};
pub use commands_import::{ImportArgs, ImportCommands};
#[cfg(feature = "client")]
pub use commands_import::{ImportOperationArgs, ImportUrlArgs};
pub use commands_integration::{
    IntegrationCommands, IntegrationInstallArgs, IntegrationRelayArgs, IntegrationStampArgs,
    IntegrationTargetArgs,
};
pub use commands_main::{
    Commands, DaemonCommands, FsckArgs, FsckCommands, FsckRepairCommands, FsckRepairGitArgs,
    MaintenanceCommands, NetdCommands,
};
pub use commands_oplog::OplogCommands;
pub use commands_query::{BlameArgs, QueryArgs};
pub use commands_redact::{
    PurgeApplyArgs, PurgeCommands, PurgeListArgs, RedactApplyArgs, RedactCommands, RedactListArgs,
    RedactShowArgs,
};
pub use commands_remote::RemoteCommands;
pub use commands_review::{
    ReviewApproveArgs, ReviewCommands, ReviewHealthArgs, ReviewListArgs, ReviewNextArgs,
    ReviewReadinessArgs, ReviewRevokeArgs, ReviewShowArgs, ReviewSignArgs, SignKindArg,
};
#[cfg(feature = "semantic")]
pub use commands_semantic::{HotEventKindArg, HotSpotKeyArg, SemanticCommands};
pub use commands_shell::{CompletionSubject, ShellCommands, ShellKind};
pub use commands_thread::{
    ThreadCleanupArgs, ThreadCommands, ThreadListArgs, ThreadMarkerCommands,
    ThreadOwnershipCommands,
};
pub use commands_visibility::{
    VisibilityCommands, VisibilityListArgs, VisibilityPromoteArgs, VisibilitySetArgs,
    VisibilityShowArgs, VisibilityTierArg,
};
pub use config::OutputMode;
pub use output_mode::CliOutputMode;
pub use shared::{
    AuthoredMessageArgs, CloneSourceArg, CodeScopeArgs, DryRunArgs, HistoricalRevisionArgs,
    HostedServerArgs, RemoteChoiceArgs, safe_clone_destination_basename, split_path_and_revision,
};
