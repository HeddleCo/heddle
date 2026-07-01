// SPDX-License-Identifier: Apache-2.0
//! Tree types: entries, structure, and supporting enums.

use std::{fmt, path::Path};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use super::ContentHash;

const TREE_FORMAT_VERSION: u8 = 2;
const ENTRY_KIND_BLOB: u8 = 0;
const ENTRY_KIND_TREE: u8 = 1;
const ENTRY_KIND_SYMLINK: u8 = 2;
const ENTRY_KIND_GITLINK: u8 = 3;
const GIT_OBJECT_FORMAT_SHA1: u8 = 1;
const GIT_OBJECT_FORMAT_SHA256: u8 = 2;

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
}

impl FileMode {
    pub fn to_byte(&self) -> u8 {
        match self {
            FileMode::Normal => 0,
            FileMode::Executable => 1,
            FileMode::Symlink => 2,
            FileMode::Gitlink => 3,
        }
    }

    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(FileMode::Normal),
            1 => Some(FileMode::Executable),
            2 => Some(FileMode::Symlink),
            3 => Some(FileMode::Gitlink),
            _ => None,
        }
    }

    pub fn to_unix_mode(&self) -> u32 {
        match self {
            FileMode::Normal => 0o100644,
            FileMode::Executable => 0o100755,
            FileMode::Symlink => 0o120000,
            FileMode::Gitlink => 0o160000,
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
}

impl EntryType {
    pub fn to_byte(&self) -> u8 {
        match self {
            EntryType::Blob => 0,
            EntryType::Tree => 1,
            EntryType::Symlink => 2,
            EntryType::Gitlink => 3,
        }
    }

    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(EntryType::Blob),
            1 => Some(EntryType::Tree),
            2 => Some(EntryType::Symlink),
            3 => Some(EntryType::Gitlink),
            _ => None,
        }
    }
}

// ── TreeEntryTarget ────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeEntryTarget {
    Blob { hash: ContentHash, executable: bool },
    Tree { hash: ContentHash },
    Symlink { hash: ContentHash },
    Gitlink { target: GitObjectId },
}

impl TreeEntryTarget {
    pub fn entry_type(&self) -> EntryType {
        match self {
            TreeEntryTarget::Blob { .. } => EntryType::Blob,
            TreeEntryTarget::Tree { .. } => EntryType::Tree,
            TreeEntryTarget::Symlink { .. } => EntryType::Symlink,
            TreeEntryTarget::Gitlink { .. } => EntryType::Gitlink,
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
        }
    }

    pub fn content_hash(&self) -> Option<ContentHash> {
        match self {
            TreeEntryTarget::Blob { hash, .. }
            | TreeEntryTarget::Tree { hash }
            | TreeEntryTarget::Symlink { hash } => Some(*hash),
            TreeEntryTarget::Gitlink { .. } => None,
        }
    }

    pub fn gitlink_target(&self) -> Option<GitObjectId> {
        match self {
            TreeEntryTarget::Gitlink { target } => Some(*target),
            _ => None,
        }
    }

    fn encoded_payload_len(&self) -> usize {
        match self {
            TreeEntryTarget::Blob { hash, .. }
            | TreeEntryTarget::Tree { hash }
            | TreeEntryTarget::Symlink { hash } => hash.as_bytes().len(),
            TreeEntryTarget::Gitlink { target } => target.as_bytes().len(),
        }
    }

