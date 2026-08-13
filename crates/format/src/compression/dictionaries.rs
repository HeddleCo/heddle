// SPDX-License-Identifier: Apache-2.0
//! Immutable dictionaries bundled with the storage-format decoder.

const TREE_STATE_V1_ID: u32 = 1;
const TREE_STATE_V1: &[u8] = include_bytes!("dictionaries/tree-state-v1.zdict");

/// A versioned zstd dictionary available to object encoders.
///
/// IDs are durable storage-format identifiers. Once an ID has shipped, its
/// bytes must never change or be removed from the decoder registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionDictionary {
    /// Dictionary v1, trained offline over serialized tree and state objects.
    TreeStateV1,
}

impl CompressionDictionary {
    pub(crate) const fn id(self) -> u32 {
        match self {
            Self::TreeStateV1 => TREE_STATE_V1_ID,
        }
    }

    pub(crate) const fn bytes(self) -> &'static [u8] {
        match self {
            Self::TreeStateV1 => TREE_STATE_V1,
        }
    }
}

pub(crate) const fn lookup(id: u32) -> Option<&'static [u8]> {
    match id {
        TREE_STATE_V1_ID => Some(TREE_STATE_V1),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_id_is_registered() {
        let dictionary = CompressionDictionary::TreeStateV1;

        assert_eq!(lookup(dictionary.id()), Some(dictionary.bytes()));
    }

    #[test]
    fn tree_state_v1_asset_is_a_trained_zstd_dictionary() {
        const ZSTD_DICTIONARY_MAGIC: [u8; 4] = [0x37, 0xA4, 0x30, 0xEC];
        const EXPECTED_BLAKE3: &str =
            "ca0ee171814fa7bf91f4c22ef7a1a4d87f7f13a4052b47774e50a8c90a85cd80";

        assert_eq!(TREE_STATE_V1.len(), 8 * 1024);
        assert_eq!(&TREE_STATE_V1[..4], &ZSTD_DICTIONARY_MAGIC);
        assert_eq!(
            blake3::hash(TREE_STATE_V1).to_hex().as_str(),
            EXPECTED_BLAKE3
        );
    }
}
