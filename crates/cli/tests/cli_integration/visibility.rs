// SPDX-License-Identifier: Apache-2.0
//! End-to-end coverage for the `heddle visibility` verb family and the
//! Invariant-A (immutable-at-capture) default-tier binding (heddle#317).
//!
//! The conformance test (spike #266 §5.4) is the load-bearing one: a state
//! captured under a restrictive repo default must keep that tier even after
//! the default later drifts more-open. Resolution happens once, at capture —
//! never recomputed at serve time — so a config drift cannot retroactively
//! expose an already-captured state.
//!
//! These tests drive the CLI binary as a subprocess so they exercise the full
//! args → handler → repo → sidecar stack rather than poking at internals.

use std::{fs, path::Path};

use serde_json::Value;
use tempfile::TempDir;

use super::heddle;

/// Overwrite `.heddle/config.toml` with a minimal-but-valid config that pins
/// `[review.discussion] default_visibility` to `tier_toml`. Every other field
/// falls back to its serde default, so this is enough to drive capture-time
/// resolution deterministically. `tier_toml` is the raw TOML value for the
/// tier (e.g. `"\"Public\""` or `"{ Restricted = { scope_label = \"embargo\" } }"`).
fn set_repo_default_visibility(repo: &Path, tier_toml: &str) {
    let config_path = repo.join(".heddle/config.toml");
    let contents = format!(
        "[repository]\nversion = 1\n\n[review.discussion]\ndefault_visibility = {tier_toml}\n"
    );
    fs::write(&config_path, contents).expect("write repo config");
}

/// `heddle init` then capture one state; return the temp dir and the captured
/// state's change id (full).
fn init_and_capture(label: &str) -> (TempDir, String) {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("note.txt"), label.as_bytes()).unwrap();
    (temp, String::new())
}

fn capture_state(temp: &Path, message: &str) -> String {
    heddle(&["capture", "-m", message], Some(temp)).expect("capture");
    let raw = heddle(&["--output", "json", "log", "--limit", "1"], Some(temp)).unwrap();
    let value: Value = serde_json::from_str(&raw).unwrap();
    value["states"][0]["change_id"]
        .as_str()
        .expect("log --output json should expose change_id")
        .to_string()
}

/// The change id of the current HEAD state (newest in the log).
fn latest_state(temp: &Path) -> String {
    let raw = heddle(&["--output", "json", "log", "--limit", "1"], Some(temp)).unwrap();
    let value: Value = serde_json::from_str(&raw).unwrap();
    value["states"][0]["change_id"]
        .as_str()
        .expect("log --output json should expose change_id")
        .to_string()
}

fn show_json(temp: &Path, state: &str) -> Value {
    let raw = heddle(
        &["--output", "json", "visibility", "show", state],
        Some(temp),
    )
    .expect("visibility show");
    serde_json::from_str(&raw).expect("visibility show output should be JSON")
}

#[test]
fn invariant_a_captured_tier_unchanged_when_default_drifts_public() {
    // Capture under a restrictive default…
    let (temp, _) = init_and_capture("secret");
    set_repo_default_visibility(temp.path(), "{ Restricted = { scope_label = \"embargo\" } }");
    let state = capture_state(temp.path(), "captured under embargo");

    let before = show_json(temp.path(), &state);
    assert_eq!(before["output_kind"], "visibility_show");
    assert_eq!(
        before["tier"], "restricted",
        "state captured under a restricted default must resolve restricted: {before}"
    );
    assert_eq!(before["label"], "embargo");
    assert_eq!(before["effective_public"], false);

    // …drift the default wide open to public…
    set_repo_default_visibility(temp.path(), "\"Public\"");

    // …and the already-captured state's effective tier is unchanged. The
    // tier was bound at capture; serve-time never re-resolves from config.
    let after = show_json(temp.path(), &state);
    assert_eq!(
        after["tier"], "restricted",
        "drifting the default to public must NOT retroactively expose a captured state: {after}"
    );
    assert_eq!(after["label"], "embargo");
    assert_eq!(after["effective_public"], false);
}

