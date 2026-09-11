// SPDX-License-Identifier: Apache-2.0
//! Tree types: entries, structure, and supporting enums.

use std::{fmt, path::Path, sync::Arc};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use super::{ContentHash, SpoolId, StateId};

/// Durable msgpack encoding version for the flat V3 tree body. This is the
/// serde-representation version, NOT the hash-scheme selector: the scheme is
/// carried separately by [`TreeScheme`] / the body magic. Leave this at 3.
const TREE_FORMAT_VERSION: u8 = 3;
/// Durable msgpack encoding version for a salted V4 tree body. A `version == 4`
/// msgpack body carries a parallel per-entry `salts` column and decodes to
/// [`TreeScheme::V4Salted`].
const TREE_FORMAT_VERSION_V4: u8 = 4;
/// Domain prefix for a V4 per-entry leaf commitment (routed through
/// [`ContentHash::typed_hasher`]).
const TREE_V4_LEAF_PREFIX: &str = "tree-v4-leaf";
/// Domain prefix for a V4 interior Merkle node.
const TREE_V4_NODE_PREFIX: &str = "tree-v4-node";
/// The v3 empty-tree domain prefix. The V4 empty root is defined to equal the
/// V3 empty-tree hash (`ContentHash::compute_typed("tree", b"")`) so the
/// import/nothing-adopted anchor sentinels do not diverge (MF-5).
const TREE_EMPTY_PREFIX: &str = "tree";
const ENTRY_KIND_BLOB: u8 = 0;
const ENTRY_KIND_TREE: u8 = 1;
const ENTRY_KIND_SYMLINK: u8 = 2;
const ENTRY_KIND_GITLINK: u8 = 3;
/// Native child-spool edge: the entry's payload is a spool-id + anchored
/// state-id, not a git commit OID. This link is
/// deliberately NOT a git submodule — see [`FileMode::Spoollink`].
const ENTRY_KIND_SPOOLLINK: u8 = 4;
const GIT_OBJECT_FORMAT_SHA1: u8 = 1;
const GIT_OBJECT_FORMAT_SHA256: u8 = 2;

// ── TreeScheme ──────────────────────────────────────────────────────

/// How a [`Tree`]'s content id is computed. The scheme is part of the
/// in-memory value, so `Tree::hash()` is a pure function of `(scheme, salts,
/// entries)` and the value determines the id at every call site (MF-4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeScheme {
    /// Flat BLAKE3 over the concatenated entry preimages (the historical hash).
    V3Flat,
    /// Salted binary Merkle tree over per-entry leaf commitments, redactable at
    /// entry granularity. Carries a parallel 32-byte salt per entry.
    V4Salted,
}

// ── TreeError ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeError {
    InvalidName(String),
    InvalidStructure(String),
}

impl std::error::Error for TreeError {}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TreeError::InvalidName(msg) => write!(f, "invalid tree entry name: {}", msg),
            TreeError::InvalidStructure(msg) => write!(f, "invalid tree structure: {}", msg),
        }
    }
}

// ── FileMode ────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileMode {
    Normal,
    Executable,
    Symlink,
    Gitlink,
    /// Native child-spool edge. This is NOT a git file mode: a spoollink
    /// points at a spool-id + state-id, not a git object, so it has no valid
    /// git submodule (`160000`) representation and [`Self::to_unix_mode`]
    /// returns `0`. Git-boundary code MUST handle it explicitly rather than
    /// emit a bogus mode.
    Spoollink,
}

impl FileMode {
    pub fn to_byte(&self) -> u8 {
        match self {
            FileMode::Normal => 0,
            FileMode::Executable => 1,
            FileMode::Symlink => 2,
            FileMode::Gitlink => 3,
            FileMode::Spoollink => 4,
        }
    }

    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(FileMode::Normal),
            1 => Some(FileMode::Executable),
            2 => Some(FileMode::Symlink),
            3 => Some(FileMode::Gitlink),
            4 => Some(FileMode::Spoollink),
            _ => None,
        }
    }

    /// The git tree/index mode for this entry. A spoollink has no git mode
    /// (it is not a git object) and returns `0` — callers on a git boundary
    /// must skip spoollinks rather than treat this as a real mode.
    pub fn to_unix_mode(&self) -> u32 {
        match self {
            FileMode::Normal => 0o100644,
            FileMode::Executable => 0o100755,
            FileMode::Symlink => 0o120000,
            FileMode::Gitlink => 0o160000,
            FileMode::Spoollink => 0,
        }
    }
}

// ── EntryType ───────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryType {
    Blob,
    Tree,
    Symlink,
    Gitlink,
    /// Native child-spool edge (see [`TreeEntryTarget::Spoollink`]).
    Spoollink,
}

impl EntryType {
    pub fn to_byte(&self) -> u8 {
        match self {
            EntryType::Blob => 0,
            EntryType::Tree => 1,
            EntryType::Symlink => 2,
            EntryType::Gitlink => 3,
            EntryType::Spoollink => 4,
        }
    }

    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(EntryType::Blob),
            1 => Some(EntryType::Tree),
            2 => Some(EntryType::Symlink),
            3 => Some(EntryType::Gitlink),
            4 => Some(EntryType::Spoollink),
            _ => None,
        }
    }
}

// ── TreeEntryTarget ────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeEntryTarget {
    Blob {
        hash: ContentHash,
        executable: bool,
    },
    Tree {
        hash: ContentHash,
    },
    Symlink {
        hash: ContentHash,
    },
    Gitlink {
        target: GitObjectId,
    },
    /// Native pointer to a child spool: a spool-id plus an anchored state-id.
    /// Unlike [`Self::Gitlink`], this is NOT a git object OID and cannot
    /// round-trip to a git submodule; git-boundary code must handle it
    /// explicitly (skip on export). The Spool children facet consumes this in
    /// a later phase.
    Spoollink {
        spool_id: SpoolId,
        state_id: StateId,
    },
}

