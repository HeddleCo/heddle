// SPDX-License-Identifier: Apache-2.0
//! Unit tests for the native hunk-level merge engine.

use super::{ConflictMarkers, MergeOutcome, text_hunk_merge, text_hunk_merge_with_markers};

/// Count `<<<<<<<` marker occurrences at column 0 (one per conflict triple).
fn count_conflicts(bytes: &[u8]) -> usize {
    let s = std::str::from_utf8(bytes).expect("merge output should be utf8");
    s.lines().filter(|line| line.starts_with("<<<<<<<")).count()
}

/// Strict check: every `<<<<<<<`, `=======`, `>>>>>>>` marker sits at column 0.
fn markers_well_formed(bytes: &[u8]) -> bool {
    let s = std::str::from_utf8(bytes).expect("merge output should be utf8");
    for line in s.lines() {
        for marker in ["<<<<<<<", "=======", ">>>>>>>"] {
            if line.contains(marker) && !line.starts_with(marker) {
                return false;
            }
        }
    }
    true
}

// =====================================================================
// Red-commit fixture 1: semantic declines, line ranges DISJOINT.
//
// Two hunks edited on opposite ends of the file. Current implementation
// (single-range merger) handles the simplest disjoint case but bails on
// multi-hunk-per-side. This fixture pushes a multi-hunk case to force
// the new engine. Expect: Clean, both edits present, no markers.
// =====================================================================
#[test]
fn disjoint_multi_hunks_auto_resolve() {
    let base = b"line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\n";
    // Ours: edits lines 1 and 7 (two disjoint hunks on our side)
    let ours = b"OUR-1\nline 2\nline 3\nline 4\nline 5\nline 6\nOUR-7\nline 8\n";
    // Theirs: edits line 4 only
    let theirs = b"line 1\nline 2\nline 3\nTHEIR-4\nline 5\nline 6\nline 7\nline 8\n";

    let outcome = text_hunk_merge(base, ours, theirs);
    match outcome {
        MergeOutcome::Clean(merged) => {
            let text = String::from_utf8(merged).unwrap();
            assert!(text.contains("OUR-1\n"), "missing ours hunk 1: {text}");
            assert!(text.contains("OUR-7\n"), "missing ours hunk 2: {text}");
            assert!(text.contains("THEIR-4\n"), "missing theirs hunk: {text}");
            assert!(!text.contains("<<<<<<<"), "should have no markers: {text}");
        }
        other => panic!("expected Clean, got {other:?}"),
    }
}

// =====================================================================
// Red-commit fixture 2: semantic declines, line ranges OVERLAP.
//
// Both sides modify the same line. Expect: Conflicts with per-hunk
// markers (not whole-file), conflict_count > 0, surrounding unchanged
// lines preserved verbatim.
// =====================================================================
#[test]
fn overlapping_hunks_produce_hunk_markers() {
    let base = b"line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\n";
    let ours = b"line 1\nline 2\nOUR-3\nline 4\nline 5\nline 6\nline 7\nline 8\n";
    let theirs = b"line 1\nline 2\nTHEIR-3\nline 4\nline 5\nline 6\nline 7\nline 8\n";

    let outcome = text_hunk_merge(base, ours, theirs);
    match outcome {
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            conflict_count,
        } => {
            assert!(
                conflict_count >= 1,
                "expected at least one conflict, got {conflict_count}"
            );
            let text = String::from_utf8(merged_bytes_with_markers).unwrap();
            assert!(
                markers_well_formed(text.as_bytes()),
                "markers should be at column 0: {text}"
            );
            // The unchanged outer lines must still appear verbatim — this is
            // the heart of "hunk-level not whole-file".
            assert!(text.starts_with("line 1\nline 2\n"), "prefix lost: {text}");
            assert!(
                text.ends_with("line 4\nline 5\nline 6\nline 7\nline 8\n"),
                "suffix lost: {text}"
            );
            assert!(text.contains("OUR-3"), "missing ours conflict body: {text}");
            assert!(
                text.contains("THEIR-3"),
                "missing theirs conflict body: {text}"
            );
        }
        other => panic!("expected Conflicts, got {other:?}"),
    }
}