#[test]
fn capture_still_applies_default_visibility() {
    // Capture funnels through the snapshot chokepoint where default visibility
    // is now bound; a non-public default must still stamp the captured state.
    let (temp, _) = init_and_capture("secret");
    set_repo_default_visibility(temp.path(), "\"Internal\"");
    let state = capture_state(temp.path(), "captured internal");

    let show = show_json(temp.path(), &state);
    assert_eq!(
        show["tier"], "internal",
        "capture must inherit the configured non-public default: {show}"
    );
    assert_eq!(show["effective_public"], false);
}

#[test]
fn cherry_pick_state_gets_default_visibility() {
    // PR #529 P1: the default-visibility binding was only on the `capture`
    // call site, so cherry-pick created a state left public even under a
    // non-public default. Now it funnels through the snapshot chokepoint.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("note.txt"), b"base").unwrap();
    heddle(&["capture", "-m", "first"], Some(temp.path())).expect("capture first");
    let first = latest_state(temp.path());

    fs::write(temp.path().join("note.txt"), b"modified").unwrap();
    heddle(&["capture", "-m", "second"], Some(temp.path())).expect("capture second");

    // Pin a restrictive default, then cherry-pick the earlier state. The new
    // snapshot it commits must inherit the restricted tier — not stay public.
    set_repo_default_visibility(temp.path(), "{ Restricted = { scope_label = \"embargo\" } }");
    heddle(&["cherry-pick", &first], Some(temp.path())).expect("cherry-pick");

    let new_state = latest_state(temp.path());
    let show = show_json(temp.path(), &new_state);
    assert_eq!(
        show["tier"], "restricted",
        "cherry-picked state must inherit the restricted default via the chokepoint: {show}"
    );
    assert_eq!(show["effective_public"], false);
}

#[test]
fn revert_state_gets_default_visibility() {
    // Sibling of the cherry-pick leak: a revert creates a new state too, and
    // must inherit the configured non-public default through the chokepoint.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("note.txt"), b"base").unwrap();
    heddle(&["capture", "-m", "first"], Some(temp.path())).expect("capture first");

    fs::write(temp.path().join("note.txt"), b"modified").unwrap();
    heddle(&["capture", "-m", "second"], Some(temp.path())).expect("capture second");
    let second = latest_state(temp.path());

    set_repo_default_visibility(temp.path(), "\"Internal\"");
    heddle(&["revert", &second], Some(temp.path())).expect("revert");

    let new_state = latest_state(temp.path());
    let show = show_json(temp.path(), &new_state);
    assert_eq!(
        show["tier"], "internal",
        "reverted state must inherit the internal default via the chokepoint: {show}"
    );
    assert_eq!(show["effective_public"], false);
}

#[test]
fn undo_after_capture_with_nonpublic_default_reverts_snapshot_and_visibility_in_one_undo() {
    // PR #529 P1 regression: the automatic capture-time default-visibility
    // binding used to be its own trailing oplog batch, so the FIRST `undo` after
    // a capture restored only the sidecar and left the snapshot in place — undo
    // took two presses, and the automatic binding polluted undo history. Folding
    // the binding into the snapshot's own batch makes ONE `undo` revert the
    // snapshot AND its auto-applied default tier together.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("note.txt"), b"base").unwrap();
    heddle(&["capture", "-m", "first"], Some(temp.path())).expect("capture first");
    let first = latest_state(temp.path());

    set_repo_default_visibility(temp.path(), "\"Internal\"");
    fs::write(temp.path().join("note.txt"), b"secret").unwrap();
    heddle(&["capture", "-m", "second"], Some(temp.path())).expect("capture second");
    let second = latest_state(temp.path());
    assert_ne!(first, second, "second capture is a distinct state");
    assert_eq!(
        show_json(temp.path(), &second)["tier"],
        "internal",
        "second capture inherits the non-public default"
    );

    // ONE undo must revert BOTH the snapshot (HEAD back to `first`) AND the
    // auto-applied visibility (the captured state reads public-by-absence again).
    heddle(&["undo"], Some(temp.path())).expect("undo capture");

    assert_eq!(
        latest_state(temp.path()),
        first,
        "one undo must revert the snapshot itself — HEAD back to the pre-capture state, \
         not just the sidecar"
    );
    let after = show_json(temp.path(), &second);
    assert_eq!(
        after["effective_public"], true,
        "the same single undo must also revert the auto-applied default visibility: {after}"
    );
    assert_eq!(after["tier"], "public");
}

