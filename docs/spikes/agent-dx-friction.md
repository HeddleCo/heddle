# Agent DX Friction: Git-overlay multi-thread + PR workflows

Status: **IN PROGRESS** (implementation ordered by what blocks dogfooding)
Date: 2026-07-09
Source: Dogfood of multi-agent review remediation (heddle threads + staging PR to GitHub)

## Problem

Parallel agent workflows that use `heddle start --path`, land/merge into a staging
thread, then open a GitHub PR currently hit structural friction:

1. Land can leave **Heddle ahead of Git** with recovery that mislabels local failures as remote.
2. Overlay bootstrap can invent a **parentless Heddle root** that export turns into an **orphan Git branch** (no merge-base with `main`).
3. N peer threads all preview as pure FF onto staging; landing one requires **manual refresh order**.
4. Refresh/merge can **drop +x / flatten symlinks** (usually via merge tree rewrite, not the leaf writer).
5. Rust threads default to **per-checkout multi-GB `target/`** (addressed: shared-target default-on for Rust solid/materialized).
6. Default **`status --output json` is the most expensive shape**, dual Sley walks, agent timeouts.

These are mostly **composition and defaults**, not missing engines.

## Capability classification

| Item | Today | Target |
|------|--------|--------|
| Land + Git checkpoint | Dual durable steps; partial failure leaves gap | One integration txn (or auto-undo) |
| Overlay first state | `ensure_current_state` may invent `parents=[]` | Lazy bind active Git tip; never orphan when Git tip exists |
| Peer fan-in | Manual serial refresh+land | **Shipped:** post-land auto-restack of same-target siblings; multi-`land --threads` still planned |
| Mode/symlink fidelity | Materialize OK if tree right; merge often wrong | Merge preserves FileMode/kind; e2e gates |
| Shared cargo target | Default-on for Rust solid/materialized | Default-on for Rust solid/materialized |
| Status shapes | Full JSON by default for agents | Probe/short shapes; one worktree walk |

## Decisions (locked for this spike)

1. **Priority:** ship **P0 blockers first** (orphan history + land/Git consistency),
   `cargo install --path` the result, then fan out P1/P2 on the new binary.
2. **No CRDT multi-parent fan-in.** Ordered pairwise merge semantics stay.
3. **Checkpoint remains a repair verb** after IntegrationTxn; not the second half of happy-path land.
4. **Probe status must not fake `verified: true`.** Skipped checks are `not_checked` / partial.
5. **Default shared-target is a deliberate behavior change** (alpha-acceptable; document in CHANGELOG).

## Work items and progress

### P0 — Blocks multi-agent + GitHub dogfood

#### P0-A. Stop orphan Git history (lazy tip bind)

| Field | Value |
|-------|--------|
| Status | **DONE** |
| Owner | agent wave 1 |
| Root cause | `ensure_current_state` invents parentless snapshot when `current_state()==None` while Git HEAD is a real commit; `git_import_guidance` always returns `None` so unimported preflight is a no-op; export mints Git commits with no parents. |
| Ideal fix | Lazy-map active Git tip into Heddle as first state; never create user-facing `parents=[]` roots when a Git tip exists; start base prefers mapped tip / default base. |
| Key files | `cli/.../snapshot.rs` (`ensure_current_state`), `repo/repository.rs` (`git_import_guidance`, `head`, `bootstrap_git_overlay`), `repo/repository_snapshot.rs` (parents), `git-projection/git_export.rs` (parent OIDs) |
| Acceptance | (1) Fresh `heddle init` on existing git repo + `start feature/x` does not create bootstrap root with empty parents when Git tip exists. (2) First export/write-through of that thread has merge-base with `main`/base tip. (3) Regression test. (4) No “Bootstrap git-overlay before starting …” as synthetic root in that path. |
| Progress | Single-tip lazy bind via `ingest::import_single_git_commit_into`; `ensure_current_state` binds active Git tip before inventing worktree roots; fails closed with `heddle adopt` recovery when tip exists but bind fails; regression `init_then_start_binds_git_tip_not_orphan_bootstrap`. |

