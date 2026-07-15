// SPDX-License-Identifier: Apache-2.0
//! Pack and prune operations for FsStore.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{
    FsStore,
    fs_io::{list_hashes_from_dir, read_file_bytes},
    fs_paths::{blobs_dir, hash_path, packs_dir, trees_dir},
};
use crate::{
    object::ContentHash,
    store::{
        HeddleError, ObjectStore, Result, codec,
        pack::{ObjectType as PackObjectType, PackBuilder},
    },
};

/// Paths of `*.pack` files in `packs_dir` that have no matching `*.idx`.
///
/// L8 residual: crash between durable pack and index publish can leave an
/// unpaired pack that [`FsStore::reload_packs`] ignores. Listing supports
/// optional GC (design: `docs/program/L8_PACK_INSTALL_JOURNAL.md` Option D).
/// Does not delete anything.
pub(crate) fn list_unpaired_pack_files(packs_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !packs_dir.exists() {
        return Ok(Vec::new());
    }
    let mut unpaired = Vec::new();
    for entry in fs::read_dir(packs_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pack") {
            continue;
        }
        let idx = path.with_extension("idx");
        if !idx.exists() {
            unpaired.push(path);
        }
    }
    unpaired.sort();
    Ok(unpaired)
}

/// Remove unpaired `*.pack` files (no matching `*.idx`) under `packs_dir`.
///
/// Safe for correctness: loaders never open unpaired packs. Bounds L8 disk
/// leak. Returns `(removed_count, bytes_freed)`.
pub(crate) fn prune_unpaired_pack_files(packs_dir: &Path) -> std::io::Result<(u64, u64)> {
    let mut removed = 0u64;
    let mut bytes_freed = 0u64;
    for path in list_unpaired_pack_files(packs_dir)? {
        let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        match fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
                bytes_freed = bytes_freed.saturating_add(bytes);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok((removed, bytes_freed))
}

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
        let mut builder = PackBuilder::new(compression);
        let mut staged: Vec<(ContentHash, Vec<u8>)> = Vec::with_capacity(blobs.len());
        for (hash, data) in blobs {
            if ObjectStore::has_blob(self, &hash)? {
                continue;
            }
            staged.push((hash, data.clone()));
            builder.add(hash, PackObjectType::Blob, data);
        }
        if staged.is_empty() {
            return Ok(());
        }
        let (pack_data, index_data, _stats) = builder.build()?;

        // Install the pack files. `install_pack_files` clears the
        // recent-objects caches because a generic pack install (e.g.
        // received over the network) might shadow loose objects we
        // didn't write. For our locally-built pack we know exactly
        // what we just installed, so we re-populate `recent_blobs`
        // with the staged contents immediately afterwards. Without
        // this the snapshot hot path takes a cache miss on every
        // blob it just wrote, and `seed_large_repository` style
        // benchmarks that snapshot-many-times-in-a-loop end up
        // re-reading every parent state from disk between
        // iterations.
        self.install_pack_files(&pack_data, &index_data)?;
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
        let mut compression = self.compression;
        if !aggressive {
            compression.max_delta_size = 0;
        }
        let mut builder = PackBuilder::new(compression);
        let loose_tree_set: std::collections::HashSet<ContentHash> =
            loose_trees.iter().copied().collect();
        let mut seen: std::collections::HashSet<crate::store::pack::PackObjectId> =
            std::collections::HashSet::new();

        // 1. Carry forward everything already in a pack so the old packs
        //    can be retired. `get_object` resolves the body + type for
        //    any id (blob/tree/state/action), and `add_id` preserves
        //    content-addressed state objects.
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
            if let Some((obj_type, mut data)) = obj_type {
                if let crate::store::pack::PackObjectId::Hash(hash) = id
                    && obj_type == PackObjectType::Tree
                    && loose_tree_set.contains(&hash)
                    && let Some(loose_data) = ObjectStore::get_tree_serialized(self, &hash)?
                {
                    data = loose_data;
                }
                builder.add_id(id, obj_type, data);
            }
        }

        // 2. Fold in the loose blobs and trees. Skip any whose hash is
        //    already covered by a carried-forward pack entry.
        for hash in &loose_blobs {
            let id = crate::store::pack::PackObjectId::Hash(*hash);
            if seen.contains(&id) {
                continue;
            }
            if let Some(blob) = ObjectStore::get_blob(self, hash)? {
                seen.insert(id);
                builder.add(*hash, PackObjectType::Blob, blob.content().to_vec());
            }
        }
        for hash in &loose_trees {
            let id = crate::store::pack::PackObjectId::Hash(*hash);
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
        // path doesn't go through here — it calls
        // `install_pack_files` directly via `put_blobs_packed_impl`,
        // which keeps its caches warm.
        self.clear_recent_object_caches();

        // Retire the superseded packs now that the consolidated pack is
        // durably installed and every object they held has been carried
        // forward. The consolidated pack is content-addressed, so if it
        // happened to hash-collide with an old pack (a store that was
        // already a single consolidated pack) that file is excluded here.
        // Stack hex digest; compare as &str — no format!/String intermediate.
        let new_pack_name = blake3::hash(&pack_data).to_hex();
        for (pack_path, index_path) in &old_pack_files {
            let is_new_pack = pack_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(|stem| stem == new_pack_name.as_str())
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

        let saved = stats.total_uncompressed - stats.total_compressed;
        Ok((stats.object_count, saved))
    }

    pub(super) fn install_pack_files(&self, pack_data: &[u8], index_data: &[u8]) -> Result<()> {
        let packs = packs_dir(&self.root);
        // L8 A+: durable staging + intent journal for in-memory pack install
        // (same crash-safety as install_pack_files_streaming).
        // Design: docs/program/L8_PACK_INSTALL_JOURNAL.md
        let _pack_name = super::pack_install_journal::install_pack_bytes_journaled(
            &packs, pack_data, index_data,
        )?;
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
    /// Sources are staged then published via the L8 A+ install journal
    /// ([`super::pack_install_journal`]): durable staging + intent, then
    /// pack/index publish with crash recovery on reload.
    pub(super) fn install_pack_files_streaming(
        &self,
        src_pack_path: &std::path::Path,
        src_index_path: &std::path::Path,
    ) -> Result<()> {
        use std::io::Read;

        let packs = packs_dir(&self.root);
        crate::fs_atomic::create_dir_all_durable(&packs)?;

        // Stream-hash the pack file to derive its name. 64 KiB chunks
        // keep the hasher's working set tiny.
        let mut hasher = blake3::Hasher::new();
        let mut file = fs::File::open(src_pack_path)?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        drop(file);
        // Native digest for potential callers; hex String only for the journal
        // path/name boundary (filenames + intent JSON).
        let pack_hash = hasher.finalize();
        let pack_name = pack_hash.to_hex().to_string();

        // L8 A+: durable staging + intent journal, then pack/index publish.
        // Recovery on reload finishes or aborts incomplete installs.
        // Design: docs/program/L8_PACK_INSTALL_JOURNAL.md
        super::pack_install_journal::install_pack_files_journaled(
            &packs,
            src_pack_path,
            src_index_path,
            &pack_name,
        )?;

        self.clear_recent_object_caches();
        self.reload_packs()?;
        Ok(())
    }

    /// Remove L8 orphan packs (`.pack` without `.idx`) from this store.
    pub fn prune_unpaired_packs(&self) -> Result<(u64, u64)> {
        let packs = packs_dir(&self.root);
        Ok(prune_unpaired_pack_files(&packs)?)
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
            let Some((obj_type, packed_data)) = pack_manager.get_hashed_object(hash)? else {
                continue;
            };
            if obj_type != PackObjectType::Tree {
                continue;
            }
            let path = hash_path(&trees_dir(&self.root), hash);
            let Some(loose_data) = read_file_bytes(&path)? else {
                continue;
            };
            let loose_data = codec::decode_tree_body(loose_data.as_slice())?;
            if packed_data == loose_data {
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

#[cfg(test)]
mod unpaired_pack_tests {
    use std::fs;

    use super::{list_unpaired_pack_files, prune_unpaired_pack_files};

    #[test]
    fn list_and_prune_unpaired_packs() {
        let dir = tempfile::tempdir().unwrap();
        let packs = dir.path();
        fs::write(packs.join("aaa.pack"), b"pack-only").unwrap();
        fs::write(packs.join("bbb.pack"), b"paired-pack").unwrap();
        fs::write(packs.join("bbb.idx"), b"paired-idx").unwrap();
        fs::write(packs.join("ccc.idx"), b"index-only").unwrap();

        let listed = list_unpaired_pack_files(packs).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].ends_with("aaa.pack"));

        let (removed, bytes) = prune_unpaired_pack_files(packs).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(bytes, b"pack-only".len() as u64);
        assert!(!packs.join("aaa.pack").exists());
        assert!(packs.join("bbb.pack").exists());
        assert!(packs.join("bbb.idx").exists());
        assert!(packs.join("ccc.idx").exists());
        assert!(list_unpaired_pack_files(packs).unwrap().is_empty());
    }

    #[test]
    fn missing_packs_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(list_unpaired_pack_files(&missing).unwrap().is_empty());
        assert_eq!(prune_unpaired_pack_files(&missing).unwrap(), (0, 0));
    }
}
