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
- Reserve operation kinds for later redaction, attestation, visibility, and import roots even if the first slice implements only a subset.

First implemented operation kinds:

- `discussion.open`
- `discussion.turn`
- `discussion.resolve`
- `discussion.reopen`
- `discussion.retarget`
- `collaboration.resolve_conflict`
- `collaboration.import_root` if migration needs it

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

### M3: Materialization, anchors, and inbox

Goal: derive useful local collaboration views from the operation log.

- Materialize records by causal graph, not timestamp order.
- Preserve current head sets internally.
- Render deterministic display order for humans and JSON convenience.
- Surface concurrent append turns as siblings, not conflicts.
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
- Implement `heddle inbox` as a derived attention view.
- Do not claim read/unread state in first-slice inbox JSON.
- Include sync/local-only diagnostics in inbox only when they affect actionability, readiness, or sync.

First-slice attention sources:

- blocker turns targeted at current actor/thread
- targeted unanswered questions
- resolution conflicts that affect current work
- ambiguous or orphaned anchors that affect actionability
- unresolved operations that affect current work or sync
- hosted rejection diagnostics once sync exists

Exit criteria:

- Same accepted operation set materializes to the same view regardless of arrival order.
- Deterministic display order does not hide conflicts, ambiguity, redactions, or local-only divergence.
- `ready` blocks only on targeted or high-severity attention, not every open discussion.

### M4: CLI replacement and migration

Goal: evolve `heddle discuss` in place from state-attached discussions to repository collaboration records.

Canonical commands:

- `heddle discuss open`
- `heddle discuss turn`
- `heddle discuss resolve`
- `heddle discuss reopen`
- `heddle discuss retarget`
- `heddle discuss resolve-conflict`
- `heddle discuss list`
- `heddle discuss show`
- `heddle inbox`

Aliases:

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

First-slice release gate:

- Command metadata is updated.
- Concrete schemas are registered.
- JSON samples are documented and pass `heddle doctor schemas`.
- Materialization/property tests pass.
- Basic collaboration `fsck`/`doctor` checks pass.
- Migration plan/apply tests exist if legacy data exists.
- User docs label local-only, Weft-backed, and planned capabilities.
- At least one end-to-end JSON-first agent workflow is documented.

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

No `read` or `unread` field appears until explicit read-state metadata exists.

## User-facing docs tasks

- Add a local discussion quickstart with JSON-first examples.
- Add a human-oriented short workflow after the JSON examples.
- Add a migration guide for state-attached discussions.
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
- General issue tracker labels, assignees, milestones, or project boards.
- Full context extraction.
- Read/unread state.
- Snooze.
- Local encrypted collaboration storage.
- Pack/segment compaction.
- General import roots except what migration needs.
- GitHub/GitLab/Git notes import or export.
