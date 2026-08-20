// SPDX-License-Identifier: Apache-2.0

use super::{CompactError, Result, decode_state_frame};
use crate::object::{State, StateId};

/// Reconstruct one state and verify its BLAKE3 typed identity.
///
/// The whole-frame checksum is verified, every fidelity column is decoded, and
/// the state's id is recomputed from the reconstructed fields before it is
/// compared with `expected`.
pub fn extract_state(bytes: &[u8], expected: StateId) -> Result<State> {
    decode_state_frame(bytes)?
        .into_iter()
        .find(|state| state.accepts_stored_id(&expected))
        .map(|mut state| {
            state.state_id = expected;
            state
        })
        .ok_or(CompactError::Missing)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::super::{decode_tree_frame, encode_tree_frame, extract_tree};
    use crate::object::{ContentHash, Tree, TreeEntry};

    #[test]
    fn extract_tree_point_read_beats_decode_all_and_serialize() {
        let trees = (0..64)
            .map(|index| {
                Tree::from_entries(vec![
                    TreeEntry::file(
                        format!("file-{index}.txt"),
                        ContentHash::from_bytes([index as u8; 32]),
                        false,
                    )
                    .unwrap(),
                ])
            })
            .collect::<Vec<_>>();
        let encoded = encode_tree_frame(&trees).unwrap();
        let first = trees[0].hash();
        let last = trees[trees.len() - 1].hash();
        let _ = extract_tree(&encoded, first).unwrap();
        let _ = decode_tree_frame(&encoded).unwrap();

        const ITERATIONS: u32 = 200;
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let decoded = decode_tree_frame(&encoded).unwrap();
            let _ = decoded
                .into_iter()
                .map(|tree| rmp_serde::to_vec_named(&tree).unwrap())
                .collect::<Vec<_>>();
        }
        let decode_all = start.elapsed();

        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let tree = extract_tree(&encoded, first).unwrap();
            let _ = rmp_serde::to_vec_named(&tree).unwrap();
        }
        let extract_first = start.elapsed();

        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let tree = extract_tree(&encoded, last).unwrap();
            let _ = rmp_serde::to_vec_named(&tree).unwrap();
        }
        let extract_last = start.elapsed();

        eprintln!(
            "compact tree frame (64 trees, {ITERATIONS} iters): decode-all+serialize-all {decode_all:?}; extract-first {extract_first:?}; extract-last {extract_last:?}"
        );
        assert!(
            extract_first < decode_all && extract_last < decode_all,
            "single-object extract should beat decode-all, first={extract_first:?} last={extract_last:?} decode_all={decode_all:?}"
        );
    }
}
