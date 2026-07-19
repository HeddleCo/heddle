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
    documented_samples_with_bound_verbs, operator_emission_output_kinds, operator_envelope_verbs,
    schema_for_verb,
};
use serde_json::Value;
use tempfile::TempDir;

use super::{git_hermetic, heddle, heddle_output};

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
    "commit",
    "clone",
    "diff",
    "undo",
    "thread list",
    "thread show",
    "doctor docs",
    "doctor schemas",
    "schemas",
    "import git",
    "export git",
    "sync git",
    "status",
    // heddle#1057 — whoami emits `output_kind: "whoami"` (matches its verb path,
    // no override needed).
    "whoami",
    // heddle#272 — output_kind sweep on the named-by-persona verbs.
    "agent presence list",
    "agent presence show",
    "agent presence explain",
    "agent presence complete",
    "auth logout",
    "auth status",
    "auth create-service-token",
    "revert",
    "redact apply",
    "redact list",
    "redact purge apply",
    "redact purge list",
    "redact show",
    "redact trust add",
    "redact trust list",
    "redact trust remove",
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
    "agent serve",
    "agent status",
    "agent stop",
    "agent task create",
    "agent task list",
    "agent task show",
    "agent task update",
    "agent fanout plan",
    "agent fanout start",
    "continue",
    "daemon stop",
    "doctor",
    "expand",
    "land",
    "log",
    "maintenance gc",
    "maintenance inspect",
    "maintenance refresh",
    "oplog recover",
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
    "timeline status",
    "timeline record-start",
    "timeline record-finish",
    "timeline fork",
    "timeline reset",
    "timeline recover",
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
    "collapse",
    "daemon serve",
    "daemon status",
    "fsck",
    "fsck repair git",
    "git-overlay",
    "hook events",
    "hook install",
    "hook list",
    "hook uninstall",
    "integration doctor",
    "integration install",
    "integration list",
    "integration relay",
    "integration uninstall",
    "integration upgrade",
    "retro",
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
    "try",
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
        // Timeline navigation subcommands intentionally share one action
        // envelope so agents can handle fork/reset/recover uniformly.
        "timeline fork" | "timeline reset" | "timeline recover" => Some("timeline_action"),
        _ => None,
    }
}