// =====================================================================
// Red-commit fixture 3: heddle#54-shape large-file with disjoint
// 3-line changes on each side.
//
// The trip report from heddle#54 had a 185-line file where heddle's
// merger collided whole-file while git produced 3 small hunks. This
// fixture reconstructs that shape: a 185-line file, ours edits lines
// 30-32, theirs edits lines 150-152 (clearly disjoint). Expect: clean
// resolution, both 3-line changes present, no markers.
//
// To prove "not the whole file", we additionally assert that any
// conflict markers (there shouldn't be any here) wouldn't subsume the
// untouched bulk of the file.
// =====================================================================
#[test]
fn large_file_disjoint_3line_changes_resolve_cleanly() {
    let base: String = (1..=185).map(|i| format!("line {i}\n")).collect();

    // Ours: replace lines 30-32 with three OUR lines
    let mut ours = String::new();
    for i in 1..=185 {
        if (30..=32).contains(&i) {
            ours.push_str(&format!("OUR-{i}\n"));
        } else {
            ours.push_str(&format!("line {i}\n"));
        }
    }

    // Theirs: replace lines 150-152 with three THEIR lines
    let mut theirs = String::new();
    for i in 1..=185 {
        if (150..=152).contains(&i) {
            theirs.push_str(&format!("THEIR-{i}\n"));
        } else {
            theirs.push_str(&format!("line {i}\n"));
        }
    }

    let outcome = text_hunk_merge(base.as_bytes(), ours.as_bytes(), theirs.as_bytes());
    match outcome {
        MergeOutcome::Clean(merged) => {
            let text = String::from_utf8(merged).unwrap();
            // Both 3-line edits landed.
            for i in 30..=32 {
                assert!(text.contains(&format!("OUR-{i}\n")), "missing OUR-{i}: ...");
            }
            for i in 150..=152 {
                assert!(
                    text.contains(&format!("THEIR-{i}\n")),
                    "missing THEIR-{i}: ..."
                );
            }
            // Outer untouched lines remain verbatim.
            assert!(text.contains("line 1\n"));
            assert!(text.contains("line 100\n"));
            assert!(text.contains("line 185\n"));
            assert!(!text.contains("<<<<<<<"), "should have no markers");
        }
        other => panic!("expected Clean on disjoint hunks, got {other:?}"),
    }
}

// =====================================================================
// Edge cases beyond the brief's three red-commit fixtures.
// =====================================================================

#[test]
fn identical_edit_on_both_sides_is_clean() {
    let base = b"a\nb\nc\n";
    let ours = b"a\nX\nc\n";
    let theirs = b"a\nX\nc\n";
    match text_hunk_merge(base, ours, theirs) {
        MergeOutcome::Clean(out) => assert_eq!(out, b"a\nX\nc\n"),
        other => panic!("expected Clean, got {other:?}"),
    }
}

#[test]
fn one_side_unchanged_takes_other() {
    let base = b"a\nb\nc\n";
    let ours = base.to_vec();
    let theirs = b"a\nB\nc\n";
    match text_hunk_merge(base, &ours, theirs) {
        MergeOutcome::Clean(out) => assert_eq!(out, b"a\nB\nc\n"),
        other => panic!("expected Clean (theirs), got {other:?}"),
    }
}

#[test]
fn binary_inputs_short_circuit() {
    let base = b"a\nb\nc\n";
    let ours = b"a\nb\n\0\n";
    let theirs = b"a\nb\nC\n";
    assert!(matches!(
        text_hunk_merge(base, ours, theirs),
        MergeOutcome::Binary
    ));
}

#[test]
fn marker_labels_threaded_through() {
    let base = b"a\nb\nc\n";
    let ours = b"a\nOUR\nc\n";
    let theirs = b"a\nTHEIR\nc\n";
    let markers = ConflictMarkers {
        ours: "feat/branch",
        theirs: "main",
    };
    let outcome = text_hunk_merge_with_markers(base, ours, theirs, markers);
    let MergeOutcome::Conflicts {
        merged_bytes_with_markers,
        ..
    } = outcome
    else {
        panic!("expected Conflicts");
    };
    let text = String::from_utf8(merged_bytes_with_markers).unwrap();
    assert!(text.contains("<<<<<<< feat/branch\n"), "ours label: {text}");
    assert!(text.contains(">>>>>>> main\n"), "theirs label: {text}");
}

