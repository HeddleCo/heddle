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

Status was verified on 2026-07-15 against API `origin/main` at `726958dd`,
Weft `origin/main` at `939f8b40`, Tapestry PR 164's reviewed head at
`66fb3bd0` merged into `feat/app-site-overhaul` as `51c2c3e5`, and Heddle
`origin/main` at `48e5c20d`.
The API capability provenance below uses merged revision `726958dd`. Heddle's
Rust dependencies deliberately remain pinned to contract revision `e3b3e6d0`:
the later API commits change package/release metadata, capability declarations,
and contract tests, but do not change `proto/` or `src/` contract code.

| Component | Current state | Cutover consequence |
|---|---|---|
| API | Revision `e3b3e6d0` preserves the four handle operations on `IdentityService`, reserves legacy subject tag/name 1 from `HandlePrincipal`, and retains public fields on tags 2–8. Revision `726958dd` is the merged capability snapshot used by this branch. Other rows below and signing-identity semantics remain unresolved. | The corrected handle contract is exact-pinnable, but the complete cutover contract is not ready. |
| Heddle | This draft branch consumes API contract revision `e3b3e6d0`, attests its capability declaration against `726958dd`, has merged Heddle `main` through `48e5c20d`, and no longer owns the legacy generated contract. | Heddle-local compilation is necessary but does not establish hosted interoperability. |
| Weft | PR `HeddleCo/weft#592` merged as `939f8b40`. Production now registers the shared `IdentityService` alongside legacy services (`crates/weft-server/src/serve.rs:84-100,163-170`) and serves its four handle methods through the shared escrow/resolution adapter (`identity.rs:171-401`). Every other shared identity method is still explicitly `UNIMPLEMENTED` (`identity.rs:101-169`), and the other registered hosted services remain legacy. | The handle producer blocker is resolved; the complete v1alpha1 producer cutover is not. |
| Tapestry | PR `HeddleCo/tapestry#164` exact-pins `@heddleco/api@0.1.1` and ports the four handle flows. Its exact reviewed head `66fb3bd0` merged into `feat/app-site-overhaul` as `51c2c3e5`. The handle-only change does not remove Tapestry's legacy `@heddleco/grpc` dependency or port its non-handle callers. | The handle web adapter blocker is resolved on the feature branch; the non-handle consumer cutover remains open. |
| Publication | `@heddleco/api@0.1.1` is published. The Rust `heddle-api@0.1.1` crate is not; `HeddleCo/api#10` is reopened and blocked because the API repository lacks `CARGO_REGISTRY_TOKEN`. Heddle therefore retains the exact git pin in `crates/client/Cargo.toml`, `crates/cli/Cargo.toml`, and `crates/daemon/Cargo.toml`, and explicitly excludes those three packages at the Cargo, workflow, and release-plz publication boundaries. | Independently publishable Heddle crates can continue releasing, but the Rust crate must publish before Heddle can replace the temporary git pin and re-enable its three dependent packages. |

## Contract correction decision matrix

These rows are prerequisites, not optional follow-ups. They retain current
behavior until the API and each consumer have a tested replacement.

