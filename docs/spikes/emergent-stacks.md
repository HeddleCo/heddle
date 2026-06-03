# Emergent Stacked Changes

Status: proposed decision
Date: 2026-06-03
Issue: heddle#474

## Decision

Native stacked changes should emerge from the existing thread model:

- `heddle start --on <thread>` creates a child thread whose initial base is the parent thread's current tip and whose dependency is recorded as `parent_thread`.
- `heddle sync` becomes the native restack operation for a thread chain: walk recorded parent edges, replay parents before children, and update each thread's frozen base after a successful replay.
- `heddle land` keeps the "merge bottom, restack the rest" workflow native: after a landed parent reaches its target, direct children are reparented to the landed target and the remaining chain is synced.
- Stack-aware display belongs in `status` and review surfaces. Do not add a new mutating `stack` command family.

This fits the accepted CLI consolidation direction: `sync` is the canonical refresh operation, `land` is the canonical ship operation, and `stack`/`workspace` views are demoted into `status`/`ready` rather than expanded as a separate noun family (`docs/spikes/whole-cli-consolidation.md:38`, `docs/spikes/whole-cli-consolidation.md:59`, `docs/spikes/whole-cli-consolidation.md:60`, `docs/spikes/whole-cli-consolidation.md:90`, `docs/spikes/whole-cli-consolidation.md:102`, `docs/spikes/whole-cli-consolidation.md:115`, `docs/spikes/whole-cli-consolidation.md:117`).

## Capability Classification

**Shipped**

- Thread records persist `parent_thread`, `target_thread`, `base_state`, and `base_root`, so Heddle already has the metadata shape needed to describe a thread with both a live dependency and a frozen replay anchor (`crates/repo/src/thread_model.rs:195-206`, `crates/repo/src/thread_storage.rs:24-35`).
- Thread storage round-trips those fields through record serialization and deserialization (`crates/repo/src/thread_storage.rs:88-115`, `crates/repo/src/thread_storage.rs:118-146`).
- `status` can already surface parent/child information because its output model includes `parent_thread` and `child_threads`, and thread summary collection derives stack depth and stale-from-parent data from thread records (`crates/cli/src/cli/commands/status.rs:55-148`, `crates/cli/src/cli/commands/thread.rs:655-692`, `crates/cli/src/cli/commands/status.rs:780-799`, `crates/cli/src/cli/commands/status.rs:1531-1545`).
- Hosted approval primitives already reason about a source thread, a target thread, and a pinned source state (`crates/client/src/grpc_hosted/user.rs:326-354`, `crates/client/src/grpc_hosted/user.rs:391-424`, `crates/cli/src/cli/commands/thread_approval.rs:128-184`).

**Foundation in place**

- A read-only stack model already computes stacks from `ThreadRecord::parent_thread`; it intentionally discovers stack shape without mutating repository state (`crates/repo/src/thread_stack.rs:1-14`).
- The stack planner already walks live thread refs and projects bottom-up rebase steps with old and new bases (`crates/repo/src/thread_stack.rs:211-237`, `crates/repo/src/thread_stack.rs:261-349`, `crates/repo/src/thread_stack.rs:370-403`).
- `start` has a hidden `--parent-thread` field and the delegated workflow already uses it to create child thread records based on a parent thread's current state (`crates/cli/src/cli/cli_args/commands_args.rs:579-596`, `crates/cli/src/cli/cli_args/commands_args.rs:618-620`, `crates/cli/src/cli/commands/workflow.rs:904-945`).
- Semantic merge machinery exists behind the current merge strategy plumbing, though existing non-merge callers still use `HunkOnly` today (`crates/cli/src/cli/commands/merge/merge_algo/mod.rs:20-35`, `crates/cli/src/cli/commands/merge/merge_algo/executor.rs:260-284`, `crates/cli/src/cli/commands/merge/mod.rs:1765-1780`).
- The current `ship` implementation is already the operational ancestor of `land`: it captures dirty thread work, refreshes stale work, merges the thread into the current target, checkpoints, and may push (`crates/cli/src/cli/cli_args/commands_main.rs:197-204`, `crates/cli/src/cli/commands/workflow.rs:271-352`, `crates/cli/src/cli/commands/workflow.rs:506-557`, `crates/cli/src/cli/commands/workflow.rs:653-670`).

**Planned**

