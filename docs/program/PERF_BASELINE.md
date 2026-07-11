# Performance baseline (measurement calibration / cert sample)

**Status:** n=5 core-loop absolute timings recorded (Wave 8 partial cert; post
open-amortization tip)  
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

## Environment (n=5 primary sample — open amortization tip)

| Field | Value |
|-------|-------|
| Commit | `a5b1dc689c755228be15cefeaffd91dbb9dd18f3` |
| Branch | `codex/correctness-architecture-performance-program` |
| Timestamp (UTC) | `20260711T155225Z` |
| OS | macOS 26.5.1 (Darwin 25.5.0 arm64) |
| CPU | Apple M1 Pro |
| Memory | 32 GB (`34359738368` bytes) |
| rustc | 1.97.0 (2d8144b78 2026-07-07) |
| cargo | 1.97.0 (c980f4866 2026-06-30) |
| Binary | `artifacts/perf/bin/heddle` (`heddle 0.10.0`) |
| Binary SHA-256 | `cfc991b427e21227539201615ce78bec7c43eecc0651031a3b66aaea671e6535` |
| Build | `CARGO_TARGET_DIR=/tmp/heddle-todo5-target cargo build --release -p heddle-cli --locked` then copy into `artifacts/perf/bin/heddle` and `target/release/heddle` |
| Host noise | Moderately noisy at bench start: load averages ≈12.8 / 19.1 / 23.8; concurrent peer cargo/rustc jobs were still finishing. Prefer quieter re-run before external citation. |

Environment snapshot:
`artifacts/perf/20260711T155225Z-environment.txt`

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
export CARGO_TARGET_DIR=/tmp/heddle-todo5-target
cargo build --release -p heddle-cli --locked
cp -f "$CARGO_TARGET_DIR/release/heddle" target/release/heddle
cp -f "$CARGO_TARGET_DIR/release/heddle" artifacts/perf/bin/heddle

# 2) Absolute multi-op timings + paired A==B self-pairs (5 trials, 1 warmup)
bash scripts/program/core-loop-bench.sh \
  --heddle "$PWD/artifacts/perf/bin/heddle" \
  --trials 5 \
  --warmup 1 \
  --out-dir "$PWD/artifacts/perf"

# Optional: open-amortization phase sample (not part of absolute series)
# On the same equal-work fixture:
#   HEDDLE_PROFILE=jsonl heddle --output json status
#   HEDDLE_PROFILE=jsonl heddle verify
# Artifacts:
#   artifacts/perf/20260711T155225Z-profile-status.jsonl
#   artifacts/perf/20260711T155225Z-profile-verify.jsonl

