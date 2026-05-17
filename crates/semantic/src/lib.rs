// SPDX-License-Identifier: Apache-2.0
//! Semantic analysis and parser-heavy diff support.

pub mod analysis;
pub mod cache;
pub mod diff;
pub mod merge;
pub mod parser;
mod symbol_extraction;
pub mod symbol_resolver;

pub use analysis::{
    AggregateKind, AggregatedChange, AggregationResult, BlastRadius, CallGraph, CallGraphNode,
    FunctionKey, HotEventKind, HotSpot, HotSpotKey, HotSpotKeyValue, HotSpotParams, HotSpotsReport,
    SimilarityMethod, aggregate_changes, analyze_actor_histogram, analyze_hot_spots,
    classify_modification, classify_modification_with_confidence, compute_similarity,
    detect_file_renames, detect_function_changes,
};
pub use cache::{SemanticParseCache, SemanticParseCacheStats};
pub use diff::{
    DiffKind, SemanticBudget, SemanticCheckOnlyResult, SemanticCheckStatus, SemanticDiffOptions,
    SemanticDiffResult, SemanticFallbackReason, SemanticSummaryResult, WorktreeStatus,
    semantic_check_only, semantic_check_only_worktree, semantic_diff, semantic_diff_summary,
    semantic_diff_summary_worktree, semantic_diff_worktree,
};
pub use merge::{ConflictMarkers, MergeOutcome, text_hunk_merge, text_hunk_merge_with_markers};
pub use parser::{Language, ParsedFile};