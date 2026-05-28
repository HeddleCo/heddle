# heddle#290 — git-bridge command surface (overlay mode)

**Status:** spike (decision doc). Impl tracked in follow-up issue(s) — split
shape proposed in §6, to confirm with the orchestrator/user before filing.
**Scope:** CLI surface / UX of `heddle bridge git <verb>` in **git-overlay
mode** only. Gated behind the `git-overlay` cargo feature
(`crates/cli/Cargo.toml:154`, `:161`). Does not touch native builds.
**Companion:** #289 fixes *what* the forward verbs report (the "exported 0
states" count). This doc decides *where/whether* the verbs are foregrounded.
Independent — neither blocks the other.

## §1 — Current state (verified against code 2026-05-28)

### The bridge verb set is flat

`heddle bridge git --help` lists every verb as a peer, in `GitCommands`
enum declaration order (`crates/cli/src/cli/cli_args/commands_bridge.rs:54-192`):

```
Commands:
  status     Show the current state of the Git overlay bridge
  init       Initialize Git mirror
  export     Export Heddle states to Git
  import     Import Git commits to Heddle
  sync       Bidirectional sync with Git (export + import)
  reconcile  Preview a recovery path when a Git branch and Heddle thread diverge
  push       Push to Git remote
  pull       Pull from Git remote
  ingest     Deep import: walk every git ref ... (ingest feature)
  reason     Mine local AI-coding-agent sessions ... (ingest feature)
```

There is no grouping by direction, no signal that any verb is rarely needed
in overlay mode, and no pointer to the native top-level command that does the
same job. `ingest`/`reason` are gated behind the `ingest` feature, which
`git-overlay` turns on (`crates/cli/Cargo.toml:161`).

### Forward (heddle → git) is already automatic in overlay

A `heddle commit`/`ship` in overlay mode writes the git commit **inline**:
`create_git_checkpoint` (`crates/cli/src/cli/commands/checkpoint.rs:105`)
drives the bridge write-through, returning
`WriteThroughOutcome::Wrote(git_commit)` (`checkpoint.rs:175-178`). It is
called inline by both `heddle commit`
(`crates/cli/src/cli/commands/git_compat.rs:156`, `:282`, `:364` —
`heddle commit` routes to `cmd_commit_compat` per
`crates/cli/src/main.rs:394`) and `heddle ship`
(`crates/cli/src/cli/commands/workflow.rs:375`, `:536`). The user never runs a bridge verb to get their own
Heddle work onto the git side — it is there the moment they commit.

Consequence: in overlay, `bridge git export` has (almost) nothing to mirror,
because the states are already on the git side. That is the "exported 0
states" surprise #289 addresses.

### Reverse (git → heddle) is never automatic

A commit that lands on the git branch *outside* Heddle (teammate push that a
plain `git pull` already fast-forwarded onto the local branch, CI, PR merge,
a bare `git commit` in the same checkout) leaves Heddle blocked until an
explicit import. The divergence health check
(`crates/cli/src/cli/commands/git_overlay_health.rs:2694-2703`) emits:

```text
Git branch 'main' advanced outside Heddle; import the new Git tip to restore the mapping
```

and its `recovery_commands` is `canonical_adopt_ref_command(&tip.branch)`
(`git_overlay_health.rs:2701`), which expands to **`heddle adopt --ref
<branch>`** (`git_overlay_health.rs:3245-3247`) — a *top-level* command
(`crates/cli/src/cli/cli_args/commands_args.rs:41-56`), not a `bridge git`
verb.

### `adopt` and `bridge git import` share one engine

`heddle adopt` is not a separate import path. It calls the same
`import_all` / `import_selected_refs` functions that `bridge git import`
calls:

- `adopt`: `crates/cli/src/cli/commands/adopt.rs:82-86`.
- `bridge git import`: `crates/cli/src/cli/commands/bridge.rs:659-662`.

`adopt` wraps them with an init-if-needed bootstrap: it runs
`Repository::bootstrap_git_overlay` only when `.heddle` is absent, otherwise
`Repository::open` (`adopt.rs:57-65`). On an already-adopted repo it is a
pure re-import and renders "Heddle already adopted this Git repo; history is
in sync" (`adopt.rs:256-261`). So **`adopt` is already designed to be re-run
as the routine catch-up**, not just a one-time conversion — the name connotes
"one-time" but the mechanism does not.

### The machine surface already foregrounds the reverse path

This is the key finding. The command-contract catalog
(`crates/cli/src/cli/commands/command_catalog.rs`) already encodes a
de-emphasis tier for every bridge verb, and a redirect to the native canonical
command for most of them. The exceptions are `bridge git ingest` and `bridge
git reason`, which are catalogued `surface(...)`-only and carry no
`canonical_command` (the `—` rows below). Verified live via `heddle commands
--command "bridge git" --output json`:

| bridge verb | tier | canonical → | kind |
|---|---|---|---|
| `bridge git status` | advanced | `status` | direct_command |
| `bridge git init` | advanced | `init` | direct_command |
| `bridge git export` | advanced | `push` | direct_command |
| `bridge git import` | advanced | `adopt` | workflow |
| `bridge git sync` | advanced | `adopt` | workflow |
| `bridge git reconcile` | advanced | `adopt` | workflow |
| `bridge git push` | advanced | `push` | direct_command |
| `bridge git pull` | advanced | `pull` | direct_command |
| `bridge git ingest` | advanced | — | — |
| `bridge git reason` | advanced | — | — |

Source: the redirecting bridge verbs are built with `git_adapter_action` /
`git_adapter_alias`, which stamp `help_visibility: "git_adapter"` and a
`canonical_command` (`command_catalog.rs:978-1004`). The import/sync/reconcile
entries explicitly carry `canonical_command = "adopt"` with note "Use adopt
for the guided Git-to-Heddle conversion workflow"
(`command_catalog.rs:1238-1240`, `:1261-1263`, `:1284-1286`); export is
aliased to `push` (`command_catalog.rs:1214-1224`). The two exceptions —
`bridge git ingest` and `bridge git reason` — are registered with `surface(…,
"git_adapter")` only (`command_catalog.rs:1323-1335`), so they get the
`git_adapter` visibility but no `canonical_command` (it is `None`).
`help_visibility: "git_adapter"` falls through to tier `"advanced"`
(`help_visibility_to_tier`, `command_catalog.rs:3664-3670`) — so none of these
appear in the `everyday` tier of `heddle commands`.