impl TreeEntryTarget {
    pub fn entry_type(&self) -> EntryType {
        match self {
            TreeEntryTarget::Blob { .. } => EntryType::Blob,
            TreeEntryTarget::Tree { .. } => EntryType::Tree,
            TreeEntryTarget::Symlink { .. } => EntryType::Symlink,
            TreeEntryTarget::Gitlink { .. } => EntryType::Gitlink,
            TreeEntryTarget::Spoollink { .. } => EntryType::Spoollink,
        }
    }

    pub fn mode(&self) -> FileMode {
        match self {
            TreeEntryTarget::Blob {
                executable: true, ..
            } => FileMode::Executable,
            TreeEntryTarget::Blob { .. } => FileMode::Normal,
            TreeEntryTarget::Tree { .. } => FileMode::Normal,
            TreeEntryTarget::Symlink { .. } => FileMode::Symlink,
            TreeEntryTarget::Gitlink { .. } => FileMode::Gitlink,
            TreeEntryTarget::Spoollink { .. } => FileMode::Spoollink,
        }
    }

    pub fn content_hash(&self) -> Option<ContentHash> {
        match self {
            TreeEntryTarget::Blob { hash, .. }
            | TreeEntryTarget::Tree { hash }
            | TreeEntryTarget::Symlink { hash } => Some(*hash),
            TreeEntryTarget::Gitlink { .. } | TreeEntryTarget::Spoollink { .. } => None,
        }
    }

    pub fn gitlink_target(&self) -> Option<GitObjectId> {
        match self {
            TreeEntryTarget::Gitlink { target } => Some(*target),
            _ => None,
        }
    }

    /// The child-spool pointer `(spool_id, state_id)` for a spoollink entry,
    /// or `None` for any other kind.
    pub fn spoollink_target(&self) -> Option<(&SpoolId, StateId)> {
        match self {
            TreeEntryTarget::Spoollink { spool_id, state_id } => Some((spool_id, *state_id)),
            _ => None,
        }
    }

    fn encoded_payload_len(&self) -> usize {
        match self {
            TreeEntryTarget::Blob { hash, .. }
            | TreeEntryTarget::Tree { hash }
            | TreeEntryTarget::Symlink { hash } => hash.as_bytes().len(),
            TreeEntryTarget::Gitlink { target } => target.as_bytes().len(),
            TreeEntryTarget::Spoollink { spool_id, state_id } => {
                4 + spool_id.as_str().len() + state_id.as_bytes().len()
            }
        }
    }

    fn update_hasher(&self, hasher: &mut blake3::Hasher) {
        self.write_payload(|bytes| {
            hasher.update(bytes);
        });
    }

    /// Emit the canonical `mode ‖ entry_type ‖ target_payload` byte sequence.
    ///
    /// This is the single source of truth for both the V3 flat hash
    /// ([`Self::update_hasher`]) and the V4 leaf preimage
    /// ([`Tree::v4_leaf_preimage`]), so the two encodings can never drift.
    fn write_payload(&self, mut emit: impl FnMut(&[u8])) {
        emit(&[self.mode().to_byte()]);
        emit(&[self.entry_type().to_byte()]);
        match self {
            TreeEntryTarget::Blob { hash, .. }
            | TreeEntryTarget::Tree { hash }
            | TreeEntryTarget::Symlink { hash } => emit(hash.as_bytes()),
            TreeEntryTarget::Gitlink { target } => {
                emit(&[git_format_to_tag(target.format())]);
                emit(target.as_bytes());
            }
            TreeEntryTarget::Spoollink { spool_id, state_id } => {
                emit(&(spool_id.as_str().len() as u32).to_le_bytes());
                emit(spool_id.as_str().as_bytes());
                emit(state_id.as_bytes());
            }
        };
    }
}

// ── TreeEntry ───────────────────────────────────────────────────────

