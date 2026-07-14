---
status: superseded
superseded-by: ADR 0048
---

# API Contract Ownership and Governance

This decision described an intermediate `heddle.v1` relocation through SDK
0.23 and 0.24. That sequence was not used. ADR 0048 supersedes it with the
net-new `heddle.api.v1alpha1` contract and coordinated hard cutover. The text
below is retained as historical decision context.

The shared `heddle.v1` protobuf contract will move from the Heddle product
repository to a neutral `HeddleCo/api` repository. That repository owns schema
source, descriptors, compatibility policy, Rust and TypeScript generation, and
coordinated SDK publication. Heddle owns local implementations, Weft owns hosted
implementations, and Tapestry remains a consumer.

The move follows a redesign, publish, extract, then cut over sequence. Heddle
publishes the approved identity split and pre-1.0 pruning as SDK 0.23. After
Heddle, Weft, and Tapestry consume that release, its exact contract becomes the
input to `HeddleCo/api`. The API repository publishes the unchanged relocation
as 0.24. Consumer changes and repository relocation do not share a release.

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

SDK 0.23 distinguishes immutable 32-byte physical `StateId`, rewrite-stable
16-byte `ChangeId`, validated `SpoolId`, and Git revision/OID identity. Physical
state fields carry `StateId`; logical lineage fields carry `ChangeId`; the CLI
encodes them as `hs-` and `hc-`. The 0.24 API-repository release preserves that
wire contract unchanged.

## Authority transfer

The protected API release tag is the authority-transfer event. Before 0.24 is
published, Heddle's frozen 0.23 tree is historical input. After publication,
Heddle switches its three direct consumers to exact registry version 0.24 and
deletes its schema, generators, generated TypeScript, and publication jobs in
one change. There is no dual writable source or vendored compatibility lane.

If a gate fails before publication, authority does not move. If either immutable
package has published, repair rolls forward with a new shared version.

## Consequences

- Heddle's current proto tree remains the source only through the extraction and
  consumer migration.
- Generated artifacts are publication outputs, never a second schema source.
- New RPCs must declare their effect and deduplication contracts explicitly.
- Method-name heuristics and source-text parsers are not contract governance.
- Pre-1.0 breaking changes remain possible, but they are explicit, coordinated,
  and verified across all consumers.
