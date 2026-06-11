// SPDX-License-Identifier: Apache-2.0
//! Snapshot-style pins on curated `--help` prose so that edits to the
//! clap doc comments don't silently regress the behavior stanzas users
//! rely on.

use super::*;

/// Equivalence guard for the in-process help renderer (HeddleCo/heddle#381).
///
/// The help assertions in this suite call `heddle_help(..)` (in-process)
/// instead of spawning the binary, on the premise that the two produce
/// byte-identical stdout. This test pins that premise: for a representative
/// spread of help-shaped argv — per-verb `--help`, nested-path `--help`, the
/// `capture --help-agent` reveal, the curated bare/advanced surfaces, and a
/// topic page — the in-process render must equal the spawned binary's stdout
/// exactly. If a future change makes a `print_*` helper diverge from its
/// `render_*` core, this fails before the substring assertions silently drift.
#[test]
fn help_render_matches_spawned_binary() {
    for args in [
        vec!["clone", "--help"],
        vec!["capture", "--help"],
        vec!["capture", "--help-agent"],
        vec!["push", "--help"],
        vec!["log", "--help"],
        vec!["bridge", "git", "import", "--help"],
        // Alias resolution: `import` is an alias that resolves to `adopt`.
        vec!["import", "--help"],
        // Curated topic page + `heddle help <verb>` clap fall-through.
        vec!["help"],
        vec!["help", "advanced"],
        vec!["help", "threads"],
        vec!["help", "git-overlay"],
        vec!["help", "status"],
        // Global-flag-prefixed capture reveal forms.
        vec!["-vC", ".", "capture", "--help-agent"],
        vec!["--output", "text", "capture", "--help-agent"],
    ] {
        let spawned = heddle(&args, None)
            .unwrap_or_else(|err| panic!("spawned `heddle {}`: {err}", args.join(" ")));
        let in_process = heddle_help(&args);
        assert_eq!(
            in_process,
            spawned,
            "in-process render of `heddle {}` must match the spawned binary's stdout byte-for-byte",
            args.join(" ")
        );
    }
}

/// `heddle clone --help` must keep its Behavior stanza: the prose that
/// answers Priya's "wait, where am I?" — default-thread resolution and
/// what `--depth` actually materializes (heddle#257).
#[test]
fn clone_help_pins_behavior_stanza() {
    let help = heddle_help(&["clone", "--help"]);

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

/// heddle#278 r4 (cid 3327325850). `capture --help-agent` is a first-class
/// clap flag, so clap parses the whole command line and the dispatch arm
/// renders the reveal help. End-to-end through the real binary: the hidden
/// agent-automation flags appear, and every global spelling clap accepts —
/// including the clustered short `-vC <path>` that the old hand-rolled
/// pre-parse scan kept missing — reaches the reveal help.
#[test]
fn capture_help_agent_reveals_hidden_flags_through_clap() {
    let expect_revealed = |help: &str, ctx: &str| {
        for flag in [
            "--agent-provider",
            "--agent-model",
            "--agent-session",
            "--agent-segment",
            "--policy",
            "--no-policy",
            "--no-agent",
            "--split",
            "--into",
        ] {
            assert!(
                help.contains(flag),
                "`{ctx}` should reveal `{flag}` inline: {help}"
            );
        }
    };

    let plain = heddle_help(&["capture", "--help-agent"]);
    expect_revealed(&plain, "capture --help-agent");

    // Clustered short globals before the verb: `-v` then valued `-C <path>`.
    // clap parses the path natively; the verb is still `capture`.
    let clustered = heddle_help(&["-vC", ".", "capture", "--help-agent"]);
    expect_revealed(&clustered, "-vC <path> capture --help-agent");

    // Long valued global before the verb.
    let long_global = heddle_help(&["--output", "text", "capture", "--help-agent"]);
    expect_revealed(&long_global, "--output text capture --help-agent");
}

/// heddle#278 r6 (cid 3327633095). `--help-agent` is `hide = true`: the
/// reveal still works (asserted above), but everyday `capture --help` stays
/// terse — the flag itself must not appear, only the after-help pointer to
/// the reveal surface. Progressive disclosure: discover via the pointer, not
/// by seeing the flag in the human help.
#[test]
fn capture_help_keeps_help_agent_hidden_but_keeps_the_pointer() {
    let help = heddle_help(&["capture", "--help"]);
    // The discovery pointer in after-help stays so agents can still find it.
    assert!(
        help.contains("heddle capture --help-agent"),
        "`capture --help` should keep the after-help pointer to `--help-agent`: {help}"
    );
    // ...but the flag must not appear in the options listing. The only
    // mention allowed is the single after-help pointer line.
    assert_eq!(
        help.matches("--help-agent").count(),
        1,
        "`capture --help` must not list the hidden `--help-agent` flag (only the after-help pointer): {help}"
    );
}
