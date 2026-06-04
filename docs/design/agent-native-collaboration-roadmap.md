# Agent-native collaboration implementation roadmap

**Status:** Planned implementation roadmap  
**Scope:** Heddle OSS CLI and local repository model. Weft and Tapestry are named only where the Heddle contract crosses repository boundaries.  
**Source decisions:** ADRs [0001](../adr/0001-repository-scoped-discussions.md)-[0038](../adr/0038-collaboration-visibility-convergence.md), especially [0016](../adr/0016-local-discussion-log-first.md), [0017](../adr/0017-evolve-discuss-in-place.md), [0032](../adr/0032-weft-backed-collaboration-sync.md), [0035](../adr/0035-collaboration-store-layout.md), [0036](../adr/0036-versioned-collaboration-operation-encoding.md), and [0037](../adr/0037-semantic-anchor-convergence.md).

## Current baseline

The current `heddle discuss` implementation is state-attached:

- CLI verbs are `open`, `append`, `resolve`, `list`, and `show`.
- Discussion data is stored in `DiscussionsBlob` on source states.
- IDs are string discussion IDs, not UUIDv7 discussion IDs.
- Turns are ordered in a mutable discussion blob, not operation-based causal history.
- Anchor travel exists for symbol anchors, but durable anchors are still tied to the old state-attached shape.
- Command catalog, schema registry, JSON docs, error envelopes, and op-id plumbing already exist and should be reused.

The roadmap below replaces the state-attached model with a repository collaboration log before 1.0. Heddle should not add compatibility shims for the old model unless explicitly requested.

## Post-rebase alignment notes

The branch was rebased onto `origin/main` after the collaboration ADRs were written. The incoming commits line up with the roadmap and provide a few implementation seams to reuse:

- `CommandRuntimeContract` is now slimmer, and mutating command metadata is concentrated through existing command catalog contracts such as `MUTATING`. Collaboration commands should use that existing contract path instead of adding a separate command metadata mechanism.
- `Cli::open_repo()` is now the common repository-open helper. New collaboration commands should use it rather than repeating cwd/`--repo` resolution.
- `objects::object::versioned_blob` now centralizes versioned MessagePack boilerplate for state-attached container blobs, including the legacy `DiscussionsBlob`. This is useful context for migration and validation, but it is not enough for durable collaboration operations because collaboration needs explicit kind dispatch, historical schema snapshots, and content-addressed envelope identity.
- Automatic ed25519 state signing is now shipped for authored source states. Collaboration operation signing should reuse the same identity direction and crypto seams where possible, but it remains staged separately: unsigned v1 collaboration operations are locally valid, and later attestations must not rewrite old operation bytes.
- Hosted session setup now has a deeper single session-open entry. Future collaboration sync should plug into that session boundary rather than creating a parallel hosted-session lifecycle.
- Oplog undo/redo semantics were consolidated per variant. Collaboration operations should stay out of source-history undo/redo and keep using compensating collaboration operations for corrections.
- Recent CLI cleanup removed dead commands and deprecated surface area. That reinforces the pre-1.0 decision to evolve `discuss` in place and remove misleading state-attached flags instead of preserving compatibility shims.

## Heddle Project alignment notes

The Heddle GitHub Project is already a large cross-repo collaboration surface: 440 items across Heddle, Weft, and Tapestry, with Status, Priority, Size, Epic, Scope, DoD Type, parent/sub-issue progress, linked pull requests, and reviewers. That project shape validates the roadmap's attention and provenance direction, but it also shows where the first version was too narrow.

Strong alignment:

<!-- doctor-docs:planned -->
- The Project's Ready, In progress, In review, and Done states are practical input to `heddle inbox`: they represent attention, readiness, review state, and recommended next work.
- Whole-CLI consolidation work aligns with the command-contract foundation; collaboration should reuse the existing command catalog and op-id machinery rather than invent a new agent contract layer.
- `VisibilityTier` / `StateVisibility` work from #315-#319 and #523 is the shared visibility substrate for collaboration; discussion visibility should consume it instead of growing a parallel model.
- StateVisibility, self-sovereign auth, signing, Weft hardening, and Tapestry review work align with the planned capability, policy, attestation, and hosted projection layers.
- Semantic merge correctness and anchor-travel work are prerequisites for trustworthy semantic anchors.

Roadmap adjustments from the Project review:

