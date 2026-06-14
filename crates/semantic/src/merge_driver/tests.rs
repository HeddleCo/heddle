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
    assert!(merged.contains("ours_new"), "ours_new missing: {merged}");
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
            assert!(text.contains("ours_method"), "ours method missing: {text}");
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
        MergeOutcome::Conflicts { conflict_count, .. } => {
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
        assert!(merged.contains(&format!("{i}_OURS")), "ours edit {i} lost");
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
    assert!(
        !merged.contains("fn target"),
        "target should be deleted: {merged}"
    );
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
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    assert!(
        !text.contains("fn gone"),
        "gone() should be removed: {text}"
    );
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
    // Both sides modify the same line of the preamble. The comment block is
    // separated from `fn f` by a blank line so it is NOT absorbed as the
    // item's leading metadata — it stays in the preamble (inter-item
    // segment 0). `use` lines can no longer serve here: they are now
    // path-keyed items (heddle#468), so divergent `use` edits would union
    // instead of conflicting. Diverging comment edits force text_hunk_merge
    // on the preamble to produce Conflicts, exercising the Conflicts arm of
    // the inter-item match.
    let base = "\
// header note: base

fn f() { 1 }
";
    let ours = "\
// header note: OURS

fn f() { 1 }
";
    let theirs = "\
// header note: THEIRS

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
    assert!(
        merged.contains("return \"AAA\""),
        "theirs edit lost: {merged}"
    );
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
    assert!(merged.contains("(a A) String()"), "A.String lost: {merged}");
    assert!(merged.contains("(b B) String()"), "B.String lost: {merged}");
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
    assert!(merged.contains("fn x() { 11 }"), "ours edit lost: {merged}");
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
    assert!(
        merged.contains("return a + b + 0"),
        "ours edit lost: {merged}"
    );
    assert!(
        merged.contains("return a - b - 0"),
        "theirs edit lost: {merged}"
    );
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
    assert!(
        merged.contains("return x * y"),
        "theirs edit lost: {merged}"
    );
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
    // base has only a top-level comment; both sides add their own items
    // (a `use` re-export plus a function) under a shared `// top header`
    // preamble. That shared header line must appear exactly once.
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
    // ours has no parseable items (only a top-level comment), base and
    // theirs each have one function. Pre-fix, ours's "// lone comment\n"
    // is consumed by the first iteration's preamble fallback AND
    // re-emitted by the postamble merge — appearing twice in the
    // output. (A `use` line can no longer stand in for the zero-items
    // side: `use` is now a path-keyed item — heddle#468.)
    let base = "fn a() { 1 }\n";
    let ours = "// lone comment\n";
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
    let use_count = text.matches("// lone comment").count();
    assert_eq!(
        use_count, 1,
        "expected ours's `// lone comment` exactly once, got {use_count}: {text}"
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
    assert!(text.contains("{ 2 }"), "alpha body should be `2`: {text}");
    // alpha appears exactly once — no spurious add/delete pair.
    let alpha_count = text.matches("fn alpha").count();
    assert_eq!(
        alpha_count, 1,
        "expected fn alpha exactly once, got {alpha_count}: {text}"
    );
}

// =====================================================================
// heddle#121: rust_impl_name's old `split_whitespace().join(" ")` shape
// kept whitespace attached to punctuation, so reformatting around `*`
// or `&` (pointer/reference receivers in the `for <type>` slot) yielded
// distinct impl keys. The `_path_punctuation` sibling above covers
// `::`/`<>`; this exercises the pointer/reference case the r2-sweep
// follow-up specifically called out.
// =====================================================================
#[test]
fn rust_impl_name_ignores_whitespace_around_pointer_punctuation() {
    let base = "\
struct Foo;
impl MyTrait for *const Foo {
    fn alpha() { 1 }
}
";
    // ours reformats whitespace around the `*` in the pointer type —
    // semantically identical impl, same method body.
    let ours = "\
struct Foo;
impl MyTrait for * const Foo {
    fn alpha() { 1 }
}
";
    // theirs modifies alpha's body.
    let theirs = "\
struct Foo;
impl MyTrait for *const Foo {
    fn alpha() { 2 }
}
";
    let outcome = merge_rust(base, ours, theirs);
    let text = assert_clean(outcome);
    assert!(text.contains("{ 2 }"), "alpha body should be `2`: {text}");
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
    assert!(
        !text.contains("fn alpha"),
        "alpha should be deleted: {text}"
    );
    assert!(
        text.contains("fn foo() { 2 }"),
        "foo body should reflect theirs: {text}"
    );
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
// heddle#490 r5: same-key CONTAINER add/add with NO base anchor. A recursive
// body merge is only sound when a BASE container exists to diff each side
// against. For the no-base add/add case (both sides independently created the
// same-key container, nothing to anchor to), recursing into the bodies via
// `merge_container_3way`/`merge_region` mis-weaves the delimiters: it attaches
// each side's opening `{`/preamble to that side's first added child, so `{` is
// emitted before BOTH children — duplicating the delimiter — and the empty-base
// safety fallback can then duplicate the whole module silently. The fix STOPS
// recursing for the no-base case and compares the two added containers as WHOLE
// units (header + body + footer): identical → take one copy; any divergence →
// one whole-container conflict. This single rule subsumes r4 (divergent header
// ⇒ different whole content ⇒ conflict) and fixes r5 (identical header + disjoint
// children ⇒ different whole content ⇒ conflict, no delimiter duplication). The
// recursive structural body merge is preserved for the base-anchored case below,
// where diffing against the base makes it sound.
// =====================================================================

// r5 core: identical header, DISJOINT children, no base. Pre-fix the empty-base
// recursion duplicated the opening `{` (one before `fn a`, one before `fn b`)
// and could silently duplicate the module; post-fix the divergent whole
// containers conflict instead.
#[test]
fn add_add_container_disjoint_children_no_base_conflicts() {
    let base = "";
    let ours = "mod foo {\n    fn a() {}\n}\n";
    let theirs = "mod foo {\n    fn b() {}\n}\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(count >= 1, "expected ≥1 conflict, got {count}: {text}");
    assert!(
        text.contains("<<<<<<<") && text.contains("=======") && text.contains(">>>>>>>"),
        "expected canonical conflict markers around the divergent module: {text}"
    );
    assert!(
        text.contains("fn a()") && text.contains("fn b()"),
        "both sides' bodies must survive inside the conflict: {text}"
    );
    // The delimiter-duplication bug: pre-fix `{` was emitted before both `fn a`
    // and `fn b`, yielding three `{` for a one-brace module. The whole-container
    // conflict emits each side's body verbatim, so each opening brace appears
    // exactly as written on that side.
    assert_eq!(
        text.matches("mod foo {").count(),
        2,
        "expected one `mod foo {{` per conflict side, not a duplicated delimiter: {text}"
    );
}

// The clean take-one counterpart: both sides add the BYTE-IDENTICAL container.
// No base, but nothing diverges → take one copy, no spurious conflict, no
// duplication.
#[test]
fn add_add_container_identical_no_base_takes_one() {
    let base = "";
    let ours = "mod foo {\n    fn a() {}\n}\n";
    let theirs = "mod foo {\n    fn a() {}\n}\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert_eq!(
        merged.matches("mod foo").count(),
        1,
        "module duplicated: {merged}"
    );
    assert_eq!(
        merged.matches("fn a()").count(),
        1,
        "body duplicated: {merged}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "no spurious conflict on identical add/add containers: {merged}"
    );
}

// r4, now via the general rule: divergent HEADER add/add. `pub mod foo` vs
// `mod foo` ⇒ different whole content ⇒ whole-container conflict. Pre-r4 this
// silently fused into a duplicated/malformed module; the whole-container rule
// surfaces it as a conflict with both header spellings preserved.
#[test]
fn add_add_container_divergent_header_no_base_conflicts() {
    let base = "";
    let ours = "pub mod foo {\n    fn a() {}\n}\n";
    let theirs = "mod foo {\n    fn a() {}\n}\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "expected ≥1 conflict on the divergent header, got {count}: {text}"
    );
    assert!(
        text.contains("<<<<<<<") && text.contains("=======") && text.contains(">>>>>>>"),
        "expected canonical conflict markers around the divergent module: {text}"
    );
    assert!(
        text.contains("pub mod foo") && text.contains("mod foo {"),
        "both header spellings must survive inside the conflict: {text}"
    );
    assert!(
        !matches!(merge_rust(base, ours, theirs), MergeOutcome::Clean(_)),
        "divergent add/add must not be a silent clean concat"
    );
}

// The masking case from the finding: a SHARED child used to let the recursive
// body merge weave a clean module, hiding the no-base unsoundness. With the
// whole-container rule, divergent bodies (`fn a` vs `fn b`) conflict even when a
// child is shared — there is still no base to anchor the merge.
#[test]
fn add_add_container_shared_child_divergent_body_no_base_conflicts() {
    let base = "";
    let ours = "mod foo {\n    fn shared() {}\n    fn a() {}\n}\n";
    let theirs = "mod foo {\n    fn shared() {}\n    fn b() {}\n}\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(count >= 1, "expected ≥1 conflict, got {count}: {text}");
    assert!(
        text.contains("<<<<<<<") && text.contains("=======") && text.contains(">>>>>>>"),
        "expected canonical conflict markers: {text}"
    );
    assert!(
        text.contains("fn a()") && text.contains("fn b()"),
        "both divergent bodies must survive inside the conflict: {text}"
    );
}

// The recursive structural merge is PRESERVED for the base-anchored case: base
// has `mod foo { fn a }`, ours adds `fn b`, theirs adds `fn c`. A real base
// container exists to diff against, so the body merges structurally + cleanly
// (`fn a` unchanged, `fn b`/`fn c` are disjoint additions). This is the path
// the r5 fix must NOT regress.
#[test]
fn base_anchored_container_merges_structurally_clean() {
    let base = "mod foo {\n    fn a() {}\n}\n";
    let ours = "mod foo {\n    fn a() {}\n    fn b() {}\n}\n";
    let theirs = "mod foo {\n    fn a() {}\n    fn c() {}\n}\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert_eq!(
        merged.matches("mod foo").count(),
        1,
        "module duplicated: {merged}"
    );
    assert_eq!(
        merged.matches("mod foo {").count(),
        1,
        "delimiter duplicated: {merged}"
    );
    assert!(
        merged.contains("fn a()") && merged.contains("fn b()") && merged.contains("fn c()"),
        "base child + both disjoint additions must weave into the single module: {merged}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "no spurious conflict on a base-anchored disjoint-additions merge: {merged}"
    );
}

// Relocated from `conformance_484_structural_matrix`: a no-base add/add of a
// DIVERGENT `pub mod m`. Pre-r5 this only "merged clean" via the buggy
// structural-recursion → conservation-floor → whole-file-concat fallback, which
// emitted TWO `pub mod m` blocks — an illegal duplicate module (E0428). The
// whole-container rule surfaces it as a conflict instead of silently producing
// the duplicate. A `mod` is NOT reopenable, so a conflict (not a weave) is the
// only sound outcome with no base anchor.
#[test]
fn add_add_divergent_mod_no_base_conflicts_not_duplicate_module() {
    let base = "pub fn a() {}\n";
    let ours = "pub fn a() {}\n\npub mod m {\n    pub use crate::x;\n}\n";
    let theirs = "pub fn a() {}\n\npub mod m {\n    pub use crate::y;\n}\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(count >= 1, "expected ≥1 conflict, got {count}: {text}");
    assert!(
        text.contains("<<<<<<<") && text.contains("=======") && text.contains(">>>>>>>"),
        "expected canonical conflict markers: {text}"
    );
    assert!(
        text.contains("pub use crate::x") && text.contains("pub use crate::y"),
        "both sides' re-exports must survive inside the conflict: {text}"
    );
}

// =====================================================================
// heddle#490 r6: BASE-anchored container whose body is EMPTY (`mod foo {}`),
// with both sides adding DIFFERENT children. The body's opening delimiter `{`
// lives at the head of each side's first inter-item range; folding it into the
// child weave let ours's `{` and theirs's `{` each emit in a separate added-
// child slot, duplicating the opening delimiter. Pre-fix a clean version was
// caught by the conservation floor (re-parse failed → text fallback), but a
// version that ALSO carried a real conflict (a divergent header) shipped the
// malformed body because the floor skipped conflict outputs. The fix makes the
// body delimiter structural — merged once in `merge_container_3way`, emitted
// around the woven children — so it can never duplicate, AND extends the floor
// to conflict outputs as a belt-and-suspenders well-formedness guard.
// =====================================================================

// Clean case: empty base body, ours adds `fn a`, theirs adds `fn b`, headers
// agree. Must weave both children into ONE `mod foo { ... }` with exactly one
// opening and one closing brace, no conflict.
#[test]
fn empty_base_container_both_sides_add_weaves_single_delimiter_clean() {
    let base = "mod foo {}\n";
    let ours = "mod foo {\n    fn a() {}\n}\n";
    let theirs = "mod foo {\n    fn b() {}\n}\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert_eq!(
        merged.matches("mod foo {").count(),
        1,
        "opening delimiter duplicated: {merged}"
    );
    assert!(
        merged.contains("fn a()") && merged.contains("fn b()"),
        "both disjoint additions must weave into the single module: {merged}"
    );
    assert_eq!(
        merged.matches('{').count(),
        merged.matches('}').count(),
        "braces must balance: {merged}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "no spurious conflict on disjoint additions into an empty base body: {merged}"
    );
}

// Conflict case: empty base body + a DIVERGENT header (`pub mod` vs
// `pub(crate) mod`) AND both sides adding different children. The header
// conflicts, but the merged output must be WELL-FORMED: exactly one body
// opening `{` and one closing `}`, balanced braces, conflict markers present.
// Pre-fix this shipped a duplicated `{` (open=4, close=3) because the floor
// skipped conflict outputs.
#[test]
fn empty_base_container_divergent_header_conflict_is_well_formed() {
    let base = "mod foo {}\n";
    let ours = "pub mod foo {\n    fn a() {}\n}\n";
    let theirs = "pub(crate) mod foo {\n    fn b() {}\n}\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "expected ≥1 conflict on the divergent header: {text}"
    );
    assert!(
        text.contains("<<<<<<<") && text.contains("=======") && text.contains(">>>>>>>"),
        "expected canonical conflict markers: {text}"
    );
    assert_eq!(
        text.matches('{').count(),
        text.matches('}').count(),
        "delimiter duplicated — braces unbalanced (the r6 malformed-conflict bug): {text}"
    );
    // Resolving either side must yield valid, balanced code.
    for side in resolve_both_sides(&text) {
        assert_eq!(
            side.matches('{').count(),
            side.matches('}').count(),
            "a resolved side has unbalanced braces: {side}"
        );
        assert!(
            crate::parser::ParsedFile::parse(side.as_str(), crate::parser::Language::Rust)
                .is_some(),
            "a resolved side does not parse: {side}"
        );
    }
}

// The extended floor guards conflict outputs: a structurally malformed conflict
// (a resolved side that does not re-parse) must NOT be emitted silently — it is
// routed to the textual fallback, which is well-formed by construction. We
// exercise the guard through a real merge that, absent the structural fix,
// would emit a duplicate-delimiter conflict; with the floor extended, even a
// regression in the weave can no longer ship a malformed conflict. Here we
// assert the strong invariant directly: whatever the driver returns for the
// r6 shape, both resolved sides re-parse.
#[test]
fn conflict_output_resolved_sides_always_reparse() {
    // A divergent-header empty-base container (the r6 malformed-conflict shape).
    let base = "mod foo {}\n";
    let ours = "pub mod foo {\n    fn a() {}\n}\n";
    let theirs = "pub(crate) mod foo {\n    fn b() {}\n}\n";
    let text = match merge_rust(base, ours, theirs) {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    for side in resolve_both_sides(&text) {
        assert!(
            crate::parser::ParsedFile::parse(side.as_str(), crate::parser::Language::Rust)
                .is_some(),
            "driver emitted a conflict whose resolved side does not parse: {side}"
        );
    }
}

// Direct floor-guard test: a deliberately MALFORMED conflict output (the r6
// duplicate-opening-`{` shape) is rejected by the extended floor, while a
// well-formed conflict passes. This pins the guard itself, independent of the
// structural fix that makes the driver emit well-formed conflicts.
#[test]
fn extended_floor_rejects_malformed_conflict_passes_well_formed() {
    use crate::parser::Language::Rust;

    // The exact malformed shape the pre-fix driver shipped: a divergent header
    // conflict followed by a body that duplicated the opening `{` (two `{`, one
    // `}`). Resolving the OURS side is unbalanced and unparseable.
    let malformed = "\
<<<<<<< OURS
pub mod foo
=======
pub(crate) mod foo
>>>>>>> THEIRS
{
    fn b() {}{
    fn a() {}
}
";
    assert!(
        !super::conflict_well_formed(malformed.as_bytes(), Rust),
        "extended floor must reject a duplicate-delimiter conflict"
    );

    // A balanced, parseable conflict (both resolved sides are valid Rust) passes.
    let well_formed = "\
<<<<<<< OURS
fn foo() { 1 }
=======
fn foo() { 2 }
>>>>>>> THEIRS
";
    assert!(
        super::conflict_well_formed(well_formed.as_bytes(), Rust),
        "extended floor must accept a well-formed conflict whose sides both parse"
    );

    // Structurally broken markers (an orphan `=======` with no open `<<<<<<<`)
    // resolve to None → treated as not-well-formed.
    let broken_markers = "fn a() {}\n=======\nfn b() {}\n";
    assert!(
        !super::conflict_well_formed(broken_markers.as_bytes(), Rust),
        "extended floor must reject structurally broken conflict markers"
    );
}

#[test]
fn resolve_conflict_sides_recognizes_indented_markers() {
    let start = "<<<<<<<";
    let sep = "=======";
    let end = ">>>>>>>";
    let text = format!(
        "\
mod m {{
    {start} OURS
    fn f() {{
    {sep}
    fn f() {{}}
    {end} THEIRS
}}
"
    );
    let (ours, theirs) =
        super::resolve_conflict_sides(&text).expect("indented conflict markers must resolve");

    assert!(
        !ours.contains("<<<<<<<") && !ours.contains("=======") && !ours.contains(">>>>>>>"),
        "ours side must not retain conflict markers:\n{ours}"
    );
    assert!(
        !theirs.contains("<<<<<<<") && !theirs.contains("=======") && !theirs.contains(">>>>>>>"),
        "theirs side must not retain conflict markers:\n{theirs}"
    );
    assert!(
        !super::conflict_well_formed(text.as_bytes(), crate::parser::Language::Rust),
        "indented markers must route through structural side parsing, where the broken ours side fails"
    );
}

#[test]
fn resolve_conflict_sides_does_not_treat_marker_like_data_as_markers() {
    let start = "<<<<<<<";
    let sep = "=======";
    let end = ">>>>>>>";
    let text = format!(
        "\
fn f() {{
    {start}not a marker
    {sep}not a marker
    {end}not a marker
}}
"
    );
    let (ours, theirs) =
        super::resolve_conflict_sides(&text).expect("marker-like content must remain content");

    assert_eq!(ours, text);
    assert_eq!(theirs, text);
}

#[test]
fn resolve_conflict_sides_preserves_column_zero_conflicts() {
    let start = "<<<<<<<";
    let sep = "=======";
    let end = ">>>>>>>";
    let text = format!(
        "\
{start} OURS
fn f() {{ 1 }}
{sep}
fn f() {{ 2 }}
{end} THEIRS
"
    );
    let (ours, theirs) =
        super::resolve_conflict_sides(&text).expect("column-zero conflict must resolve");

    assert_eq!(ours, "fn f() { 1 }\n");
    assert_eq!(theirs, "fn f() { 2 }\n");
}

