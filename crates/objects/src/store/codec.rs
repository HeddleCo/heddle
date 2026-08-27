// SPDX-License-Identifier: Apache-2.0
//! Object body codecs for loose-object backends.

use heddle_format::compression::{
    CompressionConfig, CompressionDictionary, compress, compress_with_dictionary, decompress,
    decompress_with_dictionary, is_compressed,
};

use crate::{
    object::{Action, ActionId, ContentHash, State, Tree},
    store::{HeddleError, Result},
};

pub fn encode_blob_content(content: &[u8], config: &CompressionConfig) -> Result<Vec<u8>> {
    Ok(compress(content, config)?.unwrap_or_else(|| content.to_vec()))
}

pub fn decode_blob_content(data: &[u8]) -> Result<Vec<u8>> {
    if is_compressed(data) {
        Ok(decompress(data)?)
    } else {
        Ok(data.to_vec())
    }
}

pub fn encode_tree(tree: &Tree, config: &CompressionConfig) -> Result<(ContentHash, Vec<u8>)> {
    let encoded = if config.enabled && tree.len() >= crate::object::TREE_BLOCK_MIN_ENTRIES {
        tree.encode_canonical_blocked(config.level, config.min_size)?
    } else {
        tree.encode_canonical()?
    };
    Ok((tree.hash(), encoded))
}

pub fn decode_tree(data: &[u8]) -> Result<Tree> {
    let decoded = decode_tree_body(data)?;
    decode_tree_serialized(&decoded)
}

pub fn decode_tree_serialized(data: &[u8]) -> Result<Tree> {
    Tree::decode_canonical(data).map_err(HeddleError::from)
}

/// Return the serialized tree body stored in a loose object, decompressing
/// only the loose-object wrapper. Migration code uses this to decode older
/// tree schemas without teaching the current [`Tree`] reader to accept them.
pub fn decode_tree_body(data: &[u8]) -> Result<Vec<u8>> {
    Ok(decompress_with_dictionary(data)?)
}

pub fn encode_state(state: &State, config: &CompressionConfig) -> Result<Vec<u8>> {
    let serialized = rmp_serde::to_vec(state)?;
    Ok(
        compress_with_dictionary(&serialized, config, CompressionDictionary::TreeStateV1)?
            .unwrap_or(serialized),
    )
}

pub fn decode_state(data: &[u8]) -> Result<State> {
    let decoded = decompress_with_dictionary(data)?;
    let mut state: State = rmp_serde::from_slice(&decoded)?;
    state.state_id = state.id();
    Ok(state)
}

pub fn encode_action(
    action: &mut Action,
    config: &CompressionConfig,
) -> Result<(ActionId, Vec<u8>)> {
    let id = action.id();
    let serialized = rmp_serde::to_vec(action)?;
    let data = compress(&serialized, config)?.unwrap_or(serialized);
    Ok((id, data))
}

pub fn decode_action(data: &[u8]) -> Result<Action> {
    let decoded = decode_body(data)?;
    Ok(rmp_serde::from_slice(&decoded)?)
}

