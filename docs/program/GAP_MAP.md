# Prioritized gap map

Grouped by owning subsystem. Severity: **P0** blocks trustworthy certification, **P1** blocks 1.0-shaped readiness, **P2** long-tail.

## P0 — measurement and truth

| ID | Gap | Owner | Contract violated | Fix direction |
|----|-----|-------|-------------------|---------------|
| M1 | No single curated suite manifest with skip/TODO/oracle applicability | program harness | Deterministic reporting | `scripts/program/manifest.toml` + runner |
| M2 | No machine-readable baseline artifacts with env/commit/classification | program harness | Reproducible reporting | `run-baseline.sh` + JSONL results |
| M3 | Perf claims lack paired alternating trials + equal-work checks | program harness | Truthful measurement | `paired-bench.py` + profile-before-optimize rule |
| M4 | Stability thresholds still TBD | product/docs | Release gates | Derive interim gates from oracles already in CI |

## P0 — correctness contracts

| ID | Gap | Owner | Contract | Fix direction |
|----|-----|-------|----------|---------------|
| C1 | Git round-trip oracle is necessary but not a full porcelain oracle | git-projection + ingest | Byte-identical public history | Keep fixtures green; expand corpus only when equal work defined |
| C2 | Realworld fixtures are stress/shape coverage, not continuous oracle | cli tests | Perf/correctness boundary | Classify as stress; never claim perf win on partial import |
| C3 | Formal Quint specs vs Rust property tests may drift | formal_specs + domain | Spec fidelity | Keep `specs/quint/verify.sh` + formal_specs in curated suite |
| C4 | git-process lint omits `core` / `git-projection` | tooling | No runtime git | Extend lint dirs |

## P1 — architecture / embeddability

| ID | Gap | Owner | Contract | Fix direction |
|----|-----|-------|----------|---------------|
| A1 | Large domain still in CLI (`thread`, `clone`, `workflow`, remotes, undo) | cli → core/repo | Thin delivery layer | Wave extractions by command family |
| A2 | Dual status paths (CLI status.rs + core status) | core + cli | Single ownership | CLI render-only over `heddle_core::status` |
| A3 | Ambient OnceLocks block multi-repo embed | cli, repo, semantic | DI facade | Migrate to ExecutionContext services |
| A4 | `process::exit` inside command handlers | cli | Result-only domain | Return typed errors; exit only in main |
| A5 | Monolithic core modules (status/merge) | core | Maintainability | Split by interface depth when touching |
| A6 | Bridge mirror legacy cleanup incomplete | git-projection + docs | Overlay model | Continue VERIFICATION_CLEANUP_PLAN |

## P1 — performance

| ID | Gap | Owner | Hotspot class | Fix direction |
|----|-----|-------|---------------|---------------|
| P1 | Core-loop budgets only ignored smoke test | cli | Startup + status | Promote paired release smoke + artifact store |
| P2 | Repeated repo/Sley open on status/verify paths | repo/core | Repeated open | Cache handles in session facade |
| P3 | Worktree full scans where index/fsmonitor could help | repo | Full scan | Profile first; watchman optional |
| P4 | Criterion benches not gated for regression | CI | No fail-on-reg | Interim: store weekly artifacts; later: threshold on common ops |
| P5 | CLI cold dep graph historically huge | cli packaging | Compile/startup | Keep server out of OSS CLI (done direction); continue dep audits |

## P2 — long tail

| ID | Gap | Owner | Notes |
|----|-----|-------|-------|
| L1 | Windows mount/materialization edge cases | mount/repo | Platform matrix |
| L2 | Reftable not implemented | refs | Known limitation ~10k+ refs |
| L3 | Partial clone | wire/repo | Planned |
| L4 | Semantic merge language matrix opt-in | semantic | First-class Rust/Py/JS/TS |
| L5 | Hosted collaboration sync maturity | client/weft | Foundation |
| L6 | `create_dir_all` without grandparent dirent fsync | objects `fs_atomic` / `fs_io` | After first write into a new shard (`blobs/ab/…`), parent dir is fsynced but the grandparent holding the new shard dirent may not be. Crash can drop an entire new shard tree despite per-file durability. Fix: `create_dir_all_durable` that fsyncs newly created ancestors + deepest pre-existing parent; wire into `write_file_atomic` + `write_atomic`. Cost is once-per-shard. |
| L7 | `StreamingPackBuilder::finalize` flushes but does not fsync pack/index | objects pack | Publish path now fsyncs at `publish_file_durable` install boundary (Wave 5 fix). Residual: staged files are not durable *before* install returns control if a caller reads them without publishing. Optional harden: fsync in finalize. |
| L8 | Pack-without-index window between two durable publishes | objects `install_pack_files_streaming` | Pack then index are separate durable renames; crash between leaves an unpaired `.pack` that `reload_packs` ignores. Acceptable; optional journal/two-phase if unpaired packs become a disk-leak concern. |

## Failure cluster methodology

For each material failure:

1. First real root cause (not first failing assertion).
2. Cascading setup failures separated.
3. Owning module + external contract.
4. Class: parse / semantics / persistence / ordering / concurrency / render / diagnostics / harness.
5. Smallest general fix (no test-specific production branches).
6. Cross-platform / concurrency / security / compatibility risks.

Unlock-many-downstream clusters first (repo open, verification state, git projection mapping, save primitive).