- Add minimal external artifact references in the first operation-model slice. Heddle does not need default GitHub mirroring, but it does need durable references to external issues, pull requests, review comments, and project items when those artifacts explain imported or linked collaboration history.
- Make `collaboration.import_root` a required early operation kind, not only a migration contingency. Legacy state-attached discussions and later explicit GitHub/Tapestry imports both need a first-class source and trust boundary.
- Split inbox from full semantic anchor convergence. A minimal inbox should work for targeted blockers, unanswered questions, follow-up tasks, review states, and unanchored operational work before every semantic anchor case is solved.
- Model review lifecycle as machine-actionable attention. Requested reviewers, changes requested, re-review requested, unresolved review threads, linked PRs, and CI gates should not collapse into discussion prose.
- Keep full project-board parity deferred. Labels, assignees, priority, size, epic, scope, DoD type, and project status can enter as projection/import metadata before Heddle decides whether they deserve native collaboration operations.
- Add a later explicit GitHub Project/Issue/PR-review import and cutover milestone for dogfooding. This is not default Git bridge import/export; it is an intentional migration path with import roots, source references, and policy labels.
- Treat project dependencies and rollups as a separate planning graph from collaboration operation causality. Parent/sub-issue progress and "ready after parent lands" should inform inbox/readiness, but they are not causal parents in the CRDT operation graph.

## Milestones

### M0: Command contract foundation

Goal: make the CLI surface ready for agent-native collaboration before changing the storage model.

- Add collaboration schema identifiers and versions to every `discuss` and `inbox` JSON response.
- Extend command catalog metadata for collaboration writes:
  - reuse the existing `MUTATING` contract path
  - `mutates: true`
  - `supports_op_id: true`
  - `op_id_behavior: "explicit_replay"`
  - collaboration side-effect class
- Make collaboration JSON errors carry:
  - stable machine-readable `code`
  - human `message`
  - `schema` and `schema_version`
  - recovery fields specific to the error
- Define error codes before implementation:
  - `collab_stale_heads`
  - `collab_ambiguous_id`
  - `collab_unknown_parent`
  - `collab_invalid_artifact`
  - `collab_idempotency_conflict`
  - `collab_hosted_rejected`
  - `collab_capability_expired`
  - `collab_capability_interrupted`
  - `collab_anchor_ambiguous`
  - `collab_anchor_orphaned`
- Update `docs/json-schemas.md` only after concrete schemas are registered in `crates/cli/src/cli/commands/schemas.rs`.

Exit criteria:

- `heddle commands --output json` exposes side-effect and op-id metadata for discussion writes.
- `heddle doctor schemas` can validate the new collaboration JSON samples.
- Existing state-attached `discuss` commands still work until M4 migration replaces them.

### M1: Local collaboration operation model

Goal: introduce the durable Heddle-native operation model without hosted sync.

- Add typed in-memory collaboration model:
  - `Discussion`
  - `ContextAnnotation`
  - `CollaborationOperation`
  - `CollaborationEnvelope`
  - `CollaborationOperationKind`
  - `OperationCapabilityContext`
  - `TaskProvenance`
  - `SemanticAnchor`
  - `AttentionTarget`
  - `ExternalArtifactRef`
- Use canonical MessagePack envelopes with explicit schema version and operation kind.
- Reuse the packed oplog versioning pattern:
  - latest encoder
  - explicit version dispatch
  - frozen decode-only historical schema modules
  - no unversioned decode path
- Borrow validation discipline from `versioned_msgpack_blob!`, but implement a dedicated collaboration operation codec rather than storing repository operations as state-attached container blobs.
- Compute `CollabOpId` from exact canonical envelope bytes.
- Include the collaboration idempotency key in the envelope.
- Use UUIDv7 for `DiscussionId`.
- Include minimal external artifact references when a collaboration operation is imported from or linked to a source outside the local Heddle repository.
- Reserve operation kinds for later redaction, attestation, and visibility even if the first slice implements only a subset.

First implemented operation kinds:

- `discussion.open`
- `discussion.turn`
- `discussion.resolve`
- `discussion.reopen`
- `discussion.retarget`
- `collaboration.resolve_conflict`
- `collaboration.import_root`

Minimum `ExternalArtifactRef` fields:

- provider, such as `github`
- repository or namespace
- artifact kind, such as issue, pull request, review, review comment, project item, or discussion
- external id
- URL
- parent artifact when relevant
- imported or linked time
- source trust label

