# Testing

Use [docs/TESTING_STRATEGY.md](../docs/TESTING_STRATEGY.md) as the practical
"where should this be tested?" map. This file is the short command reference.

## Running Tests

```bash
# All tests
cargo test

# Specific test
cargo test test_name

# Show output
cargo test -- --nocapture

# Run specific test file
cargo test -p heddle-cli --test comprehensive
cargo test -p heddle-cli --test production_features

# Feature-gated backend coverage
cargo test --features postgres

# With zstd compression (required for full pack/delta coverage)
cargo test --features zstd

# Formal state-machine verification
./specs/quint/verify.sh

# Ignored release-mode CLI performance smoke
cargo test --release -p heddle-cli --test cli_integration core_loop_command_surface_perf_smoke -- --ignored --nocapture
```

## Test Categories

| Category | Location | Purpose |
|----------|----------|---------|
| Unit tests | `src/` modules with `#[cfg(test)]` | Test individual functions/types |
| CLI command tests | `crates/cli/tests/core_functionality/`, `crates/cli/tests/cli_integration/`, `crates/cli/tests/comprehensive/`, `crates/cli/tests/production_features/` | End-to-end command behavior, output contracts, recovery language, and production feature coverage |
| CLI contract lints | `crates/cli/tests/*_lint.rs`, `crates/cli/tests/tier_coverage.rs`, `crates/cli/tests/op_id_coverage.rs`, `crates/cli/tests/idempotency_lint.rs` | Command rendering, idempotency, tier, advice, typed-error, docs, schema, and Git-boundary conventions |
| Object/format tests | `crates/objects/src/*_tests.rs`, `crates/objects/tests/`, `crates/format/src/*_tests.rs` | Content addressing, object codecs, worktree scans, pack/delta roundtrip, and compression behavior |
| Refs/repo/oplog tests | `crates/refs/src/refs/*_tests.rs`, `crates/repo/src/repository_tests.rs`, `crates/repo/tests/`, `crates/oplog/src/oplog/*_tests.rs` | CAS semantics, HEAD/thread convergence, packed refs, undo/recovery, timeline materialization, and oplog durability |
| Git bridge tests | `crates/cli/tests/git_bridge_integration.rs`, `crates/cli/tests/roundtrip_fidelity.rs`, `crates/cli/tests/commit_conformance.rs`, `crates/cli/tests/cli_integration/git_*`, `tests/bridge_migration_test.sh` | Git adopt/export fidelity, Sley boundary behavior, real-world Git fixtures, and SHA preservation |
| Hosted/wire/auth tests | `crates/wire/src/auth_tests.rs`, `crates/cli/tests/serve_local.rs`, `crates/cli/tests/presence_publish.rs`, Postgres-gated crate tests | Wire protocol, auth, local serve, hosted metadata, and SQL-backed behavior |
| Formal spec tests | `specs/quint/*.qnt`, `crates/cli/tests/formal_specs.rs` | Property-based tests derived from Quint specs for merge, locks, refs, agents, worktrees, and repo ops |
| Performance fixtures | `docs/perf/`, `crates/cli/tests/cli_integration/perf_core_loop.rs`, `crates/*/benches/` | Fixture-backed runtime, allocation, I/O, pack, and command-loop regressions |

## Choosing A Test Layer

- Start with the closest user or module contract. CLI behavior belongs in CLI
  integration tests; pure object/refs logic belongs beside the owning crate.
- Add a lint test when the rule is a repo-wide convention rather than one
  command's behavior.
- Add or update Quint plus Rust property coverage when a modeled guard,
  transition, or invariant changes.
- Keep default tests deterministic and local. Put release-mode, service-backed,
  public-repo, OS-kernel, large-fixture, or timing-sensitive tests behind
  `#[ignore]`, `--release`, or an explicit environment flag.
- Do not claim a performance improvement without naming the fixture, command,
  metric, baseline, and result.

## Testing Checklist

Before considering a change complete:

- [ ] `cargo build` succeeds
- [ ] `cargo test` passes
- [ ] `cargo clippy -- -D warnings` has no warnings
- [ ] `cargo fmt --check` passes
- [ ] New public APIs have doc comments
- [ ] Breaking changes are documented
- [ ] Spec and implementation are in sync
- [ ] If state machine logic changed: Quint spec updated and `./specs/quint/verify.sh` passes (see [[.agents/formal-specs]])
- [ ] If performance-sensitive code changed: fixture, metric, and baseline/result are recorded (see `docs/perf/` and `docs/TESTING_STRATEGY.md`)
- [ ] If a test is ignored: the ignore reason says why and how to run it

## Hosted / Postgres Testing

- Run focused `--features postgres` coverage when touching hosted metadata, authz, or server-mode backends.
- Add unit tests for path validation, role resolution, and config behavior.
- Add integration tests for Postgres-backed hosted metadata when behavior crosses SQL boundaries.
- Prefer Railway local services for hosted integration testing when the code depends on the real Postgres service shape.

Example local flow:

```bash
railway dev up
railway run -s Postgres -- sh -lc 'export HEDDLE_TEST_DATABASE_URL="..."; cargo test --features postgres --test pg_hosted_registry_integration -- --ignored --test-threads=1'
```
