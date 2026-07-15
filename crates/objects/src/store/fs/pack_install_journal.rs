// SPDX-License-Identifier: Apache-2.0
//! L8 A+ pack install journal: durable staging + intent (crash-safe install).
//!
//! # Layout under `.heddle/packs/`
//! ```text
//! packs/
//!   <blake3-hex>.pack
//!   <blake3-hex>.idx
//!   .staging/<install_id>/{pack,idx}
//!   .install-intent/<install_id>.json   # identifiers only (v2)
//!   .install-intent/quarantine/         # malformed / unknown-version intents
//!   .pack-install.lock
//! ```
//!
//! # Intent (v2)
//! Persists only `install_id`, `pack_name`, `phase`, `created_unix`.
//! All paths are reconstructed from a trusted `packs_dir` — never executed
//! from JSON (Codex review: path containment).
//!
//! # Protocol
//! 1. Stage pack+idx under `.staging/<id>/` (outside the per-pack lock).
//! 2. Take per-`pack_name` exclusive lock; write one durable **prepared** intent.
//! 3. Publish pack → update intent to **pack_published**.
//! 4. Publish index → **remove** intent (no Completed rewrite).
//! 5. Best-effort remove staging; fsync intent dir after intent unlink.
//!
//! Recovery lists intents under a short global listing lock, then recovers each
//! pack under `try_lock` on that pack (skip if a live install holds it).
//! Paths are reconstructed, IDs validated; garbage intents are quarantined.
//! See `docs/program/L8_PACK_INSTALL_JOURNAL.md`.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    fault_inject,
    fs_atomic::{create_dir_all_durable, publish_file_durable, sync_directory, write_file_atomic},
    lock::RepoLock,
};

/// Intent schema version (v2 = identifiers only; paths reconstructed).
pub const PACK_INSTALL_INTENT_VERSION: u32 = 2;

/// Default TTL for abandoned install intents / orphan staging (24 hours).
pub const DEFAULT_PACK_INSTALL_INTENT_TTL_SECS: i64 = 86_400;

/// Tolerate clocks slightly ahead of wall time when computing TTL expiry.
/// Far-future `created_unix` is clamped to `now` so intents cannot dodge expiry forever.
pub const INTENT_CLOCK_SKEW_TOLERANCE_SECS: i64 = 300;

const STAGING_DIR_NAME: &str = ".staging";
const INTENT_DIR_NAME: &str = ".install-intent";
const QUARANTINE_DIR_NAME: &str = "quarantine";
const STAGED_PACK_NAME: &str = "pack";
const STAGED_IDX_NAME: &str = "idx";
const PACK_LOCKS_DIR_NAME: &str = ".pack-locks";
/// Legacy global lock name (kept for recover directory scan serialization).
const PACK_INSTALL_LOCK_NAME: &str = ".pack-install.lock";

// ---------------------------------------------------------------------------
// Process-local metrics (hosted/adapters can scrape; not a full product pipeline)
// ---------------------------------------------------------------------------

static METRIC_INSTALLS_OK: AtomicU64 = AtomicU64::new(0);
static METRIC_INSTALLS_ERR: AtomicU64 = AtomicU64::new(0);
static METRIC_RECOVER_COMPLETED: AtomicU64 = AtomicU64::new(0);
static METRIC_RECOVER_ABORTED: AtomicU64 = AtomicU64::new(0);
static METRIC_RECOVER_SKIPPED: AtomicU64 = AtomicU64::new(0);
static METRIC_RECOVER_QUARANTINED: AtomicU64 = AtomicU64::new(0);

/// Snapshot of process-local pack-install counters (resettable in tests).
///
/// Hosted / maintenance adapters scrape this; it is not a full product metrics
/// pipeline, but it is the stable hook surface for recover/install observability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackInstallMetricsSnapshot {
    pub installs_ok: u64,
    pub installs_err: u64,
    pub recover_completed: u64,
    pub recover_aborted: u64,
    pub recover_skipped_in_progress: u64,
    pub recover_quarantined: u64,
}

/// Read process-local pack-install metrics.
pub fn pack_install_metrics_snapshot() -> PackInstallMetricsSnapshot {
    PackInstallMetricsSnapshot {
        installs_ok: METRIC_INSTALLS_OK.load(Ordering::Relaxed),
        installs_err: METRIC_INSTALLS_ERR.load(Ordering::Relaxed),
        recover_completed: METRIC_RECOVER_COMPLETED.load(Ordering::Relaxed),
        recover_aborted: METRIC_RECOVER_ABORTED.load(Ordering::Relaxed),
        recover_skipped_in_progress: METRIC_RECOVER_SKIPPED.load(Ordering::Relaxed),
        recover_quarantined: METRIC_RECOVER_QUARANTINED.load(Ordering::Relaxed),
    }
}

/// Reset process-local metrics (tests / process start hooks).
pub fn pack_install_metrics_reset() {
    METRIC_INSTALLS_OK.store(0, Ordering::Relaxed);
    METRIC_INSTALLS_ERR.store(0, Ordering::Relaxed);
    METRIC_RECOVER_COMPLETED.store(0, Ordering::Relaxed);
    METRIC_RECOVER_ABORTED.store(0, Ordering::Relaxed);
    METRIC_RECOVER_SKIPPED.store(0, Ordering::Relaxed);
    METRIC_RECOVER_QUARANTINED.store(0, Ordering::Relaxed);
}

fn metric_inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Install lifecycle phase recorded in the durable intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackInstallPhase {
    Prepared,
    PackPublished,
    /// Legacy; never written by v2 install. Recovery treats as cleanup.
    #[serde(other)]
    Completed,
}

/// Durable intent for a single pack+index install (identifiers only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackInstallIntent {
    pub version: u32,
    pub install_id: String,
    /// Content-addressed pack stem (blake3 hex of pack bytes).
    pub pack_name: String,
    pub phase: PackInstallPhase,
    pub created_unix: i64,
}

/// Summary of recovery work performed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackInstallRecoverReport {
    pub intents_seen: u64,
    pub completed: u64,
    pub aborted: u64,
    pub cleaned_stale_completed: u64,
    /// Non-expired intents left alone (likely a concurrent live install).
    pub skipped_in_progress: u64,
    /// Orphan `.staging/<id>` directories removed (no matching intent, past TTL).
    pub orphan_staging_swept: u64,
    /// Malformed / unknown-version intents moved to quarantine.
    pub quarantined: u64,
    pub errors: u64,
}

/// Per-`pack_name` exclusive lock (cross-thread + cross-process).
pub(crate) fn acquire_pack_name_lock(
    packs_dir: &Path,
    pack_name: &str,
) -> io::Result<crate::lock::WriteLockGuard> {
    validate_pack_name(pack_name)?;
    create_dir_all_durable(packs_dir)?;
    let locks = packs_dir.join(PACK_LOCKS_DIR_NAME);
    create_dir_all_durable(&locks)?;
    let lock = RepoLock::at(locks.join(format!("{pack_name}.lock")));
    lock.write().map_err(|e| io::Error::other(e.to_string()))
}

/// Non-blocking per-pack lock. `None` = another install holds this pack.
pub(crate) fn try_acquire_pack_name_lock(
    packs_dir: &Path,
    pack_name: &str,
) -> io::Result<Option<crate::lock::WriteLockGuard>> {
    validate_pack_name(pack_name)?;
    create_dir_all_durable(packs_dir)?;
    let locks = packs_dir.join(PACK_LOCKS_DIR_NAME);
    create_dir_all_durable(&locks)?;
    let lock = RepoLock::at(locks.join(format!("{pack_name}.lock")));
    lock.try_write()
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Short global listing lock (intent dir scan only).
pub(crate) fn acquire_pack_install_lock(
    packs_dir: &Path,
) -> io::Result<crate::lock::WriteLockGuard> {
    create_dir_all_durable(packs_dir)?;
    let lock = RepoLock::at(packs_dir.join(PACK_INSTALL_LOCK_NAME));
    lock.write().map_err(|e| io::Error::other(e.to_string()))
}

impl PackInstallIntent {
    pub fn new(install_id: String, pack_name: String) -> Self {
        Self {
            version: PACK_INSTALL_INTENT_VERSION,
            install_id,
            pack_name,
            phase: PackInstallPhase::Prepared,
            created_unix: unix_now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Path reconstruction (trusted packs_dir only)
// ---------------------------------------------------------------------------

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Single path component: no separators, no `..`, no empty, limited charset.
pub(crate) fn validate_install_id(id: &str) -> io::Result<()> {
    if id.is_empty() || id.len() > 128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid install_id length",
        ));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "install_id contains illegal characters",
        ));
    }
    if id == "." || id == ".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "install_id must not be . or ..",
        ));
    }
    Ok(())
}

