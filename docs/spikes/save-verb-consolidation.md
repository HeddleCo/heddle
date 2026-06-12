# heddle#461 - save/land verb consolidation

Status: decision spike

## Decision

Consolidate the user-facing save/land path to one canonical save verb and one
canonical land verb:

1. `heddle commit -m "..."` is the canonical save command for the current
   checkout. It must be repo-type aware: in Git-overlay repositories it records
   Heddle state and closes the Git checkpoint gap; in isolated/native checkouts
   it records Heddle state only. `capture` remains an advanced/internal
   savepoint primitive, and `checkpoint` stops being a top-level user
   breadcrumb.
2. Rename the managed-thread landing command from `ship` to `land`. `land`
   integrates a ready thread locally and may push only when explicitly asked.
   `push` remains the publish verb. `merge` remains an advanced/manual merge
   primitive, not the normal managed-thread breadcrumb.
3. Use top-level `sync`, `resolve`, `continue`, and `abort` for recovery and
   freshness. Stop suggesting `thread refresh` and `thread resolve` from
   `next_action`; they are implementation-shaped verbs and are the source of
   current loops.

The intended canonical breadcrumb chain is:

```text
commit -> ready -> land -> push|cleanup
sync -> ready -> land
resolve <path> -> continue -> land
```

Every JSON `next_action` must be executable from the checkout that emitted it
and must name a canonical verb. That invariant is more important than
preserving the current raw command strings.

## Repo-type vocabulary

The repository layer exposes two capabilities: `GitOverlay` and
`NativeHeddle` (`crates/repo/src/repository.rs:110-114`). `Repository::open`
walks to `.heddle`, and materialized worktrees can point their `.heddle`
directory at a shared object store (`crates/repo/src/repository.rs:636-666`).
The capability check is based on whether the root has Git metadata
(`crates/repo/src/repository.rs:2458-2464`).

For this spike, "isolated checkout" means a `heddle start --path` checkout: it
writes `.heddle/objectstore` and `.heddle/HEAD`, materializes files, and does
not create `.git` (`crates/cli/src/cli/commands/worktree_cmd/helpers.rs:249-301`).
That checkout therefore opens as `NativeHeddle`, even when it shares object
storage with a Git-overlay parent.

## Current verb x repo-type matrix

