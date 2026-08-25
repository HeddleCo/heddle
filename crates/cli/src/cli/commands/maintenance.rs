// SPDX-License-Identifier: Apache-2.0
use std::{num::NonZeroUsize, sync::Arc};

use anyhow::Result;
use objects::store::{
    FsRepackOperation, RepackPolicy, RepackResourceLimits, RepackSchedule, RepackScheduler,
};
use serde::Serialize;
use verbs::maintenance_plan::{MaintenanceInspectView, MaintenanceRefreshView};

use super::next_action::{NextActionValidationContext, write_full_command_json};
use crate::cli::{
    Cli, FsckCommands, FsckRepairCommands, MaintenanceCommands,
    commands::{cmd_fsck, cmd_fsck_repair_git, cmd_gc, cmd_oplog},
    should_output_json, worktree_status_options,
};

// The repack wire payload lives in cli-contract so the schema registry
// registers the real serialization type.
pub(crate) use heddle_cli_contract::cli::commands::wire::bridge::RepackOutput;

#[derive(Serialize)]
struct MaintenanceOutput<'a, T> {
    output_kind: &'static str,
    #[serde(flatten)]
    report: &'a T,
}

fn render_repack(output: &RepackOutput, json: bool) -> Result<()> {
    if json {
        write_full_command_json(
            output,
            NextActionValidationContext::without_repo(&["maintenance", "repack"]),
        )?;
    } else {
        println!(
            "Repacked {} objects ({} bytes) in {} ms; reclaimed {} bytes",
            output.objects_repacked,
            output.bytes_repacked,
            output.duration_ms,
            output.bytes_reclaimed
        );
    }
    Ok(())
}

pub fn cmd_maintenance(cli: &Cli, command: MaintenanceCommands) -> Result<()> {
    // Oplog recovery must run through its own non-validating repository
    // open: a torn oplog is exactly what it repairs, so the ordinary
    // (validating) open below would refuse before the handler ever runs.
    if let MaintenanceCommands::Oplog { command } = &command {
        return cmd_oplog(cli, command.clone());
    }

    let repo = cli.open_repo()?;
    let options = worktree_status_options(Some(repo.config()));

    match command {
        MaintenanceCommands::Fsck(args) => {
            return match args.command {
                None => cmd_fsck(cli, args.full, args.thorough, args.provenance, args.git),
                Some(FsckCommands::Repair { target }) => match target {
                    FsckRepairCommands::Git(args) => {
                        cmd_fsck_repair_git(cli, args.ref_name, args.prefer, args.preview)
                    }
                },
            };
        }
        MaintenanceCommands::Inspect => {
            let report = repo.inspect_performance_with_options(&options)?;
            if should_output_json(cli, Some(repo.config())) {
                write_full_command_json(
                    &MaintenanceOutput {
                        output_kind: "maintenance_inspect",
                        report: &report,
                    },
                    NextActionValidationContext::without_repo(&["maintenance", "inspect"]),
                )?;
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
                write_full_command_json(
                    &MaintenanceOutput {
                        output_kind: "maintenance_refresh",
                        report: &run,
                    },
                    NextActionValidationContext::without_repo(&["maintenance", "refresh"]),
                )?;
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
        MaintenanceCommands::Repack => {
            let scheduler = RepackScheduler::new(
                RepackPolicy::default(),
                RepackResourceLimits::new(NonZeroUsize::MIN),
            );
            let operation = Arc::new(FsRepackOperation::new(repo.store().clone()));
            let RepackSchedule::Started(handle) = scheduler.repack_now(operation)? else {
                unreachable!("a fresh manual repack scheduler always has capacity");
            };
            let report = handle.wait()?;
            let output = RepackOutput {
                output_kind: "maintenance_repack",
                objects_repacked: report.objects_repacked,
                bytes_repacked: report.bytes_repacked,
                duration_ms: report.duration.as_millis(),
                bytes_reclaimed: report.bytes_reclaimed,
            };
            render_repack(&output, should_output_json(cli, Some(repo.config())))?;
        }
        MaintenanceCommands::Gc {
            prune,
            aggressive,
            dry_run,
        } => {
            return cmd_gc(cli, prune, aggressive, dry_run);
        }
        // Handled above, before the validating repository open.
        MaintenanceCommands::Oplog { .. } => {}
    }

    Ok(())
}
