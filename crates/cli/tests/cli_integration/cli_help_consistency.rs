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
        vec!["import", "git", "--help"],
        // Alias resolution: `import` is an alias that resolves to `adopt`.
        vec!["import", "--help"],
        // Curated topic page + `heddle help <verb>` clap fall-through.
        vec!["help"],
        vec!["help", "advanced"],
        vec!["help", "git-concepts"],
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

/// The clone behavior prose must keep answering Priya's "wait, where am
/// I?" — default-thread resolution and what `--depth` actually
/// materializes (heddle#257). The full exposition lives on the
/// `heddle help clone` topic page since the heddle#652 help-budget pass;
/// `clone --help` keeps a one-screen Behavior summary plus the breadcrumb
/// to the topic. Both surfaces are pinned: the summary so the first
/// screen still orients, the topic so the moved prose keeps every
/// accuracy fix the old in-help stanza accumulated.
#[test]
fn clone_help_pins_behavior_stanza() {
    let summary = heddle_help(&["clone", "--help"]);

    assert!(
        summary.contains("Behavior:"),
        "clone help should include a Behavior stanza: {summary}"
    );
    // The one-screen summary still answers the headline questions —
    // where an unhinted clone lands per remote kind — and points at the
    // topic page for the rest.
    assert!(
        summary.contains("selected default branch"),
        "clone help summary should say where clones land: {summary}"
    );
    assert!(
        summary.contains("--thread"),
        "clone help summary should name the escape hatch: {summary}"
    );
    assert!(
        summary.contains("heddle help clone"),
        "clone help should breadcrumb to the full clone topic: {summary}"
    );

    let help = heddle_help(&["help", "clone"]);
    assert!(
        help.contains("Git source is streamed by Sley directly")
            && help.contains("No Git executable or `.heddle/git` mirror is used"),
        "clone topic should explain the Git-overlay transport and storage path: {help}"
    );
    assert!(
        help.contains("Native clones target `main` directly")
            && help.contains("if the remote has no `main` thread")
            && help.contains("--thread <name>"),
        "clone topic should explain native default-thread selection: {help}"
    );
    // Depth semantics: 0 means full history, N keeps the tip plus N ancestry levels.
    assert!(
        help.contains("--depth 0") && help.contains("full history"),
        "clone topic should explain that --depth 0 is full history: {help}"
    );
    assert!(
        help.contains("depth boundary") && help.contains("re-clone at a greater --depth"),
        "clone topic should explain that history past the depth boundary is absent and recovered by re-cloning at a greater depth: {help}"
    );
    assert!(
        help.contains("Git Overlay clones ingest full history")
            && help.contains("reject partial-history")
            && help.contains("options"),
        "clone topic should explain that shallow history is native-only: {help}"
    );
    // Depth-1 materializes the tip plus its immediate parents, not just the tip.
    assert!(
        help.contains("tip plus its immediate parents"),
        "clone topic should explain that --depth 1 keeps the tip plus immediate parents: {help}"
    );
    // Cross-reference to the thread model topic.
    assert!(
        help.contains("heddle help threads"),
        "clone topic should point at the threads topic: {help}"
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

/// heddle#646. The hidden `--lazy`/`--filter` clone flags need a discovery
/// affordance without bloating first-run help: human `clone --help` carries a
/// one-line breadcrumb to the topic, while the machine catalog names the
/// hidden flags.
#[test]
fn clone_help_carries_hidden_flag_breadcrumb() {
    let help = heddle_help(&["clone", "--help"]);
    assert!(
        help.contains("Advanced/planned flags: see `heddle help clone`."),
        "clone help should point to the advanced/planned flag topic: {help}"
    );
    assert!(
        !help.contains("--lazy") && !help.contains("--filter"),
        "clone help should keep hidden flags out of the human options/body: {help}"
    );
}

/// heddle#646 (same class, pull surface). `pull --lazy` is hidden too; the
/// breadcrumb keeps it discoverable.
#[test]
fn pull_help_carries_hidden_flag_breadcrumb() {
    let help = heddle_help(&["pull", "--help"]);
    assert!(
        help.contains("--lazy") && help.contains("hydrates it explicitly later"),
        "pull help should name the hidden --lazy flag and its behavior: {help}"
    );
}

/// heddle#646 (same class, start surface). The hidden automation/power
/// flags on `start` are named in an advanced-flags stanza so agents and
/// power users can discover them without reading the source.
#[test]
fn start_help_carries_hidden_flag_breadcrumb() {
    let help = heddle_help(&["start", "--help"]);
    for flag in [
        "--agent-provider",
        "--agent-model",
        "--parent-thread",
        "--print-cd-path",
        "--daemon",
        "--no-daemon",
        "--shared-target",
    ] {
        assert!(
            help.contains(flag),
            "start help should name the hidden `{flag}` flag in its advanced stanza: {help}"
        );
    }
}

/// Diff help must keep the patch format and extended-header boundary explicit.
#[test]
fn diff_help_explains_git_compatible_patch_headers() {
    let help = heddle_help(&["diff", "--help"]);
    assert!(
        help.contains("Git-compatible unified diff")
            && help.contains("extended headers for type and mode changes"),
        "diff help should explain its patch format and extended headers: {help}"
    );
}

/// Commit help must explain the capture-to-Git authority boundary.
#[test]
fn commit_help_explains_capture_boundary() {
    let help = heddle_help(&["commit", "--help"]);
    assert!(
        help.contains("Defaults to the current capture intent")
            && help.contains("Commits the complete captured tree")
            && help.contains("Git pre-commit and commit-msg hooks are not run"),
        "commit help should explain how a capture becomes Git history: {help}"
    );
}

/// The Git concepts page must explain authority boundaries and advertise only
/// the current thin Git surface.
#[test]
fn git_concepts_topic_explains_authority_and_current_surface() {
    let help = heddle_help(&["help", "git-concepts"]);

    assert!(
        help.contains("Git and Heddle own different layers."),
        "git-concepts topic should lead with the authority boundary: {help}"
    );
    for ownership in [
        "`.git` owns commits, refs",
        "the index, and worktree state",
        "coordination and durable metadata in `.heddle`",
        "captures, provenance, threads, readiness, review, and safe landing",
    ] {
        assert!(
            help.contains(ownership),
            "git-concepts topic missing ownership statement `{ownership}`: {help}"
        );
    }
    assert!(
        help.contains("`clone`,")
            && help.contains("`commit`,")
            && help.contains("`pull`,")
            && help.contains("`push`, and `remote`")
            && help.contains("embedded Sley engine directly"),
        "git-concepts topic should name the current thin Git surface and engine: {help}"
    );
    for mapping in [
        "| Intent | Git Overlay | Native Heddle |",
        "`heddle capture`, then `heddle commit`",
        "| Record a granular Heddle savepoint | `heddle capture` | `heddle capture` |",
        "| Check integration readiness | `heddle ready` | `heddle ready` |",
        "| Integrate a managed thread | `heddle land` | `heddle land` |",
        "| Synchronize source | `heddle pull` / `heddle push`",
        "| Configure remotes | `heddle remote` | `heddle remote` |",
    ] {
        assert!(
            help.contains(mapping),
            "git-concepts topic missing current mapping `{mapping}`: {help}"
        );
    }
    assert!(
        help.contains("Use `heddle init` to add that sidecar")
            && help.contains("`heddle adopt`")
            && help.contains("one atomic transition")
            && help.contains("makes Heddle the repository authority"),
        "git-concepts topic should distinguish sidecar initialization from adoption: {help}"
    );
    assert!(
        help.contains("optional Git-compatible client")
            && help.contains("it is not a Heddle dependency")
            && help.contains("`import git`")
            && help.contains("`export")
            && help.contains("git`, and `sync git`")
            && help.contains("translate data between authorities"),
        "git-concepts topic should explain unsupported Git operations and authority translation: {help}"
    );
    for removed in ["heddle checkpoint", "heddle fetch", "heddle switch"] {
        assert!(
            !help.contains(removed),
            "git-concepts topic must not advertise removed command `{removed}`: {help}"
        );
    }

    let top = heddle_help(&["help"]);
    assert!(
        top.contains("heddle help git-concepts"),
        "main help should link the git concept map from Start here: {top}"
    );
}
