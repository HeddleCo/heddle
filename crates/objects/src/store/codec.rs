// SPDX-License-Identifier: Apache-2.0
//! Object body codecs for loose-object backends.

use heddle_format::compression::{CompressionConfig, compress, decompress, is_compressed};

use crate::{
    object::{Action, ActionId, ContentHash, State, Tree, TreeDecodeError},
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
    let hash = tree.hash();
    let serialized = rmp_serde::to_vec(tree)?;
    let data = compress(&serialized, config)?.unwrap_or(serialized);
    Ok((hash, data))
}

pub fn decode_tree(data: &[u8]) -> Result<Tree> {
    let decoded = decode_tree_body(data)?;
    decode_tree_serialized(&decoded)
}

pub fn decode_tree_serialized(data: &[u8]) -> Result<Tree> {
    Tree::decode_current_msgpack(data).map_err(|error| match error {
        TreeDecodeError::Decode(error) => HeddleError::from(error),
        TreeDecodeError::Invalid(error) => HeddleError::InvalidTreeEntry(error),
    })
}

/// Return the serialized tree body stored in a loose object, decompressing
/// only the loose-object wrapper. Migration code uses this to decode older
/// tree schemas without teaching the current [`Tree`] reader to accept them.
pub fn decode_tree_body(data: &[u8]) -> Result<Vec<u8>> {
    decode_body(data)
}

pub fn encode_state(state: &State, config: &CompressionConfig) -> Result<Vec<u8>> {
    let serialized = rmp_serde::to_vec(state)?;
    Ok(compress(&serialized, config)?.unwrap_or(serialized))
}

pub fn decode_state(data: &[u8]) -> Result<State> {
    let decoded = decode_body(data)?;
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
    fn encode_decode_tree_matches_old_recipe() {
        let blob_hash = ContentHash::compute(b"codec-tree-blob");
        let tree = Tree::from_entries(vec![TreeEntry::file("file.txt", blob_hash, false).unwrap()]);
        for config in compression_configs() {
            let serialized = rmp_serde::to_vec(&tree).unwrap();
            let expected = old_encode_raw(&serialized, &config).unwrap();
            let (hash, encoded) = encode_tree(&tree, &config).unwrap();
            assert_eq!(hash, tree.hash());
            assert_eq!(encoded, expected);
            assert_eq!(decode_tree(&encoded).unwrap(), tree);
        }
    }

    #[test]
    fn encode_decode_state_matches_old_recipe() {
        let attribution = sample_attribution();
        let state = State::new(ContentHash::compute(b"codec-tree"), vec![], attribution)
            .with_intent("codec state");
        for config in compression_configs() {
            let serialized = rmp_serde::to_vec(&state).unwrap();
            let expected = old_encode_raw(&serialized, &config).unwrap();
            let encoded = encode_state(&state, &config).unwrap();
            assert_eq!(encoded, expected);
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
