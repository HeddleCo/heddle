// SPDX-License-Identifier: Apache-2.0
//! Pack and prune operations for FsStore.

use std::fs;

use super::{
    FsStore,
    fs_io::list_hashes_from_dir,
    fs_paths::{blobs_dir, hash_path, packs_dir, trees_dir},
};
use crate::{
    object::ContentHash,
    store::{
        HeddleError, ObjectStore, Result,
        pack::{ObjectType as PackObjectType, PackBuilder},
    },
};

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

    pub(super) fn pack_objects_impl(&self, _aggressive: bool) -> Result<(u64, u64)> {
        let blobs = list_hashes_from_dir(&blobs_dir(&self.root))?;
        let trees = list_hashes_from_dir(&trees_dir(&self.root))?;

        if blobs.is_empty() && trees.is_empty() {
            return Ok((0, 0));
        }

        let mut builder = PackBuilder::new(self.compression);

        for hash in &blobs {
            if let Some(blob) = ObjectStore::get_blob(self, hash)? {
                builder.add(*hash, PackObjectType::Blob, blob.content().to_vec());
            }
        }

        for hash in &trees {
            if let Some(tree) = ObjectStore::get_tree(self, hash)? {
                let data = rmp_serde::to_vec(&tree)?;
                builder.add(*hash, PackObjectType::Tree, data);
            }
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

        let saved = stats.total_uncompressed - stats.total_compressed;
        Ok((stats.object_count, saved))
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
    /// Both source files are `rename(2)`'d into place; the index is
    /// no longer copied through memory the way `install_pack_files`
    /// did via `write_pack_atomic`. Cross-device renames fall back to
    /// copy + remove for the rare EXDEV case.
    pub(super) fn install_pack_files_streaming(
        &self,
        src_pack_path: &std::path::Path,
        src_index_path: &std::path::Path,
    ) -> Result<()> {
        use std::io::Read;

        let packs = packs_dir(&self.root);
        fs::create_dir_all(&packs)?;

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
        let pack_hash = hasher.finalize();
        let pack_name = format!("{}", pack_hash.to_hex());
        let pack_path = packs.join(format!("{}.pack", pack_name));
        let index_path = packs.join(format!("{}.idx", pack_name));

        // Move the staged pack file into the store. `rename` is
        // atomic on POSIX when both paths are on the same filesystem;
        // the heddle store keeps its staging dir under the same root,
        // so this should always satisfy that constraint.
        match fs::rename(src_pack_path, &pack_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Same pack already installed (content-addressed).
                // Drop the staging copy so it doesn't accumulate.
                let _ = fs::remove_file(src_pack_path);
            }
            Err(e) => {
                // Fall back to copy + remove for the (rare) case of
                // a cross-device rename. EXDEV is not on a stable
                // ErrorKind variant; match by raw_os_error if needed
                // on Linux.
                let _ = fs::copy(src_pack_path, &pack_path)?;
                let _ = fs::remove_file(src_pack_path);
                let _ = e; // silence unused-var if EXDEV path didn't fire
            }
        }

        // Move the index file alongside the pack. Same rename
        // semantics as the pack: atomic on same-filesystem POSIX,
        // copy+remove fallback for cross-device.
        match fs::rename(src_index_path, &index_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(src_index_path);
            }
            Err(_) => {
                let _ = fs::copy(src_index_path, &index_path)?;
                let _ = fs::remove_file(src_index_path);
            }
        }
        self.clear_recent_object_caches();
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
