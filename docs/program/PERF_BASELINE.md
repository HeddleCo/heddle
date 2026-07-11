# Performance baseline (measurement calibration / cert sample)

**Status:** n=5 core-loop absolute timings re-stamped on tip `34c101ea` (Wave 6
measurement residual) **with A==B self-pairs** (`20260711T210616Z`).
**Not a claim:** this document does **not** assert a Git win, Sley win, or any
cross-tool superiority. It records absolute wall-clock process times on a
fixed equal-work Heddle fixture so later waves can detect regressions with
paired trials.

Raw machine-readable results live under `artifacts/perf/`.

---

## Disclaimer (read first)

1. **Equal work only.** Fixture recipe matches
   `crates/cli/tests/cli_integration/perf_core_loop.rs::setup_core_loop_fixture`
   (300 even-spread files, seed capture, 24 threads, one dirty tracked file).
2. **Require success.** Failed commands abort the run; timings never include
   skipped work, early-exit failures, or missing features.
3. **Absolute process wall times.** Each sample is end-to-end `heddle` process
   lifetime (including process start). stdout discarded for timing purity;
   exit code must be 0.
4. **n≥5 for certification sample.** Release certification wants ≥5 trials per
   `docs/program/RELEASE_GATES.md` (G5). This document’s primary table is the
   **n=5** tip re-stamp. Earlier stamps are retained under `artifacts/perf/`
   for historical comparison only.
5. **No budget gaming.** Budgets from `perf_core_loop.rs` are shown for
   context; this run does not lower budgets or skip ops to “pass.”
6. **Still not a Git comparison.** Absolute Heddle-only process times.
   Do **not** rephrase these numbers as a Git or Sley win.

---

## Environment (n=5 primary sample — tip `34c101ea`)

| Field | Value |
|-------|-------|
| Commit | `34c101ea951358120e6d2f13b22f4551c2845df2` |
| Branch | `codex/correctness-architecture-performance-program` |
| Timestamp (UTC) | `20260711T210616Z` |
| OS | macOS 26.5.1 (Darwin 25.5.0 arm64) |
| CPU | Apple M1 Pro |
| Memory | 32 GB (`34359738368` bytes) |
| rustc | 1.97.0 (2d8144b78 2026-07-07) |
| cargo | 1.97.0 (c980f4866 2026-06-30) |
| Binary | `/tmp/heddle-w6-perf-target/release/heddle` (`heddle 0.10.0`) |
| Build | `CARGO_TARGET_DIR=/tmp/heddle-w6-perf-target cargo build --release -p heddle-cli --locked --features client` |
| Trials | 5 timed + 1 warmup; **absolute multi-op + A==B self-pairs** |
| Host noise | Single-host residual-wave machine; **not** multi-host cert. Multi-host matrix still open. |

Environment snapshot:
`artifacts/perf/20260711T210616Z-environment.txt`

---

## Equal-work fixture recipe

Implemented by `scripts/program/core-loop-bench.sh` (must stay aligned with the
Rust smoke fixture):

1. `heddle init --principal-name perf-baseline --principal-email perf-baseline@heddle.local`
2. Write **300** files: `tracked-{index%20:02d}/file-{index:03d}.txt` with body
   `fixture file {index}\n` + 80× `x` + newline
3. `heddle capture -m seed`
4. Create **24** threads: `perf/thread-00` … `perf/thread-23`
5. Dirty one tracked file: write `dirty\n` to `tracked-00/file-000.txt`
6. Isolated `HEDDLE_CONFIG` under the fixture; `HEDDLE_PROFILE` unset

---

## Exact commands used for this baseline

```bash
# 1) Release binary (isolated target)
export CARGO_TARGET_DIR=/tmp/heddle-w6-perf-target
cargo build --release -p heddle-cli --locked --features client

# 2) Absolute multi-op timings + A==B self-pairs (5 trials, 1 warmup)
bash scripts/program/core-loop-bench.sh \
  --heddle /tmp/heddle-w6-perf-target/release/heddle \
  --trials 5 \
  --warmup 1 \
  --out-dir "$PWD/artifacts/perf"

# Optional: absolute-only (omit paired):
# bash scripts/program/core-loop-bench.sh ... --no-paired

# Optional: manual paired A/B between two commands on the same fixture
python3 scripts/program/paired-bench.py --help
```