// Resolve a conflict-marked text into its two sides (take-ours, take-theirs) by
// dropping the markers and the opposite side's hunks. Mirrors the marker shape
// emitted by `heddle-merge::markers` / `emit_addadd_conflict`.
fn resolve_both_sides(text: &str) -> [String; 2] {
    let (ours, theirs) =
        super::resolve_conflict_sides(text).expect("conflict markers must resolve in test output");
    [ours, theirs]
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

// =====================================================================
// Codex r5 P1 #2: C/C++ `classify_c_node` derives the function name
// via `identifier_in_subtree` — a DFS over the declarator subtree that
// matches the first `identifier` / `type_identifier` / etc. it sees.
// For a templated out-of-class definition `void Foo<U>::bar()`, the
// qualified identifier's scope is a `template_type` whose first
// descendant is a `type_identifier` ("Foo") — that wins the DFS over
// the actual method name ("bar"). All methods on the same templated
// type collapse to name="Foo" and end up keyed positionally; an added
// method in the middle of the run misaligns occurrence indexes, so
// two unrelated methods get 3-way merged against each other.
// =====================================================================
#[test]
fn cpp_templated_out_of_class_methods_keyed_by_their_own_name_not_template_scope() {
    // base has `Foo<U>::bar` then `Foo<U>::foo`. ours inserts
    // `Foo<U>::baz` between them. theirs edits `Foo<U>::foo`'s body.
    // Each side touches a disjoint method — clean merge expected.
    //
    // Pre-fix every method classifies as name="Foo" (the template_type
    // scope's first type_identifier). With ours's added middle
    // method, the per-side occurrence indexes diverge: base/theirs
    // place `foo` at index 1 while ours places `baz` at index 1. So
    // resolve_item 3-way merges `Foo<U>::foo`'s body against
    // `Foo<U>::baz`'s body — a clean source file gets corrupted.
    let base = "\
template <typename U>
void Foo<U>::bar() { int x = 0; (void)x; }

template <typename U>
void Foo<U>::foo() { int y = 0; (void)y; }
";
    let ours = "\
template <typename U>
void Foo<U>::bar() { int x = 0; (void)x; }

template <typename U>
void Foo<U>::baz() { int z = 0; (void)z; }

template <typename U>
void Foo<U>::foo() { int y = 0; (void)y; }
";
    let theirs = "\
template <typename U>
void Foo<U>::bar() { int x = 0; (void)x; }

template <typename U>
void Foo<U>::foo() { int y = 0; (void)y; int yy = y; (void)yy; }
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    // ours's added baz must land intact.
    assert!(
        text.contains("Foo<U>::baz()"),
        "ours's added Foo<U>::baz() must appear: {text}"
    );
    assert!(
        text.contains("int z = 0"),
        "Foo<U>::baz's body must survive verbatim: {text}"
    );
    // theirs's edit on foo must land.
    assert!(
        text.contains("int yy = y"),
        "theirs's edit on Foo<U>::foo must survive: {text}"
    );
    // Each named method appears exactly once — no duplication from
    // re-emission via misaligned occurrence indexes, no body collapse.
    let bar_count = text.matches("Foo<U>::bar()").count();
    let foo_count = text.matches("Foo<U>::foo()").count();
    let baz_count = text.matches("Foo<U>::baz()").count();
    assert_eq!(
        bar_count, 1,
        "Foo<U>::bar() must appear exactly once, got {bar_count}: {text}"
    );
    assert_eq!(
        foo_count, 1,
        "Foo<U>::foo() must appear exactly once, got {foo_count}: {text}"
    );
    assert_eq!(
        baz_count, 1,
        "Foo<U>::baz() must appear exactly once, got {baz_count}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "merge must be clean — disjoint method edits + a clean addition: {text}"
    );
}

// =====================================================================
// Codex r5 P1 #3: tree-sitter JS/TS represents decorators as a
// `decorator` node sibling that precedes the decorated `method_definition`
// inside a `class_body`. `is_leading_metadata_for` returns false for
// EVERY JS/TS node, so decorators end up in inter-item content. When
// both sides add a new decorated method at the SAME structural position
// (between two existing methods), each side's inter-item segment
// carries a different decorator — the 3-way merge of those segments
// surfaces a spurious conflict (or worse, leaks the wrong decorator
// onto the wrong method). Post-fix decorators bind to their method's
// item range; new decorated methods are added cleanly with their
// decorators attached.
// =====================================================================
#[test]
fn typescript_decorator_attaches_to_added_method_via_leading_metadata() {
    // base has two undecorated methods. ours adds a new decorated
    // method `middle` between them; theirs adds a different decorated
    // method `other` between them. Pre-fix, the decorators live in
    // ours's/theirs's inter-item segments between `foo` and the added
    // method — and base's matching inter-item segment is just
    // whitespace. The 3-way merge of those segments has all three
    // sides disagreeing (base = `\n  `, ours has `@Get` text, theirs
    // has `@Post` text), so a conflict marker drops into the inter-
    // item gap. Post-fix `@Get()` is part of `middle`'s item range and
    // `@Post()` is part of `other`'s, so the inter-item gap is just
    // whitespace on every side and the merge resolves cleanly.
    let base = "\
class C {
  foo() {}
  bar() {}
}
";
    let ours = "\
class C {
  foo() {}
  @Get()
  middle() {}
  bar() {}
}
";
    let theirs = "\
class C {
  foo() {}
  @Post()
  other() {}
  bar() {}
}
";
    let outcome = merge_at(base, ours, theirs, "f.ts");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    // Both added methods land.
    assert!(
        text.contains("middle()"),
        "ours's middle() must land: {text}"
    );
    assert!(
        text.contains("other()"),
        "theirs's other() must land: {text}"
    );
    // Both decorators land — each EXACTLY once.
    let get_count = text.matches("@Get(").count();
    let post_count = text.matches("@Post(").count();
    assert_eq!(
        get_count, 1,
        "@Get() must appear exactly once (attached to middle), got {get_count}: {text}"
    );
    assert_eq!(
        post_count, 1,
        "@Post() must appear exactly once (attached to other), got {post_count}: {text}"
    );
    // @Get must immediately precede middle (no other line between);
    // @Post must immediately precede other.
    let get_idx = text.find("@Get").expect("@Get present");
    let middle_idx = text.find("middle()").expect("middle present");
    let post_idx = text.find("@Post").expect("@Post present");
    let other_idx = text.find("other()").expect("other present");
    assert!(get_idx < middle_idx, "@Get must precede middle: {text}");
    assert!(post_idx < other_idx, "@Post must precede other: {text}");
    // Critical: each decorator binds to its OWN method, not the
    // adjacent one. Concretely, between @Get and middle there should
    // be no `bar` or `other` token; between @Post and other there
    // should be no `bar` or `middle` token.
    let between_get_and_middle = &text[get_idx..middle_idx];
    assert!(
        !between_get_and_middle.contains("other") && !between_get_and_middle.contains("bar"),
        "@Get must bind directly to middle, found stray tokens: {between_get_and_middle:?}"
    );
    let between_post_and_other = &text[post_idx..other_idx];
    assert!(
        !between_post_and_other.contains("middle") && !between_post_and_other.contains("bar"),
        "@Post must bind directly to other, found stray tokens: {between_post_and_other:?}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "merge must be clean — disjoint additions of decorated methods: {text}"
    );
}

// =====================================================================
// Codex r5 P1 #4: `reconcile_trailing_newline` pops a single byte when
// majority votes "no trailing newline". If the output ends with CRLF
// (one side carries Windows line endings into the postamble), popping
// the `\n` alone leaves a dangling `\r` — line-ending corruption.
// CRLF must be popped AS A UNIT.
// =====================================================================
#[test]
fn crlf_trailing_pair_popped_as_unit_when_majority_has_no_trailing_newline() {
    // base and ours both lack a trailing newline. theirs is the only
    // side that ends with CRLF. Majority (base + ours, 2 of 3) votes
    // "no trailing newline" so reconcile_trailing_newline strips it.
    // Pre-fix it pops a single byte (the `\n`), leaving a trailing
    // `\r`. Post-fix it pops both bytes of the CRLF together.
    //
    // The body change on ours (and no change on theirs) is what funnels
    // theirs's CRLF postamble into the output via the postamble merge.
    let base = "fn foo() {}";
    let ours = "fn foo() { 1 }";
    let theirs = "fn foo() {}\r\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(
        !merged.ends_with('\r'),
        "merged output must not end with a dangling \\r (CRLF must pop as a unit): {merged:?}"
    );
    // Stronger: the output should end with `}` (matching the
    // no-trailing-newline majority).
    assert!(
        merged.ends_with('}'),
        "merged output should end at the closing brace (majority wants no trailing newline): {merged:?}"
    );
}

// =====================================================================
// Codex r6 P1 #1 (cid 3256117895): C/C++ functions key on (kind, name,
// signature) with `extra_scope=[]`. Two methods with the same name on
// different classes/namespaces — `A::foo()` and `B::foo()` — collapse
// to the same ItemKey. The MatchKey occurrence-index disambiguator
// (r4 fix) saves the case where both sides preserve method order, but
// when one side adds a new same-named method in a different class
// BEFORE an existing one, occurrence indices misalign and
// resolve_item 3-way merges UNRELATED functions across sides.
// =====================================================================
#[test]
fn cpp_same_named_methods_in_different_classes_keep_distinct_identities() {
    // base has only `A::foo`. ours adds a new class B with its own
    // `B::foo` BEFORE A's definition (a perfectly valid C++ refactor:
    // grouped class declarations followed by definitions). theirs
    // edits A::foo's body, disjoint from ours's structural addition.
    //
    // Pre-fix every `foo` keys as (Function, "foo", [], sig). Per-side
    // occurrence indices:
    //   base    [A::foo=0]
    //   ours    [B::foo=0, A::foo=1]
    //   theirs  [A::foo=0]
    // MatchKey (foo,0) pairs base's A::foo with ours's B::foo and
    // theirs's A::foo. resolve_item runs a 3-way merge on three
    // unrelated function bodies — base/theirs match (A unchanged on
    // theirs's left vs base) but ours diverges with B's bytes, so it
    // takes ours's B::foo bytes at A::foo's slot, dropping theirs's
    // A::foo edit entirely and emitting B::foo at A's position.
    //
    // Post-fix `extra_scope=["A"]` and `["B"]` mean B::foo and A::foo
    // have distinct ItemKeys; theirs's A::foo edit lands at A's
    // position and ours's B::foo is cleanly added.
    let base = "\
class A { public: void foo(); };

void A::foo() { int a = 0; (void)a; }
";
    let ours = "\
class A { public: void foo(); };
class B { public: void foo(); };

void B::foo() { int b = 99; (void)b; }
void A::foo() { int a = 0; (void)a; }
";
    let theirs = "\
class A { public: void foo(); };

void A::foo() { int a = 2; (void)a; }
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    // theirs's edit on A::foo must land — pre-fix it's lost because
    // (foo,0) takes ours's B::foo bytes in place of A::foo.
    assert!(
        text.contains("int a = 2"),
        "theirs's A::foo edit must survive: {text}"
    );
    // ours's added B::foo must land with its body intact.
    assert!(
        text.contains("int b = 99"),
        "ours's added B::foo body must survive: {text}"
    );
    // Each definition appears exactly once — no duplication, no
    // cross-contamination via misaligned occurrence indexing.
    let a_def_count = text.matches("void A::foo()").count();
    let b_def_count = text.matches("void B::foo()").count();
    assert_eq!(
        a_def_count, 1,
        "A::foo definition must appear exactly once, got {a_def_count}: {text}"
    );
    assert_eq!(
        b_def_count, 1,
        "B::foo definition must appear exactly once, got {b_def_count}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "disjoint cross-class edits + addition must merge cleanly: {text}"
    );
}

// =====================================================================
// Codex r6 P1 #2 (cid 3256117900): r6's structural signature hash uses
// each parameter's `type` field text only. In C/C++, the parameter
// `type` is the type-specifier (e.g. "int"), but pointer/reference/
// array/function-pointer modifiers live in the `declarator` field
// alongside the parameter name. So `f(int)`, `f(int*)`, `f(int&)`,
// `f(int[])` all collapse to the same signature_hash and the same
// ItemKey — distinct overloads share an identity slot.
// =====================================================================
#[test]
fn cpp_pointer_overload_distinct_from_value_overload() {
    // base has `void f(int)`. ours adds a new overload `void f(int*)`
    // ABOVE it (a common refactor: add the more-specific overload
    // first). theirs edits the body of `f(int)`. Disjoint changes —
    // clean merge expected.
    //
    // Pre-fix every `f` keys identically because both params hash on
    // type="int". Per-side occurrences:
    //   base    [f(int)=0]
    //   ours    [f(int*)=0, f(int)=1]
    //   theirs  [f(int)=0]
    // MatchKey (f,0) pairs base's f(int) with ours's f(int*) and
    // theirs's f(int)-edited. resolve_item 3-way merges unrelated
    // function bodies → conflict on what should be a clean merge.
    //
    // Post-fix the declarator shape ('*' for the pointer overload,
    // empty for the value overload) feeds into the signature hash,
    // so f(int) and f(int*) have distinct ItemKeys.
    let base = "\
void f(int) { int x = 0; (void)x; }
";
    let ours = "\
void f(int* p) { int y = *p; (void)y; }
void f(int) { int x = 0; (void)x; }
";
    let theirs = "\
void f(int) { int x = 99; (void)x; }
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    // theirs's edit on f(int) must land — pre-fix it's lost to ours's
    // f(int*) bytes taking the (f,0) slot.
    assert!(
        text.contains("int x = 99"),
        "theirs's edit on f(int) must survive: {text}"
    );
    // ours's added f(int*) body must land verbatim.
    assert!(
        text.contains("int y = *p"),
        "ours's added f(int*) body must survive: {text}"
    );
    // No duplication or omission of either overload.
    let value_overload = text.matches("void f(int)").count();
    let ptr_overload = text.matches("void f(int* p)").count();
    assert_eq!(
        value_overload, 1,
        "void f(int) must appear exactly once, got {value_overload}: {text}"
    );
    assert_eq!(
        ptr_overload, 1,
        "void f(int* p) must appear exactly once, got {ptr_overload}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "disjoint overload addition + body edit must merge cleanly: {text}"
    );
}

// =====================================================================
// Codex r6 P2 #1 (cid 3256117904): `emit_addadd_conflict` hardcodes
// LF-only marker lines (`"\n"`). When a CRLF-style file hits the
// semantic add/add path (empty base + both sides add the same item
// with different bodies), the body bytes inherit CRLF from the
// source but the conflict markers themselves end with bare LF —
// mixed line endings break Windows tooling and produce noisy diffs.
// =====================================================================
#[test]
fn crlf_add_add_conflict_markers_use_crlf() {
    // base is empty (or comment-only) so both sides' new `foo()`
    // items hit the resolve_item add/add path. All three inputs use
    // CRLF line endings — the emitted conflict markers must do the
    // same so the file isn't half-LF half-CRLF afterwards.
    let base = "// header\r\n";
    let ours = "// header\r\nfn foo() {\r\n    1\r\n}\r\n";
    let theirs = "// header\r\nfn foo() {\r\n    2\r\n}\r\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert_eq!(count, 1, "expected 1 conflict, got {count}: {text:?}");
    // Find every marker line and assert it ends with `\r\n`.
    for line in text.split_inclusive('\n') {
        if line.starts_with("<<<<<<<") || line.starts_with("=======") || line.starts_with(">>>>>>>")
        {
            assert!(
                line.ends_with("\r\n"),
                "marker line `{}` must end with CRLF in a CRLF file: {text:?}",
                line.trim_end_matches('\n').trim_end_matches('\r'),
            );
        }
    }
    // Stronger: no bare LF in the entire output (the file is wholly
    // CRLF, so any `\n` not preceded by `\r` is a regression).
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            assert!(
                i > 0 && bytes[i - 1] == b'\r',
                "bare LF at byte {i} in otherwise-CRLF output: {text:?}"
            );
        }
    }
}

// =====================================================================
// Codex r2 P2 (PR #193, cid 3291860840): the EolPolicy refactor
// (heddle#189 r1) consolidated detection to a single whole-file policy
// computed once from `[base, ours, theirs]`. That dropped the previous
// per-item weighting in `emit_addadd_conflict`: in a mixed-EOL file
// where the LF surrounding context outnumbers a CRLF-bodied add/add
// item, the markers flip to LF and wrap a CRLF body — reintroducing
// the exact mixed-EOL hunk shape the r6 P2 #1 fix avoided. Markers
// must follow the EOL of the items they bracket.
// =====================================================================
#[test]
fn mixed_eol_add_add_marker_follows_item_bytes_not_whole_file() {
    // Surrounding context is mostly LF (many `\n` lines) but the new
    // `foo` items both arrive with CRLF bodies. Pre-fix: whole-file
    // policy votes LF (LF count >> CRLF count) → markers emit LF
    // wrapping a CRLF body. Post-fix: marker EOL follows the item
    // bytes, so both items being CRLF means the markers are CRLF.
    // Blank lines between the LF-only padding comments and `fn foo`
    // prevent the comments from being absorbed as `foo`'s leading
    // metadata — `foo`'s item bytes stay purely CRLF (3 CRLF, 0 LF),
    // while the whole-file count is dominated by LF surroundings.
    let pad = "// pad 1\n// pad 2\n// pad 3\n// pad 4\n// pad 5\n// pad 6\n// pad 7\n// pad 8\n";
    let base = format!("fn bar() {{}}\n\n{pad}\n");
    let ours = format!("fn bar() {{}}\n\n{pad}\nfn foo() {{\r\n    1\r\n}}\r\n");
    let theirs = format!("fn bar() {{}}\n\n{pad}\nfn foo() {{\r\n    2\r\n}}\r\n");
    let base = base.as_str();
    let ours = ours.as_str();
    let theirs = theirs.as_str();
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert_eq!(count, 1, "expected 1 conflict, got {count}: {text:?}");
    for line in text.split_inclusive('\n') {
        if line.starts_with("<<<<<<<") || line.starts_with("=======") || line.starts_with(">>>>>>>")
        {
            assert!(
                line.ends_with("\r\n"),
                "marker line `{}` must use CRLF to match the CRLF item bodies it wraps, \
                 even though the surrounding file is majority-LF: {text:?}",
                line.trim_end_matches('\n').trim_end_matches('\r'),
            );
        }
    }
}

// =====================================================================
// Self-audit prediction P1 (heddle#114 r7): r6's reconcile_trailing_newline
// fix made the POP case CRLF-aware (popping `\r\n` as a unit). The
// inverse path — when majority votes "yes trailing newline" and output
// lacks one — still hardcodes `b'\n'`, so a CRLF-style file whose
// reconstructed bytes happen to end without a newline gets a bare LF
// appended. Same hazard class as the r6 P2 #1 markers finding: any
// place that emits a newline must respect the file's existing EOL.
// =====================================================================
#[test]
fn reconcile_trailing_newline_add_case_uses_crlf_when_file_is_crlf() {
    // base and theirs both end with CRLF (both vote "yes trailing
    // newline", so majority = yes). ours's modification appends a new
    // function without a final newline, and the reconstruction
    // pipeline ends the output at ours's last item — no trailing
    // newline. reconcile_trailing_newline pushes one back; pre-fix
    // it pushes `\n`, post-fix it pushes `\r\n` to match the file's
    // existing CRLF style.
    let base = "fn foo() {}\r\n";
    let ours = "fn foo() {}\r\nfn bar() {}";
    let theirs = "fn foo() { 1 }\r\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(
        merged.ends_with("\r\n"),
        "merged output must end with CRLF when the file is CRLF (ADD case): {merged:?}"
    );
    // Defence in depth: no bare LF anywhere in the output.
    let bytes = merged.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            assert!(
                i > 0 && bytes[i - 1] == b'\r',
                "bare LF at byte {i} in otherwise-CRLF output: {merged:?}"
            );
        }
    }
}

// =====================================================================
// Self-audit prediction P2 (heddle#114 r7): r6's signature_hash walks
// each parameter's `type` field text. In tree-sitter-typescript,
// `required_parameter` and `optional_parameter` are different node
// kinds — `foo(x: number)` vs `foo(x?: number)` — but they share the
// same `type` field (a type_annotation around "number"). Without
// hashing the parameter NODE KIND, the two overload signatures
// collapse into one ItemKey.
//
// Same hazard class as the explicit C/C++ declarator-shape finding
// (Codex r6 P1 #2): the `type` field doesn't carry the full identity,
// so distinct overloads share a slot and the per-side occurrence
// indexer mis-pairs them when one side adds the second overload
// ahead of the first.
// =====================================================================
#[test]
fn typescript_optional_parameter_distinct_from_required_parameter() {
    // base has `foo(x: number)`. ours adds the optional-parameter
    // overload `foo(x?: number)` ABOVE it. theirs edits the body of
    // `foo(x: number)`. Disjoint edits — clean merge expected.
    //
    // Pre-fix both `foo` keys collapse on (Function, "foo", [], sig)
    // because both params have type field "number". Per-side
    // occurrences misalign at slot (foo,0): base/theirs hold the
    // required overload but ours holds the optional overload.
    let base = "\
function foo(x: number): number {
  return x + 0;
}
";
    let ours = "\
function foo(x?: number): number {
  return (x ?? 0) + 1;
}
function foo(x: number): number {
  return x + 0;
}
";
    let theirs = "\
function foo(x: number): number {
  return x + 999;
}
";
    let outcome = merge_at(base, ours, theirs, "f.ts");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    // theirs's edit on the required overload must land.
    assert!(
        text.contains("return x + 999"),
        "theirs's edit on foo(x: number) must survive: {text}"
    );
    // ours's added optional overload must land with its body intact.
    assert!(
        text.contains("return (x ?? 0) + 1"),
        "ours's added foo(x?: number) body must survive: {text}"
    );
    // Each overload appears exactly once.
    let required_count = text.matches("foo(x: number)").count();
    let optional_count = text.matches("foo(x?: number)").count();
    assert_eq!(
        required_count, 1,
        "foo(x: number) must appear exactly once, got {required_count}: {text}"
    );
    assert_eq!(
        optional_count, 1,
        "foo(x?: number) must appear exactly once, got {optional_count}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "disjoint overload addition + body edit must merge cleanly: {text}"
    );
}

