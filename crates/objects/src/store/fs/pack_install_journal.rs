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
//! 1. Stage pack+idx under `.staging/<id>/` (may be outside the exclusive lock).
//! 2. Take install lock; write one durable **prepared** intent.
//! 3. Publish pack → update intent to **pack_published**.
//! 4. Publish index → **remove** intent (no Completed rewrite).
//! 5. Best-effort remove staging; fsync intent dir after intent unlink.
//!
//! Recovery reconstructs paths, validates IDs, completes or aborts, quarantines
//! garbage intents. See `docs/program/L8_PACK_INSTALL_JOURNAL.md`.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    fs_atomic::{create_dir_all_durable, publish_file_durable, sync_directory, write_file_atomic},
    lock::RepoLock,
};

/// Intent schema version (v2 = identifiers only; paths reconstructed).
pub const PACK_INSTALL_INTENT_VERSION: u32 = 2;

/// Default TTL for abandoned install intents / orphan staging (24 hours).
pub const DEFAULT_PACK_INSTALL_INTENT_TTL_SECS: i64 = 86_400;

const STAGING_DIR_NAME: &str = ".staging";
const INTENT_DIR_NAME: &str = ".install-intent";
const QUARANTINE_DIR_NAME: &str = "quarantine";
const STAGED_PACK_NAME: &str = "pack";
const STAGED_IDX_NAME: &str = "idx";
const PACK_INSTALL_LOCK_NAME: &str = ".pack-install.lock";

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

/// Exclusive install/recover lock for `packs_dir` (cross-thread + cross-process).
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

/// Blake3 hex pack name (64 lowercase hex digits preferred; accept hex only).
pub(crate) fn validate_pack_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.len() > 128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid pack_name length",
        ));
    }
    if !name.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pack_name must be hexadecimal",
        ));
    }
    Ok(())
}

fn validate_intent_ids(intent: &PackInstallIntent) -> io::Result<()> {
    validate_install_id(&intent.install_id)?;
    validate_pack_name(&intent.pack_name)?;
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

/// Ensure a reconstructed path stays under `packs_dir` after canonicalize of parent.
fn assert_under_packs(packs_dir: &Path, candidate: &Path) -> io::Result<()> {
    let packs_canon = fs::canonicalize(packs_dir).unwrap_or_else(|_| packs_dir.to_path_buf());
    // Candidate may not exist yet — canonicalize parent + join leaf.
    let parent = candidate.parent().unwrap_or(candidate);
    let leaf = candidate
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path missing file name"))?;
    let parent_canon = if parent.exists() {
        fs::canonicalize(parent)?
    } else {
        // Walk up to first existing ancestor under packs
        let mut cur = parent.to_path_buf();
        while !cur.exists() {
            if !cur.pop() {
                break;
            }
        }
        let base = if cur.exists() {
            fs::canonicalize(&cur)?
        } else {
            packs_canon.clone()
        };
        // Rebuild relative suffix from packs
        base
    };
    let full = if parent.exists() {
        parent_canon.join(leaf)
    } else {
        // For non-existent paths under packs_dir, check components only
        for c in candidate
            .strip_prefix(packs_dir)
            .unwrap_or(candidate)
            .components()
        {
            match c {
                Component::Normal(_) | Component::CurDir => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "path escapes packs_dir",
                    ));
                }
            }
        }
        return Ok(());
    };
    if !full.starts_with(&packs_canon) && !candidate.starts_with(packs_dir) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reconstructed path escapes packs_dir",
        ));
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

