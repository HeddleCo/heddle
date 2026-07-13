// SPDX-License-Identifier: Apache-2.0
//! Merge engine interface used by managed workflows.

pub use heddle_core::merge::{
    ThreadPreviewReport, ThreeWayMergeOutcome, apply_merged_tree_external, bench_detect_renames,
    bench_find_merge_base, bench_three_way_merge, build_thread_preview_report,
    merge_thread_into_current, try_three_way_merge_between_tips,
};
