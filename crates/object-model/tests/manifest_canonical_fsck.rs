// SPDX-License-Identifier: Apache-2.0
//! Canonical manifest encoding + fsck: round-trip, hash stability, and one
//! named rejection per corruption class.
//!
//! The negative cases are the point. A checker that only ever sees valid input
//! proves nothing, so every rule below is exercised by a graph that violates it
//! and is asserted *by rule name*.

use std::collections::{BTreeMap, BTreeSet};

use heddle_object_model::object::{
    ContentHash,
    manifest::{
        FsckOptions, FsckRule, MANIFEST_LEAF_MAX_ENTRIES, MANIFEST_ROUTE_LEVELS, ManifestBranch,
        ManifestChild, ManifestLeaf, ManifestNode, ManifestObject, ManifestObjectKind,
        PackRangeAudit, PackRangeClaim, PackRecord, build_manifest, expand_manifest, fsck_manifest,
        fsck_manifest_store, fsck_manifest_with, fsck_pack_range,
    },
};
use proptest::prelude::*;

// ── Fixtures ────────────────────────────────────────────────────────

type Nodes = BTreeMap<ContentHash, Vec<u8>>;

fn object(seed: u32) -> ManifestObject {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&seed.to_be_bytes());
    ManifestObject::new(
        if seed.is_multiple_of(4) {
            ManifestObjectKind::Tree
        } else {
            ManifestObjectKind::Blob
        },
        ContentHash::compute(&bytes),
        u64::from(seed) * 13 + 1,
    )
}

fn objects(count: u32) -> Vec<ManifestObject> {
    (0..count).map(object).collect()
}

/// A two-level fixture: 100 objects spread over 32 slots leaves every child a
/// leaf, so a test can rewrite one leaf and repair its parent by hand.
fn fixture() -> (ContentHash, Nodes, Vec<ManifestObject>) {
    let set = objects(100);
    let built = build_manifest(set.clone()).expect("fixture builds");
    (built.root, built.nodes, set)
}

fn branch_at(nodes: &Nodes, hash: &ContentHash) -> ManifestBranch {
    match ManifestNode::decode(&nodes[hash]).expect("node decodes") {
        ManifestNode::Branch(branch) => branch,
        ManifestNode::Leaf(_) => panic!("expected a branch at {hash}"),
    }
}

fn leaf_at(nodes: &Nodes, hash: &ContentHash) -> ManifestLeaf {
    match ManifestNode::decode(&nodes[hash]).expect("node decodes") {
        ManifestNode::Leaf(leaf) => leaf,
        ManifestNode::Branch(_) => panic!("expected a leaf at {hash}"),
    }
}

fn insert(nodes: &mut Nodes, node: &ManifestNode) -> ContentHash {
    let bytes = node.encode();
    let hash = ContentHash::compute(&bytes);
    nodes.insert(hash, bytes);
    hash
}

/// Replace the leaf under `slot` of the root branch with `entries`, rebuilding
/// the root so every digest still checks out. When `repair_summary` is set the
/// new child summary matches the new leaf, so a test can isolate the rule it
/// actually cares about.
fn replace_root_leaf(
    root: &ContentHash,
    nodes: &mut Nodes,
    slot: u8,
    entries: Vec<ManifestObject>,
    repair_summary: bool,
) -> ContentHash {
    let branch = branch_at(nodes, root);
    let old = *branch.child_at(slot).expect("slot is occupied");

    let leaf = ManifestLeaf::new(entries).expect("leaf builds");
    let count = leaf.object_count();
    let bytes = leaf.decoded_bytes().expect("no overflow");
    let child_hash = insert(nodes, &ManifestNode::Leaf(leaf));

    let children: Vec<ManifestChild> = branch
        .children()
        .iter()
        .map(|child| {
            if child.slot == slot {
                ManifestChild {
                    slot,
                    hash: child_hash,
                    object_count: if repair_summary {
                        count
                    } else {
                        old.object_count
                    },
                    decoded_bytes: if repair_summary {
                        bytes
                    } else {
                        old.decoded_bytes
                    },
                }
            } else {
                *child
            }
        })
        .collect();

    let new_root =
        ManifestNode::Branch(ManifestBranch::new(branch.depth(), children).expect("branch builds"));
    insert(nodes, &new_root)
}

