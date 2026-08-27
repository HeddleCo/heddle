// SPDX-License-Identifier: Apache-2.0
mod fail_closed;
mod finalize;
mod fixture;
mod frontier;
mod golden;
mod prop;
mod restart;

use std::{cell::RefCell, collections::HashMap, path::Path};

use crate::blame::{
    BlamePreparation, BlameSliceAdvance, BlameSliceError, BlameSliceLimits,
    advance_file_blame_slice, blame_file, prepare_file_blame,
};
use crate::object::{Blob, ContentHash, ObjectSource, State, StateId, Tree};
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
fn no_overlap_parent_is_not_a_processed_frontier() {
    let store = store();
    let parent = put_state_with_file(&store, "fixture.txt", b"old line\n", Vec::new(), "alice");
    let tip = put_state_with_file(
        &store,
        "fixture.txt",
        b"new line\n",
        vec![parent.id()],
        "bob",
    );
    let path = Path::new("fixture.txt");
    let limits = BlameSliceLimits::unlimited();
    let before = heddle_perf_contract::snapshot();
    let provenance = blame_file(&store, &tip, path, limits).unwrap();
    let after = heddle_perf_contract::snapshot();
    assert_eq!(principals_at(&provenance, 0), vec!["bob".to_string()]);
    assert!(
        after
            .ancestors_visited
            .saturating_sub(before.ancestors_visited)
            <= 1,
        "no-overlap parent must not count as a processed frontier"
    );

    let BlamePreparation::Active { frontier, .. } =
        prepare_file_blame(&store, &tip, path, limits).unwrap()
    else {
        panic!("expected active blame");
    };
    match advance_file_blame_slice(&store, path, frontier, limits).unwrap() {
        BlameSliceAdvance::Complete { .. } => {}
        BlameSliceAdvance::Progress { .. } => {
            panic!("no-overlap parent must not become a next frontier")
        }
    }
}

struct CountingSource<'source> {
    inner: &'source crate::store::InMemoryStore,
    trees: RefCell<HashMap<ContentHash, u32>>,
    states: RefCell<HashMap<StateId, u32>>,
    blobs: RefCell<HashMap<ContentHash, u32>>,
    blob_sizes: RefCell<HashMap<ContentHash, u32>>,
}

impl<'source> CountingSource<'source> {
    fn new(inner: &'source crate::store::InMemoryStore) -> Self {
        Self {
            inner,
            trees: RefCell::new(HashMap::new()),
            states: RefCell::new(HashMap::new()),
            blobs: RefCell::new(HashMap::new()),
            blob_sizes: RefCell::new(HashMap::new()),
        }
    }
}

impl ObjectSource for CountingSource<'_> {
    fn get_tree(&self, hash: &ContentHash) -> crate::error::Result<Option<Tree>> {
        *self.trees.borrow_mut().entry(*hash).or_default() += 1;
        ObjectSource::get_tree(self.inner, hash)
    }

    fn get_state(&self, id: &StateId) -> crate::error::Result<Option<State>> {
        *self.states.borrow_mut().entry(*id).or_default() += 1;
        ObjectSource::get_state(self.inner, id)
    }

    fn get_blob(&self, hash: &ContentHash) -> crate::error::Result<Option<Blob>> {
        *self.blobs.borrow_mut().entry(*hash).or_default() += 1;
        ObjectSource::get_blob(self.inner, hash)
    }

    fn decoded_blob_len(&self, hash: &ContentHash) -> crate::error::Result<Option<u64>> {
        *self.blob_sizes.borrow_mut().entry(*hash).or_default() += 1;
        ObjectSource::decoded_blob_len(self.inner, hash)
    }
}

#[test]
fn oneshot_blame_decodes_each_object_once() {
    let store = store();
    let parent = put_state_with_file(&store, "fixture.txt", b"old line\n", Vec::new(), "alice");
    let tip = put_state_with_file(
        &store,
        "fixture.txt",
        b"old line\nnew line\n",
        vec![parent.id()],
        "bob",
    );
    let path = Path::new("fixture.txt");
    let expected = blame_file(&store, &tip, path, BlameSliceLimits::unlimited()).unwrap();
    let source = CountingSource::new(&store);

    let actual = blame_file(&source, &tip, path, BlameSliceLimits::unlimited()).unwrap();

    assert_eq!(actual, expected, "memoization must not change provenance");
    for calls in [&source.trees, &source.blobs, &source.blob_sizes] {
        let calls = calls.borrow();
        assert_eq!(calls.len(), 2, "tip and parent objects must be loaded");
        assert!(calls.values().all(|count| *count == 1));
    }
    let state_calls = source.states.borrow();
    assert_eq!(state_calls.len(), 2, "tip and parent states must be loaded");
    assert!(state_calls.values().all(|count| *count == 1));
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
        states: 2,
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
