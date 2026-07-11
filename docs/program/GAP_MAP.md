# Prioritized gap map

Grouped by owning subsystem. Severity: **P0** blocks trustworthy certification, **P1** blocks 1.0-shaped readiness, **P2** long-tail.

## P0 — measurement and truth

| ID | Gap | Owner | Contract violated | Status / fix direction |
|----|-----|-------|-------------------|------------------------|
| M1 | No single curated suite manifest with skip/TODO/oracle applicability | program harness | Deterministic reporting | **Shipped** — `scripts/program/manifest.toml` + runner (Wave 0/1; see `BASELINE.md`) |
| M2 | No machine-readable baseline artifacts with env/commit/classification | program harness | Reproducible reporting | **Shipped** — `run-baseline.sh` + JSONL/`summary.json` under `artifacts/baseline/` |
| M3 | Perf claims lack paired alternating trials + equal-work checks | program harness | Truthful measurement | **Shipped** (harness) — `paired-bench.py` + `core-loop-bench.sh` + profile-before-optimize rule; equal-work **re-stamp** and multi-host samples still open (Wave 6) |
| M4 | Stability thresholds still TBD | product/docs | Release gates | **Open** — derive interim gates from oracles already in CI; `docs/STABILITY.md` still TBD |

## P0 — correctness contracts

| ID | Gap | Owner | Contract | Fix direction |
|----|-----|-------|----------|---------------|
| C1 | Git round-trip oracle is necessary but not a full porcelain oracle | git-projection + ingest | Byte-identical public history | Keep fixtures green; expand corpus only when equal work defined |
| C2 | Realworld fixtures are stress/shape coverage, not continuous oracle | cli tests | Perf/correctness boundary | Classify as stress; never claim perf win on partial import |
| C3 | Formal Quint specs vs Rust property tests may drift | formal_specs + domain | Spec fidelity | Keep `specs/quint/verify.sh` + formal_specs in curated suite |
| C4 | git-process lint omits `core` / `git-projection` | tooling | No runtime git | **Shipped** — `git_process_lint` scan dirs include `crates/core/src` and `crates/git-projection/src` (Wave 1). Stale inverse claims may remain in older audit/contract prose — trust this row + lint source. |

## P1 — architecture / embeddability

| ID | Gap | Owner | Contract | Fix direction |
|----|-----|-------|----------|---------------|
| A1 | Large domain still in CLI (`thread`, `clone`, `workflow`, remotes, undo) | cli → core/repo | Thin delivery layer | **High-value pure plans extracted (2026-07-11)**; remaining residual is I/O/render/catalog (`cli-domain-residual.md`) |
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
| L1 | Windows mount/materialization edge cases | mount/repo | **Open** — platform residual; see [`PLATFORM_MATRIX.md`](PLATFORM_MATRIX.md). ProjFS smoke exists in CI (foundation); materialization edge cases not fully certified. |
| L2 | Reftable not implemented | refs | **Open** — packed-refs works but degrades ~10k+ refs; reftable **not implemented**. Residual + docs/tests checklist in [`PLATFORM_MATRIX.md`](PLATFORM_MATRIX.md). |
| L3 | Partial clone | wire/repo | **Planned** — lazy object fetch not supported; see platform matrix. |
| L4 | Semantic merge language matrix opt-in | semantic | First-class Rust/Py/JS/TS |
| L5 | Hosted collaboration sync maturity | client/weft | Foundation |
| L6 | Grandparent dirent durability on new object shards | objects `fs_atomic` | **Shipped on program tip** — `create_dir_all_durable` fsyncs newly created ancestors + deepest pre-existing parent; wired into `write_file_atomic` / secret / `publish_file_*_durable`, store layout init, `fs_io::write_atomic` parents, pack install dirs, redaction/visibility sidecars, agent-task dirs, lock dir ensure (`lock.rs`), agent registry dir create (`agent_registry.rs`), streaming pack bucket dir (`streaming_builder.rs`). `create_private_dir_all` also fsyncs new ancestors while keeping Unix `0o700` create mode. Residual: tests-only bare `create_dir_all` (fixtures); Windows dir fsync remains platform no-op (see `sync_directory`). |
| L7 | `StreamingPackBuilder::finalize` flushes but does not fsync pack/index | objects pack | Publish path now fsyncs at `publish_file_durable` install boundary (Wave 5 fix); bucket staging dir create is durable (L6). Residual: staged pack/index files are not fsynced *in finalize* before install — durability still arrives at the publish/install boundary. Optional harden: fsync in finalize if callers read staged files without publishing. |
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
