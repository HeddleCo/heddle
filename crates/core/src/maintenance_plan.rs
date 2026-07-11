// SPDX-License-Identifier: Apache-2.0
//! Pure maintenance inspect/run message assembly (no repo I/O).
//!
//! Owns human summary line formatters for `heddle maintenance inspect`
//! and `heddle maintenance run` from primitive report fields. Repo
//! inspection and mutation stay CLI-owned.

/// Re-export compact yes/no token (same as log/timeline fields).
pub use crate::log_plan::yes_no;

/// present/absent token for index and cache presence fields.
pub fn presence(value: bool) -> &'static str {
    if value { "present" } else { "absent" }
}

/// `Commit graph: present|absent (nodes: N, bloom-covered: M)`.
pub fn inspect_commit_graph_line(present: bool, nodes: usize, bloom: usize) -> String {
    format!(
        "Commit graph: {} (nodes: {}, bloom-covered: {})",
        presence(present),
        nodes,
        bloom
    )
}

/// `Worktree index: present|absent (files: F, directories: D, untracked directories: U)`.
pub fn inspect_worktree_index_line(
    present: bool,
    files: usize,
    directories: usize,
    untracked_directories: usize,
) -> String {
    format!(
        "Worktree index: {} (files: {}, directories: {}, untracked directories: {})",
        presence(present),
        files,
        directories,
        untracked_directories
    )
}

/// `Change monitor: BACKEND / STATUS`.
pub fn inspect_change_monitor_line(backend: &str, status: &str) -> String {
    format!("Change monitor: {backend} / {status}")
}

/// `Refs: T threads, M markers, R remotes, RT remote threads`.
pub fn inspect_refs_line(
    threads: usize,
    markers: usize,
    remotes: usize,
    remote_threads: usize,
) -> String {
    format!(
        "Refs: {threads} threads, {markers} markers, {remotes} remotes, {remote_threads} remote threads"
    )
}

/// Ref summary index inspect line.
pub fn inspect_ref_summary_index_line(
    present: bool,
    valid: bool,
    threads: usize,
    markers: usize,
    remotes: usize,
    remote_threads: usize,
) -> String {
    format!(
        "Ref summary index: {} (valid: {}, threads: {}, markers: {}, remotes: {}, remote threads: {})",
        presence(present),
        yes_no(valid),
        threads,
        markers,
        remotes,
        remote_threads
    )
}

/// `Packs: P pack files, I indexes`.
pub fn inspect_packs_line(pack_count: usize, index_count: usize) -> String {
    format!("Packs: {pack_count} pack files, {index_count} indexes")
}

/// `Partial fetch: N missing blobs`.
pub fn inspect_partial_fetch_line(missing_blob_count: usize) -> String {
    format!("Partial fetch: {missing_blob_count} missing blobs")
}

/// Pull planner cache inspect line.
pub fn inspect_pull_planner_cache_line(
    status: &str,
    manifests: usize,
    planner_entries: usize,
) -> String {
    format!(
        "Pull planner cache: {status} (manifests: {manifests}, planner entries: {planner_entries})"
    )
}

/// `Rebuilt commit graph: yes|no`.
pub fn run_rebuilt_commit_graph_line(rebuilt: bool) -> String {
    format!("Rebuilt commit graph: {}", yes_no(rebuilt))
}

/// `Rebuilt ref summary index: yes|no`.
pub fn run_rebuilt_ref_summary_index_line(rebuilt: bool) -> String {
    format!("Rebuilt ref summary index: {}", yes_no(rebuilt))
}

/// `Rebuilt worktree index: yes|no`.
pub fn run_rebuilt_worktree_index_line(rebuilt: bool) -> String {
    format!("Rebuilt worktree index: {}", yes_no(rebuilt))
}

/// `Refreshed change monitor: yes|no`.
pub fn run_refreshed_change_monitor_line(refreshed: bool) -> String {
    format!("Refreshed change monitor: {}", yes_no(refreshed))
}

