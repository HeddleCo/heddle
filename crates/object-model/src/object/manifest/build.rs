// SPDX-License-Identifier: Apache-2.0
//! Deterministic construction and expansion of a canonical manifest trie.
//!
//! Construction is a pure function of the logical object set: the same set
//! always yields the same node bytes and therefore the same root hash. Two
//! sets that differ in one object share every subtree the change does not
//! touch, so replacing one object rewrites only the old and new routes —
//! O(path depth), never O(objects).

use std::collections::{BTreeMap, BTreeSet};

use super::node::{
    MANIFEST_BRANCH_WIDTH, MANIFEST_LEAF_MAX_ENTRIES, MANIFEST_ROUTE_LEVELS, ManifestBranch,
    ManifestChild, ManifestDecodeError, ManifestKey, ManifestLeaf, ManifestNode, ManifestNodeError,
    ManifestObject,
};
use crate::object::ContentHash;

/// Read access to canonical manifest node bytes, keyed by node address.
///
/// The store is content-addressed, so an implementation may be a map, a pack
/// reader, or a network fetcher; nothing here assumes locality.
pub trait ManifestNodeSource {
    fn node_bytes(&self, hash: &ContentHash) -> Option<&[u8]>;
}

/// Optional whole-store enumeration, used only to report nodes that are
/// present but unreachable from a root.
pub trait ManifestNodeStore: ManifestNodeSource {
    fn node_hashes(&self) -> Vec<ContentHash>;
}

impl ManifestNodeSource for BTreeMap<ContentHash, Vec<u8>> {
    fn node_bytes(&self, hash: &ContentHash) -> Option<&[u8]> {
        self.get(hash).map(Vec::as_slice)
    }
}

impl ManifestNodeStore for BTreeMap<ContentHash, Vec<u8>> {
    fn node_hashes(&self) -> Vec<ContentHash> {
        self.keys().copied().collect()
    }
}

impl ManifestNodeSource for std::collections::HashMap<ContentHash, Vec<u8>> {
    fn node_bytes(&self, hash: &ContentHash) -> Option<&[u8]> {
        self.get(hash).map(Vec::as_slice)
    }
}

impl ManifestNodeStore for std::collections::HashMap<ContentHash, Vec<u8>> {
    fn node_hashes(&self) -> Vec<ContentHash> {
        self.keys().copied().collect()
    }
}

/// The output of a build: a root address plus every node byte string it
/// reaches, deduplicated by address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltManifest {
    pub root: ContentHash,
    pub nodes: BTreeMap<ContentHash, Vec<u8>>,
    pub object_count: u64,
    pub decoded_bytes: u64,
}