#### P0-B. Land / Git tip consistency (atomic integrate slice)

| Field | Value |
|-------|--------|
| Status | **DONE** (dogfood slice; full IntegrationTxn journal deferred) |
| Owner | agent wave 1 |
| Root cause | Land: Heddle merge first, then separate checkpoint; failure → `land_checkpoint_partial_failure` without auto-undo. Local `NonFastForwardRef` mis-mapped to remote FF advice. |
| Ideal fix (full) | `IntegrationTxn` in core: preflight projected FF → apply Heddle → write-through → coalesce; auto-undo on Git failure; journal for crash. |
| Slice for dogfood (this pass) | (1) Fix advice: local write-through non-FF ≠ remote push rejection. (2) On land checkpoint failure, **auto-undo** the land integration batch (or fail closed before Heddle moves when dry-run proves non-FF). (3) Tests for advice mapping + partial-failure rollback. |
| Key files | `cli/.../workflow.rs` (`cmd_land`), `cli/.../advice.rs`, `cli/.../git_overlay_txn/mod.rs`, `core/save.rs`, `git-projection/git_core.rs`, `git-projection/git_sync.rs` |
| Acceptance | (1) Land Git failure does not leave durable Heddle tip advanced without Git (or auto-undo restores). (2) Local non-FF error text does not claim “remote branch”. (3) Unit/integration coverage. |
| Progress | (1) `NonFastForwardRef.remote_destination` splits local write-through vs push destination; local maps to `git_overlay_local_non_fast_forward` (no “Remote branch” title). (2) Land checkpoint failure auto-undos land-owned integration (+ squash collapse) via `undo_batches_quiet`; success → `land_checkpoint_rolled_back`. (3) Unit advice tests + `git_overlay_matrix_land_checkpoint_failure_auto_undoes_heddle_integration` (fault inject `git_checkpoint_before_write_through`). Residual: crash between Heddle integrate and Git write-through still needs IntegrationTxn journal; source-thread capture/sync not auto-undone. |

### P1 — Unblocks agent throughput after reinstall

#### P1-A. Default shared-target for Rust workspaces

| Field | Value |
|-------|--------|
| Status | **DONE** |
| Owner | agent wave 2 (post-reinstall) |
| Root cause | Opt-in + hidden flag; `try`/fanout force `shared_target: false`. |
| Fix | Default-on for solid/materialized when root `Cargo.toml` exists; `--no-shared-target` opt-out; loud warn if config cannot be written; wire try/fanout. |
| Key files | `worktree_cmd/shared_target.rs`, `cli_args/commands_args.rs`, `thread.rs`, `start_atomic.rs`, `try_cmd.rs`, `agent_cmd.rs` |
| Acceptance | Second+ Rust thread shares `.heddle/targets/<fp>` without flags; opt-out works; tests updated. |
| Progress | Default-on for solid/materialized when root `Cargo.toml` exists; `--no-shared-target` opt-out; try/agent fanout inherit default; loud warn when existing `.cargo/config.toml` blocks redirect; skip hydrating `target` when redirect active; tests updated. |

#### P1-B. Status shapes + kill dual Sley walk + agent probe

