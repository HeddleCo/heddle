---
status: accepted
implementation: blocked
---

# Net-new public API contract and coordinated cutover

The accepted direction is a net-new, intentionally incompatible public
contract owned by `HeddleCo/api`. The target wire package is
`heddle.api.v1alpha1`; the target Rust and TypeScript packages are
`heddle-api` and `@heddleco/api`. Heddle, Weft, and Tapestry will be consumers
and adapters around that neutral contract after a coordinated cutover.

The direction is accepted, but the cutover is not complete and this ADR is not
evidence that it is safe to merge a consumer independently. The current API
candidate, Heddle adapter, Weft producer, Tapestry consumers, and package
publication are at different stages. The legacy `heddle.v1`, `heddle-grpc`,
and `@heddleco/grpc` surfaces remain live dependencies until the coordinated
window succeeds.

This decision supersedes the intermediate 0.23/0.24 relocation sequence in
ADR 0045. It does not authorize a compatibility shim or allow live behavior to
be discarded merely because a caller was missed by the migration inventory.

## Intended contract boundary

The API candidate declares its production-target domain interfaces `SHIPPED`
and keeps `AgentGatewayService` and `AgentService` explicitly `PLANNED`.
Producer-coverage gates exclude the planned services, which must not be
registered or described as live.

Native Heddle CLI and daemon behavior remains internal Rust behavior when no
cross-product contract is required. Shared behavior with a live Weft producer
or Heddle/Tapestry consumer must instead survive the cutover through a
v1alpha1 RPC or a separately approved product replacement. The migration
manifest in `HeddleCo/api` is a candidate inventory, not an authoritative
cutover approval while the discrepancies below remain open.

## Current implementation status

Status was verified on 2026-07-14 against API `f32adc1f`, Weft `6accf761`,
Tapestry `origin/feat/app-site-overhaul` at `7e729160`, Tapestry `origin/main`
at `b234795c`, and this Heddle cutover branch after merging Heddle
`origin/main` at `b0443f91`.

| Component | Current state | Cutover consequence |
|---|---|---|
| API | The v1alpha1 source and generators exist, but the migration manifest drops live or implemented behavior and signing-identity semantics are not conformance-pinned. | The API revision is not ready to publish or re-pin as the coordinated contract. |
| Heddle | This draft branch consumes API revision `f32adc1f` and no longer owns the legacy generated contract. Its latest `origin/main` merge and session-test port are complete. | Heddle-local compilation is necessary but does not establish hosted interoperability. |
| Weft | `crates/weft-server/Cargo.toml:49` still depends on `heddle-grpc` 0.23.0, and `crates/weft-server/src/serve.rs:130-157` registers legacy `heddle.v1` services. | A v1alpha1 Heddle call has no matching hosted route until a separate Weft cutover lands. |
| Tapestry | The overhaul branch still depends on `@heddleco/grpc` 0.23.0 (`package.json:50`) and imports generated `heddle/v1` modules in `src/lib/server/api.ts:12-84` and other routes. | A separate Tapestry cutover and caller audit must land in the same release window. |
| Publication | Heddle currently exact-pins a git revision in `crates/client/Cargo.toml:40`; `heddle-api` 0.1.0 is not yet available through this workspace's release pipeline. | The API package must publish before a Heddle crates.io release can include this dependency. |

## Contract correction decision matrix

These rows are prerequisites, not optional follow-ups. They retain current
behavior until the API and each consumer have a tested replacement.

