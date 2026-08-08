// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;

use super::measurement::{Counters, Sample};

pub(super) fn sample_from_trace(wall_ms: f64, trace: &Value) -> Sample {
    let total_ms = total_metric(trace, "total_ms");
    let command_body_ms = total_metric(trace, "command_body_ms");
    let status_repo = phase_metric(trace, "status repo open", "repo_open_ms")
        + phase_metric(trace, "status build total", "build_total_ms");
    let capture_repo = phase_metric(trace, "capture phases", "snapshot_ms");
    let counters = structural_counters(trace);
    Sample {
        wall_ms,
        profile_total_ms: total_ms,
        startup_ms: (wall_ms - command_body_ms).max(0.0),
        warm_repository_ms: status_repo + capture_repo,
        repository_open_ms: phase_metric(trace, "status repo open", "repo_open_ms"),
        current_state_ms: phase_metric(trace, "status current state", "current_state_ms"),
        verification_ms: phase_metric(trace, "status verification", "verification_ms"),
        worktree_status_ms: phase_metric(trace, "status worktree status", "worktree_status_ms")
            + phase_metric(trace, "capture phases", "worktree_status_ms"),
        thread_summary_ms: phase_metric(trace, "status thread summary", "thread_summary_ms"),
        snapshot_ms: phase_metric(trace, "capture phases", "snapshot_ms"),
        snapshot_tree_walk_ms: phase_metric(trace, "capture phases", "snapshot_tree_walk_ms"),
        snapshot_blob_prep_ms: phase_metric(trace, "capture phases", "snapshot_blob_prep_ms"),
        snapshot_blob_write_ms: phase_metric(trace, "capture phases", "snapshot_blob_write_ms"),
        snapshot_tree_write_ms: phase_metric(trace, "capture phases", "snapshot_tree_write_ms"),
        snapshot_state_ref_oplog_ms: phase_metric(
            trace,
            "capture phases",
            "snapshot_state_ref_oplog_ms",
        ),
        snapshot_thread_metadata_ms: phase_metric(
            trace,
            "capture phases",
            "snapshot_thread_metadata_ms",
        ),
        monitor_ms: phase_metric(trace, "structural counters", "monitor_startup_ms"),
        rendering_ms: phase_metric(trace, "status render", "render_ms")
            + phase_metric(trace, "capture phases", "render_ms"),
        network_ms: 0.0,
        counters,
    }
}

fn total_metric(trace: &Value, name: &str) -> f64 {
    trace["totals"][name]["value"].as_u64().unwrap_or(0) as f64
}

fn phase_metric(trace: &Value, phase_name: &str, metric_name: &str) -> f64 {
    trace["phases"]
        .as_array()
        .and_then(|phases| phases.iter().find(|phase| phase["name"] == phase_name))
        .and_then(|phase| phase["metrics"][metric_name]["value"].as_u64())
        .unwrap_or(0) as f64
}

fn structural_counters(trace: &Value) -> Counters {
    let metrics = trace["phases"]
        .as_array()
        .and_then(|phases| {
            phases
                .iter()
                .find(|phase| phase["name"] == "structural counters")
        })
        .map(|phase| &phase["metrics"])
        .unwrap_or_else(|| panic!("structural counters missing: {trace}"));
    let count = |name: &str| metrics[name]["value"].as_u64().unwrap_or(0);
    Counters {
        directories_scanned: count("directories_scanned"),
        directories_skipped: count("directories_skipped"),
        files_hashed: count("files_hashed"),
        monitor_changed_paths: count("monitor_changed_paths"),
        object_decodes: count("object_decodes"),
        ref_reads: count("ref_reads"),
        oplog_reads: count("oplog_reads"),
        repository_opens: count("repository_opens"),
        network_client_initialized: metrics["network_client_initialized"]["value"]
            .as_bool()
            .unwrap_or(true),
        merge_base_ancestors_visited: count("merge_base_ancestors_visited"),
    }
}
