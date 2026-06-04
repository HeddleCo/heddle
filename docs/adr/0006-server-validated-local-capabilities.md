# Server-validated local capabilities

Heddle can mint local Biscuit capabilities, but hosted authority comes from Weft validating the root capability's maximum permission scope against hosted identity and grants. Once validated, Heddle can derive immutable attenuated child capabilities locally for agents and policy context; hosted verification trusts the server-validated root scope and rejects attempts to exceed it. The OSS CLI may filter local collaboration views using the active capability, but Weft remains the hard enforcement point for remote sync and hosted access.

Agents may resolve discussions only under the active capability policy, and those resolutions carry explicit agent attribution, confidence, and resolution kind.

Local validity and hosted validity are distinct. Heddle may accept a structurally valid local collaboration operation while Weft later rejects it under hosted policy; the operation remains local and should be labeled rather than silently deleted.

Collaboration operations store the operation capability context present at creation time as immutable local provenance. The operation envelope stores the capability id and a canonical scope summary, not the full Biscuit bytes. Full token material belongs in the local capability store and Weft auth path. Collaboration sync metadata separately stores the hosted acceptance context Weft accepted for that operation on a remote. A retry may complete sync with renewed or corrected authority, so hosted acceptance context must not be confused with creation authority.

Hosted acceptance context stored locally contains Weft's acceptance facts rather than the full server-validated Biscuit proof: capability id, accepted scope summary, remote identity, accepted-at time, and policy or grant version when available. Weft owns full hosted audit proof server-side. Local sync metadata should explain and reproduce behavior without becoming a token vault.

The local capability store discards or encrypt-retires expired and superseded Biscuit token material by default while retaining non-secret capability lineage metadata. Operation and sync records keep capability ids and scope summaries; full old token material increases local secret exposure and is not required for normal provenance. A future explicit audit mode may retain encrypted token proofs.

A collaboration operation remains locally valid when its operation capability context references a capability id whose token material is no longer present in the local capability store. The operation carries its own capability id and scope summary as provenance. `fsck` may warn when referenced capability lineage metadata is missing, but missing token material does not invalidate the operation.

Routine `fsck` verifies capability context shape, well-formed capability ids and scope summaries, and reference consistency. It does not require old token proofs or hosted audit access. A deeper audit mode may compare retained lineage metadata or ask Weft to verify historical acceptance facts.

If Weft reports hosted policy or grant changes, Heddle automatically refreshes the active server-validated local capability and notifies the user that their permission scope changed. Refresh is automatic whether the scope narrows, broadens, or otherwise changes; the message makes the change visible without turning permission repair into a manual prerequisite.

Each refreshed root capability gets a new capability id linked to the prior active capability id through refresh metadata. Refresh replaces active capability context for future operations and sync attempts; it does not mutate existing immutable Biscuit tokens or rewrite previously recorded operation capability context. If the refreshed root capability narrows, Heddle automatically narrows the effective scope of locally derived capabilities by capping them against the current server-validated root. Existing child Biscuits remain immutable artifacts, but they no longer confer effective local or hosted policy context beyond the refreshed root.

Any command that contacts Weft and receives authoritative policy or grant freshness information may trigger automatic capability refresh. Pure local commands do not invent refreshes without Weft contact; they use the current cached capability context and can warn when it is expired or known stale.

When a pure local command sees expired capability context and cannot contact Weft, read-only local views continue with degraded capability-aware filtering and clear labeling. Local writes that would later claim hosted authority must either be explicit local-only writes or record that they used expired capability context so Weft can validate or reject them later. Heddle must not silently present expired policy as current hosted authority.

Operations created under expired capability context are shown in local views with a warning rather than hidden. They are locally valid records with uncertain hosted validity. Default hosted-synced views exclude them until Weft accepts them, while local views expose the expired-context provenance.

Agents may continue writing under expired capability context only when their delegated task explicitly permits local-only or offline continuation. Otherwise Heddle interrupts or blocks the agent until capability refresh. Offline continuation must be visible to the delegating human because agents can otherwise create many hosted-uncertain operations quickly.

