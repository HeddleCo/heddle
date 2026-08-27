// SPDX-License-Identifier: Apache-2.0
//! Object body codecs for loose-object backends.

use heddle_format::compression::{
    CompressionConfig, CompressionDictionary, compress, compress_with_dictionary, decompress,
    decompress_with_dictionary, is_compressed,
};

use crate::{
    object::{
        Action, ActionId, ContentHash, State, TREE_DELTA_ANCHOR_INTERVAL, TREE_DELTA_MAX_OPS, Tree,
        decode_tree_delta, decode_tree_delta_header, encode_tree_delta, is_canonical_tree,
        is_delta_tree, is_lean_tree, tree_delta,
    },
    store::{HeddleError, Result},
};

/// Store metadata needed to keep a future delta in the same bounded epoch.
/// Losing this hint is safe: the next write falls back to a fresh HLR1 anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeLineage {
    pub anchor: ContentHash,
    pub depth: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeEncodingKind {
    Lean,
    Delta {
        anchor: ContentHash,
        depth: u8,
        op_count: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedTree {
    pub hash: ContentHash,
    pub data: Vec<u8>,
    pub kind: TreeEncodingKind,
}

/// Materialized anchor information inherited from the immediate parent.
pub struct TreeDeltaBase<'a> {
    pub anchor_id: ContentHash,
    pub anchor: &'a Tree,
    /// Delta descendants between `anchor` and the immediate parent.
    pub parent_depth: u8,
}

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

pub fn encode_tree(tree: &Tree, _config: &CompressionConfig) -> Result<(ContentHash, Vec<u8>)> {
    let encoded = encode_tree_hot(tree, None)?;
    Ok((encoded.hash, encoded.data))
}

/// Encode the capture hot path. Materialized writes are cheap HLR1 anchors;
/// eligible descendants are cumulative HDC1 deltas against the epoch anchor.
pub fn encode_tree_hot(tree: &Tree, base: Option<TreeDeltaBase<'_>>) -> Result<EncodedTree> {
    let hash = tree.hash();
    let lean = tree.encode_lean()?;
    let Some(base) = base else {
        return Ok(EncodedTree {
            hash,
            data: lean,
            kind: TreeEncodingKind::Lean,
        });
    };
    let Some(depth) = base.parent_depth.checked_add(1) else {
        return Ok(EncodedTree {
            hash,
            data: lean,
            kind: TreeEncodingKind::Lean,
        });
    };
    if depth >= TREE_DELTA_ANCHOR_INTERVAL {
        return Ok(EncodedTree {
            hash,
            data: lean,
            kind: TreeEncodingKind::Lean,
        });
    }
    let ops = tree_delta(base.anchor, tree);
    if ops.len() > TREE_DELTA_MAX_OPS {
        return Ok(EncodedTree {
            hash,
            data: lean,
            kind: TreeEncodingKind::Lean,
        });
    }
    let delta = encode_tree_delta(base.anchor_id, base.anchor, tree, &ops)?;
    let header = decode_tree_delta_header(&delta)?;
    let porch_is_bounded = header.first_base_count <= 1 && header.hundred_base_count <= 100;
    if !porch_is_bounded || delta.len() >= lean.len() {
        return Ok(EncodedTree {
            hash,
            data: lean,
            kind: TreeEncodingKind::Lean,
        });
    }
    Ok(EncodedTree {
        hash,
        data: delta,
        kind: TreeEncodingKind::Delta {
            anchor: base.anchor_id,
            depth,
            op_count: ops.len(),
        },
    })
}

/// Heavy, seekable compression for repack/background use only.
pub fn encode_tree_at_rest(tree: &Tree, config: &CompressionConfig) -> Result<Vec<u8>> {
    if config.enabled && tree.len() >= crate::object::TREE_BLOCK_MIN_ENTRIES {
        Ok(tree.encode_canonical_blocked(config.level, config.min_size)?)
    } else {
        Ok(tree.encode_canonical()?)
    }
}

pub fn decode_tree(data: &[u8]) -> Result<Tree> {
    let decoded = decode_tree_body(data)?;
    decode_tree_serialized(&decoded)
}

pub fn decode_tree_serialized(data: &[u8]) -> Result<Tree> {
    if !is_canonical_tree(data) {
        return Err(HeddleError::InvalidObject(
            "HLR1/HDC1 tree decoding requires the external object key".to_string(),
        ));
    }
    Tree::decode_canonical(data).map_err(HeddleError::from)
}

/// Decode any production tree body and validate it against the external key.
/// HDC1 callers must supply its materialized anchor; canonical and HLR1 bodies
/// ignore `anchor`.
pub fn decode_tree_with_key(
    data: &[u8],
    expected: ContentHash,
    anchor: Option<&Tree>,
) -> Result<Tree> {
    let decoded = decode_tree_body(data)?;
    decode_tree_serialized_with_key(&decoded, expected, anchor)
}

pub fn decode_tree_serialized_with_key(
    data: &[u8],
    expected: ContentHash,
    anchor: Option<&Tree>,
) -> Result<Tree> {
    let tree = if is_lean_tree(data) {
        Tree::decode_lean(data, expected)?
    } else if is_delta_tree(data) {
        let anchor = anchor.ok_or_else(|| {
            HeddleError::InvalidObject("HDC1 tree is missing its materialized anchor".to_string())
        })?;
        decode_tree_delta(data, anchor, expected)?
    } else if is_canonical_tree(data) {
        Tree::decode_canonical(data)?
    } else {
        return Err(HeddleError::InvalidObject(
            "unsupported tree storage body".to_string(),
        ));
    };
    let found = tree.hash();
    if found != expected {
        return Err(HeddleError::Corruption { expected, found });
    }
    Ok(tree)
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
            assert!(crate::object::is_lean_tree(&encoded));
            assert_eq!(decode_tree_with_key(&encoded, hash, None).unwrap(), tree);
        }
    }

    #[test]
    fn lean_and_delta_round_trip_against_external_keys() {
        let anchor = tree_fixture(240, None);
        let current = tree_fixture(240, Some((117, b"changed")));
        let lean = encode_tree_hot(&anchor, None).unwrap();
        assert_eq!(lean.kind, TreeEncodingKind::Lean);
        assert_eq!(
            decode_tree_with_key(&lean.data, anchor.hash(), None).unwrap(),
            anchor
        );

        let delta = encode_tree_hot(
            &current,
            Some(TreeDeltaBase {
                anchor_id: anchor.hash(),
                anchor: &anchor,
                parent_depth: 0,
            }),
        )
        .unwrap();
        assert!(matches!(
            delta.kind,
            TreeEncodingKind::Delta {
                anchor: _,
                depth: 1,
                op_count: 1
            }
        ));
        assert!(crate::object::is_delta_tree(&delta.data));
        assert_eq!(
            decode_tree_with_key(&delta.data, current.hash(), Some(&anchor)).unwrap(),
            current
        );
    }

    #[test]
    fn delta_refreshes_anchor_after_127_descendants() {
        let anchor = tree_fixture(240, None);
        let current = tree_fixture(240, Some((117, b"changed")));

        let last_descendant = encode_tree_hot(
            &current,
            Some(TreeDeltaBase {
                anchor_id: anchor.hash(),
                anchor: &anchor,
                parent_depth: TREE_DELTA_ANCHOR_INTERVAL - 2,
            }),
        )
        .unwrap();
        assert!(matches!(
            last_descendant.kind,
            TreeEncodingKind::Delta { depth: 127, .. }
        ));

        let refreshed = encode_tree_hot(
            &current,
            Some(TreeDeltaBase {
                anchor_id: anchor.hash(),
                anchor: &anchor,
                parent_depth: TREE_DELTA_ANCHOR_INTERVAL - 1,
            }),
        )
        .unwrap();
        assert_eq!(refreshed.kind, TreeEncodingKind::Lean);
        assert!(crate::object::is_lean_tree(&refreshed.data));
    }

    #[test]
    fn delta_over_512_operations_refreshes_the_anchor() {
        let anchor = tree_fixture(600, None);
        let current = Tree::from_entries(
            anchor
                .entries()
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    TreeEntry::file(
                        entry.name(),
                        ContentHash::compute(format!("changed-{index}").as_bytes()),
                        false,
                    )
                    .unwrap()
                })
                .collect(),
        );
        let ops = tree_delta(&anchor, &current);
        assert_eq!(ops.len(), 600);
        assert!(encode_tree_delta(anchor.hash(), &anchor, &current, &ops).is_err());
        let encoded = encode_tree_hot(
            &current,
            Some(TreeDeltaBase {
                anchor_id: anchor.hash(),
                anchor: &anchor,
                parent_depth: 0,
            }),
        )
        .unwrap();
        assert_eq!(encoded.kind, TreeEncodingKind::Lean);
    }

    #[test]
    fn every_tree_form_validates_the_external_key() {
        let anchor = tree_fixture(240, None);
        let current = tree_fixture(240, Some((117, b"changed")));
        let wrong = ContentHash::compute(b"wrong-tree-key");
        let lean = anchor.encode_lean().unwrap();
        assert!(decode_tree_with_key(&lean, wrong, None).is_err());

        let ops = tree_delta(&anchor, &current);
        let delta = encode_tree_delta(anchor.hash(), &anchor, &current, &ops).unwrap();
        assert!(decode_tree_with_key(&delta, wrong, Some(&anchor)).is_err());

        let raw = current.encode_canonical().unwrap();
        assert!(decode_tree_with_key(&raw, wrong, None).is_err());
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

        let encoded_tree = encode_tree_at_rest(&tree, &CompressionConfig::default()).unwrap();
        let encoded_state = encode_state(&state, &CompressionConfig::default()).unwrap();

        assert!(
            crate::object::is_canonical_tree(&encoded_tree),
            "at-rest trees remain versioned HTR4 so resume can seek"
        );
        assert_eq!(&encoded_state[9..13], &1_u32.to_be_bytes());
    }

    #[test]
    #[cfg(feature = "zstd")]
    fn store_decoder_reads_raw_v4_and_blocked_v5() {
        let tree = tree_fixture(600, None);
        let raw = tree.encode_canonical().unwrap();
        let blocked = encode_tree_at_rest(
            &tree,
            &CompressionConfig {
                enabled: true,
                level: 3,
                min_size: 0,
                max_delta_size: CompressionConfig::default().max_delta_size,
            },
        )
        .unwrap();
        assert_eq!(raw[4], crate::object::TREE_ENCODING_VERSION);
        assert_eq!(blocked[4], crate::object::TREE_BLOCK_ENCODING_VERSION);
        assert_eq!(decode_tree_with_key(&raw, tree.hash(), None).unwrap(), tree);
        assert_eq!(
            decode_tree_with_key(&blocked, tree.hash(), None).unwrap(),
            tree
        );
    }

    #[test]
    #[cfg(feature = "zstd")]
    fn tree_state_dictionary_corpus_roundtrips_byte_identically() {
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
            let encoded_tree = encode_tree_at_rest(&tree, &config).unwrap();
            assert_eq!(Tree::decode_canonical(&encoded_tree).unwrap(), tree);

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

    fn tree_fixture(entries: usize, changed: Option<(usize, &[u8])>) -> Tree {
        Tree::from_entries(
            (0..entries)
                .map(|index| {
                    let payload = changed
                        .filter(|(changed_index, _)| *changed_index == index)
                        .map_or_else(
                            || format!("blob-{index}").into_bytes(),
                            |(_, payload)| payload.to_vec(),
                        );
                    TreeEntry::file(
                        format!("module_{index:04}.rs"),
                        ContentHash::compute(&payload),
                        false,
                    )
                    .unwrap()
                })
                .collect(),
        )
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
