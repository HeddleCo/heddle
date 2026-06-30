// SPDX-License-Identifier: Apache-2.0
//! Pack and prune operations for FsStore.

use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

use super::{
    FsStore,
    fs_io::list_hashes_from_dir,
    fs_paths::{blobs_dir, hash_path, packs_dir, trees_dir},
};
use crate::{
    object::ContentHash,
    store::{
        HeddleError, ObjectStore, Result,
        pack::{
            ObjectType as PackObjectType, PackBuilder, PackObjectId, PackStats,
            StreamingPackBuilder,
        },
    },
};

fn remove_file_ignore_missing(path: &std::path::Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(HeddleError::from(e)),
    }
}

impl FsStore {
    /// Bulk-install many blobs as a single packfile. Two fsyncs total
    /// (one for `.pack`, one for `.idx`) regardless of blob count —
    /// vs. N×fsync if each blob were written loose. Used by the
    /// snapshot hot path; called at the end of the tree walk with
    /// every new blob accumulated in memory.
    ///
    /// Skips blobs already in the store (whether loose or packed) so
    /// re-snapshotting an unchanged worktree doesn't churn the pack
    /// directory. With every blob already known, this is a no-op.
    pub(super) fn put_blobs_packed_impl(&self, blobs: Vec<(ContentHash, Vec<u8>)>) -> Result<()> {
        if blobs.is_empty() {
            return Ok(());
        }
        // Snapshot-time pack: skip the sliding-window delta search.
        // It's a CPU win on similar-content files (the GC packer
        // benefits) but for a single snapshot the inputs are
        // unrelated content (random binaries, small text, etc.) and
        // every pair-wise delta estimate runs across the full
        // payloads — for 16×4MB blobs that's tens of seconds of
        // hashing for ~zero compression benefit. GC's
        // `pack_objects_impl` keeps the full delta search; this
        // path only optimizes durability + write throughput.
        let mut compression = self.compression;
        compression.max_delta_size = 0;
        let mut staged: Vec<(ContentHash, Vec<u8>)> = Vec::with_capacity(blobs.len());
        for (hash, data) in blobs {
            if ObjectStore::has_blob(self, &hash)? {
                continue;
            }
            staged.push((hash, data));
        }
        if staged.is_empty() {
            return Ok(());
        }

        let (mut builder, pack_path, index_path) = self.begin_streaming_pack(compression)?;
        for (hash, data) in &staged {
            builder.add(*hash, PackObjectType::Blob, data.clone())?;
        }
        self.finalize_streaming_pack(builder, &pack_path, &index_path)?;

        // Install the pack files directly: this local snapshot pack
        // cannot shadow unrelated loose objects. Keep `recent_blobs`
        // warm with the staged contents so tight snapshot loops don't
        // immediately miss on every blob they just wrote.
        let install_result = self.install_pack_files_streaming(&pack_path, &index_path);
        if install_result.is_err() {
            remove_staged_pack_files(&pack_path, &index_path);
        }
        install_result?;
        if let Ok(mut cache) = self.recent_blobs.write() {
            for (hash, data) in staged {
                cache.insert(hash, crate::object::Blob::from_slice(&data));
            }
        }
        Ok(())
    }