fn decode_body(data: &[u8]) -> Result<Vec<u8>> {
    if is_compressed(data) {
        Ok(decompress(data)?)
    } else {
        Ok(data.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Attribution, Operation, Principal, StateId, TreeEntry};

    #[test]
    fn encode_decode_blob_content_matches_old_recipe() {
        let content = b"codec blob content ".repeat(64);
        for config in compression_configs() {
            let expected = old_encode_raw(&content, &config).unwrap();
            let encoded = encode_blob_content(&content, &config).unwrap();
            assert_eq!(encoded, expected);
            assert_eq!(decode_blob_content(&encoded).unwrap(), content);
        }
    }

    #[test]
    fn encode_decode_tree() {
        let blob_hash = ContentHash::compute(b"codec-tree-blob");
        let tree = Tree::from_entries(vec![TreeEntry::file("file.txt", blob_hash, false).unwrap()]);
        for config in compression_configs() {
            let (hash, encoded) = encode_tree(&tree, &config).unwrap();
            assert_eq!(hash, tree.hash());
            assert_eq!(decode_tree(&encoded).unwrap(), tree);
        }
    }

    #[test]
    #[cfg(feature = "zstd")]
    fn tree_and_state_use_versioned_dictionary_frames() {
        let tree = Tree::from_entries(
            (0..24)
                .map(|index| {
                    TreeEntry::file(
                        format!("module_{index:02}.rs"),
                        ContentHash::compute(format!("blob-{index}").as_bytes()),
                        false,
                    )
                    .unwrap()
                })
                .collect(),
        );
        let state = State::new(
            tree.hash(),
            vec![StateId::from_bytes([7; 32])],
            sample_attribution(),
        )
        .with_intent("dictionary frame verification ".repeat(32));

        let (_, encoded_tree) = encode_tree(&tree, &CompressionConfig::default()).unwrap();
        let encoded_state = encode_state(&state, &CompressionConfig::default()).unwrap();

        assert_eq!(
            encoded_tree[4],
            crate::object::TREE_BLOCK_ENCODING_VERSION,
            "eligible trees use seekable block-compressed HTR4"
        );
        assert_eq!(&encoded_state[9..13], &1_u32.to_be_bytes());
    }

    #[test]
    #[cfg(feature = "zstd")]
    fn tree_and_state_corpus_roundtrips() {
        let config = CompressionConfig::default();

        for revision in 0..64 {
            let tree = Tree::from_entries(
                (0..32)
                    .map(|entry| {
                        TreeEntry::file(
                            format!("module_{entry:02}.rs"),
                            ContentHash::compute(
                                format!("revision-{revision}-blob-{entry}").as_bytes(),
                            ),
                            entry % 11 == 0,
                        )
                        .unwrap()
                    })
                    .collect(),
            );
            let (_, encoded_tree) = encode_tree(&tree, &config).unwrap();
            assert_eq!(decode_tree_serialized(&encoded_tree).unwrap(), tree);

            let state = State::new(
                tree.hash(),
                vec![StateId::from_bytes([revision; 32])],
                sample_attribution(),
            )
            .with_intent(format!(
                "Update the representative tree/state corpus at revision {revision}. {}",
                "Preserve byte-identical object bodies. ".repeat(12)
            ));
            let serialized_state = rmp_serde::to_vec(&state).unwrap();
            let encoded_state = encode_state(&state, &config).unwrap();
            assert_eq!(
                decompress_with_dictionary(&encoded_state).unwrap(),
                serialized_state
            );
        }
    }

    #[test]
    #[cfg(feature = "zstd")]
    fn small_tree_and_disabled_compression_use_raw_htr4() {
        let tree = Tree::from_entries(
            (0..crate::object::TREE_BLOCK_MIN_ENTRIES - 1)
                .map(|index| {
                    TreeEntry::file(
                        format!("module_{index:02}.rs"),
                        ContentHash::compute(format!("blob-{index}").as_bytes()),
                        false,
                    )
                    .unwrap()
                })
                .collect(),
        );
        let (_, small) = encode_tree(&tree, &CompressionConfig::default()).unwrap();
        assert_eq!(small[4], crate::object::TREE_ENCODING_VERSION);

        let large = Tree::from_entries(
            (0..64)
                .map(|index| {
                    TreeEntry::file(
                        format!("module_{index:02}.rs"),
                        ContentHash::compute(format!("large-blob-{index}").as_bytes()),
                        false,
                    )
                    .unwrap()
                })
                .collect(),
        );
        let (_, disabled) = encode_tree(&large, &CompressionConfig::disabled()).unwrap();
        assert_eq!(disabled[4], crate::object::TREE_ENCODING_VERSION);
    }

    #[test]
    fn encode_decode_state() {
        let attribution = sample_attribution();
        let state = State::new(ContentHash::compute(b"codec-tree"), vec![], attribution)
            .with_intent("codec state");
        for config in compression_configs() {
            let encoded = encode_state(&state, &config).unwrap();
            assert_eq!(decode_state(&encoded).unwrap(), state);
        }
    }

    #[test]
    fn encode_decode_action_matches_old_recipe() {
        let attribution = sample_attribution();
        for config in compression_configs() {
            let mut action = Action::new(
                None,
                StateId::from_bytes([1; 32]),
                Operation::Snapshot,
                "codec action",
                attribution.clone(),
            );
            let id = action.id();
            let serialized = rmp_serde::to_vec(&action).unwrap();
            let expected = old_encode_raw(&serialized, &config).unwrap();

            let (encoded_id, encoded) = encode_action(&mut action, &config).unwrap();
            assert_eq!(encoded_id, id);
            assert_eq!(encoded, expected);

            let decoded = decode_action(&encoded).unwrap();
            assert_eq!(decoded.compute_id(), id);
            assert_eq!(decoded.from_state, action.from_state);
            assert_eq!(decoded.to_state, action.to_state);
            assert_eq!(decoded.operation, action.operation);
            assert_eq!(decoded.description, action.description);
            assert_eq!(decoded.semantic_changes, action.semantic_changes);
            assert_eq!(decoded.attribution, action.attribution);
            assert_eq!(decoded.timestamp, action.timestamp);
        }
    }

    fn old_encode_raw(data: &[u8], config: &CompressionConfig) -> Result<Vec<u8>> {
        Ok(compress(data, config)?.unwrap_or_else(|| data.to_vec()))
    }

    fn compression_configs() -> Vec<CompressionConfig> {
        #[cfg(feature = "zstd")]
        {
            vec![
                CompressionConfig::default(),
                CompressionConfig::disabled(),
                CompressionConfig {
                    enabled: true,
                    level: 9,
                    min_size: 0,
                    max_delta_size: CompressionConfig::default().max_delta_size,
                },
            ]
        }
        #[cfg(not(feature = "zstd"))]
        {
            vec![CompressionConfig::default(), CompressionConfig::disabled()]
        }
    }

    fn sample_attribution() -> Attribution {
        Attribution::human(Principal::new("Codec Test", "codec@example.com"))
    }
}
