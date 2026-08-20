// SPDX-License-Identifier: Apache-2.0
use std::path::Path;

use crate::blame::{blame_file, BlameSliceLimits};

use super::fixture::{principals_at, put_state_with_file, store};

#[test]
fn linear_history_credits_introducing_authors() {
    let store = store();
    let s1 = put_state_with_file(&store, "lib.rs", b"a\n", Vec::new(), "alice");
    let s2 = put_state_with_file(&store, "lib.rs", b"a\nb\n", vec![s1.id()], "bob");
    let s3 = put_state_with_file(&store, "lib.rs", b"a\nb\nc\n", vec![s2.id()], "carol");
    let provenance = blame_file(
        &store,
        &s3,
        Path::new("lib.rs"),
        BlameSliceLimits::unlimited(),
    )
    .unwrap();
    assert!(principals_at(&provenance, 0).contains(&"alice".to_string()));
    assert!(principals_at(&provenance, 1).contains(&"bob".to_string()));
    assert!(principals_at(&provenance, 2).contains(&"carol".to_string()));
}

#[test]
fn path_absence_credits_introducing_state() {
    let store = store();
    let base = put_state_with_file(&store, "other.rs", b"keep\n", Vec::new(), "alice");
    let added = put_state_with_file(&store, "lib.rs", b"new\n", vec![base.id()], "bob");
    let provenance = blame_file(
        &store,
        &added,
        Path::new("lib.rs"),
        BlameSliceLimits::unlimited(),
    )
    .unwrap();
    assert_eq!(principals_at(&provenance, 0), vec!["bob".to_string()]);
}

#[test]
fn insert_delete_replace_keep_surviving_lines() {
    let store = store();
    let base = put_state_with_file(&store, "lib.rs", b"a\nb\nc\n", Vec::new(), "alice");
    let edited = put_state_with_file(&store, "lib.rs", b"a\nX\nc\nY\n", vec![base.id()], "bob");
    let provenance = blame_file(
        &store,
        &edited,
        Path::new("lib.rs"),
        BlameSliceLimits::unlimited(),
    )
    .unwrap();
    assert!(principals_at(&provenance, 0).contains(&"alice".to_string()));
    assert_eq!(principals_at(&provenance, 1), vec!["bob".to_string()]);
    assert!(principals_at(&provenance, 2).contains(&"alice".to_string()));
    assert_eq!(principals_at(&provenance, 3), vec!["bob".to_string()]);
}

#[test]
fn ba_vs_abb_credits_rightmost_new_match() {
    let store = store();
    let base = put_state_with_file(&store, "lib.rs", b"b\na\n", Vec::new(), "alice");
    let edited = put_state_with_file(&store, "lib.rs", b"a\nb\nb\n", vec![base.id()], "bob");
    let provenance = blame_file(
        &store,
        &edited,
        Path::new("lib.rs"),
        BlameSliceLimits::unlimited(),
    )
    .unwrap();
    assert_eq!(principals_at(&provenance, 0), vec!["bob".to_string()]);
    assert_eq!(principals_at(&provenance, 1), vec!["bob".to_string()]);
    assert_eq!(principals_at(&provenance, 2), vec!["alice".to_string()]);
}

#[test]
fn ba_vs_aa_credits_leftmost_new_match() {
    let store = store();
    let base = put_state_with_file(&store, "lib.rs", b"b\na\n", Vec::new(), "alice");
    let edited = put_state_with_file(&store, "lib.rs", b"a\na\n", vec![base.id()], "bob");
    let provenance = blame_file(
        &store,
        &edited,
        Path::new("lib.rs"),
        BlameSliceLimits::unlimited(),
    )
    .unwrap();
    assert_eq!(principals_at(&provenance, 0), vec!["alice".to_string()]);
    assert_eq!(principals_at(&provenance, 1), vec!["bob".to_string()]);
}

#[test]
fn repeated_lines_are_stable() {
    let store = store();
    let base = put_state_with_file(&store, "lib.rs", b"x\na\nx\n", Vec::new(), "alice");
    let edited = put_state_with_file(&store, "lib.rs", b"x\nx\nb\n", vec![base.id()], "bob");
    let first = blame_file(
        &store,
        &edited,
        Path::new("lib.rs"),
        BlameSliceLimits::unlimited(),
    )
    .unwrap();
    let second = blame_file(
        &store,
        &edited,
        Path::new("lib.rs"),
        BlameSliceLimits::unlimited(),
    )
    .unwrap();
    assert_eq!(first, second);
}

#[test]
fn merge_history_first_parent_wins_shared_lines() {
    let store = store();
    let base = put_state_with_file(&store, "lib.rs", b"fn shared() {}\n", Vec::new(), "alice");
    let ours = put_state_with_file(
        &store,
        "lib.rs",
        b"fn shared() {}\nfn from_bob() {}\n",
        vec![base.id()],
        "bob",
    );
    let theirs = put_state_with_file(
        &store,
        "lib.rs",
        b"fn shared() {}\nfn from_carol() {}\n",
        vec![base.id()],
        "carol",
    );
    let merge = put_state_with_file(
        &store,
        "lib.rs",
        b"fn shared() {}\nfn from_bob() {}\nfn from_carol() {}\n",
        vec![ours.id(), theirs.id()],
        "dave",
    );
    let provenance = blame_file(
        &store,
        &merge,
        Path::new("lib.rs"),
        BlameSliceLimits::unlimited(),
    )
    .unwrap();
    assert!(principals_at(&provenance, 0).contains(&"alice".to_string()));
    assert!(principals_at(&provenance, 1).contains(&"bob".to_string()));
    assert!(!principals_at(&provenance, 1).contains(&"dave".to_string()));
    assert!(principals_at(&provenance, 2).contains(&"carol".to_string()));
    assert!(!principals_at(&provenance, 2).contains(&"dave".to_string()));
}

#[test]
fn empty_file_has_origin_and_no_spans() {
    let store = store();
    let state = put_state_with_file(&store, "empty.txt", b"", Vec::new(), "alice");
    let provenance = blame_file(
        &store,
        &state,
        Path::new("empty.txt"),
        BlameSliceLimits::unlimited(),
    )
    .unwrap();
    provenance.validate().unwrap();
    assert_eq!(provenance.line_count, 0);
    assert!(provenance.spans.is_empty());
    assert_eq!(
        provenance.origins[0].attribution.principal.name_lossy(),
        "alice"
    );
}

#[test]
fn no_final_newline_is_one_line() {
    let store = store();
    let state = put_state_with_file(&store, "lib.rs", b"hello", Vec::new(), "alice");
    let provenance = blame_file(
        &store,
        &state,
        Path::new("lib.rs"),
        BlameSliceLimits::unlimited(),
    )
    .unwrap();
    assert_eq!(provenance.line_count, 1);
    assert_eq!(principals_at(&provenance, 0), vec!["alice".to_string()]);
}