    /// Consolidate the object store into a single pack.
    ///
    /// GC must *shrink* the set of places a reader has to look, not grow
    /// it. The naive "pack the loose objects into a fresh pack" strategy
    /// regressed read performance badly: every `maintenance gc` minted a
    /// brand-new pack *alongside* the existing pack(s) and (by default)
    /// left the now-redundant loose copies in place. The result was an
    /// object store with strictly MORE sources to search — loose objects
    /// plus an ever-growing fleet of packs — and `PackManager::get_object`
    /// probes every pack linearly, so each extra pack roughly doubled the
    /// cost of the object lookups that `status`/`diff`/verification do.
    ///
    /// This implementation does a true repack: it folds every object
    /// already living in a pack *together with* the loose blobs and trees
    /// into one new consolidated pack, installs it, and then deletes the
    /// superseded packs. Combined with the caller's
    /// `prune_loose_objects`, the store ends a GC with exactly one pack
    /// and no loose duplicates — strictly fewer read sources than it
    /// started with. Running GC again over an already-consolidated store
    /// is a no-op (nothing loose, one pack already covers everything).
    ///
    /// State objects are addressed by `ChangeId` and may have a stale
    /// packed body shadowed by a fresher loose copy (#570). We re-pack
    /// the packed state body verbatim; the loose copy (which `prune`
    /// never touches) keeps shadowing it on read, so the shadow semantics
    /// are preserved across the repack.
    pub(super) fn pack_objects_impl(&self, aggressive: bool) -> Result<(u64, u64)> {
        let loose_blobs = list_hashes_from_dir(&blobs_dir(&self.root))?;
        let loose_trees = list_hashes_from_dir(&trees_dir(&self.root))?;

        // Snapshot what the existing packs already hold, plus the file
        // paths we'll retire once the consolidated pack is installed.
        let (existing_ids, old_pack_files) = {
            let manager = self.pack_manager().read().map_err(|_| {
                HeddleError::Config("Failed to acquire pack manager lock".to_string())
            })?;
            let ids = manager.list_all_ids()?;
            let files: Vec<(std::path::PathBuf, std::path::PathBuf)> = manager
                .pack_file_paths()
                .into_iter()
                .map(|(pack, index)| (pack.to_path_buf(), index.to_path_buf()))
                .collect();
            (ids, files)
        };

        // Nothing loose and at most one pack already — the store is
        // already consolidated; don't churn a fresh identical pack.
        if loose_blobs.is_empty() && loose_trees.is_empty() && old_pack_files.len() <= 1 {
            return Ok((0, 0));
        }

        // Consolidation packs every object that's already packed plus the
        // loose ones. The default path SKIPS the sliding-window delta
        // search: it runs across the full payloads of every object and on
        // a large native store (tens of MB across thousands of objects)
        // costs minutes for a near-zero size win, because the carried-
        // forward objects are already zstd-compressed. `--aggressive`
        // opts back into the full delta search for the rare "shrink the
        // pack at all costs" case. This mirrors the snapshot hot path
        // (`put_blobs_packed_impl`), which disables delta for the same
        // reason.
        if aggressive {
            return self.pack_objects_aggressive(
                loose_blobs,
                loose_trees,
                existing_ids,
                old_pack_files,
            );
        }

        self.pack_objects_streaming(loose_blobs, loose_trees, existing_ids, old_pack_files)
    }

