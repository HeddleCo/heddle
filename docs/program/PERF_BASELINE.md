# Performance baseline (measurement calibration / cert sample)

**Status:** n=5 core-loop absolute timings re-stamped on residual fan-out tip
`c422950f` (Wave 6 measurement residual; absolute-only, no A==B pairs this run)  
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
   **n=5** post open-amortization run. Earlier stamps are retained under
   `artifacts/perf/` for historical comparison only.
5. **No budget gaming.** Budgets from `perf_core_loop.rs` are shown for
   context; this run does not lower budgets or skip ops to “pass.”
6. **Still not a Git comparison.** Absolute Heddle-only process times.
   Do **not** rephrase these numbers as a Git or Sley win.

---

## Environment (n=5 primary sample — residual fan-out tip)

| Field | Value |
|-------|-------|
| Commit | `c422950fb780cc53700a1c4749c15e63d76587cd` |
| Branch | `codex/correctness-architecture-performance-program` |
| Timestamp (UTC) | `20260711T195417Z` |
| OS | macOS 26.5.1 (Darwin 25.5.0 arm64) |
| CPU | Apple M1 Pro |
| Memory | 32 GB (`34359738368` bytes) |
| rustc | 1.97.0 (2d8144b78 2026-07-07) |
| cargo | 1.97.0 (c980f4866 2026-06-30) |
| Binary | `/tmp/heddle-fan-perf-target/release/heddle` (`heddle 0.10.0`) |
| Build | `CARGO_TARGET_DIR=/tmp/heddle-fan-perf-target cargo build --release -p heddle-cli --locked --features client` |
| Trials | 5 timed + 1 warmup; **absolute multi-op only** (`--no-paired`) |
| Host noise | Single-host residual-wave machine; not multi-host cert. Prefer quieter re-run + A==B pairs before external citation. |

Environment snapshot:
`artifacts/perf/20260711T195417Z-environment.txt`

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
export CARGO_TARGET_DIR=/tmp/heddle-fan-perf-target
cargo build --release -p heddle-cli --locked --features client

# 2) Absolute multi-op timings (5 trials, 1 warmup; absolute-only this stamp)
bash scripts/program/core-loop-bench.sh \
  --heddle /tmp/heddle-fan-perf-target/release/heddle \
  --trials 5 \
  --warmup 1 \
  --no-paired \
  --out-dir "$PWD/artifacts/perf"

# Optional full stamp with A==B self-pairs (omit --no-paired):
# bash scripts/program/core-loop-bench.sh --heddle ... --trials 5 --warmup 1

