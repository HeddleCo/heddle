# Platform matrix & Wave 7 residual

Control-plane checklist for **Wave 7** (platform matrix & long-tail). Labels follow program truth rules:

- **Shipped** — implemented and safe to describe as current behavior  
- **Foundation** — partially implemented or CI-backed, not a complete certified surface  
- **Planned** — intentional future-state; do not claim live  

This file does **not** invent product wins. Cross-check `GAP_MAP.md` L1/L2/L3, `PRODUCT_CONTRACT.md` platforms, and `AGENTS.md` known limitations.

---

## Capability residual (Wave 7 scope)

| Area | Status | Notes / evidence |
|------|--------|------------------|
| Windows materialization (worktree / thread paths) | **Foundation** / residual open | Windows is partial in product contract (ProjFS, materialization helpers). Full materialization edge-case certification **not** claimed. |
| Windows ProjFS mount | **Foundation** | CI job `projfs-smoke` on `windows-latest` (`rust-tests.yml`); optional feature enable + ignored smoke; can self-skip if ProjFS unavailable. Mount is **optional** product surface. |
| Linux FUSE mount | **Foundation** | CI `fuse-smoke` + post-merge `fuse-bench` on `ubuntu-latest`. Optional `mount` feature. |
| macOS FSKit mount | **Foundation** | CI `fskit-check` on `macos-26`: `cargo check -p heddle-mount --features fskit` when mount paths change — **compile**, not full mount e2e in PR matrix. |
| Mount feature optional | **Shipped** (product stance) | Mount is opt-in; local VCS must not require FUSE/ProjFS/FSKit. |
| packed-refs scale | **Shipped** with known degradation | Packed-refs implemented; **degrades ~10k+ refs** (see `AGENTS.md` known limitations). |
| Reftable format | **Not implemented** / **Planned** long-tail | GAP_MAP **L2** open. No reftable store in OSS tree. |
| Partial clone / lazy object fetch | **Planned** | GAP_MAP **L3**; product non-goal until implemented. |
| Multi-host perf / equal-work re-stamp | **Open** (Wave 6 residual) | Single-host n=5 samples in `PERF_BASELINE.md`; multi-host matrix and quieter-host re-run still open. |
| L6 object-shard grandparent fsync | **In progress / delegated** | GAP_MAP **L6** — do not mark fixed until objects code lands. |

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
| Large-ref packed-refs stress (~10k+) | **Unknown** as continuous gate | **Unknown** | **Unknown** |
| Windows non-mount materialization edge cases | N/A | N/A | **Unknown** / residual open |
| Equal-work perf multi-host | **Not certified** | Single-host samples exist | **Not certified** |

**Interpretation:** Linux is the continuous correctness backbone. macOS is primary dogfood + selective FSKit compile + release artifact builds. Windows is mount-smoke foundation only. Gaps in the table are Wave 7 residual, not silent “pass.”

---

## Executable residual checklist (what would green Wave 7)

Treat items as done only when evidence is checked in (tests, CI job, or program stamp) and labeled honestly.

### Mount / materialization

- [ ] Document explicit supported Windows materialization paths vs known failures (path length, junctions, exclusive locks, line endings if any).
- [ ] ProjFS smoke remains green when mount paths change; capture skip vs fail distinction in gate language.
- [ ] FSKit: decide whether Wave 7 requires more than `cargo check` (e.g. ignored smoke on macOS runners) and record the bar.
- [ ] Confirm non-`mount` default CLI features remain green without FUSE/ProjFS/FSKit on all three OS classes claimed in docs.

### Refs scale

- [ ] Document packed-refs degradation threshold (~10k) with a reproducible stress recipe (fixture size, command, expected graceful behavior vs OOM/timeout).
- [ ] Add or classify a large-ref test as stress (not a false “oracle pass” on tiny fixtures).
- [ ] Reftable remains **Planned** unless implementation lands — do not green Wave 7 by renaming reftable as shipped.

### Partial clone

- [ ] Keep **Planned** until wire/repo lazy fetch exists.
- [ ] Ensure docs/program copy never implies partial clone is live (`PRODUCT_CONTRACT` already lists it as non-goal/planned).

### Cross-platform correctness signal

- [ ] At least one documented path for full curated or high-signal suite on Linux **and** a recorded macOS dogfood/cert stamp (already common for program tip) with env/commit artifacts.
- [ ] Windows: either (a) curated/high-signal subset job, or (b) explicit “Windows = mount foundation only” release-gate wording so residual is not mistaken for full parity.
- [ ] No unreviewed `git` process dependency regressions on scanned runtime dirs (C4 **Shipped**).

### Wave 6 handoff (adjacent, not Wave 7 exclusive)

- [ ] Equal-work tip re-stamp with paired before/after for any hotspot change.
- [ ] Multi-host or quieter-host perf sample before external speed claims.

### Wave 5 handoff (adjacent)

- [ ] L6 `create_dir_all_durable` (or equivalent) landed in objects + tests; only then mark GAP_MAP L6 closed.
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
| [GAP_MAP.md](GAP_MAP.md) | L1 / L2 / L3 ownership rows |
| [WAVES.md](WAVES.md) | Wave 5–8 status |
| [PRODUCT_CONTRACT.md](PRODUCT_CONTRACT.md) | Supported platforms / explicit non-goals |
| [PERF_BASELINE.md](PERF_BASELINE.md) | Single-host equal-work samples; multi-host open |
| [RELEASE_GATES.md](RELEASE_GATES.md) | Executable correctness/perf gates |
| `AGENTS.md` known limitations | packed-refs ~10k, partial clone, etc. |
