// SPDX-License-Identifier: Apache-2.0
//! `output_kind` JSON discriminator invariant.
//!
//! Every CLI verb that emits JSON output must carry a top-level
//! `output_kind` (or `kind`, for the catalog itself) field. Agents that
//! route on the discriminator otherwise fall back to fragile text
//! parsing.
//!
//! This module enforces the invariant in two layers:
//!
//! 1. **Catalog completeness (static).** Walks the
//!    `build_command_catalog()` table and asserts that every verb with
//!    `supports_json: true` either declares a `json_discriminator` with
//!    field `"output_kind"` (matching the snake-cased verb path) OR is
//!    listed in `UNSWEPT_TODO` as a known gap. Adding a new
//!    JSON-emitting verb without classifying it fails CI.
//!
//! 2. **Runtime contract (dynamic).** Spawns the built `heddle` binary
//!    against representative fixtures and asserts the emitted JSON
//!    actually carries the `output_kind` field with the catalog-declared
//!    value. Without this, a struct could ship without the field while
//!    the catalog claimed it was present.
//!
//! Issue-of-record: HeddleCo/heddle#272. The unswept allowlist is the
//! TODO list for follow-up sweeps; do not grow it for newly-added
//! verbs.

use std::collections::BTreeSet;

use serde_json::Value;
use tempfile::TempDir;

use cli::cli::commands::build_command_catalog;

use super::{heddle, heddle_output};

/// Verbs whose `output_kind` invariant is enforced — both the catalog
/// declaration and (where invocable) the runtime emission.
///
/// Sourced from PR #251 (the initial sweep) plus heddle#272 (which
/// closes the gap on the named-by-persona verbs from round 4 finding
/// S1).
const SWEPT: &[&str] = &[
    // PR #251 — initial discriminator coverage.
    "status",
    "verify",
    "init",
    "capture",
    "checkpoint",
    "commit",
    "clone",
    "diff",
    "undo",
    "redo",
    "thread list",
    "thread show",
    "workspace show",
    "doctor docs",
    "doctor schemas",
    "schemas",
    "bridge git status",
    "bridge git import",
    "bridge git sync",
    "bridge git reconcile",
    // heddle#272 — output_kind sweep on the named-by-persona verbs.
    "stack",
    "stack ready",
    "stack snapshot",
    "goto",
    "fork",
    "revert",
    "purge apply",
    "purge list",
    "redact apply",
    "redact list",
    "redact show",
    "redact trust add",
    "redact trust list",
    "redact trust remove",
    "stash list",
    "stash show",
    "clean",
    "discuss open",
    "discuss append",
    "discuss resolve",
    "discuss list",
    "discuss show",
    "context set",
    "context get",
    "context list",
    "context history",
    "context edit",
    "context supersede",
    "context rm",
    "context check",
    "context suggest",
    "context audit",
    "review show",
    "review sign",
    "review next",
    "review health",
    "cherry-pick",
    "bisect start",
    "bisect good",
    "bisect bad",
    "bisect reset",
];

/// The catalog itself advertises its container kind as `"kind":
/// "command_catalog"` rather than `output_kind`. Single intentional
/// exception, baked into the schema and the catalog discoverability
/// contract that agents already rely on.
const KIND_FIELD_EXCEPTIONS: &[&str] = &["commands"];

/// JSON-emitting verbs that have NOT yet had `output_kind` wired
/// through their Serialize structs. Do NOT add new verbs here — pick
/// up the sweep instead. This list is the rolldown surface for
/// follow-up work tracked separately from #272.
const UNSWEPT_TODO: &[&str] = &[
    "abort",
    "adopt",
    "actor done",
    "actor explain",
    "actor list",
    "actor show",
    "actor spawn",
    "agent capture",
    "agent heartbeat",
    "agent list",
    "agent ready",
    "agent release",
    "agent reserve",
    "agent serve",
    "agent status",
    "agent stop",
    "attempt",
    "blame",
    "branch",
    "bridge git export",
    "bridge git ingest",
    "bridge git init",
    "bridge git pull",
    "bridge git push",
    "bridge git reason",
    "checkout",
    "collapse",
    "compare",
    "conflict list",
    "conflict show",
    "continue",
    "daemon serve",
    "daemon status",
    "daemon stop",
    "delegate",
    "diagnose",
    "doctor",
    "fetch",
    "fsck",
    "gc",
    "git-overlay",
    "harness-bridge",
    "hook events",
    "hook install",
    "hook list",
    "hook uninstall",
    "index",
    "inspect",
    "integration doctor",
    "integration install",
    "integration list",
    "integration relay",
    "integration uninstall",
    "integration upgrade",
    "log",
    "maintenance gc",
    "maintenance index",
    "maintenance inspect",
    "maintenance monitor",
    "maintenance run",
    "marker create",
    "marker delete",
    "marker list",
    "marker show",
    "merge",
    "monitor",
    "pull",
    "push",
    "query",
    "rebase",
    "ready",
    "remote add",
    "remote list",
    "remote remove",
    "remote set-default",
    "remote show",
    "resolve",
    "retro",
    "semantic hot",
    "session end",
    "session list",
    "session segment",
    "session show",
    "session start",
    "ship",
    "show",
    "stash apply",
    "stash clear",
    "stash drop",
    "stash pop",
    "stash push",
    "start",
    "store warm",
    "switch",
    "sync",
    "thread absorb",
    "thread approvals",
    "thread approve",
    "thread captures",
    "thread check-merge",
    "thread cleanup",
    "thread create",
    "thread current",
    "thread drop",
    "thread move",
    "thread promote",
    "thread refresh",
    "thread rename",
    "thread resolve",
    "thread revoke-approval",
    "thread switch",
    "transaction abort",
    "transaction begin",
    "transaction commit",
    "transaction status",
    "try",
    "version",
    "watch",
    "workspace",
];