pub fn validate_name(name: &str) -> Result<(), TreeError> {
    if name.is_empty() {
        return Err(TreeError::InvalidName("entry name cannot be empty".into()));
    }
    if name == "." || name == ".." {
        return Err(TreeError::InvalidName(format!(
            "'{}' is not a valid entry name",
            name
        )));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(TreeError::InvalidName(
            "entry name cannot contain path separators".into(),
        ));
    }
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(TreeError::InvalidName(
            "entry name contains control characters".into(),
        ));
    }
    if name.len() > u16::MAX as usize {
        return Err(TreeError::InvalidName(
            "entry name exceeds the HTR4 u16 length bound".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    name: String,
    target: TreeEntryTarget,
}

impl TreeEntry {
    pub(crate) fn validate(&self) -> Result<(), TreeError> {
        validate_name(&self.name)
    }

    pub fn file(
        name: impl Into<String>,
        hash: ContentHash,
        executable: bool,
    ) -> Result<Self, TreeError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self {
            name,
            target: TreeEntryTarget::Blob { hash, executable },
        })
    }

    pub fn directory(name: impl Into<String>, hash: ContentHash) -> Result<Self, TreeError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self {
            name,
            target: TreeEntryTarget::Tree { hash },
        })
    }

    pub fn symlink(name: impl Into<String>, hash: ContentHash) -> Result<Self, TreeError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self {
            name,
            target: TreeEntryTarget::Symlink { hash },
        })
    }

    pub fn gitlink(name: impl Into<String>, target: GitObjectId) -> Result<Self, TreeError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self {
            name,
            target: TreeEntryTarget::Gitlink { target },
        })
    }

    /// Build a native child-spool edge: a pointer to `spool_id` anchored at
    /// `state_id`. Not a git submodule (see [`TreeEntryTarget::Spoollink`]).
    pub fn spoollink(
        name: impl Into<String>,
        spool_id: SpoolId,
        state_id: StateId,
    ) -> Result<Self, TreeError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self {
            name,
            target: TreeEntryTarget::Spoollink { spool_id, state_id },
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_name(&mut self, name: impl Into<String>) -> Result<(), TreeError> {
        let name = name.into();
        validate_name(&name)?;
        self.name = name;
        Ok(())
    }

    pub fn with_mode(&self, mode: FileMode) -> Result<Self, TreeError> {
        match (&self.target, mode) {
            (TreeEntryTarget::Blob { hash, .. }, FileMode::Normal | FileMode::Executable) => {
                Self::file(self.name.clone(), *hash, mode == FileMode::Executable)
            }
            (TreeEntryTarget::Symlink { .. }, FileMode::Symlink)
            | (TreeEntryTarget::Tree { .. }, _)
            | (TreeEntryTarget::Gitlink { .. }, FileMode::Gitlink)
            | (TreeEntryTarget::Spoollink { .. }, FileMode::Spoollink)
                if mode == self.mode() =>
            {
                Ok(self.clone())
            }
            _ => Err(TreeError::InvalidStructure(format!(
                "cannot apply mode {:?} to {:?} entry '{}'",
                mode,
                self.entry_type(),
                self.name
            ))),
        }
    }

    pub fn target(&self) -> &TreeEntryTarget {
        &self.target
    }

    pub fn entry_type(&self) -> EntryType {
        self.target.entry_type()
    }

    pub fn mode(&self) -> FileMode {
        self.target.mode()
    }

    pub fn content_hash(&self) -> Option<ContentHash> {
        self.target.content_hash()
    }

    pub fn leaf_content_hash(&self) -> Option<ContentHash> {
        match self.target {
            TreeEntryTarget::Blob { hash, .. } | TreeEntryTarget::Symlink { hash } => Some(hash),
            TreeEntryTarget::Tree { .. }
            | TreeEntryTarget::Gitlink { .. }
            | TreeEntryTarget::Spoollink { .. } => None,
        }
    }

    pub fn require_content_hash(&self) -> ContentHash {
        self.content_hash()
            .expect("tree entry target does not carry a Heddle content hash")
    }

    pub fn blob_hash(&self) -> Option<ContentHash> {
        match self.target {
            TreeEntryTarget::Blob { hash, .. } => Some(hash),
            _ => None,
        }
    }

    pub fn tree_hash(&self) -> Option<ContentHash> {
        match self.target {
            TreeEntryTarget::Tree { hash } => Some(hash),
            _ => None,
        }
    }

    pub fn symlink_hash(&self) -> Option<ContentHash> {
        match self.target {
            TreeEntryTarget::Symlink { hash } => Some(hash),
            _ => None,
        }
    }

    pub fn gitlink_target(&self) -> Option<GitObjectId> {
        self.target.gitlink_target()
    }

    /// The `(spool_id, state_id)` pointer for a spoollink entry, else `None`.
    pub fn spoollink_target(&self) -> Option<(&SpoolId, StateId)> {
        self.target.spoollink_target()
    }

    pub fn is_tree(&self) -> bool {
        self.entry_type() == EntryType::Tree
    }

    pub fn is_blob(&self) -> bool {
        self.entry_type() == EntryType::Blob
    }

    pub fn is_symlink(&self) -> bool {
        self.entry_type() == EntryType::Symlink
    }

    pub fn is_gitlink(&self) -> bool {
        self.entry_type() == EntryType::Gitlink
    }

    pub fn is_spoollink(&self) -> bool {
        self.entry_type() == EntryType::Spoollink
    }

    pub fn is_executable(&self) -> bool {
        self.mode() == FileMode::Executable
    }

    pub(crate) fn encoded_len(&self) -> usize {
        1 + 1 + self.target.encoded_payload_len() + self.name.len() + 1
    }

    /// Owned name-plus-target bytes used by streaming page budgets.
    pub fn decoded_size(&self) -> usize {
        self.name.len() + self.target.encoded_payload_len()
    }

    pub(crate) fn update_hasher(&self, hasher: &mut blake3::Hasher) {
        self.target.update_hasher(hasher);
        hasher.update(self.name.as_bytes());
        hasher.update(&[0]);
    }
}

// ── Tree ────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    // Trees are immutable on every read path and only change while a caller is
    // constructing a replacement tree. Sharing the entry vector makes those
    // read-path clones O(1); insert/remove detach with copy-on-write.
    entries: Arc<Vec<TreeEntry>>,
    // How this tree's id is computed. V3 trees carry `salts.is_empty()`.
    scheme: TreeScheme,
    // Per-entry 32-byte salts, parallel to `entries` (same index / name order).
    // Non-empty iff `scheme == TreeScheme::V4Salted`, in which case
    // `salts.len() == entries.len()` is a maintained invariant.
    salts: Arc<Vec<[u8; 32]>>,
}