- Public `start --on <thread>` should be the canonical way to create a recorded stack edge.
- `sync` should become parent-aware and should restack a chain, not only refresh one thread against its `target_thread`.
- `land` should cascade through dependent children after the bottom thread lands.
- Hosted review should render thread chains as dependency-linked review units, using the same thread state, approval, and review payload primitives already present.

## Code Grounding

### Thread Parent Relation

The core data model already records both dependency and replay-anchor information. `ThreadRecord` has `target_thread`, `parent_thread`, `state`, `base_state`, and `base_root` fields (`crates/repo/src/thread_model.rs:195-206`). The persisted `Thread` type mirrors those fields (`crates/repo/src/thread_storage.rs:24-35`), writes them into `ThreadRecord` (`crates/repo/src/thread_storage.rs:88-115`), and reads them back from `ThreadRecord` (`crates/repo/src/thread_storage.rs:118-146`). `ThreadManager::save` writes the record and workspace metadata (`crates/repo/src/thread_storage.rs:279-283`).

The existing parent relation is name-based, not guaranteed-id-based. `ThreadRecord` contains both `id` and `thread` (`crates/repo/src/thread_model.rs:195-199`), and current CLI code warns that `ThreadRecord::id` can diverge from `ThreadRecord::thread` for legacy or synced records (`crates/cli/src/cli/commands/thread_cmd.rs:1754-1759`). For the first native stack design, `parent_thread` should continue to record the parent thread name because that is the field existing stack discovery already consumes (`crates/repo/src/thread_stack.rs:1-14`, `crates/repo/src/thread_stack.rs:165-184`). A stable thread-id edge is an open hosted/distributed question, not a required local-stack prerequisite.

The existing stack reader treats `parent_thread` as the edge. It computes roots when the parent is absent or missing (`crates/repo/src/thread_stack.rs:97-127`), walks ancestors when asking for one thread's stack (`crates/repo/src/thread_stack.rs:134-163`), indexes children only when the parent exists in the record set (`crates/repo/src/thread_stack.rs:165-184`), and protects read-side tree construction from cycles by dropping the repeated node (`crates/repo/src/thread_stack.rs:186-207`). This is useful but insufficient for mutation: native `start --on` and reparenting must reject cycles at write time instead of relying on read-side omission.

Today's freshness calculation compares a thread's `base_state` to its `target_thread` tip and does not follow `parent_thread` (`crates/repo/src/snapshot_metadata.rs:144-160`). That means a thread can have a recorded parent relation today, but normal refresh/freshness code does not yet treat the parent tip as the effective upstream. This is the central gap for emergent stacks.

### Can The Base Track Another Thread's Tip?

Today the frozen base can equal another thread's tip, but it does not track that tip live by itself. `start_thread` resolves a base state from an existing thread ref, `--from`, or the current state (`crates/cli/src/cli/commands/thread.rs:1645-1693`). It then records `base_state` and `base_root` as strings/roots and persists `parent_thread` from the supplied arguments (`crates/cli/src/cli/commands/thread.rs:1849-1857`). The created thread's current state starts at that base and is marked current (`crates/cli/src/cli/commands/thread.rs:1858-1873`).

The delegated workflow already demonstrates the desired primitive in hidden form: it resolves a parent thread, passes `from=parent.current_state.unwrap_or(parent.base_state)`, and records `parent_thread=Some(parent.id)` when starting the child (`crates/cli/src/cli/commands/workflow.rs:815-819`, `crates/cli/src/cli/commands/workflow.rs:904-945`). That proves a child thread can snapshot another thread's current state as its base today. It does not prove live restacking works, because refresh still compares against `target_thread`, not `parent_thread` (`crates/repo/src/snapshot_metadata.rs:144-160`).

Recommended model: record both fields deliberately.

- `parent_thread`: the live dependency edge. `sync`, `land`, `status`, and hosted review resolve this to the parent's current tip when deciding whether the child is stale.
- `base_state`/`base_root`: the last successfully synced upstream snapshot. Replay uses this as the old base, and the oplog/provenance story can explain what changed during the restack.

A live-only pointer would lose the stable old base needed for deterministic replay and review. A snapshot-only base would fail to represent the dependency after the parent moves. Heddle already has both, so native stacks should make both meaningful rather than adding a new stack object.

## Public Creation Surface: `start --on <thread>`

