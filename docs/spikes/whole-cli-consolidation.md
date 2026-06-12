# Whole-CLI Consolidation Decision

Status: **ACCEPTED** (maintainer sign-off 2026-06-03 — see Maintainer Decisions below). Extends the accepted `#461` save/land core and the filed `#466`/`#467` decisions to the **entire** top-level surface.

Directive: AGGRESSIVE. The default verdict for every verb is remove / merge / demote. A verb earns a distinct top-level canonical slot only if it is the single irreducible path to a capability nothing else covers.

## Headline

**85 true top-level verbs -> 28 canonical (+ ~24 demoted to advanced/admin/help).**

(The "~115" figure in the brief counts deep subcommand leaves; the `Commands` enum in `crates/cli/src/cli/cli_args/commands_main.rs` defines 85 top-level variants, several `#[cfg(feature=...)]`-gated. All 85 are accounted for below.)

## Maintainer decisions (2026-06-03)

Direction **ACCEPTED**. Resolutions to the open questions (these override the proposed dispositions where they differ):

1. **`switch` — KEEP canonical.** One git-muscle-memory porcelain is worth keeping to ease git users in. (`checkout`/`goto` still fold into it.)
2. **`blame` — REMOVE from the canonical surface.** Per-line attribution data is surfaced via `query` instead; if `query` lacks an attribution/per-line mode today, add one later. `blame` is not a standalone top-level verb.
3. **`auth` and `presence` — BOTH KEEP canonical, as SEPARATE verbs.** The OSS CLI is the *client*: `auth` manages the locally-owned biscuit; `presence` broadcasts agent actions to listeners — distinct concerns, not one cluster. **No `admin` namespace in heddle** (the admin surface lives in weft). `support` (hosted staff grants) is an admin concern → **removed from the heddle surface**.
4. **Two daemons — KEEP SEPARATE.** The FUSE-mount daemon and the agent `serve`/`status`/`stop` control plane stay as separate sub-namespaces (correct long-term; do NOT unify under one `daemon <which>`).
5. **`discuss` — KEEP distinct canonical.** Not folded under `context`; it has its own dedup/anchor lifecycle.
6. **`marker` — MERGE into the `thread`/ref naming surface** (a name-pointing-at-a-state), not a standalone advanced primitive.

Net effect on the canonical set vs the proposal: `presence` and `discuss` are canonical; `blame`, `support`, and any `admin` namespace are dropped from heddle; `marker` folds into `thread`; `switch` stays. Implementation proceeds in the phased plan below; `#464` is the save/land (phase-0) slice already in flight.

## Completeness check

All 85 top-level `Commands` variants are covered by the 11 bucket analyses. The buckets' larger before-counts (summing ~142) double-count subcommand leaves and cross-bucket seed verbs (e.g. `fork`, `stack`, `conflict`, `monitor` appear in multiple buckets). Reconciling to the true top-level enum yields 85 -> 28. No top-level verb is unaddressed. Gaps noted: `Publish`/`Serve`/`Stop`/`Done`/`End`/`Segment`/`List`/`Spawn`/`Explain`/`Warm`/`Inspect`(maintenance) are NOT top-level verbs — they are subcommand leaves of agent/actor/session/store/daemon, correctly excluded.

## The minimal canonical surface (28)

Grouped by family. Each survives because it is the one irreducible path; justification cites the bucket analyses.

**Onboarding / repo bootstrap (3)** — `init`, `adopt`, `clone`. Each is repo-type-aware in a single handler (the variant-merge already happened inside). `init` is the only verb that makes `.heddle/` from nothing (`init.rs:52`); `adopt` is the only verb that bootstraps AND imports git history (`adopt.rs:52`); `clone` is the only verb that creates a repo from a remote you don't have (`clone.rs:147`).

**Inspect / read (7)** — `status`, `diff`, `log`, `show`, `blame`, `query`, `watch`. `status` = working-tree + next-action (`commands_main.rs:62`); `diff` = what changed (absorbs `compare`); `log` = history walk; `show` = one object (absorbs `inspect`); `blame` = per-line provenance (`blame.rs`); `query` = structured oplog filter (`query.rs`); `watch` = live oplog tail (`watch.rs`). `blame` is the marginal survivor (see open questions).

**Save / land core — the #461 accepted baseline (4)** — `commit`, `ready`, `land`, `sync`. Untouched. `commit -m` is the canonical repo-type-aware save (`git_compat.rs:113`); `ready` is the read-only verdict gate (`ready_cmd.rs:49`); `land` is the capture->refresh->land->checkpoint composite (renamed from `ship` per #461/#464, `workflow.rs:188`); `sync` is the native refresh-onto-target (`operator_loop.rs:41`).