| Verb | Actual behavior | Git-overlay validity | Isolated/native validity | Wrong-context behavior | Breadcrumb sources |
| --- | --- | --- | --- | --- | --- |
| `commit` | Daily-driver save path. The command is wired at `crates/cli/src/main.rs:412`; its args describe a "recoverable Heddle state" plus a Git checkpoint in Git-overlay mode (`crates/cli/src/cli/cli_args/commands_args.rs:272-307`). | Valid. It preflights Git-overlay mutation, captures state, creates a Git checkpoint from either the index or worktree path, and coalesces the ops (`crates/cli/src/cli/commands/git_compat.rs:113-133`, `crates/cli/src/cli/commands/git_compat.rs:280-423`). It can also close a clean-worktree `needs_checkpoint` gap (`crates/cli/src/cli/commands/git_compat.rs:150-190`). | Valid. When capability is not `GitOverlay`, it creates a snapshot and returns `status: committed` without creating a Git commit (`crates/cli/src/cli/commands/git_compat.rs:198-243`). | Plain Git is rejected by the initial Heddle preflight; missing messages and clean trees produce recovery advice (`crates/cli/src/cli/commands/git_compat.rs:113-133`, `crates/cli/src/cli/commands/git_compat.rs:468-485`, `crates/cli/src/cli/commands/git_compat.rs:1024-1034`). | Its `next_action` is verification advice, `heddle verify`, or `heddle push` (`crates/cli/src/cli/commands/git_compat.rs:973-984`). Git-overlay and native dirty-state verification advice also point at `commit` (`crates/cli/src/cli/commands/git_overlay_health.rs:2739-2755`, `crates/cli/src/cli/commands/git_overlay_health.rs:3087-3161`). |
| `capture` | Records a granular Heddle snapshot with explicit intent. The command is wired at `crates/cli/src/main.rs:384-408`; args include intent, confidence, force, agent attribution, and `--split` (`crates/cli/src/cli/cli_args/commands_args.rs:206-270`). | Valid. It preflights Git-overlay mutation, records a snapshot, may run hooks, and best-effort updates Git intent-to-add after capture (`crates/cli/src/cli/commands/snapshot.rs:128-154`, `crates/cli/src/cli/commands/snapshot.rs:173-193`, `crates/cli/src/cli/commands/snapshot.rs:444-538`). | Valid. In a native target-thread checkout, text output can recommend `heddle ready` after capture (`crates/cli/src/cli/commands/snapshot.rs:235-242`). | Plain Git is rejected; missing intent suggests `heddle capture -m "..."` (`crates/cli/src/cli/commands/snapshot.rs:128-154`, `crates/cli/src/cli/commands/snapshot.rs:267-284`). | Verification can recommend it for dirty work, and confidence blockers currently recover with `heddle capture -m "..." --confidence <confidence>` (`crates/cli/src/cli/commands/git_overlay_health.rs:2739-2755`, `crates/cli/src/cli/commands/git_overlay_health.rs:3087-3161`, `crates/cli/src/cli/commands/workflow.rs:1192-1237`). |
| `checkpoint` | Git-facing checkpoint primitive. The command is wired at `crates/cli/src/main.rs:766`; the module explicitly distinguishes granular captures from Git checkpoints (`crates/cli/src/cli/commands/checkpoint.rs:1-12`). | Valid only in Git-overlay. It requires Git identity/ref preflights, ensures current Heddle state, requires a clean worktree when configured, writes through the current checkout to Git, and records the checkpoint (`crates/cli/src/cli/commands/checkpoint.rs:120-194`). | Invalid. This is the current #458 trap when a breadcrumb sends an operator into an isolated checkout. | The command returns recovery advice saying `heddle checkpoint` is only for Git-overlay repositories and makes `heddle commit -m "..."` the primary recovery (`crates/cli/src/cli/commands/checkpoint.rs:285-299`). | Git-overlay verification recommends it for `needs_checkpoint` (`crates/cli/src/cli/commands/git_overlay_health.rs:2717-2737`, `crates/cli/src/cli/commands/git_overlay_health.rs:2781-2802`), and `checkpoint` itself passes through verification advice as `next_action` (`crates/cli/src/cli/commands/checkpoint.rs:336-363`). |
| `ready` | Checks whether a thread can be landed. It may capture dirty work when `-m` is supplied, evaluates freshness/semantic/confidence blockers, marks the thread `Ready` or `Blocked`, and never lands, checkpoints, or pushes (`crates/cli/src/cli/cli_args/commands_main.rs:219-226`, `crates/cli/src/cli/commands/ready_cmd.rs:49-96`, `crates/cli/src/cli/commands/ready_cmd.rs:152-233`). | Valid. It can capture dirty work and then returns a scoped next action from readiness/preview state (`crates/cli/src/cli/commands/ready_cmd.rs:260-342`). | Valid. The same code compares Heddle trees for dirty native worktrees (`crates/cli/src/cli/commands/ready_cmd.rs:451-466`). | Missing dirty-work intent returns `heddle ready -m "..."`; verification states such as `needs_init`, `needs_import`, `needs_reconcile`, and `git_branch_advanced` block readiness (`crates/cli/src/cli/commands/ready_cmd.rs:58-84`, `crates/cli/src/cli/commands/ready_cmd.rs:344-349`, `crates/cli/src/cli/commands/ready_cmd.rs:496-544`). | On success it points to `ship` if the merge has already been previewed, otherwise to `merge <thread> --preview` (`crates/cli/src/cli/commands/ready_cmd.rs:234-247`). |
| `sync` | Top-level smart sync. The command is wired at `crates/cli/src/main.rs:325-337` and accepts an optional thread (`crates/cli/src/cli/cli_args/commands_args.rs:862-868`). | Valid. It first handles Git-overlay remote drift, including pull/push/import-style advice, then falls through to thread workflow sync (`crates/cli/src/cli/commands/operator_loop.rs:41-124`). | Valid for managed thread freshness. The workflow sync path resolves a thread, previews staleness, refreshes if safe, and points back to `ship` (`crates/cli/src/cli/commands/workflow.rs:84-185`). | Active merge/rebase operations block it and point to `heddle continue` (`crates/cli/src/cli/commands/operator_loop.rs:41-57`). | `sync` emits `heddle ship` after current/refreshed state; stale/conflicted previews can inherit merge-preview recommendations (`crates/cli/src/cli/commands/workflow.rs:84-185`). |
| `thread refresh` | Replays a thread onto its target. It is a subcommand at `crates/cli/src/cli/cli_args/commands_thread.rs:74-75` and dispatches through `cmd_thread_refresh` (`crates/cli/src/cli/commands/thread_cmd.rs:222-293`). | Valid for threads with a target and execution path. It can update thread metadata after a successful rebase/refresh (`crates/cli/src/cli/commands/thread_cmd.rs:381-527`). | Valid for isolated checkout paths recorded in thread metadata; conflict state can be written into that checkout (`crates/cli/src/cli/commands/thread_cmd.rs:417-503`). | Branch-like threads without an execution path must be current; otherwise the advice says to switch and refresh (`crates/cli/src/cli/commands/thread_cmd.rs:396-416`, `crates/cli/src/cli/commands/thread_cmd.rs:739-757`). | Thread advice recommends refresh for stale threads (`crates/repo/src/thread_advice.rs:103-117`). Refresh conflict recovery points to `conflict list`, `resolve`, `continue`, and then `merge --preview` (`crates/cli/src/cli/commands/thread_cmd.rs:656-724`). |
| `merge` | Low-level thread merge/preview primitive. It is wired at `crates/cli/src/main.rs:565-582` and accepts `--preview`, `--no-commit`, `--no-semantic`, and `--git-commit` (`crates/cli/src/cli/cli_args/commands_args.rs:697-724`). | Valid. It opens the active worktree path, previews or applies thread integration, and can optionally write a Git checkpoint when `--git-commit` is used (`crates/cli/src/cli/commands/merge/mod.rs:183-232`, `crates/cli/src/cli/commands/merge/mod.rs:1116-1187`). | Valid for isolated thread workflows because it resolves the active worktree path before operating (`crates/cli/src/cli/commands/merge/mod.rs:183-199`). | Active merges, dirty preview targets, missing source captures, conflicts, and stale previews each return specific advice (`crates/cli/src/cli/commands/merge/mod.rs:338-398`, `crates/cli/src/cli/commands/merge/mod.rs:434-440`, `crates/cli/src/cli/commands/merge/mod.rs:925-964`, `crates/cli/src/cli/commands/merge/mod.rs:1967-2026`). | Clean preview points to `ship`; stale preview points to a refresh action; apply conflicts point to `continue`; a post-snapshot Git checkpoint failure can suggest `checkpoint` (`crates/cli/src/cli/commands/merge/mod.rs:709-731`, `crates/cli/src/cli/commands/merge/mod.rs:1967-2026`, `crates/cli/src/cli/commands/merge/mod.rs:1116-1187`). |
| `ship` | Composite landing command. Args describe capture, refresh, land, checkpoint, and optional push (`crates/cli/src/cli/cli_args/commands_main.rs:197-204`, `crates/cli/src/cli/cli_args/commands_args.rs:870-891`). | Valid. It reopens the thread execution root, captures dirty work, refreshes stale work if safe, merges or adopts manual resolution, creates a Git checkpoint after integration, and optionally pushes (`crates/cli/src/cli/commands/workflow.rs:188-352`, `crates/cli/src/cli/commands/workflow.rs:354-650`). | Valid for isolated managed-thread checkouts. It opens the active worktree path specifically so operators can run it from the parent or child checkout (`crates/cli/src/cli/commands/workflow.rs:188-201`). | Git-overlay remote-behind/diverged and index-lock states are refused before landing because they could leave Heddle landed without a Git checkpoint (`crates/cli/src/cli/commands/workflow.rs:267-269`, `crates/cli/src/cli/commands/workflow.rs:728-789`). | Integrated output points to `push` when publish is still needed, otherwise cleanup; blocked output inherits preview or blocker actions (`crates/cli/src/cli/commands/workflow.rs:469-503`, `crates/cli/src/cli/commands/workflow.rs:673-725`). |
| `resolve` | Top-level file conflict resolver. It is wired at `crates/cli/src/main.rs:584-592` and supports `--list`, `--all`, side selection, `--force`, and `--abort` (`crates/cli/src/cli/cli_args/commands_args.rs:1184-1211`). | Valid when a merge state exists. It lists conflicts, marks files resolved, writes ours/theirs when requested, and can abort the merge state (`crates/cli/src/cli/commands/resolve.rs:27-59`, `crates/cli/src/cli/commands/resolve.rs:84-216`). | Valid in isolated checkouts when conflict state was written there by refresh/merge (`crates/cli/src/cli/commands/thread_cmd.rs:656-724`). | No merge state points to `status`; no conflicts/path-not-found points to `resolve --list`; marker checks point back to `resolve <path>` or `--force` (`crates/cli/src/cli/commands/resolve.rs:300-355`). | `continue` recommends `resolve --list` or `resolve <path>` when conflicts remain (`crates/cli/src/cli/commands/operator_core.rs:203-247`). |
| `thread resolve` | Thread-level "make this thread unblocked/stale-clean" helper. It is a subcommand at `crates/cli/src/cli/cli_args/commands_thread.rs:83-84`. | Valid, but it mixes freshness, conflict recovery, manual review, and blocker acknowledgement (`crates/cli/src/cli/commands/thread_shaping.rs:257-415`). | Valid when it follows the thread execution path or conflict state (`crates/cli/src/cli/commands/thread_shaping.rs:504-536`). | If blockers remain and no active operation supplies a better action, it can return the same local action that invoked it (`crates/cli/src/cli/commands/thread_shaping.rs:397-415`, `crates/cli/src/cli/commands/thread_shaping.rs:449-464`). | Thread advice maps stale/blocked/review states to `heddle thread resolve <thread>` (`crates/repo/src/thread_advice.rs:21-31`, `crates/repo/src/thread_advice.rs:103-153`, `crates/repo/src/thread_advice.rs:170-174`). This is the #456 self-loop source. |
| `continue` / `abort` | Active-operation wrappers. They are top-level commands (`crates/cli/src/cli/cli_args/commands_main.rs:191-195`) and dispatch through the operator loop (`crates/cli/src/main.rs:339-341`). | Valid for active merge/rebase/operator states. `continue` resolves remaining conflicts or snapshots a completed manual merge; `abort` aborts merge/rebase/operator state (`crates/cli/src/cli/commands/operator_core.rs:203-304`). | Valid for isolated conflict states because the operator loop reopens the active worktree path (`crates/cli/src/cli/commands/operator_loop.rs:22-30`). | With unresolved conflicts, `continue` blocks on `resolve --list` and recommends the first `resolve <path>`; rebase manual-resolution blockers can still recommend `capture` (`crates/cli/src/cli/commands/operator_core.rs:203-247`, `crates/cli/src/cli/commands/operator_core.rs:355-391`). | Used after `resolve`; stale/conflict refresh advice also points through `continue` (`crates/cli/src/cli/commands/thread_cmd.rs:656-724`). |