fn object_index(set: &[ManifestObject]) -> BTreeMap<heddle_object_model::object::ManifestKey, u64> {
    set.iter()
        .map(|object| (object.key(), object.decoded_size))
        .collect()
}

// ── Encoding: round-trip and hash stability ─────────────────────────

#[test]
fn every_node_of_a_built_manifest_round_trips_byte_for_byte() {
    let (root, nodes, _) = fixture();
    assert!(nodes.len() > 1, "fixture should be more than one node");
    for (hash, bytes) in &nodes {
        let node = ManifestNode::decode(bytes).expect("node decodes");
        assert_eq!(&node.encode(), bytes, "re-encode differs at {hash}");
        assert_eq!(node.address(), *hash, "address is not BLAKE3 of the bytes");
    }
    assert!(nodes.contains_key(&root));
}

#[test]
fn the_same_logical_membership_always_hashes_the_same() {
    let set = objects(400);
    let mut shuffled = set.clone();
    shuffled.rotate_left(137);
    shuffled.reverse();

    let a = build_manifest(set.clone()).unwrap();
    let b = build_manifest(shuffled).unwrap();
    assert_eq!(a.root, b.root);
    assert_eq!(a.nodes, b.nodes);

    // And it is stable across runs of this binary, not merely self-consistent.
    let c = build_manifest(set).unwrap();
    assert_eq!(a.root, c.root);
}

#[test]
fn expansion_is_the_original_set_in_canonical_key_order() {
    let (root, nodes, mut set) = fixture();
    let expanded = expand_manifest(&nodes, &root).unwrap();
    set.sort_by_key(ManifestObject::key);
    assert_eq!(expanded, set);
    assert!(
        expanded.windows(2).all(|w| w[0].key() < w[1].key()),
        "expansion must be strictly ascending by plan key"
    );
}

#[test]
fn a_context_only_republish_reuses_the_root_and_a_content_change_does_not() {
    // The structural-sharing contract: identical membership → identical root
    // (nothing republished); one object replaced → bounded rewrite.
    let set = objects(2_000);
    let before = build_manifest(set.clone()).unwrap();
    assert_eq!(before.root, build_manifest(set.clone()).unwrap().root);

    let mut changed = set;
    changed[900] = object(10_000_000);
    let after = build_manifest(changed).unwrap();
    assert_ne!(before.root, after.root);

    let rewritten = after
        .nodes
        .keys()
        .filter(|hash| !before.nodes.contains_key(*hash))
        .count();
    assert!(
        rewritten <= usize::from(MANIFEST_ROUTE_LEVELS),
        "one object change rewrote {rewritten} nodes"
    );
}

// ── fsck: the happy path ────────────────────────────────────────────

#[test]
fn fsck_accepts_a_well_formed_graph() {
    let (root, nodes, set) = fixture();
    assert!(fsck_manifest(&nodes, &root).is_clean());

    let index = object_index(&set);
    let report = fsck_manifest_with(
        &nodes,
        &root,
        &FsckOptions {
            objects: Some(&index),
            report_unreachable: false,
        },
    );
    assert!(report.is_clean(), "unexpected findings: {report:?}");
}

#[test]
fn fsck_accepts_the_canonical_empty_root() {
    let built = build_manifest([]).unwrap();
    assert!(fsck_manifest(&built.nodes, &built.root).is_clean());
}

#[test]
fn fsck_accepts_a_deep_multi_level_graph() {
    // 20k objects forces at least two branch levels, exercising depth and
    // summary rules on real interior nodes.
    let built = build_manifest(objects(20_000)).unwrap();
    let has_depth_1_branch = built.nodes.values().any(|bytes| {
        matches!(ManifestNode::decode(bytes), Ok(ManifestNode::Branch(b)) if b.depth() == 1)
    });
    assert!(
        has_depth_1_branch,
        "fixture is not deep enough to be useful"
    );
    assert!(fsck_manifest(&built.nodes, &built.root).is_clean());
}

// ── fsck: one named rejection per corruption class ──────────────────