/// Snake-cased value an `output_kind` discriminator should carry for a
/// given display path. Mirrors `display.replace(['-', ' '], "_")` for
/// most verbs; wire-format-stable overrides set in PR #251 stay as-is.
fn expected_output_kind(display: &str) -> String {
    if let Some(stable) = output_kind_override(display) {
        return stable.to_string();
    }
    display.replace(['-', ' '], "_")
}

/// Pre-existing `output_kind` values that don't follow the snake-cased
/// path rule. Frozen wire format — agents already key off these.
fn output_kind_override(display: &str) -> Option<&'static str> {
    match display {
        // `workspace show` was instrumented in PR #251 as
        // `workspace_summary` (the underlying struct's semantic name)
        // rather than the snake-cased path. Wire-format-stable.
        "workspace show" => Some("workspace_summary"),
        _ => None,
    }
}

#[test]
fn every_json_emitting_verb_is_classified() {
    let catalog = build_command_catalog();
    let known: BTreeSet<&str> = SWEPT
        .iter()
        .copied()
        .chain(UNSWEPT_TODO.iter().copied())
        .chain(KIND_FIELD_EXCEPTIONS.iter().copied())
        .collect();

    let mut unclassified = Vec::new();
    for entry in &catalog.commands {
        if !entry.supports_json {
            continue;
        }
        if entry.json_kind == "none" {
            continue;
        }
        if !known.contains(entry.display.as_str()) {
            unclassified.push(entry.display.clone());
        }
    }

    assert!(
        unclassified.is_empty(),
        "New JSON-emitting verbs lack an `output_kind` classification. \
         Either add `output_kind` to the verb's Serialize struct AND add the \
         entry to `SWEPT` (with a `json_discriminator(... \"output_kind\", \
         ...)` declaration in `command_catalog.rs`), or — as a documented \
         gap — add the entry to `UNSWEPT_TODO`. New verbs MUST take the \
         first path; the second is the rolldown surface for pre-existing \
         unswept verbs.\n\nUnclassified:\n  - {}",
        unclassified.join("\n  - ")
    );
}

#[test]
fn swept_verbs_declare_output_kind_in_catalog() {
    let catalog = build_command_catalog();
    let mut missing = Vec::new();
    let mut wrong_value = Vec::new();

    for &display in SWEPT {
        let Some(entry) = catalog.commands.iter().find(|c| c.display == display) else {
            missing.push(format!("{display}: not present in command catalog"));
            continue;
        };
        let expected = expected_output_kind(display);
        let discriminator = entry
            .json_discriminators
            .iter()
            .find(|d| d.field == "output_kind");
        match discriminator {
            None => missing.push(format!(
                "{display}: catalog entry has no `output_kind` discriminator (expected value `{expected}`)"
            )),
            Some(d) if d.value != expected => wrong_value.push(format!(
                "{display}: declared output_kind=`{}` but expected `{expected}`",
                d.value
            )),
            Some(_) => {}
        }
    }

    if !missing.is_empty() || !wrong_value.is_empty() {
        let mut msg = String::new();
        if !missing.is_empty() {
            msg.push_str("Verbs in SWEPT missing the `output_kind` catalog declaration:\n  - ");
            msg.push_str(&missing.join("\n  - "));
            msg.push('\n');
        }
        if !wrong_value.is_empty() {
            msg.push_str("Verbs in SWEPT with the wrong `output_kind` value:\n  - ");
            msg.push_str(&wrong_value.join("\n  - "));
            msg.push('\n');
        }
        panic!(
            "Catalog/SWEPT contract violations. The catalog discriminator is the \
             wire-format promise agents read; it must match the verb's display \
             path (snake-cased).\n\n{msg}"
        );
    }
}

#[test]
fn kind_field_exceptions_use_kind_intentionally() {
    let catalog = build_command_catalog();
    for &display in KIND_FIELD_EXCEPTIONS {
        let entry = catalog
            .commands
            .iter()
            .find(|c| c.display == display)
            .unwrap_or_else(|| panic!("`{display}` listed in KIND_FIELD_EXCEPTIONS is not in the catalog"));
        let has_kind = entry
            .json_discriminators
            .iter()
            .any(|d| d.field == "kind");
        assert!(
            has_kind,
            "`{display}` is documented as a `kind`-rather-than-output_kind exception but the catalog declares no `kind` discriminator. Update the catalog or drop the exception."
        );
    }
}

