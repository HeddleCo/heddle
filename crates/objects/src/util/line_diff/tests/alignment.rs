// SPDX-License-Identifier: Apache-2.0
use super::{collect_runs, expand_runs, similar_pairs};
use crate::util::line_diff::{split_text_lines, EqualRun, LineDiffLimits};

#[test]
fn line_matches_preserve_simple_alignment() {
    let runs = collect_runs("a\nb\nc\n", "a\nx\nc\n", LineDiffLimits::unlimited()).unwrap();
    assert_eq!(expand_runs(&runs), vec![(0, 0), (2, 2)]);
}

#[test]
fn equal_runs_are_ascending_and_coalesced() {
    let runs = collect_runs("a\nb\nc\n", "a\nx\nc\n", LineDiffLimits::unlimited()).unwrap();
    assert_eq!(
        runs,
        vec![
            EqualRun {
                old_start: 0,
                new_start: 0,
                len: 1
            },
            EqualRun {
                old_start: 2,
                new_start: 2,
                len: 1
            }
        ]
    );
}

#[test]
fn no_final_newline_matches_str_lines() {
    assert_eq!(
        split_text_lines(b"a\nb"),
        Some(vec!["a".into(), "b".into()])
    );
    let runs = collect_runs("a\nb", "a\nb", LineDiffLimits::unlimited()).unwrap();
    assert_eq!(
        runs,
        vec![EqualRun {
            old_start: 0,
            new_start: 0,
            len: 2
        }]
    );
}

#[test]
fn empty_inputs_emit_no_runs() {
    let runs = collect_runs("", "", LineDiffLimits::unlimited()).unwrap();
    assert!(runs.is_empty());
}

#[test]
fn trailing_empty_line_is_not_dropped() {
    let old = "a\n\n";
    let new = "a\n\n";
    assert_eq!(old.lines().count(), 2);
    assert_eq!(
        ["a".to_string(), String::new()].join("\n").lines().count(),
        1
    );
    let runs = collect_runs(old, new, LineDiffLimits::unlimited()).unwrap();
    assert_eq!(
        runs,
        vec![EqualRun {
            old_start: 0,
            new_start: 0,
            len: 2
        }]
    );
}

#[test]
fn repeated_line_tie_break_is_deterministic() {
    let old = "x\na\nx\nb\nx\n";
    let new = "x\nx\nc\nx\n";
    let first = collect_runs(old, new, LineDiffLimits::unlimited()).unwrap();
    let second = collect_runs(old, new, LineDiffLimits::unlimited()).unwrap();
    assert_eq!(first, second);

    let old_lines = split_text_lines(old.as_bytes()).unwrap();
    let new_lines = split_text_lines(new.as_bytes()).unwrap();
    assert_eq!(expand_runs(&first), similar_pairs(&old_lines, &new_lines));
}

#[test]
fn ba_vs_aa_matches_similar_leftmost_new() {
    let old = "b\na\n";
    let new = "a\na\n";
    let runs = collect_runs(old, new, LineDiffLimits::unlimited()).unwrap();
    let pairs = expand_runs(&runs);
    let old_lines = split_text_lines(old.as_bytes()).unwrap();
    let new_lines = split_text_lines(new.as_bytes()).unwrap();
    let similar = similar_pairs(&old_lines, &new_lines);
    assert_eq!(pairs, similar, "ours={pairs:?} similar={similar:?}");
    assert_eq!(pairs, vec![(1, 0)]);
}

#[test]
fn partial_slide_does_not_move_the_unmatched_tail() {
    let old = "x\ny\na\nb\n";
    let new = "a\nz\na\nb\n";
    let runs = collect_runs(old, new, LineDiffLimits::unlimited()).unwrap();
    let pairs = expand_runs(&runs);
    let old_lines = split_text_lines(old.as_bytes()).unwrap();
    let new_lines = split_text_lines(new.as_bytes()).unwrap();
    let similar = similar_pairs(&old_lines, &new_lines);
    assert_eq!(pairs, similar, "ours={pairs:?} similar={similar:?}");
    assert!(
        !pairs.contains(&(3, 1)),
        "partial slide must not claim b==z: {pairs:?}"
    );
}

#[test]
fn ba_vs_abb_matches_similar_rightmost_new() {
    let old = "b\na\n";
    let new = "a\nb\nb\n";
    let runs = collect_runs(old, new, LineDiffLimits::unlimited()).unwrap();
    let pairs = expand_runs(&runs);
    let old_lines = split_text_lines(old.as_bytes()).unwrap();
    let new_lines = split_text_lines(new.as_bytes()).unwrap();
    let similar = similar_pairs(&old_lines, &new_lines);
    assert_eq!(pairs, similar, "ours={pairs:?} similar={similar:?}");
    assert_eq!(pairs, vec![(0, 2)]);
}

#[test]
fn ab_vs_abb_slides_repeated_suffix_right() {
    let old = "a\nb\n";
    let new = "a\nb\nb\n";
    let runs = collect_runs(old, new, LineDiffLimits::unlimited()).unwrap();
    let pairs = expand_runs(&runs);
    let old_lines = split_text_lines(old.as_bytes()).unwrap();
    let new_lines = split_text_lines(new.as_bytes()).unwrap();
    let similar = similar_pairs(&old_lines, &new_lines);
    assert_eq!(pairs, similar, "ours={pairs:?} similar={similar:?}");
    assert_eq!(pairs, vec![(0, 0), (1, 2)]);
}

#[test]
fn max_d_no_common_lines_does_not_panic() {
    let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
    let new = "1\n2\n3\n4\n5\n6\n7\n8\n";
    let runs = collect_runs(old, new, LineDiffLimits::unlimited()).unwrap();
    assert!(runs.is_empty());
}