#[test]
fn rejects_bad_digest() {
    let (root, mut nodes, _) = fixture();
    let victim = *nodes.keys().find(|hash| **hash != root).unwrap();
    let bytes = nodes.get_mut(&victim).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;

    let report = fsck_manifest(&nodes, &root);
    assert!(report.has_rule(FsckRule::NodeDigestMismatch), "{report:?}");
}

#[test]
fn rejects_dangling_node_ref() {
    let (root, mut nodes, _) = fixture();
    let victim = *nodes.keys().find(|hash| **hash != root).unwrap();
    nodes.remove(&victim);

    let report = fsck_manifest(&nodes, &root);
    assert!(report.has_rule(FsckRule::DanglingNodeRef), "{report:?}");
    assert!(
        report
            .findings()
            .iter()
            .any(|f| f.node == Some(victim) && f.rule == FsckRule::DanglingNodeRef)
    );
}

#[test]
fn rejects_dangling_object_ref_and_size_mismatch() {
    let (root, nodes, set) = fixture();

    let mut index = object_index(&set);
    let missing = set[3].key();
    index.remove(&missing);
    let wrong_size = set[7].key();
    *index.get_mut(&wrong_size).unwrap() += 1;

    let report = fsck_manifest_with(
        &nodes,
        &root,
        &FsckOptions {
            objects: Some(&index),
            report_unreachable: false,
        },
    );
    assert!(report.has_rule(FsckRule::DanglingObjectRef), "{report:?}");
    assert!(report.has_rule(FsckRule::ObjectSizeMismatch), "{report:?}");
}

#[test]
fn rejects_wrong_order_within_a_leaf() {
    let (root, mut nodes, _) = fixture();
    let branch = branch_at(&nodes, &root);
    let slot = branch
        .children()
        .iter()
        .find(|child| child.object_count >= 2)
        .expect("a leaf with two entries")
        .slot;
    let leaf = leaf_at(&nodes, &branch.child_at(slot).unwrap().hash);

    // Hand-write the leaf with its first two entries swapped. `ManifestLeaf`
    // would re-sort them, so the bytes are assembled directly.
    let entries = leaf.entries();
    let mut bytes = vec![b'W', b'P', b'M', b'F', 1, 0];
    bytes.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    let mut order: Vec<&ManifestObject> = entries.iter().collect();
    order.swap(0, 1);
    for entry in order {
        bytes.push(entry.kind.to_byte());
        bytes.extend_from_slice(entry.hash.as_bytes());
        bytes.extend_from_slice(&entry.decoded_size.to_be_bytes());
    }
    let corrupt = ContentHash::compute(&bytes);
    nodes.insert(corrupt, bytes);

    let children: Vec<ManifestChild> = branch
        .children()
        .iter()
        .map(|child| {
            if child.slot == slot {
                ManifestChild {
                    hash: corrupt,
                    ..*child
                }
            } else {
                *child
            }
        })
        .collect();
    let new_root = insert(
        &mut nodes,
        &ManifestNode::Branch(ManifestBranch::new(branch.depth(), children).unwrap()),
    );

    let report = fsck_manifest(&nodes, &new_root);
    assert!(
        report.has_rule(FsckRule::LeafEntriesOutOfOrder),
        "{report:?}"
    );
}

#[test]
fn rejects_duplicate_object_key_and_trailing_bytes_and_bad_magic() {
    let (root, nodes, _) = fixture();
    let leaf_hash = branch_at(&nodes, &root).children()[0].hash;
    let leaf = leaf_at(&nodes, &leaf_hash);
    let entry = leaf.entries()[0];

    // Duplicate: two identical entries, written directly.
    let mut duplicated = vec![b'W', b'P', b'M', b'F', 1, 0];
    duplicated.extend_from_slice(&2u16.to_be_bytes());
    for _ in 0..2 {
        duplicated.push(entry.kind.to_byte());
        duplicated.extend_from_slice(entry.hash.as_bytes());
        duplicated.extend_from_slice(&entry.decoded_size.to_be_bytes());
    }
    assert_eq!(
        check_single_node(duplicated),
        Some(FsckRule::DuplicateObjectKey)
    );

    let mut trailing = nodes[&leaf_hash].clone();
    trailing.push(0);
    assert_eq!(check_single_node(trailing), Some(FsckRule::TrailingBytes));

    let mut bad_magic = nodes[&leaf_hash].clone();
    bad_magic[0] = b'Q';
    assert_eq!(check_single_node(bad_magic), Some(FsckRule::MalformedNode));
}

