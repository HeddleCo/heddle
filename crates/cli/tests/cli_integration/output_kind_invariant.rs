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

use cli::cli::commands::{
    CLONE_CONNECTION_OUTPUT_KIND, CLONE_OUTPUT_KIND, build_command_catalog,
    operator_emission_output_kinds, operator_envelope_verbs,
};
use serde_json::Value;
use tempfile::TempDir;

use super::{assert_undo_requires_hard, heddle, heddle_output};

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
    "clone",
    "diff",
    "undo",
    "thread list",
    "thread show",
    "doctor docs",
    "bridge git import",
    "bridge git export",
    "sync git",
    "status",
    // heddle#1057 — whoami emits `output_kind: "whoami"` (matches its verb path,
    // no override needed).
    "whoami",
    "promote",
    "grant create",
    "grant list",
    "grant delete",
    // heddle#272 — output_kind sweep on the named-by-persona verbs.
    "agent presence list",
    "agent presence show",
    "agent presence explain",
    "agent presence complete",
    "auth login",
    "auth logout",
    "auth status",
    "auth invite",
    "auth invite list",
    "auth trust show",
    "auth trust replace",
    "auth create-service-token",
    "revert",
    "redact apply",
    "redact list",
    "redact purge apply",
    "redact purge list",
    "redact show",
    "visibility set",
    "visibility promote",
    "visibility show",
    "visibility list",
    "discuss open",
    "discuss append",
    "discuss resolve",
    "discuss reopen",
    "discuss list",
    "discuss show",
    "discuss wait",
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
    "resolve",
    "semantic diff",
    "semantic refs",
    // heddle#662 — additive discriminator paths for state inspection,
    // rebase progress JSONL, and conflict-resolution success.
    "show",
    // heddle#641 — swept the remaining verbs whose runtime JSON already
    // emits `output_kind`. Every value below was probed live against the
    // built binary (or read off the emitting struct for the daemon-style
    // verbs that can't run in a synthetic fixture); several carry
    // wire-frozen values that differ from the snake-cased display path —
    // see `output_kind_override`.
    "abort",
    "adopt",
    "agent capture",
    "agent ready",
    "agent task create",
    "agent task list",
    "agent task show",
    "agent task update",
    "agent fanout plan",
    "agent fanout start",
    "continue",
    "daemon stop",
    "netd status",
    "netd stop",
    "doctor",
    "thread expand",
    "land",
    "log",
    "maintenance gc",
    "maintenance inspect",
    "maintenance repack",
    "maintenance refresh",
    "maintenance oplog recover",
    "pull",
    "push",
    "query",
    "ready",
    "remote add",
    "remote list",
    "remote remove",
    "remote set-default",
    "remote show",
    "start",
    "sync",
    "agent timeline status",
    "agent timeline record-start",
    "agent timeline record-finish",
    "agent timeline fork",
    "agent timeline reset",
    "agent timeline recover",
    "thread cleanup",
    "thread create",
    "thread drop",
    "thread marker create",
    "thread marker delete",
    "thread marker list",
    "thread marker show",
    "thread promote",
    "thread refresh",
    "thread rename",
    "thread resolve",
    "thread revoke-approval",
    "thread switch",
    // heddle#1709 — `heddle env` (confidential-runtime env-store) verbs emit
    // `output_kind: "env_create"/"env_list"`; discriminators are in the catalog
    // but were never enumerated here.
    "env create",
    "env list",
];

/// The catalog itself advertises its container kind as `"kind":
/// "command_catalog"` rather than `output_kind`. Single intentional
/// exception, baked into the schema and the catalog discoverability
/// contract that agents already rely on.
const KIND_FIELD_EXCEPTIONS: &[&str] = &["help"];

