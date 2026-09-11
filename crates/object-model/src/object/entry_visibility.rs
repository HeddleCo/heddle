// SPDX-License-Identifier: Apache-2.0
//! EntryVisibility — a per-entry visibility sidecar for v4 salted trees.
//!
//! Where [`StateVisibility`](crate::object::StateVisibility) declares one tier
//! for a whole commit, `EntryVisibility` declares tiers for individual tree
//! *entries* so a v4 (salted-Merkle) tree can be served with some entries
//! redacted to opaque leaf hashes (design v4-redactable-tree §8; Fable C2).
//!
//! **Keyed by the state's [`ChangeId`].** One sidecar covers a state's nested
//! trees; each record names the enclosing `tree_id`, the entry's `leaf_hash`
//! (the salted per-entry commitment — the only stable, name-free handle for a
//! redacted entry), and the [`VisibilityTier`] the entry is served at. Leaf
//! hashes are stable across states that share a subtree (sticky salts), so a
//! record keyed by leaf hash survives rebasing/lineage the way a `ChangeId`
//! does.
//!
//! This is the heddle-side *produce + stage* type. The weft-side
//! accept/persist/serve seam (composing the state baseline with these
//! downward-only overrides) is a later leg; here the sidecar is only built at
//! capture and staged in the snapshot's oplog batch.

use serde::{Deserialize, Serialize};

use crate::object::{ChangeId, ContentHash, VisibilityTier};

/// Current on-disk format version for an [`EntryVisibility`] blob.
pub const ENTRY_VISIBILITY_FORMAT_VERSION: u8 = 1;

/// One per-entry visibility override within a state's trees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryVisibilityEntry {
    /// The id of the (v4 salted) tree that directly contains the entry.
    pub tree_id: ContentHash,
    /// The entry's salted per-entry leaf commitment — the name-free handle a
    /// redacted serve projection is keyed by.
    pub leaf_hash: ContentHash,
    /// The tier this entry is served at. Composed downward-only with the
    /// state baseline at serve time (a later leg).
    pub tier: VisibilityTier,
}

/// The per-state entry-visibility sidecar: the set of per-entry tier overrides
/// covering the trees reachable from one state, keyed by that state's
/// [`ChangeId`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryVisibility {
    /// Format version; [`Self::decode`] rejects anything but the current one.
    pub format_version: u8,
    /// The state (by rewrite-stable change id) these overrides apply to.
    pub change_id: ChangeId,
    /// The state's root tree id — the anchor the entry records hang beneath.
    pub tree_root: ContentHash,
    /// The per-entry overrides. Order is normalized (by `(tree_id, leaf_hash)`)
    /// so the encoded bytes are canonical for a given override set.
    pub entries: Vec<EntryVisibilityEntry>,
}

impl EntryVisibility {
    /// Build a sidecar from its overrides, normalizing entry order so equal
    /// override sets encode to identical bytes (and hash identically).
    pub fn new(
        change_id: ChangeId,
        tree_root: ContentHash,
        mut entries: Vec<EntryVisibilityEntry>,
    ) -> Self {
        entries.sort_by(|a, b| {
            a.tree_id
                .as_bytes()
                .cmp(b.tree_id.as_bytes())
                .then_with(|| a.leaf_hash.as_bytes().cmp(b.leaf_hash.as_bytes()))
        });
        Self {
            format_version: ENTRY_VISIBILITY_FORMAT_VERSION,
            change_id,
            tree_root,
            entries,
        }
    }

    /// `true` iff this sidecar carries at least one override. An empty sidecar
    /// is never persisted (absence ≡ "every entry at the state baseline").
    pub fn has_records(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Encode to canonical msgpack bytes.
    pub fn encode(&self) -> Result<Vec<u8>, EntryVisibilityError> {
        rmp_serde::to_vec_named(self).map_err(|e| EntryVisibilityError::Codec(e.to_string()))
    }

    /// Decode msgpack bytes, rejecting an unsupported format version.
    pub fn decode(bytes: &[u8]) -> Result<Self, EntryVisibilityError> {
        let value: Self =
            rmp_serde::from_slice(bytes).map_err(|e| EntryVisibilityError::Codec(e.to_string()))?;
        if value.format_version != ENTRY_VISIBILITY_FORMAT_VERSION {
            return Err(EntryVisibilityError::UnsupportedVersion(
                value.format_version,
            ));
        }
        Ok(value)
    }

    /// Content-addressed id of this sidecar — `blake3` over its canonical
    /// encoded bytes. Named in the oplog record so undo/redo can correlate.
    pub fn content_hash(&self) -> Result<ContentHash, EntryVisibilityError> {
        let bytes = self.encode()?;
        Ok(ContentHash::from_bytes(*blake3::hash(&bytes).as_bytes()))
    }
}

/// Errors produced while encoding/decoding an [`EntryVisibility`] sidecar.
#[derive(Debug, thiserror::Error)]
pub enum EntryVisibilityError {
    #[error("unsupported entry-visibility format version {0}")]
    UnsupportedVersion(u8),
    #[error("entry-visibility codec error: {0}")]
    Codec(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: &str) -> ContentHash {
        ContentHash::compute_typed("test", seed.as_bytes())
    }

    #[test]
    fn round_trips_and_normalizes_order() {
        let change = ChangeId::generate();
        let root = hash("root");
        let unordered = vec![
            EntryVisibilityEntry {
                tree_id: hash("t2"),
                leaf_hash: hash("l2"),
                tier: VisibilityTier::Internal,
            },
            EntryVisibilityEntry {
                tree_id: hash("t1"),
                leaf_hash: hash("l1"),
                tier: VisibilityTier::Private {
                    scope_label: "secret".into(),
                },
            },
        ];
        let a = EntryVisibility::new(change, root, unordered.clone());
        let mut reversed = unordered;
        reversed.reverse();
        let b = EntryVisibility::new(change, root, reversed);
        assert_eq!(
            a.encode().unwrap(),
            b.encode().unwrap(),
            "order must normalize"
        );

        let decoded = EntryVisibility::decode(&a.encode().unwrap()).unwrap();
        assert_eq!(decoded, a);
        assert_eq!(decoded.content_hash().unwrap(), a.content_hash().unwrap());
    }

    #[test]
    fn rejects_unsupported_version() {
        let change = ChangeId::generate();
        let mut sidecar = EntryVisibility::new(change, hash("root"), Vec::new());
        sidecar.format_version = 99;
        let bytes = rmp_serde::to_vec_named(&sidecar).unwrap();
        assert!(matches!(
            EntryVisibility::decode(&bytes),
            Err(EntryVisibilityError::UnsupportedVersion(99))
        ));
    }
}