// =====================================================================
// Codex r8 P1 (cid 3256283864): `c_function_scope` only fires for
// OUT-OF-CLASS definitions (`void A::foo()` where the declarator has
// a qualified_identifier). Inline methods inside `class A { void foo()
// { ... } }` have an unqualified declarator, so two classes with
// same-signature inline methods both produce ItemKey
// (Function, "foo", [], _) and collapse to one slot. When one side
// adds a new class with a same-named inline method, the per-side
// occurrence indexer mis-pairs unrelated functions across sides.
// =====================================================================
#[test]
fn cpp_inline_same_named_methods_in_different_classes_keep_distinct_identities() {
    // base has only `class A` with inline `foo`. ours adds a new
    // `class B` with its own inline `foo` BEFORE A. theirs edits A's
    // inline foo body. Disjoint changes — clean merge expected.
    //
    // Pre-fix every inline `foo` keys as (Function, "foo", [], sig).
    // Per-side occurrences:
    //   base    [A's foo=0]
    //   ours    [B's foo=0, A's foo=1]
    //   theirs  [A's foo=0]
    // MatchKey (foo,0) pairs base's A::foo with ours's B::foo and
    // theirs's A::foo — three unrelated bodies, so theirs's A::foo
    // edit gets overwritten by ours's B::foo bytes at A::foo's slot
    // and B::foo never lands at its own (foo,1) slot cleanly.
    //
    // Post-fix `class_specifier` walks up the scope, so A's foo gets
    // scope=["A"] and B's foo gets scope=["B"]. Their ItemKeys are
    // distinct; theirs's edit on A merges cleanly and B's add inserts.
    let base = "\
class A {
    void foo() { int a = 0; (void)a; }
};
";
    let ours = "\
class B {
    void foo() { int b = 99; (void)b; }
};
class A {
    void foo() { int a = 0; (void)a; }
};
";
    let theirs = "\
class A {
    void foo() { int a = 2; (void)a; }
};
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    eprintln!("DEBUG merge result:\n{text}");
    // theirs's edit on A's foo must land — pre-fix it's lost because
    // (foo,0) takes ours's B::foo bytes in place of A::foo.
    assert!(
        text.contains("int a = 2"),
        "theirs's A's foo edit must survive: {text}"
    );
    // ours's added B::foo body must land verbatim.
    assert!(
        text.contains("int b = 99"),
        "ours's added B::foo body must survive: {text}"
    );
    // Each body appears exactly once — no duplication, no
    // cross-contamination via misaligned occurrence indexing.
    let a_body_count = text.matches("int a = 2").count();
    let b_body_count = text.matches("int b = 99").count();
    assert_eq!(
        a_body_count, 1,
        "A's foo body must appear exactly once, got {a_body_count}: {text}"
    );
    assert_eq!(
        b_body_count, 1,
        "B's foo body must appear exactly once, got {b_body_count}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "disjoint cross-class inline edits + addition must merge cleanly: {text}"
    );
}

// =====================================================================
// Codex r8 P2 (cid 3256283859): `signature_hash_from_parameter_list`
// hashes only parameter types + declarator shapes — trailing function
// qualifiers (`const`, `volatile`, `&`, `&&`, `noexcept`) live as
// CHILDREN of the outer `function_declarator`, not inside the
// `parameter_list`, so `void foo()` and `void foo() const` produce the
// same signature_hash and collapse to one ItemKey. C++ member
// overloads on cv- or ref-qualifier alone are then indistinguishable
// from the merger's point of view.
// =====================================================================
#[test]
fn cpp_const_qualified_overload_distinct_from_unqualified() {
    // base has `int foo()`. ours adds the const-qualified overload
    // `int foo() const` ABOVE it. theirs edits the body of the
    // unqualified overload. Disjoint changes — clean merge expected.
    //
    // Pre-fix both `foo` keys collapse on (Function, "foo", ["A"], sig)
    // because the signature hash ignores the `const` qualifier. Per-side
    // occurrences misalign at slot (foo,0): base/theirs hold the
    // unqualified overload; ours holds the const overload.
    let base = "\
class A {
    int foo() { return 0; }
};
";
    let ours = "\
class A {
    int foo() const { return 1; }
    int foo() { return 0; }
};
";
    let theirs = "\
class A {
    int foo() { return 99; }
};
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    // theirs's edit on the unqualified overload must land.
    assert!(
        text.contains("return 99"),
        "theirs's edit on int foo() must survive: {text}"
    );
    // ours's added const overload body must land verbatim.
    assert!(
        text.contains("return 1"),
        "ours's added int foo() const body must survive: {text}"
    );
    // Each overload appears exactly once.
    let const_count = text.matches("int foo() const").count();
    let unqual_count = text.matches("int foo()").count() - const_count;
    assert_eq!(
        const_count, 1,
        "int foo() const must appear exactly once, got {const_count}: {text}"
    );
    assert_eq!(
        unqual_count, 1,
        "int foo() (unqualified) must appear exactly once, got {unqual_count}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "disjoint cv-qualifier overload addition + body edit must merge cleanly: {text}"
    );
}

// =====================================================================
// Codex r8 P2 (cid 3256283862, was heddle#130): `classify_js_node`
// recognizes `method_definition` (class methods with bodies) but not
// `method_signature` (TS interface methods) or `abstract_method_signature`
// (TS abstract class methods). Interfaces and abstract classes
// therefore extract ZERO items and the whole interface body falls
// through to text-merge — a method-level reorder collides with any
// other body edit instead of resolving as per-method moves.
// =====================================================================
#[test]
fn typescript_interface_method_reorder_merges_cleanly() {
    // base declares an interface with many methods. ours fully
    // reverses their order; theirs edits the first method's signature
    // AND the last method's signature (now at distant lines in
    // ours). Pre-fix the interface body extracts ZERO items so it
    // routes through text-merge; theirs's edits to both endpoints
    // overlap with ours's rewrite of those same line ranges and
    // produce a whole-block conflict. Post-fix each method is its
    // own item keyed by name + parameter signature, so the reorder
    // splices independently of each per-method signature edit and
    // the merge resolves cleanly.
    let base = "\
interface Foo {
  a(): void;
  b(): void;
  c(): void;
  d(): void;
  e(): void;
  f(): void;
}
";
    let ours = "\
interface Foo {
  f(): void;
  e(): void;
  d(): void;
  c(): void;
  b(): void;
  a(): void;
}
";
    let theirs = "\
interface Foo {
  a(x: number): void;
  b(): void;
  c(): void;
  d(): void;
  e(): void;
  f(y: string): void;
}
";
    // ours reverses order; theirs edits the first AND last signatures. The
    // signature edit splits each endpoint into a delete (base/ours `a()`) + an
    // add (theirs `a(x: number)`), and TS `;` statement terminators are
    // separate tokens living in the inter-item gaps — so this reorder cannot be
    // reconstructed without re-deriving terminator placement. Pre-#490 the
    // semantic path emitted a silently-UNPARSEABLE "clean" result (stray `;`,
    // doubled `}`); the heddle#490 conservation guard now re-parses every clean
    // merge and routes this to the safe textual conflict instead. Either way
    // the methods are still extracted as items (theirs's edits survive, nothing
    // is dropped) and NO silent corruption escapes.
    match merge_at(base, ours, theirs, "f.ts") {
        MergeOutcome::Clean(b) => {
            let text = String::from_utf8(b).unwrap();
            assert!(
                crate::parser::ParsedFile::parse(
                    text.as_str(),
                    crate::parser::Language::TypeScript
                )
                .is_some(),
                "clean merge must re-parse (no silent corruption): {text}"
            );
            assert!(
                text.contains("a(x: number)"),
                "theirs's edit on a lost: {text}"
            );
            assert!(
                text.contains("f(y: string)"),
                "theirs's edit on f lost: {text}"
            );
            for m in ["a(", "b(", "c(", "d(", "e(", "f("] {
                assert_eq!(text.matches(m).count(), 1, "{m} not once: {text}");
            }
        }
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => {
            let text = String::from_utf8(merged_bytes_with_markers).unwrap();
            assert!(
                text.contains("a(x: number)"),
                "theirs's edit on a lost: {text}"
            );
            assert!(
                text.contains("f(y: string)"),
                "theirs's edit on f lost: {text}"
            );
            for m in ["a(", "b(", "c(", "d(", "e(", "f("] {
                assert!(text.contains(m), "{m} silently dropped: {text}");
            }
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// =====================================================================
// Codex r8 P2 follow-up: `abstract_method_signature` is the kind for
// methods declared `abstract` inside an `abstract class`. Same shape
// as `method_signature` — no body — so it must classify identically
// or abstract classes regress into whole-class text-merge fallbacks.
// =====================================================================
#[test]
fn typescript_abstract_class_method_reorder_merges_cleanly() {
    let base = "\
abstract class Foo {
  abstract a(): void;
  abstract b(): void;
  abstract c(): void;
  abstract d(): void;
  abstract e(): void;
  abstract f(): void;
}
";
    let ours = "\
abstract class Foo {
  abstract f(): void;
  abstract e(): void;
  abstract d(): void;
  abstract c(): void;
  abstract b(): void;
  abstract a(): void;
}
";
    let theirs = "\
abstract class Foo {
  abstract a(x: number): void;
  abstract b(): void;
  abstract c(): void;
  abstract d(): void;
  abstract e(): void;
  abstract f(y: string): void;
}
";
    // Same shape as the interface reorder test: the signature edit splits each
    // endpoint into delete+add and TS `;` terminators are separate gap tokens,
    // so this reorder can't reconstruct cleanly. Pre-#490 it emitted a silently
    // unparseable result; the heddle#490 guard now re-parses clean merges and
    // routes this to a safe conflict. The abstract methods are still extracted
    // as items (theirs's edits survive, nothing dropped) — the Codex r8 P2
    // classification this test was added for still holds.
    let methods = [
        "abstract a(",
        "abstract b(",
        "abstract c(",
        "abstract d(",
        "abstract e(",
        "abstract f(",
    ];
    match merge_at(base, ours, theirs, "f.ts") {
        MergeOutcome::Clean(b) => {
            let text = String::from_utf8(b).unwrap();
            assert!(
                crate::parser::ParsedFile::parse(
                    text.as_str(),
                    crate::parser::Language::TypeScript
                )
                .is_some(),
                "clean merge must re-parse (no silent corruption): {text}"
            );
            assert!(
                text.contains("abstract a(x: number)"),
                "abstract a edit lost: {text}"
            );
            assert!(
                text.contains("abstract f(y: string)"),
                "abstract f edit lost: {text}"
            );
            for m in methods {
                assert_eq!(text.matches(m).count(), 1, "{m} not once: {text}");
            }
        }
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => {
            let text = String::from_utf8(merged_bytes_with_markers).unwrap();
            assert!(
                text.contains("abstract a(x: number)"),
                "abstract a edit lost: {text}"
            );
            assert!(
                text.contains("abstract f(y: string)"),
                "abstract f edit lost: {text}"
            );
            for m in methods {
                assert!(text.contains(m), "{m} silently dropped: {text}");
            }
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// =====================================================================
// Codex r7 P2 (cid 3256225712): `detect_eol` returns CRLF as soon as
// ANY sample contains `\r\n`. A single CRLF side then forces `\r\n`
// onto a merge whose base + other side are LF — wrong for the
// majority style of the file. Should be majority-of-occurrences with
// base style as the tie-break.
// =====================================================================
#[test]
fn detect_eol_uses_majority_when_two_of_three_inputs_are_lf() {
    // base and theirs are LF (one bare `\n` each); ours uses CRLF
    // internally but doesn't end with a newline, forcing the ADD
    // branch of reconcile_trailing_newline. With the any-CRLF rule
    // detect_eol picks CRLF and the merged file ends `\r\n`; with
    // the majority rule it picks LF (2 LF samples vs 1 CRLF sample).
    let base = "fn foo() {}\n";
    let ours = "fn foo() {}\r\nfn bar() {}";
    let theirs = "fn foo() { 1 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(
        merged.ends_with('\n') && !merged.ends_with("\r\n"),
        "merged output must end with LF (majority of inputs are LF): {merged:?}"
    );
}

// =====================================================================
// Codex r8 P2 (cid 3256283857): `emit_addadd_conflict` calls
// `detect_eol` over only the two item byte ranges. Single-line item
// bodies carry zero `\n` observations, so the majority vote is 0-0 and
// the LF fallback wins — even when the surrounding file is wholly
// CRLF. Whole-file context belongs in the sample set so the marker
// EOL matches the file's actual style.
// =====================================================================
#[test]
fn addadd_conflict_markers_use_crlf_for_single_line_items_in_crlf_file() {
    // base is a CRLF file with an existing comment header; both sides
    // independently add the same-named function with single-line
    // bodies that differ. Each added item's bytes contain ZERO `\n`
    // characters (the bodies are one line), so without whole-file
    // context `detect_eol` sees [0 CRLF, 0 LF] across the two item
    // samples and falls back to LF. The body bytes themselves carry
    // no newline, so the only EOL emitted into the conflict block
    // comes from the marker lines — and a CRLF file gaining bare-LF
    // marker lines breaks Windows tooling, the same hazard the r6
    // P2 #1 fix addressed for multi-line items.
    // Header comment is separated from the added function by a blank
    // line on each side so `leading_metadata_start` does NOT absorb
    // the comment into the function's item bytes — otherwise the
    // absorbed `// header\r\n` would inject CRLF observations into the
    // sample set and mask the bug we're testing.
    let base = "// header\r\n\r\n";
    let ours = "// header\r\n\r\nfn foo() { 1 }\r\n";
    let theirs = "// header\r\n\r\nfn foo() { 2 }\r\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert_eq!(count, 1, "expected 1 conflict, got {count}: {text:?}");
    for line in text.split_inclusive('\n') {
        if line.starts_with("<<<<<<<") || line.starts_with("=======") || line.starts_with(">>>>>>>")
        {
            assert!(
                line.ends_with("\r\n"),
                "marker line `{}` must end with CRLF in a CRLF file: {text:?}",
                line.trim_end_matches('\n').trim_end_matches('\r'),
            );
        }
    }
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            assert!(
                i > 0 && bytes[i - 1] == b'\r',
                "bare LF at byte {i} in otherwise-CRLF output: {text:?}"
            );
        }
    }
}

// =====================================================================
// Codex r9 P1 (cid 3256397416): C++ does NOT allow overloading by
// exception specification. `void foo()` and `void foo() noexcept` are
// REDECLARATIONS of the same function — not different overloads. The
// r8 P2 fix folded `noexcept` into `c_signature_hash` alongside cv-
// and ref-qualifiers, but unlike those, `noexcept` is metadata: it
// must NOT change ItemKey identity. Including it splits a logical
// function across sides whenever `noexcept` is added or removed,
// degrading the resolution to delete + add and losing disjoint body
// edits on the other side.
// =====================================================================
#[test]
fn cpp_noexcept_addition_does_not_split_function_identity() {
    // base has plain `void foo() { ... }`. ours adds `noexcept` to the
    // signature line. theirs edits the body. Disjoint changes — clean
    // merge expected.
    //
    // Pre-fix `noexcept` is in c_signature_hash, so ours's foo and
    // base/theirs's foo get different ItemKeys. ours is treated as
    // delete + add of foo; theirs's body modify on the old key races
    // the delete and either conflicts or is overwritten.
    //
    // Post-fix `noexcept` is dropped from the hash, all three sides
    // share an ItemKey, and the function-body 3-way merge picks up
    // both edits (signature line and body line) cleanly.
    let base = "\
void foo() {
    int a = 0;
    (void)a;
}
";
    let ours = "\
void foo() noexcept {
    int a = 0;
    (void)a;
}
";
    let theirs = "\
void foo() {
    int a = 99;
    (void)a;
}
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    assert!(
        text.contains("noexcept"),
        "ours's noexcept addition must survive: {text}"
    );
    assert!(
        text.contains("int a = 99"),
        "theirs's body edit must survive: {text}"
    );
    // foo definition must appear exactly once — pre-fix it can
    // duplicate (ours's add + theirs's modify) or vanish (ours's
    // delete wins).
    let foo_signature_count =
        text.matches("foo() noexcept").count() + text.matches("foo() {").count();
    assert_eq!(
        foo_signature_count, 1,
        "foo definition must appear exactly once across the noexcept addition: got {foo_signature_count}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "noexcept addition + disjoint body edit must merge cleanly: {text}"
    );
}

// =====================================================================
// Codex r9 P2 (cid 3256397421): a conditional noexcept clause
// (`noexcept(noexcept(expr))`) hashed verbatim picks up parameter
// names appearing in `expr`. A pure parameter rename then changes the
// `c_signature_hash` text, splitting identity. The r9 P1 fix removes
// `noexcept` from `c_signature_hash`, so the noexcept text isn't
// hashed at all — this test locks in that invariant against future
// regressions that try to re-fold any form of `noexcept` back in.
// =====================================================================
#[test]
fn cpp_noexcept_clause_with_param_name_survives_pure_rename() {
    // base declares `f(S x) noexcept(noexcept(x.bar()))` with a
    // multi-line body. ours renames the parameter `x` -> `y`,
    // including inside the noexcept clause. theirs edits a body
    // line. Disjoint changes — clean merge expected.
    //
    // Pre-P1-fix: the noexcept clause text changes when x -> y, so
    // c_signature_hash sees different bytes, ours and base/theirs
    // get different ItemKeys, and the rename + body edit collide.
    //
    // Post-P1-fix: noexcept isn't part of the hash, identity holds
    // across the rename, and the body merge picks up both edits.
    let base = "\
struct S { void bar() {} };
void f(S x) noexcept(noexcept(x.bar())) {
    int a = 0;
    (void)a;
}
";
    let ours = "\
struct S { void bar() {} };
void f(S y) noexcept(noexcept(y.bar())) {
    int a = 0;
    (void)a;
}
";
    let theirs = "\
struct S { void bar() {} };
void f(S x) noexcept(noexcept(x.bar())) {
    int a = 99;
    (void)a;
}
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    assert!(
        text.contains("noexcept(y.bar())"),
        "ours's rename inside the noexcept clause must survive: {text}"
    );
    assert!(
        text.contains("int a = 99"),
        "theirs's body edit must survive: {text}"
    );
    // The function definition must appear exactly once.
    let f_count = text.matches("void f(S ").count();
    assert_eq!(
        f_count, 1,
        "f must appear exactly once across the rename + body edit: got {f_count}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "param rename inside noexcept clause + disjoint body edit must merge cleanly: {text}"
    );
}

// =====================================================================
// Codex r9 P2 (cid 3256397418): inline C++ methods inherit container
// scope from `class_specifier.name` (`A`), while out-of-class
// definitions extract scope text via `c_function_scope` from the
// declarator's qualified prefix (`A<T>`). A templated method then
// keys at scope=["A"] inline but scope=["A<T>"] out-of-class, so a
// refactor that moves the method between the two forms looks like
// delete + add to the merger and disjoint edits on the other side
// surface as conflicts.
// =====================================================================
#[test]
fn cpp_template_method_refactor_inline_to_out_of_class_merges_cleanly() {
    // base has `template<class T> class A { void foo() {...} };`.
    // ours refactors foo to an out-of-class definition (declaration
    // stays inside the class body). theirs edits foo's body inline.
    // Disjoint changes — clean merge expected.
    //
    // Pre-fix: ours's out-of-class foo gets scope=["A<T>"] while
    // base/theirs's inline foo gets scope=["A"]. Different ItemKeys
    // → ours appears as delete + add; theirs's modify on the old key
    // collides with the delete.
    //
    // Post-fix: `c_function_scope` strips template-argument lists so
    // ["A<T>"] normalizes to ["A"]. All three sides share an
    // ItemKey across the refactor, and the body 3-way merge picks up
    // both ours's signature change and theirs's body edit.
    // Body indentation is deliberately uniform across the inline and
    // out-of-class forms (4 spaces, not the more idiomatic 8 spaces
    // inside a class). The function_definition item bytes cover both
    // signature and body — if the body indents differ between forms,
    // every body line counts as a change in the 3-way text merge of
    // item bytes and the disjoint-edit invariant breaks for reasons
    // unrelated to the scope-normalization fix under test.
    let base = "\
template<class T> class A {
void foo() {
    int a = 0;
    (void)a;
}
};
";
    let ours = "\
template<class T> class A {
void foo();
};
template<class T> void A<T>::foo() {
    int a = 0;
    (void)a;
}
";
    let theirs = "\
template<class T> class A {
void foo() {
    int a = 99;
    (void)a;
}
};
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    assert!(
        text.contains("void A<T>::foo()"),
        "ours's out-of-class signature must survive: {text}"
    );
    assert!(
        text.contains("int a = 99"),
        "theirs's body edit must survive: {text}"
    );
    // foo's body must appear exactly once — pre-fix it can duplicate
    // (ours's add + theirs's modify both retained) or be dropped
    // entirely if ours's delete races theirs's modify.
    let foo_body_count = text.matches("int a = 99").count();
    assert_eq!(
        foo_body_count, 1,
        "foo body must appear exactly once after the refactor: got {foo_body_count}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "inline-to-out-of-class refactor + disjoint body edit must merge cleanly: {text}"
    );
}