/// Content-addressed pack stem: exactly 64 lowercase hex digits (BLAKE3).
pub(crate) fn validate_pack_name(name: &str) -> io::Result<()> {
    pack_name_to_digest(name).map(|_| ())
}

/// Decode a validated `pack_name` to the native 32-byte BLAKE3 digest.
///
/// Prefer comparing digests over hex strings once past the FS/JSON boundary.
pub(crate) fn pack_name_to_digest(name: &str) -> io::Result<[u8; 32]> {
    if name.len() != 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pack_name must be exactly 64 lowercase hex digits (BLAKE3)",
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pack_name must be lowercase hexadecimal",
        ));
    }
    let mut digest = [0u8; 32];
    // decode_to_slice avoids an intermediate Vec; name is already length-checked.
    hex::decode_to_slice(name.as_bytes(), &mut digest).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("pack_name hex decode failed: {e}"),
        )
    })?;
    Ok(digest)
}

/// Hex form of a BLAKE3 digest for filenames / intent JSON (one allocation).
fn digest_to_pack_name(digest: &[u8; 32]) -> String {
    // blake3::Hash::to_hex is stack; to_string once at the FS boundary.
    blake3::Hash::from_bytes(*digest).to_hex().to_string()
}

fn validate_intent_ids(intent: &PackInstallIntent) -> io::Result<()> {
    validate_install_id(&intent.install_id)?;
    pack_name_to_digest(&intent.pack_name)?;
    Ok(())
}

pub(crate) fn staging_root(packs_dir: &Path) -> PathBuf {
    packs_dir.join(STAGING_DIR_NAME)
}

pub(crate) fn intent_root(packs_dir: &Path) -> PathBuf {
    packs_dir.join(INTENT_DIR_NAME)
}

fn quarantine_root(packs_dir: &Path) -> PathBuf {
    intent_root(packs_dir).join(QUARANTINE_DIR_NAME)
}

pub(crate) fn intent_path(packs_dir: &Path, install_id: &str) -> PathBuf {
    intent_root(packs_dir).join(format!("{install_id}.json"))
}

pub(crate) fn staging_dir(packs_dir: &Path, install_id: &str) -> PathBuf {
    staging_root(packs_dir).join(install_id)
}

fn staging_pack_path(packs_dir: &Path, install_id: &str) -> PathBuf {
    staging_dir(packs_dir, install_id).join(STAGED_PACK_NAME)
}

fn staging_idx_path(packs_dir: &Path, install_id: &str) -> PathBuf {
    staging_dir(packs_dir, install_id).join(STAGED_IDX_NAME)
}

fn dst_pack_path(packs_dir: &Path, pack_name: &str) -> PathBuf {
    packs_dir.join(format!("{pack_name}.pack"))
}

fn dst_idx_path(packs_dir: &Path, pack_name: &str) -> PathBuf {
    packs_dir.join(format!("{pack_name}.idx"))
}

