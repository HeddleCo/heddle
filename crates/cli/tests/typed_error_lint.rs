// SPDX-License-Identifier: Apache-2.0
//! Count-based regression lint for untyped error sites.
//!
//! Persona feedback (Priya, Agent): generic `runtime_error` envelopes
//! with bare-string error messages aren't actionable. We're migrating
//! the long tail to typed `RecoveryAdvice` variants in PR C-3, then
//! progressively from there. This test pins the count so a new
//! contributor adding an `anyhow!("…")` to a command path triggers a
//! visible regression in CI — they can either route through a typed
//! variant (preferred) or, with a justification, bump the constant.
//!
//! Strategy:
//! - Walk `crates/cli/src/cli/commands/` (excluding `tests/` and
//!   `#[cfg(test)]` items).
//! - Count call sites of `anyhow!("…")` / `anyhow::anyhow!("…")` /
//!   `bail!("…")` / `anyhow::bail!("…")` whose first argument is a
//!   string literal or `format!(…)` (not a `RecoveryAdvice::*` or
//!   helper ending in `_advice` / `_refusal`).
//! - Assert `count <= MAX_UNTYPED_ANYHOW_SITES`.
//!
//! When a PR migrates a site to typed advice, lower the constant.
//! When a PR genuinely needs a new untyped site (rare — almost
//! everything benefits from typed), bump the constant in the same
//! PR with a one-line `#[allow] // reason: …` comment above it.

use std::{fs, path::Path};

/// Baseline as measured by this test's own detector.
///
/// The PR C-0 audit reported 237 via a broader regex; this test's
/// detector is narrower (requires literal `anyhow!("…"`, `bail!("…"`,
/// or `anyhow!(format!(`), which is what we actually want to police.
/// 172 was the count the narrow detector found against the source tree
/// at the time of PR C-2; PR C-3 migrated the top 24 ranked sites (the
/// 6 typed `RecoveryAdvice` variants `missing_target_thread`,
/// `merge_no_common_ancestor`, `rebase_referenced_state_missing`,
/// `rebase_state_corrupted`, `thread_referenced_state_missing`, and
/// `thread_checkout_unavailable`), dropping the count to 152.
///
/// Decrease when you migrate sites to typed `RecoveryAdvice` (PR C-3
/// and follow-ups). Only increase with explicit justification — every
/// new untyped site is a future Priya-style "run heddle status" dead
/// end.
const MAX_UNTYPED_ANYHOW_SITES: usize = 152;

#[test]
fn untyped_error_sites_do_not_regress() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli/commands");
    let mut count = 0usize;
    let mut sites = Vec::new();
    walk_rs_files(&root, &mut |path, contents| {
        for (line_no, line) in contents.lines().enumerate() {
            if is_untyped_error_site(line) {
                count += 1;
                let rel = path.strip_prefix(&root).unwrap_or(path);
                sites.push(format!("{}:{}", rel.display(), line_no + 1));
            }
        }
    });

    eprintln!("typed_error_lint: current count = {count} (max = {MAX_UNTYPED_ANYHOW_SITES})");
    assert!(
        count <= MAX_UNTYPED_ANYHOW_SITES,
        "untyped anyhow/bail sites in cli/commands regressed: count={count} \
         max={MAX_UNTYPED_ANYHOW_SITES}. Either migrate the new site(s) to a \
         typed `RecoveryAdvice` variant (see crates/cli/src/cli/commands/advice.rs), \
         or — with explicit justification — bump MAX_UNTYPED_ANYHOW_SITES.\n\
         Current sites:\n{}",
        sites.join("\n")
    );
}

/// Close-the-class guard against the `_argv` null-sibling trap
/// (HeddleCo/heddle#254).
///
/// Recovery/advice surfaces used to emit a recommended action three ways:
/// a human `_string`, a parsed `_argv`, and a fillable `_template`. The
/// `_argv` sibling is a trap: it is `null` for every placeholder action
/// (`heddle commit -m "<message>"`), so an agent that prefers `_argv` to
/// avoid shell parsing reads `null`, treats it as "no action," and
/// silently skips recovery. We collapsed the triplet to one canonical
/// machine shape (`_template`, always present for a valid action) plus the
/// human `_string`, and dropped every `_argv` sibling.
///
/// This lint forbids re-introducing the pattern: no struct field
/// declaration or emitted JSON key naming a recovery/recommended *action*
/// or *command* as a parsed `_argv` may exist. A future PR that re-adds
/// e.g. `recommended_action_argv` fails here. Unrelated `_argv` names that
/// are not action/command siblings (`incoming_argv`, `harness_argv`) are
/// allowed.
#[test]
fn argv_action_command_siblings_are_forbidden() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli/commands");
    let mut sites = Vec::new();
    walk_rs_files(&root, &mut |path, contents| {
        for (line_no, line) in contents.lines().enumerate() {
            if let Some(ident) = forbidden_argv_sibling(line) {
                let rel = path.strip_prefix(&root).unwrap_or(path);
                sites.push(format!("{}:{} ({ident})", rel.display(), line_no + 1));
            }
        }
    });

    assert!(
        sites.is_empty(),
        "the `_argv` null-sibling trap was re-introduced (HeddleCo/heddle#254): a \
         recovery/recommended action or command must be emitted as a fillable \
         `_template` (always present) plus the human `_string` — never as a \
         parsed `_argv` sibling, which is null for every placeholder action and \
         silently reads as \"no action\" to agents. Drop the `_argv` field/key and \
         use the `_template`.\nOffending sites:\n{}",
        sites.join("\n")
    );
}