/// JSON-emitting verbs that have NOT yet had `output_kind` wired
/// through their Serialize structs. Do NOT add new verbs here — pick
/// up the sweep instead. This list is the rolldown surface for
/// follow-up work tracked separately from #272.
const UNSWEPT_TODO: &[&str] = &[
    "agent heartbeat",
    "agent list",
    "agent provenance begin",
    "agent provenance end",
    "agent provenance list",
    "agent provenance segment",
    "agent provenance show",
    "agent release",
    "agent reserve",
    "context reason git",
    "ci run",
    "thread collapse",
    "daemon serve",
    "daemon status",
    "netd serve",
    "maintenance fsck",
    "maintenance fsck repair git",
    "git-overlay",
    "hook events",
    "hook install",
    "hook list",
    "hook uninstall",
    "integration doctor",
    "integration install",
    "integration list",
    "integration relay",
    "integration stamp",
    "integration uninstall",
    "integration upgrade",
    "semantic hot",
    "session end",
    "session list",
    "session segment",
    "session show",
    "session start",
    "thread absorb",
    "thread approvals",
    "thread approve",
    "thread captures",
    "thread check-merge",
    "thread current",
    "thread move",
    "watch",
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
        // heddle#641 — runtime-probed wire values that pre-date the
        // snake-cased-path rule. The catalog advertises what the
        // commands actually emit TODAY; renaming any of these is a
        // wire-format break that must update the emitting struct, the
        // catalog discriminator, and this override in lockstep.
        //
        // `agent capture` / `agent ready` are session-validated
        // aliases that delegate to `cmd_snapshot` / `cmd_ready`, so
        // they emit the delegate's kind.
        "agent capture" => Some("capture"),
        "agent ready" => Some("ready"),
        "start" => Some("thread_start"),
        // The garbage-collection wrapper emits its inner tool's kind.
        "maintenance gc" => Some("gc"),
        // `redact purge` preserves the pre-consolidation wire values.
        "redact purge apply" => Some("purge_apply"),
        "redact purge list" => Some("purge_list"),
        // Presence folded under `agent`; the wire values keep the
        // pre-fold spellings so agents don't churn.
        "agent presence list" => Some("presence_list"),
        "agent presence show" => Some("presence_show"),
        "agent presence explain" => Some("presence_explain"),
        "agent presence complete" => Some("presence_complete"),
        // Headless invite login emits the created account, not the command path.
        "auth login" => Some("agent_account_created"),
        // Expand folded under `thread`; wire value keeps the root spelling.
        "thread expand" => Some("expand"),
        // Oplog recover folded under `maintenance`; wire value unchanged.
        "maintenance oplog recover" => Some("oplog_recover"),
        // Timeline folded under `agent`; the action verbs share one
        // pre-fold envelope value.
        "agent timeline status" => Some("timeline_status"),
        "agent timeline record-start" => Some("timeline_record_start"),
        "agent timeline record-finish" => Some("timeline_record_finish"),
        "agent timeline fork" | "agent timeline reset" | "agent timeline recover" => {
            Some("timeline_action")
        }
        _ => None,
    }
}

