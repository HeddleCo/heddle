# Agent DX friction: Git Overlay multi-thread workflows

Status: implemented

Date: 2026-07-09; reconciled with current `main` on 2026-07-15.

This spike records the invariants added for agents that work in parallel
materialized threads and land their results into a Git Overlay checkout. The
implementation keeps ordered pairwise merge semantics; it does not introduce a
multi-parent or CRDT fan-in operation.

## Shipped behavior

### Bind an existing Git tip before the first Heddle state

When `ensure_current_state` runs in a Git Overlay checkout with a commit-pointing
Git `HEAD`, it binds that commit through
`ingest::bind_single_git_commit_overlay`, records its Git checkpoint identity,
and points the matching Heddle thread at the mapped state. The binding stores
Git-to-Heddle mappings and reads mapped objects through the checkout's Git
object database instead of copying the tip into Heddle's native object store.
It creates a worktree bootstrap state only for an empty or unborn Git checkout.

If the Git tip exists but cannot be bound, the command returns
`git_overlay_tip_bind_failed` and recommends the explicit `heddle adopt`
recovery path. It does not fall back to a parentless bootstrap state.

Key implementation:

- `crates/cli/src/cli/commands/snapshot.rs`
- `crates/ingest/src/importer.rs`
- `crates/repo/src/repository.rs`

Regression: `init_then_start_binds_git_tip_not_orphan_bootstrap`.

Before a mutating command binds that tip, `log`, `show`, and
`query --attribution` construct an in-memory state graph from the reachable Git
commits. `diff HEAD` reads the Git-backed worktree status. These observe-only
paths do not create mappings, object files, state descriptors, or Heddle refs;
the in-memory graph preserves the reachable Git parent relationships.

Key implementation: `crates/ingest/src/overlay_history.rs`.

Regression:
`initialized_overlay_observe_commands_project_full_git_history_without_writes`.

### Keep land and Git checkpoint state consistent

Land writes `.heddle/incomplete-land.json` atomically before the first
land-owned collapse or integration mutation, advances it through integration
and rollback phases, and clears it only after Git write-through and oplog-batch
coalescing complete. A checkpoint error automatically undoes the land-owned
integration and any land-owned collapse batch. The marker records the pre-land
and expected Git OIDs plus the exact integration and collapse oplog batch IDs.
Recovery validates those identities before undoing them; a prepared marker
never infers ownership from whichever state or ref happens to differ at
recovery time.

`land` and `ready` recover a surviving marker; observe-only `status` reports the
pending recovery and its retry action without mutating it. Recovery verifies
the marker's target branch and the checkpoint intent's previous and published
OIDs, covering a crash after checkpoint publication but before marker removal
without mistaking an older mapping for this publication. Otherwise it rolls the
incomplete integration back. `rollback_started` / `rollback_complete` phases
make cleanup retries idempotent after an undo has already run. Every command
registered as both mutating and targeting the current repository in the command
catalog passes through this recovery check before dispatch.

Local and remote non-fast-forward failures remain distinct:
`NonFastForwardRef.remote_destination` selects either
`git_overlay_local_non_fast_forward` or remote push advice.

Key implementation:

- `crates/cli/src/cli/commands/workflow.rs`
- `crates/cli/src/cli/commands/undo.rs`
- `crates/cli/src/cli/commands/advice.rs`
- `crates/core/src/save.rs`
- `crates/git-projection/src/git_core.rs`

Regression:
`git_overlay_matrix_land_checkpoint_failure_auto_undoes_heddle_integration`.

### Land peers serially with one machine envelope

`heddle land --threads alpha,beta` lands peers in argument order against the
live target tip and stops at the first blocked peer. JSON output is one
`land_batch` object containing the requested order, successful prefix, and one
result for each attempted peer. A blocked peer retains the structured primary
command and recovery-command list from its error; the batch promotes that
primary command as its recommended action. The batch Git head and verification
state are read from the resolved landing target repository.

The first peer keeps its ordinary Heddle-derived Git parents. For each later
peer with an unmapped write-through state, the multi-peer checkpoint path asks
Git projection to use the current checkout branch tip as the exported commit
parent. This makes each later checkpoint a fast-forward of the prior peer's
checkpoint while preserving the durable state-to-Git mapping for states
already exported. Ordinary `commit` and single-peer `land` do not enable this
parent override.

Regressions:

- `git_overlay_matrix_multi_peer_land_fast_forwards_git_tip`
- `git_overlay_matrix_land_threads_flag_lands_peers_in_order`

### Restack same-target siblings after land

After a successful land, sibling threads in draft, active, ready, or blocked
state with the same `target_thread` are freshness-checked and stale siblings
are refreshed in deterministic thread-id order. This is best effort: failures
appear in `siblings_restack_failed` and operator warnings, but do not undo the
successful land. Human single-peer and batch output print the warnings and each
recorded restack failure.

Key implementation: `crates/cli/src/cli/commands/workflow.rs`.

### Share Rust build output by default

For a solid or materialized thread rooted in a Rust workspace, `start` writes a
thread-local Cargo configuration that points to the repository's shared target
directory. `--no-shared-target` opts out. Agent fan-out and `try` pass the same
default through, and a pre-existing `.cargo/config.toml` or legacy
`.cargo/config` that prevents the redirect produces a warning.

Only the generated `/.cargo/config.toml` is locally excluded from status;
tracked `.cargo` content remains visible. Active shared-target setup skips
hydrating a copied `target` directory.

The generated Cargo config and the checkout-local Git exclude file are one
atomic setup unit: failure while updating either file restores the exact prior
bytes (or prior absence), and a later `start` transaction rollback restores the
same pair.

Key implementation:

- `crates/cli/src/cli/commands/worktree_cmd/shared_target.rs`
- `crates/cli/src/cli/commands/thread.rs`
- `crates/cli/src/cli/commands/start_atomic.rs`

### Use one Sley stream for ordinary status

`Repository::git_overlay_short_status` derives both worktree changes and Git
index intent from one Sley short-status stream. Core status consumes both
results together, so `git_index_ms` represents no second scan.

Plain `status --output json` and verbose status use the all-thread
`StatusDetail::Full` shape so required machine fields remain truthful. Compact
and short machine output keep their smaller shape, and skipped verification
remains `not_checked`.

Key implementation:

- `crates/repo/src/repository.rs`
- `crates/core/src/status.rs`
- `crates/cli/src/cli/commands/status.rs`

### Preserve merge leaf kind and executable mode

The rename-aware flattened merge tree carries content hash, `EntryType`, and
the executable bit. Tree reconstruction emits blobs and symlinks with their
preserved kind. Content and conflict paths use the executable union policy, so
an executable input is not silently rewritten as mode `100644`.

Key implementation:

- `crates/merge/src/tree_merge/rename_matcher.rs`
- `crates/merge/src/tree_merge/executor.rs`

The `heddle-merge` tree-merge tests cover untouched leaves, content merges,
conflict markers, rename rebuilds, and nested-tree reconstruction.

## Deliberate limits

- Peer fan-in is serial pairwise land, not a CRDT merge.
- Lazy tip binding imports one commit identity; full-history adoption remains
  the explicit `heddle adopt` workflow.
- Source-thread capture and refresh performed before land are not part of the
  checkpoint rollback batch.
- Sibling restack failures are reported rather than made transactional with the
  completed land.
