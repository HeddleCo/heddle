// SPDX-License-Identifier: Apache-2.0
use std::path::Path;

use super::*;
use crate::object::Blob;

#[test]
fn test_diff_blobs_identical() {
    let blob = Blob::from("line 1\nline 2\n");
    let diff = diff_blobs(&blob, &blob);

    assert!(diff.iter().all(|l| matches!(l, DiffLine::Context(_))));
}

#[test]
fn test_diff_blobs_added() {
    let old = Blob::from("line 1\nline 2\n");
    let new = Blob::from("line 1\nline 2\nline 3\n");
    let diff = diff_blobs(&old, &new);

    let added: Vec<_> = diff
        .iter()
        .filter(|l| matches!(l, DiffLine::Added(_)))
        .collect();
    assert_eq!(added.len(), 1);
}

#[test]
fn test_diff_blobs_removed() {
    let old = Blob::from("line 1\nline 2\nline 3\n");
    let new = Blob::from("line 1\nline 2\n");
    let diff = diff_blobs(&old, &new);

    let removed: Vec<_> = diff
        .iter()
        .filter(|l| matches!(l, DiffLine::Removed(_)))
        .collect();
    assert_eq!(removed.len(), 1);
}

#[test]
fn test_diff_blobs_does_not_resync_on_later_repeated_line() {
    let old = Blob::from(
        r#"fn first() {
    assert_eq!(
        value,
        Some(default())
    );
}

fn second() {
    assert_eq!(
        other,
        expected
    );
}
"#,
    );
    let new = Blob::from(
        r#"fn first() {
    assert_eq!(value, None);
}

fn second() {
    assert_eq!(
        other,
        expected
    );
}
"#,
    );

    let diff = diff_blobs(&old, &new);
    let first_added_second_fn = diff
        .iter()
        .position(|line| matches!(line, DiffLine::Added(content) if content == "fn second() {"));
    let first_removed_second_fn = diff
        .iter()
        .position(|line| matches!(line, DiffLine::Removed(content) if content == "fn second() {"));

    assert!(
        first_added_second_fn.is_none() && first_removed_second_fn.is_none(),
        "unchanged second function should remain context, got: {diff:?}"
    );
    assert!(
        diff.iter()
            .any(|line| matches!(line, DiffLine::Context(content) if content == "fn second() {")),
        "unchanged repeated block should be context, got: {diff:?}"
    );
}

#[test]
fn test_diff_blobs_keeps_rust_attribute_with_inserted_item() {
    let old = Blob::from(
        r#"fn before() {}

#[test]
fn existing_test() {}
"#,
    );
    let new = Blob::from(
        r#"fn before() {}

#[test]
fn inserted_test() {}

#[test]
fn existing_test() {}
"#,
    );

    let diff = diff_blobs(&old, &new);
    let inserted_fn = diff
        .iter()
        .position(
            |line| matches!(line, DiffLine::Added(content) if content == "fn inserted_test() {}"),
        )
        .expect("inserted function should be added");
    let preceding_added_attribute = diff[..inserted_fn]
        .iter()
        .rev()
        .find(|line| !matches!(line, DiffLine::Added(content) if content.trim().is_empty()));
    assert!(
        matches!(preceding_added_attribute, Some(DiffLine::Added(content)) if content == "#[test]"),
        "inserted function should carry its own added attribute: {diff:?}"
    );

    let existing_fn = diff
        .iter()
        .position(
            |line| matches!(line, DiffLine::Context(content) if content == "fn existing_test() {}"),
        )
        .expect("existing function should remain context");
    let preceding_context_attribute = diff[..existing_fn]
        .iter()
        .rev()
        .find(|line| !matches!(line, DiffLine::Added(content) if content.trim().is_empty()));
    assert!(
        matches!(preceding_context_attribute, Some(DiffLine::Context(content)) if content == "#[test]"),
        "existing function should keep its context attribute: {diff:?}"
    );
}

#[test]
fn test_diff_blobs_keeps_python_decorator_with_inserted_function() {
    let old = Blob::from(
        r#"def before():
    return 1

@pytest.mark.slow
def existing_test():
    pass
"#,
    );
    let new = Blob::from(
        r#"def before():
    return 1

@pytest.mark.slow
def inserted_test():
    pass

@pytest.mark.slow
def existing_test():
    pass
"#,
    );

    let diff = diff_blobs(&old, &new);
    let inserted_fn = diff
        .iter()
        .position(
            |line| matches!(line, DiffLine::Added(content) if content == "def inserted_test():"),
        )
        .expect("inserted function should be added");
    let preceding_added_decorator = diff[..inserted_fn]
        .iter()
        .rev()
        .find(|line| !matches!(line, DiffLine::Added(content) if content.trim().is_empty()));
    assert!(
        matches!(preceding_added_decorator, Some(DiffLine::Added(content)) if content == "@pytest.mark.slow"),
        "inserted Python function should carry its decorator: {diff:?}"
    );

    let existing_fn = diff
        .iter()
        .position(
            |line| matches!(line, DiffLine::Context(content) if content == "def existing_test():"),
        )
        .expect("existing function should remain context");
    let preceding_context_decorator = diff[..existing_fn]
        .iter()
        .rev()
        .find(|line| !matches!(line, DiffLine::Added(content) if content.trim().is_empty()));
    assert!(
        matches!(preceding_context_decorator, Some(DiffLine::Context(content)) if content == "@pytest.mark.slow"),
        "existing Python function should keep its context decorator: {diff:?}"
    );
}

#[test]
fn test_worktree_status_is_clean() {
    let status = WorktreeStatus::default();
    assert!(status.is_clean());
    assert_eq!(status.change_count(), 0);
}

#[test]
fn test_should_ignore() {
    let patterns = vec![".heddle".to_string(), "target".to_string()];

    assert!(super::worktree_ignore::should_ignore(
        Path::new(".heddle"),
        &patterns
    ));
    assert!(super::worktree_ignore::should_ignore(
        Path::new(".heddle/objects"),
        &patterns
    ));
    assert!(super::worktree_ignore::should_ignore(
        Path::new("target"),
        &patterns
    ));
    assert!(super::worktree_ignore::should_ignore(
        Path::new("target/debug"),
        &patterns
    ));
    assert!(!super::worktree_ignore::should_ignore(
        Path::new("src"),
        &patterns
    ));
    assert!(!super::worktree_ignore::should_ignore(
        Path::new("src/main.rs"),
        &patterns
    ));
    assert!(!super::worktree_ignore::should_ignore(
        Path::new("examples/calculator/.heddle/HEAD"),
        &patterns
    ));
}
