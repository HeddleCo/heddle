# Trustworthy baseline record

## Environment (this machine)

| Field | Value |
|-------|-------|
| Commit (TODO #3 ac8c full curated re-cert) | `ac8c1aa64361f123ba5c2a542a284134d2dc2a0f` |
| Commit (TODO #3 e614 full curated re-cert) | `e6145058bc214d8681e94dd449adff4620dfb281` |
| Commit (TODO N5 full curated re-cert) | `96a422a824655ecc681042f6c71b988987efc272` |
| Commit (TODO R2 full curated re-cert) | `6a09ecb7ee96de9b6761c930e15103912f7d0e62` |
| Commit (TODO #4 full curated re-cert) | `a5b1dc689c755228be15cefeaffd91dbb9dd18f3` |
| Commit (Wave 8 full curated cert) | `d3db01439b5f245af9785871a2709786a88742b2` |
| Commit (Wave 8 partial cert) | `b7f51aa4a505c69c3dea5831ed44069f954b36c3` |
| Commit (post–Wave 2/3 re-cert) | `b748bfd4af575d9563437592213a5582de5e0f4d` |
| Commit (program start) | `74f2e20edef1572877c712c8551485fc2b5655a8` |
| Branch | `codex/correctness-architecture-performance-program` |
| OS | macOS 26.5.1 (Darwin 25.5.0 arm64) |
| CPU | Apple M1 Pro |
| Memory | 32 GB |
| rustc | 1.97.0 (2d8144b78 2026-07-07) |
| cargo | 1.97.0 (c980f4866 2026-06-30) |
| git | 2.55.0 |
| CARGO_TARGET_DIR (TODO #3 ac8c re-cert) | `/tmp/heddle-nw-t3` (seeded by `mv` from `/tmp/heddle-w-t3`) |
| CARGO_TARGET_DIR (TODO #3 e614 re-cert) | `/tmp/heddle-w-t3` (moved into `/tmp/heddle-nw-t3` for ac8c) |
| CARGO_TARGET_DIR (TODO N5 re-cert) | `/tmp/heddle-n5-t3` (seeded into `/tmp/heddle-w-t3` for e614) |
| CARGO_TARGET_DIR (TODO R2 re-cert) | `/tmp/heddle-r2-t3` (reused as `/tmp/heddle-n5-t3` for N5) |
| CARGO_TARGET_DIR (TODO #4 re-cert) | `/tmp/heddle-todo4-target` |
| CARGO_TARGET_DIR (Wave 8 full curated) | `/tmp/heddle-cert3-target` |
| CARGO_TARGET_DIR (Wave 8 high-signal) | `/tmp/heddle-cert-w8` |
| CARGO_TARGET_DIR (release perf binary) | `/tmp/heddle-todo5-target` (primary); prior `/tmp/heddle-w8-target` |
| Isolation (TODO #3 ac8c) | detached git worktree `/tmp/heddle-nw-cert` at tip `ac8c1aa6` (`dirty=0`); artifacts under main workspace |
| Isolation (TODO #3 e614) | detached git worktree `/tmp/heddle-w-cert` at tip `e6145058` (`dirty=0`); artifacts under main workspace |
| Isolation (TODO N5) | detached git worktree `/tmp/heddle-n5-cert` at tip `96a422a8` (`dirty=0`); artifacts under main workspace |
| Isolation (TODO R2) | detached git worktree `/tmp/heddle-r2-cert` at tip `6a09ecb7` (`dirty=0`); artifacts under main workspace |
| Isolation (TODO #4) | detached git worktree `/tmp/heddle-todo4-worktree` at tip (`dirty=0`); artifacts under main workspace |
| Isolation (prior) | workspace (`/Users/lukethorne/dev/HeddleCo/workspace/session-2026-07-10`) |

## Harness status

| Capability | Status |
|------------|--------|
| Curated manifest | **Shipped** — `scripts/program/manifest.toml` |
| Baseline runner + classification | **Shipped** — `scripts/program/run-baseline.sh` |
| Paired bench runner | **Shipped** — `scripts/program/paired-bench.py` |
| CLI residual inventory | **Shipped** — `scripts/program/gen-cli-domain-residual.py` |
| TODO #3 ac8c full curated re-cert (19 jobs) | **Green** — 19/19 pass on `ac8c1aa6`; see `artifacts/baseline/wave-ac8c-cert-merged/summary.json` |
| TODO #3 e614 full curated re-cert (19 jobs) | **Green** (historical) — 19/19 pass on `e6145058`; see `artifacts/baseline/wave-e614-cert-merged/summary.json` |
| TODO N5 full curated re-cert (19 jobs) | **Green** (historical) — 19/19 pass on `96a422a8`; see `artifacts/baseline/todo-n5-cert-merged/summary.json` |
| TODO R2 full curated re-cert (19 jobs) | **Green** (historical) — 19/19 pass on `6a09ecb7`; see `artifacts/baseline/todo-r2-cert-merged/summary.json` |
| TODO #4 full curated re-cert (19 jobs) | **Green** (historical) — 19/19 pass on `a5b1dc68`; see `artifacts/baseline/todo4-curated-merged/summary.json` |
| Wave 8 full curated (19 jobs) | **Green** (historical) — 19/19 pass on `d3db0143`; see `artifacts/baseline/wave-next-merged/summary.json` |
| High-signal Wave 8 re-cert (5 jobs) | **Green** — 5/5 pass; see `artifacts/baseline/post-wave-fanout2-merged/summary.json` |
| High-signal post–Wave 2/3 re-cert (7 jobs) | **Green** — 7/7 pass; see `artifacts/baseline/post-wave23-merged/summary.json` |
| Full curated suite (19 jobs) | **Current stamp** — 19 pass / 0 fail (`wave-ac8c-cert-merged` on `ac8c1aa6`); supersedes `wave-e614-cert-merged` |
| Clippy (`-D warnings`) | **Pass** on `ac8c1aa6` |
| Clippy (soft, no `-D`) | **Pass** — 0 warnings |
| `cargo doc -p heddle-core --no-deps` | **Pass** |
| Performance certification (5 trials) | **Recorded** — n=5 absolute + paired self-pairs on tip `a5b1dc68` stamp `20260711T155225Z`; see `docs/program/PERF_BASELINE.md` (**not** a Git win claim; not re-run for TODO #3 ac8c) |

## TODO #3 ac8c full curated re-cert (2026-07-11, this machine) — **current authority**

Source: `artifacts/baseline/wave-ac8c-cert-merged/summary.json` after
`bash scripts/program/run-baseline.sh --suite curated` on commit
`ac8c1aa64361f123ba5c2a542a284134d2dc2a0f` with
`CARGO_TARGET_DIR=/tmp/heddle-nw-t3`.

**Method:** detached worktree at tip (`git worktree add --detach
/tmp/heddle-nw-cert ac8c1aa6`, `dirty=0`) so concurrent dirty WIP / untracked
artifacts in the main workspace could not poison the cert. Single full-suite run
into `artifacts/baseline/wave-ac8c-cert/`; gates + merge under
`artifacts/baseline/wave-ac8c-cert-merged/`.

| Job | Status | Duration | Oracle |
|-----|--------|----------|--------|
| facade-render-free | **pass** | 334 ms | no |
| fmt-check | **pass** | 36.8 s | no |
| git-process-lint | **pass** | 15.9 s | yes |
| roundtrip-fidelity | **pass** | 12.5 s | yes |
| commit-conformance | **pass** | 6.2 s | yes |
| git-projection-engine | **pass** | 182.0 s | yes |
| lib-objects | **pass** | 8.7 s | no |
| lib-refs | **pass** | 21.3 s | no |
| lib-oplog | **pass** | 13.8 s | no |
| lib-merge | **pass** | 23.4 s | no |
| lib-format | **pass** | 3.4 s | no |
| lib-crypto | **pass** | 29.8 s | no |
| lib-core | **pass** | 82.2 s | no |
| lib-repo | **pass** | 170.0 s | no |
| lib-ingest | **pass** | 75.2 s | no |
| lib-git-projection | **pass** | 14.5 s | yes |
| cli-core-functionality | **pass** | 63.2 s | no |
| cli-state-management | **pass** | 49.1 s | no |
| formal-specs | **pass** | 3.6 s | yes |

\*Durations on warm `CARGO_TARGET_DIR=/tmp/heddle-nw-t3` after serial `heddle-mount` + `heddle-cli` prebuild (module-cache wipe + rebuild; target seeded from e614 `/tmp/heddle-w-t3`).

**Aggregate curated:** **19 pass / 0 fail.** All oracle jobs green
(git-process-lint, roundtrip-fidelity, commit-conformance, git-projection-engine,
lib-git-projection, formal-specs). **fmt-check green.**

### Extra gates (same tip / target dir / clean checkout)

| Gate | Status | Notes |
|------|--------|-------|
| `cargo clippy -p heddle-core -p heddle-cli --locked -- -D warnings` | **pass** (exit 0) | clean on tip `ac8c1aa6` |
| `cargo clippy -p heddle-core -p heddle-cli --locked` | **pass** | 0 warnings |
| `cargo doc -p heddle-core --no-deps --locked` | **pass** | 1 rustdoc intra-doc link warning (`skip_reason`); not a fail |

**Release-gate checklist:** curated+oracles+fmt+clippy `-D`+doc **green**.
Perf n=5 prior stamp retained; multi-host still open. **No blockers** from this
cert pass on tip `ac8c1aa6`.

Logs: `artifacts/baseline/wave-ac8c-cert-merged/logs/clippy-*.log`,
`cargo-doc.log`, `run-baseline-suite.log`. Runner stamp:
`artifacts/baseline/wave-ac8c-cert/`.

**Note:** A first tool-timeout attempt was killed after 7/19 jobs (partial under
`artifacts/baseline/wave-ac8c-cert-killed-partial/`, **not** authoritative). The
authoritative re-run above completed 19/19 green. Target dir was seeded by
`mv /tmp/heddle-w-t3 /tmp/heddle-nw-t3` after wiping `clang-module-cache` and a
serial mount/`heddle-cli` prebuild.

## TODO #3 e614 full curated re-cert (historical, 2026-07-11)

Source: `artifacts/baseline/wave-e614-cert-merged/summary.json` after
`bash scripts/program/run-baseline.sh --suite curated` on commit
`e6145058bc214d8681e94dd449adff4620dfb281` with
`CARGO_TARGET_DIR=/tmp/heddle-w-t3`.
**Superseded for tip authority by TODO #3 ac8c** on `ac8c1aa6`.

**Method:** detached worktree at tip (`git worktree add --detach
/tmp/heddle-w-cert e6145058`, `dirty=0`) so concurrent dirty WIP / untracked
artifacts in the main workspace could not poison the cert. Single full-suite run
into `artifacts/baseline/wave-e614-cert/`; gates + merge under
`artifacts/baseline/wave-e614-cert-merged/`.

| Job | Status | Duration | Oracle |
|-----|--------|----------|--------|
| facade-render-free | **pass** | 49 ms | no |
| fmt-check | **pass** | 3.7 s | no |
| git-process-lint | **pass** | 3.0 s | yes |
| roundtrip-fidelity | **pass** | 8.9 s | yes |
| commit-conformance | **pass** | 5.7 s | yes |
| git-projection-engine | **pass** | 289.6 s | yes |
| lib-objects | **pass** | 104.9 s | no |
| lib-refs | **pass** | 16.5 s | no |
| lib-oplog | **pass** | 9.3 s | no |
| lib-merge | **pass** | 12.2 s | no |
| lib-format | **pass** | 2.0 s | no |
| lib-crypto | **pass** | 9.8 s | no |
| lib-core | **pass** | 23.5 s | no |
| lib-repo | **pass** | 98.6 s | no |
| lib-ingest | **pass** | 25.3 s | no |
| lib-git-projection | **pass** | 4.2 s | yes |
| cli-core-functionality | **pass** | 47.6 s | no |
| cli-state-management | **pass** | 47.4 s | no |
| formal-specs | **pass** | 3.2 s | yes |

\*Durations on warm `CARGO_TARGET_DIR=/tmp/heddle-w-t3` after serial `heddle-mount` + `heddle-cli` prebuild (module-cache wipe + rebuild).

**Aggregate curated:** **19 pass / 0 fail.** All oracle jobs green
(git-process-lint, roundtrip-fidelity, commit-conformance, git-projection-engine,
lib-git-projection, formal-specs). **fmt-check green.**

### Extra gates (same tip / target dir / clean checkout)

| Gate | Status | Notes |
|------|--------|-------|
| `cargo clippy -p heddle-core -p heddle-cli --locked -- -D warnings` | **pass** (exit 0) | clean on tip `e6145058` |
| `cargo clippy -p heddle-core -p heddle-cli --locked` | **pass** | 0 warnings |
| `cargo doc -p heddle-core --no-deps --locked` | **pass** | 1 rustdoc intra-doc link warning (`skip_reason`); not a fail |

**Release-gate checklist:** curated+oracles+fmt+clippy `-D`+doc **green**.
Perf n=5 prior stamp retained; multi-host still open. **No blockers** from this
cert pass on tip `e6145058`.

Logs: `artifacts/baseline/wave-e614-cert-merged/logs/clippy-*.log`,
`cargo-doc.log`, `run-baseline-suite.log`. Runner stamp:
`artifacts/baseline/wave-e614-cert/`.

**Note:** A first attempt failed early oracle jobs because `CARGO_TARGET_DIR` was
seeded by `mv /tmp/heddle-n5-t3 /tmp/heddle-w-t3`, leaving Swift
`clang-module-cache` PCMs with baked-in `/tmp/heddle-n5-t3` paths. After wiping
`clang-module-cache` + `heddle-mount` build dirs and a serial mount/`heddle-cli`
rebuild, the authoritative re-run above completed 19/19 green. The killed partial
under `artifacts/baseline/wave-e614-cert-killed-partial/` is **not** authoritative.

## TODO N5 full curated re-cert (historical, 2026-07-11)

Source: `artifacts/baseline/todo-n5-cert-merged/summary.json` after
`bash scripts/program/run-baseline.sh --suite curated` on commit
`96a422a824655ecc681042f6c71b988987efc272` with
`CARGO_TARGET_DIR=/tmp/heddle-n5-t3`.
**Superseded for tip authority by TODO #3 e614** on `e6145058`.

**Method:** detached worktree at tip (`git worktree add --detach
/tmp/heddle-n5-cert 96a422a8`, `dirty=0`) so concurrent dirty WIP / untracked
artifacts in the main workspace could not poison the cert. Single full-suite run
into `artifacts/baseline/todo-n5-cert/`; gates + merge under
`artifacts/baseline/todo-n5-cert-merged/`.

| Job | Status | Duration | Oracle |
|-----|--------|----------|--------|
| facade-render-free | **pass** | 27 ms | no |
| fmt-check | **pass** | 2.4 s | no |
| git-process-lint | **pass** | 1.9 s | yes |
| roundtrip-fidelity | **pass** | 3.7 s | yes |
| commit-conformance | **pass** | 2.1 s | yes |
| git-projection-engine | **pass** | 90.8 s | yes |
| lib-objects | **pass** | 4.3 s | no |
| lib-refs | **pass** | 2.4 s | no |
| lib-oplog | **pass** | 4.5 s | no |
| lib-merge | **pass** | 5.3 s | no |
| lib-format | **pass** | 0.8 s | no |
| lib-crypto | **pass** | 0.6 s | no |
| lib-core | **pass** | 3.3 s | no |
| lib-repo | **pass** | 79.4 s | no |
| lib-ingest | **pass** | 26.0 s | no |
| lib-git-projection | **pass** | 4.3 s | yes |
| cli-core-functionality | **pass** | 44.5 s | no |
| cli-state-management | **pass** | 46.2 s | no |
| formal-specs | **pass** | 1.1 s | yes |

\*Durations on warm `CARGO_TARGET_DIR=/tmp/heddle-n5-t3` after serial `heddle-mount` + `heddle-cli` prebuild.

**Aggregate curated:** **19 pass / 0 fail.** All oracle jobs green
(git-process-lint, roundtrip-fidelity, commit-conformance, git-projection-engine,
lib-git-projection, formal-specs). **fmt-check green.**

### Extra gates (same tip / target dir / clean checkout)

| Gate | Status | Notes |
|------|--------|-------|
| `cargo clippy -p heddle-core -p heddle-cli --locked -- -D warnings` | **pass** (exit 0) | clean on tip `96a422a8` |
| `cargo clippy -p heddle-core -p heddle-cli --locked` | **pass** | 0 warnings |
| `cargo doc -p heddle-core --no-deps --locked` | **pass** | |

**Release-gate checklist:** curated+oracles+fmt+clippy `-D`+doc **green**.
Perf n=5 prior stamp retained; multi-host still open. **No blockers** from this
cert pass on tip `96a422a8`.

Logs: `artifacts/baseline/todo-n5-cert-merged/logs/clippy-*.log`,
`cargo-doc.log`, `run-baseline-suite.log`. Runner stamp:
`artifacts/baseline/todo-n5-cert/`.

**Note:** A first attempt under concurrent host load hit Swift `HeddleFSKit`
module-cache thrash (false fails for early `heddle-cli` oracle jobs while other
`/tmp/heddle-n5-t*` targets compiled mount in parallel). After freeing unused
`r2` cargo targets and a serial mount/`heddle-cli` prebuild, the authoritative
re-run above completed 19/19 green. The killed partial under
`artifacts/baseline/todo-n5-cert-killed-partial/` is **not** authoritative.

## TODO R2 full curated re-cert (historical, 2026-07-11)

Source: `artifacts/baseline/todo-r2-cert-merged/summary.json` after
`bash scripts/program/run-baseline.sh --suite curated` on commit
`6a09ecb7ee96de9b6761c930e15103912f7d0e62` with
`CARGO_TARGET_DIR=/tmp/heddle-r2-t3`.
**Superseded for tip authority by TODO N5** on `96a422a8`.

**Method:** detached worktree at tip (`git worktree add --detach
/tmp/heddle-r2-cert 6a09ecb7`, `dirty=0`) so concurrent dirty WIP / untracked
artifacts in the main workspace could not poison the cert. Single full-suite run
into `artifacts/baseline/todo-r2-cert/`; gates + merge under
`artifacts/baseline/todo-r2-cert-merged/`.

| Job | Status | Duration | Oracle |
|-----|--------|----------|--------|
| facade-render-free | **pass** | 106 ms | no |
| fmt-check | **pass** | 33.7 s | no |
| git-process-lint | **pass** | 120.2 s* | yes |
| roundtrip-fidelity | **pass** | 6.4 s | yes |
| commit-conformance | **pass** | 5.0 s | yes |
| git-projection-engine | **pass** | 352.2 s* | yes |
| lib-objects | **pass** | 40.4 s | no |
| lib-refs | **pass** | 16.5 s | no |
| lib-oplog | **pass** | 8.5 s | no |
| lib-merge | **pass** | 18.5 s | no |
| lib-format | **pass** | 3.3 s | no |
| lib-crypto | **pass** | 18.3 s | no |
| lib-core | **pass** | 38.0 s | no |
| lib-repo | **pass** | 115.6 s* | no |
| lib-ingest | **pass** | 29.9 s | no |
| lib-git-projection | **pass** | 4.3 s | yes |
| cli-core-functionality | **pass** | 44.5 s | no |
| cli-state-management | **pass** | 43.7 s | no |
| formal-specs | **pass** | 1.9 s | yes |

\*May include compile into `CARGO_TARGET_DIR=/tmp/heddle-r2-t3` on first use of package.

**Aggregate curated:** **19 pass / 0 fail.** All oracle jobs green
(git-process-lint, roundtrip-fidelity, commit-conformance, git-projection-engine,
lib-git-projection, formal-specs). **fmt-check green.** Extra gates (clippy
`-D`, soft clippy, cargo doc) also green on that tip. ENOSPC partial under
`todo-r2-cert-enospace-partial/` is not authoritative.

## TODO #4 full curated re-cert (historical, 2026-07-11)

Source: `artifacts/baseline/todo4-curated-merged/summary.json` after
`bash scripts/program/run-baseline.sh --suite curated` on commit
`a5b1dc689c755228be15cefeaffd91dbb9dd18f3` with
`CARGO_TARGET_DIR=/tmp/heddle-todo4-target`.
**Superseded for tip authority by TODO N5** on `96a422a8` (was superseded by R2 on `6a09ecb7`).

**Method:** detached worktree at tip (`git worktree add --detach
/tmp/heddle-todo4-worktree HEAD`, `dirty=0`) so concurrent dirty WIP in the main
workspace could not poison the cert. Single full-suite run into
`artifacts/baseline/todo4-curated/`; gates + merge under
`artifacts/baseline/todo4-curated-merged/`.

| Job | Status | Duration | Oracle |
|-----|--------|----------|--------|
| facade-render-free | **pass** | 139 ms | no |
| fmt-check | **pass** | 12.5 s | no |
| git-process-lint | **pass** | 193.5 s* | yes |
| roundtrip-fidelity | **pass** | 8.6 s | yes |
| commit-conformance | **pass** | 5.4 s | yes |
| git-projection-engine | **pass** | 251.5 s* | yes |
| lib-objects | **pass** | 34.0 s | no |
| lib-refs | **pass** | 33.8 s | no |
| lib-oplog | **pass** | 10.3 s | no |
| lib-merge | **pass** | 16.8 s | no |
| lib-format | **pass** | 1.5 s | no |
| lib-crypto | **pass** | 13.0 s | no |
| lib-core | **pass** | 52.4 s | no |
| lib-repo | **pass** | 108.9 s* | no |
| lib-ingest | **pass** | 55.2 s | no |
| lib-git-projection | **pass** | 7.1 s | yes |
| cli-core-functionality | **pass** | 51.7 s | no |
| cli-state-management | **pass** | 52.8 s | no |
| formal-specs | **pass** | 5.2 s | yes |

\*May include compile into `CARGO_TARGET_DIR=/tmp/heddle-todo4-target` on first use of package.

**Aggregate curated:** **19 pass / 0 fail.** All oracle jobs green
(git-process-lint, roundtrip-fidelity, commit-conformance, git-projection-engine,
lib-git-projection, formal-specs). **fmt-check green.**

### Extra gates (same tip / target dir / clean checkout)

| Gate | Status | Notes |
|------|--------|-------|
| `cargo clippy -p heddle-core -p heddle-cli --locked -- -D warnings` | **pass** (exit 0) | clean; prior `assign_op_pattern` no longer present on tip |
| `cargo clippy -p heddle-core -p heddle-cli --locked` | **pass** | 0 warnings |
| `cargo doc -p heddle-core --no-deps --locked` | **pass** | |

**Release-gate checklist:** curated+oracles+fmt+clippy `-D`+doc **green** on
`a5b1dc68` (historical). Superseded by TODO R2 on `6a09ecb7`.

Logs: `artifacts/baseline/todo4-curated-merged/logs/clippy-*.log`,
`cargo-doc.log`. Runner stamp: `artifacts/baseline/todo4-curated/`.

## Wave 8 full curated cert (historical, 2026-07-11)

Source: `artifacts/baseline/wave-next-merged/summary.json` after curated jobs on
commit `d3db01439b5f245af9785871a2709786a88742b2` with
`CARGO_TARGET_DIR=/tmp/heddle-cert3-target`. Method: partial
`run-baseline.sh --suite curated` (7 jobs into `wave-next-full/`) then
sequential `--job` for remaining 12 (`wave-next-<id>/`); merged here.
**Superseded for tip authority by TODO #4** on `a5b1dc68`.

| Job | Status | Duration | Oracle |
|-----|--------|----------|--------|
| facade-render-free | **pass** | 31 ms | no |
| fmt-check | **pass** | 2.4 s | no |
| git-process-lint | **pass** | 72.3 s* | yes |
| roundtrip-fidelity | **pass** | 5.0 s | yes |
| commit-conformance | **pass** | 2.9 s | yes |
| git-projection-engine | **pass** | 109.2 s* | yes |
| lib-objects | **pass** | 92.3 s* | no |
| lib-refs | **pass** | 59.4 s* | no |
| lib-oplog | **pass** | 26.9 s* | no |
| lib-merge | **pass** | 52.6 s* | no |
| lib-format | **pass** | 7.9 s | no |
| lib-crypto | **pass** | 40.2 s* | no |
| lib-core | **pass** | 88.9 s* | no |
| lib-repo | **pass** | 153.8 s* | no |
| lib-ingest | **pass** | 101.0 s* | no |
| lib-git-projection | **pass** | 16.9 s | yes |
| cli-core-functionality | **pass** | 60.5 s* | no |
| cli-state-management | **pass** | 55.1 s* | no |
| formal-specs | **pass** | 3.5 s | yes |

\*May include compile into `CARGO_TARGET_DIR=/tmp/heddle-cert3-target` on first use of package.

**Aggregate curated:** **19 pass / 0 fail.** On that tip, clippy `-D warnings`
was a **fail** (`assign_op_pattern` in `status.rs:301`); cargo doc **pass**.

## Wave 8 partial cert — high-signal re-run (historical, 2026-07-11)

Source: `artifacts/baseline/post-wave-fanout2-merged/summary.json` after
`scripts/program/run-baseline.sh --job …` for each job below on commit
`b7f51aa4`.

| Job | Status | Duration | Oracle |
|-----|--------|----------|--------|
| facade-render-free | **pass** | 234 ms | no |
| fmt-check (`cargo +nightly fmt --check`) | **pass** | 29.9 s | no |
| git-process-lint | **pass** | 73.3 s* | yes |
| roundtrip-fidelity | **pass** | 5.5 s | yes |
| lib-core | **pass** | 26.6 s* | no |

\*Includes compile into `CARGO_TARGET_DIR=/tmp/heddle-cert-w8` on this run.

**Aggregate:** **5 pass / 0 fail.** Oracle jobs green (git-process-lint,
roundtrip-fidelity). **fmt-check green** via
`scripts/program/fmt-check.sh` → `cargo +nightly fmt --all -- --check`.

Perf: n=5 core-loop absolute + A==B self-pairs on release binary built from tip
`a5b1dc68` — `artifacts/perf/20260711T155225Z-*`, documented in
`docs/program/PERF_BASELINE.md`. Prior stamp `20260711T041555Z` retained for
comparison. Explicitly **not** a Git comparison.

## Post–Wave 2/3 high-signal re-cert (historical, 2026-07-11)

Source: `artifacts/baseline/post-wave23-merged/summary.json` after
`scripts/program/run-baseline.sh --job …` for each job below on commit
`b748bfd4` (includes Wave 2/3 facade work + `cargo +nightly fmt` apply at
`c1699119`).

| Job | Status | Duration | Oracle |
|-----|--------|----------|--------|
| facade-render-free | **pass** | 34 ms | no |
| fmt-check (`cargo +nightly fmt --check`) | **pass** | 3.1 s | no |
| git-process-lint | **pass** | 116.2 s* | yes |
| roundtrip-fidelity | **pass** | 48.8 s* | yes |
| commit-conformance | **pass** | 4.7 s | yes |
| lib-core | **pass** | 68.7 s* | no |
| formal-specs | **pass** | 4.8 s | yes |

\*Includes cold compile into `CARGO_TARGET_DIR=/tmp/heddle-cert-target` on this run.

**Aggregate:** **7 pass / 0 fail.** Superseded for tip re-cert authority by
later Wave 8 / TODO #4 stamps.

## Wave 0 oracle shard (historical, pre-fmt fix)

Source: `artifacts/baseline/wave0-merged/summary.json`.

| Job | Status | Duration | Oracle |
|-----|--------|----------|--------|
| facade-render-free | pass | 39 ms | no |
| fmt-check | **fail** (pre `c1699119`) | 3.4 s | no |
| git-process-lint | pass | 1.8 s | yes |
| roundtrip-fidelity | pass | 4.8 s | yes |
| commit-conformance | pass | 3.2 s | yes |
| lib-core | pass | 38.8 s | no |
| lib-format | pass | 3.5 s | no |
| lib-crypto | pass | 18.4 s | no |
| lib-merge | pass | 26.0 s | no |

**Aggregate (historical):** 8 pass / 1 fail (fmt only). Superseded for fmt by post-wave23 re-cert.

## Wave 1 domain + oracle expansion (same machine)

Additional jobs via `run-baseline.sh --job …` merged into
`artifacts/baseline/wave1-merged/summary.json`:

| Job | Status | Duration |
|-----|--------|----------|
| lib-objects | pass | 21.1 s |
| lib-refs | pass | 14.6 s |
| lib-oplog | pass | 8.0 s |
| lib-repo | pass | 103.9 s |
| lib-ingest | pass | 33.3 s |
| lib-git-projection | pass | 5.7 s |
| git-projection-engine | pass | 92.8 s |
| formal-specs | pass | 2.4 s |

**Combined curated-so-far (historical full merge):** 18 pass / 1 fail (`fmt-check` only).  
Oracle jobs all green in that stamp: git-process-lint, roundtrip-fidelity, commit-conformance, git-projection-engine, formal-specs, lib-git-projection.

Also green: `cli-core-functionality` (46.2 s), `cli-state-management` (45.7 s).

**Full curated suite in manifest (excluding suite=perf):** historical
`artifacts/baseline/curated-merged/summary.json` was 18/19 (fmt sole fail).
**Current authority:** `artifacts/baseline/todo4-curated-merged/summary.json` —
**19/19 pass** on `a5b1dc68` (supersedes `wave-next-merged` on `d3db0143`).

**fmt status after Wave 2/3:** fixed by `c1699119` (`cargo +nightly fmt --all`);
re-certified green in post-wave23, Wave 8, and TODO #4.

### Harness blockers / notes

1. **rustfmt:** `rustfmt.toml` requires nightly (`imports_granularity`, `group_imports`). Stable `cargo fmt` mis-formats the tree — never use it to “fix” the repo. Gate is `scripts/program/fmt-check.sh` (nightly only). **Verified green** on tip `a5b1dc68`.
2. **Full curated suite** re-run complete for TODO #4 on `a5b1dc68` (**19/19 pass**, `todo4-curated-merged`) via single `run-baseline.sh --suite curated` on a detached clean worktree. Sequential `--job` merge remains an accepted cert method under tool timeouts.
3. **Clippy `-D warnings`:** **pass** on `a5b1dc68` (prior `assign_op_pattern` blocker from Wave 8 stamp on `d3db0143` no longer present). Soft clippy also clean (0 warnings).
4. **Perf certification (≥5 trials)** re-run on tip `a5b1dc68` after open amortization — stamp `20260711T155225Z` (`docs/program/PERF_BASELINE.md` + `artifacts/perf/20260711T155225Z-*`). Still **not** a Git win claim; multi-host matrix open; host was moderately noisy at sample time.
5. **Dirty-tree isolation:** when other agents hold WIP under `crates/**`, cert against HEAD must use a clean checkout/worktree (`dirty=0`), not the main workspace worktree.

## Classification vocabulary (enforced by runner)

Results are never collapsed into a single pass rate without:

- `pass` / `fail` comparable
- `skip_prereq` (e.g. missing `git` for fixture builders)
- `todo_known` / ignored-only
- `timeout` / `setup_fail` / `aborted`
- `incomparable` (dry-run or unequal work)

## Performance baseline

**n=5 primary sample on tip `a5b1dc68` (cert trial count met on this host):**  
`docs/program/PERF_BASELINE.md` + `artifacts/perf/20260711T155225Z-*`.  
Absolute equal-work core-loop timings only. Explicitly **not** a Git win claim.
Prior Wave 8 stamp `20260711T041555Z` kept for comparison (status_json median
79.9 → 62.1 ms; still single-host, moderately noisy).

Existing budgets / other benches:

- `crates/cli/tests/cli_integration/perf_core_loop.rs` (ignored release smoke)
- Criterion benches under objects/refs/oplog/cli/mount/semantic
- Weekly `.github/workflows/benchmarks.yml`

**Still open for broader speed claims:** multi-host / platform matrix, quieter-host
re-run, and any cross-tool comparison with equal work and require-success.

## How to refresh

```bash
# Full curated suite on clean tip (preferred)
git worktree add --detach /tmp/heddle-todo4-worktree HEAD
cd /tmp/heddle-todo4-worktree
export CARGO_TARGET_DIR=/tmp/heddle-todo4-target
BASELINE_OUT_DIR=/path/to/main/artifacts/baseline/todo4-curated \
  bash scripts/program/run-baseline.sh --suite curated
# Extra gates (same clean tip / target dir)
cargo clippy -p heddle-core -p heddle-cli --locked -- -D warnings
cargo clippy -p heddle-core -p heddle-cli --locked
cargo doc -p heddle-core --no-deps --locked
# inspect artifacts/baseline/todo4-curated-merged/summary.json
# Perf cert sample (not re-run for TODO #4)
export CARGO_TARGET_DIR=/tmp/heddle-w8-target
cargo build --release -p heddle-cli --locked
bash scripts/program/core-loop-bench.sh --trials 5 --warmup 1 --out-dir artifacts/perf
```

Update this file’s tables after each integrated wave.