/// Store `bytes` under their own (correct) digest and fsck them as a root, so
/// the only thing that can fail is the node's own well-formedness.
fn check_single_node(bytes: Vec<u8>) -> Option<FsckRule> {
    let hash = ContentHash::compute(&bytes);
    let nodes: Nodes = [(hash, bytes)].into_iter().collect();
    fsck_manifest(&nodes, &hash)
        .findings()
        .first()
        .map(|f| f.rule)
}

#[test]
fn rejects_subtree_summary_mismatch() {
    let (root, mut nodes, _) = fixture();
    let leaf_hash = branch_at(&nodes, &root).children()[0].hash;
    let mut entries = leaf_at(&nodes, &leaf_hash).entries().to_vec();
    entries.pop();

    // Drop an entry but leave the parent's summary claiming the old totals.
    let slot = branch_at(&nodes, &root).children()[0].slot;
    let new_root = replace_root_leaf(&root, &mut nodes, slot, entries, false);

    let report = fsck_manifest(&nodes, &new_root);
    assert!(
        report.has_rule(FsckRule::SubtreeSummaryMismatch),
        "{report:?}"
    );
}

#[test]
fn rejects_misrouted_entry() {
    let (root, mut nodes, _) = fixture();
    let branch = branch_at(&nodes, &root);
    let host = branch.children()[1];
    let donor_entry = leaf_at(&nodes, &branch.children()[0].hash).entries()[0];
    assert_ne!(
        donor_entry.key().route().group(branch.depth()),
        host.slot,
        "the donor entry must not legitimately belong in the host slot"
    );

    let mut entries = leaf_at(&nodes, &host.hash).entries().to_vec();
    entries.push(donor_entry);
    let new_root = replace_root_leaf(&root, &mut nodes, host.slot, entries, true);

    let report = fsck_manifest(&nodes, &new_root);
    assert!(report.has_rule(FsckRule::MisroutedEntry), "{report:?}");
}

#[test]
fn rejects_underfull_branch_and_empty_non_root_leaf() {
    // A branch whose whole subtree fits in one leaf must not exist, and only
    // the root may be the empty leaf. Both are built here by hand.
    let small = objects(4);
    let leaf = ManifestLeaf::new(small.clone()).unwrap();
    let mut nodes: Nodes = BTreeMap::new();
    let leaf_hash = insert(&mut nodes, &ManifestNode::Leaf(leaf));
    let slot = small[0].key().route().group(0);
    let branch = ManifestBranch::new(
        0,
        vec![ManifestChild {
            slot,
            hash: leaf_hash,
            object_count: 4,
            decoded_bytes: small.iter().map(|o| o.decoded_size).sum(),
        }],
    )
    .unwrap();
    let root = insert(&mut nodes, &ManifestNode::Branch(branch));
    let report = fsck_manifest(&nodes, &root);
    assert!(report.has_rule(FsckRule::UnderfullBranch), "{report:?}");

    let mut nodes: Nodes = BTreeMap::new();
    let empty_hash = insert(&mut nodes, &ManifestNode::empty());
    let branch = ManifestBranch::new(
        0,
        vec![ManifestChild {
            slot: 0,
            hash: empty_hash,
            object_count: 0,
            decoded_bytes: 0,
        }],
    )
    .unwrap();
    let root = insert(&mut nodes, &ManifestNode::Branch(branch));
    let report = fsck_manifest(&nodes, &root);
    assert!(report.has_rule(FsckRule::EmptyNonRootLeaf), "{report:?}");
}

#[test]
fn rejects_branch_depth_mismatch() {
    let (root, mut nodes, _) = fixture();
    let branch = branch_at(&nodes, &root);
    let lying = ManifestBranch::new(branch.depth() + 3, branch.children().to_vec()).unwrap();
    let new_root = insert(&mut nodes, &ManifestNode::Branch(lying));

    let report = fsck_manifest(&nodes, &new_root);
    assert!(report.has_rule(FsckRule::BranchDepthMismatch), "{report:?}");
}

