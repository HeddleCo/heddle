// SPDX-License-Identifier: Apache-2.0
mod fail_closed;
mod fixture;
mod golden;
mod prop;
mod restart;

use std::path::Path;

use crate::blame::{
    BlameSliceAdvance, BlameSliceError, BlameSliceLimits, advance_file_blame_slice, blame_file,
};
use crate::util::ResourceKind;

use fixture::{principals_at, put_state_with_file, store};

#[test]
fn missing_path_is_typed() {
    let store = store();
    let state = put_state_with_file(&store, "kept.txt", b"ok\n", Vec::new(), "alice");
    let err = blame_file(
        &store,
        &state,
        Path::new("missing.txt"),
        BlameSliceLimits::unlimited(),
    )
    .expect_err("missing path");
    assert!(matches!(err, BlameSliceError::MissingPath));
}

#[test]
fn binary_is_unblamable_not_budget() {
    let store = store();
    let state = put_state_with_file(&store, "bin.dat", &[0xff, 0xfe, 0x00], Vec::new(), "alice");
    let err = blame_file(
        &store,
        &state,
        Path::new("bin.dat"),
        BlameSliceLimits::unlimited(),
    )
    .expect_err("binary");
    assert!(matches!(err, BlameSliceError::Unblamable));
}

#[test]
fn line_budget_is_not_missing_object() {
    let store = store();
    let state = put_state_with_file(&store, "lib.rs", b"a\nb\nc\n", Vec::new(), "alice");
    let err = blame_file(
        &store,
        &state,
        Path::new("lib.rs"),
        BlameSliceLimits {
            states: 8,
            decoded_bytes: 1024,
            lines: 1,
            diff_work: 64,
            scratch_bytes: 64 * 1024,
        },
    )
    .expect_err("line cap");
    match err {
        BlameSliceError::BudgetExceeded(error) => {
            assert_eq!(error.kind, ResourceKind::Lines);
        }
        other => panic!("expected lines budget, got {other}"),
    }
}

#[test]
fn one_state_slices_match_oneshot() {
    let store = store();
    let base = put_state_with_file(&store, "lib.rs", b"a\n", Vec::new(), "alice");
    let mid = put_state_with_file(&store, "lib.rs", b"a\nb\n", vec![base.id()], "bob");
    let tip = put_state_with_file(&store, "lib.rs", b"a\nb\nc\n", vec![mid.id()], "carol");

    let oneshot = blame_file(
        &store,
        &tip,
        Path::new("lib.rs"),
        BlameSliceLimits::unlimited(),
    )
    .expect("oneshot");
    let sliced = blame_by_single_state_slices(&store, &tip, Path::new("lib.rs")).expect("sliced");
    assert_eq!(oneshot, sliced);
    assert!(principals_at(&oneshot, 0).contains(&"alice".to_string()));
    assert!(principals_at(&oneshot, 1).contains(&"bob".to_string()));
    assert!(principals_at(&oneshot, 2).contains(&"carol".to_string()));
}

fn blame_by_single_state_slices(
    store: &crate::store::InMemoryStore,
    state: &crate::object::State,
    path: &Path,
) -> Result<crate::object::FileProvenance, BlameSliceError> {
    use crate::blame::{BlamePreparation, finalize_file_provenance, prepare_file_blame};

    let limits = BlameSliceLimits {
        states: 1,
        decoded_bytes: 64 * 1024,
        lines: 64,
        diff_work: 1_024,
        scratch_bytes: 64 * 1024,
    };
    match prepare_file_blame(store, state, path, limits)? {
        BlamePreparation::MissingPath => Err(BlameSliceError::MissingPath),
        BlamePreparation::Unblamable => Err(BlameSliceError::Unblamable),
        BlamePreparation::Empty { file_blob, origin } => finalize_file_provenance(
            file_blob,
            0,
            [crate::blame::OriginRange {
                target_start: 0,
                len: 0,
                origin,
            }],
        ),
        BlamePreparation::Active {
            file_blob,
            line_count,
            mut frontier,
        } => {
            let mut finalized = Vec::new();
            loop {
                match advance_file_blame_slice(store, path, frontier, limits)? {
                    BlameSliceAdvance::Progress {
                        next,
                        finalized: more,
                        usage,
                    } => {
                        assert!(usage.states <= limits.states);
                        assert!(usage.decoded_bytes <= limits.decoded_bytes);
                        assert!(usage.lines > 0);
                        assert!(usage.scratch_bytes > 0 || usage.work == 0);
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
            finalize_file_provenance(file_blob, line_count, finalized)
        }
    }
}
