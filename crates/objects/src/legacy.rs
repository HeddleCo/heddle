// SPDX-License-Identifier: Apache-2.0
//! Migration-only decoders for removed durable formats.

use serde::{Deserialize, Serialize};
use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use crate::{
    error::{HeddleError, Result},
    object::{ContentHash, Tree, TreeEntry},
};

const LEGACY_GITLINK_BLOB_PREFIX: &str = "heddle-submodule:";

pub fn decode_gitlink_blob_marker(content: &[u8]) -> Option<GitObjectId> {
    let text = std::str::from_utf8(content).ok()?.trim();
    let oid = text.strip_prefix(LEGACY_GITLINK_BLOB_PREFIX)?.trim();
    GitObjectId::from_hex(GitObjectFormat::Sha1, oid).ok()
}

/// Decode the removed V1 tree schema.
///
/// This intentionally lives outside `Tree`'s `Deserialize` impl: normal
/// runtime readers accept only the current versioned tree envelope, while
/// migrations may call this one-shot decoder and immediately rewrite a V2 tree
/// body at the same semantic tree hash.
pub fn decode_legacy_tree_v1(data: &[u8]) -> Result<Tree> {
    let legacy: LegacyTreeV1 = rmp_serde::from_slice(data)?;
    legacy.into_tree()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyTreeV1 {
    entries: Vec<LegacyTreeEntryV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyTreeEntryV1 {
    name: String,
    mode: LegacyFileMode,
    entry_type: LegacyEntryType,
    hash: ContentHash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum LegacyFileMode {
    Normal,
    Executable,
    Symlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum LegacyEntryType {
    Blob,
    Tree,
    Symlink,
}

impl LegacyTreeV1 {
    fn into_tree(self) -> Result<Tree> {
        let mut entries = Vec::with_capacity(self.entries.len());
        for entry in self.entries {
            entries.push(entry.into_tree_entry()?);
        }
        let tree = Tree::from_entries(entries);
        tree.validate()?;
        Ok(tree)
    }
}

impl LegacyTreeEntryV1 {
    fn into_tree_entry(self) -> Result<TreeEntry> {
        match (self.entry_type, self.mode) {
            (LegacyEntryType::Blob, LegacyFileMode::Normal) => {
                Ok(TreeEntry::file(self.name, self.hash, false)?)
            }
            (LegacyEntryType::Blob, LegacyFileMode::Executable) => {
                Ok(TreeEntry::file(self.name, self.hash, true)?)
            }
            (LegacyEntryType::Tree, LegacyFileMode::Normal) => {
                Ok(TreeEntry::directory(self.name, self.hash)?)
            }
            (LegacyEntryType::Symlink, LegacyFileMode::Symlink) => {
                Ok(TreeEntry::symlink(self.name, self.hash)?)
            }
            (entry_type, mode) => Err(HeddleError::InvalidObject(format!(
                "invalid legacy tree entry '{}': {entry_type:?} cannot use mode {mode:?}",
                self.name
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_legacy_gitlink_blob_marker() {
        let oid = decode_gitlink_blob_marker(
            b"heddle-submodule: 0808080808080808080808080808080808080808",
        )
        .expect("legacy marker decodes");

        assert_eq!(oid.to_string(), "0808080808080808080808080808080808080808");
    }

    #[test]
    fn ignores_ordinary_blob_content() {
        assert!(decode_gitlink_blob_marker(b"not a gitlink").is_none());
    }

    #[test]
    fn decodes_legacy_tree_v1_without_sniffing_marker_blobs() {
        let blob_hash =
            ContentHash::compute(b"heddle-submodule: 0808080808080808080808080808080808080808");
        let raw = rmp_serde::to_vec(&LegacyTreeV1 {
            entries: vec![LegacyTreeEntryV1 {
                name: "vendor".to_string(),
                mode: LegacyFileMode::Normal,
                entry_type: LegacyEntryType::Blob,
                hash: blob_hash,
            }],
        })
        .expect("legacy tree serializes");

        let decoded = decode_legacy_tree_v1(&raw).expect("legacy tree decodes");
        let entry = decoded.get("vendor").expect("entry exists");

        assert!(entry.is_blob());
        assert_eq!(entry.blob_hash(), Some(blob_hash));
        assert!(entry.gitlink_target().is_none());
    }
}