/// Read `docs/json-schemas.md` from the workspace root.
fn read_json_schemas_doc() -> String {
    let doc_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("docs/json-schemas.md");
    std::fs::read_to_string(&doc_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", doc_path.display()))
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
fn doc_samples_carry_catalog_output_kind_for_every_discriminated_verb() {
    // heddle#272 r5/r8 (Codex P1, cid 3318094405): every documented sample
    // for a verb whose catalog advertises an `output_kind` discriminator
    // must show that discriminator with a catalog-advertised value. The
    // `doctor schemas` gate only checks that a sample's keys are a subset
    // of the schema's properties (it does not enforce required-field
    // presence), so it would NOT catch a sample that silently drops or
    // misnames `output_kind`; this test does.
    //
    // This drives from the FULL catalog discriminator set and binds each
    // sample to its verb the same way `doctor schemas` does (heading +
    // inline hint), so it is exhaustive over every documented sample — not
    // a hand-maintained subset. A grouped sample bound to several verbs
    // (e.g. the single `heddle undo|redo` sample) is accepted when its
    // `output_kind` matches ANY of the verbs it binds to.
    let doc = read_json_schemas_doc();

    // display -> set of advertised output_kind values (clone advertises
    // both `clone` and `clone_connection`).
    let mut advertised: std::collections::BTreeMap<String, BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for (display, value, _) in catalog_output_kind_discriminators() {
        advertised.entry(display).or_default().insert(value);
    }
    let verbs: Vec<&str> = advertised.keys().map(String::as_str).collect();

    let mut failures = Vec::new();
    let mut checked = 0usize;
    for (sample, bound) in documented_samples_with_bound_verbs(&doc, &verbs) {
        let allowed: BTreeSet<&str> = bound
            .iter()
            .filter_map(|verb| advertised.get(verb))
            .flat_map(|values| values.iter().map(String::as_str))
            .collect();
        let Some(object) = sample.as_object() else {
            failures.push(format!(
                "sample bound to {bound:?} is not a JSON object, so it cannot carry the \
                 required `output_kind` discriminator (catalog advertises {allowed:?})"
            ));
            continue;
        };
        checked += 1;
        match object.get("output_kind").and_then(Value::as_str) {
            None => failures.push(format!(
                "sample bound to {bound:?} omits the `output_kind` discriminator \
                 (catalog advertises {allowed:?})"
            )),
            Some(found) if !allowed.contains(found) => failures.push(format!(
                "sample bound to {bound:?} declares output_kind=`{found}`, which is not a \
                 catalog-advertised value for those verbs ({allowed:?})"
            )),
            Some(_) => {}
        }
    }

    assert!(
        failures.is_empty(),
        "Documented samples drift from the catalog `output_kind` contract. The catalog is the \
         source of truth; every sample bound to a discriminator verb must carry a catalog \
         value:\n  - {}",
        failures.join("\n  - ")
    );
    assert!(
        checked >= 30,
        "expected the catalog-driven doc sweep to inspect many discriminator samples; only \
         {checked} were bound — the heading/inline binding likely regressed"
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
        "redact trust list" => Some((&["redact", "trust", "list"], true)),
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
        "maintenance refresh" => Some((&["maintenance", "refresh"], true)),
        "query" => Some((&["query"], true)),
        "remote list" => Some((&["remote", "list"], true)),
        "timeline status" => Some((&["timeline", "status"], true)),
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
                "feature/fanout-plan-output-kind=../fanout-plan-output-kind:Output kind lane",
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

/// Top-level key set of the first JSON object emitted by `argv` in
/// `dir`. Panics with the captured output on failure so doc-vs-runtime
/// mismatches surface a readable diff.
fn runtime_top_level_keys(argv: &[&str], dir: &std::path::Path) -> BTreeSet<String> {
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
    let parsed: Value = serde_json::from_str(first_line).unwrap_or_else(|line_err| {
        serde_json::from_str(stdout.trim()).unwrap_or_else(|full_err| {
            panic!(
                "{argv:?} stdout not JSON: first line error: {line_err}; full stdout error: {full_err}\n  stdout: {stdout}"
            )
        })
    });
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

fn sv(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// `state_id` of the current HEAD state in `dir` (first `log` record).
fn head_state_id(dir: &std::path::Path) -> String {
    let stdout = heddle(&["--output", "json", "log"], Some(dir)).expect("heddle log");
    let first = stdout.lines().next().unwrap_or("");
    let parsed: Value = serde_json::from_str(first).expect("log stdout is JSON");
    parsed["states"][0]["state_id"]
        .as_str()
        .expect("log states[0].state_id")
        .to_string()
}

/// Build a fixture repo carrying exactly the state `output_kind`'s verb needs,
/// plus the argv (after `--output json`) that drives it. `None` means the verb
/// has no synthetic-fixture invocation here — the caller then requires the
/// verb's registered schema to pin every documented key instead.
///
/// Returning a value here is the structural anti-subset guarantee: a #272 verb
/// with a documented sample and a *generic* schema (one that pins none of the
/// real fields, as the inline `serde_json::json!` verbs do) MUST appear in this
/// match or the invariant test fails demanding it.
struct RuntimeDocCase {
    _fixture: TempDir,
    cwd: std::path::PathBuf,
    argv: Vec<String>,
}

impl RuntimeDocCase {
    fn at_root(fixture: TempDir, argv: Vec<String>) -> Self {
        let cwd = fixture.path().to_path_buf();
        Self {
            _fixture: fixture,
            cwd,
            argv,
        }
    }
}

fn init_repo_at(path: &std::path::Path) {
    std::fs::create_dir_all(path).expect("create fixture repository directory");
    heddle(
        &[
            "init",
            "--principal-name",
            "Heddle Test",
            "--principal-email",
            "heddle@test.example",
        ],
        Some(path),
    )
    .expect("initialize fixture repository");
}

fn transport_runtime_doc_case(output_kind: &str) -> Option<RuntimeDocCase> {
    let fixture = TempDir::new().expect("transport fixture");
    let remote = fixture.path().join("remote");
    let checkout = fixture.path().join("checkout");

    init_repo_at(&remote);
    std::fs::write(remote.join("base.txt"), "base\n").expect("write remote base");
    heddle(&["capture", "-m", "base"], Some(&remote)).expect("capture remote base");

    if output_kind == "clone" {
        return Some(RuntimeDocCase {
            cwd: fixture.path().to_path_buf(),
            argv: vec![
                "clone".to_string(),
                remote.to_string_lossy().into_owned(),
                checkout.to_string_lossy().into_owned(),
            ],
            _fixture: fixture,
        });
    }

    heddle(
        &[
            "clone",
            remote.to_str().expect("UTF-8 remote path"),
            checkout.to_str().expect("UTF-8 checkout path"),
        ],
        Some(fixture.path()),
    )
    .expect("clone transport fixture");

    let argv = match output_kind {
        "pull" => {
            std::fs::write(remote.join("base.txt"), "remote update\n")
                .expect("write remote update");
            heddle(&["capture", "-m", "remote update"], Some(&remote))
                .expect("capture remote update");
            sv(&["pull"])
        }
        _ => return None,
    };
    Some(RuntimeDocCase {
        _fixture: fixture,
        cwd: checkout,
        argv,
    })
}

fn runtime_doc_case(output_kind: &str) -> Option<RuntimeDocCase> {
    if matches!(output_kind, "clone" | "pull") {
        return transport_runtime_doc_case(output_kind);
    }
    if output_kind == "land_batch" {
        let fixture = TempDir::new().expect("land batch fixture");
        let work = fixture.path().join("work");
        let alpha = fixture.path().join("alpha");
        let beta = fixture.path().join("beta");
        std::fs::create_dir_all(&work).expect("create Git Overlay worktree");
        git_hermetic(&["init", "-b", "main"], &work);
        git_hermetic(&["config", "user.name", "Heddle Test"], &work);
        git_hermetic(&["config", "user.email", "heddle@test.example"], &work);
        std::fs::write(work.join("README.md"), "base\n").expect("seed Git worktree");
        git_hermetic(&["add", "README.md"], &work);
        git_hermetic(&["commit", "-m", "base"], &work);
        heddle(&["init"], Some(&work)).expect("initialize Git Overlay");
        heddle(&["import", "git", "--ref", "main"], Some(&work)).expect("import main");
        for (thread, path, file) in [
            ("alpha", alpha.as_path(), "alpha.txt"),
            ("beta", beta.as_path(), "beta.txt"),
        ] {
            heddle(
                &[
                    "start",
                    thread,
                    "--path",
                    path.to_str().expect("UTF-8 thread path"),
                    "--workspace",
                    "solid",
                ],
                Some(&work),
            )
            .expect("start batch peer");
            std::fs::write(path.join(file), format!("{thread}\n")).expect("write peer work");
            heddle(&["ready", "-m", &format!("ready {thread}")], Some(path))
                .expect("ready batch peer");
        }
        return Some(RuntimeDocCase {
            _fixture: fixture,
            cwd: work,
            argv: sv(&["land", "--threads", "alpha,beta"]),
        });
    }
    let case = match output_kind {
        "thread_switch" => {
            let t = init_fixture();
            std::fs::write(t.path().join("a.txt"), "base").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture");
            (t, sv(&["thread", "switch", "main"]))
        }
        "revert" => {
            let t = init_fixture();
            std::fs::write(t.path().join("a.txt"), "base").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture base");
            std::fs::write(t.path().join("a.txt"), "base\nmore").unwrap();
            heddle(&["capture", "-m", "second"], Some(t.path())).expect("capture second");
            (t, sv(&["revert", "HEAD"]))
        }
        "redact_apply" => {
            let t = init_fixture();
            std::fs::write(t.path().join("secrets.env"), "TOKEN=abc").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture");
            (
                t,
                sv(&[
                    "redact",
                    "apply",
                    "HEAD",
                    "--path",
                    "secrets.env",
                    "--reason",
                    "credential",
                ]),
            )
        }
        "purge_apply" => {
            let t = init_fixture();
            std::fs::write(t.path().join("secrets.env"), "TOKEN=abc").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture");
            heddle(
                &[
                    "redact",
                    "apply",
                    "HEAD",
                    "--path",
                    "secrets.env",
                    "--reason",
                    "credential",
                ],
                Some(t.path()),
            )
            .expect("redact apply");
            (
                t,
                sv(&[
                    "redact",
                    "purge",
                    "apply",
                    "HEAD",
                    "--path",
                    "secrets.env",
                    "--force",
                ]),
            )
        }
        "query_attribution" => {
            let t = init_fixture();
            std::fs::create_dir_all(t.path().join("src")).unwrap();
            std::fs::write(t.path().join("src/lib.rs"), "pub fn run() {}\n").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture");
            (
                t,
                sv(&["query", "--attribution", "src/lib.rs", "--context"]),
            )
        }
        "redact_trust_add" => (
            init_fixture(),
            sv(&[
                "redact",
                "trust",
                "add",
                "--public-key",
                "abc123def456",
                "--algorithm",
                "ed25519",
                "--label",
                "security",
            ]),
        ),
        "discuss_open" => {
            let t = init_fixture();
            std::fs::write(t.path().join("a.txt"), "fn verify(){}").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture");
            (
                t,
                sv(&["discuss", "open", "a.txt", "verify", "check edge case"]),
            )
        }
        "discuss_list" => (init_fixture(), sv(&["discuss", "list"])),
        "context_set" => {
            let t = init_fixture();
            std::fs::write(t.path().join("a.txt"), "code").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture");
            (
                t,
                sv(&[
                    "context",
                    "set",
                    "--path",
                    "a.txt",
                    "--scope",
                    "file",
                    "-m",
                    "owner note",
                ]),
            )
        }
        "context_list" => (init_fixture(), sv(&["context", "list"])),
        "review_show" => {
            let t = init_fixture();
            std::fs::write(t.path().join("a.txt"), "fn verify(){}").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture");
            let cid = head_state_id(t.path());
            (t, vec!["review".to_string(), "show".to_string(), cid])
        }
        "review_next" => {
            let t = init_fixture();
            std::fs::write(t.path().join("a.txt"), "base").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture");
            heddle(
                &["start", "review-next", "--workspace", "solid"],
                Some(t.path()),
            )
            .expect("start review-next");
            (t, sv(&["review", "next"]))
        }
        "review_health" => (init_fixture(), sv(&["review", "health"])),
        "visibility_set" => {
            let t = init_fixture();
            std::fs::write(t.path().join("a.txt"), "base").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture");
            (t, sv(&["visibility", "set", "HEAD", "--tier", "internal"]))
        }
        "visibility_promote" => {
            let t = init_fixture();
            std::fs::write(t.path().join("a.txt"), "base").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture");
            heddle(
                &[
                    "visibility",
                    "set",
                    "HEAD",
                    "--tier",
                    "restricted",
                    "--label",
                    "secret",
                ],
                Some(t.path()),
            )
            .expect("visibility set");
            (
                t,
                sv(&["visibility", "promote", "HEAD", "--tier", "internal"]),
            )
        }
        "visibility_show" => {
            let t = init_fixture();
            std::fs::write(t.path().join("a.txt"), "base").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture");
            heddle(
                &["visibility", "set", "HEAD", "--tier", "internal"],
                Some(t.path()),
            )
            .expect("visibility set");
            (t, sv(&["visibility", "show", "HEAD"]))
        }
        "visibility_list" => {
            let t = init_fixture();
            std::fs::write(t.path().join("a.txt"), "base").unwrap();
            heddle(&["capture", "-m", "base"], Some(t.path())).expect("capture");
            heddle(
                &["visibility", "set", "HEAD", "--tier", "internal"],
                Some(t.path()),
            )
            .expect("visibility set");
            (t, sv(&["visibility", "list"]))
        }
        // heddle#641 — the newly-swept generic-schema verbs. Their opaque
        // mirrors pin no fields, so the documented sample must be compared
        // against the live payload here.
        "gc" => (init_fixture(), sv(&["maintenance", "gc"])),
        // Stopping a daemon that is not running still exits 0 with the
        // full `daemon_stop` payload, so a bare init fixture suffices.
        "daemon_stop" => (init_fixture(), sv(&["daemon", "stop"])),
        "oplog_recover" => {
            // Seed three captures, then truncate the packed oplog so the index
            // trailer is destroyed and any read takes the forward-greedy salvage
            // path.
            //
            // DETERMINISM (heddle#272 flake fix): the `oplog recover` payload has
            // TWO shapes depending on WHO performs the salvage:
            //   * the everyday read path's silent auto-fallback salvaged first →
            //     handler reports `prior_recovery: true` from the durable
            //     `.oplog.recovery` sidecar, `quarantine_path` OMITTED; or
            //   * the `recover` handler itself is the first to touch the damaged
            //     body → `prior_recovery: false`, `quarantine_path` PRESENT.
            // Which one fires is a race between the recover command's own
            // repo-open reads (reconciler / status hooks call
            // `PackedOpLogIndex::open`, which auto-salvages) and the handler's
            // explicit `recover()`. That race resolved differently across
            // environments — green locally (auto-fallback won) but red on CI
            // (handler won, emitting the extra `quarantine_path` key), so the
            // documented sample's key set drifted from runtime non-deterministically.
            //
            // Pin the auto-fallback variant by running a benign read FIRST: it
            // forces the silent salvage to complete (heals `oplog.bin`, quarantines
            // the damaged original, writes the sidecar) BEFORE `oplog recover`
            // opens the repo. From that point the handler ALWAYS sees a healthy
            // oplog plus the sidecar and reports `prior_recovery: true` with no
            // `quarantine_path` — the stable operator key set the doc documents.
            let t = init_fixture();
            for i in 1..=3 {
                std::fs::write(t.path().join("f.txt"), format!("v{i}")).unwrap();
                heddle(&["capture", "-m", &format!("c{i}")], Some(t.path()))
                    .expect("fixture capture");
            }
            let oplog = t.path().join(".heddle/oplog/oplog.bin");
            let bytes = std::fs::read(&oplog).expect("read fixture oplog");
            let cut = bytes.len() * 6 / 10;
            std::fs::write(&oplog, &bytes[..cut]).expect("truncate fixture oplog");
            // Benign read: deterministically drive the silent auto-fallback to
            // salvage the oplog before the recover handler ever opens the repo.
            heddle(&["status"], Some(t.path())).expect("status triggers oplog auto-recovery");
            (t, sv(&["oplog", "recover"]))
        }
        "timeline_log" => (init_fixture(), sv(&["log", "--timeline"])),
        _ => return None,
    };
    Some(RuntimeDocCase::at_root(case.0, case.1))
}

/// Top-level property names declared by the registered schema for `verb`, or
/// an empty set when the verb has no schema or only a generic envelope.
fn schema_property_names(verb: &str) -> BTreeSet<String> {
    let Some(schema) = schema_for_verb(verb) else {
        return BTreeSet::new();
    };
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default()
}

#[test]
fn doc_samples_match_runtime_for_every_catalog_discriminator() {
    // heddle#272 r7/r8 (Codex P2, cid 3319783461): the r7 doc-sample-vs-runtime
    // check iterated a hand-maintained `SWEPT_272` subset, so documented
    // samples for earlier-swept verbs (`init`, `status`, `verify`, `clone`,
    // `thread list/show`, the bridge/doctor commands, ...) were never compared
    // against live payloads and could drift silently. This invariant re-roots
    // the loop on the FULL catalog discriminator set, so every verb the catalog
    // advertises an `output_kind` for is checked.
    //
    // For each catalog `output_kind` value with a documented sample, the sample
    // must either
    //
    //   (a) be invoked here against a fixture, with the documented sample's
    //       top-level key set asserted EXACTLY equal to the live payload's, or
    //   (b) have a registered schema that pins every documented key
    //       (doc-keys ⊆ schema-properties), so `doctor schemas` guards it.
    //
    // (b) covers verbs that can't be exercised with a synthetic fixture
    // (`review sign` cryptographically validates its `--signature`; `verify`/
    // `status` need elaborate verification fixtures). Crucially, the inline
    // `serde_json::json!` opaque verbs resolve to a GENERIC schema
    // (`additionalProperties: true`) that pins NONE of their real fields, so
    // (b) fails for them and they are forced down path (a). A new generic-schema
    // verb documented without a runtime case here fails this test rather than
    // deferring to a later round.
    //
    // heddle#641: the loop is grouped per VALUE rather than per (display,
    // value) row because one wire value can be advertised by several
    // commands (`thread_list` by `thread list` AND its `branch` alias;
    // `ready` by `ready` AND `agent ready`; `thread` by the drop/promote/
    // refresh trio). `doc_sample_top_level_keys` resolves a value to ONE
    // documented sample, so the sample is guarded once — by the runtime
    // case, or by ANY advertising display whose schema pins every
    // documented key. Requiring EVERY display to pin would force alias
    // schemas (e.g. `branch`'s mutation mirror) to model their sibling's
    // listing shape.
    let doc = read_json_schemas_doc();

    let mut failures = Vec::new();
    let mut covered_by_runtime = 0usize;
    let mut covered_by_schema = 0usize;

    // value -> displays advertising it (schema-verb-backed rows only;
    // transport-envelope discriminators like `clone_connection` have no
    // schema verb and no documented sample — they are pinned separately).
    let mut advertising: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (display, value, has_schema_verb) in catalog_output_kind_discriminators() {
        if has_schema_verb {
            advertising.entry(value).or_default().push(display);
        }
    }

    for (value, displays) in &advertising {
        let Some(doc_keys) = doc_sample_top_level_keys(&doc, value) else {
            // This value is not individually documented (it rides a grouped
            // sample under a representative sibling, e.g. `redo` under `undo`,
            // `purge list` under `purge_apply`) — nothing to compare here.
            // `doc_samples_carry_catalog_output_kind_for_every_discriminated_verb`
            // still guards the grouped sample's discriminator.
            continue;
        };

        // `doc_sample_top_level_keys` matched on the `output_kind` value, so
        // the key is present by construction; assert it defensively.
        if !doc_keys.contains("output_kind") {
            failures.push(format!(
                "{value} ({displays:?}): documented sample is missing the `output_kind` key"
            ));
            continue;
        }

        // Push payload variants are already exercised by the dedicated native
        // and Git-overlay remote matrices. Recreating those transport cases in
        // this catalog sweep can block on transport behavior unrelated to the
        // discriminator contract this module owns.
        if value == "push" {
            continue;
        }

        if let Some(case) = runtime_doc_case(value) {
            let argv_refs: Vec<&str> = std::iter::once("--output")
                .chain(std::iter::once("json"))
                .chain(case.argv.iter().map(String::as_str))
                .collect();
            let runtime_keys = runtime_top_level_keys(&argv_refs, &case.cwd);
            if !runtime_keys.contains("output_kind") {
                failures.push(format!(
                    "{value} ({displays:?}): runtime payload is missing `output_kind` (keys: {runtime_keys:?})"
                ));
            }
            if doc_keys != runtime_keys {
                let doc_only: Vec<&String> = doc_keys.difference(&runtime_keys).collect();
                let runtime_only: Vec<&String> = runtime_keys.difference(&doc_keys).collect();
                failures.push(format!(
                    "{value} ({displays:?}): documented sample does not match the live \
                     `--output json` payload.\n      in doc only:     {doc_only:?}\n      \
                     in runtime only: {runtime_only:?}\n      doc keys:     {doc_keys:?}\n      \
                     runtime keys: {runtime_keys:?}"
                ));
            }
            covered_by_runtime += 1;
            continue;
        }

        // No runtime case: at least one advertising display's registered
        // schema MUST pin every documented key, otherwise the sample is
        // unguarded and could drift freely.
        let pinned_by_some_display = displays.iter().any(|display| {
            let schema_props = schema_property_names(display);
            doc_keys
                .iter()
                .filter(|k| k.as_str() != "output_kind")
                .all(|k| schema_props.contains(k))
        });
        if pinned_by_some_display {
            covered_by_schema += 1;
        } else {
            failures.push(format!(
                "{value} ({displays:?}): no runtime case AND no advertising display's \
                 registered schema pins every documented key (schema is generic or models \
                 a different shape). Add a `runtime_doc_case` arm so the sample is checked \
                 against the live payload."
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Documented samples drifted from runtime / are unguarded:\n  - {}",
        failures.join("\n  - ")
    );

    // Sanity floor: the #272 persona verbs are runtime-checked, and the
    // earlier-swept verbs lean on the schema path. If either collapses the
    // harness silently stopped covering anything.
    assert!(
        covered_by_runtime >= 18,
        "expected the sweep to runtime-check most documented persona verbs; only {covered_by_runtime} ran"
    );
    assert!(
        covered_by_schema >= 5,
        "expected several schema-guarded earlier-swept verbs (clone, status, verify, ...); got {covered_by_schema}"
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

    // Drive each JSON-emitting variant, in an order that keeps the repo
    // consistent: undo --list (read-only) → undo (rewinds, making a redo
    // available) → undo --redo (re-applies) → undo → undo --recover. The
    // second undo recreates a clean recovery baseline before recovery restores
    // it as worktree changes.
    let cases: &[(&[&str], &str, &str)] = &[
        (&["--output", "json", "undo", "--list"], "undo_list", "undo"),
        (&["--output", "json", "undo"], "undo", "undo"),
        (&["--output", "json", "undo", "--redo"], "redo", "undo"),
        (&["--output", "json", "undo"], "undo", "undo"),
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