    fn pack_objects_aggressive(
        &self,
        loose_blobs: Vec<ContentHash>,
        loose_trees: Vec<ContentHash>,
        existing_ids: Vec<PackObjectId>,
        old_pack_files: Vec<(PathBuf, PathBuf)>,
    ) -> Result<(u64, u64)> {
        let compression = self.compression;
        let mut builder = PackBuilder::new(compression);
        let mut seen: std::collections::HashSet<PackObjectId> = std::collections::HashSet::new();

        // 1. Carry forward everything already in a pack so the old packs
        //    can be retired. `get_object` resolves the body + type for
        //    any id (blob/tree/state/action), and `add_id` preserves
        //    ChangeId-keyed state objects.
        for id in existing_ids {
            if !seen.insert(id) {
                continue;
            }
            let obj_type = {
                let manager = self.pack_manager().read().map_err(|_| {
                    HeddleError::Config("Failed to acquire pack manager lock".to_string())
                })?;
                manager.get_object(&id)?
            };
            if let Some((obj_type, data)) = obj_type {
                builder.add_id(id, obj_type, data);
            }
        }

        // 2. Fold in the loose blobs and trees. Skip any whose hash is
        //    already covered by a carried-forward pack entry.
        for hash in &loose_blobs {
            let id = PackObjectId::Hash(*hash);
            if seen.contains(&id) {
                continue;
            }
            if let Some(blob) = ObjectStore::get_blob(self, hash)? {
                seen.insert(id);
                builder.add(*hash, PackObjectType::Blob, blob.content().to_vec());
            }
        }
        for hash in &loose_trees {
            let id = PackObjectId::Hash(*hash);
            if seen.contains(&id) {
                continue;
            }
            if let Some(tree) = ObjectStore::get_tree(self, hash)? {
                let data = rmp_serde::to_vec(&tree)?;
                seen.insert(id);
                builder.add(*hash, PackObjectType::Tree, data);
            }
        }

        if seen.is_empty() {
            return Ok((0, 0));
        }

        let (pack_data, index_data, stats) = builder.build()?;
        self.install_pack_files(&pack_data, &index_data)?;
        // GC packs *replace* loose objects (followed by
        // `prune_loose_objects`). Bust the recent-objects caches so
        // a subsequent get_* doesn't return a stale `Blob`/`Tree`
        // pointing at a path we're about to delete. The snapshot hot
        // path doesn't go through here — it installs the streaming
        // pack directly via `put_blobs_packed_impl`, which keeps its
        // caches warm.
        self.clear_recent_object_caches();

        // Retire the superseded packs now that the consolidated pack is
        // durably installed and every object they held has been carried
        // forward. The consolidated pack is content-addressed, so if it
        // happened to hash-collide with an old pack (a store that was
        // already a single consolidated pack) that file is excluded here.
        let new_pack_name = blake3::hash(&pack_data).to_hex().to_string();
        self.retire_superseded_packs(&old_pack_files, &new_pack_name)?;

        let saved = stats
            .total_uncompressed
            .saturating_sub(stats.total_compressed);
        Ok((stats.object_count, saved))
    }

    fn pack_objects_streaming(
        &self,
        loose_blobs: Vec<ContentHash>,
        loose_trees: Vec<ContentHash>,
        existing_ids: Vec<PackObjectId>,
        old_pack_files: Vec<(PathBuf, PathBuf)>,
    ) -> Result<(u64, u64)> {
        let mut compression = self.compression;
        compression.max_delta_size = 0;
        let (mut builder, pack_path, index_path) = self.begin_streaming_pack(compression)?;
        let mut seen: std::collections::HashSet<PackObjectId> = std::collections::HashSet::new();

        // Carry forward everything already in a pack so the old packs
        // can be retired. `add_id` preserves ChangeId-keyed states.
        for id in existing_ids {
            if !seen.insert(id) {
                continue;
            }
            let obj_type = {
                let manager = self.pack_manager().read().map_err(|_| {
                    HeddleError::Config("Failed to acquire pack manager lock".to_string())
                })?;
                manager.get_object(&id)?
            };
            if let Some((obj_type, data)) = obj_type {
                builder.add_id(id, obj_type, data)?;
            }
        }

        // Fold in the loose blobs and trees. Skip any whose hash is
        // already covered by a carried-forward pack entry.
        for hash in &loose_blobs {
            let id = PackObjectId::Hash(*hash);
            if seen.contains(&id) {
                continue;
            }
            if let Some(blob) = ObjectStore::get_blob(self, hash)? {
                seen.insert(id);
                builder.add(*hash, PackObjectType::Blob, blob.content().to_vec())?;
            }
        }
        for hash in &loose_trees {
            let id = PackObjectId::Hash(*hash);
            if seen.contains(&id) {
                continue;
            }
            if let Some(tree) = ObjectStore::get_tree(self, hash)? {
                let data = rmp_serde::to_vec(&tree)?;
                seen.insert(id);
                builder.add(*hash, PackObjectType::Tree, data)?;
            }
        }

        if seen.is_empty() {
            drop(builder);
            remove_staged_pack_files(&pack_path, &index_path);
            return Ok((0, 0));
        }

        let install_result: Result<(PackStats, String)> = (|| {
            let stats = self.finalize_streaming_pack(builder, &pack_path, &index_path)?;
            let new_pack_name = stream_hash_file(&pack_path)?.to_hex().to_string();
            self.install_pack_files_streaming(&pack_path, &index_path)?;
            Ok((stats, new_pack_name))
        })();
        if install_result.is_err() {
            remove_staged_pack_files(&pack_path, &index_path);
        }
        let (stats, new_pack_name) = install_result?;

        // GC packs *replace* loose objects (followed by
        // `prune_loose_objects`). Bust the recent-objects caches so
        // subsequent get_* calls don't return values pointing at paths
        // we're about to delete.
        self.clear_recent_object_caches();
        self.retire_superseded_packs(&old_pack_files, &new_pack_name)?;

        let saved = stats
            .total_uncompressed
            .saturating_sub(stats.total_compressed);
        Ok((stats.object_count, saved))
    }

