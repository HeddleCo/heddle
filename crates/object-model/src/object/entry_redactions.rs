// SPDX-License-Identifier: Apache-2.0
//! Shared, name-free per-entry redaction for an already admitted state.
//!
//! Only explicit overrides contribute leaves. Unioning ancestor overrides
//! carries a restriction while the salted entry commitment remains unchanged;
//! a changed entry has a fresh salt and does not inherit content taint.

use std::collections::HashSet;

use super::{ContentHash, EntryVisibilityEntry, Tree, TreeScheme, VisibilityTier};

/// Explicit salted entry commitments hidden from one admitted reader.
///
/// This is not a state authorization proof. Callers must first admit the
/// governing Thread and apply the whole-tip and own-tier gate. Directory
/// descent, hash reachability, listings and diffs then use the same predicate.
#[derive(Clone, Debug, Default)]
pub struct EntryRedactions {
    leaves: HashSet<ContentHash>,
}

impl EntryRedactions {
    /// Fold explicit overrides whose tier the caller's authorized audience
    /// cannot read. For ancestor carry-forward, evaluate the override tier
    /// alone: the served state's baseline is checked at the whole-tip gate.
    pub fn extend_overrides(
        &mut self,
        entries: &[EntryVisibilityEntry],
        can_read: impl Fn(&VisibilityTier) -> bool,
    ) {
        self.leaves.extend(
            entries
                .iter()
                .filter(|entry| !can_read(&entry.tier))
                .map(|entry| entry.leaf_hash),
        );
    }

    /// Union restrictions from another endpoint, such as the base of a diff.
    pub fn extend(&mut self, other: &Self) {
        self.leaves.extend(other.leaves.iter().copied());
    }

    /// Opaque commitments used by [`super::PartialTree::project`].
    pub fn leaves(&self) -> &HashSet<ContentHash> {
        &self.leaves
    }

    /// Whether no explicit override is hidden for this reader.
    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    /// Whether one occurrence is visible. A denied directory must not be
    /// descended into, even to resolve a caller-supplied path or object hash.
    /// Invalid indices and malformed salted leaves fail closed.
    pub fn entry_visible(&self, tree: &Tree, index: usize) -> bool {
        if index >= tree.entries().len() {
            return false;
        }
        match tree.scheme() {
            TreeScheme::V3Flat => true,
            TreeScheme::V4Salted => tree
                .v4_leaf_hash_at(index)
                .is_some_and(|leaf| !self.leaves.contains(&leaf)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{AudienceTier, PartialTree, PartialTreeLeaf, TreeEntry, visible};

    #[test]
    fn explicit_leaf_carries_across_tree_roots_but_not_a_fresh_salt() {
        let file = TreeEntry::file("secret.txt", ContentHash::compute(b"secret"), false)
            .expect("valid file");
        let parent =
            Tree::from_entries_salted_v4(vec![file.clone()], vec![[1; 32]]).expect("salted parent");
        let inherited = Tree::from_entries_salted_v4(
            vec![
                file.clone(),
                TreeEntry::file("visible.txt", ContentHash::compute(b"ok"), false)
                    .expect("valid sibling"),
            ],
            vec![[1; 32], [2; 32]],
        )
        .expect("salted descendant");
        let changed =
            Tree::from_entries_salted_v4(vec![file], vec![[3; 32]]).expect("fresh commitment");
        let overrides = [EntryVisibilityEntry {
            tree_id: parent.hash(),
            leaf_hash: parent.v4_leaf_hash_at(0).expect("parent leaf"),
            tier: VisibilityTier::Private {
                scope_label: "security".into(),
            },
        }];
        let mut redacted = EntryRedactions::default();
        redacted.extend_overrides(&overrides, |tier| visible(tier, &AudienceTier::Internal));
        assert_ne!(parent.hash(), inherited.hash());
        assert!(!redacted.entry_visible(&parent, 0));
        assert!(
            !redacted.entry_visible(&inherited, 0),
            "unchanged leaf retains override"
        );
        assert!(redacted.entry_visible(&inherited, 1));
        assert!(
            redacted.entry_visible(&changed, 0),
            "fresh salt does not inherit taint"
        );
        assert!(
            !redacted.entry_visible(&changed, 1),
            "invalid index is not visible"
        );
        let partial = PartialTree::project(&inherited, redacted.leaves()).expect("projection");
        partial
            .verify()
            .expect("original Merkle root remains provable");
        assert_eq!(partial.declared_root(), inherited.hash());
        assert!(
            partial
                .leaves()
                .iter()
                .any(|leaf| matches!(leaf, PartialTreeLeaf::Redacted { .. }))
        );
        assert!(!partial.leaves().iter().any(|leaf| matches!(leaf,
            PartialTreeLeaf::Visible { entry, .. } if entry.name() == "secret.txt")));

        let mut labelled = EntryRedactions::default();
        labelled.extend_overrides(&overrides, |tier| {
            visible(tier, &AudienceTier::Restricted("security".into()))
        });
        assert!(labelled.entry_visible(&inherited, 0));
    }
}
