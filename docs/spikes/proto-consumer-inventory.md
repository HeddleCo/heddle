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

## Remove: clear dead or duplicate evidence

The recovery declaration surface duplicated one generic command with three
per-kind commands. Cross-repository search found no handler or client use beyond
generated code and planning documents. This pre-1.0 cleanup keeps only
`DeclareRecoveryMethod`, whose typed oneof is the method kind, and removes
`DeclareHardwareKeyRecovery`, `DeclareSocialGuardians`, `DeclarePaperCode`, and
their request envelopes. The redundant `kind` field is reserved by tag and name
so contradictory enum/oneof states cannot be represented.

No other service or RPC had enough cross-repository evidence to delete safely.
Pre-1.0 status is not, by itself, evidence that an unimplemented capability has
no owner or plan.

## Uncertain: retain pending producer cutover

- Entire services: `ImportService`, `SearchService`, and `TreeEditService`.
- `RepoEventService.SubscribeRepoEventsMulti`.
- Hosted spool settings and unified visibility: `GetSpool`,
  `SetSpoolVisibility`, and `UpdateSpoolSettings`.
- Hosted subject/handle, child-spool, monorepo, governance/membership history,
  proof, and handle-escrow additions.
- Auth subscription and the remaining generic recovery flow not present in the
  inspected Weft handler implementation.
- Hosted review verdict/check/progress additions beyond the three methods in the
  inspected Weft `StateReviewService` implementation. Their local Heddle
  implementation is real; hosted coverage is pending.

These members were added by recent Heddle changes that cite concrete Weft issues,
including the import, search, multi-repo event, review-progress, proof/escrow, and
spool-settings work. They remain because the checked-out Weft `origin/main`
predates those Heddle contract changes; deleting them would guess against named
cross-repository work.

## Cutover action

Before Weft adopts the first `HeddleCo/api` release, generate a descriptor-based
service/method inventory and compare it with the services registered by Weft.
Missing hosted handlers must fail in Weft. Each uncertain member must then move
to either implemented/kept or intentionally removed in a coordinated API,
Heddle, Weft, and Tapestry change.