    fn begin_streaming_pack(
        &self,
        compression: crate::store::compression::CompressionConfig,
    ) -> Result<(StreamingPackBuilder<File>, PathBuf, PathBuf)> {
        let packs = packs_dir(&self.root);
        fs::create_dir_all(&packs)?;

        let pack_path = crate::store::atomic::temp_path(&packs.join("streaming.pack"));
        let index_path = crate::store::atomic::temp_path(&packs.join("streaming.idx"));
        let bucket_dir = crate::store::atomic::temp_path(&packs.join("streaming-buckets"));
        let pack_file = open_streaming_pack_file(&pack_path)?;

        match StreamingPackBuilder::new(
            pack_file,
            index_path.clone(),
            compression,
            bucket_dir.clone(),
        ) {
            Ok(builder) => Ok((builder, pack_path, index_path)),
            Err(error) => {
                let _ = fs::remove_file(&pack_path);
                let _ = fs::remove_dir(&bucket_dir);
                Err(error)
            }
        }
    }

    fn finalize_streaming_pack(
        &self,
        builder: StreamingPackBuilder<File>,
        pack_path: &Path,
        index_path: &Path,
    ) -> Result<PackStats> {
        let result = builder.finalize().and_then(|(pack_file, stats)| {
            pack_file.sync_all()?;
            drop(pack_file);
            Ok(stats)
        });
        if result.is_err() {
            remove_staged_pack_files(pack_path, index_path);
        }
        result
    }

    fn retire_superseded_packs(
        &self,
        old_pack_files: &[(PathBuf, PathBuf)],
        new_pack_name: &str,
    ) -> Result<()> {
        for (pack_path, index_path) in old_pack_files {
            let is_new_pack = pack_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(|stem| stem == new_pack_name)
                .unwrap_or(false);
            if is_new_pack {
                continue;
            }
            remove_file_ignore_missing(pack_path)?;
            remove_file_ignore_missing(index_path)?;
        }
        // Disk shrank (packs removed), so `reload_if_disk_grew` would
        // not pick this up — force a full reload of the pack list.
        self.reload_packs()?;
        self.clear_recent_object_caches();
        Ok(())
    }

    pub(super) fn install_pack_files(&self, pack_data: &[u8], index_data: &[u8]) -> Result<()> {
        let packs = packs_dir(&self.root);
        fs::create_dir_all(&packs)?;

        let pack_hash = blake3::hash(pack_data);
        let pack_name = format!("{}", pack_hash.to_hex());
        let pack_path = packs.join(format!("{}.pack", pack_name));
        let index_path = packs.join(format!("{}.idx", pack_name));

        self.write_pack_atomic(&pack_path, pack_data)?;
        self.write_pack_atomic(&index_path, index_data)?;
        // Pack manager picks up the new files. We do *not* clear the
        // recent-object caches here — every caller that follows this
        // with a destructive prune is responsible for clearing them
        // explicitly. Snapshot installs rely on cache stickiness to
        // keep tight snapshot loops fast (see
        // `put_blobs_packed_impl`).
        self.reload_packs()?;
        Ok(())
    }

