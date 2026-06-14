// SPDX-License-Identifier: Apache-2.0
//! Import Git trees as Heddle trees.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use objects::object::{Blob, ChangeId, ContentHash, FileMode, State, Tree, TreeEntry};
use objects::store::{
    CompressionConfig, ObjectStore,
    pack::{ObjectType as PackObjectType, PackObjectId, StreamingPackBuilder},
};
use objects::util::{GitTreeNameClassification, GitTreeNameLossyAction, classify_git_tree_name};
use repo::Repository as HeddleRepository;
use sley::{GitObjectType, ObjectId as SleyObjectId, Repository as SleyRepository};

use crate::bridge::git_core::{GitBridgeError, GitResult};
use crate::bridge::git_util::{GitImportOptions, LossyGitImportEntry};

const SUBMODULE_PREFIX: &str = "heddle-submodule:";

pub struct GitTreeImporter<'a> {
    heddle_repo: &'a HeddleRepository,
    repo: &'a SleyRepository,
    tree_cache: HashMap<SleyObjectId, ContentHash>,
    blob_cache: HashMap<SleyObjectId, ContentHash>,
    options: GitImportOptions,
    lossy_entries: Vec<LossyGitImportEntry>,
    /// When set, imported blobs/trees/states stream into a single native
    /// pack instead of N loose objects (heddle#555). `None` keeps the
    /// legacy loose-write path for callers like [`import_git_tree`].
    pack: Option<PackImportSink>,
}

impl<'a> GitTreeImporter<'a> {
    pub fn new(heddle_repo: &'a HeddleRepository, repo: &'a SleyRepository) -> Self {
        Self::with_options(heddle_repo, repo, GitImportOptions::default())
    }

    pub fn with_options(
        heddle_repo: &'a HeddleRepository,
        repo: &'a SleyRepository,
        options: GitImportOptions,
    ) -> Self {
        Self {
            heddle_repo,
            repo,
            tree_cache: HashMap::new(),
            blob_cache: HashMap::new(),
            options,
            lossy_entries: Vec::new(),
            pack: None,
        }
    }

    /// Like [`Self::with_options`] but routes every imported blob/tree/state
    /// into `sink`'s streaming pack rather than loose object writes. The
    /// caller must call [`Self::finalize_pack_install`] after the walk (or
    /// [`Self::abort_pack`] on failure) to durably install the pack.
    pub(crate) fn with_options_packed(
        heddle_repo: &'a HeddleRepository,
        repo: &'a SleyRepository,
        options: GitImportOptions,
        sink: PackImportSink,
    ) -> Self {
        Self {
            heddle_repo,
            repo,
            tree_cache: HashMap::new(),
            blob_cache: HashMap::new(),
            options,
            lossy_entries: Vec::new(),
            pack: Some(sink),
        }
    }

    /// Persist a Heddle `State` produced by `import_commit` — into the pack
    /// when packing, else loose. Centralizing the write here is what lets
    /// the same walk/identity logic feed either sink.
    pub(crate) fn write_state(&mut self, state: &State) -> GitResult<()> {
        let repo = self.heddle_repo;
        match self.pack.as_mut() {
            Some(sink) => sink.add_state(repo.store(), state),
            None => {
                repo.store().put_state(state)?;
                Ok(())
            }
        }
    }

    /// Whether `change_id`'s state has already been buffered into this run's
    /// in-flight pack. The pack isn't installed until the walk finishes, so
    /// such a state isn't yet readable through the store — the walk's
    /// idempotency check consults this first (heddle#555 risk #2).
    pub(crate) fn state_staged_in_pack(&self, change_id: &ChangeId) -> bool {
        self.pack
            .as_ref()
            .is_some_and(|sink| sink.staged_states.contains(change_id))
    }

    /// Finalize and durably install the pack (no-op for the loose path or an
    /// empty import). Must run before refs/markers/mapping are committed so a
    /// crash can't leave them pointing into a pack that never landed.
    pub(crate) fn finalize_pack_install(&mut self) -> GitResult<()> {
        let repo = self.heddle_repo;
        if let Some(sink) = self.pack.take() {
            sink.finalize_and_install(repo.store())?;
        }
        Ok(())
    }

    /// Discard the in-flight pack and its staging files (failure path).
    pub(crate) fn abort_pack(&mut self) {
        if let Some(sink) = self.pack.take() {
            sink.abort();
        }
    }