Offline agent writes under expired capability context remain normal collaboration operations. Their operation capability context records the expired provenance, and derived warning or attention metadata surfaces the sync risk. Heddle does not create special discussion turn kinds for transport or policy state; turn kind continues to describe collaboration intent.

Offline continuation permission is checked in both the derived capability and the task assignment. The derived capability answers whether the agent may create local-only collaboration writes under expired context at all; the task assignment answers whether this agent should use that permission for the current task.

For server-validated root capabilities, Heddle uses a Weft-issued capability id as the stable hosted identity. Heddle may also record a local token hash as proof metadata, but hosted audit, refresh lineage, operation capability context, and hosted acceptance context should correlate on the Weft-issued id rather than a purely content-derived local token id.

Locally derived child capabilities use local capability ids until they are first used with Weft. Requiring Weft-issued child ids at derivation time would weaken offline local-first delegation. When a child capability is used for hosted sync or validation, Weft can bind or recognize it against the server-validated root lineage and return hosted acceptance context for the operation.

After Weft binds a locally derived child capability, future operations should use the Weft-bound child id while preserving an alias or lineage link to the original local child id. Existing operations keep their original operation capability context; new operations use the hosted-correlatable id.

Local child capability ids should be derived from canonical child capability bytes plus enough root lineage context and nonce or task identity to distinguish separate delegations. If two offline clones produce the same child capability bytes and lineage, sharing the local id is acceptable because they represent the same delegation. Separate delegations must produce distinct local ids; Weft binding can later disambiguate aliases when necessary.

When derived capability narrowing removes authority needed by an in-flight agent task, Heddle interrupts the task immediately and creates attention targeted to the agent and delegating human. The task should not keep creating operations that are predictably hosted-invalid. The agent can request renewed scope, continue under the narrowed scope through a hosted-valid continuation, or stop.

Interrupted tasks use a specific machine-readable capability-interrupted agent state rather than `done`. UI may group this with blocked work, but the state must preserve that policy changed under the task so resume and reauthorization workflows can treat it differently from ordinary blockers.

A capability-interrupted agent may write a final handoff or status discussion turn only if the narrowed capability still permits that coordination write. If not, Heddle creates local-only diagnostics or attention instead. Interruption does not grant an emergency write capability outside the refreshed scope.

Local-only diagnostics created because a final handoff turn was not permitted are not later synced as collaboration operations. If capability broadens again, the agent or delegating human may create a new explicit handoff turn with fresh attribution and operation capability context.

Capability-interruption diagnostics are operational metadata, not retained collaboration operations. Heddle retains enough local diagnostic detail for recovery and audit, while `doctor` or maintenance may summarize or expire old diagnostics according to operational metadata policy.

Capability refresh and interruption diagnostics are repository-local operational metadata. Weft and Tapestry should show hosted-side policy-change and refresh events from Weft's audit log rather than syncing Heddle's local diagnostics as collaboration content. Local diagnostics and hosted audit records can correlate by actor, repository, time, and capability id, but they remain separate records.

Capability refresh that broadens the root does not automatically broaden an in-flight agent beyond its derived attenuation. The refreshed root may permit more authority, but the child capability still caps the agent until a new derived capability is minted or assigned.

A derived child capability can remain effectively valid across root capability refresh when its attenuated scope is still covered by the refreshed root. Future policy checks should treat its effective parent lineage as passing through the refreshed root, while old operations keep the root lineage they recorded at creation time.

If the refreshed root no longer covers a child capability, Heddle preserves the child as inert historical metadata rather than active authority. It may explain old operations and interruption diagnostics, but it must not authorize future local filtering, writes, or hosted sync. Token material still follows the token-retention rule.

Capability refresh events are policy and sync metadata, not collaboration operations. They can produce attention items and diagnostics, and future operations record the refreshed operation capability context, but refresh events do not appear as discussion turns or context annotations unless a user or agent explicitly writes one.

**Status:** accepted

**Considered Options:** Pure server-issued Biscuits centralize authority but make local derivation and offline policy context weaker. Pure self-sovereign Biscuits work for local-only trust anchors, but they cannot safely authorize hosted collaboration without Weft validating the root scope.
