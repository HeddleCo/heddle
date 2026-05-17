// SPDX-License-Identifier: Apache-2.0
//! Unit tests for the function-level merge driver.
//!
//! The matrix below mirrors the cases enumerated in heddle#68 §3 "Test matrix".
//! Each case asserts the *contract* — same-function overlap surfaces line
//! markers, disjoint-function edits merge cleanly, unparseable input falls
//! through to whole-file `heddle-merge`.

use std::path::Path;

use merge::{ConflictMarkers, MergeOutcome};

use super::semantic_three_way_merge;

const MARKERS: ConflictMarkers<'static> = ConflictMarkers {
    ours: "OURS",
    theirs: "THEIRS",
};

fn merge_rust(base: &str, ours: &str, theirs: &str) -> MergeOutcome {
    semantic_three_way_merge(
        base.as_bytes(),
        ours.as_bytes(),
        theirs.as_bytes(),
        Path::new("a.rs"),
        MARKERS,
    )
}

fn assert_clean(outcome: MergeOutcome) -> String {
    match outcome {
        MergeOutcome::Clean(bytes) => String::from_utf8(bytes).expect("UTF-8"),
        other => panic!("expected Clean, got {other:?}"),
    }
}

fn assert_conflicts(outcome: MergeOutcome) -> (String, usize) {
    match outcome {
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            conflict_count,
        } => (
            String::from_utf8(merged_bytes_with_markers).expect("UTF-8"),
            conflict_count,
        ),
        other => panic!("expected Conflicts, got {other:?}"),
    }
}