Related commands such as `try`, `attempt`, `thread move`, and `thread absorb`
participate in higher-level workflows, but they are not save/land verbs for
this consolidation.

## Current breadcrumb graph

This graph is the current effective command guidance surface, not just the
nominal command list.

```text
status/thread show
  -> verification action: commit | capture | checkpoint | verify | pull | push | bridge import
  -> thread action: thread refresh | thread resolve | ready | merge --preview | ship

capture
  -> verification action
  -> ready, in native target-thread text output

commit
  -> verification action | verify | push

checkpoint
  -> verification action

ready
  -> ready -m "...", when dirty intent is missing
  -> merge <thread> --preview, when ready but not previewed
  -> ship --thread <thread> --no-push, when previewed
  -> capture -m "..." --confidence <confidence>, for confidence/test blockers

sync
  -> continue, when an active operation blocks sync
  -> pull | push | import/merge-preview style remote advice, for Git-overlay drift
  -> ship, after a current or refreshed thread
  -> merge-preview inherited action, when the stale preview blocks

thread refresh
  -> conflict list -> resolve <path> -> continue -> merge --preview
  -> switch/current-checkout advice, for branch-like threads

merge --preview
  -> ship, when clean
  -> thread refresh, when stale
  -> no action, when preview conflicts need manual inspection

merge
  -> continue, when apply produces conflicts
  -> checkpoint, when a Git checkpoint write fails after a Heddle merge snapshot

resolve <path>
  -> continue, via operator guidance

continue
  -> resolve --list | resolve <path>, while conflicts remain
  -> ship, after all conflicts are manually resolved
  -> capture -m "Manual resolution", for rebase manual-resolution blockers

thread resolve
  -> thread resolve, when blockers remain and the local action is preserved
  -> conflict list | resolve <path> | continue, when merge state exists
  -> ship, when manual resolution is current

ship
  -> push, when local land succeeded and publish is still pending
  -> thread cleanup --merged --dry-run, when no publish action remains
```

