# Platform matrix & Wave 7 residual

Control-plane checklist for **Wave 7** (platform matrix & long-tail). Labels follow program truth rules:

- **Shipped** — implemented and safe to describe as current behavior  
- **Foundation** — partially implemented or CI-backed, not a complete certified surface  
- **Planned** — intentional future-state; do not claim live  

This file does **not** invent product wins. Cross-check `GAP_MAP.md` L1/L2/L3/L6, `PRODUCT_CONTRACT.md` platforms, and `AGENTS.md` known limitations.

**Wave 7 status:** residual tracked — **not** full green. Do not claim Windows full parity, reftable product backend, or partial clone.

---

## Capability residual (Wave 7 scope)

| Area | Status | Notes / evidence |
|------|--------|------------------|
| Windows materialization (worktree / thread paths) | **Foundation** / residual open | See [Windows materialization inventory](#windows-materialization-inventory) below. Partial in product contract; edge-case certification **not** claimed. |
| Windows ProjFS mount | **Foundation** | CI job `projfs-smoke` on `windows-latest` (`rust-tests.yml`); optional feature enable + ignored smoke; can self-skip if ProjFS unavailable. Mount is **optional** product surface at runtime (kernel adapter may be missing). |
| Linux FUSE mount | **Foundation** | CI `fuse-smoke` + post-merge `fuse-bench` on `ubuntu-latest`. Optional `mount` feature / native adapter. |
| macOS FSKit mount | **Foundation** | CI `fskit-check` on `macos-26`: `cargo check -p heddle-mount --features fskit` when mount paths change — **compile**, not full mount e2e in PR matrix. |
| Mount feature optional | **Shipped** (product stance) | Local VCS must not require FUSE/ProjFS/FSKit at runtime; failures degrade. (CLI Cargo `default` may include the `mount` feature for install convenience — product stance is still “mount not required for core VCS.”) |
| packed-refs scale | **Shipped** with known degradation | Packed-refs implemented; **degrades ~10k+ refs** (`AGENTS.md`). Stress recipe: [`PACKED_REFS_STRESS.md`](PACKED_REFS_STRESS.md). |
| Reftable format | **Not implemented** / **Planned** long-tail | GAP_MAP **L2** open. Spike model + bench only; no production `RefManager` backend. |
| Partial clone / lazy object fetch | **Planned** | GAP_MAP **L3**; product non-goal until implemented (`PRODUCT_CONTRACT` explicit non-goals). |
| Multi-host perf / equal-work re-stamp | **Open** (Wave 6 residual) | Single-host n=5 samples in `PERF_BASELINE.md`; multi-host matrix and quieter-host re-run still open. |
| L6 object-shard grandparent fsync | **Shipped on tip with residual notes in GAP_MAP L6** | Production durable creates via `create_dir_all_durable` (atomic write / publish / store layout / pack install / sidecars / agent-task / lock / agent registry / streaming pack buckets; `create_private_dir_all` fsyncs new ancestors with Unix `0o700`). Residual: tests-only bare `create_dir_all`; **Windows directory fsync remains platform no-op**. |

---

## Windows materialization inventory

Short inventory of **supported vs unknown** from code/docs/CI. All Windows non-mount claims stay **Foundation** unless noted. **Not** full OS parity.

### Supported / Foundation (evidence exists)

| Surface | Evidence | Label |
|---------|----------|--------|
| Windows **release binary** (x86_64 MSVC) | `.github/workflows/release.yml` target `x86_64-pc-windows-msvc` | **Foundation** (ship artifact, not full test parity) |
| **ProjFS mount** smoke | `rust-tests.yml` job `projfs-smoke` on `windows-latest` when mount paths change; `heddle-mount --features projfs`; tests `#[ignore]` + `HEDDLE_PROJFS_AVAILABLE=1` | **Foundation** |
| ProjFS **skip vs fail** | CI comments: if Client-ProjFS / Projected-FS cannot enable or `ProjectedFSLib.dll` missing, smoke **self-skips** (job exit 0). Hard **assertion failure** is the regression signal. | Documented gate language (this file) |
| Worktree **stat signature** on Windows | `crates/repo/src/stat_signature.rs` — FILETIME→unix ns, NTFS/ReFS file index via handle, reparse-don't-follow open, share modes including `FILE_SHARE_DELETE` | **Foundation** helper |
| Recursive path remove on Windows | `crates/objects/src/fs_ops.rs` — reparse-point aware remove; final path via handle for stable walk | **Foundation** helper |
| Path separator normalization | Worktree / materialize / fsmonitor paths normalize `\` → `/` in several repo paths | **Foundation** |
| Product contract OS line | `PRODUCT_CONTRACT.md`: Windows **partial** — mount ProjFS, materialization helpers | Honest partial claim |

### Unknown / residual open (do not claim)

| Surface | Why open |
|---------|----------|
| Path length / **MAX_PATH** / long-path prefix (`\\?\`) edge cases under deep thread checkouts | No certified matrix or contract section |
| Junctions / directory symlinks beyond reparse-point remove helpers | Not a full junction certification suite |
| Exclusive locks / sharing violations during materialize vs open files (editors, AV) | Handle open has share flags; materialize crash/recovery under lock storms **not** certified |
| Line endings (CRLF) policy as a Windows materialization guarantee | Not claimed as a Wave 7 ship surface |
| Full curated / high-signal suite on Windows | **Not** matrixed as continuous gate |
| Directory **fsync** durability on Windows | `sync_directory` is a **no-op** (`objects::fs_atomic`); creation still proceeds |
| aarch64 Windows release | Parked in release workflow (cosign asset gap) |

### Release-gate wording (Windows)

**Windows = mount foundation + partial materialization helpers only.**  
Absence of a full Windows curated suite is **intentional residual**, not silent parity with Linux/macOS. Do not treat ProjFS smoke green as “Windows daily-driver certified.”

---

## What CI exercises today vs unknown

Derived from `.github/workflows/rust-tests.yml` (and related workflows). Dogfood machine class is typically macOS (see program baseline env stamps); **primary PR correctness lane is Linux ARM**.

| Surface | Linux | macOS | Windows |
|---------|-------|-------|---------|
| Main `check-and-test` (build/clippy/tests, affected pkgs) | **Yes** — `blacksmith-4vcpu-ubuntu-2404-arm` | **No** (not the PR test matrix host) | **No** |
| Facade render-free / path-change detection | **Yes** (`ubuntu-latest`) | No | No |
| Fault-injection / coverage / postgres jobs | **Yes** (Linux ARM where configured) | No | No |
| FUSE mount smoke / bench | **Yes** (`ubuntu-latest`, mount path changes / post-merge bench) | No | No |
| FSKit | No | **Compile check only** (`macos-26`, mount path changes) | No |
| ProjFS mount smoke | No | No | **Yes** (`windows-latest`, mount path changes; may self-skip) |
| Full curated program suite (`run-baseline.sh --suite curated`) | **Local/program stamps** (often macOS dogfood + Linux agents) — not a dedicated multi-OS GitHub matrix job named for Wave 7 | Program dogfood + release macOS build paths exist separately | **Unknown / not matrixed** as full curated suite |
| Large-ref packed-refs stress (~10k+) | **Documented recipe** ([`PACKED_REFS_STRESS.md`](PACKED_REFS_STRESS.md)); **not** continuous CI gate | Same | Same |
| Windows non-mount materialization edge cases | N/A | N/A | **Unknown** / residual open (inventory above) |
| Equal-work perf multi-host | **Not certified** | Single-host samples exist | **Not certified** |

**Interpretation:** Linux is the continuous correctness backbone. macOS is primary dogfood + selective FSKit compile + release artifact builds. Windows is **mount-smoke foundation + release binary**, not full materialization parity. Gaps in the table are Wave 7 residual, not silent “pass.”

---

## Executable residual checklist (what would green Wave 7)

Treat items as done only when evidence is checked in (tests, CI job, or program stamp) and labeled honestly.

### Mount / materialization

- [x] Document explicit supported Windows materialization paths vs known failures (path length, junctions, exclusive locks, line endings if any).  
  **Evidence:** [Windows materialization inventory](#windows-materialization-inventory) in this file (2026-07-11). Supported = ProjFS foundation + stat/fs helpers; path length / junctions / locks / CRLF remain **Unknown**.
- [x] ProjFS smoke remains green when mount paths change; capture skip vs fail distinction in gate language.  
  **Evidence:** `rust-tests.yml` `projfs-smoke` comments + inventory table — self-skip on missing ProjFS; hard assert = fail.
- [ ] FSKit: decide whether Wave 7 requires more than `cargo check` (e.g. ignored smoke on macOS runners) and record the bar.
- [ ] Confirm non-`mount` / core VCS paths remain green without FUSE/ProjFS/FSKit on all three OS classes claimed in docs.  
  **Note:** Linux PR matrix covers core tests; Windows full curated without mount is **not** matrixed — residual open.

### Refs scale

- [x] Document packed-refs degradation threshold (~10k) with a reproducible stress recipe (fixture size, command, expected graceful behavior vs OOM/timeout).  
  **Evidence:** [`PACKED_REFS_STRESS.md`](PACKED_REFS_STRESS.md) + `scripts/program/packed-refs-stress-recipe.sh` → `cargo bench -p heddle-refs --bench reftable_vs_packed`.
- [x] Add or classify a large-ref test as stress (not a false “oracle pass” on tiny fixtures).  
  **Evidence:** Criterion bench sizes 10k/50k/100k classified as **stress tool**; `refs_packed_tests` remain small-fixture **correctness** only. Not a continuous CI gate.
- [x] Reftable remains **Planned** unless implementation lands — do not green Wave 7 by renaming reftable as shipped.  
  **Evidence:** capability table + GAP_MAP L2 + `docs/design/reftable-spike.md` (defer).

### Partial clone

- [x] Keep **Planned** until wire/repo lazy fetch exists.  
  **Evidence:** capability table + GAP_MAP L3 + `PRODUCT_CONTRACT` non-goals.
- [x] Ensure docs/program copy never implies partial clone is live (`PRODUCT_CONTRACT` already lists it as non-goal/planned).  
  **Evidence:** this file labels **Planned** only.

### Cross-platform correctness signal

- [ ] At least one documented path for full curated or high-signal suite on Linux **and** a recorded macOS dogfood/cert stamp (already common for program tip) with env/commit artifacts.
- [x] Windows: either (a) curated/high-signal subset job, or (b) explicit “Windows = mount foundation only” release-gate wording so residual is not mistaken for full parity.  
  **Evidence:** (b) recorded under [Release-gate wording (Windows)](#release-gate-wording-windows) — mount foundation + partial helpers only; no full parity claim. (a) still open if product later wants a Windows curated subset job.
- [x] No unreviewed `git` process dependency regressions on scanned runtime dirs (C4 **Shipped**).  
  **Evidence:** GAP_MAP **C4** Shipped — `git_process_lint` scan dirs include `crates/core/src` and `crates/git-projection/src`; gate command in `RELEASE_GATES.md` G1.

### Wave 6 handoff (adjacent, not Wave 7 exclusive)

- [ ] Equal-work tip re-stamp with paired before/after for any hotspot change.
- [ ] Multi-host or quieter-host perf sample before external speed claims.

### Wave 5 handoff (adjacent)

- [x] L6 `create_dir_all_durable` (or equivalent) landed in objects + tests; residual notes remain in GAP_MAP L6 (not a silent “no residual”).  
  **Evidence:** GAP_MAP L6 **Shipped on program tip**; capability row above. Windows dir fsync no-op called out; optional remaining production sites tracked in GAP_MAP, not re-opened as “unshipped.”
- [ ] L7/L8 residuals remain optional harden notes unless product raises them to P1.

---

## Wave 7 “green” definition (interim)

Wave 7 is **green** when:

1. Platform residual above is either **closed with evidence** or **explicitly deferred** with owner + label (**Foundation** / **Planned**).  
2. No doc claims Windows full parity, reftable, or partial clone as **Shipped**.  
3. Mount optional stance is preserved in contract and CI (failures degrade; core VCS does not require mount).  
4. Large-ref packed-refs behavior is documented with a stress recipe; reftable remains honestly **not implemented**.  

Until then: status stays **Open / residual tracked** in `WAVES.md`.

---

## Related pointers

| Doc | Role |
|-----|------|
| [GAP_MAP.md](GAP_MAP.md) | L1 / L2 / L3 / L6 ownership rows |
| [WAVES.md](WAVES.md) | Wave 5–8 status |
| [PRODUCT_CONTRACT.md](PRODUCT_CONTRACT.md) | Supported platforms / explicit non-goals |
| [PERF_BASELINE.md](PERF_BASELINE.md) | Single-host equal-work samples; multi-host open |
| [RELEASE_GATES.md](RELEASE_GATES.md) | Executable correctness/perf gates |
| [PACKED_REFS_STRESS.md](PACKED_REFS_STRESS.md) | 10k+ packed-refs stress recipe + non-claims |
| `docs/design/reftable-spike.md` | Reftable defer decision (not product) |
| `AGENTS.md` known limitations | packed-refs ~10k, partial clone, etc. |