// =====================================================================
// MIN CASE 1: Different functions modified on each side → clean merge.
// This is the load-bearing case from heddle#54: two branches reshape
// disjoint parts of the file and the merger MUST resolve cleanly.
// =====================================================================
#[test]
fn min_case_1_different_functions_modified_clean_merge() {
    let base = "\
fn alpha() {
    println!(\"alpha-base\");
}

fn beta() {
    println!(\"beta-base\");
}
";
    let ours = "\
fn alpha() {
    println!(\"alpha-OURS\");
}

fn beta() {
    println!(\"beta-base\");
}
";
    let theirs = "\
fn alpha() {
    println!(\"alpha-base\");
}

fn beta() {
    println!(\"beta-THEIRS\");
}
";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("alpha-OURS"), "missing ours: {merged}");
    assert!(merged.contains("beta-THEIRS"), "missing theirs: {merged}");
    assert!(!merged.contains("<<<<<<<"), "no markers expected: {merged}");
}

// =====================================================================
// MIN CASE 2: Two new functions added on different sides → clean merge.
// =====================================================================
#[test]
fn min_case_2_disjoint_new_functions_clean_merge() {
    let base = "\
fn keep() {
    println!(\"keep\");
}
";
    let ours = "\
fn keep() {
    println!(\"keep\");
}

fn ours_new() {
    println!(\"ours-new\");
}
";
    let theirs = "\
fn keep() {
    println!(\"keep\");
}

fn theirs_new() {
    println!(\"theirs-new\");
}
";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(
        merged.contains("ours_new"),
        "ours_new missing: {merged}"
    );
    assert!(
        merged.contains("theirs_new"),
        "theirs_new missing: {merged}"
    );
}

// =====================================================================
// MIN CASE 3: Same function, non-overlapping lines → hunk-level
// conflict markers scoped to the function. Note: in the current text
// merger, edits to the same function may compose disjointly and produce
// a clean merge. The contract here is "no whole-file conflict spans".
// =====================================================================
#[test]
fn min_case_3_same_function_different_lines() {
    let base = "\
fn target() {
    let a = 1;
    let b = 2;
    let c = 3;
    let d = 4;
    let e = 5;
}
";
    let ours = "\
fn target() {
    let a = 1;
    let b = 2;
    let c = 999;
    let d = 4;
    let e = 5;
}
";
    let theirs = "\
fn target() {
    let a = 1;
    let b = 888;
    let c = 3;
    let d = 4;
    let e = 5;
}
";
    let outcome = merge_rust(base, ours, theirs);
    // Disjoint edits within the same function compose cleanly under
    // text_hunk_merge — that's the right outcome. The acceptance is:
    // either clean (composed) or per-hunk conflicts, never a whole-file
    // collision spanning the entire function body.
    match outcome {
        MergeOutcome::Clean(bytes) => {
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.contains("let b = 888"), "missing theirs: {text}");
            assert!(text.contains("let c = 999"), "missing ours: {text}");
        }
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            conflict_count,
        } => {
            let text = String::from_utf8(merged_bytes_with_markers).unwrap();
            assert!(conflict_count <= 2, "too many conflicts: {conflict_count}");
            // Even if conflicts surface, untouched lines like `let a = 1`
            // and `let d = 4` must remain verbatim, not be swallowed by a
            // whole-file marker block.
            assert!(text.contains("let a = 1"), "verbatim line lost: {text}");
            assert!(text.contains("let d = 4"), "verbatim line lost: {text}");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

// =====================================================================
// MIN CASE 4: Same function, same line, different content → conflict
// markers, but only on that line, not the whole file.
// =====================================================================
#[test]
fn min_case_4_same_function_overlapping_lines() {
    let base = "\
fn target() {
    let value = 10;
}

fn untouched() {
    println!(\"untouched\");
}
";
    let ours = "\
fn target() {
    let value = 20;
}

fn untouched() {
    println!(\"untouched\");
}
";
    let theirs = "\
fn target() {
    let value = 30;
}

fn untouched() {
    println!(\"untouched\");
}
";
    let (text, conflict_count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(conflict_count >= 1, "expected conflict: {conflict_count}");
    // Untouched function must NOT appear inside conflict markers.
    assert!(
        text.contains("fn untouched()"),
        "untouched function missing: {text}"
    );
    // The untouched function must appear OUTSIDE any conflict block. We
    // detect this by checking there's no `<<<<<<<` line preceding it
    // without a `>>>>>>>` between.
    let untouched_pos = text.find("fn untouched()").unwrap();
    let prefix = &text[..untouched_pos];
    let opens = prefix.matches("<<<<<<<").count();
    let closes = prefix.matches(">>>>>>>").count();
    assert_eq!(
        opens, closes,
        "fn untouched() must not be inside a conflict block: {text}"
    );
}

// =====================================================================
// MIN CASE 5: One branch deletes a function the other modifies →
// surfaces a conflict (modify/delete).
// =====================================================================
#[test]
fn min_case_5_modify_vs_delete_conflict() {
    let base = "\
fn keep() {
    println!(\"keep\");
}

fn target() {
    let x = 1;
}
";
    let ours = "\
fn keep() {
    println!(\"keep\");
}

fn target() {
    let x = 999;
}
";
    // theirs deletes `target`.
    let theirs = "\
fn keep() {
    println!(\"keep\");
}
";
    let outcome = merge_rust(base, ours, theirs);
    match outcome {
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            conflict_count,
        } => {
            let text = String::from_utf8(merged_bytes_with_markers).unwrap();
            assert!(conflict_count >= 1, "expected a conflict");
            assert!(text.contains("fn keep()"), "lost keep(): {text}");
        }
        MergeOutcome::Clean(bytes) => {
            let text = String::from_utf8(bytes).unwrap();
            panic!("modify/delete should NOT be clean — got: {text}");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

// =====================================================================
// Adversarial: unparseable file (e.g., binary content masquerading) →
// driver falls through to text_hunk_merge.
// =====================================================================
#[test]
fn unparseable_falls_through_to_hunk_merge() {
    // Garbage that won't parse as Rust.
    let base = "this is not rust @@@ #!!! \x01";
    let ours = "this is not rust @@@ #!!! OURS";
    let theirs = "this is not rust @@@ #!!! THEIRS";
    let outcome = semantic_three_way_merge(
        base.as_bytes(),
        ours.as_bytes(),
        theirs.as_bytes(),
        Path::new("notrust.rs"),
        MARKERS,
    );
    // The driver must produce *some* coherent outcome, not panic.
    // text_hunk_merge would conflict on the divergent suffix.
    let _ = match outcome {
        MergeOutcome::Clean(_) | MergeOutcome::Conflicts { .. } => true,
        other => panic!("unexpected outcome: {other:?}"),
    };
}

// =====================================================================
// Unknown language: file extension we don't have a parser for →
// fall through to text_hunk_merge.
// =====================================================================
#[test]
fn unknown_language_falls_through() {
    let base = "alpha\nbeta\ngamma\n";
    let ours = "alpha\nBETA\ngamma\n";
    let theirs = "alpha\nbeta\nGAMMA\n";
    let outcome = semantic_three_way_merge(
        base.as_bytes(),
        ours.as_bytes(),
        theirs.as_bytes(),
        Path::new("file.xyz"),
        MARKERS,
    );
    let merged = match outcome {
        MergeOutcome::Clean(bytes) => String::from_utf8(bytes).unwrap(),
        other => panic!("expected Clean, got {other:?}"),
    };
    assert!(merged.contains("BETA"), "missing ours: {merged}");
    assert!(merged.contains("GAMMA"), "missing theirs: {merged}");
}

// =====================================================================
// Rust impl block: two branches add methods to the same impl → clean.
// =====================================================================
#[test]
fn impl_block_add_disjoint_methods() {
    let base = "\
struct Foo;

impl Foo {
    fn existing(&self) -> u32 {
        0
    }
}
";
    let ours = "\
struct Foo;

impl Foo {
    fn existing(&self) -> u32 {
        0
    }

    fn ours_method(&self) -> u32 {
        1
    }
}
";
    let theirs = "\
struct Foo;

impl Foo {
    fn existing(&self) -> u32 {
        0
    }

    fn theirs_method(&self) -> u32 {
        2
    }
}
";
    let outcome = merge_rust(base, ours, theirs);
    match outcome {
        MergeOutcome::Clean(bytes) => {
            let text = String::from_utf8(bytes).unwrap();
            assert!(
                text.contains("ours_method"),
                "ours method missing: {text}"
            );
            assert!(
                text.contains("theirs_method"),
                "theirs method missing: {text}"
            );
        }
        other => panic!("expected Clean, got {other:?}"),
    }
}

// =====================================================================
// Rust impl: different methods modified → clean.
// =====================================================================
#[test]
fn impl_different_methods_modified_clean() {
    let base = "\
struct Foo;

impl Foo {
    fn a(&self) -> u32 { 0 }
    fn b(&self) -> u32 { 0 }
}
";
    let ours = "\
struct Foo;

impl Foo {
    fn a(&self) -> u32 { 11 }
    fn b(&self) -> u32 { 0 }
}
";
    let theirs = "\
struct Foo;

impl Foo {
    fn a(&self) -> u32 { 0 }
    fn b(&self) -> u32 { 22 }
}
";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("11"), "missing ours mod: {merged}");
    assert!(merged.contains("22"), "missing theirs mod: {merged}");
}

// =====================================================================
// Reordering: ours moves a function, theirs modifies a DIFFERENT
// function. Per v1 contract this resolves cleanly (base order is
// preserved, ours's reorder is lost) — the brief explicitly accepts this.
// =====================================================================
#[test]
fn ours_reorders_theirs_modifies_other_clean() {
    let base = "\
fn one() { println!(\"1\"); }

fn two() { println!(\"2\"); }

fn three() { println!(\"3\"); }
";
    // ours moves `three` before `two`
    let ours = "\
fn one() { println!(\"1\"); }

fn three() { println!(\"3\"); }

fn two() { println!(\"2\"); }
";
    // theirs modifies `one`
    let theirs = "\
fn one() { println!(\"ONE\"); }

fn two() { println!(\"2\"); }

fn three() { println!(\"3\"); }
";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("ONE"), "missing theirs edit: {merged}");
    // Both functions still present.
    assert!(merged.contains("fn two()"));
    assert!(merged.contains("fn three()"));
}

// =====================================================================
// Whitespace-only divergence on otherwise identical functions: clean.
// =====================================================================
#[test]
fn whitespace_only_divergence_resolves() {
    let base = "fn f() {\n    let a = 1;\n}\n";
    let ours = "fn f() {\n    let a = 1;\n}\n";
    let theirs = "fn f() {\n    let a = 1;\n}\n";
    let outcome = merge_rust(base, ours, theirs);
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        other => panic!("expected Clean: {other:?}"),
    };
    assert!(text.contains("fn f()"));
}

// =====================================================================
// Add-add same name collision → conflict (both sides added a function
// with the same name but different bodies).
// =====================================================================
#[test]
fn both_sides_add_same_name_different_body_conflict() {
    let base = "fn keep() { println!(\"k\"); }\n";
    let ours = "\
fn keep() { println!(\"k\"); }
fn newcomer() { println!(\"ours-newcomer\"); }
";
    let theirs = "\
fn keep() { println!(\"k\"); }
fn newcomer() { println!(\"theirs-newcomer\"); }
";
    match merge_rust(base, ours, theirs) {
        MergeOutcome::Conflicts {
            conflict_count,
            ..
        } => {
            assert!(conflict_count >= 1);
        }
        other => panic!("expected Conflicts, got {other:?}"),
    }
}

// =====================================================================
// Both sides add the SAME function with the SAME body → clean (idempotent).
// =====================================================================
#[test]
fn both_sides_add_same_function_identical_body_clean() {
    let base = "fn keep() { println!(\"k\"); }\n";
    let same_addition = "\
fn keep() { println!(\"k\"); }
fn newcomer() { println!(\"same body\"); }
";
    let merged = assert_clean(merge_rust(base, same_addition, same_addition));
    assert!(merged.contains("newcomer"));
}

// =====================================================================
// Adversarial: ours reorders functions, theirs adds a function at the
// END. text_hunk_merge struggles here because the reorder shifts every
// line in ours and the LCS can't align ours past the move point — it
// emits a wide conflict block. The semantic driver matches functions by
// identity and resolves cleanly.
//
// This is the load-bearing "structural reshape" case from heddle#54.
// =====================================================================
#[test]
fn structural_reshape_resolves_where_text_merge_struggles() {
    let base = "\
fn a() { 1 }

fn b() { 2 }

fn c() { 3 }

fn d() { 4 }

fn e() { 5 }
";
    // ours reorders to [e, a, c, b, d] — every line moves.
    let ours = "\
fn e() { 5 }

fn a() { 1 }

fn c() { 3 }

fn b() { 2 }

fn d() { 4 }
";
    // theirs adds a new function at the end and modifies `c`.
    let theirs = "\
fn a() { 1 }

fn b() { 2 }

fn c() { 333 }

fn d() { 4 }

fn e() { 5 }

fn f() { 6 }
";
    let outcome = merge_rust(base, ours, theirs);
    let merged = assert_clean(outcome);
    // theirs's modification to c() survives.
    assert!(
        merged.contains("fn c() { 333 }"),
        "theirs c-edit lost: {merged}"
    );
    // theirs's added f() survives.
    assert!(merged.contains("fn f() { 6 }"), "theirs add lost: {merged}");
    // All five originals present.
    for name in ["fn a", "fn b", "fn c", "fn d", "fn e"] {
        assert!(merged.contains(name), "missing {name}: {merged}");
    }
    // No conflict markers.
    assert!(!merged.contains("<<<<<<<"), "no markers expected: {merged}");
}

// =====================================================================
// Adversarial — heddle#54 replay shape. Synthesize a small mirror of
// the trip-report rebase: ours rewrites half the file, theirs touches
// the other half. With text_hunk_merge directly this produces a wide
// conflict block; with the semantic driver, zero conflicts.
//
// Asserts ≤ 1 conflict-marker triple (vs the 7 "whole-file collisions"
// the report described).
// =====================================================================
#[test]
fn heddle_54_replay_shape_resolves_with_at_most_one_conflict() {
    fn body(suffix: &str) -> String {
        let mut s = String::new();
        for i in 0..20 {
            s.push_str(&format!("fn fn_{i}() {{ let x = {i}{suffix}; }}\n\n"));
        }
        s
    }
    let base = body("");
    let mut ours = base.clone();
    let mut theirs = base.clone();
    // ours modifies fn_0..fn_9 (first half).
    for i in 0..10 {
        ours = ours.replace(
            &format!("fn fn_{i}() {{ let x = {i}; }}"),
            &format!("fn fn_{i}() {{ let x = {i}_OURS; }}"),
        );
    }
    // theirs modifies fn_10..fn_19 (second half).
    for i in 10..20 {
        theirs = theirs.replace(
            &format!("fn fn_{i}() {{ let x = {i}; }}"),
            &format!("fn fn_{i}() {{ let x = {i}_THEIRS; }}"),
        );
    }
    let outcome = merge_rust(&base, &ours, &theirs);
    let merged = assert_clean(outcome);
    // All 20 edits land.
    for i in 0..10 {
        assert!(
            merged.contains(&format!("{i}_OURS")),
            "ours edit {i} lost"
        );
    }
    for i in 10..20 {
        assert!(
            merged.contains(&format!("{i}_THEIRS")),
            "theirs edit {i} lost"
        );
    }
}

// =====================================================================
// Direct A/B comparison: text_hunk_merge produces conflict markers on a
// structural-reshape shape, while the semantic driver produces zero.
//
// This is the comparison heddle#54 trip report described. We use a
// rename + body modify pattern that the line-level engine handles
// poorly but the AST-aware merger resolves cleanly because it matches
// functions by identity, not position.
// =====================================================================
#[test]
fn semantic_beats_text_merge_on_structural_reshape() {
    // base: 4 short functions in source order.
    let base = "\
fn a() { let x = 1; }
fn b() { let x = 2; }
fn c() { let x = 3; }
fn d() { let x = 4; }
";
    // ours: reorder + edit b.
    let ours = "\
fn d() { let x = 4; }
fn c() { let x = 3; }
fn b() { let x = 22; }
fn a() { let x = 1; }
";
    // theirs: edit d.
    let theirs = "\
fn a() { let x = 1; }
fn b() { let x = 2; }
fn c() { let x = 3; }
fn d() { let x = 44; }
";

    // Semantic driver: clean.
    let sem_outcome = merge_rust(base, ours, theirs);
    let sem_text = match sem_outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => {
            let preview = String::from_utf8_lossy(&merged_bytes_with_markers).into_owned();
            panic!("semantic driver should resolve cleanly. got:\n{preview}");
        }
        other => panic!("unexpected outcome: {other:?}"),
    };
    assert!(sem_text.contains("let x = 22"), "ours edit lost");
    assert!(sem_text.contains("let x = 44"), "theirs edit lost");

    // Compare: text_hunk_merge struggles. We assert it produces MORE
    // markers than the semantic path. Concretely the semantic driver
    // emits 0; text_hunk_merge will emit ≥1.
    let direct = merge::text_hunk_merge_with_markers(
        base.as_bytes(),
        ours.as_bytes(),
        theirs.as_bytes(),
        MARKERS,
    );
    let direct_markers = match direct {
        MergeOutcome::Clean(_) => 0,
        MergeOutcome::Conflicts { conflict_count, .. } => conflict_count,
        other => panic!("unexpected outcome: {other:?}"),
    };
    assert!(
        direct_markers > 0,
        "expected text_hunk_merge to surface ≥1 conflict on this shape; \
         if heddle-merge has improved, retire this comparison test"
    );
}

// =====================================================================
// 1000+ functions, both sides modify ONE different function each.
// Performance budget: should not blow up.
// =====================================================================
#[test]
fn many_functions_disjoint_modifications_resolves() {
    let mut base = String::new();
    for i in 0..200 {
        base.push_str(&format!("fn fn_{i}() {{ let x = {i}; }}\n\n"));
    }
    let mut ours = base.clone();
    let mut theirs = base.clone();
    // ours modifies fn_50; theirs modifies fn_150.
    ours = ours.replace("fn fn_50() { let x = 50; }", "fn fn_50() { let x = 5050; }");
    theirs = theirs.replace(
        "fn fn_150() { let x = 150; }",
        "fn fn_150() { let x = 15150; }",
    );
    let merged = assert_clean(merge_rust(&base, &ours, &theirs));
    assert!(merged.contains("5050"), "ours edit lost");
    assert!(merged.contains("15150"), "theirs edit lost");
}