#[test]
fn unswept_verbs_have_no_output_kind_declaration() {
    // Defensive: if a verb is on the TODO list but the catalog already
    // declares output_kind for it, the TODO entry is stale — move it to
    // SWEPT.
    let catalog = build_command_catalog();
    let mut stale = Vec::new();
    for &display in UNSWEPT_TODO {
        let Some(entry) = catalog.commands.iter().find(|c| c.display == display) else {
            continue;
        };
        let has_output_kind = entry
            .json_discriminators
            .iter()
            .any(|d| d.field == "output_kind");
        if has_output_kind {
            stale.push(display.to_string());
        }
    }
    assert!(
        stale.is_empty(),
        "Verbs listed in UNSWEPT_TODO already declare `output_kind` in the \
         catalog. Move them to SWEPT (and add a runtime invocation if \
         feasible):\n  - {}",
        stale.join("\n  - ")
    );
}

// ---------------------------------------------------------------------
// Runtime contract: invoke a representative subset of swept verbs and
// confirm the emitted JSON carries `output_kind` matching the catalog
// declaration. The set covers the heddle#272 named-by-persona verbs
// that run safely in an empty/init'd repo without elaborate fixtures.
// ---------------------------------------------------------------------

fn init_fixture() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    heddle(
        &[
            "init",
            "--principal-name",
            "Heddle Test",
            "--principal-email",
            "heddle@test.example",
        ],
        Some(temp.path()),
    )
    .expect("heddle init");
    temp
}

/// Invocations for swept verbs we exercise at runtime. Per-verb argv +
/// whether the verb is expected to exit zero. Some named verbs need a
/// non-trivial fixture (e.g. `revert` requires a state to revert); we
/// skip those here and rely on dedicated tests elsewhere.
fn runtime_invocation_args(display: &str) -> Option<(&'static [&'static str], bool /* expect_ok */)> {
    match display {
        "stack" => Some((&["stack"], true)),
        "stack ready" => Some((&["stack", "ready"], true)),
        "purge list" => Some((&["purge", "list"], true)),
        "redact list" => Some((&["redact", "list"], true)),
        "redact trust list" => Some((&["redact", "trust", "list"], true)),
        "stash list" => Some((&["stash", "list"], true)),
        "discuss list" => Some((&["discuss", "list"], true)),
        "context list" => Some((&["context", "list"], true)),
        "review next" => Some((&["review", "next"], true)),
        "review health" => Some((&["review", "health"], true)),
        // `fork` succeeds in an init'd repo (forks the empty initial state).
        "fork" => Some((&["fork"], true)),
        // `bisect start` accepts a no-state init and emits its session.
        "bisect start" => Some((&["bisect", "start"], true)),
        "bisect reset" => Some((&["bisect", "reset"], true)),
        _ => None,
    }
}

#[test]
fn runtime_emits_output_kind_for_invokable_swept_verbs() {
    let fixture = init_fixture();
    let mut failures = Vec::new();

    for &display in SWEPT {
        let Some((argv, expect_ok)) = runtime_invocation_args(display) else {
            continue;
        };
        let expected = expected_output_kind(display);
        let mut full_argv: Vec<&str> = vec!["--output", "json"];
        full_argv.extend(argv.iter().copied());
        let output = match heddle_output(&full_argv, Some(fixture.path())) {
            Ok(out) => out,
            Err(err) => {
                failures.push(format!("{display}: spawn failed: {err}"));
                continue;
            }
        };

        if expect_ok && !output.status.success() {
            failures.push(format!(
                "{display}: exited non-zero (status {:?})\nstdout: {}\nstderr: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
            continue;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        // Pick the JSON payload from stdout: jsonl emitters print one
        // object per line; the discriminator must surface on the first
        // record. For verbs whose output root is a JSON array (e.g.
        // `context list`), the catalog mandates envelope-wrapping into
        // `{"output_kind": ..., "items": [...]}` (per heddle#272 brief
        // option (a)).
        let first_line = stdout.lines().next().unwrap_or("").trim();
        let parsed: Value = match serde_json::from_str(first_line) {
            Ok(v) => v,
            Err(err) => {
                failures.push(format!(
                    "{display}: stdout is not parseable JSON: {err}\n  first_line: {first_line}"
                ));
                continue;
            }
        };

        let actual = parsed.get("output_kind").and_then(|v| v.as_str());
        match actual {
            Some(value) if value == expected => {}
            Some(other) => failures.push(format!(
                "{display}: runtime JSON has output_kind=`{other}` but catalog declares `{expected}`"
            )),
            None => failures.push(format!(
                "{display}: runtime JSON missing `output_kind` field (expected `{expected}`); payload: {first_line}"
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "Runtime JSON output is missing or mismatches `output_kind`:\n  - {}",
        failures.join("\n  - ")
    );
}