#[test]
fn trailing_newline_divergence_does_not_conflict() {
    // Base has trailing newline; ours adds content keeping it; theirs adds
    // content but drops the trailing newline. Should still merge cleanly
    // on disjoint edits.
    let base = b"a\nb\nc\n";
    let ours = b"OUR-a\nb\nc\n";
    let theirs = b"a\nb\nTHEIR-c"; // no trailing newline
    match text_hunk_merge(base, ours, theirs) {
        MergeOutcome::Clean(out) => {
            let text = String::from_utf8(out).unwrap();
            assert!(text.contains("OUR-a"));
            assert!(text.contains("THEIR-c"));
        }
        other => panic!(
            "expected Clean on disjoint hunks even with trailing-newline divergence, got {other:?}"
        ),
    }
}

#[test]
fn two_separate_conflict_hunks_emit_two_marker_blocks() {
    // Both sides edit two different hunks, but at one hunk both differ.
    // Expect 1 conflict block, surrounded by the resolved hunk.
    let base = b"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
    let ours = b"a\nOURB\nc\nd\ne\nf\ng\nOURh\ni\nj\n";
    let theirs = b"a\nTHEIRB\nc\nd\ne\nf\ng\nTHEIRh\ni\nj\n";

    let MergeOutcome::Conflicts {
        merged_bytes_with_markers,
        conflict_count,
    } = text_hunk_merge(base, ours, theirs)
    else {
        panic!("expected Conflicts");
    };
    assert_eq!(conflict_count, 2, "should see two distinct conflict hunks");
    let count = count_conflicts(&merged_bytes_with_markers);
    assert_eq!(count, 2);
}

#[test]
fn empty_inputs_clean() {
    assert!(matches!(text_hunk_merge(b"", b"", b""), MergeOutcome::Clean(v) if v.is_empty()));
}

// heddle-specific UX: when both sides insert different content at the *same*
// anchor point (empty base slice for the hunk), concatenate rather than
// conflict. Preserves the parallel-thread append flow validated by the
// state-management integration test
// `test_merge_auto_merges_non_overlapping_same_file_appends_from_threads`.
#[test]
fn same_anchor_insertions_concat_rather_than_conflict() {
    let base = b"a\nb\n";
    let ours = b"a\nOUR-1\nOUR-2\nb\n";
    let theirs = b"a\nTHEIR-1\nTHEIR-2\nb\n";
    match text_hunk_merge(base, ours, theirs) {
        MergeOutcome::Clean(out) => {
            let text = String::from_utf8(out).unwrap();
            assert!(
                text == "a\nOUR-1\nOUR-2\nTHEIR-1\nTHEIR-2\nb\n",
                "expected concat of ours-then-theirs at same anchor; got: {text:?}"
            );
        }
        other => panic!("expected Clean (concat insertions), got {other:?}"),
    }
}

// =====================================================================
// Codex r1 — P1: split insertion+edit hunks before declaring conflicts.
//
// One side ONLY inserts at an anchor; the other side ONLY edits an
// adjacent base line. The patches don't overlap and git's merge-file
// resolves them cleanly; before the fix, our hunk classifier reported
// a conflict because the four-way classification only handles exact
// `ours == base` / `theirs == base` / `ours == theirs` equalities.
// =====================================================================
#[test]
fn insertion_plus_adjacent_edit_resolves_cleanly() {
    let base = b"X\nY\n";
    let ours = b"X\nNEW\nY\n"; // inserts NEW between X and Y
    let theirs = b"X\nY'\n"; // edits Y → Y'
    match text_hunk_merge(base, ours, theirs) {
        MergeOutcome::Clean(out) => {
            assert_eq!(out, b"X\nNEW\nY'\n", "expected X NEW Y' composition");
        }
        other => panic!("expected Clean from insertion+edit compose, got {other:?}"),
    }
}