    /// Route a blob write to the pack sink or the loose store.
    fn write_blob(&mut self, hash: ContentHash, blob: Blob) -> GitResult<()> {
        let repo = self.heddle_repo;
        match self.pack.as_mut() {
            Some(sink) => sink.add_blob(repo.store(), hash, blob.into_content()),
            None => {
                repo.store().put_blob(&blob)?;
                Ok(())
            }
        }
    }

    /// Route a tree write to the pack sink or the loose store.
    fn write_tree(&mut self, hash: ContentHash, tree: &Tree) -> GitResult<()> {
        let repo = self.heddle_repo;
        match self.pack.as_mut() {
            Some(sink) => sink.add_tree(repo.store(), hash, tree),
            None => {
                repo.store().put_tree(tree)?;
                Ok(())
            }
        }
    }

    pub fn lossy_entries(&self) -> &[LossyGitImportEntry] {
        &self.lossy_entries
    }

    pub(crate) fn lossy_enabled(&self) -> bool {
        self.options.lossy
    }

    pub fn import_tree(&mut self, tree_oid: SleyObjectId) -> GitResult<ContentHash> {
        self.import_tree_at(tree_oid, "")
    }

    fn import_tree_at(
        &mut self,
        tree_oid: SleyObjectId,
        path_prefix: &str,
    ) -> GitResult<ContentHash> {
        if let Some(hash) = self.tree_cache.get(&tree_oid) {
            return Ok(*hash);
        }

        let git_tree_entries = if tree_oid == SleyObjectId::empty_tree(self.repo.object_format()) {
            Vec::new()
        } else {
            self.repo
                .read_tree(&tree_oid)
                .map_err(|err| GitBridgeError::Git(err.to_string()))?
                .entries
        };

        let mut entries = Vec::new();

        for entry in git_tree_entries {
            let Some(name) =
                self.import_entry_name(path_prefix, entry.name.as_bytes(), entry.oid)?
            else {
                continue;
            };

            match entry.mode {
                0o100644 | 0o100755 => {
                    let hash = self.import_blob(entry.oid)?;

                    let mode = if entry.mode == 0o100755 {
                        FileMode::Executable
                    } else {
                        FileMode::Normal
                    };

                    entries.push(TreeEntry {
                        name,
                        mode,
                        entry_type: objects::object::EntryType::Blob,
                        hash,
                    });
                }
                0o120000 => {
                    let hash = self.import_blob(entry.oid)?;
                    // Phase E: must be `EntryType::Symlink` so the
                    // materialization planner reaches the symlink-write
                    // branch. Previously this said `EntryType::Blob`,
                    // which routed checkout through the regular file
                    // path and wrote the symlink target as file content
                    // (so e.g. ripgrep's `HomebrewFormula -> pkg/brew`
                    // appeared on disk as an 8-byte text file containing
                    // "pkg/brew" rather than a symlink).
                    entries.push(TreeEntry {
                        name,
                        mode: FileMode::Symlink,
                        entry_type: objects::object::EntryType::Symlink,
                        hash,
                    });
                }
                0o040000 => {
                    let subtree_hash =
                        self.import_tree_at(entry.oid, &join_tree_path(path_prefix, &name))?;
                    entries.push(TreeEntry {
                        name,
                        mode: FileMode::Normal,
                        entry_type: objects::object::EntryType::Tree,
                        hash: subtree_hash,
                    });
                }
                0o160000 => {
                    let lossy = LossyGitImportEntry::converted(
                        join_tree_path(path_prefix, &name),
                        Some(entry.oid.to_string()),
                        "gitlink/submodule entry converted to a heddle-submodule blob",
                    );
                    self.record_lossy(lossy)?;
                    let hash = self.import_gitlink(entry.oid)?;
                    entries.push(TreeEntry {
                        name,
                        mode: FileMode::Normal,
                        entry_type: objects::object::EntryType::Blob,
                        hash,
                    });
                }
                mode => {
                    return Err(GitBridgeError::Git(format!(
                        "unsupported git tree entry mode {:o} for {}",
                        mode,
                        join_tree_path(path_prefix, &name)
                    )));
                }
            }
        }
        let tree = Tree::from_entries(entries);
        let hash = tree.hash();
        self.write_tree(hash, &tree)?;
        self.tree_cache.insert(tree_oid, hash);
        Ok(hash)
    }

