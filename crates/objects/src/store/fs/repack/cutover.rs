// SPDX-License-Identifier: Apache-2.0

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::Path,
    thread,
    time::Duration,
};

use fs2::FileExt;

use super::{
    super::{FsStore, fs_paths::packs_dir, npk1::Npk1Manager},
    staging::RepackSnapshot,
};
use crate::{
    fs_atomic::sync_directory,
    object::ContentHash,
    store::{
        HeddleError, Result, SnapshotPackManager,
        pack::{RepackContext, RepackError},
        snapshot_commit::snapshot_commit_marker_path,
    },
};

const REPACK_LOCK_FILE: &str = ".repack.lock";

pub(super) struct CutoverStats {
    pub(super) removed_pack_bytes: u64,
    pub(super) replacement_bytes: u64,
}

pub(super) fn cutover(
    store: &FsStore,
    snapshot: &RepackSnapshot,
    new_name: &str,
    new_npk1_name: Option<&str>,
    replacement_preexisting: bool,
    npk1_preexisting: bool,
) -> Result<CutoverStats> {
    let mut manager = store
        .pack_manager()
        .write()
        .map_err(|_| HeddleError::Config("Failed to acquire pack manager lock".to_string()))?;
    let mut npk1_manager = store
        .npk1_manager()
        .write()
        .map_err(|_| HeddleError::Config("Failed to acquire NPK1 manager lock".to_string()))?;
    let mut removed_bytes = 0u64;
    let mut first_error = None;
    for (pack, index) in &snapshot.old_pack_files {
        if pack.file_stem().and_then(|stem| stem.to_str()) == Some(new_name) {
            continue;
        }
        for path in [pack, index] {
            let bytes = file_len(path);
            match fs::remove_file(path) {
                Ok(()) => removed_bytes = removed_bytes.saturating_add(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        for artifact_id in &snapshot.commit_artifact_ids {
            let marker = snapshot_commit_marker_path(pack, artifact_id);
            let _ = fs::remove_file(marker);
        }
    }
    for path in &snapshot.old_npk1_files {
        if path.file_stem().and_then(|stem| stem.to_str()) == new_npk1_name {
            continue;
        }
        let bytes = file_len(path);
        match fs::remove_file(path) {
            Ok(()) => removed_bytes = removed_bytes.saturating_add(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    sync_directory(&packs_dir(store.root()))?;
    *manager = SnapshotPackManager::new(packs_dir(store.root()));
    *npk1_manager = Npk1Manager::new(packs_dir(store.root()));
    drop(manager);
    drop(npk1_manager);
    store.clear_recent_object_caches();
    if let Some(error) = first_error {
        return Err(error.into());
    }
    let mut replacement_bytes = if replacement_preexisting {
        0
    } else {
        file_len(&packs_dir(store.root()).join(format!("{new_name}.pack"))).saturating_add(
            file_len(&packs_dir(store.root()).join(format!("{new_name}.idx"))),
        )
    };
    if !npk1_preexisting && let Some(name) = new_npk1_name {
        replacement_bytes = replacement_bytes.saturating_add(file_len(
            &packs_dir(store.root()).join(format!("{name}.npk")),
        ));
    }
    Ok(CutoverStats {
        removed_pack_bytes: removed_bytes,
        replacement_bytes,
    })
}

pub(super) fn publish_npk1(packs: &Path, staged: &Path) -> Result<(String, bool)> {
    let name = hash_file(staged)?;
    let target = packs.join(format!("{name}.npk"));
    if target.exists() {
        if hash_file(&target)? != name {
            return Err(HeddleError::InvalidObject(
                "existing NPK1 filename does not match its contents".to_string(),
            ));
        }
        fs::remove_file(staged)?;
        return Ok((name, true));
    }
    fs::rename(staged, &target)?;
    sync_directory(packs)?;
    Ok((name, false))
}

pub(super) fn preserve_commit_markers(
    packs: &Path,
    new_name: &str,
    artifact_ids: &[ContentHash],
) -> std::io::Result<()> {
    let pack = packs.join(format!("{new_name}.pack"));
    for artifact_id in artifact_ids {
        let marker = snapshot_commit_marker_path(&pack, artifact_id);
        match OpenOptions::new().write(true).create_new(true).open(marker) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    sync_directory(packs)
}

pub(super) fn acquire_repack_lock(
    packs: &Path,
    context: &RepackContext,
) -> std::result::Result<File, RepackError> {
    let file = open_lock_file(packs).map_err(RepackError::operation)?;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                context.checkpoint(0)?;
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(RepackError::operation(error)),
        }
    }
}

pub(in crate::store::fs) fn acquire_repack_lock_blocking(packs: &Path) -> Result<File> {
    let file = open_lock_file(packs)?;
    file.lock_exclusive()?;
    Ok(file)
}

fn open_lock_file(packs: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(packs.join(REPACK_LOCK_FILE))
}

pub(super) fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 64 * 1024];
    file.seek(SeekFrom::Start(0))?;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub(super) fn object_file_len(dir: &Path, hash: &ContentHash) -> u64 {
    let hex = hash.to_hex();
    file_len(&dir.join(&hex[..2]).join(&hex[2..]))
}

pub(super) fn file_len(path: &Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}