Primary citations:

- `status` and `thread show` print verification or thread summary guidance
  (`crates/cli/src/cli/commands/status.rs:935-965`,
  `crates/cli/src/cli/commands/status.rs:1004-1017`,
  `crates/cli/src/cli/commands/thread.rs:2815-2882`,
  `crates/cli/src/cli/commands/thread.rs:2893-2899`,
  `crates/cli/src/cli/commands/thread.rs:3102-3104`).
- `effective_next_action` prioritizes unverified trust, active operations,
  remote tracking, thread fallback, and import hints depending on scope
  (`crates/cli/src/cli/commands/next_action.rs:62-163`).
- Thread advice maps stale/blocked/ready states to refresh, resolve, ready,
  merge-preview, or ship actions (`crates/repo/src/thread_advice.rs:103-153`,
  `crates/repo/src/thread_advice.rs:170-174`).
- `thread_landing::contextual_thread_action` only rewrites `merge` and `ship`
  actions for an isolated checkout context, which leaves other verbs to rely on
  their own repo-type correctness (`crates/cli/src/cli/commands/thread_landing.rs:1-82`).
- The status follow-up helper encodes the current intended chain:
  `capture -> ready`, `ready -> merge --preview`, `merge --preview -> ship`,
  and `checkpoint|commit -> push`
  (`crates/cli/src/cli/commands/status.rs:1796-1815`).

