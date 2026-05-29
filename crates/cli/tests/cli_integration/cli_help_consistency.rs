// SPDX-License-Identifier: Apache-2.0
//! Snapshot-style pins on curated `--help` prose so that edits to the
//! clap doc comments don't silently regress the behavior stanzas users
//! rely on.

use super::*;

/// `heddle clone --help` must keep its Behavior stanza: the prose that
/// answers Priya's "wait, where am I?" — default-thread resolution and
/// what `--depth` actually materializes (heddle#257).
#[test]
fn clone_help_pins_behavior_stanza() {
    let help = heddle(&["clone", "--help"], None).expect("clone help should render");

    assert!(
        help.contains("Behavior:"),
        "clone help should include a Behavior stanza: {help}"
    );
    // Default-thread resolution for Git-overlay clones: lands on the
    // remote's advertised default branch (its Git HEAD).
    assert!(
        help.contains("no --thread") && help.contains("default branch"),
        "clone help should explain where an unhinted clone lands: {help}"
    );
    // The Git-overlay fallback chain (main, then first imported thread).
    assert!(
        help.contains("`main`") && help.contains("alphabetically first"),
        "clone help should name the default-thread fallback chain: {help}"
    );
    // The transport distinction: native-local and hosted Heddle clones target
    // `main` directly (no Git-HEAD fallback chain) — the inaccuracy this stanza
    // must not regress.
    assert!(
        help.contains("Native-local and hosted Heddle clones")
            && help.contains("target `main` directly"),
        "clone help should distinguish the Heddle-remote default from the Git-overlay chain: {help}"
    );
    // The failure mode: an unhinted native/hosted clone fails when the remote
    // has no `main` thread, and the user must pass `--thread <name>`.
    assert!(
        help.contains("the clone fails") && help.contains("--thread <name>"),
        "clone help should document that an unhinted native/hosted clone fails when the remote has no `main` thread: {help}"
    );
    // Depth semantics: 0 means full history, N keeps the tip plus N ancestry levels.
    assert!(
        help.contains("--depth 0") && help.contains("full history"),
        "clone help should explain that --depth 0 is full history: {help}"
    );
    assert!(
        help.contains("depth boundary") && help.contains("re-clone at a greater --depth"),
        "clone help should explain that history past the depth boundary is absent and recovered by re-cloning at a greater depth: {help}"
    );
    // Depth on Git-overlay clones: nonzero is rejected, --depth 0 is accepted
    // (= the full-clone default, since cmd_clone normalizes 0 to None before
    // the rejection check).
    assert!(
        help.contains("Git-overlay clones reject a nonzero --depth")
            && help.contains("--depth 0 is accepted"),
        "clone help should distinguish nonzero --depth (rejected) from --depth 0 (accepted) for Git-overlay clones: {help}"
    );
    // Depth-1 materializes the tip plus its immediate parents, not just the tip.
    assert!(
        help.contains("tip plus its immediate parents"),
        "clone help should explain that --depth 1 keeps the tip plus immediate parents: {help}"
    );
    // Cross-reference to the thread model topic.
    assert!(
        help.contains("heddle help threads"),
        "clone help should point at the threads topic: {help}"
    );
}