#[test]
fn insertion_plus_adjacent_edit_symmetric() {
    // Swap which side inserts and which edits — same expected result.
    let base = b"X\nY\n";
    let ours = b"X\nY'\n"; // edits Y → Y'
    let theirs = b"X\nNEW\nY\n"; // inserts NEW
    match text_hunk_merge(base, ours, theirs) {
        MergeOutcome::Clean(out) => {
            assert_eq!(out, b"X\nNEW\nY'\n", "expected X NEW Y' composition");
        }
        other => panic!("expected Clean from insertion+edit compose, got {other:?}"),
    }
}

#[test]
fn insertion_plus_adjacent_delete_resolves_cleanly() {
    // Composer also covers insertion + delete: ours inserts NEW, theirs
    // deletes Y. Output: X NEW (Y is gone).
    let base = b"X\nY\n";
    let ours = b"X\nNEW\nY\n";
    let theirs = b"X\n"; // Y deleted
    match text_hunk_merge(base, ours, theirs) {
        MergeOutcome::Clean(out) => {
            assert_eq!(out, b"X\nNEW\n", "expected X NEW (Y deleted)");
        }
        other => panic!("expected Clean from insertion+delete compose, got {other:?}"),
    }
}

#[test]
fn insertion_at_eof_plus_edit_resolves_cleanly() {
    // Ours appends NEW after base; theirs replaces last base line.
    // Composer must place the trailing insertion correctly.
    let base = b"X\nY\n";
    let ours = b"X\nY\nNEW\n"; // appends NEW at EOF
    let theirs = b"X\nY'\n"; // edits Y
    match text_hunk_merge(base, ours, theirs) {
        MergeOutcome::Clean(out) => {
            assert_eq!(out, b"X\nY'\nNEW\n", "trailing insertion must follow edit");
        }
        other => panic!("expected Clean from insertion+edit (EOF) compose, got {other:?}"),
    }
}

#[test]
fn insertion_at_bof_plus_edit_resolves_cleanly() {
    // Ours prepends NEW at start; theirs edits last base line.
    let base = b"X\nY\n";
    let ours = b"NEW\nX\nY\n";
    let theirs = b"X\nY'\n";
    match text_hunk_merge(base, ours, theirs) {
        MergeOutcome::Clean(out) => {
            assert_eq!(out, b"NEW\nX\nY'\n", "leading insertion must precede edits");
        }
        other => panic!("expected Clean from insertion+edit (BOF) compose, got {other:?}"),
    }
}

#[test]
fn both_sides_modify_same_line_still_conflicts() {
    // Sanity: composer must NOT auto-resolve when both sides modify the
    // same base line. This is the canonical conflict.
    let base = b"X\nY\n";
    let ours = b"X\nY'\n";
    let theirs = b"X\nY''\n";
    match text_hunk_merge(base, ours, theirs) {
        MergeOutcome::Conflicts { conflict_count, .. } => {
            assert_eq!(conflict_count, 1, "expected exactly one conflict");
        }
        other => panic!("expected Conflicts on both-sides-modify-Y, got {other:?}"),
    }
}

// =====================================================================
// Codex r1 — P2: respect line-ending differences in trailing-ws compare.
//
// `trailing_ws_equal` previously stripped line endings before comparing,
// so a CRLF-only side and an LF-only side editing the same line were
// folded into "whitespace-equivalent" and auto-resolved via prefer_clean.
// That hides genuine cross-platform divergence; line endings are
// load-bearing.
// =====================================================================
#[test]
fn crlf_vs_lf_on_same_line_conflicts() {
    // base has LF endings. ours keeps LF but edits Y. theirs uses CRLF
    // for the same edit. The "edit" content is identical mod line endings;
    // the previous buggy `trailing_ws_equal` would mark these equivalent.
    let base = b"X\nY\n";
    let ours = b"X\nY-edit\n";
    let theirs = b"X\r\nY-edit\r\n";
    match text_hunk_merge(base, ours, theirs) {
        MergeOutcome::Conflicts { conflict_count, .. } => {
            assert!(
                conflict_count >= 1,
                "CRLF vs LF must conflict, got {conflict_count}"
            );
        }
        // Allow Clean ONLY if both ends actually agree byte-for-byte (they
        // don't, in this fixture). Anything that silently picks one is a
        // regression of the documented line-ending policy.
        other => panic!("expected Conflicts on CRLF vs LF divergence, got {other:?}"),
    }
}