| Area | Grounded current behavior | Required API/cutover decision | Status |
|---|---|---|---|
| Native handles | API revision `e3b3e6d0` defines `IdentityService/ClaimHandle`, `GetHandleStatus`, `RequestHeldName`, and `ResolveHandle`. Weft PR 592 merged the shared adapter at `939f8b40`, routing those methods through normalized, subject/RPC-scoped retry keys and subject-free resolution (`identity.rs:71-99,171-401`). Tapestry PR 164's exact reviewed head `66fb3bd0` ports the matching status, request, claim, and resolution flows and pins capability provenance to `726958dd`; it merged into `feat/app-site-overhaul` as `51c2c3e5`. | Keep the merged subject-free projection, retry-key reuse, hidden failures, and route regressions intact while the separately blocked non-handle cutover proceeds. Owner: `HeddleCo/tapestry#163` / PR 164. | **Resolved for the handle slice** |
| Subscription and billing reconciliation | Weft still implements the legacy billing-authorized `RecordSubscription` write (`auth.rs:2108-2204`). Tapestry PR 164's Polar route still returns `fulfillment_not_ready` and names `RecordSubscription` as the missing entitlement write (`src/routes/api/webhook/polar/+server.ts:18-70`). The API manifest still drops it (`migration-manifest.json:142-146`) and exposes no replacement RPC. | Add the v1alpha1 write or land a reviewed reconciliation replacement, then port the Polar caller before removing the legacy route. Owner: `HeddleCo/api#1`, with Weft/Tapestry adapters. | **Blocked — API shape and Tapestry fulfillment missing** |
| Review verdicts | Weft still implements legacy `RecordVerdict` as a first-class idempotent write (`state_review.rs:298-369`). API `state_review.proto` defines `Verdict` and documents the write but its service exposes only `GetReviewPayload`, `ListSignatures`, and `SignState` (`state_review.proto:18-25,213-258`); the manifest still drops `RecordVerdict` (`migration-manifest.json:854-858`). | Add a first-class v1alpha1 `RecordVerdict` RPC and port the Weft producer. A verdict remains distinct from `SignState`. Owner: `HeddleCo/api#1`, with the Weft adapter. | **Blocked — API method and producer adapter missing** |
| Provider-token lifecycle | The API exposes only `IdentityService/StoreProviderToken` plus a token-backed import source (`identity.proto:545-554,1050-1059`; `operation.proto:83-96`); it has no retrieval, refresh, or revocation RPC. Weft encrypts/unseals access tokens (`pg_registry.rs:2107-2213`), but its shared identity adapter explicitly returns `UNIMPLEMENTED` for `StoreProviderToken` (`identity.rs:101-156`) and its import worker rejects provider-token jobs (`import_worker.rs:1747-1756`). Tapestry PR 164 still constructs `authClient()` from legacy `AuthService` and calls that client's `storeProviderToken` (`src/lib/server/api.ts:16,1952-1975`); its own source-metadata note says only storage exists (`repo-insights.ts:370-377`). | Define and implement storage, private-import consumption, expiry/refresh, and revocation as one audited lifecycle, then port the Tapestry caller to the shared service. Owner: `HeddleCo/api#1`, with Weft/Tapestry adapters. | **Blocked — lifecycle and shared caller/producer missing** |
| Current Tapestry callers | Merged PR 164 moves only the four handle calls. At exact reviewed head `66fb3bd0`, Tapestry's `package.json` still declares `@heddleco/grpc` for non-handle callers. | Complete the contract-wide caller inventory in `HeddleCo/api#1`, then port every live non-handle caller in a separately reviewed Tapestry cutover change. PR 164 resolves only `HeddleCo/tapestry#163`. | **Blocked — non-handle consumer cutover not assigned to a merged PR** |
| Signing identity and wire verification | API's unary fixture uses `principal:alice`. Heddle now derives that stable principal from the bearer token's authority subject and reuses it for unary PoP, human retries, and Push/Pull stream-opening proofs (`device_flow.rs`, `grpc_hosted/mod.rs`, `request_signing.rs`, and `sync.rs`). Weft still verifies `weft-req-sig-v1` with `x-weft-sig-*` headers and a canonical form that has no identity field (`request_signature.rs`; `server/middleware/request_signature.rs`). | Move Weft to the shared canonical/header vocabulary and add an exact cross-product conformance run. Owner: the signing workstream in `HeddleCo/api#1`; this is security-gated. | **Blocked — server wire format and cross-product verification remain** |
| Complete producer and consumer surface | Weft `939f8b40` registers only the shared handle subset; its other shared identity methods return `UNIMPLEMENTED`, and other hosted registrations are legacy. Merged Tapestry PR 164 retains the legacy package for non-handle calls, while this Heddle branch already calls v1alpha1 routes. | Finish the Weft v1alpha1 producer and Tapestry consumer ports owned by `HeddleCo/api#1` before this Draft can enter a deployment window. | **Blocked — only the handle slice is adapted** |
| Package release | Heddle uses contract revision `e3b3e6d0` with `version = "=0.1.0"`. `@heddleco/api@0.1.1` is published, but the Rust crate is absent because the API release repository has no `CARGO_REGISTRY_TOKEN`. | Resolve `HeddleCo/api#10`, publish `heddle-api@0.1.1`, and replace Heddle's temporary git dependency without changing contract behavior. | **Blocked — API #10 / Rust publication credential** |
| Cross-product tests | Heddle has descriptor/client tests, Weft PR 592 has handler/route tests, and Tapestry PR 164 has adapter/browser/route tests. None of those runs the exact Heddle and Tapestry builds against the exact Weft build and shared signing middleware planned for production. | Add an exact-version integration matrix covering both clients, the Weft route builder, signing middleware, handles, billing, verdicts, and provider-token import. Owner: `HeddleCo/api#1` contract-fidelity gate and this PR's coordinated-cutover gate. | **Blocked — no exact-build interoperability test** |
| Deployment window | No checked-in artifact records the deployment order, traffic pause/resume criteria, smoke tests, exact versions, or whole-cutover rollback points. | Record and execute that plan only after the contract, producer, consumer, package, and cross-product gates above pass. Owner: Heddle PR 1021 coordinated release. | **Blocked — window plan and executable matched set missing** |

