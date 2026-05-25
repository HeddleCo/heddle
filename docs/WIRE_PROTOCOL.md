# Phase 4 (Wire Protocol) Summary

This phase adds a minimal, local-testable wire protocol and remote operations (push/pull) for Heddle.

## Status: Complete

The wire protocol is fully implemented and tested. All core VCS commands are operational.

## Protocol Overview

- Transport: TCP.
- Framing: length-delimited messages, encoded as `[type: u8][len: u32-be][payload: bytes]`.
- Payload encoding: MessagePack, with structs encoded as maps (`rmp_serde::to_vec_named`) so new fields can be added without breaking older decoders.
- Negotiation: client sends `Hello` with `Capabilities`, server replies `HelloAck`, and both sides compute an intersection.

## Authentication

- Implemented: token-based authentication.
- Wire format: client sends an opaque token id (bytes) in `Auth`.
- Server authority: the server assigns identity/permissions based on its configured token database. Clients cannot self-assign permissions.
- `AuthAck` now includes the server-authoritative token scope so clients can introspect effective hosted visibility after connect.
- TLS: deferred for MVP. `heddle` client configuration and `hosted` server configuration both keep TLS-related fields so the design can be extended without changing the core message framing.

Environment-based local testing:

- `hosted` owns the server runtime and reads server config from its server config file or `HEDDLE_SERVER_*` overrides.
- `heddle` client commands use user config plus `HEDDLE_REMOTE_*` overrides for remote auth and TLS profiles.
- `HEDDLE_REMOTE_TOKEN=<token-id>` provides the token id used by `heddle push`/`heddle pull`.

## Hosted Admin Operations

The wire protocol now includes hosted control-plane messages for:

- namespace create/list/update/delete
- repository create/list/update/delete
- grant create/list/delete

These messages are used by the hosted client helpers and `hosted ...`
CLI commands in the hosted binary.

Hosted admin authorization is server-side and combines:

- `Permission::Admin`
- token scope (`Global`, `NamespaceTree`, `Repositories`)
- effective hosted role when grant records exist for the caller

## Reference Advertisement

`ListRefs -> RefsList` returns:

- `head`: `Attached { thread }` or `Detached { state }`
- `head_state`: resolved `ChangeId` if HEAD currently points to an existing state
- `refs`: thread + marker entries

## Object Transfer

Object sync is raw-object (no deltas/compression used in MVP).

Reachable object closure:

- `State` objects: the requested state and all ancestor states
- `Tree` objects: all trees referenced by those states
- `Blob` objects: all blobs referenced by those trees

Objects are transferred as `ObjectData { id, obj_type, data, is_delta=false }`.

## Push Flow

1. Client enumerates the state closure and sends `PushRequest { local_state, target_thread, force, create_thread=true, objects }`.
2. Server replies `PushReady { remote_head, have_objects, want_objects }`.
3. Client streams `ObjectData` for each `want_objects` entry.
4. Server stores objects, enforces fast-forward unless `force`, updates the target thread, and replies `PushComplete`.

## Pull Flow

1. Client sends `PullRequest { remote_thread, local_thread?, target_state? }`.
2. Server replies `PullReady { remote_state, objects_to_fetch }` (the full closure inventory).
3. Client computes missing objects locally and sends `WantObjects` for only those.
4. Server streams `ObjectData` for each requested object and replies `PullComplete`.

## CLI Usage

Configure a remote:

```bash
heddle remote add origin 127.0.0.1:8421
heddle remote list
```

Serve the current repository:

```bash
hosted serve --bind 127.0.0.1 --port 8421
```

Push/pull:

```bash
heddle push origin main
heddle pull origin --thread main --local-thread main
```

With token auth (local dev):

```bash
export HEDDLE_SERVER_REQUIRE_AUTH=1
export HEDDLE_SERVER_TOKEN=devtoken

export HEDDLE_REMOTE_TOKEN=devtoken
heddle push origin main
heddle pull origin --thread main --local-thread main
```

For normal client usage, prefer storing remote auth and TLS settings in user config at `~/.config/heddle/config.toml` or the `HEDDLE_CONFIG` override. Keep server storage, database, and TLS values in the server config file selected by `HEDDLE_SERVER_CONFIG` or `hosted --config`.

## Test Coverage

- Wire protocol tests in `tests/wire_protocol.rs`
- Auth tests in `tests/auth.rs`
- Integration with push/pull in `tests/production_features.rs`

## Limitations / Deferred

- TLS is not implemented (config scaffolding exists).
- Delta compression is not used in the MVP transfer flows (delta types exist but are not negotiated as enabled by default).
- Auth token database is in-memory (no persistence/rotation).
- No `file://` protocol support for local paths (remotes require host:port format).
- Grant management is currently create/list/delete over the Heddle protocol; richer membership models and explicit deny semantics remain deferred.

## Implemented Commands

All core VCS commands are implemented:

| Category | Commands |
|----------|----------|
| Repository | `init`, `status`, `snapshot`, `log`, `show`, `goto`, `diff`, `clean`, `fsck`, `gc` |
| Threads/Markers | `thread`, `marker`, `fork` |
| History | `undo`, `redo`, `blame`, `bisect` |
| Branching | `merge`, `rebase`, `resolve`, `cherry-pick`, `revert` |
| Stashing | `stash` |
| Remote | `remote`, `fetch`, `push`, `pull`, `clone`, `serve` |
| Git Bridge | `bridge` |
| Utilities | `compare`, `collapse`, `completion` |