The native canonicals all exist as top-level commands: `heddle adopt`,
`heddle pull` ("Pull from a remote repository"), `heddle push` ("Push to a
remote repository") — verified via `--help`.

## §2 — Problem (restated precisely)

The agent/JSON surface is **already correct**: every bridge verb is tiered to
`advanced`, and each verb that has a `canonical_command` redirects to its
native canonical. The two exceptions — `bridge git ingest` and `bridge git
reason` — are surface-only (`canonical_command: None`,
`command_catalog.rs:1323-1335`): still `advanced`-tiered, but with no canonical
to redirect to. The defect is confined to two **human-facing** surfaces that
ignore the catalog:

1. **`heddle bridge git --help`** is generated by clap from the enum
   doc-comments (`commands_bridge.rs:54-192`). It is flat, declaration-ordered,
   and says nothing about direction, automatic-in-overlay, or the canonical
   `adopt`/`pull`/`push` equivalents. A human reading only `--help` reaches for
   `export`/`push` when, after a teammate's commit lands locally, the thing
   they actually need is `adopt --ref` (or `import`/`sync`).

2. **Forward-verb runtime output** doesn't explain the no-op. When `export`
   has nothing to mirror in overlay, it prints "exported 0 states" (the #289
   surprise) instead of "git is already current with Heddle; to pull in
   external commits use `heddle adopt --ref <branch>`."

Net: the data model already knows reverse is load-bearing and forward is a
rarely-needed alias, but the two places a *human* looks — the help text and
the command's own output — don't say so.

## §3 — Options weighed (the four from the issue)

**Option 1 — Regroup `heddle bridge git --help`.** Lead with the overlay
model and the reverse verbs; push the forward verbs down under a "rarely
needed in overlay — git is kept current automatically" framing.
*Feasibility note:* clap 4 derive lists all subcommands under a single
`Commands:` block; it has no native per-subcommand help heading. The realistic
mechanism is (a) rewrite the group `about`/`after_help` on the `Git` variant
to state the overlay model and name the canonical reverse path, (b) reorder
the enum so reverse/diagnostic verbs come first, and (c) tighten each verb's
doc-comment to name its native canonical. This is the surface that most
directly fixes "users reach for the wrong verb," and for the verbs that have a
`canonical_command` it can be driven by the metadata the catalog already holds;
`ingest`/`reason` carry no canonical (`canonical_command: None`) and must be
handled explicitly rather than assumed to redirect.

**Option 2 — No-op self-explanation.** When a forward verb has nothing to do
in overlay, say so and point at the reverse path. Dovetails with #289's
"already in sync" wording. Fixes the surface a user sees *after* they've
already run the wrong verb. Additive to the text render path; keep the JSON
contract stable (extend, don't change).

**Option 3 — Promote the common reverse path / reconcile `adopt` vs `bridge
git import` overlap.** **Largely already shipped.** The catalog makes `adopt`
the canonical for import/sync/reconcile, the divergence recovery hint already
emits `heddle adopt --ref`, and `adopt` is built to be re-run as catch-up
(§1). The only *open* question here is naming connotation, not mechanism —
see §5. There is no new redirect infrastructure to build.

**Option 4 — Scope forward `export` to its non-overlapping jobs.** `export`
must stay: it is the single redaction chokepoint between Heddle-side
redactions and any downstream git remote
(`crates/cli/src/bridge/git_export.rs:115-124`), and the standalone
mirror/batch-seed path (writes a complete bare repo at `--destination`,
`commands_bridge.rs:76-81`). This is a *documentation* scoping, not a code
change, and it naturally folds into the Option 1 about/doc-comment rewrite:
state that `export` is for seeding an external mirror and for the
redaction-aware path, not for routine "get my work into git" (which is
automatic).

## §4 — Decision

**Implement Option 1 + Option 2, both driven by the metadata the catalog
already holds. Fold Option 4 into Option 1. Record Option 3 as already
shipped — do not re-build it.**

Rationale:

- The two unshipped gaps are exactly the two human surfaces in §2. Fixing
  them makes `--help` and runtime output consistent with what the machine
  surface already tells agents.
- Driving the `--help` rewrite from the existing `canonical_command` /
  `help_visibility` catalog data (rather than hand-curating a new grouping)
  keeps the human and machine surfaces from drifting apart — for the
  canonicalized verbs the catalog stays the single source of truth for "which
  native command supersedes this bridge verb." (`ingest`/`reason` have no
  superseding canonical — `canonical_command: None` — and are documented as
  surface-only.)
- Option 3 needs no engineering: `adopt` is canonical, the divergence hint
  uses it, and it re-runs as catch-up. The residual is the naming question in
  §5, which is a user call, not an impl task.

## §5 — One open question for the user (naming, not mechanism)

`adopt` is the canonical reverse-catch-up verb and is built to be re-run, but
the name connotes a one-time conversion ("adopt this repo"), which reads oddly
for the routine "a teammate's commit landed on my branch; resync Heddle" case.
The reverse direction actually has two sub-cases that today route to two
different verbs:

- **Remote commits not yet local** → `heddle pull` (network fetch + apply).
- **Local git branch advanced outside Heddle** (plain `git commit`/`git
  merge`/an already-landed `git pull` in the same checkout) → `heddle adopt
  --ref <branch>` (re-ingest the local tip; this is what the divergence hint
  emits).

These are genuinely distinct operations, so collapsing them to one verb would
lose meaning. **Recommendation: keep `adopt` as the canonical and fix only the
wording** — have the overlay `--help` and the no-op/divergence messages spell
out "advanced outside Heddle → `adopt --ref`; remote you haven't fetched →
`pull`." This is a wording decision the impl can carry without new commands.
If the user would rather introduce a catch-up alias (e.g. a re-ingest verb
named for routine use), that is a larger surface change and should be its own
issue. **This is the single point in this spike that wants a human call.**

## §6 — Proposed impl split (confirm before filing)

Two small follow-up issues, both `git-overlay`-gated, neither touching the
JSON/machine contract destructively:

- **Sub-issue A — overlay-aware `bridge git --help` (Options 1 + 4).**
  Rewrite the `Git` group `about`/`after_help` and per-verb doc-comments in
  `crates/cli/src/cli/cli_args/commands_bridge.rs` to: (1) state the overlay
  model ("forward is automatic on commit/ship"), (2) lead with the reverse /
  diagnostic verbs and name their native canonical (`adopt --ref`, `pull`),
  (3) re-scope `export`'s text to "seed an external mirror + redaction-aware
  export," and (4) reorder the enum so reverse/diagnostic verbs precede the
  rarely-needed forward ones. For the verbs that have a `canonical_command`,
  pull the canonical name from the catalog so the two surfaces can't drift;
  `ingest` and `reason` are surface-only (`canonical_command: None`) and must
  be handled explicitly rather than assumed to redirect. Size **S**. No JSON
  change.

- **Sub-issue B — forward-verb no-op self-explanation (Option 2).** When
  `export` (and the `export` half of `sync`) has nothing new to mirror in
  overlay, emit "git is already current with Heddle; to pull in external
  commits use `heddle adopt --ref <branch>`" instead of the bare "0 states."
  Touches the text render paths in `crates/cli/src/cli/commands/bridge.rs`
  (`GitCommands::Export` at `:593`, `GitCommands::Sync` at `:716`); extend the
  JSON additively if a flag is needed. Dovetails with #289 — coordinate the
  "already in sync" wording so they don't conflict. Size **S–M**.

Option 3 spawns **no** issue (already shipped). The §5 naming question is a
user decision, not a sub-issue, unless the user opts for a new catch-up alias.

## Out of scope

- Removing any bridge command (`export` is load-bearing — §3 Option 4).
- The `export`/`sync` count-correctness fix — that is #289.
- Changing the JSON / machine contract for any bridge verb (extend only).
- Native (non-`git-overlay`) builds.