// =====================================================================
// Codex r10 P2 (cid 3256487042): r9's `c_function_scope` strip_template_args
// is applied to EVERY scope component, which collapses explicit class
// template specializations (`A<int>`, `A<float>`) onto the same scope
// key `"A"`. When a file holds multiple specializations of the same
// primary template, a reorder/add on one side shifts the per-side
// occurrence indexer relative to base/theirs, so the merger ends up
// pairing edits across UNRELATED specializations — corrupting the
// file or producing spurious conflicts. The fix retains specialization
// arguments while still collapsing parameter-list usages (`A<T>` in a
// templated out-of-class def vs inline `A`), keyed on whether the
// function definition is wrapped in a `template_declaration` whose
// parameter list is non-empty.
// =====================================================================
#[test]
fn cpp_explicit_specializations_keep_distinct_scopes_under_reorder() {
    // base has out-of-class defs of A<int>::foo and A<float>::foo
    // (explicit specializations of a class template). ours INSERTS a
    // new A<char>::foo at the start AND edits A<float>::foo. theirs
    // edits A<int>::foo. The two sides touch disjoint specializations
    // — clean merge expected.
    //
    // Pre-fix: r9's strip_template_args collapses every scope to
    // `["A"]`. With ours inserting a method, the per-side occurrence
    // labels diverge: base has (A,foo,0)=int / (A,foo,1)=float, ours
    // has (A,foo,0)=char / (A,foo,1)=int / (A,foo,2)=float, theirs
    // has (A,foo,0)=int / (A,foo,1)=float. resolve_item pairs
    // (A,foo,1): base=float, ours=int(unedited), theirs=float — wrong
    // pairing forces theirs's no-op on float to merge against ours's
    // int. ours's edit on float lands at (A,foo,2) where base has
    // nothing → looks like a fresh add of the OLD content.
    //
    // Post-fix: scope retains the specialization argument because
    // the function definitions are NOT wrapped in a
    // template_declaration with non-empty parameters. Distinct keys
    // (foo,["A<int>"]), (foo,["A<float>"]), (foo,["A<char>"]) →
    // each specialization merges independently.
    let base = "\
void A<int>::foo() {
    int x = 0;
    (void)x;
}

void A<float>::foo() {
    int y = 0;
    (void)y;
}
";
    let ours = "\
void A<char>::foo() {
    int z = 0;
    (void)z;
}

void A<int>::foo() {
    int x = 0;
    (void)x;
}

void A<float>::foo() {
    int y = 0;
    (void)y;
    int yy = y;
    (void)yy;
}
";
    let theirs = "\
void A<int>::foo() {
    int x = 0;
    (void)x;
    int xx = x;
    (void)xx;
}

void A<float>::foo() {
    int y = 0;
    (void)y;
}
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    // Each specialization's body must contain ONLY its own edits — no
    // cross-contamination from collapsed-scope occurrence-pairing.
    // Slice the merged text by specialization header and assert each
    // slice's content.
    fn body_for(text: &str, header: &str) -> String {
        let start = text.find(header).expect("expected header in merged output");
        let after = &text[start..];
        // Body ends at the matching closing brace at column 0 (next "}").
        let close = after.find("\n}\n").expect("expected close brace");
        after[..close + 3].to_string()
    }
    let char_body = body_for(&text, "void A<char>::foo()");
    let int_body = body_for(&text, "void A<int>::foo()");
    let float_body = body_for(&text, "void A<float>::foo()");

    // A<char>::foo is ours-only — must contain only ours's body.
    assert!(
        char_body.contains("int z = 0"),
        "A<char>::foo must keep its own body: {char_body}"
    );
    assert!(
        !char_body.contains("int xx = x"),
        "theirs's edit on A<int>::foo must NOT leak into A<char>::foo: {char_body}"
    );
    assert!(
        !char_body.contains("int yy = y"),
        "ours's edit on A<float>::foo must NOT leak into A<char>::foo: {char_body}"
    );
    // A<int>::foo is theirs-edited — must contain theirs's xx edit and
    // base's x body.
    assert!(
        int_body.contains("int x = 0"),
        "A<int>::foo must keep base's body: {int_body}"
    );
    assert!(
        int_body.contains("int xx = x"),
        "theirs's edit on A<int>::foo must survive: {int_body}"
    );
    assert!(
        !int_body.contains("int z = 0"),
        "A<char>::foo body must NOT leak into A<int>::foo: {int_body}"
    );
    // A<float>::foo is ours-edited — must contain ours's yy edit.
    assert!(
        float_body.contains("int y = 0"),
        "A<float>::foo must keep base's body: {float_body}"
    );
    assert!(
        float_body.contains("int yy = y"),
        "ours's edit on A<float>::foo must survive: {float_body}"
    );
    // Each specialization appears exactly once — no duplication from
    // re-emission via misaligned occurrence indexes.
    let int_count = text.matches("A<int>::foo()").count();
    let float_count = text.matches("A<float>::foo()").count();
    let char_count = text.matches("A<char>::foo()").count();
    assert_eq!(
        int_count, 1,
        "A<int>::foo must appear exactly once: got {int_count}: {text}"
    );
    assert_eq!(
        float_count, 1,
        "A<float>::foo must appear exactly once: got {float_count}: {text}"
    );
    assert_eq!(
        char_count, 1,
        "A<char>::foo must appear exactly once: got {char_count}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "disjoint edits on distinct specializations must merge cleanly: {text}"
    );
}

// =====================================================================
// Codex r10 P2 (cid 3256487049): `c_signature_hash` uses
// `find_descendant(declarator, ["parameter_list"])` which DFS-finds
// the FIRST parameter_list anywhere under the function_declarator —
// not necessarily the function's own. When the function's qualified
// scope contains a `template_type` whose argument is itself a
// function-pointer type (e.g. `A<int(*)(double)>::foo`), the
// abstract_function_declarator inside the scope's template argument
// has its own parameter_list — and DFS reaches THAT one before the
// outer function_declarator's `parameters` field. All overloads of
// `foo` on `A<int(*)(double)>` then hash the same `(double)`
// parameter list and collapse onto identical signature_hashes, so
// distinct overloads share an ItemKey and the merger cross-pairs
// their bodies.
// =====================================================================
#[test]
fn cpp_overloads_with_function_pointer_in_scope_template_arg_stay_distinct() {
    // Two overloads of `foo` on a class template specialization whose
    // type argument is a function-pointer (`int(*)(double)`). The
    // overloads differ in their outer parameter (int vs char), so they
    // are distinct overloads and must merge independently.
    //
    // Pre-fix c_signature_hash returns the FIRST parameter_list in
    // DFS order from the function_declarator. The qualified scope's
    // template argument carries an abstract_function_declarator whose
    // parameter_list `(double)` is visited before the outer
    // function_declarator's `(int x)` / `(char y)`. Both overloads
    // hash the SAME `(double)` parameter list, ItemKeys collide, and
    // base's int-overload pairs with theirs's int-overload edit
    // correctly — but theirs's char-overload edit and ours's
    // int-overload edit cross-pair, leaking ours's body into the
    // char overload (or vice versa) when both overloads are touched.
    //
    // Post-fix c_signature_hash walks down to the function_declarator
    // carrying the actual name and uses ITS `parameters` field, so
    // outer `(int x)` and `(char y)` hash differently and the
    // overloads stay distinct.
    let base = "\
void A<int(*)(double)>::foo(int x) {
    int a = 0;
    (void)a;
    (void)x;
}

void A<int(*)(double)>::foo(char y) {
    int b = 0;
    (void)b;
    (void)y;
}
";
    // ours INSERTS a third overload `foo(short)` at the start AND
    // edits `foo(char)`. The insertion shifts the per-side
    // occurrence labels: ours has 3 occurrences vs base/theirs's 2.
    // With overloads colliding on signature_hash (the pre-fix bug),
    // (foo, scope, 1) pairs base=char / ours=int(unchanged) /
    // theirs=char(edited) — wrong pairing that pulls theirs's
    // `bb = b` edit onto the int overload's slot.
    let ours = "\
void A<int(*)(double)>::foo(short s) {
    int c = 0;
    (void)c;
    (void)s;
}

void A<int(*)(double)>::foo(int x) {
    int a = 0;
    (void)a;
    (void)x;
}

void A<int(*)(double)>::foo(char y) {
    int b = 0;
    (void)b;
    (void)y;
    int bb = b;
    (void)bb;
}
";
    let theirs = "\
void A<int(*)(double)>::foo(int x) {
    int a = 0;
    (void)a;
    (void)x;
    int aa = a;
    (void)aa;
}

void A<int(*)(double)>::foo(char y) {
    int b = 0;
    (void)b;
    (void)y;
}
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    // Slice the merged text by overload header and assert each
    // slice's content. The int overload must hold ours's `aa` edit;
    // the char overload must hold theirs's `bb` edit; neither may
    // leak the other's body.
    fn body_for(text: &str, header: &str) -> String {
        let start = text.find(header).expect("expected header in merged output");
        let after = &text[start..];
        let close = after.find("\n}\n").expect("expected close brace");
        after[..close + 3].to_string()
    }
    let short_body = body_for(&text, "void A<int(*)(double)>::foo(short s)");
    let int_body = body_for(&text, "void A<int(*)(double)>::foo(int x)");
    let char_body = body_for(&text, "void A<int(*)(double)>::foo(char y)");
    // The inserted short overload is ours-only and must keep ours's
    // body verbatim.
    assert!(
        short_body.contains("int c = 0"),
        "ours's inserted foo(short) must keep its own body: {short_body}"
    );
    assert!(
        !short_body.contains("int aa = a"),
        "theirs's edit on foo(int) must NOT leak into foo(short): {short_body}"
    );
    assert!(
        !short_body.contains("int bb = b"),
        "ours's edit on foo(char) must NOT leak into foo(short): {short_body}"
    );
    // theirs's edit on the int overload must land there only.
    assert!(
        int_body.contains("int aa = a"),
        "theirs's edit on foo(int) must survive: {int_body}"
    );
    assert!(
        !int_body.contains("int bb = b"),
        "ours's edit on foo(char) must NOT leak into foo(int): {int_body}"
    );
    // ours's edit on the char overload must land there only.
    assert!(
        char_body.contains("int bb = b"),
        "ours's edit on foo(char) must survive: {char_body}"
    );
    assert!(
        !char_body.contains("int aa = a"),
        "theirs's edit on foo(int) must NOT leak into foo(char): {char_body}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "disjoint overload edits must merge cleanly: {text}"
    );
}

// =====================================================================
// Codex r11 P1 #3 (cid 3256623807): r10's `c_function_scope` strips
// scope template-argument lists whenever the function definition is
// wrapped in a `template_declaration` with a non-empty parameter list.
// That gate fires for BOTH primary-template parameter usages
// (`template<class T> void Foo<T>::bar()` — the `<T>` references the
// enclosing template's parameter; strip to match inline `Foo`) AND
// partial-specialization arguments (`template<class T> void A<T*>::foo()`
// — the `<T*>` is the specialization pattern, NOT a parameter usage).
// Stripping the latter collapses distinct partial specializations
// (`A<T*>`, `A<T&>`) onto the same scope `["A"]`, so when one side adds
// or reorders a partial specialization, the per-side occurrence
// indexer mis-pairs unrelated methods across sides.
// =====================================================================
#[test]
fn cpp_partial_specializations_keep_distinct_scopes_under_reorder() {
    // base has out-of-class defs of A<T*>::foo and A<T&>::foo (partial
    // specializations of a class template). ours INSERTS a new
    // A<T**>::foo at the start AND edits A<T&>::foo. theirs edits
    // A<T*>::foo. The two sides touch disjoint specializations —
    // clean merge expected.
    //
    // Pre-fix: r10's strip_args fires for every scope inside a
    // non-empty template_declaration, so `A<T*>`, `A<T&>`, `A<T**>`
    // all normalize to `["A"]`. With ours inserting a method, the
    // per-side occurrence labels diverge: base has (A,foo,0)=T* /
    // (A,foo,1)=T&; ours has (A,foo,0)=T** / (A,foo,1)=T* /
    // (A,foo,2)=T&; theirs has (A,foo,0)=T* / (A,foo,1)=T&.
    // resolve_item pairs (A,foo,1) base=T& / ours=T*(unedited) /
    // theirs=T&: wrong pairing forces theirs's no-op on T& to merge
    // against ours's T*; ours's edit on T& lands at (A,foo,2) where
    // base has nothing → looks like a fresh add of the OLD content.
    //
    // Post-fix: c_function_scope only strips template-argument lists
    // when the args match the enclosing template_declaration's
    // parameter list (true primary-template parameter usage). Partial
    // specializations like `A<T*>`, `A<T&>` retain their scope text,
    // so each specialization gets a distinct ItemKey and merges
    // independently.
    let base = "\
template<class T> void A<T*>::foo() {
    int x = 0;
    (void)x;
}

template<class T> void A<T&>::foo() {
    int y = 0;
    (void)y;
}
";
    let ours = "\
template<class T> void A<T**>::foo() {
    int z = 0;
    (void)z;
}

template<class T> void A<T*>::foo() {
    int x = 0;
    (void)x;
}

template<class T> void A<T&>::foo() {
    int y = 0;
    (void)y;
    int yy = y;
    (void)yy;
}
";
    let theirs = "\
template<class T> void A<T*>::foo() {
    int x = 0;
    (void)x;
    int xx = x;
    (void)xx;
}

template<class T> void A<T&>::foo() {
    int y = 0;
    (void)y;
}
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    // Each partial specialization's body must contain only its own
    // edits — no cross-contamination from collapsed-scope
    // occurrence-pairing.
    fn body_for(text: &str, header: &str) -> String {
        let start = text
            .find(header)
            .unwrap_or_else(|| panic!("expected header {header:?} in merged output: {text}"));
        let after = &text[start..];
        let close = after.find("\n}\n").expect("expected close brace");
        after[..close + 3].to_string()
    }
    let pp_body = body_for(&text, "void A<T**>::foo()");
    let p_body = body_for(&text, "void A<T*>::foo()");
    let r_body = body_for(&text, "void A<T&>::foo()");

    // A<T**>::foo is ours-only — must contain only ours's body.
    assert!(
        pp_body.contains("int z = 0"),
        "A<T**>::foo must keep its own body: {pp_body}"
    );
    assert!(
        !pp_body.contains("int xx = x"),
        "theirs's edit on A<T*>::foo must NOT leak into A<T**>::foo: {pp_body}"
    );
    assert!(
        !pp_body.contains("int yy = y"),
        "ours's edit on A<T&>::foo must NOT leak into A<T**>::foo: {pp_body}"
    );
    // A<T*>::foo is theirs-edited — must contain theirs's xx edit.
    assert!(
        p_body.contains("int x = 0"),
        "A<T*>::foo must keep base's body: {p_body}"
    );
    assert!(
        p_body.contains("int xx = x"),
        "theirs's edit on A<T*>::foo must survive: {p_body}"
    );
    assert!(
        !p_body.contains("int z = 0"),
        "A<T**>::foo body must NOT leak into A<T*>::foo: {p_body}"
    );
    // A<T&>::foo is ours-edited — must contain ours's yy edit.
    assert!(
        r_body.contains("int y = 0"),
        "A<T&>::foo must keep base's body: {r_body}"
    );
    assert!(
        r_body.contains("int yy = y"),
        "ours's edit on A<T&>::foo must survive: {r_body}"
    );
    // Each specialization appears exactly once — no duplication from
    // re-emission via misaligned occurrence indexes. The `>` after the
    // partial-spec pattern disambiguates `A<T*>` from `A<T**>` in
    // substring search.
    let pp_count = text.matches("A<T**>::foo()").count();
    let p_count = text.matches("A<T*>::foo()").count();
    let r_count = text.matches("A<T&>::foo()").count();
    assert_eq!(
        pp_count, 1,
        "A<T**>::foo must appear exactly once: got {pp_count}: {text}"
    );
    assert_eq!(
        p_count, 1,
        "A<T*>::foo must appear exactly once: got {p_count}: {text}"
    );
    assert_eq!(
        r_count, 1,
        "A<T&>::foo must appear exactly once: got {r_count}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "disjoint edits on distinct partial specializations must merge cleanly: {text}"
    );
}

// =====================================================================
// r13 self-audit pre-fix A — same hazard class as Codex r12 P2
// (cid 3258861174): the r12 finding correctly noticed that
// `parameter_usage_arg_name` only accepts `type_descriptor` ->
// `type_identifier` arguments, but its specific example
// (`template<int N> void A<N>::foo()`) is a false positive — tree-
// sitter-cpp 0.23 parses the non-type usage `<N>` as
// `type_descriptor` -> `type_identifier "N"`, identical to the type-
// parameter case `<T>`, so the existing matcher already handles it.
//
// Variadic parameter packs (`class... Ts`) are NOT a false positive.
// At the use site, `Ts...` parses as `parameter_pack_expansion`
// wrapping a `type_descriptor`, which the matcher rejects outright —
// so an out-of-class `void A<Ts...>::foo()` keeps scope `["A<Ts...>"]`
// while the inline form `template<class... Ts> class A { void foo() {} };`
// keys at `["A"]`. The inline<->out-of-class refactor then looks like
// delete+add and disjoint edits on the other side surface as
// conflicts or get dropped.
// =====================================================================
#[test]
fn cpp_variadic_template_method_refactor_inline_to_out_of_class_merges_cleanly() {
    // base has inline `template<class... Ts> class A { void foo() {...} };`.
    // ours refactors foo to an out-of-class definition. theirs edits
    // foo's body inline. Disjoint changes — clean merge expected.
    //
    // Pre-fix: ours's out-of-class foo gets scope=["A<Ts...>"] while
    // base/theirs's inline foo gets scope=["A"]. Different ItemKeys
    // -> ours appears as delete + add; theirs's modify on the old key
    // collides with the delete.
    //
    // Post-fix: `parameter_usage_arg_name` recognises
    // `parameter_pack_expansion` whose pattern is a bare
    // `type_descriptor` -> `type_identifier`, returning the pack
    // name. `template_args_match_any_param_list` then matches
    // `<Ts...>` against the enclosing `template<class... Ts>` and
    // `c_function_scope` strips the args, so ["A<Ts...>"] normalises
    // to ["A"] for cross-side identity.
    let base = "\
template<class... Ts> class A {
void foo() {
    int a = 0;
    (void)a;
}
};
";
    let ours = "\
template<class... Ts> class A {
void foo();
};
template<class... Ts> void A<Ts...>::foo() {
    int a = 0;
    (void)a;
}
";
    let theirs = "\
template<class... Ts> class A {
void foo() {
    int a = 77;
    (void)a;
}
};
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    assert!(
        text.contains("void A<Ts...>::foo()"),
        "ours's out-of-class signature must survive: {text}"
    );
    assert!(
        text.contains("int a = 77"),
        "theirs's body edit must survive: {text}"
    );
    let foo_body_count = text.matches("int a = 77").count();
    assert_eq!(
        foo_body_count, 1,
        "foo body must appear exactly once after the refactor: got {foo_body_count}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "variadic inline-to-out-of-class refactor + disjoint body edit must merge cleanly: {text}"
    );
}