| Area | Grounded current behavior | Required API/cutover decision | Status |
|---|---|---|---|
| Native handles | Tapestry `origin/main` exposes live status, request, and claim routes (`src/routes/api/handles/{status,request,claim}/+server.ts`) backed by calls in `src/lib/server/api.ts:1964-2003`. Weft implements `GetHandleStatus`, `RequestHeldName`, `ClaimHandle`, and `ResolveHandle` in `crates/weft-server/src/server/grpc_hosted_impl/user.rs:375-850`. The API manifest currently classifies all four as dropped (`migration-manifest.json:432-434,526-528,636-648`). | Add v1alpha1 handle RPCs, or approve and implement an explicit product replacement that preserves the live handle flows. They may not be dropped on the basis of the current inventory. | **Blocked — API issue/PR required** |
| Subscription and billing reconciliation | Weft implements the billing-authorized `RecordSubscription` write at `crates/weft-server/src/server/grpc_hosted_impl/auth.rs:2091-2189`. The Tapestry overhaul Polar route is still a fulfillment stub and names this RPC as part of the missing entitlement path (`src/routes/api/webhook/polar/+server.ts:13-62`). The API manifest drops it at `migration-manifest.json:142-144`. | Retain `RecordSubscription`, or land a reviewed reconciliation replacement and port its external caller before removing the old RPC. | **Blocked — API, Weft, and Tapestry work required** |
| Review verdicts | Weft implements `RecordVerdict` as a first-class idempotent write at `crates/weft-server/src/server/grpc_hosted_impl/state_review.rs:308-370`; the API's `StateStatus` documentation still refers to `RecordVerdict` (`proto/heddle/api/v1alpha1/types.proto:41-44`) while the migration manifest drops it (`migration-manifest.json:846-848`). | Add a first-class v1alpha1 `RecordVerdict` RPC and port the producer. A verdict remains distinct from `SignState`; it is not dropped behavior. | **Blocked — API and Weft work required** |
| Provider-token lifecycle | The API candidate retains `IdentityService/StoreProviderToken` (`proto/heddle/api/v1alpha1/identity.proto:545-554,861-869`). Weft encrypts stored access and refresh tokens (`crates/weft-server/src/pg_registry.rs:2107-2197`), but its provider-token import branch explicitly fails because token resolution is not wired into the worker (`crates/weft-server/src/import_worker.rs:1747-1756`). Tapestry's overhaul stores GitHub and GitLab tokens through `src/lib/server/api.ts:1821-1845` and represents private imports with `ProviderTokenSource` at `src/lib/server/api.ts:2087-2130`. | Preserve storage and encrypted server handling, then define and implement private-import consumption, expiry/refresh, and revocation semantics as one audited lifecycle before declaring the caller cut over. | **Blocked — API/Weft semantics and Tapestry port required** |
| Current Tapestry callers | The overhaul imports legacy identity, registry, repository/content, feed, import, search, workflow/thread, discussion, state-review, review-analysis, and repository-event generated modules across `src/lib/server/api.ts`, `src/lib/server/review-api.ts`, `src/routes/api/review`, `src/routes/app/events`, and feed routes. | Audit every legacy import and invoked method against the corrected descriptor, then port and test all callers. The API drop list is not approved by checking only Heddle call sites. | **Blocked — complete consumer inventory and Tapestry PR required** |
| Signing identity | Heddle PoP canonicalization uses `hex(device_public_key)` (`crates/client/src/grpc_hosted/request_signing.rs:125-145`), while the API fixture uses `principal:alice` (`tests/fixtures/unary-signing-v1.json:2`). Heddle's human retry reuses that canonical byte string but sends `hex(credential_id)` as the identity header (`request_signing.rs:204-220`). | Define the stable identity value for each signing tier, reconcile canonical bytes with the identity header, and add human-tier conformance vectors in API before changing the pin. | **Blocked — security contract issue/PR and Fable gate required** |
| Producer and consumers | Weft registers only legacy routes and Tapestry overhaul imports only the legacy package; this Heddle branch already calls v1alpha1 paths. | Land separate Weft and Tapestry cutover PRs and deploy all three consumers in one recorded maintenance window. | **Blocked — coordinated release required** |
| Package release | The Heddle branch uses an exact git revision plus `version = "=0.1.0"`; API publication is outside Heddle's release workflow. | Publish the corrected `heddle-api` 0.1.0 release before the next Heddle release-plz publication and replace the temporary git pin as part of the coordinated work. | **Blocked — API package publication required** |

## Coordinated-cutover checklist

No Heddle, Weft, Tapestry, or API PR may claim that the cutover is complete
until every box below is backed by its owning PR and test evidence.

### Correct and release the contract

- [ ] Re-audit every dropped migration-manifest RPC against current Weft,
  Tapestry `origin/main`, and Tapestry `origin/feat/app-site-overhaul` callers.
- [ ] Restore or replace the four live handle RPCs without removing handle
  functionality.
- [ ] Retain `RecordSubscription` until the billing reconciliation caller and
  replacement, if any, are live.
- [ ] Add first-class `RecordVerdict` to v1alpha1 and port its Weft producer.
- [ ] Specify and test the complete provider-token lifecycle used by OAuth and
  private imports.
- [ ] Pin PoP and human signing-identity semantics with conformance fixtures.
- [ ] Publish corrected Rust and TypeScript API packages and record their exact
  versions and source revision.

### Prepare every producer and consumer

- [ ] Weft serves the corrected v1alpha1 descriptor and updates route policy,
  request signing, reflection, health, and handler conformance tests.
- [ ] Tapestry replaces every legacy generated import and ports its API module,
  review/event/feed routes, OAuth/provider-token flows, Polar reconciliation,
  and live handle flows.
- [ ] Heddle re-pins the corrected API revision/package and reruns its hosted
  adapter, signing, session, CLI, daemon, schema, and supply-chain gates.
- [ ] Cross-product tests prove the exact Heddle and Tapestry clients can call
  the exact Weft build planned for the window.

### Execute the maintenance window

- [ ] Record the deployment order, traffic pause/resume criteria, smoke-test
  matrix, exact versions, and whole-cutover rollback points.
- [ ] Deploy the matched Weft producer and Heddle/Tapestry consumers without a
  mixed legacy/v1alpha1 serving period.
- [ ] Verify handles, subscription reconciliation, verdict recording,
  provider-token-backed private import, and the audited Tapestry call surface
  before traffic resumes.
- [ ] Obtain the required Fable approval, then remove the Draft/do-not-merge
  hold. Until then, this cutover remains blocked.

## Request and compatibility policy

RPC effect, signing tier, retry behavior, client-operation-ID requirements,
deployment target, and maturity are compiled descriptor options. Durable
writes carry exactly one direct `client_operation_id`. The intended request
signature format is `heddle-req-sig-v1` with `x-heddle-sig-*` headers, subject
to the unresolved signing-identity row above.

The API repository uses Buf v2 and pinned generators. Generated source is a
release artifact, not committed authority. Its release workflow is designed to
publish Rust to crates.io and the scoped TypeScript package to GitHub Packages,
verify deterministic generation, and attach source, descriptor, and package
checksums to one GitHub release. That describes the release contract, not a
claim that 0.1.0 has already been published.

## Consequences

- Shared API changes are proposed and released from `HeddleCo/api`, not this
  repository.
- Heddle owns its local domain model and adapts it to generated API types only
  at hosted boundaries.
- Local behavior does not become public protobuf merely because a daemon,
  example, test, or dormant handler once used it.
- Live cross-product behavior cannot be removed without a complete caller and
  producer audit plus a coordinated replacement.
- At 1.0, the wire package advances to `heddle.api.v1`; future incompatible
  contracts require a new package generation.
- Agent live ingest, hosted query, policy, and intervention remain planned
  until honest producer and adapter implementations pass their conformance
  gates.
