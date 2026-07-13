# Heddle CLI — Operating Principles

This is how Heddle thinks about its CLI. The surface is small on purpose, the
outputs are honest on purpose, and the verbs compose because the primitives
beneath them are the right shape. Five principles run through every command:
verification, disposability, composability, restraint, honesty. Read this before you
add a verb, change a flag, or argue for a new output field.

## 1. Verification

Outputs say what they mean. Field names are stable across verbs, optional
fields serialize as explicit `null` rather than disappearing, and empty
collections come back as `[]` — never omitted. An agent that reads
`heddle ready --output json` and then `heddle status --output json` finds `change_id`,
`current_state`, and `confidence` carrying the same meaning in both places.

The full contract lives in [docs/json-schemas.md](json-schemas.md): stable
field names, explicit `null`, no leakage of unrelated context, empty
collections serialize, pretty-printing only on `heddle show`. A tooling
author can write a parser against the doc and expect the binary to match.

Verification extends down into errors. The filesystem layer in
[`crates/objects/src/fs_atomic.rs`](../crates/objects/src/fs_atomic.rs)
ships named predicates — `is_out_of_space`, `is_directory_not_empty`,
`is_permission_denied`, `is_read_only_filesystem`, `is_cross_device` — so
a "capture failed" message names the actual kernel signal it saw, on
every platform, rather than dumping a generic `io::Error`. Failure
quality is part of the contract, not a polish step.

## 2. Disposability

Speculation has no cost. `heddle try -- <cmd>` spins up an ephemeral
thread with an isolated checkout, runs the command, and either captures
the result on success or drops the thread on failure — the parent
worktree is never touched ([`crates/cli/src/cli/commands/try_cmd.rs`](../crates/cli/src/cli/commands/try_cmd.rs)).
For parallel speculation, create isolated child threads with
`heddle start <name> --parent-thread <parent> --task <task>` and run each
experiment in its own checkout.

Disposability is what makes those verbs possible. Threads are cheap, the
oplog is reliable, and rolling back is `heddle undo` — not a manual file
revert. When trying ten things costs about the same as trying one, the
agent's strategy changes. We design for that.

## 3. Composability

The same primitives appear in multiple verbs because the right ones were
chosen first. A thread is a thread whether it came from `heddle start`,
`heddle try`, or a child `heddle start --parent-thread` workflow. A capture is a capture
whether you triggered it explicitly or the verify hook fired it after a
green test run.

`heddle retro --since <marker>` is the clearest example
([`crates/cli/src/cli/commands/retro.rs`](../crates/cli/src/cli/commands/retro.rs)).
Before retro, reconstructing "what happened this turn" meant
cross-referencing `heddle log`, `heddle agent list`,
`heddle context history`, and `heddle thread marker list` separately, then
aligning timestamps by hand. Retro folds those four reads into one trip on
a single time window. It isn't new data — it's an idiom that emerged often
enough to deserve a name.

## 4. Restraint

Less to remember. The everyday `heddle help` follows the core loop:
inspect, adopt or clone, save work, isolate a thread, prove readiness,
check integration, land or push, undo, inspect history, and recover.
The exact verb list comes from the command contract table so human help
and `heddle help --output json` do not drift. Collaboration and
annotation surfaces such as `review`, `discuss`, and `context` stay
behind `heddle help advanced` and their topic pages for the moments you
need them. First-time users see the smallest surface that explains where
they are, what is in flight, what to do next, and how to recover.

State IDs follow the same logic. Every state-taking verb accepts the same
specifiers — full change ID, 4-character-or-longer prefix, marker name,
`HEAD`, `HEAD~N`, thread name — so the muscle memory you build on
`heddle show` carries to `heddle diff`, `heddle revert`,
`heddle query --attribution --state`, `heddle review show`,
and `heddle retro --since`. One acceptance rule, every state-taking verb.

Restraint in defaults: child thread workflows can share a Cargo target
directory whenever the workspace has a `Cargo.toml`, because ten parallel
`cargo build` invocations against this codebase would otherwise eat tens
of GB of disk.

## 5. Honesty

Claims match behavior. The `MergeOutput` schema separates `blockers`
(reasons the operation could not advance state) from `warnings`
(non-blocking nudges when state did advance) — and the `status` field
follows the truth: `"blocked"` flips when there are real blockers, even
if the underlying integration engine itself completed
([`crates/core/src/merge/mod.rs`](../crates/core/src/merge/mod.rs),
[`crates/cli/src/cli/commands/operator_core.rs`](../crates/cli/src/cli/commands/operator_core.rs)).
A merge that landed but couldn't write its git commit doesn't get to
report `"completed"`.

`heddle capture --confidence` extends the same logic to attribution. The
flag is honest by convention — `≥0.9` only when build, tests, and manual
verification all passed; `0.75–0.89` when most signals passed; below
`0.75` for drafts. The field exists so callers don't have to lie about
how sure they are.

## What this excludes

We don't ship features that can't be made first-class. We don't preserve
backward compatibility as a goal in itself — pre-1.0, the surface gets
broken when the surface is wrong, and `AGENTS.md` is explicit about it.
We don't add verbs to cover for missing primitives — when an idiom
emerges, we name the idiom rather than scatter flags across four verbs.
We don't pretty-print JSON everywhere — `heddle show` gets that
affordance; everything else emits compact single-line JSON for
line-oriented streaming.

## How this is enforced

`heddle doctor docs --all --output json` walks every `heddle <verb>` invocation
embedded in a tracked markdown file and reports drift: verbs that no
longer exist, flags that aren't on a verb, literal values for
`--workspace`, `--scope`, and `--kind` that aren't in the valid set
([`crates/cli/src/cli/commands/doctor_docs.rs`](../crates/cli/src/cli/commands/doctor_docs.rs)).
It's built on clap's own `Cli::command()`, so it's always in sync with
the binary you ship. Wire it into CI and the docs can't rot quietly.

[`docs/json-schemas.md`](json-schemas.md) is the JSON contract — if a
sample there disagrees with the wire output, one of them is wrong, and
either way it's a fix. Tests in `crates/objects/` and `crates/cli/`
cover the failure-quality predicates and the `blockers`/`warnings`
schema so the messages an agent sees on a full disk or a dirty merge
stay specific across refactors.

Read the principles before adding or changing CLI surface. If a change
makes one of them weaker, the change is wrong.
