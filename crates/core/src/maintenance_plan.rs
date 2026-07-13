// SPDX-License-Identifier: Apache-2.0
//! Pure maintenance inspect/refresh **fact** types + one human renderer each.
//!
//! Not twenty public `format!` sentence factories. Callers pass structured
//! fields; CLI prints `lines()`. Repo I/O stays outside this module.

/// Re-export compact yes/no token (same as log/timeline fields).
pub use crate::log_plan::yes_no;

/// present/absent token for index and cache presence fields.
pub fn presence(value: bool) -> &'static str {
    if value { "present" } else { "absent" }
}

/// Structured facts for `heddle maintenance inspect` human output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceInspectView {
    pub commit_graph_present: bool,
    pub commit_graph_nodes: usize,
    pub commit_graph_bloom: usize,
    pub worktree_index_present: bool,
    pub worktree_index_files: usize,
    pub worktree_index_directories: usize,
    pub worktree_index_untracked_directories: usize,
    pub change_monitor_backend: String,
    pub change_monitor_status: String,
    pub refs_threads: usize,
    pub refs_markers: usize,
    pub refs_remotes: usize,
    pub refs_remote_threads: usize,
    pub ref_summary_present: bool,
    pub ref_summary_valid: bool,
    pub ref_summary_threads: usize,
    pub ref_summary_markers: usize,
    pub ref_summary_remotes: usize,
    pub ref_summary_remote_threads: usize,
    pub pack_count: usize,
    pub index_count: usize,
    pub unpaired_packs: usize,
    pub pending_install_intents: usize,
    pub missing_blob_count: usize,
    pub pull_planner_status: String,
    pub pull_planner_manifests: usize,
    pub pull_planner_entries: usize,
}

impl MaintenanceInspectView {
    /// Human lines for inspect (stable order).
    pub fn lines(&self) -> Vec<String> {
        vec![
            format!(
                "Commit graph: {} (nodes: {}, bloom-covered: {})",
                presence(self.commit_graph_present),
                self.commit_graph_nodes,
                self.commit_graph_bloom
            ),
            format!(
                "Worktree index: {} (files: {}, directories: {}, untracked directories: {})",
                presence(self.worktree_index_present),
                self.worktree_index_files,
                self.worktree_index_directories,
                self.worktree_index_untracked_directories
            ),
            format!(
                "Change monitor: {} / {}",
                self.change_monitor_backend, self.change_monitor_status
            ),
            format!(
                "Refs: {} threads, {} markers, {} remotes, {} remote threads",
                self.refs_threads, self.refs_markers, self.refs_remotes, self.refs_remote_threads
            ),
            format!(
                "Ref summary index: {} (valid: {}, threads: {}, markers: {}, remotes: {}, remote threads: {})",
                presence(self.ref_summary_present),
                yes_no(self.ref_summary_valid),
                self.ref_summary_threads,
                self.ref_summary_markers,
                self.ref_summary_remotes,
                self.ref_summary_remote_threads
            ),
            format!(
                "Packs: {} pack files, {} indexes",
                self.pack_count, self.index_count
            ),
            format!(
                "Pack install: {} unpaired packs, {} pending install intents",
                self.unpaired_packs, self.pending_install_intents
            ),
            format!("Partial fetch: {} missing blobs", self.missing_blob_count),
            format!(
                "Pull planner cache: {} (manifests: {}, planner entries: {})",
                self.pull_planner_status, self.pull_planner_manifests, self.pull_planner_entries
            ),
        ]
    }
}

/// Structured facts for `heddle maintenance refresh` human output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceRefreshView {
    pub rebuilt_commit_graph: bool,
    pub rebuilt_ref_summary_index: bool,
    pub rebuilt_worktree_index: bool,
    pub refreshed_change_monitor: bool,
    pub rebuilt_pull_planner_cache: bool,
    pub pruned_pull_planner_entries: usize,
    pub pack_install_completed: u64,
    pub pack_install_aborted: u64,
    pub pack_install_skipped: u64,
    pub pack_install_quarantined: u64,
    pub unpaired_packs_pruned: u64,
    pub unpaired_pack_bytes_freed: u64,
    pub commit_graph_nodes_now: usize,
    pub commit_graph_bloom_now: usize,
    pub ref_summary_threads_now: usize,
    pub ref_summary_markers_now: usize,
    pub ref_summary_remotes_now: usize,
    pub ref_summary_remote_threads_now: usize,
    pub worktree_index_files_now: usize,
    pub pull_planner_manifests_now: usize,
    pub pull_planner_entries_now: usize,
}

