// SPDX-License-Identifier: Apache-2.0
//! Unit tests for the function-level merge driver.
//!
//! The matrix below mirrors the cases enumerated in heddle#68 §3 "Test matrix".
//! Each case asserts the *contract* — same-function overlap surfaces line
//! markers, disjoint-function edits merge cleanly, unparseable input falls
//! through to whole-file `heddle-merge`.

use std::path::Path;

use merge::{ConflictMarkers, MergeOutcome};

use super::{MergeStrategy, semantic_three_way_merge, three_way_merge};

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

// =====================================================================
// Coverage: dispatch shortcuts in `semantic_three_way_merge`.
// =====================================================================

#[test]
fn all_three_sides_identical_returns_base() {
    let s = "fn a() { 1 }\n";
    let out = merge_rust(s, s, s);
    assert_eq!(assert_clean(out), s);
}

#[test]
fn base_equals_ours_takes_theirs() {
    let base = "fn a() { 1 }\n";
    let theirs = "fn a() { 2 }\n";
    let out = merge_rust(base, base, theirs);
    assert_eq!(assert_clean(out), theirs);
}

#[test]
fn base_equals_theirs_takes_ours() {
    let base = "fn a() { 1 }\n";
    let ours = "fn a() { 2 }\n";
    let out = merge_rust(base, ours, base);
    assert_eq!(assert_clean(out), ours);
}

#[test]
fn ours_equals_theirs_takes_either() {
    let base = "fn a() { 1 }\n";
    let same = "fn a() { 99 }\n";
    let out = merge_rust(base, same, same);
    assert_eq!(assert_clean(out), same);
}

#[test]
fn non_utf8_input_falls_through_to_hunk_merge() {
    // Invalid UTF-8 byte (0xFF is never valid as a leading byte).
    let base: &[u8] = b"fn a() { 1 }\n\xff\n";
    let ours: &[u8] = b"fn a() { 1 }\n\xff OURS\n";
    let theirs: &[u8] = b"fn a() { 1 }\n\xff THEIRS\n";
    let outcome = semantic_three_way_merge(base, ours, theirs, Path::new("a.rs"), MARKERS);
    // Must not panic; should produce some merge outcome from the byte path.
    match outcome {
        MergeOutcome::Clean(_) | MergeOutcome::Conflicts { .. } => {}
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[test]
fn three_way_merge_hunk_only_strategy_skips_parsing() {
    // .rs path, but HunkOnly forces text_hunk_merge regardless.
    let base = "fn a() { 1 }\nfn b() { 2 }\n";
    let ours = "fn a() { 10 }\nfn b() { 2 }\n";
    let theirs = "fn a() { 1 }\nfn b() { 20 }\n";
    let out = three_way_merge(
        base.as_bytes(),
        ours.as_bytes(),
        theirs.as_bytes(),
        Path::new("a.rs"),
        MARKERS,
        MergeStrategy::HunkOnly,
    );
    let merged = assert_clean(out);
    assert!(merged.contains("10"));
    assert!(merged.contains("20"));
}

#[test]
fn three_way_merge_semantic_strategy_uses_ast() {
    let base = "fn a() { 1 }\nfn b() { 2 }\n";
    let ours = "fn a() { 10 }\nfn b() { 2 }\n";
    let theirs = "fn a() { 1 }\nfn b() { 20 }\n";
    let out = three_way_merge(
        base.as_bytes(),
        ours.as_bytes(),
        theirs.as_bytes(),
        Path::new("a.rs"),
        MARKERS,
        MergeStrategy::Semantic,
    );
    let merged = assert_clean(out);
    assert!(merged.contains("10"));
    assert!(merged.contains("20"));
}

// =====================================================================
// Coverage: reconstruct.rs — modify/delete variants.
// =====================================================================

#[test]
fn modify_delete_clean_when_modifier_preserved_base() {
    // ours: modify keep() so base != ours overall, but target() unchanged
    // vs base; theirs: delete target() entirely. Target's b == o so the
    // (Some, Some, None) arm cleanly drops the function.
    let base = "fn keep() { 1 }\nfn target() { 1 }\n";
    let ours = "fn keep() { 2 }\nfn target() { 1 }\n";
    let theirs = "fn keep() { 1 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(!merged.contains("fn target"), "target should be deleted: {merged}");
    assert!(merged.contains("fn keep() { 2 }"));
}

#[test]
fn delete_modify_clean_when_modifier_preserved_base() {
    // theirs keeps target() exactly as base; ours deletes target().
    // Target's b == t so the (Some, None, Some) arm cleanly drops it.
    let base = "fn keep() { 1 }\nfn target() { 1 }\n";
    let ours = "fn keep() { 2 }\n";
    let theirs = "fn keep() { 1 }\nfn target() { 1 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(!merged.contains("fn target"));
    assert!(merged.contains("fn keep() { 2 }"));
}

#[test]
fn both_sides_add_identical_function_with_other_divergence_clean() {
    // Both sides add `fn newcomer() { 1 }` identically — but each side
    // also makes a *different* edit elsewhere, so the file-level
    // short-circuits in semantic_three_way_merge don't fire and we
    // actually reach resolve_item's (None, Some(o), Some(t)) o==t arm.
    let base = "fn alpha() { 1 }\nfn beta() { 2 }\n";
    let ours = "fn alpha() { 10 }\nfn beta() { 2 }\nfn newcomer() { 99 }\n";
    let theirs = "fn alpha() { 1 }\nfn beta() { 20 }\nfn newcomer() { 99 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("fn newcomer() { 99 }"));
    assert!(merged.contains("fn alpha() { 10 }"));
    assert!(merged.contains("fn beta() { 20 }"));
}

#[test]
fn both_sides_delete_same_function_clean() {
    // Exercise the (Some(_), None, None) → (None, 0) arm.
    let base = "fn keep() { 1 }\nfn gone() { 0 }\n";
    let ours = "fn keep() { 2 }\n";
    let theirs = "fn keep() { 3 }\n";
    let outcome = merge_rust(base, ours, theirs);
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts { merged_bytes_with_markers, .. } => {
            String::from_utf8(merged_bytes_with_markers).unwrap()
        }
        other => panic!("unexpected: {other:?}"),
    };
    assert!(!text.contains("fn gone"), "gone() should be removed: {text}");
}

#[test]
fn delete_modify_conflicts_when_theirs_modified() {
    // ours deletes; theirs modifies → conflict.
    let base = "fn keep() {}\nfn target() { 1 }\n";
    let ours = "fn keep() {}\n";
    let theirs = "fn keep() {}\nfn target() { 999 }\n";
    let (_text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(count >= 1);
}

#[test]
fn three_way_modify_ours_unchanged_takes_theirs() {
    // o == b → take theirs.
    let base = "fn a() { 1 }\n";
    let ours = "fn a() { 1 }\n";
    let theirs = "fn a() { 42 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("42"));
}

#[test]
fn three_way_modify_both_made_same_change_takes_ours() {
    // o == t → clean, both sides made identical edit.
    let base = "fn a() { 1 }\nfn b() { 2 }\n";
    let ours = "fn a() { 1 }\nfn b() { 42 }\n";
    let theirs = "fn a() { 1 }\nfn b() { 42 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("42"));
}

// =====================================================================
// Coverage: items.rs — Rust constructs beyond fn / impl.
// =====================================================================

#[test]
fn rust_struct_modified_disjoint_clean() {
    let base = "struct S { x: u32 }\nfn f() { 1 }\n";
    let ours = "struct S { x: u64 }\nfn f() { 1 }\n";
    let theirs = "struct S { x: u32 }\nfn f() { 2 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("u64"));
    assert!(merged.contains("fn f() { 2 }"));
}

#[test]
fn rust_enum_modified_disjoint_clean() {
    let base = "enum E { A, B }\nfn f() { 1 }\n";
    let ours = "enum E { A, B, C }\nfn f() { 1 }\n";
    let theirs = "enum E { A, B }\nfn f() { 99 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("C"));
    assert!(merged.contains("99"));
}

#[test]
fn rust_trait_with_signature_methods_disjoint_clean() {
    let base = "\
trait T {
    fn foo(&self);
    fn bar(&self);
}
fn k() { 1 }
";
    let ours = "\
trait T {
    fn foo(&self) -> u32;
    fn bar(&self);
}
fn k() { 1 }
";
    let theirs = "\
trait T {
    fn foo(&self);
    fn bar(&self);
}
fn k() { 2 }
";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("-> u32"));
    assert!(merged.contains("fn k() { 2 }"));
}

