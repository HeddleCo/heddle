// SPDX-License-Identifier: Apache-2.0
use similar::{Algorithm, DiffOp};

use super::{
    EqualRun, LineDiffError, LineDiffLimits, lcs_line_matches, scratch_bytes_for_line_counts,
    split_text_lines, visit_lcs_equal_runs,
};
use crate::util::budget::ResourceKind;

fn collect_runs(
    old: &str,
    new: &str,
    limits: LineDiffLimits,
) -> Result<Vec<EqualRun>, LineDiffError> {
    let needed = scratch_bytes_for_line_counts(old.lines().count(), new.lines().count());
    let mut scratch = vec![0u8; needed.max(1)];
    let mut runs = Vec::new();
    visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut scratch,
        limits,
        |run| {
            runs.push(run);
            Ok::<(), std::convert::Infallible>(())
        },
    )?;
    Ok(runs)
}

fn expand_runs(runs: &[EqualRun]) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    for run in runs {
        for offset in 0..run.len {
            matches.push((run.old_start + offset, run.new_start + offset));
        }
    }
    matches
}

fn similar_pairs(old_lines: &[String], new_lines: &[String]) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    for op in similar::capture_diff_slices(Algorithm::Myers, old_lines, new_lines) {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            matches.extend((0..len).map(|offset| (old_index + offset, new_index + offset)));
        }
    }
    matches
}

#[test]
fn line_matches_preserve_simple_alignment() {
    let old = ["a", "b", "c"].map(str::to_string);
    let new = ["a", "x", "c"].map(str::to_string);
    assert_eq!(
        lcs_line_matches(&old, &new).expect("lcs"),
        vec![(0, 0), (2, 2)]
    );
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
fn work_budget_has_exact_line_pair_boundary() {
    let old = "a\nb\nc\n";
    let new = "a\nx\nc\n";
    let needed = scratch_bytes_for_line_counts(3, 3);
    let mut scratch = vec![0u8; needed];

    let over = visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut scratch,
        LineDiffLimits {
            scratch_bytes: needed as u64,
            max_lines: 16,
            max_work: 8,
        },
        |_| Ok::<(), std::convert::Infallible>(()),
    );
    match over {
        Err(LineDiffError::BudgetExceeded(error)) => {
            assert_eq!(error.kind, ResourceKind::Work);
            assert_eq!(error.limit, 8);
            assert_eq!(error.needed, 9);
        }
        other => panic!("expected work budget, got {other:?}"),
    }

    let ok = visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut scratch,
        LineDiffLimits {
            scratch_bytes: needed as u64,
            max_lines: 16,
            max_work: 9,
        },
        |_| Ok::<(), std::convert::Infallible>(()),
    );
    assert!(ok.is_ok(), "{ok:?}");
}

#[test]
fn scratch_budget_has_exact_needed_boundary() {
    let old = "a\nb\n";
    let new = "a\nc\n";
    let needed = scratch_bytes_for_line_counts(2, 2);
    let mut too_small = vec![0u8; needed.saturating_sub(1).max(1)];
    let err = visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut too_small,
        LineDiffLimits {
            scratch_bytes: u64::MAX,
            max_lines: 16,
            max_work: 16,
        },
        |_| Ok::<(), std::convert::Infallible>(()),
    );
    match err {
        Err(LineDiffError::BudgetExceeded(error)) => {
            assert_eq!(error.kind, ResourceKind::ScratchBytes);
            assert_eq!(error.needed, needed as u64);
        }
        other => panic!("expected scratch budget, got {other:?}"),
    }

    let mut exact = vec![0u8; needed];
    visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut exact,
        LineDiffLimits {
            scratch_bytes: needed as u64,
            max_lines: 16,
            max_work: 16,
        },
        |_| Ok::<(), std::convert::Infallible>(()),
    )
    .expect("exact scratch should admit the search");
}

#[test]
fn visitor_cancel_stops_before_later_runs() {
    let old = "a\nb\nc\nd\n";
    let new = "a\nx\nc\nd\n";
    let needed = scratch_bytes_for_line_counts(4, 4);
    let mut scratch = vec![0u8; needed];
    let mut seen = 0usize;
    let err = visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut scratch,
        LineDiffLimits::unlimited(),
        |_| {
            seen += 1;
            if seen == 1 { Err("stop") } else { Ok(()) }
        },
    );
    assert!(matches!(err, Err(LineDiffError::Visitor("stop"))));
    assert_eq!(seen, 1);
}

#[test]
fn line_matches_are_bounded_for_large_files() {
    let old = (0..50_000)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>();
    let mut new = old.clone();
    new[25_000] = "replacement".to_string();

    let matches = lcs_line_matches(&old, &new).expect("lcs");
    assert_eq!(matches.len(), 49_999);
    assert_eq!(matches.first(), Some(&(0, 0)));
    assert_eq!(matches.last(), Some(&(49_999, 49_999)));
    assert!(!matches.contains(&(25_000, 25_000)));
}

#[test]
fn invalid_utf8_is_not_budget_exceeded() {
    let err = visit_lcs_equal_runs(
        &[0xff, 0xfe],
        b"ok",
        &mut [0u8; 64],
        LineDiffLimits::unlimited(),
        |_| Ok::<(), std::convert::Infallible>(()),
    );
    assert!(matches!(err, Err(LineDiffError::InvalidUtf8)));
}
