# Agent coordination visibility

Agent coordination discussions are visible by default to the delegating human or policy scope that authorized the agent. Agents may create restricted discussions only when their active capability permits it; Heddle should not create invisible durable agent side channels outside explicit policy.

Collaboration visibility must consume the shared E5 visibility substrate
tracked by #315-#319 and #523. Discussion and agent-coordination visibility
project into `VisibilityTier` / `StateVisibility` rather than defining a
parallel tier enum or hosted policy model.

Agent task assignment is operational metadata for v1, not a collaboration operation. It drives execution policy and local runner behavior, including whether offline continuation is allowed. Collaboration operations may reference the agent/task identity for provenance, but the assignment itself does not become durable repository collaboration history unless a later design makes synced task delegation a first-class collaboration record.

Local task assignment metadata should use a versioned local id/envelope so future runner-policy changes do not break provenance grouping. Local task assignment ids should be opaque UUIDv7 values: sortable and collision-resistant without embedding task meaning. Human-readable task meaning belongs in task metadata or discussion titles. This versioning remains operational metadata; the collaboration operation schema only records opaque local task provenance when available.

Agent task completion is also local runner metadata in v1, not a collaboration operation. If completion matters to humans or other agents, the agent should write an explicit handoff or status discussion turn under its active capability; otherwise completion remains operational state.

After task completion, Heddle should retain a compact local task provenance summary while collaboration operations reference the task assignment. Detailed runner state such as prompts, transient configuration, scratch state, and verbose logs may expire through maintenance according to operational retention.

Compact local task provenance summaries may include model and provider details when known because they help explain agent behavior. Hosted sharing of those details is policy-filtered. Operation actor attribution remains the collaboration identity; task summaries provide diagnostic runner context, not collaboration truth.

Model and provider details may inform risk or review heuristics, but they do not replace attribution, signatures, capability policy, or verification results. Model metadata is context, not authority.

Compact task provenance summaries do not retain full prompt text by default. Prompts can contain secrets, user intent, and unrelated context. Heddle may keep prompt hashes or short user-approved labels when useful; full prompt retention requires explicit diagnostic or audit mode.

Compact task provenance summaries do not retain full tool-call logs by default. They may retain coarse counts, statuses, or links to explicit diagnostics when enabled. Full tool logs are operational debug artifacts with separate retention and privacy concerns.

Task labels are user-editable local operational metadata. Changing a label affects local display and diagnostics, not immutable operation provenance or hosted aliases. If a label is shared to Weft, Weft treats it as policy-filtered hosted projection metadata.

Changing a task label does not create a collaboration operation in v1. If the change matters to collaborators, a user or agent can explicitly write a discussion note.

If the compact task provenance summary is missing, Heddle still shows locally valid collaboration operations that reference the task assignment and marks task provenance as unavailable. Missing local operational metadata is a diagnostic warning, not a reason to hide collaboration history.

V1 task assignment does not sync through Weft. Parallel agents on different machines coordinate through discussions and attention, which are collaboration records. If cross-machine agent orchestration becomes a product feature, task assignment should be promoted deliberately to a collaboration record or Weft-owned orchestration record.

When available, collaboration operation envelopes may include `task_provenance` as optional provenance. This helps explain why an agent wrote an operation and lets inbox or diagnostics group operations by delegated work. The field name should not be `task_assignment_id`, because the assignment itself is local operational metadata rather than synced authority. Missing task provenance does not invalidate an operation because humans and ambient agents may write without local task assignment metadata.

When `task_provenance` is present in the collaboration operation envelope, it is part of the canonical bytes and therefore part of `CollabOpId`. Hosted task provenance aliases remain sync metadata and do not retroactively change the operation hash.

Human-authored operations include task provenance only when the human is acting through an explicit task assignment or tool workflow. Ordinary human discussion turns omit task provenance by default so grouping stays meaningful.

Task provenance is not required for every agent-authored operation in v1. Ambient agents, older clients, and local scripts may write without task assignment metadata. Hosted policy may require task provenance for specific contexts, but local validity does not.

