# Workspace Split Plan

This document captures the recommended plan for splitting Heddle from a single crate into a Cargo workspace. It is written to preserve context if the working session needs to be compacted or resumed later.

## Goals

- Reduce compile scope and dependency surface per binary/runtime.
- Separate domain logic from hosted runtime and CLI concerns.
- Make the hosted server independently deployable.
- Keep the `heddle` binary name stable.
- Preserve a low-risk migration path with incremental validation.

## Recommended Workspace Layout

Create a root workspace with these members:

```text
Cargo.toml                  # workspace manifest
crates/
  objects/
  proto/
  server/
  cli/
  semantic/           # optional but recommended
```

### Crate Roles

- `objects`
  - repository model, object model, refs, oplog, worktree, storage abstractions, merge/diff core, bridge logic
- `proto`
  - shared protocol/message/auth/client transport surface used by CLI and server
- `server`
  - hosted gRPC services, auth, Postgres-backed hosted registry, event fanout, server runtime
- `cli`
  - `heddle` binary, command parsing, command implementations, local workflows, hosted client invocation
- `semantic`
  - semantic analysis and parser-heavy functionality

## Why Not Only `cli` and `server`

The current repo has at least four real boundaries:

- shared domain logic
- transport/client/protocol logic
- hosted server/runtime logic
- semantic/parser-heavy logic

Splitting only into `cli` and `server` would still leave `core`, `proto`, and `semantic` tangled and would not materially reduce coupling enough.

## Module Move Plan

### `objects`

Move these first because they are the most reusable and least server-specific:

- `src/object/**`
- `src/crypto/**`
- `src/lock/**`
- `src/hooks/**`
- `src/worktree/**`
- `src/bridge/**`
- `src/refs/**` except `src/refs/pg_refs.rs`
- `src/oplog/**` except `src/oplog/pg_oplog.rs`
- `src/store/**` except:
  - `src/store/pg_registry.rs`
  - `src/store/pg_hosted_registry.rs`
- `src/protocol/delta/**`
- `src/protocol/sync.rs`

Strong candidate utility moves into core support modules:

- shared fs/atomic helpers from `src/store/atomic.rs`
- generic config or path helpers that are reused by refs/store/worktree

### `proto`

Move shared transport-facing modules here:

- `src/protocol/auth/**`
- `src/protocol/capabilities.rs`
- remaining `src/protocol/message/**`
- `src/client/**`
- `src/remote/**`

Important: do **not** keep server-specific error conversions in this crate.

### `server`

Move hosted/runtime/server-only modules here:

- `src/server/**`
- `src/ephemeral/**`
- `src/store/pg_registry.rs`
- `src/store/pg_hosted_registry.rs`
- `src/refs/pg_refs.rs`
- `src/oplog/pg_oplog.rs`

Server-specific repository opening helpers such as hosted/server-only repo factories should live here, not in `objects`.

### `cli`

Move CLI-only code here:

- `src/main.rs`
- `src/cli/**`

This crate should own the `heddle` binary target.

### `semantic`

Move parser-heavy semantic logic here:

- `src/semantic/**`

This keeps tree-sitter and language grammar churn out of the core and server by default.

## Dependency Placement

### `objects`

Keep only dependencies needed for core functionality:

- `serde`, `serde_json`, `rmp-serde`, `toml`
- `chrono`
- `blake3`, `hex`, `rand`, `base32`
- `thiserror`, `anyhow`
- `walkdir`, `ignore`, `memmap2`, `fs2`, `similar`
- crypto/signing crates:
  - `ed25519-dalek`
  - `p256`
  - `rsa`
  - `pkcs8`
  - `sec1`
  - `signature`
  - `sha2`
  - `base64`
- optional storage/compression crates as needed:
  - `zstd`
  - `aws-config`
  - `aws-sdk-s3`
  - `async-trait`
- possibly `gix` if Git bridge remains in core

Try to keep `tokio` out of `objects` unless it is truly required for storage backends that stay there.

### `proto`

- `tokio`
- `tokio-util`
- `tokio-stream`
- `bytes`
- `futures`
- `serde`
- `rmp-serde`
- `thiserror`
- `tracing`
- optional TLS transport deps if still needed by shared client layers