## Identified traps

### #456: `thread resolve` can point at itself

`RecommendedAction::Resolve` and `RecommendedAction::Review` both render as
`heddle thread resolve <thread>` (`crates/repo/src/thread_advice.rs:21-31`).
Blocked threads can therefore carry `thread resolve` as their local recommended
action (`crates/repo/src/thread_advice.rs:128-137`). `cmd_thread_resolve` reads
that recommendation from the thread summary and, if blockers remain and no
active operation supplies a better next action, returns the local action again
(`crates/cli/src/cli/commands/thread_shaping.rs:337-415`,
`crates/cli/src/cli/commands/thread_shaping.rs:449-464`).

That is a literal breadcrumb cycle:

```text
thread resolve <thread> -> thread resolve <thread>
```

### #458: `checkpoint` vs `commit` is a repo-type trap

Git-overlay verification recommends `heddle checkpoint -m "..."` for
`needs_checkpoint` (`crates/cli/src/cli/commands/git_overlay_health.rs:2717-2737`,
`crates/cli/src/cli/commands/git_overlay_health.rs:2781-2802`). The
`checkpoint` command then rejects native/isolated checkouts and says the primary
recovery is `heddle commit -m "..."` (`crates/cli/src/cli/commands/checkpoint.rs:285-299`).

