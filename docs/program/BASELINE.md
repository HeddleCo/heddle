# Trustworthy baseline record

## Environment (this machine)

| Field | Value |
|-------|-------|
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
| CARGO_TARGET_DIR (Wave 8 high-signal) | `/tmp/heddle-cert-w8` |
| CARGO_TARGET_DIR (release perf binary) | `/tmp/heddle-w8-target` |

## Harness status

| Capability | Status |
|------------|--------|
| Curated manifest | **Shipped** — `scripts/program/manifest.toml` |
| Baseline runner + classification | **Shipped** — `scripts/program/run-baseline.sh` |
| Paired bench runner | **Shipped** — `scripts/program/paired-bench.py` |
| CLI residual inventory | **Shipped** — `scripts/program/gen-cli-domain-residual.py` |
| High-signal Wave 8 re-cert (5 jobs) | **Green** — 5/5 pass; see `artifacts/baseline/post-wave-fanout2-merged/summary.json` |
| High-signal post–Wave 2/3 re-cert (7 jobs) | **Green** — 7/7 pass; see `artifacts/baseline/post-wave23-merged/summary.json` |
| Full curated suite (19 jobs) | **Prior stamp** — 18 pass / 1 fail (`fmt-check`); fmt fixed; full 19-job single-stamp refresh still open |
| Performance certification (5 trials) | **Recorded** — n=5 absolute + paired self-pairs; see `docs/program/PERF_BASELINE.md` (**not** a Git win claim) |

## Wave 8 partial cert — high-signal re-run (2026-07-11, this machine)

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

Perf: n=5 core-loop absolute + A==B self-pairs on release binary built from the
same tip — `artifacts/perf/20260711T041555Z-*`, documented in
`docs/program/PERF_BASELINE.md`. Explicitly **not** a Git comparison.

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
Wave 8 fanout2 5-job suite above for the listed jobs; commit-conformance and
formal-specs not re-run in the Wave 8 partial set.

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

**Full curated suite in manifest (excluding suite=perf):** see
`artifacts/baseline/curated-merged/summary.json` (18/19; fmt was sole fail).

**fmt status after Wave 2/3:** fixed by `c1699119` (`cargo +nightly fmt --all`);
re-certified green in post-wave23 and Wave 8 high-signal suites. Expect full
curated refresh to be 19/19 when re-run end-to-end (not re-executed in Wave 8
partial cert beyond the five high-signal jobs).

### Harness blockers / notes

1. **rustfmt:** `rustfmt.toml` requires nightly (`imports_granularity`, `group_imports`). Stable `cargo fmt` mis-formats the tree — never use it to “fix” the repo. Gate is `scripts/program/fmt-check.sh` (nightly only). **Verified green** on Wave 8 tip `b7f51aa4`.
2. **Full curated suite** not re-run end-to-end in one stamp on Wave 8 partial cert; high-signal 5-job suite is the fanout2 authority for oracles + fmt + lib-core.
3. **Perf certification (≥5 trials)** recorded for equal-work core-loop absolute + paired self-pairs (`docs/program/PERF_BASELINE.md`). Still **not** a Git win claim; multi-host matrix open.

## Classification vocabulary (enforced by runner)

Results are never collapsed into a single pass rate without:

- `pass` / `fail` comparable
- `skip_prereq` (e.g. missing `git` for fixture builders)
- `todo_known` / ignored-only
- `timeout` / `setup_fail` / `aborted`
- `incomparable` (dry-run or unequal work)

## Performance baseline

**n=5 Wave 8 sample (cert trial count met on this host):**  
`docs/program/PERF_BASELINE.md` + `artifacts/perf/20260711T041555Z-*`.  
Absolute equal-work core-loop timings only. Explicitly **not** a Git win claim.

Existing budgets / other benches:

- `crates/cli/tests/cli_integration/perf_core_loop.rs` (ignored release smoke)
- Criterion benches under objects/refs/oplog/cli/mount/semantic
- Weekly `.github/workflows/benchmarks.yml`

**Still open for broader speed claims:** multi-host / platform matrix, quiet-host
re-run, and any cross-tool comparison with equal work and require-success.

## How to refresh

```bash
# High-signal Wave 8 set (example)
export CARGO_TARGET_DIR=/tmp/heddle-cert-w8
for job in facade-render-free fmt-check git-process-lint roundtrip-fidelity lib-core; do
  BASELINE_OUT_DIR="artifacts/baseline/post-wave-fanout2-$job" \
    bash scripts/program/run-baseline.sh --job "$job"
done
# Perf cert sample
export CARGO_TARGET_DIR=/tmp/heddle-w8-target
cargo build --release -p heddle-cli --locked
bash scripts/program/core-loop-bench.sh --trials 5 --warmup 1 --out-dir artifacts/perf
# Full curated suite
bash scripts/program/run-baseline.sh
# inspect artifacts/baseline/<stamp>/summary.json
```

Update this file’s tables after each integrated wave.
