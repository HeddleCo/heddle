// SPDX-License-Identifier: Apache-2.0
//! Semantic analysis algorithms.

mod analysis_aggregate;
mod analysis_classify;
mod analysis_functions;
mod analysis_graph;
mod analysis_imports;
mod analysis_renames;
pub mod analysis_similarity;
pub mod hot_spots;

#[cfg(test)]
mod analysis_tests;

pub use analysis_aggregate::{
    AggregateKind, AggregatedChange, AggregationResult, aggregate_changes,
};
pub use analysis_classify::{classify_modification, classify_modification_with_confidence};
pub use analysis_functions::detect_function_changes;
pub(crate) use analysis_functions::detect_function_changes_with_parsed;
pub use analysis_graph::{BlastRadius, CallGraph, CallGraphNode, FunctionKey};
pub(crate) use analysis_imports::detect_import_changes_with_parsed;
pub use analysis_imports::{detect_import_changes, detect_import_changes_with_manifest};
pub use analysis_renames::detect_file_renames;
pub use analysis_similarity::{SimilarityMethod, compute_similarity};
pub use hot_spots::{
    HotEventKind, HotSpot, HotSpotKey, HotSpotKeyValue, HotSpotParams, HotSpotsReport,
    analyze_actor_histogram, analyze_hot_spots,
};
