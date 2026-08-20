// SPDX-License-Identifier: Apache-2.0
use std::path::Path;
use std::time::Instant;

use crate::blame::{
    BlamePreparation, BlameSliceError, BlameSliceLimits, advance_file_blame_slice, blame_file,
    prepare_file_blame,
};
use crate::object::{Attribution, ContentHash, Principal, State, StateId, Tree, TreeEntry};
use crate::store::ObjectStore;
use crate::util::ResourceKind;

use super::fixture::{put_state_with_file, store};

#[test]
fn missing_parent_state_is_missing_object_not_child_credit() {
    let store = store();
    let missing_parent = StateId::from_bytes([0x11; 32]);
    let later = put_state_with_file(&store, "file.txt", b"hello\n", Vec::new(), "alice");
    let child = put_state_with_file(
        &store,
        "file.txt",
        b"hello\n",
        vec![missing_parent, later.id()],
        "bob",
    );
    let err = blame_file(
        &store,
        &child,
        Path::new("file.txt"),
        BlameSliceLimits::unlimited(),
    )
    .expect_err("missing parent state");
    match err {
        BlameSliceError::MissingObject { kind, id } => {
            assert_eq!(kind, "state");
            assert_eq!(id, missing_parent.to_string());
        }
        other => panic!("expected MissingObject, got {other}"),
    }
}

#[test]
fn tree_present_blob_missing_parent_is_missing_object_not_missing_path() {
    let store = store();
    let missing_blob = ContentHash::compute(b"absent-parent-blob");
    let parent_tree = store
        .put_tree(&Tree::from_entries(vec![
            TreeEntry::file("file.txt".to_string(), missing_blob, false).unwrap(),
        ]))
        .unwrap();
    let parent = State::new(
        parent_tree,
        Vec::new(),
        Attribution::human(Principal::new("alice", "alice@example.com")),
    );
    store.put_state(&parent).unwrap();
    let child = put_state_with_file(&store, "file.txt", b"child\n", vec![parent.id()], "bob");
    let err = blame_file(
        &store,
        &child,
        Path::new("file.txt"),
        BlameSliceLimits::unlimited(),
    )
    .expect_err("missing parent blob");
    match err {
        BlameSliceError::MissingObject { kind, id } => {
            assert_eq!(kind, "blob");
            assert_eq!(id, missing_blob.to_string());
        }
        BlameSliceError::MissingPath => {
            panic!("tree-present blob miss must not be MissingPath")
        }
        other => panic!("expected MissingObject, got {other}"),
    }
}

#[test]
fn frontier_state_line_count_above_line_limit_is_typed_without_huge_bitmap() {
    let store = store();
    let state = put_state_with_file(&store, "file.txt", b"x\n", Vec::new(), "alice");
    let path = Path::new("file.txt");
    let BlamePreparation::Active { mut frontier, .. } =
        prepare_file_blame(&store, &state, path, BlameSliceLimits::unlimited()).unwrap()
    else {
        panic!("expected active frontier");
    };
    frontier.records[0].state_line_count = u32::MAX;
    let limits = BlameSliceLimits {
        lines: 16,
        ..BlameSliceLimits::unlimited()
    };
    let started = Instant::now();
    let err = advance_file_blame_slice(&store, path, frontier, limits)
        .expect_err("oversize frontier line count");
    assert!(
        started.elapsed().as_millis() < 1_000,
        "huge bitmap must not be allocated"
    );
    match err {
        BlameSliceError::BudgetExceeded(error) => {
            assert_eq!(error.kind, ResourceKind::Lines);
            assert_eq!(error.limit, 16);
            assert_eq!(error.needed, u64::from(u32::MAX));
        }
        BlameSliceError::InvalidFrontier(_) => {}
        other => panic!("expected BudgetExceeded or InvalidFrontier, got {other}"),
    }
}