/// True when `path` is `root` or a descendant (component-wise).
fn path_is_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// Ensure `candidate` cannot resolve outside `packs_dir`.
///
/// **Canonical containment is authoritative.** A lexical
/// `candidate.starts_with(packs_dir)` must never override a canonical
/// escape (classic case: `.staging` → symlink to `/tmp/evil`).
///
/// Walks every existing path component (including intermediate symlinks)
/// and rejects any prefix whose `canonicalize` leaves `packs_dir`.
/// Nonexistent trailing components are allowed only under a trusted base.
fn assert_under_packs(packs_dir: &Path, candidate: &Path) -> io::Result<()> {
    if !packs_dir.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "packs_dir does not exist for path containment check",
        ));
    }
    let packs_canon = fs::canonicalize(packs_dir)?;

    let rel = candidate.strip_prefix(packs_dir).or_else(|_| {
        candidate
            .strip_prefix(&packs_canon)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path is not under packs_dir"))
    })?;

    for c in rel.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path escapes packs_dir",
                ));
            }
        }
    }

    let mut cur = packs_canon.clone();
    for c in rel.components() {
        let Component::Normal(name) = c else {
            continue;
        };
        cur.push(name);
        // symlink_metadata succeeds for files, dirs, and (possibly broken) symlinks.
        match cur.symlink_metadata() {
            Ok(_) => {
                let canon = fs::canonicalize(&cur).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!(
                            "cannot resolve path under packs_dir ({}): {e}",
                            cur.display()
                        ),
                    )
                })?;
                if !path_is_within(&canon, &packs_canon) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "reconstructed path escapes packs_dir via symlink or mount",
                    ));
                }
                // Continue from resolved location so nested escapes are still caught.
                cur = canon;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Remaining names are pure (already validated as Normal components).
                break;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Reject hostile journal roots (`.staging`, `.install-intent`, `.pack-locks`
/// as symlinks/mounts that escape `packs_dir`).
fn ensure_journal_layout_safe(packs_dir: &Path) -> io::Result<()> {
    create_dir_all_durable(packs_dir)?;
    for name in [STAGING_DIR_NAME, INTENT_DIR_NAME, PACK_LOCKS_DIR_NAME] {
        let p = packs_dir.join(name);
        if p.symlink_metadata().is_ok() {
            assert_under_packs(packs_dir, &p)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Intent I/O
// ---------------------------------------------------------------------------

pub(crate) fn write_intent(packs_dir: &Path, intent: &PackInstallIntent) -> io::Result<()> {
    validate_intent_ids(intent)?;
    create_dir_all_durable(&intent_root(packs_dir))?;
    let path = intent_path(packs_dir, &intent.install_id);
    let bytes = serde_json::to_vec_pretty(intent)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_file_atomic(&path, &bytes)
}

pub(crate) fn load_intent(path: &Path) -> io::Result<PackInstallIntent> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub(crate) fn remove_intent(packs_dir: &Path, install_id: &str) -> io::Result<()> {
    validate_install_id(install_id)?;
    let path = intent_path(packs_dir, install_id);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // Make intent-dirent removal more durable (Codex #6).
    let _ = sync_directory(&intent_root(packs_dir));
    Ok(())
}

fn quarantine_intent_file(packs_dir: &Path, path: &Path) -> io::Result<()> {
    let qroot = quarantine_root(packs_dir);
    create_dir_all_durable(&qroot)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("intent.json");
    let dest = qroot.join(format!("{name}.{}.bad", unix_now()));
    match fs::rename(path, &dest) {
        Ok(()) => {
            let _ = sync_directory(&qroot);
            tracing::warn!(
                from = %path.display(),
                to = %dest.display(),
                "quarantined unreadable pack install intent"
            );
            Ok(())
        }
        Err(e) => {
            // If rename fails, leave original in place (do not delete).
            Err(e)
        }
    }
}

fn remove_path_best_effort(path: &Path) {
    if path.is_dir() {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

pub(crate) fn remove_staging(packs_dir: &Path, install_id: &str) {
    if validate_install_id(install_id).is_err() {
        return;
    }
    remove_path_best_effort(&staging_dir(packs_dir, install_id));
}

// ---------------------------------------------------------------------------
// Abort / complete (paths always from packs_dir + ids)
// ---------------------------------------------------------------------------

pub(crate) fn abort_install(packs_dir: &Path, intent: &PackInstallIntent) -> io::Result<()> {
    validate_intent_ids(intent)?;
    let dst_pack = dst_pack_path(packs_dir, &intent.pack_name);
    let dst_idx = dst_idx_path(packs_dir, &intent.pack_name);
    assert_under_packs(packs_dir, &dst_pack)?;
    assert_under_packs(packs_dir, &dst_idx)?;

    // Only remove final pack if index is missing (partial publish).
    if dst_pack.exists() && !dst_idx.exists() {
        let _ = fs::remove_file(&dst_pack);
        if let Some(parent) = dst_pack.parent() {
            let _ = sync_directory(parent);
        }
    }
    remove_staging(packs_dir, &intent.install_id);
    remove_intent(packs_dir, &intent.install_id)?;
    Ok(())
}

pub(crate) fn complete_from_staging(
    packs_dir: &Path,
    intent: &PackInstallIntent,
) -> io::Result<()> {
    validate_intent_ids(intent)?;
    let staging_idx = staging_idx_path(packs_dir, &intent.install_id);
    let dst_idx = dst_idx_path(packs_dir, &intent.pack_name);
    let dst_pack = dst_pack_path(packs_dir, &intent.pack_name);
    assert_under_packs(packs_dir, &staging_idx)?;
    assert_under_packs(packs_dir, &dst_idx)?;
    assert_under_packs(packs_dir, &dst_pack)?;

    if dst_pack.exists() && dst_idx.exists() {
        remove_staging(packs_dir, &intent.install_id);
        remove_intent(packs_dir, &intent.install_id)?;
        return Ok(());
    }

    if !dst_pack.exists() || !staging_idx.exists() {
        return abort_install(packs_dir, intent);
    }

    publish_file_durable(&staging_idx, &dst_idx)?;
    remove_staging(packs_dir, &intent.install_id);
    remove_intent(packs_dir, &intent.install_id)?;
    Ok(())
}

fn can_complete_quickly(packs_dir: &Path, intent: &PackInstallIntent) -> bool {
    let dst_pack = dst_pack_path(packs_dir, &intent.pack_name);
    let dst_idx = dst_idx_path(packs_dir, &intent.pack_name);
    let staging_idx = staging_idx_path(packs_dir, &intent.install_id);
    if dst_pack.exists() && dst_idx.exists() {
        return true;
    }
    dst_pack.exists() && !dst_idx.exists() && staging_idx.exists()
}

/// Effective creation time for TTL when `created_unix` is only slightly ahead
/// of wall time (within [`INTENT_CLOCK_SKEW_TOLERANCE_SECS`]).
fn effective_created_unix(created_unix: i64, now: i64) -> i64 {
    if created_unix > now {
        now
    } else {
        created_unix
    }
}

fn intent_expired(intent: &PackInstallIntent, ttl_secs: Option<i64>, now: i64) -> bool {
    match ttl_secs {
        Some(ttl) if ttl >= 0 => {
            // Far-future / large clock rollback: expire immediately so a forged
            // `created_unix = i64::MAX` (or a multi-minute clock jump) cannot
            // keep an intent alive forever. Mild skew is clamped to `now`.
            if intent.created_unix > now.saturating_add(INTENT_CLOCK_SKEW_TOLERANCE_SECS) {
                return true;
            }
            let created = effective_created_unix(intent.created_unix, now);
            created.saturating_add(ttl) < now
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Existing pair identity validation
// ---------------------------------------------------------------------------

/// Stream-hash a pack file to a native 32-byte BLAKE3 digest (no hex).
fn hash_file_blake3(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// True when a final pack+idx pair is safe to treat as already installed:
/// pack content BLAKE3 equals `pack_name`, and the index **parses** as a
/// [`crate::store::pack::PackIndex`].
///
/// Identity is checked as **native digests** (`[u8; 32]`), not hex strings.
/// Hex is only the durable/public form of `pack_name` on disk.
///
/// This does **not** prove every index offset points at a live object in the
/// pack (that is the pack reader's job on first use). It rejects empty,
/// garbage, and structurally invalid indexes so install idempotency cannot
/// accept a corrupt pair.
/// String-name entry for tests and external call sites that only have hex.
#[cfg(test)]
fn existing_pair_matches_pack_name(packs_dir: &Path, pack_name: &str) -> io::Result<bool> {
    let expected = pack_name_to_digest(pack_name)?;
    existing_pair_matches_digest(packs_dir, pack_name, &expected)
}

/// Like [`existing_pair_matches_pack_name`], but reuses an already-decoded digest
/// (install hot path: hash once in memory, compare file digest to those bytes).
fn existing_pair_matches_digest(
    packs_dir: &Path,
    pack_name: &str,
    expected: &[u8; 32],
) -> io::Result<bool> {
    let pack = dst_pack_path(packs_dir, pack_name);
    let idx = dst_idx_path(packs_dir, pack_name);
    assert_under_packs(packs_dir, &pack)?;
    assert_under_packs(packs_dir, &idx)?;
    if !pack.exists() || !idx.exists() {
        return Ok(false);
    }
    // Symlink destinations must stay under packs (assert_under_packs) and the
    // open path must be a regular file for install identity.
    if !pack.is_file() || !idx.is_file() {
        return Ok(false);
    }
    if idx.metadata()?.len() == 0 {
        return Ok(false);
    }
    let actual = hash_file_blake3(&pack)?;
    if actual != *expected {
        return Ok(false);
    }
    let idx_bytes = fs::read(&idx)?;
    match crate::store::pack::PackIndex::from_bytes(&idx_bytes) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

pub fn recover_pack_install_intents(packs_dir: &Path) -> io::Result<PackInstallRecoverReport> {
    recover_pack_install_intents_with_ttl(packs_dir, Some(DEFAULT_PACK_INSTALL_INTENT_TTL_SECS))
}

pub fn recover_pack_install_intents_with_ttl(
    packs_dir: &Path,
    ttl_secs: Option<i64>,
) -> io::Result<PackInstallRecoverReport> {
    // Reject hostile journal roots (symlink escape) before any mutation.
    ensure_journal_layout_safe(packs_dir)?;

    // Snapshot intent paths under a short global listing lock, then recover
    // each pack under its per-pack lock (try_lock → skip if install holds it).
    let intent_paths: Vec<PathBuf> = {
        let _list_guard = acquire_pack_install_lock(packs_dir)?;
        let intent_dir = intent_root(packs_dir);
        if !intent_dir.exists() {
            let mut report = PackInstallRecoverReport::default();
            let now = unix_now();
            sweep_orphan_staging(packs_dir, ttl_secs, now, &mut report);
            return Ok(report);
        }
        fs::read_dir(&intent_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| !p.is_dir() && p.extension().and_then(|x| x.to_str()) == Some("json"))
            .collect()
    };

    let mut report = PackInstallRecoverReport::default();
    let now = unix_now();

    for path in intent_paths {
        report.intents_seen += 1;

        let intent = match load_intent(&path) {
            Ok(i)
                if i.version == PACK_INSTALL_INTENT_VERSION
                    || i.version == 1 /* v1 ids still usable; paths ignored */ =>
            {
                match validate_intent_ids(&i) {
                    Ok(()) => i,
                    Err(_) => {
                        let _ = quarantine_intent_file(packs_dir, &path);
                        report.quarantined += 1;
                        metric_inc(&METRIC_RECOVER_QUARANTINED);
                        continue;
                    }
                }
            }
            Ok(_) | Err(_) => {
                match quarantine_intent_file(packs_dir, &path) {
                    Ok(()) => {
                        report.quarantined += 1;
                        metric_inc(&METRIC_RECOVER_QUARANTINED);
                    }
                    Err(_) => report.errors += 1,
                }
                continue;
            }
        };

        // Per-pack try-lock: if a live install holds it, skip (in progress).
        let pack_guard = match try_acquire_pack_name_lock(packs_dir, &intent.pack_name)? {
            Some(g) => g,
            None => {
                report.skipped_in_progress += 1;
                metric_inc(&METRIC_RECOVER_SKIPPED);
                continue;
            }
        };

        let expired = intent_expired(&intent, ttl_secs, now);
        let before_aborted = report.aborted;
        let before_completed = report.completed;
        let before_skipped = report.skipped_in_progress;
        if recover_one_intent(packs_dir, &intent, expired, &mut report).is_err() {
            report.errors += 1;
        }
        if report.completed > before_completed {
            metric_inc(&METRIC_RECOVER_COMPLETED);
        }
        if report.aborted > before_aborted {
            metric_inc(&METRIC_RECOVER_ABORTED);
        }
        if report.skipped_in_progress > before_skipped {
            metric_inc(&METRIC_RECOVER_SKIPPED);
        }
        drop(pack_guard);
    }

    sweep_orphan_staging(packs_dir, ttl_secs, now, &mut report);

    if report.intents_seen > 0
        || report.orphan_staging_swept > 0
        || report.errors > 0
        || report.completed > 0
        || report.aborted > 0
        || report.skipped_in_progress > 0
        || report.quarantined > 0
    {
        tracing::info!(
            ?packs_dir,
            intents_seen = report.intents_seen,
            completed = report.completed,
            aborted = report.aborted,
            skipped_in_progress = report.skipped_in_progress,
            cleaned_stale_completed = report.cleaned_stale_completed,
            orphan_staging_swept = report.orphan_staging_swept,
            quarantined = report.quarantined,
            errors = report.errors,
            metrics = ?pack_install_metrics_snapshot(),
            "pack install journal recovery"
        );
    } else {
        tracing::debug!(?packs_dir, "pack install journal recovery: nothing to do");
    }

    Ok(report)
}

fn recover_one_intent(
    packs_dir: &Path,
    intent: &PackInstallIntent,
    expired: bool,
    report: &mut PackInstallRecoverReport,
) -> io::Result<()> {
    if can_complete_quickly(packs_dir, intent) {
        return match intent.phase {
            PackInstallPhase::Prepared | PackInstallPhase::PackPublished => {
                let dst_pack = dst_pack_path(packs_dir, &intent.pack_name);
                let dst_idx = dst_idx_path(packs_dir, &intent.pack_name);
                if dst_pack.exists() && dst_idx.exists() {
                    remove_staging(packs_dir, &intent.install_id);
                    remove_intent(packs_dir, &intent.install_id)?;
                    report.cleaned_stale_completed += 1;
                    Ok(())
                } else {
                    complete_from_staging(packs_dir, intent)?;
                    if dst_pack_path(packs_dir, &intent.pack_name).exists()
                        && dst_idx_path(packs_dir, &intent.pack_name).exists()
                    {
                        report.completed += 1;
                    } else {
                        report.aborted += 1;
                    }
                    Ok(())
                }
            }
            PackInstallPhase::Completed => {
                remove_staging(packs_dir, &intent.install_id);
                remove_intent(packs_dir, &intent.install_id)?;
                report.cleaned_stale_completed += 1;
                Ok(())
            }
        };
    }

    if expired {
        tracing::debug!(
            install_id = %intent.install_id,
            pack_name = %intent.pack_name,
            phase = ?intent.phase,
            "aborting expired pack install intent"
        );
        abort_install(packs_dir, intent)?;
        report.aborted += 1;
        return Ok(());
    }

    match intent.phase {
        PackInstallPhase::Prepared | PackInstallPhase::PackPublished => {
            report.skipped_in_progress += 1;
            Ok(())
        }
        PackInstallPhase::Completed => {
            remove_staging(packs_dir, &intent.install_id);
            remove_intent(packs_dir, &intent.install_id)?;
            report.cleaned_stale_completed += 1;
            Ok(())
        }
    }
}

fn sweep_orphan_staging(
    packs_dir: &Path,
    ttl_secs: Option<i64>,
    now: i64,
    report: &mut PackInstallRecoverReport,
) {
    let Some(ttl) = ttl_secs.filter(|t| *t >= 0) else {
        return;
    };
    let staging = staging_root(packs_dir);
    let entries = match fs::read_dir(&staging) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if validate_install_id(id).is_err() {
            continue;
        }
        if intent_path(packs_dir, id).exists() {
            continue;
        }
        let age_ok_to_sweep = path_mtime_unix(&path)
            .map(|mtime| mtime.saturating_add(ttl) < now)
            .unwrap_or(true);
        if !age_ok_to_sweep {
            continue;
        }
        remove_path_best_effort(&path);
        report.orphan_staging_swept += 1;
    }
}

fn path_mtime_unix(path: &Path) -> Option<i64> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    Some(
        modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    )
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

fn new_install_id() -> String {
    let t = unix_now() as u64;
    let r: u64 = rand::random();
    format!("{t:016x}-{r:016x}")
}

/// Journaled in-memory install. Returns content-addressed `pack_name`.
pub fn install_pack_bytes_journaled(
    packs_dir: &Path,
    pack_data: &[u8],
    index_data: &[u8],
) -> io::Result<String> {
    match install_pack_bytes_journaled_inner(packs_dir, pack_data, index_data) {
        Ok(name) => {
            metric_inc(&METRIC_INSTALLS_OK);
            Ok(name)
        }
        Err(e) => {
            metric_inc(&METRIC_INSTALLS_ERR);
            Err(e)
        }
    }
}

fn install_pack_bytes_journaled_inner(
    packs_dir: &Path,
    pack_data: &[u8],
    index_data: &[u8],
) -> io::Result<String> {
    ensure_journal_layout_safe(packs_dir)?;
    // Hash once as native bytes; hex only for the FS/JSON name boundary.
    let digest = *blake3::hash(pack_data).as_bytes();
    let pack_name = digest_to_pack_name(&digest);

    if existing_pair_matches_digest(packs_dir, &pack_name, &digest)? {
        return Ok(pack_name);
    }

    // Stage outside per-pack lock (unique install_id).
    let install_id = new_install_id();
    validate_install_id(&install_id)?;
    let stage = staging_dir(packs_dir, &install_id);
    assert_under_packs(packs_dir, &stage)?;
    create_dir_all_durable(&stage)?;
    let staging_pack = staging_pack_path(packs_dir, &install_id);
    let staging_idx = staging_idx_path(packs_dir, &install_id);
    assert_under_packs(packs_dir, &staging_pack)?;
    assert_under_packs(packs_dir, &staging_idx)?;
    write_file_atomic(&staging_pack, pack_data)?;
    fault_inject::maybe_fail_at("pack_install_after_stage_pack")?;
    write_file_atomic(&staging_idx, index_data)?;
    fault_inject::maybe_fail_at("pack_install_after_stage_idx")?;

    // Per-pack lock for intent + publish (other pack names stay parallel).
    let _guard = acquire_pack_name_lock(packs_dir, &pack_name)?;
    fault_inject::maybe_fail_at("pack_install_after_pack_lock")?;

    if existing_pair_matches_digest(packs_dir, &pack_name, &digest)? {
        remove_staging(packs_dir, &install_id);
        return Ok(pack_name);
    }
    let dst_pack = dst_pack_path(packs_dir, &pack_name);
    let dst_idx = dst_idx_path(packs_dir, &pack_name);
    assert_under_packs(packs_dir, &dst_pack)?;
    assert_under_packs(packs_dir, &dst_idx)?;
    if dst_pack.exists() && !dst_idx.exists() {
        let _ = fs::remove_file(&dst_pack);
    }
    if dst_pack.exists() && dst_idx.exists() {
        if !existing_pair_matches_digest(packs_dir, &pack_name, &digest)? {
            let _ = fs::remove_file(&dst_pack);
            let _ = fs::remove_file(&dst_idx);
        } else {
            remove_staging(packs_dir, &install_id);
            return Ok(pack_name);
        }
    }

    let mut intent = PackInstallIntent::new(install_id.clone(), pack_name.clone());
    write_intent(packs_dir, &intent)?;
    fault_inject::maybe_fail_at("pack_install_after_intent_prepared")?;

    publish_file_durable(&staging_pack, &dst_pack)?;
    fault_inject::maybe_fail_at("pack_install_after_publish_pack")?;
    intent.phase = PackInstallPhase::PackPublished;
    write_intent(packs_dir, &intent)?;
    fault_inject::maybe_fail_at("pack_install_after_intent_pack_published")?;

    publish_file_durable(&staging_idx, &dst_idx)?;
    fault_inject::maybe_fail_at("pack_install_after_publish_idx")?;
    remove_staging(packs_dir, &install_id);
    remove_intent(packs_dir, &install_id)?;
    fault_inject::maybe_fail_at("pack_install_after_intent_removed")?;
    Ok(pack_name)
}

/// Journaled streaming install (consumes source pack/index paths).
pub fn install_pack_files_journaled(
    packs_dir: &Path,
    src_pack_path: &Path,
    src_index_path: &Path,
    pack_name: &str,
) -> io::Result<()> {
    match install_pack_files_journaled_inner(packs_dir, src_pack_path, src_index_path, pack_name) {
        Ok(()) => {
            metric_inc(&METRIC_INSTALLS_OK);
            Ok(())
        }
        Err(e) => {
            metric_inc(&METRIC_INSTALLS_ERR);
            Err(e)
        }
    }
}

fn install_pack_files_journaled_inner(
    packs_dir: &Path,
    src_pack_path: &Path,
    src_index_path: &Path,
    pack_name: &str,
) -> io::Result<()> {
    // Decode once; identity checks stay on native digests.
    let expected = pack_name_to_digest(pack_name)?;
    ensure_journal_layout_safe(packs_dir)?;

    if existing_pair_matches_digest(packs_dir, pack_name, &expected)? {
        let _ = fs::remove_file(src_pack_path);
        let _ = fs::remove_file(src_index_path);
        return Ok(());
    }

    let install_id = new_install_id();
    validate_install_id(&install_id)?;
    let stage = staging_dir(packs_dir, &install_id);
    assert_under_packs(packs_dir, &stage)?;
    create_dir_all_durable(&stage)?;
    let staging_pack = staging_pack_path(packs_dir, &install_id);
    let staging_idx = staging_idx_path(packs_dir, &install_id);
    assert_under_packs(packs_dir, &staging_pack)?;
    assert_under_packs(packs_dir, &staging_idx)?;
    publish_file_durable(src_pack_path, &staging_pack)?;
    fault_inject::maybe_fail_at("pack_install_stream_after_stage_pack")?;
    publish_file_durable(src_index_path, &staging_idx)?;
    fault_inject::maybe_fail_at("pack_install_stream_after_stage_idx")?;

    let _guard = acquire_pack_name_lock(packs_dir, pack_name)?;
    if existing_pair_matches_digest(packs_dir, pack_name, &expected)? {
        remove_staging(packs_dir, &install_id);
        return Ok(());
    }
    let dst_pack = dst_pack_path(packs_dir, pack_name);
    let dst_idx = dst_idx_path(packs_dir, pack_name);
    assert_under_packs(packs_dir, &dst_pack)?;
    assert_under_packs(packs_dir, &dst_idx)?;
    if dst_pack.exists() && !dst_idx.exists() {
        let _ = fs::remove_file(&dst_pack);
    }
    if dst_pack.exists() && dst_idx.exists() {
        if existing_pair_matches_digest(packs_dir, pack_name, &expected)? {
            remove_staging(packs_dir, &install_id);
            return Ok(());
        }
        let _ = fs::remove_file(&dst_pack);
        let _ = fs::remove_file(&dst_idx);
    }

    let mut intent = PackInstallIntent::new(install_id.clone(), pack_name.to_string());
    write_intent(packs_dir, &intent)?;
    fault_inject::maybe_fail_at("pack_install_stream_after_intent_prepared")?;

    publish_file_durable(&staging_pack, &dst_pack)?;
    fault_inject::maybe_fail_at("pack_install_stream_after_publish_pack")?;
    intent.phase = PackInstallPhase::PackPublished;
    write_intent(packs_dir, &intent)?;
    fault_inject::maybe_fail_at("pack_install_stream_after_intent_pack_published")?;

    publish_file_durable(&staging_idx, &dst_idx)?;
    fault_inject::maybe_fail_at("pack_install_stream_after_publish_idx")?;
    remove_staging(packs_dir, &install_id);
    remove_intent(packs_dir, &install_id)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_atomic::write_file_atomic;

    fn write_src(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        write_file_atomic(&p, bytes).unwrap();
        p
    }

    /// Deterministic 64-char lowercase hex pack name from a seed.
    fn pack_id(seed: &str) -> String {
        digest_to_pack_name(blake3::hash(seed.as_bytes()).as_bytes())
    }

    /// Minimal structurally valid pack index bytes.
    fn empty_idx_bytes() -> Vec<u8> {
        crate::store::pack::PackIndex::new().to_bytes()
    }

    #[test]
    fn validate_ids_reject_path_traversal() {
        assert!(validate_install_id("../evil").is_err());
        assert!(validate_install_id("a/b").is_err());
        assert!(validate_install_id("").is_err());
        assert!(validate_pack_name("../x").is_err());
        assert!(validate_pack_name("not-hex!").is_err());
        assert!(validate_pack_name("deadbeef").is_err()); // too short
        assert!(validate_pack_name(&"A".repeat(64)).is_err()); // uppercase
        assert!(validate_install_id("abc-123_OK").is_ok());
        assert!(validate_pack_name(&pack_id("ok")).is_ok());
        assert_eq!(pack_id("ok").len(), 64);
    }

    #[test]
    fn pack_name_digest_roundtrip_and_native_equality() {
        let body = b"digest-native-eq";
        let digest = *blake3::hash(body).as_bytes();
        let name = digest_to_pack_name(&digest);
        let parsed = pack_name_to_digest(&name).unwrap();
        assert_eq!(parsed, digest);
        // File identity uses bytes, not hex strings.
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        write_file_atomic(&dst_pack_path(&packs, &name), body).unwrap();
        write_file_atomic(&dst_idx_path(&packs, &name), &empty_idx_bytes()).unwrap();
        assert!(existing_pair_matches_digest(&packs, &name, &digest).unwrap());
        let mut wrong = digest;
        wrong[0] ^= 0xff;
        assert!(!existing_pair_matches_digest(&packs, &name, &wrong).unwrap());
    }

    #[test]
    fn malicious_intent_paths_ignored_reconstructed_from_packs_dir() {
        // Even if someone forged absolute paths in JSON, serde ignores unknown
        // fields and we only use install_id + pack_name.
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let outside = root.path().join("outside.txt");
        fs::write(&outside, b"secret").unwrap();

        let install_id = "malicious1";
        let pack_name = pack_id("malicious-pack");
        let intent_dir = intent_root(&packs);
        create_dir_all_durable(&intent_dir).unwrap();
        let forged = serde_json::json!({
            "version": 2,
            "install_id": install_id,
            "pack_name": pack_name,
            "phase": "prepared",
            "created_unix": 1,
            "staging_pack": outside.display().to_string(),
            "dst_pack": outside.display().to_string(),
            "dst_idx": outside.display().to_string(),
        });
        fs::write(
            intent_path(&packs, install_id),
            serde_json::to_vec_pretty(&forged).unwrap(),
        )
        .unwrap();

        // Expired → abort uses reconstructed paths only.
        let report = recover_pack_install_intents_with_ttl(&packs, Some(1)).unwrap();
        assert_eq!(report.aborted, 1);
        // Outside file must survive.
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
    }

    #[test]
    fn quarantine_unknown_version_preserves_file() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let intent_dir = intent_root(&packs);
        create_dir_all_durable(&intent_dir).unwrap();
        let path = intent_dir.join("weird.json");
        let pn = pack_id("weird");
        fs::write(
            &path,
            format!(
                r#"{{"version":99,"install_id":"x","pack_name":"{pn}","phase":"prepared","created_unix":1}}"#
            ),
        )
        .unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(1)).unwrap();
        assert_eq!(report.quarantined, 1);
        assert!(!path.exists());
        let q = quarantine_root(&packs);
        assert!(q.exists());
        assert!(fs::read_dir(&q).unwrap().count() >= 1);
    }

    #[test]
    fn short_pack_name_in_intent_is_quarantined() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        create_dir_all_durable(&intent_root(&packs)).unwrap();
        let path = intent_path(&packs, "shortname");
        fs::write(
            &path,
            br#"{"version":2,"install_id":"shortname","pack_name":"aa","phase":"prepared","created_unix":1}"#,
        )
        .unwrap();
        let report = recover_pack_install_intents_with_ttl(&packs, Some(1)).unwrap();
        assert_eq!(report.quarantined, 1);
        assert!(!path.exists());
    }

    #[test]
    fn quarantine_malformed_json() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let intent_dir = intent_root(&packs);
        create_dir_all_durable(&intent_dir).unwrap();
        let path = intent_dir.join("bad.json");
        fs::write(&path, b"not-json{{{{").unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(1)).unwrap();
        assert_eq!(report.quarantined, 1);
        assert!(!path.exists());
    }

    #[test]
    fn existing_pair_requires_hash_match_and_valid_index() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let body = b"pack-body-xyz";
        let name = format!("{}", blake3::hash(body).to_hex());
        write_file_atomic(&dst_pack_path(&packs, &name), body).unwrap();
        write_file_atomic(&dst_idx_path(&packs, &name), &empty_idx_bytes()).unwrap();
        assert!(existing_pair_matches_pack_name(&packs, &name).unwrap());

        // Structurally invalid index → not a match.
        write_file_atomic(&dst_idx_path(&packs, &name), b"not-an-index").unwrap();
        assert!(!existing_pair_matches_pack_name(&packs, &name).unwrap());

        // Wrong name for content (valid hex, wrong digest).
        let wrong = pack_id("wrong-name-for-body");
        write_file_atomic(&dst_pack_path(&packs, &wrong), body).unwrap();
        write_file_atomic(&dst_idx_path(&packs, &wrong), &empty_idx_bytes()).unwrap();
        assert!(!existing_pair_matches_pack_name(&packs, &wrong).unwrap());
    }

    #[test]
    fn install_rejects_idempotent_false_pair() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let pack_bytes = b"real-pack-content-111";
        let idx_bytes = empty_idx_bytes();
        let name = format!("{}", blake3::hash(pack_bytes).to_hex());

        // Corrupt pair with correct name but wrong content.
        write_file_atomic(&dst_pack_path(&packs, &name), b"wrong-content").unwrap();
        write_file_atomic(&dst_idx_path(&packs, &name), b"x").unwrap();
        assert!(!existing_pair_matches_pack_name(&packs, &name).unwrap());

        let out = install_pack_bytes_journaled(&packs, pack_bytes, &idx_bytes).unwrap();
        assert_eq!(out, name);
        assert_eq!(fs::read(dst_pack_path(&packs, &name)).unwrap(), pack_bytes);
    }

    #[test]
    fn journaled_install_produces_pair_and_cleans_intent() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let src_dir = root.path().join("src");
        create_dir_all_durable(&src_dir).unwrap();

        let pack_bytes = b"fake-pack-bytes-aaa";
        let idx_bytes = b"fake-idx-bytes-aaa";
        let src_pack = write_src(&src_dir, "p.pack", pack_bytes);
        let src_idx = write_src(&src_dir, "p.idx", idx_bytes);
        let name = format!("{}", blake3::hash(pack_bytes).to_hex());

        install_pack_files_journaled(&packs, &src_pack, &src_idx, &name).unwrap();

        assert!(dst_pack_path(&packs, &name).exists());
        assert!(dst_idx_path(&packs, &name).exists());
        assert_eq!(fs::read(dst_pack_path(&packs, &name)).unwrap(), pack_bytes);
        assert!(
            !intent_root(&packs).exists()
                || fs::read_dir(intent_root(&packs))
                    .unwrap()
                    .filter(|e| {
                        e.as_ref()
                            .map(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                            .unwrap_or(false)
                    })
                    .count()
                    == 0
        );
    }

    #[test]
    fn recover_pack_published_completes_from_staging() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();

        let name = pack_id("deadbeef-seed");
        let install_id = "test-install-1";
        validate_install_id(install_id).unwrap();
        validate_pack_name(&name).unwrap();
        let stage = staging_dir(&packs, install_id);
        create_dir_all_durable(&stage).unwrap();
        write_file_atomic(&staging_pack_path(&packs, install_id), b"pack-body").unwrap();
        write_file_atomic(&staging_idx_path(&packs, install_id), b"idx-body").unwrap();

        let dst_pack = dst_pack_path(&packs, &name);
        publish_file_durable(&staging_pack_path(&packs, install_id), &dst_pack).unwrap();

        let intent = PackInstallIntent {
            version: PACK_INSTALL_INTENT_VERSION,
            install_id: install_id.to_string(),
            pack_name: name.clone(),
            phase: PackInstallPhase::PackPublished,
            created_unix: 1,
        };
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents(&packs).unwrap();
        assert_eq!(report.intents_seen, 1);
        assert_eq!(report.completed, 1);
        assert!(dst_pack.exists());
        assert!(dst_idx_path(&packs, &name).exists());
        assert_eq!(fs::read(dst_idx_path(&packs, &name)).unwrap(), b"idx-body");
        assert!(!intent_path(&packs, install_id).exists());
    }

    #[test]
    fn recover_prepared_aborts_without_finals_when_expired() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let install_id = "prep-abort";
        let stage = staging_dir(&packs, install_id);
        create_dir_all_durable(&stage).unwrap();
        write_file_atomic(&staging_pack_path(&packs, install_id), b"p").unwrap();
        write_file_atomic(&staging_idx_path(&packs, install_id), b"i").unwrap();

        let mut intent = PackInstallIntent::new(install_id.into(), pack_id("aa"));
        intent.created_unix = 1;
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(60)).unwrap();
        assert_eq!(report.aborted, 1);
        assert!(!dst_pack_path(&packs, &pack_id("aa")).exists());
        assert!(!intent_path(&packs, install_id).exists());
        assert!(!stage.exists());
    }

    #[test]
    fn recover_prepared_fresh_skips_in_progress() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let install_id = "live-prep";
        let stage = staging_dir(&packs, install_id);
        create_dir_all_durable(&stage).unwrap();
        let staging_pack = staging_pack_path(&packs, install_id);
        let staging_idx = staging_idx_path(&packs, install_id);
        write_file_atomic(&staging_pack, b"live-p").unwrap();
        write_file_atomic(&staging_idx, b"live-i").unwrap();

        let intent = PackInstallIntent::new(install_id.into(), pack_id("bb"));
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(86_400)).unwrap();
        assert_eq!(report.skipped_in_progress, 1);
        assert_eq!(report.aborted, 0);
        assert!(staging_pack.exists());
        assert!(intent_path(&packs, install_id).exists());
    }

    #[test]
    fn recover_pack_published_without_staging_idx_aborts_orphan_pack() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let name = pack_id("cc");
        let install_id = "orph-1";
        let dst_pack = dst_pack_path(&packs, &name);
        write_file_atomic(&dst_pack, b"only-pack").unwrap();

        let intent = PackInstallIntent {
            version: PACK_INSTALL_INTENT_VERSION,
            install_id: install_id.into(),
            pack_name: name,
            phase: PackInstallPhase::PackPublished,
            created_unix: 1,
        };
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents(&packs).unwrap();
        assert_eq!(report.aborted, 1);
        assert!(!dst_pack.exists());
        assert!(!intent_path(&packs, install_id).exists());
    }

    #[test]
    fn pack_lock_prevents_recover_aborting_live_expired_looking_install() {
        use std::{
            sync::{Arc, Barrier},
            thread,
            time::Duration,
        };

        // Per-pack try_lock: recover must skip (not abort) while install holds
        // the pack lock — even if created_unix looks TTL-expired.
        let root = tempfile::tempdir().unwrap();
        let packs = Arc::new(root.path().join("packs"));
        create_dir_all_durable(&packs).unwrap();

        let planted_under_lock = Arc::new(Barrier::new(2));
        let both_done = Arc::new(Barrier::new(2));

        let packs_a = Arc::clone(&packs);
        let planted_a = Arc::clone(&planted_under_lock);
        let done_a = Arc::clone(&both_done);

        let installer = thread::spawn(move || {
            let packs = packs_a.as_path();
            let guard = acquire_pack_name_lock(packs, &pack_id("dd")).expect("pack lock");

            let install_id = "flock-live";
            let stage = staging_dir(packs, install_id);
            create_dir_all_durable(&stage).unwrap();
            let staging_pack = staging_pack_path(packs, install_id);
            let staging_idx = staging_idx_path(packs, install_id);
            write_file_atomic(&staging_pack, b"flock-pack").unwrap();
            write_file_atomic(&staging_idx, b"flock-idx").unwrap();
            let dst_pack = dst_pack_path(packs, &pack_id("dd"));
            let dst_idx = dst_idx_path(packs, &pack_id("dd"));

            let mut intent = PackInstallIntent::new(install_id.into(), pack_id("dd"));
            intent.created_unix = 1; // looks expired under any short TTL
            write_intent(packs, &intent).unwrap();

            planted_a.wait();
            thread::sleep(Duration::from_millis(60));

            assert!(staging_pack.exists() && staging_idx.exists());
            assert!(intent_path(packs, install_id).exists());

            publish_file_durable(&staging_pack, &dst_pack).unwrap();
            intent.phase = PackInstallPhase::PackPublished;
            write_intent(packs, &intent).unwrap();
            publish_file_durable(&staging_idx, &dst_idx).unwrap();
            remove_staging(packs, install_id);
            remove_intent(packs, install_id).unwrap();

            drop(guard);
            assert!(dst_pack.exists() && dst_idx.exists());
            done_a.wait();
        });

        let packs_b = Arc::clone(&packs);
        let planted_b = Arc::clone(&planted_under_lock);
        let done_b = Arc::clone(&both_done);

        let recoverer = thread::spawn(move || {
            let packs = packs_b.as_path();
            planted_b.wait();
            // While pack lock is held, recover try_locks and skips — must not abort.
            let mid = recover_pack_install_intents_with_ttl(packs, Some(1))
                .expect("recover under pack lock");
            assert_eq!(mid.aborted, 0, "must not abort live install: {mid:?}");
            assert!(
                mid.skipped_in_progress >= 1 || dst_pack_path(packs, &pack_id("dd")).exists(),
                "either skip in-progress or install already finished: {mid:?}"
            );
            done_b.wait();
            // After installer finishes, finals exist and no intent remains.
            assert!(dst_pack_path(packs, &pack_id("dd")).exists());
            assert!(dst_idx_path(packs, &pack_id("dd")).exists());
            assert!(!intent_path(packs, "flock-live").exists());
        });

        installer.join().expect("installer");
        recoverer.join().expect("recoverer");
    }

    #[test]
    fn recover_prepared_with_pack_and_staging_idx_completes() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let install_id = "prep-complete";
        let stage = staging_dir(&packs, install_id);
        create_dir_all_durable(&stage).unwrap();
        write_file_atomic(&staging_pack_path(&packs, install_id), b"pack-x").unwrap();
        write_file_atomic(&staging_idx_path(&packs, install_id), b"idx-x").unwrap();
        let dst_pack = dst_pack_path(&packs, &pack_id("ee"));
        publish_file_durable(&staging_pack_path(&packs, install_id), &dst_pack).unwrap();

        let intent = PackInstallIntent::new(install_id.into(), pack_id("ee"));
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents(&packs).unwrap();
        assert_eq!(report.completed, 1);
        assert!(dst_pack.exists());
        assert!(dst_idx_path(&packs, &pack_id("ee")).exists());
        assert_eq!(
            fs::read(dst_idx_path(&packs, &pack_id("ee"))).unwrap(),
            b"idx-x"
        );
    }

    #[test]
    fn journaled_install_idempotent_when_pair_exists() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let pack_bytes = b"idemp-pack-bytes";
        let name = format!("{}", blake3::hash(pack_bytes).to_hex());
        write_file_atomic(&dst_pack_path(&packs, &name), pack_bytes).unwrap();
        write_file_atomic(&dst_idx_path(&packs, &name), &empty_idx_bytes()).unwrap();
        assert!(existing_pair_matches_pack_name(&packs, &name).unwrap());

        let src_dir = root.path().join("src");
        create_dir_all_durable(&src_dir).unwrap();
        let src_pack = write_src(&src_dir, "a", b"other");
        let src_idx = write_src(&src_dir, "b", b"other-i");

        install_pack_files_journaled(&packs, &src_pack, &src_idx, &name).unwrap();
        assert_eq!(fs::read(dst_pack_path(&packs, &name)).unwrap(), pack_bytes);
    }

    #[test]
    fn install_pack_bytes_journaled_happy_path() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();

        let pack_bytes = b"in-memory-pack-body-zzz";
        let idx_bytes = empty_idx_bytes();
        let expected_name = format!("{}", blake3::hash(pack_bytes).to_hex());

        let name = install_pack_bytes_journaled(&packs, pack_bytes, &idx_bytes).unwrap();
        assert_eq!(name, expected_name);
        assert!(existing_pair_matches_pack_name(&packs, &name).unwrap());

        let name2 = install_pack_bytes_journaled(&packs, pack_bytes, &idx_bytes).unwrap();
        assert_eq!(name2, expected_name);
    }

    #[test]
    fn ttl_aborts_old_prepared_intent() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let install_id = "ttl-prep";
        let stage = staging_dir(&packs, install_id);
        create_dir_all_durable(&stage).unwrap();
        write_file_atomic(&staging_pack_path(&packs, install_id), b"stale-p").unwrap();
        write_file_atomic(&staging_idx_path(&packs, install_id), b"stale-i").unwrap();

        let mut intent = PackInstallIntent::new(install_id.into(), pack_id("ff"));
        intent.created_unix = 1;
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(60)).unwrap();
        assert_eq!(report.aborted, 1);
        assert!(!intent_path(&packs, install_id).exists());
        assert!(!stage.exists());
    }

    #[test]
    fn complete_preferred_over_ttl_when_staging_idx_present() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();

        let name = pack_id("11");
        let install_id = "ttl-complete-1";
        let stage = staging_dir(&packs, install_id);
        create_dir_all_durable(&stage).unwrap();
        write_file_atomic(&staging_pack_path(&packs, install_id), b"pack-ttl").unwrap();
        write_file_atomic(&staging_idx_path(&packs, install_id), b"idx-ttl").unwrap();

        let dst_pack = dst_pack_path(&packs, &name);
        publish_file_durable(&staging_pack_path(&packs, install_id), &dst_pack).unwrap();

        let intent = PackInstallIntent {
            version: PACK_INSTALL_INTENT_VERSION,
            install_id: install_id.into(),
            pack_name: name.clone(),
            phase: PackInstallPhase::PackPublished,
            created_unix: 1,
        };
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(1)).unwrap();
        assert_eq!(report.completed, 1);
        assert!(dst_idx_path(&packs, &name).exists());
    }

    #[test]
    fn relocated_repo_recovery_uses_new_packs_dir() {
        // Intent has only ids; moving the packs tree still recovers via new root.
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("old").join("packs");
        create_dir_all_durable(&packs).unwrap();
        let install_id = "reloc1";
        let name = pack_id("22");
        create_dir_all_durable(&staging_dir(&packs, install_id)).unwrap();
        write_file_atomic(&staging_pack_path(&packs, install_id), b"p").unwrap();
        write_file_atomic(&staging_idx_path(&packs, install_id), b"i").unwrap();
        publish_file_durable(
            &staging_pack_path(&packs, install_id),
            &dst_pack_path(&packs, &name),
        )
        .unwrap();
        // Restage idx after pack publish consumed staging pack
        write_file_atomic(&staging_idx_path(&packs, install_id), b"i").unwrap();
        let intent = PackInstallIntent {
            version: 2,
            install_id: install_id.into(),
            pack_name: name.clone(),
            phase: PackInstallPhase::PackPublished,
            created_unix: 1,
        };
        write_intent(&packs, &intent).unwrap();

        // "Move" repo: rename packs directory
        let new_packs = root.path().join("new").join("packs");
        create_dir_all_durable(new_packs.parent().unwrap()).unwrap();
        fs::rename(&packs, &new_packs).unwrap();

        let report = recover_pack_install_intents(&new_packs).unwrap();
        assert_eq!(report.completed, 1);
        assert!(dst_idx_path(&new_packs, &name).exists());
    }

    #[test]
    fn concurrent_same_pack_installs_converge() {
        use std::thread;

        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let pack_bytes = b"same-pack-concurrent-body";
        let idx_bytes = empty_idx_bytes();
        let expected = format!("{}", blake3::hash(pack_bytes).to_hex());

        let packs1 = packs.clone();
        let packs2 = packs.clone();
        let idx1 = idx_bytes.clone();
        let idx2 = idx_bytes.clone();
        let t1 = thread::spawn(move || {
            install_pack_bytes_journaled(&packs1, pack_bytes, &idx1).unwrap()
        });
        let t2 = thread::spawn(move || {
            install_pack_bytes_journaled(&packs2, pack_bytes, &idx2).unwrap()
        });
        let n1 = t1.join().unwrap();
        let n2 = t2.join().unwrap();
        assert_eq!(n1, expected);
        assert_eq!(n2, expected);
        assert!(existing_pair_matches_pack_name(&packs, &expected).unwrap());
    }

    #[test]
    fn concurrent_many_distinct_pack_installs() {
        use std::thread;

        // Distinct pack_names take distinct locks — installs progress in parallel.
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let idx_bytes = empty_idx_bytes();

        let mut handles = Vec::new();
        for i in 0..8u8 {
            let packs = packs.clone();
            let idx_bytes = idx_bytes.clone();
            handles.push(thread::spawn(move || {
                let pack_bytes = format!("many-pack-body-{i}").into_bytes();
                install_pack_bytes_journaled(&packs, &pack_bytes, &idx_bytes).unwrap()
            }));
        }
        let mut names = Vec::new();
        for h in handles {
            names.push(h.join().unwrap());
        }
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 8, "expected 8 distinct pack names");
        for name in &names {
            assert!(existing_pair_matches_pack_name(&packs, name).unwrap());
        }
    }

    #[test]
    fn far_future_created_unix_expires_immediately_under_ttl() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let install_id = "far-future";
        create_dir_all_durable(&staging_dir(&packs, install_id)).unwrap();
        write_file_atomic(&staging_pack_path(&packs, install_id), b"p").unwrap();
        write_file_atomic(&staging_idx_path(&packs, install_id), b"i").unwrap();

        let mut intent = PackInstallIntent::new(install_id.into(), pack_id("aa"));
        // Beyond skew tolerance — must not dodge expiry.
        intent.created_unix = unix_now().saturating_add(INTENT_CLOCK_SKEW_TOLERANCE_SECS + 10_000);
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(86_400)).unwrap();
        assert_eq!(report.aborted, 1, "far-future must expire: {report:?}");
        assert!(!intent_path(&packs, install_id).exists());
    }

    #[test]
    fn mild_clock_skew_does_not_expire_fresh_intent() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let install_id = "mild-skew";
        create_dir_all_durable(&staging_dir(&packs, install_id)).unwrap();
        write_file_atomic(&staging_pack_path(&packs, install_id), b"p").unwrap();
        write_file_atomic(&staging_idx_path(&packs, install_id), b"i").unwrap();

        let mut intent = PackInstallIntent::new(install_id.into(), pack_id("bb"));
        // Slightly ahead of wall clock (within tolerance) — still "in progress".
        intent.created_unix = unix_now().saturating_add(INTENT_CLOCK_SKEW_TOLERANCE_SECS / 2);
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(86_400)).unwrap();
        assert_eq!(report.skipped_in_progress, 1, "mild skew: {report:?}");
        assert_eq!(report.aborted, 0);
        assert!(intent_path(&packs, install_id).exists());
    }

    #[test]
    fn fault_after_intent_prepared_is_recoverable() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();

        let pack_bytes = b"fault-inject-pack-body";
        let idx_bytes = empty_idx_bytes();
        let expected = format!("{}", blake3::hash(pack_bytes).to_hex());

        let before_err = pack_install_metrics_snapshot().installs_err;
        let err = fault_inject::with_fault_points(&["pack_install_after_intent_prepared"], || {
            install_pack_bytes_journaled(&packs, pack_bytes, &idx_bytes)
        })
        .expect_err("fault should fire");
        assert!(
            err.to_string()
                .contains("pack_install_after_intent_prepared"),
            "err={err}"
        );
        // Process-global counters can race under parallel tests; assert non-decreasing delta.
        assert!(pack_install_metrics_snapshot().installs_err >= before_err);

        // Staging + prepared intent should remain for recovery.
        assert_eq!(intent_count_json(&packs), 1);
        assert!(!dst_pack_path(&packs, &expected).exists());

        // Force-expire prepared intent, abort, then reinstall succeeds.
        for entry in fs::read_dir(intent_root(&packs)).unwrap().flatten() {
            let p = entry.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let mut intent: PackInstallIntent =
                serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
            intent.created_unix = 1;
            write_intent(&packs, &intent).unwrap();
        }
        let report = recover_pack_install_intents_with_ttl(&packs, Some(1)).unwrap();
        assert_eq!(report.aborted, 1, "report={report:?}");
        assert_eq!(intent_count_json(&packs), 0);

        let name = install_pack_bytes_journaled(&packs, pack_bytes, &idx_bytes).unwrap();
        assert_eq!(name, expected);
        assert!(existing_pair_matches_pack_name(&packs, &expected).unwrap());
    }

    #[test]
    fn fault_after_publish_pack_recovers_to_complete() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();

        let pack_bytes = b"fault-after-pack-publish-body";
        let idx_bytes = empty_idx_bytes();
        let expected = format!("{}", blake3::hash(pack_bytes).to_hex());

        let err = fault_inject::with_fault_points(&["pack_install_after_publish_pack"], || {
            install_pack_bytes_journaled(&packs, pack_bytes, &idx_bytes)
        })
        .expect_err("fault should fire after pack publish");
        assert!(err.to_string().contains("pack_install_after_publish_pack"));

        // Pack published, intent may still be prepared (fault is after publish, before
        // phase rewrite) or pack_published depending on checkpoint placement.
        assert!(dst_pack_path(&packs, &expected).exists());
        assert!(!dst_idx_path(&packs, &expected).exists());

        let report = recover_pack_install_intents(&packs).unwrap();
        assert_eq!(report.completed, 1, "report={report:?}");
        assert!(dst_idx_path(&packs, &expected).exists());
        assert!(existing_pair_matches_pack_name(&packs, &expected).unwrap());
    }

    fn intent_count_json(packs: &Path) -> usize {
        let dir = intent_root(packs);
        if !dir.exists() {
            return 0;
        }
        fs::read_dir(dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .map(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                    .unwrap_or(false)
            })
            .count()
    }

    #[test]
    fn metrics_snapshot_tracks_install_and_recover() {
        // Process-global atomics: measure deltas so parallel tests don't flake.
        let before = pack_install_metrics_snapshot();
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let _ = install_pack_bytes_journaled(&packs, b"metrics-pack", &empty_idx_bytes()).unwrap();
        let after_install = pack_install_metrics_snapshot();
        assert!(after_install.installs_ok > before.installs_ok);

        // Plant expired prepared → recover aborts.
        let install_id = "metrics-abort";
        create_dir_all_durable(&staging_dir(&packs, install_id)).unwrap();
        write_file_atomic(&staging_pack_path(&packs, install_id), b"p").unwrap();
        write_file_atomic(&staging_idx_path(&packs, install_id), b"i").unwrap();
        let mut intent = PackInstallIntent::new(install_id.into(), pack_id("cc"));
        intent.created_unix = 1;
        write_intent(&packs, &intent).unwrap();
        let before_abort = pack_install_metrics_snapshot();
        let report = recover_pack_install_intents_with_ttl(&packs, Some(1)).unwrap();
        assert_eq!(report.aborted, 1);
        let after_abort = pack_install_metrics_snapshot();
        assert!(after_abort.recover_aborted > before_abort.recover_aborted);
    }

    #[cfg(unix)]
    #[test]
    fn assert_under_packs_rejects_staging_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let outside = root.path().join("outside");
        create_dir_all_durable(&outside).unwrap();

        // Lexically under packs, but .staging is a symlink out.
        symlink(&outside, packs.join(STAGING_DIR_NAME)).unwrap();

        let stage = staging_dir(&packs, "id1");
        let err = assert_under_packs(&packs, &stage).unwrap_err();
        assert!(
            err.to_string().contains("escapes") || err.to_string().contains("symlink"),
            "err={err}"
        );

        // Journal layout guard must fail before install.
        let err = ensure_journal_layout_safe(&packs).unwrap_err();
        assert!(
            err.to_string().contains("escapes") || err.to_string().contains("symlink"),
            "err={err}"
        );
        let err = install_pack_bytes_journaled(&packs, b"x", &empty_idx_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("escapes") || err.to_string().contains("symlink"),
            "err={err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn assert_under_packs_rejects_intent_root_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let outside = root.path().join("outside-intent");
        create_dir_all_durable(&outside).unwrap();
        symlink(&outside, packs.join(INTENT_DIR_NAME)).unwrap();

        let err = ensure_journal_layout_safe(&packs).unwrap_err();
        assert!(
            err.to_string().contains("escapes") || err.to_string().contains("symlink"),
            "err={err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn assert_under_packs_rejects_pack_locks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let outside = root.path().join("outside-locks");
        create_dir_all_durable(&outside).unwrap();
        symlink(&outside, packs.join(PACK_LOCKS_DIR_NAME)).unwrap();

        let err = ensure_journal_layout_safe(&packs).unwrap_err();
        assert!(
            err.to_string().contains("escapes") || err.to_string().contains("symlink"),
            "err={err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn assert_under_packs_rejects_destination_file_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let outside = root.path().join("evil.pack");
        fs::write(&outside, b"evil").unwrap();

        let name = pack_id("symlink-dst");
        let dst = dst_pack_path(&packs, &name);
        symlink(&outside, &dst).unwrap();

        let err = assert_under_packs(&packs, &dst).unwrap_err();
        assert!(
            err.to_string().contains("escapes") || err.to_string().contains("symlink"),
            "err={err}"
        );
        // existing_pair must refuse a symlink-out destination (error or false).
        let pair = existing_pair_matches_pack_name(&packs, &name);
        assert!(pair.as_ref().map(|v| !v).unwrap_or(true), "pair={pair:?}");
    }

    #[cfg(unix)]
    #[test]
    fn assert_under_packs_rejects_install_id_staging_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        create_dir_all_durable(&staging_root(&packs)).unwrap();
        let outside = root.path().join("outside-stage-id");
        create_dir_all_durable(&outside).unwrap();
        let install_id = "symlink-stage-id";
        symlink(&outside, staging_dir(&packs, install_id)).unwrap();

        let pack_path = staging_pack_path(&packs, install_id);
        let err = assert_under_packs(&packs, &pack_path).unwrap_err();
        assert!(
            err.to_string().contains("escapes") || err.to_string().contains("symlink"),
            "err={err}"
        );
    }

    #[test]
    fn assert_under_packs_accepts_normal_reconstructed_paths() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let name = pack_id("normal");
        let install_id = "normal-id";
        assert_under_packs(&packs, &dst_pack_path(&packs, &name)).unwrap();
        assert_under_packs(&packs, &staging_pack_path(&packs, install_id)).unwrap();
        assert_under_packs(&packs, &intent_path(&packs, install_id)).unwrap();
    }
}