    fn update_hasher(&self, hasher: &mut blake3::Hasher) {
        hasher.update(&[self.mode().to_byte()]);
        hasher.update(&[self.entry_type().to_byte()]);
        match self {
            TreeEntryTarget::Blob { hash, .. }
            | TreeEntryTarget::Tree { hash }
            | TreeEntryTarget::Symlink { hash } => hasher.update(hash.as_bytes()),
            TreeEntryTarget::Gitlink { target } => {
                hasher.update(&[git_format_to_tag(target.format())]);
                hasher.update(target.as_bytes())
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
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    name: String,
    target: TreeEntryTarget,
}

impl TreeEntry {
    #[cfg(test)]
    pub(crate) fn new_unchecked_for_tests(
        name: impl Into<String>,
        target: TreeEntryTarget,
    ) -> Self {
        Self {
            name: name.into(),
            target,
        }
    }

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
            TreeEntryTarget::Tree { .. } | TreeEntryTarget::Gitlink { .. } => None,
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

    pub fn is_executable(&self) -> bool {
        self.mode() == FileMode::Executable
    }

    pub(crate) fn encoded_len(&self) -> usize {
        1 + 1 + self.target.encoded_payload_len() + self.name.len() + 1
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
    entries: Vec<TreeEntry>,
}

impl Tree {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn from_entries(mut entries: Vec<TreeEntry>) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Self { entries }
    }

    #[cfg(test)]
    pub(crate) fn from_entries_unchecked_for_tests(entries: Vec<TreeEntry>) -> Self {
        Self { entries }
    }

    pub fn validate(&self) -> Result<(), TreeError> {
        let mut previous_name: Option<&str> = None;
        for entry in &self.entries {
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
        self.entries.retain(|e| e.name != entry.name);
        let pos = self
            .entries
            .iter()
            .position(|e| e.name > entry.name)
            .unwrap_or(self.entries.len());
        self.entries.insert(pos, entry);
    }

    pub fn remove(&mut self, name: &str) -> Option<TreeEntry> {
        let pos = self.entries.iter().position(|e| e.name == name)?;
        Some(self.entries.remove(pos))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn hash(&self) -> ContentHash {
        let total_len: usize = self.entries.iter().map(TreeEntry::encoded_len).sum();
        ContentHash::compute_typed_with_len("tree", total_len as u64, |hasher| {
            for entry in &self.entries {
                entry.update_hasher(hasher);
            }
        })
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

// ── Durable V2 tree encoding ───────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct EncodedTreeV2 {
    version: u8,
    entries: Vec<EncodedTreeEntryV2>,
}

#[derive(Serialize, Deserialize)]
struct EncodedTreeEntryV2 {
    name: String,
    kind: u8,
    hash: Option<ContentHash>,
    executable: Option<bool>,
    git_format: Option<u8>,
    git_oid: Option<Vec<u8>>,
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
pub(crate) enum TreeDecodeError {
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
        Self {
            version: TREE_FORMAT_VERSION,
            entries: tree.entries.iter().map(EncodedTreeEntryV2::from).collect(),
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
            },
            TreeEntryTarget::Tree { hash } => Self {
                name: entry.name.clone(),
                kind: ENTRY_KIND_TREE,
                hash: Some(*hash),
                executable: None,
                git_format: None,
                git_oid: None,
            },
            TreeEntryTarget::Symlink { hash } => Self {
                name: entry.name.clone(),
                kind: ENTRY_KIND_SYMLINK,
                hash: Some(*hash),
                executable: None,
                git_format: None,
                git_oid: None,
            },
            TreeEntryTarget::Gitlink { target } => Self {
                name: entry.name.clone(),
                kind: ENTRY_KIND_GITLINK,
                hash: None,
                executable: None,
                git_format: Some(git_format_to_tag(target.format())),
                git_oid: Some(target.as_bytes().to_vec()),
            },
        }
    }
}

impl TryFrom<EncodedTreeV2> for Tree {
    type Error = TreeError;

    fn try_from(encoded: EncodedTreeV2) -> Result<Self, Self::Error> {
        if encoded.version != TREE_FORMAT_VERSION {
            return Err(TreeError::InvalidStructure(format!(
                "unsupported tree format version {}; this binary writes {}",
                encoded.version, TREE_FORMAT_VERSION
            )));
        }
        let mut entries = Vec::with_capacity(encoded.entries.len());
        for entry in encoded.entries {
            entries.push(TreeEntry::try_from(entry)?);
        }
        let tree = Tree::from_entries(entries);
        tree.validate()?;
        Ok(tree)
    }
}

impl Tree {
    pub(crate) fn decode_current_msgpack(data: &[u8]) -> Result<Self, TreeDecodeError> {
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

fn git_format_to_tag(format: GitObjectFormat) -> u8 {
    match format {
        GitObjectFormat::Sha1 => GIT_OBJECT_FORMAT_SHA1,
        GitObjectFormat::Sha256 => GIT_OBJECT_FORMAT_SHA256,
    }
}

fn git_format_from_tag(tag: u8) -> Result<GitObjectFormat, TreeError> {
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
        self.entries.into_iter()
    }
}

impl<'a> IntoIterator for &'a Tree {
    type Item = &'a TreeEntry;
    type IntoIter = std::slice::Iter<'a, TreeEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}
