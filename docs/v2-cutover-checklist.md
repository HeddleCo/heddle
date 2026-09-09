# Alpha v2 cutover acceptance checklist

This is the live completion checklist for API, Heddle, Weft and Tapestry.
A checkbox means the complete production path and its relevant verification are
finished. A model/helper, advertised method, or compile-only result is insufficient.
No legacy adapter or migration bridge is part of the cutover.

## Policy invariants — clarified 2026-09-09

Privacy describes audience, independently of storage destination. Sync policy
(device-only or synced to Weft), audience (owner, explicitly authorized
people/agents, or Spool audience), and material retention are independent.
Uploading and ongoing sync never broaden audience. Sharing and public disclosure
are distinct explicit actions. Authorized users can read hosted private Threads
while every device is offline.

- [ ] Signed policy represents sync, audience and retention independently, with owner-only audience as the safe default.
- [ ] Local-key Threads work before enrollment; attaching one to an account requires an explicit signed claim and never happens as a side effect of upload.
- [ ] Original genesis owner, creator authority and admission receipts remain bound across devices/Weft; historical owned-device reads do not refresh expired original credentials.
- [ ] One Thread audience decision protects metadata, source, collaboration, evidence, search, activity, transfer and active streams; Spool membership alone cannot reveal private Threads.
- [ ] Audience changes require delegated policy-management authority, preserve capability attenuation and invalidate current observations.
- [ ] Hosted private offline access succeeds for owner/authorized parties and fails for an unrelated Spool member, including names/counts/events and object fetches.
- [ ] Sync-only and retention-only changes cannot broaden audience; concurrent policy edits cannot accidentally union recipient grants.

## Current verification checkpoint — 2026-09-09

- Hosted audience SQL: owner-only default, exact agent invites, Spool membership
  denial, explicit Spool sharing and conflict intersection passed against local
  PostgreSQL. Removing the restrictive-conflict check failed the exact assertion;
  restoring it passed. This verifies the predicate, not every RPC consumer.
- Hosted Thread mutations, review, observation, replication and list/collaboration
  query paths now apply the shared audience ceiling. Full endpoint regression and
  per-frame SQL consolidation remain pending.
- Device real-Iroh fixture passed source audience denial, collaboration, operation
  recovery, Evidence3, integrated Fetch, two 40-view cleanup rounds and quiet idle
  replication. The subsequent client+semantic fixture also passed real analysis
  completion, exact retry and queued cancellation with worker acknowledgement.
- Evidence must bind its original Thread, not merely a source State that could
  also occur in another audience. Canonical evidence v2, Rust/browser byte vectors and device origin gates pass;
  hosted origin gating passed 13 PostgreSQL tests. Removing the origin SQL
  filter failed the cross-Thread budget assertion; restored test passed. Acknowledgement
  continues to bind the exact original evidence digest.
- Hosted source closure regression passed; omitting incoming-closure validation
  admitted private globally retained bytes and failed the assertion, then restoration
  passed. Possession is distinct from a signed claim that an object exists.
- Tapestry API candidate is aligned to 2fba1eb2; typecheck reports zero errors and
  warnings, with 14 focused Thread tests passing. The additional origin-Thread presentation
  regression failed before the reducer guard and all 11 reducer tests passed after.
  Full browser acceptance is pending.
- Raw State reads now return only immutable summaries. Source attachments are
  removed: risk/conflict/semantic sidecars can carry authored attribution or
  context. Typed semantic queries remain in Analysis; authored data needs its
  originating Thread or checkout access. Descriptor absence test failed before
  removal, then six content/descriptor tests passed; current Heddle semantic
  client and Tapestry typechecks pass against this schema.
- User confirmed: local keys remain owners until explicit signed claim. Claim RPC,
  durable ownership transition and client flow are not implemented yet.

## Completed foundation

- [x] Shared signed Thread/control/evidence models and original-author receipts.
- [x] Device account/Thread/checkout/Run RPC foundation and one writer per checkout.
- [x] Shared SQLite Thread/Run metadata, indexed command receipts and bounded change history.
- [x] Bounded device artifact streaming and daemon-owned expiration.
- [x] Shared Run/Checkout invalidation and CPU-only idle capability checks.
- [x] Explicit signed viewed/named/pinned target bindings with Rust/browser vectors.
- [x] Rebuildable reference-map and typed-property equality indexes.

