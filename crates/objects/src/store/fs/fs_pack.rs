// SPDX-License-Identifier: Apache-2.0
//! Pack and prune operations for FsStore.

use std::{
    fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};

use super::{
    FsStore,
    fs_impl::validate_state_serialized,
    fs_io::{list_hashes_from_dir, list_state_ids_from_dir, read_file_bytes},
    fs_paths::{blobs_dir, hash_path, packs_dir, state_path, states_dir, trees_dir},
};
use crate::{
    object::{ContentHash, State, StateAttachment, StateAttachmentId},
    store::{
        FsRepackOperation, HeddleError, ObjectStore, RepackPolicy, RepackResourceLimits,
        RepackSchedule, RepackScheduler, Result, SnapshotCommitArtifact, SnapshotCommitDescriptor,
        TreeWrite, codec,
        pack::{ObjectType as PackObjectType, PackBuilder, PackObjectId},
        snapshot_commit::snapshot_commit_marker_path,
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

fn remove_file_counted(path: &Path) -> Result<Option<u64>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(HeddleError::from(error)),
    };
    match fs::remove_file(path) {
        Ok(()) => Ok(Some(metadata.len())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(HeddleError::from(error)),
    }
}

impl FsStore {
    /// Rewrite all packs without `hash`, remove its loose copy, and verify that
    /// neither local nor external object lookup can still serve the bytes.
    pub fn remove_blob_everywhere(&self, hash: &ContentHash) -> Result<bool> {
        let was_present = ObjectStore::has_blob_locally(self, hash)?;
        if was_present {
            let scheduler = RepackScheduler::new(
                RepackPolicy::default(),
                RepackResourceLimits::new(NonZeroUsize::MIN),
            );
            let operation = Arc::new(FsRepackOperation::new(self.clone()).excluding_blob(*hash));
            let RepackSchedule::Started(handle) = scheduler
                .repack_now(operation)
                .map_err(|error| HeddleError::InvalidObject(error.to_string()))?
            else {
                return Err(HeddleError::InvalidObject(
                    "exclusive purge repack did not start".to_string(),
                ));
            };
            handle
                .wait()
                .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;

            // The repack operation owns a clone of this store, so its atomic
            // cutover updates that clone's in-memory pack manager. Reload the
            // caller's manager from the newly published generation before
            // checking whether the purged object is still reachable.
            self.reload_packs()?;

            let loose = hash_path(&blobs_dir(&self.root), hash);
            match fs::remove_file(&loose) {
                Ok(()) => {
                    if let Some(parent) = loose.parent() {
                        crate::fs_atomic::sync_directory(parent)?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.clear_recent_object_caches();
        if ObjectStore::has_blob_locally(self, hash)?
            || ObjectStore::get_blob(self, hash)?.is_some()
            || ObjectStore::get_blob_bytes(self, hash)?.is_some()
        {
            return Err(HeddleError::InvalidObject(format!(
                "purged blob {} remains readable after pack rewrite",
                hash.short()
            )));
        }
        Ok(was_present)
    }

    /// Install a structured snapshot closure and its commit artifact through
    /// the filesystem store's single durable pack barrier.
    #[doc(hidden)]
    pub fn put_committed_snapshot_objects_packed(
        &self,
        blobs: Vec<(ContentHash, Vec<u8>)>,
        trees: Vec<TreeWrite>,
        tree: &TreeWrite,
        state: &State,
        attachments: Vec<StateAttachment>,
        artifact: SnapshotCommitArtifact,
    ) -> Result<SnapshotCommitDescriptor> {
        self.put_snapshot_objects_packed_impl(
            blobs,
            trees,
            tree,
            state,
            attachments,
            Some(artifact),
        )?
        .ok_or_else(|| {
            HeddleError::InvalidObject(
                "committed snapshot pack did not expose its artifact descriptor".to_string(),
            )
        })
    }

    /// Install blobs, root tree, state, and immutable authored attachments
    /// through one pack publication. Ordinary callers treat the pack as
    /// pre-oplog staging; committed structured snapshots add their local trust
    /// marker in the same directory barrier and make the pack authoritative.
    pub(super) fn put_snapshot_objects_packed_impl(
        &self,
        blobs: Vec<(ContentHash, Vec<u8>)>,
        trees: Vec<TreeWrite>,
        tree: &TreeWrite,
        state: &State,
        attachments: Vec<StateAttachment>,
        commit_artifact: Option<SnapshotCommitArtifact>,
    ) -> Result<Option<SnapshotCommitDescriptor>> {
        // A committed snapshot artifact is installed only after exact-once and
        // isolation validation. Its freshly-authored StateId cannot be a retry
        // (dedup returns before this callback), so avoid an expected-negative
        // pack-directory rescan on every native capture.
        let state_was_present = if commit_artifact.is_some() {
            false
        } else {
            <Self as ObjectStore>::has_state(self, &state.id())?
        };
        let mut compression = self.compression;
        if !self.snapshot_delta_search {
            compression.max_delta_size = 0;
        }
        let mut builder = PackBuilder::new(compression);

        for (hash, data) in blobs {
            if commit_artifact.is_none() && ObjectStore::has_blob_locally(self, &hash)? {
                continue;
            }
            builder.add(hash, PackObjectType::Blob, data);
        }

        let tree_hash = tree.tree.hash();
        let mut staged_trees = Vec::with_capacity(trees.len() + 1);
        let mut staged_encodings = Vec::with_capacity(trees.len() + 1);
        let mut seen_trees = std::collections::HashSet::with_capacity(trees.len() + 1);
        for authored_tree in trees {
            let authored_hash = authored_tree.tree.hash();
            let reuses_materialized = self
                .try_get_tree_serialized_once(&authored_hash)?
                .is_some_and(|body| !crate::object::is_delta_tree(&body));
            if seen_trees.insert(authored_hash)
                && !reuses_materialized
                && (commit_artifact.is_some()
                    || !ObjectStore::has_tree_locally(self, &authored_hash)?)
            {
                let encoded = self.encode_tree_write(&authored_tree)?;
                if matches!(
                    encoded.kind,
                    crate::store::codec::TreeEncodingKind::Delta { anchor, .. }
                        if anchor == authored_hash
                ) {
                    return Err(HeddleError::InvalidObject(
                        "HDC1 result id must differ from its anchor id".to_string(),
                    ));
                }
                builder.add(authored_hash, PackObjectType::Tree, encoded.data);
                staged_encodings.push((authored_hash, encoded.kind));
                staged_trees.push((authored_hash, authored_tree.tree));
            }
        }
        let reuses_materialized = self
            .try_get_tree_serialized_once(&tree_hash)?
            .is_some_and(|body| !crate::object::is_delta_tree(&body));
        if !reuses_materialized
            && (commit_artifact.is_some() || !ObjectStore::has_tree_locally(self, &tree_hash)?)
            && seen_trees.insert(tree_hash)
        {
            let encoded = self.encode_tree_write(tree)?;
            if matches!(
                encoded.kind,
                crate::store::codec::TreeEncodingKind::Delta { anchor, .. }
                    if anchor == tree_hash
            ) {
                return Err(HeddleError::InvalidObject(
                    "HDC1 result id must differ from its anchor id".to_string(),
                ));
            }
            builder.add(tree_hash, PackObjectType::Tree, encoded.data);
            staged_encodings.push((tree_hash, encoded.kind));
            staged_trees.push((tree_hash, tree.tree.clone()));
        }

        let state_id = state.id();
        builder.add_id(
            PackObjectId::StateId(state_id),
            PackObjectType::State,
            rmp_serde::to_vec_named(state)?,
        );
        let attachment_ids = attachments
            .iter()
            .map(|attachment| {
                if attachment.state_id != state_id {
                    return Err(HeddleError::InvalidObject(
                        "snapshot attachment targets a different state".to_string(),
                    ));
                }
                let id = attachment.id();
                builder.add(
                    *id.as_hash(),
                    PackObjectType::StateAttachment,
                    rmp_serde::to_vec_named(attachment)?,
                );
                Ok(id)
            })
            .collect::<Result<Vec<StateAttachmentId>>>()?;
        let artifact_id = commit_artifact.as_ref().map(SnapshotCommitArtifact::id);
        let artifact_bytes = commit_artifact
            .as_ref()
            .map(rmp_serde::to_vec_named)
            .transpose()?;
        if let Some(artifact) = &commit_artifact {
            artifact.validate()?;
            let bytes = artifact_bytes.as_ref().ok_or_else(|| {
                HeddleError::InvalidObject(
                    "snapshot commit artifact bytes were not encoded".to_string(),
                )
            })?;
            builder.add(artifact.id(), PackObjectType::SnapshotCommit, bytes.clone());
        }

        let (pack_data, index_data, _stats, retained_objects) =
            builder.build_retaining_objects()?;
        let object_ids = commit_artifact.as_ref().map(|_| {
            let mut ids = retained_objects
                .iter()
                .map(|(id, _, _)| *id)
                .collect::<Vec<_>>();
            ids.sort_unstable();
            ids
        });
        let packs = packs_dir(&self.root);
        let installed_pack_name = if commit_artifact.is_some() {
            let (Some(artifact_id), Some(artifact_bytes)) = (artifact_id, artifact_bytes) else {
                return Err(HeddleError::InvalidObject(
                    "snapshot commit artifact metadata is incomplete".to_string(),
                ));
            };
            super::pack_install_journal::install_committed_snapshot_pack_bytes(
                &packs,
                pack_data,
                index_data,
                artifact_id,
                artifact_bytes,
            )?
        } else {
            super::pack_install_journal::install_snapshot_pack_bytes(&packs, pack_data, index_data)?
        };
        {
            let mut manager = self.pack_manager().write().map_err(|_| {
                HeddleError::Config("Failed to acquire pack manager lock".to_string())
            })?;
            manager.add_pack(
                packs.join(format!("{installed_pack_name}.pack")),
                packs.join(format!("{installed_pack_name}.idx")),
            )?;
        }
        self.remember_pack_dir_modified()?;
        for (hash, kind) in staged_encodings {
            self.remember_tree_encoding(hash, kind)?;
        }
        self.materialize_packed_attachment_index(&state_id, &attachment_ids, state_was_present)?;

        if let Ok(mut cache) = self.recent_blobs.write() {
            for (id, object_type, data) in retained_objects {
                if let (PackObjectId::Hash(hash), PackObjectType::Blob) = (id, object_type) {
                    cache.insert(hash, crate::object::Blob::new(data));
                }
            }
        }
        if let Ok(mut cache) = self.recent_trees.write() {
            for (hash, authored_tree) in staged_trees {
                cache.insert(hash, authored_tree);
            }
        }
        if let Ok(mut cache) = self.recent_states.write() {
            let mut cached = state.clone();
            cached.state_id = state_id;
            cache.insert(state_id, cached);
        }
        let descriptor = if let Some(artifact) = commit_artifact {
            let pack_path = packs.join(format!("{installed_pack_name}.pack"));
            let object_ids = object_ids.ok_or_else(|| {
                HeddleError::InvalidObject(
                    "snapshot commit pack object ids were not retained".to_string(),
                )
            })?;
            Some(SnapshotCommitDescriptor {
                artifact,
                pack_name: installed_pack_name,
                pack_path,
                object_ids,
            })
        } else {
            None
        };
        Ok(descriptor)
    }

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
        if !self.snapshot_delta_search {
            compression.max_delta_size = 0;
        }
        let mut builder = PackBuilder::new(compression);
        let mut added = 0usize;
        for (hash, data) in blobs {
            if ObjectStore::has_blob_locally(self, &hash)? {
                continue;
            }
            builder.add(hash, PackObjectType::Blob, data);
            added += 1;
        }
        if added == 0 {
            return Ok(());
        }
        let (pack_data, index_data, _stats, retained_objects) =
            builder.build_retaining_objects()?;

        // A generic install clears recent-object caches because received packs
        // can shadow loose objects. This locally-built pack returns ownership
        // of its original inputs after encoding, so repopulating the cache does
        // not require a payload-sized staging or `Blob::from_slice` copy.
        self.install_pack_files(&pack_data, &index_data)?;
        if let Ok(mut cache) = self.recent_blobs.write() {
            for (id, object_type, data) in retained_objects {
                if let (PackObjectId::Hash(hash), PackObjectType::Blob) = (id, object_type) {
                    cache.insert(hash, crate::object::Blob::new(data));
                }
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
    pub(super) fn pack_objects_impl(&self, delta_search: bool) -> Result<(u64, u64)> {
        // Serialize every source-pack-retiring path with background repack,
        // including callers in another process. Ordinary immutable pack
        // installs remain concurrent and are preserved at scheduler cutover.
        let _repack_lock = super::repack::acquire_repack_lock_blocking(&packs_dir(&self.root))?;
        let loose_blobs = list_hashes_from_dir(&blobs_dir(&self.root))?;
        let loose_trees = list_hashes_from_dir(&trees_dir(&self.root))?;

        // Snapshot what the existing packs already hold, plus the file
        // paths we'll retire once the consolidated pack is installed.
        let (existing_ids, old_pack_files, commit_artifact_ids) = {
            let manager = self.pack_manager().read().map_err(|_| {
                HeddleError::Config("Failed to acquire pack manager lock".to_string())
            })?;
            let ids = manager.list_all_ids()?;
            let commit_artifact_ids = manager
                .snapshot_commit_descriptors()?
                .into_iter()
                .map(|descriptor| descriptor.artifact.id())
                .collect::<Vec<_>>();
            let files: Vec<(std::path::PathBuf, std::path::PathBuf)> = manager
                .pack_file_paths()
                .into_iter()
                .map(|(pack, index)| (pack.to_path_buf(), index.to_path_buf()))
                .collect();
            (ids, files, commit_artifact_ids)
        };

        // Nothing loose and at most one pack already — the store is
        // already consolidated; don't churn a fresh identical pack.
        if loose_blobs.is_empty() && loose_trees.is_empty() && old_pack_files.len() <= 1 {
            return Ok((0, 0));
        }

        // Consolidation packs every object that's already packed plus the
        // loose ones. The default path skips the sliding-window delta search
        // to keep foreground GC latency bounded: it searches the full payloads
        // of every object and can turn a seconds-long consolidation into
        // minutes. The caller resolves the repository's GC policy and the
        // `--aggressive` override into the `delta_search` argument. This
        // mirrors the snapshot hot path, whose policy is held by the store.
        let mut compression = self.compression;
        if !delta_search {
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
                let data = tree.encode_canonical()?;
                seen.insert(id);
                builder.add(*hash, PackObjectType::Tree, data);
            }
        }

        if seen.is_empty() {
            return Ok((0, 0));
        }

        let (pack_data, index_data, stats) = builder.build()?;
        let new_pack_name = blake3::hash(&pack_data).to_hex();
        if commit_artifact_ids.is_empty() {
            self.install_pack_files(&pack_data, &index_data)?;
        } else {
            super::pack_install_journal::install_snapshot_pack_bytes_with_commit_markers(
                &packs_dir(&self.root),
                pack_data,
                index_data,
                &commit_artifact_ids,
            )?;
            self.reload_packs()?;
        }
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
            for artifact_id in &commit_artifact_ids {
                remove_file_ignore_missing(&snapshot_commit_marker_path(pack_path, artifact_id))?;
            }
        }
        // Retiring source packs requires a full reload of the pack list.
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
        let states = list_state_ids_from_dir(&states_dir(&self.root))?;

        for hash in &blobs {
            let packed = self
                .pack_manager()
                .read()
                .map_err(|_| {
                    HeddleError::Config("Failed to acquire pack manager lock".to_string())
                })?
                .get_hashed_object(hash)?
                .is_some();
            if packed {
                let path = hash_path(&blobs_dir(&self.root), hash);
                if let Some(bytes) = remove_file_counted(&path)? {
                    bytes_freed = bytes_freed.saturating_add(bytes);
                    removed += 1;
                }
            }
        }

        for hash in &trees {
            let path = hash_path(&trees_dir(&self.root), hash);
            let Some(loose_data) = read_file_bytes(&path)? else {
                continue;
            };
            let loose_body = codec::decode_tree_body(loose_data.as_slice())?;
            let loose_is_delta = crate::object::is_delta_tree(&loose_body);
            let loose_tree = self.decode_tree_storage_body(*hash, &loose_body)?;
            let found = loose_tree.hash();
            if found != *hash {
                return Err(HeddleError::Corruption {
                    expected: *hash,
                    found,
                });
            }
            let npk1_tree = self
                .npk1_manager()
                .read()
                .map_err(|_| {
                    HeddleError::Config("Failed to acquire NPK1 manager lock".to_string())
                })?
                .get_tree(hash)?;
            if npk1_tree.as_ref() == Some(&loose_tree) {
                if let Some(bytes) = remove_file_counted(&path)? {
                    bytes_freed = bytes_freed.saturating_add(bytes);
                    removed += 1;
                }
                continue;
            }
            // Keep main's HDC1 hot-tier safety rule unless an identical,
            // materialized NPK1 tree has already made the delta redundant.
            if loose_is_delta {
                continue;
            }
            let packed = self
                .pack_manager()
                .read()
                .map_err(|_| {
                    HeddleError::Config("Failed to acquire pack manager lock".to_string())
                })?
                .get_hashed_object(hash)?;
            let Some((obj_type, packed_data)) = packed else {
                continue;
            };
            if obj_type != PackObjectType::Tree {
                continue;
            }
            // A loose current tree can intentionally shadow an older packed
            // schema at the same semantic hash. Preserve that migration copy
            // until consolidation replaces the legacy body.
            if crate::object::is_delta_tree(&packed_data) {
                continue;
            }
            let Ok(packed_tree) = codec::decode_tree_serialized_with_key(&packed_data, *hash, None)
            else {
                continue;
            };
            let packed_found = packed_tree.hash();
            if packed_found != *hash {
                return Err(HeddleError::Corruption {
                    expected: *hash,
                    found: packed_found,
                });
            }
            if packed_tree == loose_tree
                && let Some(bytes) = remove_file_counted(&path)?
            {
                bytes_freed = bytes_freed.saturating_add(bytes);
                removed += 1;
            }
        }

        for id in &states {
            let packed = self
                .pack_manager()
                .read()
                .map_err(|_| {
                    HeddleError::Config("Failed to acquire pack manager lock".to_string())
                })?
                .get_object(&PackObjectId::StateId(*id))?;
            let Some((obj_type, packed_data)) = packed else {
                continue;
            };
            if obj_type != PackObjectType::State {
                continue;
            }
            let path = state_path(&self.root, id);
            let Some(loose_data) = read_file_bytes(&path)? else {
                continue;
            };
            let loose_state = codec::decode_state(loose_data.as_slice())?;
            let packed_state = validate_state_serialized(&packed_data, *id)?;
            if !loose_state.accepts_stored_id(id) {
                return Err(HeddleError::InvalidObject(format!(
                    "loose state id mismatch while pruning: expected {id}, computed {}",
                    loose_state.id()
                )));
            }
            if packed_state == loose_state
                && let Some(bytes) = remove_file_counted(&path)?
            {
                bytes_freed = bytes_freed.saturating_add(bytes);
                removed += 1;
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
