# Evolve discuss in place

Heddle keeps `heddle discuss` as the canonical command for anchored discussions and evolves its model in place before 1.0. The existing state-attached implementation can be replaced by the repository collaboration log without adding a second command name or preserving compatibility shims.

The command name stays `discuss`, but machine-readable JSON schemas should get a new schema version or name for the repository collaboration model. Machine consumers need an explicit schema break rather than silently receiving state-attached and repository-log semantics under one contract.

Every `heddle discuss` JSON response should include the schema identifier and version, not just mutating command responses. Agents need the contract marker on reads, lists, diagnostics, conflict output, and write results.

Old `discuss` flags should be kept only when their meaning maps cleanly to the repository collaboration model. Misleading state-attached flags should be removed or renamed before 1.0 rather than preserved as compatibility shims; docs and the command catalog should explain the new contract.

`heddle discuss` should expose explicit lifecycle subcommands such as `open`, `turn`, `resolve`, and `reopen` rather than overloading one command mode. Agents need stable verbs, and humans benefit from discoverable workflows.

`turn` is the canonical append verb because it matches the domain language and supports comment, question, answer, blocker, handoff, and status turn kinds. `comment` can exist as a human alias only if it does not obscure turn-kind semantics.

Human text-mode append can default turn kind to `comment`. JSON and agent workflows should require or strongly encourage explicit turn kind so Heddle does not have to infer blockers, questions, answers, handoffs, or status updates.

Mutating `heddle discuss` subcommands should support the collaboration idempotency key surface from the first repository-log slice. `open`, `turn`, `resolve`, `reopen`, `retarget`, migration, and conflict-resolution writes should all have a replay-safe command-attempt identity rather than treating retry safety as a later hosted-only concern.

`heddle discuss open` requires a title for repository, thread, and other broad anchors. Precise code anchors may derive a title from the anchor and first turn; JSON output still returns the derived title explicitly.

`heddle discuss list` defaults to active/open discussions scoped by the current context.

<!-- doctor-docs:planned -->
`heddle inbox` owns attention-worthy work, so discussion listing should not become a second inbox.

`heddle discuss show` defaults to a capability-filtered local view under the active capability context. It should offer explicit `--full-local` and `--hosted-synced` view modes when relevant, preserving local-first access without surprising users with restricted or local-only records by default.

Hosted and live-sync flags should not appear in normal help or stable JSON before the behavior ships. Documentation can describe hosted sync and live collaboration as planned or foundation, but the command surface should not expose aspirational flags that cannot perform the described behavior.

`--full-local` does not require confirmation for read-only display, but it labels restricted, local-only, and hosted-rejected content clearly. Confirmation belongs on writes or sync actions, not local read inspection.

JSON and detail outputs should expose causal parents, record head sets, and operation IDs so agents can append correctly and diagnose concurrency. Terse human output can hide raw IDs unless needed.

<!-- doctor-docs:planned -->
`heddle discuss show --json` should include both deterministic display order and graph facts. The response should expose head sets, causal parents, operation IDs, unresolved or conflict markers, and view mode so agents can act without reverse-engineering a rendered conversation.

Human `discuss show` may keep raw graph details terse, but it must visibly mark conflicts, ambiguous anchors, redactions, and hosted-valid versus local-only divergence. Deterministic display order is not a license to hide semantic uncertainty.

JSON and detail or verbose text should expose copyable operation IDs. Conflict resolution, hosted-valid continuation, `fsck`, and diagnostics need exact operation references; terse text can use short IDs with expansion hints.

CLI input may accept unambiguous short prefixes for discussion IDs and collaboration operation IDs, matching Heddle source object UX. Ambiguous prefixes fail with candidate details. JSON should prefer full IDs.

Append commands support defaulting causal parents to the current materialized heads for human use, while agent and JSON workflows should pass observed heads explicitly. Explicit observed heads let Heddle detect stale writes or intentionally create concurrent turns.

When explicit observed heads are stale, agent and JSON workflows fail by default unless concurrency is explicitly allowed. Human text mode can prompt or explain. Silent concurrent turns from stale agent state are too easy to miss.

Generic `--allow-concurrent` is appropriate only for append-like operations where concurrency is safe. Resolve, visibility, and retarget operations use operation-specific conflict semantics rather than a generic concurrency override.

`heddle discuss resolve` should require resolution kind and reason for agents and hosted sync. Human text mode may allow a short default reason for simple local closure, but JSON should carry explicit resolution metadata, including confidence when agent-authored.

`heddle discuss resolve` on stale observed heads fails by default for agents and JSON, returning the current head set. A caller can retry after observing current state or use the explicit conflict-resolution workflow when competing resolutions exist.

<!-- doctor-docs:planned -->
Conflict resolution should use explicit command wording such as `heddle discuss resolve-conflict`. It must be visually distinct from ordinary `resolve` because it operates on conflicting operations rather than merely closing a discussion.

<!-- doctor-docs:planned -->
`heddle discuss reopen` requires a reason because it changes lifecycle after resolution. The operation records why the old resolution no longer holds or why conversation needs to resume.

<!-- doctor-docs:planned -->
`heddle discuss retarget` should be a first-class subcommand. Anchor changes are semantically important and can conflict, so they should be explicit operations with reason and old/new anchor display rather than hidden side effects.

<!-- doctor-docs:planned -->
`heddle discuss visibility` should be a first-class advanced or hosted-aware subcommand. Visibility is policy-sensitive, can conflict, and may require Weft validation; it should not be an incidental flag on unrelated commands.

**Status:** accepted

**Considered Options:** Introducing a new command would avoid breaking the current `discuss` output, but it would fragment the collaboration surface. The name already matches the intended concept, and Heddle is pre-1.0, so the command should keep the name while changing internals and schemas deliberately.
