# heddle-devtools

Workspace-internal tooling. Not published to crates.io.

## Commands

- `heddle-devtools grpc-ts [--check]` — regenerate (or audit) the
  TypeScript protobuf and Connect client package for the gRPC service.
  With `--check` the tool generates into a tempdir and diffs against
  the checked-in output; used as a CI gate. Without it, writes to
  `clients/grpc/src/gen/` and syncs `clients/grpc/package.json` to the
  `heddle-grpc` crate version.
- `heddle-devtools audit-grpc-contract` — compile the canonical descriptor and
  verify every RPC's explicit effect and deduplication options. Retry-safe RPCs
  must reach exactly one singular string `client_operation_id` field marked as
  the idempotency key; durable writes without deduplication are reported as
  known limitations.
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

The entrypoint directly imports every schema file. Consumers derive their
inventory from either that complete import closure or the canonical directory:

- `crates/grpc/build.rs` — discovers the full canonical tree for tonic-prost
  server and client codegen.
- `crates/devtools/src/main.rs::run_grpc_ts` — discovers that same tree for the
  TypeScript protobuf/Connect package and derives its root exports.
- `crates/devtools/src/audit_grpc_contract.rs` — descriptor-driven RPC effect,
  deduplication, and idempotency-key audit from the complete entrypoint.

Hosted handler coverage belongs in Weft, where the implementations and service
registration live. Heddle intentionally does not source-scan that sibling repo.

This file lives inside `crates/grpc/` so the crate's published tarball
contains everything `cargo build` needs from a fresh download. Do not
re-introduce mirror copies under `proto/` or elsewhere; the historical
mirrors drifted (missing `RedactionTransfer` before heddle#63 r1)
because nothing audited the duplication. The regression test
`heddle-devtools::tests_proto_single_source` pins the single tree, complete
entrypoint, and TypeScript export inventory.