impl BuiltManifest {
    /// Addresses of every node in this manifest, in address order.
    pub fn node_hashes(&self) -> Vec<ContentHash> {
        self.nodes.keys().copied().collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ManifestBuildError {
    #[error(
        "object key {key:?} appears twice with different declared sizes ({first} and {second})"
    )]
    ConflictingDuplicate {
        key: ManifestKey,
        first: u64,
        second: u64,
    },
    #[error("total decoded bytes overflow u64")]
    DecodedBytesOverflow,
    #[error(transparent)]
    Node(#[from] ManifestNodeError),
}

/// Build the canonical manifest for `objects`.
///
/// Objects are deduplicated by `(kind, hash)`; an exact repeat is collapsed,
/// while a repeat that disagrees about `decoded_size` is a caller bug and is
/// rejected rather than silently resolved.
pub fn build_manifest(
    objects: impl IntoIterator<Item = ManifestObject>,
) -> Result<BuiltManifest, ManifestBuildError> {
    let mut unique: BTreeMap<ManifestKey, ManifestObject> = BTreeMap::new();
    for object in objects {
        let key = object.key();
        if let Some(existing) = unique.get(&key)
            && existing.decoded_size != object.decoded_size
        {
            return Err(ManifestBuildError::ConflictingDuplicate {
                key,
                first: existing.decoded_size,
                second: object.decoded_size,
            });
        }
        unique.insert(key, object);
    }

    let entries: Vec<ManifestObject> = unique.into_values().collect();
    let mut nodes = BTreeMap::new();
    let summary = build_level(&entries, 0, &mut nodes)?;
    Ok(BuiltManifest {
        root: summary.hash,
        nodes,
        object_count: summary.object_count,
        decoded_bytes: summary.decoded_bytes,
    })
}

struct Summary {
    hash: ContentHash,
    object_count: u64,
    decoded_bytes: u64,
}

/// Emit the subtree for `entries` at `depth`.
///
/// A leaf is used when the entries fit, or when the fixed 256-bit route is
/// exhausted and no bits remain to split on. Otherwise the entries are
/// partitioned by their 5-bit route group at this depth.
fn build_level(
    entries: &[ManifestObject],
    depth: u8,
    nodes: &mut BTreeMap<ContentHash, Vec<u8>>,
) -> Result<Summary, ManifestBuildError> {
    if entries.len() <= MANIFEST_LEAF_MAX_ENTRIES || depth >= MANIFEST_ROUTE_LEVELS {
        let leaf = ManifestLeaf::new(entries.to_vec())?;
        let object_count = leaf.object_count();
        let decoded_bytes = leaf
            .decoded_bytes()
            .ok_or(ManifestBuildError::DecodedBytesOverflow)?;
        let node = ManifestNode::Leaf(leaf);
        let hash = insert(nodes, &node);
        return Ok(Summary {
            hash,
            object_count,
            decoded_bytes,
        });
    }

    let mut buckets: Vec<Vec<ManifestObject>> = vec![Vec::new(); MANIFEST_BRANCH_WIDTH];
    for entry in entries {
        let slot = entry.key().route().group(depth);
        buckets[usize::from(slot)].push(*entry);
    }

    let mut children = Vec::new();
    let mut object_count = 0u64;
    let mut decoded_bytes = 0u64;
    for (slot, bucket) in buckets.iter().enumerate() {
        if bucket.is_empty() {
            continue;
        }
        let child = build_level(bucket, depth + 1, nodes)?;
        object_count += child.object_count;
        decoded_bytes = decoded_bytes
            .checked_add(child.decoded_bytes)
            .ok_or(ManifestBuildError::DecodedBytesOverflow)?;
        children.push(ManifestChild {
            slot: slot as u8,
            hash: child.hash,
            object_count: child.object_count,
            decoded_bytes: child.decoded_bytes,
        });
    }

    let branch = ManifestNode::Branch(ManifestBranch::new(depth, children)?);
    let hash = insert(nodes, &branch);
    Ok(Summary {
        hash,
        object_count,
        decoded_bytes,
    })
}

fn insert(nodes: &mut BTreeMap<ContentHash, Vec<u8>>, node: &ManifestNode) -> ContentHash {
    let bytes = node.encode();
    let hash = ContentHash::compute(&bytes);
    nodes.insert(hash, bytes);
    hash
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ManifestExpandError {
    #[error("manifest node {0} is missing from the node source")]
    MissingNode(ContentHash),
    #[error("manifest node {hash}: {source}")]
    Decode {
        hash: ContentHash,
        #[source]
        source: ManifestDecodeError,
    },
    #[error("manifest traversal exceeded the fixed route depth at node {0}")]
    DepthExceeded(ContentHash),
}

/// Expand a manifest root into its object set, in canonical plan-key order.
///
/// This is the differential-comparison surface: the downstream consumer
/// compares this ordered expansion against its existing membership rows.
pub fn expand_manifest<S: ManifestNodeSource + ?Sized>(
    source: &S,
    root: &ContentHash,
) -> Result<Vec<ManifestObject>, ManifestExpandError> {
    let mut objects = BTreeSet::new();
    let mut visited = BTreeSet::new();
    expand_node(source, root, 0, &mut visited, &mut objects)?;
    Ok(objects.into_iter().collect())
}

fn expand_node<S: ManifestNodeSource + ?Sized>(
    source: &S,
    hash: &ContentHash,
    depth: u8,
    visited: &mut BTreeSet<ContentHash>,
    objects: &mut BTreeSet<ManifestObject>,
) -> Result<(), ManifestExpandError> {
    if depth > MANIFEST_ROUTE_LEVELS {
        return Err(ManifestExpandError::DepthExceeded(*hash));
    }
    if !visited.insert(*hash) {
        return Ok(());
    }
    let bytes = source
        .node_bytes(hash)
        .ok_or(ManifestExpandError::MissingNode(*hash))?;
    let node = ManifestNode::decode_addressed(bytes, hash).map_err(|source| {
        ManifestExpandError::Decode {
            hash: *hash,
            source,
        }
    })?;
    match node {
        ManifestNode::Leaf(leaf) => {
            objects.extend(leaf.entries().iter().copied());
        }
        ManifestNode::Branch(branch) => {
            for child in branch.children() {
                expand_node(source, &child.hash, depth + 1, visited, objects)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::manifest::node::ManifestObjectKind;

    fn object(seed: u32) -> ManifestObject {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&seed.to_be_bytes());
        ManifestObject::new(
            if seed.is_multiple_of(3) {
                ManifestObjectKind::Tree
            } else {
                ManifestObjectKind::Blob
            },
            ContentHash::compute(&bytes),
            u64::from(seed) * 7 + 1,
        )
    }

    fn objects(count: u32) -> Vec<ManifestObject> {
        (0..count).map(object).collect()
    }

    #[test]
    fn empty_set_builds_the_canonical_empty_root() {
        let built = build_manifest([]).unwrap();
        assert_eq!(built.root, ManifestNode::empty().address());
        assert_eq!(built.object_count, 0);
        assert_eq!(built.decoded_bytes, 0);
        assert!(
            expand_manifest(&built.nodes, &built.root)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn small_sets_stay_a_single_leaf() {
        let built = build_manifest(objects(MANIFEST_LEAF_MAX_ENTRIES as u32)).unwrap();
        assert_eq!(built.nodes.len(), 1);
        let node = ManifestNode::decode(&built.nodes[&built.root]).unwrap();
        assert!(matches!(node, ManifestNode::Leaf(_)));
    }

    #[test]
    fn exceeding_the_leaf_bound_splits_into_a_branch() {
        let built = build_manifest(objects(MANIFEST_LEAF_MAX_ENTRIES as u32 + 1)).unwrap();
        let node = ManifestNode::decode(&built.nodes[&built.root]).unwrap();
        let ManifestNode::Branch(branch) = node else {
            panic!("expected a branch root");
        };
        assert_eq!(branch.depth(), 0);
        let total: u64 = branch.children().iter().map(|c| c.object_count).sum();
        assert_eq!(total, MANIFEST_LEAF_MAX_ENTRIES as u64 + 1);
    }

    #[test]
    fn build_is_order_independent_and_root_stable() {
        let mut forward = objects(200);
        let mut reversed = forward.clone();
        reversed.reverse();
        // A duplicate that agrees is collapsed rather than rejected.
        forward.push(forward[7]);

        let a = build_manifest(forward).unwrap();
        let b = build_manifest(reversed).unwrap();
        assert_eq!(a.root, b.root);
        assert_eq!(a.nodes, b.nodes);
        assert_eq!(a.object_count, 200);
    }

    #[test]
    fn expansion_round_trips_the_object_set_in_key_order() {
        let mut expected = objects(500);
        let built = build_manifest(expected.clone()).unwrap();
        let expanded = expand_manifest(&built.nodes, &built.root).unwrap();
        expected.sort_by_key(ManifestObject::key);
        assert_eq!(expanded, expected);
        assert_eq!(built.object_count, expanded.len() as u64);
        assert_eq!(
            built.decoded_bytes,
            expanded.iter().map(|o| o.decoded_size).sum::<u64>()
        );
    }

    #[test]
    fn replacing_one_object_rewrites_only_a_bounded_path() {
        let base = objects(2_000);
        let before = build_manifest(base.clone()).unwrap();

        let mut after_objects = base.clone();
        after_objects[1_234] = object(999_999);
        let after = build_manifest(after_objects).unwrap();

        assert_ne!(before.root, after.root);
        let rewritten = after
            .nodes
            .keys()
            .filter(|hash| !before.nodes.contains_key(*hash))
            .count();
        // O(path depth), not O(objects): a 2000-object trie is only a few
        // levels deep, so a single replacement must not approach node count.
        assert!(
            rewritten <= usize::from(MANIFEST_ROUTE_LEVELS),
            "rewrote {rewritten} nodes for a single object change"
        );
        assert!(
            rewritten * 8 < before.nodes.len(),
            "rewrote {rewritten} of {} nodes; structural sharing is not holding",
            before.nodes.len()
        );
    }

    #[test]
    fn an_unchanged_object_set_reuses_the_root_byte_for_byte() {
        // The context-only-state case: identical content membership must
        // produce an identical root so nothing is re-published.
        let set = objects(300);
        let a = build_manifest(set.clone()).unwrap();
        let b = build_manifest(set).unwrap();
        assert_eq!(a.root, b.root);
        assert_eq!(a.nodes[&a.root], b.nodes[&b.root]);
    }

    #[test]
    fn conflicting_duplicate_sizes_are_rejected() {
        let first = object(1);
        let second = ManifestObject::new(first.kind, first.hash, first.decoded_size + 1);
        let err = build_manifest([first, second]).unwrap_err();
        assert!(matches!(
            err,
            ManifestBuildError::ConflictingDuplicate { .. }
        ));
    }

    #[test]
    fn expansion_reports_a_missing_node_rather_than_a_partial_answer() {
        let built = build_manifest(objects(100)).unwrap();
        let mut nodes = built.nodes.clone();
        let victim = *nodes
            .keys()
            .find(|hash| **hash != built.root)
            .expect("branch root has children");
        nodes.remove(&victim);
        assert_eq!(
            expand_manifest(&nodes, &built.root).unwrap_err(),
            ManifestExpandError::MissingNode(victim)
        );
    }
}