impl MaintenanceRefreshView {
    /// Human lines for refresh (stable order).
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!(
                "Rebuilt commit graph: {}",
                yes_no(self.rebuilt_commit_graph)
            ),
            format!(
                "Rebuilt ref summary index: {}",
                yes_no(self.rebuilt_ref_summary_index)
            ),
            format!(
                "Rebuilt worktree index: {}",
                yes_no(self.rebuilt_worktree_index)
            ),
            format!(
                "Refreshed change monitor: {}",
                yes_no(self.refreshed_change_monitor)
            ),
            format!(
                "Rebuilt pull planner cache: {}",
                yes_no(self.rebuilt_pull_planner_cache)
            ),
            format!(
                "Pruned pull planner entries: {}",
                self.pruned_pull_planner_entries
            ),
        ];
        let mut recover = format!(
            "Pack install intents recovered: {} completed, {} aborted",
            self.pack_install_completed, self.pack_install_aborted
        );
        if self.pack_install_skipped > 0 || self.pack_install_quarantined > 0 {
            recover.push_str(&format!(
                " (skipped in-progress: {}, quarantined: {})",
                self.pack_install_skipped, self.pack_install_quarantined
            ));
        }
        lines.push(recover);
        lines.push(if self.unpaired_packs_pruned > 0 {
            format!(
                "Pruned {} unpaired packs (freed {} bytes)",
                self.unpaired_packs_pruned, self.unpaired_pack_bytes_freed
            )
        } else {
            "No unpaired packs to prune".to_string()
        });
        lines.push(format!(
            "Commit graph now has {} nodes and {} Bloom-covered nodes",
            self.commit_graph_nodes_now, self.commit_graph_bloom_now
        ));
        lines.push(format!(
            "Ref summary index now covers {} threads, {} markers, {} remotes, and {} remote threads",
            self.ref_summary_threads_now,
            self.ref_summary_markers_now,
            self.ref_summary_remotes_now,
            self.ref_summary_remote_threads_now
        ));
        lines.push(format!(
            "Worktree index now has {} files cached",
            self.worktree_index_files_now
        ));
        lines.push(format!(
            "Pull planner cache now has {} manifests and {} planner entries",
            self.pull_planner_manifests_now, self.pull_planner_entries_now
        ));
        lines
    }
}

/// Compact GC recover summary line.
pub fn pack_install_recover_line(completed: u64, aborted: u64) -> String {
    format!("Pack install intents recovered: {completed} completed, {aborted} aborted")
}

/// Compact unpaired-pack prune summary line (GC path).
pub fn unpaired_packs_pruned_line(removed: u64, bytes_freed: u64) -> String {
    if removed > 0 {
        format!("Pruned {removed} unpaired packs (freed {bytes_freed} bytes)")
    } else {
        "No unpaired packs to prune".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspect_view_lines_include_packs_and_install() {
        let v = MaintenanceInspectView {
            commit_graph_present: true,
            commit_graph_nodes: 10,
            commit_graph_bloom: 4,
            worktree_index_present: false,
            worktree_index_files: 1,
            worktree_index_directories: 2,
            worktree_index_untracked_directories: 3,
            change_monitor_backend: "fs_events".into(),
            change_monitor_status: "ready".into(),
            refs_threads: 1,
            refs_markers: 2,
            refs_remotes: 3,
            refs_remote_threads: 4,
            ref_summary_present: true,
            ref_summary_valid: false,
            ref_summary_threads: 1,
            ref_summary_markers: 2,
            ref_summary_remotes: 3,
            ref_summary_remote_threads: 4,
            pack_count: 2,
            index_count: 2,
            unpaired_packs: 1,
            pending_install_intents: 2,
            missing_blob_count: 5,
            pull_planner_status: "ready".into(),
            pull_planner_manifests: 1,
            pull_planner_entries: 2,
        };
        let lines = v.lines();
        assert!(lines[0].contains("Commit graph: present"));
        assert!(lines.iter().any(|l| l.contains("Pack install: 1 unpaired")));
        assert!(lines.iter().any(|l| l.contains("Partial fetch: 5")));
    }

    #[test]
    fn refresh_view_lines_include_recover_detail() {
        let v = MaintenanceRefreshView {
            rebuilt_commit_graph: true,
            rebuilt_ref_summary_index: false,
            rebuilt_worktree_index: true,
            refreshed_change_monitor: false,
            rebuilt_pull_planner_cache: true,
            pruned_pull_planner_entries: 3,
            pack_install_completed: 0,
            pack_install_aborted: 1,
            pack_install_skipped: 3,
            pack_install_quarantined: 2,
            unpaired_packs_pruned: 0,
            unpaired_pack_bytes_freed: 0,
            commit_graph_nodes_now: 9,
            commit_graph_bloom_now: 2,
            ref_summary_threads_now: 1,
            ref_summary_markers_now: 2,
            ref_summary_remotes_now: 3,
            ref_summary_remote_threads_now: 4,
            worktree_index_files_now: 42,
            pull_planner_manifests_now: 1,
            pull_planner_entries_now: 2,
        };
        let lines = v.lines();
        assert!(lines.iter().any(|l| l.contains("skipped in-progress: 3")));
        assert!(lines.iter().any(|l| l.contains("No unpaired packs")));
        assert_eq!(
            pack_install_recover_line(2, 1),
            "Pack install intents recovered: 2 completed, 1 aborted"
        );
        assert_eq!(
            unpaired_packs_pruned_line(3, 99),
            "Pruned 3 unpaired packs (freed 99 bytes)"
        );
    }
}
