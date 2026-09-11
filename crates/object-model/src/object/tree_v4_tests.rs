// SPDX-License-Identifier: Apache-2.0
//! V4 salted-Merkle redactable tree tests (leg 1 core + Fable T-d).
//!
//! These reconstruct the leaf/root formula independently from the design spec,
//! so an accidental change to the hashing is caught, and exercise the
//! scheme-total encoder contract: a V4 tree round-trips with an IDENTICAL
//! `hash()` through the salt-carrying encodings and returns `Err` through the
//! salt-less ones — it is never silently re-hashed as V3.

use std::collections::HashSet;

use crate::{
    compact,
    object::{
        BytesTreeSource, ContentHash, EntryType, FileMode, PartialTree, Tree, TreeEntry,
        TreeEntryReader, TreeScheme, decode_redacted_projection, decode_salted_v4,
        encode_redacted_projection, is_redacted_tree, is_salted_tree,
    },
};

fn ch(bytes: &[u8]) -> ContentHash {
    ContentHash::compute(bytes)
}

fn sample_entries() -> Vec<TreeEntry> {
    vec![
        TreeEntry::file("readme.md", ch(b"readme"), false).unwrap(),
        TreeEntry::file("secret.md", ch(b"secret"), false).unwrap(),
        TreeEntry::directory("src", ch(b"src-tree")).unwrap(),
    ]
}

fn sample_salts() -> Vec<[u8; 32]> {
    vec![[0x11; 32], [0x22; 32], [0x33; 32]]
}

fn sample_v4() -> Tree {
    Tree::from_entries_salted_v4(sample_entries(), sample_salts()).unwrap()
}

/// Independently recompute the V4 leaf commitment for a blob entry from the
/// design spec: `typed_hasher("tree-v4-leaf", len)(salt ‖ mode ‖ type ‖
/// hash ‖ name_len(u16 LE) ‖ name)`.
fn spec_blob_leaf(name: &str, hash: ContentHash, salt: &[u8; 32]) -> ContentHash {
    let mut preimage = Vec::new();
    preimage.extend_from_slice(salt);
    preimage.push(FileMode::Normal.to_byte());
    preimage.push(EntryType::Blob.to_byte());
    preimage.extend_from_slice(hash.as_bytes());
    preimage.extend_from_slice(&(name.len() as u16).to_le_bytes());
    preimage.extend_from_slice(name.as_bytes());
    ContentHash::compute_typed("tree-v4-leaf", &preimage)
}

fn spec_node(left: ContentHash, right: ContentHash) -> ContentHash {
    let mut hasher = ContentHash::typed_hasher("tree-v4-node", 64);
    hasher.update(left.as_bytes());
    hasher.update(right.as_bytes());
    ContentHash::from_bytes(hasher.finalize().into())
}

// ── leaf / root formula ─────────────────────────────────────────────

#[test]
fn single_entry_root_matches_spec_leaf() {
    let salt = [0x11; 32];
    let hash = ch(b"readme");
    let tree = Tree::from_entries_salted_v4(
        vec![TreeEntry::file("readme.md", hash, false).unwrap()],
        vec![salt],
    )
    .unwrap();
    // MTH([leaf]) == leaf, and the leaf follows the documented preimage.
    assert_eq!(tree.hash(), spec_blob_leaf("readme.md", hash, &salt));
}

#[test]
fn two_entry_root_matches_spec_node_ordered_by_leaf_hash() {
    let s0 = [0xaa; 32];
    let s1 = [0xbb; 32];
    let h0 = ch(b"a");
    let h1 = ch(b"b");
    let tree = Tree::from_entries_salted_v4(
        vec![
            TreeEntry::file("a", h0, false).unwrap(),
            TreeEntry::file("b", h1, false).unwrap(),
        ],
        vec![s0, s1],
    )
    .unwrap();
    let leaf_a = spec_blob_leaf("a", h0, &s0);
    let leaf_b = spec_blob_leaf("b", h1, &s1);
    // Ordered by leaf hash, NOT by name.
    let (lo, hi) = if leaf_a <= leaf_b {
        (leaf_a, leaf_b)
    } else {
        (leaf_b, leaf_a)
    };
    assert_eq!(tree.hash(), spec_node(lo, hi));
}