impl Tree {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Vec::new()),
            scheme: TreeScheme::V3Flat,
            salts: Arc::new(Vec::new()),
        }
    }

    pub fn from_entries(mut entries: Vec<TreeEntry>) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            entries: Arc::new(entries),
            scheme: TreeScheme::V3Flat,
            salts: Arc::new(Vec::new()),
        }
    }

    /// Build a salted V4 tree from entries and their parallel salts.
    ///
    /// `salts[i]` is the salt for `entries[i]` (before sorting); the pair is
    /// sorted together by entry name so the parallel-vector invariant holds.
    /// The sticky-salt *inheritance* policy is a later capture-leg concern —
    /// this constructor carries whatever salts it is given.
    pub fn from_entries_salted_v4(
        entries: Vec<TreeEntry>,
        salts: Vec<[u8; 32]>,
    ) -> Result<Self, TreeError> {
        if entries.len() != salts.len() {
            return Err(TreeError::InvalidStructure(format!(
                "v4 tree has {} entries but {} salts",
                entries.len(),
                salts.len()
            )));
        }
        let mut paired: Vec<(TreeEntry, [u8; 32])> = entries.into_iter().zip(salts).collect();
        paired.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        let (entries, salts): (Vec<TreeEntry>, Vec<[u8; 32]>) = paired.into_iter().unzip();
        Self::try_from_decoded_entries_salted_v4(entries, salts)
    }

    /// Build a tree from entries that are already in canonical name order.
    ///
    /// Unlike [`Self::from_entries`], this does not sort. Decoders use it so
    /// eager and streaming paths reject the same out-of-order or duplicate
    /// encodings instead of silently canonicalizing them.
    pub fn try_from_decoded_entries(entries: Vec<TreeEntry>) -> Result<Self, TreeError> {
        let tree = Self {
            entries: Arc::new(entries),
            scheme: TreeScheme::V3Flat,
            salts: Arc::new(Vec::new()),
        };
        tree.validate()?;
        Ok(tree)
    }

    /// Build a salted V4 tree from already-name-ordered entries and their
    /// parallel salts. Decoders (HSR1, msgpack v4) use this: it does not sort,
    /// so it rejects the same out-of-order/duplicate encodings V3 does.
    pub fn try_from_decoded_entries_salted_v4(
        entries: Vec<TreeEntry>,
        salts: Vec<[u8; 32]>,
    ) -> Result<Self, TreeError> {
        if entries.len() != salts.len() {
            return Err(TreeError::InvalidStructure(format!(
                "v4 tree has {} entries but {} salts",
                entries.len(),
                salts.len()
            )));
        }
        let tree = Self {
            entries: Arc::new(entries),
            scheme: TreeScheme::V4Salted,
            salts: Arc::new(salts),
        };
        tree.validate()?;
        Ok(tree)
    }

    /// The hashing scheme this tree's id is computed under.
    pub fn scheme(&self) -> TreeScheme {
        self.scheme
    }

    /// The parallel per-entry salt vector (empty for V3 trees).
    pub fn salts(&self) -> &[[u8; 32]] {
        &self.salts
    }

    /// The salt for the entry at `index` (V4 only), or `None` for V3 / out of
    /// range.
    pub fn salt_at(&self, index: usize) -> Option<[u8; 32]> {
        self.salts.get(index).copied()
    }

    pub fn validate(&self) -> Result<(), TreeError> {
        match self.scheme {
            TreeScheme::V3Flat => {
                if !self.salts.is_empty() {
                    return Err(TreeError::InvalidStructure(
                        "v3 tree must not carry per-entry salts".into(),
                    ));
                }
            }
            TreeScheme::V4Salted => {
                if self.salts.len() != self.entries.len() {
                    return Err(TreeError::InvalidStructure(format!(
                        "v4 tree has {} entries but {} salts",
                        self.entries.len(),
                        self.salts.len()
                    )));
                }
            }
        }
        let mut previous_name: Option<&str> = None;
        for entry in self.entries.iter() {
            entry.validate()?;
            if let Some(previous) = previous_name
                && previous >= entry.name.as_str()
            {
                return Err(TreeError::InvalidStructure(
                    "entries must be strictly sorted by name".to_string(),
                ));
            }
            previous_name = Some(&entry.name);
        }
        Ok(())
    }

    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    pub fn get(&self, name: &str) -> Option<&TreeEntry> {
        let index = self
            .entries
            .binary_search_by(|entry| entry.name.as_str().cmp(name))
            .ok()?;
        self.entries.get(index)
    }

    pub fn insert(&mut self, entry: TreeEntry) {
        match self.scheme {
            TreeScheme::V3Flat => {
                let entries = Arc::make_mut(&mut self.entries);
                entries.retain(|e| e.name != entry.name);
                let pos = entries
                    .iter()
                    .position(|e| e.name > entry.name)
                    .unwrap_or(entries.len());
                entries.insert(pos, entry);
            }
            TreeScheme::V4Salted => {
                // A fresh insert or a changed entry mints a fresh 256-bit salt.
                // (Sticky-salt *inheritance* on unchanged entries is applied by
                // the capture leg before it constructs the tree, not here.)
                self.insert_salted(entry, rand::random());
            }
        }
    }

    /// V4 insert with an explicit salt, maintaining the parallel salt vector.
    /// Replacing an existing entry of the same name drops its old salt.
    pub fn insert_salted(&mut self, entry: TreeEntry, salt: [u8; 32]) {
        debug_assert_eq!(self.scheme, TreeScheme::V4Salted);
        let entries = Arc::make_mut(&mut self.entries);
        let salts = Arc::make_mut(&mut self.salts);
        if let Some(existing) = entries.iter().position(|e| e.name == entry.name) {
            entries.remove(existing);
            salts.remove(existing);
        }
        let pos = entries
            .iter()
            .position(|e| e.name > entry.name)
            .unwrap_or(entries.len());
        entries.insert(pos, entry);
        salts.insert(pos, salt);
    }

    pub fn remove(&mut self, name: &str) -> Option<TreeEntry> {
        let pos = self.entries.iter().position(|e| e.name == name)?;
        if matches!(self.scheme, TreeScheme::V4Salted) {
            Arc::make_mut(&mut self.salts).remove(pos);
        }
        Some(Arc::make_mut(&mut self.entries).remove(pos))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn hash(&self) -> ContentHash {
        match self.scheme {
            TreeScheme::V3Flat => self.flat_hash_v3(),
            TreeScheme::V4Salted => self.merkle_root_v4(),
        }
    }

    /// The historical flat hash: typed BLAKE3 over every entry preimage.
    fn flat_hash_v3(&self) -> ContentHash {
        let total_len: usize = self.entries.iter().map(TreeEntry::encoded_len).sum();
        ContentHash::compute_typed_with_len(TREE_EMPTY_PREFIX, total_len as u64, |hasher| {
            for entry in self.entries.iter() {
                entry.update_hasher(hasher);
            }
        })
    }

    /// The V4 salted per-entry leaf commitment for `entries[index]`.
    ///
    /// `leaf = typed_hasher("tree-v4-leaf", len)(salt ‖ mode ‖ entry_type ‖
    /// target_payload ‖ name_len(u16 LE) ‖ name)`, where the
    /// `mode ‖ entry_type ‖ target_payload` bytes are exactly those
    /// [`TreeEntryTarget::write_payload`] emits.
    ///
    /// Panics only via `debug_assert` if called on a V3 tree or out of range;
    /// production callers go through [`Self::merkle_root_v4`].
    fn v4_leaf_hash(entry: &TreeEntry, salt: &[u8; 32]) -> ContentHash {
        let preimage = Self::v4_leaf_preimage(entry, salt);
        ContentHash::compute_typed(TREE_V4_LEAF_PREFIX, &preimage)
    }

    /// The exact byte preimage hashed by [`Self::v4_leaf_hash`].
    fn v4_leaf_preimage(entry: &TreeEntry, salt: &[u8; 32]) -> Vec<u8> {
        let name = entry.name.as_bytes();
        // salt(32) + mode(1) + type(1) + target_payload + name_len(2) + name
        let mut buf =
            Vec::with_capacity(32 + 2 + entry.target.encoded_payload_len() + 2 + name.len());
        buf.extend_from_slice(salt);
        entry
            .target
            .write_payload(|bytes| buf.extend_from_slice(bytes));
        // Names are bounded to u16::MAX by `validate_name`.
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(name);
        buf
    }

    /// The V4 Merkle root over the salted per-entry leaves, ordered by leaf
    /// hash (§2). The empty tree reproduces the V3 empty-tree id (MF-5).
    fn merkle_root_v4(&self) -> ContentHash {
        let mut leaves: Vec<ContentHash> = self
            .entries
            .iter()
            .zip(self.salts.iter())
            .map(|(entry, salt)| Self::v4_leaf_hash(entry, salt))
            .collect();
        merkle_root_from_leaves(&mut leaves)
    }

    pub fn iter(&self) -> impl Iterator<Item = &TreeEntry> {
        self.entries.iter()
    }

    pub fn get_path(&self, path: &Path) -> Option<&TreeEntry> {
        let name = path.file_name()?.to_str()?;
        if path.parent().is_none_or(|p| p.as_os_str().is_empty()) {
            self.get(name)
        } else {
            None
        }
    }
}

