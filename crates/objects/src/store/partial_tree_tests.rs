// SPDX-License-Identifier: Apache-2.0
//! Partial (redacted) tree store-slot + read-model tests.
//!
//! Exercises the client-side partial-materialization store contract on both
//! backends: an HRT1 projection is held in a slot DISTINCT from the full-tree
//! object, a request for the full tree never returns the partial silently, the
//! projection verifies against its declared root via `reconstruct_root`, and a
//! full tree and a partial obey the monotone discipline (full supersedes
//! partial; a partial never overwrites a full).

use crate::{
    object::{ContentHash, PartialTree, Tree, TreeEntry, encode_redacted_projection},
    store::{FsStore, InMemoryStore, ObjectStore, PartialTreeWrite, TreeRead, codec},
};

fn ch(bytes: &[u8]) -> ContentHash {
    ContentHash::compute(bytes)
}

/// A V4 salted tree with one entry (`secret.md`) marked for redaction.
///
/// Returns `(full_tree, root_hash, hrt1_body, withheld_leaf_hash)`. The withheld
/// leaf hash is computed via the single-entry-tree trick: a one-leaf Merkle root
/// equals that leaf's hash.
fn fixture() -> (Tree, ContentHash, Vec<u8>, ContentHash) {
    let entries = vec![
        TreeEntry::file("readme.md", ch(b"readme"), false).unwrap(),
        TreeEntry::file("secret.md", ch(b"secret"), false).unwrap(),
        TreeEntry::directory("src", ch(b"src-tree")).unwrap(),
    ];
    let salts = vec![[0x11; 32], [0x22; 32], [0x33; 32]];
    let tree = Tree::from_entries_salted_v4(entries, salts).unwrap();
    let root = tree.hash();

    let withheld_leaf = Tree::from_entries_salted_v4(
        vec![TreeEntry::file("secret.md", ch(b"secret"), false).unwrap()],
        vec![[0x22; 32]],
    )
    .unwrap()
    .hash();

    let mut redact = std::collections::HashSet::new();
    redact.insert(withheld_leaf);
    let partial = PartialTree::project(&tree, &redact).unwrap();
    assert_eq!(partial.redacted_count(), 1);
    let body = encode_redacted_projection(&partial).unwrap();

    (tree, root, body, withheld_leaf)
}

/// The partial's visible + withheld leaf hashes reconstruct the canonical root
/// H, so a partial clone verifies against the tip's `State.tree` without the
/// withheld content. Binding to the wrong key is corruption.
#[test]
fn reconstruct_root_of_partial_equals_canonical_hash() {
    let (_full, root, body, _withheld) = fixture();
    let partial = codec::decode_partial_tree(&body, root).unwrap();
    assert_eq!(partial.reconstruct_root(), root);
    assert_eq!(partial.declared_root(), root);

    let wrong = ch(b"not-the-root");
    match codec::decode_partial_tree(&body, wrong) {
        Err(crate::store::HeddleError::Corruption { expected, found }) => {
            assert_eq!(expected, wrong);
            assert_eq!(found, root);
        }
        other => panic!("expected Corruption binding partial to wrong key, got {other:?}"),
    }
}

/// Store an HRT1 partial keyed by H; it lives in a slot distinct from the full
/// object. A request for the FULL tree H returns the typed partial via
/// `read_tree` — and `get_tree` returns `None`, never the partial-as-full.
fn assert_partial_distinct_from_full(store: &impl ObjectStore) {
    let (_full, root, body, withheld) = fixture();

    match store.put_partial_tree(&root, &body).unwrap() {
        PartialTreeWrite::Stored { redacted, visible } => {
            assert_eq!(redacted, 1);
            assert_eq!(visible, 2);
        }
        other => panic!("expected Stored, got {other:?}"),
    }

    // Full-tree accessors never surface the partial.
    assert!(!store.has_tree(&root).unwrap(), "partial must not be a full tree");
    assert!(store.get_tree(&root).unwrap().is_none(), "get_tree must not return the partial");

    // Partial slot holds it.
    assert!(store.has_partial_tree(&root).unwrap());
    assert_eq!(store.get_partial_tree_bytes(&root).unwrap().as_deref(), Some(body.as_slice()));
    assert_eq!(store.list_partial_trees().unwrap(), vec![root]);

    // The typed read model distinguishes withheld from missing/corrupt.
    match store.read_tree(&root).unwrap() {
        TreeRead::Partial(p) => {
            assert_eq!(p.reconstruct_root(), root);
            assert_eq!(p.redacted_count(), 1);
            assert_eq!(p.leaves()[..].iter().find(|l| l.leaf_hash() == withheld).map(|l| l.is_redacted()), Some(true));
        }
        other => panic!("expected Partial, got {other:?}"),
    }

    // A hash we hold nothing for is Absent, not Partial.
    assert!(matches!(store.read_tree(&ch(b"absent")).unwrap(), TreeRead::Absent));
}

