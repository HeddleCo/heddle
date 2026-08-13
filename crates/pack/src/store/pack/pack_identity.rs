// SPDX-License-Identifier: Apache-2.0
//! Distinct logical and physical identities for native packs.

use std::fmt;

use super::{ObjectType, PackObjectId};

/// BLAKE3 derive-key context for the canonical logical pack inventory.
pub const PACK_LOGICAL_ID_CONTEXT: &str = "heddle.pack.logical-id.v1";
const LOGICAL_ENTRY_LEN: usize = 1 + 32 + 1 + 32;

/// Root-spool-scoped identity of the logical objects in a pack.
///
/// This identity is stable across compression, record ordering, and delta-base
/// selection. Hosted storage must scope it to the root spool: identical object
/// inventories in unrelated root spools intentionally have the same value.
///
/// The canonical inventory is the sorted multiset of fixed-width
/// `(id-kind, id, object-type, blake3(uncompressed-content))` records, prefixed
/// by its big-endian `u64` record count and hashed with
/// [`PACK_LOGICAL_ID_CONTEXT`] as the BLAKE3 derive-key context.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct PackLogicalId([u8; 32]);

impl PackLogicalId {
    /// Construct an id from its canonical 32-byte representation.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the canonical 32-byte representation.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Return the full lowercase hexadecimal representation.
    pub fn to_hex(self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }
}

/// Hash of one finalized physical pack representation.
///
/// This is exactly `blake3(pack_bytes)`, including the pack's checksum trailer.
/// It is suitable for integrity checks and physical location, but not logical
/// pack equality or cross-machine deduplication.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct PackRepresentationHash([u8; 32]);

impl PackRepresentationHash {
    /// Hash one complete finalized pack representation.
    pub fn compute(pack_bytes: &[u8]) -> Self {
        Self(*blake3::hash(pack_bytes).as_bytes())
    }

    /// Construct a representation hash from its canonical 32-byte form.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the canonical 32-byte representation.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Return the full lowercase hexadecimal representation.
    pub fn to_hex(self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct LogicalInventoryEntry([u8; LOGICAL_ENTRY_LEN]);

impl LogicalInventoryEntry {
    fn new(id: PackObjectId, object_type: ObjectType, data: &[u8]) -> Self {
        let mut canonical = [0; LOGICAL_ENTRY_LEN];
        let (id_tag, id_bytes) = match &id {
            PackObjectId::Hash(hash) => (0, hash.as_bytes()),
            PackObjectId::StateId(state_id) => (1, state_id.as_bytes()),
            PackObjectId::AnnotatedTag(hash) => (2, hash.as_bytes()),
        };
        canonical[0] = id_tag;
        canonical[1..33].copy_from_slice(id_bytes);
        canonical[33] = object_type as u8;
        canonical[34..].copy_from_slice(blake3::hash(data).as_bytes());
        Self(canonical)
    }
}

pub(super) struct LogicalIdBuilder {
    inventory: Vec<LogicalInventoryEntry>,
}

impl LogicalIdBuilder {
    pub(super) fn new() -> Self {
        Self {
            inventory: Vec::new(),
        }
    }

    pub(super) fn push(&mut self, id: PackObjectId, object_type: ObjectType, data: &[u8]) {
        self.inventory
            .push(LogicalInventoryEntry::new(id, object_type, data));
    }

    pub(super) fn finish(mut self) -> PackLogicalId {
        self.inventory.sort_unstable();

        let mut hasher = blake3::Hasher::new_derive_key(PACK_LOGICAL_ID_CONTEXT);
        let count = u64::try_from(self.inventory.len()).expect("pack inventory length fits in u64");
        hasher.update(&count.to_be_bytes());
        for entry in self.inventory {
            hasher.update(&entry.0);
        }
        PackLogicalId(*hasher.finalize().as_bytes())
    }
}

pub(super) fn logical_id_from_objects<'a>(
    objects: impl IntoIterator<Item = (PackObjectId, ObjectType, &'a [u8])>,
) -> PackLogicalId {
    let mut builder = LogicalIdBuilder::new();
    for (id, object_type, data) in objects {
        builder.push(id, object_type, data);
    }
    builder.finish()
}

fn fmt_hash(name: &str, bytes: &[u8; 32], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let hash = blake3::Hash::from_bytes(*bytes);
    write!(f, "{name}({})", &hash.to_hex().as_str()[..8])
}

impl fmt::Debug for PackLogicalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_hash("PackLogicalId", &self.0, f)
    }
}

impl fmt::Display for PackLogicalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl fmt::Debug for PackRepresentationHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_hash("PackRepresentationHash", &self.0, f)
    }
}

impl fmt::Display for PackRepresentationHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ContentHash;

    #[test]
    fn logical_id_distinguishes_annotated_tag_and_hash_id_kinds() {
        let bytes = [7; 32];
        let data = b"same object bytes";
        let hash = ContentHash::from_bytes(bytes);

        let content_id = logical_id_from_objects([(
            PackObjectId::Hash(hash),
            ObjectType::Blob,
            data.as_slice(),
        )]);
        let annotated_tag_id = logical_id_from_objects([(
            PackObjectId::AnnotatedTag(hash),
            ObjectType::Blob,
            data.as_slice(),
        )]);

        assert_ne!(content_id, annotated_tag_id);
    }
}
