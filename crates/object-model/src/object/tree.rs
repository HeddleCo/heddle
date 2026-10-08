// SPDX-License-Identifier: Apache-2.0
//! Tree types: entries, structure, and supporting enums.

use std::{fmt, path::Path, sync::Arc};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sley_core::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use super::{
    ContentHash, SpoolId, StateId,
    tree_git_layout::{RawGitMode, git_canonical_order},
};

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

    /// Emit the canonical `mode ‖ entry_type ‖ target_payload` byte sequence.
    /// `layout_flags` are the Git-layout trailer flags of the owning entry
    /// (zero for every entry without one, which keeps its bytes unchanged).
    ///
    /// This is the single source of truth for both the V3 flat hash
    /// ([`TreeEntry::update_hasher`]) and the V4 leaf preimage
    /// ([`Tree::v4_leaf_preimage`]), so the two encodings can never drift.
    fn write_payload(&self, layout_flags: u8, mut emit: impl FnMut(&[u8])) {
        emit(&[self.mode().to_byte() | layout_flags]);
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
    // The source Git mode, recorded only when it differs from the mode Git
    // writes for `target` (heddle#2018). Checkout and diff ignore it.
    git_mode: Option<RawGitMode>,
}

impl TreeEntry {
    pub(crate) fn validate(&self) -> Result<(), TreeError> {
        validate_name(&self.name)?;
        if let Some(mode) = self.git_mode {
            self.check_raw_git_mode(mode)?;
        }
        Ok(())
    }

    fn new(name: String, target: TreeEntryTarget) -> Self {
        Self {
            name,
            target,
            git_mode: None,
        }
    }

    /// Record the mode this entry had in its source Git tree. A mode Git would
    /// write anyway is not recorded, so a canonical entry stays byte-identical
    /// to one built without a source mode. Errors when `mode` does not read as
    /// this entry's kind and executable bit.
    pub fn with_raw_git_mode(self, mode: RawGitMode) -> Result<Self, TreeError> {
        let canonical = RawGitMode::canonical(self.entry_type(), self.is_executable());
        if canonical == Some(mode) {
            return Ok(Self {
                git_mode: None,
                ..self
            });
        }
        self.check_raw_git_mode(mode)?;
        Ok(self.with_checked_raw_git_mode(mode))
    }

    pub(crate) fn with_checked_raw_git_mode(self, mode: RawGitMode) -> Self {
        Self {
            git_mode: Some(mode),
            ..self
        }
    }

    /// The recorded source Git mode, present only when it is not canonical.
    pub fn raw_git_mode(&self) -> Option<RawGitMode> {
        self.git_mode
    }

    /// The mode to write for this entry in a Git tree: the recorded source
    /// mode, else the canonical one. `None` for a spoollink.
    pub fn git_mode(&self) -> Option<RawGitMode> {
        self.git_mode
            .or_else(|| RawGitMode::canonical(self.entry_type(), self.is_executable()))
    }

    pub fn file(
        name: impl Into<String>,
        hash: ContentHash,
        executable: bool,
    ) -> Result<Self, TreeError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self::new(name, TreeEntryTarget::Blob { hash, executable }))
    }

    pub fn directory(name: impl Into<String>, hash: ContentHash) -> Result<Self, TreeError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self::new(name, TreeEntryTarget::Tree { hash }))
    }

    pub fn symlink(name: impl Into<String>, hash: ContentHash) -> Result<Self, TreeError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self::new(name, TreeEntryTarget::Symlink { hash }))
    }

    pub fn gitlink(name: impl Into<String>, target: GitObjectId) -> Result<Self, TreeError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self::new(name, TreeEntryTarget::Gitlink { target }))
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
        Ok(Self::new(
            name,
            TreeEntryTarget::Spoollink { spool_id, state_id },
        ))
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

    /// Length of this entry's hash preimage. `source_position` is the entry's
    /// position in its tree's recorded Git source order, if any.
    pub(crate) fn encoded_len(&self, source_position: Option<u32>) -> usize {
        let flags = self.layout_flags(source_position);
        1 + 1
            + self.target.encoded_payload_len()
            + self.name.len()
            + 1
            + super::tree_git_layout::layout_trailer_len(flags)
    }

    /// Owned name-plus-target bytes used by streaming page budgets.
    pub fn decoded_size(&self) -> usize {
        self.name.len() + self.target.encoded_payload_len()
    }

    /// Feed this entry's V3 preimage, `mode|flags ‖ type ‖ payload ‖ name ‖
    /// NUL ‖ layout trailer`. Without a layout the flags are zero and the
    /// trailer is empty, which is the historical preimage.
    pub(crate) fn update_hasher(&self, hasher: &mut blake3::Hasher, source_position: Option<u32>) {
        let flags = self.layout_flags(source_position);
        self.target.write_payload(flags, |bytes| {
            hasher.update(bytes);
        });
        hasher.update(self.name.as_bytes());
        hasher.update(&[0]);
        self.write_layout_trailer(source_position, |bytes| {
            hasher.update(bytes);
        });
    }
}

