// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;
use heddle_core::maintenance_plan::{
    inspect_change_monitor_line, inspect_commit_graph_line, inspect_packs_line,
    inspect_partial_fetch_line, inspect_pull_planner_cache_line, inspect_ref_summary_index_line,
    inspect_refs_line, inspect_worktree_index_line, run_commit_graph_now_line,
    run_pruned_pull_planner_entries_line, run_pull_planner_cache_now_line,
    run_rebuilt_commit_graph_line, run_rebuilt_pull_planner_cache_line,
    run_rebuilt_ref_summary_index_line, run_rebuilt_worktree_index_line, run_ref_summary_now_line,
    run_refreshed_change_monitor_line, run_worktree_index_now_line,
};

use crate::cli::{
    Cli, MaintenanceCommands,
    commands::{cmd_gc, cmd_index, cmd_monitor},
    should_output_json, worktree_status_options,
};

pub fn cmd_maintenance(cli: &Cli, command: MaintenanceCommands) -> Result<()> {
    let repo = cli.open_repo()?;
    let options = worktree_status_options(Some(repo.config()));

    match command {
        MaintenanceCommands::Inspect => {
            let report = repo.inspect_performance_with_options(&options)?;
            if should_output_json(cli, Some(repo.config())) {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!(
                    "{}",
                    inspect_commit_graph_line(
                        report.commit_graph.present,
                        report.commit_graph.node_count,
                        report.commit_graph.bloom_covered_nodes
                    )
                );
                println!(
                    "{}",
                    inspect_worktree_index_line(
                        report.worktree_index.present,
                        report.worktree_index.file_entries,
                        report.worktree_index.directory_entries,
                        report.worktree_index.untracked_directory_entries
                    )
                );
                println!(
                    "{}",
                    inspect_change_monitor_line(
                        &report.change_monitor.backend,
                        &report.change_monitor.status
                    )
                );
                println!(
                    "{}",
                    inspect_refs_line(
                        report.ref_counts.threads,
                        report.ref_counts.markers,
                        report.ref_counts.remotes,
                        report.ref_counts.remote_threads
                    )
                );
                println!(
                    "{}",
                    inspect_ref_summary_index_line(
                        report.ref_summary_index.present,
                        report.ref_summary_index.valid,
                        report.ref_summary_index.threads,
                        report.ref_summary_index.markers,
                        report.ref_summary_index.remotes,
                        report.ref_summary_index.remote_threads
                    )
                );
                println!(
                    "{}",
                    inspect_packs_line(report.pack_files.pack_count, report.pack_files.index_count)
                );
                println!(
                    "{}",
                    inspect_partial_fetch_line(report.partial_fetch.missing_blob_count)
                );
                println!(
                    "{}",
                    inspect_pull_planner_cache_line(
                        &report.pull_planner_cache.status,
                        report.pull_planner_cache.manifest_count,
                        report.pull_planner_cache.planner_entry_count
                    )
                );
            }
        }
        MaintenanceCommands::Run => {
            let run = repo.run_maintenance_with_options(&options)?;
            if should_output_json(cli, Some(repo.config())) {
                println!("{}", serde_json::to_string(&run)?);
            } else {
                println!(
                    "{}",
                    run_rebuilt_commit_graph_line(run.rebuilt_commit_graph)
                );
                println!(
                    "{}",
                    run_rebuilt_ref_summary_index_line(run.rebuilt_ref_summary_index)
                );
                println!(
                    "{}",
                    run_rebuilt_worktree_index_line(run.rebuilt_worktree_index)
                );
                println!(
                    "{}",
                    run_refreshed_change_monitor_line(run.refreshed_change_monitor)
                );
                println!(
                    "{}",
                    run_rebuilt_pull_planner_cache_line(run.rebuilt_pull_planner_cache)
                );
                println!(
                    "{}",
                    run_pruned_pull_planner_entries_line(run.pruned_pull_planner_entries)
                );
                println!(
                    "{}",
                    run_commit_graph_now_line(
                        run.report.commit_graph.node_count,
                        run.report.commit_graph.bloom_covered_nodes
                    )
                );
                println!(
                    "{}",
                    run_ref_summary_now_line(
                        run.report.ref_summary_index.threads,
                        run.report.ref_summary_index.markers,
                        run.report.ref_summary_index.remotes,
                        run.report.ref_summary_index.remote_threads
                    )
                );
                println!(
                    "{}",
                    run_worktree_index_now_line(run.report.worktree_index.file_entries)
                );
                println!(
                    "{}",
                    run_pull_planner_cache_now_line(
                        run.report.pull_planner_cache.manifest_count,
                        run.report.pull_planner_cache.planner_entry_count
                    )
                );
            }
        }
        MaintenanceCommands::Gc {
            prune,
            aggressive,
            dry_run,
        } => {
            return cmd_gc(cli, prune, aggressive, dry_run);
        }
        MaintenanceCommands::Index { dump } => {
            return cmd_index(cli, dump);
        }
        MaintenanceCommands::Monitor { paths, serve } => {
            return cmd_monitor(cli, paths, serve);
        }
    }

    Ok(())
}