#[test]
fn trailing_space_only_difference_still_auto_resolves_with_same_endings() {
    // Sanity: the trailing-whitespace folding still works when line
    // endings agree on both sides. ours has trailing space, theirs has
    // none — prefer_clean picks the cleaner one.
    let base = b"X\nY\n";
    let ours = b"X\nY-edit  \n"; // two trailing spaces
    let theirs = b"X\nY-edit\n";
    match text_hunk_merge(base, ours, theirs) {
        MergeOutcome::Clean(out) => {
            assert_eq!(out, b"X\nY-edit\n", "prefer_clean must pick no-trailing-space");
        }
        other => panic!("expected Clean (prefer_clean), got {other:?}"),
    }
}

/// Benchmark-style timing test for the no-conflict path on a heddle#54-shape
/// large file. Prints elapsed time and asserts it under a generous budget so
/// regressions show up loud. The previous `merge_single_line_ranges` walked
/// prefix/suffix in O(n); the new layered path adds two LCS diffs (Histogram
/// algorithm in `similar`) on top, so this guards against any
/// pathological-input quadratic regression.
#[test]
fn bench_no_conflict_disjoint_hunks_large_file() {
    // 1000-line base, ours edits 3 disjoint 5-line hunks, theirs edits 3
    // different disjoint 5-line hunks. No overlap — Clean expected.
    let base: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
    let mut ours = String::new();
    for i in 1..=1000 {
        let in_our_hunk = (100..=104).contains(&i)
            || (400..=404).contains(&i)
            || (700..=704).contains(&i);
        ours.push_str(&if in_our_hunk {
            format!("OUR-{i}\n")
        } else {
            format!("line {i}\n")
        });
    }
    let mut theirs = String::new();
    for i in 1..=1000 {
        let in_their_hunk = (200..=204).contains(&i)
            || (500..=504).contains(&i)
            || (800..=804).contains(&i);
        theirs.push_str(&if in_their_hunk {
            format!("THEIR-{i}\n")
        } else {
            format!("line {i}\n")
        });
    }

    // Warm up to avoid measuring first-call setup costs.
    for _ in 0..3 {
        let _ = text_hunk_merge(base.as_bytes(), ours.as_bytes(), theirs.as_bytes());
    }

    let start = std::time::Instant::now();
    let iters = 50;
    for _ in 0..iters {
        let outcome = text_hunk_merge(base.as_bytes(), ours.as_bytes(), theirs.as_bytes());
        let MergeOutcome::Clean(_) = outcome else {
            panic!("expected Clean on disjoint multi-hunk fixture");
        };
    }
    let total = start.elapsed();
    let per = total / iters;
    println!(
        "merge::bench_no_conflict_disjoint_hunks_large_file: \
         {iters}x 1000-line/3-hunk-per-side disjoint merge in {total:?} \
         (~{per:?} per call)"
    );
    // Budget: 50 ms/call in release, 500 ms/call in debug. The bench guards
    // against pathological regression, not absolute speed — debug mode adds
    // ~15× overhead from the unoptimised LCS path. Real-user-facing builds
    // are always release; this asserts both stay in a sane range.
    let budget = if cfg!(debug_assertions) {
        std::time::Duration::from_millis(500)
    } else {
        std::time::Duration::from_millis(50)
    };
    assert!(
        per < budget,
        "merge slower than {budget:?} per call on 1000-line disjoint input: {per:?}"
    );
}

// Append-to-EOF on both sides with different content — same UX rule.
#[test]
fn same_anchor_eof_appends_concat() {
    let base = b"a\nb\n";
    let ours = b"a\nb\nOUR\n";
    let theirs = b"a\nb\nTHEIR\n";
    match text_hunk_merge(base, ours, theirs) {
        MergeOutcome::Clean(out) => {
            let text = String::from_utf8(out).unwrap();
            assert!(text.contains("OUR"));
            assert!(text.contains("THEIR"));
            assert!(!text.contains("<<<<<<<"));
        }
        other => panic!("expected Clean (concat EOF appends), got {other:?}"),
    }
}