// ── Tree ────────────────────────────────────────────────────────────

/// A complete tree with its encoding scheme and per-entry salts kept together.
/// Use [`Self::from_entries_salted_v4`] to supply explicit salts for a new tree;
/// mutations through [`Self::insert`] maintain the selected scheme.
///
/// Explicit salt mutation is internal, so it cannot corrupt a flat tree:
///
/// ```compile_fail,E0624
/// use heddle_object_model::object::{ContentHash, Tree, TreeEntry};
/// let mut tree = Tree::new();
/// if let Ok(entry) = TreeEntry::file("readme", ContentHash::compute(b"text"), false) {
///     tree.insert_salted(entry, [7; 32]);
/// }
/// ```
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
    // Each entry's position in its source Git tree, parallel to `entries`.
    // Empty unless the tree was imported from a Git tree whose entries were
    // not in Git's canonical order (heddle#2018); then it is a permutation of
    // `0..entries.len()` that differs from Git's order. V3 only. Any mutation
    // drops it: an edited tree has no source to reproduce.
    source_positions: Arc<Vec<u32>>,
}

impl Tree {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Vec::new()),
            scheme: TreeScheme::V3Flat,
            salts: Arc::new(Vec::new()),
            source_positions: Arc::new(Vec::new()),
        }
    }

    pub fn from_entries(mut entries: Vec<TreeEntry>) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            entries: Arc::new(entries),
            scheme: TreeScheme::V3Flat,
            salts: Arc::new(Vec::new()),
            source_positions: Arc::new(Vec::new()),
        }
    }

    /// Build a tree from the entries of a Git tree, in the Git tree's own
    /// order. The source order is recorded only when it is not Git's
    /// canonical order. Entries carry their own raw modes
    /// ([`TreeEntry::with_raw_git_mode`]).
    ///
    /// A Git tree with two entries of the same name is not representable and
    /// is rejected, naming the entry.
    pub fn from_git_entries(entries: Vec<TreeEntry>) -> Result<Self, TreeError> {
        let canonical = entries
            .windows(2)
            .all(|pair| git_canonical_order(&pair[0], &pair[1]) == std::cmp::Ordering::Less);
        let mut paired: Vec<(TreeEntry, u32)> = Vec::with_capacity(entries.len());
        for (position, entry) in entries.into_iter().enumerate() {
            let position = u32::try_from(position).map_err(|_| {
                TreeError::InvalidStructure("git tree has more than u32::MAX entries".into())
            })?;
            paired.push((entry, position));
        }
        paired.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        if let Some(pair) = paired
            .windows(2)
            .find(|pair| pair[0].0.name == pair[1].0.name)
        {
            return Err(TreeError::InvalidStructure(format!(
                "duplicate entry name '{}'",
                pair[0].0.name
            )));
        }
        let (entries, positions): (Vec<TreeEntry>, Vec<u32>) = paired.into_iter().unzip();
        let tree = Self {
            entries: Arc::new(entries),
            scheme: TreeScheme::V3Flat,
            salts: Arc::new(Vec::new()),
            source_positions: Arc::new(if canonical { Vec::new() } else { positions }),
        };
        tree.validate()?;
        Ok(tree)
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
        Self::try_from_decoded_layout(entries, Vec::new())
    }

    /// [`Self::try_from_decoded_entries`] for a body that carries per-entry
    /// source positions (empty when it carries none).
    pub(crate) fn try_from_decoded_layout(
        entries: Vec<TreeEntry>,
        source_positions: Vec<u32>,
    ) -> Result<Self, TreeError> {
        let tree = Self {
            entries: Arc::new(entries),
            scheme: TreeScheme::V3Flat,
            salts: Arc::new(Vec::new()),
            source_positions: Arc::new(source_positions),
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
            source_positions: Arc::new(Vec::new()),
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

    /// The source Git position of each entry, parallel to [`Self::entries`].
    /// Empty unless the source tree was not in Git's canonical order.
    pub fn source_positions(&self) -> &[u32] {
        &self.source_positions
    }

    /// The source position of the entry at `index`, if the tree records one.
    pub fn source_position_at(&self, index: usize) -> Option<u32> {
        self.source_positions.get(index).copied()
    }

    /// Whether this tree records any part of a non-canonical Git source
    /// layout: a raw mode on an entry, or the source entry order.
    pub fn has_git_layout(&self) -> bool {
        !self.source_positions.is_empty()
            || self.entries.iter().any(|entry| entry.git_mode.is_some())
    }

    /// Whether this tree can only be stored as a full, self-keyed canonical
    /// body (HTR4 / HSR1). The lean, delta, and packed columnar forms carry
    /// neither salts nor the Git layout.
    pub fn requires_canonical_body(&self) -> bool {
        self.scheme == TreeScheme::V4Salted || self.has_git_layout()
    }

    /// The entries in the order a Git tree lists them: the recorded source
    /// order when there is one, else Git's canonical order.
    pub fn git_ordered_entries(&self) -> Vec<&TreeEntry> {
        let mut ordered: Vec<(usize, &TreeEntry)> = self.entries.iter().enumerate().collect();
        if self.source_positions.is_empty() {
            ordered.sort_by(|a, b| git_canonical_order(a.1, b.1));
        } else {
            ordered.sort_by_key(|(index, _)| self.source_positions.get(*index).copied());
        }
        ordered.into_iter().map(|(_, entry)| entry).collect()
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
        self.validate_source_positions()
    }

    /// Source positions are absent, or a permutation of the entries that
    /// differs from Git's canonical order. Recording the canonical order would
    /// give one Git tree two native ids.
    fn validate_source_positions(&self) -> Result<(), TreeError> {
        if self.source_positions.is_empty() {
            return Ok(());
        }
        if self.scheme != TreeScheme::V3Flat {
            return Err(TreeError::InvalidStructure(
                "only v3 trees record a git source order".into(),
            ));
        }
        if self.source_positions.len() != self.entries.len() {
            return Err(TreeError::InvalidStructure(format!(
                "tree has {} entries but {} source positions",
                self.entries.len(),
                self.source_positions.len()
            )));
        }
        let mut seen = vec![false; self.entries.len()];
        for position in self.source_positions.iter() {
            let slot = usize::try_from(*position)
                .ok()
                .and_then(|index| seen.get_mut(index))
                .ok_or_else(|| {
                    TreeError::InvalidStructure(format!(
                        "source position {position} is out of range"
                    ))
                })?;
            if *slot {
                return Err(TreeError::InvalidStructure(format!(
                    "source position {position} is repeated"
                )));
            }
            *slot = true;
        }
        let ordered = self.git_ordered_entries();
        if ordered
            .windows(2)
            .all(|pair| git_canonical_order(pair[0], pair[1]) == std::cmp::Ordering::Less)
        {
            return Err(TreeError::InvalidStructure(
                "recorded git source order is the canonical order".into(),
            ));
        }
        Ok(())
    }

    /// An edited tree has no source tree to reproduce, so it takes Git's
    /// canonical order. Raw modes stay with their entries.
    fn drop_source_order(&mut self) {
        if !self.source_positions.is_empty() {
            self.source_positions = Arc::new(Vec::new());
        }
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
        self.drop_source_order();
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
    fn insert_salted(&mut self, entry: TreeEntry, salt: [u8; 32]) {
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
        self.drop_source_order();
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
        let total_len: usize = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| entry.encoded_len(self.source_position_at(index)))
            .sum();
        ContentHash::compute_typed_with_len(TREE_EMPTY_PREFIX, total_len as u64, |hasher| {
            for (index, entry) in self.entries.iter().enumerate() {
                entry.update_hasher(hasher, self.source_position_at(index));
            }
        })
    }

    /// The V4 salted per-entry leaf commitment for `entries[index]`.
    ///
    /// `leaf = typed_hasher("tree-v4-leaf", len)(salt ‖ mode ‖ entry_type ‖
    /// target_payload ‖ name_len(u16 LE) ‖ name ‖ layout trailer)`, where the
    /// `mode ‖ entry_type ‖ target_payload` bytes are exactly those
    /// [`TreeEntryTarget::write_payload`] emits. V4 trees record no source
    /// order, so the trailer is only ever an entry's raw Git mode.
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
            .write_payload(entry.layout_flags(None), |bytes| {
                buf.extend_from_slice(bytes)
            });
        // Names are bounded to u16::MAX by `validate_name`.
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(name);
        entry.write_layout_trailer(None, |bytes| buf.extend_from_slice(bytes));
        buf
    }

    /// The salted per-entry leaf commitment for the entry at `index`, or `None`
    /// for a V3 tree / out-of-range index. This is the name-free handle a
    /// redacted serve projection is keyed by; capture-time entry-visibility
    /// authoring resolves a path to its enclosing tree + this leaf hash.
    pub fn v4_leaf_hash_at(&self, index: usize) -> Option<ContentHash> {
        if self.scheme != TreeScheme::V4Salted {
            return None;
        }
        let entry = self.entries.get(index)?;
        let salt = self.salts.get(index)?;
        Some(Self::v4_leaf_hash(entry, salt))
    }

    /// The salted leaf commitment for the entry named `name`, or `None` if the
    /// name is absent or this is a V3 tree.
    pub fn v4_leaf_hash_for(&self, name: &str) -> Option<ContentHash> {
        let index = self
            .entries
            .binary_search_by(|entry| entry.name.as_str().cmp(name))
            .ok()?;
        self.v4_leaf_hash_at(index)
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

    /// Build a V4 [`Tree`] from ONLY the visible entries of this projection,
    /// dropping the withheld (redacted) leaves entirely. Unlike
    /// [`PartialTree::into_tree`], this never errors on redacted leaves — it
    /// omits them. The result's hash therefore does NOT equal the declared
    /// root (it has fewer entries); it is the "visible set" view for status
    /// comparison, where the withheld entries are unknown to this client by
    /// construction and so must not be reported as local deletions.
    pub fn visible_tree(&self) -> Result<Tree, TreeError> {
        let mut entries = Vec::new();
        let mut salts = Vec::new();
        for leaf in &self.leaves {
            if let PartialTreeLeaf::Visible { entry, salt } = leaf {
                entries.push(entry.clone());
                salts.push(*salt);
            }
        }
        Tree::from_entries_salted_v4(entries, salts)
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
    // Parallel per-entry Git source positions (heddle#2018). Omitted unless the
    // tree records a non-canonical source order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_positions: Option<Vec<u32>>,
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
    // Source Git mode digits (heddle#2018), only when not canonical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    git_mode: Option<String>,
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
            source_positions: (!tree.source_positions.is_empty())
                .then(|| tree.source_positions.as_ref().clone()),
        }
    }
}