The code already knows `commit` is repo-type aware
(`crates/cli/src/cli/commands/git_compat.rs:198-243`). Suggesting
`checkpoint` to humans or agents is therefore exposing an implementation
primitive instead of the safe save command.

### #459: `blocked` is not actionable enough

Thread advice maps `ThreadState::Blocked` to `thread resolve`
(`crates/repo/src/thread_advice.rs:128-137`), while status output also has a
special case that keeps genuine inter-thread blockers from being overwritten by
verification advice (`crates/cli/src/cli/commands/status.rs:996-1024`). The
state can be correct or incorrect independently of the breadcrumb. The
consolidation should require each `blocked` JSON output to include a concrete
blocker kind and an executable canonical next action, not just a generic
`thread resolve`.

### #460: refresh is doing work users expect merge to do

`merge --preview` can report stale input and recommend a refresh action instead
of performing reconciliation (`crates/cli/src/cli/commands/merge/mod.rs:1967-2014`).
The actual freshness work lives in `thread refresh`
(`crates/cli/src/cli/commands/thread_cmd.rs:381-527`) and top-level `sync`
wraps that path before returning to `ship`
(`crates/cli/src/cli/commands/workflow.rs:84-185`). The current graph can send
operators through:

```text
merge --preview -> thread refresh -> resolve/continue -> merge --preview
```

That is technically accurate but ergonomically wrong. Managed-thread
freshness should be a `sync` responsibility, and managed-thread landing should
be a `land` responsibility.

## Recommended consolidated surface

### Canonical public verbs

| Verb | Recommendation | Rationale | JSON impact |
| --- | --- | --- | --- |
| `commit` | Keep as the only normal save verb. Make every dirty-work, missing-checkpoint, and manual-resolution save breadcrumb use `commit -m "..."`, including confidence-bearing cases. | Current `commit` already does the repo-type branch correctly: Git-overlay capture plus Git checkpoint, native capture only (`crates/cli/src/cli/commands/git_compat.rs:150-243`). | Replace `checkpoint`, most `capture`, and rebase-manual-resolution breadcrumbs with `commit`. |
| `ready` | Keep as the readiness/preflight verb. It may inspect or mark a thread ready/blocked, but its successful next action should be `land`, not `merge --preview` or `ship`. | The command already centralizes capture-if-message, freshness, semantic risk, and policy checks (`crates/cli/src/cli/commands/ready_cmd.rs:152-247`). The confusing part is the outgoing edge. | Replace `ready -> merge --preview -> ship` with `ready -> land`. |
| `sync` | Keep as the top-level freshness/remote-drift verb. Use it instead of suggesting `thread refresh`. | `cmd_sync_smart` already handles active operations and Git remote drift before workflow sync (`crates/cli/src/cli/commands/operator_loop.rs:41-124`), and workflow sync already refreshes threads when safe (`crates/cli/src/cli/commands/workflow.rs:84-185`). | Replace stale-thread `next_action` strings with `heddle sync` or `heddle sync --thread <thread>`. |
| `land` | Add/rename from `ship`. It lands a ready managed thread locally. `--push` is explicit; otherwise post-land output can point to `push`. | Current `ship` is the right composite behavior but the wrong name: it captures, refreshes, merges, checkpoints, and optionally pushes (`crates/cli/src/cli/commands/workflow.rs:188-650`). "Land" names local integration; "push" names publication. | Replace all `ship` next actions with `land`. Keep publish as a separate `push` next action unless the operator chose `land --push`. |
| `resolve` | Keep top-level file conflict resolution only. | The implementation is concrete and path-based (`crates/cli/src/cli/commands/resolve.rs:27-216`). | Active conflict output should use `resolve --list` or `resolve <path>`, never `thread resolve`. |
| `continue` / `abort` | Keep as active-operation controls. | The operator core already has the correct conflict continuation semantics (`crates/cli/src/cli/commands/operator_core.rs:203-304`). | `continue` remains the edge after `resolve`; `abort` remains an escape hatch, not a normal save/land edge. |
| `push` | Keep as publish. | Git-overlay remote tracking already distinguishes pull/push/import actions (`crates/cli/src/cli/commands/git_overlay_health.rs:2378-2479`). | Post-land and post-commit Git-overlay outputs can point to `push` when needed. |

