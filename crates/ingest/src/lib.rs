// SPDX-License-Identifier: Apache-2.0
//! Import git history into a native Heddle repository.
//!
//! ## Scope
//!
//! `heddle-ingest` reads a git repository — including its reflog, not just
//! reachable commit graph — and produces a Heddle repository where:
//!
//! - each git commit becomes one Heddle [`State`](objects::object::State)
//! - each ref becomes a Heddle thread (branch) or marker (tag)
//! - each reflog entry becomes a Heddle oplog `OpRecord` (honest history)
//! - `Co-Authored-By: <agent>` trailers map to agent [`Attribution`](objects::object::Attribution)
//! - agent session transcripts (Claude / Codex) can be mined for
//!   [`ReasoningPoint`]s and attached as annotations
//!
//! ## Layering
//!
//! Modules are split so the mechanical half (git walking, object translation,
//! ref emission) compiles and tests without requiring an LLM or network:
//!
//! | Module            | Responsibility                                       |
//! |-------------------|-------------------------------------------------------|
//! | [`reasoning`]     | [`ReasoningPoint`] schema — shared with the TS extractor |
//! | [`sha_map`]       | `git_sha ↔ heddle ChangeId` persistent sidecar          |
//! | [`git_walk`]      | reflog+refs walker → ordered commit stream            |
//! | [`importer`]      | git tree/blob/commit → Heddle objects and refs         |
//! | [`state_writer`]  | git commit metadata → Heddle State fields              |
//! | [`thread_writer`] | refs → Heddle threads/markers                           |
//! | [`oplog_emit`]    | reflog entries → Heddle oplog OpRecords                 |
//!
//! The flagship entry point is [`Importer::run`].

#![allow(dead_code)] // module is under active construction

pub mod git_walk;
pub mod import_options;
pub mod importer;
pub mod oplog_emit;
pub mod reasoning;
pub mod reasoning_emit;
pub mod reasoning_extract;
pub mod reasoning_pipeline;
pub mod ref_emit;
pub mod semantic_cache;
pub mod sha_map;
pub mod state_writer;
pub mod transcript;

pub use git_walk::{CommitEntry, GitSource, RefHead, RefNamespace, ReflogEntry};
pub use import_options::{ImportOptions, LossyImportAction, LossyImportEntry};
pub use importer::{
    ImportProgressEvent, ImportScope, ImportStats, Importer, import_git_into,
    import_git_into_scoped_with_options, import_git_into_scoped_with_options_and_progress,
    import_git_into_with_options, import_git_into_with_options_and_progress,
};
pub use oplog_emit::{OplogEmitStats, OplogEmitter};
pub use reasoning::{ReasoningEvidence, ReasoningPoint, ReasoningTarget};
pub use reasoning_emit::{ReasoningEmitStats, ReasoningEmitter};
pub use reasoning_extract::{
    HarvestParams, HarvestedCandidate, KeepParams, LlmRefiner, extract as extract_reasoning_points,
    harvest as harvest_reasoning_candidates, keep as keep_reasoning_candidate,
};
pub use reasoning_pipeline::{
    PreviewDecision, ReasoningPipeline, ReasoningPipelineParams, ReasoningPipelineStats,
    ReasoningPreview, pipeline_default_commits,
};
pub use ref_emit::{RefEmitStats, RefEmitter};
pub use semantic_cache::{IngestSemanticCache, IngestSemanticCacheStats};
pub use sha_map::{ShaMap, ShaMapError};
pub use state_writer::parse_attribution;
pub use transcript::{
    FileTouch, Match, MatchParams, Provider, TouchKind, Transcript, TranscriptMatcher,
    TranscriptRoots, load_all as load_transcripts,
};

/// Errors that can bubble up from any ingest stage.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("git: {0}")]
    Git(String),
    #[error("heddle: {0}")]
    Heddle(#[from] objects::error::HeddleError),
    #[error(
        "Heddle thread '{thread}' and Git ref '{branch}' diverged: thread {existing}, branch {incoming}"
    )]
    ThreadDiverged {
        thread: String,
        branch: String,
        existing: objects::object::ChangeId,
        incoming: objects::object::ChangeId,
    },
    #[error("sha map: {0}")]
    ShaMap(#[from] sha_map::ShaMapError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = IngestError> = std::result::Result<T, E>;