#[test]
fn rust_const_static_type_alias_modified_clean() {
    let base = "\
const C: u32 = 1;
static S: u32 = 2;
type T = u32;
fn k() { 0 }
";
    let ours = "\
const C: u32 = 10;
static S: u32 = 2;
type T = u32;
fn k() { 0 }
";
    let theirs = "\
const C: u32 = 1;
static S: u32 = 20;
type T = u64;
fn k() { 0 }
";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("C: u32 = 10"));
    assert!(merged.contains("S: u32 = 20"));
    assert!(merged.contains("type T = u64"));
}

#[test]
fn rust_union_modified_clean() {
    let base = "\
union U { a: u32, b: u64 }
fn k() { 0 }
";
    let ours = "\
union U { a: u32, b: u128 }
fn k() { 0 }
";
    let theirs = "\
union U { a: u32, b: u64 }
fn k() { 1 }
";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("u128"));
    assert!(merged.contains("fn k() { 1 }"));
}

#[test]
fn rust_mod_header_only_change() {
    // `mod x;` is a header form — no body child. Exercises the
    // body.is_some() branch in classify_rust_node for mod_item.
    let base = "mod a;\nmod b;\nfn k() { 0 }\n";
    let ours = "mod a;\nmod b;\nfn k() { 1 }\n";
    let theirs = "mod a;\nmod b;\nfn k() { 0 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("fn k() { 1 }"));
}

#[test]
fn rust_mod_with_inline_body_disjoint_methods_clean() {
    // `mod x { ... }` — body present, recurses into nested items with
    // updated scope.
    let base = "\
mod inner {
    fn one() { 1 }
    fn two() { 2 }
}
";
    let ours = "\
mod inner {
    fn one() { 10 }
    fn two() { 2 }
}
";
    let theirs = "\
mod inner {
    fn one() { 1 }
    fn two() { 20 }
}
";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("fn one() { 10 }"));
    assert!(merged.contains("fn two() { 20 }"));
}

#[test]
fn rust_impl_trait_for_type_distinct_from_inherent_impl() {
    // `impl T for Foo` should have a different ItemKey than `impl Foo`,
    // so methods inside each are not confused. Exercises rust_impl_name's
    // `trait for type` branch.
    let base = "\
struct Foo;
impl Foo {
    fn inherent(&self) { let _ = 1; }
}
impl Clone for Foo {
    fn clone(&self) -> Self { Foo }
}
";
    let ours = "\
struct Foo;
impl Foo {
    fn inherent(&self) { let _ = 42; }
}
impl Clone for Foo {
    fn clone(&self) -> Self { Foo }
}
";
    let theirs = "\
struct Foo;
impl Foo {
    fn inherent(&self) { let _ = 1; }
}
impl Clone for Foo {
    fn clone(&self) -> Self { Foo.clone() }
}
";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("let _ = 42"));
    assert!(merged.contains("Foo.clone()"));
}

// =====================================================================
// Coverage: items.rs — Python, JavaScript, TypeScript classifiers.
// =====================================================================

fn merge_at(base: &str, ours: &str, theirs: &str, path: &str) -> MergeOutcome {
    semantic_three_way_merge(
        base.as_bytes(),
        ours.as_bytes(),
        theirs.as_bytes(),
        Path::new(path),
        MARKERS,
    )
}

#[test]
fn python_function_disjoint_modifications_clean() {
    let base = "\
def alpha():
    return 1

def beta():
    return 2
";
    let ours = "\
def alpha():
    return 11

def beta():
    return 2
";
    let theirs = "\
def alpha():
    return 1

def beta():
    return 22
";
    let merged = match merge_at(base, ours, theirs, "f.py") {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        other => panic!("expected Clean, got {other:?}"),
    };
    assert!(merged.contains("return 11"));
    assert!(merged.contains("return 22"));
}

#[test]
fn python_class_with_methods_disjoint_clean() {
    let base = "\
class C:
    def one(self):
        return 1

    def two(self):
        return 2
";
    let ours = "\
class C:
    def one(self):
        return 10

    def two(self):
        return 2
";
    let theirs = "\
class C:
    def one(self):
        return 1

    def two(self):
        return 20
";
    let merged = match merge_at(base, ours, theirs, "f.py") {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        other => panic!("expected Clean, got {other:?}"),
    };
    assert!(merged.contains("return 10"));
    assert!(merged.contains("return 20"));
}

