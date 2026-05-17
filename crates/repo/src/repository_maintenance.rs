// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use objects::{fs_atomic::write_file_atomic, object::ChangeId};
use proto::{PlannedObject, StateClosureOptions, enumerate_state_closure_plan_with_options};
use refs::{Head, RefSummaryIndexInspection};
use serde::{Deserialize, Serialize};

use super::{
    CommitGraphIndex, Repository, Result,
    commit_graph_persistence::{commit_graph_path, load_commit_graph},
};
use crate::{FsMonitorSettings, HeddleError, WorktreeIndex, WorktreeStatusOptions};

#[derive(Debug, Clone, Serialize)]
pub struct RepositoryPerformanceInspectionReport {
    pub commit_graph: CommitGraphInspection,
    pub worktree_index: WorktreeIndexInspection,
    pub change_monitor: ChangeMonitorInspection,
    #[serde(rename = "refs")]
    pub ref_counts: RefCountsInspection,
    pub ref_summary_index: RefSummaryIndexInspection,
    pub pack_files: PackFilesInspection,
    pub partial_fetch: PartialFetchInspection,
    pub pull_planner_cache: PullPlannerCacheInspection,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitGraphInspection {
    pub present: bool,
    pub node_count: usize,
    pub bloom_covered_nodes: usize,
    pub bytes: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeIndexInspection {
    pub present: bool,
    pub file_entries: usize,
    pub directory_entries: usize,
    pub untracked_directory_entries: usize,
    pub snapshot_bytes: u64,
    pub journal_bytes: u64,
    pub journal_ops: usize,
    pub journal_replay_ms: u128,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangeMonitorInspection {
    pub backend: String,
    pub status: String,
    pub reason: Option<String>,
    pub changed_path_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefCountsInspection {
    pub total: usize,
    pub threads: usize,
    pub markers: usize,
    pub remotes: usize,
    pub remote_threads: usize,
    pub packed_refs_present: bool,
    pub packed_refs_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackFilesInspection {
    pub pack_count: usize,
    pub index_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PartialFetchInspection {
    pub count: usize,
    pub missing_blob_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PullPlannerCacheInspection {
    pub status: String,
    pub present: bool,
    pub manifest_count: usize,
    pub planner_entry_count: usize,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepositoryMaintenanceRunReport {
    pub rebuilt_commit_graph: bool,
    pub rebuilt_ref_summary_index: bool,
    pub rebuilt_worktree_index: bool,
    pub refreshed_change_monitor: bool,
    pub rebuilt_pull_planner_cache: bool,
    pub pruned_pull_planner_entries: usize,
    pub report: RepositoryPerformanceInspectionReport,
}

impl Repository {
    pub fn inspect_performance(&self) -> Result<RepositoryPerformanceInspectionReport> {
        self.inspect_performance_with_options(&self.maintenance_worktree_status_options())
    }

    pub fn inspect_performance_with_options(
        &self,
        options: &WorktreeStatusOptions,
    ) -> Result<RepositoryPerformanceInspectionReport> {
        let change_monitor = self.inspect_change_monitor_with_options(options)?;
        let threads = self.refs().list_threads()?;
        let markers = self.refs().list_markers()?;
        let remotes = self.refs().list_remotes()?;
        let missing_blobs = self.missing_blobs()?;
        let remote_threads = remotes.iter().try_fold(0usize, |acc, remote| {
            Ok::<usize, objects::error::HeddleError>(
                acc + self.refs().list_remote_threads(remote)?.len(),
            )
        })?;
        let packed_refs_path = self.heddle_dir.join("refs").join("packed-refs");
        let packed_refs_bytes = file_len_or_zero(&packed_refs_path);

        Ok(RepositoryPerformanceInspectionReport {
            commit_graph: inspect_commit_graph(self.root()),
            worktree_index: inspect_worktree_index(self.root()),
            change_monitor: ChangeMonitorInspection {
                backend: change_monitor.backend,
                status: change_monitor.status,
                reason: change_monitor.reason,
                changed_path_count: change_monitor.changed_paths.len(),
            },
            ref_counts: RefCountsInspection {
                total: threads.len() + markers.len() + remote_threads,
                threads: threads.len(),
                markers: markers.len(),
                remotes: remotes.len(),
                remote_threads,
                packed_refs_present: packed_refs_path.exists(),
                packed_refs_bytes,
            },
            ref_summary_index: self.refs().inspect_ref_summary_index()?,
            pack_files: inspect_pack_files(&self.heddle_dir),
            partial_fetch: PartialFetchInspection {
                count: missing_blobs.len(),
                missing_blob_count: missing_blobs.len(),
            },
            pull_planner_cache: inspect_pull_planner_cache(self.root()),
        })
    }

    pub fn run_maintenance(&self) -> Result<RepositoryMaintenanceRunReport> {
        self.run_maintenance_with_options(&self.maintenance_worktree_status_options())
    }

    pub fn run_maintenance_with_options(
        &self,
        options: &WorktreeStatusOptions,
    ) -> Result<RepositoryMaintenanceRunReport> {
        let mut rebuilt_commit_graph = false;
        let mut rebuilt_worktree_index = false;
        let refreshed_change_monitor;

        let state_ids = self.store().list_states()?;
        if !state_ids.is_empty() {
            let mut graph = CommitGraphIndex::new(self);
            for state_id in state_ids {
                graph.ensure_loaded(state_id).map_err(anyhow_to_heddle)?;
                graph
                    .ensure_bloom_populated(state_id)
                    .map_err(anyhow_to_heddle)?;
            }
            rebuilt_commit_graph = true;
        }

        let rebuilt_ref_summary_index = {
            let ref_summary_index = self.refs().rebuild_ref_summary_index()?;
            ref_summary_index.present && ref_summary_index.valid
        };
        let pull_planner_maintenance = maintain_pull_planner_cache(self)?;

        if let Some(state) = self.current_state()? {
            let tree = self.require_tree(&state.tree)?;
            self.compare_worktree_cached_detailed_with_options(&tree, options)?;
            rebuilt_worktree_index = true;
            refreshed_change_monitor = true;
        } else {
            self.inspect_change_monitor_with_options(options)?;
            refreshed_change_monitor = true;
        }

        let report = self.inspect_performance_with_options(options)?;
        Ok(RepositoryMaintenanceRunReport {
            rebuilt_commit_graph,
            rebuilt_ref_summary_index,
            rebuilt_worktree_index,
            refreshed_change_monitor,
            rebuilt_pull_planner_cache: pull_planner_maintenance.rebuilt,
            pruned_pull_planner_entries: pull_planner_maintenance.pruned_entries,
            report,
        })
    }

    fn maintenance_worktree_status_options(&self) -> WorktreeStatusOptions {
        WorktreeStatusOptions {
            fsmonitor: FsMonitorSettings::from(self.config.worktree.fsmonitor),
        }
    }
}

fn inspect_commit_graph(repo_root: &Path) -> CommitGraphInspection {
    let path = commit_graph_path(repo_root);
    let bytes = file_len_or_zero(&path);
    match load_commit_graph(&path) {
        Ok(Some(nodes)) => CommitGraphInspection {
            present: true,
            node_count: nodes.len(),
            bloom_covered_nodes: nodes.values().filter(|node| node.bloom.is_some()).count(),
            bytes,
            error: None,
        },
        Ok(None) => CommitGraphInspection {
            present: false,
            node_count: 0,
            bloom_covered_nodes: 0,
            bytes: 0,
            error: None,
        },
        Err(error) => CommitGraphInspection {
            present: path.exists(),
            node_count: 0,
            bloom_covered_nodes: 0,
            bytes,
            error: Some(error.to_string()),
        },
    }
}

fn inspect_worktree_index(repo_root: &Path) -> WorktreeIndexInspection {
    let index_path = repo_root.join(".heddle/state").join("index.bin");
    let journal_path = repo_root.join(".heddle/state").join("index.journal");
    match WorktreeIndex::load_profiled(&index_path) {
        Ok((index, stats)) => WorktreeIndexInspection {
            present: true,
            file_entries: index.len(),
            directory_entries: index.directory_len(),
            untracked_directory_entries: index.untracked_directory_len(),
            snapshot_bytes: stats.snapshot_bytes,
            journal_bytes: stats.journal_bytes,
            journal_ops: stats.journal_ops,
            journal_replay_ms: stats.journal_replay_ms,
            error: None,
        },
        Err(_error) if !index_path.exists() => WorktreeIndexInspection {
            present: false,
            file_entries: 0,
            directory_entries: 0,
            untracked_directory_entries: 0,
            snapshot_bytes: 0,
            journal_bytes: file_len_or_zero(&journal_path),
            journal_ops: 0,
            journal_replay_ms: 0,
            error: None,
        },
        Err(error) => WorktreeIndexInspection {
            present: true,
            file_entries: 0,
            directory_entries: 0,
            untracked_directory_entries: 0,
            snapshot_bytes: file_len_or_zero(&index_path),
            journal_bytes: file_len_or_zero(&journal_path),
            journal_ops: 0,
            journal_replay_ms: 0,
            error: Some(error.to_string()),
        },
    }
}

fn inspect_pack_files(heddle_dir: &Path) -> PackFilesInspection {
    let packs_dir = heddle_dir.join("packs");
    let mut pack_count = 0usize;
    let mut index_count = 0usize;

    if let Ok(entries) = fs::read_dir(&packs_dir) {
        for entry in entries.flatten() {
            match entry.path().extension().and_then(|ext| ext.to_str()) {
                Some("pack") => pack_count += 1,
                Some("idx") => index_count += 1,
                _ => {}
            }
        }
    }

    PackFilesInspection {
        pack_count,
        index_count,
    }
}

fn inspect_pull_planner_cache(repo_root: &Path) -> PullPlannerCacheInspection {
    let pull_root = pull_planner_root(repo_root);
    let manifest_path = pull_root.join("cold-clone-manifest.json");
    let plans_dir = pull_root.join("plans");

    let mut manifest_count = 0usize;
    let mut planner_entry_count = 0usize;
    let mut total_bytes = 0u64;

    if manifest_path.exists() {
        manifest_count = 1;
        total_bytes += file_len_or_zero(&manifest_path);
    }
    if let Ok(entries) = fs::read_dir(&plans_dir) {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type()
                && file_type.is_file()
            {
                planner_entry_count += 1;
                total_bytes += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            }
        }
    }

    let present = manifest_count > 0 || planner_entry_count > 0;
    PullPlannerCacheInspection {
        status: if present {
            "present".to_string()
        } else {
            "absent".to_string()
        },
        present,
        manifest_count,
        planner_entry_count,
        total_bytes,
    }
}

#[derive(Default)]
struct PullPlannerMaintenanceRun {
    rebuilt: bool,
    pruned_entries: usize,
}

const PULL_PLANNER_SCHEMA_VERSION: u32 = 1;
const COLD_CLONE_MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum PullAvailabilityModeMirror {
    Full,
    LazyBlobOptional,
}

impl PullAvailabilityModeMirror {
    fn as_file_fragment(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::LazyBlobOptional => "lazy-blob-optional",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPullPlannerEntryMirror {
    schema_version: u32,
    generated_at: DateTime<Utc>,
    repo_path: String,
    remote_state_id: String,
    depth: Option<u32>,
    exclude_states: Vec<String>,
    availability_mode: PullAvailabilityModeMirror,
    object_count: u32,
    planned_objects: Vec<PlannedObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredColdCloneManifestMirror {
    schema_version: u32,
    generated_at: DateTime<Utc>,
    repo_path: String,
    head: HeadSnapshotMirror,
    markers: Vec<RefSnapshotMirror>,
    threads: Vec<RefSnapshotMirror>,
    thread_entries: Vec<ColdCloneThreadEntryMirror>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HeadSnapshotMirror {
    kind: String,
    value: String,
    head_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RefSnapshotMirror {
    name: String,
    state_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ColdCloneThreadEntryMirror {
    thread: String,
    state_id: String,
    planner_key_full: String,
    planner_key_lazy: String,
    object_count: u32,
    full_closure_available: bool,
}

#[derive(Clone)]
struct PullPlannerKeyMirror {
    remote_state_id: ChangeId,
    depth: Option<u32>,
    exclude_states: Vec<ChangeId>,
    availability_mode: PullAvailabilityModeMirror,
}

impl PullPlannerKeyMirror {
    fn new(
        remote_state_id: ChangeId,
        depth: Option<u32>,
        exclude_states: Vec<ChangeId>,
        availability_mode: PullAvailabilityModeMirror,
    ) -> Self {
        Self {
            remote_state_id,
            depth,
            exclude_states,
            availability_mode,
        }
    }

    fn file_name(&self) -> String {
        let depth = self
            .depth
            .map(|value| value.to_string())
            .unwrap_or_else(|| "full".to_string());
        format!(
            "{}--depth-{}--exclude-{}--{}.json",
            self.remote_state_id.to_string_full(),
            depth,
            pull_planner_exclude_fingerprint(&self.exclude_states),
            self.availability_mode.as_file_fragment()
        )
    }
}

fn maintain_pull_planner_cache(repo: &Repository) -> Result<PullPlannerMaintenanceRun> {
    let pull_root = pull_planner_root(repo.root());
    if !pull_root.exists() {
        return Ok(PullPlannerMaintenanceRun::default());
    }

    let repo_path = discover_pull_planner_repo_path(repo.root())?;
    let Some(repo_path) = repo_path else {
        let pruned_entries = prune_invalid_pull_plans(repo, None)?;
        return Ok(PullPlannerMaintenanceRun {
            rebuilt: false,
            pruned_entries,
        });
    };

    let pruned_entries = prune_invalid_pull_plans(repo, Some(&repo_path))?;
    let rebuilt = match load_pull_planner_manifest(repo.root()) {
        Ok(Some(manifest))
            if !pull_planner_manifest_needs_rebuild(repo, &repo_path, &manifest)? =>
        {
            false
        }
        _ => {
            rebuild_pull_planner_manifest(repo, &repo_path)?;
            true
        }
    };

    Ok(PullPlannerMaintenanceRun {
        rebuilt,
        pruned_entries,
    })
}

fn discover_pull_planner_repo_path(repo_root: &Path) -> Result<Option<String>> {
    if let Ok(Some(manifest)) = load_pull_planner_manifest(repo_root) {
        return Ok(Some(manifest.repo_path));
    }

    let plans_dir = pull_planner_plans_dir(repo_root);
    if !plans_dir.exists() {
        return Ok(None);
    }
    for entry in fs::read_dir(plans_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let bytes = fs::read(entry.path())?;
        if let Ok(plan) = serde_json::from_slice::<StoredPullPlannerEntryMirror>(&bytes) {
            return Ok(Some(plan.repo_path));
        }
    }
    Ok(None)
}

fn prune_invalid_pull_plans(repo: &Repository, repo_path: Option<&str>) -> Result<usize> {
    let plans_dir = pull_planner_plans_dir(repo.root());
    if !plans_dir.exists() {
        return Ok(0);
    }

    let valid_states = repo
        .store()
        .list_states()?
        .into_iter()
        .map(|id| id.to_string_full())
        .collect::<HashSet<_>>();
    let mut pruned = 0usize;

    for entry in fs::read_dir(&plans_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let remove = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<StoredPullPlannerEntryMirror>(&bytes) {
                Ok(plan) => {
                    plan.schema_version != PULL_PLANNER_SCHEMA_VERSION
                        || repo_path.is_some_and(|expected| plan.repo_path != expected)
                        || ChangeId::parse(&plan.remote_state_id).is_err()
                        || !valid_states.contains(&plan.remote_state_id)
                }
                Err(_) => true,
            },
            Err(_) => true,
        };
        if remove {
            fs::remove_file(path)?;
            pruned += 1;
        }
    }

    Ok(pruned)
}

fn pull_planner_manifest_needs_rebuild(
    repo: &Repository,
    repo_path: &str,
    manifest: &StoredColdCloneManifestMirror,
) -> Result<bool> {
    let head = repo.refs().read_head()?;
    let threads = load_ref_snapshots(repo, true)?;
    let markers = load_ref_snapshots(repo, false)?;
    if !manifest_matches(manifest, repo_path, &head, &threads, &markers) {
        return Ok(true);
    }
    if manifest.thread_entries.len() != threads.len() {
        return Ok(true);
    }
    let plans_dir = pull_planner_plans_dir(repo.root());
    for thread in &threads {
        let Some(entry) = manifest
            .thread_entries
            .iter()
            .find(|entry| entry.thread == thread.name)
        else {
            return Ok(true);
        };
        let state_id = ChangeId::parse(&thread.state_id).map_err(|err| {
            HeddleError::Io(io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
        })?;
        let full_key =
            PullPlannerKeyMirror::new(state_id, None, Vec::new(), PullAvailabilityModeMirror::Full)
                .file_name();
        let lazy_key = PullPlannerKeyMirror::new(
            state_id,
            None,
            Vec::new(),
            PullAvailabilityModeMirror::LazyBlobOptional,
        )
        .file_name();
        if entry.state_id != thread.state_id
            || entry.planner_key_full != full_key
            || entry.planner_key_lazy != lazy_key
            || !plans_dir.join(&full_key).exists()
            || !plans_dir.join(&lazy_key).exists()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn rebuild_pull_planner_manifest(repo: &Repository, repo_path: &str) -> Result<()> {
    let head = repo.refs().read_head()?;
    let threads = load_ref_snapshots(repo, true)?;
    let markers = load_ref_snapshots(repo, false)?;
    let plans_dir = pull_planner_plans_dir(repo.root());
    fs::create_dir_all(&plans_dir)?;

    let mut thread_entries = Vec::with_capacity(threads.len());
    for thread in &threads {
        let state_id = ChangeId::parse(&thread.state_id).map_err(|err| {
            HeddleError::Io(io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
        })?;
        let full_key =
            PullPlannerKeyMirror::new(state_id, None, Vec::new(), PullAvailabilityModeMirror::Full);
        let lazy_key = PullPlannerKeyMirror::new(
            state_id,
            None,
            Vec::new(),
            PullAvailabilityModeMirror::LazyBlobOptional,
        );
        let full_plan = rebuild_pull_planner_entry(repo, repo_path, &full_key)?;
        rebuild_pull_planner_entry(repo, repo_path, &lazy_key)?;
        thread_entries.push(ColdCloneThreadEntryMirror {
            thread: thread.name.clone(),
            state_id: thread.state_id.clone(),
            planner_key_full: full_key.file_name(),
            planner_key_lazy: lazy_key.file_name(),
            object_count: full_plan.object_count,
            full_closure_available: true,
        });
    }

    let manifest = StoredColdCloneManifestMirror {
        schema_version: COLD_CLONE_MANIFEST_SCHEMA_VERSION,
        generated_at: Utc::now(),
        repo_path: repo_path.to_string(),
        head: head_snapshot(&head, &threads),
        markers,
        threads,
        thread_entries,
    };
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|err| {
        HeddleError::Io(io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
    })?;
    write_file_atomic(&pull_planner_manifest_path(repo.root()), &bytes)?;
    Ok(())
}

fn rebuild_pull_planner_entry(
    repo: &Repository,
    repo_path: &str,
    key: &PullPlannerKeyMirror,
) -> Result<StoredPullPlannerEntryMirror> {
    let planned_objects = enumerate_state_closure_plan_with_options(
        repo.store(),
        key.remote_state_id,
        StateClosureOptions {
            depth: key.depth,
            exclude_states: key.exclude_states.clone(),
        },
    )
    .map_err(|err| HeddleError::Io(io::Error::new(io::ErrorKind::InvalidData, err.to_string())))?;
    let entry = StoredPullPlannerEntryMirror {
        schema_version: PULL_PLANNER_SCHEMA_VERSION,
        generated_at: Utc::now(),
        repo_path: repo_path.to_string(),
        remote_state_id: key.remote_state_id.to_string_full(),
        depth: key.depth,
        exclude_states: sorted_change_ids(&key.exclude_states),
        availability_mode: key.availability_mode,
        object_count: u32::try_from(planned_objects.len()).unwrap_or(u32::MAX),
        planned_objects,
    };
    let bytes = serde_json::to_vec_pretty(&entry).map_err(|err| {
        HeddleError::Io(io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
    })?;
    write_file_atomic(
        &pull_planner_plans_dir(repo.root()).join(key.file_name()),
        &bytes,
    )?;
    Ok(entry)
}

fn load_pull_planner_manifest(
    repo_root: &Path,
) -> io::Result<Option<StoredColdCloneManifestMirror>> {
    let path = pull_planner_manifest_path(repo_root);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    let manifest: StoredColdCloneManifestMirror = serde_json::from_slice(&bytes)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if manifest.schema_version != COLD_CLONE_MANIFEST_SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported cold clone manifest schema version {}",
                manifest.schema_version
            ),
        ));
    }
    Ok(Some(manifest))
}

fn pull_planner_root(repo_root: &Path) -> PathBuf {
    repo_root
        .join(".heddle/state")
        .join("derived-summaries")
        .join("pull")
}

fn pull_planner_manifest_path(repo_root: &Path) -> PathBuf {
    pull_planner_root(repo_root).join("cold-clone-manifest.json")
}

fn pull_planner_plans_dir(repo_root: &Path) -> PathBuf {
    pull_planner_root(repo_root).join("plans")
}

fn sorted_change_ids(ids: &[ChangeId]) -> Vec<String> {
    let mut values = ids.iter().map(ChangeId::to_string_full).collect::<Vec<_>>();
    values.sort();
    values
}

fn pull_planner_exclude_fingerprint(ids: &[ChangeId]) -> String {
    let joined = sorted_change_ids(ids).join("\n");
    objects::object::ContentHash::compute(joined.as_bytes())
        .to_hex()
        .chars()
        .take(16)
        .collect()
}

fn load_ref_snapshots(repo: &Repository, threads: bool) -> Result<Vec<RefSnapshotMirror>> {
    let names = if threads {
        repo.refs().list_threads()?
    } else {
        repo.refs().list_markers()?
    };
    let mut snapshots = Vec::with_capacity(names.len());
    for name in names {
        let state = if threads {
            repo.refs().get_thread(&name)?
        } else {
            repo.refs().get_marker(&name)?
        }
        .ok_or_else(|| {
            HeddleError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} '{}' disappeared while rebuilding pull planner manifest",
                    if threads { "thread" } else { "marker" },
                    name
                ),
            ))
        })?;
        snapshots.push(RefSnapshotMirror {
            name,
            state_id: state.to_string_full(),
        });
    }
    snapshots.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(snapshots)
}

fn head_snapshot(head: &Head, threads: &[RefSnapshotMirror]) -> HeadSnapshotMirror {
    match head {
        Head::Attached { thread } => HeadSnapshotMirror {
            kind: "attached".to_string(),
            value: thread.clone(),
            head_state: threads
                .iter()
                .find(|snapshot| snapshot.name == *thread)
                .map(|snapshot| snapshot.state_id.clone()),
        },
        Head::Detached { state } => HeadSnapshotMirror {
            kind: "detached".to_string(),
            value: state.to_string_full(),
            head_state: Some(state.to_string_full()),
        },
    }
}

fn manifest_matches(
    manifest: &StoredColdCloneManifestMirror,
    repo_path: &str,
    head: &Head,
    threads: &[RefSnapshotMirror],
    markers: &[RefSnapshotMirror],
) -> bool {
    manifest.repo_path == repo_path
        && manifest.head == head_snapshot(head, threads)
        && manifest.threads == threads
        && manifest.markers == markers
}

fn file_len_or_zero(path: &Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn anyhow_to_heddle(error: anyhow::Error) -> HeddleError {
    HeddleError::Io(std::io::Error::other(error.to_string()))
}