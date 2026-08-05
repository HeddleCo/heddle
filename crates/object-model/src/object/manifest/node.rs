// SPDX-License-Identifier: Apache-2.0
//! Canonical manifest-node encoding — WPMF v1.
//!
//! A manifest node is addressed by `BLAKE3(canonical_node_bytes)`. There is
//! exactly one byte string per logical node, so the same logical membership
//! always produces the same root hash. Every integer is unsigned big-endian,
//! entries and children are strictly ordered, and decoding rejects duplicates,
//! truncation, trailing bytes, and any non-canonical spelling.
//!
//! The layouts below are byte-identical to the already-merged downstream
//! consumer (weft PR #1069, `docs/PLAN_MANIFEST_FORMAT.md`), including the
//! `WPMF` magic and the `weft-plan-manifest-key-v1` routing domain. Byte
//! compatibility with a shipped consumer outranks upstream nomenclature: this
//! module is the normative *definition* of bytes that already exist, not a
//! second, competing format.
//!
//! ```text
//! leaf:   "WPMF" | u8(version=1) | u8(tag=0) | u16(count)
//!                | count  * ( u8(kind) | [u8;32](hash) | u64(decoded_size) )
//! branch: "WPMF" | u8(version=1) | u8(tag=1) | u8(depth) | u32(bitmap)
//!                | popcnt * ( [u8;32](child_hash) | u64(object_count)
//!                                                 | u64(decoded_bytes) )
//! ```

use std::fmt;

use crate::object::ContentHash;

/// Magic prefix on every canonical manifest node.
pub const MANIFEST_NODE_MAGIC: [u8; 4] = *b"WPMF";
/// The only manifest format version this binary reads or writes.
pub const MANIFEST_FORMAT_VERSION: u8 = 1;
/// Domain separator for the trie route. Hashed with the plan key to derive the
/// 256 routing bits.
pub const MANIFEST_ROUTE_DOMAIN: &[u8] = b"weft-plan-manifest-key-v1";
/// Routing bits consumed per trie level (32-way branching).
pub const MANIFEST_ROUTE_BITS: u8 = 5;
/// Branch fan-out — one bitmap slot per possible 5-bit route group.
pub const MANIFEST_BRANCH_WIDTH: usize = 32;
/// Levels available before the fixed 256-bit route is exhausted
/// (`ceil(256 / 5)`). A leaf at this depth may exceed
/// [`MANIFEST_LEAF_MAX_ENTRIES`] because no routing bits remain to split it.
pub const MANIFEST_ROUTE_LEVELS: u8 = 52;
/// Entries a leaf may hold before it must split into a branch, unless the
/// route is already exhausted.
pub const MANIFEST_LEAF_MAX_ENTRIES: usize = 16;

const TAG_LEAF: u8 = 0;
const TAG_BRANCH: u8 = 1;

const LEAF_HEADER_LEN: usize = 4 + 1 + 1 + 2;
const LEAF_ENTRY_LEN: usize = 1 + 32 + 8;
const BRANCH_HEADER_LEN: usize = 4 + 1 + 1 + 1 + 4;
const BRANCH_CHILD_LEN: usize = 32 + 8 + 8;

// ── ManifestObjectKind ──────────────────────────────────────────────

/// The object types a manifest leaf may name.
///
/// Deliberately narrow: a manifest carries *content* membership. Owner
/// identity (State / StateAttachment) lives in the binding, outside the shared
/// content root, so a context-only state reuses its parent root byte-for-byte.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ManifestObjectKind {
    Blob = 0,
    Tree = 1,
}

impl ManifestObjectKind {
    pub fn to_byte(self) -> u8 {
        self as u8
    }

    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Blob),
            1 => Some(Self::Tree),
            _ => None,
        }
    }
}

impl fmt::Display for ManifestObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
        })
    }
}

// ── ManifestKey / ManifestRoute ─────────────────────────────────────

/// The trie key: `(kind, object_hash)`. Ordering is the canonical plan order —
/// kind first, then the 32 hash bytes lexicographically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManifestKey {
    pub kind: ManifestObjectKind,
    pub hash: ContentHash,
}

impl ManifestKey {
    pub fn new(kind: ManifestObjectKind, hash: ContentHash) -> Self {
        Self { kind, hash }
    }