Deferred operation kinds:

- `discussion.visibility`
- `collaboration.redaction`
- `collaboration.attestation`
- `hosted_valid_continuation`
- synced task delegation records

Exit criteria:

- Golden canonical byte and hash fixtures exist for representative operation kinds.
- Same semantic operation encoded as different schema versions produces different `CollabOpId`s.
- Same idempotency key with changed request content rejects locally.
- Missing token material does not invalidate an operation that has capability id and scope summary.
- Imported or externally linked operations retain source metadata without trusting external authorship for Heddle capability policy.

### M2: Local collaboration store

Goal: store collaboration operations as a repository-local durable log.

- Create `.heddle/collaboration/` with:
  - `ops/` for prefix-sharded immutable operation files
  - `indexes/` for rebuildable indexes
  - `views/` for rebuildable materialized views
  - `sync/` for Weft sync metadata later
  - `tmp/` for temp writes
- Write operation files with temp file, atomic rename, and directory sync.
- Treat the operation file as the durable commit point.
- Keep indexes and views derived and rebuildable.
- Use a repository collaboration lock around:
  - idempotency checks
  - operation file creation
  - minimal idempotency/sync metadata updates
- Keep pack/segment compaction out of v1.

Required indexes:

- op id to operation path
- record id to operation ids
- record id to current head set
- idempotency key scope to op id
- source anchor to record ids
- attention target to record ids or attention items

`fsck` minimum checks:

- recompute operation IDs from canonical bytes
- decode supported schema versions
- verify primary record references
- verify parent references
- distinguish unresolved parents from malformed bytes
- verify idempotency index consistency
- prove indexes/views are rebuildable

`doctor` allowed repairs:

- rebuild indexes and views
- recreate missing idempotency metadata from durable operations when unambiguous
- quarantine malformed artifacts

`doctor` forbidden repairs:

- invent replacement operation bytes
- rewrite content-addressed envelopes
- silently delete locally valid operations

Exit criteria:

- Crash after operation write but before index update is recoverable.
- Duplicate canonical operation bytes return idempotent success.
- Hash collision or same `CollabOpId` with different bytes fails closed.
- `fsck` and `doctor` do not require Weft access.

### M3a: Minimal materialization and inbox

Goal: derive useful local collaboration views and attention before full semantic anchor convergence.

- Materialize discussion records by causal graph, not timestamp order.
- Preserve current head sets internally.
- Render deterministic display order for humans and JSON convenience.
- Surface concurrent append turns as siblings, not conflicts.
- Surface incompatible lifecycle claims as conflicts.
<!-- doctor-docs:planned -->
- Implement `heddle inbox` as a derived attention view.
- Do not claim read/unread state in first-slice inbox JSON.
- Include sync/local-only diagnostics in inbox only when they affect actionability, readiness, or sync.
- Include review lifecycle and project-state projection inputs where available:
  - requested reviewers
  - changes requested
  - re-review requested
  - unresolved review threads
  - CI gate state
  - linked pull requests
  - parent/sub-issue or dependency readiness
  - imported project status, priority, epic, scope, and DoD metadata as non-authoritative projection data
- Keep planning dependencies separate from collaboration causality. A planning dependency can block readiness or route attention, but it is not a causal parent in the collaboration operation graph.

First-slice attention sources:

- blocker turns targeted at current actor/thread
- targeted unanswered questions
- resolution conflicts that affect current work
- follow-up tasks imported from or linked to review comments
- requested review or re-review on linked source work
- unresolved review threads linked to current work
- CI gate failures linked to current work
- unresolved operations that affect current work or sync
- hosted rejection diagnostics once sync exists

Exit criteria:

- Same accepted operation set materializes to the same view regardless of arrival order.
- Deterministic display order does not hide conflicts, ambiguity, redactions, or local-only divergence.
- `ready` blocks only on targeted or high-severity attention, not every open discussion.
- Review and project projection metadata can inform inbox/readiness without becoming native project-board operations.

### M3b: Semantic anchor convergence

Goal: derive useful local collaboration views from the operation log.

- Surface incompatible lifecycle, visibility, and single-anchor claims as conflicts.
- Keep conflict resolution as an explicit operation that cites conflicting operations.
- Implement semantic anchors with:
  - source state/version where the reference was created
  - semantic selector
  - derived current resolution
  - deterministic confidence/status
