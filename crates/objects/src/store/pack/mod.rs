// SPDX-License-Identifier: Apache-2.0
//! Packfile management for efficient storage.
//!
//! Packfiles bundle multiple objects together with delta compression,
//! achieving 50-70% space savings for repositories with many similar objects.

mod manager;
mod pack_builder;
mod pack_index;
mod pack_reader;
mod shared;
mod streaming_builder;
pub(crate) mod varint;

#[cfg(test)]
mod pack_tests;

pub use manager::PackManager;
pub use pack_builder::PackBuilder;
pub use pack_index::PackIndex;
pub use pack_reader::PackReader;
pub use shared::{
    PACK_CHECKSUM_LEN, PackContainerSpec, PackEntryHeader, PackObjectId, PackObjectRecord,
    append_container_checksum, compress_pack_payload, decode_tagged_entry_header,
    decompress_pack_payload, encode_tagged_entry, encode_tagged_entry_parts, has_zstd_magic,
    try_decode_tagged_entry_header, verify_container, write_container_header,
};
pub use streaming_builder::StreamingPackBuilder;

/// Object type for pack entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ObjectType {
    Blob = 0,
    Tree = 1,
    State = 2,
    Action = 3,
    Delta = 4,
}

pub(crate) fn pack_container_spec() -> PackContainerSpec {
    PackContainerSpec {
        magic: b"LMPK",
        version: 2,
    }
}

impl ObjectType {
    pub(crate) fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(ObjectType::Blob),
            1 => Some(ObjectType::Tree),
            2 => Some(ObjectType::State),
            3 => Some(ObjectType::Action),
            4 => Some(ObjectType::Delta),
            _ => None,
        }
    }
}

/// Pack statistics.
#[derive(Debug, Clone, Copy)]
pub struct PackStats {
    pub object_count: u64,
    pub total_uncompressed: u64,
    pub total_compressed: u64,
    pub delta_count: u64,
    pub compression_ratio: f64,
}