## Path A — production source references (reference_projection)

- [ ] Typed signed capture payload binds source State and scope/frontier-specific reference descriptor.
- [ ] Canonical descriptor/map/core/resolution objects persisted in the existing object store.
- [ ] Actual anchor/tag interning and capture update of affected targets without rewriting referrers.
- [ ] Exact-base fork root sharing; named and pinned references retain explicit scope.
- [ ] Admission validates closure and atomically publishes projected root/generation; recovery is idempotent.
- [ ] Fetch/publish closure includes reference objects; missing/corrupt/scope-mismatched closure is rejected.
- [ ] Production capture/fork/replication tests and independent omission controls pass.

## Path B — complete device RPCs (device_rpc_completion)

- [ ] ReadContent: blob/tree/state/diff/provenance, exact revision, bounded streaming.
- [ ] Collaboration: ObserveCollaboration, OpenDiscussion, AppendTurn, ResolveDiscussion, ReopenDiscussion, PutContext.
- [ ] Analysis: ObserveAnalysis and StartAnalysis.
- [ ] Search: bounded visibility-aware source/context/discussion/tag queries.
- [ ] Operations: ObserveOperations and CancelOperation with durable receipts/cancellation.
- [ ] Sync: Fetch and PublishContent with owned-device trust, closure validation and backpressure.
- [ ] Evidence: RecordEvidence, VerifyEvidence and AcknowledgeCheck; acknowledgement never grants passing status.
- [ ] Descriptor-driven device completeness gate passes; every handler has behavioral acceptance coverage.

## Path C — complete hosted RPCs (sqlite_dispatch)

- [ ] Attention: ObserveAttention, RecordInteraction, SetAttentionState.
- [ ] Notifications: ObserveNotifications, MarkNotificationsRead, SetNotificationPreferences, UnsubscribeNotifications.
- [ ] Integration: provider begin/complete/store/revoke, ObserveIntegrations, SetRemoteLink, ImportSource, SynchronizeRemote.
- [ ] StartAnalysis, ReadArtifact, Search and ReadProviderExtent.
- [ ] Activity/integration workers use committed changes; no observer-specific SQL polling.
- [ ] All schema inventories/fixtures/counts agree with the fresh migration set.
- [ ] Descriptor-driven hosted completeness gate and real PostgreSQL behavioral tests pass.

## Integration, clients and local query completion (root)

- [ ] Align API/Heddle/Weft Cargo pins and Tapestry vendor SDK; remove stale dependency sources.
- [x] Browser own-root local authoring renews locally with verified root history/attachment; never revives hosted sessions or expands delegated authority.
- [ ] Tapestry combined hosted/device Thread pages, discussions/context/evidence, source review and checkout-targeted actions work end to end.
- [ ] All CLI verbs/JSON/help and hosted adapters use complete v2 paths.
- [x] Actor/session presence scans become indexed SQLite lookups, separate from checkout writer leases.
- [ ] Rich typed predicates and full-text context/discussion search use bounded indexes/projections.
- [ ] All device observers share committed dispatch and appropriate local authority expiry handling.
- [ ] Remaining transcript producers honor explicit retention opt-in and immutable TTLs.
- [ ] Hosted billing worker composition and cancellation/notification/integration lifecycle are wired and tested without real external charges/messages.

## Final acceptance (root, assisted by all paths)

- [ ] Whole-repository sweeps remove/repoint stale methods, names, paths, docs, evidence maps and dependency references.
- [ ] Relevant tests, real negative controls, production build/gates and client typechecks pass; no silently skipped acceptance.
- [ ] Fresh local full v2 stack runs with real relay https://relay.preview.heddle.sh/.
- [ ] Create/capture/sync/publish/review/approve/land tested across CLI, browser and Weft, including hosted work with devices offline.
- [ ] Device-only state remains local; hosted private state retains its explicit audience; delegation/onboarding/rotation/revocation behavior verified.
- [ ] OTEL traces/metrics show bounded SQL/work/queues, quiet idle streams, backpressure and released resources after cancellation/disconnect.
- [ ] Draft PRs reflect final implementation and merge order, checks are green and all remaining limitations are explicit.

No deployment, merge, real provider charge, or external message is authorized by
this checklist. Local provider fixtures exercise those workflows until any real
external acceptance requiring additional authorization is identified.