#[test]
fn rejects_leaf_overfull() {
    // A leaf holding more than the bound while routing bits remain must have
    // split into a branch.
    let entries = objects(MANIFEST_LEAF_MAX_ENTRIES as u32 + 1);
    let leaf = ManifestLeaf::new(entries).unwrap();
    let mut nodes: Nodes = BTreeMap::new();
    let root = insert(&mut nodes, &ManifestNode::Leaf(leaf));

    let report = fsck_manifest(&nodes, &root);
    assert!(report.has_rule(FsckRule::LeafOverfull), "{report:?}");
    // The shape backstop must fire too: the canonical trie for that set is a
    // branch, not one leaf.
    assert!(
        report.has_rule(FsckRule::NonCanonicalTrieShape),
        "{report:?}"
    );
}

#[test]
fn rejects_empty_branch_bitmap_bytes() {
    let mut bytes = vec![b'W', b'P', b'M', b'F', 1, 1, 0];
    bytes.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(
        check_single_node(bytes),
        Some(FsckRule::EmptyBranchBitmap),
        "an empty bitmap must be rejected; the empty set is the empty leaf"
    );
}

#[test]
fn reports_unreachable_nodes_only_on_a_store_sweep() {
    let (root, mut nodes, _) = fixture();
    let orphan = build_manifest(objects(3)).unwrap();
    nodes.extend(orphan.nodes.clone());

    // A per-root check must stay quiet: a shared store legitimately holds
    // other roots' nodes.
    assert!(fsck_manifest(&nodes, &root).is_clean());

    let report = fsck_manifest_store(
        &nodes,
        &[root],
        &FsckOptions {
            objects: None,
            report_unreachable: true,
        },
    );
    assert!(report.has_rule(FsckRule::UnreachableNode), "{report:?}");

    // Naming both roots makes the sweep clean again.
    let report = fsck_manifest_store(
        &nodes,
        &[root, orphan.root],
        &FsckOptions {
            objects: None,
            report_unreachable: true,
        },
    );
    assert!(report.is_clean(), "{report:?}");
}

// ── fsck: pack-extent rules ─────────────────────────────────────────

fn pack_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn record_over(bytes: &[u8], range_start: u64, offset: u64, length: u64, seed: u32) -> PackRecord {
    let from = (offset - range_start) as usize;
    let slice = &bytes[from..from + length as usize];
    PackRecord::new(object(seed), offset, length, ContentHash::compute(slice))
}

/// A valid claim: three records, offset-canonical, exactly covering [0, 30).
fn valid_claim() -> (PackRangeClaim, Vec<u8>) {
    let bytes = pack_bytes(30);
    let claim = PackRangeClaim::new(
        "pack-1",
        "etag-1",
        0,
        30,
        vec![
            record_over(&bytes, 0, 20, 10, 3),
            record_over(&bytes, 0, 0, 10, 1),
            record_over(&bytes, 0, 10, 10, 2),
        ],
    );
    (claim, bytes)
}

