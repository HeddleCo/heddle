# heddle-devtools

Workspace-internal tooling. Not published to crates.io.

## Commands

- `heddle-devtools audit-coverage <lcov> --gate <crate>=<pct> ...` —
  parse `lcov.info` and fail if any gated crate falls below its
  per-crate line-coverage threshold.
- `heddle-devtools check-no-silent-default-tree-load` — repo-wide
  audit that no code path silently loads the default working tree
  (see the source for the full rule).

The public protobuf contract, descriptor audits, and Rust/TypeScript generation
tooling are owned by [`HeddleCo/api`](https://github.com/HeddleCo/api). Heddle's
developer tools intentionally do not generate or publish API packages.