Manifest opt-in suite entry: `suite = "perf"` job `core-loop-absolute-bench`
in `scripts/program/manifest.toml`.

---

## Absolute timings (n=5, 1 warmup, require success)

Source:
`artifacts/perf/20260711T210616Z-core-loop-absolute.json`

Times are **milliseconds** of process wall clock. Median, p95, and p99 are the
headline numbers for later regression comparison.

| Operation | argv (relative) | median_ms | p95_ms | p99_ms | mean_ms | stdev_ms | smoke budget_ms |
|-----------|-----------------|----------:|-------:|-------:|--------:|---------:|----------------:|
| bare_help | `heddle` | 9.3 | 9.7 | 9.7 | 9.4 | 0.2 | 250 |
| help | `heddle help` | 10.1 | 19.6 | 20.7 | 12.8 | 5.0 | 250 |
| status_text | `heddle status` | 22.0 | 23.3 | 23.4 | 22.2 | 0.8 | 650 |
| status_short | `heddle status --short` | 21.3 | 22.1 | 22.2 | 21.3 | 0.7 | 650 |
| status_json | `heddle --output json status` | 52.9 | 54.1 | 54.3 | 53.2 | 0.7 | 850 |
| log_json | `heddle --output json log` | 11.4 | 12.4 | 12.5 | 11.7 | 0.5 | 850 |
| diff_json | `heddle --output json diff` | 20.6 | 21.3 | 21.4 | 20.5 | 0.7 | 1000 |
| thread_list_json | `heddle --output json thread list` | 26.4 | 26.5 | 26.5 | 26.4 | 0.1 | 850 |

Smoke budgets are from `perf_core_loop.rs` (single-run upper bounds on this
hardware class). All **medians** remain under those budgets. This stamp is
quieter than the prior residual fan-out absolute-only sample (`20260711T195417Z`).

### Comparison vs prior primary stamp `20260711T195417Z`

Prior primary (commit `c422950f…`, absolute-only, noisier host load). Same
fixture recipe, same host class. Median deltas (this stamp − prior primary):

| Operation | prior median_ms | this median_ms | Δ ms | Δ % |
|-----------|----------------:|---------------:|-----:|----:|
| bare_help | 13.6 | 9.3 | -4.3 | -32% |
| help | 13.1 | 10.1 | -3.0 | -23% |
| status_text | 40.4 | 22.0 | -18.4 | -46% |
| status_short | 37.4 | 21.3 | -16.1 | -43% |
| status_json | 126.1 | 52.9 | -73.2 | -58% |
| log_json | 17.3 | 11.4 | -5.9 | -34% |
| diff_json | 43.7 | 20.6 | -23.1 | -53% |
| thread_list_json | 57.2 | 26.4 | -30.8 | -54% |

Interpretation (still **not** a Git win claim, **not** a hotspot optimization claim):

- Same equal-work recipe and `--features client` release binary class; this
  re-stamp is primarily a **quieter measurement residual** with A==B pairs.
- Large negative median deltas vs `195417Z` are consistent with host noise
  differences between residual concurrent load and a quieter run — treat as
  calibration, not product speed wins.
- Multi-host / quieter-host matrix remains **open** for external citation.

### Raw trial times (absolute series, ms)

| Operation | t0 | t1 | t2 | t3 | t4 |
|-----------|---:|---:|---:|---:|---:|
| bare_help | 9.3 | 9.4 | 9.2 | 9.8 | 9.2 |
| help | 10.1 | 9.1 | 9.6 | 14.3 | 20.9 |
| status_text | 22.0 | 23.5 | 21.3 | 22.6 | 21.9 |
| status_short | 21.8 | 22.2 | 20.8 | 20.7 | 21.3 |
| status_json | 53.3 | 54.3 | 52.9 | 52.9 | 52.7 |
| log_json | 12.0 | 11.4 | 12.6 | 11.4 | 11.3 |
| diff_json | 19.7 | 20.6 | 19.9 | 21.0 | 21.4 |
| thread_list_json | 26.5 | 26.5 | 26.2 | 26.4 | 26.4 |

