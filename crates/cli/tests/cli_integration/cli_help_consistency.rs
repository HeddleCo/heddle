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
    // The transport distinction: native Heddle remotes default to `main`,
    // not the Git-HEAD chain — the inaccuracy this stanza must not regress.
    assert!(
        help.contains("Heddle remote") && help.contains("lands on `main`"),
        "clone help should distinguish the Heddle-remote default from the Git-overlay chain: {help}"
    );
    // Depth semantics: 0 means full history, N is shallow.
    assert!(
        help.contains("--depth 0") && help.contains("full history"),
        "clone help should explain that --depth 0 is full history: {help}"
    );
    assert!(
        help.contains("shallow") && help.contains("shallow edge"),
        "clone help should explain shallow depth semantics: {help}"
    );
    // Depth is a Heddle-remote-only flag; Git-overlay clones reject it.
    assert!(
        help.contains("Git-overlay clones reject --depth"),
        "clone help should note that Git-overlay clones reject --depth: {help}"
    );
    // Cross-reference to the thread model topic.
    assert!(
        help.contains("heddle help threads"),
        "clone help should point at the threads topic: {help}"
    );
}