// ── V4 Merkle root + PartialTree ────────────────────────────────────

/// Compute the RFC 6962 Merkle Tree Hash over V4 leaf hashes.
///
/// Leaves are sorted ascending by their 32-byte leaf hash first (§2.2): every
/// party — a full holder recomputing leaves, or a redacted-tip holder handed
/// opaque leaf hashes — sorts the identical list, so [`Tree`] and
/// [`PartialTree`] reconstruct byte-identical roots. Ordering is by leaf hash
/// alone (no preimage tie-break): a 256-bit leaf collision is cryptographically
/// negligible, and hash-only ordering is what lets a redacted leaf (which
/// carries no preimage) participate in the same total order.
fn merkle_root_from_leaves(leaves: &mut [ContentHash]) -> ContentHash {
    leaves.sort_unstable();
    merkle_tree_hash(leaves)
}

/// RFC 6962 Merkle Tree Hash over already-ordered leaves.
fn merkle_tree_hash(leaves: &[ContentHash]) -> ContentHash {
    match leaves.len() {
        // Empty parity (MF-5): the V4 empty root equals the V3 empty-tree id.
        0 => ContentHash::compute_typed(TREE_EMPTY_PREFIX, b""),
        1 => leaves[0],
        n => {
            // k = largest power of two strictly less than n (RFC 6962:
            // k < n <= 2k). `leading_zeros` is taken on `usize` (not a widened
            // u64) so the shift is arch-independent — a crypto path must not
            // depend on the pointer width (wasm32 has usize::BITS == 32).
            let k = 1usize << ((usize::BITS - 1) - (n - 1).leading_zeros());
            let left = merkle_tree_hash(&leaves[..k]);
            let right = merkle_tree_hash(&leaves[k..]);
            let mut hasher = ContentHash::typed_hasher(TREE_V4_NODE_PREFIX, 64);
            hasher.update(left.as_bytes());
            hasher.update(right.as_bytes());
            ContentHash::from_bytes(hasher.finalize().into())
        }
    }
}

/// One leaf of a [`PartialTree`]: either fully visible (carrying its entry and
/// salt, so its leaf hash is recomputable) or redacted to an opaque 32-byte
/// leaf hash (salt, name, and target all withheld).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartialTreeLeaf {
    Visible { entry: TreeEntry, salt: [u8; 32] },
    Redacted { leaf_hash: ContentHash },
}

impl PartialTreeLeaf {
    /// The leaf hash this leaf contributes to the Merkle root.
    pub fn leaf_hash(&self) -> ContentHash {
        match self {
            PartialTreeLeaf::Visible { entry, salt } => Tree::v4_leaf_hash(entry, salt),
            PartialTreeLeaf::Redacted { leaf_hash } => *leaf_hash,
        }
    }

    pub fn is_redacted(&self) -> bool {
        matches!(self, PartialTreeLeaf::Redacted { .. })
    }
}

/// A redaction projection of a V4 [`Tree`]: visible entries keep their
/// preimage + salt; redacted entries are reduced to their opaque 32-byte leaf
/// hash. A `PartialTree` reconstructs the SAME Merkle root as the full tree, so
/// a state that commits to the full tree still verifies against the projection.
///
/// This is deliberately NOT a `Tree` (a `Tree` requires a resolved name+target
/// for every entry); a viewer holding redacted leaves cannot author over them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartialTree {
    declared_root: ContentHash,
    leaves: Vec<PartialTreeLeaf>,
}

