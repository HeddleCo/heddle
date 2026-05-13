// SPDX-License-Identifier: Apache-2.0
//! Every state-changing dispatch arm in [`crates/cli/src/main.rs`] must
//! call [`cli::operation_id::resolve_operation_id`].
//!
//! The dedup contract is wire-only without it: an agent passing
//! `--op-id` to a verb whose arm forgets to plumb the call would silently
//! lose idempotency. CI fails this test when a new verb is introduced
//! without explicit classification.
//!
//! The check is intentionally text-based (no `syn` dep): every match arm
//! `Commands::<Variant>(...) => { ... }` is parsed by a small balanced-
//! brace scanner and the body is grep-asserted for the canonical helper
//! name. Read-only verbs are an explicit allowlist below; everything else
//! is treated as state-changing. Adding a verb to either list is a
//! deliberate decision — and one a reviewer can spot at a glance.

use std::{collections::BTreeSet, path::PathBuf};

/// Variants whose arms are read-only — no oplog mutation under any
/// subcommand. Adding a verb here is a permission to skip op-id wiring;
/// removing one immediately requires a `resolve_operation_id` call.
const READ_ONLY_VARIANTS: &[&str] = &[
    "Status",
    "Watch",
    // Codex git-overlay foundation added these read-only surfaces.
    "Doctor",
    "GitOverlay",
    "Version",
    "Log",
    "Show",
    "Inspect",
    "Diff",
    "Compare",
    "Blame",
    "Completion",
    "Help",
    "Index",
    "Monitor",
    "Query",
    "Presence",
    "HarnessBridge",
    "Semantic",
    // Engineering retrospective: walks recent oplog batches and prints
    // a summary. Pure read — never writes a new OpRecord.
    "Retro",
    // Schema registry: prints the JSON schemas embedded in the CLI
    // and the registry index. No oplog mutation.
    "Schemas",
];

#[test]
fn every_state_changing_arm_resolves_op_id() {
    let main_rs = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("main.rs");
    let source = std::fs::read_to_string(&main_rs)
        .unwrap_or_else(|e| panic!("read {}: {e}", main_rs.display()));

    let arms = extract_command_arms(&source);
    assert!(
        !arms.is_empty(),
        "no Commands::<Variant> arms found in {} — has the dispatch shape changed?",
        main_rs.display()
    );

    let read_only: BTreeSet<&str> = READ_ONLY_VARIANTS.iter().copied().collect();

    let mut missing = Vec::new();
    let mut classified = BTreeSet::new();
    for arm in &arms {
        classified.insert(arm.variant.as_str());
        if read_only.contains(arm.variant.as_str()) {
            continue;
        }
        if !arm.body.contains("resolve_operation_id(") {
            missing.push(arm.variant.clone());
        }
    }

    assert!(
        missing.is_empty(),
        "the following Commands variants are state-changing but their arm in main.rs \
         doesn't call `resolve_operation_id(&cli)?`: {missing:?}.\n\
         Either wire the call (add `resolve_operation_id(&cli)?;` at the top of the \
         arm body) or, if the verb is genuinely read-only, add it to \
         READ_ONLY_VARIANTS in this test."
    );

    // Catch dangling allowlist entries — if a verb is renamed or removed
    // from main.rs and we forget to drop it from READ_ONLY_VARIANTS, the
    // allowlist silently loses meaning.
    for ro in READ_ONLY_VARIANTS {
        assert!(
            classified.contains(*ro),
            "READ_ONLY_VARIANTS lists `{ro}` but no Commands::{ro} arm was found in \
             main.rs — drop the entry or update the variant name."
        );
    }
}

#[derive(Debug)]
struct Arm {
    variant: String,
    body: String,
}

