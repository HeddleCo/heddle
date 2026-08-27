// SPDX-License-Identifier: Apache-2.0

use proptest::prelude::*;
use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use crate::object::{
    ContentHash, SpoolId, StateId, Tree, TreeEntry, TreeStreamError, is_canonical_tree,
};

fn name_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::char::range('a', 'z'), 1..=8)
        .prop_map(|chars| chars.into_iter().collect())
}

fn entry_strategy() -> impl Strategy<Value = TreeEntry> {
    let blob =
        (name_strategy(), any::<[u8; 8]>(), any::<bool>()).prop_map(|(name, bytes, exec)| {
            TreeEntry::file(name, ContentHash::compute(&bytes), exec).expect("blob")
        });
    let dir = (name_strategy(), any::<[u8; 8]>()).prop_map(|(name, bytes)| {
        TreeEntry::directory(name, ContentHash::compute(&bytes)).expect("dir")
    });
    let symlink = (name_strategy(), any::<[u8; 8]>()).prop_map(|(name, bytes)| {
        TreeEntry::symlink(name, ContentHash::compute(&bytes)).expect("symlink")
    });
    let gitlink = (name_strategy(), any::<[u8; 20]>()).prop_map(|(name, oid)| {
        TreeEntry::gitlink(
            name,
            GitObjectId::from_raw(GitObjectFormat::Sha1, &oid).expect("oid"),
        )
        .expect("gitlink")
    });
    let spoollink = (name_strategy(), any::<[u8; 32]>()).prop_map(|(name, state)| {
        TreeEntry::spoollink(
            name,
            SpoolId::parse("acme/child").expect("spool"),
            StateId::from_bytes(state),
        )
        .expect("spoollink")
    });
    prop_oneof![blob, dir, symlink, gitlink, spoollink]
}

fn unique_tree_strategy() -> impl Strategy<Value = Tree> {
    prop::collection::vec(entry_strategy(), 0..=12).prop_map(|entries| {
        let mut seen = std::collections::BTreeMap::new();
        for entry in entries {
            seen.insert(entry.name().to_string(), entry);
        }
        Tree::from_entries(seen.into_values().collect())
    })
}

fn error_class(error: &TreeStreamError) -> &'static str {
    match error {
        TreeStreamError::Invalid(_) => "invalid",
        TreeStreamError::UnsupportedVersion { .. } => "version",
        TreeStreamError::CursorMismatch(_) => "cursor",
        TreeStreamError::TruncatedFrame { .. } => "truncated",
        TreeStreamError::TrailingBytes { .. } => "trailing",
        TreeStreamError::UnexpectedEof { .. } => "eof",
        TreeStreamError::OversizedEntry { .. } => "oversized",
        TreeStreamError::InvalidPageLimits => "limits",
        TreeStreamError::UnverifiedRange => "unverified",
        TreeStreamError::Malformed(_) => "malformed",
        TreeStreamError::Compression(_) => "compression",
        TreeStreamError::Io(_) => "io",
        TreeStreamError::HashMismatch { .. } => "hash",
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn eager_and_streaming_decoders_match_on_valid_trees(tree in unique_tree_strategy()) {
        let bytes = tree.encode_canonical().expect("encode");
        prop_assert!(is_canonical_tree(&bytes));
        let eager = Tree::decode_canonical(&bytes).expect("eager");
        let streamed = Tree::decode_canonical_streamed(&bytes).expect("streamed");
        prop_assert_eq!(&eager, &tree);
        prop_assert_eq!(streamed, tree);
        let msgpack = rmp_serde::to_vec(&eager).expect("msgpack");
        let via_msgpack: Tree = rmp_serde::from_slice(&msgpack).expect("serde");
        prop_assert_eq!(via_msgpack, eager);
    }

    #[test]
    fn eager_and_streaming_decoders_agree_on_corrupted_payloads(
        tree in unique_tree_strategy(),
        index in 0usize..64,
        bit in 0u8..8,
    ) {
        let mut bytes = tree.encode_canonical().expect("encode");
        if bytes.is_empty() {
            return Ok(());
        }
        let index = index % bytes.len();
        bytes[index] ^= 1 << bit;
        let eager = Tree::decode_canonical(&bytes);
        let streamed = Tree::decode_canonical_streamed(&bytes);
        match (eager, streamed) {
            (Ok(left), Ok(right)) => prop_assert_eq!(left, right),
            (Err(_), Err(_)) => {}
            (left, right) => {
                prop_assert!(
                    false,
                    "eager={left:?} streamed={right:?} disagreed on success ({}/{})",
                    left.as_ref().err().map(error_class).unwrap_or("ok"),
                    right.as_ref().err().map(error_class).unwrap_or("ok"),
                );
            }
        }
    }
}