// =====================================================================
// r13 self-audit pre-fix B — same hazard class as Codex r12 P2
// (cid 3258861174): template-template parameter usages
// (`template<template<class> class Tmpl>` declared,
// `A<Tmpl>` used) suffer the same inline<->out-of-class scope
// mismatch as variadic packs, but via the OTHER helper:
// `template_param_name` returns None for
// `template_template_parameter_declaration` because its named
// children are `template_parameter_list` + `type_parameter_declaration`,
// neither of which matches the `identifier`/`type_identifier`
// predicate. With param_lists empty, `scope_component_text` skips
// the strip and `A<Tmpl>` is kept verbatim while the inline form
// keys at `A`.
// =====================================================================
#[test]
fn cpp_template_template_param_method_refactor_inline_to_out_of_class_merges_cleanly() {
    // base has inline
    // `template<template<class> class Tmpl> class A { void foo() {...} };`.
    // ours refactors foo to out-of-class. theirs edits foo's body
    // inline. Disjoint changes — clean merge expected.
    //
    // Pre-fix: ours's out-of-class foo gets scope=["A<Tmpl>"] while
    // base/theirs's inline foo gets scope=["A"]. The ItemKeys
    // diverge so ours looks like delete+add and the disjoint edit
    // collides.
    //
    // Post-fix: `template_param_name` recognises
    // `template_template_parameter_declaration` and returns the
    // trailing identifier text (`Tmpl`). With param_lists=[["Tmpl"]],
    // the arg `<Tmpl>` (parsed as `type_descriptor` ->
    // `type_identifier "Tmpl"`) matches and `c_function_scope`
    // strips the args, so ["A<Tmpl>"] normalises to ["A"].
    let base = "\
template<template<class> class Tmpl> class A {
void foo() {
    int a = 0;
    (void)a;
}
};
";
    let ours = "\
template<template<class> class Tmpl> class A {
void foo();
};
template<template<class> class Tmpl> void A<Tmpl>::foo() {
    int a = 0;
    (void)a;
}
";
    let theirs = "\
template<template<class> class Tmpl> class A {
void foo() {
    int a = 55;
    (void)a;
}
};
";
    let outcome = merge_at(base, ours, theirs, "f.cpp");
    let text = match outcome {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => String::from_utf8(merged_bytes_with_markers).unwrap(),
        other => panic!("unexpected: {other:?}"),
    };
    assert!(
        text.contains("void A<Tmpl>::foo()"),
        "ours's out-of-class signature must survive: {text}"
    );
    assert!(
        text.contains("int a = 55"),
        "theirs's body edit must survive: {text}"
    );
    let foo_body_count = text.matches("int a = 55").count();
    assert_eq!(
        foo_body_count, 1,
        "foo body must appear exactly once after the refactor: got {foo_body_count}: {text}"
    );
    assert!(
        !text.contains("<<<<<<<"),
        "template-template inline-to-out-of-class refactor + disjoint body edit must merge cleanly: {text}"
    );
}

// =====================================================================
// heddle#468: additive `use` / `pub use` re-exports are order-insensitive
// items keyed by their import path.
//
// Before this change `use_declaration` fell to the `_ => None` arm in the
// Rust classifier, so re-exports lived in preamble / inter-item segments
// merged by plain `text_hunk_merge`. Keying each `use` by its import path
// routes them through identity-based item resolution: disjoint paths union
// cleanly, while a same-path add/add divergence surfaces a conflict instead
// of silently concatenating both lines into a duplicate import (the AC2
// case below — pre-fix it resolved Clean with `Bar` imported twice).
// =====================================================================

// AC1: two threads each adding a distinct `pub use` at the top of the same
// file auto-combine — no manual resolution. Guards that promoting `use` to
// an item keyed by import path keeps disjoint additions unioning cleanly.
#[test]
fn rust_disjoint_use_additions_auto_combine() {
    let base = "\
pub use crate::existing::Thing;

fn anchor() { 0 }
";
    // ours prepends a distinct re-export; theirs prepends a different one.
    let ours = "\
pub use crate::aaa::Alpha;
pub use crate::existing::Thing;

fn anchor() { 0 }
";
    let theirs = "\
pub use crate::bbb::Beta;
pub use crate::existing::Thing;

fn anchor() { 0 }
";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(
        merged.contains("crate::aaa::Alpha"),
        "ours re-export lost: {merged}"
    );
    assert!(
        merged.contains("crate::bbb::Beta"),
        "theirs re-export lost: {merged}"
    );
    assert!(
        merged.contains("crate::existing::Thing"),
        "base re-export lost: {merged}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "additive disjoint re-exports must merge cleanly: {merged}"
    );
}

// AC1 variant: a plain `use` added on one side and a different one on the
// other, with no shared base `use`, still union cleanly.
#[test]
fn rust_disjoint_use_additions_from_empty_base_combine() {
    let base = "fn anchor() { 0 }\n";
    let ours = "use std::collections::HashMap;\nfn anchor() { 0 }\n";
    let theirs = "use std::fmt::Display;\nfn anchor() { 0 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("HashMap"), "ours use lost: {merged}");
    assert!(merged.contains("Display"), "theirs use lost: {merged}");
    assert!(
        !merged.contains("<<<<<<<"),
        "disjoint use additions must merge cleanly: {merged}"
    );
}

// AC2: same-path add/add of a divergent re-export still conflicts. Both
// sides add `crate::foo::Bar` (same import-path key) but disagree on
// visibility — one re-exports (`pub use`), one imports (`use`). The
// add/add divergence must surface a conflict rather than silently
// picking one or emitting both (a duplicate-name compile error).
#[test]
fn rust_same_path_divergent_use_addadd_conflicts() {
    let base = "fn anchor() { 0 }\n";
    let ours = "pub use crate::foo::Bar;\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::Bar;\nfn anchor() { 0 }\n";
    let (_text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(count >= 1, "expected a conflict on same-path divergence");
}

// Regression: both sides add the SAME `pub use` identically while making a
// disjoint function edit elsewhere — the re-export dedups to a single line
// and the merge stays clean (exercises resolve_item's add/add o==t arm for
// `use` items).
#[test]
fn rust_identical_use_addition_dedups_clean() {
    let base = "fn alpha() { 1 }\nfn beta() { 2 }\n";
    let ours = "pub use crate::shared::Thing;\nfn alpha() { 10 }\nfn beta() { 2 }\n";
    let theirs = "pub use crate::shared::Thing;\nfn alpha() { 1 }\nfn beta() { 20 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert_eq!(
        merged.matches("crate::shared::Thing").count(),
        1,
        "identical re-export must appear exactly once: {merged}"
    );
    assert!(
        merged.contains("fn alpha() { 10 }"),
        "ours edit lost: {merged}"
    );
    assert!(
        merged.contains("fn beta() { 20 }"),
        "theirs edit lost: {merged}"
    );
}

// heddle#468 r1 (Codex P2): grouped-vs-ungrouped imports must be normalized
// to per-leaf paths before keying, or an overlapping group/single pair
// unions into a duplicate import (a Rust "name defined multiple times"
// error) instead of dedup/conflict.

// One side adds the grouped form `{Bar, Baz}`, the other adds the single
// `Bar`. They share the leaf `crate::foo::Bar`, so they must NOT union into
// two lines that both import `Bar`. A conflict (or a dedup) is correct; a
// clean union containing a duplicate `Bar` is the bug.
#[test]
fn rust_grouped_vs_ungrouped_overlap_does_not_duplicate() {
    let base = "fn anchor() { 0 }\n";
    let ours = "use crate::foo::{Bar, Baz};\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::Bar;\nfn anchor() { 0 }\n";
    let outcome = merge_rust(base, ours, theirs);
    // Pre-fix this resolved Clean with two separate `use` lines importing
    // `Bar`. Representative-leaf keying collides them into an add/add
    // conflict instead.
    let (text, count) = assert_conflicts(outcome);
    assert!(
        count >= 1,
        "overlapping grouped/ungrouped imports must conflict, not union: {text}"
    );
}

// Both sides add the SAME grouped re-export while editing a disjoint
// function — the group is keyed and dedups to a single occurrence, clean.
#[test]
fn rust_identical_grouped_use_dedups_clean() {
    let base = "fn alpha() { 1 }\nfn beta() { 2 }\n";
    let ours = "pub use crate::foo::{Bar, Baz};\nfn alpha() { 10 }\nfn beta() { 2 }\n";
    let theirs = "pub use crate::foo::{Bar, Baz};\nfn alpha() { 1 }\nfn beta() { 20 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert_eq!(
        merged.matches("crate::foo::{Bar, Baz}").count(),
        1,
        "identical grouped re-export must appear exactly once: {merged}"
    );
    assert!(
        merged.contains("fn alpha() { 10 }"),
        "ours edit lost: {merged}"
    );
    assert!(
        merged.contains("fn beta() { 20 }"),
        "theirs edit lost: {merged}"
    );
}

// Distinct single re-exports on different leaf paths still auto-combine —
// the leaf-keying change must not regress the r0 union case.
#[test]
fn rust_distinct_reexports_still_auto_combine() {
    let base = "fn anchor() { 0 }\n";
    let ours = "pub use crate::a::X;\nfn anchor() { 0 }\n";
    let theirs = "pub use crate::b::Y;\nfn anchor() { 0 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(
        merged.contains("crate::a::X"),
        "ours re-export lost: {merged}"
    );
    assert!(
        merged.contains("crate::b::Y"),
        "theirs re-export lost: {merged}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "distinct re-exports must merge cleanly: {merged}"
    );
}

// Un-normalizable forms (glob, `as` alias) can't be expanded into discrete
// leaves, so they take the safe fallback: an overlapping add/add of two
// such forms conflicts rather than silently unioning into a possible
// duplicate import.
#[test]
fn rust_glob_alias_unnormalizable_conflicts_not_misunion() {
    let base = "fn anchor() { 0 }\n";
    let ours = "use crate::foo::*;\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::Bar as Renamed;\nfn anchor() { 0 }\n";
    let outcome = merge_rust(base, ours, theirs);
    let (text, count) = assert_conflicts(outcome);
    assert!(
        count >= 1,
        "un-normalizable glob/alias adds must conflict, not mis-union: {text}"
    );
}

// heddle#468 r2 (Codex): close the representative-key partiality. r1 keyed a
// `use` by its SMALLEST leaf, so an overlap on a NON-minimum leaf escaped
// collision and unioned into a duplicate import. The fix collides two `use`
// declarations whenever their expanded leaf sets intersect on ANY leaf
// (`canonicalize_use_keys` union-find), regardless of which leaf is smallest.

// Round-3 repro: the overlap is on the NON-representative leaf. ours groups
// `{Bar, Baz}` (min leaf `Bar`), theirs is the single `Baz` (the larger
// leaf). Under representative-leaf keying they got distinct keys (`Bar` vs
// `Baz`) and unioned into two lines both importing `Baz` — a Rust "defined
// multiple times" error. They must collide: a conflict (or a dedup) is
// correct, never two `Baz` imports.
#[test]
fn rust_grouped_vs_ungrouped_overlap_on_nonmin_leaf_does_not_duplicate() {
    let base = "fn anchor() { 0 }\n";
    let ours = "use crate::foo::{Bar, Baz};\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::Baz;\nfn anchor() { 0 }\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "overlap on the non-minimum leaf must conflict, not union: {text}"
    );
    // The invariant that matters regardless of conflict-vs-dedup: never two
    // lines importing `Baz`. Conflict markers may repeat the body, so assert
    // there is at most one `use ... Baz;` statement per conflict side by
    // checking the merged text never lands `Baz` twice OUTSIDE markers — here
    // the conservative check is simply that it isn't a clean union.
    assert!(
        text.contains("<<<<<<<"),
        "expected a conflict region, not a silent union: {text}"
    );
}

// 3-leaf group overlapping only on its LARGEST leaf. The group is spelled in
// DESCENDING leaf order (`{Ccc, Bbb, Aaa}`) so the component's canonical
// (smallest) leaf `Aaa` is interned LAST — exercising the union-find min
// update — while theirs imports the single LARGEST leaf `Ccc`.
// Representative-leaf keying (min `Aaa` vs `Ccc`) missed this overlap and
// unioned; leaf-set intersection collides them.
#[test]
fn rust_three_leaf_group_overlap_on_nonmin_leaf_collides() {
    let base = "fn anchor() { 0 }\n";
    let ours = "use crate::m::{Ccc, Bbb, Aaa};\nfn anchor() { 0 }\n";
    let theirs = "use crate::m::Ccc;\nfn anchor() { 0 }\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "overlap on the largest leaf must collide (conflict), not union: {text}"
    );
}

// heddle#468 r4 (Codex / maintainer): the partial-signal dedup is removed.
// The `use`-merge reduces to EXACTLY three cases — leaf-disjoint →
// auto-combine, byte-identical (incl. all leading metadata) → dedup,
// everything else → conflict. No leaves+visibility-only dedup remains, so
// any difference that survives normalization (grouping, alias, `cfg` /
// doc / attribute metadata) conflicts instead of silently dropping a side.

// Same leaf, same visibility, but spelled differently: `use a::{Baz}`
// (single-element group) vs `use a::Baz`. Under the OLD partial-signal dedup
// this collapsed to one line; under the byte-identical-only rule the two are
// not byte-equal, so they CONFLICT. This is the formatting-that-survives-
// normalization leg of case 3 — conflict, never a cosmetic dedup.
#[test]
fn rust_single_element_group_vs_plain_same_visibility_conflicts() {
    let base = "fn alpha() { 1 }\nfn beta() { 2 }\n";
    let ours = "use crate::foo::{Baz};\nfn alpha() { 10 }\nfn beta() { 2 }\n";
    let theirs = "use crate::foo::Baz;\nfn alpha() { 1 }\nfn beta() { 20 }\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "same leaf spelled two ways is not byte-identical → must conflict, \
         not partial-signal dedup: {text}"
    );
}

// Round-4 repro: same leaf, same visibility, DIFFERENT leading metadata —
// `#[cfg(unix)] use crate::foo::Bar;` vs `#[cfg(windows)] use crate::foo::Bar;`.
// The old leaves+visibility dedup returned "clean" and emitted only ours,
// SILENTLY DROPPING theirs and changing which platforms get the import — a
// correctness bug. The byte-identical-only rule sees the differing `#[cfg]`
// attribute and conflicts instead.
#[test]
fn rust_same_leaf_divergent_cfg_attribute_conflicts_not_silent_drop() {
    let base = "fn anchor() { 0 }\n";
    let ours = "#[cfg(unix)]\nuse crate::foo::Bar;\nfn anchor() { 0 }\n";
    let theirs = "#[cfg(windows)]\nuse crate::foo::Bar;\nfn anchor() { 0 }\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "same leaf with divergent #[cfg] must conflict, not silently drop a \
         platform's import: {text}"
    );
    // The dropped-platform bug: a silent dedup would emit `unix` and lose
    // `windows` (or vice-versa). A real conflict surfaces BOTH attributes.
    assert!(
        text.contains("cfg(unix)") && text.contains("cfg(windows)"),
        "both platform attributes must survive in the conflict region — \
         neither side may be silently dropped: {text}"
    );
}

// Same leaf path, DIFFERENT alias: `use crate::foo::Bar as B;` vs
// `... as C;`. The alias is part of the binding's meaning but lives outside
// the leaf path, so a partial-signal dedup would have dropped one. The
// byte-identical-only rule conflicts. (Both aliases are unanalyzable, so the
// region is poisoned and the whole-region merge conflicts the divergent
// text — heddle#468 r6.)
#[test]
fn rust_same_leaf_divergent_alias_conflicts() {
    let base = "fn anchor() { 0 }\n";
    let ours = "use crate::foo::Bar as B;\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::Bar as C;\nfn anchor() { 0 }\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "same leaf with divergent alias must conflict, not dedup: {text}"
    );
}

// Byte-identical including leading metadata → the ONE dedup leg of case 2.
// Both sides add the exact same `#[cfg(unix)] use ...;` while editing a
// disjoint function; the attributed import dedups to a single occurrence and
// the merge stays clean. Guards that adding the metadata dimension to the
// "identical" check didn't break true byte-identical dedup.
#[test]
fn rust_identical_attributed_use_addition_dedups_clean() {
    let base = "fn alpha() { 1 }\nfn beta() { 2 }\n";
    let ours = "#[cfg(unix)]\nuse crate::shared::Thing;\nfn alpha() { 10 }\nfn beta() { 2 }\n";
    let theirs = "#[cfg(unix)]\nuse crate::shared::Thing;\nfn alpha() { 1 }\nfn beta() { 20 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert_eq!(
        merged.matches("crate::shared::Thing").count(),
        1,
        "byte-identical attributed re-export must dedup to one line: {merged}"
    );
    assert_eq!(
        merged.matches("cfg(unix)").count(),
        1,
        "the shared attribute must appear exactly once: {merged}"
    );
    assert!(
        merged.contains("fn alpha() { 10 }"),
        "ours edit lost: {merged}"
    );
    assert!(
        merged.contains("fn beta() { 20 }"),
        "theirs edit lost: {merged}"
    );
}

// SAME leaf set spelled differently AND with divergent visibility
// (`pub use a::{Baz}` vs `use a::Baz`) — two independent non-byte
// differences, both landing in case 3. Must conflict.
#[test]
fn rust_single_element_group_vs_plain_divergent_visibility_conflicts() {
    let base = "fn anchor() { 0 }\n";
    let ours = "pub use crate::foo::{Baz};\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::Baz;\nfn anchor() { 0 }\n";
    let (_text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "same leaves but divergent visibility must conflict, not dedup"
    );
}

// Empty base, overlap on a non-minimum leaf. Exercises the empty-base
// add/add routing in `mod.rs` (which keys off `Item`) together with the
// canonicalized key, confirming the conflict is surfaced rather than the
// merge being routed to text_hunk_merge and silently duplicating `Z`.
#[test]
fn rust_empty_base_overlap_on_nonmin_leaf_conflicts() {
    let base = "";
    let ours = "use crate::n::{Yy, Zz};\n";
    let theirs = "use crate::n::Zz;\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "empty-base overlap on non-min leaf must conflict, not duplicate: {text}"
    );
}

// heddle#468 r5 (Codex / maintainer): the use-merge is lifted off positional
// occurrence-matching onto whole leaf-component (leaf-SET) comparison. The
// representative bug: base has a single `use crate::foo::Bar;`; ours ADDS a
// separate `use crate::foo::Baz;` line; theirs WIDENS the base item to
// `use crate::foo::{Bar, Baz};`. All three canonicalize into one component
// (`Bar`/`Baz` unioned). Under occurrence-matching, base's `Bar`@occ0 paired
// with ours's `Bar`@occ0 (unchanged) / theirs's `{Bar,Baz}`@occ0 (modify) →
// clean take-theirs, while ours's `Baz`@occ1 (add) had no theirs partner →
// clean take-ours. The merge emitted BOTH `{Bar, Baz}` and a standalone
// `Baz` — a duplicate import (Rust E0252), no conflict. Set comparison sees
// ours-text != base, theirs-text != base, ours-text != theirs → CONFLICT.

// The repro itself: a clean union of the widened group + the standalone leaf
// is the bug. The whole component must conflict instead.
#[test]
fn rust_use_base_widened_vs_separate_add_conflicts_not_duplicate() {
    let base = "use crate::foo::Bar;\nfn anchor() { 0 }\n";
    let ours = "use crate::foo::Bar;\nuse crate::foo::Baz;\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::{Bar, Baz};\nfn anchor() { 0 }\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "base-widened group vs separate-leaf add must conflict, not silently \
         emit both (a duplicate `Baz` import): {text}"
    );
    // The defining invariant: never a clean union. A duplicate import would
    // resolve Clean; the conflict region proves we did not.
    assert!(
        text.contains("<<<<<<<"),
        "expected a conflict region, not a silent union: {text}"
    );
}

// Mirror: swap ours/theirs roles (ours widens the base item, theirs adds the
// separate leaf). The set comparison is symmetric, so this must also
// conflict — guards against an asymmetry in which side is treated as "base".
#[test]
fn rust_use_base_widened_vs_separate_add_conflicts_mirror() {
    let base = "use crate::foo::Bar;\nfn anchor() { 0 }\n";
    let ours = "use crate::foo::{Bar, Baz};\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::Bar;\nuse crate::foo::Baz;\nfn anchor() { 0 }\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "mirror of the base-widened case must also conflict: {text}"
    );
    assert!(
        text.contains("<<<<<<<"),
        "expected a conflict region, not a silent union: {text}"
    );
}

// Both sides widen the SAME base item, differently: base `use crate::foo::Bar;`;
// ours → `{Bar, Baz}`; theirs → `{Bar, Qux}`. Each side modified the base
// item, divergently, so the whole component conflicts (no auto-combine of two
// incompatible regroupings into a triple-import line).
#[test]
fn rust_use_both_widen_base_differently_conflicts() {
    let base = "use crate::foo::Bar;\nfn anchor() { 0 }\n";
    let ours = "use crate::foo::{Bar, Baz};\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::{Bar, Qux};\nfn anchor() { 0 }\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "both sides widening the same base item differently must conflict: {text}"
    );
}