`start --from <state>` already creates from a frozen state (`crates/cli/src/cli/cli_args/commands_args.rs:579-596`, `crates/cli/src/cli/commands/thread.rs:1645-1693`). `start --on <thread>` should be a separate public option because it records a live parent edge as well as choosing an initial base.

Proposed behavior:

1. Resolve `<thread>` through the managed thread registry.
2. Read the parent's live tip from the thread ref, matching the stack planner's use of live refs (`crates/repo/src/thread_stack.rs:370-403`).
3. Create the child at that tip, the same way the hidden delegated path currently passes a parent current state into `start_thread` (`crates/cli/src/cli/commands/workflow.rs:904-945`).
4. Persist `parent_thread=Some(parent thread name)`, `base_state=<parent tip>`, and `base_root=<parent tip root>`.
5. Treat `--on` and `--from` as mutually exclusive: `--from` is a detached snapshot base, while `--on` is a recorded dependency.
6. Reject cycles before writing. Existing read-side code skips cycles (`crates/repo/src/thread_stack.rs:134-163`, `crates/repo/src/thread_stack.rs:186-207`), but mutation needs a hard error because a cycle makes parent-first replay undefined.

`target_thread` has two viable meanings:

- **Parent-as-target while stacked:** set `target_thread` to the parent thread so current refresh code can be adapted incrementally. When the parent lands, cascade children to the parent's previous target.
- **Final integration target:** keep `target_thread` as the eventual branch, and make `parent_thread` the only stack dependency.

Recommendation: use final integration target as the long-term semantic model, but phase in parent-as-effective-upstream in `sync` before relying on it. The current `refresh_thread_freshness` logic only knows `target_thread` (`crates/repo/src/snapshot_metadata.rs:144-160`), so the implementation must explicitly teach freshness/replay that `parent_thread` overrides the effective upstream while the parent is active.

The existing `fork` command is not the right primitive for stack creation. It creates a branch from a state and optionally switches to a thread, then records `OpRecord::Fork`; it does not create a managed thread record with `parent_thread`/`base_state` metadata (`crates/cli/src/cli/commands/fork.rs:31-38`, `crates/cli/src/cli/commands/fork.rs:43-54`, `crates/cli/src/cli/commands/fork.rs:79-116`).

## Restack Is `sync`

Current `sync` is a one-thread operator command. It resolves one thread, previews whether that thread is stale, and calls `refresh_thread` for that thread (`crates/cli/src/cli/commands/workflow.rs:84-185`). `SyncArgs` only exposes an optional `--thread` today (`crates/cli/src/cli/cli_args/commands_args.rs:862-868`).

Current `refresh_thread` is also one-thread oriented. It loads the managed thread, requires a `target_thread`, checks freshness, opens the thread checkout, preflights a three-way conflict, tries the silent rebase path, falls back to a three-way merge on intermediate rebase conflicts, and on success updates `base_state`, `base_root`, `current_state`, and freshness (`crates/cli/src/cli/commands/thread_cmd.rs:381-416`, `crates/cli/src/cli/commands/thread_cmd.rs:417-437`, `crates/cli/src/cli/commands/thread_cmd.rs:457-467`, `crates/cli/src/cli/commands/thread_cmd.rs:505-527`).

Native stack restack should reuse the same concept but change the unit of work:

1. Select the stack containing the requested/current thread.
2. Resolve each thread's effective upstream:
   - if `parent_thread` exists and the parent is active, upstream is the parent tip;
   - otherwise upstream is `target_thread` tip.
3. Build a parent-before-child plan. The existing planner already projects this shape and returns each step's `thread`, `current_state`, `old_base`, `new_base`, `parent_thread`, and `depth` (`crates/repo/src/thread_stack.rs:211-237`, `crates/repo/src/thread_stack.rs:261-349`).
4. For each step, if `old_base == new_base`, mark it no-op.
5. If the parent changed, replay the child's delta from `old_base..current_state` onto `new_base`.
6. On success, move the thread ref, update `base_state`/`base_root` to `new_base`, set `current_state` to the replay result, and continue to descendants.
7. On conflict, stop at that thread and leave descendants untouched until `continue`.

The existing planner is read-only and therefore safe to reuse as the planning substrate: it reads records, fetches current tips through refs, and does not mutate repository state (`crates/repo/src/thread_stack.rs:1-14`, `crates/repo/src/thread_stack.rs:370-403`). Mutation belongs in the `sync` executor.