- Anchor statuses:
  - `current`
  - `moved`
  - `changed`
  - `ambiguous`
  - `orphaned`
- Do not make anchor resolver thresholds per-user in v1.
- Do not depend on hosted services or live language-server state for OSS anchor resolution.
- Treat #492, the existing anchor-travel decision, as a prerequisite. Heddle should wire useful anchor-travel fields into semantic anchor status or explicitly retire them before migrating state-attached discussions.
- Add ambiguous or orphaned anchor attention when anchor status affects actionability.

Exit criteria:

- Anchor status is deterministic for a repository version.
- Existing state-attached anchor-travel fields have an explicit migration or retirement rule.
- Ambiguous or orphaned anchors feed inbox/readiness only when they affect current work or policy gates.

### M4: CLI replacement and migration

Goal: evolve `heddle discuss` in place from state-attached discussions to repository collaboration records.

Canonical commands:

- `heddle discuss open`
<!-- doctor-docs:planned -->
- `heddle discuss turn`
- `heddle discuss resolve`
<!-- doctor-docs:planned -->
- `heddle discuss reopen`
<!-- doctor-docs:planned -->
- `heddle discuss retarget`
<!-- doctor-docs:planned -->
- `heddle discuss resolve-conflict`
- `heddle discuss list`
- `heddle discuss show`
<!-- doctor-docs:planned -->
- `heddle inbox`

Aliases:

<!-- doctor-docs:planned -->
- `heddle discuss comment` may exist as a human alias for `turn --kind comment`.
- The current `append` verb should be replaced or kept only as a short-lived pre-1.0 alias if implementation sequencing needs it.

First-slice command behavior:

- JSON and agent workflows pass explicit observed heads.
- Human text mode may default observed heads to current heads for append-like turns.
- Stale explicit heads fail by default and return current heads.
- `--allow-concurrent` applies only to append-like operations.
- Resolve, visibility, and retarget use conflict semantics, not generic concurrency override.
- Human text-mode turns may default `kind` to `comment`.
- JSON and agent workflows require or strongly encourage explicit turn kind.
- `show --json` returns both deterministic display order and graph facts.
- `show` human output visibly marks conflicts, ambiguous anchors, redactions, hosted-valid divergence, and local-only status.

Migration behavior:

- Detect legacy state-attached discussions.
<!-- doctor-docs:planned -->
- `heddle discuss migrate` is advanced/doctor-oriented, not daily help.
- Migration is plan-first by default.
- Applying migration requires `--apply` or equivalent.
- Migration creates import roots with source metadata.
- New discussions receive UUIDv7 ids.
- Legacy ids remain source metadata and aliases.
- Migration is idempotent through source-derived collaboration idempotency keys.
- If migration stops halfway, rerunning the same plan completes missing operations and reports already-created records.
- Legacy source objects remain untouched until a separate cleanup.

Exit criteria:

- State-attached `DiscussionsBlob` is no longer the live discussion source of truth.
- Migration output labels imported history in JSON/detail output.
- `discuss list` defaults to active/open discussions scoped by current context.
- `inbox` owns attention-worthy work.

### M5: Tests and release gate

Goal: make the first local discussion slice shippable as a core pre-1.0 CLI feature.

Required test groups:

- Codec tests:
  - latest round-trip
  - historical decode snapshots
  - unsupported version rejection
  - golden canonical bytes
  - golden `CollabOpId`s
- Operation identity tests:
  - same bytes deduplicate
  - same id different bytes fails closed
  - same idempotency key same request returns existing op
  - same idempotency key different request rejects
- Store tests:
  - temp/rename/directory-sync path
  - crash after op write before index update
  - index rebuild
  - malformed artifact quarantine
  - unresolved parent retention
- Materialization/property tests:
  - concurrent append survival
  - stale observed heads failure
  - resolution conflicts
  - recursive conflict resolution conflicts
  - deterministic display order
  - anchor ambiguity/orphan attention
- CLI golden JSON tests:
  - `discuss open`
  - `discuss turn`
  - `discuss resolve`
  - `discuss reopen`
  - `discuss retarget`
  - `discuss resolve-conflict`
  - `discuss list`
  - `discuss show`
  - `inbox`
  - stale-head error
  - ambiguous-id error
  - idempotency conflict error
