# Release gates (program)

Interim gates for this program until maintainer locks 1.0 numbers in `docs/STABILITY.md`.
Every gate is executable from the repo root with checked-in tooling.

## G1 — Oracle correctness

| Gate | Threshold | Command |
|------|-----------|---------|
| Round-trip fidelity | 100% of curated fixtures pass | `cargo test -p heddle-cli --test roundtrip_fidelity -- --nocapture` |
| Commit conformance | 100% pass | `cargo test -p heddle-cli --test commit_conformance -- --nocapture` |
| Git process lint | 0 unexpected runtime git spawns | `cargo test -p heddle-cli --test git_process_lint` |
| Facade render-free | script exit 0 | `bash scripts/check-facade-render-free.sh` |
| Diff/patch conformance | 100% pass (non-ignored) | `cargo test -p heddle-cli --test cli_integration diff_patch_conformance` |
| Formal specs smoke | `specs/quint/verify.sh` exit 0 when Quint available; else mark `prereq_missing` | see manifest |

## G2 — Workspace quality

| Gate | Threshold | Command |
|------|-----------|---------|
| Format | clean | `bash scripts/program/fmt-check.sh` (`cargo +nightly fmt --all -- --check`; never stable `cargo fmt`) |
| Clippy (default members / affected) | 0 warnings with `-D warnings` on changed packages | `cargo clippy -p <pkgs> -- -D warnings` |
| Curated unit/integration suite | 0 unexpected fails | `bash scripts/program/run-baseline.sh --suite curated` |
| Feature matrix (local OSS) | default features green for curated | documented in manifest |

## G3 — No undocumented external deps

| Gate | Threshold |
|------|-----------|
| Runtime `git` | Not required for public overlay workflows (lint) |
| Optional tools (`watchman`, mount helpers) | Documented as optional; failures degrade gracefully |
| Network | Not required for local VCS curated suite |

## G4 — Public API / docs

| Gate | Threshold | Command |
|------|-----------|---------|
| `heddle-core` docs build | `cargo doc -p heddle-core --no-deps` exit 0 | |
| Doctor schemas clean | existing CI script | `bash scripts/check-doctor-schemas-clean.sh` when binary available |
| JSON schema drift | doctor schemas gate | CI `rust-tests.yml` |

## G5 — Performance (equal work only)

| Gate | Threshold | Method |
|------|-----------|--------|
| Core-loop smoke | No case exceeds documented release budgets in `perf_core_loop.rs` on program hardware class | release + ignored smoke, ≥3 trials dev / ≥5 cert |
| Common ops | No statistically meaningful regression vs stored baseline median (p95) for status/log/diff | paired alternating runs |
| Criterion weekly | Artifacts stored; regressions investigated, not silently ignored | `scripts/discover-benches.py` + workflow |

**Rules (non-negotiable):**

- Never claim a win from missing behavior, early exit, skipped work, weaker durability, reduced validation, stale cache, or different inputs.
- Profile only correct, comparable paths.
- Preserve raw machine-readable artifacts under `artifacts/baseline/` or `artifacts/perf/`.

### Interim numeric budgets (dev laptop class: Apple M1 Pro, release)

From `crates/cli/tests/cli_integration/perf_core_loop.rs` (smoke, not CI gate today):

| Operation | Release budget (single run) |
|-----------|----------------------------|
| bare help / help | 250 ms |
| commands JSON | 350 ms |
| status text/short | 650 ms |
| status JSON | 850 ms |
| thread list / log JSON | 850 ms |
| diff JSON | 1000 ms |
| ready JSON | 1500 ms |

Certification requires median and p95 under budget across 5 alternating trials on a warm binary, clean fixture, equal work.

## G6 — Architecture

| Gate | Threshold |
|------|-----------|
| New domain logic in CLI | Rejected unless extract-or-justify in PR |
| Dependency direction | Domain ↛ cli; facade render-free |
| Git process allowlist | Empty or reviewed entries only |

## Gate evaluation result format

Each baseline run emits `artifacts/baseline/summary.json` with per-case:

- `status`: `pass` | `fail` | `skip_prereq` | `todo_known` | `timeout` | `setup_fail` | `aborted` | `incomparable`
- `duration_ms`
- `classification_reason`
- `oracle`: bool
- `perf_eligible`: bool