#[test]
fn hash_is_deterministic_and_salt_sensitive() {
    let a = sample_v4();
    let b = sample_v4();
    assert_eq!(a.hash(), b.hash(), "same content + salts => same id");

    let mut other_salts = sample_salts();
    other_salts[1] = [0x99; 32];
    let c = Tree::from_entries_salted_v4(sample_entries(), other_salts).unwrap();
    assert_ne!(a.hash(), c.hash(), "changing a salt changes the id");
}

#[test]
fn input_order_does_not_change_v4_id() {
    let forward = sample_v4();
    let mut entries = sample_entries();
    let mut salts = sample_salts();
    entries.reverse();
    salts.reverse();
    let reversed = Tree::from_entries_salted_v4(entries, salts).unwrap();
    assert_eq!(forward.hash(), reversed.hash());
    assert_eq!(forward, reversed);
}

// ── empty-tree parity (MF-5) ────────────────────────────────────────

#[test]
fn v4_empty_root_equals_v3_empty_hash() {
    let v3_empty = Tree::new().hash();
    let v3_empty_from_entries = Tree::from_entries(vec![]).hash();
    let v4_empty = Tree::from_entries_salted_v4(vec![], vec![]).unwrap().hash();
    assert_eq!(v3_empty, v3_empty_from_entries);
    assert_eq!(
        v4_empty, v3_empty,
        "v4 empty root must equal the v3 empty-tree id (import-anchor sentinels)"
    );
    // And equal the raw typed empty hash.
    assert_eq!(v4_empty, ContentHash::compute_typed("tree", b""));
}

// ── non-invertibility ───────────────────────────────────────────────

#[test]
fn redacted_leaf_is_not_invertible_without_the_salt() {
    // A 1-entry tree's root IS its leaf hash. With a fixed (name, target) and a
    // random 256-bit salt, the leaf is unpredictable: 4096 random salts produce
    // 4096 distinct leaves, and no guessed salt reproduces the real one.
    let name = "password.txt";
    let target = ch(b"the-actual-secret-bytes");
    let real_salt: [u8; 32] = rand::random();
    let real_leaf = spec_blob_leaf(name, target, &real_salt);

    let mut seen = HashSet::new();
    seen.insert(real_leaf);
    for _ in 0..4096 {
        let guess: [u8; 32] = rand::random();
        let leaf = spec_blob_leaf(name, target, &guess);
        // Guessing name+target without the salt never confirms the leaf.
        if guess != real_salt {
            assert_ne!(leaf, real_leaf, "guessed salt must not reproduce the leaf");
        }
        seen.insert(leaf);
    }
    assert!(
        seen.len() > 4000,
        "salted leaves are near-uniformly distinct"
    );
}

// ── PartialTree reconstruction ──────────────────────────────────────

#[test]
fn redacted_partial_tree_reconstructs_the_full_root() {
    let tree = sample_v4();
    let full_root = tree.hash();

    // Redact secret.md's leaf.
    let secret = TreeEntry::file("secret.md", ch(b"secret"), false).unwrap();
    let secret_leaf = Tree::from_entries_salted_v4(vec![secret], vec![[0x22; 32]])
        .unwrap()
        .hash();
    let mut redact = HashSet::new();
    redact.insert(secret_leaf);

    let partial = PartialTree::project(&tree, &redact).unwrap();
    assert_eq!(partial.redacted_count(), 1);
    assert_eq!(
        partial.reconstruct_root(),
        full_root,
        "redacted projection must reconstruct the same merkle root"
    );
    partial.verify().unwrap();
}

#[test]
fn fully_visible_partial_tree_round_trips_to_tree() {
    let tree = sample_v4();
    let partial = PartialTree::project(&tree, &HashSet::new()).unwrap();
    assert_eq!(partial.redacted_count(), 0);
    let back = partial.into_tree().unwrap();
    assert_eq!(back, tree);
    assert_eq!(back.hash(), tree.hash());
}

#[test]
fn partial_tree_with_redaction_cannot_materialize_a_full_tree() {
    let tree = sample_v4();
    let secret_leaf = Tree::from_entries_salted_v4(
        vec![TreeEntry::file("secret.md", ch(b"secret"), false).unwrap()],
        vec![[0x22; 32]],
    )
    .unwrap()
    .hash();
    let mut redact = HashSet::new();
    redact.insert(secret_leaf);
    let partial = PartialTree::project(&tree, &redact).unwrap();
    assert!(partial.into_tree().is_err());
}

// ── HSR1 / HRT1 encode-decode ───────────────────────────────────────

