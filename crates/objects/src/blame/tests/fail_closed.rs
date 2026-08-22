// SPDX-License-Identifier: Apache-2.0
use std::path::Path;
use std::time::Instant;

use crate::blame::{
    BlamePreparation, BlameSliceAdvance, BlameSliceError, BlameSliceLimits,
    advance_file_blame_slice, blame_file, prepare_file_blame,
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
fn missing_parent_tree_is_missing_object_not_path_gone() {
    let store = store();
    let missing_tree = ContentHash::from_bytes([0x22; 32]);
    let parent = State::new(
        missing_tree,
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
    .expect_err("missing parent tree");
    match err {
        BlameSliceError::MissingObject { kind, id } => {
            assert_eq!(kind, "tree");
            assert_eq!(id, missing_tree.to_string());
        }
        BlameSliceError::MissingPath => {
            panic!("missing parent tree must not be MissingPath")
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
        BlameSliceError::InvalidFrontier(_) => {}
        other => panic!("expected InvalidFrontier, got {other}"),
    }
}

#[test]
fn unlimited_frontier_line_count_must_match_blob_before_bitmap() {
    let store = store();
    let state = put_state_with_file(&store, "file.txt", b"x\n", Vec::new(), "alice");
    let path = Path::new("file.txt");
    let BlamePreparation::Active { mut frontier, .. } =
        prepare_file_blame(&store, &state, path, BlameSliceLimits::unlimited()).unwrap()
    else {
        panic!("expected active frontier");
    };
    frontier.records[0].state_line_count = u32::MAX;
    let started = Instant::now();
    let err = advance_file_blame_slice(&store, path, frontier, BlameSliceLimits::unlimited())
        .expect_err("mismatched frontier line count");
    assert!(
        started.elapsed().as_millis() < 1_000,
        "huge bitmap must not be allocated"
    );
    assert!(
        matches!(err, BlameSliceError::InvalidFrontier(_)),
        "expected InvalidFrontier, got {err}"
    );
}

#[test]
fn mapping_past_state_line_count_is_invalid_frontier() {
    let store = store();
    let state = put_state_with_file(&store, "file.txt", b"x\n", Vec::new(), "alice");
    let path = Path::new("file.txt");
    let BlamePreparation::Active { mut frontier, .. } =
        prepare_file_blame(&store, &state, path, BlameSliceLimits::unlimited()).unwrap()
    else {
        panic!("expected active frontier");
    };
    frontier.records[0].mappings[0].len = 2;
    let err = advance_file_blame_slice(&store, path, frontier, BlameSliceLimits::unlimited())
        .expect_err("mapping past blob");
    assert!(
        matches!(err, BlameSliceError::InvalidFrontier(_)),
        "expected InvalidFrontier, got {err}"
    );
}

#[test]
fn wide_merge_parent_reads_consume_state_budget() {
    let store = store();
    let p0 = put_state_with_file(&store, "file.txt", b"a\n", Vec::new(), "p0");
    let p1 = put_state_with_file(&store, "file.txt", b"b\n", Vec::new(), "p1");
    let p2 = put_state_with_file(&store, "file.txt", b"c\n", Vec::new(), "p2");
    let p3 = put_state_with_file(&store, "file.txt", b"d\n", Vec::new(), "p3");
    let merge = put_state_with_file(
        &store,
        "file.txt",
        b"a\nb\nc\nd\n",
        vec![p0.id(), p1.id(), p2.id(), p3.id()],
        "merge",
    );
    let path = Path::new("file.txt");
    let BlamePreparation::Active { frontier, .. } =
        prepare_file_blame(&store, &merge, path, BlameSliceLimits::unlimited()).unwrap()
    else {
        panic!("expected active frontier");
    };

    let one = BlameSliceLimits {
        states: 1,
        ..BlameSliceLimits::unlimited()
    };
    let err = advance_file_blame_slice(&store, path, frontier.clone(), one)
        .expect_err("states:1 must not walk merge parents");
    match err {
        BlameSliceError::BudgetExceeded(error) => {
            assert_eq!(error.kind, ResourceKind::States);
            assert_eq!(error.limit, 1);
            assert_eq!(error.needed, 2);
        }
        other => panic!("expected States budget, got {other}"),
    }

    let two = BlameSliceLimits {
        states: 2,
        ..BlameSliceLimits::unlimited()
    };
    let err = advance_file_blame_slice(&store, path, frontier.clone(), two)
        .expect_err("states:2 is entry plus one parent");
    match err {
        BlameSliceError::BudgetExceeded(error) => {
            assert_eq!(error.kind, ResourceKind::States);
            assert_eq!(error.limit, 2);
            assert_eq!(error.needed, 3);
        }
        other => panic!("expected States budget, got {other}"),
    }

    match advance_file_blame_slice(&store, path, frontier, BlameSliceLimits::unlimited()).unwrap() {
        BlameSliceAdvance::Progress { usage, .. } | BlameSliceAdvance::Complete { usage, .. } => {
            assert_eq!(usage.states, 5);
        }
    }
}

fn sixty_lines(tag: &str) -> Vec<u8> {
    (0..60)
        .map(|index| format!("{tag}{index}\n"))
        .collect::<String>()
        .into_bytes()
}

#[test]
fn multi_parent_lines_consume_each_parent_file() {
    let store = store();
    let p0 = put_state_with_file(&store, "file.txt", &sixty_lines("a"), Vec::new(), "p0");
    let p1 = put_state_with_file(&store, "file.txt", &sixty_lines("b"), Vec::new(), "p1");
    let merge = put_state_with_file(
        &store,
        "file.txt",
        &sixty_lines("m"),
        vec![p0.id(), p1.id()],
        "merge",
    );
    let path = Path::new("file.txt");
    let BlamePreparation::Active { frontier, .. } =
        prepare_file_blame(&store, &merge, path, BlameSliceLimits::unlimited()).unwrap()
    else {
        panic!("expected active frontier");
    };

    let capped = BlameSliceLimits {
        lines: 100,
        ..BlameSliceLimits::unlimited()
    };
    let err = advance_file_blame_slice(&store, path, frontier.clone(), capped)
        .expect_err("lines:100 cannot scan entry plus two 60-line parents");
    match err {
        BlameSliceError::BudgetExceeded(error) => {
            assert_eq!(error.kind, ResourceKind::Lines);
            assert_eq!(error.limit, 100);
            assert_eq!(error.needed, 120);
        }
        other => panic!("expected Lines budget, got {other}"),
    }

    match advance_file_blame_slice(&store, path, frontier, BlameSliceLimits::unlimited()).unwrap() {
        BlameSliceAdvance::Progress { usage, .. } | BlameSliceAdvance::Complete { usage, .. } => {
            assert_eq!(usage.lines, 180);
        }
    }
}
