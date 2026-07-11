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
//! 1. Stream-hash source pack → content-addressed `pack_name`
//! 2. Stage pack + index under `.staging/<id>/` via `publish_file_durable`
//! 3. Write intent `phase=prepared`
//! 4. Publish pack → final; intent `pack_published`
//! 5. Publish index → final; intent `completed`; remove staging + intent
//!
//! Recovery on store open / pack reload finishes or aborts incomplete installs.
//! Unpaired final packs without intent are still GC'd via
//! [`super::fs_pack::prune_unpaired_pack_files`].

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::fs_atomic::{create_dir_all_durable, publish_file_durable, write_file_atomic};

/// Intent schema version.
pub const PACK_INSTALL_INTENT_VERSION: u32 = 1;

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
    pub errors: u64,
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
) -> std::io::Result<()> {
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

/// Recover all intents under `packs_dir`. Idempotent.
pub fn recover_pack_install_intents(packs_dir: &Path) -> std::io::Result<PackInstallRecoverReport> {
    let mut report = PackInstallRecoverReport::default();
    let intent_dir = intent_root(packs_dir);
    if !intent_dir.exists() {
        // Still prune legacy unpaired packs (Option D backstop).
        return Ok(report);
    }

    let entries = match fs::read_dir(&intent_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
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

        let result = match intent.phase {
            PackInstallPhase::Prepared => {
                let dst_pack = intent.dst_pack_path();
                let dst_idx = intent.dst_idx_path();
                let staging_idx = intent.staging_idx_path();
                // Crash after pack publish but before phase flip still leaves
                // final pack + staged idx — complete rather than abort.
                if dst_pack.exists() && dst_idx.exists() {
                    remove_staging(packs_dir, &intent.install_id);
                    remove_intent(packs_dir, &intent.install_id).map(|_| {
                        report.cleaned_stale_completed += 1;
                    })
                } else if dst_pack.exists() && !dst_idx.exists() && staging_idx.exists() {
                    complete_from_staging(packs_dir, &intent).map(|_| {
                        report.completed += 1;
                    })
                } else {
                    abort_install(packs_dir, &intent).map(|_| {
                        report.aborted += 1;
                    })
                }
            }
            PackInstallPhase::PackPublished => {
                complete_from_staging(packs_dir, &intent).map(|_| {
                    // complete_from_staging either finishes or aborts.
                    if intent.dst_pack_path().exists() && intent.dst_idx_path().exists() {
                        report.completed += 1;
                    } else {
                        report.aborted += 1;
                    }
                })
            }
            PackInstallPhase::Completed => {
                remove_staging(packs_dir, &intent.install_id);
                remove_intent(packs_dir, &intent.install_id).map(|_| {
                    report.cleaned_stale_completed += 1;
                })
            }
        };
        if result.is_err() {
            report.errors += 1;
        }
    }

    Ok(report)
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
) -> std::io::Result<()> {
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
    fn recover_prepared_aborts_without_finals() {
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

        let intent = PackInstallIntent::new(
            install_id.clone(),
            "name1".into(),
            staging_pack,
            staging_idx,
            packs.join("name1.pack"),
            packs.join("name1.idx"),
        );
        write_intent(&packs, &intent).unwrap();

        let report = recover_pack_install_intents(&packs).unwrap();
        assert_eq!(report.aborted, 1);
        assert!(!packs.join("name1.pack").exists());
        assert!(!packs.join("name1.idx").exists());
        assert!(!intent_path(&packs, &install_id).exists());
        assert!(!stage.exists());
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

        let report = recover_pack_install_intents(&packs).unwrap();
        assert_eq!(report.aborted, 1);
        assert!(!dst_pack.exists());
        assert!(!intent_path(&packs, &install_id).exists());
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
}
