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

use cli::cli::commands::{
    CLONE_CONNECTION_OUTPUT_KIND, CLONE_OUTPUT_KIND, build_command_catalog,
};

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
fn clone_catalog_entry_advertises_both_clone_and_clone_connection() {
    // Hosted `heddle clone --output json` emits TWO JSON records on
    // one invocation: a preliminary connection envelope
    // (`output_kind: "clone_connection"`) followed by the final clone
    // payload (`output_kind: "clone"`). Agents that consume
    // `commands` / `json_discriminators` only see legitimate
    // routes for the final record unless the catalog advertises both
    // discriminators (heddle#272 Codex r3 finding, PR #281).
    //
    // This test pins both discriminators against the constants used
    // by the runtime emission sites in `crates/cli/src/cli/commands/clone.rs`,
    // so a future rename of either value updates the catalog and the
    // runtime in lockstep — divergence fails CI.
    let catalog = build_command_catalog();
    let clone = catalog
        .commands
        .iter()
        .find(|c| c.display == "clone")
        .expect("clone should be cataloged");

    let output_kind_values: Vec<&str> = clone
        .json_discriminators
        .iter()
        .filter(|d| d.field == "output_kind")
        .map(|d| d.value.as_str())
        .collect();

    assert!(
        output_kind_values.contains(&CLONE_OUTPUT_KIND),
        "clone catalog entry must advertise `output_kind = {CLONE_OUTPUT_KIND}` \
         (the final clone payload); actually advertises {output_kind_values:?}"
    );
    assert!(
        output_kind_values.contains(&CLONE_CONNECTION_OUTPUT_KIND),
        "clone catalog entry must advertise `output_kind = {CLONE_CONNECTION_OUTPUT_KIND}` \
         alongside `{CLONE_OUTPUT_KIND}` so agents can route the hosted \
         preliminary connection envelope; actually advertises {output_kind_values:?}"
    );

    // The preliminary envelope is not backed by a documented schema
    // verb (it's a small inline object); the metadata invariant test
    // requires `no_schema_reason` to be set in that case. Pin the
    // shape here so a future refactor of the catalog helper doesn't
    // silently drop the documentation.
    let envelope = clone
        .json_discriminators
        .iter()
        .find(|d| d.value == CLONE_CONNECTION_OUTPUT_KIND)
        .expect("clone_connection discriminator must be present");
    assert!(
        envelope.schema_verb.is_none(),
        "clone_connection envelope has no schema verb (it is not a Serialize struct); \
         got schema_verb={:?}",
        envelope.schema_verb
    );
    assert!(
        envelope
            .no_schema_reason
            .as_deref()
            .is_some_and(|reason| !reason.is_empty()),
        "clone_connection envelope must document why it has no schema verb"
    );
}

#[test]
#[ignore = "requires a live hosted gRPC fixture; runtime equality is enforced \
            statically via CLONE_CONNECTION_OUTPUT_KIND (see \
            clone_catalog_entry_advertises_both_clone_and_clone_connection). \
            When a hosted-clone fixture lands, drop the #[ignore] and parse \
            both stdout records here."]