    /// The 256 routing bits for this key.
    pub fn route(&self) -> ManifestRoute {
        let mut hasher = blake3::Hasher::new();
        hasher.update(MANIFEST_ROUTE_DOMAIN);
        hasher.update(&[self.kind.to_byte()]);
        hasher.update(self.hash.as_bytes());
        ManifestRoute(hasher.finalize().into())
    }
}

/// The fixed 256-bit route derived from a [`ManifestKey`], read as successive
/// 5-bit groups, most-significant bit first.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ManifestRoute([u8; 32]);

impl ManifestRoute {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The bitmap slot this key occupies at `level`.
    ///
    /// Levels past the 256th bit are zero-padded on the right, so the final
    /// level (51) carries a single real bit. Levels at or beyond
    /// [`MANIFEST_ROUTE_LEVELS`] are exhausted and always return 0; callers
    /// must stop splitting there rather than loop forever.
    pub fn group(&self, level: u8) -> u8 {
        let start = usize::from(level) * usize::from(MANIFEST_ROUTE_BITS);
        let mut value = 0u8;
        for offset in 0..usize::from(MANIFEST_ROUTE_BITS) {
            let bit_index = start + offset;
            let bit = if bit_index >= 256 {
                0
            } else {
                (self.0[bit_index / 8] >> (7 - (bit_index % 8))) & 1
            };
            value = (value << 1) | bit;
        }
        value
    }
}

impl fmt::Debug for ManifestRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ManifestRoute({})", hex::encode(&self.0[..8]))
    }
}

// ── ManifestObject ──────────────────────────────────────────────────

/// One immutable content object named by a manifest leaf.
///
/// Only logical facts appear here. Pack id, storage key, offset, encoded
/// length, encoded digest, ETag, audience, and current head are mutable
/// control-plane facts and are resolved *after* authorization — see
/// [`super::extent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManifestObject {
    pub kind: ManifestObjectKind,
    pub hash: ContentHash,
    pub decoded_size: u64,
}

impl ManifestObject {
    pub fn new(kind: ManifestObjectKind, hash: ContentHash, decoded_size: u64) -> Self {
        Self {
            kind,
            hash,
            decoded_size,
        }
    }

    pub fn key(&self) -> ManifestKey {
        ManifestKey::new(self.kind, self.hash)
    }
}

// ── ManifestChild ───────────────────────────────────────────────────

/// A branch's reference to one child subtree, with the summary that lets a
/// planner size a subtree without descending into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestChild {
    /// Bitmap slot (`0..32`) — the child's 5-bit route group at the parent's
    /// depth.
    pub slot: u8,
    pub hash: ContentHash,
    pub object_count: u64,
    pub decoded_bytes: u64,
}