# Optional: manual paired A/B between two commands on the same fixture
python3 scripts/program/paired-bench.py --help
```

Manifest opt-in suite entry: `suite = "perf"` job `core-loop-absolute-bench`
in `scripts/program/manifest.toml`.

---

## Absolute timings (n=5, 1 warmup, require success)

Source:
`artifacts/perf/20260711T155225Z-core-loop-absolute.json`

Times are **milliseconds** of process wall clock. Median, p95, and p99 are the
headline numbers for later regression comparison.

| Operation | argv (relative) | median_ms | p95_ms | p99_ms | mean_ms | stdev_ms | smoke budget_ms |
|-----------|-----------------|----------:|-------:|-------:|--------:|---------:|----------------:|
| bare_help | `heddle` | 12.0 | 13.2 | 13.3 | 12.3 | 0.6 | 250 |
| help | `heddle help` | 12.7 | 13.2 | 13.2 | 12.6 | 0.6 | 250 |
| status_text | `heddle status` | 27.2 | 28.2 | 28.3 | 27.2 | 0.9 | 650 |
| status_short | `heddle status --short` | 26.3 | 27.3 | 27.4 | 25.9 | 1.4 | 650 |
| status_json | `heddle --output json status` | 62.1 | 64.1 | 64.5 | 61.8 | 1.9 | 850 |
| log_json | `heddle --output json log` | 14.3 | 15.3 | 15.4 | 14.5 | 0.7 | 850 |
| diff_json | `heddle --output json diff` | 24.9 | 27.5 | 27.5 | 25.6 | 1.6 | 1000 |
| thread_list_json | `heddle --output json thread list` | 31.5 | 32.4 | 32.6 | 31.3 | 1.0 | 850 |

Smoke budgets are from `perf_core_loop.rs` (single-run upper bounds on this
hardware class). All medians, p95s, and p99s in this n=5 run are **under**
those budgets. That is expected for a warm release binary on Apple M1 Pro; it
is **not** evidence that the budgets should be tightened without multi-host
data.

### Comparison vs prior n=5 stamp `20260711T041555Z`

Prior cert sample (commit `b7f51aa4…`, binary SHA
`ff84f3e1…`) was taken under concurrent cargo load. Same fixture recipe, same
host class. Median deltas (this stamp − prior):

| Operation | prior median_ms | this median_ms | Δ ms | Δ % |
|-----------|----------------:|---------------:|-----:|----:|
| bare_help | 13.1 | 12.0 | −1.0 | −8.0% |
| help | 13.1 | 12.7 | −0.4 | −3.2% |
| status_text | 34.5 | 27.2 | −7.3 | −21.2% |
| status_short | 30.6 | 26.3 | −4.3 | −14.0% |
| status_json | 79.9 | 62.1 | −17.9 | −22.4% |
| log_json | 16.7 | 14.3 | −2.4 | −14.5% |
| diff_json | 30.2 | 24.9 | −5.3 | −17.6% |
| thread_list_json | 46.2 | 31.5 | −14.7 | −31.8% |

Interpretation (still **not** a Git win claim):

- Larger drops land on repo-touching ops (`status_*`, `thread_list_json`),
  consistent with status/verify **open amortization** (CLI injects the opened
  repo into `ExecutionContext`; core reports `repo_open_ms = 0` when injected
  and does not re-open). See profile one-shot below.
- Help-only ops barely moved (spawn floor).
- Host noise still present on both samples; treat absolute ms as single-host
  calibration, not multi-host cert.

### Raw trial times (absolute series, ms)

| Operation | t0 | t1 | t2 | t3 | t4 |
|-----------|---:|---:|---:|---:|---:|
| bare_help | 13.4 | 12.0 | 11.9 | 11.9 | 12.4 |
| help | 12.1 | 11.9 | 13.1 | 13.2 | 12.7 |
| status_text | 27.8 | 25.9 | 27.2 | 26.6 | 28.3 |
| status_short | 27.5 | 24.1 | 26.3 | 25.0 | 26.6 |
| status_json | 62.1 | 62.5 | 59.7 | 64.5 | 60.3 |
| log_json | 14.3 | 15.1 | 15.4 | 14.0 | 13.7 |
| diff_json | 24.6 | 24.9 | 27.5 | 23.9 | 27.3 |
| thread_list_json | 30.1 | 31.5 | 30.5 | 31.5 | 32.7 |

---

## Open-amortization profile one-shot (same fixture recipe)

Optional `HEDDLE_PROFILE=jsonl` sample on an equal-work fixture (dirty tracked
file present; verify may exit non-zero with “uncaptured” while still emitting
phase timings). Artifacts:

- `artifacts/perf/20260711T155225Z-profile-status.jsonl`
- `artifacts/perf/20260711T155225Z-profile-verify.jsonl`

Headline phase ms (single process, not multi-trial absolute series):

| Command | Phase metric | ms | Note |
|---------|--------------|---:|------|
| status | `repo_open_ms` | 1 | Shell open folded into truthful phase; no second core open |
| status | `worktree_status_ms` | 6 | |
| status | `verification_ms` | 13 | |
| status | `thread_summary_ms` | 67 | Dominant remaining cost on this fixture |
| status | `build_total_ms` | 88 | |
| status | `total_ms` | 101 | Process wall incl. config/logging |
| verify | `plain_git_probe_ms` | 0 | Skipped when Heddle repo already injected |
| verify | `repo_open_ms` | 0 | Amortized / injected open path |
| verify | `verification_ms` | 11 | |
| verify | `total_ms` | 18 | Exit status was error (uncaptured dirty tree) |

`HEDDLE_PROFILE=1` text path for status also reported `repo_open_ms: 0` on a
follow-up run (injected repo; core does not re-open). These phase numbers are
**illustrative of amortization**, not replacements for the absolute multi-trial
table above.

---

## Paired self-pairs (A==B, alternating, n=5)

Also produced by `core-loop-bench.sh` via `scripts/program/paired-bench.py`
(alternating A/B, require success, median/mean/p95/p99/stdev). A and B are the
**same** command — used to exercise the paired harness and estimate
run-to-run noise, not to claim a win.

| Op | Artifact | A median_ms | A p95_ms | B median_ms | B p95_ms | ratio B/A |
|----|----------|------------:|---------:|------------:|--------:|----------:|
| status_json | `…-core-loop-paired-status_json.json` | 72.9 | 79.3 | 80.0 | 139.8 | 1.098 |
| log_json | `…-core-loop-paired-log_json.json` | 22.6 | 24.8 | 20.7 | 24.1 | 0.919 |
| diff_json | `…-core-loop-paired-diff_json.json` | 30.8 | 34.1 | 31.3 | 37.1 | 1.015 |
| help | `…-core-loop-paired-help.json` | 18.7 | 20.5 | 18.1 | 19.1 | 0.969 |

Prefix: `artifacts/perf/20260711T155225Z-`

Self-pair ratios near 1.0 indicate the alternating harness is balanced under
quieter conditions; `status_json` B-side p95 spike reflects residual host load
and is still **not** a cross-implementation comparison. Prefer absolute-series
median/p95 for regression baselines when A==B noise is elevated.

---

## Historical samples (superseded for primary cert stamp)

| Stamp | Commit | Trials | Role |
|-------|--------|-------:|------|
| `20260711T155225Z` | `a5b1dc68…` | 5 | **Primary** post open-amortization tip |
| `20260711T041555Z` | `b7f51aa4…` | 5 | Prior Wave 8 cert sample (noisier concurrent cargo) |
| `20260711T032344Z` | `c1699119…` | 3 | Wave 1 measurement foundation only |

Do not use n=3 as the certification trial count. Prefer
`20260711T155225Z` for regression comparison going forward unless a quieter
re-run replaces it.

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
