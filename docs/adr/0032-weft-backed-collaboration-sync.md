# Weft-backed collaboration sync

Collaboration sync requires a Weft-backed Heddle remote capability. Local Heddle repositories can store collaboration records, but sharing discussions, context annotations, attention overlays, and collaboration operations across machines or people is a Heddle-hosted capability, not a Git remote or generic source-object remote feature. Weft-backed `push` and `pull` include collaboration by default when policy permits it, while output reports source sync and collaboration sync results separately. Partial lane success is allowed, but the default command exits non-zero unless every requested lane succeeds; `--allow-partial` can make partial success an explicit operator choice. Source pushes auto-include collaboration records causally or semantically linked to pushed source content, and synced discussions can receive live gRPC updates while a Weft connection is active.

Source push should not sync every unrelated local discussion by default. Linked collaboration records travel with the pushed content; unrelated collaboration requires an explicit collaboration sync command or a later policy-driven default that makes the sharing boundary visible.

The initial linked-collaboration rule should be narrow: records anchored to pushed states, changes, changed paths or symbols, active blockers or questions for the pushed thread, and operations that causally continue those records. Broad repository discussions should not sync merely because their text mentions the branch.

Auto-sync sends the required causal closure for the hosted-valid path, not only the latest visible operation. Weft needs enough accepted parent history to validate and materialize records; hosted-rejected branches stay local unless a hosted-valid continuation deliberately bypasses them.

If source push succeeds but collaboration sync fails, Heddle should not roll back the source push. It reports partial lane success, records collaboration sync metadata, exits non-zero by default, and supports an explicit partial-success mode when the operator accepts that split state.

Retry after partial source/collaboration success resumes the collaboration lane from stored sync metadata and idempotency state. Heddle does not replay the successful source push merely to retry pending or blocked collaboration operations; it uses remote cursors and the hosted-valid causal closure needed by the collaboration lane.

Weft can reject locally accepted collaboration operations under hosted policy. Such operations stay local and are surfaced as rejected-by-remote or local-only until policy or operation content changes. Default hosted-synced views exclude them, while local diagnostics and attention surfaces report them as sync problems. Remote acceptance, rejection, pending status, and sync cursors are collaboration sync metadata, not collaboration operations.

Weft rejection reasons include both a stable machine-readable code for agents and policy workflows, and a human message for CLI/Tapestry display. Rejection reason codes are part of the shared collaboration sync contract across Weft, Heddle, and Tapestry; individual surfaces may translate display messages but must not invent incompatible code names. The code and message are sync metadata; the message can be refreshed by Weft and is not durable collaboration content.

Hosted rejections create attention items when they affect the user or agent's ability to sync or act, such as rejected operations, blocked descendants, or a required hosted-valid continuation. These attention items are derived from sync metadata and target the actor, delegating human, or relevant thread. Low-impact historical rejections can remain diagnostic-only.

Operations that causally depend on a hosted-rejected operation remain locally valid but cannot sync to Weft while their hosted causal history is invalid. Heddle marks them as blocked by hosted-invalid causal history until the rejected parent is accepted, replaced, or bypassed by a later hosted-valid operation. Weft does not accept an operation whose causal parent is outside the hosted-valid graph for that repository and policy scope.

A user or agent bypasses a hosted-rejected causal parent by creating a hosted-valid continuation: a new collaboration operation on the same collaboration record that intentionally cites the last hosted-valid operation it observed, omits the rejected operation and any hosted-invalid descendants from its causal parents, and records why the hosted-valid path is being resumed, such as recreating the turn under a valid capability. This does not erase or rewrite the rejected local operation.

Heddle should expose hosted-valid continuation through a first-class command or subcommand rather than a hidden option on ordinary discussion writes. The workflow selects the last hosted-valid operation, shows the omitted hosted-invalid chain, requires a reason, and emits the continuation operation. This keeps intentional causal splits explicit for humans and safe for agents.

The discussion command surface should name this workflow explicitly, for example `heddle discuss continue-hosted`. The wording should emphasize hosted validity because the local operation history remains valid and retained.

Continuation workflows may prefill content from a rejected operation for review, but they must not silently copy it. The emitted operation is a new collaboration operation with fresh attribution, capability context, timestamp, and continuation reason.

After a hosted-valid continuation is created, rejected local operations remain visible in full local views with local-only or rejected styling, separated from the hosted-valid path. Hosted-synced and capability-filtered views show the accepted continuation path by default so users can distinguish retained local history from Weft-accepted collaboration history.

Local-only, rejected, blocked, and capability-filtered display status is derived from sync metadata and active capability context, not stored in the immutable collaboration operation. Remote policy changes or capability changes can therefore update views without rewriting collaboration history.

If policy or grants later make a rejected operation acceptable, Heddle syncs the original operation as-is. A hosted-valid continuation is only needed when the original operation remains hosted-invalid or the user intentionally resumes from a different hosted-valid path.

Collaboration import roots sync to Weft only when hosted policy accepts the import source and trust level. Otherwise they remain local imported history. Weft should not silently ingest arbitrary orphaned collaboration history.

Invalid collaboration artifacts do not sync to Weft as collaboration operations. They remain local diagnostics or quarantine artifacts unless an explicit support bundle or export command includes them for investigation.

Hosted collaboration sync should have an explicit redaction and policy-suppression story before broad rollout. Weft should not rely on local users rewriting or deleting append-only collaboration operations to handle accidentally sensitive content.

**Status:** accepted

**Considered Options:** Letting every Heddle remote transport carry collaboration would make the feature feel more universal, but it would blur policy, capability, live coordination, and hosted product boundaries. Weft-backed sync keeps collaboration tied to the Heddle-hosted value proposition.
