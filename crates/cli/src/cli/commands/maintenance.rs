// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;

use crate::cli::{
    Cli, MaintenanceCommands,
    commands::{cmd_gc, cmd_index, cmd_monitor},
    should_output_json, worktree_status_options,
};

pub fn cmd_maintenance(cli: &Cli, command: MaintenanceCommands) -> Result<()> {
    let repo = repo::Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let options = worktree_status_options(Some(repo.config()));

    match command {
        MaintenanceCommands::Inspect => {
            let report = repo.inspect_performance_with_options(&options)?;
            if should_output_json(cli, Some(repo.config())) {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!(
                    "Commit graph: {} (nodes: {}, bloom-covered: {})",
                    presence(report.commit_graph.present),
                    report.commit_graph.node_count,
                    report.commit_graph.bloom_covered_nodes
                );
                println!(
                    "Worktree index: {} (files: {}, directories: {}, untracked directories: {})",
                    presence(report.worktree_index.present),
                    report.worktree_index.file_entries,
                    report.worktree_index.directory_entries,
                    report.worktree_index.untracked_directory_entries
                );
                println!(
                    "Change monitor: {} / {}",
                    report.change_monitor.backend, report.change_monitor.status
                );
                println!(
                    "Refs: {} threads, {} markers, {} remotes, {} remote threads",
                    report.ref_counts.threads,
                    report.ref_counts.markers,
                    report.ref_counts.remotes,
                    report.ref_counts.remote_threads
                );
                println!(
                    "Ref summary index: {} (valid: {}, threads: {}, markers: {}, remotes: {}, remote threads: {})",
                    presence(report.ref_summary_index.present),
                    yes_no(report.ref_summary_index.valid),
                    report.ref_summary_index.threads,
                    report.ref_summary_index.markers,
                    report.ref_summary_index.remotes,
                    report.ref_summary_index.remote_threads
                );
                println!(
                    "Packs: {} pack files, {} indexes",
                    report.pack_files.pack_count, report.pack_files.index_count
                );
                println!(
                    "Partial fetch: {} missing blobs",
                    report.partial_fetch.missing_blob_count
                );
                println!(
                    "Pull planner cache: {} (manifests: {}, planner entries: {})",
                    report.pull_planner_cache.status,
                    report.pull_planner_cache.manifest_count,
                    report.pull_planner_cache.planner_entry_count
                );
            }
        }
        MaintenanceCommands::Run => {
            let run = repo.run_maintenance_with_options(&options)?;
            if should_output_json(cli, Some(repo.config())) {
                println!("{}", serde_json::to_string(&run)?);
            } else {
                println!("Rebuilt commit graph: {}", yes_no(run.rebuilt_commit_graph));
                println!(
                    "Rebuilt ref summary index: {}",
                    yes_no(run.rebuilt_ref_summary_index)
                );
                println!(
                    "Rebuilt worktree index: {}",
                    yes_no(run.rebuilt_worktree_index)
                );
                println!(
                    "Refreshed change monitor: {}",
                    yes_no(run.refreshed_change_monitor)
                );
                println!(
                    "Rebuilt pull planner cache: {}",
                    yes_no(run.rebuilt_pull_planner_cache)
                );
                println!(
                    "Pruned pull planner entries: {}",
                    run.pruned_pull_planner_entries
                );
                println!(
                    "Commit graph now has {} nodes and {} Bloom-covered nodes",
                    run.report.commit_graph.node_count, run.report.commit_graph.bloom_covered_nodes
                );
                println!(
                    "Ref summary index now covers {} threads, {} markers, {} remotes, and {} remote threads",
                    run.report.ref_summary_index.threads,
                    run.report.ref_summary_index.markers,
                    run.report.ref_summary_index.remotes,
                    run.report.ref_summary_index.remote_threads
                );
                println!(
                    "Worktree index now has {} files cached",
                    run.report.worktree_index.file_entries
                );
                println!(
                    "Pull planner cache now has {} manifests and {} planner entries",
                    run.report.pull_planner_cache.manifest_count,
                    run.report.pull_planner_cache.planner_entry_count
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

fn presence(value: bool) -> &'static str {
    if value { "present" } else { "absent" }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}