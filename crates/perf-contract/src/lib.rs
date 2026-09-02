// SPDX-License-Identifier: Apache-2.0
//! Process-local structural counters for release performance contracts.

use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};

static ENABLED: OnceLock<bool> = OnceLock::new();
static DIRECTORIES_SCANNED: AtomicU64 = AtomicU64::new(0);
static DIRECTORIES_SKIPPED: AtomicU64 = AtomicU64::new(0);
static FILES_HASHED: AtomicU64 = AtomicU64::new(0);
static MONITOR_CHANGED_PATHS: AtomicU64 = AtomicU64::new(0);
static MONITOR_STARTUP_MS: AtomicU64 = AtomicU64::new(0);
static OBJECT_DECODES: AtomicU64 = AtomicU64::new(0);
static REF_READS: AtomicU64 = AtomicU64::new(0);
static OPLOG_READS: AtomicU64 = AtomicU64::new(0);
static REPOSITORY_OPENS: AtomicU64 = AtomicU64::new(0);
static NETWORK_CLIENT_INITIALIZATIONS: AtomicU64 = AtomicU64::new(0);
static ANCESTORS_VISITED: AtomicU64 = AtomicU64::new(0);
static HISTORY_OBJECTS_DECODED: AtomicU64 = AtomicU64::new(0);
static GIT_REACHABLE_COPY_OPERATIONS: AtomicU64 = AtomicU64::new(0);
static PACK_FRAME_DECOMPRESSIONS: AtomicU64 = AtomicU64::new(0);
static PACK_FRAME_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static PACK_BLOB_BODIES_HASHED: AtomicU64 = AtomicU64::new(0);
static PACK_STATE_FRAMES_DECODED: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StructuralCounters {
    pub directories_scanned: u64,
    pub directories_skipped: u64,
    pub files_hashed: u64,
    pub monitor_changed_paths: u64,
    pub monitor_startup_ms: u64,
    pub object_decodes: u64,
    pub ref_reads: u64,
    pub oplog_reads: u64,
    pub repository_opens: u64,
    pub network_client_initialized: bool,
    pub ancestors_visited: u64,
    pub history_objects_decoded: u64,
    pub git_reachable_copy_operations: u64,
    pub pack_frame_decompressions: u64,
    pub pack_frame_cache_hits: u64,
    pub pack_blob_bodies_hashed: u64,
    pub pack_state_frames_decoded: u64,
}

fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var("HEDDLE_PROFILE").is_ok_and(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no" | "off"
            )
        })
    })
}

fn add(counter: &AtomicU64, value: u64) {
    if enabled() {
        counter.fetch_add(value, Ordering::Relaxed);
    }
}

pub fn record_worktree_scan(
    directories_scanned: u64,
    directories_skipped: u64,
    files_hashed: u64,
    monitor_changed_paths: u64,
    monitor_startup_ms: u64,
) {
    add(&DIRECTORIES_SCANNED, directories_scanned);
    add(&DIRECTORIES_SKIPPED, directories_skipped);
    add(&FILES_HASHED, files_hashed);
    add(&MONITOR_CHANGED_PATHS, monitor_changed_paths);
    add(&MONITOR_STARTUP_MS, monitor_startup_ms);
}

pub fn record_object_decode() {
    add(&OBJECT_DECODES, 1);
}

pub fn record_ref_read() {
    add(&REF_READS, 1);
}

pub fn record_oplog_read() {
    add(&OPLOG_READS, 1);
}

pub fn record_repository_open() {
    add(&REPOSITORY_OPENS, 1);
}

pub fn record_network_client_initialization() {
    add(&NETWORK_CLIENT_INITIALIZATIONS, 1);
}

pub fn record_ancestors_visited(ancestors_visited: u64) {
    add(&ANCESTORS_VISITED, ancestors_visited);
}

pub fn record_history_object_decode() {
    add(&HISTORY_OBJECTS_DECODED, 1);
}

pub fn record_git_reachable_copy_operation() {
    add(&GIT_REACHABLE_COPY_OPERATIONS, 1);
}

pub fn record_pack_frame_decompression() {
    add(&PACK_FRAME_DECOMPRESSIONS, 1);
}

pub fn record_pack_frame_cache_hit() {
    add(&PACK_FRAME_CACHE_HITS, 1);
}

pub fn record_pack_blob_bodies_hashed(count: usize) {
    add(&PACK_BLOB_BODIES_HASHED, count as u64);
}

pub fn record_pack_state_frame_decode() {
    add(&PACK_STATE_FRAMES_DECODED, 1);
}

pub fn snapshot() -> StructuralCounters {
    StructuralCounters {
        directories_scanned: DIRECTORIES_SCANNED.load(Ordering::Relaxed),
        directories_skipped: DIRECTORIES_SKIPPED.load(Ordering::Relaxed),
        files_hashed: FILES_HASHED.load(Ordering::Relaxed),
        monitor_changed_paths: MONITOR_CHANGED_PATHS.load(Ordering::Relaxed),
        monitor_startup_ms: MONITOR_STARTUP_MS.load(Ordering::Relaxed),
        object_decodes: OBJECT_DECODES.load(Ordering::Relaxed),
        ref_reads: REF_READS.load(Ordering::Relaxed),
        oplog_reads: OPLOG_READS.load(Ordering::Relaxed),
        repository_opens: REPOSITORY_OPENS.load(Ordering::Relaxed),
        network_client_initialized: NETWORK_CLIENT_INITIALIZATIONS.load(Ordering::Relaxed) > 0,
        ancestors_visited: ANCESTORS_VISITED.load(Ordering::Relaxed),
        history_objects_decoded: HISTORY_OBJECTS_DECODED.load(Ordering::Relaxed),
        git_reachable_copy_operations: GIT_REACHABLE_COPY_OPERATIONS.load(Ordering::Relaxed),
        pack_frame_decompressions: PACK_FRAME_DECOMPRESSIONS.load(Ordering::Relaxed),
        pack_frame_cache_hits: PACK_FRAME_CACHE_HITS.load(Ordering::Relaxed),
        pack_blob_bodies_hashed: PACK_BLOB_BODIES_HASHED.load(Ordering::Relaxed),
        pack_state_frames_decoded: PACK_STATE_FRAMES_DECODED.load(Ordering::Relaxed),
    }
}