/// Detect a struct field declaration (`<name>_argv: …`) or an emitted JSON
/// key (`"<name>_argv"`) where `<name>` denotes a recovery/recommended
/// action or command. Returns the offending identifier. Field/key *uses*
/// (`trust.recommended_action_argv.clone()`), function definitions
/// (`fn recommended_action_argv(`), and unrelated `_argv` names
/// (`incoming_argv`) are not matched.
fn forbidden_argv_sibling(line: &str) -> Option<String> {
    const SUFFIX: &str = "_argv";
    let bytes = line.as_bytes();
    for (idx, _) in line.match_indices(SUFFIX) {
        let ident_end = idx + SUFFIX.len();
        // Walk back to the start of the identifier.
        let mut start = idx;
        while start > 0 {
            let c = bytes[start - 1];
            if c.is_ascii_alphanumeric() || c == b'_' {
                start -= 1;
            } else {
                break;
            }
        }
        let ident = &line[start..ident_end];
        // Only action/command siblings are the trap; allow `incoming_argv`,
        // `harness_argv`, `normalized_argv`, the bare `argv`, etc.
        if !(ident.contains("action") || ident.contains("command")) {
            continue;
        }
        let prev_is_quote = start > 0 && bytes[start - 1] == b'"';
        let next_is_quote = ident_end < bytes.len() && bytes[ident_end] == b'"';
        let after = line[ident_end..].trim_start();
        let is_field_decl = after.starts_with(':') && !after.starts_with("::");
        if is_field_decl || (prev_is_quote && next_is_quote) {
            return Some(ident.to_string());
        }
    }
    None
}

fn walk_rs_files(dir: &Path, visit: &mut impl FnMut(&Path, &str)) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip any `tests/` directory under the commands tree.
            if path.file_name().and_then(|n| n.to_str()) == Some("tests") {
                continue;
            }
            walk_rs_files(&path, visit);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        // Strip `#[cfg(test)] mod tests { … }` blocks. Crude balanced-brace
        // scan from the `#[cfg(test)]` marker until braces close — good
        // enough for our hand-rolled test modules; we don't have nested
        // `mod tests` inside `mod tests`.
        let cleaned = strip_cfg_test_blocks(&contents);
        visit(&path, &cleaned);
    }
}

fn strip_cfg_test_blocks(contents: &str) -> String {
    let mut out = String::with_capacity(contents.len());
    let mut chars = contents.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        // Match `#[cfg(test)]` followed by optional whitespace and
        // `mod tests` (or `mod foo` — any mod). Strip the mod body.
        if ch == '#' && contents[idx..].starts_with("#[cfg(test)]") {
            // Advance past the attribute.
            let after_attr_idx = idx + "#[cfg(test)]".len();
            // Find the next `{` after this attribute on the same logical
            // item. Look ahead char-by-char.
            let tail = &contents[after_attr_idx..];
            if let Some(brace_offset) = tail.find('{') {
                // Walk forward from the brace, tracking depth.
                let body_start = after_attr_idx + brace_offset;
                let mut depth = 0usize;
                let mut end = body_start;
                for (i, c) in contents[body_start..].char_indices() {
                    end = body_start + i;
                    if c == '{' {
                        depth += 1;
                    } else if c == '}' {
                        depth -= 1;
                        if depth == 0 {
                            end += 1;
                            break;
                        }
                    }
                }
                // Skip everything between idx and end.
                while let Some(&(next_idx, _)) = chars.peek() {
                    if next_idx >= end {
                        break;
                    }
                    chars.next();
                }
                continue;
            }
        }
        out.push(ch);
    }
    out
}

fn is_untyped_error_site(line: &str) -> bool {
    let trimmed = line.trim_start();
    // Match common call patterns.
    let patterns = [
        "anyhow!(\"",
        "anyhow::anyhow!(\"",
        "bail!(\"",
        "anyhow::bail!(\"",
        "anyhow!(format!(",
        "anyhow::anyhow!(format!(",
        "bail!(format!(",
        "anyhow::bail!(format!(",
    ];
    if !patterns.iter().any(|p| trimmed.contains(p)) {
        return false;
    }
    // Skip lines that hand off to a typed advice helper (the patterns
    // above wouldn't match those normally, but defense-in-depth: any
    // line passing `RecoveryAdvice::...` or a `_advice` / `_refusal`
    // call is typed even if it visually contains `anyhow!`).
    if trimmed.contains("RecoveryAdvice::")
        || trimmed.contains("_advice(")
        || trimmed.contains("_refusal(")
    {
        return false;
    }
    true
}
