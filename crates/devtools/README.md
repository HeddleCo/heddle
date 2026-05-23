# heddle-devtools

Workspace-internal tooling. Not published to crates.io.

## Commands

- `heddle-devtools web-proto [--check]` — regenerate (or audit) the
  TypeScript bindings for the gRPC service. With `--check` the tool
  generates into a tempdir and diffs against the checked-in output;
  used as a CI gate. Without it, writes to `web/src/lib/gen/proto/`.
- `heddle-devtools audit-idempotency` — fail if any state-changing
  RPC's request message is missing `string client_operation_id = 15`.
- `heddle-devtools audit-coverage <lcov> --gate <crate>=<pct> ...` —
  parse `lcov.info` and fail if any gated crate falls below its
  per-crate line-coverage threshold.
- `heddle-devtools check-no-silent-default-tree-load` — repo-wide
  audit that no code path silently loads the default working tree
  (see the source for the full rule).

## Single-source proto contract

There is **exactly one** copy of `service.proto` in this workspace:

```
crates/grpc/proto/heddle/v1/service.proto
```

Every consumer reads from that path:

- `crates/grpc/build.rs` — tonic-prost codegen for the Rust server
  and client.
- `crates/devtools/src/main.rs::run_web_proto` — TypeScript bindings
  (consumed by tapestry).
- `crates/devtools/src/main.rs::run_audit_idempotency` — proto-side
  idempotency lint.
- `crates/cli/tests/idempotency_lint.rs` — server-side dedup lint.

This file lives inside `crates/grpc/` so the crate's published tarball
contains everything `cargo build` needs from a fresh download. Do not
re-introduce mirror copies under `proto/` or elsewhere; the historical
mirrors drifted (missing `RedactionTransfer` before heddle#63 r1)
because nothing audited the duplication. The regression test
`heddle-devtools::tests_proto_single_source::only_canonical_proto_exists`
pins the contract.
