---
status: accepted
---

# Net-new public API contract and coordinated cutover

`HeddleCo/api` is the sole owner of the shared protobuf source, compiled
descriptors, generation tooling, compatibility policy, and Rust and TypeScript
SDK releases. The public wire package is `heddle.api.v1alpha1`; the published
packages are `heddle-api` and `@heddleco/api`. Heddle, Weft, and Tapestry are
consumers and adapters around this neutral contract. Heddle does not retain a
schema copy or a second generation path.

This is a net-new, intentionally incompatible contract. It does not relocate
or preserve `heddle.v1`, and it does not use the intermediate 0.23/0.24
sequence described by ADR 0045. The old `heddle-grpc` and `@heddleco/grpc`
packages are frozen and deprecated. Consumers exact-pin every `0.x` release.
Breaking pre-1.0 changes increment the minor version and require a checked-in
breaking-change report plus coordinated consumer release candidates.

## Contract boundary

Version 0.1.0 has ten `SHIPPED` domain interfaces: identity, registry,
repository, collaboration, state review, pull-request review, workflow,
search, attention, and repository sync. Producer coverage is derived from
compiled service metadata and requires every shipped interface declared for a
deployment target.

`AgentGatewayService` and `AgentService` are `PLANNED` contract-first
interfaces. They are present so replay, policy, intervention, privacy, and
retention semantics can be validated before an implementation ships. They are
excluded from shipped producer-coverage gates and must not be registered or
documented as live in 0.1.0.

Only production-reachable shared behavior was retained. Dormant recovery,
handle/proof/history, transaction, operation-log, hook transport, local
Timeline transport, and other proto-only surfaces were removed. Native Heddle
CLI and daemon behavior remains internal Rust behavior where no cross-product
contract is required. A checked-in migration manifest in `HeddleCo/api`
classifies every old RPC and records production evidence for each retained
method.

## Request and compatibility policy

RPC effect, signing tier, retry behavior, client-operation-ID requirements,
deployment target, and maturity are compiled descriptor options. Durable
writes carry exactly one direct `client_operation_id`. Request signatures use
the stable `heddle-req-sig-v1` identity and `x-heddle-sig-*` headers. Typed
identifiers, typed errors, deterministic protobuf encoding, directional sync
frames, and opaque cursors keep invalid states out of the transport model.

The API repository uses Buf v2 and pinned generators. Generated source is a
release artifact, not committed authority. Rust releases publish to crates.io;
the scoped TypeScript package currently publishes to GitHub Packages. A tag
builds and tests both packages, verifies deterministic generation, and attaches
source, descriptor, and package checksums to one GitHub release. A partial
publication rolls forward to a new version.

## Cutover

There is one coordinated maintenance-window cutover with no dual service
registration and no compatibility shim. Weft serves only
`heddle.api.v1alpha1`; Heddle and Tapestry consume exact 0.1.0 packages. The old
wire routes are absent after traffic resumes. External clients of the old
package are intentionally unsupported.

## Consequences

- Shared API changes are proposed and released from `HeddleCo/api`, not this
  repository.
- Heddle owns its local domain model and adapts it to generated API types only
  at hosted boundaries.
- Local behavior does not become public protobuf merely because a daemon,
  example, test, or dormant handler once used it.
- At 1.0, the wire package advances to `heddle.api.v1`; future incompatible
  contracts require a new package generation.
- Agent live ingest, hosted query, policy, and intervention remain planned
  until honest producer and adapter implementations pass their conformance
  gates.
