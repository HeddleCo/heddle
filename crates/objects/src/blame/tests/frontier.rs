// SPDX-License-Identifier: Apache-2.0
use std::cell::Cell;
use std::path::Path;

use crate::blame::{
    advance_file_blame_slice, prepare_file_blame, BlameLineMap, BlamePreparation, BlameSliceError,
    BlameSliceLimits,
};
use crate::object::{
    Attribution, Blob, ContentHash, ObjectSource, Principal, State, StateId, Tree,
};
use crate::store::ObjectStore;
use crate::util::ResourceKind;

use super::fixture::{put_state_with_file, store};

struct ProbeBeforeLoad {
    inner: crate::store::InMemoryStore,
    blob_loads: Cell<u32>,
}

impl ObjectSource for ProbeBeforeLoad {
    fn get_tree(&self, hash: &ContentHash) -> crate::error::Result<Option<Tree>> {
        ObjectSource::get_tree(&self.inner, hash)
    }

    fn get_state(&self, id: &StateId) -> crate::error::Result<Option<State>> {
        ObjectSource::get_state(&self.inner, id)
    }

    fn decoded_blob_len(&self, hash: &ContentHash) -> crate::error::Result<Option<u64>> {
        ObjectSource::decoded_blob_len(&self.inner, hash)
    }

    fn get_blob(&self, hash: &ContentHash) -> crate::error::Result<Option<Blob>> {
        self.blob_loads.set(self.blob_loads.get() + 1);
        panic!("get_blob must not materialize after a decoded_bytes reject: {hash}");
    }
}

#[test]
fn forged_origin_attribution_is_invalid_frontier() {
    let store = store();
    let state = put_state_with_file(&store, "file.txt", b"hello\n", Vec::new(), "alice");
    let path = Path::new("file.txt");
    let BlamePreparation::Active { mut frontier, .. } =
        prepare_file_blame(&store, &state, path, BlameSliceLimits::unlimited()).unwrap()
    else {
        panic!("expected active frontier");
    };
    frontier.records[0].origin.attribution = Attribution::human(Principal::new("mallory", "m@x"));
    let err = advance_file_blame_slice(&store, path, frontier, BlameSliceLimits::unlimited())
        .expect_err("forged attribution");
    assert!(
        matches!(err, BlameSliceError::InvalidFrontier(_)),
        "expected InvalidFrontier, got {err}"
    );
}

#[test]
fn overlapping_state_mappings_are_invalid_frontier() {
    let store = store();
    let state = put_state_with_file(&store, "file.txt", b"a\n", Vec::new(), "alice");
    let path = Path::new("file.txt");
    let BlamePreparation::Active { mut frontier, .. } =
        prepare_file_blame(&store, &state, path, BlameSliceLimits::unlimited()).unwrap()
    else {
        panic!("expected active frontier");
    };
    frontier.records[0].mappings = vec![
        BlameLineMap {
            state_start: 0,
            target_start: 0,
            len: 1,
        },
        BlameLineMap {
            state_start: 0,
            target_start: 1,
            len: 1,
        },
    ];
    let err = advance_file_blame_slice(&store, path, frontier, BlameSliceLimits::unlimited())
        .expect_err("overlapping maps");
    assert!(
        matches!(err, BlameSliceError::InvalidFrontier(_)),
        "expected InvalidFrontier, got {err}"
    );
}

#[test]
fn zero_length_mapping_is_invalid_frontier() {
    let store = store();
    let state = put_state_with_file(&store, "file.txt", b"a\n", Vec::new(), "alice");
    let path = Path::new("file.txt");
    let BlamePreparation::Active { mut frontier, .. } =
        prepare_file_blame(&store, &state, path, BlameSliceLimits::unlimited()).unwrap()
    else {
        panic!("expected active frontier");
    };
    frontier.records[0].mappings[0].len = 0;
    let err = advance_file_blame_slice(&store, path, frontier, BlameSliceLimits::unlimited())
        .expect_err("zero-length map");
    assert!(
        matches!(err, BlameSliceError::InvalidFrontier(_)),
        "expected InvalidFrontier, got {err}"
    );
}

#[test]
fn frontier_blob_must_belong_to_loaded_state() {
    let store = store();
    let parent = put_state_with_file(&store, "file.txt", b"parent\n", Vec::new(), "alice");
    let child = put_state_with_file(&store, "file.txt", b"child!\n", vec![parent.id()], "bob");
    let path = Path::new("file.txt");
    let BlamePreparation::Active { mut frontier, .. } =
        prepare_file_blame(&store, &child, path, BlameSliceLimits::unlimited()).unwrap()
    else {
        panic!("expected active frontier");
    };
    let parent_blob = lookup_file_blob(&store, &parent, path);
    frontier.records[0].blob_hash = parent_blob;
    let err = advance_file_blame_slice(&store, path, frontier, BlameSliceLimits::unlimited())
        .expect_err("swapped parent blob");
    assert!(
        matches!(err, BlameSliceError::InvalidFrontier(_)),
        "expected InvalidFrontier, got {err}"
    );
}

#[test]
fn decoded_byte_limit_rejects_before_get_blob() {
    let inner = store();
    let state = put_state_with_file(&inner, "file.txt", b"hello\n", Vec::new(), "alice");
    let path = Path::new("file.txt");
    let BlamePreparation::Active { frontier, .. } =
        prepare_file_blame(&inner, &state, path, BlameSliceLimits::unlimited()).unwrap()
    else {
        panic!("expected active frontier");
    };
    let probe = ProbeBeforeLoad {
        inner,
        blob_loads: Cell::new(0),
    };
    let limits = BlameSliceLimits {
        decoded_bytes: 1,
        ..BlameSliceLimits::unlimited()
    };
    let err =
        advance_file_blame_slice(&probe, path, frontier, limits).expect_err("tiny decoded_bytes");
    match err {
        BlameSliceError::BudgetExceeded(error) => {
            assert_eq!(error.kind, ResourceKind::DecodedBytes);
            assert_eq!(error.limit, 1);
            assert!(error.needed > 1);
        }
        other => panic!("expected DecodedBytes budget, got {other}"),
    }
    assert_eq!(probe.blob_loads.get(), 0);
}

fn lookup_file_blob(
    store: &crate::store::InMemoryStore,
    state: &State,
    path: &Path,
) -> ContentHash {
    let tree = ObjectStore::get_tree(store, &state.tree).unwrap().unwrap();
    let name = path.file_name().unwrap().to_str().unwrap();
    tree.get(name).unwrap().blob_hash().unwrap()
}