impl PartialTree {
    /// Assemble a partial tree from its declared root and leaves. Callers that
    /// need the root checked should use [`Self::verify`] or
    /// [`Self::from_leaves_verified`].
    pub fn new(declared_root: ContentHash, leaves: Vec<PartialTreeLeaf>) -> Self {
        Self {
            declared_root,
            leaves,
        }
    }

    /// Assemble and verify that the leaves reconstruct `declared_root`.
    pub fn from_leaves_verified(
        declared_root: ContentHash,
        leaves: Vec<PartialTreeLeaf>,
    ) -> Result<Self, TreeError> {
        let partial = Self::new(declared_root, leaves);
        partial.verify()?;
        Ok(partial)
    }

    /// Project a full V4 tree, redacting every entry whose leaf hash is in
    /// `redacted`. Entries not in `redacted` stay visible. Errors on a V3 tree
    /// (nothing to salt) or a broken salt invariant.
    pub fn project(
        tree: &Tree,
        redacted: &std::collections::HashSet<ContentHash>,
    ) -> Result<Self, TreeError> {
        if tree.scheme != TreeScheme::V4Salted {
            return Err(TreeError::InvalidStructure(
                "cannot project a redacted tree from a non-v4 tree".into(),
            ));
        }
        tree.validate()?;
        let declared_root = tree.hash();
        let leaves = tree
            .entries
            .iter()
            .zip(tree.salts.iter())
            .map(|(entry, salt)| {
                let leaf_hash = Tree::v4_leaf_hash(entry, salt);
                if redacted.contains(&leaf_hash) {
                    PartialTreeLeaf::Redacted { leaf_hash }
                } else {
                    PartialTreeLeaf::Visible {
                        entry: entry.clone(),
                        salt: *salt,
                    }
                }
            })
            .collect();
        Ok(Self {
            declared_root,
            leaves,
        })
    }

    pub fn declared_root(&self) -> ContentHash {
        self.declared_root
    }

    pub fn leaves(&self) -> &[PartialTreeLeaf] {
        &self.leaves
    }

    pub fn redacted_count(&self) -> usize {
        self.leaves.iter().filter(|leaf| leaf.is_redacted()).count()
    }

    pub fn has_redactions(&self) -> bool {
        self.leaves.iter().any(PartialTreeLeaf::is_redacted)
    }

    /// Reconstruct the Merkle root from the (visible + redacted) leaves.
    pub fn reconstruct_root(&self) -> ContentHash {
        let mut leaves: Vec<ContentHash> =
            self.leaves.iter().map(PartialTreeLeaf::leaf_hash).collect();
        merkle_root_from_leaves(&mut leaves)
    }

    /// Verify the reconstructed root equals the declared root.
    pub fn verify(&self) -> Result<(), TreeError> {
        let found = self.reconstruct_root();
        if found != self.declared_root {
            return Err(TreeError::InvalidStructure(format!(
                "partial tree reconstructs {found} but declares {}",
                self.declared_root
            )));
        }
        Ok(())
    }

    /// Losslessly convert a fully-visible partial tree back to a V4 [`Tree`].
    /// Errors if any leaf is redacted (the name/target are unknown) or the
    /// reconstructed root does not match the declared root.
    pub fn into_tree(self) -> Result<Tree, TreeError> {
        self.verify()?;
        let mut entries = Vec::with_capacity(self.leaves.len());
        let mut salts = Vec::with_capacity(self.leaves.len());
        for leaf in self.leaves {
            match leaf {
                PartialTreeLeaf::Visible { entry, salt } => {
                    entries.push(entry);
                    salts.push(salt);
                }
                PartialTreeLeaf::Redacted { .. } => {
                    return Err(TreeError::InvalidStructure(
                        "cannot materialize a redacted leaf into a full tree".into(),
                    ));
                }
            }
        }
        Tree::from_entries_salted_v4(entries, salts)
    }
}

// ── Durable V2 tree encoding ───────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct EncodedTreeV2 {
    version: u8,
    entries: Vec<EncodedTreeEntryV2>,
    // Parallel per-entry salts for a V4 salted tree. `default` keeps V3 bodies
    // byte-identical (the field is omitted entirely for V3), so existing
    // on-disk caches (`worktree-current-tree.bin`, hot sidecars) are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    salts: Option<Vec<[u8; 32]>>,
}

#[derive(Serialize, Deserialize)]
struct EncodedTreeEntryV2 {
    name: String,
    kind: u8,
    hash: Option<ContentHash>,
    executable: Option<bool>,
    git_format: Option<u8>,
    git_oid: Option<Vec<u8>>,
    // Child-spool pointer for SPOOLLINK entries. `default`
    // keeps the encoding backward-compatible: pre-SPOOLLINK payloads simply
    // omit these fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    spool_id: Option<SpoolId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    spool_state_id: Option<StateId>,
}

impl Serialize for Tree {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        EncodedTreeV2::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Tree {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = EncodedTreeV2::deserialize(deserializer)?;
        Tree::try_from(encoded).map_err(de::Error::custom)
    }
}

#[derive(Debug)]
pub enum TreeDecodeError {
    Decode(rmp_serde::decode::Error),
    Invalid(TreeError),
}

impl From<rmp_serde::decode::Error> for TreeDecodeError {
    fn from(error: rmp_serde::decode::Error) -> Self {
        Self::Decode(error)
    }
}

impl From<TreeError> for TreeDecodeError {
    fn from(error: TreeError) -> Self {
        Self::Invalid(error)
    }
}

impl From<&Tree> for EncodedTreeV2 {
    fn from(tree: &Tree) -> Self {
        let (version, salts) = match tree.scheme {
            TreeScheme::V3Flat => (TREE_FORMAT_VERSION, None),
            TreeScheme::V4Salted => (TREE_FORMAT_VERSION_V4, Some(tree.salts.as_ref().clone())),
        };
        Self {
            version,
            entries: tree.entries.iter().map(EncodedTreeEntryV2::from).collect(),
            salts,
        }
    }
}