/// `Rebuilt pull planner cache: yes|no`.
pub fn run_rebuilt_pull_planner_cache_line(rebuilt: bool) -> String {
    format!("Rebuilt pull planner cache: {}", yes_no(rebuilt))
}

/// `Pruned pull planner entries: N`.
pub fn run_pruned_pull_planner_entries_line(count: usize) -> String {
    format!("Pruned pull planner entries: {count}")
}

/// Post-run commit graph size line.
pub fn run_commit_graph_now_line(nodes: usize, bloom: usize) -> String {
    format!("Commit graph now has {nodes} nodes and {bloom} Bloom-covered nodes")
}

/// Post-run ref summary coverage line.
pub fn run_ref_summary_now_line(
    threads: usize,
    markers: usize,
    remotes: usize,
    remote_threads: usize,
) -> String {
    format!(
        "Ref summary index now covers {threads} threads, {markers} markers, {remotes} remotes, and {remote_threads} remote threads"
    )
}

/// Post-run worktree index size line.
pub fn run_worktree_index_now_line(file_entries: usize) -> String {
    format!("Worktree index now has {file_entries} files cached")
}

/// Post-run pull planner cache size line.
pub fn run_pull_planner_cache_now_line(manifests: usize, planner_entries: usize) -> String {
    format!(
        "Pull planner cache now has {manifests} manifests and {planner_entries} planner entries"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_and_yes_no() {
        assert_eq!(presence(true), "present");
        assert_eq!(presence(false), "absent");
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");
    }

    #[test]
    fn inspect_lines() {
        assert_eq!(
            inspect_commit_graph_line(true, 10, 4),
            "Commit graph: present (nodes: 10, bloom-covered: 4)"
        );
        assert_eq!(
            inspect_worktree_index_line(false, 1, 2, 3),
            "Worktree index: absent (files: 1, directories: 2, untracked directories: 3)"
        );
        assert_eq!(
            inspect_change_monitor_line("fs_events", "ready"),
            "Change monitor: fs_events / ready"
        );
        assert_eq!(
            inspect_refs_line(1, 2, 3, 4),
            "Refs: 1 threads, 2 markers, 3 remotes, 4 remote threads"
        );
        assert!(inspect_ref_summary_index_line(true, false, 1, 2, 3, 4).contains("valid: no"));
        assert_eq!(inspect_packs_line(2, 2), "Packs: 2 pack files, 2 indexes");
        assert_eq!(
            inspect_partial_fetch_line(5),
            "Partial fetch: 5 missing blobs"
        );
        assert!(inspect_pull_planner_cache_line("ready", 1, 2).contains("manifests: 1"));
    }

    #[test]
    fn run_lines() {
        assert_eq!(
            run_rebuilt_commit_graph_line(true),
            "Rebuilt commit graph: yes"
        );
        assert_eq!(
            run_rebuilt_ref_summary_index_line(false),
            "Rebuilt ref summary index: no"
        );
        assert_eq!(
            run_rebuilt_worktree_index_line(true),
            "Rebuilt worktree index: yes"
        );
        assert_eq!(
            run_refreshed_change_monitor_line(false),
            "Refreshed change monitor: no"
        );
        assert_eq!(
            run_rebuilt_pull_planner_cache_line(true),
            "Rebuilt pull planner cache: yes"
        );
        assert_eq!(
            run_pruned_pull_planner_entries_line(3),
            "Pruned pull planner entries: 3"
        );
        assert_eq!(
            run_commit_graph_now_line(9, 2),
            "Commit graph now has 9 nodes and 2 Bloom-covered nodes"
        );
        assert!(run_ref_summary_now_line(1, 2, 3, 4).contains("remote threads"));
        assert_eq!(
            run_worktree_index_now_line(42),
            "Worktree index now has 42 files cached"
        );
        assert_eq!(
            run_pull_planner_cache_now_line(1, 2),
            "Pull planner cache now has 1 manifests and 2 planner entries"
        );
    }
}
