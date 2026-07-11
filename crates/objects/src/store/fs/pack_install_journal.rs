// SPDX-License-Identifier: Apache-2.0
//! L8 A+ pack install journal: durable staging + intent (crash-safe install).
//!
//! Layout under the store packs directory (`.heddle/packs/`):
//! ```text
//! packs/
//!   <hash>.pack
//!   <hash>.idx
//!   .staging/<install_id>/pack
//!   .staging/<install_id>/idx
//!   .install-intent/<install_id>.json
//! ```
//!
//! Protocol (see `docs/program/L8_PACK_INSTALL_JOURNAL.md`):
//! 1. Hash pack bytes → content-addressed `pack_name`
//! 2. Stage pack + index under `.staging/<id>/` (atomic write or publish)
//! 3. Write intent `phase=prepared`
//! 4. Publish pack → final; intent `pack_published`
//! 5. Publish index → final; intent `completed`; remove staging + intent
//!
//! Recovery on store open / pack reload finishes or aborts incomplete installs.
//! Intents older than [`DEFAULT_PACK_INSTALL_INTENT_TTL_SECS`] are aborted unless
//! they can still complete quickly (final pack + staged index present).
//!
//! **Concurrency:** install and recover share an exclusive flock on
//! `packs/.pack-install.lock` (reentrant via [`crate::lock::RepoLock`]) so a
//! concurrent `reload_packs` cannot abort another thread's live install.
//! Non-expired Prepared/PackPublished intents that cannot complete yet are
//! treated as **in progress** and left alone (crash cleanup uses TTL expiry).
//!
//! Orphan `.staging/*` dirs without a matching intent are swept past TTL.
//! Unpaired final packs without intent are still GC'd via
//! [`super::fs_pack::prune_unpaired_pack_files`].

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    fs_atomic::{create_dir_all_durable, publish_file_durable, write_file_atomic},
    lock::RepoLock,
};

/// Intent schema version.
pub const PACK_INSTALL_INTENT_VERSION: u32 = 1;

/// Default TTL for abandoned install intents / orphan staging (24 hours).
pub const DEFAULT_PACK_INSTALL_INTENT_TTL_SECS: i64 = 86_400;

const STAGING_DIR_NAME: &str = ".staging";
const INTENT_DIR_NAME: &str = ".install-intent";
const STAGED_PACK_NAME: &str = "pack";
const STAGED_IDX_NAME: &str = "idx";

/// Install lifecycle phase recorded in the durable intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackInstallPhase {
    Prepared,
    PackPublished,
    Completed,
}

/// Durable intent for a single pack+index install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackInstallIntent {
    pub version: u32,
    pub install_id: String,
    /// Content-addressed pack stem (blake3 hex of pack bytes).
    pub pack_name: String,
    /// Absolute path to staged pack file.
    pub staging_pack: String,
    /// Absolute path to staged index file.
    pub staging_idx: String,
    /// Absolute path to final `.pack`.
    pub dst_pack: String,
    /// Absolute path to final `.idx`.
    pub dst_idx: String,
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
    pub errors: u64,
}

const PACK_INSTALL_LOCK_NAME: &str = ".pack-install.lock";

/// Exclusive install/recover lock for `packs_dir` (cross-thread + cross-process).
fn acquire_pack_install_lock(packs_dir: &Path) -> io::Result<crate::lock::WriteLockGuard> {
    create_dir_all_durable(packs_dir)?;
    let lock = RepoLock::at(packs_dir.join(PACK_INSTALL_LOCK_NAME));
    lock.write().map_err(|e| io::Error::other(e.to_string()))
}

impl PackInstallIntent {
    pub fn new(
        install_id: String,
        pack_name: String,
        staging_pack: PathBuf,
        staging_idx: PathBuf,
        dst_pack: PathBuf,
        dst_idx: PathBuf,
    ) -> Self {
        Self {
            version: PACK_INSTALL_INTENT_VERSION,
            install_id,
            pack_name,
            staging_pack: staging_pack.display().to_string(),
            staging_idx: staging_idx.display().to_string(),
            dst_pack: dst_pack.display().to_string(),
            dst_idx: dst_idx.display().to_string(),
            phase: PackInstallPhase::Prepared,
            created_unix: unix_now(),
        }
    }

