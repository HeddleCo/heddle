// SPDX-License-Identifier: Apache-2.0
//! Merge engine interface used by managed workflows.

pub use verbs::merge::{
    ThreadPreviewReport, ThreeWayMergeOutcome, apply_merged_tree_external,
    build_thread_preview_report, merge_thread_into_current,
    merge_thread_into_current_transactional, try_three_way_merge_between_tips,
};