impl From<&TreeEntry> for EncodedTreeEntryV2 {
    fn from(entry: &TreeEntry) -> Self {
        match entry.target() {
            TreeEntryTarget::Blob { hash, executable } => Self {
                name: entry.name.clone(),
                kind: ENTRY_KIND_BLOB,
                hash: Some(*hash),
                executable: Some(*executable),
                git_format: None,
                git_oid: None,
                spool_id: None,
                spool_state_id: None,
            },
            TreeEntryTarget::Tree { hash } => Self {
                name: entry.name.clone(),
                kind: ENTRY_KIND_TREE,
                hash: Some(*hash),
                executable: None,
                git_format: None,
                git_oid: None,
                spool_id: None,
                spool_state_id: None,
            },
            TreeEntryTarget::Symlink { hash } => Self {
                name: entry.name.clone(),
                kind: ENTRY_KIND_SYMLINK,
                hash: Some(*hash),
                executable: None,
                git_format: None,
                git_oid: None,
                spool_id: None,
                spool_state_id: None,
            },
            TreeEntryTarget::Gitlink { target } => Self {
                name: entry.name.clone(),
                kind: ENTRY_KIND_GITLINK,
                hash: None,
                executable: None,
                git_format: Some(git_format_to_tag(target.format())),
                git_oid: Some(target.as_bytes().to_vec()),
                spool_id: None,
                spool_state_id: None,
            },
            TreeEntryTarget::Spoollink { spool_id, state_id } => Self {
                name: entry.name.clone(),
                kind: ENTRY_KIND_SPOOLLINK,
                hash: None,
                executable: None,
                git_format: None,
                git_oid: None,
                spool_id: Some(spool_id.clone()),
                spool_state_id: Some(*state_id),
            },
        }
    }
}

impl TryFrom<EncodedTreeV2> for Tree {
    type Error = TreeError;

    fn try_from(encoded: EncodedTreeV2) -> Result<Self, Self::Error> {
        let mut entries = Vec::with_capacity(encoded.entries.len());
        for entry in encoded.entries {
            entries.push(TreeEntry::try_from(entry)?);
        }
        match encoded.version {
            TREE_FORMAT_VERSION => {
                if encoded.salts.is_some_and(|salts| !salts.is_empty()) {
                    return Err(TreeError::InvalidStructure(
                        "v3 tree body must not carry salts".into(),
                    ));
                }
                Tree::try_from_decoded_entries(entries)
            }
            TREE_FORMAT_VERSION_V4 => {
                let salts = encoded.salts.ok_or_else(|| {
                    TreeError::InvalidStructure("v4 tree body is missing its salts".into())
                })?;
                // `try_from_decoded_entries_salted_v4` re-checks len parity and
                // strict name ordering.
                Tree::try_from_decoded_entries_salted_v4(entries, salts)
            }
            other => Err(TreeError::InvalidStructure(format!(
                "unsupported tree format version {other}; this binary writes {TREE_FORMAT_VERSION} (v3) or {TREE_FORMAT_VERSION_V4} (v4)"
            ))),
        }
    }
}

impl Tree {
    pub fn decode_current_msgpack(data: &[u8]) -> Result<Self, TreeDecodeError> {
        let encoded: EncodedTreeV2 = rmp_serde::from_slice(data)?;
        Ok(Tree::try_from(encoded)?)
    }
}

impl TryFrom<EncodedTreeEntryV2> for TreeEntry {
    type Error = TreeError;

    fn try_from(encoded: EncodedTreeEntryV2) -> Result<Self, Self::Error> {
        match encoded.kind {
            ENTRY_KIND_BLOB => TreeEntry::file(
                encoded.name,
                required_hash(encoded.hash, ENTRY_KIND_BLOB)?,
                encoded.executable.unwrap_or(false),
            ),
            ENTRY_KIND_TREE => {
                TreeEntry::directory(encoded.name, required_hash(encoded.hash, ENTRY_KIND_TREE)?)
            }
            ENTRY_KIND_SYMLINK => TreeEntry::symlink(
                encoded.name,
                required_hash(encoded.hash, ENTRY_KIND_SYMLINK)?,
            ),
            ENTRY_KIND_GITLINK => {
                let format = git_format_from_tag(required_git_format(
                    encoded.git_format,
                    ENTRY_KIND_GITLINK,
                )?)?;
                let oid = encoded.git_oid.ok_or_else(|| {
                    TreeError::InvalidStructure("gitlink entry is missing git_oid".into())
                })?;
                let target = GitObjectId::from_raw(format, &oid).map_err(|err| {
                    TreeError::InvalidStructure(format!("invalid gitlink target: {err}"))
                })?;
                TreeEntry::gitlink(encoded.name, target)
            }
            ENTRY_KIND_SPOOLLINK => {
                let spool_id = encoded.spool_id.ok_or_else(|| {
                    TreeError::InvalidStructure("spoollink entry is missing spool_id".into())
                })?;
                let state_id = encoded.spool_state_id.ok_or_else(|| {
                    TreeError::InvalidStructure("spoollink entry is missing spool_state_id".into())
                })?;
                TreeEntry::spoollink(encoded.name, spool_id, state_id)
            }
            other => Err(TreeError::InvalidStructure(format!(
                "unknown tree entry kind {other}"
            ))),
        }
    }
}

fn required_hash(hash: Option<ContentHash>, kind: u8) -> Result<ContentHash, TreeError> {
    hash.ok_or_else(|| TreeError::InvalidStructure(format!("entry kind {kind} is missing hash")))
}

fn required_git_format(format: Option<u8>, kind: u8) -> Result<u8, TreeError> {
    format.ok_or_else(|| {
        TreeError::InvalidStructure(format!("entry kind {kind} is missing git_format"))
    })
}

