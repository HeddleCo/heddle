# Cross-thread undo — design + 0.3 scope

This doc covers what `heddle undo` does (and refuses to do) when the operation
being inverted touched a thread other than the one HEAD is currently attached
to. It builds on the single-thread MVP shipped under
[heddle#23](https://github.com/HeddleCo/heddle/issues/23), the `Redact` inverse
[heddle#98](https://github.com/HeddleCo/heddle/issues/98), and the
fast-forward inverse [heddle#99](https://github.com/HeddleCo/heddle/issues/99).
Audience: anyone touching `crates/cli/src/cli/commands/undo*` or adding a new
`OpRecord` variant that mutates more than one thread's ref.

## Semantic model

A "thread" in heddle is a named ref that points at a state ChangeId. HEAD
either attaches to a thread (and rides its ref) or detaches to a specific
state. An op is **cross-thread** when it mutates the *ref* of any thread that
is not the one HEAD is attached to at the moment the op runs.

Within a single worktree the undo stream is per-worktree (`Repository::op_scope()`
hashes `<root>/.heddle/HEAD`'s canonical path; see crates/repo/src/repository.rs:1329).
So "cross-thread" within one worktree means an op recorded by *this* checkout
that moved a thread ref the user wasn't sitting on. Multi-worktree shared-
backend cases are addressed below under "Danger zone — multi-worktree".

`heddle undo X` means: "make every persistent piece of state that X mutated
look the way it looked before X ran". For a cross-thread op that has three
layers of state:

1. **Refs** — thread tip pointers in the ref store (canonical).
2. **HEAD** — only the originating worktree's, never another worktree's.
3. **ThreadManager records** — `.heddle/threads/*` (lifecycle metadata read by
   `heddle thread list`, `heddle thread show`, `heddle delegate`, etc.).

Worktree *file contents* — the materialized files on disk — are only rewritten
when undo restores HEAD. We never reach into another checkout's filesystem.

## Variant matrix

For each `OpRecord` variant: does today's forward path touch a thread other
than HEAD's? Does today's inverse correctly restore it?

| Variant | Forward touches non-HEAD thread? | Inverse correct? | Notes |
|---|---|---|---|
| `Snapshot` | No — snapshot is on HEAD's thread by construction | ✅ | Single-thread by definition |
| `Goto` | No — only moves HEAD | ✅ | Single-thread by definition |
| `ThreadCreate` | **Yes** — creates a ref for a thread the user is not (necessarily) on; also writes a ThreadManager record | ❌ before this doc — inverse deletes ref but leaves ThreadManager record (and any attached worktree) orphaned | See "In scope" below |
| `ThreadDelete` | **Yes** when emitted — but the user-facing `heddle thread drop --delete-thread` path does *not* record this variant (drop tears the worktree down through `drop_thread_silent` in thread_cmd.rs:562 without touching the oplog); it is emitted only by the `rename` batch and a legacy `cmd_thread_delete` helper no longer wired to the CLI verb | Ref-only restore via `set_thread`. Sufficient for the rename round-trip path; no record-cleanup work is reachable here because `cmd_thread_rename` doesn't write a record under the new name | See "Out of scope" |
| `ThreadUpdate` | Defined but never emitted today | n/a | Inverse code path exists; unused |
| `Fork`, `Collapse` | n/a — never emitted today | n/a | Reserved for future thread-graph ops |
| `MarkerCreate`, `MarkerDelete` | No — markers attach to states, not threads | ✅ | Not cross-thread |
| `Checkpoint` | No (single-thread, agent-frequent-save) | n/a — no inverse arm yet | Tracked separately; see docs/undo.md |
| `Redact`, `Purge` | No — sidecar mutation on a specific (blob, state, path) | ✅ (`Redact` with `--allow-redact-undo`; `Purge` irreversible) | Documented in heddle#98 |
| `FastForward` (V1, legacy read-only) | **Yes** — advances target_thread ref | ✅ — inverse restores both HEAD and target_thread to `pre_target_id` | heddle#99 r1 fix |
| `FastForwardV2` | **Yes** — same as V1; carries `post_target_id` for deterministic redo | ✅ | heddle#99 r2 fix |
| `EphemeralThreadCollapse` | Touches a thread but only to retire its pointer | n/a — no inverse arm | Out of scope; thread auto-expired |
| `TransactionAbort`, `TransactionCommit`, `ConflictResolved` | No ref mutation | n/a — no inverse arm | Forensic-only records |

The two **already-correct** cross-thread cases are `FastForward` and
`FastForwardV2`. Their inverses restore HEAD *and* the target thread ref. They
work because both variants carry the thread name and the pre-state explicitly;
the inverse has everything it needs.

The **gap** case is `ThreadCreate`. Its forward path writes a ThreadManager
record (cmd_thread_create at thread.rs:1539 and cmd_start at thread.rs:1185)
and — when invoked as `heddle start --path` — materializes a worktree on
disk. Its inverse only deletes the ref. That gap is the in-scope work below.
`ThreadDelete` is not in scope because the user-facing delete path goes
through `cmd_thread_drop` / `drop_thread_silent`, which tears the worktree
down and marks the record Abandoned *without* recording an oplog entry —
there is nothing for undo to inverse on that path. The remaining recorder
(the rename batch) emits `ThreadDelete` only for the *old* name, which has
no orphan to clean up because the rename forward path never re-keyed the
record (see follow-ups below).

## Contract

For every cross-thread op the inverse follows these rules. These are checked
by integration tests in `crates/cli/tests/core_functionality/undo_and_special.rs`.

1. **Refs are the source of truth.** The inverse always restores any thread
   ref the forward op mutated to the recorded prior value. This holds even
   when HEAD is not on that thread.
2. **HEAD only moves when the recorded op moved it.** A cross-thread inverse
   does not detach, re-attach, or otherwise touch HEAD unless the original op
   did. (`FastForward[V2]` is the only cross-thread variant that recorded a
   HEAD move; its inverse correctly restores HEAD too.)
3. **No worktree file rewrites in another checkout.** The originating
   worktree's files may be rewritten when HEAD moves; another worktree's
   files are never touched.
4. **ThreadManager metadata mirrors refs.** When an inverse deletes a thread
   ref, the matching ThreadManager record is deleted **iff** there's no
   attached materialized worktree (see rule 5). We do not attempt to *refresh*
   `current_state` on the inverse, because no forward path in 0.3 keeps
   `current_state` fresh either — `cmd_thread_create` writes the initial value
   and nothing else updates it. Fixing the broader `current_state` staleness
   is a forward-path concern tracked separately (see follow-ups).
5. **Refuse rather than orphan a materialized worktree.** Undoing a
   `ThreadCreate` whose ThreadManager record has `materialized_path = Some(_)`
   refuses with a clear message naming the worktree path. The user must tear
   the worktree down manually before the undo can proceed. Same on the redo
   side for `ThreadDelete` (which would re-delete the ref). Rationale: the
   detached worktree is in another directory — possibly with uncommitted
   work, possibly being used by a long-running agent. The dirty-worktree
   refusal already governs the *current* worktree; the orphan-worktree
   refusal extends the same "fail loud" pattern to materialized siblings.
6. **Symmetry.** Redo follows the same rules. Where an undo refuses, the
   corresponding redo refuses for the same reason.
7. **Atomic per batch.** All refusals are enforced as pre-flight checks
   (see `crates/cli/src/cli/commands/undo.rs:88` for the analogous redaction
   gate) so a refusal happens *before* any state mutation. No half-applied
   chains.

## 0.3 scope

### In scope

1. **ThreadCreate inverse: clean ThreadManager record.** Undo of `ThreadCreate`
   removes the matching ThreadManager record alongside deleting the ref.
   Forward `cmd_thread_create` (thread.rs:1539) and `cmd_start` (thread.rs:1185)
   write the record; the inverse must remove it to avoid orphan entries
   surfaced by `heddle thread list` and `heddle thread show`.

2. **ThreadCreate inverse: refuse on materialized worktree.** If the
   ThreadManager record has `materialized_path = Some(path)` and that path
   still exists on disk, refuse with a message naming `path` and pointing the
   user at `heddle thread drop <name> --delete-thread`. The refusal is a
   pre-flight check (parallel to the redaction gate at undo.rs:88) so multi-
   batch undos don't half-apply, and so `--preview` surfaces the refusal
   instead of advertising "Would undo …".

3. **Thread rename (batch of `[ThreadCreate(new), ThreadDelete(old)]`)
   inherits the above rules.** Undo applies arms in reverse: undo
   `ThreadDelete(old)` first (restores `old`'s ref), then undo
   `ThreadCreate(new)` (deletes `new`'s ref + cleans any record under `new`).
   In practice no record exists under `new` because the forward
   `cmd_thread_rename` doesn't re-key the record — that pre-existing
   forward-path bug is filed as a follow-up; the inverse stays safe today.

4. **Tests (red-commit first).** `test_undo_thread_create_removes_record_when_no_worktree`,
   `test_undo_thread_create_refuses_with_materialized_worktree`,
   `test_undo_thread_rename_round_trips_refs_and_record`,
   `test_undo_preview_surfaces_worktree_refusal`. The rename test is a
   regression guard — it already passes today because the rename forward
   path never writes an orphan under the new name; it would catch a future
   regression that introduced one.

### Out of scope — filed as follow-ups in the PR description

These are real concerns but each requires substrate work beyond a per-batch
inverse. Filing them keeps 0.3 surgical.

- **Cross-worktree shared-backend safety.** When two checkouts share an
  oplog/refs backend (`heddle start --path` siblings), W1 can undo an op that
  moves a thread ref W2 has HEAD attached to. W2's HEAD then points at a
  stale state. Detection needs a worktree registry, which doesn't exist
  today (audit: `worktrees.toml` and equivalents not present;
  `WorktreeIndex` is a per-worktree stat cache, not a multi-worktree
  registry). Follow-up: design + ship a registry, then add a cross-worktree
  refusal mirroring the materialized-worktree case.
- **Daemon thread-tip cache invalidation.** No daemon-side thread cache
  exists today (audit: `crates/daemon/src/` reads refs on every RPC). When
  one lands, undo must broadcast invalidation. Until then this is a
  documented assumption, not a TODO.
- **Remote-affecting undo.** `heddle push`/`pull`/`fetch` cannot be rolled
  back unilaterally. Documented in `docs/undo.md` already; no in-scope work.
- **Worktree teardown command.** Today there's no `heddle thread drop
  --with-worktree` that atomically removes a thread and its materialized
  worktree. The refusal in this design points at manual teardown. A real
  teardown command is its own design.
- **Pre-existing forward-path ThreadManager bugs.** `cmd_thread_rename`
  (thread.rs:2119) does not re-key the ThreadManager record under the new
  name. `cmd_thread_delete` (thread.rs:2091) does not delete the
  ThreadManager record, and is no longer wired to the CLI verb anyway
  (`thread delete` translates to `thread drop --delete-thread` per
  core_functionality.rs:30–41, which goes through `drop_thread_silent`).
  `ThreadManager.current_state` is never refreshed by any forward path
  after the initial `cmd_thread_create` write — the field is functionally
  always-stale. These are forward-path bugs that predate this work; undo
  correctness for cross-thread cases is designed around the current
  forward behavior, not around fixing forward. Filing separately.
- **`heddle pull` rollback semantics.** Even setting aside remote effects,
  `pull` records ops on multiple threads. Out of scope for 0.3.

### Intentionally not supported in 0.3

These will be documented under "Not undoable" in `docs/undo.md` and surfaced
in `heddle undo --help`:

- Undoing `ThreadCreate` of a thread with an attached materialized worktree.
  Tear down the worktree first.
- Cross-worktree shared-backend safety. Single-worktree usage is the
  documented supported configuration for 0.3 undo.

## Danger zone

Patterns that *appear* single-thread but mutate state observable from
another vantage point. Each is either covered by an in-scope refusal, listed
as an out-of-scope follow-up, or documented as a known limitation.

- **Shared refs.** Two worktrees sharing one `.heddle/refstore` (the
  `heddle start --path` setup) see each other's ref writes immediately. The
  refusal in §5 of the contract covers the most common path (orphaning a
  materialized worktree by undoing the create that built it). The general
  case — W2 has HEAD on a thread W1 is undoing — is the out-of-scope
  follow-up above.
- **Materialized worktrees.** Same as above — these are the manifestation
  of "shared refs across two filesystems".
- **Daemon-cached thread tips.** No daemon cache today. When one ships,
  undo must invalidate. Audit recorded in `crates/daemon/src/`.
- **Rebase / ship internals recording `Goto` on thread movement.** The
  brief flags `rebase_ops.rs`, `workflow.rs`, `remote/remote_ops.rs`,
  `resolve.rs`. These emit `Goto`/`Snapshot`/`FastForward` on the current
  thread; per the variant matrix, none of them are cross-thread except the
  FF case, which is already correct. The systemic FF-strand pattern from
  heddle#110 is tracked there, not here.

## Implementation notes

The change is concentrated in `crates/cli/src/cli/commands/undo_apply.rs` and
`crates/cli/src/cli/commands/undo.rs`:

- New pre-flight helper `ensure_thread_worktree_undo_safe` in `undo.rs`,
  shaped like `ensure_redaction_undo_safe`. Walks batches, collects
  `ThreadCreate` ops, looks each up in ThreadManager, refuses if any have
  `materialized_path = Some(path)` where `path` still exists on disk.
  Called from `cmd_undo` before the worktree-clean check and before the
  `--preview` short-circuit so preview output stays honest.
- `apply_undo_entry`'s `ThreadCreate` arm gains a ThreadManager record
  deletion after `delete_thread_safely`. The deletion is best-effort: a
  missing record is not an error (some ThreadCreate paths might predate
  the record-writing change, or have had their record removed by a
  preceding `drop` op).
- `apply_redo_entry`'s `ThreadCreate` arm intentionally does **not**
  re-create the ThreadManager record on redo. The forward op carried only
  `(name, state)`; the record body (`execution_path`, `materialized_path`,
  workflow metadata, …) isn't reconstructible from the OpRecord. Redo
  restores the ref so subsequent operations behave correctly; a follow-up
  forward `heddle thread start <name>` or equivalent can re-establish the
  record if the user wants one.

No new `OpRecord` variant. No new public Repository API beyond what
ThreadManager already exposes.

## Test plan

All in `crates/cli/tests/core_functionality/undo_and_special.rs`, written
red-commit-first.

1. `test_undo_thread_create_removes_record_when_no_worktree` —
   `heddle thread create foo` → `heddle undo` → ref gone *and*
   `ThreadManager::find_by_thread("foo")` returns `None`. **Red today.**
2. `test_undo_thread_create_refuses_with_materialized_worktree` —
   `heddle start foo --path <tmp>` → `heddle undo` errors with a message
   naming the worktree path; ref and worktree both still present.
   **Red today.**
3. `test_undo_thread_rename_round_trips_refs_and_record` —
   `heddle thread create foo` → `heddle thread rename foo bar` →
   `heddle undo` → `foo` exists in refs, `bar` does not, no orphan record
   under `bar`. Regression guard; passes today vacuously because
   `cmd_thread_rename` never wrote a record under `bar`.
4. `test_undo_preview_surfaces_worktree_refusal` —
   `heddle undo --preview` against a worktree-attached ThreadCreate must
   surface the refusal instead of advertising "Would undo …", matching
   the preview-honesty pattern at undo.rs:88. **Red today.**

## Open questions deferred to follow-ups

- Should the worktree-attached refusal accept an `--allow-worktree-orphan`
  override? Today the answer is no — see the contract rationale. If user
  feedback shows the strict refusal is too aggressive, revisit.
- When ThreadManager record exists but the ref is missing (forward bug
  rooted in `cmd_thread_rename` not updating records), should undo heal the
  divergence? Today: no — undo only handles divergence introduced by ops in
  the undo window. Forward-path cleanup is a separate follow-up.
