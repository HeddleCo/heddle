# Performance baseline (measurement calibration)

**Status:** measurement calibration only  
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
4. **n=3 is calibration, not certification.** Release certification wants ≥5
   paired trials per `docs/program/RELEASE_GATES.md` (G5). This wave records
   ≥3 successful trials with full raw JSON.
5. **No budget gaming.** Budgets from `perf_core_loop.rs` are shown for
   context; this run does not lower budgets or skip ops to “pass.”

---

## Environment

| Field | Value |
|-------|-------|
| Commit | `c1699119855c242566e8677affef78f8f8fa1a71` |
| Branch | `codex/correctness-architecture-performance-program` |
| Timestamp (UTC) | `20260711T032344Z` |
| OS | macOS 26.5.1 (Darwin 25.5.0 arm64) |
| CPU | Apple M1 Pro |
| Memory | 32 GB (`34359738368` bytes) |
| rustc | 1.97.0 (2d8144b78 2026-07-07) |
| cargo | 1.97.0 (c980f4866 2026-06-30) |
| Binary | `target/release/heddle` (`heddle 0.10.0`) |
| Binary SHA-256 | `8e3fd8c718b239fa9a6ffa7684f9beccf49d95a9b0c59d296042bea5f221d5da` |
| Build | `CARGO_TARGET_DIR=/tmp/heddle-perf-release-target cargo build --release -p heddle-cli --locked` then copy into `target/release/heddle` |

Environment snapshot:
`artifacts/perf/20260711T032344Z-environment.txt`

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
export CARGO_TARGET_DIR=/tmp/heddle-perf-release-target
cargo build --release -p heddle-cli --locked
cp -f "$CARGO_TARGET_DIR/release/heddle" target/release/heddle

# 2) Absolute multi-op timings + paired A==B self-pairs (3 trials, 1 warmup)
bash scripts/program/core-loop-bench.sh \
  --heddle "$PWD/target/release/heddle" \
  --trials 3 \
  --warmup 1 \
  --out-dir "$PWD/artifacts/perf"

# Optional: manual paired A/B between two commands on the same fixture
python3 scripts/program/paired-bench.py --help
```

Manifest opt-in suite entry: `suite = "perf"` job `core-loop-absolute-bench`
in `scripts/program/manifest.toml`.

---

## Absolute timings (n=3, 1 warmup, require success)

Source:
`artifacts/perf/20260711T032344Z-core-loop-absolute.json`

Times are **milliseconds** of process wall clock. Median and p95 are the
headline numbers for later regression comparison.

| Operation | argv (relative) | median_ms | p95_ms | mean_ms | stdev_ms | smoke budget_ms |
|-----------|-----------------|----------:|-------:|--------:|---------:|----------------:|
| bare_help | `heddle` | 11.2 | 12.5 | 11.6 | 0.9 | 250 |
| help | `heddle help` | 11.5 | 11.5 | 11.5 | 0.1 | 250 |
| status_text | `heddle status` | 25.3 | 25.5 | 25.3 | 0.2 | 650 |
| status_short | `heddle status --short` | 23.6 | 24.2 | 23.5 | 0.9 | 650 |
| status_json | `heddle --output json status` | 58.6 | 60.0 | 59.0 | 0.9 | 850 |
| log_json | `heddle --output json log` | 14.1 | 15.4 | 14.3 | 1.1 | 850 |
| diff_json | `heddle --output json diff` | 23.7 | 24.0 | 23.7 | 0.3 | 1000 |
| thread_list_json | `heddle --output json thread list` | 31.3 | 36.1 | 32.9 | 3.2 | 850 |

Smoke budgets are from `perf_core_loop.rs` (single-run upper bounds on this
hardware class). All medians and p95s in this calibration run are **under**
those budgets. That is expected for a warm release binary on Apple M1 Pro; it
is **not** evidence that the budgets should be tightened without multi-host
data.

### Raw trial medians (absolute series)

| Operation | trial0_ms | trial1_ms | trial2_ms |
|-----------|----------:|----------:|----------:|
| bare_help | 10.9 | 12.6 | 11.2 |
| help | 11.6 | 11.5 | 11.4 |
| status_text | 25.3 | 25.1 | 25.5 |
| status_short | 24.3 | 23.6 | 22.6 |
| status_json | 58.4 | 58.6 | 60.1 |
| log_json | 13.3 | 14.1 | 15.5 |
| diff_json | 23.4 | 24.0 | 23.7 |
| thread_list_json | 31.3 | 36.6 | 30.7 |

---

## Paired self-pairs (A==B, alternating)

Also produced by `core-loop-bench.sh` via `scripts/program/paired-bench.py`
(alternating A/B, require success, median/mean/p95/p99/stdev). A and B are the
**same** command — used to exercise the paired harness and estimate
run-to-run noise, not to claim a win.

| Op | Artifact | A median_ms | A p95_ms | B median_ms | B p95_ms | ratio B/A |
|----|----------|------------:|---------:|------------:|--------:|----------:|
| status_json | `…-core-loop-paired-status_json.json` | 64.4 | 66.3 | 64.1 | 64.2 | 0.996 |
| log_json | `…-core-loop-paired-log_json.json` | 18.8 | 21.5 | 18.5 | 19.7 | 0.986 |
| diff_json | `…-core-loop-paired-diff_json.json` | 29.8 | 34.3 | 29.5 | 31.4 | 0.990 |
| help | `…-core-loop-paired-help.json` | 16.3 | 16.5 | 16.0 | 16.4 | 0.978 |

Prefix: `artifacts/perf/20260711T032344Z-`

Self-pair ratios near 1.0 indicate the alternating harness is balanced; they
are **not** cross-implementation comparisons.

---

## How to refresh

```bash
cargo build --release -p heddle-cli --locked
bash scripts/program/core-loop-bench.sh --trials 3 --warmup 1
# certification-oriented:
bash scripts/program/core-loop-bench.sh --trials 5 --warmup 1
# update this doc’s tables from the new absolute JSON
```

---

## Harness components

| Piece | Path | Role |
|-------|------|------|
| Paired alternating runner | `scripts/program/paired-bench.py` | A/B wall times; mean/median/p95/p99/stdev; require-success default |
| Equal-work multi-op runner | `scripts/program/core-loop-bench.sh` | Fixture + absolute timings + optional self-pairs |
| Smoke budgets (ignored test) | `crates/cli/tests/cli_integration/perf_core_loop.rs` | Single-run budget smoke, not CI gate |
| Manifest suite | `scripts/program/manifest.toml` `suite=perf` | Opt-in curated perf jobs |
| Raw artifacts | `artifacts/perf/` | JSON + environment snapshot |

---

## Remaining risks / limits

- **n=3** is insufficient for formal certification (want ≥5).
- Process spawn overhead dominates the fastest ops (`help` ~11 ms).
- Host was under concurrent cargo load and low free disk during the program
  wave; release binary was built with an isolated `CARGO_TARGET_DIR` then
  copied. Re-run on a quiet machine before citing numbers externally.
- Fixture is synthetic equal-work, not a large monorepo or realworld Git import.
- No Git comparison was performed; do not rephrase these numbers as “faster
  than Git.”
