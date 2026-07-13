# gRPC consumer inventory

Status: pre-extraction snapshot from Heddle `origin/main` at `ec3c7296`, plus
read-only local Weft and Tapestry checkouts inspected on 2026-07-12.

This inventory answers whether a contract member has implementation or consumer
evidence. It is not a compatibility promise. The sibling checkouts do not contain
every branch referenced by recent cross-repository issue links, so absence is
recorded as uncertain rather than treated as proof that a capability is dead.

## Keep: implementation or consumer evidence exists

- Heddle local daemon: `DiscussionService`, `HookService`,
  `OperationLogQueryService`, `SignalService`, `StateReviewService`,
  `TimelineService`, and `TransactionService`.
- Weft hosted handlers: `AuthService`, `ContentService`, `DiscussionService`,
  `FeedService`, `HookService`, `HostedUserService`,
  `OperationLogQueryService`, `RepoEventService`, `ReviewService`,
  `SignalService`, `StateReviewService`, `RepoSyncService`,
  `ThreadWorkflowService`, and `TransactionService`.
- Tapestry imports or invokes Hosted, Content, ThreadWorkflow, Feed, RepoEvent,
  Review, StateReview, and Discussion surfaces.
- Heddle hosted clients exercise Auth, Content, Hosted, RepoSync, and TreeEdit
  generated clients; TreeEdit currently has client/mock evidence but no handler
  in the inspected Weft checkout.

These services and the shared messages reachable from them stay in the extracted
contract.

## Removed before 0.23

The pre-0.23 contract removes surfaces with no production handler or consumer:
`ImportService`, `SearchService`, `TreeEditService`, multi-repository event
subscription, hosted spool settings, subject and handle resolvers, child-edge
mutation/listing, governance and membership history, proof and handle escrow,
billing subscription recording, and account recovery. Their exclusive messages
are removed with them. Generated Rust and TypeScript APIs therefore cannot imply
capabilities the product does not provide.

`ResolveMonorepo`, single-repository `SubscribeRepoEvents`, the implemented
StateReview surface, and shared identity/authentication fields remain.

## Cutover action

Before Weft adopts the first `HeddleCo/api` release, generate a descriptor-based
service/method inventory and compare it with the services registered by Weft.
Missing hosted handlers must fail in Weft. Each uncertain member must then move
to either implemented/kept or intentionally removed in a coordinated API,
Heddle, Weft, and Tapestry change.