fn intent_expired(intent: &PackInstallIntent, ttl_secs: Option<i64>, now: i64) -> bool {
    match ttl_secs {
        Some(ttl) if ttl >= 0 => intent.created_unix.saturating_add(ttl) < now,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Existing pair identity validation
// ---------------------------------------------------------------------------

fn hash_file_blake3_hex(path: &Path) -> io::Result<String> {
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
    Ok(format!("{}", hasher.finalize().to_hex()))
}

/// True when final pack+idx exist and pack content hashes to `pack_name`.
pub(crate) fn existing_pair_matches_pack_name(
    packs_dir: &Path,
    pack_name: &str,
) -> io::Result<bool> {
    validate_pack_name(pack_name)?;
    let pack = dst_pack_path(packs_dir, pack_name);
    let idx = dst_idx_path(packs_dir, pack_name);
    if !pack.exists() || !idx.exists() {
        return Ok(false);
    }
    if idx.metadata()?.len() == 0 {
        return Ok(false);
    }
    let digest = hash_file_blake3_hex(&pack)?;
    Ok(digest.eq_ignore_ascii_case(pack_name))
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
    let _guard = acquire_pack_install_lock(packs_dir)?;
    recover_pack_install_intents_with_ttl_locked(packs_dir, ttl_secs)
}

fn recover_pack_install_intents_with_ttl_locked(
    packs_dir: &Path,
    ttl_secs: Option<i64>,
) -> io::Result<PackInstallRecoverReport> {
    let mut report = PackInstallRecoverReport::default();
    let now = unix_now();
    let intent_dir = intent_root(packs_dir);

    if intent_dir.exists() {
        let entries = match fs::read_dir(&intent_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                sweep_orphan_staging(packs_dir, ttl_secs, now, &mut report);
                return Ok(report);
            }
            Err(e) => return Err(e),
        };

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    report.errors += 1;
                    continue;
                }
            };
            let path = entry.path();
            // Skip quarantine subdirectory and non-json.
            if path.is_dir() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
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
                            continue;
                        }
                    }
                }
                Ok(_) | Err(_) => {
                    // Unknown version or malformed: quarantine, never delete.
                    match quarantine_intent_file(packs_dir, &path) {
                        Ok(()) => report.quarantined += 1,
                        Err(_) => report.errors += 1,
                    }
                    continue;
                }
            };

            let expired = intent_expired(&intent, ttl_secs, now);
            if let Err(_) = recover_one_intent(packs_dir, &intent, expired, &mut report) {
                report.errors += 1;
            }
        }
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
    create_dir_all_durable(packs_dir)?;
    let pack_name = format!("{}", blake3::hash(pack_data).to_hex());

    if existing_pair_matches_pack_name(packs_dir, &pack_name)? {
        return Ok(pack_name);
    }

    // Stage outside lock (unique install_id); lock only for intent + publish.
    let install_id = new_install_id();
    validate_install_id(&install_id)?;
    let stage = staging_dir(packs_dir, &install_id);
    create_dir_all_durable(&stage)?;
    let staging_pack = staging_pack_path(packs_dir, &install_id);
    let staging_idx = staging_idx_path(packs_dir, &install_id);
    write_file_atomic(&staging_pack, pack_data)?;
    write_file_atomic(&staging_idx, index_data)?;

    let _guard = acquire_pack_install_lock(packs_dir)?;
    // Re-check under lock.
    if existing_pair_matches_pack_name(packs_dir, &pack_name)? {
        remove_staging(packs_dir, &install_id);
        return Ok(pack_name);
    }
    // Drop corrupt/orphan final pack without idx.
    let dst_pack = dst_pack_path(packs_dir, &pack_name);
    let dst_idx = dst_idx_path(packs_dir, &pack_name);
    if dst_pack.exists() && !dst_idx.exists() {
        let _ = fs::remove_file(&dst_pack);
    }
    if dst_pack.exists() && dst_idx.exists() {
        // Exists but hash mismatch — remove both and reinstall.
        if !existing_pair_matches_pack_name(packs_dir, &pack_name)? {
            let _ = fs::remove_file(&dst_pack);
            let _ = fs::remove_file(&dst_idx);
        } else {
            remove_staging(packs_dir, &install_id);
            return Ok(pack_name);
        }
    }

    let mut intent = PackInstallIntent::new(install_id.clone(), pack_name.clone());
    write_intent(packs_dir, &intent)?;

    publish_file_durable(&staging_pack, &dst_pack)?;
    intent.phase = PackInstallPhase::PackPublished;
    write_intent(packs_dir, &intent)?;

    publish_file_durable(&staging_idx, &dst_idx)?;
    // No Completed rewrite — delete intent after both finals durable.
    remove_staging(packs_dir, &install_id);
    remove_intent(packs_dir, &install_id)?;
    Ok(pack_name)
}