// Pure-additive disjoint still combines under the set-valued path: base has
// no related import; ours adds `use a::X;`, theirs adds `use b::Y;`. Disjoint
// leaves → distinct components → each is a one-side add → both land cleanly.
// Guards that lifting onto leaf-SET comparison did not regress the r0 union.
#[test]
fn rust_use_disjoint_additions_still_combine_set_path() {
    let base = "fn anchor() { 0 }\n";
    let ours = "use crate::a::X;\nfn anchor() { 0 }\n";
    let theirs = "use crate::b::Y;\nfn anchor() { 0 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(merged.contains("crate::a::X"), "ours add lost: {merged}");
    assert!(merged.contains("crate::b::Y"), "theirs add lost: {merged}");
    assert!(
        !merged.contains("<<<<<<<"),
        "disjoint additions must combine cleanly: {merged}"
    );
}

// Exact-identical multi-line component on both sides dedups to one copy.
// Base has the grouped `use crate::foo::{Bar, Baz};`; BOTH sides rewrite it
// to the same two standalone lines (so each side contributes TWO items to the
// component — the multi-occurrence shape) while editing a disjoint function.
// Byte-identical component texts → dedup, clean, no duplicate leaves.
#[test]
fn rust_use_identical_multiline_component_dedups_clean() {
    let base = "use crate::foo::{Bar, Baz};\nfn alpha() { 1 }\nfn beta() { 2 }\n";
    let ours = "use crate::foo::Bar;\nuse crate::foo::Baz;\nfn alpha() { 10 }\nfn beta() { 2 }\n";
    let theirs = "use crate::foo::Bar;\nuse crate::foo::Baz;\nfn alpha() { 1 }\nfn beta() { 20 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert_eq!(
        merged.matches("crate::foo::Bar").count(),
        1,
        "Bar must appear exactly once after dedup: {merged}"
    );
    assert_eq!(
        merged.matches("crate::foo::Baz").count(),
        1,
        "Baz must appear exactly once after dedup: {merged}"
    );
    assert!(
        merged.contains("fn alpha() { 10 }"),
        "ours edit lost: {merged}"
    );
    assert!(
        merged.contains("fn beta() { 20 }"),
        "theirs edit lost: {merged}"
    );
}

// heddle#468 r6 (Codex cid 3350239933): the clever leaf-component union is
// capped to fully-analyzable PLAIN imports. Before this fix an unanalyzable
// form (`self` in a group / nested group / glob / alias) carried a single
// sentinel "leaf" that `canonicalize_use_keys` treated as a normal leaf —
// but the sentinel is disjoint from every real import path, so an exotic
// form that overlaps a plain import landed in a DIFFERENT union-find
// component, both were emitted, and a duplicate import slipped through (the
// exact bug this PR exists to prevent). Now: any use-region containing an
// unanalyzable item on ANY side bypasses the leaf union entirely and merges
// conservatively, so the overlap surfaces as a conflict. These tests pin
// each exotic form against an overlapping plain import; the un-poisoned
// plain-only cases above (r0/r5) must stay clean.

// `self` in a group vs the plain leaf it hides: `use crate::foo::{self, Bar};`
// expands to `foo` AND `foo::Bar`, overlapping theirs' `use crate::foo::Bar;`.
// Pre-fix the `{self, Bar}` group keyed to the sentinel and `Bar` keyed to
// `crate::foo::Bar` → distinct components → both emitted → duplicate `Bar`.
// The poison path conflicts the whole region instead.
#[test]
fn rust_use_self_group_vs_plain_conflicts_not_duplicate() {
    let base = "fn anchor() { 0 }\n";
    let ours = "use crate::foo::{self, Bar};\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::Bar;\nfn anchor() { 0 }\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "self-group overlapping a plain import must conflict, not duplicate: {text}"
    );
    assert!(
        text.contains("<<<<<<<"),
        "expected a conflict region, not a silent union: {text}"
    );
}

// Nested group vs the plain leaf it hides: `use crate::foo::{bar::{Baz}};`
// (nested → unanalyzable) overlaps theirs' `use crate::foo::bar::Baz;`. The
// poison path conflicts rather than mis-unioning into a duplicate `Baz`.
#[test]
fn rust_use_nested_group_vs_plain_conflicts() {
    let base = "fn anchor() { 0 }\n";
    let ours = "use crate::foo::{bar::{Baz}};\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::bar::Baz;\nfn anchor() { 0 }\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "nested group overlapping a plain import must conflict: {text}"
    );
    assert!(
        text.contains("<<<<<<<"),
        "expected a conflict region, not a silent union: {text}"
    );
}

// Glob vs a plain import under the same path: `use crate::foo::*;` (glob →
// unanalyzable) could re-export `Bar`, so it overlaps theirs'
// `use crate::foo::Bar;`. We can't expand the glob's leaves, so the only
// safe verdict is a conflict — never a clean union that duplicates `Bar`.
#[test]
fn rust_use_glob_vs_plain_overlap_conflicts() {
    let base = "fn anchor() { 0 }\n";
    let ours = "use crate::foo::*;\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::Bar;\nfn anchor() { 0 }\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "glob overlapping a plain import must conflict, not mis-union: {text}"
    );
    assert!(
        text.contains("<<<<<<<"),
        "expected a conflict region, not a silent union: {text}"
    );
}

// Alias vs the plain leaf it renames: `use crate::foo::Bar as B;` (alias →
// unanalyzable) and theirs' `use crate::foo::Bar;` share the leaf
// `crate::foo::Bar`. The r4 alias test conflicts alias-vs-alias (both
// unanalyzable, same sentinel component); this is the alias-vs-PLAIN case
// the old sentinel keying mis-unioned — it must conflict via the new poison
// path, not emit both lines.
#[test]
fn rust_use_alias_vs_plain_same_leaf_conflicts() {
    let base = "fn anchor() { 0 }\n";
    let ours = "use crate::foo::Bar as B;\nfn anchor() { 0 }\n";
    let theirs = "use crate::foo::Bar;\nfn anchor() { 0 }\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "alias overlapping the plain leaf it renames must conflict: {text}"
    );
    assert!(
        text.contains("<<<<<<<"),
        "expected a conflict region, not a silent union: {text}"
    );
}

// Byte-identical exotic form on both sides → the dedup leg survives the
// poison path. Both sides add the SAME `use crate::foo::*;` while editing a
// disjoint function; the region is poisoned (glob is unanalyzable) but the
// two component texts are byte-identical, so the conservative merge dedups
// to one copy and stays clean. Pins that poisoning does NOT regress
// identical-text dedup into a spurious conflict.
#[test]
fn rust_use_identical_glob_both_sides_dedups_clean() {
    let base = "fn alpha() { 1 }\nfn beta() { 2 }\n";
    let ours = "use crate::foo::*;\nfn alpha() { 10 }\nfn beta() { 2 }\n";
    let theirs = "use crate::foo::*;\nfn alpha() { 1 }\nfn beta() { 20 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert_eq!(
        merged.matches("crate::foo::*").count(),
        1,
        "byte-identical glob must dedup to one line, not conflict: {merged}"
    );
    assert!(
        merged.contains("fn alpha() { 10 }"),
        "ours edit lost: {merged}"
    );
    assert!(
        merged.contains("fn beta() { 20 }"),
        "theirs edit lost: {merged}"
    );
}

// heddle#490 r7 — per-scope use-poison. An unanalyzable glob nested in
// `mod m { use x::*; }` (unchanged on both sides) must NOT poison the
// disjoint TOP-LEVEL use-region: ours adds `use a::A;` and theirs adds
// `use b::B;` at the file root, on leaves the nested glob cannot reach. With
// the old file-global poison flag these collapsed onto one USE_POISON_KEY
// component and reported a spurious add/add conflict; scope-keyed poison
// leaves the top-level region clean so both disjoint imports survive.
#[test]
fn rust_use_nested_glob_does_not_poison_disjoint_top_level_adds() {
    let base = "mod m {\n    use x::*;\n}\nfn anchor() { 0 }\n";
    let ours = "use a::A;\nmod m {\n    use x::*;\n}\nfn anchor() { 0 }\n";
    let theirs = "use b::B;\nmod m {\n    use x::*;\n}\nfn anchor() { 0 }\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(
        merged.contains("use a::A;"),
        "ours top-level add lost: {merged}"
    );
    assert!(
        merged.contains("use b::B;"),
        "theirs top-level add lost: {merged}"
    );
    assert_eq!(
        merged.matches("use x::*;").count(),
        1,
        "unchanged nested glob must dedup to one copy: {merged}"
    );
}

// heddle#490 r7 — companion: the SAME nested glob STILL poisons ITS OWN
// scope (correctness of the poison is preserved; only the cross-scope leak
// is fixed). Both sides divergently edit a plain `use` *inside* `mod m`,
// whose scope holds the unanalyzable `use x::*;`. Without the glob the two
// edits rekey to distinct leaf-components and union cleanly; WITH the glob,
// `m`'s use-region is poisoned, every `use` collapses onto USE_POISON_KEY,
// and the divergent component conservatively conflicts. A same-line
// modify/modify also conflicts under the textual floor, so the conservative
// verdict holds regardless of which path resolves it.
#[test]
fn rust_use_nested_glob_still_poisons_its_own_scope() {
    let base = "mod m {\n    use x::*;\n    use crate::foo::Thing;\n}\n";
    let ours = "mod m {\n    use x::*;\n    use crate::foo::ThingA;\n}\n";
    let theirs = "mod m {\n    use x::*;\n    use crate::foo::ThingB;\n}\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(
        count >= 1,
        "divergent use edits inside a glob-poisoned scope must conflict: {text}"
    );
    assert!(
        text.contains("<<<<<<<"),
        "expected a conflict region in the poisoned nested scope: {text}"
    );
}

// =====================================================================
// heddle#484 / #490 — close-the-class conformance harness (ported from the
// paused PR #487 branch, adapted to the container-as-node tree model).
//
// For every CLEAN semantic merge it asserts, on the PARSED output:
//   1. item-set + nesting conservation — every merged item (now INCLUDING
//      container items: impl/mod/trait, which the tree model makes
//      first-class) is present with the structural scope a contributing side
//      gave it, and none is invented or dropped;
//   2. the output re-parses — a clean merge that yields an unparseable file is
//      by definition a corruption.
//
// In the #487 branch this used a re-derived `struct_scope`; here identity is
// `(key.scope, kind, name)` flattened over the real tree via `visit_items`, so
// a child's scope IS its enclosing-container chain by construction.
// =====================================================================

/// (structural-scope, kind, name) identity of every item in `source`,
/// flattened over the container tree (no `use` canonicalization, so output and
/// inputs compare on the same footing).
fn item_identities(
    source: &str,
    language: crate::parser::Language,
) -> std::collections::BTreeSet<(Vec<String>, String, String)> {
    let parsed = crate::parser::ParsedFile::parse(source, language).expect("input must parse");
    let segs = super::items::segment_file(&parsed);
    let mut out = std::collections::BTreeSet::new();
    super::items::visit_items(&segs.items, &mut |i| {
        out.insert((
            i.key.scope.clone(),
            format!("{:?}", i.key.kind),
            i.key.name.clone(),
        ));
    });
    out
}

/// Run the conformance checks against a clean semantic merge of a Rust triple.
fn assert_conformant(base: &str, ours: &str, theirs: &str) -> String {
    assert_conformant_at(base, ours, theirs, "a.rs", crate::parser::Language::Rust)
}

/// Language-parameterized [`assert_conformant`].
fn assert_conformant_at(
    base: &str,
    ours: &str,
    theirs: &str,
    path: &str,
    language: crate::parser::Language,
) -> String {
    let out = assert_clean(merge_at(base, ours, theirs, path));

    assert!(
        crate::parser::ParsedFile::parse(&out, language).is_some(),
        "clean merge produced an unparseable file:\n{out}"
    );

    let mut expected = item_identities(base, language);
    expected.extend(item_identities(ours, language));
    expected.extend(item_identities(theirs, language));
    let got = item_identities(&out, language);
    assert_eq!(
        got, expected,
        "item-set / nesting not conserved\n got: {got:?}\n want: {expected:?}\n output:\n{out}"
    );
    out
}

#[test]
fn conformance_484_structural_matrix() {
    // {flat-top-level, trailing-mod, nested-pub-use} × {added-above,
    // added-below, added-inside}. Each side makes a disjoint additive edit;
    // every cell must survive the conformance checks.
    let cases: &[(&str, &str, &str)] = &[
        (
            "pub fn a() {}\n\n// MARK\n",
            "pub fn a() {}\n\npub fn c() {}\n\n// MARK\n",
            "pub fn a() {}\n\npub fn b() {}\n\n// MARK\n",
        ),
        (
            "pub fn a() {}\n",
            "pub fn a() {}\n\npub fn c() {}\n",
            "pub fn a() {}\n\npub fn b() {}\n",
        ),
        (
            "pub fn z() {}\n",
            "pub fn c() {}\n\npub fn z() {}\n",
            "pub fn b() {}\n\npub fn z() {}\n",
        ),
        (
            "pub fn a() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n",
            "pub fn a() {}\n\npub fn c() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n",
            "pub fn a() {}\n\npub fn b() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n",
        ),
        (
            "#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n",
            "#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n\npub fn ours_after() {}\n",
            "#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n\npub fn theirs_after() {}\n",
        ),
        (
            "mod tests {\n    fn t() {}\n}\n",
            "mod tests {\n    fn t() {}\n    fn ours_t() {}\n}\n",
            "mod tests {\n    fn t() {}\n    fn theirs_t() {}\n}\n",
        ),
        (
            "pub mod prelude {\n    pub use crate::a;\n}\n",
            "pub mod prelude {\n    pub use crate::a;\n    pub use crate::b;\n}\n\npub fn ours_fn() {}\n",
            "pub mod prelude {\n    pub use crate::a;\n    pub use crate::c;\n}\n\npub fn theirs_fn() {}\n",
        ),
        (
            "pub mod prelude {\n    pub use crate::a;\n}\n",
            "pub fn ours_fn() {}\n\npub mod prelude {\n    pub use crate::a;\n    pub use crate::b;\n}\n",
            "pub fn theirs_fn() {}\n\npub mod prelude {\n    pub use crate::a;\n    pub use crate::c;\n}\n",
        ),
        (
            "pub mod outer {\n    pub mod inner {\n        pub use crate::a;\n    }\n}\n",
            "pub mod outer {\n    pub mod inner {\n        pub use crate::a;\n        pub use crate::b;\n    }\n}\n\npub fn ours_fn() {}\n",
            "pub mod outer {\n    pub mod inner {\n        pub use crate::a;\n        pub use crate::c;\n    }\n}\n\npub fn theirs_fn() {}\n",
        ),
        // NOTE: the no-base add/add of a DIVERGENT `pub mod m` (ours adds
        // `{ pub use crate::x }`, theirs `{ pub use crate::y }`) used to land
        // here as a "clean" merge — but only via the buggy structural-recursion
        // → conservation-floor → whole-file-concat fallback, which silently
        // emitted TWO `pub mod m` blocks (an illegal duplicate module, E0428).
        // heddle#490 r5 routes that no-base add/add through a whole-container
        // comparison: divergent ⇒ conflict. It now lives in
        // `add_add_divergent_mod_no_base_conflicts_not_duplicate_module` below.
        (
            "struct S;\nimpl S {\n    fn base(&self) {}\n}\n",
            "struct S;\nimpl S {\n    fn base(&self) {}\n    fn ours_m(&self) {}\n}\n\nfn ours_top() {}\n",
            "struct S;\nimpl S {\n    fn base(&self) {}\n    fn theirs_m(&self) {}\n}\n\nfn theirs_top() {}\n",
        ),
    ];
    for (base, ours, theirs) in cases {
        assert_conformant(base, ours, theirs);
    }
}

#[test]
fn repro_484_bug1_trailing_postamble_not_duplicated() {
    // Each side adds a function above a trailing `// MARK`. Pre-fix the
    // postamble was woven in twice. The tree model never duplicates it.
    let base = "pub fn a() {}\n\n// MARK\n";
    let ours = "pub fn a() {}\n\npub fn c() {}\n\n// MARK\n";
    let theirs = "pub fn a() {}\n\npub fn b() {}\n\n// MARK\n";
    let merged = assert_conformant(base, ours, theirs);
    assert_eq!(
        merged.matches("// MARK").count(),
        1,
        "marker duplicated:\n{merged}"
    );
    assert!(
        merged.contains("pub fn b() {}"),
        "theirs add lost:\n{merged}"
    );
    assert!(merged.contains("pub fn c() {}"), "ours add lost:\n{merged}");
    // both additions precede the single marker.
    let mark = merged.find("// MARK").unwrap();
    assert!(
        merged.find("pub fn b() {}").unwrap() < mark,
        "b after marker:\n{merged}"
    );
    assert!(
        merged.find("pub fn c() {}").unwrap() < mark,
        "c after marker:\n{merged}"
    );
}

#[test]
fn repro_484_bug2_trailing_module_not_nested_or_duplicated() {
    // Each side adds a top-level fn above a trailing `mod tests`. Pre-fix one
    // fn nested INSIDE mod tests and the module duplicated (unclosed brace).
    let base = "pub fn a() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
    let ours = "pub fn a() {}\n\npub fn c() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
    let theirs = "pub fn a() {}\n\npub fn b() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
    let merged = assert_conformant(base, ours, theirs);
    assert_eq!(
        merged.matches("mod tests").count(),
        1,
        "module duplicated:\n{merged}"
    );
    // both added fns stay at top level, BEFORE the module.
    let m = merged.find("mod tests").unwrap();
    assert!(
        merged.find("pub fn b() {}").unwrap() < m,
        "b nested/after module:\n{merged}"
    );
    assert!(
        merged.find("pub fn c() {}").unwrap() < m,
        "c nested/after module:\n{merged}"
    );
    assert!(merged.contains("fn t() {}"), "module body lost:\n{merged}");
}

#[test]
fn repro_484_bug3_nested_pub_use_stays_inside_module() {
    // Each side adds a `pub use` inside `pub mod prelude` AND a sibling
    // top-level fn. Pre-fix one re-export escaped `prelude`.
    let base = "pub mod prelude {\n    pub use crate::a;\n}\n";
    let ours = "pub mod prelude {\n    pub use crate::a;\n    pub use crate::b;\n}\n\npub fn ours_fn() {}\n";
    let theirs = "pub mod prelude {\n    pub use crate::a;\n    pub use crate::c;\n}\n\npub fn theirs_fn() {}\n";
    let merged = assert_conformant(base, ours, theirs);
    assert_eq!(
        merged.matches("pub mod prelude").count(),
        1,
        "module duplicated:\n{merged}"
    );
    // The two re-exports stay INSIDE the module (before its closing brace),
    // and the sibling fns stay OUTSIDE (after it). conformance already pins
    // the scopes; this pins the byte order.
    let close = merged.find("}").unwrap();
    assert!(
        merged.find("pub use crate::b").unwrap() < close,
        "b escaped module:\n{merged}"
    );
    assert!(
        merged.find("pub use crate::c").unwrap() < close,
        "c escaped module:\n{merged}"
    );
    assert!(
        merged.find("pub fn ours_fn").unwrap() > close,
        "ours_fn inside module:\n{merged}"
    );
    assert!(
        merged.find("pub fn theirs_fn").unwrap() > close,
        "theirs_fn inside module:\n{merged}"
    );
}

#[test]
fn repro_484_reopened_rust_impl_keeps_top_level_between_blocks() {
    // Two inherent `impl Foo` blocks separated by a top-level fn. ours adds a
    // method to the first; theirs to the second. Pre-fix the impls collapsed
    // and the second's method reordered ahead of `top_level`.
    let base = "struct Foo;\n\nimpl Foo {\n    fn a(&self) {}\n}\n\nfn top_level() {}\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    let ours = "struct Foo;\n\nimpl Foo {\n    fn a(&self) {}\n    fn a2(&self) {}\n}\n\nfn top_level() {}\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    let theirs = "struct Foo;\n\nimpl Foo {\n    fn a(&self) {}\n}\n\nfn top_level() {}\n\nimpl Foo {\n    fn b(&self) {}\n    fn b2(&self) {}\n}\n";
    let merged = assert_conformant(base, ours, theirs);
    assert!(
        merged.contains("fn a2(&self) {}"),
        "ours add lost:\n{merged}"
    );
    assert!(
        merged.contains("fn b2(&self) {}"),
        "theirs add lost:\n{merged}"
    );
    assert_eq!(
        merged.matches("impl Foo").count(),
        2,
        "reopened impls collapsed:\n{merged}"
    );
    let first = merged.find("impl Foo").unwrap();
    let top = merged.find("fn top_level()").unwrap();
    let last = merged.rfind("impl Foo").unwrap();
    assert!(
        first < top && top < last,
        "top-level fn must sit between impls:\n{merged}"
    );
    assert!(
        merged.find("fn a2(&self) {}").unwrap() < top,
        "a2 escaped first impl:\n{merged}"
    );
    assert!(
        merged.find("fn b2(&self) {}").unwrap() > top,
        "b2 escaped second impl:\n{merged}"
    );
}

