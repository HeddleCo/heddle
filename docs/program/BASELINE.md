# Trustworthy baseline record

## Environment (this machine)

| Field | Value |
|-------|-------|
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
| CARGO_TARGET_DIR (TODO #4 re-cert) | `/tmp/heddle-todo4-target` |
| CARGO_TARGET_DIR (Wave 8 full curated) | `/tmp/heddle-cert3-target` |
| CARGO_TARGET_DIR (Wave 8 high-signal) | `/tmp/heddle-cert-w8` |
| CARGO_TARGET_DIR (release perf binary) | `/tmp/heddle-todo5-target` (primary); prior `/tmp/heddle-w8-target` |
| Isolation (TODO #4) | detached git worktree `/tmp/heddle-todo4-worktree` at tip (`dirty=0`); artifacts under main workspace |
| Isolation (prior) | workspace (`/Users/lukethorne/dev/HeddleCo/workspace/session-2026-07-10`) |

## Harness status

| Capability | Status |
|------------|--------|
| Curated manifest | **Shipped** — `scripts/program/manifest.toml` |
| Baseline runner + classification | **Shipped** — `scripts/program/run-baseline.sh` |
| Paired bench runner | **Shipped** — `scripts/program/paired-bench.py` |
| CLI residual inventory | **Shipped** — `scripts/program/gen-cli-domain-residual.py` |
| TODO #4 full curated re-cert (19 jobs) | **Green** — 19/19 pass on `a5b1dc68`; see `artifacts/baseline/todo4-curated-merged/summary.json` |
| Wave 8 full curated (19 jobs) | **Green** (historical) — 19/19 pass on `d3db0143`; see `artifacts/baseline/wave-next-merged/summary.json` |
| High-signal Wave 8 re-cert (5 jobs) | **Green** — 5/5 pass; see `artifacts/baseline/post-wave-fanout2-merged/summary.json` |
| High-signal post–Wave 2/3 re-cert (7 jobs) | **Green** — 7/7 pass; see `artifacts/baseline/post-wave23-merged/summary.json` |
| Full curated suite (19 jobs) | **Current stamp** — 19 pass / 0 fail (`todo4-curated-merged` on `a5b1dc68`); supersedes `wave-next-merged` |
| Clippy (`-D warnings`) | **Pass** on `a5b1dc68` (prior `assign_op_pattern` blocker cleared on tip) |
| Clippy (soft, no `-D`) | **Pass** — 0 warnings |
| `cargo doc -p heddle-core --no-deps` | **Pass** |
| Performance certification (5 trials) | **Recorded** — n=5 absolute + paired self-pairs on tip `a5b1dc68` stamp `20260711T155225Z`; see `docs/program/PERF_BASELINE.md` (**not** a Git win claim) |

## TODO #4 full curated re-cert (2026-07-11, this machine) — **current authority**

Source: `artifacts/baseline/todo4-curated-merged/summary.json` after
`bash scripts/program/run-baseline.sh --suite curated` on commit
`a5b1dc689c755228be15cefeaffd91dbb9dd18f3` with
`CARGO_TARGET_DIR=/tmp/heddle-todo4-target`.

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

**Release-gate checklist:** curated+oracles+fmt+clippy `-D`+doc **green**.
Perf n=5 prior stamp retained; multi-host still open. **No blockers** from this
cert pass on tip `a5b1dc68`.

Logs: `artifacts/baseline/todo4-curated-merged/logs/clippy-*.log`,
`cargo-doc.log`. Runner stamp: `artifacts/baseline/todo4-curated/`.

**Note:** An aborted first attempt against the main workspace dirty tree saw
WIP compile/fmt noise (other agents editing `crates/**`). That attempt is **not**
authoritative; only the clean detached worktree run above is the cert stamp.

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
