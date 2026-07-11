# Trustworthy baseline record

## Environment (this machine)

| Field | Value |
|-------|-------|
| Commit (start) | `74f2e20edef1572877c712c8551485fc2b5655a8` |
| Branch | `codex/correctness-architecture-performance-program` |
| OS | macOS 26.5.1 (Darwin 25.5.0 arm64) |
| CPU | Apple M1 Pro |
| Memory | 32 GB |
| rustc | 1.97.0 (2d8144b78 2026-07-07) |
| cargo | 1.97.0 (c980f4866 2026-06-30) |
| git | 2.55.0 |

## Harness status

| Capability | Status |
|------------|--------|
| Curated manifest | **Shipped** — `scripts/program/manifest.toml` |
| Baseline runner + classification | **Shipped** — `scripts/program/run-baseline.sh` |
| Paired bench runner | **Shipped** — `scripts/program/paired-bench.py` |
| CLI residual inventory | **Shipped** — `scripts/program/gen-cli-domain-residual.py` |
| Full curated suite green on this host | **In progress** — first wave0 oracle shard recorded under `artifacts/baseline/` |
| Performance certification (5 trials) | **Calibration recorded (3 trials)** — see `docs/program/PERF_BASELINE.md`; ≥5-trial cert still open |

## Wave 0 oracle shard (2026-07-11, this machine)

Source: `artifacts/baseline/wave0-merged/summary.json` after
`scripts/program/run-baseline.sh --job …` for each job below.

| Job | Status | Duration | Oracle |
|-----|--------|----------|--------|
| facade-render-free | pass | 39 ms | no |
| fmt-check (`cargo +nightly fmt --check`) | **pass** (post `c1699119`) | — | yes — applied `cargo +nightly fmt --all` |
| git-process-lint | pass | 1.8 s | yes |
| roundtrip-fidelity | pass | 4.8 s | yes |
| commit-conformance | pass | 3.2 s | yes |
| lib-core | pass | 38.8 s | no |
| lib-format | pass | 3.5 s | no |
| lib-crypto | pass | 18.4 s | no |
| lib-merge | pass | 26.0 s | no |

**Aggregate:** 8 pass / 1 fail (fmt only). All Git fidelity oracles in the shard passed.

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

**Combined curated-so-far:** 16 pass / 1 fail (`fmt-check` only).  
Oracle jobs all green: git-process-lint, roundtrip-fidelity, commit-conformance, git-projection-engine, formal-specs, lib-git-projection.

Also green: `cli-core-functionality` (46.2 s), `cli-state-management` (45.7 s).

**Full curated suite in manifest (excluding suite=perf): 18 pass / 1 fail (`fmt-check` only).**  
See `artifacts/baseline/curated-merged/summary.json`.

Perf calibration run (2026-07-11): equal-work core-loop absolute + paired self-pairs
with 3 trials on release binary — see `docs/program/PERF_BASELINE.md` and
`artifacts/perf/20260711T032344Z-*`. Full ≥5-trial certification still open.

### Harness blockers / notes

1. **rustfmt:** `rustfmt.toml` requires nightly (`imports_granularity`, `group_imports`). Stable `cargo fmt` mis-formats the tree — never use it to “fix” the repo. Gate is now `scripts/program/fmt-check.sh` (nightly only). Pre-existing nightly drift on `main` is a **real gate fail** for certification, not introduced by this program branch; fixing it is a separate bounded wave (or `skip_prereq` when nightly missing).
2. **Full curated suite** (repo/objects/refs/oplog/cli shards) not yet run end-to-end in one stamp; wave0 is the first trustworthy partial baseline.
3. **Perf certification (≥5 trials)** still open; equal-work fixture automation is now shipped (`scripts/program/core-loop-bench.sh`) and a 3-trial calibration is recorded in `PERF_BASELINE.md`.

## Classification vocabulary (enforced by runner)

Results are never collapsed into a single pass rate without:

- `pass` / `fail` comparable
- `skip_prereq` (e.g. missing `git` for fixture builders)
- `todo_known` / ignored-only
- `timeout` / `setup_fail` / `aborted`
- `incomparable` (dry-run or unequal work)

## Performance baseline

**Calibration (not certification):** `docs/program/PERF_BASELINE.md` records
absolute equal-work core-loop timings (n=3) with raw JSON under
`artifacts/perf/`. Explicitly **not** a Git win claim.

Existing budgets / other benches:

- `crates/cli/tests/cli_integration/perf_core_loop.rs` (ignored release smoke)
- Criterion benches under objects/refs/oplog/cli/mount/semantic
- Weekly `.github/workflows/benchmarks.yml`

**Blocker for claiming speed / cert:** ≥5 paired trials on correct paths with
raw artifacts; no early-exit gaming; no cross-tool claims from absolute-only
calibration.

## How to refresh

```bash
bash scripts/program/run-baseline.sh
# inspect artifacts/baseline/<stamp>/summary.json
```

Update this file’s tables after each integrated wave.