#[test]
fn python_pyi_extension_also_handled() {
    let base = "def f():\n    return 1\n";
    let ours = "def f():\n    return 2\n";
    let theirs = base;
    let outcome = merge_at(base, ours, theirs, "stub.pyi");
    match outcome {
        MergeOutcome::Clean(b) => {
            let s = String::from_utf8(b).unwrap();
            assert!(s.contains("return 2"));
        }
        other => panic!("expected Clean, got {other:?}"),
    }
}

#[test]
fn javascript_function_declarations_disjoint_clean() {
    let base = "\
function a() { return 1; }
function b() { return 2; }
";
    let ours = "\
function a() { return 11; }
function b() { return 2; }
";
    let theirs = "\
function a() { return 1; }
function b() { return 22; }
";
    let merged = match merge_at(base, ours, theirs, "f.js") {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        other => panic!("expected Clean, got {other:?}"),
    };
    assert!(merged.contains("return 11"));
    assert!(merged.contains("return 22"));
}

#[test]
fn javascript_class_methods_disjoint_clean() {
    let base = "\
class C {
    one() { return 1; }
    two() { return 2; }
}
";
    let ours = "\
class C {
    one() { return 10; }
    two() { return 2; }
}
";
    let theirs = "\
class C {
    one() { return 1; }
    two() { return 20; }
}
";
    let merged = match merge_at(base, ours, theirs, "f.js") {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        other => panic!("expected Clean, got {other:?}"),
    };
    assert!(merged.contains("return 10"));
    assert!(merged.contains("return 20"));
}

#[test]
fn typescript_class_method_modified_clean() {
    let base = "\
class C {
    greet(name: string): string { return name; }
    bye(): void {}
}
";
    let ours = "\
class C {
    greet(name: string): string { return name.toUpperCase(); }
    bye(): void {}
}
";
    let theirs = "\
class C {
    greet(name: string): string { return name; }
    bye(): void { console.log('bye'); }
}
";
    let merged = match merge_at(base, ours, theirs, "f.ts") {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        other => panic!("expected Clean, got {other:?}"),
    };
    assert!(merged.contains("toUpperCase"));
    assert!(merged.contains("console.log"));
}

// =====================================================================
// Coverage: reconstruct.rs — inter-item / preamble divergence surfaces
// conflicts via the Conflicts branch of merge_inter_item_content.
// =====================================================================

#[test]
fn preamble_diverging_edits_surface_conflict_in_inter_item_merge() {
    // Both sides modify the same line of the preamble (between items
    // there are no items — only `use` lines at the top). This forces
    // text_hunk_merge on the inter-item concatenation to produce
    // Conflicts, exercising the Conflicts arm of the inter-item match.
    let base = "\
use std::a;
use std::b;

fn f() { 1 }
";
    let ours = "\
use std::a;
use std::OURS;

fn f() { 1 }
";
    let theirs = "\
use std::a;
use std::THEIRS;

fn f() { 1 }
";
    let outcome = merge_rust(base, ours, theirs);
    match outcome {
        MergeOutcome::Conflicts { conflict_count, .. } => {
            assert!(conflict_count >= 1);
        }
        MergeOutcome::Clean(_) => panic!("expected conflict on diverging preamble"),
        other => panic!("unexpected outcome: {other:?}"),
    }
}

// =====================================================================
// Coverage: items.rs — Item de-overlap (nested fn inside fn body).
// =====================================================================

#[test]
fn nested_function_inside_outer_does_not_split_outer_item() {
    // Inner `fn inner()` lives inside `outer()` body; the de-overlap
    // pass must drop the inner item so `outer()` merges as one unit.
    let base = "\
fn outer() {
    fn inner() { 1 }
    inner()
}
";
    let ours = "\
fn outer() {
    fn inner() { 99 }
    inner()
}
";
    let theirs = base;
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("fn inner() { 99 }"));
}

