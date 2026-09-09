# Alpha v2 cutover acceptance checklist

This is the live completion checklist for API, Heddle, Weft and Tapestry.
A checkbox means the complete production path and its relevant verification are
finished. A model/helper, advertised method, or compile-only result is insufficient.
No legacy adapter or migration bridge is part of the cutover.

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
- [ ] Browser own-root local authoring renews locally with verified root history/attachment; never revives hosted sessions or expands delegated authority.
- [ ] Tapestry combined hosted/device Thread pages, discussions/context/evidence, source review and checkout-targeted actions work end to end.
- [ ] All CLI verbs/JSON/help and hosted adapters use complete v2 paths.
- [ ] Actor/session presence scans become indexed SQLite lookups, separate from checkout writer leases.
- [ ] Rich typed predicates and full-text context/discussion search use bounded indexes/projections.
- [ ] All device observers share committed dispatch and appropriate local authority expiry handling.
- [ ] Remaining transcript producers honor explicit retention opt-in and immutable TTLs.
- [ ] Hosted billing worker composition and cancellation/notification/integration lifecycle are wired and tested without real external charges/messages.

## Final acceptance (root, assisted by all paths)

- [ ] Whole-repository sweeps remove/repoint stale methods, names, paths, docs, evidence maps and dependency references.
- [ ] Relevant tests, real negative controls, production build/gates and client typechecks pass; no silently skipped acceptance.
- [ ] Fresh local full v2 stack runs with real relay https://relay.preview.heddle.sh/.
- [ ] Create/capture/sync/publish/review/approve/land tested across CLI, browser and Weft, including hosted work with devices offline.
- [ ] Private device work stays local unless policy opts into sharing; delegation/onboarding/rotation/revocation behavior verified.
- [ ] OTEL traces/metrics show bounded SQL/work/queues, quiet idle streams, backpressure and released resources after cancellation/disconnect.
- [ ] Draft PRs reflect final implementation and merge order, checks are green and all remaining limitations are explicit.

No deployment, merge, real provider charge, or external message is authorized by
this checklist. Local provider fixtures exercise those workflows until any real
external acceptance requiring additional authorization is identified.