// ── ManifestNode ────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestNode {
    Leaf(ManifestLeaf),
    Branch(ManifestBranch),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestLeaf {
    entries: Vec<ManifestObject>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestBranch {
    depth: u8,
    children: Vec<ManifestChild>,
}

impl ManifestLeaf {
    /// Build a leaf, sorting entries into canonical key order.
    ///
    /// Returns [`ManifestNodeError::DuplicateObjectKey`] if two entries share a
    /// `(kind, hash)` key, even when their declared sizes agree: the canonical
    /// form names each object exactly once.
    pub fn new(mut entries: Vec<ManifestObject>) -> Result<Self, ManifestNodeError> {
        entries.sort_by_key(ManifestObject::key);
        if let Some(window) = entries.windows(2).find(|w| w[0].key() == w[1].key()) {
            return Err(ManifestNodeError::DuplicateObjectKey(window[0].key()));
        }
        if u16::try_from(entries.len()).is_err() {
            return Err(ManifestNodeError::LeafCountOverflow(entries.len()));
        }
        Ok(Self { entries })
    }

    pub fn entries(&self) -> &[ManifestObject] {
        &self.entries
    }

    pub fn object_count(&self) -> u64 {
        self.entries.len() as u64
    }

    /// Total decoded bytes named by this leaf, or `None` on `u64` overflow.
    pub fn decoded_bytes(&self) -> Option<u64> {
        self.entries
            .iter()
            .try_fold(0u64, |acc, entry| acc.checked_add(entry.decoded_size))
    }
}

impl ManifestBranch {
    /// Build a branch, sorting children into ascending bitmap-slot order.
    pub fn new(depth: u8, mut children: Vec<ManifestChild>) -> Result<Self, ManifestNodeError> {
        children.sort_by_key(|child| child.slot);
        if children.is_empty() {
            return Err(ManifestNodeError::EmptyBranchBitmap);
        }
        if let Some(child) = children
            .iter()
            .find(|child| usize::from(child.slot) >= MANIFEST_BRANCH_WIDTH)
        {
            return Err(ManifestNodeError::SlotOutOfRange(child.slot));
        }
        if let Some(window) = children.windows(2).find(|w| w[0].slot == w[1].slot) {
            return Err(ManifestNodeError::DuplicateBranchSlot(window[0].slot));
        }
        Ok(Self { depth, children })
    }

    pub fn depth(&self) -> u8 {
        self.depth
    }

    pub fn children(&self) -> &[ManifestChild] {
        &self.children
    }

    pub fn bitmap(&self) -> u32 {
        self.children
            .iter()
            .fold(0u32, |acc, child| acc | (1u32 << child.slot))
    }

    pub fn child_at(&self, slot: u8) -> Option<&ManifestChild> {
        self.children.iter().find(|child| child.slot == slot)
    }
}

impl ManifestNode {
    /// The canonical empty-set root: a leaf with no entries.
    pub fn empty() -> Self {
        Self::Leaf(ManifestLeaf {
            entries: Vec::new(),
        })
    }

    /// Encode to the single canonical byte string for this logical node.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Leaf(leaf) => {
                let mut out =
                    Vec::with_capacity(LEAF_HEADER_LEN + leaf.entries.len() * LEAF_ENTRY_LEN);
                out.extend_from_slice(&MANIFEST_NODE_MAGIC);
                out.push(MANIFEST_FORMAT_VERSION);
                out.push(TAG_LEAF);
                out.extend_from_slice(&(leaf.entries.len() as u16).to_be_bytes());
                for entry in &leaf.entries {
                    out.push(entry.kind.to_byte());
                    out.extend_from_slice(entry.hash.as_bytes());
                    out.extend_from_slice(&entry.decoded_size.to_be_bytes());
                }
                out
            }
            Self::Branch(branch) => {
                let mut out = Vec::with_capacity(
                    BRANCH_HEADER_LEN + branch.children.len() * BRANCH_CHILD_LEN,
                );
                out.extend_from_slice(&MANIFEST_NODE_MAGIC);
                out.push(MANIFEST_FORMAT_VERSION);
                out.push(TAG_BRANCH);
                out.push(branch.depth);
                out.extend_from_slice(&branch.bitmap().to_be_bytes());
                for child in &branch.children {
                    out.extend_from_slice(child.hash.as_bytes());
                    out.extend_from_slice(&child.object_count.to_be_bytes());
                    out.extend_from_slice(&child.decoded_bytes.to_be_bytes());
                }
                out
            }
        }
    }

    /// The node's content address — `BLAKE3` of its canonical bytes.
    pub fn address(&self) -> ContentHash {
        ContentHash::compute(&self.encode())
    }

    /// Decode strictly.
    ///
    /// Rejects a bad magic, an unknown version or tag, an unknown object kind,
    /// truncation, trailing bytes, out-of-order or duplicated entries, an empty
    /// branch bitmap, and — as a final backstop — any input whose re-encoding
    /// differs from the input. A node that decodes here is canonical, so its
    /// address is meaningful.
    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestDecodeError> {
        let node = Self::decode_inner(bytes)?;
        if node.encode() != bytes {
            return Err(ManifestDecodeError::NonCanonicalEncoding);
        }
        Ok(node)
    }

    /// Decode and verify the node addresses to `expected`.
    pub fn decode_addressed(
        bytes: &[u8],
        expected: &ContentHash,
    ) -> Result<Self, ManifestDecodeError> {
        let node = Self::decode(bytes)?;
        let actual = ContentHash::compute(bytes);
        if actual != *expected {
            return Err(ManifestDecodeError::AddressMismatch {
                expected: *expected,
                actual,
            });
        }
        Ok(node)
    }

    fn decode_inner(bytes: &[u8]) -> Result<Self, ManifestDecodeError> {
        let mut reader = Reader::new(bytes);
        if reader.take(4)? != MANIFEST_NODE_MAGIC {
            return Err(ManifestDecodeError::BadMagic);
        }
        let version = reader.u8()?;
        if version != MANIFEST_FORMAT_VERSION {
            return Err(ManifestDecodeError::UnsupportedVersion(version));
        }
        let tag = reader.u8()?;
        let node = match tag {
            TAG_LEAF => {
                let count = usize::from(reader.u16()?);
                let mut entries = Vec::with_capacity(count.min(1024));
                let mut previous: Option<ManifestKey> = None;
                for _ in 0..count {
                    let kind_byte = reader.u8()?;
                    let kind = ManifestObjectKind::from_byte(kind_byte)
                        .ok_or(ManifestDecodeError::UnknownObjectKind(kind_byte))?;
                    let hash = ContentHash::from_bytes(reader.hash()?);
                    let decoded_size = reader.u64()?;
                    let key = ManifestKey::new(kind, hash);
                    match previous {
                        Some(prev) if prev == key => {
                            return Err(ManifestDecodeError::DuplicateObjectKey(key));
                        }
                        Some(prev) if prev > key => {
                            return Err(ManifestDecodeError::EntriesOutOfOrder);
                        }
                        _ => {}
                    }
                    previous = Some(key);
                    entries.push(ManifestObject::new(kind, hash, decoded_size));
                }
                Self::Leaf(ManifestLeaf { entries })
            }
            TAG_BRANCH => {
                let depth = reader.u8()?;
                let bitmap = reader.u32()?;
                if bitmap == 0 {
                    return Err(ManifestDecodeError::EmptyBranchBitmap);
                }
                let mut children = Vec::with_capacity(bitmap.count_ones() as usize);
                for slot in 0..MANIFEST_BRANCH_WIDTH as u8 {
                    if bitmap & (1u32 << slot) == 0 {
                        continue;
                    }
                    let hash = ContentHash::from_bytes(reader.hash()?);
                    let object_count = reader.u64()?;
                    let decoded_bytes = reader.u64()?;
                    children.push(ManifestChild {
                        slot,
                        hash,
                        object_count,
                        decoded_bytes,
                    });
                }
                Self::Branch(ManifestBranch { depth, children })
            }
            other => return Err(ManifestDecodeError::UnknownNodeTag(other)),
        };
        if !reader.is_exhausted() {
            return Err(ManifestDecodeError::TrailingBytes);
        }
        Ok(node)
    }

    /// Objects named directly by this node (empty for a branch).
    pub fn leaf_entries(&self) -> &[ManifestObject] {
        match self {
            Self::Leaf(leaf) => leaf.entries(),
            Self::Branch(_) => &[],
        }
    }
}

