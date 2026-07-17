# gRPC consumer inventory

Status: pre-extraction snapshot verified against Heddle and Weft `origin/main`
on 2026-07-13. Weft handler and registration evidence was checked at
`eb1155d8c941171817d9b15ab2a73f948e0aca47`.

This is historical migration input, not current package or service guidance.
The migration manifest in `HeddleCo/api` is not authoritative for the retained
surface until its dropped-method classifications are re-audited. ADR 0048
records the known discrepancies and the coordinated-cutover checklist.

This inventory records implementation evidence for the contract Heddle builds
and publishes. It is not a compatibility promise.

## Hosted services

Weft registers and serves `AuthService`, `ContentService`, `DiscussionService`,
`FeedService`, `HookService`, `HostedUserService`, `ImportService`,
`OperationLogQueryService`, `RepoEventService`, `RepoSyncService`,
`ReviewService`, `SearchService`, `SignalService`, `StateReviewService`,
`ThreadWorkflowService`, and `TransactionService`.

The following surfaces have concrete Weft handlers and remain in the contract:

- import job creation and progress streaming;
- hosted search;
- account-recovery declaration, proof, veto, and completion;
- subscription recording;
- spool reads, visibility, settings, child edges, and monorepo resolution;
- subject and handle resolution, claims, escrow, and proof workflows;
- governance and membership history; and
- single- and multi-repository event subscriptions.

The generated Rust and TypeScript bindings must retain those services, methods,
and their reachable messages.

## Local services

Heddle's local daemon serves `DiscussionService`, `HookService`,
`OperationLogQueryService`, `SignalService`, `StateReviewService`, and
`TimelineService`. Those contracts also remain part of the canonical package.

## Removed before 0.23

`TreeEditService` is removed. The inspected Weft server has no implementation or
registration for its `StatusForThread`, `DiffForThread`, or `LogForThread`
methods, and Heddle no longer exposes the removed hosted tree-edit client seam.

## Extraction gate

Before Weft adopts the first `HeddleCo/api` release, compare the API descriptor
with Weft's registered services and generated trait implementations. Any future
removal requires a coordinated Heddle, Weft, and Tapestry change backed by the
same implementation and consumer inventory.