- Migration tests:
  - plan output
  - apply output
  - rerun idempotency
  - interrupted apply recovery
  - imported-history labels
- Readiness tests:
  - blocker turn blocks current thread
  - answer citing question clears question attention
  - unrelated open discussion does not block
  - context conflict blocks only when linked to current work or policy gate
  - requested review or unresolved review thread blocks linked work
  - parent/dependency state affects readiness without becoming a causal parent

First-slice release gate:

- Command metadata is updated.
- Concrete schemas are registered.
- JSON samples are documented and pass `heddle doctor schemas`.
- Materialization/property tests pass.
- Basic collaboration `fsck`/`doctor` checks pass.
- Migration plan/apply tests exist if legacy data exists.
- User docs label local-only, Weft-backed, and planned capabilities.
- At least one end-to-end JSON-first agent workflow is documented.
- At least one fixture covers review comment to follow-up task to inbox item to resolution.

### M6: Weft sync preparation

Goal: prepare local collaboration for hosted sync without shipping live collaboration early.

- Build the formal convergence model before remote CRDT sync ships.
- Model:
  - concurrent append survival
  - resolution conflicts
  - recursive conflict materialization
  - command idempotency versus operation identity
  - hosted acceptance/rejection layer
  - blocked descendants
  - hosted-valid continuations
  - signatures and capabilities as predicates
- Add sync metadata states:
  - pending
  - accepted
  - rejected
  - blocked by hosted-invalid causal history
  - cursor-synced
- Keep hosted rejection reasons as sync metadata with stable codes and refreshable messages.
- Source push auto-includes only linked collaboration records.
- Auto-sync sends the required hosted-valid causal closure.
- Source push success is not rolled back if collaboration sync fails.
- Retry resumes collaboration lane from sync metadata and remote cursors.
- Add redaction and policy-suppression story before broad hosted collaboration rollout.
- Prepare hosted review collaboration before Tapestry Review MVP depends on it. Threaded discussion, approval/block state, review signatures, and policy-filtered visibility should share the same collaboration sync and rejection vocabulary.

Exit criteria:

- Local-only and hosted-synced views are distinct.
- Hosted-rejected local operations remain retained locally.
- Descendants of hosted-rejected operations are blocked from sync until accepted, bypassed, or replaced.
- Linked collaboration auto-sync does not send unrelated repository discussions.

### M7: Live sync and hosted projection

Goal: add live collaboration only after durable sync is correct.

- Add explicit foreground watch modes for inbox or discussion records.
- Use gRPC-backed live sync to deliver durable operations and sync state.
- Do not add invisible background daemon sync in the first live slice.
- Do not store presence, typing indicators, or ephemeral hosted UI state as collaboration operations.
- Let Tapestry consume hosted-safe aliases, policy-filtered projection metadata, and Weft rejection codes from Weft contracts, not Heddle local internals.

Exit criteria:

- Watch mode is a transport over the same operation log.
- Push/pull catch-up and live updates converge through the same materializer.
- Live sync can be disabled without losing correctness.

### M8: Explicit GitHub import and dogfood cutover

Goal: support deliberate migration from GitHub-hosted planning/review artifacts into Heddle-hosted collaboration without turning Git bridge into an automatic comment mirror.

- Add an explicit import workflow for selected GitHub issues, pull requests, review comments, and project items.
- Represent imported artifacts with `ExternalArtifactRef` and `collaboration.import_root`.
- Preserve source URL, source id, author metadata, imported time, parent artifact, and trust/source labels.
- Keep the import actor distinct from original external authors.
- Map GitHub Project fields to projection metadata first:
  - status
  - priority
  - size
  - epic
  - scope
  - DoD type
  - parent/sub-issue progress
  - linked pull requests
  - reviewers
- Let imported project metadata inform inbox, readiness, filters, and migration reports without making full project-board parity a first-slice Heddle primitive.
- Support a dogfood cutover report that shows which GitHub artifacts are imported, linked, stale, local-only, or still GitHub-authoritative.
- Keep default GitHub/GitLab/Git notes import/export disabled.

Exit criteria:

- A selected GitHub review comment can become a Heddle follow-up discussion or attention item with source provenance.
- Imported Project items retain enough metadata for agents to understand status, review state, and dependencies.
- Dogfood cutover can run as an explicit command or report without silently ingesting unrelated GitHub content.