Public JSON schemas for the first collaboration slice should include optional nullable task provenance fields. They can be absent in operation data, but making the shape explicit early lets agents and tests handle provenance consistently.

If task provenance was omitted from an existing operation, Heddle does not patch it into that operation. Adding it would create a different canonical envelope and therefore a different `CollabOpId`. Diagnostics may link the operation to local task metadata as derived view state, but the original operation bytes stay immutable.

A later collaboration operation cannot mutate task provenance for earlier operations. It may explicitly comment that earlier operations belonged to a task, but that statement is collaboration content, not a provenance rewrite. Derived local views may group earlier operations heuristically with warnings.

Heuristic task grouping is local derived view state and does not sync to Weft by default. Only explicit `task_provenance` in operation envelopes or Weft-minted hosted aliases participate in hosted grouping.

Weft does not receive raw local task assignment ids by default during sync. Sync may include opaque task provenance only when policy permits, but local runner assignment details should not leak automatically. Hashing the local id is not sufficient if the value remains stable and linkable; shared task provenance should use a deliberate hosted-safe alias rather than a raw or trivially correlated local id. Hosted validity relies on agent identity, capability context, and operation content; task grouping can remain local unless explicitly shared.

When hosted-safe task provenance is shared, Weft mints the alias during sync under namespace policy. Heddle may request aliasing, but Weft owns the hosted alias so it can avoid cross-remote correlation and enforce sharing rules. Local Heddle stores the alias mapping as sync metadata.

Hosted-safe task provenance aliases are stable for multiple operations from the same local task within the same Weft repository and policy scope. They should not be stable across unrelated remotes or namespaces unless policy explicitly links those scopes. They do not need to preserve UUIDv7 sortability; ordering comes from operation timestamps, operation IDs, and hosted audit metadata. Local sync metadata maps a local task assignment id to per-remote hosted aliases.

Hosted-safe task provenance aliases do not embed display labels. Alias identity stays opaque; optional labels are separate policy-filtered projection metadata so labels can change without changing alias identity.

Default hosted alias mapping is one local task assignment to one hosted alias per remote and policy scope. Many-to-one grouping is allowed only when Weft policy explicitly coalesces local tasks, such as for a hosted campaign, and Heddle records that coalescing in sync metadata.

One local task assignment may map to multiple hosted aliases on the same remote only across policy-scope changes. If Weft policy changes enough that the old alias is no longer appropriate, future operations can receive a new hosted alias, with sync metadata linking the old and new aliases as a scope transition.

Task provenance grouping can cross discussion boundaries within the same repository and policy scope. A single agent task may open discussions, append turns, and create attention-relevant operations across several collaboration records. This grouping does not create causal relationships; causal relationships remain in the collaboration operation graph.

V1 task provenance grouping applies to collaboration operations, not source history operations. Source history already has its own attribution and provenance model with different storage and sync semantics. A later cross-domain provenance view may correlate task assignment with source captures, but collaboration task provenance does not change source operation identity.

A future cross-domain provenance view should be derived by default, not a collaboration record. It can correlate source attribution, collaboration operations, task assignment metadata, and sync metadata. Only explicit human or agent commentary about that relationship becomes collaboration content.

The hosted-safe task provenance alias is hosted sync metadata and hosted projection data, not part of the immutable collaboration operation envelope. The local operation envelope may carry local task assignment provenance; Weft records the hosted alias for accepted operations when policy permits sharing.

Each Weft remote or namespace may mint a different hosted-safe alias for the same local task. Heddle treats alias mappings as per-remote sync metadata so task provenance cannot accidentally correlate unrelated hosted scopes.

Task provenance aliasing is optional hosted grouping. Missing, withheld, or policy-denied task provenance does not invalidate an otherwise valid collaboration operation unless repository policy explicitly requires task provenance for a class of agent writes.

Weft may reject or omit task provenance sharing while accepting the collaboration operation itself. In that case Heddle records the denied or omitted alias as sync metadata, not as a failed collaboration write.

