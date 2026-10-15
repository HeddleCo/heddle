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

        // GitHub-hosted runners steal wall time in bursts. Three back-to-back
        // blocks let one stall land entirely on the last extract and invert a
        // comparison that holds on a quiet core (observed: last 183ms, decode
        // 148ms, first 40ms). Interleave the phases and keep the median round.
        const ROUNDS: usize = 5;
        const ITERATIONS: u32 = 40;
        let mut rounds = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let mut decode_all = std::time::Duration::ZERO;
            let mut extract_first = std::time::Duration::ZERO;
            let mut extract_last = std::time::Duration::ZERO;
            for _ in 0..ITERATIONS {
                let start = Instant::now();
                let decoded = decode_tree_frame(&encoded).unwrap();
                let _ = decoded
                    .into_iter()
                    .map(|tree| rmp_serde::to_vec_named(&tree).unwrap())
                    .collect::<Vec<_>>();
                decode_all += start.elapsed();

                let start = Instant::now();
                let tree = extract_tree(&encoded, first).unwrap();
                let _ = rmp_serde::to_vec_named(&tree).unwrap();
                extract_first += start.elapsed();

                let start = Instant::now();
                let tree = extract_tree(&encoded, last).unwrap();
                let _ = rmp_serde::to_vec_named(&tree).unwrap();
                extract_last += start.elapsed();
            }
            rounds.push((decode_all, extract_first, extract_last));
        }
        rounds.sort_by_key(|(decode_all, _, extract_last)| {
            extract_last.as_nanos() / decode_all.as_nanos().max(1)
        });
        let (decode_all, extract_first, extract_last) = rounds[ROUNDS / 2];

        eprintln!(
            "compact tree frame (64 trees, {ROUNDS}x{ITERATIONS} interleaved, median): decode-all+serialize-all {decode_all:?}; extract-first {extract_first:?}; extract-last {extract_last:?}"
        );
        assert!(
            extract_first < decode_all && extract_last < decode_all,
            "single-object extract should beat decode-all, first={extract_first:?} last={extract_last:?} decode_all={decode_all:?}"
        );
    }
}