/// Journaled streaming install (consumes source pack/index paths).
pub fn install_pack_files_journaled(
    packs_dir: &Path,
    src_pack_path: &Path,
    src_index_path: &Path,
    pack_name: &str,
) -> io::Result<()> {
    validate_pack_name(pack_name)?;
    create_dir_all_durable(packs_dir)?;

    if existing_pair_matches_pack_name(packs_dir, pack_name)? {
        let _ = fs::remove_file(src_pack_path);
        let _ = fs::remove_file(src_index_path);
        return Ok(());
    }

    let install_id = new_install_id();
    validate_install_id(&install_id)?;
    let stage = staging_dir(packs_dir, &install_id);
    create_dir_all_durable(&stage)?;
    let staging_pack = staging_pack_path(packs_dir, &install_id);
    let staging_idx = staging_idx_path(packs_dir, &install_id);
    // Stage outside lock.
    publish_file_durable(src_pack_path, &staging_pack)?;
    publish_file_durable(src_index_path, &staging_idx)?;

    let _guard = acquire_pack_install_lock(packs_dir)?;
    if existing_pair_matches_pack_name(packs_dir, pack_name)? {
        remove_staging(packs_dir, &install_id);
        return Ok(());
    }
    let dst_pack = dst_pack_path(packs_dir, pack_name);
    let dst_idx = dst_idx_path(packs_dir, pack_name);
    if dst_pack.exists() && !dst_idx.exists() {
        let _ = fs::remove_file(&dst_pack);
    }
    if dst_pack.exists() && dst_idx.exists() {
        if existing_pair_matches_pack_name(packs_dir, pack_name)? {
            remove_staging(packs_dir, &install_id);
            return Ok(());
        }
        let _ = fs::remove_file(&dst_pack);
        let _ = fs::remove_file(&dst_idx);
    }

    let mut intent = PackInstallIntent::new(install_id.clone(), pack_name.to_string());
    write_intent(packs_dir, &intent)?;

    publish_file_durable(&staging_pack, &dst_pack)?;
    intent.phase = PackInstallPhase::PackPublished;
    write_intent(packs_dir, &intent)?;

    publish_file_durable(&staging_idx, &dst_idx)?;
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

    #[test]
    fn validate_ids_reject_path_traversal() {
        assert!(validate_install_id("../evil").is_err());
        assert!(validate_install_id("a/b").is_err());
        assert!(validate_install_id("").is_err());
        assert!(validate_pack_name("../x").is_err());
        assert!(validate_pack_name("not-hex!").is_err());
        assert!(validate_install_id("abc-123_OK").is_ok());
        assert!(validate_pack_name("deadbeef").is_ok());
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
        let pack_name = "aa";
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
        fs::write(
            &path,
            br#"{"version":99,"install_id":"x","pack_name":"aa","phase":"prepared","created_unix":1}"#,
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
    fn existing_pair_requires_hash_match() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let body = b"pack-body-xyz";
        let name = format!("{}", blake3::hash(body).to_hex());
        write_file_atomic(&dst_pack_path(&packs, &name), body).unwrap();
        write_file_atomic(&dst_idx_path(&packs, &name), b"idx").unwrap();
        assert!(existing_pair_matches_pack_name(&packs, &name).unwrap());

        // Wrong name for content.
        write_file_atomic(&dst_pack_path(&packs, "00"), body).unwrap();
        write_file_atomic(&dst_idx_path(&packs, "00"), b"idx").unwrap();
        assert!(!existing_pair_matches_pack_name(&packs, "00").unwrap());
    }

    #[test]
    fn install_rejects_idempotent_false_pair() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let pack_bytes = b"real-pack-content-111";
        let idx_bytes = b"real-idx";
        let name = format!("{}", blake3::hash(pack_bytes).to_hex());

        // Corrupt pair with correct name but wrong content.
        write_file_atomic(&dst_pack_path(&packs, &name), b"wrong-content").unwrap();
        write_file_atomic(&dst_idx_path(&packs, &name), b"x").unwrap();
        assert!(!existing_pair_matches_pack_name(&packs, &name).unwrap());

        let out = install_pack_bytes_journaled(&packs, pack_bytes, idx_bytes).unwrap();
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

        let name = "deadbeef";
        let install_id = "test-install-1";
        validate_install_id(install_id).unwrap();
        validate_pack_name(name).unwrap();
        let stage = staging_dir(&packs, install_id);
        create_dir_all_durable(&stage).unwrap();
        write_file_atomic(&staging_pack_path(&packs, install_id), b"pack-body").unwrap();
        write_file_atomic(&staging_idx_path(&packs, install_id), b"idx-body").unwrap();

        let dst_pack = dst_pack_path(&packs, name);
        publish_file_durable(&staging_pack_path(&packs, install_id), &dst_pack).unwrap();

        let intent = PackInstallIntent {
            version: PACK_INSTALL_INTENT_VERSION,
            install_id: install_id.to_string(),
            pack_name: name.to_string(),
            phase: PackInstallPhase::PackPublished,
            created_unix: 1,
        };
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents(&packs).unwrap();
        assert_eq!(report.intents_seen, 1);
        assert_eq!(report.completed, 1);
        assert!(dst_pack.exists());
        assert!(dst_idx_path(&packs, name).exists());
        assert_eq!(fs::read(dst_idx_path(&packs, name)).unwrap(), b"idx-body");
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

        let mut intent = PackInstallIntent::new(install_id.into(), "aa".into());
        intent.created_unix = 1;
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(60)).unwrap();
        assert_eq!(report.aborted, 1);
        assert!(!dst_pack_path(&packs, "aa").exists());
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

        let intent = PackInstallIntent::new(install_id.into(), "bb".into());
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
        let name = "cc";
        let install_id = "orph-1";
        let dst_pack = dst_pack_path(&packs, name);
        write_file_atomic(&dst_pack, b"only-pack").unwrap();

        let intent = PackInstallIntent {
            version: PACK_INSTALL_INTENT_VERSION,
            install_id: install_id.into(),
            pack_name: name.into(),
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
    fn flock_serializes_recover_against_expired_live_install() {
        use std::{
            sync::{Arc, Barrier},
            thread,
            time::{Duration, Instant},
        };

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
            let guard = acquire_pack_install_lock(packs).expect("install lock");

            let install_id = "flock-live";
            let stage = staging_dir(packs, install_id);
            create_dir_all_durable(&stage).unwrap();
            let staging_pack = staging_pack_path(packs, install_id);
            let staging_idx = staging_idx_path(packs, install_id);
            write_file_atomic(&staging_pack, b"flock-pack").unwrap();
            write_file_atomic(&staging_idx, b"flock-idx").unwrap();
            let dst_pack = dst_pack_path(packs, "dd");
            let dst_idx = dst_idx_path(packs, "dd");

            let mut intent = PackInstallIntent::new(install_id.into(), "dd".into());
            intent.created_unix = 1;
            write_intent(packs, &intent).unwrap();

            planted_a.wait();
            thread::sleep(Duration::from_millis(80));

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
            let t0 = Instant::now();
            let report =
                recover_pack_install_intents_with_ttl(packs, Some(1)).expect("recover under flock");
            let waited = t0.elapsed();
            assert!(
                waited >= Duration::from_millis(50),
                "recover must block on install flock (elapsed {waited:?})"
            );
            assert_eq!(report.aborted, 0, "report={report:?}");
            assert!(dst_pack_path(packs, "dd").exists());
            assert!(dst_idx_path(packs, "dd").exists());
            done_b.wait();
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
        let dst_pack = dst_pack_path(&packs, "ee");
        publish_file_durable(&staging_pack_path(&packs, install_id), &dst_pack).unwrap();

        let intent = PackInstallIntent::new(install_id.into(), "ee".into());
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents(&packs).unwrap();
        assert_eq!(report.completed, 1);
        assert!(dst_pack.exists());
        assert!(dst_idx_path(&packs, "ee").exists());
        assert_eq!(fs::read(dst_idx_path(&packs, "ee")).unwrap(), b"idx-x");
    }

    #[test]
    fn journaled_install_idempotent_when_pair_exists() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let pack_bytes = b"idemp-pack-bytes";
        let name = format!("{}", blake3::hash(pack_bytes).to_hex());
        write_file_atomic(&dst_pack_path(&packs, &name), pack_bytes).unwrap();
        write_file_atomic(&dst_idx_path(&packs, &name), b"i").unwrap();

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
        let idx_bytes = b"in-memory-idx-body-zzz";
        let expected_name = format!("{}", blake3::hash(pack_bytes).to_hex());

        let name = install_pack_bytes_journaled(&packs, pack_bytes, idx_bytes).unwrap();
        assert_eq!(name, expected_name);
        assert!(existing_pair_matches_pack_name(&packs, &name).unwrap());

        let name2 = install_pack_bytes_journaled(&packs, pack_bytes, idx_bytes).unwrap();
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

        let mut intent = PackInstallIntent::new(install_id.into(), "ff".into());
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

        let name = "11";
        let install_id = "ttl-complete-1";
        let stage = staging_dir(&packs, install_id);
        create_dir_all_durable(&stage).unwrap();
        write_file_atomic(&staging_pack_path(&packs, install_id), b"pack-ttl").unwrap();
        write_file_atomic(&staging_idx_path(&packs, install_id), b"idx-ttl").unwrap();

        let dst_pack = dst_pack_path(&packs, name);
        publish_file_durable(&staging_pack_path(&packs, install_id), &dst_pack).unwrap();

        let intent = PackInstallIntent {
            version: PACK_INSTALL_INTENT_VERSION,
            install_id: install_id.into(),
            pack_name: name.into(),
            phase: PackInstallPhase::PackPublished,
            created_unix: 1,
        };
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(1)).unwrap();
        assert_eq!(report.completed, 1);
        assert!(dst_idx_path(&packs, name).exists());
    }

    #[test]
    fn relocated_repo_recovery_uses_new_packs_dir() {
        // Intent has only ids; moving the packs tree still recovers via new root.
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("old").join("packs");
        create_dir_all_durable(&packs).unwrap();
        let install_id = "reloc1";
        let name = "22";
        create_dir_all_durable(&staging_dir(&packs, install_id)).unwrap();
        write_file_atomic(&staging_pack_path(&packs, install_id), b"p").unwrap();
        write_file_atomic(&staging_idx_path(&packs, install_id), b"i").unwrap();
        publish_file_durable(
            &staging_pack_path(&packs, install_id),
            &dst_pack_path(&packs, name),
        )
        .unwrap();
        // Restage idx after pack publish consumed staging pack
        write_file_atomic(&staging_idx_path(&packs, install_id), b"i").unwrap();
        let intent = PackInstallIntent {
            version: 2,
            install_id: install_id.into(),
            pack_name: name.into(),
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
        assert!(dst_idx_path(&new_packs, name).exists());
    }

    #[test]
    fn concurrent_same_pack_installs_converge() {
        use std::thread;

        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let pack_bytes = b"same-pack-concurrent-body";
        let idx_bytes = b"same-idx";
        let expected = format!("{}", blake3::hash(pack_bytes).to_hex());

        let packs1 = packs.clone();
        let packs2 = packs.clone();
        let t1 = thread::spawn(move || {
            install_pack_bytes_journaled(&packs1, pack_bytes, idx_bytes).unwrap()
        });
        let t2 = thread::spawn(move || {
            install_pack_bytes_journaled(&packs2, pack_bytes, idx_bytes).unwrap()
        });
        let n1 = t1.join().unwrap();
        let n2 = t2.join().unwrap();
        assert_eq!(n1, expected);
        assert_eq!(n2, expected);
        assert!(existing_pair_matches_pack_name(&packs, &expected).unwrap());
    }
}
