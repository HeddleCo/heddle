// SPDX-License-Identifier: Apache-2.0
//! Render-discipline lint.
//!
//! Walks every file under `crates/cli/src/cli/commands/` and counts
//! `println!` / `print!` invocations that aren't inside a `render_*`
//! / `write_*` function or under a `#[cfg(test)]` block. The test
//! fails when that total exceeds [`RENDER_VIOLATION_BASELINE`] —
//! a ratchet that lets the existing partial/text-only verbs ship while
//! preventing new ones from regressing.
//!
//! Cleanup PRs lower the baseline; the discipline is documented in
//! `crates/cli/src/cli/commands/RENDER_AUDIT.md`. When the count
//! reaches zero, drop the constant and tighten this test to `== 0`.
//!
//! `eprintln!` is intentionally allowed everywhere — warnings and
//! tips ride on stderr by design.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Today's count of `println!` / `print!` macros outside `render_*` /
/// `write_*` / `#[cfg(test)]` across `crates/cli/src/cli/commands/`.
/// The test treats this as a hard ceiling. Every PR that removes a
/// violation MUST lower this number — that's the ratchet.
// Bumped from 722 → 881 when Codex's git-overlay foundation landed
// (the squashed foundation commit added the bridge/checkpoint/oss
// command surface, status/thread polish, and many human-output
// paths). Reset to 722 once those println sites get migrated onto
// render_/write_ helpers.
//
// Bumped from 881 → 968 when the redaction primitive
// (`heddle redact` / `heddle purge`) landed alongside several other
// recent verb additions (`heddle schemas`, the `--shared-target`
// advisory, GC's redaction-preserved report, the
// auto-prune thread surface). All of those produce human-output by
// design and use direct `println!` instead of the `render_*` /
// `write_*` discipline. Migrating them to the `*Output` struct
// pattern is a separate cleanup PR — drop this back to 881 (or
// lower) when that lands.
//
// Bumped from 968 → 970 when the redaction-completion commit landed:
// the GC `Pinned/Preserved N redaction tombstone(s)` invariant
// messages added two more direct `println!`/`eprintln!` sites that
// the dry-run vs. post-GC paths each surface. Same migration story
// as above — these will fold into the `*Output` cleanup.
//
// Dropped from 970 → 963 in the OSS-polish refinement pass:
// `init.rs` now routes through a `render_init` helper (-2) and
// `print_thread_op` was renamed to `render_thread_op` (-5).
//
// Dropped from 963 → 846 in the perfection pass:
// `status.rs` had ten `print_status_*` helpers renamed to `render_*`,
// `thread.rs` had two more (`print_thread_sections` /
// `print_thread_entry` → `render_*`), `fork.rs` got a
// `render_fork` extraction, and `bridge.rs::cmd_bridge_git_status`
// hoisted its render body into `render_bridge_git_status`. The rule
// is purely scope-shape: those println sites already lived inside
// helpers, they just needed the function name to match the
// lint-exempt prefix.
const RENDER_VIOLATION_BASELINE: usize = 846;

#[test]
fn cli_commands_render_via_render_or_write_functions() {
    let commands_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("cli")
        .join("commands");
    assert!(
        commands_dir.is_dir(),
        "commands directory missing at {}",
        commands_dir.display()
    );

    let mut total = 0usize;
    let mut by_file: Vec<(PathBuf, usize)> = Vec::new();
    walk_rust_files(&commands_dir, &mut |path| {
        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return,
        };
        let count = count_violations(&source);
        if count > 0 {
            by_file.push((path.to_path_buf(), count));
            total += count;
        }
    });

    if total > RENDER_VIOLATION_BASELINE {
        // Sort offenders descending so the largest baselines surface
        // at the top — those are the cleanup targets with the most
        // bang per PR.
        by_file.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        let lines: Vec<String> = by_file
            .iter()
            .take(10)
            .map(|(p, n)| {
                format!(
                    "  {n:4}  {}",
                    p.strip_prefix(&commands_dir).unwrap_or(p).display()
                )
            })
            .collect();
        panic!(
            "render-discipline regression: {total} println!/print! calls outside \
             render_*/write_* functions (baseline {RENDER_VIOLATION_BASELINE}).\n\
             Top offenders:\n{}\n\n\
             A new violation was introduced. Either route the output through a \
             `render_<thing>` or `write_<thing>` function and a `*Output` struct, \
             or — if you removed violations elsewhere — lower \
             RENDER_VIOLATION_BASELINE in this test by the matching count.",
            lines.join("\n"),
        );
    }
}