---

## Paired self-pairs (A==B)

Stamp `20260711T210616Z` ran A==B self-pairs (identical command A and B,
alternating thermal control). Median ratio ≈1.0 indicates harness stability,
not a product A/B comparison.

Source: `artifacts/perf/20260711T210616Z-core-loop-paired-*.json`

| Op | A median_ms | B median_ms | A/B ratio |
|----|------------:|------------:|----------:|
| diff_json | 24.7 | 24.9 | 0.992 |
| help | 22.8 | 21.6 | 1.059 |
| log_json | 15.5 | 15.9 | 0.979 |
| status_json | 58.8 | 59.4 | 0.990 |

Harness calibration only — **not** a Git win claim.

---

## Historical samples (superseded for primary cert stamp)

| Stamp | Commit | Trials | Role |
|-------|--------|-------:|------|
| `20260711T210616Z` | `34c101ea…` | 5 | **Primary** tip re-stamp (absolute + A==B) |
| `20260711T200938Z` | `fe4d129e…` | 3 | Calibration A==B + absolute (n=3; not cert n=5) |
| `20260711T195417Z` | `c422950f…` | 5 | Prior residual fan-out tip (absolute-only) |
| `20260711T155225Z` | `a5b1dc68…` | 5 | Prior post open-amortization (+ A==B pairs) |
| `20260711T041555Z` | `b7f51aa4…` | 5 | Prior Wave 8 cert sample (noisier concurrent cargo) |
| `20260711T032344Z` | `c1699119…` | 3 | Wave 1 measurement foundation only |

Do not use n=3 as the certification trial count. Prefer `20260711T210616Z`
for tip authority going forward; multi-host samples still required before
external speed claims.

---

## How to refresh

```bash
export CARGO_TARGET_DIR=/tmp/heddle-w6-perf-target
cargo build --release -p heddle-cli --locked --features client
# certification-oriented (≥5 trials + A==B):
bash scripts/program/core-loop-bench.sh \
  --heddle /tmp/heddle-w6-perf-target/release/heddle \
  --trials 5 --warmup 1 --out-dir "$PWD/artifacts/perf"
# update this doc’s tables from the new absolute JSON
```

---

## Harness components

| Piece | Path | Role |
|-------|------|------|
| Paired alternating runner | `scripts/program/paired-bench.py` | A/B wall times; mean/median/p95/p99/stdev; require-success default |
| Equal-work multi-op runner | `scripts/program/core-loop-bench.sh` | Fixture + absolute timings + optional self-pairs (`--trials` ≥1, incl. 5) |
| Smoke budgets (ignored test) | `crates/cli/tests/cli_integration/perf_core_loop.rs` | Single-run budget smoke, not CI gate |
| Manifest suite | `scripts/program/manifest.toml` `suite=perf` | Opt-in curated perf jobs |
| Raw artifacts | `artifacts/perf/` | JSON + environment snapshot |

---

## Remaining risks / limits

- Single-host sample; multi-host matrix still **open** (prep:
  [`MULTI_HOST_PERF.md`](MULTI_HOST_PERF.md), living table
  [`MULTI_HOST_PERF_MATRIX.md`](MULTI_HOST_PERF_MATRIX.md)).
- Process spawn overhead dominates the fastest ops (`help` ~10–20 ms).
- Fixture is synthetic equal-work, not a large monorepo or realworld Git import.
- No Git comparison was performed; do not rephrase these numbers as “faster
  than Git.”
- Wave 6 hotspot *code* work remains optional and requires paired before/after
  on equal-work for any win claim.
- Full multi-host / platform matrix (Wave 7 residual) still open.