    /// Move a pack and its index already on disk into the store's
    /// pack directory, computing the pack's content-hash by streaming
    /// the file (constant memory regardless of pack size). Pairs with
    /// `StreamingPackBuilder`: pack data, the index, *and* this
    /// installation step never load the full pack or index into
    /// memory.
    ///
    /// Both source files are fsynced and `rename(2)`'d into place;
    /// the index is no longer copied through memory the way
    /// `install_pack_files` did via `write_pack_atomic`.
    /// Cross-device renames fall back to copy + fsync + remove for
    /// the rare EXDEV case.
    pub(super) fn install_pack_files_streaming(
        &self,
        src_pack_path: &std::path::Path,
        src_index_path: &std::path::Path,
    ) -> Result<()> {
        let packs = packs_dir(&self.root);
        fs::create_dir_all(&packs)?;

        // Stream-hash the pack file to derive its name. 64 KiB chunks
        // keep the hasher's working set tiny.
        let pack_hash = stream_hash_file(src_pack_path)?;
        let pack_name = format!("{}", pack_hash.to_hex());
        let pack_path = packs.join(format!("{}.pack", pack_name));
        let index_path = packs.join(format!("{}.idx", pack_name));

        durable_install_existing_file(src_pack_path, &pack_path, &packs)?;

        // Move the index file alongside the pack. Same rename semantics
        // as the pack: atomic on same-filesystem POSIX, copy+remove
        // fallback for cross-device.
        durable_install_existing_file(src_index_path, &index_path, &packs)?;
        self.reload_packs()?;
        Ok(())
    }

    pub(super) fn prune_loose_objects_impl(&self) -> Result<(u64, u64)> {
        let mut removed = 0u64;
        let mut bytes_freed = 0u64;

        let blobs = list_hashes_from_dir(&blobs_dir(&self.root))?;
        let trees = list_hashes_from_dir(&trees_dir(&self.root))?;

        let pack_manager = self
            .pack_manager()
            .read()
            .map_err(|_| HeddleError::Config("Failed to acquire pack manager lock".to_string()))?;

        for hash in &blobs {
            if pack_manager.get_hashed_object(hash)?.is_some() {
                let path = hash_path(&blobs_dir(&self.root), hash);
                match fs::metadata(&path) {
                    Ok(metadata) => match fs::remove_file(&path) {
                        Ok(()) => {
                            bytes_freed += metadata.len();
                            removed += 1;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(HeddleError::from(e)),
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(HeddleError::from(e)),
                }
            }
        }

        for hash in &trees {
            if pack_manager.get_hashed_object(hash)?.is_some() {
                let path = hash_path(&trees_dir(&self.root), hash);
                match fs::metadata(&path) {
                    Ok(metadata) => match fs::remove_file(&path) {
                        Ok(()) => {
                            bytes_freed += metadata.len();
                            removed += 1;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(HeddleError::from(e)),
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(HeddleError::from(e)),
                }
            }
        }

        Ok((removed, bytes_freed))
    }
}

fn open_streaming_pack_file(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o644);
    }
    Ok(opts.open(path)?)
}

fn stream_hash_file(path: &Path) -> Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

fn durable_install_existing_file(src: &Path, dst: &Path, parent: &Path) -> Result<()> {
    File::open(src)?.sync_all()?;
    match fs::rename(src, dst) {
        Ok(()) => {
            crate::fs_atomic::sync_directory(parent)?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(src);
            Ok(())
        }
        Err(e) if crate::fs_atomic::is_cross_device_link(&e) => {
            fs::copy(src, dst)?;
            File::open(dst)?.sync_all()?;
            let _ = fs::remove_file(src);
            crate::fs_atomic::sync_directory(parent)?;
            Ok(())
        }
        Err(e) => Err(HeddleError::from(e)),
    }
}

fn remove_staged_pack_files(pack_path: &Path, index_path: &Path) {
    let _ = fs::remove_file(pack_path);
    let _ = fs::remove_file(index_path);
}