#[test]
fn explicit_visibility_set_remains_its_own_undoable_batch() {
    // r2 preserved: an explicit `heddle visibility set` is a SEPARATE undoable
    // batch from the capture — only the AUTOMATIC capture-time default binding
    // folds into the snapshot batch. One undo of the set reverts the tier and
    // leaves the captured snapshot in place as HEAD.
    let (temp, _) = init_and_capture("ordinary"); // public default ⇒ capture has no auto binding
    let state = capture_state(temp.path(), "ordinary capture");
    assert_eq!(
        show_json(temp.path(), &state)["effective_public"],
        true,
        "capture under the public default writes no visibility record"
    );

    heddle(
        &["visibility", "set", &state, "--tier", "internal"],
        Some(temp.path()),
    )
    .expect("visibility set");
    assert_eq!(show_json(temp.path(), &state)["tier"], "internal");

    // One undo reverts ONLY the explicit set; the snapshot stays HEAD (proving
    // the set is its own batch, distinct from the capture).
    heddle(&["undo"], Some(temp.path())).expect("undo set");
    assert_eq!(
        latest_state(temp.path()),
        state,
        "undo of the explicit set must NOT revert the capture — the set is its own batch"
    );
    let after = show_json(temp.path(), &state);
    assert_eq!(
        after["effective_public"], true,
        "the explicit set is reverted by its own undo: {after}"
    );
    assert_eq!(after["tier"], "public");
}

#[test]
fn public_default_capture_stays_record_free() {
    // The common case: with the default (public) tier, capture writes no
    // visibility record — absence ≡ public.
    let (temp, _) = init_and_capture("ordinary");
    let state = capture_state(temp.path(), "ordinary capture");

    let show = show_json(temp.path(), &state);
    assert_eq!(show["tier"], "public");
    assert_eq!(show["effective_public"], true);
}

#[test]
fn visibility_set_then_show_reports_tier() {
    let (temp, _) = init_and_capture("ordinary");
    let state = capture_state(temp.path(), "ordinary capture");

    let raw = heddle(
        &[
            "--output", "json", "visibility", "set", &state, "--tier", "internal",
        ],
        Some(temp.path()),
    )
    .expect("visibility set");
    let set: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(set["output_kind"], "visibility_set");
    assert_eq!(set["tier"], "internal");

    let show = show_json(temp.path(), &state);
    assert_eq!(show["tier"], "internal");
    assert_eq!(show["effective_public"], false);
}

#[test]
fn visibility_promote_supersedes_to_less_restrictive() {
    let (temp, _) = init_and_capture("ordinary");
    let state = capture_state(temp.path(), "ordinary capture");

    heddle(
        &[
            "visibility", "set", &state, "--tier", "restricted", "--label", "embargo",
        ],
        Some(temp.path()),
    )
    .expect("visibility set restricted");

    let raw = heddle(
        &[
            "--output", "json", "visibility", "promote", &state, "--tier", "internal",
        ],
        Some(temp.path()),
    )
    .expect("visibility promote");
    let promote: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(promote["output_kind"], "visibility_promote");
    assert_eq!(promote["tier"], "internal");

    let show = show_json(temp.path(), &state);
    assert_eq!(show["tier"], "internal", "promotion should be the effective tier");
}

