# Unwrap and Expect Policy

Heddle should reach zero production `unwrap` and `expect` call sites before 1.0. Until that cleanup is complete, the gate is per-crate: crates that already have no production `clippy::unwrap_used` or `clippy::expect_used` diagnostics deny those lints at the crate root so they cannot regress.

Use this crate-root pattern for already-clean crates:

```rust
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]
```

The `not(test)` carve-out keeps inline `#[cfg(test)]` modules free to use local `unwrap` or `expect` where that keeps tests readable. Do not add these lints as workspace-wide warnings or workspace lints while non-clean crates still contain production call sites, because the main CI clippy gate runs with `-D warnings`.

The SAFE class of intentional panics, including mutex-poison `.lock().expect(...)` and unwraps guarded by an immediate prior check, should converge on a uniform helper plus a local lint allow. The helper itself is tracked separately from this gate scaffolding.

## Current Inventory

Counts below are unique primary clippy diagnostics from `cargo clippy --workspace --all-targets -- -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic`, excluding test and bench targets and paths under `tests/` or `benches/`.

| crate | unwrap_used | expect_used | panic |
|---|---:|---:|---:|
| heddle-cli | 1099 | 336 | 58 |
| heddle-cli-macro-poc | 0 | 2 | 0 |
| heddle-cli-shared | 0 | 21 | 2 |
| heddle-client | 23 | 123 | 6 |
| heddle-crypto | 0 | 78 | 5 |
| heddle-daemon | 183 | 95 | 8 |
| heddle-devtools | 32 | 12 | 0 |
| heddle-format | 25 | 5 | 1 |
| heddle-grpc | 0 | 0 | 0 |
| heddle-ingest | 478 | 79 | 3 |
| heddle-merge | 8 | 2 | 19 |
| heddle-mount | 373 | 337 | 11 |
| heddle-objects | 568 | 117 | 13 |
| heddle-oplog | 303 | 5 | 7 |
| heddle-refs | 302 | 7 | 1 |
| heddle-repo | 1993 | 430 | 22 |
| heddle-review | 57 | 15 | 1 |
| heddle-runtime-bridge | 0 | 18 | 1 |
| heddle-schema | 37 | 0 | 2 |
| heddle-semantic | 189 | 58 | 89 |
| heddle-state-review | 2 | 0 | 0 |
| heddle-wire | 96 | 2 | 0 |
| weft-client-shim | 0 | 0 | 0 |