| Field | Value |
|-------|--------|
| Status | **DONE** |
| Owner | agent wave 2 (post-reinstall) |
| Root cause | Full JSON detail; two full Sley short-status walks; clean-path verification fan-out. |
| Fix | One Sley stream → changes + index plan; shapes probe/short/default/full; agent JSON default not Full; honest partial trust. |
| Key files | `core/status.rs`, `cli/.../status.rs`, `repository.rs` (`git_overlay_short_status`) |
| Acceptance | Single Sley short-status per status invocation; `status --output json` without `--full` is not Full thread walk; probe/short documents `not_checked` where applicable; profile shows dual walk gone. |
| Progress | `Repository::git_overlay_short_status` one Sley stream → worktree + index intent; `status()` loads both via `load_git_overlay_status_and_index_plan` (profile `git_index_ms=0`). CLI: `status --output json` defaults to `StatusDetail::DefaultText` (not Full); Full only with `--verbose`. Shapes documented on `StatusDetail` (ShortText/CompactMachine/DefaultText/Full); no separate `--shape`/`Probe` flag. Machine contract and skipped checks stay `not_checked` (never fake `verified`). Cached `heddle_worktree_is_clean` on dirty-git path. Tests: `single_short_status_stream_builds_worktree_and_index_plan`, `default_detail_skips_full_thread_walk_cost_and_keeps_not_checked`. |

### P2 — Correctness + multi-agent productization

#### P2-A. Peer fan-in (multi-land / post-land sibling refresh)

| Field | Value |
|-------|--------|
| Status | **DONE** (auto-sibling restack; multi-`land --threads` deferred) |
| Owner | agent wave 2/3 |
| Root cause | Pairwise FF previews; no multi-source integrate; freshness is target-tip only. |
| Fix | Productize serial fan-in loop and/or post-land auto-refresh of siblings with same `target_thread`. Optional `land --threads a,b,c`. |
| Key files | `workflow.rs`, `thread_cmd.rs` (`refresh_thread`), `merge/plan.rs`, `snapshot_metadata.rs` |
| Acceptance | Documented command path lands N disjoint peers without manual refresh order; or auto-restack siblings after land. |
| Progress | After successful land into target `T`, best-effort `refresh_thread` of other Active/Ready/Blocked/Draft threads with `target_thread=T` that are now Stale. Failures go to `siblings_restack_failed` + operator warnings; land is not undone. JSON/text: `siblings_restacked`. No new CRDT merge. Regression: `test_land_auto_restacks_stale_sibling_peers`. Optional `land --threads a,b,c` still deferred. |

#### P2-B. Merge mode / symlink fidelity

| Field | Value |
|-------|--------|
| Status | **DONE** |
| Owner | agent wave 2/3 |
| Root cause | Merge executor invents `executable: false` / file-only trees on conflict/content/rename paths; materialize then honest. |
| Fix | Preserve FileMode/kind through merge; e2e start --path + refresh fidelity tests. |
| Key files | `merge/src/tree_merge/executor.rs`, `repository_materialization.rs`, e2e tests |
| Acceptance | Executable + relative symlink round-trip through start --path and refresh that only touches another file. |
| Progress | FlatLeaf carries hash+EntryType+executable through rename/flat rebuild; `build_nested_tree` emits Blob(+x) and Symlink correctly; recursive content/conflict paths use union-of-+x policy. Unit tests cover no-rename preserve, content-merge union, conflict +x, rename rebuild, and nested rebuild. |

## Implementation order

```text
Wave 0  Write this spike; track status here
Wave 1  P0-A + P0-B in parallel (isolated heddle threads or sequential)
        cargo test targeted; cargo install --path crates/cli
Wave 2  Dogfood with new binary; then P1-A + P1-B in parallel
Wave 3  P2-A + P2-B
```

## Explicit non-goals (this spike)

- Full Bridge Mirror deletion / residual import capture (separate architecture track).
- Segmented oplog rewrite.
- Hosted weft/tapestry changes.
- CRDT collaboration fan-in.

## Code grounding (investigation summary)

### Orphan bootstrap

- `ensure_current_state` → `create_snapshot` when no current state (`cli/.../snapshot.rs`).
- Empty parents when `prev_head` is None (`repo/repository_snapshot.rs`).
- `git_import_guidance` → always `Ok(None)` on overlay (`repo/repository.rs`).
- Export parents from Heddle only (`git-projection/git_export.rs`).

### Land dual-write

