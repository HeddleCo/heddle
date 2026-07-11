# Heddle correctness / architecture / performance program

This directory is the working control plane for driving Heddle toward:

1. Complete correctness against oracles and product contracts  
2. Clean embeddable architecture (`delivery → heddle-core → domain`)  
3. No hidden external `git` process dependency for public overlay workflows  
4. Truthful, reproducible performance measurement  
5. Maintainable public API and production readiness  

## Documents

| Doc | Purpose |
|-----|---------|
| [PRODUCT_CONTRACT.md](PRODUCT_CONTRACT.md) | What Heddle is supposed to be |
| [ARCHITECTURE_AUDIT.md](ARCHITECTURE_AUDIT.md) | Ownership map and structural problems |
| [GAP_MAP.md](GAP_MAP.md) | Prioritized gaps by subsystem |
| [RELEASE_GATES.md](RELEASE_GATES.md) | Measurable gates |
| [WAVES.md](WAVES.md) | Multi-wave plan + first tasks |
| [cli-domain-residual.md](cli-domain-residual.md) | Generated CLI residual extraction matrix |
| [BASELINE.md](BASELINE.md) | Latest trustworthy baseline record |

## Tooling

```bash
# Dry-run curated jobs
bash scripts/program/run-baseline.sh --dry-run

# Run full curated baseline (long)
bash scripts/program/run-baseline.sh

# Single job
bash scripts/program/run-baseline.sh --job git-process-lint

# Perf suite only
bash scripts/program/run-baseline.sh --suite perf

# Paired alternating timings (equal-work assumed by caller)
python3 scripts/program/paired-bench.py --name demo --trials 3 \
  --a 'true' --b 'true'

# Refresh CLI residual matrix
python3 scripts/program/gen-cli-domain-residual.py
```

Artifacts land under `artifacts/baseline/<stamp>/` (`environment.txt`, `results.jsonl`, `summary.json`, per-job logs).

## Branch

Work proceeds on `codex/correctness-architecture-performance-program` (or successor program branches). Do not rewrite unrelated user work.