#[test]
fn hsr1_canonical_round_trips_with_identical_hash() {
    let tree = sample_v4();
    let body = tree.encode_canonical().unwrap();
    assert!(is_salted_tree(&body), "v4 encode_canonical must emit HSR1");
    let decoded = Tree::decode_canonical(&body).unwrap();
    assert_eq!(decoded, tree);
    assert_eq!(decoded.hash(), tree.hash());
    assert_eq!(decoded.scheme(), TreeScheme::V4Salted);
    // The explicit salted decoder agrees.
    assert_eq!(decode_salted_v4(&body).unwrap(), tree);
}

#[test]
fn hsr1_corrupted_salt_fails_loud_not_silent() {
    let tree = sample_v4();
    let mut body = tree.encode_canonical().unwrap();
    // Flip a byte inside the first frame's salt (after the 61-byte header + the
    // 4-byte frame length prefix).
    let salt_byte = 61 + 4;
    body[salt_byte] ^= 0xff;
    match Tree::decode_canonical(&body) {
        Err(err) => {
            let msg = err.to_string();
            assert!(
                msg.contains("does not match") || msg.contains("mismatch"),
                "expected a hash-mismatch, got: {msg}"
            );
        }
        Ok(_) => panic!("a corrupted salt must not decode silently"),
    }
}

#[test]
fn hrt1_projection_round_trips_and_hides_redacted_bytes() {
    let tree = sample_v4();
    let full_root = tree.hash();
    let secret_leaf = Tree::from_entries_salted_v4(
        vec![TreeEntry::file("secret.md", ch(b"secret"), false).unwrap()],
        vec![[0x22; 32]],
    )
    .unwrap()
    .hash();
    let mut redact = HashSet::new();
    redact.insert(secret_leaf);
    let partial = PartialTree::project(&tree, &redact).unwrap();

    let body = encode_redacted_projection(&partial).unwrap();
    assert!(is_redacted_tree(&body), "must emit HRT1");
    // The redacted entry's name bytes must not appear anywhere in the body.
    assert!(
        !body.windows(b"secret.md".len()).any(|w| w == b"secret.md"),
        "redacted name must not be serialized"
    );
    // A visible name is present.
    assert!(body.windows(b"readme.md".len()).any(|w| w == b"readme.md"));

    let decoded = decode_redacted_projection(&body).unwrap();
    assert_eq!(decoded.declared_root(), full_root);
    assert_eq!(decoded.reconstruct_root(), full_root);
    decoded.verify().unwrap();
    assert_eq!(decoded.redacted_count(), 1);
}

#[test]
fn hrt1_corrupted_redacted_leaf_fails_root_check() {
    let tree = sample_v4();
    let secret_leaf = Tree::from_entries_salted_v4(
        vec![TreeEntry::file("secret.md", ch(b"secret"), false).unwrap()],
        vec![[0x22; 32]],
    )
    .unwrap()
    .hash();
    let mut redact = HashSet::new();
    redact.insert(secret_leaf);
    let partial = PartialTree::project(&tree, &redact).unwrap();
    let mut body = encode_redacted_projection(&partial).unwrap();
    // Corrupt the last byte (a redacted leaf hash tail).
    let last = body.len() - 1;
    body[last] ^= 0xff;
    assert!(
        decode_redacted_projection(&body).is_err(),
        "a corrupted redacted leaf must fail the root reconstruction check"
    );
}

// ── scheme-total encoders: salt-less encodings refuse V4 ────────────

#[test]
fn v4_refused_by_lean_delta_and_compact_encoders() {
    let tree = sample_v4();
    assert!(tree.encode_lean().is_err(), "HLR1 must refuse a v4 tree");

    // HDC1 delta: encode against a v4 anchor must error.
    let anchor = sample_v4();
    let ops = crate::object::tree_delta(&anchor, &tree);
    assert!(
        crate::object::encode_tree_delta(anchor.hash(), &anchor, &tree, &ops).is_err(),
        "HDC1 must refuse a v4 tree"
    );

    // HCT1 compact frame.
    assert!(
        compact::encode_tree_frame(&[tree]).is_err(),
        "HCT1 must refuse a v4 tree"
    );
}

// ── msgpack durable serde (the worktree-current-tree.bin cache path) ─

