// SPDX-License-Identifier: Apache-2.0
use std::path::Path;

use crate::blame::{
    BlameFrontierGroup, BlamePreparation, BlameSliceAdvance, BlameSliceLimits,
    advance_file_blame_slice, blame_file, finalize_file_provenance, prepare_file_blame,
};

use super::fixture::{put_state_with_file, store};

#[test]
fn serialized_frontier_finishes_with_same_provenance() {
    let store = store();
    let base = put_state_with_file(&store, "lib.rs", b"a\n", Vec::new(), "alice");
    let mid = put_state_with_file(&store, "lib.rs", b"a\nb\n", vec![base.id()], "bob");
    let tip = put_state_with_file(&store, "lib.rs", b"a\nb\nc\n", vec![mid.id()], "carol");
    let path = Path::new("lib.rs");
    let limits = BlameSliceLimits {
        states: 3,
        decoded_bytes: 4096,
        lines: 16,
        diff_work: 64,
        scratch_bytes: 32 * 1024,
    };

    let BlamePreparation::Active {
        file_blob,
        line_count,
        frontier,
    } = prepare_file_blame(&store, &tip, path, limits).unwrap()
    else {
        panic!("expected active blame");
    };

    let first = advance_file_blame_slice(&store, path, frontier, limits).unwrap();
    let BlameSliceAdvance::Progress {
        next,
        finalized: first_finalized,
        ..
    } = first
    else {
        panic!("expected progress after the tip slice");
    };

    let encoded = serde_json::to_string(&next).expect("serialize frontier");
    let restored: BlameFrontierGroup = serde_json::from_str(&encoded).expect("restore frontier");
    assert_eq!(restored, next);

    let mut finalized = first_finalized;
    let mut frontier = restored;
    loop {
        match advance_file_blame_slice(&store, path, frontier, limits).unwrap() {
            BlameSliceAdvance::Progress {
                next,
                finalized: more,
                ..
            } => {
                finalized.extend(more);
                frontier = next;
            }
            BlameSliceAdvance::Complete {
                finalized: more, ..
            } => {
                finalized.extend(more);
                break;
            }
        }
    }

    let restarted = finalize_file_provenance(file_blob, line_count, finalized).unwrap();
    let direct = blame_file(&store, &tip, path, BlameSliceLimits::unlimited()).unwrap();
    assert_eq!(restarted, direct);
}

#[test]
fn same_slice_input_is_idempotent() {
    let store = store();
    let base = put_state_with_file(&store, "lib.rs", b"a\n", Vec::new(), "alice");
    let tip = put_state_with_file(&store, "lib.rs", b"a\nb\n", vec![base.id()], "bob");
    let path = Path::new("lib.rs");
    let limits = BlameSliceLimits::unlimited();
    let BlamePreparation::Active { frontier, .. } =
        prepare_file_blame(&store, &tip, path, limits).unwrap()
    else {
        panic!("active");
    };
    let first = advance_file_blame_slice(&store, path, frontier.clone(), limits).unwrap();
    let second = advance_file_blame_slice(&store, path, frontier, limits).unwrap();
    assert_eq!(first, second);
}