impl From<&TreeEntry> for EncodedTreeEntryV2 {
    fn from(entry: &TreeEntry) -> Self {
        let name = entry.name.clone();
        let git_mode = entry.git_mode.map(|mode| {
            let mut digits = Vec::new();
            mode.write_digits(&mut digits);
            String::from_utf8_lossy(&digits).into_owned()
        });
        let empty = Self {
            name,
            kind: ENTRY_KIND_BLOB,
            hash: None,
            executable: None,
            git_format: None,
            git_oid: None,
            spool_id: None,
            spool_state_id: None,
            git_mode,
        };
        match entry.target() {
            TreeEntryTarget::Blob { hash, executable } => Self {
                kind: ENTRY_KIND_BLOB,
                hash: Some(*hash),
                executable: Some(*executable),
                ..empty
            },
            TreeEntryTarget::Tree { hash } => Self {
                kind: ENTRY_KIND_TREE,
                hash: Some(*hash),
                ..empty
            },
            TreeEntryTarget::Symlink { hash } => Self {
                kind: ENTRY_KIND_SYMLINK,
                hash: Some(*hash),
                ..empty
            },
            TreeEntryTarget::Gitlink { target } => Self {
                kind: ENTRY_KIND_GITLINK,
                git_format: Some(git_format_to_tag(target.format())),
                git_oid: Some(target.as_bytes().to_vec()),
                ..empty
            },
            TreeEntryTarget::Spoollink { spool_id, state_id } => Self {
                kind: ENTRY_KIND_SPOOLLINK,
                spool_id: Some(spool_id.clone()),
                spool_state_id: Some(*state_id),
                ..empty
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
        let source_positions = encoded.source_positions.unwrap_or_default();
        match encoded.version {
            TREE_FORMAT_VERSION => {
                if encoded.salts.is_some_and(|salts| !salts.is_empty()) {
                    return Err(TreeError::InvalidStructure(
                        "v3 tree body must not carry salts".into(),
                    ));
                }
                Tree::try_from_decoded_layout(entries, source_positions)
            }
            TREE_FORMAT_VERSION_V4 => {
                if !source_positions.is_empty() {
                    return Err(TreeError::InvalidStructure(
                        "v4 tree body must not carry source positions".into(),
                    ));
                }
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
        let git_mode = encoded
            .git_mode
            .as_deref()
            .map(|digits| RawGitMode::parse(digits.as_bytes()))
            .transpose()?;
        let entry = Self::try_from_encoded_target(encoded)?;
        match git_mode {
            Some(mode) => {
                entry.check_raw_git_mode(mode)?;
                Ok(entry.with_checked_raw_git_mode(mode))
            }
            None => Ok(entry),
        }
    }
}

impl TreeEntry {
    fn try_from_encoded_target(encoded: EncodedTreeEntryV2) -> Result<Self, TreeError> {
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

#[cfg(test)]
#[path = "tree_golden_tests.rs"]
mod tree_golden_tests;