### Ordering And Idempotency

Ordering must be parent-before-child. For a linear chain this is bottom-up. For a branched stack, breadth-first by depth is acceptable because every child waits for its parent; the existing planner already emits root-first steps and projects child bases from parent projected tips (`crates/repo/src/thread_stack.rs:261-349`). The first implementation should stop the whole selected stack on the first conflict for deterministic resume. Continuing independent sibling branches after one branch conflicts is possible later, but it complicates user expectations and operation state.

Idempotency should be explicit:

- Re-running `sync` after a fully successful sync should produce no thread updates because each `base_state` already equals its effective upstream.
- Re-running after an interrupted/conflicted sync should resume from the active operation state rather than recomputing and replaying completed ancestors.
- Each successful step should be durable before the next step starts, so a process crash does not require replaying already-restacked parents.

Current operator `continue` already handles in-progress merge state by checking unresolved files, completing manual resolution, and updating thread metadata (`crates/cli/src/cli/commands/operator_core.rs:203-247`, `crates/cli/src/cli/commands/operator_core.rs:306-353`). Stack sync needs an additional stack-operation sentinel that records the planned steps, the next step index, and the conflicted thread so `continue` can resume the remaining descendants after the manual resolution commits.

### Conflict UX

When a mid-stack thread conflicts:

1. Stop with the checkout positioned at the conflicted thread.
2. Write the same merge conflict state and markers current refresh writes (`crates/cli/src/cli/commands/thread_cmd.rs:558-571`).
3. `status` should show:
   - the stack chain;
   - the blocked thread;
   - ancestors already restacked;
   - descendants waiting for `continue`.
4. The user resolves files with the canonical `resolve` flow.
5. `heddle continue` completes the manual resolution for that thread, then resumes stack sync at the next descendant.

Existing refresh conflict advice already tells the user which files conflict and routes to recovery commands (`crates/cli/src/cli/commands/thread_cmd.rs:700-724`). The strings should be updated as part of the CLI consolidation so the breadcrumbs use `sync`, `resolve`, and `continue` instead of old `thread refresh` or preview-oriented language.

### Semantic Restack

The #467 direction is to make semantic merge the default. Stack sync should use semantic replay by default because parent churn commonly appears as additive imports, `pub use` changes, or nearby function additions that Git rebase treats as textual conflicts. The merge engine already has a semantic strategy that routes parseable source through the AST-aware driver and falls back to hunk merge when semantic support is unavailable (`crates/cli/src/cli/commands/merge/merge_algo/mod.rs:20-35`, `crates/cli/src/cli/commands/merge/merge_algo/executor.rs:260-284`). The current refresh/ship callers still pass `HunkOnly` through external merge entry points (`crates/cli/src/cli/commands/merge/mod.rs:1765-1780`), so adopting semantic restack is a planned behavior change, not something current `sync` already does.

### Oplog And Provenance

The oplog already has `ThreadUpdate { name, old_state, new_state }` as a record shape (`crates/oplog/src/oplog/oplog_types.rs:63-72`). Its human description is "update thread old -> new" (`crates/oplog/src/oplog/oplog_types.rs:417-420`). Stack restack should emit a `ThreadUpdate` for each thread whose ref moves, and the stack operation should be reconstructable as a sequence of these per-thread moves.

Open schema question: `ThreadUpdate` records old and new thread states but does not encode old and new `base_state`/`parent_thread` metadata (`crates/oplog/src/oplog/oplog_types.rs:63-72`). If #469 wants complete restack provenance, the implementation likely needs either:

- a metadata delta attached to `ThreadUpdate`; or
- a new thread-metadata operation for base/parent changes.

Do not silently rely on record-file mutation alone for restack provenance. The thread record is persisted (`crates/repo/src/thread_storage.rs:279-283`), but the operation log is the user-visible history of how the stack moved.

## Land Cascades

The current `ship` path already approximates the future `land`: it captures dirty work, refreshes stale work when possible, merges the thread into the current target, checkpoints, and pushes when requested (`crates/cli/src/cli/cli_args/commands_main.rs:197-204`, `crates/cli/src/cli/commands/workflow.rs:271-352`, `crates/cli/src/cli/commands/workflow.rs:506-557`, `crates/cli/src/cli/commands/workflow.rs:653-670`). The merge implementation marks the source thread as merged and stores `merged_state`, `current_state`, and freshness after the merge (`crates/cli/src/cli/commands/merge/mod.rs:1096-1114`).

