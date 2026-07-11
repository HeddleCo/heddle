# Performance baseline (measurement calibration / cert sample)

**Status:** n=5 core-loop absolute timings recorded (Wave 8 partial cert)  
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
   **n=5** Wave 8 run. An earlier n=3 calibration stamp is retained only as
   historical comparison under `artifacts/perf/20260711T032344Z-*`.
5. **No budget gaming.** Budgets from `perf_core_loop.rs` are shown for
   context; this run does not lower budgets or skip ops to “pass.”
6. **Still not a Git comparison.** Absolute Heddle-only process times.

---

## Environment (n=5 Wave 8 cert sample)

| Field | Value |
|-------|-------|
| Commit | `b7f51aa4a505c69c3dea5831ed44069f954b36c3` |
| Branch | `codex/correctness-architecture-performance-program` |
| Timestamp (UTC) | `20260711T041555Z` |
| OS | macOS 26.5.1 (Darwin 25.5.0 arm64) |
| CPU | Apple M1 Pro |
| Memory | 32 GB (`34359738368` bytes) |
| rustc | 1.97.0 (2d8144b78 2026-07-07) |
| cargo | 1.97.0 (c980f4866 2026-06-30) |
| Binary | `target/release/heddle` (`heddle 0.10.0`) |
| Binary SHA-256 | `ff84f3e1f1e281e777c02d94c2d5a68066c913ab334d05c12485b657fdc2ac1a` |
| Build | `CARGO_TARGET_DIR=/tmp/heddle-w8-target cargo build --release -p heddle-cli --locked` then copy into `target/release/heddle` and `artifacts/perf/bin/heddle` |

Environment snapshot:
`artifacts/perf/20260711T041555Z-environment.txt`

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
# 1) Release binary (isolated target used on this host due to disk contention)
export CARGO_TARGET_DIR=/tmp/heddle-w8-target
cargo build --release -p heddle-cli --locked
cp -f "$CARGO_TARGET_DIR/release/heddle" target/release/heddle
cp -f "$CARGO_TARGET_DIR/release/heddle" artifacts/perf/bin/heddle

# 2) Absolute multi-op timings + paired A==B self-pairs (5 trials, 1 warmup)
bash scripts/program/core-loop-bench.sh \
  --heddle "$PWD/target/release/heddle" \
  --trials 5 \
  --warmup 1 \
  --out-dir "$PWD/artifacts/perf"

# Optional: manual paired A/B between two commands on the same fixture
python3 scripts/program/paired-bench.py --help
```

Manifest opt-in suite entry: `suite = "perf"` job `core-loop-absolute-bench`
in `scripts/program/manifest.toml`.

---

## Absolute timings (n=5, 1 warmup, require success)

Source:
`artifacts/perf/20260711T041555Z-core-loop-absolute.json`

Times are **milliseconds** of process wall clock. Median, p95, and p99 are the
headline numbers for later regression comparison.

| Operation | argv (relative) | median_ms | p95_ms | p99_ms | mean_ms | stdev_ms | smoke budget_ms |
|-----------|-----------------|----------:|-------:|-------:|--------:|---------:|----------------:|
| bare_help | `heddle` | 13.1 | 17.9 | 18.1 | 14.3 | 3.0 | 250 |
| help | `heddle help` | 13.1 | 13.8 | 13.8 | 12.9 | 1.0 | 250 |
| status_text | `heddle status` | 34.5 | 42.0 | 43.5 | 35.0 | 5.4 | 650 |
| status_short | `heddle status --short` | 30.6 | 34.3 | 34.8 | 31.2 | 2.4 | 650 |
| status_json | `heddle --output json status` | 79.9 | 86.4 | 87.5 | 79.7 | 5.3 | 850 |
| log_json | `heddle --output json log` | 16.7 | 24.9 | 25.9 | 18.6 | 4.6 | 850 |
| diff_json | `heddle --output json diff` | 30.2 | 32.6 | 32.7 | 30.9 | 1.5 | 1000 |
| thread_list_json | `heddle --output json thread list` | 46.2 | 55.2 | 56.9 | 47.0 | 6.6 | 850 |

Smoke budgets are from `perf_core_loop.rs` (single-run upper bounds on this
hardware class). All medians, p95s, and p99s in this n=5 run are **under**
those budgets. That is expected for a warm release binary on Apple M1 Pro; it
is **not** evidence that the budgets should be tightened without multi-host
data. Host was under concurrent cargo load during this cert pass; treat
absolute ms as noisy single-host samples.

### Raw trial times (absolute series, ms)

| Operation | t0 | t1 | t2 | t3 | t4 |
|-----------|---:|---:|---:|---:|---:|
| bare_help | 12.4 | 11.2 | 13.1 | 18.2 | 16.8 |
| help | 13.1 | 11.5 | 13.8 | 13.7 | 12.7 |
| status_text | 33.1 | 29.1 | 34.5 | 43.9 | 34.5 |
| status_short | 30.6 | 28.7 | 31.8 | 34.9 | 29.9 |
| status_json | 79.9 | 87.7 | 80.8 | 73.9 | 76.4 |
| log_json | 15.6 | 16.7 | 26.1 | 14.6 | 19.8 |
| diff_json | 29.4 | 30.2 | 29.8 | 32.7 | 32.2 |
| thread_list_json | 45.7 | 47.0 | 57.3 | 38.8 | 46.2 |

---

## Paired self-pairs (A==B, alternating, n=5)

Also produced by `core-loop-bench.sh` via `scripts/program/paired-bench.py`
(alternating A/B, require success, median/mean/p95/p99/stdev). A and B are the
**same** command — used to exercise the paired harness and estimate
run-to-run noise, not to claim a win.

| Op | Artifact | A median_ms | A p95_ms | B median_ms | B p95_ms | ratio B/A |
|----|----------|------------:|---------:|------------:|--------:|----------:|
| status_json | `…-core-loop-paired-status_json.json` | 88.9 | 105.1 | 91.0 | 104.7 | 1.023 |
| log_json | `…-core-loop-paired-log_json.json` | 22.5 | 29.2 | 25.9 | 31.3 | 1.151 |
| diff_json | `…-core-loop-paired-diff_json.json` | 40.5 | 50.9 | 44.2 | 50.0 | 1.092 |
| help | `…-core-loop-paired-help.json` | 20.3 | 24.7 | 20.8 | 30.1 | 1.023 |

Prefix: `artifacts/perf/20260711T041555Z-`

Self-pair ratios near 1.0 indicate the alternating harness is balanced under
quiet conditions; ratios here reflect concurrent machine load and are still
**not** cross-implementation comparisons. Prefer absolute-series median/p95 for
regression baselines when A==B noise is elevated.

---

## Historical n=3 calibration (superseded for cert count)

Earlier Wave 1 measurement foundation stamp (n=3 only):
`artifacts/perf/20260711T032344Z-core-loop-absolute.json` on commit
`c1699119855c242566e8677affef78f8f8fa1a71`. Kept for harness continuity; do not
use n=3 as the certification trial count.

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

- Single-host sample under concurrent cargo load; re-run on a quiet machine
  before citing numbers externally.
- Process spawn overhead dominates the fastest ops (`help` ~13 ms).
- Fixture is synthetic equal-work, not a large monorepo or realworld Git import.
- No Git comparison was performed; do not rephrase these numbers as “faster
  than Git.”
- Full multi-host / platform matrix (Wave 7) still open.
