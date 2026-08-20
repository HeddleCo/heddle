// SPDX-License-Identifier: Apache-2.0
use std::path::Path;

use crate::blame::{
    BlameFrontierGroup, BlamePreparation, BlameSliceAdvance, BlameSliceError, BlameSliceLimits,
    BlameTarget, advance_file_blame_slice, blame_file, finalize_file_provenance,
    prepare_file_blame,
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

#[test]
fn mixed_job_target_and_frontier_is_invalid() {
    let store = store();
    let job_a = put_state_with_file(&store, "lib.rs", b"alpha\n", Vec::new(), "alice");
    let job_b = put_state_with_file(&store, "lib.rs", b"beta!\n", Vec::new(), "bob");
    let path = Path::new("lib.rs");
    let limits = BlameSliceLimits::unlimited();
    let BlamePreparation::Active {
        file_blob: a_blob,
        line_count: a_lines,
        frontier: a_frontier,
    } = prepare_file_blame(&store, &job_a, path, limits).unwrap()
    else {
        panic!("active A");
    };
    let BlamePreparation::Active {
        frontier: b_frontier,
        ..
    } = prepare_file_blame(&store, &job_b, path, limits).unwrap()
    else {
        panic!("active B");
    };

    let mixed = BlameFrontierGroup {
        target: BlameTarget {
            blob: a_blob,
            line_count: a_lines,
        },
        records: b_frontier.records,
    };
    let err = advance_file_blame_slice(&store, path, mixed, limits)
        .expect_err("mixed target and frontier");
    assert!(
        matches!(err, BlameSliceError::InvalidFrontier(_)),
        "expected InvalidFrontier, got {err}"
    );

    let err = a_frontier
        .require_target(b_frontier.target.blob, b_frontier.target.line_count)
        .expect_err("A frontier against B target");
    assert!(
        matches!(err, BlameSliceError::InvalidFrontier(_)),
        "expected InvalidFrontier, got {err}"
    );
}