**Lanes / isolation / execution (4)** — `start`, `switch`, `try`, `run`. `start` is the one isolated-checkout path (#466) and absorbs `delegate`/`fork`; `switch` is the single git-compat survivor for moving the checkout (absorbs `goto`, `checkout`); `try` is the ephemeral-sandbox-with-rollback (absorbs `attempt` via `--parallel`); `run` is exec-in-existing-thread (`run_cmd.rs`).

**Recovery / history (4)** — `undo`, `resolve`, `continue`, `abort`. `undo` is the oplog safety net (absorbs `redo` via `--redo`, `undo.rs`); `resolve` is the conflict write+read surface (absorbs `conflict`); `continue`/`abort` are operation-agnostic resume/cancel for merge/rebase/bisect (`operator_core.rs:203/264`) — the invariant being no integration verb defines its own `--continue`/`--abort`.

**Remote (3)** — `push`, `pull`, `remote`. Each folds git-overlay + native + hosted into one repo-type-detecting handler. `push` (`remote/mod.rs`) already routes git-overlay through `GitBridge::push` (`remote/mod.rs:229`), making `bridge git push` redundant.

**Collaboration / knowledge (3)** — `context`, `review`, `redact`. `context` is heddle's irreducible differentiator (durable symbol-anchored annotations, absorbs `discuss`); `review` is the composite review payload (`review.rs`); `redact` is the content-honesty primitive (absorbs `purge` as `redact purge`).

**Misc canonical (1)** — `clean` (the only verb that deletes untracked worktree files, native force/dry-run contract, `clean.rs:21`).

**Diagnostics / meta (5)** — `doctor`, `verify`, `help`, `shell`, `integration`, `auth`. (`doctor` is the single health umbrella absorbing `diagnose`+`schemas`+`fsck`-render; `verify` is the only nonzero-exit gate; `help` owns curated/topic disclosure; `shell` is the single shell-integration namespace absorbing `completion`; `integration` wires agent harnesses absorbing `hook`; `auth` is the hosted-credentials entry absorbing `support`/`presence`.)

**Surviving namespaces (the advanced umbrellas, count toward 28 as group nouns):** `thread`, `bridge`, `agent`, `maintenance`. These are the demotion targets — they keep as single namespaces while their leaves are pruned/folded.

## Per-family reduction matrix (cited)

| Family | Before (top-level) | After canonical | Demoted / merged (-> target) | Key citation |
|---|---|---|---|---|
| Save & snapshot | commit, capture, checkpoint, ready | commit, ready | capture->advanced, checkpoint->advanced (folded into commit) | #461 spike; `snapshot.rs:393` all funnel to create_snapshot |
| Land & rewrite-history | ship, merge, resolve, continue, abort, rebase, cherry-pick, revert, collapse, undo, redo | land, undo, resolve, continue, abort | ship->land; merge/rebase/cherry-pick/revert/collapse->advanced; redo->undo --redo; conflict->resolve | `workflow.rs:188`; per-verb --continue/--abort -> top-level continue/abort |
| Thread & isolation | start, fork, branch, switch, checkout, goto, workspace, stack, try, attempt, run, delegate (+thread group) | start, switch, try, run, thread(group) | branch->thread; checkout/goto->switch; workspace/stack->status; attempt->try; delegate/fork->start | `main.rs:512` switch|checkout shared arm; `git_compat.rs:1294` switch->cmd_switch_state_checkout |
| Remote & publish | push, pull, fetch, clone, remote, presence | push, pull, clone, remote | fetch->advanced; presence->agent | `remote/mod.rs:229` push->bridge.push |
| Inspect & diff | log, show, inspect, diff, compare, status, blame, retro, query, conflict, schemas, diagnose, doctor, verify, semantic | log, show, diff, status, blame, query, doctor, verify | inspect->show; compare->diff; diagnose->doctor; schemas/verify-render->doctor; retro/semantic->query; conflict->resolve | `main.rs:293` diagnose==doctor None |
| Recovery loop | continue, abort, resolve, conflict, goto, switch, checkout, transaction | continue, abort, resolve | conflict->resolve; goto->switch; checkout->switch alias; transaction stays hidden | `operator_core.rs:203/264` |
| Agent & harness | try, attempt, run, delegate, agent, actor, session, presence, monitor, watch, daemon, harness bridge, store warm, fork | try, run, agent(group), watch | attempt->try; delegate/fork->start; actor/session/presence->agent; monitor->maintenance; daemon->admin; harness bridge removed; store warm->maintenance | `actor_cmd.rs` vs `agent_cmd.rs` share AgentRegistry |
| Bridge & init | init, adopt, import(alias), clone, git-overlay, index, store, bridge(+10 leaves) | init, adopt, clone, bridge(group) | git-overlay->help; index/store->maintenance; bridge git push/pull/import->push/pull/adopt | `commands_main.rs:605` index hidden alias |
| Collaboration & context | context, discuss, review, marker, redact, purge | context, review, redact | discuss->context; marker->thread; purge->redact purge | `commands_redact.rs:1` redact owns purge lifecycle |
| Health & maintenance | doctor, diagnose, verify, fsck, gc, clean, purge, maintenance, store, bisect, index, monitor | doctor, verify, clean, maintenance(group) | diagnose->doctor; gc/index/monitor->maintenance(hidden aliases deleted); fsck->maintenance; store->maintenance warm; bisect REMOVED (stub); purge->redact | `bisect.rs:56` writes "{}\n", no binary search |
| Infra & meta | help, version, commands, schemas, completion, shell, semantic, integration, hook, transaction, daemon, agent-ctl, monitor, support, auth, harness bridge, presence | help, shell, integration, auth, doctor, verify | version->--version; commands->help --output json; schemas->doctor schemas; completion->shell; hook->integration; support/presence->auth; daemon->admin; transaction/harness bridge hidden | `commands_main.rs:613` monitor hidden alias |

## The advanced namespace (demoted plumbing — reachable, off everyday help)

- `commit` porcelain hides: **capture**, **checkpoint** (save savepoint primitives, `snapshot.rs:393`).
- `land` porcelain hides: **merge**, **rebase**, **cherry-pick**, **revert**, **collapse** (manual integration / history-surgery primitives). Strip their per-verb `--continue`/`--abort`/`--preview` flags; the operation-agnostic `continue`/`abort` are the only resume/cancel surface (close-the-class invariant).
- **fetch** (download-half of pull), **transaction** (already `hide=true`, unfinished).
- `thread` umbrella: canonical leaves `list`/`rename`/`drop`; advanced leaves `move`/`cleanup`/`create`/`promote`/`approval`(folded 4->1). `branch` git-mimicry demotes here.
- `maintenance` admin umbrella: `inspect`/`run`/`gc`/`index`/`monitor`/`fsck`/`warm`. Hidden top-level aliases `gc`/`index`/`monitor` DELETED (no-backcompat).
- `bridge` git-interop umbrella: `git status`/`export`/`reconcile`/`ingest`(feature)/`reason`(feature). `bridge git push`/`pull`/`import`/`init`/`sync` removed (duplicates of push/pull/adopt).
- `agent` orchestration umbrella: reservation API (`reserve`/`heartbeat`/`ready`/`release`) + `agent actor`/`agent session`/`agent presence`/`agent serve`. Unifies the 4 registry faces.
- `auth`/admin: **support**, **presence** (client-gated hosted surfaces). **daemon** (FUSE control-plane) under admin.
- `doctor` family: **schemas** introspection. `shell completion`. `integration hook`.
- Query presets: **retro**, **semantic hot**.
- **stash** kept as a single advanced git-compat namespace; **marker** demoted advanced.

## Cross-family merges (the high-value cuts)

1. **Git-mimicry collapse (lens d).** branch, switch, checkout, goto, rebase, cherry-pick, gc, fetch, bisect, version exist to look like git. Native paths (start/sync/commit/land) own the behavior. Net: ~9 top-level verbs leave.
2. **Diagnostic umbrella (lens e).** diagnose==doctor (identical `cmd_diagnose`, `main.rs:293`); schemas/fsck-render fold under doctor; verify survives only for its nonzero-exit gate.
3. **Introspection trio.** commands/schemas/completion -> help/doctor/shell.
4. **Agent-registry unification (structural).** agent/actor/session/presence = 4 faces on one `.heddle/agents` AgentRegistry -> one `agent` namespace. Removes 3 top-level groups.
5. **Status/view collapse.** status/workspace/stack/inspect/thread-show/current/resolve -> status+show.
6. **Sandbox/orchestration collapse.** try/attempt/delegate/fork -> try+start+run.
7. **Remote-dup collapse.** bridge git push/pull/import -> push/pull/adopt.
8. **Conflict-surface collapse.** conflict+resolve+scattered --abort/--list -> resolve + continue/abort.
9. **Redact lifecycle.** purge -> redact purge; discuss -> context discuss.
10. **Maintenance/admin umbrella.** gc/index/monitor/store-warm/fsck/daemon -> maintenance + admin.

## Migration notes (pre-1.0, NO compat shims)

Per the standing no-backcompat stance: delete + replace, no aliases/flags/phased deprecation unless explicitly retained for muscle-memory.

- **Hard removals (no alias):** bisect (stub), harness bridge (internal), the hidden gc/index/monitor compat aliases, bridge git push/pull/import/init/sync, version (->--version), commands, git-overlay, store group, completion group.
- **Renames:** ship -> land (#461/#464 in flight).
- **Merges that change invocation:** `attempt X` -> `try --parallel N X`; `delegate a b c` -> `start --task a --task b --task c`; `fork --name n --from s` -> `start n --from s`; `goto S` / `checkout S` -> `switch S`; `compare a b` -> `diff a b`; `redo` -> `undo --redo`; `conflict list` -> `resolve --list`; `purge apply` -> `redact purge apply`; `workspace` -> `status --all`; `stack ready` -> `ready --stack`.
- **Kept as single git-compat survivor:** `switch` (thread + state, refuses -c -> start --path). Max-aggression option demotes it too (see open questions).
- **Catalog/help:** rebuild `command_catalog.rs` tiers so the 28 canonical verbs are the everyday/advanced surface; everything else is `help advanced` topic pages only. Update `doctor docs`/`doctor schemas` drift checkers against the new surface (they are CI gates).

## Phased implementation plan

**Phase 0 — settled baseline (in flight).** #461 (ship->land, capture/checkpoint demote), #466 (start --path; demote thread create/promote), #467 (semantic default, drop --semantic). Land these first; they anchor the save/land + isolation cores.

**Phase 1 — free deletions (no capability loss, mostly mechanical).** Delete bisect (stub), harness bridge, hidden gc/index/monitor aliases, bridge git push/pull/import/init/sync, version->--version, commands->help, git-overlay->help. Each is a provably-redundant or non-functional removal citable to file:line. Smallest blast radius; do first to shrink the surface and the catalog.

**Phase 2 — synonym/router collapses.** diagnose->doctor, checkout+goto->switch, inspect->show, compare->diff, redo->undo, completion->shell, schemas->doctor schemas. These share handlers already; the work is enum/dispatch cleanup + catalog.

**Phase 3 — namespace unifications (structural).** Agent-registry: fold actor/session/presence under `agent`. Maintenance: fold fsck/store-warm under `maintenance`, move daemon under admin. Bridge: prune leaves. Thread: prune leaves (branch->thread, refresh->sync, absorb->land, resolve/show/current->status/inspect, approvals 4->1). These touch real handlers — dispatch at `xhigh` (security/registry-adjacent, multi-system).

**Phase 4 — flag/lifecycle merges.** attempt->try --parallel, delegate->start --task, fork->start --from, conflict->resolve, purge->redact purge, discuss->context discuss, workspace/stack->status/ready. Each needs the absorbing verb's args extended; verify conformance (the absorbed behavior must be reachable identically).

**Phase 5 — close-the-class invariants.** Add a conformance check that (a) no integration verb defines its own `--continue`/`--abort` (continue/abort are the only resume/cancel surface), and (b) every `next_action` breadcrumb names a canonical verb (commit/ready/land/sync/status/...), never a demoted one. Wire into `doctor docs` so regressions fail CI.

## Citations index (load-bearing, verified)

- `crates/cli/src/main.rs:512` — `Commands::Switch(args) | Commands::Checkout(args)` share one arm (checkout removal is free).
- `crates/cli/src/main.rs:293-299` — `Doctor{None}` calls `cmd_diagnose` with identical args (diagnose==doctor).
- `crates/cli/src/cli/commands/git_compat.rs:1294` — `switch <state>` delegates to `cmd_switch_state_checkout` (goto->switch).
- `crates/cli/src/cli/commands/remote/mod.rs:229` — top-level push routes git-overlay through `bridge.push` (bridge git push redundant).
- `crates/cli/src/cli/commands/bisect.rs:56` — `start` writes `"{}\n"`, good/bad echo only (no binary search; remove).
- `crates/cli/src/cli/commands/actor_cmd.rs` + `agent_cmd.rs` — share `.heddle/agents` AgentRegistry (unify under agent).
- `crates/cli/src/cli/cli_args/commands_main.rs:589/605/613` — gc/index/monitor are hidden compat aliases for maintenance (delete).
