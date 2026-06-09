// SPDX-License-Identifier: Apache-2.0
//! [`ObjectId`] — the substrate's primary object identifier type.

pub use sley_core::ObjectId;
use sley_core::ObjectFormat;

/// Well-known SHA-1 of git's canonical empty tree object.
pub const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Well-known SHA-1 of git's canonical empty blob object.
pub const EMPTY_BLOB_SHA1: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

/// Parse a 40-hex-digit SHA-1 object id.
pub fn parse_sha1_hex(hex: &str) -> sley_core::Result<ObjectId> {
    ObjectId::from_hex(ObjectFormat::Sha1, hex)
}

/// Git's canonical null object id (40 zero digits) for SHA-1 repositories.
pub fn null_sha1() -> ObjectId {
    parse_sha1_hex(&"0".repeat(40)).expect("null sha1 oid is valid")
}

/// Git's canonical empty tree object id for SHA-1 repositories.
pub fn empty_tree_sha1() -> ObjectId {
    parse_sha1_hex(EMPTY_TREE_SHA1).expect("empty tree sha1 oid is valid")
}

/// Git's canonical empty blob object id for SHA-1 repositories.
pub fn empty_blob_sha1() -> ObjectId {
    parse_sha1_hex(EMPTY_BLOB_SHA1).expect("empty blob sha1 oid is valid")
}

/// First `len` hex digits of `oid` (for short-SHA display).
pub fn short_hex(oid: &ObjectId, len: usize) -> String {
    let hex = oid.to_hex();
    hex[..len.min(hex.len())].to_string()
}