# Optional: manual paired A/B between two commands on the same fixture
python3 scripts/program/paired-bench.py --help
```

Manifest opt-in suite entry: `suite = "perf"` job `core-loop-absolute-bench`
in `scripts/program/manifest.toml`.

---

## Absolute timings (n=5, 1 warmup, require success)

Source:
`artifacts/perf/20260711T195417Z-core-loop-absolute.json`

Times are **milliseconds** of process wall clock. Median, p95, and p99 are the
headline numbers for later regression comparison.

| Operation | argv (relative) | median_ms | p95_ms | p99_ms | mean_ms | stdev_ms | smoke budget_ms |
|-----------|-----------------|----------:|-------:|-------:|--------:|---------:|----------------:|
| bare_help | `heddle` | 13.6 | 15.0 | 15.2 | 13.4 | 1.3 | 250 |
| help | `heddle help` | 13.1 | 15.0 | 15.1 | 13.4 | 1.2 | 250 |
| status_text | `heddle status` | 40.4 | 48.9 | 50.5 | 41.7 | 5.3 | 650 |
| status_short | `heddle status --short` | 37.4 | 42.0 | 42.5 | 37.9 | 3.3 | 650 |
| status_json | `heddle --output json status` | 126.1 | 153.7 | 155.6 | 126.3 | 24.3 | 850 |
| log_json | `heddle --output json log` | 17.3 | 22.0 | 22.0 | 18.9 | 2.8 | 850 |
| diff_json | `heddle --output json diff` | 43.7 | 47.8 | 47.9 | 42.3 | 5.7 | 1000 |
| thread_list_json | `heddle --output json thread list` | 57.2 | 65.7 | 66.3 | 58.0 | 6.5 | 850 |

Smoke budgets are from `perf_core_loop.rs` (single-run upper bounds on this
hardware class). All **medians** remain under those budgets. Some **p95/p99**
values for `status_json` are elevated on this host (noise / `--features client`
release binary); that is **not** treated as a budget failure for this
calibration stamp and is **not** a Git comparison.

### Comparison vs prior primary stamp `20260711T155225Z`

Prior primary (commit `a5b1dc68…`, absolute-only comparison). Same fixture
recipe, same host class. Median deltas (this stamp − prior primary):

| Operation | prior median_ms | this median_ms | Δ ms | Δ % |
|-----------|----------------:|---------------:|-----:|----:|
| bare_help | 12.0 | 13.6 | +1.6 | +13% |
| help | 12.7 | 13.1 | +0.4 | +3% |
| status_text | 27.2 | 40.4 | +13.2 | +49% |
| status_short | 26.3 | 37.4 | +11.1 | +42% |
| status_json | 62.1 | 126.1 | +64.0 | +103% |
| log_json | 14.3 | 17.3 | +3.0 | +21% |
| diff_json | 24.9 | 43.7 | +18.8 | +76% |
| thread_list_json | 31.5 | 57.2 | +25.7 | +82% |

Interpretation (still **not** a Git win claim, **not** a regression verdict):

- This tip ships residual waves + `--features client` release binary; prior
  stamp was a quieter post open-amortization sample with A==B pairs.
- Elevated repo-touching medians are consistent with **host noise and binary
  configuration differences**, not an equal-work before/after perf optimization.
- Prefer quieter multi-host re-run with A==B pairs before treating deltas as
  actionable regressions.

### Raw trial times (absolute series, ms)

| Operation | t0 | t1 | t2 | t3 | t4 |
|-----------|---:|---:|---:|---:|---:|
| bare_help | 13.6 | 13.9 | 12.1 | 15.2 | 12.1 |
| help | 15.2 | 12.5 | 14.2 | 13.1 | 12.2 |
| status_text | 50.9 | 38.6 | 40.6 | 40.4 | 37.9 |
| status_short | 34.9 | 42.7 | 39.6 | 34.8 | 37.4 |
| status_json | 156.0 | 105.3 | 99.8 | 126.1 | 144.5 |
| log_json | 16.8 | 22.1 | 17.3 | 21.9 | 16.4 |
| diff_json | 48.0 | 47.1 | 35.2 | 43.7 | 37.7 |
| thread_list_json | 52.2 | 51.5 | 62.5 | 57.2 | 66.5 |

---

## Paired self-pairs (A==B)

**Not re-run** for stamp `20260711T195417Z` (`--no-paired` absolute-only). Prior
A==B artifacts remain under `artifacts/perf/20260711T155225Z-core-loop-paired-*.json`
as harness calibration only (not a win claim).

---

## Historical samples (superseded for primary cert stamp)

| Stamp | Commit | Trials | Role |
|-------|--------|-------:|------|
| `20260711T200938Z` | `fe4d129e…` | 3 | Calibration A==B + absolute (n=3; not cert n=5) |
| `20260711T195417Z` | `c422950f…` | 5 | **Primary** residual fan-out tip (absolute-only) |
| `20260711T155225Z` | `a5b1dc68…` | 5 | Prior primary post open-amortization (+ A==B pairs) |
| `20260711T041555Z` | `b7f51aa4…` | 5 | Prior Wave 8 cert sample (noisier concurrent cargo) |
| `20260711T032344Z` | `c1699119…` | 3 | Wave 1 measurement foundation only |

Do not use n=3 as the certification trial count. Prefer
`20260711T195417Z` for tip authority going forward; use quieter multi-host +
A==B pairs before treating median deltas as actionable regressions.

---

## How to refresh

```bash
cargo build --release -p heddle-cli --locked
# certification-oriented (≥5 trials):
bash scripts/program/core-loop-bench.sh --trials 5 --warmup 1
# quick calibration:
bash scripts/program/core-loop-bench.sh --trials 3 --warmup 1
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

- Single-host sample under residual concurrent load (load ≈13 at bench start);
  re-run on a quiet machine before citing numbers externally.
- Process spawn overhead dominates the fastest ops (`help` ~13 ms).
- Fixture is synthetic equal-work, not a large monorepo or realworld Git import.
- No Git comparison was performed; do not rephrase these numbers as “faster
  than Git.”
- Open amortization is visible in phase profiles (`repo_open_ms` ≈0–1,
  `plain_git_probe_ms` = 0); remaining status cost is largely
  `thread_summary_ms` on this 24-thread fixture.
- Full multi-host / platform matrix (Wave 7) still open.