Native stack land should add the cascade:

1. Land the selected bottom/root thread into its target.
2. Mark that landed thread merged using the existing thread metadata pattern (`crates/cli/src/cli/commands/merge/mod.rs:1104-1114`).
3. For each direct child whose `parent_thread` was the landed thread:
   - set its effective upstream to the landed target tip;
   - update its parent relation according to the chosen metadata policy;
   - sync/replay it onto the new upstream.
4. Continue through descendants using the same parent-before-child sync executor.

Recommendation: `land` should auto-restack descendants by default after a successful bottom land. This makes "merge bottom, restack the rest" a first-class workflow instead of a recipe. The auto-restack should only mutate threads whose recorded parent chain depends on the landed thread, which keeps the behavior scoped to explicit dependencies. If a descendant conflicts, the landed bottom stays landed, the stack operation pauses at the conflicted child, and `continue` resumes the remaining descendants after resolution.

There are two metadata policies for direct children after their parent lands:

- **Collapse edge to target:** direct children drop `parent_thread` if their parent is now merged into `target_thread`; their `base_state` becomes the target tip. This is simple for linear stacks but loses the historical "was stacked on" relation unless the oplog records it.
- **Keep historical parent, mark effective upstream as target:** direct children retain `parent_thread` for audit/display, but sync treats a merged parent as resolved into the parent's target. This preserves provenance but requires status/review to distinguish active and historical parents.

Recommendation: keep `parent_thread` as the historical dependency and add explicit active/effective-upstream behavior in sync/status. This gives hosted review a truthful dependency graph even after the bottom lands. If that proves too subtle in implementation, collapse the edge but record a metadata oplog event.

## Emergent Surface, Not A New Stack Family

Do not add new mutating `stack` verbs. The accepted consolidation demotes `stack ready` into `ready --stack` and moves stack/workspace views into `status` (`docs/spikes/whole-cli-consolidation.md:90`, `docs/spikes/whole-cli-consolidation.md:103`, `docs/spikes/whole-cli-consolidation.md:117`). The current `stack` command implementation is also already read-only in spirit: its module describes stack display and readiness, and the CLI README calls the stack workflow read-only (`crates/cli/src/cli/commands/stack.rs:1-7`, `crates/cli/src/cli/cli_args/commands_stack.rs:1-7`, `crates/cli/README.md:10-27`).

Canonical user flow:

```text
heddle start feature-a
heddle start feature-b --on feature-a
heddle start feature-c --on feature-b

heddle status
heddle sync
heddle land feature-a
heddle continue
```

`status` should earn the minimal stack-aware view slot:

- current thread's chain from root to descendants;
- parent and child labels;
- stale/dirty flags relative to effective upstream;
- blocked conflict thread and resume command;
- hosted review identifiers when available.

This is not a new command family. It is the existing thread model rendered clearly.

## Hosted Review Sketch

A stack should appear as N reviewable thread units with recorded dependencies:

- each thread review shows `depends on <parent>` and `blocks <children>`;
- each review is anchored at the thread's `base_state` and `current_state`;
- the chain view is computed from the same `parent_thread` edges used locally;
- approvals remain pinned to source thread state, matching current hosted approval calls (`crates/client/src/grpc_hosted/user.rs:326-354`, `crates/cli/src/cli/commands/thread_approval.rs:128-184`);
- merge eligibility remains source/target based and can be extended to require parent reviews landed or approved first (`crates/client/src/grpc_hosted/user.rs:391-424`, `crates/cli/src/cli/commands/thread_approval.rs:266-330`);
- per-thread review payloads can use the existing local review service shape, which builds payloads for changed symbols consumed by CLI/web review surfaces (`crates/cli/src/cli/commands/review.rs:1-12`, `crates/cli/src/cli/commands/review.rs:39-51`, `crates/state_review/src/payload.rs:1-20`).

The local snapshot data already has the right projection shape for this: `ThreadSnapshot` exposes `thread`, `parent_thread`, `base_state`, `current_state`, `state`, and freshness (`crates/repo/src/stack_snapshot.rs:61-75`), and `RepositorySnapshot::capture` reads thread records and computes stacks (`crates/repo/src/stack_snapshot.rs:111-134`).