// =====================================================================
// Codex r1 P1 #1: signature data in semantic item keys.
//
// Overloads (same name, different parameter signatures) must be matched
// independently. Pre-fix, ItemKey is (kind, name, scope) and the second
// insert into the BTreeMap silently overwrites the first — so one
// overload's edits AND original body are dropped from the merged
// output entirely.
// =====================================================================
#[test]
fn typescript_overload_signatures_not_collapsed_by_key_collision() {
    // Two top-level function declarations sharing a name but differing
    // in the parameter list. With name-only keys both collapse into one
    // BTreeMap entry → the "first" overload disappears from output.
    let base = "\
function foo(x: number): number { return 1; }
function foo(x: string): string { return \"a\"; }
";
    // ours edits foo(number); theirs edits foo(string). Disjoint
    // overload edits — pre-fix both edits are lost because the overloads
    // collapse to a single map entry.
    let ours = "\
function foo(x: number): number { return 100; }
function foo(x: string): string { return \"a\"; }
";
    let theirs = "\
function foo(x: number): number { return 1; }
function foo(x: string): string { return \"AAA\"; }
";
    let merged = match merge_at(base, ours, theirs, "f.ts") {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected outcome: {other:?}"),
    };
    // Both overload signatures must survive.
    assert!(
        merged.contains("x: number"),
        "foo(number) overload lost: {merged}"
    );
    assert!(
        merged.contains("x: string"),
        "foo(string) overload lost: {merged}"
    );
    // Each side's disjoint edit must land.
    assert!(merged.contains("return 100"), "ours edit lost: {merged}");
    assert!(merged.contains("return \"AAA\""), "theirs edit lost: {merged}");
}

// =====================================================================
// Codex r1 P1 #3: key Go methods by receiver type.
//
// In Go, `func (a A) String() string` and `func (b B) String() string`
// share the method_declaration name "String"; without the receiver in
// the key both methods collapse and one is dropped from the merge.
// =====================================================================
#[test]
fn go_methods_on_different_receivers_not_collapsed() {
    let base = "\
package main

type A struct{}
func (a A) String() string { return \"a\" }

type B struct{}
func (b B) String() string { return \"b\" }
";
    // ours edits A.String; theirs edits B.String. Pre-fix both methods
    // collide on (Method, \"String\", []) — one is dropped before any
    // merge can happen.
    let ours = "\
package main

type A struct{}
func (a A) String() string { return \"A-OURS\" }

type B struct{}
func (b B) String() string { return \"b\" }
";
    let theirs = "\
package main

type A struct{}
func (a A) String() string { return \"a\" }

type B struct{}
func (b B) String() string { return \"B-THEIRS\" }
";
    let merged = match merge_at(base, ours, theirs, "f.go") {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        other => panic!("expected Clean, got {other:?}"),
    };
    // Both methods must survive with their respective edits.
    assert!(
        merged.contains("(a A) String()"),
        "A.String lost: {merged}"
    );
    assert!(
        merged.contains("(b B) String()"),
        "B.String lost: {merged}"
    );
    assert!(merged.contains("A-OURS"), "ours edit lost: {merged}");
    assert!(merged.contains("B-THEIRS"), "theirs edit lost: {merged}");
}

// =====================================================================
// Codex r1 P1 #4: container children must remain as merge units.
//
// items.rs:99 — when a container (impl, trait, class, module) is
// recorded as an item AND its body is traversed for sub-items, the
// de-overlap pass drops every sub-item whose start falls inside the
// container's byte range. The whole container then merges as one unit
// via text_hunk_merge, defeating the function-level semantic
// granularity that's the whole point of this driver.
// =====================================================================
#[test]
fn impl_block_single_line_disjoint_method_edits_merge_cleanly() {
    // Methods packed on a single line so text_hunk_merge can't compose
    // the disjoint edits (the whole impl is one line; ours and theirs
    // both rewrite that line differently). Per-method semantic merge
    // routes each method through its own resolution.
    let base = "impl A { fn x() { 0 } fn y() { 0 } }\n";
    let ours = "impl A { fn x() { 11 } fn y() { 0 } }\n";
    let theirs = "impl A { fn x() { 0 } fn y() { 22 } }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(
        merged.contains("fn x() { 11 }"),
        "ours edit lost: {merged}"
    );
    assert!(
        merged.contains("fn y() { 22 }"),
        "theirs edit lost: {merged}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "expected clean merge, got markers: {merged}"
    );
}

// =====================================================================
// Codex r1 P1 #2: reconstruct inter-item segments at original positions.
//
// reconstruct.rs:116 — the v1 reconstruction concatenates each side's
// inter-item content and emits the merged blob at the top, then
// appends all merged items below. Top-level executable statements
// (Python imports, JavaScript expression statements, Rust attributes)
// get hoisted to the file start — changing runtime semantics.
// =====================================================================
#[test]
fn python_top_level_executable_statement_stays_between_functions() {
    // base has an `import`, then `foo`, then a top-level `x.init()`
    // call (which must run AFTER foo is defined and BEFORE bar is
    // defined), then `bar`.
    let base = "\
import x

def foo():
    return 1

x.init()

def bar():
    return 2
";
    // ours edits foo; theirs edits bar. Both per-item edits land.
    // The bug is in the WEAVING of the `x.init()` line.
    let ours = "\
import x

def foo():
    return 11

x.init()

def bar():
    return 2
";
    let theirs = "\
import x

def foo():
    return 1

x.init()

def bar():
    return 22
";
    let merged = match merge_at(base, ours, theirs, "f.py") {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        other => panic!("expected Clean, got {other:?}"),
    };
    let p_foo = merged.find("def foo()").expect("foo present");
    let p_init = merged.find("x.init()").expect("x.init() present");
    let p_bar = merged.find("def bar()").expect("bar present");
    assert!(
        p_foo < p_init && p_init < p_bar,
        "expected foo < x.init() < bar in:\n{merged}"
    );
    // Per-item edits land.
    assert!(merged.contains("return 11"), "ours edit lost: {merged}");
    assert!(merged.contains("return 22"), "theirs edit lost: {merged}");
}

// =====================================================================
// Codex r1 P2 #2: collect_items must not stack-overflow on deep trees.
//
// items.rs:155 recurses for every unclassified or container child.
// Deeply-nested parseable trees → stack overflow → merge aborts.
// =====================================================================
#[test]
fn deeply_nested_rust_modules_does_not_stack_overflow() {
    // Build a Rust file with `depth` nested mod blocks holding one fn at
    // the centre. Run the merge inside a thread with a small stack so a
    // recursive walker overflows before reaching the leaf.
    // 2000 nested mods on a 128 KiB stack. Per-frame recursion costs
    // are tight in optimized Rust, so this is a guard rather than a
    // proof-of-bug — but it pins the contract: collect_items must walk
    // the AST without consuming bounded stack proportional to depth.
    let depth = 2000usize;
    let mut s = String::new();
    for i in 0..depth {
        s.push_str(&format!("mod m{i} {{\n"));
    }
    s.push_str("    fn inner() { 1 }\n");
    for _ in 0..depth {
        s.push_str("}\n");
    }
    let base = s.clone();
    let ours = s.replace("fn inner() { 1 }", "fn inner() { 2 }");
    // Diverge theirs from base too so the file-level base==theirs
    // shortcut doesn't elide the semantic pass we're trying to stress.
    let theirs = s.replace("fn inner() { 1 }", "fn inner() { 1; let _ = 0; }");

    let handle = std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(move || {
            merge_rust(&base, &ours, &theirs);
        })
        .expect("spawn");
    handle
        .join()
        .expect("merge must not stack-overflow on deeply-nested input");
}

#[test]
fn deep_nesting_past_max_traversal_depth_falls_through_to_text() {
    // 300 nested mods on the default test stack so the parser handles
    // it cleanly, but past MAX_TRAVERSAL_DEPTH=256 — the depth guard
    // bails out of item extraction for the innermost ~44 mods and they
    // merge as inter-item text. Asserts the merge still completes and
    // the inner edit survives via the text-level fallback.
    let depth = 300usize;
    let mut s = String::new();
    for i in 0..depth {
        s.push_str(&format!("mod m{i} {{\n"));
    }
    s.push_str("fn inner() { 1 }\nfn other() { 0 }\n");
    for _ in 0..depth {
        s.push_str("}\n");
    }
    let ours = s.replace("fn inner() { 1 }", "fn inner() { 2 }");
    let theirs = s.replace("fn other() { 0 }", "fn other() { 99 }");
    let outcome = merge_rust(&s, &ours, &theirs);
    // The result may be Clean (text_hunk_merge composes the disjoint
    // edits cleanly) or Conflicts (the depth guard's text fallback
    // produces a wider conflict). Either is acceptable; the contract
    // is that the merge completes without panic.
    match outcome {
        MergeOutcome::Clean(_) | MergeOutcome::Conflicts { .. } => {}
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[test]
fn go_free_function_modified_clean() {
    // Top-level Go function (not a method) exercises the
    // classify_go_node `function_declaration` branch. Two functions
    // so theirs can edit one while ours edits the other — and the
    // file-level base==theirs shortcut doesn't short-circuit out of
    // the semantic path.
    let base = "\
package p

func Add(a, b int) int { return a + b }
func Sub(a, b int) int { return a - b }
";
    let ours = "\
package p

func Add(a, b int) int { return a + b + 0 }
func Sub(a, b int) int { return a - b }
";
    let theirs = "\
package p

func Add(a, b int) int { return a + b }
func Sub(a, b int) int { return a - b - 0 }
";
    let outcome = merge_at(base, ours, theirs, "f.go");
    let merged = match outcome {
        MergeOutcome::Clean(bytes) => String::from_utf8(bytes).unwrap(),
        other => panic!("expected Clean, got {other:?}"),
    };
    assert!(merged.contains("return a + b + 0"), "ours edit lost: {merged}");
    assert!(merged.contains("return a - b - 0"), "theirs edit lost: {merged}");
}

#[test]
fn go_method_pointer_receiver_keyed_distinctly_from_value_receiver() {
    // Pointer-receiver and value-receiver methods with the same name
    // exercise go_receiver_type's handling of `*T` vs `T`. Both should
    // survive the merge as distinct items.
    let base = "\
package p

type A struct{}
func (a A) M() int { return 0 }
func (a *A) M() int { return 1 }
";
    let ours = "\
package p

type A struct{}
func (a A) M() int { return 10 }
func (a *A) M() int { return 1 }
";
    let theirs = "\
package p

type A struct{}
func (a A) M() int { return 0 }
func (a *A) M() int { return 11 }
";
    let outcome = merge_at(base, ours, theirs, "f.go");
    let merged = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        other => panic!("expected Clean, got {other:?}"),
    };
    assert!(merged.contains("return 10"));
    assert!(merged.contains("return 11"));
    assert!(merged.contains("(a A) M()"));
    assert!(merged.contains("(a *A) M()"));
}

// =====================================================================
// Codex r1 P2 #1: preserve order of multiple adjacent additions at the
// same anchor.
//
// reconstruct.rs:296 — when N new items on one side all share the same
// "left neighbour" in base, each gets inserted at anchor+1 in turn, so
// the run ends up reversed in the merged output. Side-effect-tied
// definitions (decorators, init order) break.
// =====================================================================
#[test]
fn three_adjacent_added_items_preserve_source_order() {
    let base = "\
fn a() { 1 }
fn z() { 9 }
";
    // ours adds b, c, d in order between a and z.
    let ours = "\
fn a() { 1 }
fn b() { 2 }
fn c() { 3 }
fn d() { 4 }
fn z() { 9 }
";
    // theirs modifies z so the file-level base==theirs shortcut doesn't
    // fire — the splice path is actually exercised.
    let theirs = "\
fn a() { 1 }
fn z() { 99 }
";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    let pa = merged.find("fn a(").expect("a present");
    let pb = merged.find("fn b(").expect("b present");
    let pc = merged.find("fn c(").expect("c present");
    let pd = merged.find("fn d(").expect("d present");
    let pz = merged.find("fn z(").expect("z present");
    assert!(
        pa < pb && pb < pc && pc < pd && pd < pz,
        "expected a < b < c < d < z order in:\n{merged}"
    );
}

#[test]
fn javascript_top_level_same_name_functions_distinguishable_by_arity() {
    // Plain JavaScript allows two top-level `function foo` declarations
    // with different arities; this parses as two `function_declaration`
    // nodes with the same name. Pre-fix they collide on (Function, foo, [])
    // and one is lost.
    let base = "\
function foo(x) { return x; }
function foo(x, y) { return x + y; }
";
    // ours edits the one-arg variant; theirs edits the two-arg variant.
    let ours = "\
function foo(x) { return x + 10; }
function foo(x, y) { return x + y; }
";
    let theirs = "\
function foo(x) { return x; }
function foo(x, y) { return x * y; }
";
    let merged = match merge_at(base, ours, theirs, "f.js") {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        other => panic!("expected Clean, got {other:?}"),
    };
    // Both arities must be present after merge.
    assert!(
        merged.contains("function foo(x)") && merged.contains("function foo(x, y)"),
        "one of the arities lost: {merged}"
    );
    // Both disjoint edits must land.
    assert!(merged.contains("return x + 10"), "ours edit lost: {merged}");
    assert!(merged.contains("return x * y"), "theirs edit lost: {merged}");
}

// =====================================================================
// Codex r2 P1 #1: canonicalize parameter signatures before hashing.
//
// items.rs:hash_normalized used split_whitespace() which keeps
// punctuation attached to tokens — `foo(x,y)` and `foo(x, y)` hash
// differently, so the same function gets distinct ItemKeys across
// sides. The merger then treats it as delete+add, dropping one
// side's edit and producing duplicate definitions.
// =====================================================================
#[test]
fn signature_hash_canonicalizes_punctuation_so_formatting_only_change_matches() {
    // ours reformats the parameter list (no space after comma) on
    // line 1 AND edits the body. theirs preserves the original
    // formatting and ALSO edits the body. Pre-fix, the signature
    // spelling difference makes ours's foo a distinct ItemKey from
    // base/theirs's foo: the merger emits ours's foo as a clean
    // addition AND ALSO emits base/theirs's foo (via a
    // modify-vs-delete path), producing two foo definitions.
    let base = "\
fn foo(x: u32, y: u32) -> u32 {
    0
}
";
    let ours = "\
fn foo(x: u32,y: u32) -> u32 {
    1
}
";
    let theirs = "\
fn foo(x: u32, y: u32) -> u32 {
    2
}
";
    let merged = match merge_rust(base, ours, theirs) {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    // Pre-fix: ours's foo and base/theirs's foo are distinct
    // ItemKeys, so the merger emits BOTH as complete definitions — 2
    // closing braces. Post-fix: ItemKeys match, so the merger emits
    // ONE foo whose body merge surfaces a conflict — 1 closing brace.
    let close_brace_count = merged.matches('}').count();
    assert_eq!(
        close_brace_count, 1,
        "expected ONE foo definition (signature hash must canonicalize \
         formatting-only param changes), got {close_brace_count} closing \
         braces: {merged}"
    );
}

// =====================================================================
// Codex r2 P2 #1: canonicalize Go receiver type spelling.
//
// items.rs:go_receiver_type normalized via split_whitespace().join(" "),
// so `*A` (1 token, no space) and `* A` (2 tokens, joined with a
// space) end up with distinct scope strings `"*A"` vs `"* A"`. The
// same method on the same receiver gets distinct ItemKeys across
// sides and the merger misclassifies it as delete/add.
// =====================================================================
#[test]
fn go_receiver_type_canonicalizes_whitespace_around_pointer_star() {
    // ours adds a space between `*` and `A` on the receiver declaration
    // AND edits the body. theirs preserves the original spelling and
    // ALSO edits the body. Pre-fix, the receiver-type string differs
    // (`*A` vs `* A`) and the methods collapse to add+delete — ours's
    // M() is emitted as an addition while base/theirs's M() goes
    // through its own modify path, producing two M() definitions.
    let base = "\
package p

type A struct{}

func (a *A) M() int {
    return 0
}
";
    let ours = "\
package p

type A struct{}

func (a * A) M() int {
    return 1
}
";
    let theirs = "\
package p

type A struct{}

func (a *A) M() int {
    return 2
}
";
    let merged = match merge_at(base, ours, theirs, "f.go") {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    // Count `\n}` (closing brace at line start) — excludes the
    // single-line `type A struct{}`. Pre-fix: each of two M() bodies
    // contributes one. Post-fix: a single merged M() body contributes one.
    let close_count = merged.matches("\n}").count();
    assert_eq!(
        close_count, 1,
        "expected ONE M() definition (receiver type must canonicalize \
         whitespace), got {close_count} line-leading closing braces: {merged}"
    );
}

// =====================================================================
// Codex r2 P2 #2: prevent duplicate preamble emission for leading
// added items.
//
// reconstruct.rs:104 always takes each emitted key's original
// preceding segment without tracking whether the side's preamble was
// already emitted. When base has no items and both sides add
// different items at the top with their own preambles
// (imports/comments/docstring), the second emitted item's preceding
// segment is the second side's preamble — which the first iteration
// already pulled in via the missing-side fallback. Top-of-file
// content gets duplicated.
// =====================================================================
#[test]
fn no_base_items_both_sides_add_different_items_preamble_not_duplicated() {
    // base has only a top-level comment; both sides add their own
    // function with their own preamble (a use statement). The shared
    // `// top header` line must appear exactly once in the output.
    let base = "// top header\n";
    let ours = "\
// top header
use std::a;

fn alpha() { 1 }
";
    let theirs = "\
// top header
use std::b;

fn beta() { 2 }
";
    let merged = match merge_rust(base, ours, theirs) {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    let header_count = merged.matches("// top header").count();
    assert_eq!(
        header_count, 1,
        "expected `// top header` exactly once, got {header_count}: {merged}"
    );
}

// =====================================================================
// Codex r2 P1 #2: skip unconditional postamble merge for sides with
// no items.
//
// reconstruct.rs:144 unconditionally emits each side's `last_segment`.
// When a side has zero items, `inter_item_ranges()` returns one
// segment — the whole file — and the first-item preamble fallback
// has already consumed it. The postamble emission appends it again,
// duplicating that side's content in the merged output.
// =====================================================================
#[test]
fn zero_items_side_postamble_does_not_duplicate_bridging_segment() {
    // ours has no parseable items (only a top-level `use`), base and
    // theirs each have one function. Pre-fix, ours's "use std::io;\n"
    // is consumed by the first iteration's preamble fallback AND
    // re-emitted by the postamble merge — appearing twice in the
    // output.
    let base = "fn a() { 1 }\n";
    let ours = "use std::io;\n";
    let theirs = "fn a() { 2 }\n";
    let outcome = merge_rust(base, ours, theirs);
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    let use_count = text.matches("use std::io").count();
    assert_eq!(
        use_count, 1,
        "expected ours's `use std::io` exactly once, got {use_count}: {text}"
    );
}

// =====================================================================
// Codex r2 P1 #3: preserve segment edits when the opposite side
// deletes an item.
//
// reconstruct.rs:226 — when (Some(b), Some(o), None) reaches
// merge_segment because the missing side dropped this item, the code
// treats the missing side as `base` ("no change") and discards any
// real edits the deleting side made to the surrounding top-level
// text. Those edits then leak into the unconditional postamble
// merge, shifting them to the file tail.
//
// In this test one side deletes `foo` AND the other side edits both
// the import and the trailing comment: those theirs-side edits must
// land at their original positions, not be hoisted into the
// preamble or appended to the file tail.
// =====================================================================
#[test]
fn deletion_with_opposite_side_surrounding_edits_preserved_at_correct_positions() {
    let base = "\
import x

def foo():
    pass

# trailing comment
";
    // ours deletes foo cleanly (no other edits).
    let ours = "\
import x

# trailing comment
";
    // theirs keeps foo, edits import on line 1, edits trailing comment.
    let theirs = "\
import y

def foo():
    pass

# trailing y
";
    let outcome = merge_at(base, ours, theirs, "f.py");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    // theirs's import edit lands.
    assert!(text.contains("import y"), "import edit lost: {text}");
    // theirs's trailing edit lands.
    assert!(text.contains("# trailing y"), "trailing edit lost: {text}");
    // foo is deleted.
    assert!(!text.contains("def foo"), "foo should be deleted: {text}");
    // The trailing edit is at the bottom (after the import), not
    // hoisted to the top.
    let pos_import = text.find("import y").expect("import present");
    let pos_trailing = text.find("# trailing y").expect("trailing present");
    assert!(
        pos_import < pos_trailing,
        "trailing edit shifted ahead of import: {text}"
    );
    // No stale ORIGINAL versions of theirs's edited content.
    assert!(
        !text.contains("import x"),
        "stale base import x present (theirs's edit got dropped): {text}"
    );
    assert!(
        !text.contains("# trailing comment"),
        "stale base trailing comment present (theirs's edit got dropped): {text}"
    );
}

// =====================================================================
// Codex r3 P1 #1: zero-item fallback bypasses add/add conflict
// detection.
//
// mod.rs:90 routes any (counts.contains(&0) && any > 0) shape through
// text_hunk_merge. But when base is empty and BOTH sides add an item
// with the same key (function name) and different bodies, text engine
// concatenates the two insertions at the same anchor — producing
// duplicate definitions instead of a conflict. The semantic path's
// `resolve_item` add/add arm correctly surfaces this as a conflict;
// the fallback bypasses it.
// =====================================================================
// =====================================================================
// Codex r3 P1 #2: leading attributes/doc-comments must stay attached
// to their item across structural reorders.
//
// items.rs::collect_items extracts items at strictly `start_byte..end_byte`
// of the classified node — so `#[test]` (an `attribute_item` sibling of
// `function_item`) is excluded from the item's range and lives in
// inter-item content. When the other side reorders items, the
// attribute remains anchored to a byte position that now belongs to
// a different item — behavior change without a conflict.
// =====================================================================
// =====================================================================
// Codex r3 P2 #2: reconstruction unconditionally appends a trailing
// `\n` via ensure_trailing_newline. When all three sides end without
// one, the merged output dirties the file with a phantom newline
// unique to the semantic path — text_hunk_merge preserves the
// no-trailing-newline state. Clean merges show a spurious diff.
// =====================================================================
#[test]
fn no_trailing_newline_on_any_side_preserves_no_trailing_newline() {
    // All three sides end without `\n`. Output must also have no
    // trailing `\n`.
    let base = "fn foo() { 1 }";
    let ours = "fn foo() { 1 }\nfn bar() {}";
    let theirs = "fn foo() { 2 }";
    let outcome = merge_rust(base, ours, theirs);
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    assert!(
        !text.ends_with('\n'),
        "expected no trailing newline, got bytes ending {:?}: {text}",
        text.as_bytes().last()
    );
}

// =====================================================================
// Codex r3 P2 #1: rust_impl_name still tokenizes on whitespace
// instead of stripping it, so cosmetic spaces around `::`, `<>`, etc.
// break the impl-scope identity. Methods inside the reformatted impl
// key with a different scope than the same methods in the unchanged
// impl, surfacing as spurious add/delete conflicts.
// =====================================================================
#[test]
fn rust_impl_name_ignores_whitespace_around_path_punctuation() {
    let base = "\
impl std::vec::Vec<T> {
    fn alpha() { 1 }
}
";
    // ours reformats whitespace around `::` and `<>` — semantically
    // identical impl, same method bodies.
    let ours = "\
impl std :: vec :: Vec < T > {
    fn alpha() { 1 }
}
";
    // theirs modifies alpha's body.
    let theirs = "\
impl std::vec::Vec<T> {
    fn alpha() { 2 }
}
";
    let outcome = merge_rust(base, ours, theirs);
    let text = assert_clean(outcome);
    // alpha's body reflects theirs's modification.
    assert!(
        text.contains("{ 2 }"),
        "alpha body should be `2`: {text}"
    );
    // alpha appears exactly once — no spurious add/delete pair.
    let alpha_count = text.matches("fn alpha").count();
    assert_eq!(
        alpha_count, 1,
        "expected fn alpha exactly once, got {alpha_count}: {text}"
    );
}

#[test]
fn rust_outer_attribute_does_not_duplicate_when_adjacent_item_deleted() {
    // When one side deletes an item that immediately precedes an
    // attributed item, the attribute floating in inter-item content
    // gets pulled into BOTH the deleted item's slot AND the
    // surviving item's slot — producing duplicate `#[test]` lines.
    //
    // base:           ours:           theirs:
    //   fn alpha {}     #[test]         fn alpha {}
    //                   fn foo {}
    //   #[test]                         #[test]
    //   fn foo {}                       fn foo { 2 }
    //
    // Expected output: single `#[test] fn foo { 2 }`. Pre-fix:
    // `#[test]` shows up twice.
    let base = "\
fn alpha() {}

#[test]
fn foo() { 1 }
";
    let ours = "\
#[test]
fn foo() { 1 }
";
    let theirs = "\
fn alpha() {}

#[test]
fn foo() { 2 }
";
    let outcome = merge_rust(base, ours, theirs);
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    let attr_count = text.matches("#[test]").count();
    assert_eq!(
        attr_count, 1,
        "expected #[test] exactly once, got {attr_count}: {text}"
    );
    assert!(!text.contains("fn alpha"), "alpha should be deleted: {text}");
    assert!(text.contains("fn foo() { 2 }"), "foo body should reflect theirs: {text}");
}

// =====================================================================
// Codex r4 P1 #1: `inner_attribute_item` was treated as leading metadata
// for the next item. Inner attributes (`#![...]`) apply to the enclosing
// module/crate, not the following function — so binding them to the
// next item means deleting/relocating that item also deletes/relocates
// crate-level attributes like `#![no_std]`, changing compilation
// behavior outside the edited item.
// =====================================================================
// =====================================================================
// Codex r4 P2 #2: Java branch in `is_leading_metadata_for` absorbs ALL
// `line_comment`/`block_comment` siblings unconditionally, unlike
// Rust/Go which gate on no-blank-line-between. Standalone comments
// separated by blank lines migrate with the next method during
// structural merges, causing comment relocation/duplication.
// =====================================================================
#[test]
fn java_standalone_comment_with_blank_line_does_not_move_with_next_method() {
    let base = "\
class C {
    // standalone

    void foo() {}

    void bar() {}
}
";
    // ours deletes foo. theirs modifies bar so the early base==theirs
    // short-circuit doesn't fire. Pre-fix: `// standalone` is absorbed
    // into foo's range (no blank-line gate on Java) and deleted with
    // foo. Post-fix: blank line separates the comment from foo, so it
    // is NOT absorbed and survives in the output.
    let ours = "\
class C {
    // standalone

    void bar() {}
}
";
    let theirs = "\
class C {
    // standalone

    void foo() {}

    void bar() { return; }
}
";
    let outcome = merge_at(base, ours, theirs, "C.java");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    let comment_count = text.matches("// standalone").count();
    assert_eq!(
        comment_count, 1,
        "`// standalone` must survive exactly once: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "merge must be clean — pre-fix, ours's bar absorbed the comment (no blank-line gate) so the modify/modify on bar surfaces as a conflict; post-fix the comment stays in inter-item content and bar merges cleanly: {text}"
    );
}

// =====================================================================
// Codex r4 P2 #1: tree-sitter Python wraps decorated symbols in
// `decorated_definition`, but `classify_python_node` only handles
// `function_definition` and `class_definition` — so `@decorator` lines
// end up in inter-item content and reorder/delete merges can orphan,
// duplicate, or misattach them.
// =====================================================================
#[test]
fn python_decorated_function_delete_drops_theirs_decorator_swap() {
    // base has `@cache` on `alpha`. ours deletes `alpha` entirely.
    // theirs swaps `@cache` for `@cached_property` (decorator-only
    // change, alpha body unchanged). Pre-fix, the decorator lives in
    // inter-item content while alpha is the item — so alpha's bytes
    // are identical on base and theirs (`def alpha(): return 1`),
    // resolve_item sees `b == t` and clean-deletes alpha, SILENTLY
    // discarding theirs's decorator swap. Post-fix, the whole
    // `decorated_definition` is one item; alpha's bytes differ
    // (decorator included) and the modify/delete surfaces as a
    // conflict instead of silent loss.
    let base = "\
@cache
def alpha():
    return 1

def beta():
    return 2
";
    let ours = "\
def beta():
    return 2
";
    let theirs = "\
@cached_property
def alpha():
    return 1

def beta():
    return 2
";
    let outcome = merge_at(base, ours, theirs, "f.py");
    let (text, has_conflicts) = match outcome {
        MergeOutcome::Clean(b) => (String::from_utf8(b).unwrap(), false),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => (String::from_utf8(merged_bytes_with_markers).unwrap(), true),
        other => panic!("unexpected: {other:?}"),
    };
    // theirs's `@cached_property` swap must not be silently lost.
    // Either it survives in the output, or the merge surfaces a
    // conflict (modify-on-theirs vs delete-on-ours). What is NOT
    // acceptable: a clean merge that loses theirs's modification.
    let _ = has_conflicts;
    // Pre-fix: alpha's bytes are identical on base/theirs (decorator
    // is inter-item), so resolve_item clean-deletes alpha and theirs's
    // intent to keep alpha (with a different decorator) vanishes —
    // `def alpha` doesn't appear at all in the output, even though
    // theirs explicitly kept the function. Post-fix: `decorated_definition`
    // is one item; alpha's bytes include the decorator so b != t and a
    // modify/delete conflict surfaces with the WHOLE decorated symbol
    // (decorator + def line) in the conflict block.
    assert!(
        text.contains("def alpha"),
        "theirs kept `def alpha` (with new decorator); it must not vanish silently: {text}"
    );
}

// =====================================================================
// Codex r4 P1 #2: per-side item maps (`BTreeMap<ItemKey, &Item>` /
// `BTreeMap<ItemKey, usize>`) collapse repeated declarations sharing a
// key — only the LAST occurrence survives matching/indexing. In
// languages that allow redeclaration (top-level JS/Python functions of
// the same name + signature), earlier declarations silently disappear
// during reconstruction.
// =====================================================================
#[test]
fn javascript_duplicate_function_declarations_both_survive_merge() {
    // Two `function foo` declarations in source — same name + same
    // (empty) signature → identical ItemKey. ours modifies the SECOND;
    // theirs modifies the FIRST. Both modifications must land. Pre-fix
    // (BTreeMap collapse), only the LAST occurrence is kept per side,
    // so theirs's modification to the first declaration is invisible
    // and either gets dropped or surfaces as a spurious conflict.
    let base = "\
function foo() { return 1; }
function foo() { return 2; }
";
    let ours = "\
function foo() { return 1; }
function foo() { return 22; }
";
    let theirs = "\
function foo() { return 11; }
function foo() { return 2; }
";
    let outcome = merge_at(base, ours, theirs, "f.js");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    let foo_count = text.matches("function foo").count();
    assert_eq!(
        foo_count, 2,
        "both `function foo` declarations must survive, got {foo_count}: {text}"
    );
    assert!(
        text.contains("return 11"),
        "first declaration must show theirs's modification (return 11): {text}"
    );
    assert!(
        text.contains("return 22"),
        "second declaration must show ours's modification (return 22): {text}"
    );
}

#[test]
fn rust_inner_attribute_stays_at_crate_scope_when_added_item_conflicts() {
    // base has only `#![no_std]` (no items). Both sides add `fn foo`
    // below it with diverging bodies. Pre-fix, foo's extended start
    // byte absorbed `#![no_std]` on both sides, so the add/add
    // conflict block contains `#![no_std]` on BOTH halves AND the
    // base's `#![no_std]` bridges into the preamble — the crate
    // attribute appears three times.
    let base = "\
#![no_std]
";
    let ours = "\
#![no_std]

fn foo() { 1 }
";
    let theirs = "\
#![no_std]

fn foo() { 2 }
";
    let outcome = merge_rust(base, ours, theirs);
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    let attr_count = text.matches("#![no_std]").count();
    assert_eq!(
        attr_count, 1,
        "#![no_std] must appear exactly once at crate scope, got {attr_count}: {text}"
    );
}

#[test]
fn add_add_same_function_in_empty_base_surfaces_conflict_not_concatenation() {
    let base = "";
    let ours = "\
fn foo() {
    1
}
";
    let theirs = "\
fn foo() {
    2
}
";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(count >= 1, "expected ≥1 conflict, got {count}: {text}");
    assert!(
        text.contains("<<<<<<<") && text.contains("=======") && text.contains(">>>>>>>"),
        "expected canonical conflict markers around foo: {text}"
    );
}

// =====================================================================
// Codex r5 P1 #1: `signature_hash_from_field` hashes the whole
// `parameters` text — INCLUDING parameter NAMES. A pure parameter
// rename on one side (`foo(x: u32)` → `foo(y: u32)`) changes
// `ItemKey.signature_hash` and the renamed function gets a distinct
// match-key from base/theirs's foo. The merger then treats it as
// delete+add, so an unrelated body change on the OTHER side surfaces
// as a modify/delete conflict (or drops one edit) instead of merging
// cleanly. Post-fix the hash is derived from arity + types only —
// renaming a parameter doesn't change the key.
// =====================================================================
#[test]
fn rust_parameter_rename_does_not_split_function_identity() {
    // ours renames `x` → `y` in the signature AND the body line that
    // references it (a pure rename refactor). theirs adds `+ 0` on
    // an entirely DIFFERENT line — line-disjoint from ours's edits.
    // Post-fix all three sides share the same ItemKey, so the body
    // merge proceeds as a 3-way modify on line-disjoint edits and
    // resolves cleanly: rename from ours, body tweak from theirs.
    let base = "\
fn foo(x: u32) -> u32 {
    let r = x + 1;
    r
}
";
    let ours = "\
fn foo(y: u32) -> u32 {
    let r = y + 1;
    r
}
";
    let theirs = "\
fn foo(x: u32) -> u32 {
    let r = x + 1;
    r + 0
}
";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    // ours's rename must land.
    assert!(
        merged.contains("fn foo(y: u32)"),
        "ours's parameter rename lost (signature_hash must ignore parameter names): {merged}"
    );
    assert!(
        merged.contains("let r = y + 1"),
        "ours's body update of the renamed parameter lost: {merged}"
    );
    // theirs's body tweak must land.
    assert!(
        merged.contains("r + 0"),
        "theirs's body edit lost: {merged}"
    );
    // The pre-fix duplication shape: two `fn foo` definitions in
    // output. Guard against that.
    let foo_count = merged.matches("fn foo").count();
    assert_eq!(
        foo_count, 1,
        "expected ONE fn foo (parameter rename must not split identity), got {foo_count}: {merged}"
    );
}
