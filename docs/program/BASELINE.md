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
| Performance certification (5 trials) | **Blocked** until oracle shard green and equal-work fixtures automated |

## Wave 0 oracle shard (2026-07-11, this machine)

Source: `artifacts/baseline/wave0-merged/summary.json` after
`scripts/program/run-baseline.sh --job …` for each job below.

| Job | Status | Duration | Oracle |
|-----|--------|----------|--------|
| facade-render-free | pass | 39 ms | no |
| fmt-check (stable cargo fmt, pre-fix) | **fail** | 3.4 s | no — tree has nightly-rustfmt drift; do **not** rewrite with stable fmt |
| git-process-lint | pass | 1.8 s | yes |
| roundtrip-fidelity | pass | 4.8 s | yes |
| commit-conformance | pass | 3.2 s | yes |
| lib-core | pass | 38.8 s | no |
| lib-format | pass | 3.5 s | no |
| lib-crypto | pass | 18.4 s | no |
| lib-merge | pass | 26.0 s | no |

**Aggregate:** 8 pass / 1 fail (fmt only). All Git fidelity oracles in the shard passed.

### Harness blockers / notes

1. **rustfmt:** `rustfmt.toml` requires nightly (`imports_granularity`, `group_imports`). Stable `cargo fmt` mis-formats the tree — never use it to “fix” the repo. Gate is now `scripts/program/fmt-check.sh` (nightly only). Pre-existing nightly drift on `main` is a **real gate fail** for certification, not introduced by this program branch; fixing it is a separate bounded wave (or `skip_prereq` when nightly missing).
2. **Full curated suite** (repo/objects/refs/oplog/cli shards) not yet run end-to-end in one stamp; wave0 is the first trustworthy partial baseline.
3. **Perf certification** still blocked until equal-work fixture automation + ≥5 paired trials.

## Classification vocabulary (enforced by runner)

Results are never collapsed into a single pass rate without:

- `pass` / `fail` comparable
- `skip_prereq` (e.g. missing `git` for fixture builders)
- `todo_known` / ignored-only
- `timeout` / `setup_fail` / `aborted`
- `incomparable` (dry-run or unequal work)

## Performance baseline

Not yet certified. Existing budgets live in:

- `crates/cli/tests/cli_integration/perf_core_loop.rs` (ignored release smoke)
- Criterion benches under objects/refs/oplog/cli/mount/semantic
- Weekly `.github/workflows/benchmarks.yml`

**Blocker for claiming speed:** must run paired trials on correct paths with raw artifacts; no early-exit gaming.

## How to refresh

```bash
bash scripts/program/run-baseline.sh
# inspect artifacts/baseline/<stamp>/summary.json
```

Update this file’s tables after each integrated wave.
