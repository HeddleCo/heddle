// SPDX-License-Identifier: Apache-2.0
//! Real `--output json` wire payloads for the CLI verbs.
//!
//! These are the serialization structs the commands emit, not schema
//! mirrors: `Serialize` and `JsonSchema` derive from the same definition,
//! so a skip-serialized field cannot reappear on the published schema.
//! `crates/cli` re-exports each payload at its historical path, and
//! [`crate::cli::commands::schemas`] registers these types directly
//! (InitOutput precedent).
//!
//! The `#[schemars(rename)]` attributes keep the published `$defs` titles
//! stable while the Rust types carry their natural names. Types whose
//! serialization is hand-written (`OperatorCommandOutput`, `ReadyOutput`)
//! pair their `Serialize` impl with a manual `JsonSchema` built from a
//! private shape struct in the same file, so the schema and the serializer
//! stay one maintenance unit.

pub mod agent;
pub mod auth;
pub mod bridge;
pub mod collab;
pub mod core_loop;
pub mod history;
pub mod land;
pub mod operator;
pub mod ready;
pub mod remote;
pub mod thread;

pub use agent::{
    ActorDoneOutput, ActorEnvironmentOutput, ActorExplainDetectedOutput, ActorListOutput,
    ActorSingleOutput, AgentFanoutCommandOutput, AgentFanoutLaneOutput, AgentFanoutOutput,
    AgentReservationEnvelope, AgentReservationListOutput, AgentReservationOutput,
    AgentTaskEnvelope, AgentTaskListOutput, AgentTaskOutput, DetectedActorOutput, SegmentEnvelope,
    SegmentOutput, SessionEnvelope, SessionListOutput, SessionOutput,
};
pub use auth::{
    AgentAccountCreatedOutput, AuthLogoutOutput, AuthStatusOutput, AuthTrustOutput, CaptureActor,
    HumanPromotionDirective, ServiceTokenOutput, WhoamiIdentity, WhoamiOutput, WhoamiRole,
};
pub use bridge::{
    ExportGitOutput, ExportedRefOutput, ImportGitOutput, IntegrationStatusOutput,
    LossyImportEntryOutput, RepackOutput, SyncGitOutput,
};
pub use collab::{
    AnchorOutput, DiscussionListOutput, DiscussionOutput, DiscussionShowOutput, DiscussionView,
    DiscussionWriteOutput, HealthEntry, NextStateView, RequiredNullableNextState, ResolutionOutput,
    ReviewHealthOutput, ReviewNextOutput, ReviewShowOutput, ReviewSignOutput, SignalView,
    SignatureView, TurnOutput, WatchActorInfo, WatchLineOutput,
};
pub use core_loop::{
    CommitOutput, SnapshotAgentOutput, SnapshotOutput, SnapshotPrincipalOutput, UndoRedoOutput,
};
pub use history::{
    BlameLine, BlameOrigin, BlameOutput, CollapsedLandOutput, ContextSnippet, ExpandOutput,
    ExpandedCaptureOutput, LogImportGuidanceOutput, LogOutput, MarkerBulkDeleteOutput, MarkerEntry,
    MarkerListOutput, MarkerOpOutput, PrincipalInfo, ReflogEntry, ReflogOutput, RevertOutput,
    ShowAgentInfo, ShowImportGuidanceOutput, ShowOutput, ShowPrincipalInfo, ShowVerificationInfo,
    StateEntry, TimelineActionOutput, TimelineLogOutput, TimelineRecordingOutput,
    TimelineStatusOutput,
};
pub use land::{
    LandBlockerCheck, LandBlockerCode, LandBlockerDetail, LandBlockerStateContext, LandOutput,
    MultiLandOutput, MultiLandPeerResult, SiblingRestackFailure, SyncOutput,
};
pub use operator::{
    OperatorAction, OperatorCommandEnvelope, OperatorCommandOutput, VerificationClaimPolicy,
};
pub use ready::{
    ReadyChecksSummary, ReadyOutput, ReadyReadinessSummary, ready_blocked_by_missing_intent,
};
pub use remote::{AdoptOutput, CloneOutput, PullOutput, PushOutput, RemoteMutationOutput};
pub use thread::{
    ApprovalOutput, ApprovalRevokeOutput, DroppedThread, EligibilityOutput, FskitReadinessReport,
    SkippedThread, ThreadAbsorbOutput, ThreadCaptureOutput, ThreadCaptureSummary,
    ThreadCleanupOutput, ThreadCurrentOutput, ThreadListImportGuidanceOutput, ThreadListOutput,
    ThreadOpOutput, ThreadRecordOutput, ThreadResolveOutput, ThreadShowOutput, UnmetOutput,
};