    fn import_entry_name(
        &mut self,
        path_prefix: &str,
        raw_name: &[u8],
        object_id: SleyObjectId,
    ) -> GitResult<Option<String>> {
        match classify_git_tree_name(raw_name) {
            GitTreeNameClassification::Representable(name) => Ok(Some(name)),
            GitTreeNameClassification::NeedsLossy(lossy) => {
                let path = join_tree_path(path_prefix, &lossy.name);
                let entry = match lossy.action {
                    GitTreeNameLossyAction::Dropped => LossyGitImportEntry::dropped(
                        path,
                        Some(object_id.to_string()),
                        lossy.reason,
                    ),
                    GitTreeNameLossyAction::Converted => LossyGitImportEntry::converted(
                        path,
                        Some(object_id.to_string()),
                        lossy.reason,
                    ),
                };
                self.record_lossy(entry)?;
                if matches!(lossy.action, GitTreeNameLossyAction::Dropped) {
                    Ok(None)
                } else {
                    Ok(Some(lossy.name))
                }
            }
        }
    }

    fn record_lossy(&mut self, entry: LossyGitImportEntry) -> GitResult<()> {
        if !self.options.lossy {
            return Err(fail_lossy_entry(&entry));
        }
        tracing::warn!(entry = %entry.summary_line(), "lossy git import accepted");
        self.lossy_entries.push(entry);
        Ok(())
    }

    fn import_blob(&mut self, blob_oid: SleyObjectId) -> GitResult<ContentHash> {
        if let Some(hash) = self.blob_cache.get(&blob_oid) {
            return Ok(*hash);
        }

        let blob = self
            .repo
            .read_object(&blob_oid)
            .map_err(|err| GitBridgeError::Git(err.to_string()))?;
        if blob.object_type != GitObjectType::Blob {
            return Err(GitBridgeError::Git(format!(
                "object {blob_oid} is {}, not a blob",
                blob.object_type.as_str()
            )));
        }

        let heddle_blob = Blob::new(blob.body.to_vec());
        let hash = heddle_blob.hash();
        self.write_blob(hash, heddle_blob)?;
        self.blob_cache.insert(blob_oid, hash);
        Ok(hash)
    }

    fn import_gitlink(&mut self, oid: SleyObjectId) -> GitResult<ContentHash> {
        if let Some(hash) = self.blob_cache.get(&oid) {
            return Ok(*hash);
        }

        let blob = Blob::new(format!("{} {}", SUBMODULE_PREFIX, oid).into_bytes());
        let hash = blob.hash();
        self.write_blob(hash, blob)?;
        self.blob_cache.insert(oid, hash);
        Ok(hash)
    }
}

/// Import a Git tree as a Heddle tree.
pub fn import_git_tree(
    heddle_repo: &HeddleRepository,
    repo: &SleyRepository,
    tree_oid: SleyObjectId,
) -> GitResult<ContentHash> {
    GitTreeImporter::new(heddle_repo, repo).import_tree(tree_oid)
}

pub(crate) fn fail_lossy_entry(entry: &LossyGitImportEntry) -> GitBridgeError {
    GitBridgeError::InvalidMapping(format!(
        "git import cannot represent tree entry losslessly: {}. Retry with --lossy to accept dropping or converting unrepresentable entries.",
        entry.summary_line()
    ))
}

fn join_tree_path(prefix: &str, name: &str) -> String {
    let name = display_tree_name(name);
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

fn display_tree_name(name: &str) -> String {
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        name.escape_debug().to_string()
    } else {
        name.to_string()
    }
}

/// Per-process counter so concurrent imports stage to distinct pack basenames.
static PACK_SINK_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write-sink that streams imported blobs/trees/states into a single native
/// pack instead of writing N loose objects (one durable rename + index vs.
/// N per-object writes). The pack stages under the Heddle store's own
/// directory, then installs atomically via `install_pack_streaming`
/// (rename(2) of pack + index) once the walk succeeds — see heddle#555.
///
/// Every bridge import semantic is preserved: this only changes the
/// durability mechanism, not which objects get written or how their
/// identity is computed.
pub(crate) struct PackImportSink {
    builder: StreamingPackBuilder<File>,
    pack_path: PathBuf,
    index_path: PathBuf,
    bucket_dir: PathBuf,
    object_count: usize,
    /// Heddle hashes already buffered this run. Guards the rare case where a
    /// lossy translation collapses two distinct git trees to one Heddle tree
    /// (the in-flight pack object isn't yet readable via `has_tree`).
    staged_trees: HashSet<ContentHash>,
    /// Change-ids already buffered this run; the walk's idempotency check
    /// reads this before falling back to the store (heddle#555 risk #2).
    staged_states: HashSet<ChangeId>,
}

