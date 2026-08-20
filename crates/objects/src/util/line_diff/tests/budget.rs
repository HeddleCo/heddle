// SPDX-License-Identifier: Apache-2.0
use super::visit_usage;
use crate::util::budget::ResourceKind;
use crate::util::line_diff::{
    scratch_bytes_for_line_counts, visit_lcs_equal_runs, LineDiffError, LineDiffLimits,
};

#[test]
fn work_budget_has_exact_consumed_boundary() {
    let old = "a\nb\nc\n";
    let new = "a\nx\nc\n";
    let measured = visit_usage(old, new, LineDiffLimits::unlimited()).expect("unlimited");
    assert!(measured.work > 0, "Myers must charge actual comparisons");

    let over = visit_usage(
        old,
        new,
        LineDiffLimits {
            scratch_bytes: u64::MAX,
            max_lines: 16,
            max_work: measured.work.saturating_sub(1),
        },
    );
    match over {
        Err(LineDiffError::BudgetExceeded(error)) => {
            assert_eq!(error.kind, ResourceKind::Work);
            assert_eq!(error.limit, measured.work.saturating_sub(1));
            assert_eq!(error.needed, measured.work);
        }
        other => panic!("expected work budget, got {other:?}"),
    }

    let ok = visit_usage(
        old,
        new,
        LineDiffLimits {
            scratch_bytes: u64::MAX,
            max_lines: 16,
            max_work: measured.work,
        },
    )
    .expect("exact consumed work should finish");
    assert_eq!(ok.work, measured.work);
}

#[test]
fn scratch_budget_has_exact_needed_boundary() {
    let old = "a\nb\n";
    let new = "a\nc\n";
    let needed = super::super::scratch::aligned_layout_bytes(2, 2);
    let mut too_small = vec![0u8; needed.saturating_sub(1).max(1)];
    let mut budget = LineDiffLimits {
        scratch_bytes: u64::MAX,
        max_lines: 16,
        max_work: 16,
    }
    .budget(too_small.len());
    let err = visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut too_small,
        &mut budget,
        |_| Ok::<(), std::convert::Infallible>(()),
    );
    match err {
        Err(LineDiffError::BudgetExceeded(error)) => {
            assert_eq!(error.kind, ResourceKind::ScratchBytes);
        }
        other => panic!("expected scratch budget, got {other:?}"),
    }

    let mut exact = vec![0u8; scratch_bytes_for_line_counts(2, 2)];
    let mut budget = LineDiffLimits {
        scratch_bytes: exact.len() as u64,
        max_lines: 16,
        max_work: 16,
    }
    .budget(exact.len());
    visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut exact,
        &mut budget,
        |_| Ok::<(), std::convert::Infallible>(()),
    )
    .expect("exact scratch should admit the search");
}

#[test]
fn misaligned_scratch_is_not_ub() {
    let old = "a\nb\n";
    let new = "a\nc\n";
    let needed = scratch_bytes_for_line_counts(2, 2);
    let mut backing = vec![0u8; needed + 8];
    let scratch = &mut backing[1..];
    let mut budget = LineDiffLimits::unlimited().budget(scratch.len());
    visit_lcs_equal_runs(old.as_bytes(), new.as_bytes(), scratch, &mut budget, |_| {
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("actual-pointer alignment must admit a misaligned slice");
}

#[test]
fn visitor_cancel_stops_before_later_runs() {
    let old = "a\nb\nc\nd\n";
    let new = "a\nx\nc\nd\n";
    let full = visit_usage(old, new, LineDiffLimits::unlimited()).expect("unlimited");
    let needed = scratch_bytes_for_line_counts(4, 4);
    let mut scratch = vec![0u8; needed];
    let mut budget = LineDiffLimits::unlimited().budget(scratch.len());
    let mut seen = 0usize;
    let err = visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut scratch,
        &mut budget,
        |_| {
            seen += 1;
            if seen == 1 {
                Err("stop")
            } else {
                Ok(())
            }
        },
    );
    assert!(matches!(err, Err(LineDiffError::Visitor("stop"))));
    assert_eq!(seen, 1);
    assert!(
        budget.used().work < full.work,
        "cancel must stop Myers, not drain the search then visit"
    );
}

#[test]
fn line_matches_are_bounded_for_large_files() {
    let old = (0..50_000)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut new = old.clone();
    new = new.replacen("line 25000", "replacement", 1);
    let needed = scratch_bytes_for_line_counts(50_000, 50_000);
    let mut scratch = vec![0u8; needed];
    let mut budget = LineDiffLimits::unlimited().budget(scratch.len());
    let mut matched = 0usize;
    let mut hit_replaced = false;
    visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut scratch,
        &mut budget,
        |run| {
            matched += run.len;
            if run.old_start <= 25_000 && run.old_start + run.len > 25_000 {
                hit_replaced = true;
            }
            Ok::<(), std::convert::Infallible>(())
        },
    )
    .expect("lcs");
    assert_eq!(matched, 49_999);
    assert!(!hit_replaced);
}

#[test]
fn invalid_utf8_is_not_budget_exceeded() {
    let err = visit_lcs_equal_runs(
        &[0xff, 0xfe],
        b"ok",
        &mut [0u8; 64],
        &mut LineDiffLimits::unlimited().budget(64),
        |_| Ok::<(), std::convert::Infallible>(()),
    );
    assert!(matches!(err, Err(LineDiffError::InvalidUtf8)));
}