#[test]
fn undo_visibility_set_restores_prior_sidecar() {
    // PR #529 P1: undoing a standalone `visibility set` must revert the
    // sidecar, not just mark the oplog entry undone. A state that was
    // public-by-absence before the set must read public again after undo, then
    // re-tiered after redo.
    let (temp, _) = init_and_capture("ordinary");
    let state = capture_state(temp.path(), "ordinary capture");
    assert_eq!(
        show_json(temp.path(), &state)["effective_public"],
        true,
        "state starts public-by-absence"
    );

    heddle(
        &["visibility", "set", &state, "--tier", "internal"],
        Some(temp.path()),
    )
    .expect("visibility set");
    assert_eq!(show_json(temp.path(), &state)["tier"], "internal");

    heddle(&["undo"], Some(temp.path())).expect("undo visibility set");
    let after_undo = show_json(temp.path(), &state);
    assert_eq!(
        after_undo["effective_public"], true,
        "undo must restore public-by-absence (sidecar removed): {after_undo}"
    );
    assert_eq!(after_undo["tier"], "public");

    heddle(&["undo", "--redo"], Some(temp.path())).expect("redo visibility set");
    let after_redo = show_json(temp.path(), &state);
    assert_eq!(
        after_redo["tier"], "internal",
        "redo must reapply the set tier: {after_redo}"
    );
    assert_eq!(after_redo["effective_public"], false);
}

#[test]
fn undo_visibility_set_restores_previous_nonpublic() {
    // When a state already carries a non-public tier, undoing a later `set`
    // must restore the PREVIOUS non-public tier, not drop to public-by-absence.
    let (temp, _) = init_and_capture("ordinary");
    let state = capture_state(temp.path(), "ordinary capture");

    heddle(
        &[
            "visibility",
            "set",
            &state,
            "--tier",
            "team-scoped",
            "--label",
            "infra",
        ],
        Some(temp.path()),
    )
    .expect("set team-scoped");
    assert_eq!(show_json(temp.path(), &state)["tier"], "team_scoped");

    heddle(
        &["visibility", "set", &state, "--tier", "internal"],
        Some(temp.path()),
    )
    .expect("set internal");
    assert_eq!(show_json(temp.path(), &state)["tier"], "internal");

    heddle(&["undo"], Some(temp.path())).expect("undo second set");
    let after = show_json(temp.path(), &state);
    assert_eq!(
        after["tier"], "team_scoped",
        "undo must restore the previous non-public tier, not absence: {after}"
    );
    assert_eq!(after["effective_public"], false);
    assert_eq!(after["label"], "infra");
}

#[test]
fn undo_visibility_promote_reverts_tier() {
    // A promote appends a superseding record; undo must drop back to the
    // pre-promote effective tier.
    let (temp, _) = init_and_capture("ordinary");
    let state = capture_state(temp.path(), "ordinary capture");

    heddle(
        &[
            "visibility",
            "set",
            &state,
            "--tier",
            "restricted",
            "--label",
            "embargo",
        ],
        Some(temp.path()),
    )
    .expect("set restricted");
    assert_eq!(show_json(temp.path(), &state)["tier"], "restricted");

    heddle(
        &["visibility", "promote", &state, "--tier", "internal"],
        Some(temp.path()),
    )
    .expect("promote to internal");
    assert_eq!(show_json(temp.path(), &state)["tier"], "internal");

    heddle(&["undo"], Some(temp.path())).expect("undo promote");
    let after_undo = show_json(temp.path(), &state);
    assert_eq!(
        after_undo["tier"], "restricted",
        "undo of promote must revert to the pre-promote tier: {after_undo}"
    );
    assert_eq!(after_undo["label"], "embargo");

    heddle(&["undo", "--redo"], Some(temp.path())).expect("redo promote");
    assert_eq!(
        show_json(temp.path(), &state)["tier"],
        "internal",
        "redo of promote must re-apply the promoted tier"
    );
}

#[test]
fn visibility_list_enumerates_tiered_states() {
    let (temp, _) = init_and_capture("ordinary");
    let state = capture_state(temp.path(), "ordinary capture");
    heddle(
        &["visibility", "set", &state, "--tier", "internal"],
        Some(temp.path()),
    )
    .expect("visibility set");

    let raw = heddle(
        &["--output", "json", "visibility", "list"],
        Some(temp.path()),
    )
    .expect("visibility list");
    let listing: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(listing["output_kind"], "visibility_list");
    assert_eq!(listing["count"], 1);
    assert_eq!(listing["states"][0]["tier"], "internal");
}