// ── Reader ──────────────────────────────────────────────────────────

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ManifestDecodeError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(ManifestDecodeError::Truncated)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(ManifestDecodeError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ManifestDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ManifestDecodeError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, ManifestDecodeError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, ManifestDecodeError> {
        let bytes = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(arr))
    }

    fn hash(&mut self) -> Result<[u8; 32], ManifestDecodeError> {
        let bytes = self.take(32)?;
        let mut arr = [0u8; 32];
        arr.copy_from_slice(bytes);
        Ok(arr)
    }

    fn is_exhausted(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

// ── Errors ──────────────────────────────────────────────────────────

/// Rejected while *constructing* a node in memory.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ManifestNodeError {
    #[error("duplicate manifest object key {0:?}")]
    DuplicateObjectKey(ManifestKey),
    #[error("leaf holds {0} entries; the canonical count field is a u16")]
    LeafCountOverflow(usize),
    #[error("branch bitmap is empty; the canonical empty set is the empty leaf")]
    EmptyBranchBitmap,
    #[error("branch slot {0} is outside 0..32")]
    SlotOutOfRange(u8),
    #[error("duplicate branch slot {0}")]
    DuplicateBranchSlot(u8),
}

/// Rejected while *decoding* node bytes. Each variant is a distinct corruption
/// class and maps to a named fsck rule via
/// [`ManifestDecodeError::fsck_rule`](super::fsck::FsckRule).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ManifestDecodeError {
    #[error("node does not start with the WPMF magic")]
    BadMagic,
    #[error("unsupported manifest format version {0}")]
    UnsupportedVersion(u8),
    #[error("unknown manifest node tag {0}")]
    UnknownNodeTag(u8),
    #[error("unknown manifest object kind {0}")]
    UnknownObjectKind(u8),
    #[error("node bytes are truncated")]
    Truncated,
    #[error("node has trailing bytes after its declared content")]
    TrailingBytes,
    #[error("leaf entries are not strictly ascending by (kind, hash)")]
    EntriesOutOfOrder,
    #[error("leaf names object key {0:?} more than once")]
    DuplicateObjectKey(ManifestKey),
    #[error("branch bitmap is empty; the canonical empty set is the empty leaf")]
    EmptyBranchBitmap,
    #[error("node bytes are a non-canonical spelling of their own content")]
    NonCanonicalEncoding,
    #[error("node bytes hash to {actual} but were addressed as {expected}")]
    AddressMismatch {
        expected: ContentHash,
        actual: ContentHash,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u8) -> ContentHash {
        ContentHash::from_bytes([seed; 32])
    }

    fn object(seed: u8, size: u64) -> ManifestObject {
        ManifestObject::new(ManifestObjectKind::Blob, hash(seed), size)
    }

    #[test]
    fn empty_leaf_is_the_canonical_empty_root() {
        let encoded = ManifestNode::empty().encode();
        assert_eq!(encoded, b"WPMF\x01\x00\x00\x00");
        assert_eq!(
            ManifestNode::decode(&encoded).unwrap(),
            ManifestNode::empty()
        );
    }

    #[test]
    fn leaf_layout_is_byte_exact() {
        let leaf = ManifestNode::Leaf(ManifestLeaf::new(vec![object(1, 0x0102)]).unwrap());
        let encoded = leaf.encode();
        assert_eq!(&encoded[..4], b"WPMF");
        assert_eq!(encoded[4], MANIFEST_FORMAT_VERSION);
        assert_eq!(encoded[5], TAG_LEAF);
        assert_eq!(&encoded[6..8], &1u16.to_be_bytes());
        assert_eq!(encoded[8], 0); // Blob
        assert_eq!(&encoded[9..41], &[1u8; 32]);
        assert_eq!(&encoded[41..49], &0x0102u64.to_be_bytes());
        assert_eq!(encoded.len(), LEAF_HEADER_LEN + LEAF_ENTRY_LEN);
    }

    #[test]
    fn branch_layout_is_byte_exact() {
        let branch = ManifestNode::Branch(
            ManifestBranch::new(
                3,
                vec![
                    ManifestChild {
                        slot: 5,
                        hash: hash(9),
                        object_count: 17,
                        decoded_bytes: 40,
                    },
                    ManifestChild {
                        slot: 1,
                        hash: hash(8),
                        object_count: 2,
                        decoded_bytes: 6,
                    },
                ],
            )
            .unwrap(),
        );
        let encoded = branch.encode();
        assert_eq!(encoded[5], TAG_BRANCH);
        assert_eq!(encoded[6], 3);
        assert_eq!(&encoded[7..11], &0b100010u32.to_be_bytes());
        // Children follow occupied bitmap slots in ascending order, so slot 1
        // precedes slot 5 regardless of construction order.
        assert_eq!(&encoded[11..43], &[8u8; 32]);
        assert_eq!(encoded.len(), BRANCH_HEADER_LEN + 2 * BRANCH_CHILD_LEN);
        assert_eq!(ManifestNode::decode(&encoded).unwrap(), branch);
    }

    #[test]
    fn decode_rejects_each_corruption_class() {
        let good = ManifestNode::Leaf(ManifestLeaf::new(vec![object(1, 1), object(2, 2)]).unwrap())
            .encode();

        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert_eq!(
            ManifestNode::decode(&bad_magic).unwrap_err(),
            ManifestDecodeError::BadMagic
        );

        let mut bad_version = good.clone();
        bad_version[4] = 2;
        assert_eq!(
            ManifestNode::decode(&bad_version).unwrap_err(),
            ManifestDecodeError::UnsupportedVersion(2)
        );

        let mut bad_tag = good.clone();
        bad_tag[5] = 7;
        assert_eq!(
            ManifestNode::decode(&bad_tag).unwrap_err(),
            ManifestDecodeError::UnknownNodeTag(7)
        );

        let mut bad_kind = good.clone();
        bad_kind[8] = 9;
        assert_eq!(
            ManifestNode::decode(&bad_kind).unwrap_err(),
            ManifestDecodeError::UnknownObjectKind(9)
        );

        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(
            ManifestNode::decode(&trailing).unwrap_err(),
            ManifestDecodeError::TrailingBytes
        );

        let truncated = &good[..good.len() - 1];
        assert_eq!(
            ManifestNode::decode(truncated).unwrap_err(),
            ManifestDecodeError::Truncated
        );

        // Swap the two entries so they descend rather than ascend.
        let mut swapped = good.clone();
        let (a, b) = (8, 8 + LEAF_ENTRY_LEN);
        let entry_a = good[a..a + LEAF_ENTRY_LEN].to_vec();
        let entry_b = good[b..b + LEAF_ENTRY_LEN].to_vec();
        swapped[a..a + LEAF_ENTRY_LEN].copy_from_slice(&entry_b);
        swapped[b..b + LEAF_ENTRY_LEN].copy_from_slice(&entry_a);
        assert_eq!(
            ManifestNode::decode(&swapped).unwrap_err(),
            ManifestDecodeError::EntriesOutOfOrder
        );

        // Duplicate the first entry over the second.
        let mut duplicated = good.clone();
        duplicated[b..b + LEAF_ENTRY_LEN].copy_from_slice(&entry_a);
        assert_eq!(
            ManifestNode::decode(&duplicated).unwrap_err(),
            ManifestDecodeError::DuplicateObjectKey(ManifestKey::new(
                ManifestObjectKind::Blob,
                hash(1)
            ))
        );
    }

    #[test]
    fn decode_rejects_empty_branch_bitmap() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MANIFEST_NODE_MAGIC);
        bytes.push(MANIFEST_FORMAT_VERSION);
        bytes.push(TAG_BRANCH);
        bytes.push(0);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(
            ManifestNode::decode(&bytes).unwrap_err(),
            ManifestDecodeError::EmptyBranchBitmap
        );
    }

    #[test]
    fn decode_addressed_rejects_a_wrong_address() {
        let node = ManifestNode::Leaf(ManifestLeaf::new(vec![object(1, 1)]).unwrap());
        let bytes = node.encode();
        let err = ManifestNode::decode_addressed(&bytes, &hash(0xff)).unwrap_err();
        assert!(matches!(err, ManifestDecodeError::AddressMismatch { .. }));
        assert!(ManifestNode::decode_addressed(&bytes, &node.address()).is_ok());
    }

    #[test]
    fn route_groups_read_five_bits_msb_first() {
        let route = ManifestRoute([0b1010_1010; 32]);
        assert_eq!(route.group(0), 0b10101);
        assert_eq!(route.group(1), 0b01010);
        // The final level starts at bit 255 — the last bit of the last byte —
        // and the remaining four are zero padding. Here that bit is 0.
        assert_eq!(route.group(MANIFEST_ROUTE_LEVELS - 1), 0b00000);
        // Same position, last bit set: the one real bit lands in the high
        // position of the group and the padding stays zero.
        assert_eq!(
            ManifestRoute([0b1010_1011; 32]).group(MANIFEST_ROUTE_LEVELS - 1),
            0b10000
        );
        // Past the route, every group is 0 — splitting must stop, not spin.
        assert_eq!(route.group(MANIFEST_ROUTE_LEVELS), 0);
    }

    #[test]
    fn route_is_domain_separated_from_the_raw_hash() {
        let key = ManifestKey::new(ManifestObjectKind::Blob, hash(3));
        assert_ne!(key.route().as_bytes(), hash(3).as_bytes());
        // Kind participates: the same hash under a different kind routes apart.
        let other = ManifestKey::new(ManifestObjectKind::Tree, hash(3));
        assert_ne!(key.route().as_bytes(), other.route().as_bytes());
    }

    #[test]
    fn constructing_a_leaf_rejects_duplicate_keys() {
        let err = ManifestLeaf::new(vec![object(1, 1), object(1, 1)]).unwrap_err();
        assert!(matches!(err, ManifestNodeError::DuplicateObjectKey(_)));
    }
}