#[test]
fn v4_msgpack_round_trips_named_and_unnamed() {
    let tree = sample_v4();
    // `to_vec_named` is exactly what repository_tree.rs uses for the cache.
    let named = rmp_serde::to_vec_named(&tree).unwrap();
    let decoded_named: Tree = rmp_serde::from_slice(&named).unwrap();
    assert_eq!(decoded_named, tree);
    assert_eq!(decoded_named.hash(), tree.hash());
    assert_eq!(decoded_named.scheme(), TreeScheme::V4Salted);

    let plain = rmp_serde::to_vec(&tree).unwrap();
    let decoded_plain: Tree = rmp_serde::from_slice(&plain).unwrap();
    assert_eq!(decoded_plain, tree);
    assert_eq!(decoded_plain.hash(), tree.hash());

    // `decode_current_msgpack` also accepts the v4 body.
    assert_eq!(Tree::decode_current_msgpack(&named).unwrap(), tree);
}

#[test]
fn v3_msgpack_body_is_unchanged_by_the_v4_field() {
    // A V3 tree must serialize with no salts field (byte-identical to before),
    // so existing on-disk caches keep round-tripping.
    let v3 = Tree::from_entries(vec![
        TreeEntry::file("a", ch(b"a"), false).unwrap(),
        TreeEntry::file("b", ch(b"b"), true).unwrap(),
    ]);
    let bytes = rmp_serde::to_vec_named(&v3).unwrap();
    let back: Tree = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back, v3);
    assert_eq!(back.scheme(), TreeScheme::V3Flat);
    assert!(back.salts().is_empty());
}

// ── tree_stream: HSR1 recognized (eager), HRT1 rejected ─────────────

#[test]
fn tree_stream_reader_refuses_hsr1_and_hrt1() {
    let tree = sample_v4();
    let hsr1 = tree.encode_canonical().unwrap();
    let source = BytesTreeSource::sequential_verify(bytes::Bytes::from(hsr1));
    assert!(
        TreeEntryReader::open(source, tree.hash(), None).is_err(),
        "the streaming reader must not accept an HSR1 body"
    );

    let secret_leaf = Tree::from_entries_salted_v4(
        vec![TreeEntry::file("secret.md", ch(b"secret"), false).unwrap()],
        vec![[0x22; 32]],
    )
    .unwrap()
    .hash();
    let mut redact = HashSet::new();
    redact.insert(secret_leaf);
    let partial = PartialTree::project(&tree, &redact).unwrap();
    let hrt1 = encode_redacted_projection(&partial).unwrap();
    let source = BytesTreeSource::sequential_verify(bytes::Bytes::from(hrt1));
    assert!(
        TreeEntryReader::open(source, tree.hash(), None).is_err(),
        "the streaming reader must reject an HRT1 projection"
    );
}

#[test]
fn decode_canonical_rejects_hrt1() {
    let tree = sample_v4();
    let secret_leaf = Tree::from_entries_salted_v4(
        vec![TreeEntry::file("secret.md", ch(b"secret"), false).unwrap()],
        vec![[0x22; 32]],
    )
    .unwrap()
    .hash();
    let mut redact = HashSet::new();
    redact.insert(secret_leaf);
    let partial = PartialTree::project(&tree, &redact).unwrap();
    let hrt1 = encode_redacted_projection(&partial).unwrap();
    assert!(
        Tree::decode_canonical(&hrt1).is_err(),
        "a redacted projection must never decode as a full tree"
    );
}

// ── insert/remove maintain the parallel salt invariant ──────────────

#[test]
fn v4_insert_and_remove_keep_salts_parallel() {
    let mut tree = sample_v4();
    let before = tree.hash();
    tree.insert(TreeEntry::file("zzz.txt", ch(b"zzz"), false).unwrap());
    assert_eq!(tree.salts().len(), tree.entries().len());
    tree.validate().unwrap();
    assert_ne!(tree.hash(), before);

    assert!(tree.remove("zzz.txt").is_some());
    assert_eq!(tree.salts().len(), tree.entries().len());
    tree.validate().unwrap();
    // Removing the just-inserted entry restores the original id (its salt was
    // dropped and the remaining salts are unchanged).
    assert_eq!(tree.hash(), before);
}

// ── from-scratch non-determinism (residual risk (d), expected) ──────

#[test]
fn from_scratch_random_salts_differ() {
    let entries = || sample_entries();
    let a =
        Tree::from_entries_salted_v4(entries(), (0..3).map(|_| rand::random()).collect()).unwrap();
    let b =
        Tree::from_entries_salted_v4(entries(), (0..3).map(|_| rand::random()).collect()).unwrap();
    assert_ne!(
        a.hash(),
        b.hash(),
        "independent from-scratch captures mint fresh salts => different ids"
    );
}