/// The single source of truth for the doc-vs-runtime sweep: every catalog
/// discriminator whose field is `output_kind`, as `(display, value,
/// has_schema_verb)`. Driving the doc invariants from this set — rather
/// than a hand-maintained `SWEPT_272`-style subset — is what makes a stale
/// sample for ANY swept verb (the early PR #251 verbs included) fail CI
/// mechanically. `has_schema_verb` is false only for transport-envelope
/// discriminators with no backing schema (e.g. `clone_connection`), which
/// carry no documented sample and are pinned separately.
fn catalog_output_kind_discriminators() -> Vec<(String, String, bool)> {
    build_command_catalog()
        .json_discriminators
        .into_iter()
        .filter(|discriminator| discriminator.field == "output_kind")
        .map(|discriminator| {
            (
                discriminator.display,
                discriminator.value,
                discriminator.schema_verb.is_some(),
            )
        })
        .collect()
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
fn operator_envelope_verbs_have_declared_emissions() {
    let catalog_verbs: BTreeSet<String> = operator_envelope_verbs().into_iter().collect();
    let emissions: BTreeSet<String> = operator_emission_output_kinds()
        .into_iter()
        .map(|(display, _)| display)
        .collect();
    let missing: Vec<&str> = catalog_verbs
        .difference(&emissions)
        .map(String::as_str)
        .collect();
    let stale: Vec<&str> = emissions
        .difference(&catalog_verbs)
        .map(String::as_str)
        .collect();

    assert!(
        missing.is_empty() && stale.is_empty(),
        "Operator envelope verbs must be registered in the catalog and in the \
         closed emission table. A missing emission would otherwise allow the \
         output_kind source to drift back toward the live operation action.\n\
         Missing emission declaration(s): {missing:?}\n\
         Stale emission declaration(s): {stale:?}"
    );
}

#[test]
fn operator_emissions_match_catalog_discriminators() {
    let catalog = build_command_catalog();
    let mut failures = Vec::new();

    for (display, output_kind) in operator_emission_output_kinds() {
        let Some(entry) = catalog
            .commands
            .iter()
            .find(|entry| entry.display == display)
        else {
            failures.push(format!("{display}: not present in command catalog"));
            continue;
        };
        let advertised: BTreeSet<&str> = entry
            .json_discriminators
            .iter()
            .filter(|discriminator| discriminator.field == "output_kind")
            .map(|discriminator| discriminator.value.as_str())
            .collect();
        if !advertised.contains(output_kind.as_str()) {
            failures.push(format!(
                "{display}: emission declares output_kind=`{output_kind}` but catalog advertises {advertised:?}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Operator emission declarations drifted from the catalog:\n  - {}",
        failures.join("\n  - ")
    );
}

#[test]
fn kind_field_exceptions_use_kind_intentionally() {
    let catalog = build_command_catalog();
    for &display in KIND_FIELD_EXCEPTIONS {
        let entry = catalog
            .commands
            .iter()
            .find(|c| c.display == display)
            .unwrap_or_else(|| {
                panic!("`{display}` listed in KIND_FIELD_EXCEPTIONS is not in the catalog")
            });
        let has_kind = entry.json_discriminators.iter().any(|d| d.field == "kind");
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
#[ignore = "requires a live hosted fixture; runtime equality is enforced \
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
fn runtime_invocation_args(
    display: &str,
) -> Option<(&'static [&'static str], bool /* expect_ok */)> {
    match display {
        "redact purge list" => Some((&["redact", "purge", "list"], true)),
        "redact list" => Some((&["redact", "list"], true)),
        "discuss list" => Some((&["discuss", "list"], true)),
        "context list" => Some((&["context", "list"], true)),
        "review next" => Some((&["review", "next"], true)),
        "review health" => Some((&["review", "health"], true)),
        // heddle#641 — the swept verbs that run clean (exit 0, full JSON
        // payload) in the shared init'd fixture, verified live before
        // being added here. Each pins runtime emission against the
        // catalog value, including the override-table verbs (`branch` →
        // `thread_list`, `inspect` → `thread_show`, and `maintenance gc` →
        // `gc`).
        // `inspect` names `main` explicitly because the earlier `fork`
        // invocation leaves the shared fixture without a current
        // thread; `ready` (which rejects imported-Git-ref targets and
        // has no equivalent escape hatch here) is runtime-covered by
        // its `agent ready` delegation probe instead.
        "abort" => Some((&["abort"], true)),
        "continue" => Some((&["continue"], true)),
        "doctor" => Some((&["doctor"], true)),
        "log" => Some((&["log"], true)),
        "maintenance gc" => Some((&["maintenance", "gc"], true)),
        "maintenance inspect" => Some((&["maintenance", "inspect"], true)),
        "maintenance repack" => Some((&["maintenance", "repack"], true)),
        "maintenance refresh" => Some((&["maintenance", "refresh"], true)),
        "query" => Some((&["query"], true)),
        "remote list" => Some((&["remote", "list"], true)),
        "timeline status" => Some((&["agent", "timeline", "status"], true)),
        "timeline record-start" => Some((
            &[
                "timeline",
                "record-start",
                "--tool-call",
                "call-output-kind",
                "--tool-name",
                "read",
                "--summary",
                "output-kind fixture",
                "--payload-hash",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ],
            true,
        )),
        "timeline record-finish" => Some((
            &[
                "timeline",
                "record-finish",
                "--tool-call",
                "call-output-kind",
                "--status",
                "succeeded",
                "--summary",
                "output-kind fixture",
                "--payload-hash",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ],
            true,
        )),
        "agent task create" => Some((
            &[
                "agent",
                "task",
                "create",
                "--task-id",
                "task-output-kind",
                "--title",
                "Output kind",
                "--thread",
                "main",
            ],
            true,
        )),
        "agent task list" => Some((&["agent", "task", "list"], true)),
        "agent task show" => Some((&["agent", "task", "show", "task-output-kind"], true)),
        "agent task update" => Some((
            &[
                "agent",
                "task",
                "update",
                "task-output-kind",
                "--status",
                "in-progress",
            ],
            true,
        )),
        "agent fanout plan" => Some((
            &[
                "agent",
                "fanout",
                "plan",
                "--title",
                "Output kind fanout",
                "--lane",
                "feature/fanout-plan-output-kind=Output kind lane",
            ],
            true,
        )),
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
    let parsed: Value = serde_json::from_str(first_line).expect("init stdout is parseable JSON");
    assert_eq!(
        parsed.get("output_kind").and_then(|v| v.as_str()),
        Some("init"),
        "`heddle init --output json` must emit `output_kind: \"init\"`; payload: {first_line}"
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

/// The set of `output_kind` values the catalog advertises for one command
/// display path (a command MAY advertise several — `undo` advertises two,
/// `clone` two).
fn advertised_output_kinds(display: &str) -> BTreeSet<String> {
    catalog_output_kind_discriminators()
        .into_iter()
        .filter(|(d, _, _)| d == display)
        .map(|(_, value, _)| value)
        .collect()
}

/// The `output_kind` of the first JSON record `argv` prints in `dir`.
fn emitted_output_kind(argv: &[&str], dir: &std::path::Path) -> String {
    let output =
        heddle_output(argv, Some(dir)).unwrap_or_else(|err| panic!("spawn {argv:?}: {err}"));
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
        .get("output_kind")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{argv:?} payload missing `output_kind`: {first_line}"))
        .to_string()
}

/// Close-the-class guard for the heddle#473 verb consolidation: a command can
/// emit MORE than one `output_kind` from a single command path — `undo` emits
/// `undo` (the rewind / `--preview`), `undo_list` (`--list`), and
/// `undo_recover` (`--recover`). Every value a
/// handler can emit must be in that command's advertised catalog discriminator
/// set, or an agent that validates responses against `heddle help --output
/// json` rejects the off-contract record.
///
/// `redo` is folded into `undo --redo`, so the `undo` catalog entry owns the
/// `redo` output_kind too.
///
/// The static catalog tests above only confirm the *first* `output_kind`
/// discriminator matches the display path; they cannot see the alternate kinds
/// a `--flag` path emits. This test drives every JSON-emitting variant and
/// asserts the emitted kind is advertised for the command that produced it.
///
/// New multi-`output_kind` command paths MUST add their variants below.
#[test]
fn folded_verb_flag_variants_emit_only_advertised_output_kinds() {
    let undo_advertised = advertised_output_kinds("undo");
    assert!(
        undo_advertised.is_superset(&BTreeSet::from([
            "undo".to_string(),
            "undo_list".to_string(),
            "redo".to_string(),
            "undo_recover".to_string(),
        ])),
        "catalog must advertise all undo-mode output_kinds; advertised: {undo_advertised:?}"
    );

    // Fixture with redo-able history: two commits, so an `undo` leaves exactly
    // one batch to redo.
    let temp = init_fixture();
    std::fs::write(temp.path().join("a.txt"), "one").unwrap();
    heddle(&["capture", "-m", "first"], Some(temp.path())).expect("capture first");
    std::fs::write(temp.path().join("a.txt"), "two").unwrap();
    heddle(&["capture", "-m", "second"], Some(temp.path())).expect("capture second");

    assert_undo_requires_hard(temp.path());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "two",
        "the rejected unqualified undo must preserve the current captured file"
    );

    // Drive each JSON-emitting variant, in an order that keeps the repo
    // consistent: undo --list (read-only) → undo --hard (explicitly rewinds,
    // making a redo available) → undo --redo (re-applies) → undo --hard →
    // undo --recover. The second hard undo recreates a clean recovery baseline
    // before recovery restores it as worktree changes.
    let cases: &[(&[&str], &str, &str)] = &[
        (&["--output", "json", "undo", "--list"], "undo_list", "undo"),
        (&["--output", "json", "undo", "--hard"], "undo", "undo"),
        (&["--output", "json", "undo", "--redo"], "redo", "undo"),
        (&["--output", "json", "undo", "--hard"], "undo", "undo"),
        (
            &["--output", "json", "undo", "--recover"],
            "undo_recover",
            "undo",
        ),
    ];

    let mut failures = Vec::new();
    for (argv, expected, display) in cases {
        let advertised = advertised_output_kinds(display);
        let kind = emitted_output_kind(argv, temp.path());
        if kind != *expected {
            failures.push(format!(
                "{argv:?}: emitted output_kind=`{kind}`, expected `{expected}`"
            ));
        }
        if !advertised.contains(&kind) {
            failures.push(format!(
                "{argv:?}: emitted output_kind=`{kind}` is NOT in the catalog-advertised \
                 set for `{display}` ({advertised:?}) — off-contract"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "undo/redo variants emit output_kinds outside the advertised set:\n  - {}",
        failures.join("\n  - ")
    );
}
