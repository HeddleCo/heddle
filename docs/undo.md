# `heddle undo`

A safety net for the operations a daily user is most likely to want to roll back.

This document describes the user-facing surface shipped under
[HeddleCo/heddle#23](https://github.com/HeddleCo/heddle/issues/23). The
design + 0.3 scope cut for cross-thread cases lives in
[docs/design/cross-thread-undo.md](design/cross-thread-undo.md). Remote-
affecting undo and persistent cross-invocation redo are tracked as
follow-ups.

## Undoable operations

| Operation        | What undo does                                                          |
|------------------|-------------------------------------------------------------------------|
| `heddle capture` | Restores `HEAD` and the worktree to the immediate parent state.         |
| `heddle land` (non-FF) | Restores `HEAD` **and** both thread refs; drops the landed state.  |
| `heddle land` (FF)     | Restores `HEAD` **and** the landed-into thread ref to its prior tip; the source thread is untouched. |
| `heddle thread switch` | Restores `HEAD` to the previous thread state. |
| `heddle thread create`   | Deletes the thread (if no further work landed on it).           |
| `heddle thread drop`     | Recreates the thread at its pre-drop tip.                       |
| `heddle thread rename`   | Renames back.                                                   |
| `heddle thread marker create`   | Deletes the marker.                                             |
| `heddle thread marker delete`     | Recreates the marker at its prior state.                        |
| `heddle redact apply`    | Removes the redaction record so the next materialize restores the original bytes. Requires `--allow-redact-undo` (see "Safety contracts"). Refused when the blob has since been purged. |
| `heddle sync`          | Restores HEAD **and** the refreshed thread ref to the pre-sync tip in a single undo step. The full replay is grouped into one oplog batch, so undo never lands on an intermediate replay state. Refused when a blob reachable from the prior tree has since been purged. |

The list above is the **shipped** surface for v0.2. The inverses live in
`crates/cli/src/cli/commands/undo_apply.rs`; the oplog records that drive them
live in `crates/oplog/src/oplog/oplog_types.rs::OpRecord`.

## Not undoable (today)

These intentionally fall outside the MVP — they either touch state we don't
own, are destructive by design, or need a substrate change we haven't shipped:

- **`heddle push` / `heddle pull`** — remote-affecting. Reverting them would
  require coordinating with the other side. File a follow-up if you need this.
- **`heddle redact purge`** — physically removes blob bytes; refused by `heddle undo`
  with a single clear message naming the affected op. Documented irreversible
  in `OpRecord::Purge`. Even with `--allow-redact-undo`, an undo chain that
  reaches across a purged redaction is refused: the `Redaction` record is
  the only on-disk audit trail that the bytes were destroyed, and removing
  it would lie about local storage.
- **`heddle start <name> --path <dir>`** — refused while the materialized
  worktree at `<dir>` still exists. The undo inverse only deletes the
  thread ref; without first tearing the worktree down, the directory would
  be left with a `.heddle/HEAD` pointing at a thread that no longer
  exists. Tear it down explicitly with `heddle thread drop <name>
  --delete-thread`, then re-run `heddle undo`. See
  [docs/design/cross-thread-undo.md](design/cross-thread-undo.md) for
  the full design.
- **Cross-worktree shared-backend undo** — two checkouts sharing one
  `.heddle/refstore` (the `heddle start --path` setup) can step on each
  other's threads. 0.3 supports single-worktree usage only; cross-
  worktree safety is filed as a follow-up. See the design doc.
- **Redo across CLI invocations** — `heddle undo --redo` works within the same shell
  session but is not yet persisted across processes.

## Safety contracts

The CLI is designed to **fail loud** instead of silently corrupting state. The
contracts below are enforced by integration tests in
`crates/cli/tests/core_functionality/undo_and_special.rs`.

- **Redact-undo opt-in.** `heddle undo` refuses to roll back a `heddle redact
  apply` unless you pass `--allow-redact-undo`. The inverse removes the
  redaction record, so the next materialize restores the original blob bytes
  — i.e. previously-hidden content becomes readable. The opt-in turns the
  re-exposure into an explicit decision instead of a side effect of a casual
  multi-step undo. Refused regardless of the flag when a `Purge` has
  destroyed the underlying bytes (the redaction's audit-trail role is then
  load-bearing).

- **Dirty worktree refusal.** If you have uncommitted changes (modified
  tracked files, untracked files), `heddle undo` refuses and surfaces the
  dirty paths. Capture the changes with `heddle capture -m "..."` (or remove
  them) and retry. The check is shared with `revert` and `sync`.
- **Destructive-boundary refusal.** If the state the inverse would restore
  has been removed from the object store — typically by `heddle maintenance gc --prune`
  reaching past the live oplog window — `heddle undo` refuses with a single
  clear message naming the missing op id. Restore from a backup or list past
  the boundary with `heddle undo --list`.
- **Sync-undo refusal across `purge`.** Undoing a `heddle sync` rewinds
  the attached thread to its pre-sync tip. If any blob reachable from
  that tree has since been redacted+purged, the rewind would land the
  worktree on a state whose next materialize fails with a missing-blob
  error. `heddle undo` refuses pre-mutation with a single message naming
  the sync batch and the purged blob — same fail-loud discipline as the
  Redact inverse's "Refused regardless of the flag when the underlying
  bytes have since been purged" rule. Restore from a backup or list past
  the sync with `heddle undo --list`.
- **Worktree-attached `ThreadCreate` refusal.** `heddle undo` refuses to
  roll back a `heddle start <name> --path <dir>` while the materialized
  worktree at `<dir>` is still on disk. The inverse only deletes the
  thread ref; silently proceeding would orphan the worktree directory
  with a broken `.heddle/HEAD`. Run `heddle thread drop <name>
  --delete-thread` first, then re-run `heddle undo`. Same refusal fires
  for `heddle undo --preview` so preview output is honest about what the
  real command would do.
- **Idempotent re-run.** Once a batch is marked undone, the next `heddle
  undo` picks the next still-active batch (or refuses if none remain). Re-
  running `heddle undo` is never destructive.
- **`--dry-run` is non-mutating.** `heddle undo --dry-run` (alias of
  `--preview`) prints the batches it would undo without touching the
  worktree, ref refs, or oplog state.

## Flags

| Flag                       | Purpose                                                     |
|----------------------------|-------------------------------------------------------------|
| `-n, --steps <N>`          | Roll back the last `N` batches (default 1).                 |
| `--list`                   | Print the recent batches without undoing.                   |
| `--depth <N>`              | How many batches `--list` shows (default 20).               |
| `--preview` / `--dry-run`  | Print what would change without applying.                   |
| `--allow-redact-undo`      | Explicit opt-in to undo a `heddle redact apply` (see "Safety contracts"). |
| `--output {auto,json,text}`| Force output format. JSON is the structured contract.        |

Run `heddle undo --help` for the curated list with examples and the explicit
"NOT undoable" reminder.

## Known caveats (filed as follow-ups)

- `OpRecord::Checkpoint` is defined but no current code path emits it; the
  variant exists for the agent-frequent-saves work in flight. When it lands
  it will need its own inverse arm in `undo_apply.rs`.
- **Redact redo is unsupported.** `heddle undo --redo` of a previously-undone
  `Redact` refuses with a clear error: the `OpRecord::Redact` entry doesn't
  preserve the full `Redaction` record (reason, redactor, signature) needed
  to faithfully re-apply, so any "redo" path would invent the missing
  fields. Re-run `heddle redact apply` to recreate. A follow-up could stash
  the removed `Redaction` on undo and restore it on redo if the round-trip
  becomes load-bearing for daily use.