/// Extract every `Commands::<Variant>` arm from the dispatch match in
/// main.rs. Tolerates `cfg(...)` attributes between arms and arms whose
/// body is either an expression (`=> cmd_foo(...),`) or a block
/// (`=> { ... }`).
///
/// We scope to the dispatch match (`match &cli.command { ... }`) by
/// finding it explicitly — this avoids picking up the arms in the
/// `command_name` helper at the bottom of main.rs, which return string
/// literals and never call `resolve_operation_id`.
fn extract_command_arms(source: &str) -> Vec<Arm> {
    let bytes = source.as_bytes();
    let mut arms = Vec::new();

    // Locate `match &cli.command {`, then walk inside its braces.
    let dispatch_marker = "match &cli.command";
    let dispatch_start = source
        .find(dispatch_marker)
        .expect("main.rs must contain a `match &cli.command` dispatch");
    let dispatch_open_brace = dispatch_start
        + dispatch_marker.len()
        + source[dispatch_start + dispatch_marker.len()..]
            .find('{')
            .expect("dispatch match must have an opening brace");
    let dispatch_close = match_close_brace(bytes, dispatch_open_brace)
        .expect("dispatch match must have a balanced closing brace");
    let scope_end = dispatch_close;

    let needle = "Commands::";
    let mut cursor = dispatch_open_brace + 1;
    while cursor < scope_end {
        let Some(rel) = source[cursor..scope_end].find(needle) else {
            break;
        };
        let start = cursor + rel;
        cursor = start + needle.len();

        // Word-boundary check: skip when preceded by an identifier char
        // (e.g. `ContextCommands::`, `ActorCommands::`, `SessionCommands::`).
        if start > 0 {
            let prev = bytes[start - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }

        let variant = read_variant_name(&source[cursor..]);
        if variant.is_empty() {
            continue;
        }

        // Walk to the `=>` token — bail if we don't find one before the
        // next match arm or a closing brace at depth 0 from `cursor`.
        let after_variant = cursor + variant.len();
        let Some(arrow_off) = find_match_arrow(&source[after_variant..]) else {
            continue;
        };
        let arrow = after_variant + arrow_off;
        let body_start = arrow + 2;

        // Body is either a block `{ ... }` or an expression terminated by
        // a comma at depth 0 (or end of file).
        let body_text =
            if let Some(brace_off) = first_non_whitespace_is_brace(&source[body_start..]) {
                // Block arm — find matching close brace.
                let block_open = body_start + brace_off;
                let close = match_close_brace(bytes, block_open).unwrap_or(bytes.len());
                source[block_open..=close.min(bytes.len() - 1)].to_string()
            } else {
                // Expression arm — read until depth-0 comma.
                let end = expr_arm_end(&source[body_start..]);
                source[body_start..body_start + end].to_string()
            };

        arms.push(Arm {
            variant: variant.to_string(),
            body: body_text,
        });
    }
    arms
}

fn read_variant_name(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

/// Find the next `=>` that's clearly part of this arm's pattern. We
/// ignore `=>` that appears nested inside parens/braces (paranoid
/// defense; clap arms typically don't have them).
fn find_match_arrow(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth_paren: i32 = 0;
    let mut depth_brace: i32 = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        match bytes[i] {
            b'(' => depth_paren += 1,
            b')' => depth_paren -= 1,
            b'{' => depth_brace += 1,
            b'}' => {
                if depth_brace == 0 {
                    return None;
                }
                depth_brace -= 1;
            }
            b'=' if bytes[i + 1] == b'>' && depth_paren == 0 && depth_brace == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn first_non_whitespace_is_brace(s: &str) -> Option<usize> {
    for (i, c) in s.char_indices() {
        if c.is_whitespace() {
            continue;
        }
        return if c == '{' { Some(i) } else { None };
    }
    None
}

fn match_close_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn expr_arm_end(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut depth_paren: i32 = 0;
    let mut depth_brace: i32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth_paren += 1,
            b')' => depth_paren -= 1,
            b'{' => depth_brace += 1,
            b'}' => depth_brace -= 1,
            b',' if depth_paren == 0 && depth_brace == 0 => return i,
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}