#[test]
fn repro_484_reopened_rust_impl_add_first_modify_second_keeps_order() {
    // ours adds a method to the FIRST impl; theirs modifies a body in the
    // SECOND. The intervening `top_level` must stay between, theirs's edit
    // survive, the two blocks stay distinct.
    let base = "struct Foo;\n\nimpl Foo {\n    fn a(&self) {}\n}\n\nfn top_level() {}\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    let ours = "struct Foo;\n\nimpl Foo {\n    fn a(&self) {}\n    fn a2(&self) {}\n}\n\nfn top_level() {}\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    let theirs = "struct Foo;\n\nimpl Foo {\n    fn a(&self) {}\n}\n\nfn top_level() {}\n\nimpl Foo {\n    fn b(&self) { let _ = 1; }\n}\n";
    let merged = assert_conformant(base, ours, theirs);
    assert!(
        merged.contains("fn a2(&self) {}"),
        "ours add lost:\n{merged}"
    );
    assert!(
        merged.contains("fn b(&self) { let _ = 1; }"),
        "theirs edit lost:\n{merged}"
    );
    assert_eq!(
        merged.matches("impl Foo").count(),
        2,
        "reopened impls collapsed:\n{merged}"
    );
    let first = merged.find("impl Foo").unwrap();
    let top = merged.find("fn top_level()").unwrap();
    let last = merged.rfind("impl Foo").unwrap();
    assert!(
        first < top && top < last,
        "top-level fn must sit between impls:\n{merged}"
    );
}

/// A reopened-`impl Foo` triple whose two blocks are joined by `sep`. ours
/// adds `fn a2` to the first block; theirs adds `fn b2` to the second.
fn impl_reopen_triple(sep: &str) -> (String, String, String) {
    let base = format!(
        "struct Foo;\n\nimpl Foo {{\n    fn a(&self) {{}}\n}}{sep}impl Foo {{\n    fn b(&self) {{}}\n}}\n"
    );
    let ours = format!(
        "struct Foo;\n\nimpl Foo {{\n    fn a(&self) {{}}\n    fn a2(&self) {{}}\n}}{sep}impl Foo {{\n    fn b(&self) {{}}\n}}\n"
    );
    let theirs = format!(
        "struct Foo;\n\nimpl Foo {{\n    fn a(&self) {{}}\n}}{sep}impl Foo {{\n    fn b(&self) {{}}\n    fn b2(&self) {{}}\n}}\n"
    );
    (base, ours, theirs)
}

#[test]
fn conformance_484_reopened_rust_impl_separator_matrix() {
    // back-to-back / whitespace / comment / item separators between the two
    // `impl Foo` blocks. Every separator must keep the blocks distinct,
    // preserve any separator text, and keep source order.
    let cases: &[(&str, &str)] = &[
        ("back-to-back", "\n"),
        ("whitespace-separated", "\n\n"),
        ("comment-separated", "\n// section\n"),
        ("item-separated", "\n\nfn top_level() {}\n\n"),
    ];
    for (label, sep) in cases {
        let (base, ours, theirs) = impl_reopen_triple(sep);
        let merged = assert_conformant(&base, &ours, &theirs);
        assert_eq!(
            merged.matches("impl Foo").count(),
            2,
            "[{label}] reopened impls collapsed:\n{merged}"
        );
        assert!(
            merged.contains("fn a2(&self) {}"),
            "[{label}] ours add lost:\n{merged}"
        );
        assert!(
            merged.contains("fn b2(&self) {}"),
            "[{label}] theirs add lost:\n{merged}"
        );
        let second_impl = merged.rfind("impl Foo").unwrap();
        assert!(
            merged.find("fn a2(&self) {}").unwrap() < second_impl,
            "[{label}] a2 escaped first block:\n{merged}"
        );
        assert!(
            merged.find("fn b2(&self) {}").unwrap() > second_impl,
            "[{label}] b2 escaped second block:\n{merged}"
        );
        if sep.contains("// section") {
            assert_eq!(
                merged.matches("// section").count(),
                1,
                "[{label}] comment separator dropped/duplicated:\n{merged}"
            );
        }
        if sep.contains("fn top_level") {
            let first_impl = merged.find("impl Foo").unwrap();
            let top = merged.find("fn top_level()").unwrap();
            assert!(
                first_impl < top && top < second_impl,
                "[{label}] top-level item must stay between blocks:\n{merged}"
            );
        }
    }
}

#[test]
fn repro_484_r3_prepend_new_impl_before_base_block_no_collapse() {
    // base has one `impl Foo { fn b }`. ours prepends a NEW `impl Foo { fn a }`
    // before it; theirs edits fn b's body. Pre-fix the prepended block and the
    // base block collapsed into a single `impl Foo`.
    let base = "struct Foo;\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    let ours =
        "struct Foo;\n\nimpl Foo {\n    fn a(&self) {}\n}\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    let theirs = "struct Foo;\n\nimpl Foo {\n    fn b(&self) { let _ = 1; }\n}\n";
    let merged = assert_conformant(base, ours, theirs);
    assert_eq!(
        merged.matches("impl Foo").count(),
        2,
        "prepended impl collapsed:\n{merged}"
    );
    assert!(
        merged.contains("fn a(&self) {}"),
        "ours add lost:\n{merged}"
    );
    assert!(
        merged.contains("fn b(&self) { let _ = 1; }"),
        "theirs edit lost:\n{merged}"
    );
    assert!(
        merged.find("fn a(&self) {}").unwrap() < merged.find("fn b(&self)").unwrap(),
        "prepended block must precede base block:\n{merged}"
    );
}

#[test]
fn conformance_484_r3_cross_side_container_alignment_matrix() {
    // Same-name-container arrangements the per-side ordinal model mis-aligned.
    // A new block added on ONE side only (prepend/append) aligns distinctly from
    // the matched base block and must merge CLEAN. The `both-add-different`
    // round-4 case (a new block added on BOTH sides with no base anchor) is no
    // longer clean: heddle#490 r5 conflicts it as a whole container — see
    // `reopened_impl_both_add_new_block_no_base_conflicts` below.
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "prepend",
            "struct Foo;\n\nimpl Foo {\n    fn b(&self) {}\n}\n",
            "struct Foo;\n\nimpl Foo {\n    fn a(&self) {}\n}\n\nimpl Foo {\n    fn b(&self) {}\n}\n",
            "struct Foo;\n\nimpl Foo {\n    fn b(&self) { let _ = 1; }\n}\n",
        ),
        (
            "append",
            "struct Foo;\n\nimpl Foo {\n    fn b(&self) {}\n}\n",
            "struct Foo;\n\nimpl Foo {\n    fn b(&self) {}\n}\n\nimpl Foo {\n    fn a(&self) {}\n}\n",
            "struct Foo;\n\nimpl Foo {\n    fn b(&self) { let _ = 1; }\n}\n",
        ),
    ];
    for (label, base, ours, theirs) in cases {
        let merged = assert_conformant(base, ours, theirs);
        for needle in ["fn a", "fn b"] {
            assert!(
                merged.contains(needle),
                "[{label}] {needle} lost:\n{merged}"
            );
        }
        assert!(
            crate::parser::ParsedFile::parse(&merged, crate::parser::Language::Rust).is_some(),
            "[{label}] unparseable merge:\n{merged}"
        );
    }
}

// The round-4 `both-add-different` case, post heddle#490 r5. base has one
// `impl Foo { fn b }`; ours prepends a NEW `impl Foo { fn a }`, theirs appends a
// NEW `impl Foo { fn c }`. The two NEW blocks have no base anchor and collide on
// the same fresh discriminator — an add/add with divergent whole content. Pre-r5
// this only "merged clean" via the buggy structural-recursion → floor →
// whole-file-concat fallback (the same fallback that silently duplicates a
// non-reopenable module). r5 routes it through the whole-container comparison:
// divergent ⇒ a single conflict that preserves both new blocks' bodies, while
// the base-anchored `fn b` block resolves cleanly outside the markers. A
// conflict is the safe outcome — the engine has no base to know these are two
// independent additions vs the same block added twice.
#[test]
fn reopened_impl_both_add_new_block_no_base_conflicts() {
    let base = "struct Foo;\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    let ours =
        "struct Foo;\n\nimpl Foo {\n    fn a(&self) {}\n}\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    let theirs =
        "struct Foo;\n\nimpl Foo {\n    fn b(&self) {}\n}\n\nimpl Foo {\n    fn c(&self) {}\n}\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(count >= 1, "expected ≥1 conflict, got {count}: {text}");
    assert!(
        text.contains("<<<<<<<") && text.contains("=======") && text.contains(">>>>>>>"),
        "expected canonical conflict markers: {text}"
    );
    assert!(
        text.contains("fn a(&self)") && text.contains("fn c(&self)"),
        "both new blocks' bodies must survive inside the conflict: {text}"
    );
    // The base-anchored `fn b` block resolves cleanly, outside the markers.
    assert!(
        text.contains("fn b(&self)"),
        "base-anchored block lost: {text}"
    );
}

#[test]
fn repro_484_r3_reordered_container_no_silent_collapse() {
    // ours REORDERS the two same-name `impl Foo` blocks; theirs edits the first
    // disjointly. Never a silent corruption: the outcome is a conflict OR a
    // clean merge that re-parses with both blocks intact.
    let base =
        "struct Foo;\n\nimpl Foo {\n    fn a(&self) {}\n}\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    let ours =
        "struct Foo;\n\nimpl Foo {\n    fn b(&self) {}\n}\n\nimpl Foo {\n    fn a(&self) {}\n}\n";
    let theirs = "struct Foo;\n\nimpl Foo {\n    fn a(&self) { let _ = 1; }\n}\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    let text = match merge_rust(base, ours, theirs) {
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            conflict_count,
        } => {
            assert!(conflict_count >= 1);
            String::from_utf8(merged_bytes_with_markers).unwrap()
        }
        MergeOutcome::Clean(bytes) => {
            let text = String::from_utf8(bytes).unwrap();
            assert!(
                crate::parser::ParsedFile::parse(&text, crate::parser::Language::Rust).is_some(),
                "clean merge is unparseable (silent corruption):\n{text}"
            );
            text
        }
        other => panic!("unexpected outcome: {other:?}"),
    };
    assert!(text.contains("fn a(&self)"), "fn a lost:\n{text}");
    assert!(text.contains("fn b(&self)"), "fn b lost:\n{text}");
}

#[test]
fn repro_484_r3_ambiguous_block_merge_no_silent_collapse() {
    // base has TWO `impl Foo` blocks. ours MERGES them into one (holding both
    // a and b) AND edits a; theirs edits a differently and keeps two blocks.
    // Never a silent collapse: conflict, or a clean re-parsing merge with both
    // methods present.
    let base = "struct Foo;\n\nimpl Foo {\n    fn a(&self) -> u32 { 0 }\n}\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    let ours = "struct Foo;\n\nimpl Foo {\n    fn a(&self) -> u32 { 9 }\n    fn b(&self) {}\n}\n";
    let theirs = "struct Foo;\n\nimpl Foo {\n    fn a(&self) -> u32 { 1 }\n}\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    match merge_rust(base, ours, theirs) {
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            conflict_count,
        } => {
            let text = String::from_utf8(merged_bytes_with_markers).unwrap();
            assert!(conflict_count >= 1, "expected a real conflict");
            assert!(text.contains("fn a(&self)"), "fn a lost:\n{text}");
            assert!(text.contains("fn b(&self)"), "fn b lost:\n{text}");
        }
        MergeOutcome::Clean(bytes) => {
            let text = String::from_utf8(bytes).unwrap();
            assert!(
                crate::parser::ParsedFile::parse(&text, crate::parser::Language::Rust).is_some(),
                "clean merge is unparseable:\n{text}"
            );
            assert!(
                text.contains("fn a(&self)") && text.contains("fn b(&self)"),
                "clean merge dropped a block:\n{text}"
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[test]
fn repro_484_r3_ambiguous_block_split_no_silent_collapse() {
    // Mirror: base has ONE `impl Foo` holding a and b. ours SPLITS it into two
    // and edits a; theirs edits a differently in the single block. No silent
    // collapse.
    let base = "struct Foo;\n\nimpl Foo {\n    fn a(&self) -> u32 { 0 }\n    fn b(&self) {}\n}\n";
    let ours = "struct Foo;\n\nimpl Foo {\n    fn a(&self) -> u32 { 9 }\n}\n\nimpl Foo {\n    fn b(&self) {}\n}\n";
    let theirs = "struct Foo;\n\nimpl Foo {\n    fn a(&self) -> u32 { 1 }\n    fn b(&self) {}\n}\n";
    let text = match merge_rust(base, ours, theirs) {
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            conflict_count,
        } => {
            assert!(conflict_count >= 1);
            String::from_utf8(merged_bytes_with_markers).unwrap()
        }
        MergeOutcome::Clean(bytes) => {
            let text = String::from_utf8(bytes).unwrap();
            assert!(
                crate::parser::ParsedFile::parse(&text, crate::parser::Language::Rust).is_some(),
                "clean merge unparseable:\n{text}"
            );
            text
        }
        other => panic!("unexpected outcome: {other:?}"),
    };
    assert!(text.contains("fn a(&self)"), "fn a lost:\n{text}");
    assert!(text.contains("fn b(&self)"), "fn b lost:\n{text}");
}

// =====================================================================
// heddle#490 r1 / Codex P1: ZERO child-overlap container alignment.
//
// The container matcher (build_aligned_match_keys) picks a base block by
// immediate child-key OVERLAP. An EMPTY container (or one whose children were
// fully replaced) overlaps nothing, so pre-fix every such block minted a FRESH
// discriminator and its base slot resolved as deleted. The positional fallback
// aligns a zero-overlap block to its next unused base slot in source order;
// content-overlap stays the PRIMARY mechanism (covered above).
//
// The additive cases below are CONTRACT guards: the add/add weaving already
// reconstructs them byte-for-byte even pre-fix, so they pin the clean-merge
// contract for empty/zero-overlap containers against future regressions rather
// than witnessing the pre-fix defect. The observable fail-pre/pass-post witness
// is `..._modify_vs_delete_empty_container_conflicts`: pre-fix the deleted base
// slot let a modify-vs-delete merge silently resolve to one side (data loss);
// the positional fallback routes it through the same modify/delete CONFLICT path
// a non-empty container already uses.
// =====================================================================

#[test]
fn repro_490_r1_two_empty_impl_blocks_one_side_adds_method_clean() {
    // Two EMPTY `impl Foo {}` blocks in base; ours adds a method to the FIRST,
    // theirs is unchanged. The first block must gain the method, the second
    // stay empty — a clean one-sided edit with no drop/dup/conflict.
    let base = "struct Foo;\n\nimpl Foo {}\n\nimpl Foo {}\n";
    let ours = "struct Foo;\n\nimpl Foo {\n    fn m(&self) {}\n}\n\nimpl Foo {}\n";
    let theirs = base;
    let merged = assert_conformant(base, ours, theirs);
    assert_eq!(
        merged.matches("impl Foo").count(),
        2,
        "empty blocks dropped/duplicated:\n{merged}"
    );
    assert!(
        merged.contains("fn m(&self) {}"),
        "ours add lost:\n{merged}"
    );
    // The method lands in the FIRST block; the second stays empty.
    let first = merged.find("impl Foo").unwrap();
    let second = merged.rfind("impl Foo").unwrap();
    let m = merged.find("fn m(&self) {}").unwrap();
    assert!(
        first < m && m < second,
        "method escaped the first block:\n{merged}"
    );
}

#[test]
fn repro_490_r1_two_empty_impl_blocks_each_side_edits_different_block_clean() {
    // Two empty blocks; ours edits the FIRST, theirs edits the SECOND. Both
    // edits land on their own base slot — a clean merge of both.
    let base = "struct Foo;\n\nimpl Foo {}\n\nimpl Foo {}\n";
    let ours = "struct Foo;\n\nimpl Foo {\n    fn ours_m(&self) {}\n}\n\nimpl Foo {}\n";
    let theirs = "struct Foo;\n\nimpl Foo {}\n\nimpl Foo {\n    fn theirs_m(&self) {}\n}\n";
    let merged = assert_conformant(base, ours, theirs);
    assert_eq!(
        merged.matches("impl Foo").count(),
        2,
        "empty blocks dropped/duplicated:\n{merged}"
    );
    assert!(
        merged.contains("fn ours_m(&self) {}"),
        "ours edit lost:\n{merged}"
    );
    assert!(
        merged.contains("fn theirs_m(&self) {}"),
        "theirs edit lost:\n{merged}"
    );
    assert!(
        merged.find("fn ours_m(&self) {}").unwrap() < merged.find("fn theirs_m(&self) {}").unwrap(),
        "edits landed in the wrong blocks:\n{merged}"
    );
}

#[test]
fn repro_490_r1_fully_replaced_impl_children_align_to_base_slot_no_conflict() {
    // A non-empty container whose children are FULLY replaced on ours (child
    // overlap with base = 0) must still align to its single base slot, not be
    // treated as delete + add. base holds `fn x`; ours replaces it with `fn y`;
    // theirs keeps `fn x` and adds `fn z`. Positional alignment recurses the
    // body and combines the edits cleanly (ours's deletion of x + add of y,
    // theirs's add of z). Contract guard for the zero-overlap fallback.
    let base = "struct Foo;\n\nimpl Foo {\n    fn x(&self) {}\n}\n";
    let ours = "struct Foo;\n\nimpl Foo {\n    fn y(&self) {}\n}\n";
    let theirs = "struct Foo;\n\nimpl Foo {\n    fn x(&self) {}\n    fn z(&self) {}\n}\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(
        crate::parser::ParsedFile::parse(&merged, crate::parser::Language::Rust).is_some(),
        "unparseable merge:\n{merged}"
    );
    assert_eq!(
        merged.matches("impl Foo").count(),
        1,
        "block duplicated:\n{merged}"
    );
    assert!(
        merged.contains("fn y(&self) {}"),
        "ours replacement lost:\n{merged}"
    );
    assert!(
        merged.contains("fn z(&self) {}"),
        "theirs add lost:\n{merged}"
    );
    assert!(
        !merged.contains("fn x(&self) {}"),
        "ours deletion of fn x dropped:\n{merged}"
    );
}

#[test]
fn repro_490_r1_fully_replaced_mod_children_align_to_base_slot_no_conflict() {
    // `mod` variant of the fully-replaced case (single block, valid Rust): base
    // `mod m { fn x }`; ours replaces the body with `fn y` (zero child-overlap);
    // theirs keeps `fn x` and adds `fn z`. Positional alignment keeps the one
    // base slot so the bodies merge instead of conflicting.
    let base = "mod m {\n    fn x() {}\n}\n";
    let ours = "mod m {\n    fn y() {}\n}\n";
    let theirs = "mod m {\n    fn x() {}\n    fn z() {}\n}\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(
        crate::parser::ParsedFile::parse(&merged, crate::parser::Language::Rust).is_some(),
        "unparseable merge:\n{merged}"
    );
    assert_eq!(
        merged.matches("mod m").count(),
        1,
        "mod duplicated:\n{merged}"
    );
    assert!(
        merged.contains("fn y() {}"),
        "ours replacement lost:\n{merged}"
    );
    assert!(merged.contains("fn z() {}"), "theirs add lost:\n{merged}");
    assert!(
        !merged.contains("fn x() {}"),
        "ours deletion of fn x dropped:\n{merged}"
    );
}

#[cfg(feature = "lang-cpp")]
#[test]
fn repro_490_r1_two_empty_cpp_namespaces_one_side_adds_clean() {
    // C++ `namespace` variant of the two-empty-blocks case: base has two empty
    // `namespace N {}` blocks; ours adds a function to the FIRST, theirs is
    // unchanged. Clean one-sided edit, both namespaces preserved.
    let base = "namespace N {}\n\nnamespace N {}\n";
    let ours = "namespace N {\nvoid m() {}\n}\n\nnamespace N {}\n";
    let theirs = base;
    let merged = assert_conformant_at(base, ours, theirs, "f.cpp", crate::parser::Language::Cpp);
    assert_eq!(
        merged.matches("namespace N").count(),
        2,
        "empty namespaces dropped/duplicated:\n{merged}"
    );
    assert!(merged.contains("void m() {}"), "ours add lost:\n{merged}");
    let first = merged.find("namespace N").unwrap();
    let second = merged.rfind("namespace N").unwrap();
    let m = merged.find("void m() {}").unwrap();
    assert!(
        first < m && m < second,
        "function escaped the first namespace:\n{merged}"
    );
}