## Command schema sketches

These are design sketches, not registered schemas. Concrete schemas belong in `crates/cli/src/cli/commands/schemas.rs` and documented samples belong in `docs/json-schemas.md`.

### Common success envelope

```json
{
  "schema": "heddle.collaboration.discuss.show",
  "schema_version": 1,
  "output_kind": "discuss_show",
  "view_mode": "capability_filtered_local",
  "local_only": true,
  "warnings": []
}
```

Rules:

- `schema` and `schema_version` are required.
- `output_kind` remains the command-shaped discriminator used by existing Heddle JSON.
- `view_mode` is required for reads.
- `local_only` is required when the user might assume Git or hosted sync shared the record.
- Empty arrays serialize as `[]`.
- Semantically permanent unset fields serialize as `null`.

### Common error envelope

```json
{
  "schema": "heddle.collaboration.error",
  "schema_version": 1,
  "output_kind": "error",
  "code": "collab_stale_heads",
  "message": "observed heads are stale for discussion 018f...",
  "recovery": {
    "current_heads": ["co_abc..."],
    "retry": "rerun with the current heads or use --allow-concurrent for a turn"
  }
}
```

Rules:

- `code` is stable and machine-readable.
- `message` is human-readable and may change.
- `recovery` shape depends on `code`.
- Stale-head errors include `current_heads`.
- Ambiguous-id errors include `candidates`.
- Hosted rejection errors include `hosted_rejection_code`, `remote`, and continuation hints when available.

### `heddle discuss open --output json`

Required fields:

- `schema`
- `schema_version`
- `output_kind: "discuss_open"`
- `discussion_id`
- `operation_id`
- `idempotency_key`
- `title`
- `anchor`
- `turn`
- `heads`
- `local_only`
- `sync`
- `warnings`

<!-- doctor-docs:planned -->
### `heddle discuss turn --output json`

Required fields:

- `schema`
- `schema_version`
- `output_kind: "discuss_turn"`
- `discussion_id`
- `operation_id`
- `idempotency_key`
- `turn`
- `observed_heads`
- `heads`
- `concurrent`
- `attention_delta`
- `local_only`
- `sync`
- `warnings`

### `heddle discuss show --output json`

Required fields:

- `schema`
- `schema_version`
- `output_kind: "discuss_show"`
- `discussion`
- `display_order`
- `operations`
- `heads`
- `conflicts`
- `anchor_status`
- `attention`
- `view_mode`
- `sync`
- `warnings`

The `display_order` is convenience output. Agents should use `operations`, `heads`, and `conflicts` for correctness.

<!-- doctor-docs:planned -->
### `heddle inbox --output json`

Required fields:

- `schema`
- `schema_version`
- `output_kind: "inbox"`
- `view_mode`
- `items`
- `groups`
- `sync_diagnostics`
- `warnings`

Each item includes:

- `item_id`
- `kind`
- `severity`
- `target`
- `record_id`
- `operation_ids`
- `reason_code`
- `blocks_ready`
- `task_provenance`
- `sync`
- `recommended_actions`
- `projection`

No `read` or `unread` field appears until explicit read-state metadata exists.

Projection metadata may include imported or hosted project/review fields such as status, priority, epic, scope, DoD type, linked pull requests, requested reviewers, and parent/sub-issue progress. Projection fields are query and attention inputs, not collaboration operation causality.

## User-facing docs tasks

- Add a local discussion quickstart with JSON-first examples.
- Add a human-oriented short workflow after the JSON examples.
- Add a migration guide for state-attached discussions.
- Add an explicit GitHub import/cutover guide once M8 starts.
- Add a troubleshooting guide for:
  - stale heads
  - local-only collaboration
  - Git push not sharing discussions
  - unresolved parents
  - hosted rejection once sync exists
- Add `fsck`/`doctor` collaboration diagnostics docs.
- Update `docs/json-schemas.md` after concrete schema registration.
- Update release notes to classify each capability:
  - shipped local behavior
  - foundation in place
  - planned Weft/Tapestry behavior

## Non-goals for the first local slice

- Hosted sync.
- Live watch mode.
- Presence or typing indicators.
- Full native project-board parity.
- Full context extraction.
- Read/unread state.
- Snooze.
- Local encrypted collaboration storage.
- Pack/segment compaction.
- Default GitHub/GitLab/Git notes import or export.