impl PackImportSink {
    pub(crate) fn new(staging_dir: &Path) -> GitResult<Self> {
        std::fs::create_dir_all(staging_dir)?;
        let seq = PACK_SINK_SEQ.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("bridge-import-{}-{}", std::process::id(), seq);
        let pack_path = staging_dir.join(format!("{run_id}.pack"));
        let index_path = staging_dir.join(format!("{run_id}.idx"));
        let bucket_dir = staging_dir.join(format!("{run_id}-buckets"));

        let pack_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&pack_path)?;
        // Delta search disabled: the streaming builder can't delta anyway and
        // the cost/benefit is poor on real history (matches the ingest path).
        let compression = CompressionConfig {
            max_delta_size: 0,
            ..CompressionConfig::default()
        };
        let builder = StreamingPackBuilder::new(
            pack_file,
            index_path.clone(),
            compression,
            bucket_dir.clone(),
        )?;

        Ok(Self {
            builder,
            pack_path,
            index_path,
            bucket_dir,
            object_count: 0,
            staged_trees: HashSet::new(),
            staged_states: HashSet::new(),
        })
    }

    fn add_blob(
        &mut self,
        store: &impl ObjectStore,
        hash: ContentHash,
        content: Vec<u8>,
    ) -> GitResult<()> {
        // Git blobs are content-addressed, so the caller's per-oid cache
        // already dedups within a run; only the cross-run (re-import) case
        // remains, which `has_blob` covers.
        if store.has_blob(&hash)? {
            return Ok(());
        }
        self.builder.add(hash, PackObjectType::Blob, content)?;
        self.object_count += 1;
        Ok(())
    }

    fn add_tree(
        &mut self,
        store: &impl ObjectStore,
        hash: ContentHash,
        tree: &Tree,
    ) -> GitResult<()> {
        if !self.staged_trees.insert(hash) {
            return Ok(());
        }
        if store.has_tree(&hash)? {
            return Ok(());
        }
        // `to_vec_named` (struct-as-map) is mandatory: every pack reader uses
        // `rmp_serde::from_slice`, which defaults to struct-as-map. Plain
        // `to_vec` would round-trip the bytes but fail deserialization.
        let data = rmp_serde::to_vec_named(tree).map_err(|e| {
            GitBridgeError::InvalidMapping(format!("serialize tree for import pack: {e}"))
        })?;
        self.builder.add(hash, PackObjectType::Tree, data)?;
        self.object_count += 1;
        Ok(())
    }

    fn add_state(&mut self, store: &impl ObjectStore, state: &State) -> GitResult<()> {
        if !self.staged_states.insert(state.change_id) {
            return Ok(());
        }
        if store.has_state(&state.change_id)? {
            return Ok(());
        }
        let data = rmp_serde::to_vec_named(state).map_err(|e| {
            GitBridgeError::InvalidMapping(format!("serialize state for import pack: {e}"))
        })?;
        self.builder.add_id(
            PackObjectId::ChangeId(state.change_id),
            PackObjectType::State,
            data,
        )?;
        self.object_count += 1;
        Ok(())
    }

    fn finalize_and_install(self, store: &impl ObjectStore) -> GitResult<()> {
        let PackImportSink {
            builder,
            pack_path,
            index_path,
            bucket_dir,
            object_count,
            ..
        } = self;

        if object_count == 0 {
            // Nothing new to install — drop the (header-only) staging pack.
            // Dropping the builder cleans the bucket dir.
            drop(builder);
            let _ = std::fs::remove_file(&pack_path);
            let _ = std::fs::remove_dir_all(&bucket_dir);
            return Ok(());
        }

        // Finalize writes the count + trailer to the pack file and the sorted
        // index to `index_path`, then cleans the bucket dir. Drop the file
        // handle before the install renames it into the store.
        let (file, _stats) = builder.finalize()?;
        drop(file);
        store.install_pack_streaming(&pack_path, &index_path)?;
        Ok(())
    }

    fn abort(self) {
        let PackImportSink {
            builder,
            pack_path,
            index_path,
            bucket_dir,
            ..
        } = self;
        // Drop cleans the bucket dir; remove the staged pack/index too.
        drop(builder);
        let _ = std::fs::remove_file(&pack_path);
        let _ = std::fs::remove_file(&index_path);
        let _ = std::fs::remove_dir_all(&bucket_dir);
    }
}
