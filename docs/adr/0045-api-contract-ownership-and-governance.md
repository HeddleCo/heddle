---
status: accepted
---

# API Contract Ownership and Governance

The shared `heddle.v1` protobuf contract will move from the Heddle product
repository to a neutral `HeddleCo/api` repository. That repository owns schema
source, descriptors, compatibility policy, Rust and TypeScript generation, and
coordinated SDK publication. Heddle owns local implementations, Weft owns hosted
implementations, and Tapestry remains a consumer.

The move follows a clean, then extract, then cut over sequence. Heddle first
establishes one flat `heddle.v1` source inventory and one complete entrypoint.
The first API-repository release preserves service paths, request field numbers,
tonic byte mappings, and generated SDK behavior. Consumer changes and schema
redesigns do not share a release with repository relocation.

## RPC contract metadata

Every RPC declares two independent protobuf options:

- `effect`: `READ_ONLY`, `TRANSIENT_WRITE`, or `DURABLE_WRITE`;
- `deduplication`: `NONE` or `CLIENT_OPERATION_ID`.

`READ_ONLY` means the handler does not mutate authoritative server state.
`TRANSIENT_WRITE` covers challenge, session, replay-cache, or derived-job state
whose loss does not alter repository or control-plane truth. `DURABLE_WRITE`
covers repository, identity, governance, review, or other authoritative state.

When deduplication is `CLIENT_OPERATION_ID`, exactly one reachable request field
must carry the `idempotency_key` field option. It must be a singular string named
`client_operation_id`, and every edge from the RPC input to that field must be
singular. Streaming envelopes are traversed through their descriptors rather
than recognized by message or method names. `NONE` requires no reachable marker,
and `READ_ONLY` requires `NONE`.

The descriptor audit reports every `DURABLE_WRITE + NONE` RPC as a known retry
limitation. Those entries are honest gaps for coordinated API and handler work,
not allow-list exceptions.

## Consumer cutover gates

- The API repository rejects lint failures and breaking changes relative to the
  last published contract.
- One release tag builds and verifies both SDKs before publishing either.
- Weft must have fatal, descriptor-derived handler coverage for every hosted RPC
  it serves before its dependency cutover. Heddle does not source-scan an
  out-of-tree Weft implementation.
- Heddle local-daemon behavior, Weft hosted behavior, and Tapestry build tests
  remain in their owning repositories.
- The Heddle schema and generated clients are deleted only after a published API
  release has passed Heddle, Weft, and Tapestry integration gates.

## Identity boundary

The existing contract uses `state_id`, `change_id`, revision-address strings,
and Git object identifiers inconsistently. Extraction must not treat those names
as one coherent identity type. Before a stable v1 contract, a coordinated API and
consumer change must distinguish physical `StateId`, rewrite-stable `ChangeId`,
and Git revision/OID identity, with typed wire representations and one canonical
CLI display encoding. Repository relocation may preserve the current wire shape
for parity, but it does not settle that semantic design.

## Consequences

- Heddle's current proto tree remains the source only through the extraction and
  consumer migration.
- Generated artifacts are publication outputs, never a second schema source.
- New RPCs must declare their effect and deduplication contracts explicitly.
- Method-name heuristics and source-text parsers are not contract governance.
- Pre-1.0 breaking changes remain possible, but they are explicit, coordinated,
  and verified across all consumers.