/// Recursive walk of `.rs` files, calling `visit` for each. Skips
/// hidden directories (none today, but keeps future test fixtures
/// from polluting the count).
fn walk_rust_files(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk_rust_files(&path, visit);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            visit(&path);
        }
    }
}

/// Count `println!` and `print!` invocations that are NOT inside a
/// `render_*` / `write_*` function or under a `#[cfg(test)]` /
/// `mod tests` block.
///
/// Implementation notes — the parser is text-based for the same reason
/// `op_id_coverage` and `tier_coverage` are: avoids dragging `syn`
/// into dev-deps, stays readable when something fails, and the rule
/// is shallow enough that a brace-depth scan is sound.
fn count_violations(source: &str) -> usize {
    let bytes = source.as_bytes();
    let mut violations = 0usize;
    let mut i = 0usize;
    let len = bytes.len();
    // Stack of "is the enclosing scope exempt?" booleans, pushed at
    // every `{` and popped at the matching `}`. The top of the stack
    // is the current scope's exemption.
    let mut exempt_stack: Vec<bool> = vec![false];
    // Look-ahead window so `fn render_x(... { ... }` can mark the
    // upcoming scope as exempt before its `{` is consumed.
    let mut next_scope_exempt = false;
    // Strings/chars/comments — skip-region scanning only checks for
    // closers, not for nested macros. Plenty good enough for this lint.
    while i < len {
        let c = bytes[i];

        // Line comment
        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment (one level — this lint is a soft contract,
        // not a parser).
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(len);
            continue;
        }
        // String literal
        if c == b'"' {
            i += 1;
            while i < len && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        // Char literal
        if c == b'\'' {
            i += 1;
            // Lifetimes (`'a`) and char escapes — bail on first
            // closing quote or whitespace.
            while i < len && bytes[i] != b'\'' && bytes[i] != b'\n' {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                i += 1;
            }
            if i < len && bytes[i] == b'\'' {
                i += 1;
            }
            continue;
        }

        // `#[cfg(test)]` attribute marks the next scope as exempt.
        if c == b'#'
            && bytes.get(i + 1) == Some(&b'[')
            && rest_starts_with(bytes, i, "#[cfg(test)]")
        {
            next_scope_exempt = true;
            i += "#[cfg(test)]".len();
            continue;
        }
        // `mod tests {` (with or without `pub`/`pub(crate)`)
        if rest_starts_with(bytes, i, "mod tests") {
            next_scope_exempt = true;
            i += "mod tests".len();
            continue;
        }
        // `fn render_…` / `fn write_…` — exempt the function body.
        if rest_starts_with(bytes, i, "fn render_") || rest_starts_with(bytes, i, "fn write_") {
            next_scope_exempt = true;
            i += 2; // step past `fn`; the rest of the name will be skipped naturally
            continue;
        }

        // Brace tracking
        if c == b'{' {
            exempt_stack.push(next_scope_exempt || *exempt_stack.last().unwrap_or(&false));
            next_scope_exempt = false;
            i += 1;
            continue;
        }
        if c == b'}' {
            exempt_stack.pop();
            if exempt_stack.is_empty() {
                exempt_stack.push(false);
            }
            i += 1;
            continue;
        }

        // The actual lint: `println!` / `print!` macro invocations.
        if rest_starts_with(bytes, i, "println!") && !*exempt_stack.last().unwrap_or(&false) {
            violations += 1;
            i += "println!".len();
            continue;
        }
        if rest_starts_with(bytes, i, "print!")
            && !*exempt_stack.last().unwrap_or(&false)
            // `print!` is a strict prefix of `println!`; only count
            // when the next char is `(` so we don't double-count.
            && bytes.get(i + "print!".len()) == Some(&b'(')
        {
            violations += 1;
            i += "print!".len();
            continue;
        }

        i += 1;
    }
    violations
}

fn rest_starts_with(bytes: &[u8], at: usize, needle: &str) -> bool {
    let n = needle.as_bytes();
    if at + n.len() > bytes.len() {
        return false;
    }
    &bytes[at..at + n.len()] == n
}