If repository policy requires hosted task provenance for a class of agent writes and aliasing is denied or unavailable, the operation is hosted-invalid under that policy while remaining locally valid. Heddle records the rejection or blocked state in sync metadata and surfaces attention to the agent and delegating human.

Task provenance policy failures use the same hosted rejection reason system as other collaboration sync rejections: stable shared machine-readable codes plus human messages. Agents should not need a separate error taxonomy for task provenance failures.

When policy permits task provenance sharing, Weft should return a stable hosted-safe alias once for that local task and policy scope, and Heddle should reuse the local mapping for subsequent operations. When policy later stops permitting sharing, future sync omits the alias and marks the local mapping inactive, while previously accepted hosted grouping remains historical audit data.

Hosted task provenance sharing can be disabled for future operations, but aliases already recorded on accepted hosted operations remain audit data. If policy later requires hiding them from a viewer, Tapestry suppresses display through policy filtering rather than rewriting accepted operation or sync history.

If Heddle loses its local alias mapping, it can ask Weft for a fresh hosted-safe alias for future operations under current policy. It must not reconstruct hosted task provenance by leaking raw local task assignment ids unless policy explicitly permits that disclosure. Previously accepted hosted operations keep whatever grouping Weft already recorded.

Hosted task provenance aliases survive local task metadata deletion. Once Weft accepts operations with an alias, the alias remains hosted audit and projection data. Local deletion only removes Heddle's ability to explain or reuse the local mapping without asking Weft again.

Hosted views should show hosted-safe task provenance aliases, not raw local task assignment ids. Full local views may show local task provenance when the local metadata is present and the active capability context permits it.

Hosted-safe task provenance aliases should appear in JSON outputs for inbox and discussion views when present and policy-visible, so agents can group work reliably. Terse human output omits them by default; verbose or detail views may show task grouping when it helps explain agent activity.

Verbose text output may show a concise task label or hosted alias when available and policy-visible, but full task provenance belongs in JSON or diagnostic commands. Human text should not become a provenance dump.

Tapestry may search, filter, and group by hosted-safe task provenance alias within the same repository and policy scope. It must not expose raw local task ids or use aliases to cross-correlate unrelated namespaces.

Hosted task provenance is not assignment authority. Weft and Tapestry may use aliases for grouping, audit, and navigation, but v1 task assignment remains local operational metadata and hosted aliases do not drive permissions, access control, runner lifecycle, or agent interruption state. Access control comes from identity, grants, capability context, and repository policy.

Hosted task provenance is not causal collaboration history. Aliases cannot be causal parents, merge inputs, or resolution evidence for discussions; they only group and explain operations for hosted audit, navigation, and diagnostics.

If synced task delegation later becomes a first-class product feature, Heddle should add explicit collaboration records or Weft orchestration records that link to historical hosted task provenance aliases. Historical collaboration operation envelopes are not rewritten to retrofit task assignment semantics.

`fsck` and `doctor` should check task provenance shape and alias mapping consistency, but missing local task assignment metadata or missing hosted alias mappings are warnings, not collaboration operation validity failures.

Heddle should provide an advanced diagnostic surface to inspect local task provenance summaries and hosted alias mappings. Agents and operators need to explain why operations grouped together or why hosted aliasing failed, while normal discussion commands should remain focused on collaboration content.

Task provenance inspection should live under `doctor` or an advanced `actor explain` style surface rather than a new top-level command. `inbox` and discussion JSON expose enough for normal agent workflows; deep mapping inspection is troubleshooting.

If Heddle later adds a handoff command, local task provenance should be optional context for that workflow rather than the handoff itself. The handoff turn remains collaboration content authored under capability policy; task provenance can prefill context or attach allowed provenance, but the handoff body should stand on its own.

**Status:** proposed

**Considered Options:** Private agent-only coordination could reduce noise, but it would weaken human oversight and trust. Ephemeral model scratch can remain outside Heddle; once an agent writes a discussion, it becomes durable collaboration history under the relevant capability policy.
