# Local discussion log first

The first collaboration implementation slice is a local repository-scoped discussion log plus derived inbox, before hosted sync or live collaboration. This slice should prove discussion open, append, resolve, reopen, semantic anchors, append-only turns, local attention derivation, schemas, docs gates, and migration from the current state-attached discussion shape if needed. `heddle inbox` should ship in this slice as the stable local "what needs me" surface for agents and humans.

The first slice does not need general collaboration import roots unless migration from the current state-attached discussion shape requires them. The operation model should leave room for import roots without expanding the initial implementation scope.

The first local-only slice does not need the hosted-valid continuation command because Weft rejection and blocking states do not exist yet. The operation model can leave room for hosted-valid continuation without adding the command before hosted sync.

The first slice does not need explicit per-actor read state unless inbox usability requires it. Heddle can start with derived attention from operation timestamps, targets, and turn kinds; explicit read state can follow once actor and principal UX is clearer.

The first slice does not need snooze. Snooze is an actor overlay and can follow after local discussion, derived inbox, and readiness behavior prove the core semantics.

The first slice does not need full context extraction. It may allow decision turns and simple manual context annotation creation if already cheap, but extraction workflows should follow after discussions and annotations are stable.

The first slice should not become a general issue tracker. Labels, assignees, milestones, and project-board state can be modeled later if needed; the first slice should prove anchored conversation, structured turn kinds, lifecycle operations, attention derivation, and conflict handling.

The first local-only slice does not have to implement full redaction, but its schema should leave room for redaction operations and its docs should state plainly that v1 local collaboration records are plaintext and append-only. Hosted collaboration sync should not broadly ship without a clear redaction and policy-suppression story.

First-slice tests should cover both lower-level materialization and CLI contracts. Materialization property tests protect convergence behavior, while CLI golden JSON tests protect the agent-facing command contract. Human text output can start with lighter smoke coverage.

The first local discussion slice counts as shipped only when command metadata is in place, JSON golden tests cover the agent contract, materialization/property tests cover core convergence behavior, basic collaboration `fsck`/`doctor` checks pass, migration plan/apply tests exist if legacy data exists, docs label local-only versus Weft-backed versus planned behavior, and at least one end-to-end agent workflow example is documented.

Release notes and docs for the first discussion slice must explicitly label what is local-only, what requires Weft-backed sync, and what remains planned. This prevents users from assuming Git hosting shares Heddle collaboration records.

First-slice docs should include JSON-first agent examples for opening a discussion, appending a turn, reading heads, resolving, checking inbox, and handling conflicts. Human examples are useful, but the agent-native value depends on clear machine workflows.

For automation-critical workflows, first-slice user-facing docs should show the JSON example before the terse human command example. Humans still get readable examples, but the agent contract should be easy to copy, test, and validate.

First-slice docs should state the verification status clearly: local discussion behavior starts with Rust/property tests, while remote CRDT sync requires the formal convergence model before shipping.

Local discussion commands should not be hidden as experimental once the first slice passes its tests and docs gates. They are core pre-1.0 commands. Hosted sync and live collaboration should be labeled separately as foundation or planned until shipped.

The first slice should expose conflict resolution for any conflict kind it can create. If resolve and reopen ship, resolution-conflict resolution must ship too. Visibility-conflict resolution can wait until visibility commands ship.

Migration from existing state-attached discussion objects should not be silent. `heddle discuss` can detect legacy discussions and offer or run an explicit migration command. Explicit migration makes it clear when legacy state-attached discussion history becomes repository collaboration history.

Legacy discussion migration should be plan-first by default. The migration command should show the proposed import roots and aliases, return the same plan in JSON for agents, and require an explicit `--apply` or equivalent before writing repository collaboration operations.

Legacy discussion migration should be idempotent. Re-running migration should return the already-created imported discussion IDs and operation IDs instead of creating duplicate import roots; deterministic source-derived collaboration idempotency keys are appropriate for migration writes.

If migration apply stops halfway, Heddle should not roll back already-written collaboration operations. The apply path should be crash-safe and idempotent: rerunning the same plan completes missing operations, reports existing imported records, and leaves legacy source objects untouched until a separate cleanup.

Legacy state-attached discussions migrate as collaboration import roots, not as if they were originally native operations. The import root records source metadata pointing to the legacy state and discussion objects.

`heddle discuss` output should expose migrated legacy history as imported history. JSON includes import source and trust metadata; human detail views show the legacy/import label, while terse list output can keep it compact.

**Status:** proposed

**Considered Options:** Starting with Weft sync would show hosted collaboration sooner, but it would multiply any mistakes in the local record model. Starting local-first preserves the agreed Heddle boundary and lets the OSS CLI validate the core collaboration semantics before introducing remote cursors, policy filtering over sync, and live hosted delivery.
