# Operation-based collaboration log

The repository collaboration log stores immutable collaboration operations and derives current collaboration views from those operations. This matches Heddle's existing strengths around immutable records, attribution, idempotency, provenance, and replay, while letting concurrent agents append independent discussion turns or metadata changes without overwriting each other; incompatible concurrent resolution operations materialize as explicit resolution conflicts rather than last-write-wins state.

Resolution conflicts are cleared by a specific conflict-resolution operation that cites the incompatible resolution operations and chooses the intended outcome. Ordinary resolve and reopen operations express discussion lifecycle intent; resolving a conflict between lifecycle claims is a separate collaboration operation. The same collaboration conflict-resolution pattern can resolve other explicit conflict kinds, such as visibility conflicts, with kind-specific payloads.

Collaboration conflict-resolution operations may themselves conflict when concurrent resolvers choose incompatible outcomes. The model treats conflicts as explicit operations over other operations, not hidden mutable state, so conflict materialization can recurse deterministically.

Unresolved conflicts still have deterministic safe effective state, but the semantic state remains conflicted. Resolution conflicts keep the discussion attention-worthy and unresolved for readiness purposes. Visibility conflicts use the most restrictive safe visibility. Display can order conflicting claims deterministically, but it must not hide the conflict.

The convergence model is repository-wide at the operation-log level and record-level at materialization time. Discussions, context annotations, and attention views can materialize independently where possible, while the shared repository log still supports cross-record operations, task provenance, anchors, and attention derivation without creating one giant mutable document.

Every collaboration operation names exactly one primary collaboration record for indexing and materialization. It may reference other records, source anchors, task provenance, or attention targets, but cross-record behavior is expressed through explicit references rather than ambiguous multi-primary writes.

Authorship, attention routing, and cross-operation references should use stable principal, agent, and operation identifiers rather than display names. Display names are render-time labels or provenance hints, not durable identity.

Materialized collaboration views retain each record's current operation head set internally. Human UI may render a deterministic linearized conversation, but the materializer preserves heads so new operations can cite what they observed and sync can reason about concurrency.

The materializer can produce multiple view modes from the same collaboration log plus sync metadata. Full local views include locally valid operations, including hosted-invalid ones with labels. Hosted-synced and capability-filtered views exclude or mark operations according to sync metadata and active policy context.

View mode is a query and materialization parameter, not operation identity. It does not affect `CollabOpId`, causal parent relationships, or durable operation bytes.

Operation timestamps are provenance and deterministic display tie-break inputs, not causality. Causal parents and operation identity define the operation graph; timestamps may be skewed or wrong.

Timestamps should not decide capability authority, hosted policy ordering, or conflict winners. Policy and convergence use causal parents, operation IDs, hosted acceptance metadata, and capability context; clock skew must not grant authority or erase conflicts.

Structurally valid operations that cite unknown causal parents are retained locally as unresolved operations, but they are not applied to materialized views until parents arrive or an explicit import/orphan rule accounts for the gap. Hosted sync rejects or blocks missing-parent operations unless the parent gap is explicitly supported.

Imported or orphaned collaboration history can become applied only through an explicit collaboration import root that records source, reason, and trust level. This creates an intentional causal root rather than silently applying missing-parent operations.

Imported collaboration roots are labeled in local views with source and trust information because their causal history is not native continuous Heddle history. Agents should be able to distinguish imported context from fully connected local collaboration history.

Imported roots still carry Heddle actor attribution. They may also carry original-author metadata when known, but Heddle distinguishes the import actor from the original author.

Original-author metadata from imports is provenance only and is not trusted for capability policy. Policy applies to the import actor and hosted validation context unless Weft explicitly validates the external authorship source.

If a malformed operation is encountered, Heddle fails that operation loudly and continues materializing the valid prefix or subgraph where safe. The affected record view shows corruption or invalid-operation attention rather than making the entire collaboration log unusable.

Malformed collaboration bytes are retained as invalid collaboration artifacts for diagnostics or quarantine, not treated as valid collaboration operations. `doctor` may quarantine or move them, but should not silently delete bytes that may explain corruption or attack attempts.

**Status:** accepted

**Considered Options:** Latest-document storage would be simpler to query, but it would make causality, audit, idempotent retry, and concurrent edits weaker. Per-field last-write-wins state would hide the sequence of decisions that matters for review and agent coordination.