## Coordinated-cutover checklist

No Heddle, Weft, Tapestry, or API PR may claim that the cutover is complete
until every box below is backed by its owning PR and test evidence.

### Correct and release the contract

- [ ] Re-audit every dropped migration-manifest RPC against current Weft,
  Tapestry `origin/main`, and Tapestry `origin/feat/app-site-overhaul` callers.
- [x] Restore the four live handle RPCs without removing handle functionality
  (`HeddleCo/api#9`, merged as `e3b3e6d0`).
- [x] Serve those four shared handle RPCs from Weft without replacing the
  legacy route before the coordinated window (`HeddleCo/weft#592`, merged as
  `939f8b40`).
- [x] Merge the exact-head-reviewed matching Tapestry handle adapter
  (`HeddleCo/tapestry#164`, reviewed head `66fb3bd0`, feature-branch merge
  `51c2c3e5`).
- [ ] Retain `RecordSubscription` until the billing reconciliation caller and
  replacement, if any, are live.
- [ ] Add first-class `RecordVerdict` to v1alpha1 and port its Weft producer.
- [ ] Specify and test the complete provider-token lifecycle used by OAuth and
  private imports.
- [ ] Pin PoP and human signing-identity semantics with conformance fixtures.
- [x] Publish `@heddleco/api@0.1.1` for the Tapestry adapter.
- [ ] Resolve `HeddleCo/api#10`'s missing `CARGO_REGISTRY_TOKEN` and publish the
  Rust `heddle-api@0.1.1` crate.

### Prepare every producer and consumer

- [x] Weft serves the four corrected shared handle methods and includes them in
  registration, reflection, health, policy, capability, and handler tests.
- [ ] Weft serves the remaining shipped v1alpha1 surface and moves request
  signing from the legacy `weft-req-sig-v1`/`x-weft-sig-*` vocabulary to the
  final shared contract.
- [ ] Tapestry merges PR 164's shared handle adapter, then replaces every
  remaining legacy generated import and ports its API module,
  review/event/feed routes, OAuth/provider-token flows, Polar reconciliation,
  and live handle flows.
- [x] Heddle re-pins API revision `e3b3e6d0` and reruns its hosted adapter,
  attests capability snapshot `726958dd`, and reruns its hosted adapter,
  signing, session, CLI, daemon, schema, and supply-chain gates on this Draft.
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
- [ ] After the coordinated prerequisites pass, obtain a final exact-head Codex
  review, then remove the Draft/do-not-merge hold. Review of an intermediate
  Draft amendment does not clear this gate.

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
checksums to one GitHub release. The TypeScript 0.1.1 package is published; the
Rust 0.1.1 crate remains blocked as described above.

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