fn hosted_clone_emits_both_discriminator_values() {
    // Placeholder for the live-network assertion: spawn `heddle
    // clone --output json <hosted-remote> <path>` against a fixture
    // server, then assert the first stdout line carries
    // `output_kind: "clone_connection"` and the final line carries
    // `output_kind: "clone"`. Both values must match the catalog.
    //
    // Until the fixture exists, the constants used by clone.rs at
    // the actual emit sites (CLONE_OUTPUT_KIND and
    // CLONE_CONNECTION_OUTPUT_KIND) are pinned to the catalog by the
    // sibling test, so a rename can't silently desync runtime from
    // catalog.
    let catalog = build_command_catalog();
    let clone = catalog
        .commands
        .iter()
        .find(|c| c.display == "clone")
        .expect("clone should be cataloged");
    let advertised: Vec<&str> = clone
        .json_discriminators
        .iter()
        .filter(|d| d.field == "output_kind")
        .map(|d| d.value.as_str())
        .collect();
    assert!(advertised.contains(&CLONE_OUTPUT_KIND));
    assert!(advertised.contains(&CLONE_CONNECTION_OUTPUT_KIND));
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

#[test]
fn swept_verb_doc_samples_show_output_kind() {
    // heddle#272 r5 (Codex P1): the `docs/json-schemas.md` sample for
    // each #272-swept verb must show the `output_kind` field so the
    // documented machine contract matches the runtime emission. The
    // `doctor schemas` gate only checks that a sample's keys are a
    // subset of the schema's properties (it does not enforce
    // required-field presence), so it would NOT catch a sample that
    // silently drops `output_kind`; this test does.
    //
    // Verbs documented with one representative sample for a pipe-listed
    // group (e.g. `bisect start|good|bad|reset`) show the first
    // variant's value; the per-variant value is mechanically
    // `display.replace(['-', ' '], "_")`.
    let doc_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("docs/json-schemas.md");
    let doc = std::fs::read_to_string(&doc_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", doc_path.display()));

    const REQUIRED: &[&str] = &[
        "goto",
        "clean",
        "revert",
        "fork",
        "cherry_pick",
        "stack",
        "stack_ready",
        "stack_snapshot",
        "stash_list",
        "stash_show",
        "review_show",
        "review_sign",
        "review_next",
        "review_health",
        "bisect_start",
        "purge_apply",
        "redact_apply",
        "redact_trust_add",
        "discuss_open",
        "discuss_list",
        "context_set",
        "context_list",
    ];

    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|value| !doc.contains(&format!("\"output_kind\": \"{value}\"")))
        .collect();

    assert!(
        missing.is_empty(),
        "docs/json-schemas.md is missing `output_kind` in the sample(s) for \
         these heddle#272-swept verbs: {missing:?}. Add `\"output_kind\": \
         \"<value>\"` to each sample so the documented contract matches the \
         runtime discriminator."
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
fn runtime_init_emits_output_kind() {
    // heddle#272 r6 (Codex P2): `init` is in SWEPT and the catalog
    // advertises `output_kind: "init"`, but the previous runtime sweep
    // never invoked `init` (it needs a fresh, un-init'd directory, so it
    // wasn't in `runtime_invocation_args`). That left an
    // advertise-without-emit gap the catalog injection in
    // `heddle schemas` could not catch. Pin it here: a clean directory
    // initialised with `--output json` must carry `output_kind: "init"`.
    let temp = TempDir::new().expect("tempdir");
    let output = heddle_output(
        &[
            "--output",
            "json",
            "init",
            "--principal-name",
            "Heddle Test",
            "--principal-email",
            "heddle@test.example",
        ],
        Some(temp.path()),
    )
    .expect("heddle init --output json");

    assert!(
        output.status.success(),
        "init exited non-zero: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next().unwrap_or("").trim();
    let parsed: Value =
        serde_json::from_str(first_line).expect("init stdout is parseable JSON");
    assert_eq!(
        parsed.get("output_kind").and_then(|v| v.as_str()),
        Some("init"),
        "`heddle init --output json` must emit `output_kind: \"init\"`; payload: {first_line}"
    );
}

/// Top-level key set of the first JSON object emitted by `argv` in
/// `dir`. Panics with the captured output on failure so doc-vs-runtime
/// mismatches surface a readable diff.
fn runtime_top_level_keys(argv: &[&str], dir: &std::path::Path) -> BTreeSet<String> {
    let output = heddle_output(argv, Some(dir))
        .unwrap_or_else(|err| panic!("spawn {argv:?}: {err}"));
    assert!(
        output.status.success(),
        "{argv:?} exited non-zero: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next().unwrap_or("").trim();
    let parsed: Value = serde_json::from_str(first_line)
        .unwrap_or_else(|err| panic!("{argv:?} stdout not JSON: {err}\n  line: {first_line}"));
    parsed
        .as_object()
        .unwrap_or_else(|| panic!("{argv:?} top-level JSON is not an object: {first_line}"))
        .keys()
        .cloned()
        .collect()
}

/// First fenced ```json block in `doc` whose top-level `output_kind`
/// equals `value`, returned as its top-level key set. `None` if no such
/// documented sample exists.
fn doc_sample_top_level_keys(doc: &str, output_kind_value: &str) -> Option<BTreeSet<String>> {
    let mut in_block = false;
    let mut buf = String::new();
    for line in doc.lines() {
        let trimmed = line.trim();
        if !in_block {
            if trimmed == "```json" {
                in_block = true;
                buf.clear();
            }
            continue;
        }
        if trimmed == "```" {
            in_block = false;
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&buf)
                && map.get("output_kind").and_then(|v| v.as_str()) == Some(output_kind_value)
            {
                return Some(map.keys().cloned().collect());
            }
            buf.clear();
            continue;
        }
        buf.push_str(line);
        buf.push('\n');
    }
    None
}

#[test]
fn swept_doc_samples_match_runtime_keys() {
    // heddle#272 r6 (Codex P2 x2): `doctor schemas` only checks that a
    // documented sample's keys are a SUBSET of the schema's properties —
    // it never checks the sample against the actual `--output json`
    // payload. So a sample can describe a stale shape (the old
    // key/value-style context payload; the `thread`/`snapshot` stack
    // wrapper) and pass. This test closes that gap: for verbs we can
    // invoke in a fixture, the documented sample's top-level keys must be
    // a subset of the real runtime keys, and `output_kind` must be
    // present in both. Doc drift now turns CI red until the sample is
    // corrected.
    let doc_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("docs/json-schemas.md");
    let doc = std::fs::read_to_string(&doc_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", doc_path.display()));

    let fixture = init_fixture();
    // Build a stack so `stack snapshot` has a member to scope to (an
    // empty repo's `main` is not part of any stack).
    heddle(
        &["start", "feature-x", "--workspace", "solid"],
        Some(fixture.path()),
    )
    .expect("heddle start feature-x");
    std::fs::write(fixture.path().join("annotated.txt"), "content")
        .expect("write annotated fixture file");

    // (output_kind value, runtime argv) — each invocation must succeed in
    // the fixture and the documented sample must mirror its key set.
    let cases: &[(&str, &[&str])] = &[
        (
            "stack_snapshot",
            &["--output", "json", "stack", "snapshot", "--thread", "feature-x"],
        ),
        (
            "context_set",
            &[
                "--output",
                "json",
                "context",
                "set",
                "--path",
                "annotated.txt",
                "--scope",
                "file",
                "-m",
                "owner note",
            ],
        ),
        ("context_list", &["--output", "json", "context", "list"]),
    ];

    let mut failures = Vec::new();
    for (output_kind, argv) in cases {
        let runtime_keys = runtime_top_level_keys(argv, fixture.path());
        let Some(doc_keys) = doc_sample_top_level_keys(&doc, output_kind) else {
            failures.push(format!(
                "{output_kind}: no documented `{output_kind}` sample found in docs/json-schemas.md"
            ));
            continue;
        };
        if !doc_keys.contains("output_kind") {
            failures.push(format!(
                "{output_kind}: documented sample is missing the `output_kind` key"
            ));
        }
        if !runtime_keys.contains("output_kind") {
            failures.push(format!(
                "{output_kind}: runtime payload is missing the `output_kind` key (keys: {runtime_keys:?})"
            ));
        }
        let extra: Vec<&String> = doc_keys.difference(&runtime_keys).collect();
        if !extra.is_empty() {
            failures.push(format!(
                "{output_kind}: documented sample has keys absent from the real `--output json` \
                 payload: {extra:?}\n      doc keys:     {doc_keys:?}\n      runtime keys: {runtime_keys:?}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Documented samples drifted from the real runtime payloads:\n  - {}",
        failures.join("\n  - ")
    );
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