pub(crate) fn git_format_to_tag(format: GitObjectFormat) -> u8 {
    match format {
        GitObjectFormat::Sha1 => GIT_OBJECT_FORMAT_SHA1,
        GitObjectFormat::Sha256 => GIT_OBJECT_FORMAT_SHA256,
    }
}

pub(crate) fn git_format_from_tag(tag: u8) -> Result<GitObjectFormat, TreeError> {
    match tag {
        GIT_OBJECT_FORMAT_SHA1 => Ok(GitObjectFormat::Sha1),
        GIT_OBJECT_FORMAT_SHA256 => Ok(GitObjectFormat::Sha256),
        other => Err(TreeError::InvalidStructure(format!(
            "unknown git object format tag {other}"
        ))),
    }
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoIterator for Tree {
    type Item = TreeEntry;
    type IntoIter = std::vec::IntoIter<TreeEntry>;

    fn into_iter(self) -> Self::IntoIter {
        Arc::try_unwrap(self.entries)
            .unwrap_or_else(|entries| (*entries).clone())
            .into_iter()
    }
}

impl<'a> IntoIterator for &'a Tree {
    type Item = &'a TreeEntry;
    type IntoIter = std::slice::Iter<'a, TreeEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

#[cfg(test)]
mod spoollink_tests {
    use super::*;

    #[test]
    fn spoollink_entry_shape() {
        let spool_id = SpoolId::parse("acme/child").unwrap();
        let state_id = StateId::from_bytes([9u8; 32]);
        let entry = TreeEntry::spoollink("child", spool_id.clone(), state_id).unwrap();

        assert!(entry.is_spoollink());
        assert_eq!(entry.entry_type(), EntryType::Spoollink);
        assert_eq!(entry.mode(), FileMode::Spoollink);
        // Native edge carries no Heddle content hash and no git OID.
        assert_eq!(entry.content_hash(), None);
        assert_eq!(entry.leaf_content_hash(), None);
        assert_eq!(entry.gitlink_target(), None);
        assert_eq!(entry.spoollink_target(), Some((&spool_id, state_id)));
    }

    #[test]
    fn spoollink_roundtrips_through_encoded_tree_v2() {
        let spool_id = SpoolId::parse("acme/child").unwrap();
        let state_id = StateId::from_bytes([2u8; 32]);

        // Mix a spoollink alongside the existing kinds so the round-trip also
        // proves existing entries are undisturbed.
        let blob_hash = ContentHash::compute(b"hello");
        let tree = Tree::from_entries(vec![
            TreeEntry::file("a_blob", blob_hash, false).unwrap(),
            TreeEntry::spoollink("z_child", spool_id.clone(), state_id).unwrap(),
        ]);

        let bytes = rmp_serde::to_vec(&tree).unwrap();
        let decoded = Tree::decode_current_msgpack(&bytes).unwrap();

        assert_eq!(decoded, tree, "tree round-trip must be lossless");

        let child = decoded
            .get("z_child")
            .expect("spoollink survives round-trip");
        assert_eq!(child.spoollink_target(), Some((&spool_id, state_id)));
        assert_eq!(child.entry_type(), EntryType::Spoollink);

        // Hash is stable and distinct from a same-name gitlink/blob shape.
        assert_eq!(decoded.hash(), tree.hash());
    }

    #[test]
    fn file_mode_spoollink_has_no_git_mode() {
        // The whole point of a dedicated kind: it must NOT masquerade as a
        // git submodule (160000) or any other real git mode.
        assert_eq!(FileMode::Spoollink.to_unix_mode(), 0);
        assert_ne!(FileMode::Spoollink.to_unix_mode(), 0o160000);
        assert_eq!(
            FileMode::from_byte(FileMode::Spoollink.to_byte()),
            Some(FileMode::Spoollink)
        );
        assert_eq!(
            EntryType::from_byte(EntryType::Spoollink.to_byte()),
            Some(EntryType::Spoollink)
        );
    }
}

#[cfg(test)]
#[path = "tree_v4_tests.rs"]
mod tree_v4_tests;

#[cfg(test)]
mod cow_tests {
    use super::*;

    fn fixture() -> Tree {
        Tree::from_entries(vec![
            TreeEntry::file("a", ContentHash::compute(b"a"), false).unwrap(),
            TreeEntry::file("b", ContentHash::compute(b"b"), true).unwrap(),
        ])
    }

    #[test]
    fn clone_shares_entries_until_mutated() {
        let original = fixture();
        let mut clone = original.clone();
        assert!(Arc::ptr_eq(&original.entries, &clone.entries));

        clone.insert(TreeEntry::file("c", ContentHash::compute(b"c"), false).unwrap());

        assert!(!Arc::ptr_eq(&original.entries, &clone.entries));
        assert!(original.get("c").is_none());
        assert!(clone.get("c").is_some());
    }

    #[test]
    fn clone_mutation_preserves_original_hash_and_encoding() {
        let original = fixture();
        let original_hash = original.hash();
        let original_bytes = rmp_serde::to_vec_named(&original).unwrap();
        let mut clone = original.clone();

        assert!(clone.remove("a").is_some());

        assert_eq!(original.hash(), original_hash);
        assert_eq!(rmp_serde::to_vec_named(&original).unwrap(), original_bytes);
        assert_ne!(clone.hash(), original_hash);
    }

    #[test]
    fn clone_and_mutate_roundtrips_through_durable_encoding() {
        let mut tree = fixture().clone();
        tree.insert(TreeEntry::directory("dir", ContentHash::compute(b"dir")).unwrap());
        let encoded = rmp_serde::to_vec_named(&tree).unwrap();
        let decoded: Tree = rmp_serde::from_slice(&encoded).unwrap();

        assert_eq!(decoded, tree);
        assert_eq!(decoded.hash(), tree.hash());
    }
}