/// A full tree SUPERSEDES a partial at read time, and a partial NEVER overwrites
/// an existing full (monotone).
fn assert_monotone(store: &impl ObjectStore) {
    let (full, root, body, _withheld) = fixture();

    // full-before-partial: the projection is refused (superseded).
    store.put_tree(&full).unwrap();
    assert_eq!(
        store.put_partial_tree(&root, &body).unwrap(),
        PartialTreeWrite::SupersededByFull
    );
    assert!(!store.has_partial_tree(&root).unwrap(), "partial must not overwrite the full");
    assert!(matches!(store.read_tree(&root).unwrap(), TreeRead::Full(_)));
}

/// partial-before-full: once the full lands, read_tree resolves to Full even
/// while a stale partial slot may still exist (read-time supersession); an
/// explicit backfill can then reclaim the slot.
fn assert_full_supersedes_stale_partial(store: &impl ObjectStore) {
    let (full, root, body, _withheld) = fixture();
    store.put_partial_tree(&root, &body).unwrap();
    assert!(store.has_partial_tree(&root).unwrap());

    store.put_tree(&full).unwrap();
    match store.read_tree(&root).unwrap() {
        TreeRead::Full(t) => assert_eq!(t.hash(), root),
        other => panic!("full must supersede the partial at read time, got {other:?}"),
    }

    // Backfill reclaims the now-redundant slot.
    store.remove_partial_tree(&root).unwrap();
    assert!(!store.has_partial_tree(&root).unwrap());
}

/// A full tree still stores/reads normally (regression).
fn assert_full_tree_regression(store: &impl ObjectStore) {
    let (full, root, _body, _withheld) = fixture();
    store.put_tree(&full).unwrap();
    assert!(store.has_tree(&root).unwrap());
    assert_eq!(store.get_tree(&root).unwrap().unwrap().hash(), root);
    assert!(matches!(store.read_tree(&root).unwrap(), TreeRead::Full(_)));
    assert!(!store.has_partial_tree(&root).unwrap());
}

/// `put_tree_serialized` routes an HRT1 body to the partial slot rather than
/// hard-refusing it in the full-tree decoder.
fn assert_put_tree_serialized_routes_hrt1(store: &impl ObjectStore) {
    let (_full, root, body, _withheld) = fixture();
    let stored = store.put_tree_serialized(&body, root).unwrap();
    assert_eq!(stored, root);
    assert!(store.has_partial_tree(&root).unwrap());
    assert!(!store.has_tree(&root).unwrap());
    assert!(matches!(store.read_tree(&root).unwrap(), TreeRead::Partial(_)));
}

#[test]
fn memory_partial_distinct_from_full() {
    assert_partial_distinct_from_full(&InMemoryStore::new());
}

#[test]
fn memory_monotone() {
    assert_monotone(&InMemoryStore::new());
}

#[test]
fn memory_full_supersedes_stale_partial() {
    assert_full_supersedes_stale_partial(&InMemoryStore::new());
}

#[test]
fn memory_full_tree_regression() {
    assert_full_tree_regression(&InMemoryStore::new());
}

#[test]
fn memory_put_tree_serialized_routes_hrt1() {
    assert_put_tree_serialized_routes_hrt1(&InMemoryStore::new());
}

fn fs_store() -> (tempfile::TempDir, FsStore) {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FsStore::new(temp.path());
    (temp, store)
}

#[test]
fn fs_partial_distinct_from_full() {
    let (_t, store) = fs_store();
    assert_partial_distinct_from_full(&store);
}

#[test]
fn fs_monotone() {
    let (_t, store) = fs_store();
    assert_monotone(&store);
}

#[test]
fn fs_full_supersedes_stale_partial() {
    let (_t, store) = fs_store();
    assert_full_supersedes_stale_partial(&store);
}

#[test]
fn fs_full_tree_regression() {
    let (_t, store) = fs_store();
    assert_full_tree_regression(&store);
}

#[test]
fn fs_put_tree_serialized_routes_hrt1() {
    let (_t, store) = fs_store();
    assert_put_tree_serialized_routes_hrt1(&store);
}