- Land: merge with `git_commit=false`, then `create_git_checkpoint` (`cli/.../workflow.rs`).
- Partial failure advice (`cli/.../advice.rs` `land_checkpoint_partial_failure`).
- Non-FF on mirror branch (`git-projection/git_sync.rs` `ensure_commit_update_fast_forward`).
- Mislabel as remote (`advice.rs` `from_git_projection_error` → `git_overlay_remote_push_rejected`).

### Peer FF illusion

- `MergePlan::build` ancestry of two tips only (`core/merge/plan.rs`).
- Start sets `target_thread` to current HEAD (`cli/.../thread.rs`).
- Refresh is single-thread rebase onto target (`cli/.../thread_cmd.rs`).

### Mode/symlink

- Materialize restores mode from tree (`repo/repository_materialization.rs`).
- Merge drops mode on conflict/content paths (`merge/.../executor.rs`).

### Shared target

- `worktree_cmd/shared_target.rs`; flag `hide = true`; default false.

### Status cost

- Dual `stream_short_status` + Full detail for JSON — **fixed** via `git_overlay_short_status` + DefaultText JSON default.

## Test plan (overall)

- [x] P0-A: overlay init + start + export has merge-base with base branch
- [x] P0-B: inject Git checkpoint failure on land → no durable Heddle-only advance (or auto-undo)
- [x] P0-B: local NonFastForwardRef message is not remote-titled
- [x] P1-A: default share + opt-out
- [x] P1-B: one Sley walk; default JSON not Full; honest not_checked
- [x] P2-A: post-land auto-restack of same-target siblings (multi-`land --threads` deferred)
- [x] P2-B: executable + symlink fidelity (merge unit tests; flat rebuild + recursive)
- [ ] `cargo install --path crates/cli` smoke after P0
- [ ] Manual: start two agents, land both to staging, `gh pr create` works

## Changelog notes (draft)

When shipping:

- fix(overlay): bind active Git tip instead of inventing orphan bootstrap roots
- fix(land): do not leave Heddle ahead of Git on checkpoint failure; clarify local vs remote non-FF
- feat(land): auto-restack same-target sibling threads after successful land
- feat(start): default shared cargo target for Rust workspaces
- perf(status): single worktree walk; agent JSON uses DefaultText not Full

## Open questions

1. Should lazy tip bind import **full history** or **single tip** only? (Recommendation: single tip first; full adopt remains explicit.)
2. Land auto-undo: always, or only when land-owned capture+merge batch is cleanly reversible?
3. JSON status default change: break alpha clients now or add `status --shape` first? **Decided:** change default now (alpha); document shapes on `StatusDetail` rather than new `--shape` flag.

## Progress log

| Date | Note |
|------|------|
| 2026-07-09 | Spike opened from multi-agent dogfood investigation. P0 next. |
| 2026-07-09 | P0-B landed (`e33e5577`): land auto-undo + local vs remote non-FF advice. |
| 2026-07-09 | P0-A landed (`8af3b876`): lazy tip bind; no orphan bootstrap when Git tip exists. |
| 2026-07-09 | `cargo install --path crates/cli --force --locked` — heddle replaced in `~/.cargo/bin`. Wave 2 (P1/P2) next. |
| 2026-07-09 | P0-A done: lazy single-tip bind in `ensure_current_state`; no orphan Bootstrap root when Git tip exists. |
| 2026-07-09 | P0-B done (dogfood slice): local vs remote non-FF advice; land auto-undo on checkpoint failure. Full IntegrationTxn journal still residual. |
| 2026-07-09 | P2-B done: merge preserves +x (union) and symlink kind through recursive and rename/flat rebuild paths; unit tests in heddle-merge. |
| 2026-07-09 | P1-B done: single Sley short-status for status+index; agent JSON DefaultText; honest not_checked. |
| 2026-07-09 | P1-A done: default shared cargo target for Rust solid/materialized; `--no-shared-target` opt-out; try/fanout inherit; loud blocked-config warn. |