#[test]
fn fsck_accepts_a_gap_free_offset_canonical_claim() {
    let (claim, bytes) = valid_claim();
    let authorized: BTreeSet<_> = claim.records().iter().map(PackRecord::key).collect();
    let report = fsck_pack_range(
        &claim,
        &PackRangeAudit {
            range_bytes: Some(&bytes),
            authorized: Some(&authorized),
        },
    );
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn claims_canonicalize_by_pack_offset_not_object_order() {
    // weft #1070: a pack laid out for delta compression has object order and
    // offset order disagree. Canonicalizing by object first breaks contiguity.
    let bytes = pack_bytes(30);
    let claim = PackRangeClaim::new(
        "pack-1",
        "etag-1",
        0,
        30,
        vec![
            record_over(&bytes, 0, 0, 10, 900),
            record_over(&bytes, 0, 10, 10, 5),
            record_over(&bytes, 0, 20, 10, 40),
        ],
    );
    let offsets: Vec<u64> = claim.records().iter().map(|r| r.offset).collect();
    assert_eq!(offsets, vec![0, 10, 20]);

    let by_object: Vec<_> = {
        let mut keys: Vec<_> = claim.records().iter().map(PackRecord::key).collect();
        keys.sort();
        keys
    };
    let in_offset_order: Vec<_> = claim.records().iter().map(PackRecord::key).collect();
    assert_ne!(
        by_object, in_offset_order,
        "fixture must have object order differ from offset order to be meaningful"
    );
    assert!(
        fsck_pack_range(&claim, &PackRangeAudit::default()).is_clean(),
        "a contiguous claim must validate regardless of object order"
    );
}

#[test]
fn rejects_extent_gap() {
    let bytes = pack_bytes(30);
    // Drop the middle record: bytes [10, 20) are covered by the range but
    // claimed by nobody — exactly the mixed-audience leak this rule blocks.
    let claim = PackRangeClaim::new(
        "pack-1",
        "etag-1",
        0,
        30,
        vec![
            record_over(&bytes, 0, 0, 10, 1),
            record_over(&bytes, 0, 20, 10, 3),
        ],
    );
    let report = fsck_pack_range(&claim, &PackRangeAudit::default());
    assert!(report.has_rule(FsckRule::ExtentGap), "{report:?}");
}

#[test]
fn rejects_extent_overlap() {
    let bytes = pack_bytes(30);
    let claim = PackRangeClaim::new(
        "pack-1",
        "etag-1",
        0,
        30,
        vec![
            record_over(&bytes, 0, 0, 15, 1),
            record_over(&bytes, 0, 10, 20, 2),
        ],
    );
    let report = fsck_pack_range(&claim, &PackRangeAudit::default());
    assert!(report.has_rule(FsckRule::ExtentOverlap), "{report:?}");
}

#[test]
fn rejects_range_coverage_mismatch_and_zero_length_extents() {
    let bytes = pack_bytes(30);
    // Records stop at 20 while the range claims 30 bytes.
    let short = PackRangeClaim::new(
        "pack-1",
        "etag-1",
        0,
        30,
        vec![
            record_over(&bytes, 0, 0, 10, 1),
            record_over(&bytes, 0, 10, 10, 2),
        ],
    );
    let report = fsck_pack_range(&short, &PackRangeAudit::default());
    assert!(
        report.has_rule(FsckRule::RangeCoverageMismatch),
        "{report:?}"
    );

    let zero = PackRangeClaim::new(
        "pack-1",
        "etag-1",
        0,
        10,
        vec![
            PackRecord::new(object(1), 0, 0, ContentHash::compute(b"")),
            record_over(&bytes, 0, 0, 10, 2),
        ],
    );
    let report = fsck_pack_range(&zero, &PackRangeAudit::default());
    assert!(report.has_rule(FsckRule::ZeroLengthExtent), "{report:?}");
}

#[test]
fn rejects_extent_digest_mismatch() {
    let (claim, mut bytes) = valid_claim();
    bytes[15] ^= 0xff;
    let report = fsck_pack_range(
        &claim,
        &PackRangeAudit {
            range_bytes: Some(&bytes),
            authorized: None,
        },
    );
    assert!(
        report.has_rule(FsckRule::ExtentDigestMismatch),
        "{report:?}"
    );
}

#[test]
fn rejects_a_grant_for_an_object_the_manifest_does_not_cover() {
    let (claim, bytes) = valid_claim();
    let mut authorized: BTreeSet<_> = claim.records().iter().map(PackRecord::key).collect();
    authorized.remove(&claim.records()[1].key());

    let report = fsck_pack_range(
        &claim,
        &PackRangeAudit {
            range_bytes: Some(&bytes),
            authorized: Some(&authorized),
        },
    );
    assert!(
        report.has_rule(FsckRule::ExtentObjectNotInManifest),
        "{report:?}"
    );
}

#[test]
fn rejects_records_written_out_of_offset_order_on_the_wire() {
    let (claim, _) = valid_claim();
    let encoded = claim.encode();
    const RECORD_LEN: usize = 1 + 32 + 8 + 8 + 8 + 32;
    let head = encoded.len() - 3 * RECORD_LEN;
    let mut swapped = encoded.clone();
    swapped[head..head + RECORD_LEN]
        .copy_from_slice(&encoded[head + RECORD_LEN..head + 2 * RECORD_LEN]);
    swapped[head + RECORD_LEN..head + 2 * RECORD_LEN]
        .copy_from_slice(&encoded[head..head + RECORD_LEN]);
    assert!(
        PackRangeClaim::decode(&swapped).is_err(),
        "a decoder must reject non-offset-canonical records"
    );
}

// ── Property tests ──────────────────────────────────────────────────

prop_compose! {
    fn arb_object()(
        is_tree in any::<bool>(),
        seed in any::<[u8; 8]>(),
        size in 0u64..1_000_000,
    ) -> ManifestObject {
        ManifestObject::new(
            if is_tree { ManifestObjectKind::Tree } else { ManifestObjectKind::Blob },
            ContentHash::compute(&seed),
            size,
        )
    }
}

fn arb_objects() -> impl Strategy<Value = Vec<ManifestObject>> {
    prop::collection::vec(arb_object(), 0..250)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// encode → decode → encode is a fixed point, and the address is BLAKE3 of
    /// exactly those bytes.
    #[test]
    fn prop_encoding_round_trip_is_stable(set in arb_objects()) {
        let built = build_manifest(dedupe(set))?;
        for (hash, bytes) in &built.nodes {
            let node = ManifestNode::decode(bytes)
                .map_err(|e| TestCaseError::fail(format!("{e}")))?;
            prop_assert_eq!(&node.encode(), bytes);
            prop_assert_eq!(node.address(), *hash);
            // Decoding the re-encoding yields the same node.
            let again = ManifestNode::decode(&node.encode())
                .map_err(|e| TestCaseError::fail(format!("{e}")))?;
            prop_assert_eq!(again, node);
        }
    }

    /// The root is a pure function of the logical set, not of insertion order.
    #[test]
    fn prop_root_is_permutation_invariant(set in arb_objects(), rotate in 0usize..250) {
        let set = dedupe(set);
        let mut permuted = set.clone();
        if !permuted.is_empty() {
            let len = permuted.len();
            permuted.rotate_left(rotate % len);
            permuted.reverse();
        }
        prop_assert_eq!(build_manifest(set)?.root, build_manifest(permuted)?.root);
    }

    /// Any manifest this builder produces passes every fsck rule.
    #[test]
    fn prop_built_graphs_pass_fsck(set in arb_objects()) {
        let set = dedupe(set);
        let built = build_manifest(set.clone())?;
        let index = object_index(&set);
        let report = fsck_manifest_with(
            &built.nodes,
            &built.root,
            &FsckOptions { objects: Some(&index), report_unreachable: false },
        );
        prop_assert!(report.is_clean(), "{:?}", report);
    }

    /// Expansion recovers exactly the input set, in canonical key order.
    #[test]
    fn prop_expansion_recovers_the_set(set in arb_objects()) {
        let mut set = dedupe(set);
        let built = build_manifest(set.clone())?;
        set.sort_by_key(ManifestObject::key);
        prop_assert_eq!(expand_manifest(&built.nodes, &built.root)?, set);
    }

    /// Flipping any single byte of any node is always caught — either the
    /// digest no longer matches, or the bytes no longer decode canonically.
    #[test]
    fn prop_single_byte_corruption_is_always_caught(
        set in prop::collection::vec(arb_object(), 20..80),
        node_pick in any::<prop::sample::Index>(),
        byte_pick in any::<prop::sample::Index>(),
        mask in 1u8..=255,
    ) {
        let built = build_manifest(dedupe(set))?;
        let hashes: Vec<ContentHash> = built.nodes.keys().copied().collect();
        let victim = *node_pick.get(&hashes);

        let mut nodes = built.nodes.clone();
        let bytes = nodes.get_mut(&victim).expect("victim present");
        let at = byte_pick.index(bytes.len());
        bytes[at] ^= mask;

        let report = fsck_manifest(&nodes, &built.root);
        prop_assert!(!report.is_clean(), "corruption at byte {} of {} went unreported", at, victim);
    }
}

/// Property inputs may repeat a key; the builder rejects a *conflicting*
/// repeat, so collapse them to the first size seen before building.
fn dedupe(set: Vec<ManifestObject>) -> Vec<ManifestObject> {
    let mut seen = BTreeMap::new();
    for object in set {
        seen.entry(object.key()).or_insert(object);
    }
    seen.into_values().collect()
}
