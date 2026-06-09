# Heddle Improvement Plan

**Tracking document for the June 2026 improvement reconnaissance.**

Last updated: 2026-06-09

---

## 1. Executive summary

Reconnaissance (2026-06-08) compared heddle's former third-party git-library substrate against **[sley](https://github.com/HeddleCo/sley)** — HeddleCo's conformance-tested, pure-Rust Git library. On representative object-read workloads (`cat-file`, `rev-parse`, `rev-list`, `ls-tree`), sley is **~1.25× faster than system git** on the sley `scripts/bench-vs-git.sh` harness. Byte-exact framing/hash parity is proven; the swap is a dep-pin + adapter pattern, not a fidelity gamble.

**Decision (shipped):** heddle now runs on sley only ([epic #594](https://github.com/HeddleCo/heddle/issues/594), P4 [#598](https://github.com/HeddleCo/heddle/issues/598)). The ~277-transitive-dep third-party git-library subtree is gone from the CLI; heddle owns the substrate (SHA-256-ready, no third-party git-library churn), and the de-lossy epic ([#564](https://github.com/HeddleCo/heddle/issues/564)) builds on a native byte-exact object sink.

**Tier 1 substrate swap:** P0–P4 are **complete**. `crates/git-substrate` (`heddle-git-substrate`) is sley-only — `ObjectId`, framing/hash, read/write, refs, index, worktree, and transport all go through sley. **P2** refs/index/worktree and remaining de-lossy work continue on this foundation.

---

## 2. Project context

### Workspace

**21 crates** (post–workspace-split OSS tree):

| Crate | Role |
|---|---|
| `cli` | `heddle` binary, command dispatch, git bridge (today) |
| `cli-shared` / `cli-macro-poc` | Shared CLI helpers / macro experiments |
| `client` / `grpc` / `proto` | Transport, RPC, message surface |
| `crypto` | Signing primitives |
| `daemon` | Local agent kernel (grpc local impl) |
| `devtools` | Repo asserters (`check-no-silent-default-tree-load`, etc.) |
| **`git-substrate`** | **Sley-backed git substrate (#595)** |
| `ingest` | Deep git import / state writer |
| `merge` / `semantic` | Merge + semantic analysis |
| `mount` | FUSE mount daemon |
| `objects` | Object model, store, state hash |
| `oplog` | Undo/redo log |
| `refs` | Threads, markers, packed refs |
| `repo` | Repository operations, git worktree status |
| `review` / `state_review` | Review workflows |
| `runtime-bridge` | Runtime integration |
| `weft-client-shim` | Hosted client shim |

Hosted server (`weft`) and web (`tapestry`) live in sibling repos.

### Goals driving this plan

1. **Own the git substrate** — sley-only via `git-substrate`; depend on local `../sley` during dev (pin git rev before publish); single adapter crate for all Git-format I/O.
2. **Byte-exact git interop** — de-lossy reconstruction from heddle state; eliminate the adopt mirror (~80–90% of adopt time).
3. **Agent/CI-safe git overlay** — no `git` on `PATH` for product paths (`git_replacement_matrix`, `realworld_git`).
4. **Architecture depth** — extract the 4k-line git bridge from `cli` into shared crates ([ADR 0014](https://github.com/HeddleCo/heddle/blob/main/docs/adr/0014-command-surface-before-collaboration-expansion.md)); deepen command surface before collaboration expansion.
5. **Measurable gates** — every substrate phase ends green on acceptance suites + asserters before the next phase lands.

### Related epics

| Epic | Link | Relationship |
|---|---|---|
| sley substrate swap | [#594](https://github.com/HeddleCo/heddle/issues/594) | **Tier 1 — complete (P0–P4)** |
| De-lossy git fidelity | [#564](https://github.com/HeddleCo/heddle/issues/564) | Tier 3; P1+ sley sink subsumes framing/hash |
| Command surface (ADR 0014) | [ADR 0014](docs/adr/0014-command-surface-before-collaboration-expansion.md) | Tier 2; bridge extraction prerequisite |
| Whole-CLI consolidation | [spike](docs/spikes/whole-cli-consolidation.md) | Deferred until command surface stable |
| CLI dep reduction | [audit](docs/CLI_DEP_AUDIT_2026-05-12.md) | Tier 4; largest win (sley swap) landed |

---

## 3. Improvement tiers (reconnaissance)

### Tier 1 — Git substrate swap (sley) · **COMPLETE (P0–P4)**

Sley now backs `cli`, `repo`, `ingest`, and `objects` (~45 files, ~14 capability groups; concentrated in `cli/src/bridge/git_core.rs`). Local `../sley` workspace path deps (`sley-core`, `sley-formats`, `sley-odb`, `sley-refs`, `sley-rev`, `sley-remote`, `sley-transport`, …); **all** Git-format translation lives in `heddle-git-substrate` (`object`, `repo`, `write`, `transport`, …).

| Item | Issue | Key paths | Effort | Notes |
|---|---|---|---|---|
| **P0: dep + object-read/hashing** | [#595](https://github.com/HeddleCo/heddle/issues/595) | `crates/git-substrate/`, `ingest/src/git_walk.rs`, `cli/src/bridge/git_import.rs`, `cli/src/bridge/git_reconstruct.rs`, `repo/src/git_worktree_status.rs` | **3–5 d** | **Done** — read adapter + `ObjectId` sweep on `git_substrate::ObjectId` |
| **P1: object/tree write + serialization** | [#596](https://github.com/HeddleCo/heddle/issues/596) | `cli/src/bridge/git_export.rs`, `git_reconstruct.rs`, `git_notes.rs`, `git-substrate/src/write.rs` | **5–7 d** | **Done** — sley sink for blob/tree/commit write; `build_commit_content` kept authoritative |
| **P2: refs + index + worktree/checkout** | [#597](https://github.com/HeddleCo/heddle/issues/597) | `cli/src/bridge/git_core.rs:1335-1558`, `repo/src/git_worktree_status.rs` | **1–2 wk** | Adapter-needed: sley worktree free fns over `git_dir` |
| **P3: transport (fetch/push/clone)** | [#577](https://github.com/HeddleCo/heddle/issues/577) | `cli/src/bridge/git_core.rs:3624-3799` (receive-pack hand-roll) | **3–5 d** | Wire `sley-remote`; pre-gate: audit remotes for v1+`deepen` |
| **P4: sley-only finalize** | [#598](https://github.com/HeddleCo/heddle/issues/598) | Workspace `Cargo.toml`, `git-substrate` | **3–5 d** | **Done** — zero third-party git-library deps; sley-only reconstruction |

**Epic:** [#594](https://github.com/HeddleCo/heddle/issues/594) · Spike: `.heddleco-orchestrator/spikes/gix-to-sley-substrate.md` §1–§6 (historical)

**Cumulative impact (landed):** ~277 transitive deps removed from the former CLI git-library subtree; adopt mirror phase → ~0 after P1+#568; compile/link time and binary size drop materially.

---

### Tier 2 — Architecture decomposition (ADR 0014)

Deepen modules before collaboration expansion. The git bridge (~4025 lines in `git_core.rs` alone) lives inside `cli` but is consumed by `repo` and `ingest` — shallow coupling, hard to test, blocks compile-scope reduction.

| Item | Issue / ADR | Key paths | Effort | Notes |
|---|---|---|---|---|
| Extract git bridge to shared crate | ADR [0014](docs/adr/0014-command-surface-before-collaboration-expansion.md) | `crates/cli/src/bridge/*` → new `crates/git-bridge/` (proposed) | **1–2 wk** | Move `git_core`, `git_import`, `git_export`, `git_sync`, `git_reconstruct`; `cli` becomes thin dispatch |
| Wire `ingest` + `repo` through `git-substrate` + bridge crate | — | `crates/ingest/`, `crates/repo/src/git_worktree_status.rs` | **3–5 d** | Depends on P0 adapter stable |
| Command catalog / schema locality | ADR 0014 | `crates/cli/src/cli/commands/`, `command_catalog.rs`, `schemas.rs` | **1 wk** | Collaboration writes need mutating metadata + idempotency keys before inbox/discuss expansion |
| Daemon carve-out (CLI dep audit Tier 1) | [audit](docs/CLI_DEP_AUDIT_2026-05-12.md) | `crates/daemon/` (exists), `crates/cli/Cargo.toml` | **3–4 h** | Drop `cli → server` kitchen-sink path (~150 transitive deps) |

---

### Tier 3 — De-lossy epic alignment with sley

Epic [#564](https://github.com/HeddleCo/heddle/issues/564): reconstruct byte-identical git objects from heddle state; drop the internal git mirror. Substrate swap **resequenced** steps 4–5 onto the sley sink (now the only write path).

| Step | Issue | Key paths | Effort | Status / sequencing |
|---|---|---|---|---|
| 1: model + format bump + backfill | [#565](https://github.com/HeddleCo/heddle/issues/565) | `objects/src/object/state_core.rs`, `cli/src/bridge/git_import.rs` | — | **Merged** |
| 1b: mirror-backed backfill migration | [#570](https://github.com/HeddleCo/heddle/issues/570) / PR [#587](https://github.com/HeddleCo/heddle/pull/587) | `git_import.rs`, backfill command | **3–5 d** | **Blocker for #595 dispatch** |
| 2: byte-exact serializers + conformance | [#566](https://github.com/HeddleCo/heddle/issues/566) | `git_reconstruct.rs`, `cli/tests/commit_conformance.rs` | **1 wk** | gpgsig pre-signature bytes load-bearing |
| 3: export reconstruct-from-state | [#567](https://github.com/HeddleCo/heddle/issues/567) | `cli/src/bridge/git_export.rs` | — | **Merged** (PR #591) |
| 4: mirror elimination | [#568](https://github.com/HeddleCo/heddle/issues/568) | `git_import.rs:508-567` (`copy_reachable_objects`/`init_mirror`) | **3–5 d** | **Blocked by P1 #596** |
| Tags: first-class CA storage | [#575](https://github.com/HeddleCo/heddle/issues/575) | `ingest/src/git_walk.rs`, `ingest/src/state_writer.rs`, markers | **1 wk** | **Blocked by P1 #596**; replaces sidecar `marker-tags/*.bin` |
| Principal → `Vec<u8>` identities | [#593](https://github.com/HeddleCo/heddle/issues/593) | `objects/src/object/state_attribution.rs`, ~51 read-sites | **1 wk** | Last fidelity gap before full mirror drop; serialize vs #595/#587 |
| Signed-fidelity CI gate | [#562](https://github.com/HeddleCo/heddle/issues/562) | `cli/tests/roundtrip_fidelity.rs` | **2–3 d** | Closes #533 signed-object untested gap |

---

### Tier 4 — Dependency polish, benchmarks, CI gates

Lower-leverage items that land independently or after Tier 1 substrate path is clear.

| Item | Reference | Key paths | Effort | Notes |
|---|---|---|---|---|
| Hand-write JSON schemas, drop `schemars` | [CLI audit Tier 3](docs/CLI_DEP_AUDIT_2026-05-12.md) | `cli/src/cli/commands/schemas.rs` | **½ d** | ~50 transitive deps |
| Replace `chrono` with std + RFC3339 helper | [CLI audit Tier 4](docs/CLI_DEP_AUDIT_2026-05-12.md) | workspace-wide timestamp sites | **½ d** | ~40 transitive deps |
| Feature-gate `notify`, `clap_complete`, etc. | [CLI audit Tier 5](docs/CLI_DEP_AUDIT_2026-05-12.md) | `crates/cli/Cargo.toml` | **2 h** | ~30 transitive deps |
| Light `tracing-subscriber` | [CLI audit Tier 6](docs/CLI_DEP_AUDIT_2026-05-12.md) | `crates/cli/` | **2 h** | CLI defaults WARN+ |
| Streaming pack adopt perf | [#555](https://github.com/HeddleCo/heddle/issues/555) | adopt path | — | **Closed**; mirror drop supersedes pack-the-mirror (#561) |
| Linux glibc floor | [#549](https://github.com/HeddleCo/heddle/issues/549) | release CI | — | **Closed** |
| Distribution manifests | [#547](https://github.com/HeddleCo/heddle/issues/547) | `.github/workflows/` | **2–3 d** | Open |

---

## 4. Tier 1 execution plan (phased)

Each phase ends **GREEN** on verification gates (§7) before the next phase starts.

### Phase A — P0 [#595](https://github.com/HeddleCo/heddle/issues/595): `git-substrate` crate + sley dep

**Status: complete**

1. ✅ Add `crates/git-substrate` (`heddle-git-substrate`) with local `../sley` path deps (`git-core`, `git-formats`, `git-odb`, `git-refs`, `git-rev`).
2. ✅ Implement read-only adapter: `ObjectId`, `ObjectKind`, framing/hash (`frame_git_object`, `object_id_for_content`), `GitRepo` wrapper, `object` helpers.
3. ✅ Migrate P0 call-sites:
   - `ingest/src/git_walk.rs` — `GitSource` holds `GitRepo`; `is_commit` / `object_is_commit` via sley odb
   - `cli/src/bridge/git_import.rs` — `peel_to_commit_oid` + import path object-kind reads via substrate
   - `cli/src/bridge/git_reconstruct.rs` — framing/hash delegates to `git_substrate`
   - `repo/src/git_worktree_status.rs` — blob hashing via `blob_object_id`
4. ✅ Type-alias blast-radius bound: production bridge + `git_bridge_tests` + integration tests use `git_substrate::ObjectId` (`sley_core::ObjectId`).
5. ✅ Conformance test: `git_substrate` framing/hash round-trip + `commit_conformance` (4/4).
6. ✅ Gates: `git_replacement_matrix` (24/24), `commit_conformance` (4/4), `check-no-silent-default-tree-load.sh` clean, default build + `cargo test -p heddle-cli --lib bridge::git` (109/109). `realworld_git` nightly matrix still `#[ignore]` in CI (registry parse gate passes).

**Hold until PR [#587](https://github.com/HeddleCo/heddle/pull/587) merges** (backfill touches `git_import.rs`). Serialize vs [#593](https://github.com/HeddleCo/heddle/issues/593) (principal bytes).

---

### Phase B — P1 [#596](https://github.com/HeddleCo/heddle/issues/596): object/tree write on sley sink

**Status: complete**

1. ✅ Swap object/blob write: `git-substrate::write_blob` / `write_commit_content` on `FileObjectDatabase`.
2. ✅ Tree build: `write_tree` + `TreeEntryMode` mapping (incl. gitlink `160000`).
3. ✅ Port `export_tree` — recursive tree export via sley sink.
4. ✅ `write_commit_object` + native mint (`export_state`) + `git_notes` on sley sink.
5. ✅ **KEEP** `build_commit_content` — fidelity commits use heddle body-builder; simple commits use `write_simple_commit`.
6. ✅ Gate: `commit_conformance` (4/4), `git_replacement_matrix` (24/24), `bridge::git` (109/109).

**Unblocks:** [#568](https://github.com/HeddleCo/heddle/issues/568) (mirror-drop), [#575](https://github.com/HeddleCo/heddle/issues/575) (tag CA storage).

---

### Phase C — Decompose repo/CLI (ADR 0014): extract git bridge to shared crate

Runs in parallel with P2 once P1 is green; do not block substrate phases on full decomposition, but start extraction before P3 transport to shrink `git_core.rs` collision surface.

1. Create `crates/git-bridge` (or equivalent); move `cli/src/bridge/{git_core,git_import,git_export,git_sync,git_reconstruct}.rs` + tests.
2. `cli` depends on `git-bridge` for dispatch only; `ingest`/`repo` depend on `git-bridge` + `git-substrate`.
3. Collapse duplicate open/discover patterns behind `git_substrate::GitRepo`.
4. Align with ADR 0014: command facts stay local; bridge commands keep schema/doc gates.
5. Update `git_process_lint.rs` / import boundaries so bridge crate is the sole `git-substrate` consumer.

**Effort:** 1–2 weeks · **Risk:** `grpc_local_impl` / hosted paths must not regress during file moves.

---

### Phase D — De-lossy epic [#564](https://github.com/HeddleCo/heddle/issues/564) alignment with sley

Sequence after P1; P4 finalizes mirror-drop.

| Milestone | Depends on | Action |
|---|---|---|
| Backfill fidelity fields from mirror | [#587](https://github.com/HeddleCo/heddle/pull/587) merge | Run `heddle bridge backfill-fidelity`; re-hash states |
| Principal byte preservation | #587 + #591 merged | [#593](https://github.com/HeddleCo/heddle/issues/593): `Principal.name/email` → `Vec<u8>`; non-UTF8 conformance corpus |
| Tag objects in CA store | P1 #596 | [#575](https://github.com/HeddleCo/heddle/issues/575): delete sidecars; sync-propagating tag objects |
| Mirror elimination | P1 + export-from-state | [#568](https://github.com/HeddleCo/heddle/issues/568): drop `init_mirror`/`copy_reachable_objects`; adopt timing gate |
| Sley-only substrate + mirror gone | P4 #598 (done) | [#568](https://github.com/HeddleCo/heddle/issues/568): drop mirror on sley-only reconstruction |

---

## 5. Status

### Tier 1 — Substrate swap

- [x] **Epic** [#594](https://github.com/HeddleCo/heddle/issues/594) — sley substrate swap
- [x] **P0** [#595](https://github.com/HeddleCo/heddle/issues/595) — `git-substrate` crate + object-read/hashing
- [x] **P1** [#596](https://github.com/HeddleCo/heddle/issues/596) — object/tree write + sley sink
- [ ] **P2** [#597](https://github.com/HeddleCo/heddle/issues/597) — refs + index + worktree/checkout
- [ ] **P3** [#577](https://github.com/HeddleCo/heddle/issues/577) — `sley-remote` transport
- [x] **P4** [#598](https://github.com/HeddleCo/heddle/issues/598) — sley-only finalize

### Tier 2 — Architecture (ADR 0014)

- [ ] Extract git bridge from `cli` to shared crate
- [ ] `ingest` / `repo` consume `git-substrate` + bridge crate
- [ ] Command catalog / schema locality for collaboration writes
- [ ] CLI daemon carve-out ([audit Tier 1](docs/CLI_DEP_AUDIT_2026-05-12.md))

### Tier 3 — De-lossy (#564)

- [x] Step 1 [#565](https://github.com/HeddleCo/heddle/issues/565) — git-fidelity fields + format bump
- [ ] Step 1b [#570](https://github.com/HeddleCo/heddle/issues/570) / PR [#587](https://github.com/HeddleCo/heddle/pull/587) — mirror-backed backfill
- [ ] Step 2 [#566](https://github.com/HeddleCo/heddle/issues/566) — byte-exact serializers + conformance harness
- [x] Step 3 [#567](https://github.com/HeddleCo/heddle/issues/567) — export reconstruct-from-state
- [ ] Step 4 [#568](https://github.com/HeddleCo/heddle/issues/568) — mirror elimination
- [ ] Tags [#575](https://github.com/HeddleCo/heddle/issues/575) — first-class CA tag objects
- [ ] Principal bytes [#593](https://github.com/HeddleCo/heddle/issues/593) — `Vec<u8>` identities
- [ ] Signed fidelity [#562](https://github.com/HeddleCo/heddle/issues/562) — CI gate for signed commits/tags

### Tier 4 — Polish

- [ ] Hand-write JSON schemas (drop `schemars`)
- [ ] Drop `chrono` (std + RFC3339)
- [ ] Feature-gate optional CLI deps
- [ ] Light tracing subscriber
- [ ] Distribution manifests [#547](https://github.com/HeddleCo/heddle/issues/547)

---

## 6. Dependencies and blockers

| Blocker | Blocks | Resolution |
|---|---|---|
| PR [#587](https://github.com/HeddleCo/heddle/pull/587) (backfill-fidelity) | **P0 #595** dispatch | Merge first; touches `git_import.rs` / `git_walk.rs` |
| PR #591 (export-from-state, #567) | **#593** dispatch | Merge before principal-bytes work |
| **P0 #595** complete | **P1 #596** | Adapter + object-read must land first |
| **P1 #596** complete | **#568**, **#575**, **P2 #597** | sley sink required for write/reconstruct paths |
| **P2 #597** complete | **P3 #577** | refs/index/worktree on adapter before transport |
| **P3 #577** complete | mirror-drop polish | transport hardening before #568 finalize |
| **#593** (principal bytes) | Full **#568** mirror drop for non-UTF8 identities | Independent of sley phases but overlaps `git_import`; claim after #587 + #591 |
| Pre-P3 gate: remote protocol audit | **P3 #577** | Confirm heddle remotes need only git protocol v1 + `deepen` (not v2-only HTTP) |
| sley pre-1.0 churn | All sley phases | Local `../sley` path dep during dev; pin exact rev before publish; conformance test on every bump; adapter is single fallback surface |

**File-scope collision map (serialize work):**

```
git_import.rs  ← #587 (backfill), #595 (P0), #593 (principal)
git_reconstruct.rs ← #595 (P0), #596 (P1), #566 (serializer)
git_export.rs  ← #567 (merged), #596 (export_tree), #593 (reconstruction)
git_core.rs    ← #597 (P2), #577 (P3), bridge extraction (Tier 2)
```

---

## 7. Verification gates

Every Tier 1 phase must pass **all** of the following before merge:

### Acceptance suites (no `git` on `PATH`)

| Gate | Path | What it covers |
|---|---|---|
| `git_replacement_matrix` | `crates/cli/tests/cli_integration/git_replacement_matrix.rs` | Fresh git worktree + native repo read/write machine streams; bridge import/export/sync/reconcile; fetch/push/clone without git subprocess |
| `realworld_git` | `crates/cli/tests/cli_integration/realworld_git.rs` | Vendored fixtures (`realworld_git/fixtures/`): complex round-trip, large binary blob stress, rebase chain, multi-remote, annotated tags, cherry-pick, GC |

```bash
cargo test -p heddle-cli --test cli_integration git_replacement_matrix -- --nocapture
cargo test -p heddle-cli --test cli_integration realworld_git -- --nocapture
```

### Repo asserter

| Gate | Path |
|---|---|
| `check-no-silent-default-tree-load` | `scripts/check-no-silent-default-tree-load.sh` (also `heddle-devtools check-no-silent-default-tree-load`) |

Prevents silent default-tree loads that break POSIX/worktree semantics.

### Build matrix

```bash
cargo build --locked --workspace          # default features
cargo build --locked --workspace --all-features
bash scripts/check-no-silent-default-tree-load.sh
```

### Substrate-specific (add per #594 DoD)

- [x] Conformance: round-trip real git objects through sley adapter (framing/hash byte-exact vs system-git baseline) — `commit_conformance` (4/4) + `git_substrate` unit tests.
- [ ] `heddle doctor schemas` clean after any `Principal`/output-boundary change (#593).
- [x] `commit_conformance.rs` green for gpgsig/extra-headers/non-UTF8 identities; `roundtrip_fidelity.rs` unchanged (P1+).

---

## 8. Benchmark and CI gaps (from reconnaissance)

| Gap | Current state | Recommended action |
|---|---|---|
| **sley vs git perf baseline** | sley `scripts/bench-vs-git.sh` (~1.25×); **not in heddle CI** | Add weekly or per-sley-bump job; pin fixture OIDs; fail on >10% regression |
| **heddle-side sley conformance on dep bump** | Required by #594; **not automated** | CI step: round-trip N objects from `realworld_git` fixtures through `git_substrate` after `Cargo.lock` sley rev change |
| **Adopt mirror-phase timing** | #568 DoD asks before/after; **no gated benchmark** | Extend `cli/tests/performance.rs` or Criterion bench for adopt phases; assert mirror phase → ~0 post-#568 |
| **Criterion benches in CI** | `cli/benches/local_ops.rs`, `objects/benches/*`, `oplog/benches/*` exist; **not invoked by workflows** ([STABILITY.md](docs/STABILITY.md)) | Wire `cargo bench` smoke or Codspeed/Bencher for core crates (objects, refs, oplog) — not just CLI |
| **Perf regression gate** | `performance.rs` asserts phase timings in test suite only | Promote adopt/export/push wall-clock ceilings to CI with platform-pinned tolerances |
| **Signed-fidelity** | [#562](https://github.com/HeddleCo/heddle/issues/562) open; #533 strips signatures | Add signed commit/tag to conformance corpus; assert adopt→export SHA equality |
| **Coverage floor** | 72% line ([STABILITY.md](docs/STABILITY.md)); 1.0 target 80% | Raise floor as bridge/substrate crates gain unit tests in `git-substrate` |
| **Large-blob soak** | `realworld_git_large_binary_blob_stress` is `#[ignore]` | Optional scheduled job with `HEDDLE_LARGE_BLOB_MB` for release candidates |
| **CLI transitive dep tracking** | ~277-dep git-library subtree removed post-P4 ([CLI audit](docs/CLI_DEP_AUDIT_2026-05-12.md)) | Re-run `cargo metadata` script; track CLI transitive count vs pre-swap baseline |

---

## 9. References

- Spike (historical): `.heddleco-orchestrator/spikes/gix-to-sley-substrate.md` (§1 inventory, §3 write paths, §5 risks, §6 phased plan)
- sley repo: https://github.com/HeddleCo/sley
- sley benchmarks: `sley/scripts/bench-vs-git.sh`, `sley/crates/git-bench/`
- Verification map: [docs/VERIFICATION_STATE_LOGIC_MAP.md](VERIFICATION_STATE_LOGIC_MAP.md)
- Stability / 1.0 gates: [docs/STABILITY.md](STABILITY.md)
- CLI world-class audit: [docs/CLI_WORLD_CLASS_AUDIT_2026-05-21.md](CLI_WORLD_CLASS_AUDIT_2026-05-21.md)