### Advanced or removed from breadcrumbs

| Verb | Recommendation |
| --- | --- |
| `checkpoint` | Remove from top-level guidance. If the primitive remains, hide or move it under an advanced Git bridge namespace. No `next_action`, recovery command, or user-facing status should suggest it. |
| `capture` | Keep as an advanced granular savepoint command for users who explicitly want Heddle captures without Git checkpointing. Do not use it as the default dirty-work or manual-resolution `next_action`; use `commit`. |
| `merge` | Keep as an advanced/manual merge primitive. Managed-thread UX should use `ready` for preview/readiness and `land` for integration. Do not use `merge --preview` as the normal readiness breadcrumb. |
| `thread refresh` | Keep as an implementation or advanced diagnostic command if needed. Stale managed-thread breadcrumbs should use top-level `sync`. |
| `thread resolve` | Remove from breadcrumbs and preferably remove or hide. Its current behavior is too broad and can self-loop. Map its legitimate cases to `sync`, `ready`, `resolve`, `continue`, or `land`. |
| `ship` | Rename to `land` pre-1.0. No compatibility shim is required by project policy, but command help and JSON templates must migrate together. |

## JSON `next_action` contract

Agents currently consume a raw command string. The implementation issue should
preserve a command string but tighten the contract:

1. `next_action` must be executable from the checkout that emitted it. A
   native/isolated checkout must never emit `checkpoint`.
2. `next_action` must use canonical verbs only: `commit`, `ready`, `sync`,
   `land`, `resolve`, `continue`, `abort`, `push`, or cleanup.
3. `next_action` must not be a no-op self-loop. If the current command cannot
   make progress, emit a concrete blocker with no `next_action`, or emit the
   next different canonical command.
4. Add semantic metadata alongside the command string so agents do not parse
   English or command names:

```json
{
  "next_action": "heddle land --thread feature --no-push",
  "next_action_kind": "land",
  "next_action_target": "thread:feature",
  "next_action_repo_type": "native-heddle"
}
```

The schema addition is optional for the first implementation pass, but the
string invariant is not optional. The migration is pre-1.0, so changing command
strings from `ship` to `land` and `checkpoint` to `commit` is acceptable.

## Implementation follow-up checklist

1. Add/rename `ship` to `land` and update help/catalog text so the everyday
   workflow is `commit -> ready -> land -> push`.
2. Change Git-overlay `needs_checkpoint` verification advice from
   `checkpoint` to `commit` (#458).
3. Remove `checkpoint` from all `next_action`, `recommended_action`, recovery,
   and status/thread-show guidance. Keep any primitive hidden or advanced.
4. Replace dirty-work and manual-resolution `capture` breadcrumbs with
   repo-type-aware `commit`; keep `capture` as explicit advanced usage only.
5. Replace `ready -> merge --preview -> ship` with `ready -> land`; fold managed
   preview/readiness into `ready` output.
6. Replace stale-thread `thread refresh` breadcrumbs with top-level `sync`
   (#460).
7. Remove `thread resolve` as a breadcrumb. For active conflicts, emit
   `resolve`/`continue`; for stale work, emit `sync`; for clean ready work,
   emit `land`; for unresolved policy blockers, emit a blocker with no
   self-loop (#456, #459).
8. Add a centralized next-action validator used by JSON emitters in status,
   thread show/list, ready, merge, sync, continue, commit, capture, and land.
   It should reject non-canonical verbs, wrong repo-type verbs, and self-loops.
9. Add regression tests for:
   - isolated checkouts never emitting `checkpoint`;
   - no `next_action` beginning with `heddle thread resolve`;
   - stale managed threads suggesting `sync`, not `merge --preview` or
     `thread refresh`;
   - dirty work suggesting `commit`, not `capture`, unless the command is an
     explicit advanced capture path;
   - ready threads suggesting `land`, not `ship`.
10. Update command docs, command catalog templates, and help topics in the same
    implementation branch so agents and humans see the same surface.
