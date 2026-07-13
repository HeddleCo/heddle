// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;
use heddle_core::maintenance_plan::{MaintenanceInspectView, MaintenanceRefreshView};
use serde::Serialize;

use crate::cli::{
    Cli, MaintenanceCommands, commands::cmd_gc, should_output_json, worktree_status_options,
};

#[derive(Serialize)]
struct MaintenanceOutput<'a, T> {
    output_kind: &'static str,
    #[serde(flatten)]
    report: &'a T,
}

pub fn cmd_maintenance(cli: &Cli, command: MaintenanceCommands) -> Result<()> {
    let repo = cli.open_repo()?;
    let options = worktree_status_options(Some(repo.config()));

    match command {
        MaintenanceCommands::Inspect => {
            let report = repo.inspect_performance_with_options(&options)?;
            if should_output_json(cli, Some(repo.config())) {
                println!(
                    "{}",
                    serde_json::to_string(&MaintenanceOutput {
                        output_kind: "maintenance_inspect",
                        report: &report,
                    })?
                );
            } else {
                let view = MaintenanceInspectView {
                    commit_graph_present: report.commit_graph.present,
                    commit_graph_nodes: report.commit_graph.node_count,
                    commit_graph_bloom: report.commit_graph.bloom_covered_nodes,
                    worktree_index_present: report.worktree_index.present,
                    worktree_index_files: report.worktree_index.file_entries,
                    worktree_index_directories: report.worktree_index.directory_entries,
                    worktree_index_untracked_directories: report
                        .worktree_index
                        .untracked_directory_entries,
                    change_monitor_backend: report.change_monitor.backend.clone(),
                    change_monitor_status: report.change_monitor.status.clone(),
                    refs_threads: report.ref_counts.threads,
                    refs_markers: report.ref_counts.markers,
                    refs_remotes: report.ref_counts.remotes,
                    refs_remote_threads: report.ref_counts.remote_threads,
                    ref_summary_present: report.ref_summary_index.present,
                    ref_summary_valid: report.ref_summary_index.valid,
                    ref_summary_threads: report.ref_summary_index.threads,
                    ref_summary_markers: report.ref_summary_index.markers,
                    ref_summary_remotes: report.ref_summary_index.remotes,
                    ref_summary_remote_threads: report.ref_summary_index.remote_threads,
                    pack_count: report.pack_files.pack_count,
                    index_count: report.pack_files.index_count,
                    unpaired_packs: report.pack_files.unpaired_pack_count,
                    pending_install_intents: report.pack_files.pending_install_intents,
                    missing_blob_count: report.partial_fetch.missing_blob_count,
                    pull_planner_status: report.pull_planner_cache.status.clone(),
                    pull_planner_manifests: report.pull_planner_cache.manifest_count,
                    pull_planner_entries: report.pull_planner_cache.planner_entry_count,
                };
                for line in view.lines() {
                    println!("{line}");
                }
            }
        }
        MaintenanceCommands::Refresh => {
            let run = repo.run_maintenance_with_options(&options)?;
            if should_output_json(cli, Some(repo.config())) {
                println!(
                    "{}",
                    serde_json::to_string(&MaintenanceOutput {
                        output_kind: "maintenance_refresh",
                        report: &run,
                    })?
                );
            } else {
                let view = MaintenanceRefreshView {
                    rebuilt_commit_graph: run.rebuilt_commit_graph,
                    rebuilt_ref_summary_index: run.rebuilt_ref_summary_index,
                    rebuilt_worktree_index: run.rebuilt_worktree_index,
                    refreshed_change_monitor: run.refreshed_change_monitor,
                    rebuilt_pull_planner_cache: run.rebuilt_pull_planner_cache,
                    pruned_pull_planner_entries: run.pruned_pull_planner_entries,
                    pack_install_completed: run.pack_install_intents_recovered_completed,
                    pack_install_aborted: run.pack_install_intents_aborted,
                    pack_install_skipped: run.pack_install_intents_skipped_in_progress,
                    pack_install_quarantined: run.pack_install_intents_quarantined,
                    unpaired_packs_pruned: run.unpaired_packs_pruned,
                    unpaired_pack_bytes_freed: run.unpaired_pack_bytes_freed,
                    commit_graph_nodes_now: run.report.commit_graph.node_count,
                    commit_graph_bloom_now: run.report.commit_graph.bloom_covered_nodes,
                    ref_summary_threads_now: run.report.ref_summary_index.threads,
                    ref_summary_markers_now: run.report.ref_summary_index.markers,
                    ref_summary_remotes_now: run.report.ref_summary_index.remotes,
                    ref_summary_remote_threads_now: run.report.ref_summary_index.remote_threads,
                    worktree_index_files_now: run.report.worktree_index.file_entries,
                    pull_planner_manifests_now: run.report.pull_planner_cache.manifest_count,
                    pull_planner_entries_now: run.report.pull_planner_cache.planner_entry_count,
                };
                for line in view.lines() {
                    println!("{line}");
                }
            }
        }
        MaintenanceCommands::Gc {
            prune,
            aggressive,
            dry_run,
        } => {
            return cmd_gc(cli, prune, aggressive, dry_run);
        }
    }

    Ok(())
}
