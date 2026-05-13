# Testing

## Running Tests

```bash
# All tests
cargo test

# Specific test
cargo test test_name

# Show output
cargo test -- --nocapture

# Run specific test file
cargo test --test comprehensive
cargo test --test production_features

# Feature-gated backends
cargo test --features postgres
cargo check --features postgres,s3

# With zstd compression (required for full pack/delta coverage)
cargo test --features zstd
```

## Test Categories

| Category | Location | Purpose |
|----------|----------|---------|
| Unit tests | `src/` modules with `#[cfg(test)]` | Test individual functions/types |
| Integration tests | `tests/comprehensive.rs` | End-to-end CLI tests |
| Production tests | `tests/production_features.rs` | Feature completeness tests |
| Wire protocol tests | `tests/wire_protocol.rs` | Network sync tests |
| Auth tests | `tests/auth.rs` | Authentication tests |
| Delta/pack tests | `src/protocol/delta/delta_tests.rs`, `src/store/pack/pack_tests.rs` | Delta codec and packfile roundtrip (require `--features zstd` for full coverage) |
| Formal spec tests | `tests/formal_specs.rs` | Property-based tests derived from Quint specs (merge, locks, refs, agents, worktrees, repo ops) |
| Quint specs | `specs/quint/*.qnt` | Formal state machine verification via random simulation |

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