Weft/tapestry should render this as a dependency graph plus per-thread review pages, not as rebased SHA stacks. Heddle state ids and oplog entries make each restack an explicit provenance event instead of replacing a PR's apparent history with a new Git commit sequence.

## Git-Stack Comparison

| Operation | Git/ghstack/Graphite shape | Heddle native shape | Why Heddle is simpler/stronger |
|---|---|---|---|
| Restack | Rebase each branch/PR and force-push rewritten commits. | `sync` walks `parent_thread` edges and replays each thread onto its effective upstream. | Dependency is recorded in thread metadata, and semantic replay can absorb source-level additive churn that textual rebase often conflicts on. |
| Reorder | Interactive rebase plus PR retargeting. | Reparent thread edges, then `sync` from the affected root. | The operation changes explicit dependencies rather than reverse-engineering them from branch ancestry. |
| Drop middle | Remove a commit/branch from the stack and rebase descendants. | Mark/drop the middle thread, reparent children to the dropped thread's parent/effective upstream, then `sync`. | Descendants know why they need replay because their parent edge changed. |
| Split | Create new commits/branches and manually move diffs between PRs. | Use `start --on` to create the new dependent thread and move selected changes with existing thread/change surgery; then `sync`. | The resulting dependency is durable thread metadata, not a convention encoded in branch names. |
| Land bottom | Merge the bottom PR, then manually restack every dependent PR. | `land` the bottom thread; Heddle updates children and runs the same stack `sync` cascade. | Landing and restacking are one recorded workflow over explicit dependencies. |

## Phased Implementation Sketch

1. **Parent-aware metadata semantics**
   - Define effective upstream: active `parent_thread` tip if present, otherwise `target_thread` tip.
   - Add write-time cycle prevention for parent changes.
   - Decide whether local parent references remain thread names or move to stable ids.

2. **Public `start --on`**
   - Add public CLI args, mutually exclusive with `--from`.
   - Resolve parent, read parent tip, create child at that tip.
   - Persist `parent_thread`, `base_state`, and `base_root`.
   - Keep `fork` unchanged because it does not create managed thread records.

3. **Stack `sync` executor**
   - Reuse the read-only stack planner as the planning substrate.
   - Replay parent-before-child.
   - Persist each successful step before proceeding.
   - Use semantic merge as the default strategy once #467 lands.
   - Add stack-operation state so `continue` resumes descendants after a mid-stack conflict.
   - Emit per-thread oplog updates, and extend oplog metadata if #469 requires base/parent deltas.

4. **`land` cascade**
   - After landing a root/bottom thread, update direct children to the landed target/effective upstream.
   - Auto-restack descendants using the same `sync` executor.
   - Stop at first conflict and resume through `continue`.

5. **Status and ready surfaces**
   - Move canonical stack display into `status`.
   - Keep any legacy `stack` command read-only/advanced during migration.
   - Route stack readiness through `ready --stack` per consolidation.

6. **Hosted review projection**
   - Expose stack edges, current/base states, freshness, and approval status to review pages.
   - Render chain dependencies and per-thread review units.
   - Keep implementation details out of this spike; the local data model should be enough to drive the projection.

## Open Questions For Maintainer Sign-Off

1. Should `parent_thread` remain a thread-name edge locally, or should stacks introduce a stable thread-id edge even though existing stack readers consume names and `ThreadRecord::id` can diverge from `thread` (`crates/cli/src/cli/commands/thread_cmd.rs:1754-1759`)?
2. After a parent lands, should children keep `parent_thread` as a historical dependency or should the edge collapse to the parent's target with only oplog provenance preserving the old relation?
3. Should `land` always auto-restack descendants, or should it auto-restack only when all affected checkouts/worktrees are available and otherwise mark them dirty-for-sync?
4. In branched stacks, should `sync` stop the entire stack on the first conflict, or continue syncing independent sibling branches?
5. Is `OpRecord::ThreadUpdate { old_state, new_state }` enough for #469 restack provenance, or should Heddle add an explicit metadata/restack operation that records old/new base and parent changes (`crates/oplog/src/oplog/oplog_types.rs:63-72`)?
6. Should final integration target and active stack parent be separate fields long-term, or should `target_thread` temporarily point to the parent while stacked to minimize the first implementation delta?
