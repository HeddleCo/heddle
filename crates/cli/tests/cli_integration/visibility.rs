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

    heddle(&["redo"], Some(temp.path())).expect("redo visibility set");
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

    heddle(&["redo"], Some(temp.path())).expect("redo promote");
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
