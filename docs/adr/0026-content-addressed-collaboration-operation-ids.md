# Content-addressed collaboration operation IDs

Collaboration operation IDs are content-addressed over the canonical operation envelope, while remaining distinct from source-history `ChangeId`s. The envelope includes the operation kind, target record, causal parents, attribution, capability context, payload, and timestamp or nonce as needed.

Heddle parses enough canonical envelope structure to compute or confirm the content-addressed id, then validates schema, references, signatures, and policy. Invalid bytes may have a diagnostic hash, but they do not receive a trusted `CollabOpId` unless they satisfy the canonical envelope rules.

Writing duplicate canonical operation bytes is not an error. If the same bytes yield an existing `CollabOpId`, the writer treats the operation as already present and returns an idempotent success while ensuring indexes can observe the existing operation.

An existing `CollabOpId` with different bytes is corruption or a hash-collision class failure and must fail closed. Heddle never replaces existing operation bytes under the same id; `fsck` or `doctor` should quarantine the conflicting artifact and report high severity.

Because the operation ID hashes the canonical envelope, accepted operation bytes are immutable. Schema evolution happens through versioned decoding into the current in-memory model, not by rewriting old operation bytes in place. Future pack or segment compaction must preserve the original envelope bytes for each operation ID unless it explicitly creates new operation IDs and records that mapping.

Redaction does not rewrite or replace the original operation bytes. A redaction operation or hosted policy overlay can change what materialized views and sync expose, but the original `CollabOpId` remains the hash of the original canonical envelope.

Two operations with different canonical envelope bytes have different operation IDs even if they decode to the same current in-memory operation. Cross-version semantic equivalence is not folded into the hash.

Retry and duplicate-write handling uses an explicit collaboration idempotency key rather than normalized operation identity. Collaboration commands reuse Heddle's existing `--op-id` CLI surface and `client_operation_id` RPC surface for this key, then store the same value explicitly in the collaboration operation envelope. The key is generated at the command-attempt boundary, included in the operation envelope, and checked in an idempotency index scoped to the collaboration record, operation kind, actor, and capability context. A retried write with the same key returns the already-accepted operation instead of appending a duplicate.

For one-shot human CLI writes, omitting `--op-id` means Heddle provides no replay guarantee, matching the existing mutating-command contract. For multi-step or crash-sensitive collaboration writes, Heddle may auto-generate the idempotency key and persist it in local pending state so a crash retry can complete the same write instead of creating a duplicate.

Reusing the same collaboration idempotency key with different request content is an idempotency conflict and must be rejected locally before writing. The operation hash can distinguish the bytes, but the idempotency key promises the same command attempt; changing the body under that key indicates a caller bug.

Validation failures that do not create an operation do not make the key safe to reuse for different content. A caller may retry the same failed request with the same key, but if it changes observed heads, payload, target, or operation kind, it should generate a new collaboration idempotency key.

Weft applies the same rule when validating hosted collaboration operations. A well-formed operation with a valid `CollabOpId` is still hosted-invalid if its idempotency key conflicts with an already accepted operation for the same actor/capability scope.

Independent actors submitting the same visible content with the same anchor and causal parents still create distinct collaboration operations unless they share the same idempotency key and actor/capability scope. Heddle does not deduplicate collaboration by semantic content because repeated agreement, duplicate human comments, and parallel agent conclusions can be meaningful records.

Collaboration idempotency keys are operational metadata. Normal human-facing discussion output shows the resulting operation ID, actor, timestamp, turn kind, and content; idempotency keys appear only in verbose/debug JSON and `fsck`/`doctor` diagnostics.

**Status:** proposed

**Considered Options:** Random IDs would be simple and avoid duplicate-content edge cases, but they would not provide the same deduplication, idempotent retry, and tamper-evidence properties as Heddle's content-addressed model. If intentionally duplicate operations need distinction, the operation envelope can include a nonce.