#[test]
fn repro_490_r1_modify_vs_delete_empty_container_conflicts() {
    // The observable fail-pre/pass-post witness. base has one EMPTY `impl Foo {}`.
    // ours adds a method to it (a modify); theirs deletes the whole block. This
    // is a textbook modify/delete and MUST conflict — exactly as a non-empty
    // container's modify/delete already does (resolve_container's Some/Some/None
    // arm). Pre-fix the empty block's zero overlap minted a fresh discriminator,
    // so the base slot resolved as a clean delete on both sides and ours's
    // modification was re-added as a brand-new block: the conflict vanished and
    // ours's content silently won (data loss + a trailing-whitespace artifact).
    // The positional fallback claims the base slot, so the modify/delete is seen
    // and conflicts.
    let base = "struct Foo;\n\nimpl Foo {}\n";
    let ours = "struct Foo;\n\nimpl Foo {\n    fn m(&self) {}\n}\n";
    let theirs = "struct Foo;\n";
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(count >= 1, "expected a modify/delete conflict, got {count}");
    assert!(
        text.contains("fn m(&self) {}"),
        "ours modification lost from conflict:\n{text}"
    );
    assert!(
        text.contains("<<<<<<<") && text.contains(">>>>>>>"),
        "missing conflict markers:\n{text}"
    );
}

// =====================================================================
// heddle#490 r2 / Codex P1: zero-overlap container alignment must respect
// the TRUE base OCCURRENCE, not "first unused slot".
//
// r1's positional fallback claimed the next UNUSED base discriminator in
// source order. When a side DELETES an earlier same-key zero-overlap container
// and edits a later one, that grabs slot 0 for the survivor instead of its true
// base occurrence (1): base slot 1 is then treated as deleted and its
// separator/leading trivia woven onto the wrong block (moved/duplicated), or a
// spurious conflict is raised. The fix aligns a zero-overlap block to the base
// candidate whose HEADER bytes (which absorb leading metadata — separator
// comments, attributes) byte-match it, run as a PRIORITY pass before the
// source-order scan so a surviving block re-anchors to its true slot even under
// delete-earlier / prepend / reorder. The class backstop is the input-grounded
// conservation floor (`conserves_inputs`): any residual mis-alignment that
// drops/invents/mis-nests an item, or fails to re-parse, routes to a textual
// CONFLICT rather than silent corruption (floor contract guards below).
// =====================================================================

#[test]
fn repro_490_r2_delete_earlier_edit_later_zero_overlap_separator_not_moved() {
    // The r2 fail-pre/pass-post witness. base: two EMPTY `impl Foo {}` blocks
    // separated by a comment, which binds as the SECOND block's leading
    // metadata. ours DELETES the first block and adds a method to the second;
    // theirs edits the separator comment. The survivor must keep its identity as
    // base's SECOND occurrence so ours's method and theirs's separator edit
    // combine cleanly, the first block is deleted, and the separator is neither
    // moved onto the wrong block nor duplicated. Pre-fix the survivor (zero
    // child overlap) claimed the earliest unused base slot (0); base slot 1 was
    // treated as deleted and its separator wove onto the wrong block — a
    // spurious conflict. Header-anchored alignment re-pins the survivor to slot
    // 1 by its `// sep` leading metadata.
    let base = "impl Foo {}\n// sep\nimpl Foo {}\n";
    let ours = "// sep\nimpl Foo {\n    fn m(&self) {}\n}\n";
    let theirs = "impl Foo {}\n// edited sep\nimpl Foo {}\n";
    let merged = assert_conformant(base, ours, theirs);
    assert_eq!(
        merged.matches("impl Foo").count(),
        1,
        "block dropped/duplicated:\n{merged}"
    );
    assert!(
        merged.contains("fn m(&self) {}"),
        "ours method lost:\n{merged}"
    );
    assert!(
        merged.contains("// edited sep"),
        "theirs separator edit lost:\n{merged}"
    );
    assert_eq!(
        merged.matches("sep").count(),
        1,
        "separator moved/duplicated:\n{merged}"
    );
}

#[test]
fn repro_490_r2_prepend_zero_overlap_keeps_survivor_base_slot() {
    // Add-earlier variant. ours PREPENDS a new empty same-key container above the
    // base block; theirs edits the base block. The prepended block must take a
    // FRESH slot while the surviving base block keeps slot 0 — matched by its
    // `// orig` leading comment. Header anchoring runs before the source-order
    // scan, so the new (header-mismatched) block can't greedily steal slot 0.
    let base = "// orig\nimpl Foo {}\n";
    let ours = "impl Foo {}\n\n// orig\nimpl Foo {}\n";
    let theirs = "// orig\nimpl Foo {\n    fn m(&self) {}\n}\n";
    let merged = assert_conformant(base, ours, theirs);
    assert_eq!(
        merged.matches("impl Foo").count(),
        2,
        "block dropped/duplicated:\n{merged}"
    );
    assert!(
        merged.contains("fn m(&self) {}"),
        "theirs method lost:\n{merged}"
    );
    // The method lands on the `// orig` (base) block; the comment survives once.
    assert_eq!(
        merged.matches("// orig").count(),
        1,
        "comment moved/duplicated:\n{merged}"
    );
    assert!(
        merged.find("// orig").unwrap() < merged.find("fn m(&self) {}").unwrap(),
        "method escaped the original block:\n{merged}"
    );
}

#[test]
fn repro_490_r2_reorder_zero_overlap_blocks_keep_identity_by_header() {
    // Reorder variant. Two empty same-key blocks distinguished only by their
    // leading comments. ours SWAPS their order; theirs adds a method to the
    // `// a` block. Header anchoring keeps each block's identity through the
    // reorder, so theirs's method lands on the `// a` block and nothing is
    // dropped, duplicated, or mis-merged.
    let base = "// a\nimpl Foo {}\n\n// b\nimpl Foo {}\n";
    let ours = "// b\nimpl Foo {}\n\n// a\nimpl Foo {}\n";
    let theirs = "// a\nimpl Foo {\n    fn am(&self) {}\n}\n\n// b\nimpl Foo {}\n";
    let merged = assert_conformant(base, ours, theirs);
    assert_eq!(
        merged.matches("impl Foo").count(),
        2,
        "block dropped/duplicated:\n{merged}"
    );
    assert!(
        merged.contains("fn am(&self) {}"),
        "theirs method lost:\n{merged}"
    );
    // The method stays bound to the `// a` block, not `// b`.
    assert!(
        merged.find("// a").unwrap() < merged.find("fn am(&self) {}").unwrap()
            && merged.find("fn am(&self) {}").unwrap() < merged.find("// b").unwrap(),
        "method bound to the wrong block:\n{merged}"
    );
}

// ---------------------------------------------------------------------
// heddle#490 floor contract guards. `conserves_inputs` is the input-grounded
// output-boundary backstop: after a CLEAN reconstruction it re-parses the
// output and checks item-set + nesting conservation against the RAW INPUTS
// (never the merge's own resolved metadata, which could encode the very
// mistake). A re-parse failure or a conservation violation routes the merge to
// the textual conflict path — so a residual mis-alignment surfaces as a CONFLICT
// the user resolves, never a silent collapse. These pin its contract directly;
// the "no false positives, all correct merges stay clean byte-identical"
// guarantee is enforced by the whole ported harness above staying green.
// ---------------------------------------------------------------------

#[test]
fn floor_490_conserves_inputs_rejects_unparseable_output() {
    use crate::parser::{Language, ParsedFile};
    let base = ParsedFile::parse("fn a() {}\n", Language::Rust).unwrap();
    let ours = ParsedFile::parse("fn a() {}\n\nfn b() {}\n", Language::Rust).unwrap();
    let theirs = ParsedFile::parse("fn a() {}\n", Language::Rust).unwrap();
    // Unbalanced braces — the shape a structural collapse would emit.
    let corrupt = b"fn a() { fn b() {\n";
    assert!(
        !super::conserves_inputs(corrupt, Language::Rust, &base, &ours, &theirs),
        "floor must reject an unparseable clean output"
    );
}

#[test]
fn floor_490_conserves_inputs_rejects_invented_or_misnested_item() {
    use crate::parser::{Language, ParsedFile};
    // Every input keeps `m` INSIDE `impl Foo` (identity [Foo], Fn, m). An output
    // that floats `m` to the top level invents identity ([], Fn, m) no input
    // had — a child escaping its container. The floor must reject it.
    let src = "impl Foo {\n    fn m(&self) {}\n}\n";
    let base = ParsedFile::parse(src, Language::Rust).unwrap();
    let ours = ParsedFile::parse(src, Language::Rust).unwrap();
    let theirs = ParsedFile::parse(src, Language::Rust).unwrap();
    let misnested = b"fn m(&self) {}\n\nimpl Foo {}\n";
    assert!(
        !super::conserves_inputs(misnested, Language::Rust, &base, &ours, &theirs),
        "floor must reject an item escaped to a scope no input had"
    );
}

#[test]
fn floor_490_conserves_inputs_allows_legitimate_deletion() {
    use crate::parser::{Language, ParsedFile};
    // A clean one-sided deletion: ours drops `b`. The floor is a SUBSET relation
    // (not equality), so the deletion is allowed — no false positive.
    let base = ParsedFile::parse("fn a() {}\n\nfn b() {}\n", Language::Rust).unwrap();
    let ours = ParsedFile::parse("fn a() {}\n", Language::Rust).unwrap();
    let theirs = ParsedFile::parse("fn a() {}\n\nfn b() {}\n", Language::Rust).unwrap();
    let deleted = b"fn a() {}\n";
    assert!(
        super::conserves_inputs(deleted, Language::Rust, &base, &ours, &theirs),
        "floor must accept a clean deletion"
    );
}

#[test]
fn floor_490_conserves_inputs_allows_within_line_edit_recombination() {
    use crate::parser::{Language, ParsedFile};
    // A within-line merge that recombines bytes from both sides. The floor keys
    // on item IDENTITY, not line text, so the recombined body is allowed.
    let base = ParsedFile::parse("fn a() { let x = 1; }\n", Language::Rust).unwrap();
    let ours = ParsedFile::parse("fn a() { let x = 2; }\n", Language::Rust).unwrap();
    let theirs = ParsedFile::parse("fn a() { let y = 1; }\n", Language::Rust).unwrap();
    let edited = b"fn a() { let x = 2; let y = 1; }\n";
    assert!(
        super::conserves_inputs(edited, Language::Rust, &base, &ours, &theirs),
        "floor must accept an edit recombination"
    );
}

// =====================================================================
// heddle#490 r3 / Codex P2: MIXED container/leaf matched item must NEVER
// enter the structural path (a PANIC class, now closed).
//
// The same `ItemKey` (kind, name, scope) can name a container on one side and a
// LEAF on another, because two syntactic forms share a key:
//   * Python: `class C` (container, `class_definition`) wrapped in a decorator
//     becomes a `decorated_definition` whose `container_body` is forced to
//     `None` (a leaf) while inheriting the inner class's key.
//   * Rust: `mod foo { … }` (container) rewritten to `mod foo;` (a leaf, no body)
//     keeps the same module key.
// Pre-fix `resolve_node` decided container-vs-leaf from the FIRST present side,
// so a base/theirs container routed the matched node into `resolve_container` →
// `merge_container_3way`, which unwrapped the leaf side's missing `body` via
// `header_bytes(...).expect("container")` → PANIC. The fix enters the structural
// path ONLY when EVERY present side is a container with a body; any mixed /
// missing-body case routes to a whole-item 3-way text merge (clean if disjoint,
// conflict if overlapping) — never panics. These are the fail-pre (panic) /
// pass-post witnesses, plus the language-mirrored sibling that closes the class.
// =====================================================================

#[test]
fn repro_490_r3_python_class_to_decorated_mixed_kind_no_panic_clean() {
    // base: `class C` with a method (container). ours adds a decorator above it,
    // which tree-sitter wraps in a `decorated_definition` → a LEAF carrying the
    // same module key (no body). theirs edits the method body (container). The
    // decorator-add and the body-edit are DISJOINT lines, so the whole-item text
    // merge is clean: both the decorator and theirs's edit survive. Pre-fix this
    // panicked in `merge_container_3way` (leaf side has no body to unwrap).
    let base = "\
class C:
    def m(self):
        return 1
";
    let ours = "\
@deco
class C:
    def m(self):
        return 1
";
    let theirs = "\
class C:
    def m(self):
        return 2
";
    let merged = match merge_at(base, ours, theirs, "f.py") {
        MergeOutcome::Clean(b) => String::from_utf8(b).unwrap(),
        other => panic!("expected Clean, got {other:?}"),
    };
    assert!(merged.contains("@deco"), "ours decorator lost:\n{merged}");
    assert!(
        merged.contains("return 2"),
        "theirs body edit lost:\n{merged}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "unexpected conflict markers:\n{merged}"
    );
}

#[test]
fn repro_490_r3_python_class_to_decorated_mixed_kind_overlap_conflicts() {
    // Same mixed container/leaf shape, but ours's decorator-add and theirs's edit
    // to the SAME method line overlap in the whole-item text merge → a normal
    // conflict (NOT a panic, NOT a silent drop). ours decorates AND edits the
    // body to `return 10`; theirs edits the body to `return 20`.
    let base = "\
class C:
    def m(self):
        return 1
";
    let ours = "\
@deco
class C:
    def m(self):
        return 10
";
    let theirs = "\
class C:
    def m(self):
        return 20
";
    let (text, count) = assert_conflicts(merge_at(base, ours, theirs, "f.py"));
    assert!(count >= 1, "expected a conflict, got {count}");
    assert!(
        text.contains("<<<<<<<") && text.contains(">>>>>>>"),
        "missing conflict markers:\n{text}"
    );
    // Both sides' divergent bodies are represented in the conflict, none lost.
    assert!(
        text.contains("return 10"),
        "ours side lost from conflict:\n{text}"
    );
    assert!(
        text.contains("return 20"),
        "theirs side lost from conflict:\n{text}"
    );
}

#[test]
fn repro_490_r3_rust_mod_block_to_decl_mixed_kind_no_panic() {
    // Rust sibling of the class. base: `mod foo { fn a() {} }` (container). ours
    // rewrites it to `mod foo;` (a LEAF, no body, same module key). theirs edits
    // inside the block (container). The kind-mismatch routes to a whole-item text
    // merge; ours's whole-block replacement overlaps theirs's in-block edit → a
    // normal conflict. The contract that matters: NO panic, and a deterministic
    // MergeOutcome. Pre-fix `merge_container_3way` unwrapped ours's missing body.
    let base = "mod foo {\n    fn a() {}\n}\n";
    let ours = "mod foo;\n";
    let theirs = "mod foo {\n    fn a() {\n        let x = 1;\n    }\n}\n";
    // The assertion here is that we get SOME outcome without panicking; the
    // overlapping change resolves as a conflict.
    let (text, count) = assert_conflicts(merge_rust(base, ours, theirs));
    assert!(count >= 1, "expected a conflict, got {count}");
    assert!(
        text.contains("<<<<<<<") && text.contains(">>>>>>>"),
        "missing conflict markers:\n{text}"
    );
    // Both forms appear inside the conflict block — `mod foo;` (ours) and the
    // edited block body (theirs) — so neither side is silently dropped.
    assert!(
        text.contains("mod foo;"),
        "ours leaf form lost from conflict:\n{text}"
    );
    assert!(
        text.contains("let x = 1;"),
        "theirs edit lost from conflict:\n{text}"
    );
}

#[test]
fn repro_490_r3_rust_mod_decl_to_block_mixed_kind_disjoint_clean() {
    // Mirror direction with a DISJOINT edit so the whole-item text merge stays
    // clean. base: `mod foo;` (leaf). ours expands it to a block with a fn
    // (container); theirs leaves it untouched. With theirs == base, the
    // unchanged-side-defers rule takes ours wholesale — no panic, clean, ours's
    // expansion preserved.
    let base = "mod foo;\n";
    let ours = "mod foo {\n    fn a() {}\n}\n";
    let theirs = "mod foo;\n";
    let merged = assert_clean(merge_rust(base, ours, theirs));
    assert!(
        merged.contains("fn a() {}"),
        "ours expansion lost:\n{merged}"
    );
    assert!(
        !merged.contains("<<<<<<<"),
        "unexpected conflict markers:\n{merged}"
    );
}

// C++ `namespace N {…} namespace N {…}` is the same close-the-class shape in
// another language; gated behind `lang-cpp` (extended-languages) since the cpp
// parser isn't in the default feature set. The alignment is language-agnostic
// and also covered by the default-feature Rust cases above.
#[cfg(feature = "lang-cpp")]
fn namespace_reopen_triple(sep: &str) -> (String, String, String) {
    let base = format!("namespace N {{\nvoid a() {{}}\n}}{sep}namespace N {{\nvoid b() {{}}\n}}\n");
    let ours = format!(
        "namespace N {{\nvoid a() {{}}\nvoid a2() {{}}\n}}{sep}namespace N {{\nvoid b() {{}}\n}}\n"
    );
    let theirs = format!(
        "namespace N {{\nvoid a() {{}}\n}}{sep}namespace N {{\nvoid b() {{}}\nvoid b2() {{}}\n}}\n"
    );
    (base, ours, theirs)
}

#[cfg(feature = "lang-cpp")]
#[test]
fn conformance_484_reopened_cpp_namespace_separator_matrix() {
    let cases: &[(&str, &str)] = &[
        ("back-to-back", "\n"),
        ("whitespace-separated", "\n\n"),
        ("comment-separated", "\n// section\n"),
        ("item-separated", "\n\nint top_level() { return 0; }\n\n"),
    ];
    for (label, sep) in cases {
        let (base, ours, theirs) = namespace_reopen_triple(sep);
        let merged =
            assert_conformant_at(&base, &ours, &theirs, "f.cpp", crate::parser::Language::Cpp);
        assert_eq!(
            merged.matches("namespace N").count(),
            2,
            "[{label}] reopened namespaces collapsed:\n{merged}"
        );
        assert!(
            merged.contains("void a2() {}"),
            "[{label}] ours add lost:\n{merged}"
        );
        assert!(
            merged.contains("void b2() {}"),
            "[{label}] theirs add lost:\n{merged}"
        );
        let second_ns = merged.rfind("namespace N").unwrap();
        assert!(
            merged.find("void a2() {}").unwrap() < second_ns,
            "[{label}] a2 escaped first block:\n{merged}"
        );
        assert!(
            merged.find("void b2() {}").unwrap() > second_ns,
            "[{label}] b2 escaped second block:\n{merged}"
        );
        if sep.contains("// section") {
            assert_eq!(
                merged.matches("// section").count(),
                1,
                "[{label}] comment separator dropped/duplicated:\n{merged}"
            );
        }
        if sep.contains("top_level") {
            let first_ns = merged.find("namespace N").unwrap();
            let top = merged.find("int top_level()").unwrap();
            assert!(
                first_ns < top && top < second_ns,
                "[{label}] top-level item must stay between blocks:\n{merged}"
            );
        }
    }
}

#[cfg(feature = "lang-cpp")]
#[test]
fn conformance_484_r3_cross_side_cpp_namespace_prepend_no_collapse() {
    // base has one `namespace N { void b(); }`. ours prepends a NEW
    // `namespace N { void a(); }`; theirs edits b's body.
    let base = "namespace N {\nvoid b() {}\n}\n";
    let ours = "namespace N {\nvoid a() {}\n}\n\nnamespace N {\nvoid b() {}\n}\n";
    let theirs = "namespace N {\nvoid b() { int x = 1; }\n}\n";
    let merged = assert_conformant_at(base, ours, theirs, "f.cpp", crate::parser::Language::Cpp);
    assert_eq!(
        merged.matches("namespace N").count(),
        2,
        "prepended namespace collapsed into base block:\n{merged}"
    );
    assert!(merged.contains("void a() {}"), "ours add lost:\n{merged}");
    assert!(
        merged.contains("void b() { int x = 1; }"),
        "theirs edit lost:\n{merged}"
    );
    assert!(
        merged.find("void a()").unwrap() < merged.find("void b()").unwrap(),
        "prepended block must precede the base block:\n{merged}"
    );
}