### `server`

- `tonic`
- `tonic-web`
- `tower-http`
- `sqlx`
- `uuid`
- `biscuit-auth`
- `tokio`
- `async-stream`
- `tracing`

### `cli`

- `clap`
- `clap_complete`
- `anyhow`
- `atty`
- `tracing-subscriber`
- `tokio`

### `semantic`

- `tree-sitter`
- language grammar crates
- semantic-analysis-specific helpers only

## Known Cyclic Dependency Risks

### Risk 1: `core` <-> `proto`

Current compression and pack paths use protocol delta helpers. If `protocol::delta` stays outside core, `core` will depend on `proto`.

Resolution:

- move `protocol/delta` into `objects`
- move `protocol/sync.rs` into `objects`

### Risk 2: `proto` <-> `server`

Current protocol error mapping includes server-side registry conversions.

Resolution:

- remove server-specific conversions from protocol
- perform registry/auth/service error mapping inside `server`

### Risk 3: fs/store helper scattering

`store/atomic.rs`-style utilities are reused in refs/store/server code.

Resolution:

- move them into a neutral `objects` internal support module before splitting crates

### Risk 4: semantic coupling to repository

Semantic diff logic depends on repository types and worktree/core objects.

Resolution:

- keep `semantic -> objects`
- do not let `objects` depend back on `semantic`

## Lowest-Risk Migration Sequence

### Phase 1: Convert root crate to a workspace shell

- create workspace root `Cargo.toml`
- create `crates/core`, `crates/server`, `crates/cli`
- keep the existing root source tree compiling until moves happen

### Phase 2: Extract `objects`

- move pure/shared modules first
- move delta/sync logic into core before introducing `proto`
- make tests for moved modules pass in the new crate

### Phase 3: Extract `semantic`

- move semantic analysis and tree-sitter deps out
- update CLI/core call sites to depend on `semantic`

### Phase 4: Extract `proto`

- move protocol auth/capabilities/message/client/remote modules
- remove server-specific protocol error conversions

### Phase 5: Extract `server`

- move hosted runtime and Postgres-backed server pieces
- make `server` depend on `objects` and `proto`

### Phase 6: Extract `cli`

- move `main.rs` and `src/cli/**`
- keep binary name as `heddle`
- update integration tests to target the new package

### Phase 7: Cleanup and feature normalization

- split features so they are crate-local where possible
- use feature forwarding only when it makes migration easier
- update docs, Dockerfiles, CI, and scripts

## Test Migration Plan

### `cli`

Move CLI-facing tests here:

- current CLI integration tests
- anything using `CARGO_BIN_EXE_heddle`

### `server`

Move hosted/server/runtime tests here:

- Postgres hosted registry integration tests
- hosted gRPC integration tests
- hosted CLI gRPC integration tests if they boot a server runtime locally

### `semantic`

Move semantic integration tests here:

- parser/semantic diff integration tests

### `objects`

Keep unit tests close to moved modules.

## Build / Packaging Implications

- Docker should build `cli` binary explicitly, e.g. `cargo build -p cli --bin heddle ...`
- hosted/server-only images may later target `server` directly if you want a dedicated server binary
- scripts such as local migration helpers and integration test runners should point at package-specific commands once the split lands

## Docs To Update When Implementing

- `README.md`
- `SPEC.md`
- `docs/ARCHITECTURE.md`
- command/testing docs under `.agents/`
- deployment docs and Docker build instructions

## Suggested First Implementation Cut

If we want the smallest credible first step, do this:

1. create workspace
2. extract `objects`
3. extract `server`
4. extract `cli`
5. then peel off `semantic` and `proto`

If we want the cleanest long-term boundaries immediately, do this:

1. create workspace
2. extract `objects`
3. move `delta` and sync logic into core
4. extract `proto`
5. extract `server`
6. extract `cli`
7. extract `semantic`

## Current Session Notes

As of this plan:

- hosted transport has been rewritten to gRPC / gRPC-Web
- legacy TCP client/server stack has been removed from the active path
- hosted integration suites are green
- docs/spec already describe gRPC as the canonical hosted transport

That makes now a good time to do the crate split, because transport boundaries are already much clearer than before.