    pub fn staging_pack_path(&self) -> PathBuf {
        PathBuf::from(&self.staging_pack)
    }

    pub fn staging_idx_path(&self) -> PathBuf {
        PathBuf::from(&self.staging_idx)
    }

    pub fn dst_pack_path(&self) -> PathBuf {
        PathBuf::from(&self.dst_pack)
    }

    pub fn dst_idx_path(&self) -> PathBuf {
        PathBuf::from(&self.dst_idx)
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn staging_root(packs_dir: &Path) -> PathBuf {
    packs_dir.join(STAGING_DIR_NAME)
}

pub(crate) fn intent_root(packs_dir: &Path) -> PathBuf {
    packs_dir.join(INTENT_DIR_NAME)
}

pub(crate) fn intent_path(packs_dir: &Path, install_id: &str) -> PathBuf {
    intent_root(packs_dir).join(format!("{install_id}.json"))
}

pub(crate) fn staging_dir(packs_dir: &Path, install_id: &str) -> PathBuf {
    staging_root(packs_dir).join(install_id)
}

/// Write intent atomically (phase updates use the same path).
pub(crate) fn write_intent(packs_dir: &Path, intent: &PackInstallIntent) -> std::io::Result<()> {
    create_dir_all_durable(&intent_root(packs_dir))?;
    let path = intent_path(packs_dir, &intent.install_id);
    let bytes = serde_json::to_vec_pretty(intent)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_file_atomic(&path, &bytes)
}

pub(crate) fn load_intent(path: &Path) -> std::io::Result<PackInstallIntent> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

pub(crate) fn remove_intent(packs_dir: &Path, install_id: &str) -> std::io::Result<()> {
    let path = intent_path(packs_dir, install_id);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn remove_path_best_effort(path: &Path) {
    if path.is_dir() {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

/// Remove staging directory for an install id.
pub(crate) fn remove_staging(packs_dir: &Path, install_id: &str) {
    remove_path_best_effort(&staging_dir(packs_dir, install_id));
}

/// Abort an incomplete install: drop partial finals, staging, intent.
pub(crate) fn abort_install(packs_dir: &Path, intent: &PackInstallIntent) -> std::io::Result<()> {
    let dst_pack = intent.dst_pack_path();
    let dst_idx = intent.dst_idx_path();
    // Only remove final pack if index is missing (partial publish).
    if dst_pack.exists() && !dst_idx.exists() {
        let _ = fs::remove_file(&dst_pack);
        if let Some(parent) = dst_pack.parent() {
            let _ = crate::fs_atomic::sync_directory(parent);
        }
    }
    // Never delete a complete pack+idx pair on abort of a *different* install
    // of the same content-addressed name — if both exist, leave them.
    remove_staging(packs_dir, &intent.install_id);
    remove_intent(packs_dir, &intent.install_id)?;
    Ok(())
}

/// Finish a pack_published install when staged index is still available.
pub(crate) fn complete_from_staging(
    packs_dir: &Path,
    intent: &PackInstallIntent,
) -> io::Result<()> {
    let staging_idx = intent.staging_idx_path();
    let dst_idx = intent.dst_idx_path();
    let dst_pack = intent.dst_pack_path();

    if dst_pack.exists() && dst_idx.exists() {
        // Already fully published (e.g. completed then crash before intent delete).
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

/// True when recovery can finish the install without re-staging (prefer over TTL abort).
fn can_complete_quickly(intent: &PackInstallIntent) -> bool {
    let dst_pack = intent.dst_pack_path();
    let dst_idx = intent.dst_idx_path();
    let staging_idx = intent.staging_idx_path();
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

/// Recover all intents under `packs_dir` with the default 24h TTL. Idempotent.
pub fn recover_pack_install_intents(packs_dir: &Path) -> io::Result<PackInstallRecoverReport> {
    recover_pack_install_intents_with_ttl(packs_dir, Some(DEFAULT_PACK_INSTALL_INTENT_TTL_SECS))
}

/// Recover install intents with an optional TTL (seconds).
///
/// Policy:
/// - Hold `packs/.pack-install.lock` for the whole recovery (serializes with install).
/// - If the install can complete (final pack + staged idx, or both finals) →
///   complete/cleanup **regardless of TTL**.
/// - Else if `created_unix + ttl < now` → **abort** (crash / abandoned install).
/// - Else → **skip in progress** (do not abort a concurrent live install).
///
/// Also sweeps orphan `.staging/*` dirs with no matching intent file when older
/// than the TTL (best-effort). `ttl_secs = None` disables expiry and orphan sweep.
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
                tracing::debug!(
                    ?packs_dir,
                    ?report,
                    "pack install journal recovery (no intent dir)"
                );
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
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            report.intents_seen += 1;
            let intent = match load_intent(&path) {
                Ok(i) if i.version == PACK_INSTALL_INTENT_VERSION => i,
                Ok(_) | Err(_) => {
                    // Unknown/corrupt: best-effort remove intent file only.
                    let _ = fs::remove_file(&path);
                    report.errors += 1;
                    continue;
                }
            };

            let expired = intent_expired(&intent, ttl_secs, now);
            let result = recover_one_intent(packs_dir, &intent, expired, &mut report);
            if result.is_err() {
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
    {
        tracing::info!(
            ?packs_dir,
            intents_seen = report.intents_seen,
            completed = report.completed,
            aborted = report.aborted,
            skipped_in_progress = report.skipped_in_progress,
            cleaned_stale_completed = report.cleaned_stale_completed,
            orphan_staging_swept = report.orphan_staging_swept,
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
    // Prefer complete over TTL abort when we can finish without re-staging.
    if can_complete_quickly(intent) {
        return match intent.phase {
            PackInstallPhase::Prepared | PackInstallPhase::PackPublished => {
                let dst_pack = intent.dst_pack_path();
                let dst_idx = intent.dst_idx_path();
                if dst_pack.exists() && dst_idx.exists() {
                    remove_staging(packs_dir, &intent.install_id);
                    remove_intent(packs_dir, &intent.install_id)?;
                    report.cleaned_stale_completed += 1;
                    Ok(())
                } else {
                    complete_from_staging(packs_dir, intent)?;
                    if intent.dst_pack_path().exists() && intent.dst_idx_path().exists() {
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
            created_unix = intent.created_unix,
            "aborting expired pack install intent"
        );
        abort_install(packs_dir, intent)?;
        report.aborted += 1;
        return Ok(());
    }

    // Not expired and cannot complete yet: treat as a live concurrent install
    // (or a crash that is still within TTL). Do **not** delete staging.
    match intent.phase {
        PackInstallPhase::Prepared | PackInstallPhase::PackPublished => {
            tracing::debug!(
                install_id = %intent.install_id,
                pack_name = %intent.pack_name,
                phase = ?intent.phase,
                "skipping in-progress pack install intent"
            );
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

/// Best-effort remove `.staging/<id>` dirs with no matching intent past TTL.
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
        if intent_path(packs_dir, id).exists() {
            continue;
        }
        let age_ok_to_sweep = path_mtime_unix(&path)
            .map(|mtime| mtime.saturating_add(ttl) < now)
            .unwrap_or(true);
        if !age_ok_to_sweep {
            continue;
        }
        tracing::debug!(staging = %path.display(), "sweeping orphan pack install staging");
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

/// Journaled in-memory install: stage pack+idx bytes, intent, publish pack then index.
///
/// Returns the content-addressed `pack_name` (blake3 hex of `pack_data`).
/// On success both final files exist and staging/intent are gone.
pub fn install_pack_bytes_journaled(
    packs_dir: &Path,
    pack_data: &[u8],
    index_data: &[u8],
) -> io::Result<String> {
    let _guard = acquire_pack_install_lock(packs_dir)?;
    install_pack_bytes_journaled_locked(packs_dir, pack_data, index_data)
}

fn install_pack_bytes_journaled_locked(
    packs_dir: &Path,
    pack_data: &[u8],
    index_data: &[u8],
) -> io::Result<String> {
    create_dir_all_durable(packs_dir)?;

    let pack_name = format!("{}", blake3::hash(pack_data).to_hex());
    let dst_pack = packs_dir.join(format!("{pack_name}.pack"));
    let dst_idx = packs_dir.join(format!("{pack_name}.idx"));

    // Idempotent: already fully installed.
    if dst_pack.exists() && dst_idx.exists() {
        return Ok(pack_name);
    }

    // Partial final from pre-journal crash: remove orphan pack so we can
    // reinstall cleanly from sources.
    if dst_pack.exists() && !dst_idx.exists() {
        let _ = fs::remove_file(&dst_pack);
    }

    let install_id = new_install_id();
    let stage = staging_dir(packs_dir, &install_id);
    create_dir_all_durable(&stage)?;

    let staging_pack = stage.join(STAGED_PACK_NAME);
    let staging_idx = stage.join(STAGED_IDX_NAME);

    // Durable writes into staging (not yet published to final names).
    write_file_atomic(&staging_pack, pack_data)?;
    write_file_atomic(&staging_idx, index_data)?;

    let mut intent = PackInstallIntent::new(
        install_id.clone(),
        pack_name.clone(),
        staging_pack.clone(),
        staging_idx.clone(),
        dst_pack.clone(),
        dst_idx.clone(),
    );
    write_intent(packs_dir, &intent)?;

    publish_file_durable(&staging_pack, &dst_pack)?;
    intent.phase = PackInstallPhase::PackPublished;
    write_intent(packs_dir, &intent)?;

    publish_file_durable(&staging_idx, &dst_idx)?;
    intent.phase = PackInstallPhase::Completed;
    write_intent(packs_dir, &intent)?;

    remove_staging(packs_dir, &install_id);
    remove_intent(packs_dir, &install_id)?;
    Ok(pack_name)
}

/// Journaled streaming install: stage sources, intent, publish pack then index.
///
/// Consumes `src_pack_path` / `src_index_path` (moved into staging then finals).
/// On success both final files exist and staging/intent are gone.
pub fn install_pack_files_journaled(
    packs_dir: &Path,
    src_pack_path: &Path,
    src_index_path: &Path,
    pack_name: &str,
) -> io::Result<()> {
    let _guard = acquire_pack_install_lock(packs_dir)?;
    install_pack_files_journaled_locked(packs_dir, src_pack_path, src_index_path, pack_name)
}

fn install_pack_files_journaled_locked(
    packs_dir: &Path,
    src_pack_path: &Path,
    src_index_path: &Path,
    pack_name: &str,
) -> io::Result<()> {
    create_dir_all_durable(packs_dir)?;

    let dst_pack = packs_dir.join(format!("{pack_name}.pack"));
    let dst_idx = packs_dir.join(format!("{pack_name}.idx"));

    // Idempotent: already fully installed.
    if dst_pack.exists() && dst_idx.exists() {
        let _ = fs::remove_file(src_pack_path);
        let _ = fs::remove_file(src_index_path);
        return Ok(());
    }

    // Partial final from pre-journal crash: remove orphan pack so we can
    // reinstall cleanly from sources.
    if dst_pack.exists() && !dst_idx.exists() {
        let _ = fs::remove_file(&dst_pack);
    }

    let install_id = new_install_id();
    let stage = staging_dir(packs_dir, &install_id);
    create_dir_all_durable(&stage)?;

    let staging_pack = stage.join(STAGED_PACK_NAME);
    let staging_idx = stage.join(STAGED_IDX_NAME);

    // Move sources into durable staging (fsync + rename).
    publish_file_durable(src_pack_path, &staging_pack)?;
    publish_file_durable(src_index_path, &staging_idx)?;

    let mut intent = PackInstallIntent::new(
        install_id.clone(),
        pack_name.to_string(),
        staging_pack.clone(),
        staging_idx.clone(),
        dst_pack.clone(),
        dst_idx.clone(),
    );
    write_intent(packs_dir, &intent)?;

    // Publish pack.
    publish_file_durable(&staging_pack, &dst_pack)?;
    intent.phase = PackInstallPhase::PackPublished;
    write_intent(packs_dir, &intent)?;

    // Publish index.
    publish_file_durable(&staging_idx, &dst_idx)?;
    intent.phase = PackInstallPhase::Completed;
    write_intent(packs_dir, &intent)?;

    remove_staging(packs_dir, &install_id);
    remove_intent(packs_dir, &install_id)?;
    Ok(())
}

fn new_install_id() -> String {
    // Time + random keeps ids unique under concurrent installs without extra deps.
    let t = unix_now() as u64;
    let r: u64 = rand::random();
    format!("{t:016x}-{r:016x}")
}

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
        let name = "abc123";

        install_pack_files_journaled(&packs, &src_pack, &src_idx, name).unwrap();

        assert!(packs.join(format!("{name}.pack")).exists());
        assert!(packs.join(format!("{name}.idx")).exists());
        assert_eq!(
            fs::read(packs.join(format!("{name}.pack"))).unwrap(),
            pack_bytes
        );
        assert!(
            !intent_root(&packs).exists()
                || fs::read_dir(intent_root(&packs)).unwrap().count() == 0
        );
        assert!(
            !staging_root(&packs).exists()
                || fs::read_dir(staging_root(&packs))
                    .map(|d| d.count() == 0)
                    .unwrap_or(true)
        );
    }

    #[test]
    fn recover_pack_published_completes_from_staging() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();

        let name = "deadbeef";
        let install_id = "test-install-1".to_string();
        let stage = staging_dir(&packs, &install_id);
        create_dir_all_durable(&stage).unwrap();
        let staging_pack = stage.join(STAGED_PACK_NAME);
        let staging_idx = stage.join(STAGED_IDX_NAME);
        write_file_atomic(&staging_pack, b"pack-body").unwrap();
        write_file_atomic(&staging_idx, b"idx-body").unwrap();

        let dst_pack = packs.join(format!("{name}.pack"));
        let dst_idx = packs.join(format!("{name}.idx"));
        // Simulate: pack already published, index still staged.
        publish_file_durable(&staging_pack, &dst_pack).unwrap();

        let intent = PackInstallIntent {
            version: PACK_INSTALL_INTENT_VERSION,
            install_id: install_id.clone(),
            pack_name: name.to_string(),
            staging_pack: staging_pack.display().to_string(),
            staging_idx: staging_idx.display().to_string(),
            dst_pack: dst_pack.display().to_string(),
            dst_idx: dst_idx.display().to_string(),
            phase: PackInstallPhase::PackPublished,
            created_unix: 1,
        };
        // staging_pack was moved; re-create path string still points at missing pack file — ok.
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents(&packs).unwrap();
        assert_eq!(report.intents_seen, 1);
        assert_eq!(report.completed, 1);
        assert!(dst_pack.exists());
        assert!(dst_idx.exists());
        assert_eq!(fs::read(&dst_idx).unwrap(), b"idx-body");
        assert!(!intent_path(&packs, &install_id).exists());
    }

    #[test]
    fn recover_prepared_aborts_without_finals_when_expired() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let install_id = "prep-abort".to_string();
        let stage = staging_dir(&packs, &install_id);
        create_dir_all_durable(&stage).unwrap();
        let staging_pack = stage.join(STAGED_PACK_NAME);
        let staging_idx = stage.join(STAGED_IDX_NAME);
        write_file_atomic(&staging_pack, b"p").unwrap();
        write_file_atomic(&staging_idx, b"i").unwrap();

        let mut intent = PackInstallIntent::new(
            install_id.clone(),
            "name1".into(),
            staging_pack,
            staging_idx,
            packs.join("name1.pack"),
            packs.join("name1.idx"),
        );
        intent.created_unix = 1; // expired under any short TTL
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(60)).unwrap();
        assert_eq!(report.aborted, 1);
        assert!(!packs.join("name1.pack").exists());
        assert!(!packs.join("name1.idx").exists());
        assert!(!intent_path(&packs, &install_id).exists());
        assert!(!stage.exists());
    }

    #[test]
    fn recover_prepared_fresh_skips_in_progress() {
        // Concurrent install mid-way: Prepared, staging present, not expired.
        // Recover must not delete the live installer's staging.
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let install_id = "live-prep".to_string();
        let stage = staging_dir(&packs, &install_id);
        create_dir_all_durable(&stage).unwrap();
        let staging_pack = stage.join(STAGED_PACK_NAME);
        let staging_idx = stage.join(STAGED_IDX_NAME);
        write_file_atomic(&staging_pack, b"live-p").unwrap();
        write_file_atomic(&staging_idx, b"live-i").unwrap();

        let intent = PackInstallIntent::new(
            install_id.clone(),
            "live-name".into(),
            staging_pack.clone(),
            staging_idx.clone(),
            packs.join("live-name.pack"),
            packs.join("live-name.idx"),
        );
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(86_400)).unwrap();
        assert_eq!(report.skipped_in_progress, 1);
        assert_eq!(report.aborted, 0);
        assert!(staging_pack.exists());
        assert!(staging_idx.exists());
        assert!(intent_path(&packs, &install_id).exists());
    }

    #[test]
    fn recover_pack_published_without_staging_idx_aborts_orphan_pack() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let name = "orphan";
        let install_id = "orph-1".to_string();
        let dst_pack = packs.join(format!("{name}.pack"));
        write_file_atomic(&dst_pack, b"only-pack").unwrap();

        let intent = PackInstallIntent {
            version: PACK_INSTALL_INTENT_VERSION,
            install_id: install_id.clone(),
            pack_name: name.into(),
            staging_pack: staging_dir(&packs, &install_id)
                .join(STAGED_PACK_NAME)
                .display()
                .to_string(),
            staging_idx: staging_dir(&packs, &install_id)
                .join(STAGED_IDX_NAME)
                .display()
                .to_string(),
            dst_pack: dst_pack.display().to_string(),
            dst_idx: packs.join(format!("{name}.idx")).display().to_string(),
            phase: PackInstallPhase::PackPublished,
            created_unix: 1,
        };
        write_intent(&packs, &intent).unwrap();

        // created_unix=1 → expired under default 24h TTL in 2026+.
        let report = recover_pack_install_intents(&packs).unwrap();
        assert_eq!(report.aborted, 1);
        assert!(!dst_pack.exists());
        assert!(!intent_path(&packs, &install_id).exists());
    }

    #[test]
    fn concurrent_reload_does_not_abort_live_install() {
        use std::{
            sync::{Arc, Barrier},
            thread,
        };

        let root = tempfile::tempdir().unwrap();
        let packs = Arc::new(root.path().join("packs"));
        create_dir_all_durable(&packs).unwrap();

        // Thread A: plant a live Prepared install (mid-protocol), wait, then finish.
        let barrier = Arc::new(Barrier::new(2));
        let packs_a = Arc::clone(&packs);
        let barrier_a = Arc::clone(&barrier);
        let installer = thread::spawn(move || {
            let packs = packs_a.as_path();
            let install_id = "concurrent-live".to_string();
            let stage = staging_dir(packs, &install_id);
            create_dir_all_durable(&stage).unwrap();
            let staging_pack = stage.join(STAGED_PACK_NAME);
            let staging_idx = stage.join(STAGED_IDX_NAME);
            write_file_atomic(&staging_pack, b"conc-pack").unwrap();
            write_file_atomic(&staging_idx, b"conc-idx").unwrap();
            let dst_pack = packs.join("conc.pack");
            let dst_idx = packs.join("conc.idx");
            let mut intent = PackInstallIntent::new(
                install_id.clone(),
                "conc".into(),
                staging_pack.clone(),
                staging_idx.clone(),
                dst_pack.clone(),
                dst_idx.clone(),
            );
            write_intent(packs, &intent).unwrap();
            // Signal recoverer: intent is Prepared, staging present.
            barrier_a.wait();
            // Wait until recoverer has run.
            barrier_a.wait();
            // Staging must still exist (recoverer did not abort).
            assert!(
                staging_pack.exists() && staging_idx.exists(),
                "concurrent recover must not delete live staging"
            );
            // Finish install as the live writer would.
            publish_file_durable(&staging_pack, &dst_pack).unwrap();
            intent.phase = PackInstallPhase::PackPublished;
            write_intent(packs, &intent).unwrap();
            publish_file_durable(&staging_idx, &dst_idx).unwrap();
            remove_staging(packs, &install_id);
            remove_intent(packs, &install_id).unwrap();
            assert!(dst_pack.exists() && dst_idx.exists());
        });

        let packs_b = Arc::clone(&packs);
        let barrier_b = Arc::clone(&barrier);
        let recoverer = thread::spawn(move || {
            barrier_b.wait(); // wait until Prepared intent exists
            let report = recover_pack_install_intents_with_ttl(packs_b.as_path(), Some(86_400))
                .expect("recover");
            assert_eq!(report.aborted, 0, "must not abort live install");
            assert!(
                report.skipped_in_progress >= 1,
                "expected skip of in-progress intent, got {report:?}"
            );
            barrier_b.wait(); // let installer finish
        });

        installer.join().expect("installer");
        recoverer.join().expect("recoverer");
        assert!(packs.join("conc.pack").exists());
        assert!(packs.join("conc.idx").exists());
    }

    #[test]
    fn recover_prepared_with_pack_and_staging_idx_completes() {
        // Crash between pack publish and phase=pack_published write.
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let install_id = "prep-complete".to_string();
        let stage = staging_dir(&packs, &install_id);
        create_dir_all_durable(&stage).unwrap();
        let staging_pack = stage.join(STAGED_PACK_NAME);
        let staging_idx = stage.join(STAGED_IDX_NAME);
        write_file_atomic(&staging_pack, b"pack-x").unwrap();
        write_file_atomic(&staging_idx, b"idx-x").unwrap();
        let dst_pack = packs.join("namex.pack");
        publish_file_durable(&staging_pack, &dst_pack).unwrap();

        let intent = PackInstallIntent::new(
            install_id.clone(),
            "namex".into(),
            staging_pack,
            staging_idx,
            dst_pack.clone(),
            packs.join("namex.idx"),
        );
        // Still marked prepared (phase flip never reached).
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents(&packs).unwrap();
        assert_eq!(report.completed, 1);
        assert!(dst_pack.exists());
        assert!(packs.join("namex.idx").exists());
        assert_eq!(fs::read(packs.join("namex.idx")).unwrap(), b"idx-x");
    }

    #[test]
    fn journaled_install_idempotent_when_pair_exists() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let name = "idemp";
        write_file_atomic(&packs.join(format!("{name}.pack")), b"p").unwrap();
        write_file_atomic(&packs.join(format!("{name}.idx")), b"i").unwrap();

        let src_dir = root.path().join("src");
        create_dir_all_durable(&src_dir).unwrap();
        let src_pack = write_src(&src_dir, "a", b"other");
        let src_idx = write_src(&src_dir, "b", b"other-i");

        install_pack_files_journaled(&packs, &src_pack, &src_idx, name).unwrap();
        assert_eq!(fs::read(packs.join(format!("{name}.pack"))).unwrap(), b"p");
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
        assert!(packs.join(format!("{name}.pack")).exists());
        assert!(packs.join(format!("{name}.idx")).exists());
        assert_eq!(
            fs::read(packs.join(format!("{name}.pack"))).unwrap(),
            pack_bytes
        );
        assert_eq!(
            fs::read(packs.join(format!("{name}.idx"))).unwrap(),
            idx_bytes
        );
        assert!(
            !intent_root(&packs).exists()
                || fs::read_dir(intent_root(&packs)).unwrap().count() == 0
        );
        assert!(
            !staging_root(&packs).exists()
                || fs::read_dir(staging_root(&packs))
                    .map(|d| d.count() == 0)
                    .unwrap_or(true)
        );

        // Idempotent second call.
        let name2 = install_pack_bytes_journaled(&packs, pack_bytes, idx_bytes).unwrap();
        assert_eq!(name2, expected_name);
    }

    #[test]
    fn ttl_aborts_old_prepared_intent() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();
        let install_id = "ttl-prep".to_string();
        let stage = staging_dir(&packs, &install_id);
        create_dir_all_durable(&stage).unwrap();
        let staging_pack = stage.join(STAGED_PACK_NAME);
        let staging_idx = stage.join(STAGED_IDX_NAME);
        write_file_atomic(&staging_pack, b"stale-p").unwrap();
        write_file_atomic(&staging_idx, b"stale-i").unwrap();

        let mut intent = PackInstallIntent::new(
            install_id.clone(),
            "stale-name".into(),
            staging_pack,
            staging_idx,
            packs.join("stale-name.pack"),
            packs.join("stale-name.idx"),
        );
        // Far in the past relative to any positive TTL.
        intent.created_unix = 1;
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(60)).unwrap();
        assert_eq!(report.intents_seen, 1);
        assert_eq!(report.aborted, 1);
        assert_eq!(report.completed, 0);
        assert!(!intent_path(&packs, &install_id).exists());
        assert!(!stage.exists());
        assert!(!packs.join("stale-name.pack").exists());
    }

    #[test]
    fn complete_preferred_over_ttl_when_staging_idx_present() {
        let root = tempfile::tempdir().unwrap();
        let packs = root.path().join("packs");
        create_dir_all_durable(&packs).unwrap();

        let name = "ttl-complete";
        let install_id = "ttl-complete-1".to_string();
        let stage = staging_dir(&packs, &install_id);
        create_dir_all_durable(&stage).unwrap();
        let staging_pack = stage.join(STAGED_PACK_NAME);
        let staging_idx = stage.join(STAGED_IDX_NAME);
        write_file_atomic(&staging_pack, b"pack-ttl").unwrap();
        write_file_atomic(&staging_idx, b"idx-ttl").unwrap();

        let dst_pack = packs.join(format!("{name}.pack"));
        let dst_idx = packs.join(format!("{name}.idx"));
        publish_file_durable(&staging_pack, &dst_pack).unwrap();

        let intent = PackInstallIntent {
            version: PACK_INSTALL_INTENT_VERSION,
            install_id: install_id.clone(),
            pack_name: name.to_string(),
            staging_pack: staging_pack.display().to_string(),
            staging_idx: staging_idx.display().to_string(),
            dst_pack: dst_pack.display().to_string(),
            dst_idx: dst_idx.display().to_string(),
            phase: PackInstallPhase::PackPublished,
            // Expired under any short TTL, but completable.
            created_unix: 1,
        };
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents_with_ttl(&packs, Some(1)).unwrap();
        assert_eq!(report.intents_seen, 1);
        assert_eq!(report.completed, 1);
        assert_eq!(report.aborted, 0);
        assert!(dst_pack.exists());
        assert!(dst_idx.exists());
        assert_eq!(fs::read(&dst_idx).unwrap(), b"idx-ttl");
        assert!(!intent_path(&packs, &install_id).exists());
    }
}
